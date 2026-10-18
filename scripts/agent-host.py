#!/usr/bin/env python3
"""Explicit Codex CLI adapter over IZU's managed start/run operations.

No history mutation is implemented here. IZU owns workspace selection,
checkpoints, job ownership, cancellation and process groups.
"""
import argparse
from contextlib import ExitStack, contextmanager
import hashlib
import json
import os
from pathlib import Path
import signal
import secrets
import socket
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time


MAX_FILES = 10000
MAX_SOURCE_BYTES = 128 * 1024 * 1024
MAX_LOG_BYTES = 8 * 1024 * 1024


def project_local_root():
    """Resolve this checkout's main checkout without running a Git command."""
    checkout = Path(__file__).resolve().parent.parent
    marker = checkout / ".git"
    if marker.is_file():
        text = marker.read_text().strip()
        if not text.startswith("gitdir: "):
            raise ValueError("invalid worktree Git directory marker")
        gitdir = (checkout / text.removeprefix("gitdir: ")).resolve(strict=True)
        common = (gitdir / (gitdir / "commondir").read_text().strip()).resolve(strict=True)
        checkout = common.parent
    if not (checkout / "scripts" / "izu-env").is_file():
        raise ValueError("cannot identify this project's main checkout")
    return (checkout / ".artifacts" / "local").resolve(strict=True)


def require_local(path):
    resolved = path.resolve()
    if not resolved.is_relative_to(project_local_root()):
        raise ValueError("path must be inside this main checkout's .artifacts/local")
    return resolved


def private_directory(path, create=False):
    """Run state contains prompts and a cancellation capability; require its owner."""
    if create:
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
    metadata = path.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise ValueError("host state must be a real directory owned by the current user")
    if stat.S_IMODE(metadata.st_mode) & 0o077:
        raise ValueError("host state must be private (mode 0700)")
    return path.resolve(strict=True)


def read_bounded(path, limit, label):
    with path.open("rb") as handle:
        data = handle.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"{label} exceeds {limit} bytes")
    return data


def settle_owned(process, group=False):
    """Reap one live owned Popen; escalation never targets a saved PID."""
    if process.poll() is None:
        try:
            process.send_signal(signal.SIGINT)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            if process.poll() is None:
                try:
                    if group:
                        os.killpg(process.pid, signal.SIGKILL)
                    else:
                        process.kill()
                except ProcessLookupError:
                    pass
    # Even SIGKILL cannot guarantee immediate reaping during an OS stall.
    # Bound the final wait and propagate uncertainty instead of hanging cleanup.
    process.wait(timeout=5)


def save(path, value):
    # Evidence directories are private and unique; never overwrite another run.
    with path.open("x", encoding="utf-8") as handle:
        json.dump(value, handle, indent=2, sort_keys=True)
        handle.write("\n")


def snapshot(root):
    files = {}
    total = 0
    for parent, directories, names in os.walk(root, followlinks=False):
        directories[:] = sorted(d for d in directories if d not in (".izu", ".git"))
        for name in sorted(names + [d for d in directories if (Path(parent) / d).is_symlink()]):
            path = Path(parent) / name
            metadata = path.lstat()
            if len(files) >= MAX_FILES:
                raise ValueError("source exceeds host evidence file budget")
            key = str(path.relative_to(root))
            if stat.S_ISLNK(metadata.st_mode):
                files[key] = {"symlink": os.readlink(path)}
            elif stat.S_ISREG(metadata.st_mode):
                total += metadata.st_size
                if total > MAX_SOURCE_BYTES:
                    raise ValueError("source exceeds host evidence byte budget")
                digest = hashlib.sha256()
                with path.open("rb") as handle:
                    for block in iter(lambda: handle.read(65536), b""):
                        digest.update(block)
                files[key] = {"sha256": digest.hexdigest(), "size": metadata.st_size,
                              "mode": stat.S_IMODE(metadata.st_mode)}
            else:
                raise ValueError(f"unsupported evidence entry: {key}")
    return files


def child(args):
    root = Path.cwd().resolve(strict=True)
    evidence = Path(args.evidence).resolve(strict=True)
    # The host makes cwd explicit before model execution and before source writes.
    command = [args.codex, "exec", "--cd", str(root), "--skip-git-repo-check",
               "--ignore-user-config", "--ephemeral", "--json",
               "--config", "project_root_markers=[]",
               "--model", args.model, "--config", 'model_reasoning_effort="medium"',
               "--sandbox", "workspace-write", "-"]
    # Native workspaces can be nested inside an unrelated Git checkout. Empty
    # root markers confine automatic startup guidance to this selected cwd.
    save(evidence / "binding.json", {"cwd": str(root), "argv": command,
         "instruction_scope": "selected workspace; explicit personal file if supplied",
         "project_instructions": {name: hashlib.sha256((root / name).read_bytes()).hexdigest()
                                  for name in ("AGENTS.md", "AGENTS.override.md")
                                  if (root / name).is_file()},
         "pid": os.getpid(), "started_at_unix_ns": time.time_ns(),
         "before": snapshot(root) if args.evidence_mode == "full" else None})
    with ExitStack() as signal_scope, (evidence / "prompt.txt").open("rb") as prompt, \
         (evidence / "codex-events.jsonl").open("xb") as events, \
         (evidence / "codex-stderr.txt").open("xb") as errors:
        exceeded = threading.Event()
        cancellation = threading.Event()

        def interrupt_codex(_signal, _frame):
            # Codex owns tool processes which may leave our process group. Give
            # its live handle one cooperative interrupt so it can reap them.
            # IZU must still perform its final owned-group cleanup afterward.
            cancellation.set()

        # Install before Popen: cancellation during host startup must not kill
        # the adapter after it has created a host whose tools need cleanup.
        for number in (signal.SIGINT, signal.SIGTERM):
            signal_scope.callback(signal.signal, number, signal.signal(number, interrupt_codex))
        if cancellation.is_set():
            save(evidence / "host-result.json", {"cwd": str(root),
                 "returncode": None, "host_launched": False,
                 "output_limit_exceeded": False, "cancellation_requested": True,
                 "finished_at_unix_ns": time.time_ns(), "after": snapshot(root) if args.evidence_mode == "full" else None})
            return 130
        process = subprocess.Popen(command, cwd=root, stdin=prompt,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)

        def drain(source, destination):
            used = 0
            for block in iter(lambda: os.read(source.fileno(), 65536), b""):
                remaining = max(0, MAX_LOG_BYTES - used)
                destination.write(block[:remaining])
                destination.flush()
                used += len(block)
                if used > MAX_LOG_BYTES:
                    exceeded.set()
            source.close()

        readers = [threading.Thread(target=drain, args=(process.stdout, events)),
                   threading.Thread(target=drain, args=(process.stderr, errors))]
        for reader in readers:
            reader.start()
        try:
            interrupted = False
            while True:
                if (cancellation.is_set() or exceeded.is_set()) and not interrupted:
                    interrupted = True
                    if process.poll() is None:
                        try:
                            process.send_signal(signal.SIGINT)
                        except ProcessLookupError:
                            pass
                try:
                    returncode = process.wait(timeout=0.05)
                    break
                except subprocess.TimeoutExpired:
                    pass
        finally:
            settle_owned(process)
            for reader in readers:
                reader.join()
    save(evidence / "host-result.json", {"cwd": str(root),
         "returncode": returncode, "output_limit_exceeded": exceeded.is_set(),
         "cancellation_requested": cancellation.is_set(),
         "finished_at_unix_ns": time.time_ns(),
         "after": snapshot(root) if args.evidence_mode == "full" else None})
    return 1 if exceeded.is_set() else 130 if cancellation.is_set() else returncode


@contextmanager
def controller_endpoint(evidence, cancellation):
    """One capability for one live controller; never signal a persisted PID."""
    token = secrets.token_hex(32)
    stopped = threading.Event()
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(4)
        listener.settimeout(0.2)
        save(evidence / "control.json", {"version": 1, "port": listener.getsockname()[1], "token": token})

        def serve():
            while not stopped.is_set():
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    break
                with connection:
                    connection.settimeout(1)
                    try:
                        data = bytearray()
                        while len(data) <= 1024 and not data.endswith(b"\n"):
                            block = connection.recv(min(256, 1025 - len(data)))
                            if not block: break
                            data.extend(block)
                        request = json.loads(data) if len(data) <= 1024 else None
                        valid = (isinstance(request, dict) and request.get("action") == "cancel"
                                 and isinstance(request.get("token"), str)
                                 and request["token"].isascii()
                                 and secrets.compare_digest(request["token"], token))
                        if valid: cancellation.set()
                        connection.sendall(json.dumps({"accepted": valid}).encode() + b"\n")
                    except (OSError, ValueError):
                        pass
        thread = threading.Thread(target=serve, name="izu-host-control")
        thread.start()
        try:
            yield
        finally:
            stopped.set()
            thread.join(timeout=2)
            if thread.is_alive():
                raise RuntimeError("host control listener did not stop")


def inspect_run(evidence, cancel=False):
    evidence = private_directory(evidence)
    def read(name):
        path = evidence / name
        if not path.exists(): return None
        if path.stat().st_size > MAX_LOG_BYTES: raise ValueError("run record exceeds limit")
        text = path.read_text()
        if not text.strip(): return None
        return json.loads(text)
    result = read("launch-result.json")
    if cancel:
        if result is not None:
            return {"state": "finished", "result": result, "cancellation_requested": False}
        control = read("control.json")
        if not control or control.get("version") != 1:
            raise ValueError("live controller endpoint is not available; no PID was signalled")
        with socket.create_connection(("127.0.0.1", int(control["port"])), timeout=2) as channel:
            channel.sendall(json.dumps({"action": "cancel", "token": control["token"]}).encode() + b"\n")
            response = json.loads(channel.makefile("rb").readline(1025))
        if response != {"accepted": True}: raise ValueError("controller rejected cancellation")
        return {"cancellation_requested": True, "next_action": "Inspect this run until terminal cleanup is recorded."}
    return {"evidence": str(evidence), "state": "finished" if result else "pending_or_running",
            "result": result, "native": read("izu-response.json") if result else None}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("start", "run", "continue", "status", "cancel", "_child"))
    parser.add_argument("--izu", type=Path)
    parser.add_argument("--repo", type=Path)
    parser.add_argument("--change")
    parser.add_argument("--codex", type=Path)
    parser.add_argument("--state-root", type=Path)
    parser.add_argument("--auth-file", type=Path,
                        help="existing auth.json; defaults to the current Codex home when present")
    parser.add_argument("--instructions-file", type=Path,
                        help="optional personal AGENTS.md, copied to the temporary Codex home")
    parser.add_argument("--evidence", type=Path)
    prompts = parser.add_mutually_exclusive_group()
    prompts.add_argument("--prompt-file", type=Path)
    prompts.add_argument("--prompt")
    parser.add_argument("--evidence-mode", choices=("native", "full"), default="native",
                        help="native checkpoint evidence, or bounded full filesystem hashes for benchmarks")
    parser.add_argument("--model", default="gpt-6.1-sol")
    parser.add_argument("--timeout-seconds", type=int, default=300)
    args = parser.parse_args()
    if args.mode in ("status", "cancel"):
        if args.evidence is None: parser.error("status/cancel require --evidence")
        print(json.dumps(inspect_run(args.evidence, cancel=args.mode == "cancel")))
        return 0
    if args.mode == "continue": args.mode = "run"
    codex = args.codex or shutil.which("codex")
    if codex is None: parser.error("Codex CLI not found; select --codex")
    args.codex = str(Path(codex).resolve(strict=True))
    if args.mode == "_child":
        if args.evidence is None: parser.error("child requires evidence")
        private_directory(args.evidence)
        return child(args)
    args.repo = args.repo or Path.cwd()
    packaged_izu = Path(__file__).resolve().parent / "izu"
    selected_izu = args.izu or (packaged_izu if packaged_izu.is_file() else shutil.which("izu"))
    if selected_izu is None: parser.error("Izu executable not found; select --izu")
    args.izu = Path(selected_izu)
    if args.change is None or (args.prompt_file is None and args.prompt is None):
        parser.error("start/continue require --change and --prompt or --prompt-file")
    if not 1 <= args.timeout_seconds <= 86400:
        parser.error("timeout must be between 1 and 86400 seconds")
    repo = args.repo.resolve(strict=True)
    executable = args.izu.resolve(strict=True)
    args.state_root = args.state_root or repo.parent / ("." + repo.name + ".izu-host")
    if args.state_root.resolve().is_relative_to(repo):
        parser.error("host state must be outside the repository source")
    state_root = private_directory(args.state_root, create=True)
    args.evidence = args.evidence or state_root / ("run-" + secrets.token_hex(8))
    if args.evidence.resolve().is_relative_to(repo):
        parser.error("host evidence must be outside the repository source")
    args.evidence.mkdir(mode=0o700, parents=True, exist_ok=False)
    evidence = private_directory(args.evidence)
    if args.prompt is not None:
        prompt = args.prompt.encode()
    else:
        prompt = read_bounded(args.prompt_file, 1024 * 1024, "prompt")
    if len(prompt) > 1024 * 1024:
        raise ValueError("prompt exceeds one MiB")
    (evidence / "prompt.txt").write_bytes(prompt)
    command = [str(executable), "--repo", str(repo), "--json"]
    if args.mode == "start":
        command += ["start", args.change]
    else:
        command += ["--change", args.change, "run"]
    command += ["--timeout-seconds", str(args.timeout_seconds),
                "--cooperative-interrupt-ms", "5000",
                "--", sys.executable, str(Path(__file__).resolve()), "_child",
                "--codex", args.codex, "--evidence", str(evidence), "--model", args.model,
                "--evidence-mode", args.evidence_mode]
    save(evidence / "launch.json", {"argv": command, "repo": str(repo),
         "adapter_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
         "izu_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
         "codex_version": subprocess.check_output([args.codex, "--version"], text=True).strip()})
    print(json.dumps({"evidence": str(evidence), "state": "starting"}), file=sys.stderr, flush=True)
    # Auth is copied only into a private temporary home outside captured source.
    # Codex token refresh can touch this copy, never the supplied account file.
    with ExitStack() as signal_scope, \
         tempfile.TemporaryDirectory(prefix="codex-session-", dir=state_root) as temporary:
        cancellation = threading.Event()

        def cancel_owned_controller(_signal, _frame):
            # Popen methods may acquire locks. Only flag work in a signal
            # handler; the owning main loop performs interruption and reaping.
            cancellation.set()

        for number in (signal.SIGINT, signal.SIGTERM):
            signal_scope.callback(signal.signal, number,
                                  signal.signal(number, cancel_owned_controller))
        signal_scope.enter_context(controller_endpoint(evidence, cancellation))
        codex_home = Path(temporary)
        if codex_home.is_relative_to(repo):
            raise ValueError("temporary auth state must be outside the repository source")
        default_auth = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "auth.json"
        auth_file = args.auth_file or (default_auth if default_auth.is_file() else None)
        if auth_file:
            auth = read_bounded(auth_file, 1024 * 1024, "auth file")
            auth_path = codex_home / "auth.json"
            descriptor = os.open(auth_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "wb") as handle:
                handle.write(auth)
        if args.instructions_file:
            instructions = read_bounded(args.instructions_file, 32768, "personal instructions")
            (codex_home / "AGENTS.md").write_bytes(instructions)
            save(evidence / "personal-instructions.json", {
                "sha256": hashlib.sha256(instructions).hexdigest(), "bytes": len(instructions)})
        environment = os.environ.copy()
        environment.update(CODEX_HOME=str(codex_home), TMPDIR=str(codex_home),
                           XDG_CACHE_HOME=str(codex_home / "cache"))
        with (evidence / "izu-response.json").open("xb") as output, \
             (evidence / "izu-stderr.txt").open("xb") as errors:
            controller_launched = not cancellation.is_set()
            if not controller_launched:
                returncode = 130
            else:
                process = subprocess.Popen(command, cwd=repo, env=environment, stdout=output,
                                           stderr=errors)
                try:
                    save(evidence / "controller.json", {"pid": process.pid,
                         "host_pid": os.getpid(), "started_at_unix_ns": time.time_ns()})
                    while True:
                        if cancellation.is_set():
                            # One bounded cooperative interrupt through the
                            # retained handle runs in finally, with escalation.
                            returncode = 130
                            break
                        try:
                            returncode = process.wait(timeout=0.05)
                            break
                        except subprocess.TimeoutExpired:
                            pass
                finally:
                    settle_owned(process)
    if cancellation.is_set():
        returncode = 130
    save(evidence / "launch-result.json", {"returncode": returncode,
         "controller_launched": controller_launched,
         "cancellation_requested": cancellation.is_set()})
    print(json.dumps({"evidence": str(evidence), "returncode": returncode}))
    return returncode


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)

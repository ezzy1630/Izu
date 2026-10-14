#!/usr/bin/env python3
"""Explicit Codex CLI adapter over IZU's managed start/run operations.

No history mutation is implemented here. IZU owns workspace selection,
checkpoints, job ownership, cancellation and process groups.
"""
import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import signal
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
               "--ignore-user-config", "--ignore-rules", "--ephemeral", "--json",
               "--model", args.model, "--config", 'model_reasoning_effort="medium"',
               "--sandbox", "workspace-write", "-"]
    save(evidence / "binding.json", {"cwd": str(root), "argv": command,
         "pid": os.getpid(), "started_at_unix_ns": time.time_ns(),
         "before": snapshot(root)})
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
                 "finished_at_unix_ns": time.time_ns(), "after": snapshot(root)})
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
         "after": snapshot(root)})
    return 1 if exceeded.is_set() else 130 if cancellation.is_set() else returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("start", "run", "_child"))
    parser.add_argument("--izu", type=Path)
    parser.add_argument("--repo", type=Path)
    parser.add_argument("--change")
    parser.add_argument("--codex", type=Path, required=True)
    parser.add_argument("--state-root", type=Path)
    parser.add_argument("--auth-file", type=Path)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--prompt-file", type=Path)
    parser.add_argument("--model", default="gpt-6.1-sol")
    parser.add_argument("--timeout-seconds", type=int, default=300)
    args = parser.parse_args()
    args.codex = str(args.codex.resolve(strict=True))
    if args.mode == "_child":
        require_local(args.evidence)
        return child(args)
    if any(value is None for value in (args.izu, args.repo, args.change,
                                       args.state_root, args.prompt_file)):
        parser.error("start/run require --izu --repo --change --state-root --prompt-file")
    if not 1 <= args.timeout_seconds <= 86400:
        parser.error("timeout must be between 1 and 86400 seconds")
    repo = args.repo.resolve(strict=True)
    executable = args.izu.resolve(strict=True)
    state_root = require_local(args.state_root).resolve(strict=True)
    require_local(args.evidence)
    if args.evidence.resolve().is_relative_to(repo):
        parser.error("host evidence must be outside the repository source")
    args.evidence.mkdir(mode=0o700, parents=True, exist_ok=False)
    evidence = args.evidence.resolve(strict=True)
    with args.prompt_file.open("rb") as handle:
        prompt = handle.read(1024 * 1024 + 1)
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
                "--codex", args.codex, "--evidence", str(evidence), "--model", args.model]
    save(evidence / "launch.json", {"argv": command, "repo": str(repo),
         "adapter_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
         "izu_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
         "codex_version": subprocess.check_output([args.codex, "--version"], text=True).strip()})
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
        codex_home = Path(temporary)
        if codex_home.is_relative_to(repo):
            raise ValueError("temporary auth state must be outside the repository source")
        if args.auth_file:
            with args.auth_file.open("rb") as handle:
                auth = handle.read(1024 * 1024 + 1)
            if len(auth) > 1024 * 1024:
                raise ValueError("auth file exceeds one MiB")
            auth_path = codex_home / "auth.json"
            descriptor = os.open(auth_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "wb") as handle:
                handle.write(auth)
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

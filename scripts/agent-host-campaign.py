#!/usr/bin/env python3
"""Run five real Codex agents in a fresh disposable native fixture.

This consumes model usage and performs local fixture history mutations only.
It never installs a host, changes account settings or publishes anything.
"""
import argparse
from contextlib import ExitStack
import importlib.util
import json
import runpy
from pathlib import Path
import subprocess
import sys
import time

sys.dont_write_bytecode = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--izu", type=Path, required=True)
    parser.add_argument("--codex", type=Path, required=True)
    parser.add_argument("--auth-file", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True, help="new directory under .artifacts/local")
    args = parser.parse_args()
    scripts = Path(__file__).resolve().parent
    host = runpy.run_path(str(scripts / "agent-host.py"))
    host["require_local"](args.root)
    args.root.mkdir(mode=0o700, parents=True, exist_ok=False)
    root = args.root.resolve(strict=True)
    repo = root / "repository"
    repo.mkdir()
    state = root / "state"
    state.mkdir(mode=0o700)
    evidence = root / "evidence"
    evidence.mkdir(mode=0o700)
    izu = str(args.izu.resolve(strict=True))
    scripts = Path(__file__).resolve().parent
    auth = ["--author-name", "HostFixture", "--author-email", "fixture@example.invalid"]

    def native(label, command, expected=0):
        result = subprocess.run([izu, "--repo", str(repo), "--json"] + command,
                                cwd=repo, capture_output=True, text=True)
        (evidence / (label + ".json")).write_text(result.stdout)
        (evidence / (label + ".stderr")).write_text(result.stderr)
        if result.returncode != expected:
            raise ValueError(f"{label} exited {result.returncode}, expected {expected}")
        response = json.loads(result.stdout)
        return response["outcome"].get("result", {}).get("data"), response

    native("init", ["init", str(repo)])
    (repo / "human.txt").write_bytes(b"preserve human work\n")
    (repo / "shared.txt").write_bytes(b"baseline\n")
    native("baseline", ["commit", "-m", "Host fixture baseline"] + auth)
    processes = []
    samples = []
    try:
        with ExitStack() as owned:
            for index in range(5):
                name = "agent-" + str(index)
                target = "shared.txt" if index < 2 else name + ".txt"
                word = name
                prompt = root / (name + "-prompt.txt")
                prompt.write_text(f"First run pwd and read human.txt. Modify only {target}: use Python pathlib.Path.write_bytes to write the UTF-8 word {word} followed by byte 10. Verify exact bytes. Then execute python3 -c 'import time; time.sleep(20)' to allow observation of runtime admission. Never modify human.txt or any other files. Do not use Git or IZU. Stop after this.\n")
                command = [sys.executable, str(scripts / "agent-host.py"), "start", "--izu", izu,
                           "--repo", str(repo), "--change", name, "--codex", str(args.codex),
                           "--state-root", str(state), "--auth-file", str(args.auth_file),
                           "--prompt-file", str(prompt), "--evidence", str(evidence / name)]
                output = owned.enter_context((evidence / (name + "-launcher.stdout")).open("w"))
                errors = owned.enter_context((evidence / (name + "-launcher.stderr")).open("w"))
                process = subprocess.Popen(command, cwd=repo, stdout=output, stderr=errors, start_new_session=True)
                owned.callback(host["settle_owned"], process, group=True)
                processes.append((name, process))
            deadline = time.monotonic() + 360
            while any(process.poll() is None for _, process in processes):
                if time.monotonic() > deadline:
                    raise ValueError("real-host campaign exceeded its observation deadline; inspect owned jobs")
                result = subprocess.run([izu, "--repo", str(repo), "--json", "jobs", "list"],
                                        cwd=repo, capture_output=True, text=True, timeout=10)
                if result.returncode != 0:
                    raise ValueError("native jobs inspection failed during the real-host campaign")
                response = json.loads(result.stdout)
                jobs = response["outcome"].get("result", {}).get("data", [])
                active = sum("Running" in job["state"] or "Starting" in job["state"] for job in jobs)
                queued = sum("Queued" in job["state"] for job in jobs)
                samples.append({"at_unix_ns": time.time_ns(), "active": active, "queued": queued,
                                "jobs": jobs})
                if active > 4:
                    raise ValueError("observed active jobs exceeded the default cap")
                time.sleep(0.25)
    finally:
        (evidence / "admission-samples.json").write_text(json.dumps(samples, indent=2))
    for name, process in processes:
        if process.returncode != 0:
            raise ValueError(f"actual host {name} exited {process.returncode}; inspect its evidence")
    if not any(sample["active"] == 4 and sample["queued"] >= 1 for sample in samples):
        raise ValueError("did not observe four active agents with another queued")
    spec = importlib.util.spec_from_file_location("host_verify", scripts / "agent-host-verify.py")
    verifier = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(verifier)
    verified = []
    for index in range(5):
        name = "agent-" + str(index)
        target = "shared.txt" if index < 2 else name + ".txt"
        verified.append(verifier.verify(evidence / name,
                         [(target, name + "\n"), ("human.txt", "preserve human work\n")]))
    if len({item["workspace"] for item in verified}) != 5:
        raise ValueError("host workspace IDs are not independent")
    if (repo / "human.txt").read_bytes() != b"preserve human work\n" or (repo / "shared.txt").read_bytes() != b"baseline\n":
        raise ValueError("human source was changed by a private host")
    if list(state.glob("codex-session-*")):
        raise ValueError("temporary authentication state remains")
    native("native-verify", ["verify"])
    native("native-recover", ["recover"])
    report = {"root": str(root), "repo": str(repo), "verified": verified,
              "maximum_active": max(sample["active"] for sample in samples),
              "observed_cap_queue": True, "auth_temporary_directories_remaining": 0}
    (evidence / "campaign.json").write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)

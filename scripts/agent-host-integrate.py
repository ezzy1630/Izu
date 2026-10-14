#!/usr/bin/env python3
"""Integrate two real conflicting agent results in a completed owned fixture."""
import argparse
import json
from pathlib import Path
import runpy
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--izu", type=Path, required=True)
    parser.add_argument("--campaign", type=Path, required=True)
    parser.add_argument("--resume", action="store_true", help="reuse this fixture's saved operation receipts")
    args = parser.parse_args()
    root = args.campaign.resolve(strict=True)
    host = runpy.run_path(str(Path(__file__).with_name("agent-host.py")))
    host["require_local"](root)
    campaign = json.loads((root / "evidence" / "campaign.json").read_text())
    repo = Path(campaign["repo"]).resolve(strict=True)
    if repo != (root / "repository").resolve(strict=True):
        parser.error("campaign repository must be the exact owned fixture directory")
    evidence = root / "integration"
    evidence.mkdir(mode=0o700, exist_ok=args.resume)
    izu = str(args.izu.resolve(strict=True))
    author = ["--author-name", "HostFixture", "--author-email", "fixture@example.invalid"]
    snapshot = host["snapshot"]
    guards = {item["workspace"]: snapshot(Path(item["cwd"])) for item in campaign["verified"]}

    def native(label, command, error=None):
        receipt = evidence / (label + ".json")
        if args.resume and receipt.exists():
            response = json.loads(receipt.read_text())
            returncode = 0 if response["outcome"]["kind"] == "ok" else 1
        else:
            result = subprocess.run([izu, "--repo", str(repo), "--json"] + command,
                                    cwd=repo, capture_output=True, text=True)
            receipt.write_text(result.stdout)
            (evidence / (label + ".stderr")).write_text(result.stderr)
            response = json.loads(result.stdout)
            returncode = result.returncode
        outcome = response["outcome"]
        if error is None:
            if returncode != 0 or outcome["kind"] != "ok":
                raise ValueError(f"{label} failed: {outcome}")
        elif returncode == 0 or outcome.get("error", {}).get("code") != error:
            raise ValueError(f"{label} did not report expected {error}: {outcome}")
        return outcome.get("result", {}).get("data")

    baseline = json.loads((root / "evidence" / "baseline.json").read_text())["outcome"]["result"]["data"]["revision"]
    sources = []
    candidates = []
    checks = []
    for index in (0, 1):
        name = "agent-" + str(index)
        commit = native("commit-" + name, ["--change", name, "commit", "-m", "Actual host result " + name] + author)
        sources.append(commit["revision"])
        check = "from pathlib import Path; assert Path('shared.txt').read_bytes() == b'" + name + "'+bytes([10]); assert Path('human.txt').read_bytes() == b'preserve human work'+bytes([10])"
        checks.append(check)
        preparation = native("candidate-" + name, ["candidate", commit["revision"], "--target", "main", "--expect", baseline, "--check", "exact-source"] + author + ["--", "python3", "-c", check])
        candidate = preparation["Ready"]["candidate"]
        candidates.append(candidate)
        native("check-" + name, ["check", candidate, "--check", "exact-source"])
    first_land = native("land-agent-0", ["land", candidates[0]])
    native("stale-land-agent-1", ["land", candidates[1]], error="stale_expectation")
    conflict = native("conflicted-candidate", ["candidate", sources[1], "--target", "main", "--expect", first_land["revision"], "--check", "exact-source"] + author + ["--", "python3", "-c", checks[1]], error="conflict")["Conflicted"]
    if [item["path"] for item in conflict["conflicts"]] != ["shared.txt"]:
        raise ValueError("conflict affected unexpected source paths")
    native("unresolved-land", ["land", conflict["candidate"]], error="invalid_request")
    resolution = native("resolve-theirs", ["resolve", conflict["candidate"], "--path", "shared.txt", "--take", "theirs"] + author)
    # Resolution creates a new immutable candidate; the old check cannot pass it.
    ready = resolution["Ready"]
    native("unchecked-resolved-land", ["land", ready["candidate"]], error="missing_check")
    native("resolved-exact-check", ["check", ready["candidate"], "--check", "exact-source"])
    final_land = native("resolved-land", ["land", ready["candidate"]])
    native("update-human", ["update"])
    if (repo / "human.txt").read_bytes() != b"preserve human work\n" or (repo / "shared.txt").read_bytes() != b"agent-1\n":
        raise ValueError("landed/default source differs from resolved candidate")
    for item in campaign["verified"]:
        if guards[item["workspace"]] != snapshot(Path(item["cwd"])):
            raise ValueError("integration changed another agent's private source")
    native("verify", ["verify"])
    native("recover", ["recover"])
    report = {"repo": str(repo), "source_revisions": sources, "first_land": first_land,
              "conflict_candidate": conflict["candidate"], "resolved_land": final_land,
              "stale_target_rejected": True, "unresolved_conflict_rejected": True,
              "fresh_resolved_check_required": True, "all_private_sources_preserved": True,
              "human_source_preserved": True}
    (evidence / "integration.json").write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError) as error:
        import sys
        print(str(error), file=sys.stderr)
        sys.exit(1)

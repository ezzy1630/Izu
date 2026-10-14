#!/usr/bin/env python3
"""Validate a completed host run's binding and independently observed files.

This proves launch/execution binding, not arbitrary task correctness. Optional
--file checks compare actual UTF-8 file contents supplied by the caller.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sys


def read(directory, name):
    path = directory / name
    if path.stat().st_size > 16 * 1024 * 1024:
        raise ValueError(f"oversized evidence: {name}")
    return json.loads(path.read_text())


def verify(directory, expected_files):
    launch = read(directory, "launch.json")
    binding = read(directory, "binding.json")
    result = read(directory, "host-result.json")
    response = read(directory, "izu-response.json")
    if response["outcome"]["kind"] != "ok":
        raise ValueError("IZU did not acknowledge a successful completed launch")
    data = response["outcome"]["result"]["data"]
    job = data["job"]
    workspace = job["request"]["workspace"]
    root = Path(binding["cwd"]).resolve(strict=True)
    if str(root) != workspace["cwd"] or str(root) != result["cwd"]:
        raise ValueError("host and native job cwd differ")
    if workspace["id"] != data["workspace"]["id"]:
        raise ValueError("receipt and job workspace IDs differ")
    if workspace["head"] != data["before"]["head"] or workspace["tree"] != data["before"]["tree"]:
        raise ValueError("launch inputs differ from preparation checkpoint")
    if result["returncode"] != 0 or result.get("output_limit_exceeded", False):
        raise ValueError("host did not finish successfully")
    finished = job["state"].get("Finished", {})
    if finished.get("outcome", {}).get("Exit", {}).get("code") != 0:
        raise ValueError("native job did not finish with exit zero")
    for name, text in expected_files:
        path = root / name
        if path.is_symlink() or not path.resolve(strict=True).is_relative_to(root):
            raise ValueError("expected file must be a regular file inside the workspace")
        expected = text.encode("utf-8")
        actual = path.read_bytes()
        if actual != expected:
            raise ValueError(f"incorrect file contents: {name}")
        digest = hashlib.sha256(actual).hexdigest()
        if result["after"][name]["sha256"] != digest:
            raise ValueError(f"file changed since host completion: {name}")
    return {"workspace": workspace["id"], "cwd": str(root), "job": job["id"],
            "izu_sha256": launch["izu_sha256"], "binding_verified": True,
            "contents_verified": [name for name, _ in expected_files]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("evidence", type=Path)
    parser.add_argument("--file", nargs=2, action="append", default=[], metavar=("PATH", "TEXT"))
    args = parser.parse_args()
    print(json.dumps(verify(args.evidence, args.file), sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)

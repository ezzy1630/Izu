#!/usr/bin/env python3
"""Repeat existing short-lock and MCP gates without modifying their deadlines.

Run cargo test --no-run --message-format=json first and select its exact test
executables. Raw failures remain failures; repeated passes do not diagnose a
historical incident. Every subprocess gets a separate owned fixture directory.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import time

from release_common import contained_new_output, source_manifest, write_json


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--storage-test', type=Path, required=True)
    parser.add_argument('--mcp-test', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--iterations', type=int, default=20)
    parser.add_argument('--workers', type=int, default=1)
    args = parser.parse_args()
    if not 1 <= args.iterations <= 100 or not 1 <= args.workers <= 8:
        parser.error('iterations must be 1..100 and workers 1..8')
    root = Path(__file__).resolve().parents[1]
    output = contained_new_output(args.output, root)
    before = source_manifest(root)
    cases = [(args.storage_test.resolve(strict=True), 'lock_wait_is_bounded_and_recovers_on_drop'),
             (args.storage_test.resolve(strict=True), 'head_absence_is_confirmed_without_relocking_guarded_readers')]
    if args.mcp_test:
        cases.append((args.mcp_test.resolve(strict=True), 'mcp_current_and_legacy_protocols_call_the_shared_engine'))
    identities = {str(binary): digest(binary) for binary, _ in cases}
    for binary, name in cases:
        listing = subprocess.run([str(binary), '--list'], capture_output=True, text=True, check=True)
        if name + ': test' not in listing.stdout:
            raise ValueError('selected binary does not contain required test: ' + name)
    def run(task):
        index, binary, name = task
        directory = output / f'{index:04d}-{name}'; directory.mkdir()
        temporary = directory / 'tmp'; temporary.mkdir()
        env = os.environ.copy(); env.update(TMPDIR=str(temporary), RUST_BACKTRACE='1')
        start = time.monotonic()
        timed_out = False
        with (directory / 'stdout').open('wb') as out, (directory / 'stderr').open('wb') as err:
            process = subprocess.Popen([str(binary), name, '--exact', '--nocapture'], cwd=root,
                env=env, stdout=out, stderr=err, start_new_session=True)
            try:
                code = process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                timed_out = True
                process.send_signal(signal.SIGINT)
                try: code = process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL); code = process.wait(timeout=5)
        row = {'index': index, 'test': name, 'executable': str(binary), 'exit': code,
               'timed_out': timed_out, 'seconds': time.monotonic()-start, 'evidence': str(directory)}
        write_json(directory / 'result.json', row)
        return row
    tasks = [(index, binary, name) for index in range(args.iterations) for binary, name in cases]
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        results = list(pool.map(run, tasks))
    unchanged = source_manifest(root) == before and all(digest(Path(p)) == h for p, h in identities.items())
    passed = unchanged and all(row['exit'] == 0 and not row['timed_out'] for row in results)
    report = {'passed': passed, 'source_unchanged': unchanged, 'source': before['source_manifest_sha256'],
              'test_executables': identities, 'platform': platform.platform(), 'workers': args.workers,
              'original_deadlines_unchanged': True, 'cases': results,
              'historical_incident_resolved': False}
    write_json(output / 'report.json', report)
    print(json.dumps({'passed': passed, 'cases': len(results), 'output': str(output)}))
    return 0 if passed else 1


if __name__ == '__main__':
    raise SystemExit(main())

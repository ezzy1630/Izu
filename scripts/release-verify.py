#!/usr/bin/env python3
"""Verify checksummed package data, then run acceptance using its extracted binaries."""
import sys
sys.dont_write_bytecode = True
import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile

from release_common import MAX_BYTES, MAX_FILES, sha, source_manifest, write_json


def extract(archive, destination):
    """Preflight the entire archive before creating any payload files."""
    with tarfile.open(archive, 'r:gz') as tar:
        entries = []
        names = set()
        ancestors = set()
        total = 0
        for entry in tar:
            path = PurePosixPath(entry.name)
            if (not entry.isfile() or path.is_absolute() or '..' in path.parts
                    or path.as_posix() != entry.name or '\\' in entry.name
                    or '\n' in entry.name or '\r' in entry.name or not path.parts):
                raise ValueError('unsafe or noncanonical archive entry: ' + repr(entry.name))
            if entry.name in names or any(parent.as_posix() in names for parent in path.parents):
                raise ValueError('duplicate or overlapping archive entry: ' + entry.name)
            if entry.name in ancestors:
                raise ValueError('overlapping archive entry: ' + entry.name)
            names.add(entry.name)
            ancestors.update(parent.as_posix() for parent in path.parents)
            total += entry.size
            if entry.size < 0 or total > MAX_BYTES or len(names) > MAX_FILES:
                raise ValueError('archive limits exceeded')
            if entry.mode not in (0o644, 0o755):
                raise ValueError('unexpected archive permissions')
            entries.append(entry)
        destination.mkdir(exist_ok=False)
        for entry in entries:
            target = destination / entry.name
            target.parent.mkdir(parents=True, exist_ok=True)
            with tar.extractfile(entry) as incoming, target.open('xb') as outgoing:
                remaining = entry.size
                while remaining:
                    block = incoming.read(min(1024 * 1024, remaining))
                    if not block:
                        raise ValueError('truncated archive entry')
                    outgoing.write(block)
                    remaining -= len(block)
            target.chmod(entry.mode)


def verify_files(root):
    sums = root / 'SHA256SUMS'
    expected = {}
    for line in sums.read_text().splitlines():
        if not re.fullmatch(r'[0-9a-f]{64}  .+', line):
            raise ValueError('invalid checksum line')
        digest, name = line.split('  ', 1)
        relative = PurePosixPath(name)
        if relative.is_absolute() or '..' in relative.parts or relative.as_posix() != name or name in expected:
            raise ValueError('unsafe or duplicate checksum name')
        expected[name] = digest
    actual = {path.relative_to(root).as_posix() for path in root.rglob('*') if path.is_file() and path != sums}
    if set(expected) != actual:
        raise ValueError('checksum inventory mismatch')
    for name, digest in expected.items():
        if sha(root / name) != digest:
            raise ValueError('checksum mismatch: ' + name)
    return sha(sums)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', type=Path, required=True)
    parser.add_argument('--expected-sha256', required=True)
    parser.add_argument('--work-root', type=Path, required=True, help='new owned extraction/evidence directory')
    parser.add_argument('--data-only', action='store_true', help='verify archive data without executing binaries; no acceptance claim')
    args = parser.parse_args()
    if not re.fullmatch('[0-9a-f]{64}', args.expected_sha256):
        raise ValueError('expected SHA256 must be lowercase hexadecimal')
    archive = args.archive.resolve()
    if sha(archive) != args.expected_sha256:
        raise ValueError('archive checksum mismatch')
    work = args.work_root.resolve()
    project = Path(__file__).resolve().parents[1]
    if not work.is_relative_to(project / '.artifacts/local'):
        raise ValueError('work root must be inside project .artifacts/local')
    work.mkdir(parents=True, exist_ok=False)
    extracted = work / 'package'
    extract(archive, extracted)
    checksum_digest = verify_files(extracted)
    manifest = json.loads((extracted / 'BUILD-MANIFEST.json').read_text())
    source = json.loads((extracted / 'SOURCE-MANIFEST.json').read_text())
    if manifest['schema_version'] != 1 or source_manifest(extracted / 'source') != source:
        raise ValueError('source manifest mismatch')
    if manifest['source_manifest_sha256'] != source['source_manifest_sha256']:
        raise ValueError('build/source identity mismatch')
    if sha(extracted / 'source/Cargo.lock') != manifest['cargo_lock_sha256']:
        raise ValueError('lock identity mismatch')
    for name in ['izu', 'izu-lab']:
        if sha(extracted / 'bin' / name) != manifest['binary_sha256'][name]:
            raise ValueError('binary identity mismatch')
    result = {'schema_version': 1, 'archive_sha256': args.expected_sha256,
              'source_manifest_sha256': source['source_manifest_sha256'], 'binary_sha256': manifest['binary_sha256'],
              'checksum_manifest_sha256': checksum_digest, 'data_verified': True, 'acceptance_passed': False,
              'scope': 'Local extracted bytes; no installation, publication, signature or support claim.'}
    if not args.data_only:
        scratch = work / 'scratch'
        scratch.mkdir()
        report = work / 'cli-workflows.json'
        command = [str(extracted / 'bin/izu-lab'), '--executable', str(extracted / 'bin/izu'),
                   '--expected-sha256', manifest['binary_sha256']['izu'], '--source-root', str(extracted / 'source'),
                   '--output', str(report), '--scratch-dir', str(scratch), 'acceptance', '--scope', 'cli-workflows']
        environment = os.environ.copy()
        environment['TMPDIR'] = str(scratch)
        with (work / 'acceptance.stdout').open('xb') as stdout, (work / 'acceptance.stderr').open('xb') as stderr:
            run = subprocess.run(command, cwd=extracted, env=environment, stdin=subprocess.DEVNULL,
                                 stdout=stdout, stderr=stderr, timeout=1800)
        result.update(command=command, exit_code=run.returncode)
        if report.exists():
            gate = json.loads(report.read_text())
            result['report_sha256'] = sha(report)
            result['acceptance_passed'] = (run.returncode == 0 and gate['scope_complete']
                and gate['source_unchanged'] and gate['worker_unchanged']
                and gate['provenance']['source_manifest_sha256'] == source['source_manifest_sha256']
                and gate['provenance']['executable_sha256'] == manifest['binary_sha256']['izu'])
            result['excluded_scope_cases'] = gate['excluded_scope_cases']
        result['archive_unchanged'] = sha(archive) == args.expected_sha256
        result['binaries_unchanged'] = all(sha(extracted / 'bin' / name) == digest
                                          for name, digest in manifest['binary_sha256'].items())
        result['data_unchanged'] = verify_files(extracted) == checksum_digest
    write_json(work / 'verification-result.json', result)
    print(json.dumps(result, sort_keys=True))
    if not args.data_only and not (result['acceptance_passed'] and result['archive_unchanged']
                                 and result['binaries_unchanged'] and result['data_unchanged']):
        raise SystemExit(2)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, KeyError, tarfile.TarError, subprocess.TimeoutExpired) as error:
        raise SystemExit(str(error))

#!/usr/bin/env python3
"""Build twice and qualify both extracted packages on the executing native host."""
import argparse
import json
from pathlib import Path
import subprocess
import sys

sys.dont_write_bytecode = True
from release_common import contained_new_output, sha, source_manifest, write_json


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cargo-wrapper', type=Path)
    parser.add_argument('--offline', action='store_true')
    args = parser.parse_args()
    root = args.source_root.resolve(strict=True)
    original = source_manifest(root)
    output = contained_new_output(args.output, root)

    def run(label, command):
        with (output / (label + '.log')).open('xb') as log:
            subprocess.run(command, cwd=root, stdin=subprocess.DEVNULL,
                           stdout=log, stderr=subprocess.STDOUT, check=True)
        print(label + ': passed', flush=True)

    run('package-input-tests', [sys.executable, '-B', str(root / 'scripts/release-tests.py')])
    packages = []
    extractions = []
    for number in [1, 2]:
        build = output / ('build-' + str(number))
        command = [sys.executable, '-B', str(root / 'scripts/package-release.py'),
                   '--source-root', str(root), '--output', str(build)]
        if args.cargo_wrapper:
            command += ['--cargo-wrapper', str(args.cargo_wrapper)]
        if args.offline:
            command += ['--offline']
        run('build-' + str(number), command)
        package = json.loads((build / 'package-result.json').read_text())
        archive = build / package['archive']
        if (package['source_manifest_sha256'] != original['source_manifest_sha256']
                or sha(archive) != package['archive_sha256']):
            raise ValueError('built package differs from the intended source or archive')
        extracted = output / ('extracted-' + str(number))
        run('extracted-' + str(number), [sys.executable, '-B', str(root / 'scripts/release-verify.py'),
            '--archive', str(archive), '--expected-sha256', package['archive_sha256'],
            '--work-root', str(extracted)])
        evidence = json.loads((extracted / 'verification-result.json').read_text())
        if (not evidence['acceptance_passed'] or not evidence['archive_unchanged']
                or not evidence['binaries_unchanged'] or not evidence['data_unchanged']
                or evidence['source_manifest_sha256'] != original['source_manifest_sha256']
                or evidence['binary_sha256'] != package['binary_sha256']):
            raise ValueError('extracted package acceptance is incomplete or mismatched')
        packages.append(package)
        extractions.append({'result': str(extracted / 'verification-result.json'),
                            'result_sha256': sha(extracted / 'verification-result.json')})
    if packages[0] != packages[1]:
        raise ValueError('independent release builds differ')
    if source_manifest(root) != original:
        raise ValueError('source changed during qualification')
    write_json(output / 'qualification.json', {
        'schema_version': 1, 'source_manifest_sha256': original['source_manifest_sha256'],
        'reproducible': True, 'package': packages[0], 'extractions': extractions,
        'scope': 'Executing host only; extracted CLI workflows. Separate fault, remote and host gates required. No installation, signing or publication.'})


if __name__ == '__main__':
    main()

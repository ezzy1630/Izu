#!/usr/bin/env python3
"""Checkpoint registered workspaces, archive native history, and prove restoration.

All repository operations go through the selected Izu executable. A successful
local drill is not an off-device backup; copying the private archive is explicit.
"""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def write(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())
    descriptor = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def read(path):
    with path.open('rb') as stream:
        raw = stream.read(16 * 1024 * 1024 + 1)
    if len(raw) > 16 * 1024 * 1024:
        raise ValueError('backup receipt exceeds limit')
    return json.loads(raw)


class Native:
    def __init__(self, binary, evidence):
        self.binary = binary
        self.evidence = evidence
        self.sequence = 0

    def call(self, repo, *arguments):
        self.sequence += 1
        stem = self.evidence / f'{self.sequence:03d}'
        command = [str(self.binary), '--repo', str(repo), '--json', *map(str, arguments)]
        # Native replies are bounded by the shared API. Keep raw stdout/stderr,
        # including uncertain publication and retained recovery paths, on failure.
        with stem.with_suffix('.stdout').open('xb') as out, stem.with_suffix('.stderr').open('xb') as err:
            result = subprocess.run(command, stdout=out, stderr=err, timeout=300)
        response = read(stem.with_suffix('.stdout'))
        if result.returncode or response['outcome']['kind'] != 'ok':
            raise ValueError(f'Izu operation failed; inspect {stem}.stdout and .stderr')
        return response['outcome']['result']['data']


def captured_state(workspaces):
    return {key: {'head': row['head'], 'tree': row['sources']['working']['tree'],
                  'sparse_paths': row.get('sparse_paths')}
            for key, row in workspaces.items()}


def restore(native, backup, receipt, destination):
    archive = backup / 'history.izu-bundle'
    if archive.is_symlink() or sha(archive) != receipt['archive_sha256']:
        raise ValueError('backup archive digest mismatch')
    verification = native.call(backup, 'bundle', 'verify', archive)
    if verification['manifest']['root_operation'] != receipt['root_operation']:
        raise ValueError('backup operation differs from receipt')
    # Native restore deliberately activates one archived workspace, avoiding
    # reuse of old machine paths. Restore each captured workspace into its own
    # new repository; retain all of them instead of losing unfinished branches.
    destination.mkdir(mode=0o700, exist_ok=False)
    paths = {}
    for workspace, expected in receipt['workspaces'].items():
        name = 'main' if workspace == receipt['selected_workspace'] else 'workspace-' + workspace
        if not re.fullmatch('[0-9a-f]{32}', workspace):
            raise ValueError('invalid archived workspace ID')
        target = destination / name
        restored = native.call(backup, 'bundle', 'restore', archive, target,
                               '--selected-workspace', workspace)
        if restored['publication']['status'] != 'durable':
            raise ValueError('restoration durability was not acknowledged')
        native.call(target, 'verify')
        workspaces = native.call(target, 'workspace', 'list')
        if captured_state(workspaces) != {workspace: expected}:
            raise ValueError('restored workspace revision, tree or sparse scope differs')
        if native.call(target, 'tags', 'list') != receipt['tags']:
            raise ValueError('restored native tags differ')
        if native.call(target, 'refs', 'list') != receipt['refs']:
            raise ValueError('restored branch references differ')
        observed = native.call(target, 'status')
        if observed['captured_tree'] != expected['tree']:
            raise ValueError(f'restored source differs for workspace {workspace}')
        paths[workspace] = str(target)
    if sha(archive) != receipt['archive_sha256']:
        raise ValueError('archive changed during restoration')
    return {'destination': str(destination), 'root_operation': receipt['root_operation'],
            'archive_sha256': receipt['archive_sha256'], 'workspace_count': len(paths),
            'repositories': paths, 'selected_repository': paths[receipt['selected_workspace']],
            'source_readback': 'all registered workspace trees matched', 'verified': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['create', 'restore'])
    parser.add_argument('--izu', type=Path)
    parser.add_argument('--repo', type=Path, default=Path.cwd())
    parser.add_argument('--output', type=Path, help='new private backup directory (create)')
    parser.add_argument('--backup', type=Path, help='existing backup directory (restore)')
    parser.add_argument('--destination', type=Path, help='new restored repository (restore)')
    args = parser.parse_args()
    packaged_izu = Path(__file__).resolve().parent / 'izu'
    selected = args.izu or (packaged_izu if packaged_izu.is_file() else shutil.which('izu'))
    if not selected:
        parser.error('select --izu or put izu on PATH')
    binary = Path(selected).resolve(strict=True)
    if args.mode == 'create':
        if args.output is None: parser.error('create requires --output')
        repo = args.repo.resolve(strict=True)
        output = args.output.resolve()
        if output.is_relative_to(repo):
            parser.error('backup output must be outside repository source')
        output.mkdir(mode=0o700, parents=True, exist_ok=False)
        evidence = output / 'evidence'; evidence.mkdir(mode=0o700)
        native = Native(binary, evidence)
        selected_workspace = native.call(repo, 'inspect')['id']
        jobs = native.call(repo, 'jobs', 'list')
        if any('Finished' not in job['state'] for job in jobs):
            raise ValueError('managed jobs are unfinished; stop them through their owning controller before backup')
        workspaces = native.call(repo, 'workspace', 'list')
        checkpoints = {}
        for workspace in workspaces:
            checkpoints[workspace] = native.call(repo, '--workspace', workspace, 'checkpoint')
        workspaces = native.call(repo, 'workspace', 'list')
        states = captured_state(workspaces)
        if set(states) != set(checkpoints) or any(states[key]['tree'] != value['tree'] or
                states[key]['head'] != value['head'] for key, value in checkpoints.items()):
            raise ValueError('workspace state changed during backup; inspect retained checkpoints')
        refs = native.call(repo, 'refs', 'list')
        tags = native.call(repo, 'tags', 'list')
        archive = output / 'history.izu-bundle'
        created = native.call(repo, 'bundle', 'create', archive)
        operation = list(checkpoints.values())[-1]['operation']
        if created['publication']['status'] != 'durable' or created['verification']['manifest']['root_operation'] != operation:
            raise ValueError('archive was not durable at the captured operation; retain and inspect it')
        receipt = {'schema_version': 1, 'archive_sha256': sha(archive), 'root_operation': operation,
                   'izu_sha256': sha(binary), 'selected_workspace': selected_workspace,
                   'workspaces': states, 'refs': refs, 'tags': tags,
                   'scope': 'captured history and checkpointed registered source; historical recovery objects can contain ignored data; no live database or external cache backup',
                   'off_device_copy_verified': False}
        write(output / 'backup.json', receipt)
        verified = restore(native, output, receipt, output / 'restore-check')
        write(output / 'restore-check.json', verified)
        print(json.dumps({'backup': str(output), **verified, 'off_device_copy_verified': False}))
    else:
        if args.backup is None or args.destination is None:
            parser.error('restore requires --backup and --destination')
        backup = args.backup.resolve(strict=True)
        destination = args.destination.resolve()
        if destination.exists() or args.destination.is_symlink():
            parser.error('restore destination must not exist')
        receipt = read(backup / 'backup.json')
        if receipt.get('schema_version') != 1: raise ValueError('unsupported backup receipt')
        # A separate new evidence directory never changes the original backup.
        evidence = destination.parent / (destination.name + '.restore-evidence')
        evidence.mkdir(mode=0o700, exist_ok=False)
        verified = restore(Native(binary, evidence), backup, receipt, destination)
        write(evidence / 'result.json', verified)
        print(json.dumps(verified))


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)

#!/usr/bin/env python3
"""Qualify an explicit source archive in a new owned Ubuntu ARM64 native volume.

No host bind mounts or host installs. No publication, volume deletion or unrelated cleanup.
The copied Rust image must provide the pinned toolchain and library notices.
"""
import argparse
import hashlib
import json
import re
import shlex
from pathlib import Path, PurePosixPath
import subprocess
import tarfile
import time

from release_common import MAX_BYTES, MAX_FILES


def project_local_root(explicit=None):
    checkout = Path(__file__).resolve().parents[1]
    try:
        state = subprocess.run(['git', 'rev-parse', '--show-toplevel', '--path-format=absolute',
                                '--git-common-dir'], cwd=checkout, capture_output=True, text=True)
    except FileNotFoundError:
        state = None
    main = None
    if state is not None and state.returncode == 0:
        top, common = state.stdout.strip().splitlines()
        # A source archive nested beneath a checkout must not inherit its parent's
        # Git metadata. Only this script's own checkout can identify its main tree.
        if Path(top).resolve(strict=True) == checkout:
            common = Path(common).resolve(strict=True)
            if common.name != '.git':
                raise ValueError('cannot identify main checkout from Git common directory')
            main = common.parent
    if explicit is not None:
        selected = explicit.resolve(strict=True)
        if main is not None and selected != main:
            raise ValueError('--project-root differs from the Git-identified main checkout')
        if not checkout.is_relative_to(selected):
            raise ValueError('script checkout must be inside the selected project root')
        main = selected
    elif main is None:
        main = checkout
    if not all((main / name).is_file() for name in ['Cargo.toml', 'Cargo.lock']):
        raise ValueError('project root must contain Cargo.toml and Cargo.lock')
    local = (main / '.artifacts/local').resolve(strict=True)
    if not local.is_relative_to(main):
        raise ValueError('project artifact directory resolves outside the main checkout')
    return local


def local_path(path, base, *, existing=False):
    resolved = path.resolve(strict=existing)
    if resolved == base or not resolved.is_relative_to(base):
        raise ValueError('input/output must resolve under main project .artifacts/local')
    if existing and not resolved.is_file():
        raise ValueError('archive must resolve to a regular file')
    return resolved


def preflight_source_archive(path, expected_archive, expected_source,
                             *, max_files=MAX_FILES, max_bytes=MAX_BYTES):
    if path.stat().st_size > max_bytes:
        raise ValueError('source archive byte limit exceeded')
    with path.open('rb') as stream:
        actual_archive = hashlib.file_digest(stream, 'sha256').hexdigest()
    if actual_archive != expected_archive:
        raise ValueError('source archive digest mismatch')
    files = {}
    ancestors = set()
    total = 0
    with tarfile.open(path, 'r:*') as archive:
        for entry in archive:
            name = PurePosixPath(entry.name)
            if (entry.type not in {tarfile.REGTYPE, tarfile.AREGTYPE} or entry.sparse is not None
                    or entry.linkname or entry.mode not in {0o644, 0o755}
                    or name.is_absolute() or '..' in name.parts
                    or not name.parts or name.as_posix() != entry.name
                    or any(character in entry.name for character in ['\\', '\n', '\r', '\0'])):
                raise ValueError('unsafe source archive entry: ' + repr(entry.name))
            parents = {parent.as_posix() for parent in name.parents if parent != PurePosixPath('.')}
            if entry.name in files or entry.name in ancestors or parents.intersection(files):
                raise ValueError('duplicate or overlapping source archive entry: ' + entry.name)
            if entry.size < 0 or entry.size > max_bytes - total or len(files) >= max_files:
                raise ValueError('source archive count/byte limit exceeded')
            total += entry.size
            ancestors.update(parents)
            with archive.extractfile(entry) as stream:
                digest = hashlib.file_digest(stream, 'sha256').hexdigest()
            files[entry.name] = digest
    source = hashlib.sha256()
    for name, digest in sorted(files.items(), key=lambda item: item[0].encode()):
        encoded = name.encode()
        source.update(len(encoded).to_bytes(8, 'little'))
        source.update(encoded)
        source.update(b'file\0')
        source.update(digest.encode())
    if source.hexdigest() != expected_source:
        raise ValueError('source manifest digest mismatch')
    return {'archive_sha256': actual_archive, 'source_manifest_sha256': source.hexdigest(),
            'file_count': len(files), 'expanded_bytes': total}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--archive', type=Path, required=True)
    p.add_argument('--project-root', type=Path, help='explicit enclosing project for a Gitless source archive; Git worktrees must use their identified main checkout')
    p.add_argument('--archive-sha256', required=True, help='trusted SHA-256 of the complete source archive')
    p.add_argument('--source-sha256', required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--name', required=True)
    p.add_argument('--rust-container', required=True)
    p.add_argument('--resume-after', choices=['format', 'clippy', 'default-tests', 'fault-tests', 'fault-clippy'], help='resume only an exact owned container; retain earlier gate records')
    p.add_argument('--reuse-candidate-1', action='store_true', help='resume with existing first candidate; extraction still verifies its source and every checksum')
    p.add_argument('--preflight-only', action='store_true', help='validate and freeze source input without contacting Docker')
    a = p.parse_args()
    if not all(re.fullmatch('[0-9a-f]{64}', digest) for digest in [a.source_sha256, a.archive_sha256]):
        raise ValueError('archive and source digests must be lowercase SHA-256 values')
    if a.reuse_candidate_1 and not a.resume_after:
        raise ValueError('candidate reuse requires an explicitly verified resume')
    if not a.name.startswith('izu-release-native-') or any(c not in 'abcdefghijklmnopqrstuvwxyz0123456789-' for c in a.name):
        raise ValueError('name must be an explicit izu-release-native- owner name')
    local = project_local_root(a.project_root)
    a.archive = local_path(a.archive, local, existing=True)
    a.output = local_path(a.output, local)
    # Validate before creating output or contacting Docker. Copying into an exclusive
    # owned file then checking again binds later extraction to the checked bytes.
    preflight_source_archive(a.archive, a.archive_sha256, a.source_sha256)
    a.output.mkdir(parents=True, exist_ok=bool(a.resume_after))
    frozen_archive = local_path(a.output / 'validated-source.tar', local,
                                existing=bool(a.resume_after))
    if a.resume_after:
        preflight = preflight_source_archive(frozen_archive, a.archive_sha256, a.source_sha256)
    else:
        with a.archive.open('rb') as source, frozen_archive.open('xb') as target:
            copied = 0
            while chunk := source.read(1024 * 1024):
                copied += len(chunk)
                if copied > MAX_BYTES:
                    raise ValueError('source archive grew beyond byte limit while freezing')
                target.write(chunk)
        frozen_archive.chmod(0o444)
        preflight = preflight_source_archive(frozen_archive, a.archive_sha256, a.source_sha256)
        (a.output / 'source-preflight.json').write_text(json.dumps(preflight, indent=2) + '\n')
    if a.preflight_only:
        print(json.dumps(preflight, sort_keys=True))
        return
    records = json.loads((a.output / 'commands.json').read_text()) if a.resume_after else []

    def run(label, command, required=True):
        started = time.time()
        with (a.output / (label + '.log')).open('xb') as log:
            result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        records.append({'label': label, 'command': command, 'exit_code': result.returncode,
                        'seconds': time.time() - started,
                        'log_sha256': hashlib.sha256((a.output / (label + '.log')).read_bytes()).hexdigest()})
        (a.output / 'commands.json').write_text(json.dumps(records, indent=2) + '\n')
        print(label + ': ' + str(result.returncode), flush=True)
        if result.returncode and required:
            raise RuntimeError(label + ' failed; inspect raw log')

    if a.resume_after:
        state = json.loads(subprocess.check_output(['docker', 'inspect', a.name]))[0]
        if state['Config']['Labels'].get('izu.owner') != a.name:
            raise ValueError('resume ownership mismatch')
        if state['HostConfig']['Init'] is not True:
            raise ValueError('qualification requires an init process that reaps orphaned children')
        mounts = state['Mounts']
        if len(mounts) != 1 or mounts[0]['Type'] != 'volume' or mounts[0]['Name'] != a.name or mounts[0]['Destination'] != '/src':
            raise ValueError('resume mount mismatch')
        run('resume-source', ['docker', 'exec', '-w', '/src', a.name, 'python3', '-B', '-c', "import sys;sys.path.insert(0,'scripts');from release_common import source_manifest;from pathlib import Path;assert source_manifest(Path('/src'))['source_manifest_sha256']=='" + a.source_sha256 + "'"])
    else:
        for kind in ['container', 'volume']:
            if subprocess.run(['docker', kind, 'inspect', a.name], stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL).returncode == 0:
                raise ValueError('refusing existing ' + kind + ': ' + a.name)
        run('volume', ['docker', 'volume', 'create', '--label', 'izu.owner=' + a.name, a.name])
        run('container', ['docker', 'run', '-d', '--init', '--platform', 'linux/arm64', '--name', a.name,
                         '--label', 'izu.owner=' + a.name, '--network', 'bridge', '--cpus', '2',
                         '--memory', '6g', '--mount', 'type=volume,src=' + a.name + ',dst=/src',
                         'ubuntu:24.04', 'sleep', 'infinity'])
        run('inspect', ['docker', 'inspect', a.name])
        run('apt', ['docker', 'exec', a.name, 'sh', '-ec',
                    'apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends '
                    'python3 build-essential git ca-certificates binutils curl file e2fsprogs util-linux time && '
                    'mkdir -p /src/.artifacts/local/tools /src/.artifacts/local/results'])
        for directory in ['cargo', 'rustup']:
            # docker cp streams archive bytes. Neither container is started or mutated by a source read.
            with (a.output / ('tool-copy-' + directory + '.log')).open('xb') as log:
                producer = subprocess.Popen(['docker', 'cp', a.rust_container + ':/usr/local/' + directory, '-'], stdout=subprocess.PIPE, stderr=log)
                consumer = subprocess.run(['docker', 'cp', '-', a.name + ':/src/.artifacts/local/tools/'], stdin=producer.stdout, stdout=log, stderr=log)
                producer.stdout.close()
                if producer.wait() or consumer.returncode:
                    raise RuntimeError('tool copy failed: ' + directory)
        preflight_source_archive(frozen_archive, a.archive_sha256, a.source_sha256)
        run('source-copy', ['docker', 'cp', str(frozen_archive), a.name + ':/src/.artifacts/local/source.tar'])
        run('source-guest-digest', ['docker', 'exec', a.name, 'sh', '-ec',
                                  'test "$(sha256sum /src/.artifacts/local/source.tar | cut -d " " -f1)" = ' + a.archive_sha256])
        run('source-extract', ['docker', 'exec', a.name, 'tar', '-xf', '/src/.artifacts/local/source.tar', '-C', '/src'])
        run('identity', ['docker', 'exec', '-w', '/src', a.name, 'sh', '-ec',
                         'uname -a; cat /etc/os-release; ldd --version; findmnt -T /src; stat -f /src; '
                         'df -h /src; cat /proc/cpuinfo; scripts/izu-env rustc -vV; scripts/izu-env cargo -V; '
                         'scripts/izu-env rustup show; python3 -B -c '\
                         + shlex.quote("import sys;sys.path.insert(0,'scripts');from release_common import source_manifest;from pathlib import Path;m=source_manifest(Path('/src'));assert m['source_manifest_sha256']=='" + a.source_sha256 + "';print(m['source_manifest_sha256'],len(m['files']))")])
    checks = [
        ('format', ['fmt', '--all', '--', '--check']),
        ('clippy', ['clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings']),
        ('default-tests', ['test', '--workspace', '--locked']),
        ('fault-tests', ['test', '-p', 'izu-store', '-p', 'izu-engine', '-p', 'izu-bundle', '-p', 'izu-runtime', '--features', 'izu-store/fault-injection,izu-engine/fault-injection,izu-bundle/fault-injection,izu-runtime/fault-injection', '--locked']),
        ('fault-clippy', ['clippy', '-p', 'izu-store', '-p', 'izu-engine', '-p', 'izu-bundle', '-p', 'izu-runtime', '--all-targets', '--features', 'izu-store/fault-injection,izu-engine/fault-injection,izu-bundle/fault-injection,izu-runtime/fault-injection', '--locked', '--', '-D', 'warnings']),
    ]
    pending = not a.resume_after
    for label, args in checks:
        if not pending:
            pending = label == a.resume_after
            continue
        run(label, ['docker', 'exec', '-w', '/src', a.name, 'python3', '-B', 'scripts/record-verification.py', label, '/src/scripts/izu-env', 'cargo', *args], required=False)
    run('package-input-tests', ['docker', 'exec', '-w', '/src', a.name, 'python3', '-B', 'scripts/release-tests.py'])
    for number in [1, 2]:
        if number == 1 and a.reuse_candidate_1:
            run('existing-package-source', ['docker', 'exec', a.name, 'python3', '-B', '-c', "import json;from pathlib import Path;p=Path('/src/.artifacts/local/candidate-1/package-result.json');assert json.loads(p.read_text())['source_manifest_sha256']=='" + a.source_sha256 + "'"])
        else:
            run('package-' + str(number), ['docker', 'exec', '-w', '/src', a.name, 'python3', '-B', 'scripts/package-release.py', '--output', '/src/.artifacts/local/candidate-' + str(number), '--cargo-wrapper', '/src/scripts/izu-env', '--offline'])
        code = "import json,subprocess;from pathlib import Path;p=Path('/src/.artifacts/local/candidate-" + str(number) + "');r=json.loads((p/'package-result.json').read_text());subprocess.run(['python3','-B','scripts/release-verify.py','--archive',str(p/r['archive']),'--expected-sha256',r['archive_sha256'],'--work-root','/src/.artifacts/local/extracted-" + str(number) + "'],check=True)"
        run('extracted-' + str(number), ['docker', 'exec', '-w', '/src', a.name, 'python3', '-B', '-c', code])
    run('reproducibility', ['docker', 'exec', a.name, 'python3', '-B', '-c', "import json;from pathlib import Path;a=json.loads(Path('/src/.artifacts/local/candidate-1/package-result.json').read_text());b=json.loads(Path('/src/.artifacts/local/candidate-2/package-result.json').read_text());assert a==b,(a,b);print(json.dumps(a,indent=2))"])
    run('abi', ['docker', 'exec', a.name, 'sh', '-ec', 'file /src/.artifacts/local/candidate-1/izu-*/bin/*; for b in /src/.artifacts/local/candidate-1/izu-*/bin/*; do sha256sum "$b"; readelf -h "$b"; readelf -l "$b"; readelf --version-info "$b"; ldd "$b"; done'])
    run('receipts-copy', ['docker', 'cp', a.name + ':/src/.artifacts/local/verification', str(a.output / 'verification')])
    for name in ['candidate-1', 'candidate-2', 'extracted-1', 'extracted-2']:
        run(name + '-copy', ['docker', 'cp', a.name + ':/src/.artifacts/local/' + name, str(a.output / name)])
    run('stop-owned', ['docker', 'stop', a.name])
    failures = [record['label'] for record in records if record['exit_code']]
    if failures:
        raise SystemExit('Qualification incomplete; failed gates: ' + ', '.join(failures))


if __name__ == '__main__':
    main()

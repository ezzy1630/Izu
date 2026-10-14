#!/usr/bin/env python3
"""Build an unpublished deterministic archive from frozen source. No installation."""
import sys
sys.dont_write_bytecode = True
import argparse
import gzip
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tomllib

from release_common import contained_new_output, sha, source_manifest, write_json


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--output', type=Path, required=True, help='new directory under source .artifacts/local')
    parser.add_argument('--cargo-wrapper', type=Path, help='absolute scripts/izu-env for the owned local toolchain')
    parser.add_argument('--offline', action='store_true')
    args = parser.parse_args()
    root = args.source_root.resolve()
    output = contained_new_output(args.output, root)
    original = source_manifest(root)
    version = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['package']['version']
    package = output / ('izu-' + version)
    frozen = package / 'source'
    frozen.mkdir(parents=True)
    for relative, entry in original['files'].items():
        destination = frozen / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(root / relative, destination)
        destination.chmod(0o755 if entry['executable'] else 0o644)
        if sha(destination) != entry['sha256']:
            raise ValueError('source changed during freeze: ' + relative)
    if source_manifest(frozen) != original or source_manifest(root) != original:
        raise ValueError('source changed during freeze')
    wrapper = []
    if args.cargo_wrapper:
        if not args.cargo_wrapper.is_absolute():
            raise ValueError('--cargo-wrapper must be absolute')
        wrapper = [str(args.cargo_wrapper.resolve())]
    environment = os.environ.copy()
    # Compile with a stable logical source path, including dependency source paths.
    flags = environment.get('RUSTFLAGS', '')
    if flags or environment.get('CARGO_ENCODED_RUSTFLAGS'):
        raise ValueError('ambient Rust flags must be unset for the default-feature release build')
    environment['RUSTFLAGS'] = '--remap-path-prefix=' + str(frozen) + '=/izu/source'
    if wrapper:
        main_root = Path(wrapper[0]).parents[1]
        environment['RUSTFLAGS'] += ' --remap-path-prefix=' + str(main_root / '.artifacts/local/tools') + '=/izu/tools'
    else:
        cargo_home = Path(environment.get('CARGO_HOME', str(Path.home() / '.cargo'))).resolve()
        environment['RUSTFLAGS'] += ' --remap-path-prefix=' + str(cargo_home) + '=/izu/cargo'
    target = frozen / 'target-izu-internal'
    environment['CARGO_TARGET_DIR'] = str(target)
    environment['CARGO_BUILD_JOBS'] = '2'
    environment['CARGO_TERM_COLOR'] = 'never'
    environment['PYTHONDONTWRITEBYTECODE'] = '1'
    environment['TMPDIR'] = str(output / 'tmp')
    (output / 'tmp').mkdir()

    def run(command):
        result = subprocess.run(wrapper + command, cwd=frozen, env=environment, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if result.returncode:
            (output / 'failed-command.stdout').write_bytes(result.stdout)
            (output / 'failed-command.stderr').write_bytes(result.stderr)
            raise RuntimeError('command failed: ' + repr(command) + '; see failed-command.stderr')
        return result

    compiler = run(['rustc', '-vV']).stdout.decode()
    cargo = run(['cargo', '-V']).stdout.decode().strip()
    sysroot = Path(run(['rustc', '--print', 'sysroot']).stdout.decode().strip())
    rust_notice = sysroot / 'share/doc/rust/COPYRIGHT-library.html'
    if not rust_notice.is_file():
        raise ValueError('toolchain COPYRIGHT-library.html missing; install rust-docs for pinned toolchain')
    common = ['--locked'] + (['--offline'] if args.offline else [])
    build_args = ['cargo', 'build', *common, '--release', '-p', 'izu-cli', '-p', 'izu-lab', '--message-format=json']
    build = run(build_args)
    (output / 'build.stdout').write_bytes(build.stdout)
    (output / 'build.stderr').write_bytes(build.stderr)
    artifacts = [json.loads(line) for line in build.stdout.decode().splitlines() if line.startswith('{')]
    used = {event['package_id'] for event in artifacts if event.get('reason') == 'compiler-artifact'}
    host = next(line.split(': ', 1)[1] for line in compiler.splitlines() if line.startswith('host: '))
    metadata = json.loads(run(['cargo', 'metadata', *common, '--format-version=1', '--filter-platform', host]).stdout)
    locked = {(item['name'], item['version'], item.get('source')): item.get('checksum')
              for item in tomllib.loads((frozen / 'Cargo.lock').read_text())['package']}
    dependencies = []
    notices = package / 'licenses'
    notices.mkdir()
    shutil.copyfile(rust_notice, notices / 'RUST-LIBRARY-NOTICES.html')
    for dependency in sorted(metadata['packages'], key=lambda item: (item['name'], item['version'])):
        if dependency['id'] not in used or dependency['source'] is None:
            continue
        crate = Path(dependency['manifest_path']).parent
        package_checksum = locked[(dependency['name'], dependency['version'], dependency['source'])]
        crate_archive = crate.parents[2] / 'cache' / crate.parent.name / (crate.name + '.crate')
        if not package_checksum or sha(crate_archive) != package_checksum:
            raise ValueError('upstream crate archive checksum mismatch: ' + dependency['id'])
        checked_files = {}
        with tarfile.open(crate_archive, 'r:gz') as upstream:
            for member in upstream:
                if member.isdir():
                    continue
                prefix = crate.name + '/'
                if not member.isfile() or not member.name.startswith(prefix):
                    raise ValueError('unexpected upstream crate archive entry')
                relative = member.name[len(prefix):]
                if '..' in Path(relative).parts or Path(relative).is_absolute() or relative in checked_files:
                    raise ValueError('unsafe upstream crate archive entry')
                with upstream.extractfile(member) as stream:
                    import hashlib
                    digest = hashlib.file_digest(stream, 'sha256').hexdigest()
                if sha(crate / relative) != digest:
                    raise ValueError('unpacked dependency changed: ' + dependency['id'] + '/' + relative)
                checked_files[relative] = digest
        license_files = []
        # Preserve upstream legal texts, including nested third-party notices.
        for path in sorted(crate.rglob('*')):
            if not path.is_file() or path.is_symlink():
                continue
            relative = path.relative_to(crate).as_posix()
            if not any(component.upper().startswith(('LICENSE', 'LICENCE', 'COPYING', 'NOTICE', 'COPYRIGHT'))
                       for component in path.relative_to(crate).parts):
                continue
            if sha(path) != checked_files.get(relative):
                raise ValueError('license absent from crate checksums or modified: ' + str(path))
            destination = notices / (dependency['name'] + '-' + dependency['version']) / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, destination)
            license_files.append({'path': destination.relative_to(package).as_posix(), 'sha256': sha(destination)})
        if not license_files or not dependency['license']:
            raise ValueError('dependency license review required: ' + dependency['id'])
        dependencies.append({'name': dependency['name'], 'version': dependency['version'],
                             'source': dependency['source'], 'license': dependency['license'],
                             'package_checksum': package_checksum, 'license_files': license_files})
    covered = {(item['source'] + '#' + item['name'] + '@' + item['version']) for item in dependencies}
    external = {item for item in used if not item.startswith('path+')}
    if external != covered:
        raise ValueError('uncovered compiler dependency identities: ' + repr(external ^ covered))
    binaries = {}
    (package / 'bin').mkdir()
    for name in ['izu', 'izu-lab']:
        found = [Path(event['executable']) for event in artifacts if event.get('reason') == 'compiler-artifact'
                 and event.get('executable') and event['target']['name'] == name]
        if len(found) != 1:
            raise ValueError('missing or ambiguous binary: ' + name)
        destination = package / 'bin' / name
        shutil.copyfile(found[0], destination)
        destination.chmod(0o755)
        binaries[name] = sha(destination)
    for name in ['NOTICE', 'LICENSE-APACHE']:
        shutil.copyfile(frozen / name, package / name)
    if source_manifest(frozen) != original or source_manifest(root) != original:
        raise ValueError('source changed during build')
    write_json(package / 'SOURCE-MANIFEST.json', original)
    write_json(package / 'DEPENDENCIES.json', dependencies)
    write_json(package / 'BUILD-MANIFEST.json', {
        'schema_version': 1, 'version': version, 'source_manifest_sha256': original['source_manifest_sha256'],
        'cargo_lock_sha256': sha(frozen / 'Cargo.lock'), 'rustc_verbose': compiler, 'cargo_version': cargo,
        'build_command': build_args, 'rustflags': '--remap-path-prefix=<frozen-source>=/izu/source; <tool-cache>=/izu/tools-or-cargo',
        'profile': 'release default features', 'binary_sha256': binaries,
        'build_host': {'system': platform.system(), 'machine': platform.machine()},
        'scope': 'Unpublished unsigned build candidate; extraction acceptance is recorded separately. No support or ABI claim.'})
    # Build outputs never enter the source archive.
    build_target = Path(metadata['target_directory']).resolve()
    if not build_target.is_relative_to(frozen):
        raise ValueError('Cargo target escaped frozen source')
    shutil.rmtree(build_target)
    files = sorted((path for path in package.rglob('*') if path.is_file()), key=lambda path: os.fsencode(path.relative_to(package)))
    (package / 'SHA256SUMS').write_text(''.join(sha(path) + '  ' + path.relative_to(package).as_posix() + '\n' for path in files))
    archive = output / (package.name + '.tar.gz')
    with archive.open('xb') as raw, gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode='w', format=tarfile.USTAR_FORMAT) as tar:
            for path in sorted(package.rglob('*'), key=lambda path: os.fsencode(path.relative_to(package))):
                if not path.is_file():
                    continue
                entry = tar.gettarinfo(str(path), arcname=path.relative_to(package).as_posix())
                entry.uid = entry.gid = entry.mtime = 0
                entry.uname = entry.gname = ''
                entry.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
                with path.open('rb') as stream:
                    tar.addfile(entry, stream)
    write_json(output / 'package-result.json', {'schema_version': 1, 'archive': archive.name,
               'archive_sha256': sha(archive), 'source_manifest_sha256': original['source_manifest_sha256'],
               'binary_sha256': binaries, 'dependencies': len(dependencies)})
    print(json.dumps({'archive': str(archive), 'sha256': sha(archive)}, sort_keys=True))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, RuntimeError, OSError, KeyError) as error:
        raise SystemExit(str(error))

"""Record local verification against an exact checkout; never changes source."""
from pathlib import Path
import datetime
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
BASE = ROOT / '.artifacts/verification'


def sha(path):
    with path.open('rb') as handle:
        return hashlib.file_digest(handle, 'sha256').hexdigest()


def source():
    files = {}
    for parent, dirs, names in os.walk(ROOT, followlinks=False):
        directory_links = [name for name in dirs if (Path(parent) / name).is_symlink()]
        dirs[:] = sorted(d for d in dirs if d not in directory_links and d not in {'.git', '.artifacts', 'target', 'node_modules', 'lab-results'} and not d.startswith('target-') and not (Path(parent) == ROOT and d == '.izu'))
        for name in sorted(names + directory_links):
            if name == '.git':
                continue
            path = Path(parent) / name
            relative = path.relative_to(ROOT).as_posix()
            files[relative] = {'kind': 'symlink', 'target': os.readlink(path)} if path.is_symlink() else {'kind': 'file', 'sha256': sha(path), 'bytes': path.stat().st_size}
    digest = hashlib.sha256()
    for relative, info in sorted(files.items(), key=lambda item: os.fsencode(item[0])):
        encoded = os.fsencode(relative)
        digest.update(len(encoded).to_bytes(8, 'little'))
        digest.update(encoded)
        digest.update((info['kind'] + '\0').encode())
        digest.update(os.fsencode(info['target']) if info['kind'] == 'symlink' else info['sha256'].encode())
    return {'source_root': str(ROOT), 'source_manifest_sha256': digest.hexdigest(), 'file_count': len(files), 'files': files}


def write_new(path, data):
    with path.open('x') as handle:
        json.dump(data, handle, indent=2)
        handle.write('\n')


def main():
    label, *command = sys.argv[1:]
    if not label or '/' in label or not command:
        raise ValueError('usage: run.py UNIQUE_LABEL COMMAND [ARGS...]')
    output = BASE / label
    output.mkdir(parents=True, exist_ok=False)
    before = source()
    write_new(output / 'source-before.json', before)
    snapshot = output / 'source'
    for relative, info in before['files'].items():
        path = snapshot / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        if info['kind'] == 'symlink':
            path.symlink_to(info['target'])
        else:
            shutil.copy2(ROOT / relative, path)
            if sha(path) != info['sha256']:
                raise RuntimeError('source changed while freezing ' + relative)
    env = os.environ.copy()
    explicit = {'TMPDIR': str(ROOT / '.artifacts/local/tmp-root'),
                'CARGO_HOME': str(ROOT / '.artifacts/local/tools/cargo'),
                'RUSTUP_HOME': str(ROOT / '.artifacts/local/tools/rustup'),
                'CARGO_BUILD_JOBS': '2',
                'CARGO_TARGET_DIR': str(ROOT / 'target-izu-internal'),
                'CARGO_TERM_COLOR': 'never', 'PYTHONDONTWRITEBYTECODE': '1'}
    for name in ['RUSTC', 'RUSTDOC', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER']:
        env.pop(name, None)
    env.update(explicit)
    Path(explicit['TMPDIR']).mkdir(parents=True, exist_ok=True)
    env['PATH'] = explicit['CARGO_HOME'] + '/bin:' + env.get('PATH', '/usr/bin:/bin')
    record = {'command': command, 'cwd': str(ROOT), 'explicit_environment': explicit, 'source_before_sha256': before['source_manifest_sha256'], 'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'log': str(output / 'run.log')}
    write_new(output / 'started.json', record)
    started = time.perf_counter()
    with (output / 'run.log').open('xb') as log:
        result = subprocess.run(command, cwd=ROOT, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
    record.update(exit_code=result.returncode, seconds=time.perf_counter() - started, finished_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(), log_sha256=sha(output / 'run.log'))
    after = source()
    write_new(output / 'source-after.json', after)
    record.update(source_after_sha256=after['source_manifest_sha256'], source_unchanged=before == after)
    write_new(output / 'result.json', record)
    print(json.dumps(record), flush=True)
    raise SystemExit(result.returncode if result.returncode else (0 if before == after else 3))


if __name__ == '__main__':
    main()

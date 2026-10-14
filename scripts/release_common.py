"""Shared local packaging primitives; archives are data until verified."""
import hashlib
import json
import os
from pathlib import Path
import stat

EXCLUDED = {'.git', '.artifacts', 'target', 'node_modules', 'lab-results', '__pycache__'}
MAX_FILES = 100000
MAX_BYTES = 2 * 1024 * 1024 * 1024


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def write_json(path, data):
    with Path(path).open('x', encoding='utf-8') as stream:
        json.dump(data, stream, sort_keys=True, indent=2, ensure_ascii=True)
        stream.write('\n')


def source_manifest(root):
    files = {}
    total_bytes = 0
    for parent, directories, names in os.walk(root, followlinks=False):
        links = [name for name in directories if (Path(parent) / name).is_symlink()]
        directories[:] = [name for name in directories if name not in links and name not in EXCLUDED
                          and not name.startswith('target-') and not (Path(parent) == root and name == '.izu')]
        for name in names + links:
            if name == '.git':
                continue
            path = Path(parent) / name
            relative = path.relative_to(root).as_posix()
            if '\n' in relative or '\r' in relative:
                raise ValueError('source names cannot contain newlines')
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                raise ValueError('release source must contain no symlinks: ' + relative)
            if not stat.S_ISREG(mode):
                raise ValueError('non-regular source entry: ' + relative)
            files[relative] = {'kind': 'file', 'sha256': sha(path), 'bytes': path.stat().st_size,
                               'executable': bool(mode & 0o111)}
            total_bytes += files[relative]['bytes']
            if total_bytes > MAX_BYTES:
                raise ValueError('source byte limit exceeded')
            if len(files) > MAX_FILES:
                raise ValueError('too many source files')
    digest = hashlib.sha256()
    for relative, entry in sorted(files.items(), key=lambda row: os.fsencode(row[0])):
        encoded = os.fsencode(relative)
        digest.update(len(encoded).to_bytes(8, 'little'))
        digest.update(encoded)
        digest.update(b'file\0')
        digest.update(entry['sha256'].encode())
    return {'source_manifest_sha256': digest.hexdigest(), 'files': files}


def contained_new_output(path, root):
    path = path.resolve()
    root = root.resolve()
    if not path.is_relative_to(root / '.artifacts' / 'local'):
        raise ValueError('output must be under the source checkout .artifacts/local')
    path.mkdir(parents=True, exist_ok=False)
    return path

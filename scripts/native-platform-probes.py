#!/usr/bin/env python3
"""Record bounded observations from owned project-local native probe fixtures."""
import json
import os
from pathlib import Path
import stat
import subprocess
import sys


def command(argv):
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=10)
        return {'argv': argv, 'status': result.returncode,
                'stdout': result.stdout[:32768], 'stderr': result.stderr[:4096]}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {'argv': argv, 'error': str(error)}


def metadata(path):
    value = path.lstat()
    return {'path': str(path), 'mode': oct(stat.S_IMODE(value.st_mode)),
            'uid': value.st_uid, 'gid': value.st_gid, 'device': value.st_dev,
            'inode': value.st_ino, 'flags': getattr(value, 'st_flags', None),
            'kind': 'symlink' if stat.S_ISLNK(value.st_mode) else
                    'directory' if stat.S_ISDIR(value.st_mode) else 'file',
            'bytes': value.st_size}


def main():
    if len(sys.argv) != 2 or sys.argv[1] not in {'rename', 'metadata'}:
        raise SystemExit('usage: native-platform-probes.py rename|metadata')
    checkout = Path(os.environ.get('GITHUB_WORKSPACE', os.getcwd())).resolve(strict=True)
    root = checkout/'.artifacts/local/native-probes'
    root.mkdir(parents=True, exist_ok=True)
    temporary = root/'t'
    temporary.mkdir(exist_ok=True)
    if root.is_symlink() or temporary.is_symlink():
        raise SystemExit('Probe fixtures must be real owned directories')
    if sys.argv[1] == 'rename':
        receipt = {'actor': command(['id']), 'host': command(['uname', '-a']),
                   'filesystem': command(['df', '-P', str(temporary)]),
                   'root': metadata(root), 'root_acl': command(['ls', '-ldeO@', str(root)]),
                   'cases': [], 'claim': 'Native syscall observations; not environment or durability qualification'}
        df_lines = receipt['filesystem'].get('stdout', '').splitlines()
        device = df_lines[-1].split()[0] if len(df_lines)>1 else None
        mounts = command(['mount'])
        receipt['mount'] = {'device': device, 'matching_lines':
            [line for line in mounts.get('stdout','').splitlines() if device and line.startswith(device+' ')]}
        for mode in [0o700, 0o500, 0o555]:
            source = temporary/f'rename-{mode:o}-staging'
            target = temporary/f'rename-{mode:o}-published'
            source.mkdir()
            child = source/'synthetic'
            child.write_bytes(b'synthetic controlled staging probe\n')
            child.chmod(0o444)
            source.chmod(mode)
            case = {'mode': oct(mode), 'before': metadata(source),
                    'acl': command(['ls', '-ldeO@', str(source)])}
            try:
                os.rename(source, target)
                case['rename'] = 'success'
                case['after'] = metadata(target)
                descriptor = os.open(target, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(descriptor)
                    case['directory_fsync'] = 'success'
                except OSError as error:
                    case['directory_fsync'] = {'errno': error.errno, 'message': str(error)}
                finally:
                    os.close(descriptor)
            except OSError as error:
                case['rename'] = {'errno': error.errno, 'message': str(error)}
            receipt['cases'].append(case)
        (root/'controlled-stage-rename.json').write_text(json.dumps(receipt,indent=2)+'\n')
        print(json.dumps({'rename_results':[case['rename'] for case in receipt['cases']]}))
        if receipt['cases'][0]['rename'] != 'success':
            raise SystemExit('Writable controlled rename failed; inspect retained receipt')
    else:
        entries = []
        pending = [temporary]
        while pending and len(entries)<100:
            path = pending.pop(0)
            try:
                value = metadata(path)
                value['acl'] = command(['ls', '-ldeO@', str(path)]) if len(entries)<10 else None
                entries.append(value)
                if value['kind']=='directory':
                    with os.scandir(path) as children:
                        for child in children:
                            if len(pending)+len(entries)>=100:
                                break
                            pending.append(Path(child.path))
            except OSError as error:
                entries.append({'path': str(path), 'error': str(error)})
        (root/'owned-fixture-metadata.json').write_text(json.dumps({'root':str(temporary),
            'limit':100,'bounded_or_truncated':bool(pending),'entries':entries,
            'scope':'lstat/ACL metadata only; no file contents and no symlink traversal'},indent=2)+'\n')
        print('Retained owned fixture metadata:',len(entries),'entries')


if __name__ == '__main__':
    main()

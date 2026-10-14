#!/usr/bin/env python3
"""Failure-only capture; the Rust owner retains and bounds this whole process group."""
import faulthandler
import sys
import time

BOOT_STARTED = time.monotonic()
if __name__ == '__main__':
    print(f'diagnostic bootstrap unix_ns={time.time_ns()}', file=sys.stderr, flush=True)
    faulthandler.dump_traceback_later(4, file=sys.stderr)

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import platform
import selectors
import subprocess
import threading

CAPTURE_BYTES = 1024 * 1024


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--parent', type=int, required=True)
    parser.add_argument('--child', type=int, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--test-probe', choices=['slow-command', 'failed-command', 'backpressure-command'],
                        help=argparse.SUPPRESS)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    output = args.output.resolve()
    output.relative_to(root / '.artifacts/local')
    # The owner creates the report before Python starts, so startup failure or
    # termination never erases the only diagnostic. Only this helper writes it
    # until the owner has stopped and reaped the entire process group.
    record = json.loads((output / 'capture.json').read_text())
    record.update(status='running', helper_pid=os.getpid(), platform=platform.system(), commands=[],
                  bootstrap_unix_ns=time.time_ns(), imports_seconds=time.monotonic() - BOOT_STARTED)
    started = time.monotonic()
    lock = threading.Lock()

    def save():
        record['seconds'] = time.monotonic() - started
        temporary = output / 'capture-helper.tmp'
        temporary.write_text(json.dumps(record, indent=2) + '\n')
        temporary.replace(output / 'capture.json')

    def update(row, **values):
        with lock:
            row.update(values)
            save()

    def plan(kind, argv, bound):
        row = {'kind': kind, 'argv': argv, 'bound_seconds': bound, 'state': 'pending'}
        with lock:
            record['commands'].append(row)
            save()
        return row

    def command(row):
        update(row, state='starting', attempted=True)
        captures = {name: bytearray() for name in ['stdout', 'stderr']}
        dropped = {name: 0 for name in captures}
        process = None
        selector = selectors.DefaultSelector()
        try:
            process = subprocess.Popen(row['argv'], stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            update(row, state='running', pid=process.pid)
            for name in captures:
                pipe = getattr(process, name)
                os.set_blocking(pipe.fileno(), False)
                selector.register(pipe, selectors.EVENT_READ, name)
            deadline = time.monotonic() + row['bound_seconds']
            timed_out = False
            while selector.get_map() or process.poll() is None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    timed_out = True
                    break
                for key, _ in selector.select(min(remaining, 0.02)):
                    data = os.read(key.fileobj.fileno(), 65536)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    name = key.data
                    keep = min(len(data), CAPTURE_BYTES - len(captures[name]))
                    captures[name].extend(data[:keep])
                    dropped[name] += len(data) - keep
            if timed_out:
                # This Popen still owns its unreaped direct child. Descendants
                # remain in the Rust-owned group and are stopped before reaping
                # the group leader, even if this command ignores/escapes its pipe.
                process.kill()
                update(row, state='timed_out')
            try:
                status = process.wait(timeout=0.2)
                update(row, state='timed_out' if timed_out else 'completed',
                       exit_code=status, direct_child_reaped=True)
            except subprocess.TimeoutExpired:
                update(row, state='cleanup_pending', direct_child_reaped=False)
        except OSError as error:
            update(row, state='failed', error=str(error))
        finally:
            selector.close()
            if process is not None:
                for name in captures:
                    getattr(process, name).close()
            for name, data in captures.items():
                path = output / (row['kind'] + '.' + name)
                path.write_bytes(data)
                update(row, **{name: str(path), name + '_dropped_bytes': dropped[name]})
        return bytes(captures['stdout'])

    save()
    identity = command(plan('ownership', ['/bin/ps', '-p', str(args.child), '-o', 'pid=,ppid='], 0.5))
    owned = identity.split() == [str(args.child).encode(), str(args.parent).encode()]
    with lock:
        record['child_owned_at_start'] = owned
        save()
    if not owned:
        with lock:
            record.update(status='refused', refusal='Child is no longer a direct child of the test; no stack or FD access')
            save()
        return

    tasks = [plan('process_state', ['/bin/ps', '-p', f'{args.parent},{args.child}',
                                    '-o', 'pid,ppid,pgid,state,etime,time,command'], 0.5)]
    if platform.system() == 'Darwin':
        tasks += [plan('pipe_ownership', ['/usr/sbin/lsof', '-nP', '-a', '-p',
                                          f'{args.parent},{args.child}', '-FpcfaDtin'], 1),
                  plan('child_stack', ['/usr/bin/sample', str(args.child), '1', '-file', '/dev/stdout'], 2.5),
                  plan('parent_stack', ['/usr/bin/sample', str(args.parent), '1', '-file', '/dev/stdout'], 2.5)]
    else:
        # Keep /proc access inside a native child too, so a slow filesystem read
        # cannot block the helper's parallel workers past the owned deadline.
        probe = r'''import json,os,sys
from pathlib import Path
kind=sys.argv[1]; result={}
for value in sys.argv[2:]:
    pid=int(value); proc=Path('/proc')/value; rows={}
    try:
        if kind=='pipe_ownership':
            for fd in (proc/'fd').iterdir():
                try: rows[fd.name]=os.readlink(fd)
                except OSError as e: rows[fd.name]={'error':str(e)}
        else:
            for task in (proc/'task').iterdir():
                rows[task.name]={}
                for name in ['stack','wchan','status']:
                    try: rows[task.name][name]=(task/name).read_text()[:65536]
                    except OSError as e: rows[task.name][name]={'error':str(e)}
    except OSError as e: rows['error']=str(e)
    result[value]=rows
print(json.dumps(result))'''
        for kind, pids in [('pipe_ownership', [args.parent, args.child]),
                           ('child_stack', [args.child]), ('parent_stack', [args.parent])]:
            tasks.append(plan(kind, [sys.executable, '-B', '-c', probe, kind, *map(str, pids)], 1))
    if args.test_probe:
        # Test only the diagnostic transport. Real pipe and child-stack attempts
        # remain intact; a native descendant proves whole-group cleanup.
        target = next(row for row in tasks if row['kind'] == 'parent_stack')
        if args.test_probe == 'slow-command':
            script = "import os,subprocess,sys,time; print('controlled slow native command',flush=True); child=subprocess.Popen([sys.executable,'-B','-c','import time; time.sleep(30)']); open(sys.argv[1],'w').write(str(child.pid)); time.sleep(30)"
            update(target, argv=[sys.executable, '-B', '-c', script, str(output / 'native-descendant.pid')], bound_seconds=0.5)
        elif args.test_probe == 'backpressure-command':
            script = """import os,subprocess,sys,time
child=subprocess.Popen([sys.executable,'-B','-c','import time; time.sleep(30)'])
with open(sys.argv[1],'w') as stream: stream.write(str(child.pid))
for descriptor in [1,2]:
    remaining=2*1024*1024
    while remaining:
        written=os.write(descriptor,b'x'*min(remaining,65536))
        if written==0: sys.exit('pipe write made no progress')
        remaining-=written
time.sleep(30)
"""
            update(target, argv=[sys.executable, '-B', '-c', script, str(output / 'native-descendant.pid')])
        else:
            update(target, argv=[sys.executable, '-B', '-c', "import sys; sys.stderr.write('controlled native command failure\\n'); sys.exit(17)"])
    # All workers are joined before the helper exits. The Rust owner kills the
    # whole cooperative group at its deadline or after helper completion.
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        for result in executor.map(command, tasks):
            del result
    with lock:
        record['status'] = 'completed'
        save()


if __name__ == '__main__':
    # Native probes have shorter bounds than this watchdog. If their helper
    # stalls too, retain its Python thread positions before the Rust owner
    # stops the whole group at the unchanged outer deadline.
    try:
        main()
    finally:
        faulthandler.cancel_dump_traceback_later()

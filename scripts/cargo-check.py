#!/usr/bin/env python3
"""Run a Cargo check with private build outputs outside captured source.

Cargo incremental caches use hard links, which Izu deliberately refuses to
capture as source. Keep those outputs separate rather than relaxing recovery
validation or silently deleting them during workspace close.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-root', type=Path, required=True)
    parser.add_argument('--cargo-wrapper', type=Path,
                        help='optional pinned-toolchain wrapper, such as scripts/izu-env')
    parser.add_argument('arguments', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    argv = args.arguments
    if argv[:1] == ['--']: argv = argv[1:]
    if not argv: argv = ['test', '--locked']
    if argv[0] not in ('test', 'check', 'build', 'clippy', 'bench'):
        parser.error('supported Cargo commands: test, check, build, clippy, bench')
    cargo_flags = argv[:argv.index('--')] if '--' in argv else argv
    if any(x == '--target-dir' or x.startswith('--target-dir=') for x in cargo_flags):
        parser.error('the check owns --target-dir; select --state-root instead')
    source = Path.cwd().resolve()
    state = args.state_root.resolve()
    if state.is_relative_to(source): parser.error('build state must be outside captured source')
    if args.state_root.is_symlink(): parser.error('build state must not be a symlink')
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    if state.stat().st_uid != os.getuid() or state.stat().st_mode & 0o077:
        parser.error('build state must be owned by the current user and private (0700)')
    run = Path(tempfile.mkdtemp(prefix='cargo-check-', dir=state))
    target = run / 'target'
    temporary = run / 'tmp'; temporary.mkdir(mode=0o700)
    command = ([str(args.cargo_wrapper.resolve(strict=True)), 'cargo'] if args.cargo_wrapper else
               [shutil.which('cargo') or 'cargo'])
    command += [argv[0], '--target-dir', str(target), *argv[1:]]
    environment = os.environ.copy(); environment['TMPDIR'] = str(temporary)
    print(json.dumps({'build_output': str(run), 'source': str(source)}), file=sys.stderr, flush=True)
    interrupted = False
    def interrupt(_signal, _frame):
        nonlocal interrupted
        interrupted = True
    previous = {number: signal.signal(number, interrupt) for number in (signal.SIGINT, signal.SIGTERM)}
    process = None
    try:
        if interrupted: return 130
        process = subprocess.Popen(command, env=environment)
        while True:
            if interrupted:
                if process.poll() is None: process.send_signal(signal.SIGINT)
                try: process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill(); process.wait(timeout=5)
                return 130
            try: return process.wait(timeout=.1)
            except subprocess.TimeoutExpired: pass
    finally:
        for number, handler in previous.items(): signal.signal(number, handler)
        if process is not None:
            if process.poll() is None: process.kill(); process.wait(timeout=5)
            (run / 'result.json').write_text(json.dumps({'argv': command, 'source': str(source),
                'exit_code': process.returncode, 'interrupted': interrupted,
                'build_outputs_retained': True}, indent=2)+'\n')


if __name__ == '__main__':
    try: raise SystemExit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr); raise SystemExit(1)

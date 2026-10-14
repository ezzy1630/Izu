#!/usr/bin/env python3
"""Bounded real-CLI native scale evidence. No third-party Python packages."""
import argparse
import concurrent.futures
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import threading
import time

MAX_PAYLOAD = 256 * 1024 * 1024


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def write_json(path, data):
    with Path(path).open('x') as stream:
        json.dump(data, stream, indent=2, sort_keys=True)
        stream.write('\n')


def disk(root):
    logical = allocated = entries = 0
    for parent, dirs, names in os.walk(root, followlinks=False):
        for name in ['.'] + dirs + names:
            if name in dirs:
                continue  # visited once as the next parent's '.'
            item = (Path(parent) / name).lstat()
            logical += item.st_size
            allocated += item.st_blocks * 512
            entries += 1
    return dict(logical_bytes=logical, allocated_bytes=allocated, entries=entries)


def source_hashes(root):
    return {p.relative_to(root).as_posix(): sha(p) for p in sorted(root.rglob('*'))
            if p.is_file() and '.izu' not in p.relative_to(root).parts}


def generate(root, count, size):
    if count > 20000 or count * size > MAX_PAYLOAD:
        raise ValueError('fixture budget exceeded')
    for index in range(count):
        path = root / 'src' / f'group{index // 100:03d}' / f'file{index:05d}.txt'
        path.parent.mkdir(parents=True, exist_ok=True)
        line = f'// generated source {index:05d}: function value() {{ return {index}; }}\n'.encode()
        path.write_bytes((line * ((size + len(line) - 1) // len(line)))[:size])


class Runner:
    def __init__(self, args, out):
        self.args, self.out = args, out
        self.records, self.groups = [], set()
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.rss_samples = []
        self.sampler = threading.Thread(target=self.sample_memory, daemon=True)
        self.sampler.start()

    def sample_memory(self):
        while not self.stop.wait(0.05):
            with self.lock:
                groups = set(self.groups)
            if not groups:
                continue
            try:
                result = subprocess.run(['/bin/ps', '-axo', 'pid=,ppid=,pgid=,rss='],
                                        capture_output=True, timeout=2, check=True, text=True)
                processes = [tuple(map(int, line.split())) for line in result.stdout.splitlines()
                             if len(line.split()) == 4]
                owned = {pid for pid, _, pgid, _ in processes if pgid in groups}
                while True:
                    expanded = owned | {pid for pid, parent, _, _ in processes if parent in owned}
                    if expanded == owned:
                        break
                    owned = expanded
                total = sum(rss * 1024 for pid, _, _, rss in processes if pid in owned)
                self.rss_samples.append({'monotonic_seconds': time.monotonic(),
                                         'active_process_groups': len(groups), 'rss_bytes': total})
            except (OSError, subprocess.SubprocessError, ValueError) as error:
                self.rss_samples.append({'error': str(error)})

    def run(self, root, args, phase):
        with self.lock:
            index = len(self.records)
            row = dict(index=index, phase=phase, argv=[str(self.args.executable), '--json', *args],
                       cwd=str(root), timeout_seconds=self.args.timeout)
            self.records.append(row)
        stdout = self.out / f'command-{index:04d}.stdout'
        stderr = self.out / f'command-{index:04d}.stderr'
        env = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'TMPDIR': str(self.out / 'tmp'),
               'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null'}
        start = time.monotonic()
        row['started_monotonic_seconds'] = start
        with stdout.open('xb') as output, stderr.open('xb') as errors:
            child = subprocess.Popen(row['argv'], cwd=root, env=env, stdout=output, stderr=errors,
                                     start_new_session=True)
            with self.lock:
                self.groups.add(child.pid)
            try:
                child.wait(timeout=self.args.timeout)
                row['timed_out'] = False
            except subprocess.TimeoutExpired:
                row['timed_out'] = True
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            finally:
                with self.lock:
                    self.groups.discard(child.pid)
        ended = time.monotonic()
        row.update(ended_monotonic_seconds=ended, wall_seconds=ended - start, exit_code=child.returncode,
                   stdout=str(stdout), stderr=str(stderr), stdout_bytes=stdout.stat().st_size,
                   stderr_bytes=stderr.stat().st_size)
        row['success'] = child.returncode == 0 and not row['timed_out']
        if row['stdout_bytes'] <= 16 * 1024 * 1024:
            try:
                response = json.loads(stdout.read_bytes())
                row['outcome'] = response.get('outcome', {}).get('kind')
                row['success'] &= row['outcome'] == 'ok'
            except (ValueError, AttributeError):
                row['success'] = False
        else:
            row['success'] = False
            row['failure'] = 'response exceeded validation byte budget'
        write_json(self.out / f'command-{index:04d}.json', row)
        if not row['success']:
            raise RuntimeError(f'command {index} failed: {phase}; retained {stderr}')
        return row

    def repeated(self, root, args, phase):
        rows = [self.run(root, args, phase) for _ in range(self.args.samples)]
        values = sorted(row['wall_seconds'] for row in rows)
        return dict(raw_seconds=[row['wall_seconds'] for row in rows],
                    p50_seconds=values[math.ceil(len(values) * .50) - 1],
                    p95_seconds=values[math.ceil(len(values) * .95) - 1])

    def close(self):
        self.stop.set()
        self.sampler.join()


def native_tree(runner, root, count, size):
    root.mkdir()
    generate(root, count, size)
    expected = source_hashes(root)
    runner.run(root, ['init', '.'], 'init')
    result = dict(files=count, bytes_per_file=size, source_before_sha256=expected,
                  disk_before=disk(root))
    result['first_capture'] = runner.run(root, ['checkpoint'], 'first_capture')['wall_seconds']
    runner.run(root, ['commit', '-m', 'generated baseline', '--author-name', 'Scale Fixture',
                      '--author-email', 'scale@example.invalid'], 'authored_baseline')
    result['warm_noop_capture'] = runner.repeated(root, ['checkpoint'], 'warm_noop_capture')
    result['warm_diff'] = runner.repeated(root, ['diff'], 'warm_diff')
    changed = next(iter(expected))
    with (root / changed).open('ab') as stream:
        stream.write(b'\n// independently validated changed source\n')
    expected[changed] = sha(root / changed)
    result['changed_capture'] = runner.run(root, ['checkpoint'], 'changed_capture')['wall_seconds']
    result['changed_diff'] = runner.run(root, ['diff'], 'changed_diff')['wall_seconds']
    runner.run(root, ['verify'], 'verify')
    if source_hashes(root) != expected:
        raise RuntimeError('source byte readback mismatch')
    result.update(source_after_sha256=expected, disk_after=disk(root), validated=True)
    return result


def history(runner, root):
    root.mkdir()
    generate(root, 32, 4096)
    runner.run(root, ['init', '.'], 'history_init')
    for index in range(runner.args.history):
        (root / 'history.txt').write_text(f'// history revision {index}\n')
        runner.run(root, ['commit', '-m', f'generated {index}', '--author-name', 'Scale Fixture',
                          '--author-email', 'scale@example.invalid'], 'history_commit')
    log = runner.repeated(root, ['log', '--limit', str(runner.args.history + 1)], 'history_log')
    for row in runner.records[-runner.args.samples:]:
        entries = json.loads(Path(row['stdout']).read_bytes())['outcome']['result']['data']
        descriptions = [entry['revision']['description'] for entry in entries]
        if len(entries) != runner.args.history + 1 or descriptions[:-1] != [
                f'generated {i}' for i in reversed(range(runner.args.history))]:
            raise RuntimeError('authored history readback mismatch')
    expected = source_hashes(root)
    checkpoint = runner.repeated(root, ['checkpoint'], 'history_checkpoint')
    runner.run(root, ['verify'], 'history_verify')
    if source_hashes(root) != expected:
        raise RuntimeError('history source byte readback mismatch')
    return dict(authored_commits=runner.args.history, log=log, checkpoint=checkpoint,
                source_after_sha256=expected, history_readback_validated=True, disk=disk(root))


def agents(runner, root):
    root.mkdir()
    generate(root, 512, 4096)
    expected = source_hashes(root)
    runner.run(root, ['init', '.'], 'agents_init')
    runner.run(root, ['commit', '-m', 'baseline', '--author-name', 'Scale Fixture',
                      '--author-email', 'scale@example.invalid'], 'agents_baseline')
    started = runner.run(root, ['start', 'repeated', '--', '/bin/sh', '-c', 'printf first > agent.txt'], 'agent_start')
    repeated = runner.repeated(root, ['--change', 'repeated', 'run', '--', '/bin/sh', '-c',
                                     'printf next >> agent.txt'], 'agent_repeat')
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        futures = [pool.submit(runner.run, root, ['start', f'parallel-{index}', '--', '/bin/sh', '-c',
                    f'printf writer{index} > writer.txt; sleep 0.2'], 'agent_parallel') for index in range(4)]
        completed = [future.result() for future in futures]
    concurrent_wall = time.monotonic() - start
    validated_private = []
    for row, filename, contents in [(started, 'agent.txt', b'first' + b'next' * runner.args.samples)] + [
            (row, 'writer.txt', f'writer{index}'.encode()) for index, row in enumerate(completed)]:
        data = json.loads(Path(row['stdout']).read_bytes())['outcome']['result']['data']
        private = Path(data['workspace']['record']['root']).resolve(strict=True)
        if not private.is_relative_to(root.resolve() / '.izu' / 'workspaces'):
            raise RuntimeError('managed receipt escaped owned workspace')
        expected_private = dict(expected)
        expected_private[filename] = hashlib.sha256(contents).hexdigest()
        if source_hashes(private) != expected_private:
            raise RuntimeError('private managed source byte readback mismatch')
        validated_private.append(dict(workspace=str(private), hashes=expected_private))
    if source_hashes(root) != expected:
        raise RuntimeError('private agents modified primary source')
    return dict(repeated=repeated, four_writer_wall_seconds=concurrent_wall, private_source_readbacks=validated_private,
                primary_source_validated=True, disk=disk(root),
                limit='Managed launches only; this does not establish cooperative checked integration')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--executable', type=Path, required=True)
    parser.add_argument('--expected-sha256', required=True)
    parser.add_argument('--candidate-identity', type=Path, required=True)
    parser.add_argument('--project-root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=5, choices=range(1, 11))
    parser.add_argument('--history', type=int, default=200, choices=range(1, 1001))
    parser.add_argument('--timeout', type=int, default=120, choices=range(1, 601))
    parser.add_argument('--cases', nargs='+', choices=['tree512', 'tree5000', 'tree20000', 'large', 'history', 'agents'],
                        default=['tree5000', 'tree20000', 'large', 'history', 'agents'])
    args = parser.parse_args()
    args.executable = args.executable.resolve(strict=True)
    if sha(args.executable) != args.expected_sha256:
        raise ValueError('executable hash mismatch before fixtures')
    identity = json.loads(args.candidate_identity.read_text())
    declared_sha = identity.get('executable_sha256', identity.get('binary_sha256', {}).get('izu'))
    if declared_sha != args.expected_sha256:
        raise ValueError('candidate identity hash mismatch')
    root = args.project_root.resolve(strict=True)
    out = args.output.resolve()
    if not out.is_relative_to(root / '.artifacts' / 'local'):
        raise ValueError('output must be inside project .artifacts/local')
    out.mkdir(parents=True, exist_ok=False)
    (out / 'tmp').mkdir()
    shutil.copy2(__file__, out / 'driver.py')
    report = dict(schema_version=1, executable=str(args.executable), executable_sha256=args.expected_sha256,
                  candidate_identity=identity, script_sha256=sha(__file__), platform=platform.platform(),
                  arguments=vars(args).copy(), cases={}, limits=[
                      'OS caches uncontrolled; first capture is not cold disk.',
                      'RSS is sampled every approximately 50ms via ps across owned process groups and observed descendants; short peaks and reparented descendants may be missed.',
                      'Aggregate RSS excludes shared-memory deduplication and kernel memory; ps adds measurement overhead.',
                      'st_blocks*512 is allocated filesystem space, not exclusive APFS device usage or I/O bytes.',
                      'Wall timings include executable startup; file generation and hashing are outside target timings.',
                      'Synthetic generated source, not arbitrary application or production performance.'])
    report['arguments'] = {k: str(v) if isinstance(v, Path) else v for k, v in report['arguments'].items()}
    report['filesystem'] = subprocess.run(['/bin/df', str(out)], capture_output=True, text=True, check=True).stdout
    runner = Runner(args, out)
    try:
        for name in args.cases:
            fixture = out / f'fixture-{name}'
            try:
                if name.startswith('tree'):
                    result = native_tree(runner, fixture, int(name[4:]), 4096)
                elif name == 'large':
                    result = native_tree(runner, fixture, 1, 64 * 1024 * 1024)
                elif name == 'history':
                    result = history(runner, fixture)
                else:
                    result = agents(runner, fixture)
                result['status'] = 'passed'
                shutil.rmtree(fixture)
                result['cleanup'] = 'removed owned successful fixture after readbacks'
            except Exception as error:
                result = dict(status='failed', error=str(error), retained_fixture=str(fixture))
            report['cases'][name] = result
            write_json(out / f'case-{name}.json', result)
    finally:
        runner.close()
        report['commands'] = runner.records
        report['rss_samples'] = runner.rss_samples
        report['sampled_aggregate_peak_rss_bytes'] = max((r.get('rss_bytes', 0) for r in runner.rss_samples), default=None)
        report['sampled_concurrent_peak_rss_bytes'] = max((r.get('rss_bytes', 0) for r in runner.rss_samples
            if r.get('active_process_groups', 0) >= 2), default=None)
        report['executable_sha256_after'] = sha(args.executable)
        report['identity_unchanged'] = report['executable_sha256_after'] == args.expected_sha256
        write_json(out / 'report.json', report)
    return 0 if report['identity_unchanged'] and all(c['status'] == 'passed' for c in report['cases'].values()) else 2


if __name__ == '__main__':
    raise SystemExit(main())

#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Select explicit native build tools, then bind their evidence to qualified packages."""
import sys
sys.dont_write_bytecode = True
import argparse
import json
import os
from pathlib import Path
import platform
import re
import selectors
import shutil
import signal
import subprocess
import tempfile

from release_common import contained_new_output, sha, source_manifest, write_json

MAX_PROBE_BYTES = 256 * 1024
PROBE_SECONDS = 15


REAP_SECONDS = 2
_UNCERTAIN_PROBES = []


def probe_worker():
    """Keep the session leader unreaped while the owner settles the whole group."""
    status = Path(sys.argv[2])
    try:
        child = subprocess.Popen(sys.argv[3:], stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        counts = {1: 0, 2: 0}
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ, 1)
            selector.register(child.stderr, selectors.EVENT_READ, 2)
            while True:
                # Observe exit before the readiness snapshot. A child can write
                # and exit after an empty select; polling afterward would lose
                # that final output. Once exited, drain only already-ready data.
                code = child.poll()
                ready = selector.select(0 if code is not None else 0.01)
                for key, _ in ready:
                    chunk = os.read(key.fileobj.fileno(), 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    descriptor = key.data
                    remaining = MAX_PROBE_BYTES + 1 - counts[descriptor]
                    chunk = chunk[:remaining]
                    counts[descriptor] += len(chunk)
                    while chunk:
                        written = os.write(descriptor, chunk)
                        if written == 0:
                            raise OSError('probe output write made no progress')
                        chunk = chunk[written:]
                    if counts[descriptor] > MAX_PROBE_BYTES:
                        raise ValueError('output_limit')
                # The worker retains group leadership even if the tool exits
                # with a descendant holding its pipes. Drain ready bytes only.
                if code is not None and not ready:
                    break
        result = {'worker': os.getpid(), 'child': child.pid, 'exit_code': code}
    except BaseException as error:
        result = {'worker': os.getpid(), 'worker_error': str(error)}
    temporary = status.with_suffix('.pending')
    temporary.write_text(json.dumps(result))
    temporary.replace(status)
    # Completion is a file receipt, not leader exit. Even a successful tool can
    # leave descendants holding stdout/stderr. Only the owner stops this group.
    # If the owner disappears, EOF stops this worker's own session as well.
    os.read(0, 1)
    os.killpg(os.getpgrp(), signal.SIGKILL)


def stop_probe(process):
    # Never poll/reap before signalling: the owned, unreaped Popen reserves the
    # leader PID. No later signal uses a saved PID after wait has released it.
    result = {'group_stop_requested': False, 'leader_reaped': False}
    if process.returncode is not None:
        result['uncertainty'] = 'leader was reaped before owned group stop'
        return result
    try:
        os.killpg(process.pid, signal.SIGKILL)
        result['group_stop_requested'] = True
    except ProcessLookupError:
        result['group_absent_before_reap'] = True
    except BaseException as error:
        result['uncertainty'] = 'group stop failed: ' + type(error).__name__
    try:
        result['leader_exit_code'] = process.wait(timeout=REAP_SECONDS)
        result['leader_reaped'] = True
    except BaseException as error:
        result['uncertainty'] = 'leader reap uncertain: ' + type(error).__name__
    if result['leader_reaped']:
        try:
            process.stdin.close()
        except OSError as error:
            result['control_close_error'] = type(error).__name__
    else:
        # Preserve live ownership and scratch evidence; do not silently discard
        # the Popen or retry signalling a potentially reaped group identity.
        _UNCERTAIN_PROBES.append(process)
    return result


def probe(argv, cwd, environment=None):
    """Bound fixed native probes and settle their owned group on every path."""
    import time
    scratch = Path(tempfile.mkdtemp(prefix='identity-', dir=cwd))
    out, err, status = scratch / 'stdout', scratch / 'stderr', scratch / 'status.json'
    process = None
    cleanup = None
    try:
        with out.open('wb') as stdout, err.open('wb') as stderr:
            process = subprocess.Popen([sys.executable, '-B', str(Path(__file__).resolve()),
                                        '__probe-worker', str(status), *argv],
                                       cwd=cwd, stdin=subprocess.PIPE, stdout=stdout, stderr=stderr,
                                       start_new_session=True, env=environment)
            try:
                deadline = time.monotonic() + PROBE_SECONDS
                while not status.exists():
                    if out.stat().st_size > MAX_PROBE_BYTES or err.stat().st_size > MAX_PROBE_BYTES:
                        raise ValueError('native probe exceeded output limit: ' + argv[0])
                    if time.monotonic() >= deadline:
                        raise ValueError('native probe timed out: ' + argv[0])
                    time.sleep(0.01)
                completed = json.loads(status.read_text())
                if completed.get('worker_error') == 'output_limit':
                    raise ValueError('native probe exceeded output limit: ' + argv[0])
                if completed.get('worker') != process.pid or 'worker_error' in completed:
                    raise ValueError('native probe worker failed')
            finally:
                cleanup = stop_probe(process)
                if not cleanup['leader_reaped'] or 'uncertainty' in cleanup:
                    raise ValueError('native probe cleanup uncertain; evidence: ' + str(scratch))
        if out.stat().st_size > MAX_PROBE_BYTES or err.stat().st_size > MAX_PROBE_BYTES:
            raise ValueError('native probe exceeded output limit: ' + argv[0])
        result = {'argv': argv, 'exit_code': completed['exit_code'],
                  'stdout': out.read_text(errors='replace'), 'stderr': err.read_text(errors='replace'),
                  'cleanup': cleanup}
        if result['exit_code'] != 0:
            raise ValueError('native probe failed: ' + repr(result))
    except BaseException as error:
        # Failures remain inspectable, including when the OS cannot confirm the
        # bounded reap. Never remove files an uncertain process may still use.
        if process is not None and cleanup is None:
            cleanup = stop_probe(process)
        try:
            (scratch / 'failure.json').write_text(json.dumps({'argv': argv, 'error': str(error),
                                                            'cleanup': cleanup}, indent=2) + '\n')
        except OSError:
            pass
        if cleanup and (not cleanup['leader_reaped'] or 'uncertainty' in cleanup):
            raise ValueError('native probe cleanup uncertain; evidence: ' + str(scratch)) from error
        if isinstance(error, (ValueError, OSError)):
            raise ValueError(str(error) + '; probe evidence: ' + str(scratch)) from error
        raise
    shutil.rmtree(scratch)
    return result


def tool(path):
    selected = Path(path)
    if not selected.is_absolute() or not selected.is_file() or not os.access(selected, os.X_OK):
        raise ValueError('native tool must be an absolute executable file: ' + str(path))
    resolved = selected.resolve(strict=True)
    return {'selected_path': str(selected), 'resolved_path': str(resolved), 'sha256': sha(resolved)}


def refuse_overrides(environment):
    # A name-only refusal avoids disclosing credential-bearing values or config content.
    exact = {'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC', 'RUSTDOC', 'RUSTC_WRAPPER',
             'RUSTC_WORKSPACE_WRAPPER', 'RUSTUP_TOOLCHAIN', 'CARGO_BUILD_TARGET',
             'CARGO_BUILD_RUSTFLAGS', 'CARGO_BUILD_RUSTC', 'CC', 'CXX', 'CPP', 'AR', 'RANLIB',
             'CFLAGS', 'CXXFLAGS', 'CPPFLAGS', 'LDFLAGS', 'HOST_CC', 'HOST_CFLAGS',
             'TARGET_CC', 'TARGET_CFLAGS', 'CRATE_CC_NO_DEFAULTS', 'CC_SHELL_ESCAPED_FLAGS',
             'SDKROOT', 'MACOSX_DEPLOYMENT_TARGET', 'DEVELOPER_DIR', 'CCC_OVERRIDE_OPTIONS',
             'GCC_EXEC_PREFIX', 'COMPILER_PATH', 'LIBRARY_PATH', 'CPATH', 'C_INCLUDE_PATH', 'LD', 'TOOLCHAINS', 'LD_LIBRARY_PATH', 'DYLD_LIBRARY_PATH', 'DYLD_INSERT_LIBRARIES',
             'CPLUS_INCLUDE_PATH', 'RUSTC_BOOTSTRAP', 'RUST_TARGET_PATH'}
    prefixes = ('CC_', 'CXX_', 'AR_', 'CFLAGS_', 'CXXFLAGS_', 'CPPFLAGS_', 'LDFLAGS_')
    refused = sorted(name for name, value in environment.items() if value and
                     (name in exact or name.startswith(prefixes) or
                      re.fullmatch(r'(HOST|TARGET)_(CC|CXX|AR|RANLIB|CFLAGS|CXXFLAGS|CPPFLAGS|LDFLAGS)', name) or
                      name.startswith('CARGO_PROFILE_') or (name.startswith('CARGO_BUILD_') and name != 'CARGO_BUILD_JOBS') or
                      (name.startswith('CARGO_TARGET_') and name.endswith(('_LINKER', '_RUSTFLAGS', '_RUNNER')))))
    if refused:
        raise ValueError('unqualified ambient build overrides: ' + ', '.join(refused))


def refuse_configs(root, cargo_home):
    candidates = []
    for directory in [root, *root.parents]:
        candidates += [directory / '.cargo/config', directory / '.cargo/config.toml']
    candidates += [cargo_home / 'config', cargo_home / 'config.toml']
    candidates += [root / name for name in source_manifest(root)['files']
                   if name.endswith(('.cargo/config', '.cargo/config.toml'))]
    for path in candidates:
        if path.exists() or path.is_symlink():
            raise ValueError('unqualified Cargo configuration: ' + str(path))


def rust_host(verbose):
    hosts = re.findall(r'^host: ([A-Za-z0-9_.-]+)$', verbose, re.MULTILINE)
    if len(hosts) != 1:
        raise ValueError('missing or ambiguous rustc host')
    return hosts[0]


def rust_toolchain_tools(output):
    rustup = tool(shutil.which('rustup') or '')
    # Homebrew's rustup symlink resolves to rustup-init, whose argv[0] selects
    # installer mode. Hash the resolved file but invoke the selected tool name.
    rustc = tool(probe([rustup['selected_path'], 'which', 'rustc'], output)['stdout'].strip())
    cargo = tool(probe([rustup['selected_path'], 'which', 'cargo'], output)['stdout'].strip())
    return rustc, cargo


def prepare(args):
    root = args.source_root.resolve(strict=True)
    refuse_overrides(os.environ)
    cargo_home = Path(os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))).resolve()
    refuse_configs(root, cargo_home)
    output = contained_new_output(args.output, root)
    source = source_manifest(root)
    rustc, cargo = rust_toolchain_tools(output)
    compiler = probe([rustc['resolved_path'], '-vV'], output)
    cargo_version = probe([cargo['resolved_path'], '-V'], output)
    # Keep the raw successful probes if later identity parsing fails.
    write_json(output / 'compiler-probes.json', {'rustc': compiler, 'cargo': cargo_version})
    host = rust_host(compiler['stdout'])
    system = platform.system()
    sdk = None
    if system == 'Darwin':
        xcrun = tool('/usr/bin/xcrun')
        cc_path = probe([xcrun['resolved_path'], '--sdk', 'macosx', '--find', 'clang'], output)['stdout'].strip()
        ar_path = probe([xcrun['resolved_path'], '--sdk', 'macosx', '--find', 'ar'], output)['stdout'].strip()
        sdk = {'path': probe([xcrun['resolved_path'], '--sdk', 'macosx', '--show-sdk-path'], output)['stdout'].strip(),
               'version': probe([xcrun['resolved_path'], '--sdk', 'macosx', '--show-sdk-version'], output),
               'xcrun': xcrun}
        if not Path(sdk['path']).is_absolute() or not Path(sdk['path']).is_dir():
            raise ValueError('selected macOS SDK missing')
        settings = Path(sdk['path']) / 'SDKSettings.json'
        sdk['settings_sha256'] = sha(settings)
        os_detail = probe(['/usr/bin/sw_vers'], output)
    elif system == 'Linux':
        cc_path = '/usr/bin/cc'
        ar_path = '/usr/bin/ar'
        os_detail = {'release': Path('/etc/os-release').read_text()[:16384],
                     'libc': probe(['/usr/bin/getconf', 'GNU_LIBC_VERSION'], output)}
    else:
        raise ValueError('native build identity supports macOS and Linux only')
    cc = tool(cc_path)
    archiver = tool(ar_path)
    cc_version = probe([cc['resolved_path'], '--version'], output)
    compiler_environment = dict(os.environ)
    if sdk:
        compiler_environment['SDKROOT'] = sdk['path']
    driver_plan = probe([cc['resolved_path'], '-###', '-x', 'c', '/dev/null',
                         '-o', str(output / 'probe-not-executed')], output, compiler_environment)
    backend = probe([cc['resolved_path'], '-print-prog-name=ld'], output)['stdout'].strip()
    backend = tool(backend if Path(backend).is_absolute() else (shutil.which(backend) or ''))
    # Apple's ld uses -v and GNU ld uses --version; each is a checked native probe.
    backend_version = probe([backend['resolved_path'], '-v' if system == 'Darwin' else '--version'], output)
    selected = {'CC': cc['resolved_path'], 'AR': archiver['resolved_path'], 'RUSTC': rustc['resolved_path'], 'CARGO_TARGET_' + host.upper().replace('-', '_') + '_LINKER': cc['resolved_path']}
    if sdk:
        selected['SDKROOT'] = sdk['path']
    if any('\n' in value or '\r' in value for value in selected.values()):
        raise ValueError('tool selection contains a newline')
    (output / 'build.env').write_text(''.join(name + '=' + value + '\n' for name, value in selected.items()))
    write_json(output / 'selection.json', {
        'schema_version': 1, 'source_root': str(root), 'source_manifest_sha256': source['source_manifest_sha256'],
        'source_file_count': len(source['files']), 'selected_environment': selected,
        'rustc': rustc, 'rustc_version': compiler, 'cargo': cargo, 'cargo_version': cargo_version,
        'c_compiler': cc, 'c_compiler_version': cc_version, 'rust_linker_driver': cc,
        'c_driver_plan': driver_plan, 'archiver': archiver,
        'linker_backend': backend, 'linker_backend_version': backend_version, 'sdk': sdk,
        'host': host, 'os': {'system': system, 'machine': platform.machine(), 'kernel': platform.release(),
                            'detail': os_detail},
        'runner_image': {name: os.environ.get(name) for name in ['ImageOS', 'ImageVersion', 'RUNNER_OS', 'RUNNER_ARCH']},
        'effective_selection': args.phase != 'observe',
        'scope': ('Explicit selection before build; apply build.env before qualification.' if args.phase != 'observe'
                  else 'Post-build tool observation only; effective compiler/linker selection for these existing packages is unproven.')})


def finish(args):
    root = args.source_root.resolve(strict=True)
    output = args.output.resolve(strict=True)
    if not output.is_relative_to(root / '.artifacts/local'):
        raise ValueError('identity output escaped source local artifacts')
    selection = json.loads((output / 'selection.json').read_text())
    if selection['source_root'] != str(root) or source_manifest(root)['source_manifest_sha256'] != selection['source_manifest_sha256']:
        raise ValueError('source differs from pre-build identity')
    expected = selection['selected_environment']
    for name, value in expected.items():
        if args.phase != 'observe' and os.environ.get(name) != value:
            raise ValueError('selected build environment changed: ' + name)
    refuse_overrides({name: value for name, value in os.environ.items() if name not in expected})
    refuse_configs(root, Path(os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))).resolve())
    for name in ['rustc', 'cargo', 'c_compiler', 'rust_linker_driver', 'linker_backend', 'archiver']:
        if tool(selection[name]['selected_path']) != selection[name]:
            raise ValueError('selected tool changed: ' + name)
    if selection['sdk'] and sha(Path(selection['sdk']['path']) / 'SDKSettings.json') != selection['sdk']['settings_sha256']:
        raise ValueError('selected SDK settings changed')
    qualification = args.qualification.resolve(strict=True)
    if not qualification.is_relative_to(root / '.artifacts/local'):
        raise ValueError('qualification escaped source local artifacts')
    q = json.loads(qualification.read_text())
    if not q['reproducible'] or q['source_manifest_sha256'] != selection['source_manifest_sha256']:
        raise ValueError('qualification source or reproducibility mismatch')
    packages = []
    for number in [1, 2]:
        base = qualification.parent
        build = base / ('build-' + str(number))
        result = json.loads((build / 'package-result.json').read_text())
        if result != q['package']:
            raise ValueError('qualified package identities differ')
        archive = build / result['archive']
        if sha(archive) != result['archive_sha256']:
            raise ValueError('qualified archive changed')
        extracted = base / ('extracted-' + str(number))
        package = extracted / 'package'
        manifest = json.loads((package / 'BUILD-MANIFEST.json').read_text())
        if manifest['rustc_verbose'] != selection['rustc_version']['stdout'] or manifest['cargo_version'] != selection['cargo_version']['stdout'].strip():
            raise ValueError('package compiler differs from selected toolchain')
        if manifest['source_manifest_sha256'] != selection['source_manifest_sha256']:
            raise ValueError('package source differs from selection')
        observations = {}
        for name, digest in result['binary_sha256'].items():
            if name not in ['izu', 'izu-lab']:
                raise ValueError('unexpected package binary')
            binary = package / 'bin' / name
            if sha(binary) != digest:
                raise ValueError('qualified binary changed')
            if selection['os']['system'] == 'Darwin':
                observations[name] = {'load_commands': probe(['/usr/bin/otool', '-l', str(binary)], output),
                                      'dynamic_dependencies': probe(['/usr/bin/otool', '-L', str(binary)], output)}
            else:
                observations[name] = {'elf_headers': probe(['/usr/bin/readelf', '-h', '-l', '-d', '-V', str(binary)], output),
                                      'dynamic_dependencies': probe(['/usr/bin/ldd', str(binary)], output)}
                libc = re.findall(r'^\s*libc\.so\.6 => (/\S+) \(',
                                  observations[name]['dynamic_dependencies']['stdout'], re.MULTILINE)
                if len(libc) != 1 or not Path(libc[0]).is_file():
                    raise ValueError('missing or ambiguous native GNU libc dependency')
                observations[name]['libc'] = {'selected_path': libc[0],
                                               'resolved_path': str(Path(libc[0]).resolve()),
                                               'sha256': sha(Path(libc[0]).resolve())}
        verification = extracted / 'verification-result.json'
        v = json.loads(verification.read_text())
        if not all(v[k] for k in ['acceptance_passed', 'archive_unchanged', 'binaries_unchanged', 'data_unchanged']):
            raise ValueError('extracted acceptance incomplete')
        packages.append({'archive': str(archive), 'archive_sha256': sha(archive),
                         'binary_sha256': result['binary_sha256'], 'verification_sha256': sha(verification),
                         'build_manifest_sha256': sha(package / 'BUILD-MANIFEST.json'), 'native_binary': observations})
    write_json(output / 'native-build-identity.json', {
        'schema_version': 1, 'selection': selection, 'selection_sha256': sha(output / 'selection.json'),
        'identity_helper_sha256': sha(Path(__file__)),
        'qualification_sha256': sha(qualification), 'packages': packages,
        'scope': 'Executed native host only. Binary load commands record minimum OS/SDK or ELF symbol requirements; no older-system support claim.'})


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('phase', choices=['prepare', 'finish', 'observe'])
    p.add_argument('--source-root', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--qualification', type=Path)
    args = p.parse_args()
    if args.phase in ['finish', 'observe'] and not args.qualification:
        p.error('finish requires --qualification')
    if args.phase in ['prepare', 'observe']:
        prepare(args)
    if args.phase in ['finish', 'observe']:
        finish(args)


if __name__ == '__main__':
    try:
        if len(sys.argv) > 1 and sys.argv[1] == '__probe-worker':
            probe_worker()
        else:
            main()
    except (ValueError, OSError, KeyError) as error:
        raise SystemExit(str(error))

#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Provenance refusal and bounded native-probe behavior, without package rebuilds."""
import sys
sys.dont_write_bytecode = True
import importlib.util
import json
import os
import time
from unittest import mock
from pathlib import Path
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
spec = importlib.util.spec_from_file_location('identity', Path(__file__).resolve().parents[1] / 'native-build-identity.py')
identity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(identity)


class IdentityTests(unittest.TestCase):
    def test_multicall_rustup_keeps_its_selected_invocation_name(self):
        local = Path(__file__).resolve().parents[2] / '.artifacts/local'
        local.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=local) as scratch:
            root = Path(scratch)
            installer = root / 'rustup-init'
            installer.write_text('#!' + sys.executable + '\n' + '''import sys
from pathlib import Path
if Path(sys.argv[0]).name != 'rustup':
    sys.exit('setup mode: unexpected argument which')
if sys.argv[1:] not in [['which', 'rustc'], ['which', 'cargo']]:
    sys.exit('unexpected tool selection')
print(Path(sys.argv[0]).parent / sys.argv[2])
''')
            installer.chmod(0o700)
            (root / 'rustup').symlink_to(installer.name)
            for name in ('rustc', 'cargo'):
                path = root / name
                path.write_text('selected tool fixture\n')
                path.chmod(0o700)
            with mock.patch.dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH']):
                rustc, cargo = identity.rust_toolchain_tools(root)
            self.assertEqual(rustc['selected_path'], str(root / 'rustc'))
            self.assertEqual(cargo['selected_path'], str(root / 'cargo'))
            self.assertEqual(sorted(path.name for path in root.iterdir()),
                             ['cargo', 'rustc', 'rustup', 'rustup-init'])

    def test_unqualified_flags_tools_and_target_overrides_refused_without_values(self):
        for name in ['RUSTFLAGS', 'CC', 'CC_aarch64_apple_darwin', 'SDKROOT',
                     'CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER', 'RUSTC_WRAPPER',
                     'CARGO_BUILD_RUSTFLAGS', 'CARGO_PROFILE_RELEASE_LTO']:
            with self.subTest(name=name), self.assertRaises(ValueError) as caught:
                identity.refuse_overrides({name: 'PRIVATE_VALUE'})
            self.assertIn(name, str(caught.exception))
            self.assertNotIn('PRIVATE_VALUE', str(caught.exception))
        identity.refuse_overrides({'CARGO_BUILD_JOBS': '2', 'CARGO_HOME': '/owned',
                                   'RUSTFLAGS': '', 'PATH': '/usr/bin'})

    def test_source_and_ancestor_cargo_configuration_refused(self):
        local = Path(__file__).resolve().parents[2] / '.artifacts/local'
        local.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=local) as scratch:
            base = Path(scratch)
            root = base / 'repo'
            root.mkdir()
            cargo = base / 'cargo'
            cargo.mkdir()
            (base / '.cargo').mkdir()
            config = base / '.cargo/config.toml'
            config.write_text('[build]\nrustflags=["PRIVATE_VALUE"]\n')
            with self.assertRaisesRegex(ValueError, 'Cargo configuration'):
                identity.refuse_configs(root, cargo)
            config.unlink()
            (root / 'nested/.cargo').mkdir(parents=True)
            (root / 'nested/.cargo/config').write_text('')
            with self.assertRaisesRegex(ValueError, 'Cargo configuration'):
                identity.refuse_configs(root, cargo)

    def test_tool_path_is_absolute_and_hash_bound(self):
        with self.assertRaisesRegex(ValueError, 'absolute executable'):
            identity.tool('cc')
        selected = identity.tool(sys.executable)
        self.assertEqual(selected['sha256'], identity.sha(Path(sys.executable).resolve()))

    def test_rust_host_requires_one_exact_host(self):
        self.assertEqual(identity.rust_host('rustc test\nhost: aarch64-apple-darwin\n'), 'aarch64-apple-darwin')
        for text in ['no host', 'host: bad/path\n', 'host: x\nhost: y\n']:
            with self.assertRaises(ValueError):
                identity.rust_host(text)

    def test_probe_rejects_nonzero_and_bounded_output(self):
        local = Path(__file__).resolve().parents[2] / '.artifacts/local'
        local.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=local) as scratch:
            root = Path(scratch)
            with self.assertRaisesRegex(ValueError, 'probe failed'):
                identity.probe([sys.executable, '-c', 'raise SystemExit(3)'], root)
            with self.assertRaisesRegex(ValueError, 'output limit'):
                identity.probe([sys.executable, '-c', 'import os; os.write(1,b"x"*300000)'], root)
            result = identity.probe([sys.executable, '-c', 'print("native")'], root)
            self.assertEqual(result['stdout'], 'native\n')
            original = identity.PROBE_SECONDS
            identity.PROBE_SECONDS = 0.05
            try:
                with self.assertRaisesRegex(ValueError, 'timed out'):
                    identity.probe([sys.executable, '-c', 'import time; time.sleep(5)'], root)
            finally:
                identity.PROBE_SECONDS = original
            failures = list(root.glob('identity-*/failure.json'))
            self.assertEqual(len(failures), 3)
            for failure in failures:
                self.assertTrue(json.loads(failure.read_text())['cleanup']['leader_reaped'])

    def test_real_probe_descendants_stop_on_success_timeout_nonzero_and_read_failure(self):
        local = Path(__file__).resolve().parents[2] / '.artifacts/local'
        local.mkdir(parents=True, exist_ok=True)
        # These model native tool exits and inherited pipes. A second Python
        # startup is unrelated to the probe's ownership and error contracts.
        fixture = r'''/bin/sleep 30 &
descendant=$!
printf '[%s,%s]' "$$" "$descendant" > "$1"
case "$2" in
    timeout) exec /bin/sleep 30 ;;
    output) /usr/bin/head -c 300000 /dev/zero; exec /bin/sleep 30 ;;
    nonzero) exit 3 ;;
esac
'''
        with tempfile.TemporaryDirectory(dir=local) as scratch:
            root = Path(scratch)
            for mode in ['success', 'timeout', 'nonzero', 'output', 'read_failure']:
                marker = root / (mode + '.json')
                command = ['/bin/sh', '-c', fixture, 'owned-native-fixture', str(marker), mode]
                original_read = Path.read_text
                def read(path, *args, **kwargs):
                    if mode == 'read_failure' and path.name == 'status.json':
                        raise OSError('intentional post-spawn read failure')
                    return original_read(path, *args, **kwargs)
                original_deadline = identity.PROBE_SECONDS
                identity.PROBE_SECONDS = 1
                started = time.monotonic()
                try:
                    with self.subTest(mode=mode), mock.patch.object(Path, 'read_text', read):
                        if mode == 'success':
                            identity.probe(command, root)
                        else:
                            expected = {
                                'timeout': 'native probe timed out',
                                'nonzero': 'native probe failed',
                                'output': 'output limit',
                                'read_failure': 'intentional post-spawn read failure',
                            }[mode]
                            with self.assertRaisesRegex(ValueError, expected):
                                identity.probe(command, root)
                finally:
                    identity.PROBE_SECONDS = original_deadline
                self.assertLess(time.monotonic() - started, 4)
                pids = json.loads(marker.read_text())
                deadline = time.monotonic() + 2
                remaining = list(pids)
                while remaining and time.monotonic() < deadline:
                    for pid in remaining[:]:
                        try:
                            os.kill(pid, 0)
                        except ProcessLookupError:
                            remaining.remove(pid)
                    time.sleep(0.01)
                self.assertEqual(remaining, [], 'owned fixture PIDs remain: ' + repr(remaining))


if __name__ == '__main__':
    unittest.main()

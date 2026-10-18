"""Check output placement and input rejection without compiling a second toolchain."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'cargo-check.py'


class CargoCheck(unittest.TestCase):
    def test_target_is_private_external_and_preserves_test_arguments(self):
        with tempfile.TemporaryDirectory(prefix='izu-cargo-output-') as temporary:
            root = Path(temporary).resolve(); source = root / 'source'; source.mkdir()
            fake = root / 'cargo-wrapper'
            fake.write_text('#!' + sys.executable + '\n' + r'''import json,sys
from pathlib import Path
args=sys.argv[1:]
assert args[:2]==['cargo','test']
target=Path(args[args.index('--target-dir')+1]); target.mkdir()
(target/'retained-build-output').write_text('preserve')
print(json.dumps(args))
''')
            fake.chmod(0o700)
            state = root / 'state'
            args = [sys.executable, str(SCRIPT), '--state-root', str(state), '--cargo-wrapper', str(fake),
                    '--', 'test', '--offline', '--', '--nocapture']
            result = subprocess.run(args, cwd=source, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            observed = json.loads(result.stdout)
            self.assertEqual(observed[-3:], ['--offline', '--', '--nocapture'])
            target = Path(observed[3])
            self.assertTrue(target.is_relative_to(state))
            self.assertFalse(target.is_relative_to(source))
            self.assertEqual((target/'retained-build-output').read_text(), 'preserve')
            self.assertEqual(list(source.iterdir()), [])
            self.assertEqual(state.stat().st_mode & 0o777, 0o700)
            for invalid in ([sys.executable, str(SCRIPT), '--state-root', str(source/'bad')],
                            [*args[:args.index('--')], '--', 'test', '--target-dir', str(source)]):
                rejected = subprocess.run(invalid, cwd=source, capture_output=True)
                self.assertEqual(rejected.returncode, 2)
            self.assertEqual(list(source.iterdir()), [])


if __name__ == '__main__':
    unittest.main()

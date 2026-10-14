"""Exercise target selection without credentials or network access."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RemoteAcceptanceTargetTests(unittest.TestCase):
    def setUp(self):
        storage = ROOT / '.artifacts/local/remote-target-tests'
        storage.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=storage)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tools = self.root / 'bin'
        self.tools.mkdir()
        self.evidence = self.root / 'evidence'
        self.calls = self.root / 'calls.jsonl'
        self.env = dict(os.environ, PATH=str(self.tools) + os.pathsep + os.environ['PATH'],
                        TEST_CALLS=str(self.calls), PYTHONDONTWRITEBYTECODE='1')
        self.write_tool('gh', '''import json,os,sys
args=sys.argv[1:]
with open(os.environ['TEST_CALLS'],'a') as log:
    log.write(json.dumps(['gh',*args])+'\\n')
if args[:2]==['auth','token']:
    print('synthetic-credential')
elif args==['api','user','--jq','.login']:
    print('release-tester')
elif args==['api','repos/release-tester/izu-acceptance-check']:
    print(json.dumps(dict(full_name='release-tester/izu-acceptance-check',
                          private=os.environ.get('TEST_PUBLIC')!='1')))
else:
    sys.exit('Unexpected network request')
''')
        self.binary = self.write_tool('izu', '''import json,os,sys
from pathlib import Path
with open(os.environ['TEST_CALLS'],'a') as log:
    log.write(json.dumps(['izu',*sys.argv[1:]])+'\\n')
if sys.argv[1:3]==['--json','init']:
    Path(sys.argv[3]).mkdir()
print('{}')
''')

    def write_tool(self, name, source):
        path = self.tools / name
        path.write_text('#!' + sys.executable + '\n' + source)
        path.chmod(0o700)
        return path

    def run_harness(self, *args):
        return subprocess.run([sys.executable, '-B', ROOT / 'scripts/remote-acceptance.py',
                               '--binary', self.binary, '--evidence', self.evidence,
                               '--phase', 'exercise', *args], cwd=ROOT, env=self.env,
                              capture_output=True, text=True, timeout=15)

    def test_missing_target_stops_before_authentication_or_evidence(self):
        result = self.run_harness()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn('--target', result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertFalse(self.evidence.exists())

    def test_non_disposable_repository_stops_before_authentication(self):
        result = self.run_harness('--target', 'release-tester/Izu')
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn('disposable', result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertFalse(self.evidence.exists())

    def test_escaping_target_stops_before_authentication(self):
        result = self.run_harness('--target', 'release-tester/izu-acceptance-check/../Izu')
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn('disposable', result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertFalse(self.evidence.exists())

    def test_selected_private_target_reaches_engine_and_receipt(self):
        result = self.run_harness('--target', 'release-tester/izu-acceptance-check')
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = json.loads((self.evidence / 'exercise-summary.json').read_text())
        self.assertEqual(summary['target'], 'release-tester/izu-acceptance-check')
        self.assertEqual(summary['url'], 'https://github.com/release-tester/izu-acceptance-check.git')
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        engine_calls = [args for args in calls if args[0] == 'izu']
        self.assertEqual(len(engine_calls), 5)
        self.assertIn(summary['url'], engine_calls[1])
        self.assertFalse((self.evidence / 'askpass.py').exists())
        self.assertNotIn('synthetic-credential', ''.join(
            path.read_text() for path in self.evidence.glob('*.json')))

    def test_public_target_metadata_refuses_engine_execution(self):
        self.env['TEST_PUBLIC'] = '1'
        result = self.run_harness('--target', 'release-tester/izu-acceptance-check')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Exact private target identity mismatch', result.stderr)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertFalse(any(args[0] == 'izu' for args in calls))
        self.assertFalse((self.evidence / 'askpass.py').exists())


if __name__ == '__main__':
    unittest.main()

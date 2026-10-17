"""Actual native backup/restore, unfinished source, corruption and no-overwrite tests."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import signal
import time
import tempfile
import unittest

SCRIPT = Path(os.environ.get('IZU_BACKUP_SCRIPT', Path(__file__).resolve().parents[1] / 'backup.py'))
BINARY = os.environ.get('IZU_TEST_BINARY')


@unittest.skipUnless(BINARY, 'set IZU_TEST_BINARY to the exact built executable')
class Backup(unittest.TestCase):
    def test_unfinished_job_is_refused_without_claiming_backup_success(self):
        with tempfile.TemporaryDirectory(prefix='izu-busy-backup-') as temporary:
            root = Path(temporary).resolve(); repo = root / 'repo'
            subprocess.run([BINARY, 'init', str(repo)], capture_output=True, check=True)
            with (root / 'job.stdout').open('wb') as out, (root / 'job.stderr').open('wb') as err:
                job = subprocess.Popen([BINARY, '--repo', str(repo), '--json', 'start', 'busy', '--',
                    sys.executable, '-c', "from pathlib import Path; import time; Path('started').write_text('preserve'); time.sleep(60)"],
                    stdout=out, stderr=err)
                try:
                    deadline = time.monotonic() + 10
                    while not list((repo / '.izu/workspaces').glob('*/started')):
                        self.assertIsNone(job.poll())
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(.02)
                    output = root / 'backup'
                    p = subprocess.run([sys.executable, str(SCRIPT), 'create', '--izu', BINARY,
                        '--repo', str(repo), '--output', str(output)], capture_output=True, text=True)
                    self.assertEqual(p.returncode, 1)
                    self.assertIn('unfinished', p.stderr)
                    self.assertFalse((output / 'backup.json').exists())
                    self.assertFalse((output / 'history.izu-bundle').exists())
                finally:
                    if job.poll() is None: job.send_signal(signal.SIGINT)
                    job.wait(timeout=10)
            self.assertEqual(next((repo / '.izu/workspaces').glob('*/started')).read_text(), 'preserve')

    def test_all_workspace_sources_restore_and_corruption_cannot_overwrite(self):
        with tempfile.TemporaryDirectory(prefix='izu-backup-') as temporary:
            root = Path(temporary).resolve()
            repo = root / 'repository'
            def native(*args):
                p = subprocess.run([BINARY, '--repo', str(repo), '--json', *map(str, args)],
                                   capture_output=True, text=True, check=True)
                return json.loads(p.stdout)['outcome']['result']['data']
            def backup(*args, expected=0):
                binary_selection = [] if os.environ.get('IZU_BACKUP_SCRIPT') else ['--izu', BINARY]
                p = subprocess.run([sys.executable, str(SCRIPT), *map(str, args), *binary_selection],
                                   capture_output=True, text=True)
                self.assertEqual(p.returncode, expected, p.stderr)
                return p
            native('init', repo)
            (repo / 'human.txt').write_text('unfinished human edits\n')
            native('checkpoint')
            agent = native('start', 'agent', '--', sys.executable, '-c',
                           "from pathlib import Path; Path('agent.txt').write_text('unfinished agent edits\\n')")
            output = root / 'backup'
            result = backup('create', '--repo', repo, '--output', output)
            receipt = json.loads(result.stdout)
            self.assertEqual(receipt['workspace_count'], 2)
            self.assertFalse(receipt['off_device_copy_verified'])
            saved = (output / 'history.izu-bundle').read_bytes()
            destinations = receipt['repositories']
            self.assertEqual((Path(destinations[agent['workspace']['id']]) / 'agent.txt').read_text(),
                             'unfinished agent edits\n')
            self.assertEqual((Path(receipt['selected_repository']) / 'human.txt').read_text(),
                             'unfinished human edits\n')
            # Restore from only the archive/receipt; no access to original source is required.
            portable = root / 'portable'; portable.mkdir(mode=0o700)
            for name in ('backup.json', 'history.izu-bundle'): shutil.copyfile(output / name, portable / name)
            moved = root / 'original-retained'; repo.rename(moved)
            restored = json.loads(backup('restore', '--backup', portable, '--destination', root / 'restored').stdout)
            self.assertEqual(restored['workspace_count'], 2)
            backup('restore', '--backup', portable, '--destination', root / 'restored', expected=2)
            self.assertEqual((moved / 'human.txt').read_text(), 'unfinished human edits\n')
            damaged = bytearray(saved); damaged[-1] ^= 1
            (portable / 'history.izu-bundle').write_bytes(damaged)
            backup('restore', '--backup', portable, '--destination', root / 'damaged', expected=1)
            self.assertFalse((root / 'damaged').exists())
            self.assertEqual((output / 'history.izu-bundle').read_bytes(), saved)
            # A backup inside captured source must be rejected before creating it.
            backup('create', '--repo', moved, '--output', moved / 'bad-backup', expected=2)
            self.assertFalse((moved / 'bad-backup').exists())


if __name__ == '__main__':
    unittest.main()

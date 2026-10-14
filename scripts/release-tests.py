#!/usr/bin/env python3
"""Adversarial package input tests; no Rust build or product claims."""
import sys
sys.dont_write_bytecode = True
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

from release_common import sha, source_manifest

spec = importlib.util.spec_from_file_location('release_verify', Path(__file__).with_name('release-verify.py'))
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)
ROOT = Path(__file__).resolve().parents[1] / '.artifacts/local'
ROOT.mkdir(parents=True, exist_ok=True)


class PackageInputs(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='release-tests-', dir=ROOT)
        self.root = Path(self.temporary.name)

    def tearDown(self):
        self.temporary.cleanup()

    def archive(self, entries):
        path = self.root / 'test.tar.gz'
        with tarfile.open(path, 'w:gz') as tar:
            for name, kind, content in entries:
                entry = tarfile.TarInfo(name)
                entry.mode = 0o644
                entry.type = kind
                entry.size = len(content) if kind == tarfile.REGTYPE else 0
                entry.linkname = '../sentinel'
                tar.addfile(entry, io.BytesIO(content) if entry.size else None)
        return path

    def assert_rejected(self, entries):
        archive = self.archive(entries)
        with self.assertRaises(ValueError):
            verify.extract(archive, self.root / 'extracted')
        self.assertFalse((self.root / 'extracted').exists())

    def test_traversal(self):
        self.assert_rejected([('../sentinel', tarfile.REGTYPE, b'bad')])

    def test_absolute(self):
        self.assert_rejected([('/sentinel', tarfile.REGTYPE, b'bad')])

    def test_symlink(self):
        self.assert_rejected([('link', tarfile.SYMTYPE, b'')])

    def test_hardlink(self):
        self.assert_rejected([('link', tarfile.LNKTYPE, b'')])

    def test_duplicate(self):
        self.assert_rejected([('a', tarfile.REGTYPE, b'first'), ('a', tarfile.REGTYPE, b'second')])

    def test_overlapping_paths(self):
        self.assert_rejected([('a/b', tarfile.REGTYPE, b'child'), ('a', tarfile.REGTYPE, b'parent')])

    def test_budget_before_extraction(self):
        archive = self.archive([('a', tarfile.REGTYPE, b'1234')])
        saved = verify.MAX_BYTES
        verify.MAX_BYTES = 3
        try:
            with self.assertRaises(ValueError):
                verify.extract(archive, self.root / 'extracted')
            self.assertFalse((self.root / 'extracted').exists())
        finally:
            verify.MAX_BYTES = saved

    def test_checksum_extra_and_changed_file(self):
        payload = self.root / 'payload'
        payload.mkdir()
        (payload / 'a').write_bytes(b'original')
        (payload / 'SHA256SUMS').write_text(sha(payload / 'a') + '  a\n')
        verify.verify_files(payload)
        (payload / 'extra').write_bytes(b'extra')
        with self.assertRaises(ValueError):
            verify.verify_files(payload)
        (payload / 'extra').unlink()
        (payload / 'a').write_bytes(b'modified')
        with self.assertRaises(ValueError):
            verify.verify_files(payload)

    def test_source_mutation_and_excluded_cache(self):
        source = self.root / 'source'
        source.mkdir()
        (source / 'a').write_bytes(b'original')
        first = source_manifest(source)
        (source / '.artifacts').mkdir()
        (source / '.artifacts/cache').write_bytes(b'ignored')
        self.assertEqual(first, source_manifest(source))
        (source / 'a').write_bytes(b'changed')
        self.assertNotEqual(first, source_manifest(source))


if __name__ == '__main__':
    unittest.main()

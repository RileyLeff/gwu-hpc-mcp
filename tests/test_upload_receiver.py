"""Offline regression tests: execute only the fixed receiver in private temp dirs."""
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest

RECEIVER = Path(__file__).resolve().parents[1] / 'src/upload_receiver.py'

class UploadTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='ghm-upload-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'group/user/project'
        self.root.mkdir(parents=True)
        (self.root / 'experiments').mkdir()

    def request(self, content='shutdown; $(reboot); `mkfs`; rm -rf /\n中文\x00'):
        return dict(root=str(self.root), directory='experiments/new-v1', files=[
            dict(name='auth.py', content=content, sha256=hashlib.sha256(content.encode()).hexdigest())])

    def run_receiver(self, request):
        return subprocess.run([sys.executable, '-I', str(RECEIVER)], input=json.dumps(request).encode(),
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)

    def test_verbatim_data_hash_and_no_execute(self):
        sentinel = self.root / 'must-not-exist'
        req = self.request("raise RuntimeError('setup/final shutdown')\n$(touch " + str(sentinel) + ")\n")
        result = self.run_receiver(req)
        self.assertEqual(result.returncode, 0, result.stderr)
        p = self.root / req['directory'] / 'auth.py'
        self.assertEqual(p.read_bytes(), req['files'][0]['content'].encode())
        self.assertEqual(stat.S_IMODE(p.stat().st_mode), 0o600)
        self.assertFalse(sentinel.exists())
        receipt = json.loads(result.stdout)
        self.assertEqual(receipt['status'], 'CREATED_NOT_EXECUTED')
        self.assertEqual(receipt['files']['auth.py']['sha256'], req['files'][0]['sha256'])

    def test_unicode_and_nul_are_data(self):
        req = self.request()
        result = self.run_receiver(req)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / req['directory'] / 'auth.py').read_bytes(), req['files'][0]['content'].encode())

    def test_existing_bundle_refused_and_preserved(self):
        req = self.request()
        self.assertEqual(self.run_receiver(req).returncode, 0)
        before = (self.root / req['directory'] / 'auth.py').read_bytes()
        self.assertNotEqual(self.run_receiver(self.request('different')).returncode, 0)
        self.assertEqual((self.root / req['directory'] / 'auth.py').read_bytes(), before)

    def test_bad_hash_creates_nothing(self):
        req = self.request(); req['files'][0]['sha256'] = '0' * 64
        self.assertNotEqual(self.run_receiver(req).returncode, 0)
        self.assertFalse((self.root / req['directory']).exists())

    def test_duplicate_names_create_nothing(self):
        req = self.request(); req['files'].append(req['files'][0])
        self.assertNotEqual(self.run_receiver(req).returncode, 0)
        self.assertFalse((self.root / req['directory']).exists())

    def test_reject_unsafe_paths(self):
        for name in ('../escape', '/tmp/escape', 'a/../escape', 'a//b', 'a/./b', 'x;reboot', 'x\ny', '$(reboot)'):
            with self.subTest(name=name):
                req = self.request(); req['directory'] = name
                self.assertNotEqual(self.run_receiver(req).returncode, 0)

    def test_reject_unsafe_filenames(self):
        for name in ('../escape', '/tmp/escape', 'x/y', '..', '.ssh', '$(reboot)'):
            with self.subTest(name=name):
                req = self.request(); req['files'][0]['name'] = name
                self.assertNotEqual(self.run_receiver(req).returncode, 0)
                self.assertFalse((self.root / req['directory']).exists())

    def test_symlink_parent_and_destination_refused(self):
        outside = Path(self.temp.name) / 'outside'; outside.mkdir()
        (self.root / 'link').symlink_to(outside, target_is_directory=True)
        req = self.request(); req['directory'] = 'link/new-v1'
        self.assertNotEqual(self.run_receiver(req).returncode, 0)
        self.assertFalse((outside / 'new-v1').exists())
        (self.root / 'experiments/new-v1').symlink_to(outside, target_is_directory=True)
        self.assertNotEqual(self.run_receiver(self.request()).returncode, 0)
        self.assertEqual(list(outside.iterdir()), [])

    def test_configured_root_alias_allowed(self):
        alias = Path(self.temp.name) / 'alias'; alias.symlink_to(self.root, target_is_directory=True)
        req = self.request(); req['root'] = str(alias)
        self.assertEqual(self.run_receiver(req).returncode, 0)

    def test_oversize_refused(self):
        req = self.request('a' * (8 * 1024 * 1024))
        self.assertNotEqual(self.run_receiver(req).returncode, 0)
        self.assertFalse((self.root / req['directory']).exists())

    def test_missing_parent_not_created(self):
        req = self.request(); req['directory'] = 'missing/new-v1'
        self.assertNotEqual(self.run_receiver(req).returncode, 0)
        self.assertFalse((self.root / 'missing').exists())

if __name__ == '__main__':
    unittest.main(verbosity=2)

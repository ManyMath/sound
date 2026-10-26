import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('cargokit_sync', Path(__file__).parents[1] / 'cargokit_sync.py')
sync = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sync)


class SyncTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / 'source'
        self.source.mkdir()
        self.git(self.source, 'init', '-q')
        (self.source / 'build.sh').write_text('safe build\n')
        (self.source / 'build.sh').chmod(0o755)
        (self.source / 'omitted.lock').write_text('fixture omitted\n')
        self.git(self.source, 'add', '.')
        self.git(self.source, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'Fixture')
        revision = self.git(self.source, 'rev-parse', 'HEAD').strip()
        self.patch = self.root / 'local.patch'
        self.patch.write_text('--- a/build.sh\n+++ b/build.sh\n@@ -1 +1 @@\n-safe build\n+patched safe build\n')
        self.recipe = {'schema': 1, 'repository': 'fixture-only', 'revision': revision,
                       'files': {'build.sh': {'mode': self.git(self.source, 'ls-tree', 'HEAD', 'build.sh').split()[0], 'sha256': sync.digest(b'safe build\n')}},
                       'omitted_files': {'omitted.lock': 'Fixture export policy'},
                       'patches': [{'file': 'local.patch', 'sha256': sync.digest(self.patch.read_bytes()), 'description': 'Fixture local change'}],
                       'expected_files_sha256': {'build.sh': sync.digest(b'patched safe build\n'), '.vendored-revision': sync.digest((revision+'\n').encode())}}
        self.manifest = self.root / 'manifest.json'
        self.output = self.root / 'output'
        self.output.mkdir()

    def git(self, repo, *args):
        return subprocess.check_output(['git', '-C', str(repo), *args], text=True)

    def build(self):
        self.manifest.write_text(json.dumps(self.recipe))
        return sync.materialize(self.manifest, self.source, self.output)

    def test_exact_revision_and_patch_produce_deterministic_tree(self):
        self.build()
        second = self.root / 'second'
        second.mkdir()
        sync.materialize(self.manifest, self.source, second)
        self.assertEqual((self.output / 'build.sh').read_text(), 'patched safe build\n')
        self.assertEqual((self.output / sync.PROVENANCE).read_bytes(), (second / sync.PROVENANCE).read_bytes())
        self.assertFalse((self.output / 'omitted.lock').exists())
        self.assertFalse((self.output / '.git').exists())

    def test_provenance_keeps_full_identity_and_visible_archive_subset(self):
        provenance = self.build()
        self.assertIn('.vendored-revision', provenance['installed_files_sha256'])
        self.assertNotIn('.vendored-revision', provenance['files_sha256'])
        self.assertEqual(provenance['publication_omitted_hidden_files'], ['.vendored-revision'])
        self.assertIn('build.sh', provenance['files_sha256'])

    def test_cli_rejects_symlink_installed_root(self):
        self.build()
        link = self.root / 'linked-installation'
        try:
            link.symlink_to(self.output, target_is_directory=True)
        except (OSError, NotImplementedError):
            self.skipTest('Host cannot create directory symlinks')
        result = subprocess.run([sys.executable, str(Path(sync.__file__)),
                                 '--manifest', str(self.manifest), '--source', str(self.source),
                                 '--check', str(link)], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Installed root must not be a symlink', result.stderr)

    def test_changed_patch_requires_new_reviewed_digest(self):
        self.patch.write_text(self.patch.read_text().replace('patched safe', 'unexpected'))
        with self.assertRaisesRegex(ValueError, 'Local patch identity'):
            self.build()

    def test_changed_source_digest_fails(self):
        self.recipe['files']['build.sh']['sha256'] = '0'*64
        with self.assertRaisesRegex(ValueError, 'Upstream source identity'):
            self.build()

    def test_final_inventory_catches_unreviewed_patch_result(self):
        self.recipe['expected_files_sha256']['build.sh'] = '0'*64
        with self.assertRaisesRegex(ValueError, 'reviewed final'):
            self.build()

    def test_omissions_must_account_for_every_upstream_path(self):
        self.recipe['omitted_files'] = {}
        with self.assertRaisesRegex(ValueError, 'inventories do not cover'):
            self.build()

    def test_installed_changes_fail_without_overwriting_them(self):
        self.build()
        installed = self.root / 'installed'
        shutil.copytree(self.output, installed)
        self.git(installed, 'init', '-q')
        sync.compare(self.output, installed)
        (installed / 'build.sh').write_text('uncommitted local change\n')
        with self.assertRaisesRegex(ValueError, 'drift'):
            sync.compare(self.output, installed)
        self.assertEqual((installed / 'build.sh').read_text(), 'uncommitted local change\n')

    def test_restrictive_umask_preserves_git_executable_identity(self):
        import os
        if os.name == 'nt':
            self.skipTest('POSIX executable bits do not apply on Windows')
        self.build()
        installed = self.root / 'installed'
        shutil.copytree(self.output, installed)
        self.git(installed, 'init', '-q')
        (installed / 'build.sh').chmod(0o700)
        sync.compare(self.output, installed)
        (installed / 'build.sh').chmod(0o600)
        with self.assertRaisesRegex(ValueError, 'executable mode drift'):
            sync.compare(self.output, installed)

    def test_extra_unignored_source_is_rejected(self):
        self.build()
        installed = self.root / 'installed'
        shutil.copytree(self.output, installed)
        self.git(installed, 'init', '-q')
        (installed / 'extra.rs').write_text('unreviewed')
        with self.assertRaisesRegex(ValueError, 'Unexpected managed files'):
            sync.compare(self.output, installed)

    def test_relative_paths_cannot_escape_or_change_git_metadata(self):
        for name in ['../outside', '/outside', 'C:/outside', '.git/config', 'dir/../outside', 'dir\\outside']:
            with self.subTest(name=name), self.assertRaises(ValueError):
                sync.relative(name)

    def test_source_revision_must_be_immutable(self):
        self.recipe['revision'] = 'main'
        with self.assertRaisesRegex(ValueError, 'immutable'):
            self.build()


if __name__ == '__main__':
    unittest.main()

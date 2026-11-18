"""Owned synthetic fixtures only; run with standard-library unittest discovery."""
from contextlib import closing
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

TOOL = Path(__file__).resolve().parents[2] / 'tools' / 'offline_sqlite_backup.py'
sys.dont_write_bytecode = True


class BackupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=Path.cwd())
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / 'synthetic.sqlite'
        with closing(sqlite3.connect(self.source)) as db:
            db.executescript('CREATE TABLE sample(id INTEGER PRIMARY KEY, value TEXT);'
                             'CREATE INDEX sample_value ON sample(value);'
                             'CREATE VIEW sample_view AS SELECT id FROM sample;')
            db.executemany('INSERT INTO sample VALUES (?, ?)', [(1, 'private-marker'), (2, None)])
            db.commit()
        self.destination = self.root / 'backup.sqlite'

    def cli(self, *args):
        return subprocess.run([sys.executable, str(TOOL), *map(str, args)],
                              capture_output=True, text=True, timeout=15)

    def snapshot(self, path):
        with closing(sqlite3.connect(path)) as db:
            return (db.execute('SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY name').fetchall(),
                    db.execute('SELECT * FROM sample ORDER BY id').fetchall())

    def digest(self, path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def test_backup_preserves_schema_rows_and_source_bytes(self):
        before = self.snapshot(self.source)
        original = self.digest(self.source)
        result = self.cli('backup', self.source, self.destination)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.snapshot(self.destination), before)
        self.assertEqual(self.digest(self.source), original)
        self.assertNotIn('private-marker', result.stdout + result.stderr)
        self.assertEqual(json.loads(result.stdout), {'status': 'ok', 'schema_objects': 3, 'tables': 1})


    def test_verify_and_restore_drill(self):
        expected = self.snapshot(self.source)
        self.assertEqual(self.cli('backup', self.source, self.destination).returncode, 0)
        verified = self.cli('verify', self.destination)
        self.assertEqual(verified.returncode, 0, verified.stderr)
        self.assertEqual(json.loads(verified.stdout)['tables'], 1)
        restored = self.root / 'restored.sqlite'
        result = self.cli('restore', self.destination, restored)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.snapshot(restored), expected)
        self.assertEqual(self.snapshot(self.destination), expected)


    def test_invalid_invocation_never_echoes_arguments(self):
        for args in [('private-marker',), ('backup',), ('--private-marker',)]:
            with self.subTest(args=args):
                result = self.cli(*args)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('private-marker', result.stdout + result.stderr)
                self.assertNotIn('Traceback', result.stderr)


    def test_missing_corrupt_and_empty_sources_fail_without_publication(self):
        for content in (None, b'private-marker-not-a-database', b''):
            bad = self.root / 'private-marker.sqlite'
            if content is not None:
                bad.write_bytes(content)
            for operation in ('backup', 'verify', 'restore'):
                with self.subTest(content=content, operation=operation):
                    args = [operation, bad]
                    if operation != 'verify':
                        args.append(self.destination)
                    result = self.cli(*args)
                    self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                    self.assertEqual(json.loads(result.stderr), {'status': 'failed'})
                    self.assertEqual(result.stdout, '')
                    self.assertFalse(self.destination.exists())
                    self.assertEqual(list(self.root.glob('.sqlite-backup-*')), [])
            if content is None:
                self.assertFalse(bad.exists())
            else:
                self.assertEqual(bad.read_bytes(), content)
                bad.unlink()


    def test_existing_destination_sidecars_are_preserved_and_refused(self):
        for suffix in ('-wal', '-shm', '-journal'):
            with self.subTest(suffix=suffix):
                sidecar = Path(str(self.destination) + suffix)
                sidecar.write_bytes(b'owned-sentinel')
                result = self.cli('backup', self.source, self.destination)
                self.assertEqual(result.returncode, 1)
                self.assertFalse(self.destination.exists())
                self.assertEqual(sidecar.read_bytes(), b'owned-sentinel')
                sidecar.unlink()


    def test_standalone_wal_with_journal_is_refused(self):
        with closing(sqlite3.connect(self.source)) as writer:
            writer.execute('PRAGMA journal_mode=WAL')
        journal = Path(str(self.source) + '-journal')
        journal.write_bytes(b'owned-possible-recovery-state')
        before = {p.name: p.read_bytes() for p in self.root.iterdir()}
        self.assertEqual(self.cli('verify', self.source).returncode, 1)
        self.assertEqual({p.name: p.read_bytes() for p in self.root.iterdir()}, before)

    def test_source_journal_is_refused_without_touching_it(self):
        journal = Path(str(self.source) + '-journal')
        journal.write_bytes(b'owned-not-a-real-journal')
        before = self.source.read_bytes()
        result = self.cli('backup', self.source, self.destination)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(journal.read_bytes(), b'owned-not-a-real-journal')
        self.assertEqual(self.source.read_bytes(), before)
        self.assertFalse(self.destination.exists())

    def test_incomplete_wal_pair_refused_without_changes(self):
        with closing(sqlite3.connect(self.source)) as writer:
            writer.execute('PRAGMA journal_mode=WAL')
        wal = Path(str(self.source) + '-wal')
        wal.write_bytes(b'owned-incomplete-wal')
        before = {p.name: p.read_bytes() for p in self.root.iterdir()}
        result = self.cli('backup', self.source, self.destination)
        self.assertEqual(result.returncode, 1)
        self.assertEqual({p.name: p.read_bytes() for p in self.root.iterdir()}, before)

    def test_wal_without_sidecars_does_not_create_source_files(self):
        with closing(sqlite3.connect(self.source)) as writer:
            writer.execute('PRAGMA journal_mode=WAL')
        before = {p.name: p.read_bytes() for p in self.root.iterdir()}
        result = self.cli('verify', self.source)
        # Refusal is safe; source-side WAL/SHM creation is not.
        self.assertIn(result.returncode, (0, 1))
        self.assertEqual({p.name: p.read_bytes() for p in self.root.iterdir()}, before)

    def test_wal_committed_rows_and_source_files_unchanged(self):
        with closing(sqlite3.connect(self.source)) as writer:
            self.assertEqual(writer.execute('PRAGMA journal_mode=WAL').fetchone(), ('wal',))
            writer.execute('PRAGMA wal_autocheckpoint=0')
            writer.execute('INSERT INTO sample VALUES (3, ?)', ('wal-only-marker',))
            writer.commit()
            expected = self.snapshot(self.source)
            files = [self.source, Path(str(self.source) + '-wal'),
                     Path(str(self.source) + '-shm')]
            before = {p: p.read_bytes() for p in files}
            result = self.cli('backup', self.source, self.destination)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(self.cli('verify', self.destination).returncode, 0)
            restored = self.root / 'wal-restored.sqlite'
            self.assertEqual(self.cli('restore', self.destination, restored).returncode, 0)
            self.assertEqual(self.snapshot(restored), expected)
            self.assertEqual(self.snapshot(self.destination), expected)
            self.assertEqual({p: p.read_bytes() for p in files}, before)

    def test_existing_same_path_hardlink_and_directory_denied(self):
        original = self.source.read_bytes()
        self.destination.write_bytes(b'owned-existing-output')
        directory = self.root / 'directory'
        directory.mkdir()
        hardlink = self.root / 'hardlink.sqlite'
        os.link(self.source, hardlink)
        for target in (self.destination, self.source, hardlink, directory,
                       self.source.parent / '.' / self.source.name):
            for operation in ('backup', 'restore'):
                with self.subTest(target=target.name, operation=operation):
                    result = self.cli(operation, self.source, target)
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(result.stderr.strip(), '{"status": "failed"}')
        self.assertEqual(self.source.read_bytes(), original)
        self.assertEqual(self.destination.read_bytes(), b'owned-existing-output')
        self.assertEqual(hardlink.read_bytes(), original)
        self.assertEqual(list(directory.iterdir()), [])

    def test_destination_symlink_and_dangling_symlink_denied(self):
        target = self.root / 'target'
        target.write_bytes(b'owned-sentinel')
        for link_target in (target, self.root / 'absent'):
            try:
                self.destination.symlink_to(link_target)
            except OSError:
                self.skipTest('OS does not permit symlink creation')
            try:
                self.assertEqual(self.cli('backup', self.source, self.destination).returncode, 1)
                self.assertTrue(self.destination.is_symlink())
                self.assertEqual(target.read_bytes(), b'owned-sentinel')
            finally:
                self.destination.unlink()

    def test_failure_cleans_only_owned_temporary_output(self):
        module = self.module()
        sentinel = self.root / '.sqlite-backup-existing.tmp'
        sentinel.write_bytes(b'owned-sentinel')
        for failure in ('check', 'publish'):
            with self.subTest(failure=failure):
                patcher = (mock.patch.object(module, 'check', side_effect=module.OperationError())
                           if failure == 'check' else
                           mock.patch.object(module.os, 'link', side_effect=OSError('private-marker')))
                with patcher, self.assertRaises((OSError, module.OperationError)):
                    module.backup(self.source, self.destination)
                self.assertFalse(self.destination.exists())
                self.assertEqual(list(self.root.glob('.sqlite-backup-*')), [sentinel])
                self.assertEqual(sentinel.read_bytes(), b'owned-sentinel')

    def test_publish_race_preserves_competing_output(self):
        module = self.module()
        real_link = os.link
        def competing_link(source, destination):
            Path(destination).write_bytes(b'owned-competitor')
            real_link(source, destination)
        with mock.patch.object(module.os, 'link', side_effect=competing_link):
            with self.assertRaises(FileExistsError):
                module.backup(self.source, self.destination)
        self.assertEqual(self.destination.read_bytes(), b'owned-competitor')
        self.assertEqual(list(self.root.glob('.sqlite-backup-*')), [])

    def module(self):
        spec = importlib.util.spec_from_file_location('offline_backup', TOOL)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def test_symlink_parent_detection_portable(self):
        module = self.module()
        real = Path.is_symlink
        with mock.patch.object(Path, 'is_symlink',
                               lambda path: path == self.root or real(path)):
            with self.assertRaises(module.OperationError):
                module.backup(self.source, self.destination)
        self.assertFalse(self.destination.exists())

    def test_junction_and_leaf_symlink_detection_portable(self):
        module = self.module()
        for attribute, rejected in (('is_symlink', self.destination),
                                    ('is_junction', self.root)):
            real = getattr(Path, attribute)
            with self.subTest(attribute=attribute):
                with mock.patch.object(Path, attribute,
                                       lambda path: path == rejected or real(path)):
                    with self.assertRaises(module.OperationError):
                        module.backup(self.source, self.destination)
                self.assertFalse(self.destination.exists())

    def test_corrupt_sqlite_header_body_is_not_published(self):
        damaged = self.root / 'damaged.sqlite'
        damaged.write_bytes(b'SQLite format 3\x00' + bytes(4096))
        original = damaged.read_bytes()
        for operation in ('backup', 'verify', 'restore'):
            args = [operation, damaged]
            if operation != 'verify':
                args.append(self.destination)
            result = self.cli(*args)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stderr.strip(), '{"status": "failed"}')
            self.assertEqual(damaged.read_bytes(), original)
            self.assertFalse(self.destination.exists())
            self.assertEqual(list(self.root.glob('.sqlite-backup-*')), [])

    def test_argument_shapes_fail_safely(self):
        for args in (('backup', self.source), ('restore', self.source),
                     ('verify', self.source, self.destination)):
            result = self.cli(*args)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout, '')
            self.assertEqual(result.stderr.strip(), '{"status": "failed"}')

    def test_symlink_parent_destination_is_denied(self):
        actual = self.root / 'actual'
        actual.mkdir()
        alias = self.root / 'alias'
        try:
            alias.symlink_to(actual, target_is_directory=True)
        except OSError:
            self.skipTest('OS does not permit symlink creation')
        result = self.cli('backup', self.source, alias / 'new.sqlite')
        self.assertEqual(result.returncode, 1)
        self.assertEqual(list(actual.iterdir()), [])


if __name__ == '__main__':
    unittest.main()

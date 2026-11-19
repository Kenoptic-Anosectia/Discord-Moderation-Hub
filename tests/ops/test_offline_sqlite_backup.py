"""Owned synthetic fixtures only; run with standard-library unittest discovery."""
import contextlib
from contextlib import closing
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import types
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


SCRATCH = Path(__file__).resolve().parents[2] / 'target' / 'sf1-backup-tool-tests'


class HardeningTests(unittest.TestCase):
    """WAL completeness (finding 1) and manifest verification (finding 2)."""

    def setUp(self):
        SCRATCH.mkdir(parents=True, exist_ok=True)
        self.tmp = tempfile.TemporaryDirectory(dir=SCRATCH)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / 'synthetic.sqlite'
        self.destination = self.root / 'backup.sqlite'
        self.manifest = Path(str(self.destination) + '.manifest.json')
        with closing(sqlite3.connect(self.source)) as db:
            db.executescript(
                'CREATE TABLE parent(id INTEGER PRIMARY KEY, name TEXT);'
                'CREATE TABLE child(id INTEGER PRIMARY KEY,'
                ' parent_id INTEGER REFERENCES parent(id), note TEXT);')
            db.executemany('INSERT INTO parent VALUES (?, ?)', [(1, 'a'), (2, 'b')])
            db.executemany('INSERT INTO child VALUES (?, ?, ?)',
                           [(1, 1, 'x'), (2, 2, 'y')])
            db.commit()

    def cli(self, *args):
        return subprocess.run([sys.executable, str(TOOL), *map(str, args)],
                              capture_output=True, text=True, timeout=15)

    def code(self, result):
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertEqual(result.stdout, '')
        body = json.loads(result.stderr)
        self.assertEqual(body['status'], 'failed')
        return body.get('code')

    def make_backup(self):
        result = self.cli('backup', self.source, self.destination)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.cli('verify', self.destination).returncode, 0)

    def rehash(self):
        """Model an attacker who also rewrites the manifest checksum."""
        data = json.loads(self.manifest.read_text())
        data['sha256'] = hashlib.sha256(self.destination.read_bytes()).hexdigest()
        self.manifest.write_text(json.dumps(data))

    def edit(self, sql):
        with closing(sqlite3.connect(self.destination)) as db:
            db.execute('PRAGMA foreign_keys=OFF')
            db.execute(sql)
            db.commit()

    def test_wal_only_rows_are_in_backup_and_manifest(self):
        with closing(sqlite3.connect(self.source)) as writer:
            self.assertEqual(writer.execute('PRAGMA journal_mode=WAL').fetchone(), ('wal',))
            writer.execute('PRAGMA wal_autocheckpoint=0')
            writer.execute('INSERT INTO parent VALUES (3, ?)', ('wal-only-a',))
            writer.execute('INSERT INTO child VALUES (3, 3, ?)', ('wal-only-b',))
            writer.commit()
            self.assertGreater(Path(str(self.source) + '-wal').stat().st_size, 0)
            self.make_backup()
        with closing(sqlite3.connect(self.destination)) as db:
            self.assertEqual(db.execute('SELECT count(*) FROM parent').fetchone(), (3,))
            self.assertEqual(db.execute('SELECT count(*) FROM child').fetchone(), (3,))
        self.assertEqual(json.loads(self.manifest.read_text())['tables'],
                         {'child': 3, 'parent': 3})

    def test_wal_without_shm_is_refused_not_read_immutable(self):
        wal = Path(str(self.source) + '-wal')
        shm = Path(str(self.source) + '-shm')
        kept = self.root / 'kept-wal.bin'
        with closing(sqlite3.connect(self.source)) as writer:
            writer.execute('PRAGMA journal_mode=WAL')
            writer.execute('PRAGMA wal_autocheckpoint=0')
            writer.execute('INSERT INTO parent VALUES (3, ?)', ('wal-only',))
            writer.commit()
            kept.write_bytes(wal.read_bytes())
        # Writer closed: sidecars are gone. Recreate a WAL holding committed
        # frames but no shm; it must be refused, never read as immutable.
        wal.write_bytes(kept.read_bytes())
        shm.unlink(missing_ok=True)
        result = self.cli('backup', self.source, self.destination)
        self.assertEqual(result.returncode, 1)
        self.assertFalse(self.destination.exists())

    def test_backup_writes_manifest(self):
        self.make_backup()
        data = json.loads(self.manifest.read_text())
        self.assertEqual(data['sha256'], hashlib.sha256(self.destination.read_bytes()).hexdigest())
        self.assertEqual(data['tables'], {'child': 2, 'parent': 2})
        self.assertRegex(data['schema_hash'], r'^[0-9a-f]{64}$')
        self.assertTrue(data['tool_version'])
        self.assertTrue(data['source_hint'])
        self.assertNotIn('synthetic', self.manifest.read_text())

    def test_existing_manifest_is_refused_and_preserved(self):
        self.manifest.write_bytes(b'owned-sentinel')
        self.assertEqual(self.cli('backup', self.source, self.destination).returncode, 1)
        self.assertFalse(self.destination.exists())
        self.assertEqual(self.manifest.read_bytes(), b'owned-sentinel')

    def test_flipped_byte_fails_checksum(self):
        self.make_backup()
        data = bytearray(self.destination.read_bytes())
        data[-1] ^= 0xFF
        self.destination.write_bytes(bytes(data))
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'checksum_mismatch')

    def test_deleted_row_fails_row_counts(self):
        self.make_backup()
        self.edit('DELETE FROM child WHERE id=2')
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'checksum_mismatch')
        self.rehash()
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'row_count_mismatch')

    def test_foreign_key_violation_fails(self):
        self.make_backup()
        self.edit('UPDATE child SET parent_id=99 WHERE id=2')
        self.rehash()
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'foreign_key_violation')

    def test_foreign_database_fails(self):
        self.make_backup()
        foreign = self.root / 'foreign.sqlite'
        with closing(sqlite3.connect(foreign)) as db:
            db.execute('CREATE TABLE other(id INTEGER PRIMARY KEY)')
            db.commit()
        self.destination.write_bytes(foreign.read_bytes())
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'checksum_mismatch')
        self.rehash()
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'schema_mismatch')

    def test_missing_and_malformed_manifest(self):
        self.make_backup()
        self.manifest.write_text('{"sha256": "private-marker"')
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'manifest_invalid')
        self.manifest.write_text('{"tables": 1}')
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'manifest_invalid')
        self.manifest.unlink()
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'manifest_missing')

    def test_failures_never_echo_private_content(self):
        self.make_backup()
        self.edit("UPDATE parent SET name='private-marker' WHERE id=1")
        result = self.cli('verify', self.destination)
        self.assertNotIn('private-marker', result.stdout + result.stderr)
        self.assertNotIn('Traceback', result.stderr)

    def plant_overlay(self, wal_header):
        """Plant a committed WAL beside the backup; main file bytes stay untouched."""
        self.make_backup()
        if wal_header:
            with closing(sqlite3.connect(self.destination)) as db:
                self.assertEqual(db.execute('PRAGMA journal_mode=WAL').fetchone(), ('wal',))
            self.rehash()
            self.assertEqual(self.cli('verify', self.destination).returncode, 0)
        before = self.destination.read_bytes()
        self.assertEqual(before[18:20], b'\x02\x02' if wal_header else b'\x01\x01')
        donor = self.root / 'donor.sqlite'
        donor.write_bytes(before)
        with closing(sqlite3.connect(donor)) as writer:
            writer.execute('PRAGMA journal_mode=WAL')
            writer.execute('PRAGMA wal_autocheckpoint=0')
            writer.execute("INSERT INTO parent VALUES (3, 'planted')")
            writer.commit()
            for suffix in ('-wal', '-shm'):
                Path(str(self.destination) + suffix).write_bytes(
                    Path(str(donor) + suffix).read_bytes())
        self.assertEqual(self.destination.read_bytes(), before)

    def test_planted_wal_overlay_fails_verify_header_2_2(self):
        self.plant_overlay(True)
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'sidecar_present')

    def test_planted_wal_overlay_fails_verify_header_1_1(self):
        self.plant_overlay(False)
        self.assertEqual(self.code(self.cli('verify', self.destination)), 'sidecar_present')

    def test_each_sidecar_alone_fails_verify(self):
        self.make_backup()
        for suffix in ('-wal', '-shm', '-journal'):
            with self.subTest(suffix=suffix):
                side = Path(str(self.destination) + suffix)
                side.write_bytes(b'')
                self.assertEqual(self.code(self.cli('verify', self.destination)), 'sidecar_present')
                side.unlink()

    def assert_restore_refused(self, code):
        restored = self.root / 'restored.sqlite'
        result = self.cli('restore', self.destination, restored)
        self.assertEqual(self.code(result), code)
        self.assertFalse(restored.exists())
        self.assertFalse(Path(str(restored) + '.manifest.json').exists())
        self.assertEqual([p.name for p in self.root.glob('.sqlite-backup-*')], [])

    def test_restore_refuses_tampered_source(self):
        self.make_backup()
        self.edit('DELETE FROM child WHERE id=2')
        self.assert_restore_refused('checksum_mismatch')

    def test_restore_refuses_missing_manifest(self):
        self.make_backup()
        self.manifest.unlink()
        self.assert_restore_refused('manifest_missing')

    def test_restore_refuses_sidecar_present(self):
        self.plant_overlay(False)
        self.assert_restore_refused('sidecar_present')

    def test_restore_refuses_foreign_key_violation(self):
        self.make_backup()
        self.edit('UPDATE child SET parent_id=99 WHERE id=2')
        self.rehash()
        self.assert_restore_refused('foreign_key_violation')

    def test_restore_of_good_backup_still_works_and_is_verifiable(self):
        self.make_backup()
        restored = self.root / 'restored.sqlite'
        self.assertEqual(self.cli('restore', self.destination, restored).returncode, 0)
        self.assertEqual(self.cli('verify', restored).returncode, 0)


FOLLOWUP_SCRATCH = Path(__file__).resolve().parents[2] / 'target' / 'sf6-backup-followup-tests'


class FollowupTests(unittest.TestCase):
    """SF6: restore copies verified bytes, publish by inode, `..`, old interpreters."""

    def setUp(self):
        FOLLOWUP_SCRATCH.mkdir(parents=True, exist_ok=True)
        self.tmp = tempfile.TemporaryDirectory(dir=FOLLOWUP_SCRATCH)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / 'synthetic.sqlite'
        self.backup_file = self.root / 'backup.sqlite'
        self.restored = self.root / 'restored.sqlite'
        self.make_db(self.source, 'genuine-row')
        self.module().backup(self.source, self.backup_file)

    def module(self):
        spec = importlib.util.spec_from_file_location('offline_backup', TOOL)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def make_db(self, path, value):
        with closing(sqlite3.connect(path)) as db:
            db.execute('CREATE TABLE sample(id INTEGER PRIMARY KEY, value TEXT)')
            db.execute('INSERT INTO sample VALUES (1, ?)', (value,))
            db.commit()

    def run_main(self, module, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = module.main([str(a) for a in args])
        return code, out.getvalue(), err.getvalue()

    def assert_nothing_published(self, target):
        self.assertFalse(target.exists())
        self.assertFalse(Path(str(target) + '.manifest.json').exists())
        self.assertEqual(list(self.root.glob('.sqlite-backup-*')), [])

    # Finding 1: restore must not certify bytes it did not verify.
    def test_restore_refuses_source_swapped_after_verify(self):
        module = self.module()
        real_verify = module.verify
        evil = self.root / 'evil.sqlite'
        self.make_db(evil, 'swapped-in-unverified-row')

        def verify_then_swap(path):
            result = real_verify(path)
            self.backup_file.write_bytes(evil.read_bytes())
            return result

        with mock.patch.object(module, 'verify', verify_then_swap):
            code, out, err = self.run_main(module, 'restore', self.backup_file, self.restored)
        self.assertEqual((code, out), (1, ''))
        self.assertEqual(json.loads(err), {'code': 'checksum_mismatch', 'status': 'failed'})
        self.assert_nothing_published(self.restored)

    def test_restore_output_is_byte_identical_to_verified_source(self):
        module = self.module()
        code, _, err = self.run_main(module, 'restore', self.backup_file, self.restored)
        self.assertEqual(code, 0, err)
        self.assertEqual(self.restored.read_bytes(), self.backup_file.read_bytes())
        self.assertEqual(module.verify(self.restored)['status'], 'ok')

    # Finding 2: publish the inode that was written and hashed, never a re-resolved path.
    def substituting_link(self):
        other = self.root / 'substitute.bin'
        other.write_bytes(b'owned-substitute')
        real_link = os.link

        def link(source, destination):
            real_link(other, destination)

        return link

    def test_backup_refuses_substituted_temp_inode(self):
        module = self.module()
        with mock.patch.object(module.os, 'link', self.substituting_link()):
            with self.assertRaises(module.OperationError):
                module.backup(self.source, self.root / 'out.sqlite')
        self.assert_nothing_published(self.root / 'out.sqlite')

    def test_restore_refuses_substituted_temp_inode(self):
        module = self.module()
        with mock.patch.object(module.os, 'link', self.substituting_link()):
            code, out, err = self.run_main(module, 'restore', self.backup_file, self.restored)
        self.assertEqual((code, out), (1, ''))
        self.assertEqual(json.loads(err)['status'], 'failed')
        self.assert_nothing_published(self.restored)

    # Finding 3: `..` in the destination.
    def test_dotdot_destination_resolving_to_source_is_refused(self):
        module = self.module()
        (self.root / 'sub').mkdir()
        before = self.source.read_bytes()
        for operation, middle in (('backup', 'sub'), ('restore', 'sub'),
                                  ('backup', 'missing'), ('restore', 'missing')):
            with self.subTest(operation=operation, middle=middle):
                origin = self.source if operation == 'backup' else self.backup_file
                dest = self.root / middle / '..' / origin.name
                code, out, err = self.run_main(module, operation, origin, dest)
                self.assertEqual((code, out), (1, ''))
                self.assertEqual(json.loads(err), {'status': 'failed'})
        self.assertEqual(self.source.read_bytes(), before)
        self.assertEqual(list(self.root.glob('.sqlite-backup-*')), [])

    def test_dotdot_destination_publishes_at_resolved_location(self):
        module = self.module()
        (self.root / 'sub').mkdir()
        dest = self.root / 'sub' / '..' / 'viadotdot.sqlite'
        code, _, err = self.run_main(module, 'backup', self.source, dest)
        self.assertEqual(code, 0, err)
        self.assertTrue((self.root / 'viadotdot.sqlite').is_file())
        self.assertTrue((self.root / 'viadotdot.sqlite.manifest.json').is_file())

    def test_dotdot_does_not_hide_a_symlinked_or_junction_parent(self):
        module = self.module()
        (self.root / 'sub').mkdir()
        real = Path.is_symlink
        dest = self.root / 'sub' / '..' / 'blocked.sqlite'
        with mock.patch.object(Path, 'is_symlink',
                               lambda path: path.name == 'sub' or real(path)):
            code, _, _ = self.run_main(module, 'backup', self.source, dest)
        self.assertEqual(code, 1)
        self.assert_nothing_published(self.root / 'blocked.sqlite')

    # Finding 4: interpreters without Path.is_junction, and any unexpected exception.
    def without_is_junction(self):
        def missing(path):
            raise AttributeError('is_junction')
        return mock.patch.object(Path, 'is_junction', property(missing))

    def test_interpreter_without_is_junction_still_works_and_stays_quiet(self):
        module = self.module()
        with self.without_is_junction():
            code, out, err = self.run_main(module, 'backup', self.source, self.root / 'old.sqlite')
        self.assertEqual(code, 0, err)
        self.assertEqual(err, '')
        self.assertEqual(self.root.joinpath('old.sqlite').read_bytes()[:16], b'SQLite format 3\x00')

    def test_junction_fallback_recognises_mount_point_reparse_tag(self):
        module = self.module()
        junction = types.SimpleNamespace(st_reparse_tag=0xA0000003)
        plain = types.SimpleNamespace()
        with self.without_is_junction():
            with mock.patch.object(module.os, 'lstat', return_value=junction):
                self.assertTrue(module.is_junction(self.root))
            with mock.patch.object(module.os, 'lstat', return_value=plain):
                self.assertFalse(module.is_junction(self.root))

    def test_unexpected_exception_becomes_content_free_failure(self):
        module = self.module()
        for operation in ('backup', 'verify', 'restore'):
            with self.subTest(operation=operation):
                args = [operation, self.backup_file] + ([] if operation == 'verify' else [self.restored])
                boom = RuntimeError(str(self.root) + 'private-marker')
                with mock.patch.object(module, 'verify', side_effect=boom), \
                     mock.patch.object(module, 'open_source', side_effect=boom):
                    code, out, err = self.run_main(module, *args)
                self.assertEqual((code, out), (1, ''))
                self.assertEqual(json.loads(err), {'status': 'failed'})
                self.assertNotIn('private-marker', err)
                self.assertNotIn(str(self.root), err)

    def test_unexpected_exception_subprocess_has_no_traceback(self):
        runner = self.root / 'run.py'
        runner.write_text(
            'import importlib.util, sys\n'
            'spec = importlib.util.spec_from_file_location("t", sys.argv[1])\n'
            'm = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)\n'
            'def boom(*a): raise RuntimeError(sys.argv[2] + "private-marker")\n'
            'm.verify = boom\n'
            'sys.exit(m.main(["verify", sys.argv[2]]))\n')
        result = subprocess.run([sys.executable, str(runner), str(TOOL), str(self.backup_file)],
                                capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stderr), {'status': 'failed'})


if __name__ == '__main__':
    unittest.main()

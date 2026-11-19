"""Explicit, offline SQLite backup. Standard library only; no discovery or retention."""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile


TOOL_VERSION = '2'
MANIFEST_SUFFIX = '.manifest.json'
MANIFEST_KEYS = {'sha256', 'tables', 'schema_hash', 'tool_version', 'source_hint'}
MANIFEST_LIMIT = 1 << 20


class OperationError(Exception):
    """A deliberately content-free operator failure with an optional stable code."""

    def __init__(self, code=None):
        super().__init__()
        self.code = code


def open_source(path):
    path = Path(path).resolve(strict=True)
    if not path.is_file() or os.path.lexists(str(path) + '-journal'):
        raise OperationError()
    with path.open('rb') as stream:
        header = stream.read(100)
        if header[:16] != b'SQLite format 3\x00':
            raise OperationError()
    # Existing WAL frames require SQLite's normal WAL reader. If both WAL
    # sidecars are absent, only a quiescent standalone main file is supported;
    # immutable avoids SQLite creating WAL/SHM alongside that source.
    immutable = False
    if header[18:20] == b'\x02\x02':
        sidecars = [Path(str(path) + suffix) for suffix in ('-wal', '-shm')]
        if not any(os.path.lexists(p) for p in sidecars):
            immutable = True
        elif not all(p.is_file() and not p.is_symlink() for p in sidecars):
            raise OperationError()
    uri = path.as_uri() + '?mode=ro' + ('&immutable=1' if immutable else '')
    return sqlite3.connect(uri, uri=True, timeout=5)


def check(db):
    if db.execute('PRAGMA integrity_check').fetchall() != [('ok',)]:
        raise OperationError()
    return {'status': 'ok',
            'schema_objects': db.execute('SELECT count(*) FROM sqlite_schema').fetchone()[0],
            'tables': db.execute("SELECT count(*) FROM sqlite_schema WHERE type='table'").fetchone()[0]}


def file_sha256(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def schema_hash(db):
    rows = db.execute('SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name').fetchall()
    return hashlib.sha256(json.dumps(rows).encode()).hexdigest()


def table_counts(db):
    names = [row[0] for row in db.execute(
        "SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")]
    return {name: db.execute('SELECT count(*) FROM "%s"' % name.replace('"', '""')).fetchone()[0]
            for name in names}


def manifest_path(path):
    return Path(str(path) + MANIFEST_SUFFIX)


def build_manifest(db, digest, source):
    hint = hashlib.sha256(Path(source).name.encode()).hexdigest()[:16]
    return {'sha256': digest, 'tables': table_counts(db), 'schema_hash': schema_hash(db),
            'tool_version': TOOL_VERSION, 'source_hint': hint}


def write_manifest(path, manifest):
    """Create-if-absent; O_EXCL also refuses to follow a planted symlink."""
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, 'O_BINARY', 0)
    fd = os.open(manifest_path(path), flags, 0o600)
    with os.fdopen(fd, 'wb') as stream:
        stream.write(json.dumps(manifest, sort_keys=True).encode())


def load_manifest(path):
    target = manifest_path(path)
    if not os.path.lexists(target):
        raise OperationError('manifest_missing')
    if target.is_symlink() or not target.is_file() or target.stat().st_size > MANIFEST_LIMIT:
        raise OperationError('manifest_invalid')
    try:
        data = json.loads(target.read_bytes())
    except ValueError:
        raise OperationError('manifest_invalid') from None
    counts = data.get('tables') if isinstance(data, dict) else None
    if (not isinstance(data, dict) or set(data) != MANIFEST_KEYS
            or not isinstance(counts, dict)
            or not all(isinstance(k, str) and type(v) is int for k, v in counts.items())
            or not all(isinstance(data[k], str) for k in MANIFEST_KEYS - {'tables'})):
        raise OperationError('manifest_invalid')
    return data


def verify(path):
    """Recompute everything the manifest claims; each failure has its own code."""
    path = Path(path).resolve(strict=True)
    with closing(open_source(path)) as db:
        try:
            manifest = load_manifest(path)
        except OperationError:
            check(db)  # an unreadable database stays a plain, code-less failure
            raise
        if file_sha256(path) != manifest['sha256']:
            raise OperationError('checksum_mismatch')
        result = check(db)
        if schema_hash(db) != manifest['schema_hash']:
            raise OperationError('schema_mismatch')
        if table_counts(db) != manifest['tables']:
            raise OperationError('row_count_mismatch')
        if db.execute('PRAGMA foreign_key_check').fetchall():
            raise OperationError('foreign_key_violation')
    return result


def backup(source, destination):
    source = Path(source).resolve(strict=True)
    destination = Path(destination).absolute()
    if (any(p.is_symlink() or p.is_junction()
            for p in (destination, *destination.parents))
            or any(os.path.lexists(str(destination) + suffix)
            for suffix in ('', '-wal', '-shm', '-journal', MANIFEST_SUFFIX))
            or destination.resolve() == source):
        raise OperationError()
    temporary = None
    published = False
    try:
        with closing(open_source(source)) as src:
            fd, name = tempfile.mkstemp(prefix='.sqlite-backup-', suffix='.tmp', dir=destination.parent)
            temporary = Path(name)
            os.close(fd)
            with closing(sqlite3.connect(temporary)) as dst:
                src.backup(dst)
                result = check(dst)
                manifest = build_manifest(dst, None, source)
            manifest['sha256'] = file_sha256(temporary)
            # Hard-link publication is atomic create-if-absent, never rename/replace.
            os.link(temporary, destination)
            published = True
            write_manifest(destination, manifest)
        return result
    except BaseException:
        if published:
            destination.unlink()
        raise
    finally:
        if temporary is not None:
            temporary.unlink()


class SafeParser(argparse.ArgumentParser):
    """Do not echo untrusted argument values, paths, or parser exceptions."""

    def error(self, message):
        self.exit(2, '{"status": "invalid_arguments"}\n')


def fail(code):
    body = {'status': 'failed'}
    if code:
        body['code'] = code
    print(json.dumps(body, sort_keys=True), file=sys.stderr)


def main(argv=None):
    parser = SafeParser(prog='offline_sqlite_backup.py', description='Offline SQLite backup to a NEW path; no retention or encryption.')
    parser.add_argument('operation', choices=['backup', 'verify', 'restore'])
    parser.add_argument('source')
    parser.add_argument('destination', nargs='?')
    args = parser.parse_args(argv)
    try:
        if args.operation == 'verify':
            if args.destination is not None:
                raise OperationError()
            result = verify(args.source)
        else:
            if args.destination is None:
                raise OperationError()
            result = backup(args.source, args.destination)
    except OperationError as error:
        fail(error.code)
        return 1
    except (OSError, sqlite3.Error, ValueError):
        fail(None)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == '__main__':
    sys.exit(main())

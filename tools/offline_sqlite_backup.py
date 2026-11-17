"""Explicit, offline SQLite backup. Standard library only; no discovery or retention."""
import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile


class OperationError(Exception):
    """A deliberately content-free operator failure."""


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


def backup(source, destination):
    source = Path(source).resolve(strict=True)
    destination = Path(destination).absolute()
    if (any(p.is_symlink() or p.is_junction()
            for p in (destination, *destination.parents))
            or any(os.path.lexists(str(destination) + suffix)
            for suffix in ('', '-wal', '-shm', '-journal'))
            or destination.resolve() == source):
        raise OperationError()
    temporary = None
    try:
        with closing(open_source(source)) as src:
            fd, name = tempfile.mkstemp(prefix='.sqlite-backup-', suffix='.tmp', dir=destination.parent)
            temporary = Path(name)
            os.close(fd)
            with closing(sqlite3.connect(temporary)) as dst:
                src.backup(dst)
                result = check(dst)
            # Hard-link publication is atomic create-if-absent, never rename/replace.
            os.link(temporary, destination)
        return result
    finally:
        if temporary is not None:
            temporary.unlink()


class SafeParser(argparse.ArgumentParser):
    """Do not echo untrusted argument values, paths, or parser exceptions."""

    def error(self, message):
        self.exit(2, '{"status": "invalid_arguments"}\n')


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
            with closing(open_source(args.source)) as db:
                result = check(db)
        else:
            if args.destination is None:
                raise OperationError()
            result = backup(args.source, args.destination)
    except (OSError, sqlite3.Error, OperationError, ValueError):
        print('{"status": "failed"}', file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == '__main__':
    sys.exit(main())

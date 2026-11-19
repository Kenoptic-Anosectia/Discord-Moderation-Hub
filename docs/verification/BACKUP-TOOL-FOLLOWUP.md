# Backup tool follow-up (SF6, tools/offline_sqlite_backup.py)

Tests: `python -m unittest discover -s tests/ops -p test_offline_sqlite_backup.py` (class `FollowupTests`).

## 1 Restore certified unverified bytes (CARD-019): REPRODUCED, fixed
- `restore` ran `verify` then `backup()` re-opened and re-serialized the source. Now `restore()` streams the source's main file into a private temp, hashing what it copies, and refuses (`checksum_mismatch`) unless that hash and a re-hash of the temp equal `manifest['sha256']`. The copy is opened `immutable=1` for the integrity, schema and row-count checks, then published.
- Test: the source is swapped after `verify` returns; restore refuses and leaves no output, manifest or temp. A good restore is byte-identical to its source.
- Not fixed: the window between `verify` and the copy still exists, it is now detected rather than closed. A swap before `verify` reads is the old `verify` limit.

## 2 Publish by inode (CARD-018): REPRODUCED, fixed where checkable
- The temp is created by `mkstemp` and its descriptor stays open; the checksum is taken from that descriptor, and `publish` compares `st_dev/st_ino` of the descriptor with the temp path before and with the published name after `os.link`. A mismatch removes the name it created and fails (`publish_mismatch`). No manifest is written for a refused file.
- Test: `os.link` is replaced to link a different file; backup and restore both refuse.
- Not fixed: `os.link` still takes a path, so the substitution is detected after the fact, not prevented (no `linkat` by descriptor on Windows; the Linux `/proc/self/fd` form was not used). `O_NOFOLLOW` is used only when opening the restore source. Run only on Windows here; POSIX not run.

## 3 `Path.absolute()` keeps `..` (CARD-035): DID NOT REPRODUCE, no code change
- `refuse_destination` already compares `destination.resolve()` with the source, so `sub/../source` (existing or missing `sub`) is refused for backup and restore. Tests added: refusal, publication at the resolved location, and a symlinked or junction `sub` named before `..` stays refused (mocked detection; a real symlink is not creatable here).

## 4 `Path.is_junction` needs Python 3.12 (CARD-036): REPRODUCED, fixed
- New `is_junction()` uses the native method when present, else the `st_reparse_tag` mount-point tag (Windows), else False. `main` now catches every `Exception` and prints only `{"status": "failed"}`.
- Tests: the attribute is made absent (backup works and is quiet); the tag is recognised through a mocked `lstat`; a forced `RuntimeError` carrying a path gives the content-free failure in-process and as a subprocess (no traceback).
- Not run: a real Python < 3.12 interpreter (3.14 only) and a real junction on a real older Windows. `KeyboardInterrupt` is deliberately not caught.

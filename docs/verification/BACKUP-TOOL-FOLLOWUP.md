# Backup tool follow-up (SF6, tools/offline_sqlite_backup.py)

Tests: `python -m unittest discover -s tests/ops -p test_offline_sqlite_backup.py` (`FollowupTests`).

## 1 Restore certified unverified bytes (CARD-019): REPRODUCED, fixed
- `restore` used to re-serialize the source after `verify`. Now `verify` returns the manifest it
  checked and `restore` uses that one object: it streams the source into a private temp, refuses
  (`checksum_mismatch`) unless the copy's hash equals `manifest['sha256']`, then runs the full
  verify checks (integrity, schema, row counts, `foreign_key_check`) on the `immutable=1` copy.
- Tests: source swapped after `verify`; source AND manifest swapped together (a foreign-key
  violating pair). Both refuse with no output, manifest or temp file.
- Not closed: a swap between `verify` and the copy is caught only if the swapped pair fails the
  checks above. A different, internally valid pair (own manifest) is not detectable without a
  signed manifest; swaps before `verify` reads are the old `verify` limit.

## 2 Publish by inode (CARD-018): REPRODUCED, fixed where checkable
- The temp's descriptor stays open and is hashed; `publish` compares `st_dev/st_ino` before and
  after `os.link`, removes the name it created on mismatch (`publish_mismatch`).
- Not fixed: `os.link` takes a path, so substitution is detected, not prevented. Windows only;
  POSIX not run.

## 3 `..` in destination (CARD-035): DID NOT REPRODUCE, no code change
- `refuse_destination` already resolves the destination; tests added and pass unchanged.

## 4 `Path.is_junction` needs Python 3.12 (CARD-036): REPRODUCED, fixed
- `is_junction()` falls back to the reparse tag; `main` catches every `Exception` and prints only
  `{"status": "failed"}`. Not run: a real Python < 3.12 or a real junction.

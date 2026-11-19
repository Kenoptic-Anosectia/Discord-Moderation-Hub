# Backup tool hardening (tools/offline_sqlite_backup.py)

Tests: `python -m unittest discover -s tests/ops -p test_offline_sqlite_backup.py` (`HardeningTests`).

## Finding 1 - `immutable=1` drops WAL-only rows: did NOT reproduce on base 13a1764
- `open_source` uses `immutable` only for a WAL-mode header with neither `-wal` nor `-shm`.
  With any sidecar it opens `mode=ro` and SQLite replays the WAL; a half-present pair is refused.
- Test `test_wal_only_rows_are_in_backup_and_manifest`. No code change. Row loss that remains: a
  DB-only copy from a running writer (the WAL is already gone) and a hardlink alias.

## Finding 2 - verify checked nothing: REPRODUCED, fixed
- `backup` writes `<dest>.manifest.json` (O_EXCL): SHA-256, row counts, schema hash, tool version,
  source hint. `verify` fails with stable codes: `sidecar_present`, `manifest_missing`,
  `manifest_invalid`, `checksum_mismatch`, `schema_mismatch`, `row_count_mismatch`,
  `foreign_key_violation`. Corrupt databases stay a plain `{"status": "failed"}`.

## WAL overlay and restore (audit CARD-019)
- `verify` refuses `sidecar_present` if `-wal`, `-shm` or `-journal` exists, because the checksum
  covers only the main file. `restore` verifies its source first; see BACKUP-TOOL-FOLLOWUP.md.

## NOT fixed / limits
- The manifest is unsigned: a schema-identical foreign DB with its own valid manifest passes.
- TOCTOU: `verify` checks sidecars then opens by path; a swap in that window is not excluded.
  `restore` copies the bytes it hashed and re-checks that copy against the single manifest
  `verify` checked, so a swap after `verify` is refused only if it fails those checks.
- A source with FK violations backs up but fails `verify`. Nothing in the Rust bot calls this tool.

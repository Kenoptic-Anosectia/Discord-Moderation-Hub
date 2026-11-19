# Backup tool hardening (tools/offline_sqlite_backup.py)

Tests: `python -m unittest discover -s tests/ops -p test_offline_sqlite_backup.py` (class `HardeningTests`).

## Finding 1 - `immutable=1` drops WAL-only rows: did NOT reproduce on base 13a1764
- `open_source` already sets `immutable` only when the header is WAL mode AND neither `-wal` nor `-shm` exists. With any sidecar present it opens plain `mode=ro`, so SQLite replays the WAL; a half-present pair is refused.
- Tests: `test_wal_only_rows_are_in_backup_and_manifest` (newest rows exist only in a non-empty `-wal`, writer still open; backup holds all of them) and `test_wal_without_shm_is_refused_not_read_immutable` pass on base for the data part. No code change for this finding.
- Not covered: a WAL created after the backup opens the source; the tool is offline-only by design.

## Finding 2 - verify checked nothing: REPRODUCED, fixed
- `backup` now writes `<dest>.manifest.json` (create-if-absent, `O_EXCL`, refused if present): SHA-256 of the backup, per-table row counts, schema hash, tool version, source hint (hash of the source file name only).
- `verify` recomputes in this order, each with its own stable stderr code: `manifest_missing`, `manifest_invalid`, `checksum_mismatch`, `schema_mismatch`, `row_count_mismatch`, `foreign_key_violation` (`PRAGMA foreign_key_check`). Unreadable/corrupt databases stay the plain `{"status": "failed"}`.
- Tests flip a byte, delete a row, break a foreign key, swap in a foreign DB. Row/FK/schema cases rewrite the manifest checksum first (attacker model) so the later check is the one that fires; without the rewrite each fails `checksum_mismatch`.
- No subprocess/shell added; symlink/junction/hardlink/sidecar defenses unchanged.

## NOT fixed / limits
- The manifest is unsigned: someone who can rewrite both the backup and its manifest, keeping schema and counts, passes verify. It detects corruption and mistakes, not a determined local attacker.
- `restore` is unchanged (still a plain copy); it does not verify the manifest of its input, and writes a manifest for its output. Run `verify` first.
- A source that already has FK violations backs up fine but then fails `verify`.
- Nothing in the Rust bot calls this tool; no wiring changed.

# Backup tool hardening (tools/offline_sqlite_backup.py)

Tests: `python -m unittest discover -s tests/ops -p test_offline_sqlite_backup.py` (class `HardeningTests`).

## Finding 1 - `immutable=1` drops WAL-only rows: did NOT reproduce on base 13a1764
- `open_source` sets `immutable` only when the header is WAL mode AND neither `-wal` nor `-shm` exists. With any sidecar it opens plain `mode=ro`, so SQLite replays the WAL; a half-present pair is refused. qa-3 probed seven shapes on base and HEAD with identical results.
- Test: `test_wal_only_rows_are_in_backup_and_manifest` (rows only in a non-empty `-wal`, writer open; backup holds all). No code change.
- Row loss that remains, because no -wal sits beside the path given: (D) a DB-only copy taken from a running writer: the WAL is gone before the tool runs, no tool can recover it; (G) a hardlink alias: the `-wal` is named after the original path.

## Finding 2 - verify checked nothing: REPRODUCED, fixed
- `backup` writes `<dest>.manifest.json` (create-if-absent, `O_EXCL`): SHA-256, per-table row counts, schema hash, tool version, source hint (hash of the source file name).
- `verify` fails with its own stable stderr code: `sidecar_present`, `manifest_missing`, `manifest_invalid`, `checksum_mismatch`, `schema_mismatch`, `row_count_mismatch`, `foreign_key_violation`. Corrupt databases stay a plain `{"status": "failed"}`.
- Tests flip a byte, delete a row, break a foreign key, swap in a foreign DB (row/FK/schema cases rewrite the manifest checksum first, else each is `checksum_mismatch`).

## WAL overlay (qa-3 round 1) and restore (audit CARD-019)
- The checksum covers only the main file, but SQLite also reads a `-wal`/`-shm` beside it, so a planted committed WAL passed `verify` (header 2/2 and 1/1). `verify` now refuses `sidecar_present` if `-wal`, `-shm` or `-journal` exists (`lexists`, before opening). Tests: planted overlay, both headers.
- `restore` now runs the destination checks and `verify` on its source first and refuses on ANY code with no output or manifest written. Tests: tampered, missing manifest, sidecar, FK violation. Otherwise unchanged.

## NOT fixed / limits
- The manifest is unsigned: a schema-identical foreign DB with its own valid manifest passes; `tool_version` and `source_hint` are recorded, never checked.
- A sidecar created between the `lexists` check and the open is not excluded (TOCTOU); needs a local attacker with write access.
- Out of scope (SF6): publish-by-inode TOCTOU, `resolve()` of the destination, Python 3.12 floor for `Path.is_junction`.
- A source that already has FK violations backs up but fails `verify`. Nothing in the Rust bot calls this tool.

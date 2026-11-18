# CORE-STORAGE (K01)

`src/storage.rs`: synchronous, Discord-free SQLite opener and per-feature migration runner for the shared `core.sqlite`.

## Rules enforced
- `open(path, reserved)` sets `busy_timeout=5000ms`, WAL (file DBs), `foreign_keys=ON`, `synchronous=NORMAL`, and verifies them. `:memory:` is supported. Opens with explicit flags (no URI mode).
- Refuses a path equal to a `reserved` path after normalization (relative/absolute, `.`/`..`, trailing separators, symlink/junction/short-name resolution of the deepest existing ancestor, case-insensitive and trailing-dot/space-stripped on Windows). Refuses `file:` URIs and, on Windows, any non-drive path prefix (UNC in any `/`/`\` mix, verbatim, device) and any component containing `:` (NTFS alternate streams) with `InvalidPath`. Refusal happens before the file is opened.
- `migrate(conn, feature, &[Migration])`: versions 1..n in order (duplicates/gaps/zero rejected before any write). Pending steps plus the `schema_versions` bump run in ONE `BEGIN IMMEDIATE` transaction; any failure rolls back the whole call. Rerun is a no-op. Stored version newer than the code is refused with no data writes; a non-integer or negative stored version is `CorruptStoredVersion`. Features are independent. An empty list on a fresh DB leaves no table.
- After each step `migrate` checks the transaction is still open; a step that ran COMMIT/ROLLBACK/END returns `StepEndedTransaction` before any later step or the version bump. Triggers (`BEGIN..END`) are fine.
- `StorageError` is a plain `Copy` enum of stable codes; no SQL, path or message text.

## Tests (31, in `src/storage.rs`)
Fresh/rerun/incremental, newer-version refusal (bytes + rows), middle-step rollback, feature independence, pragma read-back, FK, `:memory:`, reserved spellings (real files in `target/k01-tests/`), `file:` URI refusal with reserved bytes unchanged, UNC (incl. mixed-separator) and ADS refusal, trailing dot/space on a not-yet-existing reserved file (Windows only), COMMIT/ROLLBACK/END steps, trigger step, corrupt version, concurrent two-connection migrate, invalid lists, error-text leaks.
RED: `3ed6d83` (stubs): 20 failed, 1 passed. Round 2 RED `fe2fa97`: 7 new tests failed (URI, UNC, trailing dot/space, 3 step-ended-tx, non-integer version). Round 3 RED `74bb07f`: mixed-separator UNC and ADS tests failed (2).

## Config keys (read by the wiring card, not here)
- `CORE_DB_PATH`: default `core.sqlite` under the data dir (wiring card decides); unset/empty or reserved => feature layer stays OFF, bot logs a code, no file created.
- `TICKET_DB_PATH`, `REACTION_ROLE_DB_PATH` are passed as `reserved`; never opened here.

## NOT done / limits
No runtime wiring, commands, config parsing, feature tables or backup tooling. `#![allow(dead_code)]` is temporary; wiring card W1 removes it. No downgrade path: rollback = restore a backup. Work a misbehaving step already committed cannot be undone, and statements after a COMMIT inside one multi-statement step still run outside the transaction. Not detected: hardlinks to a reserved file, a step that re-opens a transaction (`COMMIT; BEGIN;`), aliasing via other mounts. Case-insensitivity tests run only on Windows.

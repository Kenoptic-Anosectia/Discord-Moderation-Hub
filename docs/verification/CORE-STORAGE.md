# CORE-STORAGE (K01)

`src/storage.rs`: synchronous, Discord-free SQLite opener and per-feature migration runner for the shared `core.sqlite`.

## Rules enforced
- `open(path, reserved)` sets `busy_timeout=5000ms`, WAL (file DBs), `foreign_keys=ON`, `synchronous=NORMAL`, and verifies them. `:memory:` is supported.
- Refuses any path equal to a `reserved` path after normalization (relative/absolute, `.`/`..`, trailing separators, symlink/short-name resolution of existing ancestors, case-insensitive on Windows). Refusal happens before the file is touched.
- `migrate(conn, feature, &[Migration])`: steps must be versions 1..n in order (duplicates / gaps / zero rejected before any write). All pending steps plus the `schema_versions` bump run in ONE `BEGIN IMMEDIATE` transaction; any failure rolls back the whole call. Rerun is a no-op. Stored version newer than the code is refused with no data writes. Features are independent.
- `StorageError` is a plain `Copy` enum of stable codes; SQLite errors collapse to `Sqlite` with no message, SQL or path.

## Tests (21, in `src/storage.rs`)
Fresh/rerun/incremental, newer-version refusal (file bytes + row dump identical), failing middle step rollback, feature independence, pragma read-back, FK enforcement, `:memory:`, reserved-path spellings (real files in `target/k01-tests/`), concurrent migrate from two connections (applied exactly once, integrity_check ok), invalid step lists, error text leak check.
RED evidence: commit `3ed6d83` (stubs `unimplemented!`): 20 failed, 1 passed (`error_codes_are_distinct_and_stable`).

## Config keys (read by the wiring card, not here)
- `CORE_DB_PATH`: default `core.sqlite` under the data dir (wiring card decides); unset/empty or a reserved path => feature layer stays OFF, bot logs a code, no file is created.
- Existing `TICKET_DB_PATH`, `REACTION_ROLE_DB_PATH` are passed as `reserved`; never opened here.

## NOT done
No runtime wiring, no commands, no config parsing, no feature tables, no backup tooling. `#![allow(dead_code)]` in `storage.rs` is temporary; wiring card W1 removes it. Step SQL must not contain its own BEGIN/COMMIT. No downgrade path: rollback = restore a backup.

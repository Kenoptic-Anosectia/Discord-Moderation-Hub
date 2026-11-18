# ARCHIVE-SCAN (A18 — Read-only archive health scan and orphan report)

Branch `wt/skynet-a08-scan`, baseline `14b8a0466fca560db627fb9c0a23608afb5cc534`.
Scope: offline, strictly read-only health verification of recorded ticket archives and immediate-child orphan classification inside an `OwnedRoot`. Nothing in this slice is wired into live Discord commands or startup runtime yet (deferred to the A08 integration slice).

## Files
- `src/tickets/archive_scan.rs` — public types (`Health`, `ScanEntry`, `ScanResult`, `OrphanReport`, `ScanError`, `MAX_ROOT_ENTRIES`), read-only functions (`scan_recorded`, `find_orphans`), and unit tests (`tickets::archive_scan::tests`).
- `docs/verification/ARCHIVE-SCAN.md` — this verification and semantics document.

## Meaning of `Health` values (`scan_recorded`)

`scan_recorded(root, entries)` evaluates each `ScanEntry` in input order without mutating the filesystem or following symlinks:

| `Health` | Meaning |
|---|---|
| `Verified` | `dir_name` matches `ticket-<digits>-<digits>`, exists as a real non-symlink directory under `root`, passes byte-level `archive::verify(root, dir, Some(&manifest_sha256), &[])` (manifest SHA-256, transcript length and SHA-256, every asset length/declared size/SHA-256, no extra files or symlinks), and its manifest matches `format == 1`, `ticket`, `channel`, `message_count == messages`, and `assets.len() as u64 == assets`. |
| `Missing` | `dir_name` has valid `ticket-<digits>-<digits>` syntax, but `symlink_metadata` reports `ErrorKind::NotFound` under `root`. |
| `Corrupt` | The path under `root` is a symlink or non-directory, `symlink_metadata` fails with a non-`NotFound` I/O error, or `archive::verify` fails (manifest hash mismatch, malformed JSON, transcript/asset byte corruption, missing/extra files, or symlinked children). |
| `IdentityMismatch` | The archive directory passes byte-level `archive::verify` against `manifest_sha256`, but the manifest metadata disagrees with the database record (`format != 1`, `ticket != entry.ticket`, `channel != entry.channel`, `message_count != entry.messages`, or `assets.len() as u64 != entry.assets`). |
| `BadName` | `dir_name` is not strictly `ticket-<digits>-<digits>` (e.g. `..`, path separators, leading dot, non-digits, wrong segment count). Rejected before any filesystem call; nothing outside `root` is read. |

## Orphan reporting (`find_orphans`) — reported, never removed

`find_orphans(root, known_dir_names)` inspects immediate children of `root.path()` via `std::fs::symlink_metadata` (never following symlinks) and enforces `MAX_ROOT_ENTRIES = 10_000` (returning `Err(ScanError::TooMany)` before classifying if exceeded):

- `unrecorded_archives`: real (non-symlink) directories named `ticket-<digits>-<digits>` that are not present in `known_dir_names`.
- `staging_debris`: non-symlink entries whose name starts with `.staging-` or `.trash-`.
- `unknown_entries`: everything else (stray files, symlinks including any symlink named `ticket-*`, and directories with any other name).

Each vector in `OrphanReport` is sorted ascending. **Orphans are reported only and never removed, renamed, or modified.** Neither `scan_recorded` nor `find_orphans` ever writes, renames, or deletes any filesystem entry.

## Point-in-time observation

A clean scan (`Health::Verified` for all recorded entries and an empty `OrphanReport`) is strictly a **point-in-time observation** of the filesystem state when the scan ran. It is not a lock, capability guarantee, or proof of future integrity: subsequent disk faults, operator edits, or concurrent same-account modifications after the scan completes can still alter or remove files on disk, so downstream operations that require verified bytes (such as restore or removal) must still perform their own point-in-use verification.

## TDD evidence (RED -> GREEN)

- **RED**: `src/tickets/archive_scan.rs` was first compiled with stub implementations (`scan_recorded` returning an empty `Vec` and `find_orphans_with_cap` returning `Err(ScanError::Io)`) against the 13 unit tests in `tickets::archive_scan::tests`.
  - Command: `CARGO_BUILD_JOBS=2 cargo test --locked --offline archive_scan`
  - Result: exit 101 — `1 passed; 12 failed` (the 1 pass in RED was `symlinked_ticket_entry_is_rejected_and_target_untouched`, which hit the unprivileged Windows symlink-creation skip path and reported the skip).
- **GREEN**: Implemented `is_archive_dir_name`, `scan_recorded`, `find_orphans`, and `find_orphans_with_cap`.
  - Command: `CARGO_BUILD_JOBS=2 cargo test --locked --offline archive_scan`
  - Result: exit 0 — `13 passed; 0 failed`.

## Verification gates (`CARGO_BUILD_JOBS=2`, `--locked --offline`)

| Command | Exit code | Result |
|---|---|---|
| `CARGO_BUILD_JOBS=2 cargo fmt --all -- --check` | 0 | Clean |
| `CARGO_BUILD_JOBS=2 cargo check --locked --offline --all-targets` | 0 | Clean |
| `CARGO_BUILD_JOBS=2 cargo test --locked --offline` | 0 | 109 bin unit tests (including 13 `tickets::archive_scan::tests`) + 30 `archive_input_safety` + 7 `logger_fixture_shapes` + 5 `ticket_archive_fixture_factory` passed (151 total, 0 failed) |
| `CARGO_BUILD_JOBS=2 cargo clippy --all-targets --locked --offline -- -D warnings -D deprecated -D clippy::result_large_err -D clippy::type_complexity` | 0 | Zero warnings/errors |

## Honest limits
- On Windows without elevated/Developer Mode symlink privilege, `std::os::windows::fs::symlink_dir` returns an OS privilege error; `symlinked_ticket_entry_is_rejected_and_target_untouched` logs `SKIPPED symlink test: OS refused symlink creation` and exits cleanly as specified.
- The `TooMany` cap (`MAX_ROOT_ENTRIES = 10_000`) is asserted for its constant value and tested via the private `find_orphans_with_cap` seam with a small cap (`5`) to avoid creating 10,001 filesystem directories during unit tests.
- This slice is an offline building block and is not yet wired into bot commands or runtime.

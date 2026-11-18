# Archive input invariant property sweeps (A14)

Baseline: `74cbe30b2afd28bade3072acde914e2d60180eb9`. Test-only change: adds `tests/archive_input_properties.rs` and this report. No production source, helper, fixture, `Cargo.toml` or `Cargo.lock` change (verified: `git diff --stat -- src Cargo.toml Cargo.lock` is empty).

## What it covers

The tests import the real production module with `#[path = "../src/archive_safety.rs"]` and call `validate_attachment_url`, `generated_asset_name` and `validate_asset_component`. They extend, rather than repeat, the hand-picked cases in `tests/archive_input_safety.rs`.

1. **Generated asset names** - injective, deterministic, ASCII-only, within `MAX_ASSET_NAME_LEN`, and accepted by the real component validator for IDs `1..=4096` plus neighborhoods of `u32::MAX` and `u64::MAX`.
2. **Accepted CDN URLs** - 8 canonical IDs x 6 synthetic signed queries x 2 allowed hosts; the query string and full URL are preserved exactly. The ephemeral-attachments root is also exercised.
3. **Rejected variants** - 7 hostile hosts, 4 non-default ports, 7 bad paths, each carrying a synthetic private marker in the query. Every variant is rejected, and neither `Display` nor `Debug` of the sanitized error echoes the marker or the host. A trailing-newline control-character variant is covered separately.

Seven tests. All inputs are synthetic; nothing resolves DNS, opens a socket or touches the filesystem. The signed-query values and the private marker are arbitrary strings, not credentials.

## Attribution and what actually happened

- **Evolve (local `evolve-tielcoder`)** wrote the test file. Its managed run used all 20 iterations and the card went to `gave_up`, so Evolve never reached formatting, a report or a commit.
- **Directorate (parent)** finished it. Evolve also modified `Cargo.lock`, which the contract forbade. All 261 package records were compared and are identical (the change was a reorder of the root package record), so the lock was restored; the diff is preserved outside this repository.
- **Two test defects were found by running the tests, not by inspection.** On first execution 5 of 7 passed and 2 failed. Both failures were in the test code, not in production: the rejection tests called `unwrap_or_else(|_| panic!(..))` / `.expect(..)` on a `Result`, which panics on the *expected* `Err`. The validator had correctly returned `Err` (for example `ControlCharacter`). Directorate changed these to `expect_err`. That two-line change, plus `rustfmt` on this one file, is the only edit made to Evolve's work.

## Evidence (parent-run, same worktree)

`CARGO_BUILD_JOBS=2`, separate target directory, `--locked --offline`:

- Before the fix: `cargo test --test archive_input_properties` - 5 passed, 2 failed (the inverted-Result tests above).
- After the fix: 7 passed, 0 failed.
- `cargo fmt --all -- --check` - exit 0 after formatting this file.
- `cargo clippy --locked --offline --test archive_input_properties -- -D warnings` - exit 0.

## Limits

- Only the focused target was run; the full suite and all-targets Clippy were not repeated for this test-only addition.
- These are deterministic sweeps, not randomized fuzzing or a proof. They do not cover redirects, filesystem races, junctions or symlinks, which `archive_safety.rs` does not claim to handle.
- Because a defect was found in the author's tests and corrected by the parent, an independent reviewer other than the finisher must approve this work.

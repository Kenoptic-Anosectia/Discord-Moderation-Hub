# Ticket archive fixtures

Synthetic, in-memory Serenity fixtures for future ticket-archive tests.

## API (`tests/support/ticket_archive_fixtures.rs`, include via `#[path]` + `mod`)

- `message_pages(count: usize) -> Vec<Vec<Message>>` — deterministic messages, pages of at most 100
  (205 -> 100/100/5; 0 -> no pages). IDs `10_000 + i` (strictly increasing), channel 1, author id 1
  named `user_{i}`, content `synthetic message number {i}`, timestamps `2026-01-01T00:00:00Z + i s`.
- `AssetFixture { attachment: Attachment, bytes: Vec<u8> }` and `asset_fixtures() -> Vec<AssetFixture>`
  — four assets: ids 100 and 101 both named `same.txt` (different bytes), id 102 `../outside.txt`,
  id 103 `C:\outside.txt`. URLs use `example.invalid`; `size` equals `bytes.len()`.

No network access and no filesystem writes; everything is built in memory.

## Tests (`tests/ticket_archive_fixture_factory.rs`, 5)

Pagination/order/wire round-trip, empty history, determinism, attachment dataset, attachment on a generated message.

## Verification (this finishing run)

- `cargo test --locked --offline` (CARGO_BUILD_JOBS=2): 14 unit + 7 logger_fixture_shapes + 5 factory tests, all pass.
- `cargo fmt --check`: clean. `cargo clippy --locked --offline --all-targets -- -D deprecated -D clippy::result_large_err -D clippy::type_complexity`: finishes; only the 5 pre-existing baseline dead-code warnings (ticket constants/functions, config fields), none from the new files.
- `git diff --check`: clean. Cargo.toml, Cargo.lock, src/, config unchanged.

## Attribution

- Directorate: pair scaffold and the original failing pagination test (behavioral RED recorded in the parent's `a09-recovery/paired-red.log`; supplied evidence, not rerun here).
- Evolve: the generator, the attachment dataset and the four additional tests.
- Cybersec: moved `AssetFixture`/`asset_fixtures` into the shared helper (`pub`), removed the unused `mut`, `assert_ne!` cleanup, formatting, this doc.

## Limitations

Synthetic IDs/timestamps only; these fixtures prove nothing about archive, controller or recovery behavior.

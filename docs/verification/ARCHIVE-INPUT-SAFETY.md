# ARCHIVE-INPUT-SAFETY (A10, T03 preparation)

Branch `wt/skynet-archive-input-safety`, baseline `65e2fc62a4d77970629394ac467f8a3c4d51e542`.
Scope: pure, I/O-free primitives for the future A07 downloader. This does **not** implement archive
downloading, redirects handling, or ticket closure, and T03 is not claimed complete.

## Files
- `src/archive_safety.rs` - module (not declared in `src/main.rs`; no wiring).
- `tests/archive_input_safety.rs` - includes it via `#[path = "../src/archive_safety.rs"]`; calls the real exported functions.
- `Cargo.toml` - one added line: `url = "=2.5.8"` (committed-metadata manifest only; owner's metadata patch NOT copied; parent overlays it at integration).
- `Cargo.lock` - exactly one added line, `"url",`, in the root package's dependency list (`git diff --numstat`: `1 0`). No package record changed or upgraded; `url 2.5.8` was already in the lock.

## API
- `validate_attachment_url(&str) -> Result<url::Url, ArchiveInputError>`: max 2048 bytes; rejects control chars and whitespace; rejects any `\`; parsed by the `url` crate; requires `https`; no userinfo (raw authority containing `@` also rejected, since `https://:@h/` parses with empty credentials); host exactly `cdn.discordapp.com` or `media.discordapp.net` (domain only - IP literals, trailing dot, subdomains, lookalikes, IDNA/punycode rejected); no non-default port (explicit `:443` is normalised away by the parser and accepted); no fragment; path exactly `/attachments/<nonzero u64>/<nonzero u64>/<filename>` or `/ephemeral-attachments/<id>/<id>/<filename>` (the ephemeral form is allowed deliberately, tested). Dot segments (including `%2e`), `%2f`, `%5c`, `%00` in the path are rejected; the normalised path must equal the literal input path. Query string preserved as parsed.
- `generated_asset_name(u64) -> Result<String, _>`: `attachment-<id>.bin`; id 0 rejected; depends on the id only.
- `validate_asset_component(&str) -> Result<(), _>`: accepts only the exact output grammar (case-sensitive, ASCII, canonical decimal, no leading zeros, u64 range, nonzero, <= 64 bytes).
- `ArchiveInputError` / `ArchiveInputErrorKind` (11 kinds). Display/Debug emit only fixed text; no URL, query or name is stored. Implements `std::error::Error`.

## TDD evidence
RED: with an always-erroring stub (same signatures), `cargo test --locked --offline --test archive_input_safety` -> `5 passed; 24 failed` (the 5 "passes" are reject-only tests that an always-Err stub satisfies vacuously). GREEN first pass found one real defect (`https://:@cdn.discordapp.com/...` accepted because empty userinfo parses as none); fixed by raw-authority `@` check. Final: `29 passed; 0 failed`.

## Final gates (CARGO_BUILD_JOBS=2, own ignored `target/`)
- `cargo fmt --check` exit 0
- `cargo check --locked --offline` / `cargo build --locked --offline`: Finished (5 pre-existing dead-code warnings in `src/main.rs`/`src/config.rs`, baseline, untouched)
- `cargo test --locked --offline`: unit 14 passed, `archive_input_safety` 29 passed, `logger_fixture_shapes` 7 passed (baseline 21 unchanged; 29 new)
- `cargo clippy --locked --offline --all-targets -- -D deprecated -D clippy::result_large_err -D clippy::type_complexity`: Finished, no errors
- `git diff --check` exit 0
- No network/DNS/file I/O in tests; synthetic strings only.

## Assumptions and integration limitations
- Initial URL validation does NOT make redirects safe: A07 must disable redirects or revalidate EVERY redirect target with `validate_attachment_url`, bound streamed bytes, and verify filesystem containment.
- `validate_asset_component` is a pure string check; it does not and cannot prevent symlink/junction/TOCTOU races - A07's open layer must do create-new opens inside a verified directory.
- Allow-list reflects current Discord CDN URL shapes (`attachments`, `ephemeral-attachments`); new shapes need deliberate addition. `ex/is/hm` signing is not verified, only preserved.
- A07 must call these functions rather than re-implementing them; original filenames belong in a manifest as display data only.
- The module is not `mod`-declared in `main.rs` (forbidden by scope), so it is exercised only through the integration test; A07 will add the `mod` line.

## Review fix 1 (changes requested on e33ab57)

Defect: `https:////:@cdn.discordapp.com/...` and `https:////@cdn.discordapp.com/...` returned Ok
because raw-authority extraction (`split_once("://")`) left a leading `/` and saw an empty
authority, while `url::Url` normalises extra slashes and empty userinfo away.

Fix: `validate_attachment_url` now requires the raw text to start with `https://`
(case-insensitive) followed by a non-slash authority; any other introducer form
(`https:////`, `https:/@`, `https:@`, ...) is rejected (`InvalidUrl`/`UserInfoNotAllowed`).
The parsed-host allow-list, signed-query preservation, case-insensitive scheme/host and
explicit `:443` behaviour are unchanged (asserted as controls).

RED (fix reverted, new test only): `rejects_noncanonical_authority_introducers_with_empty_userinfo` FAILED (0 passed, 1 failed).
GREEN: `cargo test --locked --offline`: 14 unit + 30 archive_input_safety + 7 logger pass.
New meaningful tests total: 30 (29 + 1 regression covering 13 spellings + 3 controls).
`cargo fmt --check`, targeted all-targets Clippy (-D deprecated/result_large_err/type_complexity), `git diff --check` clean.

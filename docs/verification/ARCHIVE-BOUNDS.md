# Archive bounds (audit HIGH x2)
Files changed: `src/tickets/archive.rs`, `src/tickets/archive_tests.rs`. No callers changed (no signature changed).

## 1. `verify()` quadratic id scan - reproduced
- Was: one `html.contains(data-mid="<id>")` scan per expected id (O(ids x size)).
- Fix: one pass (`match_indices`) collects every `data-mid="<value>"` into a `HashSet`; ids are O(1) lookups.
  Same exact-match semantics, same `Verify` error, every other check untouched. `render_transcript`'s
  per-attachment `find` also became a map (first entry wins).
- Test `verify_scales_linearly_with_message_count`: 5,000 vs 20,000 messages, ratio < 10 (best of 3, no absolute
  seconds). RED measured ~25x; fixed code measured about 4-6x, so the margin is ~1.6x, not wide.
  `verify_still_rejects_missing_and_partial_ids` keeps missing/prefix ids rejected.

- Caps count ESCAPED output bytes (so `"` -> `&quot;` cannot amplify 6x past them): `MAX_MESSAGE_TEXT_BYTES` =
  256 KiB per message (Discord text is ~60 KiB), `MAX_TRANSCRIPT_TEXT_BYTES` = 32 MiB per transcript, so the
  rendered text stays <= 32 MiB plus markup (< 64 MiB). Text past a cap stops BEFORE the first escape unit that
  does not fit (never inside `&quot;`) and `TRUNCATION_MARKER` (`[truncated: archive text limit]`) is written once.
  Every message stays listed, so ids and `message_count` still verify.
- Copies: text is escaped straight into the one transcript `String`; `content_fingerprint` streams into the
  SHA-256 context (identical digest).
- Tests: legacy transcript from the unmodified writer verifies and short messages render byte-identical;
  oversized message truncated + archive verifies; UTF-8 boundary; total cap with plain text AND all-quote /
  all-ampersand input asserting the RENDERED length; per-message cap for `" & ' <`; 6-byte unit with 5 left is
  dropped whole, exact fill is not marked.

## NOT fixed / depends on wiring
- Truncation is render-only: `content_fingerprint` still hashes full text; author names, filenames and embed
  `Debug` strings are not capped. Embed parts after a cut are omitted without their own marker.
- `fetch_all` holds whole `Message`s bounded by `max_messages` (200,000) not bytes: ~60 KiB x 200,000 = ~11.6 GiB
  worst case BEFORE rendering. Not bounded here; a follow-up card is needed. Markup per message is uncapped.
- `verify` still reads the whole transcript and each asset into memory.

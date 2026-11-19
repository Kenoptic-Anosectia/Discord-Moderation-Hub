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
  256 KiB per message (Discord text is ~60 KiB), `MAX_TRANSCRIPT_TEXT_BYTES` = 32 MiB per transcript: TEXT
  is capped at 32 MiB escaped. Text past a cap stops BEFORE the first escape unit that
  does not fit (never inside `&quot;`) and `TRUNCATION_MARKER` (`[truncated: archive text limit]`) is written once.
  Every message stays listed, so ids and `message_count` still verify.
- Copies: text is escaped into the one transcript `String`; the fingerprint streams into SHA-256.
- Tests: legacy archive verifies; oversized message truncated and still verifies; total cap (plain, all-quote,
  all-ampersand) asserts escaped text <= cap and rendered <= cap + markup; split-escape and exact-fill cases;
  `esc_len`/`esc_char` agree with `esc`.

## NOT fixed / depends on wiring
- Truncation is render-only: `content_fingerprint` still hashes full text; author names, filenames and embed
  `Debug` strings are not capped. Embed parts after a cut are omitted without their own marker.
- Rendered transcript is NOT bounded: markup (message wrapper, author names, per-embed wrappers emitted even
  after the budget is spent, the uncounted 31-byte marker) is uncapped. Measured worst case at 200,000 messages,
  max author names, 10 max embeds, 500 attachments: ~122 MiB rendered, publish peak ~244 MiB.
- `fetch_all` holds whole `Message`s: 200,000 x ~60 KiB = ~11.6 GiB BEFORE rendering (bounded by count, not
  bytes). Bounding total rendered size and the fetch is follow-up card SF5, not this card.
- `verify` still reads the whole transcript and each asset into memory.

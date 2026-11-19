# Archive bounds (audit HIGH x2)

Files changed: `src/tickets/archive.rs`, `src/tickets/archive_tests.rs`. No callers changed (no signature changed).

## 1. `verify()` quadratic id scan — reproduced
- Was: one `html.contains(data-mid="<id>")` scan of the transcript per expected id (O(ids x size)).
- Fix: one pass (`match_indices`) collects every `data-mid="<value>"` into a `HashSet`; each id is an O(1)
  lookup. Same exact-match semantics (an id passes only if an attribute value equals it), same `Verify` error,
  every other check untouched. `render_transcript`'s per-attachment `assets.iter().find` also became a map.
- Test: `verify_scales_linearly_with_message_count` compares 5,000 vs 20,000 messages (ratio < 10; quadratic
  = ~16x, linear = ~4x, best of 3, no absolute seconds). RED: 25.0x (1.7s -> 41.9s). Missing/prefix ids still
  rejected (`verify_still_rejects_missing_and_partial_ids`).

## 2. Unbounded message text, ~3x copies on publish — reproduced
- Caps (bytes of raw text, content + embed text): `MAX_MESSAGE_TEXT_BYTES` = 64 KiB per message (Discord allows
  ~4k chars content / 6k per embed), `MAX_TRANSCRIPT_TEXT_BYTES` = 32 MiB per transcript. Text past a cap is cut
  at a char boundary and followed by `TRUNCATION_MARKER` (`[truncated: archive text limit]`, plain ASCII, cannot
  form `data-mid="`). Every message stays listed, so ids and `message_count` still verify.
- Copies: text is escaped straight into the one transcript `String` (no per-field `esc()` temporaries);
  `content_fingerprint` streams into the SHA-256 context instead of building a full copy (identical digest).
- Tests: legacy transcript captured from the unmodified writer verifies and short messages render byte-identical;
  oversized message truncated + archive verifies; UTF-8 boundary; total cap with 576 max-size messages
  (RED: 36 MB transcript); fingerprint still covers the untruncated text.

## NOT fixed / depends on wiring
- Truncation is a rendering bound only: `content_fingerprint` (the pre-delete stability check) still hashes
  full text, and fetched `Message`s are still held whole in memory by `fetch_all` (bounded by `max_messages`,
  not by bytes). Author names, filenames and embed `Debug` strings in the fingerprint are not capped.
- Escaped output can be up to 6x the raw cap (`&quot;`); the cap counts raw bytes.
- `verify` still reads the whole transcript and each asset into memory.

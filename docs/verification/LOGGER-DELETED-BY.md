# Logger: "Deleted By" on Message Deleted (A23)

## Rule
If Discord's audit log has a matching entry for the deletion, show the executor (`<@id>`).
If it has none, assume the author deleted it: `User` (human) or `Bot (author)` (bot).
Lookup failed (no View Audit Log permission, HTTP error, no guild id): `Unknown (audit log unavailable)`.
Message not in cache (author unknown): `Unknown (message not cached)`; no channel/time guessing.

## Audit-log limits
- Discord does not audit-log self-deletes, hence "no entry => author".
- Repeated deletions by one moderator of one user's messages in one channel merge into ONE entry
  (`count` rises, id/timestamp stay at the first). An entry targeting the author in this channel
  counts as this deletion if fresh (<= 8 s by id) OR its count rose since we last looked
  (bounded in-memory map id -> count, 256 entries, oldest evicted, not seeded at startup).
- Counts are recorded on every lookup, so a later self-delete is not credited to the old entry.
- A restart empties the map: a merged entry whose first deletion is stale is missed once (shows `User`).
- Two simultaneous deletions by the same author/channel may share one count increase.
- Lookup is up to 3 tries (0, 0.5, 1.5 s), run in a spawned task; the embed timestamp is the event time.
- `MessageDeleteBulk` is not covered (follow-up).

## Privacy
The audit call lives in the `MessageDelete` arm of `log_event`, which only runs after the
ticket privacy guard allows the event; the guard is unchanged.

## Verified
Unit tests in `src/deletion_attribution.rs` (pure decision logic, map bounds, label text).
NOT tested: real Discord audit entries, permissions, timing, embed rendering in a live channel.

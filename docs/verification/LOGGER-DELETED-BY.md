# Logger: "Deleted By" on Message Deleted (A23)

## Rule
If Discord's audit log has a matching entry for the deletion, show the executor (`<@id>`).
If it has none, assume the author deleted it: `User` (human) or `Bot (author)` (bot).
Lookup failed (no View Audit Log permission, HTTP error, no guild id): `Unknown (audit log unavailable)`.
Message not in cache (author unknown): `Unknown (message not cached)`; no channel/time guessing.

## Audit-log limits and credit rule
- Discord does not audit-log self-deletes, hence "no entry => author".
- Repeated deletions by one executor of one user's messages in one channel merge into ONE entry (`count` rises, id/timestamp stay at the first). Each entry has `count` credits; a bounded in-memory map (256 ids, oldest evicted) records credits already given.
- An entry matching action, author and channel has a pending credit when `count` > given. A deletion consumes exactly one credit (only on a returned executor), so N burst deletions credit N times and the N+1th shows `User`.
- Never-seen entry: all `count` credits pending if fresh (<= 8 s by id), else baselined as already given (a later count rise is detected). Only matching entries are written to the map.
- Two matching entries: most pending credits, then freshest; only that entry is credited.
- Lookup is up to 3 tries (0, 0.5, 1.5 s), in a spawned task; a failed or credit-less try consumes nothing.
- Expiry: the map also stores the last observed count. A pending credit is spendable only if the entry is fresh by id, its count rose since last observed, or the entry saw activity (a credit or a rise) within 30 s. A stale surplus is never credited.
- Limits: a genuine self-delete while a burst is still being consumed (within 30 s of the last credit) looks like a pending credit; a moderator deletion after a long quiet gap on an already-stale merged entry falls back to `User`; the map is NOT seeded at startup (restart forgets history: a moderator's deletion of an already-stale merged entry right after restart is missed once, shows `User`); `MessageDeleteBulk` not covered.

## Privacy
The audit call lives in the `MessageDelete` arm of `log_event`, which only runs after the
ticket privacy guard allows the event; the guard is unchanged.

## Verified
Unit tests in `src/deletion_attribution.rs` (pure decision logic, map bounds, label text).
NOT tested: real Discord audit entries, permissions, timing, embed rendering in a live channel.

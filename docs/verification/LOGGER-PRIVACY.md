# SF4 — logger privacy: DM leak and deletion misattribution

Base `13a1764`, branch `wt/skynet-sf4-logger-privacy`. Files: `src/main.rs`, `src/deletion_attribution.rs`.

## CARD-013 — DM events leak into the guild log channel: REPRODUCED, FIXED

The logger had no guild check, so a DM (no guild id) reached the staff channel: `MessageUpdate` mirrored the DM's before/after text, `MessageDelete` mirrored the cached DM body, and `MessageDeleteBulk`/`InviteCreate`/`InviteDelete`/`VoiceStateUpdate` wrote DM metadata. The ticket privacy guard did not help: `privacy::decide` allows `Lookup::NotGuild` when the payload carries no guild id.

Fix: `guild_scope()` classifies an event as `Guild` only when its payload proves a guild id (fail closed); `guarded_log()` is the new production routing seam and runs the log-write sink only for guild events, so `log_event` runs its whole body (cache insert, embed build, log write) behind that guard. Interaction events are exempt: they are not channel-logging paths.

Test proof: driving that real seam with synthetic events whose guild id is absent gives zero sink runs for message create/update/delete, bulk delete, voice state, invite create/delete and reaction; the same events carrying a guild id give exactly one sink run each.

## CARD-011 — deletion misattribution: REPRODUCED, FIXED

Exact sequence: MOD_A bulk-deletes 5 of AUTHOR's messages in CHAN (audit entry id 10, count 5); the bot credits MOD_A and entry 10 keeps 4 surplus credits. 9 s later MOD_B deletes one more of AUTHOR's messages in CHAN (separate entry id 11, count 1). The lookup saw both entries and the tie-break picked the entry with the most surplus credits, so MOD_B's deletion was logged as MOD_A's — a wrong accusation against a moderator.

Fix: an entry whose count ROSE since the last observation (the only per-deletion evidence the audit log gives) outranks entries that merely hold leftover credits; the old tie-break remains the fallback when no entry rose.

## Owner rule (Ava, 2026-11-18): APPLIED

`DeletedBy::User` renders `User (assumed)`. The enum variant and the attribution logic are unchanged, `DeletedBy::Executor` still renders `<@id>`, and the `Author Type` fields in `main.rs` were not touched.

## NOT fixed / limits

- CARD-012 (freshness measured from lookup time, not event time) and CARD-024 (role-change executor ignores the entry's `changes`) reproduce on current main but need `main.rs` changes outside the logger event handlers, which this card's file scope forbids. Not attempted.
- Discord's audit log carries no message id, so when two entries rise in the same lookup the per-deletion order stays ambiguous; `Multiple possible moderators` was not added because it would break the existing two-entry tests and contradict the owner's rule.
- Verified offline only: no live Discord, no audit-log HTTP, no embed rendering.

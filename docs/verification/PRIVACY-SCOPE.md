# Privacy Scope Module

## What
`privacy_scope.rs` provides a pure, offline predicate `may_log(scope, event) -> LogDecision`
that the general logger calls before recording or caching any event. No serenity types,
no I/O, no clock reads, no randomness.

## Rules enforced
1. Excluded channel → Skip(ExcludedChannel)
2. Thread whose parent channel is excluded → Skip(ExcludedParentChannel)
3. Channel inside an excluded category → Skip(ExcludedCategory)
4. Channel moved into/out of excluded category → Skip(MovedInto/OutExcludedCategory)
5. DM (no guild_id) → Allow (separate guard handles DMs)
6. Unknown lookup → Skip(UnknownLookup) — fail closed
7. Empty configuration → Allow (today's behaviour, zero change on wiring)
8. Flash-channel IDs merged into excluded-channel set

## Tests (22 total)
RED (7/10 initial tests failed against stub): excluded_channel, thread_with_excluded_parent,
channel_in_excluded_category, move_into_excluded_category, move_out_of_excluded_category,
unknown_lookup, flash_channel_ids — all returned Allow instead of expected Skip.
Boundary: empty config allows, DM allowed, zero ids harmless, duplicate ids dedup.
Adversarial: excluded channel before category, move with only old/new parent,
normal move still checks parent category, implementation checks channels not categories.

## Config keys (read by wiring card K06)
- `LOG_EXCLUDED_CHANNEL_IDS` — comma-separated u64 list, default empty, safe failure = drop bad entry
- `LOG_EXCLUDED_CATEGORY_IDS` — comma-separated u64 list, default empty, safe failure = drop bad entry
No runtime integration, no commands, no persistence, no config parsing, no `.env` reading,
no `Cargo.toml` changes, no dependency additions.

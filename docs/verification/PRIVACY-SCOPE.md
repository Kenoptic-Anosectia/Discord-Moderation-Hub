# Privacy Scope Module
`privacy_scope.rs` gives `may_log(scope, event) -> LogDecision` — a pure, offline
predicate the general logger calls before recording or caching anything. No serenity
types, no I/O, no clock, no randomness.
## Rules enforced
1. Excluded channel → Skip(ExcludedChannel)
2. Thread whose parent channel is excluded → Skip(ExcludedParentChannel)
3. Channel in an excluded category, or the excluded category itself → Skip(ExcludedCategory)
4. Channel moved into/out of an excluded category → Skip(MovedInto/OutExcludedCategory)
5. DM (no guild_id) → Allow (a separate guard handles DMs)
6. Unknown lookup → Skip(UnknownLookup), fail closed — only when the scope is non-empty
7. Empty configuration → Allow, checked BEFORE any lookup
8. Flash-channel IDs merged into excluded-channel set
## Tests (42 in this module)
RED b13238f: 3 tests failed against 1d63c42 with Skip(UnknownLookup) not Allow —
empty_config_allows_unknown_parent, _none_channel_id, _unknown_category; that commit also
moved unknown_lookup_fails_closed, unknown_channel_id_fails_closed and
unknown_category_fails_closed onto a non-empty scope. RED 6d11c5d: 7 of 10 initial tests
failed against the stub. The round-2 tests (excluded category itself, move tri-state) had
no separate RED commit: they need the new `ParentResolution` move fields, which do not
compile against 1d63c42. Adversarial: u64::MAX ids, zero/duplicate ids, DM with unknown
lookups, resolved move between ordinary categories, unknown old/new move parent.
## Config keys (read by wiring card W1; K06 writes settings::parse_id_list)
`LOG_EXCLUDED_CHANNEL_IDS` and `LOG_EXCLUDED_CATEGORY_IDS` — comma-separated u64 lists,
default empty. A malformed list must fail closed in W1 (log disabled + alert); the bad
entry is never dropped. W1 contract: an unresolvable lookup must be passed as
`ParentResolution::Unknown`, never `None` ("None" = "this channel has no parent").
## Not done
No runtime, commands, persistence, config parsing, `.env` reading, `Cargo.toml` changes,
dependency additions, or production caller.

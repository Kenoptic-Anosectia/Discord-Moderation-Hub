# WIRING-B (W2) - flash, safeword and reaction roles registered; the /safeword shell

`features.rs` now reads three optional features from the environment (`plan_features`, pure) and
builds them (`build_features`); `main.rs` prints the plan lines, lists `/safeword` and lets the new
reaction-role path own reactions. Everything defaults OFF. NOT run against Discord (see Limits).

## Rules enforced (each has a test)
1. All-off parity: no `*_MODE` key set (or off, even with bad keys) = empty plan, no line, no
   command, no `core.sqlite`, no runner, dispatch returns 0, log exclusion exactly as configured.
2. Only a feature's own job kinds, events and command: flash live registers the two flash kinds and
   opens the core database; flash shadow neither; safeword opens the database but registers no
   kind (no runner); reaction roles needs neither. `/safeword` is listed only with safeword on.
3. A feature that is on but cannot run is dropped with a KEY and a stable code, never a value:
   no home guild (`home_guild_missing`, also an unusable `GUILD_ID`), a refused key
   (`unreadable`, `out_of_range`, `missing_id`, `invalid_id_list`, `invalid_flag`), `no_channels`,
   `protected_roles_invalid` (safeword), `restricted_roles_invalid`, `shadow_unsupported`.
4. Reaction roles: while `REACTION_ROLES_MODE` is off the old `ReactionAdd`/`ReactionRemove` arms
   run unchanged; when the feature really started (`Features::is_active`) they return at once
   (`legacy_reaction_roles_active`). The new path only reads the same table (the `reactionrole`
   command and its file are untouched); an ordinary binding grants the same role as the direct
   `handle_reaction_add` (tested with the real `ReactionRolesFeature` and a fake role service).
5. Bot flag fails closed (`reactions.rs` `precheck_event`): `None` = `bot_flag_unknown`, ignored.
   Removals carry no member, so the feature asks the service, and only for a bound reaction.
6. Flash channel ids join the log-exclusion scope when flash is planned (shadow too); never
   unblocks an unusable exclusion list.
7. `/safeword` (`safeword/slash.rs`): `dispatch` maps a call to SW4 `status`/`strike`/`clear` and
   returns SW4's text and ephemeral flag unchanged; staff, guild, mode and confirm are SW4's.
8. Safeword is the home guild only (`safeword_ops` gives none for another guild or 0); restricted
   list, flags and ids are parsed by `SafewordConfig` (one bad entry refuses all); no staff role
   = the feature runs, strike and clear are denied (`staff_role_unset` in the line).

## Tests and RED evidence
57 new unit tests (1,846 unit pass; 1,789 before); the 52 integration tests are unchanged. RED
`304e841`: 27 of 38 then-new tests failed against stubs (11 negative tests pass on a stub: ignore,
other guild, plain hook). RED 2 `0ad9d00`: 9 failed (7 slash, command list, safeword build).
Written with the code, no RED: 4 `safeword/discord.rs` tests (error mapping, store), the alert
helpers (`alert_words`, `AlertThrottle`, a seeded 5,000-step run) and two scope tests.

## Config keys (all new, commented in `.env.example`)
`FLASH_MODE`, `FLASH_CHANNEL_IDS`, `FLASH_PING_ROLE_ID`, `FLASH_DELAY_SECS`,
`FLASH_INCLUDE_THREADS`, `SAFEWORD_MODE`, `SAFEWORD_ROLE_ID`, `_CHANNEL_ID`, `_MESSAGE_ID`,
`_EMOJI_ID`, `_RESTRICTED_ROLE_IDS`, `_DM`, `_DEFER_GRANTS`, `REACTION_ROLES_MODE` (off or live).

## Limits and NOT done
- `setup` glue (plan, services, start, `Data`) is untested: it needs a live Discord context.
  `SerenityReactionServices`, the flash ports and the safeword adapters never ran live.
- Sync safeword calls use `block_in_place`: they need the multi-thread runtime (the default).
- Safeword reconcile is not wired (`members_with_state` says `members_not_supported`); `clear`
  does not remove the reaction (SW4 limit). Reaction roles read the safeword row; an unreadable
  store counts as active, so restricted roles are then not granted.
- Reaction-role alerts are words only (`alert_words`), one per text per 300 s; no alert for a
  skipped reaction. Alerts can lag a failing role by up to that window.
- `production_features()` stays empty (the FL2 test pins it); `build_features` adds the rest.
- Deviation: `handle_reaction_add/remove` now delegate to `evaluate_add/remove` after the lookup
  (a `rusqlite::Connection` held across `.await` is not `Send`). `Data`/`DataParts` gained
  `wired`/`flash_log_excluded`, so the test literals in `main.rs` and `flash/runtime.rs` changed.
- `allow(dead_code)` stays in `flash`, `role_menus` and `safeword` `mod.rs`: unused are flash
  policy/runtime helpers (`is_flash_channel`, `shadow_summary`...), the whole role-menu command,
  repository and policy, `reactions` binding commands, and SW3/SW4 reconcile and shadow summaries.
  No new `#[allow]`. Ticket opening is untouched.

## Mutation checks (each made at least one test fail; run with `timeout 300 cargo test`)
18 of 18 caught: owned switch always on, inverted (2); mode check inverted (17 failed); flash ids
dropped in `build_data`, in `plan`, in `from_config_with_flash` (3); unknown flag read as human,
no re-check after the flag lookup (2); `reaction_roles_owned` false; shadow accepted; `/safeword`
always listed; ephemeral flag dropped; other guild gets safeword ops; flash without home guild;
throttle window edge; alert keeps digits; unreadable store reads idle; `is_active` always true.
Survives, and cannot be tested offline: removing the `legacy_reaction_roles_active` call from the
two arms in `log_event_body` (needs a live `serenity::Context`); the switch itself is tested.

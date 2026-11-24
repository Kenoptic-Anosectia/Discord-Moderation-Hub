# WIRING-B (W2) - flash, safeword and reaction roles registered; the /safeword shell

`features.rs` reads three optional features from the environment (`plan_features`, pure) and
builds them (`build_features`); `main.rs` prints plan lines, lists `/safeword` and lets the new
reaction path own reactions. All default OFF; never run against Discord (see Limits).

## Rules enforced (each has a test)
1. All-off parity: no `*_MODE` key set (or off, even with bad keys) = empty plan, no line, no
   command, no `core.sqlite`, no runner, dispatch returns 0, log exclusion exactly as configured.
2. Only a feature's own job kinds, events and command: flash live registers the two flash kinds and
   opens the core database; flash shadow neither; safeword opens the database but registers no
   kind (no runner); reaction roles needs neither. `/safeword` is listed only with safeword on.
3. A feature that is on but cannot run is dropped with a KEY and a stable code, never a value:
   no home guild (`home_guild_missing`), a refused key (`unreadable`, `out_of_range`, `missing_id`,
   `invalid_id_list`, `invalid_flag`), `no_channels`, `protected_roles_invalid`,
   `restricted_roles_invalid`, `shadow_unsupported`, `safeword_unavailable` (see 4).
4. Reaction roles: with `REACTION_ROLES_MODE` off the old `ReactionAdd`/`ReactionRemove` arms run
   unchanged; once the feature really started (`Features::is_active`) they return at once
   (`legacy_reaction_roles_active`). The new path reads the same table (the `reactionrole` command
   and file are untouched); an ordinary binding grants what `handle_reaction_add` does (real
   `ReactionRolesFeature`, fake role service). Fail closed: `SAFEWORD_MODE` on but the safeword not
   started (any refusal in 3) refuses reaction roles too (`safeword_unavailable`); old arms stay.
5. Bot flag fails closed (`reactions.rs` `precheck_event`): `None` = `bot_flag_unknown`, ignored.
   Removals carry no member, so the feature asks the service, and only for a bound reaction.
6. Flash channel ids join the log-exclusion scope only when flash is planned (shadow too).
7. `/safeword` (`safeword/slash.rs`): `dispatch` maps a call to SW4 `status`/`strike`/`clear` and
   returns SW4's text and ephemeral flag unchanged; staff, guild, mode and confirm are SW4's.
8. Safeword is the home guild only (`safeword_ops`: none for another guild or 0); lists, flags and
   ids are parsed by `SafewordConfig` (one bad entry refuses all); no staff role = the feature
   runs, strike and clear are denied (`staff_role_unset` in the line).
9. Start-up decisions are pure functions that `main`/`setup` call and nothing else:
   `features::wire` (owned needs planned AND running; flash ids only when planned), `app_wiring`
   and `framework_commands` (`main.rs`). Tested from an empty environment and when dropped.

## Tests and RED evidence
Round 1: 57 new unit tests; RED `304e841` (27 of 38 failed), `0ad9d00` (9 failed); no RED for 4
`safeword/discord.rs` tests, the alert helpers, two scope tests. Round 2 (review findings): 12 new
tests (1,858 unit pass); RED `0e2334b`: 8 failed against stubs (empty-environment ones pass).

## Config keys (all new, commented in `.env.example`)
`FLASH_MODE`, `_CHANNEL_IDS`, `_PING_ROLE_ID`, `_DELAY_SECS`, `_INCLUDE_THREADS`; `SAFEWORD_MODE`,
`_ROLE_ID`, `_CHANNEL_ID`, `_MESSAGE_ID`, `_EMOJI_ID`, `_RESTRICTED_ROLE_IDS`, `_DM`,
`_DEFER_GRANTS`; `REACTION_ROLES_MODE` (off or live).

## Limits and NOT done
- Lines in `setup`/`main` that call the pure functions are untested (live context): e.g. `setup`
  passing an always-true `is_active` would survive. Flash ports, `SerenityReactionServices` and the
  safeword adapters never ran live.
- `REACTION_ROLES_MODE=live` ends reaction roles outside the home guild (the old arms served all
  guilds) and acts in ticket channels (`dispatch` runs before the ticket privacy guard the old
  arms sit behind). Both need a decision at cutover. With `SAFEWORD_MODE` off the restricted list
  is empty, so stored Active rows deny nothing. A removal whose bot flag cannot be read
  (`get_member` fails) is dropped and the role stays.
- Safeword sync calls use `block_in_place` (default multi-thread runtime). Safeword reconcile is
  not wired; `clear` does not remove the reaction (SW4 limit). An unreadable safeword store counts
  as active, so restricted roles are then not granted. Reaction-role alerts are words only, one
  per text per 300 s; none for a skipped reaction.
- `production_features()` stays empty (the FL2 test pins it). `handle_reaction_add/remove` split
  into `evaluate_add/remove` after the lookup (a `Connection` held across `.await` is not `Send`).
- `allow(dead_code)` stays in `flash`, `role_menus` and `safeword` `mod.rs`: unused are flash
  policy helpers, the role-menu command, repository and policy, `reactions` binding commands, and
  SW3/SW4 reconcile and shadow summaries. No new `#[allow]`. Ticket opening is untouched.

## Mutation checks (each made a test fail; `timeout 300 cargo test`)
Round 1, 18 of 18 (owned switch, mode check, flash ids x3, bot flag x2, shadow, `/safeword` listing,
ephemeral, guild scope x2, throttle, alert digits, store idle, `is_active`).
Round 2, 14 of 14 on the composition: `wire`/`app_wiring` owned always true or false, `is_active`
ignored (reaction, safeword), flash ids emptied, runtime unfiltered, `/safeword` always/never
listed, `safeword_unavailable` check off, inverted, or without the mode test. Survives (needs a
live `Context`): the `legacy_reaction_roles_active` calls in the arms.

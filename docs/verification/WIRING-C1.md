# WIRING-C1 (W3a) - welcome registered; startup preflight gates flash, safeword and welcome

`plan_features` reads welcome from the environment (pure). `preflight_wiring::startup_preflight`
gathers live permission facts and `features::gate_plan` removes every planned feature the
preflight refuses. `features::start_gated` is the one function `setup` calls: preflight, then
build ONLY from the gated plan (flash, safeword, reaction roles, welcome). All default OFF.

## Rules enforced (each has a test)
1. All-off parity: no `*_MODE` key (or modes off, even with bad keys) = empty plan, no line, no
   preflight call or line (fake sources count zero calls), no `core.sqlite`, no runner, no feature
   built, command list unchanged, `dispatch` returns 0.
2. Welcome on registers only `welcome_grant_roles` and `welcome_send_message` (both AtMostOnce via
   `runtime::restart_policy`), opens the core database, and acts only on a member join of the home
   guild. With the core database unusable it is dropped and does not start.
3. A welcome that cannot run is dropped with a KEY or stable code, never a value:
   `home_guild_missing`, `protected_roles_invalid`, `<KEY>: invalid_setting` (also the two link
   keys), `no_channel`, `no_template`, `template_unreadable`, the template's own code, or
   `safeword_unavailable` / `JOIN_ROLE_IDS: safeword_restricted` (rule 6).
4. `gate_plan` (pure): a planned flash, safeword or welcome whose verdict is not `ready` is not
   started; the others still start. A failed lookup is `permissions_unknown`. `validate_join_roles`
   refuses `privileged_permission`, `protected_role`, `managed_role`, `everyone_role` (the preflight
   names missing, unknown or above-bot roles) with `JOIN_ROLE_IDS`. A dropped safeword takes
   reaction roles with it (`safeword_unavailable`). `start_gated` is tested end to end: a refused
   feature has no feature, no runtime and no registration; the rest start.
5. A bot account joining grants and posts nothing (real `WelcomeFeature` through `dispatch`), also
   in shadow mode. A join in another guild is ignored by the guild filter AND by `WelcomeJobs`.
6. Safeword (K03 rule 6): while `SAFEWORD_MODE` is requested, a join role that is the safeword role
   or in `SAFEWORD_RESTRICTED_ROLE_IDS` keeps welcome off, and welcome stays off while the safeword
   is requested but not running. Safeword state survives a leave, so this is decided at startup.
   The safeword preflight request names its role AND every restricted role the swap writes.
7. Flood control: `FloodControlledAlerter` reuses the W2 `AlertThrottle`: one alert per stable code
   per 300 s across joins (fake clock; seeded 600-alert sequence). Lower-case words only.
8. No id, path or value in any preflight or startup line (sentinel test). Template links
   `WELCOME_CHANNEL_LINKS` / `WELCOME_ROLE_LINKS` (`name=id,...`, max 25 each) load through
   `welcome::config::parse_id_map`: any bad entry refuses the list; a template naming a link the
   map lacks is refused at start; with links a join renders the linked channel.

## Tests and RED evidence
76 new unit tests; 2,140 unit + 52 integration pass. RED `f0e203d` (round 1): 40 tests failed
against stubs. Round-2 RED `ed29802`: 15 of 20 new tests failed (the gate-composition test fails
because the stub builds from the ungated plan: the mutant qa-3 found). One existing assertion
changed on purpose: the safeword request now lists the restricted role too.
Gates exit 0: fmt, check, test, clippy `-D`; `Cargo.lock` untouched.

## Config keys (all in `.env.example`, commented, default OFF)
`WELCOME_MODE`, `WELCOME_CHANNEL_ID`, `WELCOME_TEMPLATE_PATH` (name; `welcome_templates/<name>.json`,
at most 64 KiB), `JOIN_ROLE_IDS`, `WELCOME_CHANNEL_LINKS`, `WELCOME_ROLE_LINKS`. Needs `GUILD_ID`.

## Limits and NOT done
- `SerenityFacts`, `serenity_ports`, the sender, alert post and guild lookup never ran live; the
  few `setup` lines left (building the fact source and closures) are untested. A failed guild-level
  lookup reads as `missing_manage_roles`, not `permissions_unknown`. Guild facts are cache only.
- Link ids are not preflighted (a deleted linked channel renders as a dead mention).
- A safeword dropped by the preflight leaves welcome on: its join roles exclude restricted roles.
- Job kinds are registered but joins are inline; a job that exists fails `welcome_job_not_scheduled`.
- `/safeword` stays listed when the preflight drops safeword; a dropped flash keeps its log
  exclusion (wire uses the original plan). Reaction roles are not preflighted.
- `production_features()` stays empty (FL2 pins it); a WE2 comment says W3 adds welcome there; I
  followed WIRING-B.md. The template is read once at start.
- `allow(dead_code)` stays in `welcome/mod.rs` (unused: `IdMap::extend_*` and accessors,
  `welcome_policy`, `WelcomeConfig::is_enabled`, ...) and `preflight.rs` (`RoleFactsEntry` helpers,
  `Report::status`, ...); removing either fails clippy. No new `#[allow]`.

## Mutation checks (each made a test fail; `cargo test`)
Round 1 (12): gate ignores verdict, bot rule off, mode unchecked, empty plan asks Discord, wrapper
guild guard, throttle, join-role refusal, reaction cascade, `needs_core_db`, failed lookup as pass,
zero guild, RunAgain. Round 2 (10): composition builds from the ungated plan (2 fail), safeword
guard off, unavailable branch off, role itself unguarded, restricted roles left out of the request
(3), empty id map at build, plan drops the map, link cap off by one, sign accepted, bad list ignored.

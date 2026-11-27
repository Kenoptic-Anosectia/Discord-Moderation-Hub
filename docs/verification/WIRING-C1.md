# WIRING-C1 (W3a) - welcome registered; startup preflight gates flash, safeword and welcome

`plan_features` reads welcome from the environment (pure); `build_welcome` builds it;
`preflight_wiring::startup_preflight` gathers live permission facts and `features::gate_plan`
removes every planned feature the preflight refuses. All default OFF; never run against Discord.

## Rules enforced (each has a test)
1. All-off parity: no `*_MODE` key (or modes off, even with bad keys) = empty plan, no line, no
   preflight call or line (fake sources count zero calls), no `core.sqlite`, no runner,
   command list, `dispatch` returns 0.
2. Welcome on registers only `welcome_grant_roles` and `welcome_send_message` (both AtMostOnce via
   `runtime::restart_policy`), opens the core database, and acts only on a member join of the home
   guild. With the core database unusable it is dropped and does not start.
3. A welcome that is on but cannot run is dropped with a KEY or stable code, never a value:
   `home_guild_missing`, `protected_roles_invalid`, `JOIN_ROLE_IDS`/`WELCOME_CHANNEL_ID`/
   `WELCOME_TEMPLATE_PATH: invalid_setting`, `no_channel`, `no_template`, `template_unreadable`,
   or the template's own code (`malformed_json`, `unknown_field`, ...).
4. `gate_plan` (pure): a planned flash, safeword or welcome whose preflight verdict is not `ready`
   is not started; the others still start. A failed channel lookup is `permissions_unknown`.
   Welcome with a refused join role (`validate_join_roles`: `privileged_permission`,
   `protected_role`, `managed_role`, `everyone_role`; the preflight itself names missing, unknown
   or above-bot roles and an unknown bot top) is dropped with `JOIN_ROLE_IDS` and the code. A
   safeword that is dropped takes reaction roles with it (`safeword_unavailable`, old arms stay).
5. A bot account joining grants and posts nothing, tested through the real `WelcomeFeature` and
   `Features::dispatch`; so does shadow mode. A join in another guild is ignored by the guild
   filter AND by the wrapper (`WelcomeJobs`), tested with each layer alone.
6. Flood control: `FloodControlledAlerter` reuses the W2 `AlertThrottle`: one alert per stable
   code per 300 s across joins (fake clock: 299 s held, 300 s through, backwards clock through,
   seeded 600-alert sequence: never above the rate, never quiet once the window passed). The text
   is lower-case words only.
7. No id, path or value in any preflight or startup line (sentinel test: no run of 5 digits, no
   template name, no path). The message mentions only the new member (`message_payload`).

## Tests and RED evidence
56 new unit tests (features 32, `welcome/discord.rs` 17, `preflight_wiring.rs` 6, `main.rs` 1);
2,120 unit + 52 integration pass (2,064 existing unit; the card's 1,789 is out of date).
RED `f0e203d`, tests against stubs: 39 of the first 55 failed (plan, gate, build, hook, flood,
payload, template read); the other 16 pass against stubs by design (all-off parity, zero-id
guards). The 56th (real-runner crash) was added after and has no RED. Gates exit 0: fmt, check,
test, clippy `-D`; `Cargo.lock` untouched.

## Config keys (all in `.env.example`, commented, default OFF)
`WELCOME_MODE` (off, shadow, live), `WELCOME_CHANNEL_ID`, `WELCOME_TEMPLATE_PATH` (a name, read
from `welcome_templates/<name>.json`, at most 64 KiB), `JOIN_ROLE_IDS`. Needs `GUILD_ID`.
## Limits and NOT done
- `SerenityFacts`, `serenity_ports`, the message sender, alert post and guild lookup never ran
  live; `setup` lines are untested (a live `Context`). A guild-level lookup that fails reads as
  no permission, so it is reported `missing_manage_roles`, not `permissions_unknown`.
- Guild facts come from the cache only; uncached = empty server name and member count 0.
  The template id map is empty, so a `{channel:KEY}`/`{role:KEY}` template is refused at start.
  The template is read once at start. Join roles are checked with safeword state Inactive.
- The job kinds are registered, but a join is handled inline and nothing schedules them; a job
  that exists fails terminally (`welcome_job_not_scheduled`) and claims no effect.
- `/safeword` stays listed when safeword is dropped by the preflight (the list is built before
  Ready); `wire` keeps it inactive. A dropped flash keeps its channels out of the log (wire is
  decided from the original plan). Reaction roles are not preflighted. Sync calls use
  `block_in_place` (multi-thread runtime). The member intent must be on in the Discord portal.
- `production_features()` stays empty (the FL2 test pins it). A WE2 comment says W3 adds welcome
  there; I followed WIRING-B.md (configurable features are built from the plan) instead.
- `allow(dead_code)` stays in `welcome/mod.rs` (unused: `WelcomeConfig::is_enabled`, `IdMap`
  inserts and accessors, render `SkipReason`, `welcome_policy`, `MAX_ID_DIGITS`,
  `JoinOutcome::failure_codes`, `RenderedWelcome` accessors) and in `preflight.rs` (unused:
  `RoleFactsEntry::found/missing/unavailable`, `Report::status`/`effective_mode`,
  `FeatureReport::effective_mode`, `mode_rank`). Removing either fails clippy; no new `#[allow]`.

## Mutation checks (each made a test fail; `timeout 300 cargo test`)
12 of 12: gate ignores the verdict (7 fail), bot rule off (5), welcome planned with the mode
unchecked (6), empty plan still asks Discord (2), wrapper guild guard off (2), throttle off (4),
join-role refusal ignored (1), reaction cascade off (1), `needs_core_db` false (5), failed channel
lookup read as a pass (3), zero guild asked (1), RunAgain instead of AtMostOnce (1).

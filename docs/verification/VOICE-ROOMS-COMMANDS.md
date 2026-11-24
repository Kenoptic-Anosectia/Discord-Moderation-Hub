# VOICE-ROOMS-COMMANDS - `/vc` command logic (VR3)
Card VR3: `src/voice_rooms/command.rs` (new), `pub mod command;` in `mod.rs`, and the owner/lock/limit
updates in `repository.rs` (`update_owner`, `update_locked`, `update_user_limit`; stubs in the RED
commit, filled in later). Logic only: no poise command, not called from `main.rs`, no feature
switched on, nothing registered with Discord, never run against Discord.
- **API.** `VcCommandHandler::new(VcCommandDeps)` and `handle(&VcCommandRequest) -> CommandOutcome`
  (`Ignored(code)`, `Applied{code,reply}`, `Refused{code,reply}`). Ports: `RoomCommandPorts` (rename,
  limit, overwrite set/delete, disconnect, move, room members, category overwrites, member profile),
  `SafewordLookup`, `CommandAlerter`, `Clock`, plus `RoleSource` from the role service.
  `SerenityRoomCommandPorts` is the real adapter (compiles; its HTTP calls were never exercised).
  `note_owner_absent` / `note_owner_present` are for the wiring card's voice-state hook.
- **Gate.** Mode off: `Ignored`. Mode on and home guild unset: `HomeGuildUnset`. Other guild or DM:
  `Ignored` before anything is read, written or sent. Channel without a row in the home guild:
  `UntrackedChannel`. Commands run one at a time (async mutex), the row is re-read inside it.
- **Owner.** Owner commands need `actor == row.owner` (the row, not channel permissions). `claim` is
  the only non-owner command: caller in the room, owner outside it, and a recorded absence
  (`note_owner_absent`) at least `claim_grace_secs` old (60 s). A claim never starts or guesses an
  absence: none recorded -> `vc_owner_absence_not_recorded`. `note_owner_present`, or a claim that
  finds the owner inside, clears the record. A return nobody reports is not seen (see NOT done).
- **Grace conflict (Directorate ruling).** Two modules on main define the same idea with different
  keys and defaults: VR1 policy `VOICE_ROOM_EMPTY_GRACE_SECS`=60, VR2 config `VC_EMPTY_GRACE_SECS`=10.
  VR3 claims follow VR1 (waiting longer is the safer failure); VR2's cleanup grace is untouched.
  Reconciling the two keys is a separate decision before voice rooms are wired.
- **permit.** Refuses owner, the bot, bot accounts, protected/restricted/privileged-permission roles
  (a role missing from the catalog is unjudgeable, so refused), active safeword, no category view,
  and a member already permitted since this process started. **reject** refuses owner, bot, bot
  accounts, protected and privileged roles; restricted-role members CAN be rejected (a deliberate,
  accepted choice: removing someone from your own room is safe). It denies `CONNECT`, then
  disconnects the member if `room_members` shows them inside.
- **transfer / claim.** New owner overwrite first, old owner overwrite removed second, row last. A
  failed step leaves the row as it was; a failed removal tries to undo the new overwrite.
- **rename.** VR1 cleaner, budget via `policy::decide_rename` (per room), then Discord. 429 ->
  `vc_rename_rate_limited`, budget used up -> `rename_budget_exhausted`; neither changes anything
  and failed attempts use no budget. **limit** 0..=99, else `vc_limit_out_of_bounds` before any
  call. **lock / unlock** set `@everyone` `CONNECT` denied over the category's own overwrite; unlock
  restores that baseline (or deletes the room overwrite when the category has none).
- **Row vs Discord.** Discord write first, row second. Discord failure: row unchanged, stable code.
  Row failure after Discord changed: `vc_row_update_failed` plus an alert (`vc_cmd_owner_row_drift`,
  `vc_cmd_lock_row_drift`, `vc_cmd_limit_row_drift`); a failed rollback alerts
  `vc_cmd_owner_overwrite_drift`. Replies are ephemeral fixed text with a `[code]`, no names or ids.
  Shadow mode runs checks and reads, writes nothing, answers `vc_shadow_noop`.
## Tests (25 command unit + 1 repository unit; fakes plus real in-memory SQLite)
Non-owner on all seven owner commands; claim (present owner, outsider, 59 s refused / 60 s accepted,
no recorded absence refused at any age, the 5000/5030/5100/5110 unobserved-return sequence refused,
observed return restarts the grace, a claim that sees the owner clears the record, racing claims);
transfer, permit and reject refusals; rename 429 and budget; limit bounds; lock/unlock baseline;
failed Discord and row writes; shadow, off, DM, other guild, two seeded runs (80 steps with a floor
of 15 applied commands; 3 seeds x 300 steps with a row-vs-outcome model check).
**RED** round 1 `f6b8a54`: 11 failed / 200 passed in `voice_rooms`. Round 2 `93b38f2`: 6 failed /
19 passed in `voice_rooms::command`, before the continuous-absence and 60 s grace fix.
**Mutants** (round 1, each failed a test): owner check off, grace `<=`, present-owner claim,
transfer target outside, no drift alert, lock without deny, no budget, shadow ignored in limit, no
disconnect, other-guild row, limit bound +1, safeword skipped, 429 counted, unlock ignoring baseline.
## Config keys (read by the wiring card, not here)
`VC_MODE` off|shadow|live (default off), `VC_CATEGORY_ID` (unset = not ready, refused), `GUILD_ID`
(home guild; unset with mode on = refused). Claim grace: wiring passes `VOICE_ROOM_EMPTY_GRACE_SECS`
(default 60, `policy::DEFAULT_EMPTY_GRACE_SECS`) as `VcCommandDeps.claim_grace_secs`. Also passed:
bot id, role-policy sets, rename budget (VR1 default 2 per 600 s), default name.
## NOT done / known limits
No poise shell (W4), not wired, `main.rs`/`config.rs`/`.env.example` untouched; enabling needs D10.
Permits, rename times and owner-absence records live in memory: a restart forgets them (a permit can
be repeated; after a restart claims are refused until an absence is recorded again). Nothing calls
`note_owner_absent`/`note_owner_present` yet. If wiring misses an owner return, the stale absence
stays and a later claim can pass the grace although the owner came back meanwhile; only reported
returns reset it. serenity may wait on its own rate limiter instead of returning 429. A member who
leaves between `room_members` and the disconnect is not re-checked. `restricted` roles are the
caller's `RoleLimits`; the role service's own deny reasons are not re-run.

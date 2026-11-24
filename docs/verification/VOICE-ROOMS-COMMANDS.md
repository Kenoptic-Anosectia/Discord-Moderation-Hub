# VOICE-ROOMS-COMMANDS - `/vc` command logic (VR3, amended by VR4)
Card VR3: `src/voice_rooms/command.rs` (new), `pub mod command;` in `mod.rs`, and owner/lock/limit
updates in `repository.rs`. Logic only: no poise command, not called from `main.rs`, no feature on,
nothing registered with or run on Discord. VR4 adds deputies, `refresh`, `reset_observations`
(`VOICE-ROOMS-DEPUTIES.md`); where the two docs differ, that one wins.
- **API.** `VcCommandHandler::new(VcCommandDeps)` and `handle(&VcCommandRequest) -> CommandOutcome`
  (`Ignored(code)`, `Applied{code,reply}`, `Refused{code,reply}`). Ports: `RoomCommandPorts`
  (rename, limit, overwrite set/delete, disconnect, move, room members, category overwrites, member
  profile, member overwrites), `SafewordLookup`, `CommandAlerter`, `Clock`, plus `RoleSource`.
  `SerenityRoomCommandPorts` is the real adapter (compiles; its HTTP calls were never exercised).
- **Gate.** Mode off: `Ignored`. Mode on and home guild unset: `HomeGuildUnset`. Other guild or DM:
  `Ignored` before anything is read, written or sent. Channel without a row in the home guild:
  `UntrackedChannel`. Commands run one at a time (async mutex), the row is re-read inside it.
- **Owner.** Owner commands need `actor == row.owner` (the row, not channel permissions); since VR4
  a `voice_room_deputies` row also qualifies, except for `transfer` and `deputize`. `claim` is the
  only command open to anyone else: caller in the room, owner outside it, and a recorded absence
  (`note_owner_absent`) at least `claim_grace_secs` old (60 s). A claim never starts or guesses an
  absence: none recorded -> `vc_owner_absence_not_recorded`. `note_owner_present`, or a claim that
  finds the owner inside, clears the record. The record must be gapless: observations more than
  `ABSENCE_MAX_GAP_SECS` (15 s) apart restart it, and a claim needs a last observation at most that
  old, else `vc_owner_absence_not_continuous`.
- **Grace conflict (Directorate ruling).** VR1 policy `VOICE_ROOM_EMPTY_GRACE_SECS`=60 and VR2
  config `VC_EMPTY_GRACE_SECS`=10 differ. VR3 claims follow VR1 (waiting longer is safer); VR2's
  cleanup grace is untouched. Reconciling the two keys is a separate decision before wiring.
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
  `vc_cmd_owner_overwrite_drift`. Replies are ephemeral fixed text with a `[code]`, no names or ids
  (VR4 refresh replies add counts only). Shadow mode runs checks and reads, writes nothing, answers
  `vc_shadow_noop` (`refresh`: `vc_shadow_refresh` with the counts it would change).
## Tests (VR3: 27 command unit + 1 repository unit; fakes plus real in-memory SQLite)
Non-owner on all seven owner commands; claim (present owner, outsider, 59 s refused / 60 s accepted,
no recorded absence, the 5000/5030/5100/5110 probe, 15 s heartbeat, gap and return restart the
grace, racing claims); refusals; rename 429 and budget; limit bounds; lock/unlock; failed writes;
shadow, off, DM, other guild. **RED** (VR3) `f6b8a54`: 11 failed / 200 passed (14 of 14 mutants
failed a test; not re-run later); `93b38f2`: 6 failed / 19 passed; `3d64491`: 3 failed / 24 passed.
## Config keys (read by the wiring card, not here)
`VC_MODE` off|shadow|live (default off), `VC_CATEGORY_ID` (unset = not ready, refused), `GUILD_ID`
(home guild; unset with mode on = refused). Claim grace: wiring passes `VOICE_ROOM_EMPTY_GRACE_SECS`
(default 60, `policy::DEFAULT_EMPTY_GRACE_SECS`) as `VcCommandDeps.claim_grace_secs`. Also passed:
bot id, role-policy sets, rename budget (VR1 default 2 per 600 s), default name.
## WIRING CONTRACT (hard; `/vc claim` depends on it)
The wiring card MUST do all three, and `/vc claim` MUST stay unregistered until it does:
1. `note_owner_present` on every owner voice join or leave of a temp room;
2. `note_owner_absent` at least every 15 s while the owner is out;
3. `reset_observations` when the gateway reconnects WITHOUT a clean resume (events may be lost; the
   cleared record means no claim passes until a fresh 60 s of observed absence).
A return plus a second exit inside one 15 s gap looks like unbroken absence unless (1) reports the
return; only (1) closes that window and (3) closes the lost-event case. A missing heartbeat or a
reset fails closed (claims refused); a missing presence report does not.
## NOT done / known limits
No poise shell (W4), not wired, `main.rs`/`config.rs`/`.env.example` untouched; enabling needs D10.
Permits, rename times and owner-absence records live in memory: a restart forgets them (a permit
can be repeated; after a restart claims are refused until an absence is recorded again). Nothing
calls the three contract methods yet. serenity may wait on its own rate limiter instead of
returning 429. A member leaving between `room_members` and the disconnect is not re-checked.
`restricted` roles come from the caller's `RoleLimits`; role deny reasons not re-run.

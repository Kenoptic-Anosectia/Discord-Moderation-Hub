# VOICE-ROOMS-COMMANDS - `/vc` command logic (VR3)
Card VR3: `src/voice_rooms/command.rs` (new), `pub mod command;` in `mod.rs`, and the owner/lock/limit
updates in `repository.rs` (`update_owner`, `update_locked`, `update_user_limit` already existed as
stubs from the RED commit; filled in here). Logic only: no poise command, not called from `main.rs`,
no feature switched on, nothing registered with Discord, never run against Discord.
- **API.** `VcCommandHandler::new(VcCommandDeps)` and `handle(&VcCommandRequest) -> CommandOutcome`
  (`Ignored(code)`, `Applied{code,reply}`, `Refused{code,reply}`). Ports: `RoomCommandPorts` (rename,
  limit, overwrite set/delete, disconnect, move, room members, category overwrites, member profile),
  `SafewordLookup`, `CommandAlerter`, `Clock`, plus `RoleSource` from the role service.
  `SerenityRoomCommandPorts` is the real adapter (compiles; its HTTP calls were never exercised).
  `note_owner_absent` / `note_owner_present` are for the wiring card's voice-state hook.
- **Gate.** Mode off: `Ignored`. Mode on and home guild unset: `HomeGuildUnset`. Other guild or DM:
  `Ignored` before anything is read, written or sent. Channel without a row in the home guild:
  `UntrackedChannel`. Commands run one at a time (async mutex), the row is re-read inside it.
- **Owner.** Owner commands need `actor == row.owner` (the row, never channel permissions).
  `claim` is the only non-owner command: caller must be in the room, the owner must be outside it, and
  the owner's absence must be at least `VC_EMPTY_GRACE_SECS` old. An absence nobody recorded starts its
  grace at the first claim, so a claim is never granted on a guess. Seeing the owner present resets it.
- **permit.** Refuses owner, the bot, bot accounts, protected/restricted/privileged-permission roles
  (a role missing from the catalog counts as unjudgeable and is refused), active safeword, no category
  view, and a member already permitted since this process started. **reject** refuses owner, bot,
  bot accounts, protected and privileged roles (restricted-role members CAN be rejected); it denies
  `CONNECT` for the member, then disconnects them if `room_members` shows them inside.
- **transfer / claim.** New owner overwrite first, old owner overwrite removed second, row last. A
  failed step leaves the row as it was; a failed removal tries to undo the new overwrite.
- **rename.** Name through the VR1 cleaner, budget through `policy::decide_rename` (per room, window
  from `VcCommandDeps.rename_budget`), then Discord. 429 -> `vc_rename_rate_limited`, budget used up ->
  `rename_budget_exhausted`; neither changes anything and failed attempts use no budget.
- **limit** 0..=99, else `vc_limit_out_of_bounds` before any call. **lock / unlock** set `@everyone`
  with `CONNECT` denied on top of the category's own `@everyone` overwrite; unlock restores that
  baseline (or deletes the room overwrite when the category has none).
- **Row vs Discord.** Discord write first, row second. Discord failure: row unchanged, stable code.
  Row write failure after Discord changed: `vc_row_update_failed` plus an alert
  (`vc_cmd_owner_row_drift`, `vc_cmd_lock_row_drift`, `vc_cmd_limit_row_drift`); a failed rollback
  alerts `vc_cmd_owner_overwrite_drift`.
- **Replies** are ephemeral fixed text with a `[code]`; they never echo a name or id. Shadow mode runs
  the checks and reads, writes nothing and answers `vc_shadow_noop`.
## Tests (21 command unit + 1 repository unit)
Fakes for ports, alerter and clock; real in-memory SQLite. Non-owner on all seven owner commands, claim
(present owner, outsider, 59 s refused / 60 s accepted, unrecorded absence, return resets, two racing
claims), transfer inside/outside, permit refusals (owner, bot, bot account, staff, privileged,
restricted,
unknown role, safeword, no category view, duplicate), reject (disconnect, failed disconnect, absent
member, permit cleared), rename (cleaned name, 429, budget at 599/600 s, other failures use no budget),
limit bounds, lock/unlock baseline, failed Discord write, failed row write (trigger), shadow, off, DM,
other guild for every subcommand, row of another guild, and two seeded runs (an 80-step run and 3 seeds
x 300 steps with random Discord and row failures; the model check asserts the row matches each outcome).
**RED** `f6b8a54` (tests + stubs): 11 failed (10 command, 1 repository) / 200 passed in `voice_rooms`.
**Mutants** (each failed at least one test): owner check off, grace `<=`, present-owner claim, transfer
target outside, no drift alert, lock without deny, no budget, shadow ignored in limit, no disconnect,
other-guild row, limit bound +1, safeword skipped, 429 counted as rename, unlock ignoring baseline.
## Config keys (read by the wiring card, not here)
`VC_MODE` off|shadow|live (default off), `VC_CATEGORY_ID` (unset = not ready, refused),
`VC_EMPTY_GRACE_SECS`
0-3600 (default 10; also the claim grace), `GUILD_ID` (home guild; unset with mode on = refused). Wiring
passes the bot id, role-policy sets, rename budget (VR1 default 2 per 600 s) and default name.
## NOT done / known limits
No poise shell (W4), not wired, `main.rs`/`config.rs`/`.env.example` untouched; enabling needs D10.
Permits, rename times and owner-absence times live in memory: a restart forgets them (a permit can be
repeated; the rename budget and absence grace restart). Nothing feeds `note_owner_absent` yet, so
until wiring does, a claim only works after a refused first attempt has started the grace. serenity may
wait on its own rate limiter instead of returning 429, so a rename can be slow, not refused. A member
who leaves between `room_members` and the disconnect is not re-checked. `restricted` roles are the
caller's `RoleLimits`; the role service's own deny reasons are not re-run.

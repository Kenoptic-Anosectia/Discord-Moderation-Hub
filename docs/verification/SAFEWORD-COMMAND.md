# Safeword staff command logic (SW4)
`src/safeword/command.rs`: `status`, `strike`, `clear` as plain functions over an `Env` (the SW3
`Ops`, a clock, the config, a `ConfirmBook`). State changes go through SW1 `step` and the SW2 store
(`StateStore`); role writes use K03 (`SafewordRestore`) and K04. Not wired, no poise command.
## Rules enforced (each has a test)
- **Gate first, touching nothing:** mode `off` = `disabled`; home guild unset/0 or another guild =
  `denied` (`no_home_guild`, `other_guild`; the same member's row in the other guild is untouched);
  caller 0 = `denied`; staff = holds `STAFF_ROLE_ID` (unset or 0 = nobody). `shadow` = `shadow_mode`
  for `strike`/`clear` (nothing written); `status` still reads.
- **`status <member>`:** staff may look at any member and also see the snapshot role count. Anyone
  else may look only at themself and never sees the count. Shown: idle, activating, active,
  `releasing` (SW1 has a fourth state; the card named three). An unreadable row is `store_failed`.
- **`strike <member> <role>`:** staff only, member must be Active. Only a role in the snapshot is
  accepted; anything else (unheld, safeword role, already struck) changes nothing, and role 0 is
  `invalid_role`. One compare-and-swap write of the machine's own state; a failed or stale write
  changes nothing.
- **`clear <member> [confirm]`:** staff only. First call arms a confirmation and restores nothing.
  The call with `confirm` must come from the same caller, for the same member, at the same stored
  row version, within `CONFIRM_WINDOW_SECS` (60, edge tested). One attempt uses it up. The book
  keeps at most `MAX_ARMED` (32) entries. A member with no safeword is `already_clear` (no write,
  no arm).
- **Release order:** restore first (K03 `SafewordRestore` decision per role, K04 write, 429 retry),
  then remove the safeword role, clear the row, send the release DM. If the restore fails the
  stored row is untouched (still Active, no DM, no clear), the reply is `restore_failed` with
  stable codes, and a retry re-derives the plan. If a later step fails the machine's own
  Releasing state is stored and `release_incomplete` is returned; held-back steps are reported to
  the machine as failed so the next `clear` re-emits them. A DM that cannot be sent is alerted
  (`dm_failed`) but does not block the clear.
- **Replies:** the only constructor makes them ephemeral; text is a fixed sentence, stable codes
  and counts. Over 60 replies from all paths are checked: ephemeral, and no test id appears in text.
## Tested
38 tests (+38 over the 1,541 unit tests; the 52 integration tests are unchanged): scope, status,
strike, confirm, restore failure, partial progress, idempotence, replies, a 250-seed liveness run on
the real SW2 repository (in-memory SQLite) with random role-write, lookup, DM and store failures,
late or lost confirmations and strikes. It must reach `cleared` within 3 rounds of the failures
stopping and asserts the paths were taken (>= 5 each of restore_failed, release_incomplete,
store_failed, confirm_expired, confirm_mismatch).
RED commit `eb8b271`: 37 tests against a stub, 34 failed (3 passed: mode off, code table, reply
sweep, which a stub satisfies). Mutations tried, each made a test fail (23/23): staff always true,
zero staff role trusted, guild check, mode off, caller 0, shadow, snapshot check, status of others,
count leak, ephemeral flag, confirm skipped/not consumed/version check, window edge (both ends),
book cap, already-clear, restore not first, clear not held back, restore abort, K03 path, final
clear, DM code dropped.
## Config keys (read by the wiring card; no new key)
`SAFEWORD_MODE` and the other `SAFEWORD_*` keys (SW3 doc); `GUILD_ID` becomes `home_guild` (unset or
0 = everything denied); `STAFF_ROLE_ID` becomes `staff_role_id` (unset or 0 = nobody is staff, so
`strike` and `clear` are denied). The window and cap are constants, not settings.
## NOT done / known limits
- No poise command, no registration, no feature switched on, nothing calls this from `main.rs`.
  `Ops` is synchronous: wiring must call it off the async executor and share one `ConfirmBook`.
- `clear` does not remove the member's reaction. If it is still on the safeword message, an SW3
  reconcile (Inactive + reaction present) activates the member again. Needs a follow-up.
- A strike cannot take back a role an earlier partial restore already wrote (tested limit).
- After a failed restore the roles that were written stay on while the row still says Active.
- No crash test: order (restore, remove, clear row, DM) means the row is cleared only after the
  roles are back, but only injected failures were tested. Confirmations are lost on restart.
- Enabling needs decision D06 (owner).

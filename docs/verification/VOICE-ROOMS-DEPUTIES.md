# VOICE-ROOMS-DEPUTIES - deputies, `/vc refresh`, claim hardening (VR4)
Card VR4 extends VR3 (`VOICE-ROOMS-COMMANDS.md`) in `command.rs` and `repository.rs`. Logic only: no
poise command, not called from `main.rs`, nothing registered, no feature on, never run live.
## Rules the code enforces
- **Authority is read from rows at command time.** Owner = the `voice_rooms` row's owner. Deputy = a
  row in `voice_room_deputies(channel, user, added_at)` (migration v2, primary key channel+user).
  Channel permissions are never consulted. A caller who is neither gets `not_room_owner`.
- **Deputies** may `rename limit lock unlock permit reject refresh` under the owner's rules and
  budgets, and `claim` like anyone. They may not `transfer` or `deputize` (`vc_deputy_not_allowed`),
  and `undeputize` only themselves (`vc_deputy_self_only`). A deputy cannot `reject` the owner
  (`vc_target_is_owner`) or another deputy (`vc_target_is_deputy`).
- **`deputize <member>`** (owner only): the target must be in the room, not the owner, the bot or
  a bot account, must pass the `permit` checks (shared code: protected/restricted/privileged role,
  role missing from the catalog, safeword, category view), must not be a deputy, and the room may
  have at most `MAX_DEPUTIES_PER_ROOM` = 5 (`vc_deputy_cap_reached`; reason: it bounds who can
  change the room without the owner). Discord first (the same permit overwrite as `permit`), the
  row second;
  a failed row write is `vc_row_update_failed` plus alert `vc_cmd_deputy_row_drift`, and the permit
  stays on Discord. The cap is also checked inside the single `INSERT` in `add_deputy`.
- **`undeputize [member]`**: omitted target = the caller. The owner may remove any deputy; a
  non-deputy target is `vc_target_not_deputy`. It does not touch the permit overwrite (`refresh`
  does). A failed row write is `vc_row_write_failed` (no Discord change happened, so no alert).
- **Owner `reject` of a deputy** removes the row in the same command; a failed removal still applies
  the ban and disconnect, then answers `vc_row_update_failed` and alerts `vc_cmd_deputy_row_drift`.
- **Rows and rooms.** Two SQLite triggers (same migration): deleting a `voice_rooms` row deletes
  its deputy rows in that statement (a recreated channel id inherits none), and changing the owner
  deletes the new owner's own deputy row. Deputies are kept across `transfer` and `claim`; the old
  owner does not become a deputy, and the new owner may `undeputize` them.
- **`refresh`** (owner or deputy): reads the member overwrites from Discord (`member_overwrites`
  port, never the in-memory map), then makes the member permit overwrites (allow CONNECT, no deny
  CONNECT) exactly the owner, the deputies and the members connected now. It adds missing ones
  first, then deletes the rest. **Ruling (QA round 1): refresh never grants more than `permit`
  would.** Before creating a permit for anyone but the owner it runs the `permit` eligibility check
  (deputies too: their rows are not rechecked). An ineligible member (bot account, protected,
  restricted or privileged role, active safeword, category not visible) gets no permit and is
  counted `skipped`; one that cannot be judged (a failed read) gets none and counts as failed; a
  429 on a read stops all further reads and writes. A skipped member who already holds a permit
  keeps it (refresh does not revoke on eligibility); a skipped deputy stays a deputy. Reject
  overwrites (deny CONNECT), role, `@everyone`, the bot's own and member overwrites that do not
  allow CONNECT are not touched; a connected banned member gets no permit. At most 100 overwrites
  are examined (`MAX_OVERWRITES_PER_CHANNEL`); the rest count as failed. The in-memory permit map
  is rebuilt from the result. Replies carry counts: `vc_refreshed` (removed, added, skipped) or
  `vc_refresh_partial` (plus failed). A failed read of overwrites, members or deputies changes
  nothing. Shadow reads (eligibility too), answers `vc_shadow_refresh`, writes nothing.
- **Claim hardening.** `reset_observations()` clears all recorded absences; the wiring contract in
  `VOICE-ROOMS-COMMANDS.md` lists its three obligations (`/vc claim` stays unregistered until met).
## Tests (28 command + 4 repository, all new; fakes plus real in-memory SQLite)
Each rule above, with restart (new handler, same database), both guilds, off, shadow, a failed
Discord write, row write and read, 429 and partial counts, the 100 cap, and a seeded run (3 seeds x
400 random commands, random Discord and SQLite failures, room rows deleted and recreated).
**RED** `9f43b90` (stubs only): 26 failed / 229 passed in `voice_rooms`. First green `0690822`: 255
passed; the final commit adds two tests (257). 18 mutants (each failed a test, then reverted; one,
a delete that finds nothing counted as failed, first survived and got a test): deputy may transfer
or deputize; deputy may undeputize others; cap `>` for `>=` (command and SQL); refresh without
deputies or without connected members in the keep-set; refresh lifting bans; refresh removing the
bot's overwrite; no delete trigger; no owner trigger; no 100 cap; shadow refresh writing; reset
no-op; no 429 stop (add and delete loop); no map rebuild; deputy may reject a deputy.
Round 2 (eligibility in refresh): **RED** `467e3aa` (check stubbed out): 3 failed / 257 passed;
green `1f39c6a` after fixing my own test setup: 260 passed. 3 mutants, all killed: ineligible
counted failed, a read 429 not stopping the run, safeword not classed as skipped.
## Config keys
None new. Uses `VC_MODE`, `VC_CATEGORY_ID`, `GUILD_ID` as in `VOICE-ROOMS-COMMANDS.md`; both limits
are code constants. Unset or invalid config fails closed as in VR3.
## NOT done / known limits
No command, no wiring, not in `main.rs`, `serenity` `member_overwrites` never run against Discord.
`refresh` never sleeps: a 429 stops the run and counts the rest as failed (the port carries no
retry-after). A permit-class overwrite is deleted whole, even with other bits set. Deputy rows are
not rechecked against later role changes (refresh withholds their permit, nothing else).

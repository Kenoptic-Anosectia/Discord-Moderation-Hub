# Role admin: `/role add | remove | info | list` logic (RA1, `src/role_admin/command.rs`)

## What it is
The testable core of Carl-style single-target role administration, behind ports: role facts
(RG2 `RoleSource`), a `RoleExecutor`, `SafewordLookup`, `AuditSink` and `Clock`. Adds are decided
by K03 (`decide`, staff path) and written by K04 (`grant::apply`). K04 has no removal executor, so
removal is decided by K03 `decide_removal` plus the hierarchy rules here, and written by a retry
loop in this file that reuses K04's `classify` and limits. Replies carry ids and codes only.

## Rules the code enforces (each has a test)
1. Off by default. `Off` refuses all four commands (`feature_off`) before anything is read.
   `Shadow` decides and reads but never writes (`would_add`, `would_remove`); `Live` writes.
2. While on: home guild unset or zero is `home_guild_unset`; staff role unset or zero is
   `staff_role_unset`; another guild or a DM is `foreign_guild` or `no_guild`; a zero id is
   `invalid_id`; an unusable `PROTECTED_ROLE_IDS` or `SAFEWORD_RESTRICTED_ROLE_IDS` (one flag each
   on the env) is `protected_roles_invalid` or `safeword_restricted_invalid`, on all four
   commands. Nothing is read, written or audited in any of these cases.
3. The caller must hold the staff role (`not_staff`), also for `info` and `list`.
4. Caller hierarchy: the caller's top role must be strictly above the role and above the target
   member, for `add` and `remove`. Bot hierarchy: the bot's top role must be strictly above the
   role (`at_or_above_bot`). A bot holding no known role can grant nothing.
5. Managed, @everyone, deleted, privileged (six permission bits) and protected roles are refused
   with stable codes on `add` and `remove`, whatever the caller's rank.
6. Active safeword on the target refuses a restricted role on `add` (`safeword_restricted`). An
   unreadable safeword state counts as active. Safeword state does not block removals.
7. `remove` of a role the member lacks is `not_held` (no write). `add` of a held role is
   `already_held`. Both are checked after authorisation.
8. Every write passes the audit-log reason `role_admin add by <caller id>` (or `remove`), on each
   retry. A 429 is retried at most 3 times; any failure is a `write_failed_<code>` reply plus an
   audit entry. One audit entry per add or remove that passes rule 2, including refusals.
9. Replies: ephemeral flag always true, ASCII, no mention syntax, cut to 1000 characters.
   `list` names at most 20 roles and counts the rest. Lookup failures are `facts_unavailable`.

## Tests (40 in `command.rs`, all behaviour through the public async functions)
Mode parsing; off, unset home guild, unset staff role, foreign guild, DM and zero ids; hierarchy
boundaries (equal and one above and below); six privileged bits; protected, managed, everyone,
deleted; safeword matrix (active, inactive, unknown, caller's own); no-op replies; audit reason
on retries; failure table; clamp of a huge retry hint; shadow mode; info and list; a 5,000-role
member; a 1,260-case sweep (3 modes x 5 callers x 6 members x 14 roles) of all four commands
(reply shape, audit count); a 600-case seeded LCG liveness test against a model of the
retry policy (one reply and one audit entry each, at most 4 calls, exact sleeps, state matches
the reply); a 200-case test that repeating a command against a flaky executor converges;
unusable protected and safeword-restricted config fails closed on all four commands.
RED evidence: commit `60a361e` (stub): 36 of 36 tests failed. Later additions: the `bound()`
test, an @everyone list test and a shadow no-op test. Round 2: commit `2d190af` adds the
`safeword_restricted_invalid` field and its test, which failed (`added` instead of the code) and
then passed once the gate checked the flag.
Mutations tried (each made a test fail): foreign guild allowed, privileged or protected check
dropped, unknown safeword read as inactive, safeword ignored on add, `<=` to `<` on both
removal hierarchy checks, empty or wrong audit reason, retry of non-429, retry limit +1, clamp
removed, no-op check dropped, list cap widened, `bound` dropped, non-ephemeral, staff check
dropped, bot hierarchy dropped on remove, remove using add rules, audit call dropped, off or
staff checks dropped, shadow writing, @everyone allowed or listed, safeword-config gate check
dropped (killed by `unusable_safeword_restricted_config_fails_closed`). One mutation (safeword
input to `decide_removal`) survives because K03 ignores that input; it is equivalent.

## Config keys (read by the wiring card, not here)
- `ROLE_ADMIN_MODE`: `off` (default), `shadow`, `live`; `parse_mode` maps anything else to off.
- Handed in via `RoleAdminEnv` from existing keys: `GUILD_ID`, `STAFF_ROLE_ID`,
  `PROTECTED_ROLE_IDS`, `SAFEWORD_RESTRICTED_ROLE_IDS`. The wiring card must set
  `protected_roles_invalid` or `safeword_restricted_invalid` on a bad entry (commands then refuse,
  rule 2). An unset list is an empty list (no protected or restricted roles); unset = empty here.

## NOT done
No poise command, no registration, no feature switched on, nothing called from `main.rs` (one
`mod role_admin;` line only). W4 must write the real `RoleExecutor`: RG2's `SerenityRoleWriter`
sends no audit reason and there is no removal call, so today the reason reaches only the port.
The guild owner has no special rank (judged by roles; Discord may answer 403). Shadow-mode
meaning is this card's choice. A mistyped mode value is silently off. The audit sink is a staff
record, not the Discord log. Removal retry duplicates K04's rule. Enabling needs decision D05.

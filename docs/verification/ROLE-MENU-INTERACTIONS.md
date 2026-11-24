# Role menu interactions (RM3)

`src/role_menus/interactions.rs`: the testable core behind a member pressing a button or picking
from a select on a role menu RM2 posted. The poise/serenity wiring is the wiring card's job (W4);
this module takes plain values in and out: no serenity type, no I/O, no clock read, no randomness in
the decision path.
## The rules the code enforces (each has a test)
1. **The custom id is parsed strictly.** A button is `rolemenu:v1:m<menu id>:r<role id>` and a
   select is `rolemenu:v1:m<menu id>:s` (`rolemenu:v1:` is the prefix PLAN section 5 names). At most
   100 bytes, ASCII digits only, no leading zero, no zero id; a foreign prefix, a wrong shape or a
   hostile number is refused as a class, and a refused id is ignored before the menu is read.
2. **An option is named by its role id, not by its position.** The role id is the option's identity
   inside a menu (RM2 stores one row per role), so a button keeps naming the same option after
   another option of the menu is removed: a button for a removed option is refused with
   `option_not_found` and never grants the option that took its place.
3. **The stored row is the truth.** The menu row must be stored, belong to the guild the press came
   from and record the message the component is on; the option must still be stored. A deleted menu,
   a deleted option, another message or another guild answers with an ephemeral notice and performs
   no write.
4. **A select names its options by its values.** Every value must be a canonical positive role id
   that is still a stored option of that menu. A forged value, a duplicate, a non-number, or more
   values than `policy::SELECT_OPTION_LIMIT` is refused with a stable code and no write. An
   exclusive group takes exactly one value; each accepted value is planned by RM1 as one click.
5. **Defer first.** The ephemeral acknowledgement happens before the menu is read and before any
   role write. A failed deferral stops the press (`defer_failed`): no read, no write. Then the work
   runs and one ephemeral follow-up is sent.
6. **RM1 decides.** The shared policy (`super::policy`) plans the click: a multi option toggles, an
   exclusive group swaps (removal first, then the addition), a max-select breach is refused
   (`max_reached`). The handler writes exactly the roles the plan names.
7. **Press-time refusals.** RM1's `check_add` calls K03 `decide` on `GrantPath::Menu` for every role
   it plans to add, so a protected, managed, privileged, deleted or @everyone role is refused at
   press time even when the stored row lists it, and an active safeword refuses a restricted role
   for that member. The handler does not decide again.
8. **Honest results.** The adds go through the K04 executor (`grant::apply`, so a 429 is retried up
   to `grant::MAX_RATE_LIMIT_RETRIES` times) and the removals through the same bounded retry. A
   failed write is reported with the classified K04 code, never discarded, and a failed follow-up is
   reported as `follow_up_failed`. A swap whose removal failed is `swap_partial`, one whose addition
   failed is `swap_partial_removed` (the member then holds neither), and a select with one value
   refused and one applied is `partial`.
9. **One member at a time.** Two presses of one member never interleave their read-modify-write: the
   whole cycle runs under that member's lock. The waiter count is taken and given back under the map
   lock, so a waiting press can never lose its slot. The entry is dropped when the member's last
   press is done (also on a panic), so the memory stays bounded.
10. **Home guild only, fail closed.** A press from another guild reads, writes and answers nothing.
   With the feature on and the home guild unset or zero, nothing is served; with the feature off,
   nothing is served. Shadow mode reports what it would do and writes nothing.
## What is tested (92 unit tests in `interactions.rs`)
Every rule above, over the fakes and over the real in-memory SQLite repository, plus the lock race
(8 threads) and three seeded LCG sweeps (presses, presses with failing writes, selects).
**RED evidence.** Commit `d72f968` (the tests with `handle_press` and `serve` stubbed out):
`1813 passed; 68 failed`. The implementation commit made all 92 pass.
## Config keys (READ by the wiring card, not by this module)
`ROLE_MENUS_MODE`: `off` (default) | `shadow` | `live`; the wiring card passes `Mode::Off` for `off`
and for any value it does not know, so a typo is off. `GUILD_ID`: the home guild; unset or zero
fails closed (`no_home_guild`). `PROTECTED_ROLE_IDS`: roles a menu may not grant; a malformed entry
makes the wiring card set `protected_roles_invalid`, and every press is then refused.
`SAFEWORD_RESTRICTED_ROLE_IDS`: roles that may not be granted while a member's safeword is active;
the wiring card passes the member's own safeword state per press. `GUILD_MEMBERS` (privileged) and
`MANAGE_ROLES` let the wiring card read the member's roles and the bot's top role; this module reads
neither.
## NOT done
No command, no slash command, no feature switched on, nothing called from `main.rs`. The module
carries no `#[allow]` of its own; it is covered by the one temporary module-level allow
`src/role_menus/mod.rs` already has (the wiring card removes it). The ports have no serenity
implementation yet. The module does not post or edit the menu message, does not touch the menu
tables and does not decide what a menu looks like (RM2 owns that); the wiring card builds the
component ids with `custom_id_for_option` and `custom_id_for_select` and writes the select values as
the role id in decimal. The per-member lock is per process, so two bot instances would not share it.
RM2 does not record whether a select is single- or multi-select, so every select is treated the same
way: a multi group toggles each value, an exclusive group takes exactly one.

# Role menu interactions (RM3)

`src/role_menus/interactions.rs` handles button and select presses on menus stored by RM2.
W4 owns the Discord adapter; RM1 planning uses plain values, with no I/O or clock reads.
## The rules the code enforces (each has a test)
1. **The custom id is parsed strictly.** A button is `rolemenu:v1:m<menu id>:r<role id>` and
   a select is `rolemenu:v1:m<menu id>:s<part>` (`rolemenu:v1:` is PLAN's prefix). At most
   100 bytes, canonical ASCII decimal numbers, positive menu and role IDs; a wrong prefix, shape
   or a hostile number is refused as a class, and a refused id is ignored before the menu is
   read. The part is the zero-based chunk number of RM2's `split_chunks`, so the select
   components of one big menu have different ids; a part at or above `MAX_SELECT_PARTS`
   (five) is refused.
2. **The stored row is the truth.** The menu's guild and message must match the press.
   A missing menu, missing option or mismatched row gets a private notice without a member
   read or role write. Buttons identify options by role ID, not position, so removing one
   option cannot make its button grant the role that takes its place.
3. **A select names its options by its values.** Every value must be a canonical positive
   role id that is still a stored option of the part the id names; a value of another part is
   refused. A forged value, a duplicate, a non-number, or more values than
   `policy::SELECT_OPTION_LIMIT` is refused with a stable code, no write. An exclusive group
   accepts at most one value; each accepted value is planned by RM1 as one click. A valid
   empty select returns `no_change` without reading member roles or role facts.
4. **Defer first.** The ephemeral acknowledgement happens before the menu is read and before
   any role write. A failed deferral stops the press (`defer_failed`): no read, no write.
   Then the work runs and one ephemeral follow-up is sent.
5. **RM1 decides.** The shared policy (`super::policy`) plans the click: a multi option
   toggles, an exclusive group swaps (removal first, then the addition), a max-select breach
   is refused (`max_reached`). The handler writes exactly the roles the plan names.
6. **Press-time refusals.** RM1's `check_add` calls K03 `decide` on `GrantPath::Menu` for
   every role it plans to add, so a protected, managed, privileged, deleted or @everyone role
   is refused at press time even when the row lists it, and an active safeword refuses a
   restricted role for that member.
7. **Honest results.** The adds go through the K04 executor (`grant::apply`, so a 429 is
   retried up to `grant::MAX_RATE_LIMIT_RETRIES` times) and the removals through the same
   bounded retry. A failed write is reported with the classified K04 code, never discarded, and
   a failed follow-up as `follow_up_failed`. The code says what happened: `write_failed` when
   nothing went through; `swap_partial` when an exclusive swap added the new option but could
   not remove every old one; `swap_partial_removed` when an exclusive swap removed every old
   option it planned to remove but could not add the new one; `partial_write` for other
   mixes of successful and failed writes. The reply details the first write failure, if any.
   With no write failures, a mixed select is `partial` and retains the first refusal's code
   or detail, e.g. `partial (max_reached)` in live and shadow.
8. **One member at a time.** A process-local member lock serializes read/plan/write. The
   map entry is dropped after the member's last press (including unwinding on panic).
9. **Home guild only, fail closed.** A press from another guild reads, writes and answers
   nothing. With the feature on and the home guild unset or zero, nothing is served; with the
   feature off, nothing is served. Shadow mode reports what it would do and writes nothing.
## What is tested (107 unit tests in `interactions.rs`)
Includes real in-memory SQLite cases, an eight-thread lock race and three seeded LCG sweeps.
**RED** `3f6b00c`: 101 passed, six failed (live/shadow refusal details, three partial-write
codes, empty-select reads). The select-part tests already passed. Mutation M10 (keep the
last refusal) fails the test with distinct safeword and max-select refusals.
## Config keys (READ by the wiring card, not by this module)
`ROLE_MENUS_MODE`: `off` (default) | `shadow` | `live`; the wiring card passes `Mode::Off`
for `off` and for any value it does not know, so a typo is off. `GUILD_ID`: the home guild;
unset or zero fails closed (`no_home_guild`). `PROTECTED_ROLE_IDS`: roles a menu may not
grant; a malformed entry makes W4 set `protected_roles_invalid`, refusing any press that
reaches role planning. `SAFEWORD_RESTRICTED_ROLE_IDS`: roles that may not be granted while a member's
safeword is active; the wiring card passes the member's safeword state per press.
The Discord adapter needs the `GUILD_MEMBERS` intent and `MANAGE_ROLES` permission.
## NOT done
No wiring, commands, menu posting, feature activation, or live Discord check. W4 must use
`custom_id_for_option(menu, role)` and `custom_id_for_select(menu, part)`, with role IDs as
decimal select values. Legacy bare `:s` is refused; stale selects may be refused when options
move between chunks. No safeword snapshot queue here. The lock does not cover other bot
instances or external edits. Writes are not transactional; `PressOutcome` records observed
successes (planned writes in shadow).

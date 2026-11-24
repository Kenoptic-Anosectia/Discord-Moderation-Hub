# Role menu interactions (RM3)

`src/role_menus/interactions.rs`: the testable core behind a member pressing a button or
picking from a select on a role menu RM2 posted. The poise/serenity wiring is the wiring
card's job (W4); this module takes plain values and returns plain values: no serenity
type, no I/O, no clock read, no randomness in the decision path.
## The rules the code enforces (each has a test)
1. **The custom id is parsed strictly.** It is `rolemenu:v1:m<menu id>:o<option index>`
   (`rolemenu:v1:` is the prefix PLAN section 5 names for role-menu components), at most
   100 bytes, ASCII digits only, no zero menu id. A foreign prefix, a wrong shape, a
   signed or non-ASCII number, a missing or extra part is refused: the whole class is
   refused, not the examples thought of. A refused id is ignored before the menu is read,
   so no member data is read and nothing is written.
2. **The stored row is the truth.** The menu row must be stored, belong to the guild the
   press came from and record the message the component is on; the option must still be
   stored. A deleted menu, a deleted option, another message or another guild answers
   with an ephemeral notice and performs no write.
3. **Defer first.** The ephemeral acknowledgement happens before the menu is read and
   before any role write. A failed deferral stops the press: no read, no write. Then the
   work runs and one ephemeral follow-up is sent.
4. **RM1 decides.** The shared policy (`super::policy`) plans the click: a multi option
   toggles, an exclusive group swaps (removal first, then the addition), a max-select
   breach is refused (`max_reached`). The handler writes exactly the roles the plan names.
5. **Press-time refusals.** RM1's `check_add` calls K03 `decide` on `GrantPath::Menu` for
   every role it plans to add, so a protected, managed, privileged, deleted or @everyone
   role is refused at press time even when the stored row lists it, and an active
   safeword refuses a restricted role for that member. The handler does not decide again.
6. **Honest results.** A swap whose removal failed is `swap_partial` (new option added,
   old one stayed); it is never reported as a success. A failed write is `write_failed`
   and reaches the failure reporter with the K04 code, never discarded. Every reply is
   built from stable codes: no role name, no label, no text.
7. **One member at a time.** Two presses of one member never interleave their
   read-modify-write: the whole cycle runs under that member's lock. The entry is dropped
   when the member's last press is done (also on a panic), so the memory is bounded by
   the members pressing at the same time. Two members never wait for each other.
8. **Home guild only, fail closed.** A press from another guild reads, writes and answers
   nothing. With the feature on and the home guild unset or zero, nothing is served; with
   the feature off, nothing is served. Shadow mode reports what it would do and writes
   nothing.
## What is tested (61 unit tests in `interactions.rs`)
Custom-id parsing; the gate (off, unset or zero home guild, another guild); forged ids;
the stored row as the truth (deleted menu, deleted option, another message, another
guild, a menu never posted, a store failure, a corrupt mode); deferral before any slow
call and a failed deferral; RM1 toggle, exclusive swap, already selected, max-select;
a blocked swap and a partial swap; the press-time refusals (protected, managed,
privileged, deleted role, unreadable protected list, safeword); write failures; shadow
mode; the per-member lock (overlapping presses, two members, release after a press, a
refused press and a panic); the real in-memory SQLite repository; and two seeded LCG
sweeps (200 cases x 8 presses) that assert every press answers exactly once, a failed
write is never reported as a success, the member's roles end up exactly as the model says
and the lock is never left held.
**RED evidence.** Commit `d2967ca` (the tests with `handle_press` stubbed out):
`17 passed; 42 failed`. The implementation commit made all 61 pass.
**Mutations.** 25 mutations were tried, one at a time, and every one was caught by at
least one test: the guild gate, the custom-id length bound, the prefix, digits-only and
zero-menu-id checks, the message and guild checks, the deferral order, the safeword
restriction (at every site), the protected list, the policy's role facts and safeword
state, the bot's top position, the menu mode, the member's roles, shadow mode, the
per-member lock, the option index, the deleted-option check, the blocked swap, the
partial swap and the removal/addition order.
## Config keys (READ by the wiring card, not by this module)
`ROLE_MENUS_MODE`: `off` (default) | `shadow` | `live`; the wiring card passes
`Mode::Off` for `off` and for any value it does not know, so a typo is off. `GUILD_ID`:
the home guild; unset or zero fails closed (`no_home_guild`). `PROTECTED_ROLE_IDS`: roles
a menu may not grant; a malformed entry makes the wiring card set
`protected_roles_invalid`, and every press is then refused.
`SAFEWORD_RESTRICTED_ROLE_IDS`: roles that may not be granted while a member's safeword
is active; the wiring card passes the member's own safeword state per press.
`GUILD_MEMBERS` (privileged) and `MANAGE_ROLES` in the guild are needed for the wiring
card to read the member's roles and the bot's top role; this module reads neither.
## NOT done
No command, no slash command, no feature switched on, nothing called from `main.rs`. The
module carries no `#[allow]` of its own; it is covered by the one temporary module-level
allow `src/role_menus/mod.rs` already has (the wiring card removes it). No serenity
implementation of the ports exists yet, so `MenuStore`, `RoleFactsSource`, `RoleService`,
`Clock`, `FailureReporter` and `InteractionResponder` have no production implementation.
The module does not post or edit the menu message, does not touch the menu tables and
does not decide what a menu looks like (RM2 owns that). Role writes go through the
`RoleService` port: the wiring card must back it with the shared role service (K04/RG2)
so the 429 retry and the fail codes stay in charge. Known limits: the exclusive swap
removes the old option before it adds the new one, so a member removed between the two
writes ends up with both roles; a select menu is pressed one option at a time, so a
multi-select Discord submits as a list is not handled here; the per-member lock is per
process, so two bot instances would not share it.

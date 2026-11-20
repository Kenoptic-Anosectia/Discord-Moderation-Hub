# Role menu selection policy (RM1)

Pure, Discord-free core: `src/role_menus/policy.rs`. `plan_click(&PlanRequest) -> Result<Plan, PlanError>` turns one click (member's current roles, group, option) into `Plan { add, remove, skipped }`; `split_components(&Menu)` lays options out in selects. Types: `Menu`, `Group`, `GroupMode::{Exclusive, Multi}`, `MenuOption`, `Click`, `GrantContext` (K03 inputs: guild id, bot top position, role facts, protected set, safeword state and restricted set).

## Rules enforced
1. Exclusive: choosing an option adds it and removes the member's other options of that group. `max` is ignored. Clicking the held option is a no-op reported as `AlreadySelected` (no clearing; choice: "choose exactly one"). Extra options held from before are removed on that click.
2. Exclusive swap is all-or-nothing: if removing any other held option is denied (`decide_removal`), nothing changes and `SwapBlocked` is reported, so a swap never adds an option while an old one stays (tested from states holding at most one option per Exclusive group). A deleted or unknown held role is reported, not blocking.
3. Multi: options accumulate; clicking a held option removes it (toggle). `max: N` refuses an add when the member already holds N options of the group (`MaxReached`); removals still work. Only roles of that group count, each once.
4. Every role in `add` passed K03 `decide` on `GrantPath::Menu`; a Deny becomes `skipped` with `Reason::Denied(code)` (K03 code). Removals use `decide_removal`. A denied add never removes the member's current option. A role with no facts counts as deleted (`role_deleted`).
5. A click on a role that is not an option of the group is skipped as `UnknownOption`. Only an unknown group id is an `Err` (`UnknownGroup`). Reasons and errors have stable `code()` strings, no names or ids.
6. Split rule: per group, options in declared order, repeated roles dropped, chunks of 25 (`SELECT_OPTION_LIMIT`); a component never mixes groups; empty group gives none. Policy treats the whole group as one set across its components. Message-level limits (rows per message) are not checked.

## Tests (34, `policy.rs`)
Exclusive swap/first pick/no-op/repair/blocked swap/gone role/denied target, multi keep-several, max boundary (below, at, zero, over, counting), toggle, unknown group/option, deleted and fact-less roles, split at 0/1/25/26/50/51 plus order and dedupe, K03 denials (privileged bits, managed, protected, safeword restricted, above bot, deleted, @everyone) for both modes, removals during safeword, code stability, and a seeded LCG sequence (200 runs x 100 clicks) re-checking every add with K03 and the Exclusive/max invariants on a model member.
RED evidence (commit 65436f6, stub returning an empty plan): 33 of 34 failed; only `reason_codes_are_stable` passed. Mutations on the finished code, each failed tests: max `>=` to `>`; exclusive removals dropped; swap-block check disabled; `decide_removal` swapped for `decide`; chunk 25 to 26; unknown-option check off; Menu path to StaffCommand. Menu to Reaction is NOT detectable (K03 treats them identically).

## Config keys
None. Menus come from a template read by a later card.

## NOT done
No runtime, commands, persistence, component posting, custom_id parsing or config; nothing calls this yet (module has one temporary `#![allow(dead_code)]` that wiring card W1 removes). Safeword deferral (`SAFEWORD_DEFER_GRANTS`) is not here: a denied restricted add is only reported.

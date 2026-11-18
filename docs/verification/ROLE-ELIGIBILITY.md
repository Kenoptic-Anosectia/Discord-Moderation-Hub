# Role eligibility policy (K03)

Pure, Discord-free decision core: `src/roles/eligibility.rs`. `decide(&GrantRequest) -> Decision` (`Allow` | `Deny(DenyReason)`) and `decide_removal`. Automated paths = every path except `StaffCommand`.

## Rules (first match wins; its reason is returned)
1. Role deleted -> `RoleDeleted`.
2. @everyone (id == guild id) -> `EveryoneRole`; managed/integration -> `ManagedRole` (all paths).
3. Position >= bot top role -> `AtOrAboveBot` (equal denies).
4. ADMINISTRATOR/MANAGE_GUILD/MANAGE_ROLES/BAN/KICK/MODERATE bit -> `PrivilegedPermission` (automated only).
5. Role in protected set -> `ProtectedRole` (automated only; staff are governed by rule 7).
6. Safeword Active + role restricted -> `SafewordRestricted` on every path except `SafewordRestore`.
7. `StaffCommand`: actor strictly above role and above target, else `ActorNotAboveRole` / `ActorNotAboveTarget`; absent staff context -> `MissingStaffContext` (fail closed).
`decide_removal`: allowed always (safeword, protected, privileged ignored) except `AtOrAboveBot`. Deleted/@everyone removals are not specially denied (spec: only hierarchy blocks).
`DenyReason` is a `Copy` 1-byte enum with `code()` stable strings; no names, ids or text.

## Tests (14, in `eligibility.rs`)
Per-rule tests, all six permission bits on every path, safeword matrix, removals, a precedence table, code stability, and a 20k-case deterministic LCG sweep of invariants.
RED evidence (commit 5dfc542, stub `decide` returning Allow): 11 of 14 failed (all behavior tests; only baseline, staff-ignored-on-automated and code-stability passed). Then implemented.

## Config keys (READ by the wiring card, not here)
- `PROTECTED_ROLE_IDS`: comma list of role ids, default empty. Unparseable entries should be dropped with a warning (never widen); empty means no extra protection (rules 1-4,6 still apply).
No other keys. Safeword state and restricted roles come from the safeword feature at call time.

## NOT done
No runtime wiring, no commands, no persistence, no config parsing, no Discord calls; nothing calls `decide` yet (module carries one temporary `#![allow(dead_code)]` that wiring card W1 removes). D05 (which ids are protected) is open and left as an input.

# Role grant executor (K04)

Pure, Discord-free core: `src/roles/grant.rs`. `apply(&mut RoleWriter, &mut Sleeper, &GrantRequest) -> GrantReport`. The caller passes the K03 `Decision` (`eligibility::decide`) per role; traits are synchronous so tests need no runtime (the async Discord writer in RG2 must bridge to this shape).

## Rules the code enforces
1. One `RoleResult` per request item, in order: `Done`, `NoOp`, `Denied(DenyReason)`, `Failed(FailCode)`. `status()` is derived from them: `AllDone` (all Done/NoOp), `AllFailed` (all Denied/Failed), otherwise `Partial`; empty request = `Empty`. A failure does not stop later roles.
2. A K03 `Deny` is never written, even if held; a deny on any repeat of a role id denies that role everywhere in the request.
3. `already_held` (caller-supplied) -> `NoOp`, no writer call. A repeated role id is written once.
4. `classify`: 403 forbidden; 404 + api code 10011 unknown_role; 404 + 10007/10013 unknown_member; 429 rate_limited; anything else (incl. a bare 404) `other`.
5. Only 429 is retried: at most 3 retries per role (4 writer calls), sleeping through the injected `Sleeper` (writer hint, else 1s/2s/3s, each sleep clamped to 30s). Then `Failed(rate_limited)`. The 3-retry limit is per role, not per request.
6. Report holds numeric ids and code strings only; `summary_line()` renders them.

## Tests (21 in `grant.rs`)
All ok; first-fails/second-ok and the reverse = Partial; code mapping incl. ambiguous/hostile statuses; 429 retry count, sleeps, hint, clamp, success on 4th call; NoOp; deny for all 10 reasons; duplicates; empty; exact summary lines; 3,000-case seeded LCG sweep checking termination, call/sleep counts, no false Done, no hidden grant and status.
RED evidence (commit e86cef7, stub `apply` returning no results): 20 of 21 failed; only `empty_request_is_empty_not_success` passed.
Mutations tried (each made at least one test fail): retries 3->4, ignoring Deny, Partial reported as AllDone, ignoring `already_held`, removing the sleep clamp, retrying non-429, widening 404 mapping, duplicate `Done` repeat.

## Config keys
None. Retry limit, backoff and clamp are constants in `grant.rs`.

## NOT done
No runtime, no commands, no Discord calls, no persistence, no real writer or sleeper, nothing calls `apply` yet. `already_held` and the K03 decisions are trusted inputs (not re-checked). A 429 on one role does not slow the next role. Retries use no jitter.

# A03 — Staff-authorized paired age-role grants

## Isolation and scope

- Task: `t_e693d5a9`; branch: `wt/skynet-staff-age-command`.
- Workspace: `C:/Users/anose/AppData/Local/hermes/projects/skynet-workers/staff-age-command`.
- Initial HEAD: `3ab45d8c50e83769ebaff5efa95f006ac28f62f9`, matching the supplied baseline.
- Initial `git status --short` was empty. `.env` existence was checked, **absent**, without reading any real environment file. The final audit also confirms absence.
- No main-checkout access, private profile/auth access, token loading, Discord requests, application startup, command registration, database initialization/migrations, local models, dependency fetching/upgrades, pushes, merges or deployments occurred. No other board cards or delegates were created.
- `Cargo.toml`, `Cargo.lock`, CI and `.gitignore` are unchanged. Existing cached locked dependencies sufficed throughout.
- The implementation commit is the local commit containing this report; its exact SHA is supplied in the same-card QA handoff. QA is still required and does not authorize integration or deployment.

## Implementation and policy

The slash name `age-verified` and its single required `user` member option are unchanged. The caller Manage Roles runtime gate and default-member visibility gate were removed; guild-only and bot Manage Roles remain. A generated-command test checks both the Poise structure and serialized Discord registration builder. In particular, `default_member_permissions` is null/omitted, **not** the Discord string `"0"` that would disable ordinary member access. No registration request was made.

`execute_age_roles` is a small async authorization/orchestration seam in the existing module. Production supplies the invoking guild, resolved caller/target members, existing `Config.staff_role_id`, configured role pair, server role IDs from a read-only Discord lookup, and an injected single-role `Member::add_role` operation. It validates guild membership/context, role existence, nonzero/non-@everyone IDs and that neither grant target is the staff role, before invoking any write. Caller permissions, ownership and role hierarchy are not alternative authorization paths. Discord still enforces the bot's own ability to grant roles.

Missing, malformed, overflowed or zero `STAFF_ROLE_ID` maps to `None` without preventing unrelated logger/reaction configuration from loading. The pure parser is tested without reading or changing process environment variables. Other settings retain their existing parsing. A nonexistent/@everyone staff role is rejected at command validation. No protected-role list or semantic marker-role mapping was introduced.

The operation remains two ordered additive role requests, first `AGE_ROLE_1`, then `AGE_ROLE_2`. It never replaces a full member role snapshot, removes a role, rolls back, persists a revocation state, or blocks deliberate repeated grants. A failed first request stops the sequence. A failed second request reports the first request's success; neither failure path reports full success. Messages distinguish request outcome from remote-state certainty because a lost response can follow a successfully applied Discord PUT.

The handler defers privately before network work. A failed defer returns a sanitized error before any role write. Failed server-role lookup sends a private no-writes explanation. All normal results/denials are private; reply failures propagate sanitized errors, with the known role-request outcome included for a failed final reply. Raw external error contents are not forwarded or logged by this command. README and `.env.example` document the staff setting, repeat behavior and non-atomic failure semantics; placeholder setting values are unchanged.

## RED → GREEN evidence

Tests were added and exercised before the corresponding new behavior. The grant seam initially preserved ordered stop-on-error calls with a generic failure result and no authorization; later stages added the missing checks and reporting. Each stage below was followed by its minimal implementation before proceeding to the next stage. The repeat/additive test characterizes retained grant semantics rather than claiming those were new.

Commands ran in this worktree. `set -o pipefail` with `tee` preserved the Cargo exit status while saving logs under ignored `target/verification/`. The following are actual observed command results:

| Stage / exact Cargo command | RED exit and observed failure | GREEN exit / result |
| --- | --- | --- |
| `cargo test --locked --offline age_command_metadata` | 101; 0 passed, 1 failed, 7 filtered; `command.required_permissions.is_empty()` failed on the old gate | 0; 1 passed, 7 filtered |
| `cargo test --locked --offline age_nonstaff` | 101; 0 passed, 1 failed, 8 filtered; returned full success instead of non-staff denial | 0; 1 passed, 8 filtered |
| `cargo test --locked --offline age_invalid_context` | 101; 0 passed, 1 failed, 9 filtered; DM case 0 performed writes and returned success | Subsequent `cargo test --locked --offline age_`: 0; 3 passed, 7 filtered |
| `cargo test --locked --offline age_` after adding failure-reporting test | 101; 4 passed, 1 failed, 7 filtered; generic `Role grant failed.` did not describe first failure / unattempted second request | 0; 5 passed, 7 filtered |
| `cargo test --locked --offline unusable_staff` | 101; 0 passed, 1 failed, 12 filtered; parser returned `Some(0)` rather than disabled `None` | 0; 1 passed, 12 filtered |

Intermediate test-fixture compilation initially rejected a borrowed temporary role array (E0515); it was corrected to a constant fixture before the executable validation RED above. That compilation error is not counted as behavior RED. An attempted log-display wrapper using `python -c` was refused by the headless command approval filter before execution; ordinary Cargo + `tee` was used instead. No approval setting was changed.

Saved local logs: `red-metadata.log`, `green-metadata.log`, `red-authorization.log`, `green-authorization.log`, `red-validation.log`, `green-validation.log`, `red-reporting.log`, `green-reporting.log`, `red-config.log`, `green-config.log` in `target/verification/`. They are ignored build evidence, not required downloadable artifacts; this tracked report retains the substantive evidence if build outputs are cleaned.

## Executed test scope

The final harness executed **13 tests: 13 passed, 0 failed, 0 ignored, 0 measured, 0 filtered**.

Six new tests:

1. `age_command_metadata_allows_staff_without_manage_roles`: generated Poise guild-only/name/parameter/bot permissions; absent caller runtime/default permission gates; serialized registration has no default-member restriction and a required Discord USER option.
2. `age_nonstaff_with_manage_roles_is_denied_before_writes`: synthetic non-staff callers with no permissions, Manage Roles, or Administrator are denied by actual orchestration with zero injected writes.
3. `age_invalid_context_or_configuration_never_writes`: 13 synthetic cases covering DM, missing caller, cross-guild caller/target, missing/zero/@everyone/nonexistent staff role, zero in either grant slot, @everyone grant target, staff-as-grant target and nonexistent grant target. Every case makes zero writes.
4. `age_staff_grants_are_ordered_additive_and_repeatable`: a staff member with empty generic permissions grants both roles in order; a fake external role set retains unrelated role 777. Four explicit invocations include already-held roles and restoration after either configured role was removed. The target snapshot intentionally stays stale; no full-role replacement occurs.
5. `age_failed_grants_report_progress_and_never_rollback`: injected failure at either request, both with initially absent configured roles and with both already present. Asserts stop-on-error call counts, retained preexisting/newly granted roles, exact truthful messages, no full-success feedback and no raw injected error content.
6. `config::tests::unusable_staff_configuration_disables_only_the_age_command`: pure parser checks absence, empty/malformed/negative/overflow/zero values and a valid synthetic role ID. No token loading or process-environment mutation.

The seven original helper regressions remain byte-for-byte unchanged and pass: ASCII truncation, Unicode scalar truncation, small-limit ellipsis behavior, emoji-input normalization, reaction-type normalization, emoji lookup-key equivalence, and code-block placeholder/fence sanitization.

## Final commands and real exits

| Exact command | Exit | Result |
| --- | ---: | --- |
| `cargo fmt --all` | 0 | Formatting applied |
| `cargo fmt --all -- --check` | 0 | No formatting differences |
| `cargo check --locked --offline` | 0 | Compiles; six retained compiler warnings |
| `cargo build --locked --offline` | 0 | Binary built, not started; six retained compiler warnings |
| `cargo test --locked --offline` | 0 | 13 passed; six retained compiler warnings |
| `cargo clippy --locked --offline --all-targets` | 0 | Ten retained diagnostics per target, duplicate emission shared between bin/test |
| `git diff --check` | 0 | No whitespace errors |
| `git diff --exit-code -- Cargo.toml Cargo.lock .github .gitignore` | 0 | Inputs unchanged |
| `python target/verification/audit.py` | 0 | Isolation/scope assertions and added-source scan pass |

Six compiler warnings retained from baseline: deprecated `GuildChannel::permissions_for_user`, two unused ticket-button constants, unused `handle_ticket_button`, unused `next_ticket_number`, and unread ticket config fields (the same grouped warning no longer includes the now-used staff field). Clippy also reports three `result_large_err` occurrences in unchanged logger/audit helpers and one unchanged reaction-query `type_complexity`. The initial clippy run found a new `items_after_test_module` warning in config tests; moving the tests to the end removed it before the final run. Warnings were not suppressed. Git also emits Windows LF-to-CRLF informational warnings for edited Rust files.

The local audit compares manifest/lock/CI inputs to baseline, asserts `.env` absence, compares the seven old tests and preceding helpers exactly, and compares every unrelated command/event after `age_verified` exactly. It scans added Rust lines for obvious hardcoded credentials, unsafe/eval/exec and removal/full-role-update constructs. The scan's first version incorrectly flagged the read-only `guild.roles` lookup as a full-role update; the rule was narrowed to mutation constructors, and the corrected audit passed. Manual review of the full diff confirms the only production command mutation call is `user.add_role`.

## Limits / independent QA handoff

- Synthetic members and injected local futures exercise actual authorization/orchestration at the role-write boundary. They do not emulate the live Discord service, permission hierarchy, timeouts, concurrent external role changes, or Poise interaction delivery.
- The real handler compiles; deferred/private reply error paths were reviewed in source, not fault-injected through a live/fake Poise transport. Errors are propagated, not swallowed. No live acceptance testing or command refresh is authorized here.
- Server-role lookup introduces a read request before grants; failure safely stops all writes. Staff membership uses Poise's resolved invoking member, not a separate caller refresh. As with any two-request workflow, authorization/state may change concurrently; no transaction or remote-state certainty is promised.
- Existing server command overrides/registrations can still affect discoverability until an explicitly authorized future refresh. The local generated metadata does not itself alter those remote settings.
- Independent QA must review the exact implementation diff, rerun the offline commands, inspect generated metadata, and approve or request changes on this same card. Do not integrate, push, deploy or release broader epics. Rollback is the isolated branch only; main and production remain untouched.

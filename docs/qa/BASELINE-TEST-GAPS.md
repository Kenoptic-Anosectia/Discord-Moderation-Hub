# A02 — Independent offline baseline and test-gap audit

## Scope and verdict

- Tested source commit: `190b1b0619900052bfd9cdc824f005bebb2ecafe`.
- Workspace: `C:/Users/anose/AppData/Local/hermes/projects/skynet-workers/baseline-audit`.
- Verified branch: `wt/skynet-baseline-audit`; `.git` is a linked-worktree file.
- Initial tracked and untracked status: clean (`git status --short` produced no entries). Root `.env` was absent, checked by existence only; no credentials were opened.
- **Baseline verdict: CHANGES REQUIRED / behavior unverified.** Formatting fails, locked offline compilation and test execution stop at lock-file validation, and no first-party behavioral tests were found. This is a completed audit, not implementation, policy, production, or release approval.
- Inputs: assigned contract and this public-source baseline only. No other worktree, private history, live Discord, bot startup, database/schema execution, dependency downloads, or production operations were used. Only this report may change.

Source references below are baseline file/line references, not claims of executed behavior. “Reachable” means wired in source if startup and framework/Discord prerequisites succeed, not independently proven live operation.

## Executed evidence

Commands ran from the assigned workspace on the installed Windows Rust toolchain. No repair or dependency-resolution retry without `--locked` was attempted.

| Command | Actual exit | Observation / limit |
| --- | --- | --- |
| `git rev-parse HEAD` | 0 | Exact tested commit above. |
| `git branch --show-current` | 0 | Expected branch above. |
| `git status --short`; `git diff --name-only` | 0 each | Initially no entries. |
| `test -e .env` in an if/else existence check | 1 for predicate; 0 for enclosing command | Printed `.env ABSENT`; no content read. |
| `git rev-parse --is-inside-work-tree`; `test -f .git` in an if/else check | 0 | Inside worktree; linked-worktree `.git` file present. |
| `cargo --version`; `rustc --version`; `rustfmt --version` | 0 each | Cargo `1.99.1 (5f94df478 2026-08-27)`, rustc `1.99.1 (58cf8f9c3 2026-10-13)`, rustfmt `1.10.0-stable (58cf8f9c35 2026-10-13)`. |
| `cargo fmt --all -- --check` | **1** | Formatting diffs in `src/config.rs` and `src/main.rs` (including import ordering and wrapping). Check-only; no formatting applied. Not a behavioral failure. |
| `cargo check --locked --offline` | **101** | `cannot update the lock file ... Cargo.lock because --locked was passed to prevent this`. No successful compilation established. |
| `cargo test --locked --offline` | **101** | Same lock-file refusal; no tests reached execution. Not a failing behavioral assertion. |
| `git ls-files src tests benches .github Cargo.toml Cargo.lock .env.example` | 0 | Only the two source files, manifest/lock, example and workflow; no tracked `tests/` or `benches/`. |
| `rg -n '#\[(test|tokio::test)|cfg\(test\)' src` | 1 | No conventional first-party Rust test markers. Source inspection likewise found no test modules; this does not quantify dependency tests. |
| `rg -n -o '"[A-Z][A-Z_0-9]+"' src/config.rs` | 0 | Environment names only, listed below. |
| `git diff --exit-code 190b1b0619900052bfd9cdc824f005bebb2ecafe -- . ':!docs/qa/BASELINE-TEST-GAPS.md'` | 0 | Before report creation, all other tracked content identical to baseline; repeated as a final scope gate. |

An initial combined name-extraction/format-check invocation was rejected by the tool approval guard because it used Python `-c` (tool exit `-1`, no process executed). The separate `rg` name-only query and actual standalone format check above replaced it. The refusal is not a source defect.

### Manifest, lock, config names, and CI

- Manifest: Rust 2021 binary application, `publish = false`; dependencies include Poise, Serenity, Tokio, dotenvy, serde/JSON and SQLite (`Cargo.toml:1-27`). No dev-dependency or explicit test-target section in the manifest. Release uses `panic = "abort"` (`Cargo.toml:35-41`).
- Concrete lock discrepancy: manifest package is `discord-moderation-hub` (`Cargo.toml:2`), whereas lock root package is `skynet-network-core` (`Cargo.lock:1607-1619`). This is a source-supported inconsistency consistent with the locked-command refusal; the command output does not enumerate every required lock change.
- Lock format is 4 (`Cargo.lock:1-3`). Inspected relevant locked versions: Poise 0.6.2 (`1022-1039`), Serenity 0.12.5 (`1510-1545`), rusqlite 0.37.0 (`1306-1318`), tokio-rusqlite 0.6.1 (`1862-1871`), libsqlite3-sys 0.35.0 (`859-860`). Differences from compatible manifest minimum versions are not alone defects.
- `src/config.rs:20-28` names: `DISCORD_TOKEN`, `LOG_CHANNEL_ID`, `AGE_ROLE_1`, `AGE_ROLE_2`, `REACTION_ROLE_DB_PATH`, `STAFF_ROLE_ID`, `TICKET_CATEGORY_ID`, `AGE_VERIFICATION_CATEGORY_ID`. `.env.example:1-15` documents these same names, with DB path and ticket IDs marked optional. No actual environment values were inspected or exercised; config behavior is outside this names-only inspection.
- CI declares Ubuntu build and tests, `cargo build --verbose` then `cargo test --verbose`, on main push/PR (`.github/workflows/rust.yml:3-22`). It does not declare fmt, Clippy, `--locked`, offline mode, or an explicit toolchain pin. No CI run result was fetched. A build or empty test suite would not prove role/privacy behavior.
- README explicitly combines implemented and in-progress features (`README.md:20-35,101-103`); it is not a command reachability specification.

## Reachability inventory

| Path | Classification and source evidence |
| --- | --- |
| Startup | `main` loads dotenv, reads config, constructs intents and framework, registers commands globally during setup, initializes reaction-role DB and fresh memory cache, then starts client (`src/main.rs:2207-2259`). **Not run**: would invoke external registration/startup and DB initialization. |
| `/age-verified` | **Wired implementation** via command vector `2228`. Guild-only; caller/default and bot `MANAGE_ROLES` metadata (`626-633`); sequential two-role addition then public success reply (`634-652`). No explicit protected-role or actor/bot hierarchy preflight in body. |
| `/reactionrole add/remove/list` | **Wired implementation**: parent includes subcommands (`655-660`), registered parent (`2228`). Subcommands declare caller `MANAGE_ROLES`; add/remove declare bot permissions (`664-804`). Add fetches message, persists binding, then attempts initial reaction; remove deletes matching message/role bindings; list formats DB rows. |
| Reaction add/remove | **Wired event branches** (`825-889`) through framework event handler (`2229-2230`). Ignore self/missing guild/user, normalize emoji, look up roles, fetch member, attempt each grant/removal. Role-operation errors are discarded. Other bots are not explicitly excluded. |
| Message create/delete/bulk-delete/update | **Wired event branches** (`816-823,891-1142`): create caches text/embeds, delete emits cached content or missing-cache notice, bulk emits IDs, update emits changed text and embed JSON attachments. Create, bulk and update skip configured log channel; single delete does not. No private-channel/category exclusion predicate in these branches. |
| Voice/channel/role/member events | **Wired event branches**: voice (`1144-1205`), channel create/delete/update (`1207-1315`), role create/delete/update (`1317-1441`), member join/update/removal (`1443-1672`). Audit lookups support attribution (`375-481`). |
| Invites, bans, emoji, AutoMod rules | **Wired event branches**: invite create/delete (`1674-1715`), ban/unban (`1717-1830`), emojis (`1832-1863`), AutoMod rule create/update/delete (`1865-1969`). Invite branches exist but explicit `GUILD_INVITES` intent is absent from intent expression (`2217-2224`); actual delivery was not tested. |
| Ticket panel | **Unregistered scaffolding**: owner-only text-channel panel function exists (`1977-2044`), but is absent from the sole command vector (`2228`). Not an available registered command in this baseline. |
| Ticket interaction handler | **Unreachable scaffolding**: `handle_ticket_button` defined (`2046-2184`), but no caller and no `InteractionCreate` branch. Event match ends in `_ => {}` (`1971`). Do not interpret ticket test proposals as tests of a live feature. |
| Ticket numbering | **Pure helper, scaffold-only caller**: `next_ticket_number` (`2186-2205`) used only by ticket handler (`2109`). Derives next suffix from existing channel names; not a persisted allocator. |
| Ticket close/claim/transcript/recovery | **Future requirements, not implemented here**: no corresponding command/event/state table in inspected baseline. No policy for their behavior is approved by this audit. |

## Confirmed observations versus unproven risks

### Current source/check-supported defects and gaps

1. **Build reproducibility/check gate:** fmt exits 1; locked offline check/test exit 101. Root package-name discrepancy is confirmed. Compilation diagnostics and runtime correctness remain unknown; no claim of dependency download failure or Rust type error is justified by these outputs.
2. **Silent failures:** reaction grant/removal errors are discarded (`853,885-887`); initial panel reaction error is discarded before a success response (`708-721`); logger send results are discarded (e.g. `921,936,985,1127-1129`). Audit retry helpers collapse lookup errors into no attribution (`411-429,452-454`). These are source-confirmed observability gaps, not observed live outages.
3. **Non-atomic age-role sequence:** first grant is retained if second grant errors; `?` returns before success reply with no rollback/partial-result branch (`643-650`). The partial-state path is explicit; whether to roll back or retain/report it requires a decision, not an invented all-or-nothing contract.
4. **Asymmetric log-channel exclusion:** single message delete (`891-938`) lacks the exclusion present for create/bulk/update (`817,945,993`). Even an uncached deletion in the log channel constructs another log message. Source supports the extra-send path, not an assertion of an infinite event loop.
5. **No private-content exclusion boundary in logger:** accessible message content/embeds are cached and edits/deletes forwarded without checking privacy/category (`816-823,891-1142`). This is a current missing safeguard; no actual disclosure, channel membership, retention authorization, or approved exclusion set was established.
6. **No executed behavior evidence:** source contains no conventional test markers, no tracked integration tests; CI's test command is not a behavioral suite. No passing runtime assertions are reported.

### Hypotheses / future risks (not reproduced bugs)

- Role workflows have permission metadata but no local checks for actor hierarchy, bot hierarchy, managed/protected/everyone roles (`626-804,825-889`). Discord/API enforcement is not proof of intended application policy. No claim of successful privilege escalation is made.
- Partial `MessageUpdate` data can be treated as empty content/embeds (`1032-1048`), potentially creating false deletion/edit signals or replacing cached fields (`1132-1141`). Needs synthetic partial-event regression evidence.
- Bulk deletion does not remove cached message entries (`940-986`); cache eviction is arbitrary HashMap-key eviction, not age order (`349-359`). Restart creates an empty cache (`2247`); historic message recovery is not implemented.
- Invite-event delivery may be incomplete without explicit invite intent; needs framework/default-intent contract verification before live claims. Member role-change audit query requests `MemberAction::Update` (`445`); correct role-update attribution needs a dependency-API fixture test.
- Duplicate ban notices are plausible from member-removal and ban-addition branches (`1593-1627,1717-1770`), but event ordering and duplication were not exercised.
- **Scaffold-only:** ticket handler treats every non-age custom ID as general (`2057-2086`); no panel-message provenance check. Duplicate suppression uses visibility of a prefixed channel, not ownership (`2088-2107`); staff visibility may reject an unrelated user's ticket. No lock/idempotency covers check-then-create (`2088-2144`); concurrent opens may duplicate channels/numbers. Creation can precede greeting/ack failure (`2144-2181`), with no durable reconciliation record. These must be addressed before wiring, not reported as current reachable exploits.

## Prioritized acceptance/test matrix (all proposals, NOT passing tests)

Legend: **I** = implementation wired in source; **S** = unreachable scaffold; **F** = future requirement; **P** = policy pending Ava. P0 precedes feature exposure/expansion; P1 is the next regression tranche; P2 is hardening. Every row below is **not executed** in this audit. Use synthetic fixtures and offline HTTP mocks; DB tests would need a separately authorized synthetic-schema contract because this card prohibits schema execution.

| Priority / area | State and baseline evidence | Bounded test proposal / acceptance target |
| --- | --- | --- |
| P0 — reproducible validation | I; manifest/lock and CI above | Separate implementation task aligns intended root name/lock and formatting, without runtime changes. Rerun locked check/test/fmt with recorded exits. No source-success inference until commands succeed. |
| P0 — role hierarchy / protected roles | I + P; `626-804,825-889` | Fixture table: caller/bot above, equal, below target role; managed/everyone/deleted role; owner/admin; missing permissions; protected-role set. Assert denied requests cause no mutation and reason is observable; exact protected set and allowed actors require Ava. Test command and reaction pathways separately. |
| P0 — partial age-role grants | I + P; `640-650` | Mock first grant failure, second failure, both success, pre-existing first/second, retry after partial grant. Assert call order and accurate partial-result reporting; choose retain-vs-compensate policy before asserting final state. Success text only after intended success. |
| P0 — reaction role failures | I; `691-721,825-889` | Mock one of multiple role operations failing, stale role, HTTP 403/404/429, missing member, malformed emoji and initial reaction failure. Assert failures are observable without falsely claiming full setup, successful unrelated grants handled per approved policy, and no unrelated role mutations. |
| P0 — private-channel exclusions | I missing safeguard + F/P; `816-823,891-1142` | After Ava defines exclusion set, test private support/age ticket, category descendant, private thread, public channel, unknown/missing cache, and log channel for create/edit/single/bulk delete. Excluded events must neither cache nor emit text/embed JSON/attachments. Test policy changes against previously cached data. No real messages. |
| P0 — ticket interaction authorization | S + F/P; `1977-2107` | Before registration/dispatch, deny unknown custom ID, forged/stale panel message, wrong guild, DM, non-owner panel post; allow approved valid panel interaction. Assert no channel creation on denial and private acknowledgement. Decide authorized panel posters and accepted panel provenance first. |
| P0 — concurrent ticket opens | S + F/P; `2088-2144,2186-2205` | Barrier-controlled two opens for same user/type; different users; general vs age; repeated interaction ID. Assert approved per-user uniqueness, idempotent retry, unique allocation, bounded acknowledgement and no leaked orphan. Decision needed on one-open rule and numbering reuse; current code supplies no atomic reservation. |
| P0 — ticket permission boundaries | S + F/P; `2112-2142` | Inspect constructed overwrite payload for requester/staff/everyone, invalid/missing category/staff role and privileged-role caveats. Assert no unintended public access in approved permission model; validate category/guild binding. Do not claim administrator denial from everyone overwrite. |
| P1 — restart / failure recovery | I for reaction DB/cache; S/F tickets; `483-624,2144-2181,2243-2248` | Separately authorized synthetic DB reopen preserves bindings; corrupt/unreadable storage fails visibly, malformed stored role ID is reported rather than silently skipped (`614-616`). Cold cache produces truthful unknown history. Ticket restart after create-before-ack reconciles one durable ownership record once implemented; no production migration. |
| P1 — pure text / emoji helpers | I; `55-75,118-125,246-347` | Empty/whitespace, escaped triple backticks, Unicode scalars, exact/over limit, max 0/1/2/3 for truncation; absent/empty optional strings; no roles/embeds; unicode/custom/animated/missing-name/malformed/space-padded emoji. Compare binding parse keys with reaction normalization; expose raw-vs-trimmed reaction mismatch. Small-limit truncation appends three dots even below limit: decide helper contract (current callers use larger limits). |
| P1 — pure voice / format helpers | I; `138-303,309-318` | Exhaust boolean/None transitions, stable voice flags ordering, unchanged state, join/leave/move; channel type fallback, invite zero limits, empty role list, automod unknown event/count, executor/target formatting and JSON attachment bytes. Assert exact bounded outputs from synthetic models. |
| P1 — ticket-number helper | S; `2186-2205` | Empty set, gaps/deleted highest, mixed prefixes, malformed suffix, non-text channel, `u32::MAX` suffix. Explicitly decide exact prefix matching, overflow handling and reuse semantics; current broad prefix/suffix scan is not an atomic ID allocator. |
| P1 — event/error observability | I; `375-481,853,885-887,921,1127-1129` | Fake-clock retries with absent/stale/wrong-target audit records and HTTP errors; assert retry budget and error-vs-no-match distinction. Mock denied log sends and role failures; require sanitized context (event/type/IDs), no message bodies/token leakage, and bounded/no recursive logging. Framework default error handling is not locally verified. |
| P1 — message cache / update regression | I; `349-359,891-1142` | Full vs partial update preserving absent fields, missing cache, embed-only update, unchanged update, single/bulk delete cache cleanup, at-limit insert/update, log-channel single delete. Assert no false edit and agreed eviction/retention behavior; do not assume arbitrary eviction is LRU. |
| P2 — persistence/query and event routing | I; `483-624,807-1974,2217-2248` | Under separate synthetic-DB authorization: add/replace/list/remove/lookup round-trip, multi-emoji same role, IDs above signed range, guild/channel scoping expectations. Offline dispatcher fixture enumerates registered commands/ignored interactions and intended intents; investigate ban dedupe and role audit action. No external command registration. |

## Policy decisions still owned by Ava

This audit approves none of the following: protected roles and role-grant eligibility; who may perform age verification; whether partial grants retain or compensate; private-channel/category exclusions and retention; staff visibility and ticket ownership/lifecycle; panel posting/provenance rules; one-open-ticket limits, numbering reuse and recovery semantics; age-document collection or storage. The age-ticket greeting currently requests an ID/picture/date of birth (`src/main.rs:2146-2153`), but is unreachable scaffold, not an approved data-handling procedure. No real identity documents or messages were examined.

## Suggested bounded follow-ups (not created by this audit)

1. **Implementation/build owner:** reconcile intended manifest/lock package name and formatting only; provide exact-head fmt/locked check/test evidence. Do not mix ticket activation into the repair.
2. **Directorate + Ava:** settle the enumerated role/privacy/ticket policy decisions in explicit acceptance contracts; F01/F02/F03 receive this audit as input, not gate release.
3. **Implementation specialist, then independent QA:** add offline mocks and pure-helper regression tests for reachable role/logger paths, starting with P0 partial failures and privacy. Keep implementation ownership separate from verification.
4. **Ticket implementation specialist, then independent QA:** implement provenance/authorization, atomic ownership/idempotency, and recovery under an approved policy before registering the scaffold. Add deterministic concurrency/failure fixtures, not live-server tests by default.
5. **Build owner:** make CI run explicit locked checks and meaningful behavioral tests with a chosen toolchain; test that zero local tests cannot be mistaken for feature acceptance.

## Delivery and remaining uncertainty

Only `docs/qa/BASELINE-TEST-GAPS.md` is authorized for the local report commit, using `git -c user.name=QA -c user.email=qa@local.invalid commit`. Final staging/commit gates must verify that this is the only added path and that every other tracked file still matches the tested baseline; exact commit and final verification results accompany the card handoff. No push or merge is authorized.

Offline check failures were recorded rather than repaired. No successful compilation, behavioral execution, CI run, Discord permission effect, ticket concurrency outcome, restart recovery, private-data protection, or production readiness was established. Source inspection identifies paths and risks; it is not a substitute for executable acceptance tests. Audit branch/report only is the rollback scope; production and the main working copy are untouched.

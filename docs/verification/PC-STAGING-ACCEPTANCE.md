# A13 — PC staging acceptance and rollback owner-review packet

## Scope and decision status

**REPORT ONLY / LIVE ACCEPTANCE NOT RUN.** This is preparation for Ava's return,
not staging authorization, deployment authorization, A07 approval, or a production
readiness verdict. Local test PASS does not approve production. Independent
`default` review is required for this report-only commit; owner approval is a
separate decision. No integration, merge, push, bot start, Discord request, command
registration, real ticket export, production migration, credential access, or
old-bot shutdown was performed.

Source snapshot: `74cbe30b2afd28bade3072acde914e2d60180eb9`, branch
`wt/skynet-staging-review-packet`. Initial HEAD matched exactly, tracked/untracked
status was clean, and root `.env` was **ABSENT**, checked by existence only.
References below are **this snapshot's line numbers**, not other branches' evidence.
Only this report is an authored tracked change. Tests and production files remain
unchanged. No configuration values, role IDs, tokens, or production DB names are
recorded here.

A07 has incomplete archive/recovery/privacy coverage on its separate branch per the
assignment. **Those checks remain pending independent acceptance.** This older
snapshot neither validates nor disproves that candidate. A12's parallel backup
utility is **not assumed accepted, integrated, or available here**. No sibling
worktree or private profile state was inspected.

## Owner decision table (markdown bullets)

- **Offline packet review — READY FOR INDEPENDENT REVIEW, not self-approved.**
  Check the exact report commit and the evidence section below; approve only the
  documentation's accuracy, not a live rollout.
- **Credentials, staging audience and live window — OWNER DECISION REQUIRED.**
  Provision a separate owner-approved application/guild, isolate access, approve
  intents and global command-registration side effects before any startup
  (`src/main.rs:2415–2448,2468–2472`). No credential value belongs in review evidence.
- **Age-role staging exercise — PENDING OWNER AUTHORIZATION / NOT RUN.**
  Configured staff authorizes the command, not ownership or Administrator alone;
  two grants are sequential, additive, and non-atomic (`src/main.rs:869–968`).
- **Panel-only demonstration — PENDING OWNER AUTHORIZATION / NOT RUN.** Actual
  guild owner may publish; configured staff alone may not. Opening remains
  hard-disabled (`src/tickets/policy.rs:53–67`; `src/tickets/runtime.rs:14–16,194–215`).
- **Ticket opening, per-ticket privacy, archive/close and recovery — HOLD.**
  Present fallback pauses general events globally while ticket lockdown is on;
  startup reconciliation and audited operator resolution are missing in this
  baseline (`src/tickets/mod.rs:6–16`; `docs/verification/TICKET-CREATION.md:51–62,96–100`).
  A07 must earn separate acceptance; no gate release is requested here.
- **Backup/restore and rollback — HOLD until independent proof and owner selection.**
  Persisted ticket state exists but does not constitute a backup or restore tool
  (`src/tickets/repository.rs:90–131`). Select a pinned, independently accepted
  prior release and compatible restored state; do not assume A12 supplies this.
- **Archive media/privacy/retention — UNSETTLED HERE.** This report grants no new
  permission to copy content, record verification, select retention/deletion
  deadlines, or change audiences. Existing guidance explicitly separates general
  logging policy from archive inclusion (`docs/qa/LOGGER-TEST-CONTRACT.md:69–73`).

## Implementation map: do not confuse these categories

| Category | Present evidence / boundary |
|---|---|
| Implemented, offline-testable | Staff membership and guild/role checks, ordered additive grants and truthful partial-failure strings (`src/main.rs:869–968`); actual-owner panel policy and explicit prospective private channel overwrites (`src/tickets/policy.rs:53–107`); persisted provenance, reservations, claim/attach and schema (`src/tickets/repository.rs:29–131`). |
| Disabled gate | Runtime `OPENING_APPROVED` is false; trusted button requests defer privately then report disabled before validate/reserve/create (`src/tickets/runtime.rs:14–16,181–191`; `src/tickets/controller.rs:59–67`). Passing mock tests with an approved controller argument do not enable runtime opening. |
| Conservative privacy fallback | Ticket-lockdown parsing is independent of successful ticket settings parsing (`src/config.rs:28–48`). General handler returns before its cache/log branches; only interaction and reaction events pass this application gate (`src/main.rs:1130–1148`; `src/tickets/mod.rs:6–16`). This is not per-category filtering, complete gateway/Serenity cache erasure, or suppression of every framework path. |
| Unimplemented at this baseline | Startup reconciliation, audited operator resolution, close/archive runtime: explicitly outstanding (`docs/verification/TICKET-CREATION.md:51–62,96–100`). A schema allowing `closed` is not an implemented close workflow (`src/tickets/repository.rs:103–106`). Archive URL/name primitives are unwired preparation, not a downloader (`docs/verification/ARCHIVE-INPUT-SAFETY.md:4–9,30–35`). |
| Unexecuted external checks | Discord effective permissions/visibility, command registration, actual HTTP outcomes, live restart/crash recovery, end-to-end archive/close and backup restoration: **NOT RUN**. Fixture construction is not archive execution (`docs/qa/TICKET-ARCHIVE-FIXTURES.md:32–34`). |

## Configuration inventory — names only

- Required configuration names: `DISCORD_TOKEN`, `LOG_CHANNEL_ID`, `AGE_ROLE_1`,
  `AGE_ROLE_2` (`src/config.rs:18–23,51–70`).
- Optional/configuration-gated names: `REACTION_ROLE_DB_PATH`, `STAFF_ROLE_ID`,
  `TICKETS_ENABLED`, `TICKET_GUILD_ID`, `TICKET_CATEGORY_ID`,
  `AGE_VERIFICATION_CATEGORY_ID`, `TICKET_DB_PATH`
  (`src/config.rs:24–48,72–75`). Ticket enabling requires the parser's exact
  affirmative sentinel, not arbitrary truthy text. Invalid/missing settings leave
  tickets unavailable; the separate lockdown flag can still pause general logging.
- Owner records only completion/validation status in the packet. Supply values
  through the owner's approved local secret mechanism, outside logs, screenshots,
  terminal history and Git. Do not paste environment dumps or token fragments.
  Do not start this bot merely to validate configuration: startup loads an override
  dotenv file, connects and globally registers commands (`src/main.rs:2415–2448,2468–2472`).

## Future owner acceptance sequence — every live step NOT RUN

All unchecked items below are proposals for a separately authorized, synthetic-only
staging window. Record executor, approval reference, exact release commit/artifact
identity, configuration-name validation status, case/result, and sanitized evidence
location. No real private messages, verification media, tokens or environment values
should be collected. Use consenting test accounts and invented content only.
An abort means stop the exercise and ask the owner; this report does not authorize
stopping an existing bot or changing Discord permissions.

### 1. Approve and isolate the window — NOT RUN

- [ ] Owner explicitly approves the staging application, guild, PC, test identities,
  audience, intents, permissions, command-registration effects, state locations,
  observers and recovery operator. Use separate staging credentials and owned
  synthetic state, not production databases or real tickets.
- [ ] Confirm the application is not installed in unintended guilds. Startup
  registers commands **globally**, not just in `TICKET_GUILD_ID`
  (`src/main.rs:2448`; conditional command list at `2406–2411`). Do not assume the
  ticket guild setting globally confines age/reaction commands or logger events.
- [ ] Preflight bot capabilities and role hierarchy using approved owner inspection;
  age command requires bot Manage Roles (`src/main.rs:916–920`). Ticket validation
  checks category guild/type, staff existence and its enumerated bot permissions
  (`src/tickets/runtime.rs:24–57`). Grant only owner-approved staging capabilities;
  do not use Administrator as a shortcut to permission acceptance.
- **Expected/evidence:** owner sign-off plus redacted application/guild isolation,
  approved capability matrix and release identity; no secrets or concrete config
  values in this report. **Abort:** wrong guild/application, unexpected installs,
  unintended global registration, ambiguous role mappings or missing authorization.
  **Decision point:** no startup until all prerequisites and rollback choice pass.

### 2. Verify authorization boundaries — NOT RUN

- [ ] With configured staff, test an ordinary staff caller lacking Manage Roles;
  invoke age verification on an owned staging member. Expect private defer/result
  and both intended role grants, not replacement of unrelated roles.
- [ ] Test an ordinary requester, nonstaff Manage Roles holder, nonstaff
  Administrator, and actual guild owner **without** configured staff. Age command
  must deny each with no role writes. Owner with configured staff is authorized
  **as staff**, not through an ownership bypass (`src/main.rs:874–901`).
- [ ] In isolated future runs, missing/invalid/nonexistent/everyone staff role,
  wrong-guild target, or invalid grant role must yield disabled/denied results and
  no role writes; do not alter production config (`src/config.rs:72–75`;
  `src/main.rs:874–901`). Existing command overrides may hide commands from valid
  staff; that is a failed visibility check, not evidence the backend authorizes
  nonstaff (`README.md:105–107`).
- **Expected/evidence:** sanitized caller-class × command matrix, private response
  observation, before/after synthetic member-role comparison and mutation audit.
  **Abort:** nonstaff grant, unexpected audience, lost unrelated role, or misleading
  success. **Decision point:** any boundary failure holds the rollout.

### 3. Verify paired grants and failure reporting — NOT RUN

- [ ] Test neither role, either role already present, both present, deliberate
  repeats, and either role manually removed then restored. Expect both requests
  in configured order; unrelated roles remain (`src/main.rs:903–913,954–957`;
  `README.md:92–103`). Owner selects distinct intended staging roles; this packet
  assigns no semantic meaning or invented IDs.
- [ ] In an expressly authorized disposable role-hierarchy exercise, make the first
  grant reject: expect first-failure notice and no second attempt. Then allow first
  but reject second: expect first-success/second-failure notice and **no automatic
  removal/rollback**. Inspect actual member state before any retry: failed HTTP
  response may still have applied a grant (`src/main.rs:903–910`).
- [ ] Keep transport timeout/reply-failure cases in controlled mocks unless the owner
  approves a safe injection method. Offline seam tests are not real HTTP delivery
  proof; never intentionally break production connectivity.
- **Expected/evidence:** ordered synthetic request trace where available, private
  response, state comparison after each case; full success only after both requests
  succeed. **Abort:** false full success, accidental revocation, blind retries after
  ambiguous state, or public error details. **Decision point:** hold and reconcile
  observed role state; software rollback does not undo granted roles.

### 4. Panel publication and disabled opening — NOT RUN

- [ ] With enabled/valid ticket settings, actual owner publishes in the configured
  guild's text channel. Expect owner-only validation, two labeled buttons,
  persisted provenance and a private confirmation that opening remains disabled
  (`src/tickets/runtime.rs:194–215`; `src/tickets/policy.rs:53–67`).
- [ ] Configured staff who is not actual owner, requester, wrong guild/channel kind
  must not successfully publish. Test copied/stale/untrusted panel separately:
  expect private rejection, not reservation/create (`src/tickets/controller.rs:47–67`;
  provenance storage `src/tickets/repository.rs:29–55`).
- [ ] Press both genuine buttons, including repeated presses as requester. Expect
  private disabled response and **zero validation/reservation/channel-create work**
  beyond provenance lookup. Compare owned staging DB rows/channel inventory before
  and after. Do not edit the source constant to obtain an opening demo.
- **Expected/evidence:** synthetic panel screenshot stripped of IDs, caller-class
  results and owned-state comparison. Unsaved panel after publication/storage
  failure must not be treated as trusted (`src/tickets/runtime.rs:210–215`).
  **Abort:** any channel opens, unexpected owner bypass, trusted copied panel, or
  unsanitized public response. **Decision point:** panel-only demonstration is not
  ticket-opening approval; A07 acceptance remains required.

### 5. Privacy exclusions and logger interaction — NOT RUN

- [ ] In lockdown, exercise synthetic message create/edit/single-delete/bulk-delete
  and channel create/update in prospective private support and age categories,
  including unknown channel metadata, then a normal staging channel. Expect the
  application's general logger/cache handler to be bypassed for **both** private
  and ordinary events, not selective successful normal logging
  (`src/tickets/mod.rs:6–16,21–54`; `src/main.rs:1132–1156`).
- [ ] Collect sanitized general-log absence and process-diagnostic observations;
  future instrumented acceptance should prove zero application cache writes/sink
  calls, not infer internal behavior from screenshots alone. Check channel names,
  topics, message text, embed JSON and attachment information for leakage. Reactions
  and interactions still pass the gate; inspect approved synthetic reaction-role
  behavior separately, do not label the fallback a universal privacy guarantee.
- [ ] Do **not** disable lockdown around private tickets merely to make the logger
  work. Without it, message create caches content and updates can copy text/embed
  JSON to logs (`src/main.rs:1150–1156,1380–1396,1454–1493`). Per-category exclusion
  is not supplied by this baseline. Future A07 must independently demonstrate
  private exclusions, unknown/restart cases and normal-event behavior together.
- **Expected/evidence:** synthetic sentinel absence across approved destinations,
  with event/case ledger and instrumentation limits stated. No real content exports.
  **Abort:** any private payload/name/topic in general logs, surprise cache/storage,
  or unacceptable loss of normal moderation logging. **Decision point:** hold rather
  than weakening privacy to regain observability; owner decides acceptable scope.

### 6. Restart, create uncertainty, close/archive — NOT RUN / OPENING HELD

- [ ] Before any future ungated build is accepted, require independently executed
  crash/restart tests across reserve, claim, remote create response, attach, greeting
  and feedback boundaries. Current repo supports durable slot/provenance state and
  a reopen regression, not startup reconciliation (`src/tickets/repository.rs:90–131,164–215`;
  `docs/verification/TICKET-CREATION.md:51–62`). Do not run these through this gated
  baseline and claim real ticket creation was exercised.
- [ ] Expected controller behavior in future approved tests: same slot/retry does
  not create duplicates; explicit known non-creation can fail/release the slot;
  uncertain create keeps it reserved; marker candidates are unverified, never
  auto-adopted even if exactly one matches. Attach precedes greeting; greeting
  failure leaves ownership active (`src/tickets/controller.rs:67–90`;
  `src/tickets/runtime.rs:154–163`). DB attach failure after remote success is also
  uncertain, not permission to delete or retry-create blindly.
- [ ] Require audited operator resolution and startup reconciliation before enabling
  opens. No normal manual DB editing workaround, automatic adoption/deletion or
  release of uncertain slots is accepted here
  (`docs/verification/TICKET-CREATION.md:51–59`).
- [ ] Close/archive acceptance remains pending A07: authorized staff/owner versus
  requester denial; complete paginated history and media integrity; safe URL,
  redirect, size and filesystem boundaries; duplicate close, send/export failures,
  partial archive, deletion failure and restart at each boundary. Expected future
  contract: no claimed successful close or destructive deletion before verified
  archive completion under separately owner-approved policy. This is a prerequisite,
  **not implemented proof or an approved retention/media policy**. Pure safety
  helpers and fixture passes do not satisfy it
  (`docs/verification/ARCHIVE-INPUT-SAFETY.md:4–9,30–35`;
  `docs/qa/TICKET-ARCHIVE-FIXTURES.md:32–34`).
- **Expected/evidence:** independently reviewed candidate commit, synthetic state
  transition/remote-operation ledger, restart crash-point results, archive manifests
  and integrity comparisons with approved access controls. **Abort:** orphan/duplicate
  channel, wrong ownership, unresolved uncertain state, leaked archive or destructive
  close before proof. **Decision point:** keep opening held until independent A07
  acceptance plus owner approval, irrespective of this baseline's green tests.

### 7. Backup/restore proof prerequisites — NOT RUN

- [ ] Inventory owner-approved state by configuration **name**, not guessed DB names:
  reaction-role store (`src/main.rs:713–735,2450`) and enabled ticket store
  (`src/main.rs:2451–2454`; schema `src/tickets/repository.rs:99–106`). Include
  separately approved archive assets/manifests if a future candidate adds them.
- [ ] Independently accept a backup method first; A12 is only a parallel candidate.
  Demonstrate SQLite-consistent snapshot semantics (including any active journal/WAL
  requirements), interrupted-copy behavior, access control, and refusal to overwrite
  the source. A raw live file copy is not sufficient proof.
- [ ] On owned synthetic files only, capture a manifest with tool/version, snapshot
  identity, byte hashes, schema/version and state totals; restore to a **different**
  owned location. Check integrity, panel provenance, slot uniqueness, interaction
  bindings, active/uncertain ticket identities, reaction mappings and any assets.
  Reopen with the exact intended release in offline harnesses; prove restored state
  remains consistent, including refusal of incompatible schema. Current ticket open
  rejects a different schema version (`src/tickets/repository.rs:99–108`).
- [ ] Require a documented reconciliation procedure for Discord changes since the
  snapshot. A DB restore cannot restore Discord channels/messages or undo role grants.
  Prove recovery without silently releasing uncertainty, inventing ownership or
  replaying creates. Define owner-approved recovery objectives and evidence handling;
  this packet supplies no invented retention/deletion deadline.
- **Expected/evidence:** independent backup review, synthetic restore transcript,
  integrity/comparison results, compatible release/state manifest and owner sign-off.
  **Abort:** untested restore, source overwrite, incompatible schema, missing assets,
  incomplete snapshot or unresolved external-state divergence. **Decision point:**
  no live acceptance depending on recovery until a restore has actually passed.

### 8. Safe rollback and single-enforcer handover — NOT RUN

- [ ] Owner selects and records the **exact pinned prior accepted release** (commit
  plus binary digest/build provenance), its compatible state snapshot, command
  surface, configuration-name inventory and approved scope. This baseline hash is
  an inspection anchor, not automatically that rollback release. No prior release
  is invented or selected by this packet.
- [ ] Before starting the candidate, identify every bot/service enforcing the same
  rule, including reaction/age automation and any restart supervisor. Obtain explicit
  owner authorization for the handover and a demonstrable single-active-enforcer
  plan. If old enforcement cannot be safely quiesced, **do not start candidate**.
- [ ] On an abort, authorized operator freezes new candidate work, drains or records
  in-flight/uncertain operations, stops the candidate and confirms no process,
  service, gateway session or automatic restart remains active. Preserve the
  approved recovery evidence. Only then consider restarting the pinned prior release.
  No overlap is allowed; if candidate shutdown is unconfirmed, leave prior stopped
  and escalate rather than run both.
- [ ] Restore only a proven compatible owned snapshot under the accepted procedure;
  never blindly downgrade an upgraded DB or point both versions at shared state.
  Reconcile remote channel/message/role mutations separately. Owner authorizes any
  command-registration restoration: startup globally re-registers commands
  (`src/main.rs:2448`), so binary rollback alone is not command-surface proof.
- [ ] Verify the pinned prior release is the sole enforcer, observe one approved
  synthetic smoke case, and obtain owner recovery confirmation. If state compatibility,
  credential isolation or uncertainty cannot be proven, remain stopped and escalate.
- **Expected/evidence:** approved cutover timeline, process/supervisor/session checks,
  release/state identities, redacted command-surface comparison and synthetic smoke
  result. **Abort/rollback triggers:** unauthorized role change, privacy leakage,
  duplicate/orphan create, false success, broken recovery/restore, wrong guild,
  unexpected registration, or unacceptable logging loss. These instructions are a
  future approval checklist, **not authorization to disable any existing bot now**.

## Fresh offline evidence

Run by QA in this worktree on 2026-11-17 against HEAD
`74cbe30b2afd28bade3072acde914e2d60180eb9` (branch
`wt/skynet-staging-review-packet`). Toolchain: `rustc 1.99.1
(58cf8f9c3 2026-10-13)`, `cargo 1.99.1 (5f94df478 2026-08-27)`. Every command
ran with `CARGO_BUILD_JOBS=1`, `CARGO_NET_OFFLINE=true` and an owned target
directory `target/a13-verify` (git-ignored, `.gitignore:9` `target/`). The bot
binary was **never started**; no network call, Discord request, command
registration, database open against a real path, or credential read occurred.
`Cargo.toml` and `Cargo.lock` were **not modified** (`git diff --stat HEAD --
Cargo.toml Cargo.lock` empty).

| # | Command | Exit | Observed result |
|---|---|---|---|
| 1 | `cargo fmt --all --check` | **0** | Clean; no diff output (log `target/a13-verify-fmt.log`, 0 bytes). |
| 2 | `cargo check --locked --offline --all-targets` | **0** | `Finished dev profile ... in 1m 21s`; **0 warnings**; 132 crates checked. |
| 3 | `cargo build --locked --offline` | **0** | `Finished dev profile ... in 0.22s` (reused check artifacts); **0 warnings**. |
| 4 | `cargo clippy --locked --offline --all-targets -- -D deprecated -D clippy::result_large_err -D clippy::type_complexity` | **0** | `Finished ... in 2.50s`; **0 warnings**. |
| 5 | `cargo test --locked --offline` | **0** | **73 passed; 0 failed; 0 ignored** across 5 binaries (below). |
| 6 | `git diff --check` | **0** | No whitespace errors. |

Test breakdown from `target/a13-verify-test.log` (each line
`N passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`):

| Test binary | Tests | Result |
|---|---|---|
| `unittests src\main.rs` | 28 | ok, 0 failed |
| `tests\archive_input_safety.rs` | 30 | ok, 0 failed |
| `tests\logger_fixture_shapes.rs` | 7 | ok, 0 failed |
| `tests\ticket_archive_fixture_factory.rs` | 5 | ok, 0 failed |
| `tests\ticket_fixture_boundaries.rs` | 3 | ok, 0 failed |
| **Total** | **73** | **73 passed, 0 failed** |

No `Doc-tests` section was emitted (no doctests in the crate).

The 28 unit tests map onto the claims made above and were individually
observed to pass: `age_nonstaff_with_manage_roles_is_denied_before_writes`,
`age_invalid_context_or_configuration_never_writes`,
`age_staff_grants_are_ordered_additive_and_repeatable`,
`age_failed_grants_report_progress_and_never_rollback`,
`age_command_metadata_allows_staff_without_manage_roles`
(`src/main.rs:199–336`);
`tickets::policy::tests::owner_only_and_config_validation`,
`tickets::policy::tests::private_payload_is_explicit_and_category_bound`
(`src/tickets/policy.rs:114–175`);
`tickets::controller::tests::untrusted_components_invalid_config_and_rollout_gate_never_create`,
`…::uncertain_operation_never_adopts_a_single_forgeable_match`,
`…::known_rejection_ack_validation_greeting_and_feedback_failures_are_truthful`,
`…::concurrent_same_slot_creates_once_but_other_slots_are_independent`,
`…::trusted_route_defers_before_work_and_retries_without_creation`
(`src/tickets/controller.rs:93–431`);
`tickets::repository::tests::panel_provenance_is_exact`,
`…::reservation_is_idempotent_and_slots_are_per_type_and_user`,
`…::claim_attach_and_known_failure_are_durable_and_concurrent`,
`…::separate_connections_and_restart_preserve_slots_and_opaque_operations`
(`src/tickets/repository.rs:160–297`);
`tickets::tests::actual_event_routing_fails_closed_without_channel_or_database_metadata`
— which contains the `const` assertion that `OPENING_APPROVED` is false
(`src/tickets/mod.rs:55–60`).

**What this evidence does and does not establish.** These are offline,
synthetic, in-process assertions. They establish that the reviewed source
compiles cleanly under the locked dependency set, that the enumerated
authorization/partial-failure/panel-provenance/state-machine boundaries hold
*as modelled by the unit-test doubles*, and that the conservative event gate
and the `OPENING_APPROVED == false` rollout gate are asserted by the code
itself. They establish **nothing** about Discord delivery, effective guild
permissions, global command registration, real HTTP outcomes, restart/crash
recovery, backup/restore, archive/close, or privacy compliance. Two specific
limits are worth stating plainly: the `controller` tests drive `route` with
`opening_approved = true` against a `Fake` Discord implementation
(`src/tickets/controller.rs:211,247,277,313,358,418`), so those green tests
exercise the controller logic but **not** the runtime gate; and the
`repository` restart test (`src/tickets/repository.rs:164–216`) reopens an
owned SQLite file and asserts durable slot/status/operation continuity, which is
**not** a crash-recovery or startup-reconciliation proof. Both boundaries are
called out again in §6.

One environment note, recorded as a limitation rather than a result: the
offline dependency cache satisfied every command above, so **no check was
skipped for lack of a cached crate**. The one thing the offline cache cannot
supply is a live Discord dependency — the `serenity`/`poise` crates are
present as source, but no live API behaviour is reachable offline.

## Remaining acceptance limits

Live-only steps above are all **NOT RUN**. No archive export/close, real restart,
backup/restore rehearsal, production database, Discord permission or real-network
failure check has been performed by A13. Existing docs' earlier baseline counts
and author RED/GREEN claims are context, not this run's independent results.
Passing fixtures prove fixtures; passing mocked controllers prove those exercised
boundaries, not Discord delivery, complete A07 coverage, or policy approval.

This report must be reviewed at its exact local commit by `default`. Owner approval,
future live authorization, A07 acceptance, A12 acceptance and any integration remain
separate gates. Rollback of this report-only change itself requires no runtime or
state action because A13 did not deploy or change any running service.

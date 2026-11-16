# A06 — Ticket routing and durable creation (T01/T02 slice)

Baseline `65e2fc62a4d77970629394ac467f8a3c4d51e542`, branch `wt/skynet-ticket-creation`.
Authored by Alexandra (initial implementation, quota-interrupted) and completed by
Cybersec (owner-authorized handoff). No live Discord, bot start, command registration,
real `.env`, tokens or production DB were used.

## Status: runtime opening is HARD-GATED

`tickets::runtime::OPENING_APPROVED = false` (a `const`; a test asserts it stays false).
With tickets enabled the bot posts the panel (owner-only) and recognises/defers
button presses, but replies privately that opening is disabled and makes **zero**
validation/reservation/create calls. Gate release belongs to A07 and requires:

1. **Privacy completion.** Current safe fallback: when `TICKETS_ENABLED=true`,
   `tickets::allow_general_events` drops *all* general logger/cache event handling
   (message, update, delete, bulk delete, channel create/update) before any cache,
   topic/name or embed-file work. Only InteractionCreate/reaction events pass.
   This is conservative (it also silences the general logger while enabled) and is
   NOT a per-category guard; A07 must replace it with category/ticket-ID classification.
2. **Audited operator recovery.** Startup reconciliation before accepting opens and a
   mutating, audited operator-resolution command do not exist yet (see Recovery).

## Architecture (src/tickets/)

| Module | Contents |
|---|---|
| `policy.rs` | `Settings{guild,staff,support_category,age_category,db_path}` + `valid()` (staff ≠ @everyone/guild id), `Kind{Support,Age}`, `owner_can_publish`, exact private permission payload builder, `marker()` opaque channel-topic marker, greetings (no ID/picture/DOB upload request; live camera-on VC) |
| `repository.rs` | `Repository` over tokio-rusqlite; versioned schema; `save_panel/trusted_panel`, `reserve`, `claim`, `attach`, `failed`, `greeting_result` |
| `controller.rs` | `route(settings, repo, request, io: &impl Discord, opening_approved)`; `Discord` trait = the only I/O boundary (`defer_private, feedback_private, validate, create, channels, greet`) |
| `runtime.rs` | Serenity adapter: `/ticket-panel` owner-only command, InteractionCreate wiring, channel creation with explicit overwrites, metadata listing |
| `mod.rs` | `allow_general_events` privacy fallback |

Config (`src/config.rs`): `TICKETS_ENABLED` (default false), `TICKET_GUILD_ID`,
`STAFF_ROLE_ID` (existing), `TICKET_CATEGORY_ID`, `AGE_VERIFICATION_CATEGORY_ID`,
`TICKET_DB_PATH`. Ticket settings are only parsed when enabled; malformed values
while off, or while on (→ tickets disabled-invalid), never affect logger/age/reaction
features. Commands list the panel command only when enabled+valid. DB file created
only when enabled. See `.env.example`.

Behavior summary: panel provenance persisted (guild/channel/message/bot author) and
checked exactly; defer (ephemeral) happens before provenance/DB/HTTP work; SQLite
reserve is transactional with partial unique index over all nonterminal states per
guild/user/kind (support/age independent); interaction retry returns same operation;
`claim` is a one-shot transition so only one worker may call create; channel attached
before greeting; greeting failure keeps ownership (`greeting=failed`); NotCreated
failure releases the slot; Unknown outcome keeps status `creating` (slot reserved,
never retried). Replies are fixed sanitized strings with operation ids only. The
legacy `is_age`-from-custom-id/channel-name handler in `main.rs` was removed.

## Recovery limits (non-destructive)

* Unknown create outcome: slot stays reserved. A retry reports unverified metadata
  candidates (channel ids in the approved guild+category with the operation marker and
  matching private overwrites) but **never auto-adopts** — one forged match is treated
  like two or zero (test `uncertain_operation_never_adopts_a_single_forgeable_match`).
* No auto-attach, no auto-delete, no auto-release. Resolution requires an operator
  command that does not exist yet: **pre-live gate (A07)**. Normal workflow must not be manual DB edits.
* Startup reconciliation before accepting opens: not implemented (opening is gated).
* Next archive seam: `Repository` ticket rows (`status`, `channel`) are the single
  ownership source; archive/close should add terminal statuses via new repository
  methods and a staff/owner-only controller path, reusing `Discord` boundary.

## Verification (CARGO_BUILD_JOBS=4, CARGO_TARGET_DIR=target, `--locked --offline`)

| Command | Exit | Result |
|---|---|---|
| baseline `cargo test` at 65e2fc6 (run by author) | 0 | 21 passed (14 bin + 7 fixtures) |
| `cargo fmt --check` | 0 | clean |
| `cargo check --locked --offline` | 0 | no warnings |
| `cargo build --locked --offline` | 0 | no warnings |
| `cargo test --locked --offline` | 0 | 28 unit + 7 fixture = 35 passed (21 baseline tests retained, no baseline assertion altered; 14 new) |
| `cargo clippy --locked --offline --all-targets -- -D deprecated -D clippy::result_large_err -D clippy::type_complexity` | 0 | no warnings |

Cleanups in the Cybersec pass: removed dead-code warning (candidate channel ids are now
reported in the recovery message) and a constant-assert clippy warning (const block).
Dependencies, Cargo.lock, CI unchanged.

RED/GREEN evidence (author logs, git-ignored `a06-*-red.log` / `a06-*-green.log` in the
worktree root, recorded by Alexandra before handoff): policy, repository, state,
controller, config, runtime, restart, recovery, stale-panel and forged-match slices.
Each red log shows the failing run before implementation, each green log the passing run.
Cybersec did not re-observe red for these slices; QA should treat them as author evidence.

## Test map

policy: owner-only publish, config validation, exact private payload; repository:
panel provenance exactness, reservation idempotency/per-type/per-user, concurrent
claim/attach with separate connections, restart reopen, opaque operation ids;
controller: untrusted inputs/invalid config/gate → no create, defer-before-work and
retry idempotency, concurrent same-slot (one create) + independent slots, truthful
ack/validate/reject/greeting/feedback failures, forged single match; mod: general
event routing fails closed on actual `FullEvent`s; main: command registration only
with valid enabled config, cached-permission/cache-miss policy.

## Known gaps for QA/A07

Per-category privacy guard (current fallback is global pause), startup reconciliation,
operator recovery command, plausible-creation-time corroboration, and close/archive.
No numeric ticket-number display beyond the durable integer `id` (monotonic AUTOINCREMENT).

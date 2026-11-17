# A07 — Verified ticket archives, safe closure, privacy and recovery

Baseline `e42ccb4f4e1cd3c6713e007dbf45e444ed03cc94`, branch `wt/skynet-ticket-archive-close`.
Author: Cybersec (`cyber-tiel-aic`). No live Discord, real token, bot start, command
registration, network, `.env`, production DB or private-ticket export was used.

## Gate status (read first)

* `tickets::runtime::OPENING_APPROVED = false` — unchanged. `main.rs` asserts it in a
  `const { assert!(..) }` block. Startup reconciliation reports and gates only
  (`Report::opening_gate(OPENING_APPROVED)`); flipping the flag is a separate owner decision.
* `TICKETS_ENABLED` stays default off. With tickets disabled, existing non-ticket behaviour is unchanged.
* **Not live-ready.** The Serenity adapter (freeze/probe/delete/DM/history) and `HttpDownloader`
  are compiled and wired but exercised only through fakes at the Discord/network boundary.
  T03/live readiness is NOT claimed.

## Modules (src/tickets/)

| Module | Role |
|---|---|
| `repository_close` | v2 migration, `NONTERMINAL` state list, CAS claims (`claim_close`, `advance(id, token, Step, now, lease)`), `record_archive(id, token, ArchiveRecord, now)`, history, audit (actor/reason), dm status |
| `privacy` | fail-closed private-scope classifier replacing the global event pause |
| `archive` | page-to-exhaustion history, A10 validators, no-redirect HTTPS downloader, owned root, staged publish + atomic rename + reverify |
| `closer` | authorization, close state machine, startup reconcile, audited recovery |
| `runtime_close` | Serenity adapter, `/ticket-close`, `/ticket-recover`, `startup_reconcile` |

## State machine and migration

`active -> closing -> archiving -> verified -> delete_requested -> (delete_uncertain) -> closed`;
`recovery` holds slots needing an operator. Nonterminal (slot-holding) states:
`reserved, creating, active, closing, archiving, verified, delete_requested, delete_uncertain, recovery`.
Only `failed`, `closed`, `released` free the unique per-guild/user/type slot.
Schema v1 -> v2 migrates owned DBs preserving tickets, IDs, operations, panels, interaction
linkage and sequences; the status CHECK and the partial unique index cover every nonterminal
state. No DB transaction or cache guard is held across awaited HTTP.

Delete is requested only after the archive is reopened and bytes/hashes/lengths/IDs reverify.
Any page/download/timeout/disk/publication error leaves the source untouched and the slot held.
Closed/released is set only on confirmed deletion or reconciled known absence after a verified archive.

## Privacy

`privacy::decide` returns Allow only for known unrelated channels / non-channel events.
Skip (no cache insert, body, embed, topic or asset work) for: durable ticket channels, configured
ticket categories (current and old/new on moves), unknown potentially-private target-guild
channels, creation-before-attach, and any failed DB/metadata lookup (fail closed).
Round-1 review fix: ThreadCreate/Update/Delete now call `repo.channel_known` on every payload
parent, so a thread under a durable ticket channel is Skipped (lookup error also Skips).
Test: `thread_events_under_a_durable_ticket_channel_are_private`. Runtime wiring is proven by the
`runtime_event_handler_calls` test.

## Recovery instructions (operator)

`/ticket-status`-style output is non-destructive. `/ticket-recover` attach/release requires
confirmation, a reason, an audited actor and expected guild/category/type/private-overwrite and
ownership checks; names/topics/nonces are corroboration only and a single candidate is never
auto-adopted. Manual DB editing is not part of the recovery path. Archives are kept; automatic
deletion is OFF.

## Dependencies added (exact edges)

`Cargo.toml`:
* `reqwest = { version = "0.12.28", default-features = false, features = ["rustls-tls"] }`
* `ring = "0.17.14"`

`Cargo.lock` vs baseline: the root `discord-moderation-hub` package gains the dependency edges
`"reqwest"` and `"ring"`, and the package entry is re-sorted (moved earlier in the file). Earlier
handoff text "no Cargo.lock change" was wrong; the lock does change. `--locked --offline` builds
succeed against the committed lock.

## Verification (run with `CARGO_BUILD_JOBS=2`, `--locked --offline`)

| Check | Result |
|---|---|
| `cargo fmt --check` | exit 0 |
| `cargo clippy --locked --offline --all-targets -- -D warnings -D deprecated -D clippy::result_large_err -D clippy::type_complexity` | exit 0, zero warnings |
| `cargo test --locked --offline` | 65 bin + 30 `archive_input_safety` + 7 `logger_fixture_shapes` + 5 `ticket_archive_fixture_factory`, all pass |
| `cargo build --locked --offline` | ok |

RED/GREEN attribution: the thread-privacy test was written first against `decide()` that checked only
configured categories and would have returned Allow (RED); after the `channel_known` parent loop it
returns Skip (GREEN). Other A07 tests (v1->v2 migration, CAS across connections, closer/reconcile/
recovery, archive/downloader fakes) were written with their modules in commits 92aaece..5002645.
The A09 fixture factory is reused via one `#[cfg(test)]` module (`tickets::fixtures`), not duplicated.

## Limits and assumptions

* Discord has no atomic snapshot and administrators bypass overwrites: the requester freeze is
  best effort and a stability probe aborts/reconciles on detected change.
* Filesystem safety = owned archive root, create-new files, generated ID-only names, staging dir +
  atomic rename, byte reverify. Concurrent same-account tampering inside the owned root is NOT
  defended against; this is not claimed to be race-proof.
* Retrieval is the recorded archive reference plus reverify; browse/restore/retention is A08.

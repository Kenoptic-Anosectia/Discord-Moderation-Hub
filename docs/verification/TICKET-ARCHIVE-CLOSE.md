# A07 — Verified ticket archives, safe closure, privacy and recovery

Baseline `e42ccb4f4e1cd3c6713e007dbf45e444ed03cc94`, branch `wt/skynet-ticket-archive-close`.
Author: Cybersec (`cyber-tiel-aic`). No live Discord, real token, bot start, command
registration, network, `.env`, production DB or private-ticket export was used.

## Gate status (read first)

* `tickets::runtime::OPENING_APPROVED = false` — unchanged. `main.rs` asserts it in a
  `const { assert!(..) }` block. Startup reconciliation records readiness (`main.rs` ~2524-2525)
  and the runtime route enforces it through `runtime::opening_permitted()` (reconcile report
  clean AND `OPENING_APPROVED`); `Report::opening_gate` is only the pure helper beneath it.
  Flipping the flag is a separate owner decision.
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
Test: `thread_events_under_a_durable_ticket_channel_are_private`.

Runtime wiring evidence (executed, not structural): `main.rs::event_handler` routes every event
through `privacy::guarded(.., &COUNTERS, || log_event(..))`; all cache insertion/log output lives
inside that sink. Tests `guarded_seam_performs_zero_sink_writes_for_private_unknown_and_error_cases`
and `guarded_seam_runs_sink_exactly_once_for_public_and_disabled` call the real `guarded` with an
injected sink and assert 0 sink executions + `skipped` counter for durable ticket ids, private
category, thread-under-ticket, unknown/contradictory lookups, unprovable moves, DB error and
missing config, and exactly one execution + `allowed` counter for public/disabled/other-guild/
non-channel events. The source-order test `runtime_event_handler_calls_the_guard_before_any_cache_insertion`
is a SECONDARY structural check only and is not counted as behavioural evidence. Limit: the
actual serenity `log_event` body is not run (needs a live Context); the seam guarantees it is not
reached.

## Commands, setup and recovery (operator)

* Archive root: `ticket-archives/` next to the configured ticket DB file (`db_path` parent),
  created if absent; each archive is `ticket-<id>-<seq>/` with `manifest.json`, transcript and
  assets. Defaults: 200,000 messages, 500 assets, 25 MiB/asset, 512 MiB total, 60 s download
  timeout. Retention/purge is OFF (A08).
* `/ticket-close` — run INSIDE the ticket channel by configured staff or the actual guild owner.
* `/ticket-recover ticket_id action [channel] [confirm] [reason]`, same authorization, ephemeral:
  * `status` — read-only state hint.
  * `retrieve` — read-only: re-verifies the recorded archive (manifest hash, transcript, assets,
    IDs) and replies with archive name, counts and manifest sha256 only (no content/paths/export).
    Works after channel deletion. Authorization is checked before any archive read; requesters,
    admins without the staff role, and foreign-guild callers get a fixed denial. Authorization
    precedes the archive read in code; the test `retrieval_is_staff_or_owner_only_...` tampers the
    archive and shows denied callers see only the denial text — a tamper-oracle INFERENCE of zero
    reads, not a counted read proof (see Round 5 limits). Full browse/restore is A08.
  * `attach` (needs `confirm=true`, `reason`) — link a candidate channel to a reserved/creating/
    recovery ticket only if guild, category, type and private overwrite match.
  * `release` (needs `confirm=true`, `reason`) — frees the slot of a reserved/creating/recovery
    ticket that has NO linked channel and no matching remote candidate; audited with actor and
    reason. Refused for any channel-linked ticket.
* Startup reconcile reports and holds slots: active tickets with an Unknown probe and any
  reserved/creating/recovery row count as unresolved; opening requires `OPENING_APPROVED`
  (false) AND a clean report (`runtime::permitted(approved)`, tested for approval true/false with
  clean and Unknown-probe reports).
* Stale publish-before-record orphans: if a previous attempt published `ticket-<id>-<seq>` and
  died before recording, the retry adopts it only if it fully re-verifies AND matches the current
  source fingerprint; otherwise it is PRESERVED untouched and the close publishes under the next
  free sequence (bounded to 32 orphans, then retained with the slot held). Deletion only follows
  a freshly verified, recorded archive plus the pre-delete stability check. Tests:
  `published_but_stale_unrecorded_archives_are_preserved_and_close_recovers` (two orphans),
  `stale_orphan_then_failed_delete_retry_never_deletes_against_stale_archive`.
* Reserved-row recovery: `reserved` rows are unresolved at startup and resolvable ONLY by the
  confirmed, reasoned, staff/owner, audited `release`/`attach` above. Tests:
  `reserved_row_is_resolved_only_by_confirmed_audited_operator_action` and the repository test
  covering `recovery_release` of a reserved row.
* Manual DB editing is not part of any recovery path. Archives are kept; automatic deletion is OFF.

## Downloader failure-path evidence

`HttpDownloader<T: Transport>` keeps the production policy (https-only, no redirects, status,
declared/streamed size caps, short body, overall deadline) in `fetch`; `ReqwestTransport` is the
only network piece. Tests in `archive_tests::http_policy` run the real `HttpDownloader::fetch`
over a scripted in-process transport (no network): success, non-2xx, redirect (not followed),
timeout/hang, oversize declared and streamed. The older string-contains builder test is secondary.
The real `ReqwestTransport` and Serenity adapter are still not exercised against a live server.

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
| `cargo test --locked --offline` | HISTORICAL (round 4, 84 bin): superseded — see the Round 6 section for the final-at-head counts |
| `cargo build --locked --offline`, `cargo check --locked --offline` | exit 0 |
| `git diff --check` | clean |

(Historical table: round 4. Earlier rounds: 65, 71, 72 bin; round 5: 91 bin. Final counts: Round 6 section.)
RED attribution: the thread-privacy RED was executed. For the round-4 items, new tests were
written alongside/after the code in the same slice; the stale-orphan test replaces the previous
assertion that such an orphan yields `Retained("exists")` (behaviour intentionally changed to
recovery). RED runs were not separately captured for them and are not claimed.

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
* Retrieval is the recorded archive reference plus reverify (`/ticket-recover retrieve`);
  no content export, browse, restore or retention (A08).
* Not covered: live Discord, real network, the serenity `log_event` body, the runtime slash
  command handlers themselves (logic is tested at `closer::*`; command metadata is checked in
  `main.rs`). Orphan skipping is bounded (32); beyond that the close is retained, not forced.

## Round 5 (same card; fixes for the round-4 BLOCK)

Executed with `CARGO_BUILD_JOBS=2 ... --locked --offline`. Final run: `cargo fmt --all -- --check`
exit 0; `cargo check` exit 0; `cargo build` exit 0; `cargo test` exit 0 with **91 bin + 30 safety
+ 7 logger + 5 factory** tests passing; all-targets Clippy with
`-D warnings -D deprecated -D clippy::result_large_err -D clippy::type_complexity` exit 0.
The round-4 "Gate status" claim above still holds: the Serenity adapter is NOT exercised live;
the real `HttpDownloader` policy is now also exercised *through the close controller*, but only
over a fake transport (no network).

| # | Defect | Fix (production code) | Regression (production controller) |
|---|---|---|---|
| 1 | Two independent `delete_uncertain` presence probes; Unknown/Absent skipped source validation | ONE classified probe: Unknown -> retain at once, Absent -> reconcile only, Present -> mandatory `source_unchanged` before any retry/delete (`closer.rs`) | `changing_presence_probes_never_delete_a_stale_or_unchecked_source` (Unknown->Present, Absent->Present, Present+edit/delete/append) |
| 2 | Foreign/unsupported-format orphan adopted | `identity_ok` (format==1, ticket, channel) enforced at orphan adoption, `verify_recorded`, `recorded_manifest` (reverify, reconcile, retrieval) + asset-count check at adoption | `foreign_or_unsupported_orphan_is_preserved_never_adopted_and_fresh_archive_is_published` (wrong ticket, wrong channel, format 2; orphan bytes preserved, fresh seq 2 published, identity mismatch rejected on reverify) |
| 3 | Fingerprint omitted embed URL/footer, author, attachment type | `content_fingerprint` now covers author name/bot flag, attachment content_type, embed url + footer | `footer_only_or_url_only_edit_is_detected_during_download_and_on_retry` (stale archive preserved, no delete until a fresh matching one is published) |

RED (new tests against the round-4 production code, i.e. fixes reverted, only the signature shim
kept so tests compile): `3 failed; 20 passed` — `foreign_or_unsupported_orphan_...`,
`footer_only_or_url_only_...`, `changing_presence_probes_...` all FAILED. GREEN: same tests pass
with the fixes (full suite counts above).

Coverage added (these passed on first run; they add evidence for behaviour that was already
correct, they are not RED/GREEN fixes):
* `privacy::tests::guarded_sink_for_invite_voice_reaction_events_private_unknown_error_public_member`:
  production `guarded` seam with an injected sink counter — InviteCreate/Delete, VoiceStateUpdate
  (old-only, new-only and both endpoints), ReactionAdd/Remove: private, unknown and DB-error
  cases give zero sink writes; public gives exactly one; a member event gives one and no lookup.
* `closer::tests::real_downloader::*`: a real `CloseIo` test adapter whose `Downloader` is the
  actual `HttpDownloader` over `fake_transport`, with real SQLite and files and a fake Discord:
  3xx, 404, 500, timeout, transport failure, oversize and short body each give
  `Retained`, zero source deletes, status `archiving`, held slot, no archive recorded; a 200
  control closes with exactly one delete.
* `restart_after_record_but_before_verified_...`: crash window between `record_archive` and the
  `verified` transition. Accurate claim: the recorded-but-unverified archive is preserved
  untouched, a fresh independently verified archive (seq 2) is published and only then is the
  source deleted once. The controller does NOT adopt the earlier recorded archive in that state.
* Retrieval denial: the existing test uses a tamper oracle (denied callers see only the denial
  text, whereas an authorized caller sees the integrity failure on the same corrupted archive).
  This is an inference of zero reads, NOT a read-counter or fault-boundary proof; no such counter
  exists. Do not cite it as a counted zero-read proof.

Still not covered / limits: no live Discord or network; the member-event control only covers
`GuildMemberRemoval`; the `Hang` timeout case relies on a 200 ms test deadline.

## Round 6 (final-at-head counts)

* Orphan adoption now compares the COMPLETE expected asset inventory (`m.assets == expected`:
  attachment id, message id, generated file, len/declared size, sha256, display name) derived from
  the current source, not just the count. A same-count orphan with a wrong attachment id, message
  association, display name or file name is preserved untouched and a fresh seq N+1 archive is
  published, verified and recorded before the single delete. An exact-inventory orphan is adopted.
  RED (before fix): `orphan_with_same_asset_count_but_wrong_identity_is_never_adopted` failed
  (`left: ticket-1-1, right: ticket-1-2`).
* `recorded_manifest` (shared by close, reconcile and retrieval) now also requires
  `manifest.assets.len() == recorded assets`. Tests `recorded_asset_count_mismatch_fails_closed_for_{close,reconcile,retrieval}`
  each failed first (close -> Closed; reconcile closed 1; retrieval "Integrity re-verified: OK"), now
  pass. Recorded manifests/hashes are never modified.
* The `#[allow(clippy::ptr_arg)]` suppressions are removed; slice helpers are adapted to the
  `Mutation` fn pointer with const closures (`EDIT_FIRST`, `EDIT_FOOTER`, `EDIT_URL`).
* Final: fmt/check/build exit 0; tests 96 bin + 30 + 7 + 5 pass; strict all-target Clippy clean.
  `OPENING_APPROVED` stays false; `TICKETS_ENABLED` default off.

## Round 7 — remaining clippy suppressions removed

- `runtime.rs`: `mod readiness_tests` moved to the end of the file; `#[allow(clippy::items_after_test_module)]` removed.
- `closer_tests.rs`: `active_ticket_with_unknown_probe_is_unresolved_and_keeps_opening_closed` now builds both reconcile reports (Unknown, clean) first and takes the synchronous `READINESS_LOCK` only around the synchronous readiness assertions, never across an `.await`; `#[allow(clippy::await_holding_lock)]` removed. Assertions unchanged (synthetic approval true/false for unknown and clean reports).
- RED: with the allow removed and the test unchanged, Clippy failed `await_holding_lock` at closer_tests.rs:837. GREEN after restructure.
- Gates: fmt/check/build exit 0; tests 96 + 30 + 7 + 5 pass; strict all-target Clippy exit 0; `grep allow(clippy src tests)` returns nothing. OPENING_APPROVED stays false.

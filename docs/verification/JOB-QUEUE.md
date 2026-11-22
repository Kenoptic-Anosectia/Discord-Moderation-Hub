# JOB-QUEUE - durable job queue core (K02)

`src/jobs/{mod,queue}.rs`: synchronous, Discord-free queue in the `jobs` table of the shared `core.sqlite` (feature `"jobs"`, created by `queue::init` through `storage::migrate`). Every call takes the connection and `now` (unix seconds); the module reads no clock and uses no randomness.

API: `init`, `schedule(&NewJob, now) -> Created{id} | Duplicate{id,state}`, `claim_due(now, limit) -> ClaimBatch{claimed, payload_failed}`, `try_claim(id, now)` (the CAS), `complete`, `fail(id, code, now, &RetryPolicy) -> Retry | Failed`, `get` (view without payload), `reconcile_after_restart(now, &KindTable, &RetryPolicy) -> Report`.

## Rules enforced
1. Dedupe: `dedupe_key` is UNIQUE. A key that exists in ANY state (also done, failed, abandoned) returns `Duplicate` and writes nothing; the first kind, payload and due time stay. A later recurrence needs a new key. This module has no DELETE.
2. Claim: one `UPDATE .. WHERE id=? AND state='pending' AND due_at<=now`; the winner is the caller whose statement changed one row.
3. Retry: delay after claim n = `min(cap, base*2^(n-1))` (default 30 s / 600 s); the job is `failed` once `max_attempts` (default 5) claims are used. The decision uses the per-job `attempts` count, not `last_error_code` (diagnostic only). An invalid error code is stored as `invalid_error_code`.
4. Restart: `RunAgain` -> `pending`, due now (or `failed` when attempts are already at the max, so a crash loop ends). `AtMostOnce` and any kind missing from the caller's table -> `abandoned`, listed in the `Report`. Duplicate table entries for one kind: any `AtMostOnce` wins. Call it before workers claim (there is no lease).
5. Payload: a JSON object of at most 4096 bytes. Oversize is refused at `schedule`; any other invalid payload is stored, then `failed` with `payload_malformed` at claim. A stored `kind` that is not a plain token (rows written around the API, incl. non-UTF-8 text) is `failed` with `kind_malformed` at claim, and at reconcile is abandoned and reported as `invalid_kind`. `claim_due`, `reconcile_after_restart` and `get` read these TEXT columns as bytes, so one such row fails/abandons only itself. Payload text is not in `QueueError`, `Debug` of `ClaimBatch`/`ClaimedJob`/`JobView`, or the `Report`.
6. `kind` is `[a-z0-9_]{1,64}`, `dedupe_key` printable ASCII 1..=128 bytes, negative times are refused. Errors are stable `QueueError` codes without SQL text.

## Tests (40 unit tests in `queue.rs`, scratch DBs under `target/k02-tests/`)
Schema/CHECK, due ordering and boundary, limit, dedupe (all terminal states, cross-kind), two connections on one file (deterministic CAS, plus 4 threads claiming 60 jobs with no overlap), complete/fail refusals, doubling and capped backoff, extreme attempts and `i64::MAX` clock, malformed/oversize payloads, hostile kind/key/code spellings, reconcile (RunAgain, AtMostOnce, unregistered, max attempts, clock behind), reopen of the file, injected clock, error-code leaks, and a liveness test: 10 seeded LCG runs of 250 random steps (schedule, claim, complete, fail, crash+reopen+reconcile) against a model worker, then a drain, asserting every job ends terminal, crashed at-most-once jobs never run again, and every abandonment was reported.
RED: `4b8157f` (stubs + 35 tests): 35 failed, 0 passed. Two bugs in my own tests were fixed afterwards (a clock not advanced past the backoff; a claim at `i64::MAX` that also took a pending job); no assertion was weakened.
RED round 2: `a2eeddd` added 5 tests after review (non-UTF-8 payload/kind at claim, bad kind at reconcile, `get` on such rows, `try_claim` not-yet-due): 4 failed, 1 passed (the due gate already existed; that test only guards it, checked by removing `AND due_at <= ?2`, which fails it).
Mutations tried, each made tests fail: CAS without `state='pending'`, `<=` to `<` on due, no backoff cap, `>=` to `>` on max attempts, RunAgain arm swapped, dedupe `DO UPDATE`, try_claim without the due gate, reconcile without the kind-token filter.

## Config keys
None read here. The wiring card reads `CORE_DB_PATH` (see CORE-STORAGE.md): unset, empty or reserved => the feature layer stays OFF and no jobs table is created.

## NOT done
No runner, timers, commands, config parsing or wiring (`#![allow(dead_code)]` stays until W1). No lease or heartbeat: after reconcile and re-claim, a stale `fail` or `complete` from the earlier run cannot be told apart and would act on the new run. No pruning of old rows. The restart `Report` is in memory only: if the process dies after reconcile committed `abandoned` but before the report is surfaced, a second reconcile does not list it (the row keeps its `restart_*` code; there is no list-abandoned API yet). Concurrency is tested only with several connections to one file in one process.

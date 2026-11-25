# JOB-RETRY — terminal failures and Flash retries (KJ3)

Scope: the durable job runner and the two Flash delete kinds. Safeword and Bump are unchanged.

- `HandlerError::new(code)` still retries. `terminal(code)` skips all further attempts and writes
  `failed` unless an opted-in job has an unresolved effect. Codes are static tokens; no Discord
  ids, names or message text travel in errors or alerts. Reconcile only changes `running` jobs.
- `Registry::register_with_retry` overrides the default for one kind, including startup reconcile;
  using the existing `RetryPolicy` keeps `register` and `RunnerConfig::retry` unchanged (5 attempts,
  30 s base, 600 s cap by default). Claims lost to a crash count toward the limit.
- Both Flash kinds use `RunAgain` and 3 claims (30 s then 60 s backoff). A known 403 is terminal
  (`delete_forbidden`); a 404 (including unknown message/channel) is already done and is not itself
  an alert, as in FL2. Invalid Flash payloads that reach the handler are terminal (`payload_invalid`).
- `retry_after(code, seconds)` stores the later of normal backoff and the supplied delay, in queue
  clock seconds. A pending Retry-After deadline survives a runner restart. The Serenity adapter
  keeps its existing REST rate limiter; the queue does not extract a delay from a residual 429.
- Flash opts into `track_uncertainty`: a 5xx, transport failure or interrupted claim may have
  deleted the message. An idempotent retry can confirm via success or 404. If still unresolved
  when the chain stops, store `abandoned` / `job_outcome_unknown`, not `failed`. A later refusal
  cannot disprove the earlier effect. A capped crash is in the existing abandonment report.
- `observe_final` runs after a final SQLite acknowledgement so retry attempts and repeated startup
  reconcile do not repost alerts. A known 403 alerts `flash_delete_forbidden`; an exhausted or
  unresolved delete alerts `flash_delete_failed`. Recovery also alerts once if its last recorded
  diagnostic was a known transient, preserving FL2's existing recovered-failure alert.

## Evidence
RED `bbcf9c0f55bb1fc4e51e37ec4931e2412952fe63`: Flash runtime suite, 43 passed / 3 failed:
- `a_403_ends_as_a_failed_job_with_one_staff_alert`: pending instead of failed after one call.
- `a_persistent_transient_delete_alerts_once_at_its_own_cap`: alert during the first retry.
- `liveness_random_flashes_faults_and_crashes_always_end_deleted_or_failed_with_an_alert`:
  seed 1 produced 24 transient alerts instead of 18 chains.

Replaced the old `a_403_ends_as_a_failed_job_with_a_staff_alert_per_attempt` assertions
`src.attempts == 3` and `forbidden == 3`: a refusal is not retryable. The replacement checks one
call, one alert, stable stored code, and two restarts without rearming. Replaced the seeded
`DeleteFailed` count equalling `transient_scripted`: count once per chain, not once per attempt.
The seeded test uses the real runner to drain due jobs and models pre-handler crashes via the
legacy queue reconcile API, which clears the diagnostic if the crash interrupts recovery.

Other coverage: unchanged 404 assertions, persistent transient cap and give-up timing, rate-limit
boundary past the backoff cap, five-attempt unrelated kind, invalid policy, per-kind crash cap,
and earlier uncertainty surviving a later refusal.
Mutation: `terminal` temporarily behaved like `new`; the real 403 test failed (pending, not
failed), then the change was reverted.

## Wiring and limits
No new keys or schema. Wiring owns existing keys: `FLASH_MODE` (off), `FLASH_CHANNEL_IDS`
(empty/inactive), `FLASH_PING_ROLE_ID` (unset/no mention), `FLASH_DELAY_SECS` (180), and
`FLASH_INCLUDE_THREADS` (true). Malformed values leave Flash off (see FLASH-RUNTIME.md for
validation, guild and DB prerequisites). Invalid retry policy is a registration error, not a
fallback. No command, feature enabled, registration in `main.rs`, or live Discord test. The
cap is on queue claims under a fixed policy, not Serenity's internal HTTP attempts.

Alerts are best effort, not an outbox: a crash after commit but before a send may lose it; an
interrupted recovery may lose its previous diagnostic. An awaited final observer can delay the
runner and shutdown by up to `handler_timeout` per yielding callback. On startup, wiring must
surface the abandonment report; the runtime observer is not replayed for such rows.

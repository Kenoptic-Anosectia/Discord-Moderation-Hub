# BUMP-RUNTIME - bump reminder runtime (BU2)

## Authority and delivery

- BU1 detection still requires a trusted listing-bot confirmation in the configured bump channel.
  `/bump status` and staff-only `/bump reset` fail closed outside the configured home guild and do
  not name it. A reset preserves the last confirmation ID. Shadow never writes, schedules or sends;
  its reset reply explicitly says nothing changed. Off is inert.
- The SQLite `bump_state` row is authoritative for the cycle, `next_due_at`, and `reminded`.
  A confirmation or reset opens a new cycle. Rearm persists any due-time change before scheduling;
  status and dispatch use the stored due time, not a fresh calculation. `last_bumper` stores the
  confirmation message ID, not a member ID.
- Jobs are disposable wake-ups. A job for another guild, another cycle or a different due time is
  a no-op. The handler checks that the row is due, then atomically marks it for the matching guild,
  cycle and due time only if unmarked. The mark commits before sending. Send failure or a crash
  after the mark may lose delivery, but does not authorize another attempt. The text is fixed and
  only the configured bump role may be mentioned.
- Wake keys are `bump:<guild>:<cycle>:<due>:<wake_gen>`. Migration v2 defaults existing rows to
  generation zero. The adapter must preserve the queue's duplicate state: pending/running means
  `AlreadyScheduled`; done/failed/abandoned does not. For a terminal duplicate, persist the next
  generation and retry scheduling once. Only a new job reports `Scheduled`; another terminal
  collision or a refused write reports `queue_refused`. No cycle or a marked cycle needs no wake.

## Wiring contract (W3)

1. Serialize bump observe, reset, rearm and reminder dispatch per guild through one owner or a
   shared lock. The mutable repository borrow guards one instance, not independent connections.
   Commit row writes before queue operations; do not hold a SQLite write transaction across a queue
   write on a separate connection to the same database or across a Discord send.
2. At startup, reconcile the queue with `KIND_TABLE`, then run bump `rearm` before any worker claims.
   Reconciliation abandons interrupted `AtMostOnce` jobs; rearm replaces an abandoned wake only if
   the current row is still unmarked. Applying new intervals before claims prevents old wakes from
   acting on a superseded due time.
3. On `queue_refused`, raise a staff/operational alert using the stable code and retry rearm with
   backoff. Don't rely on redelivery or a later process restart. The row retains the owed reminder
   and any advanced generation. Redelivered confirmations can also repair the missing wake.

W3 owns production adapters, command registration, workers, alerts and retry integration. BU2 has
not launched the bot, posted to Discord, or run a production migration.

W3 supplies the home guild separately and reads `BUMP_MODE`, `BUMP_CHANNEL_ID`, `BUMP_ROLE_ID`,
`BUMP_BOT_ID`, `BUMP_INTERVAL_SECS` and `BUMP_SUCCESS_MARKER`. Mode defaults to off, interval to
7200 seconds (allowed 600–86400), and marker to `Bump done`. Invalid values name keys, not values.

## Offline verification

Round 3 RED: `cargo test --locked --offline round3_` exited 101, 0 passed and 6 failed before the
runtime fixes. Both restart cases reported `AlreadyScheduled` for terminal wakes; a foreign job
sent the home reminder; status used the unapplied interval; a v1 row failed to load; a second
mark incorrectly succeeded. The two restart cases now run at the start of the real-queue liveness
sweep (40 seeds x 200 steps). They reopen the same SQLite file, and the sweep requires each owed
reminder to be attempted on a healthy tick and all remaining debt to drain. The expectation does
not depend on pre-checking whether a wake-up is pending. Synthetic channels only.

Round 3 GREEN: fmt check, locked/offline all-targets check, full test suite (1310 passed), and
locked/offline all-targets Clippy with warnings, deprecations, `result_large_err` and
`type_complexity` denied. Cargo build jobs were limited to two.

Round 2 evidence is retained in the history of this file; the channel/home-guild guards, truthful
shadow replies and conditional mark remain covered by their existing tests.

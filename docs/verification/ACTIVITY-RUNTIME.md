# ACTIVITY-RUNTIME (AC2)

Activity stores UTC-day message counts and voice seconds, not message content. The repository
migrates only its own tables in shared `core.sqlite`. IDs are decimal strings to preserve the
full `u64` range. The segment column admits only decimal digits, minus signs and delimiters;
its parser validates the format and the ledger validates the ordered, non-overlapping stretches.

Only the configured nonzero home guild is in scope. Webhooks (including any repeated text),
bot messages and disallowed channels do not count. AC1 decides voice credit, including AFK,
self-deafening, moves and UTC midnight splits. The AC1 mismatch documentation is corrected to
match the approved leave/move behavior. Diagnostics have stable codes, not SQL or user data.

The runtime batches totals and writes open snapshots at a shared 30-second flush/heartbeat
cadence. One transaction applies the totals and replaces the guild's open rows, so a failed
flush can retry without double-applying totals. Clean shutdown closes sessions and flushes.
On restart, AC2 consumes the saved rows in a transaction with the recovered totals. It drops
invalid rows and closes valid ones no later than each row's `last_seen_at` or restart time.
One shared cadence keeps a closed session's totals and removal from the open set atomic.

With a healthy clock, on-time ticks and successful writes, a crash can lose up to one
flush/heartbeat interval of unflushed counts and segment detail. Restart credits no more
than the persisted heartbeat allows, not the outage. Scheduler stalls or repeated write
failures can lengthen the loss window. If a session exceeds 1,024 segments, the checkpoint
omits and reports it rather than storing a partial history that could miscredit time; the
live ledger is unchanged. Shadow does not persist activity.

## Wiring contract and config

W5 supplies one runtime owner, the injected clock and the diagnostic sink. Call `tick` at
startup before dispatch and on a periodic timer even with no events; retry write errors with
backoff. Stop dispatch before calling `shutdown`; await success before closing the database.
The caller must resolve the current privacy scope for channel, parent and category, including
all log exclusions, flash and ticket channels, and deny unknown lookups. This applies at
startup reconciliation too. Live Discord privacy adapters, the timer and alert routing are
not wired here. Use one writer; independent runtimes are not supported.

- `ACTIVITY_MODE`: `off` by default; unknown mode, absent home or zero home leaves it off.
- `ACTIVITY_EXCLUDED_CHANNEL_IDS`: empty by default; a malformed entry refuses the list.
- `ACTIVITY_AFK_CHANNEL_ID`: unset by default; a non-singleton or invalid ID refuses config.
- `ACTIVITY_COUNT_SELF_DEAFENED`: `false` by default; nonblank values must be `true` or `false`.

The parser accepts an injected lookup. Refusals name only the key; leave the feature off.
No environment read, command, registration, production migration or call from `main.rs`.
Owner decision D13 (including privacy sign-off) is required before activation.

## Offline evidence

RED `d31a94a`: `timeout 300 cargo test --locked --offline activity::` returned 101:
9 failed, 88 passed. Failures: config; snapshot round trip and validation; schema; exclusions
and shutdown; heartbeat/deafening/midnight; transaction retry; corrupt-row recovery; seeded
SQLite checkpoint/restart model. The missing-home/no-store test passed against the scaffold.
The implementation commit was written after RED but was not runtime-verified in this turn.

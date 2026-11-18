# Archive Retention Policy (A15)

This module implements **pure parsing, eligibility checking, and purge planning** for optional
ticket-archive retention. It does **not** delete archives or read configuration — those are
future integration concerns.

**Owner policy:** retention is **off by default**. If the key `TICKET_ARCHIVE_RETENTION_DAYS`
is unset (or parses to `Disabled`), no archive is ever purged. The key accepts values
`1..=36500` (days); anything else is rejected.

- **Disabled:** `eligible` always returns `false`; `plan_purge` always returns `[]`.
- **AfterDays(N):** An archive is eligible when `now >= closed_at + N * 86400` (exact boundary
  → eligible; one second before → not). `closed_at` is the Unix seconds of **confirmed closure**
  (not tentative); `None` or a future timestamp → never eligible.

This is pure logic only — no filesystem, database, wall-clock reads, or wiring to config
or deletion pipelines.

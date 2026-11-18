# Archive Retention Policy (A15)

## Purpose

This module implements optional automatic deletion of ticket archives based on a configurable
retention period. The policy is **off by default** — if no retention period is configured,
archives are **never** purged automatically.

## Configuration

The retention period is set via the configuration key `TICKET_ARCHIVE_RETENTION_DAYS`.
Its value must be an integer in the range `1..=36500` (1 day to 100 years).
Any other value is rejected as invalid.

## Behaviour

- **Disabled (default / unset):** No archives are ever purged.
- **AfterDays(N):** An archive becomes eligible for purge exactly `N` days (N × 86400 seconds)
  after its confirmed closure timestamp (`closed_at`). The boundary is inclusive — an archive
  at exactly `closed_at + N*86400` is eligible; one second before is not.
- Archives with no confirmed closure (`closed_at == None`) are never purged.
- Archives whose `closed_at` is in the future are never purged.

## Scope

This is **pure logic only** — no filesystem access, no database queries, no wall-clock reads,
and no wiring to configuration or deletion pipelines. A future integration card will wire
this module into the bot's archive management workflow.

## Boundary rule

```
eligible  iff  now >= closed_at + days * 86400
```

Exact boundary → eligible. `now == closed_at + days * 86400 - 1` → not eligible.

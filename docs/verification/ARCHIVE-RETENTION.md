# Archive Retention Policy (A15)

This module implements optional automatic deletion of ticket archives based on a configurable
retention period. The policy is **off by default** — if no retention period is configured,
archives are **never** purged automatically.

The retention period is set via the key `TICKET_ARCHIVE_RETENTION_DAYS` (value `1..=36500`).
Any other value is rejected as invalid.

- **Disabled (unset):** No archives are ever purged.
- **AfterDays(N):** Eligible when `now >= closed_at + N * 86400`. Exact boundary → eligible;
  one second before → not eligible. `closed_at == None` or future → never eligible.

This is **pure logic only** — no filesystem, database, wall-clock reads, or wiring to config
or deletion pipelines. A future integration card will wire this into the bot's workflow.

# Safeword runtime (SW3)
`src/safeword/runtime.rs` + `config.rs`: reaction handlers on the SW1 machine and SW2 store. Discord,
DMs, alerts, store, sleeper and reaction users are small traits in `Ops`. Feature **not registered**.
## Rules (each tested)
- Emoji matched by id; other channel/message/emoji and bot reactors ignored. The machine's own
  snapshot is stored, so a replayed add cannot overwrite it. `off` does nothing; `shadow` writes
  nothing and counts per decision (a release counts its own effects, not an activation's).
- Every grant gets a K03 decision on `GrantPath::SafewordRestore` (K03 has no activation path, so
  the safeword-role add reuses it) and is written by the K04 executor (429 retry, held roles skipped).
- **DM rule:** a failed DM (closed, rate limited, other) raises `dm_failed`, then is *acknowledged*
  to the machine, so Active/Inactive is always reached; the outcome stays `Partial`, never success.
  No member is trapped by a DM they cannot receive.
- **Partial role failure** keeps SW1's `Activating` (the plan says "stays Active"): the honest state,
  pending steps are retried by reconcile, `partial_role_failure` alerts, never success, never released.
- **Bots:** bot members are ignored on add events; reconcile reads a bot flag from the reaction-user
  list and skips bots (a bot still moves the page cursor). Limit: a removal carries no member, so a
  bot's removal looks like a user's; harmless, a bot is never Active so it releases nobody (tested).
- Reconcile: pages of 100 (cap reported, not guessed); runs each member until settled (16 rounds).
## Tests and RED evidence
58 runtime + 15 config tests. RED commits `be7ba1e`, `da7dc60`, `2c543f9`; round 2 `cea2241` (7 failed:
closed-DM release, DM kinds, DM reconcile, bot skip, bot cursor, shadow release, and the liveness run
with permanent DM failures among 300 seeded rounds).
## Config keys (read by the wiring card, not here)
`SAFEWORD_MODE`: default off; any unset or unrecognised value is off (known limit: a typo is silent).
`_ROLE_ID`, `_CHANNEL_ID`, `_MESSAGE_ID`, `_EMOJI_ID`: one decimal id, no default; bad refuses, naming
the key. `_RESTRICTED_ROLE_IDS`: id list. `_DM`, `_DEFER_GRANTS`: `on`/`off`, default `on`.
## NOT done
Not in `main.rs`, no command, nothing on, no `.env.example`. No jobs queue: durability is the SW2 step
state plus reconcile; a crash between a DM and recording it can repeat that DM (at-least-once).

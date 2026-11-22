# BUMP-RUNTIME - bump reminder runtime (BU2)

`src/bump/{config,repository,runtime}.rs`: the runtime around the pure BU1 policy. Config parses the `BUMP_*` keys, the repository keeps the `bump_state` row, the runtime detects a bump, persists the cycle, schedules one reminder per cycle and holds the `/bump status` and staff-only `/bump reset` logic. Discord and the job queue sit behind the `ReminderChannel` and `JobQueue` traits, so the tests inject fakes.

## Rules the code enforces (each has a test)
1. A message is a bump only when the policy says so (not a webhook, the configured guild, the configured listing bot, a bot account, a real id newer than the last one, the success marker in the text or embed). A user, a webhook or a cooldown reply never counts and never changes the stored cycle.
2. A confirmed bump starts a cycle: the row is written (`cycle` grows by one, `reminded` cleared) and the reminder job for that cycle is scheduled.
3. One job per cycle: the dedupe key is `bump:<guild>:<cycle>`, so the queue's UNIQUE key refuses every repeat. The kind `bump_reminder` is `AtMostOnce`.
4. The reminder is posted once per cycle: the cycle is marked `reminded` BEFORE the post, so a crash between mark and post cannot post twice. A failed post is reported by code (`send_failed`) and the cycle stays marked.
5. A restart before the reminder is due finds the same job and posts once; a restart after it was sent finds `reminded` set and does nothing.
6. Only the bump role may be mentioned (`<@&id>`); the text is fixed wording, never a body.
7. `/bump reset` is staff-only; a non-staff caller changes nothing. A staff reset starts a new cycle from now, keeps the last confirmed message id (a replay cannot undo it) and gets its own job.
8. `shadow` writes nothing, sends nothing and schedules nothing; `would_do` returns counts and stable codes only. `off` does nothing. A refused config value names the KEY, never the value, and the feature stays off.

## Tested (61 new unit tests: 15 config, 13 repository, 33 runtime)
Detection and hostile shapes; the cooldown reply; the stored row; one job per cycle (also against the real queue's UNIQUE key); one reminder per cycle across a restart; the mentions; the staff-only reset; a replay after a reset; shadow and off; the config parse and its safe failures; the schema and a corrupt row; a seeded LCG liveness sweep (40 seeds x 200 steps) against a model runtime that bumps, ignores, resets, restarts and fails, then drains. RED evidence: commit `47b5baf` (types, migration, tests; logic stubbed) ran the new tests: 48 failed, 43 passed. Mutations tried, each made a test fail: mark the cycle reminded AFTER the post (2); a dedupe key that does not name the cycle (4); any caller may reset (1); shadow no longer blocks the post (1, after that test was added because the first shadow test passed for the wrong reason); shadow writes the row and schedules the job (2); a refused config value that does not switch the feature off (3); the due check that always says "due" (5); the stored `reminded` column that is not persisted (5).

## Config keys (READ by the wiring card W3, not by this module)
`BUMP_MODE` (default `off`; unset or unknown -> off, an unknown value names the key) | `BUMP_CHANNEL_ID`, `BUMP_ROLE_ID`, `BUMP_BOT_ID` (default unset; unset, zero or not exactly one id -> the key is named and the feature is off) | `BUMP_INTERVAL_SECS` (default `7200`; not digits or outside 600..=86400 -> the key is named) | `BUMP_SUCCESS_MARKER` (default `Bump done`; blank, over 200 bytes or a control char -> the key is named).

## NOT done
No poise command, no feature registered, not called from `main.rs`, no `.env.example` entry, no Discord call in production, no staff-alert post, no serenity implementation of `ReminderChannel`/`JobQueue` (the wiring card writes them), and `bump_state` is created only when the wiring card opens the core database. `last_bumper` holds the confirming message id: the plan names the column but not its meaning, and the runtime cannot know which member bumped (the reply comes from the listing bot).

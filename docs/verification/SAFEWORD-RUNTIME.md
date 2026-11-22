# Safeword runtime (SW3)

`src/safeword/runtime.rs` (thin Discord-facing layer) and `src/safeword/config.rs` (parser). Discord, the store, the DM sender, the alerter, the sleeper and the reaction-user list sit behind small traits in `Ops`; tests inject fakes. `SafewordFeature` implements the registry `Feature` hook but is **not registered**.

## Rules the code enforces (each has a test)
- Emoji matched by id only; other channel, message or emoji, and a bot reactor, are ignored. Reaction events are fed to the SW1 machine; the runtime stores the machine's own snapshot, so a replayed add cannot overwrite it; remove-before-add ends Inactive.
- Every grant (safeword role, restores) gets a K03 decision on `GrantPath::SafewordRestore` and is written by the K04 executor (`grant::apply`): only allowed, not-yet-held roles, 429 retried 3 times via the injected sleeper. Removals use `decide_removal`. `Ops.protected_role_ids` reaches both K03 and the SW1 restore filter.
- A partial role failure is never a success (`Outcome::Partial`): the step stays unacknowledged in the machine's `Activating`/`Releasing` state (retried on the next event or reconcile) and raises `partial_role_failure`. The plan says "stays Active"; SW1's approved machine keeps the state `Activating` until every step is acknowledged, and the safeword role is still added (tested).
- A DM failure alerts and never touches roles or snapshot. Alerts carry stable codes only.
- A failed state or role lookup at the start of a run writes nothing. Held roles are read at write time; a re-add during a release refreshes them before the hand-over is acknowledged.
- `off` does nothing (not even a read); `shadow` performs no write, DM, alert or store write and returns counts and codes (events and reconcile).
- Reconcile reads reaction users in pages of 100 (page cap is an argument; hitting it is reported, not guessed), reads stored members, and runs each member until the machine has nothing left (max 16 rounds).

## Tests and RED evidence
51 runtime + 15 config tests; whole suite 1,285 passed. RED commits: `be7ba1e` (first tests, coder-3; not re-run by me), `da7dc60` (7 tests failed: no executor retry, no already-held skip, protected roles ignored, one-action reconcile, non-converging liveness run), `2c543f9` (2 failed: off and shadow reconcile still wrote). Liveness: 300 seeded rounds with random role/DM failures that must converge; it caught a stale held-roles bug. 19 mutations were each caught by at least one test (off and shadow reconcile, emoji compare, bot flag, shadow, partial=success, 429 mapping, sleeper, page cap, K03 path, protected set, fresh held roles, refresh, lookup ack, mixed codes, rounds, facts-unavailable, held-lookup fallback).

## Config keys (read by the wiring card, not here)
`SAFEWORD_MODE` (default off; unknown value is off). `SAFEWORD_ROLE_ID`, `_CHANNEL_ID`, `_MESSAGE_ID`, `_EMOJI_ID`: required single plain decimal ids, no default; missing or bad refuses the whole config (the error names the key). `SAFEWORD_RESTRICTED_ROLE_IDS`: id list via K06, one bad entry refuses the list. `SAFEWORD_DM`, `SAFEWORD_DEFER_GRANTS`: `on`/`off`, default `on`, any other token refuses (SW1's doc said "fails closed to true"; the card rule wins). `PROTECTED_ROLE_IDS` is passed in `Ops`, not read here.

## NOT done
Not called from `main.rs`, not registered, no command, nothing switched on; no `.env.example` entries. No jobs-queue use: durability is the SW2 row plus the reconcile. The reaction bot flag is read from the event's member, so it is false when Discord omits it. A 429 `retry_after` hint is not carried to the executor. `defer_grants` is parsed and handed to the machine; no grant path calls it yet. The `MAX_ACTIONS_PER_EVENT` guard has no test.

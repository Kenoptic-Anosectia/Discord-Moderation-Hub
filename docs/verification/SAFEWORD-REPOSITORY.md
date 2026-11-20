# Safeword Repository (SW2)

`src/safeword/repository.rs` stores the SW1 `State` per (guild, user) in `safeword_state` inside the core database (`storage::open` + `storage::migrate(.., "safeword", ..)`). API: `Repository::{open,new,load,store,clear}`. Ids are stored as decimal text. `state.rs` is untouched: a private DTO maps `Snapshot` to JSON (`struck_json`/`deferred_json` hold those two lists, `snapshot_json` the rest).

## Rules Enforced (each has a test)
- **Compare-and-swap:** `store`/`clear` run in one `BEGIN IMMEDIATE` transaction and apply only if the stored version equals `expected_version` (`0` = no row). Otherwise `RepoError::Stale`, no write. Two connections racing on one file: exactly one wins (20 rounds each for create and update).
- **Whole snapshot round-trips:** every `Snapshot` field, including `failed_steps`, `restore_in_flight`, `last_emitted_restore_plan`, `last_failure`. Checked on every state of real `step` sequences and on 40 seeded random event walks. `u64::MAX` ids survive.
- **Corrupt data is an error, not a reset:** bad JSON, wrong types, unknown or duplicate keys, a missing core field, an unknown `state` text (exact match only), a non-integer version, or an `inactive` row with data. `load`, `store` and `clear` all refuse such a row and leave it byte-identical (a corrupt row is never overwritten; repair is manual).
- **Older snapshots:** only `failed_steps`, `restore_in_flight`, `last_emitted_restore_plan` may be missing; they default to empty/false. Any other missing field is an error.
- **Restart:** after dropping the connection and reopening the file, `restart_action` returns the same result for every stored state, both reaction values.
- **Errors** are `RepoError` codes (`stale_version`, `corrupt_snapshot`, `unknown_state`, `corrupt_row`, `invalid_version`, plus storage codes); none carries SQL, JSON or ids.
- **`clear` keeps the row** (state `inactive`, empty data, version bumped) so a writer holding an old version, or "no row", cannot write over a cleared member. Clearing a member with no row and version `0` writes nothing.

## Tested and RED Evidence
- RED commit `853fc01`: tests against a stub; 19 of 20 `safeword::repository::tests::*` failed (the one pass was the migration test, which the stub already did), then GREEN with the implementation (20 pass).
- Also tested: stale/future/zero versions, unrepresentable versions, member isolation, `updated_at`, newer stored schema refused, reserved path refused, and a crash/retry runtime model (40 seeds, random failures, reaction toggles, reopen at random) that must end `Inactive` and never go quiet with unacknowledged work.
- Mutations that turned tests red: dropping the version check and the `WHERE version` guard (6 failed), dropping `failed_steps` on write (5), unknown state mapped to `Inactive` (2), skipping the unreadable-row check (3), `Deferred` instead of `Immediate` transactions (2), allowing unknown JSON keys (1).

## Config Keys
None read here. The wiring card passes `CORE_DB_PATH` (core database file) to `Repository::open`; any error from `open` should fail closed (feature stays off).

## What Is NOT Done
No runtime, no commands, no wiring: nothing calls this module yet (`#![allow(dead_code)]` in `safeword/mod.rs` stays until W1). Role changes and DMs are SW3. The 5 s busy timeout (from `storage::open`) is not exercised by a test: a lock held longer than that would surface as `sqlite_error`, which callers must treat as "retry or fail closed", not as `Stale`.

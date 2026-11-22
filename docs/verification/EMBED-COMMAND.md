# EMBED-COMMAND - `/embed post|sync|list` logic (EM2)
`command.rs`: async `post`/`sync`/`list` over an `Env` of small traits, pure reply builders.
`config.rs`: lookup-closure parser. Built on EM1 (`posts::plan`, `validate_key`); not wired.
Rules enforced (each has a test):
- Owner only; staff only with `EMBED_PUBLISHERS=staff`; caller 0 denied; mode `off` = `disabled`.
- One home guild (commands are registered globally): only `Env.home_guild` is served. Another guild,
  its owner and staff too = `denied (other_guild)`; home guild unset or 0 with the mode on =
  `denied (no_home_guild)`; guild 0 = `invalid_target`. Nothing is sent or read in these cases.
- `list` and sync-all report only the calling guild's rows; a foreign row's template is not read and
  the row is not reported or named. The store still loads every row, so one corrupt row of ANY guild
  makes the home guild's list and sync-all `store_failed` (fails closed, nothing leaks).
- Keys pass `validate_key` first; a non-plain spelling is `invalid_key`, never echoed.
- Unchanged `post` = no-op; changed = EDIT; `sync` never creates; recreate needs the explicit flag.
  A send whose record write fails is deleted; if that delete fails: `posted_not_recorded` + alert.
- One call per key at a time (`KeyLocks`, `busy`). Replies are ephemeral: codes and key names only.
  Alerts carry the code only. Embeds send with empty allowed mentions. No job kind is registered.
- `shadow`: no send, edit, delete, write or alert; returns `would_*` codes and counts.
Tests: 70 new, 1,237 unit + 52 integration pass: hostile keys, auth, idempotent sync, two-guild
denial and filter, failures with the alert count checked after each call, shadow, real SQLite,
seeded liveness. RED `0965e90` (57 of 61 failed), round 2 `017ba3e` (7 of 9). Mutants: 49/49 caught.
Config keys (read by the wiring card; stage in `.env.example`):
- `EMBED_MODE`: `off` (default) | `shadow` | `live`; a typo = off. (Added: rules need a mode.)
- `EMBED_TEMPLATE_DIR`: unset = off; with the mode on, missing, control chars or >1024 bytes = off.
- `EMBED_PUBLISHERS`: `owner` (default) | `staff`; any other value = off. Errors name the key only.
NOT done: no command, no feature registered or on, no `main.rs`/`config.rs` change. Limits: Serenity
Discord and SerenityAlerts have never run live. sync-all has no per-call timeout: a hung Discord
call stalls the run and holds that key's lock. `KeyLocks` is per process. If an edit lands but its
write is lost and the template returns to the older version, the post stays on the newer one (EM1).
WIRING MUST pass `GUILD_ID` as `home_guild` (unset = all denied), call `TokioStore::migrate`,
share one `KeyLocks`, and compute `Invoker.guild_owner` for the calling guild.

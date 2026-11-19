# CONFIG-HARDENING

Card SF3 (audit MEDIUM/LOW), base `13a1764`. All three findings reproduced.

**1. Env precedence — FIXED.** `main.rs` used `dotenvy::dotenv_override()`, so a stale
`.env` beat an exported variable. `config::load_env()` now loads the nearest `.env` with
`dotenvy::from_path`, so the real environment wins. Test
`env_file_does_not_override_the_real_environment` exports
`SF3_REAL_ENV_SENTINEL=from_environment` while the file sets `from_file`; after the load
the exported value must survive and a file-only variable must still load. RED: `left:
Ok("from_file")`, `right: Ok("from_environment")`. Behaviour change: anyone who relied on
`.env` overriding the environment is affected; Ava's no-exports setup is not.
**2. `Config` Debug leak — FIXED.** `Config` derived `Debug`, so `{:?}` printed
`discord_token` verbatim. A manual impl prints `<redacted>` for the token, all other fields
unchanged. Test `config_debug_output_never_contains_the_token` checks the token never
appears and non-secret values still do. RED: derived output printed
`discord_token: "synthetic-token-must-never-be-printed"`.
**3. `LOG_CHANNEL_ID=0` — FIXED.** Read with `required_u64`, so `0` was accepted. Now
`required_discord_id`: a Discord id is a non-zero u64, so `0`, negatives and non-numeric
values are rejected and the configuration does not load — the existing safe failure, the
bot refuses to start. Test `log_channel_id_must_be_a_valid_discord_id` runs a child process
per case (`0`, `-1`, `not-a-number`, unset, valid id) and loads only for the valid id. RED:
the `0` case loaded. A rejected value fails with `ConfigValueError { key, code }` using the
stable `ConfigValueCode::InvalidDiscordId`, never the value itself.
**NOT fixed.** `AGE_ROLE_1`, `AGE_ROLE_2`, `STAFF_ROLE_ID` still accept `0` (out of scope).
The token is still a plain `String` cloned into `main`; only `Debug` is redacted. `Config`
has no `Serialize`. The precedence test covers the loader, not a full start-up. A missing
variable keeps the existing missing-variable failure. `.env.example` not edited; proposed
line: `# The real environment wins over this file: an exported value is used as-is.
LOG_CHANNEL_ID must be a real Discord id; 0 is rejected.`

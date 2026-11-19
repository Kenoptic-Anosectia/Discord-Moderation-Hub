# CONFIG-HARDENING
Card SF3 (audit MEDIUM/LOW), base `13a1764`. All three findings reproduced.

**1. Env precedence — FIXED.** `main.rs` used `dotenvy::dotenv_override()`, so a stale `.env` beat an exported variable. 
`config::load_env()` now calls `dotenvy::dotenv()`: the real environment wins, only variables it lacks come from the file. Test 
`env_file_does_not_override_the_real_environment` exports `SF3_REAL_ENV_SENTINEL=from_environment`, the file sets `from_file`; the exported 
value survives, a file-only variable loads. RED: `left: Ok("from_file")` vs `right: Ok("from_environment")`. Behaviour change for anyone 
who relied on `.env` overriding the environment; Ava is unaffected.
**2. `Config` Debug leak — FIXED.** `Config` derived `Debug`, so `{:?}` printed `discord_token` verbatim. A manual impl prints 
`<redacted>` for the token, keeps every other field, and takes the struct apart without a rest pattern, so a new field must be listed and 
decided. Test `config_debug_output_never_contains_the_token` checks the token never appears, non-secret values do too; another test checks 
no field is dropped. RED: it printed the token.
**3. `LOG_CHANNEL_ID=0` — FIXED.** Read with `required_u64`, so `0` was accepted. Now `required_discord_id`: a Discord id is a non-zero 
u64, so `0`, negatives and non-numeric values are rejected, the configuration does not load and the bot refuses to start. Test 
`log_channel_id_must_be_a_valid_discord_id` runs a child process per case (`0`, `-1`, `not-a-number`, unset, valid) and loads only for the 
valid one. RED: the `0` case loaded. Rejections fail with `ConfigValueError { key, code }` and the stable 
`ConfigValueCode::InvalidDiscordId`, never the value.
**1b. Search delegated — FIXED.** The hand written `find_env_file` walk used `Path::is_file`, which answers `false` for every metadata 
error, so it walked up and loaded another `.env`. It is deleted; `dotenvy::dotenv()` does the search and stops on a non-`NotFound` error. 
The search tests drive a child process in its own working directory: the loader returns the file dotenvy finds; a broken nearest file is an 
error.
**Limit of the round-2 evidence.** They also pass against the old `is_file` walk, so RED commit `3fdc7d2` is green: they cannot tell the 
old walk from `dotenvy::dotenv()` here. The `PermissionDenied` fail-open path was verified only by a reviewer's out-of-tree ACL probe, not 
by a repo test; round 2 is structural hardening whose only in-repo evidence is the round-1 RED.
**NOT fixed / limits.** A malformed `.env` line still lets the variables *before* it be loaded (verified: `BEFORE=loaded`, `AFTER=none` and 
a bad third line leaves both set, then errors); pre-existing dotenvy behaviour, and its error text never reaches output (`env_file_error` 
maps it to `Unreadable`). `AGE_ROLE_1`/`AGE_ROLE_2` still load `0`, but `0` is refused at use by `valid_role`; `STAFF_ROLE_ID` `0` is 
filtered by `parse_staff_role` and rejected by `Settings::valid`, so `0` is harmless. `+123`, leading zeros and ids above `i64::MAX` are 
still accepted, as before. An exported but empty `LOG_CHANNEL_ID=` wins over the file. The token is still a plain `String`; only `Debug` is 
redacted. `.env.example` not edited; proposed line: `# The real environment wins over this file.` 

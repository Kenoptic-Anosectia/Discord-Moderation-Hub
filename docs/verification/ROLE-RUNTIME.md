# Role runtime adapter (RG2)

Discord-facing layer between the pure role services and serenity: `src/roles/runtime.rs`. It builds K03 `RoleFacts` from Discord role data, implements K04's `RoleWriter` over serenity's `add_member_role`, and provides the helpers the later cards need.

## Rules the code enforces
1. A role id that is not in the guild's role list is `FactsOutcome::Missing` (the policy denies it as deleted); a lookup that *fails* is `FactsOutcome::Unavailable`, never a guess.
2. `CachedRoleSource` asks the cache first and the HTTP source on a miss. A failure of the last source is `Unavailable`. `bot_top_position` is the highest position among the roles the bot holds, or 0 when it holds none, so a bot with no known role can grant nothing.
3. `decide_from_facts` wraps the policy `Decision` in `RuntimeDecision` (the K03 enums are untouched). `Unavailable` is a deny with the code `facts_unavailable`; `grant_items` leaves such a role out of the grant request, so the executor cannot write it.
4. `SerenityRoleWriter` drives serenity's async call with `drive_blocking`: no runtime context -> `Handle::block_on`; a multi-thread runtime -> `block_in_place` + `block_on`; a current-thread runtime -> the stable status `1` (`STATUS_WRONG_CONTEXT`). It never panics on a runtime worker.
5. `map_serenity_error` keeps the HTTP status and Discord's JSON error code; a failure that is not an HTTP response gets the stable status `0` (`STATUS_NOT_HTTP`). `retry_after_ms` is `None`: serenity's typed error carries no rate-limit hint (its `ErrorResponse` has no such field) and serenity's own ratelimiter waits for a 429 and retries it before the error surfaces.
6. `TokioSleeper` waits at most its bound; the default bound is the grant module's `MAX_SLEEP_MS` (30 s).

## Tests (42 in `runtime.rs`)
Facts from synthetic serenity `Guild`/`Role`/`Member` models through a real `serenity::Cache`; managed flag, permissions, position and @everyone id carried; a deleted role id is `Missing`; bot top position with no roles, with several and with unknown roles; cache hit, cache miss and cache failure falling through to the HTTP source; a failed lookup denying the role with no writer call; the error table (404/10007, 404/10011, 403/50013, 429, 500, 502, transport) built from real serenity errors over a loopback HTTP response; the blocking-context rule (plain thread, runtime worker, `spawn_blocking`, current-thread runtime) and the real writer refusing a current-thread runtime with the stable status; the sleep bound; `already_held`; the stable deny codes; a 2,000-case seeded LCG sweep of the decision and a 500-case sweep of the source composition.
RED evidence (commit `7ecd9b9`, stub implementation): 24 of 38 failed; the 14 that passed were the ones a stub already denies. Two tests were added after that commit (the writer's context refusal and the sleeper's bound), so the suite is now 42.
Mutations tried (23, each made at least one test fail): missing role reported as unavailable or as existing; lowest instead of highest bot position; the HTTP source never asked; an unavailable lookup decided as allow; the looked-up bot position ignored; the HTTP status, the api code or the non-HTTP status dropped; a current-thread runtime blocked anyway; a plain `block_on` in a worker; the sleep clamp removed; the sleeper ignoring the clamped duration or its own bound; `already_held` always false; an unavailable role put into the grant request as a deny or as an allow; the managed flag, permissions, position or id dropped from the facts; a wrong deny code; `allows_write` ignoring an unavailable lookup.

## Config keys
None read here. The wiring card needs `MANAGE_ROLES` in the guild and the gateway intents `GUILDS` and `GUILD_MEMBERS` (`GUILD_MEMBERS` is privileged); `required_intents()` and `REQUIRED_PERMISSIONS` state them for the preflight card.

## NOT done
No command, no feature switched on, nothing called from `main.rs`, no persistence, no Discord call in production. `SerenityRoleWriter` is not exercised against Discord. `retry_after_ms` is never set from a serenity error, so the grant module always uses its own backoff on a 429. `grant_items` leaves an unavailable role out of the grant request, so the grant report does not list it; the caller reports it with `facts_unavailable`.

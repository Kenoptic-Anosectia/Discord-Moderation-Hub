# Reaction Roles on the Role Service (`src/role_menus/reactions.rs`)

## Purpose
Moves existing reaction-role binding setup (`add_binding`, `remove_binding`, `list_bindings`,
`lookup_roles_for_message_emoji`) and `ReactionAdd` / `ReactionRemove` handling onto the shared
role service (`roles::eligibility` K03, `roles::grant` K04, `roles::runtime` RG2) while preserving
the `reaction_roles` SQLite table schema (`PRIMARY KEY (message_id, emoji_key, role_id)`).

## Enforced Rules
- **Home-guild gate**: `add_binding`, `remove_binding`, and `list_bindings` reject foreign guilds
  (`foreign_guild`) and fail closed when `home_guild_id` is `None` (`home_guild_unset`).
- **Role eligibility**: setup and `ReactionAdd` run K03 `GrantPath::Reaction` via RG2, refusing
  privileged permissions, `PROTECTED_ROLE_IDS` (or invalid config), managed, `@everyone`, deleted,
  and `>= bot_top_position` roles. Active safeword denies restricted roles on add.
- **Bot filtering & removals**: self and bot reactors are skipped (`bot_reactor`). `ReactionRemove`
  uses `decide_removal`, allowing intended removals during active safeword if below the bot's top
  role, and retries HTTP 429 up to `MAX_RATE_LIMIT_RETRIES`.
- **Sanitized failure reporting**: denials, lookup failures, and role-write errors report stable
  machine codes (`FailCode` / `DenyReason`) through `FailureReporter` without user text or emoji.

## Config Inputs & Not Done
Consumes existing core inputs passed by wiring (`GUILD_ID`, `PROTECTED_ROLE_IDS`,
`SAFEWORD_RESTRICTED_ROLE_IDS`, `STAFF_ALERT_CHANNEL_ID`); adds no new `.env` keys.
`ReactionRolesFeature` provides the `Feature` hook offline; `src/main.rs`, `src/config.rs`, and
`src/features.rs` are not edited here (wiring slice W2 registers and swaps the legacy handlers).

## Verification & RED Evidence
7 RED failing behavior tests were committed first (`6ceb842`) and failed before implementation;
all 8 unit tests (parity, home-guild, K03 denials, safeword, bot skip, write failures, and seeded
429 retry liveness) now pass offline.

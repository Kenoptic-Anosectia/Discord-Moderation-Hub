# PREFLIGHT - startup preflight report per feature (OP1)

Pure `src/preflight.rs`: `evaluate(&PreflightInput) -> Report` decides at startup which optional features may run and why not. No serenity types, no I/O, no clock, no randomness.

## Rules enforced
1. Mode `Off` -> `off`, no reasons, nothing checked. `Shadow`/`Live` -> `ready` when every requirement holds, else `disabled` with the failed reason codes. The effective mode is the requested one when `ready`, `off` when `disabled`; it never exceeds the requested one (`mode_rank`).
2. Fail closed: a failed permission lookup, an `Unavailable` role fact, a missing role and an unknown bot top position are failures, never passes.
3. One entry per requested feature, in the fixed feature order; the reason codes are sorted by code string and deduplicated. `render_lines()` prints the feature name, the status and the reason codes only - no id, no configured name, no environment value.
4. The requirements, Discord bits as raw literals (`ADMINISTRATOR` 1<<3 implies every permission); every code is named after its rule (the `Reason` enum, described in the module comment):
   - role-granting features (Safeword, Welcome, RoleMenus, VoiceRooms): guild `MANAGE_ROLES` 1<<28; every configured role exists, has known facts, sits strictly below the bot's top role (position >= the bot's top is refused) and the bot's top position is known
   - Flash: at least one channel, `MANAGE_MESSAGES` 1<<13 in each
   - VoiceRooms: category `MANAGE_CHANNELS` 1<<4, `MOVE_MEMBERS` 1<<24, `CONNECT` 1<<20 (PLAN 1.5, VOICE-ROOMS.md: the bot creates and renames the rooms, moves members and reaches them)
   - Welcome, Embeds: `SEND_MESSAGES` 1<<11 and `EMBED_LINKS` 1<<14 in the target channel
   - Bump: `VIEW_CHANNEL` 1<<10 and `READ_MESSAGE_HISTORY` 1<<16 in the bump channel
   - Activity: guild `VIEW_CHANNEL` 1<<10
   - a failed lookup is `permissions_unknown`, no channel list `no_channels_configured`, no category `no_category_configured`

## What is tested (50 tests in `src/preflight.rs`)
One test per requirement (each flips exactly its own feature, the others stay ready); Mode Off never fails; `ADMINISTRATOR` satisfies every check; unknown facts fail closed; the effective mode never exceeds the requested one; the ordering and the codes are stable; the report never carries an id (sentinel `0x533343523354` in Debug, Display, lines); a seeded 2000-round LCG sweep asserts the effective mode and that with every input unknown no feature is `ready`.
RED evidence: commit `ce23dde` ran the suite against a stub `evaluate` -> 7 passed, 37 failed (the 7 are the ones a stub already satisfies); 48 mutations were tried, each caught by at least one test (list in the module comment).

## Config keys (READ by the wiring card W3, not by this module)
None are read here. W3 supplies the parsed inputs and stages `<FEATURE>_MODE` (default `off`; an unknown value -> `off`) plus the id lists the features already document (`FLASH_CHANNEL_IDS`, `VOICE_ROOM_CATEGORY_ID`, `WELCOME_TEMPLATE_PATH`, `EMBED_TEMPLATE_DIR`, `BUMP_BOT_ID`, `ACTIVITY_EXCLUDED_CHANNEL_IDS`; default empty/unset, a malformed entry refuses the whole key (K06), so the feature fails closed here).

## NOT done
No command, no feature switched on, nothing called from `main.rs` (only the `mod preflight;` line), no startup print, no `.env`/`config.rs` change, no persistence, no Discord call. The module-level `#![allow(dead_code)]` is temporary; W3 removes it. The report is not yet read by any caller.

# VOICE-ROOMS - temporary voice room policy (pure)

Card VR1. Module `src/voice_rooms/{mod,policy}.rs` - pure, Discord-free core, no serenity type, no I/O, no clock read, no randomness; `now` and every id are passed in.

## Rules enforced
- **Hub join creates a room** (`decide_join`): only a join in the hub channel of the configured guild, by a non-zero user id, and only under the per-user cap, the global cap and the per-user cooldown. Each refusal carries a stable code (`per_user_cap_reached`, `global_cap_reached`, `create_cooldown`, `not_hub_channel`, `invalid_user_id`, `guild_mismatch`).
- **Name sanitising** (`sanitize_name`): removes `<@..>`, `<@&..>`, `<#..>` mention tokens, the `@everyone` and `@here` words, control and zero-width characters, URLs (`scheme://`, `www.`), invite links and an invite host spread across spaces (`discord . gg/x`); shortens to 100 characters (characters, not bytes) and falls back to the default name when nothing is left.
- **Overwrite plan** (`build_overwrites`): the category's overwrites plus one owner overwrite. The owner's allow set is the constant `OWNER_RIGHTS` (connect, speak, move, mute); the deny set is `FORBIDDEN_OWNER_RIGHTS` (manage roles, manage channels, administrator). No input can widen the owner rights.
- **Permit** (`decide_permit`): refused for a member who could not already view the category, for a member id zero, for a requester who is not the owner, for the owner themself and in a channel the policy does not track.
- **Leaving** (`decide_leave`): the owner leaving a room that still holds members makes it `Claimable`; an empty room becomes `Cleanup` only once the grace period has elapsed. A channel the policy does not track is never deleted.
- **Rename budget** (`decide_rename`): counted per room inside its window; the budget is data, so the runtime enforces the same number. Discord's exact limit is not verified here.

## Assumptions (Chai's behaviour is unknown; conservative where silent)
- The room name at creation is the configured default; `sanitize_name` is also the public entry point for a later rename command.
- The guild id is part of the configuration: a join from another guild is out of scope.
- The exact Discord rename limit is not verified; the budget is data with a default of 2 renames per 600 s.
- Discord retired `MANAGE_PERMISSIONS`, so serenity 0.12 has no constant for it; the escalating set is manage roles, manage channels and administrator, and the retired bit is named `LEGACY_MANAGE_PERMISSIONS` so it can be asserted on.

## What is tested (58 unit tests in `src/voice_rooms/policy.rs`)
Hub join creates a room and each cap, the cooldown and the channel/guild/id checks deny; the cooldown boundary is allowed exactly at the limit; every sanitising case above; the owner overwrite grants exactly the four rights and no overwrite grants an escalating permission; the permit refusals; owner leave -> claimable, empty -> cleanup only after the grace, the grace boundary, untracked channels never deleted; the rename budget, its per-room count, its window and the "never plans more renames than the budget" loop; a seeded 2000-step join/leave sequence (small LCG, no dependency) that asserts the global and per-user caps after every step.

**RED evidence:** commit `6fec05c` ran this suite with the decision functions unimplemented - `test result: FAILED. 11 passed; 45 failed`. The 11 that passed were satisfied by the type definitions alone (the stable reason-code table and the permission-bit constants). One RED test was itself contradictory (it asserted the grace had *not* elapsed for a room whose grace *had* elapsed, which cannot pass against any implementation); it was corrected before GREEN to `a_room_that_just_became_empty_is_not_cleaned_up_yet`, so the RED and GREEN commits differ in that one test name and in no assertion that was weakened.

**Mutation evidence:** 29 deliberate mutations of the implementation, each restored from `git show HEAD:src/voice_rooms/policy.rs`; all 29 were caught by the suite (sanitiser as identity, each mention class kept, links kept, the limit removed, each cap and the cooldown removed or moved by one, a join outside the hub creating a room, each escalating permission granted, the category overwrites dropped, the permit ignoring visibility or ownership, an untracked channel deleted, the grace removed or moved by one, the claimable branch removed, the rename budget removed, counted globally or ignoring its window).

## Config keys (READ by the wiring card, not by this module; this module reads no environment variable and no file)
`VOICE_ROOMS_MODE` (default `off`; any other value means off) | `VOICE_ROOM_HUB_CHANNEL_ID` (default unset; unset means no hub, so no room is ever created) | `VOICE_ROOM_CATEGORY_ID` (default unset; unset means no category, so no room is ever created) | `VOICE_ROOM_PER_USER_CAP` (default `1`; invalid means 1) | `VOICE_ROOM_GLOBAL_CAP` (default `25`; invalid means 25) | `VOICE_ROOM_CREATE_COOLDOWN_SECS` (default `30`; invalid means 30) | `VOICE_ROOM_EMPTY_GRACE_SECS` (default `60`; invalid means 60) | `VOICE_ROOM_DEFAULT_NAME` (default `Voice Room`) | `VOICE_ROOM_RENAME_BUDGET` (default `2`) | `VOICE_ROOM_RENAME_WINDOW_SECS` (default `600`).

## What is NOT done
No runtime (nothing creates, deletes, renames or permits), no Discord registration, no commands, no event wiring, no persistence, no `.env` or `config.rs` change. The module-level `#![allow(dead_code)]` in `src/voice_rooms/mod.rs` is temporary and wiring card W1 removes it. The state (`VoiceRoomState`) is plain data the runtime owns; nothing here writes it.

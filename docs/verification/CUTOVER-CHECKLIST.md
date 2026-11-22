# Cutover Checklist — per-feature, per-bot

> Written for `main` once the policy/library modules have landed. Every `.env` key, id and name below is a placeholder.

**Two labels only.** `policy/library only, no runtime (not wired)`: code and tests exist, but nothing posts, deletes, grants or reads a key, and no `shadow`/`live` mode exists. `not built`: no code that could replace the old bot's feature (an unrelated basic command that already runs does not count; the Carl-bot section says which). **No feature is running yet, so no step below can be done today.** Every SHADOW, HANDOVER and ROLLBACK step says `NOT YET POSSIBLE: needs runtime card <name>` (names from `consolidation-plan/PLAN.md`), then describes the step it WILL be so the owner can prepare.

**Single-writer rule.** The OLD bot's feature is disabled first, then this bot's is enabled. A mode key that nothing reads does nothing, so setting one now would leave the old bot as the only writer and prove nothing. **Mode keys** (`*_MODE=off|shadow|live`) are `to be added by the wiring card; not read by any module today`, except two that a module defines in code, `VOICE_ROOMS_MODE` (VR1 policy) and `ACTIVITY_MODE` (AC1 policy), and one documented by SW1 for SW3, `SAFEWORD_MODE` (default `off`; any value other than off|shadow|live fails closed to `off`). Even those three are read by no runtime today. Other keys named below are defined (or documented) by a policy module for the wiring card to read; none is read today. Every *Pre-checks* item is an owner action.

Library-only foundations with no feature of their own: K02 jobs, K04 grant executor, K05 privacy scope, K06 settings, SW2 safeword repository, AC1 activity accounting.

---

## YAGPDB

### Flash timer (epic FL1 — policy/library only, no runtime (not wired))
1. **What:** Will, on a non-bot message with attachments in a flash channel, post a 3-minute notice and delete both messages. Mode key `FLASH_MODE=off|shadow|live`: to be added by the wiring card; not read by any module today.
2. **Pre-checks (owner):** YAGPDB flash command configured with a role and channels. Have ready (keys defined by the FL1 policy for the wiring card): `FLASH_CHANNEL_IDS`, `FLASH_PING_ROLE_ID`, `FLASH_DELAY_SECS=180`.
3. **Shadow:** NOT YET POSSIBLE: needs runtime card FL2 (with KJ2, W1, W2). Will be: set `FLASH_MODE=shadow`; SKYNET logs the notice text and delete targets; compare with YAGPDB's real output.
4. **Handover:** NOT YET POSSIBLE: needs runtime card FL2. Will be: (a) disable flash in the YAGPDB dashboard, (b) then set `FLASH_MODE=live`.
5. **Rollback:** NOT YET POSSIBLE: needs runtime card FL2. Will be: set `FLASH_MODE=off`, then re-enable flash in YAGPDB.
6. **Evidence:** YAGPDB settings screenshots now; later 3 shadow log lines with identical notice text; date.

### Safeword (epic S01 — policy/library only, no runtime (not wired): state machine and repository)
1. **What:** Will, on a `Safeword_Red` reaction, save restricted roles, remove them and add the safeword role; restore on reaction removal. Mode key `SAFEWORD_MODE=off|shadow|live`: documented by SW1 for SW3 (default `off`; an unknown value fails closed to `off`); read by nothing today.
2. **Pre-checks (owner):** YAGPDB safeword custom command active. Have ready (also named in the SW1 doc for SW3/wiring): `SAFEWORD_CHANNEL_ID`, `SAFEWORD_MESSAGE_ID`, `SAFEWORD_EMOJI_ID`, `SAFEWORD_ROLE_ID`, `SAFEWORD_RESTRICTED_ROLE_IDS`.
3. **Shadow:** NOT YET POSSIBLE: needs runtime card SW3 (with RG2, W2). Will be: set `SAFEWORD_MODE=shadow`; SKYNET logs snapshot, removals and adds; compare saved role set and DM text with YAGPDB.
4. **Handover:** NOT YET POSSIBLE: needs runtime card SW3. Will be: (a) disable the safeword command in YAGPDB, (b) then set `SAFEWORD_MODE=live`. Do it when no member is on safeword.
5. **Rollback:** NOT YET POSSIBLE: needs runtime card SW3. Will be: set `SAFEWORD_MODE=off`, re-enable YAGPDB; members on safeword lose SKYNET state.
6. **Evidence:** YAGPDB config screenshot now; later 3 shadow log lines; date.

### Rules embeds (epic EM1 — policy/library only, no runtime (not wired))
1. **What:** Will let staff publish rules embeds as managed, editable posts. Mode key `EMBEDS_MODE=off|shadow|live`: to be added by the wiring card; not read by any module today.
2. **Pre-checks (owner):** YAGPDB two CCs post the General and NSFW rules embeds; save their text. Key named by EM1 for the wiring card: `EMBED_TEMPLATE_DIR`.
3. **Shadow:** NOT YET POSSIBLE: needs runtime card EM2 (with W3). Will be: `EMBEDS_MODE=shadow`; SKYNET loads templates and logs what it would post; compare fields with YAGPDB.
4. **Handover:** NOT YET POSSIBLE: needs runtime card EM2. Will be: (a) delete/disable the YAGPDB rules CCs, (b) set `EMBEDS_MODE=live`, then run `/embed post` (planned EM2 command; it does not exist).
5. **Rollback:** NOT YET POSSIBLE: needs runtime card EM2. Will be: `EMBEDS_MODE=off`, re-create the YAGPDB CCs from the saved text.
6. **Evidence:** Side-by-side embed screenshots; later `/embed list` output (planned EM2 command); date.

### Welcome message (epic WE1 — policy/library only, no runtime (not wired): template renderer only)
1. **What:** Will post a welcome embed on join, rendered from a template. Mode key `WELCOME_MODE=off|shadow|live`: to be added by the wiring card; not read by any module today. **Join roles are NOT part of this**: granting them is a separate slice, `not built` (see `docs/verification/WELCOME-RENDER.md`; plan card WE2 via the role-grant service). YAGPDB join roles stay on YAGPDB.
2. **Pre-checks (owner):** YAGPDB welcome message and channel configured; save the text. Key named by WE1 for the wiring card: `WELCOME_TEMPLATE_PATH`; also have `WELCOME_CHANNEL_ID` ready (planned, not yet defined).
3. **Shadow:** NOT YET POSSIBLE: needs runtime card WE2 (with W3). Will be: `WELCOME_MODE=shadow`; SKYNET logs the embed it would post; compare member count and fields with YAGPDB.
4. **Handover:** NOT YET POSSIBLE: needs runtime card WE2. Will be: (a) disable the YAGPDB welcome message only (leave join roles alone), (b) then set `WELCOME_MODE=live`.
5. **Rollback:** NOT YET POSSIBLE: needs runtime card WE2. Will be: `WELCOME_MODE=off`, re-enable the YAGPDB welcome message.
6. **Evidence:** YAGPDB welcome screenshot now; later rendered-template shadow log; date.

---

## Carl-bot (epic R01 — not built: no replacement code; migration)
1. **What:** Reaction-role bindings. Only the basic `/reactionrole` command exists today; it has no groups, exclusivity, shadow, mode key or safeword guard, so it does not replace Carl-bot. Mode key `REACTIONROLE_MODE=off|shadow|live`: to be added by the wiring card; not read by any module today.
2. **Pre-checks (owner):** Carl-bot reaction-role menus active; screenshot every binding (no Carl export known). **Never bind `/reactionrole` on a message that a Carl-bot or Pronoun Picker menu manages**: it is the one SKYNET writer that runs today, and both bots would then grant roles from the same reaction (two writers).
3. **Shadow:** NOT YET POSSIBLE: needs runtime cards RR2, RM2, RM3. Will be: `REACTIONROLE_MODE=shadow`; SKYNET logs reactions and the roles it would grant; compare with Carl-bot's grants.
4. **Handover:** NOT YET POSSIBLE: needs runtime card RR2 (with W2). Will be: (a) disable reaction roles in the Carl-bot dashboard, (b) set `REACTIONROLE_MODE=live`.
5. **Rollback:** NOT YET POSSIBLE: needs runtime card RR2. Will be: `REACTIONROLE_MODE=off`, re-enable Carl-bot; rebuild bindings from the screenshots.
6. **Evidence:** Carl-bot menu screenshots now; later shadow log of grants; date.

## Pronoun Picker (epic RM1 — policy/library only, no runtime (not wired))
1. **What:** The role-menu selection policy exists (exclusive, multi, max-N, toggle); nothing posts a menu or reads clicks. Will give self-assigned pronoun roles via a multi-select menu. Mode key `ROLEMENU_MODE=off|shadow|live`: to be added by the wiring card; not read by any module today.
2. **Pre-checks (owner):** Pronoun Picker bot or Carl-bot reaction roles in use; list the pronoun roles. Role-list key `PRONOUN_ROLE_IDS`: planned, not yet defined.
3. **Shadow:** NOT YET POSSIBLE: needs runtime cards RM2, RM3 (with W4). Will be: `ROLEMENU_MODE=shadow`; SKYNET logs the selections it would apply.
4. **Handover:** NOT YET POSSIBLE: needs runtime cards RM2, RM3. Will be: (a) disable the old Pronoun Picker or Carl-bot menu, (b) set `ROLEMENU_MODE=live`.
5. **Rollback:** NOT YET POSSIBLE: needs runtime cards RM2, RM3. Will be: `ROLEMENU_MODE=off`, re-enable the old menu.
6. **Evidence:** Old menu screenshot now; later shadow log; date.

## Bump Buddy (epic BU1 — policy/library only, no runtime (not wired))
1. **What:** Will detect a Disboard bump success and post one reminder per cycle pinging a role. Mode key `BUMP_MODE=off|shadow|live`: to be added by the wiring card; not read by any module today.
2. **Pre-checks (owner):** Bump Buddy active with a bump role. Keys defined by BU1 for the wiring card: `BUMP_BOT_ID`, `BUMP_GUILD_ID`, `BUMP_SUCCESS_MARKER`, `BUMP_INTERVAL_SECS`. `BUMP_ROLE_ID` and `BUMP_CHANNEL_ID`: planned, not defined by BU1.
3. **Shadow:** NOT YET POSSIBLE: needs runtime card BU2 (with KJ2, W3). Will be: `BUMP_MODE=shadow`; SKYNET logs detection and the reminder plan; compare cooldown and text with Bump Buddy.
4. **Handover:** NOT YET POSSIBLE: needs runtime card BU2. Will be: (a) disable Bump Buddy, (b) then set `BUMP_MODE=live`.
5. **Rollback:** NOT YET POSSIBLE: needs runtime card BU2. Will be: `BUMP_MODE=off`, re-enable Bump Buddy; the next cycle resets.
6. **Evidence:** Bump Buddy settings screenshot now; later shadow detection log; date.

## Chai (epic VR1 — policy/library only, no runtime (not wired))
1. **What:** Will let members create voice rooms from a hub channel into a category. Mode key `VOICE_ROOMS_MODE=off|shadow|live`: defined by the VR1 policy for the wiring card; read by nothing today.
2. **Pre-checks (owner):** Chai voice-room modes configured; note hub and category. Keys defined by VR1 for the wiring card: `VOICE_ROOM_HUB_CHANNEL_ID`, `VOICE_ROOM_CATEGORY_ID` (singular: one hub).
3. **Shadow:** NOT YET POSSIBLE: needs runtime card VR2 (with W4). Will be: `VOICE_ROOMS_MODE=shadow`; SKYNET logs the rooms it would create and delete.
4. **Handover:** NOT YET POSSIBLE: needs runtime cards VR2, VR3 (`/vc`). Will be: (a) disable Chai's voice-room mode, (b) set `VOICE_ROOMS_MODE=live`.
5. **Rollback:** NOT YET POSSIBLE: needs runtime card VR2. Will be: `VOICE_ROOMS_MODE=off`, re-enable Chai; rooms are removed only if tracked.
6. **Evidence:** Chai settings screenshot now; later shadow log; date.

## Lurkr (epic E02 — not built)
1. **What:** XP/levels from messages and voice. Mode key `XP_MODE=off|shadow|live`: to be added by the wiring card; not read by any module today. Activity accounting (AC1, library only) will feed it; `ACTIVITY_MODE` is the AC1-defined key the AC2 runtime will read.
2. **Pre-checks (owner):** Lurkr active; record XP rules and reward roles. Keys planned, not defined: `XP_MESSAGE_COOLDOWN_MS`, `XP_VOICE_MULTIPLIER`.
3. **Shadow:** NOT YET POSSIBLE: needs runtime cards LV2, AC2 (with W5); LV1 is a pure XP-policy prerequisite, not a runtime card. Will be: `XP_MODE=shadow` accumulates XP silently; compare with Lurkr.
4. **Handover:** NOT YET POSSIBLE: needs runtime card LV2. Will be: (a) disable Lurkr XP and rewards, (b) set `XP_MODE=live`.
5. **Rollback:** NOT YET POSSIBLE: needs runtime card LV2. Will be: `XP_MODE=off`, re-enable Lurkr.
6. **Evidence:** Lurkr config screenshot now; later shadow-vs-Lurkr comparison; date.

## Statbot (epic E03 — not built)
1. **What:** Message and voice-time statistics with graphs. Mode key `STATS_MODE=off|shadow|live`: to be added by the wiring card; not read by any module today.
2. **Pre-checks (owner):** Statbot active; record its stats channel. Key planned, not defined: `STATS_SERVER_CHANNEL_ID`.
3. **Shadow:** NOT YET POSSIBLE: needs runtime cards ST2, AC2 (with W5); ST1 is a pure stats-query prerequisite, not a runtime card. Will be: `STATS_MODE=shadow`; compare counts with Statbot.
4. **Handover:** NOT YET POSSIBLE: needs runtime card ST2. Will be: (a) disable Statbot, (b) set `STATS_MODE=live`.
5. **Rollback:** NOT YET POSSIBLE: needs runtime card ST2. Will be: `STATS_MODE=off`, re-enable Statbot.
6. **Evidence:** Statbot screenshot now; later count comparison; date.

## Wick (epic M01/M02 — not built)
1. **What:** Moderation: warn, timeout, kick, ban, anti-raid, spam filters. Mode key `MOD_MODE=off|shadow|live`: to be added by the wiring card; not read by any module today.
2. **Pre-checks (owner):** Wick modules active; record staff role and log channel. Keys planned, not defined: `MOD_STAFF_ROLE_ID`, `MOD_LOG_CHANNEL_ID`.
3. **Shadow:** NOT YET POSSIBLE: needs runtime cards MO1, MO2, AM1, AM2 (observe-only). Will be: `MOD_MODE=shadow`.
4. **Handover:** NOT YET POSSIBLE: needs runtime cards MO1, MO2 (and X02). Will be: Wick is disabled last, then `MOD_MODE=live`.
5. **Rollback:** NOT YET POSSIBLE: needs runtime cards MO1, MO2. Will be: `MOD_MODE=off`, re-enable Wick.
6. **Evidence:** Wick module screenshots now; later shadow log; date.

---

## Summary

| Bot | Feature | Status | Runtime card needed |
|---|---|---|---|
| YAGPDB | Flash timer | policy/library only, no runtime (not wired) | FL2 |
| YAGPDB | Safeword | policy/library only, no runtime (not wired) | SW3 |
| YAGPDB | Rules embeds | policy/library only, no runtime (not wired) | EM2 |
| YAGPDB | Welcome message (renderer) | policy/library only, no runtime (not wired) | WE2 |
| YAGPDB | Join roles | not built | WE2 |
| Carl-bot | Reaction roles | not built | RR2, RM2, RM3 |
| Pronoun Picker | Pronoun menu | policy/library only, no runtime (not wired) | RM2, RM3 |
| Bump Buddy | Bump reminder | policy/library only, no runtime (not wired) | BU2 |
| Chai | Voice rooms | policy/library only, no runtime (not wired) | VR2, VR3 |
| Lurkr | XP/levels | not built | LV2, AC2 (LV1 pure prerequisite) |
| Statbot | Stats/graphs | not built | ST2, AC2 (ST1 pure prerequisite) |
| Wick | Moderation | not built | MO1, MO2, AM1, AM2 |

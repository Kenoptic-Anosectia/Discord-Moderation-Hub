# Cutover Checklist — per-feature, per-bot

> Every feature ships `off` by default in this repo. All `.env` keys, role/channel IDs and emoji names are placeholders.

---

## YAGPDB

### Flash timer (epic FL1 — built: pure policy `src/flash/policy.rs`)
1. **What:** On a non-bot message with attachments in a flash channel, posts a 3-minute notice and deletes both messages. Mode key: `FLASH_MODE=off|shadow|live`.
2. **Pre-checks (owner):** YAGPDB flash command configured with a role and channels. `.env` keys ready: `FLASH_CHANNEL_IDS`, `FLASH_PING_ROLE_ID`, `FLASH_DELAY_SECS=180`.
3. **Shadow:** Set `FLASH_MODE=shadow`. SKYNET logs what it would do (notice text, delete targets). Compare notice text and channel list against YAGPDB's actual output.
4. **Handover:** (a) Disable flash in YAGPDB dashboard. (b) Set `FLASH_MODE=live`.
5. **Rollback:** Set `FLASH_MODE=off`, re-enable flash in YAGPDB. No state lost.
6. **Evidence:** Screenshots of YAGPDB settings; 3 shadow-mode log lines showing identical notice text; date.

### Safeword (epic S01 — built: state machine `src/safeword/state.rs`)
1. **What:** React `Safeword_Red` on a message: saves restricted roles, removes them, adds safeword role. React removal restores. Mode key: `SAFEWORD_MODE=off|shadow|live`.
2. **Pre-checks (owner):** YAGPDB safeword custom command active. `.env` keys: `SAFEWORD_CHANNEL_ID`, `SAFEWORD_MESSAGE_ID`, `SAFEWORD_EMOJI_ID`, `SAFEWORD_ROLE_ID`, `SAFEWORD_RESTRICTED_ROLE_IDS`.
3. **Shadow:** Set `SAFEWORD_MODE=shadow`. SKYNET logs snapshot, removal, and add actions. Compare saved role set and DM text against YAGPDB.
4. **Handover:** (a) Disable safeword command in YAGPDB. (b) Set `SAFEWORD_MODE=live`.
5. **Rollback:** Set `SAFEWORD_MODE=off`, re-enable in YAGPDB. Members on safeword lose SKYNET state.
6. **Evidence:** 3 shadow-mode log lines (snapshot roles, removals); screenshot of YAGPDB config; date.

### Rules embeds (epic EM1 — built: template renderer `src/embeds/`)
1. **What:** Staff publish rules embeds as managed posts, editable in place. Mode key: `EMBEDS_MODE=off|shadow|live`.
2. **Pre-checks (owner):** YAGPDB two CCs posting General and NSFW rules embeds. `.env`: `EMBED_TEMPLATE_DIR`.
3. **Shadow:** Set `EMBEDS_MODE=shadow`. SKYNET loads templates and logs what it would post. Compare embed fields against YAGPDB output.
4. **Handover:** (a) Delete/disable YAGPDB rules CCs. (b) Set `EMBEDS_MODE=live`; run `/embed post`.
5. **Rollback:** Set `EMBEDS_MODE=off`, re-create YAGPDB CCs from saved text.
6. **Evidence:** Side-by-side embed screenshots; `/embed list` output; date.

### Welcome + join roles (epic WE1 — built: template renderer `src/welcome/render.rs`)
1. **What:** On join, posts a welcome embed and adds configured roles to non-bot members. Mode key: `WELCOME_MODE=off|shadow|live`.
2. **Pre-checks (owner):** YAGPDB join roles and welcome channel configured. `.env`: `WELCOME_CHANNEL_ID`, `JOIN_ROLE_IDS`, `WELCOME_TEMPLATE_PATH`.
3. **Shadow:** Set `WELCOME_MODE=shadow`. SKYNET logs what embed it would post and which roles it would grant. Compare member count, fields and role list against YAGPDB.
4. **Handover:** (a) Disable join roles and welcome in YAGPDB dashboard. (b) Set `WELCOME_MODE=live`.
5. **Rollback:** Set `WELCOME_MODE=off`, re-enable in YAGPDB. New joiners get old behavior.
6. **Evidence:** Shadow log of embed template rendered; YAGPDB join-role screenshot; date.

---

## Carl-bot (epic R01 — built: `/reactionrole` command with SQLite)
1. **What:** Reaction-role bindings on messages. Mode key: `REACTIONROLE_MODE=off|shadow|live`.
2. **Pre-checks (owner):** Carl-bot reaction-role menus active. `.env`: `REACTION_ROLE_DB_PATH`.
3. **Shadow:** Set `REACTIONROLE_MODE=shadow`. SKYNET logs reactions and what roles it would grant. Compare against Carl-bot's actual grants.
4. **Handover:** (a) Disable reaction roles in Carl-bot dashboard. (b) Set `REACTIONROLE_MODE=live`.
5. **Rollback:** Set `REACTIONROLE_MODE=off`, re-enable in Carl-bot. Existing bindings are lost (no Carl export known).
6. **Evidence:** Shadow log of role grants; Carl-bot menu screenshot; date.

---

## Pronoun Picker (epic R01 — policy only, not wired)
1. **What:** Self-assigned pronoun roles via a multi-select menu. Mode key: `ROLEMENU_MODE=off|shadow|live`.
2. **Pre-checks (owner):** Pronoun Picker bot or Carl-bot reaction roles for pronouns. `.env`: `PRONOUN_ROLE_IDS` (comma list).
3. **Shadow:** NOT YET BUILT — not wired. No shadow step available.
4. **Handover:** NOT YET BUILT.
5. **Rollback:** N/A.
6. **Evidence:** None yet.

---

## Bump Buddy (epic BU1 — built: policy `src/bump/policy.rs`)
1. **What:** Detects Disboard bump success, posts one reminder per cycle pinging a role. Mode key: `BUMP_MODE=off|shadow|live`.
2. **Pre-checks (owner):** Bump Buddy active with a bump role. `.env`: `BUMP_BOT_ID`, `BUMP_ROLE_ID`, `BUMP_CHANNEL_ID`.
3. **Shadow:** Set `BUMP_MODE=shadow`. SKYNET logs bump detection and reminder plan. Compare cooldown and reminder text against Bump Buddy.
4. **Handover:** (a) Disable Bump Buddy. (b) Set `BUMP_MODE=live`.
5. **Rollback:** Set `BUMP_MODE=off`, re-enable Bump Buddy. Next cycle resets.
6. **Evidence:** Shadow log of detection; Bump Buddy settings screenshot; date.

---

## Chai (epic VR1 — built: policy `src/voice_rooms/policy.rs`)
1. **What:** Members create voice rooms from a hub channel into a category. Mode key: `VOICE_ROOMS_MODE=off|shadow|live`.
2. **Pre-checks (owner):** Chai voice-room modes configured. `.env`: `VC_HUB_CHANNEL_IDS`, `VC_CATEGORY_ID`.
3. **Shadow:** NOT YET BUILT — policy exists but runtime is not wired. No shadow step available.
4. **Handover:** NOT YET BUILT.
5. **Rollback:** N/A.
6. **Evidence:** None yet.

---

## Lurkr (epic E02 — policy only, not wired)
1. **What:** XP/levels from messages and voice. Mode key: `XP_MODE=off|shadow|live`.
2. **Pre-checks (owner):** Lurkr active with XP rules. `.env`: `XP_MESSAGE_COOLDOWN_MS`, `XP_VOICE_MULTIPLIER`.
3. **Shadow:** NOT YET BUILT — no XP code on main.
4. **Handover:** NOT YET BUILT.
5. **Rollback:** N/A.
6. **Evidence:** None yet.

---

## Statbot (epic E03 — policy only, not wired)
1. **What:** Message and voice-time statistics with graphs. Mode key: `STATS_MODE=off|shadow|live`.
2. **Pre-checks (owner):** Statbot active. `.env`: `STATS_SERVER_CHANNEL_ID`.
3. **Shadow:** NOT YET BUILT — no stats code on main.
4. **Handover:** NOT YET BUILT.
5. **Rollback:** N/A.
6. **Evidence:** None yet.

---

## Wick (epic M01/M02 — not built)
1. **What:** Moderation: warn, timeout, kick, ban, anti-raid, spam filters. Mode key: `MOD_MODE=off|shadow|live`.
2. **Pre-checks (owner):** Wick modules active. `.env`: `MOD_STAFF_ROLE_ID`, `MOD_LOG_CHANNEL_ID`.
3. **Shadow:** NOT YET BUILT — moderation commands not on main.
4. **Handover:** NOT YET BUILT. Wick stays until last per plan.
5. **Rollback:** N/A.
6. **Evidence:** None yet.

---

## Summary

| Bot | Feature | Status |
|---|---|---|
| YAGPDB | Flash timer | Built (`src/flash/`) |
| YAGPDB | Safeword | Built (`src/safeword/`) |
| YAGPDB | Rules embeds | Built (`src/embeds/`) |
| YAGPDB | Welcome + join roles | Built (`src/welcome/`) |
| Carl-bot | Reaction roles | Built (`/reactionrole`) |
| Pronoun Picker | Pronoun menu | Policy only (not wired) |
| Bump Buddy | Bump reminder | Built (`src/bump/`) |
| Chai | Voice rooms | Policy only (not wired) |
| Lurkr | XP/levels | Not built |
| Statbot | Stats/graphs | Not built |
| Wick | Moderation | Not built |

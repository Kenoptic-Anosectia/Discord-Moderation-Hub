# VOICE-ROOMS - temporary voice room policy (pure)
Card VR1, `src/voice_rooms/{mod,policy}.rs`: pure core (no serenity, I/O, clock or randomness).
- **Join**: a hub join in the configured guild by a non-zero user, under the per-user cap, global
  cap and cooldown, else a stable code. **Overwrites**: the category's, unchanged, plus one owner
  overwrite allowing exactly connect, speak, move, mute; denying manage roles/channels and admin.
- **Permit** refused if the member cannot view the category, is the owner or id 0, the requester
  is not the owner, or the channel is untracked. **Leave/sweep**: `occupants` counts the leaver; the
  owner leaving a room of 2+ -> `Claimable`; an empty room -> `Cleanup` only after the grace, whoever
  left last; untracked channels are not deleted. **Rename**: per-room budget as data (2 per 600 s).
  Elapsed times use `saturating_sub`; a negative grace or rename window is `invalid_config`.
- **Name** (`sanitize_name`) is an ALLOWLIST: keep a char only if `char::is_alphanumeric()` (minus
  the Hangul fillers U+115F, U+1160, U+3164, U+FFA0, letters that draw nothing), the ASCII space or
  one of `- _ . , ' ! ? & + ( ) # :` (common in names; none forms a mention or URL path), so no
  output holds `@ < > /`. Mentions and links are stripped first, spaces collapse, the text is cut at
  100 chars, and the pass repeats until nothing changes (idempotent). Empty -> cleaned default ->
  `Voice Room`. Why: a list of what renders nothing cannot be shown complete (eight review rounds
  each found a gap); an allowlist drops every other code point by construction, with std only.
**Tests** (133 unit). Cleaner: raw-literal hostile spellings, combining marks, the cap on multi-byte
and collapsed text, the limits below, and a seeded LCG fuzz (1500 inputs): allowed chars only, <= 100
chars, no `@`, no stray space, a subsequence of the input or the default, idempotent. RED `d860e3b`
(128 passed, 10 failed) -> GREEN `ea842ab`. 21 mutants (`target/vr1-tests/org1-mut/`): 20 caught;
dropping `trim_end` after the cut is equivalent (the next pass's link step trims that space).
**Config** (read by W1; bad or negative caps/cooldown/grace = default): `VOICE_ROOMS_MODE` (off),
`VOICE_ROOM_HUB_CHANNEL_ID`, `VOICE_ROOM_CATEGORY_ID` (unset = no rooms), `VOICE_ROOM_PER_USER_CAP`
(1), `VOICE_ROOM_GLOBAL_CAP` (25), `VOICE_ROOM_CREATE_COOLDOWN_SECS` (30), `VOICE_ROOM_EMPTY_GRACE_SECS`
(60), `VOICE_ROOM_DEFAULT_NAME`, `VOICE_ROOM_RENAME_BUDGET` (2), `VOICE_ROOM_RENAME_WINDOW_SECS` (600).
**Not done**: runtime, commands, registration, persistence, config change (the runtime owns state and
`empty_since`; W1 removes the `dead_code` allow). DEFERRED (owner-approved): emoji and symbols are
dropped. Also dropped: non-alphabetic marks (`e`+U+0301, Indic virama, Thai tone mark). Font-blank
letters survive; `notes.txt` goes as a host, a bare IP stays. Discord's rename limit is unverified.

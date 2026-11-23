# ROLE-MENU-STORAGE - role menu tables and `/rolemenu` logic (RM2)

New: `src/role_menus/repository.rs` (SQLite tables) and `src/role_menus/command.rs` (logic over
three traits); `mod.rs` gains two `pub mod` lines. RM1 policy is reused unchanged. Tables:
`role_menus(id INTEGER PK, guild TEXT, key TEXT UNIQUE, mode TEXT, max_select INTEGER, channel
TEXT, message TEXT, embed_key TEXT)` and `role_menu_options(menu_id INTEGER, role TEXT, label
TEXT, emoji TEXT, position INTEGER)`. Feature name in `schema_versions`: `role_menus`; the
repository runs its own migrations and does not touch `storage.rs`. A key is unique across the
whole table; an option's role is unique inside its menu.

## Rules the code enforces (each has a test)

1. An off feature serves nothing (`disabled`). Otherwise the bot must hold Manage Roles, the
   caller must be the guild owner or hold the staff role, the guild must be the home guild, and
   an unset or zero home guild fails closed.
2. A role passes K03 (`GrantPath::Menu`) before it is stored: privileged, managed, protected,
   @everyone, deleted or at-or-above-the-bot roles get their own K03 code; an unreadable
   `PROTECTED_ROLE_IDS` list refuses every option.
3. A key is lowercase ASCII (letter or digit first, then letters, digits, `-`, `_`), at most 64
   bytes. A label is at most 100 characters and refuses control, format and blank/filler
   code points (Hangul fillers, braille blank, default-ignorables), line and paragraph
   separators and mention markup, anywhere in the text. An emoji is a custom emoji
   `<name:id>`/`<a:name:id>` (non-zero id) or exactly ONE Unicode emoji (one base with at most
   one variation selector, a skin tone, ZWJ-joined parts, a digit keycap, or one flag pair).
4. A menu holds at most 125 options (Discord: five rows of 25 in one message); the 126th is
   refused, not dropped. More than 25 are split into components of 25 in stored order.
5. `post` is idempotent: a menu with a channel and message is edited (stored channel and
   message are passed), otherwise sent. A different requested channel gets `channel_mismatch`
   and nothing is sent. An edit that reports the message gone sends a new one into the
   recorded channel and records it (`reposted`). A failed send or failed record write leaves
   the row unchanged. A recorded channel that is gone gives `channel_gone` with the row left
   alone; `unpublish <key>` (staff only) clears channel, message and embed key, after which
   `post` publishes anywhere. A menu with no options is refused.
6. Removing an option or menu touches only the menu tables; no member role is written or
   removed. Only the calling guild's menus are read. Every reply is ephemeral, codes only.

## What is tested (89 new unit tests: 24 repository, 65 command)

Every rule above, plus permission bits, a foreign guild doing nothing, split boundaries, schema
re-open, corrupt rows, trimming, the `posted` flag and the 125/126 limit. Seeded LCG liveness
sequences: `post` against failing sends; a persistent deleted message; a gone channel with the
`unpublish` reset over real SQLite; and one that draws send, edit-NotFound and record-write
faults per step plus random `unpublish`, asserting the row is unchanged on every failure, a
send is recorded as a new message, an edit keeps the message, and once faults stop the next
`post` publishes and the one after edits.
RED evidence: round 1 `7e4355a` (stubs): `FAILED. 51 passed; 48 failed`; round 3 `16e2324`
(invisible labels, one-emoji rule, record-write failure) failed before `847be6e`.
Mutations caught by named tests: 30 in rounds 1-2 (gates, K03, limits, chunking, key/emoji/label
checks, FK mapping, mode fallback, row-after-failed-send ...) and, in the last round, ignoring
a record-write error on the send path and on the repost path.

## Config keys (READ by the wiring card, not by this module)

`ROLE_MENUS_MODE`: `off` (default) | `shadow` | `live`; unset or a typo is off and the wiring
card passes `Settings::enabled = false`. `GUILD_ID`: home guild; unset/zero serves nothing.
`STAFF_ROLE_ID`: caller role; unset means only the guild owner configures. `PROTECTED_ROLE_IDS`:
roles a menu may not grant; a malformed entry must make the wiring card refuse every option.
This module reads no environment variable.

## NOT done

No command is registered, no feature switched on, nothing is called from `main.rs`; no
`config.rs`/`.env.example` change, no `Feature` implementation, no job kind. `MenuStore`,
`MenuDiscord` and `RoleFactsSource` have no production implementation (the wiring card writes
them). `src/embeds` is not called; embed posting sits behind `MenuDiscord`. Member clicks are
RM3. Limits: a sent message whose record write fails is orphaned and the next `post` sends
another; `post` and the Discord call are not one transaction, so two concurrent `post`s can
both send; `unpublish` does not delete the old Discord message. The emoji check refuses `#`/`*`
keycaps and tag-sequence flags (England). Label and emoji bounds are this module's own.
`split_chunks` copies RM1's `SELECT_OPTION_LIMIT`; RM1 splits by group, this module has one.

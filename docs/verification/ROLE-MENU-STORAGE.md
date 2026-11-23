# ROLE-MENU-STORAGE - role menu tables and `/rolemenu` logic (RM2)

New: `src/role_menus/repository.rs` (the SQLite tables) and `src/role_menus/command.rs`
(the logic over three traits); `src/role_menus/mod.rs` gains two `pub mod` lines. The RM1
policy (`src/role_menus/policy.rs`) is reused unchanged. Tables: `role_menus(id INTEGER PK,
guild TEXT, key TEXT UNIQUE, mode TEXT, max_select INTEGER, channel TEXT, message TEXT,
embed_key TEXT)` and `role_menu_options(menu_id INTEGER, role TEXT, label TEXT, emoji TEXT,
position INTEGER)`. Feature name in `schema_versions`: `role_menus`; the repository runs
its own migrations and does not touch `src/storage.rs`. A key is unique across the whole
table; an option's role is unique inside its menu.

## Rules the code enforces (each has a test)

1. An off feature serves nothing. Otherwise the bot must hold "Manage Roles", the caller
   must be the guild owner or hold the staff role, the guild must be the one home guild,
   and an unset or zero home guild fails closed.
2. A role is checked against K03 (`GrantPath::Menu`) before it is stored: privileged,
   managed, protected, @everyone, deleted or at-or-above-the-bot roles are refused with
   their own K03 code; an unreadable `PROTECTED_ROLE_IDS` list refuses every option.
3. A key is one spelling: lowercase ASCII, a letter or digit first, then letters, digits,
   `-` or `_`, at most 64 bytes. A label is non-empty after trimming, at most 100
   characters, free of control characters, format characters, line and paragraph
   separators and mention markup, and holds one visible character. An emoji is a Discord
   custom emoji `<name:id>`/`<a:name:id>` or a plain Unicode emoji; anything else is
   refused.
4. A menu holds at most 125 options (Discord: five action rows of twenty-five options in
   one message); the 126th is refused, never dropped. More than 25 options are split into
   components of 25 in the stored order.
5. `post` is idempotent: a menu naming a channel and a message is edited, a menu naming
   none is posted; the edit gets the stored channel as well as the message. A requested
   channel other than the stored one is refused and nothing is sent or edited. When the
   recorded message is gone (the edit reports `NotFound`) a new message is sent into the
   recorded channel and recorded; a failed send leaves the row unchanged; a menu with no
   options is refused.
6. Removing an option or a menu touches only the menu tables; no member role is written
   or removed anywhere here. Only the calling guild's menus are read and listed. Every
   reply is ephemeral and carries stable codes only.

## What is tested (71 new: 23 repository, 48 command)

Every rule above has a test, plus the privileged-permission bits, a foreign guild doing
nothing, the split boundaries, the schema re-opening, corrupt rows, the stored mode, the
missing menu, the trimming, the `posted` flag and the option limit. Four seeded LCG
sequences assert liveness: two drive `post` against a Discord that fails and succeeds;
two against a PERSISTENT fault - a recorded message deleted for good (`NotFound` on every
edit) - one over the fake store and one over a real SQLite database. All assert the menu
reaches a published state, a failed send never changes the row, and the row always names a
message that was really sent.

RED evidence: commit `7e4355a` (stubs that create nothing, find nothing and allow
everything) ran `cargo test --locked --offline role_menus` with
`test result: FAILED. 51 passed; 48 failed`. Only the code-stability tests passed.

Mutation evidence: 30 one-condition mutations, each caught by a named test: the gates, the
NotFound recovery, the K03 check, the option limit, the chunk size, the key, emoji and
label checks, the trimming, the `max_select`, the `posted` flag, the guild filter, the
stored-id check, the options delete, the foreign-key mapping, the mode fallback, the
duplicate key, the row written after a failed send, the reply, guild 0 and the empty menu.

## Config keys (READ by the wiring card, not by this module)

`ROLE_MENUS_MODE`: `off` (default) | `shadow` | `live`; unset or a typo means off, and
the wiring card passes `Settings::enabled = false`, so every command answers `disabled`.
`GUILD_ID`: the one home guild; unset or zero means no guild is served (fail closed).
`STAFF_ROLE_ID`: the role a caller must hold to configure a menu; unset means only the
guild owner may configure. `PROTECTED_ROLE_IDS`: the roles a menu may never grant; a
malformed entry makes the key invalid and the wiring card must then refuse every option.
This module reads no environment variable; the wiring card parses the keys and passes the
decision in.

## NOT done

No command is registered, no feature is switched on, nothing is called from `main.rs`, no
`config.rs` or `.env.example` change, no `Feature` implementation and no job kind. The
`MenuStore`, `MenuDiscord` and `RoleFactsSource` traits have no production implementation:
the wiring card writes them over the shared core database, serenity and the embed layer.
`src/embeds` is not called; the embed posting sits behind the `MenuDiscord` trait. Menu
interactions (a member clicking an option) are the later card RM3; this module never
writes a member role. The `#![allow(dead_code)]` in `src/role_menus/mod.rs` is temporary
(W1). Limits: a posted message whose record write then fails leaves the table without a
row, so the next `post` sends a second message; `post` and the Discord call are not one
transaction, so two `post` calls for one key at once can both send. A menu whose message
was deleted is re-sent on the next `post`. The label and emoji bounds are this module's
own limits. `split_chunks` copies RM1's `SELECT_OPTION_LIMIT` instead of calling RM1's
`split_components`, because RM1 splits by group and this module has one group per menu.

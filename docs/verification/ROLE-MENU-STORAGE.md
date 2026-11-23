# ROLE-MENU-STORAGE - role menu tables and `/rolemenu` logic (RM2)

New: `src/role_menus/repository.rs` (the SQLite tables) and `src/role_menus/command.rs`
(the command logic over three traits); `src/role_menus/mod.rs` gains two `pub mod` lines.
The RM1 policy (`src/role_menus/policy.rs`) is reused unchanged. Tables:
`role_menus(id INTEGER PK, guild TEXT, key TEXT UNIQUE, mode TEXT, max_select INTEGER,
channel TEXT, message TEXT, embed_key TEXT)` and `role_menu_options(menu_id INTEGER, role
TEXT, label TEXT, emoji TEXT, position INTEGER)`. The feature name in `schema_versions` is
`role_menus`; the repository runs its own migrations through `storage::migrate` and does
not touch `src/storage.rs`. A key is unique across the whole table; an option's role is
unique inside its menu.

## Rules the code enforces (each has a test)

1. Configuration needs the bot to hold "Manage Roles" (`bot_missing_manage_roles`), the
   caller to be the guild owner or to hold the configured staff role (`denied`), the guild
   to be the one home guild (`other_guild`), and an unset or zero home guild to fail closed
   (`no_home_guild`). A caller id of zero is never trusted.
2. A role is checked against K03 (`GrantPath::Menu`) before it is stored: a privileged,
   managed, protected, @everyone, deleted or at-or-above-the-bot role is refused with its own
   K03 code (`ineligible (privileged_permission)`, ...). An unreadable `PROTECTED_ROLE_IDS`
   list refuses every option (`ineligible`).
3. A key is one canonical spelling: lowercase ASCII, starting with a letter or a digit, then
   letters, digits, `-` or `_`, at most 64 bytes; every other spelling is `invalid_key` and
   is never echoed. A label is non-empty after trimming, at most 100 characters and free of
   control characters other than a newline (`invalid_label`); an emoji is non-empty after
   trimming, at most 64 characters and free of every control character, checked on the raw
   input (`invalid_emoji`).
4. `post` is idempotent: a menu that already names a channel and a message is EDITED in
   place (`edited`), a menu that does not is posted (`posted`). A failed send leaves the row
   unchanged and reports `discord_failed` or `discord_forbidden`. A menu with no options is
   `empty_menu` and nothing is sent. More than 25 options are split across messages in the
   stored order, in chunks of 25 (`SELECT_OPTION_LIMIT`), deterministically; no option is
   dropped.
5. Removing an option or a menu touches only the menu tables; no member role is written or
   removed anywhere in this module. Only the calling guild's menus are read and listed.
   Every reply is ephemeral and carries stable codes only: no member id, role name, channel
   name or free text.

## What is tested (53 new: 18 repository, 35 command)

Every rule above has a test, plus: all six privileged permission bits; a foreign guild
doing nothing on create, add, post and list; 60 options split into 25/25/10 with none
dropped and the split boundaries 0/1/25/26/50/51; the schema creating and re-opening
cleanly on an existing database; a newer stored schema version refused; a corrupt stored id
reported as `corrupt_row`. Two seeded LCG sequences (200 steps each): one drives `post`
against a Discord that fails and succeeds, asserting the menu is posted exactly once, that
a later call edits the recorded message, that a failed send never changes the row and that
the sequence ends posted; the other drives `option add`, asserting every role ends stored
exactly once.

RED evidence: commit `7e4355a` (stubs that create nothing, find nothing and allow
everything) ran `cargo test --locked --offline role_menus` with
`test result: FAILED. 51 passed; 48 failed`. Only the code-stability tests passed.

Mutation evidence: 20 one-condition mutations, each caught by a named test. Mutants tried:
post never edits; the staff, Manage Roles and foreign-guild checks removed; the K03 role
check made to allow; the chunk size 25 to 26; the options truncated to 25; the row written
after a failed send; the duplicate key swallowed; `remove_option` always reporting success;
the key check accepting uppercase; the emoji control check moved after trimming; the label
length bound removed; the unreadable protected-role list ignored; `list` reading every
guild; the unknown bot position treated as zero; posting an empty menu allowed; guild 0
accepted; a reply not ephemeral; removing a missing option reported as success.

## Config keys (READ by the wiring card, not by this module)

`ROLE_MENUS_MODE`: `off` (default) | `shadow` | `live`; unset or a typo means off.
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
interactions (a member clicking an option) are the later card RM3; this module never writes
a member role. The `#![allow(dead_code)]` in `src/role_menus/mod.rs` is temporary (W1).
Limits: a posted message whose record write then fails leaves the table without a row, so
the next `post` sends a second message; `post` and the Discord call are not one transaction,
so two `post` calls for one key at once can both send. The label and emoji bounds are this
module's own limits, not Discord's select-option limits, which the wiring card must respect.

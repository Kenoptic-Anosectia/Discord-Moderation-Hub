# Discord // Moderation Hub

Discord // Moderation Hub is a custom Discord bot built for a specific private server environment using Rust, [Poise](https://github.com/serenity-rs/poise), and [Serenity](https://github.com/serenity-rs/serenity). It handles moderation logging, role workflows, age-verification support tools, reaction roles, and other server-specific utility systems.

## Important

## This bot is **not** a generic drag-and-drop public bot.

This codebase was built explicitly for **my server**, with **my role structure**, **my channel layout**, **my moderation flow**, and **my hardcoded server IDs** in mind.

If someone else wants to use this code:
- It will **not** work correctly as-is.
- It is **not** plug-and-play.
- It will require manual changes before it can be used in another server.
- The most obvious required changes are things like **role IDs**, **channel IDs**, **log channel IDs**, **ticket category IDs**, and any other guild-specific constants.
- Some command behavior also assumes server-specific moderation logic and server-specific workflow expectations.

In other words: this repository is best treated as a **reference implementation** or **starting point**, not a ready-made public bot package.

## What the bot does

The bot currently includes or is being built to include systems such as:

- Moderation and server event logging
- Message delete and edit logging
- Voice state logging
- Invite logging
- Role create, update, and delete logging
- Member join, leave, kick, ban, and unban logging
- AutoMod rule logging
- Age-verification support commands
- Reaction role backend with slash commands
- Ticket-related support tooling

Several systems are tightly integrated with the structure of the target server, which is why reuse in other communities requires adaptation.

## Tech stack

- Rust
- Poise
- Serenity
- Tokio
- SQLite via `tokio-rusqlite` and `rusqlite`
- dotenvy
- serde / serde_json

## Setup

### 1. Clone the repository

```bash
git clone https://github.com/Kenoptic-Anosectia/Discord-Moderation-Hub
cd discord-moderation-hub
```

### 2. Create a `.env`

Create a `.env` file in the project root:

```env
DISCORD_TOKEN=your_bot_token_here
```

### 3. Update server-specific constants

Before compiling or running the bot, you must update all hardcoded IDs in the source code to match your own server.

This includes things such as:
- Role IDs
- Channel IDs
- Log channel IDs
- Ticket category IDs
- Any moderation-specific or workflow-specific constants

If you skip this step, parts of the bot will break, point to the wrong places, or silently fail.

### Staff-authorized age-role command

Configure `STAFF_ROLE_ID`, `AGE_ROLE_1` and `AGE_ROLE_2` in your local `.env`
(see `.env.example`; the values there are placeholders, not usable configuration).
`/age-verified user:<member>` requires the caller to hold the configured staff
role **in the invoking server**. Caller Manage Roles, Administrator or server
ownership alone does not authorize it; staff members do not need Manage Roles.
The bot still needs Manage Roles and Discord must permit it to assign the roles.

`STAFF_ROLE_ID` is required for this command, but remains optional for existing
logger/reaction functionality. Missing, malformed, zero, @everyone or nonexistent
staff-role configuration disables the command safely. Grant roles must exist in
the invoking server and cannot be zero, @everyone or the configured staff role.
No role's semantic meaning needs to be identified or renamed.

Each authorized invocation requests **both** configured roles, `AGE_ROLE_1` then
`AGE_ROLE_2`, additively. It never replaces unrelated roles. Already-held roles
and deliberate repeated invocations are supported, including restoring either
configured role after manual removal. Staff can remove channel access manually
later; there is no revocation database or separate restore command.

Replies are private and deferred before network work. Grants are two sequential
Discord requests, **not an atomic transaction**. A first failure stops the pair;
a second failure reports that the first request succeeded. There is no automatic
rollback/removal. A failed network response can leave Discord's final state
uncertain, so inspect the member before retrying. No full success is reported
unless both requests succeeded. Reply failures propagate a sanitized error.

The generated command no longer has a caller Manage Roles permission/visibility
gate. Existing Discord registrations or server-level command overrides may need
an authorized future refresh; this change does not register commands or deploy.

## Build and run

```bash
cargo run
```

For a release build:

```bash
cargo build --release
```

## Notes for reuse

Anyone adapting this bot for another server should expect to:
- replace all guild-specific constants,
- review permissions on every slash command,
- adjust logging expectations,
- update ticket flow behavior,
- update role logic,
- and test every command in a staging server before production use.

Again, this project was not designed as a universal public bot template. It was designed around one specific server’s operational needs.

## Project status

This bot is actively being refined and expanded. Some systems are complete, some are in progress, and some are intentionally scaffolded in stages while the server workflow is finalized.

## License

This repository is licensed under the MIT License unless otherwise noted.

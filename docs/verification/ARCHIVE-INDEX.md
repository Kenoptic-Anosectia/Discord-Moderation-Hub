# A17 — Archive index: authorization and opaque ids

Module: `src/tickets/archive_index.rs` (offline, not wired into the bot).

## Authorization
`list_archives` and `find_archive` first check, with no database access:
`settings.valid()`, `actor.guild == settings.guild`, `actor.user != 0`, and
(`actor.is_guild_owner` or `actor.roles` contains `settings.staff`). Anything else
returns `ManageError::Denied` — requesters, non-staff administrators, other guilds, user 0,
invalid settings. Tests prove this by dropping `ticket_archives` and still seeing `Denied`
(not `Db`) for every denied caller, while authorized callers reach the DB and get `Db`.
The check is repeated on every call.

## Archive id
`arc-` + first 32 lowercase hex chars of
`sha256("skynet-archive-id-v1\0{guild}\0{ticket}\0{seq}\0{manifest_sha256}")`.
`ArchiveId::parse` is strict (exact prefix, exactly 32 `[0-9a-f]`). The id is opaque: it
carries no path and callers never supply filesystem paths. It is NOT a secret or a
capability — knowing an id grants nothing; authorization is rechecked each call. Lookup
scans the caller's guild records and compares computed ids, so ids from another guild or
mutated ids yield `NotFound`; malformed input yields `BadId`. The id is never used as a
path or SQL fragment.

## Listing
Joins `ticket_archives` with `tickets` (both `support` and `age` kinds), restricted to
`tickets.guild == settings.guild`, ordered `ticket DESC, seq DESC`. Page size is
`min(limit, MAX_PAGE = 25)`; `limit == 0` returns an empty list; an offset past the end
returns an empty list. The listing reports what the database records only.

## Not covered
Verifying that archive files exist or match their manifest is A18's job.

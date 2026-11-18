# Staff archive command

`/ticket-archives action [id] [offset]` is guild-only and registered only with
enabled, valid ticket settings. The configured guild's owner or configured staff
role can use it. Replies are ephemeral, suppress mentions and omit absolute paths.

| Action | Reply and limits |
| --- | --- |
| `list` | Up to 10 recorded archives, one line each (format below), with a next-offset hint. |
| `show` | Message and asset counts for one recorded archive; requires `id`. |
| `health` | Verification counts for up to 50 recorded archives, the IDs of non-verified ones (first 10) and a next-offset hint. |
| `orphans` | Counts of unrecorded archives, staging debris and unknown entries in the archive root. |
| `retention` | Dry-run eligible count only; uses confirmed closure age in 86,400-second days. |
| `restore` | Verify and copy one recorded archive to a new local staging directory; requires `id`. |

`list` and `health` use a zero-based row offset (default 0, range 0–4,294,967,295)
and order by ticket ID descending, then archive sequence descending. IDs are
`arc-` plus 32 lowercase hex digits.

`list` prints `Archives on this page: N.` then one line per archive:
`arc-… ticket T seq S KIND STATUS M msgs A assets`. When more exist it ends with
`More archives are available: use offset X.` (X = offset + 10). The 10-row cap
keeps the worst case (20-digit numbers, longest kind and status) under Discord's
2000 characters. `health` prints the counts, then `Not verified (pass an ID to
show or restore):` with up to 10 IDs (`Showing the first 10 of N.` if cut) and, when
the 50-row page has more, `More archives are available: use offset X.` (X = offset + 50).
Kind and status are filtered to short lowercase words; no path, directory name,
mention or error text is ever printed.

Workflow: `list` (or `health` to find damaged archives) → copy an `arc-…` ID →
`show id:<ID>` to confirm counts → `restore id:<ID>` to copy it to staging.

The underlying orphan and retention reports sample at most 10 items per category;
the Discord replies show counts only, not the samples.

Both roots must open next to `TICKET_DB_PATH`: `ticket-archives` and
`ticket-restore-staging`. Only one restore can run at a time per bot process.
Each restore is capped at 10,000 files and 2 GiB including manifest and transcript.
Use bot-owned storage; do not let another writer change those directories.

`TICKET_ARCHIVE_RETENTION_DAYS` is off when unset, empty or whitespace. Otherwise
use ASCII digits for 1–36500 days (surrounding whitespace is trimmed). Invalid
values disable ticket settings and the startup diagnostic names only the variable.

The command cannot delete, move or rewrite archives, or export/upload contents.
Ticket opening remains hard-disabled. Verification was offline: live Discord,
including live command registration, was not tested.
# A20 archive management verification

Only the actual guild owner or a configured staff-role holder may manage archives, with valid settings, a matching guild and a nonzero user. Denial precedes ID parsing, restore locking, and all SQL/filesystem I/O; Administrator alone grants nothing.

| Operation | Reads and writes |
| --- | --- |
| List/show | Read guild-scoped index metadata only (`MAX_PAGE` = 25). Support and age tickets are both included. |
| Health | Reads one ordered page of up to 50 DB records (one lookahead), then verifies archive bytes; no writes. A missing, malformed or zero recorded channel is an identity mismatch without a byte scan for that row. Non-verified records are reported by opaque ID. |
| Orphans | Reads all recorded directory names for the guild and scans immediate archive-root children (scanner cap 10,000); no writes. Class counts, up to 10 basenames per class, and a truncation flag. Raw names are not rendered. |
| Retention | Dry run only. Reads confirmed closed tickets and the last transition into `closed` by history sequence; absent history is ineligible. Returns a count and up to 10 opaque IDs. Disabled does not query SQL. Nothing is deleted; automatic deletion stays off and `archive_remove` is deliberately unwired. |
| Restore | Resolves the opaque ID to recorded identity, then verifies and copies only into fresh staging under A16 limits (10,000 files, 2 GiB). No SQL or source writes. A mismatched digest on an otherwise self-verifying source is an identity failure, not a restore. |

A process-wide Tokio mutex uses `try_lock`; overlapping restores get Busy, not a queue. The blocking worker owns the guard so cancelling its waiter cannot release the gate early. The nonce is the greater of wall-clock Unix nanoseconds and the checked previous process nonce plus one. Occupied names (including survivors if the clock repeats after restart) are skipped with a no-follow metadata check before source verification; A16's exclusive creation is the final backstop. Roots and ancestors must remain exclusively owned, as in A07/A16. Renderers omit caller strings, paths, raw filenames, and error bodies.

## Failing-first evidence
The initial two focused RED runs and the review regression RED run exited 101. Each row shows an observed failed assertion; the same assertions subsequently passed against the implementation.

| Group | RED assertion |
| --- | --- |
| Authorization | `list authorization takes precedence` — `Err(Db)` vs `Err(Denied)` |
| List/show | `list keeps the index page cap` — 0 vs 25 |
| Health | `health must verify real archived bytes` — 0 vs 3 |
| Orphans | `orphans classify only unrecorded archive names` — `[]` vs `["ticket-9-9"]` |
| Retention | `configured dry run must be enabled` |
| Restore | `authorized restore must produce a staging copy: Failed("database")` |
| Serialization | `overlapping restore must return Busy` — `Failed("database")` vs `Busy` |
| Rendering | `every denial must use the same fixed reply` |
| Health channel regression | `bad recorded channels must not poison the health page` — `Err(Db)` vs one verified and three identity mismatches |
| Restore nonce regression | `first restore nonce must start at the epoch-time floor, not the legacy counter: 32` |

Fixtures use real temporary SQLite files, real owned roots, and shared synthetic archive fixtures on Windows. No Discord, network, production data, or live command wiring. A20 adds no symlink-creation test; link/reparse checks remain in the underlying archive modules. The only new lint allowance is the temporary module declaration annotation for A21 to remove. Visibility-only changes: `archive_index::authorized`, `archive_scan::is_archive_dir_name`, and `restore::{check_root, check_tree}` share the existing guards; their behavior is unchanged.

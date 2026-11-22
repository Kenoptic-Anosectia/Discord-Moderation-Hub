# EMBED-COMMAND - `/embed post|sync|list` logic (EM2)

Card EM2. `src/embeds/command.rs` (plain async functions `post`, `sync`, `list` over an `Env` of small traits, plus pure reply builders) and `src/embeds/config.rs` (parser from a lookup closure). It builds on EM1 (`posts::plan`, `validate_key`, `apply`, `list`, `get`; `template::load`). Not called from `main.rs`; no poise command, no feature registered, nothing switched on.

Rules it enforces (each has a test):
- Only the guild owner may use the commands; staff only when `EMBED_PUBLISHERS=staff`; a caller id of 0 is denied. The wiring card builds `Invoker` (user, guild owner, staff flag).
- A key is parsed with `posts::validate_key` before any lock, store or file access; any non-plain spelling is refused as `invalid_key` and never echoed. `DirTemplateSource` checks again before it builds `<dir>/<key>.json`, reads at most 256 KiB and only a regular file.
- `post` of an unchanged template does nothing; a changed one EDITS the recorded message; `sync` never creates (`not_recorded`) and edits only recorded posts; a deleted post is recreated only with the explicit flag.
- A send whose record write fails is deleted again; if that delete fails the code is `posted_not_recorded` and staff is alerted. An edit whose record write fails is `edited_not_recorded`; the next sync plans the edit again.
- One call per key at a time in this process (`KeyLocks`): a second call gets `busy`.
- Replies are `Reply` values with no public constructor and `ephemeral()` true; they carry stable codes and key names only, never template text, paths or values; alerts (`embeds: <code>`) carry the code alone. Embeds send with an empty allowed-mentions set.
- `shadow` makes no send, edit, delete, record write or alert; it returns `would_post/would_edit/would_recreate` codes and counts (`Report::counts`). `off` returns `disabled` before any call.
- No job kind: the commands answer a staff request and nothing is restart-sensitive, so `EmbedFeature` registers no job (`needs_core_db` is true for the `managed_posts` table).

Tests (61 new; 1,228 unit + 52 integration pass): hostile keys (51 spellings plus a seeded 400-key sweep against an independent oracle), owner/staff/denied for all three commands, idempotent sync, edit-not-duplicate, missing/recreate, other channel/guild, Discord and store failures, corrupt table, one alert per code per run, reply length (300 posts) and leak checks, shadow, busy key, config boundaries, real `TokioStore`, real files under `target/em2-tests/`, `render`. Seeded liveness: 4 LCG sequences x 600 steps (template changes, deleted posts, Discord and store faults) never show more posts per key than the orphans reported, then a quiet phase settles in one round and stays `unchanged` with zero writes; plus always-failing Discord and always-lost-write runs that alert on every call.
RED: commit `0965e90` (stubs): 57 of the 61 new tests failed. Mutations (30, all caught by a named test): staff always allowed, zero user trusted, forbidden lookup read as missing, no compensating delete, shadow posts/edits/alerts, lock never held/released, key validation off (command and file source), `@everyone` allowed, edit-record loss hidden, alert dedupe off, `updated_by` zero, lookup skipped, publishers typo widens, control chars in dir, mode without dir, reply limit +500, size limit off by one, invalid key echoed, failures uncounted, wrong `from` hash, public reply, edit-404 as failure, recreate forced, zero guild accepted, hash ignored, sync stops at first error.

Config keys (read by the wiring card, not here; `.env.example` to stage):
- `EMBED_MODE`: `off` (default) | `shadow` | `live`; a typo is `off`. (Added: the card names two keys, the design rules need a mode.)
- `EMBED_TEMPLATE_DIR`: unset = off; a mode that is on without it, a control character or over 1024 bytes = off and the error names the key.
- `EMBED_PUBLISHERS`: `owner` (default) | `staff`; any other value = off and the error names the key (K06 has no helper for this word, so it is parsed here, strictly).

NOT done: no command registration, no `main.rs`/`config.rs`/`.env.example` change, `SerenityDiscord`/`SerenityAlerts` never run against Discord (only `render`, `classify_status` and the mention object are tested), wiring must call `TokioStore::migrate` and share one `KeyLocks`. The lock is per process, not per database. If an edit succeeds and its record write is lost and the template then goes back to the older version, the row matches it and the post stays on the newer one (EM1's diverged-record limit).

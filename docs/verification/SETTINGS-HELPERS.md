# Settings Helpers — SETTINGS-HELPERS

## What
Pure parsing helpers in `src/settings.rs`: `parse_mode`, `parse_id_list`, `parse_duration_secs`. No I/O, no serenity types.

## Rules enforced
1. **Mode** — `off`, `shadow`, `live` (case-insensitive, trimmed). Anything else → `Off`. Unset → `Off`.
2. **ID list** — comma-separated u64. Rejects 0, negatives, Unicode digits, overflow, leading `+`. Empty interior entries (`,,`) rejected. One bad entry → error. Deduped, sorted.
3. **Duration** — integer seconds in `[min, max]`. Rejects negatives, Unicode digits, overflow, leading `+`, non-numeric.
4. **Errors** — `SettingError` carries the key name (`String`) and a stable code. Never echoes values.

## What is tested (40 tests)
- Mode: off/shadow/live accepted; case-insensitive; trimmed; `true`/`1`/`yes`/typos → Off; unset → Off.
- ID list: dedupe, sort, reject 0, reject overflow, reject negative, reject Unicode digits, reject `+5`, empty → empty, one bad entry → error, reject empty interior entries, accept u64 max.
- Duration: min OK, max OK, below min error, above max error, non-numeric error, negative error, unset error, reject `+60`, reject overflow (u64::MAX+1 with max=u64::MAX), reject longer overflow strings.
- Error privacy: sentinel `S3CR3T` as value never appears in Display/Debug output.
- Key-in-display: each parser's error Display contains the key name.

## RED evidence (failing tests before fix commit `d714196`)
The RED commit `0f8bb0c` added 5 key-in-display tests that failed against the old code:
- `parse_id_list_error_display_contains_key` — Display was `id: parse error` (wrong key)
- `parse_duration_secs_error_display_contains_key` — Display was `duration: parse error`
- `parse_duration_unset_error_contains_key` — Display was `duration: unset`
- `parse_duration_out_of_range_error_contains_key` — Display was `duration: out of range`
- `parse_id_list_empty_entry_error_contains_key` — FAILED at `unwrap_err()` (old code returned Ok for `100,,200`)
Fix commit `d714196` carried `key` into `SettingError` variants and fixed empty-entry rejection.

## Config keys (for wiring card)
| Key | Default | Safe-failure |
|-----|-------|-------------|
| `<FEATURE>_MODE` | Off | Typo → Off |
| `<FEATURE>_PROTECTED_ROLE_IDS` | empty | Bad entry → error (whole list rejected) |
| `<FEATURE>_*_SECS` | unset → error | Out of range → error |

## NOT done
No runtime, no Discord commands, no persistence, no `.env` reading (takes closures), no live wiring.

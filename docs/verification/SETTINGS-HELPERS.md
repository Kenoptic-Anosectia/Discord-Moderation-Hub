# Settings Helpers — SETTINGS-HELPERS

## What
Pure parsing helpers in `src/settings.rs`: `parse_mode`, `parse_id_list`, `parse_duration_secs`. No I/O, no serenity types.

## Rules enforced
1. **Mode** — `off`, `shadow`, `live` (case-insensitive, trimmed). Anything else → `Off`. Unset → `Off`.
2. **ID list** — comma-separated u64. Rejects 0, negatives, Unicode digits, overflow, leading `+`. Empty interior entries (`,,`) rejected. One bad entry → error. Deduped, sorted.
3. **Duration** — integer seconds in `[min, max]`. Rejects negatives, Unicode digits, overflow, leading `+`, non-numeric.
4. **Errors** — `SettingError` carries the key name (`String`) and a stable code. Never echoes values.

## What is tested (40 tests)
- Mode: off/shadow/live; case-insensitive; trimmed; `true`/`1`/`yes`/typos → Off; unset → Off.
- ID list: dedupe, sort, reject 0/overflow/negative/Unicode/`+5`, empty→empty, bad entry→error, reject empty interior, accept u64 max.
- Duration: min/max OK, below/above error, non-numeric/negative/unset error, reject `+60`, reject overflow (u64::MAX+1 with max=u64::MAX).
- Privacy: sentinel `S3CR3T` never appears in Display/Debug. Key-in-display: each parser's error contains the key name.

## RED evidence
RED commit `0f8bb0c` added 5 key-in-display tests that failed against old code: 4 returned wrong key name in Display (e.g. `"id: parse error"` instead of the passed key); `parse_id_list_empty_entry_error_contains_key` failed at `unwrap_err()` (old code returned Ok for `100,,200`). Fix commit `d714196` carried `key` into `SettingError` and fixed empty-entry rejection.

## Config keys (per-feature patterns for wiring card)
| Pattern | Default | Safe-failure |
|---------|---------|-------------|
| `<FEATURE>_MODE` | Off | Typo → Off |
| `<FEATURE>_PROTECTED_ROLE_IDS` | empty | Bad entry → error |
| `<FEATURE>_*_SECS` | unset → error | Out of range → error |

## NOT done
No runtime, no Discord commands, no persistence, no `.env` reading (takes closures), no live wiring.

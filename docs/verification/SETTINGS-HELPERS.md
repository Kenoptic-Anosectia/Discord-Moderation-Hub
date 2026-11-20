# Settings Helpers — SETTINGS-HELPERS

## What
Pure parsing helpers in `src/settings.rs` for reading `.env` feature keys via closures.
Three functions: `parse_mode`, `parse_id_list`, `parse_duration_secs`. No I/O, no serenity types.

## Rules enforced
1. **Mode** — `off`, `shadow`, `live` (case-insensitive, trimmed). Anything else → `Off`. Unset → `Off`.
2. **ID list** — comma-separated u64. Rejects 0, negatives, Unicode digits, overflow. One bad entry → whole list errors. Deduped, sorted.
3. **Duration** — integer seconds in `[min, max]`. Rejects negatives, Unicode digits, overflow, non-numeric.
4. **Errors** — `SettingError` carries only the key name and a stable code. Never echoes values.

## What is tested (30 tests)
- Mode: off/shadow/live accepted; case-insensitive; trimmed; `true`/`1`/`yes`/typos → Off; unset → Off.
- ID list: dedupe, sort, reject 0, reject overflow (`18446744073709551616`), reject negative, reject Unicode digits (`１２３`), empty → empty, one bad entry → error.
- Duration: min OK, max OK, below min error, above max error, non-numeric error, negative error, unset error.
- Error privacy: sentinel `S3CR3T` as value never appears in Display/Debug output.

## Config keys (for wiring card)
| Key | Default | Safe-failure |
|-----|---------|-------------|
| `FEATURE_MODE` | Off | Typo → Off |
| `PROTECTED_IDS` | empty | Bad entry → empty |
| `DURATION_SECS` | unset → error | Out of range → error |

## NOT done
No runtime, no Discord commands, no persistence, no `.env` reading (takes closures), no live wiring.

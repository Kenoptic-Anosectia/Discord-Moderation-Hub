# FLASH-POLICY - flash-media expiry policy (pure)

Card FL1. Module `src/flash/{mod,policy}.rs` - pure, Discord-free core.

`flash::policy::decide(&MessageFacts, &FlashConfig) -> Decision` is the decision core of the flash-media feature that replaces the old YAGPDB command: `Decision::Expire(ExpirePlan)` or `Decision::Ignore(IgnoreReason)`. No serenity type, no I/O, no clock read, no randomness; `now_ms` and every id are passed in.

Rules it enforces:
- Bot and webhook authors ignored; zero author/message ids ignored; 0 attachments ignored (parity with the original command).
- Only configured channels are flash channels. A thread of a flash channel follows `include_threads` (default true, that favours privacy).
- Notice for 180 s is exactly `<@&ROLE> New flash from <@USER> in <#CHANNEL>. This will be deleted in 3 minutes.` Without a role the role mention is omitted. A whole number of minutes renders in minutes ("1 minute", "N minutes"); anything else in seconds ("N seconds"). The original's "1 minutes" flaw is not copied.
- Allowed mentions are exactly the configured role and the author; `@everyone`/`@here` never, and no id from message text ever: text is not an input of the policy.
- Config: delay must be `10..=86400` s; an empty channel list is valid and disables the feature; duplicate channel ids removed; id 0 refused. Errors name the key, never the value. `deadline(now_ms, delay_secs)` saturates at `u64::MAX` instead of wrapping.
- `is_flash_channel` mirrors `decide` exactly (what the timer acts on). `is_flash_area` is the logger/cache exclusion and is never weaker (a thread of a flash channel is excluded even when the timer does not act on it).

Config keys (read by the wiring card, not by this module), default then safe failure: `FLASH_CHANNEL_IDS` empty=off, empty => feature disabled and every message ignored; `FLASH_PING_ROLE_ID` unset, unset => notice carries no role mention; `FLASH_DELAY_SECS` `180`, outside `10..=86400` => config refused with the key named and the value not echoed; `FLASH_INCLUDE_THREADS` `true`, `false` => threads of a flash channel not expired.

Tested (21 unit tests in `src/flash/policy.rs`): expire on a non-bot message with attachments; bot, webhook, no-attachment, other-channel, other-guild, zero-id and disabled ignores; thread handling both ways; byte-for-byte notice text for 180 s; the 60/120/90/45/10 s duration rules; the no-role notice; allowed mentions exactly {role, author}; message content unable to widen the mentions; delay bounds 10 and 86400 accepted and 9/86401/0/`u64::MAX` refused with no value in the error; duplicate/zero channel ids; the `is_flash_channel`/`decide` sweep; the deadline saturation boundary; the deletion pair. RED evidence: commit `3d40ef9` ran this suite with the implementation absent - 14 failed, 4 passed (the 4 were already satisfied by the type definitions alone).

NOT done here: no runtime (nothing posts, deletes or schedules); no Discord registration, commands, event wiring or `.env`/`config.rs` change; no persistence (the deletion pair is plain data for the durable-jobs card FL2/KJ2); the logger/cache exclusion (K05/FL2) is not implemented, this module only exposes `is_flash_area` for it; the module-level `#![allow(dead_code)]` in `src/flash/mod.rs` is temporary and W1 removes it.

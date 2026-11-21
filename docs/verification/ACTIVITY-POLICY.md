# ACTIVITY-POLICY - activity accounting policy (pure)

Card AC1. Module `src/activity/{mod,policy}.rs` - pure, Discord-free core.

`activity::policy::account_message(&MessageEvent, &ActivityConfig) -> MessageOutcome`
(`Counted(Increment)` | `Ignored(IgnoreReason)`) and
`activity::policy::Ledger::apply_voice(&VoiceEvent, &ActivityConfig) -> VoiceOutcome`
(`sessions` + `anomalies`), plus `Ledger::note_restart(now_at, last_heartbeat_at, cfg)`.
No serenity type, no I/O, no clock read, no randomness; every timestamp and id is passed in.

Rules it enforces:
- One message gives +1 for the key (UTC day, user, channel). The event type has NO content field, so content cannot be read or stored.
- Bots, excluded channels and zero ids are ignored (reason codes: `BotAuthor`, `ExcludedChannel`, `InvalidUser`/`InvalidChannel`/`InvalidGuild`, checked in that order).
- Voice: join opens, leave and move close, a state update in the same channel only changes the self-deaf flag. AFK and excluded channels are not credited (`UnaccountedChannel`).
- Self-deafened time counts only when `count_self_deafened` is true (wiring default false). A flag change mid-session splits the stretch, so only the deafened part is dropped.
- A session crossing UTC midnight is split into one record per day; each record lies inside one day.
- Duplicate join (`DuplicateUpdate`), out-of-order update (`OutOfOrderUpdate`), channel mismatch (`ChannelMismatch`) and missing old state (`MissingOldState`) raise an anomaly and credit nothing.
- Restart: every open session closes at the LAST HEARTBEAT; the gap after it is `RestartGap` and is not credited (crediting offline hours would inflate levels).
- All time is injected. `utc_day` uses integer division only and clamps a timestamp outside `YYYY-MM-DD` to `9999-12-31`; it does not panic. The day-split loop advances by at least one second whatever the boundary is.

Assumptions (the spec was silent): the `at` field is seconds since the Unix epoch; `utc_day` is `YYYY-MM-DD`; a text message in the AFK channel IS counted (AFK is a voice concept); a move whose old state is unknown opens a session in the NEW channel only.

Config keys (READ by the wiring card, not by this module; this module reads no env var and no file, so the parsing rules below are a REQUIREMENT for that card, not behaviour this module enforces):
`ACTIVITY_MODE` default `off` (the wiring card must keep an unrecognised value off); `ACTIVITY_EXCLUDED_CHANNEL_IDS` default empty (empty => nothing excluded; the wiring card must fail the whole key closed on a malformed entry rather than drop the entry); `ACTIVITY_AFK_CHANNEL_ID` default unset (unset => no AFK channel); `ACTIVITY_COUNT_SELF_DEAFENED` default `false` (false => self-deafened time is not credited).

Tested: 62 unit tests in `src/activity/policy.rs`. RED evidence: commit `2deca4e` ran the suite with a non-functional scaffold - 9 passed, 23 failed (the 9 that passed are the ones the type definitions alone satisfy: config validation, config error hygiene, anomaly code stability, the counter sum and the empty-session sweep). Then implemented in `3a5964d`.
Mutation evidence: 25 deliberate mutations were each caught by a named test - drop the bot check, the excluded-channel check, the three zero-id checks, the out-of-order guard, the AFK guard, the self-deaf guard, the midnight split, the day floor, the next-day distance; swap each of the five anomaly kinds for another; zero the credited user id, start, end and seconds; close the restart at the wrong time; credit the restart gap. The battery found three real gaps that were then closed by new tests (`the_day_start_floors_a_timestamp_before_the_epoch`, `the_next_day_start_is_the_day_after_the_day_start`, `each_anomaly_kind_has_its_own_code`) and one robustness defect (the day-split loop could stop advancing), fixed in `46a8cbc`.

NOT done: no runtime, no Discord registration, no commands, no event wiring, no `.env`/`config.rs` change, no persistence (the increments and sessions are plain data for AC2), no heartbeat source (the caller passes the last heartbeat in), no XP or stats logic. The module-level `#![allow(dead_code)]` in `src/activity/mod.rs` is temporary and wiring card W1 removes it.

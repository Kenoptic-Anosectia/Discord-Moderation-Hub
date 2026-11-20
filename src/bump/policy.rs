//! Pure decision logic for the bump confirmation and reminder cycle.
//!
//! This is the Discord-free core of the feature that replaces Bump Buddy. Nothing is known
//! about Bump Buddy's exact behaviour beyond "reminds after a Disboard bump", so where the
//! card is silent the conservative choice is made and every assumption is listed in
//! `docs/verification/BUMP-POLICY.md`.
//!
//! Everything here is a pure function of plain data. There is no serenity type, no I/O, no
//! clock read and no randomness: `now` (unix seconds) and every id are passed in.
//!
//! A message is a bump confirmation only when ALL of these hold ([`decide`]): it is not a
//! webhook message, it is in the configured guild, its author id equals the configured
//! listing-bot id, the author is a bot account, its id is a real id newer than the last
//! confirmed message, and its text or embed text contains the configured success marker.
//!
//! Errors and ignore reasons are stable enum codes. They never carry message text, names or
//! user ids, so nothing private can leak through an error path.
//!
//! Wiring card W1 removes the module-level `allow(dead_code)` in `super::mod`.

use std::fmt;

/// Environment key holding the listing bot's id. No default: unset means the feature is off.
///
/// Read by the wiring card, not by this module.
pub const KEY_BOT_ID: &str = "BUMP_BOT_ID";
/// Environment key holding the success marker text. Default: [`DEFAULT_SUCCESS_MARKER`].
pub const KEY_SUCCESS_MARKER: &str = "BUMP_SUCCESS_MARKER";
/// Environment key holding the reminder interval in seconds. Default: [`DEFAULT_INTERVAL_SECS`].
pub const KEY_INTERVAL_SECS: &str = "BUMP_INTERVAL_SECS";

/// Default success marker (matched case-sensitively as a substring).
pub const DEFAULT_SUCCESS_MARKER: &str = "Bump done";
/// Default reminder interval in seconds (2 hours, the listing bot's public cooldown; to be
/// verified in the test guild).
pub const DEFAULT_INTERVAL_SECS: u64 = 7_200;
/// Smallest accepted reminder interval in seconds (10 minutes).
pub const MIN_INTERVAL_SECS: u64 = 600;
/// Largest accepted reminder interval in seconds (24 hours).
pub const MAX_INTERVAL_SECS: u64 = 86_400;
/// Longest accepted success marker, in bytes.
pub const MAX_MARKER_LEN: usize = 200;

/// Stable reason codes for a configuration that was refused. The codes never carry the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// The guild id was zero, which is not a real guild.
    InvalidGuildId,
    /// The listing-bot id was zero, which is not a real user.
    InvalidBotId,
    /// The marker was empty, blank, too long or contained control characters.
    InvalidMarker,
    /// The interval was not plain digits or was outside `MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS`.
    IntervalInvalid,
}

/// A refused configuration. Names the key that was wrong and never echoes the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    key: &'static str,
}

impl ConfigError {
    fn new(kind: ConfigErrorKind, key: &'static str) -> Self {
        Self { kind, key }
    }

    /// The stable reason code.
    pub fn kind(&self) -> ConfigErrorKind {
        self.kind
    }

    /// The configuration key that was refused.
    pub fn key(&self) -> &'static str {
        self.key
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bump config {} refused: {:?}", self.key, self.kind)
    }
}

impl std::error::Error for ConfigError {}

/// Validated bump configuration. Fields are private so the bounds cannot be bypassed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BumpConfig {
    guild_id: u64,
    bot_id: u64,
    success_marker: String,
    interval_secs: u64,
}

/// Plain facts about one observed message. Built by the caller from the gateway event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageFacts {
    pub message_id: u64,
    /// `None` for a message outside any guild (a direct message).
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub author_id: u64,
    pub author_is_bot: bool,
    pub is_webhook: bool,
    pub content: String,
    /// All embed text (title, description, fields, footer) joined by the caller.
    pub embeds_text: String,
    /// Slash-command name the message answers, if known. Carried for the wiring card but
    /// not used by [`decide`] (see the assumptions in `BUMP-POLICY.md`).
    pub interaction_name: Option<String>,
}

/// Reminder-cycle state. Persisting it is the job of a later card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    /// Unix seconds of the confirmation (or manual reset) that started the current cycle.
    pub last_confirmed_at: Option<i64>,
    /// Id of the last confirmed message; used for idempotency and ordering.
    pub last_message_id: Option<u64>,
    /// True once the reminder for the current cycle has been marked as sent.
    pub reminded_for_cycle: bool,
}

/// Why a message was not counted. Stable codes, no message data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// The message came from a webhook.
    Webhook,
    /// The message was outside the configured guild (or in no guild).
    WrongGuild,
    /// The author is not the configured listing bot (a user, or another bot).
    NotListingBot,
    /// The author id matches but the author is not a bot account.
    NotBotAccount,
    /// The message id was zero.
    InvalidMessageId,
    /// Neither the text nor the embed text contains the success marker (cooldown, error, chatter).
    NoSuccessMarker,
    /// This message id was already confirmed, or is older than the last confirmed message.
    AlreadySeen,
}

/// A genuine bump confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Confirmation {
    pub message_id: u64,
    pub confirmed_at: i64,
    /// When the next reminder becomes due.
    pub next_due: i64,
}

/// Outcome of [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Confirmed(Confirmation),
    Ignored(IgnoreReason),
}

/// Where the current cycle stands, as seen at `now`. See [`reminder_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderStatus {
    /// No cycle has started yet, so there is nothing to remind about.
    NoCycle,
    /// The cycle is running; the reminder becomes due at `due_at`.
    NotYet { due_at: i64 },
    /// The reminder is due now and has not been marked as sent.
    Due { due_at: i64 },
    /// The reminder for this cycle was already marked as sent.
    AlreadyReminded { due_at: i64 },
}

impl BumpConfig {
    /// Validate and build a configuration. (RED stub: refuses everything.)
    pub fn new(
        _guild_id: u64,
        _bot_id: u64,
        _success_marker: &str,
        _interval_secs: u64,
    ) -> Result<Self, ConfigError> {
        Err(ConfigError::new(
            ConfigErrorKind::InvalidMarker,
            KEY_SUCCESS_MARKER,
        ))
    }

    pub fn guild_id(&self) -> u64 {
        self.guild_id
    }
    pub fn bot_id(&self) -> u64 {
        self.bot_id
    }
    pub fn success_marker(&self) -> &str {
        &self.success_marker
    }
    pub fn interval_secs(&self) -> u64 {
        self.interval_secs
    }
}

/// Parse the raw `BUMP_INTERVAL_SECS` value. (RED stub.)
pub fn parse_interval_secs(_raw: Option<&str>) -> Result<u64, ConfigError> {
    Err(ConfigError::new(
        ConfigErrorKind::IntervalInvalid,
        KEY_INTERVAL_SECS,
    ))
}

/// Decide whether a message is a bump confirmation. (RED stub.)
pub fn decide(_facts: &MessageFacts, _config: &BumpConfig, _state: &State, _now: i64) -> Decision {
    Decision::Ignored(IgnoreReason::NoSuccessMarker)
}

impl State {
    /// The state after a confirmation. (RED stub.)
    pub fn confirmed(&self, _c: &Confirmation) -> State {
        *self
    }
    /// The state after a manual reset. (RED stub.)
    pub fn reset(&self, _now: i64) -> State {
        *self
    }
    /// The state after the reminder was sent. (RED stub.)
    pub fn reminded(&self) -> State {
        *self
    }
}

/// Decide and apply in one step. (RED stub.)
pub fn observe(
    facts: &MessageFacts,
    config: &BumpConfig,
    state: &State,
    now: i64,
) -> (Decision, State) {
    (decide(facts, config, state, now), *state)
}

/// Where the current cycle stands. (RED stub.)
pub fn reminder_status(_state: &State, _config: &BumpConfig, _now: i64) -> ReminderStatus {
    ReminderStatus::NoCycle
}

/// True when the reminder should be sent now. (RED stub.)
pub fn due(_state: &State, _config: &BumpConfig, _now: i64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // Clearly synthetic ids; none is a real Discord id.
    const GUILD: u64 = 1_000_001;
    const OTHER_GUILD: u64 = 1_000_002;
    const BOT: u64 = 2_000_001;
    const OTHER_BOT: u64 = 2_000_002;
    const USER: u64 = 3_000_001;
    const CHANNEL: u64 = 4_000_001;
    const NOW: i64 = 1_700_000_000;

    fn config() -> BumpConfig {
        BumpConfig::new(GUILD, BOT, DEFAULT_SUCCESS_MARKER, DEFAULT_INTERVAL_SECS).unwrap()
    }

    fn success(message_id: u64) -> MessageFacts {
        MessageFacts {
            message_id,
            guild_id: Some(GUILD),
            channel_id: CHANNEL,
            author_id: BOT,
            author_is_bot: true,
            is_webhook: false,
            content: String::new(),
            embeds_text: "Bump done! Check it out on the listing.".to_string(),
            interaction_name: Some("bump".to_string()),
        }
    }

    fn confirmed(d: Decision) -> Confirmation {
        match d {
            Decision::Confirmed(c) => c,
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    fn running_state(config: &BumpConfig) -> State {
        let (d, s) = observe(&success(100), config, &State::default(), NOW);
        confirmed(d);
        s
    }

    // ---- confirmation rules -------------------------------------------------------------

    #[test]
    fn listing_bot_in_guild_with_marker_in_embed_is_confirmed() {
        let c = config();
        let d = decide(&success(100), &c, &State::default(), NOW);
        assert_eq!(
            d,
            Decision::Confirmed(Confirmation {
                message_id: 100,
                confirmed_at: NOW,
                next_due: NOW + 7_200,
            })
        );
    }

    #[test]
    fn marker_in_plain_content_is_confirmed_too() {
        let mut m = success(100);
        m.embeds_text.clear();
        m.content = "Bump done!".to_string();
        assert!(matches!(
            decide(&m, &config(), &State::default(), NOW),
            Decision::Confirmed(_)
        ));
    }

    #[test]
    fn interaction_name_does_not_change_the_decision() {
        for name in [None, Some("bump".to_string()), Some("anything".to_string())] {
            let mut m = success(100);
            m.interaction_name = name;
            assert!(matches!(
                decide(&m, &config(), &State::default(), NOW),
                Decision::Confirmed(_)
            ));
        }
    }

    #[test]
    fn user_with_identical_text_is_ignored() {
        let mut m = success(100);
        m.author_id = USER;
        m.author_is_bot = false;
        assert_eq!(
            decide(&m, &config(), &State::default(), NOW),
            Decision::Ignored(IgnoreReason::NotListingBot)
        );
    }

    #[test]
    fn user_carrying_the_listing_bot_id_but_not_a_bot_account_is_ignored() {
        let mut m = success(100);
        m.author_is_bot = false;
        assert_eq!(
            decide(&m, &config(), &State::default(), NOW),
            Decision::Ignored(IgnoreReason::NotBotAccount)
        );
    }

    #[test]
    fn another_bot_with_identical_text_is_ignored() {
        let mut m = success(100);
        m.author_id = OTHER_BOT;
        assert_eq!(
            decide(&m, &config(), &State::default(), NOW),
            Decision::Ignored(IgnoreReason::NotListingBot)
        );
    }

    #[test]
    fn webhook_with_the_listing_bot_id_is_ignored() {
        let mut m = success(100);
        m.is_webhook = true;
        assert_eq!(
            decide(&m, &config(), &State::default(), NOW),
            Decision::Ignored(IgnoreReason::Webhook)
        );
    }

    #[test]
    fn webhook_impersonating_with_a_different_id_is_ignored() {
        let mut m = success(100);
        m.is_webhook = true;
        m.author_id = USER;
        assert_eq!(
            decide(&m, &config(), &State::default(), NOW),
            Decision::Ignored(IgnoreReason::Webhook)
        );
    }

    #[test]
    fn cooldown_or_error_reply_from_the_listing_bot_is_ignored() {
        for text in [
            "Please wait another 1 hour and 59 minutes until the server can be bumped",
            "Something went wrong",
            "",
            "bump done", // different case: the marker match is case-sensitive
        ] {
            let mut m = success(100);
            m.embeds_text = text.to_string();
            assert_eq!(
                decide(&m, &config(), &State::default(), NOW),
                Decision::Ignored(IgnoreReason::NoSuccessMarker),
                "text {text:?}"
            );
        }
    }

    #[test]
    fn cooldown_reply_does_not_reset_a_running_cycle() {
        let c = config();
        let before = running_state(&c);
        let mut m = success(200);
        m.embeds_text = "Please wait another 90 minutes".to_string();
        let (d, after) = observe(&m, &c, &before, NOW + 3_000);
        assert_eq!(d, Decision::Ignored(IgnoreReason::NoSuccessMarker));
        // The caller observes the same due time as before the cooldown reply.
        assert_eq!(
            reminder_status(&after, &c, NOW + 3_000),
            reminder_status(&before, &c, NOW + 3_000)
        );
        assert_eq!(
            reminder_status(&after, &c, NOW + 3_000),
            ReminderStatus::NotYet {
                due_at: NOW + 7_200
            }
        );
        assert!(due(&after, &c, NOW + 7_200));
    }

    #[test]
    fn different_guild_and_direct_message_are_ignored() {
        let mut m = success(100);
        m.guild_id = Some(OTHER_GUILD);
        assert_eq!(
            decide(&m, &config(), &State::default(), NOW),
            Decision::Ignored(IgnoreReason::WrongGuild)
        );
        m.guild_id = None;
        assert_eq!(
            decide(&m, &config(), &State::default(), NOW),
            Decision::Ignored(IgnoreReason::WrongGuild)
        );
    }

    #[test]
    fn message_id_zero_is_ignored() {
        assert_eq!(
            decide(&success(0), &config(), &State::default(), NOW),
            Decision::Ignored(IgnoreReason::InvalidMessageId)
        );
    }

    #[test]
    fn same_message_id_twice_yields_one_confirmation() {
        let c = config();
        let (first, s1) = observe(&success(100), &c, &State::default(), NOW);
        assert!(matches!(first, Decision::Confirmed(_)));
        let (second, s2) = observe(&success(100), &c, &s1, NOW + 5);
        assert_eq!(second, Decision::Ignored(IgnoreReason::AlreadySeen));
        // The duplicate did not move the due time.
        assert_eq!(
            reminder_status(&s2, &c, NOW + 5),
            ReminderStatus::NotYet {
                due_at: NOW + 7_200
            }
        );
    }

    #[test]
    fn older_message_id_than_the_last_confirmed_is_ignored() {
        let c = config();
        let (_, s) = observe(&success(500), &c, &State::default(), NOW);
        assert_eq!(
            decide(&success(499), &c, &s, NOW + 10),
            Decision::Ignored(IgnoreReason::AlreadySeen)
        );
        assert!(matches!(
            decide(&success(501), &c, &s, NOW + 10),
            Decision::Confirmed(_)
        ));
    }

    #[test]
    fn a_new_confirmation_restarts_the_cycle_and_reopens_the_reminder() {
        let c = config();
        let s = running_state(&c).reminded();
        assert_eq!(
            reminder_status(&s, &c, NOW + 8_000),
            ReminderStatus::AlreadyReminded {
                due_at: NOW + 7_200
            }
        );
        let later = NOW + 9_000;
        let (d, s2) = observe(&success(101), &c, &s, later);
        assert_eq!(confirmed(d).next_due, later + 7_200);
        assert_eq!(
            reminder_status(&s2, &c, later),
            ReminderStatus::NotYet {
                due_at: later + 7_200
            }
        );
        assert!(due(&s2, &c, later + 7_200));
    }

    // ---- reminder cycle -----------------------------------------------------------------

    #[test]
    fn no_cycle_means_nothing_is_due_even_when_marked_reminded() {
        let c = config();
        assert_eq!(
            reminder_status(&State::default(), &c, NOW),
            ReminderStatus::NoCycle
        );
        assert!(!due(&State::default(), &c, i64::MAX));
        let odd = State {
            last_confirmed_at: None,
            last_message_id: None,
            reminded_for_cycle: true,
        };
        assert_eq!(reminder_status(&odd, &c, i64::MAX), ReminderStatus::NoCycle);
    }

    #[test]
    fn due_flips_exactly_at_last_confirmed_plus_interval() {
        let c = config();
        let s = running_state(&c);
        assert!(!due(&s, &c, NOW));
        assert!(!due(&s, &c, NOW + 7_199));
        assert!(due(&s, &c, NOW + 7_200));
        assert!(due(&s, &c, NOW + 7_201));
        assert_eq!(
            reminder_status(&s, &c, NOW + 7_200),
            ReminderStatus::Due {
                due_at: NOW + 7_200
            }
        );
    }

    #[test]
    fn exactly_one_reminder_per_cycle() {
        let c = config();
        let mut st = running_state(&c);
        let mut sent = 0;
        for t in [
            NOW + 7_199,
            NOW + 7_200,
            NOW + 7_201,
            NOW + 80_000,
            NOW + 9_999_999,
        ] {
            if due(&st, &c, t) {
                sent += 1;
                st = st.reminded();
            }
        }
        assert_eq!(sent, 1);
        assert!(!due(&st, &c, i64::MAX));
        assert_eq!(
            reminder_status(&st, &c, NOW + 9_999_999),
            ReminderStatus::AlreadyReminded {
                due_at: NOW + 7_200
            }
        );
    }

    #[test]
    fn manual_reset_restarts_the_cycle_from_now() {
        let c = config();
        let reminded = running_state(&c).reminded();
        let reset_at = NOW + 10_000;
        let s = reminded.reset(reset_at);
        assert!(!due(&s, &c, reset_at));
        assert!(!due(&s, &c, reset_at + 7_199));
        assert!(due(&s, &c, reset_at + 7_200));
        assert_eq!(
            reminder_status(&s, &c, reset_at),
            ReminderStatus::NotYet {
                due_at: reset_at + 7_200
            }
        );
    }

    #[test]
    fn manual_reset_before_any_confirmation_starts_a_cycle() {
        let c = config();
        let s = State::default().reset(NOW);
        assert_eq!(
            reminder_status(&s, &c, NOW),
            ReminderStatus::NotYet {
                due_at: NOW + 7_200
            }
        );
    }

    #[test]
    fn manual_reset_keeps_dedupe_so_a_replayed_confirmation_cannot_undo_it() {
        let c = config();
        let s = running_state(&c).reset(NOW + 5_000);
        assert_eq!(
            decide(&success(100), &c, &s, NOW + 5_001),
            Decision::Ignored(IgnoreReason::AlreadySeen)
        );
    }

    #[test]
    fn time_arithmetic_saturates_instead_of_overflowing() {
        let c = config();
        let (d, s) = observe(&success(100), &c, &State::default(), i64::MAX - 5);
        assert_eq!(confirmed(d).next_due, i64::MAX);
        assert!(due(&s, &c, i64::MAX));
        assert!(!due(&s, &c, i64::MAX - 1));
        let s = State::default().reset(i64::MAX);
        assert_eq!(
            reminder_status(&s, &c, 0),
            ReminderStatus::NotYet { due_at: i64::MAX }
        );
    }

    // ---- configuration ------------------------------------------------------------------

    #[test]
    fn defaults_are_the_documented_values() {
        assert_eq!(DEFAULT_SUCCESS_MARKER, "Bump done");
        assert_eq!(DEFAULT_INTERVAL_SECS, 7_200);
        assert_eq!(parse_interval_secs(None).unwrap(), 7_200);
        let c = config();
        assert_eq!(c.guild_id(), GUILD);
        assert_eq!(c.bot_id(), BOT);
        assert_eq!(c.success_marker(), "Bump done");
        assert_eq!(c.interval_secs(), 7_200);
    }

    #[test]
    fn interval_bounds_are_enforced_by_the_constructor() {
        assert_eq!(MIN_INTERVAL_SECS, 600);
        assert_eq!(MAX_INTERVAL_SECS, 86_400);
        for ok in [MIN_INTERVAL_SECS, DEFAULT_INTERVAL_SECS, MAX_INTERVAL_SECS] {
            assert_eq!(
                BumpConfig::new(GUILD, BOT, "x", ok)
                    .unwrap()
                    .interval_secs(),
                ok
            );
        }
        for bad in [0, 1, MIN_INTERVAL_SECS - 1, MAX_INTERVAL_SECS + 1, u64::MAX] {
            let e = BumpConfig::new(GUILD, BOT, "x", bad).unwrap_err();
            assert_eq!(e.kind(), ConfigErrorKind::IntervalInvalid, "interval {bad}");
            assert_eq!(e.key(), KEY_INTERVAL_SECS);
        }
    }

    #[test]
    fn interval_parser_accepts_only_plain_digits_inside_the_bounds() {
        assert_eq!(parse_interval_secs(Some("600")).unwrap(), 600);
        assert_eq!(parse_interval_secs(Some("86400")).unwrap(), 86_400);
        assert_eq!(parse_interval_secs(Some("7200")).unwrap(), 7_200);
        for bad in [
            "",
            " ",
            "599",
            "86401",
            "0",
            "-7200",
            "+7200",
            "7_200",
            "7200 ",
            " 7200",
            "7200\r",
            "7200.0",
            "0x1c20",
            "1e4",
            "7200s",
            "\u{ff17}\u{ff12}\u{ff10}\u{ff10}",
            "99999999999999999999999999",
        ] {
            let e = parse_interval_secs(Some(bad)).unwrap_err();
            assert_eq!(e.kind(), ConfigErrorKind::IntervalInvalid, "value {bad:?}");
            assert_eq!(e.key(), KEY_INTERVAL_SECS);
        }
    }

    #[test]
    fn zero_ids_are_refused() {
        assert_eq!(
            BumpConfig::new(0, BOT, "x", 7_200).unwrap_err().kind(),
            ConfigErrorKind::InvalidGuildId
        );
        assert_eq!(
            BumpConfig::new(GUILD, 0, "x", 7_200).unwrap_err().kind(),
            ConfigErrorKind::InvalidBotId
        );
    }

    #[test]
    fn marker_must_be_plain_visible_bounded_text() {
        let long = "a".repeat(MAX_MARKER_LEN + 1);
        let at_limit = "a".repeat(MAX_MARKER_LEN);
        assert!(BumpConfig::new(GUILD, BOT, &at_limit, 7_200).is_ok());
        for bad in [
            "",
            "   ",
            "\t\n",
            "bump\u{0}done",
            "bump\ndone",
            "\u{7f}",
            long.as_str(),
        ] {
            let e = BumpConfig::new(GUILD, BOT, bad, 7_200).unwrap_err();
            assert_eq!(e.kind(), ConfigErrorKind::InvalidMarker, "marker {bad:?}");
            assert_eq!(e.key(), KEY_SUCCESS_MARKER);
        }
    }

    #[test]
    fn config_errors_do_not_echo_the_value() {
        let secret = "SECRET-MARKER-\u{0}";
        let e = BumpConfig::new(GUILD, BOT, secret, 7_200).unwrap_err();
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        let e = parse_interval_secs(Some("SECRET-12345")).unwrap_err();
        assert!(!format!("{e} {e:?}").contains("SECRET"));
    }

    #[test]
    fn custom_marker_is_matched_instead_of_the_default() {
        let c = BumpConfig::new(GUILD, BOT, "Bumped!", 7_200).unwrap();
        assert!(matches!(
            decide(&success(100), &c, &State::default(), NOW),
            Decision::Ignored(IgnoreReason::NoSuccessMarker)
        ));
        let mut m = success(100);
        m.embeds_text = "Server Bumped! :thumbsup:".to_string();
        assert!(matches!(
            decide(&m, &c, &State::default(), NOW),
            Decision::Confirmed(_)
        ));
    }

    #[test]
    fn configured_interval_drives_next_due_and_due() {
        let c = BumpConfig::new(GUILD, BOT, "Bump done", 600).unwrap();
        let (d, s) = observe(&success(100), &c, &State::default(), NOW);
        assert_eq!(confirmed(d).next_due, NOW + 600);
        assert!(!due(&s, &c, NOW + 599));
        assert!(due(&s, &c, NOW + 600));
    }

    // ---- liveness: seeded random event sequences against a model runtime ----------------

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *seed >> 33
    }

    /// A model runtime that polls `due`, marks the cycle reminded BEFORE sending (at-most-once)
    /// and treats a lost send as final. The policy must offer at most one reminder per cycle
    /// and must never stay quiet once a cycle is overdue and unreminded.
    #[test]
    fn seeded_sequences_offer_at_most_one_reminder_per_cycle_and_never_go_quiet() {
        let c = config();
        let interval = c.interval_secs() as i64;
        for seed0 in 0..60u64 {
            let mut seed = seed0 + 1;
            let mut now = NOW;
            let mut state = State::default();
            let mut next_id = 1_000u64;
            let mut last_confirmed_id = 0u64;
            // independent model of the cycle
            let mut cycle_start: Option<i64> = None;
            let mut cycle_reminded = false;
            let mut cycles_started = 0usize;
            let mut reminders = 0usize;

            for _ in 0..400 {
                now += 1 + (lcg(&mut seed) % 3_000) as i64;
                match lcg(&mut seed) % 8 {
                    0 => {
                        next_id += 1 + lcg(&mut seed) % 50;
                        let (d, s) = observe(&success(next_id), &c, &state, now);
                        assert_eq!(confirmed(d).next_due, now + interval);
                        state = s;
                        last_confirmed_id = next_id;
                        cycle_start = Some(now);
                        cycle_reminded = false;
                        cycles_started += 1;
                    }
                    1 => {
                        // noise: every hostile shape must leave the cycle alone
                        let mut m = success(next_id + 1 + lcg(&mut seed) % 50);
                        match lcg(&mut seed) % 6 {
                            0 => m.author_is_bot = false,
                            1 => m.author_id = OTHER_BOT,
                            2 => m.is_webhook = true,
                            3 => m.embeds_text = "Please wait".to_string(),
                            4 => m.guild_id = Some(OTHER_GUILD),
                            _ => m.author_id = USER,
                        }
                        let (d, s) = observe(&m, &c, &state, now);
                        assert!(matches!(d, Decision::Ignored(_)), "{d:?}");
                        assert_eq!(s, state);
                    }
                    2 if last_confirmed_id > 0 => {
                        let replay = 1 + lcg(&mut seed) % last_confirmed_id;
                        let (d, s) = observe(&success(replay), &c, &state, now);
                        assert_eq!(d, Decision::Ignored(IgnoreReason::AlreadySeen));
                        assert_eq!(s, state);
                    }
                    3 => {
                        state = state.reset(now);
                        cycle_start = Some(now);
                        cycle_reminded = false;
                        cycles_started += 1;
                    }
                    _ => {
                        let model_due = matches!(
                            cycle_start,
                            Some(t) if !cycle_reminded && now >= t + interval
                        );
                        assert_eq!(due(&state, &c, now), model_due, "seed {seed0}");
                        if model_due {
                            state = state.reminded();
                            cycle_reminded = true;
                            reminders += 1; // a lost send is not retried: still one per cycle
                        }
                    }
                }
            }
            // Drain: far in the future an unreminded cycle must fire exactly once, then stay quiet.
            now += interval * 10;
            let owes = cycle_start.is_some() && !cycle_reminded;
            assert_eq!(due(&state, &c, now), owes, "seed {seed0}");
            if owes {
                state = state.reminded();
                reminders += 1;
            }
            assert!(!due(&state, &c, now + interval * 10));
            assert!(reminders <= cycles_started, "seed {seed0}");
        }
    }
}

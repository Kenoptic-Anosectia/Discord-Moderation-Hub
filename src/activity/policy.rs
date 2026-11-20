//! RED SCAFFOLD: the types and the public API exist, the behaviour does not.
//! This file is the RED commit. It is replaced by the real implementation next.

use std::collections::BTreeMap;

/// Environment key holding the operating mode of the activity feature.
pub const KEY_MODE: &str = "ACTIVITY_MODE";
/// Environment key holding the comma separated list of excluded channel ids.
pub const KEY_EXCLUDED_CHANNEL_IDS: &str = "ACTIVITY_EXCLUDED_CHANNEL_IDS";
/// Environment key holding the "time spent self-deafened counts" flag.
pub const KEY_COUNT_SELF_DEAFENED: &str = "ACTIVITY_COUNT_SELF_DEAFENED";

/// Seconds in one day.
pub const SECONDS_PER_DAY: i64 = 86_400;

/// The plain facts about one incoming message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageEvent {
    pub guild_id: u64,
    pub channel_id: u64,
    pub user_id: u64,
    pub is_bot: bool,
    pub at: i64,
}

/// The plain facts about one voice state update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceEvent {
    pub guild_id: u64,
    pub user_id: u64,
    pub old_channel: Option<u64>,
    pub new_channel: Option<u64>,
    pub self_deaf: bool,
    pub at: i64,
}

/// Validated configuration of the activity accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityConfig {
    excluded_channels: Vec<u64>,
    afk_channel: Option<u64>,
    count_self_deafened: bool,
}

impl ActivityConfig {
    /// Validates and builds the configuration.
    pub fn new(
        excluded_channels: Vec<u64>,
        afk_channel: Option<u64>,
        count_self_deafened: bool,
    ) -> Result<Self, ConfigError> {
        let mut channels: Vec<u64> = Vec::with_capacity(excluded_channels.len());
        for id in excluded_channels {
            if id == 0 {
                return Err(ConfigError::new(
                    ConfigErrorKind::InvalidChannelId,
                    KEY_EXCLUDED_CHANNEL_IDS,
                ));
            }
            if !channels.contains(&id) {
                channels.push(id);
            }
        }
        if let Some(afk) = afk_channel {
            if afk == 0 {
                return Err(ConfigError::new(
                    ConfigErrorKind::InvalidChannelId,
                    "ACTIVITY_AFK_CHANNEL_ID",
                ));
            }
        }
        Ok(Self {
            excluded_channels: channels,
            afk_channel,
            count_self_deafened,
        })
    }

    pub fn excluded_channels(&self) -> &[u64] {
        &self.excluded_channels
    }

    pub fn afk_channel(&self) -> Option<u64> {
        self.afk_channel
    }

    pub fn count_self_deafened(&self) -> bool {
        self.count_self_deafened
    }

    pub fn is_excluded(&self, channel_id: u64) -> bool {
        self.excluded_channels.contains(&channel_id)
    }

    pub fn is_afk(&self, channel_id: u64) -> bool {
        self.afk_channel == Some(channel_id)
    }
}

/// Stable reason codes for a refused configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    InvalidChannelId,
}

/// A refused configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    key: &'static str,
}

impl ConfigError {
    fn new(kind: ConfigErrorKind, key: &'static str) -> Self {
        Self { kind, key }
    }

    pub fn kind(&self) -> ConfigErrorKind {
        self.kind
    }

    pub fn key(&self) -> &'static str {
        self.key
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self.kind {
            ConfigErrorKind::InvalidChannelId => "value must not contain the id zero",
        };
        let key = self.key;
        write!(f, "{key}: {msg}")
    }
}

impl std::error::Error for ConfigError {}

/// One message counted for one user in one channel on one UTC day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Increment {
    pub utc_day: String,
    pub user_id: u64,
    pub channel_id: u64,
    pub messages: u64,
}

/// One credited stretch of voice presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSession {
    pub user_id: u64,
    pub channel_id: u64,
    pub start: i64,
    pub end: i64,
    pub seconds: u64,
}

/// Stable reason codes for an event that could not be accounted normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnomalyKind {
    MissingOldState,
    DuplicateUpdate,
    OutOfOrderUpdate,
    ChannelMismatch,
    RestartGap,
    UnaccountedChannel,
}

/// One anomaly record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anomaly {
    pub kind: AnomalyKind,
    pub guild_id: u64,
    pub user_id: u64,
    pub channel_id: Option<u64>,
    pub at: i64,
}

impl Anomaly {
    pub fn code(&self) -> &'static str {
        match self.kind {
            AnomalyKind::MissingOldState => "missing_old_state",
            AnomalyKind::DuplicateUpdate => "duplicate_update",
            AnomalyKind::OutOfOrderUpdate => "out_of_order_update",
            AnomalyKind::ChannelMismatch => "channel_mismatch",
            AnomalyKind::RestartGap => "restart_gap",
            AnomalyKind::UnaccountedChannel => "unaccounted_channel",
        }
    }
}

/// The UTC day of a timestamp, as `YYYY-MM-DD`. SCAFFOLD: returns a fixed string.
pub fn utc_day(_at: i64) -> String {
    "0000-00-00".to_string()
}

/// The start of the UTC day that contains the timestamp. SCAFFOLD: returns zero.
pub fn day_start(_at: i64) -> i64 {
    0
}

/// The start of the next UTC day. SCAFFOLD: returns zero.
pub fn next_day_start(_at: i64) -> i64 {
    0
}

/// What the policy does with one message event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageOutcome {
    Counted(Increment),
    Ignored(IgnoreReason),
}

/// Stable reason codes for a message that is not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    BotAuthor,
    ExcludedChannel,
    InvalidUser,
    InvalidChannel,
    InvalidGuild,
}

/// Accounts one message event. SCAFFOLD: counts everything.
pub fn account_message(event: &MessageEvent, _cfg: &ActivityConfig) -> MessageOutcome {
    MessageOutcome::Counted(Increment {
        utc_day: utc_day(event.at),
        user_id: event.user_id,
        channel_id: event.channel_id,
        messages: 1,
    })
}

/// The open voice presence of one member.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenSession {
    channel_id: u64,
    started_at: i64,
    last_seen_at: i64,
    self_deaf: bool,
}

/// The state the voice accounting keeps between events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    open: BTreeMap<(u64, u64), OpenSession>,
    last_seen: BTreeMap<(u64, u64), i64>,
}

/// The result of applying one voice event to a [`Ledger`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VoiceOutcome {
    pub sessions: Vec<VoiceSession>,
    pub anomalies: Vec<Anomaly>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_idle(&self, guild_id: u64, user_id: u64) -> bool {
        !self.open.contains_key(&(guild_id, user_id))
    }

    pub fn open_session_count(&self) -> usize {
        self.open.len()
    }

    /// Accounts one voice state update. SCAFFOLD: does nothing.
    pub fn apply_voice(&mut self, _event: &VoiceEvent, _cfg: &ActivityConfig) -> VoiceOutcome {
        VoiceOutcome::default()
    }

    /// Records a restart of the bot. SCAFFOLD: does nothing.
    pub fn note_restart(
        &mut self,
        _now_at: i64,
        _last_heartbeat_at: i64,
        _cfg: &ActivityConfig,
    ) -> VoiceOutcome {
        VoiceOutcome::default()
    }

    fn is_accountable(&self, _channel_id: u64, _cfg: &ActivityConfig) -> bool {
        true
    }
}

/// Closes an open session. SCAFFOLD: does nothing.
#[allow(clippy::too_many_arguments)]
fn close_session(
    _session: &OpenSession,
    _end_at: i64,
    _cfg: &ActivityConfig,
    _sessions: &mut Vec<VoiceSession>,
    _anomalies: &mut Vec<Anomaly>,
    _guild_id: u64,
    _user_id: u64,
) {
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: u64 = 900_000_000_000_000_001;
    const CHANNEL: u64 = 900_000_000_000_000_002;
    const OTHER_CHANNEL: u64 = 900_000_000_000_000_003;
    const AFK: u64 = 900_000_000_000_000_004;
    const EXCLUDED: u64 = 900_000_000_000_000_005;
    const USER: u64 = 900_000_000_000_000_010;
    const OTHER_USER: u64 = 900_000_000_000_000_011;

    const T: i64 = 1_709_640_000;

    fn cfg() -> ActivityConfig {
        ActivityConfig::new(vec![EXCLUDED], Some(AFK), false).expect("valid config")
    }

    fn plain_cfg() -> ActivityConfig {
        ActivityConfig::new(Vec::new(), None, false).expect("valid config")
    }

    fn message(at: i64) -> MessageEvent {
        MessageEvent {
            guild_id: GUILD,
            channel_id: CHANNEL,
            user_id: USER,
            is_bot: false,
            at,
        }
    }

    fn join(at: i64, channel: u64) -> VoiceEvent {
        VoiceEvent {
            guild_id: GUILD,
            user_id: USER,
            old_channel: None,
            new_channel: Some(channel),
            self_deaf: false,
            at,
        }
    }

    fn leave(at: i64, channel: u64) -> VoiceEvent {
        VoiceEvent {
            guild_id: GUILD,
            user_id: USER,
            old_channel: Some(channel),
            new_channel: None,
            self_deaf: false,
            at,
        }
    }

    fn move_to(at: i64, from: u64, to: u64) -> VoiceEvent {
        VoiceEvent {
            guild_id: GUILD,
            user_id: USER,
            old_channel: Some(from),
            new_channel: Some(to),
            self_deaf: false,
            at,
        }
    }

    // ---- message accounting -------------------------------------------------

    #[test]
    fn message_counts_one_for_day_user_channel() {
        let outcome = account_message(&message(T), &cfg());
        let MessageOutcome::Counted(increment) = outcome else {
            panic!("expected the message to count");
        };
        assert_eq!(increment.utc_day, "2024-03-05");
        assert_eq!(increment.user_id, USER);
        assert_eq!(increment.channel_id, CHANNEL);
        assert_eq!(increment.messages, 1);
    }

    #[test]
    fn message_uses_the_utc_day_of_the_timestamp() {
        let just_before = account_message(&message(1_709_640_000 - 1), &cfg());
        let just_after = account_message(&message(1_709_640_000), &cfg());
        let MessageOutcome::Counted(before) = just_before else {
            panic!("expected a count");
        };
        let MessageOutcome::Counted(after) = just_after else {
            panic!("expected a count");
        };
        assert_eq!(before.utc_day, "2024-03-05");
        assert_eq!(after.utc_day, "2024-03-05");
    }

    #[test]
    fn message_in_a_later_day_gets_a_later_day_key() {
        let next_day = account_message(&message(T + 86_400), &cfg());
        let MessageOutcome::Counted(increment) = next_day else {
            panic!("expected a count");
        };
        assert_eq!(increment.utc_day, "2024-03-06");
    }

    #[test]
    fn bot_message_is_ignored() {
        let mut event = message(T);
        event.is_bot = true;
        assert_eq!(
            account_message(&event, &cfg()),
            MessageOutcome::Ignored(IgnoreReason::BotAuthor)
        );
    }

    #[test]
    fn message_in_an_excluded_channel_is_ignored() {
        let mut event = message(T);
        event.channel_id = EXCLUDED;
        assert_eq!(
            account_message(&event, &cfg()),
            MessageOutcome::Ignored(IgnoreReason::ExcludedChannel)
        );
    }

    #[test]
    fn message_with_zero_ids_is_ignored() {
        let mut event = message(T);
        event.user_id = 0;
        assert_eq!(
            account_message(&event, &cfg()),
            MessageOutcome::Ignored(IgnoreReason::InvalidUser)
        );
        let mut event = message(T);
        event.channel_id = 0;
        assert_eq!(
            account_message(&event, &cfg()),
            MessageOutcome::Ignored(IgnoreReason::InvalidChannel)
        );
        let mut event = message(T);
        event.guild_id = 0;
        assert_eq!(
            account_message(&event, &cfg()),
            MessageOutcome::Ignored(IgnoreReason::InvalidGuild)
        );
    }

    #[test]
    fn message_in_the_afk_channel_is_still_counted() {
        let mut event = message(T);
        event.channel_id = AFK;
        let MessageOutcome::Counted(increment) = account_message(&event, &cfg()) else {
            panic!("expected a count");
        };
        assert_eq!(increment.channel_id, AFK);
    }

    // ---- voice accounting ---------------------------------------------------

    #[test]
    fn join_then_leave_credits_the_stretch() {
        let mut ledger = Ledger::new();
        let joined = ledger.apply_voice(&join(T, CHANNEL), &cfg());
        assert!(joined.sessions.is_empty());
        assert!(joined.anomalies.is_empty());
        let left = ledger.apply_voice(&leave(T + 600, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].user_id, USER);
        assert_eq!(left.sessions[0].channel_id, CHANNEL);
        assert_eq!(left.sessions[0].start, T);
        assert_eq!(left.sessions[0].end, T + 600);
        assert_eq!(left.sessions[0].seconds, 600);
    }

    #[test]
    fn move_closes_the_old_channel_and_opens_the_new_one() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let moved = ledger.apply_voice(&move_to(T + 300, CHANNEL, OTHER_CHANNEL), &cfg());
        assert_eq!(moved.sessions.len(), 1);
        assert_eq!(moved.sessions[0].channel_id, CHANNEL);
        assert_eq!(moved.sessions[0].seconds, 300);
        let left = ledger.apply_voice(&leave(T + 900, OTHER_CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].channel_id, OTHER_CHANNEL);
        assert_eq!(left.sessions[0].start, T + 300);
        assert_eq!(left.sessions[0].seconds, 600);
    }

    #[test]
    fn presence_in_the_afk_channel_is_never_credited() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, AFK), &cfg());
        let left = ledger.apply_voice(&leave(T + 600, AFK), &cfg());
        assert!(left.sessions.is_empty());
        assert_eq!(left.anomalies.len(), 1);
        assert_eq!(left.anomalies[0].kind, AnomalyKind::UnaccountedChannel);
    }

    #[test]
    fn presence_in_an_excluded_channel_is_never_credited() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, EXCLUDED), &cfg());
        let left = ledger.apply_voice(&leave(T + 600, EXCLUDED), &cfg());
        assert!(left.sessions.is_empty());
    }

    #[test]
    fn self_deafened_time_is_not_counted_by_default() {
        let mut ledger = Ledger::new();
        let mut event = join(T, CHANNEL);
        event.self_deaf = true;
        ledger.apply_voice(&event, &cfg());
        let left = ledger.apply_voice(&leave(T + 600, CHANNEL), &cfg());
        assert!(left.sessions.is_empty());
    }

    #[test]
    fn self_deafened_time_is_counted_when_the_flag_is_set() {
        let counting = ActivityConfig::new(Vec::new(), None, true).expect("valid config");
        let mut ledger = Ledger::new();
        let mut event = join(T, CHANNEL);
        event.self_deaf = true;
        ledger.apply_voice(&event, &counting);
        let left = ledger.apply_voice(&leave(T + 600, CHANNEL), &counting);
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].seconds, 600);
    }

    #[test]
    fn a_session_across_utc_midnight_is_split_exactly() {
        let midnight = day_start(T) + 86_400;
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(midnight - 300, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(midnight + 300, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 2);
        assert_eq!(left.sessions[0].start, midnight - 300);
        assert_eq!(left.sessions[0].end, midnight);
        assert_eq!(left.sessions[0].seconds, 300);
        assert_eq!(left.sessions[1].start, midnight);
        assert_eq!(left.sessions[1].end, midnight + 300);
        assert_eq!(left.sessions[1].seconds, 300);
        assert_eq!(utc_day(left.sessions[0].start), utc_day(left.sessions[0].end - 1));
        assert_ne!(utc_day(left.sessions[0].start), utc_day(left.sessions[1].start));
    }

    #[test]
    fn a_duplicate_join_does_not_credit_twice() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let again = ledger.apply_voice(&join(T, CHANNEL), &cfg());
        assert_eq!(again.anomalies.len(), 1);
        assert_eq!(again.anomalies[0].kind, AnomalyKind::DuplicateUpdate);
        let left = ledger.apply_voice(&leave(T + 600, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].seconds, 600);
    }

    #[test]
    fn an_out_of_order_update_is_flagged_and_credits_nothing() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let stale = ledger.apply_voice(&leave(T - 600, CHANNEL), &cfg());
        assert_eq!(stale.anomalies.len(), 1);
        assert_eq!(stale.anomalies[0].kind, AnomalyKind::OutOfOrderUpdate);
        assert!(stale.sessions.is_empty());
        let left = ledger.apply_voice(&leave(T + 600, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].seconds, 600);
    }

    #[test]
    fn a_first_update_that_is_a_move_opens_a_session_and_flags_the_gap() {
        let mut ledger = Ledger::new();
        let moved = ledger.apply_voice(&move_to(T, CHANNEL, OTHER_CHANNEL), &cfg());
        assert_eq!(moved.anomalies.len(), 1);
        assert_eq!(moved.anomalies[0].kind, AnomalyKind::MissingOldState);
        assert!(moved.sessions.is_empty());
        let left = ledger.apply_voice(&leave(T + 600, OTHER_CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].channel_id, OTHER_CHANNEL);
        assert_eq!(left.sessions[0].seconds, 600);
    }

    #[test]
    fn a_leave_without_a_known_session_is_flagged() {
        let mut ledger = Ledger::new();
        let left = ledger.apply_voice(&leave(T, CHANNEL), &cfg());
        assert_eq!(left.anomalies.len(), 1);
        assert_eq!(left.anomalies[0].kind, AnomalyKind::MissingOldState);
        assert!(left.sessions.is_empty());
    }

    #[test]
    fn a_restart_closes_at_the_last_heartbeat_and_flags_the_gap() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let restart = ledger.note_restart(T + 86_400, T + 60, &cfg());
        assert_eq!(restart.sessions.len(), 1);
        assert_eq!(restart.sessions[0].end, T + 60);
        assert_eq!(restart.sessions[0].seconds, 60);
        assert_eq!(restart.anomalies.len(), 1);
        assert_eq!(restart.anomalies[0].kind, AnomalyKind::RestartGap);
        assert_eq!(restart.anomalies[0].at, T + 60);
        assert!(ledger.is_idle(GUILD, USER));
    }

    #[test]
    fn a_restart_credits_nothing_beyond_the_heartbeat() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let restart = ledger.note_restart(T + 86_400, T + 60, &cfg());
        let credited: u64 = restart.sessions.iter().map(|s| s.seconds).sum();
        assert_eq!(credited, 60);
    }

    #[test]
    fn a_channel_mismatch_is_flagged_and_the_known_channel_is_closed() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(T + 600, OTHER_CHANNEL), &cfg());
        assert_eq!(left.anomalies.len(), 1);
        assert_eq!(left.anomalies[0].kind, AnomalyKind::ChannelMismatch);
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].channel_id, CHANNEL);
    }

    #[test]
    fn a_huge_timestamp_does_not_panic() {
        let mut ledger = Ledger::new();
        let huge = i64::MAX / 4;
        ledger.apply_voice(&join(huge, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(huge + 10, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].seconds, 10);
        let day = utc_day(huge);
        assert_eq!(day.len(), 10);
    }

    #[test]
    fn a_negative_timestamp_does_not_panic() {
        let mut ledger = Ledger::new();
        let negative = -100_000;
        ledger.apply_voice(&join(negative, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(negative + 10, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].seconds, 10);
        let day = utc_day(negative);
        assert_eq!(day.len(), 10);
    }

    #[test]
    fn the_day_of_a_known_epoch_is_correct() {
        assert_eq!(utc_day(0), "1970-01-01");
        assert_eq!(utc_day(86_399), "1970-01-01");
        assert_eq!(utc_day(86_400), "1970-01-02");
        assert_eq!(utc_day(951_782_400), "2000-02-29");
    }

    #[test]
    fn a_leap_day_is_handled() {
        assert_eq!(utc_day(1_709_164_800), "2024-02-29");
        assert_eq!(utc_day(1_709_251_200), "2024-03-01");
    }

    #[test]
    fn the_counter_saturates_and_never_overflows() {
        let mut total: u64 = 0;
        for _ in 0..10_000 {
            let MessageOutcome::Counted(increment) = account_message(&message(T), &cfg()) else {
                panic!("expected a count");
            };
            total = total.saturating_add(increment.messages);
        }
        assert_eq!(total, 10_000);
    }

    #[test]
    fn the_config_refuses_the_id_zero() {
        assert_eq!(
            ActivityConfig::new(vec![0], None, false).unwrap_err().kind(),
            ConfigErrorKind::InvalidChannelId
        );
        assert_eq!(
            ActivityConfig::new(Vec::new(), Some(0), false).unwrap_err().kind(),
            ConfigErrorKind::InvalidChannelId
        );
    }

    #[test]
    fn the_config_removes_duplicate_excluded_channels() {
        let config = ActivityConfig::new(vec![EXCLUDED, EXCLUDED, OTHER_CHANNEL], None, false)
            .expect("valid config");
        assert_eq!(config.excluded_channels(), &[EXCLUDED, OTHER_CHANNEL]);
    }

    #[test]
    fn the_anomaly_codes_are_stable() {
        assert_eq!(
            Anomaly {
                kind: AnomalyKind::MissingOldState,
                guild_id: GUILD,
                user_id: USER,
                channel_id: None,
                at: 0,
            }
            .code(),
            "missing_old_state"
        );
        assert_eq!(
            Anomaly {
                kind: AnomalyKind::RestartGap,
                guild_id: GUILD,
                user_id: USER,
                channel_id: None,
                at: 0,
            }
            .code(),
            "restart_gap"
        );
    }

    #[test]
    fn the_config_error_names_the_key_and_not_the_value() {
        let error = ActivityConfig::new(vec![0], None, false).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("ACTIVITY_EXCLUDED_CHANNEL_IDS"));
        assert!(!text.contains('0'));
    }

    // ---- invariant sweep ----------------------------------------------------

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 11
        }
    }

    #[test]
    fn a_seeded_event_sweep_never_credits_twice_or_overlaps() {
        let config = plain_cfg();
        let mut ledger = Ledger::new();
        let mut rng = Lcg(0x5EED_1234_ABCD_0001);
        let mut clock = 1_700_000_000i64;
        let mut all_sessions: Vec<VoiceSession> = Vec::new();
        let channels = [CHANNEL, OTHER_CHANNEL];
        let mut in_channel: Option<u64> = None;

        for _ in 0..5_000 {
            clock += (rng.next() % 120) as i64 + 1;
            let roll = rng.next() % 10;
            let event = match (roll, in_channel) {
                (0..=3, None) => {
                    in_channel = Some(channels[(rng.next() % 2) as usize]);
                    VoiceEvent {
                        guild_id: GUILD,
                        user_id: USER,
                        old_channel: None,
                        new_channel: in_channel,
                        self_deaf: rng.next() % 5 == 0,
                        at: clock,
                    }
                }
                (0..=3, Some(current)) => {
                    let next = if rng.next() % 2 == 0 {
                        channels[(rng.next() % 2) as usize]
                    } else {
                        current
                    };
                    let event = VoiceEvent {
                        guild_id: GUILD,
                        user_id: USER,
                        old_channel: Some(current),
                        new_channel: Some(next),
                        self_deaf: rng.next() % 5 == 0,
                        at: clock,
                    };
                    in_channel = Some(next);
                    event
                }
                (4..=6, Some(current)) => {
                    let event = VoiceEvent {
                        guild_id: GUILD,
                        user_id: USER,
                        old_channel: Some(current),
                        new_channel: None,
                        self_deaf: rng.next() % 5 == 0,
                        at: clock,
                    };
                    in_channel = None;
                    event
                }
                (4..=6, None) => VoiceEvent {
                    guild_id: GUILD,
                    user_id: USER,
                    old_channel: None,
                    new_channel: None,
                    self_deaf: false,
                    at: clock,
                },
                _ => {
                    let stale = if rng.next() % 2 == 0 {
                        clock - 30
                    } else {
                        clock
                    };
                    VoiceEvent {
                        guild_id: GUILD,
                        user_id: USER,
                        old_channel: in_channel,
                        new_channel: in_channel,
                        self_deaf: rng.next() % 5 == 0,
                        at: stale,
                    }
                }
            };
            let outcome = ledger.apply_voice(&event, &config);
            all_sessions.extend(outcome.sessions);
        }

        for session in &all_sessions {
            assert!(session.end >= session.start, "negative session");
            assert_eq!(
                session.seconds,
                (session.end - session.start) as u64,
                "seconds must match the span"
            );
        }

        for session in &all_sessions {
            assert_eq!(utc_day(session.start), utc_day(session.end - 1.max(0)));
        }

        let mut sorted: Vec<&VoiceSession> = all_sessions.iter().collect();
        sorted.sort_by_key(|s| (s.start, s.end));
        for pair in sorted.windows(2) {
            let (first, second) = (pair[0], pair[1]);
            assert!(
                second.start >= first.end,
                "sessions overlap: {:?} and {:?}",
                first,
                second
            );
        }
    }

    #[test]
    fn a_seeded_sweep_of_messages_never_leaks_content_or_overflows() {
        let config = plain_cfg();
        let mut rng = Lcg(0x5EED_1234_ABCD_0002);
        let mut counts: BTreeMap<(String, u64, u64), u64> = BTreeMap::new();
        for _ in 0..20_000 {
            let at = (rng.next() % 5_000_000) as i64;
            let event = MessageEvent {
                guild_id: GUILD,
                channel_id: if rng.next() % 4 == 0 { EXCLUDED } else { CHANNEL },
                user_id: if rng.next() % 8 == 0 { OTHER_USER } else { USER },
                is_bot: rng.next() % 6 == 0,
                at,
            };
            if let MessageOutcome::Counted(increment) = account_message(&event, &config) {
                let key = (
                    increment.utc_day.clone(),
                    increment.user_id,
                    increment.channel_id,
                );
                let slot = counts.entry(key).or_insert(0);
                *slot = slot.saturating_add(increment.messages);
            }
        }
        assert!(counts.keys().all(|(_, _, channel)| *channel != EXCLUDED));
        assert!(counts.keys().all(|(day, _, _)| day.len() == 10));
    }
}

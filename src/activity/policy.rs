//! Pure accounting policy for the activity feature (levels and statistics).
//!
//! This module is the Discord-free core of the activity accounting that replaces the old
//! Lurkr and Statbot bots. It answers two questions from plain facts:
//!
//! * how many messages did a user send in a channel on a given UTC day, and
//! * how long was a user present in a voice channel on each UTC day.
//!
//! Everything here is a pure function of the facts and the configuration. There is no
//! serenity type, no I/O, no clock read and no randomness: every timestamp and every id is
//! passed in by the caller. The voice side keeps its own state in [`Ledger`], so the
//! caller owns the state and the policy stays deterministic.
//!
//! Privacy notes:
//! * Message CONTENT is never an input. [`MessageEvent`] has no content field, so
//!   content can neither be read nor stored.
//! * Nothing here reads an environment variable or a file. The configuration keys are
//!   listed as constants so the wiring card can read them.
//! * Anomalies are stable enum codes. They never carry message text, user names or ids.
//!
//! Wiring card W1 removes the module-level `allow(dead_code)` in `super::mod`.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

/// Environment key holding the operating mode of the activity feature.
///
/// Default: `off`. Read by the wiring card, not by this module.
pub const KEY_MODE: &str = "ACTIVITY_MODE";
/// Environment key holding the comma separated list of excluded channel ids.
///
/// Default: empty (nothing excluded). Read by the wiring card, not by this module.
pub const KEY_EXCLUDED_CHANNEL_IDS: &str = "ACTIVITY_EXCLUDED_CHANNEL_IDS";
/// Environment key holding the "time spent self-deafened counts" flag.
///
/// Default: `false` (self-deafened time is not credited). Read by the wiring card, not by
/// this module.
pub const KEY_COUNT_SELF_DEAFENED: &str = "ACTIVITY_COUNT_SELF_DEAFENED";

/// Seconds in one day. Used to walk UTC day boundaries.
pub const SECONDS_PER_DAY: i64 = 86_400;

/// The largest number of voice records one closed session may emit.
///
/// A session that crosses more UTC days than this is not credited at all and is
/// flagged, because the split would otherwise emit one record per day and a
/// corrupt timestamp could ask for an unbounded number of them. The value is far
/// beyond any real voice presence: it is a year of days.
pub const MAX_SESSION_RECORDS: usize = 366;

/// The plain facts about one incoming message.
///
/// The content of the message is deliberately absent: the policy counts messages and
/// never looks at what was written, so no text can reach a counter, an anomaly or a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageEvent {
    /// Guild the message was sent in.
    pub guild_id: u64,
    /// Channel the message was sent in.
    pub channel_id: u64,
    /// Id of the author.
    pub user_id: u64,
    /// True when the author is a bot account.
    pub is_bot: bool,
    /// Time of the message, in seconds since the Unix epoch.
    pub at: i64,
}

/// The plain facts about one voice state update.
///
/// A Discord voice state update names the channel the member was in before the update and
/// the channel the member is in after it. `None` on both sides is not a state Discord
/// sends; the policy treats it as "no channel" on both sides and accounts nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceEvent {
    /// Guild the update came from.
    pub guild_id: u64,
    /// Id of the member whose voice state changed.
    pub user_id: u64,
    /// Channel the member was in before the update, if any.
    pub old_channel: Option<u64>,
    /// Channel the member is in after the update, if any.
    pub new_channel: Option<u64>,
    /// True when the member is self-deafened at the time of the update.
    pub self_deaf: bool,
    /// Time of the update, in seconds since the Unix epoch.
    pub at: i64,
}

/// Validated configuration of the activity accounting.
///
/// Build it with [`ActivityConfig::new`]; the fields are private so an invalid
/// configuration cannot be constructed by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityConfig {
    excluded_channels: Vec<u64>,
    afk_channel: Option<u64>,
    count_self_deafened: bool,
}

impl ActivityConfig {
    /// Validates and builds the configuration.
    ///
    /// * `excluded_channels` may be empty (nothing excluded). Duplicates are removed,
    ///   keeping the first occurrence. The id zero is refused.
    /// * `afk_channel` may be absent (no channel is the AFK channel). Zero is refused.
    /// * `count_self_deafened` decides whether time spent self-deafened is credited. The
    ///   wiring card defaults it to `false`; the flag itself carries no default here.
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
                    KEY_AFK_CHANNEL_ID,
                ));
            }
        }
        Ok(Self {
            excluded_channels: channels,
            afk_channel,
            count_self_deafened,
        })
    }

    /// The configured excluded channel ids, without duplicates.
    pub fn excluded_channels(&self) -> &[u64] {
        &self.excluded_channels
    }

    /// The configured AFK channel id, if any.
    pub fn afk_channel(&self) -> Option<u64> {
        self.afk_channel
    }

    /// Whether time spent self-deafened is credited.
    pub fn count_self_deafened(&self) -> bool {
        self.count_self_deafened
    }

    /// True when the channel is excluded from accounting.
    pub fn is_excluded(&self, channel_id: u64) -> bool {
        self.excluded_channels.contains(&channel_id)
    }

    /// True when the channel is the AFK channel.
    pub fn is_afk(&self, channel_id: u64) -> bool {
        self.afk_channel == Some(channel_id)
    }
}

/// Environment key holding the id of the AFK voice channel.
///
/// Default: unset (no channel is the AFK channel). Read by the wiring card, not by this
/// module.
pub const KEY_AFK_CHANNEL_ID: &str = "ACTIVITY_AFK_CHANNEL_ID";

/// Stable reason codes for a refused configuration.
///
/// The codes never carry the offending value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// The channel id list contained the id zero, which is not a real channel.
    InvalidChannelId,
}

/// A refused configuration.
///
/// The error names the configuration key that was wrong and never echoes the value, so it
/// is safe to log verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    key: &'static str,
}

impl ConfigError {
    fn new(kind: ConfigErrorKind, key: &'static str) -> Self {
        Self { kind, key }
    }

    /// Returns the stable reason code for this refusal.
    pub fn kind(&self) -> ConfigErrorKind {
        self.kind
    }

    /// Returns the configuration key that was refused.
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
///
/// The increment is what the persistence card sums into the daily counter row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Increment {
    /// The UTC day, as `YYYY-MM-DD`.
    pub utc_day: String,
    /// Id of the author.
    pub user_id: u64,
    /// Channel the message was sent in.
    pub channel_id: u64,
    /// Number of messages counted. Always one per increment.
    pub messages: u64,
}

/// One credited stretch of voice presence.
///
/// A session that crosses a UTC midnight is split, so every record lies inside exactly one
/// UTC day and the daily totals are right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSession {
    /// Id of the member.
    pub user_id: u64,
    /// Channel the presence was in.
    pub channel_id: u64,
    /// Start of the stretch, in seconds since the Unix epoch.
    pub start: i64,
    /// End of the stretch, in seconds since the Unix epoch. Never before `start`.
    pub end: i64,
    /// Length of the stretch in seconds. Never negative.
    pub seconds: u64,
}

/// Stable reason codes for an event that could not be accounted normally.
///
/// The codes never carry message text, user names or ids. Every anomaly is NOT credited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnomalyKind {
    /// The first update seen for this member was a move or a leave: the old state was
    /// unknown, so the presence before the update could not be accounted.
    MissingOldState,
    /// The update repeated a join for a member that already had an open session.
    DuplicateUpdate,
    /// The update carried a timestamp earlier than the last update seen for this member.
    OutOfOrderUpdate,
    /// The update named a different old channel than the one the member was last seen in.
    ChannelMismatch,
    /// The bot was restarted: an open session was closed at the last heartbeat and the
    /// gap after it is not credited.
    RestartGap,
    /// The member was present in a channel that is not accounted (excluded or AFK).
    UnaccountedChannel,
}

/// One anomaly record.
///
/// An anomaly is a stable code plus the facts needed to find the member and the time. It
/// never carries message text or a user name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anomaly {
    /// The stable reason code.
    pub kind: AnomalyKind,
    /// Guild the anomaly belongs to.
    pub guild_id: u64,
    /// Id of the member the anomaly belongs to.
    pub user_id: u64,
    /// Channel the anomaly belongs to, when one is known.
    pub channel_id: Option<u64>,
    /// Time of the anomaly, in seconds since the Unix epoch.
    pub at: i64,
}

impl Anomaly {
    /// Stable machine code; never contains names, ids or text.
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

/// The UTC day of a timestamp, as `YYYY-MM-DD`.
///
/// A timestamp before the Unix epoch is floored into the day that contains it, so the
/// result is always the day the instant falls in. A timestamp that falls outside the
/// range `YYYY-MM-DD` can hold is clamped to `9999-12-31`, so the key always has the
/// same shape and the day arithmetic never panics, whatever the timestamp.
pub fn utc_day(at: i64) -> String {
    let (year, month, day) = civil_from_days(at.div_euclid(SECONDS_PER_DAY));
    if year > 9999 {
        return "9999-12-31".to_string();
    }
    if year < 0 {
        return "0000-01-01".to_string();
    }
    format!("{year:04}-{month:02}-{day:02}")
}

/// The start of the UTC day that contains the timestamp, in seconds since the epoch.
///
/// The floor of the day is not representable for a timestamp at the very bottom of
/// the `i64` range, so it is clamped there instead of overflowing.
pub fn day_start(at: i64) -> i64 {
    at.div_euclid(SECONDS_PER_DAY)
        .saturating_mul(SECONDS_PER_DAY)
}

/// The start of the next UTC day after the one that contains the timestamp.
///
/// The start of the next day is not representable for a timestamp in the very last
/// day of the `i64` range, so it is clamped to the end of the range instead of
/// overflowing. Nothing in the representable range lies after that day, so the
/// clamped value splits no session in two.
pub fn next_day_start(at: i64) -> i64 {
    day_start(at).saturating_add(SECONDS_PER_DAY)
}

/// The number of UTC days the stretch `start..end` touches.
///
/// This is the number of records the day split emits for that stretch, computed
/// with arithmetic instead of a walk. The stretch is half open: the second at
/// `end` belongs to the next stretch. An empty stretch touches no day.
fn days_touched(start: i64, end: i64) -> u64 {
    if end <= start {
        return 0;
    }
    // The last second of the stretch is `end - 1`. The difference of the two day
    // indexes is the number of day boundaries between them, so the count is that
    // plus the first day itself.
    let last = end - 1;
    let first_day = start.div_euclid(SECONDS_PER_DAY);
    let last_day = last.div_euclid(SECONDS_PER_DAY);
    (last_day - first_day).saturating_add(1) as u64
}

/// Converts a count of days since the Unix epoch into a civil (year, month, day).
///
/// This is Howard Hinnant's `civil_from_days` algorithm, which is exact for the whole
/// range of `i64` days a timestamp can produce.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// What the policy does with one message event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageOutcome {
    /// The message counted towards the given key.
    Counted(Increment),
    /// The message was not counted. The reason says why.
    Ignored(IgnoreReason),
}

/// Stable reason codes for a message that is not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// The author is a bot account.
    BotAuthor,
    /// The channel is excluded from accounting.
    ExcludedChannel,
    /// The author id was zero, which is not a real user.
    InvalidUser,
    /// The channel id was zero, which is not a real channel.
    InvalidChannel,
    /// The guild id was zero, which is not a real guild.
    InvalidGuild,
}

/// Accounts one message event.
///
/// The message content is not an input of this function: the event type has no content
/// field, so the policy can neither read nor store it.
///
/// A counted message produces one increment for the key (UTC day, user, channel). The
/// caller sums the increments for the same key.
pub fn account_message(event: &MessageEvent, cfg: &ActivityConfig) -> MessageOutcome {
    if event.guild_id == 0 {
        return MessageOutcome::Ignored(IgnoreReason::InvalidGuild);
    }
    if event.user_id == 0 {
        return MessageOutcome::Ignored(IgnoreReason::InvalidUser);
    }
    if event.channel_id == 0 {
        return MessageOutcome::Ignored(IgnoreReason::InvalidChannel);
    }
    if event.is_bot {
        return MessageOutcome::Ignored(IgnoreReason::BotAuthor);
    }
    if cfg.is_excluded(event.channel_id) {
        return MessageOutcome::Ignored(IgnoreReason::ExcludedChannel);
    }
    MessageOutcome::Counted(Increment {
        utc_day: utc_day(event.at),
        user_id: event.user_id,
        channel_id: event.channel_id,
        messages: 1,
    })
}

/// One stretch of presence in a voice channel, with the flag that was current for it.
///
/// The open session is a list of these, because the self-deaf flag can change while the
/// member stays in the same channel.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    start: i64,
    end: i64,
    self_deaf: bool,
}

/// The open voice presence of one member.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenSession {
    channel_id: u64,
    segments: Vec<Segment>,
}

/// The state the voice accounting keeps between events.
///
/// The caller owns this value. A fresh [`Ledger`] has no open sessions. After a restart
/// the caller calls [`Ledger::note_restart`] with the last heartbeat it observed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    open: BTreeMap<(u64, u64), OpenSession>,
    last_seen: BTreeMap<(u64, u64), i64>,
}

/// The result of applying one voice event to a [`Ledger`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VoiceOutcome {
    /// The credited voice sessions, already split at UTC midnight.
    pub sessions: Vec<VoiceSession>,
    /// The anomalies the event produced. They are never credited.
    pub anomalies: Vec<Anomaly>,
}

impl Ledger {
    /// A ledger with no open sessions.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the ledger holds no open session for the member.
    pub fn is_idle(&self, guild_id: u64, user_id: u64) -> bool {
        !self.open.contains_key(&(guild_id, user_id))
    }

    /// The number of open sessions the ledger holds.
    pub fn open_session_count(&self) -> usize {
        self.open.len()
    }

    /// The channel the member's open session is in, if any.
    ///
    /// The persistence card uses this to know where a member was last seen.
    pub fn open_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
        self.open
            .get(&(guild_id, user_id))
            .map(|session| session.channel_id)
    }

    /// The time of the last update the ledger accepted for the member, if any.
    ///
    /// This is the value the persistence card keeps as the heartbeat, so a restart can
    /// close the session at the last moment the bot was known to be running.
    pub fn last_seen_at(&self, guild_id: u64, user_id: u64) -> Option<i64> {
        self.last_seen.get(&(guild_id, user_id)).copied()
    }

    /// Accounts one voice state update.
    ///
    /// The outcome carries the credited sessions and the anomalies. An anomaly is never
    /// credited.
    pub fn apply_voice(&mut self, event: &VoiceEvent, cfg: &ActivityConfig) -> VoiceOutcome {
        let mut outcome = VoiceOutcome::default();
        let key = (event.guild_id, event.user_id);

        // An update that names the id zero is refused before anything else, the
        // same way the message path refuses it. The id zero is not a real guild,
        // member or channel, so a presence recorded against it would be a
        // presence nobody had. The whole event is refused: no session is opened,
        // no session is closed and the ledger is left exactly as it was.
        let zero_channel = event.old_channel == Some(0) || event.new_channel == Some(0);
        if event.guild_id == 0 || event.user_id == 0 || zero_channel {
            outcome.anomalies.push(Anomaly {
                kind: AnomalyKind::UnaccountedChannel,
                guild_id: event.guild_id,
                user_id: event.user_id,
                channel_id: event.old_channel.or(event.new_channel),
                at: event.at,
            });
            return outcome;
        }

        // An update that is older than the last one seen for this member is refused
        // before anything else: it must not reopen or re-close a session.
        if let Some(seen) = self.last_seen.get(&key).copied() {
            if event.at < seen {
                outcome.anomalies.push(Anomaly {
                    kind: AnomalyKind::OutOfOrderUpdate,
                    guild_id: event.guild_id,
                    user_id: event.user_id,
                    channel_id: event.new_channel.or(event.old_channel),
                    at: event.at,
                });
                return outcome;
            }
        }

        match (event.old_channel, event.new_channel) {
            (None, None) => {
                // No channel on either side: there is nothing to account.
            }
            (None, Some(channel)) => {
                // A join.
                match self.open.entry(key) {
                    Entry::Occupied(_) => {
                        outcome.anomalies.push(Anomaly {
                            kind: AnomalyKind::DuplicateUpdate,
                            guild_id: event.guild_id,
                            user_id: event.user_id,
                            channel_id: Some(channel),
                            at: event.at,
                        });
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(OpenSession {
                            channel_id: channel,
                            segments: vec![Segment {
                                start: event.at,
                                end: event.at,
                                self_deaf: event.self_deaf,
                            }],
                        });
                    }
                }
            }
            (Some(old_channel), None) => {
                // A leave.
                match self.open.remove(&key) {
                    None => {
                        outcome.anomalies.push(Anomaly {
                            kind: AnomalyKind::MissingOldState,
                            guild_id: event.guild_id,
                            user_id: event.user_id,
                            channel_id: Some(old_channel),
                            at: event.at,
                        });
                    }
                    Some(session) => {
                        if session.channel_id != old_channel {
                            outcome.anomalies.push(Anomaly {
                                kind: AnomalyKind::ChannelMismatch,
                                guild_id: event.guild_id,
                                user_id: event.user_id,
                                channel_id: Some(old_channel),
                                at: event.at,
                            });
                        }
                        self.close(
                            session,
                            event.guild_id,
                            event.user_id,
                            event.at,
                            cfg,
                            &mut outcome,
                        );
                    }
                }
            }
            (Some(old_channel), Some(new_channel)) if old_channel == new_channel => {
                // A state update inside the same channel: only the self-deaf flag can
                // have changed.
                match self.open.get_mut(&key) {
                    None => {
                        outcome.anomalies.push(Anomaly {
                            kind: AnomalyKind::MissingOldState,
                            guild_id: event.guild_id,
                            user_id: event.user_id,
                            channel_id: Some(new_channel),
                            at: event.at,
                        });
                        self.open.insert(
                            key,
                            OpenSession {
                                channel_id: new_channel,
                                segments: vec![Segment {
                                    start: event.at,
                                    end: event.at,
                                    self_deaf: event.self_deaf,
                                }],
                            },
                        );
                    }
                    Some(session) => {
                        if session.channel_id != old_channel {
                            outcome.anomalies.push(Anomaly {
                                kind: AnomalyKind::ChannelMismatch,
                                guild_id: event.guild_id,
                                user_id: event.user_id,
                                channel_id: Some(old_channel),
                                at: event.at,
                            });
                        }
                        // The stretch that ran up to this update is closed with the flag
                        // that was in force for it. A new stretch starts here with the
                        // flag of this update, so the two stretches never overlap.
                        if let Some(last) = session.segments.last_mut() {
                            last.end = event.at.max(last.start);
                        }
                        session.segments.push(Segment {
                            start: event.at,
                            end: event.at,
                            self_deaf: event.self_deaf,
                        });
                    }
                }
            }
            (Some(old_channel), Some(new_channel)) => {
                // A move.
                match self.open.remove(&key) {
                    None => {
                        // The first update seen for this member is a move: the presence in
                        // the old channel could not be accounted.
                        outcome.anomalies.push(Anomaly {
                            kind: AnomalyKind::MissingOldState,
                            guild_id: event.guild_id,
                            user_id: event.user_id,
                            channel_id: Some(old_channel),
                            at: event.at,
                        });
                    }
                    Some(session) => {
                        if session.channel_id != old_channel {
                            outcome.anomalies.push(Anomaly {
                                kind: AnomalyKind::ChannelMismatch,
                                guild_id: event.guild_id,
                                user_id: event.user_id,
                                channel_id: Some(old_channel),
                                at: event.at,
                            });
                        }
                        self.close(
                            session,
                            event.guild_id,
                            event.user_id,
                            event.at,
                            cfg,
                            &mut outcome,
                        );
                    }
                }
                self.open.insert(
                    key,
                    OpenSession {
                        channel_id: new_channel,
                        segments: vec![Segment {
                            start: event.at,
                            end: event.at,
                            self_deaf: event.self_deaf,
                        }],
                    },
                );
            }
        }

        self.last_seen.insert(key, event.at);
        outcome
    }

    /// Records a restart of the bot.
    ///
    /// Every open session is closed at the last moment the bot was known to be
    /// running, which is `last_heartbeat_at`, but never later than `now_at`, the
    /// instant the bot came back. A heartbeat that claims a moment AFTER the
    /// restart would credit presence that had not happened yet, so the credited
    /// span stops at `now_at` and the difference is not credited.
    ///
    /// The difference between the credited end and the heartbeat is flagged as a
    /// [`AnomalyKind::RestartGap`], because crediting the hours the bot was
    /// offline would inflate the levels.
    pub fn note_restart(
        &mut self,
        now_at: i64,
        last_heartbeat_at: i64,
        cfg: &ActivityConfig,
    ) -> VoiceOutcome {
        let mut outcome = VoiceOutcome::default();
        // The credited span never reaches past the restart instant. When the
        // heartbeat lies about the future, the earlier of the two wins.
        let credited_end = last_heartbeat_at.min(now_at);
        let keys: Vec<(u64, u64)> = self.open.keys().copied().collect();
        for (guild_id, user_id) in keys {
            let Some(session) = self.open.remove(&(guild_id, user_id)) else {
                continue;
            };
            self.close(session, guild_id, user_id, credited_end, cfg, &mut outcome);
            outcome.anomalies.push(Anomaly {
                kind: AnomalyKind::RestartGap,
                guild_id,
                user_id,
                channel_id: None,
                at: credited_end,
            });
        }
        self.last_seen.clear();
        outcome
    }

    /// Closes an open session at `end_at` and pushes the credited stretches.
    ///
    /// The session is not credited when its channel is excluded or the AFK channel.
    /// The anomaly that says so is raised here, on the outcome that drops the
    /// session, not on the outcome that opened it.
    ///
    /// The session is also not credited when the split would need more than
    /// [`MAX_SESSION_RECORDS`] records. The number of records is known before the
    /// split starts, so an absurd span is refused instead of asking for an
    /// unbounded allocation.
    fn close(
        &self,
        session: OpenSession,
        guild_id: u64,
        user_id: u64,
        end_at: i64,
        cfg: &ActivityConfig,
        outcome: &mut VoiceOutcome,
    ) {
        let channel_id = session.channel_id;
        if cfg.is_excluded(channel_id) || cfg.is_afk(channel_id) {
            // The presence was not credited. The anomaly says why, and it is raised once,
            // on the outcome that drops the session.
            outcome.anomalies.push(Anomaly {
                kind: AnomalyKind::UnaccountedChannel,
                guild_id,
                user_id,
                channel_id: Some(channel_id),
                at: end_at,
            });
            return;
        }
        // The stretch that is still open runs until the session was closed. The earlier
        // stretches already carry their own end.
        let mut segments = session.segments;
        if let Some(last) = segments.last_mut() {
            last.end = end_at.max(last.start);
        }
        // How many records the split would emit. It is one record per UTC day the
        // session touches, summed over the stretches. The count is known without
        // walking the days, so a span that would need an unbounded number of
        // records is refused here and never allocated.
        let records: u64 = segments
            .iter()
            .map(|segment| days_touched(segment.start, segment.end))
            .fold(0u64, |total, count| total.saturating_add(count));
        if records > MAX_SESSION_RECORDS as u64 {
            // The presence is not credited: the span is beyond anything a real
            // presence can be, so the safe failure is to drop it and say so.
            outcome.anomalies.push(Anomaly {
                kind: AnomalyKind::UnaccountedChannel,
                guild_id,
                user_id,
                channel_id: Some(channel_id),
                at: end_at,
            });
            return;
        }
        for segment in &segments {
            let start = segment.start;
            let end = segment.end.max(start);
            let mut cursor = start;
            while cursor < end {
                // The boundary is the start of the next UTC day. It is strictly after
                // the cursor for every cursor the loop can take, because the cursor
                // is below the end of the session and the end of a session is an
                // `i64`, so the cursor is at most `i64::MAX - 1`. The guard below
                // keeps the loop advancing even if that ever stops being true.
                //
                // The cursor is below `end`, and `end` is an `i64`, so the step of one
                // second cannot overflow here either.
                let boundary = next_day_start(cursor);
                let stop = end.min(boundary.max(cursor.saturating_add(1)));
                if !segment.self_deaf || cfg.count_self_deafened() {
                    outcome.sessions.push(VoiceSession {
                        user_id,
                        channel_id,
                        start: cursor,
                        end: stop,
                        seconds: (stop - cursor) as u64,
                    });
                }
                cursor = stop;
            }
        }
    }
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

    /// 2024-03-05 12:00:00 UTC.
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
        // The AFK channel is a voice concept. A text message there is an ordinary
        // message and is counted.
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
        assert_eq!(
            utc_day(left.sessions[0].start),
            utc_day(left.sessions[0].end - 1)
        );
        assert_ne!(
            utc_day(left.sessions[0].start),
            utc_day(left.sessions[1].start)
        );
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
            ActivityConfig::new(vec![0], None, false)
                .unwrap_err()
                .kind(),
            ConfigErrorKind::InvalidChannelId
        );
        assert_eq!(
            ActivityConfig::new(Vec::new(), Some(0), false)
                .unwrap_err()
                .kind(),
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

    /// A small linear congruential generator, so the sweep is seeded and reproducible.
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
                        self_deaf: rng.next().is_multiple_of(5),
                        at: clock,
                    }
                }
                (0..=3, Some(current)) => {
                    let next = if rng.next().is_multiple_of(2) {
                        channels[(rng.next() % 2) as usize]
                    } else {
                        current
                    };
                    let event = VoiceEvent {
                        guild_id: GUILD,
                        user_id: USER,
                        old_channel: Some(current),
                        new_channel: Some(next),
                        self_deaf: rng.next().is_multiple_of(5),
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
                        self_deaf: rng.next().is_multiple_of(5),
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
                    let stale = if rng.next().is_multiple_of(2) {
                        clock - 30
                    } else {
                        clock
                    };
                    VoiceEvent {
                        guild_id: GUILD,
                        user_id: USER,
                        old_channel: in_channel,
                        new_channel: in_channel,
                        self_deaf: rng.next().is_multiple_of(5),
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
            assert_eq!(utc_day(session.start), utc_day(session.end - 1));
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
        let config = cfg();
        let mut rng = Lcg(0x5EED_1234_ABCD_0002);
        let mut counts: BTreeMap<(String, u64, u64), u64> = BTreeMap::new();
        for _ in 0..20_000 {
            let at = (rng.next() % 5_000_000) as i64;
            let event = MessageEvent {
                guild_id: GUILD,
                channel_id: if rng.next().is_multiple_of(4) {
                    EXCLUDED
                } else {
                    CHANNEL
                },
                user_id: if rng.next().is_multiple_of(8) {
                    OTHER_USER
                } else {
                    USER
                },
                is_bot: rng.next().is_multiple_of(6),
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

    // ---- extra behaviour cases ---------------------------------------------

    #[test]
    fn a_self_deaf_change_mid_session_only_drops_the_deafened_stretch() {
        // The member joins audible, then self-deafens halfway, then leaves.
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let mut deafened = VoiceEvent {
            guild_id: GUILD,
            user_id: USER,
            old_channel: Some(CHANNEL),
            new_channel: Some(CHANNEL),
            self_deaf: true,
            at: T + 300,
        };
        ledger.apply_voice(&deafened, &cfg());
        deafened.self_deaf = false;
        deafened.at = T + 500;
        ledger.apply_voice(&deafened, &cfg());
        let left = ledger.apply_voice(&leave(T + 900, CHANNEL), &cfg());
        // The audible stretches before and after the deafened one are credited; the
        // deafened stretch is not.
        let mut credited: Vec<(i64, i64)> =
            left.sessions.iter().map(|s| (s.start, s.end)).collect();
        credited.sort();
        assert_eq!(credited, vec![(T, T + 300), (T + 500, T + 900)]);
        let seconds: u64 = left.sessions.iter().map(|s| s.seconds).sum();
        assert_eq!(seconds, 700);
    }

    #[test]
    fn a_self_deaf_change_mid_session_credits_everything_when_the_flag_is_set() {
        let counting = ActivityConfig::new(Vec::new(), None, true).expect("valid config");
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &counting);
        let deafened = VoiceEvent {
            guild_id: GUILD,
            user_id: USER,
            old_channel: Some(CHANNEL),
            new_channel: Some(CHANNEL),
            self_deaf: true,
            at: T + 300,
        };
        ledger.apply_voice(&deafened, &counting);
        let left = ledger.apply_voice(&leave(T + 900, CHANNEL), &counting);
        let seconds: u64 = left.sessions.iter().map(|s| s.seconds).sum();
        assert_eq!(seconds, 900);
    }

    #[test]
    fn a_voice_session_split_at_midnight_lies_inside_one_day_each() {
        let midnight = day_start(T) + 86_400;
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(midnight - 100, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(midnight + 100, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 2);
        for session in &left.sessions {
            assert_eq!(utc_day(session.start), utc_day(session.end - 1));
        }
        assert_eq!(utc_day(left.sessions[0].start), utc_day(midnight - 100));
        assert_eq!(utc_day(left.sessions[1].start), utc_day(midnight));
    }

    #[test]
    fn a_session_spanning_three_days_makes_three_records() {
        let midnight = day_start(T) + 86_400;
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(midnight - 60, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(midnight + 86_400 + 60, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 3);
        let seconds: u64 = left.sessions.iter().map(|s| s.seconds).sum();
        assert_eq!(seconds, 60 + 86_400 + 60);
    }

    #[test]
    fn a_huge_timestamp_is_clamped_to_a_year_the_key_can_hold() {
        let day = utc_day(i64::MAX / 4);
        assert_eq!(day.len(), 10);
        assert_eq!(day, "9999-12-31");
    }

    #[test]
    fn the_day_key_holds_at_both_edges_of_the_range() {
        // The day key must have the same shape for every timestamp, including the
        // two edges of the range the caller could pass.
        for at in [i64::MAX, i64::MAX - 1, i64::MIN, i64::MIN + 1] {
            let day = utc_day(at);
            assert_eq!(day.len(), 10, "the day key must have ten characters");
            assert!(
                day.bytes().all(|b| b.is_ascii_digit() || b == b'-'),
                "the day key must be digits and dashes"
            );
        }
        assert_eq!(utc_day(i64::MAX), "9999-12-31");
        assert_eq!(utc_day(i64::MIN), "0000-01-01");
    }

    #[test]
    fn a_timestamp_before_the_epoch_gets_a_valid_day() {
        let day = utc_day(-1);
        assert_eq!(day, "1969-12-31");
        assert_eq!(day.len(), 10);
    }

    #[test]
    fn the_day_start_floors_a_timestamp_before_the_epoch() {
        // -1 second is inside the day that started 86400 seconds before the epoch. A
        // truncation towards zero would put it in 1970-01-01 instead.
        assert_eq!(day_start(-1), -86_400);
        assert_eq!(day_start(-86_400), -86_400);
        assert_eq!(day_start(-86_401), -172_800);
        assert_eq!(day_start(0), 0);
        assert_eq!(day_start(86_399), 0);
    }

    #[test]
    fn the_next_day_start_is_the_day_after_the_day_start() {
        assert_eq!(next_day_start(0), 86_400);
        assert_eq!(next_day_start(86_399), 86_400);
        assert_eq!(next_day_start(-1), 0);
        assert_eq!(next_day_start(-86_400), 0);
        // The next day start is exactly one day later, never the same instant.
        for at in [0i64, 1, 86_399, 86_400, -1, -86_400, 1_709_640_000] {
            assert_eq!(next_day_start(at), day_start(at) + 86_400);
            assert!(next_day_start(at) > at);
        }
    }

    #[test]
    fn the_day_start_is_the_start_of_the_day_the_day_key_names() {
        for at in [0i64, 1, 86_399, 86_400, -1, -86_400, 1_709_640_000] {
            let day = utc_day(at);
            assert_eq!(utc_day(day_start(at)), day);
        }
    }

    #[test]
    fn a_bot_in_an_excluded_channel_is_reported_as_the_bot() {
        // The order of the checks is fixed: a bot author is reported as a bot, not as
        // an excluded channel, so the reader knows why it was dropped.
        let mut event = message(T);
        event.is_bot = true;
        event.channel_id = EXCLUDED;
        assert_eq!(
            account_message(&event, &cfg()),
            MessageOutcome::Ignored(IgnoreReason::BotAuthor)
        );
    }

    #[test]
    fn a_zero_id_is_reported_before_the_bot_check() {
        let mut event = message(T);
        event.is_bot = true;
        event.user_id = 0;
        assert_eq!(
            account_message(&event, &cfg()),
            MessageOutcome::Ignored(IgnoreReason::InvalidUser)
        );
    }

    #[test]
    fn a_message_in_an_excluded_channel_is_never_counted_even_when_the_channel_is_also_the_afk_channel(
    ) {
        let both = ActivityConfig::new(vec![AFK], Some(AFK), false).expect("valid config");
        let mut event = message(T);
        event.channel_id = AFK;
        assert_eq!(
            account_message(&event, &both),
            MessageOutcome::Ignored(IgnoreReason::ExcludedChannel)
        );
    }

    #[test]
    fn two_members_in_the_same_channel_are_accounted_separately() {
        let mut ledger = Ledger::new();
        let mut other = join(T, CHANNEL);
        other.user_id = OTHER_USER;
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        ledger.apply_voice(&other, &cfg());
        let mut other_leave = leave(T + 600, CHANNEL);
        other_leave.user_id = OTHER_USER;
        let left = ledger.apply_voice(&other_leave, &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].user_id, OTHER_USER);
        let left = ledger.apply_voice(&leave(T + 300, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].user_id, USER);
        assert_eq!(left.sessions[0].seconds, 300);
    }

    #[test]
    fn a_restart_with_no_open_session_produces_nothing() {
        let mut ledger = Ledger::new();
        let restart = ledger.note_restart(T + 86_400, T + 60, &cfg());
        assert!(restart.sessions.is_empty());
        assert!(restart.anomalies.is_empty());
    }

    #[test]
    fn a_restart_clears_the_open_sessions() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        assert_eq!(ledger.open_session_count(), 1);
        ledger.note_restart(T + 86_400, T + 60, &cfg());
        assert_eq!(ledger.open_session_count(), 0);
        assert!(ledger.is_idle(GUILD, USER));
    }

    #[test]
    fn a_session_after_a_restart_starts_fresh_and_does_not_reuse_the_old_span() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        ledger.note_restart(T + 86_400, T + 60, &cfg());
        // The member comes back long after the restart. Only the new presence counts.
        let back = T + 86_400;
        ledger.apply_voice(&join(back, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(back + 300, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].start, back);
        assert_eq!(left.sessions[0].seconds, 300);
    }

    #[test]
    fn a_duplicate_leave_does_not_credit_twice() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let first = ledger.apply_voice(&leave(T + 600, CHANNEL), &cfg());
        assert_eq!(first.sessions.len(), 1);
        let second = ledger.apply_voice(&leave(T + 600, CHANNEL), &cfg());
        assert!(second.sessions.is_empty());
        assert_eq!(second.anomalies.len(), 1);
        assert_eq!(second.anomalies[0].kind, AnomalyKind::MissingOldState);
    }

    #[test]
    fn a_leave_after_a_move_closes_the_new_channel_not_the_old_one() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        ledger.apply_voice(&move_to(T + 100, CHANNEL, OTHER_CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(T + 200, OTHER_CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].channel_id, OTHER_CHANNEL);
        assert_eq!(left.sessions[0].start, T + 100);
        assert_eq!(left.sessions[0].seconds, 100);
    }

    #[test]
    fn a_move_into_the_afk_channel_closes_the_old_channel_and_credits_nothing_after() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let moved = ledger.apply_voice(&move_to(T + 300, CHANNEL, AFK), &cfg());
        assert_eq!(moved.sessions.len(), 1);
        assert_eq!(moved.sessions[0].channel_id, CHANNEL);
        assert_eq!(moved.sessions[0].seconds, 300);
        let left = ledger.apply_voice(&leave(T + 900, AFK), &cfg());
        assert!(left.sessions.is_empty());
        assert_eq!(left.anomalies.len(), 1);
        assert_eq!(left.anomalies[0].kind, AnomalyKind::UnaccountedChannel);
    }

    #[test]
    fn an_update_with_no_channel_on_either_side_accounts_nothing() {
        let mut ledger = Ledger::new();
        let event = VoiceEvent {
            guild_id: GUILD,
            user_id: USER,
            old_channel: None,
            new_channel: None,
            self_deaf: false,
            at: T,
        };
        let outcome = ledger.apply_voice(&event, &cfg());
        assert!(outcome.sessions.is_empty());
        assert!(outcome.anomalies.is_empty());
        assert!(ledger.is_idle(GUILD, USER));
    }

    #[test]
    fn an_update_in_the_same_channel_keeps_the_session_open() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let event = VoiceEvent {
            guild_id: GUILD,
            user_id: USER,
            old_channel: Some(CHANNEL),
            new_channel: Some(CHANNEL),
            self_deaf: false,
            at: T + 300,
        };
        let outcome = ledger.apply_voice(&event, &cfg());
        assert!(outcome.sessions.is_empty());
        assert!(!ledger.is_idle(GUILD, USER));
        let left = ledger.apply_voice(&leave(T + 900, CHANNEL), &cfg());
        // The update splits the presence into two stretches. Together they cover the
        // whole presence once and never overlap.
        let seconds: u64 = left.sessions.iter().map(|s| s.seconds).sum();
        assert_eq!(seconds, 900);
        let mut spans: Vec<(i64, i64)> = left.sessions.iter().map(|s| (s.start, s.end)).collect();
        spans.sort();
        for pair in spans.windows(2) {
            assert!(pair[1].0 >= pair[0].1, "the stretches must not overlap");
        }
    }

    #[test]
    fn the_ledger_reports_the_open_channel_and_the_last_seen_time() {
        let mut ledger = Ledger::new();
        assert_eq!(ledger.open_channel(GUILD, USER), None);
        assert_eq!(ledger.last_seen_at(GUILD, USER), None);
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        assert_eq!(ledger.open_channel(GUILD, USER), Some(CHANNEL));
        assert_eq!(ledger.last_seen_at(GUILD, USER), Some(T));
        ledger.apply_voice(&move_to(T + 300, CHANNEL, OTHER_CHANNEL), &cfg());
        assert_eq!(ledger.open_channel(GUILD, USER), Some(OTHER_CHANNEL));
        assert_eq!(ledger.last_seen_at(GUILD, USER), Some(T + 300));
        ledger.apply_voice(&leave(T + 900, OTHER_CHANNEL), &cfg());
        assert_eq!(ledger.open_channel(GUILD, USER), None);
        // The last accepted update is still known, so a restart could use it.
        assert_eq!(ledger.last_seen_at(GUILD, USER), Some(T + 900));
    }

    #[test]
    fn the_ledger_keeps_the_members_apart() {
        let mut ledger = Ledger::new();
        let mut other = join(T, OTHER_CHANNEL);
        other.user_id = OTHER_USER;
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        ledger.apply_voice(&other, &cfg());
        assert_eq!(ledger.open_session_count(), 2);
        assert_eq!(ledger.open_channel(GUILD, USER), Some(CHANNEL));
        assert_eq!(ledger.open_channel(GUILD, OTHER_USER), Some(OTHER_CHANNEL));
    }

    #[test]
    fn the_anomaly_records_carry_the_guild_and_the_member() {
        let mut ledger = Ledger::new();
        let moved = ledger.apply_voice(&move_to(T, CHANNEL, OTHER_CHANNEL), &cfg());
        assert_eq!(moved.anomalies.len(), 1);
        assert_eq!(moved.anomalies[0].guild_id, GUILD);
        assert_eq!(moved.anomalies[0].user_id, USER);
        assert_eq!(moved.anomalies[0].at, T);
    }

    #[test]
    fn each_anomaly_kind_has_its_own_code() {
        let kinds = [
            AnomalyKind::MissingOldState,
            AnomalyKind::DuplicateUpdate,
            AnomalyKind::OutOfOrderUpdate,
            AnomalyKind::ChannelMismatch,
            AnomalyKind::RestartGap,
            AnomalyKind::UnaccountedChannel,
        ];
        let mut codes: Vec<&'static str> = kinds
            .iter()
            .map(|kind| {
                Anomaly {
                    kind: *kind,
                    guild_id: GUILD,
                    user_id: USER,
                    channel_id: None,
                    at: 0,
                }
                .code()
            })
            .collect();
        let count = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), count, "the codes must be distinct");
        assert!(codes.contains(&"missing_old_state"));
        assert!(codes.contains(&"duplicate_update"));
        assert!(codes.contains(&"out_of_order_update"));
        assert!(codes.contains(&"channel_mismatch"));
        assert!(codes.contains(&"restart_gap"));
        assert!(codes.contains(&"unaccounted_channel"));
    }

    #[test]
    fn the_two_restart_anomalies_are_not_the_same_kind() {
        // A restart closes the session in an unaccounted channel: the channel anomaly
        // and the restart gap are two different facts about the same restart.
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, AFK), &cfg());
        let restart = ledger.note_restart(T + 60, T + 30, &cfg());
        let kinds: Vec<AnomalyKind> = restart.anomalies.iter().map(|a| a.kind).collect();
        assert!(kinds.contains(&AnomalyKind::UnaccountedChannel));
        assert!(kinds.contains(&AnomalyKind::RestartGap));
        assert_eq!(kinds.len(), 2);
    }

    #[test]
    fn the_config_reports_the_key_of_the_refusal() {
        let error = ActivityConfig::new(Vec::new(), Some(0), false).unwrap_err();
        assert_eq!(error.key(), "ACTIVITY_AFK_CHANNEL_ID");
    }

    #[test]
    fn the_config_exposes_what_it_was_given() {
        let config = ActivityConfig::new(vec![EXCLUDED], Some(AFK), true).expect("valid config");
        assert_eq!(config.excluded_channels(), &[EXCLUDED]);
        assert_eq!(config.afk_channel(), Some(AFK));
        assert!(config.count_self_deafened());
        assert!(config.is_excluded(EXCLUDED));
        assert!(!config.is_excluded(CHANNEL));
        assert!(config.is_afk(AFK));
        assert!(!config.is_afk(CHANNEL));
    }

    #[test]
    fn a_seeded_sweep_never_credits_the_afk_or_an_excluded_channel() {
        let config = cfg();
        let mut ledger = Ledger::new();
        let mut rng = Lcg(0x5EED_1234_ABCD_0003);
        let mut clock = 1_700_000_000i64;
        let channels = [CHANNEL, OTHER_CHANNEL, AFK, EXCLUDED];
        for _ in 0..4_000 {
            clock += (rng.next() % 90) as i64 + 1;
            let channel = channels[(rng.next() % 4) as usize];
            let event = VoiceEvent {
                guild_id: GUILD,
                user_id: USER,
                old_channel: None,
                new_channel: Some(channel),
                self_deaf: false,
                at: clock,
            };
            let outcome = ledger.apply_voice(&event, &config);
            assert!(outcome
                .sessions
                .iter()
                .all(|s| s.channel_id != AFK && s.channel_id != EXCLUDED));
        }
    }

    #[test]
    fn a_seeded_sweep_of_restarts_never_credits_more_than_the_heartbeat() {
        let config = plain_cfg();
        let mut ledger = Ledger::new();
        let mut rng = Lcg(0x5EED_1234_ABCD_0004);
        let mut clock = 1_700_000_000i64;
        let mut credited = 0u64;
        for _ in 0..500 {
            clock += (rng.next() % 300) as i64 + 1;
            let event = VoiceEvent {
                guild_id: GUILD,
                user_id: USER,
                old_channel: None,
                new_channel: Some(CHANNEL),
                self_deaf: false,
                at: clock,
            };
            ledger.apply_voice(&event, &config);
            let heartbeat = clock - 1;
            let restart = ledger.note_restart(clock, heartbeat, &config);
            for session in &restart.sessions {
                assert!(session.end <= heartbeat);
                credited += session.seconds;
            }
        }
        assert_eq!(credited, 0);
    }

    // ---- the edges of the timestamp range -----------------------------------
    //
    // These reach the real edges of `i64`, not a comfortable distance away from
    // them. A timestamp is seconds since the epoch, so `i64::MAX` and `i64::MIN`
    // are the extremes a caller could pass. Nothing may panic on them.

    #[test]
    fn the_next_day_start_is_after_every_cursor_the_day_split_can_take() {
        // The day split walks the cursor forward one UTC day at a time. The cursor
        // is always below the end of the session and the end of a session is an
        // `i64`, so the cursor is at most `i64::MAX - 1`. For every one of those
        // the next day start must be strictly later, otherwise the split could not
        // move on.
        for at in [
            i64::MIN,
            i64::MIN + 1,
            i64::MIN + 86_399,
            i64::MIN + 86_400,
            -86_400,
            -1,
            0,
            86_399,
            86_400,
            1_709_640_000,
            i64::MAX - 86_400,
            i64::MAX - 1,
        ] {
            assert!(
                next_day_start(at) > at,
                "the next day start must be after {at}"
            );
        }
    }

    #[test]
    fn a_join_and_leave_at_the_largest_timestamps_do_not_panic() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(i64::MAX - 50, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(i64::MAX, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].seconds, 50);
        assert_eq!(left.sessions[0].start, i64::MAX - 50);
        assert_eq!(left.sessions[0].end, i64::MAX);
    }

    #[test]
    fn a_join_and_leave_at_the_smallest_timestamps_do_not_panic() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(i64::MIN + 100, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(i64::MIN + 500, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].seconds, 400);
        assert_eq!(left.sessions[0].start, i64::MIN + 100);
        assert_eq!(left.sessions[0].end, i64::MIN + 500);
    }

    #[test]
    fn a_session_that_straddles_the_last_day_does_not_panic() {
        // The session starts in the last day that fits in `i64` and ends at the very
        // end of the range, so the day split has to work at the edge.
        let mut ledger = Ledger::new();
        let start = i64::MAX - 200_000;
        let end = i64::MAX;
        ledger.apply_voice(&join(start, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(end, CHANNEL), &cfg());
        let credited: u64 = left.sessions.iter().map(|s| s.seconds).sum();
        assert_eq!(credited, 200_000);
        for session in &left.sessions {
            assert!(session.start <= session.end);
            // The record may not reach past the end of the session it came from.
            assert!(session.end <= end);
            assert!(session.start >= start);
            assert_eq!(session.seconds, (session.end - session.start) as u64);
        }
    }

    #[test]
    fn the_day_start_does_not_panic_at_the_edges() {
        // The floor of the day is not representable at the very bottom of the
        // range, so it is clamped instead of overflowing.
        assert_eq!(day_start(i64::MIN), i64::MIN);
        assert_eq!(day_start(i64::MIN + 1), i64::MIN);
        assert_eq!(day_start(i64::MAX), i64::MAX - (i64::MAX % 86_400));
        assert_eq!(day_start(i64::MAX - 1), i64::MAX - (i64::MAX % 86_400));
    }

    #[test]
    fn the_next_day_start_does_not_panic_at_the_edges() {
        // The start of the next day is not representable at the very top of the
        // range, so it is clamped instead of overflowing.
        assert_eq!(next_day_start(i64::MAX), i64::MAX);
        assert_eq!(next_day_start(i64::MAX - 1), i64::MAX);
        assert_eq!(next_day_start(i64::MIN), i64::MIN + 86_400);
        assert_eq!(next_day_start(i64::MIN + 1), i64::MIN + 86_400);
    }

    #[test]
    fn the_day_start_stays_the_start_of_the_day_at_the_edges() {
        // The day key and the day start must still agree at the edges, which is
        // what makes the split records land in the day they belong to.
        for at in [i64::MAX, i64::MAX - 1, i64::MIN, i64::MIN + 1] {
            let start = day_start(at);
            assert_eq!(utc_day(start), utc_day(at));
        }
    }

    #[test]
    fn a_seeded_sweep_of_edge_timestamps_never_panics_and_never_overlaps() {
        // A sweep over the whole range: every credited record must stay inside the
        // session it came from, must not overlap its neighbour and must never have
        // a negative length.
        let config = plain_cfg();
        let mut rng = Lcg(0x5EED_1234_ABCD_0005);
        let mut ledger = Ledger::new();
        for _ in 0..2_000 {
            let raw = rng.next() as i64;
            // Fold the random value onto both ends of the range.
            let at = match raw % 4 {
                0 => i64::MAX - (rng.next() as i64 % 1_000_000),
                1 => i64::MIN + (rng.next() as i64 % 1_000_000),
                2 => raw,
                _ => raw / 2,
            };
            let channel = if raw % 2 == 0 { CHANNEL } else { OTHER_CHANNEL };
            let outcome = ledger.apply_voice(&join(at, channel), &config);
            for session in &outcome.sessions {
                assert!(session.start <= session.end);
                assert_eq!(session.seconds, (session.end - session.start) as u64);
            }
            // The leave follows one second after the join. The step is saturating so
            // the sweep itself cannot overflow at the top of the range.
            let leave_at = at.saturating_add(1);
            let left = ledger.apply_voice(&leave(leave_at, channel), &config);
            for session in &left.sessions {
                assert!(session.start <= session.end);
                assert!(session.start >= at);
                assert!(session.end <= leave_at);
                assert_eq!(session.seconds, (session.end - session.start) as u64);
            }
        }
    }

    // ---- a restart that would credit presence that never happened ------------

    #[test]
    fn a_restart_never_credits_past_the_restart_instant() {
        // The heartbeat claims a moment AFTER the restart, so the presence it
        // claims did not happen yet. Only the part before the restart may be
        // credited.
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let restart = ledger.note_restart(T + 10, T + 1_000, &cfg());
        let credited: u64 = restart.sessions.iter().map(|s| s.seconds).sum();
        assert_eq!(credited, 10);
        for session in &restart.sessions {
            assert!(session.end <= T + 10);
        }
    }

    #[test]
    fn a_restart_with_a_heartbeat_after_the_restart_instant_is_flagged() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let restart = ledger.note_restart(T + 10, T + 1_000, &cfg());
        assert_eq!(restart.anomalies.len(), 1);
        assert_eq!(restart.anomalies[0].kind, AnomalyKind::RestartGap);
        // The anomaly marks the START of the uncredited gap, which is the end of
        // the credited span, not the heartbeat that lies about the future.
        assert_eq!(restart.anomalies[0].at, T + 10);
    }

    #[test]
    fn a_restart_before_the_session_started_credits_nothing() {
        // The restart instant is before the session began, so there is no presence
        // to credit at all.
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, CHANNEL), &cfg());
        let restart = ledger.note_restart(T - 100, T - 50, &cfg());
        assert!(restart.sessions.is_empty());
        let credited: u64 = restart.sessions.iter().map(|s| s.seconds).sum();
        assert_eq!(credited, 0);
        assert_eq!(restart.anomalies.len(), 1);
        assert_eq!(restart.anomalies[0].kind, AnomalyKind::RestartGap);
    }

    #[test]
    fn a_restart_never_credits_more_than_the_restart_instant() {
        // The seeded sweep of restarts, with a heartbeat that sometimes lies about
        // the future. The credited span is always bounded by the restart instant.
        let config = plain_cfg();
        let mut rng = Lcg(0x5EED_1234_ABCD_0006);
        let mut clock = 1_700_000_000i64;
        let mut ledger = Ledger::new();
        for _ in 0..500 {
            clock += (rng.next() % 300) as i64 + 1;
            ledger.apply_voice(&join(clock, CHANNEL), &config);
            let heartbeat = clock - 1 + (rng.next() as i64 % 5_000);
            let restart = ledger.note_restart(clock, heartbeat, &config);
            for session in &restart.sessions {
                assert!(session.end <= clock);
            }
        }
    }

    // ---- when the unaccounted channel anomaly is raised ----------------------

    #[test]
    fn the_unaccounted_channel_anomaly_is_raised_when_the_session_is_dropped() {
        // The anomaly appears on the outcome that drops the session, not on the
        // outcome that opened it.
        let mut ledger = Ledger::new();
        let joined = ledger.apply_voice(&join(T, AFK), &cfg());
        assert!(joined.anomalies.is_empty());
        assert!(joined.sessions.is_empty());
        let left = ledger.apply_voice(&leave(T + 600, AFK), &cfg());
        assert_eq!(left.anomalies.len(), 1);
        assert_eq!(left.anomalies[0].kind, AnomalyKind::UnaccountedChannel);
        assert_eq!(left.anomalies[0].channel_id, Some(AFK));
        assert!(left.sessions.is_empty());
    }

    // ---- the record cap ----------------------------------------------------

    #[test]
    fn a_session_that_needs_more_records_than_the_cap_is_not_credited() {
        // A span longer than the cap would need one record per UTC day. The
        // session is not credited and the drop is flagged.
        let start = day_start(T);
        let end = start + (MAX_SESSION_RECORDS as i64 + 1) * SECONDS_PER_DAY;
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(start, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(end, CHANNEL), &cfg());
        assert!(left.sessions.is_empty());
        assert_eq!(left.anomalies.len(), 1);
        assert_eq!(left.anomalies[0].kind, AnomalyKind::UnaccountedChannel);
    }

    #[test]
    fn a_session_that_stays_within_the_cap_is_credited() {
        // Exactly at the cap the session is credited normally, so the cap does
        // not refuse a long but legitimate presence.
        let start = day_start(T);
        let end = start + (MAX_SESSION_RECORDS as i64) * SECONDS_PER_DAY;
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(start, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(end, CHANNEL), &cfg());
        assert_eq!(left.sessions.len(), MAX_SESSION_RECORDS);
        assert!(left.anomalies.is_empty());
    }

    #[test]
    fn a_span_across_the_whole_timestamp_range_is_not_credited_and_does_not_allocate() {
        // Two timestamps at the far ends of the range would need billions of
        // daily records. The session is dropped and flagged instead, and the
        // process does not run out of memory.
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(i64::MIN, CHANNEL), &cfg());
        let left = ledger.apply_voice(&leave(i64::MAX, CHANNEL), &cfg());
        assert!(left.sessions.is_empty());
        assert_eq!(left.anomalies.len(), 1);
        assert_eq!(left.anomalies[0].kind, AnomalyKind::UnaccountedChannel);
    }

    #[test]
    fn a_span_across_the_whole_timestamp_range_is_dropped_at_the_restart_too() {
        // The restart closes the session the same way a leave does, so the same
        // bound applies there. The heartbeat is at the top of the range, so the
        // credited span is the whole range and the cap applies to it.
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(i64::MIN, CHANNEL), &cfg());
        let restart = ledger.note_restart(i64::MAX, i64::MAX, &cfg());
        assert!(restart.sessions.is_empty());
        assert!(restart
            .anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::UnaccountedChannel));
    }

    // ---- the voice path refuses the id zero --------------------------------

    #[test]
    fn a_voice_event_with_a_zero_guild_is_refused() {
        let mut event = join(T, CHANNEL);
        event.guild_id = 0;
        let mut ledger = Ledger::new();
        let outcome = ledger.apply_voice(&event, &cfg());
        assert!(outcome.sessions.is_empty());
        assert_eq!(outcome.anomalies.len(), 1);
        assert_eq!(outcome.anomalies[0].kind, AnomalyKind::UnaccountedChannel);
    }

    #[test]
    fn a_voice_event_with_a_zero_user_is_refused() {
        let mut event = join(T, CHANNEL);
        event.user_id = 0;
        let mut ledger = Ledger::new();
        let outcome = ledger.apply_voice(&event, &cfg());
        assert!(outcome.sessions.is_empty());
        assert_eq!(outcome.anomalies.len(), 1);
        assert_eq!(outcome.anomalies[0].kind, AnomalyKind::UnaccountedChannel);
    }

    #[test]
    fn a_voice_event_with_a_zero_channel_is_refused() {
        let mut ledger = Ledger::new();
        let outcome = ledger.apply_voice(&join(T, 0), &cfg());
        assert!(outcome.sessions.is_empty());
        assert_eq!(outcome.anomalies.len(), 1);
        assert_eq!(outcome.anomalies[0].kind, AnomalyKind::UnaccountedChannel);
        assert_eq!(outcome.anomalies[0].channel_id, Some(0));
    }

    #[test]
    fn a_voice_event_with_a_zero_id_never_credits_a_session() {
        // The whole event is refused, so the leave that follows has no session
        // to close and credits nothing either.
        let mut ledger = Ledger::new();
        let joined = ledger.apply_voice(&join(T, 0), &cfg());
        assert!(joined.sessions.is_empty());
        let left = ledger.apply_voice(&leave(T + 600, 0), &cfg());
        assert!(left.sessions.is_empty());
        assert!(left
            .anomalies
            .iter()
            .all(|a| a.kind == AnomalyKind::UnaccountedChannel));
    }

    #[test]
    fn a_voice_event_with_a_zero_id_opens_no_session() {
        let mut ledger = Ledger::new();
        ledger.apply_voice(&join(T, 0), &cfg());
        assert_eq!(ledger.open_session_count(), 0);
        assert!(ledger.is_idle(GUILD, USER));
    }
}

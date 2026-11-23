//! Voice-room runtime settings (`docs/verification/VOICE-ROOMS-RUNTIME.md`).
//!
//! Reads the six `VC_*` keys from a lookup closure (no `std::env` read here) and parses them
//! only through the K06 helpers in [`crate::settings`]. A value that cannot be read switches
//! the feature off and the error names the KEY only; the value is never carried or echoed.
//! A key that is simply unset is not an error but [`Readiness::Misconfigured`] when the mode
//! is on: the runtime then does nothing at all (fail closed).

use crate::settings::{parse_duration_secs, parse_id_list, parse_mode, Mode, SettingError};

/// Environment key holding the feature mode (`off`, `shadow`, `live`).
pub const KEY_MODE: &str = "VC_MODE";
/// Environment key holding the hub channel ids (comma separated).
pub const KEY_HUB_CHANNEL_IDS: &str = "VC_HUB_CHANNEL_IDS";
/// Environment key holding the category new rooms are created in.
pub const KEY_CATEGORY_ID: &str = "VC_CATEGORY_ID";
/// Environment key holding the per-member create cooldown in seconds.
pub const KEY_CREATE_COOLDOWN_SECS: &str = "VC_CREATE_COOLDOWN_SECS";
/// Environment key holding the maximum number of tracked rooms.
pub const KEY_MAX_ROOMS: &str = "VC_MAX_ROOMS";
/// Environment key holding the empty-room grace period in seconds.
pub const KEY_EMPTY_GRACE_SECS: &str = "VC_EMPTY_GRACE_SECS";

/// Default create cooldown in seconds.
pub const DEFAULT_CREATE_COOLDOWN_SECS: u64 = 30;
/// Default maximum number of rooms.
pub const DEFAULT_MAX_ROOMS: u64 = 25;
/// Default empty-room grace period in seconds.
pub const DEFAULT_EMPTY_GRACE_SECS: u64 = 10;
/// Largest accepted cooldown (one day).
pub const MAX_COOLDOWN_SECS: u64 = 86_400;
/// Largest accepted grace period (one hour).
pub const MAX_GRACE_SECS: u64 = 3_600;
/// Largest accepted room cap (Discord allows 500 channels per server).
pub const MAX_ROOMS_LIMIT: u64 = 500;

/// Stable reason codes for a refused setting. They never carry the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// The value could not be read (not a list of ids, not a number).
    Unreadable,
    /// The value was read but is outside what the runtime accepts.
    OutOfRange,
}

/// A refused setting: the key and a stable code, never the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigError {
    key: &'static str,
    kind: ConfigErrorKind,
}

impl ConfigError {
    /// The configuration key that was refused.
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// The stable reason code.
    pub fn kind(&self) -> ConfigErrorKind {
        self.kind
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why = match self.kind {
            ConfigErrorKind::Unreadable => "value could not be read",
            ConfigErrorKind::OutOfRange => "value out of range",
        };
        write!(f, "{}: {why}", self.key)
    }
}

impl std::error::Error for ConfigError {}

/// Why a mode that is on still does nothing. Stable codes, no values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    /// `GUILD_ID` is unset or zero: there is no home guild to serve.
    HomeGuildUnset,
    /// `VC_CATEGORY_ID` is unset: there is nowhere to create rooms.
    CategoryUnset,
    /// `VC_HUB_CHANNEL_IDS` is empty: nothing to join.
    NoHubChannel,
}

impl NotReady {
    /// The stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            NotReady::HomeGuildUnset => "home_guild_unset",
            NotReady::CategoryUnset => "category_unset",
            NotReady::NoHubChannel => "no_hub_channel",
        }
    }
}

/// Whether the runtime may act.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// The mode is `off`.
    Off,
    /// The mode is on and everything the runtime needs is set.
    Ready,
    /// The mode is on but something is unset: the runtime does nothing.
    Misconfigured(NotReady),
}

/// The parsed settings of the voice-room runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSettings {
    mode: Mode,
    guild_id: Option<u64>,
    hubs: Vec<u64>,
    category_id: Option<u64>,
    create_cooldown_secs: i64,
    max_rooms: usize,
    empty_grace_secs: i64,
}

impl VoiceSettings {
    /// The configured mode. `Off` whenever a setting was refused.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The home guild, if one is configured and non-zero.
    pub fn guild_id(&self) -> Option<u64> {
        self.guild_id.filter(|id| *id != 0)
    }

    /// The hub channel ids, sorted and de-duplicated.
    pub fn hubs(&self) -> &[u64] {
        &self.hubs
    }

    /// True when `channel` is a configured hub.
    pub fn is_hub(&self, channel: u64) -> bool {
        channel != 0 && self.hubs.contains(&channel)
    }

    /// The category new rooms are created in.
    pub fn category_id(&self) -> Option<u64> {
        self.category_id.filter(|id| *id != 0)
    }

    /// Seconds a member must wait between two room creations.
    pub fn create_cooldown_secs(&self) -> i64 {
        self.create_cooldown_secs
    }

    /// The most rooms that may be tracked at once.
    pub fn max_rooms(&self) -> usize {
        self.max_rooms
    }

    /// Seconds a room stays empty before it is deleted.
    pub fn empty_grace_secs(&self) -> i64 {
        self.empty_grace_secs
    }

    /// Off, ready, or on-but-unset (fail closed).
    pub fn readiness(&self) -> Readiness {
        if self.mode == Mode::Off {
            return Readiness::Off;
        }
        if self.guild_id().is_none() {
            return Readiness::Misconfigured(NotReady::HomeGuildUnset);
        }
        if self.category_id().is_none() {
            return Readiness::Misconfigured(NotReady::CategoryUnset);
        }
        if self.hubs.is_empty() {
            return Readiness::Misconfigured(NotReady::NoHubChannel);
        }
        Readiness::Ready
    }

    /// True when the runtime may act: a mode other than `Off` and everything set.
    pub fn is_active(&self) -> bool {
        self.readiness() == Readiness::Ready
    }

    fn off(guild_id: Option<u64>) -> Self {
        VoiceSettings {
            mode: Mode::Off,
            guild_id,
            hubs: Vec::new(),
            category_id: None,
            create_cooldown_secs: i64::try_from(DEFAULT_CREATE_COOLDOWN_SECS).unwrap_or(30),
            max_rooms: usize::try_from(DEFAULT_MAX_ROOMS).unwrap_or(25),
            empty_grace_secs: i64::try_from(DEFAULT_EMPTY_GRACE_SECS).unwrap_or(10),
        }
    }
}

/// The result of [`load`]: the settings to use and the refusal that caused `Off`, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The settings; mode `Off` and nothing configured when `error` is set.
    pub settings: VoiceSettings,
    /// The first refused setting, naming only its key.
    pub error: Option<ConfigError>,
}

/// Parses the `VC_*` keys from `lookup` (blank counts as unset). `guild_id` is the
/// already-parsed `GUILD_ID` (the home guild).
pub fn load(lookup: impl Fn(&str) -> Option<String>, guild_id: Option<u64>) -> Loaded {
    match parse(&lookup, guild_id) {
        Ok(settings) => Loaded {
            settings,
            error: None,
        },
        Err(error) => Loaded {
            settings: VoiceSettings::off(guild_id),
            error: Some(error),
        },
    }
}

fn parse(
    lookup: &impl Fn(&str) -> Option<String>,
    guild_id: Option<u64>,
) -> Result<VoiceSettings, ConfigError> {
    // `parse_mode` has no error path: anything but off/shadow/live is `Off`.
    let mode = parse_mode(KEY_MODE, lookup(KEY_MODE).as_deref()).unwrap_or(Mode::Off);
    let hubs = parse_id_list(KEY_HUB_CHANNEL_IDS, lookup(KEY_HUB_CHANNEL_IDS).as_deref())
        .map_err(|e| refused(KEY_HUB_CHANNEL_IDS, &e))?;
    let category = parse_id_list(KEY_CATEGORY_ID, lookup(KEY_CATEGORY_ID).as_deref())
        .map_err(|e| refused(KEY_CATEGORY_ID, &e))?;
    let category_id = match category.as_slice() {
        [] => None,
        [one] => Some(*one),
        // A list of categories is not one category.
        _ => {
            return Err(ConfigError {
                key: KEY_CATEGORY_ID,
                kind: ConfigErrorKind::Unreadable,
            })
        }
    };
    let cooldown = number(
        KEY_CREATE_COOLDOWN_SECS,
        lookup(KEY_CREATE_COOLDOWN_SECS).as_deref(),
        (0, MAX_COOLDOWN_SECS),
        DEFAULT_CREATE_COOLDOWN_SECS,
    )?;
    let max_rooms = number(
        KEY_MAX_ROOMS,
        lookup(KEY_MAX_ROOMS).as_deref(),
        (1, MAX_ROOMS_LIMIT),
        DEFAULT_MAX_ROOMS,
    )?;
    let grace = number(
        KEY_EMPTY_GRACE_SECS,
        lookup(KEY_EMPTY_GRACE_SECS).as_deref(),
        (0, MAX_GRACE_SECS),
        DEFAULT_EMPTY_GRACE_SECS,
    )?;
    Ok(VoiceSettings {
        mode,
        guild_id,
        hubs,
        category_id,
        // Both bounded far below `i64::MAX` by the ranges above.
        create_cooldown_secs: i64::try_from(cooldown).unwrap_or(i64::MAX),
        max_rooms: usize::try_from(max_rooms).unwrap_or(usize::MAX),
        empty_grace_secs: i64::try_from(grace).unwrap_or(i64::MAX),
    })
}

/// One bounded number; unset or blank is `default`, anything unreadable is refused.
fn number(
    key: &'static str,
    raw: Option<&str>,
    (min, max): (u64, u64),
    default: u64,
) -> Result<u64, ConfigError> {
    match parse_duration_secs(key, raw, min, max) {
        Ok(value) => Ok(value),
        Err(SettingError::Unset(_)) => Ok(default),
        Err(e) => Err(refused(key, &e)),
    }
}

/// Maps a K06 error to this module's error, keeping only the key and a stable kind.
fn refused(key: &'static str, error: &SettingError) -> ConfigError {
    let kind = match error {
        SettingError::OutOfRange(_) => ConfigErrorKind::OutOfRange,
        SettingError::Unset(_) | SettingError::InvalidValue(_) | SettingError::ParseError(_) => {
            ConfigErrorKind::Unreadable
        }
    };
    ConfigError { key, kind }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic ids, far from any real Discord snowflake.
    const GUILD: u64 = 6_100_000_000_000_001;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .rev()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn live(extra: &[(&str, &str)]) -> Loaded {
        let mut pairs: Vec<(&str, &str)> = vec![
            (KEY_MODE, "live"),
            (KEY_HUB_CHANNEL_IDS, "6100000000000002"),
            (KEY_CATEGORY_ID, "6100000000000003"),
        ];
        pairs.extend_from_slice(extra);
        load(env(&pairs), Some(GUILD))
    }

    #[test]
    fn nothing_set_is_off_with_no_error_and_the_documented_defaults() {
        let loaded = load(env(&[]), None);
        assert_eq!(loaded.error, None);
        let s = loaded.settings;
        assert_eq!(s.mode(), Mode::Off);
        assert_eq!(s.readiness(), Readiness::Off);
        assert!(!s.is_active());
        assert!(s.hubs().is_empty());
        assert_eq!(s.category_id(), None);
        assert_eq!(s.create_cooldown_secs(), 30);
        assert_eq!(s.max_rooms(), 25);
        assert_eq!(s.empty_grace_secs(), 10);
    }

    #[test]
    fn a_full_valid_live_configuration_is_ready() {
        let loaded = live(&[
            (
                KEY_HUB_CHANNEL_IDS,
                " 6100000000000005 , 6100000000000002,6100000000000005",
            ),
            (KEY_CREATE_COOLDOWN_SECS, "45"),
            (KEY_MAX_ROOMS, "7"),
            (KEY_EMPTY_GRACE_SECS, "0"),
        ]);
        assert_eq!(loaded.error, None);
        let s = loaded.settings;
        assert_eq!(s.mode(), Mode::Live);
        assert_eq!(s.readiness(), Readiness::Ready);
        assert!(s.is_active());
        assert_eq!(s.guild_id(), Some(GUILD));
        assert_eq!(s.hubs(), &[6_100_000_000_000_002, 6_100_000_000_000_005]);
        assert!(s.is_hub(6_100_000_000_000_005));
        assert!(!s.is_hub(6_100_000_000_000_003));
        assert!(!s.is_hub(0));
        assert_eq!(s.category_id(), Some(6_100_000_000_000_003));
        assert_eq!(s.create_cooldown_secs(), 45);
        assert_eq!(s.max_rooms(), 7);
        assert_eq!(s.empty_grace_secs(), 0);
    }

    #[test]
    fn blank_values_use_the_defaults() {
        let loaded = live(&[
            (KEY_CREATE_COOLDOWN_SECS, "  "),
            (KEY_MAX_ROOMS, ""),
            (KEY_EMPTY_GRACE_SECS, " "),
        ]);
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.settings.create_cooldown_secs(), 30);
        assert_eq!(loaded.settings.max_rooms(), 25);
        assert_eq!(loaded.settings.empty_grace_secs(), 10);
    }

    #[test]
    fn an_unset_home_guild_category_or_hub_fails_closed_when_the_mode_is_on() {
        let no_guild = load(
            env(&[
                (KEY_MODE, "live"),
                (KEY_HUB_CHANNEL_IDS, "6100000000000002"),
                (KEY_CATEGORY_ID, "6100000000000003"),
            ]),
            None,
        );
        assert_eq!(
            no_guild.settings.readiness(),
            Readiness::Misconfigured(NotReady::HomeGuildUnset)
        );
        // A zero guild is as good as none.
        let zero_guild = load(
            env(&[
                (KEY_MODE, "shadow"),
                (KEY_HUB_CHANNEL_IDS, "6100000000000002"),
                (KEY_CATEGORY_ID, "6100000000000003"),
            ]),
            Some(0),
        );
        assert_eq!(
            zero_guild.settings.readiness(),
            Readiness::Misconfigured(NotReady::HomeGuildUnset)
        );
        assert!(!zero_guild.settings.is_active());
        let no_category = load(
            env(&[
                (KEY_MODE, "live"),
                (KEY_HUB_CHANNEL_IDS, "6100000000000002"),
            ]),
            Some(GUILD),
        );
        assert_eq!(
            no_category.settings.readiness(),
            Readiness::Misconfigured(NotReady::CategoryUnset)
        );
        let no_hub = load(
            env(&[(KEY_MODE, "live"), (KEY_CATEGORY_ID, "6100000000000003")]),
            Some(GUILD),
        );
        assert_eq!(
            no_hub.settings.readiness(),
            Readiness::Misconfigured(NotReady::NoHubChannel)
        );
        for code in [
            NotReady::HomeGuildUnset,
            NotReady::CategoryUnset,
            NotReady::NoHubChannel,
        ] {
            assert!(code
                .code()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }

    #[test]
    fn off_stays_off_even_when_everything_else_is_unset() {
        let loaded = load(env(&[(KEY_MODE, "off")]), None);
        assert_eq!(loaded.settings.readiness(), Readiness::Off);
        // An unknown mode word is off, not an error.
        let loaded = live(&[(KEY_MODE, "yes please")]);
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.settings.readiness(), Readiness::Off);
    }

    #[test]
    fn a_refused_value_switches_the_feature_off_and_names_only_the_key() {
        let cases: [(&str, &str, ConfigErrorKind); 9] = [
            (KEY_HUB_CHANNEL_IDS, "12,abc", ConfigErrorKind::Unreadable),
            (KEY_HUB_CHANNEL_IDS, "0", ConfigErrorKind::Unreadable),
            (KEY_CATEGORY_ID, "1,2", ConfigErrorKind::Unreadable),
            (KEY_CATEGORY_ID, "-5", ConfigErrorKind::Unreadable),
            (KEY_CREATE_COOLDOWN_SECS, "-1", ConfigErrorKind::Unreadable),
            (
                KEY_CREATE_COOLDOWN_SECS,
                "86401",
                ConfigErrorKind::OutOfRange,
            ),
            (KEY_MAX_ROOMS, "0", ConfigErrorKind::OutOfRange),
            (KEY_MAX_ROOMS, "501", ConfigErrorKind::OutOfRange),
            (KEY_EMPTY_GRACE_SECS, "3601", ConfigErrorKind::OutOfRange),
        ];
        for (key, value, kind) in cases {
            let loaded = live(&[(key, value)]);
            let error = loaded.error.unwrap_or_else(|| panic!("{key}={value}"));
            assert_eq!(error.key(), key, "{key}={value}");
            assert_eq!(error.kind(), kind, "{key}={value}");
            assert_eq!(loaded.settings.mode(), Mode::Off, "{key}={value}");
            assert_eq!(loaded.settings.readiness(), Readiness::Off, "{key}={value}");
            // The value is never echoed.
            assert!(!error.to_string().contains(value), "{key}={value}");
            assert!(error.to_string().starts_with(key));
        }
    }

    #[test]
    fn the_range_edges_are_accepted() {
        let loaded = live(&[
            (KEY_CREATE_COOLDOWN_SECS, "86400"),
            (KEY_MAX_ROOMS, "500"),
            (KEY_EMPTY_GRACE_SECS, "3600"),
        ]);
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.settings.create_cooldown_secs(), 86_400);
        assert_eq!(loaded.settings.max_rooms(), 500);
        assert_eq!(loaded.settings.empty_grace_secs(), 3_600);
        let loaded = live(&[(KEY_CREATE_COOLDOWN_SECS, "0"), (KEY_MAX_ROOMS, "1")]);
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.settings.create_cooldown_secs(), 0);
        assert_eq!(loaded.settings.max_rooms(), 1);
    }
}

//! Bump runtime configuration (BU2).
//!
//! Pure parsing of the `BUMP_*` keys. The module reads no environment itself: the
//! caller passes a lookup closure (`Fn(&str) -> Option<String>`), the same shape the
//! other feature config modules use.
//!
//! Every value is parsed through the K06 helpers in `crate::settings`
//! ([`settings::parse_mode`], [`settings::parse_id_list`], [`settings::parse_duration_secs`]).
//! Nothing here parses a number or a mode by hand.
//!
//! Fail-closed rule: a key whose value is refused puts that key in
//! [`BumpSettings::invalid_keys`] and the feature is switched OFF. An error names the
//! KEY and never the value, so a secret cannot leak through the startup line.
//!
//! The keys are READ by the wiring card (W3), not by this module: it stages them in
//! `.env.example` and supplies the values here.

use crate::settings::{parse_duration_secs, parse_id_list, parse_mode, Mode};

use super::policy::{
    BumpConfig, ConfigError, DEFAULT_INTERVAL_SECS, DEFAULT_SUCCESS_MARKER, KEY_BOT_ID,
    KEY_INTERVAL_SECS, KEY_SUCCESS_MARKER, MAX_INTERVAL_SECS, MIN_INTERVAL_SECS,
};

/// Environment key holding the feature mode (`off`, `shadow`, `live`).
pub const KEY_MODE: &str = "BUMP_MODE";
/// Environment key holding the id of the channel the bump bot posts in.
pub const KEY_CHANNEL_ID: &str = "BUMP_CHANNEL_ID";
/// Environment key holding the id of the role the reminder may mention.
pub const KEY_ROLE_ID: &str = "BUMP_ROLE_ID";

/// The parsed bump settings.
///
/// `invalid_keys` lists every key whose value was refused. A non-empty list means the
/// feature must stay off: [`BumpSettings::is_usable`] is false and
/// [`BumpSettings::effective_mode`] is [`Mode::Off`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BumpSettings {
    /// The requested mode. `Off` unless the feature is switched on explicitly.
    pub mode: Mode,
    /// The channel the reminder is posted in.
    pub channel_id: Option<u64>,
    /// The only role a reminder may mention.
    pub role_id: Option<u64>,
    /// The listing bot whose success reply counts as a bump.
    pub bot_id: Option<u64>,
    /// The reminder interval in seconds.
    pub interval_secs: u64,
    /// The success marker, matched case-sensitively as a substring.
    pub success_marker: String,
    /// The guild the feature serves. The wiring card supplies it (there is no
    /// `BUMP_GUILD_ID` key in the card's list; the plan's `BUMP_GUILD_ID` in
    /// `policy` is a placeholder).
    pub guild_id: Option<u64>,
    /// Keys whose value was refused. The feature stays off while this is not empty.
    pub invalid_keys: Vec<&'static str>,
}

impl BumpSettings {
    /// True when every configured value was accepted and the feature may run.
    pub fn is_usable(&self) -> bool {
        self.invalid_keys.is_empty()
    }

    /// The mode the feature actually runs in: `Off` when any value was refused.
    pub fn effective_mode(&self) -> Mode {
        if self.is_usable() {
            self.mode
        } else {
            Mode::Off
        }
    }

    /// Build the validated policy configuration for this settings set.
    ///
    /// `Err` carries the policy's own [`ConfigError`], which names the key and never
    /// the value. The settings already parsed every value, so this only refuses a
    /// combination the policy rejects (for example a zero id or a blank marker).
    pub fn policy_config(&self) -> Result<BumpConfig, ConfigError> {
        let guild = self.guild_id.unwrap_or(0);
        let bot = self.bot_id.unwrap_or(0);
        BumpConfig::new(guild, bot, &self.success_marker, self.interval_secs)
    }
}

/// Read one key through the lookup closure, treating an empty value as unset.
///
/// The K06 helpers trim their input, so an empty string would parse as "unset" for
/// the mode but as a parse error for an id. Treating it as unset here keeps one rule
/// for every key: an empty value means "not configured".
fn read(lookup: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    lookup(key).filter(|raw| !raw.trim().is_empty())
}

/// Parse the single id in `key`.
///
/// A list with more than one id is refused: these keys name exactly one id, and
/// silently taking the first would configure the wrong channel or role.
fn one_id(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    invalid: &mut Vec<&'static str>,
) -> Option<u64> {
    let raw = read(lookup, key)?;
    match parse_id_list(key, Some(&raw)) {
        Ok(ids) if ids.len() == 1 => Some(ids[0]),
        _ => {
            invalid.push(key);
            None
        }
    }
}

/// Parse every `BUMP_*` key.
///
/// `lookup` is the only source of values: this module never reads the environment.
/// `guild_id` is supplied by the caller because the card's key list has no guild key.
///
/// Safe-failure rules:
/// - `BUMP_MODE` unset or unrecognised: the feature is off. A typo is reported in
///   `invalid_keys` so the wiring card can name the key in its startup line.
/// - A missing id is not an error by itself: the mode decides whether the feature
///   needs it. `BUMP_MODE=off` with no ids at all is the normal default.
/// - A value that is present but refused (bad id, bad duration, blank marker) puts
///   its key in `invalid_keys` and the feature is off.
pub fn bump_settings(
    lookup: impl Fn(&str) -> Option<String>,
    guild_id: Option<u64>,
) -> BumpSettings {
    let mut invalid: Vec<&'static str> = Vec::new();

    // Mode first: an off feature must not complain about ids it will never use.
    let mode_raw = read(&lookup, KEY_MODE);
    let mode = parse_mode(KEY_MODE, mode_raw.as_deref()).unwrap_or(Mode::Off);
    if mode_raw.is_some() && mode == Mode::Off {
        // `parse_mode` maps anything it does not know to `Off`. A present value that
        // is not the literal "off" is a typo, and the key is named so the wiring
        // card can report it.
        let trimmed = mode_raw
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if trimmed != "off" {
            invalid.push(KEY_MODE);
        }
    }

    let channel_id = one_id(&lookup, KEY_CHANNEL_ID, &mut invalid);
    let role_id = one_id(&lookup, KEY_ROLE_ID, &mut invalid);
    let bot_id = one_id(&lookup, KEY_BOT_ID, &mut invalid);

    let interval_secs = match read(&lookup, KEY_INTERVAL_SECS) {
        None => DEFAULT_INTERVAL_SECS,
        Some(raw) => match parse_duration_secs(
            KEY_INTERVAL_SECS,
            Some(&raw),
            MIN_INTERVAL_SECS,
            MAX_INTERVAL_SECS,
        ) {
            Ok(secs) => secs,
            Err(_) => {
                invalid.push(KEY_INTERVAL_SECS);
                DEFAULT_INTERVAL_SECS
            }
        },
    };

    let success_marker = match lookup(KEY_SUCCESS_MARKER) {
        // An unset key takes the default.
        None => DEFAULT_SUCCESS_MARKER.to_string(),
        Some(raw) => {
            // A present value is validated by the policy's own constructor (blank,
            // longer than the policy's cap or holding a control character are
            // refused). A blank value is a bad value, not an unset key.
            let probe = BumpConfig::new(
                guild_id.unwrap_or(1),
                bot_id.unwrap_or(1),
                &raw,
                interval_secs,
            );
            if probe.is_err() {
                invalid.push(KEY_SUCCESS_MARKER);
                DEFAULT_SUCCESS_MARKER.to_string()
            } else {
                raw
            }
        }
    };

    // A live or shadow feature needs a channel, a role and the listing bot.
    if mode != Mode::Off {
        if channel_id.is_none() {
            invalid.push(KEY_CHANNEL_ID);
        }
        if role_id.is_none() {
            invalid.push(KEY_ROLE_ID);
        }
        if bot_id.is_none() {
            invalid.push(KEY_BOT_ID);
        }
    }

    invalid.sort_unstable();
    invalid.dedup();

    BumpSettings {
        mode,
        channel_id,
        role_id,
        bot_id,
        interval_secs,
        success_marker,
        guild_id,
        invalid_keys: invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: u64 = 1_000_001;
    const CHANNEL: u64 = 4_000_001;
    const ROLE: u64 = 5_000_001;
    const BOT: u64 = 2_000_001;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn full() -> impl Fn(&str) -> Option<String> {
        env(&[
            (KEY_MODE, "live"),
            (KEY_CHANNEL_ID, "4000001"),
            (KEY_ROLE_ID, "5000001"),
            (KEY_BOT_ID, "2000001"),
        ])
    }

    #[test]
    fn unset_keys_leave_the_feature_off_and_nothing_is_invalid() {
        let s = bump_settings(env(&[]), Some(GUILD));
        assert_eq!(s.mode, Mode::Off);
        assert_eq!(s.effective_mode(), Mode::Off);
        assert!(s.invalid_keys.is_empty());
        assert!(s.channel_id.is_none());
        assert!(s.role_id.is_none());
        assert!(s.bot_id.is_none());
        assert_eq!(s.interval_secs, DEFAULT_INTERVAL_SECS);
        assert_eq!(s.success_marker, DEFAULT_SUCCESS_MARKER);
    }

    #[test]
    fn a_complete_live_configuration_is_usable() {
        let s = bump_settings(full(), Some(GUILD));
        assert_eq!(s.mode, Mode::Live);
        assert_eq!(s.effective_mode(), Mode::Live);
        assert!(s.is_usable());
        assert!(s.invalid_keys.is_empty());
        assert_eq!(s.channel_id, Some(CHANNEL));
        assert_eq!(s.role_id, Some(ROLE));
        assert_eq!(s.bot_id, Some(BOT));
        assert_eq!(s.interval_secs, DEFAULT_INTERVAL_SECS);
        assert_eq!(s.success_marker, DEFAULT_SUCCESS_MARKER);
    }

    #[test]
    fn shadow_is_a_mode_of_its_own_and_not_a_soft_live() {
        let s = bump_settings(env(&[(KEY_MODE, "shadow")]), Some(GUILD));
        // Shadow still needs the ids it would use; without them it is not usable.
        assert_eq!(s.mode, Mode::Shadow);
        assert!(!s.is_usable());
        assert_eq!(s.effective_mode(), Mode::Off);
        assert!(s.invalid_keys.contains(&KEY_CHANNEL_ID));
    }

    #[test]
    fn a_mode_typo_names_the_key_and_switches_the_feature_off() {
        let s = bump_settings(env(&[(KEY_MODE, "shadoww")]), Some(GUILD));
        assert_eq!(s.mode, Mode::Off);
        assert_eq!(s.effective_mode(), Mode::Off);
        assert!(s.invalid_keys.contains(&KEY_MODE));
    }

    #[test]
    fn an_explicit_off_is_not_a_typo() {
        let s = bump_settings(env(&[(KEY_MODE, "off")]), Some(GUILD));
        assert_eq!(s.mode, Mode::Off);
        assert!(s.invalid_keys.is_empty());
    }

    #[test]
    fn a_bad_id_names_the_key_and_switches_the_feature_off() {
        for bad in [
            "0",
            "-1",
            "abc",
            "1,,2",
            "+5",
            "１２３",
            "18446744073709551616",
        ] {
            let s = bump_settings(
                env(&[
                    (KEY_MODE, "live"),
                    (KEY_CHANNEL_ID, bad),
                    (KEY_ROLE_ID, "5000001"),
                    (KEY_BOT_ID, "2000001"),
                ]),
                Some(GUILD),
            );
            assert_eq!(s.effective_mode(), Mode::Off, "value {bad:?}");
            assert!(s.invalid_keys.contains(&KEY_CHANNEL_ID), "value {bad:?}");
        }
    }

    #[test]
    fn a_list_of_ids_is_refused_because_the_key_names_exactly_one() {
        let s = bump_settings(
            env(&[
                (KEY_MODE, "live"),
                (KEY_CHANNEL_ID, "4000001,4000002"),
                (KEY_ROLE_ID, "5000001"),
                (KEY_BOT_ID, "2000001"),
            ]),
            Some(GUILD),
        );
        assert!(s.invalid_keys.contains(&KEY_CHANNEL_ID));
        assert_eq!(s.effective_mode(), Mode::Off);
        assert!(s.channel_id.is_none());
    }

    #[test]
    fn a_bad_interval_names_the_key_and_falls_back_to_the_default() {
        for bad in ["0", "599", "86401", "-1", "abc", "7200.0", "+7200", "7_200"] {
            let s = bump_settings(env(&[(KEY_INTERVAL_SECS, bad)]), Some(GUILD));
            assert!(s.invalid_keys.contains(&KEY_INTERVAL_SECS), "value {bad:?}");
            assert_eq!(s.interval_secs, DEFAULT_INTERVAL_SECS);
            assert_eq!(s.effective_mode(), Mode::Off);
        }
    }

    #[test]
    fn an_interval_inside_the_bounds_is_accepted() {
        for (raw, want) in [("600", 600u64), ("7200", 7_200), ("86400", 86_400)] {
            let s = bump_settings(env(&[(KEY_INTERVAL_SECS, raw)]), Some(GUILD));
            assert_eq!(s.interval_secs, want, "value {raw:?}");
            assert!(
                !s.invalid_keys.contains(&KEY_INTERVAL_SECS),
                "value {raw:?}"
            );
        }
    }

    #[test]
    fn a_blank_marker_names_the_key_and_falls_back_to_the_default() {
        for bad in ["", "   ", "\t"] {
            let s = bump_settings(env(&[(KEY_SUCCESS_MARKER, bad)]), Some(GUILD));
            assert!(
                s.invalid_keys.contains(&KEY_SUCCESS_MARKER),
                "value {bad:?}"
            );
            assert_eq!(s.success_marker, DEFAULT_SUCCESS_MARKER);
        }
    }

    #[test]
    fn a_custom_marker_is_kept() {
        let s = bump_settings(env(&[(KEY_SUCCESS_MARKER, "Bumped!")]), Some(GUILD));
        assert_eq!(s.success_marker, "Bumped!");
        assert!(!s.invalid_keys.contains(&KEY_SUCCESS_MARKER));
    }

    #[test]
    fn the_policy_config_is_built_from_the_parsed_values() {
        let s = bump_settings(full(), Some(GUILD));
        let c = s.policy_config().unwrap();
        assert_eq!(c.guild_id(), GUILD);
        assert_eq!(c.bot_id(), BOT);
        assert_eq!(c.success_marker(), DEFAULT_SUCCESS_MARKER);
        assert_eq!(c.interval_secs(), DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn a_missing_guild_makes_the_policy_config_refuse() {
        let s = bump_settings(full(), None);
        assert!(s.policy_config().is_err());
    }

    #[test]
    fn an_error_never_echoes_the_value() {
        let secret = "S3CR3T-VALUE";
        let s = bump_settings(
            env(&[(KEY_MODE, "live"), (KEY_CHANNEL_ID, secret)]),
            Some(GUILD),
        );
        assert!(!s.invalid_keys.is_empty());
        let shown = format!("{s:?}");
        assert!(!shown.contains("S3CR3T"), "{shown}");
        let e = s.policy_config().unwrap_err();
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains("S3CR3T"), "{shown}");
    }

    #[test]
    fn the_keys_are_the_documented_names() {
        assert_eq!(KEY_MODE, "BUMP_MODE");
        assert_eq!(KEY_CHANNEL_ID, "BUMP_CHANNEL_ID");
        assert_eq!(KEY_ROLE_ID, "BUMP_ROLE_ID");
        assert_eq!(KEY_BOT_ID, "BUMP_BOT_ID");
        assert_eq!(KEY_INTERVAL_SECS, "BUMP_INTERVAL_SECS");
        assert_eq!(KEY_SUCCESS_MARKER, "BUMP_SUCCESS_MARKER");
    }
}

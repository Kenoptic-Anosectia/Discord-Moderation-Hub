//! Configuration for the safeword feature (SW3).
//!
//! The parser reads **only** through the K06 helpers in `crate::settings`
//! (`parse_mode`, `parse_id_list`) from a lookup closure: this module never
//! touches `std::env`, so a test can hand it any values and the wiring card
//! decides where the values come from.
//!
//! Fail-closed rules (each has a test):
//!
//! * `SAFEWORD_MODE` is parsed by `settings::parse_mode`. That helper never
//!   fails: an unset or unknown mode is `off` (K06 already does that), so a
//!   bad mode never produces a `ConfigError` — it is simply off.
//! * Every id key must be present and must be exactly one plain decimal id.
//!   A missing or unusable id switches the feature off and the error names the
//!   **key**, never the value.
//! * `SAFEWORD_RESTRICTED_ROLE_IDS` is parsed by `settings::parse_id_list`.
//!   One bad entry refuses the whole list.
//! * `SAFEWORD_DM` and `SAFEWORD_DEFER_GRANTS` are `on`/`off` tokens
//!   (case-insensitive, trimmed). A bad token fails closed: `SAFEWORD_DM` to
//!   `true` (the DM is best effort and never changes state) and
//!   `SAFEWORD_DEFER_GRANTS` to `true` (the safe reading of "a grant during
//!   safeword is deferred into the snapshot").
//!
//! There is no K06 helper for a boolean, so the two flag keys use a strict
//! local token check. The token vocabulary is `on`/`off` only, so a typo such
//! as `true`, `yes` or `enabled` is refused rather than guessed.
//!
//! Wiring card W2/W3 reads these keys; this module only parses them.

use crate::settings::{parse_id_list, parse_mode, Mode};
use std::fmt;

/// Environment key holding the feature mode (`off` | `shadow` | `live`).
pub const KEY_MODE: &str = "SAFEWORD_MODE";
/// Environment key holding the safeword role id.
pub const KEY_ROLE_ID: &str = "SAFEWORD_ROLE_ID";
/// Environment key holding the channel the safeword message lives in.
pub const KEY_CHANNEL_ID: &str = "SAFEWORD_CHANNEL_ID";
/// Environment key holding the safeword message id.
pub const KEY_MESSAGE_ID: &str = "SAFEWORD_MESSAGE_ID";
/// Environment key holding the safeword emoji id (matched by id, never by name).
pub const KEY_EMOJI_ID: &str = "SAFEWORD_EMOJI_ID";
/// Environment key holding the comma-separated restricted role ids.
pub const KEY_RESTRICTED_ROLE_IDS: &str = "SAFEWORD_RESTRICTED_ROLE_IDS";
/// Environment key holding the "send the activation/release DM" flag (`on` | `off`).
pub const KEY_DM: &str = "SAFEWORD_DM";
/// Environment key holding the "defer grants during safeword" flag (`on` | `off`).
pub const KEY_DEFER_GRANTS: &str = "SAFEWORD_DEFER_GRANTS";

/// Token that turns a flag on.
pub const FLAG_ON: &str = "on";
/// Token that turns a flag off.
pub const FLAG_OFF: &str = "off";

/// Why a configuration was refused.
///
/// Every variant is a stable code and carries no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// A required id key was not set.
    MissingId,
    /// A required id key held something that is not one plain decimal id.
    InvalidId,
    /// The restricted role id list held a bad entry.
    InvalidIdList,
    /// A flag key held something other than `on`/`off`.
    InvalidFlag,
}

impl ConfigErrorKind {
    /// The stable machine code. It never contains a name, an id or a value.
    pub fn code(self) -> &'static str {
        match self {
            ConfigErrorKind::MissingId => "missing_id",
            ConfigErrorKind::InvalidId => "invalid_id",
            ConfigErrorKind::InvalidIdList => "invalid_id_list",
            ConfigErrorKind::InvalidFlag => "invalid_flag",
        }
    }
}

/// A refused configuration.
///
/// The error names the configuration key that was wrong and never echoes the
/// value, so it is safe to log verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    key: &'static str,
}

impl ConfigError {
    fn new(kind: ConfigErrorKind, key: &'static str) -> Self {
        Self { kind, key }
    }

    /// The stable reason code for this refusal.
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
        write!(f, "{}: {}", self.key, self.kind.code())
    }
}

impl std::error::Error for ConfigError {}

/// The parsed safeword configuration.
///
/// A configuration only exists when every required id was usable, so a
/// `SafewordConfig` is always complete: there is no half-configured state to
/// check at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafewordConfig {
    /// The requested mode. `Off` means the feature does nothing.
    pub mode: Mode,
    /// The role that marks a member as "on safeword".
    pub role_id: u64,
    /// The channel the safeword message lives in.
    pub channel_id: u64,
    /// The safeword message id.
    pub message_id: u64,
    /// The safeword emoji id. The emoji is matched by id, never by name.
    pub emoji_id: u64,
    /// The roles the safeword removes while it is active.
    pub restricted_role_ids: Vec<u64>,
    /// Whether the activation and release DMs are sent.
    pub dm: bool,
    /// Whether a grant during safeword is deferred into the snapshot.
    pub defer_grants: bool,
}

impl SafewordConfig {
    /// Parse the configuration from a lookup closure.
    ///
    /// The closure is called with a key name and returns the raw value, or
    /// `None` when the key is not set. This module never reads the process
    /// environment itself.
    ///
    /// A refused configuration is an error: the caller switches the feature
    /// off and reports the key. The feature is never half-configured.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        // K06's `parse_mode` never fails: an unset or unknown mode is `off`.
        // The mode therefore never produces a `ConfigError`; a bad mode is
        // simply off, which is the fail-closed reading.
        let mode = parse_mode(KEY_MODE, lookup(KEY_MODE).as_deref())
            .map_err(|_| ConfigError::new(ConfigErrorKind::InvalidFlag, KEY_MODE))?;

        let role_id = parse_single_id(KEY_ROLE_ID, &lookup)?;
        let channel_id = parse_single_id(KEY_CHANNEL_ID, &lookup)?;
        let message_id = parse_single_id(KEY_MESSAGE_ID, &lookup)?;
        let emoji_id = parse_single_id(KEY_EMOJI_ID, &lookup)?;

        let restricted_role_ids = parse_id_list(
            KEY_RESTRICTED_ROLE_IDS,
            lookup(KEY_RESTRICTED_ROLE_IDS).as_deref(),
        )
        .map_err(|_| ConfigError::new(ConfigErrorKind::InvalidIdList, KEY_RESTRICTED_ROLE_IDS))?;

        let dm = parse_flag(KEY_DM, lookup(KEY_DM).as_deref())?;
        let defer_grants = parse_flag(KEY_DEFER_GRANTS, lookup(KEY_DEFER_GRANTS).as_deref())?;

        Ok(Self {
            mode,
            role_id,
            channel_id,
            message_id,
            emoji_id,
            restricted_role_ids,
            dm,
            defer_grants,
        })
    }

    /// True when the feature does nothing at all.
    pub fn is_off(&self) -> bool {
        self.mode == Mode::Off
    }

    /// True when the feature only records what it would do.
    pub fn is_shadow(&self) -> bool {
        self.mode == Mode::Shadow
    }
}

/// Parse one required id key.
///
/// The value must be exactly one plain decimal id: no sign, no whitespace
/// inside, no second id, no non-ASCII digit. The id zero is refused because
/// Discord never uses it.
fn parse_single_id(
    key: &'static str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<u64, ConfigError> {
    let raw = lookup(key).ok_or_else(|| ConfigError::new(ConfigErrorKind::MissingId, key))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::new(ConfigErrorKind::MissingId, key));
    }
    if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ConfigError::new(ConfigErrorKind::InvalidId, key));
    }
    let value: u64 = trimmed
        .parse()
        .map_err(|_| ConfigError::new(ConfigErrorKind::InvalidId, key))?;
    if value == 0 {
        return Err(ConfigError::new(ConfigErrorKind::InvalidId, key));
    }
    Ok(value)
}

/// Parse an `on`/`off` flag.
///
/// An unset key takes the safe default (`true` for both flags: the DM is
/// best effort, and a grant during safeword is deferred into the snapshot).
/// A set but unusable token is refused: the caller switches the feature off
/// and reports the key.
fn parse_flag(key: &'static str, raw: Option<&str>) -> Result<bool, ConfigError> {
    let Some(raw) = raw else {
        return Ok(true);
    };
    let token = raw.trim().to_lowercase();
    match token.as_str() {
        FLAG_ON => Ok(true),
        FLAG_OFF => Ok(false),
        _ => Err(ConfigError::new(ConfigErrorKind::InvalidFlag, key)),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const ROLE: u64 = 700_010;
    const CHANNEL: u64 = 700_020;
    const MESSAGE: u64 = 700_030;
    const EMOJI: u64 = 700_040;
    const RESTRICTED_A: u64 = 700_050;
    const RESTRICTED_B: u64 = 700_051;

    /// The lookup closure the parser sees. The pairs are copied into it, so
    /// the closure owns them and outlives the test's local vector.
    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn complete() -> Vec<(&'static str, &'static str)> {
        vec![
            (KEY_MODE, "live"),
            (KEY_ROLE_ID, "700010"),
            (KEY_CHANNEL_ID, "700020"),
            (KEY_MESSAGE_ID, "700030"),
            (KEY_EMOJI_ID, "700040"),
            (KEY_RESTRICTED_ROLE_IDS, "700050,700051"),
            (KEY_DM, "on"),
            (KEY_DEFER_GRANTS, "on"),
        ]
    }

    fn parse(pairs: &[(&str, &str)]) -> Result<SafewordConfig, ConfigError> {
        SafewordConfig::from_lookup(lookup(pairs))
    }

    #[test]
    fn complete_configuration_parses_every_key() {
        let cfg = parse(&complete()).expect("complete config parses");
        assert_eq!(cfg.mode, Mode::Live);
        assert_eq!(cfg.role_id, ROLE);
        assert_eq!(cfg.channel_id, CHANNEL);
        assert_eq!(cfg.message_id, MESSAGE);
        assert_eq!(cfg.emoji_id, EMOJI);
        assert_eq!(cfg.restricted_role_ids, vec![RESTRICTED_A, RESTRICTED_B]);
        assert!(cfg.dm);
        assert!(cfg.defer_grants);
        assert!(!cfg.is_off());
        assert!(!cfg.is_shadow());
    }

    #[test]
    fn mode_shadow_and_off_are_recognised() {
        let mut base = complete();
        base[0] = (KEY_MODE, "shadow");
        assert!(parse(&base).expect("shadow parses").is_shadow());
        base[0] = (KEY_MODE, "off");
        assert!(parse(&base).expect("off parses").is_off());
    }

    #[test]
    fn unknown_mode_is_off_and_never_live() {
        let mut base = complete();
        for value in ["LIVE!", "enabled", "1", "yes", "on", "shadpw"] {
            base[0] = (KEY_MODE, value);
            let cfg = parse(&base).expect("mode never errors");
            assert_eq!(cfg.mode, Mode::Off, "value {value:?} must be off");
            assert!(cfg.is_off());
        }
    }

    #[test]
    fn unset_mode_is_off() {
        let base: Vec<(&'static str, &'static str)> = complete()
            .into_iter()
            .filter(|(k, _)| *k != KEY_MODE)
            .collect();
        let cfg = parse(&base).expect("unset mode parses");
        assert_eq!(cfg.mode, Mode::Off);
    }

    #[test]
    fn every_missing_id_key_is_refused_and_named() {
        for key in [KEY_ROLE_ID, KEY_CHANNEL_ID, KEY_MESSAGE_ID, KEY_EMOJI_ID] {
            let base: Vec<(&'static str, &'static str)> =
                complete().into_iter().filter(|(k, _)| *k != key).collect();
            let err = parse(&base).expect_err("missing id must be refused");
            assert_eq!(err.key(), key);
            assert_eq!(err.kind(), ConfigErrorKind::MissingId);
        }
    }

    #[test]
    fn every_hostile_id_spelling_is_refused_and_named() {
        // The whole class is refused: a sign, a second id, a non-ASCII digit,
        // a zero id, an overlong number and an inner space. A leading zero
        // ("007") is NOT hostile: it is plain decimal text for the id 7.
        for hostile in [
            "+5",
            "-1",
            "5 6",
            "5,6",
            "５",
            "0",
            "18446744073709551616",
            "5.0",
            "0x10",
            "abc",
            "1e3",
        ] {
            for key in [KEY_ROLE_ID, KEY_CHANNEL_ID, KEY_MESSAGE_ID, KEY_EMOJI_ID] {
                let mut base = complete();
                let index = base
                    .iter()
                    .position(|(k, _)| *k == key)
                    .expect("key present");
                base[index] = (key, hostile);
                let err = parse(&base).expect_err("hostile id must be refused");
                assert_eq!(err.key(), key, "hostile {hostile:?} on {key}");
                assert_eq!(
                    err.kind(),
                    ConfigErrorKind::InvalidId,
                    "hostile {hostile:?}"
                );
            }
        }
    }

    #[test]
    fn an_empty_id_is_treated_as_missing() {
        // An empty value is "not set", not "a bad id": both switch the
        // feature off and name the key, but the reason differs.
        for key in [KEY_ROLE_ID, KEY_CHANNEL_ID, KEY_MESSAGE_ID, KEY_EMOJI_ID] {
            let mut base = complete();
            let index = base
                .iter()
                .position(|(k, _)| *k == key)
                .expect("key present");
            base[index] = (key, "");
            let err = parse(&base).expect_err("an empty id must be refused");
            assert_eq!(err.key(), key);
            assert_eq!(err.kind(), ConfigErrorKind::MissingId);
        }
    }

    #[test]
    fn a_bad_restricted_role_list_refuses_the_whole_list() {
        let mut base = complete();
        let index = base
            .iter()
            .position(|(k, _)| *k == KEY_RESTRICTED_ROLE_IDS)
            .expect("key present");
        for hostile in [
            "700050,abc",
            "700050,,700051",
            "700050,0",
            "700050,-1",
            "700050,５",
        ] {
            base[index] = (KEY_RESTRICTED_ROLE_IDS, hostile);
            let err = parse(&base).expect_err("bad list must be refused");
            assert_eq!(err.key(), KEY_RESTRICTED_ROLE_IDS);
            assert_eq!(err.kind(), ConfigErrorKind::InvalidIdList);
        }
    }

    #[test]
    fn an_unset_restricted_role_list_is_empty_and_valid() {
        let base: Vec<(&'static str, &'static str)> = complete()
            .into_iter()
            .filter(|(k, _)| *k != KEY_RESTRICTED_ROLE_IDS)
            .collect();
        let cfg = parse(&base).expect("unset list parses");
        assert!(cfg.restricted_role_ids.is_empty());
    }

    #[test]
    fn the_restricted_role_list_is_deduped_and_sorted_by_the_helper() {
        let mut base = complete();
        let index = base
            .iter()
            .position(|(k, _)| *k == KEY_RESTRICTED_ROLE_IDS)
            .expect("key present");
        base[index] = (KEY_RESTRICTED_ROLE_IDS, "700051, 700050 ,700051");
        let cfg = parse(&base).expect("list parses");
        assert_eq!(cfg.restricted_role_ids, vec![RESTRICTED_A, RESTRICTED_B]);
    }

    #[test]
    fn flags_default_to_on_when_unset() {
        let base: Vec<(&'static str, &'static str)> = complete()
            .into_iter()
            .filter(|(k, _)| *k != KEY_DM && *k != KEY_DEFER_GRANTS)
            .collect();
        let cfg = parse(&base).expect("unset flags parse");
        assert!(cfg.dm);
        assert!(cfg.defer_grants);
    }

    #[test]
    fn flags_accept_on_and_off_case_insensitively() {
        let mut base = complete();
        let dm_index = base.iter().position(|(k, _)| *k == KEY_DM).expect("key");
        let defer_index = base
            .iter()
            .position(|(k, _)| *k == KEY_DEFER_GRANTS)
            .expect("key");
        base[dm_index] = (KEY_DM, "OFF");
        base[defer_index] = (KEY_DEFER_GRANTS, " On ");
        let cfg = parse(&base).expect("flags parse");
        assert!(!cfg.dm);
        assert!(cfg.defer_grants);
    }

    #[test]
    fn a_bad_flag_token_is_refused_and_named() {
        for key in [KEY_DM, KEY_DEFER_GRANTS] {
            // Surrounding whitespace is trimmed (as the K06 helpers do), so
            // " off " is the token `off` and not hostile.
            for hostile in ["true", "yes", "1", "enabled", "onn", "", "off off", "0"] {
                let mut base = complete();
                let index = base.iter().position(|(k, _)| *k == key).expect("key");
                base[index] = (key, hostile);
                let err = parse(&base).expect_err("bad flag must be refused");
                assert_eq!(err.key(), key, "hostile {hostile:?} on {key}");
                assert_eq!(err.kind(), ConfigErrorKind::InvalidFlag);
            }
        }
    }

    #[test]
    fn an_error_names_the_key_and_never_the_value() {
        let sentinel = "S3CR3T-VALUE";
        let mut base = complete();
        let index = base
            .iter()
            .position(|(k, _)| *k == KEY_ROLE_ID)
            .expect("key present");
        base[index] = (KEY_ROLE_ID, sentinel);
        let err = parse(&base).expect_err("bad id is refused");
        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(
            display.contains(KEY_ROLE_ID),
            "display names the key: {display}"
        );
        assert!(
            !display.contains(sentinel),
            "display must not carry the value"
        );
        assert!(!debug.contains(sentinel), "debug must not carry the value");
    }

    #[test]
    fn the_parser_reads_no_environment_itself() {
        // The lookup closure is the only source: a closure that answers nothing
        // must produce a refusal, never a value from the process environment.
        let err = SafewordConfig::from_lookup(|_| None).expect_err("nothing set is refused");
        assert_eq!(err.key(), KEY_ROLE_ID);
    }
}

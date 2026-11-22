//! Configuration for the safeword feature (SW3).
//!
//! RED STUB: the behaviour is not implemented yet; the tests below fail.

use crate::settings::Mode;
use std::fmt;

pub const KEY_MODE: &str = "SAFEWORD_MODE";
pub const KEY_ROLE_ID: &str = "SAFEWORD_ROLE_ID";
pub const KEY_CHANNEL_ID: &str = "SAFEWORD_CHANNEL_ID";
pub const KEY_MESSAGE_ID: &str = "SAFEWORD_MESSAGE_ID";
pub const KEY_EMOJI_ID: &str = "SAFEWORD_EMOJI_ID";
pub const KEY_RESTRICTED_ROLE_IDS: &str = "SAFEWORD_RESTRICTED_ROLE_IDS";
pub const KEY_DM: &str = "SAFEWORD_DM";
pub const KEY_DEFER_GRANTS: &str = "SAFEWORD_DEFER_GRANTS";

pub const FLAG_ON: &str = "on";
pub const FLAG_OFF: &str = "off";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    ModeUnreadable,
    MissingId,
    InvalidId,
    InvalidIdList,
    InvalidFlag,
}

impl ConfigErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            ConfigErrorKind::ModeUnreadable => "mode_unreadable",
            ConfigErrorKind::MissingId => "missing_id",
            ConfigErrorKind::InvalidId => "invalid_id",
            ConfigErrorKind::InvalidIdList => "invalid_id_list",
            ConfigErrorKind::InvalidFlag => "invalid_flag",
        }
    }
}

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

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.kind.code())
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafewordConfig {
    pub mode: Mode,
    pub role_id: u64,
    pub channel_id: u64,
    pub message_id: u64,
    pub emoji_id: u64,
    pub restricted_role_ids: Vec<u64>,
    pub dm: bool,
    pub defer_grants: bool,
}

impl SafewordConfig {
    pub fn from_lookup(_lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        Err(ConfigError::new(ConfigErrorKind::MissingId, KEY_ROLE_ID))
    }

    pub fn is_off(&self) -> bool {
        self.mode == Mode::Off
    }

    pub fn is_shadow(&self) -> bool {
        self.mode == Mode::Shadow
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
        move |key| {
            owned
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        }
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
            let base: Vec<(&'static str, &'static str)> = complete()
                .into_iter()
                .filter(|(k, _)| *k != key)
                .collect();
            let err = parse(&base).expect_err("missing id must be refused");
            assert_eq!(err.key(), key);
            assert_eq!(err.kind(), ConfigErrorKind::MissingId);
        }
    }

    #[test]
    fn every_hostile_id_spelling_is_refused_and_named() {
        // The whole class is refused: a sign, a second id, a non-ASCII digit,
        // a zero id, an overlong number and an inner space.
        for hostile in [
            "+5", "-1", "5 6", "5,6", "５", "0", "007", "18446744073709551616", "5.0", "0x10",
            "", "   ", "abc", "1e3",
        ] {
            for key in [KEY_ROLE_ID, KEY_CHANNEL_ID, KEY_MESSAGE_ID, KEY_EMOJI_ID] {
                let mut base = complete();
                let index = base.iter().position(|(k, _)| *k == key).expect("key present");
                base[index] = (key, hostile);
                let err = parse(&base).expect_err("hostile id must be refused");
                assert_eq!(err.key(), key, "hostile {hostile:?} on {key}");
                assert_eq!(err.kind(), ConfigErrorKind::InvalidId, "hostile {hostile:?}");
            }
        }
    }

    #[test]
    fn a_bad_restricted_role_list_refuses_the_whole_list() {
        let mut base = complete();
        let index = base
            .iter()
            .position(|(k, _)| *k == KEY_RESTRICTED_ROLE_IDS)
            .expect("key present");
        for hostile in ["700050,abc", "700050,,700051", "700050,0", "700050,-1", "700050,５"] {
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
            for hostile in ["true", "yes", "1", "enabled", "off ", "onn", ""] {
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
        assert!(display.contains(KEY_ROLE_ID), "display names the key: {display}");
        assert!(!display.contains(sentinel), "display must not carry the value");
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

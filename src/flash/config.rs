//! Flash-media configuration parser (`docs/verification/FLASH-RUNTIME.md`).
//!
//! Reads the five `FLASH_*` keys from a lookup closure (no `std::env` read here) and
//! parses them only through the K06 helpers in [`crate::settings`], with one exception
//! the doc names: K06 has no boolean helper, so `FLASH_INCLUDE_THREADS` has a strict
//! local parser. A bad value switches the feature off and the error names the KEY only;
//! the value is never carried or echoed.

use super::policy::{self, FlashConfig};
use crate::settings::Mode;

/// Environment key holding the feature mode (`off`, `shadow`, `live`).
pub const KEY_MODE: &str = "FLASH_MODE";

/// Stable reason codes for a refused setting. They never carry the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashConfigErrorKind {
    /// The value could not be read (not a list of ids, not a number, not `true`/`false`).
    Unreadable,
    /// The value was read but is outside what the policy accepts.
    OutOfRange,
}

/// A refused setting: the key and a stable code, never the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashConfigError {
    key: &'static str,
    kind: FlashConfigErrorKind,
}

impl FlashConfigError {
    /// The configuration key that was refused.
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// The stable reason code.
    pub fn kind(&self) -> FlashConfigErrorKind {
        self.kind
    }
}

impl std::fmt::Display for FlashConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why = match self.kind {
            FlashConfigErrorKind::Unreadable => "value could not be read",
            FlashConfigErrorKind::OutOfRange => "value out of range",
        };
        write!(f, "{}: {why}", self.key)
    }
}

impl std::error::Error for FlashConfigError {}

/// The parsed settings of the flash runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashSettings {
    mode: Mode,
    policy: FlashConfig,
}

impl FlashSettings {
    /// The configured mode. `Off` whenever a setting was refused.
    pub fn mode(&self) -> Mode {
        let _ = self.mode;
        unimplemented!("FL2 RED stub")
    }

    /// The validated policy configuration.
    pub fn policy(&self) -> &FlashConfig {
        &self.policy
    }

    /// True when the feature should run at all: a mode other than `Off` and at least
    /// one flash channel.
    pub fn is_active(&self) -> bool {
        unimplemented!("FL2 RED stub")
    }
}

/// The result of [`load`]: the settings to use and the refusal that caused `Off`, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The settings; mode `Off` and no channels when `error` is set.
    pub settings: FlashSettings,
    /// The first refused setting, naming only its key.
    pub error: Option<FlashConfigError>,
}

/// Parses the flash keys from `lookup` (blank counts as unset). `guild_id` is the
/// already-parsed `GUILD_ID` the policy scopes to.
pub fn load(lookup: impl Fn(&str) -> Option<String>, guild_id: Option<u64>) -> Loaded {
    let _ = (&lookup, guild_id, policy::DEFAULT_DELAY_SECS);
    unimplemented!("FL2 RED stub")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    // Synthetic ids, far from any real Discord snowflake.
    const CH_A: u64 = 4_100_000_000_000_001;
    const CH_B: u64 = 4_100_000_000_000_002;
    const ROLE: u64 = 4_100_000_000_000_003;
    const GUILD: u64 = 4_100_000_000_000_004;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn live(extra: &[(&str, &str)]) -> Loaded {
        let mut pairs: Vec<(&str, &str)> = vec![
            (KEY_MODE, "live"),
            (policy::KEY_CHANNEL_IDS, "4100000000000001"),
        ];
        pairs.extend_from_slice(extra);
        // Later pairs must win, so search from the back.
        load(
            |key| {
                pairs
                    .iter()
                    .rev()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| (*v).to_string())
            },
            None,
        )
    }

    #[test]
    fn nothing_set_is_off_with_no_error() {
        let loaded = load(env(&[]), None);
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.settings.mode(), Mode::Off);
        assert!(!loaded.settings.is_active());
        assert!(!loaded.settings.policy().is_enabled());
    }

    #[test]
    fn a_full_valid_live_configuration_is_parsed_through_the_policy() {
        let loaded = load(
            env(&[
                (KEY_MODE, "live"),
                (
                    policy::KEY_CHANNEL_IDS,
                    "4100000000000002, 4100000000000001",
                ),
                (policy::KEY_PING_ROLE_ID, "4100000000000003"),
                (policy::KEY_DELAY_SECS, "300"),
                (policy::KEY_INCLUDE_THREADS, "false"),
            ]),
            Some(GUILD),
        );
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.settings.mode(), Mode::Live);
        assert!(loaded.settings.is_active());
        let p = loaded.settings.policy();
        assert_eq!(p.channel_ids(), &[CH_A, CH_B]);
        assert_eq!(p.ping_role_id(), Some(ROLE));
        assert_eq!(p.delay_secs(), 300);
        assert!(!p.include_threads());
        assert_eq!(p.guild_id(), Some(GUILD));
    }

    #[test]
    fn defaults_are_180_seconds_threads_included_and_no_role() {
        let loaded = live(&[]);
        assert_eq!(loaded.error, None);
        let p = loaded.settings.policy();
        assert_eq!(p.delay_secs(), 180);
        assert!(p.include_threads());
        assert_eq!(p.ping_role_id(), None);
        assert_eq!(p.guild_id(), None);
    }

    #[test]
    fn shadow_is_parsed_and_is_active() {
        let loaded = load(
            env(&[
                (KEY_MODE, "shadow"),
                (policy::KEY_CHANNEL_IDS, "4100000000000001"),
            ]),
            None,
        );
        assert_eq!(loaded.settings.mode(), Mode::Shadow);
        assert!(loaded.settings.is_active());
    }

    #[test]
    fn live_without_channels_is_not_active() {
        let loaded = load(env(&[(KEY_MODE, "live")]), None);
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.settings.mode(), Mode::Live);
        assert!(!loaded.settings.is_active());
    }

    #[test]
    fn a_mode_typo_is_off_never_live() {
        for typo in ["LIVEE", "true", "1", "yes", "shado", ""] {
            let loaded = load(
                env(&[
                    (KEY_MODE, typo),
                    (policy::KEY_CHANNEL_IDS, "4100000000000001"),
                ]),
                None,
            );
            assert_eq!(loaded.settings.mode(), Mode::Off, "mode {typo:?}");
            assert!(!loaded.settings.is_active(), "mode {typo:?}");
        }
    }

    #[test]
    fn whitespace_and_case_are_tolerated_in_good_values() {
        let loaded = live(&[
            (KEY_MODE, "  LiVe "),
            (policy::KEY_DELAY_SECS, " 60 "),
            (policy::KEY_INCLUDE_THREADS, " FALSE "),
            (policy::KEY_PING_ROLE_ID, " 4100000000000003 "),
        ]);
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.settings.mode(), Mode::Live);
        assert_eq!(loaded.settings.policy().delay_secs(), 60);
        assert!(!loaded.settings.policy().include_threads());
        assert_eq!(loaded.settings.policy().ping_role_id(), Some(ROLE));
    }

    #[test]
    fn blank_optional_values_count_as_unset() {
        let loaded = live(&[
            (policy::KEY_DELAY_SECS, "  "),
            (policy::KEY_INCLUDE_THREADS, ""),
            (policy::KEY_PING_ROLE_ID, " "),
        ]);
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.settings.policy().delay_secs(), 180);
        assert!(loaded.settings.policy().include_threads());
        assert_eq!(loaded.settings.policy().ping_role_id(), None);
    }

    /// Every bad value switches the whole feature off (even with `live` set) and the
    /// error names exactly the offending key and does not carry the value.
    #[test]
    fn a_bad_value_switches_the_feature_off_and_names_only_the_key() {
        let cases: [(&str, &str); 19] = [
            (policy::KEY_CHANNEL_IDS, "s3cretvalue9"),
            (
                policy::KEY_CHANNEL_IDS,
                "4100000000000001,,4100000000000002",
            ),
            (policy::KEY_CHANNEL_IDS, "0"),
            (policy::KEY_CHANNEL_IDS, "-5"),
            (policy::KEY_CHANNEL_IDS, "１２３"),
            (policy::KEY_PING_ROLE_ID, "s3cretvalue9"),
            (policy::KEY_PING_ROLE_ID, "0"),
            (
                policy::KEY_PING_ROLE_ID,
                "4100000000000003,4100000000000005",
            ),
            (policy::KEY_DELAY_SECS, "s3cretvalue9"),
            (policy::KEY_DELAY_SECS, "9"),
            (policy::KEY_DELAY_SECS, "86401"),
            (policy::KEY_DELAY_SECS, "-180"),
            (policy::KEY_DELAY_SECS, "+180"),
            (policy::KEY_DELAY_SECS, "１８０"),
            (policy::KEY_DELAY_SECS, "18446744073709551616"),
            (policy::KEY_INCLUDE_THREADS, "s3cretvalue9"),
            (policy::KEY_INCLUDE_THREADS, "1"),
            (policy::KEY_INCLUDE_THREADS, "yes"),
            (policy::KEY_INCLUDE_THREADS, "truee"),
        ];
        for (key, value) in cases {
            let loaded = live(&[(key, value)]);
            let err = loaded
                .error
                .unwrap_or_else(|| panic!("{key}={value:?} must be refused"));
            assert_eq!(err.key(), key, "value {value:?}");
            assert_eq!(loaded.settings.mode(), Mode::Off, "{key}={value:?}");
            assert!(!loaded.settings.is_active(), "{key}={value:?}");
            let shown = format!("{err} {err:?}");
            assert!(shown.contains(key));
            for needle in ["s3cretvalue9", value] {
                if needle.len() > 2 {
                    assert!(!shown.contains(needle), "error echoes the value: {shown}");
                }
            }
        }
    }

    #[test]
    fn range_and_syntax_failures_have_distinct_stable_kinds() {
        let range = live(&[(policy::KEY_DELAY_SECS, "86401")]).error.unwrap();
        assert_eq!(range.kind(), FlashConfigErrorKind::OutOfRange);
        let syntax = live(&[(policy::KEY_DELAY_SECS, "abc")]).error.unwrap();
        assert_eq!(syntax.kind(), FlashConfigErrorKind::Unreadable);
    }

    #[test]
    fn delay_bounds_are_inclusive() {
        for ok in ["10", "86400"] {
            assert_eq!(live(&[(policy::KEY_DELAY_SECS, ok)]).error, None, "{ok}");
        }
        for bad in ["9", "86401", "0"] {
            assert!(
                live(&[(policy::KEY_DELAY_SECS, bad)]).error.is_some(),
                "{bad}"
            );
        }
    }

    #[test]
    fn an_unusable_guild_id_is_refused_by_the_policy_and_names_its_key() {
        let loaded = load(
            env(&[
                (KEY_MODE, "live"),
                (policy::KEY_CHANNEL_IDS, "4100000000000001"),
            ]),
            Some(0),
        );
        let err = loaded.error.expect("guild id zero must be refused");
        assert_eq!(err.key(), "GUILD_ID");
        assert_eq!(loaded.settings.mode(), Mode::Off);
    }

    /// The parser reads the five flash keys from the closure and nothing else.
    #[test]
    fn only_the_five_flash_keys_are_read_from_the_lookup() {
        let asked = RefCell::new(BTreeSet::new());
        let _ = load(
            |key| {
                asked.borrow_mut().insert(key.to_string());
                None
            },
            None,
        );
        let expected: BTreeSet<String> = [
            KEY_MODE,
            policy::KEY_CHANNEL_IDS,
            policy::KEY_PING_ROLE_ID,
            policy::KEY_DELAY_SECS,
            policy::KEY_INCLUDE_THREADS,
        ]
        .iter()
        .map(|k| k.to_string())
        .collect();
        assert_eq!(*asked.borrow(), expected);
    }

    /// With the mode off nothing else is validated into an error: a broken value under
    /// `FLASH_MODE=off` still names its key (the operator must hear about it), but the
    /// feature stays off either way.
    #[test]
    fn a_bad_value_under_mode_off_is_still_reported() {
        let loaded = load(
            env(&[(KEY_MODE, "off"), (policy::KEY_DELAY_SECS, "abc")]),
            None,
        );
        assert_eq!(loaded.settings.mode(), Mode::Off);
        assert_eq!(loaded.error.map(|e| e.key()), Some(policy::KEY_DELAY_SECS));
    }
}

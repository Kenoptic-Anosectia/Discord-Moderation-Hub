//! Settings of the `/embed` command (`docs/verification/EMBED-COMMAND.md`).
//!
//! The parser takes a lookup closure and reads no environment variable. Every value goes
//! through the K06 helper `settings::parse_mode` where one fits; the two keys that are not
//! a mode, an id list or a duration are parsed here by the same rule: parse, then accept
//! only the plain spellings. A bad value switches the feature OFF and the error names the
//! KEY, never the value.

use std::fmt;
use std::path::PathBuf;

use crate::settings::{parse_mode, Mode};

/// Key of the feature mode (`off`, `shadow`, `live`).
pub const MODE_KEY: &str = "EMBED_MODE";
/// Key of the directory that holds the template files.
pub const TEMPLATE_DIR_KEY: &str = "EMBED_TEMPLATE_DIR";
/// Key that says who may publish (`owner`, the default, or `staff`).
pub const PUBLISHERS_KEY: &str = "EMBED_PUBLISHERS";
/// Longest accepted template directory setting, in bytes.
pub const MAX_TEMPLATE_DIR_BYTES: usize = 1024;

/// Who may use `/embed post|sync|list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publishers {
    /// Only the guild owner (the default).
    OwnerOnly,
    /// The guild owner and staff.
    Staff,
}

/// The parsed settings of the feature.
#[derive(Clone, PartialEq, Eq)]
pub struct EmbedConfig {
    /// Off, shadow (no sends, edits or deletes) or live.
    pub mode: Mode,
    /// Directory the template files live in. `None` means "not configured".
    pub template_dir: Option<PathBuf>,
    /// Who may publish.
    pub publishers: Publishers,
}

impl fmt::Debug for EmbedConfig {
    // The directory is deliberately not printed: a path is server-specific.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmbedConfig")
            .field("mode", &self.mode)
            .field("template_dir_set", &self.template_dir.is_some())
            .field("publishers", &self.publishers)
            .finish()
    }
}

impl EmbedConfig {
    /// The configuration of a feature that is switched off.
    pub fn off() -> Self {
        EmbedConfig {
            mode: Mode::Off,
            template_dir: None,
            publishers: Publishers::OwnerOnly,
        }
    }

    /// True when the feature may run: a mode other than off AND a template directory.
    pub fn enabled(&self) -> bool {
        self.mode != Mode::Off && self.template_dir.is_some()
    }
}

/// The keys whose value was bad. It never carries a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The names of the keys that were bad, in a fixed order.
    pub keys: Vec<&'static str>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "embeds: bad setting: {}", self.keys.join(", "))
    }
}

impl std::error::Error for ConfigError {}

/// Parses the settings from a lookup closure.
pub fn parse(lookup: impl Fn(&str) -> Option<String>) -> Result<EmbedConfig, ConfigError> {
    let mut bad: Vec<&'static str> = Vec::new();
    let mode = match parse_mode(MODE_KEY, lookup(MODE_KEY).as_deref()) {
        Ok(mode) => mode,
        Err(_) => {
            bad.push(MODE_KEY);
            Mode::Off
        }
    };
    let template_dir = match parse_template_dir(lookup(TEMPLATE_DIR_KEY).as_deref()) {
        Ok(dir) => dir,
        Err(()) => {
            bad.push(TEMPLATE_DIR_KEY);
            None
        }
    };
    let publishers = match parse_publishers(lookup(PUBLISHERS_KEY).as_deref()) {
        Ok(publishers) => publishers,
        Err(()) => {
            bad.push(PUBLISHERS_KEY);
            Publishers::OwnerOnly
        }
    };
    // A mode that is on needs somewhere to read templates from.
    if bad.is_empty() && mode != Mode::Off && template_dir.is_none() {
        bad.push(TEMPLATE_DIR_KEY);
    }
    if bad.is_empty() {
        Ok(EmbedConfig {
            mode,
            template_dir,
            publishers,
        })
    } else {
        Err(ConfigError { keys: bad })
    }
}

/// The template directory: blank is "not set"; a control character or an over-long value
/// is refused. No file system access happens here.
fn parse_template_dir(raw: Option<&str>) -> Result<Option<PathBuf>, ()> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() > MAX_TEMPLATE_DIR_BYTES || trimmed.chars().any(char::is_control) {
        return Err(());
    }
    Ok(Some(PathBuf::from(trimmed)))
}

/// `owner` (default) or `staff`, ASCII case-insensitive. Anything else is refused, so a
/// typo never widens who may publish.
fn parse_publishers(raw: Option<&str>) -> Result<Publishers, ()> {
    let Some(raw) = raw else {
        return Ok(Publishers::OwnerOnly);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("owner") {
        Ok(Publishers::OwnerOnly)
    } else if trimmed.eq_ignore_ascii_case("staff") {
        Ok(Publishers::Staff)
    } else {
        Err(())
    }
}

/// Parses the settings; a bad value gives the OFF configuration and the error.
pub fn parse_or_off(lookup: impl Fn(&str) -> Option<String>) -> (EmbedConfig, Option<ConfigError>) {
    match parse(lookup) {
        Ok(config) => (config, None),
        Err(error) => (EmbedConfig::off(), Some(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn lookup_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn nothing_set_is_off_owner_only_and_no_error() {
        let (config, error) = parse_or_off(lookup_of(&[]));
        assert_eq!(config.mode, Mode::Off);
        assert_eq!(config.publishers, Publishers::OwnerOnly);
        assert!(config.template_dir.is_none());
        assert!(!config.enabled());
        assert!(error.is_none());
    }

    #[test]
    fn shadow_and_live_with_a_directory_are_enabled() {
        for (raw, mode) in [
            ("shadow", Mode::Shadow),
            ("LIVE", Mode::Live),
            (" live ", Mode::Live),
        ] {
            let config = parse(lookup_of(&[(MODE_KEY, raw), (TEMPLATE_DIR_KEY, "tpl")])).unwrap();
            assert_eq!(config.mode, mode, "{raw:?}");
            assert_eq!(config.template_dir, Some(PathBuf::from("tpl")));
            assert!(config.enabled());
            assert_eq!(config.publishers, Publishers::OwnerOnly);
        }
    }

    #[test]
    fn a_mode_typo_is_off_not_live() {
        for raw in ["true", "1", "yes", "lve", "on"] {
            let (config, error) =
                parse_or_off(lookup_of(&[(MODE_KEY, raw), (TEMPLATE_DIR_KEY, "tpl")]));
            assert_eq!(config.mode, Mode::Off, "{raw:?}");
            assert!(!config.enabled());
            assert!(error.is_none(), "a typo in the mode is simply off: {raw:?}");
        }
    }

    #[test]
    fn a_mode_without_a_directory_is_off_and_names_the_directory_key() {
        for mode in ["shadow", "live"] {
            for dir in [None, Some(""), Some("   ")] {
                let mut pairs = vec![(MODE_KEY, mode)];
                if let Some(dir) = dir {
                    pairs.push((TEMPLATE_DIR_KEY, dir));
                }
                let (config, error) = parse_or_off(lookup_of(&pairs));
                assert!(!config.enabled(), "{mode} {dir:?}");
                assert_eq!(config.mode, Mode::Off);
                assert_eq!(error.unwrap().keys, vec![TEMPLATE_DIR_KEY]);
            }
        }
    }

    #[test]
    fn publishers_staff_widens_and_owner_is_the_default() {
        for (raw, expected) in [
            (None, Publishers::OwnerOnly),
            (Some(""), Publishers::OwnerOnly),
            (Some("  "), Publishers::OwnerOnly),
            (Some("owner"), Publishers::OwnerOnly),
            (Some("OWNER"), Publishers::OwnerOnly),
            (Some("staff"), Publishers::Staff),
            (Some(" Staff "), Publishers::Staff),
        ] {
            let mut pairs = vec![(MODE_KEY, "live"), (TEMPLATE_DIR_KEY, "tpl")];
            if let Some(raw) = raw {
                pairs.push((PUBLISHERS_KEY, raw));
            }
            let config = parse(lookup_of(&pairs)).unwrap();
            assert_eq!(config.publishers, expected, "{raw:?}");
        }
    }

    #[test]
    fn a_bad_publishers_value_switches_the_feature_off_and_never_widens() {
        for raw in [
            "staf",
            "all",
            "everyone",
            "staff,owner",
            "1",
            "true",
            "owner staff",
            "stäff",
            "staff\0",
            "ｓtaff",
        ] {
            let (config, error) = parse_or_off(lookup_of(&[
                (MODE_KEY, "live"),
                (TEMPLATE_DIR_KEY, "tpl"),
                (PUBLISHERS_KEY, raw),
            ]));
            assert!(!config.enabled(), "{raw:?}");
            assert_eq!(config.mode, Mode::Off, "{raw:?}");
            assert_eq!(config.publishers, Publishers::OwnerOnly, "{raw:?}");
            assert_eq!(error.unwrap().keys, vec![PUBLISHERS_KEY], "{raw:?}");
        }
    }

    #[test]
    fn the_template_directory_boundaries() {
        let exact = "d".repeat(MAX_TEMPLATE_DIR_BYTES);
        let over = "d".repeat(MAX_TEMPLATE_DIR_BYTES + 1);
        let ok = parse(lookup_of(&[(MODE_KEY, "live"), (TEMPLATE_DIR_KEY, &exact)])).unwrap();
        assert_eq!(ok.template_dir, Some(PathBuf::from(&exact)));
        let (config, error) =
            parse_or_off(lookup_of(&[(MODE_KEY, "live"), (TEMPLATE_DIR_KEY, &over)]));
        assert!(!config.enabled());
        assert_eq!(error.unwrap().keys, vec![TEMPLATE_DIR_KEY]);
        // Surrounding whitespace is trimmed, an inner control character is refused.
        let trimmed = parse(lookup_of(&[
            (MODE_KEY, "live"),
            (TEMPLATE_DIR_KEY, "  tpl  "),
        ]))
        .unwrap();
        assert_eq!(trimmed.template_dir, Some(PathBuf::from("tpl")));
        for bad in ["tp\0l", "tp\nl", "tp\tl", "tp\u{7f}l", "tp\u{85}l"] {
            let (config, error) =
                parse_or_off(lookup_of(&[(MODE_KEY, "live"), (TEMPLATE_DIR_KEY, bad)]));
            assert!(!config.enabled(), "{bad:?}");
            assert_eq!(error.unwrap().keys, vec![TEMPLATE_DIR_KEY], "{bad:?}");
        }
    }

    #[test]
    fn a_bad_directory_is_reported_even_while_the_mode_is_off() {
        let (config, error) = parse_or_off(lookup_of(&[(TEMPLATE_DIR_KEY, "a\0b")]));
        assert!(!config.enabled());
        assert_eq!(error.unwrap().keys, vec![TEMPLATE_DIR_KEY]);
    }

    #[test]
    fn several_bad_keys_are_all_named_in_a_fixed_order() {
        let error = parse(lookup_of(&[
            (MODE_KEY, "live"),
            (TEMPLATE_DIR_KEY, "a\0b"),
            (PUBLISHERS_KEY, "nope"),
        ]))
        .unwrap_err();
        assert_eq!(error.keys, vec![TEMPLATE_DIR_KEY, PUBLISHERS_KEY]);
    }

    #[test]
    fn no_error_or_debug_output_carries_a_value() {
        let secret = "S3CR3T-VALUE";
        let (config, error) = parse_or_off(lookup_of(&[
            (MODE_KEY, "live"),
            (TEMPLATE_DIR_KEY, secret),
            (PUBLISHERS_KEY, secret),
        ]));
        let error = error.unwrap();
        for text in [
            format!("{error}"),
            format!("{error:?}"),
            format!("{config:?}"),
        ] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(format!("{error}").contains(PUBLISHERS_KEY));
        // A good configuration does not print its directory either.
        let good = parse(lookup_of(&[(MODE_KEY, "live"), (TEMPLATE_DIR_KEY, secret)])).unwrap();
        assert!(!format!("{good:?}").contains(secret));
    }

    #[test]
    fn only_the_three_documented_keys_are_read() {
        let asked = RefCell::new(Vec::<String>::new());
        let _ = parse(|key| {
            asked.borrow_mut().push(key.to_string());
            None
        });
        let mut asked = asked.into_inner();
        asked.sort();
        asked.dedup();
        let mut expected: Vec<String> = [TEMPLATE_DIR_KEY, MODE_KEY, PUBLISHERS_KEY]
            .iter()
            .map(|key| key.to_string())
            .collect();
        expected.sort();
        assert_eq!(asked, expected);
    }
}

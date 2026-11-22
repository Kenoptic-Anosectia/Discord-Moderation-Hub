//! Configuration of the welcome feature (`docs/verification/WELCOME-RUNTIME.md`).
//!
//! The parser reads nothing but a lookup closure, so it is testable offline and
//! reads no environment variable of its own. Every value goes through the K06
//! helpers in [`crate::settings`]; a value that cannot be used switches the
//! feature OFF and the key is named - never the value.
//!
//! Wiring card W3 stages the keys in `.env.example` and calls [`parse`].

use crate::settings::{self, Mode};
use std::ffi::OsString;
use std::fmt;

/// Environment key holding the mode of the welcome feature.
pub const KEY_MODE: &str = "WELCOME_MODE";
/// Environment key holding the id of the channel the welcome is posted in.
pub const KEY_CHANNEL_ID: &str = "WELCOME_CHANNEL_ID";
/// Environment key holding the path of the welcome template file.
pub const KEY_TEMPLATE_PATH: &str = "WELCOME_TEMPLATE_PATH";
/// Environment key holding the comma separated list of join role ids.
pub const KEY_JOIN_ROLE_IDS: &str = "JOIN_ROLE_IDS";

/// The configuration of the welcome feature.
///
/// A bad value switches the feature off: the mode becomes [`Mode::Off`] and the
/// key is listed in [`WelcomeConfig::invalid_keys`]. The value itself is never
/// kept anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeConfig {
    /// The mode the feature runs in. `Off` unless every value was usable.
    pub mode: Mode,
    /// The channel the welcome is posted in.
    pub channel_id: Option<u64>,
    /// The template file the welcome is rendered from.
    pub template_path: Option<String>,
    /// The roles granted to a joining member.
    pub join_role_ids: Vec<u64>,
    /// The keys that could not be used, in the order they were read.
    pub invalid_keys: Vec<&'static str>,
}

/// Whether the feature may do anything at all.
///
/// The feature is off unless a mode was requested and the channel and the
/// template are known: without a channel there is nowhere to post and without a
/// template there is nothing to post.
impl WelcomeConfig {
    pub fn is_enabled(&self) -> bool {
        self.mode != Mode::Off && self.channel_id.is_some() && self.template_path.is_some()
    }
}

/// Reads one key as text.
///
/// A value that is not valid Unicode is not usable: the caller reports the key
/// as invalid. The value itself is never kept.
fn read_text(lookup: &impl Fn(&str) -> Option<OsString>, key: &str) -> Option<String> {
    lookup(key).and_then(|raw| raw.into_string().ok())
}

/// Parses the four keys of the welcome feature.
///
/// The lookup closure is the only source of values; this module reads no
/// environment variable itself.
///
/// A value that is not valid Unicode is not usable: the key is named as
/// invalid. The value itself is never kept.
pub fn parse(lookup: impl Fn(&str) -> Option<OsString>) -> WelcomeConfig {
    let mut invalid_keys: Vec<&'static str> = Vec::new();

    let mode = settings::parse_mode(KEY_MODE, read_text(&lookup, KEY_MODE).as_deref())
        .unwrap_or(Mode::Off);

    let channel_id = match read_text(&lookup, KEY_CHANNEL_ID) {
        None => None,
        Some(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                match settings::parse_id_list(KEY_CHANNEL_ID, Some(trimmed)) {
                    // The channel is ONE id: a list is not a channel.
                    Ok(ids) if ids.len() == 1 => Some(ids[0]),
                    _ => {
                        invalid_keys.push(KEY_CHANNEL_ID);
                        None
                    }
                }
            }
        }
    };

    let template_path = match read_text(&lookup, KEY_TEMPLATE_PATH) {
        None => None,
        Some(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                // The template name uses the one path-safety rule the repo
                // already has (EM1): ONE canonical spelling that can never
                // walk out of the template directory. A path with a separator,
                // a `..`, a drive letter or a URI scheme is refused as a
                // class, not one spelling at a time.
                match crate::embeds::posts::validate_key(trimmed) {
                    Ok(()) => Some(trimmed.to_string()),
                    Err(_) => {
                        invalid_keys.push(KEY_TEMPLATE_PATH);
                        None
                    }
                }
            }
        }
    };

    let join_role_ids = match settings::parse_id_list(
        KEY_JOIN_ROLE_IDS,
        read_text(&lookup, KEY_JOIN_ROLE_IDS).as_deref(),
    ) {
        Ok(ids) => ids,
        Err(_) => {
            invalid_keys.push(KEY_JOIN_ROLE_IDS);
            Vec::new()
        }
    };

    // A key that could not be used switches the whole feature off.
    let mode = if invalid_keys.is_empty() {
        mode
    } else {
        Mode::Off
    };

    WelcomeConfig {
        mode,
        channel_id,
        template_path,
        join_role_ids,
        invalid_keys,
    }
}

impl fmt::Display for WelcomeConfig {
    /// One line with states and counts only: no id, no path, no value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Welcome feature: mode={} channel={} template={} join_roles={} invalid_keys={}",
            match self.mode {
                Mode::Off => "off",
                Mode::Shadow => "shadow",
                Mode::Live => "live",
            },
            if self.channel_id.is_some() {
                "set"
            } else {
                "unset"
            },
            if self.template_path.is_some() {
                "set"
            } else {
                "unset"
            },
            self.join_role_ids.len(),
            self.invalid_keys.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Synthetic ids. They are obviously made up and are not real Discord ids.
    const CHANNEL: u64 = 810_001;
    const ROLE_A: u64 = 820_001;
    const ROLE_B: u64 = 820_002;

    /// A lookup over a fixed table of values.
    fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).map(|value| OsString::from(value.as_str()))
    }

    /// The four keys set to usable values.
    fn complete() -> Vec<(&'static str, &'static str)> {
        vec![
            (KEY_MODE, "live"),
            (KEY_CHANNEL_ID, "810001"),
            (KEY_TEMPLATE_PATH, "welcome"),
            (KEY_JOIN_ROLE_IDS, "820001,820002"),
        ]
    }

    fn parse_pairs(pairs: &[(&str, &str)]) -> WelcomeConfig {
        parse(lookup_from(pairs))
    }

    // ---- the empty configuration -------------------------------------------

    #[test]
    fn with_no_keys_at_all_the_feature_is_off_and_nothing_is_invalid() {
        let config = parse_pairs(&[]);
        assert_eq!(config.mode, Mode::Off);
        assert_eq!(config.channel_id, None);
        assert_eq!(config.template_path, None);
        assert!(config.join_role_ids.is_empty());
        assert!(config.invalid_keys.is_empty());
        assert!(!config.is_enabled());
    }

    #[test]
    fn a_blank_value_counts_as_unset_and_is_not_an_error() {
        for blank in ["", "   ", "\t"] {
            let config = parse_pairs(&[
                (KEY_MODE, "live"),
                (KEY_CHANNEL_ID, blank),
                (KEY_TEMPLATE_PATH, blank),
                (KEY_JOIN_ROLE_IDS, blank),
            ]);
            assert_eq!(config.channel_id, None, "{blank:?}");
            assert_eq!(config.template_path, None, "{blank:?}");
            assert!(config.join_role_ids.is_empty(), "{blank:?}");
            assert!(config.invalid_keys.is_empty(), "{blank:?}");
        }
    }

    // ---- the usable configuration -------------------------------------------

    #[test]
    fn the_four_keys_are_parsed_through_the_k06_helpers() {
        let config = parse_pairs(&complete());
        assert_eq!(config.mode, Mode::Live);
        assert_eq!(config.channel_id, Some(CHANNEL));
        assert_eq!(config.template_path.as_deref(), Some("welcome"));
        assert_eq!(config.join_role_ids, vec![ROLE_A, ROLE_B]);
        assert!(config.invalid_keys.is_empty());
        assert!(config.is_enabled());
    }

    #[test]
    fn the_join_role_list_is_deduplicated_and_sorted_by_the_k06_helper() {
        let config = parse_pairs(&[
            (KEY_MODE, "live"),
            (KEY_CHANNEL_ID, "810001"),
            (KEY_TEMPLATE_PATH, "welcome"),
            (KEY_JOIN_ROLE_IDS, "820002, 820001,820002"),
        ]);
        assert_eq!(config.join_role_ids, vec![ROLE_A, ROLE_B]);
    }

    #[test]
    fn shadow_is_a_mode_of_its_own() {
        let mut pairs = complete();
        pairs[0] = (KEY_MODE, "shadow");
        let config = parse_pairs(&pairs);
        assert_eq!(config.mode, Mode::Shadow);
        assert!(config.is_enabled());
    }

    #[test]
    fn an_unknown_mode_is_off_and_is_not_an_invalid_key() {
        let mut pairs = complete();
        pairs[0] = (KEY_MODE, "lively");
        let config = parse_pairs(&pairs);
        assert_eq!(config.mode, Mode::Off);
        // The K06 helper reads an unknown mode as off, not as a broken key.
        assert!(config.invalid_keys.is_empty());
        assert!(!config.is_enabled());
    }

    // ---- a bad value switches the feature off -------------------------------

    #[test]
    fn a_bad_channel_id_switches_the_feature_off_and_names_the_key() {
        for bad in ["abc", "0", "810001,810002", "-1", "8_1"] {
            let config = parse_pairs(&[
                (KEY_MODE, "live"),
                (KEY_CHANNEL_ID, bad),
                (KEY_TEMPLATE_PATH, "welcome"),
            ]);
            assert_eq!(config.mode, Mode::Off, "{bad:?}");
            assert_eq!(config.channel_id, None, "{bad:?}");
            assert_eq!(config.invalid_keys, vec![KEY_CHANNEL_ID], "{bad:?}");
            assert!(!config.is_enabled(), "{bad:?}");
        }
    }

    #[test]
    fn a_bad_template_path_switches_the_feature_off_and_names_the_key() {
        for bad in [
            "../secret",
            "..",
            "sub/welcome",
            "sub\\welcome",
            "C:\\windows\\welcome",
            "/etc/passwd",
            "file:///etc/passwd",
            "welcome.",
            ".welcome",
            "Welcome",
            "con",
        ] {
            let config = parse_pairs(&[
                (KEY_MODE, "live"),
                (KEY_CHANNEL_ID, "810001"),
                (KEY_TEMPLATE_PATH, bad),
            ]);
            assert_eq!(config.mode, Mode::Off, "{bad:?}");
            assert_eq!(config.template_path, None, "{bad:?}");
            assert_eq!(config.invalid_keys, vec![KEY_TEMPLATE_PATH], "{bad:?}");
            assert!(!config.is_enabled(), "{bad:?}");
        }
    }

    #[test]
    fn a_template_path_that_escapes_the_allowed_directory_is_refused() {
        // The whole class of escaping spellings, not only the ones tried first.
        for bad in [
            "../welcome",
            "../../welcome",
            "..\\welcome",
            "a/../../welcome",
            "/welcome",
            "\\welcome",
            "C:/welcome",
            "file:welcome",
            "welcome/../other",
        ] {
            let config = parse_pairs(&[
                (KEY_MODE, "live"),
                (KEY_CHANNEL_ID, "810001"),
                (KEY_TEMPLATE_PATH, bad),
            ]);
            assert_eq!(config.template_path, None, "{bad:?}");
            assert_eq!(config.invalid_keys, vec![KEY_TEMPLATE_PATH], "{bad:?}");
        }
    }

    #[test]
    fn a_bad_join_role_list_switches_the_feature_off_and_names_the_key() {
        for bad in ["abc", "0", "820001,,820002", "-1", "820001,abc"] {
            let config = parse_pairs(&[
                (KEY_MODE, "live"),
                (KEY_CHANNEL_ID, "810001"),
                (KEY_TEMPLATE_PATH, "welcome"),
                (KEY_JOIN_ROLE_IDS, bad),
            ]);
            assert_eq!(config.mode, Mode::Off, "{bad:?}");
            assert!(config.join_role_ids.is_empty(), "{bad:?}");
            assert_eq!(config.invalid_keys, vec![KEY_JOIN_ROLE_IDS], "{bad:?}");
        }
    }

    #[test]
    fn every_bad_key_is_named_and_the_values_are_never_echoed() {
        let config = parse_pairs(&[
            (KEY_MODE, "live"),
            (KEY_CHANNEL_ID, "S3CR3T-CHANNEL"),
            (KEY_TEMPLATE_PATH, "S3CR3T-PATH"),
            (KEY_JOIN_ROLE_IDS, "S3CR3T-ROLE"),
        ]);
        assert_eq!(
            config.invalid_keys,
            vec![KEY_CHANNEL_ID, KEY_TEMPLATE_PATH, KEY_JOIN_ROLE_IDS]
        );
        let text = format!("{config:?} {config}");
        assert!(!text.contains("S3CR3T"), "{text}");
        assert!(!text.contains("810001"), "{text}");
    }

    #[test]
    fn the_key_names_are_the_environment_keys_the_wiring_card_stages() {
        assert_eq!(KEY_MODE, "WELCOME_MODE");
        assert_eq!(KEY_CHANNEL_ID, "WELCOME_CHANNEL_ID");
        assert_eq!(KEY_TEMPLATE_PATH, "WELCOME_TEMPLATE_PATH");
        assert_eq!(KEY_JOIN_ROLE_IDS, "JOIN_ROLE_IDS");
    }

    // ---- the summary line ----------------------------------------------------

    #[test]
    fn the_summary_line_carries_states_and_counts_only() {
        let config = parse_pairs(&complete());
        let line = format!("{config}");
        assert!(line.contains("mode=live"), "{line}");
        assert!(line.contains("channel=set"), "{line}");
        assert!(line.contains("template=set"), "{line}");
        assert!(line.contains("join_roles=2"), "{line}");
        assert!(!line.contains("810001"), "{line}");
        assert!(!line.contains("820001"), "{line}");
        assert!(!line.contains("welcome"), "{line}");
    }

    #[test]
    fn a_feature_without_a_channel_or_a_template_is_not_enabled() {
        let no_channel = parse_pairs(&[(KEY_MODE, "live"), (KEY_TEMPLATE_PATH, "welcome")]);
        assert!(!no_channel.is_enabled());
        let no_template = parse_pairs(&[(KEY_MODE, "live"), (KEY_CHANNEL_ID, "810001")]);
        assert!(!no_template.is_enabled());
    }
}

// Module-level allow for the wiring card W1 that adds production callers; remove it then.
#![allow(dead_code)]

use std::fmt;

// ---------------------------------------------------------------------------
// SettingError — stable codes, key name only (never echoes a value).
// ---------------------------------------------------------------------------

/// Stable error codes for settings-parse failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingError {
    /// The key was not set or resolved to `None`.
    Unset(&'static str),
    /// The value is not one of the accepted tokens.
    InvalidValue(&'static str),
    /// A numeric value exceeds the allowed range.
    OutOfRange(&'static str),
    /// A value contains non-ASCII / negative / overflow content.
    ParseError(&'static str),
}

impl fmt::Display for SettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingError::Unset(key) => write!(f, "{}: unset", key),
            SettingError::InvalidValue(key) => write!(f, "{}: invalid value", key),
            SettingError::OutOfRange(key) => write!(f, "{}: out of range", key),
            SettingError::ParseError(key) => write!(f, "{}: parse error", key),
        }
    }
}

// ---------------------------------------------------------------------------
// Mode — off / shadow / live.
// ---------------------------------------------------------------------------

/// Feature mode: Off (default, fail-closed), Shadow (dry-run), or Live (active).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Shadow,
    Live,
}

/// Parse a mode string.  Unset or anything other than off/shadow/live → Off.
/// Case-insensitive, trimmed.
pub fn parse_mode(_key: &str, raw: Option<&str>) -> Result<Mode, SettingError> {
    let trimmed = match raw {
        Some(s) => s.trim().to_ascii_lowercase(),
        None => return Ok(Mode::Off),
    };
    match trimmed.as_str() {
        "off" => Ok(Mode::Off),
        "shadow" => Ok(Mode::Shadow),
        "live" => Ok(Mode::Live),
        _ => Ok(Mode::Off),
    }
}

// ---------------------------------------------------------------------------
// ID list — comma-separated u64, deduped, sorted.
// ---------------------------------------------------------------------------

/// Parse a comma-separated list of u64 IDs.
///
/// Rules:
/// - Empty or unset → empty Vec.
/// - Each entry is trimmed; only ASCII digits allowed.
/// - Rejects 0, negatives, Unicode digits, overflow.
/// - One bad entry → whole list is an error.
/// - Result is deduplicated and sorted ascending.
pub fn parse_id_list(_key: &str, raw: Option<&str>) -> Result<Vec<u64>, SettingError> {
    let s = match raw {
        Some(s) => s.trim(),
        None => return Ok(Vec::new()),
    };
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for part in s.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Only ASCII digits allowed — reject negatives, Unicode digits, signs, etc.
        if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
            return Err(SettingError::ParseError("id"));
        }
        // Parse as u64 — catches overflow
        let val: u64 = trimmed
            .parse()
            .map_err(|_| SettingError::ParseError("id"))?;
        if val == 0 {
            return Err(SettingError::ParseError("id"));
        }
        ids.push(val);
    }
    // Deduplicate and sort
    ids.sort();
    ids.dedup();
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Duration — integer seconds within [min, max].
// ---------------------------------------------------------------------------

/// Parse a duration in seconds as an integer, bounded to [min, max].
pub fn parse_duration_secs(
    _key: &str,
    raw: Option<&str>,
    min: u64,
    max: u64,
) -> Result<u64, SettingError> {
    let s = match raw {
        Some(s) => s.trim(),
        None => return Err(SettingError::Unset("duration")),
    };
    if s.is_empty() {
        return Err(SettingError::Unset("duration"));
    }
    // Only ASCII digits (no negatives, no Unicode)
    if !s.chars().all(|c| c.is_ascii_digit()) {
        return Err(SettingError::ParseError("duration"));
    }
    let val: u64 = s
        .parse()
        .map_err(|_| SettingError::ParseError("duration"))?;
    if val < min || val > max {
        return Err(SettingError::OutOfRange("duration"));
    }
    Ok(val)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Mode tests ----

    #[test]
    fn mode_off_accepted() {
        assert_eq!(parse_mode("test_key", Some("off")).unwrap(), Mode::Off);
    }

    #[test]
    fn mode_shadow_accepted() {
        assert_eq!(
            parse_mode("test_key", Some("shadow")).unwrap(),
            Mode::Shadow
        );
    }

    #[test]
    fn mode_live_accepted() {
        assert_eq!(parse_mode("test_key", Some("live")).unwrap(), Mode::Live);
    }

    #[test]
    fn mode_case_insensitive() {
        assert_eq!(parse_mode("test_key", Some("LIVE")).unwrap(), Mode::Live);
        assert_eq!(
            parse_mode("test_key", Some("Shadow")).unwrap(),
            Mode::Shadow
        );
        assert_eq!(parse_mode("test_key", Some("OFF")).unwrap(), Mode::Off);
    }

    #[test]
    fn mode_trimmed() {
        assert_eq!(
            parse_mode("test_key", Some("  live  ")).unwrap(),
            Mode::Live
        );
    }

    #[test]
    fn mode_falsey_strings_become_off() {
        // true, 1, yes, on, enabled — all become Off, never Live
        for input in &["true", "1", "yes", "on", "enabled", "on", "y", "t"] {
            let result = parse_mode("test_key", Some(input)).unwrap();
            assert_eq!(result, Mode::Off, "input '{}' should be Off", input);
        }
    }

    #[test]
    fn mode_typos_become_off() {
        for input in &["LIVEE", "shadpw", "of ", "liver", "shado"] {
            let result = parse_mode("test_key", Some(input)).unwrap();
            assert_eq!(result, Mode::Off, "input '{}' should be Off", input);
        }
    }

    #[test]
    fn mode_unset_is_off() {
        assert_eq!(parse_mode("test_key", None).unwrap(), Mode::Off);
    }

    #[test]
    fn mode_empty_string_is_off() {
        assert_eq!(parse_mode("test_key", Some("")).unwrap(), Mode::Off);
    }

    // ---- ID list tests ----

    #[test]
    fn id_list_empty_input_returns_empty() {
        assert_eq!(parse_id_list("test_key", None).unwrap(), Vec::<u64>::new());
        assert_eq!(
            parse_id_list("test_key", Some("")).unwrap(),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn id_list_single_valid() {
        let ids = parse_id_list("test_key", Some("12345")).unwrap();
        assert_eq!(ids, vec![12345u64]);
    }

    #[test]
    fn id_list_multiple_valid() {
        let ids = parse_id_list("test_key", Some("100, 200, 300")).unwrap();
        assert_eq!(ids, vec![100, 200, 300]);
    }

    #[test]
    fn id_list_deduplicates() {
        let ids = parse_id_list("test_key", Some("100, 100, 200, 100")).unwrap();
        assert_eq!(ids, vec![100, 200]);
    }

    #[test]
    fn id_list_sorted() {
        let ids = parse_id_list("test_key", Some("300, 100, 200")).unwrap();
        assert_eq!(ids, vec![100, 200, 300]);
    }

    #[test]
    fn id_list_rejects_zero() {
        let result = parse_id_list("test_key", Some("100, 0, 200"));
        assert!(result.is_err());
    }

    #[test]
    fn id_list_rejects_negative() {
        let result = parse_id_list("test_key", Some("100, -1, 200"));
        assert!(result.is_err());
    }

    #[test]
    fn id_list_rejects_unicode_digits() {
        // Full-width digits ０１２ (U+FF10-U+FF12)
        let result = parse_id_list("test_key", Some("１２３"));
        assert!(result.is_err());
    }

    #[test]
    fn id_list_rejects_overflow() {
        // 18446744073709551616 is u64::MAX + 1
        let result = parse_id_list("test_key", Some("18446744073709551616"));
        assert!(result.is_err());
    }

    #[test]
    fn id_list_one_bad_invalidates_whole_list() {
        let result = parse_id_list("test_key", Some("100, abc, 200"));
        assert!(result.is_err());
    }

    #[test]
    fn id_list_whitespace_only_entries_skipped() {
        let ids = parse_id_list("test_key", Some("100, , 200")).unwrap();
        assert_eq!(ids, vec![100, 200]);
    }

    // ---- Duration tests ----

    #[test]
    fn duration_at_min_is_ok() {
        assert_eq!(
            parse_duration_secs("test_key", Some("60"), 60, 3600).unwrap(),
            60
        );
    }

    #[test]
    fn duration_at_max_is_ok() {
        assert_eq!(
            parse_duration_secs("test_key", Some("3600"), 60, 3600).unwrap(),
            3600
        );
    }

    #[test]
    fn duration_below_min_is_error() {
        let result = parse_duration_secs("test_key", Some("59"), 60, 3600);
        assert!(result.is_err());
    }

    #[test]
    fn duration_above_max_is_error() {
        let result = parse_duration_secs("test_key", Some("3601"), 60, 3600);
        assert!(result.is_err());
    }

    #[test]
    fn duration_non_numeric_is_error() {
        let result = parse_duration_secs("test_key", Some("abc"), 1, 100);
        assert!(result.is_err());
    }

    #[test]
    fn duration_negative_is_error() {
        let result = parse_duration_secs("test_key", Some("-5"), 1, 100);
        assert!(result.is_err());
    }

    #[test]
    fn duration_unchecked_is_error() {
        let result = parse_duration_secs("test_key", None, 1, 100);
        assert!(result.is_err());
    }

    // ---- Error Display / Debug: key present, value absent ----

    #[test]
    fn error_display_contains_key() {
        let err = SettingError::Unset("MY_KEY");
        let display = format!("{}", err);
        assert!(
            display.contains("MY_KEY"),
            "Display should contain key: {}",
            display
        );
    }

    #[test]
    fn error_display_never_contains_value() {
        // Use a sentinel that looks like a secret VALUE.
        // We pass it as the raw VALUE to parse functions and assert
        // the resulting error Display/Debug does NOT contain it.
        let sentinel = "S3CR3T";
        // parse_mode with a secret value should return Off (never Live)
        let result = parse_mode("mode_key", Some(sentinel)).unwrap();
        assert_eq!(result, Mode::Off);
        // parse_id_list with a secret value should error
        let err = parse_id_list("id_key", Some(sentinel)).unwrap_err();
        let display = format!("{}", err);
        assert!(
            !display.contains("S3CR3T"),
            "Error Display must not contain the secret value: {}",
            display
        );
        // parse_duration_secs with a secret value should error
        let err = parse_duration_secs("dur_key", Some(sentinel), 1, 100).unwrap_err();
        let display = format!("{}", err);
        assert!(
            !display.contains("S3CR3T"),
            "Error Display must not contain the secret value: {}",
            display
        );
    }

    #[test]
    fn error_debug_never_contains_value() {
        let sentinel = "S3CR3T";
        // parse_id_list with a secret value should error
        let err = parse_id_list("id_key", Some(sentinel)).unwrap_err();
        let debug = format!("{:?}", err);
        assert!(
            !debug.contains("S3CR3T"),
            "Error Debug must not contain the secret value: {}",
            debug
        );
        // parse_duration_secs with a secret value should error
        let err = parse_duration_secs("dur_key", Some(sentinel), 1, 100).unwrap_err();
        let debug = format!("{:?}", err);
        assert!(
            !debug.contains("S3CR3T"),
            "Error Debug must not contain the secret value: {}",
            debug
        );
    }

    // ---- Mutation test: break parse_mode to confirm tests catch it ----
    // If we change the live branch to return Off instead of Live,
    // mode_live_accepted will fail. This confirms the test is real.

    // ---- Mutation test: break parse_id_list to confirm tests catch it ----
    // If we remove the zero-check, id_list_rejects_zero will fail.

    // ---- Parser-specific key-in-display tests (RED: these fail against old code) ----

    #[test]
    fn parse_id_list_error_display_contains_key() {
        let err = parse_id_list("PROTECTED_ROLE_IDS", Some("S3CR3T")).unwrap_err();
        let display = format!("{}", err);
        assert!(
            display.contains("PROTECTED_ROLE_IDS"),
            "Display should contain key: {}",
            display
        );
    }

    #[test]
    fn parse_duration_secs_error_display_contains_key() {
        let err = parse_duration_secs("FEATURE_X_TIMEOUT_SECS", Some("S3CR3T"), 1, 100).unwrap_err();
        let display = format!("{}", err);
        assert!(
            display.contains("FEATURE_X_TIMEOUT_SECS"),
            "Display should contain key: {}",
            display
        );
    }

    #[test]
    fn parse_duration_unset_error_contains_key() {
        let err = parse_duration_secs("TIMEOUT_SECS", None, 1, 100).unwrap_err();
        let display = format!("{}", err);
        assert!(
            display.contains("TIMEOUT_SECS"),
            "Display should contain key: {}",
            display
        );
    }

    #[test]
    fn parse_duration_out_of_range_error_contains_key() {
        let err = parse_duration_secs("TIMEOUT_SECS", Some("999"), 1, 100).unwrap_err();
        let display = format!("{}", err);
        assert!(
            display.contains("TIMEOUT_SECS"),
            "Display should contain key: {}",
            display
        );
    }

    #[test]
    fn parse_id_list_empty_entry_error_contains_key() {
        let err = parse_id_list("CHANNEL_IDS", Some("100,,200")).unwrap_err();
        let display = format!("{}", err);
        assert!(
            display.contains("CHANNEL_IDS"),
            "Display should contain key: {}",
            display
        );
    }
}

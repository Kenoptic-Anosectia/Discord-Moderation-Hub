//! A15: optional retention policy parsing and pure eligibility/purge planning.
//! Owned by exactly one A08 slice.

/// Environment / config key for the retention period.
pub const RETENTION_DAYS_KEY: &str = "TICKET_ARCHIVE_RETENTION_DAYS";

/// Whether retention-based automatic purge is enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// No automatic purge — archives are never removed.
    Disabled,
    /// Purge archives after this many days since confirmed closure.
    AfterDays(u32),
}

/// Errors from parsing a retention configuration value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionError {
    /// The value could not be parsed into a valid retention policy.
    Invalid,
}

// ---------------------------------------------------------------------------
// parse_retention
// ---------------------------------------------------------------------------

/// Parse a retention policy from a raw string.
///
/// - `None` or all-whitespace → `Ok(Disabled)`
/// - Otherwise the trimmed text must be **ASCII digits only** (`[0-9]+`),
///   no sign, no decimal, no underscore, no unit, no non-ASCII digits.
/// - The parsed integer must be in `1..=36500` → `Ok(AfterDays(n))`.
/// - Anything else → `Err(Invalid)`.
///
/// This function never panics.
pub fn parse_retention(raw: Option<&str>) -> Result<RetentionPolicy, RetentionError> {
    let s = match raw {
        None => return Ok(RetentionPolicy::Disabled),
        Some(s) => s,
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(RetentionPolicy::Disabled);
    }
    // Must be ASCII digits only — reject signs, decimals, underscores,
    // non-ASCII digits (e.g. Arabic-Indic), and any other character.
    if !trimmed.chars().all(|c: char| c.is_ascii_digit()) {
        return Err(RetentionError::Invalid);
    }
    // Reject leading-zero numbers like "007" — the spec says ASCII digits
    // only and parse to integer; "0" itself is valid digits but out of range.
    // Use u64 parsing to avoid overflow, then check range.
    let n: u64 = trimmed.parse().map_err(|_| RetentionError::Invalid)?;
    if !(1..=36500).contains(&n) {
        return Err(RetentionError::Invalid);
    }
    Ok(RetentionPolicy::AfterDays(n as u32))
}

// ---------------------------------------------------------------------------
// eligible
// ---------------------------------------------------------------------------

/// Check whether a candidate archive is eligible for purge.
///
/// - `Disabled` → always `false`.
/// - `closed_at == None` → `false`.
/// - `AfterDays(d)`: `true` iff `now >= closed_at + d * 86400`.
///   Exactly at the boundary is eligible; one second before is not.
/// - `closed_at` in the future → `false`.
///
/// Uses checked/saturating i64 arithmetic — never panics.
pub fn eligible(policy: RetentionPolicy, closed_at: Option<i64>, now: i64) -> bool {
    if matches!(policy, RetentionPolicy::Disabled) {
        return false;
    }
    let Some(closed_at) = closed_at else {
        return false;
    };
    // Future closed_at → not eligible
    if now < closed_at {
        return false;
    }
    if let RetentionPolicy::AfterDays(days) = policy {
        let seconds: i64 = (days as i64).saturating_mul(86400);
        let threshold = match closed_at.checked_add(seconds) {
            Some(v) => v,
            None => return true, // overflow → far in the future, always past
        };
        return now >= threshold;
    }
    false
}

// ---------------------------------------------------------------------------
// plan_purge
// ---------------------------------------------------------------------------

/// A candidate archive for potential purge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub ticket: i64,
    pub seq: i64,
    pub dir_name: String,
    pub closed_at: Option<i64>,
}

/// Build a purge plan: eligible candidates sorted and deduplicated.
///
/// - Disabled → empty `Vec`.
/// - Returns only candidates for which `eligible` is `true`.
/// - Sorted ascending by `(ticket, seq)`.
/// - Each `(ticket, seq)` pair appears at most once.
/// - Deterministic: same input always gives same output.
pub fn plan_purge(policy: RetentionPolicy, now: i64, candidates: &[Candidate]) -> Vec<Candidate> {
    if matches!(policy, RetentionPolicy::Disabled) {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    let mut result: Vec<Candidate> = candidates
        .iter()
        .filter(|c| eligible(policy, c.closed_at, now))
        .filter(|c| seen.insert((c.ticket, c.seq)))
        .cloned()
        .collect();
    result.sort_by_key(|c| (c.ticket, c.seq));
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_retention ----

    #[test]
    fn parse_none_is_disabled() {
        assert_eq!(parse_retention(None), Ok(RetentionPolicy::Disabled));
    }

    #[test]
    fn parse_empty_is_disabled() {
        assert_eq!(parse_retention(Some("")), Ok(RetentionPolicy::Disabled));
    }

    #[test]
    fn parse_whitespace_is_disabled() {
        assert_eq!(parse_retention(Some("   ")), Ok(RetentionPolicy::Disabled));
    }

    #[test]
    fn parse_single_digit() {
        assert_eq!(
            parse_retention(Some("1")),
            Ok(RetentionPolicy::AfterDays(1))
        );
    }

    #[test]
    fn parse_max_value() {
        assert_eq!(
            parse_retention(Some("36500")),
            Ok(RetentionPolicy::AfterDays(36500))
        );
    }

    #[test]
    fn parse_with_padding_spaces() {
        assert_eq!(
            parse_retention(Some(" 30 ")),
            Ok(RetentionPolicy::AfterDays(30))
        );
    }

    // --- Invalid cases ---

    #[test]
    fn parse_zero_is_invalid() {
        assert_eq!(parse_retention(Some("0")), Err(RetentionError::Invalid));
    }

    #[test]
    fn parse_negative_is_invalid() {
        assert_eq!(parse_retention(Some("-1")), Err(RetentionError::Invalid));
    }

    #[test]
    fn parse_positive_sign_is_invalid() {
        assert_eq!(parse_retention(Some("+5")), Err(RetentionError::Invalid));
    }

    #[test]
    fn parse_unit_suffix_is_invalid() {
        assert_eq!(parse_retention(Some("30d")), Err(RetentionError::Invalid));
    }

    #[test]
    fn parse_decimal_is_invalid() {
        assert_eq!(parse_retention(Some("3.5")), Err(RetentionError::Invalid));
    }

    #[test]
    fn parse_underscore_is_invalid() {
        assert_eq!(parse_retention(Some("1_0")), Err(RetentionError::Invalid));
    }

    #[test]
    fn parse_too_large_is_invalid() {
        assert_eq!(parse_retention(Some("36501")), Err(RetentionError::Invalid));
    }

    #[test]
    fn parse_huge_number_overflows() {
        assert_eq!(
            parse_retention(Some("99999999999999999999")),
            Err(RetentionError::Invalid)
        );
    }

    #[test]
    fn parse_arabic_indic_digits_is_invalid() {
        // Arabic-Indic digits: ٣٠ = 30 in Eastern Arabic-Indic script
        assert_eq!(parse_retention(Some("٣٠")), Err(RetentionError::Invalid));
    }

    // ---- eligible ----

    #[test]
    fn eligible_disabled_never() {
        assert!(!eligible(RetentionPolicy::Disabled, Some(1000), 999999999));
    }

    #[test]
    fn eligible_disabled_no_closed() {
        assert!(!eligible(RetentionPolicy::Disabled, None, 999999999));
    }

    #[test]
    fn eligible_none_closed_at() {
        assert!(!eligible(RetentionPolicy::AfterDays(30), None, 999999999));
    }

    #[test]
    fn eligible_exact_boundary() {
        let closed_at = 1000i64;
        let days = 1u32;
        let threshold = closed_at + (days as i64) * 86400; // exactly 1 day
        assert!(eligible(
            RetentionPolicy::AfterDays(days),
            Some(closed_at),
            threshold
        ));
    }

    #[test]
    fn eligible_one_second_before_boundary() {
        let closed_at = 1000i64;
        let days = 1u32;
        let threshold = closed_at + (days as i64) * 86400;
        assert!(!eligible(
            RetentionPolicy::AfterDays(days),
            Some(closed_at),
            threshold - 1
        ));
    }

    #[test]
    fn eligible_future_closed_at() {
        assert!(!eligible(
            RetentionPolicy::AfterDays(30),
            Some(999999999),
            1000
        ));
    }

    #[test]
    fn eligible_i64_max_now() {
        // Large now, reasonable closed_at — should not panic
        let result = eligible(RetentionPolicy::AfterDays(30), Some(1000000), i64::MAX);
        assert!(result);
    }

    #[test]
    fn eligible_i64_min_closed_at() {
        // closed_at in the distant past — should not panic
        let result = eligible(RetentionPolicy::AfterDays(1), Some(i64::MIN), 1000);
        assert!(result);
    }

    #[test]
    fn eligible_max_days_no_panic() {
        // 36500 days = ~100 years — saturating_mul should handle it
        let result = eligible(RetentionPolicy::AfterDays(36500), Some(1000), i64::MAX);
        assert!(result);
    }

    #[test]
    fn eligible_max_days_too_soon() {
        // 36500 days from now — not eligible yet
        let result = eligible(
            RetentionPolicy::AfterDays(36500),
            Some(1000),
            1000 + 36500 * 86400 - 1,
        );
        assert!(!result);
    }

    // ---- plan_purge ----

    #[test]
    fn plan_purge_disabled_returns_empty() {
        let now = 1000000i64;
        let cands = vec![Candidate {
            ticket: 1,
            seq: 1,
            dir_name: "ticket-1-1".to_string(),
            closed_at: Some(100),
        }];
        let result = plan_purge(RetentionPolicy::Disabled, now, &cands);
        assert!(result.is_empty());
    }

    #[test]
    fn plan_purge_mixed_eligible_ineligible() {
        let now = 1000000i64;
        let cands = vec![
            Candidate {
                ticket: 1,
                seq: 1,
                dir_name: "ticket-1-1".to_string(),
                closed_at: Some(100),
            }, // eligible
            Candidate {
                ticket: 2,
                seq: 1,
                dir_name: "ticket-2-1".to_string(),
                closed_at: Some(999999),
            }, // not eligible
            Candidate {
                ticket: 3,
                seq: 1,
                dir_name: "ticket-3-1".to_string(),
                closed_at: None,
            }, // not eligible
        ];
        let result = plan_purge(RetentionPolicy::AfterDays(1), now, &cands);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].ticket, 1);
    }

    #[test]
    fn plan_purge_duplicates_removed() {
        let now = 1000000i64;
        let cands = vec![
            Candidate {
                ticket: 1,
                seq: 1,
                dir_name: "ticket-1-1".to_string(),
                closed_at: Some(100),
            },
            Candidate {
                ticket: 1,
                seq: 1,
                dir_name: "ticket-1-1-v2".to_string(),
                closed_at: Some(200),
            },
        ];
        let result = plan_purge(RetentionPolicy::AfterDays(1), now, &cands);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn plan_purge_sorted() {
        let now = 1000000i64;
        let cands = vec![
            Candidate {
                ticket: 5,
                seq: 1,
                dir_name: "ticket-5-1".to_string(),
                closed_at: Some(100),
            },
            Candidate {
                ticket: 1,
                seq: 1,
                dir_name: "ticket-1-1".to_string(),
                closed_at: Some(100),
            },
            Candidate {
                ticket: 3,
                seq: 2,
                dir_name: "ticket-3-2".to_string(),
                closed_at: Some(100),
            },
            Candidate {
                ticket: 3,
                seq: 1,
                dir_name: "ticket-3-1".to_string(),
                closed_at: Some(100),
            },
        ];
        let result = plan_purge(RetentionPolicy::AfterDays(1), now, &cands);
        let tickets: Vec<i64> = result.iter().map(|c| c.ticket).collect();
        let seqs: Vec<i64> = result.iter().map(|c| c.seq).collect();
        assert_eq!(tickets, vec![1, 3, 3, 5]);
        assert_eq!(seqs, vec![1, 1, 2, 1]);
    }

    #[test]
    fn plan_purge_deterministic() {
        let now = 1000000i64;
        let cands = vec![
            Candidate {
                ticket: 2,
                seq: 1,
                dir_name: "ticket-2-1".to_string(),
                closed_at: Some(100),
            },
            Candidate {
                ticket: 1,
                seq: 1,
                dir_name: "ticket-1-1".to_string(),
                closed_at: Some(100),
            },
        ];
        let r1 = plan_purge(RetentionPolicy::AfterDays(1), now, &cands);
        let r2 = plan_purge(RetentionPolicy::AfterDays(1), now, &cands);
        assert_eq!(r1, r2);
    }

    #[test]
    fn plan_purge_empty_input() {
        let result = plan_purge(RetentionPolicy::AfterDays(30), 1000, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn plan_purge_large_old_list_zero_delete() {
        // All candidates are recent → zero purge requests
        let now = 1000000i64;
        let cands: Vec<Candidate> = (1..=50)
            .map(|t| Candidate {
                ticket: t,
                seq: 1,
                dir_name: format!("ticket-{}-1", t),
                closed_at: Some(now - 100), // only 100 seconds ago
            })
            .collect();
        let result = plan_purge(RetentionPolicy::AfterDays(30), now, &cands);
        assert!(result.is_empty());
    }
}

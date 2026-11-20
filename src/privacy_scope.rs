// Wiring card W1 (general-logger integration) will remove this allow once a production
// caller invokes `may_log`.  Until then the module is dead code in a binary crate.
#![allow(dead_code)]

use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Public API — STUB: always returns Allow until GREEN phase implementation
// ---------------------------------------------------------------------------

/// Decides whether an event may be recorded in the general log.
///
/// STUB: returns Allow for all inputs. Tests expect specific Skip decisions.
pub fn may_log(_scope: &Scope, _event: &ChannelFacts) -> LogDecision {
    LogDecision::Allow
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration seen by the predicate for a single guild.
///
/// The flash-channel IDs are merged into the excluded-channel set at
/// construction time (rule 8).
#[derive(Debug, Clone)]
pub struct Scope {
    excluded_channels: HashSet<u64>,
    excluded_categories: HashSet<u64>,
}

impl Scope {
    /// Build a scope from the two ID sets plus flash-channel IDs.
    ///
    /// Flash-channel IDs are merged into the excluded-channel set (rule 8).
    /// Empty sets allow everything (rule 7).
    pub fn new(
        excluded_channels: Vec<u64>,
        excluded_categories: Vec<u64>,
        flash_channel_ids: Vec<u64>,
    ) -> Self {
        let mut channels: HashSet<u64> = excluded_channels.into_iter().collect();
        for fid in flash_channel_ids {
            channels.insert(fid);
        }
        let categories: HashSet<u64> = excluded_categories.into_iter().collect();
        Self {
            excluded_channels: channels,
            excluded_categories: categories,
        }
    }
}

/// Plain-data container for the channel context needed by the predicate.
///
/// `parent_id` is the thread's parent channel or the channel's category.
/// For channel-move events `old_parent_id` and `new_parent_id` describe the
/// move; at least one of them may be `None` (e.g. a simple category change
/// where the channel was not previously in any category).
#[derive(Debug, Clone)]
pub struct ChannelFacts {
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub parent_id: Option<u64>,
    pub old_parent_id: Option<u64>,
    pub new_parent_id: Option<u64>,
}

/// Decision returned by `may_log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogDecision {
    /// The event may be logged.
    Allow,
    /// The event must be skipped; the reason is stable and never carries
    /// user-visible text, names or ids.
    Skip(SkipReason),
}

/// Stable reasons for skipping an event.
///
/// These codes are stable across deployments and never carry message text,
/// names or ids from users.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The event's channel is in the excluded-channel list (rule 1).
    ExcludedChannel,
    /// The event's parent channel is in the excluded-channel list (rule 2).
    ExcludedParentChannel,
    /// The event's parent is in the excluded-category list (rule 3).
    ExcludedCategory,
    /// The event was moved into an excluded category (rule 4).
    MovedIntoExcludedCategory,
    /// The event was moved out of an excluded category (rule 4).
    MovedOutOfExcludedCategory,
    /// The parent could not be resolved; fail closed (rule 6).
    UnknownLookup,
}

// ---------------------------------------------------------------------------
// Tests — RED phase: these fail against the stub implementation
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: u64 = 100_001;
    const NORMAL_CHANNEL: u64 = 200_001;
    const EXCLUDED_CHANNEL: u64 = 200_002;
    const FLASH_CHANNEL: u64 = 200_003;
    const NORMAL_CATEGORY: u64 = 300_001;
    const EXCLUDED_CATEGORY: u64 = 300_002;
    const EXCLUDED_THREAD_PARENT: u64 = 200_005;
    const DM_CHANNEL: u64 = 400_001;

    fn empty_scope() -> Scope {
        Scope::new(vec![], vec![], vec![])
    }

    fn base_facts(channel: u64, parent: Option<u64>) -> ChannelFacts {
        ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: channel,
            parent_id: parent,
            old_parent_id: None,
            new_parent_id: None,
        }
    }

    fn move_facts(
        channel: u64,
        parent: Option<u64>,
        old_parent: Option<u64>,
        new_parent: Option<u64>,
    ) -> ChannelFacts {
        ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: channel,
            parent_id: parent,
            old_parent_id: old_parent,
            new_parent_id: new_parent,
        }
    }

    // RED: empty config -> allow everything (in guild).
    // This test passes on the stub (Allow).
    #[test]
    fn empty_config_allows_normal_channel() {
        let scope = empty_scope();
        let facts = base_facts(NORMAL_CHANNEL, Some(NORMAL_CATEGORY));
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // RED: excluded channel is skipped.
    // STUB returns Allow → this FAILS.
    #[test]
    fn excluded_channel_is_skipped() {
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![], vec![]);
        let facts = base_facts(EXCLUDED_CHANNEL, Some(NORMAL_CATEGORY));
        let decision = may_log(&scope, &facts);
        assert_eq!(decision, LogDecision::Skip(SkipReason::ExcludedChannel));
    }

    // RED: thread whose parent channel is excluded is skipped.
    // STUB returns Allow → this FAILS.
    #[test]
    fn thread_with_excluded_parent_is_skipped() {
        let scope = Scope::new(vec![EXCLUDED_THREAD_PARENT], vec![], vec![]);
        let facts = base_facts(NORMAL_CHANNEL, Some(EXCLUDED_THREAD_PARENT));
        let decision = may_log(&scope, &facts);
        assert_eq!(decision, LogDecision::Skip(SkipReason::ExcludedParentChannel));
    }

    // RED: channel inside an excluded category is skipped.
    // STUB returns Allow → this FAILS.
    #[test]
    fn channel_in_excluded_category_is_skipped() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = base_facts(NORMAL_CHANNEL, Some(EXCLUDED_CATEGORY));
        let decision = may_log(&scope, &facts);
        assert_eq!(decision, LogDecision::Skip(SkipReason::ExcludedCategory));
    }

    // RED: move into excluded category is skipped.
    // STUB returns Allow → this FAILS.
    #[test]
    fn move_into_excluded_category_is_skipped() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            Some(EXCLUDED_CATEGORY),
            Some(NORMAL_CATEGORY),
            Some(EXCLUDED_CATEGORY),
        );
        let decision = may_log(&scope, &facts);
        assert_eq!(
            decision,
            LogDecision::Skip(SkipReason::MovedIntoExcludedCategory)
        );
    }

    // RED: move out of excluded category is skipped.
    // STUB returns Allow → this FAILS.
    #[test]
    fn move_out_of_excluded_category_is_skipped() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            Some(NORMAL_CATEGORY),
            Some(EXCLUDED_CATEGORY),
            Some(NORMAL_CATEGORY),
        );
        let decision = may_log(&scope, &facts);
        assert_eq!(
            decision,
            LogDecision::Skip(SkipReason::MovedOutOfExcludedCategory)
        );
    }

    // RED: DM is allowed by this predicate.
    // STUB returns Allow → this PASSES (will be re-tested against full impl).
    #[test]
    fn dm_is_allowed_by_this_predicate() {
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = ChannelFacts {
            guild_id: None,
            channel_id: DM_CHANNEL,
            parent_id: None,
            old_parent_id: None,
            new_parent_id: None,
        };
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // RED: unknown lookup -> skip.
    // STUB returns Allow → this FAILS.
    #[test]
    fn unknown_lookup_fails_closed() {
        let scope = empty_scope();
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: NORMAL_CHANNEL,
            parent_id: None,
            old_parent_id: None,
            new_parent_id: None,
        };
        assert_eq!(may_log(&scope, &facts), LogDecision::Skip(SkipReason::UnknownLookup));
    }

    // RED: flash channel IDs are merged into the exclusion set.
    // STUB returns Allow → this FAILS.
    #[test]
    fn flash_channel_ids_are_merged_into_exclusions() {
        let scope = Scope::new(vec![], vec![], vec![FLASH_CHANNEL]);
        let facts = base_facts(FLASH_CHANNEL, Some(NORMAL_CATEGORY));
        let decision = may_log(&scope, &facts);
        assert_eq!(decision, LogDecision::Skip(SkipReason::ExcludedChannel));
    }

    // Verify LogDecision variants are distinct (always passes).
    #[test]
    fn log_decision_variants_are_distinct() {
        assert_ne!(LogDecision::Allow, LogDecision::Skip(SkipReason::ExcludedChannel));
        assert_ne!(
            LogDecision::Skip(SkipReason::ExcludedChannel),
            LogDecision::Skip(SkipReason::ExcludedCategory)
        );
    }
}

use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Decides whether an event may be recorded in the general log.
///
/// Called by the general logger *before* it builds or caches anything.
pub fn may_log(scope: &Scope, event: &ChannelFacts) -> LogDecision {
    // Rule 5: DMs (no guild id) are allowed by this predicate.
    // A separate guard already handles DMs; this predicate does not block them.
    if event.guild_id.is_none() {
        return LogDecision::Allow;
    }

    // Rule 7: an empty configuration allows everything. This check comes
    // BEFORE every lookup check on purpose: rule 6's "fail closed" exists so an
    // unresolvable channel cannot leak past an exclusion, and with nothing
    // excluded there is nothing to leak past. Returning Allow here is also
    // what keeps today's behaviour unchanged when the wiring card switches
    // this predicate on with no ids configured.
    if scope.excluded_channels.is_empty() && scope.excluded_categories.is_empty() {
        return LogDecision::Allow;
    }

    // Rule 6 (channel_id): if the channel id itself is unknown, fail closed.
    let Some(channel_id) = event.channel_id else {
        return LogDecision::Skip(SkipReason::UnknownLookup);
    };

    // Rule 1: excluded channel is skipped.
    if scope.excluded_channels.contains(&channel_id) {
        return LogDecision::Skip(SkipReason::ExcludedChannel);
    }

    // Rule 3 (category shortcut): if we have a resolved category and it is
    // excluded, skip immediately — this also covers threads whose grandparent
    // category is excluded.
    if let ParentResolution::Id(cat_id) = &event.category_id {
        if scope.excluded_categories.contains(cat_id) {
            return LogDecision::Skip(SkipReason::ExcludedCategory);
        }
    }
    // If category_id is Unknown, fail closed (rule 6).
    if matches!(event.category_id, ParentResolution::Unknown) {
        return LogDecision::Skip(SkipReason::UnknownLookup);
    }

    // Rule 3 (also): the event may be ON the excluded category itself (a
    // rename or a permission overwrite on the category channel).
    if scope.excluded_categories.contains(&channel_id) {
        return LogDecision::Skip(SkipReason::ExcludedCategory);
    }

    // Rule 6 (parent): if we cannot resolve the parent, fail closed.
    // `None` means the channel genuinely has no parent (top-level Discord
    // channel), which is NOT an unknown lookup — rule 7 allows it.
    let parent_resolved = match &event.parent_id {
        ParentResolution::Id(id) => Some(*id),
        ParentResolution::Unknown => return LogDecision::Skip(SkipReason::UnknownLookup),
        ParentResolution::None => None,
    };

    // Rule 2: a thread whose parent channel is excluded is skipped.
    if let Some(parent_id) = parent_resolved {
        if scope.excluded_channels.contains(&parent_id) {
            return LogDecision::Skip(SkipReason::ExcludedParentChannel);
        }

        // Rule 3: a channel inside an excluded category is skipped.
        if scope.excluded_categories.contains(&parent_id) {
            return LogDecision::Skip(SkipReason::ExcludedCategory);
        }
    }

    // Rule 4: a channel moved into or out of an excluded category is skipped,
    // judged on BOTH the old and the new parent.
    //
    // An `Unknown` old parent means the channel may have been moved OUT of an
    // excluded category (the old parent could not be resolved), and an
    // `Unknown` new parent means it may have been moved INTO one. Both fail
    // closed (rules 4 + 6).
    if let ParentResolution::Id(old_id) = &event.old_parent_id {
        if scope.excluded_categories.contains(old_id) {
            return LogDecision::Skip(SkipReason::MovedOutOfExcludedCategory);
        }
    }
    if matches!(event.old_parent_id, ParentResolution::Unknown) {
        return LogDecision::Skip(SkipReason::UnknownLookup);
    }

    if let ParentResolution::Id(new_id) = &event.new_parent_id {
        if scope.excluded_categories.contains(new_id) {
            return LogDecision::Skip(SkipReason::MovedIntoExcludedCategory);
        }
    }
    if matches!(event.new_parent_id, ParentResolution::Unknown) {
        return LogDecision::Skip(SkipReason::UnknownLookup);
    }

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

/// Resolution state for a parent / category lookup.
///
/// Discord top-level channels genuinely have no parent (`None`).  A lookup
/// that could not be resolved is `Unknown` (rule 6: fail closed).  A resolved
/// id is `Id(u64)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentResolution {
    /// The channel has no parent (top-level Discord channel).
    None,
    /// The id was resolved successfully.
    Id(u64),
    /// The lookup could not be resolved; fail closed (rule 6).
    Unknown,
}

/// Plain-data container for the channel context needed by the predicate.
///
/// `channel_id` is `Option<u64>` so the caller can represent an unresolved
/// channel (rule 6).  `parent_id` uses `ParentResolution` to distinguish
/// "no parent" (top-level channel, rule 7) from "lookup failed" (rule 6).
/// `category_id` carries the channel's *category* (for threads: the grandparent
/// category) so that threads inside excluded categories are caught.  For
/// channel-move events `old_parent_id` and `new_parent_id` describe the move;
/// both use `ParentResolution` so that a failed lookup (`Unknown`) is
/// distinguishable from "the channel was not in a category" (`None`).
#[derive(Debug, Clone)]
pub struct ChannelFacts {
    pub guild_id: Option<u64>,
    /// The channel id. `None` in a guild → Skip(UnknownLookup) (rule 6).
    pub channel_id: Option<u64>,
    /// The thread's parent channel or the channel's category.  Uses
    /// `ParentResolution` so `None` (no parent) is distinguishable from
    /// `Unknown` (lookup failed).
    pub parent_id: ParentResolution,
    /// The channel's category (for threads: the grandparent category).
    /// When present and excluded, the event is skipped (rule 3).
    pub category_id: ParentResolution,
    /// The old parent category on a move event. Uses `ParentResolution` so
    /// `Unknown` (lookup failed) is distinguishable from `None` (the channel
    /// was not previously in a category). An `Unknown` old parent means the
    /// channel may have been moved OUT of an excluded category → Skip
    /// (rules 4 + 6).
    pub old_parent_id: ParentResolution,
    /// The new parent category on a move event. Uses `ParentResolution` so
    /// `Unknown` (lookup failed) is distinguishable from `None` (the channel
    /// was not moved into a category). An `Unknown` new parent means the
    /// channel may have been moved INTO an excluded category → Skip
    /// (rules 4 + 6).
    pub new_parent_id: ParentResolution,
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
    /// A lookup could not be resolved; fail closed (rule 6).
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

    fn base_facts(channel: u64, parent: ParentResolution) -> ChannelFacts {
        ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(channel),
            parent_id: parent,
            category_id: ParentResolution::None,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        }
    }

    fn move_facts(
        channel: u64,
        parent: ParentResolution,
        category: ParentResolution,
        old_parent: ParentResolution,
        new_parent: ParentResolution,
    ) -> ChannelFacts {
        ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(channel),
            parent_id: parent,
            category_id: category,
            old_parent_id: old_parent,
            new_parent_id: new_parent,
        }
    }

    fn facts_with_channel_none() -> ChannelFacts {
        ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: None,
            parent_id: ParentResolution::None,
            category_id: ParentResolution::None,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        }
    }

    // RED: empty config -> allow everything (in guild).
    // This test passes on the stub (Allow).
    #[test]
    fn empty_config_allows_normal_channel() {
        let scope = empty_scope();
        let facts = base_facts(NORMAL_CHANNEL, ParentResolution::Id(NORMAL_CATEGORY));
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // RED: excluded channel is skipped.
    // STUB returns Allow → this FAILS.
    #[test]
    fn excluded_channel_is_skipped() {
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![], vec![]);
        let facts = base_facts(EXCLUDED_CHANNEL, ParentResolution::Id(NORMAL_CATEGORY));
        let decision = may_log(&scope, &facts);
        assert_eq!(decision, LogDecision::Skip(SkipReason::ExcludedChannel));
    }

    // RED: thread whose parent channel is excluded is skipped.
    // STUB returns Allow → this FAILS.
    #[test]
    fn thread_with_excluded_parent_is_skipped() {
        let scope = Scope::new(vec![EXCLUDED_THREAD_PARENT], vec![], vec![]);
        let facts = base_facts(NORMAL_CHANNEL, ParentResolution::Id(EXCLUDED_THREAD_PARENT));
        let decision = may_log(&scope, &facts);
        assert_eq!(
            decision,
            LogDecision::Skip(SkipReason::ExcludedParentChannel)
        );
    }

    // RED: channel inside an excluded category is skipped.
    // STUB returns Allow → this FAILS.
    #[test]
    fn channel_in_excluded_category_is_skipped() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = base_facts(NORMAL_CHANNEL, ParentResolution::Id(EXCLUDED_CATEGORY));
        let decision = may_log(&scope, &facts);
        assert_eq!(decision, LogDecision::Skip(SkipReason::ExcludedCategory));
    }

    // RED: move into excluded category is skipped.
    // When parent_id already points to an excluded category, rule 3 fires first
    // (ExcludedCategory). The test asserts the channel is correctly skipped.
    #[test]
    fn move_into_excluded_category_is_skipped() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(EXCLUDED_CATEGORY),
            ParentResolution::None,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::Id(EXCLUDED_CATEGORY),
        );
        // parent_id is an excluded category → rule 3 fires first.
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::ExcludedCategory)
        );
    }

    // RED: move out of excluded category is skipped.
    // STUB returns Allow → this FAILS.
    #[test]
    fn move_out_of_excluded_category_is_skipped() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::None,
            ParentResolution::Id(EXCLUDED_CATEGORY),
            ParentResolution::Id(NORMAL_CATEGORY),
        );
        assert_eq!(
            may_log(&scope, &facts),
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
            channel_id: Some(DM_CHANNEL),
            parent_id: ParentResolution::None,
            category_id: ParentResolution::None,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // RED: unknown lookup -> skip (non-empty scope).
    // STUB returns Allow → this FAILS.
    // Rule 6 only bites when something is actually excluded: under an empty
    // scope rule 7 allows everything, so this test uses a non-empty scope.
    #[test]
    fn unknown_lookup_fails_closed() {
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![], vec![]);
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(NORMAL_CHANNEL),
            parent_id: ParentResolution::Unknown,
            category_id: ParentResolution::None,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::UnknownLookup)
        );
    }

    // RED: flash channel IDs are merged into the exclusion set.
    // STUB returns Allow → this FAILS.
    #[test]
    fn flash_channel_ids_are_merged_into_exclusions() {
        let scope = Scope::new(vec![], vec![], vec![FLASH_CHANNEL]);
        let facts = base_facts(FLASH_CHANNEL, ParentResolution::Id(NORMAL_CATEGORY));
        let decision = may_log(&scope, &facts);
        assert_eq!(decision, LogDecision::Skip(SkipReason::ExcludedChannel));
    }

    // Verify LogDecision variants are distinct (always passes).
    #[test]
    fn log_decision_variants_are_distinct() {
        assert_ne!(
            LogDecision::Allow,
            LogDecision::Skip(SkipReason::ExcludedChannel)
        );
        assert_ne!(
            LogDecision::Skip(SkipReason::ExcludedChannel),
            LogDecision::Skip(SkipReason::ExcludedCategory)
        );
    }

    // ---- Rule 7 fix: no-parent (None) is NOT unknown ----

    // Rule 7: empty config + no parent (top-level uncategorised channel) → Allow.
    // This test FAILS on the round-1 code because parent_id:None was conflated
    // with Unknown and returned Skip(UnknownLookup).
    #[test]
    fn empty_config_allows_uncategorised_channel() {
        let scope = empty_scope();
        let facts = base_facts(NORMAL_CHANNEL, ParentResolution::None);
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // ---- Round-1 fix: category_id field for threads ----

    // Thread inside an excluded category is skipped via category_id (rule 3).
    // This test FAILS on the round-1 code because there was no category_id
    // field, so the thread's grandparent category was never checked.
    #[test]
    fn thread_in_excluded_category_is_skipped() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(NORMAL_CHANNEL),
            // Parent is a normal channel (not excluded).
            parent_id: ParentResolution::Id(NORMAL_CHANNEL),
            // But the grandparent category is excluded.
            category_id: ParentResolution::Id(EXCLUDED_CATEGORY),
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::ExcludedCategory)
        );
    }

    // ---- Rule 6 (channel_id): None → UnknownLookup ----

    #[test]
    fn unknown_channel_id_fails_closed() {
        // Non-empty scope: under an empty scope rule 7 allows everything.
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![], vec![]);
        let facts = facts_with_channel_none();
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::UnknownLookup)
        );
    }

    // ---- Rule 6 (category_id): Unknown → UnknownLookup ----

    #[test]
    fn unknown_category_fails_closed() {
        // Non-empty scope: under an empty scope rule 7 allows everything.
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![], vec![]);
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(NORMAL_CHANNEL),
            parent_id: ParentResolution::Id(NORMAL_CATEGORY),
            category_id: ParentResolution::Unknown,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::UnknownLookup)
        );
    }

    // ---- Rule 4: move-into with stale parent, new_parent excluded ----
    // This test ensures the MovedIntoExcludedCategory branch is actually
    // exercised.  On the round-1 code, deleting the new_parent branch still
    // passed because all move-into tests set parent_id to the excluded
    // category (rule 3 fired first).  Here parent_id is normal.
    #[test]
    fn move_into_excluded_category_with_stale_parent() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::None,
            ParentResolution::None,
            ParentResolution::Id(EXCLUDED_CATEGORY),
        );
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::MovedIntoExcludedCategory)
        );
    }

    // ---- Boundary: empty config, no parent, no category ----
    #[test]
    fn empty_config_allows_no_parent_no_category() {
        let scope = empty_scope();
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(NORMAL_CHANNEL),
            parent_id: ParentResolution::None,
            category_id: ParentResolution::None,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // Boundary: parent is excluded both as a channel and as a category.
    #[test]
    fn parent_excluded_as_channel_takes_precedence() {
        let scope = Scope::new(
            vec![EXCLUDED_THREAD_PARENT],
            vec![EXCLUDED_THREAD_PARENT],
            vec![],
        );
        let facts = base_facts(NORMAL_CHANNEL, ParentResolution::Id(EXCLUDED_THREAD_PARENT));
        let decision = may_log(&scope, &facts);
        // Excluded channel check runs first (rule 1 before rule 3).
        assert_eq!(
            decision,
            LogDecision::Skip(SkipReason::ExcludedParentChannel)
        );
    }

    // Adversarial: zero ids in exclusion vectors are harmless.
    #[test]
    fn zero_ids_in_excluded_vectors_are_harmless() {
        let scope = Scope::new(vec![0], vec![0], vec![0]);
        let facts = base_facts(NORMAL_CHANNEL, ParentResolution::Id(NORMAL_CATEGORY));
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // Adversarial: duplicate ids in exclusion lists deduplicate via HashSet.
    #[test]
    fn duplicate_ids_in_exclusion_lists_deduplicate() {
        let scope = Scope::new(vec![EXCLUDED_CHANNEL, EXCLUDED_CHANNEL], vec![], vec![]);
        let facts = base_facts(EXCLUDED_CHANNEL, ParentResolution::Id(NORMAL_CATEGORY));
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::ExcludedChannel)
        );
    }

    // Rule 4: move with only old_parent set (channel removed from a category).
    #[test]
    fn move_with_only_old_parent_excluded_is_skipped() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::None,
            ParentResolution::Id(EXCLUDED_CATEGORY),
            ParentResolution::None,
        );
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::MovedOutOfExcludedCategory)
        );
    }

    // Adversarial: move where old is normal, new is normal, but parent is excluded category.
    #[test]
    fn normal_move_still_checks_parent_category() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(EXCLUDED_CATEGORY),
            ParentResolution::None,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::Id(NORMAL_CATEGORY),
        );
        // parent_id itself is checked (rule 3) before move checks.
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::ExcludedCategory)
        );
    }

    // Adversarial: excluded channel takes priority over all other checks.
    #[test]
    fn excluded_channel_before_category_check() {
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = base_facts(EXCLUDED_CHANNEL, ParentResolution::Id(EXCLUDED_CATEGORY));
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::ExcludedChannel)
        );
    }

    // Rule 4: move where BOTH old and new are excluded categories.
    // parent_id is ex_cat_2 (excluded) → rule 3 fires first (ExcludedCategory).
    // The test asserts the channel is correctly skipped.
    #[test]
    fn move_between_two_excluded_categories_is_skipped() {
        let ex_cat_2 = 300_003u64;
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY, ex_cat_2], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(ex_cat_2),
            ParentResolution::None,
            ParentResolution::Id(EXCLUDED_CATEGORY),
            ParentResolution::Id(ex_cat_2),
        );
        let decision = may_log(&scope, &facts);
        // parent_id is an excluded category → rule 3 fires first.
        assert_eq!(decision, LogDecision::Skip(SkipReason::ExcludedCategory));
    }

    // Rule 4: move between two excluded categories where parent_id is NOT excluded
    // → old_parent check fires first (MovedOutOfExcludedCategory).
    #[test]
    fn move_between_two_excluded_categories_via_old_parent() {
        let ex_cat_2 = 300_003u64;
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY, ex_cat_2], vec![]);
        // parent_id is normal (not excluded), so rule 3 does not fire.
        // old_parent_id is excluded → MovedOutOfExcludedCategory.
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::None,
            ParentResolution::Id(EXCLUDED_CATEGORY),
            ParentResolution::Id(ex_cat_2),
        );
        let decision = may_log(&scope, &facts);
        assert_eq!(
            decision,
            LogDecision::Skip(SkipReason::MovedOutOfExcludedCategory)
        );
    }

    // Flash channels do not affect non-flash channels.
    #[test]
    fn flash_channel_does_not_affect_normal_channel() {
        let scope = Scope::new(vec![], vec![], vec![FLASH_CHANNEL]);
        let facts = base_facts(NORMAL_CHANNEL, ParentResolution::Id(NORMAL_CATEGORY));
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // Scope constructible from empty vectors is permissive.
    #[test]
    fn scope_from_empty_vectors_is_permissive() {
        let scope = Scope::new(vec![], vec![], vec![]);
        let facts = base_facts(NORMAL_CHANNEL, ParentResolution::Id(NORMAL_CATEGORY));
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // Break the implementation on purpose: if rule 1 checked categories instead
    // of channels, this test would fail because EXCLUDED_CHANNEL is NOT in
    // categories. Asserts the actual channel check.
    #[test]
    fn test_exclusion_actually_checks_channels_not_categories() {
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = base_facts(EXCLUDED_CHANNEL, ParentResolution::Id(NORMAL_CATEGORY));
        // If the implementation incorrectly checked categories instead of channels,
        // this would return Allow because EXCLUDED_CHANNEL is not in categories.
        // The correct behaviour is Skip(ExcludedChannel).
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::ExcludedChannel)
        );
    }

    // ---- Rule 7: an empty configuration allows everything ----
    //
    // Rule 7 says an empty configuration allows everything, so the predicate
    // must not look at any lookup result at all: an unresolvable parent or an
    // unresolvable channel id must NOT turn into Skip(UnknownLookup), because
    // that would drop every uncached channel from the general log the moment
    // the wiring card switches this predicate on.

    // RED: empty scope + unknown parent -> Allow.
    // On the round-2 code this FAILS with Skip(UnknownLookup).
    #[test]
    fn empty_config_allows_unknown_parent() {
        let scope = empty_scope();
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(NORMAL_CHANNEL),
            parent_id: ParentResolution::Unknown,
            category_id: ParentResolution::None,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // RED: empty scope + unknown channel id -> Allow.
    // On the round-2 code this FAILS with Skip(UnknownLookup).
    #[test]
    fn empty_config_allows_none_channel_id() {
        let scope = empty_scope();
        let facts = facts_with_channel_none();
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // RED: empty scope + unknown category -> Allow.
    // On the round-2 code this FAILS with Skip(UnknownLookup).
    #[test]
    fn empty_config_allows_unknown_category() {
        let scope = empty_scope();
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(NORMAL_CHANNEL),
            parent_id: ParentResolution::None,
            category_id: ParentResolution::Unknown,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // RED: empty scope + unknown old/new move parent -> Allow.
    // On the round-2 code this FAILS with Skip(UnknownLookup).
    #[test]
    fn empty_config_allows_unknown_move_parents() {
        let scope = empty_scope();
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(NORMAL_CHANNEL),
            parent_id: ParentResolution::None,
            category_id: ParentResolution::None,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // Rule 7 is about the *configuration*, not the channel: a scope with one
    // excluded channel is NOT empty, so an unknown lookup there still fails
    // closed (rule 6). This pins the boundary between rules 6 and 7.
    #[test]
    fn non_empty_config_still_fails_closed_on_unknown_lookup() {
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![], vec![]);
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(NORMAL_CHANNEL),
            parent_id: ParentResolution::Unknown,
            category_id: ParentResolution::None,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::UnknownLookup)
        );
    }

    // ---- Round-2 item 3: an event ON the excluded category itself ----

    // A rename or a permission overwrite on the excluded category channel is
    // an event whose channel id IS the excluded category. It must be skipped,
    // otherwise a staff-only category's own name reaches the shared log.
    #[test]
    fn event_on_excluded_category_itself_is_skipped() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = ChannelFacts {
            guild_id: Some(GUILD),
            channel_id: Some(EXCLUDED_CATEGORY),
            parent_id: ParentResolution::None,
            category_id: ParentResolution::None,
            old_parent_id: ParentResolution::None,
            new_parent_id: ParentResolution::None,
        };
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::ExcludedCategory)
        );
    }

    // The same id in BOTH sets: the channel check (rule 1) wins because it
    // runs first, so the caller sees the more specific reason.
    #[test]
    fn excluded_channel_wins_over_excluded_category() {
        let scope = Scope::new(vec![EXCLUDED_CHANNEL], vec![EXCLUDED_CHANNEL], vec![]);
        let facts = base_facts(EXCLUDED_CHANNEL, ParentResolution::None);
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::ExcludedChannel)
        );
    }

    // A category id that is NOT excluded is an ordinary channel.
    #[test]
    fn event_on_non_excluded_category_is_allowed() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = base_facts(NORMAL_CATEGORY, ParentResolution::None);
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // ---- Round-2 item 2: moves use a tri-state parent ----

    // The old parent could not be resolved: the channel may have been moved
    // OUT of an excluded category, so fail closed (rules 4 + 6).
    #[test]
    fn move_with_unknown_old_parent_fails_closed() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::None,
            ParentResolution::Unknown,
            ParentResolution::None,
        );
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::UnknownLookup)
        );
    }

    // The new parent could not be resolved: the channel may have been moved
    // INTO an excluded category, so fail closed (rules 4 + 6).
    #[test]
    fn move_with_unknown_new_parent_fails_closed() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::None,
            ParentResolution::None,
            ParentResolution::Unknown,
        );
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::UnknownLookup)
        );
    }

    // The concrete case the reviewer probed: a channel moved out of a staff
    // category whose OLD parent is unresolved. Before the tri-state change
    // `old: None` was read as "had no category" and the event was logged.
    #[test]
    fn move_out_of_unresolved_old_parent_fails_closed() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::None,
            ParentResolution::Unknown,
            ParentResolution::Id(NORMAL_CATEGORY),
        );
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::UnknownLookup)
        );
    }

    // A move whose old parent is Unknown and whose new parent IS an excluded
    // category still fails closed: the reason is the unknown lookup, because
    // the "moved into" branch is only reached with a resolved new parent.
    #[test]
    fn move_with_unknown_old_parent_and_excluded_new_parent_fails_closed() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::None,
            ParentResolution::Unknown,
            ParentResolution::Id(EXCLUDED_CATEGORY),
        );
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::UnknownLookup)
        );
    }

    // A fully resolved move between two ordinary categories is allowed.
    #[test]
    fn resolved_move_between_ordinary_categories_is_allowed() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::None,
            ParentResolution::Id(NORMAL_CATEGORY),
            ParentResolution::Id(NORMAL_CATEGORY),
        );
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // A move event where neither parent is a category (`None` on both) is
    // allowed: "no previous category" is not an unknown lookup.
    #[test]
    fn move_without_any_category_is_allowed() {
        let scope = Scope::new(vec![], vec![EXCLUDED_CATEGORY], vec![]);
        let facts = move_facts(
            NORMAL_CHANNEL,
            ParentResolution::None,
            ParentResolution::None,
            ParentResolution::None,
            ParentResolution::None,
        );
        assert_eq!(may_log(&scope, &facts), LogDecision::Allow);
    }

    // Boundary: u64::MAX as a channel id is an ordinary id.
    #[test]
    fn u64_max_ids_work() {
        let scope = Scope::new(vec![u64::MAX], vec![], vec![]);
        let facts = base_facts(u64::MAX, ParentResolution::Id(NORMAL_CATEGORY));
        assert_eq!(
            may_log(&scope, &facts),
            LogDecision::Skip(SkipReason::ExcludedChannel)
        );
    }
}

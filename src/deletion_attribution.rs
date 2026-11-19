//! Pure decision logic for the "Deleted By" field of the Message Deleted log embed.
use std::collections::{HashMap, VecDeque};

use crate::AUDIT_MATCH_WINDOW_MS;

pub const SEEN_CAP: usize = 256;
pub const LOOKUP_DELAYS_MS: [u64; 3] = [0, 500, 1500];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntryLite {
    pub id: u64,
    pub executor_id: u64,
    pub target_id: Option<u64>,
    pub channel_id: Option<u64>,
    pub count: u64,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditLookupFailed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedBy {
    Executor(u64),
    User,
    BotAuthor,
    AuditUnavailable,
    NotCached,
}

impl DeletedBy {
    pub fn field_text(&self) -> String {
        match self {
            DeletedBy::Executor(id) => format!("<@{id}>"),
            // Owner rule (Ava, 2026-11-18): an audit entry found -> its executor; no entry ->
            // `User (assumed)`, because nothing in the audit log points to anyone else.
            DeletedBy::User => "User (assumed)".to_string(),
            DeletedBy::BotAuthor => "Bot (author)".to_string(),
            DeletedBy::AuditUnavailable => "Unknown (audit log unavailable)".to_string(),
            DeletedBy::NotCached => "Unknown (message not cached)".to_string(),
        }
    }
}

/// How long surplus credits of an entry stay spendable after the bot last saw activity on it
/// (a credit consumed or a count rise). Keeps a burst alive while it is being consumed but lets
/// a stale surplus expire.
pub const CREDIT_TTL_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy)]
struct EntryState {
    given: u64,
    observed: u64,
    active_ms: u64,
}

pub struct LastSeenCounts {
    map: HashMap<u64, EntryState>,
    order: VecDeque<u64>,
    cap: usize,
}

impl LastSeenCounts {
    pub fn new() -> Self {
        Self::with_cap(SEEN_CAP)
    }
    pub fn with_cap(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }
    /// Credits already given for an entry id.
    #[cfg(test)]
    pub fn get(&self, id: u64) -> Option<u64> {
        self.map.get(&id).map(|s| s.given)
    }

    /// Remember an entry as fully given and observed (no recent activity); evicts the
    /// oldest-inserted id past the cap.
    #[cfg(test)]
    pub fn record(&mut self, id: u64, count: u64) {
        self.set(
            id,
            EntryState {
                given: count,
                observed: count,
                active_ms: 0,
            },
        );
    }

    fn state(&self, id: u64) -> Option<EntryState> {
        self.map.get(&id).copied()
    }

    fn set(&mut self, id: u64, state: EntryState) {
        if self.map.insert(id, state).is_none() {
            self.order.push_back(id);
            while self.map.len() > self.cap {
                match self.order.pop_front() {
                    Some(old) => {
                        self.map.remove(&old);
                    }
                    None => break,
                }
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// Decide who deleted a cached message.
///
/// Discord merges repeated deletions into one audit entry whose `count` rises. Each entry has
/// `count` credits; the map records credits already given per entry id. A matching entry (author
/// and channel; action filtered by the caller) with `count` > given has a pending credit, and a
/// returned `Executor` consumes exactly one. A never-seen entry has all credits pending if fresh,
/// else it is baselined as fully given. The map also tracks the last observed count; pending
/// credits are spendable only while the entry is fresh, its count rose, or it was active within
/// `CREDIT_TTL_MS`. Failed lookups consume nothing and are never `User`.
///
/// When several matching entries have a pending credit, an entry whose count ROSE since the
/// last observation wins: the rise is the only per-deletion evidence the audit log gives, so
/// it identifies the executor of this deletion. Only if no entry rose do the leftover credits
/// of older bursts decide (most pending credits, then the freshest entry id). A deletion by
/// one moderator is therefore never credited to a different moderator just because that other
/// moderator left surplus credits behind.
pub fn attribute_deletion(
    author_id: u64,
    author_is_bot: bool,
    channel_id: u64,
    now_ms: u64,
    lookup: Result<&[AuditEntryLite], AuditLookupFailed>,
    seen: &mut LastSeenCounts,
) -> DeletedBy {
    let entries = match lookup {
        Ok(entries) => entries,
        Err(AuditLookupFailed) => return DeletedBy::AuditUnavailable,
    };

    // best = (count rose, pending credits, entry id, executor)
    //
    // The count rise is the only per-deletion evidence Discord gives: an entry whose count
    // increased since the bot last observed it records a NEW deletion by that executor. An
    // entry whose count merely stayed the same only has leftover surplus credits from an
    // earlier burst, so it must never outrank an entry that actually rose - otherwise a
    // moderator's deletion is credited to a different moderator.
    let mut best: Option<(bool, u64, u64, u64)> = None;
    for e in entries {
        let matches = e.target_id == Some(author_id) && e.channel_id == Some(channel_id);
        if !matches {
            continue;
        }
        // Only entries for this author and channel are written to the map, so an unrelated
        // deletion's lookup cannot pre-record someone else's entry.
        let fresh = now_ms.saturating_sub(e.created_ms) <= AUDIT_MATCH_WINDOW_MS;
        let mut st = match seen.state(e.id) {
            Some(st) => st,
            // Fresh: nothing seen yet, all `count` credits pending. Stale: baseline, none pending.
            None if fresh => EntryState {
                given: 0,
                observed: 0,
                active_ms: now_ms,
            },
            None => EntryState {
                given: e.count,
                observed: e.count,
                active_ms: 0,
            },
        };
        let rose = e.count > st.observed;
        if rose {
            st.active_ms = now_ms;
        }
        // A pending credit is spendable only if the entry is fresh by id-age, its count really
        // rose since last observed, or the entry was active within CREDIT_TTL_MS. A stale
        // surplus therefore expires and is never credited.
        let live = fresh || rose || now_ms.saturating_sub(st.active_ms) <= CREDIT_TTL_MS;
        st.observed = st.observed.max(e.count);
        let pending = if live {
            e.count.saturating_sub(st.given)
        } else {
            0
        };
        seen.set(e.id, st);
        if pending == 0 {
            continue;
        }
        if best.is_none_or(|b| (rose, pending, e.id) > (b.0, b.1, b.2)) {
            best = Some((rose, pending, e.id, e.executor_id));
        }
    }

    match best {
        Some((_, _, id, executor)) => {
            // Consume exactly one credit from the chosen entry only.
            if let Some(mut st) = seen.state(id) {
                st.given += 1;
                st.active_ms = now_ms;
                seen.set(id, st);
            }
            DeletedBy::Executor(executor)
        }
        None if author_is_bot => DeletedBy::BotAuthor,
        None => DeletedBy::User,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000_000;
    const AUTHOR: u64 = 111;
    const CHAN: u64 = 222;
    const MOD_A: u64 = 901;
    const MOD_B: u64 = 902;

    fn entry(id: u64, exec: u64, count: u64, age_ms: u64) -> AuditEntryLite {
        AuditEntryLite {
            id,
            executor_id: exec,
            target_id: Some(AUTHOR),
            channel_id: Some(CHAN),
            count,
            created_ms: NOW - age_ms,
        }
    }

    /// An entry with an explicit creation timestamp, for sequences that are observed at a
    /// later point in time than `NOW`.
    fn entry_at(id: u64, exec: u64, count: u64, created_ms: u64) -> AuditEntryLite {
        AuditEntryLite {
            id,
            executor_id: exec,
            target_id: Some(AUTHOR),
            channel_id: Some(CHAN),
            count,
            created_ms,
        }
    }

    fn run(is_bot: bool, entries: &[AuditEntryLite], seen: &mut LastSeenCounts) -> DeletedBy {
        attribute_deletion(AUTHOR, is_bot, CHAN, NOW, Ok(entries), seen)
    }

    #[test]
    fn fresh_matching_entry_is_executor() {
        let mut seen = LastSeenCounts::new();
        let e = [entry(1, MOD_A, 1, 1000)];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        assert_eq!(DeletedBy::Executor(MOD_A).field_text(), "<@901>");
    }

    #[test]
    fn no_entry_is_user_or_bot_author() {
        let mut seen = LastSeenCounts::new();
        assert_eq!(run(false, &[], &mut seen), DeletedBy::User);
        assert_eq!(run(true, &[], &mut seen), DeletedBy::BotAuthor);
        // Owner rule (Ava, 2026-11-18): no audit entry -> the field says `User (assumed)`.
        assert_eq!(DeletedBy::User.field_text(), "User (assumed)");
        assert_eq!(DeletedBy::BotAuthor.field_text(), "Bot (author)");
    }

    /// SF4 / audit CARD-011: a moderator's deletion is credited to the WRONG moderator.
    ///
    /// Exact event sequence:
    /// 1. MOD_A bulk-deletes 5 messages by AUTHOR in CHAN -> audit entry id 10, count 5.
    /// 2. The bot observes that entry: one credit is consumed, MOD_A is credited, and the
    ///    entry keeps 4 surplus credits that stay spendable while it is active.
    /// 3. Nine seconds later MOD_B deletes one more message by AUTHOR in CHAN. Discord
    ///    creates a SEPARATE audit entry id 11, count 1, executor MOD_B.
    /// 4. The bot's next lookup sees both entries. Entry 10's count did NOT change (no new
    ///    deletion by MOD_A), while entry 11's count rose from never-seen to 1 - the direct
    ///    evidence that MOD_B performed the deletion now being logged.
    ///
    /// The tie-break must follow that evidence, otherwise MOD_B's deletion is logged as
    /// MOD_A's: a wrong accusation against a moderator.
    #[test]
    fn deletion_is_credited_to_the_moderator_whose_entry_count_rose() {
        let mut seen = LastSeenCounts::new();

        // Step 1+2: MOD_A's bulk delete is observed and credited. The entry was created
        // 1s before NOW.
        let a_only = [entry_at(10, MOD_A, 5, NOW - 1_000)];
        assert_eq!(run(false, &a_only, &mut seen), DeletedBy::Executor(MOD_A));

        // Step 3+4: MOD_B deletes another message of the same author in the same channel,
        // 9s after the first lookup. Entry 10 is now 10s old (stale by id age) but still
        // active (a credit was just consumed), so it keeps 4 surplus credits; entry 11 was
        // created 0.5s ago and its count rose from never-seen to 1.
        let later = NOW + 9_000;
        let both = [
            entry_at(10, MOD_A, 5, NOW - 1_000), // no new deletion by MOD_A: count unchanged
            entry_at(11, MOD_B, 1, later - 500), // MOD_B deleted a message: count rose
        ];
        let credited = attribute_deletion(AUTHOR, false, CHAN, later, Ok(&both), &mut seen);
        assert_eq!(
            credited,
            DeletedBy::Executor(MOD_B),
            "the deletion whose audit entry count rose must be credited to its executor"
        );
    }

    /// OWNER RULE (Ava, 2026-11-18): an audit entry found -> its executor; no entry -> the
    /// field shows `User (assumed)`, because nothing in the audit log points to anyone else.
    /// The enum variant and the attribution logic stay as they are; only the rendered text
    /// of the unattributed case changes.
    #[test]
    fn unattributed_case_renders_user_assumed_and_attributed_case_renders_the_executor() {
        // Unattributed: no audit entry at all -> `User (assumed)`.
        let mut seen = LastSeenCounts::new();
        assert_eq!(run(false, &[], &mut seen), DeletedBy::User);
        assert_eq!(DeletedBy::User.field_text(), "User (assumed)");

        // Attributed: a matching audit entry still renders its executor.
        let mut seen = LastSeenCounts::new();
        let e = [entry(1, MOD_A, 1, 1000)];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        assert_eq!(DeletedBy::Executor(MOD_A).field_text(), "<@901>");
    }

    #[test]
    fn quick_self_delete_after_moderator_delete_is_not_credited_to_moderator() {
        let mut seen = LastSeenCounts::new();
        let first = [entry(1, MOD_A, 1, 1000)];
        assert_eq!(run(false, &first, &mut seen), DeletedBy::Executor(MOD_A));
        // author self-deletes 3s later: same entry, count unchanged, still inside the 8s window
        let later = [entry(1, MOD_A, 1, 4000)];
        assert_eq!(run(false, &later, &mut seen), DeletedBy::User);
    }

    #[test]
    fn unrelated_lookup_does_not_pre_record_target_entry() {
        let mut seen = LastSeenCounts::new();
        let e = [entry(1, MOD_A, 1, 1000)];
        // a different author's deletion in another channel looks first over the same window
        let other = attribute_deletion(333, false, 444, NOW, Ok(&e), &mut seen);
        assert_eq!(other, DeletedBy::User);
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
    }

    #[test]
    fn stale_entry_with_unchanged_count_is_not_credited() {
        let mut seen = LastSeenCounts::new();
        let e = [entry(1, MOD_A, 1, 60_000)];
        // first look: stale and never seen -> not this deletion
        assert_eq!(run(false, &e, &mut seen), DeletedBy::User);
        // second look, same count -> still User
        assert_eq!(run(false, &e, &mut seen), DeletedBy::User);
    }

    #[test]
    fn aggregated_entry_with_raised_count_is_executor() {
        let mut seen = LastSeenCounts::new();
        let first = [entry(1, MOD_A, 1, 2000)];
        assert_eq!(run(false, &first, &mut seen), DeletedBy::Executor(MOD_A));
        // 30s later the moderator deletes another message: same id, count 2, old id timestamp
        let merged = [entry(1, MOD_A, 2, 30_000)];
        assert_eq!(run(false, &merged, &mut seen), DeletedBy::Executor(MOD_A));
    }

    #[test]
    fn self_delete_after_moderator_delete_is_not_credited_to_moderator() {
        let mut seen = LastSeenCounts::new();
        let first = [entry(1, MOD_A, 1, 2000)];
        assert_eq!(run(false, &first, &mut seen), DeletedBy::Executor(MOD_A));
        // author self-deletes later: entry unchanged (count 1) and stale
        let later = [entry(1, MOD_A, 1, 30_000)];
        assert_eq!(run(false, &later, &mut seen), DeletedBy::User);
    }

    #[test]
    fn other_target_or_channel_or_missing_option_is_ignored() {
        let mut seen = LastSeenCounts::new();
        let mut other_target = entry(1, MOD_A, 1, 500);
        other_target.target_id = Some(AUTHOR + 1);
        let mut other_chan = entry(2, MOD_A, 1, 500);
        other_chan.channel_id = Some(CHAN + 1);
        let mut no_chan = entry(3, MOD_A, 1, 500);
        no_chan.channel_id = None;
        let mut no_target = entry(4, MOD_A, 1, 500);
        no_target.target_id = None;
        let e = [other_target, other_chan, no_chan, no_target];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::User);
    }

    #[test]
    fn two_matching_entries_pick_most_increased_then_freshest() {
        let mut seen = LastSeenCounts::new();
        seen.record(1, 1);
        seen.record(2, 5);
        // entry 1 raised by 2, entry 2 is fresh but not raised -> raised wins
        let e = [entry(2, MOD_B, 5, 500), entry(1, MOD_A, 3, 40_000)];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));

        let mut seen = LastSeenCounts::new();
        // both fresh, nothing raised -> larger (newer) id wins
        let e = [entry(10, MOD_A, 1, 3000), entry(11, MOD_B, 1, 1000)];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_B));
    }

    #[test]
    fn lookup_failure_is_unknown_never_user() {
        let mut seen = LastSeenCounts::new();
        let r = attribute_deletion(AUTHOR, false, CHAN, NOW, Err(AuditLookupFailed), &mut seen);
        assert_eq!(r, DeletedBy::AuditUnavailable);
        let r = attribute_deletion(AUTHOR, true, CHAN, NOW, Err(AuditLookupFailed), &mut seen);
        assert_eq!(r, DeletedBy::AuditUnavailable);
        assert_eq!(
            DeletedBy::AuditUnavailable.field_text(),
            "Unknown (audit log unavailable)"
        );
    }

    #[test]
    fn uncached_label_text() {
        assert_eq!(
            DeletedBy::NotCached.field_text(),
            "Unknown (message not cached)"
        );
    }

    #[test]
    fn rendered_text_has_no_raw_error_text() {
        for v in [
            DeletedBy::User,
            DeletedBy::BotAuthor,
            DeletedBy::AuditUnavailable,
            DeletedBy::NotCached,
            DeletedBy::Executor(5),
        ] {
            let t = v.field_text().to_lowercase();
            assert!(!t.is_empty());
            assert!(!t.contains("error") && !t.contains("http") && !t.contains("403"));
        }
    }

    #[test]
    fn seen_map_is_bounded_and_evicts_oldest() {
        let mut seen = LastSeenCounts::with_cap(3);
        for id in 1..=5 {
            seen.record(id, id);
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(seen.get(1), None);
        assert_eq!(seen.get(2), None);
        assert_eq!(seen.get(5), Some(5));
        // re-recording an existing id does not grow or evict
        seen.record(4, 9);
        assert_eq!(seen.len(), 3);
        assert_eq!(seen.get(3), Some(3));
        assert_eq!(seen.get(4), Some(9));
    }

    #[test]
    fn default_cap_is_bounded() {
        let mut seen = LastSeenCounts::new();
        for id in 0..(SEEN_CAP as u64 * 3) {
            seen.record(id, 1);
        }
        assert_eq!(seen.len(), SEEN_CAP);
    }

    #[test]
    fn retry_delays_are_short() {
        assert!(LOOKUP_DELAYS_MS.iter().sum::<u64>() <= 3000);
        assert_eq!(LOOKUP_DELAYS_MS[0], 0);
    }

    #[test]
    fn burst_of_five_with_count_five_credits_each_then_self_delete_is_user() {
        let mut seen = LastSeenCounts::new();
        let e = [entry(1, MOD_A, 5, 1000)];
        for _ in 0..5 {
            assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        }
        assert_eq!(run(false, &e, &mut seen), DeletedBy::User);
    }

    #[test]
    fn count_rising_across_lookups_credits_each_time() {
        let mut seen = LastSeenCounts::new();
        assert_eq!(
            run(false, &[entry(1, MOD_A, 1, 2000)], &mut seen),
            DeletedBy::Executor(MOD_A)
        );
        assert_eq!(
            run(false, &[entry(1, MOD_A, 2, 20_000)], &mut seen),
            DeletedBy::Executor(MOD_A)
        );
        assert_eq!(
            run(false, &[entry(1, MOD_A, 3, 40_000)], &mut seen),
            DeletedBy::Executor(MOD_A)
        );
        assert_eq!(
            run(false, &[entry(1, MOD_A, 3, 41_000)], &mut seen),
            DeletedBy::User
        );
    }

    #[test]
    fn repeated_lookup_does_not_double_consume_one_credit() {
        let mut seen = LastSeenCounts::new();
        let e = [entry(1, MOD_A, 1, 1000)];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        assert_eq!(run(false, &e, &mut seen), DeletedBy::User);
        assert_eq!(run(true, &e, &mut seen), DeletedBy::BotAuthor);
    }

    #[test]
    fn failed_attempt_and_no_credit_attempts_do_not_consume_credit() {
        let mut seen = LastSeenCounts::new();
        let e = [entry(1, MOD_A, 1, 1000)];
        let r = attribute_deletion(AUTHOR, false, CHAN, NOW, Err(AuditLookupFailed), &mut seen);
        assert_eq!(r, DeletedBy::AuditUnavailable);
        // a retry whose lookup shows no matching entry (other channel) consumes nothing
        let r = attribute_deletion(AUTHOR, false, CHAN + 9, NOW, Ok(&e), &mut seen);
        assert_eq!(r, DeletedBy::User);
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
    }

    #[test]
    fn stale_never_seen_entry_is_not_credited_but_later_count_rise_is() {
        let mut seen = LastSeenCounts::new();
        assert_eq!(
            run(false, &[entry(1, MOD_A, 3, 60_000)], &mut seen),
            DeletedBy::User
        );
        assert_eq!(
            run(false, &[entry(1, MOD_A, 3, 61_000)], &mut seen),
            DeletedBy::User
        );
        assert_eq!(
            run(false, &[entry(1, MOD_A, 4, 70_000)], &mut seen),
            DeletedBy::Executor(MOD_A)
        );
        assert_eq!(
            run(false, &[entry(1, MOD_A, 4, 71_000)], &mut seen),
            DeletedBy::User
        );
    }

    #[test]
    fn unrelated_lookup_first_does_not_suppress_burst_credits() {
        let mut seen = LastSeenCounts::new();
        let e = [entry(1, MOD_A, 3, 1000)];
        assert_eq!(
            attribute_deletion(333, false, 444, NOW, Ok(&e), &mut seen),
            DeletedBy::User
        );
        assert_eq!(
            attribute_deletion(AUTHOR, false, 444, NOW, Ok(&e), &mut seen),
            DeletedBy::User
        );
        for _ in 0..3 {
            assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        }
        assert_eq!(run(false, &e, &mut seen), DeletedBy::User);
    }

    #[test]
    fn two_executors_pick_most_pending_credits_and_credit_only_that_entry() {
        let mut seen = LastSeenCounts::new();
        seen.record(1, 1);
        seen.record(2, 1);
        // entry 1: 3 pending (older), entry 2: 1 pending (fresher)
        let e = [entry(2, MOD_B, 2, 500), entry(1, MOD_A, 4, 30_000)];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        assert_eq!(seen.get(1), Some(2));
        assert_eq!(seen.get(2), Some(1));
        // pending now 2 vs 1: A again, then tie 1 vs 1: freshest (B), then A's last
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_B));
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        assert_eq!(run(false, &e, &mut seen), DeletedBy::User);
    }

    #[test]
    fn credit_map_stays_bounded_and_evicts_oldest_while_crediting() {
        let mut seen = LastSeenCounts::with_cap(3);
        for id in 1..=6u64 {
            let e = [entry(id, MOD_A, 1, 1000)];
            assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(seen.get(1), None);
        assert_eq!(seen.get(6), Some(1));
    }

    fn run_at(now: u64, entries: &[AuditEntryLite], seen: &mut LastSeenCounts) -> DeletedBy {
        attribute_deletion(AUTHOR, false, CHAN, now, Ok(entries), seen)
    }

    const FOUR_HOURS: u64 = 4 * 60 * 60 * 1000;

    #[test]
    fn stale_surplus_credit_is_not_spendable_hours_later() {
        let mut seen = LastSeenCounts::new();
        // first observation while fresh with count 3: one credit consumed, two surplus
        let e = [entry(1, MOD_A, 3, 1000)];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        // 4h later the author self-deletes; entry unchanged and stale
        assert_eq!(run_at(NOW + FOUR_HOURS, &e, &mut seen), DeletedBy::User);
        assert_eq!(run_at(NOW + FOUR_HOURS + 1, &e, &mut seen), DeletedBy::User);
    }

    #[test]
    fn count_rise_on_stale_entry_after_long_gap_is_still_credited() {
        let mut seen = LastSeenCounts::new();
        let e = [entry(1, MOD_A, 3, 1000)];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        let later = NOW + FOUR_HOURS;
        let rose = [entry(1, MOD_A, 4, FOUR_HOURS + 1000)];
        assert_eq!(run_at(later, &rose, &mut seen), DeletedBy::Executor(MOD_A));
        // surplus is only spendable inside the short TTL; hours later it is gone
        assert_eq!(
            run_at(later + FOUR_HOURS, &rose, &mut seen),
            DeletedBy::User
        );
    }

    #[test]
    fn burst_with_stale_id_keeps_crediting_while_credits_are_recent() {
        let mut seen = LastSeenCounts::new();
        // entry id timestamp is 21s old (stale) but the burst is being consumed right now
        let e = [entry(1, MOD_A, 5, 1000)];
        assert_eq!(run(false, &e, &mut seen), DeletedBy::Executor(MOD_A));
        let stale = [entry(1, MOD_A, 5, 21_000)];
        for i in 0..4 {
            assert_eq!(
                run_at(NOW + 20_000 + i, &stale, &mut seen),
                DeletedBy::Executor(MOD_A)
            );
        }
        assert_eq!(run_at(NOW + 20_010, &stale, &mut seen), DeletedBy::User);
    }
}

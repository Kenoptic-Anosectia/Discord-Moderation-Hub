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
            DeletedBy::User => "User".to_string(),
            DeletedBy::BotAuthor => "Bot (author)".to_string(),
            DeletedBy::AuditUnavailable => "Unknown (audit log unavailable)".to_string(),
            DeletedBy::NotCached => "Unknown (message not cached)".to_string(),
        }
    }
}

pub struct LastSeenCounts {
    map: HashMap<u64, u64>,
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
    pub fn get(&self, id: u64) -> Option<u64> {
        self.map.get(&id).copied()
    }

    /// Remember the latest count for an entry id; evicts the oldest-inserted id past the cap.
    pub fn record(&mut self, id: u64, count: u64) {
        if self.map.insert(id, count).is_none() {
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
/// An audit entry (already filtered to the message-delete action by the caller) is THIS deletion
/// when it targets the author in this channel and is either fresh by id-age or its merged `count`
/// rose since we last looked. Only matching entries (author and channel) are recorded on a successful lookup so a later
/// self-delete is not credited to an old moderator entry. A failed lookup is never reported as the
/// author deleting it.
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

    // best = (count increase, entry id, executor)
    let mut best: Option<(u64, u64, u64)> = None;
    for e in entries {
        let matches = e.target_id == Some(author_id) && e.channel_id == Some(channel_id);
        if !matches {
            continue;
        }
        // Only entries for this author and channel are recorded, so an unrelated deletion's
        // lookup cannot pre-record someone else's entry and suppress it for its real target.
        let previous = seen.get(e.id);
        seen.record(e.id, e.count);
        let increase = previous.map_or(0, |p| e.count.saturating_sub(p));
        let fresh = now_ms.saturating_sub(e.created_ms) <= AUDIT_MATCH_WINDOW_MS;
        let already_seen_unchanged = previous.is_some() && increase == 0;
        if !((fresh && !already_seen_unchanged) || increase > 0) {
            continue;
        }
        let key = (increase, e.id, e.executor_id);
        if best.is_none_or(|b| (key.0, key.1) > (b.0, b.1)) {
            best = Some(key);
        }
    }

    match best {
        Some((_, _, executor)) => DeletedBy::Executor(executor),
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
        assert_eq!(DeletedBy::User.field_text(), "User");
        assert_eq!(DeletedBy::BotAuthor.field_text(), "Bot (author)");
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
}

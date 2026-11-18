//! Fail-closed private-scope classification for general (non-ticket) event logging.
//!
//! A channel is private if its id was EVER linked to a ticket row (durable id, survives
//! close/delete/archive), or if it is, or moved from/to, a configured ticket category
//! (covers creation-before-attach and cache misses). The decision runs BEFORE any cache
//! insertion or body/embed/topic/asset construction; the caller keeps all side effects
//! behind it (see `guarded`). This module only decides: it does not log, cache, delete or
//! publish anything, and it does not decide whether a ticket may be opened.
//!
//! Trust boundary: the event payload comes straight from Discord and is untrusted, as are
//! the channel ids, parent ids and guild ids in it. A lookup may fail or miss the cache,
//! so the decision is fail-closed: an event whose privacy is unknown is skipped.

use super::policy::Settings;
use super::repository::Repository;
use serenity::all::{ChannelId, FullEvent};

/// The outcome of the private-scope decision for one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The event may be handled by the general logger.
    Allow,
    /// The event is private, unknown or otherwise unsafe to log: it must be skipped.
    Skip,
}
/// What the channel lookup found for one channel id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// A guild channel/thread; `parent` is its category (or thread parent), if any.
    Guild { parent: Option<u64> },
    /// Definitely not a guild channel (e.g. DM).
    NotGuild,
    /// Cache miss and the remote lookup failed/unavailable.
    Unknown,
}
/// Injected boundary: cache first, then HTTP, in the real adapter.
pub trait ChannelLookup: Send + Sync {
    /// Resolves a channel id to its guild/parent information. Implementations may hit the
    /// network; `Lookup::Unknown` must be returned when the channel cannot be resolved.
    fn lookup(&self, channel: ChannelId) -> impl std::future::Future<Output = Lookup> + Send;
}

/// True when the id is one of the two configured ticket categories.
///
/// The explicit `id != 0` guard keeps Discord's null id (0) from ever matching a
/// category. A configured category id of 0 is already rejected by `Settings::valid`.
fn is_category(s: &Settings, id: u64) -> bool {
    id != 0 && (id == s.support_category || id == s.age_category)
}

/// Channel-bearing parts of an event: (guild if carried, channel ids, parents known from the
/// event payload itself — old AND new for moves).
///
/// `channel_bearing` is false for events that carry no channel at all (members, roles,
/// guilds, ...); those are always allowed through. `parent_known` records that the payload
/// already proves the channel's parent, so no lookup is needed. `uncertain_old` marks a move
/// whose previous parent could not be proven from the payload.
struct Facts {
    guild: Option<u64>,
    channels: Vec<ChannelId>,
    parents: Vec<Option<u64>>,
    /// The event payload already proves what the channel's parent is (create/update).
    parent_known: bool,
    channel_bearing: bool,
    /// A move whose previous parent cannot be proven (cache miss): fail closed.
    uncertain_old: bool,
}
/// Extracts the channel-relevant facts from an event.
///
/// Only the variants listed below are channel-bearing; everything else sets
/// `channel_bearing = false` and is allowed through untouched. For channel/thread
/// create/update/delete the payload carries the parent, so `parent_known` is set and no
/// lookup is required. For a channel move the *old* parent is recorded as well as the new
/// one, and `uncertain_old` is set when the old state is missing.
fn facts(event: &FullEvent) -> Facts {
    let mut f = Facts {
        guild: None,
        channels: vec![],
        parents: vec![],
        parent_known: false,
        channel_bearing: true,
        uncertain_old: false,
    };
    match event {
        FullEvent::Message { new_message } => {
            f.guild = new_message.guild_id.map(|g| g.get());
            f.channels.push(new_message.channel_id);
        }
        FullEvent::MessageUpdate { new, event, .. } => {
            f.guild = event
                .guild_id
                .map(|g| g.get())
                .or_else(|| new.as_ref().and_then(|m| m.guild_id).map(|g| g.get()));
            f.channels.push(event.channel_id);
        }
        FullEvent::MessageDelete {
            channel_id,
            guild_id,
            ..
        }
        | FullEvent::MessageDeleteBulk {
            channel_id,
            guild_id,
            ..
        } => {
            f.guild = guild_id.map(|g| g.get());
            f.channels.push(*channel_id);
        }
        FullEvent::ChannelCreate { channel } => {
            f.guild = Some(channel.guild_id.get());
            f.channels.push(channel.id);
            f.parents.push(channel.parent_id.map(|p| p.get()));
            f.parent_known = true;
        }
        FullEvent::ChannelUpdate { old, new } => {
            f.guild = Some(new.guild_id.get());
            f.channels.push(new.id);
            f.parents.push(new.parent_id.map(|p| p.get()));
            match old {
                Some(o) => f.parents.push(o.parent_id.map(|p| p.get())),
                None => f.uncertain_old = true,
            }
            f.parent_known = true;
        }
        FullEvent::ChannelDelete { channel, .. } => {
            f.guild = Some(channel.guild_id.get());
            f.channels.push(channel.id);
            f.parents.push(channel.parent_id.map(|p| p.get()));
            f.parent_known = true;
        }
        FullEvent::ThreadCreate { thread } | FullEvent::ThreadUpdate { new: thread, .. } => {
            f.guild = Some(thread.guild_id.get());
            f.channels.push(thread.id);
            f.parents.push(thread.parent_id.map(|p| p.get()));
            f.parent_known = true;
        }
        FullEvent::ThreadDelete { thread, .. } => {
            f.guild = Some(thread.guild_id.get());
            f.channels.push(thread.id);
            f.parents.push(Some(thread.parent_id.get()));
            f.parent_known = true;
        }
        FullEvent::InviteCreate { data } => {
            f.guild = data.guild_id.map(|g| g.get());
            f.channels.push(data.channel_id);
        }
        FullEvent::InviteDelete { data } => {
            f.guild = data.guild_id.map(|g| g.get());
            f.channels.push(data.channel_id);
        }
        FullEvent::VoiceStateUpdate { old, new } => {
            f.guild = new.guild_id.map(|g| g.get());
            f.channels.extend(new.channel_id);
            f.channels.extend(old.as_ref().and_then(|o| o.channel_id));
        }
        FullEvent::ReactionAdd { add_reaction: r }
        | FullEvent::ReactionRemove {
            removed_reaction: r,
        } => {
            f.guild = r.guild_id.map(|g| g.get());
            f.channels.push(r.channel_id);
        }
        _ => f.channel_bearing = false,
    }
    f
}

/// `enabled` = tickets feature on. `settings` None while enabled means invalid config.
///
/// Returns `Decision::Skip` (never an error) when the event may belong to a private scope.
/// The checks run in this order and the first one that matches decides:
/// (1) feature disabled -> `Allow`, before any scope or configuration is inspected;
/// (2) event carries no channel -> `Allow`;
/// (3) settings or repository missing -> `Skip` (fail closed on invalid configuration);
/// (4) event from another guild -> `Allow`;
/// (5) a channel id is a known durable ticket -> `Skip`; a durable lookup error -> `Skip`;
/// (6) a carried parent is a ticket category -> `Skip`; a carried parent is a known durable
/// ticket channel -> `Skip`; a durable lookup error on a parent -> `Skip`;
/// (7) the payload's old parent is uncertain -> `Skip`;
/// (8) the payload fully describes the channel -> `Allow`;
/// (9) resolving a message-style channel finds a ticket category or a durable ticket
/// parent -> `Skip`; a contradictory or unknown lookup -> `Skip`;
/// (10) otherwise `Allow`.
///
/// The caller must run this before performing any logging or cache work for the event.
pub async fn decide(
    enabled: bool,
    settings: Option<&Settings>,
    repo: Option<&Repository>,
    event: &FullEvent,
    lookup: &impl ChannelLookup,
) -> Decision {
    if !enabled {
        return Decision::Allow; // tickets disabled: existing behaviour unchanged
    }
    let f = facts(event);
    if !f.channel_bearing {
        return Decision::Allow; // member/server/role/voice/etc. keep normal logging
    }
    let (Some(settings), Some(repo)) = (settings, repo) else {
        return Decision::Skip;
    };
    if let Some(g) = f.guild {
        if g != settings.guild {
            return Decision::Allow; // other guild: not ticket scope
        }
    }
    for ch in &f.channels {
        // (a) durable ticket linkage, including closed/archived/deleted ids.
        match repo.channel_known(ch.get()).await {
            Ok(true) => return Decision::Skip,
            Ok(false) => {}
            // A database error means the linkage cannot be established: fail closed.
            Err(_) => return Decision::Skip,
        }
    }
    // (b) category membership from the payload (old and new parents).
    if f.parents
        .iter()
        .flatten()
        .any(|p| is_category(settings, *p))
    {
        return Decision::Skip;
    }
    // A parent that is itself a durable ticket channel (thread under a ticket) is private;
    // a failed lookup also skips.
    for p in f.parents.iter().flatten() {
        match repo.channel_known(*p).await {
            Ok(false) => {}
            Ok(true) | Err(_) => return Decision::Skip,
        }
    }
    if f.uncertain_old {
        return Decision::Skip;
    }
    if f.parent_known {
        // payload fully describes the channel; ticket-category check above was enough
        // for events whose parent(s) are carried. Unknown old parent on a move is covered
        // by the durable-id check; a brand-new channel has no old parent.
        return Decision::Allow;
    }
    // Message-style events carry only the channel id: resolve its category.
    for ch in &f.channels {
        match lookup.lookup(*ch).await {
            Lookup::Guild { parent } => {
                if parent.is_some_and(|p| is_category(settings, p)) {
                    return Decision::Skip;
                }
                // A thread under a ticket channel reports the ticket channel as parent.
                if let Some(p) = parent {
                    // `unwrap_or(true)` keeps the fail-closed behaviour on a database error.
                    if repo.channel_known(p).await.unwrap_or(true) {
                        return Decision::Skip;
                    }
                }
            }
            Lookup::NotGuild => {
                if f.guild.is_some() {
                    return Decision::Skip; // contradiction => unsafe
                }
            }
            Lookup::Unknown => return Decision::Skip,
        }
    }
    Decision::Allow
}

/// Observable production counters for the guard (skipped vs. let through).
pub struct Counters {
    /// Number of events the guard refused to let through.
    pub skipped: std::sync::atomic::AtomicU64,
    /// Number of events the guard let through.
    pub allowed: std::sync::atomic::AtomicU64,
}
impl Counters {
    /// Creates a counter pair with both counters at zero.
    pub const fn new() -> Self {
        Self {
            skipped: std::sync::atomic::AtomicU64::new(0),
            allowed: std::sync::atomic::AtomicU64::new(0),
        }
    }
}
/// Process-wide counter pair used by the production event handler.
pub static COUNTERS: Counters = Counters::new();
/// Production routing seam: `event_handler` runs ALL cache insertion / body / embed / topic /
/// asset construction and log output inside `sink`, which runs on `Allow` and also on a `Skip`
/// for an interaction event. Interactions are never channel-logging paths and always pass
/// through.
///
/// Returns `None` without running the sink only when the decision is `Skip` and the event is
/// not an interaction; the sink is then not called at all. The sink is called at most once.
/// For an interaction event a `Skip` does not suppress the sink, because interactions are
/// not channel-logging paths.
pub async fn guarded<F, Fut, T>(
    enabled: bool,
    settings: Option<&Settings>,
    repo: Option<&Repository>,
    event: &FullEvent,
    lookup: &impl ChannelLookup,
    counters: &Counters,
    sink: F,
) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    use std::sync::atomic::Ordering::SeqCst;
    if decide(enabled, settings, repo, event, lookup).await == Decision::Skip
        && !matches!(event, FullEvent::InteractionCreate { .. })
    {
        counters.skipped.fetch_add(1, SeqCst);
        return None;
    }
    counters.allowed.fetch_add(1, SeqCst);
    Some(sink().await)
}

/// Real adapter: cache first, then one HTTP fetch. Never logs the error body.
pub struct SerenityLookup<'a>(pub &'a serenity::all::Context);
impl ChannelLookup for SerenityLookup<'_> {
    async fn lookup(&self, channel: ChannelId) -> Lookup {
        use serenity::all::Channel;
        // Cache first: the cache is authoritative for guild membership and avoids a request.
        for g in self.0.cache.guilds() {
            if let Some(c) = self
                .0
                .cache
                .guild(g)
                .and_then(|g| g.channels.get(&channel).cloned())
            {
                return Lookup::Guild {
                    parent: c.parent_id.map(|p| p.get()),
                };
            }
        }
        // Cache miss: exactly one HTTP fetch. A failed fetch is `Unknown`, never a guess.
        match channel.to_channel(self.0).await {
            Ok(Channel::Guild(c)) => Lookup::Guild {
                parent: c.parent_id.map(|p| p.get()),
            },
            Ok(Channel::Private(_)) => Lookup::NotGuild,
            _ => Lookup::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::policy::{Kind, Settings};
    use super::*;
    use serenity::all::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const G: u64 = 100;
    const SUPPORT: u64 = 500;
    const AGE: u64 = 600;
    fn settings() -> Settings {
        Settings {
            guild: G,
            staff: 7,
            support_category: SUPPORT,
            age_category: AGE,
            db_path: ":memory:".into(),
        }
    }
    struct Fake {
        answer: Lookup,
        calls: AtomicUsize,
    }
    impl Fake {
        fn new(a: Lookup) -> Self {
            Self {
                answer: a,
                calls: AtomicUsize::new(0),
            }
        }
    }
    impl ChannelLookup for Fake {
        async fn lookup(&self, _c: ChannelId) -> Lookup {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answer
        }
    }
    fn msg(ch: u64, guild: Option<u64>) -> FullEvent {
        let mut m = Message::default();
        m.channel_id = ChannelId::new(ch);
        m.guild_id = guild.map(GuildId::new);
        FullEvent::Message { new_message: m }
    }
    fn delete(ch: u64) -> FullEvent {
        FullEvent::MessageDelete {
            channel_id: ChannelId::new(ch),
            deleted_message_id: MessageId::new(2),
            guild_id: Some(GuildId::new(G)),
        }
    }
    fn bulk(ch: u64) -> FullEvent {
        FullEvent::MessageDeleteBulk {
            channel_id: ChannelId::new(ch),
            multiple_deleted_messages_ids: vec![MessageId::new(2)],
            guild_id: Some(GuildId::new(G)),
        }
    }
    fn update(ch: u64) -> FullEvent {
        FullEvent::MessageUpdate {
            old_if_available: None,
            new: None,
            event: serde_json::from_value(
                serde_json::json!({"id":"2","channel_id":ch.to_string(),"guild_id":G.to_string()}),
            )
            .unwrap(),
        }
    }
    fn gchan(id: u64, parent: Option<u64>) -> GuildChannel {
        let mut c = GuildChannel::default();
        c.id = ChannelId::new(id);
        c.guild_id = GuildId::new(G);
        c.parent_id = parent.map(ChannelId::new);
        c
    }
    async fn repo_with_ticket(channel: u64) -> Repository {
        let r = Repository::open(":memory:").await.unwrap();
        let t = r.reserve(1, G, 5, Kind::Support).await.unwrap();
        r.claim(t.id).await.unwrap();
        r.attach(t.id, channel).await.unwrap();
        r
    }
    async fn d(repo: &Repository, ev: &FullEvent, l: &Fake) -> Decision {
        decide(true, Some(&settings()), Some(repo), ev, l).await
    }

    /// Proves the disabled feature allows every channel-bearing event and performs no
    /// channel lookup. Fails if the disabled path starts consulting the repo or lookup.
    #[tokio::test]
    async fn disabled_tickets_change_nothing_and_never_touch_lookup() {
        let repo = repo_with_ticket(900).await;
        let l = Fake::new(Lookup::Unknown);
        for ev in [msg(900, Some(G)), delete(900), bulk(900), update(900)] {
            assert_eq!(decide(false, None, None, &ev, &l).await, Decision::Allow);
            assert_eq!(
                decide(false, Some(&settings()), Some(&repo), &ev, &l).await,
                Decision::Allow
            );
        }
        assert_eq!(l.calls.load(Ordering::SeqCst), 0);
    }
    /// Proves a channel id that ever belonged to a ticket stays private in every
    /// message-bearing path, also after close. Fails if the durable linkage stops mattering.
    #[tokio::test]
    async fn durable_ticket_ids_are_private_in_every_message_path_even_on_cache_miss() {
        let repo = repo_with_ticket(900).await;
        let l = Fake::new(Lookup::Guild { parent: None }); // lookup would say "public"
        for ev in [msg(900, Some(G)), delete(900), bulk(900), update(900)] {
            assert_eq!(d(&repo, &ev, &l).await, Decision::Skip);
        }
        // closed + deleted-from-Discord ids stay private
        repo.force_status_for_test(1, "closed").await.unwrap();
        assert_eq!(d(&repo, &delete(900), &l).await, Decision::Skip);
    }
    /// Proves channels inside a configured ticket category are private before any ticket row
    /// links them. Fails if the category check stops applying, or if creation leaks.
    #[tokio::test]
    async fn configured_category_children_are_private_before_attach() {
        let repo = Repository::open(":memory:").await.unwrap(); // no ticket row yet: creation-before-attach
        for cat in [SUPPORT, AGE] {
            let l = Fake::new(Lookup::Guild { parent: Some(cat) });
            for ev in [msg(901, Some(G)), delete(901), bulk(901), update(901)] {
                assert_eq!(d(&repo, &ev, &l).await, Decision::Skip);
            }
            let c = FullEvent::ChannelCreate {
                channel: gchan(901, Some(cat)),
            };
            assert_eq!(d(&repo, &c, &l).await, Decision::Skip);
            let del = FullEvent::ChannelDelete {
                channel: gchan(901, Some(cat)),
                messages: None,
            };
            assert_eq!(d(&repo, &del, &l).await, Decision::Skip);
        }
    }
    /// Proves a channel move is judged on both the old and the new parent.
    /// Fails if only one side of the move is checked.
    #[tokio::test]
    async fn channel_moves_check_old_and_new_category() {
        let repo = Repository::open(":memory:").await.unwrap();
        let l = Fake::new(Lookup::Unknown);
        let into = FullEvent::ChannelUpdate {
            old: Some(gchan(902, Some(1))),
            new: gchan(902, Some(SUPPORT)),
        };
        let out = FullEvent::ChannelUpdate {
            old: Some(gchan(902, Some(AGE))),
            new: gchan(902, Some(1)),
        };
        let pub_ = FullEvent::ChannelUpdate {
            old: Some(gchan(902, Some(1))),
            new: gchan(902, Some(2)),
        };
        assert_eq!(d(&repo, &into, &l).await, Decision::Skip);
        assert_eq!(d(&repo, &out, &l).await, Decision::Skip);
        assert_eq!(d(&repo, &pub_, &l).await, Decision::Allow);
    }
    /// Proves an unresolvable channel fails closed, a non-category parent is allowed, another
    /// guild is out of scope, and non-channel events are never blocked.
    #[tokio::test]
    async fn unknown_target_guild_channel_fails_closed_and_unrelated_logs_normally() {
        let repo = Repository::open(":memory:").await.unwrap();
        let unknown = Fake::new(Lookup::Unknown);
        for ev in [msg(777, Some(G)), delete(777), bulk(777), update(777)] {
            assert_eq!(d(&repo, &ev, &unknown).await, Decision::Skip);
        }
        let public = Fake::new(Lookup::Guild { parent: Some(42) });
        for ev in [msg(777, Some(G)), delete(777), bulk(777), update(777)] {
            assert_eq!(d(&repo, &ev, &public).await, Decision::Allow);
        }
        // other guild: out of ticket scope
        assert_eq!(
            d(&repo, &msg(777, Some(G + 1)), &unknown).await,
            Decision::Allow
        );
        // non-channel events (members/roles/voice/invites) are never blocked, no lookup
        let ev = FullEvent::GuildRoleCreate {
            new: Role::default(),
        };
        let before = unknown.calls.load(Ordering::SeqCst);
        assert_eq!(d(&repo, &ev, &unknown).await, Decision::Allow);
        assert_eq!(unknown.calls.load(Ordering::SeqCst), before);
    }
    /// Proves a missing configuration or repository makes channel-bearing events fail closed
    /// while non-channel events are allowed. Fails if invalid config becomes permissive.
    #[tokio::test]
    async fn invalid_config_or_missing_repo_fails_closed_for_channel_events_only() {
        let l = Fake::new(Lookup::Guild { parent: None });
        assert_eq!(
            decide(true, None, None, &msg(1, Some(G)), &l).await,
            Decision::Skip
        );
        let ev = FullEvent::GuildRoleCreate {
            new: Role::default(),
        };
        assert_eq!(decide(true, None, None, &ev, &l).await, Decision::Allow);
    }
    /// Proves a database failure while checking the durable linkage makes the event fail
    /// closed. Fails if a database error is treated as "not a ticket channel".
    #[tokio::test]
    async fn db_failure_skips() {
        let repo = Repository::open(":memory:").await.unwrap();
        repo.break_for_test().await;
        let l = Fake::new(Lookup::Guild { parent: None });
        assert_eq!(d(&repo, &msg(5, Some(G)), &l).await, Decision::Skip);
    }
    fn thread(id: u64, parent: u64) -> GuildChannel {
        let mut c = gchan(id, Some(parent));
        c.kind = ChannelType::PublicThread;
        c
    }
    /// Proves a thread under a durable ticket channel is private for all thread events.
    /// Fails if the thread parent is not resolved to the durable linkage.
    #[tokio::test]
    async fn thread_events_under_a_durable_ticket_channel_are_private() {
        let repo = repo_with_ticket(900).await;
        let l = Fake::new(Lookup::Guild { parent: None });
        let evs = [
            FullEvent::ThreadCreate {
                thread: thread(950, 900),
            },
            FullEvent::ThreadUpdate {
                old: None,
                new: thread(950, 900),
            },
            FullEvent::ThreadDelete {
                thread: serde_json::from_value(serde_json::json!({
                    "id": "950", "guild_id": G.to_string(), "parent_id": "900", "type": 11
                }))
                .unwrap(),
                full_thread_data: None,
            },
        ];
        for ev in &evs {
            assert_eq!(d(&repo, ev, &l).await, Decision::Skip);
        }
        // a thread under an unrelated channel keeps normal logging
        let ok = FullEvent::ThreadCreate {
            thread: thread(951, 12),
        };
        assert_eq!(d(&repo, &ok, &l).await, Decision::Allow);
    }
    /// Proves thread events also fail closed when the durable linkage lookup fails.
    /// Fails if a database error on the thread path is treated as "public".
    #[tokio::test]
    async fn thread_events_skip_on_db_error() {
        let repo = Repository::open(":memory:").await.unwrap();
        repo.break_for_test().await;
        let l = Fake::new(Lookup::Guild { parent: None });
        let ev = FullEvent::ThreadCreate {
            thread: thread(950, 12),
        };
        assert_eq!(d(&repo, &ev, &l).await, Decision::Skip);
    }
    /// Drives the REAL production seam `guarded` with an injected sink standing in for
    /// log_event (cache insert + log write): returns (sink writes, outcome).
    async fn through_guard(
        enabled: bool,
        repo: Option<&Repository>,
        ev: &FullEvent,
        l: &Fake,
        c: &Counters,
    ) -> (usize, Option<()>) {
        let writes = AtomicUsize::new(0);
        let s = settings();
        let out = guarded(enabled, Some(&s), repo, ev, l, c, || async {
            writes.fetch_add(1, Ordering::SeqCst);
        })
        .await;
        (writes.load(Ordering::SeqCst), out)
    }
    /// Proves the production seam performs no sink work for private, unknown, contradictory
    /// or failing cases. Fails if sink work happens before the decision, or counters drift.
    #[tokio::test]
    async fn guarded_seam_performs_zero_sink_writes_for_private_unknown_and_error_cases() {
        let c = Counters::new();
        let repo = repo_with_ticket(900).await;
        let cases: Vec<(FullEvent, Lookup)> = vec![
            (msg(900, Some(G)), Lookup::Guild { parent: None }), // durable ticket id
            (delete(900), Lookup::Guild { parent: None }),
            (msg(777, Some(G)), Lookup::Unknown), // unknown channel
            (
                msg(777, Some(G)),
                Lookup::Guild {
                    parent: Some(SUPPORT),
                },
            ), // private category
            (msg(777, Some(G)), Lookup::NotGuild), // contradiction
            (
                FullEvent::ThreadCreate {
                    thread: thread(950, 900),
                },
                Lookup::Guild { parent: None },
            ),
            (
                FullEvent::ChannelUpdate {
                    old: None,
                    new: gchan(902, Some(2)),
                },
                Lookup::Unknown,
            ),
        ];
        for (ev, look) in &cases {
            let l = Fake::new(*look);
            let (writes, out) = through_guard(true, Some(&repo), ev, &l, &c).await;
            assert_eq!((writes, out), (0, None));
        }
        // DB error and missing repo/config also fail closed
        let broken = Repository::open(":memory:").await.unwrap();
        broken.break_for_test().await;
        let l = Fake::new(Lookup::Guild { parent: None });
        assert_eq!(
            through_guard(true, Some(&broken), &msg(5, Some(G)), &l, &c).await,
            (0, None)
        );
        let none = guarded(true, None, None, &msg(5, Some(G)), &l, &c, || async {
            panic!("sink must not run")
        })
        .await;
        assert!(none.is_none());
        assert_eq!(c.skipped.load(Ordering::SeqCst), cases.len() as u64 + 2);
        assert_eq!(c.allowed.load(Ordering::SeqCst), 0);
    }
    /// Proves the sink runs exactly once for a public channel and for the disabled feature.
    /// Fails if the sink runs more than once, or if an allowed event is skipped.
    #[tokio::test]
    async fn guarded_seam_runs_sink_exactly_once_for_public_and_disabled() {
        let c = Counters::new();
        let repo = Repository::open(":memory:").await.unwrap();
        let l = Fake::new(Lookup::Guild { parent: Some(42) });
        assert_eq!(
            through_guard(true, Some(&repo), &msg(777, Some(G)), &l, &c).await,
            (1, Some(()))
        );
        assert_eq!(
            through_guard(false, None, &msg(777, Some(G)), &l, &c).await,
            (1, Some(()))
        );
        // other guild + non-channel events pass through without lookups
        let u = Fake::new(Lookup::Unknown);
        assert_eq!(
            through_guard(true, Some(&repo), &msg(5, Some(G + 1)), &u, &c)
                .await
                .0,
            1
        );
        let role = FullEvent::GuildRoleCreate {
            new: Role::default(),
        };
        assert_eq!(through_guard(true, Some(&repo), &role, &u, &c).await.0, 1);
        assert_eq!(u.calls.load(Ordering::SeqCst), 0);
        assert_eq!(c.allowed.load(Ordering::SeqCst), 4);
        assert_eq!(c.skipped.load(Ordering::SeqCst), 0);
    }
    fn invite_create(ch: u64) -> FullEvent {
        FullEvent::InviteCreate {
            data: serde_json::from_value(serde_json::json!({
                "channel_id": ch.to_string(), "guild_id": G.to_string(), "code": "abc",
                "created_at": "2026-01-01T00:00:00Z", "max_age": 0, "max_uses": 0,
                "temporary": false, "uses": 0
            }))
            .unwrap(),
        }
    }
    fn invite_delete(ch: u64) -> FullEvent {
        FullEvent::InviteDelete {
            data: serde_json::from_value(serde_json::json!({
                "channel_id": ch.to_string(), "guild_id": G.to_string(), "code": "abc"
            }))
            .unwrap(),
        }
    }
    fn voice(old: Option<u64>, new: Option<u64>) -> FullEvent {
        let vs = |c: Option<u64>| -> VoiceState {
            serde_json::from_value(serde_json::json!({
                "guild_id": G.to_string(), "channel_id": c.map(|c| c.to_string()),
                "user_id": "5", "session_id": "s", "deaf": false, "mute": false,
                "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false
            }))
            .unwrap()
        };
        FullEvent::VoiceStateUpdate {
            old: old.map(|c| vs(Some(c))),
            new: vs(new),
        }
    }
    fn reaction(ch: u64, add: bool) -> FullEvent {
        let r: Reaction = serde_json::from_value(serde_json::json!({
            "channel_id": ch.to_string(), "message_id": "3", "guild_id": G.to_string(),
            "user_id": "5", "emoji": {"name": "x"}, "burst": false, "burst_colours": [],
            "type": 0
        }))
        .unwrap();
        if add {
            FullEvent::ReactionAdd { add_reaction: r }
        } else {
            FullEvent::ReactionRemove {
                removed_reaction: r,
            }
        }
    }
    /// Proves the invite, voice-state and reaction paths share the same outcomes as the
    /// message paths. Fails if any of these event kinds bypasses the decision.
    #[tokio::test]
    async fn guarded_sink_for_invite_voice_reaction_events_private_unknown_error_public_member() {
        let c = Counters::new();
        let repo = repo_with_ticket(900).await;
        let private = Fake::new(Lookup::Guild { parent: None });
        let unknown = Fake::new(Lookup::Unknown);
        let public = Fake::new(Lookup::Guild { parent: Some(42) });
        let private_events = [
            invite_create(900),
            invite_delete(900),
            voice(None, Some(900)),
            voice(Some(900), None),
            voice(Some(900), Some(777)),
            voice(Some(777), Some(900)),
            reaction(900, true),
            reaction(900, false),
        ];
        for ev in &private_events {
            assert_eq!(
                through_guard(true, Some(&repo), ev, &private, &c).await,
                (0, None)
            );
        }
        let unknown_events = [
            invite_create(777),
            invite_delete(777),
            voice(None, Some(777)),
            voice(Some(777), None),
            reaction(777, true),
            reaction(777, false),
        ];
        for ev in &unknown_events {
            assert_eq!(
                through_guard(true, Some(&repo), ev, &unknown, &c).await,
                (0, None)
            );
        }
        let broken = Repository::open(":memory:").await.unwrap();
        broken.break_for_test().await;
        for ev in [
            invite_create(777),
            voice(Some(1), Some(777)),
            reaction(777, true),
        ] {
            assert_eq!(
                through_guard(true, Some(&broken), &ev, &private, &c).await,
                (0, None)
            );
        }
        for ev in [
            invite_create(777),
            invite_delete(777),
            voice(Some(777), Some(778)),
            reaction(777, true),
            reaction(777, false),
        ] {
            assert_eq!(
                through_guard(true, Some(&repo), &ev, &public, &c).await,
                (1, Some(())),
            );
        }
        // member control (no channel): never blocked, no lookup at all
        let member = FullEvent::GuildMemberRemoval {
            guild_id: GuildId::new(G),
            user: User::default(),
            member_data_if_available: None,
        };
        let before = unknown.calls.load(Ordering::SeqCst);
        assert_eq!(
            through_guard(true, Some(&repo), &member, &unknown, &c).await,
            (1, Some(()))
        );
        assert_eq!(unknown.calls.load(Ordering::SeqCst), before);
    }
    /// Secondary structural source-text check, not a behavioural proof: asserts `main.rs`
    /// mentions the guard before the first cache insertion, and no longer mentions the switch.
    #[test]
    fn runtime_event_handler_calls_the_guard_before_any_cache_insertion() {
        let main = include_str!("../main.rs");
        let h = main.find("async fn event_handler").unwrap();
        let body = &main[h..];
        let guard = body
            .find("tickets::privacy::guarded")
            .expect("handler must call the guard");
        let first_cache = body.find("insert_cache").unwrap();
        // every cache insertion lives in log_event, which is only reachable as the guard's sink
        assert!(guard < first_cache);
        assert!(body.contains("|| log_event(ctx, event, data)"));
        assert_eq!(main.matches("log_event(ctx, event, data)").count(), 1);
        assert!(
            !main.contains("allow_general_events"),
            "global pause must be gone"
        );
    }
}

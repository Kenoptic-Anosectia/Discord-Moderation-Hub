//! Fail-closed private-scope classification for general (non-ticket) event logging.
//!
//! Replaces the temporary global pause. Runs BEFORE any cache insertion, body/embed/topic/
//! asset construction. A channel is private if (a) its id was EVER linked to a ticket row
//! (durable id, survives close/delete/archive), or (b) it is, or moved from/to, a configured
//! ticket category (covers creation-before-attach and cache misses). If the answer cannot be
//! established for a target-guild channel, the event is skipped (never leaked).
use super::policy::Settings;
use super::repository::Repository;
use serenity::all::{ChannelId, FullEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Skip,
}
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
    fn lookup(&self, channel: ChannelId) -> impl std::future::Future<Output = Lookup> + Send;
}

fn is_category(s: &Settings, id: u64) -> bool {
    id != 0 && (id == s.support_category || id == s.age_category)
}

/// Channel-bearing parts of an event: (guild if carried, channel ids, parents known from the
/// event payload itself — old AND new for moves).
struct Facts {
    guild: Option<u64>,
    channels: Vec<ChannelId>,
    parents: Vec<Option<u64>>,
    /// The event payload already proves what the channel's parent is (create/update).
    parent_known: bool,
    channel_bearing: bool,
}
fn facts(event: &FullEvent) -> Facts {
    let mut f = Facts {
        guild: None,
        channels: vec![],
        parents: vec![],
        parent_known: false,
        channel_bearing: true,
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
            if let Some(o) = old {
                f.parents.push(o.parent_id.map(|p| p.get()));
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
        _ => f.channel_bearing = false,
    }
    f
}

/// `enabled` = tickets feature on. `settings` None while enabled means invalid config:
/// fail closed for every channel-bearing event.
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

/// Real adapter: cache first, then one HTTP fetch. Never logs the error body.
pub struct SerenityLookup<'a>(pub &'a serenity::all::Context);
impl ChannelLookup for SerenityLookup<'_> {
    async fn lookup(&self, channel: ChannelId) -> Lookup {
        use serenity::all::Channel;
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
    #[test]
    fn runtime_event_handler_calls_the_guard_before_any_cache_insertion() {
        let main = include_str!("../main.rs");
        let h = main.find("async fn event_handler").unwrap();
        let body = &main[h..];
        let guard = body
            .find("tickets::privacy::decide")
            .expect("handler must call the guard");
        let first_cache = body.find("insert_cache").unwrap();
        assert!(guard < first_cache);
        assert!(
            !main.contains("allow_general_events"),
            "global pause must be gone"
        );
    }
}

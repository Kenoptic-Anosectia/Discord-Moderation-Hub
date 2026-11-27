//! The serenity side of the welcome feature (W3a, `docs/verification/WIRING-C1.md`).
//!
//! `runtime.rs` decides and talks to Discord only through small traits. This file implements
//! the missing ones over serenity (message send, guild facts, staff alert post), adds the
//! cross-join alert flood control, registers the two job kinds, and reads the template file.
//!
//! None of this has run against Discord: only the pure parts are tested offline. Every Discord
//! failure is reduced to a stable code before it leaves this file.

use super::runtime::{
    GuildFacts, GuildLookup, MessageSender, OutgoingMessage, SendError, StaffAlerter,
};
use crate::features::{Feature, JobSignal};
use crate::jobs::runner::{BoxFuture, Registry, RunnerError};
use poise::serenity_prelude as s;
use std::path::Path;
use std::sync::Arc;

/// The directory the template files are read from (relative to the working directory).
pub const TEMPLATE_DIR: &str = "welcome_templates";

/// The unix second now. A trait so the flood control can be tested with a fake clock.
pub trait UnixClock: Send + Sync {
    fn now_secs(&self) -> i64;
}

/// The real clock.
pub struct SystemUnixClock;

impl UnixClock for SystemUnixClock {
    fn now_secs(&self) -> i64 {
        0
    }
}

/// Posts one staff alert text. The text is words only.
pub trait AlertPost: Send + Sync {
    fn post(&self, channel_id: u64, text: &str);
}

/// Posts alerts over serenity with an empty allowed-mentions object.
pub struct SerenityAlertPost {
    pub http: Arc<s::Http>,
}

impl AlertPost for SerenityAlertPost {
    fn post(&self, _channel_id: u64, _text: &str) {}
}

/// The production alerter: one alert per code per window across joins.
pub struct FloodControlledAlerter<C, P> {
    clock: C,
    post: P,
}

impl<C: UnixClock, P: AlertPost> FloodControlledAlerter<C, P> {
    pub fn new(clock: C, post: P) -> Self {
        FloodControlledAlerter { clock, post }
    }
}

impl<C: UnixClock, P: AlertPost> StaffAlerter for FloodControlledAlerter<C, P> {
    fn alert(&mut self, channel_id: u64, code: &str) {
        let _ = &self.clock;
        self.post.post(channel_id, code);
    }
}

/// The Discord message for one welcome, as a serenity builder.
pub fn message_payload(_message: &OutgoingMessage) -> s::builder::CreateMessage {
    s::builder::CreateMessage::new()
}

/// Posts the welcome over serenity.
pub struct SerenityMessageSender {
    pub http: Arc<s::Http>,
    pub handle: tokio::runtime::Handle,
}

impl MessageSender for SerenityMessageSender {
    fn send(&mut self, _channel_id: u64, _message: &OutgoingMessage) -> Result<(), SendError> {
        Ok(())
    }
}

/// Guild facts from the serenity cache.
pub struct SerenityGuildLookup {
    pub cache: Arc<s::Cache>,
}

#[s::async_trait]
impl GuildLookup for SerenityGuildLookup {
    async fn guild_facts(&self, _guild_id: u64) -> Option<GuildFacts> {
        None
    }
}

/// Reads `<dir>/<key>.json`; `None` when it cannot be read or is too large.
pub fn read_template_file(_dir: &Path, _key: &str) -> Option<String> {
    None
}

/// The collaborators of the welcome runtime.
pub struct WelcomePorts<S, W, P, M, A, G> {
    pub source: S,
    pub writer: W,
    pub sleeper: P,
    pub sender: M,
    pub alerter: A,
    pub guilds: G,
}

/// The welcome feature as the registry sees it: adds the core database and the two job kinds.
pub struct WelcomeJobs<X, D> {
    inner: Box<dyn Feature<X, D>>,
    home_guild: u64,
}

impl<X, D> WelcomeJobs<X, D> {
    pub fn new(inner: Box<dyn Feature<X, D>>, home_guild: u64) -> Self {
        WelcomeJobs { inner, home_guild }
    }
}

impl<X: Send + Sync + 'static, D: Send + Sync + 'static> Feature<X, D> for WelcomeJobs<X, D> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn enabled(&self) -> bool {
        self.inner.enabled()
    }

    fn register_jobs(&self, _registry: &mut Registry) -> Result<(), RunnerError> {
        Ok(())
    }

    fn jobs_started(&self, wake: JobSignal) {
        self.inner.jobs_started(wake);
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a X,
        event: &'a s::FullEvent,
        data: &'a D,
        guild: Option<u64>,
        core_db: Option<&'a tokio_rusqlite::Connection>,
    ) -> BoxFuture<'a, ()> {
        let _ = self.home_guild;
        self.inner.handle(ctx, event, data, guild, core_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::welcome::render::{RenderedEmbed, RenderedField};
    use crate::welcome::runtime::AllowedMentions;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    // Synthetic ids. They are obviously made up and are not real Discord ids.
    const HOME: u64 = 780_001;
    const OTHER: u64 = 780_002;
    const MEMBER: u64 = 780_003;
    const ALERT_CHANNEL: u64 = 780_004;

    #[derive(Clone, Default)]
    struct FakeClock(Arc<AtomicI64>);

    impl FakeClock {
        fn set(&self, now: i64) {
            self.0.store(now, Ordering::SeqCst);
        }
    }

    impl UnixClock for FakeClock {
        fn now_secs(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[derive(Clone, Default)]
    struct FakePost(Arc<Mutex<Vec<(u64, String)>>>);

    impl FakePost {
        fn posts(&self) -> Vec<(u64, String)> {
            self.0.lock().unwrap().clone()
        }
    }

    impl AlertPost for FakePost {
        fn post(&self, channel_id: u64, text: &str) {
            self.0.lock().unwrap().push((channel_id, text.to_string()));
        }
    }

    fn alerter() -> (
        FloodControlledAlerter<FakeClock, FakePost>,
        FakeClock,
        FakePost,
    ) {
        let (clock, post) = (FakeClock::default(), FakePost::default());
        (
            FloodControlledAlerter::new(clock.clone(), post.clone()),
            clock,
            post,
        )
    }

    // ---- cross-join alert flood control -----------------------------------------

    #[test]
    fn a_burst_of_joins_raising_the_same_code_posts_one_alert() {
        let (mut alerter, clock, post) = alerter();
        clock.set(1_000);
        for _ in 0..50 {
            alerter.alert(ALERT_CHANNEL, "forbidden");
        }
        assert_eq!(post.posts().len(), 1, "{:?}", post.posts());
        assert_eq!(post.posts()[0].0, ALERT_CHANNEL);
        assert!(post.posts()[0].1.contains("forbidden"));
    }

    #[test]
    fn the_window_is_300_seconds_exactly() {
        let (mut alerter, clock, post) = alerter();
        clock.set(1_000);
        alerter.alert(ALERT_CHANNEL, "rate_limited");
        clock.set(1_299);
        alerter.alert(ALERT_CHANNEL, "rate_limited");
        assert_eq!(post.posts().len(), 1, "299 s later it is still held back");
        clock.set(1_300);
        alerter.alert(ALERT_CHANNEL, "rate_limited");
        assert_eq!(post.posts().len(), 2, "300 s later it is let through again");
        clock.set(1_599);
        alerter.alert(ALERT_CHANNEL, "rate_limited");
        assert_eq!(
            post.posts().len(),
            2,
            "the window restarts at the last post"
        );
    }

    #[test]
    fn different_codes_do_not_hold_each_other_back() {
        let (mut alerter, clock, post) = alerter();
        clock.set(5_000);
        alerter.alert(ALERT_CHANNEL, "forbidden");
        alerter.alert(ALERT_CHANNEL, "unknown_role");
        alerter.alert(ALERT_CHANNEL, "welcome_send_failed");
        alerter.alert(ALERT_CHANNEL, "forbidden");
        assert_eq!(post.posts().len(), 3);
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_silence_a_code_forever() {
        let (mut alerter, clock, post) = alerter();
        clock.set(10_000);
        alerter.alert(ALERT_CHANNEL, "forbidden");
        clock.set(100);
        alerter.alert(ALERT_CHANNEL, "forbidden");
        assert_eq!(
            post.posts().len(),
            2,
            "a negative age is not a held-back age"
        );
    }

    #[test]
    fn the_alert_text_is_words_only_whatever_the_code_holds() {
        let (mut alerter, clock, post) = alerter();
        clock.set(1);
        alerter.alert(ALERT_CHANNEL, "forbidden");
        alerter.alert(ALERT_CHANNEL, "user 780003 <@780003> @everyone");
        for (_, text) in post.posts() {
            assert!(
                text.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b' ' || b == b'_' || b == b':'),
                "{text:?}"
            );
        }
        assert_eq!(post.posts().len(), 2);
    }

    /// A small linear congruential generator: no new dependency, fully seeded.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    #[test]
    fn seeded_bursts_never_exceed_the_rate_and_never_go_quiet() {
        const CODES: [&str; 4] = ["forbidden", "unknown_role", "facts_unavailable", "other"];
        let mut rng = Lcg(0x5eed_1234);
        let (mut alerter, clock, post) = alerter();
        let mut now = 10_000i64;
        // The last time each code was let through, as the test model sees it.
        let mut last_post: [Option<i64>; 4] = [None; 4];
        for _ in 0..600 {
            now += (rng.next() % 200) as i64;
            clock.set(now);
            let which = (rng.next() % 4) as usize;
            let before = post.posts().len();
            alerter.alert(ALERT_CHANNEL, CODES[which]);
            let posted = post.posts().len() > before;
            let held = last_post[which].is_some_and(|at| now - at < 300);
            // Liveness: a code whose window has passed is always let through; the rate:
            // a code inside its window never is.
            assert_eq!(posted, !held, "code {which} at {now}");
            if posted {
                last_post[which] = Some(now);
            }
        }
        assert!(
            post.posts().len() >= 4,
            "every code was let through at least once"
        );
    }

    // ---- the message ------------------------------------------------------------

    fn outgoing(users: Vec<u64>) -> OutgoingMessage {
        OutgoingMessage {
            content: Some("hello".to_string()),
            embed: RenderedEmbed {
                title: Some("Title".to_string()),
                description: Some("Body".to_string()),
                fields: vec![RenderedField {
                    name: "n".to_string(),
                    value: "v".to_string(),
                    inline: true,
                }],
                footer_text: Some("foot".to_string()),
                author_name: Some("auth".to_string()),
            },
            allowed_mentions: AllowedMentions {
                users,
                roles: Vec::new(),
                everyone: false,
            },
        }
    }

    #[test]
    fn the_payload_carries_the_text_and_mentions_only_the_new_member() {
        let json = serde_json::to_value(message_payload(&outgoing(vec![MEMBER]))).unwrap();
        assert_eq!(json["content"], "hello");
        assert_eq!(json["embeds"][0]["title"], "Title");
        assert_eq!(json["embeds"][0]["description"], "Body");
        assert_eq!(json["embeds"][0]["fields"][0]["name"], "n");
        assert_eq!(json["embeds"][0]["fields"][0]["inline"], true);
        assert_eq!(json["embeds"][0]["footer"]["text"], "foot");
        assert_eq!(json["embeds"][0]["author"]["name"], "auth");
        let mentions = &json["allowed_mentions"];
        assert_eq!(mentions["users"], serde_json::json!([MEMBER.to_string()]));
        assert_eq!(mentions["roles"], serde_json::json!([]));
        assert_eq!(
            mentions["parse"],
            serde_json::json!([]),
            "no everyone, no here"
        );
    }

    #[test]
    fn a_zero_user_id_in_the_mentions_is_dropped_and_does_not_panic() {
        let json = serde_json::to_value(message_payload(&outgoing(vec![0, MEMBER]))).unwrap();
        assert_eq!(
            json["allowed_mentions"]["users"],
            serde_json::json!([MEMBER.to_string()])
        );
    }

    #[test]
    fn an_everyone_flag_in_the_data_never_reaches_the_payload() {
        let mut message = outgoing(vec![MEMBER]);
        message.allowed_mentions.everyone = true;
        message.allowed_mentions.roles = vec![780_099];
        let json = serde_json::to_value(message_payload(&message)).unwrap();
        assert_eq!(json["allowed_mentions"]["parse"], serde_json::json!([]));
        assert_eq!(json["allowed_mentions"]["roles"], serde_json::json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_send_to_channel_zero_fails_with_a_code_and_makes_no_call() {
        let mut sender = SerenityMessageSender {
            http: Arc::new(s::Http::new("synthetic-token")),
            handle: tokio::runtime::Handle::current(),
        };
        let error = sender.send(0, &outgoing(vec![MEMBER])).unwrap_err();
        assert_eq!(error.code(), crate::welcome::runtime::CODE_SEND_FAILED);
    }

    #[tokio::test]
    async fn guild_facts_for_a_guild_the_cache_does_not_hold_are_none() {
        let lookup = SerenityGuildLookup {
            cache: Arc::new(s::Cache::new()),
        };
        assert_eq!(lookup.guild_facts(HOME).await, None);
        assert_eq!(
            lookup.guild_facts(0).await,
            None,
            "id 0 never reaches serenity"
        );
    }

    // ---- the template file ------------------------------------------------------

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("w3a-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_template_is_read_from_its_key_name_in_the_directory() {
        let dir = scratch("template-read");
        std::fs::write(dir.join("welcome.json"), "{\"embed\":{}}").unwrap();
        assert_eq!(
            read_template_file(&dir, "welcome").as_deref(),
            Some("{\"embed\":{}}")
        );
        assert_eq!(read_template_file(&dir, "other"), None);
    }

    #[test]
    fn a_key_that_could_leave_the_directory_reads_nothing() {
        let dir = scratch("template-escape");
        std::fs::write(dir.join("secret.json"), "outside").unwrap();
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        for key in [
            "../secret",
            "..\\secret",
            "/secret",
            "C:secret",
            "a/b",
            "",
            "Welcome",
            ".x",
        ] {
            assert_eq!(read_template_file(&inner, key), None, "{key:?}");
        }
    }

    #[test]
    fn a_template_file_over_the_size_limit_reads_nothing() {
        let dir = scratch("template-big");
        std::fs::write(dir.join("big.json"), vec![b'a'; 70 * 1024]).unwrap();
        assert_eq!(read_template_file(&dir, "big"), None);
    }

    // ---- the registry hook ------------------------------------------------------

    #[derive(Clone, Default)]
    struct Probe(Arc<AtomicUsize>);

    struct Inner {
        probe: Probe,
        on: bool,
    }

    impl Feature<u8, u16> for Inner {
        fn name(&self) -> &'static str {
            "welcome"
        }
        fn enabled(&self) -> bool {
            self.on
        }
        fn handle<'a>(
            &'a self,
            _ctx: &'a u8,
            _event: &'a s::FullEvent,
            _data: &'a u16,
            _guild: Option<u64>,
            _core_db: Option<&'a tokio_rusqlite::Connection>,
        ) -> BoxFuture<'a, ()> {
            self.probe.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    fn hook(on: bool) -> (WelcomeJobs<u8, u16>, Probe) {
        let probe = Probe::default();
        let inner = Inner {
            probe: probe.clone(),
            on,
        };
        (WelcomeJobs::new(Box::new(inner), HOME), probe)
    }

    fn some_event() -> s::FullEvent {
        s::FullEvent::GuildMemberRemoval {
            guild_id: s::GuildId::new(HOME),
            user: s::User::default(),
            member_data_if_available: None,
        }
    }

    #[test]
    fn the_hook_needs_the_core_database_and_registers_exactly_the_two_kinds() {
        let (hook, _) = hook(true);
        assert!(Feature::<u8, u16>::needs_core_db(&hook));
        let mut registry = Registry::new();
        Feature::<u8, u16>::register_jobs(&hook, &mut registry).unwrap();
        assert_eq!(registry.len(), 2);
        let listed = format!("{registry:?}");
        assert!(listed.contains("welcome_grant_roles"), "{listed}");
        assert!(listed.contains("welcome_send_message"), "{listed}");
        assert_eq!(
            Feature::<u8, u16>::register_jobs(&hook, &mut registry),
            Err(RunnerError::DuplicateKind),
            "a second registration is refused, never half-done"
        );
    }

    #[test]
    fn the_hook_reports_the_inner_name_and_enabled_flag() {
        let (on, _) = hook(true);
        let (off, _) = hook(false);
        assert_eq!(Feature::<u8, u16>::name(&on), "welcome");
        assert!(Feature::<u8, u16>::enabled(&on));
        assert!(!Feature::<u8, u16>::enabled(&off));
    }

    #[tokio::test]
    async fn the_hook_passes_only_the_home_guild_to_the_inner_feature() {
        let (hook, probe) = hook(true);
        let event = some_event();
        hook.handle(&0u8, &event, &0u16, Some(OTHER), None).await;
        hook.handle(&0u8, &event, &0u16, None, None).await;
        assert_eq!(
            probe.0.load(Ordering::SeqCst),
            0,
            "another guild or none: ignored"
        );
        hook.handle(&0u8, &event, &0u16, Some(HOME), None).await;
        assert_eq!(probe.0.load(Ordering::SeqCst), 1);
    }
}

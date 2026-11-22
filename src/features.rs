//! Feature registry (W1, `docs/verification/WIRING-A.md`).
//!
//! STUB for the RED commit: every decision here is deliberately wrong or empty.

use crate::config::Config;
use crate::jobs::queue::Report;
use crate::jobs::runner::{BoxFuture, Registry, RunnerError};
use crate::privacy_scope::Scope;
use crate::tickets::privacy::ChannelLookup;
use poise::serenity_prelude as serenity;
use std::path::PathBuf;
use std::sync::Arc;

/// A cheap, cloneable callback (wake the job runner, stop the job runner).
pub type JobSignal = Arc<dyn Fn() + Send + Sync>;

/// Which guilds feature dispatch serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuildFilter {
    Any,
    Only(u64),
    Closed,
}

/// What an event says about the guild it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventGuild {
    Guild(u64),
    NoGuild,
    NotGuildBound,
    Unknown,
}

pub fn guild_filter(_guild_id: Option<u64>, _invalid_keys: &[&str]) -> GuildFilter {
    GuildFilter::Any
}

pub fn guild_allows(_filter: GuildFilter, _event: EventGuild) -> bool {
    true
}

pub fn event_guild(_event: &serenity::FullEvent) -> EventGuild {
    EventGuild::Unknown
}

/// Plain facts about one registered feature, for the pure start-up plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeatureInfo {
    pub enabled: bool,
    pub needs_core_db: bool,
}

/// What start-up must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub keep: Vec<bool>,
    pub open_core_db: bool,
}

pub fn plan(infos: &[FeatureInfo]) -> Plan {
    Plan {
        keep: vec![true; infos.len()],
        open_core_db: false,
    }
}

/// Counts of what the start-up reconcile did. Numbers only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReportCounts {
    pub requeued: usize,
    pub abandoned_at_most_once: usize,
    pub abandoned_unregistered: usize,
    pub failed_max_attempts: usize,
}

pub fn report_counts(_report: &Report) -> ReportCounts {
    ReportCounts::default()
}

impl ReportCounts {
    pub fn line(&self) -> String {
        String::new()
    }
}

/// Settings the feature layer needs, taken from `Config`.
#[derive(Clone, Debug)]
pub struct Settings {
    pub guild: GuildFilter,
    pub core_db_path: Option<String>,
    pub reserved_db_paths: Vec<PathBuf>,
    pub staff_alert_channel: Option<u64>,
    pub protected_role_count: usize,
    pub log_excluded_channels: usize,
    pub log_excluded_categories: usize,
}

impl Settings {
    pub fn from_config(_config: &Config) -> Self {
        Settings {
            guild: GuildFilter::Any,
            core_db_path: None,
            reserved_db_paths: Vec::new(),
            staff_alert_channel: None,
            protected_role_count: 0,
            log_excluded_channels: 0,
            log_excluded_categories: 0,
        }
    }

    pub fn summary(&self) -> String {
        String::new()
    }
}

/// What the general logger does with message events.
#[derive(Clone, Debug)]
pub enum LogExclusion {
    Off,
    Scope(Scope),
    BlockMessages,
}

impl LogExclusion {
    pub fn from_config(_config: &Config) -> Self {
        LogExclusion::Off
    }
}

/// Why a message event may not be cached or logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateReason {
    Excluded,
    ConfigInvalid,
}

/// Verdict of the exclusion gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Allow,
    Skip(GateReason),
}

pub async fn message_gate(
    _exclusion: &LogExclusion,
    _event: &serenity::FullEvent,
    _lookup: &impl ChannelLookup,
) -> Gate {
    Gate::Allow
}

/// An optional feature.
pub trait Feature<X, D>: Send + Sync {
    fn name(&self) -> &'static str;
    fn enabled(&self) -> bool;
    fn needs_core_db(&self) -> bool {
        false
    }
    fn register_jobs(&self, _registry: &mut Registry) -> Result<(), RunnerError> {
        Ok(())
    }
    fn jobs_started(&self, _wake: JobSignal) {}
    fn handle<'a>(
        &'a self,
        _ctx: &'a X,
        _event: &'a serenity::FullEvent,
        _data: &'a D,
        _guild: Option<u64>,
        _core_db: Option<&'a tokio_rusqlite::Connection>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// What a started job runner hands back.
pub struct StartedJobs {
    pub counts: ReportCounts,
    pub wake: JobSignal,
    pub shutdown: JobSignal,
    pub finished: BoxFuture<'static, ()>,
}

/// Starts the job runner (injected so tests can fake it).
pub trait JobStarter: Send + Sync {
    fn start(
        &self,
        db: tokio_rusqlite::Connection,
        registry: Registry,
    ) -> BoxFuture<'_, Result<StartedJobs, RunnerError>>;
}

/// Everything `Features::start` needs.
pub struct StartRequest<'a, X, D> {
    pub features: Vec<Box<dyn Feature<X, D>>>,
    pub settings: &'a Settings,
}

/// The features that are registered in production. W1 registers none.
pub fn production_features<X, D>() -> Vec<Box<dyn Feature<X, D>>> {
    Vec::new()
}

/// Starts the real job runner (STUB).
pub struct TokioJobStarter;

impl JobStarter for TokioJobStarter {
    fn start(
        &self,
        _db: tokio_rusqlite::Connection,
        _registry: Registry,
    ) -> BoxFuture<'_, Result<StartedJobs, RunnerError>> {
        Box::pin(async { Err(RunnerError::DbClosed) })
    }
}

/// The registry of enabled features and their shared state.
pub struct Features<X, D> {
    active: Vec<Box<dyn Feature<X, D>>>,
    lines: Vec<String>,
}

impl<X: Sync + 'static, D: Sync + 'static> Features<X, D> {
    pub async fn start<S: JobStarter>(req: StartRequest<'_, X, D>, _starter: &S) -> Self {
        let _ = req.settings;
        Features {
            active: Vec::new(),
            lines: Vec::new(),
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub async fn dispatch(&self, _ctx: &X, _event: &serenity::FullEvent, _data: &D) -> usize {
        let _ = &self.active;
        0
    }

    pub async fn stop(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::queue::{ReconcileEntry, ReconcileOutcome, RestartPolicy};
    use crate::tickets::privacy::Lookup;
    use ::serenity::all::{ChannelId, FullEvent};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ---- synthetic ids (never real Discord ids) ---------------------------
    const GUILD: u64 = 700_001;
    const OTHER_GUILD: u64 = 700_002;
    const CH_PLAIN: u64 = 710_001;
    const CH_EXCLUDED: u64 = 710_002;
    const CH_IN_EXCLUDED_CATEGORY: u64 = 710_003;
    const CH_IN_OTHER_CATEGORY: u64 = 710_004;
    const CAT_EXCLUDED: u64 = 720_001;
    const CAT_OTHER: u64 = 720_002;
    const THREAD_UNDER_EXCLUDED_CHANNEL: u64 = 730_001;
    const THREAD_UNDER_PLAIN: u64 = 730_002;
    const THREAD_UNDER_CHANNEL_IN_EXCLUDED_CATEGORY: u64 = 730_003;
    const CH_UNKNOWN: u64 = 710_099;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("w1-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- synthetic events ---------------------------------------------------
    fn msg(ch: u64, guild: Option<u64>) -> FullEvent {
        let mut m = serenity::Message::default();
        m.id = serenity::MessageId::new(900_010);
        m.channel_id = ChannelId::new(ch);
        m.guild_id = guild.map(serenity::GuildId::new);
        m.content = "SYNTHETIC".to_string();
        FullEvent::Message { new_message: m }
    }

    fn update(ch: u64, guild: Option<u64>) -> FullEvent {
        let mut payload = serde_json::json!({
            "id": "900011", "channel_id": ch.to_string(), "content": "SYNTHETIC",
        });
        if let Some(g) = guild {
            payload["guild_id"] = serde_json::json!(g.to_string());
        }
        FullEvent::MessageUpdate {
            old_if_available: None,
            new: None,
            event: serde_json::from_value(payload).unwrap(),
        }
    }

    fn delete(ch: u64, guild: Option<u64>) -> FullEvent {
        FullEvent::MessageDelete {
            channel_id: ChannelId::new(ch),
            deleted_message_id: serenity::MessageId::new(900_012),
            guild_id: guild.map(serenity::GuildId::new),
        }
    }

    fn bulk_delete(ch: u64, guild: Option<u64>) -> FullEvent {
        FullEvent::MessageDeleteBulk {
            channel_id: ChannelId::new(ch),
            multiple_deleted_messages_ids: vec![serenity::MessageId::new(900_013)],
            guild_id: guild.map(serenity::GuildId::new),
        }
    }

    fn reaction(ch: u64, guild: Option<u64>) -> FullEvent {
        let mut payload = serde_json::json!({
            "channel_id": ch.to_string(), "message_id": "900015", "user_id": "900016",
            "emoji": {"name": "x"}, "burst": false, "burst_colours": [], "type": 0,
        });
        if let Some(g) = guild {
            payload["guild_id"] = serde_json::json!(g.to_string());
        }
        FullEvent::ReactionAdd {
            add_reaction: serde_json::from_value(payload).unwrap(),
        }
    }

    fn voice(guild: Option<u64>) -> FullEvent {
        let mut payload = serde_json::json!({
            "user_id": "900014", "session_id": "synthetic", "deaf": false, "mute": false,
            "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false,
        });
        if let Some(g) = guild {
            payload["guild_id"] = serde_json::json!(g.to_string());
        }
        FullEvent::VoiceStateUpdate {
            old: None,
            new: serde_json::from_value(payload).unwrap(),
        }
    }

    fn not_guild_bound() -> FullEvent {
        FullEvent::ShardsReady { total_shards: 1 }
    }

    fn unclassified() -> FullEvent {
        FullEvent::TypingStart {
            event: serde_json::from_value(serde_json::json!({
                "channel_id": "710001",
                "user_id": "900016",
                "timestamp": 1,
            }))
            .unwrap(),
        }
    }

    // ---- pure: guild filter -----------------------------------------------
    #[test]
    fn guild_filter_is_derived_from_the_setting_and_its_validity() {
        assert_eq!(guild_filter(None, &[]), GuildFilter::Any);
        assert_eq!(guild_filter(Some(GUILD), &[]), GuildFilter::Only(GUILD));
        // A GUILD_ID that was set but unusable must not widen the scope.
        assert_eq!(guild_filter(None, &["GUILD_ID"]), GuildFilter::Closed);
        assert_eq!(
            guild_filter(Some(GUILD), &["GUILD_ID"]),
            GuildFilter::Closed
        );
        // Another unusable key does not touch the guild filter.
        assert_eq!(
            guild_filter(Some(GUILD), &["CORE_DB_PATH"]),
            GuildFilter::Only(GUILD)
        );
        assert_eq!(guild_filter(None, &["CORE_DB_PATH"]), GuildFilter::Any);
    }

    #[test]
    fn guild_allows_table() {
        use EventGuild::*;
        for e in [
            Guild(GUILD),
            Guild(0),
            Guild(u64::MAX),
            NoGuild,
            NotGuildBound,
            Unknown,
        ] {
            assert!(guild_allows(GuildFilter::Any, e), "Any allows {e:?}");
            assert!(
                !guild_allows(GuildFilter::Closed, e),
                "Closed refuses {e:?}"
            );
        }
        let only = GuildFilter::Only(GUILD);
        assert!(guild_allows(only, Guild(GUILD)));
        assert!(guild_allows(only, NotGuildBound));
        for refused in [
            Guild(GUILD + 1),
            Guild(GUILD - 1),
            Guild(OTHER_GUILD),
            Guild(0),
            Guild(u64::MAX),
            NoGuild,
            Unknown,
        ] {
            assert!(!guild_allows(only, refused), "Only refuses {refused:?}");
        }
        // The largest possible id is matched exactly, not by range.
        assert!(guild_allows(GuildFilter::Only(u64::MAX), Guild(u64::MAX)));
        assert!(!guild_allows(
            GuildFilter::Only(u64::MAX),
            Guild(u64::MAX - 1)
        ));
    }

    #[test]
    fn event_guild_reads_the_guild_of_each_classified_event() {
        assert_eq!(
            event_guild(&msg(CH_PLAIN, Some(GUILD))),
            EventGuild::Guild(GUILD)
        );
        assert_eq!(event_guild(&msg(CH_PLAIN, None)), EventGuild::NoGuild);
        assert_eq!(
            event_guild(&update(CH_PLAIN, Some(GUILD))),
            EventGuild::Guild(GUILD)
        );
        assert_eq!(event_guild(&update(CH_PLAIN, None)), EventGuild::NoGuild);
        assert_eq!(
            event_guild(&delete(CH_PLAIN, Some(GUILD))),
            EventGuild::Guild(GUILD)
        );
        assert_eq!(event_guild(&delete(CH_PLAIN, None)), EventGuild::NoGuild);
        assert_eq!(
            event_guild(&bulk_delete(CH_PLAIN, Some(GUILD))),
            EventGuild::Guild(GUILD)
        );
        assert_eq!(
            event_guild(&reaction(CH_PLAIN, Some(GUILD))),
            EventGuild::Guild(GUILD)
        );
        assert_eq!(event_guild(&reaction(CH_PLAIN, None)), EventGuild::NoGuild);
        assert_eq!(event_guild(&voice(Some(GUILD))), EventGuild::Guild(GUILD));
        assert_eq!(event_guild(&voice(None)), EventGuild::NoGuild);
        assert_eq!(event_guild(&not_guild_bound()), EventGuild::NotGuildBound);
        // An event kind the classifier does not know is never assumed to be ours.
        assert_eq!(event_guild(&unclassified()), EventGuild::Unknown);
    }

    // ---- pure: plan ---------------------------------------------------------
    #[test]
    fn plan_opens_the_core_database_only_for_an_enabled_feature_that_needs_it() {
        let info = |enabled, needs_core_db| FeatureInfo {
            enabled,
            needs_core_db,
        };
        assert_eq!(
            plan(&[]),
            Plan {
                keep: vec![],
                open_core_db: false
            }
        );
        assert_eq!(
            plan(&[info(false, true), info(false, false)]),
            Plan {
                keep: vec![false, false],
                open_core_db: false
            }
        );
        assert_eq!(
            plan(&[info(true, false)]),
            Plan {
                keep: vec![true],
                open_core_db: false
            }
        );
        assert_eq!(
            plan(&[info(false, true), info(true, true)]),
            Plan {
                keep: vec![false, true],
                open_core_db: true
            }
        );
        assert_eq!(
            plan(&[info(true, false), info(true, true)]),
            Plan {
                keep: vec![true, true],
                open_core_db: true
            }
        );
    }

    // ---- pure: report counts -----------------------------------------------
    #[test]
    fn report_counts_are_numbers_only() {
        let entry = |id, outcome| ReconcileEntry {
            id,
            kind: "sentinel_kind_name".to_string(),
            outcome,
        };
        let report = Report {
            entries: vec![
                entry(4_242_001, ReconcileOutcome::Requeued),
                entry(4_242_002, ReconcileOutcome::Requeued),
                entry(4_242_003, ReconcileOutcome::AbandonedAtMostOnce),
                entry(4_242_004, ReconcileOutcome::AbandonedUnregistered),
                entry(4_242_005, ReconcileOutcome::FailedMaxAttempts),
                entry(4_242_006, ReconcileOutcome::FailedMaxAttempts),
                entry(4_242_007, ReconcileOutcome::FailedMaxAttempts),
            ],
        };
        let counts = report_counts(&report);
        assert_eq!(
            counts,
            ReportCounts {
                requeued: 2,
                abandoned_at_most_once: 1,
                abandoned_unregistered: 1,
                failed_max_attempts: 3,
            }
        );
        let line = counts.line();
        assert!(line.contains("requeued=2"), "{line}");
        assert!(line.contains("abandoned_at_most_once=1"), "{line}");
        assert!(line.contains("abandoned_unregistered=1"), "{line}");
        assert!(line.contains("failed_max_attempts=3"), "{line}");
        assert!(!line.contains("4242"), "no job id: {line}");
        assert!(!line.contains("sentinel_kind_name"), "no kind: {line}");
        assert_eq!(report_counts(&Report::default()), ReportCounts::default());
    }

    // ---- settings from config ------------------------------------------------
    fn config() -> Config {
        Config {
            discord_token: "synthetic-token".to_string(),
            log_channel_id: 700_010,
            age_role_1: 1,
            age_role_2: 2,
            reaction_role_db_path: "reaction-test.sqlite".to_string(),
            staff_role_id: None,
            tickets: None,
            ticket_archive_retention: crate::tickets::retention::RetentionPolicy::Disabled,
            ticket_privacy_lockdown: false,
            core_db_path: Some("core-test.sqlite".to_string()),
            guild_id: None,
            staff_alert_channel_id: None,
            log_excluded_channel_ids: vec![],
            log_excluded_category_ids: vec![],
            protected_role_ids: vec![],
            invalid_core_keys: vec![],
        }
    }

    #[test]
    fn settings_from_config_copies_the_core_values() {
        let mut c = config();
        c.guild_id = Some(GUILD);
        c.staff_alert_channel_id = Some(4_343_001);
        c.protected_role_ids = vec![1, 2, 3];
        c.log_excluded_channel_ids = vec![5, 6];
        c.log_excluded_category_ids = vec![7];
        c.tickets = Some(crate::tickets::policy::Settings {
            guild: 100,
            staff: 200,
            support_category: 300,
            age_category: 400,
            db_path: "tickets-test.sqlite".to_string(),
        });
        let s = Settings::from_config(&c);
        assert_eq!(s.guild, GuildFilter::Only(GUILD));
        assert_eq!(s.core_db_path.as_deref(), Some("core-test.sqlite"));
        assert_eq!(s.staff_alert_channel, Some(4_343_001));
        assert_eq!(s.protected_role_count, 3);
        assert_eq!(s.log_excluded_channels, 2);
        assert_eq!(s.log_excluded_categories, 1);
        // The files the core database must never alias.
        assert!(s
            .reserved_db_paths
            .contains(&PathBuf::from("reaction-test.sqlite")));
        assert!(s
            .reserved_db_paths
            .contains(&PathBuf::from("tickets-test.sqlite")));
        // The summary carries counts and states, never an id or a path.
        let summary = s.summary();
        assert!(!summary.is_empty());
        for secret in [
            "4343001",
            "700001",
            "core-test",
            "tickets-test",
            "reaction-test",
        ] {
            assert!(!summary.contains(secret), "{secret} leaked: {summary}");
        }
    }

    #[test]
    fn an_unusable_guild_id_closes_the_guild_filter() {
        let mut c = config();
        c.invalid_core_keys = vec!["GUILD_ID"];
        assert_eq!(Settings::from_config(&c).guild, GuildFilter::Closed);
        c.invalid_core_keys = vec![];
        assert_eq!(Settings::from_config(&c).guild, GuildFilter::Any);
    }

    // ---- log exclusion from config ------------------------------------------
    #[test]
    fn log_exclusion_from_config() {
        let c = config();
        assert!(matches!(LogExclusion::from_config(&c), LogExclusion::Off));
        let mut c = config();
        c.log_excluded_channel_ids = vec![CH_EXCLUDED];
        assert!(matches!(
            LogExclusion::from_config(&c),
            LogExclusion::Scope(_)
        ));
        let mut c = config();
        c.log_excluded_category_ids = vec![CAT_EXCLUDED];
        assert!(matches!(
            LogExclusion::from_config(&c),
            LogExclusion::Scope(_)
        ));
        // An exclusion list that could not be read must not mean "nothing excluded".
        for key in ["LOG_EXCLUDED_CHANNEL_IDS", "LOG_EXCLUDED_CATEGORY_IDS"] {
            let mut c = config();
            c.invalid_core_keys = vec![key];
            assert!(
                matches!(LogExclusion::from_config(&c), LogExclusion::BlockMessages),
                "{key}"
            );
        }
        // Other unusable keys do not touch the logger.
        let mut c = config();
        c.invalid_core_keys = vec!["GUILD_ID", "CORE_DB_PATH", "PROTECTED_ROLE_IDS"];
        assert!(matches!(LogExclusion::from_config(&c), LogExclusion::Off));
    }

    // ---- the gate -----------------------------------------------------------
    struct FakeLookup {
        map: HashMap<u64, Lookup>,
        calls: AtomicUsize,
    }

    impl FakeLookup {
        fn new() -> Self {
            let mut map = HashMap::new();
            let guild = |parent: Option<u64>| Lookup::Guild { parent };
            map.insert(CH_PLAIN, guild(None));
            map.insert(CH_EXCLUDED, guild(None));
            map.insert(CH_IN_EXCLUDED_CATEGORY, guild(Some(CAT_EXCLUDED)));
            map.insert(CH_IN_OTHER_CATEGORY, guild(Some(CAT_OTHER)));
            map.insert(CAT_EXCLUDED, guild(None));
            map.insert(CAT_OTHER, guild(None));
            map.insert(THREAD_UNDER_EXCLUDED_CHANNEL, guild(Some(CH_EXCLUDED)));
            map.insert(THREAD_UNDER_PLAIN, guild(Some(CH_PLAIN)));
            map.insert(
                THREAD_UNDER_CHANNEL_IN_EXCLUDED_CATEGORY,
                guild(Some(CH_IN_EXCLUDED_CATEGORY)),
            );
            map.insert(CH_UNKNOWN, Lookup::Unknown);
            FakeLookup {
                map,
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ChannelLookup for FakeLookup {
        async fn lookup(&self, channel: ChannelId) -> Lookup {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.map
                .get(&channel.get())
                .copied()
                .unwrap_or(Lookup::Unknown)
        }
    }

    fn scope_exclusion() -> LogExclusion {
        LogExclusion::Scope(Scope::new(vec![CH_EXCLUDED], vec![CAT_EXCLUDED], vec![]))
    }

    /// The four message events, built for one channel.
    fn message_events(ch: u64) -> Vec<(&'static str, FullEvent)> {
        vec![
            ("create", msg(ch, Some(GUILD))),
            ("update", update(ch, Some(GUILD))),
            ("delete", delete(ch, Some(GUILD))),
            ("bulk_delete", bulk_delete(ch, Some(GUILD))),
        ]
    }

    #[tokio::test]
    async fn gate_with_nothing_configured_allows_without_any_lookup() {
        let lookup = FakeLookup::new();
        for ch in [CH_PLAIN, CH_EXCLUDED, CH_UNKNOWN] {
            for (name, ev) in message_events(ch) {
                assert_eq!(
                    message_gate(&LogExclusion::Off, &ev, &lookup).await,
                    Gate::Allow,
                    "{name}"
                );
            }
        }
        assert_eq!(lookup.calls(), 0, "no cost when the feature is off");
    }

    #[tokio::test]
    async fn an_excluded_channel_is_skipped_for_create_edit_delete_and_bulk_delete() {
        let lookup = FakeLookup::new();
        for (name, ev) in message_events(CH_EXCLUDED) {
            assert_eq!(
                message_gate(&scope_exclusion(), &ev, &lookup).await,
                Gate::Skip(GateReason::Excluded),
                "{name}"
            );
        }
        for (name, ev) in message_events(CH_PLAIN) {
            assert_eq!(
                message_gate(&scope_exclusion(), &ev, &lookup).await,
                Gate::Allow,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn an_excluded_category_is_skipped_and_another_category_is_not() {
        let lookup = FakeLookup::new();
        for (name, ev) in message_events(CH_IN_EXCLUDED_CATEGORY) {
            assert_eq!(
                message_gate(&scope_exclusion(), &ev, &lookup).await,
                Gate::Skip(GateReason::Excluded),
                "{name}"
            );
        }
        for (name, ev) in message_events(CH_IN_OTHER_CATEGORY) {
            assert_eq!(
                message_gate(&scope_exclusion(), &ev, &lookup).await,
                Gate::Allow,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn threads_of_excluded_scopes_are_skipped() {
        let lookup = FakeLookup::new();
        for ch in [
            THREAD_UNDER_EXCLUDED_CHANNEL,
            THREAD_UNDER_CHANNEL_IN_EXCLUDED_CATEGORY,
        ] {
            let g = message_gate(&scope_exclusion(), &msg(ch, Some(GUILD)), &lookup).await;
            assert_eq!(g, Gate::Skip(GateReason::Excluded), "{ch}");
        }
        let g = message_gate(
            &scope_exclusion(),
            &msg(THREAD_UNDER_PLAIN, Some(GUILD)),
            &lookup,
        )
        .await;
        assert_eq!(g, Gate::Allow);
    }

    #[tokio::test]
    async fn an_unresolvable_channel_fails_closed_only_when_exclusions_are_configured() {
        let lookup = FakeLookup::new();
        for (name, ev) in message_events(CH_UNKNOWN) {
            assert_eq!(
                message_gate(&scope_exclusion(), &ev, &lookup).await,
                Gate::Skip(GateReason::Excluded),
                "{name}: unknown channel with exclusions configured"
            );
            assert_eq!(
                message_gate(&LogExclusion::Off, &ev, &lookup).await,
                Gate::Allow,
                "{name}"
            );
        }
        // A channel that resolves but whose parent cannot be resolved is also unknown.
        let mut broken_parent = FakeLookup::new();
        broken_parent.map.insert(
            CH_IN_OTHER_CATEGORY,
            Lookup::Guild {
                parent: Some(CH_UNKNOWN),
            },
        );
        let g = message_gate(
            &scope_exclusion(),
            &msg(CH_IN_OTHER_CATEGORY, Some(GUILD)),
            &broken_parent,
        )
        .await;
        assert_eq!(g, Gate::Skip(GateReason::Excluded));
        // A parent id of 0 is not a channel: fail closed instead of guessing.
        let mut zero_parent = FakeLookup::new();
        zero_parent
            .map
            .insert(CH_PLAIN, Lookup::Guild { parent: Some(0) });
        let g = message_gate(
            &scope_exclusion(),
            &msg(CH_PLAIN, Some(GUILD)),
            &zero_parent,
        )
        .await;
        assert_eq!(g, Gate::Skip(GateReason::Excluded));
        // `NotGuild` for an event that claims a guild is a contradiction: fail closed.
        let mut not_guild = FakeLookup::new();
        not_guild.map.insert(CH_PLAIN, Lookup::NotGuild);
        let g = message_gate(&scope_exclusion(), &msg(CH_PLAIN, Some(GUILD)), &not_guild).await;
        assert_eq!(g, Gate::Skip(GateReason::Excluded));
    }

    #[tokio::test]
    async fn a_message_without_a_guild_is_left_to_the_dm_guard() {
        let lookup = FakeLookup::new();
        for ev in [
            msg(CH_EXCLUDED, None),
            update(CH_EXCLUDED, None),
            delete(CH_EXCLUDED, None),
            bulk_delete(CH_EXCLUDED, None),
        ] {
            assert_eq!(
                message_gate(&scope_exclusion(), &ev, &lookup).await,
                Gate::Allow
            );
        }
        assert_eq!(lookup.calls(), 0);
    }

    #[tokio::test]
    async fn only_message_events_are_gated() {
        let lookup = FakeLookup::new();
        // Reaction roles and voice logging keep working inside excluded channels.
        for ev in [
            reaction(CH_EXCLUDED, Some(GUILD)),
            voice(Some(GUILD)),
            not_guild_bound(),
        ] {
            assert_eq!(
                message_gate(&scope_exclusion(), &ev, &lookup).await,
                Gate::Allow
            );
            assert_eq!(
                message_gate(&LogExclusion::BlockMessages, &ev, &lookup).await,
                Gate::Allow
            );
        }
    }

    #[tokio::test]
    async fn an_unreadable_exclusion_list_blocks_every_guild_message_event() {
        let lookup = FakeLookup::new();
        for ch in [CH_PLAIN, CH_EXCLUDED] {
            for (name, ev) in message_events(ch) {
                assert_eq!(
                    message_gate(&LogExclusion::BlockMessages, &ev, &lookup).await,
                    Gate::Skip(GateReason::ConfigInvalid),
                    "{name}"
                );
            }
        }
        // A DM is still the DM guard's business.
        assert_eq!(
            message_gate(&LogExclusion::BlockMessages, &msg(CH_PLAIN, None), &lookup).await,
            Gate::Allow
        );
        assert_eq!(lookup.calls(), 0);
    }

    // ---- the registry --------------------------------------------------------
    #[derive(Clone)]
    struct Probe {
        events: Arc<Mutex<Vec<String>>>,
        db_seen: Arc<Mutex<Vec<bool>>>,
        wake: Arc<Mutex<Option<JobSignal>>>,
    }

    impl Probe {
        fn new() -> Self {
            Probe {
                events: Arc::new(Mutex::new(Vec::new())),
                db_seen: Arc::new(Mutex::new(Vec::new())),
                wake: Arc::new(Mutex::new(None)),
            }
        }
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    struct TestFeature {
        name: &'static str,
        enabled: bool,
        needs_db: bool,
        kinds: Vec<&'static str>,
        probe: Probe,
    }

    impl TestFeature {
        fn new(
            name: &'static str,
            enabled: bool,
            needs_db: bool,
            kinds: &[&'static str],
            probe: &Probe,
        ) -> Box<dyn Feature<u8, u16>> {
            Box::new(TestFeature {
                name,
                enabled,
                needs_db,
                kinds: kinds.to_vec(),
                probe: probe.clone(),
            })
        }
    }

    impl Feature<u8, u16> for TestFeature {
        fn name(&self) -> &'static str {
            self.name
        }
        fn enabled(&self) -> bool {
            self.enabled
        }
        fn needs_core_db(&self) -> bool {
            self.needs_db
        }
        fn register_jobs(&self, registry: &mut Registry) -> Result<(), RunnerError> {
            for kind in &self.kinds {
                registry.register(kind, RestartPolicy::RunAgain, |_job| async { Ok(()) })?;
            }
            Ok(())
        }
        fn jobs_started(&self, wake: JobSignal) {
            *self.probe.wake.lock().unwrap() = Some(wake);
        }
        fn handle<'a>(
            &'a self,
            ctx: &'a u8,
            _event: &'a FullEvent,
            data: &'a u16,
            guild: Option<u64>,
            core_db: Option<&'a tokio_rusqlite::Connection>,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.probe
                    .events
                    .lock()
                    .unwrap()
                    .push(format!("{}:{ctx}:{data}:{guild:?}", self.name));
                self.probe.db_seen.lock().unwrap().push(core_db.is_some());
            })
        }
    }

    #[derive(Default)]
    struct FakeStarter {
        calls: Mutex<Vec<String>>,
        jobs_table_existed: Mutex<Vec<bool>>,
        fail: bool,
        wakes: Arc<AtomicUsize>,
        shutdowns: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
    }

    impl JobStarter for FakeStarter {
        fn start(
            &self,
            db: tokio_rusqlite::Connection,
            registry: Registry,
        ) -> BoxFuture<'_, Result<StartedJobs, RunnerError>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(format!("{registry:?}"));
                let has_table = db
                    .call(|c| c.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0)))
                    .await
                    .is_ok();
                self.jobs_table_existed.lock().unwrap().push(has_table);
                if self.fail {
                    return Err(RunnerError::DbClosed);
                }
                let (wakes, shutdowns, finished) = (
                    self.wakes.clone(),
                    self.shutdowns.clone(),
                    self.finished.clone(),
                );
                Ok(StartedJobs {
                    counts: ReportCounts {
                        requeued: 1,
                        ..Default::default()
                    },
                    wake: Arc::new(move || {
                        wakes.fetch_add(1, Ordering::SeqCst);
                    }),
                    shutdown: Arc::new(move || {
                        shutdowns.fetch_add(1, Ordering::SeqCst);
                    }),
                    finished: Box::pin(async move {
                        finished.fetch_add(1, Ordering::SeqCst);
                    }),
                })
            })
        }
    }

    fn settings(path: Option<&Path>, guild: GuildFilter) -> Settings {
        Settings {
            guild,
            core_db_path: path.map(|p| p.to_string_lossy().into_owned()),
            reserved_db_paths: vec![],
            staff_alert_channel: None,
            protected_role_count: 0,
            log_excluded_channels: 0,
            log_excluded_categories: 0,
        }
    }

    async fn start(
        features: Vec<Box<dyn Feature<u8, u16>>>,
        settings: &Settings,
        starter: &FakeStarter,
    ) -> Features<u8, u16> {
        Features::start(StartRequest { features, settings }, starter).await
    }

    #[tokio::test]
    async fn with_every_feature_off_nothing_is_created_started_or_delivered() {
        let dir = scratch("all-off");
        let db = dir.join("core.sqlite");
        let probe = Probe::new();
        let starter = FakeStarter::default();
        let s = settings(Some(&db), GuildFilter::Any);
        let features = start(
            vec![
                TestFeature::new("off_db", false, true, &["off_kind"], &probe),
                TestFeature::new("off_plain", false, false, &[], &probe),
            ],
            &s,
            &starter,
        )
        .await;
        assert!(!db.exists(), "core.sqlite must not be created");
        assert!(!dir.join("core.sqlite-wal").exists());
        assert!(
            starter.calls.lock().unwrap().is_empty(),
            "runner not started"
        );
        assert_eq!(
            features.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await,
            0
        );
        assert!(probe.events().is_empty());
        // And with no feature registered at all.
        let none = start(vec![], &s, &starter).await;
        assert!(!db.exists());
        assert_eq!(none.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await, 0);
        assert!(starter.calls.lock().unwrap().is_empty());
        none.stop().await;
        assert_eq!(starter.shutdowns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_feature_that_needs_no_database_creates_none_and_gets_events() {
        let dir = scratch("plain-feature");
        let db = dir.join("core.sqlite");
        let probe = Probe::new();
        let starter = FakeStarter::default();
        let s = settings(Some(&db), GuildFilter::Any);
        let f = start(
            vec![TestFeature::new("plain", true, false, &[], &probe)],
            &s,
            &starter,
        )
        .await;
        assert!(!db.exists());
        assert!(starter.calls.lock().unwrap().is_empty());
        assert_eq!(f.dispatch(&7, &msg(CH_PLAIN, Some(GUILD)), &9).await, 1);
        assert_eq!(probe.events(), vec![format!("plain:7:9:{:?}", Some(GUILD))]);
        assert_eq!(*probe.db_seen.lock().unwrap(), vec![false]);
    }

    #[tokio::test]
    async fn a_feature_that_needs_the_database_opens_it_but_starts_no_runner_without_job_kinds() {
        let dir = scratch("db-no-jobs");
        let db = dir.join("core.sqlite");
        let probe = Probe::new();
        let starter = FakeStarter::default();
        let s = settings(Some(&db), GuildFilter::Any);
        let f = start(
            vec![TestFeature::new("needs_db", true, true, &[], &probe)],
            &s,
            &starter,
        )
        .await;
        assert!(
            db.exists(),
            "the core database is created for an enabled feature that needs it"
        );
        assert!(
            starter.calls.lock().unwrap().is_empty(),
            "no job kind, no runner"
        );
        assert_eq!(f.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await, 1);
        assert_eq!(*probe.db_seen.lock().unwrap(), vec![true]);
    }

    #[tokio::test]
    async fn the_runner_starts_exactly_once_when_a_feature_registers_a_job_kind() {
        let dir = scratch("runner-once");
        let db = dir.join("core.sqlite");
        let probe_a = Probe::new();
        let probe_b = Probe::new();
        let starter = FakeStarter::default();
        let s = settings(Some(&db), GuildFilter::Any);
        let f = start(
            vec![
                TestFeature::new("alpha", true, true, &["alpha_job", "alpha_two"], &probe_a),
                TestFeature::new("beta", true, true, &["beta_job"], &probe_b),
                TestFeature::new("off", false, true, &["never_registered"], &probe_b),
            ],
            &s,
            &starter,
        )
        .await;
        let calls = starter.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "one runner for all features");
        assert_eq!(calls[0], r#"["alpha_job", "alpha_two", "beta_job"]"#);
        assert_eq!(
            *starter.jobs_table_existed.lock().unwrap(),
            vec![true],
            "queue table is ready"
        );
        // The report is printed as counts only.
        assert!(
            f.lines().iter().any(|l| l.contains("requeued=1")),
            "{:?}",
            f.lines()
        );
        // Each feature received the wake callback; calling it reaches the runner.
        let wake = probe_a
            .wake
            .lock()
            .unwrap()
            .clone()
            .expect("wake handed over");
        wake();
        assert_eq!(starter.wakes.load(Ordering::SeqCst), 1);
        assert!(probe_b.wake.lock().unwrap().is_some());
        // Stop: shutdown signalled once, finished awaited once, a second stop is a no-op.
        f.stop().await;
        f.stop().await;
        assert_eq!(starter.shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(starter.finished.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_feature_that_does_not_need_the_database_cannot_register_job_kinds() {
        let dir = scratch("jobs-need-db");
        let db = dir.join("core.sqlite");
        let probe = Probe::new();
        let starter = FakeStarter::default();
        let s = settings(Some(&db), GuildFilter::Any);
        let _f = start(
            vec![TestFeature::new("liar", true, false, &["liar_job"], &probe)],
            &s,
            &starter,
        )
        .await;
        assert!(starter.calls.lock().unwrap().is_empty());
        assert!(!db.exists());
    }

    #[tokio::test]
    async fn an_unusable_core_path_disables_only_the_features_that_need_the_database() {
        let probe_db = Probe::new();
        let probe_plain = Probe::new();
        let starter = FakeStarter::default();
        let s = settings(None, GuildFilter::Any);
        let f = start(
            vec![
                TestFeature::new("needs_db", true, true, &["job_a"], &probe_db),
                TestFeature::new("plain", true, false, &[], &probe_plain),
            ],
            &s,
            &starter,
        )
        .await;
        assert!(starter.calls.lock().unwrap().is_empty());
        assert_eq!(f.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await, 1);
        assert!(probe_db.events().is_empty());
        assert_eq!(probe_plain.events().len(), 1);
        let text = f.lines().join("\n");
        assert!(text.contains("CORE_DB_PATH"), "{text}");
    }

    #[tokio::test]
    async fn a_reserved_core_path_is_refused_with_a_code_and_no_path_in_the_message() {
        let dir = scratch("reserved");
        let reserved = dir.join("reaction-secret-name.sqlite");
        let probe = Probe::new();
        let starter = FakeStarter::default();
        let mut s = settings(Some(&reserved), GuildFilter::Any);
        s.reserved_db_paths = vec![reserved.clone()];
        let f = start(
            vec![TestFeature::new("needs_db", true, true, &["job_a"], &probe)],
            &s,
            &starter,
        )
        .await;
        assert!(
            !reserved.exists(),
            "a reserved file is never opened or created"
        );
        assert!(starter.calls.lock().unwrap().is_empty());
        assert_eq!(f.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await, 0);
        let text = f.lines().join("\n");
        assert!(text.contains("CORE_DB_PATH"), "{text}");
        assert!(text.contains("reserved_path"), "{text}");
        assert!(!text.contains("secret-name"), "path leaked: {text}");
    }

    #[tokio::test]
    async fn a_failing_runner_start_disables_the_database_features_and_prints_a_code() {
        let dir = scratch("starter-fails");
        let db = dir.join("core.sqlite");
        let probe_db = Probe::new();
        let probe_plain = Probe::new();
        let starter = FakeStarter {
            fail: true,
            ..Default::default()
        };
        let s = settings(Some(&db), GuildFilter::Any);
        let f = start(
            vec![
                TestFeature::new("needs_db", true, true, &["job_a"], &probe_db),
                TestFeature::new("plain", true, false, &[], &probe_plain),
            ],
            &s,
            &starter,
        )
        .await;
        assert_eq!(starter.calls.lock().unwrap().len(), 1);
        assert_eq!(f.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await, 1);
        assert!(probe_db.events().is_empty());
        assert_eq!(probe_plain.events().len(), 1);
        let text = f.lines().join("\n");
        assert!(text.contains("db_closed"), "{text}");
        f.stop().await;
        assert_eq!(starter.shutdowns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_feature_whose_job_registration_fails_is_dropped_and_the_rest_still_run() {
        let dir = scratch("bad-kind");
        let db = dir.join("core.sqlite");
        let probe_bad = Probe::new();
        let probe_good = Probe::new();
        let starter = FakeStarter::default();
        let s = settings(Some(&db), GuildFilter::Any);
        let f = start(
            vec![
                TestFeature::new("bad", true, true, &["good_prefix", "Bad Kind!"], &probe_bad),
                TestFeature::new("good", true, true, &["good_job"], &probe_good),
            ],
            &s,
            &starter,
        )
        .await;
        let calls = starter.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![r#"["good_job"]"#.to_string()],
            "no half-registered kinds"
        );
        assert_eq!(f.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await, 1);
        assert!(probe_bad.events().is_empty());
        let text = f.lines().join("\n");
        assert!(text.contains("bad"), "the feature is named: {text}");
        assert!(text.contains("invalid_kind"), "{text}");
        assert!(!text.contains("Bad Kind"), "{text}");
    }

    // ---- dispatch ------------------------------------------------------------
    #[tokio::test]
    async fn dispatch_honours_the_guild_filter() {
        let probe = Probe::new();
        let starter = FakeStarter::default();
        let only = settings(None, GuildFilter::Only(GUILD));
        let f = start(
            vec![TestFeature::new("plain", true, false, &[], &probe)],
            &only,
            &starter,
        )
        .await;
        assert_eq!(
            f.dispatch(&1, &msg(CH_PLAIN, Some(OTHER_GUILD)), &2).await,
            0,
            "other guild"
        );
        assert_eq!(
            f.dispatch(&1, &msg(CH_PLAIN, None), &2).await,
            0,
            "no guild fails closed"
        );
        assert_eq!(
            f.dispatch(&1, &unclassified(), &2).await,
            0,
            "unclassified fails closed"
        );
        assert!(
            probe.events().is_empty(),
            "the feature never saw a foreign event"
        );
        assert_eq!(f.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await, 1);
        assert_eq!(f.dispatch(&1, &not_guild_bound(), &2).await, 1);
        assert_eq!(probe.events().len(), 2);

        let any = settings(None, GuildFilter::Any);
        let probe_any = Probe::new();
        let f = start(
            vec![TestFeature::new("plain", true, false, &[], &probe_any)],
            &any,
            &starter,
        )
        .await;
        assert_eq!(
            f.dispatch(&1, &msg(CH_PLAIN, Some(OTHER_GUILD)), &2).await,
            1
        );
        assert_eq!(f.dispatch(&1, &msg(CH_PLAIN, None), &2).await, 1);

        let closed = settings(None, GuildFilter::Closed);
        let probe_closed = Probe::new();
        let f = start(
            vec![TestFeature::new("plain", true, false, &[], &probe_closed)],
            &closed,
            &starter,
        )
        .await;
        assert_eq!(f.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await, 0);
        assert!(probe_closed.events().is_empty());
    }

    #[tokio::test]
    async fn dispatch_delivers_in_registration_order() {
        let probe = Probe::new();
        let starter = FakeStarter::default();
        let s = settings(None, GuildFilter::Any);
        let f = start(
            vec![
                TestFeature::new("one", true, false, &[], &probe),
                TestFeature::new("two", false, false, &[], &probe),
                TestFeature::new("three", true, false, &[], &probe),
            ],
            &s,
            &starter,
        )
        .await;
        assert_eq!(f.dispatch(&1, &msg(CH_PLAIN, Some(GUILD)), &2).await, 2);
        let names: Vec<String> = probe
            .events()
            .iter()
            .map(|e| e.split(':').next().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["one", "three"]);
    }
}

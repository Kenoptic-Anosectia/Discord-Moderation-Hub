//! Feature registry (W1, `docs/verification/WIRING-A.md`).
//!
//! One place that knows which optional features are enabled, owns the state
//! they share (the core database and the durable job runner) and hands every
//! gateway event to them before the logger sees it.
//!
//! The decisions are pure functions over plain values ([`guild_filter`],
//! [`guild_allows`], [`plan`], [`report_counts`], [`build_facts`]); only
//! [`event_guild`], [`message_gate`], [`Features`] and [`TokioJobStarter`]
//! touch serenity, SQLite or tokio. The registry is generic over the context
//! type `X` and the data type `D` that the bot passes to its handlers, so it
//! can be tested without a Discord connection.
//!
//! No real feature is registered yet (`production_features` is empty): with
//! every feature off the registry opens no database, starts no runner and
//! `dispatch` returns at once.

use crate::config::{
    Config, CORE_DB_PATH_KEY, GUILD_ID_KEY, LOG_EXCLUDED_CATEGORY_IDS_KEY,
    LOG_EXCLUDED_CHANNEL_IDS_KEY, PROTECTED_ROLE_IDS_KEY,
};
use crate::flash::config::{self as flash_config, FlashConfigErrorKind, FlashSettings};
use crate::flash::runtime::{FlashDeps, FlashFeature, SerenityFlashPorts};
use crate::jobs::queue::{self, ReconcileOutcome, Report};
use crate::jobs::runner::{BoxFuture, Registry, Runner, RunnerConfig, RunnerError, SystemClock};
use crate::preflight;
use crate::preflight_wiring::{startup_preflight, FactSource};
use crate::privacy_scope::{self, ChannelFacts, LogDecision, ParentResolution, Scope};
use crate::role_menus::reactions::{
    self, ReactionEnv, ReactionRolesFeature, ReactionServices, SafewordLookup,
};
use crate::roles::grant::{RoleWriter, Sleeper};
use crate::roles::runtime::RoleSource;
use crate::safeword::command::CommandConfig;
use crate::safeword::config::{self as safeword_config, SafewordConfig};
use crate::safeword::discord::{
    RuntimeDeps, SafewordHook, SafewordRuntime, SharedStore, StoreSpec,
};
use crate::settings::{parse_id_list, parse_mode, Mode};
use crate::storage;
use crate::tickets::privacy::{ChannelLookup, Lookup};
use crate::welcome::config::{self as welcome_config, WelcomeConfig};
use crate::welcome::discord::{WelcomeJobs, WelcomePorts};
use crate::welcome::render::{IdMap, WelcomeTemplate};
use crate::welcome::runtime::{
    validate_join_roles, GrantPolicy, GuildLookup, MessageSender, StaffAlerter, WelcomeFeature,
    WelcomeRuntime,
};
use poise::serenity_prelude as serenity;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A cheap, cloneable callback (wake the job runner, stop the job runner).
pub type JobSignal = Arc<dyn Fn() + Send + Sync>;

/// How long `stop` waits beyond the runner's own shutdown grace.
const STOP_MARGIN: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Guild filter (pure)
// ---------------------------------------------------------------------------

/// Which guilds feature dispatch serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuildFilter {
    /// `GUILD_ID` is not set: every guild is served.
    Any,
    /// `GUILD_ID` is set: only this guild is served.
    Only(u64),
    /// `GUILD_ID` was set but is unusable: no event is served.
    Closed,
}

/// What an event says about the guild it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventGuild {
    /// The payload names this guild.
    Guild(u64),
    /// The event kind can carry a guild id and this one does not (a DM).
    NoGuild,
    /// The event kind has no guild by nature (gateway-level events).
    NotGuildBound,
    /// The classifier does not know this event kind.
    Unknown,
}

/// Derives the filter from the parsed `GUILD_ID` and the list of unusable keys.
///
/// A `GUILD_ID` that was set but could not be read closes the filter: a typo
/// must never widen the scope to every guild.
pub fn guild_filter(guild_id: Option<u64>, invalid_keys: &[&str]) -> GuildFilter {
    if invalid_keys.contains(&GUILD_ID_KEY) {
        return GuildFilter::Closed;
    }
    match guild_id {
        Some(id) => GuildFilter::Only(id),
        None => GuildFilter::Any,
    }
}

/// True when a feature may see an event of this guild.
///
/// With a single guild configured only an event that provably belongs to it
/// passes, plus events that have no guild by nature. An event with no guild
/// id (a DM) or of an unclassified kind is not passed.
pub fn guild_allows(filter: GuildFilter, event: EventGuild) -> bool {
    match filter {
        GuildFilter::Any => true,
        GuildFilter::Closed => false,
        GuildFilter::Only(wanted) => match event {
            EventGuild::Guild(id) => id == wanted,
            EventGuild::NotGuildBound => true,
            EventGuild::NoGuild | EventGuild::Unknown => false,
        },
    }
}

/// Reads the guild of an event. Only the event kinds listed here are
/// classified; every other kind is `Unknown` (and so refused by a configured
/// filter) until a feature card that needs it adds it.
pub fn event_guild(event: &serenity::FullEvent) -> EventGuild {
    use serenity::FullEvent as E;
    let maybe = |g: Option<serenity::GuildId>| {
        g.map_or(EventGuild::NoGuild, |g| EventGuild::Guild(g.get()))
    };
    let some = |g: serenity::GuildId| EventGuild::Guild(g.get());
    match event {
        E::Message { new_message } => maybe(new_message.guild_id),
        E::MessageUpdate { event, new, .. } => maybe(
            event
                .guild_id
                .or_else(|| new.as_ref().and_then(|m| m.guild_id)),
        ),
        E::MessageDelete { guild_id, .. } | E::MessageDeleteBulk { guild_id, .. } => {
            maybe(*guild_id)
        }
        E::ReactionAdd { add_reaction } => maybe(add_reaction.guild_id),
        E::ReactionRemove { removed_reaction } => maybe(removed_reaction.guild_id),
        E::VoiceStateUpdate { new, .. } => maybe(new.guild_id),
        E::InviteCreate { data } => maybe(data.guild_id),
        E::InviteDelete { data } => maybe(data.guild_id),
        E::InteractionCreate { interaction } => maybe(interaction.guild_id()),
        E::GuildMemberAddition { new_member } => some(new_member.guild_id),
        E::GuildMemberRemoval { guild_id, .. } => some(*guild_id),
        E::GuildMemberUpdate { event, .. } => some(event.guild_id),
        E::GuildRoleCreate { new } => some(new.guild_id),
        E::GuildRoleUpdate { new, .. } => some(new.guild_id),
        E::GuildRoleDelete { guild_id, .. } => some(*guild_id),
        E::ChannelCreate { channel } => some(channel.guild_id),
        E::ChannelUpdate { new, .. } => some(new.guild_id),
        E::ChannelDelete { channel, .. } => some(channel.guild_id),
        E::CategoryCreate { category } | E::CategoryDelete { category } => some(category.guild_id),
        E::ThreadCreate { thread } => some(thread.guild_id),
        E::ThreadUpdate { new, .. } => some(new.guild_id),
        E::ThreadDelete { thread, .. } => some(thread.guild_id),
        E::GuildCreate { guild, .. } => some(guild.id),
        E::GuildUpdate { new_data, .. } => some(new_data.id),
        E::GuildDelete { incomplete, .. } => some(incomplete.id),
        E::GuildBanAddition { guild_id, .. } | E::GuildBanRemoval { guild_id, .. } => {
            some(*guild_id)
        }
        E::GuildAuditLogEntryCreate { guild_id, .. } => some(*guild_id),
        E::Ready { .. }
        | E::Resume { .. }
        | E::CacheReady { .. }
        | E::ShardsReady { .. }
        | E::ShardStageUpdate { .. }
        | E::UserUpdate { .. } => EventGuild::NotGuildBound,
        _ => EventGuild::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Start-up plan and job-report counts (pure)
// ---------------------------------------------------------------------------

/// Plain facts about one registered feature, for the pure start-up plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeatureInfo {
    pub enabled: bool,
    pub needs_core_db: bool,
}

/// What start-up must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Per feature (same order as the input): keep it.
    pub keep: Vec<bool>,
    /// True only when a kept feature needs the core database.
    pub open_core_db: bool,
}

/// Keeps the enabled features and says whether the core database is needed.
pub fn plan(infos: &[FeatureInfo]) -> Plan {
    Plan {
        keep: infos.iter().map(|i| i.enabled).collect(),
        open_core_db: infos.iter().any(|i| i.enabled && i.needs_core_db),
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

/// Counts the entries of a reconcile report by outcome.
pub fn report_counts(report: &Report) -> ReportCounts {
    let mut counts = ReportCounts::default();
    for entry in &report.entries {
        match entry.outcome {
            ReconcileOutcome::Requeued => counts.requeued += 1,
            ReconcileOutcome::AbandonedAtMostOnce => counts.abandoned_at_most_once += 1,
            ReconcileOutcome::AbandonedUnregistered => counts.abandoned_unregistered += 1,
            ReconcileOutcome::FailedMaxAttempts => counts.failed_max_attempts += 1,
        }
    }
    counts
}

impl ReportCounts {
    /// The start-up line: numbers only, no job id, kind or payload.
    pub fn line(&self) -> String {
        format!(
            "Job runner started; restart reconcile: requeued={} abandoned_at_most_once={} abandoned_unregistered={} failed_max_attempts={}",
            self.requeued,
            self.abandoned_at_most_once,
            self.abandoned_unregistered,
            self.failed_max_attempts
        )
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Settings the feature layer needs, taken from `Config`.
#[derive(Clone, Debug)]
pub struct Settings {
    pub guild: GuildFilter,
    /// `None` when `CORE_DB_PATH` was unusable.
    pub core_db_path: Option<String>,
    /// Files the core database must never alias (ticket and reaction-role files).
    pub reserved_db_paths: Vec<PathBuf>,
    pub staff_alert_channel: Option<u64>,
    pub protected_role_count: usize,
    pub log_excluded_channels: usize,
    pub log_excluded_categories: usize,
}

impl Settings {
    pub fn from_config(config: &Config) -> Self {
        let mut reserved = vec![PathBuf::from(&config.reaction_role_db_path)];
        if let Some(tickets) = &config.tickets {
            reserved.push(PathBuf::from(&tickets.db_path));
        }
        Settings {
            guild: guild_filter(config.guild_id, &config.invalid_core_keys),
            core_db_path: config.core_db_path.clone(),
            reserved_db_paths: reserved,
            staff_alert_channel: config.staff_alert_channel_id,
            protected_role_count: config.protected_role_ids.len(),
            log_excluded_channels: config.log_excluded_channel_ids.len(),
            log_excluded_categories: config.log_excluded_category_ids.len(),
        }
    }

    /// One start-up line with states and counts. It carries no id and no path.
    pub fn summary(&self) -> String {
        let guild = match self.guild {
            GuildFilter::Any => "any",
            GuildFilter::Only(_) => "one",
            GuildFilter::Closed => "closed (GUILD_ID unusable)",
        };
        format!(
            "Feature layer: guild filter={guild}; staff alert channel={}; protected roles={}; log exclusions={} channels, {} categories; core database path={}",
            if self.staff_alert_channel.is_some() { "set" } else { "unset" },
            self.protected_role_count,
            self.log_excluded_channels,
            self.log_excluded_categories,
            if self.core_db_path.is_some() { "usable" } else { "unusable" },
        )
    }
}

// ---------------------------------------------------------------------------
// Log exclusions
// ---------------------------------------------------------------------------

/// What the general logger does with message events.
#[derive(Clone, Debug)]
pub enum LogExclusion {
    /// Nothing is excluded: the logger is exactly as before.
    Off,
    /// Message create, edit, delete and bulk delete in these channels and
    /// categories (and under them) are not cached or logged.
    Scope(Scope),
    /// An exclusion list was unusable: guild message events are not cached or
    /// logged until it is fixed (the list cannot be dropped, see K05).
    BlockMessages,
}

impl LogExclusion {
    pub fn from_config(config: &Config) -> Self {
        Self::from_config_with_flash(config, &[])
    }

    /// Like [`LogExclusion::from_config`], with the flash channel ids added to the scope so the
    /// logger never records flash content. An unusable exclusion list still blocks messages.
    pub fn from_config_with_flash(config: &Config, flash_channels: &[u64]) -> Self {
        let unusable = config
            .invalid_core_keys
            .iter()
            .any(|k| *k == LOG_EXCLUDED_CHANNEL_IDS_KEY || *k == LOG_EXCLUDED_CATEGORY_IDS_KEY);
        if unusable {
            return LogExclusion::BlockMessages;
        }
        if config.log_excluded_channel_ids.is_empty()
            && config.log_excluded_category_ids.is_empty()
            && flash_channels.is_empty()
        {
            return LogExclusion::Off;
        }
        LogExclusion::Scope(Scope::new(
            config.log_excluded_channel_ids.clone(),
            config.log_excluded_category_ids.clone(),
            flash_channels.to_vec(),
        ))
    }
}
/// Why a message event may not be cached or logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateReason {
    /// The channel is, or sits under, an excluded channel or category, or it
    /// could not be resolved while exclusions are configured.
    Excluded,
    /// An exclusion list is unusable.
    ConfigInvalid,
}

/// Verdict of the exclusion gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Allow,
    Skip(GateReason),
}

/// The guild and channel of the four message events the logger caches or logs.
fn message_channel(event: &serenity::FullEvent) -> Option<(Option<u64>, u64)> {
    use serenity::FullEvent as E;
    match event {
        E::Message { new_message } => Some((
            new_message.guild_id.map(|g| g.get()),
            new_message.channel_id.get(),
        )),
        E::MessageUpdate { event, new, .. } => Some((
            event
                .guild_id
                .or_else(|| new.as_ref().and_then(|m| m.guild_id))
                .map(|g| g.get()),
            event.channel_id.get(),
        )),
        E::MessageDelete {
            channel_id,
            guild_id,
            ..
        }
        | E::MessageDeleteBulk {
            channel_id,
            guild_id,
            ..
        } => Some((guild_id.map(|g| g.get()), channel_id.get())),
        _ => None,
    }
}

/// What one lookup says about a channel's parent.
fn parent_of(found: Lookup) -> ParentResolution {
    match found {
        // Discord ids are never 0: a 0 parent is a bad answer, not "no parent".
        Lookup::Guild { parent: Some(0) } => ParentResolution::Unknown,
        Lookup::Guild { parent: Some(id) } => ParentResolution::Id(id),
        Lookup::Guild { parent: None } => ParentResolution::None,
        // A channel an event claims to be in a guild cannot be "not a guild channel".
        Lookup::NotGuild | Lookup::Unknown => ParentResolution::Unknown,
    }
}

/// Builds the predicate's input for a guild message from the lookups.
///
/// `channel` is what the channel lookup said (its parent is the category, or
/// the parent channel of a thread); `parent` is what the lookup of that parent
/// said (its parent is the category of a thread's parent channel). Pass `None`
/// for `parent` only when `channel` has no parent.
pub fn build_facts(
    guild: u64,
    channel_id: u64,
    channel: Lookup,
    parent: Option<Lookup>,
) -> ChannelFacts {
    let parent_id = parent_of(channel);
    let category_id = match (&parent_id, parent) {
        (ParentResolution::Id(_), Some(found)) => parent_of(found),
        (ParentResolution::Id(_), None) => ParentResolution::Unknown,
        (other, _) => other.clone(),
    };
    ChannelFacts {
        guild_id: Some(guild),
        channel_id: Some(channel_id),
        parent_id,
        category_id,
        old_parent_id: ParentResolution::None,
        new_parent_id: ParentResolution::None,
    }
}

async fn lookup_channel(lookup: &impl ChannelLookup, id: u64) -> Lookup {
    if id == 0 {
        return Lookup::Unknown;
    }
    lookup.lookup(serenity::ChannelId::new(id)).await
}

/// Decides whether a message event may be cached or logged.
///
/// Only the four message events are gated; every other event is `Allow`. A
/// message with no guild id (a DM) is left to the DM guard. With nothing
/// configured the answer is `Allow` before any lookup. With exclusions
/// configured, a channel that cannot be resolved is skipped.
pub async fn message_gate(
    exclusion: &LogExclusion,
    event: &serenity::FullEvent,
    lookup: &impl ChannelLookup,
) -> Gate {
    let scope = match exclusion {
        LogExclusion::Off => return Gate::Allow,
        LogExclusion::BlockMessages => {
            return match message_channel(event) {
                Some((Some(_), _)) => Gate::Skip(GateReason::ConfigInvalid),
                _ => Gate::Allow,
            };
        }
        LogExclusion::Scope(scope) => scope,
    };
    let Some((Some(guild), channel_id)) = message_channel(event) else {
        return Gate::Allow;
    };
    let channel = lookup_channel(lookup, channel_id).await;
    let parent = match parent_of(channel) {
        ParentResolution::Id(parent_id) => Some(lookup_channel(lookup, parent_id).await),
        _ => None,
    };
    let facts = build_facts(guild, channel_id, channel, parent);
    match privacy_scope::may_log(scope, &facts) {
        LogDecision::Allow => Gate::Allow,
        LogDecision::Skip(_) => Gate::Skip(GateReason::Excluded),
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// An optional feature.
///
/// A later card implements this for its feature and adds it to
/// [`production_features`]. All methods except `name` and `enabled` default to
/// doing nothing.
pub trait Feature<X, D>: Send + Sync {
    /// Stable name used in start-up lines.
    fn name(&self) -> &'static str;
    /// Whether the feature is switched on (its own parsed setting).
    fn enabled(&self) -> bool;
    /// True when the feature uses the core database. A feature that registers
    /// job kinds must return true: `register_jobs` is only called for those.
    fn needs_core_db(&self) -> bool {
        false
    }
    /// Registers the feature's job kinds with the shared runner.
    fn register_jobs(&self, _registry: &mut Registry) -> Result<(), RunnerError> {
        Ok(())
    }
    /// Called once after the runner started, with a callback that wakes it.
    /// Call it after every job you schedule.
    fn jobs_started(&self, _wake: JobSignal) {}
    /// Called for every event the guild filter lets through, before the
    /// logger. `guild` is the event's guild when it names one; `core_db` is
    /// the shared database for a feature that needs it.
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
    /// Wakes the runner (call after scheduling a job).
    pub wake: JobSignal,
    /// Asks the runner to stop.
    pub shutdown: JobSignal,
    /// Completes when the runner has stopped.
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

/// The features that are registered in production without any setting. None: each
/// configurable feature is added by [`build_features`] from the settings in [`FeaturePlan`].
pub fn production_features<X, D>() -> Vec<Box<dyn Feature<X, D>>> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// Production wiring (W2, `docs/verification/WIRING-B.md`)
// ---------------------------------------------------------------------------

/// Reaction roles, as planned from the settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionRolesPlan {
    pub home_guild: u64,
    pub protected_roles_invalid: bool,
    pub protected_role_ids: Vec<u64>,
    pub safeword_restricted_role_ids: Vec<u64>,
    pub staff_alert_channel: Option<u64>,
}

/// Which optional features are on, read from the environment and `Config`. Pure: no Discord,
/// no I/O. With every `*_MODE` key unset it is empty and `lines` is empty.
#[derive(Clone, Debug)]
pub struct FeaturePlan {
    pub flash: Option<FlashSettings>,
    pub safeword: Option<CommandConfig>,
    pub reaction_roles: Option<ReactionRolesPlan>,
    pub welcome: Option<WelcomePlan>,
    pub staff_alert_channel: Option<u64>,
    /// The home guild (`GUILD_ID`) when it is usable; every optional feature works there only.
    pub home_guild: Option<u64>,
    /// Start-up lines: states, counts and stable codes. Never an id, a path or a value.
    pub lines: Vec<String>,
}

/// The welcome feature, as planned from the settings (W3a, `docs/verification/WIRING-C1.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WelcomePlan {
    /// The parsed `WELCOME_*` and `JOIN_ROLE_IDS` keys. Usable: a channel and a template are set.
    pub config: WelcomeConfig,
    pub home_guild: u64,
    pub channel_id: u64,
    pub protected_role_ids: Vec<u64>,
    pub staff_alert_channel: Option<u64>,
    /// The channels and roles a template may name (`{channel:KEY}`, `{role:KEY}`).
    pub ids: IdMap,
}

impl FeaturePlan {
    /// Flash channel ids the logger must never record (empty unless flash is planned).
    pub fn log_excluded_channels(&self) -> Vec<u64> {
        self.flash
            .as_ref()
            .map(|settings| settings.policy().channel_ids().to_vec())
            .unwrap_or_default()
    }

    /// True when the `/safeword` command is part of the command list.
    pub fn safeword_command(&self) -> bool {
        self.safeword.is_some()
    }

    /// True when the new reaction-role path owns the reaction events (the legacy arms stand down).
    pub fn reaction_roles_owned(&self) -> bool {
        self.reaction_roles.is_some()
    }
}

/// True when the key names a mode other than off. Unset, blank and unknown words are off, as
/// everywhere else, so a feature that is off stays silent even when its other keys are bad.
fn mode_requested(lookup: &dyn Fn(&str) -> Option<String>, key: &str) -> bool {
    parse_mode(key, lookup(key).as_deref()).unwrap_or(Mode::Off) != Mode::Off
}

fn mode_word(mode: Mode) -> &'static str {
    match mode {
        Mode::Off => "off",
        Mode::Shadow => "shadow",
        Mode::Live => "live",
    }
}

fn stays_off(feature: &str, why: &str) -> String {
    format!("Feature {feature} stays off: {why}.")
}

fn plan_flash(plan: &mut FeaturePlan, lookup: &dyn Fn(&str) -> Option<String>, home: Option<u64>) {
    if !mode_requested(lookup, flash_config::KEY_MODE) {
        return;
    }
    if home.is_none() {
        plan.lines.push(stays_off(
            "flash",
            "home guild missing (home_guild_missing)",
        ));
        return;
    }
    let loaded = flash_config::load(lookup, home);
    if let Some(error) = loaded.error {
        let code = match error.kind() {
            FlashConfigErrorKind::Unreadable => "unreadable",
            FlashConfigErrorKind::OutOfRange => "out_of_range",
        };
        plan.lines.push(stays_off(
            "flash",
            &format!("setting refused ({}: {code})", error.key()),
        ));
        return;
    }
    let settings = loaded.settings;
    if !settings.is_active() {
        plan.lines
            .push(stays_off("flash", "no flash channel set (no_channels)"));
        return;
    }
    plan.lines.push(format!(
        "Feature flash on: mode={}; channels={}.",
        mode_word(settings.mode()),
        settings.policy().channel_ids().len()
    ));
    plan.flash = Some(settings);
}

fn plan_safeword(
    plan: &mut FeaturePlan,
    lookup: &dyn Fn(&str) -> Option<String>,
    config: &Config,
    home: Option<u64>,
) {
    if !mode_requested(lookup, safeword_config::KEY_MODE) {
        return;
    }
    if home.is_none() {
        plan.lines.push(stays_off(
            "safeword",
            "home guild missing (home_guild_missing)",
        ));
        return;
    }
    // The safeword writes roles under the protected-role rule; an unusable list cannot be
    // read as "nothing is protected".
    if config.invalid_core_keys.contains(&PROTECTED_ROLE_IDS_KEY) {
        plan.lines.push(stays_off(
            "safeword",
            "protected role list unusable (protected_roles_invalid)",
        ));
        return;
    }
    match SafewordConfig::from_lookup(lookup) {
        Err(error) => plan.lines.push(stays_off(
            "safeword",
            &format!("setting refused ({}: {})", error.key(), error.kind().code()),
        )),
        Ok(parsed) => {
            let staff = if config.staff_role_id.is_some() {
                "staff role=set".to_string()
            } else {
                "staff_role_unset (strike and clear are denied)".to_string()
            };
            plan.lines.push(format!(
                "Feature safeword on: mode={}; restricted roles={}; {staff}.",
                mode_word(parsed.mode),
                parsed.restricted_role_ids.len()
            ));
            plan.safeword = Some(CommandConfig {
                safeword: parsed,
                home_guild: home,
                staff_role_id: config.staff_role_id,
            });
        }
    }
}

fn plan_reaction_roles(
    plan: &mut FeaturePlan,
    lookup: &dyn Fn(&str) -> Option<String>,
    config: &Config,
    home: Option<u64>,
) {
    match reactions::load_enabled(lookup) {
        Ok(false) => return,
        Err(refusal) => {
            plan.lines.push(stays_off("reaction_roles", refusal.code()));
            return;
        }
        Ok(true) => {}
    }
    let Some(home_guild) = home else {
        plan.lines.push(stays_off(
            "reaction_roles",
            "home guild missing (home_guild_missing)",
        ));
        return;
    };
    // The safeword's restricted roles matter only while the safeword runs; if its list cannot
    // be read, the reaction path cannot tell what to deny and stays off (fail closed).
    let restricted = if mode_requested(lookup, safeword_config::KEY_MODE) {
        match parse_id_list(
            safeword_config::KEY_RESTRICTED_ROLE_IDS,
            lookup(safeword_config::KEY_RESTRICTED_ROLE_IDS).as_deref(),
        ) {
            Ok(ids) => ids,
            Err(_) => {
                plan.lines.push(stays_off(
                    "reaction_roles",
                    "safeword restricted roles unusable (restricted_roles_invalid)",
                ));
                return;
            }
        }
    } else {
        Vec::new()
    };
    // The safeword is requested but did not start (a refused key, no home guild, an unusable
    // protected list): reaction roles would then read every member as "not under safeword" and
    // could hand out a restricted role. Refuse instead (fail closed); the legacy arms stay.
    if mode_requested(lookup, safeword_config::KEY_MODE) && plan.safeword.is_none() {
        plan.lines.push(stays_off(
            "reaction_roles",
            "safeword requested but not running (safeword_unavailable)",
        ));
        return;
    }
    plan.lines.push(format!(
        "Feature reaction_roles on: mode=live; protected roles={}; it owns the reaction events.",
        config.protected_role_ids.len()
    ));
    plan.reaction_roles = Some(ReactionRolesPlan {
        home_guild,
        protected_roles_invalid: config.invalid_core_keys.contains(&PROTECTED_ROLE_IDS_KEY),
        protected_role_ids: config.protected_role_ids.clone(),
        safeword_restricted_role_ids: restricted,
        staff_alert_channel: config.staff_alert_channel_id,
    });
}

/// Reads the optional-feature settings. A feature whose mode is off is absent and silent; a
/// feature that is on but cannot run is absent with one line naming a key and a stable code.
pub fn plan_features(lookup: &dyn Fn(&str) -> Option<String>, config: &Config) -> FeaturePlan {
    let home = match guild_filter(config.guild_id, &config.invalid_core_keys) {
        GuildFilter::Only(guild) => Some(guild),
        GuildFilter::Any | GuildFilter::Closed => None,
    };
    let mut plan = FeaturePlan {
        flash: None,
        safeword: None,
        reaction_roles: None,
        welcome: None,
        staff_alert_channel: config.staff_alert_channel_id,
        home_guild: home,
        lines: Vec::new(),
    };
    plan_flash(&mut plan, lookup, home);
    plan_safeword(&mut plan, lookup, config, home);
    plan_reaction_roles(&mut plan, lookup, config, home);
    plan_welcome(&mut plan, lookup, config, home);
    plan
}

/// Plans the welcome feature (`docs/verification/WIRING-C1.md`). A mode that is off is silent. A
/// mode that is on but cannot run is dropped with one line naming a key or a stable code.
fn plan_welcome(
    plan: &mut FeaturePlan,
    lookup: &dyn Fn(&str) -> Option<String>,
    config: &Config,
    home: Option<u64>,
) {
    if !mode_requested(lookup, welcome_config::KEY_MODE) {
        return;
    }
    let Some(home_guild) = home else {
        plan.lines.push(stays_off(
            "welcome",
            "home guild missing (home_guild_missing)",
        ));
        return;
    };
    // The join roles are checked against the protected-role rule; an unusable list cannot be
    // read as "nothing is protected".
    if config.invalid_core_keys.contains(&PROTECTED_ROLE_IDS_KEY) {
        plan.lines.push(stays_off(
            "welcome",
            "protected role list unusable (protected_roles_invalid)",
        ));
        return;
    }
    let parsed = welcome_config::parse(|key| lookup(key).map(std::ffi::OsString::from));
    if !parsed.invalid_keys.is_empty() {
        let keys: Vec<String> = parsed
            .invalid_keys
            .iter()
            .map(|key| format!("{key}: invalid_setting"))
            .collect();
        plan.lines.push(stays_off(
            "welcome",
            &format!("setting refused ({})", keys.join(", ")),
        ));
        return;
    }
    let Some(channel_id) = parsed.channel_id else {
        plan.lines
            .push(stays_off("welcome", "no channel set (no_channel)"));
        return;
    };
    if parsed.template_path.is_none() {
        plan.lines
            .push(stays_off("welcome", "no template set (no_template)"));
        return;
    }
    plan.lines.push(format!(
        "Feature welcome on: mode={}; join roles={}; staff alerts={}.",
        mode_word(parsed.mode),
        parsed.join_role_ids.len(),
        if config.staff_alert_channel_id.is_some() {
            "set"
        } else {
            "unset"
        }
    ));
    plan.welcome = Some(WelcomePlan {
        config: parsed,
        home_guild,
        channel_id,
        protected_role_ids: config.protected_role_ids.clone(),
        staff_alert_channel: config.staff_alert_channel_id,
        ids: IdMap::default(),
    });
}

/// What the preflight is asked about: one request per planned feature that grants roles or posts
/// into channels (flash, safeword, welcome), with that feature's own ids. Empty when none is
/// planned, and then nothing is looked up and nothing is printed.
pub fn preflight_requests(plan: &FeaturePlan) -> Vec<preflight::FeatureRequest> {
    let mut requests = Vec::new();
    if let Some(flash) = &plan.flash {
        let mut request = preflight::FeatureRequest::new(preflight::Feature::Flash, flash.mode());
        request.channels = flash.policy().channel_ids().to_vec();
        requests.push(request);
    }
    if let Some(command) = &plan.safeword {
        let mut request =
            preflight::FeatureRequest::new(preflight::Feature::Safeword, command.safeword.mode);
        request.roles = vec![command.safeword.role_id];
        requests.push(request);
    }
    if let Some(welcome) = &plan.welcome {
        let mut request =
            preflight::FeatureRequest::new(preflight::Feature::Welcome, welcome.config.mode);
        request.channels = vec![welcome.channel_id];
        request.roles = welcome.config.join_role_ids.clone();
        requests.push(request);
    }
    requests
}

/// The plan after the startup preflight, and the lines to print for it.
#[derive(Clone, Debug)]
pub struct Gated {
    /// The plan with every feature that may not run removed.
    pub plan: FeaturePlan,
    /// Feature names, statuses and stable codes only: never an id, a name, a path or a value.
    pub lines: Vec<String>,
}

/// The reason text of a feature the preflight does not let start, or `None` when it may start.
/// A planned feature with no verdict at all does not start (fail closed).
fn not_ready(report: &preflight::Report, feature: preflight::Feature) -> Option<String> {
    match report.feature(feature) {
        Some(entry) if entry.status == preflight::Status::Ready => None,
        Some(entry) if entry.reasons.is_empty() => Some("preflight_not_ready".to_string()),
        Some(entry) => Some(
            entry
                .reasons
                .iter()
                .map(|reason| reason.code())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        None => Some("preflight_no_verdict".to_string()),
    }
}

/// Applies the preflight verdicts to a plan. Pure.
///
/// A planned feature whose verdict is not `ready` is removed from the plan (it is not started)
/// with a line naming the reason codes. The welcome join roles are also checked against the role
/// policy with the facts the preflight gathered (`validate_join_roles`): a refused role removes
/// welcome with the key `JOIN_ROLE_IDS` and the policy code. A safeword that does not start takes
/// reaction roles with it, as the plan already does for a safeword that cannot be planned.
pub fn gate_plan(plan: &FeaturePlan, input: &preflight::PreflightInput) -> Gated {
    let report = preflight::evaluate(input);
    let mut gated = plan.clone();
    let mut lines: Vec<String> = report
        .render_lines()
        .into_iter()
        .map(|line| format!("Preflight {line}"))
        .collect();
    if gated.flash.is_some() {
        if let Some(why) = not_ready(&report, preflight::Feature::Flash) {
            gated.flash = None;
            lines.push(stays_off("flash", &format!("preflight disabled ({why})")));
        }
    }
    if gated.safeword.is_some() {
        if let Some(why) = not_ready(&report, preflight::Feature::Safeword) {
            gated.safeword = None;
            lines.push(stays_off(
                "safeword",
                &format!("preflight disabled ({why})"),
            ));
            if gated.reaction_roles.take().is_some() {
                lines.push(stays_off(
                    "reaction_roles",
                    "safeword requested but not running (safeword_unavailable)",
                ));
            }
        }
    }
    if let Some(welcome) = gated.welcome.clone() {
        if let Some(why) = not_ready(&report, preflight::Feature::Welcome) {
            gated.welcome = None;
            lines.push(stays_off("welcome", &format!("preflight disabled ({why})")));
        } else {
            let refusals = validate_join_roles(
                &welcome.config.join_role_ids,
                welcome.home_guild,
                &welcome.protected_role_ids,
                input.bot_top_position,
                &input.role_facts,
            );
            let mut codes: Vec<&'static str> = Vec::new();
            for refusal in &refusals {
                if !codes.contains(&refusal.reason) {
                    codes.push(refusal.reason);
                }
            }
            if !refusals.is_empty() {
                gated.welcome = None;
                let key = refusals[0].key;
                for code in codes {
                    lines.push(stays_off(
                        "welcome",
                        &format!("join role refused ({key}: {code})"),
                    ));
                }
            }
        }
    }
    Gated { plan: gated, lines }
}

/// Builds the welcome feature from its plan: reads and validates the template, wires the runtime
/// to the given ports and wraps it for the registry (core database, two job kinds, home guild).
/// `Ok(None)` when welcome is not planned. An unusable template stops welcome with one line that
/// names a stable code (`template_unreadable`, or the template's own refusal code) and nothing
/// from the file.
pub fn build_welcome<X, D, S, W, P, M, A, G>(
    plan: &FeaturePlan,
    ports: WelcomePorts<S, W, P, M, A, G>,
    bot_user_id: u64,
    read_template: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Box<dyn Feature<X, D>>>, String>
where
    X: Send + Sync + 'static,
    D: Send + Sync + 'static,
    S: RoleSource + Send + Sync + 'static,
    W: RoleWriter + Send + Sync + 'static,
    P: Sleeper + Send + Sync + 'static,
    M: MessageSender + Send + Sync + 'static,
    A: StaffAlerter + Send + Sync + 'static,
    G: GuildLookup + 'static,
{
    let Some(welcome) = &plan.welcome else {
        return Ok(None);
    };
    let key = welcome.config.template_path.as_deref().unwrap_or("");
    let Some(source) = read_template(key) else {
        return Err(stays_off(
            "welcome",
            "template unreadable (template_unreadable)",
        ));
    };
    let ids = IdMap::default();
    let template = WelcomeTemplate::load(&source, &ids).map_err(|error| {
        stays_off(
            "welcome",
            &format!(
                "template refused ({}: {})",
                welcome_config::KEY_TEMPLATE_PATH,
                error.kind().code()
            ),
        )
    })?;
    let runtime = WelcomeRuntime {
        config: welcome.config.clone(),
        template,
        ids,
        source: ports.source,
        writer: ports.writer,
        sleeper: ports.sleeper,
        sender: ports.sender,
        alerter: ports.alerter,
        staff_alert_channel: welcome.staff_alert_channel,
        bot_user_id,
        grant_policy: GrantPolicy {
            protected_role_ids: welcome.protected_role_ids.clone(),
            ..GrantPolicy::default()
        },
    };
    let feature = WelcomeFeature::new(runtime, ports.guilds);
    Ok(Some(Box::new(WelcomeJobs::new(
        Box::new(feature),
        welcome.home_guild,
    ))))
}

/// What building the features needs from Discord.
pub struct Services {
    pub http: Arc<serenity::Http>,
    pub cache: Arc<serenity::Cache>,
    pub bot_user_id: u64,
    pub reaction: Arc<dyn ReactionServices>,
    /// The safeword runtime, present exactly when the plan has the safeword (see
    /// [`safeword_runtime`]).
    pub safeword: Option<Arc<SafewordRuntime>>,
}

/// What [`safeword_runtime`] needs besides the plan.
pub struct SafewordParts {
    pub http: Arc<serenity::Http>,
    pub cache: Arc<serenity::Cache>,
    pub bot_user_id: u64,
    pub store: StoreSpec,
    pub protected_role_ids: Vec<u64>,
}

/// The safeword runtime for a plan that has the safeword; `None` otherwise.
pub fn safeword_runtime(plan: &FeaturePlan, parts: SafewordParts) -> Option<Arc<SafewordRuntime>> {
    let command = plan.safeword.clone()?;
    Some(Arc::new(SafewordRuntime::new(RuntimeDeps {
        command,
        http: parts.http,
        cache: parts.cache,
        bot_user_id: parts.bot_user_id,
        store: SharedStore::new(parts.store),
        staff_alert_channel: plan.staff_alert_channel,
        protected_role_ids: parts.protected_role_ids,
    })))
}

/// Builds the planned features, in registration order: flash, safeword, reaction roles. An empty
/// plan builds nothing.
pub fn build_features<X: Sync + 'static, D: Sync + 'static>(
    plan: &FeaturePlan,
    services: &Services,
) -> Vec<Box<dyn Feature<X, D>>> {
    let mut built: Vec<Box<dyn Feature<X, D>>> = Vec::new();
    if let Some(settings) = &plan.flash {
        let ports = Arc::new(SerenityFlashPorts {
            http: services.http.clone(),
            cache: services.cache.clone(),
            staff_alert_channel: plan.staff_alert_channel,
        });
        built.push(Box::new(FlashFeature::new(FlashDeps {
            settings: settings.clone(),
            ports: ports.clone(),
            alerts: ports,
            clock: Arc::new(SystemClock),
        })));
    }
    if let (Some(_), Some(runtime)) = (&plan.safeword, &services.safeword) {
        built.push(Box::new(SafewordHook::new(runtime.clone())));
    }
    if let Some(rr) = &plan.reaction_roles {
        let env = ReactionEnv {
            home_guild_id: Some(rr.home_guild),
            protected_roles_invalid: rr.protected_roles_invalid,
            protected_role_ids: rr.protected_role_ids.clone(),
            safeword_restricted_role_ids: rr.safeword_restricted_role_ids.clone(),
            staff_alert_channel_id: rr.staff_alert_channel,
            bot_user_id: services.bot_user_id,
        };
        built.push(Box::new(ReactionRolesFeature::live(
            env,
            services.reaction.clone(),
        )));
    }
    built
}

/// What [`start_gated`] needs.
pub struct GatedStart<'a> {
    pub plan: &'a FeaturePlan,
    pub bot_user_id: u64,
    pub http: Arc<serenity::Http>,
    pub cache: Arc<serenity::Cache>,
    pub facts: &'a dyn FactSource,
    pub roles: &'a dyn RoleSource,
    pub store: StoreSpec,
    pub protected_role_ids: Vec<u64>,
    /// Builds the reaction-role services; it gets the safeword lookup when the safeword runs.
    pub reaction: &'a dyn Fn(Option<SafewordLookup>) -> Arc<dyn ReactionServices>,
    pub read_template: &'a dyn Fn(&str) -> Option<String>,
}

/// What [`start_gated`] hands to `setup`.
pub struct GatedFeatures<X, D> {
    /// The features to register, in order.
    pub features: Vec<Box<dyn Feature<X, D>>>,
    /// The safeword runtime, present exactly when the safeword was planned AND passed.
    pub safeword: Option<Arc<SafewordRuntime>>,
    /// Preflight lines and the line of a welcome that could not be built. No id, path or value.
    pub lines: Vec<String>,
}

/// Preflight, then build: the one place that turns "what the environment asked for" into "what
/// is registered". RED STUB: builds from the plan as asked, ignoring the verdicts.
pub async fn start_gated<X: Send + Sync + 'static, D: Send + Sync + 'static>(
    request: GatedStart<'_>,
) -> GatedFeatures<X, D> {
    let gated = startup_preflight(
        request.plan,
        request.bot_user_id,
        request.facts,
        request.roles,
    )
    .await;
    let mut lines = gated.lines;
    let built_plan = request.plan.clone();
    let safeword = safeword_runtime(
        &built_plan,
        SafewordParts {
            http: request.http.clone(),
            cache: request.cache.clone(),
            bot_user_id: request.bot_user_id,
            store: request.store,
            protected_role_ids: request.protected_role_ids,
        },
    );
    // Reaction roles ask the safeword store whether a member is under safeword.
    let lookup: Option<SafewordLookup> = safeword.as_ref().map(|runtime| {
        let runtime = runtime.clone();
        Arc::new(move |guild: u64, member: u64| runtime.store().reaction_state(guild, member))
            as SafewordLookup
    });
    let services = Services {
        http: request.http.clone(),
        cache: request.cache.clone(),
        bot_user_id: request.bot_user_id,
        reaction: (request.reaction)(lookup),
        safeword: safeword.clone(),
    };
    let mut features: Vec<Box<dyn Feature<X, D>>> = production_features();
    features.extend(build_features(&built_plan, &services));
    if let Some(welcome) = &built_plan.welcome {
        let ports = crate::welcome::discord::serenity_ports(
            request.http.clone(),
            request.cache.clone(),
            welcome.home_guild,
        );
        match build_welcome(&built_plan, ports, request.bot_user_id, request.read_template) {
            Ok(Some(feature)) => features.push(feature),
            Ok(None) => {}
            Err(line) => lines.push(line),
        }
    }
    GatedFeatures {
        features,
        safeword,
        lines,
    }
}

/// The start-up decisions that depend on which planned features really started. Production
/// `setup` and `main` take these values from [`wire`] and from nowhere else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wiring {
    /// The new reaction-role feature owns the reaction events (the legacy arms stand down).
    pub reaction_roles_owned: bool,
    /// The `/safeword` command has a running feature behind it.
    pub safeword_active: bool,
    /// Channel ids the logger must never record (flash).
    pub flash_log_excluded: Vec<u64>,
}

/// Decides [`Wiring`] from the plan and from which features survived start-up. Pure.
pub fn wire(plan: &FeaturePlan, is_active: &dyn Fn(&str) -> bool) -> Wiring {
    Wiring {
        // Owned only if the feature was planned AND really started: a dropped feature must
        // leave the legacy arms in charge.
        reaction_roles_owned: plan.reaction_roles_owned() && is_active(reactions::FEATURE_NAME),
        safeword_active: plan.safeword_command() && is_active(SAFEWORD_FEATURE_NAME),
        flash_log_excluded: plan.log_excluded_channels(),
    }
}

/// The name the safeword feature registers under.
const SAFEWORD_FEATURE_NAME: &str = "safeword";

/// Stops the runner started by [`TokioJobStarter`].
struct JobControl {
    shutdown: JobSignal,
    finished: BoxFuture<'static, ()>,
}

/// The registry of enabled features and their shared state.
pub struct Features<X, D> {
    active: Vec<Box<dyn Feature<X, D>>>,
    guild: GuildFilter,
    core_db: Option<tokio_rusqlite::Connection>,
    jobs: Mutex<Option<JobControl>>,
    lines: Vec<String>,
}

/// Opens the core database (and its job table when jobs are wanted) off the
/// executor. The error is a stable code.
async fn open_core_db(
    settings: &Settings,
    init_jobs: bool,
) -> Result<rusqlite::Connection, &'static str> {
    let Some(path) = settings.core_db_path.clone() else {
        return Err("unusable_setting");
    };
    let reserved = settings.reserved_db_paths.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = storage::open(&path, &reserved).map_err(|e| e.code())?;
        if init_jobs {
            queue::init(&mut conn).map_err(|e| e.code())?;
        }
        Ok(conn)
    })
    .await
    .unwrap_or(Err("open_task_failed"))
}

/// Builds the shared job registry from the features that need the database.
/// A feature whose registration fails is dropped (named, with a code) and the
/// registry is rebuilt without it, so no half-registered kind survives.
fn build_registry<X, D>(
    active: &mut Vec<Box<dyn Feature<X, D>>>,
    lines: &mut Vec<String>,
) -> Registry {
    loop {
        let mut registry = Registry::new();
        let mut failed = None;
        for (index, feature) in active.iter().enumerate() {
            if !feature.needs_core_db() {
                continue;
            }
            if let Err(error) = feature.register_jobs(&mut registry) {
                failed = Some((index, error));
                break;
            }
        }
        match failed {
            None => return registry,
            Some((index, error)) => {
                let feature = active.remove(index);
                lines.push(format!(
                    "Feature {} stays off: job registration failed ({}).",
                    feature.name(),
                    error.code()
                ));
            }
        }
    }
}

fn drop_database_features<X, D>(active: &mut Vec<Box<dyn Feature<X, D>>>) {
    active.retain(|feature| !feature.needs_core_db());
}

impl<X: Sync + 'static, D: Sync + 'static> Features<X, D> {
    /// Builds the registry: keeps the enabled features, opens the core
    /// database only when one of them needs it, and starts the job runner only
    /// when one of them registered a job kind. Any failure turns the features
    /// that need the database off and is reported as a stable code.
    pub async fn start<S: JobStarter>(req: StartRequest<'_, X, D>, starter: &S) -> Self {
        let StartRequest { features, settings } = req;
        let infos: Vec<FeatureInfo> = features
            .iter()
            .map(|f| FeatureInfo {
                enabled: f.enabled(),
                needs_core_db: f.needs_core_db(),
            })
            .collect();
        let plan = plan(&infos);
        let mut active: Vec<Box<dyn Feature<X, D>>> = features
            .into_iter()
            .zip(plan.keep)
            .filter_map(|(feature, keep)| keep.then_some(feature))
            .collect();
        let mut lines = Vec::new();
        let mut core_db = None;
        let mut jobs = None;

        if plan.open_core_db {
            let registry = build_registry(&mut active, &mut lines);
            if active.iter().any(|f| f.needs_core_db()) {
                match open_core_db(settings, !registry.is_empty()).await {
                    Err(code) => {
                        lines.push(format!(
                            "Core database unavailable ({code}); {CORE_DB_PATH_KEY} is unusable or refused, so features that need it stay off."
                        ));
                        drop_database_features(&mut active);
                    }
                    Ok(conn) => {
                        let conn: tokio_rusqlite::Connection = conn.into();
                        if registry.is_empty() {
                            core_db = Some(conn);
                        } else {
                            match starter.start(conn.clone(), registry).await {
                                Ok(started) => {
                                    lines.push(started.counts.line());
                                    for feature in active.iter().filter(|f| f.needs_core_db()) {
                                        feature.jobs_started(started.wake.clone());
                                    }
                                    jobs = Some(JobControl {
                                        shutdown: started.shutdown,
                                        finished: started.finished,
                                    });
                                    core_db = Some(conn);
                                }
                                Err(error) => {
                                    lines.push(format!(
                                        "Job runner could not start ({}); features that need the core database stay off.",
                                        error.code()
                                    ));
                                    drop_database_features(&mut active);
                                }
                            }
                        }
                    }
                }
            }
        }

        Features {
            active,
            guild: settings.guild,
            core_db,
            jobs: Mutex::new(jobs),
            lines,
        }
    }

    /// Start-up lines for the caller to print (states and counts, no ids).
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// True when the named feature survived start-up (enabled, and its database and job
    /// registration worked).
    pub fn is_active(&self, name: &str) -> bool {
        self.active.iter().any(|feature| feature.name() == name)
    }

    /// Hands the event to every active feature, in registration order, when
    /// the guild filter lets it through. Returns how many features saw it.
    ///
    /// Features are awaited one after the other, so a slow feature delays the
    /// logger; a feature must not block. Dispatch itself never touches the message
    /// cache (a feature receives `data` and must not use it for that). With no
    /// active feature this returns before looking at the event.
    pub async fn dispatch(&self, ctx: &X, event: &serenity::FullEvent, data: &D) -> usize {
        if self.active.is_empty() {
            return 0;
        }
        let guild = event_guild(event);
        if !guild_allows(self.guild, guild) {
            return 0;
        }
        let guild_id = match guild {
            EventGuild::Guild(id) => Some(id),
            _ => None,
        };
        for feature in &self.active {
            let core_db = if feature.needs_core_db() {
                self.core_db.as_ref()
            } else {
                None
            };
            feature.handle(ctx, event, data, guild_id, core_db).await;
        }
        self.active.len()
    }

    /// Stops the job runner, if one was started, and waits for it (at most
    /// the runner's shutdown grace plus a small margin). Safe to call twice.
    pub async fn stop(&self) {
        let control = self
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(JobControl { shutdown, finished }) = control else {
            return;
        };
        shutdown();
        if tokio::time::timeout(RunnerConfig::DEFAULT.shutdown_grace + STOP_MARGIN, finished)
            .await
            .is_err()
        {
            eprintln!("Job runner did not stop in time.");
        }
    }
}

// ---------------------------------------------------------------------------
// The real job starter
// ---------------------------------------------------------------------------

/// Starts the real runner: reconcile once, then run on a tokio task.
pub struct TokioJobStarter;

impl JobStarter for TokioJobStarter {
    fn start(
        &self,
        db: tokio_rusqlite::Connection,
        registry: Registry,
    ) -> BoxFuture<'_, Result<StartedJobs, RunnerError>> {
        Box::pin(async move {
            let runner = Runner::new(db, Arc::new(SystemClock), registry, RunnerConfig::DEFAULT)?;
            let wake = runner.wake_handle();
            let shutdown = runner.shutdown_handle();
            let started = runner.start().await?;
            let counts = report_counts(started.report());
            let task = tokio::spawn(async move {
                match started.run().await {
                    Ok(summary) => println!(
                        "Job runner stopped: claimed={} completed={} retried={} failed={} left_running={}",
                        summary.claimed,
                        summary.completed,
                        summary.retried,
                        summary.failed,
                        summary.left_running.len()
                    ),
                    Err(error) => println!("Job runner stopped with an error ({}).", error.code()),
                }
            });
            Ok(StartedJobs {
                counts,
                wake: Arc::new(move || wake.wake()),
                shutdown: Arc::new(move || shutdown.trigger()),
                finished: Box::pin(async move {
                    let _ = task.await;
                }),
            })
        })
    }
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
    #[test]
    fn build_facts_reads_parent_and_category_and_fails_closed() {
        let guild = |parent| Lookup::Guild { parent };
        // A top-level channel: no parent, no category.
        let f = build_facts(GUILD, CH_PLAIN, guild(None), None);
        assert_eq!(f.guild_id, Some(GUILD));
        assert_eq!(f.channel_id, Some(CH_PLAIN));
        assert_eq!(f.parent_id, ParentResolution::None);
        assert_eq!(f.category_id, ParentResolution::None);
        // A channel in a category; the category itself has no parent.
        let f = build_facts(
            GUILD,
            CH_IN_OTHER_CATEGORY,
            guild(Some(CAT_OTHER)),
            Some(guild(None)),
        );
        assert_eq!(f.parent_id, ParentResolution::Id(CAT_OTHER));
        assert_eq!(f.category_id, ParentResolution::None);
        // A thread: parent is the channel, category is the grandparent.
        let f = build_facts(
            GUILD,
            THREAD_UNDER_PLAIN,
            guild(Some(CH_PLAIN)),
            Some(guild(Some(CAT_OTHER))),
        );
        assert_eq!(f.parent_id, ParentResolution::Id(CH_PLAIN));
        assert_eq!(f.category_id, ParentResolution::Id(CAT_OTHER));
        // Anything unresolved is Unknown, never None.
        let f = build_facts(GUILD, CH_PLAIN, Lookup::Unknown, None);
        assert_eq!(f.parent_id, ParentResolution::Unknown);
        assert_eq!(f.category_id, ParentResolution::Unknown);
        let f = build_facts(GUILD, CH_PLAIN, Lookup::NotGuild, None);
        assert_eq!(f.parent_id, ParentResolution::Unknown);
        let f = build_facts(GUILD, CH_PLAIN, guild(Some(0)), None);
        assert_eq!(f.parent_id, ParentResolution::Unknown);
        // A parent that is known but whose own lookup is missing or failed.
        let f = build_facts(GUILD, CH_PLAIN, guild(Some(CAT_OTHER)), None);
        assert_eq!(f.category_id, ParentResolution::Unknown);
        let f = build_facts(
            GUILD,
            CH_PLAIN,
            guild(Some(CAT_OTHER)),
            Some(Lookup::Unknown),
        );
        assert_eq!(f.category_id, ParentResolution::Unknown);
        let f = build_facts(
            GUILD,
            CH_PLAIN,
            guild(Some(CAT_OTHER)),
            Some(guild(Some(0))),
        );
        assert_eq!(f.category_id, ParentResolution::Unknown);
        // No move is described by a message event.
        assert_eq!(f.old_parent_id, ParentResolution::None);
        assert_eq!(f.new_parent_id, ParentResolution::None);
    }

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
        fn boxed(
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
                TestFeature::boxed("off_db", false, true, &["off_kind"], &probe),
                TestFeature::boxed("off_plain", false, false, &[], &probe),
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
            vec![TestFeature::boxed("plain", true, false, &[], &probe)],
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
            vec![TestFeature::boxed("needs_db", true, true, &[], &probe)],
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
                TestFeature::boxed("alpha", true, true, &["alpha_job", "alpha_two"], &probe_a),
                TestFeature::boxed("beta", true, true, &["beta_job"], &probe_b),
                TestFeature::boxed("off", false, true, &["never_registered"], &probe_b),
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
            vec![TestFeature::boxed(
                "liar",
                true,
                false,
                &["liar_job"],
                &probe,
            )],
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
                TestFeature::boxed("needs_db", true, true, &["job_a"], &probe_db),
                TestFeature::boxed("plain", true, false, &[], &probe_plain),
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
            vec![TestFeature::boxed(
                "needs_db",
                true,
                true,
                &["job_a"],
                &probe,
            )],
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
                TestFeature::boxed("needs_db", true, true, &["job_a"], &probe_db),
                TestFeature::boxed("plain", true, false, &[], &probe_plain),
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
                TestFeature::boxed("bad", true, true, &["good_prefix", "Bad Kind!"], &probe_bad),
                TestFeature::boxed("good", true, true, &["good_job"], &probe_good),
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

    // ---- the real runner start (TokioJobStarter) --------------------------------
    struct CountingJobFeature {
        ran: Arc<AtomicUsize>,
        wake: Arc<Mutex<Option<JobSignal>>>,
    }

    impl Feature<u8, u16> for CountingJobFeature {
        fn name(&self) -> &'static str {
            "counting"
        }
        fn enabled(&self) -> bool {
            true
        }
        fn needs_core_db(&self) -> bool {
            true
        }
        fn register_jobs(&self, registry: &mut Registry) -> Result<(), RunnerError> {
            let ran = self.ran.clone();
            registry.register("w1_probe", RestartPolicy::RunAgain, move |_job| {
                let ran = ran.clone();
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
        }
        fn jobs_started(&self, wake: JobSignal) {
            *self.wake.lock().unwrap() = Some(wake);
        }
    }

    async fn wait_until(what: &str, condition: impl Fn() -> bool) {
        for _ in 0..250 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_real_runner_reconciles_a_crashed_job_runs_a_new_one_and_stops() {
        use crate::jobs::queue::{NewJob, ScheduleOutcome};
        let dir = scratch("real-runner");
        let db = dir.join("core.sqlite");
        let path = db.to_string_lossy().into_owned();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        {
            // A job a previous process left in `running`.
            let mut c = storage::open(&path, &[]).unwrap();
            queue::init(&mut c).unwrap();
            let job = NewJob {
                kind: "w1_probe",
                dedupe_key: "crashed",
                payload_json: "{}",
                due_at: 0,
            };
            let ScheduleOutcome::Created { id } = queue::schedule(&c, &job, now).unwrap() else {
                panic!("fresh key");
            };
            assert!(queue::try_claim(&c, id, now).unwrap());
        }
        let ran = Arc::new(AtomicUsize::new(0));
        let wake = Arc::new(Mutex::new(None));
        let s = settings(Some(&db), GuildFilter::Any);
        let features: Features<u8, u16> = Features::start(
            StartRequest {
                features: vec![Box::new(CountingJobFeature {
                    ran: ran.clone(),
                    wake: wake.clone(),
                })],
                settings: &s,
            },
            &TokioJobStarter,
        )
        .await;
        let text = features.lines().join("\n");
        assert!(text.contains("requeued=1"), "{text}");
        wait_until("the requeued job to run", || {
            ran.load(Ordering::SeqCst) == 1
        })
        .await;

        // A job scheduled later is picked up after the wake callback.
        {
            let c = storage::open(&path, &[]).unwrap();
            let job = NewJob {
                kind: "w1_probe",
                dedupe_key: "fresh",
                payload_json: "{}",
                due_at: 0,
            };
            assert!(matches!(
                queue::schedule(&c, &job, now).unwrap(),
                ScheduleOutcome::Created { .. }
            ));
        }
        let wake = wake
            .lock()
            .unwrap()
            .clone()
            .expect("wake handed to the feature");
        wake();
        wait_until("the new job to run", || ran.load(Ordering::SeqCst) == 2).await;

        tokio::time::timeout(Duration::from_secs(20), features.stop())
            .await
            .expect("stop returns");
        features.stop().await; // a second stop is a no-op
        assert_eq!(ran.load(Ordering::SeqCst), 2, "each job ran exactly once");
    }

    // ---- dispatch ------------------------------------------------------------
    #[tokio::test]
    async fn dispatch_honours_the_guild_filter() {
        let probe = Probe::new();
        let starter = FakeStarter::default();
        let only = settings(None, GuildFilter::Only(GUILD));
        let f = start(
            vec![TestFeature::boxed("plain", true, false, &[], &probe)],
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
            vec![TestFeature::boxed("plain", true, false, &[], &probe_any)],
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
            vec![TestFeature::boxed("plain", true, false, &[], &probe_closed)],
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
                TestFeature::boxed("one", true, false, &[], &probe),
                TestFeature::boxed("two", false, false, &[], &probe),
                TestFeature::boxed("three", true, false, &[], &probe),
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

#[cfg(test)]
mod wiring_tests {
    //! W2 wiring (`docs/verification/WIRING-B.md`): reading the feature settings into a plan,
    //! building the features, what each one registers, and the log-exclusion scope.
    use super::*;
    use crate::flash::runtime::{JOB_KINDS, KIND_DELETE_NOTICE, KIND_DELETE_SOURCE};
    use crate::role_menus::reactions::{ReactionServices, RoleRemover, StorageCode};
    use crate::roles::eligibility::SafewordState;
    use crate::roles::grant::{RoleWriter, Sleeper, WriteError};
    use crate::roles::runtime::{RoleEntry, RoleSource, SourceError};
    use ::serenity::all::FullEvent;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const GUILD: u64 = 760_001;
    const OTHER_GUILD: u64 = 760_002;
    const STAFF_ROLE: u64 = 760_010;
    const FLASH_A: u64 = 760_101;
    const FLASH_B: u64 = 760_102;
    const FLASH_ROLE: u64 = 760_103;
    const SW_ROLE: u64 = 760_201;
    const SW_CHANNEL: u64 = 760_202;
    const SW_MESSAGE: u64 = 760_203;
    const SW_EMOJI: u64 = 760_204;
    const SW_RESTRICTED: u64 = 760_205;
    const ALERT_CHANNEL: u64 = 760_301;
    const MEMBER: u64 = 760_401;
    const BOT: u64 = 760_402;
    const MESSAGE: u64 = 760_403;
    const BOUND_ROLE: u64 = 760_404;
    const CHANNEL: u64 = 760_405;
    const CRAB: &str = "\u{1F980}";

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("w2b-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config() -> Config {
        Config {
            discord_token: "synthetic-token".to_string(),
            log_channel_id: 760_900,
            age_role_1: 1,
            age_role_2: 2,
            reaction_role_db_path: "reaction-test.sqlite".to_string(),
            staff_role_id: Some(STAFF_ROLE),
            tickets: None,
            ticket_archive_retention: crate::tickets::retention::RetentionPolicy::Disabled,
            ticket_privacy_lockdown: false,
            core_db_path: Some("core-test.sqlite".to_string()),
            guild_id: Some(GUILD),
            staff_alert_channel_id: Some(ALERT_CHANNEL),
            log_excluded_channel_ids: vec![],
            log_excluded_category_ids: vec![],
            protected_role_ids: vec![],
            invalid_core_keys: vec![],
        }
    }

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn plan_for(pairs: &[(&str, &str)], config: &Config) -> FeaturePlan {
        plan_features(&lookup(pairs), config)
    }

    fn flash_keys(mode: &'static str) -> Vec<(&'static str, &'static str)> {
        vec![
            ("FLASH_MODE", mode),
            ("FLASH_CHANNEL_IDS", "760101,760102"),
            ("FLASH_PING_ROLE_ID", "760103"),
        ]
    }

    fn safeword_keys(mode: &'static str) -> Vec<(&'static str, &'static str)> {
        vec![
            ("SAFEWORD_MODE", mode),
            ("SAFEWORD_ROLE_ID", "760201"),
            ("SAFEWORD_CHANNEL_ID", "760202"),
            ("SAFEWORD_MESSAGE_ID", "760203"),
            ("SAFEWORD_EMOJI_ID", "760204"),
            ("SAFEWORD_RESTRICTED_ROLE_IDS", "760205"),
        ]
    }

    type Keys<'a> = Vec<(&'a str, &'a str)>;
    type Case<'a> = (Keys<'a>, Config, &'a str);

    fn lines_text(plan: &FeaturePlan) -> String {
        plan.lines.join("\n")
    }

    // ---- reaction-role services fake -------------------------------------------
    struct Source;

    #[::serenity::async_trait]
    impl RoleSource for Source {
        async fn roles(&self, _g: u64) -> Result<Vec<RoleEntry>, SourceError> {
            let role = |id, position| RoleEntry {
                id,
                position,
                managed: false,
                permissions: 0,
            };
            Ok(vec![role(BOT, 20), role(BOUND_ROLE, 5), role(GUILD, 0)])
        }
        async fn member_roles(&self, _g: u64, member: u64) -> Result<Vec<u64>, SourceError> {
            Ok(if member == BOT { vec![BOT] } else { vec![] })
        }
    }

    #[derive(Default)]
    struct Recorder {
        log: Arc<Mutex<Vec<String>>>,
        bound_calls: AtomicUsize,
    }

    struct LogWriter(Arc<Mutex<Vec<String>>>);

    impl RoleWriter for LogWriter {
        fn add_role(&mut self, member: u64, role: u64) -> Result<(), WriteError> {
            self.0.lock().unwrap().push(format!("add:{member}:{role}"));
            Ok(())
        }
    }

    impl RoleRemover for LogWriter {
        fn remove_role(&mut self, member: u64, role: u64) -> Result<(), WriteError> {
            self.0.lock().unwrap().push(format!("rm:{member}:{role}"));
            Ok(())
        }
    }

    struct NoSleep;

    impl Sleeper for NoSleep {
        fn sleep_ms(&mut self, _ms: u64) {}
    }

    #[::serenity::async_trait]
    impl ReactionServices for Recorder {
        async fn bound_roles(&self, message: u64, key: &str) -> Result<Vec<u64>, StorageCode> {
            self.bound_calls.fetch_add(1, Ordering::SeqCst);
            Ok(if message == MESSAGE && key == CRAB {
                vec![BOUND_ROLE]
            } else {
                vec![]
            })
        }
        fn role_source(&self) -> &dyn RoleSource {
            &Source
        }
        async fn user_is_bot(&self, _g: u64, _u: u64) -> Option<bool> {
            Some(false)
        }
        fn safeword_state(&self, _g: u64, _u: u64) -> SafewordState {
            SafewordState::Inactive
        }
        fn role_writer(&self, _g: u64) -> Box<dyn RoleWriter + Send> {
            Box::new(LogWriter(self.log.clone()))
        }
        fn role_remover(&self, _g: u64) -> Box<dyn RoleRemover + Send> {
            Box::new(LogWriter(self.log.clone()))
        }
        fn sleeper(&self) -> Box<dyn Sleeper + Send> {
            Box::new(NoSleep)
        }
        fn report(&self, _line: &str) {}
    }

    fn services(plan: &FeaturePlan, recorder: &Arc<Recorder>) -> Services {
        let http = Arc::new(serenity::Http::new("synthetic-token"));
        let cache = Arc::new(serenity::Cache::new());
        let safeword = safeword_runtime(
            plan,
            SafewordParts {
                http: http.clone(),
                cache: cache.clone(),
                bot_user_id: BOT,
                store: StoreSpec {
                    path: None,
                    reserved: vec![],
                },
                protected_role_ids: vec![],
            },
        );
        Services {
            http,
            cache,
            bot_user_id: BOT,
            reaction: recorder.clone(),
            safeword,
        }
    }

    fn build(plan: &FeaturePlan, recorder: &Arc<Recorder>) -> Vec<Box<dyn Feature<u8, u16>>> {
        build_features::<u8, u16>(plan, &services(plan, recorder))
    }

    fn core_settings(path: &Path, guild: GuildFilter) -> Settings {
        Settings {
            guild,
            core_db_path: Some(path.to_string_lossy().into_owned()),
            reserved_db_paths: vec![],
            staff_alert_channel: None,
            protected_role_count: 0,
            log_excluded_channels: 0,
            log_excluded_categories: 0,
        }
    }

    #[derive(Default)]
    struct Starter {
        kinds: Mutex<Vec<String>>,
    }

    impl JobStarter for Starter {
        fn start(
            &self,
            _db: tokio_rusqlite::Connection,
            registry: Registry,
        ) -> BoxFuture<'_, Result<StartedJobs, RunnerError>> {
            Box::pin(async move {
                self.kinds.lock().unwrap().push(format!("{registry:?}"));
                Ok(StartedJobs {
                    counts: ReportCounts::default(),
                    wake: Arc::new(|| {}),
                    shutdown: Arc::new(|| {}),
                    finished: Box::pin(async {}),
                })
            })
        }
    }

    fn reaction_add(guild: u64, user: u64) -> FullEvent {
        let payload = serde_json::json!({
            "channel_id": CHANNEL.to_string(), "message_id": MESSAGE.to_string(),
            "user_id": user.to_string(), "guild_id": guild.to_string(),
            "emoji": {"name": CRAB}, "burst": false, "burst_colours": [], "type": 0,
            "member": {"user": {"id": user.to_string(), "username": "synthetic",
                       "discriminator": "0", "avatar": null, "bot": false},
                       "roles": [], "joined_at": "2020-01-01T00:00:00.000000+00:00",
                       "deaf": false, "mute": false, "flags": 0},
        });
        FullEvent::ReactionAdd {
            add_reaction: serde_json::from_value(payload).unwrap(),
        }
    }

    // ---- ALL-OFF PARITY ---------------------------------------------------------
    #[test]
    fn with_none_of_the_new_keys_set_the_plan_is_empty() {
        let plan = plan_for(&[], &config());
        assert!(plan.flash.is_none());
        assert!(plan.safeword.is_none());
        assert!(plan.reaction_roles.is_none());
        assert!(plan.lines.is_empty(), "{:?}", plan.lines);
        assert!(plan.log_excluded_channels().is_empty());
        assert!(!plan.safeword_command());
        assert!(!plan.reaction_roles_owned());
    }

    #[test]
    fn keys_set_while_every_mode_is_off_change_nothing_and_print_nothing() {
        let mut pairs = flash_keys("off");
        pairs.extend(safeword_keys("off"));
        pairs.push(("REACTION_ROLES_MODE", "off"));
        // Even bad values stay silent while the mode is off.
        pairs.push(("FLASH_DELAY_SECS", "not-a-number"));
        pairs.push(("SAFEWORD_ROLE_ID", "not-an-id"));
        let plan = plan_for(&pairs, &config());
        assert!(plan.flash.is_none() && plan.safeword.is_none() && plan.reaction_roles.is_none());
        assert!(plan.lines.is_empty(), "{:?}", plan.lines);
        assert!(plan.log_excluded_channels().is_empty());
        // Unset, empty and unknown modes are off too.
        for mode in ["", "wat", "OFF"] {
            let pairs = [("FLASH_MODE", mode), ("SAFEWORD_MODE", mode)];
            assert!(plan_for(&pairs, &config()).lines.is_empty(), "{mode}");
        }
    }

    #[tokio::test]
    async fn all_off_creates_no_database_starts_no_runner_and_delivers_nothing() {
        let dir = scratch("all-off");
        let db = dir.join("core.sqlite");
        let recorder = Arc::new(Recorder::default());
        let plan = plan_for(&[], &config());
        let features = build(&plan, &recorder);
        assert!(features.is_empty());
        let starter = Starter::default();
        let settings = core_settings(&db, GuildFilter::Only(GUILD));
        let started = Features::<u8, u16>::start(
            StartRequest {
                features,
                settings: &settings,
            },
            &starter,
        )
        .await;
        assert!(!db.exists(), "no core.sqlite");
        assert!(starter.kinds.lock().unwrap().is_empty());
        assert!(started.lines().is_empty());
        assert_eq!(
            started.dispatch(&0, &reaction_add(GUILD, MEMBER), &0).await,
            0
        );
        assert_eq!(recorder.bound_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn with_everything_off_the_log_exclusion_is_exactly_the_configured_one() {
        let plan = plan_for(&[], &config());
        let c = config();
        assert!(matches!(
            LogExclusion::from_config_with_flash(&c, &plan.log_excluded_channels()),
            LogExclusion::Off
        ));
    }

    // ---- flash ------------------------------------------------------------------
    #[test]
    fn flash_on_plans_the_feature_and_reports_counts_only() {
        for mode in ["live", "shadow"] {
            let plan = plan_for(&flash_keys(mode), &config());
            assert!(plan.flash.is_some(), "{mode}");
            assert_eq!(
                plan.log_excluded_channels(),
                vec![FLASH_A, FLASH_B],
                "{mode}"
            );
            let text = lines_text(&plan);
            assert!(text.contains("flash") && text.contains(mode), "{text}");
            assert!(text.contains("channels=2"), "{text}");
        }
        assert_eq!(FLASH_ROLE, 760_103);
    }

    #[test]
    fn flash_with_a_refused_setting_is_dropped_with_the_key_and_a_code_but_no_value() {
        let mut pairs = flash_keys("live");
        pairs.push(("FLASH_DELAY_SECS", "secret-looking-value"));
        let plan = plan_for(&pairs, &config());
        assert!(plan.flash.is_none());
        assert!(plan.log_excluded_channels().is_empty());
        let text = lines_text(&plan);
        assert!(text.contains("FLASH_DELAY_SECS"), "{text}");
        assert!(text.contains("unreadable"), "{text}");
        assert!(!text.contains("secret-looking-value"), "{text}");
        let mut pairs = flash_keys("live");
        pairs.push(("FLASH_DELAY_SECS", "5"));
        let text = lines_text(&plan_for(&pairs, &config()));
        assert!(text.contains("out_of_range"), "{text}");
    }

    #[test]
    fn flash_without_channels_is_dropped_because_it_would_do_nothing() {
        let plan = plan_for(&[("FLASH_MODE", "live")], &config());
        assert!(plan.flash.is_none());
        assert!(
            lines_text(&plan).contains("no_channels"),
            "{:?}",
            plan.lines
        );
    }

    #[test]
    fn flash_needs_the_home_guild_unset_or_unusable_drops_it() {
        let mut unset = config();
        unset.guild_id = None;
        let mut unusable = config();
        unusable.invalid_core_keys = vec!["GUILD_ID"];
        for c in [unset, unusable] {
            let plan = plan_for(&flash_keys("live"), &c);
            assert!(plan.flash.is_none());
            assert!(plan.log_excluded_channels().is_empty());
            assert!(
                lines_text(&plan).contains("home_guild_missing"),
                "{:?}",
                plan.lines
            );
        }
    }

    #[tokio::test]
    async fn enabling_flash_live_registers_only_the_flash_job_kinds() {
        let dir = scratch("flash-live");
        let db = dir.join("core.sqlite");
        let recorder = Arc::new(Recorder::default());
        let plan = plan_for(&flash_keys("live"), &config());
        let features = build(&plan, &recorder);
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].name(), "flash");
        let starter = Starter::default();
        let settings = core_settings(&db, GuildFilter::Only(GUILD));
        let started = Features::<u8, u16>::start(
            StartRequest {
                features,
                settings: &settings,
            },
            &starter,
        )
        .await;
        let calls = starter.kinds.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![format!(
                "[{:?}, {:?}]",
                KIND_DELETE_NOTICE, KIND_DELETE_SOURCE
            )]
        );
        assert_eq!(JOB_KINDS.len(), 2);
        assert!(db.exists());
        started.stop().await;
    }

    #[tokio::test]
    async fn flash_shadow_opens_no_database_and_registers_no_job() {
        let dir = scratch("flash-shadow");
        let db = dir.join("core.sqlite");
        let recorder = Arc::new(Recorder::default());
        let plan = plan_for(&flash_keys("shadow"), &config());
        let features = build(&plan, &recorder);
        assert_eq!(features.len(), 1);
        let starter = Starter::default();
        let settings = core_settings(&db, GuildFilter::Only(GUILD));
        let started = Features::<u8, u16>::start(
            StartRequest {
                features,
                settings: &settings,
            },
            &starter,
        )
        .await;
        assert!(!db.exists());
        assert!(starter.kinds.lock().unwrap().is_empty());
        assert_eq!(
            started.dispatch(&0, &reaction_add(GUILD, MEMBER), &0).await,
            1
        );
    }

    // ---- log exclusion ----------------------------------------------------------
    fn message_in(channel: u64, guild: u64) -> FullEvent {
        let mut m = serenity::Message::default();
        m.id = serenity::MessageId::new(760_500);
        m.channel_id = serenity::ChannelId::new(channel);
        m.guild_id = Some(serenity::GuildId::new(guild));
        FullEvent::Message { new_message: m }
    }

    struct TopLevel;

    impl ChannelLookup for TopLevel {
        async fn lookup(&self, _id: serenity::ChannelId) -> Lookup {
            Lookup::Guild { parent: None }
        }
    }

    #[tokio::test]
    async fn flash_channels_are_excluded_from_the_log_only_when_flash_is_on() {
        let c = config();
        let on = plan_for(&flash_keys("live"), &c);
        let scope = LogExclusion::from_config_with_flash(&c, &on.log_excluded_channels());
        for channel in [FLASH_A, FLASH_B] {
            let gate = message_gate(&scope, &message_in(channel, GUILD), &TopLevel).await;
            assert_eq!(gate, Gate::Skip(GateReason::Excluded), "{channel}");
        }
        let other = message_gate(&scope, &message_in(760_999, GUILD), &TopLevel).await;
        assert_eq!(other, Gate::Allow);

        let off = plan_for(&[], &c);
        let scope = LogExclusion::from_config_with_flash(&c, &off.log_excluded_channels());
        let gate = message_gate(&scope, &message_in(FLASH_A, GUILD), &TopLevel).await;
        assert_eq!(gate, Gate::Allow);
    }

    #[tokio::test]
    async fn flash_ids_join_the_configured_exclusions_and_never_unblock_a_bad_list() {
        let mut c = config();
        c.log_excluded_channel_ids = vec![760_777];
        let scope = LogExclusion::from_config_with_flash(&c, &[FLASH_A]);
        for channel in [760_777, FLASH_A] {
            let gate = message_gate(&scope, &message_in(channel, GUILD), &TopLevel).await;
            assert_eq!(gate, Gate::Skip(GateReason::Excluded), "{channel}");
        }
        let mut bad = config();
        bad.invalid_core_keys = vec!["LOG_EXCLUDED_CHANNEL_IDS"];
        let scope = LogExclusion::from_config_with_flash(&bad, &[FLASH_A]);
        assert!(matches!(scope, LogExclusion::BlockMessages));
    }

    // ---- reaction roles ---------------------------------------------------------
    #[test]
    fn reaction_roles_live_is_planned_with_the_home_guild_and_owns_the_events() {
        let plan = plan_for(&[("REACTION_ROLES_MODE", "live")], &config());
        let rr = plan.reaction_roles.as_ref().expect("planned");
        assert_eq!(rr.home_guild, GUILD);
        assert!(plan.reaction_roles_owned());
        assert!(
            lines_text(&plan).contains("reaction_roles"),
            "{:?}",
            plan.lines
        );
    }

    #[test]
    fn reaction_roles_refusals_keep_the_legacy_handlers_in_charge() {
        let mut no_guild = config();
        no_guild.guild_id = None;
        let cases: Vec<Case> = vec![
            (
                vec![("REACTION_ROLES_MODE", "shadow")],
                config(),
                "shadow_unsupported",
            ),
            (
                vec![("REACTION_ROLES_MODE", "live")],
                no_guild,
                "home_guild_missing",
            ),
        ];
        for (pairs, c, code) in cases {
            let plan = plan_for(&pairs, &c);
            assert!(plan.reaction_roles.is_none(), "{code}");
            assert!(
                !plan.reaction_roles_owned(),
                "{code}: the old arms must keep working"
            );
            assert!(lines_text(&plan).contains(code), "{:?}", plan.lines);
        }
    }

    #[test]
    fn reaction_roles_carry_the_protected_roles_and_safeword_restrictions() {
        let mut c = config();
        c.protected_role_ids = vec![760_600];
        let mut pairs = vec![("REACTION_ROLES_MODE", "live")];
        pairs.extend(safeword_keys("live"));
        let plan = plan_for(&pairs, &c);
        let rr = plan.reaction_roles.unwrap();
        assert_eq!(rr.protected_role_ids, vec![760_600]);
        assert_eq!(rr.safeword_restricted_role_ids, vec![SW_RESTRICTED]);
        assert!(!rr.protected_roles_invalid);
        let mut bad = config();
        bad.invalid_core_keys = vec!["PROTECTED_ROLE_IDS"];
        let plan = plan_for(&[("REACTION_ROLES_MODE", "live")], &bad);
        assert!(plan.reaction_roles.unwrap().protected_roles_invalid);
    }

    #[test]
    fn an_unusable_restricted_list_refuses_reaction_roles_while_safeword_is_on() {
        let mut pairs = vec![("REACTION_ROLES_MODE", "live")];
        pairs.extend(safeword_keys("shadow"));
        pairs.retain(|(k, _)| *k != "SAFEWORD_RESTRICTED_ROLE_IDS");
        pairs.push(("SAFEWORD_RESTRICTED_ROLE_IDS", "1,oops"));
        let plan = plan_for(&pairs, &config());
        assert!(plan.reaction_roles.is_none());
        assert!(plan.safeword.is_none());
        let text = lines_text(&plan);
        assert!(text.contains("restricted_roles_invalid"), "{text}");
        assert!(text.contains("invalid_id_list"), "{text}");
        assert!(!text.contains("oops"), "{text}");
    }

    #[tokio::test]
    async fn enabling_reaction_roles_registers_no_job_and_no_database() {
        let dir = scratch("rr-only");
        let db = dir.join("core.sqlite");
        let recorder = Arc::new(Recorder::default());
        let plan = plan_for(&[("REACTION_ROLES_MODE", "live")], &config());
        let features = build(&plan, &recorder);
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].name(), "reaction_roles");
        let starter = Starter::default();
        let settings = core_settings(&db, GuildFilter::Only(GUILD));
        let started = Features::<u8, u16>::start(
            StartRequest {
                features,
                settings: &settings,
            },
            &starter,
        )
        .await;
        assert!(!db.exists());
        assert!(starter.kinds.lock().unwrap().is_empty());
        assert!(started.is_active("reaction_roles"));
        assert!(!started.is_active("flash"));
    }

    #[tokio::test]
    async fn an_ordinary_binding_is_granted_through_the_registered_feature() {
        let dir = scratch("rr-dispatch");
        let db = dir.join("core.sqlite");
        let recorder = Arc::new(Recorder::default());
        let plan = plan_for(&[("REACTION_ROLES_MODE", "live")], &config());
        let starter = Starter::default();
        let settings = core_settings(&db, GuildFilter::Only(GUILD));
        let started = Features::<u8, u16>::start(
            StartRequest {
                features: build(&plan, &recorder),
                settings: &settings,
            },
            &starter,
        )
        .await;
        assert_eq!(
            started.dispatch(&0, &reaction_add(GUILD, MEMBER), &0).await,
            1
        );
        assert_eq!(
            *recorder.log.lock().unwrap(),
            vec![format!("add:{MEMBER}:{BOUND_ROLE}")]
        );
        // Another guild never reaches the feature.
        assert_eq!(
            started
                .dispatch(&0, &reaction_add(OTHER_GUILD, MEMBER), &0)
                .await,
            0
        );
        assert_eq!(recorder.log.lock().unwrap().len(), 1);
    }

    // ---- safeword ---------------------------------------------------------------
    #[test]
    fn safeword_on_is_planned_with_the_home_guild_and_staff_role() {
        for mode in ["shadow", "live"] {
            let plan = plan_for(&safeword_keys(mode), &config());
            let cc = plan.safeword.as_ref().expect(mode);
            assert_eq!(cc.home_guild, Some(GUILD));
            assert_eq!(cc.staff_role_id, Some(STAFF_ROLE));
            assert_eq!(cc.safeword.role_id, SW_ROLE);
            assert_eq!(cc.safeword.channel_id, SW_CHANNEL);
            assert_eq!(cc.safeword.message_id, SW_MESSAGE);
            assert_eq!(cc.safeword.emoji_id, SW_EMOJI);
            assert_eq!(cc.safeword.restricted_role_ids, vec![SW_RESTRICTED]);
            assert!(plan.safeword_command(), "{mode}");
            assert!(lines_text(&plan).contains("safeword"), "{:?}", plan.lines);
        }
    }

    #[test]
    fn safeword_refusals_drop_the_feature_and_the_command_with_a_code() {
        let mut no_guild = config();
        no_guild.guild_id = None;
        let mut protected_bad = config();
        protected_bad.invalid_core_keys = vec!["PROTECTED_ROLE_IDS"];
        let mut missing = safeword_keys("live");
        missing.retain(|(k, _)| *k != "SAFEWORD_ROLE_ID");
        let mut bad_list = safeword_keys("live");
        bad_list.retain(|(k, _)| *k != "SAFEWORD_RESTRICTED_ROLE_IDS");
        bad_list.push(("SAFEWORD_RESTRICTED_ROLE_IDS", "760205,,"));
        let mut bad_flag = safeword_keys("live");
        bad_flag.push(("SAFEWORD_DM", "yes"));
        let cases: Vec<Case> = vec![
            (safeword_keys("live"), no_guild, "home_guild_missing"),
            (
                safeword_keys("live"),
                protected_bad,
                "protected_roles_invalid",
            ),
            (missing, config(), "SAFEWORD_ROLE_ID"),
            (bad_list, config(), "SAFEWORD_RESTRICTED_ROLE_IDS"),
            (bad_flag, config(), "SAFEWORD_DM"),
        ];
        for (pairs, c, expect) in cases {
            let plan = plan_for(&pairs, &c);
            assert!(plan.safeword.is_none(), "{expect}");
            assert!(
                !plan.safeword_command(),
                "{expect}: no command without the feature"
            );
            assert!(
                lines_text(&plan).contains(expect),
                "{expect}: {:?}",
                plan.lines
            );
        }
    }

    #[test]
    fn safeword_without_a_staff_role_runs_but_says_nobody_is_staff() {
        let mut c = config();
        c.staff_role_id = None;
        let plan = plan_for(&safeword_keys("live"), &c);
        assert_eq!(plan.safeword.as_ref().unwrap().staff_role_id, None);
        assert!(
            lines_text(&plan).contains("staff_role_unset"),
            "{:?}",
            plan.lines
        );
    }

    #[tokio::test]
    async fn enabling_safeword_registers_no_job_kind_but_needs_the_core_database() {
        let dir = scratch("sw-only");
        let db = dir.join("core.sqlite");
        let recorder = Arc::new(Recorder::default());
        let plan = plan_for(&safeword_keys("live"), &config());
        let features = build(&plan, &recorder);
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].name(), "safeword");
        let starter = Starter::default();
        let settings = core_settings(&db, GuildFilter::Only(GUILD));
        let started = Features::<u8, u16>::start(
            StartRequest {
                features,
                settings: &settings,
            },
            &starter,
        )
        .await;
        assert!(db.exists(), "the registry opened the core database");
        assert!(
            starter.kinds.lock().unwrap().is_empty(),
            "no runner without job kinds"
        );
        assert!(started.is_active("safeword"));
    }

    // ---- scope ------------------------------------------------------------------
    #[tokio::test]
    async fn the_safeword_collaborators_exist_only_for_the_home_guild() {
        use crate::safeword::runtime::SafewordContext;
        let recorder = Arc::new(Recorder::default());
        let plan = plan_for(&safeword_keys("live"), &config());
        let runtime = services(&plan, &recorder).safeword.expect("planned");
        assert!(runtime.safeword_ops(OTHER_GUILD).is_none());
        assert!(runtime.safeword_ops(0).is_none());
        assert!(runtime.safeword_ops(GUILD).is_some());
        // No runtime without the safeword in the plan.
        let off = plan_for(&[], &config());
        assert!(services(&off, &recorder).safeword.is_none());
    }

    #[test]
    fn the_flash_policy_is_scoped_to_the_home_guild() {
        let plan = plan_for(&flash_keys("live"), &config());
        assert_eq!(plan.flash.unwrap().policy().guild_id(), Some(GUILD));
    }

    // ---- startup lines ----------------------------------------------------------
    #[test]
    fn startup_lines_carry_counts_and_codes_never_ids_or_values() {
        let mut c = config();
        c.protected_role_ids = vec![760_600];
        let mut pairs = flash_keys("live");
        pairs.extend(safeword_keys("shadow"));
        pairs.push(("REACTION_ROLES_MODE", "live"));
        let plan = plan_for(&pairs, &c);
        assert_eq!(plan.lines.len(), 3, "{:?}", plan.lines);
        let all = lines_text(&plan);
        for id in [
            GUILD,
            STAFF_ROLE,
            FLASH_A,
            FLASH_B,
            FLASH_ROLE,
            SW_ROLE,
            SW_CHANNEL,
            SW_MESSAGE,
            SW_EMOJI,
            SW_RESTRICTED,
            ALERT_CHANNEL,
            760_600,
        ] {
            assert!(!all.contains(&id.to_string()), "{id} leaked: {all}");
        }
        // No run of five or more digits in any line (an id is 17+ digits in production).
        let mut run = 0;
        for ch in all.chars() {
            run = if ch.is_ascii_digit() { run + 1 } else { 0 };
            assert!(run < 5, "{all}");
        }
        assert!(
            !all.contains(".sqlite") && !all.contains('\\') && !all.contains('/'),
            "{all}"
        );
    }

    #[test]
    fn the_staff_alert_channel_is_carried_for_the_features() {
        let plan = plan_for(&flash_keys("live"), &config());
        assert_eq!(plan.staff_alert_channel, Some(ALERT_CHANNEL));
        let mut c = config();
        c.staff_alert_channel_id = None;
        assert_eq!(plan_for(&flash_keys("live"), &c).staff_alert_channel, None);
    }

    // ---- reaction roles and the safeword (fail closed) --------------------------
    #[tokio::test]
    async fn a_refused_safeword_refuses_reaction_roles_instead_of_reading_everyone_as_idle() {
        // The safeword is requested but cannot start (SAFEWORD_ROLE_ID missing, a flag the
        // parser refuses, an unusable protected list). Its restricted list is still valid, so
        // before the fix reaction roles started with that list and an empty safeword lookup:
        // a member under safeword read as idle and was granted a restricted role.
        let mut protected_bad = config();
        protected_bad.invalid_core_keys = vec!["PROTECTED_ROLE_IDS"];
        let mut missing = safeword_keys("live");
        missing.retain(|(k, _)| *k != "SAFEWORD_ROLE_ID");
        let mut bad_flag = safeword_keys("shadow");
        bad_flag.push(("SAFEWORD_DM", "perhaps"));
        let cases: Vec<Case> = vec![
            (missing, config(), "SAFEWORD_ROLE_ID"),
            (bad_flag, config(), "SAFEWORD_DM"),
            (
                safeword_keys("live"),
                protected_bad,
                "protected_roles_invalid",
            ),
        ];
        for (mut pairs, c, why) in cases {
            pairs.push(("REACTION_ROLES_MODE", "live"));
            let plan = plan_for(&pairs, &c);
            assert!(plan.safeword.is_none(), "{why}: the safeword did not start");
            assert!(
                plan.reaction_roles.is_none(),
                "{why}: reaction roles must refuse"
            );
            assert!(
                !plan.reaction_roles_owned(),
                "{why}: the legacy arms keep working"
            );
            let text = lines_text(&plan);
            assert!(text.contains("safeword_unavailable"), "{why}: {text}");
            // Built from the real plan, nothing handles a reaction: no role is granted.
            let recorder = Arc::new(Recorder::default());
            assert!(build(&plan, &recorder).is_empty(), "{why}");
            assert!(recorder.log.lock().unwrap().is_empty(), "{why}");
        }
    }

    #[test]
    fn reaction_roles_still_start_when_the_safeword_is_off_or_running() {
        let plan = plan_for(&[("REACTION_ROLES_MODE", "live")], &config());
        assert!(
            plan.reaction_roles.is_some(),
            "safeword off: nothing to deny"
        );
        let mut pairs = vec![("REACTION_ROLES_MODE", "live")];
        pairs.extend(safeword_keys("live"));
        let plan = plan_for(&pairs, &config());
        assert!(plan.safeword.is_some() && plan.reaction_roles.is_some());
        // A safeword whose mode is a typo is off, as everywhere else.
        let plan = plan_for(
            &[("REACTION_ROLES_MODE", "live"), ("SAFEWORD_MODE", "lvie")],
            &config(),
        );
        assert!(plan.reaction_roles.is_some());
    }

    // ---- the pure start-up wiring ------------------------------------------------
    fn on(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name| names.contains(&name)
    }

    #[test]
    fn wiring_from_an_empty_environment_changes_nothing() {
        let plan = plan_for(&[], &config());
        // Even if every feature name were reported as running, nothing was planned.
        let wiring = wire(&plan, &|_| true);
        assert_eq!(
            wiring,
            Wiring {
                reaction_roles_owned: false,
                safeword_active: false,
                flash_log_excluded: vec![],
            }
        );
    }

    #[test]
    fn the_legacy_arms_stand_down_only_for_a_planned_and_running_reaction_feature() {
        let planned = plan_for(&[("REACTION_ROLES_MODE", "live")], &config());
        assert!(wire(&planned, &on(&["reaction_roles"])).reaction_roles_owned);
        // Planned, but dropped at start-up: the old arms keep working.
        assert!(!wire(&planned, &on(&[])).reaction_roles_owned);
        assert!(!wire(&planned, &on(&["flash", "safeword"])).reaction_roles_owned);
        // Running under that name but never planned (mode off): not owned either.
        let off = plan_for(&[], &config());
        assert!(!wire(&off, &on(&["reaction_roles"])).reaction_roles_owned);
    }

    #[test]
    fn the_safeword_command_needs_a_planned_and_running_feature() {
        let planned = plan_for(&safeword_keys("live"), &config());
        assert!(wire(&planned, &on(&["safeword"])).safeword_active);
        assert!(
            !wire(&planned, &on(&[])).safeword_active,
            "dropped at start-up"
        );
        let off = plan_for(&[], &config());
        assert!(!wire(&off, &on(&["safeword"])).safeword_active);
    }

    #[test]
    fn flash_ids_reach_the_log_exclusion_only_when_flash_is_planned() {
        let planned = plan_for(&flash_keys("live"), &config());
        assert_eq!(
            wire(&planned, &on(&[])).flash_log_excluded,
            vec![FLASH_A, FLASH_B]
        );
        // Flash that is only requested but refused (no channels) excludes nothing.
        let refused = plan_for(&[("FLASH_MODE", "live")], &config());
        assert!(wire(&refused, &on(&["flash"]))
            .flash_log_excluded
            .is_empty());
        let off = plan_for(&[], &config());
        assert!(wire(&off, &on(&["flash"])).flash_log_excluded.is_empty());
    }
}

#[cfg(test)]
mod welcome_wiring_tests {
    //! W3a wiring (`docs/verification/WIRING-C1.md`): the welcome plan and build, the real
    //! `WelcomeFeature` through `Features::dispatch`, and the startup preflight gate.
    use super::*;
    use crate::preflight::{
        PermissionLookup, PreflightInput, RoleFactsEntry, ADMINISTRATOR, EMBED_LINKS,
        MANAGE_MESSAGES, MANAGE_ROLES, SEND_MESSAGES,
    };
    use crate::preflight_wiring::fakes::{FakeFacts, FakeRoles};
    use crate::preflight_wiring::startup_preflight;
    use crate::roles::eligibility::RoleFacts;
    use crate::roles::grant::{RoleWriter, Sleeper, WriteError};
    use crate::roles::runtime::{RoleEntry, RoleSource, SourceError};
    use crate::welcome::discord::{AlertPost, FloodControlledAlerter, UnixClock};
    use crate::welcome::runtime::{
        GuildFacts, GuildLookup, MessageSender, OutgoingMessage, SendError, StaffAlerter,
    };
    use ::serenity::all::FullEvent;
    use std::path::Path;
    use std::sync::atomic::{AtomicI64, Ordering};

    // Synthetic ids. They are obviously made up and are not real Discord ids.
    const GUILD: u64 = 770_001;
    const OTHER_GUILD: u64 = 770_002;
    const BOT: u64 = 770_003;
    const MEMBER: u64 = 770_004;
    const CHANNEL: u64 = 770_010;
    const ALERT_CHANNEL: u64 = 770_011;
    const ROLE_LOW: u64 = 770_020;
    const ROLE_PRIV: u64 = 770_021;
    const ROLE_PROT: u64 = 770_022;
    const ROLE_MANAGED: u64 = 770_023;
    const ROLE_HIGH: u64 = 770_024;
    const BOT_ROLE: u64 = 770_030;
    const FLASH_CH: u64 = 770_040;
    const SW_ROLE: u64 = 770_050;
    const SW_RESTRICTED: u64 = 770_051;

    const TEMPLATE: &str = r#"{"embed":{"title":"Welcome {user_mention}","description":"You are our {member_ordinal} member."}}"#;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("w3a-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config() -> Config {
        Config {
            discord_token: "synthetic-token".to_string(),
            log_channel_id: 770_900,
            age_role_1: 1,
            age_role_2: 2,
            reaction_role_db_path: "reaction-test.sqlite".to_string(),
            staff_role_id: Some(770_005),
            tickets: None,
            ticket_archive_retention: crate::tickets::retention::RetentionPolicy::Disabled,
            ticket_privacy_lockdown: false,
            core_db_path: Some("core-test.sqlite".to_string()),
            guild_id: Some(GUILD),
            staff_alert_channel_id: Some(ALERT_CHANNEL),
            log_excluded_channel_ids: vec![],
            log_excluded_category_ids: vec![],
            protected_role_ids: vec![ROLE_PROT],
            invalid_core_keys: vec![],
        }
    }

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn plan_for(pairs: &[(&str, &str)], config: &Config) -> FeaturePlan {
        plan_features(&lookup(pairs), config)
    }

    fn welcome_keys(mode: &'static str) -> Vec<(&'static str, &'static str)> {
        vec![
            ("WELCOME_MODE", mode),
            ("WELCOME_CHANNEL_ID", "770010"),
            ("WELCOME_TEMPLATE_PATH", "welcome"),
            ("JOIN_ROLE_IDS", "770020"),
        ]
    }

    fn flash_keys() -> Vec<(&'static str, &'static str)> {
        vec![("FLASH_MODE", "live"), ("FLASH_CHANNEL_IDS", "770040")]
    }

    fn safeword_keys() -> Vec<(&'static str, &'static str)> {
        vec![
            ("SAFEWORD_MODE", "live"),
            ("SAFEWORD_ROLE_ID", "770050"),
            ("SAFEWORD_CHANNEL_ID", "770052"),
            ("SAFEWORD_MESSAGE_ID", "770053"),
            ("SAFEWORD_EMOJI_ID", "770054"),
            ("SAFEWORD_RESTRICTED_ROLE_IDS", "770051"),
        ]
    }

    fn all_keys() -> Vec<(&'static str, &'static str)> {
        let mut pairs = welcome_keys("live");
        pairs.extend(flash_keys());
        pairs.extend(safeword_keys());
        pairs
    }

    // ---- fakes behind the welcome traits ----------------------------------------

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Log {
        fn push(&self, line: String) {
            self.0.lock().unwrap().push(line);
        }
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FSource;

    #[::serenity::async_trait]
    impl RoleSource for FSource {
        async fn roles(&self, _g: u64) -> Result<Vec<RoleEntry>, SourceError> {
            let role = |id, position| RoleEntry {
                id,
                position,
                managed: false,
                permissions: 0,
            };
            Ok(vec![role(BOT_ROLE, 20), role(ROLE_LOW, 5), role(GUILD, 0)])
        }
        async fn member_roles(&self, _g: u64, member: u64) -> Result<Vec<u64>, SourceError> {
            Ok(if member == BOT {
                vec![BOT_ROLE]
            } else {
                vec![]
            })
        }
    }

    struct FWriter {
        log: Log,
        fail_status: Option<u16>,
    }

    impl RoleWriter for FWriter {
        fn add_role(&mut self, member: u64, role: u64) -> Result<(), WriteError> {
            self.log.push(format!("grant:{member}:{role}"));
            match self.fail_status {
                Some(status) => Err(WriteError {
                    status,
                    api_code: None,
                    retry_after_ms: None,
                }),
                None => Ok(()),
            }
        }
    }

    struct FSleeper;

    impl Sleeper for FSleeper {
        fn sleep_ms(&mut self, _ms: u64) {}
    }

    struct FSender(Log);

    impl MessageSender for FSender {
        fn send(&mut self, channel_id: u64, message: &OutgoingMessage) -> Result<(), SendError> {
            let title = message.embed.title.clone().unwrap_or_default();
            self.0.push(format!("send:{channel_id}:{title}"));
            Ok(())
        }
    }

    struct FAlerter(Log);

    impl StaffAlerter for FAlerter {
        fn alert(&mut self, channel_id: u64, code: &str) {
            self.0.push(format!("alert:{channel_id}:{code}"));
        }
    }

    struct FGuilds;

    #[::serenity::async_trait]
    impl GuildLookup for FGuilds {
        async fn guild_facts(&self, _guild_id: u64) -> Option<GuildFacts> {
            Some(GuildFacts {
                name: "synthetic-server".to_string(),
                member_count: 42,
            })
        }
    }

    type FakePorts = WelcomePorts<FSource, FWriter, FSleeper, FSender, FAlerter, FGuilds>;

    fn ports(log: &Log) -> FakePorts {
        WelcomePorts {
            source: FSource,
            writer: FWriter {
                log: log.clone(),
                fail_status: None,
            },
            sleeper: FSleeper,
            sender: FSender(log.clone()),
            alerter: FAlerter(log.clone()),
            guilds: FGuilds,
        }
    }

    fn read_template(key: &str) -> Option<String> {
        (key == "welcome").then(|| TEMPLATE.to_string())
    }

    fn build_one(plan: &FeaturePlan, log: &Log) -> Vec<Box<dyn Feature<u8, u16>>> {
        match build_welcome::<u8, u16, _, _, _, _, _, _>(plan, ports(log), BOT, &read_template) {
            Ok(Some(feature)) => vec![feature],
            other => panic!(
                "expected a welcome feature, got {:?}",
                other.map(|o| o.is_some())
            ),
        }
    }

    fn join(guild: u64, user: u64, bot: bool) -> FullEvent {
        let mut member = ::serenity::all::Member::default();
        member.user.id = ::serenity::all::UserId::new(user);
        member.user.name = "synthetic-user".to_string();
        member.user.bot = bot;
        member.guild_id = ::serenity::all::GuildId::new(guild);
        FullEvent::GuildMemberAddition { new_member: member }
    }

    fn leave(guild: u64) -> FullEvent {
        FullEvent::GuildMemberRemoval {
            guild_id: ::serenity::all::GuildId::new(guild),
            user: ::serenity::all::User::default(),
            member_data_if_available: None,
        }
    }

    fn core_settings(path: &Path, guild: GuildFilter) -> Settings {
        Settings {
            guild,
            core_db_path: Some(path.to_string_lossy().into_owned()),
            reserved_db_paths: vec![],
            staff_alert_channel: None,
            protected_role_count: 0,
            log_excluded_channels: 0,
            log_excluded_categories: 0,
        }
    }

    #[derive(Default)]
    struct Starter {
        kinds: Mutex<Vec<String>>,
    }

    impl JobStarter for Starter {
        fn start(
            &self,
            _db: tokio_rusqlite::Connection,
            registry: Registry,
        ) -> BoxFuture<'_, Result<StartedJobs, RunnerError>> {
            Box::pin(async move {
                self.kinds.lock().unwrap().push(format!("{registry:?}"));
                Ok(StartedJobs {
                    counts: ReportCounts::default(),
                    wake: Arc::new(|| {}),
                    shutdown: Arc::new(|| {}),
                    finished: Box::pin(async {}),
                })
            })
        }
    }

    async fn start_welcome(
        features: Vec<Box<dyn Feature<u8, u16>>>,
        dir: &Path,
        guild: GuildFilter,
        starter: &Starter,
    ) -> Features<u8, u16> {
        Features::start(
            StartRequest {
                features,
                settings: &core_settings(&dir.join("core.sqlite"), guild),
            },
            starter,
        )
        .await
    }

    // ---- planning from the environment -------------------------------------------

    #[test]
    fn welcome_is_absent_and_silent_from_an_empty_environment() {
        let plan = plan_for(&[], &config());
        assert!(plan.welcome.is_none());
        assert!(plan.lines.is_empty(), "{:?}", plan.lines);
        assert!(preflight_requests(&plan).is_empty());
    }

    #[test]
    fn welcome_keys_with_the_mode_off_or_unknown_change_nothing_even_when_bad() {
        for mode in ["off", "", "bogus"] {
            let mut pairs = welcome_keys(mode);
            pairs.push(("WELCOME_CHANNEL_ID", "not-an-id"));
            pairs.push(("JOIN_ROLE_IDS", "1,x"));
            let plan = plan_for(&pairs, &config());
            assert!(plan.welcome.is_none(), "{mode:?}");
            assert!(plan.lines.is_empty(), "{mode:?}: {:?}", plan.lines);
        }
    }

    #[test]
    fn a_complete_environment_plans_welcome_live_or_shadow_and_names_no_value() {
        for (word, mode) in [("live", Mode::Live), ("shadow", Mode::Shadow)] {
            let plan = plan_for(&welcome_keys(word), &config());
            let welcome = plan.welcome.as_ref().expect(word);
            assert_eq!(welcome.config.mode, mode);
            assert_eq!(welcome.home_guild, GUILD);
            assert_eq!(welcome.channel_id, CHANNEL);
            assert_eq!(welcome.config.join_role_ids, vec![ROLE_LOW]);
            assert_eq!(welcome.protected_role_ids, vec![ROLE_PROT]);
            assert_eq!(welcome.staff_alert_channel, Some(ALERT_CHANNEL));
            assert_eq!(plan.lines.len(), 1, "{:?}", plan.lines);
            let line = &plan.lines[0];
            assert!(line.starts_with("Feature welcome on"), "{line}");
            assert!(!line.contains("770"), "{line}");
        }
    }

    #[test]
    fn welcome_with_a_refused_setting_is_dropped_with_a_key_or_code_and_no_value() {
        struct Case {
            pairs: Vec<(&'static str, &'static str)>,
            config: Config,
            expect: &'static [&'static str],
        }
        let mut no_home = config();
        no_home.guild_id = None;
        let mut bad_protected = config();
        bad_protected.invalid_core_keys = vec![crate::config::PROTECTED_ROLE_IDS_KEY];
        let with = |key: &'static str, value: &'static str| {
            let mut pairs = welcome_keys("live");
            pairs.retain(|(k, _)| *k != key);
            if !value.is_empty() {
                pairs.push((key, value));
            }
            pairs
        };
        let cases = [
            Case {
                pairs: welcome_keys("live"),
                config: no_home,
                expect: &["home_guild_missing"],
            },
            Case {
                pairs: welcome_keys("live"),
                config: bad_protected,
                expect: &["protected_roles_invalid"],
            },
            Case {
                pairs: with("WELCOME_CHANNEL_ID", "770010,770011"),
                config: config(),
                expect: &["WELCOME_CHANNEL_ID", "invalid_setting"],
            },
            Case {
                pairs: with("WELCOME_TEMPLATE_PATH", "../SECRET-VALUE"),
                config: config(),
                expect: &["WELCOME_TEMPLATE_PATH", "invalid_setting"],
            },
            Case {
                pairs: with("JOIN_ROLE_IDS", "NOT-AN-ID-VALUE"),
                config: config(),
                expect: &["JOIN_ROLE_IDS", "invalid_setting"],
            },
            Case {
                pairs: with("WELCOME_CHANNEL_ID", ""),
                config: config(),
                expect: &["no_channel"],
            },
            Case {
                pairs: with("WELCOME_TEMPLATE_PATH", ""),
                config: config(),
                expect: &["no_template"],
            },
        ];
        for (index, case) in cases.iter().enumerate() {
            let plan = plan_for(&case.pairs, &case.config);
            assert!(plan.welcome.is_none(), "case {index}");
            assert_eq!(plan.lines.len(), 1, "case {index}: {:?}", plan.lines);
            let line = &plan.lines[0];
            assert!(
                line.starts_with("Feature welcome stays off"),
                "case {index}: {line}"
            );
            for word in case.expect {
                assert!(line.contains(word), "case {index}: {line}");
            }
            for secret in ["SECRET-VALUE", "NOT-AN-ID-VALUE", "770010", "770020"] {
                assert!(!line.contains(secret), "case {index}: {line}");
            }
        }
    }

    #[test]
    fn welcome_does_not_disturb_the_other_features_plans() {
        let with = plan_for(&all_keys(), &config());
        let mut without_welcome = all_keys();
        without_welcome.retain(|(k, _)| !k.starts_with("WELCOME") && *k != "JOIN_ROLE_IDS");
        let without = plan_for(&without_welcome, &config());
        assert!(with.welcome.is_some() && without.welcome.is_none());
        assert_eq!(
            with.flash
                .as_ref()
                .map(|f| f.policy().channel_ids().to_vec()),
            without
                .flash
                .as_ref()
                .map(|f| f.policy().channel_ids().to_vec())
        );
        assert_eq!(with.safeword.is_some(), without.safeword.is_some());
    }

    // ---- what the preflight is asked about ------------------------------------------

    #[test]
    fn the_preflight_is_asked_about_each_planned_feature_with_its_own_ids() {
        let plan = plan_for(&all_keys(), &config());
        let requests = preflight_requests(&plan);
        let find = |feature| requests.iter().find(|r| r.feature == feature).unwrap();
        let welcome = find(preflight::Feature::Welcome);
        assert_eq!(welcome.mode, Mode::Live);
        assert_eq!(welcome.channels, vec![CHANNEL]);
        assert_eq!(welcome.roles, vec![ROLE_LOW]);
        let flash = find(preflight::Feature::Flash);
        assert_eq!(flash.channels, vec![FLASH_CH]);
        let safeword = find(preflight::Feature::Safeword);
        assert_eq!(safeword.roles, vec![SW_ROLE, SW_RESTRICTED]);
        assert_eq!(requests.len(), 3);
        // Shadow is requested as shadow: the gate never upgrades it.
        let shadow = plan_for(&welcome_keys("shadow"), &config());
        assert_eq!(preflight_requests(&shadow)[0].mode, Mode::Shadow);
    }

    // ---- building ---------------------------------------------------------------------

    #[test]
    fn nothing_is_built_without_a_welcome_plan() {
        let plan = plan_for(&[], &config());
        let built = build_welcome::<u8, u16, _, _, _, _, _, _>(
            &plan,
            ports(&Log::default()),
            BOT,
            &read_template,
        );
        assert!(matches!(built, Ok(None)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_production_ports_satisfy_the_build_without_any_call_to_discord() {
        let plan = plan_for(&welcome_keys("live"), &config());
        let ports = crate::welcome::discord::serenity_ports(
            Arc::new(serenity::Http::new("synthetic-token")),
            Arc::new(serenity::Cache::new()),
            GUILD,
        );
        let built = build_welcome::<u8, u16, _, _, _, _, _, _>(&plan, ports, BOT, &read_template);
        let feature = built
            .ok()
            .flatten()
            .expect("built from the production ports");
        assert_eq!(feature.name(), "welcome");
        assert!(feature.needs_core_db());
    }

    #[test]
    fn a_planned_welcome_builds_a_feature_that_needs_the_core_database() {
        let plan = plan_for(&welcome_keys("live"), &config());
        let features = build_one(&plan, &Log::default());
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].name(), "welcome");
        assert!(features[0].enabled());
        assert!(features[0].needs_core_db());
        let mut registry = Registry::new();
        features[0].register_jobs(&mut registry).unwrap();
        let listed = format!("{registry:?}");
        assert!(listed.contains("welcome_grant_roles") && listed.contains("welcome_send_message"));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn a_template_that_cannot_be_used_stops_welcome_with_a_code_and_no_content() {
        let plan = plan_for(&welcome_keys("live"), &config());
        let unreadable =
            build_welcome::<u8, u16, _, _, _, _, _, _>(&plan, ports(&Log::default()), BOT, &|_| {
                None
            });
        let line = unreadable.err().expect("refused");
        assert!(
            line.contains("welcome") && line.contains("template_unreadable"),
            "{line}"
        );
        let broken =
            build_welcome::<u8, u16, _, _, _, _, _, _>(&plan, ports(&Log::default()), BOT, &|_| {
                Some("{\"secret\": 770010 not json".to_string())
            });
        let line = broken.err().expect("refused");
        assert!(line.contains("malformed_json"), "{line}");
        assert!(
            !line.contains("secret") && !line.contains("770010"),
            "{line}"
        );
        let unknown_field =
            build_welcome::<u8, u16, _, _, _, _, _, _>(&plan, ports(&Log::default()), BOT, &|_| {
                Some("{\"embed\":{},\"zzz\":1}".to_string())
            });
        assert!(unknown_field
            .err()
            .expect("refused")
            .contains("unknown_field"));
    }

    // ---- the real WelcomeFeature through Features::dispatch ----------------------------

    #[tokio::test]
    async fn a_human_join_grants_the_join_roles_and_posts_the_welcome() {
        let log = Log::default();
        let dir = scratch("human-join");
        let plan = plan_for(&welcome_keys("live"), &config());
        let f = start_welcome(
            build_one(&plan, &log),
            &dir,
            GuildFilter::Only(GUILD),
            &Starter::default(),
        )
        .await;
        assert!(f.is_active("welcome"));
        assert_eq!(f.dispatch(&1, &join(GUILD, MEMBER, false), &2).await, 1);
        let lines = log.lines();
        assert_eq!(
            lines,
            vec![
                format!("grant:{MEMBER}:{ROLE_LOW}"),
                format!("send:{CHANNEL}:Welcome <@{MEMBER}>"),
            ],
            "roles first, then the message, nothing else"
        );
    }

    #[tokio::test]
    async fn a_bot_account_joining_grants_nothing_and_posts_nothing() {
        let log = Log::default();
        let dir = scratch("bot-join");
        let plan = plan_for(&welcome_keys("live"), &config());
        let f = start_welcome(
            build_one(&plan, &log),
            &dir,
            GuildFilter::Only(GUILD),
            &Starter::default(),
        )
        .await;
        assert_eq!(f.dispatch(&1, &join(GUILD, BOT, true), &2).await, 1);
        assert!(log.lines().is_empty(), "{:?}", log.lines());
        // And a human right after still gets the welcome: the bot rule is per account.
        f.dispatch(&1, &join(GUILD, MEMBER, false), &2).await;
        assert_eq!(log.lines().len(), 2);
    }

    #[tokio::test]
    async fn a_join_in_another_guild_is_ignored_at_both_layers() {
        for (name, filter) in [
            ("two-guild-only", GuildFilter::Only(GUILD)),
            ("two-guild-any", GuildFilter::Any),
        ] {
            let log = Log::default();
            let dir = scratch(name);
            let plan = plan_for(&welcome_keys("live"), &config());
            let f = start_welcome(build_one(&plan, &log), &dir, filter, &Starter::default()).await;
            f.dispatch(&1, &join(OTHER_GUILD, MEMBER, false), &2).await;
            assert!(log.lines().is_empty(), "{name}: {:?}", log.lines());
        }
    }

    #[tokio::test]
    async fn only_the_member_join_event_reaches_welcome() {
        let log = Log::default();
        let dir = scratch("other-events");
        let plan = plan_for(&welcome_keys("live"), &config());
        let f = start_welcome(
            build_one(&plan, &log),
            &dir,
            GuildFilter::Only(GUILD),
            &Starter::default(),
        )
        .await;
        f.dispatch(&1, &leave(GUILD), &2).await;
        assert!(log.lines().is_empty(), "{:?}", log.lines());
    }

    #[tokio::test]
    async fn a_crash_between_claim_and_completion_abandons_welcome_jobs_on_the_real_runner() {
        use crate::jobs::queue::{self as jq, NewJob, ScheduleOutcome};
        let log = Log::default();
        let dir = scratch("crash-restart");
        let path = dir.join("core.sqlite");
        {
            let mut conn = crate::storage::open(&path.to_string_lossy(), &[]).unwrap();
            jq::init(&mut conn).unwrap();
            let now = 1_700_000_000i64;
            for (kind, key) in [
                ("welcome_grant_roles", "join-1-roles"),
                ("welcome_send_message", "join-1-message"),
            ] {
                let job = NewJob {
                    kind,
                    dedupe_key: key,
                    payload_json: "{}",
                    due_at: now,
                };
                let ScheduleOutcome::Created { id } = jq::schedule(&conn, &job, now).unwrap()
                else {
                    panic!("a fresh key is created");
                };
                // The worker claimed it and the process died before it finished.
                assert!(jq::try_claim(&conn, id, now).unwrap());
            }
        }
        let plan = plan_for(&welcome_keys("live"), &config());
        let f = Features::start(
            StartRequest {
                features: build_one(&plan, &log),
                settings: &core_settings(&path, GuildFilter::Only(GUILD)),
            },
            &TokioJobStarter,
        )
        .await;
        let started = f.lines().join("\n");
        f.stop().await;
        assert!(f.is_active("welcome"), "{started}");
        assert!(
            started.contains("requeued=0 abandoned_at_most_once=2 abandoned_unregistered=0"),
            "both are abandoned, never repeated: {started}"
        );
        assert!(
            log.lines().is_empty(),
            "nothing was granted or posted by a restart"
        );
    }

    #[tokio::test]
    async fn welcome_registers_only_its_two_job_kinds_and_opens_the_core_database() {
        let log = Log::default();
        let dir = scratch("kinds");
        let starter = Starter::default();
        let plan = plan_for(&welcome_keys("live"), &config());
        let f = start_welcome(
            build_one(&plan, &log),
            &dir,
            GuildFilter::Only(GUILD),
            &starter,
        )
        .await;
        assert!(f.is_active("welcome"));
        assert!(dir.join("core.sqlite").exists());
        let kinds = starter.kinds.lock().unwrap().clone();
        assert_eq!(
            kinds,
            vec![r#"["welcome_grant_roles", "welcome_send_message"]"#.to_string()]
        );
    }

    #[tokio::test]
    async fn an_unusable_core_database_keeps_welcome_off_and_nothing_half_starts() {
        let log = Log::default();
        let plan = plan_for(&welcome_keys("live"), &config());
        let mut settings = core_settings(Path::new("unused"), GuildFilter::Only(GUILD));
        settings.core_db_path = None;
        let f = Features::start(
            StartRequest {
                features: build_one(&plan, &log),
                settings: &settings,
            },
            &Starter::default(),
        )
        .await;
        assert!(!f.is_active("welcome"));
        assert_eq!(f.dispatch(&1, &join(GUILD, MEMBER, false), &2).await, 0);
        assert!(log.lines().is_empty());
        assert!(f
            .lines()
            .iter()
            .any(|l| l.contains("Core database unavailable")));
    }

    #[tokio::test]
    async fn welcome_in_shadow_mode_grants_and_posts_nothing() {
        let log = Log::default();
        let dir = scratch("shadow-join");
        let plan = plan_for(&welcome_keys("shadow"), &config());
        let f = start_welcome(
            build_one(&plan, &log),
            &dir,
            GuildFilter::Only(GUILD),
            &Starter::default(),
        )
        .await;
        f.dispatch(&1, &join(GUILD, MEMBER, false), &2).await;
        assert!(log.lines().is_empty(), "{:?}", log.lines());
    }

    // ---- the production alerter on the real feature path -----------------------------

    #[derive(Clone, Default)]
    struct Clock(Arc<AtomicI64>);

    impl UnixClock for Clock {
        fn now_secs(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[derive(Clone, Default)]
    struct Posts(Arc<Mutex<Vec<String>>>);

    impl AlertPost for Posts {
        fn post(&self, channel_id: u64, text: &str) {
            self.0.lock().unwrap().push(format!("{channel_id}:{text}"));
        }
    }

    #[tokio::test]
    async fn a_burst_of_failing_joins_alerts_staff_once_per_code_until_the_window_passes() {
        let log = Log::default();
        let (clock, posts) = (Clock::default(), Posts::default());
        let mut p = ports(&log);
        p.writer.fail_status = Some(403);
        let welcome = WelcomePorts {
            source: p.source,
            writer: p.writer,
            sleeper: p.sleeper,
            sender: p.sender,
            alerter: FloodControlledAlerter::new(clock.clone(), posts.clone()),
            guilds: p.guilds,
        };
        let plan = plan_for(&welcome_keys("live"), &config());
        let feature =
            build_welcome::<u8, u16, _, _, _, _, _, _>(&plan, welcome, BOT, &read_template)
                .ok()
                .flatten()
                .expect("built");
        let dir = scratch("flood");
        let f = start_welcome(
            vec![feature],
            &dir,
            GuildFilter::Only(GUILD),
            &Starter::default(),
        )
        .await;
        clock.0.store(50_000, Ordering::SeqCst);
        for user in 0..20u64 {
            f.dispatch(&1, &join(GUILD, MEMBER + user, false), &2).await;
        }
        assert_eq!(
            posts.0.lock().unwrap().len(),
            1,
            "{:?}",
            posts.0.lock().unwrap()
        );
        assert!(posts.0.lock().unwrap()[0].starts_with(&format!("{ALERT_CHANNEL}:")));
        clock.0.store(50_300, Ordering::SeqCst);
        f.dispatch(&1, &join(GUILD, MEMBER + 99, false), &2).await;
        assert_eq!(
            posts.0.lock().unwrap().len(),
            2,
            "the alert comes back after the window"
        );
    }

    // ---- ALL-OFF PARITY ----------------------------------------------------------------

    #[tokio::test]
    async fn from_an_empty_environment_nothing_is_asked_built_opened_or_printed() {
        let plan = plan_for(&[], &config());
        let (facts, roles) = (FakeFacts::default(), FakeRoles::new(vec![], vec![]));
        let gated = startup_preflight(&plan, BOT, &facts, &roles).await;
        assert!(gated.lines.is_empty(), "{:?}", gated.lines);
        assert_eq!(
            facts.calls() + roles.calls(),
            0,
            "nothing is asked of Discord"
        );
        assert!(gated.plan.flash.is_none() && gated.plan.safeword.is_none());
        assert!(gated.plan.reaction_roles.is_none() && gated.plan.welcome.is_none());
        let built = build_welcome::<u8, u16, _, _, _, _, _, _>(
            &gated.plan,
            ports(&Log::default()),
            BOT,
            &read_template,
        );
        assert!(matches!(built, Ok(None)));
        let dir = scratch("all-off");
        let starter = Starter::default();
        let f = start_welcome(vec![], &dir, GuildFilter::Only(GUILD), &starter).await;
        assert!(!dir.join("core.sqlite").exists(), "no core database");
        assert!(starter.kinds.lock().unwrap().is_empty(), "no runner");
        assert!(f.lines().is_empty());
        assert_eq!(f.dispatch(&1, &join(GUILD, MEMBER, false), &2).await, 0);
    }

    #[tokio::test]
    async fn modes_off_with_every_key_set_still_ask_and_print_nothing() {
        let mut pairs = welcome_keys("off");
        pairs.extend([("FLASH_MODE", "off"), ("SAFEWORD_MODE", "off")]);
        let plan = plan_for(&pairs, &config());
        let (facts, roles) = (FakeFacts::default(), FakeRoles::new(vec![], vec![]));
        let gated = startup_preflight(&plan, BOT, &facts, &roles).await;
        assert!(gated.lines.is_empty() && plan.lines.is_empty());
        assert_eq!(facts.calls() + roles.calls(), 0);
    }

    // ---- the preflight gate -------------------------------------------------------------

    fn role_entries() -> Vec<RoleEntry> {
        let role = |id, position, permissions: u64, managed| RoleEntry {
            id,
            position,
            managed,
            permissions,
        };
        vec![
            role(BOT_ROLE, 20, 0, false),
            role(ROLE_LOW, 5, 0, false),
            role(ROLE_PRIV, 6, ADMINISTRATOR, false),
            role(ROLE_PROT, 7, 0, false),
            role(ROLE_MANAGED, 8, 0, true),
            role(ROLE_HIGH, 30, 0, false),
            role(SW_ROLE, 4, 0, false),
            role(SW_RESTRICTED, 3, 0, false),
            role(GUILD, 0, 0, false),
        ]
    }

    fn healthy_facts() -> FakeFacts {
        FakeFacts {
            guild: Some(MANAGE_ROLES),
            channels: [
                (CHANNEL, Some(SEND_MESSAGES | EMBED_LINKS)),
                (FLASH_CH, Some(MANAGE_MESSAGES)),
            ]
            .into_iter()
            .collect(),
            ..FakeFacts::default()
        }
    }

    fn healthy_roles() -> FakeRoles {
        FakeRoles::new(role_entries(), vec![BOT_ROLE])
    }

    fn lines_text(gated: &Gated) -> String {
        gated.lines.join("\n")
    }

    #[tokio::test]
    async fn every_ready_feature_starts_and_the_report_lists_each_one() {
        let plan = plan_for(&all_keys(), &config());
        let gated = startup_preflight(&plan, BOT, &healthy_facts(), &healthy_roles()).await;
        assert!(gated.plan.flash.is_some());
        assert!(gated.plan.safeword.is_some());
        assert!(gated.plan.welcome.is_some());
        assert_eq!(
            gated.lines,
            vec![
                "Preflight flash: ready".to_string(),
                "Preflight safeword: ready".to_string(),
                "Preflight welcome: ready".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_disabled_verdict_stops_that_feature_while_the_others_still_start() {
        let plan = plan_for(&all_keys(), &config());
        let mut facts = healthy_facts();
        facts.channels.insert(CHANNEL, Some(SEND_MESSAGES)); // no EMBED_LINKS for welcome
        let gated = startup_preflight(&plan, BOT, &facts, &healthy_roles()).await;
        assert!(gated.plan.welcome.is_none(), "welcome is refused");
        assert!(gated.plan.flash.is_some() && gated.plan.safeword.is_some());
        let text = lines_text(&gated);
        assert!(
            text.contains("Preflight welcome: disabled (missing_embed_links)"),
            "{text}"
        );
        assert!(text.contains("Feature welcome stays off"), "{text}");
        // The features that are left really start; welcome never registers.
        let log = Log::default();
        let dir = scratch("gate-others-start");
        let starter = Starter::default();
        let mut built: Vec<Box<dyn Feature<u8, u16>>> = Vec::new();
        let services = Services {
            http: Arc::new(serenity::Http::new("synthetic-token")),
            cache: Arc::new(serenity::Cache::new()),
            bot_user_id: BOT,
            reaction: Arc::new(NoReaction),
            safeword: None,
        };
        built.extend(build_features::<u8, u16>(&gated.plan, &services));
        if let Ok(Some(feature)) = build_welcome::<u8, u16, _, _, _, _, _, _>(
            &gated.plan,
            ports(&log),
            BOT,
            &read_template,
        ) {
            built.push(feature);
        }
        let f = start_welcome(built, &dir, GuildFilter::Only(GUILD), &starter).await;
        assert!(f.is_active("flash"));
        assert!(!f.is_active("welcome"));
        let kinds = starter.kinds.lock().unwrap().join(" ");
        assert!(!kinds.contains("welcome"), "{kinds}");
    }

    #[tokio::test]
    async fn each_missing_permission_disables_the_feature_with_its_own_code() {
        let plan = plan_for(&all_keys(), &config());
        // (what to take away, [flash gone, safeword gone, welcome gone], the code it must name)
        type Tweak = fn(&mut FakeFacts);
        let cases: Vec<(Tweak, [bool; 3], &str)> = vec![
            (
                |f| f.guild = Some(0),
                [false, true, true],
                "missing_manage_roles",
            ),
            (
                |f| {
                    f.channels.insert(CHANNEL, Some(EMBED_LINKS));
                },
                [false, false, true],
                "missing_send_messages",
            ),
            (
                |f| {
                    f.channels.insert(FLASH_CH, Some(SEND_MESSAGES));
                },
                [true, false, false],
                "missing_manage_messages",
            ),
        ];
        for (tweak, gone, code) in cases {
            let mut facts = healthy_facts();
            tweak(&mut facts);
            let gated = startup_preflight(&plan, BOT, &facts, &healthy_roles()).await;
            let text = lines_text(&gated);
            assert!(text.contains(code), "{code}: {text}");
            assert_eq!(gated.plan.flash.is_none(), gone[0], "{code}: {text}");
            assert_eq!(gated.plan.safeword.is_none(), gone[1], "{code}: {text}");
            assert_eq!(gated.plan.welcome.is_none(), gone[2], "{code}: {text}");
        }
    }

    #[tokio::test]
    async fn a_failed_permission_lookup_disables_with_permissions_unknown() {
        let plan = plan_for(&all_keys(), &config());
        let mut facts = healthy_facts();
        facts.channels.insert(CHANNEL, None); // the lookup failed
        facts.channels.remove(&FLASH_CH); // and one that was never answered
        let gated = startup_preflight(&plan, BOT, &facts, &healthy_roles()).await;
        assert!(gated.plan.welcome.is_none() && gated.plan.flash.is_none());
        assert!(
            gated.plan.safeword.is_some(),
            "safeword needs no channel permission"
        );
        let text = lines_text(&gated);
        assert!(
            text.contains("welcome: disabled (permissions_unknown)"),
            "{text}"
        );
        assert!(
            text.contains("flash: disabled (permissions_unknown)"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn failed_role_lookups_disable_the_role_granting_features() {
        let plan = plan_for(&all_keys(), &config());
        let mut roles = healthy_roles();
        roles.roles = Err(SourceError::Unavailable);
        let gated = startup_preflight(&plan, BOT, &healthy_facts(), &roles).await;
        assert!(gated.plan.welcome.is_none() && gated.plan.safeword.is_none());
        assert!(gated.plan.flash.is_some(), "flash grants no role");
        assert!(lines_text(&gated).contains("bot_top_position_unknown"));
    }

    fn input_with_roles(plan: &FeaturePlan, role_facts: Vec<RoleFactsEntry>) -> PreflightInput {
        PreflightInput {
            features: preflight_requests(plan),
            guild_permissions: MANAGE_ROLES,
            channel_permissions: vec![
                PermissionLookup::known(CHANNEL, SEND_MESSAGES | EMBED_LINKS),
                PermissionLookup::known(FLASH_CH, MANAGE_MESSAGES),
            ],
            category_permissions: vec![],
            role_facts,
            bot_top_position: Some(20),
        }
    }

    fn found(id: u64, position: u16, permissions: u64, managed: bool) -> RoleFactsEntry {
        RoleFactsEntry::found(
            id,
            RoleFacts {
                id,
                position,
                managed,
                permissions,
                exists: true,
            },
        )
    }

    #[test]
    fn a_refused_join_role_drops_welcome_with_the_key_and_the_policy_code() {
        // (the join role, its facts, the code the line must carry)
        let cases: Vec<(u64, RoleFactsEntry, &str)> = vec![
            (
                ROLE_PRIV,
                found(ROLE_PRIV, 6, ADMINISTRATOR, false),
                "privileged_permission",
            ),
            (ROLE_PROT, found(ROLE_PROT, 7, 0, false), "protected_role"),
            (
                ROLE_MANAGED,
                found(ROLE_MANAGED, 8, 0, true),
                "managed_role",
            ),
            (GUILD, found(GUILD, 0, 0, false), "everyone_role"),
        ];
        for (role, entry, code) in cases {
            let join_roles = role.to_string();
            let mut pairs = welcome_keys("live");
            pairs.retain(|(k, _)| *k != "JOIN_ROLE_IDS");
            pairs.push(("JOIN_ROLE_IDS", join_roles.as_str()));
            let plan = plan_for(&pairs, &config());
            assert!(
                plan.welcome.is_some(),
                "{code}: the plan itself accepts the ids"
            );
            let gated = gate_plan(&plan, &input_with_roles(&plan, vec![entry]));
            assert!(gated.plan.welcome.is_none(), "{code}");
            let text = lines_text(&gated);
            assert!(
                text.contains("Preflight welcome: ready"),
                "preflight alone passes: {text}"
            );
            assert!(
                text.contains(&format!(
                    "Feature welcome stays off: join role refused (JOIN_ROLE_IDS: {code})"
                )),
                "{code}: {text}"
            );
            assert!(!text.contains(&role.to_string()), "{code}: no id in {text}");
        }
    }

    #[test]
    fn a_join_role_that_is_missing_unknown_or_above_the_bot_is_refused_too() {
        let plan = plan_for(&welcome_keys("live"), &config());
        let cases = vec![
            (vec![RoleFactsEntry::missing(ROLE_LOW)], "role_missing"),
            (
                vec![RoleFactsEntry::unavailable(ROLE_LOW)],
                "role_facts_unavailable",
            ),
            (vec![], "role_facts_unavailable"),
            (vec![found(ROLE_LOW, 20, 0, false)], "role_above_bot"),
            (vec![found(ROLE_LOW, 21, 0, false)], "role_above_bot"),
        ];
        for (facts, code) in cases {
            let gated = gate_plan(&plan, &input_with_roles(&plan, facts));
            assert!(gated.plan.welcome.is_none(), "{code}");
            assert!(
                lines_text(&gated).contains(code),
                "{code}: {}",
                lines_text(&gated)
            );
        }
        // An unknown bot top is refused with its code, never read as zero.
        let mut input = input_with_roles(&plan, vec![found(ROLE_LOW, 5, 0, false)]);
        input.bot_top_position = None;
        let gated = gate_plan(&plan, &input);
        assert!(gated.plan.welcome.is_none());
        assert!(lines_text(&gated).contains("bot_top_position_unknown"));
        // And the same role just below the bot passes.
        let ok = gate_plan(
            &plan,
            &input_with_roles(&plan, vec![found(ROLE_LOW, 19, 0, false)]),
        );
        assert!(ok.plan.welcome.is_some());
    }

    #[test]
    fn welcome_without_join_roles_still_needs_manage_roles_and_channel_permissions() {
        let mut pairs = welcome_keys("live");
        pairs.retain(|(k, _)| *k != "JOIN_ROLE_IDS");
        let plan = plan_for(&pairs, &config());
        let ok = gate_plan(&plan, &input_with_roles(&plan, vec![]));
        assert!(ok.plan.welcome.is_some());
        let mut input = input_with_roles(&plan, vec![]);
        input.guild_permissions = 0;
        assert!(gate_plan(&plan, &input).plan.welcome.is_none());
    }

    #[tokio::test]
    async fn a_safeword_that_cannot_start_takes_reaction_roles_down_with_it() {
        let mut pairs = all_keys();
        pairs.push(("REACTION_ROLES_MODE", "live"));
        let plan = plan_for(&pairs, &config());
        assert!(plan.reaction_roles.is_some(), "planned before the gate");
        let mut facts = healthy_facts();
        facts.guild = Some(0);
        let gated = startup_preflight(&plan, BOT, &facts, &healthy_roles()).await;
        assert!(gated.plan.safeword.is_none());
        assert!(
            gated.plan.reaction_roles.is_none(),
            "fail closed, as the plan rule says"
        );
        let text = lines_text(&gated);
        assert!(text.contains("Feature reaction_roles stays off"), "{text}");
        assert!(text.contains("safeword_unavailable"), "{text}");
        // A healthy bot leaves reaction roles alone.
        let healthy = startup_preflight(&plan, BOT, &healthy_facts(), &healthy_roles()).await;
        assert!(healthy.plan.reaction_roles.is_some() && healthy.plan.safeword.is_some());
    }

    #[tokio::test]
    async fn a_dropped_flash_keeps_its_channels_out_of_the_log() {
        let plan = plan_for(&flash_keys(), &config());
        let mut facts = healthy_facts();
        facts.channels.insert(FLASH_CH, Some(0));
        let gated = startup_preflight(&plan, BOT, &facts, &healthy_roles()).await;
        assert!(gated.plan.flash.is_none());
        // The wiring is decided from the ORIGINAL plan: privacy is not given back.
        assert_eq!(wire(&plan, &|_| false).flash_log_excluded, vec![FLASH_CH]);
    }

    // ---- no value in any line ------------------------------------------------------------

    #[tokio::test]
    async fn preflight_and_startup_lines_never_carry_an_id_a_path_or_a_value() {
        // Large sentinel ids: any leak of one is a 15-digit run.
        const BIG_GUILD: u64 = 987_654_321_012_001;
        const BIG_CHANNEL: u64 = 987_654_321_012_002;
        const BIG_ROLE: u64 = 987_654_321_012_003;
        const BIG_FLASH: u64 = 987_654_321_012_004;
        const BIG_BOT_ROLE: u64 = 987_654_321_012_005;
        let mut cfg = config();
        cfg.guild_id = Some(BIG_GUILD);
        cfg.protected_role_ids = vec![];
        let ids: Vec<(String, String)> = vec![
            ("WELCOME_MODE".into(), "live".into()),
            ("WELCOME_CHANNEL_ID".into(), BIG_CHANNEL.to_string()),
            ("WELCOME_TEMPLATE_PATH".into(), "sentinel-template".into()),
            ("JOIN_ROLE_IDS".into(), BIG_ROLE.to_string()),
            ("FLASH_MODE".into(), "live".into()),
            ("FLASH_CHANNEL_IDS".into(), BIG_FLASH.to_string()),
        ];
        let owned = ids.clone();
        let lookup = move |key: &str| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let plan = plan_features(&lookup, &cfg);
        let role = |id, position| RoleEntry {
            id,
            position,
            managed: false,
            permissions: 0,
        };
        let roles = FakeRoles::new(
            vec![role(BIG_BOT_ROLE, 20), role(BIG_ROLE, 5)],
            vec![BIG_BOT_ROLE],
        );
        let scenarios: Vec<FakeFacts> = vec![
            FakeFacts {
                guild: Some(MANAGE_ROLES),
                channels: [
                    (BIG_CHANNEL, Some(SEND_MESSAGES | EMBED_LINKS)),
                    (BIG_FLASH, Some(MANAGE_MESSAGES)),
                ]
                .into_iter()
                .collect(),
                ..FakeFacts::default()
            },
            FakeFacts::default(),
        ];
        let mut all_lines: Vec<String> = plan.lines.clone();
        for facts in &scenarios {
            let gated = startup_preflight(&plan, BIG_BOT_ROLE, facts, &roles).await;
            all_lines.extend(gated.lines);
        }
        assert!(
            all_lines.iter().any(|l| l.contains("ready")),
            "{all_lines:?}"
        );
        assert!(
            all_lines.iter().any(|l| l.contains("disabled")),
            "{all_lines:?}"
        );
        for line in &all_lines {
            let mut run = 0;
            for c in line.chars() {
                run = if c.is_ascii_digit() { run + 1 } else { 0 };
                assert!(run < 5, "an id-like number in {line:?}");
            }
            for forbidden in [
                "sentinel-template",
                "welcome_templates",
                "/",
                "\\",
                ".json",
                "987654",
            ] {
                assert!(!line.contains(forbidden), "{forbidden}: {line:?}");
            }
        }
    }

    struct NoReaction;

    #[::serenity::async_trait]
    impl ReactionServices for NoReaction {
        async fn bound_roles(
            &self,
            _message: u64,
            _key: &str,
        ) -> Result<Vec<u64>, crate::role_menus::reactions::StorageCode> {
            Ok(vec![])
        }
        fn role_source(&self) -> &dyn RoleSource {
            &FSource
        }
        async fn user_is_bot(&self, _g: u64, _u: u64) -> Option<bool> {
            Some(false)
        }
        fn safeword_state(&self, _g: u64, _u: u64) -> crate::roles::eligibility::SafewordState {
            crate::roles::eligibility::SafewordState::Inactive
        }
        fn role_writer(&self, _g: u64) -> Box<dyn RoleWriter + Send> {
            Box::new(FWriter {
                log: Log::default(),
                fail_status: None,
            })
        }
        fn role_remover(
            &self,
            _g: u64,
        ) -> Box<dyn crate::role_menus::reactions::RoleRemover + Send> {
            Box::new(NoRemover)
        }
        fn sleeper(&self) -> Box<dyn Sleeper + Send> {
            Box::new(FSleeper)
        }
        fn report(&self, _line: &str) {}
    }

    struct NoRemover;

    impl crate::role_menus::reactions::RoleRemover for NoRemover {
        fn remove_role(&mut self, _m: u64, _r: u64) -> Result<(), WriteError> {
            Ok(())
        }
    }

    // ---- W3a round 2: the production composition, the safeword rule, the id map -------------

    fn names(composed: &GatedFeatures<u8, u16>) -> Vec<&'static str> {
        composed.features.iter().map(|f| f.name()).collect()
    }

    async fn compose_with(
        plan: &FeaturePlan,
        facts: &FakeFacts,
        roles: &FakeRoles,
        read: &dyn Fn(&str) -> Option<String>,
    ) -> GatedFeatures<u8, u16> {
        start_gated::<u8, u16>(GatedStart {
            plan,
            bot_user_id: BOT,
            http: Arc::new(serenity::Http::new("synthetic-token")),
            cache: Arc::new(serenity::Cache::new()),
            facts,
            roles,
            store: StoreSpec {
                path: None,
                reserved: vec![],
            },
            protected_role_ids: vec![],
            reaction: &|_lookup| Arc::new(NoReaction),
            read_template: read,
        })
        .await
    }

    async fn compose(plan: &FeaturePlan, facts: &FakeFacts, roles: &FakeRoles) -> GatedFeatures<u8, u16> {
        compose_with(plan, facts, roles, &read_template).await
    }

    #[tokio::test]
    async fn the_production_composition_builds_only_what_the_preflight_lets_through() {
        let plan = plan_for(&all_keys(), &config());
        let mut facts = healthy_facts();
        facts.channels.insert(CHANNEL, Some(SEND_MESSAGES)); // welcome lacks EMBED_LINKS
        let composed = compose(&plan, &facts, &healthy_roles()).await;
        assert_eq!(names(&composed), vec!["flash", "safeword"]);
        assert!(composed.safeword.is_some());
        let text = composed.lines.join("\n");
        assert!(
            text.contains("Preflight welcome: disabled (missing_embed_links)"),
            "{text}"
        );
        // And what is registered really starts: welcome opens no kind of its own.
        let dir = scratch("compose-others-start");
        let starter = Starter::default();
        let f = start_welcome(composed.features, &dir, GuildFilter::Only(GUILD), &starter).await;
        assert!(f.is_active("flash") && f.is_active("safeword"));
        assert!(!f.is_active("welcome"));
        assert!(!starter.kinds.lock().unwrap().join(" ").contains("welcome"));
    }

    #[tokio::test]
    async fn a_dropped_safeword_leaves_no_runtime_and_takes_reaction_roles_with_it() {
        let mut pairs = all_keys();
        pairs.push(("REACTION_ROLES_MODE", "live"));
        let plan = plan_for(&pairs, &config());
        let mut facts = healthy_facts();
        facts.guild = Some(0); // no manage roles: the role features are refused
        let composed = compose(&plan, &facts, &healthy_roles()).await;
        assert_eq!(names(&composed), vec!["flash"]);
        assert!(composed.safeword.is_none(), "no safeword runtime for a refused safeword");
    }

    #[tokio::test]
    async fn the_production_composition_builds_every_ready_feature_in_order() {
        let mut pairs = all_keys();
        pairs.push(("REACTION_ROLES_MODE", "live"));
        let plan = plan_for(&pairs, &config());
        let composed = compose(&plan, &healthy_facts(), &healthy_roles()).await;
        assert_eq!(
            names(&composed),
            vec!["flash", "safeword", reactions::FEATURE_NAME, "welcome"]
        );
        assert!(composed.safeword.is_some());
        assert_eq!(
            composed.lines,
            vec![
                "Preflight flash: ready".to_string(),
                "Preflight safeword: ready".to_string(),
                "Preflight welcome: ready".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn the_production_composition_of_an_empty_plan_asks_builds_and_prints_nothing() {
        let plan = plan_for(&[], &config());
        let (facts, roles) = (FakeFacts::default(), FakeRoles::new(vec![], vec![]));
        let composed = compose(&plan, &facts, &roles).await;
        assert!(composed.features.is_empty() && composed.lines.is_empty());
        assert!(composed.safeword.is_none());
        assert_eq!(facts.calls() + roles.calls(), 0);
    }

    #[tokio::test]
    async fn the_production_composition_reports_a_template_it_cannot_use_by_code_only() {
        let plan = plan_for(&welcome_keys("live"), &config());
        let composed = compose_with(
            &plan,
            &healthy_facts(),
            &healthy_roles(),
            &|_| Some("{\"secret\": 770010 not json".to_string()),
        )
        .await;
        assert!(names(&composed).is_empty(), "no half-started welcome");
        let text = composed.lines.join("\n");
        assert!(text.contains("malformed_json"), "{text}");
        assert!(!text.contains("secret") && !text.contains("770010"), "{text}");
    }

    // ---- welcome and the safeword restriction (K03 rule 6) ----------------------------------

    #[test]
    fn a_join_role_the_safeword_restricts_keeps_welcome_off_with_the_key_and_a_code() {
        for roles in ["770051", "770050", "770020,770051", "770050,770020"] {
            let mut pairs = all_keys();
            pairs.retain(|(k, _)| *k != "JOIN_ROLE_IDS");
            pairs.push(("JOIN_ROLE_IDS", roles));
            let plan = plan_for(&pairs, &config());
            assert!(plan.safeword.is_some(), "{roles}");
            assert!(plan.welcome.is_none(), "{roles}: welcome must not start");
            let text = plan.lines.join("\n");
            assert!(
                text.contains("Feature welcome stays off: join role refused (JOIN_ROLE_IDS: safeword_restricted)"),
                "{roles}: {text}"
            );
            assert!(!text.contains("7700"), "{roles}: no id in {text}");
        }
    }

    #[test]
    fn welcome_stays_off_while_a_requested_safeword_is_not_running() {
        for mode in ["live", "shadow"] {
            let mut pairs = all_keys();
            pairs.retain(|(k, _)| *k != "SAFEWORD_ROLE_ID" && *k != "SAFEWORD_MODE");
            pairs.push(("SAFEWORD_MODE", mode));
            let plan = plan_for(&pairs, &config());
            assert!(plan.safeword.is_none(), "{mode}: the safeword itself is refused");
            assert!(plan.welcome.is_none(), "{mode}");
            let text = plan.lines.join("\n");
            assert!(
                text.contains("Feature welcome stays off: safeword requested but not running (safeword_unavailable)"),
                "{mode}: {text}"
            );
        }
    }

    #[test]
    fn without_the_safeword_requested_the_restricted_list_does_not_bind_welcome() {
        let mut pairs = welcome_keys("live");
        pairs.push(("SAFEWORD_RESTRICTED_ROLE_IDS", "770020"));
        pairs.push(("SAFEWORD_MODE", "off"));
        let plan = plan_for(&pairs, &config());
        assert!(plan.welcome.is_some());
        assert!(plan.lines.iter().all(|l| !l.contains("safeword")), "{:?}", plan.lines);
    }

    #[tokio::test]
    async fn a_restricted_join_role_never_starts_welcome_through_the_production_composition() {
        let mut pairs = all_keys();
        pairs.retain(|(k, _)| *k != "JOIN_ROLE_IDS");
        pairs.push(("JOIN_ROLE_IDS", "770051"));
        let plan = plan_for(&pairs, &config());
        let composed = compose(&plan, &healthy_facts(), &healthy_roles()).await;
        assert_eq!(names(&composed), vec!["flash", "safeword"]);
        // A member joining then reaches no welcome feature: nothing is granted or posted.
        let dir = scratch("restricted-join");
        let f = start_welcome(composed.features, &dir, GuildFilter::Only(GUILD), &Starter::default()).await;
        assert!(!f.is_active("welcome"));
        assert_eq!(f.dispatch(&1, &join(GUILD, MEMBER, false), &2).await, 0);
    }

    #[tokio::test]
    async fn a_safeword_dropped_by_the_preflight_leaves_welcome_with_unrestricted_roles_only() {
        let plan = plan_for(&all_keys(), &config());
        let mut entries = role_entries();
        entries.retain(|e| e.id != SW_ROLE);
        entries.push(RoleEntry {
            id: SW_ROLE,
            position: 30, // above the bot
            managed: false,
            permissions: 0,
        });
        let roles = FakeRoles::new(entries, vec![BOT_ROLE]);
        let gated = startup_preflight(&plan, BOT, &healthy_facts(), &roles).await;
        assert!(gated.plan.safeword.is_none());
        let welcome = gated.plan.welcome.as_ref().expect("welcome still starts");
        assert!(!welcome.config.join_role_ids.contains(&SW_RESTRICTED));
        assert!(!welcome.config.join_role_ids.contains(&SW_ROLE));
    }

    // ---- the safeword preflight request names every role the swap writes -------------------

    #[test]
    fn the_safeword_request_names_its_role_and_each_restricted_role_once() {
        let mut pairs = all_keys();
        pairs.retain(|(k, _)| *k != "SAFEWORD_RESTRICTED_ROLE_IDS");
        pairs.push(("SAFEWORD_RESTRICTED_ROLE_IDS", "770051,770055,770051"));
        let plan = plan_for(&pairs, &config());
        let requests = preflight_requests(&plan);
        let safeword = requests
            .iter()
            .find(|r| r.feature == preflight::Feature::Safeword)
            .unwrap();
        assert_eq!(safeword.roles, vec![SW_ROLE, 770_051, 770_055]);
    }

    #[tokio::test]
    async fn a_restricted_role_that_is_gone_or_above_the_bot_refuses_the_safeword_at_startup() {
        let mut pairs = all_keys();
        pairs.push(("REACTION_ROLES_MODE", "live"));
        let plan = plan_for(&pairs, &config());
        // (the restricted role's entry in the guild, the code the line must carry)
        let cases: Vec<(Option<u16>, &str)> = vec![(None, "role_missing"), (Some(21), "role_above_bot")];
        for (position, code) in cases {
            let mut entries = role_entries();
            entries.retain(|e| e.id != SW_RESTRICTED);
            if let Some(position) = position {
                entries.push(RoleEntry {
                    id: SW_RESTRICTED,
                    position,
                    managed: false,
                    permissions: 0,
                });
            }
            let roles = FakeRoles::new(entries, vec![BOT_ROLE]);
            let gated = startup_preflight(&plan, BOT, &healthy_facts(), &roles).await;
            let text = lines_text(&gated);
            assert!(gated.plan.safeword.is_none(), "{code}: {text}");
            assert!(gated.plan.reaction_roles.is_none(), "{code}: cascade");
            assert!(text.contains("Preflight safeword: disabled"), "{code}: {text}");
            assert!(text.contains(code), "{code}: {text}");
            assert!(gated.plan.flash.is_some() && gated.plan.welcome.is_some());
            assert!(!text.contains("770051"), "{code}: no id in {text}");
        }
    }

    // ---- the id map of the template ----------------------------------------------------------

    fn linked(mode: &'static str) -> Vec<(&'static str, &'static str)> {
        let mut pairs = welcome_keys(mode);
        pairs.push(("WELCOME_CHANNEL_LINKS", "rules=770060,tickets=770062"));
        pairs.push(("WELCOME_ROLE_LINKS", "pings=770061"));
        pairs
    }

    #[test]
    fn the_template_links_are_planned_from_the_environment() {
        let plan = plan_for(&linked("live"), &config());
        let ids = &plan.welcome.as_ref().expect("planned").ids;
        assert_eq!(ids.channel("rules"), Some(770_060));
        assert_eq!(ids.channel("tickets"), Some(770_062));
        assert_eq!(ids.role("pings"), Some(770_061));
        assert_eq!(plan.lines.len(), 1, "{:?}", plan.lines);
        assert!(!plan.lines[0].contains("7700"), "{:?}", plan.lines);
    }

    #[test]
    fn a_bad_link_list_drops_welcome_with_its_key_and_a_code_and_a_mode_off_stays_silent() {
        for (key, value) in [
            ("WELCOME_CHANNEL_LINKS", "rules=0"),
            ("WELCOME_CHANNEL_LINKS", "rules=770060,rules=770061"),
            ("WELCOME_ROLE_LINKS", "../x=770061"),
        ] {
            let mut pairs = welcome_keys("live");
            pairs.push((key, value));
            let plan = plan_for(&pairs, &config());
            assert!(plan.welcome.is_none(), "{key}={value}");
            let text = plan.lines.join("\n");
            assert!(text.contains(&format!("{key}: invalid_setting")), "{text}");
            assert!(!text.contains("7700") && !text.contains("../x"), "{text}");
            let mut off = welcome_keys("off");
            off.push((key, value));
            let plan = plan_for(&off, &config());
            assert!(plan.lines.is_empty() && plan.welcome.is_none());
        }
    }

    const LINK_TEMPLATE: &str = r#"{"embed":{"title":"Rules {channel:rules}","description":"Pick {role:pings}"}}"#;

    #[tokio::test]
    async fn a_template_that_links_a_channel_renders_the_linked_channel_for_a_joining_member() {
        let log = Log::default();
        let plan = plan_for(&linked("live"), &config());
        let read = |key: &str| (key == "welcome").then(|| LINK_TEMPLATE.to_string());
        let feature = build_welcome::<u8, u16, _, _, _, _, _, _>(&plan, ports(&log), BOT, &read)
            .ok()
            .flatten()
            .expect("built with its id map");
        let dir = scratch("linked-template");
        let f = start_welcome(vec![feature], &dir, GuildFilter::Only(GUILD), &Starter::default()).await;
        assert_eq!(f.dispatch(&1, &join(GUILD, MEMBER, false), &2).await, 1);
        assert_eq!(
            log.lines(),
            vec![
                format!("grant:{MEMBER}:{ROLE_LOW}"),
                format!("send:{CHANNEL}:Rules <#770060>"),
            ]
        );
        // Without the links in the environment the same template is refused at start.
        let bare = plan_for(&welcome_keys("live"), &config());
        let refused = build_welcome::<u8, u16, _, _, _, _, _, _>(&bare, ports(&log), BOT, &read);
        let line = refused.err().expect("an unknown channel key is refused");
        assert!(line.contains("template refused"), "{line}");
        assert!(!line.contains("rules"), "no template text in {line}");
    }
}

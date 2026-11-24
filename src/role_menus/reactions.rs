//! Reaction-role bindings and event handler routed through the shared role service (RR2).
//!
//! Preserves the existing `reaction_roles` SQLite schema (`guild_id`, `channel_id`, `message_id`,
//! `emoji_key`, `role_id`, `created_by`, `created_at`, PK `(message_id, emoji_key, role_id)`)
//! and command/event parity while enforcing K03 eligibility, K04 grant execution, RG2 role facts,
//! home-guild scoping, bot-reactor filtering, safeword restrictions, and sanitized write-failure
//! reporting through injectable ports.

use crate::features::Feature;
use crate::jobs::runner::BoxFuture;
use crate::roles::eligibility::{self, Decision, DenyReason, GrantPath, SafewordState};
use crate::roles::grant::{
    self, FailCode, GrantReport, RoleOutcome, RoleWriter, Sleeper, WriteError,
};
use crate::roles::runtime::{self, DecisionInputs, FactsOutcome, RoleSource, RuntimeDecision};
use crate::settings::{parse_mode, Mode};
use crate::text_helpers::{normalize_reaction_type, parse_emoji_input};
use poise::serenity_prelude as s;
use rusqlite::{params, Connection};
use std::fmt;

/// Feature name reported to the feature registry.
pub const FEATURE_NAME: &str = "reaction_roles";

/// Schema DDL matching the existing `reaction_roles` table in `main.rs`.
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS reaction_roles (
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    emoji_key TEXT NOT NULL,
    role_id TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (message_id, emoji_key, role_id)
);
"#;

/// Initializes the `reaction_roles` table if it does not exist.
pub fn init_schema(conn: &Connection) -> Result<(), StorageCode> {
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|_| StorageCode::Sqlite)
}

/// Stable error codes for `reaction_roles` SQLite operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageCode {
    /// SQLite query or write failed.
    Sqlite,
    /// A stored ID text column could not be parsed as a positive `u64`.
    CorruptId,
}

impl StorageCode {
    /// Stable machine code string; never contains SQL text, paths, or user values.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite_error",
            Self::CorruptId => "corrupt_id",
        }
    }
}

impl fmt::Display for StorageCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for StorageCode {}

/// One stored reaction-role binding row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRow {
    pub guild_id: u64,
    pub channel_id: u64,
    pub message_id: u64,
    pub emoji_key: String,
    pub role_id: u64,
}

/// Governance environment shared across binding commands and reaction events.
#[derive(Debug, Clone)]
pub struct ReactionEnv {
    /// Configured home guild (`GUILD_ID`). Commands fail closed when `None` or mismatched.
    pub home_guild_id: Option<u64>,
    /// True when `PROTECTED_ROLE_IDS` had an unusable entry in `.env` (fails closed).
    pub protected_roles_invalid: bool,
    /// Role IDs that automated grant paths (`GrantPath::Reaction`) must never grant.
    pub protected_role_ids: Vec<u64>,
    /// Role IDs restricted while a member's safeword is `Active`.
    pub safeword_restricted_role_ids: Vec<u64>,
    /// Optional staff alert channel ID for sanitized failure reports.
    pub staff_alert_channel_id: Option<u64>,
    /// The bot's own user ID (used to ignore self-reactions and look up bot role hierarchy).
    pub bot_user_id: u64,
}

/// Stable denial codes for `/reactionrole` binding setup and management commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDeny {
    /// `GUILD_ID` is unset or unusable.
    HomeGuildUnset,
    /// The command was invoked in a guild other than `home_guild_id`.
    ForeignGuild,
    /// `PROTECTED_ROLE_IDS` is invalid; automated role setup fails closed.
    ProtectedConfigInvalid,
    /// A Discord ID (guild, channel, message, role, or actor) was zero.
    InvalidId,
    /// The emoji input was empty or whitespace-only.
    EmptyEmoji,
    /// Role facts could not be retrieved from the role source.
    FactsUnavailable,
    /// K03 eligibility denied the role (`PrivilegedPermission`, `ProtectedRole`, `ManagedRole`, etc.).
    Ineligible(DenyReason),
    /// SQLite storage failed.
    Storage(StorageCode),
}

impl CommandDeny {
    /// Stable machine code; never echoes user text or IDs.
    pub fn code(self) -> &'static str {
        match self {
            Self::HomeGuildUnset => "home_guild_unset",
            Self::ForeignGuild => "foreign_guild",
            Self::ProtectedConfigInvalid => "protected_roles_invalid",
            Self::InvalidId => "invalid_id",
            Self::EmptyEmoji => "empty_emoji",
            Self::FactsUnavailable => runtime::CODE_FACTS_UNAVAILABLE,
            Self::Ineligible(reason) => reason.code(),
            Self::Storage(code) => code.code(),
        }
    }
}

/// Request to add or replace a reaction-role binding (`/reactionrole add`).
#[derive(Debug, Clone)]
pub struct AddBindingRequest {
    pub guild_id: u64,
    pub channel_id: u64,
    pub message_id: u64,
    pub emoji_input: String,
    pub role_id: u64,
    pub created_by: u64,
}

/// Request to remove a reaction-role binding (`/reactionrole remove`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveBindingRequest {
    pub guild_id: u64,
    pub message_id: u64,
    pub role_id: u64,
}

/// Request to list reaction-role bindings on a message (`/reactionrole list`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListBindingsRequest {
    pub guild_id: u64,
    pub message_id: u64,
}

fn check_home_guild(env: &ReactionEnv, guild_id: u64) -> Result<u64, CommandDeny> {
    let Some(home) = env.home_guild_id else {
        return Err(CommandDeny::HomeGuildUnset);
    };
    if home == 0 || guild_id == 0 {
        return Err(CommandDeny::InvalidId);
    }
    if guild_id != home {
        return Err(CommandDeny::ForeignGuild);
    }
    Ok(home)
}

fn parse_positive_id(raw: &str) -> Result<u64, StorageCode> {
    let parsed = raw.parse::<u64>().map_err(|_| StorageCode::CorruptId)?;
    if parsed == 0 {
        return Err(StorageCode::CorruptId);
    }
    Ok(parsed)
}

/// Validates home guild, role eligibility via K03 (`GrantPath::Reaction`), and stores the binding.
pub async fn add_binding<S: RoleSource + ?Sized>(
    conn: &Connection,
    source: &S,
    env: &ReactionEnv,
    req: &AddBindingRequest,
) -> Result<String, CommandDeny> {
    let guild_id = check_home_guild(env, req.guild_id)?;
    if req.channel_id == 0
        || req.message_id == 0
        || req.role_id == 0
        || req.created_by == 0
        || env.bot_user_id == 0
    {
        return Err(CommandDeny::InvalidId);
    }
    if env.protected_roles_invalid {
        return Err(CommandDeny::ProtectedConfigInvalid);
    }
    let emoji_key = parse_emoji_input(&req.emoji_input);
    if emoji_key.trim().is_empty() {
        return Err(CommandDeny::EmptyEmoji);
    }

    let inputs = DecisionInputs {
        path: GrantPath::Reaction,
        bot_top_position: 0,
        guild_id,
        protected_role_ids: &env.protected_role_ids,
        safeword: SafewordState::Inactive,
        safeword_restricted_role_ids: &env.safeword_restricted_role_ids,
        staff: None,
    };
    match runtime::decide_role(source, guild_id, req.role_id, env.bot_user_id, &inputs).await {
        RuntimeDecision::Decided(Decision::Allow) => {}
        RuntimeDecision::Decided(Decision::Deny(reason)) => {
            return Err(CommandDeny::Ineligible(reason));
        }
        RuntimeDecision::Unavailable => return Err(CommandDeny::FactsUnavailable),
    }

    conn.execute(
        "INSERT OR REPLACE INTO reaction_roles\n         (guild_id, channel_id, message_id, emoji_key, role_id, created_by, created_at)\n         VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'))",
        params![
            guild_id.to_string(),
            req.channel_id.to_string(),
            req.message_id.to_string(),
            emoji_key,
            req.role_id.to_string(),
            req.created_by.to_string(),
        ],
    )
    .map_err(|_| CommandDeny::Storage(StorageCode::Sqlite))?;

    Ok(emoji_key)
}

/// Validates home guild and deletes matching `(message_id, role_id)` bindings.
pub fn remove_binding(
    conn: &Connection,
    env: &ReactionEnv,
    req: &RemoveBindingRequest,
) -> Result<usize, CommandDeny> {
    let _guild_id = check_home_guild(env, req.guild_id)?;
    if req.message_id == 0 || req.role_id == 0 {
        return Err(CommandDeny::InvalidId);
    }
    conn.execute(
        "DELETE FROM reaction_roles WHERE message_id = ?1 AND role_id = ?2",
        params![req.message_id.to_string(), req.role_id.to_string()],
    )
    .map_err(|_| CommandDeny::Storage(StorageCode::Sqlite))
}

/// Validates home guild and lists bindings for `message_id` ordered by `emoji_key ASC`.
pub fn list_bindings(
    conn: &Connection,
    env: &ReactionEnv,
    req: &ListBindingsRequest,
) -> Result<Vec<BindingRow>, CommandDeny> {
    let _guild_id = check_home_guild(env, req.guild_id)?;
    if req.message_id == 0 {
        return Err(CommandDeny::InvalidId);
    }
    let mut stmt = conn
        .prepare(
            "SELECT guild_id, channel_id, message_id, emoji_key, role_id\n             FROM reaction_roles\n             WHERE message_id = ?1\n             ORDER BY emoji_key ASC, role_id ASC",
        )
        .map_err(|_| CommandDeny::Storage(StorageCode::Sqlite))?;
    let rows = stmt
        .query_map(params![req.message_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|_| CommandDeny::Storage(StorageCode::Sqlite))?;
    let mut out = Vec::new();
    for item in rows {
        let (g, c, m, emoji_key, r) =
            item.map_err(|_| CommandDeny::Storage(StorageCode::Sqlite))?;
        out.push(BindingRow {
            guild_id: parse_positive_id(&g).map_err(CommandDeny::Storage)?,
            channel_id: parse_positive_id(&c).map_err(CommandDeny::Storage)?,
            message_id: parse_positive_id(&m).map_err(CommandDeny::Storage)?,
            emoji_key,
            role_id: parse_positive_id(&r).map_err(CommandDeny::Storage)?,
        });
    }
    Ok(out)
}

/// Queries bound role IDs for `(message_id, emoji_key)` in deterministic row order.
pub fn lookup_roles_for_message_emoji(
    conn: &Connection,
    message_id: u64,
    emoji_key: &str,
) -> Result<Vec<u64>, StorageCode> {
    let mut stmt = conn
        .prepare(
            "SELECT role_id\n             FROM reaction_roles\n             WHERE message_id = ?1 AND emoji_key = ?2\n             ORDER BY rowid ASC",
        )
        .map_err(|_| StorageCode::Sqlite)?;
    let rows = stmt
        .query_map(params![message_id.to_string(), emoji_key], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| StorageCode::Sqlite)?;
    let mut out = Vec::new();
    for item in rows {
        let raw = item.map_err(|_| StorageCode::Sqlite)?;
        out.push(parse_positive_id(&raw)?);
    }
    Ok(out)
}

/// Synchronous role remover port mirroring `grant::RoleWriter` so removals are offline-testable.
pub trait RoleRemover {
    fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError>;
}

/// Injectable reporter for sanitized role-write and policy failures.
pub trait FailureReporter {
    fn report(&mut self, line: &str);
}

/// Default in-memory collector for sanitized failure lines in tests and batch runs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemoryReporter {
    pub lines: Vec<String>,
}

impl FailureReporter for MemoryReporter {
    fn report(&mut self, line: &str) {
        self.lines.push(line.to_string());
    }
}

/// Incoming reaction add event facts (pure, Discord-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionAddEvent {
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub message_id: u64,
    pub user_id: Option<u64>,
    pub user_is_bot: Option<bool>,
    pub emoji_key: String,
    pub safeword: SafewordState,
}

/// Incoming reaction remove event facts (pure, Discord-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionRemoveEvent {
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub message_id: u64,
    pub user_id: Option<u64>,
    pub user_is_bot: Option<bool>,
    pub emoji_key: String,
    pub safeword: SafewordState,
}

/// Why a reaction event was ignored before touching roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NoGuild,
    ForeignGuild,
    NoUser,
    BotReactor,
    /// The reactor's bot flag could not be established: ignored, never assumed human.
    BotFlagUnknown,
    NoBindings,
}

impl SkipReason {
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoGuild => "no_guild",
            Self::ForeignGuild => "foreign_guild",
            Self::NoUser => "no_user",
            Self::BotReactor => "bot_reactor",
            Self::BotFlagUnknown => "bot_flag_unknown",
            Self::NoBindings => "no_bindings",
        }
    }
}

/// Outcome of handling a `ReactionAdd` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionAddOutcome {
    /// Event was ignored before any role grant attempt.
    Skipped(SkipReason),
    /// Event was aborted and reported with a stable error code (e.g. storage or lookup failure).
    Aborted(&'static str),
    /// Bound roles were evaluated and passed through the K04 grant executor.
    Evaluated {
        report: GrantReport,
        unavailable_roles: Vec<u64>,
    },
}

/// Per-role outcome for a `ReactionRemove` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalRoleOutcome {
    Removed,
    Denied(DenyReason),
    Unavailable,
    Failed(FailCode),
}

/// Outcome of handling a `ReactionRemove` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionRemoveOutcome {
    /// Event was ignored before any role removal attempt.
    Skipped(SkipReason),
    /// Event was aborted and reported with a stable error code.
    Aborted(&'static str),
    /// Bound roles were evaluated for removal and executed.
    Evaluated(Vec<(u64, RemovalRoleOutcome)>),
}

/// Ports bundled for `handle_reaction_add` (keeps argument count <= 7).
pub struct AddPorts<'a, S: RoleSource + ?Sized, W: RoleWriter, L: Sleeper, R: FailureReporter> {
    pub source: &'a S,
    pub writer: &'a mut W,
    pub sleeper: &'a mut L,
    pub reporter: &'a mut R,
}

/// Ports bundled for `handle_reaction_remove` (keeps argument count <= 7).
pub struct RemovePorts<'a, S: RoleSource + ?Sized, M: RoleRemover, L: Sleeper, R: FailureReporter> {
    pub source: &'a S,
    pub remover: &'a mut M,
    pub sleeper: &'a mut L,
    pub reporter: &'a mut R,
}

fn precheck_event(
    env: &ReactionEnv,
    guild_id: Option<u64>,
    user_id: Option<u64>,
    user_is_bot: Option<bool>,
) -> Result<(u64, u64), SkipReason> {
    let Some(gid) = guild_id.filter(|&g| g != 0) else {
        return Err(SkipReason::NoGuild);
    };
    match env.home_guild_id {
        Some(home) if home == gid => {}
        _ => return Err(SkipReason::ForeignGuild),
    }
    let Some(uid) = user_id.filter(|&u| u != 0) else {
        return Err(SkipReason::NoUser);
    };
    if uid == env.bot_user_id || user_is_bot == Some(true) {
        return Err(SkipReason::BotReactor);
    }
    // An unknown flag is not "human": without proof the reactor is not a bot, ignore it.
    if user_is_bot.is_none() {
        return Err(SkipReason::BotFlagUnknown);
    }
    Ok((gid, uid))
}

fn remove_with_retry<M: RoleRemover, L: Sleeper>(
    remover: &mut M,
    sleeper: &mut L,
    member_id: u64,
    role_id: u64,
) -> RemovalRoleOutcome {
    let mut retries: u8 = 0;
    loop {
        let error = match remover.remove_role(member_id, role_id) {
            Ok(()) => return RemovalRoleOutcome::Removed,
            Err(err) => err,
        };
        let code = grant::classify(&error);
        if code == FailCode::RateLimited && retries < grant::MAX_RATE_LIMIT_RETRIES {
            retries += 1;
            let wait = error
                .retry_after_ms
                .unwrap_or(grant::DEFAULT_BACKOFF_MS * u64::from(retries))
                .min(grant::MAX_SLEEP_MS);
            sleeper.sleep_ms(wait);
            continue;
        }
        return RemovalRoleOutcome::Failed(code);
    }
}

/// Handles a reaction-add event: filters bots/foreign guilds, resolves bound roles, evaluates
/// K03 eligibility (including active safeword restrictions), applies grants via K04, and reports
/// any denials or write failures through `ports.reporter` using stable codes only.
pub async fn handle_reaction_add<S, W, L, R>(
    conn: &Connection,
    env: &ReactionEnv,
    event: &ReactionAddEvent,
    ports: AddPorts<'_, S, W, L, R>,
) -> ReactionAddOutcome
where
    S: RoleSource + ?Sized,
    W: RoleWriter,
    L: Sleeper,
    R: FailureReporter,
{
    let (guild_id, member_id) =
        match precheck_event(env, event.guild_id, event.user_id, event.user_is_bot) {
            Ok(ids) => ids,
            Err(reason) => return ReactionAddOutcome::Skipped(reason),
        };

    let role_ids = match lookup_roles_for_message_emoji(conn, event.message_id, &event.emoji_key) {
        Ok(ids) => ids,
        Err(err) => {
            let code = err.code();
            ports.reporter.report(&format!(
                "reaction_add guild={guild_id} message={} member={member_id} error={code}",
                event.message_id
            ));
            return ReactionAddOutcome::Aborted(code);
        }
    };
    evaluate_add(
        env,
        event,
        Resolved {
            guild_id,
            member_id,
            role_ids,
        },
        ports,
    )
    .await
}

/// The ids a reaction event resolved to before any role is evaluated.
struct Resolved {
    guild_id: u64,
    member_id: u64,
    /// The roles bound to the message and emoji, in row order.
    role_ids: Vec<u64>,
}

/// The part of [`handle_reaction_add`] that follows the binding lookup. It holds no database
/// handle (a `rusqlite::Connection` is not `Sync`, so it cannot be held across an `.await` in a
/// future that must be `Send`), which is what lets the live feature call it.
async fn evaluate_add<S, W, L, R>(
    env: &ReactionEnv,
    event: &ReactionAddEvent,
    resolved: Resolved,
    ports: AddPorts<'_, S, W, L, R>,
) -> ReactionAddOutcome
where
    S: RoleSource + ?Sized,
    W: RoleWriter,
    L: Sleeper,
    R: FailureReporter,
{
    let Resolved {
        guild_id,
        member_id,
        role_ids,
    } = resolved;
    if role_ids.is_empty() {
        return ReactionAddOutcome::Skipped(SkipReason::NoBindings);
    }
    if env.protected_roles_invalid {
        let code = CommandDeny::ProtectedConfigInvalid.code();
        ports.reporter.report(&format!(
            "reaction_add guild={guild_id} message={} member={member_id} error={code}",
            event.message_id
        ));
        return ReactionAddOutcome::Aborted(code);
    }

    let member_roles = match ports.source.member_roles(guild_id, member_id).await {
        Ok(roles) => roles,
        Err(_) => {
            let code = runtime::CODE_FACTS_UNAVAILABLE;
            ports.reporter.report(&format!(
                "reaction_add guild={guild_id} message={} member={member_id} error={code}",
                event.message_id
            ));
            return ReactionAddOutcome::Aborted(code);
        }
    };

    let base_inputs = DecisionInputs {
        path: GrantPath::Reaction,
        bot_top_position: 0,
        guild_id,
        protected_role_ids: &env.protected_role_ids,
        safeword: event.safeword,
        safeword_restricted_role_ids: &env.safeword_restricted_role_ids,
        staff: None,
    };
    let mut decisions: Vec<(u64, RuntimeDecision)> = Vec::with_capacity(role_ids.len());
    let mut unavailable_roles: Vec<u64> = Vec::new();
    for &role_id in &role_ids {
        let dec = runtime::decide_role(
            ports.source,
            guild_id,
            role_id,
            env.bot_user_id,
            &base_inputs,
        )
        .await;
        if dec == RuntimeDecision::Unavailable {
            unavailable_roles.push(role_id);
        }
        decisions.push((role_id, dec));
    }

    let items = runtime::grant_items(&decisions, &member_roles);
    let report = grant::apply(
        ports.writer,
        ports.sleeper,
        &grant::GrantRequest {
            member_id,
            items: &items,
        },
    );

    let has_non_ok = report
        .results
        .iter()
        .any(|r| matches!(r.outcome, RoleOutcome::Denied(_) | RoleOutcome::Failed(_)));
    if has_non_ok || !unavailable_roles.is_empty() {
        ports.reporter.report(&report.summary_line());
    }
    for &role_id in &unavailable_roles {
        ports.reporter.report(&format!(
            "reaction_add guild={guild_id} member={member_id} role={role_id}:denied:{}",
            runtime::CODE_FACTS_UNAVAILABLE
        ));
    }

    ReactionAddOutcome::Evaluated {
        report,
        unavailable_roles,
    }
}

/// Handles a reaction-remove event: filters bots/foreign guilds, resolves bound roles, checks
/// `eligibility::decide_removal` (allowing removals even during active safeword, protected, or
/// privileged status so long as below the bot's top role), removes roles with 429 retry, and
/// reports failures with sanitized stable codes.
pub async fn handle_reaction_remove<S, M, L, R>(
    conn: &Connection,
    env: &ReactionEnv,
    event: &ReactionRemoveEvent,
    ports: RemovePorts<'_, S, M, L, R>,
) -> ReactionRemoveOutcome
where
    S: RoleSource + ?Sized,
    M: RoleRemover,
    L: Sleeper,
    R: FailureReporter,
{
    let (guild_id, member_id) =
        match precheck_event(env, event.guild_id, event.user_id, event.user_is_bot) {
            Ok(ids) => ids,
            Err(reason) => return ReactionRemoveOutcome::Skipped(reason),
        };

    let role_ids = match lookup_roles_for_message_emoji(conn, event.message_id, &event.emoji_key) {
        Ok(ids) => ids,
        Err(err) => {
            let code = err.code();
            ports.reporter.report(&format!(
                "reaction_remove guild={guild_id} message={} member={member_id} error={code}",
                event.message_id
            ));
            return ReactionRemoveOutcome::Aborted(code);
        }
    };
    evaluate_remove(
        env,
        event,
        Resolved {
            guild_id,
            member_id,
            role_ids,
        },
        ports,
    )
    .await
}

/// The part of [`handle_reaction_remove`] that follows the binding lookup (see [`evaluate_add`]).
async fn evaluate_remove<S, M, L, R>(
    env: &ReactionEnv,
    event: &ReactionRemoveEvent,
    resolved: Resolved,
    ports: RemovePorts<'_, S, M, L, R>,
) -> ReactionRemoveOutcome
where
    S: RoleSource + ?Sized,
    M: RoleRemover,
    L: Sleeper,
    R: FailureReporter,
{
    let Resolved {
        guild_id,
        member_id,
        role_ids,
    } = resolved;
    if role_ids.is_empty() {
        return ReactionRemoveOutcome::Skipped(SkipReason::NoBindings);
    }

    let mut outcomes: Vec<(u64, RemovalRoleOutcome)> = Vec::with_capacity(role_ids.len());
    for &role_id in &role_ids {
        let (facts, bot_top) =
            runtime::lookup_facts(ports.source, guild_id, role_id, env.bot_user_id).await;
        let role_facts = match facts {
            FactsOutcome::Found(f) => f,
            FactsOutcome::Missing => eligibility::RoleFacts {
                id: role_id,
                position: 0,
                managed: false,
                permissions: 0,
                exists: false,
            },
            FactsOutcome::Unavailable => {
                ports.reporter.report(&format!(
                    "reaction_remove guild={guild_id} member={member_id} role={role_id}:denied:{}",
                    runtime::CODE_FACTS_UNAVAILABLE
                ));
                outcomes.push((role_id, RemovalRoleOutcome::Unavailable));
                continue;
            }
        };
        let req = eligibility::GrantRequest {
            path: GrantPath::Reaction,
            role: role_facts,
            bot_top_position: bot_top,
            guild_id,
            protected_role_ids: &env.protected_role_ids,
            safeword: event.safeword,
            safeword_restricted_role_ids: &env.safeword_restricted_role_ids,
            staff: None,
        };
        match eligibility::decide_removal(&req) {
            Decision::Allow => {
                let out = remove_with_retry(ports.remover, ports.sleeper, member_id, role_id);
                if let RemovalRoleOutcome::Failed(code) = out {
                    ports.reporter.report(&format!(
                        "reaction_remove guild={guild_id} member={member_id} role={role_id}:failed:{}",
                        code.code()
                    ));
                }
                outcomes.push((role_id, out));
            }
            Decision::Deny(reason) => {
                ports.reporter.report(&format!(
                    "reaction_remove guild={guild_id} member={member_id} role={role_id}:denied:{}",
                    reason.code()
                ));
                outcomes.push((role_id, RemovalRoleOutcome::Denied(reason)));
            }
        }
    }

    ReactionRemoveOutcome::Evaluated(outcomes)
}

/// Extracts an emoji key from a serenity `ReactionType` using the existing normalization rule.
pub fn emoji_key_from_reaction(emoji: &s::ReactionType) -> String {
    normalize_reaction_type(emoji)
}

/// Normalizes an operator emoji string from `/reactionrole add` into the canonical `emoji_key`.
pub fn emoji_key_from_input(input: &str) -> String {
    parse_emoji_input(input)
}

/// Environment key for the reaction-role feature mode. Only `off` (default) and `live` exist.
pub const KEY_MODE: &str = "REACTION_ROLES_MODE";

/// Why the mode setting was refused. A stable code, never the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeRefusal {
    /// `shadow` was asked for. The new path would own the reaction events and do nothing, which
    /// would silently switch reaction roles off, so it is refused instead.
    ShadowUnsupported,
}

impl ModeRefusal {
    pub const fn code(self) -> &'static str {
        match self {
            Self::ShadowUnsupported => "shadow_unsupported",
        }
    }
}

/// Reads `REACTION_ROLES_MODE`: `Ok(true)` = live, `Ok(false)` = off (unset, blank or any
/// unrecognised word, as for the other features), `Err` = a mode that cannot run.
pub fn load_enabled(lookup: impl Fn(&str) -> Option<String>) -> Result<bool, ModeRefusal> {
    match parse_mode(KEY_MODE, lookup(KEY_MODE).as_deref()).unwrap_or(Mode::Off) {
        Mode::Off => Ok(false),
        Mode::Live => Ok(true),
        Mode::Shadow => Err(ModeRefusal::ShadowUnsupported),
    }
}

/// What the live reaction-role path needs from outside: the binding table, Discord and the
/// safeword state. The production implementation talks to serenity; a test supplies a fake.
#[s::async_trait]
pub trait ReactionServices: Send + Sync {
    /// The roles bound to `(message, emoji key)`, in row order.
    async fn bound_roles(&self, message_id: u64, emoji_key: &str) -> Result<Vec<u64>, StorageCode>;
    /// Role and member facts.
    fn role_source(&self) -> &dyn RoleSource;
    /// Whether the user is a bot, when the event did not say. `None` = could not be established.
    async fn user_is_bot(&self, guild_id: u64, user_id: u64) -> Option<bool>;
    /// The member's safeword state (`Inactive` when the safeword feature is off).
    fn safeword_state(&self, guild_id: u64, user_id: u64) -> SafewordState;
    /// A writer that adds roles in one guild.
    fn role_writer(&self, guild_id: u64) -> Box<dyn RoleWriter + Send>;
    /// A writer that removes roles in one guild.
    fn role_remover(&self, guild_id: u64) -> Box<dyn RoleRemover + Send>;
    /// The sleeper used between rate-limit retries.
    fn sleeper(&self) -> Box<dyn Sleeper + Send>;
    /// Receives one failure line (stable codes and ids from the runtime).
    fn report(&self, line: &str);
}

/// The live part of [`ReactionRolesFeature`].
#[derive(Clone)]
struct Live {
    env: ReactionEnv,
    services: std::sync::Arc<dyn ReactionServices>,
}

/// Feature registry hook for reaction roles (`src/features.rs`).
///
/// With [`ReactionRolesFeature::live`] it handles `ReactionAdd` and `ReactionRemove` through the
/// role service; with [`ReactionRolesFeature::new`] it only reports whether it is enabled.
#[derive(Clone)]
pub struct ReactionRolesFeature {
    pub enabled: bool,
    live: Option<Live>,
}

impl ReactionRolesFeature {
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            live: None,
        }
    }

    /// The enabled feature that handles reaction events.
    pub fn live(env: ReactionEnv, services: std::sync::Arc<dyn ReactionServices>) -> Self {
        Self {
            enabled: true,
            live: Some(Live { env, services }),
        }
    }
}

impl RoleWriter for Box<dyn RoleWriter + Send> {
    fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
        (**self).add_role(member_id, role_id)
    }
}

impl RoleRemover for Box<dyn RoleRemover + Send> {
    fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
        (**self).remove_role(member_id, role_id)
    }
}

impl Sleeper for Box<dyn Sleeper + Send> {
    fn sleep_ms(&mut self, ms: u64) {
        (**self).sleep_ms(ms)
    }
}

/// Hands the handlers' failure lines to [`ReactionServices::report`].
struct ServicesReporter<'a>(&'a dyn ReactionServices);

impl FailureReporter for ServicesReporter<'_> {
    fn report(&mut self, line: &str) {
        self.0.report(line);
    }
}

/// One reaction, reduced to plain values, with the roles bound to it.
struct Prepared {
    guild_id: u64,
    user_id: u64,
    channel_id: u64,
    message_id: u64,
    emoji_key: String,
    user_is_bot: Option<bool>,
    role_ids: Vec<u64>,
}

impl Live {
    /// Reduces a reaction to its bound roles, or `None` when it is to be ignored: another
    /// guild, no user, the bot itself or a bot, nothing bound, or a bot flag that cannot be
    /// established. The cheap checks come first, so a reaction that is not bound to a role
    /// costs one database read and no Discord call.
    async fn prepare(&self, reaction: &s::Reaction) -> Option<Prepared> {
        let guild_id = reaction.guild_id.map(|g| g.get());
        let user_id = reaction.user_id.map(|u| u.get());
        let from_event = reaction.member.as_ref().map(|member| member.user.bot);
        // Step 1: everything that needs no flag. An unknown flag stands in as "human" here only.
        let (guild, user) = precheck_event(
            &self.env,
            guild_id,
            user_id,
            Some(from_event.unwrap_or(false)),
        )
        .ok()?;
        let message_id = reaction.message_id.get();
        let emoji_key = emoji_key_from_reaction(&reaction.emoji);
        // Step 2: is anything bound?
        let role_ids = match self.services.bound_roles(message_id, &emoji_key).await {
            Ok(ids) => ids,
            Err(code) => {
                self.services
                    .report(&format!("reaction_roles error={}", code.code()));
                return None;
            }
        };
        if role_ids.is_empty() {
            return None;
        }
        // Step 3: the real flag. When the event carried none, ask; unknown means ignore.
        let user_is_bot = match from_event {
            Some(flag) => Some(flag),
            None => self.services.user_is_bot(guild, user).await,
        };
        precheck_event(&self.env, guild_id, user_id, user_is_bot).ok()?;
        Some(Prepared {
            guild_id: guild,
            user_id: user,
            channel_id: reaction.channel_id.get(),
            message_id,
            emoji_key,
            user_is_bot,
            role_ids,
        })
    }

    async fn on_add(&self, reaction: &s::Reaction) {
        let Some(p) = self.prepare(reaction).await else {
            return;
        };
        let event = ReactionAddEvent {
            guild_id: Some(p.guild_id),
            channel_id: p.channel_id,
            message_id: p.message_id,
            user_id: Some(p.user_id),
            user_is_bot: p.user_is_bot,
            emoji_key: p.emoji_key,
            safeword: self.services.safeword_state(p.guild_id, p.user_id),
        };
        let mut writer = self.services.role_writer(p.guild_id);
        let mut sleeper = self.services.sleeper();
        let mut reporter = ServicesReporter(&*self.services);
        let _ = evaluate_add(
            &self.env,
            &event,
            Resolved {
                guild_id: p.guild_id,
                member_id: p.user_id,
                role_ids: p.role_ids,
            },
            AddPorts {
                source: self.services.role_source(),
                writer: &mut writer,
                sleeper: &mut sleeper,
                reporter: &mut reporter,
            },
        )
        .await;
    }

    async fn on_remove(&self, reaction: &s::Reaction) {
        let Some(p) = self.prepare(reaction).await else {
            return;
        };
        let event = ReactionRemoveEvent {
            guild_id: Some(p.guild_id),
            channel_id: p.channel_id,
            message_id: p.message_id,
            user_id: Some(p.user_id),
            user_is_bot: p.user_is_bot,
            emoji_key: p.emoji_key,
            safeword: self.services.safeword_state(p.guild_id, p.user_id),
        };
        let mut remover = self.services.role_remover(p.guild_id);
        let mut sleeper = self.services.sleeper();
        let mut reporter = ServicesReporter(&*self.services);
        let _ = evaluate_remove(
            &self.env,
            &event,
            Resolved {
                guild_id: p.guild_id,
                member_id: p.user_id,
                role_ids: p.role_ids,
            },
            RemovePorts {
                source: self.services.role_source(),
                remover: &mut remover,
                sleeper: &mut sleeper,
                reporter: &mut reporter,
            },
        )
        .await;
    }
}

impl<X: Sync, D: Sync> Feature<X, D> for ReactionRolesFeature {
    fn name(&self) -> &'static str {
        FEATURE_NAME
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn needs_core_db(&self) -> bool {
        false
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a X,
        event: &'a s::FullEvent,
        _data: &'a D,
        _guild: Option<u64>,
        _core_db: Option<&'a tokio_rusqlite::Connection>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Some(live) = &self.live else {
                return;
            };
            match event {
                s::FullEvent::ReactionAdd { add_reaction } => live.on_add(add_reaction).await,
                s::FullEvent::ReactionRemove { removed_reaction } => {
                    live.on_remove(removed_reaction).await
                }
                _ => {}
            }
        })
    }
}

// ---------------------------------------------------------------------------
// The serenity services (never run against Discord; only the pure helpers are tested)
// ---------------------------------------------------------------------------

/// Reduces a failure line to its stable words: lower-case letters and underscores only, so every
/// id, number and name is dropped. An alert is built from this, never from the raw line.
pub fn alert_words(line: &str) -> String {
    let words: Vec<&str> = line
        .split(|c: char| c == ' ' || c == ',' || c == ':' || c == '=')
        .filter(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        .collect();
    if words.is_empty() {
        "unspecified".to_string()
    } else {
        words.join(" ")
    }
}

/// How long the same alert text is held back after it was sent, in seconds.
pub const ALERT_REPEAT_SECS: i64 = 300;
/// How many distinct alert texts the throttle remembers.
const ALERT_MEMORY: usize = 32;

/// Lets one alert text through at most once per [`ALERT_REPEAT_SECS`], so a role that keeps
/// failing does not flood the staff channel. Remembers at most [`ALERT_MEMORY`] texts; when
/// full, the oldest is forgotten.
#[derive(Debug, Default)]
pub struct AlertThrottle {
    sent: Vec<(String, i64)>,
}

impl AlertThrottle {
    /// True when `text` may be sent at `now` (and records it).
    pub fn allow(&mut self, text: &str, now: i64) -> bool {
        if let Some(entry) = self.sent.iter_mut().find(|(t, _)| t == text) {
            let held = now
                .checked_sub(entry.1)
                .is_some_and(|age| (0..ALERT_REPEAT_SECS).contains(&age));
            if held {
                return false;
            }
            entry.1 = now;
            return true;
        }
        if self.sent.len() >= ALERT_MEMORY {
            if let Some(oldest) = self
                .sent
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(i, _)| i)
            {
                self.sent.remove(oldest);
            }
        }
        self.sent.push((text.to_string(), now));
        true
    }
}

/// Removes one role from a member over serenity's `remove_member_role`.
struct SerenityRoleRemover {
    guild_id: u64,
    http: std::sync::Arc<s::Http>,
    handle: tokio::runtime::Handle,
}

impl RoleRemover for SerenityRoleRemover {
    fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
        let http = std::sync::Arc::clone(&self.http);
        let guild_id = self.guild_id;
        let outcome = runtime::drive_blocking(&self.handle, async move {
            http.remove_member_role(
                s::GuildId::new(guild_id),
                s::UserId::new(member_id),
                s::RoleId::new(role_id),
                None,
            )
            .await
        })?;
        outcome.map_err(|error| runtime::map_serenity_error(&error))
    }
}

/// Looks up a member's safeword state; `None` of the lookup means the safeword feature is off.
pub type SafewordLookup = std::sync::Arc<dyn Fn(u64, u64) -> SafewordState + Send + Sync>;

/// The production [`ReactionServices`]: the `reaction_roles` table through its own connection,
/// serenity for Discord, and an optional safeword lookup.
pub struct SerenityReactionServices {
    http: std::sync::Arc<s::Http>,
    cache: std::sync::Arc<s::Cache>,
    db: std::sync::Arc<tokio_rusqlite::Connection>,
    source: runtime::CachedRoleSource<runtime::SerenityCacheSource, runtime::SerenityHttpSource>,
    staff_alert_channel: Option<u64>,
    safeword: Option<SafewordLookup>,
    throttle: std::sync::Mutex<AlertThrottle>,
}

impl SerenityReactionServices {
    pub fn new(
        http: std::sync::Arc<s::Http>,
        cache: std::sync::Arc<s::Cache>,
        db: std::sync::Arc<tokio_rusqlite::Connection>,
        staff_alert_channel: Option<u64>,
        safeword: Option<SafewordLookup>,
    ) -> Self {
        let source = runtime::CachedRoleSource::new(
            runtime::SerenityCacheSource {
                cache: cache.clone(),
            },
            runtime::SerenityHttpSource { http: http.clone() },
        );
        Self {
            http,
            cache,
            db,
            source,
            staff_alert_channel,
            safeword,
            throttle: std::sync::Mutex::new(AlertThrottle::default()),
        }
    }

    /// The bot flag from the cached member, if the cache has the member.
    fn cached_flag(&self, guild_id: u64, user_id: u64) -> Option<bool> {
        let guild = self.cache.guild(s::GuildId::new(guild_id))?;
        guild
            .members
            .get(&s::UserId::new(user_id))
            .map(|member| member.user.bot)
    }
}

#[s::async_trait]
impl ReactionServices for SerenityReactionServices {
    async fn bound_roles(&self, message_id: u64, emoji_key: &str) -> Result<Vec<u64>, StorageCode> {
        let key = emoji_key.to_string();
        self.db
            .call(
                move |conn| -> Result<Result<Vec<u64>, StorageCode>, rusqlite::Error> {
                    Ok(lookup_roles_for_message_emoji(conn, message_id, &key))
                },
            )
            .await
            .map_err(|_| StorageCode::Sqlite)?
    }

    fn role_source(&self) -> &dyn RoleSource {
        &self.source
    }

    async fn user_is_bot(&self, guild_id: u64, user_id: u64) -> Option<bool> {
        if let Some(flag) = self.cached_flag(guild_id, user_id) {
            return Some(flag);
        }
        self.http
            .get_member(s::GuildId::new(guild_id), s::UserId::new(user_id))
            .await
            .ok()
            .map(|member| member.user.bot)
    }

    fn safeword_state(&self, guild_id: u64, user_id: u64) -> SafewordState {
        match &self.safeword {
            Some(lookup) => lookup(guild_id, user_id),
            None => SafewordState::Inactive,
        }
    }

    fn role_writer(&self, guild_id: u64) -> Box<dyn RoleWriter + Send> {
        Box::new(runtime::SerenityRoleWriter::new(
            guild_id,
            self.http.clone(),
            tokio::runtime::Handle::current(),
        ))
    }

    fn role_remover(&self, guild_id: u64) -> Box<dyn RoleRemover + Send> {
        Box::new(SerenityRoleRemover {
            guild_id,
            http: self.http.clone(),
            handle: tokio::runtime::Handle::current(),
        })
    }

    fn sleeper(&self) -> Box<dyn Sleeper + Send> {
        Box::new(runtime::TokioSleeper::new(tokio::runtime::Handle::current()))
    }

    fn report(&self, line: &str) {
        let text = format!("reaction roles alert: {}", alert_words(line));
        eprintln!("{text}");
        let Some(channel) = self.staff_alert_channel.filter(|id| *id != 0) else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let allowed = self
            .throttle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .allow(&text, now);
        if !allowed {
            return;
        }
        let http = self.http.clone();
        tokio::spawn(async move {
            // No mention can fire from an alert: the allowed-mentions object is empty.
            let message = s::builder::CreateMessage::new()
                .content(text)
                .allowed_mentions(s::builder::CreateAllowedMentions::new());
            let _ = s::ChannelId::new(channel)
                .send_message(&http, message)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::{
        ADMINISTRATOR, BAN_MEMBERS, KICK_MEMBERS, MANAGE_GUILD, MANAGE_ROLES, MODERATE_MEMBERS,
    };
    use crate::roles::runtime::RoleEntry;
    use std::collections::{HashMap, VecDeque};

    const HOME_GUILD: u64 = 710_001;
    const FOREIGN_GUILD: u64 = 710_999;
    const CHANNEL: u64 = 710_010;
    const MESSAGE: u64 = 710_020;
    const BOT_USER: u64 = 710_002;
    const OTHER_BOT: u64 = 710_003;
    const STAFF_ACTOR: u64 = 710_004;
    const MEMBER: u64 = 710_005;

    const BOT_TOP_ROLE: u64 = 710_100;
    const ROLE_OK_A: u64 = 710_201;
    const ROLE_OK_B: u64 = 710_202;
    const ROLE_PRIV: u64 = 710_203;
    const ROLE_PROT: u64 = 710_204;
    const ROLE_MANAGED: u64 = 710_205;
    const ROLE_ABOVE_BOT: u64 = 710_206;
    const ROLE_RESTRICTED: u64 = 710_207;
    const ROLE_DELETED: u64 = 710_299;

    struct FakeSource {
        roles: Result<Vec<RoleEntry>, runtime::SourceError>,
        members: HashMap<u64, Vec<u64>>,
        member_err: Option<runtime::SourceError>,
    }

    impl FakeSource {
        fn standard() -> Self {
            let roles = vec![
                RoleEntry {
                    id: BOT_TOP_ROLE,
                    position: 20,
                    managed: false,
                    permissions: 0,
                },
                RoleEntry {
                    id: ROLE_OK_A,
                    position: 5,
                    managed: false,
                    permissions: 0,
                },
                RoleEntry {
                    id: ROLE_OK_B,
                    position: 6,
                    managed: false,
                    permissions: 0,
                },
                RoleEntry {
                    id: ROLE_PRIV,
                    position: 5,
                    managed: false,
                    permissions: MANAGE_ROLES,
                },
                RoleEntry {
                    id: ROLE_PROT,
                    position: 5,
                    managed: false,
                    permissions: 0,
                },
                RoleEntry {
                    id: ROLE_MANAGED,
                    position: 5,
                    managed: true,
                    permissions: 0,
                },
                RoleEntry {
                    id: ROLE_ABOVE_BOT,
                    position: 20,
                    managed: false,
                    permissions: 0,
                },
                RoleEntry {
                    id: ROLE_RESTRICTED,
                    position: 5,
                    managed: false,
                    permissions: 0,
                },
                RoleEntry {
                    id: HOME_GUILD,
                    position: 0,
                    managed: false,
                    permissions: 0,
                },
            ];
            let mut members = HashMap::new();
            members.insert(BOT_USER, vec![BOT_TOP_ROLE]);
            members.insert(MEMBER, Vec::new());
            Self {
                roles: Ok(roles),
                members,
                member_err: None,
            }
        }

        fn with_member_roles(mut self, member_id: u64, held: Vec<u64>) -> Self {
            self.members.insert(member_id, held);
            self
        }
    }

    #[s::async_trait]
    impl RoleSource for FakeSource {
        async fn roles(&self, _guild_id: u64) -> Result<Vec<RoleEntry>, runtime::SourceError> {
            self.roles.clone()
        }

        async fn member_roles(
            &self,
            _guild_id: u64,
            member_id: u64,
        ) -> Result<Vec<u64>, runtime::SourceError> {
            if let Some(err) = self.member_err {
                return Err(err);
            }
            Ok(self.members.get(&member_id).cloned().unwrap_or_default())
        }
    }

    #[derive(Default)]
    struct FakeWriter {
        script: HashMap<u64, VecDeque<Result<(), WriteError>>>,
        add_calls: Vec<(u64, u64)>,
        remove_calls: Vec<(u64, u64)>,
    }

    impl FakeWriter {
        fn fail_role(mut self, role_id: u64, results: Vec<Result<(), WriteError>>) -> Self {
            self.script.insert(role_id, results.into());
            self
        }
    }

    impl RoleWriter for FakeWriter {
        fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.add_calls.push((member_id, role_id));
            self.script
                .get_mut(&role_id)
                .and_then(|q| q.pop_front())
                .unwrap_or(Ok(()))
        }
    }

    impl RoleRemover for FakeWriter {
        fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.remove_calls.push((member_id, role_id));
            self.script
                .get_mut(&role_id)
                .and_then(|q| q.pop_front())
                .unwrap_or(Ok(()))
        }
    }

    #[derive(Default)]
    struct FakeSleeper {
        slept: Vec<u64>,
    }

    impl Sleeper for FakeSleeper {
        fn sleep_ms(&mut self, ms: u64) {
            self.slept.push(ms);
        }
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn env() -> ReactionEnv {
        ReactionEnv {
            home_guild_id: Some(HOME_GUILD),
            protected_roles_invalid: false,
            protected_role_ids: vec![ROLE_PROT],
            safeword_restricted_role_ids: vec![ROLE_RESTRICTED],
            staff_alert_channel_id: Some(710_099),
            bot_user_id: BOT_USER,
        }
    }

    fn add_req(role_id: u64, emoji: &str) -> AddBindingRequest {
        AddBindingRequest {
            guild_id: HOME_GUILD,
            channel_id: CHANNEL,
            message_id: MESSAGE,
            emoji_input: emoji.to_string(),
            role_id,
            created_by: STAFF_ACTOR,
        }
    }

    #[tokio::test]
    async fn parity_binding_add_list_lookup_and_remove_round_trip() {
        let conn = db();
        let source = FakeSource::standard();
        let e = env();

        let key_a = add_binding(&conn, &source, &e, &add_req(ROLE_OK_A, " <:wave:710301> "))
            .await
            .unwrap();
        assert_eq!(key_a, "wave:710301");

        let key_b = add_binding(&conn, &source, &e, &add_req(ROLE_OK_B, "🦀"))
            .await
            .unwrap();
        assert_eq!(key_b, "🦀");

        let listed = list_bindings(
            &conn,
            &e,
            &ListBindingsRequest {
                guild_id: HOME_GUILD,
                message_id: MESSAGE,
            },
        )
        .unwrap();
        assert_eq!(
            listed,
            vec![
                BindingRow {
                    guild_id: HOME_GUILD,
                    channel_id: CHANNEL,
                    message_id: MESSAGE,
                    emoji_key: "wave:710301".to_string(),
                    role_id: ROLE_OK_A,
                },
                BindingRow {
                    guild_id: HOME_GUILD,
                    channel_id: CHANNEL,
                    message_id: MESSAGE,
                    emoji_key: "🦀".to_string(),
                    role_id: ROLE_OK_B,
                },
            ]
        );

        assert_eq!(
            lookup_roles_for_message_emoji(&conn, MESSAGE, "wave:710301").unwrap(),
            vec![ROLE_OK_A]
        );

        let removed = remove_binding(
            &conn,
            &e,
            &RemoveBindingRequest {
                guild_id: HOME_GUILD,
                message_id: MESSAGE,
                role_id: ROLE_OK_A,
            },
        )
        .unwrap();
        assert_eq!(removed, 1);
        assert!(
            lookup_roles_for_message_emoji(&conn, MESSAGE, "wave:710301")
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn home_guild_enforced_on_add_remove_and_list_with_no_mutation_on_denial() {
        let conn = db();
        let source = FakeSource::standard();
        let mut e = env();
        add_binding(&conn, &source, &e, &add_req(ROLE_OK_A, "🦀"))
            .await
            .unwrap();

        // Foreign guild rejected on add, remove, and list.
        let mut foreign_add = add_req(ROLE_OK_B, "🔥");
        foreign_add.guild_id = FOREIGN_GUILD;
        assert_eq!(
            add_binding(&conn, &source, &e, &foreign_add).await,
            Err(CommandDeny::ForeignGuild)
        );
        assert_eq!(
            remove_binding(
                &conn,
                &e,
                &RemoveBindingRequest {
                    guild_id: FOREIGN_GUILD,
                    message_id: MESSAGE,
                    role_id: ROLE_OK_A,
                }
            ),
            Err(CommandDeny::ForeignGuild)
        );
        assert_eq!(
            list_bindings(
                &conn,
                &e,
                &ListBindingsRequest {
                    guild_id: FOREIGN_GUILD,
                    message_id: MESSAGE,
                }
            ),
            Err(CommandDeny::ForeignGuild)
        );

        // Unset home guild fails closed on add, remove, and list.
        e.home_guild_id = None;
        assert_eq!(
            add_binding(&conn, &source, &e, &add_req(ROLE_OK_B, "🔥")).await,
            Err(CommandDeny::HomeGuildUnset)
        );
        assert_eq!(
            remove_binding(
                &conn,
                &e,
                &RemoveBindingRequest {
                    guild_id: HOME_GUILD,
                    message_id: MESSAGE,
                    role_id: ROLE_OK_A,
                }
            ),
            Err(CommandDeny::HomeGuildUnset)
        );
        assert_eq!(
            list_bindings(
                &conn,
                &e,
                &ListBindingsRequest {
                    guild_id: HOME_GUILD,
                    message_id: MESSAGE,
                }
            ),
            Err(CommandDeny::HomeGuildUnset)
        );

        // Existing binding was untouched by all denied calls.
        assert_eq!(
            lookup_roles_for_message_emoji(&conn, MESSAGE, "🦀").unwrap(),
            vec![ROLE_OK_A]
        );
        assert!(lookup_roles_for_message_emoji(&conn, MESSAGE, "🔥")
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn add_binding_rejects_privileged_protected_managed_everyone_above_bot_and_deleted_roles()
    {
        let conn = db();
        let e = env();

        for bit in [
            ADMINISTRATOR,
            MANAGE_GUILD,
            MANAGE_ROLES,
            BAN_MEMBERS,
            KICK_MEMBERS,
            MODERATE_MEMBERS,
        ] {
            let mut source = FakeSource::standard();
            if let Ok(ref mut roles) = source.roles {
                roles
                    .iter_mut()
                    .find(|r| r.id == ROLE_PRIV)
                    .unwrap()
                    .permissions = bit;
            }
            assert_eq!(
                add_binding(&conn, &source, &e, &add_req(ROLE_PRIV, "🦀")).await,
                Err(CommandDeny::Ineligible(DenyReason::PrivilegedPermission))
            );
        }

        let source = FakeSource::standard();
        for (role_id, expected) in [
            (ROLE_PROT, DenyReason::ProtectedRole),
            (ROLE_MANAGED, DenyReason::ManagedRole),
            (HOME_GUILD, DenyReason::EveryoneRole),
            (ROLE_ABOVE_BOT, DenyReason::AtOrAboveBot),
            (ROLE_DELETED, DenyReason::RoleDeleted),
        ] {
            assert_eq!(
                add_binding(&conn, &source, &e, &add_req(role_id, "🦀")).await,
                Err(CommandDeny::Ineligible(expected)),
                "role {role_id}"
            );
        }

        // Invalid PROTECTED_ROLE_IDS fails closed at binding setup.
        let mut bad_env = e.clone();
        bad_env.protected_roles_invalid = true;
        assert_eq!(
            add_binding(&conn, &source, &bad_env, &add_req(ROLE_OK_A, "🦀")).await,
            Err(CommandDeny::ProtectedConfigInvalid)
        );

        // Unavailable facts fail closed.
        let mut broken_source = FakeSource::standard();
        broken_source.roles = Err(runtime::SourceError::Unavailable);
        assert_eq!(
            add_binding(&conn, &broken_source, &e, &add_req(ROLE_OK_A, "🦀")).await,
            Err(CommandDeny::FactsUnavailable)
        );

        // Table remains completely empty after all rejections.
        assert!(list_bindings(
            &conn,
            &e,
            &ListBindingsRequest {
                guild_id: HOME_GUILD,
                message_id: MESSAGE,
            }
        )
        .unwrap()
        .is_empty());
    }

    #[tokio::test]
    async fn reaction_add_grants_eligible_roles_and_skips_already_held() {
        let conn = db();
        let source = FakeSource::standard().with_member_roles(MEMBER, vec![ROLE_OK_A]);
        let e = env();
        add_binding(&conn, &source, &e, &add_req(ROLE_OK_A, "🦀"))
            .await
            .unwrap();
        add_binding(&conn, &source, &e, &add_req(ROLE_OK_B, "🦀"))
            .await
            .unwrap();

        let mut writer = FakeWriter::default();
        let mut sleeper = FakeSleeper::default();
        let mut reporter = MemoryReporter::default();
        let out = handle_reaction_add(
            &conn,
            &e,
            &ReactionAddEvent {
                guild_id: Some(HOME_GUILD),
                channel_id: CHANNEL,
                message_id: MESSAGE,
                user_id: Some(MEMBER),
                user_is_bot: Some(false),
                emoji_key: "🦀".to_string(),
                safeword: SafewordState::Inactive,
            },
            AddPorts {
                source: &source,
                writer: &mut writer,
                sleeper: &mut sleeper,
                reporter: &mut reporter,
            },
        )
        .await;

        assert_eq!(writer.add_calls, vec![(MEMBER, ROLE_OK_B)]);
        assert!(
            reporter.lines.is_empty(),
            "unexpected failures: {:?}",
            reporter.lines
        );
        match out {
            ReactionAddOutcome::Evaluated {
                report,
                unavailable_roles,
            } => {
                assert!(unavailable_roles.is_empty());
                assert_eq!(report.status(), grant::GrantStatus::AllDone);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn reaction_add_denies_privileged_protected_managed_and_active_safeword_with_no_mutation()
    {
        let conn = db();
        let source = FakeSource::standard();
        let e = env();

        // Insert legacy/pre-existing bindings directly into SQLite to simulate a role that
        // became privileged, protected, managed, or safeword-restricted after setup.
        for role_id in [
            ROLE_PRIV,
            ROLE_PROT,
            ROLE_MANAGED,
            ROLE_ABOVE_BOT,
            ROLE_DELETED,
            ROLE_RESTRICTED,
        ] {
            conn.execute(
                "INSERT INTO reaction_roles (guild_id, channel_id, message_id, emoji_key, role_id, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    HOME_GUILD.to_string(),
                    CHANNEL.to_string(),
                    MESSAGE.to_string(),
                    "🦀",
                    role_id.to_string(),
                    STAFF_ACTOR.to_string()
                ],
            )
            .unwrap();
        }

        let mut writer = FakeWriter::default();
        let mut sleeper = FakeSleeper::default();
        let mut reporter = MemoryReporter::default();
        let out = handle_reaction_add(
            &conn,
            &e,
            &ReactionAddEvent {
                guild_id: Some(HOME_GUILD),
                channel_id: CHANNEL,
                message_id: MESSAGE,
                user_id: Some(MEMBER),
                user_is_bot: Some(false),
                emoji_key: "🦀".to_string(),
                safeword: SafewordState::Active,
            },
            AddPorts {
                source: &source,
                writer: &mut writer,
                sleeper: &mut sleeper,
                reporter: &mut reporter,
            },
        )
        .await;

        assert!(writer.add_calls.is_empty(), "no denied role may be written");
        assert!(
            !reporter.lines.is_empty(),
            "denials must be reported with stable codes"
        );
        match out {
            ReactionAddOutcome::Evaluated { report, .. } => {
                assert_eq!(report.status(), grant::GrantStatus::AllFailed);
                let outcomes: Vec<RoleOutcome> = report.results.iter().map(|r| r.outcome).collect();
                assert_eq!(
                    outcomes,
                    vec![
                        RoleOutcome::Denied(DenyReason::PrivilegedPermission),
                        RoleOutcome::Denied(DenyReason::ProtectedRole),
                        RoleOutcome::Denied(DenyReason::ManagedRole),
                        RoleOutcome::Denied(DenyReason::AtOrAboveBot),
                        RoleOutcome::Denied(DenyReason::RoleDeleted),
                        RoleOutcome::Denied(DenyReason::SafewordRestricted),
                    ]
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bot_reactors_ignored_and_write_failures_reported_with_sanitized_codes() {
        let conn = db();
        let source = FakeSource::standard();
        let e = env();
        add_binding(&conn, &source, &e, &add_req(ROLE_OK_A, "🦀"))
            .await
            .unwrap();

        let mut writer = FakeWriter::default();
        let mut sleeper = FakeSleeper::default();
        let mut reporter = MemoryReporter::default();

        // Self-bot and other-bot reactions are ignored on both add and remove.
        for (uid, is_bot) in [
            (BOT_USER, Some(true)),
            (BOT_USER, None),
            (OTHER_BOT, Some(true)),
        ] {
            let out_add = handle_reaction_add(
                &conn,
                &e,
                &ReactionAddEvent {
                    guild_id: Some(HOME_GUILD),
                    channel_id: CHANNEL,
                    message_id: MESSAGE,
                    user_id: Some(uid),
                    user_is_bot: is_bot,
                    emoji_key: "🦀".to_string(),
                    safeword: SafewordState::Inactive,
                },
                AddPorts {
                    source: &source,
                    writer: &mut writer,
                    sleeper: &mut sleeper,
                    reporter: &mut reporter,
                },
            )
            .await;
            assert_eq!(out_add, ReactionAddOutcome::Skipped(SkipReason::BotReactor));

            let out_rem = handle_reaction_remove(
                &conn,
                &e,
                &ReactionRemoveEvent {
                    guild_id: Some(HOME_GUILD),
                    channel_id: CHANNEL,
                    message_id: MESSAGE,
                    user_id: Some(uid),
                    user_is_bot: is_bot,
                    emoji_key: "🦀".to_string(),
                    safeword: SafewordState::Inactive,
                },
                RemovePorts {
                    source: &source,
                    remover: &mut writer,
                    sleeper: &mut sleeper,
                    reporter: &mut reporter,
                },
            )
            .await;
            assert_eq!(
                out_rem,
                ReactionRemoveOutcome::Skipped(SkipReason::BotReactor)
            );
        }
        assert!(writer.add_calls.is_empty());
        assert!(writer.remove_calls.is_empty());

        // Non-bot write failure is reported with sanitized stable code (forbidden).
        let mut failing_writer = FakeWriter::default().fail_role(
            ROLE_OK_A,
            vec![Err(WriteError {
                status: 403,
                api_code: Some(50013),
                retry_after_ms: None,
            })],
        );
        let out_fail = handle_reaction_add(
            &conn,
            &e,
            &ReactionAddEvent {
                guild_id: Some(HOME_GUILD),
                channel_id: CHANNEL,
                message_id: MESSAGE,
                user_id: Some(MEMBER),
                user_is_bot: Some(false),
                emoji_key: "🦀".to_string(),
                safeword: SafewordState::Inactive,
            },
            AddPorts {
                source: &source,
                writer: &mut failing_writer,
                sleeper: &mut sleeper,
                reporter: &mut reporter,
            },
        )
        .await;
        assert_eq!(failing_writer.add_calls, vec![(MEMBER, ROLE_OK_A)]);
        assert_eq!(reporter.lines.len(), 1);
        assert!(
            reporter.lines[0].contains("failed:forbidden"),
            "{}",
            reporter.lines[0]
        );
        assert!(
            !reporter.lines[0].contains("🦀"),
            "emoji must not leak into report"
        );
        match out_fail {
            ReactionAddOutcome::Evaluated { report, .. } => {
                assert_eq!(report.status(), grant::GrantStatus::AllFailed);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn reaction_remove_preserves_intended_removals_during_safeword_and_reports_write_failures(
    ) {
        let conn = db();
        let source =
            FakeSource::standard().with_member_roles(MEMBER, vec![ROLE_RESTRICTED, ROLE_ABOVE_BOT]);
        let e = env();

        for role_id in [ROLE_RESTRICTED, ROLE_ABOVE_BOT] {
            conn.execute(
                "INSERT INTO reaction_roles (guild_id, channel_id, message_id, emoji_key, role_id, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    HOME_GUILD.to_string(),
                    CHANNEL.to_string(),
                    MESSAGE.to_string(),
                    "🦀",
                    role_id.to_string(),
                    STAFF_ACTOR.to_string()
                ],
            )
            .unwrap();
        }

        let mut remover = FakeWriter::default();
        let mut sleeper = FakeSleeper::default();
        let mut reporter = MemoryReporter::default();
        let out = handle_reaction_remove(
            &conn,
            &e,
            &ReactionRemoveEvent {
                guild_id: Some(HOME_GUILD),
                channel_id: CHANNEL,
                message_id: MESSAGE,
                user_id: Some(MEMBER),
                user_is_bot: Some(false),
                emoji_key: "🦀".to_string(),
                safeword: SafewordState::Active,
            },
            RemovePorts {
                source: &source,
                remover: &mut remover,
                sleeper: &mut sleeper,
                reporter: &mut reporter,
            },
        )
        .await;

        // Restricted role is removed even while safeword is Active; role at/above bot is denied.
        assert_eq!(remover.remove_calls, vec![(MEMBER, ROLE_RESTRICTED)]);
        assert_eq!(
            out,
            ReactionRemoveOutcome::Evaluated(vec![
                (ROLE_RESTRICTED, RemovalRoleOutcome::Removed),
                (
                    ROLE_ABOVE_BOT,
                    RemovalRoleOutcome::Denied(DenyReason::AtOrAboveBot)
                ),
            ])
        );
        assert_eq!(reporter.lines.len(), 1);
        assert!(
            reporter.lines[0].contains("denied:at_or_above_bot"),
            "{}",
            reporter.lines[0]
        );

        // Write failure on removal is reported with stable code.
        let mut failing_remover = FakeWriter::default().fail_role(
            ROLE_RESTRICTED,
            vec![Err(WriteError {
                status: 403,
                api_code: None,
                retry_after_ms: None,
            })],
        );
        let mut fail_reporter = MemoryReporter::default();
        let out_fail = handle_reaction_remove(
            &conn,
            &e,
            &ReactionRemoveEvent {
                guild_id: Some(HOME_GUILD),
                channel_id: CHANNEL,
                message_id: MESSAGE,
                user_id: Some(MEMBER),
                user_is_bot: Some(false),
                emoji_key: "🦀".to_string(),
                safeword: SafewordState::Active,
            },
            RemovePorts {
                source: &source,
                remover: &mut failing_remover,
                sleeper: &mut sleeper,
                reporter: &mut fail_reporter,
            },
        )
        .await;
        assert_eq!(
            out_fail,
            ReactionRemoveOutcome::Evaluated(vec![
                (
                    ROLE_RESTRICTED,
                    RemovalRoleOutcome::Failed(FailCode::Forbidden)
                ),
                (
                    ROLE_ABOVE_BOT,
                    RemovalRoleOutcome::Denied(DenyReason::AtOrAboveBot)
                ),
            ])
        );
        assert!(fail_reporter
            .lines
            .iter()
            .any(|l| l.contains("failed:forbidden")));
    }

    #[tokio::test]
    async fn seeded_liveness_retry_loop_for_add_and_remove_always_terminates_bounded() {
        let conn = db();
        let source = FakeSource::standard();
        let e = env();
        add_binding(&conn, &source, &e, &add_req(ROLE_OK_A, "🦀"))
            .await
            .unwrap();
        add_binding(&conn, &source, &e, &add_req(ROLE_OK_B, "🦀"))
            .await
            .unwrap();

        let mut lcg: u64 = 0x5EED_CAFE_1234_5678;
        let mut next_u32 = || -> u32 {
            lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
            (lcg >> 32) as u32
        };

        for _step in 0..32 {
            let mut writer = FakeWriter::default();
            for role_id in [ROLE_OK_A, ROLE_OK_B] {
                let n_attempts = (next_u32() % 6) as usize;
                let mut seq = Vec::with_capacity(n_attempts);
                for _ in 0..n_attempts {
                    let kind = next_u32() % 4;
                    let res = match kind {
                        0 => Ok(()),
                        1 => Err(WriteError {
                            status: 429,
                            api_code: None,
                            retry_after_ms: Some(250),
                        }),
                        2 => Err(WriteError {
                            status: 429,
                            api_code: None,
                            retry_after_ms: None,
                        }),
                        _ => Err(WriteError {
                            status: 403,
                            api_code: None,
                            retry_after_ms: None,
                        }),
                    };
                    seq.push(res);
                }
                writer = writer.fail_role(role_id, seq);
            }
            let mut sleeper = FakeSleeper::default();
            let mut reporter = MemoryReporter::default();
            let out_add = handle_reaction_add(
                &conn,
                &e,
                &ReactionAddEvent {
                    guild_id: Some(HOME_GUILD),
                    channel_id: CHANNEL,
                    message_id: MESSAGE,
                    user_id: Some(MEMBER),
                    user_is_bot: Some(false),
                    emoji_key: "🦀".to_string(),
                    safeword: SafewordState::Inactive,
                },
                AddPorts {
                    source: &source,
                    writer: &mut writer,
                    sleeper: &mut sleeper,
                    reporter: &mut reporter,
                },
            )
            .await;
            match out_add {
                ReactionAddOutcome::Evaluated { report, .. } => {
                    assert_eq!(report.results.len(), 2);
                    for r in &report.results {
                        assert!(r.writer_calls >= 1 && r.writer_calls <= 4);
                    }
                }
                other => panic!("unexpected add outcome: {other:?}"),
            }
            assert!(writer.add_calls.len() <= 8);

            let mut rem_sleeper = FakeSleeper::default();
            let mut rem_reporter = MemoryReporter::default();
            let out_rem = handle_reaction_remove(
                &conn,
                &e,
                &ReactionRemoveEvent {
                    guild_id: Some(HOME_GUILD),
                    channel_id: CHANNEL,
                    message_id: MESSAGE,
                    user_id: Some(MEMBER),
                    user_is_bot: Some(false),
                    emoji_key: "🦀".to_string(),
                    safeword: SafewordState::Inactive,
                },
                RemovePorts {
                    source: &source,
                    remover: &mut writer,
                    sleeper: &mut rem_sleeper,
                    reporter: &mut rem_reporter,
                },
            )
            .await;
            match out_rem {
                ReactionRemoveOutcome::Evaluated(items) => {
                    assert_eq!(items.len(), 2);
                }
                other => panic!("unexpected remove outcome: {other:?}"),
            }
            assert!(writer.remove_calls.len() <= 8);
        }
    }

    fn rate_limited(retry_after_ms: Option<u64>) -> Result<(), WriteError> {
        Err(WriteError {
            status: 429,
            api_code: None,
            retry_after_ms,
        })
    }

    async fn run_removal_of_ok_a(
        writer: &mut FakeWriter,
        sleeper: &mut FakeSleeper,
        reporter: &mut MemoryReporter,
    ) -> ReactionRemoveOutcome {
        let conn = db();
        let source = FakeSource::standard().with_member_roles(MEMBER, vec![ROLE_OK_A]);
        let e = env();
        add_binding(&conn, &source, &e, &add_req(ROLE_OK_A, "🦀"))
            .await
            .unwrap();
        handle_reaction_remove(
            &conn,
            &e,
            &ReactionRemoveEvent {
                guild_id: Some(HOME_GUILD),
                channel_id: CHANNEL,
                message_id: MESSAGE,
                user_id: Some(MEMBER),
                user_is_bot: Some(false),
                emoji_key: "🦀".to_string(),
                safeword: SafewordState::Inactive,
            },
            RemovePorts {
                source: &source,
                remover: writer,
                sleeper,
                reporter,
            },
        )
        .await
    }

    #[tokio::test]
    async fn removal_rate_limited_then_ok_is_retried_and_removed_with_recorded_wait() {
        let mut writer =
            FakeWriter::default().fail_role(ROLE_OK_A, vec![rate_limited(Some(250)), Ok(())]);
        let mut sleeper = FakeSleeper::default();
        let mut reporter = MemoryReporter::default();

        let out = run_removal_of_ok_a(&mut writer, &mut sleeper, &mut reporter).await;

        assert_eq!(
            out,
            ReactionRemoveOutcome::Evaluated(vec![(ROLE_OK_A, RemovalRoleOutcome::Removed)])
        );
        assert_eq!(
            writer.remove_calls,
            vec![(MEMBER, ROLE_OK_A), (MEMBER, ROLE_OK_A)]
        );
        assert_eq!(sleeper.slept, vec![250]);
        assert!(reporter.lines.is_empty(), "{:?}", reporter.lines);
    }

    #[tokio::test]
    async fn removal_persistent_rate_limit_fails_after_exactly_max_retries_plus_one_calls() {
        let attempts = usize::from(grant::MAX_RATE_LIMIT_RETRIES) + 1;
        // One extra scripted Ok proves no retry happens beyond the bound: if the code made
        // another call it would consume it and report Removed instead of RateLimited.
        let mut script: Vec<Result<(), WriteError>> =
            (0..attempts).map(|_| rate_limited(None)).collect();
        script.push(Ok(()));
        let mut writer = FakeWriter::default().fail_role(ROLE_OK_A, script);
        let mut sleeper = FakeSleeper::default();
        let mut reporter = MemoryReporter::default();

        let out = run_removal_of_ok_a(&mut writer, &mut sleeper, &mut reporter).await;

        assert_eq!(
            out,
            ReactionRemoveOutcome::Evaluated(vec![(
                ROLE_OK_A,
                RemovalRoleOutcome::Failed(FailCode::RateLimited)
            )])
        );
        assert_eq!(writer.remove_calls.len(), attempts);
        assert_eq!(
            sleeper.slept,
            vec![
                grant::DEFAULT_BACKOFF_MS,
                2 * grant::DEFAULT_BACKOFF_MS,
                3 * grant::DEFAULT_BACKOFF_MS
            ]
        );
        assert_eq!(reporter.lines.len(), 1);
        assert!(
            reporter.lines[0].contains("failed:rate_limited"),
            "{}",
            reporter.lines[0]
        );
    }
}

#[cfg(test)]
mod wiring_tests {
    //! W2 wiring: the live feature path, the fail-closed bot flag and the mode setting.
    use super::*;
    use crate::roles::eligibility::{SafewordState, MANAGE_ROLES};
    use crate::roles::runtime::RoleEntry;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const HOME: u64 = 720_001;
    const OTHER: u64 = 720_002;
    const CHANNEL: u64 = 720_010;
    const MESSAGE: u64 = 720_020;
    const BOT: u64 = 720_003;
    const MEMBER: u64 = 720_005;
    const ROLE_A: u64 = 720_201;
    const ROLE_PRIV: u64 = 720_203;
    const BOT_TOP: u64 = 720_100;
    const CRAB: &str = "\u{1F980}";

    struct Source;

    #[s::async_trait]
    impl RoleSource for Source {
        async fn roles(&self, _g: u64) -> Result<Vec<RoleEntry>, runtime::SourceError> {
            let role = |id, position, permissions| RoleEntry {
                id,
                position,
                managed: false,
                permissions,
            };
            Ok(vec![
                role(BOT_TOP, 20, 0),
                role(ROLE_A, 5, 0),
                role(ROLE_PRIV, 5, MANAGE_ROLES),
                role(HOME, 0, 0),
            ])
        }
        async fn member_roles(
            &self,
            _g: u64,
            member: u64,
        ) -> Result<Vec<u64>, runtime::SourceError> {
            Ok(if member == BOT { vec![BOT_TOP] } else { vec![] })
        }
    }

    type Log = Arc<Mutex<Vec<String>>>;

    struct LogWriter(Log);

    impl RoleWriter for LogWriter {
        fn add_role(&mut self, member: u64, role: u64) -> Result<(), WriteError> {
            self.0.lock().unwrap().push(format!("add:{member}:{role}"));
            Ok(())
        }
    }

    impl RoleRemover for LogWriter {
        fn remove_role(&mut self, member: u64, role: u64) -> Result<(), WriteError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("remove:{member}:{role}"));
            Ok(())
        }
    }

    struct NoSleep;

    impl Sleeper for NoSleep {
        fn sleep_ms(&mut self, _ms: u64) {}
    }

    struct Fake {
        bound: Vec<(u64, String, u64)>,
        bound_calls: AtomicUsize,
        flag: Option<bool>,
        flag_calls: AtomicUsize,
        safeword: SafewordState,
        log: Log,
    }

    impl Fake {
        fn new(flag: Option<bool>) -> Fake {
            Fake {
                bound: vec![(MESSAGE, CRAB.to_string(), ROLE_A)],
                bound_calls: AtomicUsize::new(0),
                flag,
                flag_calls: AtomicUsize::new(0),
                safeword: SafewordState::Inactive,
                log: Arc::default(),
            }
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    #[s::async_trait]
    impl ReactionServices for Fake {
        async fn bound_roles(&self, message: u64, key: &str) -> Result<Vec<u64>, StorageCode> {
            self.bound_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .bound
                .iter()
                .filter(|(m, k, _)| *m == message && k == key)
                .map(|(_, _, r)| *r)
                .collect())
        }
        fn role_source(&self) -> &dyn RoleSource {
            &Source
        }
        async fn user_is_bot(&self, _g: u64, _u: u64) -> Option<bool> {
            self.flag_calls.fetch_add(1, Ordering::SeqCst);
            self.flag
        }
        fn safeword_state(&self, _g: u64, _u: u64) -> SafewordState {
            self.safeword
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
        fn report(&self, line: &str) {
            self.log.lock().unwrap().push(format!("report:{line}"));
        }
    }

    fn env(home: Option<u64>) -> ReactionEnv {
        ReactionEnv {
            home_guild_id: home,
            protected_roles_invalid: false,
            protected_role_ids: vec![],
            safeword_restricted_role_ids: vec![],
            staff_alert_channel_id: None,
            bot_user_id: BOT,
        }
    }

    /// A reaction event in `guild` by `user`; `member_bot` puts a member object on the event.
    fn event(guild: Option<u64>, user: u64, member_bot: Option<bool>, added: bool) -> s::FullEvent {
        let mut payload = serde_json::json!({
            "channel_id": CHANNEL.to_string(), "message_id": MESSAGE.to_string(),
            "user_id": user.to_string(), "emoji": {"name": CRAB},
            "burst": false, "burst_colours": [], "type": 0,
        });
        if let Some(g) = guild {
            payload["guild_id"] = serde_json::json!(g.to_string());
        }
        if let Some(bot) = member_bot {
            payload["member"] = serde_json::json!({
                "user": {"id": user.to_string(), "username": "synthetic",
                         "discriminator": "0", "avatar": null, "bot": bot},
                "roles": [], "joined_at": "2020-01-01T00:00:00.000000+00:00",
                "deaf": false, "mute": false, "flags": 0,
            });
        }
        let reaction: s::Reaction = serde_json::from_value(payload).unwrap();
        if added {
            s::FullEvent::ReactionAdd {
                add_reaction: reaction,
            }
        } else {
            s::FullEvent::ReactionRemove {
                removed_reaction: reaction,
            }
        }
    }

    async fn deliver(feature: &ReactionRolesFeature, ev: &s::FullEvent, guild: Option<u64>) {
        <ReactionRolesFeature as Feature<(), ()>>::handle(feature, &(), ev, &(), guild, None).await;
    }

    fn live(fake: &Arc<Fake>, home: Option<u64>) -> ReactionRolesFeature {
        ReactionRolesFeature::live(env(home), fake.clone())
    }

    #[tokio::test]
    async fn an_ordinary_binding_grants_through_the_live_feature_like_the_direct_path() {
        // Direct path: the function the legacy-free wiring calls with a database handle.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO reaction_roles VALUES ('1','1',?1,?3,?2,'1',0)",
            params![MESSAGE.to_string(), ROLE_A.to_string(), CRAB],
        )
        .unwrap();
        let direct_log: Log = Arc::default();
        let mut writer = LogWriter(direct_log.clone());
        let mut sleeper = NoSleep;
        let mut reporter = MemoryReporter::default();
        let direct = handle_reaction_add(
            &conn,
            &env(Some(HOME)),
            &ReactionAddEvent {
                guild_id: Some(HOME),
                channel_id: CHANNEL,
                message_id: MESSAGE,
                user_id: Some(MEMBER),
                user_is_bot: Some(false),
                emoji_key: CRAB.to_string(),
                safeword: SafewordState::Inactive,
            },
            AddPorts {
                source: &Source,
                writer: &mut writer,
                sleeper: &mut sleeper,
                reporter: &mut reporter,
            },
        )
        .await;
        assert!(matches!(direct, ReactionAddOutcome::Evaluated { .. }));

        let fake = Arc::new(Fake::new(None));
        let feature = live(&fake, Some(HOME));
        let ev = event(Some(HOME), MEMBER, Some(false), true);
        deliver(&feature, &ev, Some(HOME)).await;
        assert_eq!(fake.log(), *direct_log.lock().unwrap());
        assert_eq!(fake.log(), vec![format!("add:{MEMBER}:{ROLE_A}")]);
    }

    #[tokio::test]
    async fn a_removal_takes_the_bound_role_back_and_needs_no_member_object() {
        let fake = Arc::new(Fake::new(Some(false)));
        let feature = live(&fake, Some(HOME));
        deliver(
            &feature,
            &event(Some(HOME), MEMBER, None, false),
            Some(HOME),
        )
        .await;
        assert_eq!(fake.log(), vec![format!("remove:{MEMBER}:{ROLE_A}")]);
        assert_eq!(fake.flag_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_role_policy_still_applies_through_the_live_feature() {
        let mut fake = Fake::new(None);
        fake.bound.push((MESSAGE, CRAB.to_string(), ROLE_PRIV));
        let fake = Arc::new(fake);
        let feature = live(&fake, Some(HOME));
        let ev = event(Some(HOME), MEMBER, Some(false), true);
        deliver(&feature, &ev, Some(HOME)).await;
        let log = fake.log();
        assert!(log.contains(&format!("add:{MEMBER}:{ROLE_A}")), "{log:?}");
        assert!(
            !log.contains(&format!("add:{MEMBER}:{ROLE_PRIV}")),
            "{log:?}"
        );
    }

    #[tokio::test]
    async fn bots_and_the_bot_itself_are_ignored_before_any_lookup() {
        for (user, member_bot) in [(MEMBER, Some(true)), (BOT, Some(false)), (BOT, None)] {
            let fake = Arc::new(Fake::new(Some(false)));
            let feature = live(&fake, Some(HOME));
            let ev = event(Some(HOME), user, member_bot, true);
            deliver(&feature, &ev, Some(HOME)).await;
            assert!(fake.log().is_empty());
            assert_eq!(fake.bound_calls.load(Ordering::SeqCst), 0);
            assert_eq!(fake.flag_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn an_unknown_bot_flag_fails_closed_through_the_live_feature() {
        // No member object and the service cannot tell: nothing is granted or removed.
        for added in [true, false] {
            let fake = Arc::new(Fake::new(None));
            let feature = live(&fake, Some(HOME));
            let ev = event(Some(HOME), MEMBER, None, added);
            deliver(&feature, &ev, Some(HOME)).await;
            assert!(fake.log().is_empty(), "added={added}: {:?}", fake.log());
        }
    }

    #[tokio::test]
    async fn the_flag_is_looked_up_only_for_a_bound_reaction() {
        let fake = Arc::new(Fake::new(Some(false)));
        let feature = live(&fake, Some(HOME));
        let mut unbound = event(Some(HOME), MEMBER, None, true);
        if let s::FullEvent::ReactionAdd { add_reaction } = &mut unbound {
            add_reaction.message_id = s::MessageId::new(MESSAGE + 1);
        }
        deliver(&feature, &unbound, Some(HOME)).await;
        assert_eq!(fake.flag_calls.load(Ordering::SeqCst), 0);
        assert!(fake.log().is_empty());
    }

    #[tokio::test]
    async fn another_guild_reads_and_writes_nothing() {
        let fake = Arc::new(Fake::new(Some(false)));
        let feature = live(&fake, Some(HOME));
        let other = event(Some(OTHER), MEMBER, Some(false), true);
        deliver(&feature, &other, Some(OTHER)).await;
        deliver(&feature, &event(None, MEMBER, Some(false), true), None).await;
        assert!(fake.log().is_empty());
        assert_eq!(fake.bound_calls.load(Ordering::SeqCst), 0);
        // And an unset home guild serves nobody.
        let feature = live(&fake, None);
        let home = event(Some(HOME), MEMBER, Some(false), true);
        deliver(&feature, &home, Some(HOME)).await;
        assert!(fake.log().is_empty());
    }

    #[tokio::test]
    async fn an_active_safeword_denies_a_restricted_role_on_add_but_not_on_remove() {
        let mut fake = Fake::new(Some(false));
        fake.safeword = SafewordState::Active;
        let fake = Arc::new(fake);
        let mut e = env(Some(HOME));
        e.safeword_restricted_role_ids = vec![ROLE_A];
        let feature = ReactionRolesFeature::live(e, fake.clone());
        let add = event(Some(HOME), MEMBER, Some(false), true);
        deliver(&feature, &add, Some(HOME)).await;
        let log = fake.log();
        assert!(!log.iter().any(|l| l.starts_with("add:")), "{log:?}");
        let remove = event(Some(HOME), MEMBER, Some(false), false);
        deliver(&feature, &remove, Some(HOME)).await;
        assert!(fake.log().contains(&format!("remove:{MEMBER}:{ROLE_A}")));
    }

    #[tokio::test]
    async fn the_plain_hook_without_services_does_nothing() {
        for enabled in [false, true] {
            let feature = ReactionRolesFeature::new(enabled);
            let ev = event(Some(HOME), MEMBER, Some(false), true);
            deliver(&feature, &ev, Some(HOME)).await;
            assert_eq!(
                <ReactionRolesFeature as Feature<(), ()>>::enabled(&feature),
                enabled
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_bot_flag_is_ignored_with_a_stable_code_by_the_handlers() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO reaction_roles VALUES ('1','1',?1,?3,?2,'1',0)",
            params![MESSAGE.to_string(), ROLE_A.to_string(), CRAB],
        )
        .unwrap();
        let log: Log = Arc::default();
        let (mut writer, mut sleeper, mut reporter) =
            (LogWriter(log.clone()), NoSleep, MemoryReporter::default());
        let add = handle_reaction_add(
            &conn,
            &env(Some(HOME)),
            &ReactionAddEvent {
                guild_id: Some(HOME),
                channel_id: CHANNEL,
                message_id: MESSAGE,
                user_id: Some(MEMBER),
                user_is_bot: None,
                emoji_key: CRAB.to_string(),
                safeword: SafewordState::Inactive,
            },
            AddPorts {
                source: &Source,
                writer: &mut writer,
                sleeper: &mut sleeper,
                reporter: &mut reporter,
            },
        )
        .await;
        assert_eq!(add, ReactionAddOutcome::Skipped(SkipReason::BotFlagUnknown));
        let remove = handle_reaction_remove(
            &conn,
            &env(Some(HOME)),
            &ReactionRemoveEvent {
                guild_id: Some(HOME),
                channel_id: CHANNEL,
                message_id: MESSAGE,
                user_id: Some(MEMBER),
                user_is_bot: None,
                emoji_key: CRAB.to_string(),
                safeword: SafewordState::Inactive,
            },
            RemovePorts {
                source: &Source,
                remover: &mut writer,
                sleeper: &mut sleeper,
                reporter: &mut reporter,
            },
        )
        .await;
        assert_eq!(
            remove,
            ReactionRemoveOutcome::Skipped(SkipReason::BotFlagUnknown)
        );
        assert!(log.lock().unwrap().is_empty());
        assert_eq!(SkipReason::BotFlagUnknown.code(), "bot_flag_unknown");
    }

    #[test]
    fn the_mode_setting_is_live_or_off_and_shadow_is_refused() {
        let with = |value: Option<&'static str>| {
            load_enabled(move |key| {
                assert_eq!(key, KEY_MODE);
                value.map(str::to_string)
            })
        };
        assert_eq!(with(None), Ok(false));
        assert_eq!(with(Some("")), Ok(false));
        assert_eq!(with(Some("off")), Ok(false));
        assert_eq!(with(Some("wat")), Ok(false));
        assert_eq!(with(Some(" LIVE ")), Ok(true));
        assert_eq!(with(Some("shadow")), Err(ModeRefusal::ShadowUnsupported));
        assert_eq!(ModeRefusal::ShadowUnsupported.code(), "shadow_unsupported");
    }
}

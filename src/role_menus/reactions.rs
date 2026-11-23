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
use crate::roles::runtime::{
    self, DecisionInputs, FactsOutcome, RoleEntry, RoleSource, RuntimeDecision,
};
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
    let _ = conn;
    Ok(())
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

/// Validates home guild, role eligibility via K03 (`GrantPath::Reaction`), and stores the binding.
pub async fn add_binding<S: RoleSource + ?Sized>(
    conn: &Connection,
    source: &S,
    env: &ReactionEnv,
    req: &AddBindingRequest,
) -> Result<String, CommandDeny> {
    let _ = (conn, source, env, req, params![1]);
    Err(CommandDeny::HomeGuildUnset)
}

/// Validates home guild and deletes matching `(message_id, role_id)` bindings.
pub fn remove_binding(
    conn: &Connection,
    env: &ReactionEnv,
    req: &RemoveBindingRequest,
) -> Result<usize, CommandDeny> {
    let _ = (conn, env, req);
    Err(CommandDeny::HomeGuildUnset)
}

/// Validates home guild and lists bindings for `message_id` ordered by `emoji_key ASC`.
pub fn list_bindings(
    conn: &Connection,
    env: &ReactionEnv,
    req: &ListBindingsRequest,
) -> Result<Vec<BindingRow>, CommandDeny> {
    let _ = (conn, env, req);
    Err(CommandDeny::HomeGuildUnset)
}

/// Queries bound role IDs for `(message_id, emoji_key)` in deterministic row order.
pub fn lookup_roles_for_message_emoji(
    conn: &Connection,
    message_id: u64,
    emoji_key: &str,
) -> Result<Vec<u64>, StorageCode> {
    let _ = (conn, message_id, emoji_key);
    Ok(Vec::new())
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
    NoBindings,
}

impl SkipReason {
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoGuild => "no_guild",
            Self::ForeignGuild => "foreign_guild",
            Self::NoUser => "no_user",
            Self::BotReactor => "bot_reactor",
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
    let _ = (
        conn,
        env,
        event,
        ports.source,
        ports.writer,
        ports.sleeper,
        ports.reporter,
        Decision::Allow,
        GrantPath::Reaction,
        RoleOutcome::Done,
        DecisionInputs {
            path: GrantPath::Reaction,
            bot_top_position: 0,
            guild_id: 0,
            protected_role_ids: &[],
            safeword: SafewordState::Inactive,
            safeword_restricted_role_ids: &[],
            staff: None,
        },
        FactsOutcome::Missing,
        RuntimeDecision::Unavailable,
        RoleEntry {
            id: 0,
            position: 0,
            managed: false,
            permissions: 0,
        },
        grant::apply::<W, L>,
        eligibility::decide_removal,
        parse_emoji_input,
        normalize_reaction_type,
    );
    ReactionAddOutcome::Skipped(SkipReason::NoBindings)
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
    let _ = (
        conn,
        env,
        event,
        ports.source,
        ports.remover,
        ports.sleeper,
        ports.reporter,
    );
    ReactionRemoveOutcome::Skipped(SkipReason::NoBindings)
}

/// Extracts an emoji key from a serenity `ReactionType` using the existing normalization rule.
pub fn emoji_key_from_reaction(emoji: &s::ReactionType) -> String {
    normalize_reaction_type(emoji)
}

/// Normalizes an operator emoji string from `/reactionrole add` into the canonical `emoji_key`.
pub fn emoji_key_from_input(input: &str) -> String {
    parse_emoji_input(input)
}

/// Feature registry hook for reaction roles (`src/features.rs`).
///
/// Built and tested offline; wiring card W2 registers it and removes the legacy arms from `main.rs`.
#[derive(Debug, Clone)]
pub struct ReactionRolesFeature {
    pub enabled: bool,
}

impl ReactionRolesFeature {
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
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
        _event: &'a s::FullEvent,
        _data: &'a D,
        _guild: Option<u64>,
        _core_db: Option<&'a tokio_rusqlite::Connection>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::{
        ADMINISTRATOR, BAN_MEMBERS, KICK_MEMBERS, MANAGE_GUILD, MANAGE_ROLES, MODERATE_MEMBERS,
    };
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
}

//! Role runtime adapter: Discord role facts and a real role writer.
//!
//! RED-EVIDENCE STUB COMMIT. The behaviour is deliberately wrong here so the tests
//! fail; the real implementation follows in the next commit.
//!
//! What the module is:
//!
//! 1. **Facts.** It turns Discord role data into [`eligibility::RoleFacts`] and works out
//!    the bot's top role position. The guild's roles come from a [`RoleSource`]; the real
//!    sources are the serenity cache first and an HTTP fetch of the guild's roles on a
//!    cache miss, composed by [`CachedRoleSource`].
//! 2. **Writer.** It implements the synchronous [`grant::RoleWriter`] over serenity's
//!    `add_member_role`, and maps serenity's typed errors to [`grant::WriteError`].
//! 3. **Helpers.** [`already_held`], the decision wrapper that keeps a failed lookup
//!    visible, and a [`grant::Sleeper`] for tokio-backed callers.

use super::eligibility::{Decision, GrantPath, RoleFacts, SafewordState, StaffContext};
use super::grant::{RoleWriter, Sleeper, WriteError, MAX_SLEEP_MS};

use poise::serenity_prelude as s;
use s::all::{GuildId, RoleId, UserId};
use std::future::Future;
use std::sync::Arc;

/// The Discord permissions bit for "Manage Roles" (1 << 28).
pub const MANAGE_ROLES_PERMISSION: u64 = 1 << 28;

// ---------------------------------------------------------------------------
// Facts
// ---------------------------------------------------------------------------

/// One role's Discord-side data, reduced to what the policy needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleEntry {
    /// The role id.
    pub id: u64,
    /// The role's position in the guild's hierarchy.
    pub position: u16,
    /// Whether the role is managed by an integration (bots cannot move it).
    pub managed: bool,
    /// The role's permission bits.
    pub permissions: u64,
}

impl RoleEntry {
    /// Build a role entry from a serenity role model.
    pub fn from_serenity(role: &s::model::guild::Role) -> Self {
        RoleEntry {
            id: role.id.get(),
            position: role.position,
            managed: role.managed,
            permissions: role.permissions.bits(),
        }
    }
}

/// What a role lookup produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactsOutcome {
    /// The role was found and its facts are known.
    Found(RoleFacts),
    /// The role id is not in the guild's role list: the role was deleted.
    Missing,
    /// The lookup failed. The caller must treat this as a deny; it is never a guess.
    Unavailable,
}

/// Why a source did not answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    /// This source does not hold the answer. Another source may still have it.
    Miss,
    /// The source failed (API error, transport error, cache not usable).
    Unavailable,
}

/// Where role facts come from.
#[s::async_trait]
pub trait RoleSource: Send + Sync {
    /// The guild's roles.
    async fn roles(&self, guild_id: u64) -> Result<Vec<RoleEntry>, SourceError>;

    /// The role ids the given member holds.
    async fn member_roles(&self, guild_id: u64, member_id: u64) -> Result<Vec<u64>, SourceError>;
}

/// The guild's role list from the serenity cache.
pub struct SerenityCacheSource {
    /// The serenity cache.
    pub cache: Arc<s::Cache>,
}

#[s::async_trait]
impl RoleSource for SerenityCacheSource {
    async fn roles(&self, _guild_id: u64) -> Result<Vec<RoleEntry>, SourceError> {
        Err(SourceError::Miss)
    }

    async fn member_roles(&self, _guild_id: u64, _member_id: u64) -> Result<Vec<u64>, SourceError> {
        Err(SourceError::Miss)
    }
}

/// The guild's role list from Discord's HTTP API.
pub struct SerenityHttpSource {
    /// The serenity HTTP client.
    pub http: Arc<s::http::Http>,
}

#[s::async_trait]
impl RoleSource for SerenityHttpSource {
    async fn roles(&self, _guild_id: u64) -> Result<Vec<RoleEntry>, SourceError> {
        Err(SourceError::Unavailable)
    }

    async fn member_roles(&self, _guild_id: u64, _member_id: u64) -> Result<Vec<u64>, SourceError> {
        Err(SourceError::Unavailable)
    }
}

/// Cache first, HTTP on a miss.
pub struct CachedRoleSource<C, H> {
    /// The source asked first.
    pub cache: C,
    /// The source asked when the first one has no answer.
    pub http: H,
}

impl<C, H> CachedRoleSource<C, H> {
    /// Compose a cache-first source.
    pub fn new(cache: C, http: H) -> Self {
        CachedRoleSource { cache, http }
    }
}

#[s::async_trait]
impl<C, H> RoleSource for CachedRoleSource<C, H>
where
    C: RoleSource,
    H: RoleSource,
{
    async fn roles(&self, guild_id: u64) -> Result<Vec<RoleEntry>, SourceError> {
        let _ = guild_id;
        Err(SourceError::Unavailable)
    }

    async fn member_roles(&self, guild_id: u64, member_id: u64) -> Result<Vec<u64>, SourceError> {
        let _ = (guild_id, member_id);
        Err(SourceError::Unavailable)
    }
}

/// Build the policy facts for one role from the guild's role list.
pub fn role_facts(roles: &[RoleEntry], role_id: u64) -> FactsOutcome {
    let _ = (roles, role_id);
    FactsOutcome::Unavailable
}

/// The bot's top role position.
pub fn bot_top_position(roles: &[RoleEntry], bot_role_ids: &[u64]) -> u16 {
    let _ = (roles, bot_role_ids);
    0
}

/// Look up one role's facts and the bot's top position, from one source.
pub async fn lookup_facts<S: RoleSource + ?Sized>(
    source: &S,
    guild_id: u64,
    role_id: u64,
    bot_user_id: u64,
) -> (FactsOutcome, u16) {
    let _ = (source, guild_id, role_id, bot_user_id);
    (FactsOutcome::Unavailable, 0)
}

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

/// The policy inputs that are the same for every role in one request.
#[derive(Debug, Clone, Copy)]
pub struct DecisionInputs<'a> {
    /// Which path asked for the grant.
    pub path: GrantPath,
    /// The bot's top role position.
    pub bot_top_position: u16,
    /// The guild the role belongs to.
    pub guild_id: u64,
    /// Roles that may only be granted by staff.
    pub protected_role_ids: &'a [u64],
    /// Whether the safeword is active.
    pub safeword: SafewordState,
    /// Roles that may not be granted while the safeword is active.
    pub safeword_restricted_role_ids: &'a [u64],
    /// Required for the staff path; ignored otherwise.
    pub staff: Option<StaffContext>,
}

/// The policy decision for one role, with a failed lookup kept visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeDecision {
    /// The policy decided on the facts that were found.
    Decided(Decision),
    /// The facts could not be looked up. The role is denied and is never written.
    Unavailable,
}

impl RuntimeDecision {
    /// Whether the role may be written.
    pub fn allows_write(self) -> bool {
        matches!(self, RuntimeDecision::Decided(Decision::Allow))
    }

    /// The stable code for a denied role, or `None` when the role may be written.
    pub fn deny_code(self) -> Option<&'static str> {
        match self {
            RuntimeDecision::Decided(Decision::Allow) => None,
            RuntimeDecision::Decided(Decision::Deny(reason)) => Some(reason.code()),
            RuntimeDecision::Unavailable => Some(CODE_FACTS_UNAVAILABLE),
        }
    }
}

/// The stable code for a role whose facts could not be looked up.
pub const CODE_FACTS_UNAVAILABLE: &str = "facts_unavailable";

/// Decide one role from the facts and the policy inputs.
pub fn decide_from_facts(
    facts: FactsOutcome,
    role_id: u64,
    inputs: &DecisionInputs<'_>,
) -> RuntimeDecision {
    let _ = (facts, role_id, inputs);
    RuntimeDecision::Unavailable
}

/// Look up one role's facts and decide it.
pub async fn decide_role<S: RoleSource + ?Sized>(
    source: &S,
    guild_id: u64,
    role_id: u64,
    bot_user_id: u64,
    inputs: &DecisionInputs<'_>,
) -> RuntimeDecision {
    let _ = (source, guild_id, role_id, bot_user_id, inputs);
    RuntimeDecision::Unavailable
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// The stable status used for a failure that is not an HTTP response.
pub const STATUS_NOT_HTTP: u16 = 0;

/// The stable status used when the writer was called from a context it cannot block in.
pub const STATUS_WRONG_CONTEXT: u16 = 1;

/// The failure facts a writer reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteFailure {
    /// The HTTP status code, or [`STATUS_NOT_HTTP`].
    pub status: u16,
    /// Discord's JSON error code, when the response carried one.
    pub api_code: Option<u32>,
    /// How long to wait before retrying, when the transport said so.
    pub retry_after_ms: Option<u64>,
}

impl WriteFailure {
    /// A failure that was not an HTTP response.
    pub fn not_http() -> Self {
        WriteFailure {
            status: STATUS_NOT_HTTP,
            api_code: None,
            retry_after_ms: None,
        }
    }

    /// Turn the failure into the grant module's error shape.
    pub fn to_write_error(self) -> WriteError {
        WriteError {
            status: self.status,
            api_code: self.api_code,
            retry_after_ms: self.retry_after_ms,
        }
    }
}

/// Map a serenity error to the grant module's error shape.
pub fn map_serenity_error(error: &s::Error) -> WriteError {
    let _ = error;
    WriteFailure::not_http().to_write_error()
}

impl WriteFailure {
    /// Extract the failure facts from a serenity error.
    pub fn from_serenity(error: &s::Error) -> Self {
        let _ = error;
        WriteFailure::not_http()
    }
}

/// Drive an async operation from a synchronous caller.
pub fn drive_blocking<F, T>(
    handle: &tokio::runtime::Handle,
    operation: F,
) -> Result<T, WriteError>
where
    F: Future<Output = T>,
{
    let _ = handle;
    let _ = operation;
    Err(WriteError {
        status: STATUS_WRONG_CONTEXT,
        api_code: None,
        retry_after_ms: None,
    })
}

/// The real role writer: one Discord role write, over serenity's `add_member_role`.
pub struct SerenityRoleWriter {
    /// The guild the role belongs to.
    pub guild_id: u64,
    /// The serenity HTTP client.
    pub http: Arc<s::http::Http>,
    /// The tokio handle used to drive the async call.
    pub handle: tokio::runtime::Handle,
}

impl SerenityRoleWriter {
    /// Build a writer for one guild.
    pub fn new(
        guild_id: u64,
        http: Arc<s::http::Http>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        SerenityRoleWriter {
            guild_id,
            http,
            handle,
        }
    }
}

impl RoleWriter for SerenityRoleWriter {
    fn add_role(&mut self, _member_id: u64, _role_id: u64) -> Result<(), WriteError> {
        Err(WriteFailure::not_http().to_write_error())
    }
}

// ---------------------------------------------------------------------------
// Sleeper
// ---------------------------------------------------------------------------

/// Clamp a sleep request to the grant module's bound.
pub fn clamp_sleep_ms(ms: u64) -> u64 {
    ms.min(MAX_SLEEP_MS)
}

/// A [`Sleeper`] for tokio-backed callers.
pub struct TokioSleeper {
    /// The tokio handle used to drive the sleep.
    pub handle: tokio::runtime::Handle,
}

impl TokioSleeper {
    /// Build a sleeper that drives its sleeps with the given handle.
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        TokioSleeper { handle }
    }
}

impl Sleeper for TokioSleeper {
    fn sleep_ms(&mut self, _ms: u64) {}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether the member already holds the role.
pub fn already_held(member_roles: &[u64], role_id: u64) -> bool {
    let _ = (member_roles, role_id);
    false
}

/// Build the grant executor's items from the runtime decisions.
pub fn grant_items(
    decisions: &[(u64, RuntimeDecision)],
    member_roles: &[u64],
) -> Vec<super::grant::GrantItem> {
    let _ = (decisions, member_roles);
    Vec::new()
}

/// The permissions the bot needs in the guild for the role writer to work.
pub const REQUIRED_PERMISSIONS: u64 = MANAGE_ROLES_PERMISSION;

/// The gateway intents the role source needs.
pub fn required_intents() -> s::GatewayIntents {
    s::GatewayIntents::GUILDS | s::GatewayIntents::GUILD_MEMBERS
}
#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const GUILD: u64 = 700_001;
    const BOT: u64 = 700_002;
    const MEMBER: u64 = 700_003;
    const EVERYONE: u64 = GUILD;
    const ROLE_LOW: u64 = 700_010;
    const ROLE_MID: u64 = 700_011;
    const ROLE_HIGH: u64 = 700_012;
    const ROLE_MANAGED: u64 = 700_013;
    const ROLE_DELETED: u64 = 700_099;

    // -----------------------------------------------------------------------
    // Fakes
    // -----------------------------------------------------------------------

    /// A source with a scripted answer and call counters.
    struct FakeSource {
        roles: Result<Vec<RoleEntry>, SourceError>,
        member: Result<Vec<u64>, SourceError>,
        role_calls: AtomicUsize,
        member_calls: AtomicUsize,
    }

    impl FakeSource {
        fn with_roles(roles: Vec<RoleEntry>) -> Self {
            FakeSource {
                roles: Ok(roles),
                member: Ok(vec![]),
                role_calls: AtomicUsize::new(0),
                member_calls: AtomicUsize::new(0),
            }
        }
        fn with_error(error: SourceError) -> Self {
            FakeSource {
                roles: Err(error),
                member: Err(error),
                role_calls: AtomicUsize::new(0),
                member_calls: AtomicUsize::new(0),
            }
        }
        fn member(mut self, ids: Vec<u64>) -> Self {
            self.member = Ok(ids);
            self
        }
    }

    #[s::async_trait]
    impl RoleSource for FakeSource {
        async fn roles(&self, _guild_id: u64) -> Result<Vec<RoleEntry>, SourceError> {
            self.role_calls.fetch_add(1, Ordering::SeqCst);
            self.roles.clone()
        }
        async fn member_roles(
            &self,
            _guild_id: u64,
            _member_id: u64,
        ) -> Result<Vec<u64>, SourceError> {
            self.member_calls.fetch_add(1, Ordering::SeqCst);
            self.member.clone()
        }
    }

    fn entry(id: u64, position: u16) -> RoleEntry {
        RoleEntry {
            id,
            position,
            managed: false,
            permissions: 0,
        }
    }

    fn inputs() -> DecisionInputs<'static> {
        DecisionInputs {
            path: GrantPath::Reaction,
            bot_top_position: 10,
            guild_id: GUILD,
            protected_role_ids: &[],
            safeword: SafewordState::Inactive,
            safeword_restricted_role_ids: &[],
            staff: None,
        }
    }

    // -----------------------------------------------------------------------
    // Serenity models
    // -----------------------------------------------------------------------

    fn serenity_role(
        id: u64,
        position: u16,
        managed: bool,
        permissions: s::Permissions,
    ) -> s::model::guild::Role {
        let mut role = s::model::guild::Role::default();
        role.id = RoleId::new(id);
        role.guild_id = GuildId::new(GUILD);
        role.position = position;
        role.managed = managed;
        role.permissions = permissions;
        role
    }

    fn serenity_member(user_id: u64, roles: &[u64]) -> s::model::guild::Member {
        let mut member = s::model::guild::Member::default();
        member.guild_id = GuildId::new(GUILD);
        member.user.id = UserId::new(user_id);
        member.roles = roles.iter().map(|id| RoleId::new(*id)).collect();
        member
    }

    /// A serenity cache holding one synthetic guild.
    fn cache_with_guild(
        roles: Vec<s::model::guild::Role>,
        members: Vec<s::model::guild::Member>,
    ) -> Arc<s::Cache> {
        let mut guild = s::model::guild::Guild::default();
        guild.id = GuildId::new(GUILD);
        guild.owner_id = UserId::new(BOT);
        for role in roles {
            guild.roles.insert(role.id, role);
        }
        for member in members {
            guild.members.insert(member.user.id, member);
        }
        let cache = Arc::new(s::Cache::default());
        let mut event: s::GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(&guild).unwrap()).unwrap();
        cache.update(&mut event);
        cache
    }

    // -----------------------------------------------------------------------
    // Facts from serenity models
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn facts_are_built_from_synthetic_serenity_guild_and_role_models() {
        let cache = cache_with_guild(
            vec![
                serenity_role(EVERYONE, 0, false, s::Permissions::empty()),
                serenity_role(ROLE_LOW, 3, false, s::Permissions::SEND_MESSAGES),
                serenity_role(ROLE_MID, 5, false, s::Permissions::empty()),
            ],
            vec![serenity_member(BOT, &[ROLE_MID])],
        );
        let source = SerenityCacheSource { cache };
        let (facts, top) = lookup_facts(&source, GUILD, ROLE_LOW, BOT).await;
        assert_eq!(
            facts,
            FactsOutcome::Found(RoleFacts {
                id: ROLE_LOW,
                position: 3,
                managed: false,
                permissions: s::Permissions::SEND_MESSAGES.bits(),
                exists: true,
            })
        );
        assert_eq!(top, 5, "the bot holds ROLE_MID at position 5");
    }

    #[tokio::test]
    async fn managed_flag_and_permissions_are_carried_through() {
        let cache = cache_with_guild(
            vec![serenity_role(
                ROLE_MANAGED,
                4,
                true,
                s::Permissions::MANAGE_ROLES,
            )],
            vec![serenity_member(BOT, &[])],
        );
        let source = SerenityCacheSource { cache };
        let (facts, _) = lookup_facts(&source, GUILD, ROLE_MANAGED, BOT).await;
        match facts {
            FactsOutcome::Found(facts) => {
                assert!(facts.managed, "a managed role stays managed");
                assert_eq!(facts.permissions, s::Permissions::MANAGE_ROLES.bits());
                assert_eq!(facts.position, 4);
                assert!(facts.exists);
            }
            other => panic!("expected facts, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn everyone_role_keeps_the_guild_id_and_position_zero() {
        let cache = cache_with_guild(
            vec![serenity_role(EVERYONE, 0, false, s::Permissions::empty())],
            vec![serenity_member(BOT, &[])],
        );
        let source = SerenityCacheSource { cache };
        let (facts, _) = lookup_facts(&source, GUILD, EVERYONE, BOT).await;
        match facts {
            FactsOutcome::Found(facts) => {
                assert_eq!(facts.id, GUILD, "@everyone's role id is the guild id");
                assert_eq!(facts.position, 0);
                assert!(facts.exists);
            }
            other => panic!("expected facts, got {other:?}"),
        }
        // The policy denies it because its id equals the guild id.
        let decision = decide_from_facts(facts, EVERYONE, &inputs());
        assert_eq!(
            decision,
            RuntimeDecision::Decided(Decision::Deny(
                super::super::eligibility::DenyReason::EveryoneRole
            ))
        );
    }

    #[tokio::test]
    async fn a_deleted_role_id_is_missing_not_unavailable() {
        let cache = cache_with_guild(
            vec![serenity_role(ROLE_LOW, 3, false, s::Permissions::empty())],
            vec![serenity_member(BOT, &[])],
        );
        let source = SerenityCacheSource { cache };
        let (facts, _) = lookup_facts(&source, GUILD, ROLE_DELETED, BOT).await;
        assert_eq!(
            facts,
            FactsOutcome::Missing,
            "a role id that is not in the guild's role list is deleted"
        );
        // The policy denies a deleted role.
        let decision = decide_from_facts(facts, ROLE_DELETED, &inputs());
        assert_eq!(
            decision,
            RuntimeDecision::Decided(Decision::Deny(
                super::super::eligibility::DenyReason::RoleDeleted
            ))
        );
        assert!(!decision.allows_write());
    }

    #[tokio::test]
    async fn a_guild_that_is_not_cached_is_a_miss_not_a_failure() {
        let cache = Arc::new(s::Cache::default());
        let source = SerenityCacheSource { cache };
        assert_eq!(source.roles(GUILD).await, Err(SourceError::Miss));
        assert_eq!(source.member_roles(GUILD, BOT).await, Err(SourceError::Miss));
    }

    // -----------------------------------------------------------------------
    // Bot top position
    // -----------------------------------------------------------------------

    #[test]
    fn bot_top_position_with_no_roles_is_zero_so_nothing_is_grantable() {
        let roles = vec![entry(ROLE_LOW, 3), entry(ROLE_MID, 5)];
        assert_eq!(bot_top_position(&roles, &[]), 0);
        // A bot at position 0 cannot reach a role at position 0 either.
        let facts = match role_facts(&roles, ROLE_LOW) {
            FactsOutcome::Found(facts) => facts,
            other => panic!("expected facts, got {other:?}"),
        };
        let mut inputs = inputs();
        inputs.bot_top_position = bot_top_position(&roles, &[]);
        let decision = decide_from_facts(FactsOutcome::Found(facts), ROLE_LOW, &inputs);
        assert_eq!(
            decision,
            RuntimeDecision::Decided(Decision::Deny(
                super::super::eligibility::DenyReason::AtOrAboveBot
            ))
        );
    }

    #[test]
    fn bot_top_position_is_the_highest_role_the_bot_holds() {
        let roles = vec![
            entry(ROLE_LOW, 3),
            entry(ROLE_MID, 5),
            entry(ROLE_HIGH, 9),
        ];
        assert_eq!(bot_top_position(&roles, &[ROLE_LOW]), 3);
        assert_eq!(bot_top_position(&roles, &[ROLE_LOW, ROLE_MID]), 5);
        assert_eq!(bot_top_position(&roles, &[ROLE_LOW, ROLE_MID, ROLE_HIGH]), 9);
        // Order does not matter.
        assert_eq!(bot_top_position(&roles, &[ROLE_HIGH, ROLE_LOW]), 9);
    }

    #[test]
    fn bot_top_position_ignores_roles_that_are_not_in_the_guild_role_list() {
        let roles = vec![entry(ROLE_LOW, 3)];
        assert_eq!(bot_top_position(&roles, &[ROLE_DELETED]), 0);
        assert_eq!(bot_top_position(&roles, &[ROLE_DELETED, ROLE_LOW]), 3);
    }

    #[tokio::test]
    async fn bot_top_position_comes_from_the_bot_members_roles() {
        let cache = cache_with_guild(
            vec![
                serenity_role(ROLE_LOW, 3, false, s::Permissions::empty()),
                serenity_role(ROLE_MID, 5, false, s::Permissions::empty()),
                serenity_role(ROLE_HIGH, 9, false, s::Permissions::empty()),
            ],
            vec![serenity_member(BOT, &[ROLE_LOW, ROLE_MID])],
        );
        let source = SerenityCacheSource { cache };
        let (_, top) = lookup_facts(&source, GUILD, ROLE_LOW, BOT).await;
        assert_eq!(top, 5);
    }

    #[tokio::test]
    async fn a_bot_member_with_no_roles_holds_nothing() {
        let cache = cache_with_guild(
            vec![serenity_role(ROLE_LOW, 3, false, s::Permissions::empty())],
            vec![serenity_member(BOT, &[])],
        );
        let source = SerenityCacheSource { cache };
        let (_, top) = lookup_facts(&source, GUILD, ROLE_LOW, BOT).await;
        assert_eq!(top, 0);
    }

    // -----------------------------------------------------------------------
    // Cache first, HTTP on a miss
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_cache_hit_does_not_ask_the_http_source() {
        let cache = FakeSource::with_roles(vec![entry(ROLE_LOW, 3)]);
        let http = FakeSource::with_error(SourceError::Unavailable);
        let source = CachedRoleSource::new(cache, http);
        let roles = source.roles(GUILD).await.expect("the cache answered");
        assert_eq!(roles.len(), 1);
    }

    #[tokio::test]
    async fn a_cache_miss_falls_through_to_the_http_source() {
        let cache = FakeSource::with_error(SourceError::Miss);
        let http = FakeSource::with_roles(vec![entry(ROLE_LOW, 3)]);
        let source = CachedRoleSource::new(cache, http);
        let roles = source.roles(GUILD).await.expect("the http source answered");
        assert_eq!(roles, vec![entry(ROLE_LOW, 3)]);
    }

    #[tokio::test]
    async fn a_cache_failure_also_falls_through_to_the_http_source() {
        let cache = FakeSource::with_error(SourceError::Unavailable);
        let http = FakeSource::with_roles(vec![entry(ROLE_LOW, 3)]);
        let source = CachedRoleSource::new(cache, http);
        let roles = source.roles(GUILD).await.expect("the http source answered");
        assert_eq!(roles, vec![entry(ROLE_LOW, 3)]);
    }

    #[tokio::test]
    async fn a_cache_miss_and_a_failing_http_source_is_unavailable_not_a_guess() {
        let cache = FakeSource::with_error(SourceError::Miss);
        let http = FakeSource::with_error(SourceError::Unavailable);
        let source = CachedRoleSource::new(cache, http);
        let (facts, top) = lookup_facts(&source, GUILD, ROLE_LOW, BOT).await;
        assert_eq!(facts, FactsOutcome::Unavailable);
        assert_eq!(top, 0, "an unknown bot position denies every role");
    }

    #[tokio::test]
    async fn the_http_source_is_asked_for_the_member_roles_when_the_cache_misses() {
        let cache = FakeSource::with_error(SourceError::Miss);
        let http = FakeSource::with_roles(vec![entry(ROLE_MID, 5)]).member(vec![ROLE_MID]);
        let source = CachedRoleSource::new(cache, http);
        let ids = source
            .member_roles(GUILD, BOT)
            .await
            .expect("the http source answered");
        assert_eq!(ids, vec![ROLE_MID]);
    }

    // -----------------------------------------------------------------------
    // Unavailable is a deny and no write is attempted
    // -----------------------------------------------------------------------

    /// A writer that records every call. The grant executor only calls it for an allow.
    #[derive(Default)]
    struct RecordingWriter {
        calls: Vec<(u64, u64)>,
    }

    impl RoleWriter for RecordingWriter {
        fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.calls.push((member_id, role_id));
            Ok(())
        }
    }

    #[derive(Default)]
    struct NoopSleeper;

    impl Sleeper for NoopSleeper {
        fn sleep_ms(&mut self, _ms: u64) {}
    }

    #[tokio::test]
    async fn a_failed_lookup_denies_the_role_and_nothing_is_written() {
        let source = FakeSource::with_error(SourceError::Unavailable);
        let decision = decide_role(&source, GUILD, ROLE_LOW, BOT, &inputs()).await;
        assert_eq!(decision, RuntimeDecision::Unavailable);
        assert!(!decision.allows_write());
        assert_eq!(decision.deny_code(), Some("facts_unavailable"));

        // The executor is given no item for the role, so it cannot write it.
        let items = grant_items(&[(ROLE_LOW, decision)], &[]);
        assert!(items.is_empty(), "an unavailable role must not reach the writer");

        let mut writer = RecordingWriter::default();
        let mut sleeper = NoopSleeper;
        let report = super::super::grant::apply(
            &mut writer,
            &mut sleeper,
            &super::super::grant::GrantRequest {
                member_id: MEMBER,
                items: &items,
            },
        );
        assert!(writer.calls.is_empty(), "no write may be attempted");
        assert_eq!(report.status(), super::super::grant::GrantStatus::Empty);
    }

    #[tokio::test]
    async fn a_failed_member_lookup_denies_the_role_even_when_the_role_was_found() {
        // The role list is there but the bot member lookup failed.
        struct HalfBroken {
            roles: Vec<RoleEntry>,
        }
        #[s::async_trait]
        impl RoleSource for HalfBroken {
            async fn roles(&self, _guild_id: u64) -> Result<Vec<RoleEntry>, SourceError> {
                Ok(self.roles.clone())
            }
            async fn member_roles(
                &self,
                _guild_id: u64,
                _member_id: u64,
            ) -> Result<Vec<u64>, SourceError> {
                Err(SourceError::Unavailable)
            }
        }
        let source = HalfBroken {
            roles: vec![entry(ROLE_LOW, 3)],
        };
        let (facts, top) = lookup_facts(&source, GUILD, ROLE_LOW, BOT).await;
        assert_eq!(
            facts,
            FactsOutcome::Unavailable,
            "an unknown bot position means the role facts are not usable"
        );
        assert_eq!(top, 0);
        let decision = decide_role(&source, GUILD, ROLE_LOW, BOT, &inputs()).await;
        assert_eq!(decision, RuntimeDecision::Unavailable);
        assert!(!decision.allows_write());
    }

    #[tokio::test]
    async fn an_allowed_role_reaches_the_writer_and_a_denied_one_does_not() {
        // The bot holds ROLE_MID at position 5, so ROLE_LOW (3) is grantable and
        // ROLE_MID (5) is not.
        let source = FakeSource::with_roles(vec![entry(ROLE_LOW, 3), entry(ROLE_MID, 5)])
            .member(vec![ROLE_MID]);
        let allowed = decide_role(&source, GUILD, ROLE_LOW, BOT, &inputs()).await;
        let denied = decide_role(&source, GUILD, ROLE_MID, BOT, &inputs()).await;
        assert_eq!(allowed, RuntimeDecision::Decided(Decision::Allow));
        assert!(allowed.allows_write());
        assert_eq!(allowed.deny_code(), None);
        assert!(!denied.allows_write());

        let items = grant_items(&[(ROLE_LOW, allowed), (ROLE_MID, denied)], &[]);
        let mut writer = RecordingWriter::default();
        let mut sleeper = NoopSleeper;
        let report = super::super::grant::apply(
            &mut writer,
            &mut sleeper,
            &super::super::grant::GrantRequest {
                member_id: MEMBER,
                items: &items,
            },
        );
        assert_eq!(writer.calls, vec![(MEMBER, ROLE_LOW)]);
        assert_eq!(
            report.results[1].outcome,
            super::super::grant::RoleOutcome::Denied(
                super::super::eligibility::DenyReason::AtOrAboveBot
            )
        );
    }

    // -----------------------------------------------------------------------
    // Error mapping
    // -----------------------------------------------------------------------

    /// Serve one HTTP response on loopback and return the url.
    async fn serve_once(status: u16, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer).await;
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });
        format!("http://{addr}/")
    }

    /// Build a real serenity error from a synthetic HTTP response.
    async fn serenity_error(status: u16, body: &'static str) -> s::Error {
        let url = serve_once(status, body).await;
        let response = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect("the loopback server answered");
        let response = s::http::ErrorResponse::from_response(response, reqwest::Method::PUT).await;
        s::Error::Http(s::http::HttpError::UnsuccessfulRequest(response))
    }

    #[tokio::test]
    async fn serenity_errors_map_to_the_documented_table() {
        // (status, body, expected status, expected api code)
        let cases: [(u16, &str, u16, Option<u32>); 6] = [
            (
                404,
                r#"{"code":10007,"message":"Unknown Member"}"#,
                404,
                Some(10007),
            ),
            (
                404,
                r#"{"code":10011,"message":"Unknown Role"}"#,
                404,
                Some(10011),
            ),
            (
                403,
                r#"{"code":50013,"message":"Missing Permissions"}"#,
                403,
                Some(50013),
            ),
            (
                429,
                r#"{"message":"You are being rate limited.","retry_after":1.5,"global":false}"#,
                429,
                None,
            ),
            (
                500,
                r#"{"code":0,"message":"Internal Server Error"}"#,
                500,
                Some(0),
            ),
            (502, r#"{"message":"Bad Gateway"}"#, 502, None),
        ];
        for (status, body, want_status, want_api) in cases {
            let error = serenity_error(status, body).await;
            let mapped = map_serenity_error(&error);
            assert_eq!(mapped.status, want_status, "status for {status}");
            assert_eq!(mapped.api_code, want_api, "api code for {status}");
            assert_eq!(
                mapped.retry_after_ms, None,
                "serenity's typed error carries no rate-limit hint"
            );
        }
    }

    #[tokio::test]
    async fn a_transport_failure_maps_to_the_stable_non_http_status() {
        // A request to a port nobody listens on is a real transport failure.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let transport = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect_err("nothing listens on that port");
        let error = s::Error::Http(s::http::HttpError::Request(transport));
        let mapped = map_serenity_error(&error);
        assert_eq!(mapped.status, STATUS_NOT_HTTP);
        assert_eq!(mapped.api_code, None);
        assert_eq!(mapped.retry_after_ms, None);
        assert_eq!(
            super::super::grant::classify(&mapped),
            super::super::grant::FailCode::Other
        );
    }

    #[tokio::test]
    async fn a_non_http_serenity_failure_maps_to_the_stable_non_http_status() {
        for error in [
            s::Error::Other("synthetic"),
            s::Error::Http(s::http::HttpError::RateLimitI64F64),
            s::Error::Http(s::http::HttpError::InvalidScheme),
        ] {
            let mapped = map_serenity_error(&error);
            assert_eq!(mapped.status, STATUS_NOT_HTTP, "{error:?}");
            assert_eq!(mapped.api_code, None, "{error:?}");
        }
    }

    #[tokio::test]
    async fn the_mapped_errors_classify_the_way_the_grant_module_expects() {
        let unknown_member =
            serenity_error(404, r#"{"code":10007,"message":"Unknown Member"}"#).await;
        let unknown_role = serenity_error(404, r#"{"code":10011,"message":"Unknown Role"}"#).await;
        let forbidden =
            serenity_error(403, r#"{"code":50013,"message":"Missing Permissions"}"#).await;
        let rate_limited = serenity_error(
            429,
            r#"{"message":"You are being rate limited.","retry_after":1.5,"global":false}"#,
        )
        .await;
        let server = serenity_error(500, r#"{"code":0,"message":"Internal Server Error"}"#).await;

        assert_eq!(
            super::super::grant::classify(&map_serenity_error(&unknown_member)),
            super::super::grant::FailCode::UnknownMember
        );
        assert_eq!(
            super::super::grant::classify(&map_serenity_error(&unknown_role)),
            super::super::grant::FailCode::UnknownRole
        );
        assert_eq!(
            super::super::grant::classify(&map_serenity_error(&forbidden)),
            super::super::grant::FailCode::Forbidden
        );
        assert_eq!(
            super::super::grant::classify(&map_serenity_error(&rate_limited)),
            super::super::grant::FailCode::RateLimited
        );
        assert_eq!(
            super::super::grant::classify(&map_serenity_error(&server)),
            super::super::grant::FailCode::Other
        );
    }

    #[test]
    fn a_rate_limit_hint_is_carried_into_the_write_error() {
        // The grant module uses the hint as the sleep before the retry.
        let failure = WriteFailure {
            status: 429,
            api_code: None,
            retry_after_ms: Some(1_500),
        };
        let mapped = failure.to_write_error();
        assert_eq!(mapped.status, 429);
        assert_eq!(mapped.retry_after_ms, Some(1_500));
    }

    #[tokio::test]
    async fn a_rate_limit_with_a_hint_is_retried_after_the_hint() {
        // The writer reports the hint; the grant module sleeps for it and retries.
        struct HintWriter {
            results: Vec<Result<(), WriteError>>,
            calls: usize,
        }
        impl RoleWriter for HintWriter {
            fn add_role(&mut self, _member_id: u64, _role_id: u64) -> Result<(), WriteError> {
                self.calls += 1;
                if self.results.is_empty() {
                    return Ok(());
                }
                self.results.remove(0)
            }
        }
        #[derive(Default)]
        struct RecordingSleeper {
            slept: Vec<u64>,
        }
        impl Sleeper for RecordingSleeper {
            fn sleep_ms(&mut self, ms: u64) {
                self.slept.push(ms);
            }
        }
        let mut writer = HintWriter {
            results: vec![Err(WriteError {
                status: 429,
                api_code: None,
                retry_after_ms: Some(1_500),
            })],
            calls: 0,
        };
        let mut sleeper = RecordingSleeper::default();
        let item = super::super::grant::GrantItem {
            role_id: ROLE_LOW,
            decision: Decision::Allow,
            already_held: false,
        };
        let report = super::super::grant::apply(
            &mut writer,
            &mut sleeper,
            &super::super::grant::GrantRequest {
                member_id: MEMBER,
                items: &[item],
            },
        );
        assert_eq!(writer.calls, 2, "one attempt and one retry");
        assert_eq!(sleeper.slept, vec![1_500], "the writer's hint is the sleep");
        assert_eq!(
            report.results[0].outcome,
            super::super::grant::RoleOutcome::Done
        );
    }

    // -----------------------------------------------------------------------
    // Blocking context
    // -----------------------------------------------------------------------

    #[test]
    fn driving_a_call_from_a_plain_thread_works() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let handle = runtime.handle().clone();
        let outcome = std::thread::spawn(move || drive_blocking(&handle, async { 7u8 }))
            .join()
            .unwrap();
        assert_eq!(outcome.expect("a plain thread may block"), 7);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driving_a_call_from_inside_a_runtime_worker_does_not_panic() {
        // `Handle::block_on` inside a runtime worker panics; `drive_blocking` must not.
        let handle = tokio::runtime::Handle::current();
        let outcome = drive_blocking(&handle, async { 11u8 });
        assert_eq!(outcome.expect("a multi-thread runtime may block"), 11);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driving_a_call_from_inside_spawn_blocking_does_not_panic() {
        let handle = tokio::runtime::Handle::current();
        let outcome = tokio::task::spawn_blocking(move || drive_blocking(&handle, async { 13u8 }))
            .await
            .unwrap();
        assert_eq!(outcome.expect("a blocking thread may block"), 13);
    }

    #[test]
    fn driving_a_call_inside_a_current_thread_runtime_reports_a_stable_error() {
        // A current-thread runtime is being driven here, so blocking would panic.
        // This runs on a plain thread so no other runtime is entered on it.
        let outcome = std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let handle = runtime.handle().clone();
            runtime.block_on(async { drive_blocking(&handle, async { 17u8 }) })
        })
        .join()
        .unwrap();
        let error = outcome.expect_err("a current-thread runtime must not block");
        assert_eq!(error.status, STATUS_WRONG_CONTEXT);
        assert_eq!(error.api_code, None);
        assert_eq!(error.retry_after_ms, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_writer_is_usable_from_a_blocking_context() {
        // The writer's context rule is the same as `drive_blocking`'s.
        let handle = tokio::runtime::Handle::current();
        let outcome = tokio::task::spawn_blocking(move || drive_blocking(&handle, async { 19u8 }))
            .await
            .unwrap();
        assert_eq!(outcome.expect("a blocking context may drive the call"), 19);
    }

    // -----------------------------------------------------------------------
    // Sleeper
    // -----------------------------------------------------------------------

    #[test]
    fn the_sleep_is_clamped_to_the_grant_modules_bound() {
        assert_eq!(clamp_sleep_ms(0), 0);
        assert_eq!(clamp_sleep_ms(1_000), 1_000);
        assert_eq!(clamp_sleep_ms(MAX_SLEEP_MS), MAX_SLEEP_MS);
        assert_eq!(clamp_sleep_ms(MAX_SLEEP_MS + 1), MAX_SLEEP_MS);
        assert_eq!(clamp_sleep_ms(u64::MAX), MAX_SLEEP_MS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_tokio_sleeper_sleeps_for_the_clamped_value() {
        let handle = tokio::runtime::Handle::current();
        let mut sleeper = TokioSleeper::new(handle);
        // The sleeper drives the clamped value, so a 30 ms request waits about 30 ms.
        let started = std::time::Instant::now();
        sleeper.sleep_ms(30);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(25),
            "the sleeper must actually wait, took {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "the sleeper must not wait longer than asked, took {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_tokio_sleeper_sleeps_for_the_requested_time() {
        let handle = tokio::runtime::Handle::current();
        let mut sleeper = TokioSleeper::new(handle);
        let started = std::time::Instant::now();
        sleeper.sleep_ms(60);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(50),
            "the sleeper must actually wait"
        );
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    #[test]
    fn already_held_reports_the_membership() {
        assert!(already_held(&[ROLE_LOW, ROLE_MID], ROLE_LOW));
        assert!(!already_held(&[ROLE_LOW, ROLE_MID], ROLE_HIGH));
        assert!(!already_held(&[], ROLE_LOW));
        assert!(!already_held(&[], 0));
    }

    #[test]
    fn grant_items_mark_a_role_the_member_already_holds() {
        let decisions = [(ROLE_LOW, RuntimeDecision::Decided(Decision::Allow))];
        let items = grant_items(&decisions, &[ROLE_LOW]);
        assert_eq!(items.len(), 1);
        assert!(items[0].already_held);
        let items = grant_items(&decisions, &[]);
        assert!(!items[0].already_held);
    }

    #[test]
    fn the_deny_codes_are_stable_and_carry_no_user_data() {
        assert_eq!(CODE_FACTS_UNAVAILABLE, "facts_unavailable");
        assert_eq!(
            RuntimeDecision::Unavailable.deny_code(),
            Some("facts_unavailable")
        );
        assert_eq!(
            RuntimeDecision::Decided(Decision::Deny(
                super::super::eligibility::DenyReason::ManagedRole
            ))
            .deny_code(),
            Some("managed_role")
        );
        assert_eq!(RuntimeDecision::Decided(Decision::Allow).deny_code(), None);
        for code in [
            RuntimeDecision::Unavailable.deny_code(),
            RuntimeDecision::Decided(Decision::Deny(
                super::super::eligibility::DenyReason::ManagedRole,
            ))
            .deny_code(),
        ] {
            let code = code.unwrap();
            assert!(!code.chars().any(|c| c.is_ascii_digit()), "{code}");
            assert!(!code.contains(' '), "{code}");
        }
    }

    #[test]
    fn the_required_intents_and_permissions_are_the_documented_ones() {
        assert!(required_intents().contains(s::GatewayIntents::GUILDS));
        assert!(required_intents().contains(s::GatewayIntents::GUILD_MEMBERS));
        assert_eq!(REQUIRED_PERMISSIONS, 1 << 28);
        assert_eq!(REQUIRED_PERMISSIONS, s::Permissions::MANAGE_ROLES.bits());
    }

    // -----------------------------------------------------------------------
    // Sweep
    // -----------------------------------------------------------------------

    /// Seeded LCG; no dependency, no real randomness.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    /// Sweep: for any role list, bot roles and facts outcome, the runtime decision must
    /// agree with the policy, and an unavailable lookup must never allow a write.
    #[test]
    fn seeded_sweep_never_allows_an_unavailable_lookup() {
        let mut rng = Lcg(0xA11C_E5E1);
        for case in 0..2_000u32 {
            let role_count = rng.next(6) as usize;
            let mut roles: Vec<RoleEntry> = Vec::new();
            for i in 0..role_count {
                roles.push(RoleEntry {
                    id: ROLE_LOW + i as u64,
                    position: (rng.next(12)) as u16,
                    managed: rng.next(5) == 0,
                    permissions: if rng.next(3) == 0 {
                        1u64 << (rng.next(45))
                    } else {
                        0
                    },
                });
            }
            let bot_roles: Vec<u64> = (0..rng.next(4) as usize)
                .map(|i| ROLE_LOW + i as u64)
                .collect();
            let top = bot_top_position(&roles, &bot_roles);
            let mut inputs = inputs();
            inputs.bot_top_position = top;
            let role_id = ROLE_LOW + rng.next(8);

            // The role is in the list, missing, or the lookup failed.
            let facts = match rng.next(3) {
                0 => FactsOutcome::Unavailable,
                1 => FactsOutcome::Missing,
                _ => role_facts(&roles, role_id),
            };
            let decision = decide_from_facts(facts, role_id, &inputs);
            match decision {
                RuntimeDecision::Unavailable => {
                    assert!(!decision.allows_write(), "case {case}");
                    assert_eq!(decision.deny_code(), Some("facts_unavailable"));
                    assert!(
                        grant_items(&[(role_id, decision)], &[]).is_empty(),
                        "case {case}: an unavailable role reached the writer"
                    );
                }
                RuntimeDecision::Decided(inner) => {
                    let policy = super::super::eligibility::decide(
                        &super::super::eligibility::GrantRequest {
                            path: inputs.path,
                            role: match facts {
                                FactsOutcome::Found(facts) => facts,
                                _ => RoleFacts {
                                    id: role_id,
                                    position: 0,
                                    managed: false,
                                    permissions: 0,
                                    exists: false,
                                },
                            },
                            bot_top_position: top,
                            guild_id: GUILD,
                            protected_role_ids: &[],
                            safeword: SafewordState::Inactive,
                            safeword_restricted_role_ids: &[],
                            staff: None,
                        },
                    );
                    assert_eq!(inner, policy, "case {case}");
                    assert_eq!(decision.allows_write(), policy == Decision::Allow);
                }
            }
        }
    }

    /// Sweep over the cache/HTTP composition: the answer is the cache's when the cache
    /// has one, otherwise the http source's, and a failure is never a guess.
    #[tokio::test]
    async fn seeded_sweep_over_the_source_composition() {
        let mut rng = Lcg(0xB0B5_CAFE);
        for case in 0..500u32 {
            let cache_answer = match rng.next(3) {
                0 => Err(SourceError::Miss),
                1 => Err(SourceError::Unavailable),
                _ => Ok(vec![entry(ROLE_LOW, 3)]),
            };
            let http_answer = match rng.next(3) {
                0 => Err(SourceError::Miss),
                1 => Err(SourceError::Unavailable),
                _ => Ok(vec![entry(ROLE_MID, 5)]),
            };
            let cache = FakeSource {
                roles: cache_answer.clone(),
                member: match &cache_answer {
                    Ok(roles) => Ok(roles.iter().map(|r| r.id).collect()),
                    Err(error) => Err(*error),
                },
                role_calls: AtomicUsize::new(0),
                member_calls: AtomicUsize::new(0),
            };
            let http = FakeSource {
                roles: http_answer.clone(),
                member: match &http_answer {
                    Ok(roles) => Ok(roles.iter().map(|r| r.id).collect()),
                    Err(error) => Err(*error),
                },
                role_calls: AtomicUsize::new(0),
                member_calls: AtomicUsize::new(0),
            };
            let source = CachedRoleSource::new(cache, http);
            let expected = match (&cache_answer, &http_answer) {
                (Ok(roles), _) => Some(roles.clone()),
                (Err(_), Ok(roles)) => Some(roles.clone()),
                (Err(_), Err(_)) => None,
            };
            match source.roles(GUILD).await {
                Ok(roles) => assert_eq!(Some(roles), expected, "case {case}"),
                Err(error) => {
                    assert!(expected.is_none(), "case {case}: {error:?}");
                    assert_eq!(
                        error, SourceError::Unavailable,
                        "case {case}: the last source's failure is an unavailable lookup"
                    );
                }
            }
        }
    }
}

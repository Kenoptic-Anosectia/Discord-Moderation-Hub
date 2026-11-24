//! The serenity side of the safeword feature (W2, `docs/verification/WIRING-B.md`).
//!
//! The SW3 runtime and the SW4 command logic talk to Discord, the store and the clock through the
//! small synchronous traits in [`super::runtime`]. This file implements them over serenity's HTTP
//! client, drives each async call with [`drive_blocking`], and shares one store between the
//! reaction handler, the `/safeword` command and the reaction-role safeword check.
//!
//! None of this has run against Discord: only the pure helpers are tested. Every Discord
//! failure is reduced to a stable code before it leaves this file.

use super::command::{CommandConfig, ConfirmBook, Env};
use super::repository::Repository;
use super::runtime::{
    DmCode, DmSender, LookupCode, MemberRoles, Ops, ReactionUser, ReactionUsers, RoleFactsSource,
    SafewordContext, SafewordFeature, StaffAlerter, StateStore, WriteCode,
};
use super::state::{RoleId, State};
use crate::features::{Feature, JobSignal};
use crate::jobs::runner::BoxFuture;
use crate::roles::eligibility::SafewordState;
use crate::roles::grant::{classify, FailCode, WriteError};
use crate::roles::runtime::{
    self as roles_runtime, drive_blocking, CachedRoleSource, FactsOutcome, RoleSource,
    SerenityCacheSource, SerenityHttpSource, TokioSleeper, STATUS_WRONG_CONTEXT,
};
use poise::serenity_prelude as s;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::runtime::Handle;

/// Where the safeword store lives: the core database path and the files it must never alias.
#[derive(Clone, Debug)]
pub struct StoreSpec {
    /// `None` when `CORE_DB_PATH` was unusable.
    pub path: Option<String>,
    pub reserved: Vec<PathBuf>,
}

struct StoreInner {
    spec: StoreSpec,
    repo: Mutex<Option<Repository>>,
}

/// One safeword store shared by every user of it. The repository is opened on first use, after
/// the feature registry has validated and created the core database, so a refused database
/// path or reserved file never gets a second chance through here.
#[derive(Clone)]
pub struct SharedStore {
    inner: Arc<StoreInner>,
}

impl SharedStore {
    pub fn new(spec: StoreSpec) -> Self {
        SharedStore {
            inner: Arc::new(StoreInner {
                spec,
                repo: Mutex::new(None),
            }),
        }
    }

    /// Runs `f` on the repository, opening it first if needed. The error is a stable code.
    fn with<T>(
        &self,
        f: impl FnOnce(&mut Repository) -> Result<T, super::repository::RepoError>,
    ) -> Result<T, &'static str> {
        let mut guard = self
            .inner
            .repo
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            let path = self.inner.spec.path.as_deref().ok_or("unusable_setting")?;
            let repo = Repository::open(path, &self.inner.spec.reserved).map_err(|e| e.code())?;
            *guard = Some(repo);
        }
        match guard.as_mut() {
            Some(repo) => f(repo).map_err(|e| e.code()),
            None => Err("store_unavailable"),
        }
    }

    /// The member's state for the reaction-role path: anything but a stored idle state counts as
    /// active, and so does a store that cannot be read (a restricted role is then not granted).
    pub fn reaction_state(&self, guild: u64, member: u64) -> SafewordState {
        match self.with(|repo| repo.load(guild, member)) {
            Ok(None) | Ok(Some((State::Inactive, _))) => SafewordState::Inactive,
            Ok(Some(_)) | Err(_) => SafewordState::Active,
        }
    }
}

/// [`StateStore`] over the shared store.
struct StoreOps(SharedStore);

impl StateStore for StoreOps {
    fn load(&mut self, guild: u64, member: u64) -> Result<Option<(State, u64)>, &'static str> {
        self.0.with(|repo| repo.load(guild, member))
    }

    fn store(
        &mut self,
        guild: u64,
        member: u64,
        state: &State,
        expected_version: u64,
        now: i64,
    ) -> Result<u64, &'static str> {
        self.0
            .with(|repo| repo.store(guild, member, state, expected_version, now))
    }

    fn clear(
        &mut self,
        guild: u64,
        member: u64,
        expected_version: u64,
        now: i64,
    ) -> Result<u64, &'static str> {
        self.0
            .with(|repo| repo.clear(guild, member, expected_version, now))
    }

    /// The SW2 repository cannot list members, so the reconcile (not wired here) cannot run.
    fn members_with_state(&mut self, _guild: u64) -> Result<Vec<u64>, &'static str> {
        Err("members_not_supported")
    }
}

/// The unix second now (0 if the clock is before 1970).
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The failure code of a write error, as the safeword runtime names it.
pub fn write_code(error: &WriteError) -> WriteCode {
    if error.status == STATUS_WRONG_CONTEXT {
        return WriteCode::Other;
    }
    match classify(error) {
        FailCode::Forbidden => WriteCode::Forbidden,
        FailCode::UnknownRole => WriteCode::UnknownRole,
        FailCode::UnknownMember => WriteCode::UnknownMember,
        FailCode::RateLimited => WriteCode::RateLimited,
        FailCode::Other => WriteCode::Other,
    }
}

/// The failure code of a direct message. 50007 is Discord's "cannot send messages to this user".
pub fn dm_code(error: &WriteError) -> DmCode {
    const CANNOT_DM: u32 = 50007;
    if error.api_code == Some(CANNOT_DM) {
        return DmCode::Closed;
    }
    match classify(error) {
        FailCode::RateLimited => DmCode::RateLimited,
        _ => DmCode::Other,
    }
}

/// Discord access for one guild, with the handle that drives the async calls.
#[derive(Clone)]
struct Net {
    http: Arc<s::Http>,
    handle: Handle,
    guild: u64,
    bot_user: u64,
    staff_alert_channel: Option<u64>,
}

impl Net {
    /// Drives one call; `None` when it could not be driven here.
    fn run<T>(&self, op: impl Future<Output = T>) -> Option<T> {
        drive_blocking(&self.handle, op).ok()
    }

    fn source(
        &self,
        cache: &Arc<s::Cache>,
    ) -> CachedRoleSource<SerenityCacheSource, SerenityHttpSource> {
        CachedRoleSource::new(
            SerenityCacheSource {
                cache: cache.clone(),
            },
            SerenityHttpSource {
                http: self.http.clone(),
            },
        )
    }
}

struct Members(Net);

impl MemberRoles for Members {
    fn member_roles(&mut self, member_id: u64) -> Result<Vec<RoleId>, LookupCode> {
        let (http, guild) = (self.0.http.clone(), self.0.guild);
        let found = self.0.run(async move {
            http.get_member(s::GuildId::new(guild), s::UserId::new(member_id))
                .await
        });
        match found {
            Some(Ok(member)) => Ok(member.roles.iter().map(|r| r.get()).collect()),
            _ => Err(LookupCode::Unavailable),
        }
    }

    fn add_role(&mut self, member_id: u64, role_id: RoleId) -> Result<(), WriteCode> {
        let (http, guild) = (self.0.http.clone(), self.0.guild);
        let done = self.0.run(async move {
            http.add_member_role(
                s::GuildId::new(guild),
                s::UserId::new(member_id),
                s::RoleId::new(role_id),
                None,
            )
            .await
        });
        match done {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(write_code(&roles_runtime::map_serenity_error(&error))),
            None => Err(WriteCode::Other),
        }
    }

    fn remove_role(&mut self, member_id: u64, role_id: RoleId) -> Result<(), WriteCode> {
        let (http, guild) = (self.0.http.clone(), self.0.guild);
        let done = self.0.run(async move {
            http.remove_member_role(
                s::GuildId::new(guild),
                s::UserId::new(member_id),
                s::RoleId::new(role_id),
                None,
            )
            .await
        });
        match done {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(write_code(&roles_runtime::map_serenity_error(&error))),
            None => Err(WriteCode::Other),
        }
    }
}

struct Facts {
    net: Net,
    cache: Arc<s::Cache>,
}

impl RoleFactsSource for Facts {
    fn role_facts(&mut self, role_id: RoleId) -> FactsOutcome {
        let source = self.net.source(&self.cache);
        let guild = self.net.guild;
        match self.net.run(async move { source.roles(guild).await }) {
            Some(Ok(roles)) => roles_runtime::role_facts(&roles, role_id),
            _ => FactsOutcome::Unavailable,
        }
    }

    fn bot_top_position(&mut self) -> Option<u16> {
        let source = self.net.source(&self.cache);
        let (guild, bot) = (self.net.guild, self.net.bot_user);
        let both = self.net.run(async move {
            let roles = source.roles(guild).await.ok()?;
            let held = source.member_roles(guild, bot).await.ok()?;
            Some((roles, held))
        });
        let (roles, held) = both??;
        Some(roles_runtime::bot_top_position(&roles, &held))
    }
}

struct Dms(Net);

impl DmSender for Dms {
    fn send_dm(&mut self, user_id: u64, text: &str) -> Result<(), DmCode> {
        let http = self.0.http.clone();
        let text = text.to_string();
        let sent = self.0.run(async move {
            let channel = s::UserId::new(user_id).create_dm_channel(&http).await?;
            // No mention can fire from a safeword message.
            let message = s::builder::CreateMessage::new()
                .content(text)
                .allowed_mentions(s::builder::CreateAllowedMentions::new());
            channel.id.send_message(&http, message).await.map(|_| ())
        });
        match sent {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(dm_code(&roles_runtime::map_serenity_error(&error))),
            None => Err(DmCode::Other),
        }
    }
}

struct Alerts(Net);

impl StaffAlerter for Alerts {
    fn alert(&mut self, code: &'static str) {
        eprintln!("safeword alert: {code}");
        let Some(channel) = self.0.staff_alert_channel.filter(|id| *id != 0) else {
            return;
        };
        let http = self.0.http.clone();
        self.0.handle.spawn(async move {
            let message = s::builder::CreateMessage::new()
                .content(format!("safeword alert: {code}"))
                .allowed_mentions(s::builder::CreateAllowedMentions::new());
            let _ = s::ChannelId::new(channel)
                .send_message(&http, message)
                .await;
        });
    }
}

struct Users(Net);

impl ReactionUsers for Users {
    fn reaction_users(
        &mut self,
        channel_id: u64,
        message_id: u64,
        emoji_id: u64,
        after: Option<u64>,
        limit: u32,
    ) -> Result<Vec<ReactionUser>, &'static str> {
        let http = self.0.http.clone();
        let limit = u8::try_from(limit).unwrap_or(100);
        let page = self.0.run(async move {
            let emoji = s::ReactionType::Custom {
                animated: false,
                id: s::EmojiId::new(emoji_id),
                name: None,
            };
            http.get_reaction_users(
                s::ChannelId::new(channel_id),
                s::MessageId::new(message_id),
                &emoji,
                limit,
                after,
            )
            .await
        });
        match page {
            Some(Ok(users)) => Ok(users
                .into_iter()
                .map(|user| ReactionUser {
                    id: user.id.get(),
                    is_bot: user.bot,
                })
                .collect()),
            _ => Err("reaction_users_failed"),
        }
    }
}

/// What [`SafewordRuntime::new`] needs.
pub struct RuntimeDeps {
    pub command: CommandConfig,
    pub http: Arc<s::Http>,
    pub cache: Arc<s::Cache>,
    pub bot_user_id: u64,
    pub store: SharedStore,
    pub staff_alert_channel: Option<u64>,
    pub protected_role_ids: Vec<u64>,
}

/// The safeword feature's shared state: the configuration, Discord access, the store and the
/// pending `clear` confirmations. The registry hook, the `/safeword` command and the
/// reaction-role safeword check all hold the same one.
pub struct SafewordRuntime {
    deps: RuntimeDeps,
    confirms: Mutex<ConfirmBook>,
}

impl SafewordRuntime {
    pub fn new(deps: RuntimeDeps) -> Self {
        SafewordRuntime {
            deps,
            confirms: Mutex::new(ConfirmBook::new()),
        }
    }

    pub fn command_config(&self) -> &CommandConfig {
        &self.deps.command
    }

    /// The shared store (for the reaction-role safeword check).
    pub fn store(&self) -> &SharedStore {
        &self.deps.store
    }

    /// The collaborators for one guild. Must be called inside a tokio runtime.
    pub fn build_ops(&self, guild: u64) -> Ops {
        let net = Net {
            http: self.deps.http.clone(),
            handle: Handle::current(),
            guild,
            bot_user: self.deps.bot_user_id,
            staff_alert_channel: self.deps.staff_alert_channel,
        };
        Ops {
            members: Box::new(Members(net.clone())),
            facts: Box::new(Facts {
                net: net.clone(),
                cache: self.deps.cache.clone(),
            }),
            dms: Box::new(Dms(net.clone())),
            alerter: Box::new(Alerts(net.clone())),
            store: Box::new(StoreOps(self.deps.store.clone())),
            reactions: Box::new(Users(net.clone())),
            sleeper: Box::new(TokioSleeper::new(net.handle.clone())),
            protected_role_ids: self.deps.protected_role_ids.clone(),
        }
    }

    /// Locks the confirmation book and builds the command environment for the home guild.
    /// Holding the lock for the whole command also keeps two commands from running at once.
    /// Blocking: call it from a blocking thread (`spawn_blocking`).
    pub fn command_env(&self) -> RuntimeEnv<'_> {
        let guild = self.deps.command.home_guild.unwrap_or(0);
        RuntimeEnv {
            config: &self.deps.command,
            now: unix_now(),
            ops: self.build_ops(guild),
            confirms: self
                .confirms
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }
}

/// The command environment: the SW4 [`Env`] over the runtime's real collaborators.
pub struct RuntimeEnv<'a> {
    config: &'a CommandConfig,
    now: i64,
    ops: Ops,
    confirms: MutexGuard<'a, ConfirmBook>,
}

impl Env for RuntimeEnv<'_> {
    fn config(&self) -> &CommandConfig {
        self.config
    }

    fn now(&self) -> i64 {
        self.now
    }

    fn ops(&mut self) -> &mut Ops {
        &mut self.ops
    }

    fn confirms(&mut self) -> &mut ConfirmBook {
        &mut self.confirms
    }
}

impl SafewordContext for Arc<SafewordRuntime> {
    /// Collaborators for the home guild only; any other guild gets none, so nothing is read,
    /// written or sent for it.
    fn safeword_ops(&self, guild_id: u64) -> Option<Ops> {
        match self.deps.command.home_guild {
            Some(home) if home != 0 && home == guild_id => Some(self.build_ops(guild_id)),
            _ => None,
        }
    }

    fn safeword_now(&self) -> i64 {
        unix_now()
    }
}

/// The registry hook. [`SafewordFeature`] implements the registry's `Feature` only for the unit
/// context, so this wrapper adapts it to the bot's real context and data types.
pub struct SafewordHook {
    inner: SafewordFeature<Arc<SafewordRuntime>>,
}

impl SafewordHook {
    pub fn new(runtime: Arc<SafewordRuntime>) -> Self {
        let config = runtime.command_config().safeword.clone();
        SafewordHook {
            inner: SafewordFeature::new(config, runtime),
        }
    }
}

static UNIT: () = ();

impl<X: Sync, D: Sync> Feature<X, D> for SafewordHook {
    fn name(&self) -> &'static str {
        <SafewordFeature<Arc<SafewordRuntime>> as Feature<(), ()>>::name(&self.inner)
    }

    fn enabled(&self) -> bool {
        <SafewordFeature<Arc<SafewordRuntime>> as Feature<(), ()>>::enabled(&self.inner)
    }

    fn needs_core_db(&self) -> bool {
        <SafewordFeature<Arc<SafewordRuntime>> as Feature<(), ()>>::needs_core_db(&self.inner)
    }

    fn jobs_started(&self, wake: JobSignal) {
        <SafewordFeature<Arc<SafewordRuntime>> as Feature<(), ()>>::jobs_started(&self.inner, wake)
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a X,
        event: &'a s::FullEvent,
        _data: &'a D,
        guild: Option<u64>,
        core_db: Option<&'a tokio_rusqlite::Connection>,
    ) -> BoxFuture<'a, ()> {
        <SafewordFeature<Arc<SafewordRuntime>> as Feature<(), ()>>::handle(
            &self.inner,
            &UNIT,
            event,
            &UNIT,
            guild,
            core_db,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(status: u16, api_code: Option<u32>) -> WriteError {
        WriteError {
            status,
            api_code,
            retry_after_ms: None,
        }
    }

    #[test]
    fn write_errors_map_to_the_runtime_codes() {
        assert_eq!(write_code(&err(403, None)), WriteCode::Forbidden);
        assert_eq!(write_code(&err(429, None)), WriteCode::RateLimited);
        assert_eq!(write_code(&err(404, Some(10011))), WriteCode::UnknownRole);
        assert_eq!(write_code(&err(404, Some(10007))), WriteCode::UnknownMember);
        assert_eq!(write_code(&err(500, None)), WriteCode::Other);
        // A call that could not be driven is a plain failure, never a success or a guess.
        assert_eq!(
            write_code(&err(STATUS_WRONG_CONTEXT, None)),
            WriteCode::Other
        );
    }

    #[test]
    fn dm_errors_map_closed_rate_limited_and_other() {
        assert_eq!(dm_code(&err(403, Some(50007))), DmCode::Closed);
        assert_eq!(dm_code(&err(429, None)), DmCode::RateLimited);
        assert_eq!(dm_code(&err(403, Some(50001))), DmCode::Other);
        assert_eq!(dm_code(&err(500, None)), DmCode::Other);
    }

    #[test]
    fn the_shared_store_without_a_usable_path_is_an_error_and_reads_as_active() {
        let store = SharedStore::new(StoreSpec {
            path: None,
            reserved: vec![],
        });
        assert_eq!(
            store.with(|repo| repo.load(1, 2)).map(|_| ()),
            Err("unusable_setting")
        );
        // A store that cannot be read must not look like "no safeword".
        assert_eq!(store.reaction_state(1, 2), SafewordState::Active);
    }

    #[test]
    fn the_shared_store_reads_idle_and_active_members() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("w2b-tests")
            .join("store");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.sqlite").to_string_lossy().into_owned();
        let store = SharedStore::new(StoreSpec {
            path: Some(path),
            reserved: vec![],
        });
        // No row: idle.
        assert_eq!(store.reaction_state(1, 2), SafewordState::Inactive);
        let active = State::Active(Default::default());
        store.with(|repo| repo.store(1, 2, &active, 0, 10)).unwrap();
        assert_eq!(store.reaction_state(1, 2), SafewordState::Active);
        // Another member and another guild are untouched.
        assert_eq!(store.reaction_state(1, 3), SafewordState::Inactive);
        assert_eq!(store.reaction_state(9, 2), SafewordState::Inactive);
        store.with(|repo| repo.clear(1, 2, 1, 11)).unwrap();
        assert_eq!(store.reaction_state(1, 2), SafewordState::Inactive);
    }
}

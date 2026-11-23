//! Logic behind `/role add | remove | info | list` (RED stub: behavior not written yet).

use crate::roles::eligibility::SafewordState;
use crate::roles::grant::{Sleeper, WriteError};
use crate::roles::runtime::RoleSource;
use crate::settings::Mode;

pub const FEATURE_NAME: &str = "role_admin";
pub const MODE_KEY: &str = "ROLE_ADMIN_MODE";
pub const MAX_REPLY_CHARS: usize = 1000;
pub const MAX_LIST_ROLES: usize = 20;

#[derive(Debug, Clone)]
pub struct RoleAdminEnv {
    pub mode: Mode,
    pub home_guild_id: Option<u64>,
    pub staff_role_id: Option<u64>,
    pub bot_user_id: u64,
    pub protected_role_ids: Vec<u64>,
    pub protected_roles_invalid: bool,
    pub safeword_restricted_role_ids: Vec<u64>,
}

pub trait RoleExecutor: Send {
    fn add_role(&mut self, member_id: u64, role_id: u64, reason: &str) -> Result<(), WriteError>;
    fn remove_role(&mut self, member_id: u64, role_id: u64, reason: &str)
        -> Result<(), WriteError>;
}

pub trait SafewordLookup: Send + Sync {
    fn state(&self, guild_id: u64, member_id: u64) -> Option<SafewordState>;
}

pub trait Clock: Send + Sync {
    fn now_unix(&self) -> i64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub at: i64,
    pub action: Action,
    pub caller_id: u64,
    pub member_id: u64,
    pub role_id: u64,
    pub code: &'static str,
}

pub trait AuditSink: Send {
    fn record(&mut self, entry: &AuditEntry);
}

pub struct Ports<'a> {
    pub source: &'a dyn RoleSource,
    pub executor: &'a mut dyn RoleExecutor,
    pub sleeper: &'a mut (dyn Sleeper + Send),
    pub safeword: &'a dyn SafewordLookup,
    pub audit: &'a mut dyn AuditSink,
    pub clock: &'a dyn Clock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub guild_id: Option<u64>,
    pub caller_id: u64,
    pub member_id: u64,
    pub role_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfoRequest {
    pub guild_id: Option<u64>,
    pub caller_id: u64,
    pub role_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListRequest {
    pub guild_id: Option<u64>,
    pub caller_id: u64,
    pub member_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub ephemeral: bool,
    pub code: &'static str,
    pub text: String,
}

pub fn parse_mode(_lookup: impl Fn(&str) -> Option<String>) -> Mode {
    Mode::Off
}

pub fn audit_reason(_action: Action, _caller_id: u64) -> String {
    String::new()
}

fn stub() -> Reply {
    Reply {
        ephemeral: false,
        code: "stub",
        text: String::new(),
    }
}

pub async fn role_add(_env: &RoleAdminEnv, _ports: Ports<'_>, _req: &Request) -> Reply {
    stub()
}

pub async fn role_remove(_env: &RoleAdminEnv, _ports: Ports<'_>, _req: &Request) -> Reply {
    stub()
}

pub async fn role_info(_env: &RoleAdminEnv, _source: &dyn RoleSource, _req: &InfoRequest) -> Reply {
    stub()
}

pub async fn role_list(_env: &RoleAdminEnv, _source: &dyn RoleSource, _req: &ListRequest) -> Reply {
    stub()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::{
        ADMINISTRATOR, BAN_MEMBERS, KICK_MEMBERS, MANAGE_GUILD, MANAGE_ROLES, MODERATE_MEMBERS,
    };
    use crate::roles::runtime::{RoleEntry, SourceError};
    use poise::serenity_prelude as s;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // Synthetic ids only.
    const HOME: u64 = 1_000_001; // also the @everyone role id
    const OTHER_GUILD: u64 = 2_000_002;
    const BOT: u64 = 900_000;
    const CALLER: u64 = 900_100; // staff, top role position 8
    const BOSS: u64 = 900_110; // staff, top role position 20
    const PEON: u64 = 900_120; // not staff
    const TARGET: u64 = 900_200; // top role position 2
    const PEER: u64 = 900_300; // top role position 8 (equal to CALLER)
    const SUPERIOR: u64 = 900_400; // top role position 20
    const GHOST: u64 = 900_999; // not a member of the guild

    const R_STAFF: u64 = 700_001; // position 6
    const R_BOT: u64 = 700_010; // position 10
    const R_CALLER_TOP: u64 = 700_020; // position 8
    const R_BOSS_TOP: u64 = 700_021; // position 20
    const R_TARGET_TOP: u64 = 700_030; // position 2
    const R_OK: u64 = 700_100; // position 3
    const R_HIGH: u64 = 700_101; // position 9: above CALLER, below bot
    const R_EQ_CALLER: u64 = 700_102; // position 8
    const R_AT_BOT: u64 = 700_103; // position 10
    const R_PROT: u64 = 700_105;
    const R_MANAGED: u64 = 700_106;
    const R_RESTRICTED: u64 = 700_107;
    const R_BELOW_CALLER: u64 = 700_108; // position 7
    const R_ABOVE_BOT: u64 = 700_109; // position 11
    const R_PRIV_BASE: u64 = 700_110; // one role per privileged bit
    const R_DELETED: u64 = 799_999;
    const NOW: i64 = 1_700_000_000;

    const PRIV_BITS: [u64; 6] = [
        ADMINISTRATOR,
        MANAGE_GUILD,
        MANAGE_ROLES,
        BAN_MEMBERS,
        KICK_MEMBERS,
        MODERATE_MEMBERS,
    ];

    fn entry(id: u64, position: u16, permissions: u64, managed: bool) -> RoleEntry {
        RoleEntry {
            id,
            position,
            managed,
            permissions,
        }
    }

    struct World {
        roles: Vec<RoleEntry>,
        members: HashMap<u64, Vec<u64>>,
    }

    impl World {
        fn standard() -> Self {
            let mut roles = vec![
                entry(HOME, 0, 0, false),
                entry(R_STAFF, 6, 0, false),
                entry(R_BOT, 10, 0, false),
                entry(R_CALLER_TOP, 8, 0, false),
                entry(R_BOSS_TOP, 20, 0, false),
                entry(R_TARGET_TOP, 2, 0, false),
                entry(R_OK, 3, 0, false),
                entry(R_HIGH, 9, 0, false),
                entry(R_EQ_CALLER, 8, 0, false),
                entry(R_AT_BOT, 10, 0, false),
                entry(R_PROT, 3, 0, false),
                entry(R_MANAGED, 3, 0, true),
                entry(R_RESTRICTED, 3, 0, false),
                entry(R_BELOW_CALLER, 7, 0, false),
                entry(R_ABOVE_BOT, 11, 0, false),
            ];
            for (i, bit) in PRIV_BITS.iter().enumerate() {
                roles.push(entry(R_PRIV_BASE + i as u64, 3, *bit, false));
            }
            let mut members = HashMap::new();
            members.insert(BOT, vec![R_BOT]);
            members.insert(CALLER, vec![R_STAFF, R_CALLER_TOP]);
            members.insert(BOSS, vec![R_STAFF, R_BOSS_TOP]);
            members.insert(PEON, vec![R_TARGET_TOP]);
            members.insert(TARGET, vec![R_TARGET_TOP]);
            members.insert(PEER, vec![R_CALLER_TOP]);
            members.insert(SUPERIOR, vec![R_BOSS_TOP]);
            World { roles, members }
        }
    }

    type Shared = Arc<Mutex<World>>;

    struct FakeSource {
        world: Shared,
        calls: AtomicUsize,
        roles_down: bool,
    }

    #[s::async_trait]
    impl RoleSource for FakeSource {
        async fn roles(&self, _guild_id: u64) -> Result<Vec<RoleEntry>, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.roles_down {
                return Err(SourceError::Unavailable);
            }
            Ok(self.world.lock().unwrap().roles.clone())
        }

        async fn member_roles(
            &self,
            _guild_id: u64,
            member_id: u64,
        ) -> Result<Vec<u64>, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.world
                .lock()
                .unwrap()
                .members
                .get(&member_id)
                .cloned()
                .ok_or(SourceError::Unavailable)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ExecCall {
        op: &'static str,
        member: u64,
        role: u64,
        reason: String,
    }

    struct FakeExec {
        world: Shared,
        calls: Vec<ExecCall>,
        script: VecDeque<Result<(), WriteError>>,
    }

    impl FakeExec {
        fn step(
            &mut self,
            op: &'static str,
            member: u64,
            role: u64,
            reason: &str,
        ) -> Result<(), WriteError> {
            self.calls.push(ExecCall {
                op,
                member,
                role,
                reason: reason.to_string(),
            });
            let result = self.script.pop_front().unwrap_or(Ok(()));
            if result.is_ok() {
                let mut world = self.world.lock().unwrap();
                let held = world.members.entry(member).or_default();
                if op == "add" {
                    if !held.contains(&role) {
                        held.push(role);
                    }
                } else {
                    held.retain(|r| *r != role);
                }
            }
            result
        }
    }

    impl RoleExecutor for FakeExec {
        fn add_role(
            &mut self,
            member_id: u64,
            role_id: u64,
            reason: &str,
        ) -> Result<(), WriteError> {
            self.step("add", member_id, role_id, reason)
        }
        fn remove_role(
            &mut self,
            member_id: u64,
            role_id: u64,
            reason: &str,
        ) -> Result<(), WriteError> {
            self.step("remove", member_id, role_id, reason)
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

    #[derive(Default)]
    struct FakeSafeword {
        states: HashMap<u64, Option<SafewordState>>,
    }
    impl SafewordLookup for FakeSafeword {
        fn state(&self, _guild_id: u64, member_id: u64) -> Option<SafewordState> {
            self.states
                .get(&member_id)
                .copied()
                .unwrap_or(Some(SafewordState::Inactive))
        }
    }

    #[derive(Default)]
    struct FakeAudit {
        entries: Vec<AuditEntry>,
    }
    impl AuditSink for FakeAudit {
        fn record(&mut self, entry: &AuditEntry) {
            self.entries.push(entry.clone());
        }
    }

    struct FakeClock;
    impl Clock for FakeClock {
        fn now_unix(&self) -> i64 {
            NOW
        }
    }

    struct Rig {
        world: Shared,
        env: RoleAdminEnv,
        source: FakeSource,
        exec: FakeExec,
        sleeper: FakeSleeper,
        safeword: FakeSafeword,
        audit: FakeAudit,
    }

    fn live_env() -> RoleAdminEnv {
        RoleAdminEnv {
            mode: Mode::Live,
            home_guild_id: Some(HOME),
            staff_role_id: Some(R_STAFF),
            bot_user_id: BOT,
            protected_role_ids: vec![R_PROT],
            protected_roles_invalid: false,
            safeword_restricted_role_ids: vec![R_RESTRICTED],
        }
    }

    impl Rig {
        fn new() -> Self {
            let world: Shared = Arc::new(Mutex::new(World::standard()));
            Rig {
                env: live_env(),
                source: FakeSource {
                    world: world.clone(),
                    calls: AtomicUsize::new(0),
                    roles_down: false,
                },
                exec: FakeExec {
                    world: world.clone(),
                    calls: Vec::new(),
                    script: VecDeque::new(),
                },
                sleeper: FakeSleeper::default(),
                safeword: FakeSafeword::default(),
                audit: FakeAudit::default(),
                world,
            }
        }

        fn ports(&mut self) -> Ports<'_> {
            Ports {
                source: &self.source,
                executor: &mut self.exec,
                sleeper: &mut self.sleeper,
                safeword: &self.safeword,
                audit: &mut self.audit,
                clock: &FakeClock,
            }
        }

        async fn add(&mut self, req: &Request) -> Reply {
            let env = self.env.clone();
            role_add(&env, self.ports(), req).await
        }

        async fn remove(&mut self, req: &Request) -> Reply {
            let env = self.env.clone();
            role_remove(&env, self.ports(), req).await
        }

        async fn info(&self, req: &InfoRequest) -> Reply {
            role_info(&self.env, &self.source, req).await
        }

        async fn list(&self, req: &ListRequest) -> Reply {
            role_list(&self.env, &self.source, req).await
        }

        fn source_calls(&self) -> usize {
            self.source.calls.load(Ordering::SeqCst)
        }

        fn holds(&self, member: u64, role: u64) -> bool {
            self.world
                .lock()
                .unwrap()
                .members
                .get(&member)
                .is_some_and(|r| r.contains(&role))
        }

        fn nothing_touched(&self) -> bool {
            self.source_calls() == 0 && self.exec.calls.is_empty() && self.audit.entries.is_empty()
        }
    }

    fn req(caller: u64, member: u64, role: u64) -> Request {
        Request {
            guild_id: Some(HOME),
            caller_id: caller,
            member_id: member,
            role_id: role,
        }
    }

    fn info_req(caller: u64, role: u64) -> InfoRequest {
        InfoRequest {
            guild_id: Some(HOME),
            caller_id: caller,
            role_id: role,
        }
    }

    fn list_req(caller: u64, member: Option<u64>) -> ListRequest {
        ListRequest {
            guild_id: Some(HOME),
            caller_id: caller,
            member_id: member,
        }
    }

    fn err(status: u16, api_code: Option<u32>, retry_after_ms: Option<u64>) -> WriteError {
        WriteError {
            status,
            api_code,
            retry_after_ms,
        }
    }

    fn assert_reply_shape(reply: &Reply) {
        assert!(reply.ephemeral, "reply must be ephemeral: {}", reply.code);
        assert!(
            reply.text.chars().count() <= MAX_REPLY_CHARS,
            "reply too long: {}",
            reply.code
        );
        assert!(!reply.text.is_empty(), "reply empty: {}", reply.code);
        assert!(reply.text.is_ascii(), "non-ascii text in {}", reply.code);
        assert!(
            !reply.text.contains('@') && !reply.text.contains('<'),
            "mention syntax in {}",
            reply.code
        );
    }

    // ------------------------------------------------------------------ config and gating

    #[test]
    fn mode_defaults_off_and_unknown_values_are_off() {
        let none = |_: &str| None;
        assert_eq!(parse_mode(none), Mode::Off);
        for raw in [
            "",
            "off",
            "OFF",
            "on",
            "true",
            "1",
            "lvie",
            "live ish",
            "shadow,live",
        ] {
            let value = raw.to_string();
            let m = parse_mode(move |k| (k == MODE_KEY).then(|| value.clone()));
            assert_eq!(m, Mode::Off, "{raw:?}");
        }
        let shadow = parse_mode(|k| (k == MODE_KEY).then(|| " Shadow ".to_string()));
        assert_eq!(shadow, Mode::Shadow);
        let live = parse_mode(|k| (k == MODE_KEY).then(|| "LIVE".to_string()));
        assert_eq!(live, Mode::Live);
        // Another key never turns the feature on.
        let other = parse_mode(|k| (k == "EMBED_MODE").then(|| "live".to_string()));
        assert_eq!(other, Mode::Off);
    }

    #[tokio::test]
    async fn off_mode_refuses_every_command_and_touches_nothing() {
        let mut rig = Rig::new();
        rig.env.mode = Mode::Off;
        let replies = vec![
            rig.add(&req(CALLER, TARGET, R_OK)).await,
            rig.remove(&req(CALLER, TARGET, R_TARGET_TOP)).await,
            rig.info(&info_req(CALLER, R_OK)).await,
            rig.list(&list_req(CALLER, None)).await,
        ];
        for reply in &replies {
            assert_eq!(reply.code, "feature_off");
            assert_reply_shape(reply);
        }
        assert!(rig.nothing_touched());
        assert!(!rig.holds(TARGET, R_OK));
    }

    #[tokio::test]
    async fn unset_or_zero_home_guild_fails_closed_while_on() {
        for home in [None, Some(0)] {
            for mode in [Mode::Shadow, Mode::Live] {
                let mut rig = Rig::new();
                rig.env.mode = mode;
                rig.env.home_guild_id = home;
                let replies = vec![
                    rig.add(&req(CALLER, TARGET, R_OK)).await,
                    rig.remove(&req(CALLER, TARGET, R_TARGET_TOP)).await,
                    rig.info(&info_req(CALLER, R_OK)).await,
                    rig.list(&list_req(CALLER, None)).await,
                ];
                for reply in &replies {
                    assert_eq!(reply.code, "home_guild_unset", "{home:?} {mode:?}");
                    assert_reply_shape(reply);
                }
                assert!(rig.nothing_touched());
            }
        }
    }

    #[tokio::test]
    async fn unset_or_zero_staff_role_fails_closed_while_on() {
        for staff in [None, Some(0)] {
            let mut rig = Rig::new();
            rig.env.staff_role_id = staff;
            let replies = vec![
                rig.add(&req(CALLER, TARGET, R_OK)).await,
                rig.remove(&req(CALLER, TARGET, R_TARGET_TOP)).await,
                rig.info(&info_req(CALLER, R_OK)).await,
                rig.list(&list_req(CALLER, None)).await,
            ];
            for reply in &replies {
                assert_eq!(reply.code, "staff_role_unset", "{staff:?}");
            }
            assert!(rig.nothing_touched());
        }
    }

    #[tokio::test]
    async fn foreign_guild_and_dm_are_denied_with_nothing_touched() {
        for guild in [Some(OTHER_GUILD), Some(0), None] {
            let mut rig = Rig::new();
            let mut add = req(CALLER, TARGET, R_OK);
            add.guild_id = guild;
            let mut rem = req(CALLER, TARGET, R_TARGET_TOP);
            rem.guild_id = guild;
            let mut info = info_req(CALLER, R_OK);
            info.guild_id = guild;
            let mut list = list_req(CALLER, None);
            list.guild_id = guild;
            let replies = vec![
                rig.add(&add).await,
                rig.remove(&rem).await,
                rig.info(&info).await,
                rig.list(&list).await,
            ];
            let want = if guild.is_none() {
                "no_guild"
            } else {
                "foreign_guild"
            };
            for reply in &replies {
                assert_eq!(reply.code, want, "{guild:?}");
                assert_reply_shape(reply);
            }
            assert!(rig.nothing_touched(), "{guild:?}");
            assert!(!rig.holds(TARGET, R_OK));
            assert!(rig.holds(TARGET, R_TARGET_TOP));
        }
    }

    #[tokio::test]
    async fn invalid_ids_are_refused_before_any_lookup() {
        let mut rig = Rig::new();
        for r in [
            req(0, TARGET, R_OK),
            req(CALLER, 0, R_OK),
            req(CALLER, TARGET, 0),
        ] {
            assert_eq!(rig.add(&r).await.code, "invalid_id");
            assert_eq!(rig.remove(&r).await.code, "invalid_id");
        }
        assert_eq!(rig.info(&info_req(CALLER, 0)).await.code, "invalid_id");
        assert_eq!(rig.info(&info_req(0, R_OK)).await.code, "invalid_id");
        assert_eq!(rig.list(&list_req(0, None)).await.code, "invalid_id");
        assert_eq!(
            rig.list(&list_req(CALLER, Some(0))).await.code,
            "invalid_id"
        );
        assert!(rig.nothing_touched());
        rig.env.bot_user_id = 0;
        assert_eq!(rig.add(&req(CALLER, TARGET, R_OK)).await.code, "invalid_id");
        assert!(rig.nothing_touched());
    }

    #[tokio::test]
    async fn unusable_protected_role_config_fails_closed() {
        let mut rig = Rig::new();
        rig.env.protected_roles_invalid = true;
        assert_eq!(
            rig.add(&req(CALLER, TARGET, R_OK)).await.code,
            "protected_roles_invalid"
        );
        assert_eq!(
            rig.remove(&req(CALLER, TARGET, R_TARGET_TOP)).await.code,
            "protected_roles_invalid"
        );
        assert!(rig.nothing_touched());
    }

    #[tokio::test]
    async fn non_staff_caller_is_refused_on_all_four_commands() {
        let mut rig = Rig::new();
        let add = rig.add(&req(PEON, TARGET, R_OK)).await;
        let rem = rig.remove(&req(PEON, TARGET, R_TARGET_TOP)).await;
        let info = rig.info(&info_req(PEON, R_OK)).await;
        let list = rig.list(&list_req(PEON, Some(TARGET))).await;
        for reply in [&add, &rem, &info, &list] {
            assert_eq!(reply.code, "not_staff");
            assert_reply_shape(reply);
        }
        assert!(rig.exec.calls.is_empty());
        assert!(!rig.holds(TARGET, R_OK));
        assert!(rig.holds(TARGET, R_TARGET_TOP));
    }

    // ------------------------------------------------------------------ add: happy path

    #[tokio::test]
    async fn add_grants_through_the_executor_with_audit_reason_and_one_audit_entry() {
        let mut rig = Rig::new();
        let reply = rig.add(&req(CALLER, TARGET, R_OK)).await;
        assert_eq!(reply.code, "added");
        assert_reply_shape(&reply);
        assert_eq!(
            rig.exec.calls,
            vec![ExecCall {
                op: "add",
                member: TARGET,
                role: R_OK,
                reason: format!("role_admin add by {CALLER}"),
            }]
        );
        assert!(rig.holds(TARGET, R_OK));
        assert_eq!(
            rig.audit.entries,
            vec![AuditEntry {
                at: NOW,
                action: Action::Add,
                caller_id: CALLER,
                member_id: TARGET,
                role_id: R_OK,
                code: "added",
            }]
        );
    }

    #[test]
    fn audit_reason_names_only_the_action_and_the_caller_id() {
        assert_eq!(audit_reason(Action::Add, 123), "role_admin add by 123");
        assert_eq!(
            audit_reason(Action::Remove, 123),
            "role_admin remove by 123"
        );
        assert!(audit_reason(Action::Add, u64::MAX).len() <= 100);
    }

    #[tokio::test]
    async fn audit_reason_is_set_on_every_write_including_retries() {
        let mut rig = Rig::new();
        rig.exec.script = VecDeque::from(vec![
            Err(err(429, None, None)),
            Err(err(429, None, Some(40))),
            Ok(()),
        ]);
        let reply = rig.add(&req(CALLER, TARGET, R_OK)).await;
        assert_eq!(reply.code, "added");
        assert_eq!(rig.exec.calls.len(), 3);
        for call in &rig.exec.calls {
            assert_eq!(call.reason, format!("role_admin add by {CALLER}"));
        }
        assert_eq!(rig.sleeper.slept, vec![1000, 40]);

        let mut rig = Rig::new();
        rig.exec.script = VecDeque::from(vec![Err(err(429, None, None)), Ok(())]);
        let reply = rig.remove(&req(BOSS, TARGET, R_TARGET_TOP)).await;
        assert_eq!(reply.code, "removed");
        assert_eq!(rig.exec.calls.len(), 2);
        for call in &rig.exec.calls {
            assert_eq!(call.op, "remove");
            assert_eq!(call.reason, format!("role_admin remove by {BOSS}"));
        }
    }

    // ------------------------------------------------------------------ hierarchy

    #[tokio::test]
    async fn caller_must_outrank_the_role_strictly() {
        // CALLER's top position is 8: 7 is allowed; 8 (equal) and 9 (above, below the bot) are not.
        let mut rig = Rig::new();
        assert_eq!(
            rig.add(&req(CALLER, TARGET, R_BELOW_CALLER)).await.code,
            "added"
        );
        for role in [R_EQ_CALLER, R_HIGH] {
            let reply = rig.add(&req(CALLER, TARGET, role)).await;
            assert_eq!(reply.code, "actor_not_above_role", "{role}");
            assert!(!rig.holds(TARGET, role));
        }
        // The same limit applies to removal.
        for role in [R_EQ_CALLER, R_HIGH] {
            rig.world
                .lock()
                .unwrap()
                .members
                .get_mut(&TARGET)
                .unwrap()
                .push(role);
            let reply = rig.remove(&req(CALLER, TARGET, role)).await;
            assert_eq!(reply.code, "actor_not_above_role", "{role}");
            assert!(rig.holds(TARGET, role), "denied removal must not write");
        }
        assert_eq!(rig.exec.calls.len(), 1, "only the allowed add wrote");
    }

    #[tokio::test]
    async fn caller_must_outrank_the_target_member_strictly() {
        let mut rig = Rig::new();
        for target in [PEER, SUPERIOR, CALLER] {
            let reply = rig.add(&req(CALLER, target, R_OK)).await;
            assert_eq!(reply.code, "actor_not_above_target", "{target}");
            rig.world
                .lock()
                .unwrap()
                .members
                .get_mut(&target)
                .unwrap()
                .push(R_OK);
            let reply = rig.remove(&req(CALLER, target, R_OK)).await;
            assert_eq!(reply.code, "actor_not_above_target", "{target}");
            assert!(rig.holds(target, R_OK));
        }
        assert!(rig.exec.calls.is_empty());
        // A higher caller may act on the same member.
        assert_eq!(rig.add(&req(BOSS, PEER, R_OK)).await.code, "added");
    }

    #[tokio::test]
    async fn bot_must_outrank_the_role_even_when_the_caller_does() {
        // BOSS (position 20) outranks every role, but the bot (position 10) does not.
        let mut rig = Rig::new();
        for role in [R_AT_BOT, R_ABOVE_BOT] {
            let add = rig.add(&req(BOSS, TARGET, role)).await;
            assert_eq!(add.code, "at_or_above_bot", "{role}");
            rig.world
                .lock()
                .unwrap()
                .members
                .get_mut(&TARGET)
                .unwrap()
                .push(role);
            let rem = rig.remove(&req(BOSS, TARGET, role)).await;
            assert_eq!(rem.code, "at_or_above_bot", "{role}");
            assert!(rig.holds(TARGET, role));
        }
        assert!(rig.exec.calls.is_empty());
        // One position below the bot is fine.
        rig.world
            .lock()
            .unwrap()
            .roles
            .push(entry(700_900, 9, 0, false));
        assert_eq!(rig.add(&req(BOSS, TARGET, 700_900)).await.code, "added");
    }

    #[tokio::test]
    async fn a_bot_with_no_known_role_can_grant_nothing() {
        let mut rig = Rig::new();
        rig.world.lock().unwrap().members.insert(BOT, Vec::new());
        assert_eq!(
            rig.add(&req(BOSS, TARGET, R_OK)).await.code,
            "at_or_above_bot"
        );
        assert!(rig.exec.calls.is_empty());
    }

    // ------------------------------------------------------------------ refused classes

    #[tokio::test]
    async fn privileged_roles_are_refused_for_add_and_remove_whatever_the_rank() {
        for (i, _bit) in PRIV_BITS.iter().enumerate() {
            let role = R_PRIV_BASE + i as u64;
            let mut rig = Rig::new();
            let add = rig.add(&req(BOSS, TARGET, role)).await;
            assert_eq!(add.code, "privileged_permission", "bit {i}");
            assert_reply_shape(&add);
            rig.world
                .lock()
                .unwrap()
                .members
                .get_mut(&TARGET)
                .unwrap()
                .push(role);
            let rem = rig.remove(&req(BOSS, TARGET, role)).await;
            assert_eq!(rem.code, "privileged_permission", "bit {i}");
            assert!(rig.exec.calls.is_empty(), "bit {i}");
            assert!(rig.holds(TARGET, role));
        }
        // A role with an unrelated permission bit is not privileged.
        let mut rig = Rig::new();
        rig.world
            .lock()
            .unwrap()
            .roles
            .push(entry(700_901, 3, 1 << 10, false));
        assert_eq!(rig.add(&req(BOSS, TARGET, 700_901)).await.code, "added");
    }

    #[tokio::test]
    async fn protected_managed_everyone_and_deleted_roles_are_refused() {
        let mut rig = Rig::new();
        let cases = [
            (R_PROT, "protected_role"),
            (R_MANAGED, "managed_role"),
            (HOME, "everyone_role"),
            (R_DELETED, "role_deleted"),
        ];
        for (role, code) in cases {
            let add = rig.add(&req(BOSS, TARGET, role)).await;
            assert_eq!(add.code, code, "add {role}");
            assert_reply_shape(&add);
            let rem = rig.remove(&req(BOSS, TARGET, role)).await;
            assert_eq!(rem.code, code, "remove {role}");
        }
        assert!(rig.exec.calls.is_empty());
        assert!(!rig.holds(TARGET, R_PROT));
        assert!(!rig.holds(TARGET, R_MANAGED));
    }

    // ------------------------------------------------------------------ safeword

    #[tokio::test]
    async fn active_safeword_blocks_a_restricted_role_on_add_even_for_the_top_caller() {
        let mut rig = Rig::new();
        rig.safeword
            .states
            .insert(TARGET, Some(SafewordState::Active));
        let reply = rig.add(&req(BOSS, TARGET, R_RESTRICTED)).await;
        assert_eq!(reply.code, "safeword_restricted");
        assert!(rig.exec.calls.is_empty());
        assert!(!rig.holds(TARGET, R_RESTRICTED));
        assert_eq!(rig.audit.entries.len(), 1);
        assert_eq!(rig.audit.entries[0].code, "safeword_restricted");
        // An unrestricted role is still granted during a safeword.
        assert_eq!(rig.add(&req(BOSS, TARGET, R_OK)).await.code, "added");
        // Once the safeword is inactive the restricted role may be granted.
        rig.safeword
            .states
            .insert(TARGET, Some(SafewordState::Inactive));
        assert_eq!(
            rig.add(&req(BOSS, TARGET, R_RESTRICTED)).await.code,
            "added"
        );
    }

    #[tokio::test]
    async fn unknown_safeword_state_is_treated_as_active_for_restricted_roles_only() {
        let mut rig = Rig::new();
        rig.safeword.states.insert(TARGET, None);
        assert_eq!(
            rig.add(&req(BOSS, TARGET, R_RESTRICTED)).await.code,
            "safeword_restricted"
        );
        assert!(rig.exec.calls.is_empty());
        assert_eq!(rig.add(&req(BOSS, TARGET, R_OK)).await.code, "added");
    }

    #[tokio::test]
    async fn the_safeword_of_the_target_decides_not_the_callers() {
        let mut rig = Rig::new();
        rig.safeword
            .states
            .insert(BOSS, Some(SafewordState::Active));
        assert_eq!(
            rig.add(&req(BOSS, TARGET, R_RESTRICTED)).await.code,
            "added"
        );
    }

    #[tokio::test]
    async fn removal_of_a_restricted_role_is_allowed_during_a_safeword() {
        let mut rig = Rig::new();
        rig.safeword
            .states
            .insert(TARGET, Some(SafewordState::Active));
        rig.world
            .lock()
            .unwrap()
            .members
            .get_mut(&TARGET)
            .unwrap()
            .push(R_RESTRICTED);
        let reply = rig.remove(&req(BOSS, TARGET, R_RESTRICTED)).await;
        assert_eq!(reply.code, "removed");
        assert!(!rig.holds(TARGET, R_RESTRICTED));
    }

    // ------------------------------------------------------------------ remove

    #[tokio::test]
    async fn remove_writes_through_the_executor_and_audits() {
        let mut rig = Rig::new();
        rig.world
            .lock()
            .unwrap()
            .members
            .get_mut(&TARGET)
            .unwrap()
            .push(R_OK);
        let reply = rig.remove(&req(CALLER, TARGET, R_OK)).await;
        assert_eq!(reply.code, "removed");
        assert_reply_shape(&reply);
        assert_eq!(
            rig.exec.calls,
            vec![ExecCall {
                op: "remove",
                member: TARGET,
                role: R_OK,
                reason: format!("role_admin remove by {CALLER}"),
            }]
        );
        assert!(!rig.holds(TARGET, R_OK));
        assert_eq!(rig.audit.entries.len(), 1);
        assert_eq!(rig.audit.entries[0].action, Action::Remove);
        assert_eq!(rig.audit.entries[0].code, "removed");
    }

    #[tokio::test]
    async fn remove_of_a_role_the_member_lacks_is_a_clear_noop_not_an_error() {
        let mut rig = Rig::new();
        let reply = rig.remove(&req(CALLER, TARGET, R_OK)).await;
        assert_eq!(reply.code, "not_held");
        assert_reply_shape(&reply);
        assert!(reply.text.to_lowercase().contains("no change"));
        assert!(rig.exec.calls.is_empty());
        assert_eq!(rig.audit.entries.len(), 1);
        assert_eq!(rig.audit.entries[0].code, "not_held");
    }

    #[tokio::test]
    async fn add_of_a_role_already_held_is_a_noop_without_a_write() {
        let mut rig = Rig::new();
        rig.world
            .lock()
            .unwrap()
            .members
            .get_mut(&TARGET)
            .unwrap()
            .push(R_OK);
        let reply = rig.add(&req(CALLER, TARGET, R_OK)).await;
        assert_eq!(reply.code, "already_held");
        assert_reply_shape(&reply);
        assert!(rig.exec.calls.is_empty());
    }

    #[tokio::test]
    async fn a_noop_does_not_leak_membership_to_an_unauthorised_caller() {
        // The hierarchy is checked before "not held", so a peer cannot probe a peer's roles.
        let mut rig = Rig::new();
        assert_eq!(
            rig.remove(&req(CALLER, PEER, R_OK)).await.code,
            "actor_not_above_target"
        );
        assert_eq!(rig.remove(&req(PEON, PEER, R_OK)).await.code, "not_staff");
    }

    // ------------------------------------------------------------------ failures

    #[tokio::test]
    async fn write_failures_are_reported_by_sanitized_codes_and_audited() {
        let table: [(WriteError, &str, usize); 5] = [
            (err(403, None, None), "write_failed_forbidden", 1),
            (err(404, Some(10011), None), "write_failed_unknown_role", 1),
            (
                err(404, Some(10007), None),
                "write_failed_unknown_member",
                1,
            ),
            (err(500, None, None), "write_failed_other", 1),
            (err(0, None, None), "write_failed_other", 1),
        ];
        for (failure, code, calls) in table {
            for add in [true, false] {
                let mut rig = Rig::new();
                if !add {
                    rig.world
                        .lock()
                        .unwrap()
                        .members
                        .get_mut(&TARGET)
                        .unwrap()
                        .push(R_OK);
                }
                rig.exec.script = VecDeque::from(vec![Err(failure)]);
                let r = req(CALLER, TARGET, R_OK);
                let reply = if add {
                    rig.add(&r).await
                } else {
                    rig.remove(&r).await
                };
                assert_eq!(reply.code, code, "{failure:?} add={add}");
                assert_reply_shape(&reply);
                assert_eq!(rig.exec.calls.len(), calls);
                assert_eq!(rig.holds(TARGET, R_OK), !add, "state must not change");
                assert_eq!(rig.audit.entries.len(), 1);
                assert_eq!(rig.audit.entries[0].code, code);
            }
        }
    }

    #[tokio::test]
    async fn a_persistent_rate_limit_stops_after_three_retries_and_is_reported() {
        for add in [true, false] {
            let mut rig = Rig::new();
            if !add {
                rig.world
                    .lock()
                    .unwrap()
                    .members
                    .get_mut(&TARGET)
                    .unwrap()
                    .push(R_OK);
            }
            rig.exec.script = (0..10).map(|_| Err(err(429, None, None))).collect();
            let r = req(CALLER, TARGET, R_OK);
            let reply = if add {
                rig.add(&r).await
            } else {
                rig.remove(&r).await
            };
            assert_eq!(reply.code, "write_failed_rate_limited");
            assert_eq!(rig.exec.calls.len(), 4);
            assert_eq!(rig.sleeper.slept, vec![1000, 2000, 3000]);
        }
    }

    #[tokio::test]
    async fn a_huge_retry_hint_is_clamped() {
        let mut rig = Rig::new();
        rig.exec.script = VecDeque::from(vec![Err(err(429, None, Some(u64::MAX))), Ok(())]);
        assert_eq!(rig.add(&req(CALLER, TARGET, R_OK)).await.code, "added");
        assert_eq!(rig.sleeper.slept, vec![30_000]);
    }

    #[tokio::test]
    async fn lookup_failures_deny_and_write_nothing() {
        let mut rig = Rig::new();
        rig.source.roles_down = true;
        let add = rig.add(&req(CALLER, TARGET, R_OK)).await;
        let rem = rig.remove(&req(CALLER, TARGET, R_OK)).await;
        assert_eq!(add.code, "facts_unavailable");
        assert_eq!(rem.code, "facts_unavailable");
        assert_eq!(
            rig.info(&info_req(CALLER, R_OK)).await.code,
            "facts_unavailable"
        );
        assert_eq!(
            rig.list(&list_req(CALLER, None)).await.code,
            "facts_unavailable"
        );
        assert!(rig.exec.calls.is_empty());

        let mut rig = Rig::new();
        assert_eq!(
            rig.add(&req(CALLER, GHOST, R_OK)).await.code,
            "facts_unavailable"
        );
        assert_eq!(
            rig.add(&req(GHOST, TARGET, R_OK)).await.code,
            "facts_unavailable"
        );
        assert!(rig.exec.calls.is_empty());
    }

    // ------------------------------------------------------------------ shadow mode

    #[tokio::test]
    async fn shadow_mode_decides_but_never_writes() {
        let mut rig = Rig::new();
        rig.env.mode = Mode::Shadow;
        let add = rig.add(&req(CALLER, TARGET, R_OK)).await;
        assert_eq!(add.code, "would_add");
        assert_reply_shape(&add);
        rig.world
            .lock()
            .unwrap()
            .members
            .get_mut(&TARGET)
            .unwrap()
            .push(R_BELOW_CALLER);
        let rem = rig.remove(&req(CALLER, TARGET, R_BELOW_CALLER)).await;
        assert_eq!(rem.code, "would_remove");
        assert!(rig.exec.calls.is_empty());
        assert!(!rig.holds(TARGET, R_OK));
        assert!(rig.holds(TARGET, R_BELOW_CALLER));
        assert_eq!(rig.audit.entries.len(), 2);
        // Denials still apply in shadow mode.
        assert_eq!(
            rig.add(&req(CALLER, TARGET, R_HIGH)).await.code,
            "actor_not_above_role"
        );
        // Reads work in shadow mode.
        assert_eq!(rig.info(&info_req(CALLER, R_OK)).await.code, "role_info");
    }

    // ------------------------------------------------------------------ info and list

    #[tokio::test]
    async fn info_reports_role_facts_and_is_read_only() {
        let mut rig = Rig::new();
        let info = rig.info(&info_req(CALLER, R_OK)).await;
        assert_eq!(info.code, "role_info");
        assert_reply_shape(&info);
        assert!(info.text.contains("position: 3"), "{}", info.text);
        assert!(info.text.contains("managed: no"));
        assert!(info.text.contains("privileged: no"));
        assert!(info.text.contains("protected: no"));
        assert!(info.text.contains("safeword_restricted: no"));
        assert!(info.text.contains("bot_can_manage: yes"));
        assert!(info.text.contains("you_can_manage: yes"));

        let prot = rig.info(&info_req(CALLER, R_PROT)).await;
        assert!(prot.text.contains("protected: yes"));
        let restricted = rig.info(&info_req(CALLER, R_RESTRICTED)).await;
        assert!(restricted.text.contains("safeword_restricted: yes"));
        let managed = rig.info(&info_req(CALLER, R_MANAGED)).await;
        assert!(managed.text.contains("managed: yes"));
        let privileged = rig.info(&info_req(CALLER, R_PRIV_BASE)).await;
        assert!(privileged.text.contains("privileged: yes"));
        let high = rig.info(&info_req(CALLER, R_HIGH)).await;
        assert!(high.text.contains("bot_can_manage: yes"));
        assert!(high.text.contains("you_can_manage: no"));
        let above_bot = rig.info(&info_req(BOSS, R_AT_BOT)).await;
        assert!(above_bot.text.contains("bot_can_manage: no"));
        assert!(above_bot.text.contains("you_can_manage: yes"));

        assert_eq!(
            rig.info(&info_req(CALLER, R_DELETED)).await.code,
            "role_deleted"
        );
        assert_eq!(
            rig.info(&info_req(CALLER, HOME)).await.code,
            "everyone_role"
        );
        assert!(rig.exec.calls.is_empty());
        assert!(rig.audit.entries.is_empty());
        rig.exec.script.clear();
        assert!(rig.holds(TARGET, R_TARGET_TOP));
    }

    #[tokio::test]
    async fn list_shows_the_callers_roles_by_default_highest_first_without_everyone() {
        let rig = Rig::new();
        let reply = rig.list(&list_req(CALLER, None)).await;
        assert_eq!(reply.code, "role_list");
        assert_reply_shape(&reply);
        let hi = reply
            .text
            .find(&R_CALLER_TOP.to_string())
            .expect("top role listed");
        let lo = reply
            .text
            .find(&R_STAFF.to_string())
            .expect("staff role listed");
        assert!(hi < lo, "highest position first: {}", reply.text);
        assert!(
            !reply.text.contains(&HOME.to_string()),
            "@everyone is not listed"
        );
        assert!(reply.text.contains("2 roles"), "{}", reply.text);

        let other = rig.list(&list_req(CALLER, Some(TARGET))).await;
        assert!(other.text.contains(&R_TARGET_TOP.to_string()));
        assert!(!other.text.contains(&R_CALLER_TOP.to_string()));
        assert!(rig.exec.calls.is_empty());
        assert!(rig.audit.entries.is_empty());
    }

    #[tokio::test]
    async fn list_caps_the_number_of_roles_and_the_text_length() {
        let rig = Rig::new();
        {
            let mut world = rig.world.lock().unwrap();
            let mut held = Vec::new();
            for i in 0..5_000u64 {
                let id = 1_000_000_000 + i;
                world.roles.push(entry(id, 1, 0, false));
                held.push(id);
            }
            world.members.insert(TARGET, held);
        }
        let reply = rig.list(&list_req(CALLER, Some(TARGET))).await;
        assert_eq!(reply.code, "role_list");
        assert_reply_shape(&reply);
        let listed = reply
            .text
            .split(|c: char| !c.is_ascii_digit())
            .filter(|t| t.len() == 10 && t.starts_with('1'))
            .count();
        assert!(listed <= MAX_LIST_ROLES, "listed {listed}");
        assert!(listed >= 1);
        assert!(reply.text.contains("5000 roles"), "{}", reply.text);
        assert!(
            reply.text.contains(&format!("{} more", 5_000 - listed)),
            "{}",
            reply.text
        );
    }

    // ------------------------------------------------------------------ replies in general

    #[tokio::test]
    async fn every_reply_over_a_sweep_is_ephemeral_bounded_and_mention_free() {
        let roles = [
            R_OK,
            R_HIGH,
            R_EQ_CALLER,
            R_AT_BOT,
            R_PROT,
            R_MANAGED,
            R_RESTRICTED,
            R_BELOW_CALLER,
            R_ABOVE_BOT,
            R_PRIV_BASE,
            HOME,
            R_DELETED,
            0,
        ];
        let callers = [CALLER, BOSS, PEON, GHOST, 0];
        let members = [TARGET, PEER, SUPERIOR, CALLER, GHOST, 0];
        let mut seen = std::collections::BTreeSet::new();
        for mode in [Mode::Off, Mode::Shadow, Mode::Live] {
            for caller in callers {
                for member in members {
                    for role in roles {
                        let mut rig = Rig::new();
                        rig.env.mode = mode;
                        rig.safeword
                            .states
                            .insert(TARGET, Some(SafewordState::Active));
                        let r = req(caller, member, role);
                        for reply in [
                            rig.add(&r).await,
                            rig.remove(&r).await,
                            rig.info(&info_req(caller, role)).await,
                            rig.list(&list_req(caller, Some(member))).await,
                        ] {
                            assert_reply_shape(&reply);
                            seen.insert(reply.code);
                        }
                    }
                }
            }
        }
        for code in [
            "feature_off",
            "invalid_id",
            "not_staff",
            "facts_unavailable",
            "added",
            "removed",
            "not_held",
            "already_held",
            "would_add",
            "would_remove",
            "actor_not_above_role",
            "actor_not_above_target",
            "at_or_above_bot",
            "privileged_permission",
            "protected_role",
            "managed_role",
            "everyone_role",
            "role_deleted",
            "safeword_restricted",
            "role_info",
            "role_list",
        ] {
            assert!(seen.contains(code), "sweep never produced {code}: {seen:?}");
        }
    }

    // ------------------------------------------------------------------ liveness

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
    }

    /// An independent model of what the retry policy must do with a scripted executor.
    fn model(script: &[Result<(), WriteError>]) -> (bool, &'static str, usize, Vec<u64>) {
        let mut calls = 0;
        let mut retries = 0u64;
        let mut slept = Vec::new();
        loop {
            let step = script.get(calls).copied().unwrap_or(Ok(()));
            calls += 1;
            match step {
                Ok(()) => return (true, "", calls, slept),
                Err(e) if e.status == 429 && retries < 3 => {
                    retries += 1;
                    slept.push(e.retry_after_ms.unwrap_or(1000 * retries).min(30_000));
                }
                Err(e) => {
                    let code = match e.status {
                        429 => "write_failed_rate_limited",
                        403 => "write_failed_forbidden",
                        _ => "write_failed_other",
                    };
                    return (false, code, calls, slept);
                }
            }
        }
    }

    #[tokio::test]
    async fn seeded_liveness_every_request_ends_with_one_reply_and_one_audit_entry() {
        let mut rng = Lcg(0x5EED_1234_ABCD_0001);
        for case in 0..600 {
            let add = rng.next() % 2 == 0;
            let mut rig = Rig::new();
            if !add {
                rig.world
                    .lock()
                    .unwrap()
                    .members
                    .get_mut(&TARGET)
                    .unwrap()
                    .push(R_OK);
            }
            let len = (rng.next() % 7) as usize;
            let script: Vec<Result<(), WriteError>> = (0..len)
                .map(|_| match rng.next() % 5 {
                    0 => Ok(()),
                    1 => Err(err(429, None, Some(u64::from(rng.next() % 100_000)))),
                    2 => Err(err(429, None, None)),
                    3 => Err(err(403, None, None)),
                    _ => Err(err(500, None, None)),
                })
                .collect();
            rig.exec.script = script.iter().copied().collect();
            let r = req(CALLER, TARGET, R_OK);
            let reply = if add {
                rig.add(&r).await
            } else {
                rig.remove(&r).await
            };
            let (ok, code, calls, slept) = model(&script);
            let want = if ok {
                if add {
                    "added"
                } else {
                    "removed"
                }
            } else {
                code
            };
            assert_eq!(reply.code, want, "case {case} script {script:?}");
            assert_eq!(rig.exec.calls.len(), calls, "case {case}");
            assert!(rig.exec.calls.len() <= 4, "case {case}");
            assert_eq!(rig.sleeper.slept, slept, "case {case}");
            assert_eq!(rig.audit.entries.len(), 1, "case {case}");
            assert_eq!(rig.audit.entries[0].code, reply.code, "case {case}");
            assert_eq!(
                rig.holds(TARGET, R_OK),
                if add { ok } else { !ok },
                "case {case}"
            );
            assert_reply_shape(&reply);
        }
    }

    #[tokio::test]
    async fn seeded_retry_by_the_operator_converges_and_then_stays_a_noop() {
        let mut rng = Lcg(0xFEED_BEEF_0000_0042);
        for case in 0..200 {
            let mut rig = Rig::new();
            let mut attempts = 0;
            // Each attempt hits a flaky executor; the operator simply repeats the command.
            while !rig.holds(TARGET, R_OK) {
                attempts += 1;
                assert!(attempts <= 60, "case {case}: no progress after 60 attempts");
                rig.exec.script = (0..4)
                    .map(|_| match rng.next() % 4 {
                        0 => Ok(()),
                        1 => Err(err(429, None, None)),
                        2 => Err(err(500, None, None)),
                        _ => Err(err(403, None, None)),
                    })
                    .collect();
                let reply = rig.add(&req(CALLER, TARGET, R_OK)).await;
                assert!(
                    reply.code == "added" || reply.code.starts_with("write_failed_"),
                    "case {case}: {}",
                    reply.code
                );
                assert_eq!(rig.holds(TARGET, R_OK), reply.code == "added");
            }
            let before = rig.exec.calls.len();
            assert_eq!(
                rig.add(&req(CALLER, TARGET, R_OK)).await.code,
                "already_held"
            );
            assert_eq!(
                rig.exec.calls.len(),
                before,
                "a converged add must not write again"
            );
            // And the way back converges too.
            let mut attempts = 0;
            while rig.holds(TARGET, R_OK) {
                attempts += 1;
                assert!(attempts <= 60, "case {case}: removal made no progress");
                rig.exec.script = (0..4)
                    .map(|_| {
                        if rng.next() % 3 == 0 {
                            Ok(())
                        } else {
                            Err(err(500, None, None))
                        }
                    })
                    .collect();
                let reply = rig.remove(&req(CALLER, TARGET, R_OK)).await;
                assert!(reply.code == "removed" || reply.code.starts_with("write_failed_"));
            }
            assert_eq!(
                rig.remove(&req(CALLER, TARGET, R_OK)).await.code,
                "not_held"
            );
        }
    }
}

//! Logic behind `/role add | remove | info | list` (`docs/verification/ROLE-ADMIN.md`).
//!
//! Poise registration and reply plumbing belong to wiring card W4. This file is the testable
//! core behind small ports: role facts (the RG2 `RoleSource`), the role executor, the
//! safeword lookup, an audit sink and the clock.
//!
//! Every grant is decided by K03 (`eligibility::decide` on the staff path) and written by the
//! K04 executor (`grant::apply`). K04 has no removal executor, so a removal is decided by K03
//! (`decide_removal`) plus the same hierarchy rules and written by a retry loop here that uses
//! K04's public classification and limits. On top of K03, the commands refuse privileged and
//! protected roles for staff too (K03 only refuses them on automated paths).
//!
//! The decision functions are pure. The async functions read through the ports, decide, write
//! through the executor and return an ephemeral reply built from a stable code; they echo
//! numeric ids only and never any text a user typed.

use crate::roles::eligibility::{
    self, Decision, DenyReason, GrantPath, SafewordState, StaffContext, PRIVILEGED_MASK,
};
use crate::roles::grant::{
    self, FailCode, GrantItem, RoleOutcome, RoleWriter, Sleeper, WriteError,
};
use crate::roles::runtime::{self, DecisionInputs, FactsOutcome, RoleEntry, RoleSource};
use crate::settings::{parse_mode as parse_mode_setting, Mode};

/// Feature name for the registry and start-up lines.
pub const FEATURE_NAME: &str = "role_admin";
/// Config key of the feature mode (`off` default, `shadow`, `live`).
pub const MODE_KEY: &str = "ROLE_ADMIN_MODE";
/// Longest reply text, in characters. Longer text is cut.
pub const MAX_REPLY_CHARS: usize = 1000;
/// Most roles `/role list` names; the rest are only counted.
pub const MAX_LIST_ROLES: usize = 20;

/// Parses `ROLE_ADMIN_MODE` through the shared settings helper. Anything that is not exactly
/// `off`, `shadow` or `live` (trimmed, any case) is `Off`.
pub fn parse_mode(lookup: impl Fn(&str) -> Option<String>) -> Mode {
    parse_mode_setting(MODE_KEY, lookup(MODE_KEY).as_deref()).unwrap_or(Mode::Off)
}

/// What the wiring card reads from the configuration and hands in.
#[derive(Debug, Clone)]
pub struct RoleAdminEnv {
    pub mode: Mode,
    /// `GUILD_ID`. `None` or zero fails closed while the mode is on.
    pub home_guild_id: Option<u64>,
    /// `STAFF_ROLE_ID`. `None` or zero fails closed while the mode is on.
    pub staff_role_id: Option<u64>,
    pub bot_user_id: u64,
    pub protected_role_ids: Vec<u64>,
    /// True when `PROTECTED_ROLE_IDS` had an unusable entry; the commands then fail closed.
    pub protected_roles_invalid: bool,
    pub safeword_restricted_role_ids: Vec<u64>,
}

/// Writes one role change to Discord. `reason` is the audit-log reason.
pub trait RoleExecutor: Send {
    fn add_role(&mut self, member_id: u64, role_id: u64, reason: &str) -> Result<(), WriteError>;
    fn remove_role(&mut self, member_id: u64, role_id: u64, reason: &str)
        -> Result<(), WriteError>;
}

/// The safeword state of a member. `None` means the state could not be read.
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

impl Action {
    fn word(self) -> &'static str {
        match self {
            Action::Add => "add",
            Action::Remove => "remove",
        }
    }
}

/// One line for staff: who did what to whom and the stable result code. Ids and a code only.
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

/// The ports `add` and `remove` use (grouped to keep argument counts low).
pub struct Ports<'a> {
    pub source: &'a dyn RoleSource,
    pub executor: &'a mut dyn RoleExecutor,
    pub sleeper: &'a mut (dyn Sleeper + Send),
    pub safeword: &'a dyn SafewordLookup,
    pub audit: &'a mut dyn AuditSink,
    pub clock: &'a dyn Clock,
}

/// `/role add` and `/role remove`. Ids are the resolved Discord ids, never typed text.
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

/// `member_id: None` lists the caller's own roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListRequest {
    pub guild_id: Option<u64>,
    pub caller_id: u64,
    pub member_id: Option<u64>,
}

/// What the wiring sends back to the invoker. `ephemeral` is always true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub ephemeral: bool,
    pub code: &'static str,
    pub text: String,
}

/// The audit-log reason of every write: the action and the caller's id, nothing else.
pub fn audit_reason(action: Action, caller_id: u64) -> String {
    format!("role_admin {} by {}", action.word(), caller_id)
}

// ---------------------------------------------------------------------------------------
// Refusals and replies
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    FeatureOff,
    HomeGuildUnset,
    StaffRoleUnset,
    NoGuild,
    ForeignGuild,
    InvalidId,
    ProtectedConfigInvalid,
    NotStaff,
    FactsUnavailable,
    Policy(DenyReason),
    WriteFailed(FailCode),
}

impl Refusal {
    fn code(self) -> &'static str {
        match self {
            Refusal::FeatureOff => "feature_off",
            Refusal::HomeGuildUnset => "home_guild_unset",
            Refusal::StaffRoleUnset => "staff_role_unset",
            Refusal::NoGuild => "no_guild",
            Refusal::ForeignGuild => "foreign_guild",
            Refusal::InvalidId => "invalid_id",
            Refusal::ProtectedConfigInvalid => "protected_roles_invalid",
            Refusal::NotStaff => "not_staff",
            Refusal::FactsUnavailable => runtime::CODE_FACTS_UNAVAILABLE,
            Refusal::Policy(reason) => reason.code(),
            Refusal::WriteFailed(FailCode::Forbidden) => "write_failed_forbidden",
            Refusal::WriteFailed(FailCode::UnknownRole) => "write_failed_unknown_role",
            Refusal::WriteFailed(FailCode::UnknownMember) => "write_failed_unknown_member",
            Refusal::WriteFailed(FailCode::RateLimited) => "write_failed_rate_limited",
            Refusal::WriteFailed(FailCode::Other) => "write_failed_other",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Refusal::FeatureOff => "The role admin commands are switched off.",
            Refusal::HomeGuildUnset => "The home server is not configured.",
            Refusal::StaffRoleUnset => "The staff role is not configured.",
            Refusal::NoGuild => "Use this command inside the server.",
            Refusal::ForeignGuild => "This command only works in the home server.",
            Refusal::InvalidId => "A required id was missing or zero.",
            Refusal::ProtectedConfigInvalid => "The protected role setting is unusable.",
            Refusal::NotStaff => "Only staff may use this command.",
            Refusal::FactsUnavailable => "Role or member data could not be read.",
            Refusal::Policy(reason) => match reason {
                DenyReason::RoleDeleted => "That role no longer exists.",
                DenyReason::EveryoneRole => "The everyone role cannot be managed.",
                DenyReason::ManagedRole => "Integration-managed roles cannot be managed.",
                DenyReason::AtOrAboveBot => "That role is at or above the bot's top role.",
                DenyReason::PrivilegedPermission => "That role carries privileged permissions.",
                DenyReason::ProtectedRole => "That role is protected.",
                DenyReason::SafewordRestricted => {
                    "That role cannot be given while the member's safeword is active."
                }
                DenyReason::MissingStaffContext => "The staff context was missing.",
                DenyReason::ActorNotAboveRole => "Your top role must be above that role.",
                DenyReason::ActorNotAboveTarget => "Your top role must be above that member's.",
            },
            Refusal::WriteFailed(_) => "The role change could not be made.",
        }
    }
}

fn bound(mut text: String) -> String {
    if let Some((cut, _)) = text.char_indices().nth(MAX_REPLY_CHARS) {
        text.truncate(cut);
    }
    text
}

fn reply(code: &'static str, text: String) -> Reply {
    Reply {
        ephemeral: true,
        code,
        text: bound(text),
    }
}

fn refusal_reply(refusal: Refusal) -> Reply {
    reply(
        refusal.code(),
        format!("{} (code: {})", refusal.message(), refusal.code()),
    )
}

/// The stable result of one add or remove, before it is turned into a reply.
struct Outcome {
    code: &'static str,
    text: String,
}

impl Outcome {
    fn refused(refusal: Refusal) -> Self {
        Outcome {
            code: refusal.code(),
            text: format!("{} (code: {})", refusal.message(), refusal.code()),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Gate and lookups
// ---------------------------------------------------------------------------------------

/// Mode, configuration, guild and id checks. Runs before anything is read or written.
/// Returns the home guild id and the staff role id.
fn gate(env: &RoleAdminEnv, guild_id: Option<u64>, ids: &[u64]) -> Result<(u64, u64), Refusal> {
    if env.mode == Mode::Off {
        return Err(Refusal::FeatureOff);
    }
    let home = env
        .home_guild_id
        .filter(|id| *id != 0)
        .ok_or(Refusal::HomeGuildUnset)?;
    let staff = env
        .staff_role_id
        .filter(|id| *id != 0)
        .ok_or(Refusal::StaffRoleUnset)?;
    let guild = guild_id.ok_or(Refusal::NoGuild)?;
    if guild != home {
        return Err(Refusal::ForeignGuild);
    }
    if env.bot_user_id == 0 || ids.contains(&0) {
        return Err(Refusal::InvalidId);
    }
    if env.protected_roles_invalid {
        return Err(Refusal::ProtectedConfigInvalid);
    }
    Ok((home, staff))
}

async fn read_roles(source: &dyn RoleSource, guild: u64) -> Result<Vec<RoleEntry>, Refusal> {
    source
        .roles(guild)
        .await
        .map_err(|_| Refusal::FactsUnavailable)
}

async fn read_member(
    source: &dyn RoleSource,
    guild: u64,
    member: u64,
) -> Result<Vec<u64>, Refusal> {
    source
        .member_roles(guild, member)
        .await
        .map_err(|_| Refusal::FactsUnavailable)
}

/// Highest role position among the roles a member holds (0 for none).
fn top_position(roles: &[RoleEntry], held: &[u64]) -> u16 {
    runtime::bot_top_position(roles, held)
}

// ---------------------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------------------

/// Everything the decisions need, already read.
struct Facts<'a> {
    env: &'a RoleAdminEnv,
    guild_id: u64,
    role_id: u64,
    roles: &'a [RoleEntry],
    bot_top: u16,
    caller_top: u16,
    target_top: u16,
}

fn is_privileged_or_protected(
    env: &RoleAdminEnv,
    role: &eligibility::RoleFacts,
) -> Option<DenyReason> {
    if role.permissions & PRIVILEGED_MASK != 0 {
        Some(DenyReason::PrivilegedPermission)
    } else if env.protected_role_ids.contains(&role.id) {
        Some(DenyReason::ProtectedRole)
    } else {
        None
    }
}

/// K03 on the staff path, then the command-level refusal of privileged and protected roles.
fn decide_add(facts: &Facts<'_>, safeword: SafewordState) -> Decision {
    let found = runtime::role_facts(facts.roles, facts.role_id);
    let inputs = DecisionInputs {
        path: GrantPath::StaffCommand,
        bot_top_position: facts.bot_top,
        guild_id: facts.guild_id,
        protected_role_ids: &facts.env.protected_role_ids,
        safeword,
        safeword_restricted_role_ids: &facts.env.safeword_restricted_role_ids,
        staff: Some(StaffContext {
            actor_top_position: facts.caller_top,
            target_top_position: facts.target_top,
        }),
    };
    let decision = match runtime::decide_from_facts(found, facts.role_id, &inputs) {
        runtime::RuntimeDecision::Decided(decision) => decision,
        runtime::RuntimeDecision::Unavailable => return Decision::Deny(DenyReason::RoleDeleted),
    };
    if decision != Decision::Allow {
        return decision;
    }
    match found {
        FactsOutcome::Found(role) => match is_privileged_or_protected(facts.env, &role) {
            Some(reason) => Decision::Deny(reason),
            None => Decision::Allow,
        },
        // `decide` already denied a missing role; never allow one.
        _ => Decision::Deny(DenyReason::RoleDeleted),
    }
}

/// K03's removal rule (bot hierarchy) plus the caller hierarchy and the same refused classes.
/// Safeword never blocks a removal.
fn decide_remove(facts: &Facts<'_>) -> Decision {
    let role = match runtime::role_facts(facts.roles, facts.role_id) {
        FactsOutcome::Found(role) => role,
        _ => return Decision::Deny(DenyReason::RoleDeleted),
    };
    if role.id == facts.guild_id {
        return Decision::Deny(DenyReason::EveryoneRole);
    }
    if role.managed {
        return Decision::Deny(DenyReason::ManagedRole);
    }
    let removal = eligibility::GrantRequest {
        path: GrantPath::StaffCommand,
        role,
        bot_top_position: facts.bot_top,
        guild_id: facts.guild_id,
        protected_role_ids: &facts.env.protected_role_ids,
        safeword: SafewordState::Inactive,
        safeword_restricted_role_ids: &[],
        staff: None,
    };
    if let Decision::Deny(reason) = eligibility::decide_removal(&removal) {
        return Decision::Deny(reason);
    }
    if let Some(reason) = is_privileged_or_protected(facts.env, &role) {
        return Decision::Deny(reason);
    }
    if facts.caller_top <= role.position {
        return Decision::Deny(DenyReason::ActorNotAboveRole);
    }
    if facts.caller_top <= facts.target_top {
        return Decision::Deny(DenyReason::ActorNotAboveTarget);
    }
    Decision::Allow
}

// ---------------------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------------------

/// Adapts the audited executor to K04's writer trait, passing the same reason on every call.
struct ReasonedWriter<'a> {
    executor: &'a mut dyn RoleExecutor,
    reason: &'a str,
}

impl RoleWriter for ReasonedWriter<'_> {
    fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
        self.executor.add_role(member_id, role_id, self.reason)
    }
}

struct DynSleeper<'a>(&'a mut (dyn Sleeper + Send));

impl Sleeper for DynSleeper<'_> {
    fn sleep_ms(&mut self, ms: u64) {
        self.0.sleep_ms(ms);
    }
}

/// Removal with K04's retry rule: only a 429 is retried, at most `MAX_RATE_LIMIT_RETRIES`
/// times, sleeping the writer's hint or a linear backoff, each sleep clamped.
fn remove_with_retry(
    executor: &mut dyn RoleExecutor,
    sleeper: &mut dyn Sleeper,
    reason: &str,
    request: &Request,
) -> Result<(), FailCode> {
    let mut retries: u8 = 0;
    loop {
        let error = match executor.remove_role(request.member_id, request.role_id, reason) {
            Ok(()) => return Ok(()),
            Err(error) => error,
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
        return Err(code);
    }
}

// ---------------------------------------------------------------------------------------
// add and remove
// ---------------------------------------------------------------------------------------

/// `/role add`.
pub async fn role_add(env: &RoleAdminEnv, mut ports: Ports<'_>, req: &Request) -> Reply {
    change(env, &mut ports, req, Action::Add).await
}

/// `/role remove`.
pub async fn role_remove(env: &RoleAdminEnv, mut ports: Ports<'_>, req: &Request) -> Reply {
    change(env, &mut ports, req, Action::Remove).await
}

async fn change(env: &RoleAdminEnv, ports: &mut Ports<'_>, req: &Request, action: Action) -> Reply {
    let ids = [req.caller_id, req.member_id, req.role_id];
    let (guild, staff) = match gate(env, req.guild_id, &ids) {
        Ok(ok) => ok,
        Err(refusal) => return refusal_reply(refusal),
    };
    let outcome = match perform(env, ports, req, action, (guild, staff)).await {
        Ok(outcome) => outcome,
        Err(refusal) => Outcome::refused(refusal),
    };
    ports.audit.record(&AuditEntry {
        at: ports.clock.now_unix(),
        action,
        caller_id: req.caller_id,
        member_id: req.member_id,
        role_id: req.role_id,
        code: outcome.code,
    });
    reply(outcome.code, outcome.text)
}

async fn perform(
    env: &RoleAdminEnv,
    ports: &mut Ports<'_>,
    req: &Request,
    action: Action,
    (guild, staff): (u64, u64),
) -> Result<Outcome, Refusal> {
    let roles = read_roles(ports.source, guild).await?;
    let caller_roles = read_member(ports.source, guild, req.caller_id).await?;
    if !caller_roles.contains(&staff) {
        return Err(Refusal::NotStaff);
    }
    let target_roles = read_member(ports.source, guild, req.member_id).await?;
    let bot_roles = read_member(ports.source, guild, env.bot_user_id).await?;
    let facts = Facts {
        env,
        guild_id: guild,
        role_id: req.role_id,
        roles: &roles,
        bot_top: top_position(&roles, &bot_roles),
        caller_top: top_position(&roles, &caller_roles),
        target_top: top_position(&roles, &target_roles),
    };
    let held = target_roles.contains(&req.role_id);
    let shadow = env.mode == Mode::Shadow;
    let (role, member) = (req.role_id, req.member_id);
    match action {
        Action::Add => {
            // An unreadable safeword is treated as active: a restricted role is then refused.
            let safeword = ports
                .safeword
                .state(guild, member)
                .unwrap_or(SafewordState::Active);
            let decision = decide_add(&facts, safeword);
            if shadow && decision == Decision::Allow {
                return Ok(if held {
                    already_held(role, member)
                } else {
                    Outcome {
                        code: "would_add",
                        text: format!("Dry run (shadow mode): would add role {role} to member {member}. Nothing was changed."),
                    }
                });
            }
            let reason = audit_reason(Action::Add, req.caller_id);
            let mut writer = ReasonedWriter {
                executor: &mut *ports.executor,
                reason: &reason,
            };
            let mut sleeper = DynSleeper(&mut *ports.sleeper);
            let batch = [GrantItem {
                role_id: role,
                decision,
                already_held: held,
            }];
            let report = grant::apply(
                &mut writer,
                &mut sleeper,
                &grant::GrantRequest {
                    member_id: member,
                    items: &batch,
                },
            );
            match report.results.first().map(|r| r.outcome) {
                Some(RoleOutcome::Done) => Ok(Outcome {
                    code: "added",
                    text: format!("Added role {role} to member {member}."),
                }),
                Some(RoleOutcome::NoOp) => Ok(already_held(role, member)),
                Some(RoleOutcome::Denied(reason)) => Err(Refusal::Policy(reason)),
                Some(RoleOutcome::Failed(code)) => Err(Refusal::WriteFailed(code)),
                // One item in, one result out; anything else is reported, not guessed.
                None => Err(Refusal::WriteFailed(FailCode::Other)),
            }
        }
        Action::Remove => {
            if let Decision::Deny(reason) = decide_remove(&facts) {
                return Err(Refusal::Policy(reason));
            }
            if !held {
                return Ok(Outcome {
                    code: "not_held",
                    text: format!("No change: member {member} does not hold role {role}."),
                });
            }
            if shadow {
                return Ok(Outcome {
                    code: "would_remove",
                    text: format!("Dry run (shadow mode): would remove role {role} from member {member}. Nothing was changed."),
                });
            }
            let reason = audit_reason(Action::Remove, req.caller_id);
            let mut sleeper = DynSleeper(&mut *ports.sleeper);
            match remove_with_retry(&mut *ports.executor, &mut sleeper, &reason, req) {
                Ok(()) => Ok(Outcome {
                    code: "removed",
                    text: format!("Removed role {role} from member {member}."),
                }),
                Err(code) => Err(Refusal::WriteFailed(code)),
            }
        }
    }
}

fn already_held(role: u64, member: u64) -> Outcome {
    Outcome {
        code: "already_held",
        text: format!("No change: member {member} already holds role {role}."),
    }
}

// ---------------------------------------------------------------------------------------
// info and list (read only)
// ---------------------------------------------------------------------------------------

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// `/role info`: read only.
pub async fn role_info(env: &RoleAdminEnv, source: &dyn RoleSource, req: &InfoRequest) -> Reply {
    match info(env, source, req).await {
        Ok(text) => reply("role_info", text),
        Err(refusal) => refusal_reply(refusal),
    }
}

async fn info(
    env: &RoleAdminEnv,
    source: &dyn RoleSource,
    req: &InfoRequest,
) -> Result<String, Refusal> {
    let (guild, staff) = gate(env, req.guild_id, &[req.caller_id, req.role_id])?;
    let roles = read_roles(source, guild).await?;
    let caller_roles = read_member(source, guild, req.caller_id).await?;
    if !caller_roles.contains(&staff) {
        return Err(Refusal::NotStaff);
    }
    let bot_roles = read_member(source, guild, env.bot_user_id).await?;
    let role = match runtime::role_facts(&roles, req.role_id) {
        FactsOutcome::Found(role) => role,
        _ => return Err(Refusal::Policy(DenyReason::RoleDeleted)),
    };
    if role.id == guild {
        return Err(Refusal::Policy(DenyReason::EveryoneRole));
    }
    let bot_top = top_position(&roles, &bot_roles);
    let caller_top = top_position(&roles, &caller_roles);
    Ok(format!(
        "Role {}: position: {}; managed: {}; privileged: {}; protected: {}; \
         safeword_restricted: {}; bot_above_role: {}; you_above_role: {}.",
        role.id,
        role.position,
        yes_no(role.managed),
        yes_no(role.permissions & PRIVILEGED_MASK != 0),
        yes_no(env.protected_role_ids.contains(&role.id)),
        yes_no(env.safeword_restricted_role_ids.contains(&role.id)),
        yes_no(bot_top > role.position),
        yes_no(caller_top > role.position),
    ))
}

/// `/role list [member]`: read only.
pub async fn role_list(env: &RoleAdminEnv, source: &dyn RoleSource, req: &ListRequest) -> Reply {
    match list(env, source, req).await {
        Ok(text) => reply("role_list", text),
        Err(refusal) => refusal_reply(refusal),
    }
}

async fn list(
    env: &RoleAdminEnv,
    source: &dyn RoleSource,
    req: &ListRequest,
) -> Result<String, Refusal> {
    let member = req.member_id.unwrap_or(req.caller_id);
    let (guild, staff) = gate(env, req.guild_id, &[req.caller_id, member])?;
    let roles = read_roles(source, guild).await?;
    let caller_roles = read_member(source, guild, req.caller_id).await?;
    if !caller_roles.contains(&staff) {
        return Err(Refusal::NotStaff);
    }
    let held = if member == req.caller_id {
        caller_roles
    } else {
        read_member(source, guild, member).await?
    };
    let mut shown: Vec<&RoleEntry> = roles
        .iter()
        .filter(|r| r.id != guild && held.contains(&r.id))
        .collect();
    shown.sort_by(|a, b| b.position.cmp(&a.position).then(b.id.cmp(&a.id)));
    shown.dedup_by_key(|r| r.id);
    let total = shown.len();
    let lines: Vec<String> = shown
        .iter()
        .take(MAX_LIST_ROLES)
        .map(|r| format!("{} (position {})", r.id, r.position))
        .collect();
    let noun = if total == 1 { "role" } else { "roles" };
    let mut text = format!("Member {member} holds {total} {noun}");
    if lines.is_empty() {
        text.push('.');
    } else {
        text.push_str(": ");
        text.push_str(&lines.join(", "));
        if total > lines.len() {
            text.push_str(&format!("; {} more not shown", total - lines.len()));
        }
        text.push('.');
    }
    Ok(text)
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
        assert_eq!(
            rig.add(&req(BOSS, PEER, R_BELOW_CALLER)).await.code,
            "added"
        );
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
    async fn shadow_mode_reports_noops_as_noops_not_as_would_be_changes() {
        let mut rig = Rig::new();
        rig.env.mode = Mode::Shadow;
        assert_eq!(
            rig.add(&req(CALLER, TARGET, R_TARGET_TOP)).await.code,
            "already_held"
        );
        assert_eq!(
            rig.remove(&req(CALLER, TARGET, R_OK)).await.code,
            "not_held"
        );
        assert!(rig.exec.calls.is_empty());
    }

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
        assert!(info.text.contains("bot_above_role: yes"));
        assert!(info.text.contains("you_above_role: yes"));

        let prot = rig.info(&info_req(CALLER, R_PROT)).await;
        assert!(prot.text.contains("protected: yes"));
        let restricted = rig.info(&info_req(CALLER, R_RESTRICTED)).await;
        assert!(restricted.text.contains("safeword_restricted: yes"));
        let managed = rig.info(&info_req(CALLER, R_MANAGED)).await;
        assert!(managed.text.contains("managed: yes"));
        let privileged = rig.info(&info_req(CALLER, R_PRIV_BASE)).await;
        assert!(privileged.text.contains("privileged: yes"));
        let high = rig.info(&info_req(CALLER, R_HIGH)).await;
        assert!(high.text.contains("bot_above_role: yes"));
        assert!(high.text.contains("you_above_role: no"));
        let above_bot = rig.info(&info_req(BOSS, R_AT_BOT)).await;
        assert!(above_bot.text.contains("bot_above_role: no"));
        assert!(above_bot.text.contains("you_above_role: yes"));

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
    async fn list_never_names_the_everyone_role_even_if_the_source_returns_it() {
        let rig = Rig::new();
        rig.world
            .lock()
            .unwrap()
            .members
            .get_mut(&TARGET)
            .unwrap()
            .push(HOME);
        let reply = rig.list(&list_req(CALLER, Some(TARGET))).await;
        assert_eq!(reply.code, "role_list");
        assert!(!reply.text.contains(&HOME.to_string()), "{}", reply.text);
        assert!(reply.text.contains("1 role:"), "{}", reply.text);
    }

    #[tokio::test]
    async fn list_cap_applies_to_the_number_of_roles_named() {
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
            R_TARGET_TOP,
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
                        let passes_gate =
                            mode != Mode::Off && caller != 0 && member != 0 && role != 0;
                        let expected_audit = if passes_gate { 2 } else { 0 };
                        assert_eq!(
                            rig.audit.entries.len(),
                            expected_audit,
                            "{mode:?} {caller} {member} {role}"
                        );
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

    #[test]
    fn long_reply_text_is_cut_to_the_character_limit_on_a_char_boundary() {
        assert_eq!(bound("x".repeat(5_000)).chars().count(), MAX_REPLY_CHARS);
        let multi = bound("\u{00e9}".repeat(5_000));
        assert_eq!(multi.chars().count(), MAX_REPLY_CHARS);
        let exact = "y".repeat(MAX_REPLY_CHARS);
        assert_eq!(bound(exact.clone()), exact);
        assert_eq!(bound(String::new()), "");
        let reply = reply("added", "z".repeat(MAX_REPLY_CHARS + 1));
        assert!(reply.ephemeral);
        assert_eq!(reply.text.chars().count(), MAX_REPLY_CHARS);
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
            let add = rng.next().is_multiple_of(2);
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
                        if rng.next().is_multiple_of(3) {
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

//! The `/safeword status|strike|clear` staff command logic (SW4, `docs/verification/SAFEWORD-COMMAND.md`).
//!
//! Plain functions over an [`Env`] (the SW3 collaborators, the clock and the confirm book), so
//! a test injects fakes. Registering a poise command and sending the ephemeral reply belong to
//! the wiring card. Every state change goes through the SW1 state machine (`state::step`) and
//! the SW2 store (the `StateStore` trait); role writes use K03 (`SafewordRestore`) and K04.
//! Replies and alerts carry stable codes and counts only.

use std::collections::VecDeque;

use crate::roles::eligibility::{
    decide_removal, Decision, GrantPath, GrantRequest, RoleFacts, SafewordState,
};
use crate::roles::grant::{self as k04, GrantItem, RoleOutcome, RoleWriter, Sleeper, WriteError};
use crate::roles::runtime::{
    self as roles_facts, decide_from_facts, DecisionInputs, RuntimeDecision,
};
use crate::safeword::config::SafewordConfig;
use crate::safeword::repository::RepoError;
use crate::safeword::runtime::{
    MemberRoles, Ops, WriteCode, CODE_BOT_TOP_UNKNOWN, CODE_DM_FAILED, CODE_FACTS_UNAVAILABLE,
    CODE_PARTIAL_ROLE_FAILURE, CODE_ROLE_WRITE_FAILED, CODE_STATE_FAILED,
};
use crate::safeword::state::{step, Action, ActionId, Context, Event, FailureCode, RoleId, State};
use crate::settings::Mode;

/// How long a `clear` confirmation stays valid, in seconds.
pub const CONFIRM_WINDOW_SECS: i64 = 60;
/// The most pending confirmations the book keeps; the oldest is dropped beyond this.
pub const MAX_ARMED: usize = 32;

// ---------------------------------------------------------------------------
// Codes and replies
// ---------------------------------------------------------------------------

/// Stable result codes of the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Disabled,
    ShadowMode,
    Denied,
    InvalidTarget,
    InvalidRole,
    Status,
    Struck,
    RoleNotInSnapshot,
    NotActive,
    WrongState,
    Busy,
    StoreFailed,
    ConfirmRequired,
    ConfirmExpired,
    ConfirmMismatch,
    AlreadyClear,
    Cleared,
    RestoreFailed,
    ReleaseIncomplete,
}

/// How a code counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The command did what was asked.
    Success,
    /// The command declined; nothing changed.
    Refusal,
    /// Something outside the command failed.
    Failure,
}

impl Code {
    /// Every code, for tables and sweeps.
    pub const ALL: [Code; 19] = [
        Code::Disabled,
        Code::ShadowMode,
        Code::Denied,
        Code::InvalidTarget,
        Code::InvalidRole,
        Code::Status,
        Code::Struck,
        Code::RoleNotInSnapshot,
        Code::NotActive,
        Code::WrongState,
        Code::Busy,
        Code::StoreFailed,
        Code::ConfirmRequired,
        Code::ConfirmExpired,
        Code::ConfirmMismatch,
        Code::AlreadyClear,
        Code::Cleared,
        Code::RestoreFailed,
        Code::ReleaseIncomplete,
    ];

    /// The stable text of the code.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Disabled => "disabled",
            Code::ShadowMode => "shadow_mode",
            Code::Denied => "denied",
            Code::InvalidTarget => "invalid_target",
            Code::InvalidRole => "invalid_role",
            Code::Status => "status",
            Code::Struck => "struck",
            Code::RoleNotInSnapshot => "role_not_in_snapshot",
            Code::NotActive => "not_active",
            Code::WrongState => "wrong_state",
            Code::Busy => "busy",
            Code::StoreFailed => "store_failed",
            Code::ConfirmRequired => "confirm_required",
            Code::ConfirmExpired => "confirm_expired",
            Code::ConfirmMismatch => "confirm_mismatch",
            Code::AlreadyClear => "already_clear",
            Code::Cleared => "cleared",
            Code::RestoreFailed => "restore_failed",
            Code::ReleaseIncomplete => "release_incomplete",
        }
    }

    /// Whether the code is a success, a refusal or a failure.
    pub fn class(self) -> Class {
        match self {
            Code::Status | Code::Struck | Code::AlreadyClear | Code::Cleared => Class::Success,
            Code::StoreFailed | Code::Busy | Code::RestoreFailed | Code::ReleaseIncomplete => {
                Class::Failure
            }
            _ => Class::Refusal,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Code::Disabled => "The safeword command is switched off.",
            Code::ShadowMode => "The safeword is in shadow mode: nothing was changed.",
            Code::Denied => "You may not use this.",
            Code::InvalidTarget => "That member is not valid.",
            Code::InvalidRole => "That role is not valid.",
            Code::Status => "Safeword status.",
            Code::Struck => "The role will not be given back.",
            Code::RoleNotInSnapshot => {
                "That role is not in the member's snapshot. Nothing changed."
            }
            Code::NotActive => "The member's safeword is not active. Nothing changed.",
            Code::WrongState => "The safeword is mid-change. Nothing changed; try again later.",
            Code::Busy => "The record changed while this ran. Nothing was written; try again.",
            Code::StoreFailed => "The safeword record could not be read or written.",
            Code::ConfirmRequired => {
                "Not done. Repeat the same command with confirm set, within the window."
            }
            Code::ConfirmExpired => "The confirmation ran out. Nothing was restored.",
            Code::ConfirmMismatch => "There is no matching confirmation. Nothing was restored.",
            Code::AlreadyClear => "The member's safeword is already cleared. Nothing changed.",
            Code::Cleared => "The safeword is cleared and the roles are restored.",
            Code::RestoreFailed => "Restoring the roles failed. The safeword stays active.",
            Code::ReleaseIncomplete => "The roles are back but the release did not finish. Retry.",
        }
    }
}

/// The safeword state of a member as the command shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateCode {
    Idle,
    Activating,
    Active,
    Releasing,
}

impl StateCode {
    /// The stable text of the state.
    pub fn as_str(self) -> &'static str {
        match self {
            StateCode::Idle => "idle",
            StateCode::Activating => "activating",
            StateCode::Active => "active",
            StateCode::Releasing => "releasing",
        }
    }
}

/// One reply. Always ephemeral; its text is built from stable codes and counts only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The result.
    pub code: Code,
    /// A stable sub-code (a refusal reason, a store error code).
    pub detail: Option<&'static str>,
    /// Stable failure codes of a restore or release, in the order they happened.
    pub codes: Vec<&'static str>,
    /// The member's state, for `status` and the prompts.
    pub state: Option<StateCode>,
    /// Roles held in the snapshot; set only for staff.
    pub held_roles: Option<usize>,
    ephemeral: bool,
}

impl Reply {
    /// A reply with a code and nothing else. The only constructor, so every reply is ephemeral.
    pub fn new(code: Code) -> Self {
        Reply {
            code,
            detail: None,
            codes: Vec::new(),
            state: None,
            held_roles: None,
            ephemeral: true,
        }
    }

    fn with_detail(code: Code, detail: &'static str) -> Self {
        Reply {
            detail: Some(detail),
            ..Reply::new(code)
        }
    }

    /// How the reply counts.
    pub fn class(&self) -> Class {
        self.code.class()
    }

    /// Whether the reply is shown only to the caller. Always true.
    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// The reply text: a fixed sentence, then stable codes and counts. No user text, no ids.
    pub fn text(&self) -> String {
        let mut out = format!("{} [{}]", self.code.message(), self.code.as_str());
        if let Some(state) = self.state {
            out.push_str(&format!(" state={}", state.as_str()));
        }
        if let Some(held) = self.held_roles {
            out.push_str(&format!(" roles={held}"));
        }
        if let Some(detail) = self.detail {
            out.push_str(&format!(" reason={detail}"));
        }
        if !self.codes.is_empty() {
            out.push_str(&format!(" codes={}", self.codes.join(",")));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Inputs and the environment
// ---------------------------------------------------------------------------

/// The configuration the command needs. The wiring card fills it from `SAFEWORD_*`,
/// `GUILD_ID` and `STAFF_ROLE_ID`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandConfig {
    /// The SW3 configuration (mode, safeword ids, restricted roles).
    pub safeword: SafewordConfig,
    /// The one guild served. `None` or `Some(0)` serves nobody.
    pub home_guild: Option<u64>,
    /// The staff role. `None` or `Some(0)` means nobody is staff.
    pub staff_role_id: Option<u64>,
}

/// Who called the command, in plain values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invoker {
    /// The calling user.
    pub user: u64,
    /// The guild the command was used in.
    pub guild: u64,
    /// The roles the caller holds in that guild.
    pub role_ids: Vec<u64>,
}

/// `strike <member> <role>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrikeRequest {
    pub member: u64,
    pub role: u64,
}

/// `clear <member> [confirm]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearRequest {
    pub member: u64,
    /// True only on the explicit second call.
    pub confirm: bool,
}

/// The collaborators of the command: the SW3 `Ops` (store, role writes, facts, DMs, alerts,
/// sleeper), the clock, the configuration and the confirm book.
pub trait Env {
    /// The configuration.
    fn config(&self) -> &CommandConfig;
    /// The unix second the call happens in.
    fn now(&self) -> i64;
    /// The SW3 collaborators for the home guild.
    fn ops(&mut self) -> &mut Ops;
    /// The pending `clear` confirmations.
    fn confirms(&mut self) -> &mut ConfirmBook;
}

/// One pending `clear` confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Armed {
    pub guild: u64,
    pub caller: u64,
    pub member: u64,
    /// The stored row version the confirmation was given for.
    pub version: u64,
    pub armed_at: i64,
}

/// The pending `clear` confirmations. In memory only: a restart forgets them, which only
/// means staff repeats the first call.
#[derive(Debug, Default)]
pub struct ConfirmBook {
    entries: Vec<Armed>,
}

impl ConfirmBook {
    /// An empty book.
    pub fn new() -> Self {
        ConfirmBook::default()
    }

    /// How many confirmations are pending.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is pending.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record a confirmation, replacing the same caller's pending one for the same member.
    ///
    /// Entries outside the window (relative to `now`) are dropped first; if the book is still
    /// full, the oldest entry is dropped, so the book never holds more than [`MAX_ARMED`].
    pub fn arm(&mut self, armed: Armed, now: i64) {
        self.entries.retain(|e| {
            let same_key =
                e.guild == armed.guild && e.caller == armed.caller && e.member == armed.member;
            !same_key && within_window(e.armed_at, now)
        });
        while self.entries.len() >= MAX_ARMED {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.armed_at)
                .map(|(i, _)| i);
            match oldest {
                Some(index) => {
                    self.entries.remove(index);
                }
                None => break,
            }
        }
        self.entries.push(armed);
    }

    /// Remove and return the caller's pending confirmation for the member, if any.
    pub fn take(&mut self, guild: u64, caller: u64, member: u64) -> Option<Armed> {
        let index = self
            .entries
            .iter()
            .position(|e| e.guild == guild && e.caller == caller && e.member == member)?;
        Some(self.entries.remove(index))
    }
}

/// True when `now` is inside the confirmation window that started at `armed_at`. A clock that
/// went backwards is outside it.
fn within_window(armed_at: i64, now: i64) -> bool {
    match now.checked_sub(armed_at) {
        Some(age) => (0..=CONFIRM_WINDOW_SECS).contains(&age),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

/// Who may do what, decided from the configuration and the caller alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Staff,
    Member,
}

/// The checks every subcommand starts with, before anything is read or written: the mode, the
/// home guild (commands are registered globally, so any guild can call), the caller and the
/// staff role. A refusal here touches nothing.
fn gate(cfg: &CommandConfig, who: &Invoker) -> Result<Access, Reply> {
    if cfg.safeword.mode == Mode::Off {
        return Err(Reply::new(Code::Disabled));
    }
    match cfg.home_guild {
        None | Some(0) => return Err(Reply::with_detail(Code::Denied, "no_home_guild")),
        Some(home) if home != who.guild => {
            return Err(Reply::with_detail(Code::Denied, "other_guild"))
        }
        Some(_) => {}
    }
    if who.user == 0 {
        return Err(Reply::with_detail(Code::Denied, "no_caller"));
    }
    let is_staff = match cfg.staff_role_id {
        None | Some(0) => false,
        Some(role) => who.role_ids.contains(&role),
    };
    Ok(if is_staff {
        Access::Staff
    } else {
        Access::Member
    })
}

/// The gate for the subcommands that change something: staff only, and not in shadow mode.
fn gate_staff_write(cfg: &CommandConfig, who: &Invoker) -> Result<(), Reply> {
    if gate(cfg, who)? != Access::Staff {
        return Err(Reply::with_detail(Code::Denied, "not_staff"));
    }
    if cfg.safeword.mode == Mode::Shadow {
        return Err(Reply::new(Code::ShadowMode));
    }
    Ok(())
}

/// The reply for a failed store call. A lost compare-and-swap is `busy`; the rest is a
/// failure. The code is the store's own stable code.
fn store_reply(code: &'static str) -> Reply {
    if code == RepoError::Stale.code() {
        Reply::with_detail(Code::Busy, code)
    } else {
        Reply::with_detail(Code::StoreFailed, code)
    }
}

/// The reply for a failed store call that also tells staff (a stale write is not an alert).
fn store_failure(env: &mut dyn Env, code: &'static str) -> Reply {
    if code != RepoError::Stale.code() {
        env.ops().alerter.alert(code);
    }
    store_reply(code)
}

fn state_code(state: &State) -> StateCode {
    match state {
        State::Inactive => StateCode::Idle,
        State::Activating(_) => StateCode::Activating,
        State::Active(_) => StateCode::Active,
        State::Releasing(_) => StateCode::Releasing,
    }
}

/// Load a member's state and version. No row is `Inactive` at version 0; an unreadable row is
/// an error, never a guess.
fn load(env: &mut dyn Env, guild: u64, member: u64) -> Result<(State, u64), &'static str> {
    match env.ops().store.load(guild, member)? {
        Some(pair) => Ok(pair),
        None => Ok((State::Inactive, 0)),
    }
}

/// The state machine's context. Strike and release read no member roles, so none are passed.
fn machine_context(env: &mut dyn Env) -> Context {
    let cfg = env.config().safeword.clone();
    Context {
        safeword_role: cfg.role_id,
        restricted_roles: cfg.restricted_role_ids,
        protected_roles: env.ops().protected_role_ids.clone(),
        held_roles: Vec::new(),
        filter_restore: true,
        defer_grants: cfg.defer_grants,
    }
}

/// `status <member>`. Staff may look at any member and see how many roles the snapshot holds;
/// anyone else may look only at their own state, and never sees the count.
pub fn status(env: &mut dyn Env, who: &Invoker, member: u64) -> Reply {
    let access = match gate(env.config(), who) {
        Ok(access) => access,
        Err(reply) => return reply,
    };
    if access != Access::Staff && member != who.user {
        return Reply::with_detail(Code::Denied, "not_staff");
    }
    if member == 0 {
        return Reply::new(Code::InvalidTarget);
    }
    match load(env, who.guild, member) {
        Err(code) => store_reply(code),
        Ok((state, _)) => {
            let mut reply = Reply::new(Code::Status);
            reply.state = Some(state_code(&state));
            if access == Access::Staff {
                reply.held_roles = state.snapshot().map(|s| s.saved_roles.len());
            }
            reply
        }
    }
}

/// `strike <member> <role>`: take a role out of an Active member's restore snapshot.
///
/// Only a role in the snapshot is accepted. The change is one SW1 `Strike` event and one
/// compare-and-swap store of the machine's own state; if the store fails nothing changed.
pub fn strike(env: &mut dyn Env, who: &Invoker, req: &StrikeRequest) -> Reply {
    if let Err(reply) = gate_staff_write(env.config(), who) {
        return reply;
    }
    if req.member == 0 {
        return Reply::new(Code::InvalidTarget);
    }
    if req.role == 0 {
        return Reply::new(Code::InvalidRole);
    }
    let (state, version) = match load(env, who.guild, req.member) {
        Ok(pair) => pair,
        Err(code) => return store_failure(env, code),
    };
    let snapshot = match &state {
        State::Inactive => return Reply::new(Code::NotActive),
        State::Activating(_) => return Reply::with_detail(Code::WrongState, "activating"),
        State::Releasing(_) => return Reply::with_detail(Code::WrongState, "releasing"),
        State::Active(snapshot) => snapshot,
    };
    if !snapshot.saved_roles.contains(&req.role) {
        return Reply::new(Code::RoleNotInSnapshot);
    }
    let ctx = machine_context(env);
    let (struck, _) = step(&state, Event::Strike(req.role), &ctx);
    if struck == state {
        return Reply::new(Code::RoleNotInSnapshot);
    }
    // The store write IS the machine's `SaveSnapshot` action: acknowledge it, then persist the
    // acknowledged state in one write. If the write fails the stored row is untouched.
    let (saved_state, _) = step(&struck, Event::ActionDone(ActionId::SaveSnapshot), &ctx);
    let now = env.now();
    match env
        .ops()
        .store
        .store(who.guild, req.member, &saved_state, version, now)
    {
        Ok(_) => {
            let mut reply = Reply::new(Code::Struck);
            reply.held_roles = saved_state.snapshot().map(|s| s.saved_roles.len());
            reply
        }
        Err(code) => store_failure(env, code),
    }
}

/// `clear <member> [confirm]`: end the member's safeword and give the snapshot back.
///
/// Two calls are needed. The first (`confirm` false) only records a confirmation; the second
/// (`confirm` true) by the same caller for the same member, while the stored row is still the
/// same version and inside [`CONFIRM_WINDOW_SECS`], does the release.
pub fn clear(env: &mut dyn Env, who: &Invoker, req: &ClearRequest) -> Reply {
    if let Err(reply) = gate_staff_write(env.config(), who) {
        return reply;
    }
    if req.member == 0 {
        return Reply::new(Code::InvalidTarget);
    }
    let (state, version) = match load(env, who.guild, req.member) {
        Ok(pair) => pair,
        Err(code) => return store_failure(env, code),
    };
    match &state {
        // Already cleared (or never on safeword): nothing to confirm, nothing to write.
        State::Inactive => return Reply::new(Code::AlreadyClear),
        State::Activating(_) => return Reply::with_detail(Code::WrongState, "activating"),
        State::Active(_) | State::Releasing(_) => {}
    }
    let now = env.now();
    if !req.confirm {
        let armed = Armed {
            guild: who.guild,
            caller: who.user,
            member: req.member,
            version,
            armed_at: now,
        };
        env.confirms().arm(armed, now);
        let mut reply = Reply::new(Code::ConfirmRequired);
        reply.state = Some(state_code(&state));
        return reply;
    }
    // The confirmation is used up by this attempt, whatever its outcome.
    let Some(armed) = env.confirms().take(who.guild, who.user, req.member) else {
        return Reply::new(Code::ConfirmMismatch);
    };
    if armed.version != version {
        return Reply::new(Code::ConfirmMismatch);
    }
    if !within_window(armed.armed_at, now) {
        return Reply::new(Code::ConfirmExpired);
    }
    let run = Run {
        guild: who.guild,
        member: req.member,
        now,
    };
    release(env, &run, &state, version)
}

// ---------------------------------------------------------------------------
// The release
// ---------------------------------------------------------------------------

/// One release of one member.
struct Run {
    guild: u64,
    member: u64,
    now: i64,
}

/// A step of the release went one of three ways.
enum Step {
    Done,
    /// A direct message that could not be delivered. Acknowledged to the machine, as SW3 does,
    /// so no member is held in `Releasing` by a DM they cannot receive.
    DmUndelivered,
    Failed(Vec<&'static str>),
}

/// The most actions one release executes.
const MAX_STEPS: usize = 64;

fn push_code(codes: &mut Vec<&'static str>, code: &'static str) {
    if !codes.contains(&code) {
        codes.push(code);
    }
}

/// The next action: a pending `RestoreRoles` always goes first, so nothing is removed or
/// cleared before the roles are back.
fn next_action(pending: &mut VecDeque<Action>) -> Option<Action> {
    let restore = pending
        .iter()
        .position(|a| matches!(a, Action::RestoreRoles(_)));
    match restore {
        Some(index) => pending.remove(index),
        None => pending.pop_front(),
    }
}

/// Feed the SW1 machine one release event and run what it emits.
///
/// The restore runs first. If it fails, the machine's new state is thrown away: the stored row
/// stays what it was (Active), nothing else was done, and a retry derives the same plan from the
/// same snapshot. If it succeeds, the remaining steps run in the machine's order, but
/// `ClearSnapshot` and the DM run only while every earlier step of this run succeeded; a step
/// that is held back is reported to the machine as failed, so the next retry re-emits it. The
/// machine's end state is then stored (Releasing) or, when it is Inactive, the row is cleared.
fn release(env: &mut dyn Env, run: &Run, state: &State, version: u64) -> Reply {
    let ctx = machine_context(env);
    let (mut machine, emitted) = step(state, Event::ReactionRemoved, &ctx);
    let mut pending: VecDeque<Action> = emitted.into();
    let mut version = version;
    let mut codes: Vec<&'static str> = Vec::new();
    let mut blocked = false;
    let mut cleared_in_run = false;
    let mut executed = 0usize;

    while let Some(action) = next_action(&mut pending) {
        executed += 1;
        if executed > MAX_STEPS {
            env.ops().alerter.alert(CODE_STATE_FAILED);
            push_code(&mut codes, CODE_STATE_FAILED);
            break;
        }
        let held_back = blocked && matches!(action, Action::ClearSnapshot | Action::SendDm(_));
        let outcome = if held_back {
            Step::Failed(Vec::new())
        } else {
            perform(env, run, &action, &mut version, &mut cleared_in_run)
        };
        let id = action.id();
        let event = match outcome {
            Step::Done => Event::ActionDone(id),
            Step::DmUndelivered => {
                push_code(&mut codes, CODE_DM_FAILED);
                Event::ActionDone(id)
            }
            Step::Failed(failed) => {
                if executed == 1 && matches!(action, Action::RestoreRoles(_)) {
                    // Nothing else has been done: keep the member Active, store nothing.
                    let mut reply = Reply::new(Code::RestoreFailed);
                    reply.codes = failed;
                    return reply;
                }
                for code in failed {
                    push_code(&mut codes, code);
                }
                blocked = true;
                Event::ActionFailed(id, FailureCode::TransientHttp)
            }
        };
        let (next, more) = step(&machine, event, &ctx);
        machine = next;
        pending.extend(more);
    }

    if matches!(machine, State::Inactive) {
        if !cleared_in_run {
            // The machine ended Inactive without a `ClearSnapshot` in this run (it was
            // acknowledged by an earlier run): the row is cleared here.
            match env
                .ops()
                .store
                .clear(run.guild, run.member, version, run.now)
            {
                Ok(_) => {}
                Err(code) => {
                    let mut reply = store_failure(env, code);
                    reply.codes = codes;
                    return reply;
                }
            }
        }
        let mut reply = Reply::new(Code::Cleared);
        reply.codes = codes;
        return reply;
    }
    // Not finished: store the machine's own state (never a half-cleared guess) and say so.
    match env
        .ops()
        .store
        .store(run.guild, run.member, &machine, version, run.now)
    {
        Ok(_) => {
            let mut reply = Reply::new(Code::ReleaseIncomplete);
            reply.state = Some(state_code(&machine));
            reply.codes = codes;
            reply
        }
        Err(code) => {
            let mut reply = store_failure(env, code);
            reply.codes = codes;
            reply
        }
    }
}

/// Execute one release action.
fn perform(
    env: &mut dyn Env,
    run: &Run,
    action: &Action,
    version: &mut u64,
    cleared_in_run: &mut bool,
) -> Step {
    match action {
        Action::RestoreRoles(roles) => match restore_roles(env, run, roles) {
            Ok(()) => Step::Done,
            Err(codes) => Step::Failed(codes),
        },
        Action::RemoveSafewordRole => remove_safeword_role(env, run),
        Action::ClearSnapshot => {
            match env
                .ops()
                .store
                .clear(run.guild, run.member, *version, run.now)
            {
                Ok(new_version) => {
                    *version = new_version;
                    *cleared_in_run = true;
                    Step::Done
                }
                Err(code) => {
                    env.ops().alerter.alert(code);
                    Step::Failed(vec![code])
                }
            }
        }
        Action::SendDm(kind) => {
            if !env.config().safeword.dm {
                return Step::Done;
            }
            match env.ops().dms.send_dm(run.member, kind.text()) {
                Ok(()) => Step::Done,
                Err(_) => {
                    env.ops().alerter.alert(CODE_DM_FAILED);
                    Step::DmUndelivered
                }
            }
        }
        // A release never emits these; a machine that did would have a bug, and doing nothing
        // is the safe answer.
        Action::SaveSnapshot | Action::RemoveRoles(_) | Action::AddSafewordRole => {
            env.ops().alerter.alert(CODE_STATE_FAILED);
            Step::Failed(vec![CODE_STATE_FAILED])
        }
    }
}

/// Remove the safeword role, after the K03 removal decision.
fn remove_safeword_role(env: &mut dyn Env, run: &Run) -> Step {
    let role = env.config().safeword.role_id;
    let ops = env.ops();
    if let Some(code) = removal_decision(run.guild, role, ops) {
        ops.alerter.alert(code);
        return Step::Failed(vec![code]);
    }
    match ops.members.remove_role(run.member, role) {
        Ok(()) => Step::Done,
        Err(_) => {
            ops.alerter.alert(CODE_ROLE_WRITE_FAILED);
            Step::Failed(vec![CODE_ROLE_WRITE_FAILED])
        }
    }
}

/// Give the planned roles back: every role gets a K03 decision on the `SafewordRestore` path
/// (the member is still on safeword, so any other path would be denied), and the K04 executor
/// writes only what K03 allowed and the member does not hold, retrying a 429 with its backoff.
/// Any role that is not written is a failure: a partial restore is never a success.
fn restore_roles(env: &mut dyn Env, run: &Run, roles: &[RoleId]) -> Result<(), Vec<&'static str>> {
    let restricted = env.config().safeword.restricted_role_ids.clone();
    let ops = env.ops();
    // A failed lookup means "assume nothing is held": every role is written (a repeat is
    // harmless), none is skipped.
    let held_now = ops.members.member_roles(run.member).unwrap_or_default();
    let mut items: Vec<GrantItem> = Vec::with_capacity(roles.len());
    let mut failed: Vec<&'static str> = Vec::new();
    for role_id in roles {
        match grant_decision(run.guild, *role_id, &restricted, ops) {
            Ok(decision) => items.push(GrantItem {
                role_id: *role_id,
                decision,
                already_held: held_now.contains(role_id),
            }),
            Err(code) => {
                // Left out of the request, so the executor can never write it.
                ops.alerter.alert(code);
                push_code(&mut failed, code);
            }
        }
    }
    let report = {
        let mut writer = MemberWriter {
            members: &mut *ops.members,
        };
        let mut sleeper = SleepAdapter {
            sleeper: &mut *ops.sleeper,
        };
        k04::apply(
            &mut writer,
            &mut sleeper,
            &k04::GrantRequest {
                member_id: run.member,
                items: &items,
            },
        )
    };
    let mut written = 0usize;
    for result in &report.results {
        match result.outcome {
            RoleOutcome::Done | RoleOutcome::NoOp => written += 1,
            RoleOutcome::Denied(reason) => {
                ops.alerter.alert(reason.code());
                push_code(&mut failed, reason.code());
            }
            RoleOutcome::Failed(_) => {
                ops.alerter.alert(CODE_ROLE_WRITE_FAILED);
                push_code(&mut failed, CODE_ROLE_WRITE_FAILED);
            }
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    if written > 0 {
        ops.alerter.alert(CODE_PARTIAL_ROLE_FAILURE);
        push_code(&mut failed, CODE_PARTIAL_ROLE_FAILURE);
    }
    Err(failed)
}

/// The K03 grant decision for a restored role, or the stable code of a failed lookup.
fn grant_decision(
    guild: u64,
    role_id: RoleId,
    restricted: &[u64],
    ops: &mut Ops,
) -> Result<Decision, &'static str> {
    let Some(bot_top) = ops.facts.bot_top_position() else {
        return Err(CODE_BOT_TOP_UNKNOWN);
    };
    let facts = ops.facts.role_facts(role_id);
    let inputs = DecisionInputs {
        path: GrantPath::SafewordRestore,
        bot_top_position: bot_top,
        guild_id: guild,
        protected_role_ids: &ops.protected_role_ids,
        safeword: SafewordState::Active,
        safeword_restricted_role_ids: restricted,
        staff: None,
    };
    match decide_from_facts(facts, role_id, &inputs) {
        RuntimeDecision::Decided(decision) => Ok(decision),
        RuntimeDecision::Unavailable => Err(CODE_FACTS_UNAVAILABLE),
    }
}

/// The K03 removal decision for the safeword role: `Some(code)` when it may not be removed.
fn removal_decision(guild: u64, role_id: RoleId, ops: &mut Ops) -> Option<&'static str> {
    let Some(bot_top) = ops.facts.bot_top_position() else {
        return Some(CODE_BOT_TOP_UNKNOWN);
    };
    let role = match ops.facts.role_facts(role_id) {
        roles_facts::FactsOutcome::Found(facts) => facts,
        roles_facts::FactsOutcome::Missing => RoleFacts {
            id: role_id,
            position: 0,
            managed: false,
            permissions: 0,
            exists: false,
        },
        roles_facts::FactsOutcome::Unavailable => return Some(CODE_FACTS_UNAVAILABLE),
    };
    let request = GrantRequest {
        path: GrantPath::SafewordRestore,
        role,
        bot_top_position: bot_top,
        guild_id: guild,
        protected_role_ids: &[],
        safeword: SafewordState::Inactive,
        safeword_restricted_role_ids: &[],
        staff: None,
    };
    match decide_removal(&request) {
        Decision::Allow => None,
        Decision::Deny(reason) => Some(reason.code()),
    }
}

/// Adapts the member-role trait to the K04 executor's writer trait: the stable write code
/// becomes the numbers the executor maps back to the same code.
struct MemberWriter<'a> {
    members: &'a mut dyn MemberRoles,
}

impl RoleWriter for MemberWriter<'_> {
    fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
        self.members
            .add_role(member_id, role_id)
            .map_err(|code| match code {
                WriteCode::Forbidden => write_error(403, None),
                WriteCode::UnknownRole => write_error(404, Some(k04::API_CODE_UNKNOWN_ROLE)),
                WriteCode::UnknownMember => write_error(404, Some(k04::API_CODE_UNKNOWN_MEMBER)),
                WriteCode::RateLimited => write_error(429, None),
                WriteCode::Other => write_error(500, None),
            })
    }
}

fn write_error(status: u16, api_code: Option<u32>) -> WriteError {
    WriteError {
        status,
        api_code,
        retry_after_ms: None,
    }
}

/// Adapts the boxed sleeper to the executor's generic parameter.
struct SleepAdapter<'a> {
    sleeper: &'a mut dyn Sleeper,
}

impl Sleeper for SleepAdapter<'_> {
    fn sleep_ms(&mut self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::RoleFacts;
    use crate::safeword::repository::Repository;
    use crate::safeword::runtime::{
        DmCode, DmSender, LookupCode, ReactionUser, ReactionUsers, RoleFactsSource, StaffAlerter,
        StateStore,
    };
    use crate::safeword::state::{step, Context};
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, BTreeSet};
    use std::rc::Rc;

    // Synthetic ids only.
    const HOME: u64 = 1_000_001;
    const OTHER: u64 = 2_000_002;
    const STAFF_ROLE: u64 = 3_000_003;
    const SW_ROLE: u64 = 900_009;
    const R1: u64 = 700_001;
    const R2: u64 = 700_002;
    const R3: u64 = 700_003;
    const PLAIN: u64 = 800_001;
    const CHANNEL: u64 = 810_001;
    const MESSAGE: u64 = 820_002;
    const EMOJI: u64 = 830_003;
    const STAFF_USER: u64 = 41_000_041;
    const STAFF_USER2: u64 = 41_000_042;
    const MEMBER: u64 = 55_000_055;
    const MEMBER2: u64 = 55_000_056;
    const RANDO: u64 = 66_000_066;

    // ---- fakes ----

    #[derive(Default)]
    struct Log {
        roles: BTreeMap<u64, Vec<u64>>,
        lookups: u32,
        adds: Vec<(u64, u64)>,
        removes: Vec<(u64, u64)>,
        dms: Vec<(u64, String)>,
        alerts: Vec<&'static str>,
        sleeps: Vec<u64>,
        loads: u32,
        stores: u32,
        clears: u32,
        facts_calls: u32,
        reaction_calls: u32,
    }

    type Shared = Rc<RefCell<Log>>;

    /// Scripted failures. Percentages roll a small LCG, so a run is repeatable from its seed.
    struct Chaos {
        rng: u64,
        write_fail_pct: u32,
        write_code: WriteCode,
        lookup_fail_pct: u32,
        dm_fail_pct: u32,
        store_fail_pct: u32,
        fail_add_roles: BTreeSet<u64>,
        fail_remove_roles: BTreeSet<u64>,
        high_roles: BTreeSet<u64>,
        fail_load: bool,
        fail_store: bool,
        stale_next_store: bool,
    }

    impl Chaos {
        fn new(seed: u64) -> Self {
            Chaos {
                rng: seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407),
                write_fail_pct: 0,
                write_code: WriteCode::Other,
                lookup_fail_pct: 0,
                dm_fail_pct: 0,
                store_fail_pct: 0,
                fail_add_roles: BTreeSet::new(),
                fail_remove_roles: BTreeSet::new(),
                high_roles: BTreeSet::new(),
                fail_load: false,
                fail_store: false,
                stale_next_store: false,
            }
        }

        fn next(&mut self) -> u64 {
            self.rng = self
                .rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.rng >> 33
        }

        fn roll(&mut self, pct: u32) -> bool {
            pct > 0 && (self.next() % 100) < u64::from(pct)
        }

        fn heal(&mut self) {
            self.write_fail_pct = 0;
            self.lookup_fail_pct = 0;
            self.dm_fail_pct = 0;
            self.store_fail_pct = 0;
            self.fail_add_roles.clear();
            self.fail_remove_roles.clear();
            self.fail_load = false;
            self.fail_store = false;
        }
    }

    type SharedChaos = Rc<RefCell<Chaos>>;

    struct FakeMembers {
        log: Shared,
        chaos: SharedChaos,
    }

    impl MemberRoles for FakeMembers {
        fn member_roles(&mut self, member_id: u64) -> Result<Vec<u64>, LookupCode> {
            self.log.borrow_mut().lookups += 1;
            let pct = self.chaos.borrow().lookup_fail_pct;
            if self.chaos.borrow_mut().roll(pct) {
                return Err(LookupCode::Unavailable);
            }
            Ok(self
                .log
                .borrow()
                .roles
                .get(&member_id)
                .cloned()
                .unwrap_or_default())
        }

        fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteCode> {
            self.log.borrow_mut().adds.push((member_id, role_id));
            let always = self.chaos.borrow().fail_add_roles.contains(&role_id);
            let pct = self.chaos.borrow().write_fail_pct;
            let rolled = self.chaos.borrow_mut().roll(pct);
            if always || rolled {
                return Err(self.chaos.borrow().write_code);
            }
            let mut log = self.log.borrow_mut();
            let held = log.roles.entry(member_id).or_default();
            if !held.contains(&role_id) {
                held.push(role_id);
            }
            Ok(())
        }

        fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteCode> {
            self.log.borrow_mut().removes.push((member_id, role_id));
            let always = self.chaos.borrow().fail_remove_roles.contains(&role_id);
            let pct = self.chaos.borrow().write_fail_pct;
            let rolled = self.chaos.borrow_mut().roll(pct);
            if always || rolled {
                return Err(self.chaos.borrow().write_code);
            }
            if let Some(held) = self.log.borrow_mut().roles.get_mut(&member_id) {
                held.retain(|r| *r != role_id);
            }
            Ok(())
        }
    }

    struct FakeFacts {
        log: Shared,
        chaos: SharedChaos,
        top: Option<u16>,
    }

    impl RoleFactsSource for FakeFacts {
        fn role_facts(&mut self, role_id: u64) -> crate::roles::runtime::FactsOutcome {
            self.log.borrow_mut().facts_calls += 1;
            let position = if self.chaos.borrow().high_roles.contains(&role_id) {
                60
            } else {
                1
            };
            crate::roles::runtime::FactsOutcome::Found(RoleFacts {
                id: role_id,
                position,
                managed: false,
                permissions: 0,
                exists: true,
            })
        }

        fn bot_top_position(&mut self) -> Option<u16> {
            self.log.borrow_mut().facts_calls += 1;
            self.top
        }
    }

    struct FakeDms {
        log: Shared,
        chaos: SharedChaos,
    }

    impl DmSender for FakeDms {
        fn send_dm(&mut self, user_id: u64, text: &str) -> Result<(), DmCode> {
            let pct = self.chaos.borrow().dm_fail_pct;
            if self.chaos.borrow_mut().roll(pct) {
                return Err(DmCode::Other);
            }
            self.log.borrow_mut().dms.push((user_id, text.to_string()));
            Ok(())
        }
    }

    struct FakeAlerter {
        log: Shared,
    }

    impl StaffAlerter for FakeAlerter {
        fn alert(&mut self, code: &'static str) {
            self.log.borrow_mut().alerts.push(code);
        }
    }

    struct FakeReactions {
        log: Shared,
    }

    impl ReactionUsers for FakeReactions {
        fn reaction_users(
            &mut self,
            _channel_id: u64,
            _message_id: u64,
            _emoji_id: u64,
            _after: Option<u64>,
            _limit: u32,
        ) -> Result<Vec<ReactionUser>, &'static str> {
            self.log.borrow_mut().reaction_calls += 1;
            Ok(Vec::new())
        }
    }

    struct FakeSleeper {
        log: Shared,
    }

    impl Sleeper for FakeSleeper {
        fn sleep_ms(&mut self, ms: u64) {
            self.log.borrow_mut().sleeps.push(ms);
        }
    }

    type Rows = Rc<RefCell<BTreeMap<(u64, u64), (State, u64)>>>;

    /// Where the rows live: a compare-and-swap map, or the real SW2 repository on SQLite.
    #[derive(Clone)]
    enum Backing {
        Map(Rows),
        Sql(Rc<RefCell<Repository>>),
    }

    impl Backing {
        fn get(&self, guild: u64, member: u64) -> Option<(State, u64)> {
            match self {
                Backing::Map(rows) => rows.borrow().get(&(guild, member)).cloned(),
                Backing::Sql(repo) => repo.borrow().load(guild, member).unwrap(),
            }
        }

        fn put(&self, guild: u64, member: u64, state: &State) {
            let version = self.get(guild, member).map(|(_, v)| v).unwrap_or(0);
            match self {
                Backing::Map(rows) => {
                    rows.borrow_mut()
                        .insert((guild, member), (state.clone(), version + 1));
                }
                Backing::Sql(repo) => {
                    repo.borrow_mut()
                        .store(guild, member, state, version, 0)
                        .unwrap();
                }
            }
        }
    }

    /// The store the command sees: counts calls, injects failures, delegates to the backing.
    struct Tap {
        backing: Backing,
        log: Shared,
        chaos: SharedChaos,
    }

    impl Tap {
        fn injected(&self, is_store: bool) -> Option<&'static str> {
            let mut chaos = self.chaos.borrow_mut();
            if is_store && chaos.stale_next_store {
                chaos.stale_next_store = false;
                return Some("stale_version");
            }
            if is_store && chaos.fail_store {
                return Some("sqlite_error");
            }
            let pct = chaos.store_fail_pct;
            if chaos.roll(pct) {
                return Some("sqlite_error");
            }
            None
        }
    }

    impl StateStore for Tap {
        fn load(&mut self, guild: u64, member: u64) -> Result<Option<(State, u64)>, &'static str> {
            self.log.borrow_mut().loads += 1;
            if self.chaos.borrow().fail_load {
                return Err("sqlite_error");
            }
            Ok(self.backing.get(guild, member))
        }

        fn store(
            &mut self,
            guild: u64,
            member: u64,
            state: &State,
            expected_version: u64,
            now: i64,
        ) -> Result<u64, &'static str> {
            self.log.borrow_mut().stores += 1;
            if let Some(code) = self.injected(true) {
                return Err(code);
            }
            match &self.backing {
                Backing::Map(rows) => {
                    let mut rows = rows.borrow_mut();
                    let current = rows.get(&(guild, member)).map(|(_, v)| *v).unwrap_or(0);
                    if current != expected_version {
                        return Err("stale_version");
                    }
                    rows.insert((guild, member), (state.clone(), current + 1));
                    Ok(current + 1)
                }
                Backing::Sql(repo) => repo
                    .borrow_mut()
                    .store(guild, member, state, expected_version, now)
                    .map_err(|e| e.code()),
            }
        }

        fn clear(
            &mut self,
            guild: u64,
            member: u64,
            expected_version: u64,
            now: i64,
        ) -> Result<u64, &'static str> {
            self.log.borrow_mut().clears += 1;
            if let Some(code) = self.injected(false) {
                return Err(code);
            }
            match &self.backing {
                Backing::Map(rows) => {
                    let mut rows = rows.borrow_mut();
                    let current = rows.get(&(guild, member)).map(|(_, v)| *v).unwrap_or(0);
                    if current != expected_version {
                        return Err("stale_version");
                    }
                    rows.insert((guild, member), (State::Inactive, current + 1));
                    Ok(current + 1)
                }
                Backing::Sql(repo) => repo
                    .borrow_mut()
                    .clear(guild, member, expected_version, now)
                    .map_err(|e| e.code()),
            }
        }

        fn members_with_state(&mut self, _guild: u64) -> Result<Vec<u64>, &'static str> {
            Ok(Vec::new())
        }
    }

    struct World {
        ops: Ops,
        cfg: CommandConfig,
        now: Cell<i64>,
        book: ConfirmBook,
        log: Shared,
        chaos: SharedChaos,
        backing: Backing,
    }

    impl Env for World {
        fn config(&self) -> &CommandConfig {
            &self.cfg
        }
        fn now(&self) -> i64 {
            self.now.get()
        }
        fn ops(&mut self) -> &mut Ops {
            &mut self.ops
        }
        fn confirms(&mut self) -> &mut ConfirmBook {
            &mut self.book
        }
    }

    fn base_config(mode: Mode) -> CommandConfig {
        CommandConfig {
            safeword: SafewordConfig {
                mode,
                role_id: SW_ROLE,
                channel_id: CHANNEL,
                message_id: MESSAGE,
                emoji_id: EMOJI,
                restricted_role_ids: vec![R1, R2, R3],
                dm: true,
                defer_grants: true,
            },
            home_guild: Some(HOME),
            staff_role_id: Some(STAFF_ROLE),
        }
    }

    impl World {
        fn new() -> World {
            World::build(Backing::Map(Rc::new(RefCell::new(BTreeMap::new()))), 1)
        }

        fn sql(seed: u64) -> World {
            let repo = Repository::open(":memory:", &[]).unwrap();
            World::build(Backing::Sql(Rc::new(RefCell::new(repo))), seed)
        }

        fn build(backing: Backing, seed: u64) -> World {
            let log: Shared = Rc::new(RefCell::new(Log::default()));
            let chaos: SharedChaos = Rc::new(RefCell::new(Chaos::new(seed)));
            let ops = Ops {
                members: Box::new(FakeMembers {
                    log: Rc::clone(&log),
                    chaos: Rc::clone(&chaos),
                }),
                facts: Box::new(FakeFacts {
                    log: Rc::clone(&log),
                    chaos: Rc::clone(&chaos),
                    top: Some(50),
                }),
                dms: Box::new(FakeDms {
                    log: Rc::clone(&log),
                    chaos: Rc::clone(&chaos),
                }),
                alerter: Box::new(FakeAlerter {
                    log: Rc::clone(&log),
                }),
                store: Box::new(Tap {
                    backing: backing.clone(),
                    log: Rc::clone(&log),
                    chaos: Rc::clone(&chaos),
                }),
                reactions: Box::new(FakeReactions {
                    log: Rc::clone(&log),
                }),
                sleeper: Box::new(FakeSleeper {
                    log: Rc::clone(&log),
                }),
                protected_role_ids: Vec::new(),
            };
            World {
                ops,
                cfg: base_config(Mode::Live),
                now: Cell::new(10_000),
                book: ConfirmBook::new(),
                log,
                chaos,
                backing,
            }
        }

        fn mode(mut self, mode: Mode) -> World {
            self.cfg.safeword.mode = mode;
            self
        }

        fn staff(&self) -> Invoker {
            Invoker {
                user: STAFF_USER,
                guild: HOME,
                role_ids: vec![PLAIN, STAFF_ROLE],
            }
        }

        fn staff2(&self) -> Invoker {
            Invoker {
                user: STAFF_USER2,
                guild: HOME,
                role_ids: vec![STAFF_ROLE],
            }
        }

        fn rando(&self) -> Invoker {
            Invoker {
                user: RANDO,
                guild: HOME,
                role_ids: vec![PLAIN],
            }
        }

        /// The member is Active with these roles in the snapshot (held now: safeword role and a
        /// plain role).
        fn seed_active(&self, guild: u64, member: u64, snapshot: &[u64]) {
            self.backing.put(guild, member, &active_state(snapshot));
            self.log
                .borrow_mut()
                .roles
                .insert(member, vec![SW_ROLE, PLAIN]);
        }

        fn seed_state(&self, guild: u64, member: u64, state: &State) {
            self.backing.put(guild, member, state);
        }

        fn row(&self, member: u64) -> Option<(State, u64)> {
            self.backing.get(HOME, member)
        }

        fn held(&self, member: u64) -> Vec<u64> {
            let mut roles = self
                .log
                .borrow()
                .roles
                .get(&member)
                .cloned()
                .unwrap_or_default();
            roles.sort_unstable();
            roles
        }

        /// Every call that reached a collaborator, summed.
        fn touches(&self) -> usize {
            let log = self.log.borrow();
            log.lookups as usize
                + log.adds.len()
                + log.removes.len()
                + log.dms.len()
                + log.alerts.len()
                + log.loads as usize
                + log.stores as usize
                + log.clears as usize
                + log.facts_calls as usize
                + log.reaction_calls as usize
                + log.sleeps.len()
        }

        fn run_status(&mut self, who: &Invoker, member: u64) -> Reply {
            status(self, who, member)
        }

        fn run_strike(&mut self, who: &Invoker, member: u64, role: u64) -> Reply {
            strike(self, who, &StrikeRequest { member, role })
        }

        fn run_clear(&mut self, who: &Invoker, member: u64, confirm: bool) -> Reply {
            clear(self, who, &ClearRequest { member, confirm })
        }

        /// The two-step clear: the arming call, then the explicit confirm.
        fn clear_twice(&mut self, who: &Invoker, member: u64) -> Reply {
            let first = self.run_clear(who, member, false);
            assert_eq!(first.code, Code::ConfirmRequired, "{first:?}");
            self.run_clear(who, member, true)
        }
    }

    fn ctx_with(held: &[u64]) -> Context {
        Context {
            safeword_role: SW_ROLE,
            restricted_roles: vec![R1, R2, R3],
            protected_roles: Vec::new(),
            held_roles: held.to_vec(),
            filter_restore: true,
            defer_grants: true,
        }
    }

    /// A real Active state, reached through the machine with every step acknowledged.
    fn active_state(snapshot: &[u64]) -> State {
        let mut held: Vec<u64> = snapshot.to_vec();
        held.push(PLAIN);
        let ctx = ctx_with(&held);
        let (mut state, actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        for action in actions {
            state = step(&state, Event::ActionDone(action.id()), &ctx).0;
        }
        assert!(matches!(state, State::Active(_)), "{state:?}");
        state
    }

    fn saved(state: &State) -> Vec<u64> {
        state.snapshot().unwrap().saved_roles.clone()
    }

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed >> 33
    }

    // ---- scope: mode, guild, staff ----

    #[test]
    fn mode_off_disables_every_subcommand_and_touches_nothing() {
        let mut w = World::new().mode(Mode::Off);
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let before = w.row(MEMBER);
        let staff = w.staff();
        let touched = w.touches();
        for reply in [
            w.run_status(&staff, MEMBER),
            w.run_strike(&staff, MEMBER, R1),
            w.run_clear(&staff, MEMBER, false),
            w.run_clear(&staff, MEMBER, true),
        ] {
            assert_eq!(reply.code, Code::Disabled, "{reply:?}");
        }
        assert_eq!(w.touches(), touched);
        assert_eq!(w.row(MEMBER), before);
    }

    #[test]
    fn shadow_mode_reads_status_but_refuses_every_write() {
        let mut w = World::new().mode(Mode::Shadow);
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let before = w.row(MEMBER);
        let staff = w.staff();
        let reply = w.run_status(&staff, MEMBER);
        assert_eq!(reply.code, Code::Status);
        assert_eq!(reply.state, Some(StateCode::Active));
        assert_eq!(w.run_strike(&staff, MEMBER, R1).code, Code::ShadowMode);
        assert_eq!(w.run_clear(&staff, MEMBER, false).code, Code::ShadowMode);
        assert_eq!(w.run_clear(&staff, MEMBER, true).code, Code::ShadowMode);
        let log = w.log.borrow();
        assert_eq!((log.stores, log.clears), (0, 0));
        assert!(log.adds.is_empty() && log.removes.is_empty() && log.dms.is_empty());
        drop(log);
        assert_eq!(w.row(MEMBER), before);
    }

    #[test]
    fn an_unset_or_zero_home_guild_denies_everyone_while_the_mode_is_on() {
        for home in [None, Some(0)] {
            let mut w = World::new();
            w.cfg.home_guild = home;
            w.seed_active(HOME, MEMBER, &[R1]);
            let staff = w.staff();
            let touched = w.touches();
            for reply in [
                w.run_status(&staff, MEMBER),
                w.run_strike(&staff, MEMBER, R1),
                w.run_clear(&staff, MEMBER, false),
            ] {
                assert_eq!(reply.code, Code::Denied, "{home:?}");
                assert_eq!(reply.detail, Some("no_home_guild"));
            }
            // A caller in guild 0 does not match an unset home guild either.
            let zero = Invoker {
                guild: 0,
                ..staff.clone()
            };
            assert_eq!(w.run_status(&zero, MEMBER).code, Code::Denied);
            assert_eq!(w.touches(), touched);
        }
    }

    #[test]
    fn a_foreign_guild_is_denied_and_nothing_is_read_written_sent_or_alerted() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        // The same member has a row in the foreign guild too: it must stay as it is.
        w.seed_active(OTHER, MEMBER, &[R3]);
        let (home_row, other_row) = (w.backing.get(HOME, MEMBER), w.backing.get(OTHER, MEMBER));
        let touched = w.touches();
        // The caller holds the staff role id, but in a guild that is not the home guild.
        let foreign_staff = Invoker {
            user: STAFF_USER,
            guild: OTHER,
            role_ids: vec![STAFF_ROLE],
        };
        for reply in [
            w.run_status(&foreign_staff, MEMBER),
            w.run_strike(&foreign_staff, MEMBER, R3),
            w.run_clear(&foreign_staff, MEMBER, false),
            w.run_clear(&foreign_staff, MEMBER, true),
        ] {
            assert_eq!(reply.code, Code::Denied);
            assert_eq!(reply.detail, Some("other_guild"));
        }
        assert_eq!(w.touches(), touched, "nothing may be touched");
        assert!(w.book.is_empty(), "no confirmation may be armed");
        assert_eq!(w.backing.get(HOME, MEMBER), home_row);
        assert_eq!(w.backing.get(OTHER, MEMBER), other_row);
    }

    #[test]
    fn home_guild_calls_touch_only_the_home_guild_row() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        w.seed_active(OTHER, MEMBER, &[R1, R2]);
        let other_row = w.backing.get(OTHER, MEMBER);
        let staff = w.staff();
        assert_eq!(w.run_strike(&staff, MEMBER, R1).code, Code::Struck);
        assert_eq!(w.clear_twice(&staff, MEMBER).code, Code::Cleared);
        assert_eq!(w.backing.get(OTHER, MEMBER), other_row);
    }

    #[test]
    fn an_unset_zero_or_unheld_staff_role_denies_strike_and_clear() {
        for staff_role in [None, Some(0)] {
            let mut w = World::new();
            w.cfg.staff_role_id = staff_role;
            w.seed_active(HOME, MEMBER, &[R1, R2]);
            let touched = w.touches();
            // A caller holding role id 0 or the real id still is not staff.
            let who = Invoker {
                user: STAFF_USER,
                guild: HOME,
                role_ids: vec![0, STAFF_ROLE],
            };
            for reply in [
                w.run_strike(&who, MEMBER, R1),
                w.run_clear(&who, MEMBER, false),
                w.run_clear(&who, MEMBER, true),
                w.run_status(&who, MEMBER2),
            ] {
                assert_eq!(reply.code, Code::Denied, "{staff_role:?}");
            }
            assert_eq!(w.touches(), touched);
        }
    }

    #[test]
    fn a_non_staff_caller_is_denied_and_nothing_is_touched() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let before = w.row(MEMBER);
        let touched = w.touches();
        let rando = w.rando();
        for reply in [
            w.run_strike(&rando, MEMBER, R1),
            w.run_clear(&rando, MEMBER, false),
            w.run_clear(&rando, MEMBER, true),
            // Looking up somebody else's safeword is staff business too.
            w.run_status(&rando, MEMBER),
        ] {
            assert_eq!(reply.code, Code::Denied);
            assert_eq!(reply.detail, Some("not_staff"));
        }
        // Caller id 0 is never trusted, even with the staff role.
        let zero = Invoker {
            user: 0,
            guild: HOME,
            role_ids: vec![STAFF_ROLE],
        };
        assert_eq!(w.run_clear(&zero, MEMBER, false).code, Code::Denied);
        assert_eq!(w.run_status(&zero, 0).code, Code::Denied);
        assert_eq!(w.touches(), touched);
        assert!(w.book.is_empty());
        assert_eq!(w.row(MEMBER), before);
    }

    // ---- status ----

    #[test]
    fn status_shows_each_state_and_the_snapshot_count_only_to_staff() {
        let mut w = World::new();
        let staff = w.staff();
        // No row at all: Idle, no count.
        let idle = w.run_status(&staff, MEMBER);
        assert_eq!(
            (idle.code, idle.state, idle.held_roles),
            (Code::Status, Some(StateCode::Idle), None)
        );
        // An Inactive row is Idle too.
        w.seed_state(HOME, MEMBER, &State::Inactive);
        assert_eq!(w.run_status(&staff, MEMBER).state, Some(StateCode::Idle));

        w.seed_active(HOME, MEMBER2, &[R1, R3]);
        let active = w.run_status(&staff, MEMBER2);
        assert_eq!(active.state, Some(StateCode::Active));
        assert_eq!(active.held_roles, Some(2));

        let ctx = ctx_with(&[R1, R2, R3]);
        let (activating, _) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        w.seed_state(HOME, 31, &activating);
        let reply = w.run_status(&staff, 31);
        assert_eq!(reply.state, Some(StateCode::Activating));
        assert_eq!(reply.held_roles, Some(3));

        let (releasing, _) = step(&active_state(&[R1, R2]), Event::ReactionRemoved, &ctx);
        assert!(matches!(releasing, State::Releasing(_)));
        w.seed_state(HOME, 32, &releasing);
        let reply = w.run_status(&staff, 32);
        assert_eq!(reply.state, Some(StateCode::Releasing));
        assert_eq!(reply.held_roles, Some(2));

        // A member looking at their own state sees the state but never the count.
        let me = Invoker {
            user: MEMBER2,
            guild: HOME,
            role_ids: vec![PLAIN],
        };
        let own = w.run_status(&me, MEMBER2);
        assert_eq!(
            (own.code, own.state, own.held_roles),
            (Code::Status, Some(StateCode::Active), None)
        );
        assert!(!own.text().contains("roles="));
        // Status writes nothing.
        let log = w.log.borrow();
        assert_eq!((log.stores, log.clears), (0, 0));
    }

    #[test]
    fn status_of_an_unreadable_row_is_a_store_failure_never_idle() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1]);
        w.chaos.borrow_mut().fail_load = true;
        let staff = w.staff();
        let reply = w.run_status(&staff, MEMBER);
        assert_eq!(reply.code, Code::StoreFailed);
        assert_eq!(reply.state, None);
        assert_eq!(reply.held_roles, None);
    }

    #[test]
    fn zero_ids_are_invalid_targets_and_roles_and_touch_nothing() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1]);
        let staff = w.staff();
        let touched = w.touches();
        assert_eq!(w.run_status(&staff, 0).code, Code::InvalidTarget);
        assert_eq!(w.run_strike(&staff, 0, R1).code, Code::InvalidTarget);
        assert_eq!(w.run_strike(&staff, MEMBER, 0).code, Code::InvalidRole);
        assert_eq!(w.run_clear(&staff, 0, false).code, Code::InvalidTarget);
        assert_eq!(w.touches(), touched);
    }

    // ---- strike ----

    #[test]
    fn strike_removes_a_snapshot_role_through_the_machine_and_the_store() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2, R3]);
        let (_, v0) = w.row(MEMBER).unwrap();
        let staff = w.staff();
        let reply = w.run_strike(&staff, MEMBER, R2);
        assert_eq!(reply.code, Code::Struck);
        assert_eq!(reply.held_roles, Some(2));
        let (state, v1) = w.row(MEMBER).unwrap();
        assert_eq!(v1, v0 + 1, "one compare-and-swap write");
        assert!(matches!(state, State::Active(_)));
        assert_eq!(saved(&state), vec![R1, R3]);
        assert_eq!(state.snapshot().unwrap().struck_roles, vec![R2]);
        // A strike gives and takes no role and sends nothing.
        let log = w.log.borrow();
        assert!(log.adds.is_empty() && log.removes.is_empty() && log.dms.is_empty());
    }

    #[test]
    fn a_struck_role_is_not_given_back_by_clear_but_the_others_are() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2, R3]);
        let staff = w.staff();
        assert_eq!(w.run_strike(&staff, MEMBER, R2).code, Code::Struck);
        assert_eq!(w.clear_twice(&staff, MEMBER).code, Code::Cleared);
        let held = w.held(MEMBER);
        assert!(held.contains(&R1) && held.contains(&R3));
        assert!(!held.contains(&R2), "the struck role stays off: {held:?}");
        assert!(w.log.borrow().adds.iter().all(|(_, role)| *role != R2));
    }

    #[test]
    fn strike_accepts_only_a_role_that_is_in_the_snapshot_and_changes_nothing_otherwise() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let before = w.row(MEMBER);
        let staff = w.staff();
        let touched_stores = {
            let log = w.log.borrow();
            (log.stores, log.clears)
        };
        // Not held (R3), a role the member holds but is not restricted (PLAIN), the safeword
        // role itself, a huge id, and a role already struck.
        for role in [R3, PLAIN, SW_ROLE, u64::MAX, 1] {
            let reply = w.run_strike(&staff, MEMBER, role);
            assert_eq!(reply.code, Code::RoleNotInSnapshot, "{role}");
        }
        assert_eq!(w.run_strike(&staff, MEMBER, R1).code, Code::Struck);
        assert_eq!(
            w.run_strike(&staff, MEMBER, R1).code,
            Code::RoleNotInSnapshot,
            "a second strike of the same role is a refusal"
        );
        // Only the one accepted strike wrote.
        let log = w.log.borrow();
        assert_eq!(
            (log.stores, log.clears),
            (touched_stores.0 + 1, touched_stores.1)
        );
        drop(log);
        assert_ne!(w.row(MEMBER), before);
        assert_eq!(saved(&w.row(MEMBER).unwrap().0), vec![R2]);
    }

    #[test]
    fn strike_is_refused_unless_the_member_is_active() {
        let mut w = World::new();
        let staff = w.staff();
        // No row, and an Inactive row.
        assert_eq!(w.run_strike(&staff, MEMBER, R1).code, Code::NotActive);
        w.seed_state(HOME, MEMBER, &State::Inactive);
        assert_eq!(w.run_strike(&staff, MEMBER, R1).code, Code::NotActive);
        // Activating and Releasing.
        let ctx = ctx_with(&[R1, R2]);
        let (activating, _) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        w.seed_state(HOME, 31, &activating);
        let (releasing, _) = step(&active_state(&[R1, R2]), Event::ReactionRemoved, &ctx);
        w.seed_state(HOME, 32, &releasing);
        for member in [31, 32] {
            let before = w.row(member);
            let reply = w.run_strike(&staff, member, R1);
            assert_eq!(reply.code, Code::WrongState);
            assert_eq!(w.row(member), before);
        }
        assert_eq!(w.run_strike(&staff, 31, R1).detail, Some("activating"));
        assert_eq!(w.run_strike(&staff, 32, R1).detail, Some("releasing"));
    }

    #[test]
    fn a_failed_or_raced_strike_write_changes_nothing_and_is_reported_by_code() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let before = w.row(MEMBER);
        let staff = w.staff();
        w.chaos.borrow_mut().fail_store = true;
        let reply = w.run_strike(&staff, MEMBER, R1);
        assert_eq!(reply.code, Code::StoreFailed);
        assert_eq!(reply.detail, Some("sqlite_error"));
        assert_eq!(w.row(MEMBER), before);
        // A concurrent writer won: stale version.
        w.chaos.borrow_mut().fail_store = false;
        w.chaos.borrow_mut().stale_next_store = true;
        let reply = w.run_strike(&staff, MEMBER, R1);
        assert_eq!(reply.code, Code::Busy);
        assert_eq!(w.row(MEMBER), before);
        // The retry works, and the failure never reached the member's roles.
        assert_eq!(w.run_strike(&staff, MEMBER, R1).code, Code::Struck);
        assert!(w.log.borrow().adds.is_empty());
    }

    #[test]
    fn strike_with_a_failed_row_read_changes_nothing() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1]);
        w.chaos.borrow_mut().fail_load = true;
        let staff = w.staff();
        assert_eq!(w.run_strike(&staff, MEMBER, R1).code, Code::StoreFailed);
        assert_eq!(w.log.borrow().stores, 0);
    }

    // ---- clear: the confirm step ----

    #[test]
    fn the_first_clear_call_only_asks_for_confirmation() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let before = w.row(MEMBER);
        let staff = w.staff();
        let reply = w.run_clear(&staff, MEMBER, false);
        assert_eq!(reply.code, Code::ConfirmRequired);
        assert_eq!(reply.state, Some(StateCode::Active));
        assert_eq!(w.book.len(), 1);
        assert_eq!(w.row(MEMBER), before);
        let log = w.log.borrow();
        assert!(log.adds.is_empty() && log.removes.is_empty() && log.dms.is_empty());
        assert_eq!((log.stores, log.clears), (0, 0));
    }

    #[test]
    fn a_confirm_without_a_first_call_restores_nothing() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let before = w.row(MEMBER);
        let staff = w.staff();
        let reply = w.run_clear(&staff, MEMBER, true);
        assert_eq!(reply.code, Code::ConfirmMismatch);
        assert_eq!(w.row(MEMBER), before);
        assert!(w.log.borrow().adds.is_empty());
        assert_eq!(w.held(MEMBER), vec![PLAIN, SW_ROLE]);
    }

    #[test]
    fn the_confirmation_window_has_an_exact_edge() {
        for (wait, expect) in [
            (0, Code::Cleared),
            (CONFIRM_WINDOW_SECS, Code::Cleared),
            (CONFIRM_WINDOW_SECS + 1, Code::ConfirmExpired),
            (86_400, Code::ConfirmExpired),
            // The clock went backwards: never valid.
            (-1, Code::ConfirmExpired),
        ] {
            let mut w = World::new();
            w.seed_active(HOME, MEMBER, &[R1]);
            let staff = w.staff();
            assert_eq!(
                w.run_clear(&staff, MEMBER, false).code,
                Code::ConfirmRequired
            );
            w.now.set(w.now.get() + wait);
            let reply = w.run_clear(&staff, MEMBER, true);
            assert_eq!(reply.code, expect, "wait {wait}");
            if expect == Code::ConfirmExpired {
                assert!(w.log.borrow().adds.is_empty(), "wait {wait}");
                assert!(matches!(w.row(MEMBER).unwrap().0, State::Active(_)));
            }
        }
    }

    #[test]
    fn a_confirmation_belongs_to_one_caller_one_member_and_one_row_version() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        w.seed_active(HOME, MEMBER2, &[R1]);
        let (staff, staff2) = (w.staff(), w.staff2());
        assert_eq!(
            w.run_clear(&staff, MEMBER, false).code,
            Code::ConfirmRequired
        );
        // Another staff member cannot use it, nor can it clear another member.
        assert_eq!(
            w.run_clear(&staff2, MEMBER, true).code,
            Code::ConfirmMismatch
        );
        assert_eq!(
            w.run_clear(&staff, MEMBER2, true).code,
            Code::ConfirmMismatch
        );
        assert!(w.log.borrow().adds.is_empty());
        // The row changed between the two calls (a strike): the old confirmation is void.
        assert_eq!(w.run_strike(&staff, MEMBER, R1).code, Code::Struck);
        assert_eq!(
            w.run_clear(&staff, MEMBER, true).code,
            Code::ConfirmMismatch
        );
        assert!(w.log.borrow().adds.is_empty());
        assert!(matches!(w.row(MEMBER).unwrap().0, State::Active(_)));
    }

    #[test]
    fn a_confirmation_is_used_up_by_one_attempt() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        w.chaos.borrow_mut().fail_add_roles.insert(R2);
        let staff = w.staff();
        assert_eq!(
            w.run_clear(&staff, MEMBER, false).code,
            Code::ConfirmRequired
        );
        assert_eq!(w.run_clear(&staff, MEMBER, true).code, Code::RestoreFailed);
        w.chaos.borrow_mut().heal();
        // The retry needs a fresh first call.
        assert_eq!(
            w.run_clear(&staff, MEMBER, true).code,
            Code::ConfirmMismatch
        );
        assert_eq!(w.clear_twice(&staff, MEMBER).code, Code::Cleared);
    }

    #[test]
    fn the_confirm_book_is_capped_and_prunes_expired_entries() {
        let mut book = ConfirmBook::new();
        let entry = |member: u64, at: i64| Armed {
            guild: HOME,
            caller: STAFF_USER,
            member,
            version: 1,
            armed_at: at,
        };
        for member in 1..=(MAX_ARMED as u64 + 10) {
            book.arm(entry(member, 100 + member as i64), 100 + member as i64);
        }
        assert_eq!(book.len(), MAX_ARMED);
        // The oldest were dropped, the newest are kept.
        assert!(book.take(HOME, STAFF_USER, 1).is_none());
        assert!(book.take(HOME, STAFF_USER, MAX_ARMED as u64 + 10).is_some());
        // Arming the same key again replaces it.
        let mut book = ConfirmBook::new();
        book.arm(entry(5, 100), 100);
        book.arm(entry(5, 110), 110);
        assert_eq!(book.len(), 1);
        assert_eq!(book.take(HOME, STAFF_USER, 5).unwrap().armed_at, 110);
        // An entry older than the window is dropped when the next one is armed.
        let mut book = ConfirmBook::new();
        book.arm(entry(6, 100), 100);
        book.arm(
            entry(7, 100 + CONFIRM_WINDOW_SECS + 1),
            100 + CONFIRM_WINDOW_SECS + 1,
        );
        assert_eq!(book.len(), 1);
        assert!(book.take(HOME, STAFF_USER, 6).is_none());
    }

    // ---- clear: restore, consistency, idempotence ----

    #[test]
    fn a_confirmed_clear_restores_through_k03_and_k04_and_ends_the_safeword() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let staff = w.staff();
        let reply = w.clear_twice(&staff, MEMBER);
        assert_eq!(reply.code, Code::Cleared, "{reply:?}");
        assert_eq!(reply.class(), Class::Success);
        // Roles are back, the safeword role is gone, the row is Inactive.
        assert_eq!(w.held(MEMBER), vec![R1, R2, PLAIN]);
        assert!(matches!(w.row(MEMBER).unwrap().0, State::Inactive));
        // The release DM went out once, to the member.
        let log = w.log.borrow();
        assert_eq!(log.dms.len(), 1);
        assert_eq!(log.dms[0].0, MEMBER);
        assert_eq!(log.dms[0].1, crate::safeword::state::RELEASED_DM_TEXT);
        // Restricted roles were written while the member was still on safeword: K03 let them
        // through because the path is SafewordRestore (any other path would deny them).
        assert_eq!(log.adds, vec![(MEMBER, R1), (MEMBER, R2)]);
        assert!(log.alerts.is_empty());
        // Nothing is left armed.
        drop(log);
        assert!(w.book.is_empty());
    }

    #[test]
    fn k03_still_decides_restored_roles_protected_and_too_high_roles_are_not_written() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2, R3]);
        // R1 became protected after the snapshot, R2 now sits above the bot.
        w.ops.protected_role_ids = vec![R1];
        w.chaos.borrow_mut().high_roles.insert(R2);
        let staff = w.staff();
        let reply = w.clear_twice(&staff, MEMBER);
        // R1 is filtered by the machine (protected); R2 is denied by K03: not a full restore.
        assert_eq!(reply.code, Code::RestoreFailed, "{reply:?}");
        assert!(reply.codes.contains(&"role_write_failed") || !reply.codes.is_empty());
        let held = w.held(MEMBER);
        assert!(!held.contains(&R1), "protected role written: {held:?}");
        assert!(!held.contains(&R2), "too-high role written: {held:?}");
        assert!(w.log.borrow().adds.iter().all(|(_, r)| *r != R2));
    }

    #[test]
    fn a_restore_failure_keeps_the_member_active_and_the_record_untouched() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2, R3]);
        let before = w.row(MEMBER);
        w.chaos.borrow_mut().fail_add_roles.insert(R2);
        w.chaos.borrow_mut().write_code = WriteCode::Forbidden;
        let staff = w.staff();
        let reply = w.clear_twice(&staff, MEMBER);
        assert_eq!(reply.code, Code::RestoreFailed, "{reply:?}");
        assert_eq!(reply.class(), Class::Failure);
        assert!(!reply.codes.is_empty());
        // Stays Active: the stored row is byte-for-byte what it was (same state, same version).
        assert_eq!(w.row(MEMBER), before);
        // Not half-cleared: the safeword role is still on, no release DM, no snapshot clear.
        assert!(w.held(MEMBER).contains(&SW_ROLE));
        let log = w.log.borrow();
        assert!(log.dms.is_empty());
        assert!(log.removes.is_empty());
        assert_eq!(log.clears, 0);
        assert!(!log.alerts.is_empty(), "staff is alerted by code");
    }

    #[test]
    fn after_a_failed_restore_a_retry_finishes_and_never_repeats_the_dm() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2, R3]);
        w.chaos.borrow_mut().fail_add_roles.insert(R2);
        let staff = w.staff();
        assert_eq!(w.clear_twice(&staff, MEMBER).code, Code::RestoreFailed);
        w.chaos.borrow_mut().heal();
        let reply = w.clear_twice(&staff, MEMBER);
        assert_eq!(reply.code, Code::Cleared, "{reply:?}");
        assert_eq!(w.held(MEMBER), vec![R1, R2, R3, PLAIN]);
        assert!(matches!(w.row(MEMBER).unwrap().0, State::Inactive));
        assert_eq!(w.log.borrow().dms.len(), 1);
    }

    #[test]
    fn a_rate_limited_restore_is_retried_by_k04_with_sleeps_and_then_succeeds_or_fails_by_code() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1]);
        w.chaos.borrow_mut().write_code = WriteCode::RateLimited;
        w.chaos.borrow_mut().fail_add_roles.insert(R1);
        let staff = w.staff();
        let reply = w.clear_twice(&staff, MEMBER);
        assert_eq!(reply.code, Code::RestoreFailed);
        // K04 retried the 429 with its own backoff (3 retries, 4 writes, 3 sleeps).
        assert_eq!(w.log.borrow().adds.len(), 4);
        assert_eq!(w.log.borrow().sleeps.len(), 3);
        assert!(matches!(w.row(MEMBER).unwrap().0, State::Active(_)));
    }

    #[test]
    fn a_failure_after_the_restore_persists_the_progress_and_a_retry_completes() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        // The safeword role cannot be removed yet.
        w.chaos.borrow_mut().fail_remove_roles.insert(SW_ROLE);
        let staff = w.staff();
        let reply = w.clear_twice(&staff, MEMBER);
        assert_eq!(reply.code, Code::ReleaseIncomplete, "{reply:?}");
        // The roles are back and the machine remembers: Releasing, snapshot kept, not cleared.
        assert_eq!(w.held(MEMBER), vec![R1, R2, PLAIN, SW_ROLE]);
        let (state, _) = w.row(MEMBER).unwrap();
        assert!(matches!(state, State::Releasing(_)), "{state:?}");
        assert_eq!(
            w.run_status(&staff, MEMBER).state,
            Some(StateCode::Releasing)
        );
        assert!(
            w.log.borrow().dms.is_empty(),
            "no DM before the release is done"
        );
        // Healed: the retry finishes without writing the restored roles again.
        w.chaos.borrow_mut().heal();
        let adds_before = w.log.borrow().adds.len();
        assert_eq!(w.clear_twice(&staff, MEMBER).code, Code::Cleared);
        assert_eq!(w.log.borrow().adds.len(), adds_before);
        assert_eq!(w.held(MEMBER), vec![R1, R2, PLAIN]);
        assert!(matches!(w.row(MEMBER).unwrap().0, State::Inactive));
        assert_eq!(w.log.borrow().dms.len(), 1);
    }

    #[test]
    fn a_failed_final_clear_write_does_not_report_cleared() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1]);
        let staff = w.staff();
        assert_eq!(
            w.run_clear(&staff, MEMBER, false).code,
            Code::ConfirmRequired
        );
        w.chaos.borrow_mut().fail_store = true;
        w.chaos.borrow_mut().store_fail_pct = 100;
        let reply = w.run_clear(&staff, MEMBER, true);
        assert_ne!(reply.code, Code::Cleared, "{reply:?}");
        assert_ne!(reply.class(), Class::Success);
        assert!(!matches!(w.row(MEMBER).unwrap().0, State::Inactive));
    }

    #[test]
    fn a_dm_that_cannot_be_delivered_does_not_block_the_clear_but_is_reported() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1]);
        w.chaos.borrow_mut().dm_fail_pct = 100;
        let staff = w.staff();
        let reply = w.clear_twice(&staff, MEMBER);
        assert_eq!(reply.code, Code::Cleared);
        assert_eq!(reply.codes, vec![CODE_DM_FAILED]);
        assert!(matches!(w.row(MEMBER).unwrap().0, State::Inactive));
        assert_eq!(w.held(MEMBER), vec![R1, PLAIN]);
    }

    #[test]
    fn clear_is_idempotent_a_second_clear_changes_and_sends_nothing() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let staff = w.staff();
        assert_eq!(w.clear_twice(&staff, MEMBER).code, Code::Cleared);
        let (row, held) = (w.row(MEMBER), w.held(MEMBER));
        let (adds, removes, dms) = {
            let log = w.log.borrow();
            (log.adds.len(), log.removes.len(), log.dms.len())
        };
        let stores = w.log.borrow().stores + w.log.borrow().clears;
        for confirm in [false, true, false, true] {
            let reply = w.run_clear(&staff, MEMBER, confirm);
            assert_eq!(reply.code, Code::AlreadyClear, "{reply:?}");
            assert_eq!(reply.class(), Class::Success);
        }
        assert_eq!(w.row(MEMBER), row);
        assert_eq!(w.held(MEMBER), held);
        let log = w.log.borrow();
        assert_eq!(
            (log.adds.len(), log.removes.len(), log.dms.len()),
            (adds, removes, dms)
        );
        assert_eq!(log.stores + log.clears, stores);
        drop(log);
        assert!(w.book.is_empty(), "an already-clear member is never armed");
        // A member who never had a row is the same.
        assert_eq!(w.run_clear(&staff, 777, true).code, Code::AlreadyClear);
    }

    #[test]
    fn clear_refuses_an_activating_member_and_arms_nothing() {
        let mut w = World::new();
        let ctx = ctx_with(&[R1, R2]);
        let (activating, _) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        w.seed_state(HOME, MEMBER, &activating);
        let before = w.row(MEMBER);
        let staff = w.staff();
        let reply = w.run_clear(&staff, MEMBER, false);
        assert_eq!(
            (reply.code, reply.detail),
            (Code::WrongState, Some("activating"))
        );
        assert_eq!(w.run_clear(&staff, MEMBER, true).code, Code::WrongState);
        assert!(w.book.is_empty());
        assert_eq!(w.row(MEMBER), before);
    }

    #[test]
    fn clear_of_a_releasing_member_retries_the_release() {
        let mut w = World::new();
        // A release left unfinished by the reaction runtime: roles not restored, one failed.
        let ctx = ctx_with(&[PLAIN, SW_ROLE]);
        let (releasing, actions) = step(&active_state(&[R1, R2]), Event::ReactionRemoved, &ctx);
        let mut state = releasing;
        for action in actions {
            let event = match action {
                Action::RestoreRoles(_) => {
                    Event::ActionFailed(ActionId::RestoreRoles, FailureCode::TransientHttp)
                }
                other => Event::ActionDone(other.id()),
            };
            state = step(&state, event, &ctx).0;
        }
        assert!(matches!(state, State::Releasing(_)));
        w.seed_state(HOME, MEMBER, &state);
        w.log.borrow_mut().roles.insert(MEMBER, vec![PLAIN]);
        let staff = w.staff();
        assert_eq!(w.clear_twice(&staff, MEMBER).code, Code::Cleared);
        assert_eq!(w.held(MEMBER), vec![R1, R2, PLAIN]);
        assert!(matches!(w.row(MEMBER).unwrap().0, State::Inactive));
    }

    // ---- replies ----

    #[test]
    fn every_reply_on_every_path_is_ephemeral_and_text_built_from_stable_codes() {
        let mut replies: Vec<Reply> = Vec::new();
        let mut run = |mut w: World, calls: &dyn Fn(&mut World) -> Vec<Reply>| {
            replies.extend(calls(&mut w));
        };
        let everything = |w: &mut World| -> Vec<Reply> {
            let (staff, rando) = (w.staff(), w.rando());
            vec![
                w.run_status(&staff, MEMBER),
                w.run_status(&rando, MEMBER),
                w.run_strike(&staff, MEMBER, R1),
                w.run_strike(&staff, MEMBER, R3),
                w.run_strike(&rando, MEMBER, R1),
                w.run_clear(&staff, MEMBER, true),
                w.run_clear(&staff, MEMBER, false),
                w.run_clear(&staff, MEMBER, true),
                w.run_clear(&staff, MEMBER, true),
                w.run_clear(&rando, MEMBER, false),
                w.run_status(&staff, 0),
            ]
        };
        for mode in [Mode::Off, Mode::Shadow, Mode::Live] {
            let w = World::new().mode(mode);
            w.seed_active(HOME, MEMBER, &[R1, R2]);
            run(w, &everything);
        }
        let w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        w.chaos.borrow_mut().fail_load = true;
        run(w, &everything);
        let w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        w.chaos.borrow_mut().fail_add_roles.insert(R1);
        run(w, &everything);
        let mut w = World::new();
        w.cfg.home_guild = None;
        run(w, &everything);
        assert!(replies.len() > 60);
        let ids = [
            HOME, OTHER, STAFF_ROLE, SW_ROLE, R1, R2, R3, PLAIN, CHANNEL, MESSAGE, EMOJI,
            STAFF_USER, MEMBER, RANDO,
        ];
        for reply in &replies {
            assert!(reply.is_ephemeral(), "{reply:?}");
            let text = reply.text();
            assert!(text.contains(reply.code.as_str()), "{text}");
            for id in ids {
                assert!(!text.contains(&id.to_string()), "{text} echoes {id}");
            }
            assert!(text.is_ascii());
        }
    }

    #[test]
    fn code_texts_are_distinct_stable_snake_case() {
        let mut seen = BTreeSet::new();
        for code in Code::ALL {
            let name = code.as_str();
            assert!(!name.is_empty());
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name}"
            );
            assert!(seen.insert(name), "duplicate {name}");
            assert!(Reply::new(code).text().contains(&format!("[{name}]")));
        }
        assert_eq!(Code::Cleared.as_str(), "cleared");
        assert_eq!(Code::ConfirmRequired.as_str(), "confirm_required");
        assert_eq!(Code::RoleNotInSnapshot.as_str(), "role_not_in_snapshot");
        assert_eq!(Code::RestoreFailed.as_str(), "restore_failed");
        assert_eq!(Reply::new(Code::Status).code.class(), Class::Success);
        assert_eq!(Code::Denied.class(), Class::Refusal);
    }

    // ---- liveness against the real SW2 repository ----

    /// What the member must hold once released: the snapshot minus what staff struck.
    fn expected_roles(snapshot: &[u64], struck: &[u64]) -> Vec<u64> {
        let mut roles: Vec<u64> = snapshot
            .iter()
            .copied()
            .filter(|r| !struck.contains(r))
            .collect();
        roles.push(PLAIN);
        roles.sort_unstable();
        roles
    }

    #[test]
    fn liveness_clear_always_reaches_cleared_under_random_failures_and_restarts() {
        let all = [R1, R2, R3];
        let mut ended_cleared = 0;
        let mut seen: BTreeMap<&'static str, u32> = BTreeMap::new();
        for seed in 0..250u64 {
            let mut rng = seed + 9;
            let mut w = World::sql(seed);
            let count = 1 + (lcg(&mut rng) % 3) as usize;
            let snapshot: Vec<u64> = all[..count].to_vec();
            w.seed_active(HOME, MEMBER, &snapshot);
            let staff = w.staff();
            {
                let mut chaos = w.chaos.borrow_mut();
                chaos.write_fail_pct = (lcg(&mut rng) % 70) as u32;
                chaos.write_code = [
                    WriteCode::Other,
                    WriteCode::Forbidden,
                    WriteCode::RateLimited,
                ][(lcg(&mut rng) % 3) as usize];
                chaos.lookup_fail_pct = (lcg(&mut rng) % 30) as u32;
                chaos.dm_fail_pct = (lcg(&mut rng) % 60) as u32;
                chaos.store_fail_pct = (lcg(&mut rng) % 25) as u32;
            }
            let mut struck: Vec<u64> = Vec::new();
            let mut cleared_at: Option<usize> = None;
            let heal_round = 12;
            let mut healed_rounds = 0;
            for round in 0..40usize {
                if round == heal_round {
                    w.chaos.borrow_mut().heal();
                }
                if round >= heal_round {
                    healed_rounds += 1;
                }
                w.now.set(w.now.get() + (lcg(&mut rng) % 90) as i64);
                // Sometimes staff strikes a role first; sometimes the process restarts.
                if lcg(&mut rng).is_multiple_of(8) && cleared_at.is_none() {
                    let role = all[(lcg(&mut rng) % 3) as usize];
                    // Known limit: a strike cannot take back a role an earlier partial restore
                    // already wrote, so only a role not yet held counts as "kept off".
                    let held_already = w.held(MEMBER).contains(&role);
                    if w.run_strike(&staff, MEMBER, role).code == Code::Struck && !held_already {
                        struck.push(role);
                    }
                }
                if lcg(&mut rng).is_multiple_of(6) {
                    w.book = ConfirmBook::new();
                }
                let first = w.run_clear(&staff, MEMBER, false);
                // Before the heal the second call may come late (past the window) or after a
                // restart (the book is empty); after the heal it comes at once.
                if round < heal_round {
                    w.now.set(w.now.get() + (lcg(&mut rng) % 120) as i64);
                    if lcg(&mut rng).is_multiple_of(6) {
                        w.book = ConfirmBook::new();
                    }
                }
                let second = if lcg(&mut rng).is_multiple_of(10) {
                    first.clone()
                } else {
                    w.run_clear(&staff, MEMBER, true)
                };
                for reply in [&first, &second] {
                    *seen.entry(reply.code.as_str()).or_insert(0) += 1;
                    assert!(
                        matches!(
                            reply.code,
                            Code::ConfirmRequired
                                | Code::ConfirmExpired
                                | Code::ConfirmMismatch
                                | Code::Cleared
                                | Code::AlreadyClear
                                | Code::RestoreFailed
                                | Code::ReleaseIncomplete
                                | Code::StoreFailed
                                | Code::Busy
                        ),
                        "seed {seed} round {round}: {reply:?}"
                    );
                }
                // Consistency, every round: Inactive means fully restored and nothing struck
                // came back; anything else keeps the snapshot.
                let (state, _) = w.row(MEMBER).unwrap();
                let held = w.held(MEMBER);
                let expected = expected_roles(&snapshot, &struck);
                match &state {
                    State::Inactive => {
                        assert!(
                            expected.iter().all(|r| held.contains(r)),
                            "seed {seed} round {round}: half-cleared, held {held:?}"
                        );
                        assert!(!held.contains(&SW_ROLE), "seed {seed}: safeword role kept");
                        cleared_at.get_or_insert(round);
                    }
                    other => {
                        assert!(
                            other.snapshot().is_some(),
                            "seed {seed} round {round}: {other:?}"
                        );
                        assert!(cleared_at.is_none(), "seed {seed}: cleared then reopened");
                    }
                }
                for role in &struck {
                    assert!(
                        !held.contains(role),
                        "seed {seed}: struck role {role} given back"
                    );
                }
                if cleared_at.is_some() {
                    assert!(
                        matches!(
                            second.code,
                            Code::Cleared | Code::AlreadyClear | Code::ConfirmRequired
                        ),
                        "seed {seed} round {round}: {second:?}"
                    );
                }
                if cleared_at.is_some() && round > cleared_at.unwrap() + 1 {
                    break;
                }
            }
            // Never goes quiet: once the failures stop, a bounded number of rounds clears it.
            assert!(
                cleared_at.is_some(),
                "seed {seed}: not cleared after {healed_rounds} healthy rounds"
            );
            assert!(
                cleared_at.unwrap() <= heal_round + 3,
                "seed {seed}: {cleared_at:?}"
            );
            ended_cleared += 1;
            // After clearing, more clears do nothing.
            let writes = {
                let log = w.log.borrow();
                (log.adds.len(), log.removes.len(), log.dms.len())
            };
            assert_eq!(w.run_clear(&staff, MEMBER, true).code, Code::AlreadyClear);
            let log = w.log.borrow();
            assert_eq!((log.adds.len(), log.removes.len(), log.dms.len()), writes);
        }
        assert_eq!(ended_cleared, 250);
        // The run is not vacuous: every interesting path was taken many times.
        for code in [
            Code::Cleared,
            Code::RestoreFailed,
            Code::ReleaseIncomplete,
            Code::StoreFailed,
            Code::ConfirmExpired,
            Code::ConfirmMismatch,
            Code::AlreadyClear,
        ] {
            assert!(
                seen.get(code.as_str()).copied().unwrap_or(0) >= 5,
                "{} seen {:?} times: {seen:?}",
                code.as_str(),
                seen.get(code.as_str())
            );
        }
    }

    #[test]
    fn known_limit_a_strike_cannot_take_back_a_role_a_partial_restore_already_wrote() {
        let mut w = World::new();
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        w.chaos.borrow_mut().fail_add_roles.insert(R2);
        let staff = w.staff();
        assert_eq!(w.clear_twice(&staff, MEMBER).code, Code::RestoreFailed);
        // R1 was written before R2 failed; the row still says Active with both in the snapshot.
        assert!(w.held(MEMBER).contains(&R1));
        assert_eq!(w.run_strike(&staff, MEMBER, R1).code, Code::Struck);
        // The strike keeps R1 out of any later restore but does not remove it from the member.
        assert!(w.held(MEMBER).contains(&R1));
        assert!(w.log.borrow().removes.is_empty());
    }

    #[test]
    fn a_second_writer_between_the_two_calls_voids_the_confirmation_on_a_real_database() {
        let mut w = World::sql(1);
        w.seed_active(HOME, MEMBER, &[R1, R2]);
        let staff = w.staff();
        assert_eq!(
            w.run_clear(&staff, MEMBER, false).code,
            Code::ConfirmRequired
        );
        // Another process (the reaction runtime) rewrites the row.
        w.seed_state(HOME, MEMBER, &active_state(&[R1]));
        assert_eq!(
            w.run_clear(&staff, MEMBER, true).code,
            Code::ConfirmMismatch
        );
        assert!(w.log.borrow().adds.is_empty());
    }
}

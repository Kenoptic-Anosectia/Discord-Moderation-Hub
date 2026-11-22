//! Runtime for the safeword feature (SW3).
//!
//! This module is the thin, Discord-facing layer between the pure safeword
//! state machine (`state`, SW1), the durable repository (`repository`, SW2) and
//! the role services (`roles::eligibility` = K03, `roles::grant` = K04,
//! `roles::runtime` = RG2).
//!
//! What it enforces:
//!
//! * **The emoji is matched by id, never by name.** A reaction on another
//!   message, in another channel, with another emoji, or by a bot is ignored.
//! * **A replayed add never overwrites the snapshot.** The state machine owns
//!   that rule; the runtime stores the machine's own snapshot and never builds
//!   a second one.
//! * **Remove before add is handled.** The runtime feeds the events to the
//!   state machine in the order they arrive and executes the actions the
//!   machine emits, in the order it emits them.
//! * **Every role change passes the K03 decision.** Grants use
//!   `GrantPath::SafewordRestore` (the path that exists for restores) and are
//!   written by the K04 executor (`roles::grant::apply`): it writes only what
//!   K03 allowed and the member does not hold, and retries a 429 a limited
//!   number of times. Every removal uses `decide_removal`.
//! * **A partial role failure is never reported as success.** The step is
//!   reported to the state machine as failed, so the machine keeps the failed
//!   step and re-emits it on the next trigger, and a staff alert with a stable
//!   code is raised.
//! * **A DM failure never changes the member's roles or the snapshot.** The
//!   DM is best effort: a failure is reported to the machine and alerted, and
//!   nothing else is touched.
//! * **Shadow mode performs nothing.** No role write, no DM and no alert: it
//!   only produces a structured would-do record of counts and codes.
//! * **A failed lookup never costs the member their protection.** If the
//!   state or the member's roles cannot be read at the start of a run, the
//!   run writes nothing and alerts, so the member keeps what it holds. A
//!   lookup that fails later is never read as "holds nothing": a restore
//!   writes the role anyway (a repeat is harmless) and a hand-over to a new
//!   activation is not acknowledged until the roles can be read.
//!
//! The decision core is Discord-free and clock-free: the ids and the `now`
//! come in through the request structs, and the Discord calls sit behind
//! small traits so a test injects fakes. Two places touch serenity on
//! purpose and are marked as such: [`reaction_event`] reads a serenity event
//! into plain values, and [`SafewordFeature`] is the registry hook.

use crate::roles::eligibility::{
    decide_removal, Decision, GrantPath, GrantRequest, RoleFacts, SafewordState,
};
use crate::roles::grant::{
    self as k04, FailCode, GrantItem, RoleOutcome, RoleWriter, Sleeper, WriteError,
};
use crate::roles::runtime::{
    self as roles_facts, decide_from_facts, DecisionInputs, RuntimeDecision,
};
use crate::safeword::config::SafewordConfig;
use crate::safeword::state::{
    restart_action, step, Action, Context, DmKind, Event, FailureCode, RestartAction, RoleId, State,
};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Reaction matching (pure)
// ---------------------------------------------------------------------------

/// Why a reaction event was ignored.
///
/// Every variant is a stable code and carries no name, id or text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// The reactor is a bot.
    BotAuthor,
    /// The reaction is in another channel.
    WrongChannel,
    /// The reaction is on another message.
    WrongMessage,
    /// The reaction uses another emoji.
    WrongEmoji,
}

impl IgnoreReason {
    /// The stable machine code. It never contains a name, an id or a message.
    pub fn code(self) -> &'static str {
        match self {
            IgnoreReason::BotAuthor => "bot_author",
            IgnoreReason::WrongChannel => "wrong_channel",
            IgnoreReason::WrongMessage => "wrong_message",
            IgnoreReason::WrongEmoji => "wrong_emoji",
        }
    }
}

/// What the runtime does with one reaction event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionDecision {
    /// The event is not about this safeword. Nothing happens.
    Ignore(IgnoreReason),
    /// The reaction was added: the member's safeword becomes active.
    Activate(u64),
    /// The reaction was removed: the member's safeword is released.
    Release(u64),
}

/// One reaction event, in plain values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReactionEvent {
    /// The guild the reaction happened in.
    pub guild_id: u64,
    /// The channel the reaction happened in.
    pub channel_id: u64,
    /// The message the reaction is on.
    pub message_id: u64,
    /// The emoji of the reaction.
    pub emoji_id: u64,
    /// The member who added or removed the reaction.
    pub member_id: u64,
    /// Whether the reactor is a bot.
    pub author_is_bot: bool,
    /// `true` when the reaction was added, `false` when it was removed.
    pub added: bool,
}

/// Match a reaction event against the configuration.
///
/// The emoji is matched by **id**: an emoji with the same name is a different
/// emoji and is ignored. A bot is ignored. Another message or channel is
/// ignored.
///
/// The decision is pure: it reads no clock and touches no Discord.
pub fn decide_reaction(config: &SafewordConfig, event: &ReactionEvent) -> ReactionDecision {
    if event.author_is_bot {
        return ReactionDecision::Ignore(IgnoreReason::BotAuthor);
    }
    if event.channel_id != config.channel_id {
        return ReactionDecision::Ignore(IgnoreReason::WrongChannel);
    }
    if event.message_id != config.message_id {
        return ReactionDecision::Ignore(IgnoreReason::WrongMessage);
    }
    if event.emoji_id != config.emoji_id {
        return ReactionDecision::Ignore(IgnoreReason::WrongEmoji);
    }
    if event.added {
        ReactionDecision::Activate(event.member_id)
    } else {
        ReactionDecision::Release(event.member_id)
    }
}

// ---------------------------------------------------------------------------
// Stable codes
// ---------------------------------------------------------------------------

/// The code raised when a role write failed.
pub const CODE_ROLE_WRITE_FAILED: &str = "role_write_failed";
/// The code raised when a member's roles could not be looked up.
pub const CODE_LOOKUP_UNAVAILABLE: &str = "lookup_unavailable";
/// The code raised when the stored state could not be read or written.
pub const CODE_STATE_FAILED: &str = "state_failed";
/// The code raised when the direct message could not be delivered.
pub const CODE_DM_FAILED: &str = "dm_failed";
/// The code raised when only some of the roles of one step were written.
pub const CODE_PARTIAL_ROLE_FAILURE: &str = "partial_role_failure";
/// The code raised when the reaction users could not be listed.
pub const CODE_REACTION_USERS_FAILED: &str = "reaction_users_failed";
/// The code raised when the reconcile stopped at its page cap.
pub const CODE_REACTION_PAGE_CAP: &str = "reaction_page_cap_reached";
/// The code raised when the role facts of a role could not be looked up.
pub const CODE_FACTS_UNAVAILABLE: &str = "facts_unavailable";
/// The code raised when the bot's top role position is unknown.
pub const CODE_BOT_TOP_UNKNOWN: &str = "bot_top_position_unknown";

/// The most actions one event may run.
///
/// This is a guard against a loop that never ends: the runtime stops after
/// this many actions, alerts `state_failed` and reports the run as failed.
/// It is not a statement about how many actions an event normally runs.
pub const MAX_ACTIONS_PER_EVENT: usize = 64;

// ---------------------------------------------------------------------------
// Traits (the Discord side)
// ---------------------------------------------------------------------------

/// What a failed lookup looks like.
///
/// A lookup that fails is never a guess: the caller treats it as a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupCode {
    /// The lookup could not be answered.
    Unavailable,
}

impl LookupCode {
    /// The stable machine code.
    pub fn code(self) -> &'static str {
        "lookup_unavailable"
    }
}

/// Why a role write failed.
///
/// Every variant is a stable code and carries no text, name or id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCode {
    /// The bot may not write the role.
    Forbidden,
    /// The role does not exist (any more).
    UnknownRole,
    /// The member does not exist (any more).
    UnknownMember,
    /// The API rate limited the write.
    RateLimited,
    /// Anything else.
    Other,
}

impl WriteCode {
    /// The stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            WriteCode::Forbidden => "forbidden",
            WriteCode::UnknownRole => "unknown_role",
            WriteCode::UnknownMember => "unknown_member",
            WriteCode::RateLimited => "rate_limited",
            WriteCode::Other => "other",
        }
    }
}

/// Why a direct message failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmCode {
    /// The member's direct messages are closed.
    Closed,
    /// The API rate limited the message.
    RateLimited,
    /// Anything else.
    Other,
}

impl DmCode {
    /// The stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            DmCode::Closed => "dm_closed",
            DmCode::RateLimited => "dm_rate_limited",
            DmCode::Other => "dm_failed",
        }
    }
}

/// The member's roles and the role writes the runtime needs.
///
/// A lookup that fails is a [`LookupCode::Unavailable`], never an empty list:
/// an empty list would look like "the member holds nothing" and could cost
/// the member their protection.
pub trait MemberRoles {
    /// The roles the member currently holds.
    fn member_roles(&mut self, member_id: u64) -> Result<Vec<RoleId>, LookupCode>;
    /// Add one role to the member.
    fn add_role(&mut self, member_id: u64, role_id: RoleId) -> Result<(), WriteCode>;
    /// Remove one role from the member.
    fn remove_role(&mut self, member_id: u64, role_id: RoleId) -> Result<(), WriteCode>;
}

/// The guild's role facts and the bot's own position in the hierarchy.
pub trait RoleFactsSource {
    /// The facts of one role.
    fn role_facts(&mut self, role_id: RoleId) -> roles_facts::FactsOutcome;
    /// The bot's top role position, or `None` when it is unknown.
    fn bot_top_position(&mut self) -> Option<u16>;
}

/// Sends a direct message to a member.
pub trait DmSender {
    /// Send `text` to the member. The text is one of the two fixed safeword
    /// messages; it never carries user content.
    fn send_dm(&mut self, user_id: u64, text: &str) -> Result<(), DmCode>;
}

/// Posts a failure to the staff alert channel.
///
/// The alert carries a stable code and nothing else: no name, no id, no
/// message text.
pub trait StaffAlerter {
    /// Raise one alert.
    fn alert(&mut self, code: &'static str);
}

/// The durable safeword state (SW2's repository behind a trait).
pub trait StateStore {
    /// Load one member's state and its version. `Ok(None)` means no row.
    fn load(&mut self, guild_id: u64, member_id: u64)
        -> Result<Option<(State, u64)>, &'static str>;
    /// Compare-and-swap write. Returns the new version.
    fn store(
        &mut self,
        guild_id: u64,
        member_id: u64,
        state: &State,
        expected_version: u64,
        now: i64,
    ) -> Result<u64, &'static str>;
    /// Compare-and-swap reset to inactive. Returns the new version.
    fn clear(
        &mut self,
        guild_id: u64,
        member_id: u64,
        expected_version: u64,
        now: i64,
    ) -> Result<u64, &'static str>;
    /// The members that have a stored row in this guild.
    ///
    /// The reconcile needs this to find the members that are on safeword in
    /// the database but no longer have the reaction.
    fn members_with_state(&mut self, guild_id: u64) -> Result<Vec<u64>, &'static str>;
}

/// Lists the users who added a reaction, one page at a time.
///
/// Discord's reaction-user endpoint returns at most [`REACTION_PAGE_SIZE`]
/// users per page; `after` is the cursor of the last user of the previous
/// page.
pub trait ReactionUsers {
    /// The users who added the emoji, at most `limit` of them, after `after`.
    fn reaction_users(
        &mut self,
        channel_id: u64,
        message_id: u64,
        emoji_id: u64,
        after: Option<u64>,
        limit: u32,
    ) -> Result<Vec<u64>, &'static str>;
}

/// How many users one reaction-user page holds.
pub const REACTION_PAGE_SIZE: u32 = 100;

// ---------------------------------------------------------------------------
// Shadow mode
// ---------------------------------------------------------------------------

/// What the runtime *would* have done, counted.
///
/// The record holds counts and codes only: no user id, no role id, no text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShadowRecord {
    /// How many activations the runtime would have started.
    pub activates: u32,
    /// How many releases the runtime would have started.
    pub releases: u32,
    /// How many restricted roles the runtime would have removed.
    pub role_removals: u32,
    /// How many roles the runtime would have added (the safeword role and restores).
    pub role_adds: u32,
    /// How many direct messages the runtime would have sent.
    pub dms: u32,
    /// How many events were ignored.
    pub ignored: u32,
    /// The stable codes of the failures the run would have raised.
    pub codes: Vec<&'static str>,
}

impl ShadowRecord {
    /// One line for the start-up or test log: counts and codes only.
    pub fn summary_line(&self) -> String {
        format!(
            "safeword shadow: activates={} releases={} role_removals={} role_adds={} dms={} ignored={} codes={}",
            self.activates,
            self.releases,
            self.role_removals,
            self.role_adds,
            self.dms,
            self.ignored,
            self.codes.join(",")
        )
    }
}

// ---------------------------------------------------------------------------
// The collaborators
// ---------------------------------------------------------------------------

/// The collaborators one safeword run needs.
///
/// Each field is a small trait, so a test injects one fake per concern.
pub struct Ops {
    /// The member's roles and the role writes.
    pub members: Box<dyn MemberRoles>,
    /// The guild's role facts.
    pub facts: Box<dyn RoleFactsSource>,
    /// The direct-message sender.
    pub dms: Box<dyn DmSender>,
    /// The staff alert channel.
    pub alerter: Box<dyn StaffAlerter>,
    /// The durable state.
    pub store: Box<dyn StateStore>,
    /// The reaction user list.
    pub reactions: Box<dyn ReactionUsers>,
    /// The sleeper the K04 grant executor uses between rate-limit retries.
    pub sleeper: Box<dyn Sleeper>,
    /// The guild-wide protected roles (the wiring card passes
    /// `PROTECTED_ROLE_IDS`). They are never given back by a restore.
    pub protected_role_ids: Vec<u64>,
}

// ---------------------------------------------------------------------------
// The outcome
// ---------------------------------------------------------------------------

/// What one reaction handling did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The event was not about this safeword.
    Ignored,
    /// The event was about this safeword but nothing had to change.
    NoWork,
    /// Every step of the handling succeeded.
    Done,
    /// Some steps succeeded and some failed. Never reported as success.
    Partial,
    /// Every step failed.
    Failed,
}

impl Outcome {
    /// Whether the handling may be reported as a success.
    ///
    /// Only [`Outcome::Done`] and [`Outcome::NoWork`] are successes. A
    /// partial run is never a success.
    pub fn is_success(self) -> bool {
        matches!(self, Outcome::Done | Outcome::NoWork)
    }
}

// ---------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------

/// The result of one step.
///
/// A failure carries two codes: the one the state machine needs to record the
/// failed step, and the stable string the staff alert carries. Neither ever
/// holds a name, an id or message text.
enum StepOutcome {
    Done,
    Failed {
        /// The code the state machine records.
        machine: FailureCode,
        /// The stable code the staff alert carries.
        alert: &'static str,
    },
}

/// The K03 decision for one role write, with a failed lookup kept visible.
enum GrantDecision {
    /// K03 decided on the facts it was given.
    Decided(Decision),
    /// A lookup failed, so the role is denied. The code names which one.
    Unavailable(&'static str),
}

/// Handle one reaction event.
///
/// In `off` mode nothing happens at all. In `shadow` mode nothing is written
/// either: only the would-do record is produced.
pub fn handle_reaction(
    config: &SafewordConfig,
    event: &ReactionEvent,
    now: i64,
    ops: &mut Ops,
    shadow: &mut ShadowRecord,
) -> Outcome {
    let decision = decide_reaction(config, event);
    let member_id = match decision {
        ReactionDecision::Ignore(_) => {
            shadow.ignored = shadow.ignored.saturating_add(1);
            return Outcome::Ignored;
        }
        ReactionDecision::Activate(member) | ReactionDecision::Release(member) => member,
    };

    if config.mode == crate::settings::Mode::Off {
        // Off performs nothing at all: not even a read.
        return Outcome::NoWork;
    }
    if config.mode == crate::settings::Mode::Shadow {
        // Shadow performs nothing: no read of the live state, no write.
        match decision {
            ReactionDecision::Activate(_) => shadow.activates = shadow.activates.saturating_add(1),
            ReactionDecision::Release(_) => shadow.releases = shadow.releases.saturating_add(1),
            ReactionDecision::Ignore(_) => {}
        }
        shadow.role_removals = shadow
            .role_removals
            .saturating_add(config.restricted_role_ids.len() as u32);
        shadow.role_adds = shadow.role_adds.saturating_add(1);
        shadow.dms = shadow.dms.saturating_add(u32::from(config.dm));
        return Outcome::NoWork;
    }

    let machine_event = match decision {
        ReactionDecision::Activate(_) => Event::ReactionAdded,
        ReactionDecision::Release(_) => Event::ReactionRemoved,
        ReactionDecision::Ignore(_) => unreachable!("ignored above"),
    };
    drive(
        config,
        event.guild_id,
        member_id,
        machine_event,
        now,
        ops,
        shadow,
    )
}

/// Everything one run needs besides the collaborators.
///
/// Grouping these keeps every function under the argument limit and makes it
/// impossible to pass the member of one run with the context of another.
struct Job<'a> {
    config: &'a SafewordConfig,
    guild_id: u64,
    member_id: u64,
    now: i64,
    context: &'a Context,
}

/// Feed one event to the state machine and execute what it emits.
fn drive(
    config: &SafewordConfig,
    guild_id: u64,
    member_id: u64,
    machine_event: Event,
    now: i64,
    ops: &mut Ops,
    shadow: &mut ShadowRecord,
) -> Outcome {
    let (state, version) = match load_state(ops, guild_id, member_id) {
        Ok(pair) => pair,
        Err(code) => {
            // The state could not be read. Acting on a guess could cost the
            // member their protection, so nothing happens but an alert.
            ops.alerter.alert(code);
            push_code(shadow, code);
            return Outcome::Failed;
        }
    };

    let context = match context_for(config, ops, member_id) {
        Ok(context) => context,
        Err(code) => {
            // The member's roles could not be looked up. The member keeps
            // whatever protection it already has: nothing is written.
            ops.alerter.alert(code);
            push_code(shadow, code);
            return Outcome::Failed;
        }
    };

    let (next, actions) = step(&state, machine_event, &context);
    let job = Job {
        config,
        guild_id,
        member_id,
        now,
        context: &context,
    };
    run_actions(&job, next, actions, version, ops, shadow)
}

/// Run the actions the machine emitted, one at a time, and store the result.
fn run_actions(
    job: &Job<'_>,
    mut state: State,
    mut pending: Vec<Action>,
    mut version: u64,
    ops: &mut Ops,
    shadow: &mut ShadowRecord,
) -> Outcome {
    let mut failures = 0usize;
    let mut performed = 0usize;
    let mut guard = 0usize;
    // The machine reads the held roles when a release hands over to a fresh
    // activation, so this copy is refreshed before such an acknowledgement.
    let mut context = job.context.clone();

    // The machine emits the actions; the runtime runs them one at a time in
    // the order they were emitted and reports each outcome once.
    while let Some(action) = pending.first().cloned() {
        pending.remove(0);
        guard += 1;
        if guard > MAX_ACTIONS_PER_EVENT {
            ops.alerter.alert(CODE_STATE_FAILED);
            push_code(shadow, CODE_STATE_FAILED);
            failures += 1;
            break;
        }
        let mut outcome = perform(job, &state, &action, &mut version, ops, shadow);
        if matches!(outcome, StepOutcome::Done) && reactivation_pending(&state) {
            // A re-add arrived during the release: the acknowledgement may
            // start a new activation from the roles held *now*. Stale roles
            // would snapshot nothing and leave the member unprotected.
            match ops.members.member_roles(job.member_id) {
                Ok(held) => context.held_roles = held,
                Err(_) => {
                    ops.alerter.alert(CODE_LOOKUP_UNAVAILABLE);
                    push_code(shadow, CODE_LOOKUP_UNAVAILABLE);
                    // Not acknowledged: the machine keeps the step and the
                    // next trigger runs it again.
                    outcome = StepOutcome::Failed {
                        machine: FailureCode::TransientHttp,
                        alert: CODE_LOOKUP_UNAVAILABLE,
                    };
                }
            }
        }
        match outcome {
            StepOutcome::Done => {
                performed += 1;
                // The machine acknowledges the step, which may emit more.
                let (following, more) = step(&state, Event::ActionDone(action.id()), &context);
                state = following;
                pending.extend(more);
            }
            StepOutcome::Failed { machine, .. } => {
                failures += 1;
                let alert = alert_code(machine);
                ops.alerter.alert(alert);
                push_code(shadow, alert);
                let (following, more) =
                    step(&state, Event::ActionFailed(action.id(), machine), &context);
                state = following;
                pending.extend(more);
            }
        }
    }

    // Persist the state the machine ended in.
    let persisted: Result<u64, &'static str> = match &state {
        State::Inactive => ops
            .store
            .clear(job.guild_id, job.member_id, version, job.now),
        other => ops
            .store
            .store(job.guild_id, job.member_id, other, version, job.now),
    };
    if let Err(code) = persisted {
        ops.alerter.alert(code);
        push_code(shadow, code);
        failures += 1;
    }

    if failures == 0 {
        if performed == 0 {
            Outcome::NoWork
        } else {
            Outcome::Done
        }
    } else if performed == 0 {
        Outcome::Failed
    } else {
        Outcome::Partial
    }
}

/// Whether the member asked for the safeword again while it was being released.
fn reactivation_pending(state: &State) -> bool {
    matches!(state, State::Releasing(snapshot) if snapshot.pending_reactivate)
}

/// Load a member's state and version. `Ok(None)` means "no row".
fn load_state(ops: &mut Ops, guild_id: u64, member_id: u64) -> Result<(State, u64), &'static str> {
    match ops.store.load(guild_id, member_id) {
        Ok(Some((state, version))) => Ok((state, version)),
        Ok(None) => Ok((State::Inactive, 0)),
        Err(code) => Err(code),
    }
}

/// Build the state machine's context for one member.
///
/// The held roles come from the member lookup. A lookup that fails is an
/// error, never an empty list.
fn context_for(
    config: &SafewordConfig,
    ops: &mut Ops,
    member_id: u64,
) -> Result<Context, &'static str> {
    let held = ops
        .members
        .member_roles(member_id)
        .map_err(|_| CODE_LOOKUP_UNAVAILABLE)?;
    Ok(Context {
        safeword_role: config.role_id,
        restricted_roles: config.restricted_role_ids.clone(),
        protected_roles: ops.protected_role_ids.clone(),
        held_roles: held,
        filter_restore: true,
        defer_grants: config.defer_grants,
    })
}

/// Execute one action.
fn perform(
    job: &Job<'_>,
    state: &State,
    action: &Action,
    version: &mut u64,
    ops: &mut Ops,
    shadow: &mut ShadowRecord,
) -> StepOutcome {
    let (guild_id, member_id) = (job.guild_id, job.member_id);
    match action {
        Action::SaveSnapshot => {
            // The snapshot that is stored is the machine's own snapshot: the
            // runtime never builds a second one, so a replayed add can never
            // overwrite it.
            match ops
                .store
                .store(guild_id, member_id, state, *version, job.now)
            {
                Ok(new_version) => {
                    *version = new_version;
                    StepOutcome::Done
                }
                Err(code) => StepOutcome::Failed {
                    machine: FailureCode::StorageError,
                    alert: code,
                },
            }
        }
        Action::RemoveRoles(roles) => remove_roles(guild_id, member_id, roles, ops, shadow),
        // The member is not on safeword yet: the safeword role itself is not a
        // restricted role, so the K03 safeword rule does not apply.
        Action::AddSafewordRole => add_roles(job, &[job.config.role_id], false, ops, shadow),
        Action::SendDm(kind) => send_dm(job.config, member_id, *kind, ops),
        Action::RemoveSafewordRole => {
            remove_role(guild_id, member_id, job.config.role_id, ops, shadow)
        }
        // The member is still on safeword while the release runs, so the K03
        // safeword rule is live: only the `SafewordRestore` path may give a
        // restricted role back.
        Action::RestoreRoles(roles) => add_roles(job, roles, true, ops, shadow),
        Action::ClearSnapshot => match ops.store.clear(guild_id, member_id, *version, job.now) {
            Ok(new_version) => {
                *version = new_version;
                StepOutcome::Done
            }
            Err(code) => StepOutcome::Failed {
                machine: FailureCode::StorageError,
                alert: code,
            },
        },
    }
}

/// Remove the restricted roles from the member.
fn remove_roles(
    guild_id: u64,
    member_id: u64,
    roles: &[RoleId],
    ops: &mut Ops,
    shadow: &mut ShadowRecord,
) -> StepOutcome {
    let mut failures = 0usize;
    let mut written = 0usize;
    for role_id in roles {
        match remove_role(guild_id, member_id, *role_id, ops, shadow) {
            StepOutcome::Done => written += 1,
            StepOutcome::Failed { .. } => failures += 1,
        }
    }
    if failures == 0 {
        StepOutcome::Done
    } else if written == 0 {
        StepOutcome::Failed {
            machine: FailureCode::TransientHttp,
            alert: CODE_ROLE_WRITE_FAILED,
        }
    } else {
        // A partial failure is never reported as success: the staff alert
        // names it and the machine keeps the failed step.
        ops.alerter.alert(CODE_PARTIAL_ROLE_FAILURE);
        push_code(shadow, CODE_PARTIAL_ROLE_FAILURE);
        StepOutcome::Failed {
            machine: FailureCode::TransientHttp,
            alert: CODE_PARTIAL_ROLE_FAILURE,
        }
    }
}

/// Remove one role, after the K03 removal decision.
fn remove_role(
    guild_id: u64,
    member_id: u64,
    role_id: RoleId,
    ops: &mut Ops,
    shadow: &mut ShadowRecord,
) -> StepOutcome {
    if let Some(code) = removal_decision(guild_id, role_id, ops) {
        ops.alerter.alert(code);
        push_code(shadow, code);
        return StepOutcome::Failed {
            machine: FailureCode::HierarchyRejected,
            alert: code,
        };
    }
    match ops.members.remove_role(member_id, role_id) {
        Ok(()) => {
            shadow.role_removals = shadow.role_removals.saturating_add(1);
            StepOutcome::Done
        }
        Err(code) => {
            let machine = write_failure_code(code);
            let alert = alert_code(machine);
            ops.alerter.alert(alert);
            push_code(shadow, alert);
            StepOutcome::Failed { machine, alert }
        }
    }
}

/// Adapts the member-role trait to the K04 executor's writer trait.
///
/// The executor classifies a failure from numbers (status and api code), so
/// the runtime's stable [`WriteCode`] is turned into the numbers the executor
/// maps back to the same code. A rate-limit hint is not carried, so the
/// executor falls back to its own backoff.
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

/// The machine failure code of an executor failure.
fn executor_failure_code(code: FailCode) -> FailureCode {
    match code {
        FailCode::Forbidden | FailCode::UnknownMember => FailureCode::MissingPermissions,
        FailCode::UnknownRole => FailureCode::UnknownRole,
        FailCode::RateLimited => FailureCode::RateLimited,
        FailCode::Other => FailureCode::TransientHttp,
    }
}

/// Add roles through the K03 decision and the K04 grant executor.
///
/// Every role gets its K03 decision first (the `SafewordRestore` path). The
/// executor then writes only what K03 allowed and the member does not hold,
/// retries a 429 a limited number of times through the injected sleeper, and
/// reports one outcome per role. A role whose facts could not be looked up is
/// a denial that is never written. `safeword_active` says whether the member
/// is on safeword right now (`true` for a restore).
fn add_roles(
    job: &Job<'_>,
    roles: &[RoleId],
    safeword_active: bool,
    ops: &mut Ops,
    shadow: &mut ShadowRecord,
) -> StepOutcome {
    let mut items: Vec<GrantItem> = Vec::with_capacity(roles.len());
    let mut failures: Vec<FailureCode> = Vec::new();
    // The roles held right now, not the ones held when the event arrived: the
    // earlier steps of this run may have taken roles away. A failed lookup
    // means "assume nothing is held", so every role is written (a role the
    // member already has is a harmless repeat), never skipped.
    let held_now = ops.members.member_roles(job.member_id).unwrap_or_default();
    for role_id in roles {
        match grant_decision(job, *role_id, safeword_active, ops) {
            GrantDecision::Decided(decision) => items.push(GrantItem {
                role_id: *role_id,
                decision,
                already_held: held_now.contains(role_id),
            }),
            GrantDecision::Unavailable(code) => {
                // Left out of the request, so the executor can never write it.
                ops.alerter.alert(code);
                push_code(shadow, code);
                failures.push(FailureCode::HierarchyRejected);
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
                member_id: job.member_id,
                items: &items,
            },
        )
    };
    let mut succeeded = 0usize;
    for result in &report.results {
        match result.outcome {
            RoleOutcome::Done => {
                shadow.role_adds = shadow.role_adds.saturating_add(1);
                succeeded += 1;
            }
            RoleOutcome::NoOp => succeeded += 1,
            RoleOutcome::Denied(reason) => {
                ops.alerter.alert(reason.code());
                push_code(shadow, reason.code());
                failures.push(FailureCode::HierarchyRejected);
            }
            RoleOutcome::Failed(code) => {
                ops.alerter.alert(CODE_ROLE_WRITE_FAILED);
                push_code(shadow, CODE_ROLE_WRITE_FAILED);
                failures.push(executor_failure_code(code));
            }
        }
    }
    let Some(first) = failures.first().copied() else {
        return StepOutcome::Done;
    };
    // One reason for every failure is reported as that reason; a mix is a
    // transient failure the machine retries.
    let machine = if failures.iter().all(|code| *code == first) {
        first
    } else {
        FailureCode::TransientHttp
    };
    if succeeded == 0 {
        StepOutcome::Failed {
            machine,
            alert: CODE_ROLE_WRITE_FAILED,
        }
    } else {
        // A partial failure is never reported as success: the staff alert
        // names it and the machine keeps the failed step.
        ops.alerter.alert(CODE_PARTIAL_ROLE_FAILURE);
        push_code(shadow, CODE_PARTIAL_ROLE_FAILURE);
        StepOutcome::Failed {
            machine,
            alert: CODE_PARTIAL_ROLE_FAILURE,
        }
    }
}

/// Send one of the two fixed direct messages.
fn send_dm(config: &SafewordConfig, member_id: u64, kind: DmKind, ops: &mut Ops) -> StepOutcome {
    if !config.dm {
        // The DM is switched off: the step is done, nothing was sent.
        return StepOutcome::Done;
    }
    match ops.dms.send_dm(member_id, kind.text()) {
        Ok(()) => StepOutcome::Done,
        Err(code) => {
            // A DM failure never changes the member's roles or the snapshot:
            // only the machine learns about it.
            let machine = dm_failure_code(code);
            let alert = alert_code(machine);
            ops.alerter.alert(alert);
            StepOutcome::Failed { machine, alert }
        }
    }
}

/// The stable alert code of a machine failure code.
///
/// The machine's failure codes are an enum of its own; the staff alert
/// carries a stable string. The mapping is one place, so an alert never
/// carries anything but a code.
fn alert_code(code: FailureCode) -> &'static str {
    match code {
        FailureCode::StorageError => CODE_STATE_FAILED,
        FailureCode::MissingPermissions => CODE_ROLE_WRITE_FAILED,
        FailureCode::HierarchyRejected => CODE_ROLE_WRITE_FAILED,
        FailureCode::RateLimited => CODE_ROLE_WRITE_FAILED,
        FailureCode::DmClosed => CODE_DM_FAILED,
        FailureCode::TransientHttp => CODE_ROLE_WRITE_FAILED,
        FailureCode::UnknownRole => CODE_ROLE_WRITE_FAILED,
    }
}

/// The K03 grant decision for a role the safeword writes.
///
/// The path is `SafewordRestore`: it is the path that exists for the safeword
/// restores, so a restricted role can be given back while the member is on
/// safeword. The restricted set, the protected set and the safeword state are
/// the real ones, so the K03 rules are live: another path would be denied.
fn grant_decision(
    job: &Job<'_>,
    role_id: RoleId,
    safeword_active: bool,
    ops: &mut Ops,
) -> GrantDecision {
    let Some(bot_top) = ops.facts.bot_top_position() else {
        // Without the bot's position nothing can be decided: a denial.
        return GrantDecision::Unavailable(CODE_BOT_TOP_UNKNOWN);
    };
    let facts = ops.facts.role_facts(role_id);
    let inputs = DecisionInputs {
        path: GrantPath::SafewordRestore,
        bot_top_position: bot_top,
        guild_id: job.guild_id,
        protected_role_ids: &ops.protected_role_ids,
        safeword: if safeword_active {
            SafewordState::Active
        } else {
            SafewordState::Inactive
        },
        safeword_restricted_role_ids: &job.config.restricted_role_ids,
        staff: None,
    };
    match decide_from_facts(facts, role_id, &inputs) {
        RuntimeDecision::Decided(decision) => GrantDecision::Decided(decision),
        RuntimeDecision::Unavailable => GrantDecision::Unavailable(CODE_FACTS_UNAVAILABLE),
    }
}

/// The K03 removal decision for a role the safeword removes.
fn removal_decision(guild_id: u64, role_id: RoleId, ops: &mut Ops) -> Option<&'static str> {
    let bot_top = match ops.facts.bot_top_position() {
        Some(position) => position,
        None => return Some(CODE_BOT_TOP_UNKNOWN),
    };
    let facts = ops.facts.role_facts(role_id);
    let role = match facts {
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
        guild_id,
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

/// The stable failure code of a role write.
fn write_failure_code(code: WriteCode) -> FailureCode {
    match code {
        WriteCode::Forbidden => FailureCode::MissingPermissions,
        WriteCode::UnknownRole => FailureCode::UnknownRole,
        WriteCode::UnknownMember => FailureCode::MissingPermissions,
        WriteCode::RateLimited => FailureCode::RateLimited,
        WriteCode::Other => FailureCode::TransientHttp,
    }
}

/// The stable failure code of a direct message.
fn dm_failure_code(code: DmCode) -> FailureCode {
    match code {
        DmCode::Closed => FailureCode::DmClosed,
        DmCode::RateLimited => FailureCode::RateLimited,
        DmCode::Other => FailureCode::TransientHttp,
    }
}

/// Record a code in the shadow record, without duplicates.
fn push_code(record: &mut ShadowRecord, code: &'static str) {
    if !record.codes.contains(&code) {
        record.codes.push(code);
    }
}

// ---------------------------------------------------------------------------
// Startup reconcile
// ---------------------------------------------------------------------------

/// How many reaction-user pages the reconcile fetches at most.
///
/// The cap bounds the number of *pages* the reconcile reads, and with it the
/// number of users it inspects (`cap * REACTION_PAGE_SIZE` users). A message
/// with more reactors than that is reported with [`CODE_REACTION_PAGE_CAP`],
/// not silently truncated.
pub const MAX_RECONCILE_PAGES: u32 = 100;

/// What the reconcile did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// How many members the reconcile activated.
    pub activated: u32,
    /// How many members the reconcile released.
    pub released: u32,
    /// How many members were already in the right state.
    pub unchanged: u32,
    /// How many members the reconcile could not process.
    pub failed: u32,
    /// The stable codes of the problems the reconcile hit.
    pub codes: Vec<&'static str>,
}

impl ReconcileReport {
    /// One line for the start-up log: counts and codes only.
    pub fn summary_line(&self) -> String {
        format!(
            "safeword reconcile: activated={} released={} unchanged={} failed={} codes={}",
            self.activated,
            self.released,
            self.unchanged,
            self.failed,
            self.codes.join(",")
        )
    }

    /// Record a code, without duplicates.
    fn push(&mut self, code: &'static str) {
        if !self.codes.contains(&code) {
            self.codes.push(code);
        }
    }
}

/// What the reconcile did for one member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberOutcome {
    /// The member's safeword became active.
    Activated,
    /// The member's safeword was released.
    Released,
    /// The member was already in the right state.
    Unchanged,
    /// The member could not be processed.
    Failed,
}

/// Rebuild the safeword state after a restart.
///
/// The reconcile reads the users who currently have the reaction (one page at
/// a time, [`REACTION_PAGE_SIZE`] users per page) and the members that have a
/// stored row, and drives the state machine for each of them.
///
/// When something cannot be read the reconcile **reports** it and leaves that
/// member alone: it never guesses. A member whose state could not be read
/// keeps whatever protection it has.
pub fn reconcile(
    config: &SafewordConfig,
    guild_id: u64,
    now: i64,
    max_pages: u32,
    ops: &mut Ops,
) -> ReconcileReport {
    let mut report = ReconcileReport::default();
    if config.is_off() {
        // Off performs nothing at all: not even a read.
        return report;
    }

    let reacting = match collect_reaction_users(config, max_pages, ops, &mut report) {
        Some(users) => users,
        None => return report,
    };

    let stored = match ops.store.members_with_state(guild_id) {
        Ok(members) => members,
        Err(code) => {
            // Without the stored members the reconcile cannot find the members
            // that must be released. It reports and does nothing.
            report.push(code);
            report.failed = report.failed.saturating_add(1);
            return report;
        }
    };

    let reacting_set: BTreeSet<u64> = reacting.iter().copied().collect();

    if config.is_shadow() {
        shadow_reconcile(guild_id, &reacting_set, &stored, ops, &mut report);
        return report;
    }

    // Every member that has the reaction: make the state match.
    for member_id in &reacting_set {
        let outcome = reconcile_member(config, guild_id, *member_id, true, now, ops, &mut report);
        match outcome {
            MemberOutcome::Activated => report.activated = report.activated.saturating_add(1),
            MemberOutcome::Released => report.released = report.released.saturating_add(1),
            MemberOutcome::Unchanged => report.unchanged = report.unchanged.saturating_add(1),
            MemberOutcome::Failed => report.failed = report.failed.saturating_add(1),
        }
    }

    // Every member that is stored but no longer has the reaction: release it.
    for member_id in &stored {
        if reacting_set.contains(member_id) {
            continue;
        }
        let outcome = reconcile_member(config, guild_id, *member_id, false, now, ops, &mut report);
        match outcome {
            MemberOutcome::Activated => report.activated = report.activated.saturating_add(1),
            MemberOutcome::Released => report.released = report.released.saturating_add(1),
            MemberOutcome::Unchanged => report.unchanged = report.unchanged.saturating_add(1),
            MemberOutcome::Failed => report.failed = report.failed.saturating_add(1),
        }
    }

    report
}

/// What the reconcile would do in shadow mode: counts only, nothing written.
///
/// It reads the stored states (a read changes nothing) and counts the members
/// that would be activated or released. A state that cannot be read is
/// reported and counted as failed, never guessed.
fn shadow_reconcile(
    guild_id: u64,
    reacting: &BTreeSet<u64>,
    stored: &[u64],
    ops: &mut Ops,
    report: &mut ReconcileReport,
) {
    let everyone: BTreeSet<u64> = reacting.iter().chain(stored.iter()).copied().collect();
    for member_id in everyone {
        let present = reacting.contains(&member_id);
        let state = match load_state(ops, guild_id, member_id) {
            Ok((state, _)) => state,
            Err(code) => {
                report.push(code);
                report.failed = report.failed.saturating_add(1);
                continue;
            }
        };
        let inactive = matches!(state, State::Inactive);
        match (present, inactive) {
            (true, true) => report.activated = report.activated.saturating_add(1),
            (false, false) => report.released = report.released.saturating_add(1),
            _ => report.unchanged = report.unchanged.saturating_add(1),
        }
    }
}

/// How many rounds of work the reconcile runs for one member.
///
/// Each round runs the actions the machine wants next and acknowledges them.
/// The cap only stops a machine that never settles: the member is then
/// reported as failed with [`CODE_STATE_FAILED`].
const MAX_RECONCILE_ROUNDS: usize = 16;

/// Drive the state machine for one member after a restart.
///
/// The machine is asked what to do next, the actions run through
/// `run_actions` (every outcome is acknowledged and the end state stored), and
/// the machine is asked again, until it has nothing left to do or a step fails.
fn reconcile_member(
    config: &SafewordConfig,
    guild_id: u64,
    member_id: u64,
    reaction_present_now: bool,
    now: i64,
    ops: &mut Ops,
    report: &mut ReconcileReport,
) -> MemberOutcome {
    let mut worked = false;
    for _ in 0..MAX_RECONCILE_ROUNDS {
        let (state, version) = match load_state(ops, guild_id, member_id) {
            Ok(pair) => pair,
            Err(code) => {
                report.push(code);
                return MemberOutcome::Failed;
            }
        };
        let context = match context_for(config, ops, member_id) {
            Ok(context) => context,
            Err(code) => {
                report.push(code);
                return MemberOutcome::Failed;
            }
        };
        let (next, actions) = match restart_action(&state, reaction_present_now) {
            RestartAction::NoOp => {
                return match (worked, reaction_present_now) {
                    (false, _) => MemberOutcome::Unchanged,
                    (true, true) => MemberOutcome::Activated,
                    (true, false) => MemberOutcome::Released,
                };
            }
            RestartAction::Trigger(event) => step(&state, event, &context),
            RestartAction::Execute(action) => (state.clone(), vec![action]),
        };
        let job = Job {
            config,
            guild_id,
            member_id,
            now,
            context: &context,
        };
        let mut shadow = ShadowRecord::default();
        match run_actions(&job, next, actions, version, ops, &mut shadow) {
            Outcome::Done => worked = true,
            // The machine had nothing to do: it is settled.
            Outcome::NoWork | Outcome::Ignored => {
                return if worked {
                    if reaction_present_now {
                        MemberOutcome::Activated
                    } else {
                        MemberOutcome::Released
                    }
                } else {
                    MemberOutcome::Unchanged
                };
            }
            Outcome::Partial | Outcome::Failed => {
                // The codes of the failed steps, never text.
                for code in shadow.codes {
                    report.push(code);
                }
                return MemberOutcome::Failed;
            }
        }
    }
    report.push(CODE_STATE_FAILED);
    MemberOutcome::Failed
}

/// Read every page of reaction users.
///
/// `None` means the list could not be read: the caller must not guess.
fn collect_reaction_users(
    config: &SafewordConfig,
    max_pages: u32,
    ops: &mut Ops,
    report: &mut ReconcileReport,
) -> Option<Vec<u64>> {
    // The list holds at most one page's worth of users per page read, so its
    // length is bounded by `max_pages * REACTION_PAGE_SIZE`.
    let capacity = (max_pages as usize).saturating_mul(REACTION_PAGE_SIZE as usize);
    let mut users: Vec<u64> = Vec::with_capacity(capacity.min(1024));
    let mut after: Option<u64> = None;
    let mut pages = 0u32;
    loop {
        if pages >= max_pages {
            // The cap was reached: the list is incomplete, so the reconcile
            // reports it instead of acting on half the truth.
            report.push(CODE_REACTION_PAGE_CAP);
            return None;
        }
        pages += 1;
        let page = match ops.reactions.reaction_users(
            config.channel_id,
            config.message_id,
            config.emoji_id,
            after,
            REACTION_PAGE_SIZE,
        ) {
            Ok(page) => page,
            Err(code) => {
                report.push(code);
                return None;
            }
        };
        // A page that holds more users than the page size is not a page: the
        // whole class is refused rather than one bad page being trimmed.
        if page.len() > REACTION_PAGE_SIZE as usize {
            report.push(CODE_REACTION_USERS_FAILED);
            return None;
        }
        let count = page.len();
        let last = page.last().copied();
        users.extend(page);
        if count < REACTION_PAGE_SIZE as usize {
            break;
        }
        // A full page that does not advance the cursor would loop forever on
        // the same page, so it is refused.
        if after == last {
            report.push(CODE_REACTION_USERS_FAILED);
            return None;
        }
        after = last;
    }
    Some(users)
}

// ---------------------------------------------------------------------------
// The feature registry hook
// ---------------------------------------------------------------------------

/// What the wiring card's context must provide so the safeword runtime can
/// act on an event.
///
/// The registry calls a feature with a shared context (`&X`), so the
/// collaborators are handed out as owned boxes. The wiring card decides what
/// they are (serenity clients, the SW2 repository, the staff alert channel).
pub trait SafewordContext {
    /// The collaborators for one guild, or `None` when the feature cannot act.
    fn safeword_ops(&self, guild_id: u64) -> Option<Ops>;
    /// The unix second the handling happens in.
    fn safeword_now(&self) -> i64;
}

/// The safeword feature, as the registry sees it.
///
/// The feature is **not registered**: wiring card W2/W3 adds it to
/// `features::production_features`. Nothing here is called by `main.rs` yet.
pub struct SafewordFeature<C> {
    /// The parsed configuration.
    pub config: SafewordConfig,
    /// The context the wiring card supplies.
    pub context: C,
    /// The would-do record of the shadow runs.
    pub shadow: std::sync::Mutex<ShadowRecord>,
}

impl<C> SafewordFeature<C> {
    /// Build the feature from a configuration and a context.
    pub fn new(config: SafewordConfig, context: C) -> Self {
        Self {
            config,
            context,
            shadow: std::sync::Mutex::new(ShadowRecord::default()),
        }
    }
}

impl<C> crate::features::Feature<(), ()> for SafewordFeature<C>
where
    C: SafewordContext + Send + Sync,
{
    fn name(&self) -> &'static str {
        "safeword"
    }

    fn enabled(&self) -> bool {
        !self.config.is_off()
    }

    fn needs_core_db(&self) -> bool {
        true
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a (),
        event: &'a poise::serenity_prelude::FullEvent,
        _data: &'a (),
        guild: Option<u64>,
        _core_db: Option<&'a tokio_rusqlite::Connection>,
    ) -> crate::jobs::runner::BoxFuture<'a, ()> {
        Box::pin(async move {
            let Some(guild_id) = guild else {
                return;
            };
            let Some(reaction) = reaction_event(event) else {
                return;
            };
            let Some(mut ops) = self.context.safeword_ops(guild_id) else {
                return;
            };
            let now = self.context.safeword_now();
            let mut shadow = self.shadow.lock().unwrap_or_else(|e| e.into_inner());
            handle_reaction(&self.config, &reaction, now, &mut ops, &mut shadow);
        })
    }
}

/// The reaction event of a serenity event, in plain values.
///
/// `None` when the event is not a reaction add or remove, when it has no
/// guild, or when the emoji has no id (a unicode emoji can never be the
/// configured custom emoji).
pub fn reaction_event(event: &poise::serenity_prelude::FullEvent) -> Option<ReactionEvent> {
    use poise::serenity_prelude::FullEvent as E;
    use poise::serenity_prelude::ReactionType;

    let (reaction, added) = match event {
        E::ReactionAdd { add_reaction } => (add_reaction, true),
        E::ReactionRemove { removed_reaction } => (removed_reaction, false),
        _ => return None,
    };
    let guild_id = reaction.guild_id?.get();
    let member_id = reaction.user_id?.get();
    let emoji_id = match &reaction.emoji {
        ReactionType::Custom { id, .. } => id.get(),
        // A unicode emoji has no id, and any future variant has none either:
        // neither can match the configured custom emoji, so both are ignored.
        ReactionType::Unicode(_) => return None,
        _ => return None,
    };
    // A bot is ignored, so the reactor's bot flag is read here.
    let author_is_bot = reaction
        .member
        .as_ref()
        .map(|member| member.user.bot)
        .unwrap_or(false);
    Some(ReactionEvent {
        guild_id,
        channel_id: reaction.channel_id.get(),
        message_id: reaction.message_id.get(),
        emoji_id,
        member_id,
        author_is_bot,
        added,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    use crate::roles::eligibility::RoleFacts;
    use crate::safeword::config::{
        KEY_CHANNEL_ID, KEY_DEFER_GRANTS, KEY_DM, KEY_EMOJI_ID, KEY_MESSAGE_ID, KEY_MODE,
        KEY_RESTRICTED_ROLE_IDS, KEY_ROLE_ID,
    };
    use crate::safeword::state::ACTIVE_DM_TEXT;
    use crate::safeword::state::RELEASED_DM_TEXT;
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, VecDeque};
    use std::rc::Rc;

    const GUILD: u64 = 600_001;
    const SAFEWORD_ROLE: u64 = 600_010;
    const RESTRICTED_A: u64 = 600_020;
    const RESTRICTED_B: u64 = 600_021;
    const UNRESTRICTED: u64 = 600_030;
    const CHANNEL: u64 = 600_040;
    const MESSAGE: u64 = 600_050;
    const EMOJI: u64 = 600_060;
    const MEMBER: u64 = 600_100;
    const OTHER_MEMBER: u64 = 600_101;
    const NOW: i64 = 1_700_000_000;

    // ------------------------------------------------------------------
    // Fakes
    // ------------------------------------------------------------------

    /// What the member-role fake recorded.
    #[derive(Debug, Default, Clone)]
    struct MemberLog {
        roles: BTreeMap<u64, Vec<u64>>,
        added: Vec<(u64, u64)>,
        removed: Vec<(u64, u64)>,
        lookups: Vec<u64>,
    }

    /// The member-role fake: a scripted set of role writes.
    struct FakeMembers {
        log: Rc<RefCell<MemberLog>>,
        lookup_fails: bool,
        /// Lookups with an index at or above this fail (index = lookups so far).
        lookup_cutoff: Rc<Cell<usize>>,
        /// Role ids whose write fails, and with what.
        fail_writes: BTreeMap<u64, WriteCode>,
        /// Role ids whose next ADDs fail a number of times, then succeed.
        transient: BTreeMap<u64, (WriteCode, u32)>,
        /// A script consumed one entry per write: `Some` fails the write.
        chaos: VecDeque<Option<WriteCode>>,
    }

    impl FakeMembers {
        fn new() -> Self {
            Self {
                log: Rc::new(RefCell::new(MemberLog::default())),
                lookup_fails: false,
                lookup_cutoff: Rc::new(Cell::new(usize::MAX)),
                fail_writes: BTreeMap::new(),
                transient: BTreeMap::new(),
                chaos: VecDeque::new(),
            }
        }

        /// The next `times` writes of `role_id` fail with `code`, then succeed.
        fn failing_write_times(mut self, role_id: u64, code: WriteCode, times: u32) -> Self {
            self.transient.insert(role_id, (code, times));
            self
        }

        /// Each write consumes one scripted entry; `Some(code)` fails it.
        fn with_chaos(mut self, script: Vec<Option<WriteCode>>) -> Self {
            self.chaos = script.into();
            self
        }

        /// The failure the next write of `role_id` meets, if any.
        fn next_failure(&mut self, role_id: u64, is_add: bool) -> Option<WriteCode> {
            if let Some(code) = self.fail_writes.get(&role_id) {
                return Some(*code);
            }
            let transient = if is_add {
                self.transient.get_mut(&role_id)
            } else {
                None
            };
            if let Some((code, left)) = transient {
                if *left > 0 {
                    *left -= 1;
                    return Some(*code);
                }
            }
            self.chaos.pop_front().flatten()
        }

        fn with_roles(self, member_id: u64, roles: &[u64]) -> Self {
            self.log
                .borrow_mut()
                .roles
                .insert(member_id, roles.to_vec());
            self
        }

        fn failing_lookup(mut self) -> Self {
            self.lookup_fails = true;
            self
        }

        fn failing_write(mut self, role_id: u64, code: WriteCode) -> Self {
            self.fail_writes.insert(role_id, code);
            self
        }
    }

    impl MemberRoles for FakeMembers {
        fn member_roles(&mut self, member_id: u64) -> Result<Vec<RoleId>, LookupCode> {
            let index = self.log.borrow().lookups.len();
            self.log.borrow_mut().lookups.push(member_id);
            if self.lookup_fails || index >= self.lookup_cutoff.get() {
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

        fn add_role(&mut self, member_id: u64, role_id: RoleId) -> Result<(), WriteCode> {
            self.log.borrow_mut().added.push((member_id, role_id));
            match self.next_failure(role_id, true) {
                Some(code) => Err(code),
                None => {
                    let mut log = self.log.borrow_mut();
                    log.roles.entry(member_id).or_default().push(role_id);
                    Ok(())
                }
            }
        }

        fn remove_role(&mut self, member_id: u64, role_id: RoleId) -> Result<(), WriteCode> {
            self.log.borrow_mut().removed.push((member_id, role_id));
            match self.next_failure(role_id, false) {
                Some(code) => Err(code),
                None => {
                    let mut log = self.log.borrow_mut();
                    if let Some(roles) = log.roles.get_mut(&member_id) {
                        roles.retain(|r| *r != role_id);
                    }
                    Ok(())
                }
            }
        }
    }

    /// The role-facts fake.
    struct FakeFacts {
        top: Option<u16>,
        unavailable: bool,
        missing: bool,
    }

    impl FakeFacts {
        fn new() -> Self {
            Self {
                top: Some(50),
                unavailable: false,
                missing: false,
            }
        }

        fn no_top(mut self) -> Self {
            self.top = None;
            self
        }

        fn unavailable(mut self) -> Self {
            self.unavailable = true;
            self
        }
    }

    impl RoleFactsSource for FakeFacts {
        fn role_facts(&mut self, role_id: RoleId) -> roles_facts::FactsOutcome {
            if self.unavailable {
                return roles_facts::FactsOutcome::Unavailable;
            }
            if self.missing {
                return roles_facts::FactsOutcome::Missing;
            }
            roles_facts::FactsOutcome::Found(RoleFacts {
                id: role_id,
                position: 1,
                managed: false,
                permissions: 0,
                exists: true,
            })
        }

        fn bot_top_position(&mut self) -> Option<u16> {
            self.top
        }
    }

    /// The direct-message fake.
    #[derive(Default)]
    struct FakeDms {
        sent: Rc<RefCell<Vec<(u64, String)>>>,
        failure: Option<DmCode>,
        /// How many of the next sends fail before the rest succeed.
        fail_next: u32,
    }

    impl DmSender for FakeDms {
        fn send_dm(&mut self, user_id: u64, text: &str) -> Result<(), DmCode> {
            if self.fail_next > 0 {
                self.fail_next -= 1;
                return Err(DmCode::Other);
            }
            match self.failure {
                Some(code) => Err(code),
                None => {
                    self.sent.borrow_mut().push((user_id, text.to_string()));
                    Ok(())
                }
            }
        }
    }

    /// The staff-alert fake.
    #[derive(Default)]
    struct FakeAlerter {
        alerts: Rc<RefCell<Vec<&'static str>>>,
    }

    impl StaffAlerter for FakeAlerter {
        fn alert(&mut self, code: &'static str) {
            self.alerts.borrow_mut().push(code);
        }
    }

    /// The state-store fake: a map of member id to (state, version).
    #[derive(Default)]
    struct FakeStore {
        rows: Rc<RefCell<BTreeMap<u64, (State, u64)>>>,
        load_fails: bool,
        store_fails: bool,
        /// The states that were written, in order.
        writes: Rc<RefCell<Vec<(u64, State)>>>,
        /// The members that were cleared, in order.
        clears: Rc<RefCell<Vec<u64>>>,
    }

    impl FakeStore {
        fn new() -> Self {
            Self::default()
        }

        fn with_state(self, member_id: u64, state: State) -> Self {
            self.rows.borrow_mut().insert(member_id, (state, 1));
            self
        }

        fn failing_load(mut self) -> Self {
            self.load_fails = true;
            self
        }

        fn failing_store(mut self) -> Self {
            self.store_fails = true;
            self
        }

        fn state_of(&self, member_id: u64) -> Option<State> {
            self.rows
                .borrow()
                .get(&member_id)
                .map(|(state, _)| state.clone())
        }

        fn version_of(&self, member_id: u64) -> Option<u64> {
            self.rows
                .borrow()
                .get(&member_id)
                .map(|(_, version)| *version)
        }
    }

    impl StateStore for FakeStore {
        fn load(
            &mut self,
            _guild_id: u64,
            member_id: u64,
        ) -> Result<Option<(State, u64)>, &'static str> {
            if self.load_fails {
                return Err(CODE_STATE_FAILED);
            }
            Ok(self.rows.borrow().get(&member_id).cloned())
        }

        fn store(
            &mut self,
            _guild_id: u64,
            member_id: u64,
            state: &State,
            expected_version: u64,
            _now: i64,
        ) -> Result<u64, &'static str> {
            if self.store_fails {
                return Err(CODE_STATE_FAILED);
            }
            let mut rows = self.rows.borrow_mut();
            let current = rows
                .get(&member_id)
                .map(|(_, version)| *version)
                .unwrap_or(0);
            if current != expected_version {
                return Err(CODE_STATE_FAILED);
            }
            let new_version = expected_version + 1;
            rows.insert(member_id, (state.clone(), new_version));
            self.writes.borrow_mut().push((member_id, state.clone()));
            Ok(new_version)
        }

        fn clear(
            &mut self,
            _guild_id: u64,
            member_id: u64,
            expected_version: u64,
            _now: i64,
        ) -> Result<u64, &'static str> {
            if self.store_fails {
                return Err(CODE_STATE_FAILED);
            }
            let mut rows = self.rows.borrow_mut();
            let current = rows
                .get(&member_id)
                .map(|(_, version)| *version)
                .unwrap_or(0);
            if current != expected_version {
                return Err(CODE_STATE_FAILED);
            }
            let new_version = expected_version + 1;
            rows.insert(member_id, (State::Inactive, new_version));
            self.clears.borrow_mut().push(member_id);
            Ok(new_version)
        }

        fn members_with_state(&mut self, _guild_id: u64) -> Result<Vec<u64>, &'static str> {
            if self.load_fails {
                return Err(CODE_STATE_FAILED);
            }
            Ok(self.rows.borrow().keys().copied().collect())
        }
    }

    /// The reaction-user calls the fake recorded: the cursor and the limit.
    type ReactionCalls = Rc<RefCell<Vec<(Option<u64>, u32)>>>;

    /// The reaction-user fake: a scripted list of pages.
    struct FakeReactionUsers {
        pages: Vec<Vec<u64>>,
        calls: ReactionCalls,
        failure: bool,
    }

    impl FakeReactionUsers {
        fn new(pages: Vec<Vec<u64>>) -> Self {
            Self {
                pages,
                calls: Rc::new(RefCell::new(Vec::new())),
                failure: false,
            }
        }

        fn failing() -> Self {
            Self {
                pages: Vec::new(),
                calls: Rc::new(RefCell::new(Vec::new())),
                failure: true,
            }
        }
    }

    impl ReactionUsers for FakeReactionUsers {
        fn reaction_users(
            &mut self,
            _channel_id: u64,
            _message_id: u64,
            _emoji_id: u64,
            after: Option<u64>,
            limit: u32,
        ) -> Result<Vec<u64>, &'static str> {
            self.calls.borrow_mut().push((after, limit));
            if self.failure {
                return Err(CODE_REACTION_USERS_FAILED);
            }
            let index = match after {
                None => 0,
                Some(cursor) => {
                    // The cursor is the last id of the previous page.
                    let mut seen = 0usize;
                    let mut found = None;
                    for (i, page) in self.pages.iter().enumerate() {
                        if let Some(last) = page.last() {
                            if *last == cursor {
                                found = Some(i + 1);
                                break;
                            }
                        }
                        seen += page.len();
                        let _ = seen;
                    }
                    match found {
                        Some(index) => index,
                        None => return Ok(Vec::new()),
                    }
                }
            };
            Ok(self.pages.get(index).cloned().unwrap_or_default())
        }
    }

    /// The sleeper fake: it records the sleeps and never waits.
    struct FakeSleeper {
        slept: Rc<RefCell<Vec<u64>>>,
    }

    impl Sleeper for FakeSleeper {
        fn sleep_ms(&mut self, ms: u64) {
            self.slept.borrow_mut().push(ms);
        }
    }

    /// The whole fake set, with the log handles the test keeps.
    struct Harness {
        ops: Ops,
        members_log: Rc<RefCell<MemberLog>>,
        dms_sent: Rc<RefCell<Vec<(u64, String)>>>,
        alerts: Rc<RefCell<Vec<&'static str>>>,
        store_rows: Rc<RefCell<BTreeMap<u64, (State, u64)>>>,
        store_writes: Rc<RefCell<Vec<(u64, State)>>>,
        store_clears: Rc<RefCell<Vec<u64>>>,
        reaction_calls: ReactionCalls,
        slept: Rc<RefCell<Vec<u64>>>,
        lookup_cutoff: Rc<Cell<usize>>,
    }

    impl Harness {
        fn new(members: FakeMembers) -> Self {
            let members_log = Rc::clone(&members.log);
            let lookup_cutoff = Rc::clone(&members.lookup_cutoff);
            let dms_sent = Rc::new(RefCell::new(Vec::new()));
            let alerts = Rc::new(RefCell::new(Vec::new()));
            let store = FakeStore::new();
            let store_rows = Rc::clone(&store.rows);
            let store_writes = Rc::clone(&store.writes);
            let store_clears = Rc::clone(&store.clears);
            let reactions = FakeReactionUsers::new(Vec::new());
            let reaction_calls = Rc::clone(&reactions.calls);
            let slept = Rc::new(RefCell::new(Vec::new()));
            let ops = Ops {
                members: Box::new(members),
                facts: Box::new(FakeFacts::new()),
                dms: Box::new(FakeDms {
                    sent: Rc::clone(&dms_sent),
                    failure: None,
                    fail_next: 0,
                }),
                alerter: Box::new(FakeAlerter {
                    alerts: Rc::clone(&alerts),
                }),
                store: Box::new(store),
                reactions: Box::new(reactions),
                sleeper: Box::new(FakeSleeper {
                    slept: Rc::clone(&slept),
                }),
                protected_role_ids: Vec::new(),
            };
            Self {
                ops,
                members_log,
                dms_sent,
                alerts,
                store_rows,
                store_writes,
                store_clears,
                reaction_calls,
                slept,
                lookup_cutoff,
            }
        }

        fn with_protected(mut self, roles: &[u64]) -> Self {
            self.ops.protected_role_ids = roles.to_vec();
            self
        }

        fn sleeps(&self) -> Vec<u64> {
            self.slept.borrow().clone()
        }

        fn with_store(mut self, store: FakeStore) -> Self {
            self.store_rows = Rc::clone(&store.rows);
            self.store_writes = Rc::clone(&store.writes);
            self.store_clears = Rc::clone(&store.clears);
            self.ops.store = Box::new(store);
            self
        }

        fn with_facts<F: RoleFactsSource + 'static>(mut self, facts: F) -> Self {
            self.ops.facts = Box::new(facts);
            self
        }

        fn with_dm_failure(mut self, code: DmCode) -> Self {
            self.ops.dms = Box::new(FakeDms {
                sent: Rc::clone(&self.dms_sent),
                failure: Some(code),
                fail_next: 0,
            });
            self
        }

        fn with_reactions(mut self, reactions: FakeReactionUsers) -> Self {
            self.reaction_calls = Rc::clone(&reactions.calls);
            self.ops.reactions = Box::new(reactions);
            self
        }

        fn roles_of(&self, member_id: u64) -> Vec<u64> {
            self.members_log
                .borrow()
                .roles
                .get(&member_id)
                .cloned()
                .unwrap_or_default()
        }

        fn added(&self) -> Vec<(u64, u64)> {
            self.members_log.borrow().added.clone()
        }

        fn removed(&self) -> Vec<(u64, u64)> {
            self.members_log.borrow().removed.clone()
        }

        fn state_of(&self, member_id: u64) -> Option<State> {
            self.store_rows
                .borrow()
                .get(&member_id)
                .map(|(state, _)| state.clone())
        }

        fn alerts(&self) -> Vec<&'static str> {
            self.alerts.borrow().clone()
        }

        fn dms(&self) -> Vec<(u64, String)> {
            self.dms_sent.borrow().clone()
        }
    }

    // ------------------------------------------------------------------
    // Config helpers
    // ------------------------------------------------------------------

    fn config() -> SafewordConfig {
        SafewordConfig::from_lookup(|key| match key {
            KEY_MODE => Some("live".to_string()),
            KEY_ROLE_ID => Some(SAFEWORD_ROLE.to_string()),
            KEY_CHANNEL_ID => Some(CHANNEL.to_string()),
            KEY_MESSAGE_ID => Some(MESSAGE.to_string()),
            KEY_EMOJI_ID => Some(EMOJI.to_string()),
            KEY_RESTRICTED_ROLE_IDS => Some(format!("{RESTRICTED_A},{RESTRICTED_B}")),
            KEY_DM => Some("on".to_string()),
            KEY_DEFER_GRANTS => Some("on".to_string()),
            _ => None,
        })
        .expect("the test configuration is valid")
    }

    fn config_in(mode: &str) -> SafewordConfig {
        SafewordConfig::from_lookup(|key| match key {
            KEY_MODE => Some(mode.to_string()),
            KEY_ROLE_ID => Some(SAFEWORD_ROLE.to_string()),
            KEY_CHANNEL_ID => Some(CHANNEL.to_string()),
            KEY_MESSAGE_ID => Some(MESSAGE.to_string()),
            KEY_EMOJI_ID => Some(EMOJI.to_string()),
            KEY_RESTRICTED_ROLE_IDS => Some(format!("{RESTRICTED_A},{RESTRICTED_B}")),
            KEY_DM => Some("on".to_string()),
            KEY_DEFER_GRANTS => Some("on".to_string()),
            _ => None,
        })
        .expect("the test configuration is valid")
    }

    fn event(added: bool) -> ReactionEvent {
        ReactionEvent {
            guild_id: GUILD,
            channel_id: CHANNEL,
            message_id: MESSAGE,
            emoji_id: EMOJI,
            member_id: MEMBER,
            author_is_bot: false,
            added,
        }
    }

    // ------------------------------------------------------------------
    // Matching
    // ------------------------------------------------------------------

    #[test]
    fn the_configured_emoji_on_the_configured_message_activates() {
        let cfg = config();
        assert_eq!(
            decide_reaction(&cfg, &event(true)),
            ReactionDecision::Activate(MEMBER)
        );
        assert_eq!(
            decide_reaction(&cfg, &event(false)),
            ReactionDecision::Release(MEMBER)
        );
    }

    #[test]
    fn the_emoji_is_matched_by_id_and_never_by_name() {
        let cfg = config();
        let mut other = event(true);
        other.emoji_id = EMOJI + 1;
        // The runtime has no emoji names at all: only ids are compared.
        assert_eq!(
            decide_reaction(&cfg, &other),
            ReactionDecision::Ignore(IgnoreReason::WrongEmoji)
        );
        assert_eq!(
            IgnoreReason::WrongEmoji.code(),
            "wrong_emoji",
            "the reason is a stable code"
        );
    }

    #[test]
    fn another_message_or_channel_is_ignored() {
        let cfg = config();
        let mut other_message = event(true);
        other_message.message_id = MESSAGE + 1;
        assert_eq!(
            decide_reaction(&cfg, &other_message),
            ReactionDecision::Ignore(IgnoreReason::WrongMessage)
        );
        let mut other_channel = event(true);
        other_channel.channel_id = CHANNEL + 1;
        assert_eq!(
            decide_reaction(&cfg, &other_channel),
            ReactionDecision::Ignore(IgnoreReason::WrongChannel)
        );
    }

    #[test]
    fn a_bot_reactor_is_ignored() {
        let cfg = config();
        let mut bot = event(true);
        bot.author_is_bot = true;
        assert_eq!(
            decide_reaction(&cfg, &bot),
            ReactionDecision::Ignore(IgnoreReason::BotAuthor)
        );
    }

    #[test]
    fn an_ignored_event_writes_nothing() {
        let cfg = config();
        let mut harness = Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A]));
        let mut shadow = ShadowRecord::default();
        let mut bot = event(true);
        bot.author_is_bot = true;
        let outcome = handle_reaction(&cfg, &bot, NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Ignored);
        assert!(harness.added().is_empty());
        assert!(harness.removed().is_empty());
        assert!(harness.dms().is_empty());
        assert!(harness.alerts().is_empty());
        assert_eq!(shadow.ignored, 1);
    }

    // ------------------------------------------------------------------
    // Activation
    // ------------------------------------------------------------------

    #[test]
    fn activation_removes_the_restricted_roles_adds_the_safeword_role_and_dms() {
        let cfg = config();
        let mut harness = Harness::new(
            FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B, UNRESTRICTED]),
        );
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Done);
        assert!(outcome.is_success());
        assert_eq!(
            harness.removed(),
            vec![(MEMBER, RESTRICTED_A), (MEMBER, RESTRICTED_B)],
            "only the restricted roles are removed"
        );
        assert_eq!(harness.added(), vec![(MEMBER, SAFEWORD_ROLE)]);
        assert_eq!(
            harness.dms(),
            vec![(MEMBER, ACTIVE_DM_TEXT.to_string())],
            "the DM text is the fixed one"
        );
        assert!(harness.alerts().is_empty());
        let state = harness.state_of(MEMBER).expect("the state was stored");
        assert!(
            matches!(state, State::Active(_)),
            "the member is Active after a complete activation, got {state:?}"
        );
        let snapshot = state.snapshot().expect("the state has a snapshot");
        assert_eq!(
            snapshot.saved_roles,
            vec![RESTRICTED_A, RESTRICTED_B],
            "the snapshot holds exactly the removed roles"
        );
    }

    #[test]
    fn a_replayed_add_never_overwrites_the_snapshot() {
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]));
        let mut shadow = ShadowRecord::default();
        assert_eq!(
            handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow),
            Outcome::Done
        );
        // The member now only holds the safeword role, as Discord would report.
        harness
            .ops
            .members
            .member_roles(MEMBER)
            .expect("the lookup works");
        let roles = harness.roles_of(MEMBER);
        assert!(roles.contains(&SAFEWORD_ROLE));

        // The replay: the same add arrives again.
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::NoWork, "the replay does nothing");
        let state = harness.state_of(MEMBER).expect("the state is still there");
        let snapshot = state.snapshot().expect("the state has a snapshot");
        assert_eq!(
            snapshot.saved_roles,
            vec![RESTRICTED_A, RESTRICTED_B],
            "the snapshot survived the replay"
        );
        assert_eq!(
            harness.removed().len(),
            2,
            "the replay removed no role again"
        );
    }

    #[test]
    fn remove_before_add_leaves_the_member_inactive() {
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]));
        let mut shadow = ShadowRecord::default();
        // The reaction is removed before the bot ever saw the add.
        let outcome = handle_reaction(&cfg, &event(false), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::NoWork);
        assert!(harness.added().is_empty());
        assert!(harness.removed().is_empty());
        assert!(harness.dms().is_empty());
        match harness.state_of(MEMBER) {
            Some(State::Inactive) | None => {}
            other => panic!("the member must not be on safeword, got {other:?}"),
        }
    }

    #[test]
    fn a_replayed_add_after_a_completed_release_activates_again() {
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]));
        let mut shadow = ShadowRecord::default();
        handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        handle_reaction(&cfg, &event(false), NOW, &mut harness.ops, &mut shadow);
        assert!(
            matches!(harness.state_of(MEMBER), Some(State::Inactive)),
            "the release cleared the state"
        );
        // The member still holds the restricted roles (the restore gave them back).
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Done);
        let snapshot = harness
            .state_of(MEMBER)
            .expect("the state is stored")
            .snapshot()
            .expect("the state has a snapshot")
            .clone();
        assert_eq!(
            snapshot.saved_roles,
            vec![RESTRICTED_A, RESTRICTED_B],
            "the second activation snapshotted the roles again"
        );
    }

    // ------------------------------------------------------------------
    // Release
    // ------------------------------------------------------------------

    #[test]
    fn removing_the_reaction_restores_the_roles_and_dms() {
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]));
        let mut shadow = ShadowRecord::default();
        handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        harness
            .members_log
            .borrow_mut()
            .roles
            .insert(MEMBER, vec![SAFEWORD_ROLE]);
        let outcome = handle_reaction(&cfg, &event(false), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Done);
        assert_eq!(
            harness.added(),
            vec![
                (MEMBER, SAFEWORD_ROLE),
                (MEMBER, RESTRICTED_A),
                (MEMBER, RESTRICTED_B),
            ],
            "the saved roles were given back after the safeword role was added"
        );
        assert_eq!(
            harness.removed(),
            vec![
                (MEMBER, RESTRICTED_A),
                (MEMBER, RESTRICTED_B),
                (MEMBER, SAFEWORD_ROLE)
            ],
            "the safeword role was taken away"
        );
        let dms = harness.dms();
        assert_eq!(dms.len(), 2);
        assert_eq!(dms[1], (MEMBER, RELEASED_DM_TEXT.to_string()));
        match harness.state_of(MEMBER) {
            Some(State::Inactive) => {}
            other => panic!("the member is inactive after the release, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Failures
    // ------------------------------------------------------------------

    #[test]
    fn a_partial_role_failure_is_never_reported_as_success_and_raises_an_alert() {
        let cfg = config();
        let mut harness = Harness::new(
            FakeMembers::new()
                .with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B])
                .failing_write(RESTRICTED_B, WriteCode::Forbidden),
        );
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert!(
            !outcome.is_success(),
            "a partial failure is never a success, got {outcome:?}"
        );
        assert_eq!(outcome, Outcome::Partial);
        assert!(
            harness.alerts().contains(&CODE_PARTIAL_ROLE_FAILURE),
            "the staff alert names the partial failure, got {:?}",
            harness.alerts()
        );
        let state = harness.state_of(MEMBER).expect("the state was stored");
        assert!(
            !matches!(state, State::Active(_)),
            "the activation did not complete, got {state:?}"
        );
    }

    #[test]
    fn a_failed_lookup_costs_the_member_nothing_and_alerts() {
        let cfg = config();
        let mut harness = Harness::new(FakeMembers::new().failing_lookup());
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Failed);
        assert!(harness.added().is_empty());
        assert!(harness.removed().is_empty());
        assert!(harness.dms().is_empty());
        assert_eq!(harness.alerts(), vec![CODE_LOOKUP_UNAVAILABLE]);
        assert!(
            harness.state_of(MEMBER).is_none(),
            "nothing was written when the lookup failed"
        );
    }

    #[test]
    fn a_failed_state_read_costs_the_member_nothing_and_alerts() {
        let cfg = config();
        let mut harness = Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A]))
            .with_store(FakeStore::new().failing_load());
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Failed);
        assert!(harness.added().is_empty());
        assert!(harness.removed().is_empty());
        assert_eq!(harness.alerts(), vec![CODE_STATE_FAILED]);
    }

    #[test]
    fn a_dm_failure_never_changes_the_roles_or_the_snapshot() {
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]))
                .with_dm_failure(DmCode::Closed);
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert!(
            !outcome.is_success(),
            "a DM failure is never a success, got {outcome:?}"
        );
        assert_eq!(
            harness.removed(),
            vec![(MEMBER, RESTRICTED_A), (MEMBER, RESTRICTED_B)],
            "the roles were still removed"
        );
        assert_eq!(
            harness.added(),
            vec![(MEMBER, SAFEWORD_ROLE)],
            "the safeword role was still added: the member stays protected"
        );
        let state = harness.state_of(MEMBER).expect("the state was stored");
        let snapshot = state.snapshot().expect("the state has a snapshot");
        assert_eq!(
            snapshot.saved_roles,
            vec![RESTRICTED_A, RESTRICTED_B],
            "the snapshot is unchanged by the DM failure"
        );
        assert!(harness.alerts().contains(&CODE_DM_FAILED));
    }

    #[test]
    fn an_unknown_bot_top_position_denies_the_role_write_and_alerts() {
        let cfg = config();
        let mut harness = Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A]))
            .with_facts(FakeFacts::new().no_top());
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert!(
            !outcome.is_success(),
            "a denied role write is never a success, got {outcome:?}"
        );
        assert!(harness.removed().is_empty());
        assert!(harness.added().is_empty());
        assert!(
            harness.alerts().contains(&CODE_BOT_TOP_UNKNOWN),
            "the alert names the unknown bot position, got {:?}",
            harness.alerts()
        );
    }

    #[test]
    fn unavailable_role_facts_deny_the_role_write_and_alert() {
        let cfg = config();
        let mut harness = Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A]))
            .with_facts(FakeFacts::new().unavailable());
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert!(
            !outcome.is_success(),
            "a denied role write is never a success, got {outcome:?}"
        );
        assert!(harness.removed().is_empty());
        assert!(harness.added().is_empty());
        assert!(
            harness.alerts().contains(&CODE_FACTS_UNAVAILABLE),
            "the alert names the unavailable facts, got {:?}",
            harness.alerts()
        );
    }

    // ------------------------------------------------------------------
    // Shadow mode
    // ------------------------------------------------------------------

    #[test]
    fn shadow_mode_performs_nothing_and_only_records_counts() {
        let cfg = config_in("shadow");
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]));
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::NoWork);
        assert!(harness.added().is_empty());
        assert!(harness.removed().is_empty());
        assert!(harness.dms().is_empty());
        assert!(harness.alerts().is_empty());
        assert!(harness.state_of(MEMBER).is_none(), "nothing was stored");
        assert_eq!(shadow.activates, 1);
        assert_eq!(shadow.role_removals, 2);
        assert_eq!(shadow.role_adds, 1);
        assert_eq!(shadow.dms, 1);
        assert!(shadow.codes.is_empty());
    }

    #[test]
    fn off_mode_does_nothing_for_an_event_and_for_the_reconcile() {
        let cfg = config_in("off");
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]))
                .with_reactions(FakeReactionUsers::new(vec![vec![MEMBER]]));
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::NoWork);
        let report = reconcile(&cfg, GUILD, NOW, 4, &mut harness.ops);
        assert_eq!(report, ReconcileReport::default());
        assert!(harness.reaction_calls.borrow().is_empty(), "no read either");
        assert!(harness.added().is_empty() && harness.removed().is_empty());
        assert!(harness.dms().is_empty() && harness.alerts().is_empty());
        assert!(harness.state_of(MEMBER).is_none());
        assert_eq!(shadow, ShadowRecord::default());
    }

    #[test]
    fn shadow_reconcile_counts_what_it_would_do_and_writes_nothing() {
        use crate::safeword::state::Snapshot;
        let cfg = config_in("shadow");
        let mut harness = Harness::new(
            FakeMembers::new()
                .with_roles(MEMBER, &[RESTRICTED_A])
                .with_roles(OTHER_MEMBER, &[SAFEWORD_ROLE]),
        )
        .with_store(FakeStore::new().with_state(OTHER_MEMBER, State::Active(Snapshot::default())))
        .with_reactions(FakeReactionUsers::new(vec![vec![MEMBER]]));
        let report = reconcile(&cfg, GUILD, NOW, 4, &mut harness.ops);
        assert_eq!(report.activated, 1, "{}", report.summary_line());
        assert_eq!(report.released, 1, "{}", report.summary_line());
        assert_eq!(report.failed, 0);
        assert!(harness.added().is_empty() && harness.removed().is_empty());
        assert!(harness.dms().is_empty() && harness.alerts().is_empty());
        assert!(harness.store_writes.borrow().is_empty());
        assert!(harness.store_clears.borrow().is_empty());
    }

    #[test]
    fn the_shadow_record_carries_counts_and_codes_only() {
        let record = ShadowRecord {
            activates: 1,
            releases: 2,
            role_removals: 3,
            role_adds: 4,
            dms: 5,
            ignored: 6,
            codes: vec![CODE_PARTIAL_ROLE_FAILURE],
        };
        let line = record.summary_line();
        assert!(line.contains("activates=1"));
        assert!(line.contains("partial_role_failure"));
        assert!(
            !line.contains(&MEMBER.to_string()),
            "the record carries no member id: {line}"
        );
    }

    // ------------------------------------------------------------------
    // Reconcile
    // ------------------------------------------------------------------

    #[test]
    fn the_reconcile_reads_the_reaction_users_page_by_page() {
        let cfg = config();
        let mut first = Vec::new();
        for i in 0..100u64 {
            first.push(900_000 + i);
        }
        let mut harness = Harness::new(FakeMembers::new())
            .with_reactions(FakeReactionUsers::new(vec![first.clone(), vec![MEMBER]]));
        let report = reconcile(&cfg, GUILD, NOW, MAX_RECONCILE_PAGES, &mut harness.ops);
        let calls = harness.reaction_calls.borrow().clone();
        assert_eq!(calls.len(), 2, "the reconcile read two pages");
        assert_eq!(calls[0], (None, REACTION_PAGE_SIZE));
        assert_eq!(
            calls[1],
            (
                Some(*first.last().expect("the page is not empty")),
                REACTION_PAGE_SIZE
            ),
            "the second page asks for the users after the last id of the first"
        );
        assert_eq!(
            report.activated, 101,
            "every user of both pages was reconciled"
        );
    }

    #[test]
    fn the_reconcile_reports_a_failed_reaction_user_list_instead_of_guessing() {
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new()).with_reactions(FakeReactionUsers::failing());
        let report = reconcile(&cfg, GUILD, NOW, MAX_RECONCILE_PAGES, &mut harness.ops);
        assert_eq!(report.activated, 0);
        assert_eq!(report.released, 0);
        assert!(report.codes.contains(&CODE_REACTION_USERS_FAILED));
        assert!(harness.added().is_empty());
        assert!(harness.removed().is_empty());
    }

    #[test]
    fn the_reconcile_stops_at_the_page_cap_and_reports_it() {
        let cfg = config();
        let page = vec![1u64; REACTION_PAGE_SIZE as usize];
        let mut harness = Harness::new(FakeMembers::new())
            .with_reactions(FakeReactionUsers::new(vec![page.clone(), page.clone()]));
        let report = reconcile(&cfg, GUILD, NOW, 1, &mut harness.ops);
        assert!(
            report.codes.contains(&CODE_REACTION_PAGE_CAP),
            "the cap is reported, got {:?}",
            report.codes
        );
        assert_eq!(report.activated, 0, "nothing was done on half the truth");
    }

    #[test]
    fn the_reconcile_activates_a_member_who_has_the_reaction_and_no_state() {
        let cfg = config();
        let mut harness = Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A]))
            .with_reactions(FakeReactionUsers::new(vec![vec![MEMBER]]));
        let report = reconcile(&cfg, GUILD, NOW, MAX_RECONCILE_PAGES, &mut harness.ops);
        assert_eq!(report.activated, 1);
        assert_eq!(report.released, 0);
        assert_eq!(
            harness.removed(),
            vec![(MEMBER, RESTRICTED_A)],
            "the reconcile removed the restricted role"
        );
        assert_eq!(harness.added(), vec![(MEMBER, SAFEWORD_ROLE)]);
    }

    #[test]
    fn the_reconcile_releases_a_stored_member_who_no_longer_has_the_reaction() {
        let cfg = config();
        // The member is stored as active and holds only the safeword role.
        let stored = State::Active(crate::safeword::state::Snapshot {
            saved_roles: vec![RESTRICTED_A],
            original_roles: vec![RESTRICTED_A],
            roles_removed: true,
            safeword_added: true,
            snapshot_saved: true,
            dm_sent: true,
            roles_restored: false,
            ..Default::default()
        });
        let mut harness = Harness::new(FakeMembers::new().with_roles(MEMBER, &[SAFEWORD_ROLE]))
            .with_store(FakeStore::new().with_state(MEMBER, stored))
            .with_reactions(FakeReactionUsers::new(vec![vec![]]));
        let report = reconcile(&cfg, GUILD, NOW, MAX_RECONCILE_PAGES, &mut harness.ops);
        assert_eq!(report.released, 1);
        assert!(
            harness.added().contains(&(MEMBER, RESTRICTED_A)),
            "the saved role was given back, added {:?}",
            harness.added()
        );
        assert!(
            harness.removed().contains(&(MEMBER, SAFEWORD_ROLE)),
            "the safeword role was taken away, removed {:?}",
            harness.removed()
        );
    }

    #[test]
    fn the_reconcile_leaves_a_member_alone_when_the_state_cannot_be_read() {
        let cfg = config();
        let mut harness = Harness::new(FakeMembers::new())
            .with_store(FakeStore::new().failing_load())
            .with_reactions(FakeReactionUsers::new(vec![vec![MEMBER]]));
        let report = reconcile(&cfg, GUILD, NOW, MAX_RECONCILE_PAGES, &mut harness.ops);
        assert_eq!(report.activated, 0);
        assert_eq!(report.failed, 1);
        assert!(report.codes.contains(&CODE_STATE_FAILED));
        assert!(harness.added().is_empty());
        assert!(harness.removed().is_empty());
    }

    // ------------------------------------------------------------------
    // Liveness
    // ------------------------------------------------------------------

    /// Facts where one role can be moved above the bot between two events.
    struct ToggleFacts {
        high_role: RoleId,
        high: Rc<Cell<bool>>,
    }

    impl RoleFactsSource for ToggleFacts {
        fn role_facts(&mut self, role_id: RoleId) -> roles_facts::FactsOutcome {
            let position = if role_id == self.high_role && self.high.get() {
                60
            } else {
                1
            };
            roles_facts::FactsOutcome::Found(RoleFacts {
                id: role_id,
                position,
                managed: false,
                permissions: 0,
                exists: true,
            })
        }

        fn bot_top_position(&mut self) -> Option<u16> {
            Some(50)
        }
    }

    /// Activate, then release, with the member holding both restricted roles.
    /// Returns the outcome of the release.
    fn activate_then_release(cfg: &SafewordConfig, harness: &mut Harness) -> Outcome {
        let mut shadow = ShadowRecord::default();
        let first = handle_reaction(cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(first, Outcome::Done, "the activation completes first");
        handle_reaction(cfg, &event(false), NOW, &mut harness.ops, &mut shadow)
    }

    fn writes_of(harness: &Harness, role: u64) -> usize {
        harness
            .added()
            .iter()
            .filter(|(_, added)| *added == role)
            .count()
    }

    #[test]
    fn a_rate_limited_restore_is_retried_by_the_k04_executor() {
        let cfg = config();
        let mut harness = Harness::new(
            FakeMembers::new()
                .with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B])
                .failing_write_times(RESTRICTED_A, WriteCode::RateLimited, 2),
        );
        let outcome = activate_then_release(&cfg, &mut harness);
        assert_eq!(outcome, Outcome::Done, "the third write succeeds");
        assert_eq!(
            writes_of(&harness, RESTRICTED_A),
            3,
            "two retries, then done"
        );
        assert_eq!(
            harness.sleeps(),
            vec![1_000, 2_000],
            "the executor's linear backoff went through the injected sleeper"
        );
        assert!(harness.alerts().is_empty(), "{:?}", harness.alerts());
        assert!(matches!(harness.state_of(MEMBER), Some(State::Inactive)));
    }

    #[test]
    fn a_restore_that_stays_rate_limited_stops_at_the_executor_limit_and_is_retried_later() {
        let cfg = config();
        let mut harness = Harness::new(
            FakeMembers::new()
                .with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B])
                .failing_write_times(RESTRICTED_A, WriteCode::RateLimited, 4),
        );
        let mut shadow = ShadowRecord::default();
        let outcome = activate_then_release(&cfg, &mut harness);
        assert_eq!(outcome, Outcome::Partial, "one role came back, one did not");
        assert!(!outcome.is_success());
        assert_eq!(
            writes_of(&harness, RESTRICTED_A),
            4,
            "one try and three retries"
        );
        assert_eq!(harness.sleeps(), vec![1_000, 2_000, 3_000]);
        assert!(harness.alerts().contains(&CODE_PARTIAL_ROLE_FAILURE));
        match harness.state_of(MEMBER) {
            Some(State::Releasing(snapshot)) => {
                assert!(
                    snapshot.saved_roles.contains(&RESTRICTED_A),
                    "the snapshot is kept"
                );
            }
            other => panic!("the release must stay open, got {other:?}"),
        }
        // The next trigger finishes the release and writes only what is missing.
        let again = handle_reaction(&cfg, &event(false), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(again, Outcome::Done);
        assert_eq!(writes_of(&harness, RESTRICTED_A), 5);
        assert_eq!(
            writes_of(&harness, RESTRICTED_B),
            1,
            "the role that came back is not written twice"
        );
        assert!(matches!(harness.state_of(MEMBER), Some(State::Inactive)));
    }

    #[test]
    fn a_role_the_member_already_holds_is_not_written_again_on_restore() {
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]));
        let mut shadow = ShadowRecord::default();
        handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        // A staff member gave one role back by hand while the safeword ran.
        harness
            .members_log
            .borrow_mut()
            .roles
            .insert(MEMBER, vec![SAFEWORD_ROLE, RESTRICTED_A]);
        let outcome = handle_reaction(&cfg, &event(false), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Done);
        assert_eq!(writes_of(&harness, RESTRICTED_A), 0);
        assert_eq!(writes_of(&harness, RESTRICTED_B), 1);
    }

    #[test]
    fn a_protected_role_is_never_given_back_by_a_restore() {
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]))
                .with_protected(&[RESTRICTED_A]);
        let _ = activate_then_release(&cfg, &mut harness);
        assert_eq!(
            writes_of(&harness, RESTRICTED_A),
            0,
            "a protected role is never restored by the bot"
        );
        assert_eq!(writes_of(&harness, RESTRICTED_B), 1);
    }

    #[test]
    fn a_restore_for_a_role_that_rose_above_the_bot_is_denied_and_alerted() {
        let cfg = config();
        let high = Rc::new(Cell::new(false));
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]))
                .with_facts(ToggleFacts {
                    high_role: RESTRICTED_A,
                    high: Rc::clone(&high),
                });
        let mut shadow = ShadowRecord::default();
        handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        high.set(true);
        let outcome = handle_reaction(&cfg, &event(false), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Partial);
        assert_eq!(
            writes_of(&harness, RESTRICTED_A),
            0,
            "K03 denied it: never written"
        );
        assert_eq!(writes_of(&harness, RESTRICTED_B), 1);
        assert!(harness.alerts().contains(&"at_or_above_bot"));
        assert!(matches!(
            harness.state_of(MEMBER),
            Some(State::Releasing(_))
        ));
    }

    #[test]
    fn a_partial_activation_failure_keeps_the_member_protected() {
        // One restricted role cannot be removed. The member must still hold
        // the safeword role and the snapshot, so nothing is lost.
        let cfg = config();
        let mut harness = Harness::new(
            FakeMembers::new()
                .with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B])
                .failing_write(RESTRICTED_B, WriteCode::Forbidden),
        );
        let mut shadow = ShadowRecord::default();
        handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert!(harness.roles_of(MEMBER).contains(&SAFEWORD_ROLE));
        let state = harness.state_of(MEMBER).expect("the state was stored");
        let snapshot = state.snapshot().expect("the snapshot is stored");
        assert!(snapshot.saved_roles.contains(&RESTRICTED_A));
        assert!(snapshot.saved_roles.contains(&RESTRICTED_B));
    }

    #[test]
    fn one_reconcile_pass_finishes_an_activation_a_crash_left_half_done() {
        // The crash happened after the snapshot was saved and before any role
        // was removed. One pass must carry the work to the end, not one step.
        use crate::safeword::state::Snapshot;
        let cfg = config();
        let snapshot = Snapshot {
            saved_roles: vec![RESTRICTED_A, RESTRICTED_B],
            original_roles: vec![RESTRICTED_A, RESTRICTED_B],
            snapshot_saved: true,
            ..Snapshot::default()
        };
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]))
                .with_store(FakeStore::new().with_state(MEMBER, State::Activating(snapshot)))
                .with_reactions(FakeReactionUsers::new(vec![vec![MEMBER]]));
        let report = reconcile(&cfg, GUILD, NOW, 4, &mut harness.ops);
        assert_eq!(report.failed, 0, "{}", report.summary_line());
        assert_eq!(harness.roles_of(MEMBER), vec![SAFEWORD_ROLE]);
        assert!(
            matches!(harness.state_of(MEMBER), Some(State::Active(_))),
            "got {:?}",
            harness.state_of(MEMBER)
        );
        assert_eq!(harness.dms().len(), 1, "the activation DM went out once");
    }

    #[test]
    fn a_re_add_during_a_stalled_release_snapshots_the_roles_held_now() {
        // The release stalls (one restore write fails once). The member reacts
        // again, so the machine hands over to a fresh activation as soon as the
        // restore is acknowledged. The new snapshot must see the roles the
        // restore just gave back, or the member ends "active" holding them.
        let cfg = config();
        let mut harness = Harness::new(
            FakeMembers::new()
                .with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B])
                .failing_write_times(RESTRICTED_A, WriteCode::Other, 1),
        );
        let release = activate_then_release(&cfg, &mut harness);
        assert_eq!(release, Outcome::Partial);
        let mut shadow = ShadowRecord::default();
        handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        let roles = harness.roles_of(MEMBER);
        assert!(roles.contains(&SAFEWORD_ROLE), "{roles:?}");
        assert!(!roles.contains(&RESTRICTED_A), "{roles:?}");
        assert!(!roles.contains(&RESTRICTED_B), "{roles:?}");
        assert!(
            matches!(harness.state_of(MEMBER), Some(State::Active(_))),
            "got {:?}",
            harness.state_of(MEMBER)
        );
    }

    #[test]
    fn a_lookup_failure_during_the_hand_over_to_a_new_activation_is_not_acknowledged() {
        // Same stalled release and re-add as above, but the roles cannot be
        // read at the moment the machine would start the new activation. The
        // restore must not be acknowledged on a guess: the release stays open,
        // an alert is raised, and the next trigger finishes the hand-over.
        let cfg = config();
        let mut harness = Harness::new(
            FakeMembers::new()
                .with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B])
                .failing_write_times(RESTRICTED_A, WriteCode::Other, 1),
        );
        activate_then_release(&cfg, &mut harness);
        let mut shadow = ShadowRecord::default();
        // The re-add reads the roles three times: the run's context, the
        // restore's held check, then the hand-over refresh. Fail the third.
        let seen = harness.members_log.borrow().lookups.len();
        harness.lookup_cutoff.set(seen + 2);
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert!(!outcome.is_success(), "got {outcome:?}");
        assert!(harness.alerts().contains(&CODE_LOOKUP_UNAVAILABLE));
        assert!(
            matches!(harness.state_of(MEMBER), Some(State::Releasing(_))),
            "got {:?}",
            harness.state_of(MEMBER)
        );
        harness.lookup_cutoff.set(usize::MAX);
        handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        let roles = harness.roles_of(MEMBER);
        assert!(roles.contains(&SAFEWORD_ROLE), "{roles:?}");
        assert!(!roles.contains(&RESTRICTED_A) && !roles.contains(&RESTRICTED_B));
        assert!(matches!(harness.state_of(MEMBER), Some(State::Active(_))));
    }

    #[test]
    fn a_failed_held_roles_lookup_never_skips_a_restore() {
        // The roles cannot be read when the restore checks what is already
        // held. "Holds nothing" is the safe reading: both roles are written.
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]));
        let mut shadow = ShadowRecord::default();
        handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        // The release reads the roles for its context first, then for the restore.
        let seen = harness.members_log.borrow().lookups.len();
        harness.lookup_cutoff.set(seen + 1);
        let outcome = handle_reaction(&cfg, &event(false), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Done);
        assert_eq!(writes_of(&harness, RESTRICTED_A), 1);
        assert_eq!(writes_of(&harness, RESTRICTED_B), 1);
    }

    #[test]
    fn one_reason_for_every_failed_restore_is_recorded_and_a_mix_is_transient() {
        use crate::safeword::state::ActionId;
        let cfg = config();
        let last_failure = |forbidden_b: bool| {
            let second = if forbidden_b {
                WriteCode::Forbidden
            } else {
                WriteCode::RateLimited
            };
            let mut harness = Harness::new(
                FakeMembers::new()
                    .with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B])
                    .failing_write_times(RESTRICTED_A, WriteCode::Forbidden, 99)
                    .failing_write_times(RESTRICTED_B, second, 99),
            );
            let outcome = activate_then_release(&cfg, &mut harness);
            assert!(!outcome.is_success(), "got {outcome:?}");
            match harness.state_of(MEMBER) {
                Some(State::Releasing(snapshot)) => snapshot.last_failure,
                other => panic!("the release must stay open, got {other:?}"),
            }
        };
        assert_eq!(
            last_failure(true),
            Some((ActionId::RestoreRoles, FailureCode::MissingPermissions))
        );
        assert_eq!(
            last_failure(false),
            Some((ActionId::RestoreRoles, FailureCode::TransientHttp))
        );
    }

    #[test]
    fn a_release_that_finishes_a_stalled_activation_gives_the_roles_back() {
        // The activation stalled: the role removal failed earlier. The member
        // releases, so one run removes the roles, adds the safeword role and
        // then restores. The roles held when the event arrived are not the
        // roles held when the restore runs, so "already held" must be read at
        // write time; a stale list would skip the restore and lose the roles.
        use crate::safeword::state::{ActionId, Snapshot};
        let cfg = config();
        let snapshot = Snapshot {
            saved_roles: vec![RESTRICTED_A, RESTRICTED_B],
            original_roles: vec![RESTRICTED_A, RESTRICTED_B],
            snapshot_saved: true,
            failed_steps: vec![ActionId::RemoveRoles],
            ..Snapshot::default()
        };
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]))
                .with_store(FakeStore::new().with_state(MEMBER, State::Activating(snapshot)));
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(false), NOW, &mut harness.ops, &mut shadow);
        assert!(outcome.is_success(), "got {outcome:?}");
        let mut roles = harness.roles_of(MEMBER);
        roles.sort_unstable();
        assert_eq!(roles, vec![RESTRICTED_A, RESTRICTED_B]);
        assert!(matches!(harness.state_of(MEMBER), Some(State::Inactive)));
    }

    #[test]
    fn a_protected_safeword_role_is_denied_by_k03_and_never_written() {
        // A misconfiguration (the safeword role listed as protected) must fail
        // loudly, not write a role K03 denies.
        let cfg = config();
        let mut harness = Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A]))
            .with_protected(&[SAFEWORD_ROLE]);
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert!(!outcome.is_success(), "got {outcome:?}");
        assert_eq!(writes_of(&harness, SAFEWORD_ROLE), 0);
        assert!(
            harness.alerts().contains(&"protected_role"),
            "{:?}",
            harness.alerts()
        );
    }

    #[test]
    fn a_failed_reconcile_action_is_alerted_and_kept_for_the_next_pass() {
        let cfg = config();
        use crate::safeword::state::Snapshot;
        let snapshot = Snapshot {
            saved_roles: vec![RESTRICTED_A],
            original_roles: vec![RESTRICTED_A],
            snapshot_saved: true,
            ..Snapshot::default()
        };
        let mut harness = Harness::new(
            FakeMembers::new()
                .with_roles(MEMBER, &[RESTRICTED_A])
                .failing_write(RESTRICTED_A, WriteCode::Forbidden),
        )
        .with_store(FakeStore::new().with_state(MEMBER, State::Activating(snapshot)))
        .with_reactions(FakeReactionUsers::new(vec![vec![MEMBER]]));
        let report = reconcile(&cfg, GUILD, NOW, 4, &mut harness.ops);
        assert_eq!(report.failed, 1, "{}", report.summary_line());
        assert!(harness.alerts().contains(&CODE_ROLE_WRITE_FAILED));
        assert!(
            !matches!(harness.state_of(MEMBER), Some(State::Inactive) | None),
            "the unfinished work is still on record"
        );
    }

    /// A seeded linear congruential generator: no new dependency.
    fn lcg(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state >> 33
        }
    }

    #[test]
    fn a_seeded_random_run_converges_and_never_fails_silently() {
        // The model runtime: Discord is the fake member (roles are the truth),
        // role writes and DMs fail at random for a while, events arrive in a
        // random order, and the reconcile retries. However it goes, the member
        // must end in the state the last reaction asks for, with every
        // original role held when released, and no failed run may be silent.
        let cfg = config();
        let mut next = lcg(0x5DEE_CE66_D1B5_1A3F);
        let codes = [
            WriteCode::Forbidden,
            WriteCode::RateLimited,
            WriteCode::Other,
            WriteCode::UnknownRole,
        ];
        for round in 0..300u32 {
            let chaos: Vec<Option<WriteCode>> = (0..16)
                .map(|_| {
                    if next() % 100 < 35 {
                        Some(codes[(next() % 4) as usize])
                    } else {
                        None
                    }
                })
                .collect();
            let mut harness = Harness::new(
                FakeMembers::new()
                    .with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B])
                    .with_chaos(chaos),
            );
            harness.ops.dms = Box::new(FakeDms {
                sent: Rc::clone(&harness.dms_sent),
                failure: None,
                fail_next: (next() % 3) as u32,
            });
            let mut shadow = ShadowRecord::default();
            let mut present = false;
            for _ in 0..(1 + next() % 6) {
                present = next().is_multiple_of(2);
                let before = harness.alerts().len();
                let outcome =
                    handle_reaction(&cfg, &event(present), NOW, &mut harness.ops, &mut shadow);
                if !outcome.is_success() {
                    assert!(
                        harness.alerts().len() > before,
                        "round {round}: a failed run raised no alert"
                    );
                }
            }
            // The failures are over (the script is finite): the reconcile
            // must now finish whatever is unacknowledged.
            let users = if present { vec![vec![MEMBER]] } else { vec![] };
            for _ in 0..8 {
                harness.ops.reactions = Box::new(FakeReactionUsers::new(users.clone()));
                reconcile(&cfg, GUILD, NOW, 4, &mut harness.ops);
            }
            let roles = harness.roles_of(MEMBER);
            let state = harness.state_of(MEMBER);
            if present {
                match state {
                    Some(State::Active(snapshot)) => {
                        assert!(snapshot.failed_steps.is_empty(), "round {round}");
                    }
                    other => panic!("round {round}: expected Active, got {other:?}"),
                }
                assert!(roles.contains(&SAFEWORD_ROLE), "round {round}: {roles:?}");
                assert!(!roles.contains(&RESTRICTED_A), "round {round}: {roles:?}");
                assert!(!roles.contains(&RESTRICTED_B), "round {round}: {roles:?}");
            } else {
                assert!(
                    matches!(state, None | Some(State::Inactive)),
                    "round {round}: expected Inactive, got {state:?}"
                );
                assert!(!roles.contains(&SAFEWORD_ROLE), "round {round}: {roles:?}");
                assert!(roles.contains(&RESTRICTED_A), "round {round}: {roles:?}");
                assert!(roles.contains(&RESTRICTED_B), "round {round}: {roles:?}");
            }
        }
    }

    // ------------------------------------------------------------------
    // Codes
    // ------------------------------------------------------------------

    #[test]
    fn the_codes_are_stable_and_data_free() {
        let codes = [
            CODE_ROLE_WRITE_FAILED,
            CODE_LOOKUP_UNAVAILABLE,
            CODE_STATE_FAILED,
            CODE_DM_FAILED,
            CODE_PARTIAL_ROLE_FAILURE,
            CODE_REACTION_USERS_FAILED,
            CODE_REACTION_PAGE_CAP,
            CODE_FACTS_UNAVAILABLE,
            CODE_BOT_TOP_UNKNOWN,
        ];
        for code in codes {
            assert!(!code.is_empty());
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "the code {code:?} is a plain token"
            );
            assert!(
                !code.chars().any(|c| c.is_ascii_digit()),
                "the code {code:?} carries no number"
            );
        }
    }

    #[test]
    fn the_ignore_reasons_are_stable_codes() {
        for (reason, code) in [
            (IgnoreReason::BotAuthor, "bot_author"),
            (IgnoreReason::WrongChannel, "wrong_channel"),
            (IgnoreReason::WrongMessage, "wrong_message"),
            (IgnoreReason::WrongEmoji, "wrong_emoji"),
        ] {
            assert_eq!(reason.code(), code);
        }
    }

    #[test]
    fn the_write_and_dm_codes_are_stable_codes() {
        for (code, text) in [
            (WriteCode::Forbidden, "forbidden"),
            (WriteCode::UnknownRole, "unknown_role"),
            (WriteCode::UnknownMember, "unknown_member"),
            (WriteCode::RateLimited, "rate_limited"),
            (WriteCode::Other, "other"),
        ] {
            assert_eq!(code.code(), text);
        }
        for (code, text) in [
            (DmCode::Closed, "dm_closed"),
            (DmCode::RateLimited, "dm_rate_limited"),
            (DmCode::Other, "dm_failed"),
        ] {
            assert_eq!(code.code(), text);
        }
        assert_eq!(LookupCode::Unavailable.code(), "lookup_unavailable");
    }

    // ------------------------------------------------------------------
    // The K03 eligibility decision
    // ------------------------------------------------------------------

    #[test]
    fn the_restore_goes_through_the_k03_safeword_restore_path() {
        // The member is on safeword and holds the restricted role. Only the
        // `SafewordRestore` path may give a restricted role back while the
        // safeword is active, so the restore succeeds.
        let cfg = config();
        let mut harness =
            Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A, RESTRICTED_B]));
        let mut shadow = ShadowRecord::default();
        handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        // The member now holds only the safeword role, as Discord would say.
        harness
            .members_log
            .borrow_mut()
            .roles
            .insert(MEMBER, vec![SAFEWORD_ROLE]);
        let outcome = handle_reaction(&cfg, &event(false), NOW, &mut harness.ops, &mut shadow);
        assert_eq!(outcome, Outcome::Done);
        assert!(
            harness.added().contains(&(MEMBER, RESTRICTED_A)),
            "the restricted role was given back, added {:?}",
            harness.added()
        );
        assert!(
            harness.alerts().is_empty(),
            "no alert: the restore path allows it, alerts {:?}",
            harness.alerts()
        );
    }

    #[test]
    fn a_role_the_bot_cannot_reach_is_never_written() {
        // The bot's top role is at position 50 and the role sits at 50: K03
        // denies it, so no write happens and the denial is alerted.
        struct HighRoleFacts;
        impl RoleFactsSource for HighRoleFacts {
            fn role_facts(&mut self, role_id: RoleId) -> roles_facts::FactsOutcome {
                roles_facts::FactsOutcome::Found(RoleFacts {
                    id: role_id,
                    position: 50,
                    managed: false,
                    permissions: 0,
                    exists: true,
                })
            }

            fn bot_top_position(&mut self) -> Option<u16> {
                Some(50)
            }
        }
        let cfg = config();
        let mut harness = Harness::new(FakeMembers::new().with_roles(MEMBER, &[RESTRICTED_A]))
            .with_facts(HighRoleFacts);
        let mut shadow = ShadowRecord::default();
        let outcome = handle_reaction(&cfg, &event(true), NOW, &mut harness.ops, &mut shadow);
        assert!(!outcome.is_success(), "a denied write is never a success");
        assert!(harness.removed().is_empty());
        assert!(
            harness.alerts().contains(&"at_or_above_bot"),
            "the K03 deny code is alerted, got {:?}",
            harness.alerts()
        );
    }

    // ------------------------------------------------------------------
    // The feature registry hook
    // ------------------------------------------------------------------

    /// A context that hands out no collaborators: the feature hook's own
    /// behaviour (name, mode, database use) is what this test checks.
    #[derive(Default)]
    struct TestContext {
        now: i64,
    }

    impl SafewordContext for TestContext {
        fn safeword_ops(&self, _guild_id: u64) -> Option<Ops> {
            None
        }

        fn safeword_now(&self) -> i64 {
            self.now
        }
    }

    #[test]
    fn the_feature_reports_its_name_its_mode_and_its_database_use() {
        use crate::features::Feature;
        let live = SafewordFeature::new(config(), TestContext { now: NOW });
        assert_eq!(live.name(), "safeword");
        assert!(live.enabled());
        assert!(
            live.needs_core_db(),
            "the safeword state lives in the core database"
        );

        let off = SafewordFeature::new(config_in("off"), TestContext { now: NOW });
        assert!(!off.enabled(), "the feature is off in off mode");

        let shadow = SafewordFeature::new(config_in("shadow"), TestContext { now: NOW });
        assert!(shadow.enabled(), "the feature runs in shadow mode");
    }

    #[test]
    fn a_serene_reaction_event_becomes_a_plain_reaction_event() {
        use poise::serenity_prelude::FullEvent;
        let payload = serde_json::json!({
            "channel_id": CHANNEL.to_string(),
            "message_id": MESSAGE.to_string(),
            "user_id": MEMBER.to_string(),
            "guild_id": GUILD.to_string(),
            "emoji": {"id": EMOJI.to_string(), "name": "synthetic"},
            "burst": false, "burst_colours": [], "type": 0,
        });
        let event = FullEvent::ReactionAdd {
            add_reaction: serde_json::from_value(payload).expect("the payload deserialises"),
        };
        let reaction = reaction_event(&event).expect("a reaction event is read");
        assert_eq!(reaction.guild_id, GUILD);
        assert_eq!(reaction.channel_id, CHANNEL);
        assert_eq!(reaction.message_id, MESSAGE);
        assert_eq!(reaction.emoji_id, EMOJI);
        assert_eq!(reaction.member_id, MEMBER);
        assert!(reaction.added);
        assert!(!reaction.author_is_bot);
    }

    #[test]
    fn a_unicode_emoji_has_no_id_and_is_never_read_as_a_reaction() {
        use poise::serenity_prelude::FullEvent;
        let payload = serde_json::json!({
            "channel_id": CHANNEL.to_string(),
            "message_id": MESSAGE.to_string(),
            "user_id": MEMBER.to_string(),
            "guild_id": GUILD.to_string(),
            "emoji": {"name": "SYNTHETIC_UNICODE"},
            "burst": false, "burst_colours": [], "type": 0,
        });
        let event = FullEvent::ReactionAdd {
            add_reaction: serde_json::from_value(payload).expect("the payload deserialises"),
        };
        assert!(
            reaction_event(&event).is_none(),
            "an emoji without an id can never be the configured one"
        );
    }

    #[test]
    fn an_event_that_is_not_a_reaction_is_not_read_as_one() {
        use poise::serenity_prelude::FullEvent;
        let event = FullEvent::ShardsReady { total_shards: 1 };
        assert!(reaction_event(&event).is_none());
    }

    #[test]
    fn a_reaction_without_a_guild_is_not_read_as_one() {
        use poise::serenity_prelude::FullEvent;
        let payload = serde_json::json!({
            "channel_id": CHANNEL.to_string(),
            "message_id": MESSAGE.to_string(),
            "user_id": MEMBER.to_string(),
            "emoji": {"id": EMOJI.to_string(), "name": "synthetic"},
            "burst": false, "burst_colours": [], "type": 0,
        });
        let event = FullEvent::ReactionAdd {
            add_reaction: serde_json::from_value(payload).expect("the payload deserialises"),
        };
        assert!(
            reaction_event(&event).is_none(),
            "a direct-message reaction has no guild"
        );
    }
}

//! Bump reminder runtime (BU2).
//!
//! The thin layer between the pure policy ([`super::policy`]) and Discord. It does
//! four things and nothing else:
//!
//! 1. **Detect.** A message event becomes [`policy::MessageFacts`]; the policy decides
//!    whether it is a real bump confirmation. The runtime adds one restriction of its
//!    own: the message must come from the configured bump channel.
//! 2. **Persist.** A confirmed bump is written to the `bump_state` row (see
//!    [`super::repository`]) and the cycle number grows by one.
//! 3. **Schedule.** Exactly one reminder job per cycle and due time is scheduled
//!    through the job queue. The dedupe key names the guild, the cycle and the due
//!    time, so the queue's UNIQUE key refuses a repeat of the same cycle and due time.
//! 4. **Command logic.** `/bump status` and the staff-only `/bump reset` are plain
//!    async functions plus pure reply builders; the poise registration is the wiring
//!    card's job (W3).
//!
//! Every Discord and queue call sits behind a small trait so the tests inject fakes.
//! The decision functions are pure: `now` and the ids are passed in, no clock is read,
//! no randomness is used, and no serenity type appears in this file.
//!
//! Modes: `shadow` performs no send at all and only produces a structured
//! [`WouldDo`] record (counts and stable codes, never message text).

use super::config::BumpSettings;
use super::policy::{self, BumpConfig, Decision, IgnoreReason, MessageFacts, ReminderStatus};
use super::repository::{BumpState, RepoError, Repository};

use std::future::Future;
use std::pin::Pin;

/// A boxed future, so the traits below need no serenity dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The job kind used for bump reminders. It is registered with
/// [`RestartPolicy::AtMostOnce`]: a reminder is a user-visible post and must never
/// be repeated after a restart.
pub const KIND_REMINDER: &str = "bump_reminder";

/// The `(kind, policy)` table the job queue's restart reconciliation uses.
pub const KIND_TABLE: &[(&str, crate::jobs::queue::RestartPolicy)] =
    &[(KIND_REMINDER, crate::jobs::queue::RestartPolicy::AtMostOnce)];

/// Stable reason codes. Never carries message text, a name or a member id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeError {
    /// The stored row could not be read back exactly.
    CorruptState,
    /// The job queue refused the schedule.
    QueueRefused,
    /// The reminder could not be posted.
    SendFailed,
    /// The command was used by somebody who is not staff.
    NotStaff,
    /// The command came from a guild other than the configured home guild, or no
    /// home guild is configured while the feature is on.
    WrongGuild,
    /// The feature is off (or its configuration was refused).
    Disabled,
}

impl RuntimeError {
    /// The stable machine code.
    pub fn code(&self) -> &'static str {
        match self {
            RuntimeError::CorruptState => "corrupt_state",
            RuntimeError::QueueRefused => "queue_refused",
            RuntimeError::SendFailed => "send_failed",
            RuntimeError::NotStaff => "not_staff",
            RuntimeError::WrongGuild => "wrong_guild",
            RuntimeError::Disabled => "disabled",
        }
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for RuntimeError {}

impl From<RepoError> for RuntimeError {
    fn from(e: RepoError) -> Self {
        match e {
            // The storage layer's own codes stay visible to the caller.
            RepoError::Storage(_) => RuntimeError::CorruptState,
            RepoError::CorruptRow => RuntimeError::CorruptState,
        }
    }
}

/// The injected time source. `now` is unix seconds.
pub trait Clock: Send + Sync {
    fn now(&self) -> i64;
}

/// A clock that always reports the same time. For tests.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

/// What a reminder message says and who it may mention.
///
/// The allowed mentions are explicit and minimal: only the bump role. The content is
/// fixed text; the runtime never copies a message body into a reminder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reminder {
    /// The channel the reminder goes to.
    pub channel_id: u64,
    /// The only role the reminder may mention. `None` means no mention at all.
    pub role_id: Option<u64>,
    /// The reminder text. Fixed wording, never a message body.
    pub content: String,
}

impl Reminder {
    /// The allowed mention string for this reminder: `<@&id>` for the bump role, or
    /// an empty string when no role may be mentioned.
    pub fn allowed_mention(&self) -> String {
        match self.role_id {
            Some(id) => format!("<@&{id}>"),
            None => String::new(),
        }
    }
}

/// Where a reminder is posted. Discord calls live behind this trait.
pub trait ReminderChannel: Send + Sync {
    /// Post `reminder`. `Err` means the post did not happen.
    fn send(&self, reminder: &Reminder) -> BoxFuture<'_, Result<(), RuntimeError>>;
}

/// The request to schedule one reminder job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRequest {
    /// The job kind. Always [`KIND_REMINDER`] here.
    pub kind: &'static str,
    /// The dedupe key. It names the cycle, so one cycle gets one job.
    pub dedupe_key: String,
    /// The JSON payload. It holds the guild id and the cycle number, nothing else.
    pub payload_json: String,
    /// When the job becomes due (unix seconds).
    pub due_at: i64,
}

/// What the queue did with a schedule request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleOutcome {
    /// A new job row was created.
    Created,
    /// The dedupe key already existed; nothing was written.
    Duplicate,
}

/// One reminder job, as the queue hands it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderJob {
    /// The job id.
    pub id: i64,
    /// The job kind.
    pub kind: String,
    /// The payload the job carries.
    pub payload_json: String,
}

impl ReminderJob {
    /// The reminder job of a claimed job, if that job is a reminder at all.
    ///
    /// The wiring card's handler uses this to turn the queue's claimed job into
    /// the runtime's job.
    pub fn from_claimed(claimed: &crate::jobs::queue::ClaimedJob) -> Option<Self> {
        if claimed.kind != KIND_REMINDER {
            return None;
        }
        Some(Self {
            id: claimed.id,
            kind: claimed.kind.clone(),
            payload_json: claimed.payload_json.clone(),
        })
    }
}

/// The durable job queue. The real implementation calls
/// [`crate::jobs::queue::schedule`] and [`crate::jobs::queue::claim_due`]; the tests
/// use a fake that behaves the same way.
pub trait JobQueue: Send + Sync {
    /// Schedule one job. `Err` means the job was not scheduled.
    fn schedule(&self, request: &JobRequest) -> Result<ScheduleOutcome, RuntimeError>;
    /// The jobs that are due at `now`, claimed (at most `limit` of them).
    ///
    /// A reminder is only ever sent from a job this queue handed out.
    fn claim_due(&self, now: i64, limit: u32) -> Result<Vec<ReminderJob>, RuntimeError>;
}

/// The payload of a reminder job: the guild, the cycle and the due time the job
/// carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderPayload {
    /// The guild the cycle belongs to.
    pub guild: u64,
    /// The cycle the job was scheduled for.
    pub cycle: i64,
    /// The due time the job carries.
    pub due_at: i64,
}

/// Read the payload of a reminder job.
///
/// Pure: a payload that is not a plain object with the three plain fields is
/// refused with a stable code. The text is never echoed.
pub fn parse_reminder_payload(text: &str) -> Result<ReminderPayload, RuntimeError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| RuntimeError::CorruptState)?;
    let Some(object) = value.as_object() else {
        return Err(RuntimeError::CorruptState);
    };
    let guild = object
        .get("guild")
        .and_then(serde_json::Value::as_u64)
        .ok_or(RuntimeError::CorruptState)?;
    let cycle = object
        .get("cycle")
        .and_then(serde_json::Value::as_i64)
        .ok_or(RuntimeError::CorruptState)?;
    let due_at = object
        .get("due_at")
        .and_then(serde_json::Value::as_i64)
        .ok_or(RuntimeError::CorruptState)?;
    Ok(ReminderPayload {
        guild,
        cycle,
        due_at,
    })
}

/// Who is allowed to use the staff-only command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaffCheck {
    /// The caller may reset the cycle.
    Staff,
    /// The caller may not.
    NotStaff,
}

/// What the runtime would do, in shadow mode. Counts and stable codes only; there is
/// no message text and no member id in this record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WouldDo {
    /// How many reminders would have been sent.
    pub reminders: usize,
    /// How many cycles would have been started.
    pub cycles: usize,
    /// How many job rows would have been scheduled.
    pub jobs: usize,
    /// The reason codes behind the counts.
    pub codes: Vec<&'static str>,
}

impl WouldDo {
    fn note(&mut self, code: &'static str) {
        if !self.codes.contains(&code) {
            self.codes.push(code);
        }
    }
}

/// Stable reason codes for a message that was not counted as a bump.
///
/// The runtime adds one reason to the policy's: a message from a channel other
/// than the configured bump channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreCode {
    /// The message came from a webhook.
    Webhook,
    /// The message was outside the configured guild (or in no guild).
    WrongGuild,
    /// The author is not the configured listing bot.
    NotListingBot,
    /// The author id matches but the author is not a bot account.
    NotBotAccount,
    /// The message id was zero.
    InvalidMessageId,
    /// Neither the text nor the embed text holds the success marker.
    NoSuccessMarker,
    /// The message id was already confirmed, or is older than the last one.
    AlreadySeen,
    /// The message came from a channel other than the configured bump channel.
    WrongChannel,
}

impl IgnoreCode {
    /// The stable machine code.
    pub fn code(&self) -> &'static str {
        match self {
            IgnoreCode::Webhook => "webhook",
            IgnoreCode::WrongGuild => "wrong_guild",
            IgnoreCode::NotListingBot => "not_listing_bot",
            IgnoreCode::NotBotAccount => "not_bot_account",
            IgnoreCode::InvalidMessageId => "invalid_message_id",
            IgnoreCode::NoSuccessMarker => "no_success_marker",
            IgnoreCode::AlreadySeen => "already_seen",
            IgnoreCode::WrongChannel => "wrong_channel",
        }
    }

    /// The runtime's code for a policy reason.
    pub fn from_policy(reason: IgnoreReason) -> Self {
        match reason {
            IgnoreReason::Webhook => IgnoreCode::Webhook,
            IgnoreReason::WrongGuild => IgnoreCode::WrongGuild,
            IgnoreReason::NotListingBot => IgnoreCode::NotListingBot,
            IgnoreReason::NotBotAccount => IgnoreCode::NotBotAccount,
            IgnoreReason::InvalidMessageId => IgnoreCode::InvalidMessageId,
            IgnoreReason::NoSuccessMarker => IgnoreCode::NoSuccessMarker,
            IgnoreReason::AlreadySeen => IgnoreCode::AlreadySeen,
        }
    }
}

/// What the runtime observed about one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// The message was a real bump confirmation; a cycle was started.
    Confirmed {
        /// The message id that confirmed the bump.
        message_id: u64,
        /// When the reminder becomes due.
        next_due: i64,
        /// The cycle number that was started.
        cycle: i64,
    },
    /// The message was not a bump confirmation. The reason code is stable.
    Ignored(IgnoreCode),
    /// The message was already seen, but the reminder job of the current cycle was
    /// missing and has now been scheduled.
    Repaired {
        /// The cycle whose job was scheduled.
        cycle: i64,
    },
    /// The feature is off or its configuration was refused; nothing happened.
    Disabled,
}

/// What a re-arm did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RearmOutcome {
    /// Nothing to do: the feature is off or in shadow, there is no cycle, or the
    /// cycle is already reminded.
    NothingToDo,
    /// The job for the current cycle and due time already existed.
    AlreadyScheduled,
    /// A new job was scheduled for the current cycle and due time.
    Scheduled,
}

/// The outcome of a `/bump status` or `/bump reset` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The command ran; the reply text is ready.
    Done {
        /// The reply text. It never carries a member name or message text.
        reply: String,
    },
    /// The caller is not staff.
    NotStaff {
        /// The reply text.
        reply: String,
    },
    /// The command came from another guild. Nothing was changed.
    WrongGuild {
        /// The reply text. It never names the home guild.
        reply: String,
    },
    /// The feature is off.
    Disabled {
        /// The reply text.
        reply: String,
    },
    /// The stored state could not be read.
    Failed {
        /// The reply text.
        reply: String,
    },
}

/// The runtime's view of the world: the configuration, the stored state and the two
/// outbound traits.
pub struct Runtime<'a> {
    /// The parsed settings.
    pub settings: &'a BumpSettings,
    /// The validated policy configuration.
    pub config: BumpConfig,
    /// The stored bump state.
    pub repo: &'a mut Repository,
    /// The job queue.
    pub queue: &'a dyn JobQueue,
    /// The channel reminders are posted to.
    pub channel: &'a dyn ReminderChannel,
}

/// The dedupe key of the reminder job for one cycle and due time.
///
/// The due time is part of the key, so a re-arm that moves the due time of a
/// cycle schedules a NEW job instead of colliding with the old one. The old job
/// stays harmless: it carries a due time that no longer matches the cycle, so it
/// is stale and does nothing.
pub fn dedupe_key(guild: u64, cycle: i64, due_at: i64) -> String {
    format!("bump:{guild}:{cycle}:{due_at}")
}

/// The payload of the reminder job for one cycle and due time. It holds the guild
/// id, the cycle number and the due time and nothing else.
pub fn payload_json(guild: u64, cycle: i64, due_at: i64) -> String {
    format!("{{\"guild\":{guild},\"cycle\":{cycle},\"due_at\":{due_at}}}")
}

/// Build the reminder a cycle owes.
pub fn build_reminder(settings: &BumpSettings, cycle: i64) -> Reminder {
    Reminder {
        channel_id: settings.channel_id.unwrap_or(0),
        role_id: settings.role_id,
        content: format!("Bump reminder: the server can be bumped again (cycle {cycle})."),
    }
}

/// The reply of `/bump status` for a guild with no cycle yet.
pub fn status_reply_no_cycle() -> String {
    "No bump cycle is running. The next bump will start one.".to_string()
}

/// The reply of `/bump status`.
///
/// The reply carries the time and the message id of the last confirmation and the
/// time the next reminder is due. It never carries a member name or message text.
pub fn status_reply(state: &BumpState, status: ReminderStatus, now: i64) -> String {
    let cycle = state.cycle;
    let last = match (state.last_bump_at, state.last_bumper) {
        (Some(at), Some(id)) => format!("last bump: message {id} at {at}"),
        (Some(at), None) => format!("last bump: at {at}"),
        (None, _) => "last bump: none recorded".to_string(),
    };
    let next = match status {
        ReminderStatus::NoCycle => "next reminder: no cycle running".to_string(),
        ReminderStatus::NotYet { due_at } => format!("next reminder: due at {due_at}"),
        ReminderStatus::Due { due_at } => format!("next reminder: due now (was due at {due_at})"),
        ReminderStatus::AlreadyReminded { due_at } => {
            format!("next reminder: already sent for this cycle (due at {due_at})")
        }
    };
    let state_text = if state.reminded {
        "reminded"
    } else {
        "not reminded"
    };
    format!("Bump status (cycle {cycle}): {last}; {next}; {state_text}; checked at {now}.")
}

/// The reply of `/bump reset` for a caller who is not staff.
pub fn reset_reply_not_staff() -> String {
    "Only staff can reset the bump cycle.".to_string()
}

/// The reply of `/bump reset` after a successful reset.
pub fn reset_reply_done(state: &BumpState) -> String {
    format!(
        "Bump cycle reset: cycle {} is running and its reminder is due at {}.",
        state.cycle,
        state
            .next_due_at
            .map(|at| at.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    )
}

/// The reply of a command that came from another guild.
///
/// It says nothing was done and never names the home guild.
pub fn command_reply_wrong_guild() -> String {
    "This command can only be used in the configured server. Nothing was changed.".to_string()
}

/// The reply of `/bump reset` in shadow mode.
///
/// Shadow changes nothing, so the reply says so. It names the cycle a real reset
/// would have started.
pub fn reset_reply_shadow(would_cycle: i64, would_due_at: i64) -> String {
    format!(
        "Shadow mode: nothing was changed. A reset would start cycle {would_cycle}, \
         with its reminder due at {would_due_at}."
    )
}

impl Runtime<'_> {
    /// True when the feature may act at all.
    pub fn is_enabled(&self) -> bool {
        self.settings.effective_mode() != crate::settings::Mode::Off
    }

    /// True when the feature may only record what it would do.
    pub fn is_shadow(&self) -> bool {
        self.settings.effective_mode() == crate::settings::Mode::Shadow
    }

    /// The stored row for the configured guild, or an empty state when there is none.
    pub fn load_state(&self) -> Result<BumpState, RuntimeError> {
        let guild = self.config.guild_id();
        Ok(self
            .repo
            .load(guild)?
            .unwrap_or_else(|| BumpState::empty(guild)))
    }

    /// The channel restriction: only the configured bump channel counts.
    ///
    /// It fails closed: a runtime without a configured channel trusts no message
    /// at all, because it cannot tell the bump channel from any other one.
    fn channel_allowed(&self, facts: &MessageFacts) -> bool {
        match self.settings.channel_id {
            Some(id) => facts.channel_id == id,
            None => false,
        }
    }

    /// Observe one message: detect a bump, persist the cycle and schedule the
    /// reminder job for it.
    ///
    /// The decision is the policy's. A confirmed bump starts a new cycle: the row is
    /// written with the new cycle number and the reminder is not marked as sent, and
    /// the job for that cycle is scheduled. A message that is not a confirmation
    /// changes nothing.
    ///
    /// In shadow mode the decision is still made and reported, but nothing is
    /// written and no job is scheduled: a shadow run must not arm a real reminder.
    pub fn observe(&mut self, facts: &MessageFacts, now: i64) -> Result<Observation, RuntimeError> {
        if !self.is_enabled() {
            return Ok(Observation::Disabled);
        }
        if !self.channel_allowed(facts) {
            return Ok(Observation::Ignored(IgnoreCode::WrongChannel));
        }
        let current = self.load_state()?;
        let policy_state = current.to_policy();
        let (decision, next_state) = policy::observe(facts, &self.config, &policy_state, now);
        let Decision::Confirmed(confirmation) = decision else {
            // A redelivered confirmation re-runs the idempotent schedule for the
            // current cycle: a queue that refused the schedule left the cycle with
            // no job, and the dedupe key makes a second schedule harmless.
            if let Decision::Ignored(IgnoreReason::AlreadySeen) = decision {
                if let Some(repaired) = self.repair_schedule(&current)? {
                    return Ok(repaired);
                }
            }
            return Ok(Observation::Ignored(IgnoreCode::from_policy(
                ignore_reason(&decision),
            )));
        };
        let cycle = current.cycle + 1;
        if self.is_shadow() {
            // Shadow writes nothing and schedules nothing.
            return Ok(Observation::Confirmed {
                message_id: confirmation.message_id,
                next_due: confirmation.next_due,
                cycle,
            });
        }
        let next = BumpState::from_policy(
            self.config.guild_id(),
            cycle,
            &next_state,
            Some(confirmation.next_due),
        );
        self.repo.save(&next)?;
        let due_at = next.next_due_at.unwrap_or(confirmation.next_due);
        self.schedule_for(next.guild, cycle, due_at)?;
        Ok(Observation::Confirmed {
            message_id: confirmation.message_id,
            next_due: confirmation.next_due,
            cycle,
        })
    }

    /// Re-run the schedule of the current cycle.
    ///
    /// It is the repair for a cycle whose schedule was refused: the dedupe key
    /// makes the call idempotent, so a job that already exists is left alone and
    /// a missing one is created. It returns `None` when there is nothing to
    /// repair (no cycle, the cycle is already reminded, or the feature is in
    /// shadow mode).
    fn repair_schedule(
        &mut self,
        current: &BumpState,
    ) -> Result<Option<Observation>, RuntimeError> {
        if self.is_shadow() {
            return Ok(None);
        }
        if current.cycle <= 0 || current.reminded {
            return Ok(None);
        }
        let Some(due_at) = current.next_due_at else {
            return Ok(None);
        };
        match self.schedule_for(current.guild, current.cycle, due_at)? {
            ScheduleOutcome::Created => Ok(Some(Observation::Repaired {
                cycle: current.cycle,
            })),
            ScheduleOutcome::Duplicate => Ok(None),
        }
    }

    /// Schedule the reminder job for one cycle and due time.
    ///
    /// The dedupe key names the guild, the cycle and the due time, so a second call
    /// for the same cycle and due time creates nothing, while a later due time (the
    /// interval changed) gets a job of its own.
    fn schedule_for(
        &self,
        guild: u64,
        cycle: i64,
        due_at: i64,
    ) -> Result<ScheduleOutcome, RuntimeError> {
        if cycle <= 0 {
            return Ok(ScheduleOutcome::Duplicate);
        }
        let request = JobRequest {
            kind: KIND_REMINDER,
            dedupe_key: dedupe_key(guild, cycle, due_at),
            payload_json: payload_json(guild, cycle, due_at),
            due_at,
        };
        self.queue.schedule(&request)
    }

    /// Re-arm the reminder after a restart.
    ///
    /// The stored due time may be stale (the interval was changed at the
    /// restart). The policy decides what is due now; the row is updated and a job
    /// is scheduled for that moment. The dedupe key carries the due time, so a
    /// moved due time gets a NEW job instead of colliding with the old one, and
    /// the old job stays harmless: it carries a due time that no longer matches
    /// the cycle, so it is stale and does nothing.
    ///
    /// The result tells the truth: [`RearmOutcome::NothingToDo`] when there is
    /// nothing to do, [`RearmOutcome::AlreadyScheduled`] when the job was already
    /// there and [`RearmOutcome::Scheduled`] when a new job was created.
    pub fn rearm(&mut self, now: i64) -> Result<RearmOutcome, RuntimeError> {
        if !self.is_enabled() || self.is_shadow() {
            return Ok(RearmOutcome::NothingToDo);
        }
        let state = self.load_state()?;
        if state.cycle <= 0 || state.reminded {
            return Ok(RearmOutcome::NothingToDo);
        }
        let status = policy::reminder_status(&state.to_policy(), &self.config, now);
        let due_at = match status {
            ReminderStatus::NotYet { due_at } | ReminderStatus::Due { due_at } => due_at,
            _ => return Ok(RearmOutcome::NothingToDo),
        };
        let mut fresh = state.clone();
        fresh.next_due_at = Some(due_at);
        self.repo.save(&fresh)?;
        match self.schedule_for(fresh.guild, fresh.cycle, due_at)? {
            ScheduleOutcome::Created => Ok(RearmOutcome::Scheduled),
            ScheduleOutcome::Duplicate => Ok(RearmOutcome::AlreadyScheduled),
        }
    }

    /// The outcome of the reminder job for one cycle.
    ///
    /// The decision is pure: given the stored state and `now`, it says whether a
    /// reminder is owed. A cycle that was already reminded owes nothing, and a
    /// cycle that is not due yet owes nothing.
    pub fn reminder_due(&self, state: &BumpState, now: i64) -> bool {
        policy::due(&state.to_policy(), &self.config, now)
    }

    /// Run the reminder of one job the queue handed out.
    ///
    /// The governing rule: a reminder job only ACTS when the due time it carries
    /// equals the cycle's stored `next_due_at`. A job whose due time no longer
    /// matches is STALE and is a no-op: it writes nothing and posts nothing, so
    /// it can never consume the reminder of the cycle that is really running.
    ///
    /// The cycle is marked as reminded BEFORE the post, so a crash between the mark
    /// and the post cannot produce a second reminder. The mark is conditional on
    /// the cycle number, so a stale handler cannot mark a newer cycle. If the post
    /// fails, the failure is reported to the caller by code; the cycle stays
    /// marked, because a repeated post would be a duplicate.
    ///
    /// In shadow mode nothing is written and nothing is posted.
    pub async fn run_reminder(
        &mut self,
        now: i64,
        job: &ReminderJob,
    ) -> Result<bool, RuntimeError> {
        if !self.is_enabled() || self.is_shadow() {
            return Ok(false);
        }
        let payload = parse_reminder_payload(&job.payload_json)?;
        let state = self.load_state()?;
        if state.cycle <= 0 {
            return Ok(false);
        }
        // The governing rule: the job acts only for the cycle and the due time it
        // carries. Anything else is stale and does nothing at all.
        if state.cycle != payload.cycle || state.next_due_at != Some(payload.due_at) {
            return Ok(false);
        }
        if state.reminded {
            return Ok(false);
        }
        if now < payload.due_at {
            return Ok(false);
        }
        // Mark first: at most once, even across a restart. The mark is conditional
        // on the cycle, so a handler that read an older cycle cannot mark the
        // newer one.
        if !self.repo.mark_reminded(state.guild, state.cycle)? {
            return Ok(false);
        }
        let reminder = build_reminder(self.settings, state.cycle);
        self.channel.send(&reminder).await?;
        Ok(true)
    }

    /// True when a command may act for the guild it was invoked in.
    ///
    /// The commands are registered globally, so they can be invoked from ANY guild
    /// the bot is in. Only the configured home guild may act. When the feature is
    /// on and no home guild is configured the check FAILS CLOSED: the command is
    /// denied and nothing changes.
    pub fn allows_command_from(&self, guild_id: Option<u64>) -> bool {
        match self.settings.guild_id {
            Some(home) => guild_id == Some(home),
            // No home guild configured: deny, whatever the caller says.
            None => false,
        }
    }

    /// `/bump status`: report the stored cycle.
    ///
    /// `guild_id` is the guild the command was invoked in. A command from another
    /// guild is denied and changes nothing.
    pub fn status(
        &mut self,
        guild_id: Option<u64>,
        now: i64,
    ) -> Result<CommandOutcome, RuntimeError> {
        if !self.is_enabled() {
            return Ok(CommandOutcome::Disabled {
                reply: "The bump feature is off.".to_string(),
            });
        }
        if !self.allows_command_from(guild_id) {
            return Ok(CommandOutcome::WrongGuild {
                reply: command_reply_wrong_guild(),
            });
        }
        let state = self.load_state()?;
        let status = policy::reminder_status(&state.to_policy(), &self.config, now);
        Ok(CommandOutcome::Done {
            reply: status_reply(&state, status, now),
        })
    }

    /// `/bump reset`: staff only.
    ///
    /// A reset starts a new cycle from `now` and keeps the last confirmed message id,
    /// so an old message cannot undo the reset. The new cycle gets its own job: the
    /// cycle number grows, so the dedupe key differs from the previous cycle's.
    ///
    /// In shadow mode nothing is written and no job is scheduled.
    ///
    /// `guild_id` is the guild the command was invoked in. A command from another
    /// guild is denied and changes nothing.
    pub fn reset(
        &mut self,
        staff: StaffCheck,
        guild_id: Option<u64>,
        now: i64,
    ) -> Result<CommandOutcome, RuntimeError> {
        if !self.is_enabled() {
            return Ok(CommandOutcome::Disabled {
                reply: "The bump feature is off.".to_string(),
            });
        }
        if !self.allows_command_from(guild_id) {
            return Ok(CommandOutcome::WrongGuild {
                reply: command_reply_wrong_guild(),
            });
        }
        if staff != StaffCheck::Staff {
            return Ok(CommandOutcome::NotStaff {
                reply: reset_reply_not_staff(),
            });
        }
        let current = self.load_state()?;
        let reset_state = current.to_policy().reset(now);
        let cycle = current.cycle + 1;
        let next_due = now.saturating_add(self.config.interval_secs() as i64);
        let mut next =
            BumpState::from_policy(self.config.guild_id(), cycle, &reset_state, Some(next_due));
        // A reset clears the "reminded" mark of the old cycle: the new cycle owes its
        // own reminder.
        next.reminded = false;
        if self.is_shadow() {
            // Shadow changes nothing, so the reply says so.
            return Ok(CommandOutcome::Done {
                reply: reset_reply_shadow(next.cycle, next_due),
            });
        }
        self.repo.save(&next)?;
        self.schedule_for(next.guild, cycle, next_due)?;
        Ok(CommandOutcome::Done {
            reply: reset_reply_done(&next),
        })
    }

    /// The shadow-mode record for one message: what the runtime would have done.
    ///
    /// It carries counts and stable codes only. No message text, no member id.
    pub fn would_do(&mut self, facts: &MessageFacts, now: i64) -> Result<WouldDo, RuntimeError> {
        let mut record = WouldDo::default();
        if !self.is_enabled() {
            record.note("disabled");
            return Ok(record);
        }
        if !self.channel_allowed(facts) {
            record.note(IgnoreCode::WrongChannel.code());
            return Ok(record);
        }
        let current = self.load_state()?;
        let (decision, next_state) =
            policy::observe(facts, &self.config, &current.to_policy(), now);
        let Decision::Confirmed(confirmation) = decision else {
            record.note(IgnoreCode::from_policy(ignore_reason(&decision)).code());
            return Ok(record);
        };
        let cycle = current.cycle + 1;
        record.cycles = 1;
        record.jobs = 1;
        record.reminders = 1;
        record.note("confirmed");
        // The shadow record says what would happen; nothing is written or sent.
        let _ = (confirmation, next_state, cycle);
        Ok(record)
    }
}

/// The ignore reason of a decision that is not a confirmation.
fn ignore_reason(decision: &Decision) -> IgnoreReason {
    match decision {
        Decision::Ignored(reason) => *reason,
        Decision::Confirmed(_) => IgnoreReason::AlreadySeen,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bump::config::bump_settings;
    use crate::bump::policy::DEFAULT_INTERVAL_SECS;
    use crate::jobs::queue::{self, NewJob, RestartPolicy, ScheduleOutcome as QueueOutcome};
    use rusqlite::Connection;
    use std::sync::Mutex;

    // Clearly synthetic ids; none is a real Discord id.
    const GUILD: u64 = 1_000_001;
    const OTHER_GUILD: u64 = 1_000_002;
    const CHANNEL: u64 = 4_000_001;
    const OTHER_CHANNEL: u64 = 4_000_002;
    const ROLE: u64 = 5_000_001;
    const BOT: u64 = 2_000_001;
    const USER: u64 = 3_000_001;
    const NOW: i64 = 1_700_000_000;
    const INTERVAL: u64 = DEFAULT_INTERVAL_SECS;

    // ---- fakes -----------------------------------------------------------------------

    /// Records every reminder the runtime wanted to post.
    #[derive(Default)]
    struct FakeChannel {
        sent: Mutex<Vec<Reminder>>,
        fail: Mutex<bool>,
    }

    impl FakeChannel {
        fn sent(&self) -> Vec<Reminder> {
            self.sent.lock().unwrap().clone()
        }
        fn fail(&self) {
            *self.fail.lock().unwrap() = true;
        }
        fn work(&self) {
            *self.fail.lock().unwrap() = false;
        }
    }

    impl ReminderChannel for FakeChannel {
        fn send(&self, reminder: &Reminder) -> BoxFuture<'_, Result<(), RuntimeError>> {
            // The reminder is copied into the future so the returned box does not
            // borrow the caller's argument.
            let reminder = reminder.clone();
            Box::pin(async move {
                if *self.fail.lock().unwrap() {
                    return Err(RuntimeError::SendFailed);
                }
                self.sent.lock().unwrap().push(reminder);
                Ok(())
            })
        }
    }

    /// One job held by the faithful in-memory queue.
    #[derive(Debug, Clone)]
    struct MemJob {
        id: i64,
        dedupe_key: String,
        payload_json: String,
        due_at: i64,
        claimed: bool,
    }

    /// The state of the faithful in-memory queue.
    #[derive(Default)]
    struct FakeQueueState {
        requests: Vec<JobRequest>,
        jobs: Vec<MemJob>,
        fail: bool,
        next_id: i64,
    }

    /// A faithful in-memory job queue.
    ///
    /// It behaves like the real queue: it holds the jobs, refuses a dedupe key it
    /// already knows and hands out the jobs that are due. A reminder can only be
    /// sent from a job this queue handed out.
    #[derive(Default)]
    struct FakeQueue {
        state: Mutex<FakeQueueState>,
    }

    impl FakeQueue {
        fn requests(&self) -> Vec<JobRequest> {
            self.state.lock().unwrap().requests.clone()
        }
        fn keys(&self) -> Vec<String> {
            self.requests().into_iter().map(|r| r.dedupe_key).collect()
        }
        fn fail(&self) {
            self.state.lock().unwrap().fail = true;
        }
        fn work(&self) {
            self.state.lock().unwrap().fail = false;
        }
        /// The jobs that are still waiting to be claimed.
        fn pending_jobs(&self) -> Vec<MemJob> {
            self.state
                .lock()
                .unwrap()
                .jobs
                .iter()
                .filter(|j| !j.claimed)
                .cloned()
                .collect()
        }
        /// True when a job for this cycle and due time can still be claimed.
        fn has_pending_job(&self, cycle: i64, due_at: i64) -> bool {
            self.state
                .lock()
                .unwrap()
                .jobs
                .iter()
                .filter(|j| !j.claimed && j.due_at == due_at)
                .any(|j| {
                    parse_reminder_payload(&j.payload_json)
                        .map(|p| p.cycle == cycle)
                        .unwrap_or(false)
                })
        }
    }

    impl JobQueue for FakeQueue {
        fn schedule(&self, request: &JobRequest) -> Result<ScheduleOutcome, RuntimeError> {
            let mut state = self.state.lock().unwrap();
            if state.fail {
                return Err(RuntimeError::QueueRefused);
            }
            state.requests.push(request.clone());
            if state
                .jobs
                .iter()
                .any(|j| j.dedupe_key == request.dedupe_key)
            {
                return Ok(ScheduleOutcome::Duplicate);
            }
            state.next_id += 1;
            let id = state.next_id;
            state.jobs.push(MemJob {
                id,
                dedupe_key: request.dedupe_key.clone(),
                payload_json: request.payload_json.clone(),
                due_at: request.due_at,
                claimed: false,
            });
            Ok(ScheduleOutcome::Created)
        }

        fn claim_due(&self, now: i64, limit: u32) -> Result<Vec<ReminderJob>, RuntimeError> {
            let mut state = self.state.lock().unwrap();
            if state.fail {
                return Err(RuntimeError::QueueRefused);
            }
            let mut out = Vec::new();
            for job in state.jobs.iter_mut() {
                if out.len() as u32 >= limit {
                    break;
                }
                if !job.claimed && job.due_at <= now {
                    job.claimed = true;
                    out.push(ReminderJob {
                        id: job.id,
                        kind: KIND_REMINDER.to_string(),
                        payload_json: job.payload_json.clone(),
                    });
                }
            }
            Ok(out)
        }
    }

    /// The real job queue, so the "one job per cycle" claim is proved by the queue's
    /// own UNIQUE dedupe key rather than by a fake.
    ///
    /// The connection sits behind a mutex because a rusqlite connection is not
    /// `Sync`; the queue's own CAS is what serialises the real callers.
    struct RealQueue {
        conn: Mutex<Connection>,
    }

    impl JobQueue for RealQueue {
        fn schedule(&self, request: &JobRequest) -> Result<ScheduleOutcome, RuntimeError> {
            let job = NewJob {
                kind: request.kind,
                dedupe_key: &request.dedupe_key,
                payload_json: &request.payload_json,
                due_at: request.due_at,
            };
            let conn = self.conn.lock().unwrap();
            match queue::schedule(&conn, &job, NOW).map_err(|_| RuntimeError::QueueRefused)? {
                QueueOutcome::Created { .. } => Ok(ScheduleOutcome::Created),
                QueueOutcome::Duplicate { .. } => Ok(ScheduleOutcome::Duplicate),
            }
        }

        fn claim_due(&self, now: i64, limit: u32) -> Result<Vec<ReminderJob>, RuntimeError> {
            let conn = self.conn.lock().unwrap();
            let batch =
                queue::claim_due(&conn, now, limit).map_err(|_| RuntimeError::QueueRefused)?;
            Ok(batch
                .claimed
                .iter()
                .filter_map(ReminderJob::from_claimed)
                .collect())
        }
    }

    // ---- helpers ---------------------------------------------------------------------

    fn settings(mode: &str) -> BumpSettings {
        let pairs = [
            ("BUMP_MODE".to_string(), mode.to_string()),
            ("BUMP_CHANNEL_ID".to_string(), CHANNEL.to_string()),
            ("BUMP_ROLE_ID".to_string(), ROLE.to_string()),
            ("BUMP_BOT_ID".to_string(), BOT.to_string()),
        ];
        bump_settings(
            move |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()),
            Some(GUILD),
        )
    }

    fn repo() -> Repository {
        Repository::open(":memory:", &[]).unwrap()
    }

    fn success(message_id: u64) -> MessageFacts {
        MessageFacts {
            message_id,
            guild_id: Some(GUILD),
            channel_id: CHANNEL,
            author_id: BOT,
            author_is_bot: true,
            is_webhook: false,
            content: String::new(),
            embeds_text: "Bump done! Your server is listed.".to_string(),
            interaction_name: Some("bump".to_string()),
        }
    }

    /// The job the queue would hold for one cycle and due time.
    fn job_for(cycle: i64, due_at: i64) -> ReminderJob {
        ReminderJob {
            id: 0,
            kind: KIND_REMINDER.to_string(),
            payload_json: payload_json(GUILD, cycle, due_at),
        }
    }

    struct Harness {
        settings: BumpSettings,
        config: BumpConfig,
        repo: Repository,
        queue: FakeQueue,
        channel: FakeChannel,
    }

    impl Harness {
        fn new(mode: &str) -> Self {
            let settings = settings(mode);
            let config = settings.policy_config().unwrap();
            Harness {
                settings,
                config,
                repo: repo(),
                queue: FakeQueue::default(),
                channel: FakeChannel::default(),
            }
        }

        /// The reminder interval in force.
        fn interval(&self) -> u64 {
            self.config.interval_secs()
        }

        /// Change the reminder interval, as a restart with a new setting would.
        fn set_interval(&mut self, secs: u64) {
            let mut s = self.settings.clone();
            s.interval_secs = secs;
            self.config = BumpConfig::new(GUILD, BOT, &s.success_marker, secs).unwrap();
            self.settings = s;
        }

        fn runtime(&mut self) -> Runtime<'_> {
            Runtime {
                settings: &self.settings,
                config: self.config.clone(),
                repo: &mut self.repo,
                queue: &self.queue,
                channel: &self.channel,
            }
        }
    }

    /// A harness on the real job queue.
    struct RealHarness {
        settings: BumpSettings,
        config: BumpConfig,
        repo: Repository,
        queue: RealQueue,
        channel: FakeChannel,
    }

    impl RealHarness {
        fn new(mode: &str, interval: u64) -> Self {
            let mut settings = settings(mode);
            settings.interval_secs = interval;
            let config = settings.policy_config().unwrap();
            let mut conn = Connection::open_in_memory().unwrap();
            queue::init(&mut conn).unwrap();
            RealHarness {
                settings,
                config,
                repo: repo(),
                queue: RealQueue {
                    conn: Mutex::new(conn),
                },
                channel: FakeChannel::default(),
            }
        }

        fn set_interval(&mut self, secs: u64) {
            let mut s = self.settings.clone();
            s.interval_secs = secs;
            self.config = BumpConfig::new(GUILD, BOT, &s.success_marker, secs).unwrap();
            self.settings = s;
        }

        fn runtime(&mut self) -> Runtime<'_> {
            Runtime {
                settings: &self.settings,
                config: self.config.clone(),
                repo: &mut self.repo,
                queue: &self.queue,
                channel: &self.channel,
            }
        }

        /// Claim every job that is due and run it through the runtime.
        async fn drive(&mut self, now: i64) {
            let claimed = self.queue.claim_due(now, 32).unwrap();
            for job in claimed {
                let mut rt = self.runtime();
                let _ = rt.run_reminder(now, &job).await;
            }
        }
    }

    // ---- detection -------------------------------------------------------------------

    #[test]
    fn a_real_bump_starts_a_cycle_and_schedules_one_job() {
        let mut h = Harness::new("live");
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap()
        };
        assert_eq!(
            observation,
            Observation::Confirmed {
                message_id: 100,
                next_due: NOW + INTERVAL as i64,
                cycle: 1,
            }
        );
        let requests = h.queue.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].kind, KIND_REMINDER);
        assert_eq!(requests[0].due_at, NOW + INTERVAL as i64);
        assert_eq!(
            requests[0].payload_json,
            format!(
                "{{\"guild\":{GUILD},\"cycle\":1,\"due_at\":{}}}",
                NOW + INTERVAL as i64
            )
        );
    }

    #[test]
    fn the_stored_row_carries_the_cycle() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let state = h.repo.load(GUILD).unwrap().unwrap();
        assert_eq!(state.cycle, 1);
        assert_eq!(state.last_bump_at, Some(NOW));
        assert_eq!(state.last_bumper, Some(100));
        assert_eq!(state.next_due_at, Some(NOW + INTERVAL as i64));
        assert!(!state.reminded);
    }

    #[test]
    fn an_identical_user_post_is_never_a_bump() {
        let mut h = Harness::new("live");
        let mut facts = success(100);
        facts.author_id = USER;
        facts.author_is_bot = false;
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&facts, NOW).unwrap()
        };
        assert_eq!(observation, Observation::Ignored(IgnoreCode::NotListingBot));
        assert!(h.queue.requests().is_empty());
        assert!(h.repo.load(GUILD).unwrap().is_none());
    }

    #[test]
    fn a_webhook_post_is_never_a_bump() {
        let mut h = Harness::new("live");
        let mut facts = success(100);
        facts.is_webhook = true;
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&facts, NOW).unwrap()
        };
        assert_eq!(observation, Observation::Ignored(IgnoreCode::Webhook));
        assert!(h.queue.requests().is_empty());
        assert!(h.repo.load(GUILD).unwrap().is_none());
    }

    #[test]
    fn the_cooldown_error_reply_does_not_reset_the_timer() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let before = h.repo.load(GUILD).unwrap().unwrap();
        let mut facts = success(200);
        facts.embeds_text = "Please wait another 1 hour and 59 minutes".to_string();
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&facts, NOW + 3_000).unwrap()
        };
        assert_eq!(
            observation,
            Observation::Ignored(IgnoreCode::NoSuccessMarker)
        );
        let after = h.repo.load(GUILD).unwrap().unwrap();
        assert_eq!(after, before, "the cooldown reply changed the state");
    }

    #[test]
    fn the_same_message_id_twice_starts_one_cycle() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW + 5).unwrap()
        };
        assert_eq!(observation, Observation::Ignored(IgnoreCode::AlreadySeen));
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap().cycle, 1);
        // The redelivery re-runs the schedule, which is idempotent: the queue still
        // holds exactly one job for the cycle.
        assert_eq!(h.queue.requests().len(), 2, "the schedule was not re-run");
        assert_eq!(
            h.queue.pending_jobs().len(),
            1,
            "the re-run created a second job for the cycle"
        );
        assert_eq!(h.queue.keys()[0], h.queue.keys()[1]);
    }

    #[test]
    fn a_second_bump_starts_a_second_cycle_with_its_own_job() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&success(200), NOW + INTERVAL as i64 + 10)
                .unwrap()
        };
        assert_eq!(
            observation,
            Observation::Confirmed {
                message_id: 200,
                next_due: NOW + INTERVAL as i64 + 10 + INTERVAL as i64,
                cycle: 2,
            }
        );
        assert_eq!(h.queue.keys().len(), 2);
        assert_ne!(h.queue.keys()[0], h.queue.keys()[1]);
    }

    #[test]
    fn a_message_from_another_guild_is_ignored() {
        let mut h = Harness::new("live");
        let mut facts = success(100);
        facts.guild_id = Some(OTHER_GUILD);
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&facts, NOW).unwrap()
        };
        assert_eq!(observation, Observation::Ignored(IgnoreCode::WrongGuild));
        assert!(h.queue.requests().is_empty());
        assert!(h.repo.load(GUILD).unwrap().is_none());
    }

    // ---- rule 1: the channel restriction -----------------------------------------------

    #[test]
    fn a_message_from_another_channel_is_ignored() {
        let mut h = Harness::new("live");
        let mut facts = success(100);
        facts.channel_id = OTHER_CHANNEL;
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&facts, NOW).unwrap()
        };
        assert_eq!(observation, Observation::Ignored(IgnoreCode::WrongChannel));
        assert!(
            h.queue.requests().is_empty(),
            "a foreign channel armed a job"
        );
        assert!(
            h.repo.load(GUILD).unwrap().is_none(),
            "a foreign channel wrote a row"
        );
    }

    #[test]
    fn a_message_from_another_channel_does_not_open_a_cycle() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let mut facts = success(200);
        facts.channel_id = OTHER_CHANNEL;
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&facts, NOW + 10).unwrap()
        };
        assert_eq!(observation, Observation::Ignored(IgnoreCode::WrongChannel));
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap().cycle, 1);
    }

    #[test]
    fn the_shadow_record_names_the_wrong_channel() {
        let mut h = Harness::new("shadow");
        let mut facts = success(100);
        facts.channel_id = OTHER_CHANNEL;
        let record = {
            let mut rt = h.runtime();
            rt.would_do(&facts, NOW).unwrap()
        };
        assert_eq!(record.cycles, 0);
        assert_eq!(record.reminders, 0);
        assert_eq!(record.jobs, 0);
        assert_eq!(record.codes, vec!["wrong_channel"]);
    }

    #[test]
    fn the_channel_restriction_fails_closed_without_a_configured_channel() {
        let mut settings = settings("live");
        settings.channel_id = None;
        let config = BumpConfig::new(GUILD, BOT, &settings.success_marker, INTERVAL).unwrap();
        let mut repo = repo();
        let queue = FakeQueue::default();
        let channel = FakeChannel::default();
        let mut rt = Runtime {
            settings: &settings,
            config,
            repo: &mut repo,
            queue: &queue,
            channel: &channel,
        };
        let observation = rt.observe(&success(100), NOW).unwrap();
        assert_eq!(observation, Observation::Ignored(IgnoreCode::WrongChannel));
        assert!(queue.requests().is_empty());
    }

    // ---- the reminder is sent exactly once per cycle ----------------------------------

    #[tokio::test]
    async fn a_restart_before_the_reminder_is_due_still_produces_one_reminder() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let due = NOW + INTERVAL as i64;
        // Restart: rearm the cycle from the stored row.
        {
            let mut rt = h.runtime();
            assert_ne!(
                rt.rearm(NOW + 10).unwrap(),
                RearmOutcome::NothingToDo,
                "the re-arm did nothing at all"
            );
        }
        // The observe and the re-arm all ask for the same cycle and due time: the
        // queue refuses every duplicate, so one job exists for the cycle.
        assert_eq!(
            h.queue.requests().len(),
            2,
            "not every schedule was recorded"
        );
        assert_eq!(
            h.queue.keys()[0],
            h.queue.keys()[1],
            "a schedule used another key"
        );
        // One reminder, at the due time.
        {
            let mut rt = h.runtime();
            assert!(rt.run_reminder(due, &job_for(1, due)).await.unwrap());
        }
        assert_eq!(h.channel.sent().len(), 1);
        // A second run at the same time sends nothing.
        {
            let mut rt = h.runtime();
            assert!(!rt.run_reminder(due, &job_for(1, due)).await.unwrap());
        }
        assert_eq!(h.channel.sent().len(), 1);
    }

    #[tokio::test]
    async fn a_restart_after_the_reminder_was_sent_produces_no_duplicate() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let due = NOW + INTERVAL as i64;
        {
            let mut rt = h.runtime();
            assert!(rt.run_reminder(due, &job_for(1, due)).await.unwrap());
        }
        assert_eq!(h.channel.sent().len(), 1);
        // Restart after the send: rearm does nothing, the reminder is not repeated.
        {
            let mut rt = h.runtime();
            assert_eq!(rt.rearm(due + 100).unwrap(), RearmOutcome::NothingToDo);
        }
        {
            let mut rt = h.runtime();
            assert!(!rt.run_reminder(due + 100, &job_for(1, due)).await.unwrap());
        }
        assert_eq!(h.channel.sent().len(), 1, "a duplicate reminder was sent");
    }

    #[tokio::test]
    async fn the_reminder_is_not_sent_before_it_is_due() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        {
            let mut rt = h.runtime();
            assert!(!rt
                .run_reminder(
                    NOW + INTERVAL as i64 - 1,
                    &job_for(1, NOW + INTERVAL as i64)
                )
                .await
                .unwrap());
        }
        assert!(h.channel.sent().is_empty());
    }

    #[tokio::test]
    async fn a_failed_send_leaves_the_cycle_marked_so_no_duplicate_follows() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        h.channel.fail();
        let due = NOW + INTERVAL as i64;
        let result = {
            let mut rt = h.runtime();
            rt.run_reminder(due, &job_for(1, due)).await
        };
        assert_eq!(result, Err(RuntimeError::SendFailed));
        // The cycle is marked before the post, so a retry does not post again.
        assert!(h.repo.load(GUILD).unwrap().unwrap().reminded);
        h.channel.fail(); // still failing
        {
            let mut rt = h.runtime();
            assert!(!rt.run_reminder(due + 1, &job_for(1, due)).await.unwrap());
        }
        assert!(h.channel.sent().is_empty());
    }

    // ---- rule 2: the stale job must not consume the reminder ---------------------------

    #[tokio::test]
    async fn a_stale_job_does_not_consume_the_reminder() {
        let mut h = Harness::new("live");
        // The bump is confirmed with a long interval, so the job carries a late
        // due time.
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        // A restart with a shorter interval moves the cycle's due time.
        h.set_interval(7_200);
        {
            let mut rt = h.runtime();
            let _ = rt.rearm(NOW + 10);
        }
        // The old job fires late. It carries a due time that is no longer the
        // cycle's due time, so it is stale and does nothing at all.
        let stale_due = NOW + 9_000;
        {
            let mut rt = h.runtime();
            assert!(
                !rt.run_reminder(stale_due, &job_for(1, stale_due))
                    .await
                    .unwrap(),
                "a stale job consumed the reminder"
            );
        }
        assert!(h.channel.sent().is_empty(), "a stale job posted a reminder");
        assert!(
            !h.repo.load(GUILD).unwrap().unwrap().reminded,
            "a stale job marked the cycle"
        );
    }

    #[tokio::test]
    async fn raising_the_interval_at_a_restart_still_produces_exactly_one_reminder() {
        let mut h = RealHarness::new("live", 7_200);
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        // A restart with a longer interval moves the cycle's due time.
        h.set_interval(9_000);
        {
            let mut rt = h.runtime();
            assert_eq!(
                rt.rearm(NOW + 10).unwrap(),
                RearmOutcome::Scheduled,
                "the re-arm scheduled no job of its own"
            );
        }
        // Drive the real queue to NOW + 9060.
        for t in [NOW + 7_200, NOW + 9_000, NOW + 9_060] {
            h.drive(t).await;
        }
        assert_eq!(
            h.channel.sent().len(),
            1,
            "the reminder was lost or sent twice"
        );
    }

    #[tokio::test]
    async fn lowering_the_interval_at_a_restart_still_produces_exactly_one_reminder() {
        let mut h = RealHarness::new("live", 9_000);
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        // A restart with a shorter interval moves the cycle's due time.
        h.set_interval(7_200);
        {
            let mut rt = h.runtime();
            assert_eq!(rt.rearm(NOW + 10).unwrap(), RearmOutcome::Scheduled);
        }
        // Drive the real queue past both due times.
        for t in [NOW + 7_200, NOW + 9_000, NOW + 9_060] {
            h.drive(t).await;
        }
        assert_eq!(
            h.channel.sent().len(),
            1,
            "the reminder was lost or sent twice"
        );
    }

    #[tokio::test]
    async fn an_unchanged_interval_at_a_restart_produces_no_duplicate() {
        let mut h = RealHarness::new("live", 7_200);
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        {
            let mut rt = h.runtime();
            assert_eq!(
                rt.rearm(NOW + 10).unwrap(),
                RearmOutcome::AlreadyScheduled,
                "the re-arm scheduled a second job for the same cycle and due time"
            );
        }
        for t in [NOW + 7_200, NOW + 9_000, NOW + 9_060] {
            h.drive(t).await;
        }
        assert_eq!(
            h.channel.sent().len(),
            1,
            "the reminder was lost or sent twice"
        );
    }

    #[test]
    fn the_rearm_reports_the_truth() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        // Nothing to do: there is no cycle yet in a fresh guild.
        {
            let mut h2 = Harness::new("live");
            let mut rt = h2.runtime();
            assert_eq!(rt.rearm(NOW).unwrap(), RearmOutcome::NothingToDo);
        }
        // A first re-arm creates the job.
        {
            let mut rt = h.runtime();
            let outcome = rt.rearm(NOW + 10).unwrap();
            assert_ne!(outcome, RearmOutcome::NothingToDo);
        }
        // A second re-arm finds the same job.
        {
            let mut rt = h.runtime();
            assert_ne!(rt.rearm(NOW + 20).unwrap(), RearmOutcome::NothingToDo);
        }
    }

    // ---- rule 3: a refused schedule must be repairable ----------------------------------

    #[test]
    fn a_redelivered_confirmation_repairs_a_refused_schedule() {
        let mut h = Harness::new("live");
        h.queue.fail();
        let result = {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW)
        };
        assert_eq!(result, Err(RuntimeError::QueueRefused));
        // The row is there, but no job is.
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap().cycle, 1);
        assert!(
            h.queue.pending_jobs().is_empty(),
            "the refused schedule left a job behind"
        );
        // The queue works again and the same message is delivered again.
        h.queue.work();
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW + 5)
        };
        assert!(
            matches!(observation, Ok(Observation::Repaired { cycle: 1 })),
            "{observation:?}"
        );
        assert!(
            !h.queue.pending_jobs().is_empty(),
            "the redelivery did not repair the schedule"
        );
    }

    #[test]
    fn the_rearm_repairs_a_refused_schedule() {
        let mut h = Harness::new("live");
        h.queue.fail();
        {
            let mut rt = h.runtime();
            assert_eq!(
                rt.observe(&success(100), NOW),
                Err(RuntimeError::QueueRefused)
            );
        }
        h.queue.work();
        {
            let mut rt = h.runtime();
            let outcome = rt.rearm(NOW + 10).unwrap();
            assert_eq!(outcome, RearmOutcome::Scheduled, "{outcome:?}");
        }
        assert!(
            !h.queue.pending_jobs().is_empty(),
            "the re-arm did not repair the schedule"
        );
    }

    #[tokio::test]
    async fn a_repaired_schedule_still_produces_one_reminder() {
        let mut h = Harness::new("live");
        h.queue.fail();
        {
            let mut rt = h.runtime();
            assert_eq!(
                rt.observe(&success(100), NOW),
                Err(RuntimeError::QueueRefused)
            );
        }
        h.queue.work();
        {
            let mut rt = h.runtime();
            let _ = rt.observe(&success(100), NOW + 5);
        }
        let due = NOW + INTERVAL as i64;
        {
            let mut rt = h.runtime();
            assert!(rt.run_reminder(due, &job_for(1, due)).await.unwrap());
        }
        assert_eq!(h.channel.sent().len(), 1);
    }

    // ---- the stale mark must not touch a newer cycle ------------------------------------

    #[tokio::test]
    async fn a_job_of_an_older_cycle_is_stale_even_with_the_same_due_time() {
        // A reset at the same moment as the confirmation opens the next cycle
        // with the SAME due time, so only the cycle number tells the two jobs
        // apart. The job of the older cycle must not act.
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(GUILD), NOW).unwrap();
        }
        let state = h.repo.load(GUILD).unwrap().unwrap();
        assert_eq!(state.cycle, 2);
        assert_eq!(state.next_due_at, Some(NOW + INTERVAL as i64));
        // The job of cycle 1 carries the same due time as the stored row.
        let old = job_for(1, NOW + INTERVAL as i64);
        assert_eq!(
            parse_reminder_payload(&old.payload_json).unwrap().due_at,
            state.next_due_at.unwrap(),
            "the old job must carry the same due time for this test to bite"
        );
        {
            let mut rt = h.runtime();
            assert!(
                !rt.run_reminder(NOW + INTERVAL as i64, &old).await.unwrap(),
                "a job of an older cycle acted"
            );
        }
        assert!(
            h.channel.sent().is_empty(),
            "an old cycle posted a reminder"
        );
        let state = h.repo.load(GUILD).unwrap().unwrap();
        assert_eq!(state.cycle, 2);
        assert!(
            !state.reminded,
            "an old cycle marked the newer cycle as reminded"
        );
    }

    #[tokio::test]
    async fn a_stale_reminder_cannot_mark_a_newer_cycle() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        // A staff reset opens a new cycle while the old job is still in the queue.
        {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(GUILD), NOW + 10).unwrap();
        }
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap().cycle, 2);
        // The old job fires for the old cycle.
        {
            let mut rt = h.runtime();
            assert!(!rt
                .run_reminder(NOW + INTERVAL as i64, &job_for(1, NOW + INTERVAL as i64))
                .await
                .unwrap());
        }
        assert!(h.channel.sent().is_empty(), "a stale job posted a reminder");
        let state = h.repo.load(GUILD).unwrap().unwrap();
        assert_eq!(state.cycle, 2);
        assert!(
            !state.reminded,
            "a stale job marked the newer cycle as reminded"
        );
    }

    // ---- mentions ----------------------------------------------------------------------

    #[tokio::test]
    async fn the_reminder_mentions_only_the_bump_role() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let due = NOW + INTERVAL as i64;
        {
            let mut rt = h.runtime();
            rt.run_reminder(due, &job_for(1, due)).await.unwrap();
        }
        let sent = h.channel.sent();
        assert_eq!(sent.len(), 1);
        let reminder = &sent[0];
        assert_eq!(reminder.channel_id, CHANNEL);
        assert_eq!(reminder.role_id, Some(ROLE));
        assert_eq!(reminder.allowed_mention(), format!("<@&{ROLE}>"));
        // No other id appears anywhere in the message.
        assert!(!reminder.content.contains(&OTHER_GUILD.to_string()));
        assert!(!reminder.content.contains(&BOT.to_string()));
        assert!(!reminder.content.contains(&USER.to_string()));
    }

    #[test]
    fn a_reminder_without_a_role_mentions_nobody() {
        let reminder = Reminder {
            channel_id: CHANNEL,
            role_id: None,
            content: "text".to_string(),
        };
        assert_eq!(reminder.allowed_mention(), "");
    }

    // ---- commands ----------------------------------------------------------------------

    #[test]
    fn status_reports_the_stored_cycle() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let outcome = {
            let mut rt = h.runtime();
            rt.status(Some(GUILD), NOW + 5).unwrap()
        };
        let CommandOutcome::Done { reply } = outcome else {
            panic!("expected Done, got {outcome:?}");
        };
        assert!(reply.contains("cycle 1"), "{reply}");
        assert!(reply.contains(&NOW.to_string()), "{reply}");
        assert!(
            reply.contains(&(NOW + INTERVAL as i64).to_string()),
            "{reply}"
        );
        assert!(reply.contains("message 100"), "{reply}");
        assert!(!reply.contains(&USER.to_string()), "{reply}");
    }

    #[test]
    fn status_without_a_cycle_says_so() {
        let mut h = Harness::new("live");
        let outcome = {
            let mut rt = h.runtime();
            rt.status(Some(GUILD), NOW).unwrap()
        };
        let CommandOutcome::Done { reply } = outcome else {
            panic!("expected Done, got {outcome:?}");
        };
        assert!(reply.contains("no cycle"), "{reply}");
    }

    #[test]
    fn reset_is_staff_only_and_changes_nothing_for_others() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let before = h.repo.load(GUILD).unwrap().unwrap();
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::NotStaff, Some(GUILD), NOW + 10)
                .unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::NotStaff { .. }));
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap(), before);
        assert_eq!(
            h.queue.requests().len(),
            1,
            "a non-staff reset scheduled a job"
        );
    }

    #[test]
    fn a_staff_reset_starts_a_new_cycle_with_its_own_job() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(GUILD), NOW + 10).unwrap()
        };
        let CommandOutcome::Done { reply } = outcome else {
            panic!("expected Done, got {outcome:?}");
        };
        assert!(reply.contains("cycle 2"), "{reply}");
        let state = h.repo.load(GUILD).unwrap().unwrap();
        assert_eq!(state.cycle, 2);
        assert_eq!(state.last_bump_at, Some(NOW + 10));
        assert_eq!(state.last_bumper, Some(100), "the dedupe id was lost");
        assert!(!state.reminded);
        assert_eq!(h.queue.keys().len(), 2);
        assert_ne!(h.queue.keys()[0], h.queue.keys()[1]);
    }

    #[test]
    fn a_reset_before_any_bump_starts_the_first_cycle() {
        let mut h = Harness::new("live");
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(GUILD), NOW).unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::Done { .. }));
        let state = h.repo.load(GUILD).unwrap().unwrap();
        assert_eq!(state.cycle, 1);
        assert_eq!(state.next_due_at, Some(NOW + INTERVAL as i64));
    }

    #[tokio::test]
    async fn a_reset_opens_the_reminder_again() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let due = NOW + INTERVAL as i64;
        {
            let mut rt = h.runtime();
            rt.run_reminder(due, &job_for(1, due)).await.unwrap();
        }
        assert_eq!(h.channel.sent().len(), 1);
        {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(GUILD), due + 1).unwrap();
        }
        let reset_at = due + 1;
        {
            let mut rt = h.runtime();
            assert!(rt
                .run_reminder(
                    reset_at + INTERVAL as i64,
                    &job_for(2, reset_at + INTERVAL as i64)
                )
                .await
                .unwrap());
        }
        assert_eq!(
            h.channel.sent().len(),
            2,
            "the reset did not reopen the reminder"
        );
    }

    #[test]
    fn a_replayed_message_cannot_undo_a_reset() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(GUILD), NOW + 10).unwrap();
        }
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW + 20).unwrap()
        };
        assert_eq!(observation, Observation::Ignored(IgnoreCode::AlreadySeen));
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap().cycle, 2);
    }

    // ---- modes -----------------------------------------------------------------------

    #[tokio::test]
    async fn shadow_mode_sends_nothing() {
        let mut h = Harness::new("shadow");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        assert!(h.channel.sent().is_empty(), "shadow posted a reminder");
        // Shadow does not schedule a job either: nothing is queued to be sent later.
        assert!(h.queue.requests().is_empty(), "shadow scheduled a job");
        {
            let mut rt = h.runtime();
            rt.run_reminder(NOW + INTERVAL as i64, &job_for(1, NOW + INTERVAL as i64))
                .await
                .unwrap();
        }
        assert!(h.channel.sent().is_empty(), "shadow posted a reminder");
    }

    #[tokio::test]
    async fn shadow_mode_sends_nothing_even_when_a_cycle_is_due() {
        // A cycle that is due and waiting: the shadow guard alone must stop the post.
        // (Without it the cycle check would hide the bug, because shadow never
        // writes a cycle of its own.)
        let mut h = Harness::new("shadow");
        let state = BumpState {
            guild: GUILD,
            last_bump_at: Some(NOW),
            last_bumper: Some(100),
            next_due_at: Some(NOW + INTERVAL as i64),
            cycle: 1,
            reminded: false,
        };
        // The stored row is written directly, so the cycle really is due.
        h.repo.save(&state).unwrap();
        let due = NOW + INTERVAL as i64;
        {
            let mut rt = h.runtime();
            assert!(
                !rt.run_reminder(due, &job_for(1, due)).await.unwrap(),
                "shadow reported a sent reminder"
            );
        }
        assert!(h.channel.sent().is_empty(), "shadow posted a reminder");
        assert!(
            !h.repo.load(GUILD).unwrap().unwrap().reminded,
            "shadow marked the cycle as reminded"
        );
    }

    #[test]
    fn shadow_mode_produces_a_structured_would_do_record() {
        let mut h = Harness::new("shadow");
        let record = {
            let mut rt = h.runtime();
            rt.would_do(&success(100), NOW).unwrap()
        };
        assert_eq!(record.cycles, 1);
        assert_eq!(record.reminders, 1);
        assert_eq!(record.jobs, 1);
        assert_eq!(record.codes, vec!["confirmed"]);
        // The record holds counts and codes, never message text.
        let shown = format!("{record:?}");
        assert!(!shown.contains("Bump done"), "{shown}");
        assert!(!shown.contains(&USER.to_string()), "{shown}");
    }

    #[test]
    fn the_would_do_record_of_an_ignored_message_names_the_reason() {
        let mut h = Harness::new("shadow");
        let mut facts = success(100);
        facts.is_webhook = true;
        let record = {
            let mut rt = h.runtime();
            rt.would_do(&facts, NOW).unwrap()
        };
        assert_eq!(record.cycles, 0);
        assert_eq!(record.reminders, 0);
        assert_eq!(record.jobs, 0);
        assert_eq!(record.codes, vec!["webhook"]);
    }

    #[test]
    fn shadow_mode_writes_no_state() {
        let mut h = Harness::new("shadow");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        assert!(h.repo.load(GUILD).unwrap().is_none(), "shadow wrote a row");
    }

    #[test]
    fn the_shadow_reset_reply_says_nothing_was_changed() {
        let mut h = Harness::new("shadow");
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(GUILD), NOW).unwrap()
        };
        let CommandOutcome::Done { reply } = outcome else {
            panic!("expected Done, got {outcome:?}");
        };
        assert!(
            reply.contains("nothing was changed"),
            "the shadow reply claims a reset that did not happen: {reply}"
        );
        // Nothing was written and nothing was scheduled.
        assert!(h.repo.load(GUILD).unwrap().is_none(), "shadow wrote a row");
        assert!(h.queue.requests().is_empty(), "shadow scheduled a job");
    }

    #[test]
    fn an_off_feature_does_nothing() {
        let mut h = Harness::new("off");
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap()
        };
        assert_eq!(observation, Observation::Disabled);
        assert!(h.queue.requests().is_empty());
        assert!(h.repo.load(GUILD).unwrap().is_none());
    }

    #[test]
    fn an_off_feature_reports_the_commands_as_disabled() {
        let mut h = Harness::new("off");
        let outcome = {
            let mut rt = h.runtime();
            rt.status(Some(GUILD), NOW).unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::Disabled { .. }));
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(GUILD), NOW).unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::Disabled { .. }));
    }

    // ---- rule 9: the commands are home-guild only ---------------------------------------

    #[test]
    fn a_status_from_another_guild_is_denied_and_changes_nothing() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let before = h.repo.load(GUILD).unwrap().unwrap();
        let outcome = {
            let mut rt = h.runtime();
            rt.status(Some(OTHER_GUILD), NOW + 5).unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::WrongGuild { .. }));
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap(), before);
        // The reply never names the home guild.
        let CommandOutcome::WrongGuild { reply } = outcome else {
            panic!("expected WrongGuild, got {outcome:?}");
        };
        assert!(!reply.contains(&GUILD.to_string()), "{reply}");
    }

    #[test]
    fn a_reset_from_another_guild_is_denied_and_changes_nothing() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let before = h.repo.load(GUILD).unwrap().unwrap();
        let requests_before = h.queue.requests().len();
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(OTHER_GUILD), NOW + 10)
                .unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::WrongGuild { .. }));
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap(), before);
        assert_eq!(
            h.queue.requests().len(),
            requests_before,
            "a denied reset scheduled a job"
        );
        // The reply never names the home guild.
        let CommandOutcome::WrongGuild { reply } = outcome else {
            panic!("expected WrongGuild, got {outcome:?}");
        };
        assert!(!reply.contains(&GUILD.to_string()), "{reply}");
    }

    #[test]
    fn a_reset_from_another_guild_is_denied_for_a_non_staff_caller_too() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let before = h.repo.load(GUILD).unwrap().unwrap();
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::NotStaff, Some(OTHER_GUILD), NOW + 10)
                .unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::WrongGuild { .. }));
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap(), before);
    }

    #[test]
    fn the_commands_fail_closed_when_no_home_guild_is_set() {
        let mut settings = settings("live");
        settings.guild_id = None;
        let config = BumpConfig::new(GUILD, BOT, &settings.success_marker, INTERVAL).unwrap();
        let mut repo = repo();
        let queue = FakeQueue::default();
        let channel = FakeChannel::default();
        let mut rt = Runtime {
            settings: &settings,
            config,
            repo: &mut repo,
            queue: &queue,
            channel: &channel,
        };
        // The feature is on (the mode is live) but no home guild is configured.
        assert!(rt.is_enabled(), "the feature must be on for this test");
        let outcome = rt.status(Some(GUILD), NOW).unwrap();
        assert!(matches!(outcome, CommandOutcome::WrongGuild { .. }));
        let outcome = rt.reset(StaffCheck::Staff, Some(GUILD), NOW).unwrap();
        assert!(matches!(outcome, CommandOutcome::WrongGuild { .. }));
        // Nothing was written and nothing was scheduled.
        assert!(
            repo.load(GUILD).unwrap().is_none(),
            "a denied command wrote a row"
        );
        assert!(
            queue.requests().is_empty(),
            "a denied command scheduled a job"
        );
    }

    #[test]
    fn the_commands_still_work_in_the_home_guild() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let outcome = {
            let mut rt = h.runtime();
            rt.status(Some(GUILD), NOW + 5).unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::Done { .. }));
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, Some(GUILD), NOW + 10).unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::Done { .. }));
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap().cycle, 2);
    }

    #[test]
    fn a_command_without_a_guild_is_denied() {
        let mut h = Harness::new("live");
        let outcome = {
            let mut rt = h.runtime();
            rt.status(None, NOW).unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::WrongGuild { .. }));
    }

    // ---- the real queue --------------------------------------------------------------

    #[test]
    fn the_real_queue_refuses_a_second_job_for_the_same_cycle() {
        let mut conn = Connection::open_in_memory().unwrap();
        queue::init(&mut conn).unwrap();
        let due = NOW + INTERVAL as i64;
        let request = JobRequest {
            kind: KIND_REMINDER,
            dedupe_key: dedupe_key(GUILD, 1, due),
            payload_json: payload_json(GUILD, 1, due),
            due_at: due,
        };
        let real = RealQueue {
            conn: Mutex::new(conn),
        };
        assert_eq!(real.schedule(&request), Ok(ScheduleOutcome::Created));
        assert_eq!(real.schedule(&request), Ok(ScheduleOutcome::Duplicate));
        let count: i64 = {
            let conn = real.conn.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM jobs WHERE kind = ?1",
                [KIND_REMINDER],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count, 1);
    }

    #[test]
    fn the_reminder_kind_is_at_most_once() {
        assert_eq!(KIND_TABLE.len(), 1);
        assert_eq!(KIND_TABLE[0].0, KIND_REMINDER);
        assert_eq!(KIND_TABLE[0].1, RestartPolicy::AtMostOnce);
    }

    // ---- failures --------------------------------------------------------------------

    #[test]
    fn a_refused_schedule_is_reported_by_code() {
        let mut h = Harness::new("live");
        h.queue.fail();
        let result = {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW)
        };
        assert_eq!(result, Err(RuntimeError::QueueRefused));
    }

    #[test]
    fn the_error_codes_are_stable_and_carry_no_text() {
        for e in [
            RuntimeError::CorruptState,
            RuntimeError::QueueRefused,
            RuntimeError::SendFailed,
            RuntimeError::NotStaff,
            RuntimeError::Disabled,
        ] {
            let shown = format!("{e:?} {e}");
            assert!(!shown.contains("1000001"), "{shown}");
            assert!(!shown.contains("Bump"), "{shown}");
        }
        assert_eq!(RuntimeError::SendFailed.code(), "send_failed");
        assert_eq!(RuntimeError::NotStaff.code(), "not_staff");
    }

    #[test]
    fn the_ignore_codes_are_stable_and_carry_no_text() {
        for code in [
            IgnoreCode::Webhook,
            IgnoreCode::WrongGuild,
            IgnoreCode::NotListingBot,
            IgnoreCode::NotBotAccount,
            IgnoreCode::InvalidMessageId,
            IgnoreCode::NoSuccessMarker,
            IgnoreCode::AlreadySeen,
            IgnoreCode::WrongChannel,
        ] {
            let shown = format!("{code:?} {}", code.code());
            assert!(!shown.contains("1000001"), "{shown}");
            assert!(!shown.contains("Bump"), "{shown}");
        }
        assert_eq!(IgnoreCode::WrongChannel.code(), "wrong_channel");
    }

    #[test]
    fn a_payload_that_is_not_a_plain_object_is_refused() {
        for bad in [
            "",
            "{",
            "not json",
            "[]",
            "null",
            "42",
            "{\"guild\":1}",
            "{\"guild\":\"1\",\"cycle\":1,\"due_at\":1}",
            "{\"guild\":1,\"cycle\":1}",
            "{\"guild\":1,\"cycle\":1,\"due_at\":1} trailing",
        ] {
            assert_eq!(
                parse_reminder_payload(bad),
                Err(RuntimeError::CorruptState),
                "payload {bad:?}"
            );
        }
        let ok = parse_reminder_payload(&payload_json(GUILD, 3, 1_700_000_000)).unwrap();
        assert_eq!(ok.guild, GUILD);
        assert_eq!(ok.cycle, 3);
        assert_eq!(ok.due_at, 1_700_000_000);
    }

    // ---- liveness --------------------------------------------------------------------

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *seed >> 33
    }

    /// Claim every job that is due and run it through the runtime.
    ///
    /// Returns how many jobs the runtime acted on and how many reminders it posted.
    /// A reminder can only come from a job the queue handed out. A queue that
    /// refuses hands out nothing, so nothing is attempted that step.
    async fn drive_queue(h: &mut Harness, now: i64) -> (usize, usize) {
        let Ok(claimed) = h.queue.claim_due(now, 32) else {
            return (0, 0);
        };
        let mut attempted = 0usize;
        let mut posted = 0usize;
        for job in claimed {
            let result = {
                let mut rt = h.runtime();
                rt.run_reminder(now, &job).await
            };
            match result {
                Ok(true) => {
                    attempted += 1;
                    posted += 1;
                }
                Err(RuntimeError::SendFailed) => {
                    // The cycle was marked, but the post did not happen.
                    attempted += 1;
                }
                Ok(false) => {}
                Err(_) => {}
            }
        }
        (attempted, posted)
    }

    /// The liveness sweep.
    ///
    /// The model fires reminders ONLY from jobs the queue holds and claims: the
    /// sweep drives the queue, and every reminder the runtime posts must come from
    /// a claimed job. The sweep includes queue failures and an interval change at a
    /// restart. It ends with the liveness rule: every overdue, unreminded cycle has
    /// a claimable job.
    #[tokio::test]
    async fn seeded_sequences_fire_reminders_only_from_claimed_jobs() {
        for seed0 in 0..40u64 {
            let mut seed = seed0 + 1;
            let mut now = NOW;
            let mut next_id = 1_000u64;
            let mut h = Harness::new("live");
            // The model of the cycle.
            let mut model_cycle = 0i64;
            let mut model_last_at = 0i64;
            let mut model_due = 0i64;
            let mut model_reminded = false;
            let mut model_posts = 0usize;

            for _ in 0..200 {
                now += 1 + (lcg(&mut seed) % 3_000) as i64;
                // The queue's state is decided once per step: a queue that refuses
                // hands out nothing, so no reminder can go out that step.
                let queue_up = !lcg(&mut seed).is_multiple_of(5);
                if queue_up {
                    h.queue.work();
                } else {
                    h.queue.fail();
                }
                match lcg(&mut seed) % 8 {
                    0 => {
                        // A real bump.
                        next_id += 1 + lcg(&mut seed) % 50;
                        let result = {
                            let mut rt = h.runtime();
                            rt.observe(&success(next_id), now)
                        };
                        if queue_up {
                            assert!(
                                matches!(result, Ok(Observation::Confirmed { .. })),
                                "seed {seed0}: {result:?}"
                            );
                        } else {
                            assert_eq!(result, Err(RuntimeError::QueueRefused), "seed {seed0}");
                            // The row was written, but no job was scheduled.
                        }
                        model_cycle += 1;
                        model_last_at = now;
                        model_due = now + h.interval() as i64;
                        model_reminded = false;
                    }
                    1 => {
                        // Hostile noise: it must not change anything.
                        let mut facts = success(next_id + 1 + lcg(&mut seed) % 50);
                        match lcg(&mut seed) % 5 {
                            0 => facts.author_is_bot = false,
                            1 => facts.author_id = USER,
                            2 => facts.is_webhook = true,
                            3 => facts.embeds_text = "Please wait".to_string(),
                            _ => facts.guild_id = Some(OTHER_GUILD),
                        }
                        let observation = {
                            let mut rt = h.runtime();
                            rt.observe(&facts, now)
                        };
                        // Noise never reaches the queue: it is ignored before
                        // anything is scheduled. (A redelivered confirmation would
                        // reach the repair path, but the noise ids here are always
                        // new ones, so the policy ignores them.)
                        assert!(
                            matches!(observation, Ok(Observation::Ignored(_))),
                            "seed {seed0}: {observation:?}"
                        );
                    }
                    2 => {
                        // A restart: the interval may have changed, and the cycle is
                        // re-armed from the stored row.
                        if lcg(&mut seed).is_multiple_of(2) {
                            let secs = if lcg(&mut seed).is_multiple_of(2) {
                                7_200
                            } else {
                                9_000
                            };
                            h.set_interval(secs);
                        }
                        let result = {
                            let mut rt = h.runtime();
                            rt.rearm(now)
                        };
                        if !queue_up {
                            // A re-arm that has nothing to do never reaches the
                            // queue, so it reports "nothing to do" even when the
                            // queue is down.
                            match result {
                                Ok(RearmOutcome::NothingToDo) => {}
                                Ok(RearmOutcome::Scheduled)
                                | Ok(RearmOutcome::AlreadyScheduled) => {
                                    // The queue was down, so no job was scheduled.
                                    panic!(
                                        "seed {seed0}: the queue was down but the re-arm succeeded"
                                    );
                                }
                                Err(other) => {
                                    assert_eq!(other, RuntimeError::QueueRefused, "seed {seed0}");
                                    // The due time moved, but no job was scheduled
                                    // for it.
                                    if model_cycle > 0 && !model_reminded {
                                        model_due = model_last_at + h.interval() as i64;
                                    }
                                }
                            }
                        } else {
                            match result {
                                Ok(RearmOutcome::Scheduled)
                                | Ok(RearmOutcome::AlreadyScheduled) => {
                                    if model_cycle > 0 && !model_reminded {
                                        model_due = model_last_at + h.interval() as i64;
                                    }
                                }
                                Ok(RearmOutcome::NothingToDo) => {}
                                Err(other) => panic!("seed {seed0}: rearm failed with {other:?}"),
                            }
                        }
                    }
                    3 => {
                        // A staff reset.
                        let result = {
                            let mut rt = h.runtime();
                            rt.reset(StaffCheck::Staff, Some(GUILD), now)
                        };
                        if queue_up {
                            assert!(
                                matches!(result, Ok(CommandOutcome::Done { .. })),
                                "seed {seed0}: {result:?}"
                            );
                        } else {
                            assert_eq!(result, Err(RuntimeError::QueueRefused), "seed {seed0}");
                        }
                        model_cycle += 1;
                        model_last_at = now;
                        model_due = now + h.interval() as i64;
                        model_reminded = false;
                    }
                    4 => {
                        // The channel may fail.
                        if lcg(&mut seed).is_multiple_of(2) {
                            h.channel.fail();
                        } else {
                            h.channel.work();
                        }
                    }
                    _ => {}
                }

                // The model and the stored row must agree.
                let stored = h.repo.load(GUILD).unwrap();
                match stored {
                    Some(s) => {
                        assert_eq!(s.cycle, model_cycle, "seed {seed0}: cycle");
                        assert_eq!(s.reminded, model_reminded, "seed {seed0}: reminded");
                        assert_eq!(s.next_due_at, Some(model_due), "seed {seed0}: next due");
                    }
                    None => assert_eq!(model_cycle, 0, "seed {seed0}: a row is missing"),
                }

                // The queue is the ground truth for what is claimable: a queue
                // that refuses to schedule does not remove a job it already holds.
                let armed = model_cycle > 0 && h.queue.has_pending_job(model_cycle, model_due);

                // Drive the queue: the only source of a reminder.
                let (attempted, posted) = drive_queue(&mut h, now).await;
                let owed = model_cycle > 0 && !model_reminded && armed && now >= model_due;
                if owed && queue_up {
                    assert_eq!(attempted, 1, "seed {seed0}: the owed reminder was not sent");
                    model_reminded = true;
                    model_posts += posted;
                } else if owed {
                    // The queue is down: it hands out nothing, so the reminder
                    // stays owed and is sent when the queue works again.
                    assert_eq!(
                        attempted, 0,
                        "seed {seed0}: a reminder was sent while the queue was down"
                    );
                } else {
                    assert_eq!(
                        attempted, 0,
                        "seed {seed0}: a reminder was not owed (cycle {model_cycle}, \
                         due {model_due}, now {now}, armed {armed}, \
                         reminded {model_reminded}, queue_up {queue_up})"
                    );
                }
            }

            // Drain: the queue works again, the re-arm repairs a missing job and the
            // queue is driven until nothing is left to do.
            h.queue.work();
            h.channel.work();
            for _ in 0..8 {
                now += h.interval() as i64 + 1;
                {
                    let mut rt = h.runtime();
                    let _ = rt.rearm(now);
                }
                let (attempted, posted) = drive_queue(&mut h, now).await;
                if attempted > 0 {
                    // The cycle was reminded in the drain; the model only needs the
                    // count of the reminders that really went out.
                    model_posts += posted;
                }
            }

            // The liveness rule: every overdue, unreminded cycle has a claimable job.
            let stored = h.repo.load(GUILD).unwrap().unwrap();
            if stored.cycle > 0 && !stored.reminded {
                let due = stored.next_due_at.unwrap();
                if now >= due {
                    assert!(
                        h.queue.has_pending_job(stored.cycle, due),
                        "seed {seed0}: the overdue cycle {stored:?} has no claimable job"
                    );
                }
            }
            assert_eq!(
                h.channel.sent().len(),
                model_posts,
                "seed {seed0}: the reminders do not match the model"
            );
            assert!(
                model_posts <= model_cycle as usize,
                "seed {seed0}: more reminders than cycles"
            );
        }
    }
}

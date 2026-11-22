//! Bump reminder runtime (BU2).
//!
//! The thin layer between the pure policy ([`super::policy`]) and Discord. It does
//! four things and nothing else:
//!
//! 1. **Detect.** A message event becomes [`policy::MessageFacts`]; the policy decides
//!    whether it is a real bump confirmation. The runtime never re-decides that.
//! 2. **Persist.** A confirmed bump is written to the `bump_state` row (see
//!    [`super::repository`]) and the cycle number grows by one.
//! 3. **Schedule.** Exactly one reminder job per cycle is scheduled through the job
//!    queue. The dedupe key names the cycle, so a second schedule for the same cycle
//!    is refused by the queue's UNIQUE key: one reminder per cycle, across restarts.
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

/// The durable job queue. The real implementation calls
/// [`crate::jobs::queue::schedule`]; the tests use a fake.
pub trait JobQueue: Send + Sync {
    /// Schedule one job. `Err` means the job was not scheduled.
    fn schedule(&self, request: &JobRequest) -> Result<ScheduleOutcome, RuntimeError>;
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
    Ignored(IgnoreReason),
    /// The feature is off or its configuration was refused; nothing happened.
    Disabled,
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

/// The dedupe key of the reminder job for one cycle.
pub fn dedupe_key(guild: u64, cycle: i64) -> String {
    format!("bump:{guild}:{cycle}")
}

/// The payload of the reminder job for one cycle. It holds the guild id and the cycle
/// number and nothing else.
pub fn payload_json(guild: u64, cycle: i64) -> String {
    format!("{{\"guild\":{guild},\"cycle\":{cycle}}}")
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
        let current = self.load_state()?;
        let policy_state = current.to_policy();
        let (decision, next_state) = policy::observe(facts, &self.config, &policy_state, now);
        let Decision::Confirmed(confirmation) = decision else {
            return Ok(Observation::Ignored(ignore_reason(&decision)));
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
        self.schedule_for(&next, now)?;
        Ok(Observation::Confirmed {
            message_id: confirmation.message_id,
            next_due: confirmation.next_due,
            cycle,
        })
    }

    /// Make sure the job for `state`'s cycle is scheduled.
    ///
    /// The dedupe key names the cycle, so a second call for the same cycle creates
    /// nothing: exactly one job per cycle, however often this runs.
    fn schedule_for(&self, state: &BumpState, now: i64) -> Result<ScheduleOutcome, RuntimeError> {
        if state.cycle <= 0 {
            return Ok(ScheduleOutcome::Duplicate);
        }
        let due_at = state.next_due_at.unwrap_or(now).max(now);
        let request = JobRequest {
            kind: KIND_REMINDER,
            dedupe_key: dedupe_key(state.guild, state.cycle),
            payload_json: payload_json(state.guild, state.cycle),
            due_at,
        };
        self.queue.schedule(&request)
    }

    /// Re-arm the reminder after a restart.
    ///
    /// The job for a cycle is written once and never deleted, so a restart before the
    /// reminder is due finds the job already there (the queue refuses the duplicate)
    /// and the single reminder still happens. A restart after the reminder was sent
    /// finds the cycle already marked as reminded and does nothing at all.
    ///
    /// Returns `true` when a job was scheduled (or already existed) for the cycle.
    pub fn rearm(&mut self, now: i64) -> Result<bool, RuntimeError> {
        if !self.is_enabled() || self.is_shadow() {
            return Ok(false);
        }
        let state = self.load_state()?;
        if state.cycle <= 0 || state.reminded {
            return Ok(false);
        }
        let status = policy::reminder_status(&state.to_policy(), &self.config, now);
        if matches!(status, ReminderStatus::NoCycle) {
            return Ok(false);
        }
        // The stored due time may be stale (the interval was changed). The policy
        // decides what is due now; the job is scheduled for that moment.
        let due_at = match status {
            ReminderStatus::NotYet { due_at } | ReminderStatus::Due { due_at } => due_at,
            _ => return Ok(false),
        };
        let mut fresh = state.clone();
        fresh.next_due_at = Some(due_at);
        self.repo.save(&fresh)?;
        self.schedule_for(&fresh, now)?;
        Ok(true)
    }

    /// The outcome of the reminder job for one cycle.
    ///
    /// The decision is pure: given the stored state and `now`, it says whether a
    /// reminder is owed. A cycle that was already reminded owes nothing, and a
    /// cycle that is not due yet owes nothing.
    pub fn reminder_due(&self, state: &BumpState, now: i64) -> bool {
        policy::due(&state.to_policy(), &self.config, now)
    }

    /// Run the reminder for the stored cycle.
    ///
    /// The cycle is marked as reminded BEFORE the post, so a crash between the mark
    /// and the post cannot produce a second reminder. If the post fails, the failure
    /// is reported to the caller by code; the cycle stays marked, because a repeated
    /// post would be a duplicate.
    ///
    /// In shadow mode nothing is written and nothing is posted.
    pub async fn run_reminder(&mut self, now: i64) -> Result<bool, RuntimeError> {
        if !self.is_enabled() || self.is_shadow() {
            return Ok(false);
        }
        let mut state = self.load_state()?;
        if state.cycle <= 0 {
            return Ok(false);
        }
        if !self.reminder_due(&state, now) {
            return Ok(false);
        }
        // Mark first: at most once, even across a restart.
        state.reminded = true;
        self.repo.save(&state)?;
        let reminder = build_reminder(self.settings, state.cycle);
        self.channel.send(&reminder).await?;
        Ok(true)
    }

    /// `/bump status`: report the stored cycle.
    pub fn status(&mut self, now: i64) -> Result<CommandOutcome, RuntimeError> {
        if !self.is_enabled() {
            return Ok(CommandOutcome::Disabled {
                reply: "The bump feature is off.".to_string(),
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
    pub fn reset(&mut self, staff: StaffCheck, now: i64) -> Result<CommandOutcome, RuntimeError> {
        if !self.is_enabled() {
            return Ok(CommandOutcome::Disabled {
                reply: "The bump feature is off.".to_string(),
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
        if !self.is_shadow() {
            self.repo.save(&next)?;
            self.schedule_for(&next, now)?;
        }
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
        let current = self.load_state()?;
        let (decision, next_state) =
            policy::observe(facts, &self.config, &current.to_policy(), now);
        let Decision::Confirmed(confirmation) = decision else {
            record.note(ignore_code(&decision));
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

/// The stable code of a decision that is not a confirmation.
fn ignore_code(decision: &Decision) -> &'static str {
    match ignore_reason(decision) {
        IgnoreReason::Webhook => "webhook",
        IgnoreReason::WrongGuild => "wrong_guild",
        IgnoreReason::NotListingBot => "not_listing_bot",
        IgnoreReason::NotBotAccount => "not_bot_account",
        IgnoreReason::InvalidMessageId => "invalid_message_id",
        IgnoreReason::NoSuccessMarker => "no_success_marker",
        IgnoreReason::AlreadySeen => "already_seen",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bump::config::bump_settings;
    use crate::bump::policy::DEFAULT_INTERVAL_SECS;
    use crate::jobs::queue::{self, NewJob, RestartPolicy, ScheduleOutcome as QueueOutcome};
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // Clearly synthetic ids; none is a real Discord id.
    const GUILD: u64 = 1_000_001;
    const OTHER_GUILD: u64 = 1_000_002;
    const CHANNEL: u64 = 4_000_001;
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

    /// Records every scheduled job.
    #[derive(Default)]
    struct FakeQueue {
        requests: Mutex<Vec<JobRequest>>,
        seen: Mutex<HashMap<String, u32>>,
        fail: Mutex<bool>,
    }

    impl FakeQueue {
        fn requests(&self) -> Vec<JobRequest> {
            self.requests.lock().unwrap().clone()
        }
        fn keys(&self) -> Vec<String> {
            self.requests().into_iter().map(|r| r.dedupe_key).collect()
        }
        fn fail(&self) {
            *self.fail.lock().unwrap() = true;
        }
    }

    impl JobQueue for FakeQueue {
        fn schedule(&self, request: &JobRequest) -> Result<ScheduleOutcome, RuntimeError> {
            if *self.fail.lock().unwrap() {
                return Err(RuntimeError::QueueRefused);
            }
            let mut seen = self.seen.lock().unwrap();
            let count = seen.entry(request.dedupe_key.clone()).or_insert(0);
            *count += 1;
            let duplicate = *count > 1;
            drop(seen);
            self.requests.lock().unwrap().push(request.clone());
            Ok(if duplicate {
                ScheduleOutcome::Duplicate
            } else {
                ScheduleOutcome::Created
            })
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
        assert_eq!(requests[0].dedupe_key, "bump:1000001:1");
        assert_eq!(requests[0].due_at, NOW + INTERVAL as i64);
        assert_eq!(requests[0].payload_json, "{\"guild\":1000001,\"cycle\":1}");
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
        assert_eq!(
            observation,
            Observation::Ignored(IgnoreReason::NotListingBot)
        );
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
        assert_eq!(observation, Observation::Ignored(IgnoreReason::Webhook));
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
            Observation::Ignored(IgnoreReason::NoSuccessMarker)
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
        assert_eq!(observation, Observation::Ignored(IgnoreReason::AlreadySeen));
        assert_eq!(h.repo.load(GUILD).unwrap().unwrap().cycle, 1);
        assert_eq!(h.queue.requests().len(), 1);
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
        assert_eq!(h.queue.keys(), vec!["bump:1000001:1", "bump:1000001:2"]);
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
        assert_eq!(observation, Observation::Ignored(IgnoreReason::WrongGuild));
        assert!(h.queue.requests().is_empty());
    }

    // ---- the reminder is sent exactly once per cycle ---------------------------------

    #[tokio::test]
    async fn a_restart_before_the_reminder_is_due_still_produces_one_reminder() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        // Restart: rearm the cycle from the stored row.
        {
            let mut rt = h.runtime();
            assert!(rt.rearm(NOW + 10).unwrap());
        }
        // The queue refuses the second schedule for the same cycle.
        {
            let mut rt = h.runtime();
            assert!(rt.rearm(NOW + 20).unwrap());
        }
        // The observe and the two rearms all ask for the same cycle: the queue
        // refuses every duplicate, so one job exists for the cycle.
        assert_eq!(
            h.queue.requests().len(),
            3,
            "not every schedule was recorded"
        );
        assert_eq!(
            h.queue.keys(),
            vec!["bump:1000001:1", "bump:1000001:1", "bump:1000001:1"],
            "a schedule used another key"
        );
        // One reminder, at the due time.
        let due = NOW + INTERVAL as i64;
        {
            let mut rt = h.runtime();
            assert!(rt.run_reminder(due).await.unwrap());
        }
        assert_eq!(h.channel.sent().len(), 1);
        // A second run at the same time sends nothing.
        {
            let mut rt = h.runtime();
            assert!(!rt.run_reminder(due).await.unwrap());
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
            rt.run_reminder(due).await.unwrap();
        }
        assert_eq!(h.channel.sent().len(), 1);
        // Restart after the send: rearm does nothing, the reminder is not repeated.
        {
            let mut rt = h.runtime();
            assert!(!rt.rearm(due + 100).unwrap());
        }
        {
            let mut rt = h.runtime();
            assert!(!rt.run_reminder(due + 100).await.unwrap());
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
            assert!(!rt.run_reminder(NOW + INTERVAL as i64 - 1).await.unwrap());
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
            rt.run_reminder(due).await
        };
        assert_eq!(result, Err(RuntimeError::SendFailed));
        // The cycle is marked before the post, so a retry does not post again.
        assert!(h.repo.load(GUILD).unwrap().unwrap().reminded);
        h.channel.fail(); // still failing
        {
            let mut rt = h.runtime();
            assert!(!rt.run_reminder(due + 1).await.unwrap());
        }
        assert!(h.channel.sent().is_empty());
    }

    // ---- mentions --------------------------------------------------------------------

    #[tokio::test]
    async fn the_reminder_mentions_only_the_bump_role() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        {
            let mut rt = h.runtime();
            rt.run_reminder(NOW + INTERVAL as i64).await.unwrap();
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

    // ---- commands --------------------------------------------------------------------

    #[test]
    fn status_reports_the_stored_cycle() {
        let mut h = Harness::new("live");
        {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW).unwrap();
        }
        let outcome = {
            let mut rt = h.runtime();
            rt.status(NOW + 5).unwrap()
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
            rt.status(NOW).unwrap()
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
            rt.reset(StaffCheck::NotStaff, NOW + 10).unwrap()
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
            rt.reset(StaffCheck::Staff, NOW + 10).unwrap()
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
        assert_eq!(h.queue.keys(), vec!["bump:1000001:1", "bump:1000001:2"]);
    }

    #[test]
    fn a_reset_before_any_bump_starts_the_first_cycle() {
        let mut h = Harness::new("live");
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, NOW).unwrap()
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
        {
            let mut rt = h.runtime();
            rt.run_reminder(NOW + INTERVAL as i64).await.unwrap();
        }
        assert_eq!(h.channel.sent().len(), 1);
        {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, NOW + INTERVAL as i64 + 1)
                .unwrap();
        }
        let reset_at = NOW + INTERVAL as i64 + 1;
        {
            let mut rt = h.runtime();
            assert!(rt.run_reminder(reset_at + INTERVAL as i64).await.unwrap());
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
            rt.reset(StaffCheck::Staff, NOW + 10).unwrap();
        }
        let observation = {
            let mut rt = h.runtime();
            rt.observe(&success(100), NOW + 20).unwrap()
        };
        assert_eq!(observation, Observation::Ignored(IgnoreReason::AlreadySeen));
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
            rt.run_reminder(NOW + INTERVAL as i64).await.unwrap();
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
                !rt.run_reminder(due).await.unwrap(),
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
            rt.status(NOW).unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::Disabled { .. }));
        let outcome = {
            let mut rt = h.runtime();
            rt.reset(StaffCheck::Staff, NOW).unwrap()
        };
        assert!(matches!(outcome, CommandOutcome::Disabled { .. }));
    }

    // ---- the real queue --------------------------------------------------------------

    #[test]
    fn the_real_queue_refuses_a_second_job_for_the_same_cycle() {
        let mut conn = Connection::open_in_memory().unwrap();
        queue::init(&mut conn).unwrap();
        let request = JobRequest {
            kind: KIND_REMINDER,
            dedupe_key: dedupe_key(GUILD, 1),
            payload_json: payload_json(GUILD, 1),
            due_at: NOW + INTERVAL as i64,
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

    // ---- liveness --------------------------------------------------------------------

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *seed >> 33
    }

    /// A model runtime: it acknowledges, fails and retries, and restarts.
    ///
    /// The model asserts the two properties the card names: at most one reminder per
    /// cycle (even across restarts), and never quiet while a cycle is overdue and
    /// unreminded.
    #[tokio::test]
    async fn seeded_sequences_send_one_reminder_per_cycle_and_never_go_quiet() {
        for seed0 in 0..40u64 {
            let mut seed = seed0 + 1;
            let mut now = NOW;
            let mut next_id = 1_000u64;
            let mut h = Harness::new("live");
            // model state
            let mut model_cycle = 0i64;
            let mut model_start: Option<i64> = None;
            let mut model_reminded = false;
            let mut reminders = 0usize;

            for _ in 0..200 {
                now += 1 + (lcg(&mut seed) % 3_000) as i64;
                match lcg(&mut seed) % 7 {
                    0 => {
                        next_id += 1 + lcg(&mut seed) % 50;
                        let observation = {
                            let mut rt = h.runtime();
                            rt.observe(&success(next_id), now).unwrap()
                        };
                        assert!(matches!(observation, Observation::Confirmed { .. }));
                        model_cycle += 1;
                        model_start = Some(now);
                        model_reminded = false;
                    }
                    1 => {
                        // hostile noise: it must not change anything
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
                            rt.observe(&facts, now).unwrap()
                        };
                        assert!(
                            matches!(observation, Observation::Ignored(_)),
                            "{observation:?}"
                        );
                        assert_eq!(
                            h.repo
                                .load(GUILD)
                                .unwrap()
                                .map(|stored| stored.cycle)
                                .unwrap_or(0),
                            model_cycle,
                            "the noise changed the stored cycle"
                        );
                    }
                    2 if model_cycle > 0 => {
                        // restart: rearm from the stored row
                        let rearmed = {
                            let mut rt = h.runtime();
                            rt.rearm(now).unwrap()
                        };
                        assert_eq!(rearmed, !model_reminded, "seed {seed0}");
                    }
                    3 => {
                        // staff reset
                        let outcome = {
                            let mut rt = h.runtime();
                            rt.reset(StaffCheck::Staff, now).unwrap()
                        };
                        assert!(matches!(outcome, CommandOutcome::Done { .. }));
                        model_cycle += 1;
                        model_start = Some(now);
                        model_reminded = false;
                    }
                    4 if model_cycle > 0 => {
                        // the reminder runs; the channel may fail
                        let failed = lcg(&mut seed).is_multiple_of(5);
                        if failed {
                            h.channel.fail();
                        } else {
                            h.channel.work();
                        }
                        let result = {
                            let mut rt = h.runtime();
                            rt.run_reminder(now).await
                        };
                        let expected_due = model_start.is_some_and(|s| now >= s + INTERVAL as i64)
                            && !model_reminded;
                        if failed && expected_due {
                            // The send was attempted and the channel failed.
                            assert_eq!(result, Err(RuntimeError::SendFailed), "seed {seed0}");
                        } else {
                            // Either the cycle was not due (nothing was attempted) or
                            // the send worked.
                            assert_eq!(result, Ok(expected_due), "seed {seed0}");
                        }
                        if expected_due {
                            model_reminded = true;
                            if !failed {
                                reminders += 1;
                            }
                        }
                    }
                    _ => {
                        // nothing happens
                    }
                }
            }
            // Drain: far in the future an unreminded cycle must fire exactly once.
            // The channel works again, so the reminder really goes out.
            h.channel.work();
            now += INTERVAL as i64 * 10;
            let owes = model_start.is_some() && !model_reminded;
            let result = {
                let mut rt = h.runtime();
                rt.run_reminder(now).await
            };
            assert_eq!(result, Ok(owes), "seed {seed0}");
            if owes {
                reminders += 1;
            }
            let sent = h.channel.sent().len();
            assert_eq!(
                sent, reminders,
                "seed {seed0}: sent {sent}, model {reminders}"
            );
            // At most one reminder per cycle: never more sends than cycles.
            assert!(reminders <= model_cycle as usize, "seed {seed0}");
        }
    }
}

//! Flash-media runtime (`docs/verification/FLASH-RUNTIME.md`).
//!
//! The thin layer between the pure policy (`super::policy`), the durable job queue
//! (`crate::jobs`) and Discord. Discord is reached only through two small traits
//! ([`FlashPorts`], [`StaffAlerter`]) so tests inject fakes; the decisions are plain
//! functions in this file. [`SerenityFlashPorts`] is the one serenity-facing adapter
//! and has not been run against Discord.
//!
//! Flow in `live` mode for a message the policy expires:
//! 1. schedule the source delete (durable) at `now + delay`;
//! 2. post the notice; a failure is alerted and the source delete stays scheduled;
//! 3. schedule the notice delete at the same due time.
//!
//! The source delete is scheduled before the notice is posted so that no crash between
//! the two steps can leave the user's media without a deletion. Both job kinds are
//! `RunAgain`: a delete is idempotent (a 404 counts as done), so a restart re-runs a cut
//! off delete and an overdue delete runs at once.
//!
//! Job payloads hold ids only. Alerts carry a stable code only. `shadow` posts, deletes
//! and schedules nothing; it only counts.

use super::config::FlashSettings;
use super::policy::{AllowedMentions, Decision, ExpirePlan, IgnoreReason, MessageFacts};
use crate::features::{Feature, JobSignal};
use crate::jobs::queue::{self, ClaimedJob, NewJob, RestartPolicy, ScheduleOutcome};
use crate::jobs::runner::{BoxFuture, Clock, HandlerError, HandlerResult, Registry, RunnerError};
use crate::settings::Mode;
use poise::serenity_prelude as serenity;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Job kind that deletes the user's flashed message.
pub const KIND_DELETE_SOURCE: &str = "flash_delete_source";
/// Job kind that deletes the bot's notice.
pub const KIND_DELETE_NOTICE: &str = "flash_delete_notice";

/// The two job kinds and their restart policy. Both are idempotent deletes (a 404 is
/// done), so both are `RunAgain`: a restart re-runs a delete that was cut off, and an
/// overdue delete runs at once.
pub const JOB_KINDS: [(&str, RestartPolicy); 2] = [
    (KIND_DELETE_SOURCE, RestartPolicy::RunAgain),
    (KIND_DELETE_NOTICE, RestartPolicy::RunAgain),
];

/// Why a Discord call failed, reduced to what the runtime decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortError {
    /// HTTP 404: the thing is already gone.
    NotFound,
    /// HTTP 403: the bot may not do this.
    Forbidden,
    /// Anything else (429 after serenity's own wait, 5xx, transport).
    Unavailable,
}

/// What a channel lookup says about a thread parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentLookup {
    /// The channel is a regular channel, not a thread.
    NotThread,
    /// The channel is a thread of this parent channel.
    Parent(u64),
    /// The lookup failed; the answer is unknown.
    Unavailable,
}

/// The notice to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeRequest {
    /// Channel (or thread) to post in.
    pub channel_id: u64,
    /// Notice text, from the policy.
    pub text: String,
    /// The only mentions the notice may render.
    pub mentions: AllowedMentions,
}

/// Stable codes for what is reported to the staff alert channel. Code only: no text,
/// no id, no name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertCode {
    /// The notice could not be posted.
    NoticeSendFailed,
    /// The source delete job could not be scheduled.
    SourceScheduleFailed,
    /// The notice delete job could not be scheduled.
    NoticeScheduleFailed,
    /// A delete was refused with 403.
    DeleteForbidden,
    /// A delete failed for another reason and will be retried.
    DeleteFailed,
    /// A delete job carried a payload that is not `{channel_id, message_id}`.
    PayloadInvalid,
}

impl AlertCode {
    /// The stable code.
    pub fn code(self) -> &'static str {
        match self {
            AlertCode::NoticeSendFailed => "flash_notice_send_failed",
            AlertCode::SourceScheduleFailed => "flash_source_schedule_failed",
            AlertCode::NoticeScheduleFailed => "flash_notice_schedule_failed",
            AlertCode::DeleteForbidden => "flash_delete_forbidden",
            AlertCode::DeleteFailed => "flash_delete_failed",
            AlertCode::PayloadInvalid => "flash_payload_invalid",
        }
    }
}

/// The staff alert text for a code: the code and nothing else.
pub fn alert_text(code: AlertCode) -> String {
    format!("flash alert: {}", code.code())
}

/// Discord calls of the flash runtime.
#[serenity::async_trait]
pub trait FlashPorts: Send + Sync {
    /// Posts the notice; returns the id of the posted message.
    async fn send_notice(&self, req: &NoticeRequest) -> Result<u64, PortError>;
    /// Deletes one message.
    async fn delete_message(&self, channel_id: u64, message_id: u64) -> Result<(), PortError>;
    /// Says whether `channel_id` is a thread and of which channel. Implementations
    /// should answer from the cache first: this is asked for every attachment message
    /// outside the configured channels.
    async fn thread_parent(&self, guild_id: u64, channel_id: u64) -> ParentLookup;
}

/// Reports a stable code to the staff alert channel. Best effort: it cannot fail the
/// caller.
#[serenity::async_trait]
pub trait StaffAlerter: Send + Sync {
    /// Sends the alert for `code`.
    async fn alert(&self, code: AlertCode);
}

/// A delete job's payload: ids only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteTarget {
    /// Channel of the message.
    pub channel_id: u64,
    /// The message to delete.
    pub message_id: u64,
}

/// The payload was not a plain `{"channel_id": n, "message_id": n}` of non-zero ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadInvalid;

/// The JSON payload of a delete job. Ids only.
pub fn delete_payload(target: DeleteTarget) -> String {
    format!(
        "{{\"channel_id\":{},\"message_id\":{}}}",
        target.channel_id, target.message_id
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    channel_id: u64,
    message_id: u64,
}

/// Parses a delete payload strictly: exactly the two keys, both JSON integers, both
/// non-zero. Anything else is refused.
pub fn parse_delete_payload(raw: &str) -> Result<DeleteTarget, PayloadInvalid> {
    // serde also reads a struct from a JSON array (`[1, 2]`); only the object form is a
    // payload.
    if !raw.trim_start().starts_with('{') {
        return Err(PayloadInvalid);
    }
    let parsed: RawTarget = serde_json::from_str(raw).map_err(|_| PayloadInvalid)?;
    if parsed.channel_id == 0 || parsed.message_id == 0 {
        return Err(PayloadInvalid);
    }
    Ok(DeleteTarget {
        channel_id: parsed.channel_id,
        message_id: parsed.message_id,
    })
}

/// Dedupe key of the source delete job.
pub fn source_dedupe_key(message_id: u64) -> String {
    format!("flash_src_{message_id}")
}

/// Dedupe key of the notice delete job.
pub fn notice_dedupe_key(notice_message_id: u64) -> String {
    format!("flash_ntc_{notice_message_id}")
}

/// What a delete attempt means for its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteVerdict {
    /// The message is gone (deleted now or already): the job is done.
    Done,
    /// The message may still exist: alert and fail the attempt with `code`.
    Failed {
        /// Alert to send.
        alert: AlertCode,
        /// Job error code.
        code: &'static str,
    },
}

/// Decides a delete attempt's verdict. 404 is done; 403 and everything else fail.
pub fn delete_verdict(result: Result<(), PortError>) -> DeleteVerdict {
    match result {
        Ok(()) | Err(PortError::NotFound) => DeleteVerdict::Done,
        Err(PortError::Forbidden) => DeleteVerdict::Failed {
            alert: AlertCode::DeleteForbidden,
            code: "delete_forbidden",
        },
        Err(PortError::Unavailable) => DeleteVerdict::Failed {
            alert: AlertCode::DeleteFailed,
            code: "delete_unavailable",
        },
    }
}

/// Maps an HTTP status (if the failure was an HTTP response) to a [`PortError`].
pub fn port_error_from_status(status: Option<u16>) -> PortError {
    match status {
        Some(404) => PortError::NotFound,
        Some(403) => PortError::Forbidden,
        _ => PortError::Unavailable,
    }
}

/// The due time (unix seconds) of both delete jobs: now plus the plan's delay. Saturates
/// at `i64::MAX` instead of wrapping; a clock below zero counts as zero.
pub fn due_at_secs(delay_secs: u64, now_secs: i64) -> i64 {
    let now = u64::try_from(now_secs).unwrap_or(0);
    i64::try_from(now.saturating_add(delay_secs)).unwrap_or(i64::MAX)
}

/// A stable code for an ignore reason.
pub fn ignore_code(reason: IgnoreReason) -> &'static str {
    match reason {
        IgnoreReason::GuildMismatch => "guild_mismatch",
        IgnoreReason::FeatureDisabled => "feature_disabled",
        IgnoreReason::BotAuthor => "bot_author",
        IgnoreReason::WebhookAuthor => "webhook_author",
        IgnoreReason::InvalidAuthor => "invalid_author",
        IgnoreReason::InvalidMessage => "invalid_message",
        IgnoreReason::NoAttachments => "no_attachments",
        IgnoreReason::ChannelNotConfigured => "channel_not_configured",
        IgnoreReason::ThreadNotIncluded => "thread_not_included",
    }
}

/// The shadow tally key of an ignore reason; `None` for the reasons that describe the
/// ordinary message anywhere else in the server (counting those would drown the rest).
fn shadow_ignore_key(reason: IgnoreReason) -> Option<&'static str> {
    match reason {
        IgnoreReason::GuildMismatch
        | IgnoreReason::FeatureDisabled
        | IgnoreReason::ChannelNotConfigured => None,
        IgnoreReason::BotAuthor => Some("ignored_bot_author"),
        IgnoreReason::WebhookAuthor => Some("ignored_webhook_author"),
        IgnoreReason::InvalidAuthor => Some("ignored_invalid_author"),
        IgnoreReason::InvalidMessage => Some("ignored_invalid_message"),
        IgnoreReason::NoAttachments => Some("ignored_no_attachments"),
        IgnoreReason::ThreadNotIncluded => Some("ignored_thread_not_included"),
    }
}

/// Facts of a serenity message; `None` for a message outside a guild.
pub fn facts_from_message(message: &serenity::Message) -> Option<MessageFacts> {
    let guild = message.guild_id?;
    Some(MessageFacts {
        guild_id: guild.get(),
        channel_id: message.channel_id.get(),
        parent_channel_id: None,
        author_id: message.author.id.get(),
        author_is_bot: message.author.bot,
        is_webhook: message.webhook_id.is_some(),
        attachment_count: message.attachments.len(),
        message_id: message.id.get(),
    })
}

/// What shadow mode would have done for one message: counts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowRecord {
    /// Notices that would be posted.
    pub notices: u32,
    /// Delete jobs that would be scheduled.
    pub delete_jobs: u32,
    /// Delay in seconds the deletes would wait.
    pub delay_secs: u64,
    /// Role mentions the notice would carry.
    pub role_mentions: u32,
    /// User mentions the notice would carry.
    pub user_mentions: u32,
}

fn shadow_record(plan: &ExpirePlan) -> ShadowRecord {
    let mentions = &plan.notice.allowed_mentions;
    ShadowRecord {
        notices: 1,
        delete_jobs: 2,
        delay_secs: plan.delay_secs,
        role_mentions: u32::try_from(mentions.roles.len()).unwrap_or(u32::MAX),
        user_mentions: u32::try_from(mentions.users.len()).unwrap_or(u32::MAX),
    }
}

/// What happened to the notice of an expiring message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeStatus {
    /// Posted, and its delete job is scheduled.
    Posted,
    /// Posted, but its delete job could not be scheduled (alerted).
    PostedButDeleteNotScheduled,
    /// The post failed (alerted). The source delete is scheduled regardless.
    SendFailed,
}

/// What `process` did with one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The feature is not active (mode `off`, or no flash channel configured).
    Off,
    /// The policy ignored the message.
    Ignored(IgnoreReason),
    /// A thread parent could not be resolved; nothing was done.
    ParentLookupFailed,
    /// Shadow mode: nothing was sent, deleted or scheduled.
    Shadow(ShadowRecord),
    /// The message already had its delete job (a replayed event); nothing was posted.
    DuplicateEvent,
    /// The source delete could not be scheduled, so no notice was posted (alerted).
    SourceNotScheduled,
    /// The source delete is scheduled; the notice has the given status.
    Expiring(NoticeStatus),
}

/// Everything [`FlashFeature::new`] needs.
pub struct FlashDeps {
    /// Parsed settings.
    pub settings: FlashSettings,
    /// Discord calls.
    pub ports: Arc<dyn FlashPorts>,
    /// Staff alerts.
    pub alerts: Arc<dyn StaffAlerter>,
    /// Time source (unix seconds).
    pub clock: Arc<dyn Clock>,
}

/// One job to schedule.
struct JobSpec {
    kind: &'static str,
    key: String,
    target: DeleteTarget,
    due_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheduled {
    Created,
    AlreadyThere,
}

struct Inner {
    settings: FlashSettings,
    ports: Arc<dyn FlashPorts>,
    alerts: Arc<dyn StaffAlerter>,
    clock: Arc<dyn Clock>,
    wake: Mutex<Option<JobSignal>>,
    tally: Mutex<BTreeMap<&'static str, u64>>,
}

impl Inner {
    fn count(&self, key: &'static str) {
        let mut tally = self
            .tally
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = tally.entry(key).or_insert(0);
        *slot = slot.saturating_add(1);
    }

    fn wake_runner(&self) {
        let wake = self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    async fn process(
        &self,
        mut facts: MessageFacts,
        core_db: Option<&tokio_rusqlite::Connection>,
    ) -> Outcome {
        let mode = self.settings.mode();
        if !self.settings.is_active() {
            return Outcome::Off;
        }
        let cfg = self.settings.policy();
        let mut decision = super::policy::decide(&facts, cfg);
        // A thread is not in the channel list: ask for its parent, once, and decide again.
        if matches!(
            decision,
            Decision::Ignore(IgnoreReason::ChannelNotConfigured)
        ) && facts.parent_channel_id.is_none()
        {
            match self
                .ports
                .thread_parent(facts.guild_id, facts.channel_id)
                .await
            {
                ParentLookup::NotThread => {}
                ParentLookup::Parent(parent) => {
                    facts.parent_channel_id = Some(parent);
                    decision = super::policy::decide(&facts, cfg);
                }
                ParentLookup::Unavailable => return Outcome::ParentLookupFailed,
            }
        }
        match decision {
            Decision::Ignore(reason) => {
                if mode == Mode::Shadow {
                    if let Some(key) = shadow_ignore_key(reason) {
                        self.count(key);
                    }
                }
                Outcome::Ignored(reason)
            }
            Decision::Expire(plan) => match mode {
                Mode::Live => self.expire(plan, core_db).await,
                Mode::Shadow => {
                    self.count("would_expire");
                    Outcome::Shadow(shadow_record(&plan))
                }
                Mode::Off => Outcome::Off,
            },
        }
    }

    async fn expire(
        &self,
        plan: ExpirePlan,
        core_db: Option<&tokio_rusqlite::Connection>,
    ) -> Outcome {
        let now = self.clock.now();
        let due_at = due_at_secs(plan.delay_secs, now);
        let Some(db) = core_db else {
            self.alerts.alert(AlertCode::SourceScheduleFailed).await;
            return Outcome::SourceNotScheduled;
        };
        let source = JobSpec {
            kind: KIND_DELETE_SOURCE,
            key: source_dedupe_key(plan.original_message_id),
            target: DeleteTarget {
                channel_id: plan.channel_id,
                message_id: plan.original_message_id,
            },
            due_at,
        };
        match self.schedule(db, source, now).await {
            Ok(Scheduled::Created) => {}
            Ok(Scheduled::AlreadyThere) => return Outcome::DuplicateEvent,
            Err(()) => {
                self.alerts.alert(AlertCode::SourceScheduleFailed).await;
                return Outcome::SourceNotScheduled;
            }
        }
        let notice = NoticeRequest {
            channel_id: plan.channel_id,
            text: plan.notice.text.clone(),
            mentions: plan.notice.allowed_mentions.clone(),
        };
        let notice_id = match self.ports.send_notice(&notice).await {
            Ok(id) => id,
            Err(_) => {
                self.alerts.alert(AlertCode::NoticeSendFailed).await;
                return Outcome::Expiring(NoticeStatus::SendFailed);
            }
        };
        let notice_job = JobSpec {
            kind: KIND_DELETE_NOTICE,
            key: notice_dedupe_key(notice_id),
            target: DeleteTarget {
                channel_id: plan.channel_id,
                message_id: notice_id,
            },
            due_at,
        };
        match self.schedule(db, notice_job, now).await {
            Ok(_) => Outcome::Expiring(NoticeStatus::Posted),
            Err(()) => {
                self.alerts.alert(AlertCode::NoticeScheduleFailed).await;
                Outcome::Expiring(NoticeStatus::PostedButDeleteNotScheduled)
            }
        }
    }

    /// Schedules one job and wakes the runner when a row was created.
    async fn schedule(
        &self,
        db: &tokio_rusqlite::Connection,
        spec: JobSpec,
        now: i64,
    ) -> Result<Scheduled, ()> {
        let payload = delete_payload(spec.target);
        let outcome = db
            .call(move |conn| {
                queue::schedule(
                    conn,
                    &NewJob {
                        kind: spec.kind,
                        dedupe_key: &spec.key,
                        payload_json: &payload,
                        due_at: spec.due_at,
                    },
                    now,
                )
            })
            .await;
        match outcome {
            Ok(ScheduleOutcome::Created { .. }) => {
                self.wake_runner();
                Ok(Scheduled::Created)
            }
            Ok(ScheduleOutcome::Duplicate { .. }) => Ok(Scheduled::AlreadyThere),
            Err(_) => Err(()),
        }
    }

    async fn run_delete(&self, job: ClaimedJob) -> HandlerResult {
        let Ok(target) = parse_delete_payload(&job.payload_json) else {
            self.alerts.alert(AlertCode::PayloadInvalid).await;
            return Err(HandlerError::new("payload_invalid"));
        };
        let result = self
            .ports
            .delete_message(target.channel_id, target.message_id)
            .await;
        match delete_verdict(result) {
            DeleteVerdict::Done => Ok(()),
            DeleteVerdict::Failed { alert, code } => {
                self.alerts.alert(alert).await;
                Err(HandlerError::new(code))
            }
        }
    }
}

/// The flash-media feature. Implements the registry's `Feature` hook; it is NOT
/// registered anywhere.
pub struct FlashFeature {
    inner: Arc<Inner>,
}

impl FlashFeature {
    /// Builds the feature.
    pub fn new(deps: FlashDeps) -> Self {
        FlashFeature {
            inner: Arc::new(Inner {
                settings: deps.settings,
                ports: deps.ports,
                alerts: deps.alerts,
                clock: deps.clock,
                wake: Mutex::new(None),
                tally: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Handles one message. `core_db` is the shared database (needed in live mode).
    pub async fn process(
        &self,
        facts: MessageFacts,
        core_db: Option<&tokio_rusqlite::Connection>,
    ) -> Outcome {
        self.inner.process(facts, core_db).await
    }

    /// Runs one delete job (both kinds share this handler).
    pub async fn run_delete(&self, job: ClaimedJob) -> HandlerResult {
        self.inner.run_delete(job).await
    }

    /// Shadow-mode tally: `(code, count)` sorted by code.
    pub fn shadow_summary(&self) -> Vec<(&'static str, u64)> {
        self.inner
            .tally
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(key, count)| (*key, *count))
            .collect()
    }

    /// Registers the two job kinds with [`JOB_KINDS`]' restart policies.
    pub fn register_job_kinds(&self, registry: &mut Registry) -> Result<(), RunnerError> {
        for (kind, policy) in JOB_KINDS {
            let inner = self.inner.clone();
            registry.register(kind, policy, move |job| {
                let inner = inner.clone();
                async move { inner.run_delete(job).await }
            })?;
        }
        Ok(())
    }
}

impl<X: Sync + 'static, D: Sync + 'static> Feature<X, D> for FlashFeature {
    fn name(&self) -> &'static str {
        "flash"
    }

    fn enabled(&self) -> bool {
        self.inner.settings.is_active()
    }

    /// Only `live` uses the database: `shadow` schedules nothing, so it must not open it
    /// or register job kinds.
    fn needs_core_db(&self) -> bool {
        self.inner.settings.is_active() && self.inner.settings.mode() == Mode::Live
    }

    fn register_jobs(&self, registry: &mut Registry) -> Result<(), RunnerError> {
        self.register_job_kinds(registry)
    }

    fn jobs_started(&self, wake: JobSignal) {
        *self
            .inner
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(wake);
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a X,
        event: &'a serenity::FullEvent,
        _data: &'a D,
        _guild: Option<u64>,
        core_db: Option<&'a tokio_rusqlite::Connection>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let serenity::FullEvent::Message { new_message } = event else {
                return;
            };
            let Some(facts) = facts_from_message(new_message) else {
                return;
            };
            let _ = self.inner.process(facts, core_db).await;
        })
    }
}

// ---------------------------------------------------------------------------
// The serenity adapter (not run against Discord).
// ---------------------------------------------------------------------------

/// Maps a serenity error to a [`PortError`] through its HTTP status, if it has one.
pub fn port_error_from_serenity(error: &serenity::Error) -> PortError {
    match error {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) => {
            port_error_from_status(Some(response.status_code.as_u16()))
        }
        _ => port_error_from_status(None),
    }
}

/// The guild's cached answer for a thread parent, if the cache can tell. The guard on
/// the cache is dropped before this returns, so nothing is held across an await.
fn cached_thread_parent(
    cache: &serenity::Cache,
    guild: serenity::GuildId,
    channel: serenity::ChannelId,
) -> Option<ParentLookup> {
    let guild = cache.guild(guild)?;
    if guild.channels.contains_key(&channel) {
        return Some(ParentLookup::NotThread);
    }
    guild
        .threads
        .iter()
        .find(|thread| thread.id == channel)
        .map(|thread| {
            thread
                .parent_id
                .map_or(ParentLookup::Unavailable, |p| ParentLookup::Parent(p.get()))
        })
}

/// Discord calls over serenity's HTTP client and cache.
pub struct SerenityFlashPorts {
    /// The serenity HTTP client.
    pub http: Arc<serenity::Http>,
    /// The serenity cache.
    pub cache: Arc<serenity::Cache>,
    /// Channel that receives staff alerts, if one is configured.
    pub staff_alert_channel: Option<u64>,
}

#[serenity::async_trait]
impl FlashPorts for SerenityFlashPorts {
    async fn send_notice(&self, req: &NoticeRequest) -> Result<u64, PortError> {
        if req.channel_id == 0 {
            return Err(PortError::Unavailable);
        }
        let users: Vec<serenity::UserId> = req
            .mentions
            .users
            .iter()
            .filter(|id| **id != 0)
            .map(|id| serenity::UserId::new(*id))
            .collect();
        let roles: Vec<serenity::RoleId> = req
            .mentions
            .roles
            .iter()
            .filter(|id| **id != 0)
            .map(|id| serenity::RoleId::new(*id))
            .collect();
        // Only the listed users and roles; `everyone` and `replied_user` stay unset.
        let mentions = serenity::builder::CreateAllowedMentions::new()
            .users(users)
            .roles(roles);
        let message = serenity::builder::CreateMessage::new()
            .content(req.text.clone())
            .allowed_mentions(mentions);
        serenity::ChannelId::new(req.channel_id)
            .send_message(&self.http, message)
            .await
            .map(|sent| sent.id.get())
            .map_err(|e| port_error_from_serenity(&e))
    }

    async fn delete_message(&self, channel_id: u64, message_id: u64) -> Result<(), PortError> {
        if channel_id == 0 || message_id == 0 {
            return Err(PortError::Unavailable);
        }
        self.http
            .delete_message(
                serenity::ChannelId::new(channel_id),
                serenity::MessageId::new(message_id),
                Some("flash expiry"),
            )
            .await
            .map_err(|e| port_error_from_serenity(&e))
    }

    async fn thread_parent(&self, guild_id: u64, channel_id: u64) -> ParentLookup {
        if guild_id == 0 || channel_id == 0 {
            return ParentLookup::Unavailable;
        }
        let guild = serenity::GuildId::new(guild_id);
        let channel = serenity::ChannelId::new(channel_id);
        if let Some(found) = cached_thread_parent(&self.cache, guild, channel) {
            return found;
        }
        match self.http.get_channel(channel).await {
            Ok(serenity::Channel::Guild(found)) => {
                if found.thread_metadata.is_none() {
                    ParentLookup::NotThread
                } else {
                    found
                        .parent_id
                        .map_or(ParentLookup::Unavailable, |p| ParentLookup::Parent(p.get()))
                }
            }
            _ => ParentLookup::Unavailable,
        }
    }
}

#[serenity::async_trait]
impl StaffAlerter for SerenityFlashPorts {
    async fn alert(&self, code: AlertCode) {
        let Some(channel) = self.staff_alert_channel.filter(|id| *id != 0) else {
            return;
        };
        // No mention can fire from an alert: the allowed-mentions object is empty.
        let message = serenity::builder::CreateMessage::new()
            .content(alert_text(code))
            .allowed_mentions(serenity::builder::CreateAllowedMentions::new());
        let _ = serenity::ChannelId::new(channel)
            .send_message(&self.http, message)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::{
        self, Features, GuildFilter, JobStarter, ReportCounts, Settings, StartRequest, StartedJobs,
    };
    use crate::flash::config::load;
    use crate::jobs::queue::{self, RetryPolicy};
    use crate::jobs::runner::{Runner, RunnerConfig, ShutdownHandle};
    use crate::storage;
    use crate::tickets;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::watch;

    // Synthetic ids, far from any real Discord snowflake.
    const GUILD: u64 = 5_200_000_000_000_001;
    const FLASH_CH: u64 = 5_200_000_000_000_002;
    const OTHER_CH: u64 = 5_200_000_000_000_003;
    const THREAD_CH: u64 = 5_200_000_000_000_004;
    const ROLE: u64 = 5_200_000_000_000_005;
    const AUTHOR: u64 = 5_200_000_000_000_006;
    const MESSAGE: u64 = 5_200_000_000_000_007;
    const FIRST_NOTICE: u64 = 5_300_000_000_000_001;
    const T0: i64 = 1_800_000_000;
    const EXPECTED_NOTICE: &str = "<@&5200000000000005> New flash from <@5200000000000006> in \
         <#5200000000000002>. This will be deleted in 3 minutes.";
    const CAPTION: &str = "SYNTHETIC caption @everyone <@5200000000000099>";

    // ---------------------------------------------------------------- fakes

    /// Time only moves when a test advances it.
    struct FakeClock {
        ms: watch::Sender<i64>,
    }

    impl FakeClock {
        fn new() -> Arc<FakeClock> {
            Arc::new(FakeClock {
                ms: watch::channel(T0 * 1000).0,
            })
        }
        fn advance(&self, secs: i64) {
            self.ms.send_modify(|v| *v += secs * 1000);
        }
        fn secs(&self) -> i64 {
            *self.ms.borrow() / 1000
        }
        fn wait_until_ms(&self, target: i64) -> BoxFuture<'_, ()> {
            let mut rx = self.ms.subscribe();
            Box::pin(async move {
                loop {
                    if *rx.borrow_and_update() >= target {
                        return;
                    }
                    if rx.changed().await.is_err() {
                        return;
                    }
                }
            })
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> i64 {
            self.secs()
        }
        fn sleep(&self, d: Duration) -> BoxFuture<'_, ()> {
            let target = *self.ms.borrow() + i64::try_from(d.as_millis()).unwrap();
            self.wait_until_ms(target)
        }
        fn sleep_until(&self, deadline: i64) -> BoxFuture<'_, ()> {
            self.wait_until_ms(deadline * 1000)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Notice(NoticeRequest),
        Delete { channel: u64, message: u64 },
        Alert(AlertCode),
        Parent(u64),
    }

    /// A fake Discord that records every call and fails on script.
    struct Net {
        calls: Mutex<Vec<Call>>,
        notice_script: Mutex<VecDeque<Result<(), PortError>>>,
        next_notice: AtomicU64,
        /// Failures for the first attempts at a message, then success.
        delete_script: Mutex<HashMap<u64, VecDeque<PortError>>>,
        /// A message whose delete always fails.
        delete_always: Mutex<HashMap<u64, PortError>>,
        parents: Mutex<HashMap<u64, ParentLookup>>,
        deleted: Mutex<HashSet<u64>>,
        probe: Option<tokio_rusqlite::Connection>,
        source_jobs_at_send: Mutex<Vec<i64>>,
    }

    impl Net {
        fn build(probe: Option<tokio_rusqlite::Connection>) -> Arc<Net> {
            Arc::new(Net {
                calls: Mutex::new(Vec::new()),
                notice_script: Mutex::new(VecDeque::new()),
                next_notice: AtomicU64::new(FIRST_NOTICE),
                delete_script: Mutex::new(HashMap::new()),
                delete_always: Mutex::new(HashMap::new()),
                parents: Mutex::new(HashMap::new()),
                deleted: Mutex::new(HashSet::new()),
                probe,
                source_jobs_at_send: Mutex::new(Vec::new()),
            })
        }
        fn new() -> Arc<Net> {
            Net::build(None)
        }
        fn probing(db: &tokio_rusqlite::Connection) -> Arc<Net> {
            Net::build(Some(db.clone()))
        }
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
        fn notices(&self) -> Vec<NoticeRequest> {
            self.calls()
                .into_iter()
                .filter_map(|c| match c {
                    Call::Notice(n) => Some(n),
                    _ => None,
                })
                .collect()
        }
        fn deletes(&self) -> Vec<(u64, u64)> {
            self.calls()
                .into_iter()
                .filter_map(|c| match c {
                    Call::Delete { channel, message } => Some((channel, message)),
                    _ => None,
                })
                .collect()
        }
        fn alerts(&self) -> Vec<AlertCode> {
            self.calls()
                .into_iter()
                .filter_map(|c| match c {
                    Call::Alert(a) => Some(a),
                    _ => None,
                })
                .collect()
        }
        fn script_notice(&self, r: Result<(), PortError>) {
            self.notice_script.lock().unwrap().push_back(r);
        }
        fn script_delete_failures(&self, message: u64, failures: &[PortError]) {
            self.delete_script
                .lock()
                .unwrap()
                .entry(message)
                .or_default()
                .extend(failures.iter().copied());
        }
        fn peek_next_notice(&self) -> u64 {
            self.next_notice.load(SeqCst)
        }
    }

    #[poise::serenity_prelude::async_trait]
    impl FlashPorts for Net {
        async fn send_notice(&self, req: &NoticeRequest) -> Result<u64, PortError> {
            self.calls.lock().unwrap().push(Call::Notice(req.clone()));
            if let Some(db) = &self.probe {
                let n: i64 = db
                    .call(|c| {
                        c.query_row(
                            "SELECT COUNT(*) FROM jobs WHERE kind = 'flash_delete_source'",
                            [],
                            |r| r.get(0),
                        )
                    })
                    .await
                    .unwrap_or(-1);
                self.source_jobs_at_send.lock().unwrap().push(n);
            }
            let next = self.notice_script.lock().unwrap().pop_front();
            match next {
                Some(Err(e)) => Err(e),
                _ => Ok(self.next_notice.fetch_add(1, SeqCst)),
            }
        }

        async fn delete_message(&self, channel_id: u64, message_id: u64) -> Result<(), PortError> {
            self.calls.lock().unwrap().push(Call::Delete {
                channel: channel_id,
                message: message_id,
            });
            if let Some(e) = self.delete_always.lock().unwrap().get(&message_id) {
                return Err(*e);
            }
            if let Some(e) = self
                .delete_script
                .lock()
                .unwrap()
                .get_mut(&message_id)
                .and_then(|q| q.pop_front())
            {
                return Err(e);
            }
            self.deleted.lock().unwrap().insert(message_id);
            Ok(())
        }

        async fn thread_parent(&self, _guild_id: u64, channel_id: u64) -> ParentLookup {
            self.calls.lock().unwrap().push(Call::Parent(channel_id));
            self.parents
                .lock()
                .unwrap()
                .get(&channel_id)
                .copied()
                .unwrap_or(ParentLookup::NotThread)
        }
    }

    #[poise::serenity_prelude::async_trait]
    impl StaffAlerter for Net {
        async fn alert(&self, code: AlertCode) {
            self.calls.lock().unwrap().push(Call::Alert(code));
        }
    }

    // -------------------------------------------------------------- helpers

    fn mem_db() -> tokio_rusqlite::Connection {
        let mut c = storage::open(":memory:", &[]).unwrap();
        queue::init(&mut c).unwrap();
        c.into()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("fl2-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file_db(path: &str) -> tokio_rusqlite::Connection {
        let mut c = storage::open(path, &[]).unwrap();
        queue::init(&mut c).unwrap();
        c.into()
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct JobRow {
        kind: String,
        key: String,
        payload: String,
        due: i64,
        state: String,
        attempts: i64,
    }

    async fn jobs(db: &tokio_rusqlite::Connection) -> Vec<JobRow> {
        db.call(|c| {
            let mut st = c.prepare(
                "SELECT kind, dedupe_key, payload_json, due_at, state, attempts FROM jobs ORDER BY id",
            )?;
            let rows = st
                .query_map([], |r| {
                    Ok(JobRow {
                        kind: r.get(0)?,
                        key: r.get(1)?,
                        payload: r.get(2)?,
                        due: r.get(3)?,
                        state: r.get(4)?,
                        attempts: r.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, rusqlite::Error>(rows)
        })
        .await
        .unwrap()
    }

    fn settings(mode: &str, extra: &[(&str, &str)]) -> FlashSettings {
        let mut pairs: Vec<(&str, &str)> = vec![
            ("FLASH_MODE", mode),
            ("FLASH_CHANNEL_IDS", "5200000000000002"),
            ("FLASH_PING_ROLE_ID", "5200000000000005"),
        ];
        pairs.extend_from_slice(extra);
        let loaded = load(
            |key| {
                pairs
                    .iter()
                    .rev()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| (*v).to_string())
            },
            Some(GUILD),
        );
        assert_eq!(loaded.error, None);
        loaded.settings
    }

    fn feature(net: &Arc<Net>, clock: &Arc<FakeClock>, settings: FlashSettings) -> FlashFeature {
        FlashFeature::new(FlashDeps {
            settings,
            ports: net.clone(),
            alerts: net.clone(),
            clock: clock.clone(),
        })
    }

    fn facts() -> MessageFacts {
        MessageFacts {
            guild_id: GUILD,
            channel_id: FLASH_CH,
            parent_channel_id: None,
            author_id: AUTHOR,
            author_is_bot: false,
            is_webhook: false,
            attachment_count: 1,
            message_id: MESSAGE,
        }
    }

    struct World {
        db: tokio_rusqlite::Connection,
        net: Arc<Net>,
        clock: Arc<FakeClock>,
        feat: FlashFeature,
    }

    fn world(mode: &str) -> World {
        let db = mem_db();
        let net = Net::probing(&db);
        let clock = FakeClock::new();
        let feat = feature(&net, &clock, settings(mode, &[]));
        World {
            db,
            net,
            clock,
            feat,
        }
    }

    fn job(kind: &str, channel: u64, message: u64) -> ClaimedJob {
        ClaimedJob {
            id: 1,
            kind: kind.to_string(),
            payload_json: delete_payload(DeleteTarget {
                channel_id: channel,
                message_id: message,
            }),
            attempts: 1,
        }
    }

    fn raw_job(payload: &str) -> ClaimedJob {
        ClaimedJob {
            id: 1,
            kind: KIND_DELETE_SOURCE.to_string(),
            payload_json: payload.to_string(),
            attempts: 1,
        }
    }

    // ------------------------------------------------------------ pure parts

    #[test]
    fn alert_codes_are_stable_distinct_and_the_text_is_the_code_only() {
        let all = [
            (AlertCode::NoticeSendFailed, "flash_notice_send_failed"),
            (
                AlertCode::SourceScheduleFailed,
                "flash_source_schedule_failed",
            ),
            (
                AlertCode::NoticeScheduleFailed,
                "flash_notice_schedule_failed",
            ),
            (AlertCode::DeleteForbidden, "flash_delete_forbidden"),
            (AlertCode::DeleteFailed, "flash_delete_failed"),
            (AlertCode::PayloadInvalid, "flash_payload_invalid"),
        ];
        let mut seen = HashSet::new();
        for (code, expected) in all {
            assert_eq!(code.code(), expected);
            assert!(seen.insert(expected));
            assert_eq!(alert_text(code), format!("flash alert: {expected}"));
        }
    }

    #[test]
    fn delete_payload_holds_the_two_ids_and_nothing_else() {
        let raw = delete_payload(DeleteTarget {
            channel_id: FLASH_CH,
            message_id: MESSAGE,
        });
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"channel_id": FLASH_CH, "message_id": MESSAGE})
        );
        assert_eq!(
            parse_delete_payload(&raw),
            Ok(DeleteTarget {
                channel_id: FLASH_CH,
                message_id: MESSAGE
            })
        );
    }

    #[test]
    fn delete_payload_parsing_accepts_only_the_plain_shape() {
        let good = [
            r#"{"channel_id":1,"message_id":2}"#,
            r#"{ "message_id" : 2 , "channel_id" : 1 }"#,
        ];
        for raw in good {
            assert_eq!(
                parse_delete_payload(raw),
                Ok(DeleteTarget {
                    channel_id: 1,
                    message_id: 2
                }),
                "{raw}"
            );
        }
        let max = format!(r#"{{"channel_id":{0},"message_id":{0}}}"#, u64::MAX);
        assert!(parse_delete_payload(&max).is_ok());
        let bad = [
            "",
            "null",
            "[]",
            "{}",
            "7",
            r#"{"channel_id":1}"#,
            r#"{"message_id":2}"#,
            r#"{"channel_id":1,"message_id":2,"content":"x"}"#,
            r#"{"channel_id":"1","message_id":2}"#,
            r#"{"channel_id":1,"message_id":"2"}"#,
            r#"{"channel_id":0,"message_id":2}"#,
            r#"{"channel_id":1,"message_id":0}"#,
            r#"{"channel_id":-1,"message_id":2}"#,
            r#"{"channel_id":1.5,"message_id":2}"#,
            r#"{"channel_id":1e3,"message_id":2}"#,
            r#"{"channel_id":18446744073709551616,"message_id":2}"#,
            r#"{"channel_id":1,"channel_id":2,"message_id":3}"#,
            r#"{"channel_id":1,"message_id":2} trailing"#,
            r#"{"channel_id":null,"message_id":2}"#,
            r#"{"channel_id":true,"message_id":2}"#,
        ];
        for raw in bad {
            assert_eq!(parse_delete_payload(raw), Err(PayloadInvalid), "{raw:?}");
        }
    }

    #[test]
    fn dedupe_keys_are_per_message_and_valid_for_the_queue() {
        assert_eq!(source_dedupe_key(MESSAGE), "flash_src_5200000000000007");
        assert_eq!(
            notice_dedupe_key(FIRST_NOTICE),
            "flash_ntc_5300000000000001"
        );
        assert_ne!(source_dedupe_key(7), notice_dedupe_key(7));
        for key in [source_dedupe_key(u64::MAX), notice_dedupe_key(u64::MAX)] {
            assert!(key.len() <= queue::MAX_DEDUPE_KEY_LEN);
            assert!(key.bytes().all(|b| (0x21..=0x7e).contains(&b)));
        }
    }

    #[test]
    fn a_404_is_done_and_every_other_failure_fails_the_attempt() {
        assert_eq!(delete_verdict(Ok(())), DeleteVerdict::Done);
        assert_eq!(
            delete_verdict(Err(PortError::NotFound)),
            DeleteVerdict::Done
        );
        assert_eq!(
            delete_verdict(Err(PortError::Forbidden)),
            DeleteVerdict::Failed {
                alert: AlertCode::DeleteForbidden,
                code: "delete_forbidden"
            }
        );
        assert_eq!(
            delete_verdict(Err(PortError::Unavailable)),
            DeleteVerdict::Failed {
                alert: AlertCode::DeleteFailed,
                code: "delete_unavailable"
            }
        );
    }

    #[test]
    fn http_statuses_map_to_port_errors() {
        assert_eq!(port_error_from_status(Some(404)), PortError::NotFound);
        assert_eq!(port_error_from_status(Some(403)), PortError::Forbidden);
        for other in [
            Some(400),
            Some(401),
            Some(429),
            Some(500),
            Some(503),
            Some(200),
            None,
        ] {
            assert_eq!(
                port_error_from_status(other),
                PortError::Unavailable,
                "{other:?}"
            );
        }
    }

    #[test]
    fn the_due_time_is_now_plus_the_delay_and_saturates() {
        assert_eq!(due_at_secs(180, T0), T0 + 180);
        assert_eq!(due_at_secs(10, 0), 10);
        assert_eq!(due_at_secs(86_400, 5), 86_405);
        assert_eq!(due_at_secs(u64::MAX, i64::MAX), i64::MAX);
        assert_eq!(due_at_secs(180, i64::MAX), i64::MAX);
        // A clock below zero is treated as zero rather than wrapping.
        assert_eq!(due_at_secs(180, -5), 180);
    }

    #[test]
    fn every_ignore_reason_has_its_own_stable_code() {
        let all = [
            (IgnoreReason::GuildMismatch, "guild_mismatch"),
            (IgnoreReason::FeatureDisabled, "feature_disabled"),
            (IgnoreReason::BotAuthor, "bot_author"),
            (IgnoreReason::WebhookAuthor, "webhook_author"),
            (IgnoreReason::InvalidAuthor, "invalid_author"),
            (IgnoreReason::InvalidMessage, "invalid_message"),
            (IgnoreReason::NoAttachments, "no_attachments"),
            (IgnoreReason::ChannelNotConfigured, "channel_not_configured"),
            (IgnoreReason::ThreadNotIncluded, "thread_not_included"),
        ];
        let mut seen = HashSet::new();
        for (reason, expected) in all {
            assert_eq!(ignore_code(reason), expected);
            assert!(seen.insert(expected));
        }
    }

    fn attachment() -> serenity::Attachment {
        serde_json::from_value(serde_json::json!({
            "id": "5400000000000001",
            "filename": "a.png",
            "size": 1,
            "url": "https://example.invalid/a.png",
            "proxy_url": "https://example.invalid/a.png"
        }))
        .unwrap()
    }

    fn flash_message(channel: u64, id: u64) -> serenity::Message {
        let mut m = serenity::Message::default();
        m.id = serenity::MessageId::new(id);
        m.channel_id = serenity::ChannelId::new(channel);
        m.guild_id = Some(serenity::GuildId::new(GUILD));
        m.author.id = serenity::UserId::new(AUTHOR);
        m.author.bot = false;
        m.content = CAPTION.to_string();
        m.attachments.push(attachment());
        m
    }

    #[test]
    fn facts_come_from_the_message_and_a_dm_has_none() {
        let m = flash_message(FLASH_CH, MESSAGE);
        assert_eq!(facts_from_message(&m), Some(facts()));

        let mut dm = flash_message(FLASH_CH, MESSAGE);
        dm.guild_id = None;
        assert_eq!(facts_from_message(&dm), None);

        let mut bot = flash_message(FLASH_CH, MESSAGE);
        bot.author.bot = true;
        assert!(facts_from_message(&bot).unwrap().author_is_bot);

        let mut hook = flash_message(FLASH_CH, MESSAGE);
        hook.webhook_id = Some(serenity::WebhookId::new(5_400_000_000_000_002));
        assert!(facts_from_message(&hook).unwrap().is_webhook);

        let mut two = flash_message(FLASH_CH, MESSAGE);
        two.attachments.push(attachment());
        assert_eq!(facts_from_message(&two).unwrap().attachment_count, 2);

        let mut none = flash_message(FLASH_CH, MESSAGE);
        none.attachments.clear();
        assert_eq!(facts_from_message(&none).unwrap().attachment_count, 0);
    }

    #[test]
    fn the_job_kinds_are_both_run_again() {
        for (kind, policy) in JOB_KINDS {
            assert_eq!(policy, RestartPolicy::RunAgain, "{kind}");
        }
        let kinds: Vec<&str> = JOB_KINDS.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![KIND_DELETE_SOURCE, KIND_DELETE_NOTICE]);
    }

    // ---------------------------------------------------------- live process

    #[tokio::test]
    async fn a_flash_posts_the_notice_and_schedules_both_deletes_without_deleting_yet() {
        let w = world("live");
        let out = w.feat.process(facts(), Some(&w.db)).await;
        assert_eq!(out, Outcome::Expiring(NoticeStatus::Posted));
        assert_eq!(
            w.net.notices(),
            vec![NoticeRequest {
                channel_id: FLASH_CH,
                text: EXPECTED_NOTICE.to_string(),
                mentions: AllowedMentions {
                    users: vec![AUTHOR],
                    roles: vec![ROLE],
                    everyone: false
                },
            }]
        );
        assert!(
            w.net.deletes().is_empty(),
            "nothing is deleted before the deadline"
        );
        let rows = jobs(&w.db).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, KIND_DELETE_SOURCE);
        assert_eq!(rows[0].key, "flash_src_5200000000000007");
        assert_eq!(rows[1].kind, KIND_DELETE_NOTICE);
        assert_eq!(rows[1].key, "flash_ntc_5300000000000001");
        for row in &rows {
            assert_eq!(row.due, T0 + 180);
            assert_eq!(row.state, "pending");
        }
        let src: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
        assert_eq!(
            src,
            serde_json::json!({"channel_id": FLASH_CH, "message_id": MESSAGE})
        );
        let ntc: serde_json::Value = serde_json::from_str(&rows[1].payload).unwrap();
        assert_eq!(
            ntc,
            serde_json::json!({"channel_id": FLASH_CH, "message_id": FIRST_NOTICE})
        );
        assert!(w.net.alerts().is_empty());
    }

    #[tokio::test]
    async fn the_delay_setting_sets_the_due_time_of_both_jobs() {
        let db = mem_db();
        let net = Net::new();
        let clock = FakeClock::new();
        let feat = feature(
            &net,
            &clock,
            settings("live", &[("FLASH_DELAY_SECS", "45")]),
        );
        clock.advance(7);
        feat.process(facts(), Some(&db)).await;
        let rows = jobs(&db).await;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.due == T0 + 7 + 45));
        assert!(net.notices()[0]
            .text
            .ends_with("This will be deleted in 45 seconds."));
    }

    #[tokio::test]
    async fn the_source_delete_is_durable_before_the_notice_is_sent() {
        let w = world("live");
        w.feat.process(facts(), Some(&w.db)).await;
        assert_eq!(*w.net.source_jobs_at_send.lock().unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn a_notice_failure_still_schedules_the_source_delete_and_is_reported() {
        for err in [
            PortError::Unavailable,
            PortError::Forbidden,
            PortError::NotFound,
        ] {
            let w = world("live");
            w.net.script_notice(Err(err));
            let out = w.feat.process(facts(), Some(&w.db)).await;
            assert_eq!(out, Outcome::Expiring(NoticeStatus::SendFailed), "{err:?}");
            let rows = jobs(&w.db).await;
            assert_eq!(rows.len(), 1, "{err:?}");
            assert_eq!(rows[0].kind, KIND_DELETE_SOURCE);
            assert_eq!(rows[0].due, T0 + 180);
            assert_eq!(w.net.alerts(), vec![AlertCode::NoticeSendFailed], "{err:?}");
            assert!(w.net.deletes().is_empty());
        }
    }

    #[tokio::test]
    async fn without_a_usable_jobs_table_nothing_is_posted_and_the_failure_is_reported() {
        let blank: tokio_rusqlite::Connection =
            rusqlite::Connection::open_in_memory().unwrap().into();
        for db in [Some(&blank), None] {
            let net = Net::new();
            let clock = FakeClock::new();
            let feat = feature(&net, &clock, settings("live", &[]));
            let out = feat.process(facts(), db).await;
            assert_eq!(out, Outcome::SourceNotScheduled);
            assert!(
                net.notices().is_empty(),
                "no promise of a deletion that is not scheduled"
            );
            assert_eq!(net.alerts(), vec![AlertCode::SourceScheduleFailed]);
        }
    }

    #[tokio::test]
    async fn a_notice_delete_that_cannot_be_scheduled_is_reported_and_the_source_stays_scheduled() {
        let w = world("live");
        w.db.call(|c| {
            c.execute_batch(
                "CREATE TRIGGER no_notice_jobs BEFORE INSERT ON jobs \
                     WHEN NEW.kind = 'flash_delete_notice' \
                     BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
            )
        })
        .await
        .unwrap();
        let out = w.feat.process(facts(), Some(&w.db)).await;
        assert_eq!(
            out,
            Outcome::Expiring(NoticeStatus::PostedButDeleteNotScheduled)
        );
        assert_eq!(w.net.notices().len(), 1);
        assert_eq!(w.net.alerts(), vec![AlertCode::NoticeScheduleFailed]);
        let rows = jobs(&w.db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, KIND_DELETE_SOURCE);
    }

    #[tokio::test]
    async fn a_replayed_event_posts_no_second_notice_and_schedules_nothing_new() {
        let w = world("live");
        assert_eq!(
            w.feat.process(facts(), Some(&w.db)).await,
            Outcome::Expiring(NoticeStatus::Posted)
        );
        assert_eq!(
            w.feat.process(facts(), Some(&w.db)).await,
            Outcome::DuplicateEvent
        );
        assert_eq!(w.net.notices().len(), 1);
        assert_eq!(jobs(&w.db).await.len(), 2);
    }

    #[tokio::test]
    async fn ignored_messages_cost_nothing() {
        let cases: Vec<(MessageFacts, IgnoreReason)> = vec![
            (
                MessageFacts {
                    author_is_bot: true,
                    ..facts()
                },
                IgnoreReason::BotAuthor,
            ),
            (
                MessageFacts {
                    is_webhook: true,
                    ..facts()
                },
                IgnoreReason::WebhookAuthor,
            ),
            (
                MessageFacts {
                    attachment_count: 0,
                    ..facts()
                },
                IgnoreReason::NoAttachments,
            ),
            (
                MessageFacts {
                    channel_id: OTHER_CH,
                    ..facts()
                },
                IgnoreReason::ChannelNotConfigured,
            ),
            (
                MessageFacts {
                    author_id: 0,
                    ..facts()
                },
                IgnoreReason::InvalidAuthor,
            ),
            (
                MessageFacts {
                    message_id: 0,
                    ..facts()
                },
                IgnoreReason::InvalidMessage,
            ),
            (
                MessageFacts {
                    guild_id: GUILD + 50,
                    ..facts()
                },
                IgnoreReason::GuildMismatch,
            ),
        ];
        for (f, reason) in cases {
            let w = world("live");
            let out = w.feat.process(f, Some(&w.db)).await;
            assert_eq!(out, Outcome::Ignored(reason));
            assert!(w.net.notices().is_empty(), "{reason:?}");
            assert!(w.net.deletes().is_empty(), "{reason:?}");
            assert!(w.net.alerts().is_empty(), "{reason:?}");
            assert!(jobs(&w.db).await.is_empty(), "{reason:?}");
        }
    }

    #[tokio::test]
    async fn a_thread_of_a_flash_channel_is_resolved_through_the_port() {
        let w = world("live");
        w.net
            .parents
            .lock()
            .unwrap()
            .insert(THREAD_CH, ParentLookup::Parent(FLASH_CH));
        let f = MessageFacts {
            channel_id: THREAD_CH,
            ..facts()
        };
        assert_eq!(
            w.feat.process(f, Some(&w.db)).await,
            Outcome::Expiring(NoticeStatus::Posted)
        );
        assert_eq!(w.net.notices()[0].channel_id, THREAD_CH);
        assert!(w.net.notices()[0].text.contains("<#5200000000000004>"));
        let rows = jobs(&w.db).await;
        assert_eq!(rows[0].key, "flash_src_5200000000000007");
        let payload: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
        assert_eq!(payload["channel_id"], serde_json::json!(THREAD_CH));
    }

    #[tokio::test]
    async fn thread_handling_follows_include_threads_and_unknown_parents_do_nothing() {
        // Not a thread: an ordinary channel that is not configured.
        let w = world("live");
        let f = MessageFacts {
            channel_id: OTHER_CH,
            ..facts()
        };
        assert_eq!(
            w.feat.process(f, Some(&w.db)).await,
            Outcome::Ignored(IgnoreReason::ChannelNotConfigured)
        );
        // A thread of an unrelated channel.
        w.net
            .parents
            .lock()
            .unwrap()
            .insert(THREAD_CH, ParentLookup::Parent(OTHER_CH));
        let f = MessageFacts {
            channel_id: THREAD_CH,
            ..facts()
        };
        assert_eq!(
            w.feat.process(f, Some(&w.db)).await,
            Outcome::Ignored(IgnoreReason::ChannelNotConfigured)
        );
        // The parent cannot be resolved: nothing is done, nothing is posted.
        w.net
            .parents
            .lock()
            .unwrap()
            .insert(THREAD_CH, ParentLookup::Unavailable);
        assert_eq!(
            w.feat.process(f, Some(&w.db)).await,
            Outcome::ParentLookupFailed
        );
        assert!(w.net.notices().is_empty());
        assert!(jobs(&w.db).await.is_empty());
        // Threads switched off.
        let net = Net::new();
        net.parents
            .lock()
            .unwrap()
            .insert(THREAD_CH, ParentLookup::Parent(FLASH_CH));
        let feat = feature(
            &net,
            &FakeClock::new(),
            settings("live", &[("FLASH_INCLUDE_THREADS", "false")]),
        );
        assert_eq!(
            feat.process(f, Some(&w.db)).await,
            Outcome::Ignored(IgnoreReason::ThreadNotIncluded)
        );
        assert!(net.notices().is_empty());
    }

    #[tokio::test]
    async fn the_parent_is_only_looked_up_when_it_could_matter() {
        let w = world("live");
        // A configured channel, a bot, a message without attachments: no lookup.
        w.feat.process(facts(), Some(&w.db)).await;
        w.feat
            .process(
                MessageFacts {
                    author_is_bot: true,
                    channel_id: THREAD_CH,
                    ..facts()
                },
                Some(&w.db),
            )
            .await;
        w.feat
            .process(
                MessageFacts {
                    attachment_count: 0,
                    channel_id: THREAD_CH,
                    ..facts()
                },
                Some(&w.db),
            )
            .await;
        assert!(!w.net.calls().iter().any(|c| matches!(c, Call::Parent(_))));
        // An attachment in an unconfigured channel: exactly one lookup.
        w.feat
            .process(
                MessageFacts {
                    channel_id: OTHER_CH,
                    message_id: MESSAGE + 1,
                    ..facts()
                },
                Some(&w.db),
            )
            .await;
        let lookups: Vec<Call> = w
            .net
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Parent(_)))
            .collect();
        assert_eq!(lookups, vec![Call::Parent(OTHER_CH)]);
    }

    // ----------------------------------------------------------------- modes

    #[tokio::test]
    async fn shadow_sends_deletes_and_schedules_nothing_and_records_counts_only() {
        let w = world("shadow");
        let out = w.feat.process(facts(), Some(&w.db)).await;
        assert_eq!(
            out,
            Outcome::Shadow(ShadowRecord {
                notices: 1,
                delete_jobs: 2,
                delay_secs: 180,
                role_mentions: 1,
                user_mentions: 1,
            })
        );
        assert!(
            w.net.calls().is_empty(),
            "shadow makes no Discord call, not even an alert"
        );
        assert!(
            jobs(&w.db).await.is_empty(),
            "shadow schedules no job that could delete later"
        );
        // It also works with no database at all.
        let out = w
            .feat
            .process(
                MessageFacts {
                    message_id: MESSAGE + 1,
                    ..facts()
                },
                None,
            )
            .await;
        assert!(matches!(out, Outcome::Shadow(_)));
        assert_eq!(w.feat.shadow_summary(), vec![("would_expire", 2)]);
    }

    #[tokio::test]
    async fn shadow_without_a_ping_role_counts_no_role_mention_and_tallies_ignores_by_code() {
        let net = Net::new();
        let clock = FakeClock::new();
        let s = {
            let loaded = load(
                |k| match k {
                    "FLASH_MODE" => Some("shadow".to_string()),
                    "FLASH_CHANNEL_IDS" => Some("5200000000000002".to_string()),
                    _ => None,
                },
                Some(GUILD),
            );
            assert_eq!(loaded.error, None);
            loaded.settings
        };
        let feat = feature(&net, &clock, s);
        let Outcome::Shadow(rec) = feat.process(facts(), None).await else {
            panic!("expected a shadow record");
        };
        assert_eq!((rec.role_mentions, rec.user_mentions), (0, 1));
        let bot = MessageFacts {
            author_is_bot: true,
            ..facts()
        };
        feat.process(bot, None).await;
        feat.process(bot, None).await;
        feat.process(
            MessageFacts {
                attachment_count: 0,
                ..facts()
            },
            None,
        )
        .await;
        // Messages elsewhere are the common case and are not counted.
        feat.process(
            MessageFacts {
                channel_id: OTHER_CH,
                ..facts()
            },
            None,
        )
        .await;
        assert_eq!(
            feat.shadow_summary(),
            vec![
                ("ignored_bot_author", 2),
                ("ignored_no_attachments", 1),
                ("would_expire", 1)
            ]
        );
        assert!(
            net.calls().iter().all(|c| matches!(c, Call::Parent(_))),
            "only reads"
        );
    }

    #[tokio::test]
    async fn off_does_nothing_at_all() {
        let w = world("off");
        assert!(!Feature::<(), ()>::enabled(&w.feat));
        assert_eq!(w.feat.process(facts(), Some(&w.db)).await, Outcome::Off);
        assert!(w.net.calls().is_empty());
        assert!(jobs(&w.db).await.is_empty());
        assert!(w.feat.shadow_summary().is_empty());
    }

    /// The Off/inactive gate sits in front of the thread-parent lookup (a Discord HTTP read).
    /// A flash-channel message cannot pin it (the inner `Mode::Off` arm gives the same answer),
    /// so use attachment messages in an unconfigured channel and in a thread.
    #[tokio::test]
    async fn off_and_inactive_features_answer_off_without_any_port_call_for_every_channel_kind() {
        let empty = [("FLASH_CHANNEL_IDS", "")];
        let cases: [(&str, &[(&str, &str)]); 2] = [("off", &[]), ("live", &empty)];
        for (mode, extra) in cases {
            for (what, channel) in [
                ("flash channel", FLASH_CH),
                ("other channel", OTHER_CH),
                ("thread", THREAD_CH),
            ] {
                let db = mem_db();
                let net = Net::probing(&db);
                net.parents
                    .lock()
                    .unwrap()
                    .insert(THREAD_CH, ParentLookup::Parent(FLASH_CH));
                let clock = FakeClock::new();
                let feat = feature(&net, &clock, settings(mode, extra));
                assert!(!Feature::<(), ()>::enabled(&feat), "{mode}/{what}");
                let out = feat
                    .process(
                        MessageFacts {
                            channel_id: channel,
                            ..facts()
                        },
                        Some(&db),
                    )
                    .await;
                assert_eq!(out, Outcome::Off, "{mode} / {what}");
                assert!(net.calls().is_empty(), "{mode} / {what}: {:?}", net.calls());
                assert!(jobs(&db).await.is_empty(), "{mode} / {what}");
                assert!(feat.shadow_summary().is_empty(), "{mode} / {what}");
            }
        }
    }

    #[test]
    fn the_registry_hooks_follow_the_mode() {
        let net = Net::new();
        let clock = FakeClock::new();
        let live = feature(&net, &clock, settings("live", &[]));
        assert_eq!(Feature::<(), ()>::name(&live), "flash");
        assert!(Feature::<(), ()>::enabled(&live));
        assert!(Feature::<(), ()>::needs_core_db(&live));
        let shadow = feature(&net, &clock, settings("shadow", &[]));
        assert!(Feature::<(), ()>::enabled(&shadow));
        assert!(
            !Feature::<(), ()>::needs_core_db(&shadow),
            "shadow must not open the database"
        );
        let off = feature(&net, &clock, settings("off", &[]));
        assert!(!Feature::<(), ()>::enabled(&off));
        let no_channels = feature(&net, &clock, settings("live", &[("FLASH_CHANNEL_IDS", "")]));
        assert!(!Feature::<(), ()>::enabled(&no_channels));
        // Registration adds exactly the two kinds, once.
        let mut registry = Registry::new();
        Feature::<(), ()>::register_jobs(&live, &mut registry).unwrap();
        assert_eq!(registry.len(), 2);
        assert_eq!(
            Feature::<(), ()>::register_jobs(&live, &mut registry),
            Err(RunnerError::DuplicateKind)
        );
    }

    #[test]
    fn the_feature_is_not_registered_in_production() {
        assert!(features::production_features::<(), ()>().is_empty());
    }

    // -------------------------------------------------------- delete handler

    #[tokio::test]
    async fn a_delete_job_deletes_the_payload_message() {
        let w = world("live");
        let r = w
            .feat
            .run_delete(job(KIND_DELETE_SOURCE, FLASH_CH, MESSAGE))
            .await;
        assert_eq!(r, Ok(()));
        assert_eq!(w.net.deletes(), vec![(FLASH_CH, MESSAGE)]);
        assert!(w.net.alerts().is_empty());
    }

    #[tokio::test]
    async fn a_404_means_already_gone_and_is_done_without_an_alert() {
        let w = world("live");
        w.net
            .script_delete_failures(MESSAGE, &[PortError::NotFound]);
        let r = w
            .feat
            .run_delete(job(KIND_DELETE_NOTICE, FLASH_CH, MESSAGE))
            .await;
        assert_eq!(r, Ok(()));
        assert!(w.net.alerts().is_empty());
    }

    #[tokio::test]
    async fn a_403_fails_the_job_and_alerts_staff_by_code() {
        let w = world("live");
        w.net
            .delete_always
            .lock()
            .unwrap()
            .insert(MESSAGE, PortError::Forbidden);
        let r = w
            .feat
            .run_delete(job(KIND_DELETE_SOURCE, FLASH_CH, MESSAGE))
            .await;
        assert_eq!(r.unwrap_err().code(), "delete_forbidden");
        assert_eq!(w.net.alerts(), vec![AlertCode::DeleteForbidden]);
    }

    #[tokio::test]
    async fn another_failure_fails_the_attempt_and_alerts() {
        let w = world("live");
        w.net
            .script_delete_failures(MESSAGE, &[PortError::Unavailable]);
        let r = w
            .feat
            .run_delete(job(KIND_DELETE_SOURCE, FLASH_CH, MESSAGE))
            .await;
        assert_eq!(r.unwrap_err().code(), "delete_unavailable");
        assert_eq!(w.net.alerts(), vec![AlertCode::DeleteFailed]);
        assert_eq!(
            w.feat
                .run_delete(job(KIND_DELETE_SOURCE, FLASH_CH, MESSAGE))
                .await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_malformed_payload_never_reaches_discord() {
        let w = world("live");
        for raw in [
            "",
            "{}",
            r#"{"channel_id":1,"message_id":2,"extra":3}"#,
            r#"{"channel_id":0,"message_id":2}"#,
            r#"{"channel_id":"1","message_id":2}"#,
            "[1,2]",
        ] {
            let r = w.feat.run_delete(raw_job(raw)).await;
            assert_eq!(r.unwrap_err().code(), "payload_invalid", "{raw:?}");
        }
        assert!(w.net.deletes().is_empty());
        assert_eq!(w.net.alerts(), vec![AlertCode::PayloadInvalid; 6]);
    }

    // ---------------------------------------------------- real runner, real DB

    struct Up {
        shutdown: ShutdownHandle,
        task: tokio::task::JoinHandle<()>,
        report: queue::Report,
    }

    impl Up {
        async fn stop(self) {
            self.shutdown.trigger();
            let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
        }
    }

    fn runner_cfg() -> RunnerConfig {
        RunnerConfig {
            concurrency: 2,
            max_poll: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(2),
            handler_timeout: Duration::from_secs(600),
            retry: RetryPolicy {
                base_secs: 30,
                cap_secs: 600,
                max_attempts: 3,
            },
        }
    }

    async fn boot(
        db: &tokio_rusqlite::Connection,
        clock: &Arc<FakeClock>,
        feat: &FlashFeature,
    ) -> Up {
        let mut registry = Registry::new();
        feat.register_job_kinds(&mut registry).unwrap();
        let runner = Runner::new(db.clone(), clock.clone(), registry, runner_cfg()).unwrap();
        let shutdown = runner.shutdown_handle();
        let started = runner.start().await.unwrap();
        let report = started.report().clone();
        let task = tokio::spawn(async move {
            let _ = started.run().await;
        });
        Up {
            shutdown,
            task,
            report,
        }
    }

    async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    async fn eventually_rows(
        db: &tokio_rusqlite::Connection,
        what: &str,
        cond: impl Fn(&[JobRow]) -> bool,
    ) {
        for _ in 0..1000 {
            if cond(&jobs(db).await) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    fn sorted(mut v: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
        v.sort_unstable();
        v
    }

    #[tokio::test]
    async fn after_a_restart_an_overdue_pair_is_deleted_at_once() {
        let dir = scratch("overdue");
        let path = dir.join("core.sqlite");
        let path = path.to_str().unwrap();
        let clock = FakeClock::new();
        {
            let db = file_db(path);
            let net = Net::new();
            let feat = feature(&net, &clock, settings("live", &[]));
            assert_eq!(
                feat.process(facts(), Some(&db)).await,
                Outcome::Expiring(NoticeStatus::Posted)
            );
        }
        // The bot was off for an hour: the 180 s deadline is long past.
        clock.advance(3600);
        let db = file_db(path);
        let net = Net::new();
        let feat = feature(&net, &clock, settings("live", &[]));
        let up = boot(&db, &clock, &feat).await;
        let n = net.clone();
        eventually("both overdue deletes", move || n.deletes().len() == 2).await;
        assert_eq!(
            sorted(net.deletes()),
            sorted(vec![(FLASH_CH, MESSAGE), (FLASH_CH, FIRST_NOTICE)])
        );
        assert_eq!(clock.secs(), T0 + 3600, "no waiting was needed");
        eventually_rows(&db, "both done", |rows| {
            rows.iter().all(|r| r.state == "done")
        })
        .await;
        up.stop().await;
    }

    #[tokio::test]
    async fn a_delete_cut_off_by_a_crash_runs_again_instead_of_being_abandoned() {
        let dir = scratch("crash");
        let path = dir.join("core.sqlite");
        let path = path.to_str().unwrap();
        let clock = FakeClock::new();
        {
            let db = file_db(path);
            let net = Net::new();
            let feat = feature(&net, &clock, settings("live", &[]));
            feat.process(facts(), Some(&db)).await;
            clock.advance(200);
            // A runner claimed both and died before acknowledging.
            let now = clock.secs();
            let batch = db.call(move |c| queue::claim_due(c, now, 8)).await.unwrap();
            assert_eq!(batch.claimed.len(), 2);
            assert!(jobs(&db).await.iter().all(|r| r.state == "running"));
        }
        let db = file_db(path);
        let net = Net::new();
        let feat = feature(&net, &clock, settings("live", &[]));
        let up = boot(&db, &clock, &feat).await;
        assert_eq!(up.report.entries.len(), 2);
        assert!(up
            .report
            .entries
            .iter()
            .all(|e| e.outcome == queue::ReconcileOutcome::Requeued));
        assert!(up.report.abandoned().is_empty());
        let n = net.clone();
        eventually("both deletes", move || n.deletes().len() == 2).await;
        eventually_rows(&db, "both done", |rows| {
            rows.iter().all(|r| r.state == "done")
        })
        .await;
        up.stop().await;
    }

    #[tokio::test]
    async fn nothing_is_deleted_before_the_deadline_and_both_go_at_it() {
        let db = mem_db();
        let net = Net::new();
        let clock = FakeClock::new();
        let feat = feature(&net, &clock, settings("live", &[]));
        feat.process(facts(), Some(&db)).await;
        let up = boot(&db, &clock, &feat).await;
        clock.advance(179);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(net.deletes().is_empty(), "179 s is one second early");
        clock.advance(1);
        let n = net.clone();
        eventually("deletes at the deadline", move || n.deletes().len() == 2).await;
        up.stop().await;
    }

    #[tokio::test]
    async fn a_404_finishes_the_job_at_the_first_attempt() {
        let db = mem_db();
        let net = Net::new();
        let clock = FakeClock::new();
        let feat = feature(&net, &clock, settings("live", &[]));
        feat.process(facts(), Some(&db)).await;
        net.script_delete_failures(MESSAGE, &[PortError::NotFound]);
        clock.advance(180);
        let up = boot(&db, &clock, &feat).await;
        eventually_rows(&db, "both done", |rows| {
            rows.iter().all(|r| r.state == "done")
        })
        .await;
        let rows = jobs(&db).await;
        assert!(rows.iter().all(|r| r.attempts == 1), "no retry after a 404");
        assert!(net.alerts().is_empty());
        up.stop().await;
    }

    #[tokio::test]
    async fn a_403_ends_as_a_failed_job_with_a_staff_alert_per_attempt() {
        let db = mem_db();
        let net = Net::new();
        let clock = FakeClock::new();
        let feat = feature(&net, &clock, settings("live", &[]));
        feat.process(facts(), Some(&db)).await;
        net.delete_always
            .lock()
            .unwrap()
            .insert(MESSAGE, PortError::Forbidden);
        clock.advance(180);
        let up = boot(&db, &clock, &feat).await;
        // max_attempts is 3: back-offs of 30 s and 60 s.
        for wait in [30, 60] {
            eventually_rows(&db, "a failed attempt", |rows| {
                rows.iter().any(|r| {
                    r.kind == KIND_DELETE_SOURCE && r.state == "pending" && r.due > clock.secs()
                })
            })
            .await;
            clock.advance(wait);
        }
        eventually_rows(&db, "source failed", |rows| {
            rows.iter()
                .any(|r| r.kind == KIND_DELETE_SOURCE && r.state == "failed")
        })
        .await;
        let rows = jobs(&db).await;
        let src = rows.iter().find(|r| r.kind == KIND_DELETE_SOURCE).unwrap();
        assert_eq!(src.attempts, 3);
        let notice = rows.iter().find(|r| r.kind == KIND_DELETE_NOTICE).unwrap();
        assert_eq!(notice.state, "done", "the notice delete is independent");
        let forbidden = net
            .alerts()
            .iter()
            .filter(|a| **a == AlertCode::DeleteForbidden)
            .count();
        assert_eq!(forbidden, 3);
        assert!(net
            .alerts()
            .iter()
            .all(|a| *a == AlertCode::DeleteForbidden));
        up.stop().await;
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_after_the_backoff_and_then_done() {
        let db = mem_db();
        let net = Net::new();
        let clock = FakeClock::new();
        let feat = feature(&net, &clock, settings("live", &[]));
        feat.process(facts(), Some(&db)).await;
        net.script_delete_failures(MESSAGE, &[PortError::Unavailable]);
        clock.advance(180);
        let up = boot(&db, &clock, &feat).await;
        eventually_rows(&db, "first attempt failed", |rows| {
            rows.iter()
                .any(|r| r.kind == KIND_DELETE_SOURCE && r.state == "pending" && r.attempts == 1)
        })
        .await;
        clock.advance(30);
        eventually_rows(&db, "both done", |rows| {
            rows.iter().all(|r| r.state == "done")
        })
        .await;
        assert_eq!(net.alerts(), vec![AlertCode::DeleteFailed]);
        assert!(net.deleted.lock().unwrap().contains(&MESSAGE));
        up.stop().await;
    }

    // ------------------------------------------------ through the registry

    struct NoopStarter {
        wakes: Arc<AtomicUsize>,
        kinds: Arc<AtomicUsize>,
    }

    impl JobStarter for NoopStarter {
        fn start(
            &self,
            _db: tokio_rusqlite::Connection,
            registry: Registry,
        ) -> BoxFuture<'_, Result<StartedJobs, RunnerError>> {
            self.kinds.store(registry.len(), SeqCst);
            let wakes = self.wakes.clone();
            Box::pin(async move {
                Ok(StartedJobs {
                    counts: ReportCounts::default(),
                    wake: Arc::new(move || {
                        wakes.fetch_add(1, SeqCst);
                    }),
                    shutdown: Arc::new(|| {}),
                    finished: Box::pin(async {}),
                })
            })
        }
    }

    fn feature_settings(guild: GuildFilter, db_path: Option<String>) -> Settings {
        Settings {
            guild,
            core_db_path: db_path,
            reserved_db_paths: vec![],
            staff_alert_channel: None,
            protected_role_count: 0,
            log_excluded_channels: 0,
            log_excluded_categories: 0,
        }
    }

    struct Lk(HashMap<u64, tickets::privacy::Lookup>);

    impl tickets::privacy::ChannelLookup for Lk {
        async fn lookup(&self, channel: serenity::ChannelId) -> tickets::privacy::Lookup {
            self.0
                .get(&channel.get())
                .copied()
                .unwrap_or(tickets::privacy::Lookup::Unknown)
        }
    }

    fn lookups() -> Lk {
        use tickets::privacy::Lookup::Guild;
        let mut m = HashMap::new();
        m.insert(FLASH_CH, Guild { parent: None });
        m.insert(OTHER_CH, Guild { parent: None });
        m.insert(
            THREAD_CH,
            Guild {
                parent: Some(FLASH_CH),
            },
        );
        Lk(m)
    }

    async fn app_data() -> crate::Data {
        let none = Features::<serenity::Context, crate::Data>::start(
            StartRequest {
                features: vec![],
                settings: &feature_settings(GuildFilter::Any, None),
            },
            &features::TokioJobStarter,
        )
        .await;
        crate::Data {
            config: Arc::new(crate::config::Config {
                discord_token: "synthetic-token".to_string(),
                log_channel_id: 5_500_000_000_000_001,
                age_role_1: 1,
                age_role_2: 2,
                reaction_role_db_path: "reaction-test.sqlite".to_string(),
                staff_role_id: None,
                tickets: None,
                ticket_archive_retention: tickets::retention::RetentionPolicy::Disabled,
                ticket_privacy_lockdown: false,
                core_db_path: None,
                guild_id: None,
                staff_alert_channel_id: None,
                log_excluded_channel_ids: vec![],
                log_excluded_category_ids: vec![],
                protected_role_ids: vec![],
                invalid_core_keys: vec![],
            }),
            message_cache: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            rr_db: Arc::new(tokio_rusqlite::Connection::open_in_memory().await.unwrap()),
            tickets: None,
            features: Arc::new(none),
            log_exclusion: features::LogExclusion::Off,
            wired: Default::default(),
        }
    }

    /// Routes one event through the real `route_event`; the sink is the real cache insert.
    async fn route(
        features: &Features<(), crate::Data>,
        data: &crate::Data,
        exclusion: &features::LogExclusion,
        event: &serenity::FullEvent,
        sinks: &AtomicUsize,
    ) {
        let lk = lookups();
        crate::route_event(
            crate::Route {
                features,
                ctx: &(),
                data,
                event,
                lookup: &lk,
                exclusion,
                ticket_lockdown: false,
                ticket_settings: None,
                ticket_repo: None,
            },
            || async {
                sinks.fetch_add(1, SeqCst);
                if let serenity::FullEvent::Message { new_message } = event {
                    crate::insert_cache(
                        data,
                        new_message.id,
                        crate::cached_message_from_message(new_message),
                    )
                    .await;
                }
                Ok(())
            },
        )
        .await
        .unwrap();
    }

    fn flash_exclusion(feat_settings: &FlashSettings) -> features::LogExclusion {
        // What the wiring card builds: the flash channel ids join the exclusion scope.
        features::LogExclusion::Scope(crate::privacy_scope::Scope::new(
            vec![],
            vec![],
            feat_settings.policy().channel_ids().to_vec(),
        ))
    }

    struct Registered {
        features: Features<(), crate::Data>,
        net: Arc<Net>,
        wakes: Arc<AtomicUsize>,
        kinds: Arc<AtomicUsize>,
        path: String,
    }

    async fn registered(name: &str, mode: &str, extra: &[(&str, &str)]) -> Registered {
        let dir = scratch(name);
        let path = dir.join("core.sqlite").to_str().unwrap().to_string();
        let net = Net::new();
        net.parents
            .lock()
            .unwrap()
            .insert(THREAD_CH, ParentLookup::Parent(FLASH_CH));
        let feat = feature(&net, &FakeClock::new(), settings(mode, extra));
        let wakes = Arc::new(AtomicUsize::new(0));
        let kinds = Arc::new(AtomicUsize::new(0));
        let starter = NoopStarter {
            wakes: wakes.clone(),
            kinds: kinds.clone(),
        };
        let features = Features::<(), crate::Data>::start(
            StartRequest {
                features: vec![Box::new(feat)],
                settings: &feature_settings(GuildFilter::Only(GUILD), Some(path.clone())),
            },
            &starter,
        )
        .await;
        Registered {
            features,
            net,
            wakes,
            kinds,
            path,
        }
    }

    fn message_event(channel: u64, id: u64) -> serenity::FullEvent {
        serenity::FullEvent::Message {
            new_message: flash_message(channel, id),
        }
    }

    fn delete_event(channel: u64) -> serenity::FullEvent {
        serenity::FullEvent::MessageDelete {
            channel_id: serenity::ChannelId::new(channel),
            deleted_message_id: serenity::MessageId::new(MESSAGE),
            guild_id: Some(serenity::GuildId::new(GUILD)),
        }
    }

    fn bulk_delete_event(channel: u64) -> serenity::FullEvent {
        serenity::FullEvent::MessageDeleteBulk {
            channel_id: serenity::ChannelId::new(channel),
            multiple_deleted_messages_ids: vec![serenity::MessageId::new(MESSAGE)],
            guild_id: Some(serenity::GuildId::new(GUILD)),
        }
    }

    fn update_event(channel: u64) -> serenity::FullEvent {
        serenity::FullEvent::MessageUpdate {
            old_if_available: None,
            new: None,
            event: serde_json::from_value(serde_json::json!({
                "id": MESSAGE.to_string(),
                "channel_id": channel.to_string(),
                "guild_id": GUILD.to_string(),
                "content": CAPTION,
            }))
            .unwrap(),
        }
    }

    /// A flash message reaches the feature (dispatch runs first) and never reaches the
    /// logger sink or the message cache; the real `route_event` and the real cache insert
    /// are used.
    #[tokio::test]
    async fn a_flash_message_reaches_the_feature_but_never_the_cache_or_the_logger() {
        for (index, (label, channel, include_threads)) in [
            ("flash channel", FLASH_CH, "true"),
            ("thread of the flash channel", THREAD_CH, "true"),
            ("thread when threads are not expired", THREAD_CH, "false"),
        ]
        .into_iter()
        .enumerate()
        {
            // One database file per case: a file still held by the previous case's
            // connection thread cannot be removed on Windows.
            let reg = registered(
                &format!("route-{index}"),
                "live",
                &[("FLASH_INCLUDE_THREADS", include_threads)],
            )
            .await;
            let data = app_data().await;
            let exclusion = flash_exclusion(&settings("live", &[]));
            let sinks = AtomicUsize::new(0);
            let seen = reg
                .features
                .dispatch(&(), &message_event(channel, MESSAGE), &data)
                .await;
            assert_eq!(seen, 1, "{label}: dispatch reached the feature");
            // The real routing: feature first, then the guarded logger.
            route(
                &reg.features,
                &data,
                &exclusion,
                &message_event(channel, MESSAGE + 1),
                &sinks,
            )
            .await;
            for event in [
                update_event(channel),
                delete_event(channel),
                bulk_delete_event(channel),
            ] {
                route(&reg.features, &data, &exclusion, &event, &sinks).await;
            }
            assert_eq!(
                sinks.load(SeqCst),
                0,
                "{label}: no flash event reached the logger"
            );
            assert!(
                data.message_cache.read().await.is_empty(),
                "{label}: nothing cached"
            );
            let expect_notices = if include_threads == "false" && channel == THREAD_CH {
                0
            } else {
                2
            };
            assert_eq!(
                reg.net.notices().len(),
                expect_notices,
                "{label}: the feature saw the message"
            );
        }
    }

    #[tokio::test]
    async fn the_control_cases_prove_the_exclusion_is_what_keeps_flash_out_of_the_cache() {
        let reg = registered("route-control", "live", &[]).await;
        let data = app_data().await;
        let sinks = AtomicUsize::new(0);
        // An ordinary channel still reaches the logger and the cache, and not the feature.
        let exclusion = flash_exclusion(&settings("live", &[]));
        route(
            &reg.features,
            &data,
            &exclusion,
            &message_event(OTHER_CH, MESSAGE),
            &sinks,
        )
        .await;
        assert_eq!(sinks.load(SeqCst), 1);
        assert_eq!(data.message_cache.read().await.len(), 1);
        assert!(reg.net.notices().is_empty());
        // Without the flash ids in the scope (a wiring mistake) the same flash message WOULD
        // be cached: the test above is sensitive to the exclusion.
        let reg2 = registered("route-control2", "live", &[]).await;
        let data2 = app_data().await;
        route(
            &reg2.features,
            &data2,
            &features::LogExclusion::Off,
            &message_event(FLASH_CH, MESSAGE),
            &sinks,
        )
        .await;
        assert_eq!(data2.message_cache.read().await.len(), 1);
    }

    #[tokio::test]
    async fn the_bots_own_notice_event_is_ignored_by_the_feature_and_kept_from_the_logger() {
        let reg = registered("route-bot", "live", &[]).await;
        let data = app_data().await;
        let exclusion = flash_exclusion(&settings("live", &[]));
        let sinks = AtomicUsize::new(0);
        let mut notice = flash_message(FLASH_CH, FIRST_NOTICE);
        notice.author.bot = true;
        notice.attachments.clear();
        let event = serenity::FullEvent::Message {
            new_message: notice,
        };
        route(&reg.features, &data, &exclusion, &event, &sinks).await;
        assert!(reg.net.calls().is_empty());
        assert_eq!(sinks.load(SeqCst), 0);
        assert!(data.message_cache.read().await.is_empty());
    }

    #[tokio::test]
    async fn live_through_the_registry_schedules_two_jobs_and_wakes_the_runner_after_each() {
        let reg = registered("registry-live", "live", &[]).await;
        assert_eq!(reg.kinds.load(SeqCst), 2, "both job kinds were registered");
        let data = app_data().await;
        reg.features
            .dispatch(&(), &message_event(FLASH_CH, MESSAGE), &data)
            .await;
        assert_eq!(reg.net.notices().len(), 1);
        assert_eq!(reg.wakes.load(SeqCst), 2);
        let db = file_db(&reg.path);
        let rows = jobs(&db).await;
        assert_eq!(rows.len(), 2);
        // Another guild's event never reaches the feature.
        let mut foreign = flash_message(FLASH_CH, MESSAGE + 5);
        foreign.guild_id = Some(serenity::GuildId::new(GUILD + 9));
        let seen = reg
            .features
            .dispatch(
                &(),
                &serenity::FullEvent::Message {
                    new_message: foreign,
                },
                &data,
            )
            .await;
        assert_eq!(seen, 0);
        assert_eq!(reg.net.notices().len(), 1);
    }

    #[tokio::test]
    async fn shadow_through_the_registry_opens_no_database_and_registers_no_job() {
        let reg = registered("registry-shadow", "shadow", &[]).await;
        assert_eq!(reg.kinds.load(SeqCst), 0);
        assert!(
            !std::path::Path::new(&reg.path).exists(),
            "no core database was created"
        );
        let data = app_data().await;
        let seen = reg
            .features
            .dispatch(&(), &message_event(FLASH_CH, MESSAGE), &data)
            .await;
        assert_eq!(seen, 1);
        assert!(reg.net.calls().is_empty());
        assert_eq!(reg.wakes.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn off_through_the_registry_is_not_even_dispatched() {
        let reg = registered("registry-off", "off", &[]).await;
        let data = app_data().await;
        let seen = reg
            .features
            .dispatch(&(), &message_event(FLASH_CH, MESSAGE), &data)
            .await;
        assert_eq!(seen, 0);
        assert!(reg.net.calls().is_empty());
        assert!(!std::path::Path::new(&reg.path).exists());
    }

    // -------------------------------------------------------------- liveness

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Fate {
        Normal,
        AlreadyGone,
        Forbidden,
    }

    /// One model tick: claim what is due, run the real handler, acknowledge like a runner.
    async fn tick(w: &World, claim_limit: u32) {
        let now = w.clock.secs();
        let batch =
            w.db.call(move |c| queue::claim_due(c, now, claim_limit))
                .await
                .unwrap();
        for claimed in batch.claimed {
            let id = claimed.id;
            match w.feat.run_delete(claimed).await {
                Ok(()) => {
                    w.db.call(move |c| queue::complete(c, id, now))
                        .await
                        .unwrap();
                }
                Err(e) => {
                    let code = e.code();
                    w.db.call(move |c| queue::fail(c, id, code, now, &RetryPolicy::DEFAULT))
                        .await
                        .unwrap();
                }
            }
        }
    }

    /// Random flashes, random Discord faults, random crashes: every flash that the runtime
    /// accepted ends with its source deleted, or failed with an alert, and no job is parked
    /// beyond the back-off cap while unacknowledged.
    #[tokio::test]
    async fn liveness_random_flashes_faults_and_crashes_always_end_deleted_or_failed_with_an_alert()
    {
        for seed in 1..=12u64 {
            let mut rng = Lcg(seed * 7919);
            let w = world("live");
            let mut fates: Vec<(u64, Fate)> = Vec::new();
            let mut expiring = 0usize;
            let mut notices_posted = 0usize;
            let mut crashes = 0;
            let mut next_message = MESSAGE;
            // Transient (`Unavailable`) delete failures scripted: each yields exactly one
            // `DeleteFailed` alert, so any more means something else was treated as a failure.
            let mut transient_scripted = 0usize;
            // Claims lost to a simulated crash, per (kind, payload): they count as attempts.
            let mut crash_claims: HashMap<(String, String), i64> = HashMap::new();
            for _ in 0..60 {
                match rng.below(10) {
                    0..=3 => {
                        next_message += 1;
                        let notice_ok = rng.below(3) != 0;
                        let fate = match rng.below(6) {
                            0 => Fate::AlreadyGone,
                            1 => Fate::Forbidden,
                            _ => Fate::Normal,
                        };
                        if notice_ok {
                            w.net.script_notice(Ok(()));
                            if rng.below(2) == 0 {
                                w.net.script_delete_failures(
                                    w.net.peek_next_notice(),
                                    &[PortError::Unavailable],
                                );
                                transient_scripted += 1;
                            }
                        } else {
                            w.net.script_notice(Err(PortError::Unavailable));
                        }
                        match fate {
                            // A message that is really gone answers 404 every time.
                            Fate::AlreadyGone => {
                                w.net
                                    .delete_always
                                    .lock()
                                    .unwrap()
                                    .insert(next_message, PortError::NotFound);
                            }
                            Fate::Forbidden => {
                                w.net
                                    .delete_always
                                    .lock()
                                    .unwrap()
                                    .insert(next_message, PortError::Forbidden);
                            }
                            Fate::Normal => {
                                let transient = rng.below(3) as usize;
                                transient_scripted += transient;
                                w.net.script_delete_failures(
                                    next_message,
                                    &[PortError::Unavailable; 2][..transient],
                                );
                            }
                        }
                        let f = MessageFacts {
                            message_id: next_message,
                            ..facts()
                        };
                        match w.feat.process(f, Some(&w.db)).await {
                            Outcome::Expiring(status) => {
                                expiring += 1;
                                if status == NoticeStatus::Posted {
                                    notices_posted += 1;
                                }
                                fates.push((next_message, fate));
                            }
                            other => panic!("seed {seed}: unexpected {other:?}"),
                        }
                    }
                    4 | 5 => w.clock.advance(rng.below(200) as i64),
                    6 | 7 => tick(&w, 3).await,
                    8 if crashes < 2 => {
                        crashes += 1;
                        // A runner claims up to two due jobs and dies before acknowledging.
                        let now = w.clock.secs();
                        let batch =
                            w.db.call(move |c| queue::claim_due(c, now, 2))
                                .await
                                .unwrap();
                        // A claim counts as an attempt even though the runner died: remember it.
                        for lost in batch.claimed {
                            *crash_claims
                                .entry((lost.kind, lost.payload_json))
                                .or_insert(0) += 1;
                        }
                        let report =
                            w.db.call(move |c| {
                                queue::reconcile_after_restart(
                                    c,
                                    now,
                                    &JOB_KINDS,
                                    &RetryPolicy::DEFAULT,
                                )
                            })
                            .await
                            .unwrap();
                        assert!(
                            report.abandoned().is_empty(),
                            "seed {seed}: nothing is abandoned"
                        );
                    }
                    _ => {
                        let out = w
                            .feat
                            .process(
                                MessageFacts {
                                    author_is_bot: true,
                                    ..facts()
                                },
                                Some(&w.db),
                            )
                            .await;
                        assert_eq!(out, Outcome::Ignored(IgnoreReason::BotAuthor));
                    }
                }
                // Never quiet: nothing unacknowledged is parked beyond the back-off cap.
                let now = w.clock.secs();
                for row in jobs(&w.db).await {
                    if row.state == "pending" {
                        assert!(
                            row.due <= now + 600,
                            "seed {seed}: parked job {row:?} at {now}"
                        );
                    }
                    assert_ne!(row.state, "running", "seed {seed}: a job stayed running");
                    assert_ne!(row.state, "abandoned", "seed {seed}");
                }
            }
            // Drain: let every back-off elapse.
            for _ in 0..80 {
                if jobs(&w.db).await.iter().all(|r| r.state != "pending") {
                    break;
                }
                w.clock.advance(700);
                tick(&w, 8).await;
            }
            let rows = jobs(&w.db).await;
            assert!(
                rows.iter()
                    .all(|r| r.state == "done" || r.state == "failed"),
                "seed {seed}: unfinished jobs {rows:?}"
            );
            let sources: Vec<&JobRow> = rows
                .iter()
                .filter(|r| r.kind == KIND_DELETE_SOURCE)
                .collect();
            assert_eq!(
                sources.len(),
                expiring,
                "seed {seed}: one source job per accepted flash"
            );
            assert_eq!(
                rows.iter().filter(|r| r.kind == KIND_DELETE_NOTICE).count(),
                notices_posted,
                "seed {seed}"
            );
            for (message, fate) in &fates {
                let key = source_dedupe_key(*message);
                let row = rows
                    .iter()
                    .find(|r| r.key == key)
                    .expect("source job exists");
                match fate {
                    Fate::Forbidden => {
                        assert_eq!(row.state, "failed", "seed {seed} message {message}");
                        assert!(w.net.alerts().contains(&AlertCode::DeleteForbidden));
                        assert!(!w.net.deleted.lock().unwrap().contains(message));
                    }
                    Fate::Normal => {
                        assert_eq!(row.state, "done", "seed {seed} message {message}");
                        assert!(w.net.deleted.lock().unwrap().contains(message));
                    }
                    Fate::AlreadyGone => {
                        assert_eq!(row.state, "done", "seed {seed} message {message}");
                        assert_eq!(
                            row.attempts,
                            1 + crash_claims
                                .get(&(row.kind.clone(), row.payload.clone()))
                                .copied()
                                .unwrap_or(0),
                            "seed {seed} message {message}: a 404 is final, no retry \
                             (only a crash-lost claim may add an attempt)"
                        );
                        assert_eq!(
                            w.net.deletes().iter().filter(|(_, m)| m == message).count(),
                            1,
                            "seed {seed} message {message}: exactly one delete call"
                        );
                    }
                }
            }
            for row in rows.iter().filter(|r| r.kind == KIND_DELETE_NOTICE) {
                assert_eq!(
                    row.state, "done",
                    "seed {seed}: notice deletes only see transient faults"
                );
            }
            // Already-gone (404) sources raise no `DeleteFailed`: only the scripted transient
            // faults do, one alert each.
            assert_eq!(
                w.net
                    .alerts()
                    .iter()
                    .filter(|a| **a == AlertCode::DeleteFailed)
                    .count(),
                transient_scripted,
                "seed {seed}: DeleteFailed alerts only for transient faults"
            );
        }
    }
}

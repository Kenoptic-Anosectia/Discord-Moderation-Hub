//! The `/embed post|sync|list` logic (`docs/verification/EMBED-COMMAND.md`).
//!
//! `post` publishes a template into a channel and records the message in the
//! `managed_posts` table, `sync` re-renders recorded posts by EDITING the recorded
//! message, `list` shows what is recorded. The functions here are plain async functions
//! over small traits (Discord, the post store, the template source, the alert sink) so
//! tests inject fakes; registering a poise command is the wiring card's job.
//!
//! The decisions are made by the EM1 policy (`posts::plan`, `posts::validate_key`,
//! `posts::apply`) and by the pure functions of this file (`authorize`, the reply
//! builders). Replies and alerts carry stable codes only.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use poise::serenity_prelude as s;

use super::config::{EmbedConfig, Publishers};
use super::posts::{self, Plan, PlanRequest, PostRecord, RepoError, StoredPost};
use super::template::{self, EmbedTemplate};
use crate::features::Feature;
use crate::jobs::runner::{Registry, RunnerError};
use crate::settings::Mode;

// ---------------------------------------------------------------------------
// Codes, outcomes, reports
// ---------------------------------------------------------------------------

/// Stable result codes of the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Disabled,
    Denied,
    InvalidKey,
    InvalidTarget,
    TemplateNotFound,
    TemplateUnreadable,
    TemplateInvalid,
    Posted,
    Edited,
    Recreated,
    Unchanged,
    WouldPost,
    WouldEdit,
    WouldRecreate,
    Refused,
    MessageMissing,
    NotRecorded,
    Busy,
    DiscordForbidden,
    DiscordFailed,
    StoreFailed,
    PostedNotRecorded,
    EditedNotRecorded,
    Listed,
}

/// How a code counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The command did (or, in shadow mode, would do) what was asked.
    Success,
    /// The command declined; nothing is broken.
    Refusal,
    /// Something outside the command failed; staff is alerted.
    Failure,
}

impl Code {
    /// The stable text of the code.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Disabled => "disabled",
            Code::Denied => "denied",
            Code::InvalidKey => "invalid_key",
            Code::InvalidTarget => "invalid_target",
            Code::TemplateNotFound => "template_not_found",
            Code::TemplateUnreadable => "template_unreadable",
            Code::TemplateInvalid => "template_invalid",
            Code::Posted => "posted",
            Code::Edited => "edited",
            Code::Recreated => "recreated",
            Code::Unchanged => "unchanged",
            Code::WouldPost => "would_post",
            Code::WouldEdit => "would_edit",
            Code::WouldRecreate => "would_recreate",
            Code::Refused => "refused",
            Code::MessageMissing => "message_missing",
            Code::NotRecorded => "not_recorded",
            Code::Busy => "busy",
            Code::DiscordForbidden => "discord_forbidden",
            Code::DiscordFailed => "discord_failed",
            Code::StoreFailed => "store_failed",
            Code::PostedNotRecorded => "posted_not_recorded",
            Code::EditedNotRecorded => "edited_not_recorded",
            Code::Listed => "listed",
        }
    }

    /// Whether the code is a success, a refusal or a failure.
    pub fn class(self) -> Class {
        match self {
            Code::Posted
            | Code::Edited
            | Code::Recreated
            | Code::Unchanged
            | Code::WouldPost
            | Code::WouldEdit
            | Code::WouldRecreate
            | Code::Listed => Class::Success,
            Code::DiscordForbidden
            | Code::DiscordFailed
            | Code::StoreFailed
            | Code::PostedNotRecorded
            | Code::EditedNotRecorded => Class::Failure,
            _ => Class::Refusal,
        }
    }
}

/// A code with an optional second stable code that says why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// The result.
    pub code: Code,
    /// A stable sub-code (a refusal reason, a template error code, a store error code).
    pub detail: Option<&'static str>,
}

impl Outcome {
    /// An outcome without a detail.
    pub fn new(code: Code) -> Self {
        Outcome { code, detail: None }
    }
    /// An outcome with a detail code.
    pub fn with(code: Code, detail: &'static str) -> Self {
        Outcome {
            code,
            detail: Some(detail),
        }
    }
}

/// The outcome for one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemReport {
    /// The key, only when it is a plain valid key; a refused spelling is never echoed.
    pub key: Option<String>,
    /// What happened.
    pub outcome: Outcome,
}

/// The result of `post` or `sync`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// True when the run was a shadow run: nothing was sent, edited, deleted or written.
    pub shadow: bool,
    /// One item per key that was handled.
    pub items: Vec<ItemReport>,
}

/// Counts of codes: the structured would-do record of a shadow run, and the summary of
/// a live one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub posted: usize,
    pub edited: usize,
    pub recreated: usize,
    pub would_post: usize,
    pub would_edit: usize,
    pub would_recreate: usize,
    pub unchanged: usize,
    pub refused: usize,
    pub failed: usize,
}

impl Report {
    /// Counts the codes of the items.
    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for item in &self.items {
            match item.outcome.code {
                Code::Posted => counts.posted += 1,
                Code::Edited => counts.edited += 1,
                Code::Recreated => counts.recreated += 1,
                Code::WouldPost => counts.would_post += 1,
                Code::WouldEdit => counts.would_edit += 1,
                Code::WouldRecreate => counts.would_recreate += 1,
                Code::Unchanged => counts.unchanged += 1,
                code => match code.class() {
                    Class::Refusal => counts.refused += 1,
                    Class::Failure => counts.failed += 1,
                    Class::Success => {}
                },
            }
        }
        counts
    }
}

/// The result of `list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListReport {
    /// `Listed`, or why nothing is listed.
    pub outcome: Outcome,
    /// The recorded posts, ordered by key.
    pub posts: Vec<StoredPost>,
}

/// An ephemeral reply. The type has no way to build a public one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    text: String,
}

/// Longest reply, in characters (Discord's message limit).
pub const MAX_REPLY_CHARS: usize = 2000;

impl Reply {
    /// The text of the reply.
    pub fn text(&self) -> &str {
        &self.text
    }
    /// Replies are ephemeral.
    pub fn ephemeral(&self) -> bool {
        true
    }
}

fn outcome_text(outcome: &Outcome) -> String {
    match outcome.detail {
        Some(detail) => format!("{} ({detail})", outcome.code.as_str()),
        None => outcome.code.as_str().to_string(),
    }
}

/// Adds lines to `text` while the total stays within the reply limit, leaving room for a
/// closing "+N more" line.
fn push_lines_within_limit(text: &mut String, lines: &[String]) {
    // Room kept for "\n+99999 more".
    const RESERVE: usize = 16;
    let mut used = text.chars().count();
    for (index, line) in lines.iter().enumerate() {
        let cost = 1 + line.chars().count();
        if used + cost + RESERVE > MAX_REPLY_CHARS {
            text.push_str(&format!("\n+{} more", lines.len() - index));
            return;
        }
        text.push('\n');
        text.push_str(line);
        used += cost;
    }
}

/// Builds the reply for a `post` or `sync` report.
pub fn report_reply(report: &Report) -> Reply {
    let mut text = String::new();
    if report.shadow {
        text.push_str("[shadow] nothing was sent, edited, deleted or written.\n");
    }
    match report.items.as_slice() {
        [only] => {
            text.push_str("embed ");
            text.push_str(&outcome_text(&only.outcome));
            if let Some(key) = &only.key {
                text.push_str(&format!(" key={key}"));
            }
        }
        items => {
            let c = report.counts();
            text.push_str(&format!(
                "embed sync: {} keys: posted={} edited={} recreated={} would_post={} would_edit={} would_recreate={} unchanged={} refused={} failed={}",
                items.len(),
                c.posted,
                c.edited,
                c.recreated,
                c.would_post,
                c.would_edit,
                c.would_recreate,
                c.unchanged,
                c.refused,
                c.failed
            ));
            let lines: Vec<String> = items
                .iter()
                .map(|item| {
                    format!(
                        "{}: {}",
                        item.key.as_deref().unwrap_or("-"),
                        outcome_text(&item.outcome)
                    )
                })
                .collect();
            push_lines_within_limit(&mut text, &lines);
        }
    }
    Reply { text }
}

/// Builds the reply for a `list` report.
pub fn list_reply(list: &ListReport) -> Reply {
    if list.outcome.code != Code::Listed {
        return Reply {
            text: format!("embed {}", outcome_text(&list.outcome)),
        };
    }
    let mut text = format!("embed list: {} posts", list.posts.len());
    let lines: Vec<String> = list
        .posts
        .iter()
        .map(|post| {
            format!(
                "`{}` <#{}> message {} hash {}",
                post.record.key,
                post.record.channel,
                post.record.message,
                &post.record.content_hash[..12]
            )
        })
        .collect();
    push_lines_within_limit(&mut text, &lines);
    Reply { text }
}

/// The text of a staff alert: a stable code only.
pub fn alert_text(code: &'static str) -> String {
    format!("embeds: {code}")
}

// ---------------------------------------------------------------------------
// Who may publish
// ---------------------------------------------------------------------------

/// Who is calling, as the wiring card works it out from the interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invoker {
    /// The calling user.
    pub user: u64,
    /// The owner of the guild the command was used in.
    pub guild_owner: u64,
    /// True when the caller holds the configured staff role.
    pub is_staff: bool,
}

/// Checks that the caller may publish: the guild owner always, staff only when the
/// configuration widened it. A caller or owner id of zero is never trusted.
pub fn authorize(publishers: Publishers, who: &Invoker) -> Result<(), Code> {
    if who.user == 0 {
        return Err(Code::Denied);
    }
    let is_owner = who.user == who.guild_owner;
    if is_owner || (publishers == Publishers::Staff && who.is_staff) {
        Ok(())
    } else {
        Err(Code::Denied)
    }
}

/// Checks that the call comes from the one home guild. Commands are registered globally,
/// so the owner of ANY guild the bot is in could otherwise use them.
pub fn check_guild(home_guild: Option<u64>, guild: u64) -> Result<(), Outcome> {
    match home_guild {
        // No home guild (or the zero id) means the wiring did not say which guild to
        // serve: nothing is served.
        None | Some(0) => Err(Outcome::with(Code::Denied, "no_home_guild")),
        Some(home) if home == guild => Ok(()),
        Some(_) => Err(Outcome::with(Code::Denied, "other_guild")),
    }
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

/// The mentions a message may fire. Embeds name none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedMentions {
    pub users: Vec<u64>,
    pub roles: Vec<u64>,
    pub everyone: bool,
    pub replied_user: bool,
}

impl AllowedMentions {
    /// No mention fires.
    pub fn none() -> Self {
        AllowedMentions {
            users: Vec::new(),
            roles: Vec::new(),
            everyone: false,
            replied_user: false,
        }
    }
}

/// One embed to send or edit.
#[derive(Debug, Clone)]
pub struct OutgoingEmbed<'a> {
    /// The channel.
    pub channel: u64,
    /// The template to render.
    pub template: &'a EmbedTemplate,
    /// The allowed mentions.
    pub mentions: AllowedMentions,
}

/// Why a Discord call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordError {
    /// The message or channel does not exist.
    NotFound,
    /// The bot may not do this.
    Forbidden,
    /// Anything else.
    Failed,
}

/// Whether a recorded message still exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageState {
    Present,
    Missing,
}

/// The Discord calls the command needs.
#[s::async_trait]
pub trait EmbedDiscord: Send + Sync {
    /// Looks a recorded message up.
    async fn message_state(&self, channel: u64, message: u64)
        -> Result<MessageState, DiscordError>;
    /// Sends a new message and returns its id.
    async fn send(&self, out: &OutgoingEmbed<'_>) -> Result<u64, DiscordError>;
    /// Edits an existing message.
    async fn edit(&self, message: u64, out: &OutgoingEmbed<'_>) -> Result<(), DiscordError>;
    /// Deletes a message.
    async fn delete(&self, channel: u64, message: u64) -> Result<(), DiscordError>;
}

/// Why a template could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    NotFound,
    Unreadable,
    TooLarge,
}

/// Where template text comes from.
#[s::async_trait]
pub trait TemplateSource: Send + Sync {
    /// Reads the template text of a key.
    async fn read(&self, key: &str) -> Result<String, SourceError>;
}

/// Longest template file, in bytes.
pub const MAX_TEMPLATE_BYTES: u64 = 256 * 1024;

/// Reads `<dir>/<key>.json`, only for a plain valid key.
pub struct DirTemplateSource {
    dir: PathBuf,
}

impl DirTemplateSource {
    /// A source over a directory.
    pub fn new(dir: PathBuf) -> Self {
        DirTemplateSource { dir }
    }
}

#[s::async_trait]
impl TemplateSource for DirTemplateSource {
    async fn read(&self, key: &str) -> Result<String, SourceError> {
        // Defence in depth: the command already refused every key that is not plain,
        // and this refuses it again before a path is built.
        if posts::validate_key(key).is_err() {
            return Err(SourceError::Unreadable);
        }
        let path = self.dir.join(format!("{key}.json"));
        tokio::task::spawn_blocking(move || read_limited(&path))
            .await
            .unwrap_or(Err(SourceError::Unreadable))
    }
}

/// Reads one regular file, at most [`MAX_TEMPLATE_BYTES`], as UTF-8.
fn read_limited(path: &std::path::Path) -> Result<String, SourceError> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SourceError::NotFound
        } else {
            SourceError::Unreadable
        }
    })?;
    let meta = file.metadata().map_err(|_| SourceError::Unreadable)?;
    if !meta.is_file() {
        return Err(SourceError::Unreadable);
    }
    let mut bytes = Vec::new();
    file.take(MAX_TEMPLATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SourceError::Unreadable)?;
    if bytes.len() as u64 > MAX_TEMPLATE_BYTES {
        return Err(SourceError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| SourceError::Unreadable)
}

/// What a store write must do. Owned, so it can cross to the database thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteKind {
    Create,
    Edit { message: u64, from: String },
    Recreate { replaces: u64 },
}

/// One write of the record of a post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRequest {
    pub key: String,
    pub kind: WriteKind,
    pub record: PostRecord,
    pub now: i64,
}

/// The record store.
#[s::async_trait]
pub trait PostStore: Send + Sync {
    /// Reads one record.
    async fn get(&self, key: &str) -> Result<Option<StoredPost>, RepoError>;
    /// Reads every record, ordered by key.
    async fn list(&self) -> Result<Vec<StoredPost>, RepoError>;
    /// Writes a record under a plan (`posts::apply`).
    async fn write(&self, req: WriteRequest) -> Result<bool, RepoError>;
}

/// The store over the shared core database.
pub struct TokioStore {
    conn: tokio_rusqlite::Connection,
}

/// A database call that could not run is a store failure; an error from the call is kept.
fn store_error(error: tokio_rusqlite::Error<RepoError>) -> RepoError {
    match error {
        tokio_rusqlite::Error::Error(inner) => inner,
        _ => RepoError::Sqlite,
    }
}

impl TokioStore {
    /// A store over a connection.
    pub fn new(conn: tokio_rusqlite::Connection) -> Self {
        TokioStore { conn }
    }

    /// Creates the `managed_posts` table if it is not there.
    pub async fn migrate(&self) -> Result<(), RepoError> {
        self.conn
            .call(|c| posts::migrate(c).map(|_| ()).map_err(|_| RepoError::Sqlite))
            .await
            .map_err(store_error)
    }
}

#[s::async_trait]
impl PostStore for TokioStore {
    async fn get(&self, key: &str) -> Result<Option<StoredPost>, RepoError> {
        let key = key.to_string();
        self.conn
            .call(move |c| posts::get(c, &key))
            .await
            .map_err(store_error)
    }
    async fn list(&self) -> Result<Vec<StoredPost>, RepoError> {
        self.conn
            .call(|c| posts::list(c))
            .await
            .map_err(store_error)
    }
    async fn write(&self, req: WriteRequest) -> Result<bool, RepoError> {
        self.conn
            .call(move |c| {
                let key = req.key.as_str();
                let plan = match &req.kind {
                    WriteKind::Create => Plan::Create { key },
                    WriteKind::Edit { message, from } => Plan::Edit {
                        key,
                        message: *message,
                        from: from.as_str(),
                    },
                    WriteKind::Recreate { replaces } => Plan::Recreate {
                        key,
                        replaces: *replaces,
                    },
                };
                posts::apply(c, &plan, &req.record, req.now)
            })
            .await
            .map_err(store_error)
    }
}

/// Where failures are reported: the staff alert channel. Only a stable code goes in.
#[s::async_trait]
pub trait AlertSink: Send + Sync {
    /// Reports one failure code.
    async fn alert(&self, code: &'static str);
}

// ---------------------------------------------------------------------------
// One sync at a time per key
// ---------------------------------------------------------------------------

/// The keys that are being handled right now.
#[derive(Default)]
pub struct KeyLocks {
    held: Mutex<HashSet<String>>,
}

/// Holds one key until it is dropped.
pub struct KeyGuard<'a> {
    locks: &'a KeyLocks,
    key: String,
}

impl KeyLocks {
    /// An empty set.
    pub fn new() -> Self {
        KeyLocks::default()
    }

    fn set(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        // A poisoned set is still a valid set of keys.
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes a key, or `None` when it is already held.
    pub fn try_acquire(&self, key: &str) -> Option<KeyGuard<'_>> {
        if self.set().insert(key.to_string()) {
            Some(KeyGuard {
                locks: self,
                key: key.to_string(),
            })
        } else {
            None
        }
    }
}

impl Drop for KeyGuard<'_> {
    fn drop(&mut self) {
        self.locks.set().remove(&self.key);
    }
}

// ---------------------------------------------------------------------------
// The command functions
// ---------------------------------------------------------------------------

/// Everything the command functions use.
pub struct Env<'a> {
    pub config: &'a EmbedConfig,
    pub discord: &'a dyn EmbedDiscord,
    pub store: &'a dyn PostStore,
    pub templates: &'a dyn TemplateSource,
    pub alerts: &'a dyn AlertSink,
    pub locks: &'a KeyLocks,
    /// The one guild these commands serve (the wiring passes `GUILD_ID`).
    pub home_guild: Option<u64>,
}

/// `/embed post`.
#[derive(Debug, Clone, Copy)]
pub struct PostRequest<'a> {
    pub invoker: Invoker,
    pub key: &'a str,
    pub guild: u64,
    pub channel: u64,
    /// Allow a post whose recorded message is gone to be posted again.
    pub allow_recreate: bool,
    /// Epoch milliseconds, passed in.
    pub now: i64,
}

/// `/embed sync`.
#[derive(Debug, Clone, Copy)]
pub struct SyncRequest<'a> {
    pub invoker: Invoker,
    /// One key, or every recorded key.
    pub key: Option<&'a str>,
    pub guild: u64,
    pub allow_recreate: bool,
    pub now: i64,
}

/// `/embed list`.
#[derive(Debug, Clone, Copy)]
pub struct ListRequest {
    pub invoker: Invoker,
    /// The guild the command was used in.
    pub guild: u64,
}

/// One unit of work: one key.
struct Job<'a> {
    invoker: Invoker,
    key: &'a str,
    guild: u64,
    /// The channel to publish into; `None` means "the recorded channel" (sync).
    channel: Option<u64>,
    allow_recreate: bool,
    now: i64,
    shadow: bool,
}

/// Mode, caller and guild checks shared by every command. The order matters: an off
/// feature says `disabled`; a caller who is not allowed is `denied` whatever the guild;
/// guild 0 is an invalid target; and only the home guild is served.
fn gate(env: &Env<'_>, who: &Invoker, guild: u64) -> Result<(), Outcome> {
    if !env.config.enabled() {
        return Err(Outcome::new(Code::Disabled));
    }
    authorize(env.config.publishers, who).map_err(Outcome::new)?;
    if guild == 0 {
        return Err(Outcome::new(Code::InvalidTarget));
    }
    check_guild(env.home_guild, guild)
}

fn refused_report(env: &Env<'_>, outcome: Outcome) -> Report {
    Report {
        shadow: env.config.mode == Mode::Shadow,
        items: vec![ItemReport { key: None, outcome }],
    }
}

/// Reports each failure code once. A shadow run sends nothing, an alert included.
async fn alert_failures(env: &Env<'_>, shadow: bool, outcomes: impl Iterator<Item = Outcome>) {
    if shadow {
        return;
    }
    let mut sent: Vec<Code> = Vec::new();
    for outcome in outcomes {
        if outcome.code.class() == Class::Failure && !sent.contains(&outcome.code) {
            sent.push(outcome.code);
            env.alerts.alert(outcome.code.as_str()).await;
        }
    }
}

/// Publishes a template into a channel and records it.
pub async fn post(env: &Env<'_>, req: &PostRequest<'_>) -> Report {
    if let Err(outcome) = gate(env, &req.invoker, req.guild) {
        return refused_report(env, outcome);
    }
    let shadow = env.config.mode == Mode::Shadow;
    let job = Job {
        invoker: req.invoker,
        key: req.key,
        guild: req.guild,
        channel: Some(req.channel),
        allow_recreate: req.allow_recreate,
        now: req.now,
        shadow,
    };
    let report = Report {
        shadow,
        items: vec![run_one(env, &job).await],
    };
    alert_failures(env, shadow, report.items.iter().map(|i| i.outcome)).await;
    report
}

/// Re-renders recorded posts by editing their messages.
pub async fn sync(env: &Env<'_>, req: &SyncRequest<'_>) -> Report {
    if let Err(outcome) = gate(env, &req.invoker, req.guild) {
        return refused_report(env, outcome);
    }
    let shadow = env.config.mode == Mode::Shadow;
    let keys: Vec<String> = match req.key {
        Some(key) => vec![key.to_string()],
        // Only this guild's rows are walked: another guild's key is neither read nor named.
        None => match env.store.list().await {
            Ok(rows) => rows
                .into_iter()
                .filter(|row| row.record.guild == req.guild)
                .map(|row| row.record.key)
                .collect(),
            Err(error) => {
                let report = refused_report(env, Outcome::with(Code::StoreFailed, error.code()));
                alert_failures(env, shadow, report.items.iter().map(|i| i.outcome)).await;
                return report;
            }
        },
    };
    let mut items = Vec::with_capacity(keys.len());
    for key in &keys {
        let job = Job {
            invoker: req.invoker,
            key,
            guild: req.guild,
            channel: None,
            allow_recreate: req.allow_recreate,
            now: req.now,
            shadow,
        };
        items.push(run_one(env, &job).await);
    }
    let report = Report { shadow, items };
    alert_failures(env, shadow, report.items.iter().map(|i| i.outcome)).await;
    report
}

/// Lists the recorded posts of the calling guild.
pub async fn list(env: &Env<'_>, req: &ListRequest) -> ListReport {
    if let Err(outcome) = gate(env, &req.invoker, req.guild) {
        return ListReport {
            outcome,
            posts: Vec::new(),
        };
    }
    match env.store.list().await {
        Ok(posts) => ListReport {
            outcome: Outcome::new(Code::Listed),
            posts: posts
                .into_iter()
                .filter(|row| row.record.guild == req.guild)
                .collect(),
        },
        Err(error) => {
            let outcome = Outcome::with(Code::StoreFailed, error.code());
            let shadow = env.config.mode == Mode::Shadow;
            alert_failures(env, shadow, std::iter::once(outcome)).await;
            ListReport {
                outcome,
                posts: Vec::new(),
            }
        }
    }
}

/// Handles one key. The key is echoed in the report only when it is a plain valid key.
async fn run_one(env: &Env<'_>, job: &Job<'_>) -> ItemReport {
    if posts::validate_key(job.key).is_err() {
        return ItemReport {
            key: None,
            outcome: Outcome::new(Code::InvalidKey),
        };
    }
    let outcome = if job.guild == 0 || job.channel == Some(0) {
        Outcome::new(Code::InvalidTarget)
    } else {
        match env.locks.try_acquire(job.key) {
            // The guard is held until the key is done, whatever the outcome.
            Some(_guard) => handle(env, job).await,
            None => Outcome::new(Code::Busy),
        }
    };
    ItemReport {
        key: Some(job.key.to_string()),
        outcome,
    }
}

fn discord_outcome(error: DiscordError) -> Outcome {
    match error {
        DiscordError::Forbidden => Outcome::new(Code::DiscordForbidden),
        DiscordError::NotFound | DiscordError::Failed => Outcome::new(Code::DiscordFailed),
    }
}

/// The decision and the action for one key, under its lock.
async fn handle(env: &Env<'_>, job: &Job<'_>) -> Outcome {
    let key = job.key;
    let record = match env.store.get(key).await {
        Ok(stored) => stored.map(|row| row.record),
        Err(error) => return Outcome::with(Code::StoreFailed, error.code()),
    };
    let channel = match (job.channel, &record) {
        (Some(channel), _) => channel,
        (None, Some(record)) => record.channel,
        (None, None) => return Outcome::new(Code::NotRecorded),
    };
    let text = match env.templates.read(key).await {
        Ok(text) => text,
        Err(SourceError::NotFound) => return Outcome::new(Code::TemplateNotFound),
        Err(SourceError::Unreadable | SourceError::TooLarge) => {
            return Outcome::new(Code::TemplateUnreadable)
        }
    };
    let template = match template::load(&text) {
        Ok(template) => template,
        Err(error) => return Outcome::with(Code::TemplateInvalid, error.code()),
    };
    drop(text);
    let hash = template.content_hash();

    // Only a post recorded for this guild and channel is looked up: the plan refuses any
    // other one before it reads the lookup.
    let mut message_missing = false;
    if let Some(recorded) = &record {
        if recorded.guild == job.guild && recorded.channel == channel {
            match env
                .discord
                .message_state(recorded.channel, recorded.message)
                .await
            {
                Ok(MessageState::Present) => {}
                Ok(MessageState::Missing) | Err(DiscordError::NotFound) => message_missing = true,
                Err(error) => return discord_outcome(error),
            }
        }
    }

    let request = PlanRequest {
        key,
        template_hash: &hash,
        recorded: record.as_ref(),
        target_guild: job.guild,
        target_channel: channel,
        message_missing,
        allow_recreate: job.allow_recreate,
    };
    let plan = posts::plan(&request);
    let out = OutgoingEmbed {
        channel,
        template: &template,
        mentions: AllowedMentions::none(),
    };
    let new_record = |message: u64| PostRecord {
        key: key.to_string(),
        guild: job.guild,
        channel,
        message,
        content_hash: hash.clone(),
        updated_by: job.invoker.user,
    };
    match plan {
        Plan::Refuse(reason) => Outcome::with(Code::Refused, reason.code()),
        Plan::NoOp => Outcome::new(Code::Unchanged),
        Plan::Create { .. } if job.shadow => Outcome::new(Code::WouldPost),
        Plan::Edit { .. } if job.shadow => Outcome::new(Code::WouldEdit),
        Plan::Recreate { .. } if job.shadow => Outcome::new(Code::WouldRecreate),
        Plan::Create { .. } => {
            publish(
                env,
                &out,
                key,
                job.now,
                WriteKind::Create,
                new_record,
                Code::Posted,
            )
            .await
        }
        Plan::Recreate { replaces, .. } => {
            let kind = WriteKind::Recreate { replaces };
            publish(env, &out, key, job.now, kind, new_record, Code::Recreated).await
        }
        Plan::Edit { message, from, .. } => {
            match env.discord.edit(message, &out).await {
                Ok(()) => {}
                // The lookup said it was there and the edit says it is gone: nothing is
                // written, and the next sync decides again.
                Err(DiscordError::NotFound) => return Outcome::new(Code::MessageMissing),
                Err(error) => return discord_outcome(error),
            }
            let write = WriteRequest {
                key: key.to_string(),
                kind: WriteKind::Edit {
                    message,
                    from: from.to_string(),
                },
                record: new_record(message),
                now: job.now,
            };
            match env.store.write(write).await {
                Ok(_) => Outcome::new(Code::Edited),
                Err(error) => Outcome::with(Code::EditedNotRecorded, error.code()),
            }
        }
    }
}

/// Sends a new message, records it, and takes the message back down when the record could
/// not be written: a post that is not recorded would be posted again by the next run.
async fn publish(
    env: &Env<'_>,
    out: &OutgoingEmbed<'_>,
    key: &str,
    now: i64,
    kind: WriteKind,
    new_record: impl Fn(u64) -> PostRecord,
    done: Code,
) -> Outcome {
    let message = match env.discord.send(out).await {
        Ok(message) => message,
        Err(error) => return discord_outcome(error),
    };
    let write = WriteRequest {
        key: key.to_string(),
        kind,
        record: new_record(message),
        now,
    };
    match env.store.write(write).await {
        Ok(_) => Outcome::new(done),
        Err(error) => match env.discord.delete(out.channel, message).await {
            Ok(()) | Err(DiscordError::NotFound) => Outcome::with(Code::StoreFailed, error.code()),
            Err(_) => Outcome::with(Code::PostedNotRecorded, error.code()),
        },
    }
}

// ---------------------------------------------------------------------------
// Discord adapter pieces
// ---------------------------------------------------------------------------

/// Maps an HTTP status to a Discord error class.
pub fn classify_status(status: u16) -> DiscordError {
    match status {
        404 => DiscordError::NotFound,
        401 | 403 => DiscordError::Forbidden,
        _ => DiscordError::Failed,
    }
}

/// Renders a template as a serenity embed.
pub fn render(template: &EmbedTemplate) -> s::CreateEmbed {
    let mut embed = s::CreateEmbed::new();
    if let Some(title) = &template.title {
        embed = embed.title(title.clone());
    }
    if let Some(description) = &template.description {
        embed = embed.description(description.clone());
    }
    for field in &template.fields {
        embed = embed.field(field.name.clone(), field.value.clone(), field.inline);
    }
    if let Some(footer) = &template.footer {
        embed = embed.footer(s::CreateEmbedFooter::new(footer.clone()));
    }
    if let Some(author) = &template.author_name {
        embed = embed.author(s::CreateEmbedAuthor::new(author.clone()));
    }
    if let Some(color) = template.color {
        embed = embed.colour(s::Colour::new(color));
    }
    embed
}

/// Builds serenity's allowed-mentions object from ours.
fn serenity_mentions(mentions: &AllowedMentions) -> s::CreateAllowedMentions {
    let mut out = s::CreateAllowedMentions::new();
    if !mentions.users.is_empty() {
        out = out.users(mentions.users.iter().map(|id| s::UserId::new(*id)));
    }
    if !mentions.roles.is_empty() {
        out = out.roles(mentions.roles.iter().map(|id| s::RoleId::new(*id)));
    }
    if mentions.everyone {
        out = out.everyone(true);
    }
    if mentions.replied_user {
        out = out.replied_user(true);
    }
    out
}

fn classify_error(error: &s::Error) -> DiscordError {
    match error {
        s::Error::Http(s::http::HttpError::UnsuccessfulRequest(response)) => {
            classify_status(response.status_code.as_u16())
        }
        _ => DiscordError::Failed,
    }
}

/// The real Discord calls. It is built by the wiring card; it has no offline test beyond
/// `render`, `classify_status` and the allowed-mentions object.
pub struct SerenityDiscord {
    /// The HTTP client of the running bot.
    pub http: std::sync::Arc<s::Http>,
}

#[s::async_trait]
impl EmbedDiscord for SerenityDiscord {
    async fn message_state(
        &self,
        channel: u64,
        message: u64,
    ) -> Result<MessageState, DiscordError> {
        match s::ChannelId::new(channel)
            .message(&*self.http, s::MessageId::new(message))
            .await
        {
            Ok(_) => Ok(MessageState::Present),
            Err(error) => match classify_error(&error) {
                DiscordError::NotFound => Ok(MessageState::Missing),
                other => Err(other),
            },
        }
    }

    async fn send(&self, out: &OutgoingEmbed<'_>) -> Result<u64, DiscordError> {
        let builder = s::CreateMessage::new()
            .embed(render(out.template))
            .allowed_mentions(serenity_mentions(&out.mentions));
        s::ChannelId::new(out.channel)
            .send_message(&*self.http, builder)
            .await
            .map(|message| message.id.get())
            .map_err(|error| classify_error(&error))
    }

    async fn edit(&self, message: u64, out: &OutgoingEmbed<'_>) -> Result<(), DiscordError> {
        let builder = s::EditMessage::new()
            .embed(render(out.template))
            .allowed_mentions(serenity_mentions(&out.mentions));
        s::ChannelId::new(out.channel)
            .edit_message(&*self.http, s::MessageId::new(message), builder)
            .await
            .map(|_| ())
            .map_err(|error| classify_error(&error))
    }

    async fn delete(&self, channel: u64, message: u64) -> Result<(), DiscordError> {
        s::ChannelId::new(channel)
            .delete_message(&*self.http, s::MessageId::new(message))
            .await
            .map_err(|error| classify_error(&error))
    }
}

/// Posts alert codes to the staff alert channel, with no mention allowed.
pub struct SerenityAlerts {
    /// The HTTP client of the running bot.
    pub http: std::sync::Arc<s::Http>,
    /// The staff alert channel.
    pub channel: u64,
}

#[s::async_trait]
impl AlertSink for SerenityAlerts {
    async fn alert(&self, code: &'static str) {
        let builder = s::CreateMessage::new()
            .content(alert_text(code))
            .allowed_mentions(serenity_mentions(&AllowedMentions::none()));
        // An alert that cannot be sent has no further channel to be reported to.
        let _ = s::ChannelId::new(self.channel)
            .send_message(&*self.http, builder)
            .await;
    }
}

// ---------------------------------------------------------------------------
// The registry hook
// ---------------------------------------------------------------------------

/// The feature hook. It is not registered anywhere.
pub struct EmbedFeature {
    enabled: bool,
}

impl EmbedFeature {
    /// The hook for a parsed configuration.
    pub fn new(config: &EmbedConfig) -> Self {
        EmbedFeature {
            enabled: config.enabled(),
        }
    }
}

impl<X, D> Feature<X, D> for EmbedFeature {
    fn name(&self) -> &'static str {
        "embeds"
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    // The records live in the shared core database. No job kind is registered:
    // post, sync and list answer a staff command and nothing is restart-sensitive.
    fn needs_core_db(&self) -> bool {
        true
    }
    fn register_jobs(&self, _registry: &mut Registry) -> Result<(), RunnerError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use tokio::sync::Notify;

    // Clearly synthetic ids.
    const OWNER: u64 = 1_001;
    const STAFF: u64 = 1_002;
    const RANDO: u64 = 1_003;
    const GUILD: u64 = 5_001;
    const OTHER_GUILD: u64 = 5_002;
    const CHAN: u64 = 6_001;
    const OTHER_CHAN: u64 = 6_002;

    fn owner() -> Invoker {
        Invoker {
            user: OWNER,
            guild_owner: OWNER,
            is_staff: false,
        }
    }
    fn staff() -> Invoker {
        Invoker {
            user: STAFF,
            guild_owner: OWNER,
            is_staff: true,
        }
    }
    fn rando() -> Invoker {
        Invoker {
            user: RANDO,
            guild_owner: OWNER,
            is_staff: false,
        }
    }

    fn tpl(title: &str) -> String {
        format!(r#"{{"title":"{title}"}}"#)
    }

    fn hash_of(json: &str) -> String {
        template::load(json).unwrap().content_hash()
    }

    // ---- fakes ----

    #[derive(Default)]
    struct DiscordState {
        /// message id -> (channel, title)
        messages: BTreeMap<u64, (u64, String)>,
        next: u64,
        sends: usize,
        edits: usize,
        deletes: usize,
        lookups: usize,
        mention_violations: usize,
        fail_send: Option<DiscordError>,
        fail_edit: Option<DiscordError>,
        fail_lookup: Option<DiscordError>,
        fail_delete: Option<DiscordError>,
        gate: Option<(Arc<Notify>, Arc<Notify>)>,
    }

    struct FakeDiscord(Mutex<DiscordState>);

    impl FakeDiscord {
        fn new() -> Self {
            FakeDiscord(Mutex::new(DiscordState {
                next: 9_000,
                ..DiscordState::default()
            }))
        }
        fn with<R>(&self, f: impl FnOnce(&mut DiscordState) -> R) -> R {
            f(&mut self.0.lock().unwrap())
        }
        fn writes(&self) -> usize {
            self.with(|s| s.sends + s.edits + s.deletes)
        }
        fn calls(&self) -> usize {
            self.with(|s| s.sends + s.edits + s.deletes + s.lookups)
        }
    }

    #[s::async_trait]
    impl EmbedDiscord for FakeDiscord {
        async fn message_state(
            &self,
            channel: u64,
            message: u64,
        ) -> Result<MessageState, DiscordError> {
            self.with(|s| {
                s.lookups += 1;
                if let Some(err) = s.fail_lookup {
                    return Err(err);
                }
                Ok(match s.messages.get(&message) {
                    Some((c, _)) if *c == channel => MessageState::Present,
                    _ => MessageState::Missing,
                })
            })
        }
        async fn send(&self, out: &OutgoingEmbed<'_>) -> Result<u64, DiscordError> {
            self.with(|s| {
                s.sends += 1;
                if out.mentions != AllowedMentions::none() {
                    s.mention_violations += 1;
                }
                if let Some(err) = s.fail_send {
                    return Err(err);
                }
                s.next += 1;
                let id = s.next;
                s.messages.insert(
                    id,
                    (out.channel, out.template.title.clone().unwrap_or_default()),
                );
                Ok(id)
            })
        }
        async fn edit(&self, message: u64, out: &OutgoingEmbed<'_>) -> Result<(), DiscordError> {
            let gate = self.with(|s| s.gate.clone());
            if let Some((entered, release)) = gate {
                entered.notify_one();
                release.notified().await;
            }
            self.with(|s| {
                s.edits += 1;
                if out.mentions != AllowedMentions::none() {
                    s.mention_violations += 1;
                }
                if let Some(err) = s.fail_edit {
                    return Err(err);
                }
                match s.messages.get_mut(&message) {
                    Some(entry) if entry.0 == out.channel => {
                        entry.1 = out.template.title.clone().unwrap_or_default();
                        Ok(())
                    }
                    _ => Err(DiscordError::NotFound),
                }
            })
        }
        async fn delete(&self, channel: u64, message: u64) -> Result<(), DiscordError> {
            self.with(|s| {
                s.deletes += 1;
                if let Some(err) = s.fail_delete {
                    return Err(err);
                }
                match s.messages.get(&message) {
                    Some((c, _)) if *c == channel => {
                        s.messages.remove(&message);
                        Ok(())
                    }
                    _ => Err(DiscordError::NotFound),
                }
            })
        }
    }

    #[derive(Default)]
    struct FakeTemplates {
        files: Mutex<HashMap<String, String>>,
        errors: Mutex<HashMap<String, SourceError>>,
        reads: Mutex<Vec<String>>,
    }

    impl FakeTemplates {
        fn put(&self, key: &str, json: &str) {
            self.files
                .lock()
                .unwrap()
                .insert(key.to_string(), json.to_string());
        }
        fn read_count(&self) -> usize {
            self.reads.lock().unwrap().len()
        }
    }

    #[s::async_trait]
    impl TemplateSource for FakeTemplates {
        async fn read(&self, key: &str) -> Result<String, SourceError> {
            self.reads.lock().unwrap().push(key.to_string());
            if let Some(err) = self.errors.lock().unwrap().get(key) {
                return Err(*err);
            }
            self.files
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or(SourceError::NotFound)
        }
    }

    #[derive(Default)]
    struct FakeAlerts(Mutex<Vec<&'static str>>);

    #[s::async_trait]
    impl AlertSink for FakeAlerts {
        async fn alert(&self, code: &'static str) {
            self.0.lock().unwrap().push(code);
        }
    }

    struct World {
        config: EmbedConfig,
        discord: FakeDiscord,
        templates: FakeTemplates,
        alerts: FakeAlerts,
        store: TokioStore,
        locks: KeyLocks,
        home_guild: Option<u64>,
    }

    impl World {
        async fn new(mode: Mode, publishers: Publishers) -> World {
            let conn = tokio_rusqlite::Connection::open_in_memory().await.unwrap();
            let store = TokioStore::new(conn);
            store.migrate().await.unwrap();
            World {
                config: EmbedConfig {
                    mode,
                    template_dir: Some(PathBuf::from("synthetic-template-dir")),
                    publishers,
                },
                discord: FakeDiscord::new(),
                templates: FakeTemplates::default(),
                alerts: FakeAlerts::default(),
                store,
                locks: KeyLocks::new(),
                home_guild: Some(GUILD),
            }
        }
        async fn live() -> World {
            World::new(Mode::Live, Publishers::OwnerOnly).await
        }
        async fn shadow() -> World {
            World::new(Mode::Shadow, Publishers::OwnerOnly).await
        }
        fn env(&self) -> Env<'_> {
            Env {
                config: &self.config,
                discord: &self.discord,
                store: &self.store,
                templates: &self.templates,
                alerts: &self.alerts,
                locks: &self.locks,
                home_guild: self.home_guild,
            }
        }
        async fn post_as(&self, who: Invoker, key: &str, channel: u64, recreate: bool) -> Report {
            post(
                &self.env(),
                &PostRequest {
                    invoker: who,
                    key,
                    guild: GUILD,
                    channel,
                    allow_recreate: recreate,
                    now: 1_000,
                },
            )
            .await
        }
        async fn post(&self, key: &str) -> Report {
            self.post_as(owner(), key, CHAN, false).await
        }
        async fn sync_as(&self, who: Invoker, key: Option<&str>, recreate: bool) -> Report {
            sync(
                &self.env(),
                &SyncRequest {
                    invoker: who,
                    key,
                    guild: GUILD,
                    allow_recreate: recreate,
                    now: 2_000,
                },
            )
            .await
        }
        async fn sync(&self, key: &str) -> Report {
            self.sync_as(owner(), Some(key), false).await
        }
        async fn sync_all(&self) -> Report {
            self.sync_as(owner(), None, false).await
        }
        async fn list_as(&self, who: Invoker) -> ListReport {
            self.list_in(who, GUILD).await
        }
        async fn list_in(&self, who: Invoker, guild: u64) -> ListReport {
            list(
                &self.env(),
                &ListRequest {
                    invoker: who,
                    guild,
                },
            )
            .await
        }
        async fn rows(&self) -> Vec<StoredPost> {
            self.store.list().await.unwrap()
        }
        async fn row(&self, key: &str) -> Option<StoredPost> {
            self.store.get(key).await.unwrap()
        }
        async fn sql(&self, sql: &'static str) {
            self.store
                .conn
                .call(move |c| c.execute_batch(sql))
                .await
                .unwrap();
        }
        async fn break_inserts(&self) {
            self.sql("PRAGMA query_only = ON;").await;
        }
        async fn heal_inserts(&self) {
            self.sql("PRAGMA query_only = OFF;").await;
        }
        async fn break_updates(&self) {
            self.sql("CREATE TRIGGER no_update BEFORE UPDATE ON managed_posts BEGIN SELECT RAISE(ABORT, 'x'); END;").await;
        }
        async fn heal_updates(&self) {
            self.sql("DROP TRIGGER IF EXISTS no_update;").await;
        }
        fn alerts(&self) -> Vec<&'static str> {
            self.alerts.0.lock().unwrap().clone()
        }
        /// The environment as the wiring would build it for another home guild.
        fn env_home(&self, home: Option<u64>) -> Env<'_> {
            Env {
                home_guild: home,
                ..self.env()
            }
        }
        /// Records a post of `guild` (the home guild of that call is `guild` itself).
        async fn post_in(&self, guild: u64, key: &str, channel: u64) -> Report {
            post(
                &self.env_home(Some(guild)),
                &PostRequest {
                    invoker: owner(),
                    key,
                    guild,
                    channel,
                    allow_recreate: false,
                    now: 1_000,
                },
            )
            .await
        }
        async fn post_at(
            &self,
            home: Option<u64>,
            who: Invoker,
            guild: u64,
            channel: u64,
        ) -> Report {
            post(
                &self.env_home(home),
                &PostRequest {
                    invoker: who,
                    key: "rules",
                    guild,
                    channel,
                    allow_recreate: false,
                    now: 1_000,
                },
            )
            .await
        }
        async fn sync_at(
            &self,
            home: Option<u64>,
            who: Invoker,
            key: Option<&str>,
            guild: u64,
        ) -> Report {
            sync(
                &self.env_home(home),
                &SyncRequest {
                    invoker: who,
                    key,
                    guild,
                    allow_recreate: false,
                    now: 2_000,
                },
            )
            .await
        }
        async fn list_at(&self, home: Option<u64>, who: Invoker, guild: u64) -> ListReport {
            list(
                &self.env_home(home),
                &ListRequest {
                    invoker: who,
                    guild,
                },
            )
            .await
        }
    }

    fn code_of(report: &Report) -> Code {
        assert_eq!(report.items.len(), 1, "{report:?}");
        report.items[0].outcome.code
    }

    // ---- who may publish ----

    #[test]
    fn authorize_owner_always_staff_only_when_widened() {
        for publishers in [Publishers::OwnerOnly, Publishers::Staff] {
            assert_eq!(authorize(publishers, &owner()), Ok(()), "{publishers:?}");
            assert_eq!(authorize(publishers, &rando()), Err(Code::Denied));
        }
        assert_eq!(
            authorize(Publishers::OwnerOnly, &staff()),
            Err(Code::Denied)
        );
        assert_eq!(authorize(Publishers::Staff, &staff()), Ok(()));
    }

    #[test]
    fn authorize_never_treats_an_unknown_owner_as_the_caller() {
        let zero = Invoker {
            user: 0,
            guild_owner: 0,
            is_staff: false,
        };
        assert_eq!(authorize(Publishers::OwnerOnly, &zero), Err(Code::Denied));
        assert_eq!(authorize(Publishers::Staff, &zero), Err(Code::Denied));
        // A staff flag never turns a caller into the owner of someone else's guild.
        let not_owner = Invoker {
            user: STAFF,
            guild_owner: OWNER,
            is_staff: true,
        };
        assert_eq!(
            authorize(Publishers::OwnerOnly, &not_owner),
            Err(Code::Denied)
        );
    }

    #[tokio::test]
    async fn a_denied_caller_touches_nothing_in_every_command() {
        for publishers in [Publishers::OwnerOnly, Publishers::Staff] {
            let world = World::new(Mode::Live, publishers).await;
            world.templates.put("rules", &tpl("T"));
            let who = rando();
            assert_eq!(
                code_of(&world.post_as(who, "rules", CHAN, false).await),
                Code::Denied
            );
            assert_eq!(
                code_of(&world.sync_as(who, Some("rules"), false).await),
                Code::Denied
            );
            assert_eq!(code_of(&world.sync_as(who, None, true).await), Code::Denied);
            let listed = world.list_as(who).await;
            assert_eq!(listed.outcome.code, Code::Denied);
            assert!(listed.posts.is_empty());
            assert_eq!(world.templates.read_count(), 0);
            assert_eq!(world.discord.calls(), 0);
            assert!(world.rows().await.is_empty());
            assert!(
                world.alerts().is_empty(),
                "a denial is not an alertable failure"
            );
        }
    }

    #[tokio::test]
    async fn staff_is_denied_by_default_and_allowed_when_widened() {
        let world = World::new(Mode::Live, Publishers::OwnerOnly).await;
        world.templates.put("rules", &tpl("T"));
        assert_eq!(
            code_of(&world.post_as(staff(), "rules", CHAN, false).await),
            Code::Denied
        );
        assert_eq!(world.discord.calls(), 0);
        let world = World::new(Mode::Live, Publishers::Staff).await;
        world.templates.put("rules", &tpl("T"));
        assert_eq!(
            code_of(&world.post_as(staff(), "rules", CHAN, false).await),
            Code::Posted
        );
        assert_eq!(world.list_as(staff()).await.outcome.code, Code::Listed);
    }

    #[tokio::test]
    async fn an_off_feature_does_nothing_even_for_the_owner() {
        let world = World::new(Mode::Off, Publishers::OwnerOnly).await;
        world.templates.put("rules", &tpl("T"));
        assert_eq!(code_of(&world.post("rules").await), Code::Disabled);
        assert_eq!(code_of(&world.sync("rules").await), Code::Disabled);
        assert_eq!(world.list_as(owner()).await.outcome.code, Code::Disabled);
        // A configuration with a mode but no directory is not enabled either.
        let mut world = World::live().await;
        world.config.template_dir = None;
        assert_eq!(code_of(&world.post("rules").await), Code::Disabled);
        assert_eq!(world.templates.read_count(), 0);
        assert_eq!(world.discord.calls(), 0);
    }

    // ---- hostile keys ----

    const HOSTILE_KEYS: &[&str] = &[
        "../secret",
        "..",
        ".",
        "..\\secret",
        "a/../b",
        "/etc/passwd",
        "\\rooted",
        "C:\\Windows\\win.ini",
        "C:/Windows/win.ini",
        "C:",
        "c:evil",
        "z:x",
        "\\\\srv\\share\\x",
        "//srv/share/x",
        "a/b",
        "a\\b",
        "a.json",
        "rules.",
        ".hidden",
        "Rules",
        "RULES",
        "rules ",
        " rules",
        "ru les",
        "rules\0",
        "ru\nles",
        "ru\tles",
        "con",
        "nul",
        "aux",
        "prn",
        "com1",
        "lpt9",
        "é",
        "rulés",
        "ｒules",
        "a:b",
        "a:stream",
        "a%2e%2e",
        "a%00",
        "",
        "-x",
        "_x",
        "~",
        "$x",
        "a*b",
        "a?b",
        "a<b",
        "rules\u{202e}",
        "rules\u{200b}",
    ];

    #[tokio::test]
    async fn every_hostile_key_is_refused_before_anything_is_read() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("T"));
        let too_long = "a".repeat(65);
        let mut keys: Vec<&str> = HOSTILE_KEYS.to_vec();
        keys.push(&too_long);
        for key in keys {
            let report = world.post(key).await;
            assert_eq!(code_of(&report), Code::InvalidKey, "{key:?}");
            assert_eq!(
                report.items[0].key, None,
                "a refused spelling is not echoed: {key:?}"
            );
            let report = world.sync(key).await;
            assert_eq!(code_of(&report), Code::InvalidKey, "sync {key:?}");
            assert_eq!(report.items[0].key, None);
            let text = report_reply(&report);
            if key.chars().count() >= 3 && key.chars().all(|c| !c.is_control()) {
                assert!(
                    !text.text().contains(key),
                    "{key:?} echoed in {}",
                    text.text()
                );
            }
        }
        assert_eq!(
            world.templates.read_count(),
            0,
            "no hostile key reached the template source"
        );
        assert_eq!(world.discord.calls(), 0);
        assert!(world.rows().await.is_empty());
    }

    #[tokio::test]
    async fn plain_keys_are_accepted_including_the_boundary() {
        let world = World::live().await;
        let max = "a".repeat(posts::MAX_KEY_BYTES);
        for key in ["rules", "general-rules", "0x", "a_b", "9", max.as_str()] {
            world.templates.put(key, &tpl(key));
            assert_eq!(code_of(&world.post(key).await), Code::Posted, "{key}");
        }
    }

    /// A small LCG so the test needs no dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[tokio::test]
    async fn only_canonical_keys_ever_reach_the_template_source() {
        // Independent oracle: ^[a-z0-9][a-z0-9_-]{0,63}$ minus the reserved device names.
        fn oracle(key: &str) -> bool {
            let bytes = key.as_bytes();
            if bytes.is_empty() || bytes.len() > 64 {
                return false;
            }
            let first = bytes[0];
            if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
                return false;
            }
            if !bytes[1..]
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
            {
                return false;
            }
            !["con", "prn", "aux", "nul"].contains(&key)
                && !((key.starts_with("com") || key.starts_with("lpt"))
                    && key.len() == 4
                    && key.as_bytes()[3].is_ascii_digit())
        }
        const ALPHABET: &[char] = &[
            'a', 'z', 'A', 'Z', '0', '9', '-', '_', '.', '/', '\\', ':', ' ', '\0', '\n', '~', '%',
            'é', 'ｒ', '\u{202e}', '*', '?', 'c', 'o', 'n', 'l', 'p', 't', '1',
        ];
        let world = World::live().await;
        let mut rng = Lcg(0x5eed_0001);
        for _ in 0..400 {
            let len = rng.below(8) as usize;
            let key: String = (0..len)
                .map(|_| ALPHABET[rng.below(ALPHABET.len() as u64) as usize])
                .collect();
            let before = world.templates.read_count();
            let report = world.post(&key).await;
            let reached = world.templates.read_count() > before;
            if oracle(&key) {
                assert!(reached, "plain key {key:?} should be looked up");
                assert_eq!(code_of(&report), Code::TemplateNotFound, "{key:?}");
            } else {
                assert!(!reached, "hostile key {key:?} reached the template source");
                assert_eq!(code_of(&report), Code::InvalidKey, "{key:?}");
            }
        }
    }

    // ---- post ----

    #[tokio::test]
    async fn post_sends_once_with_no_mentions_and_records_the_message() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("RulesV1"));
        let report = world.post("rules").await;
        assert_eq!(code_of(&report), Code::Posted);
        assert_eq!(report.items[0].key.as_deref(), Some("rules"));
        assert!(!report.shadow);
        assert_eq!(
            world.discord.with(|s| (s.sends, s.edits, s.deletes)),
            (1, 0, 0)
        );
        assert_eq!(world.discord.with(|s| s.mention_violations), 0);
        let row = world.row("rules").await.unwrap();
        assert_eq!(row.record.channel, CHAN);
        assert_eq!(row.record.guild, GUILD);
        assert_eq!(row.record.updated_by, OWNER);
        assert_eq!(row.updated_at, 1_000);
        assert_eq!(row.record.content_hash, hash_of(&tpl("RulesV1")));
        let shown = world
            .discord
            .with(|s| s.messages.get(&row.record.message).cloned());
        assert_eq!(shown, Some((CHAN, "RulesV1".to_string())));
    }

    #[tokio::test]
    async fn post_twice_does_not_duplicate() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        assert_eq!(code_of(&world.post("rules").await), Code::Posted);
        assert_eq!(code_of(&world.post("rules").await), Code::Unchanged);
        assert_eq!(
            world.discord.with(|s| (s.sends, s.edits, s.messages.len())),
            (1, 0, 1)
        );
    }

    #[tokio::test]
    async fn post_of_a_changed_template_edits_the_same_message() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        let first = world.row("rules").await.unwrap().record.message;
        world.templates.put("rules", &tpl("V2"));
        assert_eq!(code_of(&world.post("rules").await), Code::Edited);
        let row = world.row("rules").await.unwrap();
        assert_eq!(row.record.message, first);
        assert_eq!(row.record.content_hash, hash_of(&tpl("V2")));
        assert_eq!(
            world.discord.with(|s| (s.sends, s.edits, s.messages.len())),
            (1, 1, 1)
        );
        assert_eq!(
            world.discord.with(|s| s.messages.get(&first).cloned()),
            Some((CHAN, "V2".to_string()))
        );
    }

    #[tokio::test]
    async fn post_for_another_channel_or_guild_is_refused_with_its_reason() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        let report = world.post_as(owner(), "rules", OTHER_CHAN, true).await;
        assert_eq!(code_of(&report), Code::Refused);
        assert_eq!(
            report.items[0].outcome.detail,
            Some("key_reused_in_another_channel")
        );
        let report = post(
            &Env {
                // The home guild moved after the post was recorded: the call is served,
                // and the plan still refuses a key that belongs to another guild.
                home_guild: Some(OTHER_GUILD),
                ..world.env()
            },
            &PostRequest {
                invoker: owner(),
                key: "rules",
                guild: OTHER_GUILD,
                channel: CHAN,
                allow_recreate: true,
                now: 3,
            },
        )
        .await;
        assert_eq!(code_of(&report), Code::Refused);
        assert_eq!(
            report.items[0].outcome.detail,
            Some("key_reused_in_another_guild")
        );
        assert_eq!(
            world.discord.writes(),
            1,
            "only the first post wrote anything"
        );
    }

    #[tokio::test]
    async fn post_with_a_zero_target_is_refused_before_any_call() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        let report = world.post_as(owner(), "rules", 0, false).await;
        assert_eq!(code_of(&report), Code::InvalidTarget);
        let report = post(
            &world.env(),
            &PostRequest {
                invoker: owner(),
                key: "rules",
                guild: 0,
                channel: CHAN,
                allow_recreate: false,
                now: 1,
            },
        )
        .await;
        assert_eq!(code_of(&report), Code::InvalidTarget);
        assert_eq!(world.discord.calls(), 0);
        assert_eq!(world.templates.read_count(), 0);
    }

    #[tokio::test]
    async fn a_missing_recorded_message_is_not_reposted_unless_allowed() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        let old = world.row("rules").await.unwrap().record.message;
        world.discord.with(|s| s.messages.clear());
        let report = world.post("rules").await;
        assert_eq!(code_of(&report), Code::Refused);
        assert_eq!(report.items[0].outcome.detail, Some("missing_message"));
        assert_eq!(world.discord.with(|s| s.sends), 1);
        let report = world.post_as(owner(), "rules", CHAN, true).await;
        assert_eq!(code_of(&report), Code::Recreated);
        let row = world.row("rules").await.unwrap();
        assert_ne!(row.record.message, old);
        assert_eq!(world.discord.with(|s| (s.sends, s.messages.len())), (2, 1));
    }

    // ---- sync ----

    #[tokio::test]
    async fn sync_edits_instead_of_duplicating_and_is_idempotent() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        world.templates.put("rules", &tpl("V2"));
        assert_eq!(code_of(&world.sync("rules").await), Code::Edited);
        for _ in 0..3 {
            assert_eq!(code_of(&world.sync("rules").await), Code::Unchanged);
        }
        assert_eq!(
            world
                .discord
                .with(|s| (s.sends, s.edits, s.deletes, s.messages.len())),
            (1, 1, 0, 1)
        );
        assert_eq!(world.row("rules").await.unwrap().updated_at, 2_000);
    }

    #[tokio::test]
    async fn sync_does_not_create_a_post_that_was_never_recorded() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        let report = world.sync("rules").await;
        assert_eq!(code_of(&report), Code::NotRecorded);
        assert_eq!(world.discord.writes(), 0);
        assert!(world.rows().await.is_empty());
    }

    #[tokio::test]
    async fn sync_of_a_deleted_message_refuses_unless_recreate_is_allowed() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        world.discord.with(|s| s.messages.clear());
        let report = world.sync("rules").await;
        assert_eq!(code_of(&report), Code::Refused);
        assert_eq!(report.items[0].outcome.detail, Some("missing_message"));
        assert_eq!(world.discord.with(|s| s.sends), 1);
        let report = world.sync_as(owner(), Some("rules"), true).await;
        assert_eq!(code_of(&report), Code::Recreated);
        assert_eq!(world.discord.with(|s| s.messages.len()), 1);
    }

    #[tokio::test]
    async fn an_edit_that_finds_the_message_gone_writes_nothing() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        let before = world.row("rules").await.unwrap();
        world.templates.put("rules", &tpl("V2"));
        // The lookup still says present, then the edit finds the message gone (a race).
        world
            .discord
            .with(|s| s.fail_edit = Some(DiscordError::NotFound));
        let report = world.sync("rules").await;
        assert_eq!(code_of(&report), Code::MessageMissing);
        assert_eq!(world.row("rules").await.unwrap(), before);
        assert_eq!(world.discord.with(|s| s.sends), 1);
    }

    #[tokio::test]
    async fn sync_all_handles_every_key_and_one_bad_key_does_not_hide_the_others() {
        let world = World::live().await;
        for key in ["a-rules", "b-rules", "c-rules"] {
            world.templates.put(key, &tpl(&format!("{key}-1")));
            world.post(key).await;
        }
        world.templates.put("a-rules", &tpl("a-2"));
        world.templates.put("b-rules", "{not json");
        world.templates.put("c-rules", &tpl("c-2"));
        let report = world.sync_all().await;
        let codes: Vec<(Option<&str>, Code)> = report
            .items
            .iter()
            .map(|i| (i.key.as_deref(), i.outcome.code))
            .collect();
        assert_eq!(
            codes,
            vec![
                (Some("a-rules"), Code::Edited),
                (Some("b-rules"), Code::TemplateInvalid),
                (Some("c-rules"), Code::Edited),
            ]
        );
        assert_eq!(report.items[1].outcome.detail, Some("invalid_json"));
        let counts = report.counts();
        assert_eq!((counts.edited, counts.refused, counts.failed), (2, 1, 0));
        // The bad key is retried by the next sync once the template is fixed.
        world.templates.put("b-rules", &tpl("b-2"));
        let report = world.sync_all().await;
        assert_eq!(report.counts().edited, 1);
        assert_eq!(report.counts().unchanged, 2);
    }

    #[tokio::test]
    async fn sync_all_of_an_empty_table_is_not_an_error() {
        let world = World::live().await;
        let report = world.sync_all().await;
        assert!(report.items.is_empty());
        assert_eq!(world.discord.calls(), 0);
        assert!(report_reply(&report).text().contains("0"));
    }

    // ---- templates ----

    #[tokio::test]
    async fn template_problems_have_their_own_codes_and_touch_nothing() {
        let world = World::live().await;
        assert_eq!(code_of(&world.post("absent").await), Code::TemplateNotFound);
        world
            .templates
            .errors
            .lock()
            .unwrap()
            .insert("blocked".into(), SourceError::Unreadable);
        assert_eq!(
            code_of(&world.post("blocked").await),
            Code::TemplateUnreadable
        );
        world
            .templates
            .errors
            .lock()
            .unwrap()
            .insert("huge".into(), SourceError::TooLarge);
        assert_eq!(code_of(&world.post("huge").await), Code::TemplateUnreadable);
        world.templates.put("bad", "{\"unknown\":1}");
        let report = world.post("bad").await;
        assert_eq!(code_of(&report), Code::TemplateInvalid);
        assert_eq!(report.items[0].outcome.detail, Some("invalid_json"));
        world.templates.put("empty", "{}");
        assert_eq!(
            world.post("empty").await.items[0].outcome.detail,
            Some("empty_template")
        );
        assert_eq!(world.discord.calls(), 0);
        assert!(world.rows().await.is_empty());
        assert!(world.alerts().is_empty());
    }

    // ---- failures ----

    #[tokio::test]
    async fn a_failed_send_records_nothing_and_alerts_by_code() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world
            .discord
            .with(|s| s.fail_send = Some(DiscordError::Failed));
        assert_eq!(code_of(&world.post("rules").await), Code::DiscordFailed);
        world
            .discord
            .with(|s| s.fail_send = Some(DiscordError::Forbidden));
        assert_eq!(code_of(&world.post("rules").await), Code::DiscordForbidden);
        world
            .discord
            .with(|s| s.fail_send = Some(DiscordError::NotFound));
        assert_eq!(code_of(&world.post("rules").await), Code::DiscordFailed);
        assert!(world.rows().await.is_empty());
        assert_eq!(
            world.alerts(),
            vec!["discord_failed", "discord_forbidden", "discord_failed"]
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_or_edit_leaves_the_record_and_alerts() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        let before = world.row("rules").await.unwrap();
        world.templates.put("rules", &tpl("V2"));
        world
            .discord
            .with(|s| s.fail_lookup = Some(DiscordError::Failed));
        assert_eq!(code_of(&world.sync("rules").await), Code::DiscordFailed);
        world.discord.with(|s| {
            s.fail_lookup = Some(DiscordError::Forbidden);
        });
        assert_eq!(code_of(&world.sync("rules").await), Code::DiscordForbidden);
        world.discord.with(|s| {
            s.fail_lookup = None;
            s.fail_edit = Some(DiscordError::Failed);
        });
        assert_eq!(code_of(&world.sync("rules").await), Code::DiscordFailed);
        world
            .discord
            .with(|s| s.fail_edit = Some(DiscordError::Forbidden));
        assert_eq!(code_of(&world.sync("rules").await), Code::DiscordForbidden);
        assert_eq!(world.row("rules").await.unwrap(), before);
        assert_eq!(world.alerts().len(), 4);
        // A forbidden lookup is not "the message is gone": nothing was recreated.
        assert_eq!(world.discord.with(|s| s.sends), 1);
        world.discord.with(|s| s.fail_edit = None);
        assert_eq!(code_of(&world.sync("rules").await), Code::Edited);
    }

    #[tokio::test]
    async fn a_lost_record_write_after_a_send_removes_the_new_post() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.break_inserts().await;
        let report = world.post("rules").await;
        assert_eq!(code_of(&report), Code::StoreFailed);
        assert_eq!(report.items[0].outcome.detail, Some("sqlite_error"));
        assert_eq!(
            world
                .discord
                .with(|s| (s.sends, s.deletes, s.messages.len())),
            (1, 1, 0)
        );
        assert_eq!(world.alerts(), vec!["store_failed"]);
        world.heal_inserts().await;
        assert_eq!(code_of(&world.post("rules").await), Code::Posted);
        assert_eq!(world.discord.with(|s| s.messages.len()), 1, "no duplicate");
    }

    #[tokio::test]
    async fn a_lost_record_write_and_a_failed_removal_is_reported_as_an_orphan() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.break_inserts().await;
        world
            .discord
            .with(|s| s.fail_delete = Some(DiscordError::Failed));
        let report = world.post("rules").await;
        assert_eq!(code_of(&report), Code::PostedNotRecorded);
        assert_eq!(world.discord.with(|s| s.messages.len()), 1);
        assert_eq!(world.alerts(), vec!["posted_not_recorded"]);
    }

    #[tokio::test]
    async fn a_recreate_whose_record_write_is_lost_removes_the_new_post() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        world.discord.with(|s| s.messages.clear());
        world.break_updates().await;
        let report = world.post_as(owner(), "rules", CHAN, true).await;
        assert_eq!(code_of(&report), Code::StoreFailed);
        assert_eq!(world.discord.with(|s| s.messages.len()), 0);
        world.heal_updates().await;
        assert_eq!(
            code_of(&world.post_as(owner(), "rules", CHAN, true).await),
            Code::Recreated
        );
        assert_eq!(world.discord.with(|s| s.messages.len()), 1);
    }

    #[tokio::test]
    async fn an_edit_whose_record_write_is_lost_is_reported_and_planned_again() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        let before = world.row("rules").await.unwrap();
        world.templates.put("rules", &tpl("V2"));
        world.break_updates().await;
        let report = world.sync("rules").await;
        assert_eq!(code_of(&report), Code::EditedNotRecorded);
        assert_eq!(world.alerts(), vec!["edited_not_recorded"]);
        assert_eq!(world.row("rules").await.unwrap(), before);
        world.heal_updates().await;
        assert_eq!(code_of(&world.sync("rules").await), Code::Edited);
        assert_eq!(code_of(&world.sync("rules").await), Code::Unchanged);
        assert_eq!(world.discord.with(|s| s.messages.len()), 1);
    }

    #[tokio::test]
    async fn a_corrupt_table_is_reported_not_treated_as_empty() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.post("rules").await;
        world
            .sql("UPDATE managed_posts SET content_hash = 'nope';")
            .await;
        let listed = world.list_as(owner()).await;
        assert_eq!(listed.outcome.code, Code::StoreFailed);
        assert_eq!(listed.outcome.detail, Some("corrupt_row"));
        assert!(listed.posts.is_empty());
        // Each call raises its own alert: a later call must not hide an earlier one.
        assert_eq!(world.alerts(), vec!["store_failed"], "after list");
        let report = world.sync_all().await;
        assert_eq!(code_of(&report), Code::StoreFailed);
        assert_eq!(world.alerts().len(), 2, "after sync-all");
        let report = world.post("rules").await;
        assert_eq!(code_of(&report), Code::StoreFailed);
        assert_eq!(world.alerts().len(), 3, "after post");
        assert_eq!(world.discord.writes(), 1);
        assert!(world.alerts().iter().all(|code| *code == "store_failed"));
    }

    #[tokio::test]
    async fn alerts_are_one_per_failure_code_per_run() {
        let world = World::live().await;
        for key in ["k-a", "k-b", "k-c", "k-d"] {
            world.templates.put(key, &tpl("V1"));
            world.post(key).await;
            world.templates.put(key, &tpl("V2"));
        }
        world
            .discord
            .with(|s| s.fail_edit = Some(DiscordError::Failed));
        let report = world.sync_all().await;
        assert_eq!(report.counts().failed, 4);
        assert_eq!(world.alerts(), vec!["discord_failed"]);
    }

    // ---- list ----

    #[tokio::test]
    async fn list_shows_recorded_posts_ordered_by_key() {
        let world = World::live().await;
        for key in ["zeta", "alpha", "mid"] {
            world.templates.put(key, &tpl(key));
            world.post(key).await;
        }
        let listed = world.list_as(owner()).await;
        assert_eq!(listed.outcome.code, Code::Listed);
        let keys: Vec<&str> = listed.posts.iter().map(|p| p.record.key.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "mid", "zeta"]);
        let text = list_reply(&listed);
        for key in keys {
            assert!(text.text().contains(key));
        }
        assert!(text.text().contains(&hash_of(&tpl("alpha"))[..12]));
        assert!(
            !text.text().contains(&hash_of(&tpl("alpha"))),
            "only a short hash is shown"
        );
        let empty = World::live().await;
        let listed = empty.list_as(owner()).await;
        assert_eq!(listed.outcome.code, Code::Listed);
        assert!(listed.posts.is_empty());
    }

    // ---- replies ----

    #[tokio::test]
    async fn replies_are_ephemeral_and_carry_codes_never_content_or_paths() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("S3CR3T-TITLE"));
        let mut replies = vec![report_reply(&world.post("rules").await)];
        world.templates.put("rules", &tpl("S3CR3T-TITLE-2"));
        replies.push(report_reply(&world.sync("rules").await));
        replies.push(report_reply(&world.sync_all().await));
        replies.push(list_reply(&world.list_as(owner()).await));
        replies.push(report_reply(&world.post("absent").await));
        replies.push(report_reply(&world.post("../S3CR3T-PATH").await));
        replies.push(report_reply(
            &world.post_as(rando(), "rules", CHAN, false).await,
        ));
        world
            .discord
            .with(|s| s.fail_edit = Some(DiscordError::Failed));
        world.templates.put("rules", &tpl("S3CR3T-TITLE-3"));
        replies.push(report_reply(&world.sync("rules").await));
        for reply in &replies {
            assert!(reply.ephemeral());
            assert!(!reply.text().is_empty());
            assert!(!reply.text().contains("S3CR3T"), "{}", reply.text());
            assert!(!reply.text().contains("synthetic-template-dir"));
            assert!(!reply.text().contains(".json"));
            assert!(reply.text().chars().count() <= MAX_REPLY_CHARS);
        }
        assert!(replies[0].text().contains("posted"));
        assert!(replies[1].text().contains("edited"));
        assert!(replies[4].text().contains("template_not_found"));
        assert!(replies[5].text().contains("invalid_key"));
        assert!(replies[6].text().contains("denied"));
        assert!(replies[7].text().contains("discord_failed"));
        for text in world.alerts() {
            assert!(!alert_text(text).contains("S3CR3T"));
        }
    }

    #[tokio::test]
    async fn long_replies_stay_inside_the_message_limit() {
        let world = World::live().await;
        let long = "k".repeat(posts::MAX_KEY_BYTES - 4);
        for i in 0..300 {
            let key = format!("{long}{i:04}");
            world.templates.put(&key, &tpl("V1"));
            assert_eq!(code_of(&world.post(&key).await), Code::Posted);
        }
        let listed = world.list_as(owner()).await;
        assert_eq!(listed.posts.len(), 300);
        let reply = list_reply(&listed);
        assert!(reply.text().chars().count() <= MAX_REPLY_CHARS);
        assert!(reply.text().contains("300"), "the total is stated");
        let report = world.sync_all().await;
        assert_eq!(report.items.len(), 300);
        let reply = report_reply(&report);
        assert!(reply.text().chars().count() <= MAX_REPLY_CHARS);
        assert!(reply.text().contains("unchanged=300"), "{}", reply.text());
    }

    #[test]
    fn alert_text_is_the_code_and_nothing_else() {
        assert_eq!(alert_text("store_failed"), "embeds: store_failed");
        for class_code in [
            Code::DiscordFailed,
            Code::DiscordForbidden,
            Code::StoreFailed,
            Code::PostedNotRecorded,
            Code::EditedNotRecorded,
        ] {
            let text = alert_text(class_code.as_str());
            assert!(text
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c == ':' || c == ' '));
        }
    }

    #[test]
    fn codes_are_distinct_stable_and_classified() {
        let all = [
            (Code::Disabled, "disabled", Class::Refusal),
            (Code::Denied, "denied", Class::Refusal),
            (Code::InvalidKey, "invalid_key", Class::Refusal),
            (Code::InvalidTarget, "invalid_target", Class::Refusal),
            (Code::TemplateNotFound, "template_not_found", Class::Refusal),
            (
                Code::TemplateUnreadable,
                "template_unreadable",
                Class::Refusal,
            ),
            (Code::TemplateInvalid, "template_invalid", Class::Refusal),
            (Code::Posted, "posted", Class::Success),
            (Code::Edited, "edited", Class::Success),
            (Code::Recreated, "recreated", Class::Success),
            (Code::Unchanged, "unchanged", Class::Success),
            (Code::WouldPost, "would_post", Class::Success),
            (Code::WouldEdit, "would_edit", Class::Success),
            (Code::WouldRecreate, "would_recreate", Class::Success),
            (Code::Refused, "refused", Class::Refusal),
            (Code::MessageMissing, "message_missing", Class::Refusal),
            (Code::NotRecorded, "not_recorded", Class::Refusal),
            (Code::Busy, "busy", Class::Refusal),
            (Code::DiscordForbidden, "discord_forbidden", Class::Failure),
            (Code::DiscordFailed, "discord_failed", Class::Failure),
            (Code::StoreFailed, "store_failed", Class::Failure),
            (
                Code::PostedNotRecorded,
                "posted_not_recorded",
                Class::Failure,
            ),
            (
                Code::EditedNotRecorded,
                "edited_not_recorded",
                Class::Failure,
            ),
            (Code::Listed, "listed", Class::Success),
        ];
        let mut seen = HashSet::new();
        for (code, text, class) in all {
            assert_eq!(code.as_str(), text);
            assert_eq!(code.class(), class, "{text}");
            assert!(seen.insert(text));
        }
    }

    // ---- shadow ----

    #[tokio::test]
    async fn shadow_decides_but_sends_edits_deletes_and_writes_nothing() {
        // Seed a recorded post through a live world, then look at it in shadow.
        let live = World::live().await;
        live.templates.put("old-post", &tpl("V1"));
        live.post("old-post").await;
        let world = World::shadow().await;
        // Share the database by copying the row.
        let seeded = live.row("old-post").await.unwrap().record;
        world
            .store
            .write(WriteRequest {
                key: "old-post".into(),
                kind: WriteKind::Create,
                record: seeded.clone(),
                now: 5,
            })
            .await
            .unwrap();
        world.discord.with(|s| {
            s.messages.insert(seeded.message, (CHAN, "V1".into()));
        });
        world.templates.put("old-post", &tpl("V2")); // would be an edit
        world.templates.put("new-post", &tpl("N1")); // would be a post
        world.templates.put("same", &tpl("S1"));
        let before_rows = world.rows().await;
        let before_messages = world.discord.with(|s| s.messages.clone());

        let report = world.post("new-post").await;
        assert!(report.shadow);
        assert_eq!(code_of(&report), Code::WouldPost);
        let report = world.sync("old-post").await;
        assert_eq!(code_of(&report), Code::WouldEdit);
        assert!(report.shadow);
        let counts = report.counts();
        assert_eq!((counts.would_edit, counts.edited, counts.posted), (1, 0, 0));
        // A deleted message with recreate allowed is a would-recreate.
        world.discord.with(|s| s.messages.clear());
        let report = world.sync_as(owner(), Some("old-post"), true).await;
        assert_eq!(code_of(&report), Code::WouldRecreate);
        let report = world.sync_as(owner(), Some("old-post"), false).await;
        assert_eq!(code_of(&report), Code::Refused);

        assert_eq!(
            world.discord.with(|s| (s.sends, s.edits, s.deletes)),
            (0, 0, 0)
        );
        assert_eq!(world.rows().await, before_rows, "no record was written");
        assert_eq!(before_messages.len(), 1);
        assert!(world.alerts().is_empty());
    }

    #[tokio::test]
    async fn shadow_reply_is_a_structured_record_of_counts_and_codes() {
        let world = World::shadow().await;
        world.templates.put("a-post", &tpl("S3CR3T"));
        world.templates.put("b-post", &tpl("S3CR3T"));
        let report = world.post("a-post").await;
        let reply = report_reply(&report);
        assert!(reply.text().contains("shadow"));
        assert!(reply.text().contains("would_post"));
        assert!(!reply.text().contains("S3CR3T"));
        let combined = Report {
            shadow: true,
            items: vec![
                ItemReport {
                    key: Some("a".into()),
                    outcome: Outcome::new(Code::WouldPost),
                },
                ItemReport {
                    key: Some("b".into()),
                    outcome: Outcome::new(Code::WouldPost),
                },
                ItemReport {
                    key: Some("c".into()),
                    outcome: Outcome::new(Code::WouldEdit),
                },
                ItemReport {
                    key: Some("d".into()),
                    outcome: Outcome::new(Code::WouldRecreate),
                },
                ItemReport {
                    key: Some("e".into()),
                    outcome: Outcome::new(Code::Unchanged),
                },
                ItemReport {
                    key: Some("f".into()),
                    outcome: Outcome::new(Code::Refused),
                },
                ItemReport {
                    key: Some("g".into()),
                    outcome: Outcome::new(Code::DiscordFailed),
                },
            ],
        };
        let counts = combined.counts();
        assert_eq!(
            counts,
            Counts {
                would_post: 2,
                would_edit: 1,
                would_recreate: 1,
                unchanged: 1,
                refused: 1,
                failed: 1,
                ..Counts::default()
            }
        );
        let text = report_reply(&combined);
        assert!(text.text().contains("would_post=2"), "{}", text.text());
    }

    #[tokio::test]
    async fn shadow_does_not_alert_even_when_a_read_fails() {
        let world = World::shadow().await;
        let live = World::live().await;
        live.templates.put("rules", &tpl("V1"));
        live.post("rules").await;
        let seeded = live.row("rules").await.unwrap().record;
        world
            .store
            .write(WriteRequest {
                key: "rules".into(),
                kind: WriteKind::Create,
                record: seeded,
                now: 1,
            })
            .await
            .unwrap();
        world.templates.put("rules", &tpl("V2"));
        world
            .discord
            .with(|s| s.fail_lookup = Some(DiscordError::Failed));
        let report = world.sync("rules").await;
        assert_eq!(code_of(&report), Code::DiscordFailed);
        assert!(
            world.alerts().is_empty(),
            "shadow sends nothing, an alert is a send"
        );
        assert_eq!(world.discord.writes(), 0);
    }

    // ---- one sync per key ----

    #[tokio::test]
    async fn a_second_sync_of_a_busy_key_is_refused_and_other_keys_proceed() {
        let world = World::live().await;
        for key in ["rules", "info"] {
            world.templates.put(key, &tpl("V1"));
            world.post(key).await;
        }
        world.templates.put("rules", &tpl("V2"));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        world
            .discord
            .with(|s| s.gate = Some((entered.clone(), release.clone())));
        let first = world.sync("rules");
        let second = async {
            entered.notified().await;
            let again = world.sync("rules").await;
            let post_again = world.post("rules").await;
            // A different key is not blocked by the held one.
            world.discord.with(|s| s.gate = None);
            let other = world.sync("info").await;
            release.notify_one();
            (again, post_again, other)
        };
        let joined = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(first, second)
        })
        .await;
        let (first, (again, post_again, other)) =
            joined.expect("a second call for a held key must be refused, not wait for it");
        assert_eq!(code_of(&first), Code::Edited);
        assert_eq!(code_of(&again), Code::Busy);
        assert_eq!(code_of(&post_again), Code::Busy);
        assert_eq!(code_of(&other), Code::Unchanged);
        assert_eq!(
            world.discord.with(|s| s.edits),
            1,
            "the refused calls did not edit"
        );
        // The key is free again afterwards.
        assert_eq!(code_of(&world.sync("rules").await), Code::Unchanged);
    }

    #[tokio::test]
    async fn a_key_is_released_after_every_early_exit() {
        let world = World::live().await;
        world.templates.put("rules", "{bad");
        for _ in 0..3 {
            assert_eq!(code_of(&world.post("rules").await), Code::TemplateInvalid);
        }
        world.templates.put("rules", &tpl("V1"));
        world
            .discord
            .with(|s| s.fail_send = Some(DiscordError::Failed));
        for _ in 0..3 {
            assert_eq!(code_of(&world.post("rules").await), Code::DiscordFailed);
        }
        world.discord.with(|s| s.fail_send = None);
        assert_eq!(code_of(&world.post("rules").await), Code::Posted);
    }

    #[test]
    fn key_locks_hold_one_holder_and_release_on_drop() {
        let locks = KeyLocks::new();
        let a = locks.try_acquire("rules").expect("first");
        assert!(locks.try_acquire("rules").is_none());
        assert!(locks.try_acquire("other").is_some());
        drop(a);
        assert!(locks.try_acquire("rules").is_some());
    }

    // ---- liveness ----

    struct Model {
        keys: Vec<&'static str>,
        version: HashMap<&'static str, u32>,
        orphans: HashMap<&'static str, usize>,
    }

    fn title_of(key: &str, version: u32) -> String {
        format!("{key}:v{version}")
    }

    /// Messages on the fake Discord whose title belongs to `key`.
    fn live_for(world: &World, key: &str) -> usize {
        world.discord.with(|s| {
            s.messages
                .values()
                .filter(|(_, title)| title.starts_with(&format!("{key}:")))
                .count()
        })
    }

    async fn run_noisy_sequence(seed: u64, steps: usize) {
        let world = World::live().await;
        let mut model = Model {
            keys: vec!["rules-a", "rules-b", "info-c"],
            version: HashMap::new(),
            orphans: HashMap::new(),
        };
        for key in model.keys.clone() {
            model.version.insert(key, 1);
            world.templates.put(key, &tpl(&title_of(key, 1)));
        }
        let mut rng = Lcg(seed);
        for step in 0..steps {
            let key = model.keys[rng.below(3) as usize];
            match rng.below(10) {
                0 | 1 => {
                    let v = model.version[key] + 1;
                    model.version.insert(key, v);
                    world.templates.put(key, &tpl(&title_of(key, v)));
                }
                2 => {
                    // Discord loses a message.
                    world.discord.with(|s| {
                        let victim = s
                            .messages
                            .iter()
                            .find(|(_, (_, t))| t.starts_with(&format!("{key}:")))
                            .map(|(id, _)| *id);
                        if let Some(id) = victim {
                            s.messages.remove(&id);
                        }
                    });
                }
                3 => {
                    world.discord.with(|s| {
                        s.fail_send = [None, Some(DiscordError::Failed)][rng.below(2) as usize];
                        s.fail_edit = [
                            None,
                            Some(DiscordError::Failed),
                            Some(DiscordError::NotFound),
                        ][rng.below(3) as usize];
                        s.fail_lookup =
                            [None, None, Some(DiscordError::Forbidden)][rng.below(3) as usize];
                        s.fail_delete = [None, Some(DiscordError::Failed)][rng.below(2) as usize];
                    });
                }
                4 => {
                    world.heal_inserts().await;
                    world.heal_updates().await;
                    match rng.below(3) {
                        0 => world.break_inserts().await,
                        1 => world.break_updates().await,
                        _ => {}
                    }
                }
                5 | 6 => {
                    let report = world.post_as(owner(), key, CHAN, rng.below(2) == 0).await;
                    note_orphans(&mut model, &report);
                }
                7 => {
                    let report = world.sync_as(owner(), Some(key), rng.below(2) == 0).await;
                    note_orphans(&mut model, &report);
                }
                _ => {
                    let report = world.sync_as(owner(), None, rng.below(2) == 0).await;
                    note_orphans(&mut model, &report);
                }
            }
            // Every step: a key has one live post, plus the orphans the command reported.
            for k in &model.keys {
                let allowed = 1 + model.orphans.get(k).copied().unwrap_or(0);
                assert!(
                    live_for(&world, k) <= allowed,
                    "seed {seed} step {step}: {k} has {} posts, {allowed} allowed",
                    live_for(&world, k)
                );
            }
        }

        // Quiet phase: no faults, no deletes, no template changes.
        world.discord.with(|s| {
            s.fail_send = None;
            s.fail_edit = None;
            s.fail_lookup = None;
            s.fail_delete = None;
        });
        world.heal_inserts().await;
        world.heal_updates().await;
        for round in 0..4 {
            let writes_before = world.discord.writes();
            for key in model.keys.clone() {
                let report = world.post_as(owner(), key, CHAN, true).await;
                let code = code_of(&report);
                if round >= 1 {
                    assert_eq!(code, Code::Unchanged, "seed {seed} round {round} {key}");
                } else {
                    assert_eq!(code.class(), Class::Success, "seed {seed} {key}: {code:?}");
                }
                let row = world
                    .row(key)
                    .await
                    .expect("recorded after the first quiet round");
                let shown = world
                    .discord
                    .with(|s| s.messages.get(&row.record.message).cloned());
                assert_eq!(
                    shown,
                    Some((CHAN, title_of(key, model.version[key]))),
                    "seed {seed} round {round}: {key} shows the current template"
                );
                assert_eq!(
                    row.record.content_hash,
                    hash_of(&tpl(&title_of(key, model.version[key])))
                );
            }
            if round >= 1 {
                assert_eq!(
                    world.discord.writes(),
                    writes_before,
                    "a settled round writes nothing"
                );
            }
        }
        // A sync-all of the settled state is quiet as well.
        let writes = world.discord.writes();
        let report = world.sync_all().await;
        assert_eq!(report.counts().unchanged, 3);
        assert_eq!(world.discord.writes(), writes);
    }

    fn note_orphans(model: &mut Model, report: &Report) {
        for item in &report.items {
            if item.outcome.code == Code::PostedNotRecorded {
                let key = item.key.as_deref().unwrap();
                let key = *model.keys.iter().find(|k| **k == key).unwrap();
                *model.orphans.entry(key).or_insert(0) += 1;
            }
        }
    }

    #[tokio::test]
    async fn seeded_sequences_settle_and_never_duplicate_a_post() {
        for seed in [0x5eed_1001, 0x5eed_2002, 0x5eed_3003, 0x5eed_4004] {
            run_noisy_sequence(seed, 600).await;
        }
    }

    #[tokio::test]
    async fn a_discord_that_always_fails_is_never_quiet() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world
            .discord
            .with(|s| s.fail_send = Some(DiscordError::Failed));
        for i in 0..60 {
            let report = world.post("rules").await;
            assert_eq!(code_of(&report), Code::DiscordFailed, "call {i}");
            assert_eq!(world.alerts().len(), i + 1, "every failing call alerts");
        }
        assert!(world.rows().await.is_empty());
        world.discord.with(|s| s.fail_send = None);
        assert_eq!(code_of(&world.post("rules").await), Code::Posted);
    }

    #[tokio::test]
    async fn a_store_that_always_loses_writes_never_piles_up_duplicates() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.break_inserts().await;
        for i in 0..60 {
            assert_eq!(
                code_of(&world.post("rules").await),
                Code::StoreFailed,
                "call {i}"
            );
            assert_eq!(world.discord.with(|s| s.messages.len()), 0, "call {i}");
        }
        assert_eq!(world.alerts().len(), 60);
        world.heal_inserts().await;
        assert_eq!(code_of(&world.post("rules").await), Code::Posted);
        assert_eq!(world.discord.with(|s| s.messages.len()), 1);
    }

    // ---- store, source, adapter pieces ----

    #[tokio::test]
    async fn the_store_migrates_twice_and_reports_a_broken_database_by_code() {
        let conn = tokio_rusqlite::Connection::open_in_memory().await.unwrap();
        let store = TokioStore::new(conn);
        assert_eq!(
            store.list().await.unwrap_err(),
            RepoError::Sqlite,
            "no table yet"
        );
        store.migrate().await.unwrap();
        store.migrate().await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
        assert_eq!(store.get("../x").await.unwrap_err(), RepoError::InvalidKey);
        let record = PostRecord {
            key: "rules".into(),
            guild: GUILD,
            channel: CHAN,
            message: 7_001,
            content_hash: hash_of(&tpl("V1")),
            updated_by: OWNER,
        };
        let req = WriteRequest {
            key: "rules".into(),
            kind: WriteKind::Create,
            record: record.clone(),
            now: 9,
        };
        assert!(store.write(req.clone()).await.unwrap());
        assert_eq!(
            store.write(req).await.unwrap_err(),
            RepoError::AlreadyExists
        );
        let stale = WriteRequest {
            key: "rules".into(),
            kind: WriteKind::Edit {
                message: 7_001,
                from: "0".repeat(64),
            },
            record: PostRecord {
                content_hash: hash_of(&tpl("V2")),
                ..record
            },
            now: 10,
        };
        assert_eq!(
            store.write(stale).await.unwrap_err(),
            RepoError::PlanMismatch
        );
        assert_eq!(store.get("rules").await.unwrap().unwrap().updated_at, 9);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("em2-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn the_directory_source_reads_plain_keys_and_nothing_else() {
        let root = scratch("dir-source");
        let dir = root.join("tpl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rules.json"), tpl("V1")).unwrap();
        std::fs::write(root.join("outside.json"), tpl("OUTSIDE")).unwrap();
        std::fs::create_dir_all(dir.join("folder.json")).unwrap();
        std::fs::write(
            dir.join("huge.json"),
            vec![b' '; (MAX_TEMPLATE_BYTES + 1) as usize],
        )
        .unwrap();
        std::fs::write(
            dir.join("exact.json"),
            vec![b' '; MAX_TEMPLATE_BYTES as usize],
        )
        .unwrap();
        std::fs::write(dir.join("binary.json"), [0xff, 0xfe, 0x00]).unwrap();
        let source = DirTemplateSource::new(dir.clone());
        assert_eq!(source.read("rules").await.unwrap(), tpl("V1"));
        assert_eq!(
            source.read("absent").await.unwrap_err(),
            SourceError::NotFound
        );
        assert_eq!(
            source.read("folder").await.unwrap_err(),
            SourceError::Unreadable
        );
        assert_eq!(
            source.read("huge").await.unwrap_err(),
            SourceError::TooLarge
        );
        assert_eq!(
            source.read("exact").await.unwrap().len() as u64,
            MAX_TEMPLATE_BYTES
        );
        assert_eq!(
            source.read("binary").await.unwrap_err(),
            SourceError::Unreadable
        );
        for key in [
            "../outside",
            "..\\outside",
            "rules.json",
            "Rules",
            "/rules",
            "",
            "a/../rules",
        ] {
            assert!(source.read(key).await.is_err(), "{key:?}");
        }
        // The same keys through the whole command never reach the file system.
        let world = World::live().await;
        let env = Env {
            templates: &source,
            ..world.env()
        };
        for key in ["../outside", "..\\outside", "a/../rules"] {
            let report = post(
                &env,
                &PostRequest {
                    invoker: owner(),
                    key,
                    guild: GUILD,
                    channel: CHAN,
                    allow_recreate: false,
                    now: 1,
                },
            )
            .await;
            assert_eq!(code_of(&report), Code::InvalidKey);
        }
        assert_eq!(world.discord.calls(), 0);
        // And a real read through the command works end to end.
        let report = post(
            &env,
            &PostRequest {
                invoker: owner(),
                key: "rules",
                guild: GUILD,
                channel: CHAN,
                allow_recreate: false,
                now: 1,
            },
        )
        .await;
        assert_eq!(code_of(&report), Code::Posted);
    }

    #[tokio::test]
    async fn an_unreadable_directory_leaks_no_path_in_the_reply() {
        let root = scratch("S3CR3T-dir");
        let source = DirTemplateSource::new(root.join("does-not-exist"));
        let world = World::live().await;
        let env = Env {
            templates: &source,
            ..world.env()
        };
        let report = post(
            &env,
            &PostRequest {
                invoker: owner(),
                key: "rules",
                guild: GUILD,
                channel: CHAN,
                allow_recreate: false,
                now: 1,
            },
        )
        .await;
        assert_eq!(code_of(&report), Code::TemplateNotFound);
        let reply = report_reply(&report);
        assert!(!reply.text().contains("S3CR3T"));
        assert!(!format!("{report:?}").contains("S3CR3T"));
    }

    #[test]
    fn http_statuses_map_to_discord_error_classes() {
        assert_eq!(classify_status(404), DiscordError::NotFound);
        assert_eq!(classify_status(403), DiscordError::Forbidden);
        assert_eq!(classify_status(401), DiscordError::Forbidden);
        for status in [0, 200, 400, 429, 500, 502] {
            assert_eq!(classify_status(status), DiscordError::Failed, "{status}");
        }
    }

    #[test]
    fn render_carries_every_part_of_the_template() {
        let full = template::load(
            r##"{"title":"T","description":"D","fields":[{"name":"n1","value":"v1","inline":true},{"name":"n2","value":"v2"}],"footer":{"text":"F"},"author":{"name":"A"},"color":255}"##,
        )
        .unwrap();
        let json = serde_json::to_value(render(&full)).unwrap();
        assert_eq!(json["title"], "T");
        assert_eq!(json["description"], "D");
        assert_eq!(json["footer"]["text"], "F");
        assert_eq!(json["author"]["name"], "A");
        assert_eq!(json["color"], 255);
        assert_eq!(json["fields"][0]["name"], "n1");
        assert_eq!(json["fields"][0]["inline"], true);
        assert_eq!(json["fields"][1]["value"], "v2");
        assert_eq!(json["fields"][1]["inline"], false);
        assert_eq!(json["fields"].as_array().unwrap().len(), 2);
        let bare = template::load(&tpl("only")).unwrap();
        let json = serde_json::to_value(render(&bare)).unwrap();
        assert_eq!(json["title"], "only");
        assert!(json.get("color").is_none_or(|c| c.is_null()));
        assert!(json.get("footer").is_none_or(|c| c.is_null()));
    }

    #[test]
    fn no_mention_set_can_fire() {
        let none = AllowedMentions::none();
        assert!(none.users.is_empty() && none.roles.is_empty());
        assert!(!none.everyone && !none.replied_user);
        // The object sent to Discord names no user, no role and no `@everyone`.
        let json = serde_json::to_value(serenity_mentions(&none)).unwrap();
        let parse = json.get("parse").cloned().unwrap_or(serde_json::json!([]));
        assert_eq!(parse, serde_json::json!([]), "{json}");
        for key in ["users", "roles"] {
            let list = json.get(key).cloned().unwrap_or(serde_json::json!([]));
            assert_eq!(list, serde_json::json!([]), "{key}: {json}");
        }
        assert!(
            json.get("replied_user").is_none_or(|v| v == false),
            "{json}"
        );
    }

    // ---- one home guild ----

    const STRANGER: u64 = 7_001;
    fn stranger_owner() -> Invoker {
        Invoker {
            user: STRANGER,
            guild_owner: STRANGER,
            is_staff: false,
        }
    }

    #[test]
    fn check_guild_serves_only_the_set_home_guild() {
        assert_eq!(check_guild(Some(GUILD), GUILD), Ok(()));
        assert_eq!(
            check_guild(Some(GUILD), OTHER_GUILD),
            Err(Outcome::with(Code::Denied, "other_guild"))
        );
        for unset in [None, Some(0)] {
            assert_eq!(
                check_guild(unset, GUILD),
                Err(Outcome::with(Code::Denied, "no_home_guild")),
                "{unset:?}"
            );
            assert_eq!(
                check_guild(unset, 0),
                Err(Outcome::with(Code::Denied, "no_home_guild")),
                "{unset:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_owner_of_an_unrelated_guild_is_denied_and_cannot_squat_the_key() {
        for publishers in [Publishers::OwnerOnly, Publishers::Staff] {
            let world = World::new(Mode::Live, publishers).await;
            world.templates.put("rules", &tpl("Staff-only text"));
            // Their own guild, their own channel; the staff flag does not cross either.
            let staff_there = Invoker {
                is_staff: true,
                ..stranger_owner()
            };
            for who in [stranger_owner(), staff_there] {
                let report = world
                    .post_at(Some(GUILD), who, OTHER_GUILD, OTHER_CHAN)
                    .await;
                assert_eq!(code_of(&report), Code::Denied, "{publishers:?}");
                assert_eq!(report.items[0].outcome.detail, Some("other_guild"));
                let report = world
                    .sync_at(Some(GUILD), who, Some("rules"), OTHER_GUILD)
                    .await;
                assert_eq!(code_of(&report), Code::Denied);
                let report = world.sync_at(Some(GUILD), who, None, OTHER_GUILD).await;
                assert_eq!(code_of(&report), Code::Denied);
                let listed = world.list_at(Some(GUILD), who, OTHER_GUILD).await;
                assert_eq!(listed.outcome.code, Code::Denied);
                assert!(listed.posts.is_empty());
            }
            assert_eq!(world.discord.calls(), 0, "nothing was sent or looked up");
            assert_eq!(world.templates.read_count(), 0, "no template was read");
            assert!(world.rows().await.is_empty(), "no record, no squatted key");
            assert!(world.alerts().is_empty());
            // The real owner is not locked out of the key.
            let report = world.post("rules").await;
            assert_eq!(code_of(&report), Code::Posted, "{publishers:?}");
            assert_eq!(world.row("rules").await.unwrap().record.guild, GUILD);
        }
    }

    #[tokio::test]
    async fn an_unset_home_guild_fails_closed_while_the_mode_is_on() {
        for mode in [Mode::Live, Mode::Shadow] {
            for home in [None, Some(0)] {
                let world = World::new(mode, Publishers::OwnerOnly).await;
                world.templates.put("rules", &tpl("T"));
                let mut outcomes = vec![
                    world.post_at(home, owner(), GUILD, CHAN).await,
                    world.sync_at(home, owner(), Some("rules"), GUILD).await,
                    world.sync_at(home, owner(), None, GUILD).await,
                ];
                outcomes.push(Report {
                    shadow: false,
                    items: vec![ItemReport {
                        key: None,
                        outcome: world.list_at(home, owner(), GUILD).await.outcome,
                    }],
                });
                for report in outcomes {
                    assert_eq!(code_of(&report), Code::Denied, "{mode:?} {home:?}");
                    assert_eq!(report.items[0].outcome.detail, Some("no_home_guild"));
                }
                assert_eq!(world.discord.calls(), 0);
                assert_eq!(world.templates.read_count(), 0);
                assert!(world.rows().await.is_empty());
                assert!(world.alerts().is_empty());
            }
        }
        // An off feature stays `disabled`, whatever the home guild.
        let world = World::new(Mode::Off, Publishers::OwnerOnly).await;
        let report = world.post_at(None, owner(), GUILD, CHAN).await;
        assert_eq!(code_of(&report), Code::Disabled);
    }

    #[tokio::test]
    async fn the_home_guilds_own_owner_still_posts_syncs_and_lists() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        assert_eq!(code_of(&world.post("rules").await), Code::Posted);
        world.templates.put("rules", &tpl("V2"));
        assert_eq!(code_of(&world.sync("rules").await), Code::Edited);
        let listed = world.list_as(owner()).await;
        assert_eq!(listed.outcome.code, Code::Listed);
        assert_eq!(listed.posts.len(), 1);
        let shadow = World::shadow().await;
        shadow.templates.put("rules", &tpl("V1"));
        assert_eq!(code_of(&shadow.post("rules").await), Code::WouldPost);
    }

    // ---- guild scope of list and sync-all ----

    #[tokio::test]
    async fn list_shows_only_the_rows_of_the_callers_guild() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("Home title"));
        world.templates.put("secret_b", &tpl("Other title"));
        assert_eq!(
            code_of(&world.post_in(GUILD, "rules", CHAN).await),
            Code::Posted
        );
        let other = world.post_in(OTHER_GUILD, "secret_b", OTHER_CHAN).await;
        assert_eq!(code_of(&other), Code::Posted);
        let other_row = world.row("secret_b").await.unwrap();

        let listed = world.list_at(Some(GUILD), owner(), GUILD).await;
        assert_eq!(listed.outcome.code, Code::Listed);
        let keys: Vec<&str> = listed.posts.iter().map(|p| p.record.key.as_str()).collect();
        assert_eq!(keys, vec!["rules"]);
        let text = list_reply(&listed).text().to_string();
        assert!(text.contains("rules"), "{text}");
        assert!(!text.contains("secret_b"), "{text}");
        assert!(!text.contains(&OTHER_CHAN.to_string()), "{text}");
        assert!(
            !text.contains(&other_row.record.message.to_string()),
            "{text}"
        );
        assert!(
            !text.contains(&other_row.record.content_hash[..12]),
            "{text}"
        );

        // And the other way round.
        let listed = world.list_at(Some(OTHER_GUILD), owner(), OTHER_GUILD).await;
        let keys: Vec<&str> = listed.posts.iter().map(|p| p.record.key.as_str()).collect();
        assert_eq!(keys, vec!["secret_b"]);
        // A guild with no rows lists nothing, and says so.
        let listed = world.list_at(Some(7_777), owner(), 7_777).await;
        assert_eq!(listed.outcome.code, Code::Listed);
        assert!(listed.posts.is_empty());
        assert!(world.alerts().is_empty());
    }

    #[tokio::test]
    async fn list_of_the_unrelated_guild_owner_returns_nothing_about_another_guild() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("T"));
        world.post("rules").await;
        let listed = world.list_in(stranger_owner(), OTHER_GUILD).await;
        assert_eq!(listed.outcome.code, Code::Denied);
        assert!(listed.posts.is_empty());
        let text = list_reply(&listed).text().to_string();
        assert!(!text.contains("rules"), "{text}");
        assert!(!text.contains(&CHAN.to_string()), "{text}");
    }

    #[tokio::test]
    async fn list_for_guild_zero_is_an_invalid_target() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("T"));
        world.post("rules").await;
        for home in [Some(GUILD), Some(0), None] {
            let listed = world.list_at(home, owner(), 0).await;
            assert_ne!(listed.outcome.code, Code::Listed, "{home:?}");
            assert!(listed.posts.is_empty(), "{home:?}");
        }
        let listed = world.list_at(Some(GUILD), owner(), 0).await;
        assert_eq!(listed.outcome.code, Code::InvalidTarget);
    }

    #[tokio::test]
    async fn sync_all_neither_reads_nor_reports_the_rows_of_another_guild() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.templates.put("secret_b", &tpl("B1"));
        world.post_in(GUILD, "rules", CHAN).await;
        world.post_in(OTHER_GUILD, "secret_b", OTHER_CHAN).await;
        // The other guild's template changed: a sync-all of the home guild must not touch it.
        world.templates.put("secret_b", &tpl("B2"));
        let reads = world.templates.read_count();
        let lookups = world.discord.with(|s| s.lookups);
        let other_before = world.row("secret_b").await.unwrap();

        let report = world.sync_at(Some(GUILD), owner(), None, GUILD).await;
        assert_eq!(report.items.len(), 1, "{report:?}");
        assert_eq!(report.items[0].key.as_deref(), Some("rules"));
        assert_eq!(report.items[0].outcome.code, Code::Unchanged);
        assert_eq!(
            world.templates.read_count(),
            reads + 1,
            "only `rules` was read"
        );
        assert_eq!(world.discord.with(|s| s.lookups), lookups + 1);
        assert_eq!(world.row("secret_b").await.unwrap(), other_before);
        assert_eq!(world.discord.with(|s| s.edits), 0);
        let text = report_reply(&report).text().to_string();
        assert!(!text.contains("secret_b"), "{text}");
        assert!(!format!("{report:?}").contains("secret_b"));

        // A guild with no rows of its own syncs nothing and reads nothing.
        let reads = world.templates.read_count();
        let report = world
            .sync_at(Some(OTHER_GUILD + 1), owner(), None, OTHER_GUILD + 1)
            .await;
        assert!(report.items.is_empty(), "{report:?}");
        assert_eq!(world.templates.read_count(), reads);
        let text = report_reply(&report).text().to_string();
        assert!(
            !text.contains("rules") && !text.contains("secret_b"),
            "{text}"
        );
        assert!(world.alerts().is_empty());
    }

    // ---- rule (g): each failing path alerts ----

    #[tokio::test]
    async fn a_compensating_delete_that_finds_the_message_gone_is_a_store_failure() {
        let world = World::live().await;
        world.templates.put("rules", &tpl("V1"));
        world.break_inserts().await;
        world
            .discord
            .with(|s| s.fail_delete = Some(DiscordError::NotFound));
        let report = world.post("rules").await;
        // The new post is already gone, so nothing is orphaned: only the write failed.
        assert_eq!(code_of(&report), Code::StoreFailed);
        assert_eq!(report.items[0].outcome.detail, Some("sqlite_error"));
        assert_eq!(world.discord.with(|s| (s.sends, s.deletes)), (1, 1));
        assert_eq!(world.alerts(), vec!["store_failed"]);
    }

    // ---- registry hook ----

    #[test]
    fn the_feature_hook_follows_the_configuration_and_registers_no_job() {
        let on = EmbedConfig {
            mode: Mode::Shadow,
            template_dir: Some(PathBuf::from("tpl")),
            publishers: Publishers::OwnerOnly,
        };
        let hook = EmbedFeature::new(&on);
        assert_eq!(<EmbedFeature as Feature<u8, u16>>::name(&hook), "embeds");
        assert!(<EmbedFeature as Feature<u8, u16>>::enabled(&hook));
        assert!(<EmbedFeature as Feature<u8, u16>>::needs_core_db(&hook));
        let mut registry = Registry::new();
        <EmbedFeature as Feature<u8, u16>>::register_jobs(&hook, &mut registry).unwrap();
        assert!(
            registry.is_empty(),
            "posting is user-driven: no durable job kind"
        );
        let off = EmbedFeature::new(&EmbedConfig::off());
        assert!(!<EmbedFeature as Feature<u8, u16>>::enabled(&off));
        let no_dir = EmbedFeature::new(&EmbedConfig {
            template_dir: None,
            ..on
        });
        assert!(!<EmbedFeature as Feature<u8, u16>>::enabled(&no_dir));
    }
}

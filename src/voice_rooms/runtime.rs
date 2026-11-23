//! Voice-room runtime (`docs/verification/VOICE-ROOMS-RUNTIME.md`).
//!
//! The thin layer between the approved pure policy (`super::policy`), the `voice_rooms`
//! table (`super::repository`), the durable job queue (`crate::jobs`) and Discord. Discord is
//! reached only through two small traits ([`RoomPorts`], [`StaffAlerter`]) so tests inject
//! fakes; the decisions are plain functions in this file. [`SerenityRoomPorts`] is the one
//! serenity-facing adapter and has NOT been run against Discord.
//!
//! The stored row is the truth about which channels the bot owns. A queued cleanup job is a
//! disposable wake-up: when it fires it re-reads the row and the live occupancy and decides
//! again. A channel without a row is never deleted or modified.
//!
//! Flow in `live` mode for a member who joins a hub channel:
//! 1. decide with the policy (caps, cooldown); a denial is a stable code and nothing else;
//! 2. create the room; a failure leaves the member in the hub and writes no row;
//! 3. write the row, THEN move the member; a failed move deletes the new, empty channel and
//!    its row (when the delete itself fails the row stays and a cleanup job is scheduled).
//!
//! When a tracked room becomes empty a `vc_cleanup` job is scheduled `VC_EMPTY_GRACE_SECS`
//! later. At fire time the handler deletes the channel only if it is still empty and the grace
//! has elapsed; a member who rejoined cancels the cleanup simply by being there.
//!
//! Job payloads hold ids and counters only. Alerts carry a stable code only.

use super::config::{NotReady, Readiness, VoiceSettings};
use super::policy::{
    self, Decision, DenyReason, JoinEvent, OverwriteTarget, Room, RoomOverwrite, VoiceRoomConfig,
    VoiceRoomState,
};
use super::repository::{self, RoomRow};
use crate::features::{Feature, JobSignal};
use crate::jobs::queue::{self, ClaimedJob, JobState, NewJob, RestartPolicy, ScheduleOutcome};
use crate::jobs::runner::{BoxFuture, Clock, HandlerError, HandlerResult, Registry, RunnerError};
use crate::settings::Mode;
use poise::serenity_prelude as serenity;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Job kind that deletes a tracked room once it has stayed empty.
pub const KIND_CLEANUP: &str = "vc_cleanup";

/// The job kind and its restart policy. The delete is idempotent (a 404 counts as done and the
/// handler re-reads the row and the occupancy), so a restart runs a cut-off job again.
pub const JOB_KINDS: [(&str, RestartPolicy); 1] = [(KIND_CLEANUP, RestartPolicy::RunAgain)];

/// How many times one cleanup chain is retried after a Discord failure before it gives up
/// (the row then stays and the next restart reconcile retries).
pub const MAX_CLEANUP_ATTEMPTS: u32 = 8;
const RETRY_BASE_SECS: i64 = 15;
const RETRY_CAP_SECS: i64 = 900;
/// How many dedupe-key variants one wake-up may try (see [`Inner::schedule_cleanup`]).
const MAX_KEY_VARIANTS: u32 = 4;

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

/// What the live state says about a tracked channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    /// The channel no longer exists.
    Missing,
    /// The channel exists and holds this many members.
    Members(usize),
    /// The answer is unknown (cache cold, lookup failed).
    Unknown,
}

/// The room to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRoomRequest {
    /// Guild to create the channel in.
    pub guild_id: u64,
    /// Category the channel goes under.
    pub category_id: u64,
    /// The cleaned room name, from the policy.
    pub name: String,
    /// The overwrites from the policy: the category's plus the owner's.
    pub overwrites: Vec<RoomOverwrite>,
    /// Member limit; 0 is unlimited.
    pub user_limit: u32,
}

/// Discord calls of the voice-room runtime.
#[serenity::async_trait]
pub trait RoomPorts: Send + Sync {
    /// The permission overwrites the category itself carries.
    async fn category_overwrites(
        &self,
        guild_id: u64,
        category_id: u64,
    ) -> Result<Vec<RoomOverwrite>, PortError>;
    /// Creates the room; returns the new channel id.
    async fn create_room(&self, req: &CreateRoomRequest) -> Result<u64, PortError>;
    /// Moves a member (who is in a voice channel) into `channel_id`.
    async fn move_member(
        &self,
        guild_id: u64,
        user_id: u64,
        channel_id: u64,
    ) -> Result<(), PortError>;
    /// Deletes a channel. Only ever called for a channel that has a row.
    async fn delete_room(&self, guild_id: u64, channel_id: u64) -> Result<(), PortError>;
    /// What the live state says about a channel.
    async fn occupancy(&self, guild_id: u64, channel_id: u64) -> Occupancy;
}

/// Stable codes for what is reported to the staff alert channel. Code only: no text, no id,
/// no name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertCode {
    /// The category's overwrites could not be read, so no room was created.
    CategoryLookupFailed,
    /// The room could not be created.
    CreateFailed,
    /// The new room could not be recorded; it was deleted again.
    StoreFailed,
    /// The member could not be moved; the new room was deleted again.
    MoveFailed,
    /// A delete failed; the room is kept and a cleanup is scheduled.
    DeleteFailed,
    /// A cleanup job could not be scheduled.
    ScheduleFailed,
    /// A cleanup chain used up its retries; the row stays for the next reconcile.
    CleanupGaveUp,
    /// A cleanup job carried a payload that is not the expected shape.
    PayloadInvalid,
    /// The core database is not available to the runtime.
    StoreUnavailable,
}

impl AlertCode {
    /// The stable code.
    pub fn code(self) -> &'static str {
        match self {
            AlertCode::CategoryLookupFailed => "vc_category_lookup_failed",
            AlertCode::CreateFailed => "vc_create_failed",
            AlertCode::StoreFailed => "vc_store_failed",
            AlertCode::MoveFailed => "vc_move_failed",
            AlertCode::DeleteFailed => "vc_delete_failed",
            AlertCode::ScheduleFailed => "vc_schedule_failed",
            AlertCode::CleanupGaveUp => "vc_cleanup_gave_up",
            AlertCode::PayloadInvalid => "vc_payload_invalid",
            AlertCode::StoreUnavailable => "vc_store_unavailable",
        }
    }
}

/// The staff alert text for a code: the code and nothing else.
pub fn alert_text(code: AlertCode) -> String {
    format!("voice rooms alert: {}", code.code())
}

/// Reports a stable code to the staff alert channel. Best effort: it cannot fail the caller.
#[serenity::async_trait]
pub trait StaffAlerter: Send + Sync {
    /// Sends the alert for `code`.
    async fn alert(&self, code: AlertCode);
}

/// A cleanup job's payload: ids and counters only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupPayload {
    /// The guild of the room.
    pub guild: u64,
    /// The room channel.
    pub channel: u64,
    /// When the room was seen empty (unix seconds).
    pub since: i64,
    /// How many Discord failures this chain has already had.
    pub attempt: u32,
}

/// The payload was not the plain `{guild, channel, since, attempt}` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadInvalid;

/// The JSON payload of a cleanup job. Ids and counters only.
pub fn cleanup_payload(p: &CleanupPayload) -> String {
    format!(
        "{{\"guild\":{},\"channel\":{},\"since\":{},\"attempt\":{}}}",
        p.guild, p.channel, p.since, p.attempt
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCleanup {
    guild: u64,
    channel: u64,
    since: i64,
    attempt: u32,
}

/// Parses a cleanup payload strictly: exactly the four keys, JSON integers, non-zero ids, a
/// non-negative time and an attempt counter within [`MAX_CLEANUP_ATTEMPTS`].
pub fn parse_cleanup_payload(raw: &str) -> Result<CleanupPayload, PayloadInvalid> {
    if true {
        // RED stub: no payload is valid.
        return Err(PayloadInvalid);
    }
    // serde also reads a struct from a JSON array; only the object form is a payload.
    if !raw.trim_start().starts_with('{') {
        return Err(PayloadInvalid);
    }
    let parsed: RawCleanup = serde_json::from_str(raw).map_err(|_| PayloadInvalid)?;
    if parsed.guild == 0 || parsed.channel == 0 || parsed.since < 0 {
        return Err(PayloadInvalid);
    }
    if parsed.attempt > MAX_CLEANUP_ATTEMPTS {
        return Err(PayloadInvalid);
    }
    Ok(CleanupPayload {
        guild: parsed.guild,
        channel: parsed.channel,
        since: parsed.since,
        attempt: parsed.attempt,
    })
}

/// Dedupe key of one cleanup wake-up: the room, its due time and the chain's attempt number,
/// plus a `variant` when an earlier job with the same key is already finished.
pub fn cleanup_dedupe_key(p: &CleanupPayload, due_at: i64, variant: u32) -> String {
    if variant == 0 {
        format!("vc_empty_{}_{}_{}", p.channel, due_at, p.attempt)
    } else {
        format!(
            "vc_empty_{}_{}_{}_{}",
            p.channel, due_at, p.attempt, variant
        )
    }
}

/// When a cleanup is due: the moment the room was seen empty plus the grace period.
pub fn due_at(since: i64, grace_secs: i64) -> i64 {
    since.saturating_add(grace_secs.max(0)).max(0)
}

/// Seconds to wait before retrying a failed cleanup: doubles per attempt, capped.
pub fn retry_delay_secs(attempt: u32) -> i64 {
    if true {
        // RED stub: retry at once.
        return 0;
    }
    let shift = attempt.min(20);
    RETRY_BASE_SECS
        .saturating_mul(1i64 << shift)
        .min(RETRY_CAP_SECS)
}

/// Maps an HTTP status (if the failure was an HTTP response) to a [`PortError`].
pub fn port_error_from_status(status: Option<u16>) -> PortError {
    match status {
        Some(404) => PortError::NotFound,
        Some(403) => PortError::Forbidden,
        _ => PortError::Unavailable,
    }
}

/// The plain facts of one voice-state update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceFacts {
    /// The guild of the update; `None` for an update outside a guild.
    pub guild_id: Option<u64>,
    /// The member whose state changed.
    pub user_id: u64,
    /// Whether the member is a bot; `None` when the update did not say.
    pub is_bot: Option<bool>,
    /// The voice channel before the update, if known.
    pub old_channel: Option<u64>,
    /// The voice channel after the update; `None` when the member disconnected.
    pub new_channel: Option<u64>,
}

/// Why a voice event is ignored. Stable codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreCode {
    /// The event belongs to a guild other than the home guild (or to no guild).
    WrongGuild,
    /// The member is a bot (this bot included).
    Bot,
    /// The update did not say whether the member is a bot.
    UnknownMember,
    /// The user id was zero.
    InvalidUser,
    /// The member stayed in the same channel (mute, deafen, stream).
    NoChannelChange,
    /// Shadow mode: the event is not a hub join.
    NotHubJoin,
}

impl IgnoreCode {
    /// The stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            IgnoreCode::WrongGuild => "wrong_guild",
            IgnoreCode::Bot => "bot",
            IgnoreCode::UnknownMember => "unknown_member",
            IgnoreCode::InvalidUser => "invalid_user",
            IgnoreCode::NoChannelChange => "no_channel_change",
            IgnoreCode::NotHubJoin => "not_hub_join",
        }
    }
}

/// The channel changes of one event that the runtime acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moves {
    /// The channel the member left.
    pub left: Option<u64>,
    /// The channel the member joined.
    pub joined: Option<u64>,
}

/// Classifies a voice event. Pure: the home guild and the bot's own id are passed in. The
/// guild is checked first, so an event of another guild is ignored before anything else is
/// looked at.
pub fn classify(
    facts: &VoiceFacts,
    home_guild: u64,
    bot_user: Option<u64>,
) -> Result<Moves, IgnoreCode> {
    if true {
        // RED stub: every event is ignored.
        return Err(IgnoreCode::NoChannelChange);
    }
    if home_guild == 0 || facts.guild_id != Some(home_guild) {
        return Err(IgnoreCode::WrongGuild);
    }
    if facts.user_id == 0 {
        return Err(IgnoreCode::InvalidUser);
    }
    if bot_user == Some(facts.user_id) {
        return Err(IgnoreCode::Bot);
    }
    match facts.is_bot {
        None => return Err(IgnoreCode::UnknownMember),
        Some(true) => return Err(IgnoreCode::Bot),
        Some(false) => {}
    }
    if facts.old_channel == facts.new_channel {
        return Err(IgnoreCode::NoChannelChange);
    }
    Ok(Moves {
        left: facts.old_channel.filter(|id| *id != 0),
        joined: facts.new_channel.filter(|id| *id != 0),
    })
}

/// What `process` did with one voice event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The mode is `off`.
    Off,
    /// The mode is on but something is unset: nothing was done.
    NotReady(NotReady),
    /// `live` mode without the core database: nothing was done (alerted).
    NoDatabase,
    /// The event is not one the runtime acts on.
    Ignored(IgnoreCode),
    /// Shadow mode: a room would have been created; nothing was sent, written or scheduled.
    Shadow,
    /// The event was relevant but there was nothing to do.
    Nothing,
    /// A room was created, recorded and the member moved into it.
    Created {
        /// The new room channel.
        channel: u64,
    },
    /// The policy denied the join (cap or cooldown). Silent: no Discord call, no alert.
    Denied(DenyReason),
    /// The category's overwrites could not be read; no room was created (alerted).
    CategoryLookupFailed,
    /// The room could not be created; the member stays in the hub, no row (alerted).
    CreateFailed,
    /// The room could not be recorded; it was deleted again (alerted).
    StoreFailed,
    /// The member could not be moved (alerted). `channel_deleted` says whether the new empty
    /// channel and its row are gone; when false the row stays and a cleanup is scheduled.
    MoveFailed {
        /// True when the new channel was deleted and no row remains.
        channel_deleted: bool,
    },
    /// A tracked room is empty and a cleanup job is scheduled.
    CleanupScheduled,
    /// A tracked room is empty but the cleanup could not be scheduled (alerted).
    CleanupScheduleFailed,
    /// A tracked room's channel no longer exists; its row was dropped.
    RoomGone,
}

/// What a reconcile pass did. Counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// The feature was off, misconfigured or had no database: nothing was done.
    pub skipped: bool,
    /// Rows dropped because their channel no longer exists.
    pub dropped_missing: u32,
    /// Empty tracked channels deleted (and their rows dropped).
    pub deleted_empty: u32,
    /// Occupied tracked channels left alone.
    pub kept: u32,
    /// Rooms whose delete failed or whose state was unknown: a cleanup job was scheduled.
    pub rescheduled: u32,
    /// Store or schedule failures (alerted).
    pub failed: u32,
}

/// Everything [`VoiceRoomsFeature::new`] needs.
pub struct VoiceDeps {
    /// Parsed settings.
    pub settings: VoiceSettings,
    /// Discord calls.
    pub ports: Arc<dyn RoomPorts>,
    /// Staff alerts.
    pub alerts: Arc<dyn StaffAlerter>,
    /// Time source (unix seconds).
    pub clock: Arc<dyn Clock>,
    /// The bot's own user id, if known; its voice events are ignored.
    pub bot_user_id: Option<u64>,
}

type Db = tokio_rusqlite::Connection;

/// A database or queue call failed; the caller alerts with the code that fits.
#[derive(Debug, Clone, Copy)]
struct StoreError;

struct Inner {
    settings: VoiceSettings,
    ports: Arc<dyn RoomPorts>,
    alerts: Arc<dyn StaffAlerter>,
    clock: Arc<dyn Clock>,
    bot_user_id: Option<u64>,
    wake: Mutex<Option<JobSignal>>,
    tally: Mutex<BTreeMap<&'static str, u64>>,
    /// Serializes room creation so the caps and the cooldown are checked against the rows of
    /// every earlier creation.
    create_lock: tokio::sync::Mutex<()>,
    /// When each member last asked for a room (also when the attempt failed).
    last_attempt: Mutex<HashMap<u64, i64>>,
    /// When a tracked room was last seen empty, by channel.
    empty_since: Mutex<HashMap<u64, i64>>,
    migrated: AtomicBool,
    db: Mutex<Option<Db>>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Inner {
    fn count(&self, key: &'static str) {
        let mut tally = locked(&self.tally);
        let slot = tally.entry(key).or_insert(0);
        *slot = slot.saturating_add(1);
    }

    fn wake_runner(&self) {
        let wake = locked(&self.wake).clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    fn attach_db(&self, db: &Db) {
        let mut slot = locked(&self.db);
        if slot.is_none() {
            *slot = Some(db.clone());
        }
    }

    fn attached_db(&self) -> Option<Db> {
        locked(&self.db).clone()
    }

    /// Creates the table on first use.
    async fn ready_db(&self, db: &Db) -> Result<(), StoreError> {
        if self.migrated.load(Ordering::SeqCst) {
            return Ok(());
        }
        db.call(repository::migrate).await.map_err(|_| StoreError)?;
        self.migrated.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn store_get(&self, db: &Db, channel: u64) -> Result<Option<RoomRow>, StoreError> {
        db.call(move |conn| repository::get(conn, channel))
            .await
            .map_err(|_| StoreError)
    }

    async fn store_list(&self, db: &Db, guild: u64) -> Result<Vec<RoomRow>, StoreError> {
        db.call(move |conn| repository::list(conn, guild))
            .await
            .map_err(|_| StoreError)
    }

    async fn store_insert(&self, db: &Db, row: RoomRow) -> Result<(), StoreError> {
        match db.call(move |conn| repository::insert(conn, &row)).await {
            Ok(true) => Ok(()),
            _ => Err(StoreError),
        }
    }

    async fn store_delete(&self, db: &Db, channel: u64) -> Result<(), StoreError> {
        db.call(move |conn| repository::delete(conn, channel))
            .await
            .map(|_| ())
            .map_err(|_| StoreError)
    }

    fn policy_config(&self, home: u64, hub: u64, category: u64) -> VoiceRoomConfig {
        let s = &self.settings;
        VoiceRoomConfig {
            guild_id: home,
            hub_channel_id: hub,
            category_id: category,
            default_name: policy::DEFAULT_ROOM_NAME.to_string(),
            per_user_cap: policy::DEFAULT_PER_USER_CAP,
            global_cap: s.max_rooms(),
            create_cooldown_secs: s.create_cooldown_secs(),
            empty_grace_secs: s.empty_grace_secs(),
            rename_budget: policy::RenameBudget::default(),
            category_overwrites: Vec::new(),
        }
    }

    async fn process(&self, facts: VoiceFacts, core_db: Option<&Db>) -> Outcome {
        if true {
            // RED stub: acts on nothing.
            return Outcome::Off;
        }
        match self.settings.readiness() {
            Readiness::Off => return Outcome::Off,
            Readiness::Misconfigured(reason) => return Outcome::NotReady(reason),
            Readiness::Ready => {}
        }
        // Ready implies a home guild.
        let Some(home) = self.settings.guild_id() else {
            return Outcome::NotReady(NotReady::HomeGuildUnset);
        };
        let moves = match classify(&facts, home, self.bot_user_id) {
            Ok(moves) => moves,
            Err(code) => return Outcome::Ignored(code),
        };
        if self.settings.mode() != Mode::Live {
            // Shadow: counts only; nothing is read, sent, written or scheduled.
            return match moves.joined {
                Some(joined) if self.settings.is_hub(joined) => {
                    self.count("would_create");
                    Outcome::Shadow
                }
                _ => Outcome::Ignored(IgnoreCode::NotHubJoin),
            };
        }
        let Some(db) = core_db else {
            self.alerts.alert(AlertCode::StoreUnavailable).await;
            return Outcome::NoDatabase;
        };
        self.attach_db(db);
        if self.ready_db(db).await.is_err() {
            self.alerts.alert(AlertCode::StoreUnavailable).await;
            return Outcome::StoreFailed;
        }
        let mut outcome = Outcome::Nothing;
        if let Some(left) = moves.left {
            outcome = self.on_leave(db, home, left).await;
        }
        if let Some(joined) = moves.joined {
            locked(&self.empty_since).remove(&joined);
            if self.settings.is_hub(joined) {
                let user = facts.user_id;
                outcome = self.on_hub_join(db, home, user, joined).await;
            }
        }
        outcome
    }

    /// A member left `channel`: when it is a tracked room that is now empty, schedule its
    /// cleanup. An untracked channel is never looked at further.
    async fn on_leave(&self, db: &Db, home: u64, channel: u64) -> Outcome {
        let row = match self.store_get(db, channel).await {
            Ok(Some(row)) if row.guild == home => row,
            Ok(_) => return Outcome::Nothing,
            Err(StoreError) => {
                self.alerts.alert(AlertCode::StoreUnavailable).await;
                return Outcome::StoreFailed;
            }
        };
        match self.ports.occupancy(home, row.channel).await {
            Occupancy::Members(n) if n > 0 => Outcome::Nothing,
            Occupancy::Missing => match self.store_delete(db, row.channel).await {
                Ok(()) => Outcome::RoomGone,
                Err(StoreError) => {
                    self.alerts.alert(AlertCode::StoreFailed).await;
                    Outcome::StoreFailed
                }
            },
            Occupancy::Members(_) | Occupancy::Unknown => {
                let now = self.clock.now();
                locked(&self.empty_since).insert(row.channel, now);
                let payload = CleanupPayload {
                    guild: home,
                    channel: row.channel,
                    since: now,
                    attempt: 0,
                };
                let due = due_at(now, self.settings.empty_grace_secs());
                match self.schedule_cleanup(db, &payload, due).await {
                    Ok(()) => Outcome::CleanupScheduled,
                    Err(StoreError) => {
                        self.alerts.alert(AlertCode::ScheduleFailed).await;
                        Outcome::CleanupScheduleFailed
                    }
                }
            }
        }
    }

    /// Schedules one cleanup wake-up. A job under the same key that is still `pending` is that
    /// wake-up (same room, same due time). A finished or running job under the key must not
    /// swallow new work, so the next key variant is tried.
    async fn schedule_cleanup(
        &self,
        db: &Db,
        payload: &CleanupPayload,
        due: i64,
    ) -> Result<(), StoreError> {
        let now = self.clock.now();
        let json = cleanup_payload(payload);
        for variant in 0..MAX_KEY_VARIANTS {
            let key = cleanup_dedupe_key(payload, due, variant);
            let json = json.clone();
            let outcome = db
                .call(move |conn| {
                    queue::schedule(
                        conn,
                        &NewJob {
                            kind: KIND_CLEANUP,
                            dedupe_key: &key,
                            payload_json: &json,
                            due_at: due,
                        },
                        now,
                    )
                })
                .await;
            match outcome {
                Ok(ScheduleOutcome::Created { .. }) => {
                    self.wake_runner();
                    return Ok(());
                }
                Ok(ScheduleOutcome::Duplicate {
                    state: JobState::Pending,
                    ..
                }) => return Ok(()),
                Ok(ScheduleOutcome::Duplicate { .. }) => {}
                Err(_) => return Err(StoreError),
            }
        }
        Err(StoreError)
    }

    /// A member joined a hub channel.
    async fn on_hub_join(&self, db: &Db, home: u64, user: u64, hub: u64) -> Outcome {
        let _serial = self.create_lock.lock().await;
        let now = self.clock.now();
        let rows = match self.store_list(db, home).await {
            Ok(rows) => rows,
            Err(StoreError) => {
                self.alerts.alert(AlertCode::StoreUnavailable).await;
                return Outcome::StoreFailed;
            }
        };
        // A channel with a row is a room, never a hub, whatever the settings say.
        if rows.iter().any(|r| r.channel == hub) {
            return Outcome::Nothing;
        }
        let Some(category) = self.settings.category_id() else {
            return Outcome::NotReady(NotReady::CategoryUnset);
        };
        let mut last_create: HashMap<u64, i64> = locked(&self.last_attempt).clone();
        for row in &rows {
            let slot = last_create.entry(row.owner).or_insert(row.created_at);
            *slot = (*slot).max(row.created_at);
        }
        let state = VoiceRoomState {
            rooms: rows
                .iter()
                .map(|r| Room {
                    channel_id: r.channel,
                    owner_id: r.owner,
                    created_at: r.created_at,
                    empty_since: None,
                    occupants: 0,
                })
                .collect(),
            last_create,
            renames: Vec::new(),
        };
        let mut config = self.policy_config(home, hub, category);
        let event = JoinEvent {
            guild_id: home,
            user_id: user,
            channel_id: hub,
            at: now,
        };
        let plan = match policy::decide_join(&event, &config, &state) {
            Decision::Create(plan) => plan,
            Decision::Deny(reason) => return Outcome::Denied(reason),
            Decision::Allow | Decision::Cleanup(_) | Decision::Claimable(_) => {
                return Outcome::Nothing
            }
        };
        // From here on the attempt counts for the cooldown, success or not, so a failing
        // Discord call cannot be hammered by one member rejoining the hub.
        locked(&self.last_attempt).insert(user, now);
        match self.ports.category_overwrites(home, category).await {
            Ok(overwrites) => config.category_overwrites = overwrites,
            Err(_) => {
                self.alerts.alert(AlertCode::CategoryLookupFailed).await;
                return Outcome::CategoryLookupFailed;
            }
        }
        let request = CreateRoomRequest {
            guild_id: home,
            category_id: category,
            name: plan.name,
            overwrites: policy::build_overwrites(&config, user),
            user_limit: 0,
        };
        let channel = match self.ports.create_room(&request).await {
            Ok(channel) => channel,
            Err(_) => {
                self.alerts.alert(AlertCode::CreateFailed).await;
                return Outcome::CreateFailed;
            }
        };
        let row = RoomRow {
            channel,
            guild: home,
            owner: user,
            created_at: now,
            locked: false,
            user_limit: 0,
        };
        // The row is written BEFORE the move: a crash in between leaves a tracked empty room
        // that the reconcile deletes, never an untracked channel nobody may delete.
        if self.store_insert(db, row).await.is_err() {
            self.alerts.alert(AlertCode::StoreFailed).await;
            if self.ports.delete_room(home, channel).await.is_err() {
                self.alerts.alert(AlertCode::DeleteFailed).await;
            }
            return Outcome::StoreFailed;
        }
        if self.ports.move_member(home, user, channel).await.is_ok() {
            return Outcome::Created { channel };
        }
        self.alerts.alert(AlertCode::MoveFailed).await;
        match self.ports.delete_room(home, channel).await {
            Ok(()) | Err(PortError::NotFound) => {
                if self.store_delete(db, channel).await.is_err() {
                    self.alerts.alert(AlertCode::StoreFailed).await;
                }
                Outcome::MoveFailed {
                    channel_deleted: true,
                }
            }
            Err(_) => {
                self.alerts.alert(AlertCode::DeleteFailed).await;
                locked(&self.empty_since).insert(channel, now);
                let payload = CleanupPayload {
                    guild: home,
                    channel,
                    since: now,
                    attempt: 0,
                };
                let due = due_at(now, self.settings.empty_grace_secs());
                if self.schedule_cleanup(db, &payload, due).await.is_err() {
                    self.alerts.alert(AlertCode::ScheduleFailed).await;
                }
                Outcome::MoveFailed {
                    channel_deleted: false,
                }
            }
        }
    }

    /// The cleanup job's handler. The row and the live occupancy decide; the job is only a
    /// wake-up. `Ok` means nothing more is owed by this job (including "somebody rejoined").
    async fn run_cleanup(&self, job: ClaimedJob) -> HandlerResult {
        if true {
            // RED stub: every wake-up is a no-op.
            return Ok(());
        }
        let Ok(p) = parse_cleanup_payload(&job.payload_json) else {
            self.alerts.alert(AlertCode::PayloadInvalid).await;
            return Err(HandlerError::new("payload_invalid"));
        };
        if self.settings.mode() != Mode::Live || !self.settings.is_active() {
            return Ok(());
        }
        if self.settings.guild_id() != Some(p.guild) {
            return Ok(());
        }
        let Some(db) = self.attached_db() else {
            return Err(HandlerError::new("db_not_attached"));
        };
        if self.ready_db(&db).await.is_err() {
            return Err(HandlerError::new("store_unavailable"));
        }
        match self.store_get(&db, p.channel).await {
            Ok(Some(row)) if row.guild == p.guild => {}
            Ok(_) => return Ok(()),
            Err(StoreError) => return Err(HandlerError::new("store_failed")),
        }
        let now = self.clock.now();
        match self.ports.occupancy(p.guild, p.channel).await {
            Occupancy::Missing => self
                .store_delete(&db, p.channel)
                .await
                .map_err(|_| HandlerError::new("store_failed")),
            Occupancy::Members(n) if n > 0 => Ok(()),
            Occupancy::Unknown => {
                self.retry_later(&db, &p, now, AlertCode::DeleteFailed)
                    .await
            }
            Occupancy::Members(_) => {
                let since = locked(&self.empty_since)
                    .get(&p.channel)
                    .copied()
                    .unwrap_or(p.since);
                let config = self.policy_config(p.guild, 0, 0);
                let state = VoiceRoomState {
                    rooms: vec![Room {
                        channel_id: p.channel,
                        owner_id: 0,
                        created_at: 0,
                        empty_since: Some(since),
                        occupants: 0,
                    }],
                    ..VoiceRoomState::default()
                };
                match policy::decide_sweep(p.channel, now, &config, &state) {
                    Decision::Cleanup(_) => self.delete_and_forget(&db, &p, now).await,
                    Decision::Deny(DenyReason::EmptyGraceNotElapsed) => {
                        // Fired early: the room became empty again after this wake-up was set.
                        let due = due_at(since, self.settings.empty_grace_secs());
                        self.schedule_cleanup(&db, &CleanupPayload { since, ..p }, due)
                            .await
                            .map_err(|_| HandlerError::new("schedule_failed"))
                    }
                    _ => Ok(()),
                }
            }
        }
    }

    /// Deletes the channel, then its row (in that order: a row without a channel is dropped by
    /// the next look, a channel without a row would never be deleted).
    async fn delete_and_forget(&self, db: &Db, p: &CleanupPayload, now: i64) -> HandlerResult {
        match self.ports.delete_room(p.guild, p.channel).await {
            Ok(()) | Err(PortError::NotFound) => {
                locked(&self.empty_since).remove(&p.channel);
                self.store_delete(db, p.channel)
                    .await
                    .map_err(|_| HandlerError::new("store_failed"))
            }
            Err(_) => self.retry_later(db, p, now, AlertCode::DeleteFailed).await,
        }
    }

    /// A Discord failure: schedule the next attempt of this chain with a growing delay; give
    /// up (alerted, row kept) after [`MAX_CLEANUP_ATTEMPTS`]. Only the first failure of a
    /// chain alerts, so a long outage is one alert and not one per retry.
    async fn retry_later(
        &self,
        db: &Db,
        p: &CleanupPayload,
        now: i64,
        alert: AlertCode,
    ) -> HandlerResult {
        if p.attempt + 1 >= MAX_CLEANUP_ATTEMPTS {
            self.alerts.alert(AlertCode::CleanupGaveUp).await;
            return Ok(());
        }
        if p.attempt == 0 {
            self.alerts.alert(alert).await;
        }
        let next = CleanupPayload {
            attempt: p.attempt + 1,
            ..*p
        };
        let due = now.saturating_add(retry_delay_secs(p.attempt));
        match self.schedule_cleanup(db, &next, due).await {
            Ok(()) => Ok(()),
            Err(StoreError) => {
                self.alerts.alert(AlertCode::ScheduleFailed).await;
                Err(HandlerError::new("schedule_failed"))
            }
        }
    }

    /// Restart reconcile over the rows of the home guild, and nothing else: a channel that has
    /// no row is never listed, deleted or modified.
    async fn reconcile(&self, core_db: Option<&Db>) -> ReconcileReport {
        if true {
            // RED stub: reconciles nothing.
            return ReconcileReport {
                skipped: true,
                ..ReconcileReport::default()
            };
        }
        let mut report = ReconcileReport::default();
        let Some(home) = self.settings.guild_id() else {
            report.skipped = true;
            return report;
        };
        if !self.settings.is_active() || self.settings.mode() != Mode::Live {
            report.skipped = true;
            return report;
        }
        let Some(db) = core_db else {
            self.alerts.alert(AlertCode::StoreUnavailable).await;
            report.skipped = true;
            return report;
        };
        self.attach_db(db);
        let rows = match self.ready_db(db).await {
            Ok(()) => self.store_list(db, home).await,
            Err(e) => Err(e),
        };
        let Ok(rows) = rows else {
            self.alerts.alert(AlertCode::StoreUnavailable).await;
            report.skipped = true;
            return report;
        };
        let now = self.clock.now();
        for row in rows {
            match self.ports.occupancy(home, row.channel).await {
                Occupancy::Members(n) if n > 0 => report.kept += 1,
                Occupancy::Missing => match self.store_delete(db, row.channel).await {
                    Ok(()) => report.dropped_missing += 1,
                    Err(StoreError) => {
                        self.alerts.alert(AlertCode::StoreFailed).await;
                        report.failed += 1;
                    }
                },
                Occupancy::Members(_) => match self.ports.delete_room(home, row.channel).await {
                    Ok(()) | Err(PortError::NotFound) => {
                        match self.store_delete(db, row.channel).await {
                            Ok(()) => report.deleted_empty += 1,
                            Err(StoreError) => {
                                self.alerts.alert(AlertCode::StoreFailed).await;
                                report.failed += 1;
                            }
                        }
                    }
                    Err(_) => {
                        self.alerts.alert(AlertCode::DeleteFailed).await;
                        self.reconcile_reschedule(db, home, row.channel, now, &mut report)
                            .await;
                    }
                },
                Occupancy::Unknown => {
                    self.reconcile_reschedule(db, home, row.channel, now, &mut report)
                        .await;
                }
            }
        }
        report
    }

    async fn reconcile_reschedule(
        &self,
        db: &Db,
        home: u64,
        channel: u64,
        now: i64,
        report: &mut ReconcileReport,
    ) {
        let payload = CleanupPayload {
            guild: home,
            channel,
            since: now,
            attempt: 0,
        };
        let due = due_at(now, self.settings.empty_grace_secs());
        match self.schedule_cleanup(db, &payload, due).await {
            Ok(()) => report.rescheduled += 1,
            Err(StoreError) => {
                self.alerts.alert(AlertCode::ScheduleFailed).await;
                report.failed += 1;
            }
        }
    }
}

/// The voice-rooms feature. Implements the registry's `Feature` hook; it is NOT registered
/// anywhere.
pub struct VoiceRoomsFeature {
    inner: Arc<Inner>,
}

impl VoiceRoomsFeature {
    /// Builds the feature.
    pub fn new(deps: VoiceDeps) -> Self {
        VoiceRoomsFeature {
            inner: Arc::new(Inner {
                settings: deps.settings,
                ports: deps.ports,
                alerts: deps.alerts,
                clock: deps.clock,
                bot_user_id: deps.bot_user_id,
                wake: Mutex::new(None),
                tally: Mutex::new(BTreeMap::new()),
                create_lock: tokio::sync::Mutex::new(()),
                last_attempt: Mutex::new(HashMap::new()),
                empty_since: Mutex::new(HashMap::new()),
                migrated: AtomicBool::new(false),
                db: Mutex::new(None),
            }),
        }
    }

    /// Handles one voice-state update. `core_db` is the shared database (needed in live mode).
    pub async fn process(&self, facts: VoiceFacts, core_db: Option<&Db>) -> Outcome {
        self.inner.process(facts, core_db).await
    }

    /// Restart reconcile: drops rows whose channel is gone and deletes empty tracked channels.
    /// Idempotent. Channels without a row are never touched.
    pub async fn reconcile(&self, core_db: Option<&Db>) -> ReconcileReport {
        self.inner.reconcile(core_db).await
    }

    /// Runs one cleanup job.
    pub async fn run_cleanup(&self, job: ClaimedJob) -> HandlerResult {
        self.inner.run_cleanup(job).await
    }

    /// Shadow-mode tally: `(code, count)` sorted by code.
    pub fn shadow_summary(&self) -> Vec<(&'static str, u64)> {
        locked(&self.inner.tally)
            .iter()
            .map(|(key, count)| (*key, *count))
            .collect()
    }

    /// Registers the cleanup job kind with [`JOB_KINDS`]' restart policy.
    pub fn register_job_kinds(&self, registry: &mut Registry) -> Result<(), RunnerError> {
        for (kind, policy) in JOB_KINDS {
            let inner = self.inner.clone();
            registry.register(kind, policy, move |job| {
                let inner = inner.clone();
                async move { inner.run_cleanup(job).await }
            })?;
        }
        Ok(())
    }
}

impl<X: Sync + 'static, D: Sync + 'static> Feature<X, D> for VoiceRoomsFeature {
    fn name(&self) -> &'static str {
        "voice_rooms"
    }

    fn enabled(&self) -> bool {
        self.inner.settings.is_active()
    }

    /// Only `live` uses the database: `shadow` reads, writes and schedules nothing.
    fn needs_core_db(&self) -> bool {
        self.inner.settings.is_active() && self.inner.settings.mode() == Mode::Live
    }

    fn register_jobs(&self, registry: &mut Registry) -> Result<(), RunnerError> {
        self.register_job_kinds(registry)
    }

    fn jobs_started(&self, wake: JobSignal) {
        *locked(&self.inner.wake) = Some(wake);
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a X,
        event: &'a serenity::FullEvent,
        _data: &'a D,
        _guild: Option<u64>,
        core_db: Option<&'a Db>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match event {
                serenity::FullEvent::VoiceStateUpdate { old, new } => {
                    let facts = facts_from_voice(old.as_ref(), new);
                    let _ = self.inner.process(facts, core_db).await;
                }
                serenity::FullEvent::CacheReady { .. } => {
                    let _ = self.inner.reconcile(core_db).await;
                }
                _ => {}
            }
        })
    }
}

/// Facts of a serenity voice-state update. The member's bot flag comes from the update's
/// member; when the update carries none the flag is unknown and the event is ignored.
pub fn facts_from_voice(
    old: Option<&serenity::VoiceState>,
    new: &serenity::VoiceState,
) -> VoiceFacts {
    VoiceFacts {
        guild_id: new.guild_id.map(|g| g.get()),
        user_id: new.user_id.get(),
        is_bot: new.member.as_ref().map(|m| m.user.bot),
        old_channel: old.and_then(|v| v.channel_id).map(|c| c.get()),
        new_channel: new.channel_id.map(|c| c.get()),
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

/// The cached occupancy of a channel, if the cache knows the guild and the channel. The guard
/// on the cache is dropped before this returns, so nothing is held across an await.
fn cached_occupancy(
    cache: &serenity::Cache,
    guild: serenity::GuildId,
    channel: serenity::ChannelId,
) -> Option<usize> {
    let guild = cache.guild(guild)?;
    if !guild.channels.contains_key(&channel) {
        return None;
    }
    Some(
        guild
            .voice_states
            .values()
            .filter(|state| state.channel_id == Some(channel))
            .count(),
    )
}

fn to_serenity_overwrite(
    guild_id: u64,
    o: &RoomOverwrite,
) -> Option<serenity::PermissionOverwrite> {
    let kind = match o.target {
        OverwriteTarget::Everyone => {
            serenity::PermissionOverwriteType::Role(serenity::RoleId::new(guild_id))
        }
        OverwriteTarget::Role(id) if id != 0 => {
            serenity::PermissionOverwriteType::Role(serenity::RoleId::new(id))
        }
        OverwriteTarget::Member(id) if id != 0 => {
            serenity::PermissionOverwriteType::Member(serenity::UserId::new(id))
        }
        _ => return None,
    };
    Some(serenity::PermissionOverwrite {
        allow: serenity::Permissions::from_bits_truncate(o.allow),
        deny: serenity::Permissions::from_bits_truncate(o.deny),
        kind,
    })
}

fn from_serenity_overwrite(guild_id: u64, o: &serenity::PermissionOverwrite) -> RoomOverwrite {
    let target = match o.kind {
        serenity::PermissionOverwriteType::Role(id) if id.get() == guild_id => {
            OverwriteTarget::Everyone
        }
        serenity::PermissionOverwriteType::Role(id) => OverwriteTarget::Role(id.get()),
        serenity::PermissionOverwriteType::Member(id) => OverwriteTarget::Member(id.get()),
        _ => OverwriteTarget::Role(0),
    };
    RoomOverwrite {
        target,
        allow: o.allow.bits(),
        deny: o.deny.bits(),
    }
}

/// Discord calls over serenity's HTTP client and cache.
pub struct SerenityRoomPorts {
    /// The serenity HTTP client.
    pub http: Arc<serenity::Http>,
    /// The serenity cache.
    pub cache: Arc<serenity::Cache>,
    /// Channel that receives staff alerts, if one is configured.
    pub staff_alert_channel: Option<u64>,
}

#[serenity::async_trait]
impl RoomPorts for SerenityRoomPorts {
    async fn category_overwrites(
        &self,
        guild_id: u64,
        category_id: u64,
    ) -> Result<Vec<RoomOverwrite>, PortError> {
        if guild_id == 0 || category_id == 0 {
            return Err(PortError::Unavailable);
        }
        match self
            .http
            .get_channel(serenity::ChannelId::new(category_id))
            .await
        {
            Ok(serenity::Channel::Guild(found))
                if found.guild_id.get() == guild_id
                    && found.kind == serenity::ChannelType::Category =>
            {
                Ok(found
                    .permission_overwrites
                    .iter()
                    .map(|o| from_serenity_overwrite(guild_id, o))
                    .collect())
            }
            Ok(_) => Err(PortError::Unavailable),
            Err(e) => Err(port_error_from_serenity(&e)),
        }
    }

    async fn create_room(&self, req: &CreateRoomRequest) -> Result<u64, PortError> {
        if req.guild_id == 0 || req.category_id == 0 {
            return Err(PortError::Unavailable);
        }
        let mut overwrites = Vec::with_capacity(req.overwrites.len());
        for o in &req.overwrites {
            // An overwrite that cannot be expressed is a refusal, never a silent drop.
            overwrites.push(to_serenity_overwrite(req.guild_id, o).ok_or(PortError::Unavailable)?);
        }
        let builder = serenity::builder::CreateChannel::new(req.name.clone())
            .kind(serenity::ChannelType::Voice)
            .category(serenity::ChannelId::new(req.category_id))
            .permissions(overwrites)
            .user_limit(req.user_limit)
            .audit_log_reason("voice room created");
        serenity::GuildId::new(req.guild_id)
            .create_channel(&self.http, builder)
            .await
            .map(|channel| channel.id.get())
            .map_err(|e| port_error_from_serenity(&e))
    }

    async fn move_member(
        &self,
        guild_id: u64,
        user_id: u64,
        channel_id: u64,
    ) -> Result<(), PortError> {
        if guild_id == 0 || user_id == 0 || channel_id == 0 {
            return Err(PortError::Unavailable);
        }
        serenity::GuildId::new(guild_id)
            .move_member(
                &self.http,
                serenity::UserId::new(user_id),
                serenity::ChannelId::new(channel_id),
            )
            .await
            .map(|_| ())
            .map_err(|e| port_error_from_serenity(&e))
    }

    async fn delete_room(&self, _guild_id: u64, channel_id: u64) -> Result<(), PortError> {
        if channel_id == 0 {
            return Err(PortError::Unavailable);
        }
        self.http
            .delete_channel(
                serenity::ChannelId::new(channel_id),
                Some("voice room cleanup"),
            )
            .await
            .map(|_| ())
            .map_err(|e| port_error_from_serenity(&e))
    }

    async fn occupancy(&self, guild_id: u64, channel_id: u64) -> Occupancy {
        if guild_id == 0 || channel_id == 0 {
            return Occupancy::Unknown;
        }
        let guild = serenity::GuildId::new(guild_id);
        let channel = serenity::ChannelId::new(channel_id);
        if let Some(count) = cached_occupancy(&self.cache, guild, channel) {
            return Occupancy::Members(count);
        }
        // Not in the cache: that alone does not prove the channel is gone (the cache can lag a
        // channel the bot just created), so ask Discord before saying `Missing`.
        match self.http.get_channel(channel).await {
            Ok(_) => match cached_occupancy(&self.cache, guild, channel) {
                Some(count) => Occupancy::Members(count),
                None => Occupancy::Unknown,
            },
            Err(e) => match port_error_from_serenity(&e) {
                PortError::NotFound => Occupancy::Missing,
                _ => Occupancy::Unknown,
            },
        }
    }
}

#[serenity::async_trait]
impl StaffAlerter for SerenityRoomPorts {
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
    use crate::jobs::queue::RetryPolicy;
    use crate::jobs::runner::{Runner, RunnerConfig, ShutdownHandle};
    use crate::storage;
    use crate::voice_rooms::config::{self, load};
    use crate::voice_rooms::policy::{FORBIDDEN_OWNER_RIGHTS, OWNER_RIGHTS};
    use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
    use std::time::Duration;
    use tokio::sync::watch;

    // Synthetic ids, far from any real Discord snowflake.
    const GUILD: u64 = 6_300_000_000_000_001;
    const OTHER_GUILD: u64 = 6_300_000_000_000_002;
    const HUB: u64 = 6_300_000_000_000_003;
    const HUB2: u64 = 6_300_000_000_000_004;
    const CAT: u64 = 6_300_000_000_000_005;
    const BOT: u64 = 6_300_000_000_000_006;
    const OTHER_CAT: u64 = 6_300_000_000_000_007;
    const U1: u64 = 6_300_000_000_000_011;
    const U2: u64 = 6_300_000_000_000_012;
    const U3: u64 = 6_300_000_000_000_013;
    const FIRST_ROOM: u64 = 6_400_000_000_000_001;
    const UNTRACKED_A: u64 = 6_300_000_000_000_021;
    const UNTRACKED_B: u64 = 6_300_000_000_000_022;
    const T0: i64 = 1_800_000_000;

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
        Overwrites,
        Create(CreateRoomRequest),
        Move { user: u64, channel: u64 },
        Delete(u64),
        Alert(AlertCode),
    }

    #[derive(Default)]
    struct World {
        channels: BTreeMap<u64, (u64, String)>,
        loc: HashMap<u64, u64>,
        created: BTreeSet<u64>,
        deleted: Vec<u64>,
    }

    /// A fake Discord: a tiny world of channels and members, every call recorded, failures on
    /// script (or, for the liveness test, from a seeded generator).
    struct Net {
        world: Mutex<World>,
        calls: Mutex<Vec<Call>>,
        category: Mutex<Result<Vec<RoomOverwrite>, PortError>>,
        scripts: Mutex<HashMap<&'static str, VecDeque<PortError>>>,
        always: Mutex<HashMap<&'static str, PortError>>,
        unknown: Mutex<HashSet<u64>>,
        rng: Mutex<Option<(u64, [u64; 4])>>,
        next_channel: AtomicU64,
        probe: Option<Db>,
        row_at_move: Mutex<Vec<bool>>,
    }

    impl Net {
        fn build(probe: Option<Db>) -> Arc<Net> {
            let net = Arc::new(Net {
                world: Mutex::new(World::default()),
                calls: Mutex::new(Vec::new()),
                category: Mutex::new(Ok(Vec::new())),
                scripts: Mutex::new(HashMap::new()),
                always: Mutex::new(HashMap::new()),
                unknown: Mutex::new(HashSet::new()),
                rng: Mutex::new(None),
                next_channel: AtomicU64::new(FIRST_ROOM),
                probe,
                row_at_move: Mutex::new(Vec::new()),
            });
            net.add_channel(HUB, CAT, "Hub");
            net.add_channel(HUB2, CAT, "Hub 2");
            net
        }
        fn new() -> Arc<Net> {
            Net::build(None)
        }
        fn add_channel(&self, id: u64, category: u64, name: &str) {
            locked(&self.world)
                .channels
                .insert(id, (category, name.to_string()));
        }
        fn put(&self, user: u64, channel: u64) {
            locked(&self.world).loc.insert(user, channel);
        }
        fn unplug(&self, user: u64) {
            locked(&self.world).loc.remove(&user);
        }
        fn at(&self, user: u64) -> Option<u64> {
            locked(&self.world).loc.get(&user).copied()
        }
        fn exists(&self, channel: u64) -> bool {
            locked(&self.world).channels.contains_key(&channel)
        }
        fn members(&self, channel: u64) -> usize {
            locked(&self.world)
                .loc
                .values()
                .filter(|c| **c == channel)
                .count()
        }
        fn calls(&self) -> Vec<Call> {
            locked(&self.calls).clone()
        }
        fn clear_calls(&self) {
            locked(&self.calls).clear();
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
        fn non_alert_calls(&self) -> Vec<Call> {
            self.calls()
                .into_iter()
                .filter(|c| !matches!(c, Call::Alert(_)))
                .collect()
        }
        fn deletes(&self) -> Vec<u64> {
            self.calls()
                .into_iter()
                .filter_map(|c| match c {
                    Call::Delete(ch) => Some(ch),
                    _ => None,
                })
                .collect()
        }
        fn creates(&self) -> Vec<CreateRoomRequest> {
            self.calls()
                .into_iter()
                .filter_map(|c| match c {
                    Call::Create(r) => Some(r),
                    _ => None,
                })
                .collect()
        }
        fn script(&self, op: &'static str, failures: &[PortError]) {
            locked(&self.scripts)
                .entry(op)
                .or_default()
                .extend(failures.iter().copied());
        }
        fn fail_always(&self, op: &'static str, error: PortError) {
            locked(&self.always).insert(op, error);
        }
        fn heal(&self, op: &'static str) {
            locked(&self.always).remove(op);
        }
        fn fault(&self, op: &'static str, slot: usize) -> Option<PortError> {
            if let Some(e) = locked(&self.scripts)
                .get_mut(op)
                .and_then(|q| q.pop_front())
            {
                return Some(e);
            }
            if let Some(e) = locked(&self.always).get(op) {
                return Some(*e);
            }
            let mut rng = locked(&self.rng);
            let (state, rates) = rng.as_mut()?;
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (((*state >> 33) % 100) < rates[slot]).then_some(PortError::Unavailable)
        }
    }

    #[poise::serenity_prelude::async_trait]
    impl RoomPorts for Net {
        async fn category_overwrites(
            &self,
            _guild_id: u64,
            _category_id: u64,
        ) -> Result<Vec<RoomOverwrite>, PortError> {
            locked(&self.calls).push(Call::Overwrites);
            if let Some(e) = self.fault("overwrites", 0) {
                return Err(e);
            }
            self.category.lock().unwrap().clone()
        }

        async fn create_room(&self, req: &CreateRoomRequest) -> Result<u64, PortError> {
            locked(&self.calls).push(Call::Create(req.clone()));
            if let Some(e) = self.fault("create", 1) {
                return Err(e);
            }
            let id = self.next_channel.fetch_add(1, SeqCst);
            let mut world = locked(&self.world);
            world
                .channels
                .insert(id, (req.category_id, req.name.clone()));
            world.created.insert(id);
            Ok(id)
        }

        async fn move_member(
            &self,
            _guild_id: u64,
            user_id: u64,
            channel_id: u64,
        ) -> Result<(), PortError> {
            locked(&self.calls).push(Call::Move {
                user: user_id,
                channel: channel_id,
            });
            if let Some(db) = &self.probe {
                let n: i64 = db
                    .call(move |c| {
                        c.query_row(
                            "SELECT COUNT(*) FROM voice_rooms WHERE channel = ?1",
                            [channel_id.to_string()],
                            |r| r.get(0),
                        )
                    })
                    .await
                    .unwrap_or(-1);
                locked(&self.row_at_move).push(n == 1);
            }
            if let Some(e) = self.fault("move", 2) {
                return Err(e);
            }
            let mut world = locked(&self.world);
            if !world.channels.contains_key(&channel_id) {
                return Err(PortError::NotFound);
            }
            if !world.loc.contains_key(&user_id) {
                return Err(PortError::Unavailable);
            }
            world.loc.insert(user_id, channel_id);
            Ok(())
        }

        async fn delete_room(&self, _guild_id: u64, channel_id: u64) -> Result<(), PortError> {
            locked(&self.calls).push(Call::Delete(channel_id));
            if let Some(e) = self.fault("delete", 3) {
                return Err(e);
            }
            let mut world = locked(&self.world);
            if world.channels.remove(&channel_id).is_none() {
                return Err(PortError::NotFound);
            }
            world.loc.retain(|_, c| *c != channel_id);
            world.deleted.push(channel_id);
            Ok(())
        }

        async fn occupancy(&self, _guild_id: u64, channel_id: u64) -> Occupancy {
            if locked(&self.unknown).contains(&channel_id) {
                return Occupancy::Unknown;
            }
            if !self.exists(channel_id) {
                return Occupancy::Missing;
            }
            Occupancy::Members(self.members(channel_id))
        }
    }

    #[poise::serenity_prelude::async_trait]
    impl StaffAlerter for Net {
        async fn alert(&self, code: AlertCode) {
            locked(&self.calls).push(Call::Alert(code));
        }
    }

    // -------------------------------------------------------------- helpers

    fn mem_db() -> Db {
        let mut c = storage::open(":memory:", &[]).unwrap();
        queue::init(&mut c).unwrap();
        c.into()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("vr2-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file_db(path: &str) -> Db {
        let mut c = storage::open(path, &[]).unwrap();
        queue::init(&mut c).unwrap();
        c.into()
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct JobRow {
        id: i64,
        kind: String,
        key: String,
        payload: String,
        due: i64,
        state: String,
    }

    async fn jobs(db: &Db) -> Vec<JobRow> {
        db.call(|c| {
            let mut st = c.prepare(
                "SELECT id, kind, dedupe_key, payload_json, due_at, state FROM jobs ORDER BY id",
            )?;
            let rows = st
                .query_map([], |r| {
                    Ok(JobRow {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        key: r.get(2)?,
                        payload: r.get(3)?,
                        due: r.get(4)?,
                        state: r.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, rusqlite::Error>(rows)
        })
        .await
        .unwrap()
    }

    async fn pending(db: &Db) -> Vec<JobRow> {
        jobs(db)
            .await
            .into_iter()
            .filter(|j| j.state == "pending")
            .collect()
    }

    async fn rows(db: &Db) -> Vec<RoomRow> {
        db.call(|c| repository::list(c, GUILD)).await.unwrap()
    }

    async fn all_rows(db: &Db) -> usize {
        db.call(|c| {
            c.query_row("SELECT COUNT(*) FROM voice_rooms", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .await
        .unwrap() as usize
    }

    async fn add_row(db: &Db, channel: u64, guild: u64, owner: u64, created_at: i64) {
        db.call(move |c| {
            repository::migrate(c).unwrap();
            repository::insert(
                c,
                &RoomRow {
                    channel,
                    guild,
                    owner,
                    created_at,
                    locked: false,
                    user_limit: 0,
                },
            )
        })
        .await
        .unwrap();
    }

    fn settings(mode: &str, extra: &[(&str, &str)]) -> VoiceSettings {
        let hub = format!("{HUB},{HUB2}");
        let cat = CAT.to_string();
        let mut pairs: Vec<(&str, &str)> = vec![
            (config::KEY_MODE, mode),
            (config::KEY_HUB_CHANNEL_IDS, hub.as_str()),
            (config::KEY_CATEGORY_ID, cat.as_str()),
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

    fn feature(
        net: &Arc<Net>,
        clock: &Arc<FakeClock>,
        settings: VoiceSettings,
    ) -> VoiceRoomsFeature {
        VoiceRoomsFeature::new(VoiceDeps {
            settings,
            ports: net.clone(),
            alerts: net.clone(),
            clock: clock.clone(),
            bot_user_id: Some(BOT),
        })
    }

    struct Harness {
        db: Db,
        net: Arc<Net>,
        clock: Arc<FakeClock>,
        feat: VoiceRoomsFeature,
        wakes: Arc<AtomicUsize>,
    }

    fn harness_with(extra: &[(&str, &str)]) -> Harness {
        let db = mem_db();
        let net = Net::build(Some(db.clone()));
        let clock = FakeClock::new();
        let feat = feature(&net, &clock, settings("live", extra));
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        <VoiceRoomsFeature as Feature<(), ()>>::jobs_started(
            &feat,
            Arc::new(move || {
                counter.fetch_add(1, SeqCst);
            }),
        );
        Harness {
            db,
            net,
            clock,
            feat,
            wakes,
        }
    }

    fn harness() -> Harness {
        harness_with(&[])
    }

    fn facts(user: u64, old: Option<u64>, new: Option<u64>) -> VoiceFacts {
        VoiceFacts {
            guild_id: Some(GUILD),
            user_id: user,
            is_bot: Some(false),
            old_channel: old,
            new_channel: new,
        }
    }

    impl Harness {
        /// A member connects to `channel` from nowhere (Discord updates the member first,
        /// then tells the bot).
        async fn connect(&self, user: u64, channel: u64) -> Outcome {
            self.net.put(user, channel);
            self.feat
                .process(facts(user, None, Some(channel)), Some(&self.db))
                .await
        }
        /// A member moves from `old` to `new`.
        async fn switch(&self, user: u64, old: u64, new: u64) -> Outcome {
            self.net.put(user, new);
            self.feat
                .process(facts(user, Some(old), Some(new)), Some(&self.db))
                .await
        }
        /// A member disconnects from `old`.
        async fn disconnect(&self, user: u64, old: u64) -> Outcome {
            self.net.unplug(user);
            self.feat
                .process(facts(user, Some(old), None), Some(&self.db))
                .await
        }
        /// Creates a room for `user` through the real flow and returns its channel.
        async fn room_for(&self, user: u64) -> u64 {
            match self.connect(user, HUB).await {
                Outcome::Created { channel } => channel,
                other => panic!("expected a room, got {other:?}"),
            }
        }
    }

    fn claimed(row: &JobRow) -> ClaimedJob {
        ClaimedJob {
            id: row.id,
            kind: row.kind.clone(),
            payload_json: row.payload.clone(),
            attempts: 1,
        }
    }

    fn payload(channel: u64, since: i64) -> ClaimedJob {
        ClaimedJob {
            id: 1,
            kind: KIND_CLEANUP.to_string(),
            payload_json: cleanup_payload(&CleanupPayload {
                guild: GUILD,
                channel,
                since,
                attempt: 0,
            }),
            attempts: 1,
        }
    }

    // ------------------------------------------------------------ pure parts

    #[test]
    fn classify_ignores_other_guilds_bots_and_non_moves_in_a_fixed_order() {
        let base = facts(U1, None, Some(HUB));
        assert_eq!(
            classify(&base, GUILD, Some(BOT)),
            Ok(Moves {
                left: None,
                joined: Some(HUB)
            })
        );
        let mut f = base;
        f.guild_id = Some(OTHER_GUILD);
        assert_eq!(classify(&f, GUILD, Some(BOT)), Err(IgnoreCode::WrongGuild));
        f.guild_id = None;
        assert_eq!(classify(&f, GUILD, Some(BOT)), Err(IgnoreCode::WrongGuild));
        // An unset or zero home guild matches nothing, not even a zero guild id.
        assert_eq!(classify(&base, 0, Some(BOT)), Err(IgnoreCode::WrongGuild));
        let mut zero = base;
        zero.guild_id = Some(0);
        assert_eq!(classify(&zero, 0, None), Err(IgnoreCode::WrongGuild));
        let mut f = base;
        f.user_id = 0;
        assert_eq!(classify(&f, GUILD, None), Err(IgnoreCode::InvalidUser));
        let mut f = base;
        f.user_id = BOT;
        assert_eq!(classify(&f, GUILD, Some(BOT)), Err(IgnoreCode::Bot));
        let mut f = base;
        f.is_bot = Some(true);
        assert_eq!(classify(&f, GUILD, Some(BOT)), Err(IgnoreCode::Bot));
        let mut f = base;
        f.is_bot = None;
        assert_eq!(
            classify(&f, GUILD, Some(BOT)),
            Err(IgnoreCode::UnknownMember)
        );
        assert_eq!(
            classify(&facts(U1, Some(HUB), Some(HUB)), GUILD, None),
            Err(IgnoreCode::NoChannelChange)
        );
        assert_eq!(
            classify(&facts(U1, None, None), GUILD, None),
            Err(IgnoreCode::NoChannelChange)
        );
        assert_eq!(
            classify(&facts(U1, Some(UNTRACKED_A), Some(HUB)), GUILD, None),
            Ok(Moves {
                left: Some(UNTRACKED_A),
                joined: Some(HUB)
            })
        );
        // A zero channel id is no channel.
        assert_eq!(
            classify(&facts(U1, Some(0), Some(HUB)), GUILD, None),
            Ok(Moves {
                left: None,
                joined: Some(HUB)
            })
        );
    }

    #[test]
    fn ignore_and_alert_codes_are_stable_distinct_and_the_text_is_the_code_only() {
        let mut seen = HashSet::new();
        for (code, expected) in [
            (AlertCode::CategoryLookupFailed, "vc_category_lookup_failed"),
            (AlertCode::CreateFailed, "vc_create_failed"),
            (AlertCode::StoreFailed, "vc_store_failed"),
            (AlertCode::MoveFailed, "vc_move_failed"),
            (AlertCode::DeleteFailed, "vc_delete_failed"),
            (AlertCode::ScheduleFailed, "vc_schedule_failed"),
            (AlertCode::CleanupGaveUp, "vc_cleanup_gave_up"),
            (AlertCode::PayloadInvalid, "vc_payload_invalid"),
            (AlertCode::StoreUnavailable, "vc_store_unavailable"),
        ] {
            assert_eq!(code.code(), expected);
            assert!(seen.insert(expected));
            assert_eq!(alert_text(code), format!("voice rooms alert: {expected}"));
        }
        let mut seen = HashSet::new();
        for code in [
            IgnoreCode::WrongGuild,
            IgnoreCode::Bot,
            IgnoreCode::UnknownMember,
            IgnoreCode::InvalidUser,
            IgnoreCode::NoChannelChange,
            IgnoreCode::NotHubJoin,
        ] {
            assert!(seen.insert(code.code()));
        }
    }

    #[test]
    fn the_cleanup_payload_holds_ids_and_counters_and_nothing_else() {
        let p = CleanupPayload {
            guild: GUILD,
            channel: FIRST_ROOM,
            since: T0,
            attempt: 3,
        };
        let raw = cleanup_payload(&p);
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"guild": GUILD, "channel": FIRST_ROOM, "since": T0, "attempt": 3})
        );
        assert_eq!(parse_cleanup_payload(&raw), Ok(p));
    }

    #[test]
    fn cleanup_payload_parsing_accepts_only_the_plain_shape() {
        let ok = r#"{"guild":1,"channel":2,"since":0,"attempt":0}"#;
        assert!(parse_cleanup_payload(ok).is_ok());
        let max = format!(
            r#"{{"guild":{0},"channel":{0},"since":{1},"attempt":{2}}}"#,
            u64::MAX,
            i64::MAX,
            MAX_CLEANUP_ATTEMPTS
        );
        assert!(parse_cleanup_payload(&max).is_ok());
        let bad = [
            "",
            "null",
            "[]",
            "[1,2,0,0]",
            "{}",
            r#"{"guild":1,"channel":2,"since":0}"#,
            r#"{"guild":1,"channel":2,"since":0,"attempt":0,"x":1}"#,
            r#"{"guild":"1","channel":2,"since":0,"attempt":0}"#,
            r#"{"guild":0,"channel":2,"since":0,"attempt":0}"#,
            r#"{"guild":1,"channel":0,"since":0,"attempt":0}"#,
            r#"{"guild":1,"channel":2,"since":-1,"attempt":0}"#,
            r#"{"guild":1,"channel":2,"since":0,"attempt":-1}"#,
            r#"{"guild":1,"channel":2,"since":0,"attempt":9}"#,
            r#"{"guild":1.5,"channel":2,"since":0,"attempt":0}"#,
            r#"{"guild":1,"guild":2,"channel":2,"since":0,"attempt":0}"#,
            r#"{"guild":1,"channel":2,"since":0,"attempt":0} trailing"#,
            r#"{"guild":18446744073709551616,"channel":2,"since":0,"attempt":0}"#,
        ];
        for raw in bad {
            assert_eq!(parse_cleanup_payload(raw), Err(PayloadInvalid), "{raw:?}");
        }
    }

    #[test]
    fn dedupe_keys_are_valid_for_the_queue_and_differ_per_room_due_attempt_and_variant() {
        let p = CleanupPayload {
            guild: GUILD,
            channel: FIRST_ROOM,
            since: T0,
            attempt: 0,
        };
        let mut keys = HashSet::new();
        for (channel, due, attempt, variant) in [
            (FIRST_ROOM, T0, 0, 0),
            (FIRST_ROOM + 1, T0, 0, 0),
            (FIRST_ROOM, T0 + 1, 0, 0),
            (FIRST_ROOM, T0, 1, 0),
            (FIRST_ROOM, T0, 0, 1),
            (FIRST_ROOM, T0, 0, 2),
        ] {
            let key = cleanup_dedupe_key(
                &CleanupPayload {
                    channel,
                    attempt,
                    ..p
                },
                due,
                variant,
            );
            assert!(keys.insert(key.clone()), "{key}");
            assert!(key.len() <= queue::MAX_DEDUPE_KEY_LEN);
            assert!(key.bytes().all(|b| (0x21..=0x7e).contains(&b)));
        }
        let widest = cleanup_dedupe_key(
            &CleanupPayload {
                guild: u64::MAX,
                channel: u64::MAX,
                since: i64::MAX,
                attempt: MAX_CLEANUP_ATTEMPTS,
            },
            i64::MAX,
            MAX_KEY_VARIANTS,
        );
        assert!(widest.len() <= queue::MAX_DEDUPE_KEY_LEN);
    }

    #[test]
    fn the_due_time_is_since_plus_grace_and_saturates() {
        assert_eq!(due_at(T0, 10), T0 + 10);
        assert_eq!(due_at(T0, 0), T0);
        assert_eq!(due_at(i64::MAX, 10), i64::MAX);
        assert_eq!(due_at(5, -3), 5);
        assert_eq!(due_at(-9, 2), 0);
    }

    #[test]
    fn the_retry_delay_doubles_and_is_capped() {
        assert_eq!(retry_delay_secs(0), 15);
        assert_eq!(retry_delay_secs(1), 30);
        assert_eq!(retry_delay_secs(2), 60);
        assert_eq!(retry_delay_secs(5), 480);
        assert_eq!(retry_delay_secs(6), 900);
        assert_eq!(retry_delay_secs(7), 900);
        assert_eq!(retry_delay_secs(u32::MAX), 900);
    }

    #[test]
    fn http_statuses_map_to_port_errors() {
        assert_eq!(port_error_from_status(Some(404)), PortError::NotFound);
        assert_eq!(port_error_from_status(Some(403)), PortError::Forbidden);
        for other in [Some(400), Some(401), Some(429), Some(500), Some(200), None] {
            assert_eq!(port_error_from_status(other), PortError::Unavailable);
        }
    }

    #[test]
    fn the_overwrite_mapping_round_trips_and_refuses_a_zero_id() {
        let everyone = RoomOverwrite {
            target: OverwriteTarget::Everyone,
            allow: 0,
            deny: policy::CONNECT,
        };
        let s = to_serenity_overwrite(GUILD, &everyone).unwrap();
        assert_eq!(from_serenity_overwrite(GUILD, &s), everyone);
        let member = RoomOverwrite {
            target: OverwriteTarget::Member(U1),
            allow: OWNER_RIGHTS,
            deny: FORBIDDEN_OWNER_RIGHTS,
        };
        let s = to_serenity_overwrite(GUILD, &member).unwrap();
        assert_eq!(from_serenity_overwrite(GUILD, &s), member);
        let role = RoomOverwrite {
            target: OverwriteTarget::Role(U2),
            allow: policy::SPEAK,
            deny: 0,
        };
        let s = to_serenity_overwrite(GUILD, &role).unwrap();
        assert_eq!(from_serenity_overwrite(GUILD, &s), role);
        for zero in [OverwriteTarget::Role(0), OverwriteTarget::Member(0)] {
            let o = RoomOverwrite {
                target: zero,
                allow: 0,
                deny: 0,
            };
            assert!(to_serenity_overwrite(GUILD, &o).is_none());
        }
    }

    #[tokio::test]
    async fn the_feature_hooks_follow_the_mode() {
        for (mode, enabled, db) in [
            ("off", false, false),
            ("shadow", true, false),
            ("live", true, true),
        ] {
            let net = Net::new();
            let clock = FakeClock::new();
            let feat = feature(&net, &clock, settings(mode, &[]));
            assert_eq!(
                <VoiceRoomsFeature as Feature<(), ()>>::enabled(&feat),
                enabled
            );
            assert_eq!(
                <VoiceRoomsFeature as Feature<(), ()>>::needs_core_db(&feat),
                db
            );
            assert_eq!(
                <VoiceRoomsFeature as Feature<(), ()>>::name(&feat),
                "voice_rooms"
            );
        }
        // Mode on but nothing to serve: the feature is not enabled at all.
        let loaded = load(
            |key| (key == config::KEY_MODE).then(|| "live".to_string()),
            Some(GUILD),
        );
        let net = Net::new();
        let feat = feature(&net, &FakeClock::new(), loaded.settings);
        assert!(!<VoiceRoomsFeature as Feature<(), ()>>::enabled(&feat));
        assert!(!<VoiceRoomsFeature as Feature<(), ()>>::needs_core_db(
            &feat
        ));
    }

    #[test]
    fn the_cleanup_kind_registers_once_with_the_run_again_policy() {
        let net = Net::new();
        let feat = feature(&net, &FakeClock::new(), settings("live", &[]));
        let mut registry = Registry::new();
        <VoiceRoomsFeature as Feature<(), ()>>::register_jobs(&feat, &mut registry).unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(
            feat.register_job_kinds(&mut registry),
            Err(RunnerError::DuplicateKind)
        );
        assert_eq!(JOB_KINDS, [(KIND_CLEANUP, RestartPolicy::RunAgain)]);
    }

    // ------------------------------------------------------------ gating

    #[tokio::test]
    async fn off_shadow_and_a_misconfigured_mode_do_nothing_at_all() {
        let net = Net::new();
        let clock = FakeClock::new();
        let db = mem_db();
        let join = facts(U1, None, Some(HUB));
        let off = feature(&net, &clock, settings("off", &[]));
        assert_eq!(off.process(join, Some(&db)).await, Outcome::Off);
        assert_eq!(
            off.reconcile(Some(&db)).await,
            ReconcileReport {
                skipped: true,
                ..Default::default()
            }
        );
        // No category: fail closed.
        let loaded = load(
            |key| match key {
                "VC_MODE" => Some("live".to_string()),
                "VC_HUB_CHANNEL_IDS" => Some(HUB.to_string()),
                _ => None,
            },
            Some(GUILD),
        );
        let no_cat = feature(&net, &clock, loaded.settings);
        assert_eq!(
            no_cat.process(join, Some(&db)).await,
            Outcome::NotReady(NotReady::CategoryUnset)
        );
        // No home guild: fail closed, the hub join of "a" guild is not served.
        let loaded = load(
            |key| match key {
                "VC_MODE" => Some("live".to_string()),
                "VC_HUB_CHANNEL_IDS" => Some(HUB.to_string()),
                "VC_CATEGORY_ID" => Some(CAT.to_string()),
                _ => None,
            },
            None,
        );
        let no_guild = feature(&net, &clock, loaded.settings);
        assert_eq!(
            no_guild.process(join, Some(&db)).await,
            Outcome::NotReady(NotReady::HomeGuildUnset)
        );
        assert!(no_guild.reconcile(Some(&db)).await.skipped);
        // Shadow counts a hub join and does nothing else.
        let shadow = feature(&net, &clock, settings("shadow", &[]));
        assert_eq!(shadow.process(join, None).await, Outcome::Shadow);
        assert_eq!(
            shadow
                .process(facts(U1, None, Some(UNTRACKED_A)), None)
                .await,
            Outcome::Ignored(IgnoreCode::NotHubJoin)
        );
        assert_eq!(shadow.shadow_summary(), vec![("would_create", 1)]);
        assert!(shadow.reconcile(Some(&db)).await.skipped);
        assert!(net.calls().is_empty(), "no Discord call in these modes");
        assert!(jobs(&db).await.is_empty());
        assert_eq!(
            db.call(|c| c.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'voice_rooms'",
                [],
                |r| r.get::<_, i64>(0)
            ))
            .await
            .unwrap(),
            0,
            "not even the table is created"
        );
    }

    #[tokio::test]
    async fn live_mode_without_the_database_does_nothing_and_alerts() {
        let h = harness();
        assert_eq!(
            h.feat.process(facts(U1, None, Some(HUB)), None).await,
            Outcome::NoDatabase
        );
        assert_eq!(h.net.alerts(), vec![AlertCode::StoreUnavailable]);
        assert!(h.net.non_alert_calls().is_empty());
    }

    #[tokio::test]
    async fn an_event_of_another_guild_reads_writes_sends_and_alerts_nothing() {
        let h = harness();
        let room = h.room_for(U1).await;
        h.net.clear_calls();
        let jobs_before = jobs(&h.db).await.len();
        let rows_before = rows(&h.db).await;
        let hub_join = VoiceFacts {
            guild_id: Some(OTHER_GUILD),
            ..facts(U2, None, Some(HUB))
        };
        h.net.put(U2, HUB);
        assert_eq!(
            h.feat.process(hub_join, Some(&h.db)).await,
            Outcome::Ignored(IgnoreCode::WrongGuild)
        );
        // The owner of a real room leaves it, but the event claims another guild.
        h.net.unplug(U1);
        let leave = VoiceFacts {
            guild_id: Some(OTHER_GUILD),
            ..facts(U1, Some(room), None)
        };
        assert_eq!(
            h.feat.process(leave, Some(&h.db)).await,
            Outcome::Ignored(IgnoreCode::WrongGuild)
        );
        let no_guild = VoiceFacts {
            guild_id: None,
            ..facts(U1, Some(room), None)
        };
        assert_eq!(
            h.feat.process(no_guild, Some(&h.db)).await,
            Outcome::Ignored(IgnoreCode::WrongGuild)
        );
        assert!(h.net.calls().is_empty());
        assert_eq!(jobs(&h.db).await.len(), jobs_before);
        assert_eq!(rows(&h.db).await, rows_before);
        // The same leave in the home guild does schedule the cleanup.
        assert_eq!(
            h.feat
                .process(facts(U1, Some(room), None), Some(&h.db))
                .await,
            Outcome::CleanupScheduled
        );
    }

    #[tokio::test]
    async fn bots_and_the_bots_own_voice_events_are_ignored() {
        let h = harness();
        let cases = [
            VoiceFacts {
                is_bot: Some(true),
                ..facts(U2, None, Some(HUB))
            },
            VoiceFacts {
                user_id: BOT,
                ..facts(U2, None, Some(HUB))
            },
            VoiceFacts {
                is_bot: None,
                ..facts(U2, None, Some(HUB))
            },
            facts(0, None, Some(HUB)),
        ];
        let expect = [
            IgnoreCode::Bot,
            IgnoreCode::Bot,
            IgnoreCode::UnknownMember,
            IgnoreCode::InvalidUser,
        ];
        for (case, code) in cases.into_iter().zip(expect) {
            assert_eq!(
                h.feat.process(case, Some(&h.db)).await,
                Outcome::Ignored(code)
            );
        }
        assert!(h.net.calls().is_empty());
        assert!(jobs(&h.db).await.is_empty());
    }

    #[tokio::test]
    async fn a_non_hub_join_and_a_move_inside_one_channel_do_nothing() {
        let h = harness();
        assert_eq!(h.connect(U1, UNTRACKED_A).await, Outcome::Nothing);
        assert_eq!(
            h.feat
                .process(facts(U1, Some(UNTRACKED_A), Some(UNTRACKED_A)), Some(&h.db))
                .await,
            Outcome::Ignored(IgnoreCode::NoChannelChange)
        );
        assert!(h.net.calls().is_empty());
    }

    #[tokio::test]
    async fn a_channel_with_a_row_is_a_room_not_a_hub_even_if_the_settings_name_it() {
        let h = harness();
        add_row(&h.db, HUB, GUILD, U2, T0 - 1000).await;
        assert_eq!(h.connect(U1, HUB).await, Outcome::Nothing);
        assert!(h.net.calls().is_empty());
        assert_eq!(rows(&h.db).await.len(), 1);
    }

    // ------------------------------------------------------------ create

    #[tokio::test]
    async fn a_hub_join_creates_the_room_records_the_row_then_moves_the_member() {
        let h = harness();
        let everyone = RoomOverwrite {
            target: OverwriteTarget::Everyone,
            allow: 0,
            deny: policy::CONNECT,
        };
        let role = RoomOverwrite {
            target: OverwriteTarget::Role(U3),
            allow: policy::CONNECT,
            deny: 0,
        };
        *locked(&h.net.category) = Ok(vec![everyone, role]);
        let outcome = h.connect(U1, HUB).await;
        assert_eq!(
            outcome,
            Outcome::Created {
                channel: FIRST_ROOM
            }
        );
        assert_eq!(
            h.net.non_alert_calls(),
            vec![
                Call::Overwrites,
                Call::Create(CreateRoomRequest {
                    guild_id: GUILD,
                    category_id: CAT,
                    name: "Voice Room".to_string(),
                    overwrites: vec![
                        everyone,
                        role,
                        RoomOverwrite {
                            target: OverwriteTarget::Member(U1),
                            allow: OWNER_RIGHTS,
                            deny: FORBIDDEN_OWNER_RIGHTS,
                        },
                    ],
                    user_limit: 0,
                }),
                Call::Move {
                    user: U1,
                    channel: FIRST_ROOM
                },
            ]
        );
        assert!(h.net.alerts().is_empty());
        assert_eq!(h.net.at(U1), Some(FIRST_ROOM));
        assert_eq!(
            rows(&h.db).await,
            vec![RoomRow {
                channel: FIRST_ROOM,
                guild: GUILD,
                owner: U1,
                created_at: T0,
                locked: false,
                user_limit: 0
            }]
        );
        assert_eq!(
            *locked(&h.net.row_at_move),
            vec![true],
            "the row came first"
        );
        // The owner never gets the rights VR1 forbids.
        let owner = h.net.creates()[0].overwrites.last().copied().unwrap();
        assert_eq!(owner.allow & FORBIDDEN_OWNER_RIGHTS, 0);
        assert_eq!(owner.deny & FORBIDDEN_OWNER_RIGHTS, FORBIDDEN_OWNER_RIGHTS);
        // Nothing is scheduled while the room is occupied.
        assert!(jobs(&h.db).await.is_empty());
    }

    #[tokio::test]
    async fn the_second_hub_works_like_the_first() {
        let h = harness();
        assert_eq!(
            h.connect(U1, HUB2).await,
            Outcome::Created {
                channel: FIRST_ROOM
            }
        );
    }

    #[tokio::test]
    async fn when_the_create_fails_the_member_stays_in_the_hub_and_no_row_is_written() {
        let h = harness();
        h.net.script("create", &[PortError::Unavailable]);
        assert_eq!(h.connect(U1, HUB).await, Outcome::CreateFailed);
        assert_eq!(h.net.at(U1), Some(HUB), "the member stays in the hub");
        assert!(rows(&h.db).await.is_empty());
        assert!(!h
            .net
            .non_alert_calls()
            .iter()
            .any(|c| matches!(c, Call::Move { .. } | Call::Delete(_))));
        assert_eq!(h.net.alerts(), vec![AlertCode::CreateFailed]);
        assert!(jobs(&h.db).await.is_empty());
        assert!(locked(&h.net.world).created.is_empty());
    }

    #[tokio::test]
    async fn a_category_lookup_failure_creates_nothing() {
        let h = harness();
        *locked(&h.net.category) = Err(PortError::Forbidden);
        assert_eq!(h.connect(U1, HUB).await, Outcome::CategoryLookupFailed);
        assert_eq!(h.net.at(U1), Some(HUB));
        assert!(h.net.creates().is_empty());
        assert!(rows(&h.db).await.is_empty());
        assert_eq!(h.net.alerts(), vec![AlertCode::CategoryLookupFailed]);
    }

    #[tokio::test]
    async fn when_the_move_fails_the_new_empty_channel_is_deleted_and_no_row_remains() {
        let h = harness();
        h.net.script("move", &[PortError::Unavailable]);
        assert_eq!(
            h.connect(U1, HUB).await,
            Outcome::MoveFailed {
                channel_deleted: true
            }
        );
        assert_eq!(h.net.deletes(), vec![FIRST_ROOM]);
        assert!(!h.net.exists(FIRST_ROOM));
        assert_eq!(h.net.at(U1), Some(HUB), "the member stays in the hub");
        assert!(rows(&h.db).await.is_empty());
        assert!(jobs(&h.db).await.is_empty());
        assert_eq!(h.net.alerts(), vec![AlertCode::MoveFailed]);
    }

    #[tokio::test]
    async fn a_delete_that_says_not_found_after_a_failed_move_counts_as_deleted() {
        let h = harness();
        h.net.script("move", &[PortError::Unavailable]);
        h.net.script("delete", &[PortError::NotFound]);
        assert_eq!(
            h.connect(U1, HUB).await,
            Outcome::MoveFailed {
                channel_deleted: true
            }
        );
        assert!(rows(&h.db).await.is_empty());
    }

    #[tokio::test]
    async fn when_the_move_and_the_delete_both_fail_the_row_stays_and_a_cleanup_is_owed() {
        let h = harness();
        h.net.script("move", &[PortError::Unavailable]);
        h.net.script("delete", &[PortError::Forbidden]);
        assert_eq!(
            h.connect(U1, HUB).await,
            Outcome::MoveFailed {
                channel_deleted: false
            }
        );
        assert_eq!(
            h.net.alerts(),
            vec![AlertCode::MoveFailed, AlertCode::DeleteFailed]
        );
        assert_eq!(rows(&h.db).await.len(), 1, "the room is still tracked");
        let owed = pending(&h.db).await;
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].kind, KIND_CLEANUP);
        assert_eq!(owed[0].due, T0 + 10);
        // The grace passes and the cleanup finishes the job.
        h.clock.advance(10);
        h.feat.run_cleanup(claimed(&owed[0])).await.unwrap();
        assert!(!h.net.exists(FIRST_ROOM));
        assert!(rows(&h.db).await.is_empty());
    }

    #[tokio::test]
    async fn when_the_row_cannot_be_written_the_new_channel_is_deleted_and_nobody_is_moved() {
        let h = harness();
        // Create the table, then make every insert fail.
        h.db.call(|c| {
            repository::migrate(c).unwrap();
            c.execute_batch(
                "CREATE TRIGGER refuse_rooms BEFORE INSERT ON voice_rooms \
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
        })
        .await
        .unwrap();
        assert_eq!(h.connect(U1, HUB).await, Outcome::StoreFailed);
        assert_eq!(h.net.deletes(), vec![FIRST_ROOM]);
        assert!(!h.net.exists(FIRST_ROOM));
        assert!(!h.net.calls().iter().any(|c| matches!(c, Call::Move { .. })));
        assert_eq!(h.net.alerts(), vec![AlertCode::StoreFailed]);
        assert_eq!(h.net.at(U1), Some(HUB));
    }

    // ------------------------------------------------------------ caps and cooldown

    #[tokio::test]
    async fn the_cooldown_denies_silently_and_ends_exactly_at_the_configured_second() {
        let h = harness();
        let room = h.room_for(U1).await;
        // The room is gone again (cleanup done); only the cooldown stands in the way.
        h.net.unplug(U1);
        h.db.call(move |c| repository::delete(c, room))
            .await
            .unwrap();
        h.net.clear_calls();
        for (after, denied) in [(1, true), (10, true), (29, true), (30, false)] {
            h.clock.advance(after - (h.clock.secs() - T0).max(0));
            let outcome = h.connect(U1, HUB).await;
            if denied {
                assert_eq!(
                    outcome,
                    Outcome::Denied(DenyReason::CreateCooldown),
                    "{after}s"
                );
                assert!(h.net.calls().is_empty(), "a denial is silent ({after}s)");
            } else {
                assert!(matches!(outcome, Outcome::Created { .. }), "{after}s");
            }
        }
    }

    #[tokio::test]
    async fn a_failed_attempt_also_starts_the_cooldown_so_failures_cannot_be_hammered() {
        let h = harness();
        h.net.script("create", &[PortError::Unavailable]);
        assert_eq!(h.connect(U1, HUB).await, Outcome::CreateFailed);
        h.net.clear_calls();
        h.clock.advance(5);
        assert_eq!(
            h.connect(U1, HUB).await,
            Outcome::Denied(DenyReason::CreateCooldown)
        );
        assert!(
            h.net.calls().is_empty(),
            "no second create, no second alert"
        );
        h.clock.advance(25);
        assert!(matches!(h.connect(U1, HUB).await, Outcome::Created { .. }));
        // Another member was never affected by U1's cooldown.
        assert!(matches!(h.connect(U2, HUB).await, Outcome::Created { .. }));
    }

    #[tokio::test]
    async fn the_per_member_and_global_caps_deny_silently() {
        let h = harness_with(&[
            (config::KEY_MAX_ROOMS, "2"),
            (config::KEY_CREATE_COOLDOWN_SECS, "0"),
        ]);
        h.room_for(U1).await;
        h.net.clear_calls();
        // U1 already owns a room.
        h.net.put(U1, HUB);
        assert_eq!(
            h.feat
                .process(facts(U1, Some(FIRST_ROOM), Some(HUB)), Some(&h.db))
                .await,
            Outcome::Denied(DenyReason::PerUserCapReached)
        );
        h.net.clear_calls();
        // The room of U1 still holds nobody now; give it a second room of another member.
        h.room_for(U2).await;
        h.net.clear_calls();
        let outcome = h.connect(U3, HUB).await;
        assert_eq!(outcome, Outcome::Denied(DenyReason::GlobalCapReached));
        assert!(
            h.net.calls().is_empty(),
            "no overwrite lookup, no create, no alert"
        );
        assert_eq!(rows(&h.db).await.len(), 2);
        assert_eq!(h.net.at(U3), Some(HUB));
    }

    #[tokio::test]
    async fn two_joins_at_once_cannot_both_slip_under_a_cap_of_one() {
        let h = harness_with(&[(config::KEY_MAX_ROOMS, "1")]);
        h.net.put(U1, HUB);
        h.net.put(U2, HUB);
        let (a, b) = tokio::join!(
            h.feat.process(facts(U1, None, Some(HUB)), Some(&h.db)),
            h.feat.process(facts(U2, None, Some(HUB)), Some(&h.db)),
        );
        let created = [a, b]
            .iter()
            .filter(|o| matches!(o, Outcome::Created { .. }))
            .count();
        let denied = [a, b]
            .iter()
            .filter(|o| matches!(o, Outcome::Denied(DenyReason::GlobalCapReached)))
            .count();
        assert_eq!((created, denied), (1, 1), "{a:?} {b:?}");
        assert_eq!(rows(&h.db).await.len(), 1);
    }

    // ------------------------------------------------------------ cleanup

    #[tokio::test]
    async fn an_emptied_room_schedules_one_cleanup_after_the_grace_and_wakes_the_runner() {
        let h = harness();
        let room = h.room_for(U1).await;
        h.clock.advance(100);
        assert_eq!(h.disconnect(U1, room).await, Outcome::CleanupScheduled);
        let owed = pending(&h.db).await;
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].kind, "vc_cleanup");
        assert_eq!(owed[0].due, T0 + 110, "empty at +100, grace 10");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&owed[0].payload).unwrap(),
            serde_json::json!({"guild": GUILD, "channel": room, "since": T0 + 100, "attempt": 0})
        );
        assert_eq!(h.wakes.load(SeqCst), 1);
        // The same leave delivered twice schedules nothing more.
        assert_eq!(h.disconnect(U1, room).await, Outcome::CleanupScheduled);
        assert_eq!(jobs(&h.db).await.len(), 1);
        assert!(
            h.net.exists(room),
            "nothing is deleted before the job fires"
        );
    }

    #[tokio::test]
    async fn a_room_that_still_holds_members_or_is_not_tracked_schedules_nothing() {
        let h = harness_with(&[(config::KEY_CREATE_COOLDOWN_SECS, "0")]);
        let room = h.room_for(U1).await;
        h.net.put(U2, room);
        assert_eq!(h.disconnect(U1, room).await, Outcome::Nothing);
        assert!(jobs(&h.db).await.is_empty());
        // An empty channel the bot did not create: looked at by name only, never touched.
        h.net.add_channel(UNTRACKED_A, CAT, "Voice Room");
        h.net.put(U3, UNTRACKED_A);
        h.net.clear_calls();
        assert_eq!(h.disconnect(U3, UNTRACKED_A).await, Outcome::Nothing);
        assert!(h.net.calls().is_empty());
        assert!(jobs(&h.db).await.is_empty());
        assert!(h.net.exists(UNTRACKED_A));
    }

    #[tokio::test]
    async fn moving_out_of_a_room_into_the_hub_cleans_up_the_old_room_and_builds_a_new_one() {
        let h = harness_with(&[(config::KEY_CREATE_COOLDOWN_SECS, "0")]);
        let room = h.room_for(U1).await;
        // U1 gives the room up for a fresh one: the policy's per-member cap sees the old room
        // still tracked, so the join is denied, but the cleanup of the old room is scheduled.
        assert_eq!(
            h.switch(U1, room, HUB).await,
            Outcome::Denied(DenyReason::PerUserCapReached)
        );
        assert_eq!(
            pending(&h.db).await.len(),
            1,
            "the old room is on its way out"
        );
    }

    #[tokio::test]
    async fn the_job_deletes_a_room_that_stayed_empty_and_forgets_its_row() {
        let h = harness();
        let room = h.room_for(U1).await;
        h.disconnect(U1, room).await;
        let job = pending(&h.db).await.remove(0);
        h.clock.advance(10);
        h.net.clear_calls();
        assert_eq!(h.feat.run_cleanup(claimed(&job)).await, Ok(()));
        assert_eq!(h.net.deletes(), vec![room]);
        assert!(!h.net.exists(room));
        assert!(rows(&h.db).await.is_empty());
        assert!(h.net.alerts().is_empty());
        // A second wake-up for the same room finds nothing and does nothing.
        h.net.clear_calls();
        assert_eq!(h.feat.run_cleanup(claimed(&job)).await, Ok(()));
        assert!(h.net.calls().is_empty());
    }

    #[tokio::test]
    async fn a_member_rejoining_during_the_grace_cancels_the_cleanup() {
        let h = harness();
        let room = h.room_for(U1).await;
        h.disconnect(U1, room).await;
        let job = pending(&h.db).await.remove(0);
        h.clock.advance(4);
        assert_eq!(h.connect(U2, room).await, Outcome::Nothing);
        h.clock.advance(6);
        h.net.clear_calls();
        assert_eq!(h.feat.run_cleanup(claimed(&job)).await, Ok(()));
        assert!(h.net.deletes().is_empty(), "the room is in use");
        assert!(h.net.exists(room));
        assert_eq!(rows(&h.db).await.len(), 1, "the row stays");
        // When they leave again a new cleanup is owed.
        assert_eq!(h.disconnect(U2, room).await, Outcome::CleanupScheduled);
    }

    #[tokio::test]
    async fn a_wake_up_that_fires_early_never_deletes_a_room_that_emptied_again_later() {
        let h = harness();
        let room = h.room_for(U1).await;
        h.disconnect(U1, room).await; // empty at T0, due T0+10
        let first = pending(&h.db).await.remove(0);
        h.clock.advance(3);
        h.connect(U2, room).await; // rejoin
        h.clock.advance(5);
        assert_eq!(h.disconnect(U2, room).await, Outcome::CleanupScheduled); // empty at T0+8
        assert_eq!(pending(&h.db).await.len(), 2);
        h.clock.advance(2); // T0+10: the first wake-up fires
        h.net.clear_calls();
        assert_eq!(h.feat.run_cleanup(claimed(&first)).await, Ok(()));
        assert!(
            h.net.deletes().is_empty(),
            "only 2 s of the second grace have passed"
        );
        assert!(h.net.exists(room));
        // The second wake-up (due T0+18) is the one that deletes.
        h.clock.advance(8);
        let second = pending(&h.db)
            .await
            .into_iter()
            .find(|j| j.due == T0 + 18)
            .unwrap();
        assert_eq!(h.feat.run_cleanup(claimed(&second)).await, Ok(()));
        assert_eq!(h.net.deletes(), vec![room]);
    }

    #[tokio::test]
    async fn an_early_wake_up_whose_successor_is_missing_schedules_it_instead_of_going_quiet() {
        let h = harness();
        let room = h.room_for(U1).await;
        h.disconnect(U1, room).await;
        // The room emptied at T0+8 in the live state, but only a wake-up for T0 exists.
        let early = pending(&h.db).await.remove(0);
        h.clock.advance(8);
        h.connect(U2, room).await;
        h.disconnect(U2, room).await;
        h.db.call(|c| c.execute("DELETE FROM jobs WHERE due_at > 1800000000 + 10", []))
            .await
            .unwrap();
        h.clock.advance(2);
        assert_eq!(h.feat.run_cleanup(claimed(&early)).await, Ok(()));
        let owed = pending(&h.db).await;
        assert!(
            owed.iter().any(|j| j.due == T0 + 18),
            "the owed delete is re-armed for the end of the second grace: {owed:?}"
        );
        assert!(h.net.exists(room));
    }

    #[tokio::test]
    async fn a_burnt_dedupe_key_does_not_swallow_a_later_cleanup() {
        // Grace 0: the wake-up is due at once, so two empties in one second share a key.
        let h = harness_with(&[(config::KEY_EMPTY_GRACE_SECS, "0")]);
        let room = h.room_for(U1).await;
        h.disconnect(U1, room).await;
        let first = pending(&h.db).await.remove(0);
        h.connect(U2, room).await; // rejoined before the wake-up fires
        let id = first.id;
        h.db.call(move |c| {
            assert!(queue::try_claim(c, id, T0).unwrap());
            queue::complete(c, id, T0)
        })
        .await
        .unwrap();
        assert_eq!(h.feat.run_cleanup(claimed(&first)).await, Ok(()));
        assert!(h.net.exists(room));
        // The same second, empty again: the finished job must not hide this one.
        assert_eq!(h.disconnect(U2, room).await, Outcome::CleanupScheduled);
        let owed = pending(&h.db).await;
        assert_eq!(owed.len(), 1, "{:?}", jobs(&h.db).await);
        assert_ne!(owed[0].key, first.key);
        assert_eq!(h.feat.run_cleanup(claimed(&owed[0])).await, Ok(()));
        assert!(!h.net.exists(room));
    }

    #[tokio::test]
    async fn a_wake_up_for_an_untracked_channel_or_another_guild_touches_nothing() {
        let h = harness();
        let room = h.room_for(U1).await;
        h.net.add_channel(UNTRACKED_A, CAT, "Voice Room");
        h.net.clear_calls();
        // No row: the channel exists and is empty, and is left alone.
        assert_eq!(h.feat.run_cleanup(payload(UNTRACKED_A, T0)).await, Ok(()));
        // A payload for another guild.
        let other = ClaimedJob {
            payload_json: cleanup_payload(&CleanupPayload {
                guild: OTHER_GUILD,
                channel: room,
                since: T0,
                attempt: 0,
            }),
            ..payload(room, T0)
        };
        h.net.unplug(U1);
        assert_eq!(h.feat.run_cleanup(other).await, Ok(()));
        // A row of another guild under a payload of the home guild.
        add_row(&h.db, UNTRACKED_B, OTHER_GUILD, U2, T0).await;
        h.net.add_channel(UNTRACKED_B, CAT, "Other");
        assert_eq!(h.feat.run_cleanup(payload(UNTRACKED_B, T0)).await, Ok(()));
        assert!(h.net.calls().is_empty(), "{:?}", h.net.calls());
        assert!(h.net.exists(UNTRACKED_A) && h.net.exists(UNTRACKED_B) && h.net.exists(room));
        assert_eq!(all_rows(&h.db).await, 2);
    }

    #[tokio::test]
    async fn a_wake_up_for_a_vanished_channel_drops_the_row() {
        let h = harness();
        let room = h.room_for(U1).await;
        h.disconnect(U1, room).await;
        locked(&h.net.world).channels.remove(&room);
        h.net.clear_calls();
        h.clock.advance(10);
        assert_eq!(h.feat.run_cleanup(payload(room, T0)).await, Ok(()));
        assert!(rows(&h.db).await.is_empty());
        assert!(h.net.calls().is_empty());
    }

    #[tokio::test]
    async fn a_bad_payload_fails_with_a_stable_code_and_one_alert() {
        let h = harness();
        for raw in [
            "",
            "[]",
            "{}",
            r#"{"guild":1,"channel":0,"since":0,"attempt":0}"#,
        ] {
            let job = ClaimedJob {
                payload_json: raw.to_string(),
                ..payload(1, 0)
            };
            assert_eq!(
                h.feat.run_cleanup(job).await,
                Err(HandlerError::new("payload_invalid")),
                "{raw:?}"
            );
        }
        assert_eq!(h.net.alerts(), vec![AlertCode::PayloadInvalid; 4]);
        assert!(h.net.non_alert_calls().is_empty());
    }

    #[tokio::test]
    async fn before_the_database_is_attached_a_wake_up_fails_retryably_and_does_nothing() {
        let net = Net::new();
        let clock = FakeClock::new();
        let feat = feature(&net, &clock, settings("live", &[]));
        assert_eq!(
            feat.run_cleanup(payload(FIRST_ROOM, T0)).await,
            Err(HandlerError::new("db_not_attached"))
        );
        assert!(net.calls().is_empty());
        // A wake-up of a feature that is off is a no-op, not an error.
        let off = feature(&net, &clock, settings("off", &[]));
        assert_eq!(off.run_cleanup(payload(FIRST_ROOM, T0)).await, Ok(()));
    }

    #[tokio::test]
    async fn a_failing_delete_is_retried_with_one_alert_and_gives_up_after_the_limit_keeping_the_row(
    ) {
        let h = harness();
        let room = h.room_for(U1).await;
        h.disconnect(U1, room).await;
        h.net.fail_always("delete", PortError::Unavailable);
        let mut chain = 0u32;
        loop {
            h.clock.advance(1000); // past any retry delay
            let owed = pending(&h.db).await;
            if owed.is_empty() {
                break;
            }
            assert_eq!(owed.len(), 1, "one live wake-up at a time: {owed:?}");
            let id = owed[0].id;
            h.db.call(move |c| {
                assert!(queue::try_claim(c, id, 4_000_000_000).unwrap());
                queue::complete(c, id, 4_000_000_000)
            })
            .await
            .unwrap();
            assert_eq!(h.feat.run_cleanup(claimed(&owed[0])).await, Ok(()));
            chain += 1;
            assert!(chain <= MAX_CLEANUP_ATTEMPTS, "the chain must end");
        }
        assert_eq!(chain, MAX_CLEANUP_ATTEMPTS);
        assert_eq!(
            h.net.alerts(),
            vec![AlertCode::DeleteFailed, AlertCode::CleanupGaveUp],
            "one alert for the outage, one when giving up"
        );
        assert_eq!(rows(&h.db).await.len(), 1, "the row stays");
        assert!(h.net.exists(room));
        // The next restart reconcile, with Discord healthy again, finishes the job.
        h.net.heal("delete");
        let report = h.feat.reconcile(Some(&h.db)).await;
        assert_eq!(report.deleted_empty, 1);
        assert!(!h.net.exists(room));
        assert!(rows(&h.db).await.is_empty());
    }

    #[tokio::test]
    async fn a_wake_up_that_cannot_tell_the_occupancy_retries_instead_of_deleting() {
        let h = harness();
        let room = h.room_for(U1).await;
        h.disconnect(U1, room).await;
        locked(&h.net.unknown).insert(room);
        h.clock.advance(10);
        h.net.clear_calls();
        assert_eq!(h.feat.run_cleanup(payload(room, T0)).await, Ok(()));
        assert!(h.net.deletes().is_empty());
        let owed = pending(&h.db).await;
        assert!(
            owed.iter().any(|j| j.payload.contains("\"attempt\":1")),
            "{owed:?}"
        );
        assert!(h.net.exists(room));
    }

    // ------------------------------------------------------------ reconcile

    #[tokio::test]
    async fn reconcile_drops_a_missing_channel_deletes_an_empty_tracked_one_and_never_touches_the_rest(
    ) {
        let h = harness();
        const MISSING: u64 = 6_500_000_000_000_001;
        const EMPTY: u64 = 6_500_000_000_000_002;
        const BUSY: u64 = 6_500_000_000_000_003;
        const FOREIGN: u64 = 6_500_000_000_000_004;
        add_row(&h.db, MISSING, GUILD, U1, T0 - 50).await;
        add_row(&h.db, EMPTY, GUILD, U2, T0 - 40).await;
        add_row(&h.db, BUSY, GUILD, U3, T0 - 30).await;
        add_row(&h.db, FOREIGN, OTHER_GUILD, U3, T0 - 20).await;
        h.net.add_channel(EMPTY, CAT, "Voice Room");
        h.net.add_channel(BUSY, CAT, "Voice Room");
        h.net.add_channel(FOREIGN, CAT, "Voice Room");
        h.net.put(U3, BUSY);
        // Channels that look exactly like rooms but have no row.
        h.net.add_channel(UNTRACKED_A, CAT, "Voice Room");
        h.net.add_channel(UNTRACKED_B, OTHER_CAT, "Voice Room");
        let report = h.feat.reconcile(Some(&h.db)).await;
        assert_eq!(
            report,
            ReconcileReport {
                skipped: false,
                dropped_missing: 1,
                deleted_empty: 1,
                kept: 1,
                rescheduled: 0,
                failed: 0
            }
        );
        assert_eq!(h.net.deletes(), vec![EMPTY], "only the tracked empty room");
        for survivor in [BUSY, FOREIGN, UNTRACKED_A, UNTRACKED_B, HUB, HUB2] {
            assert!(h.net.exists(survivor), "{survivor}");
        }
        assert!(!h.net.exists(EMPTY));
        let left: Vec<u64> = rows(&h.db).await.iter().map(|r| r.channel).collect();
        assert_eq!(left, vec![BUSY]);
        assert_eq!(
            all_rows(&h.db).await,
            2,
            "the other guild's row is untouched"
        );
        // A second pass changes nothing.
        h.net.clear_calls();
        let again = h.feat.reconcile(Some(&h.db)).await;
        assert_eq!(
            again,
            ReconcileReport {
                kept: 1,
                ..Default::default()
            }
        );
        assert!(h.net.calls().is_empty() || h.net.deletes().is_empty());
        assert!(h.net.alerts().is_empty());
    }

    #[tokio::test]
    async fn a_reconcile_delete_that_fails_keeps_the_row_alerts_and_owes_a_cleanup() {
        let h = harness();
        const EMPTY: u64 = 6_500_000_000_000_012;
        const UNSURE: u64 = 6_500_000_000_000_013;
        add_row(&h.db, EMPTY, GUILD, U2, T0 - 40).await;
        add_row(&h.db, UNSURE, GUILD, U3, T0 - 30).await;
        h.net.add_channel(EMPTY, CAT, "Voice Room");
        h.net.add_channel(UNSURE, CAT, "Voice Room");
        locked(&h.net.unknown).insert(UNSURE);
        h.net.script("delete", &[PortError::Forbidden]);
        let report = h.feat.reconcile(Some(&h.db)).await;
        assert_eq!(report.rescheduled, 2);
        assert_eq!(report.deleted_empty, 0);
        assert_eq!(h.net.alerts(), vec![AlertCode::DeleteFailed]);
        assert_eq!(rows(&h.db).await.len(), 2, "both rows stay");
        assert_eq!(pending(&h.db).await.len(), 2, "both owe a wake-up");
        // Repeating the reconcile does not pile up wake-ups for the same rooms.
        h.net.heal("delete");
        locked(&h.net.unknown).clear();
        let again = h.feat.reconcile(Some(&h.db)).await;
        assert_eq!(again.deleted_empty, 2);
        assert!(rows(&h.db).await.is_empty());
    }

    // ------------------------------------------------------------ liveness

    struct Up {
        shutdown: ShutdownHandle,
        task: tokio::task::JoinHandle<()>,
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

    async fn boot(db: &Db, clock: &Arc<FakeClock>, feat: &VoiceRoomsFeature) -> Up {
        let mut registry = Registry::new();
        feat.register_job_kinds(&mut registry).unwrap();
        let runner = Runner::new(db.clone(), clock.clone(), registry, runner_cfg()).unwrap();
        let wake = runner.wake_handle();
        <VoiceRoomsFeature as Feature<(), ()>>::jobs_started(feat, Arc::new(move || wake.wake()));
        let shutdown = runner.shutdown_handle();
        let started = runner.start().await.unwrap();
        let task = tokio::spawn(async move {
            let _ = started.run().await;
        });
        Up { shutdown, task }
    }

    /// Waits until the runner has nothing due and nothing running.
    async fn settle(db: &Db, clock: &Arc<FakeClock>) {
        let now = clock.secs();
        for _ in 0..2000 {
            let busy = db
                .call(move |c| {
                    c.query_row(
                        "SELECT COUNT(*) FROM jobs WHERE state = 'running' \
                         OR (state = 'pending' AND due_at <= ?1)",
                        [now],
                        |r| r.get::<_, i64>(0),
                    )
                })
                .await
                .unwrap();
            if busy == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the runner did not settle");
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % bound
        }
    }

    /// A seeded run against the real queue, the real runner and a real SQLite file, with Discord
    /// failures, restarts and crashes between claim and acknowledgement.
    async fn liveness(seed: u64) -> (usize, usize, usize) {
        let dir = scratch(&format!("liveness-{seed}"));
        let path = dir.join("core.sqlite");
        let path = path.to_str().unwrap();
        let clock = FakeClock::new();
        let net = Net::new();
        net.add_channel(UNTRACKED_A, CAT, "Voice Room");
        net.add_channel(UNTRACKED_B, OTHER_CAT, "Voice Room");
        let users = [U1, U2, U3, U1 + 100, U2 + 100];
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let extra = [
            (config::KEY_MAX_ROOMS, "3"),
            (config::KEY_CREATE_COOLDOWN_SECS, "3"),
            (config::KEY_EMPTY_GRACE_SECS, "5"),
        ];
        *locked(&net.rng) = Some((seed ^ 0xABCD, [8, 15, 15, 25]));
        for _ in 0..4 {
            let db = file_db(path);
            let feat = feature(&net, &clock, settings("live", &extra));
            let up = boot(&db, &clock, &feat).await;
            feat.reconcile(Some(&db)).await;
            settle(&db, &clock).await;
            for _ in 0..40 {
                let user = users[rng.next(users.len() as u64) as usize];
                let here = net.at(user);
                let rooms: Vec<u64> = {
                    let world = locked(&net.world);
                    world
                        .created
                        .iter()
                        .copied()
                        .filter(|c| world.channels.contains_key(c))
                        .collect()
                };
                match rng.next(10) {
                    0..=2 => {
                        // Join a hub (from nowhere or from wherever the member is).
                        net.put(user, HUB);
                        feat.process(facts(user, here, Some(HUB)), Some(&db)).await;
                    }
                    3 | 4 => {
                        if let Some(old) = here {
                            net.unplug(user);
                            feat.process(facts(user, Some(old), None), Some(&db)).await;
                        }
                    }
                    5 | 6 => {
                        if !rooms.is_empty() {
                            let room = rooms[rng.next(rooms.len() as u64) as usize];
                            net.put(user, room);
                            feat.process(facts(user, here, Some(room)), Some(&db)).await;
                        }
                    }
                    _ => {
                        clock.advance(1 + rng.next(12) as i64);
                    }
                }
                settle(&db, &clock).await;
            }
            up.stop().await;
            if rng.next(2) == 0 {
                // A runner claimed what was due and died before acknowledging.
                clock.advance(7);
                let now = clock.secs();
                let _ = db.call(move |c| queue::claim_due(c, now, 8)).await.unwrap();
            }
            drop(feat);
            drop(db);
            clock.advance(rng.next(30) as i64);
        }
        // Quiet phase: Discord is healthy, everybody leaves, time passes.
        *locked(&net.rng) = None;
        let db = file_db(path);
        let feat = feature(&net, &clock, settings("live", &extra));
        let up = boot(&db, &clock, &feat).await;
        feat.reconcile(Some(&db)).await;
        for user in users {
            if let Some(old) = net.at(user) {
                net.unplug(user);
                feat.process(facts(user, Some(old), None), Some(&db)).await;
            }
        }
        for _ in 0..70 {
            clock.advance(30);
            settle(&db, &clock).await;
        }
        up.stop().await;
        {
            let world = locked(&net.world);
            let leftover: Vec<u64> = world
                .created
                .iter()
                .copied()
                .filter(|c| world.channels.contains_key(c))
                .collect();
            assert!(
                leftover.is_empty(),
                "seed {seed}: empty rooms linger: {leftover:?}"
            );
            for untracked in [UNTRACKED_A, UNTRACKED_B, HUB, HUB2] {
                assert!(
                    world.channels.contains_key(&untracked),
                    "seed {seed}: {untracked}"
                );
            }
            for deleted in &world.deleted {
                assert!(
                    world.created.contains(deleted),
                    "seed {seed}: only bot rooms are deleted"
                );
            }
        }
        assert_eq!(
            all_rows(&db).await,
            0,
            "seed {seed}: no row outlives its room"
        );
        let states: Vec<JobRow> = jobs(&db).await;
        assert!(
            states
                .iter()
                .all(|j| j.state != "pending" && j.state != "running"),
            "seed {seed}: unfinished jobs {states:?}"
        );
        let gave_up = net.alerts().contains(&AlertCode::CleanupGaveUp);
        assert!(!gave_up, "seed {seed}: a chain gave up");
        let world = locked(&net.world);
        (world.created.len(), world.deleted.len(), net.alerts().len())
    }

    #[tokio::test]
    async fn liveness_seeded_joins_leaves_faults_restarts_and_crashes_always_end_with_no_stray_room(
    ) {
        let (mut created, mut deleted, mut alerts) = (0, 0, 0);
        for seed in 0..6 {
            let (c, d, a) = liveness(seed).await;
            created += c;
            deleted += d;
            alerts += a;
        }
        // The runs must have done real work and met real faults, or they prove nothing.
        assert!(created >= 30, "rooms created: {created}");
        assert!(deleted >= 30, "rooms deleted: {deleted}");
        assert!(alerts >= 10, "faults met: {alerts}");
    }

    #[tokio::test]
    async fn an_owed_cleanup_survives_a_restart_and_runs_when_the_new_runner_starts() {
        let dir = scratch("restart");
        let path = dir.join("core.sqlite");
        let path = path.to_str().unwrap();
        let clock = FakeClock::new();
        let net = Net::new();
        let room;
        {
            let db = file_db(path);
            let feat = feature(&net, &clock, settings("live", &[]));
            net.put(U1, HUB);
            room = match feat.process(facts(U1, None, Some(HUB)), Some(&db)).await {
                Outcome::Created { channel } => channel,
                other => panic!("{other:?}"),
            };
            net.unplug(U1);
            assert_eq!(
                feat.process(facts(U1, Some(room), None), Some(&db)).await,
                Outcome::CleanupScheduled
            );
        }
        // The bot was down for an hour; nobody was in the room.
        clock.advance(3600);
        let db = file_db(path);
        let feat = feature(&net, &clock, settings("live", &[]));
        let up = boot(&db, &clock, &feat).await;
        // The gateway comes up a moment later: the reconcile attaches the database.
        let report = feat.reconcile(Some(&db)).await;
        settle(&db, &clock).await;
        up.stop().await;
        assert!(!net.exists(room));
        assert_eq!(all_rows(&db).await, 0);
        assert!(report.deleted_empty <= 1);
    }
}

//! Durable, restart-safe job queue on top of `storage` (feature `"jobs"`).
//!
//! Synchronous and Discord-free. Every function takes the connection and the
//! current time (`now`, unix seconds) from the caller: no clock is read and no
//! randomness is used. Errors are stable codes ([`QueueError`]); neither
//! errors, `Debug` output of results, nor the restart [`Report`] carry payload
//! text.
//!
//! Lifecycle: `pending` -> `running` (claim, compare-and-swap) -> `done`, or
//! back to `pending` with backoff ([`fail`]), or `failed` once
//! [`RetryPolicy::max_attempts`] claims have been used. A restart turns jobs
//! found `running` into `pending` (kind is `RunAgain`) or `abandoned`
//! (`AtMostOnce` or unregistered kind) via [`reconcile_after_restart`].
//!
//! Dedupe: `dedupe_key` is UNIQUE for the life of the row; rows are never
//! deleted by this module. Scheduling a key that already exists, in ANY state
//! (including done, failed or abandoned), creates nothing and returns
//! [`ScheduleOutcome::Duplicate`] with the existing id and state; the original
//! kind, payload and due time are kept. A caller that wants a later
//! recurrence must put the cycle into the key.

use crate::storage::{self, Migration, StorageError};
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OptionalExtension};

pub const FEATURE: &str = "jobs";
pub const MAX_KIND_LEN: usize = 64;
pub const MAX_DEDUPE_KEY_LEN: usize = 128;
pub const MAX_PAYLOAD_BYTES: usize = 4096;
pub const MAX_CODE_LEN: usize = 48;

/// Stable `last_error_code` values written by this module itself.
pub const CODE_PAYLOAD_MALFORMED: &str = "payload_malformed";
pub const CODE_KIND_MALFORMED: &str = "kind_malformed";
/// Shown in views and reports instead of a stored `kind` that is not a plain
/// `[a-z0-9_]{1,64}` token (only possible for rows written around this API).
pub const KIND_PLACEHOLDER: &str = "invalid_kind";
/// Same, for a stored `dedupe_key` that is not printable ASCII.
pub const KEY_PLACEHOLDER: &str = "invalid_key";
pub const CODE_INVALID_CODE: &str = "invalid_error_code";
pub const CODE_RESTART_RERUN: &str = "restart_rerun";
pub const CODE_RESTART_AT_MOST_ONCE: &str = "restart_at_most_once";
pub const CODE_RESTART_UNREGISTERED: &str = "restart_unregistered_kind";
pub const CODE_RESTART_MAX_ATTEMPTS: &str = "restart_max_attempts";

/// Migration list for feature `"jobs"`. Apply with [`init`].
pub const MIGRATIONS: [Migration; 1] = [Migration {
    version: 1,
    sql: "CREATE TABLE jobs (\
            id INTEGER PRIMARY KEY, \
            kind TEXT NOT NULL CHECK (typeof(kind) = 'text'), \
            dedupe_key TEXT NOT NULL UNIQUE CHECK (typeof(dedupe_key) = 'text'), \
            payload_json TEXT NOT NULL CHECK (typeof(payload_json) = 'text'), \
            due_at INTEGER NOT NULL CHECK (typeof(due_at) = 'integer'), \
            state TEXT NOT NULL CHECK (state IN ('pending','running','done','failed','abandoned')), \
            attempts INTEGER NOT NULL DEFAULT 0 CHECK (typeof(attempts) = 'integer' AND attempts >= 0), \
            last_error_code TEXT, \
            updated_at INTEGER NOT NULL CHECK (typeof(updated_at) = 'integer')\
          ); \
          CREATE INDEX jobs_state_due ON jobs (state, due_at, id);",
}];

/// Stable error codes. Never carries SQL text, payload text or ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// Any SQLite engine error (message deliberately dropped).
    Sqlite,
    /// Any error from the storage layer (migration refusal etc.).
    Storage(StorageError),
    InvalidKind,
    InvalidDedupeKey,
    PayloadTooLarge,
    /// A time argument was negative.
    InvalidTime,
    InvalidPolicy,
    NotFound,
    /// The job exists but is not `running` (complete/fail on a stale handle).
    NotRunning,
}

impl QueueError {
    pub fn code(&self) -> &'static str {
        match self {
            QueueError::Sqlite => "sqlite_error",
            QueueError::Storage(_) => "storage_error",
            QueueError::InvalidKind => "invalid_kind",
            QueueError::InvalidDedupeKey => "invalid_dedupe_key",
            QueueError::PayloadTooLarge => "payload_too_large",
            QueueError::InvalidTime => "invalid_time",
            QueueError::InvalidPolicy => "invalid_policy",
            QueueError::NotFound => "not_found",
            QueueError::NotRunning => "not_running",
        }
    }
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for QueueError {}

impl From<rusqlite::Error> for QueueError {
    fn from(_: rusqlite::Error) -> Self {
        QueueError::Sqlite
    }
}

impl From<StorageError> for QueueError {
    fn from(e: StorageError) -> Self {
        QueueError::Storage(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Pending,
    Running,
    Done,
    Failed,
    Abandoned,
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Pending => "pending",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Abandoned => "abandoned",
        }
    }

    pub fn from_db(s: &str) -> Option<JobState> {
        Some(match s {
            "pending" => JobState::Pending,
            "running" => JobState::Running,
            "done" => JobState::Done,
            "failed" => JobState::Failed,
            "abandoned" => JobState::Abandoned,
            _ => return None,
        })
    }

    /// `done`, `failed` and `abandoned` never change again through this module.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed | JobState::Abandoned
        )
    }
}

/// What a restart does to a job found `running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Idempotent effect: run it again.
    RunAgain,
    /// Irreversible effect: never repeat; abandon and report.
    AtMostOnce,
}

/// Caller-provided table of `(kind, policy)`; no hidden global state.
pub type KindTable<'a> = &'a [(&'a str, RestartPolicy)];

/// Policy for `kind`. A kind missing from the table is `None` (reconcile
/// treats it as at-most-once). If the table lists a kind more than once and any
/// entry says `AtMostOnce`, the result is `AtMostOnce`.
pub fn policy_for(table: KindTable<'_>, kind: &str) -> Option<RestartPolicy> {
    let mut found = None;
    for (k, p) in table {
        if *k == kind {
            if *p == RestartPolicy::AtMostOnce {
                return Some(RestartPolicy::AtMostOnce);
            }
            found = Some(*p);
        }
    }
    found
}

/// Retry rules for [`fail`] and [`reconcile_after_restart`]. A job may be
/// claimed at most `max_attempts` times; the delay after claim number `n` is
/// `min(cap_secs, base_secs * 2^(n-1))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub base_secs: i64,
    pub cap_secs: i64,
    pub max_attempts: u32,
}

impl RetryPolicy {
    pub const DEFAULT: RetryPolicy = RetryPolicy {
        base_secs: 30,
        cap_secs: 600,
        max_attempts: 5,
    };

    /// `max_attempts >= 1`, `base_secs >= 1`, `cap_secs >= base_secs`.
    pub fn validate(&self) -> Result<(), QueueError> {
        if self.max_attempts == 0 || self.base_secs < 1 || self.cap_secs < self.base_secs {
            return Err(QueueError::InvalidPolicy);
        }
        Ok(())
    }

    /// Delay after claim number `attempts` (1-based). Saturating: no overflow
    /// or shift panic for any input.
    pub fn backoff_secs(&self, attempts: i64) -> i64 {
        let n = attempts.saturating_sub(1).max(0);
        if n >= 62 {
            return self.cap_secs;
        }
        self.base_secs.saturating_mul(1i64 << n).min(self.cap_secs)
    }
}

/// Input of [`schedule`].
#[derive(Debug, Clone, Copy)]
pub struct NewJob<'a> {
    /// `[a-z0-9_]`, 1..=64 bytes.
    pub kind: &'a str,
    /// Printable ASCII without spaces (`0x21..=0x7e`), 1..=128 bytes.
    pub dedupe_key: &'a str,
    /// At most [`MAX_PAYLOAD_BYTES`]. Should be a JSON object; anything else is
    /// stored and then failed at claim time with [`CODE_PAYLOAD_MALFORMED`].
    pub payload_json: &'a str,
    pub due_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleOutcome {
    Created {
        id: i64,
    },
    /// The key already exists (in any state); nothing was written.
    Duplicate {
        id: i64,
        state: JobState,
    },
}

/// A job handed to a worker. `Debug` is redacted: it never prints the payload.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    pub id: i64,
    pub kind: String,
    pub payload_json: String,
    /// Claims used so far, including this one.
    pub attempts: i64,
}

impl std::fmt::Debug for ClaimedJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimedJob")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClaimBatch {
    /// Jobs this caller won, in `(due_at, id)` order.
    pub claimed: Vec<ClaimedJob>,
    /// Jobs refused at claim (malformed payload, or a stored kind that is not a
    /// plain token); they are now `failed` and the others in the call went on.
    pub payload_failed: Vec<i64>,
}

/// Read-only view of a row. Deliberately has no payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobView {
    pub id: i64,
    pub kind: String,
    pub dedupe_key: String,
    pub due_at: i64,
    pub state: JobState,
    pub attempts: i64,
    /// Diagnostic only: never used to decide whether to retry.
    pub last_error_code: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    /// Back to `pending`, claimable at `due_at`.
    Retry { due_at: i64, attempts: i64 },
    /// `max_attempts` reached: terminal `failed`.
    Failed { attempts: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// `RunAgain`: pending again, due now.
    Requeued,
    /// Abandoned rather than repeating an interrupted effect (`AtMostOnce`
    /// or an uncertainty-aware kind whose claims are used up).
    AbandonedAtMostOnce,
    /// Kind not in the caller's table: abandoned, not re-run.
    AbandonedUnregistered,
    /// `RunAgain` but all attempts were already used: terminal `failed`.
    FailedMaxAttempts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileEntry {
    pub id: i64,
    pub kind: String,
    pub outcome: ReconcileOutcome,
}

/// Every job found `running` and what was done to it. No payload text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub entries: Vec<ReconcileEntry>,
}

impl Report {
    /// The entries that were abandoned (and so must be surfaced to staff).
    pub fn abandoned(&self) -> Vec<&ReconcileEntry> {
        self.entries
            .iter()
            .filter(|e| {
                matches!(
                    e.outcome,
                    ReconcileOutcome::AbandonedAtMostOnce | ReconcileOutcome::AbandonedUnregistered
                )
            })
            .collect()
    }
}

fn valid_token(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_dedupe_key(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_DEDUPE_KEY_LEN && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Read a TEXT column as raw bytes and convert it here, so one row holding
/// invalid UTF-8 (only possible when written around this API: the schema checks
/// `typeof` but not encoding) cannot make a whole query fail. Anything that is
/// not valid UTF-8 text (including NULL) reads as `None`.
fn text_col(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Option<String>> {
    Ok(match r.get_ref(i)? {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok().map(str::to_owned),
        _ => None,
    })
}

fn check_time(t: i64) -> Result<(), QueueError> {
    if t < 0 {
        Err(QueueError::InvalidTime)
    } else {
        Ok(())
    }
}

/// A payload is plainly valid only if it is within the size cap and parses as
/// one JSON object. Everything else (scalars, arrays, trailing text, too deep)
/// is refused as a class.
fn payload_is_plain_object(text: &str) -> bool {
    text.len() <= MAX_PAYLOAD_BYTES
        && matches!(
            serde_json::from_str::<serde_json::Value>(text),
            Ok(serde_json::Value::Object(_))
        )
}

/// Create the `jobs` table through the storage migration runner (idempotent).
pub fn init(conn: &mut Connection) -> Result<(), QueueError> {
    storage::migrate(conn, FEATURE, &MIGRATIONS)?;
    Ok(())
}

/// Insert a pending job, or report the existing one for this `dedupe_key`.
pub fn schedule(
    conn: &Connection,
    job: &NewJob<'_>,
    now: i64,
) -> Result<ScheduleOutcome, QueueError> {
    if !valid_token(job.kind, MAX_KIND_LEN) {
        return Err(QueueError::InvalidKind);
    }
    if !valid_dedupe_key(job.dedupe_key) {
        return Err(QueueError::InvalidDedupeKey);
    }
    if job.payload_json.len() > MAX_PAYLOAD_BYTES {
        return Err(QueueError::PayloadTooLarge);
    }
    check_time(job.due_at)?;
    check_time(now)?;
    let inserted = conn.execute(
        "INSERT INTO jobs (kind, dedupe_key, payload_json, due_at, state, attempts, \
         last_error_code, updated_at) VALUES (?1, ?2, ?3, ?4, 'pending', 0, NULL, ?5) \
         ON CONFLICT(dedupe_key) DO NOTHING",
        params![job.kind, job.dedupe_key, job.payload_json, job.due_at, now],
    )?;
    if inserted == 1 {
        return Ok(ScheduleOutcome::Created {
            id: conn.last_insert_rowid(),
        });
    }
    let (id, state): (i64, String) = conn.query_row(
        "SELECT id, state FROM jobs WHERE dedupe_key = ?1",
        [job.dedupe_key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let state = JobState::from_db(&state).ok_or(QueueError::Sqlite)?;
    Ok(ScheduleOutcome::Duplicate { id, state })
}

/// The compare-and-swap: ONE statement moves `pending` -> `running`, bumps
/// `attempts` and stamps `updated_at`; the rows-affected count decides the
/// winner. Returns `true` only for the caller whose statement changed the row.
pub fn try_claim(conn: &Connection, id: i64, now: i64) -> Result<bool, QueueError> {
    check_time(now)?;
    let n = conn.execute(
        "UPDATE jobs SET state = 'running', attempts = attempts + 1, updated_at = ?2 \
         WHERE id = ?1 AND state = 'pending' AND due_at <= ?2",
        params![id, now],
    )?;
    Ok(n == 1)
}

/// Claim up to `limit` due jobs (`due_at <= now`), earliest first, ties by id.
/// `limit` caps the candidates examined: a candidate whose payload is refused,
/// or that another claimer wins first, uses one slot without being returned.
pub fn claim_due(conn: &Connection, now: i64, limit: u32) -> Result<ClaimBatch, QueueError> {
    check_time(now)?;
    let mut batch = ClaimBatch::default();
    if limit == 0 {
        return Ok(batch);
    }
    let candidates: Vec<(i64, Option<String>, Option<String>, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT id, kind, payload_json, attempts FROM jobs \
             WHERE state = 'pending' AND due_at <= ?1 ORDER BY due_at, id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now, i64::from(limit)], |r| {
            Ok((r.get(0)?, text_col(r, 1)?, text_col(r, 2)?, r.get(3)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    for (id, kind, payload_json, attempts) in candidates {
        // Judge each row on its own: a bad row is failed, the rest go on.
        let kind = kind.filter(|k| valid_token(k, MAX_KIND_LEN));
        let payload_ok = payload_json.as_deref().is_some_and(payload_is_plain_object);
        let refusal = if kind.is_none() {
            Some(CODE_KIND_MALFORMED)
        } else if !payload_ok {
            Some(CODE_PAYLOAD_MALFORMED)
        } else {
            None
        };
        if let Some(code) = refusal {
            let n = conn.execute(
                "UPDATE jobs SET state = 'failed', attempts = attempts + 1, \
                 last_error_code = ?3, updated_at = ?2 \
                 WHERE id = ?1 AND state = 'pending' AND due_at <= ?2",
                params![id, now, code],
            )?;
            if n == 1 {
                batch.payload_failed.push(id);
            }
            continue;
        }
        let (Some(kind), Some(payload_json)) = (kind, payload_json) else {
            continue;
        };
        if try_claim(conn, id, now)? {
            batch.claimed.push(ClaimedJob {
                id,
                kind,
                payload_json,
                attempts: attempts + 1,
            });
        }
    }
    Ok(batch)
}

fn missing_job_error(conn: &Connection, id: i64) -> Result<QueueError, QueueError> {
    let exists: Option<i64> = conn
        .query_row("SELECT 1 FROM jobs WHERE id = ?1", [id], |r| r.get(0))
        .optional()?;
    Ok(if exists.is_some() {
        QueueError::NotRunning
    } else {
        QueueError::NotFound
    })
}

/// `running` -> `done`. Refused (`NotRunning`/`NotFound`) for any other state.
pub fn complete(conn: &Connection, id: i64, now: i64) -> Result<(), QueueError> {
    check_time(now)?;
    let n = conn.execute(
        "UPDATE jobs SET state = 'done', updated_at = ?2 WHERE id = ?1 AND state = 'running'",
        params![id, now],
    )?;
    if n == 1 {
        Ok(())
    } else {
        Err(missing_job_error(conn, id)?)
    }
}

/// Record a failed run of a `running` job. Retry (back to `pending` at
/// `now + backoff`) while claims used `< max_attempts`; otherwise `failed`.
/// The retry decision comes from the per-job `attempts` count, not from the
/// error code. A `code` that is not `[a-z0-9_]{1,48}` is stored as
/// [`CODE_INVALID_CODE`] so a bad code cannot leave the job stuck running.
pub fn fail(
    conn: &Connection,
    id: i64,
    code: &str,
    now: i64,
    policy: &RetryPolicy,
) -> Result<FailOutcome, QueueError> {
    fail_after(conn, id, code, now, policy, 0, false).map(|(outcome, _)| outcome)
}

pub(super) const CODE_OUTCOME_UNKNOWN: &str = "job_outcome_unknown";

/// Like `fail`, but honors Retry-After and can preserve an unresolved effect.
/// The second return value says whether exhaustion abandoned an uncertain job.
pub(super) fn fail_after(
    conn: &Connection,
    id: i64,
    code: &str,
    now: i64,
    policy: &RetryPolicy,
    min_delay_secs: i64,
    track_uncertainty: bool,
) -> Result<(FailOutcome, bool), QueueError> {
    policy.validate()?;
    check_time(now)?;
    let row = conn
        .query_row(
            "SELECT attempts, last_error_code FROM jobs WHERE id = ?1 AND state = 'running'",
            [id],
            |r| Ok((r.get::<_, i64>(0)?, text_col(r, 1)?)),
        )
        .optional()?;
    let Some((attempts, previous)) = row else {
        return Err(missing_job_error(conn, id)?);
    };
    let uncertain = track_uncertainty
        && (code == CODE_OUTCOME_UNKNOWN || previous.as_deref() == Some(CODE_OUTCOME_UNKNOWN));
    let code = if uncertain {
        CODE_OUTCOME_UNKNOWN
    } else if valid_token(code, MAX_CODE_LEN) {
        code
    } else {
        CODE_INVALID_CODE
    };
    let exhausted = attempts >= i64::from(policy.max_attempts);
    let due_at = now.saturating_add(policy.backoff_secs(attempts).max(min_delay_secs));
    // `attempts = ?3` makes this a CAS on the run we just read: if the row
    // changed between the read and this write, nothing is updated. It does not
    // identify the worker: a stale `fail` from an earlier run is not detectable.
    let n = if exhausted {
        conn.execute(
            "UPDATE jobs SET state = ?5, last_error_code = ?4, updated_at = ?2 \
             WHERE id = ?1 AND state = 'running' AND attempts = ?3",
            params![
                id,
                now,
                attempts,
                code,
                if uncertain { "abandoned" } else { "failed" }
            ],
        )?
    } else {
        conn.execute(
            "UPDATE jobs SET state = 'pending', due_at = ?5, last_error_code = ?4, \
             updated_at = ?2 WHERE id = ?1 AND state = 'running' AND attempts = ?3",
            params![id, now, attempts, code, due_at],
        )?
    };
    if n != 1 {
        return Err(QueueError::NotRunning);
    }
    Ok(if exhausted {
        (FailOutcome::Failed { attempts }, uncertain)
    } else {
        (FailOutcome::Retry { due_at, attempts }, false)
    })
}

type JobRow = (
    i64,
    Option<String>,
    Option<String>,
    i64,
    String,
    i64,
    Option<String>,
    i64,
);

/// Read one job (no payload).
pub fn get(conn: &Connection, id: i64) -> Result<Option<JobView>, QueueError> {
    let row: Option<JobRow> = conn
        .query_row(
            "SELECT id, kind, dedupe_key, due_at, state, attempts, last_error_code, updated_at \
             FROM jobs WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    text_col(r, 1)?,
                    text_col(r, 2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    text_col(r, 6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()?;
    let Some((id, kind, dedupe_key, due_at, state, attempts, last_error_code, updated_at)) = row
    else {
        return Ok(None);
    };
    Ok(Some(JobView {
        id,
        kind: kind
            .filter(|k| valid_token(k, MAX_KIND_LEN))
            .unwrap_or_else(|| KIND_PLACEHOLDER.to_string()),
        dedupe_key: dedupe_key
            .filter(|k| valid_dedupe_key(k))
            .unwrap_or_else(|| KEY_PLACEHOLDER.to_string()),
        due_at,
        state: JobState::from_db(&state).ok_or(QueueError::Sqlite)?,
        attempts,
        last_error_code,
        updated_at,
    }))
}

/// Resolve every job found `running` after a restart. Call it once at startup,
/// BEFORE any worker claims: there is no lease, so a `running` job of a live
/// worker would be indistinguishable from a crashed one.
///
/// - `RunAgain` kind with claims left: `pending`, due at `min(due_at, now)`.
/// - `RunAgain` kind with `attempts >= max_attempts`: `failed` (a job that
///   crashes the process every time cannot loop forever).
/// - `AtMostOnce` or a kind missing from `table`: `abandoned`, and reported.
///
/// Each job is resolved with its own compare-and-swap (`WHERE state='running'`),
/// so a failure midway can be repaired by calling this again.
pub fn reconcile_after_restart(
    conn: &Connection,
    now: i64,
    table: KindTable<'_>,
    retry: &RetryPolicy,
) -> Result<Report, QueueError> {
    retry.validate()?;
    reconcile_with_retry(conn, now, table, |_| (retry.max_attempts, false))
}

/// Runner variant: resolve each registered kind's validated retry policy.
pub(super) fn reconcile_with_retry(
    conn: &Connection,
    now: i64,
    table: KindTable<'_>,
    retry_for: impl Fn(&str) -> (u32, bool),
) -> Result<Report, QueueError> {
    check_time(now)?;
    let running: Vec<(i64, Option<String>, i64)> = {
        let mut stmt = conn
            .prepare("SELECT id, kind, attempts FROM jobs WHERE state = 'running' ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, text_col(r, 1)?, r.get(2)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut report = Report::default();
    for (id, kind, attempts) in running {
        // A kind that is not a plain token (or not UTF-8) cannot be in the
        // caller's table: fail closed (abandoned, reported) without echoing it.
        let kind = kind.filter(|k| valid_token(k, MAX_KIND_LEN));
        let policy = kind
            .as_deref()
            .and_then(|k| policy_for(table, k).map(|p| (p, retry_for(k))));
        let (outcome, n) = match policy {
            Some((RestartPolicy::RunAgain, (max_attempts, track_uncertainty)))
                if attempts < i64::from(max_attempts) =>
            {
                (
                    ReconcileOutcome::Requeued,
                    conn.execute(
                        "UPDATE jobs SET state = 'pending', due_at = MIN(due_at, ?2), \
                     last_error_code = ?3, updated_at = ?2 WHERE id = ?1 AND state = 'running'",
                        params![
                            id,
                            now,
                            if track_uncertainty {
                                CODE_OUTCOME_UNKNOWN
                            } else {
                                CODE_RESTART_RERUN
                            }
                        ],
                    )?,
                )
            }
            Some((RestartPolicy::RunAgain, (_, true))) => (
                ReconcileOutcome::AbandonedAtMostOnce,
                set_terminal(conn, id, now, "abandoned", CODE_OUTCOME_UNKNOWN)?,
            ),
            Some((RestartPolicy::RunAgain, _)) => (
                ReconcileOutcome::FailedMaxAttempts,
                set_terminal(conn, id, now, "failed", CODE_RESTART_MAX_ATTEMPTS)?,
            ),
            Some((RestartPolicy::AtMostOnce, _)) => (
                ReconcileOutcome::AbandonedAtMostOnce,
                set_terminal(conn, id, now, "abandoned", CODE_RESTART_AT_MOST_ONCE)?,
            ),
            None => (
                ReconcileOutcome::AbandonedUnregistered,
                set_terminal(conn, id, now, "abandoned", CODE_RESTART_UNREGISTERED)?,
            ),
        };
        if n == 1 {
            report.entries.push(ReconcileEntry {
                id,
                kind: kind.unwrap_or_else(|| KIND_PLACEHOLDER.to_string()),
                outcome,
            });
        }
    }
    Ok(report)
}

/// `state` is one of two literals chosen by this module, never caller input.
fn set_terminal(
    conn: &Connection,
    id: i64,
    now: i64,
    state: &'static str,
    code: &'static str,
) -> Result<usize, QueueError> {
    let n = conn.execute(
        "UPDATE jobs SET state = ?3, last_error_code = ?4, updated_at = ?2 \
         WHERE id = ?1 AND state = 'running'",
        params![id, now, state, code],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    const D: RetryPolicy = RetryPolicy::DEFAULT;
    const TABLE: KindTable<'static> = &[
        ("rerun", RestartPolicy::RunAgain),
        ("once", RestartPolicy::AtMostOnce),
    ];

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("k02-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mem() -> Connection {
        let mut c = storage::open(":memory:", &[]).unwrap();
        init(&mut c).unwrap();
        c
    }

    fn open_file(path: &str) -> Connection {
        let mut c = storage::open(path, &[]).unwrap();
        init(&mut c).unwrap();
        c
    }

    fn nj<'a>(kind: &'a str, key: &'a str, due: i64) -> NewJob<'a> {
        NewJob {
            kind,
            dedupe_key: key,
            payload_json: "{\"id\":\"1\"}",
            due_at: due,
        }
    }

    fn created(c: &Connection, kind: &str, key: &str, due: i64, now: i64) -> i64 {
        match schedule(c, &nj(kind, key, due), now).unwrap() {
            ScheduleOutcome::Created { id } => id,
            other => panic!("expected Created, got {other:?}"),
        }
    }

    fn state_of(c: &Connection, id: i64) -> JobState {
        get(c, id).unwrap().unwrap().state
    }

    fn ids(b: &ClaimBatch) -> Vec<i64> {
        b.claimed.iter().map(|j| j.id).collect()
    }

    // ---- schema ----

    #[test]
    fn migration_creates_jobs_table_with_state_check_and_is_idempotent() {
        let mut c = mem();
        init(&mut c).unwrap();
        let v: i64 = c
            .query_row(
                "SELECT version FROM schema_versions WHERE feature = 'jobs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 1);
        let bad = c.execute(
            "INSERT INTO jobs (kind, dedupe_key, payload_json, due_at, state, attempts, updated_at) \
             VALUES ('k', 'k1', '{}', 1, 'bogus', 0, 1)",
            [],
        );
        assert!(bad.is_err(), "state CHECK must refuse unknown states");
        let ok = c.execute(
            "INSERT INTO jobs (kind, dedupe_key, payload_json, due_at, state, attempts, updated_at) \
             VALUES ('k', 'k1', '{}', 1, 'pending', 0, 1)",
            [],
        );
        assert!(ok.is_ok());
        let dup = c.execute(
            "INSERT INTO jobs (kind, dedupe_key, payload_json, due_at, state, attempts, updated_at) \
             VALUES ('k', 'k1', '{}', 1, 'pending', 0, 1)",
            [],
        );
        assert!(dup.is_err(), "dedupe_key must be UNIQUE in the schema");
    }

    // ---- ordering, boundary, limit ----

    #[test]
    fn claim_returns_due_jobs_ordered_by_due_then_id() {
        let c = mem();
        let late = created(&c, "rerun", "late", 300, 0);
        let early = created(&c, "rerun", "early", 100, 0);
        let tie_a = created(&c, "rerun", "tie_a", 200, 0);
        let tie_b = created(&c, "rerun", "tie_b", 200, 0);
        let future = created(&c, "rerun", "future", 10_000, 0);
        let batch = claim_due(&c, 500, 10).unwrap();
        assert_eq!(ids(&batch), vec![early, tie_a, tie_b, late]);
        assert!(batch.payload_failed.is_empty());
        assert_eq!(state_of(&c, future), JobState::Pending);
        assert_eq!(state_of(&c, early), JobState::Running);
    }

    #[test]
    fn claim_boundary_due_equal_now_is_claimable_one_before_is_not() {
        let c = mem();
        let id = created(&c, "rerun", "edge", 100, 0);
        assert!(claim_due(&c, 99, 10).unwrap().claimed.is_empty());
        assert_eq!(state_of(&c, id), JobState::Pending);
        let b = claim_due(&c, 100, 10).unwrap();
        assert_eq!(ids(&b), vec![id]);
        assert_eq!(b.claimed[0].attempts, 1);
        assert_eq!(b.claimed[0].kind, "rerun");
        assert_eq!(b.claimed[0].payload_json, "{\"id\":\"1\"}");
    }

    #[test]
    fn claim_limit_takes_earliest_first_and_zero_claims_nothing() {
        let c = mem();
        let mut all = Vec::new();
        for i in 0..5 {
            all.push(created(&c, "rerun", &format!("k{i}"), 10 + i, 0));
        }
        assert!(claim_due(&c, 100, 0).unwrap().claimed.is_empty());
        assert_eq!(state_of(&c, all[0]), JobState::Pending);
        let first = claim_due(&c, 100, 2).unwrap();
        assert_eq!(ids(&first), vec![all[0], all[1]]);
        let rest = claim_due(&c, 100, 10).unwrap();
        assert_eq!(ids(&rest), vec![all[2], all[3], all[4]]);
        assert!(claim_due(&c, 100, 10).unwrap().claimed.is_empty());
    }

    #[test]
    fn claimed_job_is_never_handed_out_twice() {
        let c = mem();
        created(&c, "rerun", "a", 0, 0);
        assert_eq!(claim_due(&c, 10, 10).unwrap().claimed.len(), 1);
        assert!(claim_due(&c, 10, 10).unwrap().claimed.is_empty());
        assert!(claim_due(&c, 1_000_000, 10).unwrap().claimed.is_empty());
    }

    // ---- dedupe ----

    #[test]
    fn same_dedupe_key_twice_yields_exactly_one_job_and_first_wins() {
        let c = mem();
        let first = NewJob {
            kind: "rerun",
            dedupe_key: "flash:1:2",
            payload_json: "{\"v\":\"first\"}",
            due_at: 50,
        };
        let second = NewJob {
            kind: "once",
            dedupe_key: "flash:1:2",
            payload_json: "{\"v\":\"second\"}",
            due_at: 5,
        };
        let id = match schedule(&c, &first, 0).unwrap() {
            ScheduleOutcome::Created { id } => id,
            o => panic!("{o:?}"),
        };
        assert_eq!(
            schedule(&c, &second, 1).unwrap(),
            ScheduleOutcome::Duplicate {
                id,
                state: JobState::Pending
            }
        );
        let b = claim_due(&c, 100, 10).unwrap();
        assert_eq!(b.claimed.len(), 1);
        assert_eq!(b.claimed[0].payload_json, "{\"v\":\"first\"}");
        assert_eq!(b.claimed[0].kind, "rerun");
        let v = get(&c, id).unwrap().unwrap();
        assert_eq!(v.due_at, 50, "duplicate must not move the original due_at");
    }

    #[test]
    fn dedupe_key_stays_consumed_after_done_failed_and_abandoned() {
        let c = mem();
        let done = created(&c, "rerun", "k_done", 0, 0);
        let failed = created(&c, "rerun", "k_failed", 0, 0);
        let abandoned = created(&c, "once", "k_ab", 0, 0);
        let pend = created(&c, "rerun", "k_pend", 0, 0);
        let b = claim_due(&c, 1, 3).unwrap();
        assert_eq!(b.claimed.len(), 3);
        complete(&c, done, 2).unwrap();
        let strict = RetryPolicy {
            base_secs: 1,
            cap_secs: 1,
            max_attempts: 1,
        };
        assert_eq!(
            fail(&c, failed, "boom", 2, &strict).unwrap(),
            FailOutcome::Failed { attempts: 1 }
        );
        reconcile_after_restart(&c, 3, TABLE, &D).unwrap();
        assert_eq!(state_of(&c, abandoned), JobState::Abandoned);
        for (key, id, st) in [
            ("k_done", done, JobState::Done),
            ("k_failed", failed, JobState::Failed),
            ("k_ab", abandoned, JobState::Abandoned),
            ("k_pend", pend, JobState::Pending),
        ] {
            assert_eq!(
                schedule(&c, &nj("rerun", key, 0), 4).unwrap(),
                ScheduleOutcome::Duplicate { id, state: st },
                "{key}"
            );
        }
        // nothing was resurrected: only the still-pending job is claimable
        assert_eq!(ids(&claim_due(&c, 10, 10).unwrap()), vec![pend]);
    }

    #[test]
    fn dedupe_is_global_across_kinds() {
        let c = mem();
        let id = created(&c, "rerun", "shared", 0, 0);
        assert_eq!(
            schedule(&c, &nj("once", "shared", 0), 0).unwrap(),
            ScheduleOutcome::Duplicate {
                id,
                state: JobState::Pending
            }
        );
        assert_eq!(get(&c, id).unwrap().unwrap().kind, "rerun");
    }

    // ---- CAS claim, two connections ----

    #[test]
    fn two_connections_exactly_one_wins_the_cas() {
        let dir = scratch("cas_two");
        let p = dir.join("core.sqlite");
        let ps = p.to_str().unwrap();
        let a = open_file(ps);
        let b = open_file(ps);
        let id = created(&a, "rerun", "only", 0, 0);
        let wa = try_claim(&a, id, 10).unwrap();
        let wb = try_claim(&b, id, 10).unwrap();
        assert!(wa ^ wb, "exactly one claimer wins: a={wa} b={wb}");
        assert!(wa, "first caller wins");
        assert_eq!(state_of(&b, id), JobState::Running);
        assert_eq!(get(&b, id).unwrap().unwrap().attempts, 1, "claimed once");
        // the loser sees nothing left to claim
        assert!(claim_due(&b, 10, 10).unwrap().claimed.is_empty());
    }

    #[test]
    fn try_claim_refuses_not_yet_pending_states_and_unknown_ids() {
        let c = mem();
        let id = created(&c, "rerun", "x", 0, 0);
        assert!(try_claim(&c, id, 1).unwrap());
        assert!(!try_claim(&c, id, 1).unwrap(), "running is not claimable");
        complete(&c, id, 2).unwrap();
        assert!(!try_claim(&c, id, 3).unwrap(), "done is not claimable");
        assert!(!try_claim(&c, 424_242, 3).unwrap());
    }

    #[test]
    fn concurrent_claimers_partition_the_jobs_with_no_overlap() {
        let dir = scratch("cas_threads");
        let p = dir.join("core.sqlite");
        let ps = p.to_str().unwrap().to_string();
        {
            let c = open_file(&ps);
            for i in 0..60 {
                created(&c, "rerun", &format!("job{i}"), 0, 0);
            }
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let ps = ps.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let c = open_file(&ps);
                    barrier.wait();
                    let mut mine = Vec::new();
                    loop {
                        let b = claim_due(&c, 10, 7).unwrap();
                        if b.claimed.is_empty() {
                            break;
                        }
                        mine.extend(ids(&b));
                    }
                    mine
                })
            })
            .collect();
        let mut seen = HashSet::new();
        let mut total = 0;
        for h in handles {
            for id in h.join().unwrap() {
                total += 1;
                assert!(seen.insert(id), "job {id} claimed by two workers");
            }
        }
        assert_eq!(total, 60);
    }

    // ---- complete ----

    #[test]
    fn complete_moves_running_to_done_once() {
        let c = mem();
        let id = created(&c, "rerun", "a", 0, 0);
        assert_eq!(complete(&c, id, 1), Err(QueueError::NotRunning));
        claim_due(&c, 5, 1).unwrap();
        complete(&c, id, 7).unwrap();
        let v = get(&c, id).unwrap().unwrap();
        assert_eq!(v.state, JobState::Done);
        assert_eq!(v.updated_at, 7);
        assert_eq!(complete(&c, id, 8), Err(QueueError::NotRunning));
        assert_eq!(get(&c, id).unwrap().unwrap().updated_at, 7);
        assert_eq!(complete(&c, 9999, 8), Err(QueueError::NotFound));
        assert!(claim_due(&c, 100, 10).unwrap().claimed.is_empty());
    }

    // ---- retry / backoff ----

    #[test]
    fn fail_retries_with_doubling_backoff_then_fails_at_max_attempts() {
        let c = mem();
        let id = created(&c, "rerun", "r", 0, 0);
        let mut now = 1_000;
        let mut delays = Vec::new();
        for attempt in 1..=4 {
            let b = claim_due(&c, now, 1).unwrap();
            assert_eq!(ids(&b), vec![id], "attempt {attempt} claimable");
            assert_eq!(b.claimed[0].attempts, attempt);
            match fail(&c, id, "net_error", now, &D).unwrap() {
                FailOutcome::Retry { due_at, attempts } => {
                    assert_eq!(attempts, attempt);
                    delays.push(due_at - now);
                    // not claimable one second early, claimable exactly on time
                    assert!(claim_due(&c, due_at - 1, 1).unwrap().claimed.is_empty());
                    assert_eq!(state_of(&c, id), JobState::Pending);
                    now = due_at;
                }
                other => panic!("expected retry, got {other:?}"),
            }
        }
        assert_eq!(delays, vec![30, 60, 120, 240]);
        assert_eq!(ids(&claim_due(&c, now, 1).unwrap()), vec![id]);
        assert_eq!(
            fail(&c, id, "net_error", now, &D).unwrap(),
            FailOutcome::Failed { attempts: 5 }
        );
        let v = get(&c, id).unwrap().unwrap();
        assert_eq!(v.state, JobState::Failed);
        assert_eq!(v.last_error_code.as_deref(), Some("net_error"));
        assert!(claim_due(&c, i64::MAX, 10).unwrap().claimed.is_empty());
    }

    #[test]
    fn backoff_is_capped() {
        let policy = RetryPolicy {
            base_secs: 100,
            cap_secs: 250,
            max_attempts: 6,
        };
        let c = mem();
        let id = created(&c, "rerun", "cap", 0, 0);
        let mut now = 0;
        let mut delays = Vec::new();
        for _ in 0..5 {
            claim_due(&c, now, 1).unwrap();
            match fail(&c, id, "e", now, &policy).unwrap() {
                FailOutcome::Retry { due_at, .. } => {
                    delays.push(due_at - now);
                    now = due_at;
                }
                o => panic!("{o:?}"),
            }
        }
        assert_eq!(delays, vec![100, 200, 250, 250, 250]);
        claim_due(&c, now, 1).unwrap();
        assert_eq!(
            fail(&c, id, "e", now, &policy).unwrap(),
            FailOutcome::Failed { attempts: 6 }
        );
    }

    #[test]
    fn backoff_function_handles_extreme_attempts_without_panic_or_overflow() {
        let p = RetryPolicy {
            base_secs: 30,
            cap_secs: 600,
            max_attempts: 5,
        };
        assert_eq!(p.backoff_secs(1), 30);
        assert_eq!(p.backoff_secs(2), 60);
        assert_eq!(p.backoff_secs(5), 480);
        assert_eq!(p.backoff_secs(6), 600);
        for n in [62, 63, 64, 65, 1_000, i64::MAX] {
            assert_eq!(p.backoff_secs(n), 600, "attempts {n}");
        }
        assert_eq!(p.backoff_secs(0), 30);
        assert_eq!(p.backoff_secs(-5), 30);
        let huge = RetryPolicy {
            base_secs: i64::MAX / 2 + 1,
            cap_secs: i64::MAX,
            max_attempts: 9,
        };
        assert_eq!(huge.backoff_secs(3), i64::MAX);
    }

    #[test]
    fn fail_near_i64_max_clock_saturates_instead_of_wrapping() {
        let c = mem();
        let id = created(&c, "rerun", "sat", 0, 0);
        claim_due(&c, 0, 1).unwrap();
        match fail(&c, id, "e", i64::MAX - 5, &D).unwrap() {
            FailOutcome::Retry { due_at, .. } => assert_eq!(due_at, i64::MAX),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn invalid_retry_policies_are_refused_before_any_write() {
        let c = mem();
        let id = created(&c, "rerun", "p", 0, 0);
        claim_due(&c, 0, 1).unwrap();
        for bad in [
            RetryPolicy {
                base_secs: 30,
                cap_secs: 600,
                max_attempts: 0,
            },
            RetryPolicy {
                base_secs: 0,
                cap_secs: 600,
                max_attempts: 3,
            },
            RetryPolicy {
                base_secs: 100,
                cap_secs: 50,
                max_attempts: 3,
            },
            RetryPolicy {
                base_secs: -1,
                cap_secs: 50,
                max_attempts: 3,
            },
        ] {
            assert_eq!(bad.validate(), Err(QueueError::InvalidPolicy));
            assert_eq!(fail(&c, id, "e", 1, &bad), Err(QueueError::InvalidPolicy));
            assert_eq!(
                reconcile_after_restart(&c, 1, TABLE, &bad),
                Err(QueueError::InvalidPolicy)
            );
            assert_eq!(state_of(&c, id), JobState::Running);
        }
        assert_eq!(D.validate(), Ok(()));
    }

    #[test]
    fn fail_on_non_running_or_unknown_job_is_refused() {
        let c = mem();
        let id = created(&c, "rerun", "f", 0, 0);
        assert_eq!(fail(&c, id, "e", 1, &D), Err(QueueError::NotRunning));
        assert_eq!(fail(&c, 777, "e", 1, &D), Err(QueueError::NotFound));
        claim_due(&c, 1, 1).unwrap();
        complete(&c, id, 2).unwrap();
        assert_eq!(fail(&c, id, "e", 3, &D), Err(QueueError::NotRunning));
        assert_eq!(state_of(&c, id), JobState::Done);
    }

    #[test]
    fn hostile_error_code_is_replaced_by_a_stable_code_and_job_still_progresses() {
        let c = mem();
        let id = created(&c, "rerun", "h", 0, 0);
        let mut now = 1;
        claim_due(&c, 1, 1).unwrap();
        let hostile = "user said: hello <@123> DROP TABLE jobs;";
        assert!(matches!(
            fail(&c, id, hostile, 1, &D).unwrap(),
            FailOutcome::Retry { .. }
        ));
        let v = get(&c, id).unwrap().unwrap();
        assert_eq!(v.state, JobState::Pending);
        assert_eq!(v.last_error_code.as_deref(), Some(CODE_INVALID_CODE));
        for code in [
            "",
            "UPPER",
            "a b",
            &"x".repeat(MAX_CODE_LEN + 1),
            "é",
            "a\nb",
        ] {
            now += 10_000;
            assert_eq!(ids(&claim_due(&c, now, 1).unwrap()), vec![id]);
            fail(&c, id, code, now, &D).unwrap();
            assert_eq!(
                get(&c, id).unwrap().unwrap().last_error_code.as_deref(),
                Some(CODE_INVALID_CODE),
                "{code:?}"
            );
            if state_of(&c, id) == JobState::Failed {
                break;
            }
        }
    }

    // ---- payloads ----

    #[test]
    fn malformed_payload_fails_with_stable_code_and_never_panics() {
        let deep = "[".repeat(2000);
        let cases: Vec<&str> = vec![
            "",
            "{",
            "not json secret_marker_zz",
            "{\"secret_marker_zz\": ",
            "[1,2]",
            "null",
            "42",
            "\"secret_marker_zz\"",
            "{\"a\":1} secret_marker_zz",
            "{\"a\":1}{\"b\":2}",
            &deep,
        ];
        for (i, payload) in cases.iter().enumerate() {
            let c = mem();
            let key = format!("m{i}");
            let id = match schedule(
                &c,
                &NewJob {
                    kind: "rerun",
                    dedupe_key: &key,
                    payload_json: payload,
                    due_at: 0,
                },
                0,
            )
            .unwrap()
            {
                ScheduleOutcome::Created { id } => id,
                o => panic!("{o:?}"),
            };
            let batch = claim_due(&c, 10, 10).unwrap();
            assert!(batch.claimed.is_empty(), "case {i} was handed out");
            assert_eq!(batch.payload_failed, vec![id], "case {i}");
            let v = get(&c, id).unwrap().unwrap();
            assert_eq!(v.state, JobState::Failed, "case {i}");
            assert_eq!(v.last_error_code.as_deref(), Some(CODE_PAYLOAD_MALFORMED));
            let text = format!("{batch:?} {v:?}");
            assert!(!text.contains("secret_marker_zz"), "payload leaked: {text}");
            // terminal: never claimable again
            assert!(claim_due(&c, i64::MAX, 10).unwrap().claimed.is_empty());
        }
    }

    #[test]
    fn plain_object_payloads_are_accepted() {
        let c = mem();
        for (i, p) in [
            "{}",
            "  {\"a\": [1, 2, {\"b\": null}]}  ",
            "{\"id\":\"9\"}\n",
        ]
        .iter()
        .enumerate()
        {
            let key = format!("ok{i}");
            schedule(
                &c,
                &NewJob {
                    kind: "rerun",
                    dedupe_key: &key,
                    payload_json: p,
                    due_at: 0,
                },
                0,
            )
            .unwrap();
        }
        let b = claim_due(&c, 1, 10).unwrap();
        assert_eq!(b.claimed.len(), 3);
        assert!(b.payload_failed.is_empty());
    }

    #[test]
    fn oversize_payload_is_refused_at_schedule_and_at_claim() {
        let c = mem();
        let big = format!("{{\"k\":\"{}\"}}", "a".repeat(MAX_PAYLOAD_BYTES));
        assert_eq!(
            schedule(
                &c,
                &NewJob {
                    kind: "rerun",
                    dedupe_key: "big",
                    payload_json: &big,
                    due_at: 0
                },
                0
            ),
            Err(QueueError::PayloadTooLarge)
        );
        // nothing was stored: the key is still free
        assert!(matches!(
            schedule(&c, &nj("rerun", "big", 0), 0).unwrap(),
            ScheduleOutcome::Created { .. }
        ));
        // a row written around the API is still refused at claim
        c.execute(
            "INSERT INTO jobs (kind, dedupe_key, payload_json, due_at, state, attempts, updated_at) \
             VALUES ('rerun', 'raw_big', ?1, 0, 'pending', 0, 0)",
            [&big],
        )
        .unwrap();
        let b = claim_due(&c, 5, 10).unwrap();
        assert_eq!(b.claimed.len(), 1);
        assert_eq!(b.payload_failed.len(), 1);
    }

    #[test]
    fn payload_at_exact_limit_is_accepted() {
        let c = mem();
        let pad = MAX_PAYLOAD_BYTES - "{\"k\":\"\"}".len();
        let p = format!("{{\"k\":\"{}\"}}", "a".repeat(pad));
        assert_eq!(p.len(), MAX_PAYLOAD_BYTES);
        schedule(
            &c,
            &NewJob {
                kind: "rerun",
                dedupe_key: "edge",
                payload_json: &p,
                due_at: 0,
            },
            0,
        )
        .unwrap();
        assert_eq!(claim_due(&c, 1, 1).unwrap().claimed.len(), 1);
    }

    // ---- rows written around the API (qa-3 round 1) ----

    /// Insert a row whose `kind`/`payload_json` are given as raw SQL
    /// expressions (so they can be invalid UTF-8 text: the schema only checks
    /// `typeof(..) = 'text'`).
    fn raw_row(c: &Connection, kind_sql: &str, payload_sql: &str, key: &str, state: &str) -> i64 {
        c.execute(
            &format!(
                "INSERT INTO jobs (kind, dedupe_key, payload_json, due_at, state, attempts, updated_at) \
                 VALUES ({kind_sql}, '{key}', {payload_sql}, 0, '{state}', 1, 0)"
            ),
            [],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    #[test]
    fn try_claim_refuses_a_job_that_is_not_yet_due() {
        let c = mem();
        let id = created(&c, "rerun", "later", 100, 0);
        assert!(!try_claim(&c, id, 99).unwrap(), "one second early");
        let v = get(&c, id).unwrap().unwrap();
        assert_eq!(v.state, JobState::Pending);
        assert_eq!(
            v.attempts, 0,
            "a refused claim must not count as an attempt"
        );
        assert!(try_claim(&c, id, 100).unwrap(), "exactly due");
        assert_eq!(state_of(&c, id), JobState::Running);
    }

    #[test]
    fn non_utf8_payload_fails_only_that_row_and_other_jobs_are_still_claimed() {
        let c = mem();
        let before = created(&c, "rerun", "before", 0, 0);
        let bad = raw_row(
            &c,
            "'rerun'",
            "CAST(x'7bff7d' AS TEXT)",
            "bad_payload",
            "pending",
        );
        let after = created(&c, "rerun", "after", 0, 0);
        let b = claim_due(&c, 10, 10).expect("one bad row must not stop the queue");
        assert_eq!(ids(&b), vec![before, after]);
        assert_eq!(b.payload_failed, vec![bad]);
        let v = get(&c, bad).unwrap().unwrap();
        assert_eq!(v.state, JobState::Failed);
        assert_eq!(v.last_error_code.as_deref(), Some(CODE_PAYLOAD_MALFORMED));
        // and the next call is healthy too (it used to fail on every call)
        let again = created(&c, "rerun", "later", 5, 0);
        assert_eq!(ids(&claim_due(&c, 10, 10).unwrap()), vec![again]);
    }

    #[test]
    fn bad_kind_at_claim_fails_only_that_row_and_other_jobs_are_still_claimed() {
        let c = mem();
        let good = created(&c, "rerun", "good", 5, 0);
        let non_utf8 = raw_row(&c, "CAST(x'ff' AS TEXT)", "'{}'", "bad_kind_a", "pending");
        let spaced = raw_row(&c, "'Secret User Text'", "'{}'", "bad_kind_b", "pending");
        let b = claim_due(&c, 10, 10).expect("one bad row must not stop the queue");
        assert_eq!(ids(&b), vec![good]);
        assert_eq!(b.payload_failed, vec![non_utf8, spaced]);
        for id in [non_utf8, spaced] {
            let v = get(&c, id).unwrap().unwrap();
            assert_eq!(v.state, JobState::Failed);
            assert_eq!(v.last_error_code.as_deref(), Some(CODE_KIND_MALFORMED));
            assert_eq!(v.kind, KIND_PLACEHOLDER, "stored kind text is not echoed");
        }
        assert_eq!(state_of(&c, good), JobState::Running);
    }

    #[test]
    fn reconcile_handles_non_utf8_and_hostile_kinds_without_stopping_the_rest() {
        let c = mem();
        let once = raw_row(&c, "'once'", "'{}'", "once1", "running");
        let bad = raw_row(&c, "CAST(x'ff' AS TEXT)", "'{}'", "bad_a", "running");
        let spaced = raw_row(&c, "'Secret User Text'", "'{}'", "bad_b", "running");
        let rerun = raw_row(&c, "'rerun'", "'{}'", "rerun1", "running");
        let rep = reconcile_after_restart(&c, 50, TABLE, &D)
            .expect("one bad row must not stop reconcile");
        let by_id: HashMap<i64, &ReconcileEntry> = rep.entries.iter().map(|e| (e.id, e)).collect();
        assert_eq!(rep.entries.len(), 4, "{rep:?}");
        assert_eq!(by_id[&once].outcome, ReconcileOutcome::AbandonedAtMostOnce);
        assert_eq!(by_id[&once].kind, "once");
        assert_eq!(by_id[&rerun].outcome, ReconcileOutcome::Requeued);
        for id in [bad, spaced] {
            assert_eq!(by_id[&id].outcome, ReconcileOutcome::AbandonedUnregistered);
            assert_eq!(by_id[&id].kind, KIND_PLACEHOLDER);
            assert_eq!(state_of(&c, id), JobState::Abandoned);
        }
        assert_eq!(rep.abandoned().len(), 3);
        assert!(!format!("{rep:?}").contains("Secret"), "{rep:?}");
        assert_eq!(state_of(&c, once), JobState::Abandoned);
        assert_eq!(state_of(&c, rerun), JobState::Pending);
    }

    #[test]
    fn get_on_a_row_with_non_utf8_text_columns_does_not_error() {
        let c = mem();
        let id = raw_row(&c, "CAST(x'ff' AS TEXT)", "'{}'", "k", "pending");
        let v = get(&c, id).expect("must not be an engine error").unwrap();
        assert_eq!(v.kind, KIND_PLACEHOLDER);
        let id2 = raw_row(&c, "'rerun'", "'{}'", "x", "pending");
        c.execute(
            "UPDATE jobs SET dedupe_key = CAST(x'c328' AS TEXT), \
             last_error_code = CAST(x'fe' AS TEXT) WHERE id = ?1",
            [id2],
        )
        .unwrap();
        let v2 = get(&c, id2).unwrap().unwrap();
        assert_eq!(v2.dedupe_key, KEY_PLACEHOLDER);
        assert_eq!(v2.last_error_code, None);
    }

    // ---- input validation ----

    #[test]
    fn invalid_kind_and_dedupe_key_spellings_are_refused_and_store_nothing() {
        let c = mem();
        let long_kind = "k".repeat(MAX_KIND_LEN + 1);
        let long_key = "k".repeat(MAX_DEDUPE_KEY_LEN + 1);
        for kind in [
            "",
            "Rerun",
            "a b",
            "a-b",
            "a\nb",
            "é",
            "a\0b",
            long_kind.as_str(),
        ] {
            assert_eq!(
                schedule(&c, &nj(kind, "good", 0), 0),
                Err(QueueError::InvalidKind),
                "{kind:?}"
            );
        }
        for key in [
            "",
            "a b",
            "a\nb",
            "a\tb",
            "é",
            "a\0b",
            "a\u{202e}b",
            long_key.as_str(),
        ] {
            assert_eq!(
                schedule(&c, &nj("rerun", key, 0), 0),
                Err(QueueError::InvalidDedupeKey),
                "{key:?}"
            );
        }
        assert!(claim_due(&c, 100, 10).unwrap().claimed.is_empty());
        // boundary lengths are accepted
        let k_ok = "k".repeat(MAX_KIND_LEN);
        let d_ok = "d".repeat(MAX_DEDUPE_KEY_LEN);
        assert!(schedule(&c, &nj(&k_ok, &d_ok, 0), 0).is_ok());
        assert!(schedule(&c, &nj("flash_delete", "flash:123:456-x.y_z/#", 0), 0).is_ok());
    }

    #[test]
    fn negative_times_are_refused() {
        let c = mem();
        assert_eq!(
            schedule(&c, &nj("rerun", "a", -1), 0),
            Err(QueueError::InvalidTime)
        );
        assert_eq!(
            schedule(&c, &nj("rerun", "a", 0), -1),
            Err(QueueError::InvalidTime)
        );
        assert_eq!(claim_due(&c, -1, 1), Err(QueueError::InvalidTime));
        assert_eq!(complete(&c, 1, -1), Err(QueueError::InvalidTime));
        assert_eq!(fail(&c, 1, "e", -1, &D), Err(QueueError::InvalidTime));
        assert_eq!(
            reconcile_after_restart(&c, -1, TABLE, &D),
            Err(QueueError::InvalidTime)
        );
        assert!(matches!(
            schedule(&c, &nj("rerun", "a", 0), 0),
            Ok(ScheduleOutcome::Created { .. })
        ));
    }

    // ---- restart reconcile ----

    #[test]
    fn reconcile_reruns_run_again_and_abandons_and_reports_at_most_once() {
        let c = mem();
        let r = created(&c, "rerun", "r", 0, 0);
        let o = created(&c, "once", "o", 1, 0);
        let u = created(&c, "mystery", "u", 2, 0);
        let pending = created(&c, "once", "p", 9_999, 0);
        let done = created(&c, "rerun", "d", 3, 0);
        assert_eq!(claim_due(&c, 10, 10).unwrap().claimed.len(), 4);
        complete(&c, done, 11).unwrap();
        // --- crash: nothing completed r, o, u ---
        let rep = reconcile_after_restart(&c, 100, TABLE, &D).unwrap();
        let by_id: HashMap<i64, ReconcileOutcome> =
            rep.entries.iter().map(|e| (e.id, e.outcome)).collect();
        assert_eq!(rep.entries.len(), 3, "{rep:?}");
        assert_eq!(by_id[&r], ReconcileOutcome::Requeued);
        assert_eq!(by_id[&o], ReconcileOutcome::AbandonedAtMostOnce);
        assert_eq!(by_id[&u], ReconcileOutcome::AbandonedUnregistered);
        assert_eq!(rep.abandoned().len(), 2);
        assert!(rep.entries.iter().any(|e| e.id == o && e.kind == "once"));
        assert_eq!(state_of(&c, r), JobState::Pending);
        assert_eq!(state_of(&c, o), JobState::Abandoned);
        assert_eq!(state_of(&c, u), JobState::Abandoned);
        assert_eq!(state_of(&c, pending), JobState::Pending);
        assert_eq!(state_of(&c, done), JobState::Done);
        assert_eq!(
            get(&c, o).unwrap().unwrap().last_error_code.as_deref(),
            Some(CODE_RESTART_AT_MOST_ONCE)
        );
        assert_eq!(
            get(&c, u).unwrap().unwrap().last_error_code.as_deref(),
            Some(CODE_RESTART_UNREGISTERED)
        );
        // the RunAgain job runs again immediately; the abandoned ones never do
        let b = claim_due(&c, 100, 10).unwrap();
        assert_eq!(ids(&b), vec![r]);
        assert_eq!(b.claimed[0].attempts, 2);
        assert!(!try_claim(&c, o, i64::MAX).unwrap());
        assert!(!try_claim(&c, u, i64::MAX).unwrap());
        // a second reconcile finds nothing new to report except the re-claimed r
        let again = reconcile_after_restart(&c, 101, TABLE, &D).unwrap();
        assert_eq!(again.entries.len(), 1);
        assert_eq!(again.entries[0].id, r);
        let third = reconcile_after_restart(&c, 102, TABLE, &D).unwrap();
        assert!(third.entries.is_empty(), "{third:?}");
    }

    #[test]
    fn reconcile_on_clean_queue_changes_and_reports_nothing() {
        let c = mem();
        let a = created(&c, "once", "a", 0, 0);
        let b = created(&c, "rerun", "b", 0, 0);
        claim_due(&c, 1, 1).unwrap();
        complete(&c, a, 2).unwrap();
        let before = (get(&c, a).unwrap(), get(&c, b).unwrap());
        let rep = reconcile_after_restart(&c, 50, TABLE, &D).unwrap();
        assert!(rep.entries.is_empty());
        assert_eq!((get(&c, a).unwrap(), get(&c, b).unwrap()), before);
    }

    #[test]
    fn conflicting_duplicate_table_entries_resolve_to_at_most_once() {
        let table: KindTable<'_> = &[
            ("dup", RestartPolicy::RunAgain),
            ("dup", RestartPolicy::AtMostOnce),
        ];
        assert_eq!(policy_for(table, "dup"), Some(RestartPolicy::AtMostOnce));
        let rev: KindTable<'_> = &[
            ("dup", RestartPolicy::AtMostOnce),
            ("dup", RestartPolicy::RunAgain),
        ];
        assert_eq!(policy_for(rev, "dup"), Some(RestartPolicy::AtMostOnce));
        assert_eq!(policy_for(table, "other"), None);
        assert_eq!(policy_for(&[], "dup"), None);
        assert_eq!(policy_for(TABLE, "rerun"), Some(RestartPolicy::RunAgain));
        let c = mem();
        let id = created(&c, "dup", "d", 0, 0);
        claim_due(&c, 1, 1).unwrap();
        reconcile_after_restart(&c, 2, table, &D).unwrap();
        assert_eq!(state_of(&c, id), JobState::Abandoned);
    }

    #[test]
    fn crash_looping_run_again_job_stops_at_max_attempts_and_is_reported() {
        let c = mem();
        let id = created(&c, "rerun", "loop", 0, 0);
        let policy = RetryPolicy {
            base_secs: 1,
            cap_secs: 1,
            max_attempts: 3,
        };
        for round in 1..=3 {
            let b = claim_due(&c, 10 * round, 1).unwrap();
            assert_eq!(ids(&b), vec![id], "round {round}");
            let rep = reconcile_after_restart(&c, 10 * round, TABLE, &policy).unwrap();
            let expected = if round < 3 {
                ReconcileOutcome::Requeued
            } else {
                ReconcileOutcome::FailedMaxAttempts
            };
            assert_eq!(rep.entries.len(), 1);
            assert_eq!(rep.entries[0].outcome, expected, "round {round}");
        }
        let v = get(&c, id).unwrap().unwrap();
        assert_eq!(v.state, JobState::Failed);
        assert_eq!(v.attempts, 3);
        assert_eq!(
            v.last_error_code.as_deref(),
            Some(CODE_RESTART_MAX_ATTEMPTS)
        );
        assert!(claim_due(&c, i64::MAX, 10).unwrap().claimed.is_empty());
    }

    #[test]
    fn reconcile_with_clock_behind_the_stored_due_time_still_runs_at_once() {
        let c = mem();
        let id = created(&c, "rerun", "skew", 5_000, 0);
        claim_due(&c, 5_000, 1).unwrap();
        // restart with a clock that went backwards
        reconcile_after_restart(&c, 100, TABLE, &D).unwrap();
        assert_eq!(ids(&claim_due(&c, 100, 1).unwrap()), vec![id]);
    }

    #[test]
    fn reconcile_report_and_debug_never_contain_payload_text() {
        let c = mem();
        for (i, kind) in ["rerun", "once", "mystery"].iter().enumerate() {
            let key = format!("leak{i}");
            schedule(
                &c,
                &NewJob {
                    kind,
                    dedupe_key: &key,
                    payload_json: "{\"secret_marker_zz\":\"user text\"}",
                    due_at: 0,
                },
                0,
            )
            .unwrap();
        }
        let b = claim_due(&c, 1, 10).unwrap();
        let rep = reconcile_after_restart(&c, 2, TABLE, &D).unwrap();
        let mut text = format!("{rep:?} {b:?}");
        for e in &rep.entries {
            text.push_str(&format!("{:?}", get(&c, e.id).unwrap()));
        }
        assert!(!text.contains("secret_marker_zz"), "{text}");
        assert!(!text.contains("user text"), "{text}");
    }

    // ---- durability across a restart ----

    #[test]
    fn scheduled_work_survives_reopening_the_database_file() {
        let dir = scratch("durable");
        let p = dir.join("core.sqlite");
        let ps = p.to_str().unwrap();
        let (run, once, later) = {
            let c = open_file(ps);
            let run = created(&c, "rerun", "run", 0, 0);
            let once = created(&c, "once", "once", 0, 0);
            let later = created(&c, "rerun", "later", 9_000, 0);
            assert_eq!(claim_due(&c, 10, 10).unwrap().claimed.len(), 2);
            (run, once, later)
        }; // connection dropped: the process "crashed" with two jobs running
        let c = open_file(ps);
        let rep = reconcile_after_restart(&c, 20, TABLE, &D).unwrap();
        assert_eq!(rep.entries.len(), 2);
        assert_eq!(state_of(&c, once), JobState::Abandoned);
        assert_eq!(ids(&claim_due(&c, 20, 10).unwrap()), vec![run]);
        assert_eq!(state_of(&c, later), JobState::Pending);
        assert_eq!(ids(&claim_due(&c, 9_000, 10).unwrap()), vec![later]);
        // the dedupe key survived the restart too
        assert!(matches!(
            schedule(&c, &nj("rerun", "once", 0), 21).unwrap(),
            ScheduleOutcome::Duplicate {
                state: JobState::Abandoned,
                ..
            }
        ));
    }

    // ---- injected clock ----

    #[test]
    fn results_depend_only_on_injected_time() {
        for base in [0i64, 1_700_000_000, 4_000_000_000] {
            let c = mem();
            let id = created(&c, "rerun", "t", base + 10, base);
            assert_eq!(get(&c, id).unwrap().unwrap().updated_at, base);
            assert!(claim_due(&c, base + 9, 1).unwrap().claimed.is_empty());
            assert_eq!(ids(&claim_due(&c, base + 10, 1).unwrap()), vec![id]);
            assert_eq!(get(&c, id).unwrap().unwrap().updated_at, base + 10);
            match fail(&c, id, "e", base + 11, &D).unwrap() {
                FailOutcome::Retry { due_at, .. } => assert_eq!(due_at, base + 11 + 30),
                o => panic!("{o:?}"),
            }
            assert_eq!(get(&c, id).unwrap().unwrap().updated_at, base + 11);
        }
    }

    // ---- error codes ----

    #[test]
    fn error_codes_are_distinct_stable_and_carry_no_sql_text() {
        let all = [
            QueueError::Sqlite,
            QueueError::Storage(StorageError::Sqlite),
            QueueError::InvalidKind,
            QueueError::InvalidDedupeKey,
            QueueError::PayloadTooLarge,
            QueueError::InvalidTime,
            QueueError::InvalidPolicy,
            QueueError::NotFound,
            QueueError::NotRunning,
        ];
        let mut codes: Vec<_> = all.iter().map(|e| e.code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        assert_eq!(QueueError::NotRunning.code(), "not_running");
        assert_eq!(QueueError::Sqlite.code(), "sqlite_error");
        assert_eq!(QueueError::PayloadTooLarge.to_string(), "payload_too_large");
        // a real engine error (table missing) is reduced to the stable code
        let bare = storage::open(":memory:", &[]).unwrap();
        let e = schedule(&bare, &nj("rerun", "x", 0), 0).unwrap_err();
        assert_eq!(e, QueueError::Sqlite);
        let text = format!("{e:?} {e}");
        for needle in ["jobs", "no such", "INSERT", "table"] {
            assert!(!text.contains(needle), "{text}");
        }
    }

    // ---- liveness ----

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

    #[test]
    fn seeded_random_sequences_always_end_terminal_with_every_abandonment_reported() {
        let policy = RetryPolicy {
            base_secs: 5,
            cap_secs: 40,
            max_attempts: 4,
        };
        for seed in 1..=10u64 {
            let dir = scratch(&format!("liveness{seed}"));
            let path = dir.join("core.sqlite");
            let ps = path.to_str().unwrap().to_string();
            let mut conn = open_file(&ps);
            let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut now: i64 = 1_000;
            let kinds = ["rerun", "once", "mystery"];
            // model runtime: what the "worker" believes it is running
            let mut in_flight: Vec<i64> = Vec::new();
            let mut all_ids: Vec<i64> = Vec::new();
            let mut kind_of: HashMap<i64, &str> = HashMap::new();
            let mut reported_abandoned: HashSet<i64> = HashSet::new();
            let mut crashed_no_rerun: HashSet<i64> = HashSet::new();
            let mut terminal_seen: HashMap<i64, JobState> = HashMap::new();

            let check_terminal_stable =
                |conn: &Connection, seen: &mut HashMap<i64, JobState>, ids: &[i64]| {
                    for id in ids {
                        let st = state_of(conn, *id);
                        if let Some(prev) = seen.get(id) {
                            assert_eq!(*prev, st, "terminal job {id} changed state");
                        } else if st.is_terminal() {
                            seen.insert(*id, st);
                        }
                    }
                };

            for _step in 0..250 {
                match rng.below(10) {
                    0..=2 => {
                        // schedule; a small key space forces duplicates
                        let key_n = rng.below(40);
                        let kind = kinds[rng.below(3) as usize];
                        let key = format!("k{key_n}");
                        let payload = if rng.below(8) == 0 {
                            "{broken"
                        } else {
                            "{\"id\":\"7\"}"
                        };
                        let due = now + rng.below(60) as i64 - 20;
                        let out = schedule(
                            &conn,
                            &NewJob {
                                kind,
                                dedupe_key: &key,
                                payload_json: payload,
                                due_at: due.max(0),
                            },
                            now,
                        )
                        .unwrap();
                        if let ScheduleOutcome::Created { id } = out {
                            assert!(!all_ids.contains(&id));
                            all_ids.push(id);
                            kind_of.insert(id, kind);
                        }
                    }
                    3 => now += rng.below(30) as i64,
                    4 | 5 => {
                        let limit = 1 + rng.below(4) as u32;
                        let b = claim_due(&conn, now, limit).unwrap();
                        for j in &b.claimed {
                            assert!(!in_flight.contains(&j.id), "double dispatch of {}", j.id);
                            assert!(
                                !crashed_no_rerun.contains(&j.id),
                                "at-most-once job {} ran again after a crash",
                                j.id
                            );
                            in_flight.push(j.id);
                        }
                        for id in &b.payload_failed {
                            assert_eq!(state_of(&conn, *id), JobState::Failed);
                        }
                    }
                    6 | 7 if !in_flight.is_empty() => {
                        let i = rng.below(in_flight.len() as u64) as usize;
                        let id = in_flight.swap_remove(i);
                        complete(&conn, id, now).unwrap();
                        assert_eq!(state_of(&conn, id), JobState::Done);
                    }
                    8 if !in_flight.is_empty() => {
                        let i = rng.below(in_flight.len() as u64) as usize;
                        let id = in_flight.swap_remove(i);
                        let before = get(&conn, id).unwrap().unwrap().attempts;
                        let out = fail(&conn, id, "worker_error", now, &policy).unwrap();
                        match out {
                            FailOutcome::Retry { due_at, attempts } => {
                                assert!(attempts < policy.max_attempts as i64);
                                assert_eq!(attempts, before);
                                assert!(due_at > now && due_at - now <= policy.cap_secs);
                            }
                            FailOutcome::Failed { attempts } => {
                                assert_eq!(attempts, policy.max_attempts as i64);
                            }
                        }
                    }
                    9 => {
                        // crash: the worker forgets everything in flight; the db
                        // file is reopened and reconciled
                        let lost: Vec<i64> = std::mem::take(&mut in_flight);
                        drop(conn);
                        conn = open_file(&ps);
                        let before: HashMap<i64, i64> = lost
                            .iter()
                            .map(|id| (*id, get(&conn, *id).unwrap().unwrap().attempts))
                            .collect();
                        let rep = reconcile_after_restart(&conn, now, TABLE, &policy).unwrap();
                        let reported: HashMap<i64, ReconcileOutcome> =
                            rep.entries.iter().map(|e| (e.id, e.outcome)).collect();
                        assert_eq!(reported.len(), lost.len(), "{rep:?} vs {lost:?}");
                        for id in lost {
                            let out = reported[&id];
                            match (kind_of[&id], out) {
                                ("rerun", ReconcileOutcome::Requeued) => {
                                    assert_eq!(state_of(&conn, id), JobState::Pending);
                                    assert!(before[&id] < policy.max_attempts as i64);
                                }
                                ("rerun", ReconcileOutcome::FailedMaxAttempts) => {
                                    assert_eq!(state_of(&conn, id), JobState::Failed);
                                    assert!(before[&id] >= policy.max_attempts as i64);
                                }
                                ("once", ReconcileOutcome::AbandonedAtMostOnce)
                                | ("mystery", ReconcileOutcome::AbandonedUnregistered) => {
                                    assert_eq!(state_of(&conn, id), JobState::Abandoned);
                                    reported_abandoned.insert(id);
                                    crashed_no_rerun.insert(id);
                                }
                                other => panic!("unexpected reconcile result {other:?}"),
                            }
                        }
                    }
                    _ => now += 1,
                }
                check_terminal_stable(&conn, &mut terminal_seen, &all_ids);
            }

            // Drain phase: work the queue to completion. In-flight work from the
            // random phase is finished by the worker (no crash), then everything
            // else is claimed as it becomes due.
            for id in std::mem::take(&mut in_flight) {
                complete(&conn, id, now).unwrap();
            }
            let mut rounds = 0;
            loop {
                let open: Vec<i64> = all_ids
                    .iter()
                    .copied()
                    .filter(|id| !state_of(&conn, *id).is_terminal())
                    .collect();
                if open.is_empty() {
                    break;
                }
                rounds += 1;
                assert!(
                    rounds <= 20,
                    "seed {seed}: still open after drain: {open:?}"
                );
                now += policy.cap_secs + 1;
                let b = claim_due(&conn, now, 1_000).unwrap();
                assert!(
                    !b.claimed.is_empty() || !b.payload_failed.is_empty(),
                    "seed {seed}: queue went quiet with open work {open:?}"
                );
                for (n, j) in b.claimed.iter().enumerate() {
                    assert!(!crashed_no_rerun.contains(&j.id));
                    // alternate success and failure so retries and exhaustion both occur
                    if (n + rounds) % 2 == 0 {
                        complete(&conn, j.id, now).unwrap();
                    } else {
                        fail(&conn, j.id, "worker_error", now, &policy).unwrap();
                    }
                }
                check_terminal_stable(&conn, &mut terminal_seen, &all_ids);
            }
            for id in &all_ids {
                let st = state_of(&conn, *id);
                assert!(st.is_terminal(), "job {id} left {st:?}");
                assert_ne!(st, JobState::Running);
                if st == JobState::Abandoned {
                    assert!(
                        reported_abandoned.contains(id),
                        "abandonment {id} unreported"
                    );
                }
            }
            assert!(!all_ids.is_empty(), "seed {seed} scheduled nothing");
        }
    }
}

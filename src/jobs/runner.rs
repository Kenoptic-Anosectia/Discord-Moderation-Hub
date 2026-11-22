//! Async job runner on top of [`super::queue`] (`docs/verification/JOB-RUNNER.md`).
//!
//! Executes the durable jobs of the queue: reconcile once at start, then claim
//! due jobs up to a concurrency limit, run each kind's handler as its own tokio
//! task, and acknowledge the result with `complete` or `fail`. The synchronous
//! queue is bridged into tokio through `tokio_rusqlite::Connection::call`, so
//! every SQL statement runs on the connection's own thread and never on the
//! executor. Time and sleeping come from an injected [`Clock`]; a [`WakeHandle`]
//! wakes the loop when something is scheduled and a [`ShutdownHandle`] stops it.
//!
//! Errors, summaries and handler failures carry stable codes and job ids only:
//! a handler reports failure with a `&'static str` code ([`HandlerError`]), so
//! no runtime text (payload, user content, SQL) can ride along.
//!
//! Unwired building block: no production caller exists yet (wiring card W1).

use super::queue::{
    self, ClaimedJob, FailOutcome, QueueError, Report, RestartPolicy, RetryPolicy, MAX_KIND_LEN,
};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Notify};
use tokio::task::JoinSet;

/// Hard maximum for [`RunnerConfig::concurrency`].
pub const MAX_CONCURRENCY: usize = 16;
/// Smallest and largest accepted [`RunnerConfig::max_poll`].
pub const MIN_POLL: Duration = Duration::from_secs(1);
pub const MAX_POLL: Duration = Duration::from_secs(3600);
/// Smallest and largest accepted [`RunnerConfig::shutdown_grace`]. The floor is
/// non-zero so in-flight handlers always get a real chance to finish; the cap
/// is the bound on how long a shutdown can take.
pub const MIN_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
pub const MAX_SHUTDOWN_GRACE: Duration = Duration::from_secs(300);
/// Smallest and largest accepted [`RunnerConfig::handler_timeout`].
pub const MIN_HANDLER_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_HANDLER_TIMEOUT: Duration = Duration::from_secs(3600);

/// Codes the runner itself writes as a job's `last_error_code`.
pub const CODE_NO_HANDLER: &str = "no_handler";
pub const CODE_PANICKED: &str = "handler_panicked";
pub const CODE_CANCELLED: &str = "handler_cancelled";
pub const CODE_TIMED_OUT: &str = "handler_timed_out";

/// Wait before retrying after a database error (claim, acknowledge or the
/// next-due lookup failed): the loop sleeps this long before trying again.
const DB_RETRY_DELAY: Duration = Duration::from_secs(1);

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Injected time source. `now` is unix seconds (the unit the queue uses).
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> i64;
    fn sleep(&self, d: Duration) -> BoxFuture<'_, ()>;
    /// Sleep until `deadline` (unix seconds); ready at once if it has passed.
    /// An absolute deadline, not a duration, so time that passes between the
    /// caller reading `now` and calling this is not added to the wait.
    fn sleep_until(&self, deadline: i64) -> BoxFuture<'_, ()>;
}

/// The real clock. The only place in this module that reads wall-clock time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }

    fn sleep(&self, d: Duration) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(d))
    }

    fn sleep_until(&self, deadline: i64) -> BoxFuture<'_, ()> {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let target = Duration::from_secs(u64::try_from(deadline).unwrap_or(0));
        Box::pin(tokio::time::sleep(target.saturating_sub(since_epoch)))
    }
}

/// Stable runner error codes. Never carries text, SQL or ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerError {
    InvalidConcurrency,
    InvalidPoll,
    InvalidShutdownGrace,
    InvalidHandlerTimeout,
    InvalidRetryPolicy,
    InvalidKind,
    DuplicateKind,
    /// A queue call failed (see the inner stable code).
    Queue(QueueError),
    /// The database connection thread is gone; the runner cannot continue.
    DbClosed,
}

impl RunnerError {
    pub fn code(&self) -> &'static str {
        match self {
            RunnerError::InvalidConcurrency => "invalid_concurrency",
            RunnerError::InvalidPoll => "invalid_poll",
            RunnerError::InvalidShutdownGrace => "invalid_shutdown_grace",
            RunnerError::InvalidHandlerTimeout => "invalid_handler_timeout",
            RunnerError::InvalidRetryPolicy => "invalid_retry_policy",
            RunnerError::InvalidKind => "invalid_kind",
            RunnerError::DuplicateKind => "duplicate_kind",
            RunnerError::Queue(q) => q.code(),
            RunnerError::DbClosed => "db_closed",
        }
    }
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for RunnerError {}

/// A handler's failure: a stable code and nothing else. `&'static str` means a
/// handler cannot put runtime text (payload, user content) into it. A code that
/// is not `[a-z0-9_]{1,48}` is stored by the queue as `invalid_error_code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandlerError {
    code: &'static str,
}

impl HandlerError {
    pub const fn new(code: &'static str) -> Self {
        HandlerError { code }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

pub type HandlerResult = Result<(), HandlerError>;
type Handler = Arc<dyn Fn(ClaimedJob) -> BoxFuture<'static, HandlerResult> + Send + Sync>;

struct Entry {
    policy: RestartPolicy,
    handler: Handler,
}

/// Kind name -> (restart policy, async handler). The same table feeds
/// `reconcile_after_restart`, so a kind's policy has one source of truth.
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.entries.keys()).finish()
    }
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    /// Register `handler` for `kind` (`[a-z0-9_]{1,64}`, once). The handler
    /// returns `Ok(())` when its effect is done, or `Err` ONLY when the effect
    /// did not happen and running it again is acceptable: an `Err` is retried
    /// by the queue's retry policy whatever the restart policy says.
    pub fn register<F, Fut>(
        &mut self,
        kind: &str,
        policy: RestartPolicy,
        handler: F,
    ) -> Result<(), RunnerError>
    where
        F: Fn(ClaimedJob) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult> + Send + 'static,
    {
        let plain = !kind.is_empty()
            && kind.len() <= MAX_KIND_LEN
            && kind
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !plain {
            return Err(RunnerError::InvalidKind);
        }
        if self.entries.contains_key(kind) {
            return Err(RunnerError::DuplicateKind);
        }
        let handler: Handler = Arc::new(move |job| Box::pin(handler(job)));
        self.entries
            .insert(kind.to_string(), Entry { policy, handler });
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Owned `(kind, policy)` pairs for `reconcile_after_restart`.
    fn kind_table(&self) -> Vec<(String, RestartPolicy)> {
        self.entries
            .iter()
            .map(|(k, e)| (k.clone(), e.policy))
            .collect()
    }
}

/// Constructor parameters of [`Runner::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerConfig {
    /// Handlers running at once: `1..=`[`MAX_CONCURRENCY`].
    pub concurrency: usize,
    /// Longest sleep with nothing due: [`MIN_POLL`]..=[`MAX_POLL`].
    pub max_poll: Duration,
    /// How long shutdown waits for in-flight handlers before abandoning them.
    pub shutdown_grace: Duration,
    /// Longest one handler may run before it is aborted and treated as a
    /// cancel: [`MIN_HANDLER_TIMEOUT`]..=[`MAX_HANDLER_TIMEOUT`].
    pub handler_timeout: Duration,
    pub retry: RetryPolicy,
}

impl RunnerConfig {
    pub const DEFAULT: RunnerConfig = RunnerConfig {
        concurrency: 4,
        max_poll: Duration::from_secs(30),
        shutdown_grace: Duration::from_secs(10),
        handler_timeout: Duration::from_secs(60),
        retry: RetryPolicy::DEFAULT,
    };

    pub fn validate(&self) -> Result<(), RunnerError> {
        if !(1..=MAX_CONCURRENCY).contains(&self.concurrency) {
            return Err(RunnerError::InvalidConcurrency);
        }
        if !(MIN_POLL..=MAX_POLL).contains(&self.max_poll) {
            return Err(RunnerError::InvalidPoll);
        }
        if !(MIN_SHUTDOWN_GRACE..=MAX_SHUTDOWN_GRACE).contains(&self.shutdown_grace) {
            return Err(RunnerError::InvalidShutdownGrace);
        }
        if !(MIN_HANDLER_TIMEOUT..=MAX_HANDLER_TIMEOUT).contains(&self.handler_timeout) {
            return Err(RunnerError::InvalidHandlerTimeout);
        }
        self.retry
            .validate()
            .map_err(|_| RunnerError::InvalidRetryPolicy)
    }
}

/// Cloneable handle that wakes the runner; call it after scheduling a job.
/// A wake that arrives while the runner is busy is remembered (one permit).
#[derive(Debug, Clone, Default)]
pub struct WakeHandle(Arc<Notify>);

impl WakeHandle {
    pub fn wake(&self) {
        self.0.notify_one();
    }
}

/// Cloneable handle that asks the runner to stop.
#[derive(Debug, Clone)]
pub struct ShutdownHandle(Arc<watch::Sender<bool>>);

impl ShutdownHandle {
    pub fn trigger(&self) {
        self.0.send_replace(true);
    }
}

/// What a finished [`Started::run`] did. Counters and job ids only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub claimed: u64,
    pub completed: u64,
    /// Handler failures put back to `pending` with backoff.
    pub retried: u64,
    /// Jobs made terminal `failed` by an acknowledgement (retries exhausted,
    /// no handler, or an at-most-once panic).
    pub failed: u64,
    /// Jobs the queue refused at claim (malformed payload or kind).
    pub refused_at_claim: u64,
    /// Database calls that failed and were retried or dropped.
    pub db_errors: u64,
    /// Acknowledgements the queue refused because the job was no longer
    /// `running` (changed behind the runner's back).
    pub refused_acks: u64,
    /// Jobs still `running` in the database at return (in-flight past the
    /// grace period, or an acknowledgement that could not be written), sorted.
    pub left_running: Vec<i64>,
}

pub struct Runner<C: Clock> {
    db: tokio_rusqlite::Connection,
    clock: Arc<C>,
    registry: Arc<Registry>,
    cfg: RunnerConfig,
    wake: WakeHandle,
    shutdown: ShutdownHandle,
}

/// A runner whose startup reconcile has completed; the only way to `run`.
pub struct Started<C: Clock> {
    runner: Runner<C>,
    report: Report,
}

impl<C: Clock> Runner<C> {
    pub fn new(
        db: tokio_rusqlite::Connection,
        clock: Arc<C>,
        registry: Registry,
        cfg: RunnerConfig,
    ) -> Result<Self, RunnerError> {
        cfg.validate()?;
        Ok(Runner {
            db,
            clock,
            registry: Arc::new(registry),
            cfg,
            wake: WakeHandle::default(),
            shutdown: ShutdownHandle(Arc::new(watch::channel(false).0)),
        })
    }

    pub fn wake_handle(&self) -> WakeHandle {
        self.wake.clone()
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// Reconcile jobs found `running` (a previous process died or was stopped
    /// mid-run) using the registry's policies, BEFORE anything is claimed.
    pub async fn start(self) -> Result<Started<C>, RunnerError> {
        let table = self.registry.kind_table();
        let retry = self.cfg.retry;
        let now = self.clock.now();
        let report = self
            .db
            .call(move |c| {
                let table: Vec<(&str, RestartPolicy)> =
                    table.iter().map(|(k, p)| (k.as_str(), *p)).collect();
                queue::reconcile_after_restart(c, now, &table, &retry)
            })
            .await
            .map_err(map_db)?;
        Ok(Started {
            runner: self,
            report,
        })
    }
}

fn map_db(e: tokio_rusqlite::Error<QueueError>) -> RunnerError {
    match e {
        tokio_rusqlite::Error::Error(q) => RunnerError::Queue(q),
        _ => RunnerError::DbClosed,
    }
}

/// Retry policy that makes the first `fail` terminal: used where running the
/// job again cannot help (no handler) or may repeat an effect (at-most-once
/// panic).
const TERMINAL: RetryPolicy = RetryPolicy {
    base_secs: 1,
    cap_secs: 1,
    max_attempts: 1,
};

/// A handler result waiting to be written to the queue.
#[derive(Debug, Clone, Copy)]
enum Ack {
    Complete {
        id: i64,
    },
    Fail {
        id: i64,
        code: &'static str,
        terminal: bool,
    },
}

impl Ack {
    fn id(&self) -> i64 {
        match self {
            Ack::Complete { id } | Ack::Fail { id, .. } => *id,
        }
    }
}

enum AckDone {
    Completed,
    Retried,
    Failed,
}

enum AckError {
    /// Connection thread gone.
    Closed,
    /// Engine error: keep the acknowledgement and try again.
    Transient,
    /// The queue says the job is not `running` (or unknown): nothing to write.
    Refused,
}

type JoinResult = Result<(tokio::task::Id, TaskEnd), tokio::task::JoinError>;

/// How a handler task ended (apart from a panic, which `JoinSet` reports).
enum TaskEnd {
    /// The handler returned.
    Returned(HandlerResult),
    /// The handler ran past `handler_timeout` and its future was dropped.
    TimedOut,
}

/// Mutable state of one `run`.
struct Live {
    set: JoinSet<TaskEnd>,
    /// task id -> (job id, restart policy of its kind)
    tasks: HashMap<tokio::task::Id, (i64, RestartPolicy)>,
    acks: Vec<Ack>,
    sum: RunSummary,
}

impl Live {
    /// Turn a finished handler task into an acknowledgement to write.
    fn finish(&mut self, res: JoinResult) {
        let ack = match res {
            Ok((tid, end)) => {
                let Some((id, policy)) = self.tasks.remove(&tid) else {
                    return;
                };
                match end {
                    TaskEnd::Returned(Ok(())) => Ack::Complete { id },
                    TaskEnd::Returned(Err(e)) => Ack::Fail {
                        id,
                        code: e.code(),
                        terminal: false,
                    },
                    // Treated as a cancel: the effect may have happened
                    // before the handler hung, so at-most-once is terminal.
                    TaskEnd::TimedOut => Ack::Fail {
                        id,
                        code: CODE_TIMED_OUT,
                        terminal: policy == RestartPolicy::AtMostOnce,
                    },
                }
            }
            Err(join_err) => {
                let Some((id, policy)) = self.tasks.remove(&join_err.id()) else {
                    return;
                };
                let code = if join_err.is_panic() {
                    CODE_PANICKED
                } else {
                    CODE_CANCELLED
                };
                // A panic or cancel may have come after the effect: an
                // at-most-once job must not be run again.
                Ack::Fail {
                    id,
                    code,
                    terminal: policy == RestartPolicy::AtMostOnce,
                }
            }
        };
        self.acks.push(ack);
    }

    fn reap_finished(&mut self) {
        while let Some(r) = self.set.try_join_next_with_id() {
            self.finish(r);
        }
    }
}

impl<C: Clock> Runner<C> {
    async fn claim(&self, now: i64, limit: u32) -> Result<queue::ClaimBatch, RunnerError> {
        self.db
            .call(move |c| queue::claim_due(c, now, limit))
            .await
            .map_err(map_db)
    }

    /// Earliest `due_at` among pending jobs. The queue module has no such
    /// call, so this is the one read-only statement the runner issues itself.
    async fn next_due(&self) -> Result<Option<i64>, RunnerError> {
        self.db
            .call(|c| {
                c.query_row(
                    "SELECT MIN(due_at) FROM jobs WHERE state = 'pending'",
                    [],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .map_err(QueueError::from)
            })
            .await
            .map_err(map_db)
    }

    async fn write_ack(&self, ack: Ack) -> Result<AckDone, AckError> {
        let now = self.clock.now();
        let retry = match ack {
            Ack::Fail { terminal: true, .. } => TERMINAL,
            _ => self.cfg.retry,
        };
        let res = self
            .db
            .call(move |c| match ack {
                Ack::Complete { id } => queue::complete(c, id, now).map(|()| AckDone::Completed),
                Ack::Fail { id, code, .. } => {
                    queue::fail(c, id, code, now, &retry).map(|o| match o {
                        FailOutcome::Retry { .. } => AckDone::Retried,
                        FailOutcome::Failed { .. } => AckDone::Failed,
                    })
                }
            })
            .await;
        match res {
            Ok(done) => Ok(done),
            Err(tokio_rusqlite::Error::Error(QueueError::Sqlite | QueueError::Storage(_))) => {
                Err(AckError::Transient)
            }
            Err(tokio_rusqlite::Error::Error(_)) => Err(AckError::Refused),
            Err(_) => Err(AckError::Closed),
        }
    }

    /// Write every pending acknowledgement once. Returns `true` when some
    /// could not be written (they stay queued for the next pass).
    async fn flush_acks(&self, live: &mut Live) -> Result<bool, RunnerError> {
        let pending = std::mem::take(&mut live.acks);
        let mut left = false;
        let mut it = pending.into_iter();
        while let Some(ack) = it.next() {
            match self.write_ack(ack).await {
                Ok(AckDone::Completed) => live.sum.completed += 1,
                Ok(AckDone::Retried) => live.sum.retried += 1,
                Ok(AckDone::Failed) => live.sum.failed += 1,
                Err(AckError::Refused) => live.sum.refused_acks += 1,
                Err(AckError::Transient) => {
                    live.sum.db_errors += 1;
                    live.acks.push(ack);
                    left = true;
                }
                Err(AckError::Closed) => {
                    live.acks.push(ack);
                    live.acks.extend(it);
                    return Err(RunnerError::DbClosed);
                }
            }
        }
        Ok(left)
    }

    /// Run the handler for `job` as its own task, or queue the terminal
    /// failure for a kind with no handler.
    fn dispatch(&self, live: &mut Live, job: ClaimedJob) {
        live.sum.claimed += 1;
        let id = job.id;
        let Some(entry) = self.registry.entries.get(&job.kind) else {
            live.acks.push(Ack::Fail {
                id,
                code: CODE_NO_HANDLER,
                terminal: true,
            });
            return;
        };
        let handler = entry.handler.clone();
        let clock = self.clock.clone();
        let timeout = self.cfg.handler_timeout;
        // The handler is called inside the task, so even a handler function
        // that panics before returning its future cannot reach the loop. The
        // injected clock bounds the run: when its timer fires first, the
        // handler future is dropped (at its next `.await`; code that never
        // awaits cannot be stopped) and the task reports `TimedOut`.
        let handle = live.set.spawn(async move {
            let run = (*handler)(job);
            tokio::select! {
                biased;
                r = run => TaskEnd::Returned(r),
                () = clock.sleep(timeout) => TaskEnd::TimedOut,
            }
        });
        live.tasks.insert(handle.id(), (id, entry.policy));
    }

    /// Deadline (unix seconds) to sleep until when nothing can be claimed now.
    async fn idle_deadline(&self, now: i64, live: &mut Live) -> Result<i64, RunnerError> {
        let poll = i64::try_from(self.cfg.max_poll.as_secs()).unwrap_or(i64::MAX);
        let latest = now.saturating_add(poll);
        match self.next_due().await {
            Ok(None) => Ok(latest),
            // At least 1 s ahead: a due job that could not be claimed (another
            // claimer won it) must not turn into a spin.
            Ok(Some(due)) => Ok(due.max(now.saturating_add(1)).min(latest)),
            Err(RunnerError::DbClosed) => Err(RunnerError::DbClosed),
            Err(_) => {
                live.sum.db_errors += 1;
                Ok(now.saturating_add(1))
            }
        }
    }

    async fn main_loop(
        &self,
        live: &mut Live,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<(), RunnerError> {
        let slots = self.cfg.concurrency;
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            live.reap_finished();
            let mut trouble = self.flush_acks(live).await?;
            let now = self.clock.now();
            let free = slots.saturating_sub(live.set.len());
            if free > 0 {
                let limit = u32::try_from(free).unwrap_or(u32::MAX);
                match self.claim(now, limit).await {
                    Ok(batch) => {
                        live.sum.refused_at_claim += batch.payload_failed.len() as u64;
                        let progressed =
                            !batch.claimed.is_empty() || !batch.payload_failed.is_empty();
                        for job in batch.claimed {
                            self.dispatch(live, job);
                        }
                        if progressed {
                            // More may be due; loop without sleeping. Each
                            // such pass changed at least one row, so it ends.
                            continue;
                        }
                    }
                    Err(RunnerError::DbClosed) => return Err(RunnerError::DbClosed),
                    Err(_) => {
                        live.sum.db_errors += 1;
                        trouble = true;
                    }
                }
            }
            let free = slots.saturating_sub(live.set.len());
            let sleeper: Option<BoxFuture<'_, ()>> = if trouble {
                Some(self.clock.sleep(DB_RETRY_DELAY))
            } else if free > 0 {
                Some(self.clock.sleep_until(self.idle_deadline(now, live).await?))
            } else {
                // Every slot is busy: only a finishing handler (or shutdown)
                // can change anything, so no timer is needed.
                None
            };
            let mut joined = None;
            tokio::select! {
                biased;
                _ = stop.changed() => {}
                r = live.set.join_next_with_id(), if !live.set.is_empty() => { joined = r; }
                _ = self.wake.0.notified() => {}
                _ = async {
                    match sleeper {
                        Some(s) => s.await,
                        None => std::future::pending().await,
                    }
                } => {}
            }
            if let Some(r) = joined {
                live.finish(r);
            }
        }
    }

    /// Shutdown: claim nothing, give in-flight handlers `shutdown_grace`,
    /// write their results, abandon the rest (their jobs stay `running`).
    async fn drain(&self, live: &mut Live) {
        if !live.set.is_empty() {
            let mut grace = self.clock.sleep(self.cfg.shutdown_grace);
            while !live.set.is_empty() {
                let mut joined = None;
                let mut timed_out = false;
                tokio::select! {
                    biased;
                    r = live.set.join_next_with_id() => { joined = r; }
                    _ = &mut grace => { timed_out = true; }
                }
                if let Some(r) = joined {
                    live.finish(r);
                    if self.flush_acks(live).await.is_err() {
                        break;
                    }
                }
                if timed_out {
                    live.reap_finished();
                    break;
                }
            }
        }
        let _ = self.flush_acks(live).await;
        live.set.abort_all();
    }
}

impl<C: Clock> Started<C> {
    /// What the startup reconcile did (abandoned jobs must be surfaced).
    pub fn report(&self) -> &Report {
        &self.report
    }

    /// Run until shutdown is triggered (or the database goes away).
    pub async fn run(self) -> Result<RunSummary, RunnerError> {
        let runner = self.runner;
        let mut live = Live {
            set: JoinSet::new(),
            tasks: HashMap::new(),
            acks: Vec::new(),
            sum: RunSummary::default(),
        };
        let mut stop = runner.shutdown.0.subscribe();
        runner.main_loop(&mut live, &mut stop).await?;
        runner.drain(&mut live).await;
        let mut left: Vec<i64> = live
            .tasks
            .values()
            .map(|(id, _)| *id)
            .chain(live.acks.iter().map(Ack::id))
            .collect();
        left.sort_unstable();
        left.dedup();
        live.sum.left_running = left;
        Ok(live.sum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::queue::{JobState, JobView, NewJob, ReconcileOutcome, ScheduleOutcome};
    use crate::storage;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering::SeqCst};
    use std::sync::Mutex;

    /// Synthetic start of the fake clock (unix seconds).
    const T0: i64 = 1_000_000;

    // ---- fake clock: time only moves when a test advances it ----

    struct FakeClock {
        ms: watch::Sender<i64>,
        now_calls: AtomicUsize,
        sleep_calls: AtomicUsize,
        last_sleep_ms: AtomicI64,
        last_deadline: AtomicI64,
    }

    impl FakeClock {
        fn new() -> Arc<FakeClock> {
            Arc::new(FakeClock {
                ms: watch::channel(T0 * 1000).0,
                now_calls: AtomicUsize::new(0),
                sleep_calls: AtomicUsize::new(0),
                last_sleep_ms: AtomicI64::new(-1),
                last_deadline: AtomicI64::new(-1),
            })
        }
        fn advance(&self, secs: i64) {
            self.ms.send_modify(|v| *v += secs * 1000);
        }
        fn secs(&self) -> i64 {
            *self.ms.borrow() / 1000
        }
        fn sleeps(&self) -> usize {
            self.sleep_calls.load(SeqCst)
        }
        fn nows(&self) -> usize {
            self.now_calls.load(SeqCst)
        }
        fn last_sleep_secs(&self) -> i64 {
            self.last_sleep_ms.load(SeqCst) / 1000
        }
        fn last_deadline(&self) -> i64 {
            self.last_deadline.load(SeqCst)
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
            self.now_calls.fetch_add(1, SeqCst);
            *self.ms.borrow() / 1000
        }
        fn sleep(&self, d: Duration) -> BoxFuture<'_, ()> {
            self.sleep_calls.fetch_add(1, SeqCst);
            let d_ms = i64::try_from(d.as_millis()).unwrap();
            self.last_sleep_ms.store(d_ms, SeqCst);
            let target = *self.ms.borrow() + d_ms;
            self.wait_until_ms(target)
        }
        fn sleep_until(&self, deadline: i64) -> BoxFuture<'_, ()> {
            self.sleep_calls.fetch_add(1, SeqCst);
            self.last_deadline.store(deadline, SeqCst);
            self.wait_until_ms(deadline * 1000)
        }
    }

    // ---- helpers ----

    fn cfg(n: usize) -> RunnerConfig {
        RunnerConfig {
            concurrency: n,
            max_poll: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(5),
            handler_timeout: Duration::from_secs(600),
            retry: RetryPolicy {
                base_secs: 30,
                cap_secs: 600,
                max_attempts: 3,
            },
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("kj2-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mem_db() -> tokio_rusqlite::Connection {
        let mut c = storage::open(":memory:", &[]).unwrap();
        queue::init(&mut c).unwrap();
        c.into()
    }

    fn file_db(path: &str) -> tokio_rusqlite::Connection {
        let mut c = storage::open(path, &[]).unwrap();
        queue::init(&mut c).unwrap();
        c.into()
    }

    async fn sched(
        db: &tokio_rusqlite::Connection,
        kind: &str,
        key: &str,
        payload: &str,
        due: i64,
    ) -> i64 {
        let (kind, key, payload) = (kind.to_string(), key.to_string(), payload.to_string());
        let out = db
            .call(move |c| {
                queue::schedule(
                    c,
                    &NewJob {
                        kind: &kind,
                        dedupe_key: &key,
                        payload_json: &payload,
                        due_at: due,
                    },
                    T0,
                )
            })
            .await
            .unwrap();
        match out {
            ScheduleOutcome::Created { id } => id,
            other => panic!("expected Created, got {other:?}"),
        }
    }

    async fn view(db: &tokio_rusqlite::Connection, id: i64) -> JobView {
        db.call(move |c| queue::get(c, id)).await.unwrap().unwrap()
    }

    async fn sql(db: &tokio_rusqlite::Connection, stmt: &'static str) {
        db.call(move |c| c.execute_batch(stmt)).await.unwrap();
    }

    /// Wait (real time, 1 ms polls, 10 s deadline) until `f` is true.
    async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..10_000 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    async fn wait_state(db: &tokio_rusqlite::Connection, id: i64, want: JobState) {
        for _ in 0..10_000 {
            if view(db, id).await.state == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!(
            "job {id} never reached {want:?}; is {:?}",
            view(db, id).await.state
        );
    }

    /// Wait until the job has been run `attempts` times and put back to
    /// `pending` with a recorded error (a bare `pending` is also its state
    /// before the first claim, so state alone proves nothing).
    async fn wait_retry(db: &tokio_rusqlite::Connection, id: i64, attempts: i64) {
        for _ in 0..10_000 {
            let v = view(db, id).await;
            if v.state == JobState::Pending && v.attempts == attempts && v.last_error_code.is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("job {id} never returned to pending after attempt {attempts}");
    }

    /// Give a runner that should stay quiet a real chance to misbehave.
    async fn linger() {
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    struct Up {
        clock: Arc<FakeClock>,
        wake: WakeHandle,
        stop: ShutdownHandle,
        join: tokio::task::JoinHandle<Result<RunSummary, RunnerError>>,
        report: Report,
    }

    async fn boot(
        db: &tokio_rusqlite::Connection,
        clock: &Arc<FakeClock>,
        reg: Registry,
        cfg: RunnerConfig,
    ) -> Up {
        let runner = Runner::new(db.clone(), clock.clone(), reg, cfg).unwrap();
        let wake = runner.wake_handle();
        let stop = runner.shutdown_handle();
        let started = runner.start().await.unwrap();
        let report = started.report().clone();
        let join = tokio::spawn(started.run());
        Up {
            clock: clock.clone(),
            wake,
            stop,
            join,
            report,
        }
    }

    impl Up {
        /// Shut down a runner with nothing in flight: returns at once.
        async fn stop_idle(self) -> RunSummary {
            self.stop.trigger();
            self.join.await.unwrap().unwrap()
        }

        /// Shut down a runner with handlers stuck in flight: wait until it is
        /// sleeping out the grace period, then let that time pass.
        async fn stop_after_grace(self, grace_secs: i64) -> RunSummary {
            let before = self.clock.sleeps();
            self.stop.trigger();
            let clock = self.clock.clone();
            eventually("grace sleep", || {
                clock.sleeps() > before && clock.last_sleep_secs() == grace_secs
            })
            .await;
            self.clock.advance(grace_secs);
            self.join.await.unwrap().unwrap()
        }
    }

    #[derive(Default)]
    struct Gate(AtomicBool);
    impl Gate {
        fn open(&self) {
            self.0.store(true, SeqCst);
        }
        async fn wait(&self) {
            while !self.0.load(SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }

    // ---- configuration and registry ----

    #[test]
    fn config_default_is_valid_and_bounds_are_enforced_by_runner_new() {
        assert_eq!(RunnerConfig::DEFAULT.validate(), Ok(()));
        let ok = |c: RunnerConfig| c.validate();
        let base = cfg(1);
        assert_eq!(
            ok(RunnerConfig {
                concurrency: 0,
                ..base
            }),
            Err(RunnerError::InvalidConcurrency)
        );
        assert_eq!(
            ok(RunnerConfig {
                concurrency: MAX_CONCURRENCY + 1,
                ..base
            }),
            Err(RunnerError::InvalidConcurrency)
        );
        assert_eq!(
            ok(RunnerConfig {
                concurrency: MAX_CONCURRENCY,
                ..base
            }),
            Ok(())
        );
        assert_eq!(
            ok(RunnerConfig {
                concurrency: usize::MAX,
                ..base
            }),
            Err(RunnerError::InvalidConcurrency)
        );
        assert_eq!(
            ok(RunnerConfig {
                max_poll: MIN_POLL - Duration::from_millis(1),
                ..base
            }),
            Err(RunnerError::InvalidPoll)
        );
        assert_eq!(
            ok(RunnerConfig {
                max_poll: Duration::ZERO,
                ..base
            }),
            Err(RunnerError::InvalidPoll)
        );
        assert_eq!(
            ok(RunnerConfig {
                max_poll: MIN_POLL,
                ..base
            }),
            Ok(())
        );
        assert_eq!(
            ok(RunnerConfig {
                max_poll: MAX_POLL,
                ..base
            }),
            Ok(())
        );
        assert_eq!(
            ok(RunnerConfig {
                max_poll: MAX_POLL + Duration::from_millis(1),
                ..base
            }),
            Err(RunnerError::InvalidPoll)
        );
        let bad_retry = RetryPolicy {
            base_secs: 0,
            cap_secs: 10,
            max_attempts: 3,
        };
        assert_eq!(
            ok(RunnerConfig {
                retry: bad_retry,
                ..base
            }),
            Err(RunnerError::InvalidRetryPolicy)
        );
        let zero_attempts = RetryPolicy {
            base_secs: 1,
            cap_secs: 10,
            max_attempts: 0,
        };
        assert_eq!(
            ok(RunnerConfig {
                retry: zero_attempts,
                ..base
            }),
            Err(RunnerError::InvalidRetryPolicy)
        );
    }

    #[test]
    fn shutdown_grace_and_handler_timeout_are_validated_with_boundaries() {
        let base = cfg(1);
        let check = |c: RunnerConfig| c.validate();
        let ms = Duration::from_millis(1);
        // shutdown_grace: non-zero floor, documented maximum, both edges.
        let grace = |d: Duration| {
            check(RunnerConfig {
                shutdown_grace: d,
                ..base
            })
        };
        assert_eq!(
            grace(Duration::ZERO),
            Err(RunnerError::InvalidShutdownGrace)
        );
        assert_eq!(
            grace(MIN_SHUTDOWN_GRACE - ms),
            Err(RunnerError::InvalidShutdownGrace)
        );
        assert_eq!(grace(MIN_SHUTDOWN_GRACE), Ok(()));
        assert_eq!(grace(MAX_SHUTDOWN_GRACE), Ok(()));
        assert_eq!(
            grace(MAX_SHUTDOWN_GRACE + ms),
            Err(RunnerError::InvalidShutdownGrace)
        );
        assert_eq!(
            grace(Duration::from_secs(365 * 24 * 3600)),
            Err(RunnerError::InvalidShutdownGrace)
        );
        assert_eq!(grace(Duration::MAX), Err(RunnerError::InvalidShutdownGrace));
        // handler_timeout: same shape.
        let timeout = |d: Duration| {
            check(RunnerConfig {
                handler_timeout: d,
                ..base
            })
        };
        assert_eq!(
            timeout(Duration::ZERO),
            Err(RunnerError::InvalidHandlerTimeout)
        );
        assert_eq!(
            timeout(MIN_HANDLER_TIMEOUT - ms),
            Err(RunnerError::InvalidHandlerTimeout)
        );
        assert_eq!(timeout(MIN_HANDLER_TIMEOUT), Ok(()));
        assert_eq!(timeout(MAX_HANDLER_TIMEOUT), Ok(()));
        assert_eq!(
            timeout(MAX_HANDLER_TIMEOUT + ms),
            Err(RunnerError::InvalidHandlerTimeout)
        );
        assert_eq!(
            timeout(Duration::MAX),
            Err(RunnerError::InvalidHandlerTimeout)
        );
        // The shipped defaults sit inside both ranges, and the ranges are sane.
        let d = RunnerConfig::DEFAULT;
        assert!((MIN_SHUTDOWN_GRACE..=MAX_SHUTDOWN_GRACE).contains(&d.shutdown_grace));
        assert!((MIN_HANDLER_TIMEOUT..=MAX_HANDLER_TIMEOUT).contains(&d.handler_timeout));
        assert!(MIN_SHUTDOWN_GRACE > Duration::ZERO && MIN_HANDLER_TIMEOUT > Duration::ZERO);
        assert_eq!(
            RunnerError::InvalidShutdownGrace.code(),
            "invalid_shutdown_grace"
        );
        assert_eq!(
            RunnerError::InvalidHandlerTimeout.code(),
            "invalid_handler_timeout"
        );
    }

    #[tokio::test]
    async fn runner_new_refuses_an_out_of_range_grace_or_timeout() {
        let bad_grace = RunnerConfig {
            shutdown_grace: Duration::ZERO,
            ..cfg(1)
        };
        let bad_timeout = RunnerConfig {
            handler_timeout: Duration::MAX,
            ..cfg(1)
        };
        for (c, want) in [
            (bad_grace, RunnerError::InvalidShutdownGrace),
            (bad_timeout, RunnerError::InvalidHandlerTimeout),
        ] {
            let err = Runner::new(mem_db(), FakeClock::new(), Registry::new(), c).err();
            assert_eq!(err, Some(want));
        }
    }

    #[tokio::test]
    async fn runner_new_refuses_an_invalid_config() {
        let clock = FakeClock::new();
        let bad = RunnerConfig {
            concurrency: 0,
            ..cfg(1)
        };
        let err = Runner::new(mem_db(), clock, Registry::new(), bad).err();
        assert_eq!(err, Some(RunnerError::InvalidConcurrency));
    }

    #[tokio::test]
    async fn registry_accepts_exactly_the_kinds_the_queue_accepts_and_refuses_duplicates() {
        let db = mem_db();
        let long = "a".repeat(MAX_KIND_LEN);
        let too_long = "a".repeat(MAX_KIND_LEN + 1);
        let kinds = [
            "ok_kind",
            "a",
            "k9",
            "_",
            long.as_str(),
            "",
            "Upper",
            "has space",
            "dash-ed",
            "uni\u{e9}",
            "nul\0byte",
            too_long.as_str(),
            "dot.ted",
        ];
        for (i, kind) in kinds.iter().enumerate() {
            let mut reg = Registry::new();
            let reg_ok = reg
                .register(kind, RestartPolicy::RunAgain, |_| async { Ok(()) })
                .is_ok();
            let kind_owned = kind.to_string();
            let key = format!("k{i}");
            let queue_ok = db
                .call(move |c| {
                    queue::schedule(
                        c,
                        &NewJob {
                            kind: &kind_owned,
                            dedupe_key: &key,
                            payload_json: "{}",
                            due_at: T0,
                        },
                        T0,
                    )
                })
                .await
                .is_ok();
            assert_eq!(reg_ok, queue_ok, "kind index {i}");
            assert_eq!(reg.len(), usize::from(reg_ok));
        }
        let mut reg = Registry::new();
        assert!(reg
            .register("dup", RestartPolicy::RunAgain, |_| async { Ok(()) })
            .is_ok());
        assert_eq!(
            reg.register("dup", RestartPolicy::AtMostOnce, |_| async { Ok(()) }),
            Err(RunnerError::DuplicateKind)
        );
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    // ---- startup reconcile ----

    #[tokio::test]
    async fn start_reconciles_before_the_first_claim_per_registry_policy() {
        let db = mem_db();
        let clock = FakeClock::new();
        let rerun = sched(&db, "rerun", "a", "{}", T0 - 5).await;
        let once = sched(&db, "once", "b", "{}", T0 - 5).await;
        let ghost = sched(&db, "ghost", "c", "{}", T0 - 5).await;
        for id in [rerun, once, ghost] {
            assert!(db.call(move |c| queue::try_claim(c, id, T0)).await.unwrap());
        }
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut reg = Registry::new();
        for (kind, policy) in [
            ("rerun", RestartPolicy::RunAgain),
            ("once", RestartPolicy::AtMostOnce),
        ] {
            let calls = calls.clone();
            reg.register(kind, policy, move |j| {
                let calls = calls.clone();
                async move {
                    calls.lock().unwrap().push(j.kind);
                    Ok(())
                }
            })
            .unwrap();
        }
        let up = boot(&db, &clock, reg, cfg(2)).await;
        // The report is what the caller logs: it names every job found running.
        let mut seen: Vec<(i64, ReconcileOutcome)> = up
            .report
            .entries
            .iter()
            .map(|e| (e.id, e.outcome))
            .collect();
        seen.sort_by_key(|(id, _)| *id);
        assert_eq!(
            seen,
            vec![
                (rerun, ReconcileOutcome::Requeued),
                (once, ReconcileOutcome::AbandonedAtMostOnce),
                (ghost, ReconcileOutcome::AbandonedUnregistered),
            ]
        );
        wait_state(&db, rerun, JobState::Done).await;
        linger().await;
        // Only the RunAgain job ran; the abandoned ones were never re-run.
        assert_eq!(*calls.lock().unwrap(), vec!["rerun".to_string()]);
        assert_eq!(view(&db, once).await.state, JobState::Abandoned);
        assert_eq!(view(&db, ghost).await.state, JobState::Abandoned);
        up.stop_idle().await;
    }

    #[tokio::test]
    async fn start_fails_closed_when_the_database_is_gone() {
        let db = mem_db();
        db.clone().close().await.unwrap();
        let clock = FakeClock::new();
        let runner = Runner::new(db, clock, Registry::new(), cfg(1)).unwrap();
        assert_eq!(runner.start().await.err(), Some(RunnerError::DbClosed));
    }

    // ---- ordering and the due gate ----

    #[tokio::test]
    async fn overdue_jobs_at_start_run_in_due_order_and_future_jobs_wait() {
        let db = mem_db();
        let clock = FakeClock::new();
        // Inserted out of order; ties on due_at are broken by id.
        let c = sched(&db, "t", "c", "{\"n\":3}", T0 - 10).await;
        let a = sched(&db, "t", "a", "{\"n\":1}", T0 - 100).await;
        let b = sched(&db, "t", "b", "{\"n\":2}", T0 - 10).await;
        let d = sched(&db, "t", "d", "{\"n\":4}", T0).await;
        let later = sched(&db, "t", "e", "{\"n\":5}", T0 + 1).await;
        let order = Arc::new(Mutex::new(Vec::<i64>::new()));
        let mut reg = Registry::new();
        let o = order.clone();
        reg.register("t", RestartPolicy::RunAgain, move |j| {
            let o = o.clone();
            async move {
                o.lock().unwrap().push(j.id);
                Ok(())
            }
        })
        .unwrap();
        // One at a time, so execution order is claim order = (due_at, id).
        let up = boot(&db, &clock, reg, cfg(1)).await;
        wait_state(&db, d, JobState::Done).await;
        linger().await;
        assert_eq!(*order.lock().unwrap(), vec![a, c, b, d]);
        assert_eq!(
            view(&db, later).await.state,
            JobState::Pending,
            "due in 1 s: not yet"
        );
        clock.advance(1);
        wait_state(&db, later, JobState::Done).await;
        assert_eq!(*order.lock().unwrap(), vec![a, c, b, d, later]);
        up.stop_idle().await;
    }

    // ---- retry, backoff, unknown kind, panics ----

    #[tokio::test]
    async fn handler_error_retries_with_backoff_then_fails_after_max_attempts() {
        let db = mem_db();
        let clock = FakeClock::new();
        let id = sched(&db, "flaky", "a", "{}", T0).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut reg = Registry::new();
        let n = calls.clone();
        reg.register("flaky", RestartPolicy::RunAgain, move |_| {
            let n = n.clone();
            async move {
                n.fetch_add(1, SeqCst);
                Err(HandlerError::new("upstream_down"))
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        wait_retry(&db, id, 1).await;
        let v = view(&db, id).await;
        assert_eq!(
            (v.attempts, v.due_at, v.last_error_code.as_deref()),
            (1, T0 + 30, Some("upstream_down"))
        );
        assert_eq!(calls.load(SeqCst), 1);
        // 29 s later it still must not have run again; at 30 s it does.
        clock.advance(29);
        linger().await;
        assert_eq!(calls.load(SeqCst), 1, "backoff not over");
        clock.advance(1);
        eventually("second run", || calls.load(SeqCst) == 2).await;
        wait_retry(&db, id, 2).await;
        let v = view(&db, id).await;
        assert_eq!((v.attempts, v.due_at), (2, T0 + 30 + 60), "doubled backoff");
        clock.advance(59);
        linger().await;
        assert_eq!(calls.load(SeqCst), 2);
        clock.advance(1);
        wait_state(&db, id, JobState::Failed).await;
        let v = view(&db, id).await;
        assert_eq!(
            (v.attempts, v.last_error_code.as_deref()),
            (3, Some("upstream_down"))
        );
        // Terminal: no further run however much time passes.
        clock.advance(100_000);
        up.wake.wake();
        linger().await;
        assert_eq!(calls.load(SeqCst), 3);
        up.stop_idle().await;
    }

    #[tokio::test]
    async fn unknown_kind_is_failed_with_a_stable_code_and_no_other_handler_runs_it() {
        let db = mem_db();
        let clock = FakeClock::new();
        let ghost = sched(&db, "ghost", "g", "{\"x\":1}", T0 - 1).await;
        let good = sched(&db, "good", "h", "{}", T0).await;
        let ran = Arc::new(Mutex::new(Vec::<i64>::new()));
        let mut reg = Registry::new();
        for kind in ["good", "other"] {
            let ran = ran.clone();
            reg.register(kind, RestartPolicy::RunAgain, move |j| {
                let ran = ran.clone();
                async move {
                    ran.lock().unwrap().push(j.id);
                    Ok(())
                }
            })
            .unwrap();
        }
        let up = boot(&db, &clock, reg, cfg(2)).await;
        wait_state(&db, ghost, JobState::Failed).await;
        wait_state(&db, good, JobState::Done).await;
        let v = view(&db, ghost).await;
        assert_eq!(
            (v.attempts, v.last_error_code.as_deref()),
            (1, Some(CODE_NO_HANDLER))
        );
        assert_eq!(
            *ran.lock().unwrap(),
            vec![good],
            "the ghost job reached no handler"
        );
        let summary = up.stop_idle().await;
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.completed, 1);
    }

    #[tokio::test]
    async fn a_panicking_handler_is_a_failed_job_and_the_runner_keeps_going() {
        let db = mem_db();
        let clock = FakeClock::new();
        let bad = sched(&db, "boom", "a", "{\"secret\":\"SECRET_PAYLOAD\"}", T0).await;
        let mut reg = Registry::new();
        reg.register("boom", RestartPolicy::RunAgain, |j| async move {
            if j.payload_json.contains("SECRET_PAYLOAD") {
                panic!("PANIC_TEXT {}", j.payload_json);
            }
            Ok(())
        })
        .unwrap();
        let ok_calls = Arc::new(AtomicUsize::new(0));
        let n = ok_calls.clone();
        reg.register("fine", RestartPolicy::RunAgain, move |_| {
            let n = n.clone();
            async move {
                n.fetch_add(1, SeqCst);
                Ok(())
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        wait_retry(&db, bad, 1).await;
        let v = view(&db, bad).await;
        assert_eq!(v.last_error_code.as_deref(), Some(CODE_PANICKED));
        assert_eq!(
            v.due_at,
            T0 + 30,
            "a RunAgain panic is retried with backoff"
        );
        // The runner survived: a job scheduled afterwards still runs.
        let after = sched(&db, "fine", "b", "{}", T0).await;
        up.wake.wake();
        wait_state(&db, after, JobState::Done).await;
        // ...and the panicking job keeps being retried, then ends failed.
        for _ in 0..2 {
            clock.advance(100);
            up.wake.wake();
            linger().await;
        }
        wait_state(&db, bad, JobState::Failed).await;
        assert!(!up.join.is_finished(), "runner must still be up");
        let summary = up.stop_idle().await;
        let text = format!("{summary:?}");
        assert!(
            !text.contains("SECRET_PAYLOAD") && !text.contains("PANIC_TEXT"),
            "{text}"
        );
        assert_eq!(ok_calls.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn an_at_most_once_panic_is_terminal_because_its_effect_may_have_happened() {
        let db = mem_db();
        let clock = FakeClock::new();
        let id = sched(&db, "ping", "a", "{}", T0).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let mut reg = Registry::new();
        reg.register("ping", RestartPolicy::AtMostOnce, move |_| {
            let n = n.clone();
            async move {
                n.fetch_add(1, SeqCst);
                if n.load(SeqCst) > 0 {
                    panic!("after the effect");
                }
                Ok(())
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        wait_state(&db, id, JobState::Failed).await;
        let v = view(&db, id).await;
        assert_eq!(
            (v.attempts, v.last_error_code.as_deref()),
            (1, Some(CODE_PANICKED))
        );
        clock.advance(10_000);
        up.wake.wake();
        linger().await;
        assert_eq!(calls.load(SeqCst), 1, "never run a second time");
        up.stop_idle().await;
    }

    #[tokio::test]
    async fn malformed_rows_fail_alone_and_the_rest_still_run() {
        let db = mem_db();
        let clock = FakeClock::new();
        let bad_payload = sched(&db, "t", "a", "not json", T0 - 3).await;
        let bad_kind = sched(&db, "t", "b", "{}", T0 - 2).await;
        sql(
            &db,
            "UPDATE jobs SET kind = 'Bad Kind!' WHERE dedupe_key = 'b'",
        )
        .await;
        let good = sched(&db, "t", "c", "{}", T0 - 1).await;
        let calls = Arc::new(Mutex::new(Vec::<i64>::new()));
        let mut reg = Registry::new();
        let cl = calls.clone();
        reg.register("t", RestartPolicy::RunAgain, move |j| {
            let cl = cl.clone();
            async move {
                cl.lock().unwrap().push(j.id);
                Ok(())
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, cfg(3)).await;
        wait_state(&db, good, JobState::Done).await;
        assert_eq!(
            view(&db, bad_payload).await.last_error_code.as_deref(),
            Some(queue::CODE_PAYLOAD_MALFORMED)
        );
        assert_eq!(
            view(&db, bad_kind).await.last_error_code.as_deref(),
            Some(queue::CODE_KIND_MALFORMED)
        );
        assert_eq!(*calls.lock().unwrap(), vec![good]);
        let summary = up.stop_idle().await;
        assert_eq!(summary.refused_at_claim, 2);
    }

    #[tokio::test]
    async fn handler_error_codes_that_are_not_plain_tokens_are_stored_as_invalid_code() {
        let db = mem_db();
        let clock = FakeClock::new();
        let id = sched(&db, "t", "a", "{}", T0).await;
        let mut reg = Registry::new();
        reg.register("t", RestartPolicy::RunAgain, |_| async {
            Err(HandlerError::new("Has Spaces And Text"))
        })
        .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        wait_retry(&db, id, 1).await;
        assert_eq!(
            view(&db, id).await.last_error_code.as_deref(),
            Some(queue::CODE_INVALID_CODE)
        );
        up.stop_idle().await;
    }

    // ---- concurrency ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrency_never_exceeds_n_and_reaches_n() {
        for n in [1usize, 2, 5] {
            let db = mem_db();
            let clock = FakeClock::new();
            let mut ids = Vec::new();
            for i in 0..12 {
                ids.push(sched(&db, "t", &format!("k{i}"), "{}", T0).await);
            }
            let inflight = Arc::new(AtomicUsize::new(0));
            let max = Arc::new(AtomicUsize::new(0));
            let gate = Arc::new(Gate::default());
            let mut reg = Registry::new();
            let (i2, m2, g2) = (inflight.clone(), max.clone(), gate.clone());
            reg.register("t", RestartPolicy::RunAgain, move |_| {
                let (i2, m2, g2) = (i2.clone(), m2.clone(), g2.clone());
                async move {
                    let now = i2.fetch_add(1, SeqCst) + 1;
                    m2.fetch_max(now, SeqCst);
                    g2.wait().await;
                    i2.fetch_sub(1, SeqCst);
                    Ok(())
                }
            })
            .unwrap();
            let up = boot(&db, &clock, reg, cfg(n)).await;
            let i3 = inflight.clone();
            eventually("n handlers in flight", || i3.load(SeqCst) == n).await;
            linger().await;
            assert_eq!(
                inflight.load(SeqCst),
                n,
                "n={n}: stays at the limit while the rest wait"
            );
            let pending = db
                .call(|c| {
                    c.query_row("SELECT COUNT(*) FROM jobs WHERE state='pending'", [], |r| {
                        r.get::<_, i64>(0)
                    })
                })
                .await
                .unwrap();
            assert_eq!(
                pending,
                12 - i64::try_from(n).unwrap(),
                "unclaimed jobs stay pending, not running"
            );
            gate.open();
            for id in &ids {
                wait_state(&db, *id, JobState::Done).await;
            }
            assert_eq!(max.load(SeqCst), n, "n={n}");
            up.stop_idle().await;
        }
    }

    // ---- wake-on-schedule, no busy loop ----

    #[tokio::test]
    async fn an_empty_queue_sleeps_instead_of_spinning() {
        let db = mem_db();
        let clock = FakeClock::new();
        let mut reg = Registry::new();
        reg.register("t", RestartPolicy::RunAgain, |_| async { Ok(()) })
            .unwrap();
        let up = boot(&db, &clock, reg, cfg(2)).await;
        let c = clock.clone();
        eventually("first sleep", || c.sleeps() >= 1).await;
        assert_eq!(
            clock.last_deadline(),
            T0 + 30,
            "sleeps the max poll interval"
        );
        let sample = (clock.nows(), clock.sleeps());
        linger().await;
        assert_eq!(
            (clock.nows(), clock.sleeps()),
            sample,
            "no wake-ups while nothing happens"
        );
        // One poll interval later: exactly one more pass.
        clock.advance(30);
        let c = clock.clone();
        eventually("second sleep", || c.sleeps() == sample.1 + 1).await;
        let sample = (clock.nows(), clock.sleeps());
        linger().await;
        assert_eq!((clock.nows(), clock.sleeps()), sample);
        up.stop_idle().await;
    }

    #[tokio::test]
    async fn sleeps_until_the_next_due_job_but_never_longer_than_the_poll_interval() {
        let db = mem_db();
        let clock = FakeClock::new();
        let soon = sched(&db, "t", "a", "{}", T0 + 10).await;
        let mut reg = Registry::new();
        reg.register("t", RestartPolicy::RunAgain, |_| async { Ok(()) })
            .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        let c = clock.clone();
        eventually("sleep", || c.sleeps() >= 1).await;
        assert_eq!(
            clock.last_deadline(),
            T0 + 10,
            "wakes exactly when the job is due"
        );
        clock.advance(10);
        wait_state(&db, soon, JobState::Done).await;
        let far = sched(&db, "t", "b", "{}", T0 + 10 + 500).await;
        up.wake.wake();
        let before = clock.sleeps();
        let c = clock.clone();
        eventually("re-sleep", || c.sleeps() > before).await;
        assert_eq!(clock.last_deadline(), T0 + 10 + 30, "capped at max_poll");
        assert_eq!(view(&db, far).await.state, JobState::Pending);
        up.stop_idle().await;
    }

    #[tokio::test]
    async fn a_wake_makes_a_newly_scheduled_job_run_without_waiting_for_the_poll() {
        let db = mem_db();
        let clock = FakeClock::new();
        let mut reg = Registry::new();
        reg.register("t", RestartPolicy::RunAgain, |_| async { Ok(()) })
            .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        let c = clock.clone();
        eventually("asleep", || c.sleeps() >= 1).await;
        let id = sched(&db, "t", "a", "{}", T0).await;
        linger().await;
        assert_eq!(
            view(&db, id).await.state,
            JobState::Pending,
            "no wake, no clock movement: still asleep"
        );
        up.wake.wake();
        wait_state(&db, id, JobState::Done).await;
        // A wake sent while the runner is busy is not lost either.
        let id2 = sched(&db, "t", "b", "{}", T0).await;
        up.wake.wake();
        wait_state(&db, id2, JobState::Done).await;
        up.stop_idle().await;
    }

    // ---- shutdown and restart ----

    #[tokio::test]
    async fn shutdown_mid_run_leaves_jobs_running_and_the_next_start_reconciles_per_policy() {
        let dir = scratch("shutdown-restart");
        let path = dir.join("core.sqlite");
        let path = path.to_str().unwrap();
        let db = file_db(path);
        let clock = FakeClock::new();
        let rerun = sched(&db, "rerun", "a", "{}", T0).await;
        let once = sched(&db, "once", "b", "{}", T0).await;
        let calls = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let gate = Arc::new(Gate::default());
        let make_reg = |calls: Arc<Mutex<HashMap<String, usize>>>, gate: Arc<Gate>| {
            let mut reg = Registry::new();
            for (kind, policy) in [
                ("rerun", RestartPolicy::RunAgain),
                ("once", RestartPolicy::AtMostOnce),
            ] {
                let (calls, gate) = (calls.clone(), gate.clone());
                reg.register(kind, policy, move |j| {
                    let (calls, gate) = (calls.clone(), gate.clone());
                    async move {
                        *calls.lock().unwrap().entry(j.kind).or_default() += 1;
                        gate.wait().await;
                        Ok(())
                    }
                })
                .unwrap();
            }
            reg
        };
        // Run 1: both handlers are stuck when shutdown arrives.
        let up = boot(&db, &clock, make_reg(calls.clone(), gate.clone()), cfg(2)).await;
        let cl = calls.clone();
        eventually("both started", || {
            cl.lock().unwrap().values().sum::<usize>() == 2
        })
        .await;
        let summary = up.stop_after_grace(5).await;
        assert_eq!(summary.left_running, vec![rerun, once]);
        assert_eq!(summary.completed, 0);
        assert_eq!(view(&db, rerun).await.state, JobState::Running);
        assert_eq!(view(&db, once).await.state, JobState::Running);
        drop(db);
        // Run 2 (a "restart": new connection to the same file, new runner).
        gate.open();
        let db2 = file_db(path);
        let up2 = boot(&db2, &clock, make_reg(calls.clone(), gate.clone()), cfg(2)).await;
        let mut seen: Vec<(i64, ReconcileOutcome)> = up2
            .report
            .entries
            .iter()
            .map(|e| (e.id, e.outcome))
            .collect();
        seen.sort_by_key(|(id, _)| *id);
        assert_eq!(
            seen,
            vec![
                (rerun, ReconcileOutcome::Requeued),
                (once, ReconcileOutcome::AbandonedAtMostOnce)
            ]
        );
        let abandoned = up2.report.abandoned();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(
            (abandoned[0].id, abandoned[0].kind.as_str()),
            (once, "once")
        );
        wait_state(&db2, rerun, JobState::Done).await;
        linger().await;
        assert_eq!(view(&db2, once).await.state, JobState::Abandoned);
        let (reruns, onces) = {
            let c = calls.lock().unwrap();
            (c["rerun"], c["once"])
        };
        assert_eq!(
            (reruns, onces),
            (2, 1),
            "RunAgain re-ran, AtMostOnce did not"
        );
        up2.stop_idle().await;
    }

    #[tokio::test]
    async fn shutdown_lets_in_flight_finish_within_grace_and_claims_nothing_new() {
        let db = mem_db();
        let clock = FakeClock::new();
        let first = sched(&db, "t", "a", "{}", T0).await;
        let second = sched(&db, "t", "b", "{}", T0).await;
        let gate = Arc::new(Gate::default());
        let started = Arc::new(AtomicUsize::new(0));
        let mut reg = Registry::new();
        let (g, s) = (gate.clone(), started.clone());
        reg.register("t", RestartPolicy::RunAgain, move |_| {
            let (g, s) = (g.clone(), s.clone());
            async move {
                s.fetch_add(1, SeqCst);
                g.wait().await;
                Ok(())
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        let s = started.clone();
        eventually("first started", || s.load(SeqCst) == 1).await;
        up.stop.trigger();
        let c = clock.clone();
        eventually("grace sleep", || c.last_sleep_secs() == 5).await;
        gate.open(); // finishes well inside the grace period (the clock never moved)
        let summary = up.join.await.unwrap().unwrap();
        assert_eq!(summary.left_running, Vec::<i64>::new());
        assert_eq!(summary.completed, 1);
        assert_eq!(view(&db, first).await.state, JobState::Done);
        assert_eq!(
            view(&db, second).await.state,
            JobState::Pending,
            "not claimed after shutdown"
        );
        assert_eq!(started.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn shutdown_of_an_idle_runner_returns_without_any_clock_movement() {
        let db = mem_db();
        let clock = FakeClock::new();
        let up = boot(&db, &clock, Registry::new(), cfg(1)).await;
        let c = clock.clone();
        eventually("asleep", || c.sleeps() >= 1).await;
        let summary = up.stop_idle().await;
        assert_eq!(summary, RunSummary::default());
    }

    #[tokio::test]
    async fn shutdown_requested_before_run_claims_nothing() {
        let db = mem_db();
        let clock = FakeClock::new();
        let id = sched(&db, "t", "a", "{}", T0 - 1).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut reg = Registry::new();
        let n = calls.clone();
        reg.register("t", RestartPolicy::RunAgain, move |_| {
            let n = n.clone();
            async move {
                n.fetch_add(1, SeqCst);
                Ok(())
            }
        })
        .unwrap();
        let runner = Runner::new(db.clone(), clock.clone(), reg, cfg(1)).unwrap();
        runner.shutdown_handle().trigger();
        let summary = runner.start().await.unwrap().run().await.unwrap();
        assert_eq!(summary, RunSummary::default());
        assert_eq!(calls.load(SeqCst), 0);
        assert_eq!(view(&db, id).await.state, JobState::Pending);
    }

    // ---- database trouble ----

    #[tokio::test]
    async fn a_failed_acknowledgement_is_retried_not_forgotten() {
        let db = mem_db();
        let clock = FakeClock::new();
        sql(
            &db,
            "CREATE TABLE test_flags (block INTEGER NOT NULL); INSERT INTO test_flags VALUES (1); \
             CREATE TRIGGER block_done BEFORE UPDATE ON jobs WHEN NEW.state = 'done' \
             AND (SELECT block FROM test_flags) = 1 BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
        )
        .await;
        let id = sched(&db, "t", "a", "{}", T0).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut reg = Registry::new();
        let n = calls.clone();
        reg.register("t", RestartPolicy::RunAgain, move |_| {
            let n = n.clone();
            async move {
                n.fetch_add(1, SeqCst);
                Ok(())
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        let c = clock.clone();
        eventually("retry sleep", || c.last_sleep_secs() == 1).await;
        assert_eq!(
            view(&db, id).await.state,
            JobState::Running,
            "ack could not be written yet"
        );
        sql(&db, "UPDATE test_flags SET block = 0").await;
        clock.advance(1);
        wait_state(&db, id, JobState::Done).await;
        assert_eq!(calls.load(SeqCst), 1, "the handler was not run again");
        let summary = up.stop_idle().await;
        assert!(summary.db_errors >= 1);
        assert_eq!(summary.completed, 1);
        assert!(summary.left_running.is_empty());
    }

    #[tokio::test]
    async fn an_acknowledgement_that_never_succeeds_is_reported_as_left_running_on_shutdown() {
        let db = mem_db();
        let clock = FakeClock::new();
        sql(
            &db,
            "CREATE TRIGGER block_done BEFORE UPDATE ON jobs WHEN NEW.state = 'done' \
             BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
        )
        .await;
        let id = sched(&db, "t", "a", "{}", T0).await;
        let mut reg = Registry::new();
        reg.register("t", RestartPolicy::RunAgain, |_| async { Ok(()) })
            .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        let c = clock.clone();
        eventually("retry sleep", || c.last_sleep_secs() == 1).await;
        let summary = up.stop_idle().await;
        assert_eq!(summary.left_running, vec![id]);
        assert_eq!(view(&db, id).await.state, JobState::Running);
    }

    #[tokio::test]
    async fn the_runner_stops_with_a_stable_code_when_the_database_closes() {
        let db = mem_db();
        let clock = FakeClock::new();
        let up = boot(&db, &clock, Registry::new(), cfg(1)).await;
        let c = clock.clone();
        eventually("asleep", || c.sleeps() >= 1).await;
        db.clone().close().await.unwrap();
        up.wake.wake();
        let res = up.join.await.unwrap();
        assert_eq!(res, Err(RunnerError::DbClosed));
        assert_eq!(RunnerError::DbClosed.code(), "db_closed");
    }

    #[test]
    fn error_codes_are_distinct_tokens_without_text() {
        let all = [
            RunnerError::InvalidConcurrency,
            RunnerError::InvalidPoll,
            RunnerError::InvalidShutdownGrace,
            RunnerError::InvalidHandlerTimeout,
            RunnerError::InvalidRetryPolicy,
            RunnerError::InvalidKind,
            RunnerError::DuplicateKind,
            RunnerError::Queue(QueueError::Sqlite),
            RunnerError::DbClosed,
        ];
        let codes: BTreeSet<&str> = all.iter().map(|e| e.code()).collect();
        assert_eq!(codes.len(), all.len());
        for e in all {
            assert!(e
                .code()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_'));
            assert_eq!(e.to_string(), e.code());
        }
    }

    #[tokio::test]
    async fn the_system_clock_ticks_and_sleeps() {
        let c = SystemClock;
        assert!(c.now() > 0);
        c.sleep(Duration::from_millis(2)).await;
        // A deadline that has passed is ready at once; one far ahead is not.
        c.sleep_until(0).await;
        c.sleep_until(c.now() - 1000).await;
        let far =
            tokio::time::timeout(Duration::from_millis(20), c.sleep_until(c.now() + 3600)).await;
        assert!(
            far.is_err(),
            "a deadline an hour away must still be pending"
        );
    }

    // ---- handler timeout (a hung handler must not hold its slot forever) ----

    /// Sets a flag when dropped: proves the runner aborted a handler future.
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, SeqCst);
        }
    }

    fn timeout_cfg(n: usize, secs: u64) -> RunnerConfig {
        RunnerConfig {
            handler_timeout: Duration::from_secs(secs),
            ..cfg(n)
        }
    }

    #[tokio::test]
    async fn a_hung_handler_times_out_frees_its_slot_and_is_retried_with_backoff() {
        let db = mem_db();
        let clock = FakeClock::new();
        // Concurrency 1: while `hang` holds the only slot, `ok` cannot run.
        let hang = sched(&db, "hang", "a", "{}", T0).await;
        let ok = sched(&db, "ok", "b", "{}", T0 + 1).await;
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let ok_calls = Arc::new(AtomicUsize::new(0));
        let mut reg = Registry::new();
        let (s, d) = (started.clone(), dropped.clone());
        reg.register("hang", RestartPolicy::RunAgain, move |_| {
            let (s, d) = (s.clone(), d.clone());
            async move {
                s.fetch_add(1, SeqCst);
                let _flag = DropFlag(d);
                std::future::pending::<()>().await;
                Ok(())
            }
        })
        .unwrap();
        let n = ok_calls.clone();
        reg.register("ok", RestartPolicy::RunAgain, move |_| {
            let n = n.clone();
            async move {
                n.fetch_add(1, SeqCst);
                Ok(())
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, timeout_cfg(1, 60)).await;
        let s = started.clone();
        eventually("hang started", || s.load(SeqCst) == 1).await;
        // Hours of idle wakes do not free the slot before the timeout...
        clock.advance(59);
        up.wake.wake();
        linger().await;
        assert_eq!(view(&db, hang).await.state, JobState::Running);
        assert_eq!(view(&db, ok).await.state, JobState::Pending);
        assert!(!dropped.load(SeqCst), "not aborted before its timeout");
        // ...and the timeout does.
        clock.advance(1);
        wait_state(&db, ok, JobState::Done).await;
        assert_eq!(ok_calls.load(SeqCst), 1);
        wait_retry(&db, hang, 1).await;
        let v = view(&db, hang).await;
        assert_eq!(
            (v.attempts, v.due_at, v.last_error_code.as_deref()),
            (1, T0 + 60 + 30, Some(CODE_TIMED_OUT)),
            "RunAgain: failed with the queue's backoff"
        );
        assert!(dropped.load(SeqCst), "the hung handler future was aborted");
        assert_eq!(started.load(SeqCst), 1, "not run again before its backoff");
        let summary = up.stop_idle().await;
        assert_eq!((summary.completed, summary.retried), (1, 1));
        assert!(summary.left_running.is_empty());
    }

    #[tokio::test]
    async fn a_hung_at_most_once_handler_times_out_terminal_and_is_never_run_again() {
        let db = mem_db();
        let clock = FakeClock::new();
        let once = sched(&db, "once", "a", "{}", T0).await;
        let ok = sched(&db, "ok", "b", "{}", T0 + 1).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut reg = Registry::new();
        let n = calls.clone();
        reg.register("once", RestartPolicy::AtMostOnce, move |_| {
            let n = n.clone();
            async move {
                n.fetch_add(1, SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            }
        })
        .unwrap();
        reg.register("ok", RestartPolicy::RunAgain, |_| async { Ok(()) })
            .unwrap();
        let up = boot(&db, &clock, reg, timeout_cfg(1, 60)).await;
        let c = calls.clone();
        eventually("once started", || c.load(SeqCst) == 1).await;
        clock.advance(60);
        wait_state(&db, once, JobState::Failed).await;
        wait_state(&db, ok, JobState::Done).await;
        let v = view(&db, once).await;
        assert_eq!(
            (v.attempts, v.last_error_code.as_deref()),
            (1, Some(CODE_TIMED_OUT))
        );
        clock.advance(100_000);
        up.wake.wake();
        linger().await;
        assert_eq!(
            calls.load(SeqCst),
            1,
            "its effect may have happened: no rerun"
        );
        let summary = up.stop_idle().await;
        assert_eq!((summary.failed, summary.completed), (1, 1));
        assert!(summary.left_running.is_empty());
    }

    #[tokio::test]
    async fn the_timeout_only_cuts_handlers_that_overrun_it() {
        let db = mem_db();
        let clock = FakeClock::new();
        let id = sched(&db, "slow", "a", "{}", T0).await;
        let gate = Arc::new(Gate::default());
        let started = Arc::new(AtomicUsize::new(0));
        let mut reg = Registry::new();
        let (g, s) = (gate.clone(), started.clone());
        reg.register("slow", RestartPolicy::RunAgain, move |_| {
            let (g, s) = (g.clone(), s.clone());
            async move {
                s.fetch_add(1, SeqCst);
                g.wait().await;
                Ok(())
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, timeout_cfg(1, 60)).await;
        let s = started.clone();
        eventually("started", || s.load(SeqCst) == 1).await;
        clock.advance(59);
        linger().await;
        gate.open(); // finishes with 1 s to spare
        wait_state(&db, id, JobState::Done).await;
        clock.advance(10_000);
        up.wake.wake();
        linger().await;
        let v = view(&db, id).await;
        assert_eq!(
            (v.state, v.attempts, v.last_error_code),
            (JobState::Done, 1, None)
        );
        assert_eq!(started.load(SeqCst), 1);
        up.stop_idle().await;
    }

    // ---- no spin when a due job cannot be claimed; unknown kinds in bulk ----

    #[tokio::test]
    async fn a_due_job_whose_claim_always_loses_does_not_make_the_runner_spin() {
        let db = mem_db();
        let clock = FakeClock::new();
        let id = sched(&db, "t", "a", "{}", T0 - 5).await;
        assert_eq!(id, 1);
        // Another claimer always wins: the claim UPDATE changes nothing, so
        // the job stays `pending` and due.
        sql(
            &db,
            "CREATE TRIGGER lose_claim BEFORE UPDATE ON jobs WHEN OLD.id = 1 \
             BEGIN SELECT RAISE(IGNORE); END;",
        )
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut reg = Registry::new();
        let n = calls.clone();
        reg.register("t", RestartPolicy::RunAgain, move |_| {
            let n = n.clone();
            async move {
                n.fetch_add(1, SeqCst);
                Ok(())
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        let c = clock.clone();
        eventually("first sleep", || c.sleeps() >= 1).await;
        // Frozen fake clock, ~80 ms of real time: a spin would make thousands
        // of passes (each one asks the clock for a sleep).
        linger().await;
        linger().await;
        assert_eq!(clock.sleeps(), 1, "one sleep, not a busy loop");
        assert!(clock.nows() <= 4, "clock reads: {}", clock.nows());
        assert_eq!(
            clock.last_deadline(),
            T0 + 1,
            "a due job that cannot be claimed is retried a second later, not at once"
        );
        assert_eq!(view(&db, id).await.state, JobState::Pending);
        // Time moving one second makes exactly one more pass.
        clock.advance(1);
        let c = clock.clone();
        eventually("second sleep", || c.sleeps() == 2).await;
        linger().await;
        assert_eq!(clock.sleeps(), 2);
        assert_eq!(clock.last_deadline(), T0 + 2);
        assert_eq!(calls.load(SeqCst), 0);
        let summary = up.stop_idle().await;
        assert_eq!(summary, RunSummary::default());
    }

    #[tokio::test]
    async fn many_unknown_kind_rows_are_all_failed_without_the_clock_moving() {
        let db = mem_db();
        let clock = FakeClock::new();
        let mut ids = Vec::new();
        for i in 0..7 {
            ids.push(sched(&db, "ghost", &format!("g{i}"), "{}", T0 - 1).await);
        }
        // Concurrency 2: seven rows take several claim batches. Each batch
        // changes rows, so the loop must go straight on to the next one
        // instead of waiting for time to pass (the clock is frozen).
        let up = boot(&db, &clock, Registry::new(), cfg(2)).await;
        for id in &ids {
            wait_state(&db, *id, JobState::Failed).await;
            let v = view(&db, *id).await;
            assert_eq!(v.last_error_code.as_deref(), Some(CODE_NO_HANDLER));
        }
        assert_eq!(clock.secs(), T0, "the clock never moved");
        let summary = up.stop_idle().await;
        assert_eq!((summary.claimed, summary.failed), (7, 7));
        assert!(summary.left_running.is_empty());
    }

    // ---- refused acknowledgements are counted ----

    #[tokio::test]
    async fn an_acknowledgement_the_queue_refuses_is_counted_not_hidden() {
        let db = mem_db();
        let clock = FakeClock::new();
        let id = sched(&db, "t", "a", "{}", T0).await;
        let mut reg = Registry::new();
        let db2 = db.clone();
        reg.register("t", RestartPolicy::RunAgain, move |j| {
            let db2 = db2.clone();
            async move {
                // Something else finishes the job behind the runner's back.
                let jid = j.id;
                db2.call(move |c| queue::complete(c, jid, T0))
                    .await
                    .unwrap();
                Ok(())
            }
        })
        .unwrap();
        let up = boot(&db, &clock, reg, cfg(1)).await;
        wait_state(&db, id, JobState::Done).await;
        linger().await;
        let summary = up.stop_idle().await;
        assert_eq!(
            summary.refused_acks, 1,
            "the runner's own complete was refused"
        );
        assert_eq!(summary.completed, 0);
        assert!(summary.left_running.is_empty());
        assert_eq!(view(&db, id).await.state, JobState::Done);
    }

    // ---- liveness ----

    struct Lcg(u64);
    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    /// Handlers of the hang kinds give up (time out) this many fake seconds
    /// after they start.
    const LIVE_TIMEOUT_SECS: i64 = 400;

    struct Model {
        clock: Arc<FakeClock>,
        runs: Mutex<HashMap<i64, u32>>,
        /// Hung handlers that are running: job id -> fake second at which its
        /// timeout fires.
        hung: Mutex<BTreeMap<i64, i64>>,
        released: AtomicBool,
    }

    impl Model {
        fn new(clock: &Arc<FakeClock>) -> Arc<Model> {
            Arc::new(Model {
                clock: clock.clone(),
                runs: Mutex::default(),
                hung: Mutex::default(),
                released: AtomicBool::new(false),
            })
        }

        /// Ids of hung handlers whose timeout has not fired yet at `now`.
        fn live_hung(&self, now: i64) -> BTreeSet<i64> {
            let hung = self.hung.lock().unwrap();
            hung.iter()
                .filter(|(_, deadline)| now < **deadline)
                .map(|(id, _)| *id)
                .collect()
        }
    }

    fn live_cfg(slots: usize) -> RunnerConfig {
        RunnerConfig {
            handler_timeout: Duration::from_secs(u64::try_from(LIVE_TIMEOUT_SECS).unwrap()),
            ..cfg(slots)
        }
    }

    struct HangGuard(Arc<Model>, i64);
    impl Drop for HangGuard {
        fn drop(&mut self) {
            self.0.hung.lock().unwrap().remove(&self.1);
        }
    }

    const LIVE_KINDS: [(&str, RestartPolicy); 8] = [
        ("t_ok", RestartPolicy::RunAgain),
        ("t_flaky", RestartPolicy::RunAgain),
        ("t_fail", RestartPolicy::RunAgain),
        ("t_panic", RestartPolicy::RunAgain),
        ("t_hang", RestartPolicy::RunAgain),
        ("o_ok", RestartPolicy::AtMostOnce),
        ("o_panic", RestartPolicy::AtMostOnce),
        ("o_hang", RestartPolicy::AtMostOnce),
    ];

    fn live_registry(model: &Arc<Model>) -> Registry {
        let mut reg = Registry::new();
        for (kind, policy) in LIVE_KINDS {
            let model = model.clone();
            reg.register(kind, policy, move |j| {
                let model = model.clone();
                async move {
                    *model.runs.lock().unwrap().entry(j.id).or_default() += 1;
                    match j.kind.as_str() {
                        "t_ok" | "o_ok" => Ok(()),
                        "t_flaky" if j.attempts >= 2 => Ok(()),
                        "t_flaky" => Err(HandlerError::new("flaky")),
                        "t_fail" => Err(HandlerError::new("always_fails")),
                        "t_panic" | "o_panic" => panic!("model panic"),
                        _ => {
                            let deadline = model.clock.secs() + LIVE_TIMEOUT_SECS;
                            model.hung.lock().unwrap().insert(j.id, deadline);
                            let _g = HangGuard(model.clone(), j.id);
                            while !model.released.load(SeqCst) {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                            }
                            Ok(())
                        }
                    }
                }
            })
            .unwrap();
        }
        reg
    }

    /// True once the runner is quiet: no job is `running` except a handler
    /// that is hung and whose timeout has not fired yet, and nothing due waits
    /// unless every slot is taken by such a hung handler. A hung job past its
    /// timeout is NOT excused: the runner must have aborted it by then.
    async fn quiet(db: &tokio_rusqlite::Connection, model: &Model, now: i64, slots: usize) -> bool {
        let hung = model.live_hung(now);
        let hung_n = hung.len();
        db.call(move |c| -> rusqlite::Result<bool> {
            let mut st = c.prepare("SELECT id, state, due_at FROM jobs")?;
            let rows: Vec<(i64, String, i64)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?;
            let stray_running = rows
                .iter()
                .any(|(id, s, _)| s == "running" && !hung.contains(id));
            let due_waiting = rows.iter().any(|(_, s, d)| s == "pending" && *d <= now);
            Ok(!stray_running && (!due_waiting || hung_n >= slots))
        })
        .await
        .unwrap()
    }

    async fn settle(
        db: &tokio_rusqlite::Connection,
        model: &Model,
        clock: &FakeClock,
        slots: usize,
        seed: u64,
    ) {
        for i in 0..10_000 {
            if quiet(db, model, clock.secs(), slots).await {
                return;
            }
            if i % 4 == 3 {
                // Time keeps flowing: a runner that was not yet asleep when a
                // job arrived unannounced finds it at its next poll.
                clock.advance(30);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("seed {seed}: runner went quiet with work unacknowledged");
    }

    #[tokio::test]
    async fn liveness_random_schedule_fail_panic_and_restart_always_reaches_terminal_states() {
        const SLOTS: usize = 3;
        for seed in 1..=12u64 {
            let mut rng = Lcg(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let db = mem_db();
            let clock = FakeClock::new();
            let model = Model::new(&clock);
            let mut up = boot(&db, &clock, live_registry(&model), live_cfg(SLOTS)).await;
            let mut reported_abandoned = BTreeSet::<i64>::new();
            let mut all_ids = Vec::<(i64, &str)>::new();
            for step in 0..40u64 {
                match rng.below(10) {
                    0..=4 => {
                        let kind = if rng.below(12) == 0 {
                            "t_ghost"
                        } else {
                            LIVE_KINDS[usize::try_from(rng.below(8)).unwrap()].0
                        };
                        let due = clock.secs() + i64::try_from(rng.below(40)).unwrap() - 8;
                        let id = sched(&db, kind, &format!("s{seed}-{step}"), "{}", due).await;
                        all_ids.push((id, kind));
                        if rng.below(2) == 0 {
                            up.wake.wake();
                        } else {
                            // No wake: the job is found by the poll interval.
                            clock.advance(30);
                        }
                    }
                    5..=6 => clock.advance(i64::try_from(rng.below(90)).unwrap() + 1),
                    7 => up.wake.wake(),
                    8 => {
                        settle(&db, &model, &clock, SLOTS, seed).await;
                        // A hung handler whose timeout would fire inside the
                        // grace period would end differently from one that is
                        // abandoned: let those time out first.
                        while model
                            .hung
                            .lock()
                            .unwrap()
                            .values()
                            .any(|d| *d <= clock.secs() + 5)
                        {
                            clock.advance(5);
                            settle(&db, &model, &clock, SLOTS, seed).await;
                        }
                        let hung_before: BTreeSet<i64> =
                            model.hung.lock().unwrap().keys().copied().collect();
                        let summary = if hung_before.is_empty() {
                            up.stop_idle().await
                        } else {
                            up.stop_after_grace(5).await
                        };
                        let left: BTreeSet<i64> = summary.left_running.iter().copied().collect();
                        assert_eq!(
                            left, hung_before,
                            "seed {seed}: left_running is exactly the hung jobs"
                        );
                        up = boot(&db, &clock, live_registry(&model), live_cfg(SLOTS)).await;
                        for e in up.report.abandoned() {
                            reported_abandoned.insert(e.id);
                        }
                    }
                    _ => {}
                }
                settle(&db, &model, &clock, SLOTS, seed).await;
                // While the runner is up, whatever is `running` is a live hung handler.
                let hung: Vec<i64> = model.hung.lock().unwrap().keys().copied().collect();
                for id in hung {
                    assert_eq!(view(&db, id).await.state, JobState::Running, "seed {seed}");
                }
            }
            // Drain: release the hung handlers and let time pass until all is terminal.
            model.released.store(true, SeqCst);
            let mut terminal = false;
            for _ in 0..40 {
                clock.advance(700);
                up.wake.wake();
                settle(&db, &model, &clock, SLOTS, seed).await;
                let states = db
                    .call(|c| -> rusqlite::Result<Vec<String>> {
                        let mut st = c.prepare("SELECT state FROM jobs")?;
                        let rows = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
                        Ok(rows)
                    })
                    .await
                    .unwrap();
                if states
                    .iter()
                    .all(|s| matches!(s.as_str(), "done" | "failed" | "abandoned"))
                {
                    terminal = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            assert!(
                terminal,
                "seed {seed}: jobs left non-terminal with the runner up"
            );
            let mut abandoned = BTreeSet::new();
            for (id, kind) in &all_ids {
                let v = view(&db, *id).await;
                assert!(v.state.is_terminal(), "seed {seed} job {id}");
                let runs = model.runs.lock().unwrap().get(id).copied().unwrap_or(0);
                match (*kind, v.state) {
                    ("t_ghost", s) => {
                        assert_eq!(s, JobState::Failed);
                        assert_eq!(v.last_error_code.as_deref(), Some(CODE_NO_HANDLER));
                        assert_eq!(runs, 0);
                    }
                    ("t_ok", s) => assert_eq!(s, JobState::Done, "seed {seed}"),
                    ("t_flaky", s) => assert_eq!(s, JobState::Done, "seed {seed}"),
                    ("t_fail", s) => {
                        assert_eq!(s, JobState::Failed);
                        assert_eq!(
                            (v.attempts, v.last_error_code.as_deref()),
                            (3, Some("always_fails"))
                        );
                    }
                    ("t_panic", s) => {
                        assert_eq!(s, JobState::Failed);
                        assert_eq!(v.last_error_code.as_deref(), Some(CODE_PANICKED));
                    }
                    ("o_panic", s) => {
                        assert_eq!(s, JobState::Failed);
                        assert_eq!(runs, 1, "seed {seed}: at-most-once panic ran twice");
                    }
                    ("o_ok", s) => assert_eq!(s, JobState::Done),
                    ("o_hang", s) => {
                        assert!(
                            matches!(s, JobState::Done | JobState::Abandoned | JobState::Failed),
                            "seed {seed}"
                        );
                        if s == JobState::Failed {
                            assert_eq!(
                                v.last_error_code.as_deref(),
                                Some(CODE_TIMED_OUT),
                                "seed {seed}: an at-most-once hang ends terminal only by timeout"
                            );
                        }
                        assert_eq!(runs, 1, "seed {seed}: at-most-once job ran twice");
                    }
                    ("t_hang", s) => assert!(
                        matches!(s, JobState::Done | JobState::Failed),
                        "seed {seed}"
                    ),
                    other => panic!("unexpected {other:?}"),
                }
                if v.state == JobState::Abandoned {
                    abandoned.insert(*id);
                }
            }
            assert_eq!(
                abandoned, reported_abandoned,
                "seed {seed}: every abandonment was reported"
            );
            up.stop_idle().await;
        }
    }
}

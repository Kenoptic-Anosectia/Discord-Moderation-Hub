//! Activity batching and restart reconciliation over injected facts and storage.
use super::{
    config::Settings,
    policy::{
        self, AnomalyKind, Ledger, MessageEvent, MessageOutcome, VoiceEvent, VoiceOutcome,
        MAX_SNAPSHOT_SEGMENTS,
    },
    repository::{Batch, DailyKey, RepoError, Repository, SavedSession, Store},
};
use crate::{features::Feature, jobs::runner::BoxFuture, settings::Mode};
use poise::serenity_prelude as serenity;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

pub const CHECKPOINT_SECS: i64 = 30;

#[derive(Debug, Clone, Copy)]
pub enum Input {
    Message {
        event: MessageEvent,
        webhook: bool,
        allowed: bool,
    },
    Voice {
        event: VoiceEvent,
        old_allowed: bool,
        new_allowed: bool,
    },
}

pub struct Runtime {
    settings: Settings,
    ledger: Ledger,
    pending: Batch,
    checkpoint: Option<i64>,
    stopped: bool,
}

impl Runtime {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            ledger: Ledger::new(),
            pending: Batch::new(),
            checkpoint: None,
            stopped: false,
        }
    }

    /// Applies one event without calling storage. Returned codes contain no user data.
    pub fn observe(&mut self, input: Input) -> Vec<&'static str> {
        let guild = match input {
            Input::Message { event, .. } => event.guild_id,
            Input::Voice { event, .. } => event.guild_id,
        };
        if self.stopped
            || self.settings.mode() == Mode::Off
            || self.settings.home_guild() != Some(guild)
        {
            return vec![];
        }
        let live = self.settings.mode() == Mode::Live;
        match input {
            Input::Message {
                event,
                webhook: false,
                allowed: true,
            } => {
                if let MessageOutcome::Counted(i) =
                    policy::account_message(&event, &self.settings.policy)
                {
                    if !live {
                        return vec!["activity_shadow_message"];
                    }
                    let counts = self
                        .pending
                        .entry(DailyKey {
                            day: i.utc_day,
                            user: i.user_id,
                            channel: i.channel_id,
                        })
                        .or_default();
                    counts.messages = counts.messages.saturating_add(i.messages as i64);
                }
                vec![]
            }
            Input::Voice {
                mut event,
                old_allowed,
                new_allowed,
            } => {
                if event.old_channel == Some(0)
                    || event.new_channel == Some(0)
                    || event.user_id == 0
                {
                    return self
                        .ledger
                        .apply_voice(&event, &self.settings.policy)
                        .anomalies
                        .iter()
                        .map(policy::Anomaly::code)
                        .collect();
                }
                let tracked = self.ledger.open_channel(guild, event.user_id);
                if ((!old_allowed && event.old_channel == tracked)
                    || (!new_allowed && event.new_channel == tracked))
                    && self
                        .ledger
                        .last_seen_at(guild, event.user_id)
                        .is_none_or(|seen| event.at >= seen)
                {
                    self.ledger.discard_open(guild, event.user_id);
                    event.old_channel = None;
                }
                if !old_allowed {
                    event.old_channel = None;
                }
                if !new_allowed {
                    event.new_channel = None;
                }
                if event.old_channel.is_none() && event.new_channel.is_none() {
                    return vec![];
                }
                let outcome = self.ledger.apply_voice(&event, &self.settings.policy);
                let mut codes: Vec<_> = outcome
                    .anomalies
                    .iter()
                    .map(policy::Anomaly::code)
                    .collect();
                if live {
                    add_voice(&mut self.pending, outcome);
                } else if !outcome.sessions.is_empty() {
                    codes.push("activity_shadow_voice");
                }
                codes
            }
            Input::Message { .. } => vec![],
        }
    }

    /// First tick reconciles; subsequent ticks checkpoint when due. Call at startup
    /// and periodically, even if no events arrived. Retry errors without replacing the runtime.
    pub fn tick(
        &mut self,
        now: i64,
        store: &mut impl Store,
    ) -> Result<Vec<&'static str>, RepoError> {
        self.drive(now, store, false, |_, _| true)
    }

    /// Closes live sessions and flushes pending totals; retry on error.
    pub fn shutdown(
        &mut self,
        now: i64,
        store: &mut impl Store,
    ) -> Result<Vec<&'static str>, RepoError> {
        self.drive(now, store, true, |_, _| true)
    }

    fn drive(
        &mut self,
        now: i64,
        store: &mut impl Store,
        shutdown: bool,
        allowed: impl Fn(u64, u64) -> bool,
    ) -> Result<Vec<&'static str>, RepoError> {
        let Some(guild) = self
            .settings
            .home_guild()
            .filter(|_| self.settings.mode() == Mode::Live && !self.stopped)
        else {
            self.stopped |= shutdown;
            return Ok(vec![]);
        };
        let mut codes = Vec::new();
        if self.checkpoint.is_none() {
            // This transaction consumes old rows and credits their totals together.
            // A failed attempt cannot acknowledge either half.
            codes = self.reconcile(now, store, guild, &allowed)?;
            self.checkpoint = Some(now);
        }
        let previous = self.checkpoint.unwrap_or(now);
        if now < previous && !shutdown {
            return Err(RepoError::ClockRegression);
        }
        let elapsed = now.saturating_sub(previous);
        if !shutdown && elapsed < CHECKPOINT_SECS {
            return Ok(codes);
        }

        let mut open = Vec::new();
        for snapshot in self.ledger.snapshot_open() {
            if !allowed(guild, snapshot.channel_id) {
                self.ledger.discard_open(guild, snapshot.user_id);
                codes.push("snapshot_out_of_scope");
            } else if !shutdown {
                if snapshot.segments.len() > MAX_SNAPSHOT_SEGMENTS {
                    codes.push("snapshot_too_many_segments");
                } else if snapshot.segments.last().is_some_and(|s| s.end > now) {
                    codes.push("activity_clock_regression");
                } else {
                    open.push(SavedSession {
                        snapshot,
                        last_seen_at: now,
                    });
                }
            }
        }
        if shutdown {
            // Work on a copy: if commit fails, the open sessions remain retryable.
            let mut ledger = self.ledger.clone();
            let mut batch = self.pending.clone();
            ledger.clamp_open_for_restart(now);
            let outcome = ledger.note_restart(now, now, &self.settings.policy);
            codes.extend(
                outcome
                    .anomalies
                    .iter()
                    .filter(|a| a.kind != AnomalyKind::RestartGap)
                    .map(policy::Anomaly::code),
            );
            add_voice(&mut batch, outcome);
            store.commit(guild, &batch, &[])?;
            self.ledger = ledger;
            self.stopped = true;
        } else {
            store.commit(guild, &self.pending, &open)?;
            // Stay on the original cadence after a late wake or a retry.
            self.checkpoint = Some(now - elapsed % CHECKPOINT_SECS);
        }
        self.pending.clear();
        Ok(codes)
    }

    fn reconcile(
        &self,
        now: i64,
        store: &mut impl Store,
        guild: u64,
        allowed: &impl Fn(u64, u64) -> bool,
    ) -> Result<Vec<&'static str>, RepoError> {
        let mut recovered = Batch::new();
        let mut codes = Vec::new();
        let mut seen = BTreeSet::new();
        for row in store.load(guild)? {
            let SavedSession {
                snapshot,
                last_seen_at,
            } = match row {
                Ok(saved) => saved,
                Err(code) => {
                    codes.push(code);
                    continue;
                }
            };
            if snapshot.guild_id != guild || !allowed(guild, snapshot.channel_id) {
                codes.push("snapshot_out_of_scope");
                continue;
            }
            let user = snapshot.user_id;
            let future_segment = snapshot.segments.iter().any(|s| s.end > last_seen_at);
            match Ledger::restore([snapshot]) {
                Ok(mut ledger) if !future_segment => {
                    if !seen.insert(user) {
                        codes.push("snapshot_duplicate_open");
                        continue;
                    }
                    ledger.clamp_open_for_restart(now.min(last_seen_at));
                    let outcome = ledger.note_restart(now, last_seen_at, &self.settings.policy);
                    codes.extend(outcome.anomalies.iter().map(policy::Anomaly::code));
                    add_voice(&mut recovered, outcome);
                }
                Ok(_) => codes.push("snapshot_malformed"),
                Err(error) => codes.push(error.code()),
            }
        }
        store.commit(guild, &recovered, &[])?;
        Ok(codes)
    }
}

fn add_voice(batch: &mut Batch, outcome: VoiceOutcome) {
    for session in outcome.sessions {
        let counts = batch
            .entry(DailyKey {
                day: policy::utc_day(session.start),
                user: session.user_id,
                channel: session.channel_id,
            })
            .or_default();
        counts.voice_secs = counts.voice_secs.saturating_add(session.seconds as i64);
    }
}

/// W5 supplies its clock, fail-closed privacy decision (including parent/category and
/// ticket scope), and code-only diagnostic sink. No message content crosses this port.
pub trait Ports: Send + Sync + 'static {
    fn now(&self) -> i64;
    fn channel_allowed(&self, guild: u64, channel: u64) -> bool;
    fn report(&self, code: &'static str);
}

pub struct ActivityFeature {
    settings: Settings,
    runtime: Arc<Mutex<Runtime>>,
    ports: Arc<dyn Ports>,
}

impl ActivityFeature {
    pub fn new(settings: Settings, ports: impl Ports) -> Self {
        Self {
            runtime: Arc::new(Mutex::new(Runtime::new(settings.clone()))),
            settings,
            ports: Arc::new(ports),
        }
    }

    /// Call on startup and at least every `CHECKPOINT_SECS`, independent of gateway traffic.
    pub async fn tick(&self, db: Option<&tokio_rusqlite::Connection>) -> Result<(), RepoError> {
        self.drive(db, false).await
    }

    /// Stop dispatch first; await this before closing the core database.
    pub async fn shutdown(&self, db: Option<&tokio_rusqlite::Connection>) -> Result<(), RepoError> {
        self.drive(db, true).await
    }

    async fn drive(
        &self,
        db: Option<&tokio_rusqlite::Connection>,
        shutdown: bool,
    ) -> Result<(), RepoError> {
        if self.settings.mode() != Mode::Live {
            return Ok(());
        }
        let result = if let Some(db) = db {
            let runtime = Arc::clone(&self.runtime);
            let ports = Arc::clone(&self.ports);
            match db
                .call(move |conn| -> Result<_, rusqlite::Error> {
                    Ok((|| {
                        let mut runtime =
                            runtime.lock().map_err(|_| RepoError::StateUnavailable)?;
                        let mut store = Repository::new(conn);
                        if runtime.checkpoint.is_none() {
                            store.migrate()?;
                        }
                        runtime.drive(ports.now(), &mut store, shutdown, |guild, channel| {
                            ports.channel_allowed(guild, channel)
                        })
                    })())
                })
                .await
            {
                Ok(result) => result,
                Err(_) => Err(RepoError::ConnectionUnavailable),
            }
        } else {
            Err(RepoError::ConnectionUnavailable)
        };
        match result {
            Ok(codes) => {
                for code in codes {
                    self.ports.report(code);
                }
                Ok(())
            }
            Err(error) => {
                self.ports.report(error.code());
                Err(error)
            }
        }
    }

    fn accept(&self, event: &serenity::FullEvent, routed: Option<u64>) {
        let Some(guild) = self
            .settings
            .home_guild()
            .filter(|_| self.settings.mode() != Mode::Off)
        else {
            return;
        };
        if routed != Some(guild) {
            return;
        }
        let code_result = {
            let Ok(mut runtime) = self.runtime.lock() else {
                self.ports.report(RepoError::StateUnavailable.code());
                return;
            };
            match event {
                serenity::FullEvent::Message { new_message: m }
                    if m.guild_id.is_some_and(|id| id.get() == guild) =>
                {
                    let channel_id = m.channel_id.get();
                    runtime.observe(Input::Message {
                        event: MessageEvent {
                            guild_id: guild,
                            user_id: m.author.id.get(),
                            channel_id,
                            is_bot: m.author.bot,
                            at: m.timestamp.unix_timestamp(),
                        },
                        webhook: m.webhook_id.is_some(),
                        allowed: self.ports.channel_allowed(guild, channel_id),
                    })
                }
                serenity::FullEvent::VoiceStateUpdate { old, new }
                    if new.guild_id.is_some_and(|id| id.get() == guild) =>
                {
                    let old_channel = old
                        .as_ref()
                        .and_then(|state| state.channel_id)
                        .map(|id| id.get());
                    let new_channel = new.channel_id.map(|id| id.get());
                    let allowed = |channel: Option<u64>| {
                        channel.is_none_or(|id| self.ports.channel_allowed(guild, id))
                    };
                    runtime.observe(Input::Voice {
                        event: VoiceEvent {
                            guild_id: guild,
                            user_id: new.user_id.get(),
                            old_channel,
                            new_channel,
                            self_deaf: new.self_deaf,
                            at: self.ports.now(),
                        },
                        old_allowed: allowed(old_channel),
                        new_allowed: allowed(new_channel),
                    })
                }
                _ => return,
            }
        };
        for code in code_result {
            self.ports.report(code);
        }
    }
}

impl<X, D> Feature<X, D> for ActivityFeature {
    fn name(&self) -> &'static str {
        "activity"
    }
    fn enabled(&self) -> bool {
        self.settings.mode() != Mode::Off
    }
    fn needs_core_db(&self) -> bool {
        self.settings.mode() == Mode::Live
    }

    fn handle<'a>(
        &'a self,
        _: &'a X,
        event: &'a serenity::FullEvent,
        _: &'a D,
        guild: Option<u64>,
        _: Option<&'a tokio_rusqlite::Connection>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.accept(event, guild);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{config, policy as p};
    use rusqlite::Connection;

    fn runtime(home: Option<u64>) -> Runtime {
        Runtime::new(
            config::parse(
                |key| match key {
                    p::KEY_MODE => Some("live".into()),
                    p::KEY_EXCLUDED_CHANNEL_IDS => Some("4".into()),
                    p::KEY_AFK_CHANNEL_ID => Some("5".into()),
                    _ => None,
                },
                home,
            )
            .unwrap(),
        )
    }

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        Repository::new(&mut conn).migrate().unwrap();
        conn
    }

    fn message(guild: u64, channel: u64, bot: bool, webhook: bool, allowed: bool) -> Input {
        Input::Message {
            event: MessageEvent {
                guild_id: guild,
                user_id: 2,
                channel_id: channel,
                is_bot: bot,
                at: 0,
            },
            webhook,
            allowed,
        }
    }

    fn voice(
        at: i64,
        old_channel: Option<u64>,
        new_channel: Option<u64>,
        self_deaf: bool,
    ) -> Input {
        Input::Voice {
            event: VoiceEvent {
                guild_id: 1,
                user_id: 2,
                old_channel,
                new_channel,
                self_deaf,
                at,
            },
            old_allowed: true,
            new_allowed: true,
        }
    }

    fn totals(conn: &Connection) -> (i64, i64) {
        conn.query_row(
            "SELECT coalesce(sum(messages),0), coalesce(sum(voice_secs),0) FROM activity_daily",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    fn tick(rt: &mut Runtime, now: i64, conn: &mut Connection) -> Vec<&'static str> {
        rt.tick(now, &mut Repository::new(conn)).unwrap()
    }

    #[test]
    fn ac2_exclusions_and_clean_shutdown_batch() {
        let mut conn = db();
        let mut rt = runtime(Some(1));
        tick(&mut rt, 0, &mut conn);
        for input in [
            message(1, 4, false, false, true),
            message(1, 6, false, false, false),
            message(1, 7, false, false, false),
            message(1, 3, true, false, true),
            message(1, 3, false, true, true),
            message(1, 3, false, true, true),
            message(1, 3, false, false, true),
            message(2, 3, false, false, true),
        ] {
            rt.observe(input);
        }
        rt.observe(voice(1, None, Some(5), false));
        assert_eq!(
            rt.observe(voice(2, Some(5), Some(3), false)),
            ["unaccounted_channel"]
        );
        tick(&mut rt, CHECKPOINT_SECS - 1, &mut conn);
        assert_eq!(totals(&conn), (0, 0));
        rt.shutdown(CHECKPOINT_SECS - 1, &mut Repository::new(&mut conn))
            .unwrap();
        assert_eq!(totals(&conn), (1, CHECKPOINT_SECS - 3));
    }

    #[test]
    fn ac2_heartbeat_recovers_deafened_segments_and_splits_midnight() {
        let mut conn = db();
        let mut rt = runtime(Some(1));
        let midnight = p::SECONDS_PER_DAY;
        tick(&mut rt, midnight - 20, &mut conn);
        rt.observe(voice(midnight - 10, None, Some(3), false));
        rt.observe(voice(midnight - 5, Some(3), Some(3), true));
        rt.observe(voice(midnight + 3, Some(3), Some(3), false));
        tick(&mut rt, midnight + 10, &mut conn);
        assert_eq!(totals(&conn), (0, 0));
        rt.observe(voice(midnight + 11, Some(3), None, false));
        drop(rt);
        let mut rt = runtime(Some(1));
        assert_eq!(tick(&mut rt, midnight + 99_000, &mut conn), ["restart_gap"]);
        let daily = conn
            .prepare("SELECT day,voice_secs FROM activity_daily ORDER BY day")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(daily, [("1970-01-01".into(), 5), ("1970-01-02".into(), 7)]);
        assert!(tick(&mut runtime(Some(1)), midnight + 100_000, &mut conn).is_empty());
        assert_eq!(totals(&conn), (0, 12));
    }

    #[test]
    fn ac2_failed_checkpoint_rolls_back_daily_and_keeps_pending() {
        let mut conn = db();
        let mut rt = runtime(Some(1));
        tick(&mut rt, 0, &mut conn);
        rt.observe(message(1, 3, false, false, true));
        tick(&mut rt, CHECKPOINT_SECS, &mut conn);
        rt.observe(message(1, 3, false, false, true));
        rt.observe(voice(CHECKPOINT_SECS + 1, None, Some(3), false));
        conn.execute_batch("CREATE TRIGGER reject BEFORE INSERT ON voice_sessions BEGIN SELECT raise(FAIL,'fault'); END;").unwrap();
        assert_eq!(
            rt.tick(2 * CHECKPOINT_SECS, &mut Repository::new(&mut conn)),
            Err(RepoError::Sqlite)
        );
        assert_eq!(totals(&conn), (1, 0));
        conn.execute_batch("DROP TRIGGER reject").unwrap();
        tick(&mut rt, 2 * CHECKPOINT_SECS + 1, &mut conn);
        assert_eq!(totals(&conn), (2, 0));
    }

    #[test]
    fn ac2_corrupt_session_drops_without_discarding_valid_neighbor() {
        let mut conn = db();
        conn.execute_batch("INSERT INTO voice_sessions VALUES ('1','2','3',0,10,'0:0:0'), ('1','8','3',0,10,'0:0:2'), ('1','9','3',0,10,'0:5:0;3:3:0'), ('2','2','3',0,10,'0:0:0');").unwrap();
        let mut codes = tick(&mut runtime(Some(1)), 50, &mut conn);
        codes.sort_unstable();
        assert_eq!(
            codes,
            [
                "restart_gap",
                "snapshot_malformed",
                "snapshot_segment_order"
            ]
        );
        assert_eq!(totals(&conn), (0, 10));
        assert_eq!(
            conn.query_row("SELECT guild FROM voice_sessions", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "2"
        );
    }

    #[test]
    fn ac2_unset_or_zero_home_does_not_touch_store() {
        struct ForbiddenStore;
        impl Store for ForbiddenStore {
            fn load(
                &mut self,
                _: u64,
            ) -> Result<Vec<Result<SavedSession, &'static str>>, RepoError> {
                panic!("unexpected read")
            }
            fn commit(&mut self, _: u64, _: &Batch, _: &[SavedSession]) -> Result<(), RepoError> {
                panic!("unexpected write")
            }
        }
        for home in [None, Some(0)] {
            let mut rt = runtime(home);
            assert!(rt.observe(message(1, 3, false, false, true)).is_empty());
            assert!(rt.tick(50, &mut ForbiddenStore).unwrap().is_empty());
            assert!(rt.shutdown(51, &mut ForbiddenStore).unwrap().is_empty());
        }
    }

    #[test]
    fn ac2_seeded_sqlite_checkpoints_restart_and_drain() {
        for seed in 1..=8u64 {
            let mut rng = seed;
            let mut conn = db();
            let mut rt = runtime(Some(1));
            let mut owed = 0;
            let mut durable = 0;
            tick(&mut rt, 0, &mut conn);
            for step in 1..=80 {
                let now = step * CHECKPOINT_SECS;
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                let count = ((rng >> 32) % 5) as i64 + 1;
                for _ in 0..count {
                    rt.observe(message(1, 3, false, false, true));
                }
                owed += count;
                rt.observe(message(2, 3, false, false, true));
                match (rng >> 40) % 4 {
                    0 => {
                        rt = runtime(Some(1));
                        owed = 0;
                        tick(&mut rt, now, &mut conn);
                    }
                    1 => {
                        conn.execute_batch("CREATE TRIGGER reject_insert BEFORE INSERT ON activity_daily BEGIN SELECT raise(FAIL,'fault'); END;").unwrap();
                        assert_eq!(
                            rt.tick(now, &mut Repository::new(&mut conn)),
                            Err(RepoError::Sqlite)
                        );
                        conn.execute_batch("DROP TRIGGER reject_insert").unwrap();
                        tick(&mut rt, now + 1, &mut conn);
                        durable += owed;
                        owed = 0;
                    }
                    _ => {
                        // A retry at now+1 may shift the next due time by one second.
                        tick(&mut rt, now + 1, &mut conn);
                        durable += owed;
                        owed = 0;
                    }
                }
                assert_eq!(totals(&conn), (durable, 0));
            }
            rt.shutdown(81 * CHECKPOINT_SECS, &mut Repository::new(&mut conn))
                .unwrap();
            assert_eq!(totals(&conn), (durable + owed, 0));
        }
    }
}

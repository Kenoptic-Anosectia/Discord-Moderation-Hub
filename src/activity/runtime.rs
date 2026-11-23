//! Activity batching and restart reconciliation over injected facts and storage.
use super::{
    config::Settings,
    policy::{MessageEvent, VoiceEvent},
    repository::{RepoError, Store},
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

pub struct Runtime(Settings);

impl Runtime {
    pub fn new(settings: Settings) -> Self {
        Self(settings)
    }
    pub fn observe(&mut self, _: Input) -> Vec<&'static str> {
        vec![]
    }
    pub fn tick(&mut self, _: i64, _: &mut impl Store) -> Result<Vec<&'static str>, RepoError> {
        Ok(vec![])
    }
    pub fn shutdown(&mut self, _: i64, _: &mut impl Store) -> Result<Vec<&'static str>, RepoError> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{
        config, policy as p,
        repository::{Batch, Repository, SavedSession},
    };
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

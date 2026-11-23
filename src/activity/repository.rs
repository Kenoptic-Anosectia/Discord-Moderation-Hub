//! SQLite activity storage on the shared core database.
use super::policy::OpenSnapshot;
use rusqlite::Connection;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DailyKey {
    pub day: String,
    pub user: u64,
    pub channel: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub messages: i64,
    pub voice_secs: i64,
}

pub type Batch = BTreeMap<DailyKey, Counts>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSession {
    pub snapshot: OpenSnapshot,
    pub last_seen_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoError {
    Sqlite,
}

pub trait Store {
    fn load(&mut self, guild: u64) -> Result<Vec<Result<SavedSession, &'static str>>, RepoError>;
    fn commit(&mut self, guild: u64, batch: &Batch, open: &[SavedSession])
        -> Result<(), RepoError>;
}

pub struct Repository<'a>(&'a mut Connection);

impl<'a> Repository<'a> {
    pub fn new(conn: &'a mut Connection) -> Self {
        Self(conn)
    }

    pub fn migrate(&mut self) -> Result<(), RepoError> {
        Ok(())
    }
}

impl Store for Repository<'_> {
    fn load(&mut self, _: u64) -> Result<Vec<Result<SavedSession, &'static str>>, RepoError> {
        Ok(vec![])
    }

    fn commit(&mut self, _: u64, _: &Batch, _: &[SavedSession]) -> Result<(), RepoError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ac2_schema_has_only_numeric_facts_and_day_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        Repository::new(&mut conn).migrate().unwrap();
        for (table, expected) in [
            (
                "activity_daily",
                &["day", "guild", "user", "channel", "messages", "voice_secs"][..],
            ),
            (
                "voice_sessions",
                &[
                    "guild",
                    "user",
                    "channel",
                    "started_at",
                    "last_seen_at",
                    "segments",
                ][..],
            ),
        ] {
            let columns = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(columns, expected);
        }
        assert!(conn
            .execute(
                "INSERT INTO voice_sessions VALUES ('1','2','3',0,1,'letters')",
                []
            )
            .is_err());
        assert!(conn
            .execute(
                "INSERT INTO activity_daily VALUES ('hello-text','1','2','3',0,0)",
                []
            )
            .is_err());
    }
}

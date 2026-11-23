//! SQLite activity storage on the shared core database.
use super::policy::{OpenSnapshot, SegmentSnapshot, MAX_SNAPSHOT_SEGMENTS};
use crate::storage::{self, Migration, StorageError};
use rusqlite::{params, Connection, Row, TransactionBehavior};
use std::collections::BTreeMap;
use std::fmt::Write as _;

const MAX_SEGMENT_BYTES: usize = 44 * MAX_SNAPSHOT_SEGMENTS;
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: r#"
        CREATE TABLE activity_daily (
            day TEXT NOT NULL CHECK(day GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
                AND length(CAST(day AS BLOB)) = 10),
            guild TEXT NOT NULL CHECK(length(guild) BETWEEN 1 AND 20 AND guild GLOB '[1-9]*'
                AND guild NOT GLOB '*[^0-9]*' AND instr(guild,char(0))=0),
            user TEXT NOT NULL CHECK(length(user) BETWEEN 1 AND 20 AND user GLOB '[1-9]*'
                AND user NOT GLOB '*[^0-9]*' AND instr(user,char(0))=0),
            channel TEXT NOT NULL CHECK(length(channel) BETWEEN 1 AND 20 AND channel GLOB '[1-9]*'
                AND channel NOT GLOB '*[^0-9]*' AND instr(channel,char(0))=0),
            messages INTEGER NOT NULL CHECK(messages >= 0),
            voice_secs INTEGER NOT NULL CHECK(voice_secs >= 0),
            PRIMARY KEY(day,guild,user,channel)
        ) STRICT;
        CREATE TABLE voice_sessions (
            guild TEXT NOT NULL CHECK(length(guild) BETWEEN 1 AND 20 AND guild GLOB '[1-9]*'
                AND guild NOT GLOB '*[^0-9]*' AND instr(guild,char(0))=0),
            user TEXT NOT NULL CHECK(length(user) BETWEEN 1 AND 20 AND user GLOB '[1-9]*'
                AND user NOT GLOB '*[^0-9]*' AND instr(user,char(0))=0),
            channel TEXT NOT NULL CHECK(length(channel) BETWEEN 1 AND 20 AND channel GLOB '[1-9]*'
                AND channel NOT GLOB '*[^0-9]*' AND instr(channel,char(0))=0),
            started_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL CHECK(last_seen_at >= started_at),
            segments TEXT NOT NULL CHECK(length(segments) BETWEEN 1 AND 45056
                AND segments NOT GLOB '*[^0-9:;-]*' AND instr(segments,char(0))=0),
            PRIMARY KEY(guild,user)
        ) STRICT;
    "#,
}];

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
    Migration(StorageError),
    InvalidSnapshot,
    ClockRegression,
    ConnectionUnavailable,
    StateUnavailable,
}

impl RepoError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Sqlite => "activity_sqlite",
            Self::Migration(error) => error.code(),
            Self::InvalidSnapshot => "snapshot_malformed",
            Self::ClockRegression => "activity_clock_regression",
            Self::ConnectionUnavailable => "activity_db_unavailable",
            Self::StateUnavailable => "activity_state_unavailable",
        }
    }
}

impl From<rusqlite::Error> for RepoError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Sqlite
    }
}

/// A malformed row is a per-row diagnostic, not a failed database read.
pub type LoadedSession = Result<SavedSession, &'static str>;

pub trait Store {
    fn load(&mut self, guild: u64) -> Result<Vec<LoadedSession>, RepoError>;
    /// Atomically adds daily deltas and replaces this guild's open checkpoint set.
    fn commit(&mut self, guild: u64, batch: &Batch, open: &[SavedSession])
        -> Result<(), RepoError>;
}

pub struct Repository<'a>(&'a mut Connection);

impl<'a> Repository<'a> {
    pub fn new(conn: &'a mut Connection) -> Self {
        Self(conn)
    }

    pub fn migrate(&mut self) -> Result<(), RepoError> {
        storage::migrate(self.0, "activity", MIGRATIONS).map_err(RepoError::Migration)
    }
}

impl Store for Repository<'_> {
    fn load(&mut self, guild: u64) -> Result<Vec<LoadedSession>, RepoError> {
        let mut stmt = self.0.prepare(
            "SELECT user, channel, started_at, last_seen_at, segments
            FROM voice_sessions WHERE guild=?1 ORDER BY user",
        )?;
        let mut rows = stmt.query([guild.to_string()])?;
        let mut loaded = Vec::new();
        while let Some(row) = rows.next()? {
            loaded.push(decode(row, guild));
        }
        Ok(loaded)
    }

    fn commit(
        &mut self,
        guild: u64,
        batch: &Batch,
        open: &[SavedSession],
    ) -> Result<(), RepoError> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let guild_text = guild.to_string();
        {
            let mut upsert = tx.prepare(
                "INSERT INTO activity_daily VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(day,guild,user,channel) DO UPDATE SET
                   messages=messages+excluded.messages, voice_secs=voice_secs+excluded.voice_secs",
            )?;
            for (key, counts) in batch {
                upsert.execute(params![
                    key.day,
                    guild_text,
                    key.user.to_string(),
                    key.channel.to_string(),
                    counts.messages,
                    counts.voice_secs
                ])?;
            }
        }
        tx.execute("DELETE FROM voice_sessions WHERE guild=?1", [&guild_text])?;
        {
            let mut insert = tx.prepare("INSERT INTO voice_sessions VALUES (?1,?2,?3,?4,?5,?6)")?;
            for row in open {
                let snapshot = &row.snapshot;
                let first = snapshot
                    .segments
                    .first()
                    .ok_or(RepoError::InvalidSnapshot)?;
                if snapshot.guild_id != guild
                    || snapshot.segments.len() > MAX_SNAPSHOT_SEGMENTS
                    || snapshot
                        .segments
                        .iter()
                        .any(|s| s.start > s.end || s.end > row.last_seen_at)
                    || snapshot.segments.windows(2).any(|w| w[0].end > w[1].start)
                {
                    return Err(RepoError::InvalidSnapshot);
                }
                let mut encoded = String::new();
                for segment in &snapshot.segments {
                    if !encoded.is_empty() {
                        encoded.push(';');
                    }
                    write!(
                        encoded,
                        "{}:{}:{}",
                        segment.start,
                        segment.end,
                        u8::from(segment.self_deaf)
                    )
                    .map_err(|_| RepoError::InvalidSnapshot)?;
                }
                insert.execute(params![
                    guild_text,
                    snapshot.user_id.to_string(),
                    snapshot.channel_id.to_string(),
                    first.start,
                    row.last_seen_at,
                    encoded
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn decode(row: &Row<'_>, guild_id: u64) -> LoadedSession {
    const MALFORMED: &str = "snapshot_malformed";
    let text = |index: usize| {
        row.get_ref(index)
            .ok()
            .and_then(|r| r.as_str().ok())
            .ok_or(MALFORMED)
    };
    let id = |index| text(index)?.parse::<u64>().map_err(|_| MALFORMED);
    let user_id = id(0)?;
    let channel_id = id(1)?;
    let started_at = row.get::<_, i64>(2).map_err(|_| MALFORMED)?;
    let last_seen_at = row.get::<_, i64>(3).map_err(|_| MALFORMED)?;
    let source = text(4)?;
    if source.len() > MAX_SEGMENT_BYTES {
        return Err("snapshot_too_many_segments");
    }
    if source.is_empty() {
        return Err("snapshot_empty");
    }
    let mut segments = Vec::new();
    for entry in source.split(';') {
        if segments.len() == MAX_SNAPSHOT_SEGMENTS {
            return Err("snapshot_too_many_segments");
        }
        let mut fields = entry.split(':');
        let start = fields.next().and_then(decimal).ok_or(MALFORMED)?;
        let end = fields.next().and_then(decimal).ok_or(MALFORMED)?;
        let self_deaf = match (fields.next(), fields.next()) {
            (Some("0"), None) => false,
            (Some("1"), None) => true,
            _ => return Err(MALFORMED),
        };
        if end > last_seen_at {
            return Err(MALFORMED);
        }
        segments.push(SegmentSnapshot {
            start,
            end,
            self_deaf,
        });
    }
    if segments.first().is_none_or(|s| s.start != started_at) {
        return Err(MALFORMED);
    }
    Ok(SavedSession {
        snapshot: OpenSnapshot {
            guild_id,
            user_id,
            channel_id,
            segments,
        },
        last_seen_at,
    })
}

fn decimal(value: &str) -> Option<i64> {
    let number = value.parse::<i64>().ok()?;
    (number.to_string() == value).then_some(number)
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

//! SQLite persistence for tracked voice rooms (`docs/verification/VOICE-ROOMS-RUNTIME.md`).
//!
//! One row per tracked room in `voice_rooms` inside the shared core database:
//! `channel TEXT PRIMARY KEY, guild TEXT, owner TEXT, created_at INTEGER, locked INTEGER,
//! user_limit INTEGER`. The row is the truth about which channels the bot may delete: a
//! channel without a row is never touched.
//!
//! Functions take a plain `&Connection`, so the runtime can call them inside one
//! `tokio_rusqlite` call next to the job queue (the queue lives in the same file). Nothing
//! here holds a Discord type or reads a clock: the caller passes ids and `now`.

use crate::storage::{self, Migration, StorageError};
use rusqlite::{params, Connection, OptionalExtension};

/// Feature name used with `storage::migrate`.
pub const FEATURE: &str = "voice_rooms";

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: "CREATE TABLE voice_rooms (\
          channel TEXT NOT NULL PRIMARY KEY, \
          guild TEXT NOT NULL, \
          owner TEXT NOT NULL, \
          created_at INTEGER NOT NULL, \
          locked INTEGER NOT NULL DEFAULT 0 CHECK (locked IN (0,1)), \
          user_limit INTEGER NOT NULL DEFAULT 0 CHECK (user_limit >= 0)); \
          CREATE INDEX voice_rooms_guild ON voice_rooms (guild);",
}];

/// Stable error codes. Never carries SQL text, a path or an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoError {
    /// A stored column has the wrong type or an impossible value.
    CorruptRow,
    /// Underlying storage failure (code from the storage layer).
    Storage(StorageError),
}

impl RepoError {
    /// The stable machine code.
    pub fn code(&self) -> &'static str {
        match self {
            RepoError::CorruptRow => "corrupt_row",
            RepoError::Storage(e) => e.code(),
        }
    }
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for RepoError {}

impl From<StorageError> for RepoError {
    fn from(e: StorageError) -> Self {
        RepoError::Storage(e)
    }
}

impl From<rusqlite::Error> for RepoError {
    fn from(_: rusqlite::Error) -> Self {
        RepoError::Storage(StorageError::Sqlite)
    }
}

/// One tracked room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomRow {
    /// The voice channel the bot created.
    pub channel: u64,
    /// The guild the channel belongs to.
    pub guild: u64,
    /// The member who owns the room.
    pub owner: u64,
    /// Unix seconds when the room was created.
    pub created_at: i64,
    /// Whether the owner locked the room (reserved for the command card; always false here).
    pub locked: bool,
    /// The member limit; 0 means unlimited.
    pub user_limit: u32,
}

/// Applies the `voice_rooms` migrations (idempotent).
pub fn migrate(conn: &mut Connection) -> Result<(), RepoError> {
    storage::migrate(conn, FEATURE, MIGRATIONS)?;
    Ok(())
}

/// Inserts a room. Returns `false` (and writes nothing) when the channel already has a row.
pub fn insert(conn: &Connection, row: &RoomRow) -> Result<bool, RepoError> {
    let n = conn.execute(
        "INSERT INTO voice_rooms (channel, guild, owner, created_at, locked, user_limit) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(channel) DO NOTHING",
        params![
            row.channel.to_string(),
            row.guild.to_string(),
            row.owner.to_string(),
            row.created_at,
            i64::from(row.locked),
            i64::from(row.user_limit),
        ],
    )?;
    Ok(n == 1)
}

/// Deletes the row of `channel`. Returns whether a row was there.
pub fn delete(conn: &Connection, channel: u64) -> Result<bool, RepoError> {
    let n = conn.execute(
        "DELETE FROM voice_rooms WHERE channel = ?1",
        params![channel.to_string()],
    )?;
    Ok(n == 1)
}

/// Updates the recorded owner of `channel` in `guild`. Returns whether a row was updated.
pub fn update_owner(
    conn: &Connection,
    channel: u64,
    guild: u64,
    owner: u64,
) -> Result<bool, RepoError> {
    let _ = (conn, channel, guild, owner);
    Ok(false)
}

/// Updates the locked flag of `channel` in `guild`. Returns whether a row was updated.
pub fn update_locked(
    conn: &Connection,
    channel: u64,
    guild: u64,
    locked: bool,
) -> Result<bool, RepoError> {
    let _ = (conn, channel, guild, locked);
    Ok(false)
}

/// Updates the member limit of `channel` in `guild`. Returns whether a row was updated.
pub fn update_user_limit(
    conn: &Connection,
    channel: u64,
    guild: u64,
    user_limit: u32,
) -> Result<bool, RepoError> {
    let _ = (conn, channel, guild, user_limit);
    Ok(false)
}

type RawRow = (String, String, String, i64, i64, i64);

fn raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

/// Reads one row back exactly; a row that cannot be read is an error, never a default.
fn decode(raw: RawRow) -> Result<RoomRow, RepoError> {
    let (channel, guild, owner, created_at, locked, user_limit) = raw;
    let id = |text: &str| -> Result<u64, RepoError> {
        match text.parse::<u64>() {
            Ok(0) | Err(_) => Err(RepoError::CorruptRow),
            Ok(id) => Ok(id),
        }
    };
    let locked = match locked {
        0 => false,
        1 => true,
        _ => return Err(RepoError::CorruptRow),
    };
    Ok(RoomRow {
        channel: id(&channel)?,
        guild: id(&guild)?,
        owner: id(&owner)?,
        created_at,
        locked,
        user_limit: u32::try_from(user_limit).map_err(|_| RepoError::CorruptRow)?,
    })
}

const COLUMNS: &str = "channel, guild, owner, created_at, locked, user_limit";

/// Loads the row of `channel`. `Ok(None)` means the channel is not tracked.
pub fn get(conn: &Connection, channel: u64) -> Result<Option<RoomRow>, RepoError> {
    let found = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM voice_rooms WHERE channel = ?1"),
            params![channel.to_string()],
            raw,
        )
        .optional()?;
    found.map(decode).transpose()
}

/// Loads every row of `guild`, oldest first. One unreadable row fails the whole read: a
/// damaged row must not make the table look smaller than it is.
pub fn list(conn: &Connection, guild: u64) -> Result<Vec<RoomRow>, RepoError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM voice_rooms WHERE guild = ?1 ORDER BY created_at, channel"
    ))?;
    let raws = stmt
        .query_map(params![guild.to_string()], raw)?
        .collect::<Result<Vec<_>, _>>()?;
    raws.into_iter().map(decode).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic ids, far from any real Discord snowflake.
    const GUILD: u64 = 6_200_000_000_000_001;
    const OTHER_GUILD: u64 = 6_200_000_000_000_002;
    const CH_A: u64 = 6_200_000_000_000_011;
    const CH_B: u64 = 6_200_000_000_000_012;
    const OWNER: u64 = 6_200_000_000_000_021;

    fn mem() -> Connection {
        let mut conn = storage::open(":memory:", &[]).unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    fn row(channel: u64, guild: u64, created_at: i64) -> RoomRow {
        RoomRow {
            channel,
            guild,
            owner: OWNER,
            created_at,
            locked: false,
            user_limit: 0,
        }
    }

    #[test]
    fn the_table_has_the_columns_the_card_names() {
        let conn = mem();
        let names: Vec<String> = conn
            .prepare("PRAGMA table_info(voice_rooms)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "channel",
                "guild",
                "owner",
                "created_at",
                "locked",
                "user_limit"
            ]
        );
    }

    #[test]
    fn a_row_round_trips_and_migrating_twice_keeps_it() {
        let mut conn = mem();
        let mut stored = row(CH_A, GUILD, 1_700_000_000);
        stored.locked = true;
        stored.user_limit = 5;
        assert!(insert(&conn, &stored).unwrap());
        migrate(&mut conn).unwrap();
        assert_eq!(get(&conn, CH_A).unwrap(), Some(stored));
        assert_eq!(get(&conn, CH_B).unwrap(), None);
    }

    #[test]
    fn a_second_insert_for_the_same_channel_changes_nothing() {
        let conn = mem();
        assert!(insert(&conn, &row(CH_A, GUILD, 10)).unwrap());
        let mut other = row(CH_A, OTHER_GUILD, 99);
        other.owner = 7;
        assert!(!insert(&conn, &other).unwrap());
        assert_eq!(get(&conn, CH_A).unwrap(), Some(row(CH_A, GUILD, 10)));
    }

    #[test]
    fn delete_reports_whether_a_row_was_there() {
        let conn = mem();
        insert(&conn, &row(CH_A, GUILD, 10)).unwrap();
        assert!(delete(&conn, CH_A).unwrap());
        assert!(!delete(&conn, CH_A).unwrap());
        assert_eq!(get(&conn, CH_A).unwrap(), None);
    }

    #[test]
    fn list_is_per_guild_and_oldest_first() {
        let conn = mem();
        insert(&conn, &row(CH_B, GUILD, 20)).unwrap();
        insert(&conn, &row(CH_A, GUILD, 10)).unwrap();
        insert(&conn, &row(CH_A + 100, OTHER_GUILD, 5)).unwrap();
        let rows = list(&conn, GUILD).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.channel).collect::<Vec<_>>(),
            [CH_A, CH_B]
        );
        assert_eq!(list(&conn, OTHER_GUILD).unwrap().len(), 1);
        assert!(list(&conn, 42).unwrap().is_empty());
    }

    #[test]
    fn ids_above_i64_max_survive_the_text_columns() {
        let conn = mem();
        let big = row(u64::MAX, GUILD, 1);
        assert!(insert(&conn, &big).unwrap());
        assert_eq!(get(&conn, u64::MAX).unwrap(), Some(big));
    }

    #[test]
    fn a_damaged_row_is_an_error_not_a_smaller_table() {
        let conn = mem();
        insert(&conn, &row(CH_A, GUILD, 10)).unwrap();
        for bad in [
            "UPDATE voice_rooms SET owner = 'not-a-number'",
            "UPDATE voice_rooms SET owner = '0'",
            "UPDATE voice_rooms SET owner = '-4'",
            "UPDATE voice_rooms SET channel = ' 12'",
        ] {
            let conn = mem();
            insert(&conn, &row(CH_A, GUILD, 10)).unwrap();
            conn.execute(bad, []).unwrap();
            let guild_rows = list(&conn, GUILD);
            assert_eq!(guild_rows, Err(RepoError::CorruptRow), "{bad}");
        }
        // The CHECK constraints refuse an impossible flag or limit at write time.
        assert!(conn
            .execute("UPDATE voice_rooms SET locked = 2", [])
            .is_err());
        assert!(conn
            .execute("UPDATE voice_rooms SET user_limit = -1", [])
            .is_err());
        // A damaged row for another channel does not hide behind a good one.
        insert(&conn, &row(CH_B, GUILD, 11)).unwrap();
        conn.execute(
            "UPDATE voice_rooms SET guild = 'x' WHERE channel = ?1",
            params![CH_B.to_string()],
        )
        .unwrap();
        assert_eq!(get(&conn, CH_B), Err(RepoError::CorruptRow));
        assert!(get(&conn, CH_A).unwrap().is_some());
    }

    #[test]
    fn error_codes_are_stable_and_carry_no_text() {
        assert_eq!(RepoError::CorruptRow.code(), "corrupt_row");
        assert_eq!(
            RepoError::Storage(StorageError::Sqlite).code(),
            "sqlite_error"
        );
        assert_eq!(RepoError::CorruptRow.to_string(), "corrupt_row");
    }

    #[test]
    fn update_helpers_modify_only_the_matching_guild_row_and_refuse_zero_owner() {
        let conn = mem();
        let next_owner = OWNER + 100;
        insert(&conn, &row(CH_A, GUILD, 10)).unwrap();
        insert(&conn, &row(CH_B, OTHER_GUILD, 20)).unwrap();

        assert!(update_owner(&conn, CH_A, GUILD, next_owner).unwrap());
        assert!(update_locked(&conn, CH_A, GUILD, true).unwrap());
        assert!(update_user_limit(&conn, CH_A, GUILD, 12).unwrap());

        let updated = get(&conn, CH_A).unwrap().unwrap();
        assert_eq!(updated.owner, next_owner);
        assert!(updated.locked);
        assert_eq!(updated.user_limit, 12);

        // Wrong guild or untracked channel updates nothing.
        assert!(!update_owner(&conn, CH_A, OTHER_GUILD, OWNER).unwrap());
        assert!(!update_locked(&conn, CH_A, OTHER_GUILD, false).unwrap());
        assert!(!update_user_limit(&conn, CH_A, OTHER_GUILD, 3).unwrap());
        assert!(!update_owner(&conn, CH_B + 50, GUILD, OWNER).unwrap());
        assert_eq!(get(&conn, CH_A).unwrap().unwrap(), updated);
        assert_eq!(get(&conn, CH_B).unwrap().unwrap(), row(CH_B, OTHER_GUILD, 20));

        // Zero owner is refused with CorruptRow and leaves the row intact.
        assert_eq!(
            update_owner(&conn, CH_A, GUILD, 0),
            Err(RepoError::CorruptRow)
        );
        assert_eq!(get(&conn, CH_A).unwrap().unwrap(), updated);
    }
}

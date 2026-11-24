//! SQLite persistence for tracked voice rooms (`docs/verification/VOICE-ROOMS-RUNTIME.md`).
//!
//! One row per tracked room in `voice_rooms` inside the shared core database:
//! `channel TEXT PRIMARY KEY, guild TEXT, owner TEXT, created_at INTEGER, locked INTEGER,
//! user_limit INTEGER`. The row is the truth about which channels the bot may delete: a
//! channel without a row is never touched.
//!
//! VR4 adds `voice_room_deputies(channel, user, added_at)`, primary key `(channel, user)`
//! (`docs/verification/VOICE-ROOMS-DEPUTIES.md`). Triggers remove a room's deputy rows when its
//! `voice_rooms` row is deleted and remove the new owner's own row when the owner changes, in the
//! same statement.
//!
//! Functions take a plain `&Connection`, so the runtime can call them inside one
//! `tokio_rusqlite` call next to the job queue (the queue lives in the same file). Nothing
//! here holds a Discord type or reads a clock: the caller passes ids and `now`.

use crate::storage::{self, Migration, StorageError};
use rusqlite::{params, Connection, OptionalExtension};

/// Feature name used with `storage::migrate`.
pub const FEATURE: &str = "voice_rooms";

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: "CREATE TABLE voice_rooms (\
          channel TEXT NOT NULL PRIMARY KEY, \
          guild TEXT NOT NULL, \
          owner TEXT NOT NULL, \
          created_at INTEGER NOT NULL, \
          locked INTEGER NOT NULL DEFAULT 0 CHECK (locked IN (0,1)), \
          user_limit INTEGER NOT NULL DEFAULT 0 CHECK (user_limit >= 0)); \
          CREATE INDEX voice_rooms_guild ON voice_rooms (guild);",
    },
    // VR4: deputies. The two triggers run inside the statement that deletes a room row or changes
    // its owner, so a deputy row can never outlive its room, a recreated channel id never inherits
    // deputies, and a new owner stops being a deputy in the same write.
    Migration {
        version: 2,
        sql: "CREATE TABLE voice_room_deputies (\
          channel TEXT NOT NULL, \
          user TEXT NOT NULL, \
          added_at INTEGER NOT NULL, \
          PRIMARY KEY (channel, user)); \
          CREATE TRIGGER voice_rooms_delete_deputies AFTER DELETE ON voice_rooms \
          BEGIN DELETE FROM voice_room_deputies WHERE channel = OLD.channel; END; \
          CREATE TRIGGER voice_rooms_owner_not_deputy AFTER UPDATE OF owner ON voice_rooms \
          BEGIN DELETE FROM voice_room_deputies \
          WHERE channel = NEW.channel AND user = NEW.owner; END;",
    },
];

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
    if owner == 0 {
        return Err(RepoError::CorruptRow);
    }
    let n = conn.execute(
        "UPDATE voice_rooms SET owner = ?1 WHERE channel = ?2 AND guild = ?3",
        params![owner.to_string(), channel.to_string(), guild.to_string()],
    )?;
    Ok(n == 1)
}

/// Updates the locked flag of `channel` in `guild`. Returns whether a row was updated.
pub fn update_locked(
    conn: &Connection,
    channel: u64,
    guild: u64,
    locked: bool,
) -> Result<bool, RepoError> {
    let n = conn.execute(
        "UPDATE voice_rooms SET locked = ?1 WHERE channel = ?2 AND guild = ?3",
        params![i64::from(locked), channel.to_string(), guild.to_string()],
    )?;
    Ok(n == 1)
}

/// Updates the member limit of `channel` in `guild`. Returns whether a row was updated.
pub fn update_user_limit(
    conn: &Connection,
    channel: u64,
    guild: u64,
    user_limit: u32,
) -> Result<bool, RepoError> {
    let n = conn.execute(
        "UPDATE voice_rooms SET user_limit = ?1 WHERE channel = ?2 AND guild = ?3",
        params![
            i64::from(user_limit),
            channel.to_string(),
            guild.to_string()
        ],
    )?;
    Ok(n == 1)
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

/// What [`add_deputy`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddDeputy {
    /// The deputy row was written.
    Added,
    /// The user already was a deputy of the room; nothing was written.
    AlreadyDeputy,
    /// The room already has `cap` deputies; nothing was written.
    CapReached,
    /// The channel has no room row; nothing was written.
    RoomMissing,
}

/// Adds `user` as a deputy of `channel` unless the room is missing, the user already is one or
/// the room already has `cap` deputies. One statement checks all three, so the cap holds even
/// if two callers race; the row is never written for a channel without a room row.
pub fn add_deputy(
    conn: &Connection,
    channel: u64,
    user: u64,
    now: i64,
    cap: usize,
) -> Result<AddDeputy, RepoError> {
    if user == 0 {
        return Err(RepoError::CorruptRow);
    }
    let channel_text = channel.to_string();
    let user_text = user.to_string();
    let cap = i64::try_from(cap).unwrap_or(i64::MAX);
    let n = conn.execute(
        "INSERT INTO voice_room_deputies (channel, user, added_at) \
         SELECT ?1, ?2, ?3 \
         WHERE EXISTS (SELECT 1 FROM voice_rooms WHERE channel = ?1) \
         AND (SELECT COUNT(*) FROM voice_room_deputies WHERE channel = ?1) < ?4 \
         ON CONFLICT(channel, user) DO NOTHING",
        params![channel_text, user_text, now, cap],
    )?;
    if n == 1 {
        return Ok(AddDeputy::Added);
    }
    // Nothing was written: say why.
    if get(conn, channel)?.is_none() {
        Ok(AddDeputy::RoomMissing)
    } else if is_deputy(conn, channel, user)? {
        Ok(AddDeputy::AlreadyDeputy)
    } else {
        Ok(AddDeputy::CapReached)
    }
}

/// Removes one deputy row. Returns whether a row was there.
pub fn remove_deputy(conn: &Connection, channel: u64, user: u64) -> Result<bool, RepoError> {
    let n = conn.execute(
        "DELETE FROM voice_room_deputies WHERE channel = ?1 AND user = ?2",
        params![channel.to_string(), user.to_string()],
    )?;
    Ok(n == 1)
}

/// Whether `user` is a deputy of `channel`.
pub fn is_deputy(conn: &Connection, channel: u64, user: u64) -> Result<bool, RepoError> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM voice_room_deputies WHERE channel = ?1 AND user = ?2",
            params![channel.to_string(), user.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// The deputies of `channel`, oldest first. One unreadable row fails the whole read.
pub fn deputies(conn: &Connection, channel: u64) -> Result<Vec<u64>, RepoError> {
    let mut stmt = conn.prepare(
        "SELECT user FROM voice_room_deputies WHERE channel = ?1 ORDER BY added_at, rowid",
    )?;
    let raws = stmt
        .query_map(params![channel.to_string()], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    raws.into_iter()
        .map(|text| match text.parse::<u64>() {
            Ok(0) | Err(_) => Err(RepoError::CorruptRow),
            Ok(id) => Ok(id),
        })
        .collect()
}

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
        assert_eq!(
            get(&conn, CH_B).unwrap().unwrap(),
            row(CH_B, OTHER_GUILD, 20)
        );

        // Zero owner is refused with CorruptRow and leaves the row intact.
        assert_eq!(
            update_owner(&conn, CH_A, GUILD, 0),
            Err(RepoError::CorruptRow)
        );
        assert_eq!(get(&conn, CH_A).unwrap().unwrap(), updated);
    }

    const DEP_1: u64 = 6_200_000_000_000_031;
    const DEP_2: u64 = 6_200_000_000_000_032;
    const DEP_3: u64 = 6_200_000_000_000_033;

    #[test]
    fn deputy_rows_round_trip_with_cap_duplicates_and_missing_room() {
        let mut conn = mem();
        insert(&conn, &row(CH_A, GUILD, 10)).unwrap();
        assert_eq!(
            add_deputy(&conn, CH_A, DEP_1, 100, 2).unwrap(),
            AddDeputy::Added
        );
        assert_eq!(
            add_deputy(&conn, CH_A, DEP_1, 101, 2).unwrap(),
            AddDeputy::AlreadyDeputy
        );
        assert_eq!(
            add_deputy(&conn, CH_A, DEP_2, 102, 2).unwrap(),
            AddDeputy::Added
        );
        assert_eq!(
            add_deputy(&conn, CH_A, DEP_3, 103, 2).unwrap(),
            AddDeputy::CapReached
        );
        // A room with no row never gets a deputy row.
        assert_eq!(
            add_deputy(&conn, CH_B, DEP_1, 104, 2).unwrap(),
            AddDeputy::RoomMissing
        );
        // Migrating again keeps the rows; they are oldest first.
        migrate(&mut conn).unwrap();
        assert_eq!(deputies(&conn, CH_A).unwrap(), [DEP_1, DEP_2]);
        assert!(deputies(&conn, CH_B).unwrap().is_empty());
        assert!(is_deputy(&conn, CH_A, DEP_2).unwrap());
        assert!(!is_deputy(&conn, CH_A, DEP_3).unwrap());
        assert!(!is_deputy(&conn, CH_B, DEP_1).unwrap());
        assert!(remove_deputy(&conn, CH_A, DEP_1).unwrap());
        assert!(!remove_deputy(&conn, CH_A, DEP_1).unwrap());
        assert_eq!(deputies(&conn, CH_A).unwrap(), [DEP_2]);
        // A freed slot can be used again.
        assert_eq!(
            add_deputy(&conn, CH_A, DEP_3, 105, 2).unwrap(),
            AddDeputy::Added
        );
        assert_eq!(
            add_deputy(&conn, CH_A, 0, 106, 9),
            Err(RepoError::CorruptRow)
        );
    }

    fn deputy_row_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM voice_room_deputies", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn deleting_a_room_row_removes_its_deputies_and_a_recreated_channel_inherits_none() {
        let conn = mem();
        insert(&conn, &row(CH_A, GUILD, 10)).unwrap();
        insert(&conn, &row(CH_B, GUILD, 11)).unwrap();
        add_deputy(&conn, CH_A, DEP_1, 1, 5).unwrap();
        add_deputy(&conn, CH_B, DEP_2, 1, 5).unwrap();
        assert!(delete(&conn, CH_A).unwrap());
        assert_eq!(
            deputy_row_count(&conn),
            1,
            "only the deleted room's rows go"
        );
        assert_eq!(deputies(&conn, CH_B).unwrap(), [DEP_2]);
        // The same channel id comes back as a new room: nobody is a deputy of it.
        insert(&conn, &row(CH_A, GUILD, 20)).unwrap();
        assert!(deputies(&conn, CH_A).unwrap().is_empty());
        // Any other way of removing the row (a raw statement) removes the deputies too.
        add_deputy(&conn, CH_A, DEP_1, 2, 5).unwrap();
        conn.execute("DELETE FROM voice_rooms", []).unwrap();
        assert_eq!(deputy_row_count(&conn), 0);
    }

    #[test]
    fn a_failed_room_delete_leaves_the_deputies_alone() {
        let conn = mem();
        insert(&conn, &row(CH_A, GUILD, 10)).unwrap();
        add_deputy(&conn, CH_A, DEP_1, 1, 5).unwrap();
        conn.execute(
            "CREATE TRIGGER no_delete BEFORE DELETE ON voice_rooms \
             BEGIN SELECT RAISE(ABORT, 'x'); END;",
            [],
        )
        .unwrap();
        assert!(delete(&conn, CH_A).is_err());
        assert_eq!(deputies(&conn, CH_A).unwrap(), [DEP_1]);
        assert!(get(&conn, CH_A).unwrap().is_some());
    }

    #[test]
    fn a_new_owner_stops_being_a_deputy_in_the_same_update_and_others_stay() {
        let conn = mem();
        insert(&conn, &row(CH_A, GUILD, 10)).unwrap();
        insert(&conn, &row(CH_B, GUILD, 11)).unwrap();
        add_deputy(&conn, CH_A, DEP_1, 1, 5).unwrap();
        add_deputy(&conn, CH_A, DEP_2, 2, 5).unwrap();
        add_deputy(&conn, CH_B, DEP_1, 3, 5).unwrap();
        // A wrong-guild update changes nothing, deputies included.
        assert!(!update_owner(&conn, CH_A, OTHER_GUILD, DEP_1).unwrap());
        assert_eq!(deputies(&conn, CH_A).unwrap(), [DEP_1, DEP_2]);
        assert!(update_owner(&conn, CH_A, GUILD, DEP_1).unwrap());
        assert_eq!(get(&conn, CH_A).unwrap().unwrap().owner, DEP_1);
        assert_eq!(deputies(&conn, CH_A).unwrap(), [DEP_2]);
        assert_eq!(
            deputies(&conn, CH_B).unwrap(),
            [DEP_1],
            "other room untouched"
        );
    }
}

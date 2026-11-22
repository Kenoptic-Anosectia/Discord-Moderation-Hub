//! SQLite persistence for the bump reminder cycle (BU2).
//!
//! One row per guild in `bump_state` inside the shared core database. The columns are
//! the ones the plan names: `guild TEXT PRIMARY KEY, last_bump_at INTEGER,
//! last_bumper TEXT, next_due_at INTEGER, cycle INTEGER, reminded INTEGER`.
//!
//! Holds no Discord types and reads no clock: the caller passes ids and `now`.
//!
//! ## What `last_bumper` holds
//!
//! The plan names the column but does not define it. The runtime cannot know which
//! *member* bumped: the confirmation comes from the listing bot, so no member id is in
//! the message. What the runtime does know is the id of the message that confirmed the
//! bump. `last_bumper` therefore holds that message id as TEXT, which is also what
//! makes the policy's "same message id twice -> one confirmation" rule survive a
//! restart. `/bump status` shows the id and the time, never message text.

use super::policy::State;
use crate::storage::{self, Migration, StorageError};
use rusqlite::{params, Connection, OptionalExtension};

/// Feature name used with `storage::migrate`.
pub const FEATURE: &str = "bump";

/// The plan's table shape.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: "CREATE TABLE bump_state (\
          guild TEXT NOT NULL PRIMARY KEY, \
          last_bump_at INTEGER, \
          last_bumper TEXT, \
          next_due_at INTEGER, \
          cycle INTEGER NOT NULL DEFAULT 0 CHECK (cycle >= 0), \
          reminded INTEGER NOT NULL DEFAULT 0 CHECK (reminded IN (0,1)));",
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

/// One guild's stored bump cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BumpState {
    /// The guild this row belongs to.
    pub guild: u64,
    /// Unix seconds of the confirmation (or manual reset) that started the cycle.
    pub last_bump_at: Option<i64>,
    /// Id of the message that confirmed the cycle, as TEXT. `None` before the first
    /// confirmation.
    pub last_bumper: Option<u64>,
    /// When the reminder for this cycle becomes due.
    pub next_due_at: Option<i64>,
    /// The cycle number. It starts at 0 (no cycle yet) and grows by one per
    /// confirmation or manual reset; it names the cycle in the job queue's dedupe key.
    pub cycle: i64,
    /// True once the reminder for this cycle has been marked as sent.
    pub reminded: bool,
}

impl BumpState {
    /// The state of a guild that has never bumped.
    pub fn empty(guild: u64) -> Self {
        BumpState {
            guild,
            last_bump_at: None,
            last_bumper: None,
            next_due_at: None,
            cycle: 0,
            reminded: false,
        }
    }

    /// The policy state this row describes.
    pub fn to_policy(&self) -> State {
        State {
            last_confirmed_at: self.last_bump_at,
            last_message_id: self.last_bumper,
            reminded_for_cycle: self.reminded,
        }
    }

    /// Build a row from a policy state, keeping the cycle number and the due time.
    pub fn from_policy(guild: u64, cycle: i64, state: &State, next_due_at: Option<i64>) -> Self {
        BumpState {
            guild,
            last_bump_at: state.last_confirmed_at,
            last_bumper: state.last_message_id,
            next_due_at,
            cycle,
            reminded: state.reminded_for_cycle,
        }
    }
}

/// Bump state store over one core-database connection.
#[derive(Debug)]
pub struct Repository {
    conn: Connection,
}

impl Repository {
    /// Opens the core database (refusing `reserved` paths) and applies the migrations.
    pub fn open(path: &str, reserved: &[std::path::PathBuf]) -> Result<Self, RepoError> {
        Self::new(storage::open(path, reserved)?)
    }

    /// Applies the bump migrations to an already opened connection.
    pub fn new(mut conn: Connection) -> Result<Self, RepoError> {
        storage::migrate(&mut conn, FEATURE, MIGRATIONS)?;
        Ok(Self { conn })
    }

    /// Loads one guild's row. `Ok(None)` means no row yet.
    ///
    /// A row that cannot be read back exactly is an error, never a default state: a
    /// damaged row must not look like "no cycle", which would re-arm a reminder.
    pub fn load(&self, guild: u64) -> Result<Option<BumpState>, RepoError> {
        let raw = self
            .conn
            .query_row(
                "SELECT last_bump_at, last_bumper, next_due_at, cycle, reminded \
                 FROM bump_state WHERE guild = ?1",
                params![guild.to_string()],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((last_bump_at, last_bumper, next_due_at, cycle, reminded)) = raw else {
            return Ok(None);
        };
        let last_bumper = match last_bumper {
            None => None,
            Some(text) => Some(text.parse::<u64>().map_err(|_| RepoError::CorruptRow)?),
        };
        let reminded = match reminded {
            0 => false,
            1 => true,
            _ => return Err(RepoError::CorruptRow),
        };
        if cycle < 0 {
            return Err(RepoError::CorruptRow);
        }
        Ok(Some(BumpState {
            guild,
            last_bump_at,
            last_bumper,
            next_due_at,
            cycle,
            reminded,
        }))
    }

    /// Writes the row for its guild, replacing any row already there.
    pub fn save(&mut self, state: &BumpState) -> Result<(), RepoError> {
        self.conn.execute(
            "INSERT INTO bump_state (guild, last_bump_at, last_bumper, next_due_at, cycle, reminded) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(guild) DO UPDATE SET \
               last_bump_at = excluded.last_bump_at, \
               last_bumper = excluded.last_bumper, \
               next_due_at = excluded.next_due_at, \
               cycle = excluded.cycle, \
               reminded = excluded.reminded",
            params![
                state.guild.to_string(),
                state.last_bump_at,
                state.last_bumper.map(|id| id.to_string()),
                state.next_due_at,
                state.cycle,
                i64::from(state.reminded),
            ],
        )?;
        Ok(())
    }

    /// Mark the cycle as reminded, but only if the stored row is still that cycle.
    ///
    /// The write is conditional on the cycle number, so a handler that read an
    /// older cycle cannot mark a newer one: it finds `false` and must not post.
    /// Returns `true` when the row was changed.
    pub fn mark_reminded(&mut self, guild: u64, cycle: i64) -> Result<bool, RepoError> {
        let n = self.conn.execute(
            "UPDATE bump_state SET reminded = 1 WHERE guild = ?1 AND cycle = ?2",
            params![guild.to_string(), cycle],
        )?;
        Ok(n == 1)
    }

    /// The connection, for a caller that must reach the shared database (the job
    /// queue lives in the same file).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const GUILD: u64 = 1_000_001;
    const OTHER_GUILD: u64 = 1_000_002;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("bu2-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("core.sqlite")
    }

    fn mem() -> Repository {
        Repository::open(":memory:", &[]).unwrap()
    }

    fn state() -> BumpState {
        BumpState {
            guild: GUILD,
            last_bump_at: Some(1_700_000_000),
            last_bumper: Some(123_456_789_012_345_678),
            next_due_at: Some(1_700_007_200),
            cycle: 3,
            reminded: false,
        }
    }

    #[test]
    fn the_table_has_the_columns_the_plan_names() {
        let mut repo = mem();
        repo.save(&state()).unwrap();
        let names: Vec<String> = repo
            .connection()
            .prepare("PRAGMA table_info(bump_state)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "guild",
                "last_bump_at",
                "last_bumper",
                "next_due_at",
                "cycle",
                "reminded"
            ]
        );
    }

    #[test]
    fn a_missing_row_loads_as_none() {
        let repo = mem();
        assert_eq!(repo.load(GUILD), Ok(None));
    }

    #[test]
    fn a_saved_row_round_trips() {
        let mut repo = mem();
        let s = state();
        repo.save(&s).unwrap();
        assert_eq!(repo.load(GUILD), Ok(Some(s)));
    }

    #[test]
    fn saving_twice_replaces_the_row() {
        let mut repo = mem();
        repo.save(&state()).unwrap();
        let mut second = state();
        second.cycle = 4;
        second.reminded = true;
        second.last_bump_at = Some(1_700_009_999);
        repo.save(&second).unwrap();
        assert_eq!(repo.load(GUILD), Ok(Some(second)));
    }

    #[test]
    fn guilds_are_isolated() {
        let mut repo = mem();
        let mut other = state();
        other.guild = OTHER_GUILD;
        other.cycle = 9;
        repo.save(&state()).unwrap();
        repo.save(&other).unwrap();
        assert_eq!(repo.load(GUILD).unwrap().unwrap().cycle, 3);
        assert_eq!(repo.load(OTHER_GUILD).unwrap().unwrap().cycle, 9);
        assert_eq!(repo.load(4_000_004), Ok(None));
    }

    #[test]
    fn an_empty_state_round_trips() {
        let mut repo = mem();
        let s = BumpState::empty(GUILD);
        repo.save(&s).unwrap();
        assert_eq!(repo.load(GUILD), Ok(Some(s)));
    }

    #[test]
    fn a_corrupt_bumper_text_is_an_error_not_a_default() {
        let path = scratch("corrupt-bumper");
        let mut repo = Repository::open(path.to_str().unwrap(), &[]).unwrap();
        repo.save(&state()).unwrap();
        drop(repo);
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE bump_state SET last_bumper = 'not-a-number' WHERE guild = ?1",
                params![GUILD.to_string()],
            )
            .unwrap();
        let repo = Repository::open(path.to_str().unwrap(), &[]).unwrap();
        assert_eq!(repo.load(GUILD), Err(RepoError::CorruptRow));
    }

    #[test]
    fn an_impossible_reminded_value_is_refused_by_the_schema() {
        let path = scratch("corrupt-reminded");
        let mut repo = Repository::open(path.to_str().unwrap(), &[]).unwrap();
        repo.save(&state()).unwrap();
        drop(repo);
        // The CHECK constraint refuses the write: a row can never hold a value the
        // reader does not understand.
        let written = Connection::open(&path).unwrap().execute(
            "UPDATE bump_state SET reminded = 7 WHERE guild = ?1",
            params![GUILD.to_string()],
        );
        assert!(written.is_err(), "the schema accepted reminded = 7");
        let repo = Repository::open(path.to_str().unwrap(), &[]).unwrap();
        assert!(!repo.load(GUILD).unwrap().unwrap().reminded);
    }

    #[test]
    fn a_negative_cycle_is_refused_by_the_schema() {
        let path = scratch("corrupt-cycle");
        let mut repo = Repository::open(path.to_str().unwrap(), &[]).unwrap();
        repo.save(&state()).unwrap();
        drop(repo);
        let written = Connection::open(&path).unwrap().execute(
            "UPDATE bump_state SET cycle = -1 WHERE guild = ?1",
            params![GUILD.to_string()],
        );
        assert!(written.is_err(), "the schema accepted cycle = -1");
        let repo = Repository::open(path.to_str().unwrap(), &[]).unwrap();
        assert_eq!(repo.load(GUILD).unwrap().unwrap().cycle, 3);
    }

    #[test]
    fn the_policy_state_round_trips_through_the_row() {
        let mut repo = mem();
        let s = state();
        repo.save(&s).unwrap();
        let loaded = repo.load(GUILD).unwrap().unwrap();
        assert_eq!(loaded.to_policy(), s.to_policy());
    }

    #[test]
    fn the_mark_of_the_reminder_is_conditional_on_the_cycle() {
        let mut repo = mem();
        repo.save(&state()).unwrap();
        // The stored cycle is 3: the mark lands.
        assert!(repo.mark_reminded(GUILD, 3).unwrap());
        assert!(repo.load(GUILD).unwrap().unwrap().reminded);
        // A stale handler that read an older cycle must not mark a newer one.
        let mut newer = state();
        newer.cycle = 4;
        newer.reminded = false;
        repo.save(&newer).unwrap();
        assert!(
            !repo.mark_reminded(GUILD, 3).unwrap(),
            "a stale cycle marked the row"
        );
        let stored = repo.load(GUILD).unwrap().unwrap();
        assert_eq!(stored.cycle, 4);
        assert!(!stored.reminded, "the newer cycle was marked as reminded");
        // The right cycle marks it.
        assert!(repo.mark_reminded(GUILD, 4).unwrap());
        assert!(repo.load(GUILD).unwrap().unwrap().reminded);
    }

    #[test]
    fn a_mark_for_another_guild_changes_nothing() {
        let mut repo = mem();
        repo.save(&state()).unwrap();
        assert!(!repo.mark_reminded(OTHER_GUILD, 3).unwrap());
        assert!(!repo.load(GUILD).unwrap().unwrap().reminded);
    }

    #[test]
    fn errors_are_stable_codes_that_carry_no_sql_or_path() {
        let e = RepoError::CorruptRow;
        assert_eq!(e.code(), "corrupt_row");
        assert_eq!(format!("{e}"), "corrupt_row");
        let e = RepoError::Storage(StorageError::Sqlite);
        assert_eq!(e.code(), "sqlite_error");
        let shown = format!("{e:?} {e}");
        assert!(!shown.contains("SELECT"), "{shown}");
    }

    #[test]
    fn a_newer_stored_schema_is_refused() {
        let path = scratch("schema");
        drop(Repository::open(path.to_str().unwrap(), &[]).unwrap());
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE schema_versions SET version = 99 WHERE feature = 'bump'",
                [],
            )
            .unwrap();
        assert_eq!(
            Repository::open(path.to_str().unwrap(), &[]).unwrap_err(),
            RepoError::Storage(StorageError::StoredVersionNewer)
        );
    }

    #[test]
    fn rerunning_the_migration_is_a_no_op() {
        let path = scratch("rerun");
        drop(Repository::open(path.to_str().unwrap(), &[]).unwrap());
        let mut repo = Repository::open(path.to_str().unwrap(), &[]).unwrap();
        repo.save(&state()).unwrap();
        drop(repo);
        let repo = Repository::open(path.to_str().unwrap(), &[]).unwrap();
        assert_eq!(repo.load(GUILD), Ok(Some(state())));
    }
}

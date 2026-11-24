//! SQLite persistence for role menus (RM2).
//!
//! Two tables in the shared core database:
//! `role_menus(id INTEGER PK, guild TEXT, key TEXT UNIQUE, mode TEXT,
//! max_select INTEGER, channel TEXT, message TEXT, embed_key TEXT)` and
//! `role_menu_options(menu_id INTEGER, role TEXT, label TEXT, emoji TEXT,
//! position INTEGER)`.
//!
//! The repository creates and migrates its own tables (it does not touch
//! `storage.rs`). It holds no Discord types, reads no clock and does no I/O
//! outside the database: ids and `now` are passed in.
//!
//! Every error is a stable code that carries no SQL text, path, id or name.

use std::fmt;

use rusqlite::{params, Connection, OptionalExtension};

use crate::storage::{Migration, StorageError};

/// Name of the feature in the shared `schema_versions` table.
pub const FEATURE: &str = "role_menus";

/// The migrations of the `role_menus` feature, in order.
pub const MIGRATIONS: [Migration; 2] = [
    Migration {
        version: 1,
        sql: "CREATE TABLE IF NOT EXISTS role_menus (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            guild TEXT NOT NULL,
            key TEXT NOT NULL UNIQUE,
            mode TEXT NOT NULL,
            max_select INTEGER NOT NULL DEFAULT 0,
            channel TEXT,
            message TEXT,
            embed_key TEXT
        );",
    },
    Migration {
        version: 2,
        sql: "CREATE TABLE IF NOT EXISTS role_menu_options (
            menu_id INTEGER NOT NULL REFERENCES role_menus(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            label TEXT NOT NULL,
            emoji TEXT NOT NULL,
            position INTEGER NOT NULL,
            PRIMARY KEY (menu_id, role)
        );",
    },
];

/// Stable error codes of the role-menu repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoError {
    /// A stored column holds a value the reader cannot understand.
    CorruptRow,
    /// The menu key is already used by another menu.
    KeyTaken,
    /// The option is already stored for this menu.
    OptionExists,
    /// The option is not stored for this menu.
    OptionMissing,
    /// The menu is not stored.
    MenuMissing,
    /// The mode is not one of the two known spellings.
    InvalidMode,
    /// Underlying storage failure (code from the storage layer).
    Storage(StorageError),
}

impl RepoError {
    /// The stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            RepoError::CorruptRow => "corrupt_row",
            RepoError::KeyTaken => "key_taken",
            RepoError::OptionExists => "option_exists",
            RepoError::OptionMissing => "option_missing",
            RepoError::MenuMissing => "menu_missing",
            RepoError::InvalidMode => "invalid_mode",
            RepoError::Storage(e) => e.code(),
        }
    }
}

/// The two mode spellings the table may hold.
pub const MODES: [&str; 2] = ["exclusive", "multi"];

/// The extended SQLite result codes this module reads.
///
/// They are the documented SQLite values for `SQLITE_CONSTRAINT_*` and do not
/// change between SQLite versions.
const SQLITE_CONSTRAINT_FOREIGNKEY: i32 = 787;
const SQLITE_CONSTRAINT_PRIMARYKEY: i32 = 1555;
const SQLITE_CONSTRAINT_UNIQUE: i32 = 2067;

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

/// One stored role menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuRow {
    /// The row id.
    pub id: i64,
    /// The guild the menu belongs to.
    pub guild: u64,
    /// The menu key, unique across the table.
    pub key: String,
    /// The mode of the menu (`exclusive` or `multi`).
    pub mode: String,
    /// The maximum number of options a member may hold (0 = unbounded).
    pub max_select: u32,
    /// The channel the menu was posted in, if it was posted.
    pub channel: Option<u64>,
    /// The message the menu was posted as, if it was posted.
    pub message: Option<u64>,
    /// The key of the managed embed that renders the menu, if any.
    pub embed_key: Option<String>,
}

/// One stored option of a role menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRow {
    /// The menu the option belongs to.
    pub menu_id: i64,
    /// The role the option grants, as text.
    pub role: u64,
    /// The label shown on the option.
    pub label: String,
    /// The emoji shown on the option.
    pub emoji: String,
    /// The position of the option inside the menu.
    pub position: i64,
}

/// Applies the migrations of the `role_menus` feature to a connection.
///
/// Idempotent: running it again changes nothing, and a stored version newer
/// than the code knows is refused with no writes (the storage layer's rule).
pub fn migrate(conn: &mut Connection) -> Result<u32, StorageError> {
    crate::storage::migrate(conn, FEATURE, &MIGRATIONS)
}

fn parse_id(text: &str) -> Result<u64, RepoError> {
    let id = text.parse::<u64>().map_err(|_| RepoError::CorruptRow)?;
    if id == 0 {
        return Err(RepoError::CorruptRow);
    }
    Ok(id)
}

fn parse_optional_id(text: Option<String>) -> Result<Option<u64>, RepoError> {
    match text {
        None => Ok(None),
        Some(text) => Ok(Some(parse_id(&text)?)),
    }
}

/// Reads one menu row.
///
/// A stored id that is not a positive number, or a `max_select` that is
/// negative, is a [`RepoError::CorruptRow`]: a damaged row must not look like
/// "no menu" or "no options".
fn menu_from_row(row: &rusqlite::Row<'_>) -> Result<MenuRow, RepoError> {
    let id: i64 = row.get(0).map_err(|_| RepoError::CorruptRow)?;
    let guild_text: String = row.get(1).map_err(|_| RepoError::CorruptRow)?;
    let key: String = row.get(2).map_err(|_| RepoError::CorruptRow)?;
    let mode: String = row.get(3).map_err(|_| RepoError::CorruptRow)?;
    let max_select: i64 = row.get(4).map_err(|_| RepoError::CorruptRow)?;
    let channel: Option<String> = row.get(5).map_err(|_| RepoError::CorruptRow)?;
    let message: Option<String> = row.get(6).map_err(|_| RepoError::CorruptRow)?;
    let embed_key: Option<String> = row.get(7).map_err(|_| RepoError::CorruptRow)?;
    if max_select < 0 {
        return Err(RepoError::CorruptRow);
    }
    Ok(MenuRow {
        id,
        guild: parse_id(&guild_text)?,
        key,
        mode,
        max_select: u32::try_from(max_select).map_err(|_| RepoError::CorruptRow)?,
        channel: parse_optional_id(channel)?,
        message: parse_optional_id(message)?,
        embed_key,
    })
}

/// The rusqlite-side wrapper of [`menu_from_row`].
///
/// A corrupt row is reported as a rusqlite error so it travels out of the
/// query; the caller turns it into [`RepoError::CorruptRow`].
fn menu_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MenuRow> {
    menu_from_row(row).map_err(|_| {
        rusqlite::Error::InvalidColumnType(
            0,
            "role_menus row".to_string(),
            rusqlite::types::Type::Text,
        )
    })
}

/// Turns a rusqlite error into a repository error.
///
/// The `InvalidColumnType` marker is how [`menu_row`] reports a corrupt row;
/// every other engine error is a plain storage failure.
fn row_error(error: rusqlite::Error) -> RepoError {
    match error {
        rusqlite::Error::InvalidColumnType(_, _, _) => RepoError::CorruptRow,
        _ => RepoError::Storage(StorageError::Sqlite),
    }
}

/// Creates a menu row and returns its id.
///
/// The key is unique across the whole table: a key that is already used is
/// refused with [`RepoError::KeyTaken`] and nothing is written. A mode that is
/// not one of [`MODES`] is refused with [`RepoError::InvalidMode`]: a row must
/// never hold a mode the reader cannot understand.
pub fn create_menu(
    conn: &Connection,
    guild: u64,
    key: &str,
    mode: &str,
    max_select: u32,
) -> Result<i64, RepoError> {
    if !MODES.contains(&mode) {
        return Err(RepoError::InvalidMode);
    }
    // A plain INSERT, not INSERT OR IGNORE: the duplicate must be reported,
    // never silently swallowed.
    match conn.execute(
        "INSERT INTO role_menus (guild, key, mode, max_select) \
         VALUES (?1, ?2, ?3, ?4)",
        params![guild.to_string(), key, mode, i64::from(max_select)],
    ) {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(error, _)) => {
            return Err(match error.extended_code {
                SQLITE_CONSTRAINT_UNIQUE | SQLITE_CONSTRAINT_PRIMARYKEY => RepoError::KeyTaken,
                code => constraint_error(code),
            });
        }
        Err(_) => return Err(RepoError::Storage(StorageError::Sqlite)),
    }
    let id: i64 = conn
        .query_row(
            "SELECT id FROM role_menus WHERE guild = ?1 AND key = ?2",
            params![guild.to_string(), key],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(RepoError::KeyTaken)?;
    Ok(id)
}

/// Reads one menu by guild and key. `Ok(None)` means no such menu.
pub fn get_menu(conn: &Connection, guild: u64, key: &str) -> Result<Option<MenuRow>, RepoError> {
    let row = conn
        .query_row(
            "SELECT id, guild, key, mode, max_select, channel, message, embed_key \
             FROM role_menus WHERE guild = ?1 AND key = ?2",
            params![guild.to_string(), key],
            menu_row,
        )
        .optional()
        .map_err(row_error)?;
    Ok(row)
}

/// Lists the menus of one guild, ordered by key.
pub fn list_menus(conn: &Connection, guild: u64) -> Result<Vec<MenuRow>, RepoError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, guild, key, mode, max_select, channel, message, embed_key \
             FROM role_menus WHERE guild = ?1 ORDER BY key ASC",
        )
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    let rows = stmt
        .query_map(params![guild.to_string()], menu_row)
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    let mut out = Vec::new();
    for item in rows {
        out.push(item.map_err(row_error)?);
    }
    Ok(out)
}

/// Stores one option of a menu and returns its position.
///
/// The role is unique inside a menu: a second option for the same role is
/// refused with [`RepoError::OptionExists`] and the menu keeps its options.
/// An option for a menu that does not exist is refused with
/// [`RepoError::MenuMissing`]: the two constraint kinds are told apart by
/// their extended SQLite code, so a missing menu never looks like a
/// duplicate option.
pub fn add_option(
    conn: &Connection,
    menu_id: i64,
    role: u64,
    label: &str,
    emoji: &str,
) -> Result<i64, RepoError> {
    let next: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM role_menu_options WHERE menu_id = ?1",
            params![menu_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    match conn.execute(
        "INSERT INTO role_menu_options (menu_id, role, label, emoji, position) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![menu_id, role.to_string(), label, emoji, next],
    ) {
        Ok(_) => Ok(next),
        Err(rusqlite::Error::SqliteFailure(error, _)) => Err(constraint_error(error.extended_code)),
        Err(_) => Err(RepoError::Storage(StorageError::Sqlite)),
    }
}

/// Turns a constraint violation into the error the caller must see.
fn constraint_error(extended_code: i32) -> RepoError {
    match extended_code {
        SQLITE_CONSTRAINT_FOREIGNKEY => RepoError::MenuMissing,
        SQLITE_CONSTRAINT_PRIMARYKEY | SQLITE_CONSTRAINT_UNIQUE => RepoError::OptionExists,
        _ => RepoError::Storage(StorageError::Sqlite),
    }
}

/// Lists the options of one menu, ordered by position.
pub fn list_options(conn: &Connection, menu_id: i64) -> Result<Vec<OptionRow>, RepoError> {
    let mut stmt = conn
        .prepare(
            "SELECT menu_id, role, label, emoji, position FROM role_menu_options \
             WHERE menu_id = ?1 ORDER BY position ASC, role ASC",
        )
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    let rows = stmt
        .query_map(params![menu_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    let mut out = Vec::new();
    for item in rows {
        let (menu_id, role, label, emoji, position) = item.map_err(|_| RepoError::CorruptRow)?;
        let role = parse_id(&role).map_err(|_| RepoError::CorruptRow)?;
        out.push(OptionRow {
            menu_id,
            role,
            label,
            emoji,
            position,
        });
    }
    Ok(out)
}

/// Deletes one option of a menu. Returns whether a row was deleted.
///
/// Deleting an option never touches member roles: this function only removes
/// the stored row.
pub fn remove_option(conn: &Connection, menu_id: i64, role: u64) -> Result<bool, RepoError> {
    let n = conn
        .execute(
            "DELETE FROM role_menu_options WHERE menu_id = ?1 AND role = ?2",
            params![menu_id, role.to_string()],
        )
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    Ok(n == 1)
}

/// Records where a menu was published.
///
/// Only the channel, the message and the embed key are written; the menu
/// itself is not changed.
pub fn set_published(
    conn: &Connection,
    menu_id: i64,
    channel: u64,
    message: u64,
    embed_key: Option<&str>,
) -> Result<(), RepoError> {
    let n = conn
        .execute(
            "UPDATE role_menus SET channel = ?1, message = ?2, embed_key = ?3 WHERE id = ?4",
            params![channel.to_string(), message.to_string(), embed_key, menu_id],
        )
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    if n != 1 {
        return Err(RepoError::MenuMissing);
    }
    Ok(())
}

/// Changes the mode and the maximum of a menu.
///
/// Only those two columns are written: the key, the guild, the options and
/// the published location are untouched. A mode that is not one of [`MODES`]
/// is refused with [`RepoError::InvalidMode`] and nothing is written; a menu
/// that is not stored is refused with [`RepoError::MenuMissing`].
pub fn update_menu(
    conn: &Connection,
    menu_id: i64,
    mode: &str,
    max_select: u32,
) -> Result<(), RepoError> {
    if !MODES.contains(&mode) {
        return Err(RepoError::InvalidMode);
    }
    let n = conn
        .execute(
            "UPDATE role_menus SET mode = ?1, max_select = ?2 WHERE id = ?3",
            params![mode, i64::from(max_select), menu_id],
        )
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    if n != 1 {
        return Err(RepoError::MenuMissing);
    }
    Ok(())
}

/// Clears the recorded location of a menu.
///
/// The channel, the message and the embed key are set to null, so the menu
/// may be published again somewhere else. The menu row itself and its
/// options are untouched.
///
/// A menu that is not stored is refused with [`RepoError::MenuMissing`].
pub fn clear_published(conn: &Connection, menu_id: i64) -> Result<(), RepoError> {
    let n = conn
        .execute(
            "UPDATE role_menus SET channel = NULL, message = NULL, embed_key = NULL WHERE id = ?1",
            params![menu_id],
        )
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    if n != 1 {
        return Err(RepoError::MenuMissing);
    }
    Ok(())
}

/// What a menu deletion removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deleted {
    /// True when a menu row was deleted.
    pub menu: bool,
    /// The number of option rows the delete removed.
    pub options: u32,
}

/// Deletes a menu and its options.
///
/// The options are deleted by this function, not only by the foreign key
/// cascade, and the number of removed options is reported so the caller can
/// see that the delete really happened.
///
/// Deleting a menu never touches member roles.
pub fn delete_menu(conn: &Connection, menu_id: i64) -> Result<Deleted, RepoError> {
    let options = conn
        .execute(
            "DELETE FROM role_menu_options WHERE menu_id = ?1",
            params![menu_id],
        )
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    let n = conn
        .execute("DELETE FROM role_menus WHERE id = ?1", params![menu_id])
        .map_err(|_| RepoError::Storage(StorageError::Sqlite))?;
    Ok(Deleted {
        menu: n == 1,
        options: u32::try_from(options).unwrap_or(u32::MAX),
    })
}

/// The number of options a menu holds.
pub fn count_options(conn: &Connection, menu_id: i64) -> Result<u32, RepoError> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM role_menu_options WHERE menu_id = ?1",
            params![menu_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Synthetic ids only; never a real Discord id.
    const GUILD: u64 = 640_001;
    const OTHER_GUILD: u64 = 640_002;
    const CHANNEL: u64 = 640_010;
    const MESSAGE: u64 = 640_020;
    const ROLE_A: u64 = 640_101;
    const ROLE_B: u64 = 640_102;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("rm2-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("core.sqlite")
    }

    fn open(path: &str) -> Connection {
        let mut conn = crate::storage::open(path, &[]).unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    fn mem() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    #[test]
    fn the_schema_has_the_columns_the_card_names() {
        let conn = mem();
        let names: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(role_menus)").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            names,
            vec![
                "id",
                "guild",
                "key",
                "mode",
                "max_select",
                "channel",
                "message",
                "embed_key"
            ]
        );
        let option_names: Vec<String> = {
            let mut stmt = conn
                .prepare("PRAGMA table_info(role_menu_options)")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            option_names,
            vec!["menu_id", "role", "label", "emoji", "position"]
        );
    }

    #[test]
    fn a_created_menu_round_trips() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        let menu = get_menu(&conn, GUILD, "main").unwrap().unwrap();
        assert_eq!(menu.id, id);
        assert_eq!(menu.guild, GUILD);
        assert_eq!(menu.key, "main");
        assert_eq!(menu.mode, "exclusive");
        assert_eq!(menu.max_select, 0);
        assert_eq!(menu.channel, None);
        assert_eq!(menu.message, None);
        assert_eq!(menu.embed_key, None);
    }

    #[test]
    fn a_missing_menu_loads_as_none() {
        let conn = mem();
        assert_eq!(get_menu(&conn, GUILD, "nothing").unwrap(), None);
    }

    #[test]
    fn a_key_is_unique_and_a_second_create_is_refused() {
        let conn = mem();
        create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        assert_eq!(
            create_menu(&conn, GUILD, "main", "multi", 3),
            Err(RepoError::KeyTaken)
        );
        // The first row is untouched.
        let menu = get_menu(&conn, GUILD, "main").unwrap().unwrap();
        assert_eq!(menu.mode, "exclusive");
        assert_eq!(menu.max_select, 0);
        assert_eq!(list_menus(&conn, GUILD).unwrap().len(), 1);
    }

    #[test]
    fn guilds_are_isolated() {
        let conn = mem();
        create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        create_menu(&conn, OTHER_GUILD, "other", "multi", 2).unwrap();
        assert_eq!(
            get_menu(&conn, GUILD, "main").unwrap().unwrap().mode,
            "exclusive"
        );
        assert_eq!(
            get_menu(&conn, OTHER_GUILD, "other").unwrap().unwrap().mode,
            "multi"
        );
        // Each guild sees only its own menus.
        assert_eq!(list_menus(&conn, GUILD).unwrap().len(), 1);
        assert_eq!(list_menus(&conn, OTHER_GUILD).unwrap().len(), 1);
        assert_eq!(list_menus(&conn, GUILD).unwrap()[0].key, "main");
        assert_eq!(list_menus(&conn, OTHER_GUILD).unwrap()[0].key, "other");
    }

    #[test]
    fn a_key_is_unique_across_the_whole_table() {
        let conn = mem();
        create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        // The same key in another guild is refused too: the key names one menu.
        assert_eq!(
            create_menu(&conn, OTHER_GUILD, "main", "exclusive", 0),
            Err(RepoError::KeyTaken)
        );
        assert_eq!(list_menus(&conn, OTHER_GUILD).unwrap().len(), 0);
    }

    #[test]
    fn menus_are_listed_in_key_order() {
        let conn = mem();
        create_menu(&conn, GUILD, "zulu", "exclusive", 0).unwrap();
        create_menu(&conn, GUILD, "alpha", "exclusive", 0).unwrap();
        create_menu(&conn, GUILD, "mike", "exclusive", 0).unwrap();
        let keys: Vec<String> = list_menus(&conn, GUILD)
            .unwrap()
            .into_iter()
            .map(|m| m.key)
            .collect();
        assert_eq!(keys, vec!["alpha", "mike", "zulu"]);
    }

    #[test]
    fn options_round_trip_in_position_order() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        assert_eq!(add_option(&conn, id, ROLE_A, "Alpha", "a").unwrap(), 0);
        assert_eq!(add_option(&conn, id, ROLE_B, "Beta", "b").unwrap(), 1);
        let options = list_options(&conn, id).unwrap();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].role, ROLE_A);
        assert_eq!(options[0].label, "Alpha");
        assert_eq!(options[0].emoji, "a");
        assert_eq!(options[0].position, 0);
        assert_eq!(options[1].role, ROLE_B);
        assert_eq!(options[1].position, 1);
        assert_eq!(count_options(&conn, id).unwrap(), 2);
    }

    #[test]
    fn the_same_role_cannot_be_added_twice_to_one_menu() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        add_option(&conn, id, ROLE_A, "Alpha", "a").unwrap();
        assert_eq!(
            add_option(&conn, id, ROLE_A, "Other", "b"),
            Err(RepoError::OptionExists)
        );
        // The stored option is unchanged.
        let options = list_options(&conn, id).unwrap();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].label, "Alpha");
    }

    #[test]
    fn the_same_role_may_be_added_to_two_menus() {
        let conn = mem();
        let a = create_menu(&conn, GUILD, "one", "exclusive", 0).unwrap();
        let b = create_menu(&conn, GUILD, "two", "exclusive", 0).unwrap();
        add_option(&conn, a, ROLE_A, "Alpha", "a").unwrap();
        add_option(&conn, b, ROLE_A, "Alpha", "a").unwrap();
        assert_eq!(count_options(&conn, a).unwrap(), 1);
        assert_eq!(count_options(&conn, b).unwrap(), 1);
    }

    #[test]
    fn removing_an_option_removes_the_row_and_reports_whether_it_was_there() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        add_option(&conn, id, ROLE_A, "Alpha", "a").unwrap();
        assert!(remove_option(&conn, id, ROLE_A).unwrap());
        assert!(!remove_option(&conn, id, ROLE_A).unwrap());
        assert_eq!(count_options(&conn, id).unwrap(), 0);
        // Another role is untouched.
        assert!(!remove_option(&conn, id, ROLE_B).unwrap());
    }

    #[test]
    fn deleting_a_menu_deletes_its_options() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        add_option(&conn, id, ROLE_A, "Alpha", "a").unwrap();
        add_option(&conn, id, ROLE_B, "Beta", "b").unwrap();
        let deleted = delete_menu(&conn, id).unwrap();
        assert!(deleted.menu, "the menu row is gone");
        assert_eq!(deleted.options, 2, "both option rows were deleted");
        assert_eq!(list_options(&conn, id).unwrap().len(), 0);
        assert_eq!(get_menu(&conn, GUILD, "main").unwrap(), None);
        let again = delete_menu(&conn, id).unwrap();
        assert!(!again.menu, "there is no menu left to delete");
        assert_eq!(again.options, 0);
    }

    #[test]
    fn an_unknown_mode_is_refused_and_nothing_is_written() {
        let conn = mem();
        assert_eq!(
            create_menu(&conn, GUILD, "main", "garbage", 0),
            Err(RepoError::InvalidMode)
        );
        assert_eq!(list_menus(&conn, GUILD).unwrap().len(), 0);
        // The two known spellings are accepted.
        create_menu(&conn, GUILD, "one", "exclusive", 0).unwrap();
        create_menu(&conn, GUILD, "two", "multi", 2).unwrap();
        assert_eq!(list_menus(&conn, GUILD).unwrap().len(), 2);
    }

    #[test]
    fn an_option_for_a_missing_menu_is_refused_as_a_missing_menu() {
        let path = scratch("fk");
        let ps = path.to_str().unwrap();
        let conn = open(ps);
        // The foreign key is on in a real database: the violation must not be
        // reported as a duplicate option.
        assert_eq!(
            add_option(&conn, 4242, ROLE_A, "Alpha", "a"),
            Err(RepoError::MenuMissing)
        );
        // A duplicate option of a menu that exists is still a duplicate.
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        add_option(&conn, id, ROLE_A, "Alpha", "a").unwrap();
        assert_eq!(
            add_option(&conn, id, ROLE_A, "Other", "b"),
            Err(RepoError::OptionExists)
        );
    }

    #[test]
    fn a_stored_id_of_zero_is_a_corrupt_row() {
        let path = scratch("zero");
        let ps = path.to_str().unwrap();
        {
            let conn = open(ps);
            create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        }
        Connection::open(&path)
            .unwrap()
            .execute("UPDATE role_menus SET channel = '0' WHERE key = 'main'", [])
            .unwrap();
        let conn = open(ps);
        assert_eq!(
            get_menu(&conn, GUILD, "main").unwrap_err(),
            RepoError::CorruptRow
        );
    }

    #[test]
    fn a_stored_role_of_zero_is_a_corrupt_row() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        add_option(&conn, id, ROLE_A, "Alpha", "a").unwrap();
        conn.execute(
            "UPDATE role_menu_options SET role = '0' WHERE menu_id = ?1",
            params![id],
        )
        .unwrap();
        assert_eq!(list_options(&conn, id).unwrap_err(), RepoError::CorruptRow);
    }

    #[test]
    fn set_published_records_the_channel_message_and_embed_key() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        set_published(&conn, id, CHANNEL, MESSAGE, Some("main")).unwrap();
        let menu = get_menu(&conn, GUILD, "main").unwrap().unwrap();
        assert_eq!(menu.channel, Some(CHANNEL));
        assert_eq!(menu.message, Some(MESSAGE));
        assert_eq!(menu.embed_key.as_deref(), Some("main"));
    }

    #[test]
    fn set_published_on_a_missing_menu_is_refused() {
        let conn = mem();
        assert_eq!(
            set_published(&conn, 999, CHANNEL, MESSAGE, None),
            Err(RepoError::MenuMissing)
        );
    }

    #[test]
    fn clear_published_clears_the_location_and_keeps_the_menu_and_its_options() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        add_option(&conn, id, ROLE_A, "Alpha", "a").unwrap();
        set_published(&conn, id, CHANNEL, MESSAGE, Some("main")).unwrap();

        clear_published(&conn, id).unwrap();
        let menu = get_menu(&conn, GUILD, "main").unwrap().unwrap();
        assert_eq!(menu.channel, None, "the channel is cleared");
        assert_eq!(menu.message, None, "the message is cleared");
        assert_eq!(menu.embed_key, None, "the embed key is cleared");
        // The menu and its options are untouched.
        assert_eq!(menu.key, "main");
        assert_eq!(list_options(&conn, id).unwrap().len(), 1);
    }

    #[test]
    fn clear_published_on_a_missing_menu_is_refused() {
        let conn = mem();
        assert_eq!(clear_published(&conn, 999), Err(RepoError::MenuMissing));
    }

    #[test]
    fn the_schema_creates_and_re_opens_cleanly_on_an_existing_database() {
        let path = scratch("reopen");
        let ps = path.to_str().unwrap();
        {
            let conn = open(ps);
            let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
            add_option(&conn, id, ROLE_A, "Alpha", "a").unwrap();
            set_published(&conn, id, CHANNEL, MESSAGE, Some("main")).unwrap();
        }
        // Re-opening an existing database applies the migrations again: no error,
        // and the stored rows are still there.
        let conn = open(ps);
        let menu = get_menu(&conn, GUILD, "main").unwrap().unwrap();
        assert_eq!(menu.message, Some(MESSAGE));
        assert_eq!(list_options(&conn, menu.id).unwrap().len(), 1);
    }

    #[test]
    fn a_stored_version_newer_than_the_code_is_refused() {
        let path = scratch("newer");
        let ps = path.to_str().unwrap();
        drop(open(ps));
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE schema_versions SET version = 99 WHERE feature = 'role_menus'",
                [],
            )
            .unwrap();
        let mut conn = crate::storage::open(ps, &[]).unwrap();
        assert_eq!(migrate(&mut conn), Err(StorageError::StoredVersionNewer));
    }

    #[test]
    fn a_corrupt_stored_id_is_an_error_not_a_default() {
        let path = scratch("corrupt");
        let ps = path.to_str().unwrap();
        {
            let conn = open(ps);
            create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        }
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE role_menus SET channel = 'not-a-number' WHERE key = 'main'",
                [],
            )
            .unwrap();
        let conn = open(ps);
        assert_eq!(
            get_menu(&conn, GUILD, "main").unwrap_err(),
            RepoError::CorruptRow
        );
    }

    #[test]
    fn error_codes_are_stable_and_carry_no_sql_or_path() {
        let cases = [
            (RepoError::CorruptRow, "corrupt_row"),
            (RepoError::KeyTaken, "key_taken"),
            (RepoError::OptionExists, "option_exists"),
            (RepoError::OptionMissing, "option_missing"),
            (RepoError::MenuMissing, "menu_missing"),
            (RepoError::InvalidMode, "invalid_mode"),
            (RepoError::Storage(StorageError::Sqlite), "sqlite_error"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(format!("{error}"), code);
            assert!(!format!("{error:?}").contains("SELECT"));
        }
    }

    #[test]
    fn update_menu_changes_mode_and_maximum_and_keeps_everything_else() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
        let other = create_menu(&conn, GUILD, "other", "exclusive", 0).unwrap();
        add_option(&conn, id, ROLE_A, "Alpha", "a").unwrap();
        set_published(&conn, id, CHANNEL, MESSAGE, Some("main")).unwrap();

        update_menu(&conn, id, "multi", 4).unwrap();
        let menu = get_menu(&conn, GUILD, "main").unwrap().unwrap();
        assert_eq!(menu.mode, "multi");
        assert_eq!(menu.max_select, 4);
        assert_eq!(menu.key, "main");
        assert_eq!(menu.channel, Some(CHANNEL));
        assert_eq!(menu.message, Some(MESSAGE));
        assert_eq!(menu.embed_key.as_deref(), Some("main"));
        assert_eq!(list_options(&conn, id).unwrap().len(), 1);
        // Another menu is not touched.
        let untouched = get_menu(&conn, GUILD, "other").unwrap().unwrap();
        assert_eq!(untouched.id, other);
        assert_eq!(
            (untouched.mode.as_str(), untouched.max_select),
            ("exclusive", 0)
        );
    }

    #[test]
    fn update_menu_refuses_an_unknown_mode_and_a_missing_menu_and_writes_nothing() {
        let conn = mem();
        let id = create_menu(&conn, GUILD, "main", "multi", 3).unwrap();
        for bad in ["", "garbage", "MULTI", "multi ", "Exclusive"] {
            assert_eq!(
                update_menu(&conn, id, bad, 1),
                Err(RepoError::InvalidMode),
                "{bad:?}"
            );
        }
        let menu = get_menu(&conn, GUILD, "main").unwrap().unwrap();
        assert_eq!((menu.mode.as_str(), menu.max_select), ("multi", 3));
        assert_eq!(
            update_menu(&conn, 999, "multi", 1),
            Err(RepoError::MenuMissing)
        );
    }

    #[test]
    fn update_menu_persists_across_a_reopen() {
        let path = scratch("update-reopen");
        let ps = path.to_str().unwrap();
        {
            let conn = open(ps);
            let id = create_menu(&conn, GUILD, "main", "exclusive", 0).unwrap();
            update_menu(&conn, id, "multi", 2).unwrap();
        }
        let conn = open(ps);
        let menu = get_menu(&conn, GUILD, "main").unwrap().unwrap();
        assert_eq!((menu.mode.as_str(), menu.max_select), ("multi", 2));
    }
}

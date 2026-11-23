//! SQLite persistence for role menus (RM2).
//!
//! RED STUB: the behaviour is not implemented yet; only the tests are.

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
            RepoError::Storage(e) => e.code(),
        }
    }
}

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
pub fn migrate(conn: &mut Connection) -> Result<u32, StorageError> {
    crate::storage::migrate(conn, FEATURE, &MIGRATIONS)
}

/// RED STUB: returns an empty id.
pub fn create_menu(
    _conn: &Connection,
    _guild: u64,
    _key: &str,
    _mode: &str,
    _max_select: u32,
) -> Result<i64, RepoError> {
    Err(RepoError::Storage(StorageError::Sqlite))
}

/// RED STUB: finds nothing.
pub fn get_menu(_conn: &Connection, _guild: u64, _key: &str) -> Result<Option<MenuRow>, RepoError> {
    Ok(None)
}

/// RED STUB: lists nothing.
pub fn list_menus(_conn: &Connection, _guild: u64) -> Result<Vec<MenuRow>, RepoError> {
    Ok(Vec::new())
}

/// RED STUB: stores nothing.
pub fn add_option(
    _conn: &Connection,
    _menu_id: i64,
    _role: u64,
    _label: &str,
    _emoji: &str,
) -> Result<i64, RepoError> {
    Err(RepoError::Storage(StorageError::Sqlite))
}

/// RED STUB: lists nothing.
pub fn list_options(_conn: &Connection, _menu_id: i64) -> Result<Vec<OptionRow>, RepoError> {
    Ok(Vec::new())
}

/// RED STUB: removes nothing.
pub fn remove_option(_conn: &Connection, _menu_id: i64, _role: u64) -> Result<bool, RepoError> {
    Ok(false)
}

/// RED STUB: records nothing.
pub fn set_published(
    _conn: &Connection,
    _menu_id: i64,
    _channel: u64,
    _message: u64,
    _embed_key: Option<&str>,
) -> Result<(), RepoError> {
    Err(RepoError::MenuMissing)
}

/// RED STUB: deletes nothing.
pub fn delete_menu(_conn: &Connection, _menu_id: i64) -> Result<bool, RepoError> {
    Ok(false)
}

/// RED STUB: counts nothing.
pub fn count_options(_conn: &Connection, _menu_id: i64) -> Result<u32, RepoError> {
    Ok(0)
}

#[cfg(test)]
mod unused_helpers {
    use super::*;

    #[test]
    fn the_stub_keeps_the_optional_import_used() {
        let conn = Connection::open_in_memory().unwrap();
        let value: Option<i64> = conn
            .query_row("SELECT 1", [], |r| r.get(0))
            .optional()
            .unwrap();
        assert_eq!(value, Some(1));
        assert_eq!(params!["x"].len(), 1);
    }
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
        assert!(delete_menu(&conn, id).unwrap());
        assert_eq!(list_options(&conn, id).unwrap().len(), 0);
        assert_eq!(get_menu(&conn, GUILD, "main").unwrap(), None);
        assert!(!delete_menu(&conn, id).unwrap());
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
            (RepoError::Storage(StorageError::Sqlite), "sqlite_error"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(format!("{error}"), code);
            assert!(!format!("{error:?}").contains("SELECT"));
        }
    }
}

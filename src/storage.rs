//! Core SQLite storage layer: opener and per-feature versioned migrations.
//!
//! Synchronous and Discord-free. Every new feature keeps its tables in ONE
//! shared file (`core.sqlite`, path supplied by the wiring card). This module
//! refuses to open any `reserved` path (the ticket and reaction-role files).
//!
//! Connection settings applied by [`open`]: `busy_timeout`=5000 ms, WAL journal
//! (file databases), `foreign_keys=ON`, `synchronous=NORMAL`.

// Temporary: wiring card W1 removes this once a production caller exists.
#![allow(dead_code)]

use rusqlite::{Connection, TransactionBehavior};
use std::path::{Component, Path, PathBuf};

/// Busy timeout applied to every connection, in milliseconds.
pub const BUSY_TIMEOUT_MS: u32 = 5000;

/// Stable error codes. Never carries SQL text, paths or user data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    /// Any SQLite engine error (message deliberately dropped).
    Sqlite,
    ReservedPath,
    InvalidPath,
    InvalidFeature,
    DuplicateVersion,
    NonContiguousVersions,
    StoredVersionNewer,
    CorruptStoredVersion,
    PragmaNotApplied,
}

impl StorageError {
    pub fn code(&self) -> &'static str {
        match self {
            StorageError::Sqlite => "sqlite_error",
            StorageError::ReservedPath => "reserved_path",
            StorageError::InvalidPath => "invalid_path",
            StorageError::InvalidFeature => "invalid_feature",
            StorageError::DuplicateVersion => "duplicate_version",
            StorageError::NonContiguousVersions => "non_contiguous_versions",
            StorageError::StoredVersionNewer => "stored_version_newer",
            StorageError::CorruptStoredVersion => "corrupt_stored_version",
            StorageError::PragmaNotApplied => "pragma_not_applied",
        }
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for StorageError {}

impl From<rusqlite::Error> for StorageError {
    fn from(_: rusqlite::Error) -> Self {
        StorageError::Sqlite
    }
}

/// One ordered migration step. Versions must run 1, 2, 3, ... with no gaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    pub version: u32,
    pub sql: &'static str,
}

pub fn open(path: &str, reserved: &[PathBuf]) -> Result<Connection, StorageError> {
    let _ = (path, reserved);
    unimplemented!("K01 RED stub")
}

pub fn migrate(
    conn: &mut Connection,
    feature: &str,
    migrations: &[Migration],
) -> Result<u32, StorageError> {
    let _ = (conn, feature, migrations);
    unimplemented!("K01 RED stub")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const M1: Migration = Migration {
        version: 1,
        sql: "CREATE TABLE a_items (id INTEGER PRIMARY KEY, v TEXT NOT NULL DEFAULT 'd');",
    };
    const M2: Migration = Migration {
        version: 2,
        sql: "CREATE TABLE a_more (id INTEGER PRIMARY KEY); INSERT INTO a_items (v) VALUES ('x');",
    };
    const M3: Migration = Migration {
        version: 3,
        sql: "ALTER TABLE a_items ADD COLUMN extra TEXT;",
    };

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("k01-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mem() -> Connection {
        open(":memory:", &[]).unwrap()
    }

    fn version_of(conn: &Connection, feature: &str) -> Option<i64> {
        conn.query_row(
            "SELECT version FROM schema_versions WHERE feature = ?1",
            [feature],
            |r| r.get(0),
        )
        .ok()
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    }

    #[test]
    fn fresh_db_creates_and_records_version() {
        let mut c = mem();
        assert_eq!(migrate(&mut c, "a", &[M1, M2]), Ok(2));
        assert_eq!(version_of(&c, "a"), Some(2));
        assert!(table_exists(&c, "a_items") && table_exists(&c, "a_more"));
    }

    #[test]
    fn rerun_is_idempotent() {
        let mut c = mem();
        migrate(&mut c, "a", &[M1, M2]).unwrap();
        // M2 inserts a row; a re-apply would add a second one.
        assert_eq!(migrate(&mut c, "a", &[M1, M2]), Ok(2));
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM a_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn later_call_applies_only_pending_steps() {
        let mut c = mem();
        migrate(&mut c, "a", &[M1]).unwrap();
        assert_eq!(migrate(&mut c, "a", &[M1, M2, M3]), Ok(3));
        assert_eq!(version_of(&c, "a"), Some(3));
    }

    #[test]
    fn empty_migration_list_is_a_noop_for_new_feature() {
        let mut c = mem();
        assert_eq!(migrate(&mut c, "a", &[]), Ok(0));
        assert_eq!(version_of(&c, "a"), None);
    }

    #[test]
    fn stored_version_newer_than_code_is_refused_with_zero_writes() {
        let dir = scratch("newer");
        let p = dir.join("core.sqlite");
        let ps = p.to_str().unwrap();
        {
            let mut c = open(ps, &[]).unwrap();
            migrate(&mut c, "a", &[M1, M2, M3]).unwrap();
        }
        let before = std::fs::read(&p).unwrap();
        {
            let mut c = open(ps, &[]).unwrap();
            let dump = |c: &Connection| -> Vec<(i64, String)> {
                let mut s = c.prepare("SELECT id, v FROM a_items").unwrap();
                let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
                rows.map(|r| r.unwrap()).collect()
            };
            let rows_before = dump(&c);
            assert_eq!(
                migrate(&mut c, "a", &[M1, M2]),
                Err(StorageError::StoredVersionNewer)
            );
            assert_eq!(version_of(&c, "a"), Some(3));
            assert_eq!(dump(&c), rows_before);
        }
        let after = std::fs::read(&p).unwrap();
        assert!(before == after, "db file changed on refused migrate");
    }

    #[test]
    fn stored_version_newer_with_empty_list_is_refused() {
        let mut c = mem();
        migrate(&mut c, "a", &[M1]).unwrap();
        assert_eq!(
            migrate(&mut c, "a", &[]),
            Err(StorageError::StoredVersionNewer)
        );
    }

    #[test]
    fn failing_middle_step_rolls_back_whole_call() {
        let mut c = mem();
        migrate(&mut c, "a", &[M1]).unwrap();
        let bad = Migration {
            version: 3,
            sql: "CREATE TABLE a_tmp (id INTEGER); INSERT INTO no_such_table_zz VALUES (1);",
        };
        let m4 = Migration {
            version: 4,
            sql: "CREATE TABLE a_after (id INTEGER);",
        };
        assert_eq!(
            migrate(&mut c, "a", &[M1, M2, bad, m4]),
            Err(StorageError::Sqlite)
        );
        assert_eq!(version_of(&c, "a"), Some(1));
        assert!(!table_exists(&c, "a_more"), "step 2 leaked");
        assert!(!table_exists(&c, "a_tmp"), "step 3 partial leaked");
        assert!(!table_exists(&c, "a_after"));
        // the same connection is usable afterwards (no dangling transaction)
        assert_eq!(migrate(&mut c, "a", &[M1, M2]), Ok(2));
    }

    #[test]
    fn failing_first_run_records_nothing() {
        let mut c = mem();
        let bad = Migration {
            version: 1,
            sql: "THIS IS NOT SQL;",
        };
        assert_eq!(migrate(&mut c, "a", &[bad]), Err(StorageError::Sqlite));
        assert_eq!(version_of(&c, "a"), None);
    }

    #[test]
    fn features_migrate_independently() {
        let mut c = mem();
        migrate(&mut c, "a", &[M1]).unwrap();
        let bad = Migration {
            version: 1,
            sql: "CREATE TABLE b_one (id INTEGER); SELECT * FROM nope_zz;",
        };
        assert_eq!(migrate(&mut c, "b", &[bad]), Err(StorageError::Sqlite));
        assert_eq!(version_of(&c, "a"), Some(1));
        assert_eq!(version_of(&c, "b"), None);
        assert!(!table_exists(&c, "b_one"));
        let b_ok = Migration {
            version: 1,
            sql: "CREATE TABLE b_one (id INTEGER);",
        };
        assert_eq!(migrate(&mut c, "b", &[b_ok]), Ok(1));
        assert_eq!(migrate(&mut c, "a", &[M1, M2]), Ok(2));
        assert_eq!(version_of(&c, "b"), Some(1));
    }

    #[test]
    fn newer_feature_does_not_block_other_features() {
        let mut c = mem();
        migrate(&mut c, "a", &[M1, M2]).unwrap();
        assert!(migrate(&mut c, "a", &[M1]).is_err());
        let b = Migration {
            version: 1,
            sql: "CREATE TABLE b_one (id INTEGER);",
        };
        assert_eq!(migrate(&mut c, "b", &[b]), Ok(1));
    }

    #[test]
    fn invalid_step_lists_rejected_before_any_write() {
        let mut c = mem();
        let dup = [M1, M1];
        let gap = [M1, M3];
        let from_two = [M2, M3];
        let desc = [M2, M1];
        let zero = [Migration {
            version: 0,
            sql: "SELECT 1;",
        }];
        assert_eq!(
            migrate(&mut c, "a", &dup),
            Err(StorageError::DuplicateVersion)
        );
        assert_eq!(
            migrate(&mut c, "a", &gap),
            Err(StorageError::NonContiguousVersions)
        );
        assert_eq!(
            migrate(&mut c, "a", &from_two),
            Err(StorageError::NonContiguousVersions)
        );
        assert_eq!(
            migrate(&mut c, "a", &desc),
            Err(StorageError::NonContiguousVersions)
        );
        assert_eq!(
            migrate(&mut c, "a", &zero),
            Err(StorageError::NonContiguousVersions)
        );
        assert_eq!(
            migrate(&mut c, "", &[M1]),
            Err(StorageError::InvalidFeature)
        );
        assert!(
            !table_exists(&c, "schema_versions"),
            "validation must run before any write"
        );
        assert!(!table_exists(&c, "a_items"));
    }

    #[test]
    fn file_open_sets_pragmas() {
        let dir = scratch("pragmas");
        let p = dir.join("core.sqlite");
        let c = open(p.to_str().unwrap(), &[]).unwrap();
        let s = |q: &str| -> String {
            c.query_row(q, [], |r| r.get::<_, rusqlite::types::Value>(0))
                .map(|v| match v {
                    rusqlite::types::Value::Integer(i) => i.to_string(),
                    rusqlite::types::Value::Text(t) => t,
                    _ => String::new(),
                })
                .unwrap()
        };
        assert_eq!(s("PRAGMA journal_mode"), "wal");
        assert_eq!(s("PRAGMA foreign_keys"), "1");
        assert_eq!(s("PRAGMA synchronous"), "1"); // NORMAL
        assert_eq!(s("PRAGMA busy_timeout"), BUSY_TIMEOUT_MS.to_string());
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let mut c = mem();
        let m = Migration {
            version: 1,
            sql: "CREATE TABLE p (id INTEGER PRIMARY KEY); CREATE TABLE ch (pid INTEGER REFERENCES p(id));",
        };
        migrate(&mut c, "fk", &[m]).unwrap();
        assert!(c.execute("INSERT INTO ch VALUES (99)", []).is_err());
    }

    #[test]
    fn memory_works() {
        let mut c = open(":memory:", &[PathBuf::from(":memory:")]).unwrap();
        assert_eq!(migrate(&mut c, "a", &[M1]), Ok(1));
        let fk: i64 = c
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn reserved_path_refused_in_all_spellings() {
        let dir = scratch("reserved");
        let reserved = dir.join("tickets.sqlite");
        std::fs::write(&reserved, b"").unwrap();
        let before = std::fs::read(&reserved).unwrap();
        let rs = [reserved.clone()];
        let abs = reserved.to_str().unwrap().to_string();
        let mut spellings = vec![
            abs.clone(),
            format!("{}/./tickets.sqlite", dir.to_str().unwrap()),
            format!("{}/sub/../tickets.sqlite", dir.to_str().unwrap()),
            format!("{}/.//tickets.sqlite", dir.to_str().unwrap()),
        ];
        // relative spelling from the current directory
        let cwd = std::env::current_dir().unwrap();
        if let Ok(rel) = reserved.strip_prefix(&cwd) {
            spellings.push(rel.to_str().unwrap().to_string());
            spellings.push(format!("./{}", rel.to_str().unwrap()));
        }
        if cfg!(windows) {
            spellings.push(abs.to_uppercase());
            spellings.push(abs.replace('/', "\\"));
            spellings.push(abs.replace('\\', "/"));
            spellings.push(abs.to_lowercase());
        }
        for sp in &spellings {
            assert_eq!(
                open(sp, &rs).err(),
                Some(StorageError::ReservedPath),
                "spelling not refused: {sp}"
            );
        }
        assert!(std::fs::read(&reserved).unwrap() == before);
        assert!(!dir.join("tickets.sqlite-wal").exists());
    }

    #[test]
    fn reserved_nonexistent_path_and_trailing_separator_refused() {
        let dir = scratch("reserved2");
        let reserved = dir.join("rr.sqlite");
        let rs = [dir.join("other.sqlite"), reserved.clone()];
        let sp = format!("{}/x/../rr.sqlite", dir.to_str().unwrap());
        assert_eq!(open(&sp, &rs).err(), Some(StorageError::ReservedPath));
        // reserved given with a trailing separator is still the same path
        let rs2 = [PathBuf::from(format!("{}/", reserved.to_str().unwrap()))];
        assert_eq!(
            open(reserved.to_str().unwrap(), &rs2).err(),
            Some(StorageError::ReservedPath)
        );
        assert!(!reserved.exists(), "refused open must not create the file");
    }

    #[test]
    fn non_reserved_path_opens_and_creates_file() {
        let dir = scratch("ok");
        let reserved = [dir.join("tickets.sqlite"), dir.join("roles.sqlite")];
        let p = dir.join("core.sqlite");
        let _c = open(p.to_str().unwrap(), &reserved).unwrap();
        assert!(p.exists());
        assert!(!reserved[0].exists() && !reserved[1].exists());
    }

    #[test]
    fn empty_path_rejected() {
        assert_eq!(open("", &[]).err(), Some(StorageError::InvalidPath));
    }

    #[test]
    fn concurrent_migrations_on_one_file_do_not_corrupt() {
        let dir = scratch("concurrent");
        let p = dir.join("core.sqlite");
        let ps = p.to_str().unwrap().to_string();
        // create the file first so both threads race on migrate, not on creation
        drop(open(&ps, &[]).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let ps = ps.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    let mut c = open(&ps, &[]).unwrap();
                    b.wait();
                    migrate(&mut c, "a", &[M1, M2, M3])
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), Ok(3));
        }
        let c = open(&ps, &[]).unwrap();
        assert_eq!(version_of(&c, "a"), Some(3));
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM a_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "step 2 applied exactly once");
        let ic: String = c
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ic, "ok");
    }

    #[test]
    fn errors_carry_no_sql_or_path_text() {
        let mut c = mem();
        let bad = Migration {
            version: 1,
            sql: "INSERT INTO secret_table_marker_zz VALUES ('private_value_zz');",
        };
        let e = migrate(&mut c, "feat_marker_zz", &[bad]).unwrap_err();
        let dir = scratch("leak");
        let reserved = dir.join("leak_marker_zz.sqlite");
        let e2 = open(reserved.to_str().unwrap(), &[reserved.clone()]).unwrap_err();
        let bad_open = open(dir.to_str().unwrap(), &[]).unwrap_err(); // a directory
        for err in [e, e2, bad_open] {
            let text = format!("{err:?} | {err}");
            for needle in [
                "marker_zz",
                "private_value",
                "INSERT",
                "k01-tests",
                "no such",
            ] {
                assert!(!text.contains(needle), "leaked {needle} in {text}");
            }
            assert!(text.len() < 80, "{text}");
        }
        assert_eq!(
            StorageError::Sqlite.to_string(),
            StorageError::Sqlite.code()
        );
    }

    #[test]
    fn error_codes_are_distinct_and_stable() {
        let all = [
            StorageError::Sqlite,
            StorageError::ReservedPath,
            StorageError::InvalidPath,
            StorageError::InvalidFeature,
            StorageError::DuplicateVersion,
            StorageError::NonContiguousVersions,
            StorageError::StoredVersionNewer,
            StorageError::CorruptStoredVersion,
            StorageError::PragmaNotApplied,
        ];
        let mut codes: Vec<_> = all.iter().map(|e| e.code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        assert_eq!(
            StorageError::StoredVersionNewer.code(),
            "stored_version_newer"
        );
    }
}

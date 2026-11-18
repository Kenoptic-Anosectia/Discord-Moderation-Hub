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

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
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
    /// A step ended the surrounding transaction (COMMIT/ROLLBACK/END).
    StepEndedTransaction,
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
            StorageError::StepEndedTransaction => "step_ended_transaction",
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

/// Lexically normalise a path: absolute, no `.`/`..`, no trailing separator,
/// symlinks/8.3 names resolved for the longest existing ancestor, and
/// lower-cased on Windows. Used only for comparison, never for opening.
fn normalize(path: &Path) -> Result<String, StorageError> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| StorageError::InvalidPath)?
            .join(path)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) if cfg!(windows) => {
                // Win32 silently drops trailing dots/spaces from a component.
                let text = name.to_string_lossy();
                let trimmed = text.trim_end_matches(['.', ' ']);
                if trimmed.is_empty() {
                    return Err(StorageError::InvalidPath);
                }
                out.push(trimmed);
            }
            other => out.push(other.as_os_str()),
        }
    }
    // Resolve the deepest existing ancestor (symlinks, short names).
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut base = out.clone();
    let resolved = loop {
        if let Ok(c) = base.canonicalize() {
            break c;
        }
        match (base.file_name().map(|n| n.to_os_string()), base.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                base = parent.to_path_buf();
            }
            _ => break out.clone(),
        }
    };
    let mut full = resolved;
    for part in tail.iter().rev() {
        full.push(part);
    }
    let mut s = full.to_string_lossy().replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) {
        s = s.to_lowercase();
    }
    Ok(s)
}

fn pragma_text(conn: &Connection, sql: &str) -> Result<String, StorageError> {
    let v: rusqlite::types::Value = conn.query_row(sql, [], |r| r.get(0))?;
    Ok(match v {
        rusqlite::types::Value::Integer(i) => i.to_string(),
        rusqlite::types::Value::Text(t) => t.to_lowercase(),
        _ => String::new(),
    })
}

/// Open (creating if needed) the core database at `path`, refusing any path
/// that normalises to one of `reserved`. `":memory:"` opens an in-memory DB.
pub fn open(path: &str, reserved: &[PathBuf]) -> Result<Connection, StorageError> {
    if path.is_empty() {
        return Err(StorageError::InvalidPath);
    }
    if path == ":memory:" {
        let conn = Connection::open_in_memory()?;
        apply_pragmas(&conn, false)?;
        return Ok(conn);
    }
    // `file:` URIs and UNC/admin-share spellings can alias a reserved file.
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("file:")
        || path.starts_with("\\\\")
        || (cfg!(windows) && path.starts_with("//"))
    {
        return Err(StorageError::InvalidPath);
    }
    if cfg!(windows) && windows_path_is_aliasing_form(Path::new(path)) {
        return Err(StorageError::InvalidPath);
    }
    let wanted = normalize(Path::new(path))?;
    for r in reserved {
        if normalize(r)? == wanted {
            return Err(StorageError::ReservedPath);
        }
    }
    // Explicit flags: no SQLITE_OPEN_URI, so the path is never parsed as a URI.
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    apply_pragmas(&conn, true)?;
    Ok(conn)
}

/// Windows-only: true for any path prefix other than a plain drive (UNC in any
/// separator mix, verbatim, device) and for NTFS alternate-data-stream spellings
/// (a `:` inside a normal component). Checked on components, not on strings.
fn windows_path_is_aliasing_form(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Prefix(p) => !matches!(p.kind(), std::path::Prefix::Disk(_)),
        Component::Normal(n) => n.to_string_lossy().contains(':'),
        _ => false,
    })
}

fn apply_pragmas(conn: &Connection, wal: bool) -> Result<(), StorageError> {
    conn.busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))?;
    if wal && pragma_text(conn, "PRAGMA journal_mode = WAL")? != "wal" {
        return Err(StorageError::PragmaNotApplied);
    }
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")?;
    if pragma_text(conn, "PRAGMA foreign_keys")? != "1"
        || pragma_text(conn, "PRAGMA synchronous")? != "1"
    {
        return Err(StorageError::PragmaNotApplied);
    }
    Ok(())
}

fn validate(feature: &str, migrations: &[Migration]) -> Result<(), StorageError> {
    if feature.is_empty() {
        return Err(StorageError::InvalidFeature);
    }
    let mut prev = 0u32;
    for m in migrations {
        if m.version == prev && prev != 0 {
            return Err(StorageError::DuplicateVersion);
        }
        if migrations.iter().filter(|o| o.version == m.version).count() > 1 {
            return Err(StorageError::DuplicateVersion);
        }
        if m.version != prev + 1 {
            return Err(StorageError::NonContiguousVersions);
        }
        prev = m.version;
    }
    Ok(())
}

/// Apply every pending step for `feature` in ONE `BEGIN IMMEDIATE`
/// transaction together with the version bump. Returns the resulting version.
/// A stored version newer than the code knows is refused with no writes.
/// Step SQL must not contain its own transaction control (BEGIN/COMMIT).
pub fn migrate(
    conn: &mut Connection,
    feature: &str,
    migrations: &[Migration],
) -> Result<u32, StorageError> {
    validate(feature, migrations)?;
    let latest = migrations.last().map_or(0, |m| m.version);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_versions \
         (feature TEXT PRIMARY KEY, version INTEGER NOT NULL)",
    )?;
    let stored: Option<rusqlite::types::Value> = match tx.query_row(
        "SELECT version FROM schema_versions WHERE feature = ?1",
        [feature],
        |r| r.get(0),
    ) {
        Ok(v) => Some(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(_) => return Err(StorageError::Sqlite),
    };
    let current = match stored {
        None => 0u32,
        Some(rusqlite::types::Value::Integer(v)) => {
            u32::try_from(v).map_err(|_| StorageError::CorruptStoredVersion)?
        }
        Some(_) => return Err(StorageError::CorruptStoredVersion),
    };
    if current > latest {
        return Err(StorageError::StoredVersionNewer); // tx dropped => rollback
    }
    if current == latest {
        return Ok(current);
    }
    for m in migrations.iter().filter(|m| m.version > current) {
        tx.execute_batch(m.sql)?;
        if tx.is_autocommit() {
            // The step ran COMMIT/ROLLBACK/END itself: stop before any later
            // step or the version bump. Already-committed work cannot be undone.
            return Err(StorageError::StepEndedTransaction);
        }
    }
    tx.execute(
        "INSERT INTO schema_versions (feature, version) VALUES (?1, ?2) \
         ON CONFLICT(feature) DO UPDATE SET version = excluded.version",
        rusqlite::params![feature, i64::from(latest)],
    )?;
    tx.commit()?;
    Ok(latest)
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
        let e2 = open(reserved.to_str().unwrap(), std::slice::from_ref(&reserved)).unwrap_err();
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

    #[test]
    fn file_uri_spellings_refused_and_reserved_file_untouched() {
        let dir = scratch("uri");
        let reserved = dir.join("tickets.sqlite");
        {
            let c = Connection::open(&reserved).unwrap();
            c.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (42);")
                .unwrap();
        }
        let before = std::fs::read(&reserved).unwrap();
        let abs = reserved.to_str().unwrap().replace('\\', "/");
        let spellings = [
            format!("file:{abs}"),
            format!("file:///{abs}"),
            format!("file:{abs}?cache=shared"),
            format!("FILE:{abs}"),
            "file::memory:".to_string(),
        ];
        for sp in &spellings {
            assert!(
                open(sp, std::slice::from_ref(&reserved)).is_err(),
                "uri spelling accepted: {sp}"
            );
        }
        assert!(
            std::fs::read(&reserved).unwrap() == before,
            "reserved file changed"
        );
        assert!(!dir.join("tickets.sqlite-wal").exists());
    }

    #[test]
    fn unc_style_paths_are_refused() {
        assert_eq!(
            open("\\\\localhost\\C$\\x\\tickets.sqlite", &[]).err(),
            Some(StorageError::InvalidPath)
        );
        if cfg!(windows) {
            assert_eq!(
                open("//localhost/C$/x/tickets.sqlite", &[]).err(),
                Some(StorageError::InvalidPath)
            );
        }
    }

    #[test]
    fn windows_mixed_separator_unc_spellings_refused_and_reserved_untouched() {
        if !cfg!(windows) {
            return;
        }
        let dir = scratch("mixedunc");
        let reserved = dir.join("tickets.sqlite");
        {
            let c = Connection::open(&reserved).unwrap();
            c.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (42);")
                .unwrap();
        }
        let before = std::fs::read(&reserved).unwrap();
        let abs = reserved.to_str().unwrap();
        let share = format!("{}$", &abs[..1]);
        let rest = abs[2..].trim_start_matches(['\\', '/']);
        for lead in ["/\\", "\\/", "\\\\", "//"] {
            let sp = format!("{lead}localhost\\{share}\\{rest}");
            assert!(
                open(&sp, std::slice::from_ref(&reserved)).is_err(),
                "unc spelling accepted: {sp}"
            );
        }
        for sp in [
            "\\\\?\\UNC\\localhost\\C$\\x",
            "\\\\.\\C:\\x",
            "/\\?\\C:\\x",
        ] {
            assert!(open(sp, &[]).is_err(), "prefix accepted: {sp}");
        }
        assert!(
            std::fs::read(&reserved).unwrap() == before,
            "reserved changed"
        );
        let missing = dir.join("roles.sqlite");
        let mrest = missing.to_str().unwrap()[2..].trim_start_matches(['\\', '/']);
        let sp = format!("/\\localhost\\{share}\\{mrest}");
        assert!(open(&sp, std::slice::from_ref(&missing)).is_err());
        assert!(!missing.exists(), "reserved file created via UNC spelling");
    }

    #[test]
    fn windows_alternate_data_stream_spellings_refused() {
        if !cfg!(windows) {
            return;
        }
        let dir = scratch("ads");
        let d = dir.to_str().unwrap();
        let missing = dir.join("roles.sqlite");
        for sp in [
            format!("{d}/roles.sqlite:stream"),
            format!("{d}/roles.sqlite::$DATA"),
            format!("{d}/roles.sqlite:x:$DATA"),
        ] {
            assert_eq!(
                open(&sp, std::slice::from_ref(&missing)).err(),
                Some(StorageError::InvalidPath),
                "ads accepted: {sp}"
            );
            assert!(!missing.exists(), "reserved file created via {sp}");
        }
        let existing = dir.join("tickets.sqlite");
        std::fs::write(&existing, b"x").unwrap();
        let sp = format!("{d}/tickets.sqlite:stream");
        assert!(open(&sp, std::slice::from_ref(&existing)).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"x");
        // A plain drive colon still works.
        assert!(open(&format!("{d}/core.sqlite"), &[]).is_ok());
    }

    #[test]
    fn windows_trailing_dot_and_space_spellings_of_missing_reserved_file_refused() {
        if !cfg!(windows) {
            return;
        }
        let dir = scratch("trailing");
        let reserved = dir.join("roles.sqlite");
        let d = dir.to_str().unwrap();
        for sp in [
            format!("{d}/roles.sqlite."),
            format!("{d}/roles.sqlite "),
            format!("{d}/roles.sqlite. ."),
            format!("{d}/roles.sqlite../"),
        ] {
            assert_eq!(
                open(&sp, std::slice::from_ref(&reserved)).err(),
                Some(StorageError::ReservedPath),
                "not refused: {sp:?}"
            );
            assert!(!reserved.exists(), "reserved file created via {sp:?}");
        }
        assert_eq!(
            open(&format!("{d}/..."), &[]).err(),
            Some(StorageError::InvalidPath)
        );
    }

    #[test]
    fn step_that_commits_is_refused_and_no_later_step_runs() {
        let mut c = mem();
        let s1 = Migration {
            version: 1,
            sql: "CREATE TABLE t1 (x); COMMIT;",
        };
        let s2 = Migration {
            version: 2,
            sql: "CREATE TABLE t2 (x);",
        };
        assert_eq!(
            migrate(&mut c, "a", &[s1, s2]),
            Err(StorageError::StepEndedTransaction)
        );
        assert!(!table_exists(&c, "t2"), "later step ran");
        assert_eq!(version_of(&c, "a"), None, "version bumped");
    }

    #[test]
    fn step_that_commits_last_is_not_reported_as_success() {
        let mut c = mem();
        let s1 = Migration {
            version: 1,
            sql: "CREATE TABLE t1 (x); END;",
        };
        assert_eq!(
            migrate(&mut c, "a", &[s1]),
            Err(StorageError::StepEndedTransaction)
        );
        assert_eq!(version_of(&c, "a"), None);
    }

    #[test]
    fn step_that_rolls_back_is_refused_and_no_later_step_runs() {
        let mut c = mem();
        let s1 = Migration {
            version: 1,
            sql: "CREATE TABLE t1 (x);",
        };
        let s2 = Migration {
            version: 2,
            sql: "ROLLBACK;",
        };
        let s3 = Migration {
            version: 3,
            sql: "CREATE TABLE t3 (x);",
        };
        assert_eq!(
            migrate(&mut c, "a", &[s1, s2, s3]),
            Err(StorageError::StepEndedTransaction)
        );
        assert!(!table_exists(&c, "t3"), "later step ran");
        assert_eq!(version_of(&c, "a"), None);
        // connection still usable afterwards
        assert_eq!(migrate(&mut c, "b", &[M1]), Ok(1));
    }

    #[test]
    fn trigger_with_begin_end_inside_step_still_works() {
        let mut c = mem();
        let s1 = Migration {
            version: 1,
            sql: "CREATE TABLE t (x); CREATE TABLE log (y); \
                  CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.x); END;",
        };
        assert_eq!(migrate(&mut c, "a", &[s1]), Ok(1));
        c.execute("INSERT INTO t VALUES (7)", []).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn non_integer_stored_version_is_corrupt_not_sqlite() {
        for bad in ["'abc'", "1.5"] {
            let mut c = mem();
            migrate(&mut c, "a", &[M1]).unwrap();
            c.execute_batch(&format!(
                "UPDATE schema_versions SET version = {bad} WHERE feature = 'a';"
            ))
            .unwrap();
            assert_eq!(
                migrate(&mut c, "a", &[M1]),
                Err(StorageError::CorruptStoredVersion),
                "{bad}"
            );
        }
    }
}

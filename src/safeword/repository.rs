//! SQLite persistence for the safeword state machine (SW2).
//!
//! One row per (guild, user) in `safeword_state` inside the shared core database.
//! Writes are compare-and-swap on `version`. Holds no Discord types and reads no clock:
//! the caller passes ids and `now`.

use super::state::{ActionId, FailureCode, RoleId, Snapshot, State};
use crate::storage::{self, Migration, StorageError};
use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Row version. `0` means "no row yet"; the first successful write produces `1`.
pub type Version = u64;
/// The version a successful write produced.
pub type NewVersion = Version;

/// Feature name used with `storage::migrate`.
pub const FEATURE: &str = "safeword";

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: "CREATE TABLE safeword_state (\
          guild TEXT NOT NULL, user TEXT NOT NULL, state TEXT NOT NULL, \
          snapshot_json TEXT NOT NULL, struck_json TEXT NOT NULL, deferred_json TEXT NOT NULL, \
          version INTEGER NOT NULL CHECK (version >= 1), updated_at INTEGER NOT NULL, \
          PRIMARY KEY (guild, user));",
}];

/// Stable error codes. Never carries SQL text, JSON text, ids or paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoError {
    /// The stored version differs from the caller's expected version; nothing was written.
    Stale,
    /// A stored JSON column does not parse into the expected shape.
    CorruptJson,
    /// The stored `state` text is not one of the four known states.
    UnknownState,
    /// A stored column has the wrong type or an impossible value.
    CorruptRow,
    /// The expected version cannot be represented (or incremented) in SQLite.
    InvalidVersion,
    /// Underlying storage failure (code from the storage layer).
    Storage(StorageError),
}

impl RepoError {
    pub fn code(&self) -> &'static str {
        match self {
            RepoError::Stale => "stale_version",
            RepoError::CorruptJson => "corrupt_snapshot",
            RepoError::UnknownState => "unknown_state",
            RepoError::CorruptRow => "corrupt_row",
            RepoError::InvalidVersion => "invalid_version",
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

/// Safeword state store over one core-database connection.
#[derive(Debug)]
pub struct Repository {
    conn: Connection,
}

impl Repository {
    /// Opens the core database (refusing `reserved` paths) and applies the safeword migrations.
    pub fn open(path: &str, reserved: &[PathBuf]) -> Result<Self, RepoError> {
        Self::new(storage::open(path, reserved)?)
    }

    /// Applies the safeword migrations to an already opened connection.
    pub fn new(mut conn: Connection) -> Result<Self, RepoError> {
        storage::migrate(&mut conn, FEATURE, MIGRATIONS)?;
        Ok(Self { conn })
    }

    /// Loads one member. `Ok(None)` means no row (treat as `Inactive` at version 0).
    /// A row that cannot be read back exactly is an error, never a default state.
    pub fn load(&self, guild: u64, user: u64) -> Result<Option<(State, Version)>, RepoError> {
        let Some(raw) = read_raw(&self.conn, guild, user)? else {
            return Ok(None);
        };
        let version = stored_version(&raw)?;
        Ok(Some((decode_state(&raw)?, version)))
    }

    /// Compare-and-swap write. Applies only when the stored version equals `expected_version`
    /// (`0` = no row). Returns the new version, or `Stale` with nothing written. A row that
    /// cannot be decoded is reported as an error and left as it is, not overwritten.
    pub fn store(
        &mut self,
        guild: u64,
        user: u64,
        state: &State,
        expected_version: Version,
        now: i64,
    ) -> Result<NewVersion, RepoError> {
        let write = Write {
            key: (guild, user),
            encoded: encode_state(state)?,
            expected_version,
            now,
            create_if_missing: true,
        };
        self.write(&write)
    }

    /// Compare-and-swap reset to `Inactive`. The row is kept (empty, version bumped) rather than
    /// deleted, so a writer that read an older version, or "no row", can never write over it.
    /// Clearing a member with no row and `expected_version == 0` writes nothing and returns `0`.
    pub fn clear(
        &mut self,
        guild: u64,
        user: u64,
        expected_version: Version,
        now: i64,
    ) -> Result<NewVersion, RepoError> {
        let write = Write {
            key: (guild, user),
            encoded: encode_state(&State::Inactive)?,
            expected_version,
            now,
            create_if_missing: false,
        };
        self.write(&write)
    }

    fn write(&mut self, w: &Write) -> Result<NewVersion, RepoError> {
        let new_version = w
            .expected_version
            .checked_add(1)
            .filter(|v| i64::try_from(*v).is_ok())
            .ok_or(RepoError::InvalidVersion)?;
        let (guild, user) = (w.key.0.to_string(), w.key.1.to_string());
        // IMMEDIATE: take the write lock before reading, so the version we check is the version
        // we replace. A losing writer waits (busy timeout) and then sees the winner's version.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        match read_raw(&tx, w.key.0, w.key.1)? {
            None if w.expected_version != 0 => return Err(RepoError::Stale),
            None if !w.create_if_missing => return Ok(0),
            None => {
                tx.execute(
                    "INSERT INTO safeword_state (guild, user, state, snapshot_json, struck_json, \
                     deferred_json, version, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        guild,
                        user,
                        w.encoded.state,
                        w.encoded.snapshot,
                        w.encoded.struck,
                        w.encoded.deferred,
                        new_version as i64,
                        w.now
                    ],
                )?;
            }
            Some(raw) => {
                if stored_version(&raw)? != w.expected_version {
                    return Err(RepoError::Stale);
                }
                decode_state(&raw)?; // never overwrite a row we cannot read
                let changed = tx.execute(
                    "UPDATE safeword_state SET state = ?3, snapshot_json = ?4, struck_json = ?5, \
                     deferred_json = ?6, version = ?7, updated_at = ?8 \
                     WHERE guild = ?1 AND user = ?2 AND version = ?9",
                    params![
                        guild,
                        user,
                        w.encoded.state,
                        w.encoded.snapshot,
                        w.encoded.struck,
                        w.encoded.deferred,
                        new_version as i64,
                        w.now,
                        w.expected_version as i64
                    ],
                )?;
                if changed != 1 {
                    return Err(RepoError::Stale);
                }
            }
        }
        tx.commit()?;
        Ok(new_version)
    }
}

/// One compare-and-swap write request.
struct Write {
    key: (u64, u64),
    encoded: Encoded,
    expected_version: Version,
    now: i64,
    create_if_missing: bool,
}

/// The four stored text columns for one state.
struct Encoded {
    state: &'static str,
    snapshot: String,
    struck: String,
    deferred: String,
}

/// Raw column values, read untyped so a wrong-typed value is reported, not coerced.
struct RawRow {
    state: Value,
    snapshot: Value,
    struck: Value,
    deferred: Value,
    version: Value,
}

fn read_raw(conn: &Connection, guild: u64, user: u64) -> Result<Option<RawRow>, RepoError> {
    Ok(conn
        .query_row(
            "SELECT state, snapshot_json, struck_json, deferred_json, version \
             FROM safeword_state WHERE guild = ?1 AND user = ?2",
            params![guild.to_string(), user.to_string()],
            |r| {
                Ok(RawRow {
                    state: r.get(0)?,
                    snapshot: r.get(1)?,
                    struck: r.get(2)?,
                    deferred: r.get(3)?,
                    version: r.get(4)?,
                })
            },
        )
        .optional()?)
}

fn stored_version(raw: &RawRow) -> Result<Version, RepoError> {
    match raw.version {
        Value::Integer(v) if v >= 1 => Ok(v as u64),
        _ => Err(RepoError::CorruptRow),
    }
}

fn text_of(v: &Value) -> Result<&str, RepoError> {
    match v {
        Value::Text(t) => Ok(t),
        _ => Err(RepoError::CorruptRow),
    }
}

const INACTIVE: &str = "inactive";
const ACTIVATING: &str = "activating";
const ACTIVE: &str = "active";
const RELEASING: &str = "releasing";
const EMPTY_OBJECT: &str = "{}";
const EMPTY_LIST: &str = "[]";

fn encode_state(state: &State) -> Result<Encoded, RepoError> {
    let (name, snapshot) = match state {
        State::Inactive => {
            return Ok(Encoded {
                state: INACTIVE,
                snapshot: EMPTY_OBJECT.to_string(),
                struck: EMPTY_LIST.to_string(),
                deferred: EMPTY_LIST.to_string(),
            })
        }
        State::Activating(s) => (ACTIVATING, s),
        State::Active(s) => (ACTIVE, s),
        State::Releasing(s) => (RELEASING, s),
    };
    let (dto, struck, deferred) = SnapshotDto::split(snapshot);
    let json = |r: Result<String, serde_json::Error>| r.map_err(|_| RepoError::CorruptJson);
    Ok(Encoded {
        state: name,
        snapshot: json(serde_json::to_string(&dto))?,
        struck: json(serde_json::to_string(&struck))?,
        deferred: json(serde_json::to_string(&deferred))?,
    })
}

/// Parses a row strictly: known state text, exact JSON shapes, nothing defaulted except the
/// fields documented on `SnapshotDto`.
fn decode_state(raw: &RawRow) -> Result<State, RepoError> {
    let name = text_of(&raw.state)?;
    if !matches!(name, INACTIVE | ACTIVATING | ACTIVE | RELEASING) {
        return Err(RepoError::UnknownState);
    }
    let (snapshot, struck, deferred) = (
        text_of(&raw.snapshot)?,
        text_of(&raw.struck)?,
        text_of(&raw.deferred)?,
    );
    if name == INACTIVE {
        // An inactive member carries no protection data; anything else is a damaged row.
        return if (snapshot, struck, deferred) == (EMPTY_OBJECT, EMPTY_LIST, EMPTY_LIST) {
            Ok(State::Inactive)
        } else {
            Err(RepoError::CorruptRow)
        };
    }
    let parse = |e: serde_json::Error| {
        let _ = e; // error text may echo stored JSON; keep only the code
        RepoError::CorruptJson
    };
    let dto: SnapshotDto = serde_json::from_str(snapshot).map_err(parse)?;
    let struck: Vec<RoleId> = serde_json::from_str(struck).map_err(parse)?;
    let deferred: Vec<RoleId> = serde_json::from_str(deferred).map_err(parse)?;
    let snapshot = dto.join(struck, deferred);
    Ok(match name {
        ACTIVATING => State::Activating(snapshot),
        ACTIVE => State::Active(snapshot),
        _ => State::Releasing(snapshot),
    })
}

/// Stored form of `Snapshot` minus `struck_roles` and `deferred_roles`, which have their own
/// columns. A private mirror, so `state.rs` stays free of serde. Unknown keys are refused (a
/// newer build's data is not silently dropped). Only the fields added to SW1 after its first
/// round (`failed_steps`, `restore_in_flight`, `last_emitted_restore_plan`) may be absent; they
/// default to empty/false. Every other field is required.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotDto {
    saved_roles: Vec<RoleId>,
    original_roles: Vec<RoleId>,
    deleted_roles: Vec<RoleId>,
    restore_plan: Vec<RoleId>,
    snapshot_saved: bool,
    roles_removed: bool,
    safeword_added: bool,
    dm_sent: bool,
    pending_release: bool,
    pending_reactivate: bool,
    safeword_removed: bool,
    roles_restored: bool,
    #[serde(default)]
    restore_in_flight: bool,
    #[serde(default)]
    last_emitted_restore_plan: Vec<RoleId>,
    snapshot_cleared: bool,
    release_dm_sent: bool,
    // serde treats a missing `Option` key as `None`; `deserialize_with` makes the key required.
    #[serde(deserialize_with = "required_option")]
    last_failure: Option<FailureDto>,
    #[serde(default)]
    failed_steps: Vec<ActionIdDto>,
}

/// Deserializes an `Option` whose key must be present (it may still be `null`).
fn required_option<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailureDto {
    step: ActionIdDto,
    code: FailureCodeDto,
}

impl SnapshotDto {
    /// Splits a snapshot into this DTO plus the struck and deferred lists. Destructures every
    /// field, so adding a field to `Snapshot` fails to compile here until it is persisted.
    fn split(s: &Snapshot) -> (Self, Vec<RoleId>, Vec<RoleId>) {
        let Snapshot {
            saved_roles,
            original_roles,
            struck_roles,
            deleted_roles,
            deferred_roles,
            restore_plan,
            snapshot_saved,
            roles_removed,
            safeword_added,
            dm_sent,
            pending_release,
            pending_reactivate,
            safeword_removed,
            roles_restored,
            restore_in_flight,
            last_emitted_restore_plan,
            snapshot_cleared,
            release_dm_sent,
            last_failure,
            failed_steps,
        } = s.clone();
        let dto = Self {
            saved_roles,
            original_roles,
            deleted_roles,
            restore_plan,
            snapshot_saved,
            roles_removed,
            safeword_added,
            dm_sent,
            pending_release,
            pending_reactivate,
            safeword_removed,
            roles_restored,
            restore_in_flight,
            last_emitted_restore_plan,
            snapshot_cleared,
            release_dm_sent,
            last_failure: last_failure.map(|(step, code)| FailureDto {
                step: step.into(),
                code: code.into(),
            }),
            failed_steps: failed_steps.into_iter().map(Into::into).collect(),
        };
        (dto, struck_roles, deferred_roles)
    }

    fn join(self, struck_roles: Vec<RoleId>, deferred_roles: Vec<RoleId>) -> Snapshot {
        Snapshot {
            saved_roles: self.saved_roles,
            original_roles: self.original_roles,
            struck_roles,
            deleted_roles: self.deleted_roles,
            deferred_roles,
            restore_plan: self.restore_plan,
            snapshot_saved: self.snapshot_saved,
            roles_removed: self.roles_removed,
            safeword_added: self.safeword_added,
            dm_sent: self.dm_sent,
            pending_release: self.pending_release,
            pending_reactivate: self.pending_reactivate,
            safeword_removed: self.safeword_removed,
            roles_restored: self.roles_restored,
            restore_in_flight: self.restore_in_flight,
            last_emitted_restore_plan: self.last_emitted_restore_plan,
            snapshot_cleared: self.snapshot_cleared,
            release_dm_sent: self.release_dm_sent,
            last_failure: self.last_failure.map(|f| (f.step.into(), f.code.into())),
            failed_steps: self.failed_steps.into_iter().map(Into::into).collect(),
        }
    }
}

/// Generates a serde mirror of a fieldless SW1 enum with exhaustive conversions both ways, so a
/// new SW1 variant fails to compile here instead of being lost.
macro_rules! mirror_enum {
    ($dto:ident <-> $real:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Serialize, Deserialize, Clone, Copy)]
        #[serde(rename_all = "snake_case")]
        enum $dto { $($variant),+ }
        impl From<$real> for $dto {
            fn from(v: $real) -> Self {
                match v { $($real::$variant => Self::$variant),+ }
            }
        }
        impl From<$dto> for $real {
            fn from(v: $dto) -> Self {
                match v { $($dto::$variant => Self::$variant),+ }
            }
        }
    };
}

mirror_enum!(ActionIdDto <-> ActionId {
    SaveSnapshot, RemoveRoles, AddSafewordRole, SendActivatedDm,
    RemoveSafewordRole, RestoreRoles, ClearSnapshot, SendReleasedDm,
});
mirror_enum!(FailureCodeDto <-> FailureCode {
    StorageError, MissingPermissions, HierarchyRejected, RateLimited,
    DmClosed, TransientHttp, UnknownRole,
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safeword::state::{
        restart_action, step, Action, ActionId, Context, Event, FailureCode, RestartAction,
        Snapshot,
    };
    use rusqlite::types::Value;
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier};

    const G: u64 = 111;
    const U: u64 = 222;
    const NOW: i64 = 1_000;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("sw2-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("core.sqlite")
    }

    fn open_at(path: &Path) -> Repository {
        Repository::open(path.to_str().unwrap(), &[]).unwrap()
    }

    fn mem() -> Repository {
        Repository::open(":memory:", &[]).unwrap()
    }

    fn ctx() -> Context {
        Context {
            safeword_role: 900,
            restricted_roles: vec![101, 102, 103, 104, 105],
            protected_roles: vec![],
            held_roles: vec![101, 102, 201, 104],
            filter_restore: true,
            defer_grants: true,
        }
    }

    /// Runs real `step` over `events` from `Inactive`, returning every state reached (first = Inactive).
    fn walk(events: &[Event]) -> Vec<State> {
        let c = ctx();
        let mut states = vec![State::Inactive];
        for e in events {
            let next = step(states.last().unwrap(), *e, &c).0;
            states.push(next);
        }
        states
    }

    fn lifecycle_events() -> Vec<Event> {
        use ActionId as A;
        use Event::*;
        vec![
            ReactionAdded,
            ActionFailed(A::SaveSnapshot, FailureCode::StorageError),
            ActionDone(A::SaveSnapshot),
            ActionDone(A::RemoveRoles),
            ActionFailed(A::AddSafewordRole, FailureCode::RateLimited),
            ActionDone(A::AddSafewordRole),
            ActionDone(A::SendActivatedDm),
            GrantDuringActive(105),
            Strike(101),
            RoleDeleted(102),
            ReactionRemoved,
            ActionDone(A::RemoveSafewordRole),
            ActionFailed(A::RestoreRoles, FailureCode::TransientHttp),
            ReactionRemoved,
            GrantDuringActive(103),
            ActionDone(A::RestoreRoles),
            ActionDone(A::RestoreRoles),
            ActionDone(A::ClearSnapshot),
            ActionDone(A::SendReleasedDm),
        ]
    }

    /// Release interrupted by a re-add: exercises `pending_reactivate` and re-activation.
    fn reactivate_events() -> Vec<Event> {
        use ActionId as A;
        use Event::*;
        vec![
            ReactionAdded,
            ActionDone(A::SaveSnapshot),
            ActionDone(A::RemoveRoles),
            ActionDone(A::AddSafewordRole),
            ActionDone(A::SendActivatedDm),
            ReactionRemoved,
            ReactionAdded,
            ActionDone(A::RemoveSafewordRole),
            ActionDone(A::RestoreRoles),
            ActionDone(A::ClearSnapshot),
            ActionDone(A::SendReleasedDm),
        ]
    }

    fn real_states() -> Vec<State> {
        let mut all = walk(&lifecycle_events());
        all.extend(walk(&reactivate_events()));
        all
    }

    fn snap(s: &State) -> Option<&Snapshot> {
        s.snapshot()
    }

    fn dump(path: &Path) -> Vec<Vec<Value>> {
        let c = Connection::open(path).unwrap();
        let mut st = c
            .prepare(
                "SELECT guild, user, state, snapshot_json, struck_json, deferred_json, \
                 version, updated_at FROM safeword_state ORDER BY guild, user",
            )
            .unwrap();
        let n = st.column_count();
        let rows = st
            .query_map([], |r| {
                (0..n)
                    .map(|i| r.get::<_, Value>(i))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// Overwrites one column of the (G, U) row behind the repository's back.
    fn poke(path: &Path, column: &str, value: Value) {
        assert!([
            "state",
            "snapshot_json",
            "struck_json",
            "deferred_json",
            "version"
        ]
        .contains(&column));
        let c = Connection::open(path).unwrap();
        let n = c
            .execute(
                &format!(
                    "UPDATE safeword_state SET {column} = ?1 WHERE guild = '111' AND user = '222'"
                ),
                [value],
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    fn column(path: &Path, column: &str) -> Value {
        let c = Connection::open(path).unwrap();
        c.query_row(
            &format!("SELECT {column} FROM safeword_state WHERE guild = '111' AND user = '222'"),
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    fn activating() -> State {
        walk(&[Event::ReactionAdded]).pop().unwrap()
    }

    fn assert_row_survives(path: &Path, repo: &mut Repository, expect: RepoError, why: &str) {
        let before = dump(path);
        assert_eq!(repo.load(G, U), Err(expect), "load: {why}");
        assert_eq!(
            repo.store(G, U, &State::Inactive, 1, NOW + 1),
            Err(expect),
            "store: {why}"
        );
        assert_eq!(repo.clear(G, U, 1, NOW + 1), Err(expect), "clear: {why}");
        assert_eq!(dump(path), before, "row changed: {why}");
    }

    #[test]
    fn missing_row_loads_as_none_and_is_only_creatable_with_version_zero() {
        let path = scratch("create");
        let mut repo = open_at(&path);
        assert_eq!(repo.load(G, U), Ok(None));
        for wrong in [1u64, 2, 99, u64::from(u32::MAX)] {
            assert_eq!(
                repo.store(G, U, &activating(), wrong, NOW),
                Err(RepoError::Stale),
                "expected {wrong}"
            );
        }
        assert_eq!(repo.load(G, U), Ok(None));
        assert!(dump(&path).is_empty());
        assert_eq!(repo.store(G, U, &activating(), 0, NOW), Ok(1));
        assert_eq!(repo.load(G, U), Ok(Some((activating(), 1))));
    }

    #[test]
    fn stale_version_update_is_rejected_and_writes_nothing() {
        let path = scratch("stale");
        let mut repo = open_at(&path);
        let states = real_states();
        let (a, b, c) = (&states[1], &states[2], &states[3]);
        assert_eq!(repo.store(G, U, a, 0, 10), Ok(1));
        assert_eq!(repo.store(G, U, b, 1, 20), Ok(2));
        let before = dump(&path);
        for wrong in [0u64, 1, 3, 4, 1_000] {
            assert_eq!(
                repo.store(G, U, c, wrong, 30),
                Err(RepoError::Stale),
                "expected {wrong}"
            );
            assert_eq!(dump(&path), before, "write leaked for expected {wrong}");
        }
        assert_eq!(repo.load(G, U), Ok(Some((b.clone(), 2))));
        assert_eq!(repo.store(G, U, c, 2, 30), Ok(3));
    }

    #[test]
    fn unrepresentable_versions_are_refused_without_a_write() {
        let path = scratch("badver");
        let mut repo = open_at(&path);
        repo.store(G, U, &activating(), 0, NOW).unwrap();
        let before = dump(&path);
        for v in [u64::MAX, i64::MAX as u64, i64::MAX as u64 + 1] {
            assert_eq!(
                repo.store(G, U, &State::Inactive, v, NOW),
                Err(RepoError::InvalidVersion),
                "{v}"
            );
        }
        assert_eq!(dump(&path), before);
    }

    #[test]
    fn members_are_isolated_including_ids_that_concatenate_alike() {
        let mut repo = mem();
        let s = activating();
        assert_eq!(repo.store(1, 23, &s, 0, NOW), Ok(1));
        assert_eq!(repo.store(12, 3, &State::Inactive, 0, NOW), Ok(1));
        assert_eq!(repo.store(1, 23, &s, 1, NOW), Ok(2));
        assert_eq!(repo.load(12, 3), Ok(Some((State::Inactive, 1))));
        assert_eq!(repo.load(1, 23), Ok(Some((s, 2))));
        assert_eq!(repo.load(23, 1), Ok(None));
    }

    #[test]
    fn concurrent_create_exactly_one_writer_wins() {
        let path = scratch("race-create");
        drop(open_at(&path));
        for round in 0..20u64 {
            let barrier = Arc::new(Barrier::new(2));
            let handles: Vec<_> = (0..2u64)
                .map(|who| {
                    let (p, b) = (path.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        let mut repo = open_at(&p);
                        // distinguishable states: each writer saw a different held-role set
                        let mut c = ctx();
                        c.held_roles = vec![101 + who];
                        let state = step(&State::Inactive, Event::ReactionAdded, &c).0;
                        b.wait();
                        (state.clone(), repo.store(G, 1000 + round, &state, 0, NOW))
                    })
                })
                .collect();
            let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let wins: Vec<_> = results.iter().filter(|(_, r)| r.is_ok()).collect();
            let losses: Vec<_> = results.iter().filter(|(_, r)| r.is_err()).collect();
            assert_eq!(wins.len(), 1, "round {round}: {results:?}");
            assert_eq!(wins[0].1, Ok(1));
            assert_eq!(losses[0].1, Err(RepoError::Stale), "round {round}");
            let stored = open_at(&path).load(G, 1000 + round).unwrap().unwrap();
            assert_eq!(
                stored,
                (wins[0].0.clone(), 1),
                "round {round}: loser overwrote"
            );
        }
    }

    #[test]
    fn concurrent_update_from_same_version_exactly_one_writer_wins() {
        let path = scratch("race-update");
        let mut seed = open_at(&path);
        let base = activating();
        for round in 0..20u64 {
            seed.store(G, 2000 + round, &base, 0, NOW).unwrap();
        }
        for round in 0..20u64 {
            let b = Arc::new(Barrier::new(2));
            let handles: Vec<_> = (0..2u64)
                .map(|who| {
                    let (p, b) = (path.clone(), b.clone());
                    std::thread::spawn(move || {
                        let mut repo = open_at(&p);
                        let next = if who == 0 {
                            State::Inactive
                        } else {
                            walk(&lifecycle_events())[6].clone()
                        };
                        b.wait();
                        (next.clone(), repo.store(G, 2000 + round, &next, 1, NOW + 1))
                    })
                })
                .collect();
            let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let wins: Vec<_> = results.iter().filter(|(_, r)| r.is_ok()).collect();
            assert_eq!(wins.len(), 1, "round {round}: {results:?}");
            assert_eq!(wins[0].1, Ok(2));
            assert!(results.iter().any(|(_, r)| *r == Err(RepoError::Stale)));
            let stored = open_at(&path).load(G, 2000 + round).unwrap().unwrap();
            assert_eq!(stored, (wins[0].0.clone(), 2), "round {round}");
        }
    }

    #[test]
    fn real_step_sequences_round_trip_every_intermediate_state() {
        let states = real_states();
        // The sequences must actually reach the fields this card is about.
        let any = |f: &dyn Fn(&Snapshot) -> bool| states.iter().filter_map(snap).any(f);
        assert!(
            any(&|s| !s.failed_steps.is_empty()),
            "no failed_steps state"
        );
        assert!(any(&|s| s.restore_in_flight), "no restore_in_flight state");
        assert!(
            any(&|s| !s.last_emitted_restore_plan.is_empty()),
            "no restore plan state"
        );
        assert!(any(&|s| s.last_failure.is_some()), "no last_failure state");
        assert!(any(
            &|s| !s.struck_roles.is_empty() && !s.deleted_roles.is_empty()
        ));
        assert!(any(&|s| !s.deferred_roles.is_empty()), "no deferred state");
        assert!(
            any(&|s| s.pending_reactivate),
            "no pending_reactivate state"
        );
        assert!(
            any(&|s| !s.restore_plan.is_empty()),
            "no restore_plan state"
        );
        assert!(states.iter().any(|s| matches!(s, State::Activating(_))));
        assert!(states.iter().any(|s| matches!(s, State::Active(_))));
        assert!(states.iter().any(|s| matches!(s, State::Releasing(_))));

        let path = scratch("roundtrip");
        let mut repo = open_at(&path);
        let mut version = 0;
        for (i, s) in states.iter().enumerate() {
            version = repo.store(G, U, s, version, NOW + i as i64).unwrap();
            assert_eq!(repo.load(G, U), Ok(Some((s.clone(), version))), "step {i}");
        }
        assert_eq!(version, states.len() as u64);
    }

    #[test]
    fn seeded_random_event_walks_round_trip_every_state() {
        let mut rng = Lcg(0x5EED);
        let mut repo = mem();
        let c = ctx();
        let ids = [
            ActionId::SaveSnapshot,
            ActionId::RemoveRoles,
            ActionId::AddSafewordRole,
            ActionId::SendActivatedDm,
            ActionId::RemoveSafewordRole,
            ActionId::RestoreRoles,
            ActionId::ClearSnapshot,
            ActionId::SendReleasedDm,
        ];
        let codes = [
            FailureCode::StorageError,
            FailureCode::MissingPermissions,
            FailureCode::HierarchyRejected,
            FailureCode::RateLimited,
            FailureCode::DmClosed,
            FailureCode::TransientHttp,
            FailureCode::UnknownRole,
        ];
        let roles = [0u64, 101, 102, 103, 104, 105, 201, 900, u64::MAX];
        let mut seen_failed = false;
        let mut seen_flight = false;
        for walk_no in 0..40u64 {
            let mut state = State::Inactive;
            let mut version = repo.load(G, walk_no).unwrap().map_or(0, |(_, v)| v);
            for _ in 0..60 {
                let pick = |rng: &mut Lcg, n: usize| (rng.next() % n as u64) as usize;
                let ev = match rng.next() % 7 {
                    0 => Event::ReactionAdded,
                    1 => Event::ReactionRemoved,
                    2 => Event::Strike(roles[pick(&mut rng, roles.len())]),
                    3 => Event::RoleDeleted(roles[pick(&mut rng, roles.len())]),
                    4 => Event::GrantDuringActive(roles[pick(&mut rng, roles.len())]),
                    5 => Event::ActionDone(ids[pick(&mut rng, ids.len())]),
                    _ => Event::ActionFailed(
                        ids[pick(&mut rng, ids.len())],
                        codes[pick(&mut rng, codes.len())],
                    ),
                };
                state = step(&state, ev, &c).0;
                version = repo.store(G, walk_no, &state, version, NOW).unwrap();
                assert_eq!(repo.load(G, walk_no), Ok(Some((state.clone(), version))));
                if let Some(s) = state.snapshot() {
                    seen_failed |= !s.failed_steps.is_empty();
                    seen_flight |= s.restore_in_flight;
                }
            }
        }
        assert!(
            seen_failed && seen_flight,
            "walks never reached the interesting fields"
        );
    }

    #[test]
    fn extreme_ids_and_role_ids_survive_without_u64_loss() {
        let path = scratch("extreme");
        let mut repo = open_at(&path);
        let mut c = ctx();
        c.restricted_roles = vec![u64::MAX, u64::MAX - 1, (1 << 53) + 1];
        c.held_roles = vec![u64::MAX, (1 << 53) + 1];
        let (state, _) = step(&State::Inactive, Event::ReactionAdded, &c);
        assert_eq!(
            repo.store(u64::MAX, u64::MAX - 1, &state, 0, i64::MAX),
            Ok(1)
        );
        drop(repo);
        let repo = open_at(&path);
        assert_eq!(
            repo.load(u64::MAX, u64::MAX - 1),
            Ok(Some((state.clone(), 1)))
        );
        assert!(state.snapshot().unwrap().saved_roles.contains(&u64::MAX));
        assert_eq!(repo.load(u64::MAX, u64::MAX), Ok(None));
    }

    #[test]
    fn corrupt_snapshot_json_is_an_error_and_leaves_the_row_untouched() {
        let path = scratch("corrupt-json");
        let mut repo = open_at(&path);
        assert_eq!(repo.store(G, U, &activating(), 0, NOW), Ok(1));
        let valid = match column(&path, "snapshot_json") {
            Value::Text(t) => t,
            other => panic!("{other:?}"),
        };
        let dup = format!("{},\"saved_roles\":[]}}", &valid[..valid.len() - 1]);
        let unknown = format!("{},\"surprise\":1}}", &valid[..valid.len() - 1]);
        let wrong_type = valid.replacen("\"saved_roles\":[", "\"saved_roles\":[\"x\",", 1);
        let negative = valid.replacen("\"saved_roles\":[", "\"saved_roles\":[-1,", 1);
        let float = valid.replacen("\"saved_roles\":[", "\"saved_roles\":[1.5,", 1);
        let huge = valid.replacen(
            "\"saved_roles\":[",
            "\"saved_roles\":[18446744073709551616,",
            1,
        );
        let bad_bool = valid.replacen("\"snapshot_saved\":false", "\"snapshot_saved\":\"no\"", 1);
        let missing_core = valid.replacen("\"saved_roles\":", "\"renamed_roles\":", 1);
        assert_ne!(wrong_type, valid);
        assert_ne!(bad_bool, valid, "fixture lacks snapshot_saved=false");
        let cases: Vec<(&str, String)> = vec![
            ("not json", "not json".into()),
            ("empty", String::new()),
            ("truncated", valid[..valid.len() / 2].to_string()),
            ("null", "null".into()),
            ("array", "[]".into()),
            ("empty object", "{}".into()),
            ("duplicate field", dup),
            ("unknown field", unknown),
            ("wrong element type", wrong_type),
            ("negative id", negative),
            ("float id", float),
            ("id above u64", huge),
            ("wrong bool type", bad_bool),
            ("missing core field", missing_core),
        ];
        for (why, json) in cases {
            poke(&path, "snapshot_json", text(&json));
            assert_row_survives(&path, &mut repo, RepoError::CorruptJson, why);
        }
        for (why, json) in [
            ("not json", "x"),
            ("object", "{}"),
            ("null", "null"),
            ("negative", "[-1]"),
        ] {
            poke(&path, "snapshot_json", text(&valid));
            poke(&path, "struck_json", text(json));
            assert_row_survives(
                &path,
                &mut repo,
                RepoError::CorruptJson,
                &format!("struck {why}"),
            );
            poke(&path, "struck_json", text("[]"));
            poke(&path, "deferred_json", text(json));
            assert_row_survives(
                &path,
                &mut repo,
                RepoError::CorruptJson,
                &format!("deferred {why}"),
            );
            poke(&path, "deferred_json", text("[]"));
        }
        // Restoring the valid payload makes the member readable again, unchanged.
        poke(&path, "snapshot_json", text(&valid));
        assert_eq!(repo.load(G, U), Ok(Some((activating(), 1))));
    }

    fn stored_snapshot(path: &Path) -> serde_json::Value {
        let Value::Text(json) = column(path, "snapshot_json") else {
            panic!("snapshot_json is not text");
        };
        serde_json::from_str(&json).unwrap()
    }

    /// A real state (reached by `step`) whose `last_failure` is set.
    fn state_with_failure() -> State {
        real_states()
            .into_iter()
            .find(|s| snap(s).is_some_and(|x| x.last_failure.is_some()))
            .expect("a real step sequence leaves last_failure set")
    }

    /// Keys that an older build did not write; every other snapshot key is required.
    const LEGACY_KEYS: [&str; 3] = [
        "failed_steps",
        "restore_in_flight",
        "last_emitted_restore_plan",
    ];

    #[test]
    fn deleting_any_non_legacy_snapshot_key_is_an_error_and_leaves_the_row_untouched() {
        let path = scratch("missing-key");
        let mut repo = open_at(&path);
        let state = state_with_failure();
        assert_eq!(repo.store(G, U, &state, 0, NOW), Ok(1));
        let valid = stored_snapshot(&path);
        let keys: Vec<String> = valid.as_object().unwrap().keys().cloned().collect();
        // pins the key list: a new SW1 field must be classified required or legacy here
        assert_eq!(keys.len(), 18, "snapshot_json keys: {keys:?}");
        let mut deleted = 0;
        for key in keys.iter().filter(|k| !LEGACY_KEYS.contains(&k.as_str())) {
            let mut v = valid.clone();
            assert!(v.as_object_mut().unwrap().remove(key).is_some());
            poke(&path, "snapshot_json", text(&v.to_string()));
            assert_row_survives(
                &path,
                &mut repo,
                RepoError::CorruptJson,
                &format!("deleted key {key}"),
            );
            deleted += 1;
        }
        assert_eq!(deleted, 15);
        for inner in ["step", "code"] {
            let mut v = valid.clone();
            let failure = v["last_failure"].as_object_mut().expect("last_failure set");
            assert!(failure.remove(inner).is_some());
            poke(&path, "snapshot_json", text(&v.to_string()));
            assert_row_survives(
                &path,
                &mut repo,
                RepoError::CorruptJson,
                &format!("deleted last_failure.{inner}"),
            );
        }
        // an unknown key inside last_failure is refused like an unknown top-level key
        let mut v = valid.clone();
        v["last_failure"]
            .as_object_mut()
            .unwrap()
            .insert("surprise".into(), serde_json::Value::from(1));
        poke(&path, "snapshot_json", text(&v.to_string()));
        assert_row_survives(
            &path,
            &mut repo,
            RepoError::CorruptJson,
            "unknown key in last_failure",
        );
        poke(&path, "snapshot_json", text(&valid.to_string()));
        assert_eq!(repo.load(G, U), Ok(Some((state, 1))));
    }

    #[test]
    fn struck_and_deferred_roles_are_stored_in_their_own_named_columns() {
        let path = scratch("columns");
        let mut repo = open_at(&path);
        let mut state = state_with_failure();
        let (State::Activating(s) | State::Active(s) | State::Releasing(s)) = &mut state else {
            panic!("fixture has a snapshot");
        };
        s.struck_roles = vec![301, 302];
        s.deferred_roles = vec![401];
        let expect = state.clone();
        assert_eq!(repo.store(G, U, &state, 0, NOW), Ok(1));
        assert_eq!(column(&path, "struck_json"), text("[301,302]"));
        assert_eq!(column(&path, "deferred_json"), text("[401]"));
        assert_eq!(repo.load(G, U), Ok(Some((expect, 1))));
    }

    #[test]
    fn unknown_state_string_is_refused_and_never_becomes_inactive() {
        let path = scratch("unknown-state");
        let mut repo = open_at(&path);
        repo.store(G, U, &activating(), 0, NOW).unwrap();
        for bad in [
            "",
            "Active",
            "ACTIVE",
            " active",
            "active ",
            "paused",
            "released",
            "activating\0",
            "inactive;",
            "\u{0430}ctive",
        ] {
            poke(&path, "state", text(bad));
            assert_row_survives(&path, &mut repo, RepoError::UnknownState, bad);
        }
        poke(&path, "state", Value::Integer(2));
        let before = dump(&path);
        // an integer coerces to text in a TEXT column: still not a known state
        assert_eq!(repo.load(G, U), Err(RepoError::UnknownState));
        assert_eq!(dump(&path), before);
    }

    #[test]
    fn impossible_column_values_are_corrupt_rows_not_silent_defaults() {
        let path = scratch("corrupt-row");
        let mut repo = open_at(&path);
        repo.store(G, U, &activating(), 0, NOW).unwrap();
        // version that is not an integer
        poke(&path, "version", text("abc"));
        assert_row_survives(&path, &mut repo, RepoError::CorruptRow, "text version");
        poke(&path, "version", Value::Real(1.5));
        assert_row_survives(&path, &mut repo, RepoError::CorruptRow, "real version");
        poke(&path, "version", Value::Integer(1));
        // an `inactive` row must carry no protection data
        poke(&path, "state", text("inactive"));
        assert_row_survives(
            &path,
            &mut repo,
            RepoError::CorruptRow,
            "inactive with snapshot",
        );
    }

    #[test]
    fn snapshot_from_an_older_build_loads_with_defaults() {
        let path = scratch("legacy");
        let mut repo = open_at(&path);
        let released = walk(&lifecycle_events());
        let rich = released
            .iter()
            .find(|s| {
                matches!(s, State::Releasing(x) if x.restore_in_flight
                    && !x.last_emitted_restore_plan.is_empty()
                    && !x.failed_steps.is_empty())
            })
            .or_else(|| {
                released.iter().find(|s| {
                    matches!(s, State::Releasing(x) if x.restore_in_flight || !x.failed_steps.is_empty())
                })
            })
            .expect("a Releasing state with the new fields set")
            .clone();
        repo.store(G, U, &rich, 0, NOW).unwrap();
        // Rewrite the stored JSON the way an older build wrote it: no new keys at all.
        let Value::Text(json) = column(&path, "snapshot_json") else {
            panic!("snapshot_json is not text");
        };
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object_mut().unwrap();
        for key in [
            "failed_steps",
            "restore_in_flight",
            "last_emitted_restore_plan",
        ] {
            assert!(obj.remove(key).is_some(), "fixture lacks {key}");
        }
        poke(&path, "snapshot_json", text(&v.to_string()));

        let mut expected = rich.clone();
        if let State::Releasing(s) = &mut expected {
            s.failed_steps = Vec::new();
            s.restore_in_flight = false;
            s.last_emitted_restore_plan = Vec::new();
        }
        assert_ne!(expected, rich, "fixture must differ from defaults");
        assert_eq!(repo.load(G, U), Ok(Some((expected.clone(), 1))));
        // and it can then be rewritten in the current format
        assert_eq!(repo.store(G, U, &expected, 1, NOW + 1), Ok(2));
        assert_eq!(repo.load(G, U), Ok(Some((expected, 2))));
    }

    #[test]
    fn restart_recovery_returns_what_it_returned_before_the_restart() {
        let path = scratch("restart");
        let states = real_states();
        let mut version = 0;
        for (i, s) in states.iter().enumerate() {
            let mut repo = open_at(&path);
            version = repo.store(G, U, s, version, NOW + i as i64).unwrap();
            drop(repo); // simulated restart: connection gone, file remains
            let reopened = open_at(&path);
            let (loaded, v) = reopened.load(G, U).unwrap().unwrap();
            assert_eq!(v, version);
            assert_eq!(&loaded, s, "state {i}");
            for reaction in [true, false] {
                assert_eq!(
                    restart_action(&loaded, reaction),
                    restart_action(s, reaction),
                    "state {i}, reaction {reaction}"
                );
            }
        }
    }

    #[test]
    fn restart_recovery_uses_persisted_fields_not_defaults() {
        let path = scratch("restart-concrete");
        let mut repo = open_at(&path);
        // Active whose SaveSnapshot failed: recovery must re-run SaveSnapshot.
        let mut s = walk(&[
            Event::ReactionAdded,
            Event::ActionDone(ActionId::SaveSnapshot),
            Event::ActionDone(ActionId::RemoveRoles),
            Event::ActionDone(ActionId::AddSafewordRole),
            Event::ActionDone(ActionId::SendActivatedDm),
        ])
        .pop()
        .unwrap();
        let State::Active(inner) = &mut s else {
            panic!("not Active: {s:?}")
        };
        inner.last_failure = Some((ActionId::SaveSnapshot, FailureCode::StorageError));
        assert_eq!(
            restart_action(&s, true),
            RestartAction::Execute(Action::SaveSnapshot)
        );
        repo.store(G, U, &s, 0, NOW).unwrap();
        drop(repo);
        let (loaded, _) = open_at(&path).load(G, U).unwrap().unwrap();
        assert_eq!(
            restart_action(&loaded, true),
            RestartAction::Execute(Action::SaveSnapshot)
        );
        assert_eq!(
            restart_action(&loaded, false),
            RestartAction::Trigger(Event::ReactionRemoved)
        );
    }

    #[test]
    fn clear_is_compare_and_swap_and_keeps_versions_moving_forward() {
        let path = scratch("clear");
        let mut repo = open_at(&path);
        // clearing a member with no row writes nothing
        assert_eq!(repo.clear(G, U, 0, NOW), Ok(0));
        assert!(dump(&path).is_empty());
        assert_eq!(repo.clear(G, U, 3, NOW), Err(RepoError::Stale));
        repo.store(G, U, &activating(), 0, NOW).unwrap();
        assert_eq!(repo.clear(G, U, 0, NOW), Err(RepoError::Stale));
        assert_eq!(repo.clear(G, U, 5, NOW), Err(RepoError::Stale));
        assert_eq!(repo.load(G, U), Ok(Some((activating(), 1))));
        assert_eq!(repo.clear(G, U, 1, NOW + 1), Ok(2));
        assert_eq!(repo.load(G, U), Ok(Some((State::Inactive, 2))));
        // no protection data is kept after a clear
        assert_eq!(column(&path, "snapshot_json"), text("{}"));
        // A writer that read "no row" before the clear must not be able to create over it.
        assert_eq!(
            repo.store(G, U, &activating(), 0, NOW + 2),
            Err(RepoError::Stale)
        );
        assert_eq!(repo.store(G, U, &activating(), 2, NOW + 2), Ok(3));
    }

    #[test]
    fn updated_at_records_the_caller_supplied_time() {
        let path = scratch("updated-at");
        let mut repo = open_at(&path);
        repo.store(G, U, &activating(), 0, 1234).unwrap();
        let rows = dump(&path);
        assert_eq!(rows[0][7], Value::Integer(1234));
        assert_eq!(rows[0][0], text("111"));
        assert_eq!(rows[0][1], text("222"));
        assert_eq!(rows[0][2], text("activating"));
    }

    #[test]
    fn errors_are_stable_codes_that_carry_no_json_sql_or_ids() {
        let path = scratch("leak");
        let mut repo = open_at(&path);
        repo.store(G, U, &activating(), 0, NOW).unwrap();
        poke(
            &path,
            "snapshot_json",
            text("{\"private_marker_zz\": \"SELECT secret_zz\"}"),
        );
        let e1 = repo.load(G, U).unwrap_err();
        poke(&path, "state", text("marker_state_zz"));
        let e2 = repo.load(G, U).unwrap_err();
        let e3 = repo.store(G, U, &State::Inactive, 9, NOW).unwrap_err();
        let e4 = Repository::open(":memory:", &[])
            .and_then(|mut r| {
                r.store(G, U, &State::Inactive, 0, NOW)
                    .and_then(|_| r.store(G, U, &State::Inactive, 0, NOW))
            })
            .unwrap_err();
        for e in [e1, e2, e3, e4] {
            let t = format!("{e:?} | {e} | {}", e.code());
            for needle in [
                "marker",
                "secret",
                "SELECT",
                "111",
                "222",
                "snapshot_json",
                "{",
            ] {
                assert!(!t.contains(needle), "leaked {needle:?} in {t}");
            }
        }
        let all = [
            RepoError::Stale,
            RepoError::CorruptJson,
            RepoError::UnknownState,
            RepoError::CorruptRow,
            RepoError::InvalidVersion,
            RepoError::Storage(StorageError::Sqlite),
        ];
        let mut codes: Vec<_> = all.iter().map(RepoError::code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        assert_eq!(RepoError::Stale.code(), "stale_version");
        assert_eq!(e1, RepoError::CorruptJson);
        assert_eq!(e2, RepoError::UnknownState);
    }

    #[test]
    fn newer_stored_schema_is_refused_and_reserved_paths_are_refused() {
        let path = scratch("schema");
        drop(open_at(&path));
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE schema_versions SET version = 99 WHERE feature = 'safeword'",
                [],
            )
            .unwrap();
        assert_eq!(
            Repository::open(path.to_str().unwrap(), &[]).unwrap_err(),
            RepoError::Storage(StorageError::StoredVersionNewer)
        );
        let other = scratch("schema-reserved");
        assert_eq!(
            Repository::open(other.to_str().unwrap(), std::slice::from_ref(&other)).unwrap_err(),
            RepoError::Storage(StorageError::ReservedPath)
        );
        assert!(!other.exists());
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    /// Model runtime: executes emitted actions (acknowledging or failing them), toggles the
    /// reaction, crashes and reopens the file at random, persisting every transition.
    struct Model {
        path: PathBuf,
        repo: Repository,
        state: State,
        version: Version,
        queue: VecDeque<Action>,
        reaction: bool,
        rng: Lcg,
    }

    impl Model {
        fn apply(&mut self, event: Event) {
            let (next, actions) = step(&self.state, event, &ctx());
            self.version = self
                .repo
                .store(G, U, &next, self.version, NOW)
                .expect("persist transition");
            self.state = next;
            self.queue.extend(actions);
        }

        fn restart(&mut self, reopen: bool) {
            if reopen {
                self.repo = open_at(&self.path);
                let (s, v) = self
                    .repo
                    .load(G, U)
                    .unwrap()
                    .unwrap_or((State::Inactive, 0));
                assert_eq!(s, self.state, "state lost across restart");
                self.version = v;
            }
            self.queue.clear(); // in-flight work does not survive a crash
            match restart_action(&self.state, self.reaction) {
                RestartAction::NoOp => {
                    let quiet_ok = matches!(self.state, State::Inactive)
                        || (matches!(self.state, State::Active(_)) && self.reaction);
                    assert!(
                        quiet_ok,
                        "went quiet with unacknowledged work: {:?}",
                        self.state
                    );
                }
                RestartAction::Trigger(e) => self.apply(e),
                RestartAction::Execute(a) => self.queue.push_back(a),
            }
        }

        fn run(&mut self) -> bool {
            self.apply(Event::ReactionAdded);
            for i in 0..3000 {
                if i == 400 && self.reaction {
                    self.reaction = false;
                    self.apply(Event::ReactionRemoved);
                }
                let done = matches!(self.state, State::Inactive) && !self.reaction;
                if done && self.queue.is_empty() {
                    return true;
                }
                let roll = self.rng.next();
                if i < 400 && roll.is_multiple_of(13) {
                    self.reaction = !self.reaction;
                    let e = if self.reaction {
                        Event::ReactionAdded
                    } else {
                        Event::ReactionRemoved
                    };
                    self.apply(e);
                } else if roll.is_multiple_of(9) {
                    self.restart(true);
                } else if let Some(a) = self.queue.pop_front() {
                    let e = if self.rng.next().is_multiple_of(4) {
                        Event::ActionFailed(a.id(), FailureCode::TransientHttp)
                    } else {
                        Event::ActionDone(a.id())
                    };
                    self.apply(e);
                } else {
                    self.restart(false);
                }
            }
            false
        }
    }

    #[test]
    fn crash_and_retry_runtime_always_reaches_inactive_through_the_repository() {
        for seed in 0..40u64 {
            let path = scratch(&format!("liveness-{seed}"));
            let mut m = Model {
                repo: open_at(&path),
                path,
                state: State::Inactive,
                version: 0,
                queue: VecDeque::new(),
                reaction: true,
                rng: Lcg(seed * 7919 + 1),
            };
            assert!(m.run(), "seed {seed} never reached Inactive: {:?}", m.state);
            let on_disk = open_at(&m.path).load(G, U).unwrap().unwrap();
            assert_eq!(on_disk, (State::Inactive, m.version), "seed {seed}");
        }
    }
}

//! Managed embed posts: the plan and the repository.
//!
//! A managed post is a message the bot owns: a rules post, an information post. It is
//! posted ONCE and then EDITED IN PLACE when the owner changes the template, never
//! duplicated. This module decides what to do and stores which message belongs to
//! which key.
//!
//! The planner is pure: it reads only the plain facts handed to it, it never reads the
//! clock, never touches the network and never carries text. The repository takes `now`
//! as a parameter for the same reason.
//!
//! Privacy notes:
//! * Every error and every refusal reason is a stable code. No refusal names a
//!   template, a channel name or a user.
//! * The repository stores ids and hashes only. It never stores template text.
//!
//! Wiring card W1 removes the module-level `allow(dead_code)` in `super::mod`.

use std::fmt;

use rusqlite::Connection;

use crate::storage::{Migration, StorageError};

/// Name of the feature in the shared `schema_versions` table.
pub const FEATURE: &str = "embeds";

/// The migrations of the `embeds` feature, in order.
pub const MIGRATIONS: [Migration; 1] = [Migration {
    version: 1,
    sql: "CREATE TABLE IF NOT EXISTS managed_posts (
            key TEXT PRIMARY KEY,
            guild TEXT NOT NULL,
            channel TEXT NOT NULL,
            message TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            updated_by TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );",
}];

/// Stable reason codes for a plan that was refused.
///
/// The codes never carry template text, channel names or user ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// The key is not a usable key: it is empty, too long, or carries a character that
    /// is not allowed in a key.
    InvalidKey,
    /// The template hash is not a SHA-256 hex digest, so it cannot be compared with
    /// the recorded one.
    InvalidHash,
    /// The key is already recorded for a different channel. Reusing it would publish
    /// the same post twice, in two places.
    KeyReusedInAnotherChannel,
    /// The key is already recorded for a different guild.
    KeyReusedInAnotherGuild,
    /// The recorded message no longer exists and the caller did not allow the post to
    /// be recreated. A deleted rules post may be deliberate.
    MissingMessage,
}

impl RefuseReason {
    /// Returns the stable code of this reason.
    pub fn code(self) -> &'static str {
        match self {
            RefuseReason::InvalidKey => "invalid_key",
            RefuseReason::InvalidHash => "invalid_hash",
            RefuseReason::KeyReusedInAnotherChannel => "key_reused_in_another_channel",
            RefuseReason::KeyReusedInAnotherGuild => "key_reused_in_another_guild",
            RefuseReason::MissingMessage => "missing_message",
        }
    }
}

impl fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// The record of one managed post.
///
/// It holds ids and a content hash only. The template text is never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostRecord {
    /// The key that identifies the managed post.
    pub key: String,
    /// The guild the post lives in.
    pub guild: u64,
    /// The channel the post lives in.
    pub channel: u64,
    /// The id of the message that carries the post.
    pub message: u64,
    /// The content hash of the template that was published.
    pub content_hash: String,
    /// The id of the user that caused the last write. Zero means "not a user", for
    /// example a startup sync.
    pub updated_by: u64,
}

/// The record of one managed post together with the time of its last write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPost {
    /// The stored record.
    pub record: PostRecord,
    /// When the record was last written, in epoch milliseconds. Supplied by the
    /// caller; the module never reads the clock.
    pub updated_at: i64,
}

/// What the runtime should do with one managed post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// No post exists for this key: post the template and record the new message.
    Create,
    /// The post exists and the template changed: edit the recorded message in place.
    Edit {
        /// The id of the message to edit. It is the SAME message that was recorded
        /// before, never a new one.
        message: u64,
    },
    /// The post exists and the template is unchanged: do nothing at all.
    NoOp,
    /// The recorded message is gone and the caller explicitly allowed the post to be
    /// posted again.
    Recreate,
    /// The post must not be touched. The reason says why.
    Refuse(RefuseReason),
}

/// The plain facts the planner needs.
///
/// This is a request struct so that no function grows past the argument limit, and so
/// that every input the planner reads is visible in one place.
#[derive(Debug, Clone, Copy)]
pub struct PlanRequest<'a> {
    /// The key that identifies the managed post.
    pub key: &'a str,
    /// The content hash of the template that should be published.
    pub template_hash: &'a str,
    /// The record of the post, if one exists.
    pub recorded: Option<&'a PostRecord>,
    /// The guild the post should live in.
    pub target_guild: u64,
    /// The channel the post should live in.
    pub target_channel: u64,
    /// True when the caller looked the recorded message up and it does not exist any
    /// more. Only meaningful when `recorded` is `Some`.
    pub message_missing: bool,
    /// True when the caller explicitly allows a missing post to be posted again.
    pub allow_recreate: bool,
}

/// The decision for one managed post.
///
/// The rules, in the order they are applied:
/// 1. A key that is not usable is refused. A key that names a file or a path could
///    make the wiring card read the wrong template.
/// 2. A template hash that is not a SHA-256 hex digest is refused: it cannot be
///    compared with the recorded one.
/// 3. A key with no record is created: there is nothing to edit.
/// 4. A key recorded for another guild or another channel is refused: the same post
///    must not appear in two places.
/// 5. A recorded message that is gone is refused, unless the caller explicitly
///    allowed the post to be created again. A deleted rules post may be deliberate.
/// 6. A recorded post whose hash equals the template hash is a no-op: reposting
///    would spam the channel.
/// 7. Otherwise the recorded message is edited in place. The plan never names a
///    message that was not recorded.
pub fn plan(request: &PlanRequest<'_>) -> Plan {
    if validate_key(request.key).is_err() {
        return Plan::Refuse(RefuseReason::InvalidKey);
    }
    if !is_content_hash(request.template_hash) {
        return Plan::Refuse(RefuseReason::InvalidHash);
    }
    let Some(recorded) = request.recorded else {
        return Plan::Create;
    };
    if recorded.guild != request.target_guild {
        return Plan::Refuse(RefuseReason::KeyReusedInAnotherGuild);
    }
    if recorded.channel != request.target_channel {
        return Plan::Refuse(RefuseReason::KeyReusedInAnotherChannel);
    }
    if request.message_missing {
        return if request.allow_recreate {
            Plan::Recreate
        } else {
            Plan::Refuse(RefuseReason::MissingMessage)
        };
    }
    if recorded.content_hash == request.template_hash {
        return Plan::NoOp;
    }
    Plan::Edit {
        message: recorded.message,
    }
}

/// Maximum length of a key, in bytes.
pub const MAX_KEY_BYTES: usize = 64;

/// Checks that a key is usable.
///
/// A key names one managed post and, through the wiring card, one template file. It is
/// therefore restricted to a plain, short name: ASCII letters, digits, `-`, `_` and
/// `.`, with no path separator, no `..` and no leading dot. Every other spelling is
/// refused, so a key can never walk out of the template directory.
pub fn validate_key(key: &str) -> Result<(), RefuseReason> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(RefuseReason::InvalidKey);
    }
    if key == "." || key == ".." || key.starts_with('.') || key.contains("..") {
        return Err(RefuseReason::InvalidKey);
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(RefuseReason::InvalidKey);
    }
    Ok(())
}

/// True when `hash` is a SHA-256 hex digest: 64 lowercase hex characters.
pub fn is_content_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Applies the migrations of the `embeds` feature to a connection.
///
/// The migration creates the `managed_posts` table. It is idempotent: running it
/// again changes nothing.
pub fn migrate(conn: &mut Connection) -> Result<u32, StorageError> {
    crate::storage::migrate(conn, FEATURE, &MIGRATIONS)
}

/// Reads the record of one managed post.
///
/// Returns `Ok(None)` when no row exists for the key.
///
/// A stored column that does not hold what it must hold is reported as an error, never
/// silently defaulted: a corrupt row must not turn into a plan that reposts the post.
pub fn get(conn: &Connection, key: &str) -> Result<Option<StoredPost>, RepoError> {
    validate_key(key).map_err(|_| RepoError::InvalidKey)?;
    let mut stmt = conn.prepare(
        "SELECT guild, channel, message, content_hash, updated_by, updated_at
         FROM managed_posts WHERE key = ?1",
    )?;
    let mut rows = stmt.query([key])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let guild = parse_id(row.get::<_, String>(0)?)?;
    let channel = parse_id(row.get::<_, String>(1)?)?;
    let message = parse_id(row.get::<_, String>(2)?)?;
    let content_hash: String = row.get(3)?;
    if !is_content_hash(&content_hash) {
        return Err(RepoError::CorruptRow);
    }
    let updated_by = parse_optional_id(row.get::<_, String>(4)?)?;
    let updated_at = row.get::<_, i64>(5)?;
    Ok(Some(StoredPost {
        record: PostRecord {
            key: key.to_string(),
            guild,
            channel,
            message,
            content_hash,
            updated_by,
        },
        updated_at,
    }))
}

/// Writes the record of one managed post, replacing any row with the same key.
///
/// `now` is the timestamp of the write in epoch milliseconds; the caller supplies it,
/// so this module never reads the clock.
pub fn upsert(conn: &Connection, record: &PostRecord, now: i64) -> Result<(), RepoError> {
    validate_key(&record.key).map_err(|_| RepoError::InvalidKey)?;
    if !is_content_hash(&record.content_hash) {
        return Err(RepoError::CorruptRow);
    }
    if record.guild == 0 || record.channel == 0 || record.message == 0 {
        return Err(RepoError::InvalidId);
    }
    conn.execute(
        "INSERT INTO managed_posts
            (key, guild, channel, message, content_hash, updated_by, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(key) DO UPDATE SET
            guild = excluded.guild,
            channel = excluded.channel,
            message = excluded.message,
            content_hash = excluded.content_hash,
            updated_by = excluded.updated_by,
            updated_at = excluded.updated_at",
        rusqlite::params![
            record.key,
            record.guild.to_string(),
            record.channel.to_string(),
            record.message.to_string(),
            record.content_hash,
            record.updated_by.to_string(),
            now,
        ],
    )?;
    Ok(())
}

/// Writes the record only when the plan allows a write.
///
/// Returns `Ok(true)` when a row was written and `Ok(false)` when the plan forbids a
/// write, which is the case for `Plan::NoOp` and `Plan::Refuse`.
pub fn apply(
    conn: &Connection,
    plan: &Plan,
    record: &PostRecord,
    now: i64,
) -> Result<bool, RepoError> {
    match plan {
        Plan::NoOp | Plan::Refuse(_) => Ok(false),
        Plan::Create | Plan::Edit { .. } | Plan::Recreate => {
            upsert(conn, record, now)?;
            Ok(true)
        }
    }
}

/// Lists every managed post, ordered by key.
pub fn list(conn: &Connection) -> Result<Vec<StoredPost>, RepoError> {
    let mut stmt = conn.prepare(
        "SELECT key, guild, channel, message, content_hash, updated_by, updated_at
         FROM managed_posts ORDER BY key ASC",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let guild = parse_id(row.get::<_, String>(1)?)?;
        let channel = parse_id(row.get::<_, String>(2)?)?;
        let message = parse_id(row.get::<_, String>(3)?)?;
        let content_hash: String = row.get(4)?;
        if !is_content_hash(&content_hash) {
            return Err(RepoError::CorruptRow);
        }
        let updated_by = parse_optional_id(row.get::<_, String>(5)?)?;
        let updated_at = row.get::<_, i64>(6)?;
        out.push(StoredPost {
            record: PostRecord {
                key,
                guild,
                channel,
                message,
                content_hash,
                updated_by,
            },
            updated_at,
        });
    }
    Ok(out)
}

/// Deletes the record of one managed post.
///
/// Returns `Ok(true)` when a row was deleted and `Ok(false)` when there was none.
pub fn delete(conn: &Connection, key: &str) -> Result<bool, RepoError> {
    validate_key(key).map_err(|_| RepoError::InvalidKey)?;
    let removed = conn.execute("DELETE FROM managed_posts WHERE key = ?1", [key])?;
    Ok(removed > 0)
}

/// Parses an id that is stored as text.
///
/// A value that is not a number, or that is the id zero for a required id, is a
/// corrupt row: it is reported, never defaulted to zero.
fn parse_id(text: String) -> Result<u64, RepoError> {
    let id = text.parse::<u64>().map_err(|_| RepoError::InvalidId)?;
    if id == 0 {
        return Err(RepoError::InvalidId);
    }
    Ok(id)
}

/// Parses an id that may be the id zero.
///
/// `updated_by` is the id of the user that caused the last write. The id zero means
/// "not a user", for example a startup sync, so it is accepted here.
fn parse_optional_id(text: String) -> Result<u64, RepoError> {
    text.parse::<u64>().map_err(|_| RepoError::InvalidId)
}

/// Stable error codes of the repository.
///
/// The codes never carry SQL text, paths or user data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoError {
    /// Any SQLite engine error (the message is deliberately dropped).
    Sqlite,
    /// The key is not a usable key.
    InvalidKey,
    /// A stored id is not a number, or a required id is the id zero.
    InvalidId,
    /// A stored column does not hold what it must hold.
    CorruptRow,
}

impl RepoError {
    /// Returns the stable code of this reason.
    pub fn code(self) -> &'static str {
        match self {
            RepoError::Sqlite => "sqlite_error",
            RepoError::InvalidKey => "invalid_key",
            RepoError::InvalidId => "invalid_id",
            RepoError::CorruptRow => "corrupt_row",
        }
    }
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for RepoError {}

impl From<rusqlite::Error> for RepoError {
    fn from(_: rusqlite::Error) -> Self {
        RepoError::Sqlite
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Synthetic ids. They are deliberately far from any real Discord snowflake and
    // carry no meaning: 18 digits that were never issued by Discord.
    const GUILD: u64 = 1_000_000_000_000_000_001;
    const CHANNEL: u64 = 1_000_000_000_000_000_002;
    const OTHER_CHANNEL: u64 = 1_000_000_000_000_000_003;
    const OTHER_GUILD: u64 = 1_000_000_000_000_000_004;
    const MESSAGE: u64 = 1_000_000_000_000_000_005;
    const NEW_MESSAGE: u64 = 1_000_000_000_000_000_006;
    const USER: u64 = 1_000_000_000_000_000_007;

    /// A content hash of 64 lowercase hex characters.
    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    /// A second, different content hash.
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Builds a request for a key with no recorded post.
    fn request<'a>(key: &'a str, hash: &'a str) -> PlanRequest<'a> {
        PlanRequest {
            key,
            template_hash: hash,
            recorded: None,
            target_guild: GUILD,
            target_channel: CHANNEL,
            message_missing: false,
            allow_recreate: false,
        }
    }

    /// Builds a record of a published post.
    fn record(hash: &str) -> PostRecord {
        PostRecord {
            key: "general-rules".to_string(),
            guild: GUILD,
            channel: CHANNEL,
            message: MESSAGE,
            content_hash: hash.to_string(),
            updated_by: USER,
        }
    }

    /// A scratch directory for the file-backed database tests.
    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("em1-embeds-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An in-memory database with the `embeds` feature migrated.
    fn mem_db() -> Connection {
        let mut conn = crate::storage::open(":memory:", &[]).unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    /// The same template published twice is a no-op: reposting would spam the channel.
    #[test]
    fn same_template_twice_is_a_no_op() {
        let recorded = record(HASH_A);
        let mut req = request("general-rules", HASH_A);
        req.recorded = Some(&recorded);
        assert_eq!(plan(&req), Plan::NoOp);
    }

    /// A changed template edits the SAME recorded message; it never creates a new post.
    #[test]
    fn changed_template_edits_the_same_message() {
        let recorded = record(HASH_A);
        let mut req = request("general-rules", HASH_B);
        req.recorded = Some(&recorded);
        assert_eq!(plan(&req), Plan::Edit { message: MESSAGE });
    }

    /// A key with no record is created.
    #[test]
    fn unknown_key_is_created() {
        assert_eq!(plan(&request("general-rules", HASH_A)), Plan::Create);
    }

    /// A recorded message that is gone is refused unless the caller explicitly allows
    /// the post to be created again.
    #[test]
    fn missing_message_is_refused_unless_recreate_is_allowed() {
        let recorded = record(HASH_A);
        let mut req = request("general-rules", HASH_A);
        req.recorded = Some(&recorded);
        req.message_missing = true;
        assert_eq!(plan(&req), Plan::Refuse(RefuseReason::MissingMessage));

        req.allow_recreate = true;
        assert_eq!(plan(&req), Plan::Recreate);
    }

    /// A missing message is refused even when the template changed: the caller must
    /// allow the recreation, not the content.
    #[test]
    fn missing_message_is_refused_even_when_the_template_changed() {
        let recorded = record(HASH_A);
        let mut req = request("general-rules", HASH_B);
        req.recorded = Some(&recorded);
        req.message_missing = true;
        assert_eq!(plan(&req), Plan::Refuse(RefuseReason::MissingMessage));
    }

    /// A key already recorded for another channel is refused: the same key must not
    /// publish the same post in two places.
    #[test]
    fn key_reuse_in_another_channel_is_refused() {
        let recorded = record(HASH_A);
        let mut req = request("general-rules", HASH_B);
        req.recorded = Some(&recorded);
        req.target_channel = OTHER_CHANNEL;
        assert_eq!(
            plan(&req),
            Plan::Refuse(RefuseReason::KeyReusedInAnotherChannel)
        );
    }

    /// A key already recorded for another guild is refused.
    #[test]
    fn key_reuse_in_another_guild_is_refused() {
        let recorded = record(HASH_A);
        let mut req = request("general-rules", HASH_B);
        req.recorded = Some(&recorded);
        req.target_guild = OTHER_GUILD;
        assert_eq!(
            plan(&req),
            Plan::Refuse(RefuseReason::KeyReusedInAnotherGuild)
        );
    }

    /// Allowing a recreation does not allow a key to be reused in another channel.
    #[test]
    fn allow_recreate_does_not_allow_key_reuse() {
        let recorded = record(HASH_A);
        let mut req = request("general-rules", HASH_B);
        req.recorded = Some(&recorded);
        req.target_channel = OTHER_CHANNEL;
        req.allow_recreate = true;
        assert_eq!(
            plan(&req),
            Plan::Refuse(RefuseReason::KeyReusedInAnotherChannel)
        );
    }

    /// A key that is not usable is refused before anything else is considered.
    #[test]
    fn an_unusable_key_is_refused() {
        for key in [
            "",
            "  ",
            "with space",
            "../escape",
            "a/b",
            "a\\b",
            "..",
            ".",
        ] {
            let req = request(key, HASH_A);
            assert_eq!(
                plan(&req),
                Plan::Refuse(RefuseReason::InvalidKey),
                "{key:?}"
            );
        }
    }

    /// A key that is too long is refused.
    #[test]
    fn an_over_long_key_is_refused() {
        let key = "k".repeat(256);
        let req = request(&key, HASH_A);
        assert_eq!(plan(&req), Plan::Refuse(RefuseReason::InvalidKey));
    }

    /// A template hash that is not a SHA-256 hex digest is refused: it could not be
    /// compared with the recorded one.
    #[test]
    fn a_malformed_hash_is_refused() {
        for hash in ["", "not-a-hash", "abc", &"A".repeat(64)] {
            let req = request("general-rules", hash);
            assert_eq!(
                plan(&req),
                Plan::Refuse(RefuseReason::InvalidHash),
                "{hash:?}"
            );
        }
    }

    /// A recorded hash that is not a SHA-256 hex digest cannot match, so the plan is an
    /// edit rather than a no-op.
    #[test]
    fn a_corrupt_recorded_hash_does_not_produce_a_no_op() {
        let recorded = record("");
        let mut req = request("general-rules", HASH_A);
        req.recorded = Some(&recorded);
        assert_eq!(plan(&req), Plan::Edit { message: MESSAGE });
    }

    /// Every refusal code is distinct and stable.
    #[test]
    fn refusal_codes_are_distinct_and_stable() {
        let all = [
            RefuseReason::InvalidKey,
            RefuseReason::InvalidHash,
            RefuseReason::KeyReusedInAnotherChannel,
            RefuseReason::KeyReusedInAnotherGuild,
            RefuseReason::MissingMessage,
        ];
        let mut codes: Vec<_> = all.iter().map(|reason| reason.code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        assert_eq!(RefuseReason::MissingMessage.code(), "missing_message");
    }

    /// The planner is a pure function: the same request always gives the same plan,
    /// and the plan never depends on the order the facts are read.
    #[test]
    fn the_planner_is_pure() {
        let recorded = record(HASH_A);
        let mut req = request("general-rules", HASH_A);
        req.recorded = Some(&recorded);
        let first = plan(&req);
        for _ in 0..10 {
            assert_eq!(plan(&req), first);
        }
    }

    /// The migrations create the `managed_posts` table and are idempotent.
    #[test]
    fn migrations_create_the_table_and_are_idempotent() {
        let mut conn = crate::storage::open(":memory:", &[]).unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), 1);
        assert_eq!(migrate(&mut conn).unwrap(), 1);
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='managed_posts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
    }

    /// A record survives a round trip through the database.
    #[test]
    fn repository_round_trip() {
        let conn = mem_db();
        let original = record(HASH_A);
        upsert(&conn, &original, 1_700_000_000_000).unwrap();

        let stored = get(&conn, "general-rules").unwrap().expect("row exists");
        assert_eq!(stored.record, original);
        assert_eq!(stored.updated_at, 1_700_000_000_000);
    }

    /// A key with no row reads back as `None`.
    #[test]
    fn reading_an_unknown_key_returns_none() {
        let conn = mem_db();
        assert!(get(&conn, "general-rules").unwrap().is_none());
    }

    /// Writing the same key twice replaces the row instead of adding one.
    #[test]
    fn writing_the_same_key_twice_replaces_the_row() {
        let conn = mem_db();
        upsert(&conn, &record(HASH_A), 1).unwrap();
        let mut second = record(HASH_B);
        second.message = NEW_MESSAGE;
        upsert(&conn, &second, 2).unwrap();

        let all = list(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].record.message, NEW_MESSAGE);
        assert_eq!(all[0].record.content_hash, HASH_B);
        assert_eq!(all[0].updated_at, 2);
    }

    /// The stored row survives reopening the database file.
    #[test]
    fn the_record_survives_reopening_the_file() {
        let dir = scratch("roundtrip");
        let path = dir.join("core.sqlite");
        let path_str = path.to_str().unwrap().to_string();
        {
            let mut conn = crate::storage::open(&path_str, &[]).unwrap();
            migrate(&mut conn).unwrap();
            upsert(&conn, &record(HASH_A), 42).unwrap();
        }
        let conn = crate::storage::open(&path_str, &[]).unwrap();
        let stored = get(&conn, "general-rules").unwrap().expect("row exists");
        assert_eq!(stored.record, record(HASH_A));
        assert_eq!(stored.updated_at, 42);
    }

    /// The repository stores ids as text and parses them back.
    #[test]
    fn ids_are_stored_as_text_and_parsed_back() {
        let conn = mem_db();
        upsert(&conn, &record(HASH_A), 1).unwrap();
        let guild: String = conn
            .query_row(
                "SELECT guild FROM managed_posts WHERE key = ?1",
                ["general-rules"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(guild, GUILD.to_string());
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.record.guild, GUILD);
        assert_eq!(stored.record.channel, CHANNEL);
        assert_eq!(stored.record.message, MESSAGE);
        assert_eq!(stored.record.updated_by, USER);
    }

    /// A row whose guild is not a number is reported, not silently defaulted.
    #[test]
    fn a_corrupt_id_is_reported() {
        let conn = mem_db();
        upsert(&conn, &record(HASH_A), 1).unwrap();
        conn.execute(
            "UPDATE managed_posts SET guild = 'not-a-number' WHERE key = 'general-rules'",
            [],
        )
        .unwrap();
        assert_eq!(
            get(&conn, "general-rules").unwrap_err(),
            RepoError::InvalidId
        );
    }

    /// A stored id of zero is reported: the id zero is not a real guild, channel or
    /// message, so it must never be read back as if it were.
    #[test]
    fn a_stored_id_of_zero_is_reported() {
        let conn = mem_db();
        upsert(&conn, &record(HASH_A), 1).unwrap();
        for column in ["guild", "channel", "message"] {
            conn.execute(
                &format!("UPDATE managed_posts SET {column} = '0' WHERE key = 'general-rules'"),
                [],
            )
            .unwrap();
            assert_eq!(
                get(&conn, "general-rules").unwrap_err(),
                RepoError::InvalidId,
                "column {column}"
            );
            upsert(&conn, &record(HASH_A), 1).unwrap();
        }
    }

    /// A stored `updated_by` of zero is accepted: it means "not a user".
    #[test]
    fn a_stored_updated_by_of_zero_is_accepted() {
        let conn = mem_db();
        let mut original = record(HASH_A);
        original.updated_by = 0;
        upsert(&conn, &original, 1).unwrap();
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.record.updated_by, 0);
    }

    /// A row whose content hash is not a hash is reported.
    #[test]
    fn a_corrupt_hash_is_reported() {
        let conn = mem_db();
        upsert(&conn, &record(HASH_A), 1).unwrap();
        conn.execute(
            "UPDATE managed_posts SET content_hash = 'nope' WHERE key = 'general-rules'",
            [],
        )
        .unwrap();
        assert_eq!(
            get(&conn, "general-rules").unwrap_err(),
            RepoError::CorruptRow
        );
    }

    /// A key that is not usable is refused by the repository too.
    #[test]
    fn the_repository_refuses_an_unusable_key() {
        let conn = mem_db();
        assert_eq!(get(&conn, "../escape").unwrap_err(), RepoError::InvalidKey);
        let mut bad = record(HASH_A);
        bad.key = "../escape".to_string();
        assert_eq!(upsert(&conn, &bad, 1).unwrap_err(), RepoError::InvalidKey);
    }

    /// A record with the id zero is refused: it is not a real message.
    #[test]
    fn a_zero_message_id_is_refused() {
        let conn = mem_db();
        let mut bad = record(HASH_A);
        bad.message = 0;
        assert_eq!(upsert(&conn, &bad, 1).unwrap_err(), RepoError::InvalidId);
    }

    /// A plan that forbids a write leaves the table untouched.
    #[test]
    fn a_forbidden_plan_writes_nothing() {
        let conn = mem_db();
        let original = record(HASH_A);

        assert!(!apply(&conn, &Plan::NoOp, &original, 1).unwrap());
        assert!(list(&conn).unwrap().is_empty());

        let refuse = Plan::Refuse(RefuseReason::MissingMessage);
        assert!(!apply(&conn, &refuse, &original, 1).unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }

    /// A plan that allows a write stores the record.
    #[test]
    fn an_allowed_plan_writes_the_record() {
        let conn = mem_db();
        let original = record(HASH_A);

        assert!(apply(&conn, &Plan::Create, &original, 7).unwrap());
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.record, original);
        assert_eq!(stored.updated_at, 7);
    }

    /// The plan guards the write: the same template twice writes nothing the second
    /// time, so the row keeps its first timestamp.
    #[test]
    fn the_plan_guards_the_second_write() {
        let conn = mem_db();
        let original = record(HASH_A);
        apply(&conn, &Plan::Create, &original, 7).unwrap();
        assert!(!apply(&conn, &Plan::NoOp, &original, 99).unwrap());
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.updated_at, 7);
    }

    /// Deleting a record removes it; deleting an unknown key changes nothing.
    #[test]
    fn delete_removes_the_record() {
        let conn = mem_db();
        upsert(&conn, &record(HASH_A), 1).unwrap();
        assert!(delete(&conn, "general-rules").unwrap());
        assert!(get(&conn, "general-rules").unwrap().is_none());
        assert!(!delete(&conn, "general-rules").unwrap());
    }

    /// Listing returns every row, ordered by key.
    #[test]
    fn list_returns_every_row_ordered_by_key() {
        let conn = mem_db();
        for key in ["nsfw-rules", "general-rules", "welcome"] {
            let mut row = record(HASH_A);
            row.key = key.to_string();
            upsert(&conn, &row, 1).unwrap();
        }
        let keys: Vec<String> = list(&conn)
            .unwrap()
            .into_iter()
            .map(|stored| stored.record.key)
            .collect();
        assert_eq!(keys, ["general-rules", "nsfw-rules", "welcome"]);
    }

    /// The repository errors carry no SQL text and no user data.
    #[test]
    fn repository_errors_carry_no_data() {
        for err in [
            RepoError::Sqlite,
            RepoError::InvalidKey,
            RepoError::InvalidId,
            RepoError::CorruptRow,
        ] {
            let text = format!("{err:?} | {err}");
            assert!(text.len() < 60, "{text}");
            assert!(!text.contains("managed_posts"), "{text}");
            assert!(!text.contains("SELECT"), "{text}");
        }
        assert_eq!(RepoError::Sqlite.to_string(), RepoError::Sqlite.code());
    }
}

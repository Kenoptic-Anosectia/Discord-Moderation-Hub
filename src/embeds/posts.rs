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
//! Limits, stated plainly:
//! * The plan never names a message that was not recorded, and [`apply`] refuses a
//!   record that contradicts its plan.
//! * The plan cannot know whether a post that was just made really exists, and it
//!   cannot know whether the record of a post was written. A post that succeeds on
//!   Discord and whose record write then fails leaves the table without a row, so
//!   the next sync plans a `Create` again and a second copy of the post is made.
//!   That window is between the post and the record of it, and it is not closed by
//!   this module.
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
    /// The key is not a usable key: it is empty, too long, or not written in the one
    /// canonical spelling this module accepts.
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
/// A key names one managed post and, through the wiring card, one template file. It
/// is therefore restricted to ONE canonical spelling: a lowercase ASCII name that
/// starts with a letter or a digit and continues with letters, digits, `-` or `_`.
///
/// The spelling is canonical on purpose. The host and the target are Windows, where
/// the file system is case-insensitive: `General-Rules` and `general-rules` would be
/// two database rows for one template file, and both would plan a `Create`, so the
/// same post would be published twice. For the same reason the spelling carries no
/// dot (a trailing dot is a Windows path spelling, not a name), no path separator
/// and no leading character that could make a hidden file, and it is not one of the
/// reserved device names (`con`, `prn`, `aux`, `nul`, `com0`-`com9`, `lpt0`-`lpt9`),
/// which name a device and not a file.
///
/// Every other spelling is refused, so a key can never walk out of the template
/// directory and never names two different files at once.
pub fn validate_key(key: &str) -> Result<(), RefuseReason> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(RefuseReason::InvalidKey);
    }
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return Err(RefuseReason::InvalidKey);
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(RefuseReason::InvalidKey);
    }
    if !bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    }) {
        return Err(RefuseReason::InvalidKey);
    }
    if is_reserved_device_name(key) {
        return Err(RefuseReason::InvalidKey);
    }
    Ok(())
}

/// True when the key is one of the reserved Windows device names.
///
/// The names are already lowercase, because a key with a capital letter is refused
/// before this is reached.
fn is_reserved_device_name(key: &str) -> bool {
    const STATIC_NAMES: [&str; 4] = ["con", "prn", "aux", "nul"];
    if STATIC_NAMES.contains(&key) {
        return true;
    }
    // `com1`..`com9` and `lpt1`..`lpt9` are the numbered devices. `com0` is not one
    // on Windows, but it is refused as well: the whole class is refused, so a host
    // that does reserve it cannot make the module accept it.
    let Some(rest) = key.strip_prefix("com").or_else(|| key.strip_prefix("lpt")) else {
        return false;
    };
    matches!(
        rest,
        "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"
    )
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
///
/// This is the raw write. It does not look at a plan. Use [`apply`] to write a row
/// that follows a plan.
pub fn upsert(conn: &Connection, record: &PostRecord, now: i64) -> Result<(), RepoError> {
    check_record(record)?;
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

/// Writes the record of one managed post and refuses to replace an existing row.
///
/// This is the write of a `Create` plan: the post does not exist yet, so a key that
/// already has a row is a stale plan and is refused with
/// [`RepoError::AlreadyExists`]. The primary key of the table does the check, so no
/// compare-and-swap is needed.
fn insert(conn: &Connection, record: &PostRecord, now: i64) -> Result<(), RepoError> {
    check_record(record)?;
    conn.execute(
        "INSERT INTO managed_posts
            (key, guild, channel, message, content_hash, updated_by, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            record.key,
            record.guild.to_string(),
            record.channel.to_string(),
            record.message.to_string(),
            record.content_hash,
            record.updated_by.to_string(),
            now,
        ],
    )
    .map_err(|err| match err {
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            RepoError::AlreadyExists
        }
        other => RepoError::from(other),
    })?;
    Ok(())
}

/// Writes the record only when the plan allows a write AND the record agrees with
/// the plan.
///
/// Returns `Ok(true)` when a row was written and `Ok(false)` when the plan forbids a
/// write, which is the case for `Plan::NoOp` and `Plan::Refuse`.
///
/// The record is checked against the plan before anything is written:
/// * an `Edit` plan names the message that must be edited, so a record that names
///   another message is refused with `plan_mismatch`: writing it would orphan the
///   real message and forget where it is;
/// * an `Edit` or `Recreate` plan edits the post that is already recorded, so a
///   record that names another guild or another channel is refused with
///   `plan_mismatch`: the key would then live in two places;
/// * a `Create` plan posts a post that does not exist yet, so a key that already has
///   a row is refused with `already_exists`: the recorded post would be forgotten
///   and the same post would be published twice.
///
/// The check is a read followed by a write, not an atomic compare-and-swap. Two
/// processes that write the same key at the same moment can still race between the
/// read and the write; the card for this module asks for simple upserts, not for a
/// compare-and-swap.
pub fn apply(
    conn: &Connection,
    plan: &Plan,
    record: &PostRecord,
    now: i64,
) -> Result<bool, RepoError> {
    match plan {
        Plan::NoOp | Plan::Refuse(_) => Ok(false),
        Plan::Create => {
            check_new_record(conn, record)?;
            insert(conn, record, now)?;
            Ok(true)
        }
        Plan::Edit { message } => {
            check_recorded_post(conn, record, Some(*message))?;
            upsert(conn, record, now)?;
            Ok(true)
        }
        Plan::Recreate => {
            check_recorded_post(conn, record, None)?;
            upsert(conn, record, now)?;
            Ok(true)
        }
    }
}

/// Checks that a `Create` plan may write: the key must have no row yet, and the
/// record itself must be usable.
fn check_new_record(conn: &Connection, record: &PostRecord) -> Result<(), RepoError> {
    check_record(record)?;
    if get(conn, &record.key)?.is_some() {
        return Err(RepoError::AlreadyExists);
    }
    Ok(())
}

/// Checks that an `Edit` or `Recreate` plan may write.
///
/// `message` is the message an `Edit` plan names. A `Recreate` plan has no message:
/// the old one is gone and a new one is posted, so only the guild and the channel
/// are checked.
fn check_recorded_post(
    conn: &Connection,
    record: &PostRecord,
    message: Option<u64>,
) -> Result<(), RepoError> {
    check_record(record)?;
    let Some(stored) = get(conn, &record.key)? else {
        // There is no recorded post, so there is nothing to edit or to recreate.
        return Err(RepoError::PlanMismatch);
    };
    if stored.record.guild != record.guild || stored.record.channel != record.channel {
        return Err(RepoError::PlanMismatch);
    }
    if let Some(message) = message {
        if stored.record.message != message || record.message != message {
            return Err(RepoError::PlanMismatch);
        }
    }
    Ok(())
}

/// Checks that a record itself is usable before it is written.
fn check_record(record: &PostRecord) -> Result<(), RepoError> {
    validate_key(&record.key).map_err(|_| RepoError::InvalidKey)?;
    if !is_content_hash(&record.content_hash) {
        return Err(RepoError::CorruptRow);
    }
    if record.guild == 0 || record.channel == 0 || record.message == 0 {
        return Err(RepoError::InvalidId);
    }
    Ok(())
}

/// Lists every managed post, ordered by key.
///
/// A row whose key is not a usable key is reported as [`RepoError::CorruptRow`]: this
/// module never writes such a row, so it can only be a row that was written by
/// something else or that was corrupted.
pub fn list(conn: &Connection) -> Result<Vec<StoredPost>, RepoError> {
    let mut stmt = conn.prepare(
        "SELECT key, guild, channel, message, content_hash, updated_by, updated_at
         FROM managed_posts ORDER BY key ASC",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        validate_key(&key).map_err(|_| RepoError::CorruptRow)?;
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
/// A value that is not a number, that is not written the way this module writes it
/// (a `+` sign or a leading zero is a second spelling of the same number) or that
/// is the id zero for a required id, is a corrupt row: it is reported, never
/// defaulted to zero.
fn parse_id(text: String) -> Result<u64, RepoError> {
    let id = parse_canonical_id(&text)?;
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
    parse_canonical_id(&text)
}

/// Parses an id that is written the way this module writes it.
///
/// The text must be the plain decimal spelling of the number: no sign, no leading
/// zeros and no whitespace. A second spelling of the same number would be a second
/// way to write the same id, so it is refused rather than silently accepted.
fn parse_canonical_id(text: &str) -> Result<u64, RepoError> {
    let id = text.parse::<u64>().map_err(|_| RepoError::InvalidId)?;
    if id.to_string() != text {
        return Err(RepoError::InvalidId);
    }
    Ok(id)
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
    /// The record does not match the plan it is written under.
    PlanMismatch,
    /// The row already exists and the plan does not allow replacing it.
    AlreadyExists,
}

impl RepoError {
    /// Returns the stable code of this reason.
    pub fn code(self) -> &'static str {
        match self {
            RepoError::Sqlite => "sqlite_error",
            RepoError::InvalidKey => "invalid_key",
            RepoError::InvalidId => "invalid_id",
            RepoError::CorruptRow => "corrupt_row",
            RepoError::PlanMismatch => "plan_mismatch",
            RepoError::AlreadyExists => "already_exists",
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
            RepoError::PlanMismatch,
            RepoError::AlreadyExists,
        ] {
            let text = format!("{err:?} | {err}");
            assert!(text.len() < 60, "{text}");
            assert!(!text.contains("managed_posts"), "{text}");
            assert!(!text.contains("SELECT"), "{text}");
        }
        assert_eq!(RepoError::Sqlite.to_string(), RepoError::Sqlite.code());
    }

    /// A key that differs only in case names the SAME template file on Windows, so
    /// only ONE canonical spelling is accepted. Two spellings would be two rows and
    /// two posts of the same template.
    #[test]
    fn a_case_alias_of_a_key_is_refused() {
        assert!(validate_key("general-rules").is_ok());
        for key in [
            "General-Rules",
            "GENERAL-RULES",
            "General-rules",
            "gEnErAl-RuLeS",
        ] {
            assert_eq!(
                validate_key(key).unwrap_err(),
                RefuseReason::InvalidKey,
                "{key:?}"
            );
        }
    }

    /// The reserved Windows device names name a device, not a template file.
    #[test]
    fn a_reserved_device_name_is_refused() {
        for key in [
            "con", "prn", "aux", "nul", "com0", "com1", "com9", "lpt0", "lpt1", "lpt9",
        ] {
            assert_eq!(
                validate_key(key).unwrap_err(),
                RefuseReason::InvalidKey,
                "{key:?}"
            );
        }
        // A name that merely starts like one is a normal file name.
        assert!(validate_key("console").is_ok());
        assert!(validate_key("com10").is_ok());
        assert!(validate_key("nul-rules").is_ok());
    }

    /// A key with a dot in it is refused: a trailing dot is a Windows path spelling
    /// and a dot separates an extension.
    #[test]
    fn a_key_with_a_dot_is_refused() {
        for key in ["rules.", "rules.json", ".rules", "ru.les"] {
            assert_eq!(
                validate_key(key).unwrap_err(),
                RefuseReason::InvalidKey,
                "{key:?}"
            );
        }
    }

    /// A key must start with a letter or a digit: a leading `-` or `_` is not a
    /// plain name.
    #[test]
    fn a_key_with_a_leading_mark_is_refused() {
        for key in ["-rules", "_rules", "-", "_"] {
            assert_eq!(
                validate_key(key).unwrap_err(),
                RefuseReason::InvalidKey,
                "{key:?}"
            );
        }
    }

    /// A key that is too long is refused; the limit is 64 bytes.
    #[test]
    fn a_key_of_the_limit_length_is_accepted() {
        let key = "a".repeat(MAX_KEY_BYTES);
        assert!(validate_key(&key).is_ok());
        let longer = "a".repeat(MAX_KEY_BYTES + 1);
        assert_eq!(validate_key(&longer).unwrap_err(), RefuseReason::InvalidKey);
    }

    /// The planner refuses the same spellings the repository does.
    #[test]
    fn the_planner_refuses_a_case_alias() {
        let req = request("General-Rules", HASH_A);
        assert_eq!(plan(&req), Plan::Refuse(RefuseReason::InvalidKey));
    }

    /// An `Edit` plan names the message that must be edited. A record that names a
    /// DIFFERENT message contradicts the plan and is refused: writing it would
    /// orphan the real message and forget where it is.
    #[test]
    fn an_edit_for_another_message_is_refused() {
        let conn = mem_db();
        let original = record(HASH_A);
        apply(&conn, &Plan::Create, &original, 1).unwrap();

        let mut wrong = original.clone();
        wrong.message = NEW_MESSAGE;
        assert_eq!(
            apply(&conn, &Plan::Edit { message: MESSAGE }, &wrong, 2).unwrap_err(),
            RepoError::PlanMismatch
        );
        // The row is untouched.
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.record.message, MESSAGE);
        assert_eq!(stored.updated_at, 1);
    }

    /// An `Edit` plan names the message; a record that names the RIGHT message is
    /// written.
    #[test]
    fn an_edit_for_the_planned_message_is_written() {
        let conn = mem_db();
        let original = record(HASH_A);
        apply(&conn, &Plan::Create, &original, 1).unwrap();

        let mut edited = original.clone();
        edited.content_hash = HASH_B.to_string();
        assert!(apply(&conn, &Plan::Edit { message: MESSAGE }, &edited, 2).unwrap());
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.record.content_hash, HASH_B);
        assert_eq!(stored.updated_at, 2);
    }

    /// A `Create` plan must not replace a row that already exists: the recorded post
    /// would be forgotten and the same post would be published twice.
    #[test]
    fn a_create_over_an_existing_row_is_refused() {
        let conn = mem_db();
        let original = record(HASH_A);
        apply(&conn, &Plan::Create, &original, 1).unwrap();

        let mut stale = original.clone();
        stale.message = NEW_MESSAGE;
        stale.channel = OTHER_CHANNEL;
        assert_eq!(
            apply(&conn, &Plan::Create, &stale, 2).unwrap_err(),
            RepoError::AlreadyExists
        );
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.record.message, MESSAGE);
        assert_eq!(stored.record.channel, CHANNEL);
    }

    /// A `Recreate` plan replaces the row, because the old message is gone.
    #[test]
    fn a_recreate_replaces_the_row() {
        let conn = mem_db();
        let original = record(HASH_A);
        apply(&conn, &Plan::Create, &original, 1).unwrap();

        let mut recreated = original.clone();
        recreated.message = NEW_MESSAGE;
        recreated.content_hash = HASH_B.to_string();
        assert!(apply(&conn, &Plan::Recreate, &recreated, 2).unwrap());
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.record.message, NEW_MESSAGE);
        assert_eq!(stored.updated_at, 2);
    }

    /// A record that names another channel than the one on file must not move the
    /// post to another channel: the key would then live in two places.
    #[test]
    fn a_write_that_moves_the_channel_is_refused() {
        let conn = mem_db();
        let original = record(HASH_A);
        apply(&conn, &Plan::Create, &original, 1).unwrap();

        let mut moved = original.clone();
        moved.channel = OTHER_CHANNEL;
        assert_eq!(
            apply(&conn, &Plan::Edit { message: MESSAGE }, &moved, 2).unwrap_err(),
            RepoError::PlanMismatch
        );
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.record.channel, CHANNEL);
    }

    /// A record that names another guild than the one on file must not move the post
    /// to another guild.
    #[test]
    fn a_write_that_moves_the_guild_is_refused() {
        let conn = mem_db();
        let original = record(HASH_A);
        apply(&conn, &Plan::Create, &original, 1).unwrap();

        let mut moved = original.clone();
        moved.guild = OTHER_GUILD;
        assert_eq!(
            apply(&conn, &Plan::Edit { message: MESSAGE }, &moved, 2).unwrap_err(),
            RepoError::PlanMismatch
        );
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.record.guild, GUILD);
    }

    /// An `Edit` plan for a key that has no recorded row is refused: there is no
    /// post to edit, so writing a row would invent one.
    #[test]
    fn an_edit_without_a_recorded_row_is_refused() {
        let conn = mem_db();
        assert_eq!(
            apply(&conn, &Plan::Edit { message: MESSAGE }, &record(HASH_A), 1).unwrap_err(),
            RepoError::PlanMismatch
        );
        assert!(list(&conn).unwrap().is_empty());
    }

    /// A `Recreate` plan for a key that has no recorded row is refused for the same
    /// reason: there is nothing to recreate.
    #[test]
    fn a_recreate_without_a_recorded_row_is_refused() {
        let conn = mem_db();
        assert_eq!(
            apply(&conn, &Plan::Recreate, &record(HASH_A), 1).unwrap_err(),
            RepoError::PlanMismatch
        );
        assert!(list(&conn).unwrap().is_empty());
    }

    /// A `Create` plan for a key that has no row writes the row.
    #[test]
    fn a_create_for_an_unknown_key_writes_the_row() {
        let conn = mem_db();
        assert!(apply(&conn, &Plan::Create, &record(HASH_A), 5).unwrap());
        let stored = get(&conn, "general-rules").unwrap().unwrap();
        assert_eq!(stored.updated_at, 5);
    }

    /// Listing a table that holds a row with an unusable key reports the row as
    /// corrupt: the key is not a key this module would ever write.
    #[test]
    fn a_stored_key_that_is_unusable_is_reported() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO managed_posts
                (key, guild, channel, message, content_hash, updated_by, updated_at)
             VALUES ('../evil', ?1, ?2, ?3, ?4, ?5, 1)",
            rusqlite::params![
                GUILD.to_string(),
                CHANNEL.to_string(),
                MESSAGE.to_string(),
                HASH_A,
                USER.to_string(),
            ],
        )
        .unwrap();
        assert_eq!(list(&conn).unwrap_err(), RepoError::CorruptRow);
        // The row cannot be read by its key either: the key is refused before the
        // row is looked up.
        assert_eq!(get(&conn, "../evil").unwrap_err(), RepoError::InvalidKey);
    }

    /// A stored id that is not written the way this module writes it is corrupt: a
    /// second spelling of the same number must not be read back as a valid id.
    #[test]
    fn a_non_canonical_stored_id_is_reported() {
        let conn = mem_db();
        upsert(&conn, &record(HASH_A), 1).unwrap();
        for spelling in [
            format!("+{MESSAGE}"),
            format!("0{MESSAGE}"),
            format!("00{MESSAGE}"),
        ] {
            conn.execute(
                "UPDATE managed_posts SET message = ?1 WHERE key = 'general-rules'",
                [&spelling],
            )
            .unwrap();
            assert_eq!(
                get(&conn, "general-rules").unwrap_err(),
                RepoError::InvalidId,
                "{spelling}"
            );
            upsert(&conn, &record(HASH_A), 1).unwrap();
        }
    }

    /// A seeded sequence of sync steps must never leave the module in a state where
    /// work is silently forgotten, and it must reach `NoOp` once the template stops
    /// changing.
    ///
    /// The model runtime below drives the real `plan` and the real `apply`:
    /// * a template that changes every few steps,
    /// * a message that a moderator deletes now and then,
    /// * a Discord call that fails now and then,
    /// * a record write that fails now and then.
    ///
    /// What is asserted is what the caller observes: the plan of each step, and the
    /// row that is left behind.
    #[test]
    fn a_seeded_sync_sequence_reaches_a_final_state() {
        // A tiny linear congruential generator. No dependency is needed for a seeded
        // pseudo-random sequence.
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> u64 {
                // The constants are the ones of the standard 64-bit LCG.
                self.0 = self
                    .0
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                self.0 >> 11
            }
            fn below(&mut self, bound: u64) -> u64 {
                self.next() % bound
            }
        }

        let conn = mem_db();
        let mut rng = Lcg(0x5EED_1234_ABCD_0001);
        // The messages that exist on Discord for this key.
        let mut live: Vec<u64> = Vec::new();
        let mut now = 1_000i64;
        let mut last_plan = Plan::NoOp;
        let mut edits = 0usize;
        let mut creates = 0usize;
        // The number of record writes that were lost. Every lost write can leave one
        // extra live message behind: the window between a successful post and the
        // record of it. This is the limit the module states plainly.
        let mut lost_writes = 0usize;

        for step in 0..600 {
            now += 1;
            // The owner changes the template now and then.
            let hash = if rng.below(4) == 0 { HASH_B } else { HASH_A };
            // A moderator deletes the post now and then.
            if rng.below(20) == 0 {
                live.clear();
            }
            // The Discord call fails now and then.
            let discord_fails = rng.below(10) == 0;
            // The record write fails now and then.
            let write_fails = rng.below(10) == 0;

            let recorded = get(&conn, "general-rules").unwrap();
            let request = PlanRequest {
                key: "general-rules",
                template_hash: hash,
                recorded: recorded.as_ref().map(|stored| &stored.record),
                target_guild: GUILD,
                target_channel: CHANNEL,
                message_missing: recorded.is_some()
                    && !live.contains(&recorded.as_ref().unwrap().record.message),
                allow_recreate: rng.below(3) == 0,
            };
            let plan = plan(&request);
            last_plan = plan;
            match plan {
                Plan::Create | Plan::Recreate => {
                    creates += 1;
                    if !discord_fails {
                        let new_message = MESSAGE + u64::try_from(step).unwrap();
                        live.push(new_message);
                        let mut new_record = record(hash);
                        new_record.message = new_message;
                        if write_fails {
                            lost_writes += 1;
                        } else {
                            apply(&conn, &plan, &new_record, now).unwrap();
                        }
                    }
                }
                Plan::Edit { message } => {
                    edits += 1;
                    // An edit never changes which message carries the post.
                    assert!(
                        live.contains(&message),
                        "an edit targeted a message that does not exist"
                    );
                    if !discord_fails && !write_fails {
                        let mut edited = record(hash);
                        edited.message = message;
                        apply(&conn, &plan, &edited, now).unwrap();
                    }
                }
                Plan::NoOp | Plan::Refuse(_) => {}
            }
            // Whatever happened, the row that is left behind must be readable and
            // must carry a usable key.
            let stored = get(&conn, "general-rules").unwrap();
            if let Some(stored) = stored {
                assert!(validate_key(&stored.record.key).is_ok());
                assert!(is_content_hash(&stored.record.content_hash));
                assert_eq!(stored.record.guild, GUILD);
                assert_eq!(stored.record.channel, CHANNEL);
                assert_ne!(stored.record.message, 0);
            }
        }

        // The live posts are bounded by the writes that were lost: every live
        // message is either the recorded one or one whose record was lost. This is
        // the duplicate window the module states plainly: a post that succeeds on
        // Discord and whose record write then fails is followed by a second `Create`.
        assert!(
            live.len() <= lost_writes + 1,
            "live messages {} exceed lost writes {} plus one",
            live.len(),
            lost_writes
        );

        // The run did work: it posted and it edited.
        assert!(creates > 0, "the sequence never posted");
        assert!(edits > 0, "the sequence never edited");

        // The last steps of the sequence ran with a template that did not change, so
        // the module must have settled instead of posting again and again. The
        // recorded hash is whatever the last successful write carried: asking for
        // THAT hash again must be a no-op.
        let recorded = get(&conn, "general-rules").unwrap();
        assert!(recorded.is_some(), "nothing was recorded at the end");
        let request = PlanRequest {
            key: "general-rules",
            template_hash: &recorded.as_ref().unwrap().record.content_hash,
            recorded: recorded.as_ref().map(|stored| &stored.record),
            target_guild: GUILD,
            target_channel: CHANNEL,
            message_missing: false,
            allow_recreate: false,
        };
        assert_eq!(plan(&request), Plan::NoOp);
        // The module never went quiet: every step either planned an action or
        // deliberately refused one. A plan that stopped appearing would mean the
        // sync gave up.
        assert!(matches!(
            last_plan,
            Plan::NoOp | Plan::Refuse(_) | Plan::Edit { .. } | Plan::Create | Plan::Recreate
        ));
    }

    /// The same seeded sequence, but with the record write failing often: the module
    /// must still never write a row that contradicts its plan, and it must still
    /// reach a final state instead of trying forever.
    #[test]
    fn a_seeded_sync_sequence_with_lost_writes_still_settles() {
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> u64 {
                self.0 = self
                    .0
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                self.0 >> 11
            }
            fn below(&mut self, bound: u64) -> u64 {
                self.next() % bound
            }
        }

        let conn = mem_db();
        let mut rng = Lcg(0x5EED_1234_ABCD_0002);
        let mut live = true;
        let mut now = 1_000i64;
        let mut posts = 0usize;

        for step in 0..600 {
            now += 1;
            let hash = if rng.below(4) == 0 { HASH_B } else { HASH_A };
            if rng.below(20) == 0 {
                live = false;
            }
            let recorded = get(&conn, "general-rules").unwrap();
            let request = PlanRequest {
                key: "general-rules",
                template_hash: hash,
                recorded: recorded.as_ref().map(|stored| &stored.record),
                target_guild: GUILD,
                target_channel: CHANNEL,
                message_missing: recorded.is_some() && !live,
                allow_recreate: true,
            };
            let plan = plan(&request);
            match plan {
                Plan::Create | Plan::Recreate => {
                    posts += 1;
                    // The post succeeds on Discord, but the record write is lost
                    // every other time.
                    live = true;
                    let mut new_record = record(hash);
                    new_record.message = MESSAGE + u64::try_from(step).unwrap();
                    if rng.below(2) == 0 {
                        // The write is lost. The next step plans `Create` again and
                        // posts a second copy: that is the documented window between
                        // a successful post and the record of it.
                    } else {
                        let _ = apply(&conn, &Plan::Recreate, &new_record, now);
                    }
                }
                Plan::Edit { .. } => {
                    let mut edited = record(hash);
                    if let Some(stored) = &recorded {
                        edited.message = stored.record.message;
                    }
                    let _ = apply(&conn, &plan, &edited, now);
                }
                Plan::NoOp | Plan::Refuse(_) => {}
            }
            // The row that is left behind is always readable and always usable.
            if let Some(stored) = get(&conn, "general-rules").unwrap() {
                assert!(validate_key(&stored.record.key).is_ok());
                assert!(is_content_hash(&stored.record.content_hash));
            }
        }
        assert!(posts > 0, "the sequence never posted");
    }
}

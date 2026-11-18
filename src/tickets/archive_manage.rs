//! Staff-only, Discord-free archive operations. Report data and reply text are separate.

use super::archive::{self, OwnedRoot};
pub use super::archive_index::ArchiveSummary;
use super::archive_index::{self, archive_id, ArchiveId};
use super::archive_scan::{self, Health, ScanEntry};
use super::restore::{self as restore_io, RestoreError, RestoreLimits, RestoreRequest};
use super::{
    closer::Actor,
    policy::Settings,
    repository::Repository,
    retention::{self, Candidate, RetentionPolicy},
};
use rusqlite::{params, OptionalExtension, Row};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::sync::Mutex;

const SAMPLE_LIMIT: usize = 10;
static RESTORE_GATE: Mutex<()> = Mutex::const_new(());

pub struct Manage<'a> {
    pub settings: &'a Settings,
    pub repo: &'a Repository,
    pub root: &'a OwnedRoot,
    pub staging: &'a OwnedRoot,
    pub now: &'a (dyn Fn() -> i64 + Send + Sync),
}

pub const HEALTH_PAGE: usize = 50;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManageError {
    Denied,
    NotFound,
    BadId,
    Db,
    Scan,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct HealthReport {
    pub verified: usize,
    pub missing: usize,
    pub corrupt: usize,
    pub identity_mismatch: usize,
    pub bad_name: usize,
    pub non_verified: Vec<ArchiveId>,
    pub has_more: bool,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OrphanClass {
    pub count: usize,
    pub names: Vec<String>,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OrphanSummary {
    pub unrecorded_archives: OrphanClass,
    pub staging_debris: OrphanClass,
    pub unknown_entries: OrphanClass,
    pub truncated: bool,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RetentionReport {
    pub enabled: bool,
    pub eligible: usize,
    pub sample: Vec<ArchiveId>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    Restored {
        dest_name: String,
        files: u64,
        bytes: u64,
    },
    Busy,
    NotFound,
    BadId,
    Denied,
    Failed(&'static str),
}

fn authorize(m: &Manage<'_>, a: &Actor) -> Result<(), ManageError> {
    if archive_index::authorized(m.settings, a) {
        Ok(())
    } else {
        Err(ManageError::Denied)
    }
}
impl From<archive_index::ManageError> for ManageError {
    fn from(error: archive_index::ManageError) -> Self {
        use archive_index::ManageError as IndexError;
        match error {
            IndexError::Denied => Self::Denied,
            IndexError::NotFound => Self::NotFound,
            IndexError::BadId => Self::BadId,
            IndexError::Db => Self::Db,
        }
    }
}

pub async fn list(
    m: &Manage<'_>,
    a: &Actor,
    limit: usize,
    offset: usize,
) -> Result<Vec<ArchiveSummary>, ManageError> {
    Ok(archive_index::list_archives(m.repo, m.settings, a, limit, offset).await?)
}

pub async fn show(m: &Manage<'_>, a: &Actor, id: &str) -> Result<ArchiveSummary, ManageError> {
    Ok(archive_index::find_archive(m.repo, m.settings, a, id).await?)
}

fn scan_entry(row: &Row<'_>) -> rusqlite::Result<ScanEntry> {
    let channel: String = row.get(6)?;
    Ok(ScanEntry {
        ticket: row.get(0)?,
        seq: row.get(1)?,
        dir_name: row.get(2)?,
        manifest_sha256: row.get(3)?,
        messages: row.get(4)?,
        assets: row.get(5)?,
        channel: channel.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

// OwnedRoot is a canonical pathname with an exclusive-ownership contract, not an open fd.
// Verify the existing root *before* using the constructor (which would otherwise create it).
fn reopen(path: &Path) -> Result<OwnedRoot, RestoreError> {
    restore_io::check_root(path)?;
    let root = OwnedRoot::open(path).map_err(|_| RestoreError::Io)?;
    if root.path() != path {
        return Err(RestoreError::Unsafe);
    }
    Ok(root)
}

pub async fn health(m: &Manage<'_>, a: &Actor, offset: usize) -> Result<HealthReport, ManageError> {
    authorize(m, a)?;
    let guild = m.settings.guild;
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    let (entries, has_more) = m
        .repo
        .db
        .call(move |c| -> rusqlite::Result<_> {
            let mut sql = c.prepare(
                "SELECT a.ticket,a.seq,a.dir_name,a.manifest_sha256,a.messages,a.assets,t.channel \
             FROM ticket_archives a JOIN tickets t ON t.id=a.ticket WHERE t.guild=?1 \
             ORDER BY a.ticket DESC,a.seq DESC LIMIT ?2 OFFSET ?3",
            )?;
            let mut rows = sql.query(params![guild.to_string(), HEALTH_PAGE as i64 + 1, offset])?;
            let mut entries = Vec::new();
            while entries.len() < HEALTH_PAGE {
                let Some(row) = rows.next()? else { break };
                entries.push(scan_entry(row)?);
            }
            let has_more = entries.len() == HEALTH_PAGE && rows.next()?.is_some();
            Ok((entries, has_more))
        })
        .await
        .map_err(|_| ManageError::Db)?;
    if entries.is_empty() {
        return Ok(HealthReport::default());
    }
    let path = m.root.path().to_owned();
    tokio::task::spawn_blocking(move || {
        let root = reopen(&path).map_err(|_| ManageError::Scan)?;
        let mut report = HealthReport {
            has_more,
            ..HealthReport::default()
        };
        for (entry, result) in entries
            .iter()
            .zip(archive_scan::scan_recorded(&root, &entries))
        {
            match result.health {
                Health::Verified => {
                    report.verified += 1;
                    continue;
                }
                Health::Missing => report.missing += 1,
                Health::Corrupt => report.corrupt += 1,
                Health::IdentityMismatch => report.identity_mismatch += 1,
                Health::BadName => report.bad_name += 1,
            }
            report.non_verified.push(archive_id(
                guild,
                entry.ticket,
                entry.seq,
                &entry.manifest_sha256,
            ));
        }
        Ok(report)
    })
    .await
    .map_err(|_| ManageError::Scan)?
}

pub async fn orphans(m: &Manage<'_>, a: &Actor) -> Result<OrphanSummary, ManageError> {
    authorize(m, a)?;
    let guild = m.settings.guild.to_string();
    let mut known: Vec<String> = m.repo.db.call(move |c| -> rusqlite::Result<_> {
        let mut sql = c.prepare(
            "SELECT DISTINCT a.dir_name FROM ticket_archives a JOIN tickets t ON t.id=a.ticket WHERE t.guild=?1")?;
        let names = sql.query_map([guild], |r| r.get(0))?;
        names.collect()
    }).await.map_err(|_| ManageError::Db)?;
    known.sort_unstable();
    let path = m.root.path().to_owned();
    tokio::task::spawn_blocking(move || {
        let root = reopen(&path).map_err(|_| ManageError::Scan)?;
        let report = archive_scan::find_orphans(&root, &known).map_err(|_| ManageError::Scan)?;
        let sample = |mut names: Vec<String>| {
            // This also suppresses a recorded name if its entry has an unexpected type.
            names.retain(|name| known.binary_search(name).is_err());
            let count = names.len();
            names.truncate(SAMPLE_LIMIT);
            OrphanClass { count, names }
        };
        let unrecorded_archives = sample(report.unrecorded_archives);
        let staging_debris = sample(report.staging_debris);
        let unknown_entries = sample(report.unknown_entries);
        let truncated = [&unrecorded_archives, &staging_debris, &unknown_entries]
            .iter()
            .any(|class| class.count > SAMPLE_LIMIT);
        Ok(OrphanSummary {
            unrecorded_archives,
            staging_debris,
            unknown_entries,
            truncated,
        })
    })
    .await
    .map_err(|_| ManageError::Scan)?
}

pub async fn retention_plan(
    m: &Manage<'_>,
    a: &Actor,
    policy: RetentionPolicy,
) -> Result<RetentionReport, ManageError> {
    authorize(m, a)?;
    if policy == RetentionPolicy::Disabled {
        return Ok(RetentionReport::default());
    }
    let guild = m.settings.guild;
    let rows: Vec<(Candidate, String)> = m.repo.db.call(move |c| -> rusqlite::Result<_> {
        let mut sql = c.prepare(
            "SELECT a.ticket,a.seq,a.dir_name,a.manifest_sha256, \
                    (SELECT h.at FROM ticket_history h WHERE h.ticket=t.id AND h.to_status='closed' \
                     AND h.from_status<>'closed' ORDER BY h.id DESC LIMIT 1) \
             FROM ticket_archives a JOIN tickets t ON t.id=a.ticket \
             WHERE t.guild=?1 AND t.status='closed'")?;
        let rows = sql.query_map([guild.to_string()], |r| Ok((Candidate {
            ticket: r.get(0)?, seq: r.get(1)?, dir_name: r.get(2)?, closed_at: r.get(4)?,
        }, r.get(3)?)))?;
        rows.collect()
    }).await.map_err(|_| ManageError::Db)?;
    let mut ids = BTreeMap::new();
    let candidates: Vec<_> = rows
        .into_iter()
        .map(|(candidate, sha)| {
            ids.insert((candidate.ticket, candidate.seq), sha);
            candidate
        })
        .collect();
    let plan = retention::plan_purge(policy, (m.now)(), &candidates);
    let sample = plan
        .iter()
        .take(SAMPLE_LIMIT)
        .map(|c| archive_id(guild, c.ticket, c.seq, &ids[&(c.ticket, c.seq)]))
        .collect();
    Ok(RetentionReport {
        enabled: true,
        eligible: plan.len(),
        sample,
    })
}

async fn restore_record(m: &Manage<'_>, a: &Actor, id: &str) -> Result<ScanEntry, ManageError> {
    let selected = show(m, a, id).await?;
    let guild = m.settings.guild;
    let entry =
        m.repo
            .db
            .call(move |c| -> rusqlite::Result<_> {
                c.query_row(
            "SELECT a.ticket,a.seq,a.dir_name,a.manifest_sha256,a.messages,a.assets,t.channel \
             FROM ticket_archives a JOIN tickets t ON t.id=a.ticket \
             WHERE t.guild=?1 AND a.ticket=?2 AND a.seq=?3",
            params![guild.to_string(), selected.ticket, selected.seq], scan_entry,
        ).optional()
            })
            .await
            .map_err(|_| ManageError::Db)?;
    // Do not restore a different version if the row changed after opaque-id lookup.
    entry
        .filter(|r| archive_id(guild, r.ticket, r.seq, &r.manifest_sha256).as_str() == id)
        .ok_or(ManageError::NotFound)
}

/// A16 reports hash mismatch and damaged content as Verify. Only classify a
/// differing digest as Identity if the safely reopened source also self-verifies.
fn stale_hash(root: &OwnedRoot, req: &RestoreRequest<'_>) -> bool {
    if !archive_scan::is_archive_dir_name(req.dir_name)
        || restore_io::check_root(root.path()).is_err()
    {
        return false;
    }
    let dir = root.path().join(req.dir_name);
    restore_io::check_tree(&dir).is_ok()
        && std::fs::read(dir.join("manifest.json"))
            .is_ok_and(|bytes| archive::sha256_hex(&bytes) != req.expected_manifest_sha)
        && archive::verify(root, &dir, None, &[]).is_ok()
}

pub async fn restore(m: &Manage<'_>, a: &Actor, id: &str) -> RestoreOutcome {
    if authorize(m, a).is_err() {
        return RestoreOutcome::Denied;
    }
    let Ok(guard) = RESTORE_GATE.try_lock() else {
        return RestoreOutcome::Busy;
    };
    let recorded = match restore_record(m, a, id).await {
        Ok(recorded) => recorded,
        Err(ManageError::Denied) => return RestoreOutcome::Denied,
        Err(ManageError::NotFound) => return RestoreOutcome::NotFound,
        Err(ManageError::BadId) => return RestoreOutcome::BadId,
        Err(ManageError::Db | ManageError::Scan) => return RestoreOutcome::Failed("database"),
    };
    let source = m.root.path().to_owned();
    let stage = m.staging.path().to_owned();
    tokio::task::spawn_blocking(move || {
        // A cancelled waiter must not release the lock while this worker still runs.
        let _guard = guard;
        match restore_copy(&source, &stage, &recorded) {
            Ok(done) => RestoreOutcome::Restored {
                dest_name: done.dest_name,
                files: done.files,
                bytes: done.bytes,
            },
            Err(e) => RestoreOutcome::Failed(restore_kind(e)),
        }
    })
    .await
    .unwrap_or(RestoreOutcome::Failed("worker"))
}

fn restore_copy(
    source: &Path,
    stage: &Path,
    recorded: &ScanEntry,
) -> Result<restore_io::Restored, RestoreError> {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let source = reopen(source)?;
    let stage = reopen(stage)?;
    loop {
        // Checked monotonic counter never repeats in this process. A surviving
        // destination from a previous process is skipped by atomic creation.
        // Read and increment are serialized by RESTORE_GATE.
        let nonce = NONCE.load(Ordering::Relaxed);
        NONCE.store(
            nonce.checked_add(1).ok_or(RestoreError::Exists)?,
            Ordering::Relaxed,
        );
        let req = RestoreRequest {
            dir_name: &recorded.dir_name,
            expected_manifest_sha: &recorded.manifest_sha256,
            expected_ticket: recorded.ticket,
            expected_channel: recorded.channel,
            nonce,
        };
        match restore_io::restore_to_staging(&source, &stage, &req, &RestoreLimits::default()) {
            Err(RestoreError::Exists) => continue,
            Err(RestoreError::Verify) if stale_hash(&source, &req) => {
                return Err(RestoreError::Identity)
            }
            result => return result,
        }
    }
}

fn restore_kind(error: RestoreError) -> &'static str {
    match error {
        RestoreError::BadName => "name",
        RestoreError::Verify => "verify",
        RestoreError::Identity => "identity",
        RestoreError::Exists => "exists",
        RestoreError::TooLarge => "limit",
        RestoreError::Unsafe => "unsafe",
        RestoreError::Io => "io",
    }
}

const DENIAL: &str = "You cannot manage archives.";

fn reply(error: ManageError) -> &'static str {
    match error {
        ManageError::Denied => DENIAL,
        ManageError::NotFound => "Archive not found.",
        ManageError::BadId => "Invalid archive ID.",
        ManageError::Db | ManageError::Scan => "Archive operation failed.",
    }
}

pub fn render_list(result: &Result<Vec<ArchiveSummary>, ManageError>) -> String {
    match result {
        Ok(rows) if rows.is_empty() => "No archives on this page.".into(),
        Ok(rows) => format!("Archives on this page: {}.", rows.len()),
        Err(e) => reply(*e).into(),
    }
}

pub fn render_show(result: &Result<ArchiveSummary, ManageError>) -> String {
    match result {
        Ok(row) => format!(
            "Archive {}: {} messages, {} assets.",
            row.id.as_str(),
            row.messages,
            row.assets
        ),
        Err(e) => reply(*e).into(),
    }
}

pub fn render_health(result: &Result<HealthReport, ManageError>) -> String {
    match result {
        Ok(r) => format!("Archive health: {} verified, {} missing, {} corrupt, {} identity mismatches, {} invalid names.{}",
            r.verified, r.missing, r.corrupt, r.identity_mismatch, r.bad_name,
            if r.has_more { " More archives are available." } else { "" }),
        Err(e) => reply(*e).into(),
    }
}

pub fn render_orphans(result: &Result<OrphanSummary, ManageError>) -> String {
    match result {
        Ok(r) => format!(
            "Orphan scan: {} unrecorded archives, {} staging entries, {} unknown entries.",
            r.unrecorded_archives.count, r.staging_debris.count, r.unknown_entries.count
        ),
        Err(e) => reply(*e).into(),
    }
}

pub fn render_retention(result: &Result<RetentionReport, ManageError>) -> String {
    match result {
        Ok(r) if r.enabled => format!(
            "Retention dry run: {} eligible. Nothing was deleted.",
            r.eligible
        ),
        Ok(_) => "Retention is disabled. Nothing was deleted.".into(),
        Err(e) => reply(*e).into(),
    }
}

pub fn render_restore(result: &RestoreOutcome) -> String {
    match result {
        RestoreOutcome::Restored { files, bytes, .. } => {
            format!("Restored to staging: {files} files, {bytes} bytes.")
        }
        RestoreOutcome::Busy => "Another archive restore is in progress.".into(),
        RestoreOutcome::Denied => DENIAL.into(),
        RestoreOutcome::NotFound => reply(ManageError::NotFound).into(),
        RestoreOutcome::BadId => reply(ManageError::BadId).into(),
        RestoreOutcome::Failed(_) => "Archive restore failed.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        archive::{self, AssetEntry, Downloaded, PublishInput},
        fixtures,
    };
    use super::*;
    use rusqlite::params;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    const GUILD: u64 = 100;
    const STAFF: u64 = 200;
    const ARCHIVE_ID: &str = "arc-00000000000000000000000000000000";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/a20-tests");
            fs::create_dir_all(&parent).unwrap();
            let path = parent.join(format!(
                "{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        repo: Repository,
        root: OwnedRoot,
        staging: OwnedRoot,
        settings: Settings,
        scratch: Scratch,
    }

    impl Fixture {
        async fn new() -> Self {
            let scratch = Scratch::new();
            let db_path = scratch
                .0
                .join("archives.sqlite")
                .to_str()
                .unwrap()
                .to_owned();
            let repo = Repository::open(&db_path).await.unwrap();
            let root = OwnedRoot::open(&scratch.0.join("archive")).unwrap();
            let staging = OwnedRoot::open(&scratch.0.join("staging")).unwrap();
            let settings = Settings {
                guild: GUILD,
                staff: STAFF,
                support_category: 7,
                age_category: 8,
                db_path,
            };
            Self {
                repo,
                root,
                staging,
                settings,
                scratch,
            }
        }

        fn manage(&self) -> Manage<'_> {
            Manage {
                settings: &self.settings,
                repo: &self.repo,
                root: &self.root,
                staging: &self.staging,
                now: &|| i64::MAX,
            }
        }
    }

    fn staff() -> Actor {
        Actor {
            user: 8,
            guild: GUILD,
            roles: vec![STAFF],
            is_guild_owner: false,
            channel: 1,
        }
    }

    async fn assert_denied(m: &Manage<'_>, a: &Actor) {
        assert_eq!(
            list(m, a, 25, 0).await,
            Err(ManageError::Denied),
            "list authorization takes precedence"
        );
        assert_eq!(show(m, a, ARCHIVE_ID).await, Err(ManageError::Denied));
        assert_eq!(show(m, a, "not an ID").await, Err(ManageError::Denied));
        assert_eq!(health(m, a, 0).await, Err(ManageError::Denied));
        assert_eq!(orphans(m, a).await, Err(ManageError::Denied));
        assert_eq!(
            retention_plan(m, a, RetentionPolicy::AfterDays(1)).await,
            Err(ManageError::Denied)
        );
        assert_eq!(
            retention_plan(m, a, RetentionPolicy::Disabled).await,
            Err(ManageError::Denied)
        );
        assert_eq!(restore(m, a, ARCHIVE_ID).await, RestoreOutcome::Denied);
        assert_eq!(restore(m, a, "not an ID").await, RestoreOutcome::Denied);
    }

    #[tokio::test]
    async fn authorization_precedes_all_io() {
        let f = Fixture::new().await;
        let invalid = Settings {
            staff: 0,
            ..f.settings.clone()
        };
        let cases = [
            (
                &f.settings,
                Actor {
                    roles: vec![],
                    ..staff()
                },
            ),
            // Permissions such as Administrator are intentionally not part of Actor.
            (
                &f.settings,
                Actor {
                    roles: vec![999],
                    ..staff()
                },
            ),
            (
                &f.settings,
                Actor {
                    guild: GUILD + 1,
                    is_guild_owner: true,
                    ..staff()
                },
            ),
            (
                &f.settings,
                Actor {
                    user: 0,
                    is_guild_owner: true,
                    ..staff()
                },
            ),
            (&invalid, staff()),
        ];
        for (settings, actor) in &cases {
            assert_denied(
                &Manage {
                    settings,
                    ..f.manage()
                },
                actor,
            )
            .await;
        }

        // Corrupt the persisted SQLite schema while retaining the open repository handle.
        f.repo.db.call(|c| -> rusqlite::Result<()> {
            c.execute_batch("PRAGMA writable_schema=ON; UPDATE sqlite_master SET sql='malformed' WHERE name='ticket_archives'; PRAGMA writable_schema=RESET;")
        }).await.unwrap();
        fs::remove_dir(f.root.path()).unwrap();
        fs::remove_dir(f.staging.path()).unwrap();
        let db_path = f.scratch.0.join("archives.sqlite");
        let corrupt = fs::read(&db_path).unwrap();
        assert!(
            Repository::open(db_path.to_str().unwrap()).await.is_err(),
            "file must actually be corrupt"
        );
        for (settings, actor) in &cases {
            assert_denied(
                &Manage {
                    settings,
                    ..f.manage()
                },
                actor,
            )
            .await;
        }
        assert_eq!(fs::read(db_path).unwrap(), corrupt);
        assert!(!f.root.path().exists() && !f.staging.path().exists());
    }

    const CHANNEL: u64 = 500;
    static RESTORE_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    impl Fixture {
        async fn publish(
            &self,
            guild: u64,
            ticket: i64,
            status: &'static str,
            closed_at: Option<i64>,
        ) -> ArchiveId {
            let channel = CHANNEL + ticket as u64;
            let mut messages: Vec<_> = fixtures::message_pages(1).into_iter().flatten().collect();
            let asset = fixtures::asset_fixtures().remove(0);
            messages[0].attachments.push(asset.attachment.clone());
            let len = asset.bytes.len() as u64;
            let published = archive::publish(
                &self.root,
                &PublishInput {
                    ticket,
                    channel,
                    seq: 1,
                    nonce: 1,
                    messages: &messages,
                    downloaded: &[Downloaded {
                        entry: AssetEntry {
                            attachment_id: asset.attachment.id.get(),
                            message_id: messages[0].id.get(),
                            file: "attachment-100.bin".into(),
                            len,
                            sha256: archive::sha256_hex(&asset.bytes),
                            display_name: asset.attachment.filename,
                            declared_size: len,
                        },
                        bytes: asset.bytes,
                    }],
                },
            )
            .unwrap();
            let id = archive_index::archive_id(guild, ticket, 1, &published.manifest_sha256);
            self.repo.db.call(move |c| -> rusqlite::Result<()> {
                c.execute("INSERT INTO tickets(id,guild,opener,kind,status,channel) VALUES(?1,?2,'8','age',?3,?4)",
                    params![ticket, guild.to_string(), status, channel.to_string()])?;
                if let Some(at) = closed_at {
                    c.execute("INSERT INTO ticket_history(ticket,at,from_status,to_status,actor,note) VALUES(?1,?2,'verified','closed','8','confirmed')", params![ticket, at])?;
                }
                c.execute("INSERT INTO ticket_archives VALUES(?1,1,?2,?3,?4,?5,50)", params![ticket,published.dir_name,published.manifest_sha256,published.messages,published.assets])?;
                Ok(())
            }).await.unwrap();
            id
        }

        async fn extra_rows(&self, ticket: i64, n: usize) {
            self.repo
                .db
                .call(move |c| -> rusqlite::Result<()> {
                    let tx = c.transaction()?;
                    for seq in 2..=n + 1 {
                        tx.execute(
                            "INSERT INTO ticket_archives VALUES(?1,?2,?3,'unused',1,1,50)",
                            params![ticket, seq, format!("ticket-{ticket}-{seq}")],
                        )?;
                    }
                    tx.commit()
                })
                .await
                .unwrap();
        }

        fn dir(&self, ticket: i64) -> PathBuf {
            self.root.path().join(format!("ticket-{ticket}-1"))
        }
    }

    type Tree = Vec<(PathBuf, Option<(u64, String)>)>;

    fn snapshot(root: &Path) -> Tree {
        fn visit(root: &Path, dir: &Path, entries: &mut Tree) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let bytes = if entry.file_type().unwrap().is_dir() {
                    visit(root, &path, entries);
                    None
                } else {
                    let bytes = fs::read(&path).unwrap();
                    Some((bytes.len() as u64, archive::sha256_hex(&bytes)))
                };
                entries.push((path.strip_prefix(root).unwrap().to_owned(), bytes));
            }
        }
        let mut entries = Vec::new();
        visit(root, root, &mut entries);
        entries.sort_unstable();
        entries
    }

    #[tokio::test]
    async fn index_parity_and_max_page() {
        let f = Fixture::new().await;
        let id = f.publish(GUILD, 1, "closed", Some(1)).await;
        f.extra_rows(1, archive_index::MAX_PAGE + 1).await;
        f.publish(GUILD + 1, 2, "closed", Some(1)).await;
        let a = staff();
        let m = f.manage();
        let expected = archive_index::list_archives(m.repo, m.settings, &a, usize::MAX, 1)
            .await
            .unwrap();
        let got = list(&m, &a, usize::MAX, 1).await.unwrap();
        assert_eq!(
            got.len(),
            archive_index::MAX_PAGE,
            "list keeps the index page cap"
        );
        assert_eq!(got, expected);
        let owner = Actor {
            roles: vec![],
            is_guild_owner: true,
            ..a
        };
        let summary = show(&m, &owner, id.as_str()).await.unwrap();
        assert_eq!(
            summary,
            archive_index::find_archive(m.repo, m.settings, &owner, id.as_str())
                .await
                .unwrap()
        );
        assert_eq!(summary.kind, "age");
    }

    #[tokio::test]
    async fn health_uses_database_identity_and_pages() {
        let f = Fixture::new().await;
        let good = f.publish(GUILD, 4, "closed", Some(1)).await;
        let tampered = f.publish(GUILD, 3, "closed", Some(1)).await;
        let missing = f.publish(GUILD, 2, "closed", Some(1)).await;
        let mismatch = f.publish(GUILD, 1, "closed", Some(1)).await;
        f.extra_rows(1, HEALTH_PAGE).await;
        let bad_name = archive_index::archive_id(GUILD, 1, 2, "unused");
        f.repo
            .db
            .call(|c| -> rusqlite::Result<()> {
                c.execute(
                    "UPDATE ticket_archives SET messages=9 WHERE ticket=1 AND seq=1",
                    [],
                )?;
                c.execute(
                    "UPDATE ticket_archives SET dir_name='../escape' WHERE ticket=1 AND seq=2",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let m = f.manage();
        let a = staff();
        assert_ne!(good.as_str(), "ticket-4-1");
        let first = health(&m, &a, 0).await.unwrap();
        assert_eq!(first.verified, 3, "health must verify real archived bytes");
        assert_eq!(first.missing, HEALTH_PAGE - 3);
        assert!(first.has_more);
        fs::remove_file(f.dir(3).join("assets/attachment-100.bin")).unwrap();
        fs::remove_dir_all(f.dir(2)).unwrap();
        let first = health(&m, &a, 0).await.unwrap();
        assert_eq!(
            (first.verified, first.corrupt, first.missing),
            (1, 1, HEALTH_PAGE - 2)
        );
        assert_eq!(&first.non_verified[..2], &[tampered, missing]);
        let tail = health(&m, &a, HEALTH_PAGE).await.unwrap();
        assert_eq!(
            (tail.missing, tail.bad_name, tail.identity_mismatch),
            (2, 1, 1)
        );
        assert_eq!(&tail.non_verified[2..], &[bad_name, mismatch]);
        assert!(!tail.has_more);
        assert_eq!(
            health(&m, &a, usize::MAX).await.unwrap(),
            HealthReport::default()
        );
    }

    #[tokio::test]
    async fn orphans_use_all_records_and_cap_each_class() {
        let f = Fixture::new().await;
        f.publish(GUILD, 1, "closed", None).await;
        f.extra_rows(1, HEALTH_PAGE).await;
        for seq in 2..=HEALTH_PAGE + 1 {
            fs::create_dir(f.root.path().join(format!("ticket-1-{seq}"))).unwrap();
        }
        for name in ["ticket-9-9", ".staging-x", ".trash-1-1-1"] {
            fs::create_dir(f.root.path().join(name)).unwrap();
        }
        for n in 0..12 {
            fs::write(f.root.path().join(format!("unknown-{n:02}")), b"").unwrap();
        }
        let report = orphans(&f.manage(), &staff()).await.unwrap();
        assert_eq!(
            report.unrecorded_archives.names,
            ["ticket-9-9"],
            "orphans classify only unrecorded archive names"
        );
        assert_eq!(report.unrecorded_archives.count, 1);
        assert_eq!(report.staging_debris.names, [".staging-x", ".trash-1-1-1"]);
        assert_eq!(report.staging_debris.count, 2);
        assert_eq!(report.unknown_entries.count, 12);
        assert_eq!(
            report.unknown_entries.names,
            (0..10)
                .map(|n| format!("unknown-{n:02}"))
                .collect::<Vec<_>>()
        );
        assert!(report.truncated);
    }

    #[tokio::test]
    async fn retention_requires_confirmed_old_closure_and_never_mutates() {
        let f = Fixture::new().await;
        let oldest = f.publish(GUILD, 1, "closed", Some(10)).await;
        f.publish(GUILD, 2, "closed", Some(99_999)).await;
        f.publish(GUILD, 3, "closed", None).await;
        f.publish(GUILD, 4, "closed", Some(i64::MAX - 1)).await;
        f.publish(GUILD, 5, "active", Some(10)).await;
        f.publish(GUILD + 1, 6, "closed", Some(10)).await;
        f.extra_rows(1, 12).await;
        let tree = snapshot(f.root.path());
        let db = archive::sha256_hex(&fs::read(&f.settings.db_path).unwrap());
        let a = staff();
        let m = Manage {
            now: &|| 100_000,
            ..f.manage()
        };
        assert_eq!(
            retention_plan(&m, &a, RetentionPolicy::Disabled)
                .await
                .unwrap(),
            RetentionReport::default()
        );
        let policy = RetentionPolicy::AfterDays(1);
        let plan = retention_plan(&m, &a, policy).await.unwrap();
        assert!(plan.enabled, "configured dry run must be enabled");
        assert_eq!(plan.eligible, 13);
        assert_eq!(plan.sample.len(), 10);
        assert_eq!(plan.sample[0], oldest);
        assert_eq!(plan, retention_plan(&m, &a, policy).await.unwrap());
        assert_eq!(
            retention_plan(&f.manage(), &a, policy)
                .await
                .unwrap()
                .eligible,
            14
        );
        assert_eq!(snapshot(f.root.path()), tree);
        assert_eq!(
            archive::sha256_hex(&fs::read(&f.settings.db_path).unwrap()),
            db
        );
        assert!(f.dir(1).is_dir());
        assert!(snapshot(f.staging.path()).is_empty());
    }

    #[tokio::test]
    async fn restore_copies_exact_bytes_and_rejects_invalid_records() {
        let _test_gate = RESTORE_TESTS.lock().await;
        let f = Fixture::new().await;
        let id = f.publish(GUILD, 1, "closed", Some(1)).await;
        let foreign = f.publish(GUILD + 1, 2, "closed", Some(1)).await;
        let stale = f.publish(GUILD, 3, "closed", Some(1)).await;
        let source = f.dir(1);
        let want = snapshot(&source);
        let all = snapshot(f.root.path());
        let a = staff();
        let m = f.manage();
        let outcome = restore(&m, &a, id.as_str()).await;
        let RestoreOutcome::Restored {
            dest_name,
            files,
            bytes,
        } = outcome
        else {
            panic!("authorized restore must produce a staging copy: {outcome:?}");
        };
        assert_eq!(files, 3);
        assert_eq!(
            bytes,
            want.iter()
                .filter_map(|(_, f)| f.as_ref().map(|(len, _)| *len))
                .sum::<u64>()
        );
        assert_eq!(snapshot(&f.staging.path().join(dest_name)), want);
        assert_eq!(snapshot(f.root.path()), all);
        let staging = snapshot(f.staging.path());
        let html = source.join("transcript.html");
        let mut payload = fs::read(&html).unwrap();
        payload[0] ^= 1;
        fs::write(&html, payload).unwrap();
        assert_eq!(
            restore(&m, &a, id.as_str()).await,
            RestoreOutcome::Failed("verify")
        );
        assert_eq!(
            restore(&m, &a, foreign.as_str()).await,
            RestoreOutcome::NotFound
        );
        assert_eq!(restore(&m, &a, ARCHIVE_ID).await, RestoreOutcome::NotFound);
        assert_eq!(restore(&m, &a, "ticket-1-1").await, RestoreOutcome::BadId);
        let wrong_hash = "0".repeat(64);
        let stale_id = archive_index::archive_id(GUILD, 3, 1, &wrong_hash);
        f.repo
            .db
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "UPDATE ticket_archives SET manifest_sha256=?1 WHERE ticket=3",
                    [wrong_hash],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            restore(&m, &a, stale.as_str()).await,
            RestoreOutcome::NotFound
        );
        assert_eq!(
            restore(&m, &a, stale_id.as_str()).await,
            RestoreOutcome::Failed("identity")
        );
        assert_eq!(snapshot(f.staging.path()), staging);
    }

    #[tokio::test]
    async fn restore_is_process_wide_nonqueuing_and_releases_gate() {
        let _test_gate = RESTORE_TESTS.lock().await;
        let f = Fixture::new().await;
        let id = f.publish(GUILD, 1, "closed", None).await;
        let a = staff();
        let m = f.manage();
        // Park the repository worker so the first branch yields after acquiring the gate.
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let db = f.repo.db.clone();
        let parked = tokio::spawn(async move {
            db.call(move |_| -> rusqlite::Result<()> {
                entered_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(std::time::Duration::from_secs(3));
                Ok(())
            })
            .await
            .unwrap();
        });
        entered_rx.await.unwrap();
        let (first, second) = tokio::join!(biased;
            restore(&m, &a, id.as_str()),
            async { let result = restore(&m, &a, id.as_str()).await; let _ = release_tx.send(()); result },
        );
        parked.await.unwrap();
        assert_eq!(
            second,
            RestoreOutcome::Busy,
            "overlapping restore must return Busy"
        );
        let RestoreOutcome::Restored {
            dest_name: first, ..
        } = first
        else {
            panic!("first restore must succeed");
        };
        let third = restore(&m, &a, id.as_str()).await;
        let RestoreOutcome::Restored {
            dest_name: third, ..
        } = third
        else {
            panic!("third restore must succeed");
        };
        assert_ne!(first, third);
    }

    #[tokio::test]
    async fn renderers_emit_only_sanitized_text() {
        const DENIED: &str = "You cannot manage archives.";
        const RAW: &str = r"<@123> C:\secret\path SQLITE_CORRUPT";
        let denial = [
            render_list(&Err(ManageError::Denied)),
            render_show(&Err(ManageError::Denied)),
            render_health(&Err(ManageError::Denied)),
            render_orphans(&Err(ManageError::Denied)),
            render_retention(&Err(ManageError::Denied)),
            render_restore(&RestoreOutcome::Denied),
        ];
        assert!(
            denial.iter().all(|text| text == DENIED),
            "every denial must use the same fixed reply"
        );
        let f = Fixture::new().await;
        let id = f.publish(GUILD, 1, "closed", None).await;
        let m = f.manage();
        let a = staff();
        let mut replies = vec![
            render_list(&list(&m, &a, 5, 0).await),
            render_show(&show(&m, &a, id.as_str()).await),
            render_health(&health(&m, &a, 0).await),
            render_retention(&retention_plan(&m, &a, RetentionPolicy::Disabled).await),
            render_orphans(&Ok(OrphanSummary {
                unknown_entries: OrphanClass {
                    count: 1,
                    names: vec![RAW.into()],
                },
                ..OrphanSummary::default()
            })),
            render_restore(&RestoreOutcome::Restored {
                dest_name: RAW.into(),
                files: 3,
                bytes: 12,
            }),
        ];
        for kind in [
            ManageError::Db,
            ManageError::Scan,
            ManageError::NotFound,
            ManageError::BadId,
        ] {
            replies.push(render_show(&Err(kind)));
        }
        for outcome in [
            RestoreOutcome::Busy,
            RestoreOutcome::NotFound,
            RestoreOutcome::BadId,
            RestoreOutcome::Failed(RAW),
        ] {
            replies.push(render_restore(&outcome));
        }
        for text in replies {
            assert!(!text.is_empty());
            for forbidden in ["@", "<", "\\", "C:", "SQLITE_CORRUPT"] {
                assert!(!text.contains(forbidden), "unsafe reply: {text}");
            }
        }
    }
}

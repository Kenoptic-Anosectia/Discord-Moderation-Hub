//! A17: opaque archive ids, authorized listing and lookup. Owned by exactly one A08 slice.
use super::archive::sha256_hex;
use super::closer::Actor;
use super::policy::Settings;
use super::repository::Repository;

/// Opaque archive id: `arc-` + 32 lowercase hex characters. Not a path, not a capability.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ArchiveId(String);

impl ArchiveId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Strict parse: exact prefix and exactly 32 characters of `[0-9a-f]`.
    pub fn parse(s: &str) -> Option<ArchiveId> {
        let hex = s.strip_prefix("arc-")?;
        if hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Some(ArchiveId(s.to_string()))
        } else {
            None
        }
    }
}

/// id = "arc-" + first 32 hex chars of the versioned, NUL-separated digest.
pub fn archive_id(guild: u64, ticket: i64, seq: i64, manifest_sha256: &str) -> ArchiveId {
    let digest = sha256_hex(
        format!("skynet-archive-id-v1\0{guild}\0{ticket}\0{seq}\0{manifest_sha256}").as_bytes(),
    );
    ArchiveId(format!("arc-{}", &digest[..32]))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveSummary {
    pub id: ArchiveId,
    pub ticket: i64,
    pub seq: i64,
    pub kind: String,
    pub ticket_status: String,
    pub messages: u64,
    pub assets: u64,
    pub verified_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManageError {
    Denied,
    NotFound,
    BadId,
    Db,
}

pub const MAX_PAGE: usize = 25;

pub(super) fn authorized(settings: &Settings, actor: &Actor) -> bool {
    settings.valid()
        && actor.guild == settings.guild
        && actor.user != 0
        && (actor.is_guild_owner || actor.roles.contains(&settings.staff))
}

/// Raw recorded row: (ticket, seq, kind, status, messages, assets, verified_at, manifest sha).
type Row = (i64, i64, String, String, i64, i64, i64, String);

fn summarize(guild: u64, r: Row) -> Result<ArchiveSummary, ManageError> {
    // Counts that cannot be represented are corrupt records: fail rather than invent values.
    let messages = u64::try_from(r.4).map_err(|_| ManageError::Db)?;
    let assets = u64::try_from(r.5).map_err(|_| ManageError::Db)?;
    Ok(ArchiveSummary {
        id: archive_id(guild, r.0, r.1, &r.7),
        ticket: r.0,
        seq: r.1,
        kind: r.2,
        ticket_status: r.3,
        messages,
        assets,
        verified_at: r.6,
    })
}

/// Guild rows ordered ticket DESC, seq DESC. A `limit` of -1 means unbounded (SQLite).
async fn rows(
    repo: &Repository,
    guild: u64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Row>, ManageError> {
    repo.db
        .call(move |c| -> rusqlite::Result<Vec<Row>> {
            let mut st = c.prepare(
                "SELECT a.ticket,a.seq,t.kind,t.status,a.messages,a.assets,a.verified_at,a.manifest_sha256 \
                 FROM ticket_archives a JOIN tickets t ON t.id=a.ticket \
                 WHERE t.guild=?1 ORDER BY a.ticket DESC, a.seq DESC LIMIT ?2 OFFSET ?3",
            )?;
            let it = st.query_map(rusqlite::params![guild.to_string(), limit, offset], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })?;
            it.collect()
        })
        .await
        .map_err(|_| ManageError::Db)
}

/// Authorization precedes any database access.
pub async fn list_archives(
    repo: &Repository,
    settings: &Settings,
    actor: &Actor,
    limit: usize,
    offset: usize,
) -> Result<Vec<ArchiveSummary>, ManageError> {
    if !authorized(settings, actor) {
        return Err(ManageError::Denied);
    }
    let limit = limit.min(MAX_PAGE);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    let r = rows(repo, settings.guild, limit as i64, offset).await?;
    r.into_iter()
        .map(|r| summarize(settings.guild, r))
        .collect()
}

/// Authorization first; the id is only ever compared with computed ids, never used as a path or SQL.
pub async fn find_archive(
    repo: &Repository,
    settings: &Settings,
    actor: &Actor,
    id: &str,
) -> Result<ArchiveSummary, ManageError> {
    if !authorized(settings, actor) {
        return Err(ManageError::Denied);
    }
    let wanted = ArchiveId::parse(id).ok_or(ManageError::BadId)?;
    let all = rows(repo, settings.guild, -1, 0).await?;
    let all = all
        .into_iter()
        .map(|r| summarize(settings.guild, r))
        .collect::<Result<Vec<_>, _>>()?;
    let mut hits = all.into_iter().filter(|s| s.id == wanted);
    match (hits.next(), hits.next()) {
        (Some(s), None) => Ok(s),
        _ => Err(ManageError::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> String {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/a08-tests")
            .join(format!(
                "{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("t.sqlite").to_str().unwrap().to_string()
    }

    const G: u64 = 100;
    const STAFF: u64 = 200;

    fn settings() -> Settings {
        Settings {
            guild: G,
            staff: STAFF,
            support_category: 7,
            age_category: 8,
            db_path: "x".into(),
        }
    }
    fn actor(user: u64, guild: u64, owner: bool, roles: &[u64]) -> Actor {
        Actor {
            user,
            guild,
            is_guild_owner: owner,
            roles: roles.to_vec(),
            channel: 1,
        }
    }
    fn staff() -> Actor {
        actor(5, G, false, &[STAFF])
    }
    async fn repo() -> Repository {
        Repository::open(&temp_db("a17")).await.unwrap()
    }
    async fn add(r: &Repository, guild: u64, id: i64, kind: &str, seqs: &[i64]) {
        let kind = kind.to_string();
        let seqs = seqs.to_vec();
        r.db.call(move |c| -> rusqlite::Result<()> {
            c.execute(
                "INSERT INTO tickets(id,guild,opener,kind,status) VALUES(?1,?2,?3,?4,'closed')",
                rusqlite::params![id, guild.to_string(), (1000 + id).to_string(), kind],
            )?;
            for s in seqs {
                c.execute(
                    "INSERT INTO ticket_archives(ticket,seq,dir_name,manifest_sha256,messages,assets,verified_at) VALUES(?1,?2,?3,?4,3,1,50)",
                    rusqlite::params![id, s, format!("ticket-{id}-{s}"), format!("sha{id}-{s}")],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    }
    async fn broken() -> Repository {
        let r = repo().await;
        r.db.call(|c| -> rusqlite::Result<()> { c.execute_batch("DROP TABLE ticket_archives;") })
            .await
            .unwrap();
        r
    }

    #[tokio::test]
    async fn denial_precedes_database() {
        let r = broken().await;
        let s = settings();
        let mut bad = settings();
        bad.staff = 0;
        let cases: Vec<(Settings, Actor)> = vec![
            (s.clone(), actor(5, G, false, &[])),
            (s.clone(), actor(5, G, false, &[999, 998])),
            (s.clone(), actor(5, G + 1, true, &[STAFF])),
            (s.clone(), actor(0, G, true, &[STAFF])),
            (bad, staff()),
        ];
        let well_formed = "arc-00000000000000000000000000000000";
        for (st, a) in &cases {
            assert_eq!(
                list_archives(&r, st, a, 10, 0).await,
                Err(ManageError::Denied)
            );
            assert_eq!(
                find_archive(&r, st, a, well_formed).await,
                Err(ManageError::Denied)
            );
            assert_eq!(
                find_archive(&r, st, a, "../x").await,
                Err(ManageError::Denied)
            );
        }
        // Authorized callers do reach the (broken) database.
        assert_eq!(
            list_archives(&r, &s, &staff(), 10, 0).await,
            Err(ManageError::Db)
        );
        assert_eq!(
            list_archives(&r, &s, &actor(5, G, true, &[]), 10, 0).await,
            Err(ManageError::Db)
        );
        assert_eq!(
            find_archive(&r, &s, &staff(), well_formed).await,
            Err(ManageError::Db)
        );
    }

    #[tokio::test]
    async fn empty_database_both_kinds_and_guild_isolation() {
        let r = repo().await;
        assert!(list_archives(&r, &settings(), &staff(), 10, 0)
            .await
            .unwrap()
            .is_empty());
        add(&r, G, 1, "support", &[1]).await;
        add(&r, G, 2, "age", &[1]).await;
        add(&r, G + 1, 3, "support", &[1]).await;
        let owner = actor(9, G, true, &[]);
        let l = list_archives(&r, &settings(), &owner, 10, 0).await.unwrap();
        assert_eq!(
            l.iter()
                .map(|s| (s.ticket, s.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![(2, "age"), (1, "support")]
        );
        assert_eq!((l[0].messages, l[0].assets, l[0].verified_at), (3, 1, 50));
        assert_eq!(l[0].ticket_status, "closed");
        assert_eq!(l[0].id, archive_id(G, 2, 1, "sha2-1"));
    }

    #[tokio::test]
    async fn ordering_and_paging() {
        let r = repo().await;
        for t in 1..=14 {
            let kind = if t % 2 == 0 { "age" } else { "support" };
            add(&r, G, t, kind, &[1, 2]).await;
        }
        let (s, a) = (settings(), staff());
        let all = list_archives(&r, &s, &a, 1000, 0).await.unwrap();
        assert_eq!(all.len(), MAX_PAGE);
        assert_eq!((all[0].ticket, all[0].seq), (14, 2));
        assert_eq!((all[1].ticket, all[1].seq), (14, 1));
        assert_eq!((all[24].ticket, all[24].seq), (2, 2));
        assert!(list_archives(&r, &s, &a, 0, 0).await.unwrap().is_empty());
        let p = list_archives(&r, &s, &a, 3, 26).await.unwrap();
        assert_eq!(
            p.iter().map(|x| (x.ticket, x.seq)).collect::<Vec<_>>(),
            vec![(1, 2), (1, 1)]
        );
        assert!(list_archives(&r, &s, &a, 5, 28).await.unwrap().is_empty());
        assert!(list_archives(&r, &s, &a, 5, usize::MAX)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ids_deterministic_distinct_and_strict() {
        let a = archive_id(1, 2, 3, "m");
        assert_eq!(a, archive_id(1, 2, 3, "m"));
        assert_ne!(a, archive_id(1, 2, 4, "m"));
        assert_ne!(a, archive_id(1, 2, 3, "n"));
        assert_ne!(a, archive_id(2, 2, 3, "m"));
        assert_ne!(a, archive_id(1, 3, 3, "m"));
        assert_eq!(a.as_str().len(), 36);
        assert_eq!(ArchiveId::parse(a.as_str()), Some(a));
        let h = "0123456789abcdef0123456789abcdef";
        assert!(ArchiveId::parse(&format!("arc-{h}")).is_some());
        for bad in [
            format!("arx-{h}"),
            format!("ARC-{h}"),
            format!("arc-{}", &h[..31]),
            format!("arc-{h}0"),
            format!("arc-{}", h.to_uppercase()),
            format!("arc-{}g", &h[..31]),
            format!("arc-{}", "é".repeat(16)),
            "../x".to_string(),
            format!("arc-../../{}", &h[..23]),
            format!("arc-{h}/"),
            format!(" arc-{h}"),
            String::new(),
            h.to_string(),
        ] {
            assert!(ArchiveId::parse(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn id_fixed_vector() {
        // sha256 of "skynet-archive-id-v1\0100\07\02\0abc123", computed independently with sha256sum.
        assert_eq!(
            archive_id(100, 7, 2, "abc123").as_str(),
            "arc-f2904db292557ed5ae741f8bf775a5c3"
        );
    }

    #[tokio::test]
    async fn negative_counts_are_db_errors_and_nonzero_control_passes() {
        let r = repo().await;
        add(&r, G, 1, "support", &[1]).await;
        let (s, a) = (settings(), staff());
        let ok = list_archives(&r, &s, &a, 10, 0).await.unwrap();
        assert_eq!((ok[0].messages, ok[0].assets), (3, 1));
        let id = archive_id(G, 1, 1, "sha1-1");
        let found = find_archive(&r, &s, &a, id.as_str()).await.unwrap();
        assert_eq!(found.messages, 3);
        for col in ["messages", "assets"] {
            r.db.call(move |c| -> rusqlite::Result<()> {
                c.execute_batch(&format!("UPDATE ticket_archives SET {col}=-1;"))
            })
            .await
            .unwrap();
            assert_eq!(list_archives(&r, &s, &a, 10, 0).await, Err(ManageError::Db));
            assert_eq!(
                find_archive(&r, &s, &a, id.as_str()).await,
                Err(ManageError::Db)
            );
            r.db.call(move |c| -> rusqlite::Result<()> {
                c.execute_batch(&format!("UPDATE ticket_archives SET {col}=3;"))
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn find_archive_cases() {
        let r = repo().await;
        add(&r, G, 1, "support", &[1, 2]).await;
        add(&r, G + 1, 2, "age", &[1]).await;
        let (s, a) = (settings(), staff());
        let real = archive_id(G, 1, 2, "sha1-2");
        let got = find_archive(&r, &s, &a, real.as_str()).await.unwrap();
        assert_eq!((got.ticket, got.seq, got.kind.as_str()), (1, 2, "support"));
        for bad in ["garbage", "../x", ""] {
            assert_eq!(find_archive(&r, &s, &a, bad).await, Err(ManageError::BadId));
        }
        let missing = archive_id(G, 99, 1, "nope");
        assert_eq!(
            find_archive(&r, &s, &a, missing.as_str()).await,
            Err(ManageError::NotFound)
        );
        // Another guild's real archive is not visible, whichever guild the id was derived for.
        for id in [
            archive_id(G + 1, 2, 1, "sha2-1"),
            archive_id(G, 2, 1, "sha2-1"),
            archive_id(G + 1, 1, 2, "sha1-2"),
        ] {
            assert_eq!(
                find_archive(&r, &s, &a, id.as_str()).await,
                Err(ManageError::NotFound)
            );
        }
        // Mutated id.
        let mut m = real.as_str().to_string();
        let last = if m.ends_with('0') { '1' } else { '0' };
        m.pop();
        m.push(last);
        assert_eq!(
            find_archive(&r, &s, &a, &m).await,
            Err(ManageError::NotFound)
        );
    }
}

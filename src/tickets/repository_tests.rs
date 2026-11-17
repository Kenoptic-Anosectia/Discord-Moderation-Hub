use super::*;
use crate::tickets::repository_close::Step;

pub fn temp_db(tag: &str) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/a07-owned-tests");
    std::fs::create_dir_all(&root).unwrap();
    root.join(format!(
        "{tag}-{}-{}.sqlite",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
    .to_str()
    .unwrap()
    .to_string()
}

const V1: &str = "CREATE TABLE ticket_schema(version INTEGER NOT NULL); INSERT INTO ticket_schema VALUES(1);
 CREATE TABLE ticket_panels(guild TEXT NOT NULL, channel TEXT NOT NULL, message TEXT PRIMARY KEY, bot TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1);
 CREATE TABLE tickets(id INTEGER PRIMARY KEY AUTOINCREMENT, operation TEXT NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(16)))), guild TEXT NOT NULL, opener TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('support','age')), status TEXT NOT NULL CHECK(status IN ('reserved','creating','active','failed','closed')), channel TEXT UNIQUE, greeting TEXT NOT NULL DEFAULT 'pending');
 CREATE UNIQUE INDEX ticket_active_slot ON tickets(guild,opener,kind) WHERE status IN ('reserved','creating','active');
 CREATE TABLE ticket_interactions(interaction TEXT PRIMARY KEY, ticket INTEGER NOT NULL REFERENCES tickets(id));
 INSERT INTO ticket_panels VALUES('100','5','6','7',1);
 INSERT INTO tickets(id,operation,guild,opener,kind,status,channel,greeting) VALUES(3,'opa','100','11','support','active','900','sent'),(8,'opb','100','12','age','failed',NULL,'pending'),(9,'opc','100','13','support','creating',NULL,'pending');
 INSERT INTO ticket_interactions VALUES('1',3),('2',9);";

#[tokio::test]
async fn v1_database_migrates_preserving_rows_links_sequence_and_slots() {
    let path = temp_db("v1");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(V1).unwrap();
    }
    let r = Repository::open(&path).await.unwrap();
    assert_eq!(r.schema_version().await.unwrap(), 2);
    let t = r.ticket_by_id(3).await.unwrap().unwrap();
    assert_eq!(
        (
            t.operation.as_str(),
            t.status.as_str(),
            t.channel,
            t.greeting.as_str()
        ),
        ("opa", "active", Some(900), "sent")
    );
    assert!(r
        .trusted_panel(Panel {
            guild: 100,
            channel: 5,
            message: 6,
            bot: 7
        })
        .await
        .unwrap());
    // interaction linkage preserved: replaying interaction 2 returns ticket 9
    assert_eq!(r.reserve(2, 100, 13, Kind::Support).await.unwrap().id, 9);
    // AUTOINCREMENT sequence preserved: next id is above the v1 maximum
    assert!(r.reserve(50, 100, 99, Kind::Support).await.unwrap().id > 9);
    // the creating (uncertain) slot is still held after migration
    assert_eq!(r.reserve(51, 100, 13, Kind::Support).await.unwrap().id, 9);
    drop(r);
    let again = Repository::open(&path).await.unwrap();
    assert_eq!(again.schema_version().await.unwrap(), 2);
    assert_eq!(
        again.ticket_by_id(8).await.unwrap().unwrap().status,
        "failed"
    );
}

#[tokio::test]
async fn migration_sequence_never_reuses_ids_even_above_surviving_rows() {
    let path = temp_db("seq");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(V1).unwrap();
        // a high-water mark above every surviving row (deleted ticket 40 once existed)
        c.execute("UPDATE sqlite_sequence SET seq=40 WHERE name='tickets'", [])
            .unwrap();
    }
    let r = Repository::open(&path).await.unwrap();
    assert!(r.reserve(60, 100, 77, Kind::Age).await.unwrap().id > 40);
}

#[tokio::test]
async fn slot_index_covers_every_nonterminal_state_and_releases_only_terminal() {
    for (state, held) in [
        ("reserved", true),
        ("creating", true),
        ("active", true),
        ("closing", true),
        ("archiving", true),
        ("verified", true),
        ("delete_requested", true),
        ("delete_uncertain", true),
        ("recovery", true),
        ("failed", false),
        ("closed", false),
        ("released", false),
    ] {
        let r = Repository::open(&temp_db("slot")).await.unwrap();
        let t = r.reserve(1, 100, 5, Kind::Support).await.unwrap();
        r.force_status_for_test(t.id, state).await.unwrap();
        let again = r.reserve(2, 100, 5, Kind::Support).await.unwrap();
        assert_eq!(again.id == t.id, held, "{state}");
        assert_ne!(r.reserve(3, 100, 5, Kind::Age).await.unwrap().id, t.id);
    }
}

#[tokio::test]
async fn close_claim_is_compare_and_set_across_independent_connections() {
    let path = temp_db("cas");
    let a = Repository::open(&path).await.unwrap();
    let b = Repository::open(&path).await.unwrap();
    let t = a.reserve(1, 100, 5, Kind::Support).await.unwrap();
    a.claim(t.id).await.unwrap();
    a.attach(t.id, 900).await.unwrap();
    let (x, y) = tokio::join!(
        a.claim_close(t.id, 1, "tok-a", 1000, 2000),
        b.claim_close(t.id, 2, "tok-b", 1000, 2000)
    );
    assert_ne!(x.unwrap(), y.unwrap(), "exactly one worker wins");
    assert_eq!(
        a.ticket_by_id(t.id).await.unwrap().unwrap().status,
        "closing"
    );
    assert!(!a
        .advance(
            t.id,
            "wrong",
            Step {
                from: "closing",
                to: "archiving",
                note: "x",
            },
            1001,
            3000,
        )
        .await
        .unwrap());
    assert!(
        !a.resume_close(t.id, 3, "tok-c", 1500, 3000).await.unwrap(),
        "live lease blocks resume"
    );
    let (p, q) = tokio::join!(
        a.resume_close(t.id, 3, "tok-c", 2001, 4000),
        b.resume_close(t.id, 4, "tok-d", 2001, 4000)
    );
    assert_ne!(p.unwrap(), q.unwrap(), "exactly one resumer");
    assert!(a
        .history(t.id)
        .await
        .unwrap()
        .iter()
        .any(|e| e.to == "closing"));
}

#[tokio::test]
async fn recovery_release_and_attach_are_audited_and_only_from_unverified_states() {
    let r = Repository::open(":memory:").await.unwrap();
    let t = r.reserve(1, 100, 5, Kind::Support).await.unwrap();
    assert!(
        !r.recovery_release(t.id, 9, "why", 10).await.unwrap(),
        "reserved is not recoverable"
    );
    r.claim(t.id).await.unwrap();
    assert!(r
        .recovery_attach(t.id, 777, 9, "operator verified", 11)
        .await
        .unwrap());
    let cur = r.ticket_by_id(t.id).await.unwrap().unwrap();
    assert_eq!((cur.status.as_str(), cur.channel), ("active", Some(777)));
    let audit = r.audit(t.id).await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(
        (
            audit[0].actor,
            audit[0].action.as_str(),
            audit[0].reason.as_str()
        ),
        (9, "attach", "operator verified")
    );
    let u = r.reserve(2, 100, 6, Kind::Support).await.unwrap();
    r.claim(u.id).await.unwrap();
    assert!(r
        .recovery_release(u.id, 9, "confirmed absent", 12)
        .await
        .unwrap());
    assert_eq!(
        r.ticket_by_id(u.id).await.unwrap().unwrap().status,
        "released"
    );
    assert!(r.reserve(3, 100, 6, Kind::Support).await.unwrap().id > u.id);
}

#[tokio::test]
async fn channel_linkage_lookups_are_private_forever_for_closed_tickets() {
    let r = Repository::open(":memory:").await.unwrap();
    let t = r.reserve(1, 100, 5, Kind::Support).await.unwrap();
    r.claim(t.id).await.unwrap();
    r.attach(t.id, 901).await.unwrap();
    assert!(r.channel_known(901).await.unwrap());
    assert!(!r.channel_known(902).await.unwrap());
    r.force_status_for_test(t.id, "closed").await.unwrap();
    assert!(
        r.channel_known(901).await.unwrap(),
        "deleted/archived ids stay private"
    );
    assert!(r.ticket_by_channel(100, 901).await.unwrap().is_some());
    assert!(r.ticket_by_channel(101, 901).await.unwrap().is_none());
}

use super::*;
use crate::tickets::policy::Kind;
use serenity::all::Message;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

#[path = "../../tests/support/ticket_archive_fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

const G: u64 = 100;
const STAFF: u64 = 200;
const CH: u64 = 900;
const OPENER: u64 = 55;

fn settings() -> Settings {
    Settings { guild: G, staff: STAFF, support_category: 300, age_category: 400, db_path: "owned-test.sqlite".into() }
}
struct Fake {
    msgs: Vec<Message>,
    history_fail: bool,
    delete: Mutex<Vec<DeleteOutcome>>, // queue; last repeats
    probe: Mutex<Presence>,
    dm_ok: bool,
    log: Mutex<Vec<String>>,
    root: PathBuf,
    cands: Vec<RemoteChannel>,
    pages_done: Mutex<usize>,
    archive_existed_at_delete: Mutex<Vec<bool>>,
}
impl Fake {
    fn new(root: &std::path::Path, n: usize) -> Self {
        Self {
            msgs: fixtures::message_pages(n).into_iter().flatten().collect(),
            history_fail: false,
            delete: Mutex::new(vec![DeleteOutcome::Deleted]),
            probe: Mutex::new(Presence::Present),
            dm_ok: true,
            log: Mutex::new(vec![]),
            root: root.to_path_buf(),
            cands: vec![],
            pages_done: Mutex::new(0),
            archive_existed_at_delete: Mutex::new(vec![]),
        }
    }
    fn calls(&self, name: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|l| l.starts_with(name)).count()
    }
}
impl HistorySource for Fake {
    async fn page_after(&self, after: u64) -> Result<Vec<Message>, ArchiveError> {
        self.log.lock().unwrap().push("history".into());
        if self.history_fail {
            return Err(ArchiveError::Source);
        }
        Ok(self.msgs.iter().filter(|m| m.id.get() > after).take(100).cloned().collect())
    }
}
impl Downloader for Fake {
    async fn fetch(&self, _u: &url::Url, _l: u64) -> Result<Vec<u8>, ArchiveError> {
        Err(ArchiveError::Download)
    }
}
impl CloseIo for Fake {
    async fn freeze(&self, _c: u64, _r: u64) -> Result<(), ()> {
        self.log.lock().unwrap().push("freeze".into());
        Ok(())
    }
    async fn probe(&self, _c: u64) -> Presence {
        self.log.lock().unwrap().push("probe".into());
        *self.probe.lock().unwrap()
    }
    async fn delete(&self, _c: u64) -> DeleteOutcome {
        self.log.lock().unwrap().push("delete".into());
        let any = std::fs::read_dir(&self.root).map(|d| d.filter_map(|e| e.ok()).any(|e| !e.file_name().to_string_lossy().starts_with("."))).unwrap_or(false);
        self.archive_existed_at_delete.lock().unwrap().push(any);
        let mut q = self.delete.lock().unwrap();
        if q.len() > 1 { q.remove(0) } else { q[0] }
    }
    async fn dm_closed(&self, _r: u64) -> Result<(), ()> {
        self.log.lock().unwrap().push("dm".into());
        if self.dm_ok { Ok(()) } else { Err(()) }
    }
    async fn candidates(&self, _t: &Ticket) -> Result<Vec<RemoteChannel>, ()> {
        Ok(self.cands.clone())
    }
}

struct H {
    repo: Repository,
    root: OwnedRoot,
    dir: PathBuf,
    clock: AtomicI64,
}
async fn harness(tag: &str) -> H {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/a07-owned-tests").join(format!(
        "close-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let root = OwnedRoot::open(&dir.join("archives")).unwrap();
    let repo = Repository::open(dir.join("t.sqlite").to_str().unwrap()).await.unwrap();
    H { repo, root, dir, clock: AtomicI64::new(1000) }
}
async fn active(h: &H, interaction: u64, kind: Kind) -> Ticket {
    let t = h.repo.reserve(interaction, G, OPENER, kind).await.unwrap();
    assert!(h.repo.claim(t.id).await.unwrap());
    h.repo.attach(t.id, CH + interaction).await.unwrap();
    h.repo.ticket_by_id(t.id).await.unwrap().unwrap()
}
fn actor(t: &Ticket) -> Actor {
    Actor { user: 7, guild: G, is_guild_owner: false, roles: vec![STAFF], channel: t.channel.unwrap() }
}
async fn run(h: &H, io: &Fake, a: &Actor, id: i64, tok: &str, s: &Settings) -> CloseOutcome {
    let now = |_: ()| 0;
    let _ = now;
    let clock = &h.clock;
    let f = move || clock.fetch_add(1, Ordering::SeqCst);
    let limits = Limits::default();
    let env = Env { settings: s, repo: &h.repo, io, root: &h.root, limits: &limits, now: &f };
    close_ticket(&env, a, id, tok).await
}
async fn status(h: &H, id: i64) -> String {
    h.repo.ticket_by_id(id).await.unwrap().unwrap().status
}

#[tokio::test]
async fn happy_path_both_kinds_archives_verifies_then_deletes_once() {
    for (i, kind) in [(1u64, Kind::Support), (2, Kind::Age)] {
        let h = harness("happy").await;
        let t = active(&h, i, kind).await;
        let io = Fake::new(&h.dir.join("archives"), 250);
        assert_eq!(run(&h, &io, &actor(&t), t.id, "tok", &settings()).await, CloseOutcome::Closed);
        assert_eq!(io.calls("delete"), 1);
        assert_eq!(*io.archive_existed_at_delete.lock().unwrap(), vec![true]);
        assert_eq!(status(&h, t.id).await, "closed");
        let ar = h.repo.latest_archive(t.id).await.unwrap().unwrap();
        assert_eq!(ar.messages, 250);
        assert_eq!(h.repo.dm_status(t.id).await.unwrap().as_deref(), Some("sent"));
        // slot freed only now
        assert!(h.repo.reserve(10 + i, G, OPENER, kind).await.is_ok());
        let hist: Vec<_> = h.repo.history(t.id).await.unwrap().into_iter().map(|e| e.to).collect();
        assert_eq!(hist, ["closing", "archiving", "verified", "delete_requested", "closed"]);
        let _ = std::fs::remove_dir_all(&h.dir);
    }
}
#[tokio::test]
async fn unauthorized_actors_get_denied_with_zero_private_io_or_mutation() {
    let h = harness("authz").await;
    let t = active(&h, 1, Kind::Support).await;
    let s = settings();
    let good = actor(&t);
    let cases = [
        Actor { roles: vec![], ..good.clone() },                                    // requester / admin without staff role
        Actor { user: OPENER, roles: vec![], ..good.clone() },                     // requester
        Actor { guild: G + 1, ..good.clone() },                                    // foreign guild
        Actor { channel: CH + 99, ..good.clone() },                                // copied/other channel
        Actor { roles: vec![STAFF + 1], ..good.clone() },                          // wrong role
    ];
    for a in cases {
        let io = Fake::new(&h.dir.join("archives"), 5);
        assert_eq!(run(&h, &io, &a, t.id, "x", &s).await, CloseOutcome::Denied);
        assert!(io.log.lock().unwrap().is_empty());
        assert_eq!(status(&h, t.id).await, "active");
    }
    let io = Fake::new(&h.dir.join("archives"), 5);
    assert_eq!(run(&h, &io, &good, 9999, "x", &s).await, CloseOutcome::Denied);
    // actual owner without staff role is allowed
    let owner = Actor { is_guild_owner: true, roles: vec![], ..good };
    assert_eq!(run(&h, &io, &owner, t.id, "o", &s).await, CloseOutcome::Closed);
    let _ = std::fs::remove_dir_all(&h.dir);
}
#[tokio::test]
async fn any_incompleteness_means_zero_deletion_and_slot_retained() {
    let h = harness("incomplete").await;
    let t = active(&h, 1, Kind::Support).await;
    let mut io = Fake::new(&h.dir.join("archives"), 120);
    io.history_fail = true;
    assert!(matches!(run(&h, &io, &actor(&t), t.id, "a", &settings()).await, CloseOutcome::Retained(_)));
    assert_eq!(io.calls("delete"), 0);
    assert_eq!(status(&h, t.id).await, "archiving");
    // slot still held (returns the same operation instead of a new one)
    let again = h.repo.reserve(77, G, OPENER, Kind::Support).await;
    assert!(again.map(|x| x.id == t.id).unwrap_or(true));
    assert!(h.repo.latest_archive(t.id).await.unwrap().is_none());
    // retry succeeds after the transient failure, without overwriting anything
    io.history_fail = false;
    assert_eq!(run(&h, &io, &actor(&t), t.id, "b", &settings()).await, CloseOutcome::Closed);
    let _ = std::fs::remove_dir_all(&h.dir);
}
#[tokio::test]
async fn unknown_delete_keeps_archive_and_reconciles_without_blind_repeat() {
    let h = harness("uncertain").await;
    let t = active(&h, 1, Kind::Support).await;
    let io = Fake::new(&h.dir.join("archives"), 10);
    *io.delete.lock().unwrap() = vec![DeleteOutcome::Unknown];
    assert!(matches!(run(&h, &io, &actor(&t), t.id, "a", &settings()).await, CloseOutcome::Retained(_)));
    assert_eq!(status(&h, t.id).await, "delete_uncertain");
    assert_eq!(io.calls("delete"), 1);
    // probe Unknown -> still retained, still no new delete
    *io.probe.lock().unwrap() = Presence::Unknown;
    assert!(matches!(run(&h, &io, &actor(&t), t.id, "b", &settings()).await, CloseOutcome::Retained(_)));
    assert_eq!(io.calls("delete"), 1);
    // archive corrupted -> even known absence must NOT close
    let ar = h.repo.latest_archive(t.id).await.unwrap().unwrap();
    let tp = h.root.path().join(&ar.dir_name).join("transcript.html");
    let good = std::fs::read(&tp).unwrap();
    std::fs::write(&tp, b"x").unwrap();
    *io.probe.lock().unwrap() = Presence::Absent;
    assert!(matches!(run(&h, &io, &actor(&t), t.id, "c", &settings()).await, CloseOutcome::Retained("reverify")));
    assert_eq!(status(&h, t.id).await, "delete_uncertain");
    // restored + known absence => closed, with no extra delete call
    std::fs::write(&tp, good).unwrap();
    assert_eq!(run(&h, &io, &actor(&t), t.id, "d", &settings()).await, CloseOutcome::Closed);
    assert_eq!(io.calls("delete"), 1);
    let _ = std::fs::remove_dir_all(&h.dir);
}
#[tokio::test]
async fn rejected_delete_returns_to_verified_and_retry_deletes_again() {
    let h = harness("rejected").await;
    let t = active(&h, 1, Kind::Age).await;
    let io = Fake::new(&h.dir.join("archives"), 3);
    *io.delete.lock().unwrap() = vec![DeleteOutcome::Rejected, DeleteOutcome::Deleted];
    assert!(matches!(run(&h, &io, &actor(&t), t.id, "a", &settings()).await, CloseOutcome::Retained(_)));
    assert_eq!(status(&h, t.id).await, "verified");
    assert_eq!(run(&h, &io, &actor(&t), t.id, "b", &settings()).await, CloseOutcome::Closed);
    // the earlier verified archive was reused, not regenerated
    assert_eq!(h.repo.latest_archive(t.id).await.unwrap().unwrap().seq, 1);
    let _ = std::fs::remove_dir_all(&h.dir);
}
#[tokio::test]
async fn parallel_close_workers_are_prevented() {
    let h = harness("parallel").await;
    let t = active(&h, 1, Kind::Support).await;
    let io = Fake::new(&h.dir.join("archives"), 300);
    let s = settings();
    let a = actor(&t);
    let (x, y) = tokio::join!(run(&h, &io, &a, t.id, "one", &s), run(&h, &io, &a, t.id, "two", &s));
    let outs = [x, y];
    assert_eq!(outs.iter().filter(|o| **o == CloseOutcome::Closed).count(), 1, "{outs:?}");
    assert_eq!(io.calls("delete"), 1);
    let _ = std::fs::remove_dir_all(&h.dir);
}
#[tokio::test]
async fn dm_failure_is_recorded_and_does_not_undo_closure() {
    let h = harness("dm").await;
    let t = active(&h, 1, Kind::Support).await;
    let mut io = Fake::new(&h.dir.join("archives"), 3);
    io.dm_ok = false;
    assert_eq!(run(&h, &io, &actor(&t), t.id, "a", &settings()).await, CloseOutcome::Closed);
    assert_eq!(h.repo.dm_status(t.id).await.unwrap().as_deref(), Some("failed"));
    let _ = std::fs::remove_dir_all(&h.dir);
}
#[test]
fn replies_are_fixed_text() {
    for o in [CloseOutcome::Closed, CloseOutcome::Denied, CloseOutcome::Busy, CloseOutcome::Retained("any internal detail")] {
        assert!(!o.reply().contains("internal detail"));
    }
}

macro_rules! env_run { ($h:expr, $io:expr, $s:expr, |$e:ident| $body:expr) => {{ let clock = &$h.clock; let now = move || clock.fetch_add(1, Ordering::SeqCst); let limits = Limits::default(); let $e = Env { settings: $s, repo: &$h.repo, io: $io, root: &$h.root, limits: &limits, now: &now }; let $e = &$e; $body.await }}; }

#[tokio::test]
async fn reconcile_retains_unverified_create_and_never_adopts_or_releases() {
    let h = harness("recon").await;
    let t = h.repo.reserve(1, G, OPENER, Kind::Support).await.unwrap();
    assert!(h.repo.claim(t.id).await.unwrap()); // creating, outcome unknown
    let mut io = Fake::new(&h.dir.join("archives"), 1);
    io.cands = vec![RemoteChannel { id: 555, guild: G, category: 300, topic: super::super::policy::marker(&t.operation), private: true }];
    let s = settings();
    let r = env_run!(h, &io, &s, |e| reconcile(e)).unwrap();
    assert_eq!(r.moved_to_recovery, 1);
    assert_eq!(r.unresolved, 1);
    assert!(!r.opening_gate(true));
    let tk = h.repo.ticket_by_id(t.id).await.unwrap().unwrap();
    assert_eq!((tk.status.as_str(), tk.channel), ("recovery", None)); // candidate NOT adopted
    // slot still held
    assert_eq!(h.repo.reserve(2, G, OPENER, Kind::Support).await.map(|x| x.id).ok(), Some(t.id));
    let clean = harness("recon2").await;
    let r = env_run!(clean, &io, &s, |e| reconcile(e)).unwrap();
    assert!(r.opening_gate(true) && !r.opening_gate(false));
    let _ = std::fs::remove_dir_all(&h.dir);
    let _ = std::fs::remove_dir_all(&clean.dir);
}
#[tokio::test]
async fn reconcile_closes_only_known_absence_after_verified_archive() {
    let h = harness("recon3").await;
    let t = active(&h, 1, Kind::Support).await;
    let io = Fake::new(&h.dir.join("archives"), 4);
    *io.delete.lock().unwrap() = vec![DeleteOutcome::Unknown];
    run(&h, &io, &actor(&t), t.id, "a", &settings()).await;
    assert_eq!(status(&h, t.id).await, "delete_uncertain");
    let s = settings();
    *io.probe.lock().unwrap() = Presence::Unknown;
    let r = env_run!(h, &io, &s, |e| reconcile(e)).unwrap();
    assert_eq!((r.closed_by_known_absence, r.unresolved), (0, 1));
    *io.probe.lock().unwrap() = Presence::Absent;
    let r = env_run!(h, &io, &s, |e| reconcile(e)).unwrap();
    assert_eq!((r.closed_by_known_absence, r.unresolved), (1, 0));
    assert_eq!(status(&h, t.id).await, "closed");
    assert_eq!(io.calls("delete"), 1);
    let _ = std::fs::remove_dir_all(&h.dir);
}
#[tokio::test]
async fn operator_recovery_needs_confirmation_reason_checks_and_is_audited() {
    let h = harness("op").await;
    let t = h.repo.reserve(1, G, OPENER, Kind::Support).await.unwrap();
    assert!(h.repo.claim(t.id).await.unwrap());
    let mut io = Fake::new(&h.dir.join("archives"), 1);
    io.cands = vec![
        RemoteChannel { id: 555, guild: G, category: 300, topic: String::new(), private: true },
        RemoteChannel { id: 556, guild: G, category: 301, topic: String::new(), private: true }, // wrong category
        RemoteChannel { id: 557, guild: G, category: 300, topic: String::new(), private: false }, // not private
    ];
    let s = settings();
    let staff = Actor { user: 7, guild: G, is_guild_owner: false, roles: vec![STAFF], channel: 1 };
    let nobody = Actor { roles: vec![], ..staff.clone() };
    let st = |a: Actor, r: Recovery| {
        let (h, io, s) = (&h, &io, &s);
        async move { env_run!(h, io, s, |e| recover(e, &a, t.id, r)) }
    };
    assert_eq!(st(nobody, Recovery::Status).await, "You cannot manage this ticket.");
    assert!(st(staff.clone(), Recovery::Status).await.contains("unresolved"));
    assert_eq!(status(&h, t.id).await, "creating"); // status is non-destructive
    assert!(st(staff.clone(), Recovery::Attach { channel: 555, confirm: false, reason: "r".into() }).await.contains("confirmation"));
    assert!(st(staff.clone(), Recovery::Attach { channel: 555, confirm: true, reason: " ".into() }).await.contains("reason"));
    for bad in [556u64, 557, 999] {
        assert!(st(staff.clone(), Recovery::Attach { channel: bad, confirm: true, reason: "r".into() }).await.contains("does not match"));
    }
    assert!(st(staff.clone(), Recovery::Release { confirm: true, reason: "r".into() }).await.contains("Slot released"), "no marker candidate -> release allowed");
    assert_eq!(status(&h, t.id).await, "released");
    let audit = h.repo.audit(t.id).await.unwrap();
    assert_eq!((audit[0].actor, audit[0].action.as_str(), audit[0].reason.as_str()), (7, "release", "r"));
    // attach path on a fresh operation
    let t2 = h.repo.reserve(2, G, OPENER, Kind::Support).await.unwrap();
    h.repo.claim(t2.id).await.unwrap();
    let r = env_run!(h, &io, &s, |e| recover(e, &staff, t2.id, Recovery::Attach { channel: 555, confirm: true, reason: "verified in UI".into() }));
    assert!(r.contains("attached"));
    assert_eq!(h.repo.ticket_by_id(t2.id).await.unwrap().unwrap().channel, Some(555));
    assert_eq!(h.repo.audit(t2.id).await.unwrap()[0].action, "attach");
    let _ = std::fs::remove_dir_all(&h.dir);
}

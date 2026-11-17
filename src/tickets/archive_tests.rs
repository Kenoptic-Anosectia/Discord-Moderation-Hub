use super::*;
use serenity::all::{Attachment, MessageId};
use std::sync::Mutex;

#[path = "../../tests/support/ticket_archive_fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

fn owned(tag: &str) -> (PathBuf, OwnedRoot) {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/a07-owned-tests")
        .join(format!("arc-{}-{}-{}", tag, std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let r = OwnedRoot::open(&p).unwrap();
    (p, r)
}

struct Pages {
    msgs: Vec<Message>,
    fail_at: Option<usize>,
    late: Mutex<Option<Message>>, // appears after exhaustion
    calls: Mutex<usize>,
    dup_boundary: bool,
    empties: Mutex<usize>,
}
impl Pages {
    fn new(n: usize) -> Self {
        Self { msgs: fixtures::message_pages(n).into_iter().flatten().collect(), fail_at: None, late: Mutex::new(None), calls: Mutex::new(0), dup_boundary: false, empties: Mutex::new(0) }
    }
}
impl HistorySource for Pages {
    async fn page_after(&self, after: u64) -> Result<Vec<Message>, ArchiveError> {
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        if self.fail_at == Some(n) {
            return Err(ArchiveError::Source);
        }
        let mut v: Vec<Message> = self.msgs.iter().filter(|m| m.id.get() > after).take(100).cloned().collect();
        if self.dup_boundary && after > 0 && !v.is_empty() {
            v.insert(0, self.msgs.iter().find(|m| m.id.get() == after).unwrap().clone());
        }
        if v.is_empty() && { let mut e = self.empties.lock().unwrap(); *e += 1; *e >= 2 } {
            if let Some(l) = self.late.lock().unwrap().clone() {
                return Ok(vec![l]);
            }
        }
        v.reverse(); // Discord returns newest-first: ordering must be imposed by us
        Ok(v)
    }
}

#[tokio::test]
async fn fetches_all_pages_beyond_100_in_order() {
    for n in [0usize, 1, 99, 100, 101, 250, 1000] {
        let got = fetch_all(&Pages::new(n), &Limits::default()).await.unwrap();
        assert_eq!(got.len(), n);
        assert!(got.windows(2).all(|w| w[0].id < w[1].id));
    }
}
#[tokio::test]
async fn page_error_duplicate_boundary_and_late_message_abort() {
    let mut p = Pages::new(250);
    p.fail_at = Some(2);
    assert_eq!(fetch_all(&p, &Limits::default()).await.unwrap_err(), ArchiveError::Source);
    let mut p = Pages::new(250);
    p.dup_boundary = true;
    assert_eq!(fetch_all(&p, &Limits::default()).await.unwrap_err(), ArchiveError::Order);
    let p = Pages::new(150);
    let mut late = p.msgs[0].clone();
    late.id = MessageId::new(99_999_999);
    *p.late.lock().unwrap() = Some(late);
    assert_eq!(fetch_all(&p, &Limits::default()).await.unwrap_err(), ArchiveError::Changed);
    let lim = Limits { max_messages: 10, ..Limits::default() };
    assert_eq!(fetch_all(&Pages::new(150), &lim).await.unwrap_err(), ArchiveError::TooLarge);
}

struct FakeDl {
    bytes: Vec<u8>,
    seen: Mutex<Vec<String>>,
}
impl Downloader for FakeDl {
    async fn fetch(&self, url: &url::Url, limit: u64) -> Result<Vec<u8>, ArchiveError> {
        self.seen.lock().unwrap().push(url.to_string());
        if self.bytes.len() as u64 > limit {
            return Err(ArchiveError::TooLarge);
        }
        Ok(self.bytes.clone())
    }
}
fn with_asset(mut m: Message, f: &fixtures::AssetFixture, url: &str, size: Option<u32>) -> Message {
    let mut a: Attachment = f.attachment.clone();
    a.url = url.to_string();
    if let Some(s) = size {
        a.size = s;
    }
    m.attachments.push(a);
    m
}
const CDN: &str = "https://cdn.discordapp.com/attachments/1/100/same.txt?ex=1&is=2&hm=3";

#[tokio::test]
async fn downloads_use_a10_validators_and_generated_names() {
    let fx = fixtures::asset_fixtures();
    let mut msgs = Pages::new(3).msgs;
    msgs[0] = with_asset(msgs[0].clone(), &fx[0], CDN, None);
    let dl = FakeDl { bytes: fx[0].bytes.clone(), seen: Mutex::new(vec![]) };
    let got = download_all(&dl, &msgs, &Limits::default()).await.unwrap();
    assert_eq!(got[0].entry.file, "attachment-100.bin");
    assert_eq!(got[0].entry.display_name, "same.txt");
    // hostile display names (traversal / drive) never reach the filesystem layer
    for hostile in [&fx[2], &fx[3]] {
        let mut m2 = Pages::new(1).msgs;
        m2[0] = with_asset(m2[0].clone(), hostile, "https://cdn.discordapp.com/attachments/1/300/x", None);
        let dl = FakeDl { bytes: hostile.bytes.clone(), seen: Mutex::new(vec![]) };
        let g = download_all(&dl, &m2, &Limits::default()).await.unwrap();
        assert!(validate_asset_component(&g[0].entry.file).is_ok());
    }
    // bad URL (A10 rejects) -> zero fetches
    for bad in ["http://cdn.discordapp.com/attachments/1/100/a", "https://evil.example/attachments/1/100/a", "https://cdn.discordapp.com/x"] {
        let mut m = Pages::new(1).msgs;
        m[0] = with_asset(m[0].clone(), &fx[0], bad, None);
        let dl = FakeDl { bytes: fx[0].bytes.clone(), seen: Mutex::new(vec![]) };
        assert_eq!(download_all(&dl, &m, &Limits::default()).await.err().unwrap(), ArchiveError::UnsafeInput);
        assert!(dl.seen.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn size_mismatch_and_limits_fail() {
    let fx = fixtures::asset_fixtures();
    let mut m = Pages::new(1).msgs;
    m[0] = with_asset(m[0].clone(), &fx[0], CDN, Some(5));
    let dl = FakeDl { bytes: fx[0].bytes.clone(), seen: Mutex::new(vec![]) };
    // declared 5 < actual -> fake enforces limit as TooLarge; declared==limit mismatch path:
    assert!(download_all(&dl, &m, &Limits::default()).await.is_err());
    let mut m = Pages::new(1).msgs;
    m[0] = with_asset(m[0].clone(), &fx[0], CDN, None);
    let lim = Limits { max_asset_bytes: 3, ..Limits::default() };
    assert_eq!(download_all(&dl, &m, &lim).await.err().unwrap(), ArchiveError::TooLarge);
    let lim = Limits { max_assets: 0, ..Limits::default() };
    assert_eq!(download_all(&dl, &m, &lim).await.err().unwrap(), ArchiveError::TooLarge);
    // short body: declared larger than delivered
    let mut m = Pages::new(1).msgs;
    m[0] = with_asset(m[0].clone(), &fx[0], CDN, Some(9999));
    assert_eq!(download_all(&dl, &m, &Limits::default()).await.err().unwrap(), ArchiveError::SizeMismatch);
}
#[test]
fn production_downloader_disables_redirects_and_non_https() {
    let src = include_str!("archive.rs");
    assert!(src.contains("redirect::Policy::none()") && src.contains("https_only(true)"));
    assert!(HttpDownloader::new(std::time::Duration::from_secs(1)).is_ok());
}

#[test]
fn transcript_escapes_and_has_no_active_or_remote_content() {
    let mut msgs = Pages::new(2).msgs;
    msgs[0].content = "<script>alert(1)</script><img src=http://x/y onerror=1> & \"q\"".into();
    msgs[0].author.name = "<svg onload=x>".into();
    let h = render_transcript(7, &msgs, &[]);
    assert!(!h.contains("<script") && !h.contains("<svg") && !h.contains("<img"));
    assert!(h.contains("&lt;script&gt;") && h.contains("&amp;"));
    assert!(!h.contains("<a href=\"http") && !h.contains("<link") && !h.contains("<iframe"));
    assert_eq!(h, render_transcript(7, &msgs, &[])); // deterministic
}

async fn build(n: usize, with_assets: bool) -> (Vec<Message>, Vec<Downloaded>) {
    let mut msgs = Pages::new(n).msgs;
    let mut dls = vec![];
    if with_assets {
        let fx = fixtures::asset_fixtures();
        msgs[0] = with_asset(msgs[0].clone(), &fx[0], CDN, None);
        let dl = FakeDl { bytes: fx[0].bytes.clone(), seen: Mutex::new(vec![]) };
        dls = download_all(&dl, &msgs, &Limits::default()).await.unwrap();
    }
    (msgs, dls)
}
fn input<'a>(m: &'a [Message], d: &'a [Downloaded], seq: i64) -> PublishInput<'a> {
    PublishInput { ticket: 5, channel: 9, seq, nonce: 1, messages: m, downloaded: d }
}

#[tokio::test]
async fn publish_and_verify_roundtrip_then_detects_corruption() {
    let (p, root) = owned("rt");
    let (m, d) = build(150, true).await;
    let out = publish(&root, &input(&m, &d, 1)).unwrap();
    let dir = root.path().join(&out.dir_name);
    let ids: Vec<u64> = m.iter().map(|x| x.id.get()).collect();
    assert!(verify(&root, &dir, Some(&out.manifest_sha256), &ids).is_ok());
    assert!(std::fs::read_dir(root.path()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".staging")));
    // corrupt asset / delete asset / extra file / wrong id list / wrong manifest hash
    let asset = dir.join("assets/attachment-100.bin");
    let orig = std::fs::read(&asset).unwrap();
    std::fs::write(&asset, b"tampered").unwrap();
    assert_eq!(verify(&root, &dir, None, &ids).unwrap_err(), ArchiveError::Verify);
    std::fs::write(&asset, &orig).unwrap();
    assert!(verify(&root, &dir, None, &ids).is_ok());
    std::fs::write(dir.join("extra.txt"), b"x").unwrap();
    assert_eq!(verify(&root, &dir, None, &ids).unwrap_err(), ArchiveError::Verify);
    std::fs::remove_file(dir.join("extra.txt")).unwrap();
    assert_eq!(verify(&root, &dir, Some("00"), &ids).unwrap_err(), ArchiveError::Verify);
    assert_eq!(verify(&root, &dir, None, &ids[1..]).unwrap_err(), ArchiveError::Verify);
    std::fs::remove_file(&asset).unwrap();
    assert!(verify(&root, &dir, None, &ids).is_err());
    let _ = std::fs::remove_dir_all(p);
}
#[tokio::test]
async fn existing_final_archive_is_never_overwritten() {
    let (p, root) = owned("nover");
    let (m, d) = build(3, false).await;
    let first = publish(&root, &input(&m, &d, 1)).unwrap();
    let fin = root.path().join(&first.dir_name);
    let before = std::fs::read(fin.join("manifest.json")).unwrap();
    assert_eq!(publish(&root, &input(&m, &d, 1)).unwrap_err(), ArchiveError::Exists);
    assert_eq!(std::fs::read(fin.join("manifest.json")).unwrap(), before);
    assert!(publish(&root, &input(&m, &d, 2)).is_ok()); // retry uses a new sequence
    let _ = std::fs::remove_dir_all(p);
}
#[tokio::test]
async fn failed_publication_leaves_no_final_dir_and_cleans_staging() {
    let (p, root) = owned("fault");
    let (m, mut d) = build(3, true).await;
    d[0].entry.file = "../escape.bin".into(); // would-be traversal
    assert_eq!(publish(&root, &input(&m, &d, 1)).unwrap_err(), ArchiveError::UnsafeInput);
    d[0].entry.file = "attachment-100.bin".into();
    d[0].entry.sha256 = "00".into(); // manifest says one thing, bytes another
    assert_eq!(publish(&root, &input(&m, &d, 1)).unwrap_err(), ArchiveError::Verify);
    let names: Vec<_> = std::fs::read_dir(root.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(names.is_empty(), "{names:?}");
    // pre-existing staging path (create-new) => Io, no overwrite
    std::fs::create_dir(root.path().join(".staging-5-1-1")).unwrap();
    let (m2, d2) = build(2, false).await;
    assert_eq!(publish(&root, &input(&m2, &d2, 1)).unwrap_err(), ArchiveError::Io);
    let _ = std::fs::remove_dir_all(p);
}
#[test]
fn root_rejects_file_and_symlink_roots() {
    let (p, root) = owned("sym");
    let f = root.path().join("afile");
    std::fs::write(&f, b"x").unwrap();
    assert!(OwnedRoot::open(&f).is_err());
    let link = p.join("link");
    // Symlink creation may need privilege on Windows; assert only when it succeeds.
    #[cfg(unix)]
    let ok = std::os::unix::fs::symlink(&p, &link).is_ok();
    #[cfg(windows)]
    let ok = std::os::windows::fs::symlink_dir(&p, &link).is_ok();
    if ok {
        assert_eq!(OwnedRoot::open(&link).err(), Some(ArchiveError::Unsafe));
    }
    let _ = std::fs::remove_dir_all(p);
}

//! Exercises history collection, attachment downloads, and archive publication and verification.
//! Synthetic history, URLs, and on-disk mutations stand in for untrusted inputs.
//! Writes are confined to fixture roots; no real ticket data or live HTTP requests are used.

use super::*;
use serenity::all::{Attachment, MessageId};
use std::sync::Mutex;

use crate::tickets::fixtures;

/// Opens a fixture-owned archive root under `target` for a trusted test tag.
/// Returns the cleanup path and root handle; panics if setup fails.
fn owned(tag: &str) -> (PathBuf, OwnedRoot) {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/a07-owned-tests")
        .join(format!(
            "arc-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    let r = OwnedRoot::open(&p).unwrap();
    (p, r)
}

/// Models cursor pages from synthetic history, with optional source faults and late arrival.
struct Pages {
    msgs: Vec<Message>,
    fail_at: Option<usize>,
    late: Mutex<Option<Message>>, // appears after exhaustion
    calls: Mutex<usize>,
    dup_boundary: bool,
    empties: Mutex<usize>,
}
impl Pages {
    /// Builds ordered synthetic history with normal paging until a fault is configured.
    fn new(n: usize) -> Self {
        Self {
            msgs: fixtures::message_pages(n).into_iter().flatten().collect(),
            fail_at: None,
            late: Mutex::new(None),
            calls: Mutex::new(0),
            dup_boundary: false,
            empties: Mutex::new(0),
        }
    }
}
impl HistorySource for Pages {
    /// Serves newest-first cursor pages, with a scripted error or deliberate paging inconsistency.
    async fn page_after(&self, after: u64) -> Result<Vec<Message>, ArchiveError> {
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        if self.fail_at == Some(n) {
            return Err(ArchiveError::Source);
        }
        let mut v: Vec<Message> = self
            .msgs
            .iter()
            .filter(|m| m.id.get() > after)
            .take(100)
            .cloned()
            .collect();
        // Repeat the cursor item to catch acceptance of an overlapping page.
        if self.dup_boundary && after > 0 && !v.is_empty() {
            v.insert(
                0,
                self.msgs
                    .iter()
                    .find(|m| m.id.get() == after)
                    .unwrap()
                    .clone(),
            );
        }
        // Wait until the first empty page so the arrival tests the exhaustion recheck.
        if v.is_empty() && {
            let mut e = self.empties.lock().unwrap();
            *e += 1;
            *e >= 2
        } {
            if let Some(l) = self.late.lock().unwrap().clone() {
                return Ok(vec![l]);
            }
        }
        v.reverse(); // Discord returns newest-first: ordering must be imposed by us
        Ok(v)
    }
}

/// Fetches exactly the requested number of messages in increasing ID order across page cuts;
/// catches stopping after one page or returning server order.
#[tokio::test]
async fn fetches_all_pages_beyond_100_in_order() {
    for n in [0usize, 1, 99, 100, 101, 250, 1000] {
        let got = fetch_all(&Pages::new(n), &Limits::default()).await.unwrap();
        assert_eq!(got.len(), n);
        assert!(got.windows(2).all(|w| w[0].id < w[1].id));
    }
}
/// Source failure, cursor overlap, late arrival, and message limits return distinct errors;
/// catches treating incomplete or inconsistent history as a complete snapshot.
#[tokio::test]
async fn page_error_duplicate_boundary_and_late_message_abort() {
    let mut p = Pages::new(250);
    p.fail_at = Some(2);
    assert_eq!(
        fetch_all(&p, &Limits::default()).await.unwrap_err(),
        ArchiveError::Source
    );
    let mut p = Pages::new(250);
    p.dup_boundary = true;
    assert_eq!(
        fetch_all(&p, &Limits::default()).await.unwrap_err(),
        ArchiveError::Order
    );
    let p = Pages::new(150);
    let mut late = p.msgs[0].clone();
    late.id = MessageId::new(99_999_999);
    *p.late.lock().unwrap() = Some(late);
    assert_eq!(
        fetch_all(&p, &Limits::default()).await.unwrap_err(),
        ArchiveError::Changed
    );
    let lim = Limits {
        max_messages: 10,
        ..Limits::default()
    };
    assert_eq!(
        fetch_all(&Pages::new(150), &lim).await.unwrap_err(),
        ArchiveError::TooLarge
    );
}

/// In-memory downloader that records attempted synthetic URLs and enforces its byte budget.
struct FakeDl {
    bytes: Vec<u8>,
    seen: Mutex<Vec<String>>,
}
impl Downloader for FakeDl {
    /// Returns fixture bytes or `TooLarge`; records the attempt without making an HTTP request.
    async fn fetch(&self, url: &url::Url, limit: u64) -> Result<Vec<u8>, ArchiveError> {
        self.seen.lock().unwrap().push(url.to_string());
        if self.bytes.len() as u64 > limit {
            return Err(ArchiveError::TooLarge);
        }
        Ok(self.bytes.clone())
    }
}
/// Adds synthetic attachment metadata with a chosen URL and optional declared-size override.
fn with_asset(mut m: Message, f: &fixtures::AssetFixture, url: &str, size: Option<u32>) -> Message {
    let mut a: Attachment = f.attachment.clone();
    a.url = url.to_string();
    if let Some(s) = size {
        a.size = s;
    }
    m.attachments.push(a);
    m
}
/// Allowlisted attachment URL with synthetic signature fields; never fetched live here.
const CDN: &str = "https://cdn.discordapp.com/attachments/1/100/same.txt?ex=1&is=2&hm=3";

/// Checks generated file names independently of display names and rejects unsafe URLs before fetch;
/// catches passing rejected URLs to the downloader or using the original name as the file key.
#[tokio::test]
async fn downloads_use_a10_validators_and_generated_names() {
    let fx = fixtures::asset_fixtures();
    let mut msgs = Pages::new(3).msgs;
    msgs[0] = with_asset(msgs[0].clone(), &fx[0], CDN, None);
    let dl = FakeDl {
        bytes: fx[0].bytes.clone(),
        seen: Mutex::new(vec![]),
    };
    let got = download_all(&dl, &msgs, &Limits::default()).await.unwrap();
    assert_eq!(got[0].entry.file, "attachment-100.bin");
    assert_eq!(got[0].entry.display_name, "same.txt");
    // hostile display names (traversal / drive) never reach the filesystem layer
    for hostile in [&fx[2], &fx[3]] {
        let mut m2 = Pages::new(1).msgs;
        m2[0] = with_asset(
            m2[0].clone(),
            hostile,
            "https://cdn.discordapp.com/attachments/1/300/x",
            None,
        );
        let dl = FakeDl {
            bytes: hostile.bytes.clone(),
            seen: Mutex::new(vec![]),
        };
        let g = download_all(&dl, &m2, &Limits::default()).await.unwrap();
        assert!(validate_asset_component(&g[0].entry.file).is_ok());
    }
    // bad URL (A10 rejects) -> zero fetches
    for bad in [
        "http://cdn.discordapp.com/attachments/1/100/a",
        "https://evil.example/attachments/1/100/a",
        "https://cdn.discordapp.com/x",
    ] {
        let mut m = Pages::new(1).msgs;
        m[0] = with_asset(m[0].clone(), &fx[0], bad, None);
        let dl = FakeDl {
            bytes: fx[0].bytes.clone(),
            seen: Mutex::new(vec![]),
        };
        assert_eq!(
            download_all(&dl, &m, &Limits::default())
                .await
                .err()
                .unwrap(),
            ArchiveError::UnsafeInput
        );
        assert!(dl.seen.lock().unwrap().is_empty());
    }
}
/// Checks failure on length disagreement, per-asset byte overflow, and attachment-count overflow;
/// catches accepting a partial response or ignoring the configured limits.
#[tokio::test]
async fn size_mismatch_and_limits_fail() {
    let fx = fixtures::asset_fixtures();
    let mut m = Pages::new(1).msgs;
    m[0] = with_asset(m[0].clone(), &fx[0], CDN, Some(5));
    let dl = FakeDl {
        bytes: fx[0].bytes.clone(),
        seen: Mutex::new(vec![]),
    };
    // An oversized fake response and a short response exercise different checks.
    assert!(download_all(&dl, &m, &Limits::default()).await.is_err());
    let mut m = Pages::new(1).msgs;
    m[0] = with_asset(m[0].clone(), &fx[0], CDN, None);
    let lim = Limits {
        max_asset_bytes: 3,
        ..Limits::default()
    };
    assert_eq!(
        download_all(&dl, &m, &lim).await.err().unwrap(),
        ArchiveError::TooLarge
    );
    let lim = Limits {
        max_assets: 0,
        ..Limits::default()
    };
    assert_eq!(
        download_all(&dl, &m, &lim).await.err().unwrap(),
        ArchiveError::TooLarge
    );
    // short body: declared larger than delivered
    let mut m = Pages::new(1).msgs;
    m[0] = with_asset(m[0].clone(), &fx[0], CDN, Some(9999));
    assert_eq!(
        download_all(&dl, &m, &Limits::default())
            .await
            .err()
            .unwrap(),
        ArchiveError::SizeMismatch
    );
}
/// Checks the source contains both client policy settings and that client construction succeeds;
/// catches removing either setting's source marker, not runtime redirect behavior.
#[test]
fn production_downloader_disables_redirects_and_non_https() {
    let src = include_str!("archive.rs");
    assert!(src.contains("redirect::Policy::none()") && src.contains("https_only(true)"));
    assert!(HttpDownloader::new(std::time::Duration::from_secs(1)).is_ok());
}

/// Checks deterministic escaping of sampled message and author markup and absence of sampled tags;
/// catches interpolating raw fixture HTML into a transcript.
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

/// Builds synthetic history and optionally downloads one fixture attachment in memory.
/// Pass a nonzero count when requesting an attachment; setup failures panic.
async fn build(n: usize, with_assets: bool) -> (Vec<Message>, Vec<Downloaded>) {
    let mut msgs = Pages::new(n).msgs;
    let mut dls = vec![];
    if with_assets {
        let fx = fixtures::asset_fixtures();
        msgs[0] = with_asset(msgs[0].clone(), &fx[0], CDN, None);
        let dl = FakeDl {
            bytes: fx[0].bytes.clone(),
            seen: Mutex::new(vec![]),
        };
        dls = download_all(&dl, &msgs, &Limits::default()).await.unwrap();
    }
    (msgs, dls)
}
/// Binds fixture history and downloaded assets to a fixed synthetic identity and chosen sequence.
fn input<'a>(m: &'a [Message], d: &'a [Downloaded], seq: i64) -> PublishInput<'a> {
    PublishInput {
        ticket: 5,
        channel: 9,
        seq,
        nonce: 1,
        messages: m,
        downloaded: d,
    }
}

/// A published archive verifies, but changed or missing asset bytes, extra files, and wrong
/// expectations fail; catches a verifier that trusts incomplete or mismatched archives.
#[tokio::test]
async fn publish_and_verify_roundtrip_then_detects_corruption() {
    let (p, root) = owned("rt");
    let (m, d) = build(150, true).await;
    let out = publish(&root, &input(&m, &d, 1)).unwrap();
    let dir = root.path().join(&out.dir_name);
    let ids: Vec<u64> = m.iter().map(|x| x.id.get()).collect();
    assert!(verify(&root, &dir, Some(&out.manifest_sha256), &ids).is_ok());
    assert!(std::fs::read_dir(root.path()).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".staging")));
    // corrupt asset / delete asset / extra file / wrong id list / wrong manifest hash
    let asset = dir.join("assets/attachment-100.bin");
    let orig = std::fs::read(&asset).unwrap();
    std::fs::write(&asset, b"tampered").unwrap();
    assert_eq!(
        verify(&root, &dir, None, &ids).unwrap_err(),
        ArchiveError::Verify
    );
    std::fs::write(&asset, &orig).unwrap();
    assert!(verify(&root, &dir, None, &ids).is_ok());
    std::fs::write(dir.join("extra.txt"), b"x").unwrap();
    assert_eq!(
        verify(&root, &dir, None, &ids).unwrap_err(),
        ArchiveError::Verify
    );
    std::fs::remove_file(dir.join("extra.txt")).unwrap();
    assert_eq!(
        verify(&root, &dir, Some("00"), &ids).unwrap_err(),
        ArchiveError::Verify
    );
    assert_eq!(
        verify(&root, &dir, None, &ids[1..]).unwrap_err(),
        ArchiveError::Verify
    );
    std::fs::remove_file(&asset).unwrap();
    assert!(verify(&root, &dir, None, &ids).is_err());
    let _ = std::fs::remove_dir_all(p);
}
/// Reusing a final archive name returns `Exists` and preserves its manifest;
/// catches overwriting an earlier version rather than publishing under a fresh sequence.
#[tokio::test]
async fn existing_final_archive_is_never_overwritten() {
    let (p, root) = owned("nover");
    let (m, d) = build(3, false).await;
    let first = publish(&root, &input(&m, &d, 1)).unwrap();
    let fin = root.path().join(&first.dir_name);
    let before = std::fs::read(fin.join("manifest.json")).unwrap();
    assert_eq!(
        publish(&root, &input(&m, &d, 1)).unwrap_err(),
        ArchiveError::Exists
    );
    assert_eq!(std::fs::read(fin.join("manifest.json")).unwrap(), before);
    assert!(publish(&root, &input(&m, &d, 2)).is_ok()); // retry uses a new sequence
    let _ = std::fs::remove_dir_all(p);
}
/// Unsafe names and digest mismatch leave the root empty; staging collision returns `Io`.
/// Catches leftover partial archives or accepting an occupied staging path.
#[tokio::test]
async fn failed_publication_leaves_no_final_dir_and_cleans_staging() {
    let (p, root) = owned("fault");
    let (m, mut d) = build(3, true).await;
    d[0].entry.file = "../escape.bin".into(); // would-be traversal
    assert_eq!(
        publish(&root, &input(&m, &d, 1)).unwrap_err(),
        ArchiveError::UnsafeInput
    );
    d[0].entry.file = "attachment-100.bin".into();
    d[0].entry.sha256 = "00".into(); // manifest says one thing, bytes another
    assert_eq!(
        publish(&root, &input(&m, &d, 1)).unwrap_err(),
        ArchiveError::Verify
    );
    let names: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(names.is_empty(), "{names:?}");
    // pre-existing staging path (create-new) => Io, no overwrite
    std::fs::create_dir(root.path().join(".staging-5-1-1")).unwrap();
    let (m2, d2) = build(2, false).await;
    assert_eq!(
        publish(&root, &input(&m2, &d2, 1)).unwrap_err(),
        ArchiveError::Io
    );
    let _ = std::fs::remove_dir_all(p);
}
/// Rejects a regular file root and, where the fixture link can be created, a symlink root;
/// catches accepting those paths as owned archive directories.
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

// ---------- production HttpDownloader policy over a controlled no-network transport ----------
mod http_policy {
    use super::super::fake_transport::{FakeTransport, Script};
    use super::*;
    use std::time::Duration;

    /// Builds the actual downloader policy with scripted responses and a bounded timeout.
    fn dl(script: Vec<Script>, fallback: Script) -> HttpDownloader<FakeTransport> {
        HttpDownloader::with_transport(
            FakeTransport::new(script, fallback),
            Duration::from_millis(200),
        )
    }
    fn url() -> url::Url {
        validate_attachment_url(CDN).unwrap()
    }
    /// Constructs a synthetic HTTP response with optional declared size and separate stream chunks.
    fn resp(status: u16, declared: Option<u64>, chunks: &[&[u8]]) -> Script {
        Script::Resp {
            status,
            declared,
            chunks: chunks.iter().map(|c| c.to_vec()).collect(),
        }
    }
    /// Two successful body chunks produce one exact payload from a single request;
    /// catches dropping a chunk or fetching again.
    #[tokio::test]
    async fn success_streams_and_returns_exact_bytes() {
        let d = dl(vec![resp(200, Some(6), &[b"abc", b"def"])], Script::Fail);
        assert_eq!(d.fetch(&url(), 100).await.unwrap(), b"abcdef");
        assert_eq!(d.transport.get_count(), 1);
    }
    /// Sampled redirect statuses fail after one request even with a successful fallback;
    /// catches following a redirect with a second transport call.
    #[tokio::test]
    async fn redirects_fail_and_the_target_is_never_fetched() {
        for code in [301u16, 302, 303, 307, 308] {
            let d = dl(
                vec![resp(code, None, &[b"Location: https://evil.example/x"])],
                resp(200, Some(2), &[b"ok"]),
            );
            assert_eq!(
                d.fetch(&url(), 100).await.unwrap_err(),
                ArchiveError::Download,
                "{code}"
            );
            assert_eq!(
                d.transport.get_count(),
                1,
                "redirect target must not be requested"
            );
        }
    }
    /// Sampled non-success statuses return `Download` after one request;
    /// catches accepting an HTTP error response as attachment bytes.
    #[tokio::test]
    async fn non_2xx_statuses_fail_without_reading_the_body() {
        for code in [100u16, 400, 401, 403, 404, 429, 500, 503] {
            let d = dl(vec![resp(code, Some(2), &[b"no"])], Script::Fail);
            assert_eq!(
                d.fetch(&url(), 100).await.unwrap_err(),
                ArchiveError::Download,
                "{code}"
            );
            assert_eq!(d.transport.get_count(), 1);
        }
    }
    /// A transport error and a hanging response both return `Download`, with the hang bounded;
    /// catches waiting indefinitely for the transport.
    #[tokio::test]
    async fn transport_error_and_timeout_map_to_download_failure() {
        let d = dl(vec![Script::Fail], Script::Fail);
        assert_eq!(
            d.fetch(&url(), 100).await.unwrap_err(),
            ArchiveError::Download
        );
        let d = dl(vec![Script::Hang], Script::Fail);
        let t = std::time::Instant::now();
        assert_eq!(
            d.fetch(&url(), 100).await.unwrap_err(),
            ArchiveError::Download
        );
        assert!(t.elapsed() < Duration::from_secs(5), "deadline enforced");
    }
    /// Oversized declarations, oversized streams, and short bodies fail;
    /// catches enforcing limits on headers alone or accepting incomplete bytes.
    #[tokio::test]
    async fn oversize_declared_or_streamed_is_rejected_and_short_body_fails() {
        let d = dl(vec![resp(200, Some(101), &[b"x"])], Script::Fail);
        assert_eq!(
            d.fetch(&url(), 100).await.unwrap_err(),
            ArchiveError::TooLarge
        );
        let big = vec![7u8; 60];
        let d = dl(vec![resp(200, None, &[&big, &big])], Script::Fail);
        assert_eq!(
            d.fetch(&url(), 100).await.unwrap_err(),
            ArchiveError::TooLarge
        );
        let d = dl(vec![resp(200, Some(5), &[&big])], Script::Fail);
        assert!(d.fetch(&url(), 100).await.is_err());
        let d = dl(vec![resp(200, Some(10), &[b"short"])], Script::Fail);
        assert_eq!(
            d.fetch(&url(), 100).await.unwrap_err(),
            ArchiveError::Download
        );
    }
    /// Parsed disallowed URLs still return `UnsafeInput` before transport;
    /// catches trusting the URL type alone as proof of validation.
    #[tokio::test]
    async fn non_allowlisted_or_non_https_urls_never_reach_the_transport() {
        let d = dl(vec![], resp(200, Some(2), &[b"ok"]));
        for bad in [
            "http://cdn.discordapp.com/attachments/1/100/a.txt",
            "https://evil.example/attachments/1/100/a.txt",
            "https://127.0.0.1/a.txt",
        ] {
            let u = url::Url::parse(bad).unwrap();
            assert_eq!(
                d.fetch(&u, 100).await.unwrap_err(),
                ArchiveError::UnsafeInput,
                "{bad}"
            );
        }
        assert_eq!(d.transport.get_count(), 0);
    }
    /// The attachment batch propagates a downloader redirect error and accepts the valid control;
    /// catches swallowing transport-policy errors in archive collection.
    #[tokio::test]
    async fn download_all_through_the_production_downloader_fails_closed() {
        let fx = fixtures::asset_fixtures();
        let mut m = Pages::new(1).msgs;
        m[0] = with_asset(m[0].clone(), &fx[0], CDN, Some(5));
        let d = dl(vec![resp(302, None, &[])], Script::Fail);
        assert_eq!(
            download_all(&d, &m, &Limits::default())
                .await
                .err()
                .unwrap(),
            ArchiveError::Download
        );
        let d = dl(vec![resp(200, Some(5), &[b"12345"])], Script::Fail);
        assert!(download_all(&d, &m, &Limits::default()).await.is_ok());
    }
}

// ---------- bounds (SF2 archive-bounds) ----------

/// Opens a scratch root under `target/sf2-archive-bounds-tests` for a trusted test tag.
fn bounds_root(tag: &str) -> (PathBuf, OwnedRoot) {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/sf2-archive-bounds-tests")
        .join(format!(
            "b-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    let r = OwnedRoot::open(&p).unwrap();
    (p, r)
}

/// Writes `transcript` plus a matching manifest (no assets) into a new dir and returns its path.
fn write_raw_archive(root: &OwnedRoot, name: &str, transcript: &str, ids: &[u64]) -> PathBuf {
    let dir = root.path().join(name);
    std::fs::create_dir(&dir).unwrap();
    let m = Manifest {
        format: 1,
        ticket: 7,
        channel: 9,
        message_count: ids.len() as u64,
        first_message: ids.first().copied().unwrap_or(0),
        last_message: ids.last().copied().unwrap_or(0),
        message_ids_sha256: String::new(),
        content_sha256: String::new(),
        transcript_len: transcript.len() as u64,
        transcript_sha256: sha256_hex(transcript.as_bytes()),
        assets: vec![],
    };
    std::fs::write(dir.join("transcript.html"), transcript).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&m).unwrap(),
    )
    .unwrap();
    dir
}

/// Best-of-three wall time of `verify` for an `n`-message synthetic archive.
fn verify_time(tag: &str, n: usize) -> std::time::Duration {
    let (p, root) = bounds_root(tag);
    let msgs = Pages::new(n).msgs;
    let ids: Vec<u64> = msgs.iter().map(|m| m.id.get()).collect();
    let dir = write_raw_archive(&root, "a", &render_transcript(7, &msgs, &[]), &ids);
    let best = (0..3)
        .map(|_| {
            let t = std::time::Instant::now();
            verify(&root, &dir, None, &ids).unwrap();
            t.elapsed()
        })
        .min()
        .unwrap();
    let _ = std::fs::remove_dir_all(p);
    best
}

/// Finding 1: `verify` must scale linearly with transcript size. Compares 5,000 vs 20,000
/// messages (4x); a quadratic scan costs ~16x, a single pass ~4x. The bound (<10x) leaves wide
/// margin for noise and avoids any absolute wall-clock threshold.
#[test]
fn verify_scales_linearly_with_message_count() {
    let small = verify_time("scale-s", 5_000);
    let large = verify_time("scale-l", 20_000);
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 10.0,
        "verify looks super-linear: 4x messages took {ratio:.1}x ({small:?} -> {large:?})"
    );
}

/// Verify still fails closed for a missing id and for an id that is only a prefix of a real one.
#[test]
fn verify_still_rejects_missing_and_partial_ids() {
    let (p, root) = bounds_root("ids");
    let msgs = Pages::new(300).msgs;
    let ids: Vec<u64> = msgs.iter().map(|m| m.id.get()).collect();
    let dir = write_raw_archive(&root, "a", &render_transcript(7, &msgs, &[]), &ids);
    assert!(verify(&root, &dir, None, &ids).is_ok());
    let mut missing = ids.clone();
    *missing.last_mut().unwrap() = 99_999_999;
    assert_eq!(
        verify(&root, &dir, None, &missing).unwrap_err(),
        ArchiveError::Verify
    );
    // 1000 is a prefix of the real id 10000 but never an exact data-mid value.
    let mut prefix = ids.clone();
    prefix[0] = 1000;
    assert_eq!(
        verify(&root, &dir, None, &prefix).unwrap_err(),
        ArchiveError::Verify
    );
    let _ = std::fs::remove_dir_all(p);
}

/// Verbatim transcript from the unmodified writer for `Pages::new(2)` (ticket 7), captured
/// before the bounds change. Pins that existing archives still verify and that short messages
/// render byte-identically.
const LEGACY_TRANSCRIPT: &str = "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\"><title>Ticket 7 transcript</title><style>body{font-family:sans-serif}.m{border-bottom:1px solid #ccc;padding:6px}.meta{color:#555;font-size:small}pre{white-space:pre-wrap}</style></head><body>\n<h1>Ticket 7</h1><p>2 messages</p>\n<div class=\"m\" data-mid=\"10000\"><div class=\"meta\">user_0 (id 1) at 2026-01-01T00:00:00.000Z</div><pre>synthetic message number 0</pre></div>\n<div class=\"m\" data-mid=\"10001\"><div class=\"meta\">user_1 (id 1) at 2026-01-01T00:00:01.000Z</div><pre>synthetic message number 1</pre></div>\n</body></html>\n";

/// Finding 2 (compat): an archive written by the old writer verifies, and the new renderer
/// emits the same bytes for short messages.
#[test]
fn legacy_archive_still_verifies_and_short_messages_render_identically() {
    let (p, root) = bounds_root("legacy");
    let dir = write_raw_archive(&root, "a", LEGACY_TRANSCRIPT, &[10000, 10001]);
    assert!(verify(&root, &dir, None, &[10000, 10001]).is_ok());
    assert_eq!(
        render_transcript(7, &Pages::new(2).msgs, &[]),
        LEGACY_TRANSCRIPT
    );
    let _ = std::fs::remove_dir_all(p);
}

/// Finding 2: one oversized message is cut to the per-message cap plus the stable marker, and
/// the published archive still verifies.
#[tokio::test]
async fn oversized_message_is_truncated_with_marker_and_archive_verifies() {
    let (p, root) = bounds_root("big");
    let (mut m, d) = build(3, false).await;
    m[1].content = "A".repeat(MAX_MESSAGE_TEXT_BYTES * 10);
    let out = publish(&root, &input(&m, &d, 1)).unwrap();
    let dir = root.path().join(&out.dir_name);
    let html = std::fs::read_to_string(dir.join("transcript.html")).unwrap();
    assert!(html.contains(&"A".repeat(MAX_MESSAGE_TEXT_BYTES)));
    assert_eq!(html.matches(TRUNCATION_MARKER).count(), 1);
    assert!(
        html.len() < MAX_MESSAGE_TEXT_BYTES * 2,
        "len {}",
        html.len()
    );
    let ids: Vec<u64> = m.iter().map(|x| x.id.get()).collect();
    assert!(verify(&root, &dir, Some(&out.manifest_sha256), &ids).is_ok());
    let _ = std::fs::remove_dir_all(p);
}

/// Truncation never splits a multi-byte character; output stays bounded and valid UTF-8.
#[test]
fn truncation_respects_char_boundaries() {
    let mut msgs = Pages::new(1).msgs;
    msgs[0].content = "é".repeat(MAX_MESSAGE_TEXT_BYTES); // 2 bytes per char
    let h = render_transcript(7, &msgs, &[]);
    assert!(h.contains(TRUNCATION_MARKER));
    assert!(h.len() < MAX_MESSAGE_TEXT_BYTES + 4096);
}

/// Finding 2: many maximal messages are bounded by the total transcript text cap, every message
/// stays listed (ids still verify), and the cut-off messages carry the marker.
#[tokio::test]
async fn total_transcript_text_is_capped_and_all_ids_remain() {
    let (p, root) = bounds_root("total");
    let n = MAX_TRANSCRIPT_TEXT_BYTES / MAX_MESSAGE_TEXT_BYTES + 40;
    let (mut m, d) = build(n, false).await;
    for x in m.iter_mut() {
        x.content = "B".repeat(MAX_MESSAGE_TEXT_BYTES);
    }
    let out = publish(&root, &input(&m, &d, 1)).unwrap();
    let dir = root.path().join(&out.dir_name);
    let html = std::fs::read_to_string(dir.join("transcript.html")).unwrap();
    assert!(
        html.len() < MAX_TRANSCRIPT_TEXT_BYTES + n * 1024,
        "transcript {} bytes not bounded",
        html.len()
    );
    assert!(html.contains(TRUNCATION_MARKER));
    let ids: Vec<u64> = m.iter().map(|x| x.id.get()).collect();
    assert_eq!(out.messages as usize, n);
    assert!(verify(&root, &dir, Some(&out.manifest_sha256), &ids).is_ok());
    let _ = std::fs::remove_dir_all(p);
}

/// Truncation is a rendering bound only: the content fingerprint (compared against the live
/// channel before deletion) still covers the full, untruncated text.
#[test]
fn fingerprint_still_covers_untruncated_text() {
    let mut a = Pages::new(1).msgs;
    let mut b = a.clone();
    a[0].content = format!("{}X", "A".repeat(MAX_MESSAGE_TEXT_BYTES * 2));
    b[0].content = format!("{}Y", "A".repeat(MAX_MESSAGE_TEXT_BYTES * 2));
    assert_ne!(content_fingerprint(&a), content_fingerprint(&b));
}

/// Text inside the first `<pre>` of a rendered transcript.
fn first_pre(h: &str) -> &str {
    let a = h.find("<pre>").unwrap() + 5;
    &h[a..a + h[a..].find("</pre>").unwrap()]
}

/// Escaping amplifies (`"` -> 6 bytes, `&` -> 5): the caps count ESCAPED bytes, so one message
/// of escaping characters renders to at most the per-message cap plus the marker.
#[test]
fn per_message_cap_counts_escaped_bytes() {
    for (ch, _) in [('"', 6), ('&', 5), ('\'', 5), ('<', 4)] {
        let mut msgs = Pages::new(1).msgs;
        msgs[0].content = ch.to_string().repeat(MAX_MESSAGE_TEXT_BYTES * 2);
        let h = render_transcript(7, &msgs, &[]);
        let pre = first_pre(&h);
        assert!(pre.ends_with(TRUNCATION_MARKER), "{ch}: marker missing");
        assert!(
            pre.len() <= MAX_MESSAGE_TEXT_BYTES + TRUNCATION_MARKER.len(),
            "{ch}: escaped text {} bytes over cap",
            pre.len()
        );
    }
}

/// Total cap over many all-quote / all-ampersand messages is enforced on the RENDERED length.
#[test]
fn total_cap_counts_escaped_bytes_for_quote_and_ampersand() {
    for ch in ['"', '&'] {
        let n = MAX_TRANSCRIPT_TEXT_BYTES / MAX_MESSAGE_TEXT_BYTES + 40;
        let mut msgs = Pages::new(n).msgs;
        for x in msgs.iter_mut() {
            x.content = ch.to_string().repeat(MAX_MESSAGE_TEXT_BYTES);
        }
        let h = render_transcript(7, &msgs, &[]);
        assert!(
            h.len() < MAX_TRANSCRIPT_TEXT_BYTES + n * 1024,
            "{ch}: rendered {} bytes not bounded",
            h.len()
        );
        assert!(h.len() < 64 * 1024 * 1024);
        assert!(h.contains(TRUNCATION_MARKER));
        for x in &msgs {
            assert!(h.contains(&format!("data-mid=\"{}\"", x.id.get())));
        }
    }
}

/// Never cut inside an escape sequence: a 6-byte unit with only 5 bytes left is dropped whole.
#[test]
fn truncation_never_splits_an_escape_sequence() {
    let mut msgs = Pages::new(1).msgs;
    msgs[0].content = format!("{}\"", "a".repeat(MAX_MESSAGE_TEXT_BYTES - 5));
    let h = render_transcript(7, &msgs, &[]);
    let pre = first_pre(&h);
    assert!(!pre.contains('&'), "partial or whole escape kept: {pre:.20}");
    assert!(pre.ends_with(TRUNCATION_MARKER));
    // Exactly filling the budget is not a cut: no marker.
    msgs[0].content = format!("{}\"", "a".repeat(MAX_MESSAGE_TEXT_BYTES - 6));
    let h = render_transcript(7, &msgs, &[]);
    let pre = first_pre(&h);
    assert!(pre.ends_with("&quot;"));
    assert!(!pre.contains(TRUNCATION_MARKER));
}

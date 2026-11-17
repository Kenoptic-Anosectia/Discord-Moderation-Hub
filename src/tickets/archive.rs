//! Verified ticket archive: exhaustive history fetch, safe offline transcript, bounded
//! attachment download, atomic publication into an owned root, and byte-level re-verification.
//!
//! Errors are fixed kinds only; nothing attacker-controlled (URLs, names, bodies) is carried.
use crate::archive_safety::{generated_asset_name, validate_asset_component, validate_attachment_url};
use serde::{Deserialize, Serialize};
use serenity::all::Message;
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveError {
    Source,       // history page fetch failed
    Order,        // page ordering/duplicate/boundary violation
    Changed,      // channel changed after exhaustion (cutoff/stability check)
    TooLarge,     // count / byte limits
    UnsafeInput,  // A10 validator rejected a URL/name
    Download,     // transport/status/redirect/timeout
    SizeMismatch, // declared vs actual
    Io,           // filesystem / disk full / publication fault
    Unsafe,       // symlink/junction/escape detected
    Verify,       // reopen/reverify mismatch
    Exists,       // final archive directory already exists (never overwritten)
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_messages: usize,
    pub max_asset_bytes: u64,
    pub max_assets: usize,
    pub max_total_bytes: u64,
    pub download_timeout: std::time::Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_messages: 200_000,
            max_asset_bytes: 25 * 1024 * 1024,
            max_assets: 500,
            max_total_bytes: 512 * 1024 * 1024,
            download_timeout: std::time::Duration::from_secs(60),
        }
    }
}

// ---------- history ----------
pub trait HistorySource: Send + Sync {
    /// Up to 100 messages with id > `after` (any order within the page).
    fn page_after(&self, after: u64) -> impl std::future::Future<Output = Result<Vec<Message>, ArchiveError>> + Send;
}
/// Fetch ALL pages to exhaustion in ascending id order. Detects duplicates, regressions and
/// (after exhaustion) a stability probe: any newer message aborts rather than claiming atomicity.
pub async fn fetch_all(src: &impl HistorySource, limits: &Limits) -> Result<Vec<Message>, ArchiveError> {
    let mut out: Vec<Message> = Vec::new();
    let mut cursor = 0u64;
    loop {
        let mut page = src.page_after(cursor).await?;
        if page.is_empty() {
            break;
        }
        page.sort_by_key(|m| m.id.get());
        for w in page.windows(2) {
            if w[0].id == w[1].id {
                return Err(ArchiveError::Order);
            }
        }
        if page[0].id.get() <= cursor {
            return Err(ArchiveError::Order); // overlap / non-advancing boundary
        }
        cursor = page.last().map(|m| m.id.get()).unwrap_or(cursor);
        out.extend(page);
        if out.len() > limits.max_messages {
            return Err(ArchiveError::TooLarge);
        }
    }
    // stability probe: nothing may have appeared after the cursor
    if !src.page_after(cursor).await?.is_empty() {
        return Err(ArchiveError::Changed);
    }
    Ok(out)
}

// ---------- transcript ----------
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            '\0' => {}
            c => o.push(c),
        }
    }
    o
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetEntry {
    pub attachment_id: u64,
    pub message_id: u64,
    pub file: String,
    pub len: u64,
    pub sha256: String,
    pub display_name: String,
    pub declared_size: u64,
}
/// Deterministic, self-contained, script-free transcript. Links are plain text except local
/// relative asset hrefs generated from ids; no remote resources are ever referenced.
pub fn render_transcript(ticket: i64, messages: &[Message], assets: &[AssetEntry]) -> String {
    let mut h = String::new();
    h.push_str("<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">");
    h.push_str(&format!("<title>Ticket {ticket} transcript</title>"));
    h.push_str("<style>body{font-family:sans-serif}.m{border-bottom:1px solid #ccc;padding:6px}.meta{color:#555;font-size:small}pre{white-space:pre-wrap}</style></head><body>\n");
    h.push_str(&format!("<h1>Ticket {ticket}</h1><p>{} messages</p>\n", messages.len()));
    for m in messages {
        h.push_str(&format!(
            "<div class=\"m\" data-mid=\"{}\"><div class=\"meta\">{} (id {}{}) at {}",
            m.id.get(),
            esc(&m.author.name),
            m.author.id.get(),
            if m.author.bot { ", bot" } else { "" },
            esc(&m.timestamp.to_string())
        ));
        if let Some(e) = m.edited_timestamp {
            h.push_str(&format!(" (edited {})", esc(&e.to_string())));
        }
        h.push_str("</div>");
        h.push_str(&format!("<pre>{}</pre>", esc(&m.content)));
        for e in &m.embeds {
            h.push_str("<blockquote>");
            for part in [e.title.as_deref(), e.description.as_deref(), e.url.as_deref(), e.footer.as_ref().map(|f| f.text.as_str())].into_iter().flatten() {
                h.push_str(&format!("<div>{}</div>", esc(part)));
            }
            for f in &e.fields {
                h.push_str(&format!("<div><b>{}</b>: {}</div>", esc(&f.name), esc(&f.value)));
            }
            h.push_str("</blockquote>");
        }
        for a in &m.attachments {
            match assets.iter().find(|x| x.attachment_id == a.id.get() && x.message_id == m.id.get()) {
                Some(x) => h.push_str(&format!(
                    "<div class=\"meta\">attachment: {} ({} bytes) <a href=\"assets/{}\">{}</a></div>",
                    esc(&a.filename), x.len, esc(&x.file), esc(&x.file)
                )),
                None => h.push_str(&format!("<div class=\"meta\">attachment: {} (not archived)</div>", esc(&a.filename))),
            }
        }
        h.push_str("</div>\n");
    }
    h.push_str("</body></html>\n");
    h
}

// ---------- hashing / manifest ----------
pub fn sha256_hex(b: &[u8]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, b);
    d.as_ref().iter().map(|x| format!("{x:02x}")).collect()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: u32,
    pub ticket: i64,
    pub channel: u64,
    pub message_count: u64,
    pub first_message: u64,
    pub last_message: u64,
    pub message_ids_sha256: String,
    pub transcript_len: u64,
    pub transcript_sha256: String,
    pub assets: Vec<AssetEntry>,
}
fn ids_hash(messages: &[Message]) -> String {
    let mut s = String::new();
    for m in messages {
        s.push_str(&m.id.get().to_string());
        s.push('\n');
    }
    sha256_hex(s.as_bytes())
}

// ---------- attachments ----------
pub trait Downloader: Send + Sync {
    /// Fetch a validated URL with redirects DISABLED; abort once `limit` bytes are exceeded.
    fn fetch(&self, url: &url::Url, limit: u64) -> impl std::future::Future<Output = Result<Vec<u8>, ArchiveError>> + Send;
}
pub struct Downloaded {
    pub entry: AssetEntry,
    pub bytes: Vec<u8>,
}
/// Download every attachment under count/size/total limits. The initial URL goes through the
/// A10 validator; the production downloader never follows redirects.
pub async fn download_all(dl: &impl Downloader, messages: &[Message], limits: &Limits) -> Result<Vec<Downloaded>, ArchiveError> {
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut seen = BTreeSet::new();
    for m in messages {
        for a in &m.attachments {
            if !seen.insert(a.id.get()) {
                return Err(ArchiveError::Order);
            }
            if out.len() >= limits.max_assets {
                return Err(ArchiveError::TooLarge);
            }
            let url = validate_attachment_url(&a.url).map_err(|_| ArchiveError::UnsafeInput)?;
            let name = generated_asset_name(a.id.get()).map_err(|_| ArchiveError::UnsafeInput)?;
            let declared = u64::from(a.size);
            if declared > limits.max_asset_bytes || total.saturating_add(declared) > limits.max_total_bytes {
                return Err(ArchiveError::TooLarge);
            }
            let bytes = dl.fetch(&url, declared.min(limits.max_asset_bytes)).await?;
            if bytes.len() as u64 != declared {
                return Err(ArchiveError::SizeMismatch);
            }
            total += bytes.len() as u64;
            out.push(Downloaded {
                entry: AssetEntry {
                    attachment_id: a.id.get(),
                    message_id: m.id.get(),
                    file: name,
                    len: bytes.len() as u64,
                    sha256: sha256_hex(&bytes),
                    display_name: a.filename.clone(),
                    declared_size: declared,
                },
                bytes,
            });
        }
    }
    Ok(out)
}

/// Production downloader: reqwest, rustls, redirects disabled, per-request timeout, streamed
/// with a hard byte cap (no unbounded allocation), HTTPS CDN allow-list already enforced by caller.
pub struct HttpDownloader {
    client: reqwest::Client,
}
impl HttpDownloader {
    pub fn new(timeout: std::time::Duration) -> Result<Self, ArchiveError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .https_only(true)
            .build()
            .map_err(|_| ArchiveError::Download)?;
        Ok(Self { client })
    }
}
impl Downloader for HttpDownloader {
    async fn fetch(&self, url: &url::Url, limit: u64) -> Result<Vec<u8>, ArchiveError> {
        // Re-validate here too: defense in depth for any future caller.
        validate_attachment_url(url.as_str()).map_err(|_| ArchiveError::UnsafeInput)?;
        let mut resp = self.client.get(url.clone()).send().await.map_err(|_| ArchiveError::Download)?;
        if !resp.status().is_success() {
            return Err(ArchiveError::Download); // includes every 3xx: redirects are never followed
        }
        if resp.content_length().is_some_and(|l| l > limit) {
            return Err(ArchiveError::TooLarge);
        }
        let mut buf = Vec::with_capacity(limit.min(1 << 20) as usize);
        while let Some(chunk) = resp.chunk().await.map_err(|_| ArchiveError::Download)? {
            if (buf.len() + chunk.len()) as u64 > limit {
                return Err(ArchiveError::TooLarge);
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }
}

// ---------- owned root, atomic publication, verification ----------
/// A canonicalised directory the bot owns. Everything is created beneath it with
/// create-new semantics and re-checked against the canonical root.
///
/// BOUND: without handle-relative openat (not available in std on Windows) a concurrent
/// same-account process that swaps a directory for a junction between our checks can still
/// race us. We bound this by (1) refusing a symlinked root, (2) create_new for every file and
/// directory, (3) re-canonicalising each directory immediately after creation and before
/// the final rename, and (4) byte re-verification before any source deletion.
pub struct OwnedRoot {
    root: PathBuf,
}
impl OwnedRoot {
    pub fn open(path: &Path) -> Result<Self, ArchiveError> {
        std::fs::create_dir_all(path).map_err(|_| ArchiveError::Io)?;
        let md = std::fs::symlink_metadata(path).map_err(|_| ArchiveError::Io)?;
        if md.file_type().is_symlink() || !md.is_dir() {
            return Err(ArchiveError::Unsafe);
        }
        Ok(Self { root: std::fs::canonicalize(path).map_err(|_| ArchiveError::Io)? })
    }
    pub fn path(&self) -> &Path {
        &self.root
    }
    fn contained(&self, p: &Path) -> Result<(), ArchiveError> {
        let c = std::fs::canonicalize(p).map_err(|_| ArchiveError::Unsafe)?;
        if c.starts_with(&self.root) {
            Ok(())
        } else {
            Err(ArchiveError::Unsafe)
        }
    }
    fn write_new(&self, dir: &Path, name: &str, bytes: &[u8]) -> Result<(), ArchiveError> {
        self.contained(dir)?;
        let p = dir.join(name);
        let mut f = OpenOptions::new().write(true).create_new(true).open(&p).map_err(|_| ArchiveError::Io)?;
        f.write_all(bytes).map_err(|_| ArchiveError::Io)?;
        f.sync_all().map_err(|_| ArchiveError::Io)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
    pub dir_name: String,
    pub manifest_sha256: String,
    pub messages: u64,
    pub assets: u64,
}
pub struct PublishInput<'a> {
    pub ticket: i64,
    pub channel: u64,
    pub seq: i64,
    pub nonce: u64,
    pub messages: &'a [Message],
    pub downloaded: &'a [Downloaded],
}
/// Stage into `.staging-*`, verify the staged bytes, atomically rename to
/// `ticket-<id>-<seq>`, then re-verify the FINAL location. An existing final directory is never
/// touched (a retry uses the next sequence). Staging debris is removed on failure.
pub fn publish(root: &OwnedRoot, i: &PublishInput<'_>) -> Result<Published, ArchiveError> {
    let entries: Vec<AssetEntry> = i.downloaded.iter().map(|d| d.entry.clone()).collect();
    let transcript = render_transcript(i.ticket, i.messages, &entries);
    let manifest = Manifest {
        format: 1,
        ticket: i.ticket,
        channel: i.channel,
        message_count: i.messages.len() as u64,
        first_message: i.messages.first().map(|m| m.id.get()).unwrap_or(0),
        last_message: i.messages.last().map(|m| m.id.get()).unwrap_or(0),
        message_ids_sha256: ids_hash(i.messages),
        transcript_len: transcript.len() as u64,
        transcript_sha256: sha256_hex(transcript.as_bytes()),
        assets: entries,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|_| ArchiveError::Io)?;
    let manifest_sha = sha256_hex(&manifest_bytes);
    let final_name = format!("ticket-{}-{}", i.ticket, i.seq);
    let final_dir = root.path().join(&final_name);
    if std::fs::symlink_metadata(&final_dir).is_ok() {
        return Err(ArchiveError::Exists);
    }
    let staging = root.path().join(format!(".staging-{}-{}-{}", i.ticket, i.seq, i.nonce));
    let result = (|| {
        std::fs::create_dir(&staging).map_err(|_| ArchiveError::Io)?;
        root.contained(&staging)?;
        root.write_new(&staging, "transcript.html", transcript.as_bytes())?;
        root.write_new(&staging, "manifest.json", &manifest_bytes)?;
        if !i.downloaded.is_empty() {
            let assets = staging.join("assets");
            std::fs::create_dir(&assets).map_err(|_| ArchiveError::Io)?;
            for d in i.downloaded {
                validate_asset_component(&d.entry.file).map_err(|_| ArchiveError::UnsafeInput)?;
                root.write_new(&assets, &d.entry.file, &d.bytes)?;
            }
        }
        let ids: Vec<u64> = i.messages.iter().map(|m| m.id.get()).collect();
        verify(root, &staging, Some(&manifest_sha), &ids)?;
        root.contained(&staging)?;
        std::fs::rename(&staging, &final_dir).map_err(|_| ArchiveError::Io)?; // fails if final exists on Windows
        verify(root, &final_dir, Some(&manifest_sha), &ids)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        return result.map(|_| unreachable!());
    }
    Ok(Published { dir_name: final_name, manifest_sha256: manifest_sha, messages: manifest.message_count, assets: manifest.assets.len() as u64 })
}

/// Reopen the directory and verify the REAL bytes: manifest hash, transcript length/hash, every
/// asset length/hash, no extra/foreign entries, no symlinks, and every expected message id present.
pub fn verify(root: &OwnedRoot, dir: &Path, manifest_sha: Option<&str>, expected_ids: &[u64]) -> Result<Manifest, ArchiveError> {
    root.contained(dir).map_err(|_| ArchiveError::Unsafe)?;
    let regular = |p: &Path| -> Result<(), ArchiveError> {
        let md = std::fs::symlink_metadata(p).map_err(|_| ArchiveError::Verify)?;
        if md.file_type().is_symlink() {
            return Err(ArchiveError::Unsafe);
        }
        if !md.is_file() {
            return Err(ArchiveError::Verify);
        }
        Ok(())
    };
    let mp = dir.join("manifest.json");
    regular(&mp)?;
    let mbytes = std::fs::read(&mp).map_err(|_| ArchiveError::Verify)?;
    if let Some(want) = manifest_sha {
        if sha256_hex(&mbytes) != want {
            return Err(ArchiveError::Verify);
        }
    }
    let m: Manifest = serde_json::from_slice(&mbytes).map_err(|_| ArchiveError::Verify)?;
    let tp = dir.join("transcript.html");
    regular(&tp)?;
    let tbytes = std::fs::read(&tp).map_err(|_| ArchiveError::Verify)?;
    if tbytes.len() as u64 != m.transcript_len || sha256_hex(&tbytes) != m.transcript_sha256 {
        return Err(ArchiveError::Verify);
    }
    let html = String::from_utf8_lossy(&tbytes);
    for id in expected_ids {
        if !html.contains(&format!("data-mid=\"{id}\"")) {
            return Err(ArchiveError::Verify);
        }
    }
    if m.message_count != expected_ids.len() as u64 {
        return Err(ArchiveError::Verify);
    }
    let mut expected_files: BTreeSet<String> = ["manifest.json".to_string(), "transcript.html".to_string()].into();
    if !m.assets.is_empty() {
        expected_files.insert("assets".into());
        let ad = dir.join("assets");
        let md = std::fs::symlink_metadata(&ad).map_err(|_| ArchiveError::Verify)?;
        if md.file_type().is_symlink() || !md.is_dir() {
            return Err(ArchiveError::Unsafe);
        }
        let mut names = BTreeSet::new();
        for e in std::fs::read_dir(&ad).map_err(|_| ArchiveError::Verify)? {
            names.insert(e.map_err(|_| ArchiveError::Verify)?.file_name().to_string_lossy().into_owned());
        }
        let want: BTreeSet<String> = m.assets.iter().map(|a| a.file.clone()).collect();
        if names != want || want.len() != m.assets.len() {
            return Err(ArchiveError::Verify);
        }
        for a in &m.assets {
            validate_asset_component(&a.file).map_err(|_| ArchiveError::UnsafeInput)?;
            let p = ad.join(&a.file);
            regular(&p)?;
            let b = std::fs::read(&p).map_err(|_| ArchiveError::Verify)?;
            if b.len() as u64 != a.len || a.len != a.declared_size || sha256_hex(&b) != a.sha256 {
                return Err(ArchiveError::Verify);
            }
        }
    }
    let mut actual = BTreeSet::new();
    for e in std::fs::read_dir(dir).map_err(|_| ArchiveError::Verify)? {
        actual.insert(e.map_err(|_| ArchiveError::Verify)?.file_name().to_string_lossy().into_owned());
    }
    if actual != expected_files {
        return Err(ArchiveError::Verify);
    }
    Ok(m)
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;

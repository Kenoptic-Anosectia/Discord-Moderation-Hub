//! Verified ticket archiving: exhaustive message history fetch, static offline HTML transcript
//! rendering, bounded attachment download, atomic directory publication, and byte-level verification
//! (`docs/verification/TICKET-ARCHIVE-CLOSE.md`).
//!
//! Responsible for turning a ticket channel's message stream and attachments into an immutable,
//! SHA-256-verified on-disk bundle (`manifest.json`, `transcript.html`, `assets/attachment-<id>.bin`).
//! Deliberately does not mutate SQLite state, delete Discord channels, or follow HTTP redirects.
//! Trust boundary: message bodies, author names, embeds, attachment URLs, filenames, and downloaded
//! bytes are untrusted; HTML output is escaped under a script-free CSP, on-disk paths use numeric IDs
//! inside [`OwnedRoot`], and [`ArchiveError`] carries no untrusted strings.
use crate::archive_safety::{
    generated_asset_name, validate_asset_component, validate_attachment_url,
};
use serde::{Deserialize, Serialize};
use serenity::all::Message;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Fixed failure category for history fetching, attachment downloading, publication, and verification.
///
/// Carries no dynamic payload so callers can log or persist the failure without leaking
/// private ticket text, CDN tokens, or filesystem paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveError {
    /// History page fetch from the upstream source failed.
    Source, // history page fetch failed
    /// Duplicate ID, non-increasing page boundary, or duplicate attachment ID across messages.
    Order, // page ordering/duplicate/boundary violation
    /// Post-exhaustion stability probe found new or altered channel state.
    Changed, // channel changed after exhaustion (cutoff/stability check)
    /// Message count, asset count, single-asset size, or total archive byte cap exceeded.
    TooLarge, // count / byte limits
    /// Attachment URL or asset filename was rejected by [`crate::archive_safety`].
    UnsafeInput, // A10 validator rejected a URL/name
    /// HTTP transport error, non-2xx status (including 3xx redirects), deadline, or Content-Length mismatch.
    Download, // transport/status/redirect/timeout
    /// Downloaded attachment byte count did not match the Discord message metadata size.
    SizeMismatch, // declared vs actual
    /// Filesystem creation, write, sync, or atomic rename failure.
    Io, // filesystem / disk full / publication fault
    /// Symlink, non-directory root, or canonical path escaping [`OwnedRoot`] detected.
    Unsafe, // symlink/junction/escape detected
    /// Re-opened archive failed manifest, transcript, asset hash/length, or directory entry checks.
    Verify, // reopen/reverify mismatch
    /// Target `ticket-<id>-<seq>` directory already exists and is never overwritten.
    Exists, // final archive directory already exists (never overwritten)
}

/// Resource and timeout bounds enforced during message history collection and attachment downloading.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum number of messages permitted in a single ticket archive.
    pub max_messages: usize,
    /// Maximum byte size permitted for any single attachment.
    pub max_asset_bytes: u64,
    /// Maximum number of attachments permitted across the entire ticket.
    pub max_assets: usize,
    /// Maximum cumulative attachment bytes permitted across the entire ticket.
    pub max_total_bytes: u64,
    /// Per-attachment wall-clock deadline applied to [`HttpDownloader::fetch`].
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

/// Per-message cap in ESCAPED output bytes (content plus embed text, after HTML escaping, so a
/// quote-only message cannot expand 6x past it). Discord allows about 60 KiB of text per message,
/// so 256 KiB never cuts a legitimate message but bounds an abusive one.
pub const MAX_MESSAGE_TEXT_BYTES: usize = 256 * 1024;
/// Total cap in ESCAPED output bytes of message text in one transcript, so the rendered transcript
/// stays well under 64 MiB (per-message markup is extra); later messages stay listed
/// (id, author, time) but their text is replaced by [`TRUNCATION_MARKER`].
pub const MAX_TRANSCRIPT_TEXT_BYTES: usize = 32 * 1024 * 1024;
/// Stable plain-ASCII marker appended where text was cut; needs no escaping and can never form
/// the `data-mid="` attribute that `verify` indexes.
pub const TRUNCATION_MARKER: &str = "[truncated: archive text limit]";

// ---------- history ----------
/// Paginated read-only message history provider for a single Discord ticket channel.
///
/// Implementations must return only messages from the target channel with `id > after`
/// and map transport or gateway errors to [`ArchiveError::Source`].
pub trait HistorySource: Send + Sync {
    /// Up to 100 messages with id > `after` (any order within the page).
    fn page_after(
        &self,
        after: u64,
    ) -> impl std::future::Future<Output = Result<Vec<Message>, ArchiveError>> + Send;
}
/// Fetches all message pages from `src` in strictly ascending ID order until exhaustion,
/// followed by one stability probe past the final cursor.
///
/// Caller must supply bounded [`Limits`]. Fails closed on source errors ([`ArchiveError::Source`]),
/// duplicate or non-advancing message IDs ([`ArchiveError::Order`]), exceeding `limits.max_messages`
/// ([`ArchiveError::TooLarge`]), or any message arriving during the final probe ([`ArchiveError::Changed`]).
pub async fn fetch_all(
    src: &impl HistorySource,
    limits: &Limits,
) -> Result<Vec<Message>, ArchiveError> {
    let mut out: Vec<Message> = Vec::new();
    let mut cursor = 0u64;
    loop {
        let mut page = src.page_after(cursor).await?;
        if page.is_empty() {
            break;
        }
        // Discord pages may arrive newest-first; sort ascending before checking monotonicity.
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
        // Enforce the message cap after each page so an oversized channel aborts before full collection.
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
/// Escapes `&`, `<`, `>`, `"`, and `'` into HTML entities and strips `\0` bytes.
///
/// Used on every untrusted string interpolated into `transcript.html` so user content
/// cannot inject markup, attributes, or scripts into the offline archive.
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
/// Per-message text budget: writes escaped text straight into the transcript (no intermediate
/// copies), cutting at a char boundary and appending [`TRUNCATION_MARKER`] once the budget is
/// spent. The budget counts raw (pre-escape) bytes, so escaped output is at most 6x the cap.
struct MsgText {
    start: usize,
    left: usize,
    /// True once this message hit its budget; later parts of the message are omitted.
    cut: bool,
}
impl MsgText {
    /// Budget is the per-message cap, further limited by what remains of the transcript total.
    fn new(total_left: usize) -> Self {
        let b = MAX_MESSAGE_TEXT_BYTES.min(total_left);
        Self {
            start: b,
            left: b,
            cut: false,
        }
    }
    /// Raw bytes consumed so far (to be deducted from the transcript total).
    fn used(&self) -> usize {
        self.start - self.left
    }
    /// Appends the HTML-escaped prefix of `s` that fits, then the marker if anything was cut.
    fn put(&mut self, h: &mut String, s: &str) {
        for c in s.chars() {
            let n = esc_len(c);
            if n > self.left {
                self.cut = true;
                self.left = 0;
                h.push_str(TRUNCATION_MARKER);
                return;
            }
            self.left -= n;
            esc_char(h, c);
        }
    }
}
/// Escaped byte length of one char (must match [`esc_char`]).
fn esc_len(c: char) -> usize {
    match c {
        '&' | '\'' => 5,
        '<' | '>' => 4,
        '"' => 6,
        '\0' => 0,
        c => c.len_utf8(),
    }
}
/// Appends the escaped form of one char (see [`esc`]) without allocating a temporary.
fn esc_char(o: &mut String, c: char) {
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
/// Manifest metadata record for a single archived attachment file under `assets/`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetEntry {
    /// Discord attachment snowflake ID (used to derive `file`).
    pub attachment_id: u64,
    /// Discord message snowflake ID that carried this attachment.
    pub message_id: u64,
    /// Canonical on-disk filename (`attachment-<attachment_id>.bin`), validated before use.
    pub file: String,
    /// Actual verified byte length written to disk.
    pub len: u64,
    /// Lowercase hex SHA-256 digest of the downloaded attachment bytes.
    pub sha256: String,
    /// Original untrusted Discord filename retained strictly as display metadata.
    pub display_name: String,
    /// Byte size declared by Discord's message attachment metadata (`len` must match).
    pub declared_size: u64,
}
/// Renders a deterministic, self-contained, script-free HTML transcript for `ticket`.
///
/// Caller must pass `messages` in ascending ID order and `assets` whose `file` names came from
/// [`generated_asset_name`]. Infallible; embeds a `default-src 'none'` Content-Security-Policy,
/// HTML-escapes all message/embed/attachment fields, and links only local relative `assets/<file>` paths.
pub fn render_transcript(ticket: i64, messages: &[Message], assets: &[AssetEntry]) -> String {
    let mut h = String::new();
    h.push_str("<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">");
    h.push_str(&format!("<title>Ticket {ticket} transcript</title>"));
    h.push_str("<style>body{font-family:sans-serif}.m{border-bottom:1px solid #ccc;padding:6px}.meta{color:#555;font-size:small}pre{white-space:pre-wrap}</style></head><body>\n");
    h.push_str(&format!(
        "<h1>Ticket {ticket}</h1><p>{} messages</p>\n",
        messages.len()
    ));
    // First entry wins, matching the previous linear `find`.
    let mut by_attachment: HashMap<(u64, u64), &AssetEntry> = HashMap::new();
    for x in assets {
        by_attachment
            .entry((x.attachment_id, x.message_id))
            .or_insert(x);
    }
    let mut total_left = MAX_TRANSCRIPT_TEXT_BYTES;
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
        let mut t = MsgText::new(total_left);
        h.push_str("<pre>");
        t.put(&mut h, &m.content);
        h.push_str("</pre>");
        for e in &m.embeds {
            h.push_str("<blockquote>");
            for part in [
                e.title.as_deref(),
                e.description.as_deref(),
                e.url.as_deref(),
                e.footer.as_ref().map(|f| f.text.as_str()),
            ]
            .into_iter()
            .flatten()
            {
                if !t.cut {
                    h.push_str("<div>");
                    t.put(&mut h, part);
                    h.push_str("</div>");
                }
            }
            for f in &e.fields {
                if !t.cut {
                    h.push_str("<div><b>");
                    t.put(&mut h, &f.name);
                    h.push_str("</b>: ");
                    t.put(&mut h, &f.value);
                    h.push_str("</div>");
                }
            }
            h.push_str("</blockquote>");
        }
        total_left -= t.used();
        for a in &m.attachments {
            match by_attachment.get(&(a.id.get(), m.id.get())) {
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
/// Computes the lowercase 64-character hexadecimal SHA-256 digest of `b`.
pub fn sha256_hex(b: &[u8]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, b);
    d.as_ref().iter().map(|x| format!("{x:02x}")).collect()
}
/// Serializable `manifest.json` root record binding ticket identity, message digests, transcript, and assets.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// Schema version of the archive manifest (currently `1`).
    pub format: u32,
    /// Ticket database ID (`tickets.id`).
    pub ticket: i64,
    /// Discord channel snowflake ID of the archived ticket.
    pub channel: u64,
    /// Total number of messages recorded in the archive.
    pub message_count: u64,
    /// Snowflake ID of the earliest message (or `0` when `message_count == 0`).
    pub first_message: u64,
    /// Snowflake ID of the latest message (or `0` when `message_count == 0`).
    pub last_message: u64,
    /// SHA-256 hex digest of newline-delimited message IDs in archive order.
    pub message_ids_sha256: String,
    /// Digest of ids + content + edit stamps + attachment/embed identity at archive time; the
    /// pre-delete stability check compares the live channel against this.
    #[serde(default)]
    pub content_sha256: String,
    /// Exact byte length of `transcript.html`.
    pub transcript_len: u64,
    /// Lowercase hex SHA-256 digest of `transcript.html`.
    pub transcript_sha256: String,
    /// Metadata and digests for every archived attachment under `assets/`.
    pub assets: Vec<AssetEntry>,
}
/// Computes the SHA-256 hex digest of each message ID in `messages`, one decimal ID per line.
fn ids_hash(messages: &[Message]) -> String {
    let mut s = String::new();
    for m in messages {
        s.push_str(&m.id.get().to_string());
        s.push('\n');
    }
    sha256_hex(s.as_bytes())
}

/// Computes a canonical SHA-256 hex digest over all archived message fields (IDs, authors, bodies,
/// timestamps, edit timestamps, attachments, and embeds) sorted by message ID.
///
/// Length-prefixing every field (`<byte_len>:<value>\n`) prevents delimiter-collision ambiguity
/// so any edit, deletion, insertion, or attachment change before channel deletion alters the digest.
pub fn content_fingerprint(messages: &[Message]) -> String {
    let mut ms: Vec<&Message> = messages.iter().collect();
    ms.sort_by_key(|m| m.id.get());
    // Stream into the hasher instead of materializing the whole fingerprint input (same bytes).
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut put = |x: &str| {
        ctx.update(x.len().to_string().as_bytes());
        ctx.update(b":");
        ctx.update(x.as_bytes());
        ctx.update(b"\n");
    };
    for m in ms {
        put(&m.id.get().to_string());
        put(&m.author.id.get().to_string());
        put(&m.content);
        put(&m
            .edited_timestamp
            .map(|e| e.to_string())
            .unwrap_or_default());
        put(&m.timestamp.to_string());
        put(&m.author.name);
        put(if m.author.bot { "bot" } else { "user" });
        for a in &m.attachments {
            put(&format!(
                "a{}:{}:{}:{:?}",
                a.id.get(),
                a.size,
                a.filename,
                a.content_type
            ));
        }
        for e in &m.embeds {
            put(&format!(
                "e{:?}|{:?}|{:?}|{:?}|{:?}",
                e.title,
                e.description,
                e.url,
                e.footer.as_ref().map(|f| &f.text),
                e.fields
                    .iter()
                    .map(|f| (&f.name, &f.value))
                    .collect::<Vec<_>>()
            ));
        }
    }
    ctx.finish()
        .as_ref()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

// ---------- attachments ----------
/// Bounded attachment byte fetcher that refuses HTTP redirects and enforces a hard byte cap.
pub trait Downloader: Send + Sync {
    /// Fetch a validated URL with redirects DISABLED; abort once `limit` bytes are exceeded.
    fn fetch(
        &self,
        url: &url::Url,
        limit: u64,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, ArchiveError>> + Send;
}
/// In-memory downloaded attachment pairing its verified manifest [`AssetEntry`] with raw bytes.
pub struct Downloaded {
    /// Manifest entry carrying canonical filename, SHA-256 digest, and verified byte length.
    pub entry: AssetEntry,
    /// Raw attachment payload bytes matching `entry.len` and `entry.sha256`.
    pub bytes: Vec<u8>,
}
/// Validates and downloads every attachment across `messages` within [`Limits`].
///
/// Caller must supply `messages` in archive order and a non-redirecting [`Downloader`].
/// Fails closed on duplicate attachment IDs ([`ArchiveError::Order`]), invalid CDN URLs or zero IDs
/// ([`ArchiveError::UnsafeInput`]), asset count or byte cap violations ([`ArchiveError::TooLarge`]),
/// transport/status/timeout errors ([`ArchiveError::Download`]), or body length mismatch ([`ArchiveError::SizeMismatch`]).
pub async fn download_all(
    dl: &impl Downloader,
    messages: &[Message],
    limits: &Limits,
) -> Result<Vec<Downloaded>, ArchiveError> {
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut seen = BTreeSet::new();
    for m in messages {
        for a in &m.attachments {
            // Attachment IDs determine on-disk filenames (`attachment-<id>.bin`), so a duplicate ID would collide.
            if !seen.insert(a.id.get()) {
                return Err(ArchiveError::Order);
            }
            if out.len() >= limits.max_assets {
                return Err(ArchiveError::TooLarge);
            }
            // Validate URL and derive canonical ID-based filename before any network request.
            let url = validate_attachment_url(&a.url).map_err(|_| ArchiveError::UnsafeInput)?;
            let name = generated_asset_name(a.id.get()).map_err(|_| ArchiveError::UnsafeInput)?;
            let declared = u64::from(a.size);
            if declared > limits.max_asset_bytes
                || total.saturating_add(declared) > limits.max_total_bytes
            {
                return Err(ArchiveError::TooLarge);
            }
            let bytes = dl.fetch(&url, declared.min(limits.max_asset_bytes)).await?;
            // Require exact byte count match against Discord's message metadata to reject truncated or padded bodies.
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

/// Transport seam under the production downloader. The decision/streaming policy (status,
/// redirects, size caps, short bodies, deadline) lives in [`HttpDownloader::fetch`] and runs
/// identically over the real reqwest transport and a controlled no-network test transport.
pub trait Transport: Send + Sync {
    /// Streaming response type returned by [`Transport::get`].
    type Resp: TransportResp;
    /// Issues a single HTTP GET request for `url` without following redirects.
    fn get(
        &self,
        url: &url::Url,
    ) -> impl std::future::Future<Output = Result<Self::Resp, ArchiveError>> + Send;
}
/// Streaming HTTP response interface exposing status code, optional `Content-Length`, and body chunks.
pub trait TransportResp: Send {
    /// Returns the numeric HTTP status code.
    fn status(&self) -> u16;
    /// Returns the declared `Content-Length` header value, if present.
    fn content_length(&self) -> Option<u64>;
    /// Reads the next response body chunk, or `Ok(None)` at EOF.
    fn chunk(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, ArchiveError>> + Send;
}
/// Production [`Transport`] backed by a hardened `reqwest::Client` that refuses redirects and plaintext HTTP.
pub struct ReqwestTransport {
    client: reqwest::Client,
}
impl ReqwestTransport {
    /// Builds a `ReqwestTransport` configured with `timeout`, no redirect following, and HTTPS-only enforcement.
    ///
    /// Fails closed with [`ArchiveError::Download`] if TLS/client initialization fails.
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
impl TransportResp for reqwest::Response {
    fn status(&self) -> u16 {
        reqwest::Response::status(self).as_u16()
    }
    fn content_length(&self) -> Option<u64> {
        reqwest::Response::content_length(self)
    }
    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, ArchiveError> {
        reqwest::Response::chunk(self)
            .await
            .map(|c| c.map(|b| b.to_vec()))
            .map_err(|_| ArchiveError::Download)
    }
}
impl Transport for ReqwestTransport {
    type Resp = reqwest::Response;
    async fn get(&self, url: &url::Url) -> Result<reqwest::Response, ArchiveError> {
        self.client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| ArchiveError::Download)
    }
}

/// Production downloader: redirects are never followed (every non-2xx, including 3xx, fails
/// without a second request), a hard overall deadline, streamed with a hard byte cap (no
/// unbounded allocation) and a declared-length check. HTTPS CDN allow-list is re-validated.
pub struct HttpDownloader<T: Transport = ReqwestTransport> {
    transport: T,
    deadline: std::time::Duration,
}
impl HttpDownloader<ReqwestTransport> {
    /// Creates a production [`HttpDownloader`] using [`ReqwestTransport`] and per-fetch `timeout`.
    ///
    /// Fails closed with [`ArchiveError::Download`] if the underlying HTTP client cannot be constructed.
    pub fn new(timeout: std::time::Duration) -> Result<Self, ArchiveError> {
        Ok(Self::with_transport(
            ReqwestTransport::new(timeout)?,
            timeout,
        ))
    }
}
impl<T: Transport> HttpDownloader<T> {
    /// Wraps a custom or test [`Transport`] with the production streaming policy and `deadline`.
    pub fn with_transport(transport: T, deadline: std::time::Duration) -> Self {
        Self {
            transport,
            deadline,
        }
    }
    /// Streams `url` from `self.transport` while enforcing status, `Content-Length`, and chunk byte caps.
    ///
    /// Rejects any non-2xx status (including 3xx redirects), pre-checks `Content-Length` against `limit`,
    /// caps initial allocation at 1 MiB so an untrusted header cannot trigger a huge allocation,
    /// and verifies the final byte count against `Content-Length` when present.
    async fn fetch_inner(&self, url: &url::Url, limit: u64) -> Result<Vec<u8>, ArchiveError> {
        let mut resp = self.transport.get(url).await?;
        if !(200..300).contains(&resp.status()) {
            return Err(ArchiveError::Download); // includes every 3xx: redirects are never followed
        }
        let declared = resp.content_length();
        if declared.is_some_and(|l| l > limit) {
            return Err(ArchiveError::TooLarge);
        }
        let mut buf = Vec::with_capacity(limit.min(1 << 20) as usize);
        while let Some(chunk) = resp.chunk().await? {
            if (buf.len() + chunk.len()) as u64 > limit {
                return Err(ArchiveError::TooLarge);
            }
            buf.extend_from_slice(&chunk);
        }
        if declared.is_some_and(|l| l != buf.len() as u64) {
            return Err(ArchiveError::Download); // short (or long) body vs declared length
        }
        Ok(buf)
    }
}
impl<T: Transport> Downloader for HttpDownloader<T> {
    async fn fetch(&self, url: &url::Url, limit: u64) -> Result<Vec<u8>, ArchiveError> {
        // Re-validate here too: defense in depth for any future caller.
        validate_attachment_url(url.as_str()).map_err(|_| ArchiveError::UnsafeInput)?;
        match tokio::time::timeout(self.deadline, self.fetch_inner(url, limit)).await {
            Ok(r) => r,
            Err(_) => Err(ArchiveError::Download),
        }
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
    /// Creates `path` if absent, verifies via `symlink_metadata` that it is a real directory (not a symlink),
    /// and stores its canonicalized path as the containment root.
    ///
    /// Fails closed with [`ArchiveError::Unsafe`] if `path` is a symlink or non-directory, or
    /// [`ArchiveError::Io`] on filesystem failure.
    pub fn open(path: &Path) -> Result<Self, ArchiveError> {
        std::fs::create_dir_all(path).map_err(|_| ArchiveError::Io)?;
        let md = std::fs::symlink_metadata(path).map_err(|_| ArchiveError::Io)?;
        if md.file_type().is_symlink() || !md.is_dir() {
            return Err(ArchiveError::Unsafe);
        }
        Ok(Self {
            root: std::fs::canonicalize(path).map_err(|_| ArchiveError::Io)?,
        })
    }
    pub fn path(&self) -> &Path {
        &self.root
    }
    /// Verifies that `p` canonicalizes to a path inside `self.root`; fails closed with [`ArchiveError::Unsafe`]
    /// if canonicalization fails or if a symlink/junction redirected `p` outside the archive root.
    fn contained(&self, p: &Path) -> Result<(), ArchiveError> {
        let c = std::fs::canonicalize(p).map_err(|_| ArchiveError::Unsafe)?;
        if c.starts_with(&self.root) {
            Ok(())
        } else {
            Err(ArchiveError::Unsafe)
        }
    }
    /// Creates a brand-new file `name` inside `dir` with `create_new(true)`, writes `bytes`, and calls `sync_all`.
    ///
    /// Re-verifies containment of `dir` first and refuses to overwrite or follow an existing target file.
    fn write_new(&self, dir: &Path, name: &str, bytes: &[u8]) -> Result<(), ArchiveError> {
        self.contained(dir)?;
        let p = dir.join(name);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .map_err(|_| ArchiveError::Io)?;
        f.write_all(bytes).map_err(|_| ArchiveError::Io)?;
        f.sync_all().map_err(|_| ArchiveError::Io)?;
        Ok(())
    }
}

/// Summary returned after an archive bundle is staged, atomically published, and re-verified on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
    /// Final published directory name (`ticket-<ticket>-<seq>`) relative to [`OwnedRoot`].
    pub dir_name: String,
    /// Verified SHA-256 hex digest of the on-disk `manifest.json`.
    pub manifest_sha256: String,
    /// Total number of archived messages verified in `manifest.json` and `transcript.html`.
    pub messages: u64,
    /// Total number of archived attachment files verified under `assets/`.
    pub assets: u64,
}
/// Inputs required to stage, publish, and verify a single ticket archive attempt.
pub struct PublishInput<'a> {
    /// Ticket database ID (`tickets.id`).
    pub ticket: i64,
    /// Discord channel snowflake ID being archived.
    pub channel: u64,
    /// Monotonic per-ticket attempt sequence number (`ticket_archives.seq`).
    pub seq: i64,
    /// Per-attempt nonce differentiating `.staging-<ticket>-<seq>-<nonce>` directories.
    pub nonce: u64,
    /// Complete, ascending-order message history returned by [`fetch_all`].
    pub messages: &'a [Message],
    /// Verified attachment payloads returned by [`download_all`].
    pub downloaded: &'a [Downloaded],
}
/// Stage into `.staging-*`, verify the staged bytes, atomically rename to
/// `ticket-<id>-<seq>`, then re-verify the FINAL location. An existing final directory is never
/// touched (a retry uses the next sequence). Staging debris is removed on failure.
///
/// Caller must pass an [`OwnedRoot`] and pre-validated messages/downloads. Fails closed without
/// overwriting `ticket-<id>-<seq>` ([`ArchiveError::Exists`]) and cleans up `.staging-*` on any
/// stage write, rename, or verification error.
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
        content_sha256: content_fingerprint(i.messages),
        transcript_len: transcript.len() as u64,
        transcript_sha256: sha256_hex(transcript.as_bytes()),
        assets: entries,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|_| ArchiveError::Io)?;
    let manifest_sha = sha256_hex(&manifest_bytes);
    let final_name = format!("ticket-{}-{}", i.ticket, i.seq);
    let final_dir = root.path().join(&final_name);
    // Refuse early if the target directory or a broken symlink already exists; published archives are immutable.
    if std::fs::symlink_metadata(&final_dir).is_ok() {
        return Err(ArchiveError::Exists);
    }
    let staging = root
        .path()
        .join(format!(".staging-{}-{}-{}", i.ticket, i.seq, i.nonce));
    let result = (|| {
        std::fs::create_dir(&staging).map_err(|_| ArchiveError::Io)?;
        root.contained(&staging)?;
        root.write_new(&staging, "transcript.html", transcript.as_bytes())?;
        root.write_new(&staging, "manifest.json", &manifest_bytes)?;
        if !i.downloaded.is_empty() {
            let assets = staging.join("assets");
            std::fs::create_dir(&assets).map_err(|_| ArchiveError::Io)?;
            for d in i.downloaded {
                // Re-validate asset filename component before writing to disk.
                validate_asset_component(&d.entry.file).map_err(|_| ArchiveError::UnsafeInput)?;
                root.write_new(&assets, &d.entry.file, &d.bytes)?;
            }
        }
        let ids: Vec<u64> = i.messages.iter().map(|m| m.id.get()).collect();
        // Verify the staged bundle before exposing it at the final path, then re-verify after rename.
        verify(root, &staging, Some(&manifest_sha), &ids)?;
        root.contained(&staging)?;
        std::fs::rename(&staging, &final_dir).map_err(|_| ArchiveError::Io)?; // fails if final exists on Windows
        verify(root, &final_dir, Some(&manifest_sha), &ids)?;
        Ok(())
    })();
    if result.is_err() {
        // Best-effort cleanup of staging directory if staging or rename failed; never touches `final_dir`.
        let _ = std::fs::remove_dir_all(&staging);
        return result.map(|_| unreachable!());
    }
    Ok(Published {
        dir_name: final_name,
        manifest_sha256: manifest_sha,
        messages: manifest.message_count,
        assets: manifest.assets.len() as u64,
    })
}

/// Reopen the directory and verify the REAL bytes: manifest hash, transcript length/hash, every
/// asset length/hash, no extra/foreign entries, no symlinks, and every expected message id present.
///
/// Caller must supply the enclosing [`OwnedRoot`] and target `dir`. Fails closed with
/// [`ArchiveError::Unsafe`] on containment escapes or symlinks, [`ArchiveError::UnsafeInput`] on
/// non-canonical asset filenames, or [`ArchiveError::Verify`] on any missing/extra file or hash mismatch.
pub fn verify(
    root: &OwnedRoot,
    dir: &Path,
    manifest_sha: Option<&str>,
    expected_ids: &[u64],
) -> Result<Manifest, ArchiveError> {
    root.contained(dir).map_err(|_| ArchiveError::Unsafe)?;
    // Inspect `symlink_metadata` before reading so symlinks are rejected without being followed.
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
    // One pass over the transcript collects every `data-mid="<value>"`; each expected id is then
    // an O(1) lookup. Equivalent to the former per-id `contains(data-mid="<id>")` scan: an id
    // matches only if some attribute value equals its decimal form exactly.
    const MID: &str = "data-mid=\"";
    let present: HashSet<&str> = html
        .match_indices(MID)
        .filter_map(|(i, _)| {
            let rest = &html[i + MID.len()..];
            rest.find('"').map(|end| &rest[..end])
        })
        .collect();
    for id in expected_ids {
        if !present.contains(id.to_string().as_str()) {
            return Err(ArchiveError::Verify);
        }
    }
    if !expected_ids.is_empty() && m.message_count != expected_ids.len() as u64 {
        return Err(ArchiveError::Verify);
    }
    let mut expected_files: BTreeSet<String> =
        ["manifest.json".to_string(), "transcript.html".to_string()].into();
    if !m.assets.is_empty() {
        expected_files.insert("assets".into());
        let ad = dir.join("assets");
        let md = std::fs::symlink_metadata(&ad).map_err(|_| ArchiveError::Verify)?;
        if md.file_type().is_symlink() || !md.is_dir() {
            return Err(ArchiveError::Unsafe);
        }
        let mut names = BTreeSet::new();
        for e in std::fs::read_dir(&ad).map_err(|_| ArchiveError::Verify)? {
            names.insert(
                e.map_err(|_| ArchiveError::Verify)?
                    .file_name()
                    .to_string_lossy()
                    .into_owned(),
            );
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
        actual.insert(
            e.map_err(|_| ArchiveError::Verify)?
                .file_name()
                .to_string_lossy()
                .into_owned(),
        );
    }
    if actual != expected_files {
        return Err(ArchiveError::Verify);
    }
    Ok(m)
}

/// Controlled, no-network transport for exercising the production downloader policy.
#[cfg(test)]
pub mod fake_transport {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    /// Pre-programmed outcome for a single [`FakeTransport::get`] call.
    #[derive(Clone)]
    pub enum Script {
        /// Return an HTTP response with status code, optional `Content-Length`, and body chunks.
        Resp {
            status: u16,
            declared: Option<u64>,
            chunks: Vec<Vec<u8>>,
        },
        /// Immediately fail the request with [`ArchiveError::Download`].
        Fail,
        /// Pend indefinitely so [`HttpDownloader::fetch`] exercises its timeout deadline.
        Hang,
    }
    /// Deterministic in-memory [`Transport`] that pops [`Script`] responses and records requested URLs.
    pub struct FakeTransport {
        script: Mutex<VecDeque<Script>>,
        fallback: Script,
        /// Ordered log of URL strings passed to [`Transport::get`].
        pub gets: Mutex<Vec<String>>,
    }
    impl FakeTransport {
        /// Creates a [`FakeTransport`] that serves `script` in FIFO order and `fallback` once empty.
        pub fn new(script: Vec<Script>, fallback: Script) -> Self {
            Self {
                script: Mutex::new(script.into()),
                fallback,
                gets: Mutex::new(vec![]),
            }
        }
        /// Returns the total number of `get` requests issued against this transport.
        pub fn get_count(&self) -> usize {
            self.gets.lock().unwrap().len()
        }
    }
    /// In-memory [`TransportResp`] yielding pre-scripted status, length, and byte chunks.
    pub struct FakeResp {
        status: u16,
        declared: Option<u64>,
        chunks: VecDeque<Vec<u8>>,
    }
    impl TransportResp for FakeResp {
        fn status(&self) -> u16 {
            self.status
        }
        fn content_length(&self) -> Option<u64> {
            self.declared
        }
        async fn chunk(&mut self) -> Result<Option<Vec<u8>>, ArchiveError> {
            Ok(self.chunks.pop_front())
        }
    }
    impl Transport for FakeTransport {
        type Resp = FakeResp;
        async fn get(&self, url: &url::Url) -> Result<FakeResp, ArchiveError> {
            self.gets.lock().unwrap().push(url.to_string());
            let s = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.fallback.clone());
            match s {
                Script::Resp {
                    status,
                    declared,
                    chunks,
                } => Ok(FakeResp {
                    status,
                    declared,
                    chunks: chunks.into(),
                }),
                Script::Fail => Err(ArchiveError::Download),
                Script::Hang => {
                    std::future::pending::<()>().await;
                    Err(ArchiveError::Download)
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;

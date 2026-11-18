//! Pure input-safety primitives for the (future) attachment archiver.
//!
//! This module performs no I/O: no DNS, no HTTP, no filesystem access. It is a
//! reusable building block for the real downloader (A07) and does NOT implement
//! archive downloading or ticket closure.
//!
//! # API
//! * [`validate_attachment_url`] - parse with the real `url` crate and accept only
//!   `https://cdn.discordapp.com` / `https://media.discordapp.net` URLs whose path is
//!   `/attachments/<channel_id>/<attachment_id>/<filename>` or
//!   `/ephemeral-attachments/<app_id>/<attachment_id>/<filename>` (both ids non-zero u64
//!   decimal). The `ephemeral-attachments` form is allowed deliberately: Discord serves
//!   ephemeral interaction attachments from that prefix on the same CDN hosts. The query
//!   string (signed `ex`/`is`/`hm` parameters) is preserved as parsed.
//! * [`generated_asset_name`] - the only way to build an on-disk asset name:
//!   `attachment-<id>.bin`, derived from the numeric attachment id alone. Original
//!   (untrusted) filenames never reach the filesystem layer; keep them as display
//!   metadata in a future manifest.
//! * [`validate_asset_component`] - accepts exactly the grammar produced by
//!   [`generated_asset_name`] (strict, case-sensitive, ASCII, canonical decimal).
//!
//! # Limits of this module
//! * Validating the *initial* URL does not make redirects safe. The downloader must
//!   disable redirects or re-run [`validate_attachment_url`] on EVERY redirect target,
//!   bound the streamed byte count, and enforce filesystem containment.
//! * [`validate_asset_component`] is a pure string check. It cannot prevent
//!   symlink/junction/TOCTOU races; those belong to the file-open layer in A07
//!   (e.g. create-new open inside a verified directory handle).
//! * The URL check is a syntactic allow-list; it says nothing about the host's DNS
//!   resolution or TLS identity.
//! * Errors carry only a fixed [`ArchiveInputErrorKind`]; no attacker-controlled text
//!   (URL, query, filename) is stored or displayed.

use std::fmt;
use url::{Host, Url};

/// Maximum accepted raw URL length in bytes (checked before parsing).
pub const MAX_URL_LEN: usize = 2048;
/// Maximum accepted asset component length in bytes.
pub const MAX_ASSET_NAME_LEN: usize = 64;

const ALLOWED_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];
const ALLOWED_PATH_ROOTS: [&str; 2] = ["attachments", "ephemeral-attachments"];
const MAX_FILENAME_SEGMENT_LEN: usize = 255;
const ASSET_PREFIX: &str = "attachment-";
const ASSET_SUFFIX: &str = ".bin";

/// Category of rejection. Contains no input data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveInputErrorKind {
    InvalidUrl,
    UnsupportedScheme,
    HostNotAllowed,
    UserInfoNotAllowed,
    PortNotAllowed,
    FragmentNotAllowed,
    /// Control character or whitespace anywhere in the raw input.
    ControlCharacter,
    PathNotAllowed,
    TooLong,
    InvalidAttachmentId,
    InvalidAssetName,
}

impl ArchiveInputErrorKind {
    fn message(self) -> &'static str {
        match self {
            Self::InvalidUrl => "attachment URL could not be parsed",
            Self::UnsupportedScheme => "attachment URL must use https",
            Self::HostNotAllowed => "attachment URL host is not an approved Discord CDN host",
            Self::UserInfoNotAllowed => "attachment URL must not contain credentials",
            Self::PortNotAllowed => "attachment URL must not specify a non-default port",
            Self::FragmentNotAllowed => "attachment URL must not contain a fragment",
            Self::ControlCharacter => "input contains a control character or whitespace",
            Self::PathNotAllowed => "attachment URL path is not a Discord attachment path",
            Self::TooLong => "input exceeds the maximum allowed length",
            Self::InvalidAttachmentId => "attachment id is invalid",
            Self::InvalidAssetName => "asset name is not a generated attachment name",
        }
    }
}

/// Sanitized error: only the kind is retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveInputError {
    kind: ArchiveInputErrorKind,
}

impl ArchiveInputError {
    fn new(kind: ArchiveInputErrorKind) -> Self {
        Self { kind }
    }

    #[allow(dead_code)] // A10 public surface; used by the integration suite
    pub fn kind(&self) -> ArchiveInputErrorKind {
        self.kind
    }
}

impl fmt::Display for ArchiveInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.message())
    }
}

impl std::error::Error for ArchiveInputError {}

fn fail<T>(kind: ArchiveInputErrorKind) -> Result<T, ArchiveInputError> {
    Err(ArchiveInputError::new(kind))
}

/// Non-zero canonical decimal u64 (no sign, no leading zeros, ASCII digits only).
fn parse_canonical_id(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || s.starts_with('0') {
        return None;
    }
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// Validate a Discord attachment URL. See the module docs for the exact policy.
///
/// Callers MUST also re-validate every redirect target (or disable redirects).
pub fn validate_attachment_url(raw: &str) -> Result<Url, ArchiveInputError> {
    use ArchiveInputErrorKind as K;
    if raw.len() > MAX_URL_LEN {
        return fail(K::TooLong);
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return fail(K::ControlCharacter);
    }
    // The url crate treats '\' as '/' for https; refuse any backslash outright.
    if raw.contains('\\') {
        return fail(K::PathNotAllowed);
    }
    let url = Url::parse(raw).map_err(|_| ArchiveInputError::new(K::InvalidUrl))?;
    if url.scheme() != "https" {
        return fail(K::UnsupportedScheme);
    }
    // `https://:@host/` parses to an empty username and no password, so also inspect the raw
    // authority text (everything between "://" and the first '/', '?' or '#') for '@'.
    // The url crate also tolerates extra/missing slashes after "https:" (e.g. `https:////@host`)
    // and normalises them away, so only the canonical `https://<nonempty authority>` form is
    // accepted; otherwise the raw authority text could be misread as empty.
    let rest = match raw.get(..8) {
        Some(p) if p.eq_ignore_ascii_case("https://") => &raw[8..],
        _ => return fail(K::InvalidUrl),
    };
    if rest.starts_with('/') {
        return fail(K::InvalidUrl);
    }
    let raw_authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if !url.username().is_empty() || url.password().is_some() || raw_authority.contains('@') {
        return fail(K::UserInfoNotAllowed);
    }
    match url.host() {
        Some(Host::Domain(d)) if ALLOWED_HOSTS.contains(&d) => {}
        _ => return fail(K::HostNotAllowed),
    }
    // The parser drops an explicit default port (:443), so any remaining port is non-default.
    if url.port().is_some() {
        return fail(K::PortNotAllowed);
    }
    if url.fragment().is_some() {
        return fail(K::FragmentNotAllowed);
    }

    // The parser silently resolves dot segments (including %2e); the normalised path must still
    // be the literal text we were given, otherwise the input tried to smuggle a traversal.
    let raw_no_query = raw.split('?').next().unwrap_or(raw);
    if !raw_no_query.ends_with(url.path()) {
        return fail(K::PathNotAllowed);
    }
    let lower_path = url.path().to_ascii_lowercase();
    if ["%2e", "%2f", "%5c", "%00"]
        .iter()
        .any(|p| lower_path.contains(p))
    {
        return fail(K::PathNotAllowed);
    }
    let segments: Vec<&str> = match url.path_segments() {
        Some(s) => s.collect(),
        None => return fail(K::PathNotAllowed),
    };
    let ok_shape = match segments.as_slice() {
        [root, channel, attachment, filename] => {
            ALLOWED_PATH_ROOTS.contains(root)
                && parse_canonical_id(channel).is_some()
                && parse_canonical_id(attachment).is_some()
                && !filename.is_empty()
                && *filename != "."
                && *filename != ".."
                && filename.len() <= MAX_FILENAME_SEGMENT_LEN
        }
        _ => false,
    };
    if !ok_shape {
        return fail(K::PathNotAllowed);
    }
    Ok(url)
}

/// Build the on-disk asset file name for an attachment: `attachment-<id>.bin`.
///
/// Depends only on the numeric id, so distinct ids can never collide and original
/// filenames can never influence the path. Rejects id 0.
pub fn generated_asset_name(attachment_id: u64) -> Result<String, ArchiveInputError> {
    if attachment_id == 0 {
        return fail(ArchiveInputErrorKind::InvalidAttachmentId);
    }
    Ok(format!("{ASSET_PREFIX}{attachment_id}{ASSET_SUFFIX}"))
}

/// Accept only names exactly equal to some `generated_asset_name(id)` output.
///
/// Pure string check; does not defend against symlink/junction races (see module docs).
pub fn validate_asset_component(name: &str) -> Result<(), ArchiveInputError> {
    use ArchiveInputErrorKind as K;
    if name.len() > MAX_ASSET_NAME_LEN {
        return fail(K::TooLong);
    }
    let id = name
        .strip_prefix(ASSET_PREFIX)
        .and_then(|rest| rest.strip_suffix(ASSET_SUFFIX))
        .and_then(parse_canonical_id);
    match id {
        Some(id) if generated_asset_name(id).as_deref() == Ok(name) => Ok(()),
        _ => fail(K::InvalidAssetName),
    }
}

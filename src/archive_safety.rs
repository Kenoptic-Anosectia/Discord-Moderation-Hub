//! Pure input-safety primitives for ticket attachment archiving (`docs/verification/ARCHIVE-INPUT-SAFETY.md`).
//!
//! Responsible for validating Discord CDN attachment URLs against a strict HTTPS host and path
//! allow-list, generating deterministic ID-based on-disk asset filenames (`attachment-<id>.bin`),
//! and verifying asset filename components before filesystem writes or reads.
//! Deliberately performs no I/O (no DNS, HTTP, or filesystem access) and does not defend against
//! HTTP redirects, DNS/TLS spoofing, or symlink/TOCTOU races on disk.
//! Trust boundary: inputs are untrusted URL strings, attachment IDs, and manifest filenames;
//! original user filenames never reach disk paths, and errors carry only fixed [`ArchiveInputErrorKind`]
//! values so untrusted URLs, signed query tokens, or filenames are never echoed or leaked.

use std::fmt;
use url::{Host, Url};

/// Maximum accepted raw URL length in bytes (checked before parsing).
pub const MAX_URL_LEN: usize = 2048;
/// Maximum accepted asset component length in bytes.
pub const MAX_ASSET_NAME_LEN: usize = 64;

/// Approved Discord CDN hostnames; any other host is rejected before network I/O.
const ALLOWED_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];
/// Approved first path segments for standard and ephemeral Discord interaction attachments.
const ALLOWED_PATH_ROOTS: [&str; 2] = ["attachments", "ephemeral-attachments"];
/// Maximum byte length of the trailing filename segment in an attachment URL path.
const MAX_FILENAME_SEGMENT_LEN: usize = 255;
/// Fixed ASCII prefix required on all generated on-disk asset filenames.
const ASSET_PREFIX: &str = "attachment-";
/// Fixed ASCII extension required on all generated on-disk asset filenames.
const ASSET_SUFFIX: &str = ".bin";

/// Fixed rejection category for attachment URL and asset-name validation.
///
/// Carries no slice or copy of the rejected input so logging or displaying the error
/// fails closed without leaking untrusted URLs, query tokens, or filenames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveInputErrorKind {
    /// Input could not be parsed as a canonical `https://<authority>/...` URL.
    InvalidUrl,
    /// Parsed URL scheme was not `https`.
    UnsupportedScheme,
    /// Host was missing, an IP literal, or not in `ALLOWED_HOSTS`.
    HostNotAllowed,
    /// URL contained a username, password, or `@` in the raw authority.
    UserInfoNotAllowed,
    /// URL specified an explicit non-default port.
    PortNotAllowed,
    /// URL contained a `#fragment` component.
    FragmentNotAllowed,
    /// Control character or whitespace anywhere in the raw input.
    ControlCharacter,
    /// Path contained backslashes, traversal/encoded-separator sequences, or wrong segment shape.
    PathNotAllowed,
    /// Raw URL or asset name exceeded [`MAX_URL_LEN`] or [`MAX_ASSET_NAME_LEN`].
    TooLong,
    /// Attachment ID was zero.
    InvalidAttachmentId,
    /// Asset filename did not match canonical `attachment-<non-zero u64>.bin`.
    InvalidAssetName,
}

impl ArchiveInputErrorKind {
    /// Returns a static, input-free description suitable for logs and error displays.
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

/// Sanitized validation error retaining only [`ArchiveInputErrorKind`] and never the offending input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveInputError {
    kind: ArchiveInputErrorKind,
}

impl ArchiveInputError {
    fn new(kind: ArchiveInputErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the fixed rejection category for this validation failure.
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

/// Parses `s` as a non-zero canonical decimal `u64` (ASCII digits only, no sign, no leading zero).
///
/// Rejecting leading zeros prevents distinct string representations from mapping to the same ID.
fn parse_canonical_id(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || s.starts_with('0') {
        return None;
    }
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// Validates that `raw` is a canonical Discord CDN attachment URL safe to pass to an non-redirecting HTTPS client.
///
/// Caller must disable HTTP redirects (or re-validate every target) and bound response bytes.
/// Fails closed with [`ArchiveInputError`] if length, character set, scheme, authority, host,
/// port, fragment, or 4-segment attachment path shape violates policy.
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
    // Reject percent-encoded dots, slashes, backslashes, and NUL bytes so downstream consumers
    // that decode path segments cannot be tricked into path traversal or truncation.
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
    // Require exact 4-segment Discord attachment layout: `<root>/<channel_or_app_id>/<attachment_id>/<filename>`.
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

/// Builds the canonical on-disk asset filename `attachment-<attachment_id>.bin` from the numeric ID alone.
///
/// Caller must still write the resulting name inside a verified directory handle with `create_new`.
/// Fails closed with [`ArchiveInputErrorKind::InvalidAttachmentId`] when `attachment_id == 0`;
/// untrusted original filenames are ignored so they can never influence filesystem paths.
pub fn generated_asset_name(attachment_id: u64) -> Result<String, ArchiveInputError> {
    if attachment_id == 0 {
        return fail(ArchiveInputErrorKind::InvalidAttachmentId);
    }
    Ok(format!("{ASSET_PREFIX}{attachment_id}{ASSET_SUFFIX}"))
}

/// Verifies that `name` is an exact, canonical `attachment-<id>.bin` filename produced by [`generated_asset_name`].
///
/// Pure string check; caller must still enforce directory containment and non-symlink file checks on disk.
/// Fails closed with [`ArchiveInputErrorKind::TooLong`] or [`ArchiveInputErrorKind::InvalidAssetName`]
/// on any path separator, leading zero, non-ASCII digit, or extra characters.
pub fn validate_asset_component(name: &str) -> Result<(), ArchiveInputError> {
    use ArchiveInputErrorKind as K;
    if name.len() > MAX_ASSET_NAME_LEN {
        return fail(K::TooLong);
    }
    // Round-trip through canonical ID parsing and `generated_asset_name` so only exact bijective names pass.
    let id = name
        .strip_prefix(ASSET_PREFIX)
        .and_then(|rest| rest.strip_suffix(ASSET_SUFFIX))
        .and_then(parse_canonical_id);
    match id {
        Some(id) if generated_asset_name(id).as_deref() == Ok(name) => Ok(()),
        _ => fail(K::InvalidAssetName),
    }
}

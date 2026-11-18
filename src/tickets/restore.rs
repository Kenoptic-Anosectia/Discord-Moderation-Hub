//! A16: safe offline restore into a new bounded staging directory.

use super::archive::{self, ArchiveError, Manifest, OwnedRoot};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, ErrorKind, Read};
use std::path::Path;

const MANIFEST: &str = "manifest.json";
const TRANSCRIPT: &str = "transcript.html";
const ASSETS: &str = "assets";

#[derive(Clone, Debug)]
pub struct RestoreRequest<'a> {
    pub dir_name: &'a str,
    pub expected_manifest_sha: &'a str,
    pub expected_ticket: i64,
    pub expected_channel: u64,
    pub nonce: u64,
}

#[derive(Clone, Debug)]
pub struct RestoreLimits {
    pub max_files: usize,
    pub max_total_bytes: u64,
}

impl Default for RestoreLimits {
    fn default() -> Self {
        Self {
            max_files: 10_000,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreError {
    BadName,
    Verify,
    Identity,
    Exists,
    TooLarge,
    Unsafe,
    Io,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    pub dest_name: String,
    pub files: u64,
    pub bytes: u64,
}

/// Restore one verified archive under a fresh, exclusively owned staging root.
/// The roots and their ancestors must not be concurrently changed by another writer.
/// The returned file and byte counts include the manifest and transcript.
pub fn restore_to_staging(
    archive_root: &OwnedRoot,
    staging_root: &OwnedRoot,
    req: &RestoreRequest<'_>,
    limits: &RestoreLimits,
) -> Result<Restored, RestoreError> {
    let RestoreRequest {
        dir_name,
        expected_manifest_sha,
        expected_ticket,
        expected_channel,
        nonce,
    } = *req;
    let (ticket, seq) = dir_name
        .strip_prefix("ticket-")
        .and_then(|suffix| suffix.split_once('-'))
        .filter(|(ticket, seq)| digits(ticket) && digits(seq))
        .ok_or(RestoreError::BadName)?;

    let source_root = archive_root.path();
    let source = source_root.join(dir_name);
    check_root(source_root)?;
    // A07's verifier does not check the archive directory itself for links and
    // does not reject all Windows reparse points. Check types before it reads.
    check_tree(&source)?;
    let manifest = archive::verify(archive_root, &source, Some(expected_manifest_sha), &[])
        .map_err(|err| match err {
            ArchiveError::Unsafe => RestoreError::Unsafe,
            _ => RestoreError::Verify,
        })?;
    if manifest.format != 1
        || manifest.ticket != expected_ticket
        || manifest.channel != expected_channel
    {
        return Err(RestoreError::Identity);
    }

    let manifest_len = file_len(&source.join(MANIFEST))?;
    let (files, bytes) = check_limits(&manifest, manifest_len, limits)?;
    let staging = staging_root.path();
    check_root(staging)?;
    if staging.starts_with(source_root) {
        return Err(RestoreError::Unsafe);
    }
    let dest_name = format!("restore-{ticket}-{seq}-{nonce}");
    let dest = staging.join(&dest_name);
    // create_dir, unlike create_dir_all, fails atomically if the name is taken.
    fs::create_dir(&dest).map_err(|err| {
        if err.kind() == ErrorKind::AlreadyExists || fs::symlink_metadata(&dest).is_ok() {
            RestoreError::Exists
        } else {
            RestoreError::Io
        }
    })?;

    let result = (|| {
        checked_dir(&dest, RestoreError::Io)?;
        checked_dir(&source, RestoreError::Verify)?;
        copy_exact(&source, &dest, MANIFEST, manifest_len)?;
        copy_exact(&source, &dest, TRANSCRIPT, manifest.transcript_len)?;
        if !manifest.assets.is_empty() {
            let from = source.join(ASSETS);
            checked_dir(&from, RestoreError::Verify)?;
            let to = dest.join(ASSETS);
            fs::create_dir(&to).map_err(|_| RestoreError::Io)?;
            checked_dir(&to, RestoreError::Io)?;
            for asset in &manifest.assets {
                // The verified manifest has unique, validated asset components.
                copy_exact(&from, &to, &asset.file, asset.len)?;
            }
        }
        verify_copy(staging, &dest, expected_manifest_sha)?;
        Ok(Restored {
            dest_name,
            files,
            bytes,
        })
    })();
    if result.is_err() {
        // Only reached after we created the directory. Never clean up through a
        // changed root or a replaced link. Surface cleanup faults as I/O errors.
        check_root(staging).map_err(|_| RestoreError::Io)?;
        checked_dir(&dest, RestoreError::Io).map_err(|_| RestoreError::Io)?;
        fs::remove_dir_all(&dest).map_err(|_| RestoreError::Io)?;
    }
    result
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

fn redirected(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

fn require_dir(meta: &Metadata) -> Result<(), RestoreError> {
    if redirected(meta) || !meta.is_dir() {
        return Err(RestoreError::Unsafe);
    }
    Ok(())
}

fn checked_dir(path: &Path, read_error: RestoreError) -> Result<(), RestoreError> {
    require_dir(&fs::symlink_metadata(path).map_err(|_| read_error)?)?;
    if fs::canonicalize(path).map_err(|_| read_error)? != path {
        return Err(RestoreError::Unsafe);
    }
    Ok(())
}

pub(super) fn check_root(root: &Path) -> Result<(), RestoreError> {
    // Check ancestors top down so even metadata on a child cannot traverse a
    // pre-existing link (including an ancestor replaced since OwnedRoot::open).
    let ancestors: Vec<_> = root.ancestors().collect();
    for path in ancestors.into_iter().rev().filter(|path| path.has_root()) {
        require_dir(&fs::symlink_metadata(path).map_err(|_| RestoreError::Io)?)?;
    }
    if fs::canonicalize(root).map_err(|_| RestoreError::Io)? != root {
        return Err(RestoreError::Unsafe);
    }
    Ok(())
}

fn file_len(path: &Path) -> Result<u64, RestoreError> {
    let meta = fs::symlink_metadata(path).map_err(|_| RestoreError::Verify)?;
    if redirected(&meta) || !meta.is_file() {
        return Err(RestoreError::Unsafe);
    }
    Ok(meta.len())
}

pub(super) fn check_tree(dir: &Path) -> Result<(), RestoreError> {
    checked_dir(dir, RestoreError::Verify)?;
    for entry in fs::read_dir(dir).map_err(|_| RestoreError::Verify)? {
        let entry = entry.map_err(|_| RestoreError::Verify)?;
        if entry.file_name() == ASSETS {
            let assets = entry.path();
            checked_dir(&assets, RestoreError::Verify)?;
            for asset in fs::read_dir(assets).map_err(|_| RestoreError::Verify)? {
                file_len(&asset.map_err(|_| RestoreError::Verify)?.path())?;
            }
        } else {
            file_len(&entry.path())?;
        }
    }
    Ok(())
}

fn check_limits(
    manifest: &Manifest,
    manifest_len: u64,
    limits: &RestoreLimits,
) -> Result<(u64, u64), RestoreError> {
    let files = manifest
        .assets
        .len()
        .checked_add(2)
        .ok_or(RestoreError::TooLarge)?;
    if files > limits.max_files {
        return Err(RestoreError::TooLarge);
    }
    let bytes = manifest
        .assets
        .iter()
        .map(|asset| asset.len)
        .chain([manifest_len, manifest.transcript_len])
        .try_fold(0u64, u64::checked_add)
        .filter(|&bytes| bytes <= limits.max_total_bytes)
        .ok_or(RestoreError::TooLarge)?;
    Ok((files as u64, bytes))
}

fn copy_exact(from: &Path, to: &Path, name: &str, len: u64) -> Result<(), RestoreError> {
    let source = from.join(name);
    if file_len(&source)? != len {
        return Err(RestoreError::Verify);
    }
    let mut input = File::open(source).map_err(|_| RestoreError::Io)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to.join(name))
        .map_err(|_| RestoreError::Io)?;
    // Never write more than was budgeted, even if the source length changes.
    let copied =
        io::copy(&mut (&mut input).take(len), &mut output).map_err(|_| RestoreError::Io)?;
    if copied != len || input.read(&mut [0]).map_err(|_| RestoreError::Io)? != 0 {
        return Err(RestoreError::Verify);
    }
    output.sync_all().map_err(|_| RestoreError::Io)
}

fn verify_copy(staging: &Path, dest: &Path, manifest_sha: &str) -> Result<(), RestoreError> {
    check_root(staging).map_err(|_| RestoreError::Verify)?;
    let root = OwnedRoot::open(staging).map_err(|_| RestoreError::Verify)?;
    if root.path() != staging {
        return Err(RestoreError::Verify);
    }
    check_tree(dest).map_err(|_| RestoreError::Verify)?;
    archive::verify(&root, dest, Some(manifest_sha), &[]).map_err(|_| RestoreError::Verify)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tickets::archive::{self, AssetEntry, Downloaded, PublishInput, Published};
    use crate::tickets::fixtures;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const ASSET: &str = "assets/attachment-100.bin";
    const FILES: [&str; 3] = ["manifest.json", "transcript.html", ASSET];
    const DEST: &str = "restore-5-1-42";

    struct Fixture {
        home: PathBuf,
        archive: OwnedRoot,
        staging: OwnedRoot,
        published: Published,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/a08-tests");
            fs::create_dir_all(&parent).unwrap();
            let home = parent.join(format!(
                "restore-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&home).unwrap();
            let archive = OwnedRoot::open(&home.join("archive")).unwrap();
            let staging = OwnedRoot::open(&home.join("staging")).unwrap();
            let mut messages = fixtures::message_pages(1)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            let sample = fixtures::asset_fixtures().remove(0);
            let len = sample.bytes.len() as u64;
            messages[0].attachments.push(sample.attachment.clone());
            let published = archive::publish(
                &archive,
                &PublishInput {
                    ticket: 5,
                    channel: 9,
                    seq: 1,
                    nonce: 1,
                    messages: &messages,
                    downloaded: &[Downloaded {
                        entry: AssetEntry {
                            attachment_id: sample.attachment.id.get(),
                            message_id: messages[0].id.get(),
                            file: "attachment-100.bin".into(),
                            len,
                            sha256: archive::sha256_hex(&sample.bytes),
                            display_name: sample.attachment.filename,
                            declared_size: len,
                        },
                        bytes: sample.bytes,
                    }],
                },
            )
            .unwrap();
            Self {
                home,
                archive,
                staging,
                published,
            }
        }

        fn source(&self) -> PathBuf {
            self.archive.path().join(&self.published.dir_name)
        }

        fn run(&self, sha: &str, ticket: i64, channel: u64) -> Result<Restored, RestoreError> {
            restore_to_staging(
                &self.archive,
                &self.staging,
                &RestoreRequest {
                    expected_manifest_sha: sha,
                    expected_ticket: ticket,
                    expected_channel: channel,
                    ..self.request(42)
                },
                &RestoreLimits::default(),
            )
        }

        fn request(&self, nonce: u64) -> RestoreRequest<'_> {
            RestoreRequest {
                dir_name: &self.published.dir_name,
                expected_manifest_sha: &self.published.manifest_sha256,
                expected_ticket: 5,
                expected_channel: 9,
                nonce,
            }
        }

        fn restore(&self, nonce: u64, limits: &RestoreLimits) -> Result<Restored, RestoreError> {
            restore_to_staging(&self.archive, &self.staging, &self.request(nonce), limits)
        }

        fn empty_staging(&self) {
            assert!(fs::read_dir(self.staging.path()).unwrap().next().is_none());
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    fn contents(dir: &Path) -> [Vec<u8>; 3] {
        FILES.map(|name| fs::read(dir.join(name)).unwrap())
    }

    #[test]
    fn restores_exact_bytes_and_preserves_source() {
        let f = Fixture::new();
        let before = contents(&f.source());
        let bytes = before.iter().map(|b| b.len() as u64).sum();
        let actual = f
            .restore(
                42,
                &RestoreLimits {
                    max_files: 3,
                    max_total_bytes: bytes,
                },
            )
            .unwrap();
        assert_eq!(
            actual,
            Restored {
                dest_name: DEST.into(),
                files: 3,
                bytes
            }
        );
        let dest = f.staging.path().join(DEST);
        assert_eq!(contents(&dest), before);
        assert_eq!(contents(&f.source()), before);
        assert!(
            archive::verify(&f.staging, &dest, Some(&f.published.manifest_sha256), &[]).is_ok()
        );
        assert!(archive::verify(
            &f.archive,
            &f.source(),
            Some(&f.published.manifest_sha256),
            &[]
        )
        .is_ok());
    }

    #[test]
    fn rejects_wrong_manifest_hash() {
        let f = Fixture::new();
        assert_eq!(f.run("00", 5, 9), Err(RestoreError::Verify));
        f.empty_staging();
    }

    #[test]
    fn rejects_foreign_ticket_and_channel() {
        let f = Fixture::new();
        for (ticket, channel) in [(6, 9), (5, 10)] {
            assert_eq!(
                f.run(&f.published.manifest_sha256, ticket, channel),
                Err(RestoreError::Identity)
            );
            f.empty_staging();
        }
    }

    #[test]
    fn rejects_unknown_manifest_format() {
        let f = Fixture::new();
        let path = f.source().join("manifest.json");
        let mut manifest: archive::Manifest =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest.format = 2;
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            f.run(&archive::sha256_hex(&bytes), 5, 9),
            Err(RestoreError::Identity)
        );
        f.empty_staging();
    }

    #[test]
    fn rejects_transcript_and_asset_tampering() {
        let f = Fixture::new();
        for file in ["transcript.html", ASSET] {
            let path = f.source().join(file);
            let original = fs::read(&path).unwrap();
            let mut changed = original.clone();
            changed[0] ^= 1;
            fs::write(&path, changed).unwrap();
            assert_eq!(
                f.restore(42, &RestoreLimits::default()),
                Err(RestoreError::Verify)
            );
            f.empty_staging();
            fs::write(&path, original).unwrap();
        }
    }

    #[test]
    fn rejects_bad_names_without_touching_filesystem() {
        let f = Fixture::new();
        let before = contents(&f.source());
        for name in [
            "..",
            "../x",
            "ticket-1-1/..",
            "/ticket-1-1",
            "C:\\ticket-1-1",
            ".ticket-1-1",
            "ticket-a-b",
            "",
            "ticket-1-1\\x",
            "ticket-1-1-1",
            "ticket--1",
            "ticket-1-",
            "ticket-１-1",
            "ticket-1-1 ",
        ] {
            assert_eq!(
                restore_to_staging(
                    &f.archive,
                    &f.staging,
                    &RestoreRequest {
                        dir_name: name,
                        ..f.request(42)
                    },
                    &RestoreLimits::default()
                ),
                Err(RestoreError::BadName),
                "{name:?}"
            );
            f.empty_staging();
        }
        assert_eq!(contents(&f.source()), before);
    }

    #[test]
    fn refuses_existing_directory_or_file_without_changing_it() {
        let f = Fixture::new();
        let dest = f.staging.path().join(DEST);
        fs::create_dir(&dest).unwrap();
        let marker = dest.join("marker");
        fs::write(&marker, b"keep me").unwrap();
        assert_eq!(
            f.restore(42, &RestoreLimits::default()),
            Err(RestoreError::Exists)
        );
        assert_eq!(fs::read(&marker).unwrap(), b"keep me");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 1);
        fs::remove_dir_all(&dest).unwrap();
        fs::write(&dest, b"keep this too").unwrap();
        assert_eq!(
            f.restore(42, &RestoreLimits::default()),
            Err(RestoreError::Exists)
        );
        assert_eq!(fs::read(&dest).unwrap(), b"keep this too");
    }

    #[test]
    fn limits_include_manifest_and_assets() {
        let f = Fixture::new();
        let total = contents(&f.source())
            .iter()
            .map(|b| b.len() as u64)
            .sum::<u64>();
        for limits in [
            RestoreLimits {
                max_files: 2,
                max_total_bytes: total,
            },
            RestoreLimits {
                max_files: 3,
                max_total_bytes: total - 1,
            },
        ] {
            assert_eq!(f.restore(42, &limits), Err(RestoreError::TooLarge));
            f.empty_staging();
        }
    }

    #[test]
    fn refuses_same_or_nested_staging_root() {
        let f = Fixture::new();
        let nested = OwnedRoot::open(&f.archive.path().join("child/staging")).unwrap();
        for staging in [&f.archive, &nested] {
            assert_eq!(
                restore_to_staging(
                    &f.archive,
                    staging,
                    &f.request(42),
                    &RestoreLimits::default()
                ),
                Err(RestoreError::Unsafe)
            );
            assert!(!staging.path().join(DEST).exists());
        }
        assert!(fs::read_dir(nested.path()).unwrap().next().is_none());
    }

    #[test]
    fn refuses_nonregular_source_file() {
        let f = Fixture::new();
        let path = f.source().join("transcript.html");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(
            f.restore(42, &RestoreLimits::default()),
            Err(RestoreError::Unsafe)
        );
        f.empty_staging();
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn refuses_source_symlink() {
        let f = Fixture::new();
        let path = f.source().join(ASSET);
        let target = f.home.join("outside-archive.bin");
        fs::rename(&path, &target).unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&target, &path);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&target, &path);
        if let Err(e) = made {
            eprintln!("SKIP refuses_source_symlink: OS denied symlink creation: {e}");
            return;
        }
        assert_eq!(
            f.restore(42, &RestoreLimits::default()),
            Err(RestoreError::Unsafe)
        );
        f.empty_staging();
    }

    #[test]
    fn different_nonces_keep_both_restores() {
        let f = Fixture::new();
        let first = f.restore(42, &RestoreLimits::default()).unwrap();
        let first_path = f.staging.path().join(&first.dest_name);
        let first_bytes = contents(&first_path);
        let second = f.restore(43, &RestoreLimits::default()).unwrap();
        assert_eq!(first.dest_name, DEST);
        assert_eq!(second.dest_name, "restore-5-1-43");
        assert_eq!(contents(&first_path), first_bytes);
        assert_eq!(
            contents(&f.staging.path().join(second.dest_name)),
            first_bytes
        );
        assert_eq!(fs::read_dir(f.staging.path()).unwrap().count(), 2);
    }
}

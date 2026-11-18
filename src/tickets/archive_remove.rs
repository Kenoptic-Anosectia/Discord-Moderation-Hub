//! A19: verified single-archive directory removal primitive. Owned by exactly one A08 slice.

use crate::tickets::archive::{verify, OwnedRoot};
use std::path::PathBuf;

/// Failure kinds of the removal primitive. Every one of them leaves the archive directory
/// untouched unless it is [`RemoveError::Io`] raised *after* the atomic rename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveError {
    /// The requested name is not exactly `ticket-<digits>-<digits>`; the filesystem was not touched.
    BadName,
    /// No such entry in the root.
    NotFound,
    /// The entry exists but is a symlink/junction or a regular file, not a real directory.
    Unsafe,
    /// Byte-level re-verification of the archive failed; the directory was not touched.
    Verify,
    /// The verified manifest does not describe the archive the caller expected.
    Identity,
    /// A filesystem operation failed (rename, delete, trash-name collision).
    Io,
}

/// What a successful removal reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    pub dir_name: String,
    pub files: u64,
    pub bytes: u64,
}

/// Everything the removal primitive needs to know about ONE archive.
///
/// Bundled into a struct (rather than a long argument list) so the call site is self-documenting
/// and the lint budget is not spent on argument count.
#[derive(Clone, Debug)]
pub struct RemoveRequest<'a> {
    pub dir_name: &'a str,
    pub expected_manifest_sha: &'a str,
    pub expected_ticket: i64,
    pub expected_channel: u64,
    pub expected_messages: u64,
    pub expected_assets: u64,
    pub nonce: u64,
}

/// Split `ticket-<ticket>-<seq>` into its numeric parts. Rejects anything else.
fn parse_archive_name(name: &str) -> Option<(i64, i64)> {
    let rest = name.strip_prefix("ticket-")?;
    let (a, b) = rest.split_once('-')?;
    if a.is_empty()
        || b.is_empty()
        || !a.bytes().all(|c| c.is_ascii_digit())
        || !b.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    // Digits only: no sign, no overflow surprises beyond i64.
    let ticket: i64 = a.parse().ok()?;
    let seq: i64 = b.parse().ok()?;
    Some((ticket, seq))
}

/// The crash-recovery trash name: `.trash-<ticket>-<seq>-<nonce>`.
fn trash_name(ticket: i64, seq: i64, nonce: u64) -> String {
    format!(".trash-{}-{}-{}", ticket, seq, nonce)
}

/// True only for exactly `.trash-<digits>-<digits>-<digits>`.
fn is_trash_name(name: &str) -> bool {
    let rest = match name.strip_prefix(".trash-") {
        Some(r) => r,
        None => return false,
    };
    let parts: Vec<&str> = rest.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
}

/// Removes archive `dir_name` from `root`. Order matters:
/// 1. `dir_name` must be exactly `ticket-<digits>-<digits>` (BadName; filesystem untouched).
/// 2. The entry must exist as a REAL directory (`symlink_metadata`); a symlink/junction/file ->
///    Unsafe, target untouched; absent -> NotFound.
/// 3. `archive::verify(root, dir, Some(expected_manifest_sha), &[])` must succeed (Verify) and the
///    manifest must satisfy format==1, ticket==expected_ticket, channel==expected_channel,
///    message_count==expected_messages, assets.len() as u64==expected_assets (Identity). Any
///    failure leaves the directory untouched.
/// 4. Rename the directory ATOMICALLY to `.trash-<ticket>-<seq>-<nonce>` inside the same root
///    (never overwrite: if that name exists -> Io), then `remove_dir_all` the trash directory. A
///    crash between the two steps leaves only a `.trash-*` directory, never a half-deleted
///    `ticket-*` archive.
/// 5. Return counts of files/bytes removed (counted from the verified manifest + transcript +
///    manifest file).
pub fn remove_archive_dir(
    root: &OwnedRoot,
    req: &RemoveRequest<'_>,
) -> Result<Removed, RemoveError> {
    let (ticket, seq) = parse_archive_name(req.dir_name).ok_or(RemoveError::BadName)?;
    let dir_name = req.dir_name;
    let dir = root.path().join(dir_name);

    // 2. The entry must exist as a REAL directory.
    let md = match std::fs::symlink_metadata(&dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RemoveError::NotFound),
        Err(_) => return Err(RemoveError::Io),
    };
    if md.file_type().is_symlink() || !md.is_dir() {
        return Err(RemoveError::Unsafe);
    }

    // 3. Byte-level verification, then identity of the manifest against the caller's expectation.
    let manifest = verify(root, &dir, Some(req.expected_manifest_sha), &[])
        .map_err(|_| RemoveError::Verify)?;
    if manifest.format != 1
        || manifest.ticket != req.expected_ticket
        || manifest.channel != req.expected_channel
        || manifest.message_count != req.expected_messages
        || manifest.assets.len() as u64 != req.expected_assets
    {
        return Err(RemoveError::Identity);
    }

    // The removal counts are taken from the VERIFIED manifest plus the transcript and manifest
    // file, so the manifest file's own length is read BEFORE anything is renamed or deleted.
    let manifest_len = std::fs::metadata(dir.join("manifest.json"))
        .map(|m| m.len())
        .map_err(|_| RemoveError::Verify)?;
    let files = 2u64 + manifest.assets.len() as u64;
    let bytes =
        manifest.transcript_len + manifest_len + manifest.assets.iter().map(|a| a.len).sum::<u64>();

    // 4. Atomic rename to a trash name inside the same root, then delete it.
    let trash = root.path().join(trash_name(ticket, seq, req.nonce));
    if std::fs::symlink_metadata(&trash).is_ok() {
        return Err(RemoveError::Io); // never overwrite an existing trash entry
    }
    std::fs::rename(&dir, &trash).map_err(|_| RemoveError::Io)?;
    std::fs::remove_dir_all(&trash).map_err(|_| RemoveError::Io)?;

    Ok(Removed {
        dir_name: dir_name.to_string(),
        files,
        bytes,
    })
}

/// Crash recovery: removes ONLY immediate children of `root` that are real directories (not
/// symlinks) whose names match exactly `.trash-<digits>-<digits>-<digits>`; returns how many were
/// removed. Never touches any other name, any `ticket-*` archive, or anything through a symlink.
/// Idempotent: a second call returns 0.
pub fn finish_pending_removals(root: &OwnedRoot) -> Result<usize, RemoveError> {
    let mut removed = 0usize;
    let entries = std::fs::read_dir(root.path()).map_err(|_| RemoveError::Io)?;
    for entry in entries {
        let entry = entry.map_err(|_| RemoveError::Io)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_trash_name(&name) {
            continue;
        }
        let path: PathBuf = root.path().join(&name);
        let md = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue, // vanished underneath us: nothing to remove
        };
        if md.file_type().is_symlink() || !md.is_dir() {
            continue; // never follow a symlink, never delete a plain file
        }
        std::fs::remove_dir_all(&path).map_err(|_| RemoveError::Io)?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tickets::archive::{publish, Downloaded, PublishInput};
    use serenity::all::{Attachment, Message};
    use std::path::Path;

    /// Scratch root under the worktree's `target/`, unique per test run.
    fn owned(tag: &str) -> (PathBuf, OwnedRoot) {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/a08-tests/remove")
            .join(format!(
                "rm-{}-{}-{}",
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

    fn messages(n: usize) -> Vec<Message> {
        let mut out = Vec::new();
        for i in 0..n {
            let mut m = Message::default();
            m.id = serenity::all::MessageId::new(20_000 + i as u64);
            m.channel_id = serenity::all::ChannelId::new(1);
            m.author.id = serenity::all::UserId::new(1);
            m.author.name = format!("user_{}", i);
            m.content = format!("synthetic message number {}", i);
            m.timestamp =
                serenity::all::Timestamp::from_unix_timestamp(1_767_225_600 + i as i64).unwrap();
            out.push(m);
        }
        out
    }

    /// One synthetic asset attached to the first message.
    fn with_asset(m: &mut Message, bytes: Vec<u8>) {
        let a: Attachment = serde_json::from_value(serde_json::json!({
            "id": "100",
            "filename": "same.txt",
            "proxy_url": "https://example.invalid/proxy",
            "size": bytes.len(),
            "url": "https://example.invalid/asset"
        }))
        .unwrap();
        m.attachments.push(a);
    }

    fn downloaded(bytes: Vec<u8>) -> Vec<Downloaded> {
        vec![Downloaded {
            entry: crate::tickets::archive::AssetEntry {
                attachment_id: 100,
                message_id: 20_000,
                file: "attachment-100.bin".to_string(),
                len: bytes.len() as u64,
                sha256: crate::tickets::archive::sha256_hex(&bytes),
                display_name: "same.txt".to_string(),
                declared_size: bytes.len() as u64,
            },
            bytes,
        }]
    }

    /// Publish a real archive with `archive::publish` and return its published descriptor.
    fn publish_archive(
        root: &OwnedRoot,
        ticket: i64,
        channel: u64,
        seq: i64,
        msgs: &[Message],
        dls: &[Downloaded],
    ) -> crate::tickets::archive::Published {
        publish(
            root,
            &PublishInput {
                ticket,
                channel,
                seq,
                nonce: 1,
                messages: msgs,
                downloaded: dls,
            },
        )
        .unwrap()
    }

    /// A published archive with one message and one asset.
    fn simple_archive(
        root: &OwnedRoot,
        ticket: i64,
        channel: u64,
        seq: i64,
    ) -> crate::tickets::archive::Published {
        let bytes = b"synthetic attachment payload".to_vec();
        let mut msgs = messages(3);
        with_asset(&mut msgs[0], bytes.clone());
        publish_archive(root, ticket, channel, seq, &msgs, &downloaded(bytes))
    }

    /// Build a removal request for a published archive (the common case in these tests).
    fn req(
        pubd: &crate::tickets::archive::Published,
        ticket: i64,
        channel: u64,
        messages: u64,
        assets: u64,
        nonce: u64,
    ) -> RemoveRequest<'_> {
        RemoveRequest {
            dir_name: &pubd.dir_name,
            expected_manifest_sha: &pubd.manifest_sha256,
            expected_ticket: ticket,
            expected_channel: channel,
            expected_messages: messages,
            expected_assets: assets,
            nonce,
        }
    }

    /// Build a removal request from raw parts (used for names that are not real archives).
    fn raw_req<'a>(
        dir_name: &'a str,
        manifest_sha: &'a str,
        ticket: i64,
        channel: u64,
        messages: u64,
        assets: u64,
        nonce: u64,
    ) -> RemoveRequest<'a> {
        RemoveRequest {
            dir_name,
            expected_manifest_sha: manifest_sha,
            expected_ticket: ticket,
            expected_channel: channel,
            expected_messages: messages,
            expected_assets: assets,
            nonce,
        }
    }

    fn read_all_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                let p = e.path();
                let md = std::fs::symlink_metadata(&p).unwrap();
                if md.is_dir() {
                    stack.push(p);
                } else {
                    out.push((p.to_string_lossy().into_owned(), std::fs::read(&p).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    // ---------- happy path ----------

    #[test]
    fn removes_one_verified_archive_and_counts_files_and_bytes() {
        let (p, root) = owned("happy");
        let pubd = simple_archive(&root, 5, 9, 1);
        let dir = root.path().join(&pubd.dir_name);
        let manifest_bytes = std::fs::metadata(dir.join("manifest.json")).unwrap().len();
        let transcript_bytes = std::fs::metadata(dir.join("transcript.html"))
            .unwrap()
            .len();
        let asset_bytes = std::fs::metadata(dir.join("assets/attachment-100.bin"))
            .unwrap()
            .len();

        let removed =
            remove_archive_dir(&root, &req(&pubd, 5, 9, pubd.messages, pubd.assets, 7)).unwrap();

        assert_eq!(removed.dir_name, pubd.dir_name);
        assert_eq!(removed.files, 3); // transcript.html + manifest.json + 1 asset file
        assert_eq!(
            removed.bytes,
            transcript_bytes + manifest_bytes + asset_bytes
        );
        assert!(!dir.exists(), "the archive directory must be gone");
        assert!(std::fs::symlink_metadata(&dir).is_err());
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn sibling_archives_and_unrelated_files_are_untouched_byte_for_byte() {
        let (p, root) = owned("siblings");
        let a = simple_archive(&root, 5, 9, 1);
        let b = simple_archive(&root, 6, 9, 1);
        let c = simple_archive(&root, 5, 9, 2);
        let unrelated = root.path().join("notes.txt");
        std::fs::write(&unrelated, b"do not touch me").unwrap();
        let unrelated_dir = root.path().join("other-dir");
        std::fs::create_dir(&unrelated_dir).unwrap();
        std::fs::write(unrelated_dir.join("keep.bin"), b"keep").unwrap();

        let before_b = read_all_bytes(&root.path().join(&b.dir_name));
        let before_c = read_all_bytes(&root.path().join(&c.dir_name));
        let before_unrelated = std::fs::read(&unrelated).unwrap();

        remove_archive_dir(&root, &req(&a, 5, 9, a.messages, a.assets, 7)).unwrap();

        assert_eq!(read_all_bytes(&root.path().join(&b.dir_name)), before_b);
        assert_eq!(read_all_bytes(&root.path().join(&c.dir_name)), before_c);
        assert_eq!(std::fs::read(&unrelated).unwrap(), before_unrelated);
        assert!(unrelated_dir.join("keep.bin").exists());
        assert!(!root.path().join(&a.dir_name).exists());
        let _ = std::fs::remove_dir_all(p);
    }

    // ---------- verification failures ----------

    #[test]
    fn wrong_manifest_sha_is_verify_and_leaves_the_archive_intact() {
        let (p, root) = owned("badsha");
        let pubd = simple_archive(&root, 5, 9, 1);
        let dir = root.path().join(&pubd.dir_name);
        let before = read_all_bytes(&dir);

        assert_eq!(
            remove_archive_dir(
                &root,
                &raw_req(
                    &pubd.dir_name,
                    "deadbeef",
                    5,
                    9,
                    pubd.messages,
                    pubd.assets,
                    7
                )
            )
            .unwrap_err(),
            RemoveError::Verify
        );
        assert!(dir.exists());
        assert_eq!(read_all_bytes(&dir), before);
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn tampered_transcript_is_verify_and_leaves_the_archive_intact() {
        let (p, root) = owned("tamper");
        let pubd = simple_archive(&root, 5, 9, 1);
        let dir = root.path().join(&pubd.dir_name);
        std::fs::write(dir.join("transcript.html"), b"<html>tampered</html>").unwrap();
        // The state that must survive the failed removal is the tampered state.
        let before = read_all_bytes(&dir);

        assert_eq!(
            remove_archive_dir(&root, &req(&pubd, 5, 9, pubd.messages, pubd.assets, 7))
                .unwrap_err(),
            RemoveError::Verify
        );
        assert!(dir.exists());
        assert_eq!(read_all_bytes(&dir), before);
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn tampered_asset_is_verify_and_leaves_the_archive_intact() {
        let (p, root) = owned("tamperasset");
        let pubd = simple_archive(&root, 5, 9, 1);
        let dir = root.path().join(&pubd.dir_name);
        std::fs::write(dir.join("assets/attachment-100.bin"), b"tampered").unwrap();

        assert_eq!(
            remove_archive_dir(&root, &req(&pubd, 5, 9, pubd.messages, pubd.assets, 7))
                .unwrap_err(),
            RemoveError::Verify
        );
        assert!(dir.exists());
        assert!(dir.join("assets/attachment-100.bin").exists());
        let _ = std::fs::remove_dir_all(p);
    }

    // ---------- identity failures ----------

    #[test]
    fn foreign_ticket_channel_or_counts_is_identity_and_untouched() {
        let (p, root) = owned("identity");
        let pubd = simple_archive(&root, 5, 9, 1);
        let dir = root.path().join(&pubd.dir_name);
        let before = read_all_bytes(&dir);

        // wrong ticket
        assert_eq!(
            remove_archive_dir(&root, &req(&pubd, 6, 9, pubd.messages, pubd.assets, 7))
                .unwrap_err(),
            RemoveError::Identity
        );
        // wrong channel
        assert_eq!(
            remove_archive_dir(&root, &req(&pubd, 5, 10, pubd.messages, pubd.assets, 7))
                .unwrap_err(),
            RemoveError::Identity
        );
        // wrong message count
        assert_eq!(
            remove_archive_dir(&root, &req(&pubd, 5, 9, pubd.messages + 1, pubd.assets, 7))
                .unwrap_err(),
            RemoveError::Identity
        );
        // wrong asset count
        assert_eq!(
            remove_archive_dir(&root, &req(&pubd, 5, 9, pubd.messages, pubd.assets + 1, 7))
                .unwrap_err(),
            RemoveError::Identity
        );
        assert!(dir.exists());
        assert_eq!(read_all_bytes(&dir), before);
        let _ = std::fs::remove_dir_all(p);
    }

    // ---------- bad names ----------

    #[test]
    fn bad_names_are_rejected_without_touching_anything() {
        let (p, root) = owned("badname");
        let outside = p
            .parent()
            .unwrap()
            .join(format!("outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"keep").unwrap();

        let bad = [
            "..",
            "../x",
            "ticket-../x",
            "/ticket-5-1",
            "C:\\ticket-5-1",
            "C:\\Windows",
            "ticket-5-1/../..",
            "ticket-5\\1",
            ".ticket-5-1",
            ".trash-1-1-1",
            "ticket-x-1",
            "ticket--1",
            "ticket-5-",
            "ticket-5",
            "ticket-5-1-1",
            "",
            " ",
            "ticket-5-1 ",
            "ticket-+5-1",
            "ticket-5-1/",
            "./ticket-5-1",
        ];
        for name in bad {
            assert_eq!(
                remove_archive_dir(&root, &raw_req(name, "00", 5, 9, 1, 0, 7)).unwrap_err(),
                RemoveError::BadName,
                "name {name:?} must be rejected"
            );
        }
        // nothing outside the root was touched
        assert!(outside.join("keep.txt").exists());
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(p);
    }

    // ---------- absent / unsafe ----------

    #[test]
    fn absent_archive_is_not_found() {
        let (p, root) = owned("absent");
        assert_eq!(
            remove_archive_dir(&root, &raw_req("ticket-404-1", "00", 404, 9, 1, 0, 7)).unwrap_err(),
            RemoveError::NotFound
        );
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn a_regular_file_named_like_an_archive_is_unsafe() {
        let (p, root) = owned("plainfile");
        std::fs::write(root.path().join("ticket-5-1"), b"not a directory").unwrap();
        assert_eq!(
            remove_archive_dir(&root, &raw_req("ticket-5-1", "00", 5, 9, 1, 0, 7)).unwrap_err(),
            RemoveError::Unsafe
        );
        assert_eq!(
            std::fs::read(root.path().join("ticket-5-1")).unwrap(),
            b"not a directory"
        );
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn a_symlinked_archive_entry_is_unsafe_and_the_link_target_survives() {
        let (p, root) = owned("symlink");
        let target = p
            .parent()
            .unwrap()
            .join(format!("symlink-target-{}", std::process::id()));
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("precious.txt"), b"precious").unwrap();

        let link = root.path().join("ticket-5-1");
        let made = symlink(&target, &link);
        if made.is_err() {
            eprintln!("SKIP: symlinked_archive_entry_is_unsafe_and_the_link_target_survives — OS refused symlink creation: {made:?}");
            let _ = std::fs::remove_dir_all(p);
            return;
        }

        assert_eq!(
            remove_archive_dir(&root, &raw_req("ticket-5-1", "00", 5, 9, 1, 0, 7)).unwrap_err(),
            RemoveError::Unsafe
        );
        assert!(target.join("precious.txt").exists());
        assert_eq!(
            std::fs::read(target.join("precious.txt")).unwrap(),
            b"precious"
        );
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let _ = std::fs::remove_dir_all(p);
        let _ = std::fs::remove_dir_all(&target);
    }

    // ---------- trash collision ----------

    #[test]
    fn an_existing_trash_name_is_io_and_the_original_survives() {
        let (p, root) = owned("collision");
        let pubd = simple_archive(&root, 5, 9, 1);
        let dir = root.path().join(&pubd.dir_name);
        let before = read_all_bytes(&dir);
        // Pre-create the exact trash name the removal would use.
        let trash = root.path().join(".trash-5-1-7");
        std::fs::create_dir(&trash).unwrap();
        std::fs::write(trash.join("marker.txt"), b"marker").unwrap();

        assert_eq!(
            remove_archive_dir(&root, &req(&pubd, 5, 9, pubd.messages, pubd.assets, 7))
                .unwrap_err(),
            RemoveError::Io
        );
        assert!(dir.exists());
        assert_eq!(read_all_bytes(&dir), before);
        assert!(trash.join("marker.txt").exists());
        let _ = std::fs::remove_dir_all(p);
    }

    // ---------- crash recovery ----------

    #[test]
    fn finish_pending_removals_clears_only_real_trash_directories() {
        let (p, root) = owned("crash");
        let pubd = simple_archive(&root, 5, 9, 1);
        let archive_dir = root.path().join(&pubd.dir_name);

        // A crash between rename and delete leaves exactly this behind.
        let crash_trash = root.path().join(".trash-1-1-7");
        std::fs::create_dir(&crash_trash).unwrap();
        std::fs::write(crash_trash.join("leftover.bin"), b"leftover").unwrap();

        // Look-alikes and other entries that must survive untouched.
        let staging = root.path().join(".staging-1-1-1");
        std::fs::create_dir(&staging).unwrap();
        let look1 = root.path().join(".trash-x");
        std::fs::create_dir(&look1).unwrap();
        let look2 = root.path().join(".trash-1-1");
        std::fs::create_dir(&look2).unwrap();
        let look3 = root.path().join("trash-1-1-1");
        std::fs::create_dir(&look3).unwrap();
        let look4 = root.path().join(".trash-1-1-1-1");
        std::fs::create_dir(&look4).unwrap();
        let stray = root.path().join("stray.txt");
        std::fs::write(&stray, b"stray").unwrap();
        let trash_file = root.path().join(".trash-2-2-3");
        std::fs::write(&trash_file, b"a file, not a dir").unwrap();

        let archive_before = read_all_bytes(&archive_dir);

        assert_eq!(finish_pending_removals(&root).unwrap(), 1);
        assert!(!crash_trash.exists());

        // Idempotent: a second call removes nothing.
        assert_eq!(finish_pending_removals(&root).unwrap(), 0);

        // Everything else is untouched.
        assert!(archive_dir.exists());
        assert_eq!(read_all_bytes(&archive_dir), archive_before);
        assert!(staging.exists());
        assert!(look1.exists());
        assert!(look2.exists());
        assert!(look3.exists());
        assert!(look4.exists());
        assert!(stray.exists());
        assert!(trash_file.exists());
        assert_eq!(std::fs::read(&trash_file).unwrap(), b"a file, not a dir");
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn finish_pending_removals_never_follows_a_symlinked_trash_entry() {
        let (p, root) = owned("crashsym");
        let target = p
            .parent()
            .unwrap()
            .join(format!("crash-sym-target-{}", std::process::id()));
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("precious.txt"), b"precious").unwrap();

        let link = root.path().join(".trash-1-1-7");
        let made = symlink(&target, &link);
        if made.is_err() {
            eprintln!("SKIP: finish_pending_removals_never_follows_a_symlinked_trash_entry — OS refused symlink creation: {made:?}");
            let _ = std::fs::remove_dir_all(p);
            let _ = std::fs::remove_dir_all(&target);
            return;
        }

        assert_eq!(finish_pending_removals(&root).unwrap(), 0);
        assert!(target.join("precious.txt").exists());
        assert_eq!(
            std::fs::read(target.join("precious.txt")).unwrap(),
            b"precious"
        );
        let _ = std::fs::remove_dir_all(p);
        let _ = std::fs::remove_dir_all(&target);
    }

    // ---------- repeatability ----------

    #[test]
    fn removing_the_same_archive_twice_is_not_found_the_second_time() {
        let (p, root) = owned("repeat");
        let pubd = simple_archive(&root, 5, 9, 1);
        let sibling = simple_archive(&root, 6, 9, 1);
        let sibling_before = read_all_bytes(&root.path().join(&sibling.dir_name));

        remove_archive_dir(&root, &req(&pubd, 5, 9, pubd.messages, pubd.assets, 7)).unwrap();

        assert_eq!(
            remove_archive_dir(&root, &req(&pubd, 5, 9, pubd.messages, pubd.assets, 8))
                .unwrap_err(),
            RemoveError::NotFound
        );
        // The failed second attempt left the sibling and the root alone.
        assert_eq!(
            read_all_bytes(&root.path().join(&sibling.dir_name)),
            sibling_before
        );
        assert!(!root.path().join(".trash-5-1-8").exists());
        let _ = std::fs::remove_dir_all(p);
    }

    // ---------- name parsing unit coverage ----------

    #[test]
    fn trash_names_are_matched_exactly() {
        assert!(is_trash_name(".trash-1-1-7"));
        assert!(is_trash_name(".trash-123-456-7890"));
        assert!(!is_trash_name(".trash-x"));
        assert!(!is_trash_name(".trash-1-1"));
        assert!(!is_trash_name("trash-1-1-1"));
        assert!(!is_trash_name(".trash-1-1-1-1"));
        assert!(!is_trash_name(".trash--1-1"));
        assert!(!is_trash_name(".trash-1-1-"));
        assert!(!is_trash_name(".trash-1-1-1 "));
        assert!(!is_trash_name(".staging-1-1-1"));
        assert!(!is_trash_name("ticket-1-1"));
    }

    #[test]
    fn archive_names_are_parsed_strictly() {
        assert_eq!(parse_archive_name("ticket-5-1"), Some((5, 1)));
        assert_eq!(parse_archive_name("ticket-0-0"), Some((0, 0)));
        assert_eq!(parse_archive_name("ticket-12-345"), Some((12, 345)));
        assert_eq!(parse_archive_name("ticket-5-1 "), None);
        assert_eq!(parse_archive_name("ticket--1"), None);
        assert_eq!(parse_archive_name("ticket-5-"), None);
        assert_eq!(parse_archive_name("ticket-x-1"), None);
        assert_eq!(parse_archive_name(".ticket-5-1"), None);
        assert_eq!(parse_archive_name("ticket-5-1-1"), None);
        assert_eq!(parse_archive_name(""), None);
    }

    /// Create a directory symlink (or a Windows junction, which needs no elevated privilege),
    /// returning the OS error if the platform refuses every form.
    ///
    /// A Windows junction reports `is_symlink() == true` and `is_dir() == false` under
    /// `symlink_metadata`, so it exercises exactly the same guard as a POSIX symlink.
    fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            // A real symlink needs SeCreateSymbolicLinkPrivilege; a junction does not, and
            // `symlink_metadata` reports a junction as a symlink, so the guard under test is
            // the same either way. Try the symlink first, then the junction.
            if std::os::windows::fs::symlink_dir(target, link).is_ok() {
                return Ok(());
            }
            // `mklink` parses any forward slash in a path as a switch, so hand it
            // backslash-separated paths.
            let out = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &link.to_string_lossy().replace('/', "\\"),
                    &target.to_string_lossy().replace('/', "\\"),
                ])
                .output()
                .map_err(std::io::Error::other)?;
            if out.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    String::from_utf8_lossy(&out.stderr).to_string(),
                ))
            }
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
    }
}

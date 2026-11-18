//! A18: read-only archive health scan and orphan report. Owned by exactly one A08 slice.

use super::archive::{self, OwnedRoot};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Verified,
    Missing,
    Corrupt,
    IdentityMismatch,
    BadName,
}

#[derive(Clone, Debug)]
pub struct ScanEntry {
    pub ticket: i64,
    pub seq: i64,
    pub dir_name: String,
    pub manifest_sha256: String,
    pub messages: u64,
    pub assets: u64,
    pub channel: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanResult {
    pub ticket: i64,
    pub seq: i64,
    pub health: Health,
}

pub(super) fn is_archive_dir_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("ticket-") else {
        return false;
    };
    let Some((ticket, seq)) = rest.split_once('-') else {
        return false;
    };
    !ticket.is_empty()
        && !seq.is_empty()
        && ticket.bytes().all(|b| b.is_ascii_digit())
        && seq.bytes().all(|b| b.is_ascii_digit())
}

fn scan_one(root: &OwnedRoot, entry: &ScanEntry) -> Health {
    if !is_archive_dir_name(&entry.dir_name) {
        return Health::BadName;
    }
    let dir = root.path().join(&entry.dir_name);
    let md = match std::fs::symlink_metadata(&dir) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Health::Missing,
        Err(_) => return Health::Corrupt,
    };
    if md.file_type().is_symlink() || !md.is_dir() {
        return Health::Corrupt;
    }
    let Ok(m) = archive::verify(root, &dir, Some(&entry.manifest_sha256), &[]) else {
        return Health::Corrupt;
    };
    if m.format == 1
        && m.ticket == entry.ticket
        && m.channel == entry.channel
        && m.message_count == entry.messages
        && m.assets.len() as u64 == entry.assets
    {
        Health::Verified
    } else {
        Health::IdentityMismatch
    }
}

/// One result per input entry, in input order. For each entry: dir_name must be exactly `ticket-<digits>-<digits>`
/// (else BadName, filesystem untouched); the directory must exist as a real directory, not a symlink (via
/// `symlink_metadata`; absent -> Missing); then `archive::verify(root, dir, Some(&manifest_sha256), &[])`:
/// Err -> Corrupt; Ok(m) requires m.format==1, m.ticket==entry.ticket, m.channel==entry.channel,
/// m.message_count==entry.messages, m.assets.len() as u64==entry.assets, else IdentityMismatch; otherwise Verified.
pub fn scan_recorded(root: &OwnedRoot, entries: &[ScanEntry]) -> Vec<ScanResult> {
    entries
        .iter()
        .map(|entry| ScanResult {
            ticket: entry.ticket,
            seq: entry.seq,
            health: scan_one(root, entry),
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrphanReport {
    pub unrecorded_archives: Vec<String>,
    pub staging_debris: Vec<String>,
    pub unknown_entries: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanError {
    Io,
    TooMany,
}

pub const MAX_ROOT_ENTRIES: usize = 10_000;

/// Lists the immediate children of the archive root (`symlink_metadata`, never following links) and classifies each
/// by NAME and type: a real directory named `ticket-<digits>-<digits>` that is not in `known_dir_names` ->
/// unrecorded_archives; a name starting `.staging-` or `.trash-` -> staging_debris; everything else (files, symlinks,
/// other names, a `ticket-*` name that is a symlink) -> unknown_entries. Each Vec sorted ascending. More than
/// MAX_ROOT_ENTRIES children -> Err(TooMany) without classifying. This function must not delete or touch anything.
pub fn find_orphans(
    root: &OwnedRoot,
    known_dir_names: &[String],
) -> Result<OrphanReport, ScanError> {
    find_orphans_with_cap(root, known_dir_names, MAX_ROOT_ENTRIES)
}

fn find_orphans_with_cap(
    root: &OwnedRoot,
    known_dir_names: &[String],
    max_entries: usize,
) -> Result<OrphanReport, ScanError> {
    let rd = std::fs::read_dir(root.path()).map_err(|_| ScanError::Io)?;
    let mut children = Vec::new();
    for item in rd {
        let entry = item.map_err(|_| ScanError::Io)?;
        if children.len() >= max_entries {
            return Err(ScanError::TooMany);
        }
        children.push(entry);
    }

    let known: BTreeSet<&str> = known_dir_names.iter().map(String::as_str).collect();
    let mut report = OrphanReport::default();

    for entry in children {
        let md = std::fs::symlink_metadata(entry.path()).map_err(|_| ScanError::Io)?;
        let ft = md.file_type();
        let os_name = entry.file_name();
        let Some(name) = os_name.to_str().map(str::to_owned) else {
            report
                .unknown_entries
                .push(os_name.to_string_lossy().into_owned());
            continue;
        };
        if !ft.is_symlink() && ft.is_dir() && is_archive_dir_name(&name) {
            if !known.contains(name.as_str()) {
                report.unrecorded_archives.push(name);
            }
        } else if !ft.is_symlink() && (name.starts_with(".staging-") || name.starts_with(".trash-"))
        {
            report.staging_debris.push(name);
        } else {
            report.unknown_entries.push(name);
        }
    }

    report.unrecorded_archives.sort();
    report.staging_debris.sort();
    report.unknown_entries.sort();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tickets::archive::{
        self, sha256_hex, ArchiveError, Downloaded, Downloader, Limits, Manifest, PublishInput,
        Published,
    };
    use crate::tickets::fixtures;
    use serenity::all::{Attachment, Message};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    const CDN: &str = "https://cdn.discordapp.com/attachments/1/100/same.txt?ex=1&is=2&hm=3";

    fn scratch_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/a08-tests")
            .join(format!(
                "scan-{}-{}-{}-{}",
                tag,
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn owned(tag: &str) -> (PathBuf, OwnedRoot) {
        let p = scratch_dir(tag);
        let r = OwnedRoot::open(&p).unwrap();
        (p, r)
    }

    struct FixedDl(Vec<u8>);
    impl Downloader for FixedDl {
        async fn fetch(&self, _url: &url::Url, limit: u64) -> Result<Vec<u8>, ArchiveError> {
            if self.0.len() as u64 > limit {
                return Err(ArchiveError::TooLarge);
            }
            Ok(self.0.clone())
        }
    }

    async fn build_inputs(n: usize, with_assets: bool) -> (Vec<Message>, Vec<Downloaded>) {
        let mut msgs: Vec<Message> = fixtures::message_pages(n).into_iter().flatten().collect();
        let mut dls = Vec::new();
        if with_assets {
            let fx = fixtures::asset_fixtures();
            let mut a: Attachment = fx[0].attachment.clone();
            a.url = CDN.to_string();
            msgs[0].attachments.push(a);
            let dl = FixedDl(fx[0].bytes.clone());
            dls = archive::download_all(&dl, &msgs, &Limits::default())
                .await
                .unwrap();
        }
        (msgs, dls)
    }

    async fn publish_one(
        root: &OwnedRoot,
        ticket: i64,
        channel: u64,
        seq: i64,
        msg_count: usize,
        with_assets: bool,
    ) -> (Published, ScanEntry) {
        let (msgs, dls) = build_inputs(msg_count, with_assets).await;
        let pubd = archive::publish(
            root,
            &PublishInput {
                ticket,
                channel,
                seq,
                nonce: 1,
                messages: &msgs,
                downloaded: &dls,
            },
        )
        .unwrap();
        let entry = ScanEntry {
            ticket,
            seq,
            dir_name: pubd.dir_name.clone(),
            manifest_sha256: pubd.manifest_sha256.clone(),
            messages: pubd.messages,
            assets: pubd.assets,
            channel,
        };
        (pubd, entry)
    }

    fn tree_snapshot(dir: &Path) -> BTreeMap<String, String> {
        fn walk(base: &Path, cur: &Path, out: &mut BTreeMap<String, String>) {
            let mut entries: Vec<_> = std::fs::read_dir(cur)
                .unwrap()
                .map(|e| e.unwrap())
                .collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let p = e.path();
                let rel = p
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let md = std::fs::symlink_metadata(&p).unwrap();
                if md.file_type().is_symlink() {
                    out.insert(rel, "symlink".into());
                } else if md.is_dir() {
                    out.insert(rel.clone(), "dir".into());
                    walk(base, &p, out);
                } else {
                    let bytes = std::fs::read(&p).unwrap();
                    out.insert(rel, format!("file:{}", sha256_hex(&bytes)));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    #[tokio::test]
    async fn intact_archive_is_verified() {
        let (p, root) = owned("intact");
        let (_pubd, entry) = publish_one(&root, 10, 100, 1, 3, true).await;
        let got = scan_recorded(&root, std::slice::from_ref(&entry));
        assert_eq!(
            got,
            vec![ScanResult {
                ticket: 10,
                seq: 1,
                health: Health::Verified,
            }]
        );
        let _ = std::fs::remove_dir_all(p);
    }

    #[tokio::test]
    async fn deleted_directory_is_missing() {
        let (p, root) = owned("missing");
        let (pubd, entry) = publish_one(&root, 11, 101, 1, 2, false).await;
        std::fs::remove_dir_all(root.path().join(&pubd.dir_name)).unwrap();
        let got = scan_recorded(&root, &[entry]);
        assert_eq!(
            got,
            vec![ScanResult {
                ticket: 11,
                seq: 1,
                health: Health::Missing,
            }]
        );
        let _ = std::fs::remove_dir_all(p);
    }

    #[tokio::test]
    async fn transcript_byte_flipped_is_corrupt() {
        let (p, root) = owned("tx-flip");
        let (pubd, entry) = publish_one(&root, 12, 102, 1, 2, false).await;
        let tp = root.path().join(&pubd.dir_name).join("transcript.html");
        let mut bytes = std::fs::read(&tp).unwrap();
        bytes[0] ^= 0x01;
        std::fs::write(&tp, &bytes).unwrap();
        let got = scan_recorded(&root, &[entry]);
        assert_eq!(got[0].health, Health::Corrupt);
        let _ = std::fs::remove_dir_all(p);
    }

    #[tokio::test]
    async fn asset_byte_flipped_is_corrupt() {
        let (p, root) = owned("asset-flip");
        let (pubd, entry) = publish_one(&root, 13, 103, 1, 2, true).await;
        let ap = root
            .path()
            .join(&pubd.dir_name)
            .join("assets/attachment-100.bin");
        let mut bytes = std::fs::read(&ap).unwrap();
        bytes[0] ^= 0x01;
        std::fs::write(&ap, &bytes).unwrap();
        let got = scan_recorded(&root, &[entry]);
        assert_eq!(got[0].health, Health::Corrupt);
        let _ = std::fs::remove_dir_all(p);
    }

    #[tokio::test]
    async fn manifest_hash_mismatch_is_corrupt() {
        let (p, root) = owned("sha-mismatch");
        let (_pubd, mut entry) = publish_one(&root, 14, 104, 1, 2, false).await;
        entry.manifest_sha256 = "00".repeat(32);
        let got = scan_recorded(&root, &[entry]);
        assert_eq!(got[0].health, Health::Corrupt);
        let _ = std::fs::remove_dir_all(p);
    }

    #[tokio::test]
    async fn manifest_of_another_ticket_or_channel_or_format_is_identity_mismatch() {
        let (p, root) = owned("id-mismatch");
        // Archive published for ticket 20, channel 200
        let (foreign_ticket, _) = publish_one(&root, 20, 200, 1, 2, false).await;
        // Archive published for ticket 15, channel 999 (wrong channel)
        let (foreign_chan, _) = publish_one(&root, 15, 999, 2, 2, false).await;

        // Move foreign_ticket into ticket-15-1 and check with its own valid manifest_sha256
        let target1 = root.path().join("ticket-15-1");
        std::fs::rename(root.path().join(&foreign_ticket.dir_name), &target1).unwrap();
        let e_ticket = ScanEntry {
            ticket: 15,
            seq: 1,
            dir_name: "ticket-15-1".into(),
            manifest_sha256: foreign_ticket.manifest_sha256,
            messages: foreign_ticket.messages,
            assets: foreign_ticket.assets,
            channel: 200,
        };
        let e_chan = ScanEntry {
            ticket: 15,
            seq: 2,
            dir_name: foreign_chan.dir_name,
            manifest_sha256: foreign_chan.manifest_sha256,
            messages: foreign_chan.messages,
            assets: foreign_chan.assets,
            channel: 200,
        };

        // Also test format != 1 with matching manifest_sha256
        let (fmt_pub, mut e_fmt) = publish_one(&root, 15, 200, 3, 2, false).await;
        let mp = root.path().join(&fmt_pub.dir_name).join("manifest.json");
        let mut m: Manifest = serde_json::from_slice(&std::fs::read(&mp).unwrap()).unwrap();
        m.format = 2;
        let new_mbytes = serde_json::to_vec_pretty(&m).unwrap();
        e_fmt.manifest_sha256 = sha256_hex(&new_mbytes);
        std::fs::write(&mp, &new_mbytes).unwrap();

        let got = scan_recorded(&root, &[e_ticket, e_chan, e_fmt]);
        assert_eq!(got[0].health, Health::IdentityMismatch);
        assert_eq!(got[1].health, Health::IdentityMismatch);
        assert_eq!(got[2].health, Health::IdentityMismatch);
        let _ = std::fs::remove_dir_all(p);
    }

    #[tokio::test]
    async fn recorded_message_or_asset_count_differing_is_identity_mismatch() {
        let (p, root) = owned("count-mismatch");
        let (_pubd, base) = publish_one(&root, 16, 106, 1, 3, true).await;
        let mut bad_msgs = base.clone();
        bad_msgs.messages += 1;
        let mut bad_assets = base.clone();
        bad_assets.assets = 0;
        let got = scan_recorded(&root, &[bad_msgs, bad_assets]);
        assert_eq!(got[0].health, Health::IdentityMismatch);
        assert_eq!(got[1].health, Health::IdentityMismatch);
        let _ = std::fs::remove_dir_all(p);
    }

    #[tokio::test]
    async fn bad_names_return_bad_name_and_never_read_outside_root() {
        let parent = scratch_dir("bad-names");
        let outside_root = OwnedRoot::open(&parent).unwrap();
        let (outside_pub, outside_entry) = publish_one(&outside_root, 1, 10, 1, 2, false).await;
        let inner_path = parent.join("inner-root");
        let root = OwnedRoot::open(&inner_path).unwrap();

        // Also place a malformed directory inside root to prove wrong-shape names in root are BadName
        std::fs::create_dir_all(root.path().join("ticket-1")).unwrap();

        let bad_names = [
            "..",
            "../ticket-1-1",
            "..\\ticket-1-1",
            &format!("../{}", outside_pub.dir_name),
            "ticket-1-1/sub",
            "ticket-1-1\\sub",
            "C:ticket-1-1",
            ".ticket-1-1",
            ".staging-1-1-1",
            "ticket-1",
            "ticket-a-1",
            "ticket-1-b",
            "ticket-1-1-1",
            "ticket--1",
            "ticket-1-",
            "other-1-1",
            "",
        ];
        let entries: Vec<ScanEntry> = bad_names
            .iter()
            .enumerate()
            .map(|(i, name)| ScanEntry {
                ticket: outside_entry.ticket,
                seq: (i as i64) + 1,
                dir_name: (*name).to_string(),
                manifest_sha256: outside_entry.manifest_sha256.clone(),
                messages: outside_entry.messages,
                assets: outside_entry.assets,
                channel: outside_entry.channel,
            })
            .collect();
        let got = scan_recorded(&root, &entries);
        assert_eq!(got.len(), entries.len());
        for (i, r) in got.iter().enumerate() {
            assert_eq!(
                r.health,
                Health::BadName,
                "name {:?} must be BadName",
                bad_names[i]
            );
            assert_eq!(r.seq, (i as i64) + 1);
        }
        let _ = std::fs::remove_dir_all(parent);
    }

    #[tokio::test]
    async fn symlinked_ticket_entry_is_rejected_and_target_untouched() {
        let parent = scratch_dir("symlink");
        let target_root_path = parent.join("target-root");
        let target_root = OwnedRoot::open(&target_root_path).unwrap();
        let (target_pub, mut entry) = publish_one(&target_root, 77, 770, 1, 2, true).await;
        let target_dir = target_root.path().join(&target_pub.dir_name);
        let before = tree_snapshot(&target_dir);

        let scan_root_path = parent.join("scan-root");
        let scan_root = OwnedRoot::open(&scan_root_path).unwrap();
        let link_path = scan_root.path().join("ticket-77-1");

        #[cfg(unix)]
        let symlink_res = std::os::unix::fs::symlink(&target_dir, &link_path);
        #[cfg(windows)]
        let symlink_res = std::os::windows::fs::symlink_dir(&target_dir, &link_path);

        if let Err(e) = symlink_res {
            eprintln!("SKIPPED symlink test: OS refused symlink creation: {e}");
            let _ = std::fs::remove_dir_all(parent);
            return;
        }

        entry.dir_name = "ticket-77-1".into();
        let got = scan_recorded(&scan_root, &[entry]);
        assert_eq!(got.len(), 1);
        assert_ne!(got[0].health, Health::Verified);
        assert_eq!(got[0].health, Health::Corrupt);

        let orphans = find_orphans(&scan_root, &[]).unwrap();
        assert!(orphans.unrecorded_archives.is_empty());
        assert_eq!(orphans.unknown_entries, vec!["ticket-77-1".to_string()]);
        assert_eq!(tree_snapshot(&target_dir), before);
        let _ = std::fs::remove_dir_all(parent);
    }

    #[tokio::test]
    async fn results_keep_input_order_and_repeated_scan_is_read_only() {
        let (p, root) = owned("order-ro");
        let (_p1, e1) = publish_one(&root, 1, 10, 1, 2, true).await;
        let (p2, e2) = publish_one(&root, 2, 20, 1, 2, false).await;
        std::fs::remove_dir_all(root.path().join(&p2.dir_name)).unwrap();
        let (_p3, mut e3) = publish_one(&root, 3, 30, 1, 2, false).await;
        e3.messages = 999;
        let e4 = ScanEntry {
            ticket: 4,
            seq: 1,
            dir_name: "../bad".into(),
            manifest_sha256: String::new(),
            messages: 0,
            assets: 0,
            channel: 40,
        };
        let (p5, e5) = publish_one(&root, 5, 50, 1, 2, false).await;
        std::fs::write(
            root.path().join(&p5.dir_name).join("transcript.html"),
            b"corrupt",
        )
        .unwrap();

        let inputs = vec![e4, e2, e1.clone(), e5, e3];
        let snap_before = tree_snapshot(root.path());

        let run1 = scan_recorded(&root, &inputs);
        let run2 = scan_recorded(&root, &inputs);
        assert_eq!(run1, run2);
        assert_eq!(
            run1,
            vec![
                ScanResult {
                    ticket: 4,
                    seq: 1,
                    health: Health::BadName
                },
                ScanResult {
                    ticket: 2,
                    seq: 1,
                    health: Health::Missing
                },
                ScanResult {
                    ticket: 1,
                    seq: 1,
                    health: Health::Verified
                },
                ScanResult {
                    ticket: 5,
                    seq: 1,
                    health: Health::Corrupt
                },
                ScanResult {
                    ticket: 3,
                    seq: 1,
                    health: Health::IdentityMismatch
                },
            ]
        );
        let orphans1 = find_orphans(&root, std::slice::from_ref(&e1.dir_name)).unwrap();
        let orphans2 = find_orphans(&root, std::slice::from_ref(&e1.dir_name)).unwrap();
        assert_eq!(orphans1, orphans2);
        assert_eq!(tree_snapshot(root.path()), snap_before);
        let _ = std::fs::remove_dir_all(p);
    }

    #[tokio::test]
    async fn find_orphans_classifies_excludes_known_and_sorts() {
        let (p, root) = owned("orphans");
        // Known archive dirs
        std::fs::create_dir(root.path().join("ticket-1-1")).unwrap();
        std::fs::create_dir(root.path().join("ticket-2-1")).unwrap();
        // Unrecorded archive dirs (out of order)
        std::fs::create_dir(root.path().join("ticket-9-2")).unwrap();
        std::fs::create_dir(root.path().join("ticket-3-1")).unwrap();
        // Staging and trash debris
        std::fs::create_dir(root.path().join(".trash-ticket-8-1")).unwrap();
        std::fs::create_dir(root.path().join(".staging-4-1-99")).unwrap();
        // Unknown entries: a stray file, a stray dir, a file named like ticket-*, and wrong-shape ticket-*
        std::fs::write(root.path().join("stray.txt"), b"hi").unwrap();
        std::fs::create_dir(root.path().join("notes")).unwrap();
        std::fs::write(root.path().join("ticket-7-1"), b"not a dir").unwrap();
        std::fs::create_dir(root.path().join("ticket-bad-1")).unwrap();

        let snap_before = tree_snapshot(root.path());
        let report =
            find_orphans(&root, &["ticket-1-1".to_string(), "ticket-2-1".to_string()]).unwrap();

        assert_eq!(
            report.unrecorded_archives,
            vec!["ticket-3-1".to_string(), "ticket-9-2".to_string()]
        );
        assert_eq!(
            report.staging_debris,
            vec![
                ".staging-4-1-99".to_string(),
                ".trash-ticket-8-1".to_string()
            ]
        );
        assert_eq!(
            report.unknown_entries,
            vec![
                "notes".to_string(),
                "stray.txt".to_string(),
                "ticket-7-1".to_string(),
                "ticket-bad-1".to_string(),
            ]
        );
        assert_eq!(tree_snapshot(root.path()), snap_before);
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn find_orphans_empty_root_is_empty_report() {
        let (p, root) = owned("empty");
        let report = find_orphans(&root, &["ticket-1-1".to_string()]).unwrap();
        assert_eq!(report, OrphanReport::default());
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn find_orphans_enforces_entry_cap() {
        assert_eq!(MAX_ROOT_ENTRIES, 10_000);
        let (p, root) = owned("cap");
        for i in 1..=5 {
            std::fs::create_dir(root.path().join(format!("ticket-{i}-1"))).unwrap();
        }
        assert!(find_orphans_with_cap(&root, &[], 5).is_ok());
        std::fs::create_dir(root.path().join("ticket-6-1")).unwrap();
        assert_eq!(
            find_orphans_with_cap(&root, &[], 5),
            Err(ScanError::TooMany)
        );
        let _ = std::fs::remove_dir_all(p);
    }
}

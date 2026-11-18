//! Authorized ticket closure, startup reconciliation and audited operator recovery.
//!
//! State machine (repository-enforced, token-checked compare-and-set):
//! active -> closing(freeze) -> archiving -> verified -> delete_requested -> closed
//!                                                     \-> delete_uncertain -> closed on KNOWN absence
//! Source deletion is only ever requested after the archive was re-read from disk and its
//! hashes/lengths/ids verified. Every other path retains source channel and per-type slot.
//! No DB transaction is held across any awaited Discord/HTTP/file call.
//!
//! Responsible for: the authorization decision for closing and recovering a ticket, the close
//! sequence (freeze, archive, re-verify, delete), the startup classification of unfinished
//! rows, and the read-only archive status reply. Discord side effects go through the `CloseIo`
//! trait and archive writing through the archive module; this file holds the decision logic
//! and itself reads the archive `manifest.json`.
//! Not responsible for: opening tickets, the archive file format, or browsing/restoring files.
//! Trust boundary: the `Actor` must come from a trusted interaction. Remote error bodies,
//! message content and file paths are never placed in a reply; replies are fixed text, plus the
//! archive reference and integrity facts (directory name, status, counts, manifest sha256)
//! that `retrieve` formats into its reply.
//! Recorded archive directory names are re-checked before they touch the filesystem.
use super::{
    archive::{self, ArchiveError, Downloader, HistorySource, Limits, OwnedRoot, PublishInput},
    controller::RemoteChannel,
    policy::Settings,
    repository::{Repository, Ticket},
    repository_close::{ArchiveRecord, Step},
};

/// Who is asking, as established by the trusted interaction (member roles + actual guild
/// owner id). Discord Administrator / Manage Roles permission bits are deliberately NOT inputs.
#[derive(Clone, Debug)]
pub struct Actor {
    /// Discord user id of the requester; 0 is never authorized.
    pub user: u64,
    /// Guild the interaction came from; must equal the configured guild.
    pub guild: u64,
    /// True only when the interaction identified this user as the actual guild owner.
    pub is_guild_owner: bool,
    /// Role ids held by the user, used only to look for the configured staff role.
    pub roles: Vec<u64>,
    /// Channel the command was invoked in.
    pub channel: u64,
}

/// Result of asking whether a channel still exists.
/// `Unknown` means it could not be determined; callers treat it as neither present nor
/// absent and retain the channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Present,
    Absent,
    Unknown,
}
/// Classified result of a channel deletion request.
/// Of the delete outcomes, only `Deleted` and `NotFound` allow a ticket to be closed
/// (a channel found `Absent` by presence checks also closes it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
    Rejected, // definite client rejection, channel still exists
    Unknown,  // timeout / 5xx / transport
}

/// Side-effect surface the closer needs from Discord. Implementations must classify
/// outcomes honestly: any failure that cannot prove the answer must be reported as an
/// error or `Unknown`, because the closer fails closed on those.
#[allow(async_fn_in_trait)]
pub trait CloseIo: HistorySource + Downloader {
    /// Narrow requester write freeze (admins can still bypass; this is best effort).
    async fn freeze(&self, channel: u64, requester: u64) -> Result<(), ()>;
    /// Classifies whether the channel exists. Must return `Unknown` when it cannot tell.
    async fn probe(&self, channel: u64) -> Presence;
    /// Requests deletion of the channel and classifies the result (see `DeleteOutcome`).
    async fn delete(&self, channel: u64) -> DeleteOutcome;
    /// Best-effort closure notice; carries no archive.
    async fn dm_closed(&self, requester: u64) -> Result<(), ()>;
    /// Lists remote channels that could belong to this ticket's operation, used by operator
    /// recovery. `Err` means remote state could not be read.
    async fn candidates(&self, ticket: &Ticket) -> Result<Vec<RemoteChannel>, ()>;
}

/// Staff role holder or the actual guild owner, in the configured guild, invoking from the
/// ticket's own durable channel. Requesters are not special-cased: they are simply not staff.
pub fn authorized(settings: &Settings, a: &Actor, t: &Ticket) -> bool {
    // Pure check, no IO. The channel clause ties the request to the ticket's stored channel,
    // so a command typed in a different channel cannot close it.
    settings.valid()
        && a.guild == settings.guild
        && t.guild == settings.guild
        && t.channel == Some(a.channel)
        && a.user != 0
        && (a.is_guild_owner || a.roles.contains(&settings.staff))
}

/// Outcome of a close attempt. Only `Closed` means the source channel is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    /// Archive verified, deletion confirmed (or channel known absent), ticket marked closed.
    Closed,
    /// Actor not authorized, or ticket unknown, or the ticket lookup failed. Only the ticket row
    /// is read before authorization; nothing is changed.
    Denied,
    /// Another worker holds a live lease, or the claim could not be made.
    Busy,
    /// Channel and slot are retained. The payload is a short internal reason label; `reply`
    /// does not show it.
    Retained(&'static str),
}
impl CloseOutcome {
    /// Fixed, sanitized user-facing text. Never includes remote error bodies.
    pub fn reply(self) -> &'static str {
        match self {
            Self::Closed => "Ticket archived, verified and closed.",
            Self::Denied => "You cannot close this ticket from here.",
            Self::Busy => "A close operation is already in progress for this ticket.",
            Self::Retained(_) => "The ticket was NOT deleted: the archive or deletion could not be confirmed. The channel and slot are retained; run the close command again or use recovery status.",
        }
    }
}

/// Dependencies for close, reconcile and recovery, borrowed for one operation.
pub struct Env<'a, I: CloseIo> {
    /// Guild configuration (guild id, staff role, categories).
    pub settings: &'a Settings,
    /// Ticket repository; all state transitions go through its token-checked methods.
    pub repo: &'a Repository,
    /// Remote and history access (Discord and downloads).
    pub io: &'a I,
    /// Owned archive root under which archives are published and re-verified.
    pub root: &'a OwnedRoot,
    /// Bounds applied to history reads and downloads.
    pub limits: &'a Limits,
    /// Injected clock, so lease expiry can be tested; also used as the publish nonce.
    pub now: &'a (dyn Fn() -> i64 + Send + Sync),
}
/// Lease length added to the clock value when a claim or step is made. Another worker may
/// take over only once `lease_until <= now`.
const LEASE: i64 = 900;
/// Upper bound on preserved unrecorded orphan directories skipped in one close attempt.
const MAX_ORPHANS: i64 = 32;

/// True when the recorded archive still verifies on disk for this ticket and channel.
fn verify_recorded(
    root: &OwnedRoot,
    ar: &super::repository_close::ArchiveRef,
    ticket: i64,
    channel: u64,
) -> bool {
    recorded_manifest(root, ar, ticket, channel).is_some()
}
/// A manifest may only be trusted for THIS ticket: supported format, same ticket id and same
/// channel. Anything else (foreign/mismatched/unsupported) is rejected and preserved untouched.
fn identity_ok(m: &archive::Manifest, ticket: i64, channel: u64) -> bool {
    m.format == 1 && m.ticket == ticket && m.channel == channel
}
/// Re-verifies a recorded archive against the disk and returns its manifest, or `None`.
/// Checked in order: the directory name is a plain child name (no separators, `:` or leading
/// dot) before any filesystem access; the manifest verifies against the recorded hash;
/// message and asset counts equal the recorded ones; the identity matches this ticket and
/// channel. Any failure gives `None` (never trusted).
fn recorded_manifest(
    root: &OwnedRoot,
    ar: &super::repository_close::ArchiveRef,
    ticket: i64,
    channel: u64,
) -> Option<archive::Manifest> {
    let dir = root.path().join(&ar.dir_name);
    // dir_name came from our own DB but is re-checked as a plain child component.
    if ar.dir_name.contains(['/', '\\', ':']) || ar.dir_name.starts_with('.') {
        return None;
    }
    match archive::verify(root, &dir, Some(&ar.manifest_sha256), &[]) {
        Ok(m)
            if m.message_count == ar.messages
                && m.assets.len() as u64 == ar.assets
                && identity_ok(&m, ticket, channel) =>
        {
            Some(m)
        }
        _ => None,
    }
}
/// Fresh full read of the source channel compared (content, edits, deletions, additions)
/// against what an archive recorded. Any failure to read is "unknown" and is NOT stable.
async fn source_unchanged<I: CloseIo>(env: &Env<'_, I>, want_sha: &str, want_count: u64) -> bool {
    if want_sha.is_empty() {
        return false;
    }
    match archive::fetch_all(env.io, env.limits).await {
        Ok(m) => m.len() as u64 == want_count && archive::content_fingerprint(&m) == want_sha,
        Err(_) => false,
    }
}

/// Closes one ticket. Ordering is the safety argument:
/// Steps, in order: authorize before any private read or mutation; claim or resume under a lease so one
/// worker runs; freeze and archive; re-read the archive from disk and re-check the
/// live source against it; only then request deletion.
/// Every path that does not end in a confirmed deletion returns `Retained` (or `Denied`/
/// `Busy`) and leaves the channel and slot in place. No database transaction is held across
/// the awaited Discord or file calls; each repository call is its own short transaction.
pub async fn close_ticket<I: CloseIo>(
    env: &Env<'_, I>,
    a: &Actor,
    ticket_id: i64,
    token: &str,
) -> CloseOutcome {
    // 1. authorization on guild + durable linkage BEFORE any private read or mutation.
    let Ok(Some(t)) = env.repo.ticket_by_id(ticket_id).await else {
        // An unknown ticket id yields the same Denied outcome as an unauthorized actor.
        return CloseOutcome::Denied;
    };
    if !authorized(env.settings, a, &t) {
        return CloseOutcome::Denied;
    }
    let now = (env.now)();
    // 2. exactly one worker: CAS claim, or take over only an EXPIRED lease.
    let claimed = match env
        .repo
        .claim_close(t.id, a.user, token, now, now + LEASE)
        .await
    {
        // Fresh claim succeeded (ticket was active with a channel).
        Ok(true) => true,
        // Not freshly claimable: try to take over only an in-close ticket whose lease expired.
        Ok(false) => matches!(
            env.repo
                .resume_close(t.id, a.user, token, now, now + LEASE)
                .await,
            Ok(true)
        ),
        // Repository error: treated as not claimed, so nothing proceeds.
        Err(_) => false,
    };
    if !claimed {
        return CloseOutcome::Busy;
    }
    // Re-read after claiming so the dispatch below uses the status the lease was taken in
    // (a resumed close may already be past `closing`).
    let Ok(Some(mut t)) = env.repo.ticket_by_id(t.id).await else {
        return CloseOutcome::Retained("state");
    };
    // Fail closed without a channel: the claim requires one, but resume does not check it.
    let Some(channel) = t.channel else {
        return CloseOutcome::Retained("no channel");
    };
    // Token-checked transition; a repository error counts as failure (false).
    let step = |from: &'static str, to: &'static str, note: &'static str| {
        let (repo, id, tok) = (env.repo, ticket_id, token.to_string());
        let now = (env.now)();
        async move {
            repo.advance(id, &tok, Step { from, to, note }, now, now + LEASE)
                .await
                .unwrap_or(false)
        }
    };
    // Every non-success exit comes through here: expire the lease so a retry may resume,
    // and leave the channel and slot untouched. No deletion is possible from this path.
    let retain = |why: &'static str| async move {
        let _ = env.repo.release_lease(ticket_id, token).await;
        CloseOutcome::Retained(why)
    };
    // 3. freeze + archive (unless an archive was already verified on a previous run).
    if t.status == "closing" {
        let _ = env.io.freeze(channel, t.opener).await; // best effort; stability probe catches writes
        if !step("closing", "archiving", "frozen").await {
            return retain("transition").await;
        }
        t.status = "archiving".into();
    }
    if t.status == "archiving" {
        // Fresh full read of the history; failure retains (nothing archived or deleted).
        let msgs = match archive::fetch_all(env.io, env.limits).await {
            Ok(m) => m,
            Err(_) => return retain("history").await,
        };
        let dl = match archive::download_all(env.io, &msgs, env.limits).await {
            Ok(d) => d,
            Err(_) => return retain("assets").await,
        };
        // content stability: nothing added, edited or deleted while assets were downloading.
        if !source_unchanged(env, &archive::content_fingerprint(&msgs), msgs.len() as u64).await {
            return retain("changed").await;
        }
        // Starting sequence for the archive directory name.
        let Ok(base_seq) = env.repo.next_archive_seq(ticket_id).await else {
            return retain("db").await;
        };
        let nonce = (env.now)() as u64;
        // Publish-before-record recovery. A previous run may have published a directory for
        // this sequence and died before recording it. Per colliding directory: adopt it only
        // if it fully re-verifies AND matches the current source; otherwise PRESERVE it
        // (never delete or overwrite) and publish under the next free sequence. Bounded, so a
        // retry always makes progress instead of colliding on the same name forever.
        // Complete expected inventory (attachment/message ids, generated file, sizes, hashes,
        // display metadata) derived from the CURRENT source; an orphan must match it exactly.
        let expected_assets: Vec<archive::AssetEntry> =
            dl.iter().map(|d| d.entry.clone()).collect();
        let mut published = None;
        for seq in base_seq..base_seq + MAX_ORPHANS {
            match archive::publish(
                env.root,
                &PublishInput {
                    ticket: ticket_id,
                    channel,
                    seq,
                    nonce,
                    messages: &msgs,
                    downloaded: &dl,
                },
            ) {
                Ok(p) => {
                    published = Some(p);
                    break;
                }
                Err(ArchiveError::Exists) => {
                    let name = format!("ticket-{ticket_id}-{seq}");
                    let dir = env.root.path().join(&name);
                    let ids: Vec<u64> = msgs.iter().map(|m| m.id.get()).collect();
                    let adopted = std::fs::read(dir.join("manifest.json"))
                        .ok()
                        .map(|b| archive::sha256_hex(&b))
                        .and_then(
                            |sha| match archive::verify(env.root, &dir, Some(&sha), &ids) {
                                Ok(m)
                                    if identity_ok(&m, ticket_id, channel)
                                        && m.content_sha256
                                            == archive::content_fingerprint(&msgs)
                                        && m.assets == expected_assets
                                        && m.message_count == msgs.len() as u64 =>
                                {
                                    Some(archive::Published {
                                        dir_name: name.clone(),
                                        manifest_sha256: sha,
                                        messages: m.message_count,
                                        assets: m.assets.len() as u64,
                                    })
                                }
                                _ => None,
                            },
                        );
                    if let Some(p) = adopted {
                        published = Some(p);
                        break;
                    }
                    // stale/corrupt orphan: preserved; try the next sequence
                }
                // Only an already-existing directory is retried with the next sequence; any other
                // publish error aborts and retains.
                Err(_) => return retain("publish").await,
            }
        }
        // Every candidate sequence collided with a directory that could not be adopted; retain.
        // The colliding directories are left on disk untouched.
        let Some(published) = published else {
            return retain("exists").await;
        };
        // Record only after publish. If recording or the archiving -> verified step fails, retain;
        // the published directory remains on disk and a rerun can adopt it through the collision
        // path above if it still matches the source.
        let rec = env
            .repo
            .record_archive(
                ticket_id,
                token,
                ArchiveRecord {
                    dir_name: &published.dir_name,
                    sha: &published.manifest_sha256,
                    messages: published.messages,
                    assets: published.assets,
                },
                (env.now)(),
            )
            .await;
        if !matches!(rec, Ok(Some(_))) || !step("archiving", "verified", "archive verified").await {
            return retain("record").await;
        }
        t.status = "verified".into();
    }
    // 4. independent reopen/reverify of what is on disk, then and only then delete.
    if matches!(
        t.status.as_str(),
        "verified" | "delete_requested" | "delete_uncertain"
    ) {
        // No recorded archive means there is nothing verified to delete against.
        let Ok(Some(ar)) = env.repo.latest_archive(ticket_id).await else {
            return retain("no archive").await;
        };
        // Re-verify from disk now rather than trusting the stored record.
        let Some(manifest) = recorded_manifest(env.root, &ar, ticket_id, channel) else {
            return retain("reverify").await;
        };
        // Immediately before ANY destructive step (also resumed verified/delete states): the
        // live source must still equal the verified archive. Absent-source reconciliation below
        // needs no check (nothing left to delete).
        // ONE classified presence outcome for delete_uncertain: Unknown => retain at once,
        // Absent => reconcile only (nothing left to delete), Present => the mandatory source
        // stability validation below runs BEFORE any retry/delete.
        if t.status == "delete_uncertain" {
            match env.io.probe(channel).await {
                Presence::Absent => {
                    return finish(env, &t, token, ticket_id, "absence reconciled").await;
                }
                Presence::Unknown => return retain("probe").await,
                Presence::Present => {}
            }
        }
        if !source_unchanged(env, &manifest.content_sha256, manifest.message_count).await {
            // keep source + slot + the old verified archive; next run publishes a new version
            // Roll the row back toward archiving along edges allowed by the repository's transition
            // table (delete_requested or delete_uncertain -> verified -> archiving). Step failures are
            // ignored because the result is retained either way.
            match t.status.as_str() {
                "delete_requested" => {
                    let _ = step("delete_requested", "verified", "source changed").await;
                    let _ = step("verified", "archiving", "source changed; re-archive").await;
                }
                "verified" => {
                    let _ = step("verified", "archiving", "source changed; re-archive").await;
                }
                "delete_uncertain" => {
                    let _ = step("delete_uncertain", "verified", "source changed").await;
                    let _ = step("verified", "archiving", "source changed; re-archive").await;
                }
                _ => {}
            }
            return retain("changed").await;
        }
        // Persist the intent to delete before the destructive call: delete_uncertain ->
        // delete_requested or verified -> delete_requested. A row already at delete_requested
        // needs no step.
        if t.status == "delete_uncertain" {
            // presence was classified Present above and the source re-validated; never blind.
            if !step("delete_uncertain", "delete_requested", "retry after probe").await {
                return retain("transition").await;
            }
        } else if t.status == "verified"
            && !step("verified", "delete_requested", "requesting delete").await
        {
            return retain("transition").await;
        }
        // Deleted and NotFound both finish the close. A definite rejection means the channel
        // still exists, so the row returns to verified. An unknown outcome moves to
        // delete_uncertain, so the next run must probe presence before any retry.
        match env.io.delete(channel).await {
            DeleteOutcome::Deleted | DeleteOutcome::NotFound => {
                return finish(env, &t, token, ticket_id, "deleted").await
            }
            DeleteOutcome::Rejected => {
                let _ = step("delete_requested", "verified", "delete rejected").await;
                return retain("delete rejected").await;
            }
            DeleteOutcome::Unknown => {
                let _ = step(
                    "delete_requested",
                    "delete_uncertain",
                    "delete outcome unknown",
                )
                .await;
                return retain("delete unknown").await;
            }
        }
    }
    // Reached for any status not handled above; nothing is done.
    retain("state").await
}

/// Moves the ticket to `closed` and sends a best-effort closure DM.
/// Re-reads the stored status for the compare-and-set `from`; a missing row, read error or
/// refused transition returns `Retained`, so the ticket is never reported closed unrecorded.
/// The DM result is recorded separately and never affects the outcome.
async fn finish<I: CloseIo>(
    env: &Env<'_, I>,
    t: &Ticket,
    token: &str,
    id: i64,
    note: &'static str,
) -> CloseOutcome {
    // The compare-and-set needs the status actually stored, not the caller's cached copy.
    let from = match env.repo.ticket_by_id(id).await {
        Ok(Some(x)) => x.status,
        _ => return CloseOutcome::Retained("state"),
    };
    let now = (env.now)();
    // Lease value 0 on the closing step; the repository only allows this from
    // delete_requested, delete_uncertain or verified.
    if !env
        .repo
        .advance(
            id,
            token,
            Step {
                from: &from,
                to: "closed",
                note,
            },
            now,
            0,
        )
        .await
        .unwrap_or(false)
    {
        return CloseOutcome::Retained("transition");
    }
    // DM is best effort and recorded separately; it never carries the archive.
    let ok = env.io.dm_closed(t.opener).await.is_ok();
    let _ = env
        .repo
        .record_dm(id, if ok { "sent" } else { "failed" }, (env.now)())
        .await;
    CloseOutcome::Closed
}

// ---------------- startup reconciliation ----------------
/// Summary of one startup reconciliation pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Rows moved into `recovery` by this pass.
    pub moved_to_recovery: usize,
    /// Rows closed because a verified archive exists and the channel is known absent.
    pub closed_by_known_absence: usize,
    /// Rows left needing attention; any non-zero value keeps the opening gate closed.
    pub unresolved: usize,
}
impl Report {
    /// Opening may be allowed only after reconciliation, and only when nothing is unresolved.
    pub fn opening_gate(&self, approved: bool) -> bool {
        approved && self.unresolved == 0
    }
}
/// Classify every nonterminal row. Never retries a create/delete blindly, never auto-adopts a
/// candidate, never releases a slot on unverified evidence.
pub async fn reconcile<I: CloseIo>(env: &Env<'_, I>) -> Result<Report, crate::Error> {
    let mut r = Report::default();
    for t in env.repo.nonterminal().await? {
        let now = (env.now)();
        // Classify each row by (status, channel). Any combination not matched by an arm below
        // counts as unresolved, so unexpected states keep the gate closed.
        match (t.status.as_str(), t.channel) {
            ("creating", None) => {
                // remote outcome unknown: retain slot, require audited operator recovery
                if env
                    .repo
                    .mark_recovery(t.id, now, "startup: create outcome unverified")
                    .await?
                {
                    r.moved_to_recovery += 1;
                }
                r.unresolved += 1;
            }
            // Needs an audited operator; never resolved automatically.
            ("reserved", _) | ("recovery", _) => r.unresolved += 1,
            ("active", Some(ch)) => {
                let presence = env.io.probe(ch).await;
                if presence == Presence::Unknown {
                    // cannot prove the source channel state: fail closed
                    r.unresolved += 1;
                } else if presence == Presence::Absent {
                    // remote gone without a verified archive: keep slot, operator decides
                    if env
                        .repo
                        .mark_recovery(
                            t.id,
                            now,
                            "startup: active channel absent, no verified archive",
                        )
                        .await?
                    {
                        r.moved_to_recovery += 1;
                    }
                    r.unresolved += 1;
                }
            }
            (
                "closing" | "archiving" | "verified" | "delete_requested" | "delete_uncertain",
                Some(ch),
            ) => {
                // Fixed token with actor 0. resume_close only succeeds when the lease has expired, so a
                // live worker elsewhere is not disturbed.
                let tok = "reconcile";
                if !env
                    .repo
                    .resume_close(t.id, 0, tok, now, now + LEASE)
                    .await?
                {
                    r.unresolved += 1; // live lease elsewhere or not resumable
                    continue;
                }
                // Absence is only trusted together with an archive that re-verifies from disk.
                let verified = match env.repo.latest_archive(t.id).await? {
                    Some(ar) => verify_recorded(env.root, &ar, t.id, ch),
                    None => false,
                };
                if verified && env.io.probe(ch).await == Presence::Absent {
                    let from = env
                        .repo
                        .ticket_by_id(t.id)
                        .await?
                        .map(|x| x.status)
                        .unwrap_or_default();
                    // closing/archiving never reach closed directly; only post-verified states do.
                    if matches!(
                        from.as_str(),
                        "verified" | "delete_requested" | "delete_uncertain"
                    ) && env
                        .repo
                        .advance(
                            t.id,
                            tok,
                            Step {
                                from: &from,
                                to: "closed",
                                note: "startup: absence reconciled after verified archive",
                            },
                            (env.now)(),
                            0,
                        )
                        .await?
                    {
                        r.closed_by_known_absence += 1;
                        continue;
                    }
                }
                // Not closed here: give the lease back and count it unresolved.
                let _ = env.repo.release_lease(t.id, tok).await;
                r.unresolved += 1;
            }
            _ => r.unresolved += 1,
        }
    }
    Ok(r)
}

// ---------------- operator recovery ----------------
/// Operator recovery action. Status is read-only; the other two change state only with
/// explicit confirmation and a reason.
#[derive(Clone, Debug)]
pub enum Recovery {
    Status,
    /// Attach a remote channel to an unattached unresolved operation.
    Attach {
        channel: u64,
        confirm: bool,
        reason: String,
    },
    /// Free the slot of an unresolved operation that owns no channel.
    Release {
        confirm: bool,
        reason: String,
    },
}
/// Staff role holder or actual guild owner in the configured guild, for a ticket of that
/// guild. Same as `authorized` except that the invoking channel is not compared.
fn staff_or_owner(settings: &Settings, a: &Actor, guild: u64) -> bool {
    settings.valid()
        && a.guild == settings.guild
        && guild == settings.guild
        && a.user != 0
        && (a.is_guild_owner || a.roles.contains(&settings.staff))
}
/// Returns a fixed, sanitized reply. Status is read-only; mutations need confirm + reason +
/// verified expectations and are audited with the actor.
pub async fn recover<I: CloseIo>(
    env: &Env<'_, I>,
    a: &Actor,
    ticket_id: i64,
    action: Recovery,
) -> &'static str {
    let Ok(Some(t)) = env.repo.ticket_by_id(ticket_id).await else {
        return "Unknown ticket.";
    };
    // Authorization before any remote read or mutation.
    if !staff_or_owner(env.settings, a, t.guild) {
        return "You cannot manage this ticket.";
    }
    let now = (env.now)();
    match action {
        Recovery::Status => match t.status.as_str() {
            "creating" | "recovery" | "reserved" => "Ticket operation is unresolved; its slot is retained. Use ticket-recover attach or release with confirmation and a reason.",
            "closed" | "failed" | "released" => "Ticket is finished.",
            _ => "Ticket is open or mid-close; no action needed unless a close stays unresolved.",
        },
        Recovery::Attach { channel, confirm, reason } => {
            // Preconditions: confirmation, a non-empty reason, no channel yet, and an unresolved status.
            if !confirm || reason.trim().is_empty() || t.channel.is_some() || !matches!(t.status.as_str(), "reserved" | "creating" | "recovery") {
                return "Attach needs confirmation, a reason and an unattached unresolved operation.";
            }
            let Ok(c) = env.io.candidates(&t).await else { return "Remote state could not be read; nothing changed." };
            let kind_cat = env.settings.category(t.kind);
            // Ownership/shape checks: expected guild+category+type+private overwrite. Topic marker is corroboration only.
            let ok = c.iter().any(|x| x.id == channel && x.guild == env.settings.guild && x.category == kind_cat && x.private);
            if !ok {
                return "Channel does not match the expected guild, category, type and private overwrite; nothing changed.";
            }
            match env.repo.recovery_attach(t.id, channel, a.user, reason.trim(), now).await {
                Ok(true) => "Channel attached by audited operator action.",
                _ => "Attach refused (already owned or state changed); nothing changed.",
            }
        }
        Recovery::Release { confirm, reason } => {
            if !confirm || reason.trim().is_empty() {
                return "Release needs confirmation and a reason.";
            }
            // A ticket linked to a channel is never releasable: the channel (and its data) exist.
            if t.channel.is_some() {
                return "Ticket is linked to a channel; release refused and the slot is retained. Nothing changed.";
            }
            let Ok(c) = env.io.candidates(&t).await else { return "Remote state could not be read; nothing changed." };
            // A candidate in this guild and category carrying the operation's topic marker blocks release.
            let marker = super::policy::marker(&t.operation);
            let possible = c.iter().any(|x| x.guild == env.settings.guild && x.category == env.settings.category(t.kind) && x.topic == marker);
            if possible {
                return "A possible remote channel exists; attach or remove it first. Nothing changed.";
            }
            match env.repo.recovery_release(t.id, a.user, reason.trim(), now).await {
                Ok(true) => "Slot released by audited operator action.",
                _ => "Release refused; nothing changed.",
            }
        }
    }
}

/// Minimal read-only retrieval (full browse/restore is A08): for configured staff or the
/// actual guild owner ONLY, in the configured guild, for a ticket of this guild. Authorization
/// happens BEFORE any archive-reference or file read (zero reads on denial). The recorded
/// archive is re-verified byte-for-byte (manifest hash, transcript, assets) and the reply
/// carries only the reference and integrity result, never message content or file paths.
pub async fn retrieve<I: CloseIo>(env: &Env<'_, I>, a: &Actor, ticket_id: i64) -> String {
    let Ok(Some(t)) = env.repo.ticket_by_id(ticket_id).await else {
        return "Unknown ticket.".into();
    };
    if !staff_or_owner(env.settings, a, t.guild) {
        return "You cannot manage this ticket.".into();
    }
    let Ok(Some(ar)) = env.repo.latest_archive(t.id).await else {
        return "No verified archive is recorded for this ticket.".into();
    };
    // A ticket without a channel is verified against channel id 0.
    match recorded_manifest(env.root, &ar, t.id, t.channel.unwrap_or(0)) {
        Some(m) => format!(
            "Archive {} (ticket status: {}): {} messages, {} assets, manifest sha256 {}. Integrity re-verified: OK.",
            ar.dir_name, t.status, m.message_count, m.assets.len(), ar.manifest_sha256
        ),
        None => "Archive integrity check FAILED (missing or modified). Nothing was changed or exported; keep the ticket record and escalate to the operator.".into(),
    }
}

/// Behavioral tests for this module live in `closer_tests.rs`.
#[cfg(test)]
#[path = "closer_tests.rs"]
mod tests;

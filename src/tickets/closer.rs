//! Authorized ticket closure, startup reconciliation and audited operator recovery.
//!
//! State machine (repository-enforced, token-checked compare-and-set):
//! active -> closing(freeze) -> archiving -> verified -> delete_requested -> closed
//!                                                     \-> delete_uncertain -> closed on KNOWN absence
//! Source deletion is only ever requested after the archive was re-read from disk and its
//! hashes/lengths/ids verified. Every other path retains source channel and per-type slot.
//! No DB transaction is held across any awaited Discord/HTTP/file call.
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
    pub user: u64,
    pub guild: u64,
    pub is_guild_owner: bool,
    pub roles: Vec<u64>,
    /// Channel the command was invoked in.
    pub channel: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Present,
    Absent,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
    Rejected, // definite client rejection, channel still exists
    Unknown,  // timeout / 5xx / transport
}

#[allow(async_fn_in_trait)]
pub trait CloseIo: HistorySource + Downloader {
    /// Narrow requester write freeze (admins can still bypass; this is best effort).
    async fn freeze(&self, channel: u64, requester: u64) -> Result<(), ()>;
    async fn probe(&self, channel: u64) -> Presence;
    async fn delete(&self, channel: u64) -> DeleteOutcome;
    /// Best-effort closure notice; carries no archive.
    async fn dm_closed(&self, requester: u64) -> Result<(), ()>;
    async fn candidates(&self, ticket: &Ticket) -> Result<Vec<RemoteChannel>, ()>;
}

/// Staff role holder or the actual guild owner, in the configured guild, invoking from the
/// ticket's own durable channel. Requesters are not special-cased: they are simply not staff.
pub fn authorized(settings: &Settings, a: &Actor, t: &Ticket) -> bool {
    settings.valid()
        && a.guild == settings.guild
        && t.guild == settings.guild
        && t.channel == Some(a.channel)
        && a.user != 0
        && (a.is_guild_owner || a.roles.contains(&settings.staff))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    Closed,
    Denied,
    Busy,
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

pub struct Env<'a, I: CloseIo> {
    pub settings: &'a Settings,
    pub repo: &'a Repository,
    pub io: &'a I,
    pub root: &'a OwnedRoot,
    pub limits: &'a Limits,
    pub now: &'a (dyn Fn() -> i64 + Send + Sync),
}
const LEASE: i64 = 900;

fn verify_recorded(root: &OwnedRoot, ar: &super::repository_close::ArchiveRef) -> bool {
    recorded_manifest(root, ar).is_some()
}
fn recorded_manifest(
    root: &OwnedRoot,
    ar: &super::repository_close::ArchiveRef,
) -> Option<archive::Manifest> {
    let dir = root.path().join(&ar.dir_name);
    // dir_name came from our own DB but is re-checked as a plain child component.
    if ar.dir_name.contains(['/', '\\', ':']) || ar.dir_name.starts_with('.') {
        return None;
    }
    match archive::verify(root, &dir, Some(&ar.manifest_sha256), &[]) {
        Ok(m) if m.message_count == ar.messages => Some(m),
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

pub async fn close_ticket<I: CloseIo>(
    env: &Env<'_, I>,
    a: &Actor,
    ticket_id: i64,
    token: &str,
) -> CloseOutcome {
    // 1. authorization on guild + durable linkage BEFORE any private read or mutation.
    let Ok(Some(t)) = env.repo.ticket_by_id(ticket_id).await else {
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
        Ok(true) => true,
        Ok(false) => matches!(
            env.repo
                .resume_close(t.id, a.user, token, now, now + LEASE)
                .await,
            Ok(true)
        ),
        Err(_) => false,
    };
    if !claimed {
        return CloseOutcome::Busy;
    }
    let Ok(Some(mut t)) = env.repo.ticket_by_id(t.id).await else {
        return CloseOutcome::Retained("state");
    };
    let Some(channel) = t.channel else {
        return CloseOutcome::Retained("no channel");
    };
    let step = |from: &'static str, to: &'static str, note: &'static str| {
        let (repo, id, tok) = (env.repo, ticket_id, token.to_string());
        let now = (env.now)();
        async move {
            repo.advance(id, &tok, Step { from, to, note }, now, now + LEASE)
                .await
                .unwrap_or(false)
        }
    };
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
        let Ok(seq) = env.repo.next_archive_seq(ticket_id).await else {
            return retain("db").await;
        };
        let nonce = (env.now)() as u64;
        let published = match archive::publish(
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
            Ok(p) => p,
            // publish-before-record recovery: a previous run published this seq but died before
            // recording. Adopt it only if it fully re-verifies AND matches the current source.
            Err(ArchiveError::Exists) => {
                let name = format!("ticket-{ticket_id}-{seq}");
                let dir = env.root.path().join(&name);
                let ids: Vec<u64> = msgs.iter().map(|m| m.id.get()).collect();
                let adopted = std::fs::read(dir.join("manifest.json"))
                    .ok()
                    .map(|b| archive::sha256_hex(&b))
                    .and_then(|sha| {
                        match archive::verify(env.root, &dir, Some(&sha), &ids) {
                            Ok(m)
                                if m.content_sha256 == archive::content_fingerprint(&msgs)
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
                        }
                    });
                match adopted {
                    Some(p) => p,
                    None => return retain("exists").await,
                }
            }
            Err(_) => return retain("publish").await,
        };
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
        let Ok(Some(ar)) = env.repo.latest_archive(ticket_id).await else {
            return retain("no archive").await;
        };
        let Some(manifest) = recorded_manifest(env.root, &ar) else {
            return retain("reverify").await;
        };
        // Immediately before ANY destructive step (also resumed verified/delete states): the
        // live source must still equal the verified archive. Absent-source reconciliation below
        // needs no check (nothing left to delete).
        if t.status != "delete_uncertain" || env.io.probe(channel).await == Presence::Present {
            if !source_unchanged(env, &manifest.content_sha256, manifest.message_count).await {
                // keep source + slot + the old verified archive; next run publishes a new version
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
        }
        if t.status == "delete_uncertain" {
            // never blindly repeat: look first.
            match env.io.probe(channel).await {
                Presence::Absent => {
                    return finish(env, &t, token, ticket_id, "absence reconciled").await;
                }
                Presence::Unknown => return retain("probe").await,
                Presence::Present => {}
            }
            if !step("delete_uncertain", "delete_requested", "retry after probe").await {
                return retain("transition").await;
            }
        } else if t.status == "verified"
            && !step("verified", "delete_requested", "requesting delete").await
        {
            return retain("transition").await;
        }
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
    retain("state").await
}

async fn finish<I: CloseIo>(
    env: &Env<'_, I>,
    t: &Ticket,
    token: &str,
    id: i64,
    note: &'static str,
) -> CloseOutcome {
    let from = match env.repo.ticket_by_id(id).await {
        Ok(Some(x)) => x.status,
        _ => return CloseOutcome::Retained("state"),
    };
    let now = (env.now)();
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
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub moved_to_recovery: usize,
    pub closed_by_known_absence: usize,
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
            ("reserved", _) | ("recovery", _) => r.unresolved += 1,
            ("active", Some(ch)) => {
                if env.io.probe(ch).await == Presence::Absent {
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
                let tok = "reconcile";
                if !env
                    .repo
                    .resume_close(t.id, 0, tok, now, now + LEASE)
                    .await?
                {
                    r.unresolved += 1; // live lease elsewhere or not resumable
                    continue;
                }
                let verified = match env.repo.latest_archive(t.id).await? {
                    Some(ar) => verify_recorded(env.root, &ar),
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
                let _ = env.repo.release_lease(t.id, tok).await;
                r.unresolved += 1;
            }
            _ => r.unresolved += 1,
        }
    }
    Ok(r)
}

// ---------------- operator recovery ----------------
#[derive(Clone, Debug)]
pub enum Recovery {
    Status,
    Attach {
        channel: u64,
        confirm: bool,
        reason: String,
    },
    Release {
        confirm: bool,
        reason: String,
    },
}
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
    if !staff_or_owner(env.settings, a, t.guild) {
        return "You cannot manage this ticket.";
    }
    let now = (env.now)();
    match action {
        Recovery::Status => match t.status.as_str() {
            "creating" | "recovery" | "reserved" => "Ticket operation is unresolved; its slot is retained. Use attach/release with confirmation and a reason.",
            "closed" | "failed" | "released" => "Ticket is finished.",
            _ => "Ticket is open or mid-close; no action needed unless a close stays unresolved.",
        },
        Recovery::Attach { channel, confirm, reason } => {
            if !confirm || reason.trim().is_empty() || t.channel.is_some() || !matches!(t.status.as_str(), "creating" | "recovery") {
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

#[cfg(test)]
#[path = "closer_tests.rs"]
mod tests;

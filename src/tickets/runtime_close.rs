//! Real Serenity adapter + slash-command entrypoints for close / recovery / status, and the
//! startup reconciliation that must complete before opening can ever be approved.
//! Failure replies are fixed strings; remote error bodies are never logged or echoed.
use super::{
    archive::{ArchiveError, Downloader, HistorySource, HttpDownloader, Limits, OwnedRoot},
    closer::{self, Actor, CloseIo, CloseOutcome, DeleteOutcome, Env, Presence, Recovery},
    controller::RemoteChannel,
    policy::Settings,
    repository::{Repository, Ticket},
};
use crate::{Context, Error};
use poise::serenity_prelude as s;
use s::all::{
    ChannelId, CreateAllowedMentions, CreateMessage, GetMessages, MessageId, PermissionOverwrite,
    PermissionOverwriteType, Permissions, UserId,
};

pub struct Live<'a> {
    pub ctx: &'a s::Context,
    pub settings: &'a Settings,
    pub channel: u64,
    dl: HttpDownloader,
}
impl<'a> Live<'a> {
    pub fn new(
        ctx: &'a s::Context,
        settings: &'a Settings,
        channel: u64,
    ) -> Result<Self, ArchiveError> {
        Ok(Self {
            ctx,
            settings,
            channel,
            dl: HttpDownloader::new(Limits::default().download_timeout)?,
        })
    }
}
fn status_of(e: &s::Error) -> Option<u16> {
    match e {
        s::Error::Http(s::HttpError::UnsuccessfulRequest(r)) => Some(r.status_code.as_u16()),
        _ => None,
    }
}
impl HistorySource for Live<'_> {
    async fn page_after(&self, after: u64) -> Result<Vec<s::Message>, ArchiveError> {
        // Explicit ID ordering: always `after` cursor, never "recent 100".
        let builder = GetMessages::new()
            .limit(100)
            .after(MessageId::new(after.max(1)));
        ChannelId::new(self.channel)
            .messages(&self.ctx.http, builder)
            .await
            .map_err(|_| ArchiveError::Source)
    }
}
impl Downloader for Live<'_> {
    async fn fetch(&self, url: &url::Url, limit: u64) -> Result<Vec<u8>, ArchiveError> {
        self.dl.fetch(url, limit).await
    }
}
impl CloseIo for Live<'_> {
    async fn freeze(&self, channel: u64, requester: u64) -> Result<(), ()> {
        // Narrow: only the requester's send/thread/reaction ability. Administrators bypass
        // overwrites and Discord has no atomic snapshot, so the stability probe still applies.
        let ow = PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::SEND_MESSAGES
                | Permissions::ADD_REACTIONS
                | Permissions::CREATE_PUBLIC_THREADS
                | Permissions::CREATE_PRIVATE_THREADS
                | Permissions::SEND_MESSAGES_IN_THREADS,
            kind: PermissionOverwriteType::Member(UserId::new(requester)),
        };
        ChannelId::new(channel)
            .create_permission(&self.ctx.http, ow)
            .await
            .map_err(|_| ())
    }
    async fn probe(&self, channel: u64) -> Presence {
        match ChannelId::new(channel).to_channel(&self.ctx.http).await {
            Ok(_) => Presence::Present,
            Err(e) if status_of(&e) == Some(404) => Presence::Absent,
            Err(_) => Presence::Unknown,
        }
    }
    async fn delete(&self, channel: u64) -> DeleteOutcome {
        match ChannelId::new(channel).delete(&self.ctx.http).await {
            Ok(_) => DeleteOutcome::Deleted,
            Err(e) => match status_of(&e) {
                Some(404) => DeleteOutcome::NotFound,
                Some(400 | 401 | 403) => DeleteOutcome::Rejected,
                _ => DeleteOutcome::Unknown,
            },
        }
    }
    async fn dm_closed(&self, requester: u64) -> Result<(), ()> {
        let dm = UserId::new(requester)
            .create_dm_channel(&self.ctx.http)
            .await
            .map_err(|_| ())?;
        dm.send_message(
            &self.ctx.http,
            CreateMessage::new()
                .content("Your ticket was closed. This is a notice only; no archive is attached.")
                .allowed_mentions(CreateAllowedMentions::new()),
        )
        .await
        .map(|_| ())
        .map_err(|_| ())
    }
    async fn candidates(&self, ticket: &Ticket) -> Result<Vec<RemoteChannel>, ()> {
        let bot = self.ctx.cache.current_user().id.get();
        let chans = s::GuildId::new(self.settings.guild)
            .channels(&self.ctx.http)
            .await
            .map_err(|_| ())?;
        Ok(chans
            .values()
            .filter(|c| c.kind == s::ChannelType::Text)
            .map(|c| {
                let everyone = c.permission_overwrites.iter().any(|p| {
                    p.kind == PermissionOverwriteType::Role(s::RoleId::new(self.settings.guild))
                        && p.deny.contains(Permissions::VIEW_CHANNEL)
                        && !p.allow.contains(Permissions::VIEW_CHANNEL)
                });
                let narrow = c.permission_overwrites.iter().all(|p| {
                    !p.allow.contains(Permissions::VIEW_CHANNEL)
                        || match p.kind {
                            PermissionOverwriteType::Role(id) => id.get() == self.settings.staff,
                            PermissionOverwriteType::Member(id) => {
                                [bot, ticket.opener].contains(&id.get())
                            }
                            _ => false,
                        }
                });
                RemoteChannel {
                    id: c.id.get(),
                    guild: c.guild_id.get(),
                    category: c.parent_id.map(|i| i.get()).unwrap_or(0),
                    topic: c.topic.clone().unwrap_or_default(),
                    private: everyone && narrow,
                }
            })
            .collect())
    }
}

pub fn archive_root(settings: &Settings) -> Result<OwnedRoot, ArchiveError> {
    let base = std::path::Path::new(&settings.db_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    OwnedRoot::open(&base.join("ticket-archives"))
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
async fn actor(ctx: Context<'_>) -> Option<Actor> {
    let guild = ctx.guild_id()?;
    let owner = guild
        .to_partial_guild(&ctx.serenity_context().http)
        .await
        .ok()?
        .owner_id;
    let member = ctx.author_member().await?;
    Some(Actor {
        user: ctx.author().id.get(),
        guild: guild.get(),
        is_guild_owner: ctx.author().id == owner,
        roles: member.roles.iter().map(|r| r.get()).collect(),
        channel: ctx.channel_id().get(),
    })
}
async fn reply(ctx: Context<'_>, text: &str) -> Result<(), Error> {
    ctx.send(
        poise::CreateReply::default()
            .content(text)
            .ephemeral(true)
            .allowed_mentions(CreateAllowedMentions::new()),
    )
    .await?;
    Ok(())
}
const UNAVAILABLE: &str = "Ticket close is unavailable (configuration or storage).";

/// `/ticket-close`: must be run inside the ticket channel by staff or the guild owner.
pub async fn close(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?; // prompt ephemeral ack before any private read
    let (Some(settings), Some(repo)) = (
        ctx.data().config.tickets.as_ref(),
        ctx.data().tickets.as_ref(),
    ) else {
        return reply(ctx, UNAVAILABLE).await;
    };
    let Some(a) = actor(ctx).await else {
        return reply(ctx, CloseOutcome::Denied.reply()).await;
    };
    let Ok(Some(t)) = repo.ticket_by_channel(a.guild, a.channel).await else {
        return reply(ctx, CloseOutcome::Denied.reply()).await;
    };
    let (Ok(root), Ok(io)) = (
        archive_root(settings),
        Live::new(ctx.serenity_context(), settings, a.channel),
    ) else {
        return reply(ctx, UNAVAILABLE).await;
    };
    let limits = Limits::default();
    let clock: &(dyn Fn() -> i64 + Send + Sync) = &now;
    let env = Env {
        settings,
        repo,
        io: &io,
        root: &root,
        limits: &limits,
        now: clock,
    };
    let token = format!("{}-{}", ctx.id(), now());
    reply(
        ctx,
        closer::close_ticket(&env, &a, t.id, &token).await.reply(),
    )
    .await
}

/// `/ticket-recover`: status is read-only; attach/release require confirm + reason.
pub async fn recover(
    ctx: Context<'_>,
    ticket_id: i64,
    action: String,
    channel: Option<u64>,
    confirm: bool,
    reason: Option<String>,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let (Some(settings), Some(repo)) = (
        ctx.data().config.tickets.as_ref(),
        ctx.data().tickets.as_ref(),
    ) else {
        return reply(ctx, UNAVAILABLE).await;
    };
    let Some(a) = actor(ctx).await else {
        return reply(ctx, "You cannot manage this ticket.").await;
    };
    let (Ok(root), Ok(io)) = (
        archive_root(settings),
        Live::new(ctx.serenity_context(), settings, a.channel),
    ) else {
        return reply(ctx, UNAVAILABLE).await;
    };
    let limits = Limits::default();
    let clock: &(dyn Fn() -> i64 + Send + Sync) = &now;
    let env = Env {
        settings,
        repo,
        io: &io,
        root: &root,
        limits: &limits,
        now: clock,
    };
    let reason = reason.unwrap_or_default();
    if action == "retrieve" {
        return reply(ctx, &closer::retrieve(&env, &a, ticket_id).await).await;
    }
    let act = match (action.as_str(), channel) {
        ("status", _) => Recovery::Status,
        ("attach", Some(channel)) => Recovery::Attach {
            channel,
            confirm,
            reason,
        },
        ("release", _) => Recovery::Release { confirm, reason },
        _ => {
            return reply(
                ctx,
                "Unknown action. Use status, retrieve, attach or release.",
            )
            .await
        }
    };
    reply(ctx, closer::recover(&env, &a, ticket_id, act).await).await
}

/// Startup: reconcile BEFORE opening can be allowed. Returns whether opening may be approved
/// given the compile-time rollout flag (the flag stays false in this change).
pub async fn startup_reconcile(
    ctx: &s::Context,
    settings: &Settings,
    repo: &Repository,
) -> Result<closer::Report, Error> {
    let root = archive_root(settings).map_err(|_| "archive root unavailable")?;
    let io = Live::new(ctx, settings, 0).map_err(|_| "http client unavailable")?;
    let limits = Limits::default();
    let clock: &(dyn Fn() -> i64 + Send + Sync) = &now;
    let env = Env {
        settings,
        repo,
        io: &io,
        root: &root,
        limits: &limits,
        now: clock,
    };
    closer::reconcile(&env).await
}

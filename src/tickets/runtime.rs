//! Discord-facing runtime for ticket panels and the opening gate.
//!
//! Responsible for: the process-wide opening gate (hard approval constant AND a clean
//! startup reconciliation), the serenity adapter behind the controller's `Discord` trait,
//! the capability checks in `validate`, and panel publication by the guild owner.
//! Not responsible for: closing or archiving (see `closer`), or enabling ticket opening:
//! the approval constant is false, so opening stays disabled.
//! Trust boundary: component interactions and channel overwrites come from Discord and are
//! treated as untrusted input. Replies built here are private (ephemeral) with mentions
//! suppressed, and panel publication collapses failures into fixed text.
use super::{
    controller::{self, CreateFailure, Discord, RemoteChannel, Request},
    policy::{self, Kind, Settings},
    repository::{Panel, Repository, Ticket},
};
use crate::{Context, Error};
use poise::serenity_prelude as s;
use s::all::{
    ChannelType, CreateAllowedMentions, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, CreateMessage, GuildId,
    PermissionOverwriteType, Permissions,
};

// A07 must replace the conservative event gate with demonstrated private-event
// classification, and add audited recovery/startup reconciliation before release.
/// Hard approval gate for ticket opening. It is false, so `opening_permitted` is false
/// whatever the reconcile state; nothing at runtime changes it.
pub const OPENING_APPROVED: bool = false;

/// Set only by a startup reconciliation that finished with nothing unresolved. Starts false, so
/// a failed or skipped reconcile keeps opening gated regardless of approval.
static RECONCILED_CLEAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Records the outcome of the startup reconciliation in the process-wide flag.
/// `None` (reconcile failed or was skipped) and any report with unresolved rows store false;
/// only a report whose `opening_gate(true)` holds stores true. Fails closed.
pub fn record_reconcile(report: Option<&super::closer::Report>) {
    let clean = report.is_some_and(|r| r.opening_gate(true));
    RECONCILED_CLEAN.store(clean, std::sync::atomic::Ordering::SeqCst);
}
/// The single value every opening path consults (hard gate AND clean reconcile).
pub fn opening_permitted() -> bool {
    permitted(OPENING_APPROVED)
}
/// Same decision with the approval value injected, so tests can exercise both settings.
pub fn permitted(approved: bool) -> bool {
    approved && RECONCILED_CLEAN.load(std::sync::atomic::Ordering::SeqCst)
}

/// Serialises tests that touch the process-wide readiness flag.
#[cfg(test)]
pub static READINESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serenity-backed implementation context for one component interaction.
/// Borrowed for the duration of the interaction only.
pub struct Adapter<'a> {
    pub ctx: &'a s::Context,
    pub interaction: &'a s::ComponentInteraction,
    pub settings: &'a Settings,
}

/// Checks that the configured guild, staff role, category for `kind` and the bot's own
/// permissions are usable before a panel is published.
/// Returns `Err` with a fixed generic message on any configuration mismatch; a failed
/// Discord lookup is propagated as an `Err` too. Fails closed either way.
pub async fn validate(ctx: &s::Context, settings: &Settings, kind: Kind) -> Result<(), Error> {
    // Reject configuration that fails its own validity check before any Discord call.
    if !settings.valid() {
        return Err("invalid ticket configuration".into());
    }
    let guild = GuildId::new(settings.guild)
        .to_partial_guild(&ctx.http)
        .await?;
    // The staff role must exist in the guild and must not be the guild's @everyone role
    // (its id equals the guild id), otherwise staff access would be granted to everyone.
    if !guild.roles.contains_key(&s::RoleId::new(settings.staff))
        || settings.staff == settings.guild
    {
        return Err("invalid staff role".into());
    }
    let category = s::ChannelId::new(settings.category(kind))
        .to_channel(&ctx.http)
        .await?
        .guild()
        .ok_or("category unavailable")?;
    // The category must belong to the configured guild and really be a category channel.
    if category.guild_id.get() != settings.guild || category.kind != ChannelType::Category {
        return Err("invalid ticket category".into());
    }
    // The bot needs every permission it will later use for channel creation and archiving;
    // checked against the category so a missing grant is found before any ticket is attempted.
    let bot = ctx.cache.current_user().id;
    let member = GuildId::new(settings.guild).member(&ctx.http, bot).await?;
    let permissions = guild.user_permissions_in(&category, &member);
    let needed = Permissions::MANAGE_CHANNELS
        | Permissions::MANAGE_ROLES
        | Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::ATTACH_FILES
        | Permissions::EMBED_LINKS;
    if !permissions.contains(needed) {
        return Err("bot capability unavailable".into());
    }
    Ok(())
}

#[s::async_trait]
impl Discord for Adapter<'_> {
    // Acknowledge privately (ephemeral) with no allowed mentions so the deferred reply
    // can never be visible to others or ping anyone.
    async fn defer_private(&self) -> Result<(), Error> {
        self.interaction
            .create_response(
                &self.ctx.http,
                CreateInteractionResponse::Defer(
                    CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .allowed_mentions(CreateAllowedMentions::new()),
                ),
            )
            .await?;
        Ok(())
    }
    async fn feedback_private(&self, text: &str) -> Result<(), Error> {
        self.interaction
            .create_followup(
                &self.ctx.http,
                CreateInteractionResponseFollowup::new()
                    .content(text)
                    .ephemeral(true)
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await?;
        Ok(())
    }
    async fn validate(&self, kind: Kind) -> Result<(), Error> {
        validate(self.ctx, self.settings, kind).await
    }
    // Builds the channel payload from the stored ticket (kind, opener, operation id) and
    // maps a failed create through `classify_create_error` to a `CreateFailure`.
    async fn create(&self, ticket: &Ticket) -> Result<u64, CreateFailure> {
        let bot = self.ctx.cache.current_user().id.get();
        let payload = policy::channel_payload(
            self.settings,
            ticket.kind,
            ticket.opener,
            bot,
            ticket.id,
            &ticket.operation,
        );
        match GuildId::new(self.settings.guild)
            .create_channel(&self.ctx.http, payload)
            .await
        {
            Ok(channel) => Ok(channel.id.get()),
            Err(error) => Err(classify_create_error(&error)),
        }
    }
    // Lists text channels in the guild and marks each `private` only when the @everyone
    // overwrite denies VIEW_CHANNEL without allowing it (`everyone`) AND every overwrite
    // that allows VIEW_CHANNEL belongs to the staff role, the bot or this ticket's opener
    // (`narrow`). Any other allowed viewer, or an unknown overwrite kind, makes it not private.
    async fn channels(&self, ticket: &Ticket) -> Result<Vec<RemoteChannel>, Error> {
        let channels = GuildId::new(self.settings.guild)
            .channels(&self.ctx.http)
            .await?;
        let bot = self.ctx.cache.current_user().id.get();
        Ok(channels
            .values()
            .filter(|c| c.kind == ChannelType::Text)
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
                    category: c.parent_id.map(|id| id.get()).unwrap_or(0),
                    topic: c.topic.clone().unwrap_or_default(),
                    private: everyone && narrow,
                }
            })
            .collect())
    }
    // Greeting posted with mentions suppressed.
    async fn greet(&self, channel: u64, kind: Kind) -> Result<(), Error> {
        s::ChannelId::new(channel)
            .send_message(
                &self.ctx.http,
                CreateMessage::new()
                    .content(policy::greeting(kind))
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await?;
        Ok(())
    }
}
/// Maps a Discord channel-create error to `CreateFailure`.
/// Only an HTTP 400, 401, 403 or 404 rejection is `NotCreated` (a channel is known not to
/// exist, so the slot may be released). Everything else, including timeouts, 429, transport
/// and server errors, is `Unknown` and keeps the slot reserved for recovery.
fn classify_create_error(error: &s::Error) -> CreateFailure {
    // Only explicit client rejections known not to have created a channel release
    // the slot. Timeout, 429, transport and server failures are uncertain.
    if let s::Error::Http(s::HttpError::UnsuccessfulRequest(response)) = error {
        if [400, 401, 403, 404].contains(&response.status_code.as_u16()) {
            return CreateFailure::NotCreated;
        }
    }
    CreateFailure::Unknown
}
/// Entry point for a button interaction: converts it into a controller `Request` and
/// routes it, passing `opening_permitted()` so that the controller refuses to open
/// tickets while the gate is closed. Returns the controller's result unchanged.
pub async fn component(
    ctx: &s::Context,
    interaction: &s::ComponentInteraction,
    settings: &Settings,
    repo: &Repository,
) -> Result<(), Error> {
    let request = Request {
        interaction: interaction.id.get(),
        guild: interaction.guild_id.map(|id| id.get()),
        channel: interaction.channel_id.get(),
        message: interaction.message.id.get(),
        author: interaction.message.author.id.get(),
        author_is_bot: interaction.message.author.bot,
        bot: ctx.cache.current_user().id.get(),
        opener: interaction.user.id.get(),
        custom_id: interaction.data.custom_id.clone(),
    };
    controller::route(
        settings,
        repo,
        &request,
        &Adapter {
            ctx,
            interaction,
            settings,
        },
        opening_permitted(),
    )
    .await
}
/// Owner command that posts the ticket panel in `channel` and records its provenance.
/// Defers privately first. Requires ticket configuration and storage; otherwise replies
/// that they are unavailable. The panel text states that opening is disabled.
/// Any failure inside collapses to one fixed reply that gives no remote detail.
pub async fn publish(ctx: Context<'_>, channel: s::GuildChannel) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(settings) = ctx.data().config.tickets.as_ref() else {
        return private_reply(ctx, "Ticket configuration is unavailable.").await;
    };
    let Some(repo) = ctx.data().tickets.as_ref() else {
        return private_reply(ctx, "Ticket storage is unavailable.").await;
    };
    let result:Result<(),Error>=async {
  // Owner check uses the guild's real owner id and a fresh fetch of the channel (not the
  // possibly stale value passed in), so a mismatch of owner, guild or channel type denies.
  let guild_id=ctx.guild_id().ok_or("guild required")?;
  let guild=guild_id.to_partial_guild(&ctx.serenity_context().http).await?;
  let fresh=channel.id.to_channel(&ctx.serenity_context().http).await?.guild().ok_or("text channel required")?;
  if !policy::owner_can_publish(settings,guild_id.get(),ctx.author().id.get(),guild.owner_id.get(),fresh.guild_id.get(),fresh.kind==ChannelType::Text) {return Err("owner or channel mismatch".into());}
  // Both ticket kinds are validated before anything is posted.
  validate(ctx.serenity_context(),settings,Kind::Support).await?;
  validate(ctx.serenity_context(),settings,Kind::Age).await?;
  let components=vec![s::CreateActionRow::Buttons(vec![s::CreateButton::new(policy::SUPPORT).label("Contact staff privately").style(s::ButtonStyle::Primary),s::CreateButton::new(policy::AGE).label("Request live age verification").style(s::ButtonStyle::Success)])];
  let message=fresh.id.send_message(&ctx.serenity_context().http,CreateMessage::new().content("Ticket requests: support or live camera-on voice verification. Opening is disabled pending rollout review. Never upload ID documents here.").components(components).allowed_mentions(CreateAllowedMentions::new())).await?;
  // Provenance check: the saved panel must be a message authored by this bot itself;
  // otherwise nothing is saved and the posted message stays inert.
  if !message.author.bot || message.author.id!=ctx.serenity_context().cache.current_user().id {return Err("panel origin mismatch".into());}
  repo.save_panel(Panel {guild:settings.guild,channel:message.channel_id.get(),message:message.id.get(),bot:message.author.id.get()}).await?;
  Ok(())
 }.await;
    // Reply wording differs only by success; success text repeats that opening stays disabled.
    private_reply(ctx,if result.is_ok(){"Panel posted and its provenance saved. Ticket opening remains disabled pending rollout review."}else{"Panel publication could not be confirmed. Only the actual server owner may publish in the configured guild; check channel, categories, role and bot permissions. Any unsaved panel is inert."}).await
}
/// Sends an ephemeral reply with mentions suppressed.
async fn private_reply(ctx: Context<'_>, text: &str) -> Result<(), Error> {
    ctx.send(
        poise::CreateReply::default()
            .content(text)
            .ephemeral(true)
            .allowed_mentions(CreateAllowedMentions::new()),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    /// Proves the gate stays closed with no reconcile, with a clean reconcile (hard approval
    /// is false) and with an unresolved one. Fails if `opening_permitted` ignored the constant
    /// or treated a missing or unresolved reconcile as clean.
    #[test]
    fn opening_stays_closed_without_approval_or_clean_reconcile() {
        let _g = READINESS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        record_reconcile(None);
        assert!(!opening_permitted());
        record_reconcile(Some(&super::super::closer::Report::default()));
        // clean reconcile alone never opens while the hard gate is false
        assert!(!opening_permitted());
        record_reconcile(Some(&super::super::closer::Report {
            unresolved: 1,
            ..Default::default()
        }));
        assert!(!opening_permitted());
    }
}

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
pub const OPENING_APPROVED: bool = false;

pub struct Adapter<'a> {
    pub ctx: &'a s::Context,
    pub interaction: &'a s::ComponentInteraction,
    pub settings: &'a Settings,
}

pub async fn validate(ctx: &s::Context, settings: &Settings, kind: Kind) -> Result<(), Error> {
    if !settings.valid() {
        return Err("invalid ticket configuration".into());
    }
    let guild = GuildId::new(settings.guild)
        .to_partial_guild(&ctx.http)
        .await?;
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
    if category.guild_id.get() != settings.guild || category.kind != ChannelType::Category {
        return Err("invalid ticket category".into());
    }
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
        OPENING_APPROVED,
    )
    .await
}
pub async fn publish(ctx: Context<'_>, channel: s::GuildChannel) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(settings) = ctx.data().config.tickets.as_ref() else {
        return private_reply(ctx, "Ticket configuration is unavailable.").await;
    };
    let Some(repo) = ctx.data().tickets.as_ref() else {
        return private_reply(ctx, "Ticket storage is unavailable.").await;
    };
    let result:Result<(),Error>=async {
  let guild_id=ctx.guild_id().ok_or("guild required")?;
  let guild=guild_id.to_partial_guild(&ctx.serenity_context().http).await?;
  let fresh=channel.id.to_channel(&ctx.serenity_context().http).await?.guild().ok_or("text channel required")?;
  if !policy::owner_can_publish(settings,guild_id.get(),ctx.author().id.get(),guild.owner_id.get(),fresh.guild_id.get(),fresh.kind==ChannelType::Text) {return Err("owner or channel mismatch".into());}
  validate(ctx.serenity_context(),settings,Kind::Support).await?;
  validate(ctx.serenity_context(),settings,Kind::Age).await?;
  let components=vec![s::CreateActionRow::Buttons(vec![s::CreateButton::new(policy::SUPPORT).label("Contact staff privately").style(s::ButtonStyle::Primary),s::CreateButton::new(policy::AGE).label("Request live age verification").style(s::ButtonStyle::Success)])];
  let message=fresh.id.send_message(&ctx.serenity_context().http,CreateMessage::new().content("Ticket requests: support or live camera-on voice verification. Opening is disabled pending rollout review. Never upload ID documents here.").components(components).allowed_mentions(CreateAllowedMentions::new())).await?;
  if !message.author.bot || message.author.id!=ctx.serenity_context().cache.current_user().id {return Err("panel origin mismatch".into());}
  repo.save_panel(Panel {guild:settings.guild,channel:message.channel_id.get(),message:message.id.get(),bot:message.author.id.get()}).await?;
  Ok(())
 }.await;
    private_reply(ctx,if result.is_ok(){"Panel posted and its provenance saved. Ticket opening remains disabled pending rollout review."}else{"Panel publication could not be confirmed. Only the actual server owner may publish in the configured guild; check channel, categories, role and bot permissions. Any unsaved panel is inert."}).await
}
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

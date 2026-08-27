use poise::serenity_prelude as serenity;
use poise::serenity_prelude::Mentionable;
use serde::Serialize;
use serenity::all::{
    ButtonStyle, ChannelId, ComponentInteraction, CreateActionRow, CreateButton, CreateChannel,
    CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, GetMessages, GuildChannel, PermissionOverwrite,
    PermissionOverwriteType, Permissions, RoleId,
};
use serenity::model::channel::ChannelType;
use serenity::model::colour::Colour;
use serenity::model::guild::audit_log::{Action, MemberAction};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio::time::sleep;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

const AGE_ROLE_1: u64 = 1428510218643570830;
const AGE_ROLE_2: u64 = 1535363488309059704;
const LOG_CHANNEL_ID: u64 = 1536201073428267040;

const STAFF_ROLE_ID: u64 = 123456789012345678;
const TICKET_CATEGORY_ID: u64 = 0;
const AGE_VERIFICATION_CATEGORY_ID: u64 = 0;

const TICKET_PANEL_GENERAL_BUTTON: &str = "ticket_open_general";
const TICKET_PANEL_AGE_BUTTON: &str = "ticket_open_age_verify";
const TICKET_CLOSE_BUTTON: &str = "ticket_close";

const TICKET_TOPIC_GENERAL_PREFIX: &str = "ticket_opener:";
const TICKET_TOPIC_AGE_PREFIX: &str = "age_verification_opener:";

const TICKET_ARCHIVE_ROOT: &str = "ticket_archives";
const TICKET_ARCHIVE_RECENT_DIR: &str = "ticket_archives/recent";
const TICKET_ARCHIVE_ZIPPED_DIR: &str = "ticket_archives/zipped";
const TICKET_ARCHIVE_TMP_DIR: &str = "ticket_archives/tmp";
const TICKET_FETCH_LIMIT: u8 = 100;

const AUDIT_MATCH_WINDOW_MS: u64 = 8000;
const CACHE_LIMIT: usize = 10000;

struct Data {
    message_cache: Arc<RwLock<HashMap<serenity::MessageId, CachedMessage>>>,
    message_order: Arc<RwLock<VecDeque<serenity::MessageId>>>,
}

#[derive(Clone)]
struct CachedMessage {
    author_id: u64,
    author_tag: String,
    author_avatar_url: Option<String>,
    content: String,
    embeds: Vec<serenity::Embed>,
    is_bot: bool,
}

#[derive(Clone)]
struct AuditActorInfo {
    moderator: String,
    reason: Option<String>,
}

#[derive(Serialize)]
struct ArchivedAttachment {
    filename: String,
    saved_as: String,
    url: String,
    size: u32,
    content_type: Option<String>,
}

#[derive(Serialize)]
struct ArchivedMessage {
    message_id: u64,
    author_id: u64,
    author_name: String,
    timestamp: String,
    content: String,
    attachments: Vec<ArchivedAttachment>,
}

#[derive(Serialize)]
struct TicketArchiveMetadata {
    channel_id: u64,
    channel_name: String,
    opener_id: Option<u64>,
    closed_by_id: u64,
    closed_at: String,
    message_count: usize,
}

fn truncate_for_embed(input: &str, max_len: usize) -> String {
    if input.chars().count() <= max_len {
        input.to_string()
    } else {
        input
            .chars()
            .take(max_len.saturating_sub(3))
            .collect::<String>()
            + "..."
    }
}

fn code_block_or_placeholder(content: &str) -> String {
    let cleaned = if content.trim().is_empty() {
        "*No text content*".to_string()
    } else {
        content.replace("```", "'''")
    };

    format!("```{}\n```", truncate_for_embed(&cleaned, 980))
}

fn json_code_block(value: &str) -> String {
    let cleaned = if value.trim().is_empty() {
        "*No JSON content*".to_string()
    } else {
        value.replace("```", "'''")
    };

    format!("```json\n{}\n```", truncate_for_embed(&cleaned, 3900))
}

fn build_log_footer() -> CreateEmbedFooter {
    CreateEmbedFooter::new("Cabaret Bot • Logger")
}

fn build_author(name: &str, avatar_url: Option<&str>) -> CreateEmbedAuthor {
    let author = CreateEmbedAuthor::new(name);
    if let Some(url) = avatar_url {
        author.icon_url(url)
    } else {
        author
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_user_label(user: &serenity::User) -> String {
    user.name.clone()
}

fn embeds_to_pretty_json(embeds: &[serenity::Embed]) -> String {
    if embeds.is_empty() {
        "*No embeds*".to_string()
    } else {
        serde_json::to_string_pretty(embeds)
            .unwrap_or_else(|_| "*Failed to serialize embeds*".to_string())
    }
}

fn cached_message_from_message(message: &serenity::Message) -> CachedMessage {
    CachedMessage {
        author_id: message.author.id.get(),
        author_tag: format_user_label(&message.author),
        author_avatar_url: message.author.avatar_url(),
        content: message.content.clone(),
        embeds: message.embeds.clone(),
        is_bot: message.author.bot,
    }
}

fn format_voice_channel(channel_id: Option<serenity::ChannelId>) -> String {
    match channel_id {
        Some(id) => format!("<#{}> (`{}`)", id.get(), id.get()),
        None => "Not in a voice channel".to_string(),
    }
}

fn option_bool(value: Option<bool>) -> bool {
    value.unwrap_or(false)
}

fn push_voice_change(changes: &mut Vec<String>, label: &str, before: bool, after: bool) {
    if before != after {
        changes.push(format!(
            "{}: {} -> {}",
            label,
            if before { "On" } else { "Off" },
            if after { "On" } else { "Off" }
        ));
    }
}

fn describe_voice_state_changes(
    old: Option<&serenity::VoiceState>,
    new: &serenity::VoiceState,
) -> Vec<String> {
    let old_self_mute = old.map(|s| s.self_mute).unwrap_or(false);
    let new_self_mute = new.self_mute;

    let old_self_deaf = old.map(|s| s.self_deaf).unwrap_or(false);
    let new_self_deaf = new.self_deaf;

    let old_mute = old.map(|s| s.mute).unwrap_or(false);
    let new_mute = new.mute;

    let old_deaf = old.map(|s| s.deaf).unwrap_or(false);
    let new_deaf = new.deaf;

    let old_stream = old.map(|s| option_bool(s.self_stream)).unwrap_or(false);
    let new_stream = option_bool(new.self_stream);

    let old_video = old.map(|s| s.self_video).unwrap_or(false);
    let new_video = new.self_video;

    let old_suppress = old.map(|s| s.suppress).unwrap_or(false);
    let new_suppress = new.suppress;

    let mut changes = Vec::new();

    push_voice_change(&mut changes, "Self Mute", old_self_mute, new_self_mute);
    push_voice_change(&mut changes, "Self Deaf", old_self_deaf, new_self_deaf);
    push_voice_change(&mut changes, "Server Mute", old_mute, new_mute);
    push_voice_change(&mut changes, "Server Deaf", old_deaf, new_deaf);
    push_voice_change(&mut changes, "Streaming", old_stream, new_stream);
    push_voice_change(&mut changes, "Camera", old_video, new_video);
    push_voice_change(&mut changes, "Suppressed", old_suppress, new_suppress);

    changes
}

fn voice_state_flags(state: &serenity::VoiceState) -> String {
    let mut flags = Vec::new();

    if state.self_mute {
        flags.push("self-muted");
    }
    if state.self_deaf {
        flags.push("self-deafened");
    }
    if state.mute {
        flags.push("server-muted");
    }
    if state.deaf {
        flags.push("server-deafened");
    }
    if state.self_stream.unwrap_or(false) {
        flags.push("streaming");
    }
    if state.self_video {
        flags.push("camera");
    }
    if state.suppress {
        flags.push("suppressed");
    }

    if flags.is_empty() {
        "No special voice flags".to_string()
    } else {
        flags.join(", ")
    }
}

fn channel_kind_name(kind: ChannelType) -> &'static str {
    match kind {
        ChannelType::Text => "Text",
        ChannelType::Voice => "Voice",
        ChannelType::Category => "Category",
        ChannelType::News => "Announcement",
        ChannelType::Stage => "Stage",
        ChannelType::Forum => "Forum",
        ChannelType::NewsThread => "News Thread",
        ChannelType::PublicThread => "Public Thread",
        ChannelType::PrivateThread => "Private Thread",
        ChannelType::Directory => "Directory",
        _ => "Other",
    }
}

fn option_string(value: Option<String>) -> String {
    match value {
        Some(v) if !v.is_empty() => v,
        _ => "*None*".to_string(),
    }
}

fn format_role_list(role_ids: &[serenity::RoleId]) -> String {
    if role_ids.is_empty() {
        "*None*".to_string()
    } else {
        role_ids
            .iter()
            .map(|id| format!("<@&{}>", id.get()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn format_invite_max_age(max_age: u32) -> String {
    if max_age == 0 {
        "Never expires".to_string()
    } else {
        format!("{} seconds", max_age)
    }
}

fn format_invite_max_uses(max_uses: u8) -> String {
    if max_uses == 0 {
        "Unlimited".to_string()
    } else {
        max_uses.to_string()
    }
}

fn format_automod_trigger(rule: &serenity::model::guild::automod::Rule) -> String {
    format!("{:?}", rule.trigger)
}

fn format_automod_event_type(
    event_type: serenity::model::guild::automod::EventType,
) -> &'static str {
    match event_type {
        serenity::model::guild::automod::EventType::MessageSend => "Message Send",
        serenity::model::guild::automod::EventType::Unknown(_) => "Unknown",
        _ => "Other",
    }
}

fn format_automod_action_count(
    actions: &[serenity::model::guild::automod::Action],
) -> String {
    if actions.is_empty() {
        "*None*".to_string()
    } else {
        actions.len().to_string()
    }
}

async fn insert_cache(
    data: &Data,
    message_id: serenity::MessageId,
    cached: CachedMessage,
) {
    let mut cache = data.message_cache.write().await;
    let mut order = data.message_order.write().await;

    if cache.len() >= CACHE_LIMIT {
        while let Some(oldest) = order.pop_front() {
            if cache.remove(&oldest).is_some() {
                break;
            }
        }
    }

    cache.insert(message_id, cached);
    order.push_back(message_id);
}

async fn log_to_channel(
    ctx: &serenity::Context,
    embed: CreateEmbed,
) -> Result<(), serenity::Error> {
    let channel_id = serenity::ChannelId::new(LOG_CHANNEL_ID);

    channel_id
        .send_message(&ctx.http, CreateMessage::new().embed(embed))
        .await?;

    Ok(())
}

async fn find_recent_audit_actor(
    ctx: &serenity::Context,
    guild_id: serenity::GuildId,
    target_user_id: serenity::UserId,
    action: Action,
) -> Result<Option<AuditActorInfo>, serenity::Error> {
    let audit_logs = guild_id
        .audit_logs(&ctx.http, Some(action), None, None, Some(10))
        .await?;

    let now_ms = unix_now_ms();

    for entry in audit_logs.entries {
        let target_matches = entry
            .target_id
            .map(|id| id.get() == target_user_id.get())
            .unwrap_or(false);

        if !target_matches {
            continue;
        }

        let entry_ts_ms = entry.id.created_at().unix_timestamp() as u64 * 1000;
        let age_ms = now_ms.saturating_sub(entry_ts_ms);

        if age_ms <= AUDIT_MATCH_WINDOW_MS {
            return Ok(Some(AuditActorInfo {
                moderator: format!("<@{}>", entry.user_id.get()),
                reason: entry.reason.clone(),
            }));
        }
    }

    Ok(None)
}

async fn find_recent_audit_actor_with_retry(
    ctx: &serenity::Context,
    guild_id: serenity::GuildId,
    target_user_id: serenity::UserId,
    action: Action,
) -> Option<AuditActorInfo> {
    for delay_ms in [0_u64, 1000, 2000, 4000] {
        if delay_ms > 0 {
            sleep(Duration::from_millis(delay_ms)).await;
        }

        if let Ok(Some(info)) =
            find_recent_audit_actor(ctx, guild_id, target_user_id, action).await
        {
            return Some(info);
        }
    }

    None
}

fn safe_fs_name(input: &str) -> String {
    input.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

async fn ensure_dir(path: &Path) -> Result<(), Error> {
    fs::create_dir_all(path).await?;
    Ok(())
}

async fn write_string_file(path: &Path, contents: &str) -> Result<(), Error> {
    let mut file = fs::File::create(path).await?;
    file.write_all(contents.as_bytes()).await?;
    Ok(())
}

async fn initialize_storage() -> Result<(), Error> {
    for path in [
        TICKET_ARCHIVE_ROOT,
        TICKET_ARCHIVE_RECENT_DIR,
        TICKET_ARCHIVE_ZIPPED_DIR,
        TICKET_ARCHIVE_TMP_DIR,
    ] {
        fs::create_dir_all(path).await?;
    }

    println!("Ticket archive storage initialized.");
    Ok(())
}

fn next_ticket_number(
    channels: &HashMap<serenity::ChannelId, GuildChannel>,
    prefix: &str,
) -> u32 {
    channels
        .values()
        .filter_map(|ch| {
            if !ch.name.starts_with(prefix) {
                return None;
            }

            ch.name
                .rsplit('-')
                .next()
                .and_then(|part| part.parse::<u32>().ok())
        })
        .max()
        .unwrap_or(0)
        + 1
}

fn channel_visible_to_user(ch: &GuildChannel, user_id: serenity::UserId) -> bool {
    ch.permission_overwrites.iter().any(|overwrite| {
        matches!(overwrite.kind, PermissionOverwriteType::Member(id) if id == user_id)
            && overwrite.allow.contains(Permissions::VIEW_CHANNEL)
    })
}

fn extract_ticket_opener_id(channel: &GuildChannel) -> Option<serenity::UserId> {
    let topic = channel.topic.as_deref()?;

    let raw_id = topic
        .strip_prefix(TICKET_TOPIC_GENERAL_PREFIX)
        .or_else(|| topic.strip_prefix(TICKET_TOPIC_AGE_PREFIX))?;

    raw_id.parse::<u64>().ok().map(serenity::UserId::new)
}

async fn member_is_staff_or_owner(
    ctx: &serenity::Context,
    guild_id: serenity::GuildId,
    user_id: serenity::UserId,
) -> Result<bool, Error> {
    let guild = guild_id.to_partial_guild(&ctx.http).await?;
    if guild.owner_id == user_id {
        return Ok(true);
    }

    let member = guild_id.member(&ctx.http, user_id).await?;
    Ok(member.roles.iter().any(|role_id| role_id.get() == STAFF_ROLE_ID))
}

async fn dm_ticket_closed(
    ctx: &serenity::Context,
    opener_id: serenity::UserId,
    closer_id: serenity::UserId,
    channel_name: &str,
) {
    if let Ok(user) = opener_id.to_user(&ctx.http).await {
        if let Ok(dm) = user.create_dm_channel(&ctx.http).await {
            let embed = CreateEmbed::new()
                .title("Ticket Closed")
                .description(format!(
                    "Your ticket `{}` has been closed by <@{}>.",
                    channel_name,
                    closer_id.get()
                ))
                .color(Colour::from_rgb(235, 69, 90));

            let _ = dm
                .send_message(&ctx.http, CreateMessage::new().embed(embed))
                .await;
        }
    }
}

async fn archive_ticket_channel(
    channel: &GuildChannel,
    closer_id: serenity::UserId,
) -> Result<(), Error> {
    let opener_id = extract_ticket_opener_id(channel).map(|id| id.get());

    let folder_name = format!("{}-{}", safe_fs_name(&channel.name), channel.id.get());
    let base_dir = PathBuf::from(TICKET_ARCHIVE_RECENT_DIR).join(folder_name);
    let attachments_dir = base_dir.join("attachments");

    ensure_dir(&base_dir).await?;
    ensure_dir(&attachments_dir).await?;

    let metadata = TicketArchiveMetadata {
        channel_id: channel.id.get(),
        channel_name: channel.name.clone(),
        opener_id,
        closed_by_id: closer_id.get(),
        closed_at: serenity::Timestamp::now().to_string(),
        message_count: 0,
    };

    let transcript_md = format!(
        "# Ticket Transcript Placeholder\n\n\
         - Channel: {}\n\
         - Channel ID: {}\n\
         - Closed By: {}\n\
         - Status: Placeholder archive initialized\n\n\
         This placeholder archive tree was generated automatically.\n\
         Full transcript harvesting and attachment downloads can be expanded here next.\n",
        channel.name,
        channel.id.get(),
        closer_id.get(),
    );

    write_string_file(&base_dir.join("transcript.md"), &transcript_md).await?;
    write_string_file(
        &base_dir.join("metadata.json"),
        &serde_json::to_string_pretty(&metadata)?,
    )
    .await?;
    write_string_file(&base_dir.join("messages.json"), "[]").await?;

    Ok(())
}

async fn handle_ticket_button(
    ctx: &serenity::Context,
    interaction: &ComponentInteraction,
) -> Result<(), Error> {
    let guild_id = match interaction.guild_id {
        Some(id) => id,
        None => return Ok(()),
    };

    let user_id = interaction.user.id;
    let is_age_verification = interaction.data.custom_id == TICKET_PANEL_AGE_BUTTON;

    let prefix = if is_age_verification {
        "age-verification-ticket"
    } else {
        "ticket"
    };

    let category_id = if is_age_verification {
        AGE_VERIFICATION_CATEGORY_ID
    } else {
        TICKET_CATEGORY_ID
    };

    let topic_prefix = if is_age_verification {
        TICKET_TOPIC_AGE_PREFIX
    } else {
        TICKET_TOPIC_GENERAL_PREFIX
    };

    let existing_channels = guild_id.channels(&ctx.http).await?;

    if existing_channels.values().any(|ch| {
        ch.kind == ChannelType::Text
            && ch.name.starts_with(prefix)
            && channel_visible_to_user(ch, user_id)
    }) {
        interaction
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("You already have an open ticket of this type.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    let next_number = next_ticket_number(&existing_channels, prefix);
    let channel_name = format!("{prefix}-{next_number:04}");
    let topic = format!("{topic_prefix}{}", user_id.get());

    let mut builder = CreateChannel::new(channel_name)
        .kind(ChannelType::Text)
        .topic(topic)
        .permissions(vec![
            PermissionOverwrite {
                allow: Permissions::VIEW_CHANNEL
                    | Permissions::SEND_MESSAGES
                    | Permissions::READ_MESSAGE_HISTORY
                    | Permissions::ATTACH_FILES
                    | Permissions::EMBED_LINKS,
                deny: Permissions::empty(),
                kind: PermissionOverwriteType::Member(user_id),
            },
            PermissionOverwrite {
                allow: Permissions::VIEW_CHANNEL
                    | Permissions::SEND_MESSAGES
                    | Permissions::READ_MESSAGE_HISTORY
                    | Permissions::ATTACH_FILES
                    | Permissions::EMBED_LINKS,
                deny: Permissions::empty(),
                kind: PermissionOverwriteType::Role(RoleId::new(STAFF_ROLE_ID)),
            },
            PermissionOverwrite {
                allow: Permissions::empty(),
                deny: Permissions::VIEW_CHANNEL,
                kind: PermissionOverwriteType::Role(RoleId::new(guild_id.get())),
            },
        ]);

    if category_id != 0 {
        builder = builder.category(ChannelId::new(category_id));
    }

    let created_channel = guild_id.create_channel(&ctx.http, builder).await?;

    let ticket_embed = if is_age_verification {
        CreateEmbed::new()
            .title("Age Verification Ticket")
            .description(
                "A staff member will assist you shortly.\n\
                 Please have your ID ready. You may cover any sensitive information with tape, but staff must still be able to verify your picture and date of birth. Age verification will ONLY be conducted on VC there are no exceptions to this rule."
            )
            .color(Colour::from_rgb(87, 242, 135))
    } else {
        CreateEmbed::new()
            .title("Private Staff Ticket")
            .description(
                "Thank you for contacting support.\n\
                 Please describe your issue and wait for a response."
            )
            .color(Colour::from_rgb(88, 101, 242))
    };

    let components = vec![CreateActionRow::Buttons(vec![
        CreateButton::new(TICKET_CLOSE_BUTTON)
            .label("Close")
            .style(ButtonStyle::Danger),
    ])];

    let ping_message = format!(
        "{} <@&{}> thanks for opening a ticket.",
        interaction.user.mention(),
        STAFF_ROLE_ID
    );

    created_channel
        .id
        .send_message(
            &ctx.http,
            CreateMessage::new()
                .content(ping_message)
                .embed(ticket_embed)
                .components(components),
        )
        .await?;

    interaction
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!(
                        "Your ticket has been created: {}",
                        created_channel.id.mention()
                    ))
                    .ephemeral(true),
            ),
        )
        .await?;

    Ok(())
}

async fn handle_ticket_close_button(
    ctx: &serenity::Context,
    interaction: &ComponentInteraction,
) -> Result<(), Error> {
    let guild_id = match interaction.guild_id {
        Some(id) => id,
        None => return Ok(()),
    };

    let channel_id = interaction.channel_id;
    let channel = channel_id.to_channel(&ctx.http).await?;

    let guild_channel = match channel.guild() {
        Some(ch) => ch,
        None => {
            interaction
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("This button only works in ticket channels.")
                            .ephemeral(true),
                    ),
                )
                .await?;
            return Ok(());
        }
    };

    let allowed = member_is_staff_or_owner(ctx, guild_id, interaction.user.id).await?;
    if !allowed {
        interaction
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("Only staff can close tickets.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    interaction
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("Archiving and closing ticket...")
                    .ephemeral(true),
            ),
        )
        .await?;

    let opener_id = extract_ticket_opener_id(&guild_channel);

    archive_ticket_channel(&guild_channel, interaction.user.id).await?;

    if let Some(opener_id) = opener_id {
        dm_ticket_closed(ctx, opener_id, interaction.user.id, &guild_channel.name).await;
    }

    let closed_embed = CreateEmbed::new()
        .title("Ticket Closed")
        .description(format!(
            "This ticket was closed by {}.\nThe placeholder archive has been written to disk.\nThis channel will be deleted in 5 seconds.",
            interaction.user.mention()
        ))
        .color(Colour::from_rgb(235, 69, 90));

    channel_id
        .send_message(&ctx.http, CreateMessage::new().embed(closed_embed))
        .await?;

    sleep(Duration::from_secs(5)).await;
    channel_id.delete(&ctx.http).await?;

    Ok(())
}

/// Mark the user as age verified.
#[poise::command(
    slash_command,
    guild_only,
    rename = "age-verified",
    required_permissions = "MANAGE_ROLES",
    default_member_permissions = "MANAGE_ROLES",
    required_bot_permissions = "MANAGE_ROLES"
)]
async fn age_verified(
    ctx: Context<'_>,
    #[description = "The user to mark as age verified"] user: serenity::Member,
) -> Result<(), Error> {
    let http = ctx.serenity_context().http.clone();

    let role1 = serenity::RoleId::new(AGE_ROLE_1);
    let role2 = serenity::RoleId::new(AGE_ROLE_2);

    user.add_role(&http, role1).await?;
    user.add_role(&http, role2).await?;

    ctx.say(format!(
        "{} has been marked as age verified and given the age verified roles.",
        user.mention()
    ))
    .await?;

    Ok(())
}

#[poise::command(slash_command, guild_only, rename = "ticket-panel-post")]
async fn ticket_panel_post(
    ctx: Context<'_>,
    #[description = "Channel to post the ticket panel in"] channel: serenity::Channel,
) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .ok_or("This command can only be used in a server")?;
    let guild = guild_id.to_partial_guild(&ctx.serenity_context().http).await?;

    if ctx.author().id != guild.owner_id {
        ctx.send(
            poise::CreateReply::default()
                .content("Only the server owner can use this command.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    let guild_channel = match channel.guild() {
        Some(ch) if ch.kind == ChannelType::Text => ch,
        _ => {
            ctx.send(
                poise::CreateReply::default()
                    .content("Please choose a text channel.")
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    let embed = CreateEmbed::new()
        .title("Open a Ticket")
        .description(
            "Use the buttons below to open a private ticket.\n\n\
             **Contact staff privately** opens a normal support ticket.\n\
             **Open an age verification ticket** opens a private age verification ticket."
        )
        .color(Colour::from_rgb(88, 101, 242));

    let components = vec![CreateActionRow::Buttons(vec![
        CreateButton::new(TICKET_PANEL_GENERAL_BUTTON)
            .label("Contact staff privately")
            .style(ButtonStyle::Primary),
        CreateButton::new(TICKET_PANEL_AGE_BUTTON)
            .label("Open an age verification ticket")
            .style(ButtonStyle::Success),
    ])];

    guild_channel
        .id
        .send_message(
            &ctx.serenity_context().http,
            CreateMessage::new().embed(embed).components(components),
        )
        .await?;

    ctx.send(
        poise::CreateReply::default()
            .content(format!("Ticket panel posted in {}.", guild_channel.id.mention()))
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

async fn event_handler(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    _framework: poise::FrameworkContext<'_, Data, Error>,
    data: &Data,
) -> Result<(), Error> {
    match event {
        serenity::FullEvent::InteractionCreate { interaction } => {
            if let serenity::Interaction::Component(component) = interaction {
                match component.data.custom_id.as_str() {
                    TICKET_PANEL_GENERAL_BUTTON | TICKET_PANEL_AGE_BUTTON => {
                        handle_ticket_button(ctx, component).await?;
                    }
                    TICKET_CLOSE_BUTTON => {
                        handle_ticket_close_button(ctx, component).await?;
                    }
                    _ => {}
                }
            }
        }

        serenity::FullEvent::Message { new_message } => {
            if new_message.channel_id.get() == LOG_CHANNEL_ID {
                return Ok(());
            }

            let cached = cached_message_from_message(new_message);
            insert_cache(data, new_message.id, cached).await;
        }

        serenity::FullEvent::MessageDelete {
            channel_id,
            deleted_message_id,
            guild_id: _,
        } => {
            let cached = data.message_cache.write().await.remove(deleted_message_id);

            if let Some(cached) = cached {
                let embed = CreateEmbed::new()
                    .author(build_author(
                        &cached.author_tag,
                        cached.author_avatar_url.as_deref(),
                    ))
                    .title("Message Deleted")
                    .color(Colour::DARK_RED)
                    .field(
                        "Channel",
                        format!("<#{}> (`{}`)", channel_id.get(), channel_id.get()),
                        false,
                    )
                    .field("Author ID", format!("`{}`", cached.author_id), true)
                    .field(
                        "Author Type",
                        if cached.is_bot { "Bot" } else { "User" },
                        true,
                    )
                    .field("Content", code_block_or_placeholder(&cached.content), false)
                    .footer(build_log_footer())
                    .timestamp(serenity::Timestamp::now());

                let _ = log_to_channel(ctx, embed).await;
            } else {
                let embed = CreateEmbed::new()
                    .title("Message Deleted")
                    .color(Colour::DARK_RED)
                    .field(
                        "Channel",
                        format!("<#{}> (`{}`)", channel_id.get(), channel_id.get()),
                        false,
                    )
                    .field("Message ID", format!("`{}`", deleted_message_id.get()), false)
                    .description("Original message data was not available in cache.")
                    .footer(build_log_footer())
                    .timestamp(serenity::Timestamp::now());

                let _ = log_to_channel(ctx, embed).await;
            }
        }

        serenity::FullEvent::MessageDeleteBulk {
            channel_id,
            multiple_deleted_messages_ids,
            guild_id: _,
        } => {
            if channel_id.get() == LOG_CHANNEL_ID {
                return Ok(());
            }

            let count = multiple_deleted_messages_ids.len();

            let preview = multiple_deleted_messages_ids
                .iter()
                .take(10)
                .map(|id| format!("`{}`", id.get()))
                .collect::<Vec<_>>()
                .join(", ");

            let extra = if count > 10 {
                format!("\nAnd {} more...", count - 10)
            } else {
                String::new()
            };

            let embed = CreateEmbed::new()
                .title("Bulk Message Delete")
                .color(Colour::DARK_RED)
                .field(
                    "Channel",
                    format!("<#{}> (`{}`)", channel_id.get(), channel_id.get()),
                    false,
                )
                .field("Deleted Messages", format!("`{}`", count), true)
                .field(
                    "Message IDs",
                    if preview.is_empty() {
                        "*No message IDs provided*".to_string()
                    } else {
                        format!("{}{}", preview, extra)
                    },
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::MessageUpdate {
            old_if_available,
            new,
            event,
        } => {
            if event.channel_id.get() == LOG_CHANNEL_ID {
                return Ok(());
            }

            let existing_cache = data.message_cache.read().await.get(&event.id).cloned();

            let author_name = existing_cache
                .as_ref()
                .map(|c| c.author_tag.clone())
                .or_else(|| new.as_ref().map(|m| format_user_label(&m.author)))
                .or_else(|| old_if_available.as_ref().map(|m| format_user_label(&m.author)))
                .unwrap_or_else(|| "Unknown User".to_string());

            let author_avatar = existing_cache
                .as_ref()
                .and_then(|c| c.author_avatar_url.clone())
                .or_else(|| new.as_ref().and_then(|m| m.author.avatar_url()))
                .or_else(|| old_if_available.as_ref().and_then(|m| m.author.avatar_url()));

            let author_id = existing_cache
                .as_ref()
                .map(|c| c.author_id)
                .or_else(|| new.as_ref().map(|m| m.author.id.get()))
                .or_else(|| old_if_available.as_ref().map(|m| m.author.id.get()))
                .unwrap_or(0);

            let is_bot = existing_cache
                .as_ref()
                .map(|c| c.is_bot)
                .or_else(|| new.as_ref().map(|m| m.author.bot))
                .or_else(|| old_if_available.as_ref().map(|m| m.author.bot))
                .unwrap_or(false);

            let before = existing_cache
                .as_ref()
                .map(|c| c.content.clone())
                .or_else(|| old_if_available.as_ref().map(|m| m.content.clone()))
                .unwrap_or_default();

            let after = new
                .as_ref()
                .map(|m| m.content.clone())
                .or_else(|| event.content.clone())
                .unwrap_or_default();

            let before_embeds = existing_cache
                .as_ref()
                .map(|c| c.embeds.clone())
                .or_else(|| old_if_available.as_ref().map(|m| m.embeds.clone()))
                .unwrap_or_default();

            let after_embeds = new
                .as_ref()
                .map(|m| m.embeds.clone())
                .or_else(|| event.embeds.clone())
                .unwrap_or_default();

            let text_changed = before != after;
            let embeds_changed = before_embeds != after_embeds;

            let before_trimmed = before.trim();
            let after_trimmed = after.trim();

            if !text_changed && !embeds_changed {
                return Ok(());
            }

            let before_display = if before_trimmed.is_empty() {
                "*No text content*".to_string()
            } else {
                before.clone()
            };

            let after_display = if after_trimmed.is_empty() {
                "*No text content*".to_string()
            } else {
                after.clone()
            };

            let before_embed_json = embeds_to_pretty_json(&before_embeds);
            let after_embed_json = embeds_to_pretty_json(&after_embeds);

            let mut embed = CreateEmbed::new()
                .author(build_author(&author_name, author_avatar.as_deref()))
                .title("Message Updated")
                .color(Colour::ORANGE)
                .field(
                    "Channel",
                    format!("<#{}> (`{}`)", event.channel_id.get(), event.channel_id.get()),
                    false,
                )
                .field(
                    "Author",
                    if author_id == 0 {
                        author_name.clone()
                    } else {
                        format!("{} (`{}`)", author_name, author_id)
                    },
                    false,
                )
                .field("Text Changed", if text_changed { "Yes" } else { "No" }, true)
                .field(
                    "Embeds Changed",
                    if embeds_changed { "Yes" } else { "No" },
                    true,
                );

            if text_changed {
                embed = embed
                    .field("Before Text", code_block_or_placeholder(&before_display), false)
                    .field("After Text", code_block_or_placeholder(&after_display), false);
            }

            if embeds_changed {
                embed = embed
                    .field("Before Embeds", json_code_block(&before_embed_json), false)
                    .field("After Embeds", json_code_block(&after_embed_json), false);
            }

            embed = embed
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;

            let cached = CachedMessage {
                author_id,
                author_tag: author_name,
                author_avatar_url: author_avatar,
                content: after,
                embeds: after_embeds,
                is_bot,
            };

            insert_cache(data, event.id, cached).await;
        }

        serenity::FullEvent::VoiceStateUpdate { old, new } => {
            let user_id = new.user_id;
            let old_channel = old.as_ref().and_then(|s| s.channel_id);
            let new_channel = new.channel_id;

            let member = if let Some(guild_id) = new.guild_id {
                guild_id.member(&ctx.http, user_id).await.ok()
            } else {
                None
            };

            let author_name = member
                .as_ref()
                .map(|m| m.user.name.clone())
                .unwrap_or_else(|| format!("User {}", user_id.get()));

            let author_avatar = member.as_ref().and_then(|m| m.user.avatar_url());

            if old_channel != new_channel {
                let title = if old_channel.is_none() && new_channel.is_some() {
                    "Voice Joined"
                } else if old_channel.is_some() && new_channel.is_none() {
                    "Voice Left"
                } else {
                    "Voice Moved"
                };

                let embed = CreateEmbed::new()
                    .author(build_author(&author_name, author_avatar.as_deref()))
                    .title(title)
                    .color(Colour::BLUE)
                    .field("Member", format!("<@{}>", user_id.get()), true)
                    .field("User ID", format!("`{}`", user_id.get()), true)
                    .field("Before", format_voice_channel(old_channel), false)
                    .field("After", format_voice_channel(new_channel), false)
                    .field("State", voice_state_flags(new), false)
                    .footer(build_log_footer())
                    .timestamp(serenity::Timestamp::now());

                let _ = log_to_channel(ctx, embed).await;
                return Ok(());
            }

            let changes = describe_voice_state_changes(old.as_ref(), new);

            if changes.is_empty() {
                return Ok(());
            }

            let embed = CreateEmbed::new()
                .author(build_author(&author_name, author_avatar.as_deref()))
                .title("Voice Status Updated")
                .color(Colour::from_rgb(0, 170, 170))
                .field("Member", format!("<@{}>", user_id.get()), true)
                .field("User ID", format!("`{}`", user_id.get()), true)
                .field("Channel", format_voice_channel(new_channel), false)
                .field("Changes", changes.join("\n"), false)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::ChannelCreate { channel } => {
            let embed = CreateEmbed::new()
                .title("Channel Created")
                .color(Colour::DARK_GREEN)
                .field(
                    "Channel",
                    format!("<#{}> (`{}`)", channel.id.get(), channel.id.get()),
                    false,
                )
                .field("Name", channel.name.clone(), true)
                .field("Type", channel_kind_name(channel.kind), true)
                .field(
                    "Category",
                    match channel.parent_id {
                        Some(id) => format!("<#{}> (`{}`)", id.get(), id.get()),
                        None => "*None*".to_string(),
                    },
                    false,
                )
                .field("NSFW", if channel.nsfw { "Yes" } else { "No" }, true)
                .field("Topic", option_string(channel.topic.clone()), false)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::ChannelDelete { channel, messages: _ } => {
            let embed = CreateEmbed::new()
                .title("Channel Deleted")
                .color(Colour::DARK_RED)
                .field("Channel ID", format!("`{}`", channel.id.get()), true)
                .field("Name", channel.name.clone(), true)
                .field("Type", channel_kind_name(channel.kind), true)
                .field(
                    "Category",
                    match channel.parent_id {
                        Some(id) => format!("<#{}> (`{}`)", id.get(), id.get()),
                        None => "*None*".to_string(),
                    },
                    false,
                )
                .field("NSFW", if channel.nsfw { "Yes" } else { "No" }, true)
                .field("Topic", option_string(channel.topic.clone()), false)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::ChannelUpdate { old, new } => {
            let old_name = old
                .as_ref()
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "*Unknown*".to_string());
            let new_name = new.name.clone();

            let old_kind = old
                .as_ref()
                .map(|c| channel_kind_name(c.kind).to_string())
                .unwrap_or_else(|| "*Unknown*".to_string());
            let new_kind = channel_kind_name(new.kind).to_string();

            let old_topic = old.as_ref().and_then(|c| c.topic.clone());
            let new_topic = new.topic.clone();

            let old_nsfw = old.as_ref().map(|c| c.nsfw).unwrap_or(false);
            let new_nsfw = new.nsfw;

            let old_parent = old.as_ref().and_then(|c| c.parent_id);
            let new_parent = new.parent_id;

            let embed = CreateEmbed::new()
                .title("Channel Updated")
                .color(Colour::ORANGE)
                .field(
                    "Channel",
                    format!("<#{}> (`{}`)", new.id.get(), new.id.get()),
                    false,
                )
                .field("Name Before", old_name, true)
                .field("Name After", new_name, true)
                .field("Type Before", old_kind, true)
                .field("Type After", new_kind, true)
                .field("Topic Before", option_string(old_topic), false)
                .field("Topic After", option_string(new_topic), false)
                .field("NSFW Before", if old_nsfw { "Yes" } else { "No" }, true)
                .field("NSFW After", if new_nsfw { "Yes" } else { "No" }, true)
                .field(
                    "Category Before",
                    match old_parent {
                        Some(id) => format!("<#{}> (`{}`)", id.get(), id.get()),
                        None => "*None*".to_string(),
                    },
                    false,
                )
                .field(
                    "Category After",
                    match new_parent {
                        Some(id) => format!("<#{}> (`{}`)", id.get(), id.get()),
                        None => "*None*".to_string(),
                    },
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::GuildRoleCreate { new } => {
            let embed = CreateEmbed::new()
                .title("Role Created")
                .color(Colour::DARK_GREEN)
                .field("Role", format!("<@&{}>", new.id.get()), true)
                .field("Role ID", format!("`{}`", new.id.get()), true)
                .field("Name", new.name.clone(), true)
                .field("Color", format!("`#{:06X}`", new.colour.0), true)
                .field("Hoisted", if new.hoist { "Yes" } else { "No" }, true)
                .field("Mentionable", if new.mentionable { "Yes" } else { "No" }, true)
                .field("Permissions", format!("`{}`", new.permissions.bits()), false)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::GuildRoleDelete {
            guild_id: _,
            removed_role_id,
            removed_role_data_if_available,
        } => {
            let role_name = removed_role_data_if_available
                .as_ref()
                .map(|r| r.name.clone())
                .unwrap_or_else(|| "*Unknown*".to_string());

            let role_color = removed_role_data_if_available
                .as_ref()
                .map(|r| format!("`#{:06X}`", r.colour.0))
                .unwrap_or_else(|| "*Unknown*".to_string());

            let embed = CreateEmbed::new()
                .title("Role Deleted")
                .color(Colour::DARK_RED)
                .field("Role ID", format!("`{}`", removed_role_id.get()), true)
                .field("Name", role_name, true)
                .field("Color", role_color, true)
                .field(
                    "Mentionable",
                    removed_role_data_if_available
                        .as_ref()
                        .map(|r| if r.mentionable { "Yes" } else { "No" })
                        .unwrap_or("*Unknown*"),
                    true,
                )
                .field(
                    "Hoisted",
                    removed_role_data_if_available
                        .as_ref()
                        .map(|r| if r.hoist { "Yes" } else { "No" })
                        .unwrap_or("*Unknown*"),
                    true,
                )
                .field(
                    "Permissions",
                    removed_role_data_if_available
                        .as_ref()
                        .map(|r| format!("`{}`", r.permissions.bits()))
                        .unwrap_or_else(|| "*Unknown*".to_string()),
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::GuildRoleUpdate {
            old_data_if_available,
            new,
        } => {
            let old_name = old_data_if_available
                .as_ref()
                .map(|r| r.name.clone())
                .unwrap_or_else(|| "*Unknown*".to_string());

            let old_color = old_data_if_available
                .as_ref()
                .map(|r| format!("`#{:06X}`", r.colour.0))
                .unwrap_or_else(|| "*Unknown*".to_string());

            let old_hoist = old_data_if_available.as_ref().map(|r| r.hoist).unwrap_or(false);
            let old_mentionable = old_data_if_available
                .as_ref()
                .map(|r| r.mentionable)
                .unwrap_or(false);

            let old_permissions = old_data_if_available
                .as_ref()
                .map(|r| format!("`{}`", r.permissions.bits()))
                .unwrap_or_else(|| "*Unknown*".to_string());

            let embed = CreateEmbed::new()
                .title("Role Updated")
                .color(Colour::ORANGE)
                .field("Role", format!("<@&{}>", new.id.get()), true)
                .field("Role ID", format!("`{}`", new.id.get()), true)
                .field("Name Before", old_name, true)
                .field("Name After", new.name.clone(), true)
                .field("Color Before", old_color, true)
                .field("Color After", format!("`#{:06X}`", new.colour.0), true)
                .field("Hoisted Before", if old_hoist { "Yes" } else { "No" }, true)
                .field("Hoisted After", if new.hoist { "Yes" } else { "No" }, true)
                .field(
                    "Mentionable Before",
                    if old_mentionable { "Yes" } else { "No" },
                    true,
                )
                .field(
                    "Mentionable After",
                    if new.mentionable { "Yes" } else { "No" },
                    true,
                )
                .field("Permissions Before", old_permissions, false)
                .field("Permissions After", format!("`{}`", new.permissions.bits()), false)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::GuildMemberAddition { new_member } => {
            let avatar = new_member.user.avatar_url();
            let embed = CreateEmbed::new()
                .author(build_author(
                    &format_user_label(&new_member.user),
                    avatar.as_deref(),
                ))
                .title("Member Joined")
                .color(Colour::DARK_GREEN)
                .field("Member", format!("<@{}>", new_member.user.id.get()), true)
                .field("User ID", format!("`{}`", new_member.user.id.get()), true)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::GuildMemberUpdate {
            old_if_available,
            new,
            event,
        } => {
            let user_id = event.user.id;
            let old_nick = old_if_available.as_ref().and_then(|m| m.nick.clone());
            let new_nick = new
                .as_ref()
                .and_then(|m| m.nick.clone())
                .or_else(|| event.nick.clone());

            let old_roles = old_if_available
                .as_ref()
                .map(|m| m.roles.clone())
                .unwrap_or_default();

            let new_roles = new
                .as_ref()
                .map(|m| m.roles.clone())
                .unwrap_or_else(|| event.roles.clone());

            let old_pending = old_if_available.as_ref().map(|m| m.pending).unwrap_or(false);
            let new_pending = new.as_ref().map(|m| m.pending).unwrap_or(event.pending);

            let old_timeout = old_if_available
                .as_ref()
                .and_then(|m| m.communication_disabled_until);

            let new_timeout = new
                .as_ref()
                .and_then(|m| m.communication_disabled_until)
                .or(event.communication_disabled_until);

            let display_name = new
                .as_ref()
                .map(|m| m.user.name.clone())
                .unwrap_or_else(|| event.user.name.clone());

            let avatar = new
                .as_ref()
                .and_then(|m| m.user.avatar_url())
                .or_else(|| event.user.avatar_url());

            let embed = CreateEmbed::new()
                .author(build_author(&display_name, avatar.as_deref()))
                .title("Member Updated")
                .color(Colour::ORANGE)
                .field("Member", format!("<@{}>", user_id.get()), true)
                .field("User ID", format!("`{}`", user_id.get()), true)
                .field(
                    "Nickname Before",
                    old_nick.unwrap_or_else(|| "*None*".to_string()),
                    true,
                )
                .field(
                    "Nickname After",
                    new_nick.unwrap_or_else(|| "*None*".to_string()),
                    true,
                )
                .field("Roles Before", format_role_list(&old_roles), false)
                .field("Roles After", format_role_list(&new_roles), false)
                .field("Pending Before", if old_pending { "Yes" } else { "No" }, true)
                .field("Pending After", if new_pending { "Yes" } else { "No" }, true)
                .field(
                    "Timeout Before",
                    old_timeout
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "*None*".to_string()),
                    false,
                )
                .field(
                    "Timeout After",
                    new_timeout
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "*None*".to_string()),
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::GuildMemberRemoval {
            guild_id,
            user,
            member_data_if_available: _,
        } => {
            let avatar = user.avatar_url();

            sleep(Duration::from_millis(2000)).await;

            if let Some(info) = find_recent_audit_actor_with_retry(
                ctx,
                *guild_id,
                user.id,
                Action::Member(MemberAction::BanAdd),
            )
            .await
            {
                let embed = CreateEmbed::new()
                    .author(build_author(&format_user_label(user), avatar.as_deref()))
                    .title("Member Banned")
                    .color(Colour::from_rgb(180, 32, 42))
                    .field("Member", format!("<@{}>", user.id.get()), true)
                    .field("User ID", format!("`{}`", user.id.get()), true)
                    .field("Moderator", info.moderator, false)
                    .field(
                        "Reason",
                        info.reason
                            .unwrap_or_else(|| "*No reason provided*".to_string()),
                        false,
                    )
                    .footer(build_log_footer())
                    .timestamp(serenity::Timestamp::now());

                let _ = log_to_channel(ctx, embed).await;
                return Ok(());
            }

            if let Some(info) = find_recent_audit_actor_with_retry(
                ctx,
                *guild_id,
                user.id,
                Action::Member(MemberAction::Kick),
            )
            .await
            {
                let embed = CreateEmbed::new()
                    .author(build_author(&format_user_label(user), avatar.as_deref()))
                    .title("Member Kicked")
                    .color(Colour::RED)
                    .field("Member", format!("<@{}>", user.id.get()), true)
                    .field("User ID", format!("`{}`", user.id.get()), true)
                    .field("Moderator", info.moderator, false)
                    .field(
                        "Reason",
                        info.reason
                            .unwrap_or_else(|| "*No reason provided*".to_string()),
                        false,
                    )
                    .footer(build_log_footer())
                    .timestamp(serenity::Timestamp::now());

                let _ = log_to_channel(ctx, embed).await;
                return Ok(());
            }

            let embed = CreateEmbed::new()
                .author(build_author(&format_user_label(user), avatar.as_deref()))
                .title("Member Left")
                .color(Colour::DARK_GREY)
                .field("Member", format!("<@{}>", user.id.get()), true)
                .field("User ID", format!("`{}`", user.id.get()), true)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::InviteCreate { data } => {
            let embed = CreateEmbed::new()
                .title("Invite Created")
                .color(Colour::DARK_GREEN)
                .field("Code", format!("`{}`", data.code), true)
                .field(
                    "Channel",
                    format!("<#{}> (`{}`)", data.channel_id.get(), data.channel_id.get()),
                    false,
                )
                .field(
                    "Inviter",
                    data.inviter
                        .as_ref()
                        .map(|u| format!("<@{}> (`{}`)", u.id.get(), u.id.get()))
                        .unwrap_or_else(|| "*Unknown*".to_string()),
                    false,
                )
                .field("Temporary", if data.temporary { "Yes" } else { "No" }, true)
                .field("Max Uses", format_invite_max_uses(data.max_uses), true)
                .field("Max Age", format_invite_max_age(data.max_age), true)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::InviteDelete { data } => {
            let embed = CreateEmbed::new()
                .title("Invite Deleted")
                .color(Colour::DARK_RED)
                .field("Code", format!("`{}`", data.code), true)
                .field(
                    "Channel",
                    format!("<#{}> (`{}`)", data.channel_id.get(), data.channel_id.get()),
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::GuildBanAddition { guild_id, banned_user } => {
            let avatar = banned_user.avatar_url();

            let audit = find_recent_audit_actor_with_retry(
                ctx,
                *guild_id,
                banned_user.id,
                Action::Member(MemberAction::BanAdd),
            )
            .await;

            let embed = CreateEmbed::new()
                .author(build_author(
                    &format_user_label(banned_user),
                    avatar.as_deref(),
                ))
                .title("Member Banned")
                .color(Colour::from_rgb(180, 32, 42))
                .field("Member", format!("<@{}>", banned_user.id.get()), true)
                .field("User ID", format!("`{}`", banned_user.id.get()), true)
                .field(
                    "Moderator",
                    audit.as_ref()
                        .map(|a| a.moderator.clone())
                        .unwrap_or_else(|| "Unknown".to_string()),
                    false,
                )
                .field(
                    "Reason",
                    audit.and_then(|a| a.reason)
                        .unwrap_or_else(|| "*No reason provided*".to_string()),
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::GuildBanRemoval {
            guild_id,
            unbanned_user,
        } => {
            let avatar = unbanned_user.avatar_url();

            let audit = find_recent_audit_actor_with_retry(
                ctx,
                *guild_id,
                unbanned_user.id,
                Action::Member(MemberAction::BanRemove),
            )
            .await;

            let embed = CreateEmbed::new()
                .author(build_author(
                    &format_user_label(unbanned_user),
                    avatar.as_deref(),
                ))
                .title("Member Unbanned")
                .color(Colour::DARK_GREEN)
                .field("Member", format!("<@{}>", unbanned_user.id.get()), true)
                .field("User ID", format!("`{}`", unbanned_user.id.get()), true)
                .field(
                    "Moderator",
                    audit.as_ref()
                        .map(|a| a.moderator.clone())
                        .unwrap_or_else(|| "Unknown".to_string()),
                    false,
                )
                .field(
                    "Reason",
                    audit.and_then(|a| a.reason)
                        .unwrap_or_else(|| "*No reason provided*".to_string()),
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::GuildEmojisUpdate {
            guild_id,
            current_state,
        } => {
            let emoji_count = current_state.len();

            let emoji_preview = current_state
                .values()
                .take(20)
                .map(|emoji| format!(":{}: (`{}`)", emoji.name, emoji.id.get()))
                .collect::<Vec<_>>()
                .join(", ");

            let embed = CreateEmbed::new()
                .title("Guild Emojis Updated")
                .color(Colour::ORANGE)
                .field("Guild ID", format!("`{}`", guild_id.get()), true)
                .field("Emoji Count", format!("`{}`", emoji_count), true)
                .field(
                    "Current Emojis",
                    if emoji_preview.is_empty() {
                        "*No emojis present*".to_string()
                    } else {
                        truncate_for_embed(&emoji_preview, 1000)
                    },
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::AutoModRuleCreate { rule } => {
            let embed = CreateEmbed::new()
                .title("AutoMod Rule Created")
                .color(Colour::DARK_GREEN)
                .field("Rule Name", rule.name.clone(), true)
                .field("Rule ID", format!("`{}`", rule.id.get()), true)
                .field("Guild ID", format!("`{}`", rule.guild_id.get()), true)
                .field(
                    "Creator",
                    format!("<@{}> (`{}`)", rule.creator_id.get(), rule.creator_id.get()),
                    false,
                )
                .field("Event Type", format_automod_event_type(rule.event_type), true)
                .field(
                    "Trigger",
                    truncate_for_embed(&format_automod_trigger(rule), 1000),
                    false,
                )
                .field("Enabled", if rule.enabled { "Yes" } else { "No" }, true)
                .field("Actions", format_automod_action_count(&rule.actions), true)
                .field("Exempt Roles", format_role_list(&rule.exempt_roles), false)
                .field(
                    "Exempt Channels",
                    if rule.exempt_channels.is_empty() {
                        "*None*".to_string()
                    } else {
                        rule.exempt_channels
                            .iter()
                            .map(|id| format!("<#{}>", id.get()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::AutoModRuleUpdate { rule } => {
            let embed = CreateEmbed::new()
                .title("AutoMod Rule Updated")
                .color(Colour::ORANGE)
                .field("Rule Name", rule.name.clone(), true)
                .field("Rule ID", format!("`{}`", rule.id.get()), true)
                .field("Guild ID", format!("`{}`", rule.guild_id.get()), true)
                .field(
                    "Creator",
                    format!("<@{}> (`{}`)", rule.creator_id.get(), rule.creator_id.get()),
                    false,
                )
                .field("Event Type", format_automod_event_type(rule.event_type), true)
                .field(
                    "Trigger",
                    truncate_for_embed(&format_automod_trigger(rule), 1000),
                    false,
                )
                .field("Enabled", if rule.enabled { "Yes" } else { "No" }, true)
                .field("Actions", format_automod_action_count(&rule.actions), true)
                .field("Exempt Roles", format_role_list(&rule.exempt_roles), false)
                .field(
                    "Exempt Channels",
                    if rule.exempt_channels.is_empty() {
                        "*None*".to_string()
                    } else {
                        rule.exempt_channels
                            .iter()
                            .map(|id| format!("<#{}>", id.get()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        serenity::FullEvent::AutoModRuleDelete { rule } => {
            let embed = CreateEmbed::new()
                .title("AutoMod Rule Deleted")
                .color(Colour::DARK_RED)
                .field("Rule Name", rule.name.clone(), true)
                .field("Rule ID", format!("`{}`", rule.id.get()), true)
                .field("Guild ID", format!("`{}`", rule.guild_id.get()), true)
                .field(
                    "Creator",
                    format!("<@{}> (`{}`)", rule.creator_id.get(), rule.creator_id.get()),
                    false,
                )
                .field("Event Type", format_automod_event_type(rule.event_type), true)
                .field(
                    "Trigger",
                    truncate_for_embed(&format_automod_trigger(rule), 1000),
                    false,
                )
                .field("Enabled", if rule.enabled { "Yes" } else { "No" }, true)
                .field("Actions", format_automod_action_count(&rule.actions), true)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, embed).await;
        }

        _ => {}
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    match dotenvy::dotenv_override() {
        Ok(path) => println!("Loaded .env from: {}", path.display()),
        Err(err) => println!("Could not load .env: {}", err),
    }

    let token = std::env::var("DISCORD_TOKEN")
        .expect("DISCORD_TOKEN must be set in your environment or .env file");

    let intents = serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_MEMBERS
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::MESSAGE_CONTENT
        | serenity::GatewayIntents::GUILD_MODERATION
        | serenity::GatewayIntents::GUILD_VOICE_STATES
        | serenity::GatewayIntents::AUTO_MODERATION_CONFIGURATION;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![age_verified(), ticket_panel_post()],
            event_handler: |ctx, event, framework, data| {
                Box::pin(event_handler(ctx, event, framework, data))
            },
            ..Default::default()
        })
        .setup(|ctx, ready, framework| {
            Box::pin(async move {
                println!("Bot is online as {}", ready.user.name);

                initialize_storage().await?;
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;

                Ok(Data {
                    message_cache: Arc::new(RwLock::new(HashMap::new())),
                    message_order: Arc::new(RwLock::new(VecDeque::new())),
                })
            })
        })
        .build();

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await?;

    client.start().await?;
    Ok(())
}
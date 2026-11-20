mod archive_safety;
mod bump;
mod config;
mod deletion_attribution;
mod flash;
mod roles;
mod safeword;
mod storage;
mod text_helpers;
mod tickets;

use config::Config;
use deletion_attribution::{
    attribute_deletion, AuditEntryLite, AuditLookupFailed, DeletedBy, LastSeenCounts,
    LOOKUP_DELAYS_MS,
};
use poise::serenity_prelude as serenity;
use poise::serenity_prelude::Mentionable;
use rusqlite::params;
#[cfg(test)]
use serenity::all::{ChannelId, PermissionOverwrite, PermissionOverwriteType, Permissions};
use serenity::builder::{
    CreateAttachment, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateMessage,
};
use serenity::model::channel::{ChannelType, ReactionType};
use serenity::model::colour::Colour;
use serenity::model::guild::audit_log::{Action, MemberAction, MessageAction};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use text_helpers::{
    code_block_or_placeholder, normalize_reaction_type, parse_emoji_input, truncate_for_embed,
};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_rusqlite::Connection;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

const AUDIT_MATCH_WINDOW_MS: u64 = 8000;
const CACHE_LIMIT: usize = 10000;

struct Data {
    config: Arc<Config>,
    message_cache: Arc<RwLock<HashMap<serenity::MessageId, CachedMessage>>>,
    rr_db: Arc<Connection>,
    tickets: Option<tickets::repository::Repository>,
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

// Retained baseline regression for Serenity's cache permission calculation.
#[cfg(test)]
macro_rules! cached_channel_permissions {
    ($cache:expr, $channel:expr, $user_id:expr) => {{
        let cache: &serenity::Cache = $cache;
        let channel: &serenity::GuildChannel = $channel;
        let user_id: serenity::UserId = $user_id;
        cache.guild(channel.guild_id).and_then(|guild| {
            let member = guild.members.get(&user_id)?;
            Some(guild.user_permissions_in(channel, member))
        })
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ticket_commands_are_registered_only_with_valid_enabled_config() {
        assert!(!configured_commands(false)
            .iter()
            .any(|c| c.name == "ticket-panel-post"));
        assert!(configured_commands(true)
            .iter()
            .any(|c| c.name == "ticket-panel-post"));
    }
    #[test]
    fn archive_command_is_gated_and_has_slash_metadata() {
        const NAME: &str = "ticket-archives";
        assert!(!configured_commands(false).iter().any(|c| c.name == NAME));
        let enabled = configured_commands(true);
        let command = enabled.iter().find(|c| c.name == NAME).expect("registered");
        assert!(command.guild_only && command.slash_action.is_some());
        let params: Vec<_> = command
            .parameters
            .iter()
            .map(|p| (p.name.as_str(), p.required))
            .collect();
        assert_eq!(params, [("action", true), ("id", false), ("offset", false)]);
    }

    #[test]
    fn close_and_recovery_commands_have_real_metadata_and_are_gated() {
        for name in ["ticket-close", "ticket-recover"] {
            assert!(!configured_commands(false).iter().any(|c| c.name == name));
            let on = configured_commands(true);
            let c = on.iter().find(|c| c.name == name).expect("registered");
            assert!(c.guild_only && c.slash_action.is_some());
        }
        let on = configured_commands(true);
        let rec = on.iter().find(|c| c.name == "ticket-recover").unwrap();
        let names: Vec<_> = rec.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["ticket_id", "action", "channel", "confirm", "reason"]
        );
        const {
            assert!(
                !tickets::runtime::OPENING_APPROVED,
                "opening stays gated in this change"
            )
        };
    }

    #[test]
    fn ticket_cached_permissions_preserve_visibility_and_cache_miss_policy() {
        let guild_id = serenity::GuildId::new(100);
        let user_id = serenity::UserId::new(200);
        // Exercise Serenity's real cache, role calculation, and channel overwrites offline.
        for case in 0..9 {
            let cache = serenity::Cache::default();
            let mut guild = serenity::Guild::default();
            guild.id = guild_id;
            guild.owner_id = serenity::UserId::new(if case == 6 { 200 } else { 999 });
            let mut member = serenity::Member::default();
            member.guild_id = guild_id;
            member.user.id = user_id;
            let mut everyone = serenity::Role::default();
            everyone.id = serenity::RoleId::new(guild_id.get());
            everyone.guild_id = guild_id;
            everyone.permissions = if case == 4 {
                Permissions::empty()
            } else {
                Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES
            };
            guild.roles.insert(everyone.id, everyone);
            let mut channel = serenity::GuildChannel::default();
            channel.id = ChannelId::new(300);
            channel.guild_id = guild_id;
            if matches!(case, 3 | 5 | 6) {
                channel.permission_overwrites.push(PermissionOverwrite {
                    allow: Permissions::empty(),
                    deny: Permissions::VIEW_CHANNEL,
                    kind: PermissionOverwriteType::Member(user_id),
                });
            }
            if case == 4 {
                channel.permission_overwrites.push(PermissionOverwrite {
                    allow: Permissions::VIEW_CHANNEL,
                    deny: Permissions::empty(),
                    kind: PermissionOverwriteType::Member(user_id),
                });
            }
            if matches!(case, 5 | 8) {
                let mut role = serenity::Role::default();
                role.id = serenity::RoleId::new(400);
                role.guild_id = guild_id;
                role.permissions = if case == 5 {
                    Permissions::ADMINISTRATOR
                } else {
                    Permissions::VIEW_CHANNEL
                };
                member.roles.push(role.id);
                guild.roles.insert(role.id, role);
                if case == 8 {
                    channel.permission_overwrites.push(PermissionOverwrite {
                        allow: Permissions::empty(),
                        deny: Permissions::VIEW_CHANNEL,
                        kind: PermissionOverwriteType::Role(serenity::RoleId::new(400)),
                    });
                }
            }
            if case != 1 {
                guild.members.insert(user_id, member);
            }
            if case != 0 {
                let mut event: serenity::GuildCreateEvent =
                    serde_json::from_value(serde_json::to_value(&guild).unwrap()).unwrap();
                cache.update(&mut event);
            }
            if case == 7 {
                // Lookup must use the channel's guild, not an interaction's guild.
                channel.guild_id = serenity::GuildId::new(101);
            }
            let permissions = cached_channel_permissions!(&cache, &channel, user_id);
            assert_eq!(
                permissions.is_none(),
                matches!(case, 0 | 1 | 7),
                "case {case}"
            );
            assert_eq!(
                permissions.map(|p| p.view_channel()).unwrap_or(false),
                matches!(case, 2 | 4 | 5 | 6),
                "case {case}"
            );
            if case == 2 {
                assert!(permissions.unwrap().send_messages());
            }
        }
    }

    fn age_member(roles: &[u64], permissions: Permissions) -> serenity::Member {
        let mut member = serenity::Member::default();
        member.guild_id = serenity::GuildId::new(100);
        member.roles = roles.iter().map(|id| serenity::RoleId::new(*id)).collect();
        member.permissions = Some(permissions);
        member
    }

    fn age_request<'a>(
        caller: &'a serenity::Member,
        target: &'a serenity::Member,
    ) -> AgeRoleRequest<'a> {
        AgeRoleRequest {
            guild_id: Some(serenity::GuildId::new(100)),
            caller: Some(caller),
            target,
            staff_role_id: Some(200),
            grant_roles: [300, 400],
            guild_roles: {
                const ROLES: &[serenity::RoleId] = &[
                    serenity::RoleId::new(200),
                    serenity::RoleId::new(300),
                    serenity::RoleId::new(400),
                ];
                ROLES
            },
        }
    }

    #[tokio::test]
    async fn age_nonstaff_with_manage_roles_is_denied_before_writes() {
        for permissions in [
            Permissions::empty(),
            Permissions::MANAGE_ROLES,
            Permissions::ADMINISTRATOR,
        ] {
            let caller = age_member(&[999], permissions);
            let target = age_member(&[777], Permissions::empty());
            let mut writes = Vec::new();
            let message = execute_age_roles(age_request(&caller, &target), |role| {
                writes.push(role);
                std::future::ready(Ok::<_, ()>(()))
            })
            .await;
            assert_eq!(
                message,
                "Only members of the configured staff role can use this command."
            );
            assert!(writes.is_empty());
        }
    }

    #[tokio::test]
    async fn age_invalid_context_or_configuration_never_writes() {
        let staff = age_member(&[200, 100, 999], Permissions::empty());
        let target = age_member(&[777], Permissions::empty());
        let mut foreign = staff.clone();
        foreign.guild_id = serenity::GuildId::new(101);
        for case in 0..13 {
            let mut request = age_request(&staff, &target);
            match case {
                0 => request.guild_id = None,
                1 => request.caller = None,
                2 => request.caller = Some(&foreign),
                3 => request.target = &foreign,
                4 => request.staff_role_id = None,
                5 => request.staff_role_id = Some(0),
                6 => request.staff_role_id = Some(100), // @everyone
                7 => request.staff_role_id = Some(999), // not a guild role
                8 => request.grant_roles[0] = 0,
                9 => request.grant_roles[1] = 0,
                10 => request.grant_roles[0] = 100,
                11 => request.grant_roles[1] = 200, // must not grant staff
                12 => request.grant_roles[1] = 999,
                _ => unreachable!(),
            }
            let mut writes = Vec::new();
            let message = execute_age_roles(request, |role| {
                writes.push(role);
                std::future::ready(Ok::<_, ()>(()))
            })
            .await;
            assert!(writes.is_empty(), "case {case}: {message}");
            assert!(!message.contains("now granted"));
        }
    }

    #[tokio::test]
    async fn age_staff_grants_are_ordered_additive_and_repeatable() {
        let staff = age_member(&[200], Permissions::empty());
        let target = age_member(&[777], Permissions::empty());
        let mut held = std::collections::HashSet::from([777]);
        // Existing roles are retained; explicit repeats restore either removed role.
        for removed in [None, None, Some(300), Some(400)] {
            if let Some(role) = removed {
                held.remove(&role);
            }
            let mut writes = Vec::new();
            let message = execute_age_roles(age_request(&staff, &target), |role| {
                writes.push(role.get());
                held.insert(role.get());
                std::future::ready(Ok::<_, ()>(()))
            })
            .await;
            assert_eq!(writes, [300, 400]);
            assert_eq!(held, std::collections::HashSet::from([777, 300, 400]));
            assert_eq!(message, "Both configured age roles are now granted.");
        }
    }

    #[tokio::test]
    async fn age_failed_grants_report_progress_and_never_rollback() {
        let staff = age_member(&[200], Permissions::empty());
        let target = age_member(&[777], Permissions::empty());
        for fail_at in [1, 2] {
            for preexisting in [vec![777], vec![777, 300, 400]] {
                let mut held: std::collections::HashSet<_> = preexisting.iter().copied().collect();
                let mut writes = Vec::new();
                let message = execute_age_roles(age_request(&staff, &target), |role| {
                    writes.push(role.get());
                    let result = if writes.len() == fail_at {
                        Err("synthetic private error")
                    } else {
                        held.insert(role.get());
                        Ok(())
                    };
                    std::future::ready(result)
                })
                .await;
                assert_eq!(writes.len(), fail_at);
                assert!(preexisting.iter().all(|role| held.contains(role)));
                assert_eq!(
                    message,
                    if fail_at == 1 {
                        "AGE_ROLE_1 request failed; AGE_ROLE_2 was not attempted. Existing roles were not removed. Discord may have applied the failed request; check the member before retrying."
                    } else {
                        "AGE_ROLE_1 request succeeded; AGE_ROLE_2 request failed. No roles were removed. Discord may have applied the failed request; check the member before retrying."
                    }
                );
                if fail_at == 2 {
                    assert!(held.contains(&300));
                }
                assert!(!message.contains("Both configured"));
                assert!(!message.contains("synthetic private error"));
            }
        }
    }

    #[test]
    fn age_command_metadata_allows_staff_without_manage_roles() {
        let command = age_verified();
        assert_eq!(command.name, "age-verified");
        assert!(command.guild_only);
        assert!(command.required_permissions.is_empty());
        assert!(command.default_member_permissions.is_empty());
        assert_eq!(command.required_bot_permissions, Permissions::MANAGE_ROLES);
        assert_eq!(command.parameters.len(), 1);
        assert_eq!(command.parameters[0].name, "user");
        assert!(command.slash_action.is_some());
        let registration =
            serde_json::to_value(command.create_as_slash_command().unwrap()).unwrap();
        // Discord treats a literal permissions "0" differently from no gate:
        // require the generated registration to omit/null the caller restriction.
        assert!(registration["default_member_permissions"].is_null());
        assert_eq!(registration["options"][0]["required"], true);
        assert_eq!(registration["options"][0]["type"], 6); // Discord USER option
    }

    // ---------------------------------------------------------------------
    // SF4: DM events must never reach the guild log channel.
    //
    // The logger's production routing seam is `guarded_log`: it runs the log-write sink
    // only for events that are provably guild events. These tests drive that real seam
    // with synthetic events whose guild id is absent (DM / group DM) and with the same
    // events carrying a guild id, and observe whether the sink ran.
    // ---------------------------------------------------------------------

    /// Synthetic guild id used by the logger scope tests.
    const SYNTHETIC_GUILD: u64 = 900_001;
    /// Synthetic DM channel id used by the logger scope tests.
    const SYNTHETIC_DM_CHANNEL: u64 = 900_002;
    /// Synthetic guild channel id used by the logger scope tests.
    const SYNTHETIC_GUILD_CHANNEL: u64 = 900_003;

    /// A synthetic message-create event in the given channel, with the given guild id.
    fn synthetic_message(ch: u64, guild: Option<u64>) -> serenity::FullEvent {
        let mut m = serenity::Message::default();
        m.id = serenity::MessageId::new(900_010);
        m.channel_id = serenity::ChannelId::new(ch);
        m.guild_id = guild.map(serenity::GuildId::new);
        m.content = "SYNTHETIC private DM text".to_string();
        serenity::FullEvent::Message { new_message: m }
    }

    /// A synthetic message-update event in the given channel, with the given guild id.
    fn synthetic_update(ch: u64, guild: Option<u64>) -> serenity::FullEvent {
        let mut payload = serde_json::json!({
            "id": "900011",
            "channel_id": ch.to_string(),
            "content": "SYNTHETIC private DM text",
        });
        if let Some(g) = guild {
            payload["guild_id"] = serde_json::json!(g.to_string());
        }
        serenity::FullEvent::MessageUpdate {
            old_if_available: None,
            new: None,
            event: serde_json::from_value(payload).unwrap(),
        }
    }

    /// A synthetic message-delete event in the given channel, with the given guild id.
    fn synthetic_delete(ch: u64, guild: Option<u64>) -> serenity::FullEvent {
        serenity::FullEvent::MessageDelete {
            channel_id: serenity::ChannelId::new(ch),
            deleted_message_id: serenity::MessageId::new(900_012),
            guild_id: guild.map(serenity::GuildId::new),
        }
    }

    /// A synthetic bulk-delete event in the given channel, with the given guild id.
    fn synthetic_bulk_delete(ch: u64, guild: Option<u64>) -> serenity::FullEvent {
        serenity::FullEvent::MessageDeleteBulk {
            channel_id: serenity::ChannelId::new(ch),
            multiple_deleted_messages_ids: vec![serenity::MessageId::new(900_013)],
            guild_id: guild.map(serenity::GuildId::new),
        }
    }

    /// A synthetic voice-state event with the given guild id.
    fn synthetic_voice(guild: Option<u64>) -> serenity::FullEvent {
        let mut payload = serde_json::json!({
            "user_id": "900014",
            "session_id": "synthetic",
            "deaf": false,
            "mute": false,
            "self_deaf": false,
            "self_mute": false,
            "self_video": false,
            "suppress": false,
        });
        if let Some(g) = guild {
            payload["guild_id"] = serde_json::json!(g.to_string());
        }
        serenity::FullEvent::VoiceStateUpdate {
            old: None,
            new: serde_json::from_value(payload).unwrap(),
        }
    }

    /// A synthetic invite-create event for the given channel, with the given guild id.
    fn synthetic_invite_create(ch: u64, guild: Option<u64>) -> serenity::FullEvent {
        let mut payload = serde_json::json!({
            "channel_id": ch.to_string(),
            "code": "SYNTHETIC",
            "created_at": "2026-01-01T00:00:00Z",
            "max_age": 0,
            "max_uses": 0,
            "temporary": false,
            "uses": 0,
        });
        if let Some(g) = guild {
            payload["guild_id"] = serde_json::json!(g.to_string());
        }
        serenity::FullEvent::InviteCreate {
            data: serde_json::from_value(payload).unwrap(),
        }
    }

    /// A synthetic invite-delete event for the given channel, with the given guild id.
    fn synthetic_invite_delete(ch: u64, guild: Option<u64>) -> serenity::FullEvent {
        let mut payload = serde_json::json!({
            "channel_id": ch.to_string(),
            "code": "SYNTHETIC",
        });
        if let Some(g) = guild {
            payload["guild_id"] = serde_json::json!(g.to_string());
        }
        serenity::FullEvent::InviteDelete {
            data: serde_json::from_value(payload).unwrap(),
        }
    }

    /// A synthetic reaction-add event for the given channel, with the given guild id.
    fn synthetic_reaction(ch: u64, guild: Option<u64>) -> serenity::FullEvent {
        let mut payload = serde_json::json!({
            "channel_id": ch.to_string(),
            "message_id": "900015",
            "user_id": "900016",
            "emoji": {"name": "x"},
            "burst": false,
            "burst_colours": [],
            "type": 0,
        });
        if let Some(g) = guild {
            payload["guild_id"] = serde_json::json!(g.to_string());
        }
        serenity::FullEvent::ReactionAdd {
            add_reaction: serde_json::from_value(payload).unwrap(),
        }
    }

    /// Every logger path that can see an event with no guild id (DM / group DM).
    fn synthetic_dm_events() -> Vec<serenity::FullEvent> {
        vec![
            synthetic_message(SYNTHETIC_DM_CHANNEL, None),
            synthetic_update(SYNTHETIC_DM_CHANNEL, None),
            synthetic_delete(SYNTHETIC_DM_CHANNEL, None),
            synthetic_bulk_delete(SYNTHETIC_DM_CHANNEL, None),
            synthetic_voice(None),
            synthetic_invite_create(SYNTHETIC_DM_CHANNEL, None),
            synthetic_invite_delete(SYNTHETIC_DM_CHANNEL, None),
            synthetic_reaction(SYNTHETIC_DM_CHANNEL, None),
        ]
    }

    /// The same logger paths carrying a guild id: these are ordinary guild events.
    fn synthetic_guild_events() -> Vec<serenity::FullEvent> {
        vec![
            synthetic_message(SYNTHETIC_GUILD_CHANNEL, Some(SYNTHETIC_GUILD)),
            synthetic_update(SYNTHETIC_GUILD_CHANNEL, Some(SYNTHETIC_GUILD)),
            synthetic_delete(SYNTHETIC_GUILD_CHANNEL, Some(SYNTHETIC_GUILD)),
            synthetic_bulk_delete(SYNTHETIC_GUILD_CHANNEL, Some(SYNTHETIC_GUILD)),
            synthetic_voice(Some(SYNTHETIC_GUILD)),
            synthetic_invite_create(SYNTHETIC_GUILD_CHANNEL, Some(SYNTHETIC_GUILD)),
            synthetic_invite_delete(SYNTHETIC_GUILD_CHANNEL, Some(SYNTHETIC_GUILD)),
            synthetic_reaction(SYNTHETIC_GUILD_CHANNEL, Some(SYNTHETIC_GUILD)),
        ]
    }

    /// Drives the REAL production routing seam and reports how often the log-write sink ran.
    async fn sink_runs(event: &serenity::FullEvent) -> (usize, bool) {
        let runs = std::sync::atomic::AtomicUsize::new(0);
        let out = guarded_log(event, || async {
            runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
        .await;
        (
            runs.load(std::sync::atomic::Ordering::SeqCst),
            out.is_some(),
        )
    }

    /// Proves an event with no guild id (DM / group DM) never reaches the guild log channel:
    /// the production seam performs zero log writes for every logger path that can see one.
    /// Fails if any DM path starts writing to the log channel again.
    #[tokio::test]
    async fn dm_events_never_reach_the_guild_log_channel() {
        for ev in synthetic_dm_events() {
            let (runs, ran) = sink_runs(&ev).await;
            assert_eq!(runs, 0, "DM event must not write to the log channel");
            assert!(!ran, "DM event must not run the log sink");
        }
    }

    /// Proves the fix does not silence ordinary guild logging: the same paths carrying a
    /// guild id still run the log-write sink exactly once. Fails if the guard is too broad.
    #[tokio::test]
    async fn guild_events_still_reach_the_guild_log_channel() {
        for ev in synthetic_guild_events() {
            let (runs, ran) = sink_runs(&ev).await;
            assert_eq!(runs, 1, "guild event must still write to the log channel");
            assert!(ran, "guild event must still run the log sink");
        }
    }

    /// Proves the content-bearing DM path (message edit) is skipped, so the DM's before and
    /// after text can never be mirrored into the guild log channel. Fails if the update arm
    /// starts handling DM events again.
    #[tokio::test]
    async fn dm_message_update_never_mirrors_content_into_the_log_channel() {
        let dm_update = synthetic_update(SYNTHETIC_DM_CHANNEL, None);
        let (runs, _) = sink_runs(&dm_update).await;
        assert_eq!(runs, 0);

        // The same edit in a guild is still logged.
        let guild_update = synthetic_update(SYNTHETIC_GUILD_CHANNEL, Some(SYNTHETIC_GUILD));
        let (runs, _) = sink_runs(&guild_update).await;
        assert_eq!(runs, 1);
    }

    /// Proves the DM message-delete path is skipped, so cached DM content can never be
    /// mirrored into the guild log channel. Fails if the delete arm handles DMs again.
    #[tokio::test]
    async fn dm_message_delete_never_mirrors_cached_content_into_the_log_channel() {
        let (runs, _) = sink_runs(&synthetic_delete(SYNTHETIC_DM_CHANNEL, None)).await;
        assert_eq!(runs, 0);
        let (runs, _) = sink_runs(&synthetic_bulk_delete(SYNTHETIC_DM_CHANNEL, None)).await;
        assert_eq!(runs, 0);

        let (runs, _) = sink_runs(&synthetic_delete(
            SYNTHETIC_GUILD_CHANNEL,
            Some(SYNTHETIC_GUILD),
        ))
        .await;
        assert_eq!(runs, 1);
        let (runs, _) = sink_runs(&synthetic_bulk_delete(
            SYNTHETIC_GUILD_CHANNEL,
            Some(SYNTHETIC_GUILD),
        ))
        .await;
        assert_eq!(runs, 1);
    }

    /// Proves a DM message-create event is not cached for later mirroring: the production
    /// seam skips it, so no DM content is retained for a later log write. Fails if the
    /// create path starts handling DM events again.
    #[tokio::test]
    async fn dm_message_create_is_not_cached_for_later_mirroring() {
        let (runs, _) = sink_runs(&synthetic_message(SYNTHETIC_DM_CHANNEL, None)).await;
        assert_eq!(runs, 0);
        let (runs, _) = sink_runs(&synthetic_message(
            SYNTHETIC_GUILD_CHANNEL,
            Some(SYNTHETIC_GUILD),
        ))
        .await;
        assert_eq!(runs, 1);
    }
}

fn build_log_footer() -> CreateEmbedFooter {
    CreateEmbedFooter::new("SKYNET // NETWORK CORE • Logger")
}

/// Guild scope of an event, for the guild log channel.
///
/// The guild log channel is a shared staff channel. Only an event that is provably a guild
/// event may be written there: an event with no guild id (a DM or group DM) is private and
/// must never appear in a staff channel, not even without its message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuildScope {
    /// The payload proves a guild id: the event may be written to the guild log channel.
    Guild,
    /// The payload carries no guild id (a DM or group DM): the event must never be written
    /// to the guild log channel, and its content must never appear there.
    NotGuild,
}

/// Classifies an event for the guild log channel.
///
/// Fails closed: an event is `Guild` only when its payload proves a guild id. A DM or group
/// DM carries none and is `NotGuild`. Interaction events are not channel-logging paths (the
/// ticket runtime handles them and they never write to the log channel), so they stay
/// `Guild` and are not affected by this guard.
fn guild_scope(event: &serenity::FullEvent) -> GuildScope {
    let guild_id = match event {
        serenity::FullEvent::Message { new_message } => new_message.guild_id,
        serenity::FullEvent::MessageUpdate { event, new, .. } => event
            .guild_id
            .or_else(|| new.as_ref().and_then(|m| m.guild_id)),
        serenity::FullEvent::MessageDelete { guild_id, .. } => *guild_id,
        serenity::FullEvent::MessageDeleteBulk { guild_id, .. } => *guild_id,
        serenity::FullEvent::VoiceStateUpdate { new, .. } => new.guild_id,
        serenity::FullEvent::InviteCreate { data } => data.guild_id,
        serenity::FullEvent::InviteDelete { data } => data.guild_id,
        serenity::FullEvent::ReactionAdd { add_reaction } => add_reaction.guild_id,
        serenity::FullEvent::ReactionRemove { removed_reaction } => removed_reaction.guild_id,
        // Everything else (roles, members, channels, threads, interactions) is a guild-only
        // event or is not a channel-logging path at all.
        _ => return GuildScope::Guild,
    };
    match guild_id {
        Some(_) => GuildScope::Guild,
        None => GuildScope::NotGuild,
    }
}

/// Production routing seam for the general logger.
///
/// Runs the sink only for events that are provably guild events. An event with no guild id
/// (a DM or group DM) is skipped, so neither its metadata nor its content can reach the
/// guild log channel. The sink runs at most once.
async fn guarded_log<F, Fut, T>(event: &serenity::FullEvent, sink: F) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    if guild_scope(event) == GuildScope::NotGuild {
        return None;
    }
    Some(sink().await)
}

async fn log_to_channel_with_files(
    ctx: &serenity::Context,
    log_channel_id: u64,
    embed: CreateEmbed,
    files: Vec<CreateAttachment>,
) -> Result<(), Box<serenity::Error>> {
    let channel_id = serenity::ChannelId::new(log_channel_id);

    let mut message = CreateMessage::new().embed(embed);
    for file in files {
        message = message.add_file(file);
    }

    channel_id.send_message(&ctx.http, message).await?;
    Ok(())
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

fn format_automod_action_count(actions: &[serenity::model::guild::automod::Action]) -> String {
    if actions.is_empty() {
        "*None*".to_string()
    } else {
        actions.len().to_string()
    }
}

fn format_optional_text(value: Option<String>) -> String {
    value.unwrap_or_else(|| "*None*".to_string())
}

fn format_user_id_codeblock(executor_id: u64, target_id: u64) -> String {
    format!(
        "```text\nExecutor = {}\nTarget   = {}\n```",
        executor_id, target_id
    )
}

fn json_bytes_file(name: &str, value: &str) -> CreateAttachment {
    CreateAttachment::bytes(value.as_bytes().to_vec(), name.to_string())
}

async fn insert_cache(data: &Data, message_id: serenity::MessageId, cached: CachedMessage) {
    let mut cache = data.message_cache.write().await;

    if cache.len() >= CACHE_LIMIT {
        if let Some(first_key) = cache.keys().next().cloned() {
            cache.remove(&first_key);
        }
    }

    cache.insert(message_id, cached);
}

async fn log_to_channel(
    ctx: &serenity::Context,
    log_channel_id: u64,
    embed: CreateEmbed,
) -> Result<(), Box<serenity::Error>> {
    let channel_id = serenity::ChannelId::new(log_channel_id);

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
) -> Result<Option<AuditActorInfo>, Box<serenity::Error>> {
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

        if let Ok(Some(info)) = find_recent_audit_actor(ctx, guild_id, target_user_id, action).await
        {
            return Some(info);
        }
    }

    None
}

static DELETE_AUDIT_SEEN: std::sync::OnceLock<std::sync::Mutex<LastSeenCounts>> =
    std::sync::OnceLock::new();

/// Thin Serenity shell around `deletion_attribution::attribute_deletion`.
async fn lookup_deleted_by(
    ctx: &serenity::Context,
    guild_id: Option<serenity::GuildId>,
    author_id: u64,
    author_is_bot: bool,
    channel_id: u64,
) -> DeletedBy {
    let Some(guild_id) = guild_id else {
        return DeletedBy::AuditUnavailable;
    };

    let mut result = DeletedBy::AuditUnavailable;
    for delay_ms in LOOKUP_DELAYS_MS {
        if delay_ms > 0 {
            sleep(Duration::from_millis(delay_ms)).await;
        }

        let fetched = guild_id
            .audit_logs(
                &ctx.http,
                Some(Action::Message(MessageAction::Delete)),
                None,
                None,
                Some(50),
            )
            .await
            .map(|logs| {
                logs.entries
                    .iter()
                    .map(|entry| AuditEntryLite {
                        id: entry.id.get(),
                        executor_id: entry.user_id.get(),
                        target_id: entry.target_id.map(|id| id.get()),
                        channel_id: entry
                            .options
                            .as_ref()
                            .and_then(|o| o.channel_id)
                            .map(|id| id.get()),
                        count: entry.options.as_ref().and_then(|o| o.count).unwrap_or(1),
                        created_ms: entry.id.created_at().unix_timestamp() as u64 * 1000,
                    })
                    .collect::<Vec<_>>()
            })
            .map_err(|_| AuditLookupFailed);

        result = {
            let mut seen = DELETE_AUDIT_SEEN
                .get_or_init(|| std::sync::Mutex::new(LastSeenCounts::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            attribute_deletion(
                author_id,
                author_is_bot,
                channel_id,
                unix_now_ms(),
                fetched.as_deref().map_err(|e| *e),
                &mut seen,
            )
        };

        if matches!(result, DeletedBy::Executor(_)) {
            break;
        }
    }

    result
}

async fn find_recent_member_role_update_actor(
    ctx: &serenity::Context,
    guild_id: serenity::GuildId,
    target_user_id: serenity::UserId,
) -> Option<AuditActorInfo> {
    for delay_ms in [0_u64, 1000, 2000, 4000] {
        if delay_ms > 0 {
            sleep(Duration::from_millis(delay_ms)).await;
        }

        let result = guild_id
            .audit_logs(
                &ctx.http,
                Some(Action::Member(MemberAction::Update)),
                None,
                None,
                Some(10),
            )
            .await;

        let Ok(audit_logs) = result else {
            continue;
        };

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
                return Some(AuditActorInfo {
                    moderator: format!("<@{}>", entry.user_id.get()),
                    reason: entry.reason.clone(),
                });
            }
        }
    }

    None
}

async fn init_reaction_role_db(path: &str) -> Result<Arc<Connection>, Error> {
    let conn = Connection::open(path).await?;

    conn.call(|conn| -> Result<(), rusqlite::Error> {
        conn.execute_batch(
            r#"
CREATE TABLE IF NOT EXISTS reaction_roles (
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    emoji_key TEXT NOT NULL,
    role_id TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (message_id, emoji_key, role_id)
);
"#,
        )?;
        Ok(())
    })
    .await?;

    Ok(Arc::new(conn))
}

async fn rr_add_binding(
    db: Arc<Connection>,
    guild_id: u64,
    channel_id: u64,
    message_id: u64,
    emoji_key: &str,
    role_id: u64,
    created_by: u64,
) -> Result<(), Error> {
    let emoji_key = emoji_key.to_string();

    db.call(move |conn| -> Result<(), rusqlite::Error> {
        conn.execute(
            "INSERT OR REPLACE INTO reaction_roles
            (guild_id, channel_id, message_id, emoji_key, role_id, created_by, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'))",
            params![
                guild_id.to_string(),
                channel_id.to_string(),
                message_id.to_string(),
                emoji_key,
                role_id.to_string(),
                created_by.to_string(),
            ],
        )?;
        Ok(())
    })
    .await?;

    Ok(())
}

async fn rr_remove_binding(
    db: Arc<Connection>,
    message_id: u64,
    role_id: u64,
) -> Result<usize, Error> {
    let affected = db
        .call(move |conn| -> Result<usize, rusqlite::Error> {
            let rows = conn.execute(
                "DELETE FROM reaction_roles WHERE message_id = ?1 AND role_id = ?2",
                params![message_id.to_string(), role_id.to_string()],
            )?;
            Ok(rows)
        })
        .await?;

    Ok(affected)
}

type ReactionRoleRow = (String, String, String, String, String);

async fn rr_list_bindings(
    db: Arc<Connection>,
    message_id: u64,
) -> Result<Vec<ReactionRoleRow>, Error> {
    let rows = db
        .call(
            move |conn| -> Result<Vec<ReactionRoleRow>, rusqlite::Error> {
                let mut stmt = conn.prepare(
                    "SELECT guild_id, channel_id, message_id, emoji_key, role_id
                     FROM reaction_roles
                     WHERE message_id = ?1
                     ORDER BY emoji_key ASC",
                )?;

                let mapped = stmt.query_map(params![message_id.to_string()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?;

                let mut out = Vec::new();
                for item in mapped {
                    out.push(item?);
                }

                Ok(out)
            },
        )
        .await?;

    Ok(rows)
}

async fn rr_lookup_roles_for_message_emoji(
    db: Arc<Connection>,
    message_id: u64,
    emoji_key: String,
) -> Result<Vec<u64>, Error> {
    let rows = db
        .call(move |conn| -> Result<Vec<u64>, rusqlite::Error> {
            let mut stmt = conn.prepare(
                "SELECT role_id
                 FROM reaction_roles
                 WHERE message_id = ?1 AND emoji_key = ?2",
            )?;

            let mapped = stmt.query_map(params![message_id.to_string(), emoji_key], |row| {
                row.get::<_, String>(0)
            })?;

            let mut out = Vec::new();
            for item in mapped {
                if let Ok(parsed) = item?.parse::<u64>() {
                    out.push(parsed);
                }
            }

            Ok(out)
        })
        .await?;

    Ok(rows)
}

// The injected operation is the single-role additive Discord endpoint, never a
// replacement of the target's complete (potentially stale) role snapshot.
struct AgeRoleRequest<'a> {
    guild_id: Option<serenity::GuildId>,
    caller: Option<&'a serenity::Member>,
    target: &'a serenity::Member,
    staff_role_id: Option<u64>,
    grant_roles: [u64; 2],
    guild_roles: &'a [serenity::RoleId],
}

async fn execute_age_roles<F, Fut, E>(request: AgeRoleRequest<'_>, mut grant: F) -> &'static str
where
    F: FnMut(serenity::RoleId) -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    let Some(guild_id) = request.guild_id else {
        return "This command can only be used in a server.";
    };
    if request.target.guild_id != guild_id
        || !request
            .caller
            .is_some_and(|caller| caller.guild_id == guild_id)
    {
        return "Could not verify caller and target membership in this server. No roles were requested.";
    }
    let valid_role = |id: u64| {
        id != 0 && id != guild_id.get() && request.guild_roles.iter().any(|role| role.get() == id)
    };
    let Some(staff) = request.staff_role_id.filter(|id| valid_role(*id)) else {
        return "This command is disabled: configure a valid STAFF_ROLE_ID for this server.";
    };
    if !request
        .caller
        .is_some_and(|caller| caller.roles.iter().any(|role| role.get() == staff))
    {
        return "Only members of the configured staff role can use this command.";
    }
    if request
        .grant_roles
        .iter()
        .any(|id| !valid_role(*id) || *id == staff)
    {
        return "This command is disabled: AGE_ROLE_1 and AGE_ROLE_2 must be valid non-staff roles in this server.";
    }
    for (index, role) in request.grant_roles.into_iter().enumerate() {
        if grant(serenity::RoleId::new(role)).await.is_err() {
            // A transport failure need not mean Discord did not apply the PUT.
            return if index == 0 {
                "AGE_ROLE_1 request failed; AGE_ROLE_2 was not attempted. Existing roles were not removed. Discord may have applied the failed request; check the member before retrying."
            } else {
                "AGE_ROLE_1 request succeeded; AGE_ROLE_2 request failed. No roles were removed. Discord may have applied the failed request; check the member before retrying."
            };
        }
    }
    "Both configured age roles are now granted."
}

#[poise::command(
    slash_command,
    guild_only,
    rename = "age-verified",
    required_bot_permissions = "MANAGE_ROLES"
)]
async fn age_verified(
    ctx: Context<'_>,
    #[description = "The user to mark as age verified"] user: serenity::Member,
) -> Result<(), Error> {
    // Defer privately before any network lookup or mutation. If acknowledging
    // fails, return before touching roles; never silently discard reply errors.
    ctx.defer_ephemeral()
        .await
        .map_err(|_| "age-verified: private acknowledgement failed; no role writes attempted")?;
    let http = &ctx.serenity_context().http;
    let caller = ctx.author_member().await;
    let config = &ctx.data().config;
    let guild_roles = match ctx.guild_id() {
        Some(guild) => match guild.roles(http).await {
            Ok(roles) => roles.into_keys().collect::<Vec<_>>(),
            Err(_) => {
                ctx.send(poise::CreateReply::default()
                    .content("Could not validate this server's roles. No role writes were attempted; try again later.")
                    .ephemeral(true))
                    .await
                    .map_err(|_| "age-verified: private validation-failure reply failed; no role writes attempted")?;
                return Ok(());
            }
        },
        None => Vec::new(),
    };
    let message = execute_age_roles(
        AgeRoleRequest {
            guild_id: ctx.guild_id(),
            caller: caller.as_deref(),
            target: &user,
            staff_role_id: config.staff_role_id,
            grant_roles: [config.age_role_1, config.age_role_2],
            guild_roles: &guild_roles,
        },
        |role| user.add_role(http, role),
    )
    .await;

    ctx.send(
        poise::CreateReply::default()
            .content(message)
            .ephemeral(true),
    )
    .await
    .map_err(|_| format!("age-verified: private result reply failed; outcome: {message}"))?;
    Ok(())
}

#[poise::command(
    slash_command,
    guild_only,
    subcommands("reactionrole_add", "reactionrole_remove", "reactionrole_list")
)]
async fn reactionrole(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

#[poise::command(
    slash_command,
    guild_only,
    rename = "add",
    required_permissions = "MANAGE_ROLES",
    default_member_permissions = "MANAGE_ROLES",
    required_bot_permissions = "MANAGE_ROLES | ADD_REACTIONS | READ_MESSAGE_HISTORY"
)]
async fn reactionrole_add(
    ctx: Context<'_>,
    #[description = "Message ID"] message_id: String,
    #[description = "Emoji, e.g. 😀 or <:name:id>"] emoji: String,
    #[description = "Role to assign"] role: serenity::Role,
    #[description = "Channel containing the target message"] channel: serenity::GuildChannel,
) -> Result<(), Error> {
    let guild_id = ctx
        .guild_id()
        .ok_or("This command can only be used in a server")?;
    let parsed_message_id = message_id.parse::<u64>()?;
    let emoji_key = parse_emoji_input(&emoji);

    let target_message = channel
        .id
        .message(
            &ctx.serenity_context().http,
            serenity::MessageId::new(parsed_message_id),
        )
        .await?;

    rr_add_binding(
        ctx.data().rr_db.clone(),
        guild_id.get(),
        channel.id.get(),
        parsed_message_id,
        &emoji_key,
        role.id.get(),
        ctx.author().id.get(),
    )
    .await?;

    let reaction_type = if emoji.starts_with("<:") || emoji.starts_with("<a:") {
        ReactionType::try_from(emoji.as_str())?
    } else {
        ReactionType::Unicode(emoji.clone())
    };

    let _ = target_message
        .react(&ctx.serenity_context().http, reaction_type)
        .await;

    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "Added reaction role: {} -> {} on message `{}` in {}.",
                emoji,
                role.mention(),
                parsed_message_id,
                channel.id.mention()
            ))
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

#[poise::command(
    slash_command,
    guild_only,
    rename = "remove",
    required_permissions = "MANAGE_ROLES",
    default_member_permissions = "MANAGE_ROLES",
    required_bot_permissions = "MANAGE_ROLES"
)]
async fn reactionrole_remove(
    ctx: Context<'_>,
    #[description = "Role to unbind"] role: serenity::Role,
    #[description = "Target message ID"] message_id: String,
) -> Result<(), Error> {
    let parsed_message_id = message_id.parse::<u64>()?;

    let removed =
        rr_remove_binding(ctx.data().rr_db.clone(), parsed_message_id, role.id.get()).await?;

    ctx.send(
        poise::CreateReply::default()
            .content(if removed == 0 {
                format!(
                    "No reaction-role binding found for {} on message `{}`.",
                    role.mention(),
                    parsed_message_id
                )
            } else {
                format!(
                    "Removed {} reaction-role binding(s) for {} on message `{}`.",
                    removed,
                    role.mention(),
                    parsed_message_id
                )
            })
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

#[poise::command(
    slash_command,
    guild_only,
    rename = "list",
    required_permissions = "MANAGE_ROLES",
    default_member_permissions = "MANAGE_ROLES"
)]
async fn reactionrole_list(
    ctx: Context<'_>,
    #[description = "Target message ID"] message_id: String,
) -> Result<(), Error> {
    let parsed_message_id = message_id.parse::<u64>()?;
    let bindings = rr_list_bindings(ctx.data().rr_db.clone(), parsed_message_id).await?;

    let description = if bindings.is_empty() {
        "No reaction-role bindings found for that message.".to_string()
    } else {
        bindings
            .into_iter()
            .map(|(_, channel_id, _, emoji_key, role_id)| {
                format!("- `{}` -> <@&{}> in <#{}>", emoji_key, role_id, channel_id)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let embed = CreateEmbed::new()
        .title("Reaction Role Bindings")
        .description(description)
        .color(Colour::BLUE);

    ctx.send(poise::CreateReply::default().embed(embed).ephemeral(true))
        .await?;

    Ok(())
}

async fn event_handler(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    _framework: poise::FrameworkContext<'_, Data, Error>,
    data: &Data,
) -> Result<(), Error> {
    // Fail-closed private-scope guard: ALL cache insertion / log construction runs inside the
    // sink, which is only invoked when the guard allows the event.
    tickets::privacy::guarded(
        data.config.ticket_privacy_lockdown,
        data.config.tickets.as_ref(),
        data.tickets.as_ref(),
        event,
        &tickets::privacy::SerenityLookup(ctx),
        &tickets::privacy::COUNTERS,
        || log_event(ctx, event, data),
    )
    .await
    .unwrap_or(Ok(()))
}

async fn log_event(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    // Fail closed: an event with no guild id (a DM or group DM) is never a guild event and
    // must not reach the guild log channel. Its content must never appear there either, so
    // the whole logger body - cache insertion, embed construction and the log write - runs
    // only behind this guard.
    let outcome = guarded_log(event, || log_event_body(ctx, event, data)).await;
    outcome.unwrap_or(Ok(()))
}

/// The general logger body: builds the log embed and writes it to the guild log channel.
///
/// Only reached for events that are provably guild events (see `guarded_log`).
async fn log_event_body(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    let log_channel_id = data.config.log_channel_id;

    match event {
        serenity::FullEvent::InteractionCreate {
            interaction: serenity::Interaction::Component(component),
        } => {
            if let (Some(settings), Some(repo)) = (&data.config.tickets, &data.tickets) {
                // Do not propagate raw ticket errors into Poise's general error logs.
                if tickets::runtime::component(ctx, component, settings, repo)
                    .await
                    .is_err()
                {
                    eprintln!("ticket interaction: private response unavailable");
                }
            }
        }
        serenity::FullEvent::Message { new_message } => {
            if new_message.channel_id.get() == log_channel_id {
                return Ok(());
            }

            let cached = cached_message_from_message(new_message);
            insert_cache(data, new_message.id, cached).await;
        }

        serenity::FullEvent::ReactionAdd { add_reaction } => {
            if add_reaction.user_id == Some(ctx.cache.current_user().id) {
                return Ok(());
            }

            let Some(guild_id) = add_reaction.guild_id else {
                return Ok(());
            };

            let Some(user_id) = add_reaction.user_id else {
                return Ok(());
            };

            let emoji_key = normalize_reaction_type(&add_reaction.emoji);

            let role_ids = rr_lookup_roles_for_message_emoji(
                data.rr_db.clone(),
                add_reaction.message_id.get(),
                emoji_key,
            )
            .await?;

            if role_ids.is_empty() {
                return Ok(());
            }

            let member = guild_id.member(&ctx.http, user_id).await?;
            for role_id in role_ids {
                let _ = member
                    .add_role(&ctx.http, serenity::RoleId::new(role_id))
                    .await;
            }
        }

        serenity::FullEvent::ReactionRemove { removed_reaction } => {
            let Some(guild_id) = removed_reaction.guild_id else {
                return Ok(());
            };

            let Some(user_id) = removed_reaction.user_id else {
                return Ok(());
            };

            if user_id == ctx.cache.current_user().id {
                return Ok(());
            }

            let emoji_key = normalize_reaction_type(&removed_reaction.emoji);

            let role_ids = rr_lookup_roles_for_message_emoji(
                data.rr_db.clone(),
                removed_reaction.message_id.get(),
                emoji_key,
            )
            .await?;

            if role_ids.is_empty() {
                return Ok(());
            }

            let member = guild_id.member(&ctx.http, user_id).await?;
            for role_id in role_ids {
                let _ = member
                    .remove_role(&ctx.http, serenity::RoleId::new(role_id))
                    .await;
            }
        }

        serenity::FullEvent::MessageDelete {
            channel_id,
            deleted_message_id,
            guild_id,
        } => {
            let cached = data.message_cache.write().await.remove(deleted_message_id);

            if let Some(cached) = cached {
                // Timestamp is taken at the delete event, not after the audit lookup.
                let event_time = serenity::Timestamp::now();
                let ctx = ctx.clone();
                let channel_id = *channel_id;
                let guild_id = *guild_id;
                // The audit lookup retries briefly, so it runs off the event handler.
                // This code is only reached through log_event, i.e. after the privacy guard.
                tokio::spawn(async move {
                    let deleted_by = lookup_deleted_by(
                        &ctx,
                        guild_id,
                        cached.author_id,
                        cached.is_bot,
                        channel_id.get(),
                    )
                    .await;
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
                        .field("Deleted By", deleted_by.field_text(), true)
                        .field("Content", code_block_or_placeholder(&cached.content), false)
                        .footer(build_log_footer())
                        .timestamp(event_time);

                    let _ = log_to_channel(&ctx, log_channel_id, embed).await;
                });
            } else {
                let embed = CreateEmbed::new()
                    .title("Message Deleted")
                    .color(Colour::DARK_RED)
                    .field(
                        "Channel",
                        format!("<#{}> (`{}`)", channel_id.get(), channel_id.get()),
                        false,
                    )
                    .field(
                        "Message ID",
                        format!("`{}`", deleted_message_id.get()),
                        false,
                    )
                    .field("Deleted By", DeletedBy::NotCached.field_text(), false)
                    .description("Original message data was not available in cache.")
                    .footer(build_log_footer())
                    .timestamp(serenity::Timestamp::now());

                let _ = log_to_channel(ctx, log_channel_id, embed).await;
            }
        }

        serenity::FullEvent::MessageDeleteBulk {
            channel_id,
            multiple_deleted_messages_ids,
            guild_id: _,
        } => {
            if channel_id.get() == log_channel_id {
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
        }

        serenity::FullEvent::MessageUpdate {
            old_if_available,
            new,
            event,
        } => {
            if event.channel_id.get() == log_channel_id {
                return Ok(());
            }

            let existing_cache = data.message_cache.read().await.get(&event.id).cloned();

            let author_name = existing_cache
                .as_ref()
                .map(|c| c.author_tag.clone())
                .or_else(|| new.as_ref().map(|m| format_user_label(&m.author)))
                .or_else(|| {
                    old_if_available
                        .as_ref()
                        .map(|m| format_user_label(&m.author))
                })
                .unwrap_or_else(|| "Unknown User".to_string());

            let author_avatar = existing_cache
                .as_ref()
                .and_then(|c| c.author_avatar_url.clone())
                .or_else(|| new.as_ref().and_then(|m| m.author.avatar_url()))
                .or_else(|| {
                    old_if_available
                        .as_ref()
                        .and_then(|m| m.author.avatar_url())
                });

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

            if !text_changed && !embeds_changed {
                return Ok(());
            }

            let before_display = if before.trim().is_empty() {
                "*No text content*".to_string()
            } else {
                before.clone()
            };

            let after_display = if after.trim().is_empty() {
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
                    format!(
                        "<#{}> (`{}`)",
                        event.channel_id.get(),
                        event.channel_id.get()
                    ),
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
                .field("Author Type", if is_bot { "Bot" } else { "User" }, true)
                .field(
                    "Text Changed",
                    if text_changed { "Yes" } else { "No" },
                    true,
                )
                .field(
                    "Embeds Changed",
                    if embeds_changed { "Yes" } else { "No" },
                    true,
                );

            if text_changed {
                embed = embed
                    .field(
                        "Before Text",
                        code_block_or_placeholder(&before_display),
                        false,
                    )
                    .field(
                        "After Text",
                        code_block_or_placeholder(&after_display),
                        false,
                    );
            }

            if embeds_changed {
                embed = embed.field(
                    "Embed Payloads",
                    "Rich embed JSON attached as file(s).",
                    false,
                );
            }

            embed = embed
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            if embeds_changed {
                let mut files = Vec::new();

                if before_embed_json != "*No embeds*" {
                    files.push(json_bytes_file("before_embeds.json", &before_embed_json));
                }

                if after_embed_json != "*No embeds*" {
                    files.push(json_bytes_file("after_embeds.json", &after_embed_json));
                }

                let _ = log_to_channel_with_files(ctx, log_channel_id, embed, files).await;
            } else {
                let _ = log_to_channel(ctx, log_channel_id, embed).await;
            }

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

                let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
        }

        serenity::FullEvent::ChannelDelete {
            channel,
            messages: _,
        } => {
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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
                .field(
                    "Mentionable",
                    if new.mentionable { "Yes" } else { "No" },
                    true,
                )
                .field(
                    "Permissions",
                    format!("`{}`", new.permissions.bits()),
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let old_hoist = old_data_if_available
                .as_ref()
                .map(|r| r.hoist)
                .unwrap_or(false);
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
                .field(
                    "Permissions After",
                    format!("`{}`", new.permissions.bits()),
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let old_pending = old_if_available
                .as_ref()
                .map(|m| m.pending)
                .unwrap_or(false);
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

            let added_roles: Vec<serenity::RoleId> = new_roles
                .iter()
                .filter(|role| !old_roles.contains(role))
                .copied()
                .collect();

            let removed_roles: Vec<serenity::RoleId> = old_roles
                .iter()
                .filter(|role| !new_roles.contains(role))
                .copied()
                .collect();

            let role_change_actor = if !added_roles.is_empty() || !removed_roles.is_empty() {
                find_recent_member_role_update_actor(ctx, event.guild_id, user_id).await
            } else {
                None
            };

            let mut embed = CreateEmbed::new()
                .author(build_author(&display_name, avatar.as_deref()))
                .title("Member Updated")
                .color(Colour::ORANGE)
                .field("Member", format!("<@{}>", user_id.get()), true)
                .field("User ID", format!("`{}`", user_id.get()), true)
                .field("Nickname Before", format_optional_text(old_nick), true)
                .field("Nickname After", format_optional_text(new_nick), true)
                .field("Roles Before", format_role_list(&old_roles), false)
                .field("Roles After", format_role_list(&new_roles), false);

            if !added_roles.is_empty() {
                embed = embed.field("Role Added", format_role_list(&added_roles), false);
            }

            if !removed_roles.is_empty() {
                embed = embed.field("Role Removed", format_role_list(&removed_roles), false);
            }

            if let Some(actor) = role_change_actor {
                let executor_id = actor
                    .moderator
                    .trim_start_matches("<@")
                    .trim_start_matches('!')
                    .trim_end_matches('>')
                    .parse::<u64>()
                    .unwrap_or(0);

                embed = embed
                    .field("Executor", actor.moderator, true)
                    .field("Target", format!("<@{}>", user_id.get()), true)
                    .field(
                        "ID",
                        format_user_id_codeblock(executor_id, user_id.get()),
                        false,
                    );
            }

            embed = embed
                .field(
                    "Pending Before",
                    if old_pending { "Yes" } else { "No" },
                    true,
                )
                .field(
                    "Pending After",
                    if new_pending { "Yes" } else { "No" },
                    true,
                )
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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
                let executor_id = info
                    .moderator
                    .trim_start_matches("<@")
                    .trim_start_matches('!')
                    .trim_end_matches('>')
                    .parse::<u64>()
                    .unwrap_or(0);

                let embed = CreateEmbed::new()
                    .author(build_author(&format_user_label(user), avatar.as_deref()))
                    .title("Member Banned")
                    .color(Colour::from_rgb(180, 32, 42))
                    .field("Member", format!("<@{}>", user.id.get()), true)
                    .field("User ID", format!("`{}`", user.id.get()), true)
                    .field("Moderator", info.moderator, false)
                    .field(
                        "ID",
                        format_user_id_codeblock(executor_id, user.id.get()),
                        false,
                    )
                    .field(
                        "Reason",
                        info.reason
                            .unwrap_or_else(|| "*No reason provided*".to_string()),
                        false,
                    )
                    .footer(build_log_footer())
                    .timestamp(serenity::Timestamp::now());

                let _ = log_to_channel(ctx, log_channel_id, embed).await;
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
                let executor_id = info
                    .moderator
                    .trim_start_matches("<@")
                    .trim_start_matches('!')
                    .trim_end_matches('>')
                    .parse::<u64>()
                    .unwrap_or(0);

                let embed = CreateEmbed::new()
                    .author(build_author(&format_user_label(user), avatar.as_deref()))
                    .title("Member Kicked")
                    .color(Colour::RED)
                    .field("Member", format!("<@{}>", user.id.get()), true)
                    .field("User ID", format!("`{}`", user.id.get()), true)
                    .field("Moderator", info.moderator, false)
                    .field(
                        "ID",
                        format_user_id_codeblock(executor_id, user.id.get()),
                        false,
                    )
                    .field(
                        "Reason",
                        info.reason
                            .unwrap_or_else(|| "*No reason provided*".to_string()),
                        false,
                    )
                    .footer(build_log_footer())
                    .timestamp(serenity::Timestamp::now());

                let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
        }

        serenity::FullEvent::GuildBanAddition {
            guild_id,
            banned_user,
        } => {
            let avatar = banned_user.avatar_url();

            let audit = find_recent_audit_actor_with_retry(
                ctx,
                *guild_id,
                banned_user.id,
                Action::Member(MemberAction::BanAdd),
            )
            .await;

            let executor_id = audit
                .as_ref()
                .and_then(|a| {
                    a.moderator
                        .trim_start_matches("<@")
                        .trim_start_matches('!')
                        .trim_end_matches('>')
                        .parse::<u64>()
                        .ok()
                })
                .unwrap_or(0);

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
                    audit
                        .as_ref()
                        .map(|a| a.moderator.clone())
                        .unwrap_or_else(|| "Unknown".to_string()),
                    false,
                )
                .field(
                    "ID",
                    format_user_id_codeblock(executor_id, banned_user.id.get()),
                    false,
                )
                .field(
                    "Reason",
                    audit
                        .and_then(|a| a.reason)
                        .unwrap_or_else(|| "*No reason provided*".to_string()),
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let executor_id = audit
                .as_ref()
                .and_then(|a| {
                    a.moderator
                        .trim_start_matches("<@")
                        .trim_start_matches('!')
                        .trim_end_matches('>')
                        .parse::<u64>()
                        .ok()
                })
                .unwrap_or(0);

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
                    audit
                        .as_ref()
                        .map(|a| a.moderator.clone())
                        .unwrap_or_else(|| "Unknown".to_string()),
                    false,
                )
                .field(
                    "ID",
                    format_user_id_codeblock(executor_id, unbanned_user.id.get()),
                    false,
                )
                .field(
                    "Reason",
                    audit
                        .and_then(|a| a.reason)
                        .unwrap_or_else(|| "*No reason provided*".to_string()),
                    false,
                )
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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
                .field(
                    "Event Type",
                    format_automod_event_type(rule.event_type),
                    true,
                )
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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
                .field(
                    "Event Type",
                    format_automod_event_type(rule.event_type),
                    true,
                )
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

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
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
                .field(
                    "Event Type",
                    format_automod_event_type(rule.event_type),
                    true,
                )
                .field(
                    "Trigger",
                    truncate_for_embed(&format_automod_trigger(rule), 1000),
                    false,
                )
                .field("Enabled", if rule.enabled { "Yes" } else { "No" }, true)
                .field("Actions", format_automod_action_count(&rule.actions), true)
                .footer(build_log_footer())
                .timestamp(serenity::Timestamp::now());

            let _ = log_to_channel(ctx, log_channel_id, embed).await;
        }

        _ => {}
    }

    Ok(())
}

#[poise::command(slash_command, guild_only, rename = "ticket-panel-post")]
async fn ticket_panel_post(
    ctx: Context<'_>,
    #[description = "Channel to post the ticket panel in"] channel: serenity::GuildChannel,
) -> Result<(), Error> {
    tickets::runtime::publish(ctx, channel).await
}

#[poise::command(slash_command, guild_only, rename = "ticket-close")]
async fn ticket_close(ctx: Context<'_>) -> Result<(), Error> {
    tickets::runtime_close::close(ctx).await
}

#[poise::command(slash_command, guild_only, rename = "ticket-recover")]
async fn ticket_recover(
    ctx: Context<'_>,
    #[description = "Ticket id"] ticket_id: i64,
    #[description = "status, attach or release"] action: String,
    #[description = "Channel id (attach only)"] channel: Option<u64>,
    #[description = "Explicit confirmation for attach/release"] confirm: Option<bool>,
    #[description = "Audit reason for attach/release"] reason: Option<String>,
) -> Result<(), Error> {
    tickets::runtime_close::recover(
        ctx,
        ticket_id,
        action,
        channel,
        confirm.unwrap_or(false),
        reason,
    )
    .await
}

#[poise::command(slash_command, guild_only, rename = "ticket-archives")]
async fn ticket_archives(
    ctx: Context<'_>,
    #[description = "list, show, health, orphans, retention or restore"] action: String,
    #[description = "Opaque archive ID (show or restore)"] id: Option<String>,
    #[description = "Number of rows to skip (list or health)"] offset: Option<u32>,
) -> Result<(), Error> {
    tickets::runtime_archives::command(ctx, &action, id.as_deref(), offset.unwrap_or(0) as usize)
        .await
}

fn configured_commands(enabled: bool) -> Vec<poise::Command<Data, Error>> {
    let mut commands = vec![age_verified(), reactionrole()];
    if enabled {
        commands.push(ticket_panel_post());
        commands.push(ticket_close());
        commands.push(ticket_recover());
        commands.push(ticket_archives());
    }
    commands
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    match config::load_env() {
        Ok(path) => println!("Loaded .env from: {}", path.display()),
        Err(err) => println!("Could not load .env: {}", err),
    }

    let config = Arc::new(Config::from_env()?);
    let token = config.discord_token.clone();

    let intents = serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_MEMBERS
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::GUILD_MESSAGE_REACTIONS
        | serenity::GatewayIntents::MESSAGE_CONTENT
        | serenity::GatewayIntents::GUILD_MODERATION
        | serenity::GatewayIntents::GUILD_VOICE_STATES
        | serenity::GatewayIntents::AUTO_MODERATION_CONFIGURATION;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: configured_commands(config.tickets.is_some()),
            event_handler: |ctx, event, framework, data| {
                Box::pin(event_handler(ctx, event, framework, data))
            },
            ..Default::default()
        })
        .setup({
            let config = config.clone();
            move |ctx, ready, framework| {
                let config = config.clone();
                Box::pin(async move {
                    println!("Bot is online as {}", ready.user.name);

                    poise::builtins::register_globally(ctx, &framework.options().commands).await?;

                    let rr_db = init_reaction_role_db(&config.reaction_role_db_path).await?;
                    let tickets=match &config.tickets {
                        Some(settings)=>Some(tickets::repository::Repository::open(&settings.db_path).await?),
                        None=>None,
                    };
                    if let (Some(settings), Some(repo)) = (&config.tickets, &tickets) {
                        // Reconciliation runs before opening could ever be approved; failure keeps opening gated.
                        match tickets::runtime_close::startup_reconcile(ctx, settings, repo).await {
                            Ok(r) => { tickets::runtime::record_reconcile(Some(&r)); println!("Ticket reconciliation: {} to recovery, {} closed by known absence, {} unresolved; opening permitted: {}", r.moved_to_recovery, r.closed_by_known_absence, r.unresolved, tickets::runtime::opening_permitted()) }
                            Err(_) => { tickets::runtime::record_reconcile(None); println!("Ticket reconciliation failed; opening remains gated.") }
                        }
                    }
                    if config.ticket_privacy_lockdown { println!("Ticket privacy lockdown: channel events in private ticket scope are skipped fail-closed; ticket opening remains gated."); }

                    Ok(Data {
                        config,
                        message_cache: Arc::new(RwLock::new(HashMap::new())),
                        rr_db,
                        tickets,
                    })
                })
            }
        })
        .build();

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await?;

    client.start().await?;

    Ok(())
}

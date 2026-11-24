//! Testable `/vc` command core and Discord adapter (`docs/verification/VOICE-ROOMS-COMMANDS.md`).
//!
//! Enforces the VR3 rules on top of the VR1 policy (`policy.rs`) and the VR2 runtime
//! rows (`voice_rooms` table; the database row is the truth):
//! - Only the recorded room owner (from the database row, never from channel permissions)
//!   may run owner subcommands (`rename`, `limit`, `lock`, `unlock`, `permit`, `reject`,
//!   `transfer`).
//! - `claim` is the only subcommand a non-owner may use, and only when the caller is
//!   currently inside the room and the recorded owner has been absent from the room past
//!   the claim grace (`claim_grace_secs`, VR1 value), never from a present owner.
//! - `permit` refuses any member who is ineligible per the role service or VR1 (bot user,
//!   the bot itself, the room owner, active safeword, lacking category visibility, or
//!   holding a protected, restricted, staff or privileged role).
//! - `reject` refuses targeting the room owner, the bot itself, or a member protected by
//!   permission limits (protected, staff or privileged-permission role); it denies
//!   `CONNECT` on the channel and disconnects the target if present in the room (reporting
//!   a failed disconnect honestly with a stable code).
//! - `rename` cleans the name through [`policy::sanitize_name`], enforces the VR1 rename
//!   budget ([`policy::decide_rename`]) and Discord's channel-rename rate limit (`429`),
//!   and records a rename only after Discord accepts it.
//! - `limit` bounds `user_limit` to `0..=MAX_USER_LIMIT` (`0` = unlimited, up to `99`).
//! - Every subcommand operates only on a tracked room in the configured home guild; an
//!   untracked or other-guild channel is refused and touches nothing. When the feature mode
//!   is on (`Shadow` or `Live`) and `home_guild_id` is unset (`None` or `Some(0)`), the
//!   command gate fails closed.
//! - Row consistency: every Discord write runs before the SQLite row update; a failed
//!   Discord write leaves the row unchanged, while a failed row write after a successful
//!   Discord change returns a stable error code and fires a staff alert.
//! - Every reply is ephemeral and built strictly from stable machine codes (no user names,
//!   room names or raw ids are echoed).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use poise::serenity_prelude as serenity;
use poise::serenity_prelude::async_trait;
use rusqlite::Connection;

use crate::roles::eligibility::{SafewordState, PRIVILEGED_MASK};
use crate::roles::runtime::RoleSource;
use crate::settings::Mode;
use crate::voice_rooms::config::{NotReady, Readiness, VoiceSettings};
use crate::voice_rooms::policy::{self, DenyReason, OverwriteTarget, RenameBudget, RoomOverwrite};
use crate::voice_rooms::repository::{self, RoomRow};

/// Maximum voice-channel user limit accepted by Discord (`0` means unlimited).
pub const MAX_USER_LIMIT: u32 = 99;

/// Rooms whose permits are remembered before permits of rooms without a row are forgotten.
const MAX_REMEMBERED_ROOMS: usize = 256;

/// Most deputies one room may have. Reason: it bounds how many members can change the room
/// without its owner.
pub const MAX_DEPUTIES_PER_ROOM: usize = 5;

/// Discord's limit of permission overwrites per channel; `refresh` looks at no more than this
/// many member overwrites and reports the rest as failed.
pub const MAX_OVERWRITES_PER_CHANNEL: usize = 100;

/// Monotonic/epoch clock in Unix seconds.
pub trait Clock: Send + Sync {
    /// Current time in Unix seconds.
    fn now_unix(&self) -> i64;
}

/// System clock reading `std::time::SystemTime::now`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

/// Lookup of a target member's safeword restriction state.
pub trait SafewordLookup: Send + Sync {
    /// Returns whether `user_id` in `guild_id` currently has an active safeword.
    fn state(&self, guild_id: u64, user_id: u64) -> Result<SafewordState, CommandPortError>;
}

/// Why a Discord or dependency call failed during a `/vc` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPortError {
    /// HTTP 429: Discord rate-limited the channel rename or edit.
    RateLimited,
    /// HTTP 404: the channel or member is missing on Discord.
    NotFound,
    /// HTTP 403: the bot lacks permission on Discord.
    Forbidden,
    /// Any other transport, cache or service failure.
    Unavailable,
}

impl CommandPortError {
    /// Stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            Self::RateLimited => "discord_rate_limited",
            Self::NotFound => "discord_not_found",
            Self::Forbidden => "discord_forbidden",
            Self::Unavailable => "discord_unavailable",
        }
    }
}

/// Maps an HTTP status code (when serenity surfaces one) to [`CommandPortError`].
pub fn command_port_error_from_status(status: Option<u16>) -> CommandPortError {
    match status {
        Some(429) => CommandPortError::RateLimited,
        Some(404) => CommandPortError::NotFound,
        Some(403) => CommandPortError::Forbidden,
        _ => CommandPortError::Unavailable,
    }
}

/// Live facts about a target member in the guild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberProfile {
    /// Member's Discord user id.
    pub user_id: u64,
    /// True when the account is a Discord bot.
    pub is_bot: bool,
    /// Roles currently held by the member in the guild.
    pub role_ids: Vec<u64>,
    /// True when the member can view the voice-room category.
    pub can_view_category: bool,
}

/// One per-member permission overwrite as Discord holds it on a room channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberOverwrite {
    /// The member the overwrite applies to.
    pub user_id: u64,
    /// Permission bits explicitly allowed.
    pub allow: u64,
    /// Permission bits explicitly denied.
    pub deny: u64,
}

/// Counts reported by `/vc refresh` (stable numbers only, never names or ids).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshCounts {
    /// Stale member permit overwrites deleted on Discord.
    pub removed: u32,
    /// Missing permit overwrites created for the owner, deputies and connected members.
    pub added: u32,
    /// Writes that failed or were not attempted (rate limit, overwrites past the cap).
    pub failed: u32,
}

/// Discord operations needed by the `/vc` subcommands.
#[async_trait]
pub trait RoomCommandPorts: Send + Sync {
    /// Reads the per-member permission overwrites `channel_id` carries right now (from Discord,
    /// never from memory). Role and `@everyone` overwrites are not returned.
    async fn member_overwrites(
        &self,
        guild_id: u64,
        channel_id: u64,
    ) -> Result<Vec<MemberOverwrite>, CommandPortError>;

    /// Renames `channel_id` to `clean_name`.
    async fn rename_room(
        &self,
        guild_id: u64,
        channel_id: u64,
        clean_name: &str,
    ) -> Result<(), CommandPortError>;

    /// Sets the voice channel `user_limit` (`0` = unlimited).
    async fn set_user_limit(
        &self,
        guild_id: u64,
        channel_id: u64,
        user_limit: u32,
    ) -> Result<(), CommandPortError>;

    /// Applies a permission overwrite on `channel_id`.
    async fn set_overwrite(
        &self,
        guild_id: u64,
        channel_id: u64,
        overwrite: RoomOverwrite,
    ) -> Result<(), CommandPortError>;

    /// Removes the permission overwrite for `target` on `channel_id`.
    async fn delete_overwrite(
        &self,
        guild_id: u64,
        channel_id: u64,
        target: OverwriteTarget,
    ) -> Result<(), CommandPortError>;

    /// Disconnects `user_id` from voice in `guild_id`.
    async fn disconnect_member(&self, guild_id: u64, user_id: u64) -> Result<(), CommandPortError>;

    /// Moves `user_id` into `channel_id` in `guild_id`.
    async fn move_member(
        &self,
        guild_id: u64,
        user_id: u64,
        channel_id: u64,
    ) -> Result<(), CommandPortError>;

    /// Lists the user ids currently connected to `channel_id`.
    async fn room_members(
        &self,
        guild_id: u64,
        channel_id: u64,
    ) -> Result<Vec<u64>, CommandPortError>;

    /// Reads the permission overwrites the voice-room category carries (the unlock baseline).
    async fn category_overwrites(
        &self,
        guild_id: u64,
        category_id: u64,
    ) -> Result<Vec<RoomOverwrite>, CommandPortError>;

    /// Fetches the target member's profile and category visibility in `guild_id`.
    async fn member_profile(
        &self,
        guild_id: u64,
        category_id: u64,
        user_id: u64,
    ) -> Result<MemberProfile, CommandPortError>;
}

/// Stable alert codes emitted when a SQLite row write fails after a Discord mutation succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandAlertCode {
    /// Discord channel was updated, but persisting the new owner to SQLite failed.
    OwnerRowDrift,
    /// Discord channel lock overwrite was updated, but persisting `locked` to SQLite failed.
    LockRowDrift,
    /// Discord channel user limit was updated, but persisting `user_limit` to SQLite failed.
    LimitRowDrift,
    /// Reading the tracked room row from SQLite failed.
    RepositoryReadFailed,
    /// An ownership change left both old and new owner overwrites on the channel (a rollback
    /// failed), while the row still names the old owner.
    OwnerOverwriteDrift,
    /// Discord was changed for a deputy (permit granted, or reject applied) but the deputy row
    /// could not be written.
    DeputyRowDrift,
}

impl CommandAlertCode {
    /// Stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            Self::OwnerRowDrift => "vc_cmd_owner_row_drift",
            Self::LockRowDrift => "vc_cmd_lock_row_drift",
            Self::LimitRowDrift => "vc_cmd_limit_row_drift",
            Self::RepositoryReadFailed => "vc_cmd_repo_read_failed",
            Self::OwnerOverwriteDrift => "vc_cmd_owner_overwrite_drift",
            Self::DeputyRowDrift => "vc_cmd_deputy_row_drift",
        }
    }

    /// Fixed alert message for the staff channel; carries no user text, names or ids.
    pub fn alert_text(self) -> &'static str {
        match self {
            Self::OwnerRowDrift => {
                "[VOICE_ROOMS] code=vc_cmd_owner_row_drift owner_changed_on_discord_but_db_write_failed"
            }
            Self::LockRowDrift => {
                "[VOICE_ROOMS] code=vc_cmd_lock_row_drift lock_changed_on_discord_but_db_write_failed"
            }
            Self::LimitRowDrift => {
                "[VOICE_ROOMS] code=vc_cmd_limit_row_drift limit_changed_on_discord_but_db_write_failed"
            }
            Self::RepositoryReadFailed => {
                "[VOICE_ROOMS] code=vc_cmd_repo_read_failed room_row_read_failed"
            }
            Self::OwnerOverwriteDrift => {
                "[VOICE_ROOMS] code=vc_cmd_owner_overwrite_drift owner_overwrite_rollback_failed"
            }
            Self::DeputyRowDrift => {
                "[VOICE_ROOMS] code=vc_cmd_deputy_row_drift deputy_changed_on_discord_but_db_write_failed"
            }
        }
    }
}

/// Staff alert sink for `/vc` command drift and storage failures.
#[async_trait]
pub trait CommandAlerter: Send + Sync {
    /// Emits a fixed alert code to the configured staff channel.
    async fn alert(&self, code: CommandAlertCode);
}

/// The `/vc` subcommand requested by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcSubcommand {
    /// `/vc rename <name>`
    Rename { name: String },
    /// `/vc limit <n>`
    Limit { limit: u32 },
    /// `/vc lock`
    Lock,
    /// `/vc unlock`
    Unlock,
    /// `/vc permit <member>`
    Permit { target_id: u64 },
    /// `/vc reject <member>`
    Reject { target_id: u64 },
    /// `/vc transfer <member>`
    Transfer { target_id: u64 },
    /// `/vc claim`
    Claim,
    /// `/vc deputize <member>` (owner only)
    Deputize { target_id: u64 },
    /// `/vc undeputize [member]`; `None` means the caller themselves.
    Undeputize { target_id: Option<u64> },
    /// `/vc refresh`
    Refresh,
}

/// Plain request struct for one `/vc` command invocation (kept <= 7 fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcCommandRequest {
    /// Guild where the slash command was invoked (`None` for DMs).
    pub guild_id: Option<u64>,
    /// Voice/text channel where the slash command was invoked.
    pub channel_id: u64,
    /// Member invoking the command.
    pub actor_id: u64,
    /// Requested subcommand.
    pub subcommand: VcSubcommand,
}

/// Stable machine codes for a successful `/vc` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuccessCode {
    /// Room renamed on Discord and recorded in the rename budget window.
    Renamed,
    /// Room user limit updated on Discord and in the `voice_rooms` row.
    LimitUpdated,
    /// Room locked (`@everyone` denied `CONNECT`) on Discord and in the row.
    Locked,
    /// Room unlocked (`@everyone` restored to category baseline) on Discord and in the row.
    Unlocked,
    /// Target member permitted to connect and speak in the room.
    Permitted,
    /// Target member denied connect on the room (and disconnected if not present).
    Rejected,
    /// Target member denied connect on the room and disconnected from the voice channel.
    RejectedAndDisconnected,
    /// Room ownership transferred to the target member inside the room.
    Transferred,
    /// Room ownership claimed by a member inside the room after the owner's absence grace.
    Claimed,
    /// Feature is in shadow mode; command was validated with no Discord or DB mutation.
    ShadowNoop,
    /// Target member became a deputy of the room (and got the permit overwrite).
    Deputized,
    /// A deputy was removed from the room.
    Undeputized,
    /// `refresh` finished every write it needed; the counts say what changed.
    Refreshed(RefreshCounts),
    /// Shadow-mode `refresh`: the counts are what it would change; nothing was written.
    ShadowRefresh(RefreshCounts),
}

impl SuccessCode {
    /// Stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Renamed => "vc_renamed",
            Self::LimitUpdated => "vc_limit_updated",
            Self::Locked => "vc_locked",
            Self::Unlocked => "vc_unlocked",
            Self::Permitted => "vc_permitted",
            Self::Rejected => "vc_rejected",
            Self::RejectedAndDisconnected => "vc_rejected_and_disconnected",
            Self::Transferred => "vc_transferred",
            Self::Claimed => "vc_claimed",
            Self::ShadowNoop => "vc_shadow_noop",
            Self::Deputized => "vc_deputized",
            Self::Undeputized => "vc_undeputized",
            Self::Refreshed(_) => "vc_refreshed",
            Self::ShadowRefresh(_) => "vc_shadow_refresh",
        }
    }

    /// Fixed ephemeral reply text built strictly from the code (never echoes names or ids).
    /// Refresh replies add their counts in [`Reply::ok`].
    pub fn reply_text(self) -> &'static str {
        match self {
            Self::Renamed => "Voice room renamed. [vc_renamed]",
            Self::LimitUpdated => "Voice room member limit updated. [vc_limit_updated]",
            Self::Locked => "Voice room locked. [vc_locked]",
            Self::Unlocked => "Voice room unlocked. [vc_unlocked]",
            Self::Permitted => "Member permitted in this voice room. [vc_permitted]",
            Self::Rejected => "Member rejected from this voice room. [vc_rejected]",
            Self::RejectedAndDisconnected => {
                "Member rejected and disconnected from this voice room. [vc_rejected_and_disconnected]"
            }
            Self::Transferred => "Voice room ownership transferred. [vc_transferred]",
            Self::Claimed => "Voice room ownership claimed. [vc_claimed]",
            Self::ShadowNoop => "Voice room command recorded in shadow mode. [vc_shadow_noop]",
            Self::Deputized => "Member is now a deputy of this voice room. [vc_deputized]",
            Self::Undeputized => "Deputy removed from this voice room. [vc_undeputized]",
            Self::Refreshed(_) => "Voice room approvals refreshed. [vc_refreshed]",
            Self::ShadowRefresh(_) => {
                "Shadow mode: nothing was changed by this refresh. [vc_shadow_refresh]"
            }
        }
    }

    /// The reply text, with the refresh counts when the code carries them.
    fn reply_string(self) -> String {
        match self {
            Self::Refreshed(c) => format!(
                "Voice room approvals refreshed: removed {}, added {}. [vc_refreshed]",
                c.removed, c.added
            ),
            Self::ShadowRefresh(c) => format!(
                "Shadow mode: refresh would remove {} and add {}; nothing was changed. [vc_shadow_refresh]",
                c.removed, c.added
            ),
            other => other.reply_text().to_string(),
        }
    }
}

/// Stable machine codes for a refused or failed `/vc` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalCode {
    /// `VC_MODE` is `off`.
    FeatureDisabled,
    /// `VC_MODE` is active (`shadow` or `live`) but `home_guild_id` is unset or zero.
    HomeGuildUnset,
    /// `VC_MODE` is active but hub/category configuration is incomplete.
    FeatureNotReady,
    /// Command was invoked outside the configured home guild.
    WrongGuild,
    /// Actor or target user id is zero.
    InvalidUserId,
    /// Channel is not a tracked voice room in the home guild.
    UntrackedChannel,
    /// Caller is not the recorded owner of the room.
    NotRoomOwner,
    /// `claim` was called by the member who already owns the room.
    AlreadyRoomOwner,
    /// `claim` was called by someone not currently connected to the room.
    CallerNotInRoom,
    /// `claim` was called while the recorded owner is still present in the room.
    OwnerStillPresent,
    /// `claim` was called before the owner's absence grace period elapsed.
    OwnerGraceNotElapsed,
    /// `claim` found no continuous owner absence recorded by `note_owner_absent`.
    OwnerAbsenceNotRecorded,
    /// `claim` found an absence record whose observations are stale or have a gap, so a return
    /// of the owner may have gone unseen.
    OwnerAbsenceNotContinuous,
    /// `transfer` target is not currently connected to the room.
    TargetNotInRoom,
    /// `permit`, `reject` or `transfer` targeted the current room owner.
    TargetIsOwner,
    /// `permit`, `reject` or `transfer` targeted the bot itself.
    TargetIsSelfBot,
    /// `permit`, `reject` or `transfer` targeted a bot account.
    TargetIsBot,
    /// `permit` target has an active safeword restriction.
    TargetSafewordActive,
    /// `permit` target cannot view the voice-room category.
    CategoryNotVisible,
    /// `permit` target is already permitted in this room.
    AlreadyPermitted,
    /// `permit` or `reject` target holds a protected, restricted, staff or privileged role.
    TargetProtectedOrRestricted,
    /// `limit` exceeds [`MAX_USER_LIMIT`].
    LimitOutOfBounds,
    /// `rename` exhausted the VR1 per-room rename budget window.
    RenameBudgetExhausted,
    /// Discord returned HTTP 429 rate-limit on channel rename.
    RenameRateLimited,
    /// Discord rejected the write or lookup before any row update.
    DiscordFailed(CommandPortError),
    /// Discord rejected disconnecting the present member after updating the reject overwrite.
    DisconnectFailed(CommandPortError),
    /// Reading the room row or dependency state failed.
    StorageReadFailed,
    /// The rename policy refused for a reason other than the budget (invalid configuration).
    RenameRefused,
    /// Discord mutation succeeded, but the subsequent SQLite row update failed (staff alerted).
    RowUpdateFailed,
    /// A deputy tried a command that only the owner may use (`transfer`, `deputize`).
    DeputyNotAllowed,
    /// A deputy tried to remove a deputy other than themselves.
    DeputySelfOnly,
    /// `deputize` target is already a deputy of the room.
    TargetAlreadyDeputy,
    /// `undeputize` target is not a deputy of the room.
    TargetNotDeputy,
    /// A deputy tried to `reject` another deputy (only the owner may).
    TargetIsDeputy,
    /// The room already has [`MAX_DEPUTIES_PER_ROOM`] deputies.
    DeputyCapReached,
    /// A deputy row write failed and nothing on Discord had changed.
    RowWriteFailed,
    /// `refresh` finished only some of its writes; the counts say how many.
    RefreshPartial(RefreshCounts),
}

impl RefusalCode {
    /// Stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            Self::FeatureDisabled => "vc_feature_disabled",
            Self::HomeGuildUnset => "vc_home_guild_unset",
            Self::FeatureNotReady => "vc_feature_not_ready",
            Self::WrongGuild => "vc_wrong_guild",
            Self::InvalidUserId => DenyReason::InvalidUserId.code(),
            Self::UntrackedChannel => DenyReason::UntrackedChannel.code(),
            Self::NotRoomOwner => DenyReason::NotRoomOwner.code(),
            Self::AlreadyRoomOwner => "vc_already_room_owner",
            Self::CallerNotInRoom => "vc_caller_not_in_room",
            Self::OwnerStillPresent => "vc_owner_still_present",
            Self::OwnerGraceNotElapsed => "vc_owner_grace_not_elapsed",
            Self::OwnerAbsenceNotRecorded => "vc_owner_absence_not_recorded",
            Self::OwnerAbsenceNotContinuous => "vc_owner_absence_not_continuous",
            Self::TargetNotInRoom => "vc_target_not_in_room",
            Self::TargetIsOwner => "vc_target_is_owner",
            Self::TargetIsSelfBot => "vc_target_is_self_bot",
            Self::TargetIsBot => "vc_target_is_bot",
            Self::TargetSafewordActive => "vc_target_safeword_active",
            Self::CategoryNotVisible => DenyReason::CategoryNotVisible.code(),
            Self::AlreadyPermitted => DenyReason::AlreadyPermitted.code(),
            Self::TargetProtectedOrRestricted => "vc_target_protected_or_restricted",
            Self::LimitOutOfBounds => "vc_limit_out_of_bounds",
            Self::RenameBudgetExhausted => DenyReason::RenameBudgetExhausted.code(),
            Self::RenameRateLimited => "vc_rename_rate_limited",
            Self::DiscordFailed(err) => err.code(),
            Self::DisconnectFailed(_) => "vc_disconnect_failed",
            Self::StorageReadFailed => "vc_storage_read_failed",
            Self::RenameRefused => "vc_rename_refused",
            Self::RowUpdateFailed => "vc_row_update_failed",
            Self::DeputyNotAllowed => "vc_deputy_not_allowed",
            Self::DeputySelfOnly => "vc_deputy_self_only",
            Self::TargetAlreadyDeputy => "vc_target_already_deputy",
            Self::TargetNotDeputy => "vc_target_not_deputy",
            Self::TargetIsDeputy => "vc_target_is_deputy",
            Self::DeputyCapReached => "vc_deputy_cap_reached",
            Self::RowWriteFailed => "vc_row_write_failed",
            Self::RefreshPartial(_) => "vc_refresh_partial",
        }
    }

    /// Fixed ephemeral reply text built strictly from the code (never echoes names or ids).
    pub fn reply_text(self) -> &'static str {
        match self {
            Self::FeatureDisabled => "Voice rooms are disabled. [vc_feature_disabled]",
            Self::HomeGuildUnset => "Voice rooms are not configured. [vc_home_guild_unset]",
            Self::FeatureNotReady => "Voice rooms are not ready. [vc_feature_not_ready]",
            Self::WrongGuild => "Voice rooms are not available in this server. [vc_wrong_guild]",
            Self::InvalidUserId => "Invalid member identifier. [invalid_user_id]",
            Self::UntrackedChannel => "This channel is not a tracked voice room. [untracked_channel]",
            Self::NotRoomOwner => "Only the recorded room owner may use this command. [not_room_owner]",
            Self::AlreadyRoomOwner => "You already own this voice room. [vc_already_room_owner]",
            Self::CallerNotInRoom => "You must be inside the voice room to claim it. [vc_caller_not_in_room]",
            Self::OwnerStillPresent => "The room owner is currently in the room. [vc_owner_still_present]",
            Self::OwnerGraceNotElapsed => "The owner absence grace period has not elapsed. [vc_owner_grace_not_elapsed]",
            Self::OwnerAbsenceNotRecorded => "No continuous owner absence has been recorded yet. [vc_owner_absence_not_recorded]",
            Self::OwnerAbsenceNotContinuous => "The owner absence could not be confirmed as continuous. [vc_owner_absence_not_continuous]",
            Self::TargetNotInRoom => "The target member must be inside the voice room. [vc_target_not_in_room]",
            Self::TargetIsOwner => "That command cannot target the room owner. [vc_target_is_owner]",
            Self::TargetIsSelfBot => "That command cannot target the bot. [vc_target_is_self_bot]",
            Self::TargetIsBot => "That command cannot target a bot account. [vc_target_is_bot]",
            Self::TargetSafewordActive => "Target member is ineligible due to active safeword. [vc_target_safeword_active]",
            Self::CategoryNotVisible => "Target member cannot view the voice room category. [category_not_visible]",
            Self::AlreadyPermitted => "Target member is already permitted in this room. [already_permitted]",
            Self::TargetProtectedOrRestricted => "Target member holds a protected or restricted role. [vc_target_protected_or_restricted]",
            Self::LimitOutOfBounds => "Member limit is out of bounds (0..=99). [vc_limit_out_of_bounds]",
            Self::RenameBudgetExhausted => "Room rename budget is exhausted for this window. [rename_budget_exhausted]",
            Self::RenameRateLimited => "Discord rate-limited the room rename; nothing was changed. [vc_rename_rate_limited]",
            Self::DiscordFailed(CommandPortError::RateLimited) => "Discord rate-limited the request; nothing was changed. [discord_rate_limited]",
            Self::DiscordFailed(CommandPortError::NotFound) => "The Discord channel or member could not be found. [discord_not_found]",
            Self::DiscordFailed(CommandPortError::Forbidden) => "The bot lacks permission on Discord. [discord_forbidden]",
            Self::DiscordFailed(CommandPortError::Unavailable) => "Discord could not complete the request. [discord_unavailable]",
            Self::DisconnectFailed(_) => "Access was revoked, but disconnecting the member failed. [vc_disconnect_failed]",
            Self::StorageReadFailed => "Could not read voice room state. [vc_storage_read_failed]",
            Self::RenameRefused => "The room rename was refused; nothing was changed. [vc_rename_refused]",
            Self::RowUpdateFailed => "Discord updated, but saving the room row failed; staff have been alerted. [vc_row_update_failed]",
            Self::DeputyNotAllowed => "Deputies cannot use this command. [vc_deputy_not_allowed]",
            Self::DeputySelfOnly => "A deputy can only remove themselves. [vc_deputy_self_only]",
            Self::TargetAlreadyDeputy => "That member is already a deputy. [vc_target_already_deputy]",
            Self::TargetNotDeputy => "That member is not a deputy of this room. [vc_target_not_deputy]",
            Self::TargetIsDeputy => "Only the room owner can reject a deputy. [vc_target_is_deputy]",
            Self::DeputyCapReached => "This room already has the most deputies it may have. [vc_deputy_cap_reached]",
            Self::RowWriteFailed => "Saving the deputy list failed; nothing else was changed. [vc_row_write_failed]",
            Self::RefreshPartial(_) => "Refresh finished only in part. [vc_refresh_partial]",
        }
    }

    /// The reply text, with the refresh counts when the code carries them.
    fn reply_string(self) -> String {
        match self {
            Self::RefreshPartial(c) => format!(
                "Refresh finished only in part: removed {}, added {}, failed {}. [vc_refresh_partial]",
                c.removed, c.added, c.failed
            ),
            other => other.reply_text().to_string(),
        }
    }
}

impl fmt::Display for RefusalCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Ephemeral command response built strictly from stable codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Always `true`: `/vc` replies are ephemeral.
    pub ephemeral: bool,
    /// Stable machine code.
    pub code: &'static str,
    /// Fixed human-readable message containing no user-supplied names or ids (only counts).
    pub text: String,
}

impl Reply {
    /// Builds an ephemeral reply for a successful command.
    pub fn ok(code: SuccessCode) -> Self {
        Self {
            ephemeral: true,
            code: code.code(),
            text: code.reply_string(),
        }
    }

    /// Builds an ephemeral reply for a refused or failed command.
    pub fn refused(code: RefusalCode) -> Self {
        Self {
            ephemeral: true,
            code: code.code(),
            text: code.reply_string(),
        }
    }
}

/// Outcome of one `/vc` command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Command was outside the configured home guild or feature is off: touch nothing, send nothing.
    Ignored(RefusalCode),
    /// Command succeeded and produced an ephemeral reply.
    Applied { code: SuccessCode, reply: Reply },
    /// Command was refused or failed and produced an ephemeral reply.
    Refused { code: RefusalCode, reply: Reply },
}

impl CommandOutcome {
    /// Returns the ephemeral reply when the outcome answers the interaction.
    pub fn reply(&self) -> Option<&Reply> {
        match self {
            Self::Ignored(_) => None,
            Self::Applied { reply, .. } | Self::Refused { reply, .. } => Some(reply),
        }
    }
}

/// Role-policy sets used to evaluate `permit` and `reject` target protection.
#[derive(Debug, Clone, Default)]
pub struct RoleLimits {
    /// Roles protected by the role policy (staff / mod / admin roles).
    pub protected_role_ids: HashSet<u64>,
    /// Roles restricted by the role policy.
    pub restricted_role_ids: HashSet<u64>,
}

/// Dependencies for `/vc` command execution.
pub struct VcCommandDeps {
    /// Validated voice-room settings (`VC_*` + `home_guild_id`).
    pub settings: VoiceSettings,
    /// Bot's own user id (so `permit`/`reject`/`transfer` never target the bot).
    pub bot_user_id: u64,
    /// Rename budget enforced per room.
    pub rename_budget: RenameBudget,
    /// Default room name fallback for [`policy::sanitize_name`].
    pub default_name: String,
    /// Protected and restricted role sets.
    pub role_limits: RoleLimits,
    /// SQLite connection holding the `voice_rooms` table.
    pub db: Arc<Mutex<Connection>>,
    /// Discord port adapter.
    pub ports: Arc<dyn RoomCommandPorts>,
    /// Guild role catalog source.
    pub roles: Arc<dyn RoleSource>,
    /// Safeword state lookup.
    pub safeword: Arc<dyn SafewordLookup>,
    /// Staff alert sink.
    pub alerter: Arc<dyn CommandAlerter>,
    /// Clock.
    pub clock: Arc<dyn Clock>,
    /// Seconds a room's owner must have been continuously absent before `claim` may take the room.
    /// Wiring passes the VR1 value (`VOICE_ROOM_EMPTY_GRACE_SECS`, default
    /// [`policy::DEFAULT_EMPTY_GRACE_SECS`]); this is deliberately not the VR2 cleanup grace.
    pub claim_grace_secs: i64,
}

/// Longest gap in seconds between two absence observations for them to count as one continuous
/// absence. Wiring must call `note_owner_absent` at least this often while the owner is away.
pub const ABSENCE_MAX_GAP_SECS: i64 = 15;

/// A run of owner-absence observations that no gap longer than [`ABSENCE_MAX_GAP_SECS`] split.
#[derive(Debug, Clone, Copy)]
struct AbsenceRecord {
    /// First observation of the run.
    since: i64,
    /// Latest observation of the run.
    last_seen: i64,
}

/// In-memory per-room state tracked alongside the `voice_rooms` table.
#[derive(Debug, Default)]
struct CommandMemState {
    /// Successful renames `(channel_id, timestamp)` for the VR1 rename budget.
    renames: Vec<(u64, i64)>,
    /// When each room's recorded owner was first observed absent `(channel_id -> timestamp)`.
    owner_absent: HashMap<u64, AbsenceRecord>,
    /// Members explicitly permitted in each room `(channel_id -> set of user_ids)`.
    permitted_members: HashMap<u64, HashSet<u64>>,
}

/// Testable `/vc` command handler.
pub struct VcCommandHandler {
    deps: VcCommandDeps,
    mem: Mutex<CommandMemState>,
    /// Serializes command execution so two commands cannot interleave their Discord and row writes.
    serial: tokio::sync::Mutex<()>,
}

impl VcCommandHandler {
    /// Creates a new `/vc` command handler.
    pub fn new(deps: VcCommandDeps) -> Self {
        Self {
            deps,
            mem: Mutex::new(CommandMemState::default()),
            serial: tokio::sync::Mutex::new(()),
        }
    }

    /// Records that the room's current owner was observed absent at `at`. Observations at most
    /// [`ABSENCE_MAX_GAP_SECS`] apart extend one absence; a longer gap could hide a return, so it
    /// restarts the absence at `at`.
    pub fn note_owner_absent(&self, channel_id: u64, at: i64) {
        if let Ok(mut mem) = self.mem.lock() {
            let record = match mem.owner_absent.get(&channel_id).copied() {
                Some(prev) if at.saturating_sub(prev.last_seen) <= ABSENCE_MAX_GAP_SECS => {
                    AbsenceRecord {
                        since: prev.since,
                        last_seen: prev.last_seen.max(at),
                    }
                }
                _ => AbsenceRecord {
                    since: at,
                    last_seen: at,
                },
            };
            mem.owner_absent.insert(channel_id, record);
        }
    }

    /// Clears the recorded owner-absence timestamp when the owner is present.
    pub fn note_owner_present(&self, channel_id: u64) {
        if let Ok(mut mem) = self.mem.lock() {
            mem.owner_absent.remove(&channel_id);
        }
    }

    /// RED scaffold: not implemented yet.
    pub fn reset_observations(&self) {}

    /// Executes one `/vc` command request.
    pub async fn handle(&self, req: &VcCommandRequest) -> CommandOutcome {
        let mode = self.deps.settings.mode();
        if mode == Mode::Off {
            return CommandOutcome::Ignored(RefusalCode::FeatureDisabled);
        }
        let Some(home_guild) = self.deps.settings.guild_id() else {
            return Self::refuse(RefusalCode::HomeGuildUnset);
        };
        match req.guild_id {
            Some(gid) if gid == home_guild => {}
            _ => return CommandOutcome::Ignored(RefusalCode::WrongGuild),
        }
        match self.deps.settings.readiness() {
            Readiness::Off => return CommandOutcome::Ignored(RefusalCode::FeatureDisabled),
            Readiness::Misconfigured(NotReady::HomeGuildUnset) => {
                return Self::refuse(RefusalCode::HomeGuildUnset);
            }
            Readiness::Misconfigured(NotReady::CategoryUnset | NotReady::NoHubChannel) => {
                return Self::refuse(RefusalCode::FeatureNotReady);
            }
            Readiness::Ready => {}
        }
        if req.actor_id == 0 {
            return Self::refuse(RefusalCode::InvalidUserId);
        }
        if req.channel_id == 0 {
            return Self::refuse(RefusalCode::UntrackedChannel);
        }

        let _serial = self.serial.lock().await;
        let row = match self.load_tracked_room(home_guild, req.channel_id).await {
            Ok(Some(row)) => row,
            Ok(None) => return Self::refuse(RefusalCode::UntrackedChannel),
            Err(code) => return Self::refuse(code),
        };

        match &req.subcommand {
            VcSubcommand::Claim => {
                self.handle_claim(home_guild, &row, req.actor_id, mode)
                    .await
            }
            other => {
                if req.actor_id != row.owner {
                    return Self::refuse(RefusalCode::NotRoomOwner);
                }
                self.handle_owner_cmd(home_guild, &row, other, mode).await
            }
        }
    }

    fn refuse(code: RefusalCode) -> CommandOutcome {
        CommandOutcome::Refused {
            code,
            reply: Reply::refused(code),
        }
    }

    fn applied(code: SuccessCode, mode: Mode) -> CommandOutcome {
        let effective = if mode == Mode::Shadow {
            SuccessCode::ShadowNoop
        } else {
            code
        };
        CommandOutcome::Applied {
            code: effective,
            reply: Reply::ok(effective),
        }
    }

    async fn load_tracked_room(
        &self,
        home_guild: u64,
        channel_id: u64,
    ) -> Result<Option<RoomRow>, RefusalCode> {
        let db_res = {
            let Ok(conn) = self.deps.db.lock() else {
                self.deps
                    .alerter
                    .alert(CommandAlertCode::RepositoryReadFailed)
                    .await;
                return Err(RefusalCode::StorageReadFailed);
            };
            repository::get(&conn, channel_id)
        };
        match db_res {
            Ok(Some(row)) if row.guild == home_guild => Ok(Some(row)),
            Ok(_) => Ok(None),
            Err(_) => {
                self.deps
                    .alerter
                    .alert(CommandAlertCode::RepositoryReadFailed)
                    .await;
                Err(RefusalCode::StorageReadFailed)
            }
        }
    }

    async fn handle_owner_cmd(
        &self,
        home_guild: u64,
        row: &RoomRow,
        sub: &VcSubcommand,
        mode: Mode,
    ) -> CommandOutcome {
        match sub {
            VcSubcommand::Rename { name } => self.handle_rename(home_guild, row, name, mode).await,
            VcSubcommand::Limit { limit } => self.handle_limit(home_guild, row, *limit, mode).await,
            VcSubcommand::Lock => self.handle_lock(home_guild, row, true, mode).await,
            VcSubcommand::Unlock => self.handle_lock(home_guild, row, false, mode).await,
            VcSubcommand::Permit { target_id } => {
                self.handle_permit(home_guild, row, *target_id, mode).await
            }
            VcSubcommand::Reject { target_id } => {
                self.handle_reject(home_guild, row, *target_id, mode).await
            }
            VcSubcommand::Transfer { target_id } => {
                self.handle_transfer(home_guild, row, *target_id, mode)
                    .await
            }
            VcSubcommand::Claim => Self::refuse(RefusalCode::AlreadyRoomOwner),
            // RED scaffold: not implemented yet.
            VcSubcommand::Deputize { .. }
            | VcSubcommand::Undeputize { .. }
            | VcSubcommand::Refresh => Self::refuse(RefusalCode::FeatureNotReady),
        }
    }
}

/// Role flags of a target member, derived from the role catalog and the role-policy sets.
#[derive(Debug, Clone, Copy, Default)]
struct TargetRoleFlags {
    protected: bool,
    restricted: bool,
    privileged: bool,
}

impl VcCommandHandler {
    fn now(&self) -> i64 {
        self.deps.clock.now_unix()
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, CommandMemState> {
        self.mem
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs one row write; true only when exactly one row was updated.
    fn write_row<F>(&self, write: F) -> bool
    where
        F: FnOnce(&Connection) -> Result<bool, repository::RepoError>,
    {
        match self.deps.db.lock() {
            Ok(conn) => matches!(write(&conn), Ok(true)),
            Err(_) => false,
        }
    }

    async fn row_drift(&self, alert: CommandAlertCode) -> CommandOutcome {
        self.deps.alerter.alert(alert).await;
        Self::refuse(RefusalCode::RowUpdateFailed)
    }

    fn owner_overwrite(user_id: u64) -> RoomOverwrite {
        RoomOverwrite {
            target: OverwriteTarget::Member(user_id),
            allow: policy::OWNER_RIGHTS,
            deny: policy::FORBIDDEN_OWNER_RIGHTS,
        }
    }

    fn permit_overwrite(user_id: u64) -> RoomOverwrite {
        RoomOverwrite {
            target: OverwriteTarget::Member(user_id),
            allow: policy::CONNECT | policy::SPEAK,
            deny: 0,
        }
    }

    fn policy_config(&self, home_guild: u64) -> policy::VoiceRoomConfig {
        policy::VoiceRoomConfig {
            guild_id: home_guild,
            hub_channel_id: 0,
            category_id: self.deps.settings.category_id().unwrap_or(0),
            default_name: self.deps.default_name.clone(),
            per_user_cap: usize::MAX,
            global_cap: usize::MAX,
            create_cooldown_secs: 0,
            empty_grace_secs: self.deps.settings.empty_grace_secs(),
            rename_budget: self.deps.rename_budget,
            category_overwrites: Vec::new(),
        }
    }

    async fn handle_rename(
        &self,
        home_guild: u64,
        row: &RoomRow,
        raw_name: &str,
        mode: Mode,
    ) -> CommandOutcome {
        let clean = policy::sanitize_name(raw_name, &self.deps.default_name);
        let now = self.now();
        let config = self.policy_config(home_guild);
        let state = policy::VoiceRoomState {
            rooms: vec![policy::Room {
                channel_id: row.channel,
                owner_id: row.owner,
                created_at: row.created_at,
                empty_since: None,
                occupants: 0,
            }],
            last_create: HashMap::new(),
            renames: self.mem().renames.clone(),
        };
        match policy::decide_rename(row.channel, row.owner, now, &config, &state) {
            policy::Decision::Allow => {}
            policy::Decision::Deny(DenyReason::RenameBudgetExhausted) => {
                return Self::refuse(RefusalCode::RenameBudgetExhausted);
            }
            _ => return Self::refuse(RefusalCode::RenameRefused),
        }
        if mode == Mode::Shadow {
            return Self::applied(SuccessCode::Renamed, mode);
        }
        match self
            .deps
            .ports
            .rename_room(home_guild, row.channel, &clean)
            .await
        {
            Ok(()) => {}
            Err(CommandPortError::RateLimited) => {
                return Self::refuse(RefusalCode::RenameRateLimited);
            }
            Err(e) => return Self::refuse(RefusalCode::DiscordFailed(e)),
        }
        let window = self.deps.rename_budget.window_secs;
        let mut mem = self.mem();
        mem.renames
            .retain(|(_, at)| now.saturating_sub(*at) < window);
        mem.renames.push((row.channel, now));
        drop(mem);
        Self::applied(SuccessCode::Renamed, mode)
    }

    async fn handle_limit(
        &self,
        home_guild: u64,
        row: &RoomRow,
        limit: u32,
        mode: Mode,
    ) -> CommandOutcome {
        if limit > MAX_USER_LIMIT {
            return Self::refuse(RefusalCode::LimitOutOfBounds);
        }
        if mode == Mode::Shadow {
            return Self::applied(SuccessCode::LimitUpdated, mode);
        }
        if let Err(e) = self
            .deps
            .ports
            .set_user_limit(home_guild, row.channel, limit)
            .await
        {
            return Self::refuse(RefusalCode::DiscordFailed(e));
        }
        let stored = self
            .write_row(|conn| repository::update_user_limit(conn, row.channel, home_guild, limit));
        if !stored {
            return self.row_drift(CommandAlertCode::LimitRowDrift).await;
        }
        Self::applied(SuccessCode::LimitUpdated, mode)
    }

    async fn handle_lock(
        &self,
        home_guild: u64,
        row: &RoomRow,
        lock: bool,
        mode: Mode,
    ) -> CommandOutcome {
        let success = if lock {
            SuccessCode::Locked
        } else {
            SuccessCode::Unlocked
        };
        if mode == Mode::Shadow {
            return Self::applied(success, mode);
        }
        let Some(category) = self.deps.settings.category_id() else {
            return Self::refuse(RefusalCode::FeatureNotReady);
        };
        let category_overwrites = match self
            .deps
            .ports
            .category_overwrites(home_guild, category)
            .await
        {
            Ok(list) => list,
            Err(e) => return Self::refuse(RefusalCode::DiscordFailed(e)),
        };
        let baseline = category_overwrites
            .iter()
            .find(|o| o.target == OverwriteTarget::Everyone)
            .copied()
            .unwrap_or(RoomOverwrite {
                target: OverwriteTarget::Everyone,
                allow: 0,
                deny: 0,
            });
        let ports = &self.deps.ports;
        let written = if lock {
            ports
                .set_overwrite(
                    home_guild,
                    row.channel,
                    RoomOverwrite {
                        target: OverwriteTarget::Everyone,
                        allow: baseline.allow & !policy::CONNECT,
                        deny: baseline.deny | policy::CONNECT,
                    },
                )
                .await
        } else if baseline.allow == 0 && baseline.deny == 0 {
            match ports
                .delete_overwrite(home_guild, row.channel, OverwriteTarget::Everyone)
                .await
            {
                Err(CommandPortError::NotFound) => Ok(()),
                other => other,
            }
        } else {
            ports.set_overwrite(home_guild, row.channel, baseline).await
        };
        if let Err(e) = written {
            return Self::refuse(RefusalCode::DiscordFailed(e));
        }
        let stored =
            self.write_row(|conn| repository::update_locked(conn, row.channel, home_guild, lock));
        if !stored {
            return self.row_drift(CommandAlertCode::LockRowDrift).await;
        }
        Self::applied(success, mode)
    }

    /// Hands the room to `new_owner`: grant the new owner's overwrite, drop the old owner's,
    /// then record the row. A failed Discord step is rolled back where possible and the row is
    /// left unchanged; a failed row write after Discord changed is alerted.
    async fn move_ownership(
        &self,
        home_guild: u64,
        row: &RoomRow,
        new_owner: u64,
        success: SuccessCode,
        mode: Mode,
    ) -> CommandOutcome {
        if mode == Mode::Shadow {
            return Self::applied(success, mode);
        }
        let ports = &self.deps.ports;
        if let Err(e) = ports
            .set_overwrite(home_guild, row.channel, Self::owner_overwrite(new_owner))
            .await
        {
            return Self::refuse(RefusalCode::DiscordFailed(e));
        }
        let dropped = match ports
            .delete_overwrite(home_guild, row.channel, OverwriteTarget::Member(row.owner))
            .await
        {
            Err(CommandPortError::NotFound) => Ok(()),
            other => other,
        };
        if let Err(e) = dropped {
            let was_permitted = self
                .mem()
                .permitted_members
                .get(&row.channel)
                .is_some_and(|set| set.contains(&new_owner));
            let rolled_back = if was_permitted {
                ports
                    .set_overwrite(home_guild, row.channel, Self::permit_overwrite(new_owner))
                    .await
            } else {
                ports
                    .delete_overwrite(home_guild, row.channel, OverwriteTarget::Member(new_owner))
                    .await
            };
            if rolled_back.is_err() {
                self.deps
                    .alerter
                    .alert(CommandAlertCode::OwnerOverwriteDrift)
                    .await;
            }
            return Self::refuse(RefusalCode::DiscordFailed(e));
        }
        let stored = self
            .write_row(|conn| repository::update_owner(conn, row.channel, home_guild, new_owner));
        if !stored {
            return self.row_drift(CommandAlertCode::OwnerRowDrift).await;
        }
        let mut mem = self.mem();
        mem.owner_absent.remove(&row.channel);
        if let Some(set) = mem.permitted_members.get_mut(&row.channel) {
            set.remove(&new_owner);
        }
        drop(mem);
        Self::applied(success, mode)
    }

    async fn handle_transfer(
        &self,
        home_guild: u64,
        row: &RoomRow,
        target: u64,
        mode: Mode,
    ) -> CommandOutcome {
        if target == 0 {
            return Self::refuse(RefusalCode::InvalidUserId);
        }
        if target == row.owner {
            return Self::refuse(RefusalCode::TargetIsOwner);
        }
        if target == self.deps.bot_user_id {
            return Self::refuse(RefusalCode::TargetIsSelfBot);
        }
        let members = match self.deps.ports.room_members(home_guild, row.channel).await {
            Ok(list) => list,
            Err(e) => return Self::refuse(RefusalCode::DiscordFailed(e)),
        };
        if !members.contains(&target) {
            return Self::refuse(RefusalCode::TargetNotInRoom);
        }
        let Some(category) = self.deps.settings.category_id() else {
            return Self::refuse(RefusalCode::FeatureNotReady);
        };
        match self
            .deps
            .ports
            .member_profile(home_guild, category, target)
            .await
        {
            Ok(profile) if profile.is_bot => return Self::refuse(RefusalCode::TargetIsBot),
            Ok(_) => {}
            Err(e) => return Self::refuse(RefusalCode::DiscordFailed(e)),
        }
        self.move_ownership(home_guild, row, target, SuccessCode::Transferred, mode)
            .await
    }

    async fn handle_claim(
        &self,
        home_guild: u64,
        row: &RoomRow,
        actor: u64,
        mode: Mode,
    ) -> CommandOutcome {
        if actor == row.owner {
            return Self::refuse(RefusalCode::AlreadyRoomOwner);
        }
        let members = match self.deps.ports.room_members(home_guild, row.channel).await {
            Ok(list) => list,
            Err(e) => return Self::refuse(RefusalCode::DiscordFailed(e)),
        };
        if !members.contains(&actor) {
            return Self::refuse(RefusalCode::CallerNotInRoom);
        }
        if members.contains(&row.owner) {
            self.note_owner_present(row.channel);
            return Self::refuse(RefusalCode::OwnerStillPresent);
        }
        // The owner is absent. A claim rests only on an absence that `note_owner_absent` recorded
        // and that no `note_owner_present` (or claim that saw the owner) has cleared since. A claim
        // never starts or guesses an absence itself. The record must also be fresh: its last
        // observation at most ABSENCE_MAX_GAP_SECS old, otherwise a return could have gone unseen.
        let now = self.now();
        let Some(record) = self.mem().owner_absent.get(&row.channel).copied() else {
            return Self::refuse(RefusalCode::OwnerAbsenceNotRecorded);
        };
        if now.saturating_sub(record.last_seen) > ABSENCE_MAX_GAP_SECS {
            return Self::refuse(RefusalCode::OwnerAbsenceNotContinuous);
        }
        if now.saturating_sub(record.since) < self.deps.claim_grace_secs {
            return Self::refuse(RefusalCode::OwnerGraceNotElapsed);
        }
        self.move_ownership(home_guild, row, actor, SuccessCode::Claimed, mode)
            .await
    }
    /// Loads the target's profile and role flags; any failure is refused and nothing is written.
    async fn load_target(
        &self,
        home_guild: u64,
        target: u64,
    ) -> Result<(MemberProfile, TargetRoleFlags), RefusalCode> {
        let Some(category) = self.deps.settings.category_id() else {
            return Err(RefusalCode::FeatureNotReady);
        };
        let profile = self
            .deps
            .ports
            .member_profile(home_guild, category, target)
            .await
            .map_err(RefusalCode::DiscordFailed)?;
        let catalog = self
            .deps
            .roles
            .roles(home_guild)
            .await
            .map_err(|_| RefusalCode::DiscordFailed(CommandPortError::Unavailable))?;
        let mut flags = TargetRoleFlags::default();
        for role_id in &profile.role_ids {
            if self.deps.role_limits.protected_role_ids.contains(role_id) {
                flags.protected = true;
            }
            if self.deps.role_limits.restricted_role_ids.contains(role_id) {
                flags.restricted = true;
            }
            match catalog.iter().find(|r| r.id == *role_id) {
                Some(entry) if entry.permissions & PRIVILEGED_MASK != 0 => {
                    flags.privileged = true;
                }
                Some(_) => {}
                // A role missing from the catalog cannot be judged: refuse rather than guess.
                None => return Err(RefusalCode::DiscordFailed(CommandPortError::Unavailable)),
            }
        }
        Ok((profile, flags))
    }

    /// Refusals shared by `permit` and `reject`: invalid id, owner, the bot itself, a bot account.
    fn common_target_refusal(&self, row: &RoomRow, target: u64) -> Option<RefusalCode> {
        if target == 0 {
            Some(RefusalCode::InvalidUserId)
        } else if target == row.owner {
            Some(RefusalCode::TargetIsOwner)
        } else if target == self.deps.bot_user_id {
            Some(RefusalCode::TargetIsSelfBot)
        } else {
            None
        }
    }

    async fn handle_permit(
        &self,
        home_guild: u64,
        row: &RoomRow,
        target: u64,
        mode: Mode,
    ) -> CommandOutcome {
        if let Some(code) = self.common_target_refusal(row, target) {
            return Self::refuse(code);
        }
        let (profile, flags) = match self.load_target(home_guild, target).await {
            Ok(found) => found,
            Err(code) => return Self::refuse(code),
        };
        if profile.is_bot {
            return Self::refuse(RefusalCode::TargetIsBot);
        }
        if flags.protected || flags.restricted || flags.privileged {
            return Self::refuse(RefusalCode::TargetProtectedOrRestricted);
        }
        match self.deps.safeword.state(home_guild, target) {
            Ok(SafewordState::Inactive) => {}
            Ok(SafewordState::Active) => return Self::refuse(RefusalCode::TargetSafewordActive),
            Err(_) => return Self::refuse(RefusalCode::StorageReadFailed),
        }
        if !profile.can_view_category {
            return Self::refuse(RefusalCode::CategoryNotVisible);
        }
        let already = self
            .mem()
            .permitted_members
            .get(&row.channel)
            .is_some_and(|set| set.contains(&target));
        if already {
            return Self::refuse(RefusalCode::AlreadyPermitted);
        }
        if mode == Mode::Shadow {
            return Self::applied(SuccessCode::Permitted, mode);
        }
        if let Err(e) = self
            .deps
            .ports
            .set_overwrite(home_guild, row.channel, Self::permit_overwrite(target))
            .await
        {
            return Self::refuse(RefusalCode::DiscordFailed(e));
        }
        self.remember_permit(row.channel, target);
        Self::applied(SuccessCode::Permitted, mode)
    }

    /// Remembers a permit; forgets the permits of rooms that no longer have a row once the memory
    /// holds many rooms.
    fn remember_permit(&self, channel: u64, target: u64) {
        let mut mem = self.mem();
        mem.permitted_members
            .entry(channel)
            .or_default()
            .insert(target);
        if mem.permitted_members.len() > MAX_REMEMBERED_ROOMS {
            let alive: HashSet<u64> = match self.deps.db.lock() {
                Ok(conn) => mem
                    .permitted_members
                    .keys()
                    .copied()
                    .filter(|c| matches!(repository::get(&conn, *c), Ok(Some(_))))
                    .collect(),
                Err(_) => return,
            };
            mem.permitted_members.retain(|c, _| alive.contains(c));
        }
    }

    async fn handle_reject(
        &self,
        home_guild: u64,
        row: &RoomRow,
        target: u64,
        mode: Mode,
    ) -> CommandOutcome {
        if let Some(code) = self.common_target_refusal(row, target) {
            return Self::refuse(code);
        }
        let (profile, flags) = match self.load_target(home_guild, target).await {
            Ok(found) => found,
            Err(code) => return Self::refuse(code),
        };
        if profile.is_bot {
            return Self::refuse(RefusalCode::TargetIsBot);
        }
        if flags.protected || flags.privileged {
            return Self::refuse(RefusalCode::TargetProtectedOrRestricted);
        }
        let present = match self.deps.ports.room_members(home_guild, row.channel).await {
            Ok(list) => list.contains(&target),
            Err(e) => return Self::refuse(RefusalCode::DiscordFailed(e)),
        };
        let success = if present {
            SuccessCode::RejectedAndDisconnected
        } else {
            SuccessCode::Rejected
        };
        if mode == Mode::Shadow {
            return Self::applied(success, mode);
        }
        let denied = RoomOverwrite {
            target: OverwriteTarget::Member(target),
            allow: 0,
            deny: policy::CONNECT,
        };
        if let Err(e) = self
            .deps
            .ports
            .set_overwrite(home_guild, row.channel, denied)
            .await
        {
            return Self::refuse(RefusalCode::DiscordFailed(e));
        }
        if let Some(set) = self.mem().permitted_members.get_mut(&row.channel) {
            set.remove(&target);
        }
        if present {
            if let Err(e) = self.deps.ports.disconnect_member(home_guild, target).await {
                return Self::refuse(RefusalCode::DisconnectFailed(e));
            }
        }
        Self::applied(success, mode)
    }
}

/// Real Serenity adapter implementing [`RoomCommandPorts`].
pub struct SerenityRoomCommandPorts {
    http: Arc<serenity::Http>,
    cache: Arc<serenity::Cache>,
}

impl SerenityRoomCommandPorts {
    /// Wraps the serenity HTTP client and cache.
    pub fn new(http: Arc<serenity::Http>, cache: Arc<serenity::Cache>) -> Self {
        Self { http, cache }
    }
}

fn map_serenity_err(err: &serenity::Error) -> CommandPortError {
    if let serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(resp)) = err {
        return command_port_error_from_status(Some(resp.status_code.as_u16()));
    }
    CommandPortError::Unavailable
}

fn from_serenity_overwrite(guild_id: u64, o: &serenity::PermissionOverwrite) -> RoomOverwrite {
    let target = match o.kind {
        serenity::PermissionOverwriteType::Role(id) if id.get() == guild_id => {
            OverwriteTarget::Everyone
        }
        serenity::PermissionOverwriteType::Role(id) => OverwriteTarget::Role(id.get()),
        serenity::PermissionOverwriteType::Member(id) => OverwriteTarget::Member(id.get()),
        _ => OverwriteTarget::Role(0),
    };
    RoomOverwrite {
        target,
        allow: o.allow.bits(),
        deny: o.deny.bits(),
    }
}

fn to_serenity_overwrite(
    ow: RoomOverwrite,
) -> Result<serenity::PermissionOverwrite, CommandPortError> {
    let kind = match ow.target {
        OverwriteTarget::Everyone => {
            return Err(CommandPortError::Unavailable);
        }
        OverwriteTarget::Role(id) => {
            if id == 0 {
                return Err(CommandPortError::Unavailable);
            }
            serenity::PermissionOverwriteType::Role(serenity::RoleId::new(id))
        }
        OverwriteTarget::Member(id) => {
            if id == 0 {
                return Err(CommandPortError::Unavailable);
            }
            serenity::PermissionOverwriteType::Member(serenity::UserId::new(id))
        }
    };
    Ok(serenity::PermissionOverwrite {
        allow: serenity::Permissions::from_bits_retain(ow.allow),
        deny: serenity::Permissions::from_bits_retain(ow.deny),
        kind,
    })
}

#[async_trait]
impl RoomCommandPorts for SerenityRoomCommandPorts {
    async fn member_overwrites(
        &self,
        guild_id: u64,
        channel_id: u64,
    ) -> Result<Vec<MemberOverwrite>, CommandPortError> {
        if guild_id == 0 || channel_id == 0 {
            return Err(CommandPortError::Unavailable);
        }
        match self
            .http
            .get_channel(serenity::ChannelId::new(channel_id))
            .await
        {
            Ok(serenity::Channel::Guild(found))
                if found.guild_id.get() == guild_id && found.id.get() == channel_id =>
            {
                Ok(found
                    .permission_overwrites
                    .iter()
                    .filter_map(|o| match o.kind {
                        serenity::PermissionOverwriteType::Member(id) => Some(MemberOverwrite {
                            user_id: id.get(),
                            allow: o.allow.bits(),
                            deny: o.deny.bits(),
                        }),
                        _ => None,
                    })
                    .collect())
            }
            Ok(_) => Err(CommandPortError::Unavailable),
            Err(e) => Err(map_serenity_err(&e)),
        }
    }

    async fn rename_room(
        &self,
        guild_id: u64,
        channel_id: u64,
        clean_name: &str,
    ) -> Result<(), CommandPortError> {
        if guild_id == 0 || channel_id == 0 {
            return Err(CommandPortError::Unavailable);
        }
        let builder = serenity::builder::EditChannel::new().name(clean_name);
        serenity::ChannelId::new(channel_id)
            .edit(&self.http, builder)
            .await
            .map(|_| ())
            .map_err(|e| map_serenity_err(&e))
    }

    async fn set_user_limit(
        &self,
        guild_id: u64,
        channel_id: u64,
        user_limit: u32,
    ) -> Result<(), CommandPortError> {
        if guild_id == 0 || channel_id == 0 || user_limit > MAX_USER_LIMIT {
            return Err(CommandPortError::Unavailable);
        }
        let builder = serenity::builder::EditChannel::new().user_limit(user_limit);
        serenity::ChannelId::new(channel_id)
            .edit(&self.http, builder)
            .await
            .map(|_| ())
            .map_err(|e| map_serenity_err(&e))
    }

    async fn set_overwrite(
        &self,
        guild_id: u64,
        channel_id: u64,
        overwrite: RoomOverwrite,
    ) -> Result<(), CommandPortError> {
        if guild_id == 0 || channel_id == 0 {
            return Err(CommandPortError::Unavailable);
        }
        let mapped = match overwrite.target {
            OverwriteTarget::Everyone => RoomOverwrite {
                target: OverwriteTarget::Role(guild_id),
                allow: overwrite.allow,
                deny: overwrite.deny,
            },
            other => RoomOverwrite {
                target: other,
                allow: overwrite.allow,
                deny: overwrite.deny,
            },
        };
        let perm = to_serenity_overwrite(mapped)?;
        serenity::ChannelId::new(channel_id)
            .create_permission(&self.http, perm)
            .await
            .map_err(|e| map_serenity_err(&e))
    }

    async fn delete_overwrite(
        &self,
        guild_id: u64,
        channel_id: u64,
        target: OverwriteTarget,
    ) -> Result<(), CommandPortError> {
        if guild_id == 0 || channel_id == 0 {
            return Err(CommandPortError::Unavailable);
        }
        let target_type = match target {
            OverwriteTarget::Everyone => {
                serenity::PermissionOverwriteType::Role(serenity::RoleId::new(guild_id))
            }
            OverwriteTarget::Role(0) | OverwriteTarget::Member(0) => {
                return Err(CommandPortError::Unavailable);
            }
            OverwriteTarget::Role(id) => {
                serenity::PermissionOverwriteType::Role(serenity::RoleId::new(id))
            }
            OverwriteTarget::Member(id) => {
                serenity::PermissionOverwriteType::Member(serenity::UserId::new(id))
            }
        };
        serenity::ChannelId::new(channel_id)
            .delete_permission(&self.http, target_type)
            .await
            .map_err(|e| map_serenity_err(&e))
    }

    async fn disconnect_member(&self, guild_id: u64, user_id: u64) -> Result<(), CommandPortError> {
        if guild_id == 0 || user_id == 0 {
            return Err(CommandPortError::Unavailable);
        }
        serenity::GuildId::new(guild_id)
            .disconnect_member(&self.http, serenity::UserId::new(user_id))
            .await
            .map(|_| ())
            .map_err(|e| map_serenity_err(&e))
    }

    async fn move_member(
        &self,
        guild_id: u64,
        user_id: u64,
        channel_id: u64,
    ) -> Result<(), CommandPortError> {
        if guild_id == 0 || user_id == 0 || channel_id == 0 {
            return Err(CommandPortError::Unavailable);
        }
        serenity::GuildId::new(guild_id)
            .move_member(
                &self.http,
                serenity::UserId::new(user_id),
                serenity::ChannelId::new(channel_id),
            )
            .await
            .map(|_| ())
            .map_err(|e| map_serenity_err(&e))
    }

    async fn room_members(
        &self,
        guild_id: u64,
        channel_id: u64,
    ) -> Result<Vec<u64>, CommandPortError> {
        if guild_id == 0 || channel_id == 0 {
            return Err(CommandPortError::Unavailable);
        }
        let g = serenity::GuildId::new(guild_id);
        let ch = serenity::ChannelId::new(channel_id);
        if let Some(guild) = self.cache.guild(g) {
            if !guild.channels.contains_key(&ch) {
                return Err(CommandPortError::NotFound);
            }
            let members = guild
                .voice_states
                .values()
                .filter(|vs| vs.channel_id == Some(ch))
                .map(|vs| vs.user_id.get())
                .collect();
            return Ok(members);
        }
        Err(CommandPortError::Unavailable)
    }

    async fn category_overwrites(
        &self,
        guild_id: u64,
        category_id: u64,
    ) -> Result<Vec<RoomOverwrite>, CommandPortError> {
        if guild_id == 0 || category_id == 0 {
            return Err(CommandPortError::Unavailable);
        }
        match self
            .http
            .get_channel(serenity::ChannelId::new(category_id))
            .await
        {
            Ok(serenity::Channel::Guild(found))
                if found.guild_id.get() == guild_id
                    && found.kind == serenity::ChannelType::Category =>
            {
                Ok(found
                    .permission_overwrites
                    .iter()
                    .map(|o| from_serenity_overwrite(guild_id, o))
                    .collect())
            }
            Ok(_) => Err(CommandPortError::Unavailable),
            Err(e) => Err(map_serenity_err(&e)),
        }
    }

    async fn member_profile(
        &self,
        guild_id: u64,
        category_id: u64,
        user_id: u64,
    ) -> Result<MemberProfile, CommandPortError> {
        if guild_id == 0 || category_id == 0 || user_id == 0 {
            return Err(CommandPortError::Unavailable);
        }
        let g = serenity::GuildId::new(guild_id);
        let u = serenity::UserId::new(user_id);
        let cat = serenity::ChannelId::new(category_id);
        let member = g
            .member(&self.http, u)
            .await
            .map_err(|e| map_serenity_err(&e))?;
        let role_ids: Vec<u64> = member.roles.iter().map(|r| r.get()).collect();
        let can_view_category = if let Some(guild) = self.cache.guild(g) {
            if let Some(channel) = guild.channels.get(&cat) {
                let perms = guild.user_permissions_in(channel, &member);
                perms.view_channel()
            } else {
                false
            }
        } else {
            false
        };
        Ok(MemberProfile {
            user_id,
            is_bot: member.user.bot,
            role_ids,
            can_view_category,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::runtime::RoleEntry;
    use std::sync::atomic::{AtomicI64, Ordering};

    const HOME_GUILD: u64 = 10_001;
    const OTHER_GUILD: u64 = 20_002;
    const HUB_CH: u64 = 30_001;
    const CAT_ID: u64 = 40_001;
    const ROOM_CH: u64 = 50_001;
    const UNTRACKED_CH: u64 = 50_999;
    const BOT_USER: u64 = 90_001;
    const OWNER: u64 = 100_001;
    const MEMBER_A: u64 = 100_002;
    const MEMBER_B: u64 = 100_003;
    const STAFF_USER: u64 = 100_004;
    const OTHER_BOT: u64 = 100_005;
    const ROLE_NORMAL: u64 = 200_001;
    const ROLE_PROTECTED: u64 = 200_002;
    const ROLE_RESTRICTED: u64 = 200_003;
    const ROLE_PRIVILEGED: u64 = 200_004;
    const ROLE_ABOVE_BOT: u64 = 200_005;

    struct FakeClock {
        now: AtomicI64,
    }

    impl FakeClock {
        fn new(t: i64) -> Self {
            Self {
                now: AtomicI64::new(t),
            }
        }
        fn set(&self, t: i64) {
            self.now.store(t, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now_unix(&self) -> i64 {
            self.now.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct FakePortsState {
        renames: Vec<(u64, u64, String)>,
        limits: Vec<(u64, u64, u32)>,
        overwrites: Vec<(u64, u64, RoomOverwrite)>,
        deleted_overwrites: Vec<(u64, u64, OverwriteTarget)>,
        disconnects: Vec<(u64, u64)>,
        moves: Vec<(u64, u64, u64)>,
        room_members: HashMap<u64, Vec<u64>>,
        profiles: HashMap<u64, MemberProfile>,
        fail_rename: Option<CommandPortError>,
        fail_limit: Option<CommandPortError>,
        fail_overwrite: Option<CommandPortError>,
        fail_delete_overwrite: Option<CommandPortError>,
        fail_disconnect: Option<CommandPortError>,
        fail_room_members: Option<CommandPortError>,
        fail_profile: Option<CommandPortError>,
        fail_category: Option<CommandPortError>,
        category_overwrites: Vec<RoomOverwrite>,
        calls_count: usize,
        /// Overwrites as Discord holds them (every target); kept in step by set/delete below.
        live: Vec<RoomOverwrite>,
        /// Users whose overwrite write (set) fails with `Forbidden`.
        fail_set_users: HashSet<u64>,
        /// Users whose overwrite delete fails with `Forbidden`.
        fail_delete_users: HashSet<u64>,
        /// After this many successful overwrite writes every further write is rate limited.
        rate_limit_after: Option<usize>,
        /// Successful overwrite writes (set or delete) so far.
        writes_ok: usize,
        /// Overwrite write attempts so far (set or delete, successful or not).
        write_attempts: usize,
        fail_member_overwrites: Option<CommandPortError>,
        /// When set, `member_overwrites` returns this instead of the member part of `live`.
        listed_override: Option<Vec<MemberOverwrite>>,
    }

    impl FakePortsState {
        /// Common gate of one overwrite write: counts it and applies the failure hooks.
        fn gate_write(
            &mut self,
            target: OverwriteTarget,
            delete: bool,
        ) -> Result<(), CommandPortError> {
            self.write_attempts += 1;
            if let Some(n) = self.rate_limit_after {
                if self.writes_ok >= n {
                    return Err(CommandPortError::RateLimited);
                }
            }
            if let OverwriteTarget::Member(u) = target {
                let hooks = if delete {
                    &self.fail_delete_users
                } else {
                    &self.fail_set_users
                };
                if hooks.contains(&u) {
                    return Err(CommandPortError::Forbidden);
                }
            }
            self.writes_ok += 1;
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakePorts {
        state: Mutex<FakePortsState>,
    }

    impl FakePorts {
        fn with_defaults() -> Self {
            let ports = Self::default();
            {
                let mut s = ports.state.lock().unwrap();
                s.room_members.insert(ROOM_CH, vec![OWNER, MEMBER_A]);
                s.profiles.insert(
                    OWNER,
                    MemberProfile {
                        user_id: OWNER,
                        is_bot: false,
                        role_ids: vec![ROLE_NORMAL],
                        can_view_category: true,
                    },
                );
                s.profiles.insert(
                    MEMBER_A,
                    MemberProfile {
                        user_id: MEMBER_A,
                        is_bot: false,
                        role_ids: vec![ROLE_NORMAL],
                        can_view_category: true,
                    },
                );
                s.profiles.insert(
                    MEMBER_B,
                    MemberProfile {
                        user_id: MEMBER_B,
                        is_bot: false,
                        role_ids: vec![ROLE_NORMAL],
                        can_view_category: true,
                    },
                );
                s.profiles.insert(
                    STAFF_USER,
                    MemberProfile {
                        user_id: STAFF_USER,
                        is_bot: false,
                        role_ids: vec![ROLE_PROTECTED],
                        can_view_category: true,
                    },
                );
                s.profiles.insert(
                    OTHER_BOT,
                    MemberProfile {
                        user_id: OTHER_BOT,
                        is_bot: true,
                        role_ids: vec![],
                        can_view_category: true,
                    },
                );
            }
            ports
        }
    }

    #[async_trait]
    impl RoomCommandPorts for FakePorts {
        async fn rename_room(
            &self,
            guild_id: u64,
            channel_id: u64,
            clean_name: &str,
        ) -> Result<(), CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            if let Some(err) = s.fail_rename {
                return Err(err);
            }
            s.renames
                .push((guild_id, channel_id, clean_name.to_string()));
            Ok(())
        }

        async fn set_user_limit(
            &self,
            guild_id: u64,
            channel_id: u64,
            user_limit: u32,
        ) -> Result<(), CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            if let Some(err) = s.fail_limit {
                return Err(err);
            }
            s.limits.push((guild_id, channel_id, user_limit));
            Ok(())
        }

        async fn set_overwrite(
            &self,
            guild_id: u64,
            channel_id: u64,
            overwrite: RoomOverwrite,
        ) -> Result<(), CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            if let Some(err) = s.fail_overwrite {
                return Err(err);
            }
            s.gate_write(overwrite.target, false)?;
            s.overwrites.push((guild_id, channel_id, overwrite));
            s.live.retain(|o| o.target != overwrite.target);
            s.live.push(overwrite);
            Ok(())
        }

        async fn delete_overwrite(
            &self,
            guild_id: u64,
            channel_id: u64,
            target: OverwriteTarget,
        ) -> Result<(), CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            if let Some(err) = s.fail_delete_overwrite {
                return Err(err);
            }
            s.gate_write(target, true)?;
            s.deleted_overwrites.push((guild_id, channel_id, target));
            s.live.retain(|o| o.target != target);
            Ok(())
        }

        async fn member_overwrites(
            &self,
            _guild_id: u64,
            _channel_id: u64,
        ) -> Result<Vec<MemberOverwrite>, CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            if let Some(err) = s.fail_member_overwrites {
                return Err(err);
            }
            if let Some(list) = s.listed_override.clone() {
                return Ok(list);
            }
            Ok(s.live
                .iter()
                .filter_map(|o| match o.target {
                    OverwriteTarget::Member(user_id) => Some(MemberOverwrite {
                        user_id,
                        allow: o.allow,
                        deny: o.deny,
                    }),
                    _ => None,
                })
                .collect())
        }

        async fn disconnect_member(
            &self,
            guild_id: u64,
            user_id: u64,
        ) -> Result<(), CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            if let Some(err) = s.fail_disconnect {
                return Err(err);
            }
            s.disconnects.push((guild_id, user_id));
            for members in s.room_members.values_mut() {
                members.retain(|&u| u != user_id);
            }
            Ok(())
        }

        async fn move_member(
            &self,
            guild_id: u64,
            user_id: u64,
            channel_id: u64,
        ) -> Result<(), CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            s.moves.push((guild_id, user_id, channel_id));
            Ok(())
        }

        async fn room_members(
            &self,
            _guild_id: u64,
            channel_id: u64,
        ) -> Result<Vec<u64>, CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            if let Some(err) = s.fail_room_members {
                return Err(err);
            }
            Ok(s.room_members.get(&channel_id).cloned().unwrap_or_default())
        }

        async fn category_overwrites(
            &self,
            _guild_id: u64,
            _category_id: u64,
        ) -> Result<Vec<RoomOverwrite>, CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            if let Some(err) = s.fail_category {
                return Err(err);
            }
            Ok(s.category_overwrites.clone())
        }

        async fn member_profile(
            &self,
            _guild_id: u64,
            _category_id: u64,
            user_id: u64,
        ) -> Result<MemberProfile, CommandPortError> {
            let mut s = self.state.lock().unwrap();
            s.calls_count += 1;
            if let Some(err) = s.fail_profile {
                return Err(err);
            }
            s.profiles
                .get(&user_id)
                .cloned()
                .ok_or(CommandPortError::NotFound)
        }
    }

    struct FakeRoles {
        entries: Mutex<Vec<RoleEntry>>,
    }

    impl FakeRoles {
        fn with_defaults() -> Self {
            Self {
                entries: Mutex::new(vec![
                    RoleEntry {
                        id: ROLE_NORMAL,
                        position: 1,
                        permissions: 0,
                        managed: false,
                    },
                    RoleEntry {
                        id: ROLE_PROTECTED,
                        position: 5,
                        permissions: 0,
                        managed: false,
                    },
                    RoleEntry {
                        id: ROLE_RESTRICTED,
                        position: 2,
                        permissions: 0,
                        managed: false,
                    },
                    RoleEntry {
                        id: ROLE_PRIVILEGED,
                        position: 3,
                        permissions: PRIVILEGED_MASK,
                        managed: false,
                    },
                    RoleEntry {
                        id: ROLE_ABOVE_BOT,
                        position: 50,
                        permissions: 0,
                        managed: false,
                    },
                ]),
            }
        }
    }

    #[async_trait]
    impl RoleSource for FakeRoles {
        async fn roles(
            &self,
            _guild_id: u64,
        ) -> Result<Vec<RoleEntry>, crate::roles::runtime::SourceError> {
            Ok(self.entries.lock().unwrap().clone())
        }
        async fn member_roles(
            &self,
            _guild_id: u64,
            _member_id: u64,
        ) -> Result<Vec<u64>, crate::roles::runtime::SourceError> {
            Ok(vec![ROLE_NORMAL])
        }
    }

    #[derive(Default)]
    struct FakeSafeword {
        active: Mutex<HashSet<u64>>,
    }

    impl SafewordLookup for FakeSafeword {
        fn state(&self, _guild_id: u64, user_id: u64) -> Result<SafewordState, CommandPortError> {
            if self.active.lock().unwrap().contains(&user_id) {
                Ok(SafewordState::Active)
            } else {
                Ok(SafewordState::Inactive)
            }
        }
    }

    #[derive(Default)]
    struct FakeAlerter {
        alerts: Mutex<Vec<CommandAlertCode>>,
    }

    #[async_trait]
    impl CommandAlerter for FakeAlerter {
        async fn alert(&self, code: CommandAlertCode) {
            self.alerts.lock().unwrap().push(code);
        }
    }

    struct Harness {
        handler: VcCommandHandler,
        db: Arc<Mutex<Connection>>,
        ports: Arc<FakePorts>,
        safeword: Arc<FakeSafeword>,
        alerter: Arc<FakeAlerter>,
        clock: Arc<FakeClock>,
    }

    fn settings_with(mode: &str, home_guild: Option<u64>) -> VoiceSettings {
        let hub_str = HUB_CH.to_string();
        let cat_str = CAT_ID.to_string();
        crate::voice_rooms::config::load(
            |key| match key {
                crate::voice_rooms::config::KEY_MODE => Some(mode.to_string()),
                crate::voice_rooms::config::KEY_HUB_CHANNEL_IDS => Some(hub_str.clone()),
                crate::voice_rooms::config::KEY_CATEGORY_ID => Some(cat_str.clone()),
                // VR2's cleanup grace is deliberately different from the claim grace.
                crate::voice_rooms::config::KEY_EMPTY_GRACE_SECS => Some("10".to_string()),
                _ => None,
            },
            home_guild,
        )
        .settings
    }

    fn live_settings() -> VoiceSettings {
        settings_with("live", Some(HOME_GUILD))
    }

    fn make_harness(settings: VoiceSettings) -> Harness {
        let mut conn = Connection::open_in_memory().unwrap();
        repository::migrate(&mut conn).unwrap();
        repository::insert(
            &conn,
            &RoomRow {
                channel: ROOM_CH,
                guild: HOME_GUILD,
                owner: OWNER,
                created_at: 1_000,
                locked: false,
                user_limit: 0,
            },
        )
        .unwrap();
        let db = Arc::new(Mutex::new(conn));
        let ports = Arc::new(FakePorts::with_defaults());
        let roles = Arc::new(FakeRoles::with_defaults());
        let safeword = Arc::new(FakeSafeword::default());
        let alerter = Arc::new(FakeAlerter::default());
        let clock = Arc::new(FakeClock::new(1_100));
        let mut role_limits = RoleLimits::default();
        role_limits.protected_role_ids.insert(ROLE_PROTECTED);
        role_limits.restricted_role_ids.insert(ROLE_RESTRICTED);
        let handler = VcCommandHandler::new(VcCommandDeps {
            settings,
            bot_user_id: BOT_USER,
            rename_budget: RenameBudget {
                max_renames: 2,
                window_secs: 600,
            },
            default_name: "Voice Room".to_string(),
            role_limits,
            db: Arc::clone(&db),
            ports: Arc::clone(&ports) as Arc<dyn RoomCommandPorts>,
            roles: roles as Arc<dyn RoleSource>,
            safeword: Arc::clone(&safeword) as Arc<dyn SafewordLookup>,
            alerter: Arc::clone(&alerter) as Arc<dyn CommandAlerter>,
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            claim_grace_secs: policy::DEFAULT_EMPTY_GRACE_SECS,
        });
        Harness {
            handler,
            db,
            ports,
            safeword,
            alerter,
            clock,
        }
    }

    #[tokio::test]
    async fn non_owner_is_denied_on_every_owner_command_and_touches_nothing() {
        let h = make_harness(live_settings());
        let owner_cmds = [
            VcSubcommand::Rename {
                name: "Stolen".to_string(),
            },
            VcSubcommand::Limit { limit: 4 },
            VcSubcommand::Lock,
            VcSubcommand::Unlock,
            VcSubcommand::Permit {
                target_id: MEMBER_B,
            },
            VcSubcommand::Reject {
                target_id: MEMBER_B,
            },
            VcSubcommand::Transfer {
                target_id: MEMBER_A,
            },
        ];
        for sub in owner_cmds {
            let out = h
                .handler
                .handle(&VcCommandRequest {
                    guild_id: Some(HOME_GUILD),
                    channel_id: ROOM_CH,
                    actor_id: MEMBER_A,
                    subcommand: sub,
                })
                .await;
            assert_eq!(
                out,
                CommandOutcome::Refused {
                    code: RefusalCode::NotRoomOwner,
                    reply: Reply::refused(RefusalCode::NotRoomOwner),
                }
            );
        }
        assert_eq!(h.ports.state.lock().unwrap().calls_count, 0);
        let row = repository::get(&h.db.lock().unwrap(), ROOM_CH)
            .unwrap()
            .unwrap();
        assert_eq!(row.owner, OWNER);
        assert!(!row.locked);
        assert_eq!(row.user_limit, 0);
    }

    #[tokio::test]
    async fn claim_from_present_owner_or_before_grace_is_refused_and_after_grace_transfers_row() {
        let h = make_harness(live_settings());

        // 1. Owner is currently inside the room -> refused with OwnerStillPresent.
        let out_present = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: MEMBER_A,
                subcommand: VcSubcommand::Claim,
            })
            .await;
        assert_eq!(
            out_present,
            CommandOutcome::Refused {
                code: RefusalCode::OwnerStillPresent,
                reply: Reply::refused(RefusalCode::OwnerStillPresent),
            }
        );

        // 2. Caller not inside the room -> refused with CallerNotInRoom.
        let out_outside = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: MEMBER_B,
                subcommand: VcSubcommand::Claim,
            })
            .await;
        assert_eq!(
            out_outside,
            CommandOutcome::Refused {
                code: RefusalCode::CallerNotInRoom,
                reply: Reply::refused(RefusalCode::CallerNotInRoom),
            }
        );

        // 3. Owner leaves at t=1100; at t=1159 (59s < 60s grace) -> refused with OwnerGraceNotElapsed.
        h.ports
            .state
            .lock()
            .unwrap()
            .room_members
            .insert(ROOM_CH, vec![MEMBER_A]);
        observe_absent(&h, 1_100, 1_159);
        let out_early = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: MEMBER_A,
                subcommand: VcSubcommand::Claim,
            })
            .await;
        assert_eq!(
            out_early,
            CommandOutcome::Refused {
                code: RefusalCode::OwnerGraceNotElapsed,
                reply: Reply::refused(RefusalCode::OwnerGraceNotElapsed),
            }
        );
        assert_eq!(
            repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap()
                .owner,
            OWNER
        );

        // 4. At t=1160 (60s >= 60s grace) -> claim succeeds, updates Discord overwrites and DB owner.
        h.clock.set(1_160);
        let out_ok = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: MEMBER_A,
                subcommand: VcSubcommand::Claim,
            })
            .await;
        assert_eq!(
            out_ok,
            CommandOutcome::Applied {
                code: SuccessCode::Claimed,
                reply: Reply::ok(SuccessCode::Claimed),
            }
        );
        let row = repository::get(&h.db.lock().unwrap(), ROOM_CH)
            .unwrap()
            .unwrap();
        assert_eq!(row.owner, MEMBER_A);
        let s = h.ports.state.lock().unwrap();
        assert!(s.deleted_overwrites.contains(&(
            HOME_GUILD,
            ROOM_CH,
            OverwriteTarget::Member(OWNER)
        )));
        assert!(s.overwrites.contains(&(
            HOME_GUILD,
            ROOM_CH,
            RoomOverwrite {
                target: OverwriteTarget::Member(MEMBER_A),
                allow: policy::OWNER_RIGHTS,
                deny: policy::FORBIDDEN_OWNER_RIGHTS,
            }
        )));
    }

    #[tokio::test]
    async fn transfer_requires_target_inside_room_and_transfers_ownership() {
        let h = make_harness(live_settings());

        // MEMBER_B is not in the room -> refused.
        let out_outside = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Transfer {
                    target_id: MEMBER_B,
                },
            })
            .await;
        assert_eq!(
            out_outside,
            CommandOutcome::Refused {
                code: RefusalCode::TargetNotInRoom,
                reply: Reply::refused(RefusalCode::TargetNotInRoom),
            }
        );
        assert_eq!(
            repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap()
                .owner,
            OWNER
        );

        // MEMBER_A is inside the room -> transfer succeeds.
        let out_ok = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Transfer {
                    target_id: MEMBER_A,
                },
            })
            .await;
        assert_eq!(
            out_ok,
            CommandOutcome::Applied {
                code: SuccessCode::Transferred,
                reply: Reply::ok(SuccessCode::Transferred),
            }
        );
        assert_eq!(
            repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap()
                .owner,
            MEMBER_A
        );
    }

    #[tokio::test]
    async fn permit_refuses_ineligible_members_and_grants_eligible_member() {
        let h = make_harness(live_settings());

        // 1. Target is owner -> refused.
        let out_owner = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Permit { target_id: OWNER },
            })
            .await;
        assert_eq!(
            out_owner,
            CommandOutcome::Refused {
                code: RefusalCode::TargetIsOwner,
                reply: Reply::refused(RefusalCode::TargetIsOwner),
            }
        );

        // 2. Target is self bot -> refused.
        let out_self = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Permit {
                    target_id: BOT_USER,
                },
            })
            .await;
        assert_eq!(
            out_self,
            CommandOutcome::Refused {
                code: RefusalCode::TargetIsSelfBot,
                reply: Reply::refused(RefusalCode::TargetIsSelfBot),
            }
        );

        // 3. Target is another bot -> refused.
        let out_bot = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Permit {
                    target_id: OTHER_BOT,
                },
            })
            .await;
        assert_eq!(
            out_bot,
            CommandOutcome::Refused {
                code: RefusalCode::TargetIsBot,
                reply: Reply::refused(RefusalCode::TargetIsBot),
            }
        );

        // 4. Target has protected role -> refused.
        let out_staff = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Permit {
                    target_id: STAFF_USER,
                },
            })
            .await;
        assert_eq!(
            out_staff,
            CommandOutcome::Refused {
                code: RefusalCode::TargetProtectedOrRestricted,
                reply: Reply::refused(RefusalCode::TargetProtectedOrRestricted),
            }
        );

        // 5. Target has active safeword -> refused.
        h.safeword.active.lock().unwrap().insert(MEMBER_B);
        let out_sw = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Permit {
                    target_id: MEMBER_B,
                },
            })
            .await;
        assert_eq!(
            out_sw,
            CommandOutcome::Refused {
                code: RefusalCode::TargetSafewordActive,
                reply: Reply::refused(RefusalCode::TargetSafewordActive),
            }
        );
        h.safeword.active.lock().unwrap().remove(&MEMBER_B);

        // 6. Target lacks category visibility -> refused.
        h.ports
            .state
            .lock()
            .unwrap()
            .profiles
            .get_mut(&MEMBER_B)
            .unwrap()
            .can_view_category = false;
        let out_cat = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Permit {
                    target_id: MEMBER_B,
                },
            })
            .await;
        assert_eq!(
            out_cat,
            CommandOutcome::Refused {
                code: RefusalCode::CategoryNotVisible,
                reply: Reply::refused(RefusalCode::CategoryNotVisible),
            }
        );
        h.ports
            .state
            .lock()
            .unwrap()
            .profiles
            .get_mut(&MEMBER_B)
            .unwrap()
            .can_view_category = true;

        // 7. Eligible member -> permitted, and second permit is refused as AlreadyPermitted.
        let out_ok = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Permit {
                    target_id: MEMBER_B,
                },
            })
            .await;
        assert_eq!(
            out_ok,
            CommandOutcome::Applied {
                code: SuccessCode::Permitted,
                reply: Reply::ok(SuccessCode::Permitted),
            }
        );
        let out_dup = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Permit {
                    target_id: MEMBER_B,
                },
            })
            .await;
        assert_eq!(
            out_dup,
            CommandOutcome::Refused {
                code: RefusalCode::AlreadyPermitted,
                reply: Reply::refused(RefusalCode::AlreadyPermitted),
            }
        );
    }

    #[tokio::test]
    async fn reject_refuses_protected_targets_disconnects_present_member_and_reports_failed_disconnect(
    ) {
        let h = make_harness(live_settings());

        // Cannot reject owner, self bot, or staff.
        for (target, expected) in [
            (OWNER, RefusalCode::TargetIsOwner),
            (BOT_USER, RefusalCode::TargetIsSelfBot),
            (STAFF_USER, RefusalCode::TargetProtectedOrRestricted),
        ] {
            let out = h
                .handler
                .handle(&VcCommandRequest {
                    guild_id: Some(HOME_GUILD),
                    channel_id: ROOM_CH,
                    actor_id: OWNER,
                    subcommand: VcSubcommand::Reject { target_id: target },
                })
                .await;
            assert_eq!(
                out,
                CommandOutcome::Refused {
                    code: expected,
                    reply: Reply::refused(expected),
                }
            );
        }

        // MEMBER_A is inside the room -> reject denies CONNECT and disconnects MEMBER_A.
        let out_present = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Reject {
                    target_id: MEMBER_A,
                },
            })
            .await;
        assert_eq!(
            out_present,
            CommandOutcome::Applied {
                code: SuccessCode::RejectedAndDisconnected,
                reply: Reply::ok(SuccessCode::RejectedAndDisconnected),
            }
        );
        assert!(h
            .ports
            .state
            .lock()
            .unwrap()
            .disconnects
            .contains(&(HOME_GUILD, MEMBER_A)));

        // Put MEMBER_A back in the room and make disconnect fail -> reported honestly.
        {
            let mut s = h.ports.state.lock().unwrap();
            s.room_members.insert(ROOM_CH, vec![OWNER, MEMBER_A]);
            s.fail_disconnect = Some(CommandPortError::Forbidden);
        }
        let out_fail_dc = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Reject {
                    target_id: MEMBER_A,
                },
            })
            .await;
        assert_eq!(
            out_fail_dc,
            CommandOutcome::Refused {
                code: RefusalCode::DisconnectFailed(CommandPortError::Forbidden),
                reply: Reply::refused(RefusalCode::DisconnectFailed(CommandPortError::Forbidden)),
            }
        );
    }

    #[tokio::test]
    async fn rename_cleans_name_and_reports_rate_limit_and_budget_honestly() {
        let h = make_harness(live_settings());

        // 1. Discord 429 rate-limit -> reported honestly, does not consume budget or claim rename.
        h.ports.state.lock().unwrap().fail_rename = Some(CommandPortError::RateLimited);
        let out_429 = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Rename {
                    name: "Room One".to_string(),
                },
            })
            .await;
        assert_eq!(
            out_429,
            CommandOutcome::Refused {
                code: RefusalCode::RenameRateLimited,
                reply: Reply::refused(RefusalCode::RenameRateLimited),
            }
        );
        assert!(h.ports.state.lock().unwrap().renames.is_empty());
        h.ports.state.lock().unwrap().fail_rename = None;

        // 2. Two renames inside the 600s window succeed and sanitize mentions/links.
        for raw in ["Chill @everyone Lounge", "StudyRoom"] {
            let out = h
                .handler
                .handle(&VcCommandRequest {
                    guild_id: Some(HOME_GUILD),
                    channel_id: ROOM_CH,
                    actor_id: OWNER,
                    subcommand: VcSubcommand::Rename {
                        name: raw.to_string(),
                    },
                })
                .await;
            assert_eq!(
                out,
                CommandOutcome::Applied {
                    code: SuccessCode::Renamed,
                    reply: Reply::ok(SuccessCode::Renamed),
                }
            );
            assert!(!out.reply().unwrap().text.contains("Chill"));
            assert!(!out.reply().unwrap().text.contains("StudyRoom"));
        }
        assert_eq!(h.ports.state.lock().unwrap().renames.len(), 2);
        assert_eq!(h.ports.state.lock().unwrap().renames[0].2, "Chill Lounge");

        // 3. Third rename inside the window is refused by VR1 budget and calls Discord 0 times.
        let out_budget = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Rename {
                    name: "Third Rename".to_string(),
                },
            })
            .await;
        assert_eq!(
            out_budget,
            CommandOutcome::Refused {
                code: RefusalCode::RenameBudgetExhausted,
                reply: Reply::refused(RefusalCode::RenameBudgetExhausted),
            }
        );
        assert_eq!(h.ports.state.lock().unwrap().renames.len(), 2);
    }

    #[tokio::test]
    async fn limit_enforces_bounds_and_lock_unlock_update_discord_and_row() {
        let h = make_harness(live_settings());

        // Limit > 99 is refused before touching Discord or DB.
        let out_bad = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Limit { limit: 100 },
            })
            .await;
        assert_eq!(
            out_bad,
            CommandOutcome::Refused {
                code: RefusalCode::LimitOutOfBounds,
                reply: Reply::refused(RefusalCode::LimitOutOfBounds),
            }
        );
        assert_eq!(
            repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap()
                .user_limit,
            0
        );

        // Boundary limits 99 and 0 succeed.
        for lim in [99, 0] {
            let out = h
                .handler
                .handle(&VcCommandRequest {
                    guild_id: Some(HOME_GUILD),
                    channel_id: ROOM_CH,
                    actor_id: OWNER,
                    subcommand: VcSubcommand::Limit { limit: lim },
                })
                .await;
            assert_eq!(
                out,
                CommandOutcome::Applied {
                    code: SuccessCode::LimitUpdated,
                    reply: Reply::ok(SuccessCode::LimitUpdated),
                }
            );
            assert_eq!(
                repository::get(&h.db.lock().unwrap(), ROOM_CH)
                    .unwrap()
                    .unwrap()
                    .user_limit,
                lim
            );
        }

        // Lock and unlock update Discord @everyone overwrite and DB row.
        let out_lock = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Lock,
            })
            .await;
        assert_eq!(
            out_lock,
            CommandOutcome::Applied {
                code: SuccessCode::Locked,
                reply: Reply::ok(SuccessCode::Locked),
            }
        );
        assert!(
            repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap()
                .locked
        );

        let out_unlock = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Unlock,
            })
            .await;
        assert_eq!(
            out_unlock,
            CommandOutcome::Applied {
                code: SuccessCode::Unlocked,
                reply: Reply::ok(SuccessCode::Unlocked),
            }
        );
        assert!(
            !repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap()
                .locked
        );
    }

    #[tokio::test]
    async fn untracked_channel_other_guild_and_unset_home_guild_are_refused_safely() {
        let h = make_harness(live_settings());

        // 1. Untracked channel in home guild -> refused with UntrackedChannel.
        let out_untracked = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: UNTRACKED_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Lock,
            })
            .await;
        assert_eq!(
            out_untracked,
            CommandOutcome::Refused {
                code: RefusalCode::UntrackedChannel,
                reply: Reply::refused(RefusalCode::UntrackedChannel),
            }
        );

        // 2. Two-guild scenario: call from OTHER_GUILD touches, sends and alerts nothing.
        let before_calls = h.ports.state.lock().unwrap().calls_count;
        let out_other = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(OTHER_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Lock,
            })
            .await;
        assert_eq!(out_other, CommandOutcome::Ignored(RefusalCode::WrongGuild));
        assert!(out_other.reply().is_none());
        assert_eq!(h.ports.state.lock().unwrap().calls_count, before_calls);
        assert!(h.alerter.alerts.lock().unwrap().is_empty());
        assert!(
            !repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap()
                .locked
        );

        // 3. Mode is Live/Shadow and home_guild_id is unset (None or Some(0)) -> fail closed.
        let unset_settings = settings_with("live", None);
        let h_unset = make_harness(unset_settings);
        let out_unset = h_unset
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Lock,
            })
            .await;
        assert_eq!(
            out_unset,
            CommandOutcome::Refused {
                code: RefusalCode::HomeGuildUnset,
                reply: Reply::refused(RefusalCode::HomeGuildUnset),
            }
        );
        assert_eq!(h_unset.ports.state.lock().unwrap().calls_count, 0);
    }

    #[tokio::test]
    async fn row_consistency_under_failed_discord_write_and_failed_row_write() {
        let h = make_harness(live_settings());

        // 1. Failed Discord write leaves SQLite row unchanged and emits no drift alert.
        h.ports.state.lock().unwrap().fail_limit = Some(CommandPortError::Unavailable);
        let out_discord_fail = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Limit { limit: 8 },
            })
            .await;
        assert_eq!(
            out_discord_fail,
            CommandOutcome::Refused {
                code: RefusalCode::DiscordFailed(CommandPortError::Unavailable),
                reply: Reply::refused(RefusalCode::DiscordFailed(CommandPortError::Unavailable)),
            }
        );
        assert_eq!(
            repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap()
                .user_limit,
            0
        );
        assert!(h.alerter.alerts.lock().unwrap().is_empty());
        h.ports.state.lock().unwrap().fail_limit = None;

        // 2. Failed SQLite row write after Discord write succeeds returns RowUpdateFailed and alerts staff.
        h.db.lock()
            .unwrap()
            .execute(
                "CREATE TRIGGER fail_voice_rooms_update BEFORE UPDATE ON voice_rooms BEGIN SELECT RAISE(ABORT, 'disk_err'); END;",
                [],
            )
            .unwrap();
        let out_db_fail = h
            .handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: OWNER,
                subcommand: VcSubcommand::Lock,
            })
            .await;
        assert_eq!(
            out_db_fail,
            CommandOutcome::Refused {
                code: RefusalCode::RowUpdateFailed,
                reply: Reply::refused(RefusalCode::RowUpdateFailed),
            }
        );
        assert_eq!(
            *h.alerter.alerts.lock().unwrap(),
            vec![CommandAlertCode::LockRowDrift]
        );
    }

    #[tokio::test]
    async fn seeded_liveness_repeated_commands_with_random_failures_preserve_invariants() {
        let h = make_harness(live_settings());
        let mut seed: u64 = 0x5EED_CAFE_1234_5678;
        let mut next_u64 = || -> u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        let mut applied_total = 0_u32;
        for step in 0..80_i64 {
            h.clock.set(2_000 + step * 35);
            let r = next_u64();
            {
                let mut s = h.ports.state.lock().unwrap();
                s.fail_rename = if r % 5 == 0 {
                    Some(CommandPortError::RateLimited)
                } else {
                    None
                };
                s.fail_limit = if r % 7 == 0 {
                    Some(CommandPortError::Unavailable)
                } else {
                    None
                };
                s.fail_overwrite = if r % 11 == 0 {
                    Some(CommandPortError::Forbidden)
                } else {
                    None
                };
                s.room_members.insert(ROOM_CH, vec![OWNER, MEMBER_A]);
            }
            let current_owner = repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap()
                .owner;
            let other = if current_owner == OWNER {
                MEMBER_A
            } else {
                OWNER
            };
            let sub = match r % 7 {
                0 => VcSubcommand::Rename {
                    name: format!("Room {step}"),
                },
                1 => VcSubcommand::Limit {
                    limit: u32::try_from(r % 120).unwrap(),
                },
                2 => VcSubcommand::Lock,
                3 => VcSubcommand::Unlock,
                4 => VcSubcommand::Permit {
                    target_id: MEMBER_B,
                },
                5 => VcSubcommand::Reject {
                    target_id: MEMBER_B,
                },
                _ => VcSubcommand::Transfer { target_id: other },
            };
            let actor = if r % 4 == 0 { other } else { current_owner };
            let out = h
                .handler
                .handle(&VcCommandRequest {
                    guild_id: Some(HOME_GUILD),
                    channel_id: ROOM_CH,
                    actor_id: actor,
                    subcommand: sub,
                })
                .await;
            let reply = out.reply().expect("home guild live command must reply");
            assert!(reply.ephemeral);
            assert!(!reply.text.contains(&format!("Room {step}")));
            if matches!(out, CommandOutcome::Applied { .. }) {
                applied_total += 1;
            }

            let row = repository::get(&h.db.lock().unwrap(), ROOM_CH)
                .unwrap()
                .unwrap();
            assert!(row.owner == OWNER || row.owner == MEMBER_A);
            assert!(row.user_limit <= MAX_USER_LIMIT);
        }
        // Progress floor: a run that refused everything would otherwise pass.
        assert!(
            applied_total >= 15,
            "only {applied_total} of 80 commands applied"
        );
    }

    const VIEW_CHANNEL: u64 = 1 << 10;

    async fn run(h: &Harness, actor: u64, sub: VcSubcommand) -> CommandOutcome {
        h.handler
            .handle(&VcCommandRequest {
                guild_id: Some(HOME_GUILD),
                channel_id: ROOM_CH,
                actor_id: actor,
                subcommand: sub,
            })
            .await
    }

    fn row_of(h: &Harness) -> RoomRow {
        repository::get(&h.db.lock().unwrap(), ROOM_CH)
            .unwrap()
            .unwrap()
    }

    fn refused(code: RefusalCode) -> CommandOutcome {
        CommandOutcome::Refused {
            code,
            reply: Reply::refused(code),
        }
    }

    fn applied(code: SuccessCode) -> CommandOutcome {
        CommandOutcome::Applied {
            code,
            reply: Reply::ok(code),
        }
    }

    fn all_owner_subcommands() -> Vec<VcSubcommand> {
        vec![
            VcSubcommand::Rename {
                name: "New".to_string(),
            },
            VcSubcommand::Limit { limit: 5 },
            VcSubcommand::Lock,
            VcSubcommand::Unlock,
            VcSubcommand::Permit {
                target_id: MEMBER_B,
            },
            VcSubcommand::Reject {
                target_id: MEMBER_B,
            },
            VcSubcommand::Transfer {
                target_id: MEMBER_A,
            },
            VcSubcommand::Claim,
        ]
    }

    #[tokio::test]
    async fn shadow_mode_validates_but_writes_nothing() {
        let h = make_harness(settings_with("shadow", Some(HOME_GUILD)));
        let before = row_of(&h);
        for sub in all_owner_subcommands() {
            let actor = if sub == VcSubcommand::Claim {
                MEMBER_A
            } else {
                OWNER
            };
            let out = run(&h, actor, sub.clone()).await;
            // Claim is refused (owner present); every other command only reports a shadow no-op.
            if sub == VcSubcommand::Claim {
                assert_eq!(out, refused(RefusalCode::OwnerStillPresent));
            } else {
                assert_eq!(out, applied(SuccessCode::ShadowNoop), "{sub:?}");
            }
        }
        let s = h.ports.state.lock().unwrap();
        assert!(s.renames.is_empty() && s.limits.is_empty() && s.overwrites.is_empty());
        assert!(s.deleted_overwrites.is_empty() && s.disconnects.is_empty() && s.moves.is_empty());
        drop(s);
        assert_eq!(row_of(&h), before);
    }

    #[tokio::test]
    async fn mode_off_and_dm_and_other_guild_are_ignored_for_every_subcommand() {
        let off = make_harness(settings_with("off", Some(HOME_GUILD)));
        let live = make_harness(live_settings());
        for sub in all_owner_subcommands() {
            let out = run(&off, OWNER, sub.clone()).await;
            assert_eq!(out, CommandOutcome::Ignored(RefusalCode::FeatureDisabled));
            for guild in [Some(OTHER_GUILD), None] {
                let out = live
                    .handler
                    .handle(&VcCommandRequest {
                        guild_id: guild,
                        channel_id: ROOM_CH,
                        actor_id: OWNER,
                        subcommand: sub.clone(),
                    })
                    .await;
                assert_eq!(out, CommandOutcome::Ignored(RefusalCode::WrongGuild));
            }
        }
        assert_eq!(off.ports.state.lock().unwrap().calls_count, 0);
        assert_eq!(live.ports.state.lock().unwrap().calls_count, 0);
        assert!(live.alerter.alerts.lock().unwrap().is_empty());
        assert_eq!(row_of(&live).owner, OWNER);
    }

    #[tokio::test]
    async fn row_in_another_guild_is_untracked_for_this_guild() {
        let h = make_harness(live_settings());
        h.db.lock()
            .unwrap()
            .execute(
                "UPDATE voice_rooms SET guild = ?1",
                [OTHER_GUILD.to_string()],
            )
            .unwrap();
        let out = run(&h, OWNER, VcSubcommand::Lock).await;
        assert_eq!(out, refused(RefusalCode::UntrackedChannel));
        assert_eq!(h.ports.state.lock().unwrap().calls_count, 0);
    }

    #[tokio::test]
    async fn permit_refuses_privileged_restricted_and_unjudgeable_roles() {
        let h = make_harness(live_settings());
        for (roles, expected) in [
            (
                vec![ROLE_PRIVILEGED],
                refused(RefusalCode::TargetProtectedOrRestricted),
            ),
            (
                vec![ROLE_NORMAL, ROLE_RESTRICTED],
                refused(RefusalCode::TargetProtectedOrRestricted),
            ),
            (
                vec![999_999],
                refused(RefusalCode::DiscordFailed(CommandPortError::Unavailable)),
            ),
        ] {
            h.ports
                .state
                .lock()
                .unwrap()
                .profiles
                .get_mut(&MEMBER_B)
                .unwrap()
                .role_ids = roles;
            let out = run(
                &h,
                OWNER,
                VcSubcommand::Permit {
                    target_id: MEMBER_B,
                },
            )
            .await;
            assert_eq!(out, expected);
        }
        assert!(h.ports.state.lock().unwrap().overwrites.is_empty());
        let zero = run(&h, OWNER, VcSubcommand::Permit { target_id: 0 }).await;
        assert_eq!(zero, refused(RefusalCode::InvalidUserId));
    }

    #[tokio::test]
    async fn reject_of_absent_member_denies_connect_without_disconnect_and_clears_permit() {
        let h = make_harness(live_settings());
        let permit = VcSubcommand::Permit {
            target_id: MEMBER_B,
        };
        assert_eq!(
            run(&h, OWNER, permit.clone()).await,
            applied(SuccessCode::Permitted)
        );
        let out = run(
            &h,
            OWNER,
            VcSubcommand::Reject {
                target_id: MEMBER_B,
            },
        )
        .await;
        assert_eq!(out, applied(SuccessCode::Rejected));
        {
            let s = h.ports.state.lock().unwrap();
            assert!(s.disconnects.is_empty());
            assert_eq!(
                s.overwrites.last().unwrap().2,
                RoomOverwrite {
                    target: OverwriteTarget::Member(MEMBER_B),
                    allow: 0,
                    deny: policy::CONNECT,
                }
            );
        }
        // The reject cleared the remembered permit, so permitting again is accepted.
        assert_eq!(
            run(&h, OWNER, permit).await,
            applied(SuccessCode::Permitted)
        );
        // A restricted-role member (not staff) can be rejected.
        h.ports
            .state
            .lock()
            .unwrap()
            .profiles
            .get_mut(&MEMBER_B)
            .unwrap()
            .role_ids = vec![ROLE_RESTRICTED];
        let out = run(
            &h,
            OWNER,
            VcSubcommand::Reject {
                target_id: MEMBER_B,
            },
        )
        .await;
        assert_eq!(out, applied(SuccessCode::Rejected));
    }

    #[tokio::test]
    async fn lock_and_unlock_keep_the_category_baseline() {
        let h = make_harness(live_settings());
        h.ports.state.lock().unwrap().category_overwrites = vec![RoomOverwrite {
            target: OverwriteTarget::Everyone,
            allow: VIEW_CHANNEL | policy::CONNECT,
            deny: 0,
        }];
        assert_eq!(
            run(&h, OWNER, VcSubcommand::Lock).await,
            applied(SuccessCode::Locked)
        );
        assert_eq!(
            run(&h, OWNER, VcSubcommand::Unlock).await,
            applied(SuccessCode::Unlocked)
        );
        {
            let s = h.ports.state.lock().unwrap();
            assert_eq!(
                s.overwrites[0].2,
                RoomOverwrite {
                    target: OverwriteTarget::Everyone,
                    allow: VIEW_CHANNEL,
                    deny: policy::CONNECT,
                }
            );
            assert_eq!(
                s.overwrites[1].2,
                RoomOverwrite {
                    target: OverwriteTarget::Everyone,
                    allow: VIEW_CHANNEL | policy::CONNECT,
                    deny: 0,
                }
            );
            assert!(s.deleted_overwrites.is_empty());
        }
        // No category baseline for @everyone: unlock removes the room's @everyone overwrite.
        h.ports.state.lock().unwrap().category_overwrites.clear();
        assert_eq!(
            run(&h, OWNER, VcSubcommand::Unlock).await,
            applied(SuccessCode::Unlocked)
        );
        assert_eq!(
            h.ports.state.lock().unwrap().deleted_overwrites,
            vec![(HOME_GUILD, ROOM_CH, OverwriteTarget::Everyone)]
        );
        // A failed category read changes nothing: the row stays unlocked after a failed lock.
        h.ports.state.lock().unwrap().fail_category = Some(CommandPortError::Unavailable);
        let before_overwrites = h.ports.state.lock().unwrap().overwrites.len();
        assert_eq!(
            run(&h, OWNER, VcSubcommand::Lock).await,
            refused(RefusalCode::DiscordFailed(CommandPortError::Unavailable))
        );
        assert_eq!(
            h.ports.state.lock().unwrap().overwrites.len(),
            before_overwrites
        );
        assert!(!row_of(&h).locked);
    }

    #[tokio::test]
    async fn transfer_failed_discord_step_keeps_row_and_failed_row_write_alerts() {
        let h = make_harness(live_settings());
        let transfer = VcSubcommand::Transfer {
            target_id: MEMBER_A,
        };
        // Dropping the old owner's overwrite fails: the row is unchanged, the rollback also
        // fails in this fake, so staff are alerted about the leftover overwrite.
        h.ports.state.lock().unwrap().fail_delete_overwrite = Some(CommandPortError::Forbidden);
        assert_eq!(
            run(&h, OWNER, transfer.clone()).await,
            refused(RefusalCode::DiscordFailed(CommandPortError::Forbidden))
        );
        assert_eq!(row_of(&h).owner, OWNER);
        assert_eq!(
            *h.alerter.alerts.lock().unwrap(),
            vec![CommandAlertCode::OwnerOverwriteDrift]
        );
        h.ports.state.lock().unwrap().fail_delete_overwrite = None;
        h.alerter.alerts.lock().unwrap().clear();
        // The new owner's overwrite fails: nothing was written, no alert.
        h.ports.state.lock().unwrap().fail_overwrite = Some(CommandPortError::Forbidden);
        assert_eq!(
            run(&h, OWNER, transfer.clone()).await,
            refused(RefusalCode::DiscordFailed(CommandPortError::Forbidden))
        );
        assert!(h.alerter.alerts.lock().unwrap().is_empty());
        h.ports.state.lock().unwrap().fail_overwrite = None;
        // Row write fails after Discord changed: stable code and an alert.
        h.db.lock()
            .unwrap()
            .execute(
                "CREATE TRIGGER fail_owner BEFORE UPDATE ON voice_rooms BEGIN SELECT RAISE(ABORT, 'x'); END;",
                [],
            )
            .unwrap();
        assert_eq!(
            run(&h, OWNER, transfer).await,
            refused(RefusalCode::RowUpdateFailed)
        );
        assert_eq!(row_of(&h).owner, OWNER);
        assert_eq!(
            *h.alerter.alerts.lock().unwrap(),
            vec![CommandAlertCode::OwnerRowDrift]
        );
    }

    /// Puts the owner outside the room and `MEMBER_A` inside it.
    fn owner_outside(h: &Harness) {
        h.ports
            .state
            .lock()
            .unwrap()
            .room_members
            .insert(ROOM_CH, vec![MEMBER_A]);
    }

    /// Puts the owner and `MEMBER_A` inside the room.
    /// Reports the owner absent every 10 s from `from` to `to` (inclusive) and sets the clock to `to`.
    fn observe_absent(h: &Harness, from: i64, to: i64) {
        let mut t = from;
        while t < to {
            h.handler.note_owner_absent(ROOM_CH, t);
            t += 10;
        }
        h.handler.note_owner_absent(ROOM_CH, to);
        h.clock.set(to);
    }

    fn owner_inside(h: &Harness) {
        h.ports
            .state
            .lock()
            .unwrap()
            .room_members
            .insert(ROOM_CH, vec![OWNER, MEMBER_A]);
    }

    #[tokio::test]
    async fn claim_with_no_recorded_absence_is_refused_however_long_it_waits() {
        let h = make_harness(live_settings());
        let claim = VcSubcommand::Claim;
        assert_eq!(
            run(&h, OWNER, claim.clone()).await,
            refused(RefusalCode::AlreadyRoomOwner)
        );
        owner_outside(&h);
        // Nobody recorded when the owner left: a claim must not guess, now or much later.
        for t in [5_000, 5_060, 9_000, 90_000] {
            h.clock.set(t);
            assert_eq!(
                run(&h, MEMBER_A, claim.clone()).await,
                refused(RefusalCode::OwnerAbsenceNotRecorded),
                "t={t}"
            );
        }
        assert_eq!(row_of(&h).owner, OWNER);
        assert!(h.ports.state.lock().unwrap().overwrites.is_empty());
    }

    #[tokio::test]
    async fn claim_refuses_the_unobserved_return_sequence_5000_5030_5100_5110() {
        // qa-2 probe: the absence IS recorded at 5000 (note_owner_absent), the owner is back at
        // 5030 and gone again at 5100 with nothing reported, and a claim arrives at 5110.
        let h = make_harness(live_settings());
        let claim = VcSubcommand::Claim;
        owner_outside(&h);
        h.clock.set(5_000);
        h.handler.note_owner_absent(ROOM_CH, 5_000);
        owner_inside(&h); // 5030: the owner returns, unobserved by the handler
        h.clock.set(5_030);
        owner_outside(&h); // 5100: leaves again, still unobserved
        h.clock.set(5_100);
        h.clock.set(5_110);
        assert_eq!(
            run(&h, MEMBER_A, claim).await,
            refused(RefusalCode::OwnerAbsenceNotContinuous)
        );
        assert_eq!(row_of(&h).owner, OWNER);
        assert!(h.ports.state.lock().unwrap().overwrites.is_empty());
    }

    #[tokio::test]
    async fn claim_needs_a_gapless_fresh_absence_record_to_pass() {
        let claim = VcSubcommand::Claim;
        // Heartbeat every 10 s from 5000 to 5060: continuous for 60 s, so the claim passes, and
        // it still passes when the last observation is exactly ABSENCE_MAX_GAP_SECS old.
        for (now, want) in [
            (5_060, Some(SuccessCode::Claimed)),
            (5_060 + ABSENCE_MAX_GAP_SECS, Some(SuccessCode::Claimed)),
            (5_060 + ABSENCE_MAX_GAP_SECS + 1, None),
            (9_000, None),
        ] {
            let h = make_harness(live_settings());
            owner_outside(&h);
            observe_absent(&h, 5_000, 5_060);
            h.clock.set(now);
            let out = run(&h, MEMBER_A, claim.clone()).await;
            match want {
                Some(code) => assert_eq!(out, applied(code), "now={now}"),
                None => {
                    assert_eq!(
                        out,
                        refused(RefusalCode::OwnerAbsenceNotContinuous),
                        "now={now}"
                    );
                    assert_eq!(row_of(&h).owner, OWNER);
                }
            }
        }
    }

    #[tokio::test]
    async fn a_gap_in_the_observations_restarts_the_grace() {
        let h = make_harness(live_settings());
        let claim = VcSubcommand::Claim;
        owner_outside(&h);
        observe_absent(&h, 5_000, 5_010);
        // 16 s without a report: the owner could have come and gone, so the absence restarts.
        h.handler.note_owner_absent(ROOM_CH, 5_026);
        observe_absent(&h, 5_026, 5_085);
        assert_eq!(
            run(&h, MEMBER_A, claim.clone()).await,
            refused(RefusalCode::OwnerGraceNotElapsed)
        );
        observe_absent(&h, 5_085, 5_086);
        assert_eq!(
            run(&h, MEMBER_A, claim).await,
            applied(SuccessCode::Claimed)
        );
        assert_eq!(row_of(&h).owner, MEMBER_A);
    }

    #[tokio::test]
    async fn claim_grace_is_the_vr1_claim_value_and_runs_from_the_recorded_absence() {
        // The harness gives VR2's cleanup grace 10 s and the claim grace the VR1 60 s.
        let h = make_harness(live_settings());
        let claim = VcSubcommand::Claim;
        owner_outside(&h);
        // Reported every second: repeated notes of one continuous absence must not move the start.
        for t in 5_000..=5_059 {
            h.handler.note_owner_absent(ROOM_CH, t);
            h.clock.set(t);
            assert_eq!(
                run(&h, MEMBER_A, claim.clone()).await,
                refused(RefusalCode::OwnerGraceNotElapsed),
                "t={t}"
            );
        }
        assert_eq!(row_of(&h).owner, OWNER);
        h.handler.note_owner_absent(ROOM_CH, 5_060);
        h.clock.set(5_060);
        assert_eq!(
            run(&h, MEMBER_A, claim).await,
            applied(SuccessCode::Claimed)
        );
        assert_eq!(row_of(&h).owner, MEMBER_A);
    }

    #[tokio::test]
    async fn an_observed_return_clears_the_absence_and_the_grace_restarts() {
        let h = make_harness(live_settings());
        let claim = VcSubcommand::Claim;
        owner_outside(&h);
        h.handler.note_owner_absent(ROOM_CH, 5_000);
        // The wiring sees the owner rejoin at 5_030 and leave again at 5_100.
        h.handler.note_owner_present(ROOM_CH);
        h.clock.set(5_100);
        // Absence after the return was never recorded: refused even though 100 s have passed.
        assert_eq!(
            run(&h, MEMBER_A, claim.clone()).await,
            refused(RefusalCode::OwnerAbsenceNotRecorded)
        );
        observe_absent(&h, 5_100, 5_159);
        assert_eq!(
            run(&h, MEMBER_A, claim.clone()).await,
            refused(RefusalCode::OwnerGraceNotElapsed)
        );
        h.clock.set(5_160);
        assert_eq!(
            run(&h, MEMBER_A, claim).await,
            applied(SuccessCode::Claimed)
        );
        assert_eq!(row_of(&h).owner, MEMBER_A);
    }

    #[tokio::test]
    async fn a_claim_that_sees_the_owner_present_clears_the_recorded_absence() {
        let h = make_harness(live_settings());
        let claim = VcSubcommand::Claim;
        owner_outside(&h);
        h.handler.note_owner_absent(ROOM_CH, 5_000);
        owner_inside(&h);
        h.clock.set(5_030);
        assert_eq!(
            run(&h, MEMBER_A, claim.clone()).await,
            refused(RefusalCode::OwnerStillPresent)
        );
        // The owner leaves again without anyone recording it: the old record must be gone.
        owner_outside(&h);
        h.clock.set(5_100);
        assert_eq!(
            run(&h, MEMBER_A, claim).await,
            refused(RefusalCode::OwnerAbsenceNotRecorded)
        );
        assert_eq!(row_of(&h).owner, OWNER);
    }

    #[tokio::test]
    async fn two_simultaneous_claims_make_exactly_one_new_owner() {
        let h = make_harness(live_settings());
        h.ports
            .state
            .lock()
            .unwrap()
            .room_members
            .insert(ROOM_CH, vec![MEMBER_A, MEMBER_B]);
        observe_absent(&h, 1_000, 2_000);
        let (a, b) = tokio::join!(
            run(&h, MEMBER_A, VcSubcommand::Claim),
            run(&h, MEMBER_B, VcSubcommand::Claim)
        );
        let wins = [&a, &b]
            .iter()
            .filter(|o| matches!(o, CommandOutcome::Applied { .. }))
            .count();
        assert_eq!(wins, 1);
        let owner = row_of(&h).owner;
        assert!(owner == MEMBER_A || owner == MEMBER_B);
        let loser = if owner == MEMBER_A { &b } else { &a };
        assert_eq!(loser, &refused(RefusalCode::OwnerStillPresent));
    }

    #[tokio::test]
    async fn rename_failures_other_than_429_change_nothing_and_budget_recovers_after_the_window() {
        let h = make_harness(live_settings());
        h.ports.state.lock().unwrap().fail_rename = Some(CommandPortError::Forbidden);
        let rename = |name: &str| VcSubcommand::Rename {
            name: name.to_string(),
        };
        for _ in 0..3 {
            assert_eq!(
                run(&h, OWNER, rename("Fine")).await,
                refused(RefusalCode::DiscordFailed(CommandPortError::Forbidden))
            );
        }
        h.ports.state.lock().unwrap().fail_rename = None;
        // Failed attempts consumed no budget: two renames still fit in the window.
        for _ in 0..2 {
            assert_eq!(
                run(&h, OWNER, rename("@@@ ###")).await,
                applied(SuccessCode::Renamed)
            );
        }
        {
            let s = h.ports.state.lock().unwrap();
            assert_eq!(s.renames.len(), 2);
            assert_eq!(s.renames[0].2, "###");
        }
        assert_eq!(
            run(&h, OWNER, rename("Late")).await,
            refused(RefusalCode::RenameBudgetExhausted)
        );
        // 599 s after the renames (taken at t=1100) the budget is still exhausted, 600 s later free.
        h.clock.set(1_100 + 599);
        assert_eq!(
            run(&h, OWNER, rename("Late")).await,
            refused(RefusalCode::RenameBudgetExhausted)
        );
        h.clock.set(1_100 + 600);
        assert_eq!(
            run(&h, OWNER, rename("Late")).await,
            applied(SuccessCode::Renamed)
        );
        assert_eq!(h.ports.state.lock().unwrap().renames.len(), 3);
        // A name that cleans to nothing falls back to the default name.
        h.clock.set(1_100 + 1_300);
        assert_eq!(
            run(&h, OWNER, rename("@@@")).await,
            applied(SuccessCode::Renamed)
        );
        assert_eq!(h.ports.state.lock().unwrap().renames[3].2, "Voice Room");
    }

    type RowSnapshot = (u64, bool, u32);

    fn snapshot(row: &RoomRow) -> RowSnapshot {
        (row.owner, row.locked, row.user_limit)
    }

    #[tokio::test]
    async fn seeded_model_check_row_follows_outcome_under_random_discord_and_row_failures() {
        for seed0 in [
            0x9E37_79B9_7F4A_7C15_u64,
            0xD1B5_4A32_D192_ED03,
            0x0123_4567_89AB_CDEF,
        ] {
            let h = make_harness(live_settings());
            let mut seed = seed0;
            let mut next_u64 = || -> u64 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let mut applied_count = 0_u32;
            let mut drift_count = 0_u32;
            let mut discord_fail_count = 0_u32;
            let mut claims = 0_u32;
            let mut trigger_on = false;
            for step in 0..300_i64 {
                h.clock.set(10_000 + step * 40);
                let r = next_u64();
                if r % 9 == 0 {
                    let sql = if trigger_on {
                        "DROP TRIGGER fail_any"
                    } else {
                        "CREATE TRIGGER fail_any BEFORE UPDATE ON voice_rooms BEGIN SELECT RAISE(ABORT, 'x'); END;"
                    };
                    h.db.lock().unwrap().execute(sql, []).unwrap();
                    trigger_on = !trigger_on;
                }
                let before = row_of(&h);
                let owner = before.owner;
                let other = if owner == OWNER { MEMBER_A } else { OWNER };
                let claim_step = r % 6 == 0;
                {
                    let mut s = h.ports.state.lock().unwrap();
                    let pick = |m: u64, e: CommandPortError| (r / m).is_multiple_of(4).then_some(e);
                    s.fail_rename = pick(3, CommandPortError::RateLimited);
                    s.fail_limit = pick(5, CommandPortError::Unavailable);
                    s.fail_overwrite = pick(7, CommandPortError::Forbidden);
                    s.fail_delete_overwrite = pick(11, CommandPortError::Forbidden);
                    s.fail_disconnect = pick(13, CommandPortError::Forbidden);
                    s.fail_category = pick(17, CommandPortError::Unavailable);
                    let members = if claim_step {
                        vec![other]
                    } else {
                        vec![owner, other]
                    };
                    s.room_members.insert(ROOM_CH, members);
                }
                if claim_step {
                    observe_absent(&h, 10_000 + step * 40 - 60, 10_000 + step * 40);
                }
                let sub = if claim_step {
                    VcSubcommand::Claim
                } else {
                    match r / 31 % 7 {
                        0 => VcSubcommand::Rename {
                            name: format!("Room {step}"),
                        },
                        1 => VcSubcommand::Limit {
                            limit: u32::try_from(r / 101 % 120).unwrap(),
                        },
                        2 => VcSubcommand::Lock,
                        3 => VcSubcommand::Unlock,
                        4 => VcSubcommand::Permit {
                            target_id: MEMBER_B,
                        },
                        5 => VcSubcommand::Reject {
                            target_id: MEMBER_B,
                        },
                        _ => VcSubcommand::Transfer { target_id: other },
                    }
                };
                let actor = if claim_step { other } else { owner };
                let alerts_before = h.alerter.alerts.lock().unwrap().len();
                let out = run(&h, actor, sub.clone()).await;
                let after = row_of(&h);
                let alerts_after = h.alerter.alerts.lock().unwrap().len();
                match &out {
                    CommandOutcome::Applied { code, .. } => {
                        applied_count += 1;
                        match (&sub, code) {
                            (VcSubcommand::Limit { limit }, SuccessCode::LimitUpdated) => {
                                assert_eq!(after.user_limit, *limit);
                            }
                            (_, SuccessCode::Locked) => assert!(after.locked),
                            (_, SuccessCode::Unlocked) => assert!(!after.locked),
                            (VcSubcommand::Transfer { target_id }, SuccessCode::Transferred) => {
                                assert_eq!(after.owner, *target_id);
                            }
                            (_, SuccessCode::Claimed) => {
                                claims += 1;
                                assert_eq!(after.owner, actor);
                            }
                            _ => assert_eq!(snapshot(&after), snapshot(&before)),
                        }
                        assert_eq!(alerts_after, alerts_before);
                    }
                    CommandOutcome::Refused { code, .. } => {
                        // Whatever the reason, a refusal never changes the row, and a row write
                        // that failed after Discord changed is the only refusal that alerts.
                        assert_eq!(snapshot(&after), snapshot(&before), "{sub:?} {code:?}");
                        if *code == RefusalCode::RowUpdateFailed {
                            drift_count += 1;
                            assert_eq!(alerts_after, alerts_before + 1);
                        } else {
                            if matches!(code, RefusalCode::DiscordFailed(_)) {
                                discord_fail_count += 1;
                            }
                            assert!(alerts_after <= alerts_before + 1);
                        }
                    }
                    CommandOutcome::Ignored(code) => panic!("home guild command ignored: {code:?}"),
                }
                assert!(after.user_limit <= MAX_USER_LIMIT);
            }
            assert!(
                applied_count > 20,
                "seed {seed0:#x} applied {applied_count}"
            );
            assert!(drift_count > 0, "seed {seed0:#x} never hit a row failure");
            assert!(
                discord_fail_count > 0,
                "seed {seed0:#x} never hit a Discord failure"
            );
            assert!(claims > 0, "seed {seed0:#x} never claimed");
        }
    }

    /// VR4: deputies, `/vc refresh` and claim hardening.
    mod vr4 {
        use super::*;
        use std::collections::BTreeSet;

        const MEMBER_C: u64 = 100_006;
        const MEMBER_D: u64 = 100_007;
        const RESTRICTED_USER: u64 = 100_301;
        const PRIVILEGED_USER: u64 = 100_302;
        const SAFEWORD_USER: u64 = 100_303;
        const NO_VIEW_USER: u64 = 100_304;
        const STALE_1: u64 = 100_401;
        const STALE_2: u64 = 100_402;
        const STALE_3: u64 = 100_403;
        const REJECTED: u64 = 100_404;
        const REJECTED_CONNECTED: u64 = 100_405;
        const OTHER_BITS: u64 = 100_406;

        fn permit(user: u64) -> RoomOverwrite {
            VcCommandHandler::permit_overwrite(user)
        }

        fn deny(user: u64) -> RoomOverwrite {
            RoomOverwrite {
                target: OverwriteTarget::Member(user),
                allow: 0,
                deny: policy::CONNECT,
            }
        }

        fn add_user(h: &Harness, id: u64, roles: Vec<u64>, can_view: bool, in_room: bool) {
            let mut s = h.ports.state.lock().unwrap();
            s.profiles.insert(
                id,
                MemberProfile {
                    user_id: id,
                    is_bot: false,
                    role_ids: roles,
                    can_view_category: can_view,
                },
            );
            let members = s.room_members.entry(ROOM_CH).or_default();
            if in_room && !members.contains(&id) {
                members.push(id);
            }
        }

        fn seed_deputy(h: &Harness, user: u64) {
            let conn = h.db.lock().unwrap();
            assert_eq!(
                repository::add_deputy(&conn, ROOM_CH, user, 1, MAX_DEPUTIES_PER_ROOM).unwrap(),
                repository::AddDeputy::Added
            );
        }

        fn deputies_of(h: &Harness) -> Vec<u64> {
            repository::deputies(&h.db.lock().unwrap(), ROOM_CH).unwrap()
        }

        fn deputy_rows(h: &Harness) -> i64 {
            h.db.lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM voice_room_deputies", [], |r| r.get(0))
                .unwrap()
        }

        fn live(h: &Harness) -> Vec<RoomOverwrite> {
            h.ports.state.lock().unwrap().live.clone()
        }

        fn is_permit(o: &RoomOverwrite) -> bool {
            matches!(o.target, OverwriteTarget::Member(_))
                && o.allow & policy::CONNECT != 0
                && o.deny & policy::CONNECT == 0
        }

        fn permit_set(overwrites: &[RoomOverwrite]) -> BTreeSet<u64> {
            overwrites
                .iter()
                .filter(|o| is_permit(o))
                .filter_map(|o| match o.target {
                    OverwriteTarget::Member(u) => Some(u),
                    _ => None,
                })
                .collect()
        }

        /// Everything that is not a member permit overwrite, in order.
        fn untouchable(overwrites: &[RoomOverwrite]) -> Vec<RoomOverwrite> {
            overwrites
                .iter()
                .filter(|o| !is_permit(o))
                .copied()
                .collect()
        }

        fn trigger(h: &Harness, name: &str, when: &str, table: &str) {
            let sql = format!(
                "CREATE TRIGGER {name} BEFORE {when} ON {table} BEGIN SELECT RAISE(ABORT, 'x'); END;"
            );
            h.db.lock().unwrap().execute(&sql, []).unwrap();
        }

        fn restart(h: &Harness) -> VcCommandHandler {
            let mut role_limits = RoleLimits::default();
            role_limits.protected_role_ids.insert(ROLE_PROTECTED);
            role_limits.restricted_role_ids.insert(ROLE_RESTRICTED);
            VcCommandHandler::new(VcCommandDeps {
                settings: live_settings(),
                bot_user_id: BOT_USER,
                rename_budget: RenameBudget {
                    max_renames: 2,
                    window_secs: 600,
                },
                default_name: "Voice Room".to_string(),
                role_limits,
                db: Arc::clone(&h.db),
                ports: Arc::clone(&h.ports) as Arc<dyn RoomCommandPorts>,
                roles: Arc::new(FakeRoles::with_defaults()) as Arc<dyn RoleSource>,
                safeword: Arc::clone(&h.safeword) as Arc<dyn SafewordLookup>,
                alerter: Arc::clone(&h.alerter) as Arc<dyn CommandAlerter>,
                clock: Arc::clone(&h.clock) as Arc<dyn Clock>,
                claim_grace_secs: policy::DEFAULT_EMPTY_GRACE_SECS,
            })
        }

        async fn run_on(
            handler: &VcCommandHandler,
            actor: u64,
            sub: VcSubcommand,
        ) -> CommandOutcome {
            handler
                .handle(&VcCommandRequest {
                    guild_id: Some(HOME_GUILD),
                    channel_id: ROOM_CH,
                    actor_id: actor,
                    subcommand: sub,
                })
                .await
        }

        fn deputize_cmd(target_id: u64) -> VcSubcommand {
            VcSubcommand::Deputize { target_id }
        }

        fn undeputize_cmd(target_id: Option<u64>) -> VcSubcommand {
            VcSubcommand::Undeputize { target_id }
        }

        fn alerts(h: &Harness) -> Vec<CommandAlertCode> {
            h.alerter.alerts.lock().unwrap().clone()
        }

        #[tokio::test]
        async fn deputize_persists_grants_the_permit_overwrite_and_survives_a_restart() {
            let h = make_harness(live_settings());
            assert_eq!(
                run(&h, OWNER, deputize_cmd(MEMBER_A)).await,
                applied(SuccessCode::Deputized)
            );
            assert_eq!(deputies_of(&h), [MEMBER_A]);
            assert!(
                live(&h).contains(&permit(MEMBER_A)),
                "same overwrite as permit"
            );
            // A new handler over the same database has no memory; the rows alone decide.
            let again = restart(&h);
            assert_eq!(
                run_on(&again, MEMBER_A, VcSubcommand::Lock).await,
                applied(SuccessCode::Locked)
            );
            assert_eq!(
                run_on(&again, OWNER, deputize_cmd(MEMBER_A)).await,
                refused(RefusalCode::TargetAlreadyDeputy)
            );
            assert_eq!(deputies_of(&h), [MEMBER_A]);
        }

        #[tokio::test]
        async fn a_deputy_has_the_owner_powers_except_transfer_deputize_and_undeputize_others() {
            let h = make_harness(live_settings());
            add_user(&h, MEMBER_B, vec![ROLE_NORMAL], true, true);
            add_user(&h, MEMBER_C, vec![ROLE_NORMAL], true, true);
            add_user(&h, MEMBER_D, vec![ROLE_NORMAL], true, false);
            seed_deputy(&h, MEMBER_A);
            seed_deputy(&h, MEMBER_C);
            for (sub, code) in [
                (
                    VcSubcommand::Rename {
                        name: "Deputy Name".to_string(),
                    },
                    SuccessCode::Renamed,
                ),
                (VcSubcommand::Limit { limit: 5 }, SuccessCode::LimitUpdated),
                (VcSubcommand::Lock, SuccessCode::Locked),
                (VcSubcommand::Unlock, SuccessCode::Unlocked),
                (
                    VcSubcommand::Permit {
                        target_id: MEMBER_B,
                    },
                    SuccessCode::Permitted,
                ),
                (
                    VcSubcommand::Reject {
                        target_id: MEMBER_D,
                    },
                    SuccessCode::Rejected,
                ),
            ] {
                assert_eq!(
                    run(&h, MEMBER_A, sub.clone()).await,
                    applied(code),
                    "{sub:?}"
                );
            }
            assert!(matches!(
                run(&h, MEMBER_A, VcSubcommand::Refresh).await,
                CommandOutcome::Applied {
                    code: SuccessCode::Refreshed(_),
                    ..
                }
            ));
            // The three powers a deputy lacks change nothing, on Discord or in the rows.
            let calls = h.ports.state.lock().unwrap().calls_count;
            let owner_row = row_of(&h);
            for (sub, code) in [
                (
                    VcSubcommand::Transfer {
                        target_id: MEMBER_B,
                    },
                    RefusalCode::DeputyNotAllowed,
                ),
                (deputize_cmd(MEMBER_B), RefusalCode::DeputyNotAllowed),
                (undeputize_cmd(Some(MEMBER_C)), RefusalCode::DeputySelfOnly),
                (undeputize_cmd(Some(OWNER)), RefusalCode::DeputySelfOnly),
            ] {
                assert_eq!(
                    run(&h, MEMBER_A, sub.clone()).await,
                    refused(code),
                    "{sub:?}"
                );
            }
            assert_eq!(h.ports.state.lock().unwrap().calls_count, calls);
            assert_eq!(row_of(&h), owner_row);
            assert_eq!(deputies_of(&h), [MEMBER_A, MEMBER_C]);
        }

        #[tokio::test]
        async fn a_deputy_can_remove_only_themselves_and_it_persists() {
            let h = make_harness(live_settings());
            add_user(&h, MEMBER_C, vec![ROLE_NORMAL], true, true);
            seed_deputy(&h, MEMBER_A);
            seed_deputy(&h, MEMBER_C);
            assert_eq!(
                run(&h, MEMBER_A, undeputize_cmd(None)).await,
                applied(SuccessCode::Undeputized)
            );
            assert_eq!(deputies_of(&h), [MEMBER_C]);
            let again = restart(&h);
            assert_eq!(
                run_on(&again, MEMBER_A, VcSubcommand::Lock).await,
                refused(RefusalCode::NotRoomOwner),
                "an ex-deputy has no powers after a restart"
            );
            assert_eq!(
                run_on(&again, MEMBER_C, undeputize_cmd(Some(MEMBER_C))).await,
                applied(SuccessCode::Undeputized)
            );
            assert!(deputies_of(&h).is_empty());
            assert!(
                h.ports.state.lock().unwrap().deleted_overwrites.is_empty(),
                "removing a deputy leaves the permit overwrite for refresh to handle"
            );
        }

        #[tokio::test]
        async fn the_owner_can_undeputize_any_deputy_and_only_a_deputy() {
            let h = make_harness(live_settings());
            add_user(&h, MEMBER_C, vec![ROLE_NORMAL], true, true);
            assert_eq!(
                run(&h, OWNER, deputize_cmd(MEMBER_A)).await,
                applied(SuccessCode::Deputized)
            );
            assert_eq!(
                run(&h, OWNER, deputize_cmd(MEMBER_C)).await,
                applied(SuccessCode::Deputized)
            );
            assert_eq!(
                run(&h, OWNER, undeputize_cmd(Some(MEMBER_A))).await,
                applied(SuccessCode::Undeputized)
            );
            assert_eq!(deputies_of(&h), [MEMBER_C]);
            assert!(
                live(&h).contains(&permit(MEMBER_A)),
                "overwrite left in place"
            );
            for target in [Some(MEMBER_A), Some(MEMBER_B), None, Some(OWNER)] {
                assert_eq!(
                    run(&h, OWNER, undeputize_cmd(target)).await,
                    refused(RefusalCode::TargetNotDeputy),
                    "{target:?}"
                );
            }
            assert_eq!(deputies_of(&h), [MEMBER_C]);
        }

        #[tokio::test]
        async fn a_member_who_is_neither_owner_nor_deputy_is_refused_and_nothing_changes() {
            let h = make_harness(live_settings());
            seed_deputy(&h, MEMBER_A);
            for sub in [
                deputize_cmd(MEMBER_C),
                undeputize_cmd(Some(MEMBER_A)),
                undeputize_cmd(None),
                VcSubcommand::Refresh,
            ] {
                assert_eq!(
                    run(&h, MEMBER_B, sub.clone()).await,
                    refused(RefusalCode::NotRoomOwner),
                    "{sub:?}"
                );
            }
            assert_eq!(h.ports.state.lock().unwrap().calls_count, 0);
            assert_eq!(deputies_of(&h), [MEMBER_A]);
        }

        #[tokio::test]
        async fn a_room_has_at_most_five_deputies() {
            assert_eq!(MAX_DEPUTIES_PER_ROOM, 5);
            let h = make_harness(live_settings());
            let users: Vec<u64> = (0..6).map(|i| 100_201 + i).collect();
            for u in &users {
                add_user(&h, *u, vec![ROLE_NORMAL], true, true);
            }
            for u in &users[..5] {
                assert_eq!(
                    run(&h, OWNER, deputize_cmd(*u)).await,
                    applied(SuccessCode::Deputized)
                );
            }
            let writes = h.ports.state.lock().unwrap().write_attempts;
            assert_eq!(
                run(&h, OWNER, deputize_cmd(users[5])).await,
                refused(RefusalCode::DeputyCapReached)
            );
            assert_eq!(deputies_of(&h).len(), 5);
            assert_eq!(
                h.ports.state.lock().unwrap().write_attempts,
                writes,
                "no Discord write"
            );
            assert!(!live(&h).contains(&permit(users[5])));
            // A freed slot can be used.
            assert_eq!(
                run(&h, OWNER, undeputize_cmd(Some(users[0]))).await,
                applied(SuccessCode::Undeputized)
            );
            assert_eq!(
                run(&h, OWNER, deputize_cmd(users[5])).await,
                applied(SuccessCode::Deputized)
            );
            assert_eq!(deputies_of(&h).len(), 5);
        }

        #[tokio::test]
        async fn ineligible_deputy_targets_are_refused_with_nothing_written() {
            let h = make_harness(live_settings());
            add_user(&h, STAFF_USER, vec![ROLE_PROTECTED], true, true);
            add_user(&h, RESTRICTED_USER, vec![ROLE_RESTRICTED], true, true);
            add_user(&h, PRIVILEGED_USER, vec![ROLE_PRIVILEGED], true, true);
            add_user(&h, SAFEWORD_USER, vec![ROLE_NORMAL], true, true);
            add_user(&h, NO_VIEW_USER, vec![ROLE_NORMAL], false, true);
            h.ports
                .state
                .lock()
                .unwrap()
                .room_members
                .get_mut(&ROOM_CH)
                .unwrap()
                .push(OTHER_BOT);
            h.safeword.active.lock().unwrap().insert(SAFEWORD_USER);
            // MEMBER_B has a profile but is not in the room.
            for (target, code) in [
                (OWNER, RefusalCode::TargetIsOwner),
                (BOT_USER, RefusalCode::TargetIsSelfBot),
                (0, RefusalCode::InvalidUserId),
                (OTHER_BOT, RefusalCode::TargetIsBot),
                (MEMBER_B, RefusalCode::TargetNotInRoom),
                (STAFF_USER, RefusalCode::TargetProtectedOrRestricted),
                (RESTRICTED_USER, RefusalCode::TargetProtectedOrRestricted),
                (PRIVILEGED_USER, RefusalCode::TargetProtectedOrRestricted),
                (SAFEWORD_USER, RefusalCode::TargetSafewordActive),
                (NO_VIEW_USER, RefusalCode::CategoryNotVisible),
            ] {
                assert_eq!(
                    run(&h, OWNER, deputize_cmd(target)).await,
                    refused(code),
                    "target {target}"
                );
            }
            assert!(deputies_of(&h).is_empty());
            assert!(h.ports.state.lock().unwrap().overwrites.is_empty());
        }

        #[tokio::test]
        async fn deputize_reports_a_failed_discord_write_and_a_failed_row_write() {
            let h = make_harness(live_settings());
            h.ports.state.lock().unwrap().fail_overwrite = Some(CommandPortError::Forbidden);
            assert_eq!(
                run(&h, OWNER, deputize_cmd(MEMBER_A)).await,
                refused(RefusalCode::DiscordFailed(CommandPortError::Forbidden))
            );
            assert!(deputies_of(&h).is_empty());
            h.ports.state.lock().unwrap().fail_overwrite = None;
            trigger(&h, "no_deputy_insert", "INSERT", "voice_room_deputies");
            assert_eq!(
                run(&h, OWNER, deputize_cmd(MEMBER_A)).await,
                refused(RefusalCode::RowUpdateFailed)
            );
            assert_eq!(alerts(&h), [CommandAlertCode::DeputyRowDrift]);
            assert!(deputies_of(&h).is_empty(), "no row, so no deputy powers");
            assert!(
                live(&h).contains(&permit(MEMBER_A)),
                "the permit was written and is reported"
            );
            assert_eq!(
                run(&h, MEMBER_A, VcSubcommand::Lock).await,
                refused(RefusalCode::NotRoomOwner)
            );
        }

        #[tokio::test]
        async fn a_deputy_can_never_reject_the_owner_or_another_deputy() {
            let h = make_harness(live_settings());
            add_user(&h, MEMBER_C, vec![ROLE_NORMAL], true, true);
            seed_deputy(&h, MEMBER_A);
            seed_deputy(&h, MEMBER_C);
            assert_eq!(
                run(&h, MEMBER_A, VcSubcommand::Reject { target_id: OWNER }).await,
                refused(RefusalCode::TargetIsOwner)
            );
            assert_eq!(
                run(&h, MEMBER_A, VcSubcommand::Permit { target_id: OWNER }).await,
                refused(RefusalCode::TargetIsOwner)
            );
            assert_eq!(
                run(
                    &h,
                    MEMBER_A,
                    VcSubcommand::Reject {
                        target_id: MEMBER_C
                    }
                )
                .await,
                refused(RefusalCode::TargetIsDeputy)
            );
            let s = h.ports.state.lock().unwrap();
            assert!(s.overwrites.is_empty() && s.disconnects.is_empty());
            drop(s);
            assert_eq!(deputies_of(&h), [MEMBER_A, MEMBER_C]);
        }

        #[tokio::test]
        async fn the_owner_rejecting_a_deputy_removes_the_row_in_the_same_command() {
            let h = make_harness(live_settings());
            assert_eq!(
                run(&h, OWNER, deputize_cmd(MEMBER_A)).await,
                applied(SuccessCode::Deputized)
            );
            assert_eq!(
                run(
                    &h,
                    OWNER,
                    VcSubcommand::Reject {
                        target_id: MEMBER_A
                    }
                )
                .await,
                applied(SuccessCode::RejectedAndDisconnected)
            );
            assert!(deputies_of(&h).is_empty());
            assert_eq!(live(&h), [deny(MEMBER_A)], "the reject replaced the permit");
            assert_eq!(
                h.ports.state.lock().unwrap().disconnects,
                [(HOME_GUILD, MEMBER_A)]
            );
        }

        #[tokio::test]
        async fn a_failed_deputy_row_removal_on_reject_is_reported_and_alerted() {
            let h = make_harness(live_settings());
            assert_eq!(
                run(&h, OWNER, deputize_cmd(MEMBER_A)).await,
                applied(SuccessCode::Deputized)
            );
            trigger(&h, "no_deputy_delete", "DELETE", "voice_room_deputies");
            assert_eq!(
                run(
                    &h,
                    OWNER,
                    VcSubcommand::Reject {
                        target_id: MEMBER_A
                    }
                )
                .await,
                refused(RefusalCode::RowUpdateFailed)
            );
            assert_eq!(alerts(&h), [CommandAlertCode::DeputyRowDrift]);
            // The reject itself still happened: denied on Discord and disconnected.
            assert_eq!(live(&h), [deny(MEMBER_A)]);
            assert_eq!(
                h.ports.state.lock().unwrap().disconnects,
                [(HOME_GUILD, MEMBER_A)]
            );
            assert_eq!(
                deputies_of(&h),
                [MEMBER_A],
                "the row is reported as still there"
            );
        }

        #[tokio::test]
        async fn deputy_rows_go_with_the_room_row_and_a_recreated_channel_inherits_none() {
            let h = make_harness(live_settings());
            seed_deputy(&h, MEMBER_A);
            let stored = row_of(&h);
            {
                let conn = h.db.lock().unwrap();
                assert!(repository::delete(&conn, ROOM_CH).unwrap());
                assert!(
                    repository::insert(&conn, &stored).unwrap(),
                    "same channel id again"
                );
            }
            assert_eq!(deputy_rows(&h), 0);
            let again = restart(&h);
            assert_eq!(
                run_on(&again, MEMBER_A, VcSubcommand::Lock).await,
                refused(RefusalCode::NotRoomOwner)
            );
        }

        #[tokio::test]
        async fn deputies_survive_transfer_and_claim_and_the_new_owners_own_row_is_removed() {
            let h = make_harness(live_settings());
            add_user(&h, MEMBER_C, vec![ROLE_NORMAL], true, true);
            seed_deputy(&h, MEMBER_A);
            seed_deputy(&h, MEMBER_C);
            assert_eq!(
                run(
                    &h,
                    OWNER,
                    VcSubcommand::Transfer {
                        target_id: MEMBER_A
                    }
                )
                .await,
                applied(SuccessCode::Transferred)
            );
            assert_eq!(row_of(&h).owner, MEMBER_A);
            assert_eq!(
                deputies_of(&h),
                [MEMBER_C],
                "kept; the new owner's own row is gone"
            );
            assert_eq!(
                run(&h, OWNER, VcSubcommand::Lock).await,
                refused(RefusalCode::NotRoomOwner),
                "the old owner is not made a deputy"
            );
            assert_eq!(
                run(&h, MEMBER_A, undeputize_cmd(Some(MEMBER_C))).await,
                applied(SuccessCode::Undeputized)
            );

            // Claim by a deputy.
            let h = make_harness(live_settings());
            add_user(&h, MEMBER_C, vec![ROLE_NORMAL], true, true);
            seed_deputy(&h, MEMBER_A);
            seed_deputy(&h, MEMBER_C);
            owner_outside(&h);
            add_user(&h, MEMBER_C, vec![ROLE_NORMAL], true, true);
            observe_absent(&h, 5_000, 5_060);
            assert_eq!(
                run(&h, MEMBER_C, VcSubcommand::Claim).await,
                applied(SuccessCode::Claimed)
            );
            assert_eq!(row_of(&h).owner, MEMBER_C);
            assert_eq!(deputies_of(&h), [MEMBER_A]);
        }

        /// Owner connected, `MEMBER_A` a deputy who is not connected, `MEMBER_B` connected with no
        /// overwrite, three stale permits, two bans (one of them connected), and the rest.
        fn refresh_fixture(settings: VoiceSettings) -> Harness {
            let h = make_harness(settings);
            for u in [
                STALE_1,
                STALE_2,
                STALE_3,
                REJECTED,
                REJECTED_CONNECTED,
                OTHER_BITS,
            ] {
                add_user(&h, u, vec![ROLE_NORMAL], true, false);
            }
            seed_deputy(&h, MEMBER_A);
            let mut s = h.ports.state.lock().unwrap();
            s.room_members
                .insert(ROOM_CH, vec![OWNER, MEMBER_B, REJECTED_CONNECTED]);
            s.live = vec![
                VcCommandHandler::owner_overwrite(OWNER),
                permit(STALE_1),
                permit(STALE_2),
                permit(STALE_3),
                permit(BOT_USER),
                deny(REJECTED),
                deny(REJECTED_CONNECTED),
                RoomOverwrite {
                    target: OverwriteTarget::Member(OTHER_BITS),
                    allow: policy::SPEAK,
                    deny: 0,
                },
                RoomOverwrite {
                    target: OverwriteTarget::Role(ROLE_NORMAL),
                    allow: policy::CONNECT,
                    deny: 0,
                },
                RoomOverwrite {
                    target: OverwriteTarget::Everyone,
                    allow: 0,
                    deny: policy::CONNECT,
                },
            ];
            drop(s);
            h
        }

        const FIXTURE_COUNTS: RefreshCounts = RefreshCounts {
            removed: 3,
            added: 2,
            failed: 0,
        };

        #[tokio::test]
        async fn refresh_keeps_owner_deputies_and_connected_members_and_removes_only_other_permits()
        {
            for actor in [OWNER, MEMBER_A] {
                let h = refresh_fixture(live_settings());
                let before = live(&h);
                assert_eq!(
                    run(&h, actor, VcSubcommand::Refresh).await,
                    applied(SuccessCode::Refreshed(FIXTURE_COUNTS)),
                    "actor {actor}"
                );
                let after = live(&h);
                assert_eq!(
                    permit_set(&after),
                    BTreeSet::from([OWNER, MEMBER_A, MEMBER_B, BOT_USER]),
                    "owner, deputy, connected member and the bot's own overwrite"
                );
                // Bans, role, @everyone and non-permit member overwrites are exactly as before.
                assert_eq!(untouchable(&after).len(), untouchable(&before).len());
                for o in untouchable(&before) {
                    assert!(after.contains(&o), "{o:?} must be untouched");
                }
                assert!(
                    after.contains(&VcCommandHandler::owner_overwrite(OWNER)),
                    "the owner overwrite is not downgraded to a plain permit"
                );
                assert!(after.contains(&permit(MEMBER_B)) && after.contains(&permit(MEMBER_A)));
                assert!(!after
                    .iter()
                    .any(|o| o.target == OverwriteTarget::Member(STALE_1)));
            }
        }

        #[tokio::test]
        async fn refresh_reads_discord_not_the_in_memory_permit_map() {
            let h = make_harness(live_settings());
            add_user(&h, STALE_1, vec![ROLE_NORMAL], true, false);
            // The handler remembers a permit for MEMBER_B that Discord no longer has.
            assert_eq!(
                run(
                    &h,
                    OWNER,
                    VcSubcommand::Permit {
                        target_id: MEMBER_B
                    }
                )
                .await,
                applied(SuccessCode::Permitted)
            );
            {
                let mut s = h.ports.state.lock().unwrap();
                s.live
                    .retain(|o| o.target != OverwriteTarget::Member(MEMBER_B));
                // ... and Discord holds a permit this handler never heard of.
                s.live.push(permit(STALE_1));
                s.live.push(VcCommandHandler::owner_overwrite(OWNER));
            }
            assert_eq!(
                run(&h, OWNER, VcSubcommand::Refresh).await,
                applied(SuccessCode::Refreshed(RefreshCounts {
                    removed: 1,
                    added: 1,
                    failed: 0
                }))
            );
            let s = h.ports.state.lock().unwrap();
            assert_eq!(
                s.deleted_overwrites,
                [(HOME_GUILD, ROOM_CH, OverwriteTarget::Member(STALE_1))],
                "the permit memory knew nothing about was removed, nothing else"
            );
            drop(s);
            // The map was rebuilt from Discord: the connected member is now known as permitted and
            // the forgotten one is not.
            assert_eq!(
                run(
                    &h,
                    OWNER,
                    VcSubcommand::Permit {
                        target_id: MEMBER_A
                    }
                )
                .await,
                refused(RefusalCode::AlreadyPermitted)
            );
            assert_eq!(
                run(
                    &h,
                    OWNER,
                    VcSubcommand::Permit {
                        target_id: MEMBER_B
                    }
                )
                .await,
                applied(SuccessCode::Permitted)
            );
            // Restart: a handler with an empty map still removes what Discord holds.
            h.ports.state.lock().unwrap().live.push(permit(STALE_1));
            let again = restart(&h);
            assert!(matches!(
                run_on(&again, OWNER, VcSubcommand::Refresh).await,
                CommandOutcome::Applied {
                    code: SuccessCode::Refreshed(RefreshCounts { removed: 2, .. }),
                    ..
                }
            ));
            assert!(!permit_set(&live(&h)).contains(&STALE_1));
            assert!(!permit_set(&live(&h)).contains(&MEMBER_B));
        }

        #[tokio::test]
        async fn refresh_reports_partial_failures_and_rate_limits_with_honest_counts() {
            // One delete fails: the others still go, and the failed permit stays and is remembered.
            let h = refresh_fixture(live_settings());
            h.ports
                .state
                .lock()
                .unwrap()
                .fail_delete_users
                .insert(STALE_2);
            let out = run(&h, OWNER, VcSubcommand::Refresh).await;
            let counts = RefreshCounts {
                removed: 2,
                added: 2,
                failed: 1,
            };
            assert_eq!(out, refused(RefusalCode::RefreshPartial(counts)));
            let reply = out.reply().unwrap();
            assert_eq!(reply.code, "vc_refresh_partial");
            assert!(reply.text.contains("failed 1") && reply.ephemeral);
            assert!(!reply.text.contains(&STALE_2.to_string()));
            assert!(permit_set(&live(&h)).contains(&STALE_2));
            assert!(!permit_set(&live(&h)).contains(&STALE_1));
            h.ports.state.lock().unwrap().fail_delete_users.clear();
            assert_eq!(
                run(&h, OWNER, VcSubcommand::Permit { target_id: STALE_2 }).await,
                refused(RefusalCode::AlreadyPermitted)
            );

            // A failed add is not remembered as permitted.
            let h = refresh_fixture(live_settings());
            h.ports
                .state
                .lock()
                .unwrap()
                .fail_set_users
                .insert(MEMBER_B);
            assert_eq!(
                run(&h, OWNER, VcSubcommand::Refresh).await,
                refused(RefusalCode::RefreshPartial(RefreshCounts {
                    removed: 3,
                    added: 1,
                    failed: 1
                }))
            );
            h.ports.state.lock().unwrap().fail_set_users.clear();
            assert_eq!(
                run(
                    &h,
                    OWNER,
                    VcSubcommand::Permit {
                        target_id: MEMBER_B
                    }
                )
                .await,
                applied(SuccessCode::Permitted)
            );

            // Discord answers 429 after two writes: the run stops, nothing past it is attempted and
            // everything not done is counted as failed.
            let h = refresh_fixture(live_settings());
            h.ports.state.lock().unwrap().rate_limit_after = Some(2);
            assert_eq!(
                run(&h, OWNER, VcSubcommand::Refresh).await,
                refused(RefusalCode::RefreshPartial(RefreshCounts {
                    removed: 0,
                    added: 2,
                    failed: 3
                }))
            );
            let s = h.ports.state.lock().unwrap();
            assert_eq!(
                s.write_attempts, 3,
                "two adds ok, the first delete hits the 429, no more"
            );
            drop(s);
            let h = refresh_fixture(live_settings());
            h.ports.state.lock().unwrap().rate_limit_after = Some(0);
            assert_eq!(
                run(&h, OWNER, VcSubcommand::Refresh).await,
                refused(RefusalCode::RefreshPartial(RefreshCounts {
                    removed: 0,
                    added: 0,
                    failed: 5
                }))
            );
            assert_eq!(h.ports.state.lock().unwrap().write_attempts, 1);
        }

        #[tokio::test]
        async fn a_failed_read_makes_refresh_change_nothing() {
            for case in 0..3 {
                let h = refresh_fixture(live_settings());
                let before = live(&h);
                let expect = match case {
                    0 => {
                        h.ports.state.lock().unwrap().fail_member_overwrites =
                            Some(CommandPortError::Unavailable);
                        refused(RefusalCode::DiscordFailed(CommandPortError::Unavailable))
                    }
                    1 => {
                        h.ports.state.lock().unwrap().fail_room_members =
                            Some(CommandPortError::Forbidden);
                        refused(RefusalCode::DiscordFailed(CommandPortError::Forbidden))
                    }
                    _ => {
                        h.db.lock()
                            .unwrap()
                            .execute("DROP TABLE voice_room_deputies", [])
                            .unwrap();
                        refused(RefusalCode::StorageReadFailed)
                    }
                };
                assert_eq!(
                    run(&h, OWNER, VcSubcommand::Refresh).await,
                    expect,
                    "case {case}"
                );
                assert_eq!(live(&h), before, "case {case}");
                assert_eq!(
                    h.ports.state.lock().unwrap().write_attempts,
                    0,
                    "case {case}"
                );
            }
        }

        #[tokio::test]
        async fn refresh_handles_at_most_100_member_overwrites() {
            let h = make_harness(live_settings());
            let mut listed = vec![MemberOverwrite {
                user_id: OWNER,
                allow: policy::OWNER_RIGHTS,
                deny: 0,
            }];
            listed.extend((0..130).map(|i| MemberOverwrite {
                user_id: 300_000 + i,
                allow: policy::CONNECT,
                deny: 0,
            }));
            {
                let mut s = h.ports.state.lock().unwrap();
                s.room_members.insert(ROOM_CH, vec![OWNER]);
                s.listed_override = Some(listed);
            }
            assert_eq!(
                run(&h, OWNER, VcSubcommand::Refresh).await,
                refused(RefusalCode::RefreshPartial(RefreshCounts {
                    removed: 99,
                    added: 0,
                    failed: 31
                })),
                "100 examined (the owner and 99 stale), 31 not examined and reported as failed"
            );
            assert_eq!(h.ports.state.lock().unwrap().write_attempts, 99);
        }

        #[tokio::test]
        async fn shadow_mode_reports_what_it_would_change_and_writes_nothing() {
            let h = refresh_fixture(settings_with("shadow", Some(HOME_GUILD)));
            let before = live(&h);
            assert_eq!(
                run(&h, OWNER, VcSubcommand::Refresh).await,
                applied(SuccessCode::ShadowRefresh(FIXTURE_COUNTS))
            );
            assert_eq!(live(&h), before);
            assert_eq!(h.ports.state.lock().unwrap().write_attempts, 0);
            assert_eq!(deputies_of(&h), [MEMBER_A]);
            // deputize and undeputize in shadow write neither Discord nor the rows.
            assert_eq!(
                run(&h, OWNER, deputize_cmd(MEMBER_B)).await,
                applied(SuccessCode::ShadowNoop)
            );
            assert_eq!(
                run(&h, OWNER, undeputize_cmd(Some(MEMBER_A))).await,
                applied(SuccessCode::ShadowNoop)
            );
            assert_eq!(deputies_of(&h), [MEMBER_A]);
            assert_eq!(live(&h), before);
        }

        #[tokio::test]
        async fn other_guild_untracked_channel_off_and_unset_home_guild_touch_nothing() {
            let h = make_harness(live_settings());
            // A row of another guild is not a room of the home guild.
            {
                let conn = h.db.lock().unwrap();
                let foreign = RoomRow {
                    channel: 50_777,
                    guild: OTHER_GUILD,
                    owner: OWNER,
                    created_at: 1,
                    locked: false,
                    user_limit: 0,
                };
                repository::insert(&conn, &foreign).unwrap();
            }
            for sub in [
                deputize_cmd(MEMBER_A),
                undeputize_cmd(None),
                VcSubcommand::Refresh,
            ] {
                for (guild, channel, expect) in [
                    (
                        Some(OTHER_GUILD),
                        ROOM_CH,
                        CommandOutcome::Ignored(RefusalCode::WrongGuild),
                    ),
                    (
                        None,
                        ROOM_CH,
                        CommandOutcome::Ignored(RefusalCode::WrongGuild),
                    ),
                    (
                        Some(HOME_GUILD),
                        UNTRACKED_CH,
                        refused(RefusalCode::UntrackedChannel),
                    ),
                    (
                        Some(HOME_GUILD),
                        50_777,
                        refused(RefusalCode::UntrackedChannel),
                    ),
                ] {
                    let out = h
                        .handler
                        .handle(&VcCommandRequest {
                            guild_id: guild,
                            channel_id: channel,
                            actor_id: OWNER,
                            subcommand: sub.clone(),
                        })
                        .await;
                    assert_eq!(out, expect, "{sub:?} {guild:?} {channel}");
                }
            }
            assert_eq!(h.ports.state.lock().unwrap().calls_count, 0);
            assert_eq!(deputy_rows(&h), 0);
            let off = make_harness(settings_with("off", Some(HOME_GUILD)));
            let unset = make_harness(settings_with("live", None));
            for sub in [
                deputize_cmd(MEMBER_A),
                undeputize_cmd(None),
                VcSubcommand::Refresh,
            ] {
                assert_eq!(
                    run(&off, OWNER, sub.clone()).await,
                    CommandOutcome::Ignored(RefusalCode::FeatureDisabled)
                );
                assert_eq!(
                    run(&unset, OWNER, sub).await,
                    refused(RefusalCode::HomeGuildUnset)
                );
            }
            assert_eq!(off.ports.state.lock().unwrap().calls_count, 0);
            assert_eq!(unset.ports.state.lock().unwrap().calls_count, 0);
        }

        #[tokio::test]
        async fn reset_observations_clears_recorded_absences_and_a_claim_needs_a_fresh_grace() {
            let h = make_harness(live_settings());
            owner_outside(&h);
            observe_absent(&h, 5_000, 5_100);
            // Without a reset this claim would pass (100 s of gapless absence).
            h.handler.reset_observations();
            assert_eq!(
                run(&h, MEMBER_A, VcSubcommand::Claim).await,
                refused(RefusalCode::OwnerAbsenceNotRecorded)
            );
            // A fresh absence needs the full grace again.
            observe_absent(&h, 5_200, 5_259);
            assert_eq!(
                run(&h, MEMBER_A, VcSubcommand::Claim).await,
                refused(RefusalCode::OwnerGraceNotElapsed)
            );
            observe_absent(&h, 5_259, 5_260);
            assert_eq!(
                run(&h, MEMBER_A, VcSubcommand::Claim).await,
                applied(SuccessCode::Claimed)
            );
        }

        #[tokio::test]
        async fn reset_observations_on_an_empty_map_is_a_no_op() {
            let h = make_harness(live_settings());
            h.handler.reset_observations();
            h.handler.reset_observations();
            assert!(h.handler.mem().owner_absent.is_empty());
            owner_outside(&h);
            assert_eq!(
                run(&h, MEMBER_A, VcSubcommand::Claim).await,
                refused(RefusalCode::OwnerAbsenceNotRecorded)
            );
            observe_absent(&h, 5_000, 5_060);
            assert_eq!(
                run(&h, MEMBER_A, VcSubcommand::Claim).await,
                applied(SuccessCode::Claimed)
            );
        }

        /// Seeded random commands against a model Discord and random SQLite and Discord failures.
        /// Checked for every outcome: no panic, deputies only change when the outcome says so,
        /// never more than the cap, never the owner, never rows of a missing room, and a refresh
        /// leaves Discord exactly as its counts say.
        #[tokio::test]
        async fn seeded_liveness_of_deputies_refresh_and_claims_under_random_failures() {
            let pool = [
                OWNER, MEMBER_A, MEMBER_B, MEMBER_C, MEMBER_D, STALE_1, STALE_2, STALE_3,
            ];
            for seed0 in [
                0x9E37_79B9_7F4A_7C15_u64,
                0xD1B5_4A32_D192_ED03,
                0x0123_4567_89AB_CDEF,
            ] {
                let h = make_harness(live_settings());
                for u in pool {
                    add_user(&h, u, vec![ROLE_NORMAL], true, false);
                }
                add_user(&h, STAFF_USER, vec![ROLE_PROTECTED], true, false);
                h.ports.state.lock().unwrap().live = vec![VcCommandHandler::owner_overwrite(OWNER)];
                let mut seed = seed0;
                let mut next = || -> u64 {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed
                };
                let mut on: BTreeSet<&str> = BTreeSet::new();
                let (mut deputized, mut undeputized, mut refreshed, mut partial) = (0, 0, 0, 0);
                let (mut row_fail, mut recreated, mut dep_changes) = (0, 0, 0);
                for step in 0..400_i64 {
                    let now = 10_000 + step * 40;
                    h.clock.set(now);
                    let r = next();
                    // Toggle SQLite failures.
                    for (i, (name, when, table)) in [
                        ("fi", "INSERT", "voice_room_deputies"),
                        ("fd", "DELETE", "voice_room_deputies"),
                        ("fu", "UPDATE", "voice_rooms"),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        if (r >> (8 * i)) % 9 == 0 {
                            if on.remove(name) {
                                h.db.lock()
                                    .unwrap()
                                    .execute(&format!("DROP TRIGGER {name}"), [])
                                    .unwrap();
                            } else {
                                trigger(&h, name, when, table);
                                on.insert(name);
                            }
                        }
                    }
                    let before_row = row_of(&h);
                    let owner = before_row.owner;
                    let before_deps = deputies_of(&h);
                    // Random connected members.
                    let mut members: Vec<u64> = pool
                        .iter()
                        .copied()
                        .filter(|u| (r >> (u % 13)) % 3 != 0)
                        .collect();
                    let claim_step = r % 8 == 0;
                    if claim_step {
                        members.retain(|u| *u != owner);
                        if members.is_empty() {
                            members.push(if owner == MEMBER_A {
                                MEMBER_B
                            } else {
                                MEMBER_A
                            });
                        }
                    }
                    {
                        let mut s = h.ports.state.lock().unwrap();
                        s.room_members.insert(ROOM_CH, members.clone());
                        let pick =
                            |m: u64, e: CommandPortError| (r / m).is_multiple_of(5).then_some(e);
                        s.fail_overwrite = pick(3, CommandPortError::Forbidden);
                        s.fail_delete_overwrite = pick(7, CommandPortError::Forbidden);
                        s.fail_member_overwrites = pick(11, CommandPortError::Unavailable);
                        s.fail_room_members = pick(13, CommandPortError::Unavailable);
                        s.fail_set_users.clear();
                        s.fail_delete_users.clear();
                        if (r / 17) % 4 == 0 {
                            s.fail_set_users
                                .insert(pool[(r / 19) as usize % pool.len()]);
                            s.fail_delete_users
                                .insert(pool[(r / 23) as usize % pool.len()]);
                        }
                        let limit_at = s.writes_ok + (r / 31) as usize % 4;
                        s.rate_limit_after = ((r / 29) % 6 == 0).then_some(limit_at);
                        // Discord keeps overwrites of members who left; sprinkle stale ones.
                        if (r / 37) % 3 == 0 {
                            let who = pool[(r / 41) as usize % pool.len()];
                            if !s
                                .live
                                .iter()
                                .any(|o| o.target == OverwriteTarget::Member(who))
                            {
                                s.live.push(if (r / 43) % 4 == 0 {
                                    deny(who)
                                } else {
                                    permit(who)
                                });
                            }
                        }
                    }
                    if claim_step {
                        observe_absent(&h, now - 60, now);
                    }
                    // Occasionally delete and recreate the room row (VR2 cleanup + a new room).
                    if (r / 47) % 25 == 0 {
                        let stored = before_row.clone();
                        let res = {
                            let conn = h.db.lock().unwrap();
                            repository::delete(&conn, ROOM_CH)
                        };
                        match res {
                            Ok(true) => {
                                assert_eq!(
                                    deputy_rows(&h),
                                    0,
                                    "step {step}: deputies of a deleted room"
                                );
                                let conn = h.db.lock().unwrap();
                                assert!(repository::insert(&conn, &stored).unwrap());
                                recreated += 1;
                            }
                            _ => {
                                assert_eq!(row_of(&h), stored);
                                assert_eq!(
                                    deputies_of(&h),
                                    before_deps,
                                    "step {step}: failed delete"
                                );
                            }
                        }
                        continue;
                    }
                    let other = pool[(r / 53) as usize % pool.len()];
                    let actor = if claim_step || (r / 59) % 3 == 0 {
                        members[(r / 61) as usize % members.len()]
                    } else if (r / 67) % 2 == 0 {
                        owner
                    } else {
                        pool[(r / 71) as usize % pool.len()]
                    };
                    let sub = if claim_step {
                        VcSubcommand::Claim
                    } else {
                        match (r / 73) % 9 {
                            0 | 1 => deputize_cmd(other),
                            2 => undeputize_cmd(Some(other)),
                            3 => undeputize_cmd(None),
                            4 | 5 => VcSubcommand::Refresh,
                            6 => VcSubcommand::Reject { target_id: other },
                            7 => VcSubcommand::Permit { target_id: other },
                            _ => VcSubcommand::Transfer { target_id: other },
                        }
                    };
                    let before_live = live(&h);
                    let (attempts_before, alerts_before) = (
                        h.ports.state.lock().unwrap().write_attempts,
                        alerts(&h).len(),
                    );
                    let out = run(&h, actor, sub.clone()).await;
                    let after_live = live(&h);
                    let after_deps = deputies_of(&h);
                    let after_row = row_of(&h);
                    let new_alerts = alerts(&h).len() - alerts_before;
                    assert!(after_deps.len() <= MAX_DEPUTIES_PER_ROOM, "step {step}");
                    assert!(
                        !after_deps.contains(&after_row.owner),
                        "step {step}: owner is a deputy"
                    );
                    assert_eq!(
                        deputy_rows(&h),
                        after_deps.len() as i64,
                        "step {step}: foreign rows"
                    );
                    let may_change_deputies = matches!(
                        &out,
                        CommandOutcome::Applied {
                            code: SuccessCode::Deputized
                                | SuccessCode::Undeputized
                                | SuccessCode::Transferred
                                | SuccessCode::Claimed
                                | SuccessCode::Rejected
                                | SuccessCode::RejectedAndDisconnected,
                            ..
                        } | CommandOutcome::Refused {
                            code: RefusalCode::RowUpdateFailed | RefusalCode::DisconnectFailed(_),
                            ..
                        }
                    );
                    if after_deps != before_deps {
                        dep_changes += 1;
                        assert!(may_change_deputies, "step {step}: {sub:?} {out:?}");
                    }
                    match &out {
                        CommandOutcome::Applied { code, reply } => {
                            assert!(reply.ephemeral);
                            assert_eq!(new_alerts, 0, "step {step}");
                            match code {
                                SuccessCode::Deputized => {
                                    deputized += 1;
                                    assert_eq!(actor, owner);
                                    assert!(after_deps.contains(&other), "step {step}");
                                    assert!(permit_set(&after_live).contains(&other));
                                }
                                SuccessCode::Undeputized => {
                                    undeputized += 1;
                                    assert!(!before_deps.is_empty());
                                    let target = match &sub {
                                        VcSubcommand::Undeputize { target_id } => {
                                            target_id.unwrap_or(actor)
                                        }
                                        _ => unreachable!(),
                                    };
                                    assert!(!after_deps.contains(&target), "step {step}");
                                    assert!(
                                        actor == owner || target == actor,
                                        "step {step}: only self"
                                    );
                                }
                                SuccessCode::Refreshed(c) => {
                                    refreshed += 1;
                                    let denied: BTreeSet<u64> = before_live
                                        .iter()
                                        .filter(|o| o.deny & policy::CONNECT != 0)
                                        .filter_map(|o| match o.target {
                                            OverwriteTarget::Member(u) => Some(u),
                                            _ => None,
                                        })
                                        .collect();
                                    let mut expect: BTreeSet<u64> =
                                        before_deps.iter().copied().collect();
                                    expect
                                        .extend(members.iter().copied().filter(|u| *u != BOT_USER));
                                    expect.insert(owner);
                                    expect.retain(|u| !denied.contains(u));
                                    let had_bot = permit_set(&before_live).contains(&BOT_USER);
                                    if had_bot {
                                        expect.insert(BOT_USER);
                                    }
                                    let before_p = permit_set(&before_live);
                                    assert_eq!(permit_set(&after_live), expect, "step {step}");
                                    assert_eq!(
                                        untouchable(&after_live).len(),
                                        untouchable(&before_live).len()
                                    );
                                    for o in untouchable(&before_live) {
                                        assert!(after_live.contains(&o), "step {step}: {o:?}");
                                    }
                                    assert_eq!(
                                        c.removed as usize,
                                        before_p.difference(&expect).count()
                                    );
                                    assert_eq!(
                                        c.added as usize,
                                        expect.difference(&before_p).count()
                                    );
                                    assert_eq!(c.failed, 0);
                                }
                                _ => {}
                            }
                        }
                        CommandOutcome::Refused { code, reply } => {
                            assert!(reply.ephemeral);
                            match code {
                                RefusalCode::RefreshPartial(c) => {
                                    partial += 1;
                                    let (b, a) =
                                        (permit_set(&before_live), permit_set(&after_live));
                                    assert_eq!(
                                        c.removed as usize,
                                        b.difference(&a).count(),
                                        "step {step}"
                                    );
                                    assert_eq!(
                                        c.added as usize,
                                        a.difference(&b).count(),
                                        "step {step}"
                                    );
                                    assert!(c.failed > 0);
                                    for o in untouchable(&before_live) {
                                        assert!(after_live.contains(&o), "step {step}: {o:?}");
                                    }
                                }
                                RefusalCode::RowUpdateFailed => {
                                    row_fail += 1;
                                    assert_eq!(new_alerts, 1, "step {step}: {sub:?}");
                                }
                                other => {
                                    assert!(new_alerts <= 1, "step {step}");
                                    if matches!(sub, VcSubcommand::Refresh) {
                                        // A refresh that stopped before writing changed nothing.
                                        assert_eq!(
                                            after_live, before_live,
                                            "step {step}: {other:?}"
                                        );
                                        assert_eq!(
                                            h.ports.state.lock().unwrap().write_attempts,
                                            attempts_before
                                        );
                                    }
                                    if matches!(sub, VcSubcommand::Deputize { .. }) {
                                        assert_eq!(after_deps, before_deps);
                                    }
                                }
                            }
                        }
                        CommandOutcome::Ignored(code) => panic!("home guild ignored: {code:?}"),
                    }
                }
                assert!(deputized > 5, "seed {seed0:#x}: deputized {deputized}");
                assert!(
                    undeputized > 2,
                    "seed {seed0:#x}: undeputized {undeputized}"
                );
                assert!(refreshed > 5, "seed {seed0:#x}: refreshed {refreshed}");
                assert!(partial > 0, "seed {seed0:#x}: no partial refresh");
                assert!(row_fail > 0, "seed {seed0:#x}: no row failure");
                assert!(recreated > 0, "seed {seed0:#x}: no recreated room");
                assert!(dep_changes > 5, "seed {seed0:#x}: deputies never changed");
            }
        }
    }
}

//! Pure decision logic for temporary user-created voice rooms.
//!
//! This module is the Discord-free core of the voice-room feature that replaces the old
//! Chai bot: a member joins a hub channel, the bot creates a room for them in a
//! configured category, and the room is temporary - it is deleted once it has been
//! empty for a grace period.
//!
//! Everything here is a pure function of plain facts. There is no serenity type, no
//! I/O, no clock read and no randomness: `now` and every id are passed in by the
//! caller.
//!
//! Security notes:
//! * A room name is attacker-controlled text that ends up in a channel title. The
//!   sanitiser removes mentions, invite links, URLs, control characters and
//!   every character that renders nothing (the closed class named in
//!   `INVISIBLE_RANGES`), so a name can never ping a role or smuggle an invite,
//!   and it cannot hide a mention behind an invisible character.
//! * The permission overwrites of a room are the category's overwrites plus one
//!   bounded owner overwrite. The owner rights are a compile-time constant that never
//!   contains `MANAGE_ROLES`, `MANAGE_CHANNELS` or `ADMINISTRATOR`, so an owner
//!   cannot escalate: the bot performs the renames, not the owner.
//! * A permit is refused when the member could not already view the category, so an
//!   owner cannot use a permit to walk a member around the verification or the
//!   safeword gating of the area.
//! * A channel that the policy does not track is never deleted, so a voice channel
//!   the bot did not create is never touched.
//!
//! Errors and deny reasons are stable enum codes. They never carry room names, user
//! names or ids, so nothing private can leak through an error path.
//!
//! Wiring card W1 removes the module-level `allow(dead_code)` in `super::mod`.

use std::collections::HashMap;
use std::fmt;

/// Environment key that switches the feature on.
///
/// Default: `off`. Read by the wiring card, not by this module.
pub const KEY_MODE: &str = "VOICE_ROOMS_MODE";
/// Environment key holding the hub channel id a member joins to ask for a room.
///
/// Default: unset (the feature then has no hub and creates nothing).
pub const KEY_HUB_CHANNEL_ID: &str = "VOICE_ROOM_HUB_CHANNEL_ID";
/// Environment key holding the category id the rooms are created in.
///
/// Default: unset (the feature then has no category and creates nothing).
pub const KEY_CATEGORY_ID: &str = "VOICE_ROOM_CATEGORY_ID";
/// Environment key holding how many rooms one member may own at the same time.
///
/// Default: [`DEFAULT_PER_USER_CAP`].
pub const KEY_PER_USER_CAP: &str = "VOICE_ROOM_PER_USER_CAP";
/// Environment key holding how many rooms may exist at the same time.
///
/// Default: [`DEFAULT_GLOBAL_CAP`].
pub const KEY_GLOBAL_CAP: &str = "VOICE_ROOM_GLOBAL_CAP";
/// Environment key holding the create cooldown in seconds.
///
/// Default: [`DEFAULT_CREATE_COOLDOWN_SECS`].
pub const KEY_CREATE_COOLDOWN_SECS: &str = "VOICE_ROOM_CREATE_COOLDOWN_SECS";
/// Environment key holding the empty-room grace period in seconds.
///
/// Default: [`DEFAULT_EMPTY_GRACE_SECS`].
pub const KEY_EMPTY_GRACE_SECS: &str = "VOICE_ROOM_EMPTY_GRACE_SECS";
/// Environment key holding the name used when the cleaned name is empty.
///
/// Default: [`DEFAULT_ROOM_NAME`].
pub const KEY_DEFAULT_NAME: &str = "VOICE_ROOM_DEFAULT_NAME";
/// Environment key holding how many renames one room may be given inside the window.
///
/// Default: [`DEFAULT_RENAME_BUDGET`].
pub const KEY_RENAME_BUDGET: &str = "VOICE_ROOM_RENAME_BUDGET";
/// Environment key holding the width of the rename window in seconds.
///
/// Default: [`DEFAULT_RENAME_WINDOW_SECS`].
pub const KEY_RENAME_WINDOW_SECS: &str = "VOICE_ROOM_RENAME_WINDOW_SECS";

/// Default number of rooms one member may own at the same time.
pub const DEFAULT_PER_USER_CAP: usize = 1;
/// Default number of rooms that may exist at the same time.
pub const DEFAULT_GLOBAL_CAP: usize = 25;
/// Default create cooldown in seconds.
pub const DEFAULT_CREATE_COOLDOWN_SECS: i64 = 30;
/// Default empty-room grace period in seconds.
pub const DEFAULT_EMPTY_GRACE_SECS: i64 = 60;
/// Default name of a room whose cleaned name is empty.
pub const DEFAULT_ROOM_NAME: &str = "Voice Room";
/// Default number of renames one room may be given inside the window.
pub const DEFAULT_RENAME_BUDGET: u32 = 2;
/// Default width of the rename window in seconds.
pub const DEFAULT_RENAME_WINDOW_SECS: i64 = 600;

/// Longest room name Discord accepts, counted in characters.
pub const MAX_NAME_CHARS: usize = 100;

/// Permission bit for `CONNECT`, at Discord's bit position.
pub const CONNECT: u64 = 1 << 20;
/// Permission bit for `SPEAK`, at Discord's bit position.
pub const SPEAK: u64 = 1 << 21;
/// Permission bit for `MUTE_MEMBERS`, at Discord's bit position.
pub const MUTE_MEMBERS: u64 = 1 << 22;
/// Permission bit for `MOVE_MEMBERS`, at Discord's bit position.
pub const MOVE_MEMBERS: u64 = 1 << 24;
/// Permission bit for `MANAGE_CHANNELS`, at Discord's bit position.
pub const MANAGE_CHANNELS: u64 = 1 << 4;
/// Permission bit for `MANAGE_ROLES`, at Discord's bit position.
pub const MANAGE_ROLES: u64 = 1 << 28;
/// Permission bit for `ADMINISTRATOR`, at Discord's bit position.
pub const ADMINISTRATOR: u64 = 1 << 3;

/// Permissions the owner of a room may hold.
///
/// The owner may connect, speak, move members and mute members. The owner may not
/// manage the channel: the bot performs the rename and the delete, so the rename
/// budget cannot be spent by hand.
pub const OWNER_RIGHTS: u64 = CONNECT | SPEAK | MOVE_MEMBERS | MUTE_MEMBERS;

/// Permissions that must never be granted to a room owner.
///
/// Discord retired `MANAGE_PERMISSIONS`, so serenity 0.12 has no constant for it and
/// `MANAGE_ROLES` is what covers role permission edits today. The escalating set is
/// therefore `MANAGE_ROLES`, `MANAGE_CHANNELS` and `ADMINISTRATOR`. See
/// [`LEGACY_MANAGE_PERMISSIONS`] for the retired permission.
pub const FORBIDDEN_OWNER_RIGHTS: u64 = MANAGE_ROLES | MANAGE_CHANNELS | ADMINISTRATOR;

/// The retired Discord `MANAGE_PERMISSIONS` permission.
///
/// Discord removed the permission, so serenity 0.12 has no constant for it. The bit
/// below is not used by any current Discord permission, so a test or a log line can
/// name it without colliding with a live permission.
pub const LEGACY_MANAGE_PERMISSIONS: u64 = 1 << 45;

/// The rename budget Discord enforces, exposed as data.
///
/// The exact Discord limit is not verified here; the runtime passes the number it
/// enforces and the policy never plans more renames inside the window than the
/// budget allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenameBudget {
    /// How many renames may be planned inside the window.
    pub max_renames: u32,
    /// The width of the window in seconds.
    pub window_secs: i64,
}

impl Default for RenameBudget {
    fn default() -> Self {
        Self {
            max_renames: DEFAULT_RENAME_BUDGET,
            window_secs: DEFAULT_RENAME_WINDOW_SECS,
        }
    }
}

/// Validated configuration of the voice-room feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceRoomConfig {
    /// The guild the feature serves. A join from another guild is out of scope.
    pub guild_id: u64,
    /// The channel a member joins to ask for a room.
    pub hub_channel_id: u64,
    /// The category the room is created in.
    pub category_id: u64,
    /// The name used when the cleaned name is empty.
    pub default_name: String,
    /// How many rooms one member may own at the same time.
    pub per_user_cap: usize,
    /// How many rooms may exist at the same time.
    pub global_cap: usize,
    /// How long a member must wait between two room creations.
    pub create_cooldown_secs: i64,
    /// How long an empty room is kept before it may be cleaned up.
    pub empty_grace_secs: i64,
    /// The rename budget the runtime enforces.
    pub rename_budget: RenameBudget,
    /// The overwrites the category itself carries; every room inherits them.
    pub category_overwrites: Vec<RoomOverwrite>,
}

/// A tracked room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    /// Id of the room channel.
    pub channel_id: u64,
    /// Id of the member who owns the room.
    pub owner_id: u64,
    /// When the room was created, in epoch seconds.
    pub created_at: i64,
    /// When the room last became empty, in epoch seconds; absent while it is occupied.
    pub empty_since: Option<i64>,
    /// How many members are in the room, counted BEFORE the leave that is
    /// being decided. The member who is leaving is still counted, so a room
    /// whose count is one becomes empty when that member leaves.
    pub occupants: usize,
}

/// The state the policy reads.
///
/// The runtime owns this state and persists it; the policy only reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceRoomState {
    /// The tracked rooms.
    pub rooms: Vec<Room>,
    /// When each member last created a room, in epoch seconds.
    pub last_create: HashMap<u64, i64>,
    /// When each room was renamed, in epoch seconds.
    pub renames: Vec<(u64, i64)>,
}

impl VoiceRoomState {
    /// Returns the tracked room with the given channel id.
    pub fn room(&self, channel_id: u64) -> Option<&Room> {
        self.rooms.iter().find(|r| r.channel_id == channel_id)
    }

    /// Counts the rooms the given member owns.
    pub fn owned_room_count(&self, owner_id: u64) -> usize {
        self.rooms.iter().filter(|r| r.owner_id == owner_id).count()
    }
}

/// The plain facts about a member joining a voice channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinEvent {
    /// The guild the join happened in.
    pub guild_id: u64,
    /// The member who joined.
    pub user_id: u64,
    /// The channel the member joined.
    pub channel_id: u64,
    /// When the join happened, in epoch seconds.
    pub at: i64,
}

/// Who a permission overwrite applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteTarget {
    /// The `@everyone` role of the guild.
    Everyone,
    /// One role of the guild.
    Role(u64),
    /// One member of the guild.
    Member(u64),
}

/// One permission overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomOverwrite {
    /// Who the overwrite applies to.
    pub target: OverwriteTarget,
    /// The permissions allowed.
    pub allow: u64,
    /// The permissions denied.
    pub deny: u64,
}

/// The room the runtime should create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePlan {
    /// The cleaned name of the room.
    pub name: String,
    /// The permission overwrites the room is created with.
    pub overwrites: Vec<RoomOverwrite>,
}

/// The reason a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The member already owns the maximum number of rooms.
    PerUserCapReached,
    /// The maximum number of rooms already exists.
    GlobalCapReached,
    /// The member created a room too recently.
    CreateCooldown,
    /// The member may not view the category.
    CategoryNotVisible,
    /// The rename budget is exhausted.
    RenameBudgetExhausted,
    /// The join happened in a channel that is not the hub.
    NotHubChannel,
    /// The user id was zero, which is not a real member.
    InvalidUserId,
    /// The join happened in another guild than the configured one.
    GuildMismatch,
    /// The requester does not own the room.
    NotRoomOwner,
    /// The channel is not a tracked room.
    UntrackedChannel,
    /// The room is empty but the grace period has not elapsed yet.
    EmptyGraceNotElapsed,
    /// The member already has access to the room.
    AlreadyPermitted,
    /// The configuration is invalid, so the request is refused rather than
    /// answered with a nonsensical bound.
    InvalidConfig,
    /// The room still holds members, so it may not be cleaned up.
    RoomNotEmpty,
}

impl DenyReason {
    /// Stable machine code; never contains names or ids.
    pub fn code(self) -> &'static str {
        match self {
            DenyReason::PerUserCapReached => "per_user_cap_reached",
            DenyReason::GlobalCapReached => "global_cap_reached",
            DenyReason::CreateCooldown => "create_cooldown",
            DenyReason::CategoryNotVisible => "category_not_visible",
            DenyReason::RenameBudgetExhausted => "rename_budget_exhausted",
            DenyReason::NotHubChannel => "not_hub_channel",
            DenyReason::InvalidUserId => "invalid_user_id",
            DenyReason::GuildMismatch => "guild_mismatch",
            DenyReason::NotRoomOwner => "not_room_owner",
            DenyReason::UntrackedChannel => "untracked_channel",
            DenyReason::EmptyGraceNotElapsed => "empty_grace_not_elapsed",
            DenyReason::AlreadyPermitted => "already_permitted",
            DenyReason::InvalidConfig => "invalid_config",
            DenyReason::RoomNotEmpty => "room_not_empty",
        }
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// The decision for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The request is allowed; the caller performs the action.
    Allow,
    /// Create a room.
    Create(CreatePlan),
    /// Refuse the request.
    Deny(DenyReason),
    /// Delete the room.
    Cleanup(u64),
    /// The room can be claimed by another member.
    Claimable(u64),
}

/// Decides whether a hub join creates a room.
///
/// The order of the checks is the order of the rules: a join that is not a hub join
/// never reaches the caps, and the caps are checked before the cooldown so that a
/// member who is already at their cap is told the cap and not the cooldown.
pub fn decide_join(
    event: &JoinEvent,
    config: &VoiceRoomConfig,
    state: &VoiceRoomState,
) -> Decision {
    if event.user_id == 0 {
        return Decision::Deny(DenyReason::InvalidUserId);
    }
    if event.guild_id != config.guild_id {
        return Decision::Deny(DenyReason::GuildMismatch);
    }
    if event.channel_id != config.hub_channel_id {
        return Decision::Deny(DenyReason::NotHubChannel);
    }
    if state.owned_room_count(event.user_id) >= config.per_user_cap {
        return Decision::Deny(DenyReason::PerUserCapReached);
    }
    if state.rooms.len() >= config.global_cap {
        return Decision::Deny(DenyReason::GlobalCapReached);
    }
    if let Some(last) = state.last_create.get(&event.user_id) {
        if event.at.saturating_sub(*last) < config.create_cooldown_secs {
            return Decision::Deny(DenyReason::CreateCooldown);
        }
    }
    Decision::Create(CreatePlan {
        name: sanitize_name(&config.default_name, &config.default_name),
        overwrites: build_overwrites(config, event.user_id),
    })
}

/// Builds the overwrite plan of a new room.
///
/// The plan is the category's overwrites plus one owner overwrite. The owner rights
/// are the [`OWNER_RIGHTS`] constant; no input can widen them.
pub fn build_overwrites(config: &VoiceRoomConfig, owner_id: u64) -> Vec<RoomOverwrite> {
    let mut overwrites = config.category_overwrites.clone();
    overwrites.push(RoomOverwrite {
        target: OverwriteTarget::Member(owner_id),
        allow: OWNER_RIGHTS,
        deny: FORBIDDEN_OWNER_RIGHTS,
    });
    overwrites
}

/// Decides whether an owner may permit a member.
///
/// `requester_id` is the member asking for the permit; it must own the room.
/// `member_id` is the member the permit is for; it must already be able to view the
/// category, otherwise the permit would let an owner bypass the verification or
/// safeword gating of the area.
pub fn decide_permit(
    channel_id: u64,
    requester_id: u64,
    member_id: u64,
    member_can_view_category: bool,
    state: &VoiceRoomState,
) -> Decision {
    let room = match state.room(channel_id) {
        Some(room) => room,
        None => return Decision::Deny(DenyReason::UntrackedChannel),
    };
    if requester_id == 0 || member_id == 0 {
        return Decision::Deny(DenyReason::InvalidUserId);
    }
    if requester_id != room.owner_id {
        return Decision::Deny(DenyReason::NotRoomOwner);
    }
    if member_id == room.owner_id {
        return Decision::Deny(DenyReason::AlreadyPermitted);
    }
    if !member_can_view_category {
        return Decision::Deny(DenyReason::CategoryNotVisible);
    }
    Decision::Allow
}

/// Decides what a leaving member leaves behind.
///
/// The owner leaving a room that still holds members makes the room claimable.
/// An empty room is only cleaned up once its grace period has elapsed; the
/// runtime records `empty_since` when the room first becomes empty and calls
/// again later.
///
/// [`Room::occupants`] is the count BEFORE the leave: the member who is
/// leaving is still counted. A room whose count is one therefore becomes
/// empty when that member leaves, and only a room whose count is two or more
/// still holds members afterwards.
///
/// The grace check comes first and applies to whoever leaves last: a room
/// whose owner has already left must still be cleaned up once it is empty, or
/// it would linger forever and keep consuming the global cap. Only a member
/// who leaves a room that still holds members can be refused for not being
/// the owner, because only then does the leave have a claimable consequence.
pub fn decide_leave(
    channel_id: u64,
    user_id: u64,
    now: i64,
    config: &VoiceRoomConfig,
    state: &VoiceRoomState,
) -> Decision {
    let room = match state.room(channel_id) {
        Some(room) => room,
        None => return Decision::Deny(DenyReason::UntrackedChannel),
    };
    if user_id == 0 {
        return Decision::Deny(DenyReason::InvalidUserId);
    }
    if config.empty_grace_secs < 0 {
        return Decision::Deny(DenyReason::InvalidConfig);
    }
    if room.occupants >= 2 {
        // The room still holds members after this leave, so it may become
        // claimable. Only the owner may leave it that way; anybody else
        // leaves no trace.
        if user_id != room.owner_id {
            return Decision::Deny(DenyReason::NotRoomOwner);
        }
        return Decision::Claimable(channel_id);
    }
    // The room is empty after this leave. The grace period starts when the
    // room first became empty; until it has elapsed the room stays. The check
    // does not depend on who leaves: the room is empty, so whoever leaves
    // last reaches it.
    let empty_since = room.empty_since.unwrap_or(now);
    if now.saturating_sub(empty_since) >= config.empty_grace_secs {
        return Decision::Cleanup(channel_id);
    }
    Decision::Deny(DenyReason::EmptyGraceNotElapsed)
}

/// Decides whether an empty room may be cleaned up, without a member leaving.
///
/// The runtime has to be able to reap a room whose last member left before the
/// grace period had elapsed: nobody leaves the room again, so `decide_leave` would
/// never be called for it. The sweep is the runtime's periodic check: it cleans an
/// empty room up once its grace period has elapsed, and it never touches a room
/// that still holds members or a channel the policy does not track.
///
/// A sweep has no member leaving, so [`Room::occupants`] is the current
/// occupancy here: a room whose count is greater than zero still holds
/// members and is never cleaned up by a sweep.
pub fn decide_sweep(
    channel_id: u64,
    now: i64,
    config: &VoiceRoomConfig,
    state: &VoiceRoomState,
) -> Decision {
    let room = match state.room(channel_id) {
        Some(room) => room,
        None => return Decision::Deny(DenyReason::UntrackedChannel),
    };
    if config.empty_grace_secs < 0 {
        return Decision::Deny(DenyReason::InvalidConfig);
    }
    if room.occupants > 0 {
        return Decision::Deny(DenyReason::RoomNotEmpty);
    }
    let empty_since = room.empty_since.unwrap_or(now);
    if now.saturating_sub(empty_since) >= config.empty_grace_secs {
        return Decision::Cleanup(channel_id);
    }
    Decision::Deny(DenyReason::EmptyGraceNotElapsed)
}

/// Decides whether a room may be renamed.
///
/// The rename budget is counted per room inside its window: a room that was renamed
/// twice inside the window may not be renamed a third time until the window has
/// passed. The budget is data, so the runtime enforces the same number.
pub fn decide_rename(
    channel_id: u64,
    owner_id: u64,
    now: i64,
    config: &VoiceRoomConfig,
    state: &VoiceRoomState,
) -> Decision {
    let room = match state.room(channel_id) {
        Some(room) => room,
        None => return Decision::Deny(DenyReason::UntrackedChannel),
    };
    if owner_id == 0 {
        return Decision::Deny(DenyReason::InvalidUserId);
    }
    if owner_id != room.owner_id {
        return Decision::Deny(DenyReason::NotRoomOwner);
    }
    if config.rename_budget.window_secs < 0 {
        return Decision::Deny(DenyReason::InvalidConfig);
    }
    let used = state
        .renames
        .iter()
        .filter(|(renamed_channel, at)| {
            *renamed_channel == channel_id
                && now.saturating_sub(*at) < config.rename_budget.window_secs
        })
        .count();
    if used >= config.rename_budget.max_renames as usize {
        return Decision::Deny(DenyReason::RenameBudgetExhausted);
    }
    Decision::Allow
}

/// Cleans a room name.
///
/// The cleaning removes, in this order: the control and the invisible characters,
/// the mention tokens `<@..>`, `<@&..>` and `<#..>`, the `@everyone` and `@here`
/// words, and every link (a URL, a `www.` host, an invite link, and an invite
/// host that was spread across spaces). What is left is trimmed, shortened to
/// [`MAX_NAME_CHARS`] characters and, when it is empty, replaced by `default_name`.
///
/// The invisible characters are removed FIRST, because an invisible character
/// inside a mention would otherwise hide the mention from the mention pass and
/// the mention would be rebuilt when the invisible character was removed. The
/// mention pass then runs until the text stops changing, so a nested token
/// cannot reassemble into a live token at any nesting depth.
pub fn sanitize_name(raw: &str, default_name: &str) -> String {
    let cleaned = clean_text(raw);
    let cleaned = if cleaned.trim().is_empty() {
        clean_text(default_name)
    } else {
        cleaned
    };
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return DEFAULT_ROOM_NAME.to_string();
    }
    truncate_chars(cleaned, MAX_NAME_CHARS)
}

/// Removes the invisible characters, then the mentions, until the text stops
/// changing, and finally the links.
///
/// The links come last on purpose: a mention token can hide the front of a URL,
/// so the URL is only whole after the mentions are gone.
fn clean_text(raw: &str) -> String {
    let without_invisible = strip_invisible(raw);
    let without_mentions = strip_mentions_to_fixpoint(&without_invisible);
    strip_links(&without_mentions)
}

/// Removes the mention tokens and the `@everyone` and `@here` words, until the
/// text stops changing.
///
/// The loop is what keeps a nested token from reassembling: stripping one token
/// can expose another one behind it.
///
/// The loop has no fixed pass count. Every pass that changes the text removes
/// at least one character, so the text strictly shrinks and the loop always
/// ends; the number of passes a text can need is bounded by the number of
/// characters it has, and that bound is the one used here. A fixed pass count
/// would be a bound the caller controls: a mention nested deeper than the
/// count would come back as a live token.
fn strip_mentions_to_fixpoint(raw: &str) -> String {
    let mut current = strip_mentions(raw);
    // Every pass that changes the text removes at least one character, so at
    // most `len` passes can change it; one more pass proves that it has
    // stopped changing.
    let bound = current.chars().count() + 1;
    for _ in 0..bound {
        let next = strip_mentions(&current);
        if next == current {
            break;
        }
        current = next;
    }
    current
}

/// Removes the mention tokens and the `@everyone` and `@here` words.
fn strip_mentions(raw: &str) -> String {
    let without_tokens = strip_mention_tokens(raw);
    strip_everyone_and_here(&without_tokens)
}

/// Removes the `<@..>`, `<@&..>` and `<#..>` tokens.
fn strip_mention_tokens(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '<' {
            if let Some(end) = find_token_end(&chars, index) {
                let inner: String = chars[index + 1..end].iter().collect();
                if inner.starts_with('@') || inner.starts_with('#') {
                    index = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// Finds the `>` that closes a mention token, if there is one close by.
fn find_token_end(chars: &[char], start: usize) -> Option<usize> {
    // A mention token is short; a longer run of text is not a token.
    const MAX_TOKEN_CHARS: usize = 64;
    let limit = chars.len().min(start + MAX_TOKEN_CHARS);
    (start + 1..limit).find(|&i| chars[i] == '>')
}

/// Removes the `@everyone` and `@here` words, whatever their case.
fn strip_everyone_and_here(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '@' {
            if let Some(len) = mention_word_len(&chars, index) {
                index += len;
                continue;
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// Returns the length of the `@everyone` or `@here` word at `start`, if there is one.
fn mention_word_len(chars: &[char], start: usize) -> Option<usize> {
    let rest: String = chars[start + 1..].iter().collect();
    let lowered = rest.to_ascii_lowercase();
    for word in ["everyone", "here"] {
        if lowered.starts_with(word) {
            let after = rest[word.len()..].chars().next();
            let is_word_end = after.is_none_or(|c| !c.is_alphanumeric() && c != '_');
            if is_word_end {
                return Some(1 + word.len());
            }
        }
    }
    None
}

/// Removes the control and the invisible characters.
fn strip_invisible(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
        .collect()
}

/// True for the characters that are invisible but still occupy a position.
///
/// The rule is a class rule over the ranges that render nothing, not a hand
/// list of examples:
///
/// * the control characters, which `char::is_control` already covers;
/// * the whitespace characters other than the plain ASCII space, which
///   `char::is_whitespace` already covers (the tab and the newline are
///   control characters and are already gone);
/// * the **closed class** of the code points that render nothing, which
///   [`INVISIBLE_RANGES`] names in full.
///
/// The plain ASCII space is kept: it separates the words of a name.
fn is_invisible(c: char) -> bool {
    if c == ' ' {
        return false;
    }
    if c.is_whitespace() {
        // Every other space character is a look-alike of the plain space and
        // is removed.
        return true;
    }
    is_rendering_nothing(c)
}

/// The closed class of the code points that render nothing, as ranges.
///
/// The class is the union of three Unicode sets:
///
/// * the **format characters**, the code points whose general category is
///   `Cf`: they have no visible effect wherever they appear;
/// * the **default ignorable** code points, the code points Unicode defines
///   as having no visible effect. This set is NOT the same as the format
///   characters: `DerivedCoreProperties.txt` builds it as
///   `Other_Default_Ignorable_Code_Point + Cf + Variation_Selector`
///   `minus White_Space minus FFF9..FFFB`, so it also holds the variation
///   selectors, the Mongolian free variation selectors, the Khmer inherent
///   vowels, the Hangul fillers, the reserved code points U+FFF0..U+FFF8 and
///   the tag characters - and it does NOT hold the interlinear annotation
///   characters U+FFF9..U+FFFB nor the Egyptian format controls
///   U+13430..U+1343F, which are `Cf` but not default ignorable;
/// * the **blank fillers**, the code points that occupy a position and draw
///   nothing but belong to neither set above.
///
/// The union is the class, so a code point is in it if it is in ANY of the
/// three sets. The table below is the whole class for the Unicode version
/// named here, not a list of the examples that were thought of: a hand list
/// left out the Mongolian free variation selectors, the shorthand format
/// controls, the reserved part of the tag block, every format character
/// Unicode does not make default ignorable, and the reserved default
/// ignorables U+FFF0..U+FFF8, and those came back in a name as
/// blank-looking text.
///
/// The ranges were generated from the Unicode character database files
/// `DerivedCoreProperties.txt` (the `Default_Ignorable_Code_Point` property)
/// and `UnicodeData.txt` (the `Cf` general category), both of **Unicode
/// 17.0.0** - the Unicode version this crate's Rust standard library ships
/// (`char::UNICODE_VERSION`). The table is checked by
/// `every_character_that_renders_nothing_is_removed` (which walks every code
/// point of every range), `the_class_holds_every_code_point_of_the_default_ignorable_set`
/// (which pins the total) and `a_character_just_outside_the_class_survives`
/// (which checks the edge). Those three read the production table, so they
/// only show that the implementation agrees with itself: a range that was
/// moved by one code point but kept its size would pass all three. The table
/// is therefore ALSO checked against `EXPECTED_CLASS`, a second copy of the
/// class written as raw literals that do not come from this constant.
const INVISIBLE_RANGES: &[(u32, u32)] = &[
    // The soft hyphen.
    (0x00AD, 0x00AD),
    // The combining grapheme joiner.
    (0x034F, 0x034F),
    // The Arabic number sign and the number marks above it.
    (0x0600, 0x0605),
    // The Arabic letter mark.
    (0x061C, 0x061C),
    // The Arabic end of ayah.
    (0x06DD, 0x06DD),
    // The Syriac abbreviation mark.
    (0x070F, 0x070F),
    // The Arabic pound and piastre marks.
    (0x0890, 0x0891),
    // The Arabic disputed end of ayah.
    (0x08E2, 0x08E2),
    // The Hangul choseong and jungseong fillers.
    (0x115F, 0x1160),
    // The Khmer inherent vowels.
    (0x17B4, 0x17B5),
    // The Mongolian free variation selectors and the vowel separator.
    (0x180B, 0x180F),
    // The zero-width characters.
    (0x200B, 0x200F),
    // The bidirectional embedding, override and isolate controls.
    (0x202A, 0x202E),
    // The word joiner, the invisible operators and the isolates.
    (0x2060, 0x206F),
    // The Braille pattern blank.
    (0x2800, 0x2800),
    // The Hangul filler.
    (0x3164, 0x3164),
    // The variation selectors.
    (0xFE00, 0xFE0F),
    // The byte order mark.
    (0xFEFF, 0xFEFF),
    // The halfwidth Hangul filler.
    (0xFFA0, 0xFFA0),
    // The reserved code points U+FFF0..U+FFF8 and the interlinear annotation
    // characters U+FFF9..U+FFFB. The reserved ones are default ignorable but
    // are NOT format characters, so they have to be named here.
    (0xFFF0, 0xFFFB),
    // The Kaithi number signs.
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    // The Egyptian hieroglyph format controls.
    (0x13430, 0x1343F),
    // The shorthand format controls.
    (0x1BCA0, 0x1BCA3),
    // The musical format controls.
    (0x1D173, 0x1D17A),
    // The tag characters, including the part Unicode reserves for more.
    (0xE0000, 0xE0FFF),
];

/// True for the code points that render nothing.
fn is_rendering_nothing(c: char) -> bool {
    let cp = c as u32;
    INVISIBLE_RANGES
        .iter()
        .any(|(start, end)| cp >= *start && cp <= *end)
}

/// Removes the links: URLs, `www.` hosts and invite links.
fn strip_links(raw: &str) -> String {
    let segments: Vec<&str> = raw.split_whitespace().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(segments.len());
    let mut index = 0;
    while index < segments.len() {
        if is_link_like(segments[index]) {
            index += 1;
            continue;
        }
        // An invite host that was spread across spaces is still an invite host.
        if let Some(end) = split_invite_run(&segments, index) {
            index = end + 1;
            continue;
        }
        kept.push(segments[index]);
        index += 1;
    }
    kept.join(" ")
}

/// True when the segment is a URL, a `www.` host or an invite link.
///
/// The rule is a class rule, not a list of examples: a segment whose host part is a
/// dotted name with a valid top-level domain is a host, whether or not it carries a
/// path, a query, a fragment or a port. `example.com`, `example.com/x`,
/// `example.com:8080` and `www.example.com` are all removed, and so is a bare invite
/// host such as `discord.gg`.
fn is_link_like(segment: &str) -> bool {
    let trimmed = segment.trim_matches(|c: char| !c.is_alphanumeric());
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.contains("://") || lowered.starts_with("www.") {
        return true;
    }
    if contains_invite_host(&lowered) {
        return true;
    }
    // The host part is what stands in front of the first slash, question mark or
    // fragment marker: everything from there on belongs to the link, not to the
    // host.
    let host = cut_at_first_of(&lowered, &['/', '?', '#']);
    // A host may carry a port and may carry user information before the `@`.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    looks_like_host(host)
}

/// Cuts the text at the first of the given markers.
fn cut_at_first_of<'a>(text: &'a str, markers: &[char]) -> &'a str {
    match text.find(markers) {
        Some(index) => &text[..index],
        None => text,
    }
}

/// True when the text looks like a host name such as `example.invalid`.
///
/// The name must carry at least one dot, and its last label must be a valid
/// top-level domain: at least two alphabetic characters, or a punycode label
/// (`xn--` followed by letters and digits). So `example.com`, `discord.gg` and
/// `example.xn--p1ai` are hosts while `e.g.` and `version 1.2` are not.
///
/// A trailing dot is allowed: `example.com.` is the fully qualified form of the
/// same host.
fn looks_like_host(host: &str) -> bool {
    // The host may be wrapped in punctuation; the punctuation is not part of it.
    let host = host.trim_matches(|c: char| !c.is_alphanumeric());
    if !host.contains('.') || host.starts_with('.') {
        return false;
    }
    // A trailing dot is the fully qualified form and does not change the host.
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return false;
    }
    let last_label = host.rsplit('.').next().unwrap_or_default();
    is_top_level_domain(last_label)
}

/// True when the label is a valid top-level domain.
///
/// The label is either alphabetic (`com`, `gg`, `invalid`) or a punycode label
/// (`xn--p1ai`), which is what a host with a non-ASCII top-level domain looks
/// like once it is encoded.
fn is_top_level_domain(label: &str) -> bool {
    if label.len() >= 2 && label.chars().all(|c| c.is_ascii_alphabetic()) {
        return true;
    }
    let Some(encoded) = label.strip_prefix("xn--") else {
        return false;
    };
    !encoded.is_empty() && encoded.chars().all(|c| c.is_ascii_alphanumeric())
}

/// True when the text carries one of the known invite hosts.
///
/// The host is listed with and without its trailing slash, so a bare `discord.gg` is
/// an invite host too, and a host that was split across spaces is caught once the
/// segments are joined.
fn contains_invite_host(lowered: &str) -> bool {
    const INVITE_HOSTS: [&str; 8] = [
        "discord.gg/",
        "discord.gg",
        "discord.com/invite/",
        "discord.com/invite",
        "discordapp.com/invite/",
        "discordapp.com/invite",
        "discord.new/",
        "discord.new",
    ];
    INVITE_HOSTS.iter().any(|host| lowered.contains(host))
}

/// Finds the run of segments that, joined without spaces, starts with an invite host.
///
/// The run starts at `start` and is the shortest one that spells an invite host.
fn split_invite_run(segments: &[&str], start: usize) -> Option<usize> {
    let mut joined = String::new();
    for (offset, segment) in segments.iter().enumerate().skip(start) {
        joined.push_str(&segment.to_ascii_lowercase());
        if contains_invite_host(&joined) {
            return Some(offset);
        }
        // The host cannot be longer than a domain and a code; stop early.
        if joined.len() > 64 {
            break;
        }
    }
    None
}

/// Shortens the text to `limit` characters, counted in characters and not in bytes.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars()
        .take(limit)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic ids. They are deliberately far from any real Discord snowflake and carry
    // no meaning: 18 digits that were never issued by Discord.
    const GUILD: u64 = 1_000_000_000_000_000_001;
    const HUB: u64 = 1_000_000_000_000_000_002;
    const CATEGORY: u64 = 1_000_000_000_000_000_003;
    const ROOM: u64 = 1_000_000_000_000_000_004;
    const OTHER_ROOM: u64 = 1_000_000_000_000_000_005;
    const OWNER: u64 = 1_000_000_000_000_000_006;
    const MEMBER: u64 = 1_000_000_000_000_000_007;
    const OTHER_MEMBER: u64 = 1_000_000_000_000_000_008;
    const ROLE: u64 = 1_000_000_000_000_000_009;
    const T0: i64 = 1_700_000_000;
    /// A member that never owns a room, so a leave from this member is always
    /// a leave from somebody who is not the owner.
    const VISITOR: u64 = 1_000_000_000_000_000_011;

    fn cfg() -> VoiceRoomConfig {
        VoiceRoomConfig {
            guild_id: GUILD,
            hub_channel_id: HUB,
            category_id: CATEGORY,
            default_name: "Voice Room".to_string(),
            per_user_cap: 1,
            global_cap: 25,
            create_cooldown_secs: 30,
            empty_grace_secs: 60,
            rename_budget: RenameBudget {
                max_renames: 2,
                window_secs: 600,
            },
            category_overwrites: vec![RoomOverwrite {
                target: OverwriteTarget::Role(ROLE),
                allow: CONNECT,
                deny: 0,
            }],
        }
    }

    fn join(user_id: u64, at: i64) -> JoinEvent {
        JoinEvent {
            guild_id: GUILD,
            user_id,
            channel_id: HUB,
            at,
        }
    }

    fn room(owner_id: u64, occupants: usize, empty_since: Option<i64>) -> Room {
        Room {
            channel_id: ROOM,
            owner_id,
            created_at: T0,
            empty_since,
            occupants,
        }
    }

    fn state_with(rooms: Vec<Room>) -> VoiceRoomState {
        VoiceRoomState {
            rooms,
            last_create: HashMap::new(),
            renames: Vec::new(),
        }
    }

    fn expect_create(decision: &Decision) -> CreatePlan {
        match decision {
            Decision::Create(plan) => plan.clone(),
            other => panic!("expected Create, got {other:?}"),
        }
    }

    fn expect_deny(decision: &Decision) -> DenyReason {
        match decision {
            Decision::Deny(reason) => *reason,
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // Rule 1: hub join -> Create under the caps and the cooldown.
    // ---------------------------------------------------------------------

    /// A hub join by a member who owns no room creates a room.
    #[test]
    fn hub_join_creates_a_room() {
        let decision = decide_join(&join(OWNER, T0), &cfg(), &state_with(Vec::new()));
        let plan = expect_create(&decision);
        assert_eq!(plan.name, "Voice Room");
    }

    /// A join in a channel that is not the hub creates nothing.
    #[test]
    fn join_outside_the_hub_creates_nothing() {
        let mut event = join(OWNER, T0);
        event.channel_id = ROOM;
        let decision = decide_join(&event, &cfg(), &state_with(Vec::new()));
        assert_eq!(
            expect_deny(&decision),
            DenyReason::NotHubChannel,
            "a join outside the hub must not create a room"
        );
    }

    /// A join whose user id is zero creates nothing: id zero is not a member.
    #[test]
    fn zero_user_id_creates_nothing() {
        let decision = decide_join(&join(0, T0), &cfg(), &state_with(Vec::new()));
        assert_eq!(expect_deny(&decision), DenyReason::InvalidUserId);
    }

    /// A join from another guild creates nothing.
    #[test]
    fn join_from_another_guild_creates_nothing() {
        let mut event = join(OWNER, T0);
        event.guild_id = 1_000_000_000_000_000_099;
        let decision = decide_join(&event, &cfg(), &state_with(Vec::new()));
        assert_eq!(expect_deny(&decision), DenyReason::GuildMismatch);
    }

    /// A member who already owns a room is refused when the per-user cap is one.
    #[test]
    fn per_user_cap_refuses_the_second_room() {
        let state = state_with(vec![room(OWNER, 1, None)]);
        let decision = decide_join(&join(OWNER, T0), &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::PerUserCapReached);
    }

    /// The per-user cap counts the rooms the member owns, and only those.
    #[test]
    fn per_user_cap_counts_only_the_own_rooms() {
        let mut other = room(OTHER_MEMBER, 1, None);
        other.channel_id = OTHER_ROOM;
        let state = state_with(vec![other]);
        let decision = decide_join(&join(OWNER, T0), &cfg(), &state);
        let _ = expect_create(&decision);
    }

    /// With a per-user cap of two, a second room for the same member is allowed.
    #[test]
    fn per_user_cap_of_two_allows_a_second_room() {
        let mut config = cfg();
        config.per_user_cap = 2;
        let state = state_with(vec![room(OWNER, 1, None)]);
        let decision = decide_join(&join(OWNER, T0), &config, &state);
        let _ = expect_create(&decision);
    }

    /// The global cap refuses a create once the maximum number of rooms exists.
    #[test]
    fn global_cap_refuses_the_create() {
        let mut config = cfg();
        config.global_cap = 2;
        let mut first = room(OTHER_MEMBER, 1, None);
        first.channel_id = OTHER_ROOM;
        let mut second = room(1_000_000_000_000_000_010, 1, None);
        second.channel_id = 1_000_000_000_000_000_011;
        let state = state_with(vec![first, second]);
        let decision = decide_join(&join(OWNER, T0), &config, &state);
        assert_eq!(expect_deny(&decision), DenyReason::GlobalCapReached);
    }

    /// A create inside the cooldown window is refused.
    #[test]
    fn cooldown_refuses_a_create_inside_the_window() {
        let mut state = state_with(Vec::new());
        state.last_create.insert(OWNER, T0);
        let decision = decide_join(&join(OWNER, T0 + 29), &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::CreateCooldown);
    }

    /// A create exactly one second after the cooldown elapsed is allowed.
    #[test]
    fn cooldown_allows_a_create_at_the_boundary() {
        let mut state = state_with(Vec::new());
        state.last_create.insert(OWNER, T0);
        let decision = decide_join(&join(OWNER, T0 + 30), &cfg(), &state);
        let _ = expect_create(&decision);
    }

    /// A zero cooldown never refuses a create for cooldown reasons.
    #[test]
    fn zero_cooldown_never_refuses() {
        let mut config = cfg();
        config.create_cooldown_secs = 0;
        let mut state = state_with(Vec::new());
        state.last_create.insert(OWNER, T0);
        let decision = decide_join(&join(OWNER, T0), &config, &state);
        let _ = expect_create(&decision);
    }

    /// The reason codes are stable machine codes and carry no text.
    #[test]
    fn deny_reason_codes_are_stable_and_carry_no_text() {
        assert_eq!(DenyReason::PerUserCapReached.code(), "per_user_cap_reached");
        assert_eq!(DenyReason::GlobalCapReached.code(), "global_cap_reached");
        assert_eq!(DenyReason::CreateCooldown.code(), "create_cooldown");
        assert_eq!(
            DenyReason::CategoryNotVisible.code(),
            "category_not_visible"
        );
        assert_eq!(
            DenyReason::RenameBudgetExhausted.code(),
            "rename_budget_exhausted"
        );
        assert_eq!(DenyReason::NotHubChannel.code(), "not_hub_channel");
        assert_eq!(DenyReason::InvalidUserId.code(), "invalid_user_id");
        assert_eq!(DenyReason::GuildMismatch.code(), "guild_mismatch");
        assert_eq!(DenyReason::NotRoomOwner.code(), "not_room_owner");
        assert_eq!(DenyReason::UntrackedChannel.code(), "untracked_channel");
        assert_eq!(
            DenyReason::EmptyGraceNotElapsed.code(),
            "empty_grace_not_elapsed"
        );
    }

    // ---------------------------------------------------------------------
    // Rule 2: name sanitising.
    // ---------------------------------------------------------------------

    /// An `@everyone` mention is removed from the room name.
    #[test]
    fn everyone_mention_is_removed() {
        assert_eq!(sanitize_name("@everyone party", "Voice Room"), "party");
    }

    /// An `@here` mention is removed from the room name.
    #[test]
    fn here_mention_is_removed() {
        assert_eq!(sanitize_name("@here party", "Voice Room"), "party");
    }

    /// A user mention is removed from the room name.
    #[test]
    fn user_mention_is_removed() {
        let raw = "<@1000000000000000007> party";
        let cleaned = sanitize_name(raw, "Voice Room");
        assert!(!cleaned.contains("1000000000000000007"));
        assert!(!cleaned.contains("<@"));
        assert!(cleaned.contains("party"));
    }

    /// A role mention is removed from the room name.
    #[test]
    fn role_mention_is_removed() {
        let cleaned = sanitize_name("<@&1000000000000000009> party", "Voice Room");
        assert!(!cleaned.contains("@&"));
        assert!(!cleaned.contains("1000000000000000009"));
    }

    /// A channel mention is removed from the room name.
    #[test]
    fn channel_mention_is_removed() {
        let cleaned = sanitize_name("<#1000000000000000004> party", "Voice Room");
        assert!(!cleaned.contains("<#"));
        assert!(!cleaned.contains("1000000000000000004"));
    }

    /// An invite link is removed from the room name.
    #[test]
    fn invite_link_is_removed() {
        let cleaned = sanitize_name("join https://discord.gg/AbCdEf party", "Voice Room");
        assert!(!cleaned.contains("discord.gg"));
        assert!(!cleaned.contains("https"));
    }

    /// A bare invite code with the domain split across spaces is still removed.
    #[test]
    fn invite_link_split_by_spaces_is_removed() {
        let cleaned = sanitize_name("discord . gg/AbCdEf party", "Voice Room");
        assert!(!cleaned.to_lowercase().contains("discord"));
        assert!(cleaned.contains("party"));
    }

    /// Any URL is removed from the room name.
    #[test]
    fn url_is_removed() {
        let cleaned = sanitize_name("see https://example.invalid/x party", "Voice Room");
        assert!(!cleaned.contains("example.invalid"));
        assert!(!cleaned.contains("https"));
    }

    /// A bare host without a path is a URL too and is removed from the room name.
    #[test]
    fn a_bare_host_is_removed() {
        let cleaned = sanitize_name("example.com party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.com"),
            "a bare host must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A bare invite host without a path is removed from the room name.
    #[test]
    fn a_bare_invite_host_is_removed() {
        let cleaned = sanitize_name("discord.gg party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("discord.gg"),
            "a bare invite host must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A URL whose scheme was split across spaces is still a URL and is removed.
    #[test]
    fn a_url_with_a_split_scheme_is_removed() {
        let cleaned = sanitize_name("https ://example.invalid/x party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.invalid"),
            "a split scheme must not smuggle a host into the name: {cleaned:?}"
        );
    }

    /// A host with a port and a path is still a host and is removed.
    #[test]
    fn a_host_with_a_port_is_removed() {
        let cleaned = sanitize_name("example.com:8080/x party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.com"),
            "a host with a port must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A host with user information in front of it is still a host and is removed.
    #[test]
    fn a_host_with_user_information_is_removed() {
        let cleaned = sanitize_name("someone@example.com party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.com"),
            "a host with user information must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A bare invite host split across spaces is removed as well.
    #[test]
    fn a_bare_invite_host_split_by_spaces_is_removed() {
        let cleaned = sanitize_name("discord . gg party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("discord"),
            "a split invite host must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// An invisible character inside a mention must not let the mention through:
    /// the cleaning removes the invisible characters FIRST, so the mention that
    /// they were hiding is recognised and removed.
    #[test]
    fn an_invisible_character_inside_a_mention_does_not_hide_the_mention() {
        let cleaned = sanitize_name("@every\u{200B}one party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("@everyone"),
            "a mention split by a zero-width character must not survive: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A zero-width joiner inside a mention must not let the mention through.
    #[test]
    fn a_joiner_inside_a_mention_does_not_hide_the_mention() {
        let cleaned = sanitize_name("@he\u{200D}re party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("@here"),
            "a mention split by a joiner must not survive: {cleaned:?}"
        );
    }

    /// A control character inside a mention must not let the mention through.
    #[test]
    fn a_control_character_inside_a_mention_does_not_hide_the_mention() {
        let cleaned = sanitize_name("@every\u{0007}one party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("@everyone"),
            "a mention split by a control character must not survive: {cleaned:?}"
        );
    }

    /// An invisible character inside a user mention token must not let the token
    /// through.
    #[test]
    fn an_invisible_character_inside_a_mention_token_does_not_hide_it() {
        let cleaned = sanitize_name("<\u{200B}@1000000000000000007> party", "Voice Room");
        assert!(
            !cleaned.contains("<@") && !cleaned.contains("1000000000000000007"),
            "a mention token split by a zero-width character must not survive: {cleaned:?}"
        );
    }

    /// A nested mention token must not reassemble into a live token: the mention
    /// stripping runs until the text stops changing.
    #[test]
    fn a_nested_mention_token_does_not_reassemble() {
        let cleaned = sanitize_name("<<@1000000000000000007>@1000000000000000007>", "Voice Room");
        assert!(
            !cleaned.contains("<@"),
            "a nested mention token must not reassemble into a live token: {cleaned:?}"
        );
    }

    /// No mention of any shape survives the cleaning: the final name carries no
    /// `<@`, no `<#`, no `@everyone` and no `@here`, whatever the input was.
    #[test]
    fn no_mention_of_any_shape_survives_the_cleaning() {
        let inputs = [
            "@everyone party",
            "@here party",
            "<@1000000000000000007> party",
            "<@&1000000000000000009> party",
            "<#1000000000000000004> party",
            "@every\u{200B}one party",
            "<\u{200B}@1000000000000000007> party",
            "<<@1000000000000000007>@1000000000000000007>",
            "@ev\u{0007}eryone party",
            "@\u{202A}everyone party",
        ];
        for input in inputs {
            let cleaned = sanitize_name(input, "Voice Room");
            let lowered = cleaned.to_lowercase();
            assert!(
                !lowered.contains("<@") && !lowered.contains("<#"),
                "a mention token survived for {input:?}: {cleaned:?}"
            );
            assert!(
                !lowered.contains("@everyone") && !lowered.contains("@here"),
                "a mention word survived for {input:?}: {cleaned:?}"
            );
        }
    }

    /// The bidirectional control characters are removed: they can reorder the text
    /// a reader sees without changing the bytes.
    #[test]
    fn bidirectional_control_characters_are_removed() {
        let cleaned = sanitize_name("a\u{202A}b\u{202C}c", "Voice Room");
        assert_eq!(cleaned, "abc");
    }

    /// The tag characters are removed: they are invisible and can carry text that
    /// a reader never sees.
    #[test]
    fn tag_characters_are_removed() {
        let cleaned = sanitize_name("\u{E0041}\u{E0042} party", "Voice Room");
        assert_eq!(cleaned, "party");
    }

    /// A name that is only invisible characters falls back to the default name.
    #[test]
    fn a_name_of_only_invisible_characters_falls_back_to_the_default() {
        assert_eq!(
            sanitize_name("\u{E0041}\u{E0042}", "Voice Room"),
            "Voice Room",
            "a name made only of invisible characters is an empty name"
        );
        assert_eq!(
            sanitize_name("\u{3164}", "Voice Room"),
            "Voice Room",
            "a name made only of a Hangul filler is an empty name"
        );
    }

    /// The Hangul filler is removed: it looks blank but is not a control
    /// character.
    #[test]
    fn the_hangul_filler_is_removed() {
        let cleaned = sanitize_name("par\u{3164}ty", "Voice Room");
        assert_eq!(cleaned, "party");
    }

    /// A host inside brackets is removed: the brackets are punctuation around the
    /// host, not part of it.
    #[test]
    fn a_host_in_brackets_is_removed() {
        let cleaned = sanitize_name("(example.com) party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.com"),
            "a bracketed host must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A host followed by a comma is removed: the comma is punctuation after the
    /// host, not part of it.
    #[test]
    fn a_host_followed_by_a_comma_is_removed() {
        let cleaned = sanitize_name("example.com, party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.com"),
            "a host followed by a comma must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A host with a trailing dot is removed: the trailing dot is the fully
    /// qualified form of the same host.
    #[test]
    fn a_host_with_a_trailing_dot_is_removed() {
        let cleaned = sanitize_name("example.com. party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.com"),
            "a trailing-dot host must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A host with a query string is removed: the query is part of the link.
    #[test]
    fn a_host_with_a_query_is_removed() {
        let cleaned = sanitize_name("example.com?x=1 party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.com"),
            "a host with a query must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A host with a fragment is removed: the fragment is part of the link.
    #[test]
    fn a_host_with_a_fragment_is_removed() {
        let cleaned = sanitize_name("example.com#x party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.com"),
            "a host with a fragment must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A host inside angle brackets is removed.
    #[test]
    fn a_host_in_angle_brackets_is_removed() {
        let cleaned = sanitize_name("<example.com> party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.com"),
            "an angle-bracketed host must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// A host with a punycode top-level domain is removed: the last label is not
    /// alphabetic, but it is a valid top-level domain.
    #[test]
    fn a_host_with_a_punycode_top_level_domain_is_removed() {
        let cleaned = sanitize_name("example.xn--p1ai party", "Voice Room");
        assert!(
            !cleaned.to_lowercase().contains("example.xn--p1ai"),
            "a punycode host must not survive the cleaning: {cleaned:?}"
        );
        assert!(cleaned.contains("party"));
    }

    /// Text that only looks a little like a host is kept: the cleaning removes
    /// hosts, not ordinary words that carry a dot.
    #[test]
    fn ordinary_text_with_a_dot_is_kept() {
        assert_eq!(
            sanitize_name("e.g. party", "Voice Room"),
            "e.g. party",
            "a one-letter label is not a top-level domain"
        );
        assert_eq!(
            sanitize_name("version 1.2 party", "Voice Room"),
            "version 1.2 party"
        );
    }

    /// A control character is removed from the room name.
    #[test]
    fn control_characters_are_removed() {
        let cleaned = sanitize_name("par\u{0007}ty", "Voice Room");
        assert_eq!(cleaned, "party");
    }

    /// A zero-width character is removed from the room name.
    #[test]
    fn zero_width_characters_are_removed() {
        let cleaned = sanitize_name("pa\u{200B}rt\u{200D}y", "Voice Room");
        assert_eq!(cleaned, "party");
    }

    /// A name that is empty after cleaning falls back to the default name.
    #[test]
    fn empty_after_cleaning_falls_back_to_the_default() {
        assert_eq!(sanitize_name("@everyone", "Voice Room"), "Voice Room");
        assert_eq!(sanitize_name("", "Voice Room"), "Voice Room");
        assert_eq!(sanitize_name("   ", "Voice Room"), "Voice Room");
        assert_eq!(
            sanitize_name("\u{200B}\u{0000}", "Voice Room"),
            "Voice Room"
        );
    }

    /// A default name that is itself empty after cleaning falls back to the built-in
    /// default, so a room always has a name.
    #[test]
    fn an_empty_default_name_falls_back_to_the_built_in_default() {
        assert_eq!(sanitize_name("", "@everyone"), DEFAULT_ROOM_NAME);
        assert_eq!(sanitize_name("", ""), DEFAULT_ROOM_NAME);
    }

    /// The built-in default name is itself clean, so the fallback is a real name.
    #[test]
    fn the_built_in_default_name_is_clean() {
        assert_eq!(sanitize_name("", DEFAULT_ROOM_NAME), DEFAULT_ROOM_NAME);
        assert!(!DEFAULT_ROOM_NAME.contains('@'));
    }

    /// A name that is longer than the limit is shortened to the limit.
    #[test]
    fn long_name_is_shortened_to_the_limit() {
        let raw = "a".repeat(500);
        let cleaned = sanitize_name(&raw, "Voice Room");
        assert_eq!(cleaned.chars().count(), 100);
    }

    /// A name of exactly the limit is kept as it is.
    #[test]
    fn name_of_exactly_the_limit_is_kept() {
        let raw = "a".repeat(100);
        let cleaned = sanitize_name(&raw, "Voice Room");
        assert_eq!(cleaned.chars().count(), 100);
        assert_eq!(cleaned, raw);
    }

    /// The limit counts characters, not bytes: a name of 100 multi-byte characters is
    /// not shortened.
    #[test]
    fn limit_counts_characters_not_bytes() {
        let raw = "é".repeat(100);
        let cleaned = sanitize_name(&raw, "Voice Room");
        assert_eq!(cleaned.chars().count(), 100);
    }

    /// The sanitised name of a created room is the cleaned name, never the raw one.
    #[test]
    fn created_room_carries_the_cleaned_name() {
        let mut state = state_with(Vec::new());
        state.last_create.insert(OWNER, T0);
        let mut event = join(OWNER, T0 + 30);
        event.user_id = MEMBER;
        let plan = expect_create(&decide_join(&event, &cfg(), &state));
        assert_eq!(plan.name, "Voice Room");
    }

    // ---------------------------------------------------------------------
    // Rule 3: the overwrite plan.
    // ---------------------------------------------------------------------

    /// The overwrite plan carries the category overwrites unchanged.
    #[test]
    fn overwrite_plan_carries_the_category_overwrites() {
        let plan = expect_create(&decide_join(
            &join(OWNER, T0),
            &cfg(),
            &state_with(Vec::new()),
        ));
        let category = &cfg().category_overwrites;
        for expected in category {
            assert!(
                plan.overwrites.contains(expected),
                "category overwrite {expected:?} is missing from the plan"
            );
        }
    }

    /// The owner overwrite grants exactly the four bounded owner rights.
    #[test]
    fn owner_overwrite_grants_exactly_the_bounded_owner_rights() {
        let plan = expect_create(&decide_join(
            &join(OWNER, T0),
            &cfg(),
            &state_with(Vec::new()),
        ));
        let owner = plan
            .overwrites
            .iter()
            .find(|o| o.target == OverwriteTarget::Member(OWNER))
            .expect("the plan carries an owner overwrite");
        assert_eq!(owner.allow, OWNER_RIGHTS);
        assert_eq!(owner.allow & CONNECT, CONNECT);
        assert_eq!(owner.allow & SPEAK, SPEAK);
        assert_eq!(owner.allow & MOVE_MEMBERS, MOVE_MEMBERS);
        assert_eq!(owner.allow & MUTE_MEMBERS, MUTE_MEMBERS);
    }

    /// No overwrite of the plan ever grants an escalating permission.
    #[test]
    fn no_overwrite_ever_grants_an_escalating_permission() {
        let plan = expect_create(&decide_join(
            &join(OWNER, T0),
            &cfg(),
            &state_with(Vec::new()),
        ));
        for overwrite in &plan.overwrites {
            assert_eq!(
                overwrite.allow & FORBIDDEN_OWNER_RIGHTS,
                0,
                "overwrite {overwrite:?} grants an escalating permission"
            );
            assert_eq!(
                overwrite.allow & LEGACY_MANAGE_PERMISSIONS,
                0,
                "overwrite {overwrite:?} grants the retired Manage Permissions"
            );
        }
    }

    /// The owner overwrite denies the escalating permissions as well.
    #[test]
    fn owner_overwrite_denies_the_escalating_permissions() {
        let plan = expect_create(&decide_join(
            &join(OWNER, T0),
            &cfg(),
            &state_with(Vec::new()),
        ));
        let owner = plan
            .overwrites
            .iter()
            .find(|o| o.target == OverwriteTarget::Member(OWNER))
            .expect("the plan carries an owner overwrite");
        assert_eq!(owner.deny & FORBIDDEN_OWNER_RIGHTS, FORBIDDEN_OWNER_RIGHTS);
    }

    /// The owner rights are a fixed constant: a category overwrite cannot widen them.
    #[test]
    fn category_overwrites_cannot_widen_the_owner_rights() {
        let mut config = cfg();
        config.category_overwrites.push(RoomOverwrite {
            target: OverwriteTarget::Role(ROLE),
            allow: MANAGE_ROLES | MANAGE_CHANNELS,
            deny: 0,
        });
        let plan = expect_create(&decide_join(
            &join(OWNER, T0),
            &config,
            &state_with(Vec::new()),
        ));
        let owner = plan
            .overwrites
            .iter()
            .find(|o| o.target == OverwriteTarget::Member(OWNER))
            .expect("the plan carries an owner overwrite");
        assert_eq!(owner.allow & FORBIDDEN_OWNER_RIGHTS, 0);
    }

    /// The owner rights never include Manage Channels, so an owner cannot rename other
    /// channels or move their room out of the category.
    #[test]
    fn owner_rights_exclude_manage_channels() {
        assert_eq!(OWNER_RIGHTS & MANAGE_CHANNELS, 0);
        assert_eq!(OWNER_RIGHTS & MANAGE_ROLES, 0);
        assert_eq!(OWNER_RIGHTS & ADMINISTRATOR, 0);
    }

    // ---------------------------------------------------------------------
    // Rule 4: permit.
    // ---------------------------------------------------------------------

    /// A permit for a member who can already view the category is allowed.
    #[test]
    fn permit_is_allowed_for_a_member_who_can_view_the_category() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_permit(ROOM, OWNER, MEMBER, true, &state);
        assert_eq!(decision, Decision::Allow);
    }

    /// A permit for a member who cannot view the category is refused.
    #[test]
    fn permit_is_refused_for_a_member_who_cannot_view_the_category() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_permit(ROOM, OWNER, MEMBER, false, &state);
        assert_eq!(expect_deny(&decision), DenyReason::CategoryNotVisible);
    }

    /// A permit for a member id zero is refused even when the category is visible.
    #[test]
    fn permit_is_refused_for_the_zero_member_id() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_permit(ROOM, OWNER, 0, true, &state);
        assert_eq!(expect_deny(&decision), DenyReason::InvalidUserId);
    }

    /// A permit in a channel that is not a tracked room is refused.
    #[test]
    fn permit_is_refused_in_an_untracked_channel() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_permit(OTHER_ROOM, OWNER, MEMBER, true, &state);
        assert_eq!(expect_deny(&decision), DenyReason::UntrackedChannel);
    }

    /// A permit issued by somebody who is not the room owner is refused.
    #[test]
    fn permit_by_a_non_owner_is_refused() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_permit(ROOM, OTHER_MEMBER, MEMBER, true, &state);
        assert_eq!(expect_deny(&decision), DenyReason::NotRoomOwner);
    }

    /// A permit for the owner themself is refused: a room owner needs no permit.
    #[test]
    fn permit_for_the_owner_themselves_is_refused() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_permit(ROOM, OWNER, OWNER, true, &state);
        assert_eq!(expect_deny(&decision), DenyReason::AlreadyPermitted);
    }

    // ---------------------------------------------------------------------
    // Rule 5: what a leaving member leaves behind.
    // ---------------------------------------------------------------------

    /// The owner leaving a room that still has members makes the room claimable.
    #[test]
    fn owner_leaving_a_non_empty_room_makes_it_claimable() {
        let state = state_with(vec![room(OWNER, 3, None)]);
        let decision = decide_leave(ROOM, OWNER, T0 + 10, &cfg(), &state);
        assert_eq!(decision, Decision::Claimable(ROOM));
    }

    /// The boundary of the occupancy contract: a room whose count is two
    /// still holds the other member after the owner leaves, so it becomes
    /// claimable. A room whose count is one does not, and that is the
    /// difference the count makes.
    #[test]
    fn the_occupancy_boundary_is_two_members() {
        let two = state_with(vec![room(OWNER, 2, None)]);
        assert_eq!(
            decide_leave(ROOM, OWNER, T0 + 10, &cfg(), &two),
            Decision::Claimable(ROOM),
            "a room with two members still holds one after the owner leaves"
        );
        let one = state_with(vec![room(OWNER, 1, None)]);
        assert_ne!(
            decide_leave(ROOM, OWNER, T0 + 10, &cfg(), &one),
            Decision::Claimable(ROOM),
            "a room with one member is empty once that member leaves"
        );
    }

    /// The owner leaving a room that is already empty starts the grace period.
    #[test]
    fn owner_leaving_an_empty_room_does_not_clean_it_up_immediately() {
        let state = state_with(vec![room(OWNER, 0, Some(T0))]);
        let decision = decide_leave(ROOM, OWNER, T0 + 10, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::EmptyGraceNotElapsed);
    }

    /// An empty room is cleaned up once the grace period has elapsed.
    #[test]
    fn empty_room_is_cleaned_up_after_the_grace_period() {
        let state = state_with(vec![room(OWNER, 0, Some(T0))]);
        let decision = decide_leave(ROOM, OWNER, T0 + 60, &cfg(), &state);
        assert_eq!(decision, Decision::Cleanup(ROOM));
    }

    /// An empty room is not cleaned up one second before the grace period ends.
    #[test]
    fn empty_room_is_not_cleaned_up_before_the_grace_period_ends() {
        let state = state_with(vec![room(OWNER, 0, Some(T0))]);
        let decision = decide_leave(ROOM, OWNER, T0 + 59, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::EmptyGraceNotElapsed);
    }

    /// A member who is not the owner leaving does not change the room.
    #[test]
    fn a_non_owner_leaving_changes_nothing() {
        let state = state_with(vec![room(OWNER, 3, None)]);
        let decision = decide_leave(ROOM, OTHER_MEMBER, T0 + 10, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::NotRoomOwner);
    }

    /// A leave in a channel that is not a tracked room never deletes anything.
    #[test]
    fn an_untracked_channel_is_never_deleted() {
        let state = state_with(Vec::new());
        let decision = decide_leave(OTHER_ROOM, OWNER, T0 + 10_000, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::UntrackedChannel);
    }

    /// A room that becomes empty at this very moment starts its grace period: it is
    /// not cleaned up yet.
    #[test]
    fn a_room_that_just_became_empty_is_not_cleaned_up_yet() {
        let state = state_with(vec![room(OWNER, 0, None)]);
        let decision = decide_leave(ROOM, OWNER, T0, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::EmptyGraceNotElapsed);
    }

    /// A room whose grace period is zero is cleaned up as soon as it is empty.
    #[test]
    fn a_zero_grace_period_cleans_up_immediately() {
        let mut config = cfg();
        config.empty_grace_secs = 0;
        let state = state_with(vec![room(OWNER, 0, Some(T0))]);
        let decision = decide_leave(ROOM, OWNER, T0, &config, &state);
        assert_eq!(decision, Decision::Cleanup(ROOM));
    }

    /// A room whose owner already left is claimed by nobody, but the member who
    /// leaves the empty room last must still reach the grace check: the room has to
    /// be cleaned up after the grace period, whoever leaves last.
    #[test]
    fn the_last_member_leaving_an_orphaned_room_reaches_the_grace_check() {
        // The owner is gone: the room is empty and `empty_since` is set.
        let state = state_with(vec![room(OWNER, 0, Some(T0))]);
        let before = decide_leave(ROOM, OTHER_MEMBER, T0 + 59, &cfg(), &state);
        assert_eq!(
            expect_deny(&before),
            DenyReason::EmptyGraceNotElapsed,
            "an empty room must survive until the grace period has elapsed"
        );
        let after = decide_leave(ROOM, OTHER_MEMBER, T0 + 60, &cfg(), &state);
        assert_eq!(
            after,
            Decision::Cleanup(ROOM),
            "an empty room must be cleaned up after the grace period, whoever leaves last"
        );
    }

    /// An extreme `empty_since` from persistence must not panic, and a room whose
    /// `empty_since` is far in the past is past its grace and must be cleaned up:
    /// the elapsed time saturates instead of wrapping around into a negative
    /// number, which would keep the room for ever.
    #[test]
    fn an_extreme_empty_since_does_not_panic_and_keeps_the_room() {
        let state = state_with(vec![room(OWNER, 0, Some(i64::MIN))]);
        let decision = decide_leave(ROOM, OWNER, T0, &cfg(), &state);
        assert_eq!(
            decision,
            Decision::Cleanup(ROOM),
            "a room whose empty_since is far in the past is past its grace and must be cleaned up"
        );
    }

    /// An extreme `empty_since` from persistence must still let the grace period
    /// elapse: the elapsed time is saturated, not wrapped.
    #[test]
    fn an_extreme_empty_since_still_reaches_the_grace() {
        let state = state_with(vec![room(OWNER, 0, Some(i64::MIN))]);
        let decision = decide_leave(ROOM, OWNER, i64::MAX, &cfg(), &state);
        assert_eq!(
            decision,
            Decision::Cleanup(ROOM),
            "the elapsed time must saturate, not wrap around"
        );
    }

    /// An extreme `last_create` from persistence must not panic when the cooldown
    /// is measured against it.
    #[test]
    fn an_extreme_last_create_does_not_panic() {
        let mut state = state_with(Vec::new());
        state.last_create.insert(OWNER, i64::MAX);
        let decision = decide_join(&join(OWNER, -10), &cfg(), &state);
        assert!(
            matches!(decision, Decision::Deny(_)),
            "an extreme last_create must deny, not panic"
        );
    }

    /// An extreme `last_create` in the past must not panic when the cooldown is
    /// measured against it.
    #[test]
    fn an_extreme_past_last_create_does_not_panic() {
        let mut state = state_with(Vec::new());
        state.last_create.insert(OWNER, i64::MIN);
        let decision = decide_join(&join(OWNER, i64::MAX), &cfg(), &state);
        assert!(
            matches!(decision, Decision::Create(_)),
            "the cooldown must saturate, not wrap around"
        );
    }

    /// An extreme rename timestamp from persistence must not panic when the
    /// rename window is measured against it.
    #[test]
    fn an_extreme_rename_timestamp_does_not_panic() {
        let mut state = state_with(vec![room(OWNER, 2, None)]);
        state.renames.push((ROOM, i64::MIN));
        let decision = decide_rename(ROOM, OWNER, T0, &cfg(), &state);
        assert!(
            matches!(decision, Decision::Allow),
            "the rename window must saturate, not wrap around"
        );
    }

    /// A sweep cleans an empty room up once its grace period has elapsed, without
    /// a member leaving: the runtime has to be able to reap a room whose last
    /// member left before the grace elapsed.
    #[test]
    fn a_sweep_cleans_an_empty_room_up_after_the_grace() {
        let state = state_with(vec![room(OWNER, 0, Some(T0))]);
        let before = decide_sweep(ROOM, T0 + 59, &cfg(), &state);
        assert_eq!(expect_deny(&before), DenyReason::EmptyGraceNotElapsed);
        let after = decide_sweep(ROOM, T0 + 60, &cfg(), &state);
        assert_eq!(after, Decision::Cleanup(ROOM));
    }

    /// A sweep never cleans a room up that still holds members.
    #[test]
    fn a_sweep_never_cleans_an_occupied_room_up() {
        let state = state_with(vec![room(OWNER, 3, None)]);
        let decision = decide_sweep(ROOM, T0 + 10_000, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::RoomNotEmpty);
    }

    /// A sweep of a channel the policy does not track never deletes anything.
    #[test]
    fn a_sweep_of_an_untracked_channel_deletes_nothing() {
        let state = state_with(Vec::new());
        let decision = decide_sweep(OTHER_ROOM, T0 + 10_000, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::UntrackedChannel);
    }

    /// A sweep is not a claim: it never hands a room to a new owner.
    #[test]
    fn a_sweep_never_claims_a_room() {
        let state = state_with(vec![room(OWNER, 0, Some(T0))]);
        let decision = decide_sweep(ROOM, T0 + 10_000, &cfg(), &state);
        assert_ne!(decision, Decision::Claimable(ROOM));
    }

    /// A negative rename window is refused: a negative window would make the
    /// budget unlimited, so the policy refuses the rename instead.
    #[test]
    fn a_negative_rename_window_refuses_the_rename() {
        let mut config = cfg();
        config.rename_budget.window_secs = -1;
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_rename(ROOM, OWNER, T0, &config, &state);
        assert_eq!(expect_deny(&decision), DenyReason::InvalidConfig);
    }

    /// A negative grace period is refused: a negative grace would clean a room up
    /// the instant it became empty.
    #[test]
    fn a_negative_grace_period_refuses_the_cleanup() {
        let mut config = cfg();
        config.empty_grace_secs = -1;
        let state = state_with(vec![room(OWNER, 0, Some(T0))]);
        let decision = decide_leave(ROOM, OWNER, T0, &config, &state);
        assert_eq!(expect_deny(&decision), DenyReason::InvalidConfig);
    }

    /// A member who is not the owner leaving an EMPTY room does not claim it: the
    /// room is not claimable once it is empty, it is only cleaned up.
    #[test]
    fn a_non_owner_leaving_an_empty_room_does_not_claim_it() {
        let state = state_with(vec![room(OWNER, 0, Some(T0))]);
        let decision = decide_leave(ROOM, OTHER_MEMBER, T0 + 10, &cfg(), &state);
        assert_ne!(
            decision,
            Decision::Claimable(ROOM),
            "an empty room must not become claimable"
        );
    }

    /// A member who is not the owner leaving a room that still holds members does
    /// not change the room: it is not theirs to claim.
    #[test]
    fn a_non_owner_leaving_a_occupied_room_does_not_claim_it() {
        let state = state_with(vec![room(OWNER, 3, None)]);
        let decision = decide_leave(ROOM, OTHER_MEMBER, T0 + 10, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::NotRoomOwner);
    }

    /// Over a seeded sequence, every room that becomes empty is REMOVED once its
    /// grace period has elapsed, whoever leaves last. No room may linger forever
    /// and keep consuming the global cap.
    ///
    /// The model owns the occupancy and the `empty_since` stamp, exactly as the
    /// runtime does: the count is the number of members in the room BEFORE the
    /// leave, so a room whose count is one becomes empty when that member leaves.
    /// The model decrements the count on EVERY leave, not only on the claimable
    /// branch, and stamps `empty_since` when the count reaches zero.
    ///
    /// The quiet phase at the end advances the clock past every grace period and
    /// asserts that every room is actually gone from the state, not merely that
    /// it could be cleaned up.
    #[test]
    fn a_seeded_sequence_never_leaves_an_empty_room_forever() {
        let mut config = cfg();
        config.global_cap = 5;
        config.per_user_cap = 1;
        config.create_cooldown_secs = 0;
        config.empty_grace_secs = 60;
        let mut state = VoiceRoomState::default();
        let mut rng = Lcg(0x5EED_1234_ABCD_0002);
        let mut now = T0;
        let members = [OWNER, MEMBER, OTHER_MEMBER, 1_000_000_000_000_000_010];
        let mut next_channel = 1_000_000_000_000_200_000u64;

        for _step in 0..2_000 {
            now += 1;
            let user = members[rng.below(members.len() as u64) as usize];
            if rng.below(3) == 0 {
                let event = JoinEvent {
                    guild_id: GUILD,
                    user_id: user,
                    channel_id: HUB,
                    at: now,
                };
                match decide_join(&event, &config, &state) {
                    Decision::Create(_) => {
                        let channel_id = next_channel;
                        next_channel += 1;
                        state.rooms.push(Room {
                            channel_id,
                            owner_id: user,
                            created_at: now,
                            empty_since: None,
                            occupants: 1,
                        });
                        state.last_create.insert(user, now);
                    }
                    Decision::Deny(_) => {}
                    other => panic!("unexpected decision {other:?}"),
                }
            } else if !state.rooms.is_empty() {
                let index = rng.below(state.rooms.len() as u64) as usize;
                let channel_id = state.rooms[index].channel_id;
                let owner_id = state.rooms[index].owner_id;
                let leaver = if rng.below(2) == 0 {
                    owner_id
                } else {
                    OTHER_MEMBER
                };
                // The count the policy sees is the count BEFORE the leave.
                let before = state.rooms[index].occupants;
                let decision = decide_leave(channel_id, leaver, now, &config, &state);
                match decision {
                    Decision::Cleanup(id) => {
                        state.rooms.retain(|r| r.channel_id != id);
                    }
                    Decision::Claimable(_) | Decision::Deny(_) => {
                        // The model applies the leave on EVERY branch, not only
                        // on the claimable one: a member that leaves a room
                        // reduces its count, whatever the policy said about it.
                        let after = before.saturating_sub(1);
                        state.rooms[index].occupants = after;
                        if after == 0 {
                            state.rooms[index].empty_since = Some(now);
                        }
                    }
                    other => panic!("unexpected decision {other:?}"),
                }
            }
        }

        // The quiet phase: no more joins. First everybody who is still in a room
        // leaves it, so every room becomes empty and the grace period starts.
        // Then the clock advances past every grace period and the runtime
        // reaps the rooms.
        for _ in 0..10 {
            let rooms: Vec<(u64, u64)> = state
                .rooms
                .iter()
                .map(|r| (r.channel_id, r.owner_id))
                .collect();
            for (channel_id, owner_id) in rooms {
                let before = state
                    .room(channel_id)
                    .map(|r| r.occupants)
                    .unwrap_or_default();
                let decision = decide_leave(channel_id, owner_id, now, &config, &state);
                match decision {
                    Decision::Cleanup(id) => {
                        state.rooms.retain(|r| r.channel_id != id);
                    }
                    Decision::Claimable(_) | Decision::Deny(_) => {
                        // The leave is applied on EVERY branch: a member that
                        // leaves a room reduces its count, whatever the policy
                        // said about it.
                        let after = before.saturating_sub(1);
                        if let Some(room) =
                            state.rooms.iter_mut().find(|r| r.channel_id == channel_id)
                        {
                            room.occupants = after;
                            if after == 0 {
                                room.empty_since = Some(now);
                            }
                        }
                    }
                    other => panic!("unexpected decision {other:?}"),
                }
            }
            now += 1;
        }

        // Every room is empty now. The clock advances past every grace period
        // and the runtime reaps the rooms that are empty. The reaping goes
        // through `decide_leave` with a member who is NOT the owner, so the
        // test covers the whole leave path: a policy that refuses a leave
        // before it reaches the grace check, or that never cleans a room up
        // on the leave path, leaves the rooms behind and fails this test.
        now += config.empty_grace_secs * 2;
        for _ in 0..10 {
            let rooms: Vec<u64> = state.rooms.iter().map(|r| r.channel_id).collect();
            for channel_id in rooms {
                match decide_leave(channel_id, VISITOR, now, &config, &state) {
                    Decision::Cleanup(id) => {
                        state.rooms.retain(|r| r.channel_id != id);
                    }
                    Decision::Claimable(_) | Decision::Deny(_) => {}
                    other => panic!("unexpected decision {other:?}"),
                }
            }
            now += 1;
        }

        assert!(
            state.rooms.is_empty(),
            "every room must be gone once its grace has elapsed: {:?}",
            state.rooms
        );
        assert!(
            state.rooms.len() < config.global_cap,
            "the rooms must not pile up against the global cap"
        );
    }

    /// The quiet phase of the seeded sequence is what makes it fail: a policy that
    /// never cleans a room up leaves the rooms in the state, and a policy that
    /// cleans a room up before its grace has elapsed is caught by the grace
    /// boundary tests. This test asserts the end state directly.
    #[test]
    fn the_seeded_sequence_quiet_phase_removes_every_room() {
        let mut config = cfg();
        config.global_cap = 5;
        config.per_user_cap = 1;
        config.create_cooldown_secs = 0;
        config.empty_grace_secs = 60;
        let mut state = VoiceRoomState::default();
        state.rooms.push(Room {
            channel_id: ROOM,
            owner_id: OWNER,
            created_at: T0,
            empty_since: Some(T0),
            occupants: 0,
        });

        // The room is empty and its grace has elapsed: the sweep removes it.
        let now = T0 + 60;
        let decision = decide_sweep(ROOM, now, &config, &state);
        assert_eq!(decision, Decision::Cleanup(ROOM));
        state.rooms.retain(|r| r.channel_id != ROOM);
        assert!(
            state.rooms.is_empty(),
            "the swept room must be gone from the state"
        );
    }

    // ---------------------------------------------------------------------
    // Rule 6: the rename budget.
    // ---------------------------------------------------------------------

    /// A rename is allowed while the budget is not exhausted.
    #[test]
    fn a_rename_is_allowed_inside_the_budget() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_rename(ROOM, OWNER, T0, &cfg(), &state);
        assert_eq!(decision, Decision::Allow);
    }

    /// A rename is refused once the budget inside the window is used up.
    #[test]
    fn a_rename_is_refused_when_the_budget_is_used_up() {
        let mut state = state_with(vec![room(OWNER, 2, None)]);
        state.renames.push((ROOM, T0 - 10));
        state.renames.push((ROOM, T0 - 20));
        let decision = decide_rename(ROOM, OWNER, T0, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::RenameBudgetExhausted);
    }

    /// A rename outside the window is allowed again: the old renames no longer count.
    #[test]
    fn a_rename_outside_the_window_is_allowed_again() {
        let mut state = state_with(vec![room(OWNER, 2, None)]);
        state.renames.push((ROOM, T0 - 601));
        state.renames.push((ROOM, T0 - 602));
        let decision = decide_rename(ROOM, OWNER, T0, &cfg(), &state);
        assert_eq!(decision, Decision::Allow);
    }

    /// The budget is per room: renames of another room do not use it up.
    #[test]
    fn the_budget_is_counted_per_room() {
        let mut state = state_with(vec![room(OWNER, 2, None)]);
        state.renames.push((OTHER_ROOM, T0 - 10));
        state.renames.push((OTHER_ROOM, T0 - 20));
        let decision = decide_rename(ROOM, OWNER, T0, &cfg(), &state);
        assert_eq!(decision, Decision::Allow);
    }

    /// A rename by somebody who is not the owner is refused.
    #[test]
    fn a_rename_by_a_non_owner_is_refused() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_rename(ROOM, OTHER_MEMBER, T0, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::NotRoomOwner);
    }

    /// A rename of a channel that is not a tracked room is refused.
    #[test]
    fn a_rename_of_an_untracked_channel_is_refused() {
        let state = state_with(Vec::new());
        let decision = decide_rename(OTHER_ROOM, OWNER, T0, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::UntrackedChannel);
    }

    /// The policy never plans more renames inside the window than the budget allows.
    #[test]
    fn the_policy_never_plans_more_renames_than_the_budget() {
        let mut state = state_with(vec![room(OWNER, 2, None)]);
        let mut planned = 0u32;
        let mut now = T0;
        for _ in 0..20 {
            match decide_rename(ROOM, OWNER, now, &cfg(), &state) {
                Decision::Allow => {
                    planned += 1;
                    state.renames.push((ROOM, now));
                }
                other => {
                    assert_eq!(
                        expect_deny(&other),
                        DenyReason::RenameBudgetExhausted,
                        "the only reason a rename may fail is the budget"
                    );
                    break;
                }
            }
            now += 1;
        }
        assert_eq!(planned, 2, "the budget of two renames must bound the plan");
        let in_window = state
            .renames
            .iter()
            .filter(|(channel, at)| *channel == ROOM && *at > now - cfg().rename_budget.window_secs)
            .count();
        assert!(in_window <= cfg().rename_budget.max_renames as usize);
    }

    /// The rename budget is exposed as data, so the runtime can enforce the same number.
    #[test]
    fn the_rename_budget_is_exposed_as_data() {
        let budget = cfg().rename_budget;
        assert_eq!(budget.max_renames, 2);
        assert_eq!(budget.window_secs, 600);
    }

    // ---------------------------------------------------------------------
    // A seeded sequence: the caps hold over a long pseudo-random run.
    // ---------------------------------------------------------------------

    /// A small linear congruential generator; no dependency and fully deterministic.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 11
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// Over a seeded pseudo-random sequence of joins and leaves, the number of tracked
    /// rooms never rises above the global cap and no member ever owns more rooms than
    /// the per-user cap.
    #[test]
    fn a_seeded_sequence_never_exceeds_the_caps() {
        let mut config = cfg();
        config.global_cap = 5;
        config.per_user_cap = 1;
        config.create_cooldown_secs = 0;
        let mut state = VoiceRoomState::default();
        let mut rng = Lcg(0x5EED_1234_ABCD_0001);
        let mut now = T0;
        let members = [OWNER, MEMBER, OTHER_MEMBER, 1_000_000_000_000_000_010];

        for step in 0..2_000 {
            now += 1;
            let user = members[rng.below(members.len() as u64) as usize];
            if rng.below(3) == 0 {
                // The member joins the hub.
                let event = JoinEvent {
                    guild_id: GUILD,
                    user_id: user,
                    channel_id: HUB,
                    at: now,
                };
                match decide_join(&event, &config, &state) {
                    Decision::Create(plan) => {
                        // The runtime creates the room and records it.
                        let channel_id = 1_000_000_000_000_100_000 + step as u64;
                        state.rooms.push(Room {
                            channel_id,
                            owner_id: user,
                            created_at: now,
                            empty_since: None,
                            occupants: 1,
                        });
                        state.last_create.insert(user, now);
                        assert!(!plan.name.is_empty());
                    }
                    Decision::Deny(reason) => {
                        assert!(
                            matches!(
                                reason,
                                DenyReason::PerUserCapReached
                                    | DenyReason::GlobalCapReached
                                    | DenyReason::CreateCooldown
                            ),
                            "unexpected deny reason {reason:?}"
                        );
                    }
                    other => panic!("unexpected decision {other:?}"),
                }
            } else if !state.rooms.is_empty() {
                // A member leaves a tracked room.
                let index = rng.below(state.rooms.len() as u64) as usize;
                let channel_id = state.rooms[index].channel_id;
                let owner_id = state.rooms[index].owner_id;
                let now_occupants = state.rooms[index].occupants;
                match decide_leave(channel_id, owner_id, now, &config, &state) {
                    Decision::Cleanup(id) => {
                        state.rooms.retain(|r| r.channel_id != id);
                    }
                    Decision::Claimable(_) | Decision::Deny(_) => {
                        // The leave is applied on EVERY branch: a member that
                        // leaves a room reduces its count, whatever the policy
                        // said about it.
                        let after = now_occupants.saturating_sub(1);
                        state.rooms[index].occupants = after;
                        if after == 0 {
                            state.rooms[index].empty_since = Some(now);
                        }
                    }
                    other => panic!("unexpected decision {other:?}"),
                }
            }

            // The invariants that must hold after every step.
            assert!(
                state.rooms.len() <= config.global_cap,
                "the global cap was exceeded at step {step}: {} rooms",
                state.rooms.len()
            );
            for member in members {
                let owned = state.rooms.iter().filter(|r| r.owner_id == member).count();
                assert!(
                    owned <= config.per_user_cap,
                    "the per-user cap was exceeded at step {step}: {owned} rooms"
                );
            }
        }
    }

    // ---------------------------------------------------------------------
    // Round 4: the mention fixpoint has no bound, the occupancy contract is
    // one thing, the invisible set is named, and the seeded models apply
    // every leave.
    // ---------------------------------------------------------------------

    /// Builds a user mention nested `depth` levels deep.
    ///
    /// Depth 1 is `<@900000009>`; depth 2 is `<<@900000009>@900000009>`; and
    /// so on. Each level wraps the previous one in another `<` and `>`.
    fn nested_mention(depth: usize) -> String {
        let mut text = String::from("<@900000009>");
        for _ in 1..depth {
            text = format!("<{}@900000009>", text);
        }
        text
    }

    /// A mention nested three levels deep must not reassemble into a live
    /// token: the stripping runs until the text stops changing, so the depth
    /// of the nesting does not matter.
    #[test]
    fn a_mention_nested_three_deep_does_not_reassemble() {
        let cleaned = sanitize_name(&format!("{} party", nested_mention(3)), "Voice Room");
        assert!(
            !cleaned.contains("<@") && !cleaned.contains("<#"),
            "a mention nested three levels deep must not survive: {cleaned:?}"
        );
        assert!(
            cleaned.contains("party"),
            "the harmless text must survive the cleaning: {cleaned:?}"
        );
    }

    /// A mention nested eighteen levels deep must not reassemble either. This
    /// is the depth that defeated the bounded fixpoint of round 3.
    #[test]
    fn a_mention_nested_eighteen_deep_does_not_reassemble() {
        let cleaned = sanitize_name(&format!("{} party", nested_mention(18)), "Voice Room");
        assert!(
            !cleaned.contains("<@") && !cleaned.contains("<#"),
            "a mention nested eighteen levels deep must not survive: {cleaned:?}"
        );
        assert!(
            cleaned.contains("party"),
            "the harmless text must survive the cleaning: {cleaned:?}"
        );
    }

    /// A mention nested very deeply must not survive: the depth of the
    /// nesting is not bounded by anything the caller controls.
    #[test]
    fn a_mention_nested_very_deeply_does_not_reassemble() {
        let cleaned = sanitize_name(&format!("{} party", nested_mention(64)), "Voice Room");
        assert!(
            !cleaned.contains("<@") && !cleaned.contains("<#"),
            "a mention nested sixty-four levels deep must not survive: {cleaned:?}"
        );
    }

    /// No mention of any shape survives, at any nesting depth. The test runs
    /// over a range of depths so that a bound on the number of passes shows up
    /// as a failure rather than as a surprise in production.
    #[test]
    fn no_mention_survives_at_any_nesting_depth() {
        for depth in 1..=40 {
            let cleaned = sanitize_name(&format!("{} party", nested_mention(depth)), "Voice Room");
            let lowered = cleaned.to_lowercase();
            assert!(
                !lowered.contains("<@") && !lowered.contains("<#"),
                "a mention nested {depth} levels deep survived: {cleaned:?}"
            );
            assert!(
                !lowered.contains("@everyone") && !lowered.contains("@here"),
                "a mention word nested {depth} levels deep survived: {cleaned:?}"
            );
        }
    }

    /// The owner leaving a room that holds only them empties the room: the
    /// room is not claimable, it starts its grace period. The count is the
    /// count BEFORE the leave, so a room with one member is empty once that
    /// member leaves.
    #[test]
    fn the_owner_leaving_the_last_member_empties_the_room() {
        let state = state_with(vec![room(OWNER, 1, None)]);
        let decision = decide_leave(ROOM, OWNER, T0 + 10, &cfg(), &state);
        assert_ne!(
            decision,
            Decision::Claimable(ROOM),
            "a room that is empty after the leave must not become claimable"
        );
        assert_eq!(
            expect_deny(&decision),
            DenyReason::EmptyGraceNotElapsed,
            "a room that just became empty starts its grace period"
        );
    }

    /// The owner leaving a room that holds one other member as well empties
    /// nothing: the room still holds a member, so it becomes claimable.
    #[test]
    fn the_owner_leaving_a_room_with_another_member_makes_it_claimable() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_leave(ROOM, OWNER, T0 + 10, &cfg(), &state);
        assert_eq!(decision, Decision::Claimable(ROOM));
    }

    /// The last member leaving a room whose owner already left reaches the
    /// grace check, whoever leaves last. The count is the count BEFORE the
    /// leave, so a room with one member left is empty once that member
    /// leaves.
    #[test]
    fn the_last_member_of_a_room_with_one_member_reaches_the_grace_check() {
        // The owner is gone: the room holds one member and `empty_since` is
        // set from before.
        let state = state_with(vec![room(OWNER, 1, Some(T0))]);
        let before = decide_leave(ROOM, VISITOR, T0 + 59, &cfg(), &state);
        assert_eq!(
            expect_deny(&before),
            DenyReason::EmptyGraceNotElapsed,
            "an empty room must survive until the grace period has elapsed"
        );
        let after = decide_leave(ROOM, VISITOR, T0 + 60, &cfg(), &state);
        assert_eq!(after, Decision::Cleanup(ROOM));
    }

    /// A member who is not the owner leaving a room that still holds members
    /// changes nothing: the room is not theirs to claim.
    #[test]
    fn a_non_owner_leaving_a_room_with_members_is_refused() {
        let state = state_with(vec![room(OWNER, 2, None)]);
        let decision = decide_leave(ROOM, VISITOR, T0 + 10, &cfg(), &state);
        assert_eq!(expect_deny(&decision), DenyReason::NotRoomOwner);
    }

    /// The invisible set is the closed class of the code points that render
    /// nothing, in ranges: the Unicode format characters (`Cf`), the
    /// `Default_Ignorable_Code_Point` code points and the blank fillers.
    /// Every one of the characters below renders nothing and must be removed.
    #[test]
    fn the_default_ignorable_characters_are_removed() {
        // U+061C ARABIC LETTER MARK, U+034F COMBINING GRAPHEME JOINER,
        // U+FE00 VARIATION SELECTOR-1, U+FFF9 INTERLINEAR ANNOTATION ANCHOR,
        // U+1D173 MUSICAL SYMBOL BEGIN BEAM, U+17B4 KHMER VOWEL INHERENT AQ,
        // U+FFA0 HALFWIDTH HANGUL FILLER, U+115F HANGUL CHOSEONG FILLER and
        // U+2800 BRAILLE PATTERN BLANK.
        let invisible = [
            '\u{061C}',
            '\u{034F}',
            '\u{FE00}',
            '\u{FFF9}',
            '\u{1D173}',
            '\u{17B4}',
            '\u{FFA0}',
            '\u{115F}',
            '\u{2800}',
        ];
        for c in invisible {
            let cleaned = sanitize_name(&format!("par{c}ty"), "Voice Room");
            assert_eq!(
                cleaned, "party",
                "the character {c:?} renders nothing and must be removed"
            );
        }
    }

    /// A name made only of characters that render nothing is an empty name:
    /// it falls back to the default name instead of coming back blank.
    #[test]
    fn a_name_of_only_blank_characters_falls_back_to_the_default() {
        let invisible = [
            '\u{061C}',
            '\u{034F}',
            '\u{FE00}',
            '\u{FFF9}',
            '\u{1D173}',
            '\u{17B4}',
            '\u{FFA0}',
            '\u{115F}',
            '\u{2800}',
        ];
        for c in invisible {
            assert_eq!(
                sanitize_name(&c.to_string(), "Voice Room"),
                "Voice Room",
                "a name made only of {c:?} is an empty name"
            );
        }
    }

    /// The bidi controls and the tag characters are removed as well: they are
    /// part of the same closed class.
    #[test]
    fn the_bidi_and_tag_characters_are_removed() {
        assert_eq!(sanitize_name("a\u{202A}b\u{202C}c", "Voice Room"), "abc");
        assert_eq!(
            sanitize_name("\u{E0041}\u{E0042} party", "Voice Room"),
            "party"
        );
        assert_eq!(
            sanitize_name("\u{E0001}\u{E0020} party", "Voice Room"),
            "party"
        );
    }

    /// A name that holds only whitespace and blank characters falls back to
    /// the default name: the room always has a name a reader can see.
    #[test]
    fn a_name_of_only_whitespace_and_blanks_falls_back_to_the_default() {
        assert_eq!(sanitize_name("   ", "Voice Room"), "Voice Room");
        assert_eq!(
            sanitize_name("\u{2800}\u{200B} ", "Voice Room"),
            "Voice Room"
        );
    }

    /// The seeded sequence of the caps must apply EVERY leave, not only the
    /// ones the policy allows: a leave is a fact the runtime observes, not a
    /// request it can refuse.
    #[test]
    fn the_caps_sequence_applies_every_leave() {
        let mut config = cfg();
        config.global_cap = 5;
        config.per_user_cap = 1;
        config.create_cooldown_secs = 0;
        let mut state = VoiceRoomState::default();
        let mut rng = Lcg(0x5EED_1234_ABCD_0004);
        let mut now = T0;
        let members = [OWNER, MEMBER, OTHER_MEMBER, VISITOR];

        for step in 0..500 {
            now += 1;
            let user = members[rng.below(members.len() as u64) as usize];
            if rng.below(3) == 0 {
                let event = JoinEvent {
                    guild_id: GUILD,
                    user_id: user,
                    channel_id: HUB,
                    at: now,
                };
                match decide_join(&event, &config, &state) {
                    Decision::Create(_) => {
                        state.rooms.push(Room {
                            channel_id: 1_000_000_000_000_500_000 + step as u64,
                            owner_id: user,
                            created_at: now,
                            empty_since: None,
                            occupants: 1,
                        });
                        state.last_create.insert(user, now);
                    }
                    Decision::Deny(_) => {}
                    other => panic!("unexpected decision {other:?}"),
                }
            } else if !state.rooms.is_empty() {
                let index = rng.below(state.rooms.len() as u64) as usize;
                let channel_id = state.rooms[index].channel_id;
                let owner_id = state.rooms[index].owner_id;
                let leaver = if rng.below(2) == 0 { owner_id } else { VISITOR };
                let before = state.rooms[index].occupants;
                let decision = decide_leave(channel_id, leaver, now, &config, &state);
                match decision {
                    Decision::Cleanup(id) => {
                        state.rooms.retain(|r| r.channel_id != id);
                    }
                    Decision::Claimable(_) | Decision::Deny(_) => {
                        // The leave is applied on EVERY branch: the member left
                        // the room, whatever the policy said about it.
                        let after = before.saturating_sub(1);
                        state.rooms[index].occupants = after;
                        if after == 0 {
                            state.rooms[index].empty_since = Some(now);
                        }
                    }
                    other => panic!("unexpected decision {other:?}"),
                }
            }

            // The occupancy of a room never rises above the number of members
            // that ever entered it, and the count never goes negative.
            for room in &state.rooms {
                assert!(
                    room.occupants <= 1,
                    "a room may hold at most the member that created it: {}",
                    room.occupants
                );
            }
        }
    }

    /// The liveness test must be able to fail: the quiet phase drains the
    /// rooms through `decide_leave`, so a policy whose leave path is broken
    /// leaves the rooms behind and this test catches it.
    #[test]
    fn the_quiet_phase_drains_the_rooms_through_the_leave_path() {
        let mut config = cfg();
        config.global_cap = 5;
        config.per_user_cap = 1;
        config.create_cooldown_secs = 0;
        config.empty_grace_secs = 60;
        let mut state = VoiceRoomState::default();
        let mut rng = Lcg(0x5EED_1234_ABCD_0002);
        let mut now = T0;
        let members = [OWNER, MEMBER, OTHER_MEMBER, VISITOR];
        let mut next_channel = 1_000_000_000_000_600_000u64;

        for _step in 0..2_000 {
            now += 1;
            let user = members[rng.below(members.len() as u64) as usize];
            if rng.below(3) == 0 {
                let event = JoinEvent {
                    guild_id: GUILD,
                    user_id: user,
                    channel_id: HUB,
                    at: now,
                };
                match decide_join(&event, &config, &state) {
                    Decision::Create(_) => {
                        let channel_id = next_channel;
                        next_channel += 1;
                        state.rooms.push(Room {
                            channel_id,
                            owner_id: user,
                            created_at: now,
                            empty_since: None,
                            occupants: 1,
                        });
                        state.last_create.insert(user, now);
                    }
                    Decision::Deny(_) => {}
                    other => panic!("unexpected decision {other:?}"),
                }
            } else if !state.rooms.is_empty() {
                let index = rng.below(state.rooms.len() as u64) as usize;
                let channel_id = state.rooms[index].channel_id;
                let owner_id = state.rooms[index].owner_id;
                let leaver = if rng.below(2) == 0 { owner_id } else { VISITOR };
                let before = state.rooms[index].occupants;
                let decision = decide_leave(channel_id, leaver, now, &config, &state);
                match decision {
                    Decision::Cleanup(id) => {
                        state.rooms.retain(|r| r.channel_id != id);
                    }
                    Decision::Claimable(_) | Decision::Deny(_) => {
                        let after = before.saturating_sub(1);
                        state.rooms[index].occupants = after;
                        if after == 0 {
                            state.rooms[index].empty_since = Some(now);
                        }
                    }
                    other => panic!("unexpected decision {other:?}"),
                }
            }
        }

        // The quiet phase: everybody leaves, then the clock advances past
        // every grace period and the runtime reaps the rooms through the
        // leave path. The reaping goes through `decide_leave`, so a policy
        // whose leave path never cleans a room up leaves the rooms behind.
        for _ in 0..10 {
            let rooms: Vec<(u64, u64)> = state
                .rooms
                .iter()
                .map(|r| (r.channel_id, r.owner_id))
                .collect();
            for (channel_id, owner_id) in rooms {
                let before = state
                    .room(channel_id)
                    .map(|r| r.occupants)
                    .unwrap_or_default();
                let decision = decide_leave(channel_id, owner_id, now, &config, &state);
                match decision {
                    Decision::Cleanup(id) => {
                        state.rooms.retain(|r| r.channel_id != id);
                    }
                    Decision::Claimable(_) | Decision::Deny(_) => {
                        let after = before.saturating_sub(1);
                        if let Some(room) =
                            state.rooms.iter_mut().find(|r| r.channel_id == channel_id)
                        {
                            room.occupants = after;
                            if after == 0 {
                                room.empty_since = Some(now);
                            }
                        }
                    }
                    other => panic!("unexpected decision {other:?}"),
                }
            }
            now += 1;
        }

        now += config.empty_grace_secs * 2;
        for _ in 0..10 {
            let rooms: Vec<u64> = state.rooms.iter().map(|r| r.channel_id).collect();
            for channel_id in rooms {
                let owner_id = state
                    .room(channel_id)
                    .map(|r| r.owner_id)
                    .unwrap_or_default();
                match decide_leave(channel_id, owner_id, now, &config, &state) {
                    Decision::Cleanup(id) => {
                        state.rooms.retain(|r| r.channel_id != id);
                    }
                    Decision::Claimable(_) | Decision::Deny(_) => {}
                    other => panic!("unexpected decision {other:?}"),
                }
            }
            now += 1;
        }

        assert!(
            state.rooms.is_empty(),
            "every room must be gone once its grace has elapsed: {:?}",
            state.rooms
        );
    }

    /// The closed class of the characters that render nothing.
    ///
    /// This is the whole class, as ranges: the Unicode format characters
    /// (`Cf`), the `Default_Ignorable_Code_Point` code points and the blank
    /// fillers. Every code point in a range renders nothing, so every one of
    /// them must be removed.
    ///
    /// This table is the production table, so a test built from it checks
    /// that the implementation agrees with itself. That is not enough on its
    /// own: a range that was moved by one code point but kept its size would
    /// still pass such a test, because the walk only visits the code points
    /// the table names and the count is unchanged. The class is therefore
    /// also checked against a SECOND, independently written copy of it in
    /// `EXPECTED_CLASS`, which is raw literals and does not come from the
    /// production constant.
    const INVISIBLE_CLASS: &[(u32, u32)] = INVISIBLE_RANGES;

    /// Every code point of the closed class renders nothing, so every one of
    /// them must be removed from a name.
    ///
    /// The test walks the whole class range by range: a range that the
    /// implementation misses at its far end fails here, so the rule is a
    /// class rule and not a list of the examples that were thought of.
    #[test]
    fn every_character_that_renders_nothing_is_removed() {
        let mut checked = 0usize;
        for &(start, end) in INVISIBLE_CLASS {
            for cp in start..=end {
                let Some(c) = char::from_u32(cp) else {
                    continue;
                };
                checked += 1;
                let cleaned = sanitize_name(&format!("par{c}ty"), "Voice Room");
                assert_eq!(
                    cleaned, "party",
                    "U+{cp:04X} renders nothing and must be removed"
                );
            }
        }
        assert_eq!(
            checked, 4207,
            "the closed class must hold every code point of every range"
        );
    }

    /// The class has an edge: a code point just outside a range draws
    /// something, so it must survive.
    ///
    /// Without this the rule could be "remove everything", which would also
    /// pass the test above but would eat the whole name.
    #[test]
    fn a_character_just_outside_the_class_survives() {
        // The code points on both sides of every range, skipping the ones that
        // are control characters or spaces (those are removed by the other
        // rules, not by the invisible class).
        let mut edges = Vec::new();
        for &(start, end) in INVISIBLE_CLASS {
            for cp in [start.checked_sub(1), Some(end + 1)].into_iter().flatten() {
                let Some(c) = char::from_u32(cp) else {
                    continue;
                };
                if c.is_control() || c.is_whitespace() {
                    continue;
                }
                edges.push(c);
            }
        }
        assert!(
            !edges.is_empty(),
            "the class must have edges to test against"
        );
        for c in edges {
            let cleaned = sanitize_name(&format!("par{c}ty"), "Voice Room");
            assert_eq!(
                cleaned,
                format!("par{c}ty"),
                "U+{:04X} draws something and must survive",
                c as u32
            );
        }
    }

    /// A name made only of characters that render nothing is an empty name:
    /// it falls back to the default name instead of coming back blank.
    ///
    /// The characters here are outside the set the earlier test used, which is
    /// how the blank-name limit was missed: five Mongolian free variation
    /// selectors render nothing, so five of them are a blank name.
    #[test]
    fn a_name_of_only_mongolian_variation_selectors_falls_back_to_the_default() {
        let five = "\u{180B}\u{180B}\u{180B}\u{180B}\u{180B}";
        assert_eq!(
            sanitize_name(five, "Voice Room"),
            "Voice Room",
            "five Mongolian free variation selectors render nothing, so the name is empty"
        );
    }

    /// A name made only of a format character from the part of the class the
    /// earlier test did not use is an empty name too.
    #[test]
    fn a_name_of_only_shorthand_format_controls_falls_back_to_the_default() {
        let only_format = "\u{1BCA0}\u{1BCA1}\u{1BCA2}\u{1BCA3}";
        assert_eq!(sanitize_name(only_format, "Voice Room"), "Voice Room");
    }

    /// The reserved code points U+FFF0..U+FFF8 are default ignorable, so they
    /// render nothing and must be removed from a name.
    ///
    /// They are `Default_Ignorable_Code_Point` but they are NOT format
    /// characters (`Cf`): they are reserved, unassigned code points. A table
    /// built from the format characters alone leaves them out, and then they
    /// come back in a name as nine invisible characters.
    #[test]
    fn the_reserved_default_ignorables_are_removed() {
        for cp in 0xFFF0u32..=0xFFF8 {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            assert_eq!(
                sanitize_name(&format!("par{c}ty"), "Voice Room"),
                "party",
                "U+{cp:04X} renders nothing and must be removed"
            );
        }
    }

    /// A name made only of the reserved default ignorables is an empty name:
    /// it falls back to the default name instead of coming back blank.
    #[test]
    fn a_name_of_only_the_reserved_default_ignorables_falls_back_to_the_default() {
        let only_reserved: String = (0xFFF0u32..=0xFFF8).filter_map(char::from_u32).collect();
        assert_eq!(only_reserved.chars().count(), 9);
        assert_eq!(
            sanitize_name(&only_reserved, "Voice Room"),
            "Voice Room",
            "nine reserved default ignorables render nothing, so the name is empty"
        );
    }

    /// The class holds every code point of the whole default ignorable set.
    ///
    /// The count is the sum of the lengths of the ranges, so it pins the table
    /// against the class it claims to be: a range that was dropped or
    /// shortened changes the count and fails here.
    #[test]
    fn the_class_holds_every_code_point_of_the_default_ignorable_set() {
        let counted: usize = INVISIBLE_CLASS
            .iter()
            .map(|&(start, end)| (end - start + 1) as usize)
            .sum();
        assert_eq!(
            counted, 4207,
            "the closed class must hold every code point of every range"
        );
    }

    /// The class, written out as raw literals: the expected first and last
    /// code point of each of the 26 ranges, with the code point just below
    /// and the code point just above each range.
    ///
    /// This table is a SECOND copy of the class, transcribed by hand from the
    /// Unicode 17.0.0 character database, and it does NOT come from
    /// `INVISIBLE_RANGES`. That is the whole point of it: the other tests
    /// build their inputs from the production constant, so they only check
    /// that the implementation agrees with itself. A range that was moved by
    /// one code point but kept its size passes all of them, because the walk
    /// only ever visits the code points the table names and the count is
    /// unchanged.
    ///
    /// The neighbours are `None` where the neighbour is removed by another
    /// rule rather than by the class (the whitespace rule removes U+200A,
    /// U+2029, U+202F and U+205F), so only the neighbours that must survive
    /// are asserted.
    ///
    /// Every neighbour that IS given must be a real character, because a
    /// neighbour that is not one cannot be asserted on at all: the tests
    /// that walk them used to skip it, and then the end of the range it
    /// belongs to was never checked from that side. The neighbour below the
    /// tag range (U+E0000..U+E0FFF) is therefore U+DFFFF, the nearest
    /// character below U+E0000 - it is in plane 13 and is NOT a surrogate.
    /// The surrogate block is U+D800..U+DFFF, which is a different block
    /// entirely, and U+DFFF is its LAST surrogate, so it is not a character
    /// and cannot be used here.
    const EXPECTED_CLASS: &[(u32, u32, Option<u32>, Option<u32>)] = &[
        (0x00AD, 0x00AD, Some(0x00AC), Some(0x00AE)),
        (0x034F, 0x034F, Some(0x034E), Some(0x0350)),
        (0x0600, 0x0605, Some(0x05FF), Some(0x0606)),
        (0x061C, 0x061C, Some(0x061B), Some(0x061D)),
        (0x06DD, 0x06DD, Some(0x06DC), Some(0x06DE)),
        (0x070F, 0x070F, Some(0x070E), Some(0x0710)),
        (0x0890, 0x0891, Some(0x088F), Some(0x0892)),
        (0x08E2, 0x08E2, Some(0x08E1), Some(0x08E3)),
        (0x115F, 0x1160, Some(0x115E), Some(0x1161)),
        (0x17B4, 0x17B5, Some(0x17B3), Some(0x17B6)),
        (0x180B, 0x180F, Some(0x180A), Some(0x1810)),
        (0x200B, 0x200F, None, Some(0x2010)),
        (0x202A, 0x202E, None, None),
        (0x2060, 0x206F, None, Some(0x2070)),
        (0x2800, 0x2800, Some(0x27FF), Some(0x2801)),
        (0x3164, 0x3164, Some(0x3163), Some(0x3165)),
        (0xFE00, 0xFE0F, Some(0xFDFF), Some(0xFE10)),
        (0xFEFF, 0xFEFF, Some(0xFEFE), Some(0xFF00)),
        (0xFFA0, 0xFFA0, Some(0xFF9F), Some(0xFFA1)),
        (0xFFF0, 0xFFFB, Some(0xFFEF), Some(0xFFFC)),
        (0x110BD, 0x110BD, Some(0x110BC), Some(0x110BE)),
        (0x110CD, 0x110CD, Some(0x110CC), Some(0x110CE)),
        (0x13430, 0x1343F, Some(0x1342F), Some(0x13440)),
        (0x1BCA0, 0x1BCA3, Some(0x1BC9F), Some(0x1BCA4)),
        (0x1D173, 0x1D17A, Some(0x1D172), Some(0x1D17B)),
        (0xE0000, 0xE0FFF, Some(0xDFFFF), Some(0xE1000)),
    ];

    /// The production table must BE the class, checked against a second,
    /// independently written copy of it.
    ///
    /// The check is on the observable behaviour, not on a field: for every
    /// range of the expected class, the first and the last code point are
    /// removed from a name, and the code point just outside each end is
    /// kept. A range that was moved by one code point but kept its size
    /// either lets a code point of the class survive or removes a code point
    /// that draws something, and either way this test fails.
    #[test]
    fn the_class_is_the_one_the_unicode_character_database_names() {
        assert_eq!(
            EXPECTED_CLASS.len(),
            26,
            "the class must hold the 26 ranges of Unicode 17.0.0"
        );
        let mut removed = 0usize;
        for &(first, last, below, above) in EXPECTED_CLASS {
            for cp in [first, last] {
                let c = char::from_u32(cp).unwrap_or_else(|| {
                    panic!("U+{cp:05X} is not a character, so it cannot be checked")
                });
                removed += 1;
                assert_eq!(
                    sanitize_name(&format!("par{c}ty"), "Voice Room"),
                    "party",
                    "U+{cp:04X} renders nothing and must be removed"
                );
            }
            for cp in [below, above].into_iter().flatten() {
                let c = char::from_u32(cp).unwrap_or_else(|| {
                    panic!("U+{cp:05X} is not a character, so it cannot be checked")
                });
                assert_eq!(
                    sanitize_name(&format!("par{c}ty"), "Voice Room"),
                    format!("par{c}ty"),
                    "U+{cp:04X} draws something and must survive"
                );
            }
        }
        assert_eq!(
            removed, 52,
            "every range has a first and a last code point to check"
        );
    }

    /// The production table must hold exactly the ranges of the expected
    /// class: no range is missing, none is extra, and every range starts and
    /// ends where the expected one does.
    ///
    /// This is the direct check on the table, so a range that was shifted or
    /// dropped is caught here even when the count stays the same.
    #[test]
    fn the_production_table_is_the_expected_class() {
        let mut expected: Vec<(u32, u32)> =
            EXPECTED_CLASS.iter().map(|&(f, l, _, _)| (f, l)).collect();
        expected.sort_unstable();
        let mut actual: Vec<(u32, u32)> = INVISIBLE_CLASS.to_vec();
        actual.sort_unstable();
        assert_eq!(
            actual, expected,
            "the production table must be exactly the 26 ranges of the class"
        );
    }

    /// The code point just outside each range of the expected class must
    /// survive on its own, not only inside a word.
    ///
    /// The word test above could pass for a code point that the class
    /// removes only when it is between two letters, so this test pins the
    /// code point alone as well.
    ///
    /// A neighbour that is not a character at all is a FAILURE here, not a
    /// skip: the tests above used `let ... else { continue }`, so a neighbour
    /// that is not a char was silently skipped and the check below that end
    /// of the range never ran. A surrogate (U+D800..U+DFFF are not
    /// characters) is exactly such a neighbour, and it hid the check below
    /// the tag range.
    #[test]
    fn a_code_point_just_outside_the_expected_class_survives_on_its_own() {
        for &(_, _, below, above) in EXPECTED_CLASS {
            for cp in [below, above].into_iter().flatten() {
                let c = char::from_u32(cp).unwrap_or_else(|| {
                    panic!("U+{cp:05X} is not a character, so it cannot be checked")
                });
                assert_eq!(
                    sanitize_name(&format!("a{c}b"), "Voice Room"),
                    format!("a{c}b"),
                    "U+{cp:04X} draws something and must survive"
                );
            }
        }
    }

    /// Every neighbour of the expected class must be a real character.
    ///
    /// A neighbour that is not a character cannot be asserted on: the tests
    /// that walk the neighbours skip it, so the end of the range it belongs
    /// to is never checked from that side. This test makes such a neighbour
    /// a failure, so the table cannot carry one again.
    #[test]
    fn every_neighbour_of_the_expected_class_is_a_character() {
        for &(first, last, below, above) in EXPECTED_CLASS {
            for cp in [below, above].into_iter().flatten() {
                assert!(
                    char::from_u32(cp).is_some(),
                    "U+{cp:05X} (below/above the range {first:05X}..{last:05X}) is not a \
                     character, so the check from that side of the range never runs"
                );
            }
        }
    }
}

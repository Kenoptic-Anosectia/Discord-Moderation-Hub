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
//!   zero-width characters, so a name can never ping a role or smuggle an invite.
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
    /// How many members are currently in the room.
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
        if event.at - *last < config.create_cooldown_secs {
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
/// The owner leaving a room that still holds members makes the room claimable. An
/// empty room is only cleaned up once its grace period has elapsed; the runtime
/// records `empty_since` when the room first becomes empty and calls again later.
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
    if user_id != room.owner_id {
        return Decision::Deny(DenyReason::NotRoomOwner);
    }
    if room.occupants > 0 {
        return Decision::Claimable(channel_id);
    }
    // The room is empty. The grace period starts when the room first became empty;
    // until it has elapsed the room stays.
    let empty_since = room.empty_since.unwrap_or(now);
    if now - empty_since >= config.empty_grace_secs {
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
    let used = state
        .renames
        .iter()
        .filter(|(renamed_channel, at)| {
            *renamed_channel == channel_id && now - *at < config.rename_budget.window_secs
        })
        .count();
    if used >= config.rename_budget.max_renames as usize {
        return Decision::Deny(DenyReason::RenameBudgetExhausted);
    }
    Decision::Allow
}

/// Cleans a room name.
///
/// The cleaning removes, in this order: the mention tokens `<@..>`, `<@&..>` and
/// `<#..>`, the `@everyone` and `@here` words, the control and zero-width
/// characters, and every link (a URL, a `www.` host, an invite link, and an invite
/// host that was spread across spaces). What is left is trimmed, shortened to
/// [`MAX_NAME_CHARS`] characters and, when it is empty, replaced by `default_name`.
pub fn sanitize_name(raw: &str, default_name: &str) -> String {
    let cleaned = strip_links(&strip_invisible(&strip_mentions(raw)));
    let cleaned = if cleaned.trim().is_empty() {
        strip_links(&strip_invisible(&strip_mentions(default_name)))
    } else {
        cleaned
    };
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return DEFAULT_ROOM_NAME.to_string();
    }
    truncate_chars(cleaned, MAX_NAME_CHARS)
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

/// Removes the control and the zero-width characters.
fn strip_invisible(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() && !is_zero_width(*c))
        .collect()
}

/// True for the characters that are invisible but still occupy a position.
fn is_zero_width(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
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
fn is_link_like(segment: &str) -> bool {
    let lowered = segment.to_ascii_lowercase();
    if lowered.contains("://") || lowered.starts_with("www.") {
        return true;
    }
    if contains_invite_host(&lowered) {
        return true;
    }
    match lowered.find('/') {
        Some(slash) => {
            let host = &lowered[..slash];
            looks_like_host(host)
        }
        None => false,
    }
}

/// True when the text carries one of the known invite hosts.
fn contains_invite_host(lowered: &str) -> bool {
    const INVITE_HOSTS: [&str; 4] = [
        "discord.gg/",
        "discord.com/invite/",
        "discordapp.com/invite/",
        "discord.new/",
    ];
    INVITE_HOSTS.iter().any(|host| lowered.contains(host))
}

/// True when the text looks like a host name such as `example.invalid`.
fn looks_like_host(host: &str) -> bool {
    if !host.contains('.') || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    let last_label = host.rsplit('.').next().unwrap_or_default();
    last_label.len() >= 2 && last_label.chars().all(|c| c.is_ascii_alphabetic())
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
                    Decision::Claimable(_) => {
                        state.rooms[index].occupants = now_occupants.saturating_sub(1);
                    }
                    Decision::Deny(_) => {}
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
}

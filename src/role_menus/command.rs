//! `/rolemenu create | option | post | edit | list | unpublish` logic (RM2).
//!
//! The functions here are plain async functions over small traits (the menu
//! store, the embed poster, the role-facts source) so that tests inject fakes;
//! registering a poise command is the wiring card's job.
//!
//! Decisions are made from plain values: the stored menu rows are the truth and
//! every reply is a stable code. No serenity type, no clock read, no randomness
//! and no I/O outside the injected traits appear in the decision logic.
//!
//! Privacy notes:
//! * Every refusal and every failure is a stable code. No reply names a
//!   member, a role name, a channel name or free text.
//! * The module never writes a member role. Configuring a menu, adding an
//!   option and removing an option only touch the menu tables; the grant
//!   itself is the later card RM3.

use std::collections::BTreeSet;

use poise::serenity_prelude::async_trait;

use crate::roles::eligibility::{decide, Decision, DenyReason, GrantPath, RoleFacts};

use super::policy::{self, GroupMode, SELECT_OPTION_LIMIT};
use super::repository::{MenuRow, OptionRow, RepoError};

/// Longest menu key, in bytes.
pub const MAX_KEY_BYTES: usize = 64;

/// Longest option label, in characters (Discord's select-option label limit).
pub const MAX_LABEL_CHARS: usize = 100;

/// Longest option emoji, in characters.
pub const MAX_EMOJI_CHARS: usize = 64;

/// Discord's limit of action rows in one message.
pub const MAX_ROWS_PER_MESSAGE: usize = 5;

/// The most options one menu may hold: Discord's five action rows of twenty-five
/// options each. A menu that would need a sixth row is refused at configuration
/// time instead of being sent and rejected by Discord.
pub const MAX_OPTIONS_PER_MENU: usize = MAX_ROWS_PER_MESSAGE * SELECT_OPTION_LIMIT;

/// The most ZWJ joins one emoji may hold (the longest family sequence has three).
pub const MAX_ZWJ_JOINS: usize = 4;

/// The modes a menu may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuMode {
    /// One option of the group at a time.
    Exclusive,
    /// Several options at a time.
    Multi,
}

impl MenuMode {
    /// The stable text stored in the row.
    pub fn as_str(self) -> &'static str {
        match self {
            MenuMode::Exclusive => "exclusive",
            MenuMode::Multi => "multi",
        }
    }

    /// Parses a stored mode. An unknown spelling is refused.
    pub fn parse(text: &str) -> Option<MenuMode> {
        match text {
            "exclusive" => Some(MenuMode::Exclusive),
            "multi" => Some(MenuMode::Multi),
            _ => None,
        }
    }
}

/// Stable result codes of the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// The feature is off.
    Disabled,
    /// The caller may not do this.
    Denied,
    /// The command was used in a guild that is not the home guild.
    OtherGuild,
    /// No home guild is configured: nothing is served.
    NoHomeGuild,
    /// The bot has not got "Manage Roles" in the guild.
    BotMissingManageRoles,
    /// The key is not a usable menu key.
    InvalidKey,
    /// A target id (role, channel) is zero or otherwise unusable.
    InvalidTarget,
    /// The key is already used by another menu.
    KeyTaken,
    /// No menu with that key exists.
    MenuNotFound,
    /// The menu already has an option for that role.
    OptionExists,
    /// The menu has no option for that role.
    OptionNotFound,
    /// The role may not be granted through a menu.
    Ineligible,
    /// The label is empty, too long or holds a character that may not be shown.
    InvalidLabel,
    /// The emoji is empty, too long or is not an emoji.
    InvalidEmoji,
    /// The menu has no options yet.
    EmptyMenu,
    /// The menu already holds as many options as one message may carry.
    TooManyOptions,
    /// The mode and the max-select do not fit together.
    InvalidMode,
    /// The menu was created.
    Created,
    /// The option was added.
    OptionAdded,
    /// The option was removed.
    OptionRemoved,
    /// The menu embed was posted.
    Posted,
    /// The menu embed was edited in place.
    Edited,
    /// The recorded message was gone: a new one was posted in its place.
    Reposted,
    /// The menu is posted in another channel than the one that was given.
    ChannelMismatch,
    /// The menu was listed.
    Listed,
    /// The store could not be read or written.
    StoreFailed,
    /// Discord refused or failed the call.
    DiscordFailed,
    /// Discord refused the call (the bot may not do this).
    DiscordForbidden,
    /// The recorded message does not exist any more.
    MessageGone,
    /// The channel the menu was posted in does not exist any more.
    ChannelGone,
    /// The recorded location of the menu was cleared.
    Unpublished,
    /// The mode and/or the maximum of the menu were changed.
    MenuUpdated,
    /// The role facts could not be looked up.
    FactsUnavailable,
    /// The stored row could not be understood.
    CorruptRow,
}

impl Code {
    /// The stable text of the code.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Disabled => "disabled",
            Code::Denied => "denied",
            Code::OtherGuild => "other_guild",
            Code::NoHomeGuild => "no_home_guild",
            Code::BotMissingManageRoles => "bot_missing_manage_roles",
            Code::InvalidKey => "invalid_key",
            Code::InvalidTarget => "invalid_target",
            Code::KeyTaken => "key_taken",
            Code::MenuNotFound => "menu_not_found",
            Code::OptionExists => "option_exists",
            Code::OptionNotFound => "option_not_found",
            Code::Ineligible => "ineligible",
            Code::InvalidLabel => "invalid_label",
            Code::InvalidEmoji => "invalid_emoji",
            Code::EmptyMenu => "empty_menu",
            Code::TooManyOptions => "too_many_options",
            Code::InvalidMode => "invalid_mode",
            Code::Created => "created",
            Code::OptionAdded => "option_added",
            Code::OptionRemoved => "option_removed",
            Code::Posted => "posted",
            Code::Edited => "edited",
            Code::Reposted => "reposted",
            Code::ChannelMismatch => "channel_mismatch",
            Code::Listed => "listed",
            Code::StoreFailed => "store_failed",
            Code::DiscordFailed => "discord_failed",
            Code::DiscordForbidden => "discord_forbidden",
            Code::MessageGone => "message_gone",
            Code::ChannelGone => "channel_gone",
            Code::Unpublished => "unpublished",
            Code::MenuUpdated => "menu_updated",
            Code::FactsUnavailable => "facts_unavailable",
            Code::CorruptRow => "corrupt_row",
        }
    }
}

/// An outcome: one code and, when there is one, a stable second code that says
/// why (a K03 denial code, a store error code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// The result.
    pub code: Code,
    /// A stable sub-code.
    pub detail: Option<&'static str>,
}

impl Outcome {
    /// An outcome without a detail.
    pub fn new(code: Code) -> Self {
        Outcome { code, detail: None }
    }

    /// An outcome with a detail code.
    pub fn with(code: Code, detail: &'static str) -> Self {
        Outcome {
            code,
            detail: Some(detail),
        }
    }

    /// The full stable text, for example `ineligible (protected_role)`.
    pub fn text(self) -> String {
        match self.detail {
            Some(detail) => format!("{} ({detail})", self.code.as_str()),
            None => self.code.as_str().to_string(),
        }
    }
}

/// An ephemeral reply. The type has no way to build a public one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    text: String,
}

impl Reply {
    /// The text of the reply.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replies are ephemeral.
    pub fn ephemeral(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Who is calling, as the wiring card works it out from the interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invoker {
    /// The calling user.
    pub user: u64,
    /// The owner of the guild the command was used in.
    pub guild_owner: u64,
    /// True when the caller holds the configured staff role.
    pub is_staff: bool,
}

/// The configuration the command needs. Every field is decided by the wiring
/// card; this module reads them and nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// True when the feature is on (`ROLE_MENUS_MODE`). While it is off every
    /// command says `disabled` and nothing is read or written.
    pub enabled: bool,
    /// The one guild these commands serve (`GUILD_ID`).
    pub home_guild: Option<u64>,
    /// The staff role a caller must hold (`STAFF_ROLE_ID`).
    pub staff_role_id: Option<u64>,
    /// True when the bot holds "Manage Roles" in the home guild.
    pub bot_has_manage_roles: bool,
    /// Roles that may never be granted through a menu (`PROTECTED_ROLE_IDS`).
    pub protected_role_ids: Vec<u64>,
    /// True when `PROTECTED_ROLE_IDS` could not be read: fail closed.
    pub protected_roles_invalid: bool,
}

/// `/rolemenu create <key>`.
#[derive(Debug, Clone, Copy)]
pub struct CreateRequest<'a> {
    /// Who is calling.
    pub invoker: Invoker,
    /// The guild the command was used in.
    pub guild: u64,
    /// The menu key.
    pub key: &'a str,
    /// The mode of the menu.
    pub mode: MenuMode,
    /// The maximum number of options a member may hold (0 = unbounded).
    pub max_select: u32,
}

/// `/rolemenu option add <key> <role> <label> <emoji>`.
#[derive(Debug, Clone, Copy)]
pub struct AddOptionRequest<'a> {
    /// Who is calling.
    pub invoker: Invoker,
    /// The guild the command was used in.
    pub guild: u64,
    /// The menu key.
    pub key: &'a str,
    /// The role the option grants.
    pub role: u64,
    /// The label shown on the option.
    pub label: &'a str,
    /// The emoji shown on the option.
    pub emoji: &'a str,
}

/// `/rolemenu option remove <key> <role>`.
#[derive(Debug, Clone, Copy)]
pub struct RemoveOptionRequest<'a> {
    /// Who is calling.
    pub invoker: Invoker,
    /// The guild the command was used in.
    pub guild: u64,
    /// The menu key.
    pub key: &'a str,
    /// The role the option grants.
    pub role: u64,
}

/// `/rolemenu post <key>` and `/rolemenu edit <key>`.
#[derive(Debug, Clone, Copy)]
pub struct PostRequest<'a> {
    /// Who is calling.
    pub invoker: Invoker,
    /// The guild the command was used in.
    pub guild: u64,
    /// The menu key.
    pub key: &'a str,
    /// The channel to post into.
    pub channel: u64,
}

/// `/rolemenu edit <key> [mode] [max_select]`.
#[derive(Debug, Clone, Copy)]
pub struct EditRequest<'a> {
    /// Who is calling.
    pub invoker: Invoker,
    /// The guild the command was used in.
    pub guild: u64,
    /// The menu key.
    pub key: &'a str,
    /// The new mode, or `None` to keep the stored one.
    pub mode: Option<MenuMode>,
    /// The new maximum (0 = unbounded), or `None` to keep the stored one.
    pub max_select: Option<u32>,
}

/// `/rolemenu unpublish <key>`.
#[derive(Debug, Clone, Copy)]
pub struct UnpublishRequest<'a> {
    /// Who is calling.
    pub invoker: Invoker,
    /// The guild the command was used in.
    pub guild: u64,
    /// The menu key.
    pub key: &'a str,
}

/// `/rolemenu list`.
#[derive(Debug, Clone, Copy)]
pub struct ListRequest {
    /// Who is calling.
    pub invoker: Invoker,
    /// The guild the command was used in.
    pub guild: u64,
}

/// One menu as `/rolemenu list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedMenu {
    /// The menu key.
    pub key: String,
    /// The mode of the menu.
    pub mode: String,
    /// The number of options the menu holds.
    pub options: u32,
    /// True when the menu has been posted.
    pub posted: bool,
}

/// The result of `/rolemenu list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListReport {
    /// The outcome of the call.
    pub outcome: Outcome,
    /// The menus of the calling guild, ordered by key.
    pub menus: Vec<ListedMenu>,
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

/// The menu tables.
#[async_trait]
pub trait MenuStore: Send + Sync {
    /// Creates a menu and returns its id.
    async fn create_menu(
        &self,
        guild: u64,
        key: &str,
        mode: &str,
        max_select: u32,
    ) -> Result<i64, RepoError>;
    /// Reads one menu by guild and key.
    async fn get_menu(&self, guild: u64, key: &str) -> Result<Option<MenuRow>, RepoError>;
    /// Lists the menus of one guild, ordered by key.
    async fn list_menus(&self, guild: u64) -> Result<Vec<MenuRow>, RepoError>;
    /// Stores one option of a menu and returns its position.
    async fn add_option(
        &self,
        menu_id: i64,
        role: u64,
        label: &str,
        emoji: &str,
    ) -> Result<i64, RepoError>;
    /// Lists the options of one menu, ordered by position.
    async fn list_options(&self, menu_id: i64) -> Result<Vec<OptionRow>, RepoError>;
    /// Deletes one option of a menu.
    async fn remove_option(&self, menu_id: i64, role: u64) -> Result<bool, RepoError>;
    /// The number of options a menu holds.
    async fn count_options(&self, menu_id: i64) -> Result<u32, RepoError>;
    /// Records where a menu was published.
    async fn set_published(
        &self,
        menu_id: i64,
        channel: u64,
        message: u64,
        embed_key: Option<&str>,
    ) -> Result<(), RepoError>;
    /// Changes the mode and the maximum of a menu.
    async fn update_menu(&self, menu_id: i64, mode: &str, max_select: u32)
        -> Result<(), RepoError>;
    /// Clears the recorded location of a menu.
    ///
    /// Used by the reset path: a menu whose channel was deleted can only be
    /// published again after its recorded channel and message were cleared.
    async fn clear_published(&self, menu_id: i64) -> Result<(), RepoError>;
}

/// One select component of a menu, ready to be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedOption {
    /// The role the option grants.
    pub role: u64,
    /// The label shown on the option.
    pub label: String,
    /// The emoji shown on the option.
    pub emoji: String,
}

/// One message of a menu: the options that fit on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedChunk {
    /// The zero-based number of this chunk.
    pub part: usize,
    /// The options of this chunk, in order.
    pub options: Vec<RenderedOption>,
}

/// Why a Discord call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordError {
    /// The bot may not do this.
    Forbidden,
    /// The message the call named does not exist any more.
    ///
    /// The message is gone but the channel is still there: the caller may
    /// send a new message in its place.
    NotFound,
    /// The channel the call named does not exist any more.
    ///
    /// A deleted channel is not a deleted message: a new message cannot be
    /// sent into it either, so the recorded location of a menu in that
    /// channel is dead and must be cleared before the menu can be published
    /// again.
    ChannelGone,
    /// Anything else (transport error, missing channel, ...).
    Failed,
}

/// The calls the command needs to publish a menu.
#[async_trait]
pub trait MenuDiscord: Send + Sync {
    /// Sends a new menu message and returns its id.
    async fn send_menu(&self, channel: u64, chunks: &[RenderedChunk]) -> Result<u64, DiscordError>;
    /// Edits an existing menu message in place.
    ///
    /// The channel is passed as well as the message: Discord needs both to
    /// find the message. A message that no longer exists is reported as
    /// [`DiscordError::NotFound`], never as a plain failure, so the caller can
    /// post a new one in its place.
    async fn edit_menu(
        &self,
        channel: u64,
        message: u64,
        chunks: &[RenderedChunk],
    ) -> Result<(), DiscordError>;
}

/// Where the facts of a role come from.
#[async_trait]
pub trait RoleFactsSource: Send + Sync {
    /// The facts of one role. `None` means the role is not in the guild.
    async fn role_facts(&self, guild: u64, role: u64) -> Option<RoleFacts>;
    /// The position of the bot's top role in the guild.
    async fn bot_top_position(&self, guild: u64) -> Option<u16>;
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Checks that a menu key is usable.
///
/// One canonical spelling: a lowercase ASCII name that starts with a letter or
/// a digit and continues with letters, digits, `-` or `_`, at most
/// [`MAX_KEY_BYTES`] bytes. Every other spelling is refused, so a key can
/// never name a path and never names two different menus.
pub fn validate_key(key: &str) -> Result<(), Code> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(Code::InvalidKey);
    }
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return Err(Code::InvalidKey);
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(Code::InvalidKey);
    }
    if !bytes.all(|byte| {
        byte.is_ascii_digit() || byte.is_ascii_lowercase() || matches!(byte, b'-' | b'_')
    }) {
        return Err(Code::InvalidKey);
    }
    Ok(())
}

/// Checks that a label is usable.
///
/// A label is plain text that Discord shows inside a select option. It must
/// not be empty, not be longer than [`MAX_LABEL_CHARS`] characters and must
/// not hold a character that could hide or fake the text:
///
/// * no control character (the newline included),
/// * no format character (`Cf`: zero-width and bidirectional marks),
/// * no blank code point (see [`is_blank_char`]: the Hangul fillers, the
///   braille blank and the rest of the default-ignorable set),
/// * no line separator (`U+2028`) and no paragraph separator (`U+2029`),
/// * no Discord mention markup (`<@…>`, `<#…>`, `<:…>`, `<a:…>`).
///
/// Every one of those classes is refused wherever it appears in the label,
/// so a label that shows the reader nothing (only fillers, only spaces) is
/// refused too: an empty label is not a label.
pub fn validate_label(label: &str) -> Result<(), Code> {
    let trimmed = label.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_LABEL_CHARS {
        return Err(Code::InvalidLabel);
    }
    if trimmed.chars().any(is_forbidden_label_char) {
        return Err(Code::InvalidLabel);
    }
    if trimmed.chars().any(is_blank_char) {
        return Err(Code::InvalidLabel);
    }
    if holds_markup(trimmed) {
        return Err(Code::InvalidLabel);
    }
    Ok(())
}

/// True for a character a label may not hold.
///
/// The check is on the code point, not on the spelling: every control
/// character, every format character (`Cf`) and the two Unicode separators
/// are refused.
fn is_forbidden_label_char(c: char) -> bool {
    if c.is_control() {
        return true;
    }
    if is_format_char(c) {
        return true;
    }
    matches!(c, '\u{2028}' | '\u{2029}')
}

/// True for a character an emoji may not hold.
///
/// An emoji is not free text: it may not hide behind a control character, a
/// bidirectional mark or a zero-width mark. Three code points are NOT
/// refused: the zero-width joiner and the two variation selectors, because
/// they are what joins the parts of one emoji (a family, a skin tone, a
/// heart).
fn is_forbidden_emoji_char(c: char) -> bool {
    if c.is_control() {
        return true;
    }
    if is_emoji_mark(c) {
        return false;
    }
    // A bidirectional or invisible mark other than the joiner.
    if is_format_char(c) {
        return true;
    }
    // A blank code point.
    if is_blank_char(c) {
        return true;
    }
    matches!(c, '\u{2028}' | '\u{2029}')
}

/// True for the marks that join the parts of one emoji.
fn is_emoji_mark(c: char) -> bool {
    matches!(c, '\u{200D}' | '\u{FE0E}' | '\u{FE0F}')
}

/// True for a Unicode format character (`Cf`).
///
/// These are the zero-width and bidirectional marks: they change how the text
/// is shown or read without changing what it says.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// True for a code point that occupies space but shows the reader nothing.
///
/// These are the Unicode DEFAULT IGNORABLE code points that are not already
/// format characters, plus the blank fillers: the Hangul fillers, the
/// braille blank and the Khmer inherent vowels. A label that holds only
/// these would look empty, so it is refused.
///
/// The set is a class, not a list of examples: every code point in these
/// ranges is refused, not only the ones named here.
fn is_blank_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' // SOFT HYPHEN
            | '\u{034F}' // COMBINING GRAPHEME JOINER
            | '\u{061C}' // ARABIC LETTER MARK
            | '\u{115F}'..='\u{1160}' // HANGUL CHOSEONG/JUNGSEONG FILLER
            | '\u{17B4}'..='\u{17B5}' // KHMER VOWEL INHERENT AQ/AA
            | '\u{180B}'..='\u{180F}' // MONGOLIAN FREE VARIATION SELECTORS
            | '\u{200B}'..='\u{200F}' // ZERO WIDTH SPACE AND MARKS
            | '\u{202A}'..='\u{202E}' // BIDIRECTIONAL FORMATTING
            | '\u{2060}'..='\u{206F}' // WORD JOINER AND DIRECTIONAL FORMATTING
            | '\u{2800}' // BRAILLE PATTERN BLANK
            | '\u{3164}' // HANGUL FILLER
            | '\u{FEFF}' // ZERO WIDTH NO-BREAK SPACE
            | '\u{FFA0}' // HALFWIDTH HANGUL FILLER
            | '\u{FFF9}'..='\u{FFFB}' // INTERLINEAR ANNOTATION
            | '\u{E0000}'..='\u{E0FFF}' // TAGS AND VARIATION SELECTORS
            | '\u{FE00}'..='\u{FE0F}' // VARIATION SELECTORS
    )
}

/// True when the text holds Discord markup that would be rendered as a
/// mention or an emoji instead of as plain text.
fn holds_markup(text: &str) -> bool {
    for marker in ["<@", "<#", "<:", "<a:"] {
        if text.contains(marker) {
            return true;
        }
    }
    false
}

/// Checks that an emoji is usable.
///
/// Two spellings are accepted and nothing else:
///
/// * a custom emoji in the Discord form `<name:id>` or `<a:name:id>`, where
///   the name is two to thirty-two characters of letters, digits and
///   underscores and the id is a number that is not zero,
/// * a plain Unicode emoji: EXACTLY ONE emoji, with no whitespace, no
///   markup and no character that could hide it.
///
/// "Exactly one" is the whole rule: Discord shows one emoji next to a select
/// option, so a string that holds two emoji (`"🎉🎉"`) or a skin tone
/// without its base (`"🏻"`) is refused. One emoji is a base code point,
/// optionally followed by a variation selector and a skin tone, and at most
/// [`MAX_ZWJ_JOINS`] more parts joined with the zero-width joiner; a keycap
/// is a digit with the keycap mark; a flag is a pair of regional indicators.
/// See [`is_single_emoji`] for the grammar and its limit.
///
/// Everything else is refused, so free text, a mention, a link and a
/// zero-width mark can never be stored as the emoji of an option.
pub fn validate_emoji(emoji: &str) -> Result<(), Code> {
    let trimmed = emoji.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_EMOJI_CHARS {
        return Err(Code::InvalidEmoji);
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(Code::InvalidEmoji);
    }
    if trimmed.chars().any(is_forbidden_emoji_char) {
        return Err(Code::InvalidEmoji);
    }
    if trimmed.starts_with('<') {
        return validate_custom_emoji(trimmed);
    }
    if is_single_emoji(trimmed) {
        return Ok(());
    }
    Err(Code::InvalidEmoji)
}

/// Checks the Discord custom-emoji form `<name:id>` or `<a:name:id>`.
fn validate_custom_emoji(text: &str) -> Result<(), Code> {
    let Some(rest) = text.strip_prefix('<') else {
        return Err(Code::InvalidEmoji);
    };
    let Some(rest) = rest.strip_prefix("a:").or_else(|| rest.strip_prefix(':')) else {
        return Err(Code::InvalidEmoji);
    };
    let Some((name, id)) = rest.split_once(':') else {
        return Err(Code::InvalidEmoji);
    };
    let Some(id) = id.strip_suffix('>') else {
        return Err(Code::InvalidEmoji);
    };
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(Code::InvalidEmoji);
    }
    if id.parse::<u64>().unwrap_or(0) == 0 {
        return Err(Code::InvalidEmoji);
    }
    let name_length = name.chars().count();
    if !(2..=32).contains(&name_length) {
        return Err(Code::InvalidEmoji);
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Code::InvalidEmoji);
    }
    Ok(())
}

/// True when the text is exactly one Unicode emoji.
///
/// The grammar, walked left to right, and nothing outside it is accepted:
///
/// * a flag: exactly two regional indicators,
/// * a keycap: an ASCII digit, an optional variation selector, then the
///   keycap mark `U+20E3` as the LAST character (the mark is not accepted
///   after anything that is not a digit),
/// * otherwise one or more PARTS joined by the zero-width joiner, at most
///   [`MAX_ZWJ_JOINS`] joins. A part is a base code point, an optional
///   variation selector, an optional skin tone (which may be followed by a
///   variation selector if none came before it). A joiner must be followed
///   by the next part, so it cannot be last, doubled or followed by a
///   non-base; a skin tone is followed by a joiner or the end, never by
///   another base.
///
/// Limit: "base" is a set of coarse code-point ranges ([`is_emoji_base`]),
/// not an emoji table, so a code point inside a range that is not really an
/// emoji, a skin tone on a base that cannot take one, and a pair of regional
/// indicators that is not a real flag are all accepted.
fn is_single_emoji(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let Some(&first) = chars.first() else {
        return false;
    };
    if is_regional_indicator(first) {
        return chars.len() == 2 && is_regional_indicator(chars[1]);
    }
    if first.is_ascii_digit() {
        return matches!(&chars[1..], ['\u{20E3}'] | ['\u{FE0F}', '\u{20E3}']);
    }
    let mut index = 0usize;
    let mut joins = 0usize;
    loop {
        let Some(end) = part_end(&chars, index) else {
            return false;
        };
        index = end;
        if index == chars.len() {
            return true;
        }
        if chars[index] != '\u{200D}' {
            return false;
        }
        joins += 1;
        if joins > MAX_ZWJ_JOINS {
            return false;
        }
        index += 1;
    }
}

/// The index just after the emoji part that starts at `start`, or `None`
/// when no part starts there.
fn part_end(chars: &[char], start: usize) -> Option<usize> {
    if start >= chars.len() || !is_emoji_base(chars[start]) {
        return None;
    }
    let mut index = start + 1;
    let is_selector = |c: char| matches!(c, '\u{FE0E}' | '\u{FE0F}');
    let mut selector_seen = false;
    if index < chars.len() && is_selector(chars[index]) {
        selector_seen = true;
        index += 1;
    }
    if index < chars.len() && is_skin_tone(chars[index]) {
        index += 1;
        if !selector_seen && index < chars.len() && is_selector(chars[index]) {
            index += 1;
        }
    }
    Some(index)
}

/// True for a code point that is an emoji on its own.
///
/// A skin-tone modifier and a regional indicator are NOT bases: they are
/// parts that must follow a base (or another regional indicator).
fn is_emoji_base(c: char) -> bool {
    if is_skin_tone(c) || is_regional_indicator(c) {
        return false;
    }
    matches!(
        c,
        '\u{00A9}' | '\u{00AE}'
            | '\u{203C}'
            | '\u{2049}'
            | '\u{2122}'
            | '\u{2139}'
            | '\u{2190}'..='\u{21FF}'
            | '\u{2300}'..='\u{23FF}'
            | '\u{24C2}'
            | '\u{25AA}'..='\u{25FE}'
            | '\u{2600}'..='\u{27BF}'
            | '\u{2934}'..='\u{2935}'
            | '\u{2B00}'..='\u{2BFF}'
            | '\u{3030}'
            | '\u{303D}'
            | '\u{3297}'
            | '\u{3299}'
            | '\u{1F000}'..='\u{1FAFF}'
    )
}

/// True for a skin-tone modifier (`U+1F3FB` to `U+1F3FF`).
fn is_skin_tone(c: char) -> bool {
    matches!(c, '\u{1F3FB}'..='\u{1F3FF}')
}

/// True for a regional indicator, the half of a flag.
fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

/// Splits the options of a menu into the select components of one message.
///
/// A component holds at most [`SELECT_OPTION_LIMIT`] options, in the stored
/// order, and no option is dropped: a menu with more options than fit on one
/// component produces more components. A menu never needs more than
/// [`MAX_ROWS_PER_MESSAGE`] components, because a menu may not hold more than
/// [`MAX_OPTIONS_PER_MENU`] options at all.
///
/// The split is deterministic.
pub fn split_chunks(options: &[OptionRow]) -> Vec<RenderedChunk> {
    options
        .chunks(SELECT_OPTION_LIMIT)
        .enumerate()
        .map(|(part, chunk)| RenderedChunk {
            part,
            options: chunk
                .iter()
                .map(|o| RenderedOption {
                    role: o.role,
                    label: o.label.clone(),
                    emoji: o.emoji.clone(),
                })
                .collect(),
        })
        .collect()
}

/// The policy menu of one stored menu, for the shared RM1 policy.
///
/// A stored mode that is not one of the two known spellings is refused with
/// [`Code::CorruptRow`]: a mode the reader cannot understand must never fall
/// back to a mode that is more permissive.
pub fn to_policy_menu(menu: &MenuRow, options: &[OptionRow]) -> Result<policy::Menu, Code> {
    let mode = match MenuMode::parse(&menu.mode) {
        Some(MenuMode::Exclusive) => GroupMode::Exclusive,
        Some(MenuMode::Multi) => GroupMode::Multi,
        None => return Err(Code::CorruptRow),
    };
    Ok(policy::Menu {
        key: menu.key.clone(),
        groups: vec![policy::Group {
            id: menu.key.clone(),
            mode,
            max: if menu.max_select == 0 {
                None
            } else {
                Some(menu.max_select)
            },
            options: options
                .iter()
                .map(|o| policy::MenuOption {
                    role_id: o.role,
                    label_key: o.label.clone(),
                })
                .collect(),
        }],
    })
}

/// The roles a menu may grant, as a set.
pub fn menu_roles(options: &[OptionRow]) -> BTreeSet<u64> {
    options.iter().map(|o| o.role).collect()
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// Checks the caller and the guild.
///
/// The order matters: an off feature says `disabled`; a caller who is not
/// staff is `denied` whatever the guild; a guild that is not the home guild is
/// `other_guild`; and a bot without "Manage Roles" may not configure anything.
pub fn gate(settings: &Settings, invoker: &Invoker, guild: u64) -> Result<(), Outcome> {
    if !settings.enabled {
        return Err(Outcome::new(Code::Disabled));
    }
    if !settings.bot_has_manage_roles {
        return Err(Outcome::new(Code::BotMissingManageRoles));
    }
    if invoker.user == 0 {
        return Err(Outcome::new(Code::Denied));
    }
    let is_owner = invoker.user == invoker.guild_owner;
    let is_staff = match settings.staff_role_id {
        Some(role) => invoker.is_staff && role != 0,
        None => false,
    };
    if !(is_owner || is_staff) {
        return Err(Outcome::new(Code::Denied));
    }
    if guild == 0 {
        return Err(Outcome::new(Code::InvalidTarget));
    }
    match settings.home_guild {
        None | Some(0) => Err(Outcome::new(Code::NoHomeGuild)),
        Some(home) if home == guild => Ok(()),
        Some(_) => Err(Outcome::new(Code::OtherGuild)),
    }
}

/// Checks that the role may be granted through a menu.
///
/// The same K03 rules as at grant time are applied at configuration time: a
/// privileged, managed or protected role is refused with its own stable code.
pub fn check_role(
    settings: &Settings,
    facts: &RoleFacts,
    bot_top_position: u16,
    guild: u64,
) -> Result<(), Outcome> {
    let request = crate::roles::eligibility::GrantRequest {
        path: GrantPath::Menu,
        role: *facts,
        bot_top_position,
        guild_id: guild,
        protected_role_ids: &settings.protected_role_ids,
        safeword: crate::roles::eligibility::SafewordState::Inactive,
        safeword_restricted_role_ids: &[],
        staff: None,
    };
    match decide(&request) {
        Decision::Allow => Ok(()),
        Decision::Deny(reason) => Err(Outcome::with(Code::Ineligible, reason.code())),
    }
}

/// The code a K03 denial is reported with.
pub fn deny_code(reason: DenyReason) -> &'static str {
    reason.code()
}

// ---------------------------------------------------------------------------
// Reply builders
// ---------------------------------------------------------------------------

/// Builds the reply of a single-outcome command.
pub fn reply_for(outcome: Outcome) -> Reply {
    Reply {
        text: format!("rolemenu {}", outcome.text()),
    }
}

/// Builds the reply of `/rolemenu list`.
pub fn list_reply(report: &ListReport) -> Reply {
    if report.outcome.code != Code::Listed {
        return reply_for(report.outcome);
    }
    let mut text = format!("rolemenu list: {} menus", report.menus.len());
    for menu in &report.menus {
        text.push_str(&format!(
            "\n{} mode={} options={} posted={}",
            menu.key, menu.mode, menu.options, menu.posted
        ));
    }
    Reply { text }
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

/// True when a mode and a maximum fit together.
///
/// An exclusive menu may not name a maximum, and no maximum may exceed what
/// one message can hold ([`MAX_OPTIONS_PER_MENU`]).
fn mode_fits(mode: MenuMode, max_select: u32) -> bool {
    if mode == MenuMode::Exclusive && max_select > 0 {
        return false;
    }
    max_select as usize <= MAX_OPTIONS_PER_MENU
}

/// `/rolemenu create <key>`.
///
/// Creates the menu row. The key is validated first, and a key that is
/// already taken is refused. An exclusive menu may not name a maximum number
/// of options: the two rules do not fit together, and a menu that holds one
/// rule must not be read as holding the other.
pub async fn create<S: MenuStore + ?Sized>(
    store: &S,
    settings: &Settings,
    req: &CreateRequest<'_>,
) -> Outcome {
    if let Err(outcome) = gate(settings, &req.invoker, req.guild) {
        return outcome;
    }
    if validate_key(req.key).is_err() {
        return Outcome::new(Code::InvalidKey);
    }
    if !mode_fits(req.mode, req.max_select) {
        return Outcome::new(Code::InvalidMode);
    }
    match store
        .create_menu(req.guild, req.key, req.mode.as_str(), req.max_select)
        .await
    {
        Ok(_) => Outcome::new(Code::Created),
        Err(RepoError::KeyTaken) => Outcome::new(Code::KeyTaken),
        Err(RepoError::InvalidMode) => Outcome::new(Code::InvalidMode),
        Err(error) => Outcome::with(Code::StoreFailed, error.code()),
    }
}

/// `/rolemenu option add <key> <role> <label> <emoji>`.
///
/// The role is checked against the K03 rules before it is stored, so a
/// privileged, managed or protected role never reaches the table.
pub async fn add_option<S: MenuStore + ?Sized, F: RoleFactsSource + ?Sized>(
    store: &S,
    facts: &F,
    settings: &Settings,
    req: &AddOptionRequest<'_>,
) -> Outcome {
    if let Err(outcome) = gate(settings, &req.invoker, req.guild) {
        return outcome;
    }
    if validate_key(req.key).is_err() {
        return Outcome::new(Code::InvalidKey);
    }
    if req.role == 0 {
        return Outcome::new(Code::InvalidTarget);
    }
    if validate_label(req.label).is_err() {
        return Outcome::new(Code::InvalidLabel);
    }
    if validate_emoji(req.emoji).is_err() {
        return Outcome::new(Code::InvalidEmoji);
    }
    if settings.protected_roles_invalid {
        return Outcome::new(Code::Ineligible);
    }
    let menu = match store.get_menu(req.guild, req.key).await {
        Ok(Some(menu)) => menu,
        Ok(None) => return Outcome::new(Code::MenuNotFound),
        Err(error) => return Outcome::with(Code::StoreFailed, error.code()),
    };
    // A menu may not hold more options than one message may carry: Discord
    // allows five action rows of twenty-five options. The option is refused
    // here, never dropped later.
    match store.count_options(menu.id).await {
        Ok(count) if count as usize >= MAX_OPTIONS_PER_MENU => {
            return Outcome::new(Code::TooManyOptions);
        }
        Ok(_) => {}
        Err(error) => return Outcome::with(Code::StoreFailed, error.code()),
    }
    let Some(role_facts) = facts.role_facts(req.guild, req.role).await else {
        return Outcome::new(Code::FactsUnavailable);
    };
    let Some(bot_top) = facts.bot_top_position(req.guild).await else {
        return Outcome::new(Code::FactsUnavailable);
    };
    if let Err(outcome) = check_role(settings, &role_facts, bot_top, req.guild) {
        return outcome;
    }
    match store
        .add_option(menu.id, req.role, req.label.trim(), req.emoji.trim())
        .await
    {
        Ok(_) => Outcome::new(Code::OptionAdded),
        Err(RepoError::OptionExists) => Outcome::new(Code::OptionExists),
        Err(error) => Outcome::with(Code::StoreFailed, error.code()),
    }
}

/// `/rolemenu option remove <key> <role>`.
///
/// Removes the stored option. No member role is touched.
pub async fn remove_option<S: MenuStore + ?Sized>(
    store: &S,
    settings: &Settings,
    req: &RemoveOptionRequest<'_>,
) -> Outcome {
    if let Err(outcome) = gate(settings, &req.invoker, req.guild) {
        return outcome;
    }
    if validate_key(req.key).is_err() {
        return Outcome::new(Code::InvalidKey);
    }
    if req.role == 0 {
        return Outcome::new(Code::InvalidTarget);
    }
    let menu = match store.get_menu(req.guild, req.key).await {
        Ok(Some(menu)) => menu,
        Ok(None) => return Outcome::new(Code::MenuNotFound),
        Err(error) => return Outcome::with(Code::StoreFailed, error.code()),
    };
    match store.remove_option(menu.id, req.role).await {
        Ok(true) => Outcome::new(Code::OptionRemoved),
        Ok(false) => Outcome::new(Code::OptionNotFound),
        Err(error) => Outcome::with(Code::StoreFailed, error.code()),
    }
}

/// `/rolemenu edit <key> [mode] [max_select]`.
///
/// Changes the mode and/or the maximum of a menu. What is not given keeps its
/// stored value, and the result must pass the same rule as `create`
/// ([`mode_fits`]): switching a menu with a maximum to exclusive therefore
/// needs an explicit maximum of zero. A refused or failed edit leaves the row
/// as it was. The key, the options and the published location are not
/// touched, and no member role is written.
pub async fn edit<S: MenuStore + ?Sized>(
    store: &S,
    settings: &Settings,
    req: &EditRequest<'_>,
) -> Outcome {
    if let Err(outcome) = gate(settings, &req.invoker, req.guild) {
        return outcome;
    }
    if validate_key(req.key).is_err() {
        return Outcome::new(Code::InvalidKey);
    }
    if req.mode.is_none() && req.max_select.is_none() {
        return Outcome::with(Code::InvalidMode, "nothing_to_change");
    }
    let menu = match store.get_menu(req.guild, req.key).await {
        Ok(Some(menu)) => menu,
        Ok(None) => return Outcome::new(Code::MenuNotFound),
        Err(error) => return Outcome::with(Code::StoreFailed, error.code()),
    };
    // A stored mode the reader cannot understand is never guessed; naming a
    // mode explicitly repairs it.
    let mode = match req.mode {
        Some(mode) => mode,
        None => match MenuMode::parse(&menu.mode) {
            Some(mode) => mode,
            None => return Outcome::new(Code::CorruptRow),
        },
    };
    let max_select = req.max_select.unwrap_or(menu.max_select);
    if !mode_fits(mode, max_select) {
        return Outcome::new(Code::InvalidMode);
    }
    match store.update_menu(menu.id, mode.as_str(), max_select).await {
        Ok(()) => Outcome::new(Code::MenuUpdated),
        Err(RepoError::MenuMissing) => Outcome::new(Code::MenuNotFound),
        Err(RepoError::InvalidMode) => Outcome::new(Code::InvalidMode),
        Err(error) => Outcome::with(Code::StoreFailed, error.code()),
    }
}

/// `/rolemenu post <key>`.
///
/// Posts the managed embed of the menu, or edits the message the row already
/// names. The row is the truth: a menu that was already posted is edited, not
/// posted again. A failed send leaves the row unchanged.
///
/// Three cases the row decides:
///
/// * the row names no message: a new message is sent into the requested
///   channel and the row records it,
/// * the row names a message in the requested channel: that message is edited,
/// * the row names a message in another channel: the call is refused with
///   [`Code::ChannelMismatch`] and nothing is sent or edited, because moving
///   a published menu is not this command's business.
///
/// When the recorded message no longer exists (a moderator deleted it), the
/// call is not lost: a new message is sent into the recorded channel and the
/// row records it. That is reported as [`Code::Reposted`].
///
/// When the recorded CHANNEL no longer exists (a moderator deleted the whole
/// channel), the recorded location is dead: the call is refused with
/// [`Code::ChannelGone`] and the row is left unchanged, because a message
/// cannot be sent into a channel that is gone either. The way out is
/// [`unpublish`], which clears the recorded location, after which the next
/// `post` sends the menu into the channel the staff names.
pub async fn post<S: MenuStore + ?Sized, D: MenuDiscord + ?Sized>(
    store: &S,
    discord: &D,
    settings: &Settings,
    req: &PostRequest<'_>,
) -> Outcome {
    if let Err(outcome) = gate(settings, &req.invoker, req.guild) {
        return outcome;
    }
    if validate_key(req.key).is_err() {
        return Outcome::new(Code::InvalidKey);
    }
    if req.channel == 0 {
        return Outcome::new(Code::InvalidTarget);
    }
    let menu = match store.get_menu(req.guild, req.key).await {
        Ok(Some(menu)) => menu,
        Ok(None) => return Outcome::new(Code::MenuNotFound),
        Err(error) => return Outcome::with(Code::StoreFailed, error.code()),
    };
    let options = match store.list_options(menu.id).await {
        Ok(options) => options,
        Err(error) => return Outcome::with(Code::StoreFailed, error.code()),
    };
    if options.is_empty() {
        return Outcome::new(Code::EmptyMenu);
    }
    let chunks = split_chunks(&options);
    match (menu.channel, menu.message) {
        // The row names a message in another channel than the one that was
        // given: refuse, and touch nothing.
        (Some(channel), Some(_message)) if channel != req.channel => {
            Outcome::new(Code::ChannelMismatch)
        }
        (Some(channel), Some(message)) => {
            match discord.edit_menu(channel, message, &chunks).await {
                Ok(()) => Outcome::new(Code::Edited),
                Err(DiscordError::NotFound) => {
                    // The recorded message is gone: post a new one in the
                    // recorded channel and record it.
                    repost(store, discord, menu.id, channel, req.key, &chunks).await
                }
                Err(error) => discord_error(error),
            }
        }
        // The row names no message: send a new one.
        _ => match discord.send_menu(req.channel, &chunks).await {
            Ok(message) => {
                match store
                    .set_published(menu.id, req.channel, message, Some(req.key))
                    .await
                {
                    Ok(()) => Outcome::new(Code::Posted),
                    Err(error) => Outcome::with(Code::StoreFailed, error.code()),
                }
            }
            Err(error) => discord_error(error),
        },
    }
}

/// `/rolemenu unpublish <key>`.
///
/// Clears the recorded location of a menu, so it may be published again.
///
/// This is the way out of a dead location: when the channel a menu was
/// posted in is deleted for good, `post` answers `channel_gone` and the
/// recorded channel and message stay. `unpublish` clears them, and the next
/// `post` sends the menu into the channel the staff names.
///
/// The menu and its options are untouched: only the recorded channel, the
/// recorded message and the embed key are cleared. No member role is
/// touched.
pub async fn unpublish<S: MenuStore + ?Sized>(
    store: &S,
    settings: &Settings,
    req: &UnpublishRequest<'_>,
) -> Outcome {
    if let Err(outcome) = gate(settings, &req.invoker, req.guild) {
        return outcome;
    }
    if validate_key(req.key).is_err() {
        return Outcome::new(Code::InvalidKey);
    }
    let menu = match store.get_menu(req.guild, req.key).await {
        Ok(Some(menu)) => menu,
        Ok(None) => return Outcome::new(Code::MenuNotFound),
        Err(error) => return Outcome::with(Code::StoreFailed, error.code()),
    };
    match store.clear_published(menu.id).await {
        Ok(()) => Outcome::new(Code::Unpublished),
        Err(RepoError::MenuMissing) => Outcome::new(Code::MenuNotFound),
        Err(error) => Outcome::with(Code::StoreFailed, error.code()),
    }
}

/// Sends a new message for a menu whose recorded message is gone.
///
/// The row is only changed when the new message was really sent: a failed
/// send leaves the row exactly as it was.
async fn repost<S: MenuStore + ?Sized, D: MenuDiscord + ?Sized>(
    store: &S,
    discord: &D,
    menu_id: i64,
    channel: u64,
    key: &str,
    chunks: &[RenderedChunk],
) -> Outcome {
    let message = match discord.send_menu(channel, chunks).await {
        Ok(message) => message,
        Err(error) => return discord_error(error),
    };
    match store
        .set_published(menu_id, channel, message, Some(key))
        .await
    {
        Ok(()) => Outcome::new(Code::Reposted),
        Err(error) => Outcome::with(Code::StoreFailed, error.code()),
    }
}

fn discord_error(error: DiscordError) -> Outcome {
    match error {
        DiscordError::Forbidden => Outcome::new(Code::DiscordForbidden),
        DiscordError::NotFound => Outcome::new(Code::MessageGone),
        DiscordError::ChannelGone => Outcome::new(Code::ChannelGone),
        DiscordError::Failed => Outcome::new(Code::DiscordFailed),
    }
}

/// `/rolemenu list`.
///
/// Only the calling guild's menus are listed.
pub async fn list<S: MenuStore + ?Sized>(
    store: &S,
    settings: &Settings,
    req: &ListRequest,
) -> ListReport {
    if let Err(outcome) = gate(settings, &req.invoker, req.guild) {
        return ListReport {
            outcome,
            menus: Vec::new(),
        };
    }
    let menus = match store.list_menus(req.guild).await {
        Ok(menus) => menus,
        Err(error) => {
            return ListReport {
                outcome: Outcome::with(Code::StoreFailed, error.code()),
                menus: Vec::new(),
            }
        }
    };
    let mut listed = Vec::with_capacity(menus.len());
    for menu in menus {
        let options = match store.list_options(menu.id).await {
            Ok(options) => options.len(),
            Err(error) => {
                return ListReport {
                    outcome: Outcome::with(Code::StoreFailed, error.code()),
                    menus: Vec::new(),
                }
            }
        };
        listed.push(ListedMenu {
            key: menu.key,
            mode: menu.mode,
            options: u32::try_from(options).unwrap_or(u32::MAX),
            posted: menu.message.is_some(),
        });
    }
    ListReport {
        outcome: Outcome::new(Code::Listed),
        menus: listed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::{
        ADMINISTRATOR, BAN_MEMBERS, KICK_MEMBERS, MANAGE_GUILD, MANAGE_ROLES, MODERATE_MEMBERS,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;

    // Synthetic ids only; never a real Discord id.
    const GUILD: u64 = 650_001;
    const OTHER_GUILD: u64 = 650_002;
    const CHANNEL: u64 = 650_010;
    const MESSAGE: u64 = 650_020;
    const OWNER: u64 = 650_030;
    const STAFF: u64 = 650_031;
    const MEMBER: u64 = 650_032;
    const STAFF_ROLE: u64 = 650_040;

    const ROLE_OK: u64 = 650_101;
    const ROLE_PRIV: u64 = 650_102;
    const ROLE_MANAGED: u64 = 650_103;
    const ROLE_PROTECTED: u64 = 650_104;
    const ROLE_ABOVE_BOT: u64 = 650_105;
    const ROLE_DELETED: u64 = 650_199;

    // -----------------------------------------------------------------------
    // Fakes
    // -----------------------------------------------------------------------

    /// An in-memory menu store that records every call.
    #[derive(Default)]
    struct FakeStore {
        menus: Mutex<Vec<MenuRow>>,
        options: Mutex<Vec<OptionRow>>,
        next_id: Mutex<i64>,
        /// When set, every call fails with this error.
        fail: Option<RepoError>,
        /// Set to fail only `set_published`.
        fail_publish: Option<RepoError>,
        /// Set to fail only `clear_published`.
        fail_clear: Option<RepoError>,
        /// Set to fail only `update_menu`.
        fail_update: Option<RepoError>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn new() -> FakeStore {
            FakeStore {
                next_id: Mutex::new(1),
                ..Default::default()
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn menu(&self, guild: u64, key: &str) -> Option<MenuRow> {
            self.menus
                .lock()
                .unwrap()
                .iter()
                .find(|m| m.guild == guild && m.key == key)
                .cloned()
        }

        fn option_count(&self) -> usize {
            self.options.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl MenuStore for FakeStore {
        async fn create_menu(
            &self,
            guild: u64,
            key: &str,
            mode: &str,
            max_select: u32,
        ) -> Result<i64, RepoError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create_menu {key}"));
            if let Some(error) = self.fail {
                return Err(error);
            }
            let mut menus = self.menus.lock().unwrap();
            if menus.iter().any(|m| m.key == key) {
                return Err(RepoError::KeyTaken);
            }
            let mut next = self.next_id.lock().unwrap();
            let id = *next;
            *next += 1;
            menus.push(MenuRow {
                id,
                guild,
                key: key.to_string(),
                mode: mode.to_string(),
                max_select,
                channel: None,
                message: None,
                embed_key: None,
            });
            Ok(id)
        }

        async fn get_menu(&self, guild: u64, key: &str) -> Result<Option<MenuRow>, RepoError> {
            self.calls.lock().unwrap().push(format!("get_menu {key}"));
            if let Some(error) = self.fail {
                return Err(error);
            }
            Ok(self.menu(guild, key))
        }

        async fn list_menus(&self, guild: u64) -> Result<Vec<MenuRow>, RepoError> {
            self.calls.lock().unwrap().push("list_menus".to_string());
            if let Some(error) = self.fail {
                return Err(error);
            }
            let mut rows: Vec<MenuRow> = self
                .menus
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.guild == guild)
                .cloned()
                .collect();
            rows.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(rows)
        }

        async fn add_option(
            &self,
            menu_id: i64,
            role: u64,
            label: &str,
            emoji: &str,
        ) -> Result<i64, RepoError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("add_option {menu_id} {role}"));
            if let Some(error) = self.fail {
                return Err(error);
            }
            let mut options = self.options.lock().unwrap();
            if options
                .iter()
                .any(|o| o.menu_id == menu_id && o.role == role)
            {
                return Err(RepoError::OptionExists);
            }
            let position = options
                .iter()
                .filter(|o| o.menu_id == menu_id)
                .map(|o| o.position)
                .max()
                .map_or(0, |m| m + 1);
            options.push(OptionRow {
                menu_id,
                role,
                label: label.to_string(),
                emoji: emoji.to_string(),
                position,
            });
            Ok(position)
        }

        async fn list_options(&self, menu_id: i64) -> Result<Vec<OptionRow>, RepoError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("list_options {menu_id}"));
            if let Some(error) = self.fail {
                return Err(error);
            }
            let mut rows: Vec<OptionRow> = self
                .options
                .lock()
                .unwrap()
                .iter()
                .filter(|o| o.menu_id == menu_id)
                .cloned()
                .collect();
            rows.sort_by_key(|o| o.position);
            Ok(rows)
        }

        async fn remove_option(&self, menu_id: i64, role: u64) -> Result<bool, RepoError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("remove_option {menu_id} {role}"));
            if let Some(error) = self.fail {
                return Err(error);
            }
            let mut options = self.options.lock().unwrap();
            let before = options.len();
            options.retain(|o| !(o.menu_id == menu_id && o.role == role));
            Ok(options.len() != before)
        }

        async fn count_options(&self, menu_id: i64) -> Result<u32, RepoError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("count_options {menu_id}"));
            if let Some(error) = self.fail {
                return Err(error);
            }
            let options = self.options.lock().unwrap();
            let count = options.iter().filter(|o| o.menu_id == menu_id).count();
            Ok(u32::try_from(count).unwrap_or(u32::MAX))
        }

        async fn set_published(
            &self,
            menu_id: i64,
            channel: u64,
            message: u64,
            embed_key: Option<&str>,
        ) -> Result<(), RepoError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set_published {menu_id}"));
            if let Some(error) = self.fail_publish {
                return Err(error);
            }
            if let Some(error) = self.fail {
                return Err(error);
            }
            let mut menus = self.menus.lock().unwrap();
            for menu in menus.iter_mut() {
                if menu.id == menu_id {
                    menu.channel = Some(channel);
                    menu.message = Some(message);
                    menu.embed_key = embed_key.map(str::to_string);
                    return Ok(());
                }
            }
            Err(RepoError::MenuMissing)
        }

        async fn update_menu(
            &self,
            menu_id: i64,
            mode: &str,
            max_select: u32,
        ) -> Result<(), RepoError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("update_menu {menu_id}"));
            if let Some(error) = self.fail_update {
                return Err(error);
            }
            if let Some(error) = self.fail {
                return Err(error);
            }
            let mut menus = self.menus.lock().unwrap();
            for menu in menus.iter_mut() {
                if menu.id == menu_id {
                    menu.mode = mode.to_string();
                    menu.max_select = max_select;
                    return Ok(());
                }
            }
            Err(RepoError::MenuMissing)
        }

        async fn clear_published(&self, menu_id: i64) -> Result<(), RepoError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("clear_published {menu_id}"));
            if let Some(error) = self.fail_clear {
                return Err(error);
            }
            if let Some(error) = self.fail {
                return Err(error);
            }
            let mut menus = self.menus.lock().unwrap();
            for menu in menus.iter_mut() {
                if menu.id == menu_id {
                    menu.channel = None;
                    menu.message = None;
                    menu.embed_key = None;
                    return Ok(());
                }
            }
            Err(RepoError::MenuMissing)
        }
    }

    /// A Discord fake that records the calls it was given.
    #[derive(Default)]
    struct FakeDiscord {
        sends: Mutex<Vec<(u64, Vec<RenderedChunk>)>>,
        edits: Mutex<Vec<(u64, u64, Vec<RenderedChunk>)>>,
        /// When set, every call fails with this error.
        fail: Option<DiscordError>,
        /// When set, every edit fails with this error (a deleted message).
        fail_edit: Option<DiscordError>,
        /// When set, every call into this channel fails with `ChannelGone`.
        gone_channel: Option<u64>,
        next_message: Mutex<u64>,
    }

    impl FakeDiscord {
        fn new() -> FakeDiscord {
            FakeDiscord {
                next_message: Mutex::new(MESSAGE),
                ..Default::default()
            }
        }

        fn send_count(&self) -> usize {
            self.sends.lock().unwrap().len()
        }

        fn edit_count(&self) -> usize {
            self.edits.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl MenuDiscord for FakeDiscord {
        async fn send_menu(
            &self,
            channel: u64,
            chunks: &[RenderedChunk],
        ) -> Result<u64, DiscordError> {
            self.sends.lock().unwrap().push((channel, chunks.to_vec()));
            if self.gone_channel == Some(channel) {
                return Err(DiscordError::ChannelGone);
            }
            if let Some(error) = self.fail {
                return Err(error);
            }
            let mut next = self.next_message.lock().unwrap();
            let id = *next;
            *next += 1;
            Ok(id)
        }

        async fn edit_menu(
            &self,
            channel: u64,
            message: u64,
            chunks: &[RenderedChunk],
        ) -> Result<(), DiscordError> {
            self.edits
                .lock()
                .unwrap()
                .push((channel, message, chunks.to_vec()));
            if self.gone_channel == Some(channel) {
                return Err(DiscordError::ChannelGone);
            }
            if let Some(error) = self.fail_edit {
                return Err(error);
            }
            match self.fail {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    /// A menu store over a real SQLite database.
    ///
    /// The commands are exercised over the real tables here, not over a fake:
    /// the point of this store is that a row survives a restart and that the
    /// foreign key and the unique key really refuse a write.
    struct SqlStore {
        conn: Mutex<rusqlite::Connection>,
    }

    impl SqlStore {
        fn open(path: &std::path::Path) -> SqlStore {
            let mut conn = crate::storage::open(path.to_str().unwrap(), &[]).unwrap();
            crate::role_menus::repository::migrate(&mut conn).unwrap();
            SqlStore {
                conn: Mutex::new(conn),
            }
        }
    }

    #[async_trait]
    impl MenuStore for SqlStore {
        async fn create_menu(
            &self,
            guild: u64,
            key: &str,
            mode: &str,
            max_select: u32,
        ) -> Result<i64, RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::create_menu(&conn, guild, key, mode, max_select)
        }

        async fn get_menu(&self, guild: u64, key: &str) -> Result<Option<MenuRow>, RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::get_menu(&conn, guild, key)
        }

        async fn list_menus(&self, guild: u64) -> Result<Vec<MenuRow>, RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::list_menus(&conn, guild)
        }

        async fn add_option(
            &self,
            menu_id: i64,
            role: u64,
            label: &str,
            emoji: &str,
        ) -> Result<i64, RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::add_option(&conn, menu_id, role, label, emoji)
        }

        async fn list_options(&self, menu_id: i64) -> Result<Vec<OptionRow>, RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::list_options(&conn, menu_id)
        }

        async fn remove_option(&self, menu_id: i64, role: u64) -> Result<bool, RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::remove_option(&conn, menu_id, role)
        }

        async fn count_options(&self, menu_id: i64) -> Result<u32, RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::count_options(&conn, menu_id)
        }

        async fn set_published(
            &self,
            menu_id: i64,
            channel: u64,
            message: u64,
            embed_key: Option<&str>,
        ) -> Result<(), RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::set_published(
                &conn, menu_id, channel, message, embed_key,
            )
        }

        async fn update_menu(
            &self,
            menu_id: i64,
            mode: &str,
            max_select: u32,
        ) -> Result<(), RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::update_menu(&conn, menu_id, mode, max_select)
        }

        async fn clear_published(&self, menu_id: i64) -> Result<(), RepoError> {
            let conn = self.conn.lock().unwrap();
            crate::role_menus::repository::clear_published(&conn, menu_id)
        }
    }

    /// A role-facts fake.
    #[derive(Default)]
    struct FakeFacts {
        roles: Mutex<HashMap<u64, RoleFacts>>,
        bot_top: Mutex<Option<u16>>,
    }

    impl FakeFacts {
        fn new() -> FakeFacts {
            let facts = FakeFacts::default();
            {
                let mut roles = facts.roles.lock().unwrap();
                for id in [
                    ROLE_OK,
                    ROLE_PRIV,
                    ROLE_MANAGED,
                    ROLE_PROTECTED,
                    ROLE_ABOVE_BOT,
                ] {
                    roles.insert(
                        id,
                        RoleFacts {
                            id,
                            position: 5,
                            managed: false,
                            permissions: 0,
                            exists: true,
                        },
                    );
                }
            }
            *facts.bot_top.lock().unwrap() = Some(10);
            facts
        }

        fn set(&self, id: u64, f: RoleFacts) {
            self.roles.lock().unwrap().insert(id, f);
        }

        fn with_bot_top(self, top: Option<u16>) -> FakeFacts {
            *self.bot_top.lock().unwrap() = top;
            self
        }
    }

    #[async_trait]
    impl RoleFactsSource for FakeFacts {
        async fn role_facts(&self, guild: u64, role: u64) -> Option<RoleFacts> {
            if guild == 0 {
                return None;
            }
            self.roles.lock().unwrap().get(&role).copied()
        }

        async fn bot_top_position(&self, _guild: u64) -> Option<u16> {
            *self.bot_top.lock().unwrap()
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn settings() -> Settings {
        Settings {
            enabled: true,
            home_guild: Some(GUILD),
            staff_role_id: Some(STAFF_ROLE),
            bot_has_manage_roles: true,
            protected_role_ids: vec![ROLE_PROTECTED],
            protected_roles_invalid: false,
        }
    }

    fn owner() -> Invoker {
        Invoker {
            user: OWNER,
            guild_owner: OWNER,
            is_staff: false,
        }
    }

    fn staff() -> Invoker {
        Invoker {
            user: STAFF,
            guild_owner: OWNER,
            is_staff: true,
        }
    }

    fn member() -> Invoker {
        Invoker {
            user: MEMBER,
            guild_owner: OWNER,
            is_staff: false,
        }
    }

    fn create_req(key: &str) -> CreateRequest<'_> {
        CreateRequest {
            invoker: staff(),
            guild: GUILD,
            key,
            mode: MenuMode::Exclusive,
            max_select: 0,
        }
    }

    fn add_req(key: &str, role: u64) -> AddOptionRequest<'_> {
        AddOptionRequest {
            invoker: staff(),
            guild: GUILD,
            key,
            role,
            label: "Label",
            emoji: "🎉",
        }
    }

    fn remove_req(key: &str, role: u64) -> RemoveOptionRequest<'_> {
        RemoveOptionRequest {
            invoker: staff(),
            guild: GUILD,
            key,
            role,
        }
    }

    fn post_req(key: &str) -> PostRequest<'_> {
        PostRequest {
            invoker: staff(),
            guild: GUILD,
            key,
            channel: CHANNEL,
        }
    }

    async fn seed_menu(store: &FakeStore, key: &str, roles: &[u64]) {
        create(
            store,
            &settings(),
            &CreateRequest {
                invoker: staff(),
                guild: GUILD,
                key,
                mode: MenuMode::Exclusive,
                max_select: 0,
            },
        )
        .await;
        for role in roles {
            let label = format!("Label {role}");
            add_option(
                store,
                &FakeFacts::new(),
                &settings(),
                &AddOptionRequest {
                    invoker: staff(),
                    guild: GUILD,
                    key,
                    role: *role,
                    label: &label,
                    emoji: "🎉",
                },
            )
            .await;
        }
    }

    // -----------------------------------------------------------------------
    // Pure: key, label, emoji
    // -----------------------------------------------------------------------

    #[test]
    fn a_plain_key_is_accepted_and_a_hostile_spelling_is_not() {
        for good in ["a", "main", "role-menu", "role_menu", "m1", "0"] {
            assert_eq!(validate_key(good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "Main",
            "-main",
            "_main",
            "main menu",
            "main/menu",
            "main.menu",
            "main..",
            "main/../etc",
            "main\\x",
            "main\0x",
            "main\n",
            "caf\u{e9}",
            "ma\u{200b}in",
        ] {
            assert_eq!(validate_key(bad), Err(Code::InvalidKey), "{bad:?}");
        }
        // A key longer than the limit is refused.
        assert_eq!(
            validate_key(&"a".repeat(MAX_KEY_BYTES + 1)),
            Err(Code::InvalidKey)
        );
        assert_eq!(validate_key(&"a".repeat(MAX_KEY_BYTES)), Ok(()));
    }

    #[test]
    fn a_label_must_be_non_empty_bounded_and_free_of_hidden_characters() {
        assert_eq!(validate_label("Alpha"), Ok(()));
        assert_eq!(validate_label("  Alpha  "), Ok(()));
        assert_eq!(validate_label(""), Err(Code::InvalidLabel));
        assert_eq!(validate_label("   "), Err(Code::InvalidLabel));
        assert_eq!(
            validate_label(&"a".repeat(MAX_LABEL_CHARS + 1)),
            Err(Code::InvalidLabel)
        );
        assert_eq!(validate_label(&"a".repeat(MAX_LABEL_CHARS)), Ok(()));
        // A control character is refused.
        assert_eq!(validate_label("Alpha\u{0}"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("Alpha\u{7}"), Err(Code::InvalidLabel));
        // A newline is refused: a label is one line of plain text.
        assert_eq!(validate_label("Alpha\nBeta"), Err(Code::InvalidLabel));
        // The line and paragraph separators are refused.
        assert_eq!(validate_label("Alpha\u{2028}Beta"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("Alpha\u{2029}Beta"), Err(Code::InvalidLabel));
        // A format character is refused: zero-width and bidirectional marks.
        assert_eq!(validate_label("Alpha\u{200B}"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("Alpha\u{FEFF}"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("Alpha\u{202E}evil"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("Alpha\u{200E}"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("Alpha\u{2060}"), Err(Code::InvalidLabel));
        // A label that is only invisible is refused.
        assert_eq!(validate_label("\u{200B}"), Err(Code::InvalidLabel));
        // Mention markup is refused.
        assert_eq!(validate_label("<@&650101>"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("<@650101>"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("<#650010>"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("<:party:650101>"), Err(Code::InvalidLabel));
        // A plain angle bracket is not markup and stays allowed.
        assert_eq!(validate_label("Level < 5"), Ok(()));
    }

    #[test]
    fn an_emoji_must_be_a_custom_emoji_or_a_unicode_emoji() {
        // Plain Unicode emoji are accepted.
        assert_eq!(validate_emoji("🎉"), Ok(()));
        assert_eq!(validate_emoji("👍"), Ok(()));
        assert_eq!(validate_emoji("🎉\u{FE0F}"), Ok(()));
        assert_eq!(validate_emoji("👍🏽"), Ok(()));
        // The Discord custom-emoji forms are accepted.
        assert_eq!(validate_emoji("<:party:650101>"), Ok(()));
        assert_eq!(validate_emoji("<a:party:650101>"), Ok(()));
        assert_eq!(validate_emoji("<:two_words:650101>"), Ok(()));
        // Free text is refused.
        assert_eq!(
            validate_emoji("not an emoji at all"),
            Err(Code::InvalidEmoji)
        );
        assert_eq!(validate_emoji("x"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji(""), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("   "), Err(Code::InvalidEmoji));
        // A mention is refused.
        assert_eq!(validate_emoji("@everyone"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("<@&650101>"), Err(Code::InvalidEmoji));
        // A link is refused.
        assert_eq!(
            validate_emoji("https://example.com/x"),
            Err(Code::InvalidEmoji)
        );
        // An invisible or a hiding mark is refused.
        assert_eq!(validate_emoji("\u{200B}"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("\u{FEFF}"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("\u{202E}x"), Err(Code::InvalidEmoji));
        // A control character is refused.
        assert_eq!(validate_emoji("x\u{0}"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("x\n"), Err(Code::InvalidEmoji));
        // A too long emoji is refused.
        assert_eq!(
            validate_emoji(&"🎉".repeat(MAX_EMOJI_CHARS + 1)),
            Err(Code::InvalidEmoji)
        );
        // A string of many emoji is refused as well: it is not ONE emoji.
        assert_eq!(
            validate_emoji(&"🎉".repeat(MAX_EMOJI_CHARS)),
            Err(Code::InvalidEmoji)
        );
        // The length bound still bites on one long emoji: a ZWJ chain of
        // bases is longer than the limit.
        let long_chain = "👨".repeat(MAX_EMOJI_CHARS) + "‍";
        assert_eq!(validate_emoji(&long_chain), Err(Code::InvalidEmoji));
        // A custom emoji with a bad name or id is refused.
        assert_eq!(validate_emoji("<:a:650101>"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("<:party:0>"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("<:party:abc>"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("<:party:650101"), Err(Code::InvalidEmoji));
        assert_eq!(
            validate_emoji("<:bad name:650101>"),
            Err(Code::InvalidEmoji)
        );
        assert_eq!(
            validate_emoji("<:party:650101> trailing"),
            Err(Code::InvalidEmoji)
        );
    }

    // -----------------------------------------------------------------------
    // Pure: the gate
    // -----------------------------------------------------------------------

    #[test]
    fn configuration_requires_the_staff_role_or_the_guild_owner() {
        let s = settings();
        assert_eq!(gate(&s, &staff(), GUILD), Ok(()));
        assert_eq!(gate(&s, &owner(), GUILD), Ok(()));
        assert_eq!(gate(&s, &member(), GUILD), Err(Outcome::new(Code::Denied)));
        // A caller id of zero is never trusted.
        let zero = Invoker {
            user: 0,
            guild_owner: OWNER,
            is_staff: true,
        };
        assert_eq!(gate(&s, &zero, GUILD), Err(Outcome::new(Code::Denied)));
        // Without a configured staff role, only the owner may configure.
        let mut no_staff = s.clone();
        no_staff.staff_role_id = None;
        assert_eq!(
            gate(&no_staff, &staff(), GUILD),
            Err(Outcome::new(Code::Denied))
        );
        assert_eq!(gate(&no_staff, &owner(), GUILD), Ok(()));
        // A staff role of zero is not a staff role.
        let mut zero_role = s.clone();
        zero_role.staff_role_id = Some(0);
        assert_eq!(
            gate(&zero_role, &staff(), GUILD),
            Err(Outcome::new(Code::Denied))
        );
    }

    #[test]
    fn configuration_requires_manage_roles_for_the_bot() {
        let mut s = settings();
        s.bot_has_manage_roles = false;
        assert_eq!(
            gate(&s, &owner(), GUILD),
            Err(Outcome::new(Code::BotMissingManageRoles))
        );
        assert_eq!(
            gate(&s, &staff(), GUILD),
            Err(Outcome::new(Code::BotMissingManageRoles))
        );
    }

    #[test]
    fn a_foreign_guild_is_refused_and_an_unset_home_guild_fails_closed() {
        let s = settings();
        assert_eq!(
            gate(&s, &owner(), OTHER_GUILD),
            Err(Outcome::new(Code::OtherGuild))
        );
        assert_eq!(
            gate(&s, &owner(), 0),
            Err(Outcome::new(Code::InvalidTarget))
        );
        let mut unset = s.clone();
        unset.home_guild = None;
        assert_eq!(
            gate(&unset, &owner(), GUILD),
            Err(Outcome::new(Code::NoHomeGuild))
        );
        let mut zero = s.clone();
        zero.home_guild = Some(0);
        assert_eq!(
            gate(&zero, &owner(), GUILD),
            Err(Outcome::new(Code::NoHomeGuild))
        );
    }

    // -----------------------------------------------------------------------
    // Pure: the role check
    // -----------------------------------------------------------------------

    #[test]
    fn a_privileged_managed_or_protected_role_is_refused_at_configuration_time() {
        let s = settings();
        let ok = RoleFacts {
            id: ROLE_OK,
            position: 5,
            managed: false,
            permissions: 0,
            exists: true,
        };
        assert_eq!(check_role(&s, &ok, 10, GUILD), Ok(()));

        let privileged = RoleFacts {
            id: ROLE_PRIV,
            permissions: ADMINISTRATOR,
            ..ok
        };
        assert_eq!(
            check_role(&s, &privileged, 10, GUILD),
            Err(Outcome::with(Code::Ineligible, "privileged_permission"))
        );
        for bit in [
            ADMINISTRATOR,
            MANAGE_GUILD,
            MANAGE_ROLES,
            BAN_MEMBERS,
            KICK_MEMBERS,
            MODERATE_MEMBERS,
        ] {
            let facts = RoleFacts {
                id: ROLE_PRIV,
                permissions: bit,
                ..ok
            };
            let outcome = check_role(&s, &facts, 10, GUILD).unwrap_err();
            assert_eq!(outcome.detail, Some("privileged_permission"), "{bit:#x}");
        }

        let managed = RoleFacts {
            id: ROLE_MANAGED,
            managed: true,
            ..ok
        };
        assert_eq!(
            check_role(&s, &managed, 10, GUILD),
            Err(Outcome::with(Code::Ineligible, "managed_role"))
        );

        let protected = RoleFacts {
            id: ROLE_PROTECTED,
            ..ok
        };
        assert_eq!(
            check_role(&s, &protected, 10, GUILD),
            Err(Outcome::with(Code::Ineligible, "protected_role"))
        );

        let above = RoleFacts {
            id: ROLE_ABOVE_BOT,
            position: 10,
            ..ok
        };
        assert_eq!(
            check_role(&s, &above, 10, GUILD),
            Err(Outcome::with(Code::Ineligible, "at_or_above_bot"))
        );

        let deleted = RoleFacts {
            id: ROLE_DELETED,
            exists: false,
            ..ok
        };
        assert_eq!(
            check_role(&s, &deleted, 10, GUILD),
            Err(Outcome::with(Code::Ineligible, "role_deleted"))
        );

        // The @everyone role (its id equals the guild id) is refused.
        let everyone = RoleFacts { id: GUILD, ..ok };
        assert_eq!(
            check_role(&s, &everyone, 10, GUILD),
            Err(Outcome::with(Code::Ineligible, "everyone_role"))
        );
    }

    // -----------------------------------------------------------------------
    // create
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_makes_a_menu_and_the_key_is_unique() {
        let store = FakeStore::new();
        let outcome = create(&store, &settings(), &create_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::Created));
        let menu = store.menu(GUILD, "main").expect("menu stored");
        assert_eq!(menu.mode, "exclusive");
        assert_eq!(menu.max_select, 0);
        assert_eq!(menu.channel, None);
        assert_eq!(menu.message, None);

        // A second create with the same key is refused.
        let outcome = create(&store, &settings(), &create_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::KeyTaken));
        assert_eq!(store.menus.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_refuses_a_hostile_key_and_writes_nothing() {
        let store = FakeStore::new();
        let outcome = create(&store, &settings(), &create_req("Main Menu")).await;
        assert_eq!(outcome, Outcome::new(Code::InvalidKey));
        assert!(store.menus.lock().unwrap().is_empty());
        assert!(!store.calls().iter().any(|c| c.starts_with("create_menu")));
    }

    #[tokio::test]
    async fn create_in_a_foreign_guild_does_nothing() {
        let store = FakeStore::new();
        let mut req = create_req("main");
        req.guild = OTHER_GUILD;
        let outcome = create(&store, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::OtherGuild));
        assert!(store.menus.lock().unwrap().is_empty());
        assert!(store.calls().is_empty());
    }

    #[tokio::test]
    async fn create_by_a_non_staff_caller_is_denied_and_writes_nothing() {
        let store = FakeStore::new();
        let mut req = create_req("main");
        req.invoker = member();
        let outcome = create(&store, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::Denied));
        assert!(store.menus.lock().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // option add
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn adding_an_option_stores_it_and_repeating_the_role_is_refused() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        seed_menu(&store, "main", &[]).await;
        let outcome = add_option(&store, &facts, &settings(), &add_req("main", ROLE_OK)).await;
        assert_eq!(outcome, Outcome::new(Code::OptionAdded));
        assert_eq!(store.option_count(), 1);

        let outcome = add_option(&store, &facts, &settings(), &add_req("main", ROLE_OK)).await;
        assert_eq!(outcome, Outcome::new(Code::OptionExists));
        assert_eq!(store.option_count(), 1);
    }

    #[tokio::test]
    async fn a_privileged_managed_or_protected_role_is_refused_and_stored_nowhere() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        seed_menu(&store, "main", &[]).await;

        facts.set(
            ROLE_PRIV,
            RoleFacts {
                id: ROLE_PRIV,
                position: 5,
                managed: false,
                permissions: ADMINISTRATOR,
                exists: true,
            },
        );
        let outcome = add_option(&store, &facts, &settings(), &add_req("main", ROLE_PRIV)).await;
        assert_eq!(
            outcome,
            Outcome::with(Code::Ineligible, "privileged_permission")
        );

        facts.set(
            ROLE_MANAGED,
            RoleFacts {
                id: ROLE_MANAGED,
                position: 5,
                managed: true,
                permissions: 0,
                exists: true,
            },
        );
        let outcome = add_option(&store, &facts, &settings(), &add_req("main", ROLE_MANAGED)).await;
        assert_eq!(outcome, Outcome::with(Code::Ineligible, "managed_role"));

        let outcome = add_option(
            &store,
            &facts,
            &settings(),
            &add_req("main", ROLE_PROTECTED),
        )
        .await;
        assert_eq!(outcome, Outcome::with(Code::Ineligible, "protected_role"));

        assert_eq!(store.option_count(), 0, "no refused option is stored");
    }

    #[tokio::test]
    async fn an_unusable_protected_role_list_fails_closed() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        seed_menu(&store, "main", &[]).await;
        let mut s = settings();
        s.protected_roles_invalid = true;
        let outcome = add_option(&store, &facts, &s, &add_req("main", ROLE_OK)).await;
        assert_eq!(outcome, Outcome::new(Code::Ineligible));
        assert_eq!(store.option_count(), 0);
    }

    #[tokio::test]
    async fn an_unknown_role_and_an_unknown_bot_position_fail_closed() {
        let store = FakeStore::new();
        seed_menu(&store, "main", &[]).await;

        let facts = FakeFacts::new();
        let outcome = add_option(&store, &facts, &settings(), &add_req("main", ROLE_DELETED)).await;
        assert_eq!(outcome, Outcome::new(Code::FactsUnavailable));
        assert_eq!(store.option_count(), 0);

        let facts = FakeFacts::new().with_bot_top(None);
        let outcome = add_option(&store, &facts, &settings(), &add_req("main", ROLE_OK)).await;
        assert_eq!(outcome, Outcome::new(Code::FactsUnavailable));
        assert_eq!(store.option_count(), 0);
    }

    #[tokio::test]
    async fn a_hostile_label_or_emoji_is_refused_and_stored_nowhere() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        seed_menu(&store, "main", &[]).await;

        let mut req = add_req("main", ROLE_OK);
        req.label = "";
        let outcome = add_option(&store, &facts, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::InvalidLabel));

        let mut req = add_req("main", ROLE_OK);
        let long_label = "a".repeat(MAX_LABEL_CHARS + 1);
        req.label = &long_label;
        let outcome = add_option(&store, &facts, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::InvalidLabel));

        let mut req = add_req("main", ROLE_OK);
        req.emoji = "";
        let outcome = add_option(&store, &facts, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::InvalidEmoji));

        let mut req = add_req("main", ROLE_OK);
        req.emoji = "x\u{0}";
        let outcome = add_option(&store, &facts, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::InvalidEmoji));

        assert_eq!(store.option_count(), 0);
    }

    #[tokio::test]
    async fn adding_an_option_to_a_missing_menu_is_refused() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        let outcome = add_option(&store, &facts, &settings(), &add_req("ghost", ROLE_OK)).await;
        assert_eq!(outcome, Outcome::new(Code::MenuNotFound));
        assert_eq!(store.option_count(), 0);
    }

    #[tokio::test]
    async fn adding_an_option_in_a_foreign_guild_does_nothing() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        seed_menu(&store, "main", &[]).await;
        let mut req = add_req("main", ROLE_OK);
        req.guild = OTHER_GUILD;
        let outcome = add_option(&store, &facts, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::OtherGuild));
        assert_eq!(store.option_count(), 0);
    }

    // -----------------------------------------------------------------------
    // option remove
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn removing_an_option_removes_the_row_and_never_touches_member_roles() {
        let store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        let outcome = remove_option(&store, &settings(), &remove_req("main", ROLE_OK)).await;
        assert_eq!(outcome, Outcome::new(Code::OptionRemoved));
        assert_eq!(store.option_count(), 0);

        // Removing it again reports that there was nothing to remove.
        let outcome = remove_option(&store, &settings(), &remove_req("main", ROLE_OK)).await;
        assert_eq!(outcome, Outcome::new(Code::OptionNotFound));
    }

    #[tokio::test]
    async fn removing_an_option_of_a_missing_menu_is_refused() {
        let store = FakeStore::new();
        let outcome = remove_option(&store, &settings(), &remove_req("ghost", ROLE_OK)).await;
        assert_eq!(outcome, Outcome::new(Code::MenuNotFound));
    }

    // -----------------------------------------------------------------------
    // post / edit
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn post_sends_the_embed_and_records_the_message() {
        let store = FakeStore::new();
        let discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::Posted));
        assert_eq!(discord.send_count(), 1);
        assert_eq!(discord.edit_count(), 0);
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!(menu.channel, Some(CHANNEL));
        assert_eq!(menu.message, Some(MESSAGE));
        assert_eq!(menu.embed_key.as_deref(), Some("main"));
    }

    #[tokio::test]
    async fn post_is_idempotent_and_edits_the_same_message() {
        let store = FakeStore::new();
        let discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        post(&store, &discord, &settings(), &post_req("main")).await;
        let first = store.menu(GUILD, "main").unwrap().message;

        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::Edited));
        assert_eq!(discord.send_count(), 1, "no second message");
        assert_eq!(discord.edit_count(), 1);
        let (channel, edited, _) = discord.edits.lock().unwrap()[0].clone();
        assert_eq!(channel, CHANNEL, "the stored channel is passed to the edit");
        assert_eq!(Some(edited), first, "the recorded message is edited");
        assert_eq!(store.menu(GUILD, "main").unwrap().message, first);
    }

    #[tokio::test]
    async fn a_failed_send_leaves_the_row_unchanged_and_reports_a_stable_code() {
        let store = FakeStore::new();
        let mut discord = FakeDiscord::new();
        discord.fail = Some(DiscordError::Failed);
        seed_menu(&store, "main", &[ROLE_OK]).await;

        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::DiscordFailed));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!(menu.channel, None, "the row is unchanged");
        assert_eq!(menu.message, None);
        assert_eq!(menu.embed_key, None);

        // A forbidden call is reported with its own code.
        let mut discord = FakeDiscord::new();
        discord.fail = Some(DiscordError::Forbidden);
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::DiscordForbidden));
        assert_eq!(store.menu(GUILD, "main").unwrap().message, None);
    }

    #[tokio::test]
    async fn a_failed_edit_leaves_the_recorded_message_unchanged() {
        let store = FakeStore::new();
        let mut discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        post(&store, &discord, &settings(), &post_req("main")).await;
        discord.fail = Some(DiscordError::Failed);

        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::DiscordFailed));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!(menu.message, Some(MESSAGE));
        assert_eq!(menu.channel, Some(CHANNEL));
    }

    #[tokio::test]
    async fn posting_a_menu_without_options_is_refused_and_nothing_is_sent() {
        let store = FakeStore::new();
        let discord = FakeDiscord::new();
        seed_menu(&store, "main", &[]).await;
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::EmptyMenu));
        assert_eq!(discord.send_count(), 0);
        assert_eq!(discord.edit_count(), 0);
    }

    #[tokio::test]
    async fn posting_a_missing_menu_is_refused_and_nothing_is_sent() {
        let store = FakeStore::new();
        let discord = FakeDiscord::new();
        let outcome = post(&store, &discord, &settings(), &post_req("ghost")).await;
        assert_eq!(outcome, Outcome::new(Code::MenuNotFound));
        assert_eq!(discord.send_count(), 0);
    }

    #[tokio::test]
    async fn posting_in_a_foreign_guild_sends_nothing() {
        let store = FakeStore::new();
        let discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        let mut req = post_req("main");
        req.guild = OTHER_GUILD;
        let outcome = post(&store, &discord, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::OtherGuild));
        assert_eq!(discord.send_count(), 0);
        assert_eq!(discord.edit_count(), 0);
        assert_eq!(store.menu(GUILD, "main").unwrap().message, None);
    }

    #[tokio::test]
    async fn more_than_twenty_five_options_are_split_and_none_is_dropped() {
        let store = FakeStore::new();
        let discord = FakeDiscord::new();
        create(&store, &settings(), &create_req("big")).await;
        for index in 0..60u64 {
            store.options.lock().unwrap().push(OptionRow {
                menu_id: store.menu(GUILD, "big").unwrap().id,
                role: 700_000 + index,
                label: format!("Label {index}"),
                emoji: "🎉".to_string(),
                position: index as i64,
            });
        }
        let outcome = post(&store, &discord, &settings(), &post_req("big")).await;
        assert_eq!(outcome, Outcome::new(Code::Posted));
        let sends = discord.sends.lock().unwrap();
        let chunks = &sends[0].1;
        assert_eq!(chunks.len(), 3, "60 options need three messages");
        assert_eq!(chunks[0].options.len(), 25);
        assert_eq!(chunks[1].options.len(), 25);
        assert_eq!(chunks[2].options.len(), 10);
        // No option is lost and the order is kept.
        let flat: Vec<u64> = chunks
            .iter()
            .flat_map(|c| c.options.iter().map(|o| o.role))
            .collect();
        assert_eq!(flat, (0..60).map(|i| 700_000 + i).collect::<Vec<u64>>());
    }

    #[test]
    fn the_split_is_deterministic_and_respects_the_limit() {
        let rows: Vec<OptionRow> = (0..51)
            .map(|i| OptionRow {
                menu_id: 1,
                role: 800_000 + i,
                label: format!("L{i}"),
                emoji: "🎉".to_string(),
                position: i as i64,
            })
            .collect();
        let chunks = split_chunks(&rows);
        assert_eq!(
            chunks.iter().map(|c| c.options.len()).collect::<Vec<_>>(),
            vec![25, 25, 1]
        );
        assert_eq!(chunks[0].part, 0);
        assert_eq!(chunks[2].part, 2);
        assert_eq!(split_chunks(&rows), chunks);
        for chunk in &chunks {
            assert!(chunk.options.len() <= SELECT_OPTION_LIMIT);
        }
        // The boundaries 0, 1, 25, 26, 50, 51.
        assert_eq!(split_chunks(&rows[..0]).len(), 0);
        assert_eq!(split_chunks(&rows[..1]).len(), 1);
        assert_eq!(split_chunks(&rows[..25]).len(), 1);
        assert_eq!(split_chunks(&rows[..26]).len(), 2);
        assert_eq!(split_chunks(&rows[..50]).len(), 2);
    }

    // -----------------------------------------------------------------------
    // list
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_reports_only_the_calling_guilds_menus() {
        let store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        store.menus.lock().unwrap().push(MenuRow {
            id: 99,
            guild: OTHER_GUILD,
            key: "other".to_string(),
            mode: "exclusive".to_string(),
            max_select: 0,
            channel: None,
            message: None,
            embed_key: None,
        });
        let report = list(
            &store,
            &settings(),
            &ListRequest {
                invoker: staff(),
                guild: GUILD,
            },
        )
        .await;
        assert_eq!(report.outcome, Outcome::new(Code::Listed));
        assert_eq!(report.menus.len(), 1);
        assert_eq!(report.menus[0].key, "main");
        assert_eq!(report.menus[0].options, 1);
        assert!(!report.menus[0].posted);
    }

    #[tokio::test]
    async fn list_in_a_foreign_guild_reports_nothing() {
        let store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        let report = list(
            &store,
            &settings(),
            &ListRequest {
                invoker: staff(),
                guild: OTHER_GUILD,
            },
        )
        .await;
        assert_eq!(report.outcome, Outcome::new(Code::OtherGuild));
        assert!(report.menus.is_empty());
    }

    #[tokio::test]
    async fn a_store_failure_is_reported_with_a_stable_code() {
        let mut store = FakeStore::new();
        store.fail = Some(RepoError::Storage(crate::storage::StorageError::Sqlite));
        let report = list(
            &store,
            &settings(),
            &ListRequest {
                invoker: staff(),
                guild: GUILD,
            },
        )
        .await;
        assert_eq!(
            report.outcome,
            Outcome::with(Code::StoreFailed, "sqlite_error")
        );
        assert!(report.menus.is_empty());
    }

    // -----------------------------------------------------------------------
    // Replies
    // -----------------------------------------------------------------------

    #[test]
    fn replies_are_ephemeral_and_carry_codes_only() {
        let reply = reply_for(Outcome::with(Code::Ineligible, "protected_role"));
        assert!(reply.ephemeral());
        assert_eq!(reply.text(), "rolemenu ineligible (protected_role)");
        let report = ListReport {
            outcome: Outcome::new(Code::Listed),
            menus: vec![ListedMenu {
                key: "main".to_string(),
                mode: "exclusive".to_string(),
                options: 3,
                posted: true,
            }],
        };
        let reply = list_reply(&report);
        assert!(reply.ephemeral());
        assert!(reply.text().contains("main"));
        assert!(reply.text().contains("options=3"));
    }

    // -----------------------------------------------------------------------
    // Seeded liveness: a post that fails and retries always settles
    // -----------------------------------------------------------------------

    /// A deterministic LCG; no external randomness.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    #[tokio::test]
    async fn a_seeded_post_sequence_always_settles_and_never_duplicates_a_message() {
        let store = FakeStore::new();
        let mut discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;

        let mut rng = Lcg(0x5EED_1234_ABCD);
        let mut posted = false;
        for step in 0..200 {
            // The Discord call fails on some steps and succeeds on others.
            let fails = rng.next(3) == 0;
            discord.fail = if fails {
                Some(DiscordError::Failed)
            } else {
                None
            };
            let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
            match outcome.code {
                Code::Posted => {
                    assert!(!posted, "the menu was posted twice");
                    posted = true;
                    // The row is the truth: it now names the message.
                    assert_eq!(store.menu(GUILD, "main").unwrap().message, Some(MESSAGE));
                }
                Code::Edited => {
                    assert!(posted, "an edit happened before a post");
                    assert_eq!(store.menu(GUILD, "main").unwrap().message, Some(MESSAGE));
                }
                Code::DiscordFailed => {
                    // A failed send never changes the row.
                    let menu = store.menu(GUILD, "main").unwrap();
                    assert_eq!(menu.message, if posted { Some(MESSAGE) } else { None });
                }
                other => panic!("unexpected code {other:?}"),
            }
            let _ = step;
        }
        // At the end the menu is posted exactly once and one message exists.
        assert!(posted, "the sequence never posted the menu");
        assert_eq!(discord.send_count(), 1, "exactly one message was sent");
        assert_eq!(store.menu(GUILD, "main").unwrap().message, Some(MESSAGE));
    }

    #[tokio::test]
    async fn a_seeded_option_sequence_keeps_the_menu_consistent() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        // The sequence uses roles the fake knows about.
        for index in 0..12u64 {
            facts.set(
                900_000 + index,
                RoleFacts {
                    id: 900_000 + index,
                    position: 5,
                    managed: false,
                    permissions: 0,
                    exists: true,
                },
            );
        }
        seed_menu(&store, "main", &[]).await;

        let mut rng = Lcg(0x1234_5678_9ABC);
        for _ in 0..200 {
            let role = 900_000 + rng.next(12);
            let outcome = add_option(&store, &facts, &settings(), &add_req("main", role)).await;
            // The role is now stored exactly once: either it was added, or it
            // was already there and the add is refused.
            assert!(
                matches!(outcome.code, Code::OptionAdded | Code::OptionExists),
                "{outcome:?}"
            );
            let menu = store.menu(GUILD, "main").unwrap();
            let options = store.list_options(menu.id).await.unwrap();
            assert_eq!(options.iter().filter(|o| o.role == role).count(), 1);
        }
        // Every role is stored once and the count matches.
        let menu = store.menu(GUILD, "main").unwrap();
        let options = store.list_options(menu.id).await.unwrap();
        assert_eq!(options.len(), 12);
        let mut roles: Vec<u64> = options.iter().map(|o| o.role).collect();
        roles.sort_unstable();
        roles.dedup();
        assert_eq!(roles.len(), 12);
    }

    // -----------------------------------------------------------------------
    // A deleted message
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_deleted_message_is_reposted_and_the_row_names_the_new_message() {
        let store = FakeStore::new();
        let mut discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        post(&store, &discord, &settings(), &post_req("main")).await;
        let first = store.menu(GUILD, "main").unwrap().message.unwrap();

        // The message was deleted: every edit reports that it is gone.
        discord.fail_edit = Some(DiscordError::NotFound);
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::Reposted));
        let second = store.menu(GUILD, "main").unwrap().message.unwrap();
        assert_ne!(second, first, "a new message was sent");
        assert_eq!(discord.send_count(), 2, "one new message was sent");
        assert_eq!(store.menu(GUILD, "main").unwrap().channel, Some(CHANNEL));
    }

    #[tokio::test]
    async fn a_failed_repost_leaves_the_row_unchanged() {
        let store = FakeStore::new();
        let mut discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        post(&store, &discord, &settings(), &post_req("main")).await;
        let first = store.menu(GUILD, "main").unwrap().message;

        // The message is gone and the new send fails too.
        discord.fail_edit = Some(DiscordError::NotFound);
        discord.fail = Some(DiscordError::Failed);
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::DiscordFailed));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!(menu.message, first, "the row still names the old message");
        assert_eq!(menu.channel, Some(CHANNEL));
    }

    // -----------------------------------------------------------------------
    // The channel of a published menu
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn posting_to_another_channel_is_refused_and_nothing_is_sent() {
        let store = FakeStore::new();
        let discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        post(&store, &discord, &settings(), &post_req("main")).await;
        let sends = discord.send_count();

        let mut req = post_req("main");
        req.channel = CHANNEL + 1;
        let outcome = post(&store, &discord, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::ChannelMismatch));
        assert_eq!(discord.send_count(), sends, "no message was sent");
        assert_eq!(discord.edit_count(), 0, "the old message was not edited");
        assert_eq!(
            store.menu(GUILD, "main").unwrap().channel,
            Some(CHANNEL),
            "the row still names the old channel"
        );
    }

    // -----------------------------------------------------------------------
    // The number of options
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_menu_may_not_hold_more_options_than_one_message_may_carry() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        seed_menu(&store, "big", &[]).await;
        let menu_id = store.menu(GUILD, "big").unwrap().id;
        // Fill the menu right up to the limit.
        for index in 0..MAX_OPTIONS_PER_MENU as u64 {
            store.options.lock().unwrap().push(OptionRow {
                menu_id,
                role: 950_000 + index,
                label: format!("Label {index}"),
                emoji: "🎉".to_string(),
                position: index as i64,
            });
        }
        let count = store.count_options(menu_id).await.unwrap();
        assert_eq!(count as usize, MAX_OPTIONS_PER_MENU);

        // The next option is refused: the menu may not need a sixth row.
        let outcome = add_option(&store, &facts, &settings(), &add_req("big", 960_000)).await;
        assert_eq!(outcome, Outcome::new(Code::TooManyOptions));
        assert_eq!(
            store.count_options(menu_id).await.unwrap() as usize,
            MAX_OPTIONS_PER_MENU,
            "the refused option was not stored"
        );

        // The menu still fits into one message: five components.
        let options = store.list_options(menu_id).await.unwrap();
        let chunks = split_chunks(&options);
        assert_eq!(chunks.len(), MAX_ROWS_PER_MESSAGE);
        assert!(chunks
            .iter()
            .all(|c| c.options.len() <= SELECT_OPTION_LIMIT));
    }

    #[tokio::test]
    async fn a_menu_below_the_limit_still_takes_an_option() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        facts.set(
            960_000,
            RoleFacts {
                id: 960_000,
                position: 5,
                managed: false,
                permissions: 0,
                exists: true,
            },
        );
        seed_menu(&store, "big", &[]).await;
        let menu_id = store.menu(GUILD, "big").unwrap().id;
        for index in 0..(MAX_OPTIONS_PER_MENU as u64 - 1) {
            store.options.lock().unwrap().push(OptionRow {
                menu_id,
                role: 950_000 + index,
                label: format!("Label {index}"),
                emoji: "🎉".to_string(),
                position: index as i64,
            });
        }
        let outcome = add_option(&store, &facts, &settings(), &add_req("big", 960_000)).await;
        assert_eq!(outcome, Outcome::new(Code::OptionAdded));
        assert_eq!(
            store.count_options(menu_id).await.unwrap() as usize,
            MAX_OPTIONS_PER_MENU
        );
    }

    // -----------------------------------------------------------------------
    // What a stored row says
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_maximum_number_of_options_is_stored_and_read_back() {
        let store = FakeStore::new();
        let outcome = create(
            &store,
            &settings(),
            &CreateRequest {
                invoker: staff(),
                guild: GUILD,
                key: "multi",
                mode: MenuMode::Multi,
                max_select: 3,
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::Created));
        let menu = store.menu(GUILD, "multi").unwrap();
        assert_eq!(menu.max_select, 3, "the maximum is stored");
        // The policy menu carries the maximum as well.
        let policy_menu = to_policy_menu(&menu, &[]).unwrap();
        assert_eq!(policy_menu.groups[0].max, Some(3));
    }

    #[tokio::test]
    async fn an_exclusive_menu_may_not_name_a_maximum() {
        let store = FakeStore::new();
        let outcome = create(
            &store,
            &settings(),
            &CreateRequest {
                invoker: staff(),
                guild: GUILD,
                key: "main",
                mode: MenuMode::Exclusive,
                max_select: 2,
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::InvalidMode));
        assert!(store.menus.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_stored_mode_is_refused_instead_of_guessed() {
        let store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        store.menus.lock().unwrap()[0].mode = "garbage".to_string();
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!(to_policy_menu(&menu, &[]), Err(Code::CorruptRow));
    }

    #[tokio::test]
    async fn a_label_and_an_emoji_are_stored_trimmed() {
        let store = FakeStore::new();
        let facts = FakeFacts::new();
        seed_menu(&store, "main", &[]).await;
        let mut req = add_req("main", ROLE_OK);
        req.label = "  Alpha  ";
        req.emoji = "  🎉  ";
        let outcome = add_option(&store, &facts, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::OptionAdded));
        let menu = store.menu(GUILD, "main").unwrap();
        let options = store.list_options(menu.id).await.unwrap();
        assert_eq!(options[0].label, "Alpha");
        assert_eq!(options[0].emoji, "🎉");
    }

    #[tokio::test]
    async fn a_posted_menu_is_listed_as_posted() {
        let store = FakeStore::new();
        let discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        post(&store, &discord, &settings(), &post_req("main")).await;
        let report = list(
            &store,
            &settings(),
            &ListRequest {
                invoker: staff(),
                guild: GUILD,
            },
        )
        .await;
        assert_eq!(report.outcome, Outcome::new(Code::Listed));
        assert_eq!(report.menus.len(), 1);
        assert!(report.menus[0].posted, "the menu was posted");
        assert_eq!(report.menus[0].options, 1);
    }

    #[tokio::test]
    async fn an_off_feature_serves_nothing() {
        let store = FakeStore::new();
        let mut s = settings();
        s.enabled = false;
        assert_eq!(gate(&s, &staff(), GUILD), Err(Outcome::new(Code::Disabled)));
        let outcome = create(&store, &s, &create_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::Disabled));
        assert!(store.calls().is_empty(), "nothing was read or written");
    }

    // -----------------------------------------------------------------------
    // The real database
    // -----------------------------------------------------------------------

    fn scratch_db(name: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("rm2-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("core.sqlite")
    }

    #[tokio::test]
    async fn the_commands_work_over_a_real_database() {
        let path = scratch_db("real");
        let store = SqlStore::open(&path);
        let facts = FakeFacts::new();
        let outcome = create(&store, &settings(), &create_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::Created));
        let outcome = add_option(&store, &facts, &settings(), &add_req("main", ROLE_OK)).await;
        assert_eq!(outcome, Outcome::new(Code::OptionAdded));

        // The same key is refused by the real unique key.
        let outcome = create(&store, &settings(), &create_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::KeyTaken));

        // The row survives a restart.
        drop(store);
        let store = SqlStore::open(&path);
        let report = list(
            &store,
            &settings(),
            &ListRequest {
                invoker: staff(),
                guild: GUILD,
            },
        )
        .await;
        assert_eq!(report.outcome, Outcome::new(Code::Listed));
        assert_eq!(report.menus.len(), 1);
        assert_eq!(report.menus[0].options, 1);
    }

    #[tokio::test]
    async fn a_seeded_sequence_over_the_real_database_always_settles() {
        let path = scratch_db("real-liveness");
        let store = SqlStore::open(&path);
        let mut discord = FakeDiscord::new();
        seed_menu_sql(&store, "main", &[ROLE_OK]).await;

        // A message that is deleted for good: every edit reports it as gone.
        discord.fail_edit = Some(DiscordError::NotFound);
        let mut rng = Lcg(0xDEAD_BEEF_2024);
        let mut published = false;
        let mut row_before: Option<(Option<u64>, Option<u64>)> = None;
        for _ in 0..80 {
            // The send fails on some steps: the row must not change then.
            discord.fail = if rng.next(4) == 0 {
                Some(DiscordError::Failed)
            } else {
                None
            };
            let before = store.get_menu(GUILD, "main").await.unwrap().unwrap();
            let before = (before.channel, before.message);
            let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
            match outcome.code {
                Code::Posted | Code::Reposted => {
                    published = true;
                    let menu = store.get_menu(GUILD, "main").await.unwrap().unwrap();
                    assert!(menu.message.is_some(), "the row names a message");
                    assert_eq!(menu.channel, Some(CHANNEL));
                    row_before = Some((menu.channel, menu.message));
                }
                Code::DiscordFailed => {
                    // A failed send never changes the row.
                    let menu = store.get_menu(GUILD, "main").await.unwrap().unwrap();
                    assert_eq!((menu.channel, menu.message), before, "the row is unchanged");
                    assert_eq!(row_before, Some(before), "the row never moved");
                }
                other => panic!("unexpected code {other:?}"),
            }
        }
        assert!(published, "the menu was never published");
        let menu = store.get_menu(GUILD, "main").await.unwrap().unwrap();
        assert!(menu.message.is_some(), "the menu ends published");
        assert_eq!(menu.channel, Some(CHANNEL));
    }

    /// A seeded sequence over a channel that is deleted for good.
    ///
    /// The channel never comes back: every call into it reports that it is
    /// gone. The sequence asserts that the command never pretends to have
    /// published the menu, that the row is never changed by the failed call,
    /// and that the reset path (`unpublish`) always brings the menu back to a
    /// published state in a channel that exists.
    #[tokio::test]
    async fn a_seeded_gone_channel_sequence_always_reaches_a_published_menu() {
        let path = scratch_db("gone-channel-liveness");
        let store = SqlStore::open(&path);
        let mut discord = FakeDiscord::new();
        seed_menu_sql(&store, "main", &[ROLE_OK]).await;
        assert_eq!(
            post(&store, &discord, &settings(), &post_req("main")).await,
            Outcome::new(Code::Posted)
        );

        let mut rng = Lcg(0x000A_11CE_5EED_0001);
        let mut recovered = 0;
        let mut published = 0;
        for _ in 0..40 {
            // The channel the menu currently lives in is deleted for good.
            let menu = store.get_menu(GUILD, "main").await.unwrap().unwrap();
            let live = menu.channel.unwrap();
            discord.gone_channel = Some(live);

            // The row before the call.
            let before = store.get_menu(GUILD, "main").await.unwrap().unwrap();
            let before = (before.channel, before.message);
            let mut req = post_req("main");
            req.channel = live;
            let outcome = post(&store, &discord, &settings(), &req).await;
            assert_eq!(
                outcome,
                Outcome::new(Code::ChannelGone),
                "a gone channel never publishes"
            );
            let after = store.get_menu(GUILD, "main").await.unwrap().unwrap();
            assert_eq!(
                (after.channel, after.message),
                before,
                "the row is unchanged by the failed call"
            );

            // Every few steps the staff reset the menu and post it again in a
            // channel that exists.
            if rng.next(3) == 0 {
                let outcome = unpublish(
                    &store,
                    &settings(),
                    &UnpublishRequest {
                        invoker: staff(),
                        guild: GUILD,
                        key: "main",
                    },
                )
                .await;
                assert_eq!(outcome, Outcome::new(Code::Unpublished));
                discord.gone_channel = None;
                let mut req = post_req("main");
                req.channel = live + 1;
                assert_eq!(
                    post(&store, &discord, &settings(), &req).await,
                    Outcome::new(Code::Posted)
                );
                let menu = store.get_menu(GUILD, "main").await.unwrap().unwrap();
                assert_eq!(menu.channel, Some(live + 1));
                assert!(menu.message.is_some(), "the menu is published again");
                recovered += 1;
                published += 1;
            }
        }
        assert!(recovered > 0, "the sequence never exercised the reset path");
        assert_eq!(published, recovered, "every reset published the menu");
    }

    /// Creates a menu with options over a store, without the command layer.
    async fn seed_menu_sql(store: &SqlStore, key: &str, roles: &[u64]) {
        let id = store.create_menu(GUILD, key, "exclusive", 0).await.unwrap();
        for role in roles {
            store.add_option(id, *role, "Label", "🎉").await.unwrap();
        }
    }

    // -----------------------------------------------------------------------
    // A deleted channel: the way out
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_deleted_channel_is_reported_and_the_row_is_left_alone() {
        let store = FakeStore::new();
        let mut discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        post(&store, &discord, &settings(), &post_req("main")).await;
        let before = store.menu(GUILD, "main").unwrap();

        // A moderator deleted the whole channel the menu lived in.
        discord.gone_channel = Some(CHANNEL);
        for _ in 0..25 {
            let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
            assert_eq!(
                outcome,
                Outcome::new(Code::ChannelGone),
                "a gone channel has its own code"
            );
        }
        let after = store.menu(GUILD, "main").unwrap();
        assert_eq!(after.channel, before.channel, "the row is unchanged");
        assert_eq!(after.message, before.message);
        assert_eq!(
            discord.send_count(),
            1,
            "no message was sent into the gone channel"
        );
    }

    #[tokio::test]
    async fn unpublish_clears_the_recorded_location_and_the_menu_is_posted_again() {
        let store = FakeStore::new();
        let mut discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        post(&store, &discord, &settings(), &post_req("main")).await;

        // The channel is gone for good: `post` cannot fix the row.
        discord.gone_channel = Some(CHANNEL);
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::ChannelGone));

        // The way out: unpublish clears the recorded location.
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: staff(),
                guild: GUILD,
                key: "main",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::Unpublished));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!(menu.channel, None, "the recorded channel is cleared");
        assert_eq!(menu.message, None);
        assert_eq!(menu.embed_key, None);
        // The menu and its options are untouched.
        assert_eq!(store.option_count(), 1, "the option is kept");
        assert_eq!(store.menu(GUILD, "main").unwrap().key, "main");

        // Now the menu can be posted into a channel that still exists.
        discord.gone_channel = None;
        let mut req = post_req("main");
        req.channel = CHANNEL + 1;
        let outcome = post(&store, &discord, &settings(), &req).await;
        assert_eq!(outcome, Outcome::new(Code::Posted));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!(menu.channel, Some(CHANNEL + 1), "the new place is recorded");
        assert!(menu.message.is_some());
    }

    #[tokio::test]
    async fn unpublish_of_a_missing_menu_is_refused_and_writes_nothing() {
        let store = FakeStore::new();
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: staff(),
                guild: GUILD,
                key: "ghost",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::MenuNotFound));
        assert!(
            !store
                .calls()
                .iter()
                .any(|c| c.starts_with("clear_published")),
            "nothing was written"
        );
        assert!(store.menus.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unpublish_by_a_non_staff_caller_is_denied_and_writes_nothing() {
        let store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        let before = store.calls().len();
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: member(),
                guild: GUILD,
                key: "main",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::Denied));
        assert_eq!(store.calls().len(), before, "nothing was read or written");
    }

    #[tokio::test]
    async fn unpublish_in_a_foreign_guild_does_nothing() {
        let store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        let before = store.calls().len();
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: staff(),
                guild: OTHER_GUILD,
                key: "main",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::OtherGuild));
        assert_eq!(store.calls().len(), before, "nothing was read or written");
    }

    #[tokio::test]
    async fn unpublish_refuses_a_hostile_key_and_writes_nothing() {
        let store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        let before = store.calls().len();
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: staff(),
                guild: GUILD,
                key: "Main Menu",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::InvalidKey));
        assert_eq!(store.calls().len(), before, "nothing was read or written");
    }

    #[tokio::test]
    async fn a_store_failure_during_unpublish_is_reported_with_a_stable_code() {
        let mut store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        store.fail = Some(RepoError::Storage(crate::storage::StorageError::Sqlite));
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: staff(),
                guild: GUILD,
                key: "main",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::with(Code::StoreFailed, "sqlite_error"));
        // The row still names the old place: the clear did not happen.
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!(menu.channel, None);
        assert_eq!(menu.message, None);
    }

    #[tokio::test]
    async fn a_menu_that_vanished_during_unpublish_is_reported_as_missing() {
        let store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        // The row disappears between the read and the clear.
        store.menus.lock().unwrap().clear();
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: staff(),
                guild: GUILD,
                key: "main",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::MenuNotFound));
    }

    #[tokio::test]
    async fn a_failure_of_the_clear_itself_is_reported_with_a_stable_code() {
        let mut store = FakeStore::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        // The row was read, but the clear itself fails.
        store.fail_clear = Some(RepoError::MenuMissing);
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: staff(),
                guild: GUILD,
                key: "main",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::MenuNotFound));

        // A storage failure is reported with the storage code.
        store.fail_clear = Some(RepoError::Storage(crate::storage::StorageError::Sqlite));
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: staff(),
                guild: GUILD,
                key: "main",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::with(Code::StoreFailed, "sqlite_error"));
    }

    #[tokio::test]
    async fn a_gone_channel_never_leaves_the_menu_unpublishable_over_the_real_database() {
        let path = scratch_db("gone-channel");
        let store = SqlStore::open(&path);
        let mut discord = FakeDiscord::new();
        seed_menu_sql(&store, "main", &[ROLE_OK]).await;
        assert_eq!(
            post(&store, &discord, &settings(), &post_req("main")).await,
            Outcome::new(Code::Posted)
        );

        // The channel is deleted for good: every call into it fails.
        discord.gone_channel = Some(CHANNEL);
        for _ in 0..25 {
            assert_eq!(
                post(&store, &discord, &settings(), &post_req("main")).await,
                Outcome::new(Code::ChannelGone)
            );
        }

        // The reset path works over the real database too.
        let outcome = unpublish(
            &store,
            &settings(),
            &UnpublishRequest {
                invoker: staff(),
                guild: GUILD,
                key: "main",
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::Unpublished));
        let menu = store.get_menu(GUILD, "main").await.unwrap().unwrap();
        assert_eq!(menu.channel, None);
        assert_eq!(menu.message, None);

        // And the menu is publishable again in a channel that exists.
        discord.gone_channel = None;
        let mut req = post_req("main");
        req.channel = CHANNEL + 1;
        assert_eq!(
            post(&store, &discord, &settings(), &req).await,
            Outcome::new(Code::Posted)
        );
        let menu = store.get_menu(GUILD, "main").await.unwrap().unwrap();
        assert_eq!(menu.channel, Some(CHANNEL + 1));
        assert!(menu.message.is_some());
    }

    // -----------------------------------------------------------------------
    // A label may not look empty
    // -----------------------------------------------------------------------

    #[test]
    fn a_label_made_only_of_invisible_characters_is_refused() {
        // The blank fillers and the default-ignorable marks: a label made
        // only of them shows the reader nothing.
        for invisible in [
            "\u{3164}", // HANGUL FILLER
            "\u{115F}", // HANGUL CHOSEONG FILLER
            "\u{1160}", // HANGUL JUNGSEONG FILLER
            "\u{2800}", // BRAILLE PATTERN BLANK
            "\u{034F}", // COMBINING GRAPHEME JOINER
            "\u{FFA0}", // HALFWIDTH HANGUL FILLER
            "\u{17B4}", // KHMER VOWEL INHERENT AQ
            "\u{17B5}", // KHMER VOWEL INHERENT AA
            "\u{180B}", // MONGOLIAN FREE VARIATION SELECTOR ONE
            "\u{FE0F}", // VARIATION SELECTOR-16
        ] {
            assert_eq!(
                validate_label(invisible),
                Err(Code::InvalidLabel),
                "{invisible:?}"
            );
        }
        // A mix of fillers is refused as well.
        assert_eq!(validate_label("\u{3164}\u{2800}"), Err(Code::InvalidLabel));
        // A filler inside a label is refused too: the whole class is refused.
        assert_eq!(validate_label("A\u{3164}"), Err(Code::InvalidLabel));
        // An ordinary label with visible characters is kept.
        assert_eq!(validate_label("Alpha"), Ok(()));
    }

    // -----------------------------------------------------------------------
    // An emoji is exactly one emoji
    // -----------------------------------------------------------------------

    #[test]
    fn an_emoji_must_be_exactly_one_emoji() {
        // One emoji is accepted.
        assert_eq!(validate_emoji("🎉"), Ok(()));
        assert_eq!(validate_emoji("🎉\u{FE0F}"), Ok(()));
        assert_eq!(validate_emoji("👍🏽"), Ok(()));
        assert_eq!(validate_emoji("👨‍👩‍👧"), Ok(()));
        assert_eq!(validate_emoji("🇺🇸"), Ok(()));
        assert_eq!(validate_emoji("1️⃣"), Ok(()));
        // Two emoji in one string are refused.
        assert_eq!(validate_emoji("🎉🎉"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("⬛⬛"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("🎉🎉🎉🎉"), Err(Code::InvalidEmoji));
        // A skin tone without a base is refused.
        assert_eq!(validate_emoji("🏻🎉"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("🏻"), Err(Code::InvalidEmoji));
        // An emoji with no base at all is refused.
        assert_eq!(validate_emoji("\u{FE0F}"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("\u{20E3}"), Err(Code::InvalidEmoji));
        // A lone regional indicator is refused: a flag is a pair.
        assert_eq!(validate_emoji("\u{1F1FA}"), Err(Code::InvalidEmoji));
        // Several variation selectors after one base are refused.
        assert_eq!(
            validate_emoji("🎉\u{FE0F}\u{FE0F}"),
            Err(Code::InvalidEmoji)
        );
    }

    // -----------------------------------------------------------------------
    // Known limits of the one-emoji check
    // -----------------------------------------------------------------------

    #[test]
    fn the_emoji_check_names_its_limits_by_refusing_rather_than_guessing() {
        // Plain `#` and `*` keycaps and tag-sequence flags are not accepted:
        // the doc names these as limits, and they fail closed.
        assert_eq!(validate_emoji("#\u{FE0F}\u{20E3}"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("*\u{FE0F}\u{20E3}"), Err(Code::InvalidEmoji));
        assert_eq!(
            validate_emoji("\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}"),
            Err(Code::InvalidEmoji)
        );
    }

    // -----------------------------------------------------------------------
    // The record write may fail
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_failed_record_write_leaves_the_row_unchanged_and_the_next_post_publishes() {
        let mut store = FakeStore::new();
        let discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;

        // The message is sent, but the row cannot be written.
        store.fail_publish = Some(RepoError::Storage(crate::storage::StorageError::Sqlite));
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::with(Code::StoreFailed, "sqlite_error"));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!(menu.message, None, "the row is unchanged");
        assert_eq!(menu.channel, None);
        assert_eq!(menu.embed_key, None);
        assert_eq!(discord.send_count(), 1, "the message was sent");

        // The store works again: the next post publishes the menu.
        store.fail_publish = None;
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::Posted));
        let menu = store.menu(GUILD, "main").unwrap();
        assert!(menu.message.is_some(), "the row names the new message");
        assert_eq!(menu.channel, Some(CHANNEL));
        assert_eq!(
            discord.send_count(),
            2,
            "the first message was orphaned by the failed write"
        );
    }

    // -----------------------------------------------------------------------
    // The seeded sequence drives its faults from the random values
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_persistent_deleted_message_always_reaches_a_published_menu() {
        let store = FakeStore::new();
        let mut discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        // The edit always reports that the message is gone: the recovery must
        // still reach a published menu, not give up.
        discord.fail_edit = Some(DiscordError::NotFound);

        let mut rng = Lcg(0xC0FF_EE00_1234);
        let mut published = false;
        let mut last_published = (None, None);
        for _ in 0..60 {
            // The send itself fails on some steps: the row must not change then.
            discord.fail = if rng.next(4) == 0 {
                Some(DiscordError::Failed)
            } else {
                None
            };
            let before = store.menu(GUILD, "main").unwrap();
            let before = (before.channel, before.message);
            let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
            match outcome.code {
                Code::Posted | Code::Reposted => {
                    published = true;
                    let menu = store.menu(GUILD, "main").unwrap();
                    let sent = discord.sends.lock().unwrap().last().unwrap().0;
                    assert_eq!(menu.channel, Some(sent));
                    assert!(menu.message.is_some());
                    last_published = (menu.channel, menu.message);
                }
                Code::DiscordFailed => {
                    // A failed send never changes the row.
                    let menu = store.menu(GUILD, "main").unwrap();
                    assert_eq!((menu.channel, menu.message), before, "the row is unchanged");
                    assert_eq!(last_published, before, "the row never moved");
                }
                other => panic!("unexpected code {other:?}"),
            }
        }
        assert!(published, "the menu was never published");
        let menu = store.menu(GUILD, "main").unwrap();
        assert!(menu.message.is_some(), "the menu ends published");
        assert_eq!(menu.channel, Some(CHANNEL));
    }

    #[tokio::test]
    async fn a_seeded_sequence_with_send_edit_and_record_faults_always_settles() {
        let mut store = FakeStore::new();
        let mut discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;

        let mut rng = Lcg(0x5EED_0000_FA17);
        let mut record_failures = 0;
        let mut send_failures = 0;
        let mut published = 0;
        for _ in 0..120 {
            // Now and then the staff reset the menu, so the plain send path
            // (not only the edit and repost paths) meets the record fault.
            if rng.next(5) == 0 {
                let reset = UnpublishRequest {
                    invoker: staff(),
                    guild: GUILD,
                    key: "main",
                };
                store.fail_publish = None;
                assert_eq!(
                    unpublish(&store, &settings(), &reset).await,
                    Outcome::new(Code::Unpublished)
                );
            }
            // Each step draws its own faults: a failed send, a deleted message
            // (the edit says NotFound) and a record write that fails after a
            // good send.
            discord.fail = (rng.next(4) == 0).then_some(DiscordError::Failed);
            discord.fail_edit = (rng.next(3) == 0).then_some(DiscordError::NotFound);
            store.fail_publish = (rng.next(4) == 0)
                .then_some(RepoError::Storage(crate::storage::StorageError::Sqlite));
            let before = store.menu(GUILD, "main").unwrap();
            let before = (before.channel, before.message);
            let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
            let after = store.menu(GUILD, "main").unwrap();
            let after = (after.channel, after.message);
            match outcome.code {
                Code::StoreFailed => {
                    record_failures += 1;
                    assert_eq!(
                        after, before,
                        "a failed record write leaves the row as it was"
                    );
                }
                Code::DiscordFailed => {
                    send_failures += 1;
                    assert_eq!(
                        after, before,
                        "a failed Discord call leaves the row as it was"
                    );
                }
                Code::Posted | Code::Reposted => {
                    published += 1;
                    assert_eq!(after.0, Some(CHANNEL));
                    assert!(after.1.is_some(), "a published menu names its message");
                    assert_ne!(after.1, before.1, "a send is recorded as the new message");
                }
                Code::Edited => {
                    published += 1;
                    assert_eq!(after, before, "an edit keeps the same message");
                    assert!(after.1.is_some());
                }
                other => panic!("unexpected code {other:?}"),
            }
        }
        assert!(
            record_failures > 0,
            "the sequence never failed a record write"
        );
        assert!(
            send_failures > 0,
            "the sequence never failed a Discord call"
        );
        assert!(published > 0, "the sequence never published");

        // Once the faults stop, the very next post reaches a published menu and
        // the one after that edits the same message instead of sending another.
        discord.fail = None;
        discord.fail_edit = None;
        store.fail_publish = None;
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert!(
            matches!(outcome.code, Code::Posted | Code::Reposted | Code::Edited),
            "{outcome:?}"
        );
        let settled = store.menu(GUILD, "main").unwrap();
        assert!(settled.message.is_some());
        let sends = discord.send_count();
        let outcome = post(&store, &discord, &settings(), &post_req("main")).await;
        assert_eq!(outcome, Outcome::new(Code::Edited));
        assert_eq!(
            discord.send_count(),
            sends,
            "an idempotent post sends nothing"
        );
        assert_eq!(store.menu(GUILD, "main").unwrap().message, settled.message);
    }

    // -----------------------------------------------------------------------
    // The one-emoji walker: each part of the grammar has its own test
    // -----------------------------------------------------------------------

    #[test]
    fn a_keycap_mark_is_allowed_only_after_a_digit_and_ends_the_emoji() {
        for good in [
            "1\u{20E3}",
            "1\u{FE0F}\u{20E3}",
            "9\u{FE0F}\u{20E3}",
            "0\u{20E3}",
        ] {
            assert_eq!(validate_emoji(good), Ok(()), "{good:?}");
        }
        for bad in [
            // The keycap mark after a base that is not a digit.
            "\u{1F389}\u{20E3}",
            "\u{00A9}\u{20E3}",
            "\u{1F389}\u{FE0F}\u{20E3}",
            // The mark only follows a digit: not a letter or #, with or without a selector.
            "a\u{20E3}",
            "a\u{FE0F}\u{20E3}",
            "A\u{20E3}",
            "#\u{20E3}",
            "*\u{20E3}",
            // The mark may not be doubled or repeated.
            "1\u{20E3}\u{20E3}",
            "1\u{FE0F}\u{20E3}\u{FE0F}",
            // Anything after the keycap is a second part, not one emoji.
            "1\u{FE0F}\u{20E3}x",
            "1\u{FE0F}\u{20E3}\u{1F389}",
            "1\u{FE0F}\u{20E3}1\u{FE0F}\u{20E3}",
            "1\u{FE0F}\u{20E3}\u{200D}\u{1F389}",
            // A digit alone, or with a skin tone, is not a keycap.
            "1",
            "1\u{1F3FB}",
        ] {
            assert_eq!(validate_emoji(bad), Err(Code::InvalidEmoji), "{bad:?}");
        }
    }

    #[test]
    fn a_zwj_must_be_followed_by_a_base_and_a_joined_sequence_is_one_emoji() {
        // Valid: the family and the skin-toned sequences.
        for good in [
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}",
            // person, skin tone, joiner, laptop
            "\u{1F9D1}\u{1F3FB}\u{200D}\u{1F4BB}",
            // woman raising hand, skin tone, joiner, female sign, variation selector
            "\u{1F64B}\u{1F3FD}\u{200D}\u{2640}\u{FE0F}",
            // heart on fire style: base, selector, joiner, base
            "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
        ] {
            assert_eq!(validate_emoji(good), Ok(()), "{good:?}");
        }
        for bad in [
            // A joiner at the end.
            "\u{1F389}\u{200D}",
            // A joiner followed by something that is not a base.
            "\u{1F389}\u{200D}x",
            "\u{1F389}\u{200D}1",
            "\u{1F389}\u{200D}\u{1F3FB}",
            "\u{1F389}\u{200D}\u{FE0F}",
            "\u{1F389}\u{200D}\u{20E3}",
            "\u{1F389}\u{200D}\u{1F1FA}",
            // Two joiners in a row.
            "\u{1F389}\u{200D}\u{200D}\u{1F389}",
            // A joiner first.
            "\u{200D}\u{1F389}",
        ] {
            assert_eq!(validate_emoji(bad), Err(Code::InvalidEmoji), "{bad:?}");
        }
    }

    #[test]
    fn the_number_of_zwj_joins_is_bounded() {
        // Raw literals, not built from MAX_ZWJ_JOINS, so moving the bound fails here.
        let party = "\u{1F389}";
        let zwj = "\u{200D}";
        let four = [party, party, party, party, party].join(zwj);
        let five = [party, party, party, party, party, party].join(zwj);
        assert_eq!(four.matches(zwj).count(), 4);
        assert_eq!(five.matches(zwj).count(), 5);
        assert_eq!(validate_emoji(&four), Ok(()));
        assert_eq!(validate_emoji(&five), Err(Code::InvalidEmoji));
        assert_eq!(
            validate_emoji(&[party; 9].join(zwj)),
            Err(Code::InvalidEmoji)
        );
        // Real sequences with 3 joins stay accepted.
        assert_eq!(
            validate_emoji("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}"),
            Ok(())
        );
        assert_eq!(validate_emoji("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}"), Ok(()));
    }

    #[test]
    fn a_regional_indicator_is_one_half_of_a_pair_and_nothing_else() {
        assert_eq!(validate_emoji("\u{1F1FA}\u{1F1F8}"), Ok(()));
        for bad in [
            // The second half is not a regional indicator.
            "\u{1F1FA}x",
            "\u{1F1FA}\u{1F389}",
            "\u{1F1FA}\u{FE0F}",
            "\u{1F1FA}\u{200D}\u{1F1F8}",
            "\u{1F1FA}\u{1F3FB}",
            // The first half is not a regional indicator.
            "\u{1F389}\u{1F1F8}",
            "x\u{1F1F8}",
            // A third indicator, or a second pair.
            "\u{1F1FA}\u{1F1F8}\u{1F1FA}",
            "\u{1F1FA}\u{1F1F8}\u{1F1FA}\u{1F1F8}",
            // A pair with a trailing selector or joiner.
            "\u{1F1FA}\u{1F1F8}\u{FE0F}",
            "\u{1F1FA}\u{1F1F8}\u{200D}\u{1F389}",
        ] {
            assert_eq!(validate_emoji(bad), Err(Code::InvalidEmoji), "{bad:?}");
        }
    }

    #[test]
    fn a_skin_tone_follows_its_base_and_is_not_followed_by_another_base() {
        for good in [
            "\u{1F44D}\u{1F3FD}",
            "\u{1F44D}\u{FE0F}\u{1F3FD}",
            "\u{1F44D}\u{1F3FD}\u{FE0F}",
        ] {
            assert_eq!(validate_emoji(good), Ok(()), "{good:?}");
        }
        for bad in [
            // A skin tone first.
            "\u{1F3FD}\u{1F44D}",
            // Another base straight after the skin tone, with no joiner.
            "\u{1F44D}\u{1F3FD}\u{1F44D}",
            "\u{1F64B}\u{1F3FD}\u{1F389}",
            // Three joiner-less parts (odd lengths): each junction must be a joiner.
            "\u{1F389}\u{1F389}\u{1F389}",
            "\u{1F44D}\u{1F3FD}\u{1F389}\u{1F389}",
            // Two skin tones.
            "\u{1F44D}\u{1F3FD}\u{1F3FD}",
            "\u{1F44D}\u{1F3FB}\u{1F3FF}",
            // A skin tone then a plain character.
            "\u{1F44D}\u{1F3FD}x",
            // Two selectors around a skin tone.
            "\u{1F44D}\u{FE0F}\u{1F3FD}\u{FE0F}",
        ] {
            assert_eq!(validate_emoji(bad), Err(Code::InvalidEmoji), "{bad:?}");
        }
    }

    // -----------------------------------------------------------------------
    // /rolemenu edit
    // -----------------------------------------------------------------------

    fn edit_req(key: &str, mode: Option<MenuMode>, max_select: Option<u32>) -> EditRequest<'_> {
        EditRequest {
            invoker: staff(),
            guild: GUILD,
            key,
            mode,
            max_select,
        }
    }

    async fn seed_multi(store: &FakeStore, key: &str) {
        let outcome = create(
            store,
            &settings(),
            &CreateRequest {
                invoker: staff(),
                guild: GUILD,
                key,
                mode: MenuMode::Multi,
                max_select: 3,
            },
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::Created));
    }

    #[tokio::test]
    async fn edit_changes_the_mode_and_the_maximum_and_nothing_else() {
        let store = FakeStore::new();
        let discord = FakeDiscord::new();
        seed_menu(&store, "main", &[ROLE_OK]).await;
        assert_eq!(
            post(&store, &discord, &settings(), &post_req("main")).await,
            Outcome::new(Code::Posted)
        );
        let before = store.menu(GUILD, "main").unwrap();

        let outcome = edit(
            &store,
            &settings(),
            &edit_req("main", Some(MenuMode::Multi), Some(2)),
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::MenuUpdated));
        let after = store.menu(GUILD, "main").unwrap();
        assert_eq!(after.mode, "multi");
        assert_eq!(after.max_select, 2);
        // The key, the guild, the published location and the options stay.
        assert_eq!(after.id, before.id);
        assert_eq!(after.key, before.key);
        assert_eq!(after.guild, before.guild);
        assert_eq!(after.channel, before.channel);
        assert_eq!(after.message, before.message);
        assert_eq!(after.embed_key, before.embed_key);
        assert_eq!(store.option_count(), 1);
        // The edit sent and edited nothing on Discord.
        assert_eq!(discord.send_count(), 1);
        assert_eq!(discord.edit_count(), 0);
    }

    #[tokio::test]
    async fn edit_keeps_what_it_is_not_given() {
        let store = FakeStore::new();
        seed_multi(&store, "main").await;

        // Only the maximum: the mode stays.
        let outcome = edit(&store, &settings(), &edit_req("main", None, Some(5))).await;
        assert_eq!(outcome, Outcome::new(Code::MenuUpdated));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!((menu.mode.as_str(), menu.max_select), ("multi", 5));

        // Only the mode, to multi again: the maximum stays.
        let outcome = edit(
            &store,
            &settings(),
            &edit_req("main", Some(MenuMode::Multi), None),
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::MenuUpdated));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!((menu.mode.as_str(), menu.max_select), ("multi", 5));
    }

    #[tokio::test]
    async fn edit_refuses_an_exclusive_menu_with_a_maximum_and_leaves_the_row() {
        let store = FakeStore::new();
        seed_multi(&store, "main").await;

        // Exclusive plus a maximum.
        for (mode, max) in [
            (Some(MenuMode::Exclusive), Some(2)),
            // Exclusive alone, while the stored maximum is 3.
            (Some(MenuMode::Exclusive), None),
        ] {
            let outcome = edit(&store, &settings(), &edit_req("main", mode, max)).await;
            assert_eq!(outcome, Outcome::new(Code::InvalidMode), "{mode:?} {max:?}");
            let menu = store.menu(GUILD, "main").unwrap();
            assert_eq!((menu.mode.as_str(), menu.max_select), ("multi", 3));
        }
        assert!(!store.calls().iter().any(|c| c.starts_with("update_menu")));

        // Exclusive with an explicit maximum of zero is the valid spelling.
        let outcome = edit(
            &store,
            &settings(),
            &edit_req("main", Some(MenuMode::Exclusive), Some(0)),
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::MenuUpdated));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!((menu.mode.as_str(), menu.max_select), ("exclusive", 0));

        // A maximum alone on an exclusive menu is refused too.
        let outcome = edit(&store, &settings(), &edit_req("main", None, Some(1))).await;
        assert_eq!(outcome, Outcome::new(Code::InvalidMode));
        assert_eq!(store.menu(GUILD, "main").unwrap().max_select, 0);
    }

    #[tokio::test]
    async fn edit_and_create_bound_the_maximum_by_what_one_message_can_hold() {
        let store = FakeStore::new();
        seed_multi(&store, "main").await;
        let over = u32::try_from(MAX_OPTIONS_PER_MENU).unwrap() + 1;
        let at = u32::try_from(MAX_OPTIONS_PER_MENU).unwrap();

        let outcome = edit(
            &store,
            &settings(),
            &edit_req("main", Some(MenuMode::Multi), Some(over)),
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::InvalidMode));
        assert_eq!(store.menu(GUILD, "main").unwrap().max_select, 3);
        let outcome = edit(&store, &settings(), &edit_req("main", None, Some(u32::MAX))).await;
        assert_eq!(outcome, Outcome::new(Code::InvalidMode));
        let outcome = edit(&store, &settings(), &edit_req("main", None, Some(at))).await;
        assert_eq!(outcome, Outcome::new(Code::MenuUpdated));
        assert_eq!(store.menu(GUILD, "main").unwrap().max_select, at);

        // The same bound applies to create.
        let make = |key, max_select| CreateRequest {
            invoker: staff(),
            guild: GUILD,
            key,
            mode: MenuMode::Multi,
            max_select,
        };
        assert_eq!(
            create(&store, &settings(), &make("big", over)).await,
            Outcome::new(Code::InvalidMode)
        );
        assert!(store.menu(GUILD, "big").is_none());
        assert_eq!(
            create(&store, &settings(), &make("fits", at)).await,
            Outcome::new(Code::Created)
        );
    }

    #[tokio::test]
    async fn edit_with_nothing_to_change_is_refused_and_writes_nothing() {
        let store = FakeStore::new();
        seed_multi(&store, "main").await;
        let outcome = edit(&store, &settings(), &edit_req("main", None, None)).await;
        assert_eq!(
            outcome,
            Outcome::with(Code::InvalidMode, "nothing_to_change")
        );
        assert!(!store.calls().iter().any(|c| c.starts_with("update_menu")));
    }

    #[tokio::test]
    async fn edit_requires_the_gate_and_a_valid_key_and_an_existing_menu() {
        let store = FakeStore::new();
        seed_multi(&store, "main").await;
        let change = Some(MenuMode::Multi);
        let calls_before = store.calls().len();

        // A member who is not staff.
        let mut req = edit_req("main", change, Some(1));
        req.invoker = member();
        assert_eq!(
            edit(&store, &settings(), &req).await,
            Outcome::new(Code::Denied)
        );
        // The bot without Manage Roles.
        let mut no_manage = settings();
        no_manage.bot_has_manage_roles = false;
        assert_eq!(
            edit(&store, &no_manage, &edit_req("main", change, Some(1))).await,
            Outcome::new(Code::BotMissingManageRoles)
        );
        // The feature off.
        let mut off = settings();
        off.enabled = false;
        assert_eq!(
            edit(&store, &off, &edit_req("main", change, Some(1))).await,
            Outcome::new(Code::Disabled)
        );
        // An unset home guild fails closed.
        let mut no_home = settings();
        no_home.home_guild = None;
        assert_eq!(
            edit(&store, &no_home, &edit_req("main", change, Some(1))).await,
            Outcome::new(Code::NoHomeGuild)
        );
        // A foreign guild reads and writes nothing.
        let mut foreign = edit_req("main", change, Some(1));
        foreign.guild = OTHER_GUILD;
        assert_eq!(
            edit(&store, &settings(), &foreign).await,
            Outcome::new(Code::OtherGuild)
        );
        assert_eq!(
            store.calls().len(),
            calls_before,
            "none of the refusals touched the store"
        );
        assert_eq!(store.menu(GUILD, "main").unwrap().max_select, 3);

        // A bad key never reaches the store.
        for key in ["", "Main", "../x", "a b"] {
            assert_eq!(
                edit(&store, &settings(), &edit_req(key, change, Some(1))).await,
                Outcome::new(Code::InvalidKey),
                "{key:?}"
            );
        }
        assert_eq!(store.calls().len(), calls_before);

        // An unknown key.
        assert_eq!(
            edit(&store, &settings(), &edit_req("nope", change, Some(1))).await,
            Outcome::new(Code::MenuNotFound)
        );
        // The guild owner may edit as well.
        let mut by_owner = edit_req("main", change, Some(1));
        by_owner.invoker = owner();
        assert_eq!(
            edit(&store, &settings(), &by_owner).await,
            Outcome::new(Code::MenuUpdated)
        );
        assert_eq!(store.menu(GUILD, "main").unwrap().max_select, 1);
    }

    #[tokio::test]
    async fn edit_never_reaches_a_menu_of_another_guild() {
        let store = FakeStore::new();
        seed_multi(&store, "main").await;
        // The same key in a foreign guild's namespace is not found.
        store.menus.lock().unwrap()[0].guild = OTHER_GUILD;
        let outcome = edit(&store, &settings(), &edit_req("main", None, Some(1))).await;
        assert_eq!(outcome, Outcome::new(Code::MenuNotFound));
        assert!(!store.calls().iter().any(|c| c.starts_with("update_menu")));
    }

    #[tokio::test]
    async fn edit_reports_store_failures_and_a_corrupt_row_and_leaves_the_row() {
        // The write fails.
        let mut store = FakeStore::new();
        seed_multi(&store, "main").await;
        store.fail_update = Some(RepoError::Storage(crate::storage::StorageError::Sqlite));
        let outcome = edit(&store, &settings(), &edit_req("main", None, Some(1))).await;
        assert_eq!(outcome, Outcome::with(Code::StoreFailed, "sqlite_error"));
        assert_eq!(store.menu(GUILD, "main").unwrap().max_select, 3);

        // The menu vanished between the read and the write.
        store.fail_update = Some(RepoError::MenuMissing);
        let outcome = edit(&store, &settings(), &edit_req("main", None, Some(1))).await;
        assert_eq!(outcome, Outcome::new(Code::MenuNotFound));

        // The read fails.
        store.fail_update = None;
        store.fail = Some(RepoError::Storage(crate::storage::StorageError::Sqlite));
        let outcome = edit(&store, &settings(), &edit_req("main", None, Some(1))).await;
        assert_eq!(outcome, Outcome::with(Code::StoreFailed, "sqlite_error"));
        store.fail = None;

        // A stored mode the reader cannot understand is not guessed.
        store.menus.lock().unwrap()[0].mode = "garbage".to_string();
        let outcome = edit(&store, &settings(), &edit_req("main", None, Some(1))).await;
        assert_eq!(outcome, Outcome::new(Code::CorruptRow));
        let menu = store.menu(GUILD, "main").unwrap();
        assert_eq!((menu.mode.as_str(), menu.max_select), ("garbage", 3));
        // An explicit valid mode repairs it.
        let outcome = edit(
            &store,
            &settings(),
            &edit_req("main", Some(MenuMode::Multi), Some(1)),
        )
        .await;
        assert_eq!(outcome, Outcome::new(Code::MenuUpdated));
        assert_eq!(store.menu(GUILD, "main").unwrap().mode, "multi");
    }

    #[tokio::test]
    async fn an_edit_over_real_sqlite_survives_a_reopen_and_keeps_options_and_location() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("rm2-coder1-tests")
            .join("edit-reopen");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.sqlite");
        let discord = FakeDiscord::new();
        {
            let store = SqlStore::open(&path);
            seed_menu_sql(&store, "main", &[ROLE_OK]).await;
            assert_eq!(
                post(&store, &discord, &settings(), &post_req("main")).await,
                Outcome::new(Code::Posted)
            );
            let outcome = edit(
                &store,
                &settings(),
                &edit_req("main", Some(MenuMode::Multi), Some(2)),
            )
            .await;
            assert_eq!(outcome, Outcome::new(Code::MenuUpdated));
            // A refused edit does not change the row.
            let outcome = edit(
                &store,
                &settings(),
                &edit_req("main", Some(MenuMode::Exclusive), Some(2)),
            )
            .await;
            assert_eq!(outcome, Outcome::new(Code::InvalidMode));
        }
        let store = SqlStore::open(&path);
        let menu = store.get_menu(GUILD, "main").await.unwrap().unwrap();
        assert_eq!((menu.mode.as_str(), menu.max_select), ("multi", 2));
        assert_eq!(menu.channel, Some(CHANNEL));
        assert!(menu.message.is_some());
        assert_eq!(store.count_options(menu.id).await.unwrap(), 1);
        // And the menu is still idempotently posted.
        assert_eq!(
            post(&store, &discord, &settings(), &post_req("main")).await,
            Outcome::new(Code::Edited)
        );
        assert_eq!(discord.send_count(), 1);
    }

    #[test]
    fn edit_has_a_stable_code_and_an_ephemeral_reply() {
        assert_eq!(Code::MenuUpdated.as_str(), "menu_updated");
        let reply = reply_for(Outcome::new(Code::MenuUpdated));
        assert_eq!(reply.text(), "rolemenu menu_updated");
        assert!(reply.ephemeral());
    }

    #[tokio::test]
    async fn a_seeded_sequence_of_edits_and_faults_never_leaves_an_invalid_row() {
        let mut store = FakeStore::new();
        seed_multi(&store, "main").await;
        let mut rng = Lcg(0xED17_5EED_0001);
        let mut updated = 0;
        let mut refused = 0;
        let mut failed = 0;
        for _ in 0..200 {
            store.fail_update = (rng.next(4) == 0)
                .then_some(RepoError::Storage(crate::storage::StorageError::Sqlite));
            let mode = match rng.next(3) {
                0 => None,
                1 => Some(MenuMode::Exclusive),
                _ => Some(MenuMode::Multi),
            };
            let max = match rng.next(3) {
                0 => None,
                1 => Some(0),
                _ => Some(u32::try_from(rng.next(6)).unwrap()),
            };
            let before = store.menu(GUILD, "main").unwrap();
            let outcome = edit(&store, &settings(), &edit_req("main", mode, max)).await;
            let after = store.menu(GUILD, "main").unwrap();
            match outcome.code {
                Code::MenuUpdated => {
                    updated += 1;
                    assert_eq!(
                        after.mode,
                        mode.map_or(before.mode.clone(), |m| m.as_str().to_string())
                    );
                    assert_eq!(after.max_select, max.unwrap_or(before.max_select));
                }
                Code::InvalidMode => {
                    refused += 1;
                    assert_eq!(
                        (&after.mode, after.max_select),
                        (&before.mode, before.max_select)
                    );
                }
                Code::StoreFailed => {
                    failed += 1;
                    assert_eq!(
                        (&after.mode, after.max_select),
                        (&before.mode, before.max_select)
                    );
                }
                other => panic!("unexpected code {other:?}"),
            }
            // Whatever happened, the row is one the policy can read, and an
            // exclusive row never names a maximum.
            assert!(to_policy_menu(&after, &[]).is_ok());
            assert!(after.mode != "exclusive" || after.max_select == 0);
        }
        assert!(
            updated > 0 && refused > 0 && failed > 0,
            "{updated} {refused} {failed}"
        );
    }
}

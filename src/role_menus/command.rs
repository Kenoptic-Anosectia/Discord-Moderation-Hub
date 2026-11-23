//! `/rolemenu create | option | post | edit | list` logic (RM2).
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
    /// The label is empty, too long or holds a control character.
    InvalidLabel,
    /// The emoji is empty, too long or holds a control character.
    InvalidEmoji,
    /// The menu has no options yet.
    EmptyMenu,
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
    /// The menu was listed.
    Listed,
    /// The store could not be read or written.
    StoreFailed,
    /// Discord refused or failed the call.
    DiscordFailed,
    /// Discord refused the call (the bot may not do this).
    DiscordForbidden,
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
            Code::Created => "created",
            Code::OptionAdded => "option_added",
            Code::OptionRemoved => "option_removed",
            Code::Posted => "posted",
            Code::Edited => "edited",
            Code::Listed => "listed",
            Code::StoreFailed => "store_failed",
            Code::DiscordFailed => "discord_failed",
            Code::DiscordForbidden => "discord_forbidden",
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
    /// Records where a menu was published.
    async fn set_published(
        &self,
        menu_id: i64,
        channel: u64,
        message: u64,
        embed_key: Option<&str>,
    ) -> Result<(), RepoError>;
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
    /// Anything else (transport error, missing channel, ...).
    Failed,
}

/// The calls the command needs to publish a menu.
#[async_trait]
pub trait MenuDiscord: Send + Sync {
    /// Sends a new menu message and returns its id.
    async fn send_menu(&self, channel: u64, chunks: &[RenderedChunk]) -> Result<u64, DiscordError>;
    /// Edits an existing menu message in place.
    async fn edit_menu(&self, message: u64, chunks: &[RenderedChunk]) -> Result<(), DiscordError>;
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

/// Checks that a label is usable: not empty, not too long and free of control
/// characters other than a newline.
pub fn validate_label(label: &str) -> Result<(), Code> {
    let trimmed = label.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_LABEL_CHARS {
        return Err(Code::InvalidLabel);
    }
    if trimmed.chars().any(|c| c.is_control() && c != '\n') {
        return Err(Code::InvalidLabel);
    }
    Ok(())
}

/// Checks that an emoji is usable: not empty, not too long and free of control
/// characters.
///
/// The control-character check runs on the raw input, before trimming, so a
/// newline or a NUL hidden at the end of the input is refused instead of being
/// trimmed away.
pub fn validate_emoji(emoji: &str) -> Result<(), Code> {
    if emoji.chars().any(char::is_control) {
        return Err(Code::InvalidEmoji);
    }
    let trimmed = emoji.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_EMOJI_CHARS {
        return Err(Code::InvalidEmoji);
    }
    Ok(())
}

/// Splits the options of a menu into the chunks of one message.
///
/// A chunk holds at most [`SELECT_OPTION_LIMIT`] options, in the stored order,
/// and no option is dropped: a menu with more options than fit on one message
/// produces more chunks. The split is deterministic.
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
pub fn to_policy_menu(menu: &MenuRow, options: &[OptionRow]) -> policy::Menu {
    let mode = match MenuMode::parse(&menu.mode) {
        Some(MenuMode::Exclusive) => GroupMode::Exclusive,
        _ => GroupMode::Multi,
    };
    policy::Menu {
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
    }
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

/// `/rolemenu create <key>`.
///
/// Creates the menu row. The key is validated first, and a key that is
/// already taken is refused.
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
    match store
        .create_menu(req.guild, req.key, req.mode.as_str(), req.max_select)
        .await
    {
        Ok(_) => Outcome::new(Code::Created),
        Err(RepoError::KeyTaken) => Outcome::new(Code::KeyTaken),
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

/// `/rolemenu post <key>` and `/rolemenu edit <key>`.
///
/// Posts the managed embed of the menu, or edits the message the row already
/// names. The row is the truth: a menu that was already posted is edited, not
/// posted again. A failed send leaves the row unchanged.
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
    let already_posted = menu.message.is_some() && menu.channel.is_some();
    let outcome = if already_posted {
        let message = menu.message.unwrap_or(0);
        match discord.edit_menu(message, &chunks).await {
            Ok(()) => Outcome::new(Code::Edited),
            Err(error) => return discord_error(error),
        }
    } else {
        match discord.send_menu(req.channel, &chunks).await {
            Ok(message) => {
                match store
                    .set_published(menu.id, req.channel, message, Some(req.key))
                    .await
                {
                    Ok(()) => Outcome::new(Code::Posted),
                    Err(error) => {
                        return Outcome::with(Code::StoreFailed, error.code());
                    }
                }
            }
            Err(error) => return discord_error(error),
        }
    };
    outcome
}

fn discord_error(error: DiscordError) -> Outcome {
    match error {
        DiscordError::Forbidden => Outcome::new(Code::DiscordForbidden),
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
    }

    /// A Discord fake that records the calls it was given.
    #[derive(Default)]
    struct FakeDiscord {
        sends: Mutex<Vec<(u64, Vec<RenderedChunk>)>>,
        edits: Mutex<Vec<(u64, Vec<RenderedChunk>)>>,
        /// When set, every call fails with this error.
        fail: Option<DiscordError>,
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
            message: u64,
            chunks: &[RenderedChunk],
        ) -> Result<(), DiscordError> {
            self.edits.lock().unwrap().push((message, chunks.to_vec()));
            match self.fail {
                Some(error) => Err(error),
                None => Ok(()),
            }
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
            emoji: "x",
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
                    emoji: "x",
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
    fn a_label_must_be_non_empty_bounded_and_free_of_control_characters() {
        assert_eq!(validate_label("Alpha"), Ok(()));
        assert_eq!(validate_label("  Alpha  "), Ok(()));
        assert_eq!(validate_label(""), Err(Code::InvalidLabel));
        assert_eq!(validate_label("   "), Err(Code::InvalidLabel));
        assert_eq!(
            validate_label(&"a".repeat(MAX_LABEL_CHARS + 1)),
            Err(Code::InvalidLabel)
        );
        assert_eq!(validate_label(&"a".repeat(MAX_LABEL_CHARS)), Ok(()));
        assert_eq!(validate_label("Alpha\u{0}"), Err(Code::InvalidLabel));
        assert_eq!(validate_label("Alpha\u{7}"), Err(Code::InvalidLabel));
        // A newline is allowed in a label.
        assert_eq!(validate_label("Alpha\nBeta"), Ok(()));
    }

    #[test]
    fn an_emoji_must_be_non_empty_bounded_and_free_of_control_characters() {
        assert_eq!(validate_emoji("x"), Ok(()));
        assert_eq!(validate_emoji("\u{1F980}"), Ok(()));
        assert_eq!(validate_emoji(""), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("   "), Err(Code::InvalidEmoji));
        assert_eq!(
            validate_emoji(&"x".repeat(MAX_EMOJI_CHARS + 1)),
            Err(Code::InvalidEmoji)
        );
        assert_eq!(validate_emoji(&"x".repeat(MAX_EMOJI_CHARS)), Ok(()));
        assert_eq!(validate_emoji("x\u{0}"), Err(Code::InvalidEmoji));
        assert_eq!(validate_emoji("x\n"), Err(Code::InvalidEmoji));
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
        let edited = discord.edits.lock().unwrap()[0].0;
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
                emoji: "x".to_string(),
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
                emoji: "x".to_string(),
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
}

//! Role menu interactions: a member presses a button or picks from a select (RM3).
//!
//! This module is the testable core behind the role-menu components that RM2
//! posts. It never touches serenity: the wiring card (W4) reads the incoming
//! interaction, hands the plain values over, and answers with what this module
//! says.
//!
//! What it enforces:
//!
//! 1. **The custom id is parsed strictly.** The id has the fixed prefix
//!    [`CUSTOM_ID_PREFIX`] and names a stored menu and one of its two kinds of
//!    component: a button (`m<menu id>:r<role id>`) or a select
//!    (`m<menu id>:s<part>`). It is at most [`MAX_CUSTOM_ID_LEN`] bytes and
//!    holds nothing but ASCII digits and the prefix's separators. A forged,
//!    malformed, other-menu, other-guild or other-message id is ignored: no
//!    member data is read and nothing is written.
//! 2. **The stored row is the truth.** The interaction's own guild and
//!    message must be the ones the menu row records, and the option must
//!    still be stored. A deleted menu or option answers with a notice and
//!    writes nothing.
//! 3. **Defer first.** The acknowledgement is an ephemeral deferral that
//!    happens before any slow call, then the work runs, then one ephemeral
//!    follow-up is sent. Every reply is built from stable codes: no role
//!    name, no label and no member text is ever echoed.
//! 4. **RM1 decides.** The click is planned by the shared policy
//!    (`super::policy`): a multi option toggles, an exclusive group swaps, and a
//!    max-select breach is refused. A partial write is reported honestly
//!    (`swap_partial`, `swap_partial_removed` or `partial_write`) and never
//!    as a success.
//! 5. **Every role write goes through the role service** (K03 decide, K04
//!    grant), never directly. Protected, managed and privileged roles are
//!    refused at press time even when the stored row lists them, and an active
//!    safeword blocks a restricted option for that member.
//! 6. **One member at a time.** Two quick presses by the same member never
//!    interleave their read-modify-write: the handler serializes them behind a
//!    per-member lock whose entries are released when the press is done.
//! 7. **Home guild only, fail closed.** A press from another guild touches
//!    nothing. With the feature on and the home guild unset, nothing is
//!    served. In shadow mode the module reports what it would do and writes
//!    nothing. A failed write is reported with a stable code and never
//!    discarded.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::roles::eligibility::{RoleFacts, SafewordState};
use crate::roles::grant::{self, FailCode, GrantItem, RoleOutcome, WriteError};

use super::command::{self, to_policy_menu};
use super::policy;
use super::repository::{MenuRow, OptionRow, RepoError};

/// The prefix every role-menu component id carries (PLAN section 5, rule 5).
pub const CUSTOM_ID_PREFIX: &str = "rolemenu:v1:";

/// Discord's limit for a component's custom id, in bytes.
pub const MAX_CUSTOM_ID_LEN: usize = 100;

/// The stable codes of this module.
///
/// Every code is a fixed word: no code carries a role name, a label, a member
/// id or any text a user typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// The feature is off.
    Disabled,
    /// The press came from a guild that is not the home guild.
    OtherGuild,
    /// The feature is on but the home guild is not set: nothing is served.
    NoHomeGuild,
    /// The custom id is not a role-menu id at all (bad prefix, bad shape or too long).
    InvalidCustomId,
    /// A submitted select value is not a role id at all.
    InvalidValue,
    /// The custom id names a menu that is not stored.
    MenuNotFound,
    /// The custom id names an option that is not stored.
    OptionNotFound,
    /// The press did not come from the message the menu row records.
    MessageMismatch,
    /// The menu row belongs to another guild than the press.
    GuildMismatch,
    /// The stored menu could not be understood.
    CorruptRow,
    /// The stored menu could not be read.
    StoreFailed,
    /// The acknowledgement could not be sent.
    DeferFailed,
    /// The role facts could not be looked up.
    FactsUnavailable,
    /// The member's roles could not be read.
    MemberRolesUnavailable,
    /// The role was refused by K03 (the detail carries the K03 code).
    Ineligible,
    /// The option was added.
    Added,
    /// The option was removed.
    Removed,
    /// An exclusive swap moved the member from one option to another.
    Swapped,
    /// The member already held the option of an exclusive group.
    AlreadySelected,
    /// The group already holds as many options as it may.
    MaxReached,
    /// The click named an option that is not an option of the group.
    UnknownOption,
    /// The press named more options than one press may name.
    TooManySelected,
    /// The exclusive swap could not remove an old option; nothing changed.
    SwapBlocked,
    /// An exclusive swap added the new option but could not remove an old one.
    SwapPartial,
    /// An exclusive swap completed all planned removals but failed to add the new option.
    SwapPartialRemoved,
    /// Some planned writes failed and some succeeded, without either swap outcome above.
    PartialWrite,
    /// At least one role write failed and none succeeded.
    WriteFailed,
    /// Some of the submitted values were applied and some were refused.
    Partial,
    /// Nothing changed.
    NoChange,
}

impl Code {
    /// The stable machine code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Code::Disabled => "disabled",
            Code::OtherGuild => "other_guild",
            Code::NoHomeGuild => "no_home_guild",
            Code::InvalidCustomId => "invalid_custom_id",
            Code::InvalidValue => "invalid_value",
            Code::MenuNotFound => "menu_not_found",
            Code::OptionNotFound => "option_not_found",
            Code::MessageMismatch => "message_mismatch",
            Code::GuildMismatch => "guild_mismatch",
            Code::CorruptRow => "corrupt_row",
            Code::StoreFailed => "store_failed",
            Code::DeferFailed => "defer_failed",
            Code::FactsUnavailable => "facts_unavailable",
            Code::MemberRolesUnavailable => "member_roles_unavailable",
            Code::Ineligible => "ineligible",
            Code::Added => "added",
            Code::Removed => "removed",
            Code::Swapped => "swapped",
            Code::AlreadySelected => "already_selected",
            Code::MaxReached => "max_reached",
            Code::UnknownOption => "unknown_option",
            Code::TooManySelected => "too_many_selected",
            Code::SwapBlocked => "swap_blocked",
            Code::SwapPartial => "swap_partial",
            Code::SwapPartialRemoved => "swap_partial_removed",
            Code::PartialWrite => "partial_write",
            Code::WriteFailed => "write_failed",
            Code::Partial => "partial",
            Code::NoChange => "no_change",
        }
    }
}

/// An outcome: one code and, when there is one, a stable second code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// The result.
    pub code: Code,
    /// A stable sub-code (a K03 reason code, a K04 fail code, a store code).
    pub detail: Option<&'static str>,
}

impl Outcome {
    /// An outcome without a detail.
    pub const fn new(code: Code) -> Self {
        Outcome { code, detail: None }
    }

    /// An outcome with a detail code.
    pub const fn with(code: Code, detail: &'static str) -> Self {
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

    /// The stable sub-code of this outcome.
    ///
    /// An outcome that carries a detail keeps it; an outcome that carries
    /// only a code uses the code itself. So a refusal that has no detail of
    /// its own (`max_reached`, `already_selected`) still names itself
    /// instead of losing its name.
    fn sub_code(self) -> &'static str {
        self.detail.unwrap_or_else(|| self.code.as_str())
    }
}

// ---------------------------------------------------------------------------
// Custom id
// ---------------------------------------------------------------------------

/// Which kind of component a custom id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    /// A button for one stored option, named by the option's role id.
    ///
    /// The role id is the option's identity inside a menu: RM2 stores one
    /// option row per role, so a button keeps naming the same option even
    /// after another option of the menu is removed.
    Option(u64),
    /// A select component of the menu, named by its part.
    ///
    /// The part is the zero-based chunk number of RM2's `split_chunks`: a
    /// menu with more than 25 options is rendered as several select
    /// components in one message, and each of them needs its own component
    /// id. The submitted values name the roles.
    Select(usize),
}

/// The menu and component a custom id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCustomId {
    /// The stored menu the id names.
    pub menu_id: i64,
    /// Which component of the menu the id names.
    pub selector: Selector,
}

/// Why a component id was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The id is longer than [`MAX_CUSTOM_ID_LEN`].
    TooLong,
    /// The id does not start with [`CUSTOM_ID_PREFIX`].
    BadPrefix,
    /// The id does not have the shape `prefix m<digits>:r<digits>` or `prefix m<digits>:s`.
    Malformed,
    /// A number in the id is not a number, or is zero where zero is not allowed.
    BadNumber,
    /// A number in the id is written in a spelling the bot never writes.
    NotCanonical,
}

impl ParseError {
    /// The stable machine code.
    pub const fn code(self) -> &'static str {
        match self {
            ParseError::TooLong => "custom_id_too_long",
            ParseError::BadPrefix => "custom_id_bad_prefix",
            ParseError::Malformed => "custom_id_malformed",
            ParseError::BadNumber => "custom_id_bad_number",
            ParseError::NotCanonical => "custom_id_not_canonical",
        }
    }
}

/// Parses a decimal number the way the bot writes it.
///
/// Only ASCII digits are accepted, the number must not have a leading zero
/// (unless it is the single digit `0`) and it must fit the target type. Every
/// other spelling of a number is refused, so a hostile spelling of the class
/// is refused and not guessed.
fn parse_canonical<T: std::str::FromStr>(digits: &str) -> Result<T, ParseError> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::BadNumber);
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(ParseError::NotCanonical);
    }
    digits.parse::<T>().map_err(|_| ParseError::BadNumber)
}

/// The most select components one message may hold: Discord's five action
/// rows. A menu may not hold more than `MAX_ROWS_PER_MESSAGE` components, so
/// a part number at or above this can never name a real component.
pub const MAX_SELECT_PARTS: usize = command::MAX_ROWS_PER_MESSAGE;

/// Parse a component id of a role menu.
///
/// The id is `rolemenu:v1:m<menu id>:r<role id>` for a button or
/// `rolemenu:v1:m<menu id>:s<part>` for a select component. Every part must
/// be there and must be ASCII digits only: a letter, a sign, a space, a
/// separator of its own or any other character makes the whole id malformed,
/// so a hostile spelling of the class is refused and not guessed.
///
/// A menu id of zero, a role id of zero, a part at or above
/// [`MAX_SELECT_PARTS`] and a number that does not fit are refused: none of
/// them can name a stored row or a real component.
pub fn parse_custom_id(raw: &str) -> Result<ParsedCustomId, ParseError> {
    if raw.len() > MAX_CUSTOM_ID_LEN {
        return Err(ParseError::TooLong);
    }
    let Some(rest) = raw.strip_prefix(CUSTOM_ID_PREFIX) else {
        return Err(ParseError::BadPrefix);
    };
    let mut parts = rest.split(':');
    let (Some(menu_part), Some(kind_part)) = (parts.next(), parts.next()) else {
        return Err(ParseError::Malformed);
    };
    if parts.next().is_some() {
        return Err(ParseError::Malformed);
    }
    let Some(menu_digits) = menu_part.strip_prefix('m') else {
        return Err(ParseError::Malformed);
    };
    let menu_id: i64 = parse_canonical(menu_digits)?;
    if menu_id <= 0 {
        return Err(ParseError::BadNumber);
    }
    let selector = if let Some(part_digits) = kind_part.strip_prefix('s') {
        // A select component always names its part: a bare `s` cannot say
        // which of a menu's select components was pressed.
        let part: usize = parse_canonical(part_digits)?;
        if part >= MAX_SELECT_PARTS {
            return Err(ParseError::BadNumber);
        }
        Selector::Select(part)
    } else {
        let Some(role_digits) = kind_part.strip_prefix('r') else {
            return Err(ParseError::Malformed);
        };
        let role_id: u64 = parse_canonical(role_digits)?;
        if role_id == 0 {
            return Err(ParseError::BadNumber);
        }
        Selector::Option(role_id)
    };
    Ok(ParsedCustomId { menu_id, selector })
}

/// Build the component id of one option (button) of a menu.
///
/// The result is always shorter than [`MAX_CUSTOM_ID_LEN`]; a menu id or a
/// role id that could not fit is refused instead of being cut.
pub fn custom_id_for_option(menu_id: i64, role_id: u64) -> Result<String, ParseError> {
    if menu_id <= 0 || role_id == 0 {
        return Err(ParseError::BadNumber);
    }
    let raw = format!("{CUSTOM_ID_PREFIX}m{menu_id}:r{role_id}");
    if raw.len() > MAX_CUSTOM_ID_LEN {
        return Err(ParseError::TooLong);
    }
    Ok(raw)
}

/// Build the component id of one select component of a menu.
///
/// The part is the zero-based chunk number of RM2's `split_chunks`: a menu
/// with more than 25 options is rendered as several select components in one
/// message, and each of them needs its own component id, because Discord
/// wants the custom ids of one message to be different from each other.
///
/// The result is always shorter than [`MAX_CUSTOM_ID_LEN`]; a menu id that
/// could not fit is refused instead of being cut, and a part at or above
/// [`MAX_SELECT_PARTS`] is refused because no message holds that many
/// components.
pub fn custom_id_for_select(menu_id: i64, part: usize) -> Result<String, ParseError> {
    if menu_id <= 0 {
        return Err(ParseError::BadNumber);
    }
    if part >= MAX_SELECT_PARTS {
        return Err(ParseError::BadNumber);
    }
    let raw = format!("{CUSTOM_ID_PREFIX}m{menu_id}:s{part}");
    if raw.len() > MAX_CUSTOM_ID_LEN {
        return Err(ParseError::TooLong);
    }
    Ok(raw)
}

/// Parse one submitted select value into a role id.
///
/// The value is the string the bot wrote as the select option's value: the
/// role id in decimal. Only ASCII digits are accepted, the number must not
/// have a leading zero and it must not be zero: none of those can name a
/// stored option.
pub fn parse_value(raw: &str) -> Result<u64, ParseError> {
    let role_id: u64 = parse_canonical(raw)?;
    if role_id == 0 {
        return Err(ParseError::BadNumber);
    }
    Ok(role_id)
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// The mode the feature runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The feature is off: nothing is served.
    Off,
    /// The feature reports what it would do and writes nothing.
    Shadow,
    /// The feature does the work.
    Live,
}

/// The configuration the handler needs. Every field is decided by the wiring
/// card; this module reads them and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The mode the feature runs in.
    pub mode: Mode,
    /// The one guild these interactions are served in (`GUILD_ID`).
    pub home_guild: Option<u64>,
    /// Roles that may never be granted through a menu (`PROTECTED_ROLE_IDS`).
    pub protected_role_ids: Vec<u64>,
    /// True when `PROTECTED_ROLE_IDS` could not be read: fail closed.
    pub protected_roles_invalid: bool,
    /// Roles that may not be granted while the safeword is active.
    pub safeword_restricted_role_ids: Vec<u64>,
}

impl Settings {
    /// Settings for a live feature in the given home guild.
    pub fn live(home_guild: u64) -> Self {
        Settings {
            mode: Mode::Live,
            home_guild: Some(home_guild),
            protected_role_ids: Vec::new(),
            protected_roles_invalid: false,
            safeword_restricted_role_ids: Vec::new(),
        }
    }
}

/// One press of a role-menu component.
///
/// The values come from the interaction: the wiring card reads them off the
/// serenity object and passes them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    /// The guild the press came from.
    pub guild_id: u64,
    /// The message the component is on.
    pub message_id: u64,
    /// The member who pressed.
    pub member_id: u64,
    /// The custom id of the component.
    pub custom_id: String,
    /// The values a select component submitted. Empty for a button.
    pub values: Vec<String>,
    /// Whether the member's safeword is active.
    pub safeword: SafewordState,
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// The menu tables, read through the ids a component id names.
pub trait MenuStore {
    /// Reads one menu by its row id. `Ok(None)` means no such menu.
    fn get_menu_by_id(&self, menu_id: i64) -> Result<Option<MenuRow>, RepoError>;
    /// Lists the options of one menu in repository order, as used by RM2 to
    /// render the select chunks.
    fn list_options(&self, menu_id: i64) -> Result<Vec<OptionRow>, RepoError>;
}

/// Why a lookup did not answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupFail {
    /// The lookup failed. The caller denies; it never guesses.
    Unavailable,
}

impl LookupFail {
    /// The stable machine code.
    pub const fn code(self) -> &'static str {
        match self {
            LookupFail::Unavailable => "facts_unavailable",
        }
    }
}

/// Where the facts of a role come from.
pub trait RoleFactsSource {
    /// The facts of one role. `Ok(None)` means the role is not in the guild.
    fn role_facts(&self, guild_id: u64, role_id: u64) -> Result<Option<RoleFacts>, LookupFail>;
    /// The position of the bot's top role in the guild.
    fn bot_top_position(&self, guild_id: u64) -> Result<u16, LookupFail>;
}

/// Removes one role from a member.
///
/// The shape mirrors `grant::RoleWriter` so removals are offline-testable and
/// can go through the same bounded retry.
pub trait RoleRemover {
    /// Removes one role from the member.
    fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError>;
}

/// The role service the handler writes through.
///
/// The handler never writes a role itself: every add and every removal goes
/// through this port, so the K03 decision and the K04 executor stay in charge.
/// The add side is `grant::RoleWriter`, so the K04 executor can be used
/// unchanged; the remove side is `RoleRemover`, so a removal can be retried
/// the same way.
pub trait RoleService: grant::RoleWriter + RoleRemover {
    /// The role ids the member holds.
    fn member_roles(&self, guild_id: u64, member_id: u64) -> Result<Vec<u64>, LookupFail>;
}

/// Injectable reporter for sanitized failure lines.
pub trait FailureReporter {
    /// Reports one line. The line holds ids and stable codes only.
    fn report(&mut self, line: &str);
}

/// A reporter that keeps the lines in memory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemoryReporter {
    /// The lines that were reported.
    pub lines: Vec<String>,
}

impl FailureReporter for MemoryReporter {
    fn report(&mut self, line: &str) {
        self.lines.push(line.to_string());
    }
}

/// The port that answers the interaction on Discord.
///
/// The wiring card implements this over poise: the deferral is the
/// acknowledgement, the follow-up is the ephemeral answer.
pub trait InteractionResponder {
    /// Acknowledges the interaction privately (an ephemeral deferral).
    fn defer(&mut self) -> Result<(), ()>;
    /// Sends the ephemeral follow-up.
    fn follow_up(&mut self, text: &str) -> Result<(), ()>;
}

/// The ports one press needs.
pub struct PressPorts<'a, S: MenuStore + ?Sized, F: RoleFactsSource + ?Sized, R: RoleService> {
    /// The menu tables.
    pub store: &'a S,
    /// The role facts.
    pub facts: &'a F,
    /// The role service.
    pub roles: &'a mut R,
    /// The sleeper the K04 retry waits through.
    pub sleeper: &'a mut dyn grant::Sleeper,
    /// Where sanitized failures go.
    pub reporter: &'a mut dyn FailureReporter,
    /// The Discord side of the interaction.
    pub responder: &'a mut dyn InteractionResponder,
}

/// A sized adapter around a borrowed sleeper.
///
/// The K04 executor is generic over a sized sleeper, so a `dyn` sleeper is
/// wrapped in this before it is handed over. The wrapper only forwards the
/// call; it holds no state of its own.
struct SleeperRef<'a>(&'a mut dyn grant::Sleeper);

impl grant::Sleeper for SleeperRef<'_> {
    fn sleep_ms(&mut self, ms: u64) {
        self.0.sleep_ms(ms);
    }
}

// ---------------------------------------------------------------------------
// The per-member lock
// ---------------------------------------------------------------------------

/// One member's gate.
#[derive(Debug, Default)]
struct MemberGate {
    gate: Mutex<()>,
}

/// One member's entry in the lock map: the gate and the number of presses
/// that hold it or wait for it.
#[derive(Debug)]
struct MemberEntry {
    gate: Arc<MemberGate>,
    waiters: usize,
}

/// Releases the member's turn when the press ends, also on a panic.
///
/// The release must happen even when the work unwinds: a member whose press
/// panicked must not keep its slot forever, or the member could never press
/// again.
struct TurnGuard<'a> {
    locks: &'a MemberLocks,
    member_id: u64,
    gate: Arc<MemberGate>,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.locks.finish(self.member_id, &self.gate);
    }
}

/// Serializes the presses of one member.
///
/// A member may press twice in a row; the two read-modify-write cycles must
/// not interleave. Each member that is being served holds its own gate, so
/// two different members never wait for each other. The entry is removed when
/// the last press of that member is done, so the memory is bounded by the
/// number of members that press at the same time.
///
/// The waiter count is taken and given back under the map lock, so a press
/// that waits for the gate can never be counted as gone: the entry is only
/// removed when no press of that member is left.
#[derive(Debug, Default)]
pub struct MemberLocks {
    inner: Mutex<HashMap<u64, MemberEntry>>,
}

impl MemberLocks {
    /// A lock set with no member in it.
    pub fn new() -> Self {
        MemberLocks {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// The number of members that hold a lock right now.
    pub fn held(&self) -> usize {
        self.inner.lock().map(|map| map.len()).unwrap_or(0)
    }

    /// Wait for the member's turn, run the press, and release the turn.
    ///
    /// The member's entry is removed when the last press of that member is
    /// done, so a finished press leaves nothing behind. Two presses of the
    /// same member that overlap in time run one after the other: the second
    /// one waits for the first to finish.
    pub fn with_member<T>(&self, member_id: u64, work: impl FnOnce() -> T) -> T {
        let gate = self.acquire(member_id);
        let guard = gate.gate.lock().unwrap_or_else(|e| e.into_inner());
        // The guard releases the member's turn even when the work unwinds.
        let turn = TurnGuard {
            locks: self,
            member_id,
            gate: Arc::clone(&gate),
        };
        let result = work();
        drop(guard);
        drop(turn);
        result
    }

    /// Take the member's gate and count this press as one of its waiters.
    ///
    /// The count happens under the map lock, so a press that waits for the
    /// gate is already counted when the press that holds it gives the turn
    /// back.
    fn acquire(&self, member_id: u64) -> Arc<MemberGate> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(member_id).or_insert_with(|| MemberEntry {
            gate: Arc::new(MemberGate::default()),
            waiters: 0,
        });
        entry.waiters += 1;
        Arc::clone(&entry.gate)
    }

    /// Give the member's turn back and drop the entry when nobody waits.
    fn finish(&self, member_id: u64, gate: &Arc<MemberGate>) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = map.get_mut(&member_id) else {
            return;
        };
        if !Arc::ptr_eq(&entry.gate, gate) {
            return;
        }
        entry.waiters = entry.waiters.saturating_sub(1);
        if entry.waiters == 0 {
            map.remove(&member_id);
        }
    }
}

// ---------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------

/// What the handler did with one press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PressOutcome {
    /// The code the follow-up carries.
    pub outcome: Outcome,
    /// The roles the handler added.
    pub added: Vec<u64>,
    /// The roles the handler removed.
    pub removed: Vec<u64>,
    /// True when the handler wrote nothing because the feature is in shadow.
    pub shadow: bool,
}

impl PressOutcome {
    /// An outcome that changed nothing.
    fn ignored(outcome: Outcome) -> Self {
        PressOutcome {
            outcome,
            added: Vec::new(),
            removed: Vec::new(),
            shadow: false,
        }
    }
}

/// Handle one press of a role-menu component.
///
/// The order is fixed: the custom id is parsed, the guild is checked, the
/// press is acknowledged privately, and only then is any slow call made.
pub fn handle_press<S, F, R>(
    settings: &Settings,
    locks: &MemberLocks,
    press: &Press,
    ports: &mut PressPorts<'_, S, F, R>,
) -> PressOutcome
where
    S: MenuStore + ?Sized,
    F: RoleFactsSource + ?Sized,
    R: RoleService,
{
    // 1. The feature must be on and the home guild must be known.
    if settings.mode == Mode::Off {
        return PressOutcome::ignored(Outcome::new(Code::Disabled));
    }
    let Some(home) = settings.home_guild else {
        return PressOutcome::ignored(Outcome::new(Code::NoHomeGuild));
    };
    if home == 0 {
        return PressOutcome::ignored(Outcome::new(Code::NoHomeGuild));
    }
    // 2. Only the home guild is served. A press from another guild touches
    //    nothing at all: not even the member's roles are read.
    if press.guild_id != home {
        return PressOutcome::ignored(Outcome::new(Code::OtherGuild));
    }
    // 3. The custom id is parsed strictly. A forged id is ignored before any
    //    member data is read.
    let parsed = match parse_custom_id(&press.custom_id) {
        Ok(parsed) => parsed,
        Err(error) => {
            ports.reporter.report(&format!(
                "rolemenu_press guild={} member={} error={}",
                press.guild_id,
                press.member_id,
                error.code()
            ));
            return PressOutcome::ignored(Outcome::new(Code::InvalidCustomId));
        }
    };

    // 4. The acknowledgement comes before the work.
    if ports.responder.defer().is_err() {
        ports.reporter.report(&format!(
            "rolemenu_press guild={} member={} error=defer_failed",
            press.guild_id, press.member_id
        ));
        return PressOutcome::ignored(Outcome::new(Code::DeferFailed));
    }

    // 5. One member at a time: the whole read-modify-write cycle of a press
    //    runs under the member's lock.
    locks.with_member(press.member_id, || serve(settings, press, parsed, ports))
}

/// The work of one press, under the member's lock.
fn serve<S, F, R>(
    settings: &Settings,
    press: &Press,
    parsed: ParsedCustomId,
    ports: &mut PressPorts<'_, S, F, R>,
) -> PressOutcome
where
    S: MenuStore + ?Sized,
    F: RoleFactsSource + ?Sized,
    R: RoleService,
{
    // The stored row is the truth: the menu must exist and belong to this
    // guild.
    let menu = match ports.store.get_menu_by_id(parsed.menu_id) {
        Ok(Some(menu)) => menu,
        Ok(None) => return notice(ports, press, Outcome::new(Code::MenuNotFound)),
        Err(error) => return notice(ports, press, Outcome::with(Code::StoreFailed, error.code())),
    };
    if menu.guild != press.guild_id {
        return notice(ports, press, Outcome::new(Code::GuildMismatch));
    }
    // The message the component is on must be the one the row records.
    match menu.message {
        Some(message) if message == press.message_id => {}
        _ => return notice(ports, press, Outcome::new(Code::MessageMismatch)),
    }
    // The options must still be stored.
    let options = match ports.store.list_options(parsed.menu_id) {
        Ok(options) => options,
        Err(error) => return notice(ports, press, Outcome::with(Code::StoreFailed, error.code())),
    };

    // The targets of the press: the roles the press names. A button names
    // one option by its role id; a select names the options by its values.
    let targets = match collect_targets(press, parsed.selector, &options) {
        Ok(targets) => targets,
        Err(outcome) => return notice(ports, press, outcome),
    };

    // The stored menu becomes the shared policy menu. A row whose mode is not
    // known is corrupt and must not fall back to a more permissive mode.
    let policy_menu = match to_policy_menu(&menu, &options) {
        Ok(policy_menu) => policy_menu,
        Err(_) => return notice(ports, press, Outcome::new(Code::CorruptRow)),
    };
    let group = match policy_menu.groups.first() {
        Some(group) => group,
        None => return notice(ports, press, Outcome::new(Code::CorruptRow)),
    };
    // An exclusive group takes exactly one option: a select that submits
    // more than one is refused before anything is read about the member.
    if group.mode == policy::GroupMode::Exclusive && targets.len() > 1 {
        return notice(ports, press, Outcome::new(Code::TooManySelected));
    }

    // An unreadable protected-role list refuses every press.
    if settings.protected_roles_invalid {
        return notice(ports, press, Outcome::new(Code::Ineligible));
    }

    // A valid empty select needs neither member roles nor role facts.
    if targets.is_empty() {
        return notice(ports, press, Outcome::new(Code::NoChange));
    }

    // The member's roles are read from the role service, not from the press.
    let member_roles = match ports.roles.member_roles(press.guild_id, press.member_id) {
        Ok(roles) => roles,
        Err(error) => {
            return notice(
                ports,
                press,
                Outcome::with(Code::MemberRolesUnavailable, error.code()),
            )
        }
    };

    // The bot's top role position decides which roles the bot may hand out.
    let bot_top = match ports.facts.bot_top_position(press.guild_id) {
        Ok(top) => top,
        Err(error) => {
            return notice(
                ports,
                press,
                Outcome::with(Code::FactsUnavailable, error.code()),
            )
        }
    };

    // The facts of every role the press may touch are read once.
    let mut facts_by_role = Vec::new();
    for option in &options {
        let facts = match ports.facts.role_facts(press.guild_id, option.role) {
            Ok(Some(facts)) => facts,
            Ok(None) => RoleFacts {
                id: option.role,
                position: 0,
                managed: false,
                permissions: 0,
                exists: false,
            },
            Err(error) => {
                return notice(
                    ports,
                    press,
                    Outcome::with(Code::FactsUnavailable, error.code()),
                )
            }
        };
        facts_by_role.push(facts);
    }

    // Every target is planned by the shared RM1 policy, one click at a time.
    // The member's roles are carried along so the second target of a select
    // sees what the first one changed.
    let mut held: Vec<u64> = member_roles.clone();
    let mut add: Vec<u64> = Vec::new();
    let mut remove: Vec<u64> = Vec::new();
    let mut refusal: Option<Outcome> = None;
    for target in &targets {
        let plan = match policy::plan_click(&policy::PlanRequest {
            menu: &policy_menu,
            click: &policy::Click {
                member_roles: held.clone(),
                group_id: group.id.clone(),
                option_role_id: *target,
            },
            grant: policy::GrantContext {
                guild_id: press.guild_id,
                bot_top_position: bot_top,
                role_facts: &facts_by_role,
                protected_role_ids: &settings.protected_role_ids,
                safeword: press.safeword,
                safeword_restricted_role_ids: &settings.safeword_restricted_role_ids,
            },
        }) {
            Ok(plan) => plan,
            Err(error) => {
                return notice(ports, press, Outcome::with(Code::Ineligible, error.code()))
            }
        };
        // The policy's own reasons are reported: a refusal that never
        // reaches a write. A blocked swap is reported as `swap_blocked` even
        // though the policy also names the removal that could not be done.
        let blocked_swap = plan
            .skipped
            .iter()
            .any(|(_, reason)| matches!(reason, policy::Reason::SwapBlocked));
        if blocked_swap {
            return notice(ports, press, Outcome::new(Code::SwapBlocked));
        }
        if let Some((_, reason)) = plan.skipped.first() {
            let outcome = match reason {
                policy::Reason::Denied(deny) => Outcome::with(Code::Ineligible, deny.code()),
                policy::Reason::RemovalDenied(deny) => Outcome::with(Code::Ineligible, deny.code()),
                policy::Reason::UnknownOption => Outcome::new(Code::UnknownOption),
                policy::Reason::MaxReached => Outcome::new(Code::MaxReached),
                policy::Reason::AlreadySelected => Outcome::new(Code::AlreadySelected),
                policy::Reason::SwapBlocked => Outcome::new(Code::SwapBlocked),
            };
            if refusal.is_none() {
                refusal = Some(outcome);
            }
            continue;
        }
        for role in &plan.remove {
            if !remove.contains(role) {
                remove.push(*role);
            }
            held.retain(|held_role| held_role != role);
        }
        for role in &plan.add {
            if !add.contains(role) {
                add.push(*role);
            }
            held.push(*role);
        }
    }

    // Nothing was planned and something was refused: the press is a notice.
    if add.is_empty() && remove.is_empty() {
        if let Some(outcome) = refusal {
            return notice(ports, press, outcome);
        }
        return notice(ports, press, Outcome::new(Code::NoChange));
    }

    // In shadow mode nothing is written; the outcome says what would happen.
    if settings.mode == Mode::Shadow {
        let outcome = shadow_outcome(&add, &remove, refusal);
        let result = PressOutcome {
            outcome,
            added: add,
            removed: remove,
            shadow: true,
        };
        finish(ports, &result);
        return result;
    }

    // The removals come first, then the additions: an exclusive swap leaves
    // the member with the option they pressed.
    let mut removed = Vec::new();
    let mut failure: Option<FailCode> = None;
    for &role in &remove {
        let outcome = {
            let mut sleeper = SleeperRef(ports.sleeper);
            remove_with_retry(&mut *ports.roles, &mut sleeper, press.member_id, role)
        };
        match outcome {
            Ok(()) => removed.push(role),
            Err(code) => {
                if failure.is_none() {
                    failure = Some(code);
                }
                report_write_failure(ports.reporter, press, role, code);
            }
        }
    }
    let mut added = Vec::new();
    if !add.is_empty() {
        let items: Vec<GrantItem> = add
            .iter()
            .map(|role_id| GrantItem {
                role_id: *role_id,
                decision: crate::roles::eligibility::Decision::Allow,
                already_held: false,
            })
            .collect();
        let report = {
            let mut sleeper = SleeperRef(ports.sleeper);
            grant::apply(
                &mut *ports.roles,
                &mut sleeper,
                &grant::GrantRequest {
                    member_id: press.member_id,
                    items: &items,
                },
            )
        };
        for result in &report.results {
            match result.outcome {
                RoleOutcome::Done => added.push(result.role_id),
                RoleOutcome::NoOp => {}
                RoleOutcome::Denied(_) => {
                    if failure.is_none() {
                        failure = Some(FailCode::Other);
                    }
                    report_write_failure(ports.reporter, press, result.role_id, FailCode::Other);
                }
                RoleOutcome::Failed(code) => {
                    if failure.is_none() {
                        failure = Some(code);
                    }
                    report_write_failure(ports.reporter, press, result.role_id, code);
                }
            }
        }
    }

    let outcome = match failure {
        Some(code) => {
            // A failure is never reported as a success. The code says what
            // really happened, so it is derived from the writes that were
            // planned and the writes that went through.
            failure_outcome(group.mode, &add, &remove, &added, &removed, code)
        }
        None => match refusal {
            Some(outcome) => Outcome::with(Code::Partial, outcome.sub_code()),
            None if added.is_empty() => Outcome::new(Code::Removed),
            None if removed.is_empty() => Outcome::new(Code::Added),
            None => Outcome::new(Code::Swapped),
        },
    };

    let result = PressOutcome {
        outcome,
        added,
        removed,
        shadow: false,
    };
    finish(ports, &result);
    result
}

/// The options one select component of a menu holds.
///
/// The part is the zero-based chunk number of RM2's `split_chunks`: a
/// component holds at most `policy::SELECT_OPTION_LIMIT` options, in the
/// stored order, and a menu with more options produces more components.
pub fn options_of_part(options: &[OptionRow], part: usize) -> &[OptionRow] {
    let start = part.saturating_mul(policy::SELECT_OPTION_LIMIT);
    if start >= options.len() {
        return &[];
    }
    let end = start
        .saturating_add(policy::SELECT_OPTION_LIMIT)
        .min(options.len());
    &options[start..end]
}

/// The roles one press names.
///
/// A button names one option by its role id. A select names its options by
/// the values it submitted: every value must be a role id that is still a
/// stored option of the part the component id names.
fn collect_targets(
    press: &Press,
    selector: Selector,
    options: &[OptionRow],
) -> Result<Vec<u64>, Outcome> {
    match selector {
        Selector::Option(role_id) => {
            if !options.iter().any(|option| option.role == role_id) {
                return Err(Outcome::new(Code::OptionNotFound));
            }
            Ok(vec![role_id])
        }
        Selector::Select(part) => {
            if press.values.is_empty() {
                return Ok(Vec::new());
            }
            if press.values.len() > policy::SELECT_OPTION_LIMIT {
                return Err(Outcome::new(Code::TooManySelected));
            }
            // Only the options of this part may be named: a value of another
            // part of the same menu is not an option of this component.
            let part_options = options_of_part(options, part);
            let mut targets = Vec::with_capacity(press.values.len());
            for value in &press.values {
                let role_id = match parse_value(value) {
                    Ok(role_id) => role_id,
                    Err(_) => return Err(Outcome::new(Code::InvalidValue)),
                };
                if !part_options.iter().any(|option| option.role == role_id) {
                    return Err(Outcome::new(Code::OptionNotFound));
                }
                if targets.contains(&role_id) {
                    return Err(Outcome::new(Code::InvalidValue));
                }
                targets.push(role_id);
            }
            Ok(targets)
        }
    }
}

/// The outcome a failed press reports: exactly what happened.
///
/// The code is derived from the writes that were planned and the writes that
/// went through, so the member is told what really is the case:
///
/// * `write_failed` — nothing at all went through.
/// * `swap_partial` — an exclusive swap added the new option but could not
///   remove every old option.
/// * `swap_partial_removed` — an exclusive swap removed every old option
///   but could not add the new one.
/// * `partial_write` — some other press: some writes went through and some
///   did not. This covers a multi group, a press that only added and a
///   press that only removed.
///
/// The detail is the classified K04 fail code of the first failure.
fn failure_outcome(
    group_mode: policy::GroupMode,
    planned_add: &[u64],
    planned_remove: &[u64],
    added: &[u64],
    removed: &[u64],
    code: FailCode,
) -> Outcome {
    // A swap is an exclusive group that removes an old option and adds a new
    // one in the same press. Nothing else is a swap, so nothing else may
    // claim to be one.
    let swap = group_mode == policy::GroupMode::Exclusive
        && !planned_add.is_empty()
        && !planned_remove.is_empty();
    let outcome = if added.is_empty() && removed.is_empty() {
        Code::WriteFailed
    } else if swap && !added.is_empty() && removed.len() < planned_remove.len() {
        Code::SwapPartial
    } else if swap && added.is_empty() && removed.len() == planned_remove.len() {
        Code::SwapPartialRemoved
    } else {
        Code::PartialWrite
    };
    Outcome::with(outcome, code.code())
}

/// The outcome a shadow press reports: what it would have done.
fn shadow_outcome(add: &[u64], remove: &[u64], refusal: Option<Outcome>) -> Outcome {
    if let Some(outcome) = refusal {
        return Outcome::with(Code::Partial, outcome.sub_code());
    }
    if add.is_empty() && remove.is_empty() {
        Outcome::new(Code::NoChange)
    } else if add.is_empty() {
        Outcome::new(Code::Removed)
    } else if remove.is_empty() {
        Outcome::new(Code::Added)
    } else {
        Outcome::new(Code::Swapped)
    }
}

/// Remove one role, retrying a rate limit a bounded number of times.
///
/// The shape is the one RR2 uses: only a 429 is retried, and the wait is
/// clamped.
fn remove_with_retry<R: RoleRemover, L: grant::Sleeper>(
    remover: &mut R,
    sleeper: &mut L,
    member_id: u64,
    role_id: u64,
) -> Result<(), FailCode> {
    let mut retries: u8 = 0;
    loop {
        let error = match remover.remove_role(member_id, role_id) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let code = grant::classify(&error);
        if code == FailCode::RateLimited && retries < grant::MAX_RATE_LIMIT_RETRIES {
            retries += 1;
            let wait = error
                .retry_after_ms
                .unwrap_or(grant::DEFAULT_BACKOFF_MS * u64::from(retries))
                .min(grant::MAX_SLEEP_MS);
            sleeper.sleep_ms(wait);
            continue;
        }
        return Err(code);
    }
}

/// Report one failed role write with a stable code.
fn report_write_failure(
    reporter: &mut dyn FailureReporter,
    press: &Press,
    role: u64,
    code: FailCode,
) {
    reporter.report(&format!(
        "rolemenu_press guild={} member={} role={role}:failed:{}",
        press.guild_id,
        press.member_id,
        code.code()
    ));
}

/// Send the ephemeral follow-up.
///
/// A follow-up that fails is a lost answer, so it is reported: it is never
/// discarded.
fn finish<S, F, R>(ports: &mut PressPorts<'_, S, F, R>, result: &PressOutcome)
where
    S: MenuStore + ?Sized,
    F: RoleFactsSource + ?Sized,
    R: RoleService,
{
    if ports.responder.follow_up(&result.outcome.text()).is_err() {
        ports.reporter.report(&format!(
            "rolemenu_press error=follow_up_failed outcome={}",
            result.outcome.text()
        ));
    }
}

/// Answer with a notice: an ephemeral follow-up and no write at all.
fn notice<S, F, R>(
    ports: &mut PressPorts<'_, S, F, R>,
    press: &Press,
    outcome: Outcome,
) -> PressOutcome
where
    S: MenuStore + ?Sized,
    F: RoleFactsSource + ?Sized,
    R: RoleService,
{
    ports.reporter.report(&format!(
        "rolemenu_press guild={} member={} outcome={}",
        press.guild_id,
        press.member_id,
        outcome.text()
    ));
    let result = PressOutcome::ignored(outcome);
    if ports.responder.follow_up(&result.outcome.text()).is_err() {
        ports.reporter.report(&format!(
            "rolemenu_press error=follow_up_failed outcome={}",
            result.outcome.text()
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::{ADMINISTRATOR, MANAGE_ROLES};
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // Synthetic ids only; never a real Discord id.
    const HOME: u64 = 770_001;
    const OTHER_GUILD: u64 = 770_002;
    const MESSAGE: u64 = 770_010;
    const OTHER_MESSAGE: u64 = 770_011;
    const MEMBER: u64 = 770_020;
    const ROLE_A: u64 = 770_101;
    const ROLE_B: u64 = 770_102;
    const ROLE_C: u64 = 770_103;
    const ROLE_D: u64 = 770_104;
    const BOT_TOP: u16 = 10;

    // ---------------------------------------------------------------------
    // Fakes
    // ---------------------------------------------------------------------

    /// A menu store over plain rows, with an optional failure per call.
    #[derive(Default)]
    struct FakeStore {
        menus: HashMap<i64, MenuRow>,
        options: HashMap<i64, Vec<OptionRow>>,
        menu_error: Option<RepoError>,
        option_error: Option<RepoError>,
        menu_reads: AtomicUsize,
        option_reads: AtomicUsize,
    }

    impl FakeStore {
        fn with_menu(menu: MenuRow, options: Vec<OptionRow>) -> Self {
            let mut store = FakeStore::default();
            store.options.insert(menu.id, options);
            store.menus.insert(menu.id, menu);
            store
        }
        fn menu_reads(&self) -> usize {
            self.menu_reads.load(Ordering::SeqCst)
        }
    }

    impl MenuStore for FakeStore {
        fn get_menu_by_id(&self, menu_id: i64) -> Result<Option<MenuRow>, RepoError> {
            self.menu_reads.fetch_add(1, Ordering::SeqCst);
            match self.menu_error {
                Some(error) => Err(error),
                None => Ok(self.menus.get(&menu_id).cloned()),
            }
        }
        fn list_options(&self, menu_id: i64) -> Result<Vec<OptionRow>, RepoError> {
            self.option_reads.fetch_add(1, Ordering::SeqCst);
            match self.option_error {
                Some(error) => Err(error),
                None => Ok(self.options.get(&menu_id).cloned().unwrap_or_default()),
            }
        }
    }

    /// Role facts over a plain map.
    struct FakeFacts {
        facts: HashMap<u64, RoleFacts>,
        bot_top: Result<u16, LookupFail>,
        fail: Option<LookupFail>,
        reads: AtomicUsize,
    }

    impl FakeFacts {
        fn new(ids: &[u64]) -> Self {
            let mut facts = HashMap::new();
            for id in ids {
                facts.insert(
                    *id,
                    RoleFacts {
                        id: *id,
                        position: 5,
                        managed: false,
                        permissions: 0,
                        exists: true,
                    },
                );
            }
            FakeFacts {
                facts,
                bot_top: Ok(BOT_TOP),
                fail: None,
                reads: AtomicUsize::new(0),
            }
        }
    }

    impl RoleFactsSource for FakeFacts {
        fn role_facts(
            &self,
            _guild_id: u64,
            role_id: u64,
        ) -> Result<Option<RoleFacts>, LookupFail> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            match self.fail {
                Some(error) => Err(error),
                None => Ok(self.facts.get(&role_id).copied()),
            }
        }
        fn bot_top_position(&self, _guild_id: u64) -> Result<u16, LookupFail> {
            self.bot_top
        }
    }

    /// A role service over a plain member-role map, with scripted failures.
    ///
    /// The add failures are a queue per role: every call takes the next
    /// scripted result, so a 429 can be scripted to fail once and then
    /// succeed.
    #[derive(Default)]
    struct FakeRoles {
        held: HashMap<u64, Vec<u64>>,
        add_failures: HashMap<u64, Vec<WriteError>>,
        remove_failures: HashMap<u64, Vec<WriteError>>,
        add_calls: Mutex<Vec<u64>>,
        remove_calls: Mutex<Vec<u64>>,
        member_roles_fail: Option<LookupFail>,
        member_roles_reads: AtomicUsize,
    }

    impl FakeRoles {
        fn new(member: u64, held: &[u64]) -> Self {
            let mut roles = FakeRoles::default();
            roles.held.insert(member, held.to_vec());
            roles
        }
        fn added(&self) -> Vec<u64> {
            self.add_calls.lock().expect("add calls").clone()
        }
        fn removed(&self) -> Vec<u64> {
            self.remove_calls.lock().expect("remove calls").clone()
        }
        fn add_call_count(&self, role_id: u64) -> usize {
            self.add_calls
                .lock()
                .expect("add calls")
                .iter()
                .filter(|r| **r == role_id)
                .count()
        }
        fn remove_call_count(&self, role_id: u64) -> usize {
            self.remove_calls
                .lock()
                .expect("remove calls")
                .iter()
                .filter(|r| **r == role_id)
                .count()
        }
    }

    impl grant::RoleWriter for FakeRoles {
        fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.add_calls.lock().expect("add calls").push(role_id);
            if let Some(queue) = self.add_failures.get_mut(&role_id) {
                if !queue.is_empty() {
                    return Err(queue.remove(0));
                }
            }
            self.held.entry(member_id).or_default().push(role_id);
            Ok(())
        }
    }

    impl RoleRemover for FakeRoles {
        fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.remove_calls
                .lock()
                .expect("remove calls")
                .push(role_id);
            if let Some(queue) = self.remove_failures.get_mut(&role_id) {
                if !queue.is_empty() {
                    return Err(queue.remove(0));
                }
            }
            if let Some(list) = self.held.get_mut(&member_id) {
                list.retain(|r| *r != role_id);
            }
            Ok(())
        }
    }

    impl RoleService for FakeRoles {
        fn member_roles(&self, _guild_id: u64, member_id: u64) -> Result<Vec<u64>, LookupFail> {
            self.member_roles_reads.fetch_add(1, Ordering::SeqCst);
            match self.member_roles_fail {
                Some(error) => Err(error),
                None => Ok(self.held.get(&member_id).cloned().unwrap_or_default()),
            }
        }
    }

    /// A sleeper that records the waits instead of waiting.
    #[derive(Default)]
    struct FakeSleeper {
        slept: Mutex<Vec<u64>>,
    }

    impl FakeSleeper {
        fn slept(&self) -> Vec<u64> {
            self.slept.lock().expect("slept").clone()
        }
    }

    impl grant::Sleeper for FakeSleeper {
        fn sleep_ms(&mut self, ms: u64) {
            self.slept.lock().expect("slept").push(ms);
        }
    }

    /// A responder that records the order of the calls.
    #[derive(Default)]
    struct FakeResponder {
        calls: Mutex<Vec<String>>,
        defer_fails: bool,
        follow_up_fails: bool,
    }

    impl FakeResponder {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
        /// The follow-up texts, in order.
        fn follow_ups(&self) -> Vec<String> {
            self.calls()
                .iter()
                .filter_map(|c| c.strip_prefix("follow_up:").map(|s| s.to_string()))
                .collect()
        }
    }

    impl InteractionResponder for FakeResponder {
        fn defer(&mut self) -> Result<(), ()> {
            self.calls.lock().expect("calls").push("defer".to_string());
            if self.defer_fails {
                Err(())
            } else {
                Ok(())
            }
        }
        fn follow_up(&mut self, text: &str) -> Result<(), ()> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("follow_up:{text}"));
            if self.follow_up_fails {
                Err(())
            } else {
                Ok(())
            }
        }
    }

    fn forbidden() -> WriteError {
        WriteError {
            status: 403,
            api_code: None,
            retry_after_ms: None,
        }
    }

    fn rate_limited(retry_after_ms: Option<u64>) -> WriteError {
        WriteError {
            status: 429,
            api_code: None,
            retry_after_ms,
        }
    }

    fn menu_row(id: i64, guild: u64, message: Option<u64>) -> MenuRow {
        MenuRow {
            id,
            guild,
            key: "main".to_string(),
            mode: "multi".to_string(),
            max_select: 0,
            channel: Some(770_030),
            message,
            embed_key: None,
        }
    }

    fn option_row(menu_id: i64, role: u64, position: i64) -> OptionRow {
        OptionRow {
            menu_id,
            role,
            label: format!("label_{role}"),
            emoji: "\u{1F600}".to_string(),
            position,
        }
    }

    fn settings() -> Settings {
        Settings::live(HOME)
    }

    /// A press of the button of one option.
    fn press(custom_id: &str) -> Press {
        Press {
            guild_id: HOME,
            message_id: MESSAGE,
            member_id: MEMBER,
            custom_id: custom_id.to_string(),
            values: Vec::new(),
            safeword: SafewordState::Inactive,
        }
    }

    /// A press of the button of one option of a menu.
    fn press_option(menu_id: i64, role_id: u64) -> Press {
        press(&custom_id_for_option(menu_id, role_id).expect("custom id"))
    }

    /// A press of the select component of a menu, with the given values.
    fn press_select(menu_id: i64, values: &[u64]) -> Press {
        press_select_part(menu_id, 0, values)
    }

    /// A press of the select component of one part of a menu.
    fn press_select_part(menu_id: i64, part: usize, values: &[u64]) -> Press {
        let mut press = press(&custom_id_for_select(menu_id, part).expect("custom id"));
        press.values = values.iter().map(|role| role.to_string()).collect();
        press
    }

    /// A whole press with the fakes wired up.
    struct Harness<S: MenuStore> {
        store: S,
        facts: FakeFacts,
        roles: FakeRoles,
        responder: FakeResponder,
        reporter: MemoryReporter,
        sleeper: FakeSleeper,
        locks: MemberLocks,
    }

    impl<S: MenuStore> Harness<S> {
        fn new(store: S, facts: FakeFacts, roles: FakeRoles) -> Self {
            Harness {
                store,
                facts,
                roles,
                responder: FakeResponder::default(),
                reporter: MemoryReporter::default(),
                sleeper: FakeSleeper::default(),
                locks: MemberLocks::new(),
            }
        }

        fn run(&mut self, settings: &Settings, press: &Press) -> PressOutcome {
            let mut ports = PressPorts {
                store: &self.store,
                facts: &self.facts,
                roles: &mut self.roles,
                sleeper: &mut self.sleeper,
                reporter: &mut self.reporter,
                responder: &mut self.responder,
            };
            handle_press(settings, &self.locks, press, &mut ports)
        }

        fn follow_ups(&self) -> Vec<String> {
            self.responder.follow_ups()
        }
    }

    /// A multi menu with three options.
    fn multi_menu() -> FakeStore {
        FakeStore::with_menu(
            menu_row(1, HOME, Some(MESSAGE)),
            vec![
                option_row(1, ROLE_A, 0),
                option_row(1, ROLE_B, 1),
                option_row(1, ROLE_C, 2),
            ],
        )
    }

    /// An exclusive menu with three options.
    fn exclusive_menu() -> FakeStore {
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").mode = "exclusive".to_string();
        store
    }

    // ---------------------------------------------------------------------
    // Custom id parsing
    // ---------------------------------------------------------------------

    #[test]
    fn a_well_formed_button_id_names_a_menu_and_a_role() {
        let parsed = parse_custom_id("rolemenu:v1:m12:r3").expect("parsed");
        assert_eq!(parsed.menu_id, 12);
        assert_eq!(parsed.selector, Selector::Option(3));
    }

    #[test]
    fn a_well_formed_select_id_names_a_menu_and_a_part() {
        let parsed = parse_custom_id("rolemenu:v1:m12:s0").expect("parsed");
        assert_eq!(parsed.menu_id, 12);
        assert_eq!(parsed.selector, Selector::Select(0));
        let parsed = parse_custom_id("rolemenu:v1:m12:s3").expect("parsed");
        assert_eq!(parsed.menu_id, 12);
        assert_eq!(parsed.selector, Selector::Select(3));
    }

    #[test]
    fn the_option_custom_id_round_trips() {
        let raw = custom_id_for_option(42, 7).expect("built");
        assert_eq!(raw, "rolemenu:v1:m42:r7");
        let parsed = parse_custom_id(&raw).expect("parsed");
        assert_eq!(parsed.menu_id, 42);
        assert_eq!(parsed.selector, Selector::Option(7));
    }

    #[test]
    fn the_select_custom_id_round_trips() {
        let raw = custom_id_for_select(42, 0).expect("built");
        assert_eq!(raw, "rolemenu:v1:m42:s0");
        let parsed = parse_custom_id(&raw).expect("parsed");
        assert_eq!(parsed.menu_id, 42);
        assert_eq!(parsed.selector, Selector::Select(0));
        let raw = custom_id_for_select(42, 4).expect("built");
        assert_eq!(raw, "rolemenu:v1:m42:s4");
        let parsed = parse_custom_id(&raw).expect("parsed");
        assert_eq!(parsed.menu_id, 42);
        assert_eq!(parsed.selector, Selector::Select(4));
    }

    #[test]
    fn a_custom_id_longer_than_the_limit_is_refused() {
        let raw = format!("rolemenu:v1:m1:r{}", "9".repeat(MAX_CUSTOM_ID_LEN));
        assert!(raw.len() > MAX_CUSTOM_ID_LEN);
        assert_eq!(parse_custom_id(&raw), Err(ParseError::TooLong));
    }

    #[test]
    fn the_longest_writable_custom_id_is_parsed() {
        // The longest id the bot can write: a menu id with the most digits an
        // i64 holds and a role id with the most digits a u64 holds.
        let raw = format!("rolemenu:v1:m{}:r{}", i64::MAX, u64::MAX);
        assert!(raw.len() < MAX_CUSTOM_ID_LEN);
        let parsed = parse_custom_id(&raw).expect("parsed");
        assert_eq!(parsed.menu_id, i64::MAX);
        assert_eq!(parsed.selector, Selector::Option(u64::MAX));
    }

    #[test]
    fn a_custom_id_that_fills_the_limit_is_still_length_accepted() {
        // An id that fills Discord's limit exactly passes the length check;
        // it is refused later only because its role id does not fit a u64.
        let raw = format!("rolemenu:v1:m1:r{}", "9".repeat(MAX_CUSTOM_ID_LEN - 16));
        assert_eq!(raw.len(), MAX_CUSTOM_ID_LEN);
        assert_eq!(parse_custom_id(&raw), Err(ParseError::BadNumber));
    }

    #[test]
    fn a_foreign_prefix_is_refused() {
        assert_eq!(
            parse_custom_id("rolemenu:v2:m1:r0"),
            Err(ParseError::BadPrefix)
        );
        assert_eq!(parse_custom_id("vc:v1:m1:r0"), Err(ParseError::BadPrefix));
        assert_eq!(parse_custom_id(""), Err(ParseError::BadPrefix));
    }

    #[test]
    fn a_malformed_custom_id_is_refused() {
        for raw in [
            "rolemenu:v1:",
            "rolemenu:v1:m1",
            "rolemenu:v1:r1",
            "rolemenu:v1:m1:",
            "rolemenu:v1::r1",
            "rolemenu:v1:m1:r1:extra",
            "rolemenu:v1:x1:r1",
            "rolemenu:v1:m1:x1",
            "rolemenu:v1:m1:r1 ",
            "rolemenu:v1:m 1:r1",
            "rolemenu:v1:m1:r1\u{0}",
            "rolemenu:v1:m1:sx",
            "rolemenu:v1:m1:S",
        ] {
            assert!(parse_custom_id(raw).is_err(), "{raw:?} must not parse");
        }
    }

    #[test]
    fn a_select_id_without_a_part_is_refused() {
        // A bare `s` cannot say which of a menu's select components was
        // pressed, so it names no component at all.
        assert!(parse_custom_id("rolemenu:v1:m1:s").is_err());
        assert!(parse_custom_id("rolemenu:v1:m1:s:0").is_err());
        assert!(parse_custom_id("rolemenu:v1:m1:s-1").is_err());
        assert!(parse_custom_id("rolemenu:v1:m1:s 1").is_err());
    }

    #[test]
    fn a_select_id_with_a_part_that_no_message_can_hold_is_refused() {
        // Discord's message holds five action rows, so a part at or above
        // five names no component of any menu.
        assert!(parse_custom_id("rolemenu:v1:m1:s5").is_err());
        assert!(parse_custom_id("rolemenu:v1:m1:s99").is_err());
        assert_eq!(
            custom_id_for_select(1, MAX_SELECT_PARTS),
            Err(ParseError::BadNumber)
        );
    }

    #[test]
    fn a_custom_id_with_a_hostile_number_is_refused() {
        for raw in [
            "rolemenu:v1:m-1:r0",
            "rolemenu:v1:m+1:r0",
            "rolemenu:v1:m1e3:r0",
            "rolemenu:v1:m0:r0",
            "rolemenu:v1:m1:r-1",
            "rolemenu:v1:m1:r1_0",
            "rolemenu:v1:m\u{0661}:r0",
            "rolemenu:v1:m1:r\u{0661}",
        ] {
            assert_eq!(
                parse_custom_id(raw),
                Err(ParseError::BadNumber),
                "{raw:?} must not parse"
            );
        }
    }

    #[test]
    fn a_custom_id_with_a_non_canonical_number_is_refused() {
        // The bot writes a number in one way only. A spelling with a leading
        // zero names the same number but is not the spelling the bot writes,
        // so it is refused instead of being normalized.
        for raw in [
            "rolemenu:v1:m001:r0",
            "rolemenu:v1:m1:r000",
            "rolemenu:v1:m01:s",
            "rolemenu:v1:m1:r01",
        ] {
            assert_eq!(
                parse_custom_id(raw),
                Err(ParseError::NotCanonical),
                "{raw:?} must not parse"
            );
        }
    }

    #[test]
    fn a_custom_id_with_a_huge_number_is_refused() {
        let raw = format!("rolemenu:v1:m1:r{}", "9".repeat(30));
        assert_eq!(parse_custom_id(&raw), Err(ParseError::BadNumber));
    }

    #[test]
    fn parse_error_codes_are_stable() {
        assert_eq!(ParseError::TooLong.code(), "custom_id_too_long");
        assert_eq!(ParseError::BadPrefix.code(), "custom_id_bad_prefix");
        assert_eq!(ParseError::Malformed.code(), "custom_id_malformed");
        assert_eq!(ParseError::BadNumber.code(), "custom_id_bad_number");
        assert_eq!(ParseError::NotCanonical.code(), "custom_id_not_canonical");
    }

    #[test]
    fn a_value_is_parsed_strictly() {
        assert_eq!(parse_value("42"), Ok(42));
        for raw in [
            "",
            "0",
            "01",
            "-1",
            "+1",
            "4 2",
            "42x",
            "\u{0661}",
            "999999999999999999999999",
        ] {
            assert!(parse_value(raw).is_err(), "{raw:?} must not parse");
        }
    }

    #[test]
    fn a_custom_id_for_a_zero_id_is_refused() {
        assert_eq!(custom_id_for_option(0, ROLE_A), Err(ParseError::BadNumber));
        assert_eq!(custom_id_for_option(1, 0), Err(ParseError::BadNumber));
        assert_eq!(custom_id_for_select(0, 0), Err(ParseError::BadNumber));
    }

    // ---------------------------------------------------------------------
    // The gate
    // ---------------------------------------------------------------------

    #[test]
    fn an_off_feature_serves_nothing_and_reads_nothing() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut settings = settings();
        settings.mode = Mode::Off;
        let result = harness.run(&settings, &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Disabled);
        assert_eq!(harness.store.menu_reads(), 0);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert!(harness.responder.calls().is_empty());
    }

    #[test]
    fn an_unset_home_guild_fails_closed() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut settings = settings();
        settings.home_guild = None;
        let result = harness.run(&settings, &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::NoHomeGuild);
        assert_eq!(harness.store.menu_reads(), 0);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_zero_home_guild_fails_closed() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut settings = settings();
        settings.home_guild = Some(0);
        let result = harness.run(&settings, &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::NoHomeGuild);
        assert_eq!(harness.store.menu_reads(), 0);
    }

    #[test]
    fn a_press_from_another_guild_touches_nothing() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut other = press_option(1, ROLE_A);
        other.guild_id = OTHER_GUILD;
        let result = harness.run(&settings(), &other);
        assert_eq!(result.outcome.code, Code::OtherGuild);
        assert_eq!(harness.store.menu_reads(), 0);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert_eq!(harness.facts.reads.load(Ordering::SeqCst), 0);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
        assert!(harness.responder.calls().is_empty());
    }

    // ---------------------------------------------------------------------
    // Forged custom ids
    // ---------------------------------------------------------------------

    #[test]
    fn a_forged_custom_id_is_ignored_and_reads_no_member_data() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        // A well-formed id that names a menu nobody stored: the press is
        // ignored, the member's roles are never read and nothing is written.
        let result = harness.run(&settings(), &press("rolemenu:v1:m999:r1"));
        assert_eq!(result.outcome.code, Code::MenuNotFound);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert_eq!(harness.facts.reads.load(Ordering::SeqCst), 0);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
    }

    #[test]
    fn a_malformed_custom_id_is_ignored_before_the_menu_is_read() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press("nonsense"));
        assert_eq!(result.outcome.code, Code::InvalidCustomId);
        assert_eq!(harness.store.menu_reads(), 0);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_oversized_custom_id_is_ignored_before_the_menu_is_read() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let raw = format!("rolemenu:v1:m1:r{}", "9".repeat(MAX_CUSTOM_ID_LEN));
        let result = harness.run(&settings(), &press(&raw));
        assert_eq!(result.outcome.code, Code::InvalidCustomId);
        assert_eq!(harness.store.menu_reads(), 0);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_custom_id_naming_another_menu_is_ignored_and_writes_nothing() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        // The id is well formed but names a menu that is not stored: the
        // press is ignored and nothing is written.
        let result = harness.run(&settings(), &press("rolemenu:v1:m2:r1"));
        assert_eq!(result.outcome.code, Code::MenuNotFound);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
        assert_eq!(harness.follow_ups(), vec!["menu_not_found".to_string()]);
    }

    #[test]
    fn a_button_for_a_role_of_another_menu_is_ignored() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C, ROLE_D]),
            FakeRoles::new(MEMBER, &[]),
        );
        // ROLE_D is a real role but not an option of this menu.
        let result = harness.run(&settings(), &press_option(1, ROLE_D));
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(harness.roles.added().is_empty());
    }

    // ---------------------------------------------------------------------
    // The stored row is the truth
    // ---------------------------------------------------------------------

    #[test]
    fn a_deleted_menu_answers_with_a_notice_and_writes_nothing() {
        let store = FakeStore::default();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::MenuNotFound);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
        assert_eq!(harness.follow_ups(), vec!["menu_not_found".to_string()]);
    }

    #[test]
    fn a_deleted_option_answers_with_a_notice_and_writes_nothing() {
        let mut store = multi_menu();
        store.options.get_mut(&1).expect("options").pop();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_C));
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.follow_ups(), vec!["option_not_found".to_string()]);
    }

    #[test]
    fn a_button_for_an_option_removed_from_the_front_grants_nothing() {
        // The button names the role, not the position of the option, so
        // removing the first option cannot make the old button grant the
        // option that took its place.
        let mut store = multi_menu();
        store.options.get_mut(&1).expect("options").remove(0);
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
        // The button of the option that is still there works.
        let result = harness.run(&settings(), &press_option(1, ROLE_B));
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(harness.roles.added(), vec![ROLE_B]);
    }

    #[test]
    fn a_press_on_another_message_is_refused() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut other = press_option(1, ROLE_A);
        other.message_id = OTHER_MESSAGE;
        let result = harness.run(&settings(), &other);
        assert_eq!(result.outcome.code, Code::MessageMismatch);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.follow_ups(), vec!["message_mismatch".to_string()]);
    }

    #[test]
    fn a_menu_row_of_another_guild_is_refused() {
        let store = FakeStore::with_menu(
            menu_row(1, OTHER_GUILD, Some(MESSAGE)),
            vec![option_row(1, ROLE_A, 0)],
        );
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::GuildMismatch);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn a_menu_that_was_never_posted_is_refused() {
        let store = FakeStore::with_menu(menu_row(1, HOME, None), vec![option_row(1, ROLE_A, 0)]);
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::MessageMismatch);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn a_store_failure_is_reported_with_a_stable_code() {
        let mut store = multi_menu();
        store.menu_error = Some(RepoError::Storage(crate::storage::StorageError::Sqlite));
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::StoreFailed);
        assert_eq!(result.outcome.detail, Some("sqlite_error"));
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn a_corrupt_mode_is_refused() {
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").mode = "sideways".to_string();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::CorruptRow);
        assert!(harness.roles.added().is_empty());
    }

    // ---------------------------------------------------------------------
    // Deferral
    // ---------------------------------------------------------------------

    #[test]
    fn the_deferral_happens_before_any_slow_call() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        harness.run(&settings(), &press_option(1, ROLE_A));
        let calls = harness.responder.calls();
        assert_eq!(calls[0], "defer");
        assert!(calls.len() >= 2);
        assert!(calls[1].starts_with("follow_up:"));
    }

    #[test]
    fn the_deferral_happens_before_the_menu_is_read() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        harness.run(&settings(), &press_option(1, ROLE_A));
        // The store was read after the deferral: the responder recorded the
        // deferral first and the store read could only follow it.
        assert_eq!(harness.responder.calls()[0], "defer");
        assert_eq!(harness.store.menu_reads(), 1);
    }

    #[test]
    fn a_failed_deferral_stops_the_press_before_any_write() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        harness.responder.defer_fails = true;
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::DeferFailed);
        assert_eq!(harness.store.menu_reads(), 0);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.responder.calls(), vec!["defer".to_string()]);
        // The failure is reported, not discarded.
        assert!(harness
            .reporter
            .lines
            .iter()
            .any(|line| line.contains("defer_failed")));
    }

    // ---------------------------------------------------------------------
    // Toggle, swap, max
    // ---------------------------------------------------------------------

    #[test]
    fn pressing_an_option_of_a_multi_menu_adds_the_role() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(result.added, vec![ROLE_A]);
        assert_eq!(harness.roles.added(), vec![ROLE_A]);
    }

    #[test]
    fn pressing_a_held_option_of_a_multi_menu_removes_the_role() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Removed);
        assert_eq!(result.removed, vec![ROLE_A]);
        assert_eq!(harness.roles.removed(), vec![ROLE_A]);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_exclusive_group_swaps_the_old_option_out() {
        let store = exclusive_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_B));
        assert_eq!(result.outcome.code, Code::Swapped);
        assert_eq!(result.added, vec![ROLE_B]);
        assert_eq!(result.removed, vec![ROLE_A]);
        // The removal comes first: the member ends with the option they pressed.
        assert_eq!(harness.roles.removed(), vec![ROLE_A]);
        assert_eq!(harness.roles.added(), vec![ROLE_B]);
    }

    #[test]
    fn pressing_the_held_option_of_an_exclusive_group_changes_nothing() {
        let store = exclusive_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::AlreadySelected);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
    }

    #[test]
    fn a_max_select_breach_is_refused() {
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").max_select = 2;
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A, ROLE_B]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_C));
        assert_eq!(result.outcome.code, Code::MaxReached);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
    }

    #[test]
    fn a_max_select_breach_does_not_stop_a_removal() {
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").max_select = 2;
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A, ROLE_B]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Removed);
        assert_eq!(harness.roles.removed(), vec![ROLE_A]);
    }

    #[test]
    fn a_swap_whose_removal_fails_is_reported_as_partial_and_never_as_success() {
        let store = exclusive_menu();
        let mut roles = FakeRoles::new(MEMBER, &[ROLE_A]);
        roles.remove_failures.insert(ROLE_A, vec![forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_B));
        assert_eq!(result.outcome.code, Code::SwapPartial);
        assert_eq!(result.added, vec![ROLE_B]);
        assert!(result.removed.is_empty());
        // The failure was reported, not discarded, with the classified code.
        assert!(harness
            .reporter
            .lines
            .iter()
            .any(|line| line.contains("failed:forbidden")));
        assert_eq!(result.outcome.detail, Some("forbidden"));
    }

    #[test]
    fn a_swap_whose_addition_fails_says_the_old_option_is_gone() {
        // The old option was removed and the new one could not be added: the
        // member holds neither. The answer says exactly that.
        let store = exclusive_menu();
        let mut roles = FakeRoles::new(MEMBER, &[ROLE_A]);
        roles.add_failures.insert(ROLE_B, vec![forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_B));
        assert_eq!(result.outcome.code, Code::SwapPartialRemoved);
        assert_eq!(result.outcome.detail, Some("forbidden"));
        assert!(result.added.is_empty());
        assert_eq!(result.removed, vec![ROLE_A]);
        // The member really holds neither option now.
        assert!(harness
            .roles
            .held
            .get(&MEMBER)
            .expect("held")
            .iter()
            .all(|role| *role != ROLE_A && *role != ROLE_B));
    }

    #[test]
    fn a_swap_that_is_blocked_changes_nothing() {
        let store = exclusive_menu();
        // The old option sits at the bot's own height: it cannot be removed.
        let mut facts = FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]);
        facts.facts.insert(
            ROLE_A,
            RoleFacts {
                id: ROLE_A,
                position: BOT_TOP,
                managed: false,
                permissions: 0,
                exists: true,
            },
        );
        let mut harness = Harness::new(store, facts, FakeRoles::new(MEMBER, &[ROLE_A]));
        let result = harness.run(&settings(), &press_option(1, ROLE_B));
        assert_eq!(result.outcome.code, Code::SwapBlocked);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
    }

    // ---------------------------------------------------------------------
    // The select path
    // ---------------------------------------------------------------------

    #[test]
    fn a_select_with_one_value_adds_that_option() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_select(1, &[ROLE_A]));
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(result.added, vec![ROLE_A]);
        assert_eq!(harness.roles.added(), vec![ROLE_A]);
    }

    #[test]
    fn a_select_with_two_values_adds_both_in_a_multi_group() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_select(1, &[ROLE_A, ROLE_B]));
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(result.added, vec![ROLE_A, ROLE_B]);
        assert_eq!(harness.roles.added(), vec![ROLE_A, ROLE_B]);
    }

    #[test]
    fn a_select_with_two_values_in_an_exclusive_group_is_refused() {
        let store = exclusive_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_select(1, &[ROLE_A, ROLE_B]));
        assert_eq!(result.outcome.code, Code::TooManySelected);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
    }

    #[test]
    fn a_select_value_of_another_menu_is_refused_and_writes_nothing() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C, ROLE_D]),
            FakeRoles::new(MEMBER, &[]),
        );
        // ROLE_D is a real role but not an option of this menu.
        let result = harness.run(&settings(), &press_select(1, &[ROLE_D]));
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_select_value_that_is_not_a_role_id_is_refused_and_writes_nothing() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut bad = press_select(1, &[ROLE_A]);
        bad.values = vec!["not-a-role".to_string()];
        let result = harness.run(&settings(), &bad);
        assert_eq!(result.outcome.code, Code::InvalidValue);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_select_with_a_duplicated_value_is_refused_and_writes_nothing() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_select(1, &[ROLE_A, ROLE_A]));
        assert_eq!(result.outcome.code, Code::InvalidValue);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn a_select_with_more_values_than_a_select_may_hold_is_refused() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let values: Vec<u64> = (0..=policy::SELECT_OPTION_LIMIT as u64)
            .map(|i| 780_000 + i)
            .collect();
        let result = harness.run(&settings(), &press_select(1, &values));
        assert_eq!(result.outcome.code, Code::TooManySelected);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_select_with_no_value_changes_nothing() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_select(1, &[]));
        assert_eq!(result.outcome.code, Code::NoChange);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
    }

    #[test]
    fn a_select_that_toggles_a_held_option_removes_it() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A]),
        );
        let result = harness.run(&settings(), &press_select(1, &[ROLE_A]));
        assert_eq!(result.outcome.code, Code::Removed);
        assert_eq!(result.removed, vec![ROLE_A]);
    }

    #[test]
    fn a_select_where_one_value_is_refused_says_it_was_partial() {
        // The safeword blocks ROLE_A but not ROLE_B: one value is applied and
        // one is refused, and the answer says so.
        let store = multi_menu();
        let mut settings = settings();
        settings.safeword_restricted_role_ids = vec![ROLE_A];
        let mut blocked = press_select(1, &[ROLE_A, ROLE_B]);
        blocked.safeword = SafewordState::Active;
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings, &blocked);
        assert_eq!(result.outcome.code, Code::Partial);
        assert_eq!(result.outcome.detail, Some("safeword_restricted"));
        assert_eq!(result.added, vec![ROLE_B]);
        assert_eq!(harness.roles.added(), vec![ROLE_B]);
    }

    #[test]
    fn a_select_for_a_deleted_option_is_refused_and_writes_nothing() {
        let mut store = multi_menu();
        store
            .options
            .get_mut(&1)
            .expect("options")
            .retain(|o| o.role != ROLE_A);
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_select(1, &[ROLE_A]));
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(harness.roles.added().is_empty());
    }

    // ---------------------------------------------------------------------
    // K03 at press time
    // ---------------------------------------------------------------------

    #[test]
    fn a_protected_role_is_refused_at_press_time() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut settings = settings();
        settings.protected_role_ids = vec![ROLE_A];
        let result = harness.run(&settings, &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Ineligible);
        assert_eq!(result.outcome.detail, Some("protected_role"));
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn a_managed_role_is_refused_at_press_time() {
        let store = multi_menu();
        let mut facts = FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]);
        facts.facts.insert(
            ROLE_A,
            RoleFacts {
                id: ROLE_A,
                position: 5,
                managed: true,
                permissions: 0,
                exists: true,
            },
        );
        let mut harness = Harness::new(store, facts, FakeRoles::new(MEMBER, &[]));
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Ineligible);
        assert_eq!(result.outcome.detail, Some("managed_role"));
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn a_privileged_role_is_refused_at_press_time() {
        let store = multi_menu();
        let mut facts = FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]);
        facts.facts.insert(
            ROLE_A,
            RoleFacts {
                id: ROLE_A,
                position: 5,
                managed: false,
                permissions: ADMINISTRATOR | MANAGE_ROLES,
                exists: true,
            },
        );
        let mut harness = Harness::new(store, facts, FakeRoles::new(MEMBER, &[]));
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Ineligible);
        assert_eq!(result.outcome.detail, Some("privileged_permission"));
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_unreadable_protected_role_list_refuses_every_press() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut settings = settings();
        settings.protected_roles_invalid = true;
        let result = harness.run(&settings, &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Ineligible);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_active_safeword_blocks_a_restricted_option() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut settings = settings();
        settings.safeword_restricted_role_ids = vec![ROLE_A];
        let mut blocked = press_option(1, ROLE_A);
        blocked.safeword = SafewordState::Active;
        let result = harness.run(&settings, &blocked);
        assert_eq!(result.outcome.code, Code::Ineligible);
        assert_eq!(result.outcome.detail, Some("safeword_restricted"));
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_active_safeword_does_not_block_an_unrestricted_option() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut settings = settings();
        settings.safeword_restricted_role_ids = vec![ROLE_C];
        let mut other = press_option(1, ROLE_A);
        other.safeword = SafewordState::Active;
        let result = harness.run(&settings, &other);
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(harness.roles.added(), vec![ROLE_A]);
    }

    #[test]
    fn a_role_that_was_deleted_is_refused() {
        let store = multi_menu();
        let facts = FakeFacts::new(&[ROLE_B, ROLE_C]);
        let mut harness = Harness::new(store, facts, FakeRoles::new(MEMBER, &[]));
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Ineligible);
        assert_eq!(result.outcome.detail, Some("role_deleted"));
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn the_handler_writes_only_the_roles_the_policy_planned() {
        // The policy's plan is the only thing that authorizes a write. A role
        // that is not an option of the menu is never written, and the roles
        // that are written are exactly the ones the outcome names.
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(1, ROLE_B));
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(harness.roles.added(), result.added);
        assert_eq!(harness.roles.removed(), result.removed);
        assert!(harness
            .roles
            .added()
            .iter()
            .all(|role| [ROLE_A, ROLE_B, ROLE_C].contains(role)));
    }

    #[test]
    fn a_role_outside_the_menu_is_never_written() {
        // The pressed option is the only role the press may touch: a role
        // that no option lists is not written.
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C, ROLE_D]),
            FakeRoles::new(MEMBER, &[]),
        );
        harness.run(&settings(), &press_option(1, ROLE_A));
        assert!(!harness.roles.added().contains(&ROLE_D));
        assert!(!harness.roles.removed().contains(&ROLE_D));
    }

    #[test]
    fn an_unavailable_facts_lookup_is_refused() {
        let store = multi_menu();
        let mut facts = FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]);
        facts.fail = Some(LookupFail::Unavailable);
        let mut harness = Harness::new(store, facts, FakeRoles::new(MEMBER, &[]));
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::FactsUnavailable);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_unavailable_member_role_lookup_is_refused() {
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[]);
        roles.member_roles_fail = Some(LookupFail::Unavailable);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::MemberRolesUnavailable);
        assert!(harness.roles.added().is_empty());
    }

    // ---------------------------------------------------------------------
    // Write failures
    // ---------------------------------------------------------------------

    #[test]
    fn a_failed_add_is_reported_with_a_stable_code() {
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[]);
        roles.add_failures.insert(ROLE_A, vec![forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::WriteFailed);
        assert!(result.added.is_empty());
        assert!(harness
            .reporter
            .lines
            .iter()
            .any(|line| line.contains("failed:forbidden")));
    }

    #[test]
    fn the_member_facing_fail_code_is_the_classified_one() {
        // A 403 is not reported as `other`.
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[]);
        roles.add_failures.insert(ROLE_A, vec![forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.detail, Some("forbidden"));
        assert_eq!(result.outcome.text(), "write_failed (forbidden)");
    }

    #[test]
    fn a_rate_limited_add_is_retried_and_then_succeeds() {
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[]);
        roles
            .add_failures
            .insert(ROLE_A, vec![rate_limited(Some(25))]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(result.added, vec![ROLE_A]);
        // One attempt plus one retry.
        assert_eq!(harness.roles.add_call_count(ROLE_A), 2);
        assert_eq!(harness.sleeper.slept(), vec![25]);
    }

    #[test]
    fn a_rate_limited_add_that_never_succeeds_is_reported_as_rate_limited() {
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[]);
        roles.add_failures.insert(
            ROLE_A,
            vec![
                rate_limited(None),
                rate_limited(None),
                rate_limited(None),
                rate_limited(None),
                rate_limited(None),
            ],
        );
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::WriteFailed);
        assert_eq!(result.outcome.detail, Some("rate_limited"));
        // One attempt plus three retries, then it gives up.
        assert_eq!(harness.roles.add_call_count(ROLE_A), 4);
    }

    #[test]
    fn a_forbidden_add_is_not_retried() {
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[]);
        roles
            .add_failures
            .insert(ROLE_A, vec![forbidden(), forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(harness.roles.add_call_count(ROLE_A), 1);
        assert!(harness.sleeper.slept().is_empty());
    }

    #[test]
    fn a_rate_limited_removal_is_retried_and_then_succeeds() {
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[ROLE_A]);
        roles
            .remove_failures
            .insert(ROLE_A, vec![rate_limited(Some(10))]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Removed);
        assert_eq!(harness.roles.remove_call_count(ROLE_A), 2);
        assert_eq!(harness.sleeper.slept(), vec![10]);
    }

    #[test]
    fn a_failed_add_never_echoes_a_role_name_or_member_text() {
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[]);
        roles.add_failures.insert(ROLE_A, vec![forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        harness.run(&settings(), &press_option(1, ROLE_A));
        let follow_ups = harness.follow_ups();
        assert_eq!(follow_ups, vec!["write_failed (forbidden)".to_string()]);
        for line in &harness.reporter.lines {
            assert!(!line.contains("label_"));
            assert!(!line.contains('\u{1F600}'));
        }
    }

    #[test]
    fn a_failed_follow_up_is_reported_and_not_discarded() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        harness.responder.follow_up_fails = true;
        let result = harness.run(&settings(), &press_option(1, ROLE_A));
        // The role was really added; the answer was lost and that is reported.
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(harness.roles.added(), vec![ROLE_A]);
        assert!(harness
            .reporter
            .lines
            .iter()
            .any(|line| line.contains("follow_up_failed")));
    }

    #[test]
    fn a_failed_follow_up_of_a_notice_is_reported() {
        let store = FakeStore::default();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A]),
            FakeRoles::new(MEMBER, &[]),
        );
        harness.responder.follow_up_fails = true;
        harness.run(&settings(), &press_option(1, ROLE_A));
        assert!(harness
            .reporter
            .lines
            .iter()
            .any(|line| line.contains("follow_up_failed")));
    }

    // ---------------------------------------------------------------------
    // Shadow mode
    // ---------------------------------------------------------------------

    #[test]
    fn shadow_mode_writes_nothing_and_says_what_it_would_do() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut settings = settings();
        settings.mode = Mode::Shadow;
        let result = harness.run(&settings, &press_option(1, ROLE_A));
        assert_eq!(result.outcome.code, Code::Added);
        assert!(result.shadow);
        assert_eq!(result.added, vec![ROLE_A]);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
        assert!(harness.roles.held.get(&MEMBER).expect("held").is_empty());
    }

    #[test]
    fn shadow_mode_writes_nothing_for_a_select_either() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut settings = settings();
        settings.mode = Mode::Shadow;
        let result = harness.run(&settings, &press_select(1, &[ROLE_A, ROLE_B]));
        assert!(result.shadow);
        assert_eq!(result.added, vec![ROLE_A, ROLE_B]);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
    }

    // ---------------------------------------------------------------------
    // Per-member serialization
    // ---------------------------------------------------------------------

    /// A store that records every read into a shared log.
    struct LoggingStore {
        inner: FakeStore,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl MenuStore for LoggingStore {
        fn get_menu_by_id(&self, menu_id: i64) -> Result<Option<MenuRow>, RepoError> {
            self.log
                .lock()
                .expect("log")
                .push(format!("read_menu:{menu_id}"));
            self.inner.get_menu_by_id(menu_id)
        }
        fn list_options(&self, menu_id: i64) -> Result<Vec<OptionRow>, RepoError> {
            self.log
                .lock()
                .expect("log")
                .push(format!("read_options:{menu_id}"));
            self.inner.list_options(menu_id)
        }
    }

    /// A role service that records every write into a shared log.
    struct LoggingRoles {
        inner: FakeRoles,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl grant::RoleWriter for LoggingRoles {
        fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.log
                .lock()
                .expect("log")
                .push(format!("write:{role_id}"));
            self.inner.add_role(member_id, role_id)
        }
    }

    impl RoleRemover for LoggingRoles {
        fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.log
                .lock()
                .expect("log")
                .push(format!("write:{role_id}"));
            self.inner.remove_role(member_id, role_id)
        }
    }

    impl RoleService for LoggingRoles {
        fn member_roles(&self, guild_id: u64, member_id: u64) -> Result<Vec<u64>, LookupFail> {
            self.inner.member_roles(guild_id, member_id)
        }
    }

    #[test]
    fn two_presses_of_one_member_do_not_interleave() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let store = Arc::new(LoggingStore {
            inner: multi_menu(),
            log: Arc::clone(&log),
        });
        let facts = Arc::new(FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]));
        let locks = Arc::new(MemberLocks::new());
        let settings = Arc::new(settings());

        // Two threads press the same member at the same time.
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|index| {
                let store = Arc::clone(&store);
                let facts = Arc::clone(&facts);
                let locks = Arc::clone(&locks);
                let settings = Arc::clone(&settings);
                let barrier = Arc::clone(&barrier);
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    let mut local_roles = LoggingRoles {
                        inner: FakeRoles::new(MEMBER, &[]),
                        log: Arc::clone(&log),
                    };
                    let mut reporter = MemoryReporter::default();
                    let mut responder = FakeResponder::default();
                    let mut sleeper = FakeSleeper::default();
                    let mut ports = PressPorts {
                        store: store.as_ref(),
                        facts: facts.as_ref(),
                        roles: &mut local_roles,
                        sleeper: &mut sleeper,
                        reporter: &mut reporter,
                        responder: &mut responder,
                    };
                    barrier.wait();
                    handle_press(
                        settings.as_ref(),
                        locks.as_ref(),
                        &press_option(1, [ROLE_A, ROLE_B][index]),
                        &mut ports,
                    )
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread");
        }

        // The two presses never overlap. The log holds one block per press:
        // the press's two reads, then its write. A press that read its menu
        // while the other press was between its own read and its write would
        // show a `read_menu` in the middle of the other press's block.
        let events = log.lock().expect("log").clone();
        let reads = events.iter().filter(|e| e.starts_with("read_")).count();
        let writes = events.iter().filter(|e| e.starts_with("write:")).count();
        assert_eq!(reads, 4, "{events:?}");
        assert_eq!(writes, 2, "{events:?}");
        // The first press's block: its menu read, its options read, its write.
        assert_eq!(events[0], "read_menu:1", "{events:?}");
        assert_eq!(events[1], "read_options:1", "{events:?}");
        assert!(events[2].starts_with("write:"), "{events:?}");
        // The second press's block starts only after the first press's write.
        assert_eq!(events[3], "read_menu:1", "{events:?}");
        assert_eq!(events[4], "read_options:1", "{events:?}");
        assert!(events[5].starts_with("write:"), "{events:?}");
        // No read sits between a press's own options read and its write.
        for (index, event) in events.iter().enumerate() {
            if event.starts_with("write:") {
                let before = &events[..index];
                let menu_reads = before.iter().filter(|e| e.starts_with("read_menu")).count();
                let options_reads = before
                    .iter()
                    .filter(|e| e.starts_with("read_options"))
                    .count();
                assert_eq!(
                    menu_reads, options_reads,
                    "a press read its options without its menu: {events:?}"
                );
            }
        }
    }

    #[test]
    fn many_presses_of_one_member_never_overlap() {
        // The lock must hold under a real race: many threads press the same
        // member at once and no two critical sections may overlap.
        let locks = Arc::new(MemberLocks::new());
        let inside = Arc::new(AtomicUsize::new(0));
        let overlap = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let locks = Arc::clone(&locks);
            let inside = Arc::clone(&inside);
            let overlap = Arc::clone(&overlap);
            let stop = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                for _ in 0..2_000 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    locks.with_member(MEMBER, || {
                        if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                            overlap.fetch_add(1, Ordering::SeqCst);
                            stop.store(true, Ordering::Relaxed);
                        }
                        std::hint::spin_loop();
                        inside.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }));
        }
        for handle in handles {
            handle.join().expect("join");
        }
        assert_eq!(
            overlap.load(Ordering::SeqCst),
            0,
            "two presses of one member ran at the same time"
        );
        assert_eq!(locks.held(), 0, "the lock was left held");
    }

    #[test]
    fn two_presses_of_two_members_do_not_block_each_other() {
        let locks = Arc::new(MemberLocks::new());
        let inner = Arc::new(Mutex::new(0usize));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|member| {
                let locks = Arc::clone(&locks);
                let inner = Arc::clone(&inner);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    locks.with_member(member, || {
                        *inner.lock().expect("inner") += 1;
                    })
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread");
        }
        assert_eq!(*inner.lock().expect("inner"), 2);
        assert_eq!(locks.held(), 0);
    }

    #[test]
    fn the_member_lock_is_released_after_a_press() {
        let locks = MemberLocks::new();
        locks.with_member(MEMBER, || {});
        assert_eq!(locks.held(), 0);
        let mut ran = false;
        locks.with_member(MEMBER, || ran = true);
        assert!(ran);
    }

    #[test]
    fn the_member_lock_is_released_after_a_failing_press() {
        let locks = MemberLocks::new();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            locks.with_member(MEMBER, || panic!("boom"));
        }));
        assert_eq!(locks.held(), 0);
    }

    #[test]
    fn the_member_lock_is_released_after_a_press_that_is_refused() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        harness.run(&settings(), &press_option(1, ROLE_A));
        assert_eq!(harness.locks.held(), 0);
    }

    // ---------------------------------------------------------------------
    // The real repository
    // ---------------------------------------------------------------------

    /// The real repository over an in-memory SQLite database.
    struct RealStore {
        conn: Connection,
    }

    impl RealStore {
        fn new() -> Self {
            let mut conn = Connection::open_in_memory().expect("in-memory db");
            crate::role_menus::repository::migrate(&mut conn).expect("migrate");
            RealStore { conn }
        }
    }

    impl MenuStore for RealStore {
        fn get_menu_by_id(&self, menu_id: i64) -> Result<Option<MenuRow>, RepoError> {
            crate::role_menus::repository::get_menu_by_id(&self.conn, menu_id)
        }
        fn list_options(&self, menu_id: i64) -> Result<Vec<OptionRow>, RepoError> {
            crate::role_menus::repository::list_options(&self.conn, menu_id)
        }
    }

    #[test]
    fn the_real_repository_serves_a_press() {
        use crate::role_menus::repository;
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        repository::migrate(&mut conn).expect("migrate");
        let menu_id = repository::create_menu(&conn, HOME, "main", "multi", 0).expect("menu");
        repository::add_option(&conn, menu_id, ROLE_A, "label_a", "\u{1F600}").expect("option");
        repository::set_published(&conn, menu_id, 770_030, MESSAGE, None).expect("published");

        let store = RealStore { conn };
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(menu_id, ROLE_A));
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(harness.roles.added(), vec![ROLE_A]);
    }

    #[test]
    fn the_real_repository_refuses_a_deleted_option_and_writes_nothing() {
        use crate::role_menus::repository;
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        repository::migrate(&mut conn).expect("migrate");
        let menu_id = repository::create_menu(&conn, HOME, "main", "multi", 0).expect("menu");
        repository::add_option(&conn, menu_id, ROLE_A, "label_a", "\u{1F600}").expect("option");
        repository::set_published(&conn, menu_id, 770_030, MESSAGE, None).expect("published");
        repository::remove_option(&conn, menu_id, ROLE_A).expect("removed");

        let store = RealStore { conn };
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(menu_id, ROLE_A));
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.follow_ups(), vec!["option_not_found".to_string()]);
    }

    #[test]
    fn the_real_repository_refuses_a_stale_button_after_the_first_option_is_removed() {
        use crate::role_menus::repository;
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        repository::migrate(&mut conn).expect("migrate");
        let menu_id = repository::create_menu(&conn, HOME, "main", "multi", 0).expect("menu");
        repository::add_option(&conn, menu_id, ROLE_A, "label_a", "\u{1F600}").expect("a");
        repository::add_option(&conn, menu_id, ROLE_B, "label_b", "\u{1F600}").expect("b");
        repository::add_option(&conn, menu_id, ROLE_C, "label_c", "\u{1F600}").expect("c");
        repository::set_published(&conn, menu_id, 770_030, MESSAGE, None).expect("published");
        // The button the member still sees for ROLE_A.
        let old_button = custom_id_for_option(menu_id, ROLE_A).expect("id");
        repository::remove_option(&conn, menu_id, ROLE_A).expect("removed");

        let store = RealStore { conn };
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press(&old_button));
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(
            harness.roles.added().is_empty(),
            "the stale button granted a role"
        );
    }

    #[test]
    fn the_real_repository_refuses_a_deleted_menu_and_writes_nothing() {
        use crate::role_menus::repository;
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        repository::migrate(&mut conn).expect("migrate");
        let menu_id = repository::create_menu(&conn, HOME, "main", "multi", 0).expect("menu");
        repository::add_option(&conn, menu_id, ROLE_A, "label_a", "\u{1F600}").expect("option");
        repository::set_published(&conn, menu_id, 770_030, MESSAGE, None).expect("published");
        repository::delete_menu(&conn, menu_id).expect("deleted");

        let store = RealStore { conn };
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_option(menu_id, ROLE_A));
        assert_eq!(result.outcome.code, Code::MenuNotFound);
        assert!(harness.roles.added().is_empty());
    }

    // ---------------------------------------------------------------------
    // Liveness
    // ---------------------------------------------------------------------

    /// Seeded LCG; no dependency, no real randomness.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    /// Liveness sweep: a bounded, seeded sequence of presses against a model
    /// member. Every press must answer exactly once, the answer must never
    /// claim a success that did not happen, and the lock must never stay held.
    #[test]
    fn a_seeded_sequence_of_presses_always_answers_and_never_claims_a_false_success() {
        let mut rng = Lcg(0x5EED_CAFE);
        for case in 0..200u32 {
            let mut store = multi_menu();
            let mode = if rng.next(2) == 0 {
                "multi"
            } else {
                "exclusive"
            };
            store.menus.get_mut(&1).expect("menu").mode = mode.to_string();
            store.menus.get_mut(&1).expect("menu").max_select = rng.next(3) as u32;
            // Every third case has a failing write, on the role that case
            // presses first.
            let failing = rng.next(3) == 0;
            let failing_role = [ROLE_A, ROLE_B, ROLE_C][rng.next(3) as usize];
            let mut roles = FakeRoles::new(MEMBER, &[]);
            if failing {
                roles.add_failures.insert(failing_role, vec![forbidden()]);
            }
            let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
            // The model member: the roles it holds after the presses.
            let mut model: Vec<u64> = Vec::new();
            for step in 0..8u32 {
                let index = rng.next(3) as usize;
                let role = [ROLE_A, ROLE_B, ROLE_C][index];
                let lines_before = harness.reporter.lines.len();
                let result = harness.run(&settings(), &press_option(1, role));
                // The press always answers exactly once.
                assert_eq!(
                    harness.follow_ups().len(),
                    (step + 1) as usize,
                    "case {case}"
                );
                // A press that really tried to add the failing role must not
                // be reported as a success. Only the lines of this press
                // count.
                let add_was_attempted = harness.reporter.lines[lines_before..]
                    .iter()
                    .any(|line| line.contains(&format!("role={role}:failed:forbidden")));
                match result.outcome.code {
                    Code::Added | Code::Swapped | Code::Removed => {
                        assert!(
                            !add_was_attempted,
                            "case {case}: a failed write was reported as a success"
                        );
                    }
                    Code::AlreadySelected
                    | Code::MaxReached
                    | Code::SwapBlocked
                    | Code::Ineligible
                    | Code::WriteFailed
                    | Code::SwapPartial
                    | Code::SwapPartialRemoved
                    | Code::NoChange => {}
                    other => panic!("case {case}: unexpected {other:?}"),
                }
                // The model follows the roles the handler actually added and
                // removed.
                for removed in &result.removed {
                    model.retain(|r| r != removed);
                }
                for added in &result.added {
                    model.retain(|r| r != added);
                    model.push(*added);
                }
                // The lock is never held after a press.
                assert_eq!(harness.locks.held(), 0, "case {case}");
            }
            // The member ends with exactly the roles the model says it has.
            let held = harness.roles.held.get(&MEMBER).expect("held").clone();
            let mut expected = model;
            expected.sort_unstable();
            expected.dedup();
            let mut actual = held;
            actual.sort_unstable();
            actual.dedup();
            assert_eq!(actual, expected, "case {case}");
        }
    }

    /// Liveness sweep over a failing write: the handler must always answer
    /// and must never report a success that the writer refused.
    #[test]
    fn a_seeded_sequence_of_presses_with_failing_writes_never_claims_success() {
        let mut rng = Lcg(0xC0FFEE);
        for case in 0..200u32 {
            let store = multi_menu();
            let mut roles = FakeRoles::new(MEMBER, &[]);
            let role = [ROLE_A, ROLE_B, ROLE_C][rng.next(3) as usize];
            roles.add_failures.insert(role, vec![forbidden()]);
            let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
            let result = harness.run(&settings(), &press_option(1, role));
            assert!(
                result.outcome.code != Code::Added && result.outcome.code != Code::Swapped,
                "case {case}: a failed write was reported as a success"
            );
            assert!(result.added.is_empty(), "case {case}");
            assert_eq!(harness.follow_ups().len(), 1, "case {case}");
            assert!(
                harness
                    .reporter
                    .lines
                    .iter()
                    .any(|line| line.contains("failed:forbidden")),
                "case {case}: the failure was discarded"
            );
        }
    }

    /// Liveness sweep over selects: every submitted value is either applied
    /// or refused, the member never holds more than the group allows, and the
    /// press always answers exactly once.
    #[test]
    fn a_seeded_sequence_of_selects_keeps_every_invariant() {
        let mut rng = Lcg(0x5E1EC7);
        for case in 0..120u32 {
            let mut store = multi_menu();
            store.menus.get_mut(&1).expect("menu").mode = "multi".to_string();
            store.menus.get_mut(&1).expect("menu").max_select = 2;
            let mut harness = Harness::new(
                store,
                FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
                FakeRoles::new(MEMBER, &[]),
            );
            let mut held: Vec<u64> = Vec::new();
            for step in 0..10u32 {
                let count = 1 + rng.next(3) as usize;
                let mut values: Vec<u64> = Vec::new();
                for _ in 0..count {
                    let role = [ROLE_A, ROLE_B, ROLE_C][rng.next(3) as usize];
                    if !values.contains(&role) {
                        values.push(role);
                    }
                }
                let result = harness.run(&settings(), &press_select(1, &values));
                assert_eq!(
                    harness.follow_ups().len(),
                    (step + 1) as usize,
                    "case {case} step {step}"
                );
                for removed in &result.removed {
                    held.retain(|role| role != removed);
                }
                for added in &result.added {
                    held.retain(|role| role != added);
                    held.push(*added);
                }
                // The group holds at most `max` options.
                assert!(
                    held.len() <= 2,
                    "case {case} step {step}: the member holds {held:?}"
                );
                // Every role the handler wrote is a stored option.
                for role in result.added.iter().chain(result.removed.iter()) {
                    assert!(
                        [ROLE_A, ROLE_B, ROLE_C].contains(role),
                        "case {case}: a role outside the menu was written"
                    );
                }
            }
            assert_eq!(harness.locks.held(), 0, "case {case}");
        }
    }

    // ---------------------------------------------------------------------
    // Codes
    // ---------------------------------------------------------------------

    #[test]
    fn the_codes_are_stable() {
        assert_eq!(Code::Disabled.as_str(), "disabled");
        assert_eq!(Code::OtherGuild.as_str(), "other_guild");
        assert_eq!(Code::NoHomeGuild.as_str(), "no_home_guild");
        assert_eq!(Code::InvalidCustomId.as_str(), "invalid_custom_id");
        assert_eq!(Code::InvalidValue.as_str(), "invalid_value");
        assert_eq!(Code::MenuNotFound.as_str(), "menu_not_found");
        assert_eq!(Code::OptionNotFound.as_str(), "option_not_found");
        assert_eq!(Code::MessageMismatch.as_str(), "message_mismatch");
        assert_eq!(Code::GuildMismatch.as_str(), "guild_mismatch");
        assert_eq!(Code::CorruptRow.as_str(), "corrupt_row");
        assert_eq!(Code::StoreFailed.as_str(), "store_failed");
        assert_eq!(Code::DeferFailed.as_str(), "defer_failed");
        assert_eq!(Code::FactsUnavailable.as_str(), "facts_unavailable");
        assert_eq!(
            Code::MemberRolesUnavailable.as_str(),
            "member_roles_unavailable"
        );
        assert_eq!(Code::Ineligible.as_str(), "ineligible");
        assert_eq!(Code::Added.as_str(), "added");
        assert_eq!(Code::Removed.as_str(), "removed");
        assert_eq!(Code::Swapped.as_str(), "swapped");
        assert_eq!(Code::AlreadySelected.as_str(), "already_selected");
        assert_eq!(Code::MaxReached.as_str(), "max_reached");
        assert_eq!(Code::UnknownOption.as_str(), "unknown_option");
        assert_eq!(Code::TooManySelected.as_str(), "too_many_selected");
        assert_eq!(Code::SwapBlocked.as_str(), "swap_blocked");
        assert_eq!(Code::SwapPartial.as_str(), "swap_partial");
        assert_eq!(Code::SwapPartialRemoved.as_str(), "swap_partial_removed");
        assert_eq!(Code::WriteFailed.as_str(), "write_failed");
        assert_eq!(Code::Partial.as_str(), "partial");
        assert_eq!(Code::NoChange.as_str(), "no_change");
    }

    #[test]
    fn the_outcome_text_is_a_code_and_a_detail_only() {
        assert_eq!(Outcome::new(Code::Added).text(), "added");
        assert_eq!(
            Outcome::with(Code::Ineligible, "protected_role").text(),
            "ineligible (protected_role)"
        );
    }

    // ---------------------------------------------------------------------
    // Round 3: the select part, the refusal code and the partial codes
    // ---------------------------------------------------------------------

    /// A menu with more options than one select component may hold.
    ///
    /// The options are numbered from `ROLE_A` upward, so the first option of
    /// the second part is the one after the first 25 options.
    fn big_menu(option_count: usize) -> FakeStore {
        let options: Vec<OptionRow> = (0..option_count)
            .map(|i| option_row(1, ROLE_A + i as u64, i as i64))
            .collect();
        FakeStore::with_menu(menu_row(1, HOME, Some(MESSAGE)), options)
    }

    #[test]
    fn two_parts_of_one_menu_have_two_different_component_ids() {
        let first = custom_id_for_select(7, 0).expect("built");
        let second = custom_id_for_select(7, 1).expect("built");
        assert_ne!(first, second, "Discord wants different ids in one message");
        assert_eq!(first, "rolemenu:v1:m7:s0");
        assert_eq!(second, "rolemenu:v1:m7:s1");
    }

    #[test]
    fn every_part_of_a_menu_with_more_than_twenty_five_options_parses() {
        // 30 options: two select components in one message.
        let store = big_menu(30);
        let chunks = command::split_chunks(store.options.get(&1).expect("options"));
        assert_eq!(chunks.len(), 2);
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_A + 25, ROLE_A + 29]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut ids = Vec::new();
        for chunk in chunks {
            let id = custom_id_for_select(1, chunk.part).expect("built");
            let parsed = parse_custom_id(&id).expect("parsed");
            assert_eq!(parsed.menu_id, 1);
            assert_eq!(parsed.selector, Selector::Select(chunk.part));
            ids.push(id);
        }
        assert_ne!(ids[0], ids[1]);
        // The first option of the second part belongs to the second part.
        let mut press = press_select_part(1, 1, &[ROLE_A + 25]);
        press.values = vec![(ROLE_A + 25).to_string()];
        let result = harness.run(&settings(), &press);
        assert_eq!(result.outcome.code, Code::Added);
        assert_eq!(result.added, vec![ROLE_A + 25]);
    }

    #[test]
    fn a_value_of_another_part_is_refused_and_writes_nothing() {
        // ROLE_A is in the first part; part 1 holds the options from
        // ROLE_A + 25 on. A value of another part names no option of this
        // component.
        let store = big_menu(30);
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_A + 29]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut press = press_select_part(1, 1, &[ROLE_A]);
        press.values = vec![ROLE_A.to_string()];
        let result = harness.run(&settings(), &press);
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_options_of_a_part_are_the_ones_of_that_chunk() {
        let store = big_menu(30);
        let options = store.options.get(&1).expect("options");
        assert_eq!(
            options_of_part(options, 0).len(),
            policy::SELECT_OPTION_LIMIT
        );
        assert_eq!(options_of_part(options, 1).len(), 5);
        assert!(options_of_part(options, 2).is_empty());
        assert!(options_of_part(options, 9_999).is_empty());
    }

    #[test]
    fn a_refused_select_value_keeps_its_own_code() {
        // The member holds ROLE_A and the group allows two options. A select
        // of ROLE_B and ROLE_C adds ROLE_B and then hits the limit on
        // ROLE_C: the answer says which refusal it was.
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").max_select = 2;
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A]),
        );
        let result = harness.run(&settings(), &press_select(1, &[ROLE_B, ROLE_C]));
        assert_eq!(result.outcome.code, Code::Partial);
        assert_eq!(result.outcome.detail, Some("max_reached"));
        assert_eq!(result.added, vec![ROLE_B]);
    }

    #[test]
    fn a_refused_select_value_keeps_its_own_code_in_shadow_mode() {
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").max_select = 2;
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A]),
        );
        let mut settings = settings();
        settings.mode = Mode::Shadow;
        let result = harness.run(&settings, &press_select(1, &[ROLE_B, ROLE_C]));
        assert_eq!(result.outcome.code, Code::Partial);
        assert_eq!(result.outcome.detail, Some("max_reached"));
        assert!(result.shadow);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn the_first_refusal_is_the_one_that_is_reported() {
        // Two values, both refused: the first refusal is the one the member
        // is told about, so the answer names one stable reason and not a
        // blend of two.
        let store = multi_menu();
        let mut settings = settings();
        settings.safeword_restricted_role_ids = vec![ROLE_A, ROLE_B];
        let mut blocked = press_select(1, &[ROLE_A, ROLE_B]);
        blocked.safeword = SafewordState::Active;
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings, &blocked);
        // Nothing was applied, so this is not a partial press: the refusal
        // itself is the answer.
        assert_eq!(result.outcome.code, Code::Ineligible);
        assert_eq!(result.outcome.detail, Some("safeword_restricted"));
        assert!(result.added.is_empty());
        assert!(result.removed.is_empty());
    }

    #[test]
    fn the_first_refusal_is_the_one_that_is_reported_when_something_was_applied() {
        // A hits safeword, B is applied, and C hits max-select. The two
        // refusals differ, so dropping the first changes the reply.
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").max_select = 1;
        let mut settings = settings();
        settings.safeword_restricted_role_ids = vec![ROLE_A];
        let mut blocked = press_select(1, &[ROLE_A, ROLE_B, ROLE_C]);
        blocked.safeword = SafewordState::Active;
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings, &blocked);
        assert_eq!(result.outcome.code, Code::Partial);
        assert_eq!(result.outcome.detail, Some("safeword_restricted"));
        assert_eq!(result.added, vec![ROLE_B]);
    }

    #[test]
    fn a_multi_select_whose_addition_fails_is_not_reported_as_a_swap() {
        // A multi group: ROLE_A is held and removed, ROLE_B is added but the
        // write fails. No swap was planned, so the answer must not claim one.
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[ROLE_A]);
        roles.add_failures.insert(ROLE_B, vec![forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_select(1, &[ROLE_A, ROLE_B]));
        assert_eq!(result.outcome.code, Code::PartialWrite);
        assert_eq!(result.outcome.detail, Some("forbidden"));
        assert_eq!(result.removed, vec![ROLE_A]);
        assert!(result.added.is_empty());
    }

    #[test]
    fn a_multi_select_whose_removal_fails_is_not_reported_as_a_swap() {
        // Both values are held, so both are removals and no swap was
        // planned. ROLE_B's removal fails.
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[ROLE_A, ROLE_B]);
        roles.remove_failures.insert(ROLE_B, vec![forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_select(1, &[ROLE_A, ROLE_B]));
        assert_eq!(result.outcome.code, Code::PartialWrite);
        assert_eq!(result.outcome.detail, Some("forbidden"));
        assert_eq!(result.removed, vec![ROLE_A]);
        assert!(result.added.is_empty());
    }

    #[test]
    fn an_exclusive_swap_with_a_failed_removal_and_add_is_a_partial_write() {
        // The member holds ROLE_A and ROLE_B of an exclusive group and
        // presses ROLE_C. ROLE_A is removed, ROLE_B's removal fails and
        // ROLE_C cannot be added. The answer must not claim that the member
        // holds neither: ROLE_B is still there.
        let store = exclusive_menu();
        let mut roles = FakeRoles::new(MEMBER, &[ROLE_A, ROLE_B]);
        roles.remove_failures.insert(ROLE_B, vec![forbidden()]);
        roles.add_failures.insert(ROLE_C, vec![forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_C));
        assert_eq!(result.outcome.code, Code::PartialWrite);
        assert_eq!(result.outcome.detail, Some("forbidden"));
        assert_eq!(result.removed, vec![ROLE_A]);
        assert!(result.added.is_empty());
        // The member really still holds ROLE_B.
        assert!(harness
            .roles
            .held
            .get(&MEMBER)
            .expect("held")
            .contains(&ROLE_B));
    }

    #[test]
    fn a_write_that_fails_entirely_is_reported_as_a_write_failure() {
        // Nothing succeeded: the answer is a plain write failure, not a
        // partial one.
        let store = exclusive_menu();
        let mut roles = FakeRoles::new(MEMBER, &[ROLE_A]);
        roles.remove_failures.insert(ROLE_A, vec![forbidden()]);
        roles.add_failures.insert(ROLE_B, vec![forbidden()]);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press_option(1, ROLE_B));
        assert_eq!(result.outcome.code, Code::WriteFailed);
        assert_eq!(result.outcome.detail, Some("forbidden"));
        assert!(result.added.is_empty());
        assert!(result.removed.is_empty());
    }

    #[test]
    fn an_empty_select_reads_no_member_data() {
        // A select with no value is a no-op: it must not read the member's
        // roles or the role facts.
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press_select(1, &[]));
        assert_eq!(result.outcome.code, Code::NoChange);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert_eq!(harness.facts.reads.load(Ordering::SeqCst), 0);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
    }
}

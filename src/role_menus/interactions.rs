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
//!    [`CUSTOM_ID_PREFIX`] and names a stored menu and one of its options by
//!    index. It is at most [`MAX_CUSTOM_ID_LEN`] bytes and holds nothing but
//!    ASCII digits and the prefix's separators. A forged, malformed,
//!    other-menu, other-guild or other-message id is ignored: no member data
//!    is read and nothing is written.
//! 2. **The stored row is the truth.** The interaction's own guild and message
//!    must be the ones the menu row records, and the option must still be
//!    stored. A deleted menu or option answers with a notice and writes
//!    nothing.
//! 3. **Defer first.** The acknowledgement is an ephemeral deferral that
//!    happens before any slow call, then the work runs, then one ephemeral
//!    follow-up is sent. Every reply is built from stable codes: no role
//!    name, no label and no member text is ever echoed.
//! 4. **RM1 decides.** The click is planned by the shared policy
//!    (`super::policy`): a multi option toggles, an exclusive group swaps, and a
//!    max-select breach is refused. A partial swap is reported honestly
//!    (`swap_partial`) and never as a success.
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

use crate::roles::eligibility::{
    decide, decide_removal, Decision, GrantPath, GrantRequest, RoleFacts, SafewordState,
};
use crate::roles::grant::{self, FailCode, WriteError};

use super::command::to_policy_menu;
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
    /// The custom id names a menu that is not stored.
    MenuNotFound,
    /// The custom id names an option that is not stored.
    OptionNotFound,
    /// The press did not come from the message the menu row records.
    MessageMismatch,
    /// The menu row belongs to another guild than the press.
    GuildMismatch,
    /// The stored menu row could not be understood.
    CorruptRow,
    /// The stored menu could not be read.
    StoreFailed,
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
    /// The exclusive swap could not remove an old option; nothing changed.
    SwapBlocked,
    /// An exclusive swap added the new option but could not remove an old one.
    SwapPartial,
    /// A role write failed (the detail carries the K04 fail code).
    WriteFailed,
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
            Code::MenuNotFound => "menu_not_found",
            Code::OptionNotFound => "option_not_found",
            Code::MessageMismatch => "message_mismatch",
            Code::GuildMismatch => "guild_mismatch",
            Code::CorruptRow => "corrupt_row",
            Code::StoreFailed => "store_failed",
            Code::FactsUnavailable => "facts_unavailable",
            Code::MemberRolesUnavailable => "member_roles_unavailable",
            Code::Ineligible => "ineligible",
            Code::Added => "added",
            Code::Removed => "removed",
            Code::Swapped => "swapped",
            Code::AlreadySelected => "already_selected",
            Code::MaxReached => "max_reached",
            Code::UnknownOption => "unknown_option",
            Code::SwapBlocked => "swap_blocked",
            Code::SwapPartial => "swap_partial",
            Code::WriteFailed => "write_failed",
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
}

// ---------------------------------------------------------------------------
// Custom id
// ---------------------------------------------------------------------------

/// The menu and option a component id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCustomId {
    /// The stored menu the id names.
    pub menu_id: i64,
    /// The index of the option inside the menu, in stored order.
    pub option_index: usize,
}

/// Why a component id was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The id is longer than [`MAX_CUSTOM_ID_LEN`].
    TooLong,
    /// The id does not start with [`CUSTOM_ID_PREFIX`].
    BadPrefix,
    /// The id does not have the shape `prefix m<digits>:o<digits>`.
    Malformed,
    /// A number in the id is not a number, or is zero where zero is not allowed.
    BadNumber,
}

impl ParseError {
    /// The stable machine code.
    pub const fn code(self) -> &'static str {
        match self {
            ParseError::TooLong => "custom_id_too_long",
            ParseError::BadPrefix => "custom_id_bad_prefix",
            ParseError::Malformed => "custom_id_malformed",
            ParseError::BadNumber => "custom_id_bad_number",
        }
    }
}

/// Parse a component id of a role menu.
///
/// The id is `rolemenu:v1:m<menu id>:o<option index>`. Every part must be
/// there and must be ASCII digits only: a letter, a sign, a space, a
/// separator of its own or any other character makes the whole id malformed,
/// so a hostile spelling of the class is refused and not guessed.
///
/// A menu id of zero or an option index that does not fit in a `usize` is
/// refused: neither can name a stored row.
pub fn parse_custom_id(raw: &str) -> Result<ParsedCustomId, ParseError> {
    if raw.len() > MAX_CUSTOM_ID_LEN {
        return Err(ParseError::TooLong);
    }
    let Some(rest) = raw.strip_prefix(CUSTOM_ID_PREFIX) else {
        return Err(ParseError::BadPrefix);
    };
    let mut parts = rest.split(':');
    let (Some(menu_part), Some(option_part)) = (parts.next(), parts.next()) else {
        return Err(ParseError::Malformed);
    };
    if parts.next().is_some() {
        return Err(ParseError::Malformed);
    }
    let Some(menu_digits) = menu_part.strip_prefix('m') else {
        return Err(ParseError::Malformed);
    };
    let Some(option_digits) = option_part.strip_prefix('o') else {
        return Err(ParseError::Malformed);
    };
    if menu_digits.is_empty() || option_digits.is_empty() {
        return Err(ParseError::Malformed);
    }
    if !menu_digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::BadNumber);
    }
    if !option_digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::BadNumber);
    }
    let menu_id: i64 = menu_digits.parse().map_err(|_| ParseError::BadNumber)?;
    let option_index: usize = option_digits.parse().map_err(|_| ParseError::BadNumber)?;
    if menu_id <= 0 {
        return Err(ParseError::BadNumber);
    }
    Ok(ParsedCustomId {
        menu_id,
        option_index,
    })
}

/// Build the component id of one option of a menu.
///
/// The result is always shorter than [`MAX_CUSTOM_ID_LEN`]; a menu id that
/// could not fit is refused instead of being cut.
pub fn custom_id_for(menu_id: i64, option_index: usize) -> Result<String, ParseError> {
    if menu_id <= 0 {
        return Err(ParseError::BadNumber);
    }
    let raw = format!("{CUSTOM_ID_PREFIX}m{menu_id}:o{option_index}");
    if raw.len() > MAX_CUSTOM_ID_LEN {
        return Err(ParseError::TooLong);
    }
    Ok(raw)
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
    /// The bot's own user id.
    pub bot_user_id: u64,
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
            bot_user_id: 0,
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
    /// Lists the options of one menu, ordered by position.
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

/// The role service the handler writes through.
///
/// The handler never writes a role itself: every add and every removal goes
/// through this port, so the K03 decision and the K04 executor stay in charge.
pub trait RoleService {
    /// The role ids the member holds.
    fn member_roles(&self, guild_id: u64, member_id: u64) -> Result<Vec<u64>, LookupFail>;
    /// Adds one role to the member.
    fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError>;
    /// Removes one role from the member.
    fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError>;
}

/// The clock. The handler never reads the clock itself.
pub trait Clock {
    /// The current time, as a unix timestamp in seconds.
    fn now(&self) -> i64;
}

/// A fixed clock for tests and for callers that have no clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct FixedClock {
    /// The time the clock reports.
    pub now: i64,
}

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.now
    }
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
    /// The clock.
    pub clock: &'a dyn Clock,
    /// Where sanitized failures go.
    pub reporter: &'a mut dyn FailureReporter,
    /// The Discord side of the interaction.
    pub responder: &'a mut dyn InteractionResponder,
}

// ---------------------------------------------------------------------------
// The per-member lock
// ---------------------------------------------------------------------------

/// One member's lock, with the number of presses that want it.
#[derive(Debug, Default)]
struct MemberLock {
    gate: Mutex<()>,
    waiting: Mutex<usize>,
}

/// Serializes the presses of one member.
///
/// A member may press twice in a row; the two read-modify-write cycles must
/// not interleave. Each member that is being served holds its own lock, so
/// two different members never wait for each other. The entry is removed when
/// the last press of that member is done, so the memory is bounded by the
/// number of members that press at the same time.
#[derive(Debug, Default)]
pub struct MemberLocks {
    inner: Mutex<HashMap<u64, Arc<MemberLock>>>,
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
        let lock = self.lock_for(member_id);
        {
            let mut waiting = lock.waiting.lock().unwrap_or_else(|e| e.into_inner());
            *waiting += 1;
        }
        let guard = lock.gate.lock().unwrap_or_else(|e| e.into_inner());
        let result = work();
        drop(guard);
        {
            let mut waiting = lock.waiting.lock().unwrap_or_else(|e| e.into_inner());
            *waiting = waiting.saturating_sub(1);
            let last = *waiting == 0;
            if last {
                self.release(member_id, &lock);
            }
        }
        result
    }

    /// The lock of one member, taken out of the map.
    fn lock_for(&self, member_id: u64) -> Arc<MemberLock> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            map.entry(member_id)
                .or_insert_with(|| Arc::new(MemberLock::default())),
        )
    }

    /// Drop the member's entry when nobody else is waiting for it.
    fn release(&self, member_id: u64, lock: &Arc<MemberLock>) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let still_the_same = map
            .get(&member_id)
            .is_some_and(|current| Arc::ptr_eq(current, lock));
        if still_the_same {
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
    // RED STUB: the behaviour is implemented in the next commit.
    let _ = (settings, locks, press, ports);
    PressOutcome::ignored(Outcome::new(Code::InvalidCustomId))
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
    // The option must still be stored.
    let options = match ports.store.list_options(parsed.menu_id) {
        Ok(options) => options,
        Err(error) => return notice(ports, press, Outcome::with(Code::StoreFailed, error.code())),
    };
    let Some(option) = options.get(parsed.option_index) else {
        return notice(ports, press, Outcome::new(Code::OptionNotFound));
    };
    let role_id = option.role;

    // The stored menu becomes the shared policy menu. A row whose mode is not
    // known is corrupt and must not fall back to a more permissive mode.
    let policy_menu = match to_policy_menu(&menu, &options) {
        Ok(policy_menu) => policy_menu,
        Err(_) => return notice(ports, press, Outcome::new(Code::CorruptRow)),
    };
    let group_id = match policy_menu.groups.first() {
        Some(group) => group.id.clone(),
        None => return notice(ports, press, Outcome::new(Code::CorruptRow)),
    };

    // An unreadable protected-role list refuses every press.
    if settings.protected_roles_invalid {
        return notice(ports, press, Outcome::new(Code::Ineligible));
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

    // The click is planned by the shared RM1 policy.
    let plan = match policy::plan_click(&policy::PlanRequest {
        menu: &policy_menu,
        click: &policy::Click {
            member_roles: member_roles.clone(),
            group_id,
            option_role_id: role_id,
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
        Err(error) => return notice(ports, press, Outcome::with(Code::Ineligible, error.code())),
    };

    // The policy's own reasons are reported first: a refusal that never
    // reaches a write.
    if let Some((_, reason)) = plan.skipped.first() {
        let code = match reason {
            policy::Reason::Denied(deny) => Outcome::with(Code::Ineligible, deny.code()),
            policy::Reason::RemovalDenied(deny) => Outcome::with(Code::Ineligible, deny.code()),
            policy::Reason::UnknownOption => Outcome::new(Code::UnknownOption),
            policy::Reason::MaxReached => Outcome::new(Code::MaxReached),
            policy::Reason::AlreadySelected => Outcome::new(Code::AlreadySelected),
            policy::Reason::SwapBlocked => Outcome::new(Code::SwapBlocked),
        };
        return notice(ports, press, code);
    }

    // Every role that is about to be written is checked by K03 again, here,
    // so a protected, managed or privileged role is refused at press time even
    // when the stored row lists it.
    for &role in plan.add.iter().chain(plan.remove.iter()) {
        let facts = facts_by_role
            .iter()
            .find(|f| f.id == role)
            .copied()
            .unwrap_or(RoleFacts {
                id: role,
                position: 0,
                managed: false,
                permissions: 0,
                exists: false,
            });
        let request = GrantRequest {
            path: GrantPath::Menu,
            role: facts,
            bot_top_position: bot_top,
            guild_id: press.guild_id,
            protected_role_ids: &settings.protected_role_ids,
            safeword: press.safeword,
            safeword_restricted_role_ids: &settings.safeword_restricted_role_ids,
            staff: None,
        };
        let decision = if plan.add.contains(&role) {
            decide(&request)
        } else {
            decide_removal(&request)
        };
        if let Decision::Deny(reason) = decision {
            return notice(ports, press, Outcome::with(Code::Ineligible, reason.code()));
        }
    }

    // In shadow mode nothing is written; the outcome says what would happen.
    if settings.mode == Mode::Shadow {
        let outcome = shadow_outcome(&plan);
        let result = PressOutcome {
            outcome,
            added: plan.add.clone(),
            removed: plan.remove.clone(),
            shadow: true,
        };
        finish(ports, &result);
        return result;
    }

    // The removals come first, then the addition: an exclusive swap leaves
    // the member with the option they pressed.
    let mut removed = Vec::new();
    let mut add_failed = false;
    let mut remove_failed = false;
    for &role in &plan.remove {
        match ports.roles.remove_role(press.member_id, role) {
            Ok(()) => removed.push(role),
            Err(error) => {
                remove_failed = true;
                report_write_failure(ports, press, role, &error);
            }
        }
    }
    let mut added = Vec::new();
    for &role in &plan.add {
        match ports.roles.add_role(press.member_id, role) {
            Ok(()) => added.push(role),
            Err(error) => {
                add_failed = true;
                report_write_failure(ports, press, role, &error);
            }
        }
    }

    let outcome = if add_failed || remove_failed {
        // A failure is never reported as a success. A swap that added the
        // new option but could not remove an old one says exactly that.
        if !added.is_empty() && remove_failed {
            Outcome::new(Code::SwapPartial)
        } else {
            Outcome::with(Code::WriteFailed, FailCode::Other.code())
        }
    } else if added.is_empty() && removed.is_empty() {
        Outcome::new(Code::NoChange)
    } else if added.is_empty() {
        Outcome::new(Code::Removed)
    } else if removed.is_empty() {
        Outcome::new(Code::Added)
    } else {
        Outcome::new(Code::Swapped)
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

/// The outcome a shadow press reports: what it would have done.
fn shadow_outcome(plan: &policy::Plan) -> Outcome {
    if plan.add.is_empty() && plan.remove.is_empty() {
        Outcome::new(Code::NoChange)
    } else if plan.add.is_empty() {
        Outcome::new(Code::Removed)
    } else if plan.remove.is_empty() {
        Outcome::new(Code::Added)
    } else {
        Outcome::new(Code::Swapped)
    }
}

/// Report one failed role write with a stable code.
fn report_write_failure<S, F, R>(
    ports: &mut PressPorts<'_, S, F, R>,
    press: &Press,
    role: u64,
    error: &WriteError,
) where
    S: MenuStore + ?Sized,
    F: RoleFactsSource + ?Sized,
    R: RoleService,
{
    let code = grant::classify(error);
    ports.reporter.report(&format!(
        "rolemenu_press guild={} member={} role={role}:failed:{}",
        press.guild_id,
        press.member_id,
        code.code()
    ));
}

/// Send the ephemeral follow-up.
fn finish<S, F, R>(ports: &mut PressPorts<'_, S, F, R>, result: &PressOutcome)
where
    S: MenuStore + ?Sized,
    F: RoleFactsSource + ?Sized,
    R: RoleService,
{
    let _ = ports.responder.follow_up(&result.outcome.text());
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
    let _ = ports.responder.follow_up(&result.outcome.text());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::{ADMINISTRATOR, MANAGE_ROLES};
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    #[derive(Default)]
    struct FakeRoles {
        held: HashMap<u64, Vec<u64>>,
        add_failures: HashMap<u64, WriteError>,
        remove_failures: HashMap<u64, WriteError>,
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
    }

    impl RoleService for FakeRoles {
        fn member_roles(&self, _guild_id: u64, member_id: u64) -> Result<Vec<u64>, LookupFail> {
            self.member_roles_reads.fetch_add(1, Ordering::SeqCst);
            match self.member_roles_fail {
                Some(error) => Err(error),
                None => Ok(self.held.get(&member_id).cloned().unwrap_or_default()),
            }
        }
        fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.add_calls.lock().expect("add calls").push(role_id);
            if let Some(error) = self.add_failures.get(&role_id) {
                return Err(*error);
            }
            self.held.entry(member_id).or_default().push(role_id);
            Ok(())
        }
        fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.remove_calls
                .lock()
                .expect("remove calls")
                .push(role_id);
            if let Some(error) = self.remove_failures.get(&role_id) {
                return Err(*error);
            }
            if let Some(list) = self.held.get_mut(&member_id) {
                list.retain(|r| *r != role_id);
            }
            Ok(())
        }
    }

    /// A responder that records the order of the calls.
    #[derive(Default)]
    struct FakeResponder {
        calls: Mutex<Vec<String>>,
        defer_fails: bool,
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
            Ok(())
        }
    }

    fn forbidden() -> WriteError {
        WriteError {
            status: 403,
            api_code: None,
            retry_after_ms: None,
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

    fn press(custom_id: &str) -> Press {
        Press {
            guild_id: HOME,
            message_id: MESSAGE,
            member_id: MEMBER,
            custom_id: custom_id.to_string(),
            safeword: SafewordState::Inactive,
        }
    }

    /// A whole press with the fakes wired up.
    struct Harness<S: MenuStore> {
        store: S,
        facts: FakeFacts,
        roles: FakeRoles,
        responder: FakeResponder,
        reporter: MemoryReporter,
        locks: MemberLocks,
        clock: FixedClock,
    }

    impl<S: MenuStore> Harness<S> {
        fn new(store: S, facts: FakeFacts, roles: FakeRoles) -> Self {
            Harness {
                store,
                facts,
                roles,
                responder: FakeResponder::default(),
                reporter: MemoryReporter::default(),
                locks: MemberLocks::new(),
                clock: FixedClock { now: 1_700_000_000 },
            }
        }

        fn run(&mut self, settings: &Settings, press: &Press) -> PressOutcome {
            let mut ports = PressPorts {
                store: &self.store,
                facts: &self.facts,
                roles: &mut self.roles,
                clock: &self.clock,
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

    // ---------------------------------------------------------------------
    // Custom id parsing
    // ---------------------------------------------------------------------

    #[test]
    fn a_well_formed_custom_id_names_a_menu_and_an_option() {
        let parsed = parse_custom_id("rolemenu:v1:m12:o3").expect("parsed");
        assert_eq!(parsed.menu_id, 12);
        assert_eq!(parsed.option_index, 3);
    }

    #[test]
    fn custom_id_round_trips() {
        let raw = custom_id_for(42, 7).expect("built");
        assert_eq!(raw, "rolemenu:v1:m42:o7");
        let parsed = parse_custom_id(&raw).expect("parsed");
        assert_eq!(parsed.menu_id, 42);
        assert_eq!(parsed.option_index, 7);
    }

    #[test]
    fn a_custom_id_longer_than_the_limit_is_refused() {
        let raw = format!("rolemenu:v1:m1:o{}", "9".repeat(MAX_CUSTOM_ID_LEN));
        assert!(raw.len() > MAX_CUSTOM_ID_LEN);
        assert_eq!(parse_custom_id(&raw), Err(ParseError::TooLong));
    }

    #[test]
    fn a_custom_id_at_the_limit_is_still_parsed() {
        let prefix = "rolemenu:v1:m1:o";
        let padding = MAX_CUSTOM_ID_LEN - prefix.len();
        let raw = format!("{prefix}{}", "9".repeat(padding));
        assert_eq!(raw.len(), MAX_CUSTOM_ID_LEN);
        assert!(parse_custom_id(&raw).is_ok());
    }

    #[test]
    fn a_foreign_prefix_is_refused() {
        assert_eq!(
            parse_custom_id("rolemenu:v2:m1:o0"),
            Err(ParseError::BadPrefix)
        );
        assert_eq!(parse_custom_id("vc:v1:m1:o0"), Err(ParseError::BadPrefix));
        assert_eq!(parse_custom_id(""), Err(ParseError::BadPrefix));
    }

    #[test]
    fn a_malformed_custom_id_is_refused() {
        for raw in [
            "rolemenu:v1:",
            "rolemenu:v1:m1",
            "rolemenu:v1:o1",
            "rolemenu:v1:m1:",
            "rolemenu:v1::o1",
            "rolemenu:v1:m1:o1:extra",
            "rolemenu:v1:x1:o1",
            "rolemenu:v1:m1:x1",
            "rolemenu:v1:m1:o1 ",
            "rolemenu:v1:m 1:o1",
            "rolemenu:v1:m1:o1\u{0}",
        ] {
            assert!(parse_custom_id(raw).is_err(), "{raw:?} must not parse");
        }
    }

    #[test]
    fn a_custom_id_with_a_hostile_number_is_refused() {
        for raw in [
            "rolemenu:v1:m-1:o0",
            "rolemenu:v1:m+1:o0",
            "rolemenu:v1:m1e3:o0",
            "rolemenu:v1:m0:o0",
            "rolemenu:v1:m1:o-1",
            "rolemenu:v1:m1:o1_0",
            "rolemenu:v1:m\u{0661}:o0",
            "rolemenu:v1:m1:o\u{0661}",
        ] {
            assert_eq!(
                parse_custom_id(raw),
                Err(ParseError::BadNumber),
                "{raw:?} must not parse"
            );
        }
    }

    #[test]
    fn a_custom_id_with_a_huge_option_index_is_refused() {
        let raw = format!("rolemenu:v1:m1:o{}", "9".repeat(30));
        assert_eq!(parse_custom_id(&raw), Err(ParseError::BadNumber));
    }

    #[test]
    fn parse_error_codes_are_stable() {
        assert_eq!(ParseError::TooLong.code(), "custom_id_too_long");
        assert_eq!(ParseError::BadPrefix.code(), "custom_id_bad_prefix");
        assert_eq!(ParseError::Malformed.code(), "custom_id_malformed");
        assert_eq!(ParseError::BadNumber.code(), "custom_id_bad_number");
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
        let result = harness.run(&settings, &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings, &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings, &press("rolemenu:v1:m1:o0"));
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
        let mut other = press("rolemenu:v1:m1:o0");
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m999:o0"));
        assert_eq!(result.outcome.code, Code::InvalidCustomId);
        assert_eq!(harness.store.menu_reads(), 0);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert_eq!(harness.facts.reads.load(Ordering::SeqCst), 0);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
        assert!(harness.responder.calls().is_empty());
    }

    #[test]
    fn a_malformed_custom_id_is_ignored_and_reads_no_member_data() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press("nonsense"));
        assert_eq!(result.outcome.code, Code::InvalidCustomId);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_oversized_custom_id_is_ignored() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let raw = format!("rolemenu:v1:m1:o{}", "9".repeat(MAX_CUSTOM_ID_LEN));
        let result = harness.run(&settings(), &press(&raw));
        assert_eq!(result.outcome.code, Code::InvalidCustomId);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_custom_id_naming_another_menu_is_ignored() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let result = harness.run(&settings(), &press("rolemenu:v1:m2:o0"));
        assert_eq!(result.outcome.code, Code::InvalidCustomId);
        assert_eq!(harness.store.menu_reads(), 0);
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o9"));
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.follow_ups(), vec!["option_not_found".to_string()]);
    }

    #[test]
    fn a_press_on_another_message_is_refused() {
        let store = multi_menu();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[]),
        );
        let mut other = press("rolemenu:v1:m1:o0");
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
        assert_eq!(result.outcome.code, Code::StoreFailed);
        assert_eq!(harness.store.menu_reads(), 0);
        assert_eq!(harness.roles.member_roles_reads.load(Ordering::SeqCst), 0);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.responder.calls(), vec!["defer".to_string()]);
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
        assert_eq!(result.outcome.code, Code::Removed);
        assert_eq!(result.removed, vec![ROLE_A]);
        assert_eq!(harness.roles.removed(), vec![ROLE_A]);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_exclusive_group_swaps_the_old_option_out() {
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").mode = "exclusive".to_string();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A]),
        );
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o1"));
        assert_eq!(result.outcome.code, Code::Swapped);
        assert_eq!(result.added, vec![ROLE_B]);
        assert_eq!(result.removed, vec![ROLE_A]);
        // The removal comes first: the member ends with the option they pressed.
        assert_eq!(harness.roles.removed(), vec![ROLE_A]);
        assert_eq!(harness.roles.added(), vec![ROLE_B]);
    }

    #[test]
    fn pressing_the_held_option_of_an_exclusive_group_changes_nothing() {
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").mode = "exclusive".to_string();
        let mut harness = Harness::new(
            store,
            FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]),
            FakeRoles::new(MEMBER, &[ROLE_A]),
        );
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o2"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
        assert_eq!(result.outcome.code, Code::Removed);
        assert_eq!(harness.roles.removed(), vec![ROLE_A]);
    }

    #[test]
    fn a_swap_whose_removal_fails_is_reported_as_partial_and_never_as_success() {
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").mode = "exclusive".to_string();
        let mut roles = FakeRoles::new(MEMBER, &[ROLE_A]);
        roles.remove_failures.insert(ROLE_A, forbidden());
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o1"));
        assert_eq!(result.outcome.code, Code::SwapPartial);
        assert_eq!(result.added, vec![ROLE_B]);
        assert!(result.removed.is_empty());
        // The failure was reported, not discarded.
        assert!(harness
            .reporter
            .lines
            .iter()
            .any(|line| line.contains("failed:forbidden")));
    }

    #[test]
    fn a_swap_that_is_blocked_changes_nothing() {
        let mut store = multi_menu();
        store.menus.get_mut(&1).expect("menu").mode = "exclusive".to_string();
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o1"));
        assert_eq!(result.outcome.code, Code::SwapBlocked);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
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
        let result = harness.run(&settings, &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings, &press("rolemenu:v1:m1:o0"));
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
        let mut blocked = press("rolemenu:v1:m1:o0");
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
        let mut other = press("rolemenu:v1:m1:o0");
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
        assert_eq!(result.outcome.code, Code::Ineligible);
        assert_eq!(result.outcome.detail, Some("role_deleted"));
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_unavailable_facts_lookup_is_refused() {
        let store = multi_menu();
        let mut facts = FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]);
        facts.fail = Some(LookupFail::Unavailable);
        let mut harness = Harness::new(store, facts, FakeRoles::new(MEMBER, &[]));
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
        assert_eq!(result.outcome.code, Code::FactsUnavailable);
        assert!(harness.roles.added().is_empty());
    }

    #[test]
    fn an_unavailable_member_role_lookup_is_refused() {
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[]);
        roles.member_roles_fail = Some(LookupFail::Unavailable);
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        roles.add_failures.insert(ROLE_A, forbidden());
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
        assert_eq!(result.outcome.code, Code::WriteFailed);
        assert!(result.added.is_empty());
        assert!(harness
            .reporter
            .lines
            .iter()
            .any(|line| line.contains("failed:forbidden")));
    }

    #[test]
    fn a_failed_add_never_echoes_a_role_name_or_member_text() {
        let store = multi_menu();
        let mut roles = FakeRoles::new(MEMBER, &[]);
        roles.add_failures.insert(ROLE_A, forbidden());
        let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
        harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
        let follow_ups = harness.follow_ups();
        assert_eq!(follow_ups, vec!["write_failed (other)".to_string()]);
        for line in &harness.reporter.lines {
            assert!(!line.contains("label_"));
            assert!(!line.contains('\u{1F600}'));
        }
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
        let result = harness.run(&settings, &press("rolemenu:v1:m1:o0"));
        assert_eq!(result.outcome.code, Code::Added);
        assert!(result.shadow);
        assert_eq!(result.added, vec![ROLE_A]);
        assert!(harness.roles.added().is_empty());
        assert!(harness.roles.removed().is_empty());
        assert!(harness.roles.held.get(&MEMBER).expect("held").is_empty());
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

    impl RoleService for LoggingRoles {
        fn member_roles(&self, guild_id: u64, member_id: u64) -> Result<Vec<u64>, LookupFail> {
            self.inner.member_roles(guild_id, member_id)
        }
        fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.log
                .lock()
                .expect("log")
                .push(format!("write:{role_id}"));
            self.inner.add_role(member_id, role_id)
        }
        fn remove_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.log
                .lock()
                .expect("log")
                .push(format!("write:{role_id}"));
            self.inner.remove_role(member_id, role_id)
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
                    let clock = FixedClock { now: 1_700_000_000 };
                    let mut ports = PressPorts {
                        store: store.as_ref(),
                        facts: facts.as_ref(),
                        roles: &mut local_roles,
                        clock: &clock,
                        reporter: &mut reporter,
                        responder: &mut responder,
                    };
                    barrier.wait();
                    handle_press(
                        settings.as_ref(),
                        locks.as_ref(),
                        &press(&format!("rolemenu:v1:m1:o{index}")),
                        &mut ports,
                    )
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread");
        }

        // The two presses never overlap: the second press's reads all come
        // after the first press's write, so exactly one read sits between the
        // first write and the last read.
        let events = log.lock().expect("log").clone();
        let reads = events.iter().filter(|e| e.starts_with("read_")).count();
        let writes = events.iter().filter(|e| e.starts_with("write:")).count();
        assert_eq!(reads, 4, "{events:?}");
        assert_eq!(writes, 2, "{events:?}");
        let first_write = events
            .iter()
            .position(|e| e.starts_with("write:"))
            .expect("a write");
        let reads_after_first_write = events
            .iter()
            .skip(first_write)
            .filter(|e| e.starts_with("read_"))
            .count();
        assert_eq!(
            reads_after_first_write, 1,
            "the two presses interleaved: {events:?}"
        );
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
        harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
        assert_eq!(result.outcome.code, Code::OptionNotFound);
        assert!(harness.roles.added().is_empty());
        assert_eq!(harness.follow_ups(), vec!["option_not_found".to_string()]);
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
        let result = harness.run(&settings(), &press("rolemenu:v1:m1:o0"));
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
            let mut roles = FakeRoles::new(MEMBER, &[]);
            // Every third case has a failing write.
            let failing = rng.next(3) == 0;
            if failing {
                roles.add_failures.insert(ROLE_A, forbidden());
            }
            let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
            // The model member: the roles it holds after the presses.
            let mut model: Vec<u64> = Vec::new();
            for step in 0..8u32 {
                let index = rng.next(3) as usize;
                let result = harness.run(&settings(), &press(&format!("rolemenu:v1:m1:o{index}")));
                // The press always answers exactly once.
                assert_eq!(
                    harness.follow_ups().len(),
                    (step + 1) as usize,
                    "case {case}"
                );
                match result.outcome.code {
                    Code::Added | Code::Swapped | Code::Removed => {
                        assert!(!failing, "case {case}: a failed write was a success");
                    }
                    Code::AlreadySelected
                    | Code::MaxReached
                    | Code::SwapBlocked
                    | Code::Ineligible
                    | Code::WriteFailed
                    | Code::SwapPartial
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
            // The member ends with exactly the roles the model says it has,
            // unless a write failed in this case.
            if !failing {
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
            roles.add_failures.insert(role, forbidden());
            let mut harness = Harness::new(store, FakeFacts::new(&[ROLE_A, ROLE_B, ROLE_C]), roles);
            let index = [ROLE_A, ROLE_B, ROLE_C]
                .iter()
                .position(|r| *r == role)
                .expect("role");
            let result = harness.run(&settings(), &press(&format!("rolemenu:v1:m1:o{index}")));
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

    // ---------------------------------------------------------------------
    // Codes
    // ---------------------------------------------------------------------

    #[test]
    fn the_codes_are_stable() {
        assert_eq!(Code::Disabled.as_str(), "disabled");
        assert_eq!(Code::OtherGuild.as_str(), "other_guild");
        assert_eq!(Code::NoHomeGuild.as_str(), "no_home_guild");
        assert_eq!(Code::InvalidCustomId.as_str(), "invalid_custom_id");
        assert_eq!(Code::MenuNotFound.as_str(), "menu_not_found");
        assert_eq!(Code::OptionNotFound.as_str(), "option_not_found");
        assert_eq!(Code::MessageMismatch.as_str(), "message_mismatch");
        assert_eq!(Code::GuildMismatch.as_str(), "guild_mismatch");
        assert_eq!(Code::CorruptRow.as_str(), "corrupt_row");
        assert_eq!(Code::StoreFailed.as_str(), "store_failed");
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
        assert_eq!(Code::SwapBlocked.as_str(), "swap_blocked");
        assert_eq!(Code::SwapPartial.as_str(), "swap_partial");
        assert_eq!(Code::WriteFailed.as_str(), "write_failed");
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
}

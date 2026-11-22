//! Startup preflight: a pure, per-feature configuration and permission report.
//!
//! RED-EVIDENCE STUB COMMIT. The behaviour below is deliberately incomplete so the
//! tests fail; the real implementation follows in the next commit.
//!
//! What the module is:
//!
//! * [`evaluate`] takes plain, Discord-free inputs (requested modes, configured ids,
//!   permission bit sets, role facts, the bot's top role position) and returns a
//!   [`Report`]: for every feature, a status (`Off`, `Ready`, `Disabled`) and the stable
//!   reason codes of the requirements that failed.
//! * [`Report::effective_mode`] gives the mode a feature may actually run in. It never
//!   upgrades the requested mode: a disabled feature is `Off`.
//!
//! The module is pure: no serenity types, no I/O, no clock, no randomness. Every id and
//! every permission bit is passed in by the caller.

#![allow(dead_code)]

use crate::roles::runtime::FactsOutcome;
use crate::settings::Mode;

// ---------------------------------------------------------------------------
// Permission bits (Discord's own values, pinned here as raw literals).
// ---------------------------------------------------------------------------

/// Discord's `ADMINISTRATOR` bit. It implies every other permission.
pub const ADMINISTRATOR: u64 = 1 << 3;
/// Discord's `MANAGE_CHANNELS` bit.
pub const MANAGE_CHANNELS: u64 = 1 << 4;
/// Discord's `VIEW_CHANNEL` bit.
pub const VIEW_CHANNEL: u64 = 1 << 10;
/// Discord's `SEND_MESSAGES` bit.
pub const SEND_MESSAGES: u64 = 1 << 11;
/// Discord's `MANAGE_MESSAGES` bit.
pub const MANAGE_MESSAGES: u64 = 1 << 13;
/// Discord's `EMBED_LINKS` bit.
pub const EMBED_LINKS: u64 = 1 << 14;
/// Discord's `READ_MESSAGE_HISTORY` bit.
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;
/// Discord's `CONNECT` bit.
pub const CONNECT: u64 = 1 << 20;
/// Discord's `MOVE_MEMBERS` bit.
pub const MOVE_MEMBERS: u64 = 1 << 24;
/// Discord's `MANAGE_ROLES` bit.
pub const MANAGE_ROLES: u64 = 1 << 28;

// ---------------------------------------------------------------------------
// Feature
// ---------------------------------------------------------------------------

/// The optional features the preflight reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    /// Flash-media expiry.
    Flash,
    /// The safeword role swap.
    Safeword,
    /// The welcome embed and join roles.
    Welcome,
    /// Managed embed posts.
    Embeds,
    /// The bump reminder.
    Bump,
    /// User-owned voice rooms.
    VoiceRooms,
    /// Button/select role menus.
    RoleMenus,
    /// Activity accounting.
    Activity,
}

impl Feature {
    /// Every feature, in the stable report order.
    pub const ALL: [Feature; 8] = [
        Feature::Flash,
        Feature::Safeword,
        Feature::Welcome,
        Feature::Embeds,
        Feature::Bump,
        Feature::VoiceRooms,
        Feature::RoleMenus,
        Feature::Activity,
    ];

    /// The stable, lowercase name of the feature.
    pub fn name(self) -> &'static str {
        match self {
            Feature::Flash => "flash",
            Feature::Safeword => "safeword",
            Feature::Welcome => "welcome",
            Feature::Embeds => "embeds",
            Feature::Bump => "bump",
            Feature::VoiceRooms => "voice_rooms",
            Feature::RoleMenus => "role_menus",
            Feature::Activity => "activity",
        }
    }
}

impl std::fmt::Display for Feature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// Status and reasons
// ---------------------------------------------------------------------------

/// What the preflight decided about one feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The feature was requested off.
    Off,
    /// The feature was requested on and every requirement holds.
    Ready,
    /// The feature was requested on and at least one requirement failed.
    Disabled,
}

impl Status {
    /// The stable, lowercase name of the status.
    pub fn name(self) -> &'static str {
        match self {
            Status::Off => "off",
            Status::Ready => "ready",
            Status::Disabled => "disabled",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a feature is disabled.
///
/// Every variant is a stable code and carries no text, name or id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    /// The bot has not got `MANAGE_ROLES` at guild level.
    MissingManageRoles,
    /// A configured role sits at or above the bot's top role.
    RoleAboveBot,
    /// A configured role does not exist (it was deleted).
    RoleMissing,
    /// The facts of a configured role could not be looked up.
    RoleFactsUnavailable,
    /// The bot's top role position is unknown.
    BotTopPositionUnknown,
    /// The feature has no configured channel to work in.
    NoChannelsConfigured,
    /// The feature has no configured category to work in.
    NoCategoryConfigured,
    /// The bot has not got `MANAGE_MESSAGES` in a configured channel.
    MissingManageMessages,
    /// The bot has not got `SEND_MESSAGES` in a configured channel.
    MissingSendMessages,
    /// The bot has not got `EMBED_LINKS` in a configured channel.
    MissingEmbedLinks,
    /// The bot has not got `VIEW_CHANNEL` in a configured channel.
    MissingViewChannel,
    /// The bot has not got `READ_MESSAGE_HISTORY` in a configured channel.
    MissingReadMessageHistory,
    /// The bot has not got `MANAGE_CHANNELS` in a configured category.
    MissingManageChannels,
    /// The bot has not got `MOVE_MEMBERS` in a configured category.
    MissingMoveMembers,
    /// The bot has not got `CONNECT` in a configured category.
    MissingConnect,
    /// The permissions of a configured channel or category could not be looked up.
    PermissionsUnknown,
}

impl Reason {
    /// The stable machine code. It never contains a name, an id or a message.
    pub fn code(self) -> &'static str {
        match self {
            Reason::MissingManageRoles => "missing_manage_roles",
            Reason::RoleAboveBot => "role_above_bot",
            Reason::RoleMissing => "role_missing",
            Reason::RoleFactsUnavailable => "role_facts_unavailable",
            Reason::BotTopPositionUnknown => "bot_top_position_unknown",
            Reason::NoChannelsConfigured => "no_channels_configured",
            Reason::NoCategoryConfigured => "no_category_configured",
            Reason::MissingManageMessages => "missing_manage_messages",
            Reason::MissingSendMessages => "missing_send_messages",
            Reason::MissingEmbedLinks => "missing_embed_links",
            Reason::MissingViewChannel => "missing_view_channel",
            Reason::MissingReadMessageHistory => "missing_read_message_history",
            Reason::MissingManageChannels => "missing_manage_channels",
            Reason::MissingMoveMembers => "missing_move_members",
            Reason::MissingConnect => "missing_connect",
            Reason::PermissionsUnknown => "permissions_unknown",
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// What one feature was configured with, and what mode it was requested in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRequest {
    /// The feature this request is about.
    pub feature: Feature,
    /// The mode the configuration asked for.
    pub mode: Mode,
    /// The channels the feature works in.
    pub channels: Vec<u64>,
    /// The category the feature works in, when it has one.
    pub category: Option<u64>,
    /// The roles the feature grants, when it grants any.
    pub roles: Vec<u64>,
}

impl FeatureRequest {
    /// A request with the given mode and no configured ids.
    pub fn new(feature: Feature, mode: Mode) -> Self {
        FeatureRequest {
            feature,
            mode,
            channels: Vec::new(),
            category: None,
            roles: Vec::new(),
        }
    }
}

/// The bot's effective permissions in one channel or category.
///
/// `None` means the lookup failed, which is never a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionLookup {
    /// The channel or category the permissions belong to.
    pub target_id: u64,
    /// The bot's effective permission bits, or `None` when the lookup failed.
    pub permissions: Option<u64>,
}

impl PermissionLookup {
    /// A successful lookup.
    pub fn known(target_id: u64, permissions: u64) -> Self {
        PermissionLookup {
            target_id,
            permissions: Some(permissions),
        }
    }

    /// A failed lookup.
    pub fn unknown(target_id: u64) -> Self {
        PermissionLookup {
            target_id,
            permissions: None,
        }
    }
}

/// The facts of one configured role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleFactsEntry {
    /// The role the facts belong to.
    pub role_id: u64,
    /// The outcome of the role lookup.
    pub outcome: FactsOutcome,
}

impl RoleFactsEntry {
    /// The facts of a role that was found.
    pub fn found(role_id: u64, facts: crate::roles::eligibility::RoleFacts) -> Self {
        RoleFactsEntry {
            role_id,
            outcome: FactsOutcome::Found(facts),
        }
    }

    /// A role that is not in the guild's role list (deleted).
    pub fn missing(role_id: u64) -> Self {
        RoleFactsEntry {
            role_id,
            outcome: FactsOutcome::Missing,
        }
    }

    /// A role whose facts could not be looked up.
    pub fn unavailable(role_id: u64) -> Self {
        RoleFactsEntry {
            role_id,
            outcome: FactsOutcome::Unavailable,
        }
    }
}

/// Everything the preflight reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreflightInput {
    /// The per-feature requests, in the caller's order.
    pub features: Vec<FeatureRequest>,
    /// The bot's permission bits at guild level.
    pub guild_permissions: u64,
    /// The bot's effective permissions per configured channel.
    pub channel_permissions: Vec<PermissionLookup>,
    /// The bot's effective permissions per configured category.
    pub category_permissions: Vec<PermissionLookup>,
    /// The facts of every configured role.
    pub role_facts: Vec<RoleFactsEntry>,
    /// The bot's top role position. `None` means it is unknown.
    pub bot_top_position: Option<u16>,
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// What the preflight decided about one feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureReport {
    /// The feature this entry is about.
    pub feature: Feature,
    /// The mode that was requested.
    pub requested_mode: Mode,
    /// The status the preflight decided.
    pub status: Status,
    /// The reasons the feature is disabled, sorted and free of duplicates.
    pub reasons: Vec<Reason>,
}

/// The preflight's decision about every feature it was asked about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// One entry per requested feature, in the stable feature order.
    pub entries: Vec<FeatureReport>,
}

impl Report {
    /// The entry for one feature, if the report holds one.
    pub fn feature(&self, feature: Feature) -> Option<&FeatureReport> {
        self.entries.iter().find(|e| e.feature == feature)
    }

    /// The status of one feature, if the report holds it.
    pub fn status(&self, feature: Feature) -> Option<Status> {
        self.feature(feature).map(|e| e.status)
    }

    /// The mode a feature may actually run in.
    ///
    /// A disabled feature is `Off`. The effective mode never exceeds the requested one.
    pub fn effective_mode(&self, feature: Feature) -> Option<Mode> {
        self.feature(feature).map(|e| e.effective_mode())
    }

    /// The rendered report lines.
    ///
    /// Every line holds a feature name, a status and the reason codes only. No id, no
    /// name from the configuration and no value from the environment ever appears.
    pub fn render_lines(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| {
                if e.reasons.is_empty() {
                    format!("{}: {}", e.feature.name(), e.status.name())
                } else {
                    let codes: Vec<&str> = e.reasons.iter().map(|r| r.code()).collect();
                    format!(
                        "{}: {} ({})",
                        e.feature.name(),
                        e.status.name(),
                        codes.join(", ")
                    )
                }
            })
            .collect()
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for line in self.render_lines() {
            if !first {
                f.write_str("\n")?;
            }
            first = false;
            f.write_str(&line)?;
        }
        Ok(())
    }
}

impl FeatureReport {
    /// The mode this feature may actually run in.
    ///
    /// `Disabled` becomes `Off`. The effective mode never exceeds the requested one.
    pub fn effective_mode(&self) -> Mode {
        match self.status {
            Status::Disabled => Mode::Off,
            Status::Ready => self.requested_mode,
            Status::Off => Mode::Off,
        }
    }
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

/// The rank of a mode: `Off` (0) < `Shadow` (1) < `Live` (2).
///
/// Used to compare a requested mode with an effective one. The rank is a plain
/// number so that "never exceeds" is a single comparison.
pub fn mode_rank(mode: Mode) -> u8 {
    match mode {
        Mode::Off => 0,
        Mode::Shadow => 1,
        Mode::Live => 2,
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Decide which optional features may run.
pub fn evaluate(input: &PreflightInput) -> Report {
    let _ = input;
    Report::default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::RoleFacts;

    // Synthetic ids. They are obviously made up and are not real Discord ids.
    const ROLE_ID: u64 = 900;
    const ROLE_ID_2: u64 = 901;
    const FLASH_CHANNEL: u64 = 100;
    const WELCOME_CHANNEL: u64 = 200;
    const EMBED_CHANNEL: u64 = 300;
    const BUMP_CHANNEL: u64 = 400;
    const VC_CATEGORY: u64 = 500;

    /// The guild-level bits of a healthy bot: it may manage roles and channels.
    fn guild_bits() -> u64 {
        MANAGE_ROLES | VIEW_CHANNEL | MANAGE_CHANNELS | MOVE_MEMBERS | CONNECT
    }

    /// The bits of a healthy channel.
    fn channel_bits() -> u64 {
        VIEW_CHANNEL | SEND_MESSAGES | EMBED_LINKS | MANAGE_MESSAGES | READ_MESSAGE_HISTORY
    }

    /// The bits of a healthy voice-room category.
    fn category_bits() -> u64 {
        MANAGE_CHANNELS | MOVE_MEMBERS | CONNECT
    }

    fn feature_req(
        feature: Feature,
        mode: Mode,
        channels: Vec<u64>,
        category: Option<u64>,
        roles: Vec<u64>,
    ) -> FeatureRequest {
        FeatureRequest {
            feature,
            mode,
            channels,
            category,
            roles,
        }
    }

    /// An input in which every feature is requested live and every requirement holds.
    fn base_input() -> PreflightInput {
        PreflightInput {
            features: vec![
                feature_req(Feature::Flash, Mode::Live, vec![FLASH_CHANNEL], None, vec![]),
                feature_req(Feature::Safeword, Mode::Live, vec![], None, vec![ROLE_ID]),
                feature_req(
                    Feature::Welcome,
                    Mode::Live,
                    vec![WELCOME_CHANNEL],
                    None,
                    vec![ROLE_ID],
                ),
                feature_req(Feature::Embeds, Mode::Live, vec![EMBED_CHANNEL], None, vec![]),
                feature_req(Feature::Bump, Mode::Live, vec![BUMP_CHANNEL], None, vec![]),
                feature_req(
                    Feature::VoiceRooms,
                    Mode::Live,
                    vec![],
                    Some(VC_CATEGORY),
                    vec![ROLE_ID],
                ),
                feature_req(Feature::RoleMenus, Mode::Live, vec![], None, vec![ROLE_ID]),
                feature_req(Feature::Activity, Mode::Live, vec![], None, vec![]),
            ],
            guild_permissions: guild_bits(),
            channel_permissions: vec![
                PermissionLookup::known(FLASH_CHANNEL, channel_bits()),
                PermissionLookup::known(WELCOME_CHANNEL, channel_bits()),
                PermissionLookup::known(EMBED_CHANNEL, channel_bits()),
                PermissionLookup::known(BUMP_CHANNEL, channel_bits()),
            ],
            category_permissions: vec![PermissionLookup::known(VC_CATEGORY, category_bits())],
            role_facts: vec![RoleFactsEntry::found(
                ROLE_ID,
                RoleFacts {
                    id: ROLE_ID,
                    position: 5,
                    managed: false,
                    permissions: 0,
                    exists: true,
                },
            )],
            bot_top_position: Some(10),
        }
    }

    fn reasons_of(report: &Report, feature: Feature) -> Vec<Reason> {
        report
            .feature(feature)
            .map(|e| e.reasons.clone())
            .unwrap_or_default()
    }

    /// Assert that only `target` differs from the healthy base.
    fn assert_only_target_changed(report: &Report, target: Feature) {
        for feature in Feature::ALL {
            if feature == target {
                continue;
            }
            assert_eq!(
                report.status(feature),
                Some(Status::Ready),
                "feature {} must stay ready",
                feature.name()
            );
        }
    }

    // ---- Flash ----

    #[test]
    fn flash_channel_without_manage_messages_disables_only_flash() {
        let mut input = base_input();
        input.channel_permissions[0] = PermissionLookup::known(
            FLASH_CHANNEL,
            channel_bits() & !MANAGE_MESSAGES,
        );
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Flash), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Flash).contains(&Reason::MissingManageMessages));
        assert_only_target_changed(&report, Feature::Flash);
    }

    #[test]
    fn flash_without_configured_channels_is_disabled() {
        let mut input = base_input();
        input.features[0].channels.clear();
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Flash), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Flash).contains(&Reason::NoChannelsConfigured));
        assert_only_target_changed(&report, Feature::Flash);
    }

    #[test]
    fn flash_channel_lookup_failure_disables_flash() {
        let mut input = base_input();
        input.channel_permissions[0] = PermissionLookup::unknown(FLASH_CHANNEL);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Flash), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Flash).contains(&Reason::PermissionsUnknown));
        assert_only_target_changed(&report, Feature::Flash);
    }

    #[test]
    fn flash_channel_missing_from_the_lookup_table_disables_flash() {
        let mut input = base_input();
        input.channel_permissions[0] = PermissionLookup::known(EMBED_CHANNEL, channel_bits());
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Flash), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Flash).contains(&Reason::PermissionsUnknown));
    }

    // ---- Role-granting requirements ----

    #[test]
    fn missing_manage_roles_disables_every_role_granting_feature() {
        let mut input = base_input();
        input.guild_permissions &= !MANAGE_ROLES;
        let report = evaluate(&input);
        for feature in [
            Feature::Safeword,
            Feature::Welcome,
            Feature::VoiceRooms,
            Feature::RoleMenus,
        ] {
            assert_eq!(report.status(feature), Some(Status::Disabled));
            assert!(
                reasons_of(&report, feature).contains(&Reason::MissingManageRoles),
                "feature {} must report missing_manage_roles",
                feature.name()
            );
        }
        for feature in [Feature::Flash, Feature::Embeds, Feature::Bump, Feature::Activity] {
            assert_eq!(report.status(feature), Some(Status::Ready));
        }
    }

    #[test]
    fn administrator_bit_satisfies_the_manage_roles_check() {
        let mut input = base_input();
        input.guild_permissions = (input.guild_permissions & !MANAGE_ROLES) | ADMINISTRATOR;
        let report = evaluate(&input);
        for feature in [
            Feature::Safeword,
            Feature::Welcome,
            Feature::VoiceRooms,
            Feature::RoleMenus,
        ] {
            assert_eq!(report.status(feature), Some(Status::Ready));
            assert!(!reasons_of(&report, feature).contains(&Reason::MissingManageRoles));
        }
    }

    #[test]
    fn administrator_bit_satisfies_the_channel_checks() {
        let mut input = base_input();
        input.channel_permissions[0] = PermissionLookup::known(FLASH_CHANNEL, ADMINISTRATOR);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Flash), Some(Status::Ready));
    }

    #[test]
    fn a_role_above_the_bot_disables_only_that_feature() {
        let mut input = base_input();
        input.role_facts[0] = RoleFactsEntry::found(
            ROLE_ID,
            RoleFacts {
                id: ROLE_ID,
                position: 11,
                managed: false,
                permissions: 0,
                exists: true,
            },
        );
        let report = evaluate(&input);
        for feature in [
            Feature::Safeword,
            Feature::Welcome,
            Feature::VoiceRooms,
            Feature::RoleMenus,
        ] {
            assert_eq!(report.status(feature), Some(Status::Disabled));
            assert!(reasons_of(&report, feature).contains(&Reason::RoleAboveBot));
        }
        for feature in [Feature::Flash, Feature::Embeds, Feature::Bump, Feature::Activity] {
            assert_eq!(report.status(feature), Some(Status::Ready));
        }
    }

    #[test]
    fn a_role_at_the_bots_top_position_is_refused() {
        let mut input = base_input();
        input.role_facts[0] = RoleFactsEntry::found(
            ROLE_ID,
            RoleFacts {
                id: ROLE_ID,
                position: 10,
                managed: false,
                permissions: 0,
                exists: true,
            },
        );
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::RoleMenus), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::RoleMenus).contains(&Reason::RoleAboveBot));
    }

    #[test]
    fn a_role_below_the_bot_is_accepted() {
        let mut input = base_input();
        input.role_facts[0] = RoleFactsEntry::found(
            ROLE_ID,
            RoleFacts {
                id: ROLE_ID,
                position: 9,
                managed: false,
                permissions: 0,
                exists: true,
            },
        );
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::RoleMenus), Some(Status::Ready));
    }

    #[test]
    fn a_missing_role_disables_the_role_granting_features() {
        let mut input = base_input();
        input.role_facts[0] = RoleFactsEntry::missing(ROLE_ID);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::RoleMenus), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::RoleMenus).contains(&Reason::RoleMissing));
    }

    #[test]
    fn a_role_with_exists_false_is_refused_as_missing() {
        let mut input = base_input();
        input.role_facts[0] = RoleFactsEntry::found(
            ROLE_ID,
            RoleFacts {
                id: ROLE_ID,
                position: 1,
                managed: false,
                permissions: 0,
                exists: false,
            },
        );
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::RoleMenus), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::RoleMenus).contains(&Reason::RoleMissing));
    }

    #[test]
    fn unavailable_role_facts_disable_the_role_granting_features() {
        let mut input = base_input();
        input.role_facts[0] = RoleFactsEntry::unavailable(ROLE_ID);
        let report = evaluate(&input);
        for feature in [
            Feature::Safeword,
            Feature::Welcome,
            Feature::VoiceRooms,
            Feature::RoleMenus,
        ] {
            assert_eq!(report.status(feature), Some(Status::Disabled));
            assert!(reasons_of(&report, feature).contains(&Reason::RoleFactsUnavailable));
        }
    }

    #[test]
    fn a_configured_role_without_any_facts_entry_disables_the_feature() {
        let mut input = base_input();
        input.role_facts.clear();
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::RoleMenus), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::RoleMenus).contains(&Reason::RoleFactsUnavailable));
    }

    #[test]
    fn an_unknown_bot_top_position_disables_the_role_granting_features() {
        let mut input = base_input();
        input.bot_top_position = None;
        let report = evaluate(&input);
        for feature in [
            Feature::Safeword,
            Feature::Welcome,
            Feature::VoiceRooms,
            Feature::RoleMenus,
        ] {
            assert_eq!(report.status(feature), Some(Status::Disabled));
            assert!(
                reasons_of(&report, feature).contains(&Reason::BotTopPositionUnknown),
                "feature {} must report bot_top_position_unknown",
                feature.name()
            );
        }
        for feature in [Feature::Flash, Feature::Embeds, Feature::Bump, Feature::Activity] {
            assert_eq!(report.status(feature), Some(Status::Ready));
        }
    }

    #[test]
    fn one_bad_role_among_several_disables_the_feature() {
        let mut input = base_input();
        input.features[6].roles = vec![ROLE_ID, ROLE_ID_2];
        input.role_facts.push(RoleFactsEntry::missing(ROLE_ID_2));
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::RoleMenus), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::RoleMenus).contains(&Reason::RoleMissing));
    }

    // ---- Voice rooms ----

    #[test]
    fn voice_room_category_without_manage_channels_disables_only_voice_rooms() {
        let mut input = base_input();
        input.category_permissions[0] =
            PermissionLookup::known(VC_CATEGORY, category_bits() & !MANAGE_CHANNELS);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::VoiceRooms), Some(Status::Disabled));
        assert!(
            reasons_of(&report, Feature::VoiceRooms).contains(&Reason::MissingManageChannels)
        );
        assert_only_target_changed(&report, Feature::VoiceRooms);
    }

    #[test]
    fn voice_room_category_without_move_members_disables_voice_rooms() {
        let mut input = base_input();
        input.category_permissions[0] =
            PermissionLookup::known(VC_CATEGORY, category_bits() & !MOVE_MEMBERS);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::VoiceRooms), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::VoiceRooms).contains(&Reason::MissingMoveMembers));
    }

    #[test]
    fn voice_room_category_without_connect_disables_voice_rooms() {
        let mut input = base_input();
        input.category_permissions[0] =
            PermissionLookup::known(VC_CATEGORY, category_bits() & !CONNECT);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::VoiceRooms), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::VoiceRooms).contains(&Reason::MissingConnect));
    }

    #[test]
    fn voice_room_category_lookup_failure_disables_voice_rooms() {
        let mut input = base_input();
        input.category_permissions[0] = PermissionLookup::unknown(VC_CATEGORY);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::VoiceRooms), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::VoiceRooms).contains(&Reason::PermissionsUnknown));
    }

    #[test]
    fn voice_rooms_without_a_configured_category_is_disabled() {
        let mut input = base_input();
        input.features[5].category = None;
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::VoiceRooms), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::VoiceRooms).contains(&Reason::NoCategoryConfigured));
    }

    // ---- Welcome and Embeds ----

    #[test]
    fn welcome_channel_without_send_messages_disables_only_welcome() {
        let mut input = base_input();
        input.channel_permissions[1] =
            PermissionLookup::known(WELCOME_CHANNEL, channel_bits() & !SEND_MESSAGES);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Welcome), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Welcome).contains(&Reason::MissingSendMessages));
        assert_only_target_changed(&report, Feature::Welcome);
    }

    #[test]
    fn welcome_channel_without_embed_links_disables_welcome() {
        let mut input = base_input();
        input.channel_permissions[1] =
            PermissionLookup::known(WELCOME_CHANNEL, channel_bits() & !EMBED_LINKS);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Welcome), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Welcome).contains(&Reason::MissingEmbedLinks));
    }

    #[test]
    fn embeds_channel_without_embed_links_disables_only_embeds() {
        let mut input = base_input();
        input.channel_permissions[2] =
            PermissionLookup::known(EMBED_CHANNEL, channel_bits() & !EMBED_LINKS);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Embeds), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Embeds).contains(&Reason::MissingEmbedLinks));
        assert_only_target_changed(&report, Feature::Embeds);
    }

    #[test]
    fn embeds_channel_without_send_messages_disables_embeds() {
        let mut input = base_input();
        input.channel_permissions[2] =
            PermissionLookup::known(EMBED_CHANNEL, channel_bits() & !SEND_MESSAGES);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Embeds), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Embeds).contains(&Reason::MissingSendMessages));
    }

    #[test]
    fn welcome_without_a_configured_channel_is_disabled() {
        let mut input = base_input();
        input.features[2].channels.clear();
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Welcome), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Welcome).contains(&Reason::NoChannelsConfigured));
    }

    #[test]
    fn embeds_without_a_configured_channel_is_disabled() {
        let mut input = base_input();
        input.features[3].channels.clear();
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Embeds), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Embeds).contains(&Reason::NoChannelsConfigured));
    }

    // ---- Bump ----

    #[test]
    fn bump_channel_without_view_channel_disables_only_bump() {
        let mut input = base_input();
        input.channel_permissions[3] =
            PermissionLookup::known(BUMP_CHANNEL, channel_bits() & !VIEW_CHANNEL);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Bump), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Bump).contains(&Reason::MissingViewChannel));
        assert_only_target_changed(&report, Feature::Bump);
    }

    #[test]
    fn bump_channel_without_read_message_history_disables_bump() {
        let mut input = base_input();
        input.channel_permissions[3] =
            PermissionLookup::known(BUMP_CHANNEL, channel_bits() & !READ_MESSAGE_HISTORY);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Bump), Some(Status::Disabled));
        assert!(
            reasons_of(&report, Feature::Bump).contains(&Reason::MissingReadMessageHistory)
        );
    }

    #[test]
    fn bump_without_a_configured_channel_is_disabled() {
        let mut input = base_input();
        input.features[4].channels.clear();
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Bump), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Bump).contains(&Reason::NoChannelsConfigured));
    }

    // ---- Activity ----

    #[test]
    fn activity_without_view_channel_at_guild_level_is_disabled() {
        let mut input = base_input();
        input.guild_permissions &= !VIEW_CHANNEL;
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Activity), Some(Status::Disabled));
        assert!(reasons_of(&report, Feature::Activity).contains(&Reason::MissingViewChannel));
        assert_only_target_changed(&report, Feature::Activity);
    }

    // ---- Modes ----

    #[test]
    fn mode_off_never_produces_a_failure() {
        let mut input = base_input();
        // Break every requirement at once.
        input.guild_permissions = 0;
        input.channel_permissions.clear();
        input.category_permissions.clear();
        input.role_facts.clear();
        input.bot_top_position = None;
        for feature in &mut input.features {
            feature.mode = Mode::Off;
        }
        let report = evaluate(&input);
        for feature in Feature::ALL {
            assert_eq!(report.status(feature), Some(Status::Off));
            assert!(
                reasons_of(&report, feature).is_empty(),
                "feature {} must have no reasons",
                feature.name()
            );
        }
    }

    #[test]
    fn a_shadow_feature_that_meets_its_requirements_keeps_shadow() {
        let mut input = base_input();
        input.features[0].mode = Mode::Shadow;
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Flash), Some(Status::Ready));
        assert_eq!(report.effective_mode(Feature::Flash), Some(Mode::Shadow));
    }

    #[test]
    fn a_disabled_feature_has_the_effective_mode_off() {
        let mut input = base_input();
        input.features[0].mode = Mode::Live;
        input.channel_permissions[0] = PermissionLookup::unknown(FLASH_CHANNEL);
        let report = evaluate(&input);
        assert_eq!(report.effective_mode(Feature::Flash), Some(Mode::Off));
    }

    #[test]
    fn the_effective_mode_never_exceeds_the_requested_mode() {
        let mut input = base_input();
        for (index, feature) in input.features.iter_mut().enumerate() {
            feature.mode = match index % 3 {
                0 => Mode::Off,
                1 => Mode::Shadow,
                _ => Mode::Live,
            };
        }
        let report = evaluate(&input);
        for entry in &report.entries {
            assert!(
                mode_rank(entry.effective_mode()) <= mode_rank(entry.requested_mode),
                "feature {}: effective mode exceeds the requested one",
                entry.feature.name()
            );
        }
    }

    // ---- Fail closed ----

    #[test]
    fn unknown_facts_fail_closed() {
        let mut input = base_input();
        input.role_facts = vec![RoleFactsEntry::unavailable(ROLE_ID)];
        input.channel_permissions = vec![PermissionLookup::unknown(FLASH_CHANNEL)];
        input.category_permissions = vec![PermissionLookup::unknown(VC_CATEGORY)];
        input.bot_top_position = None;
        let report = evaluate(&input);
        for feature in Feature::ALL {
            assert_ne!(
                report.status(feature),
                Some(Status::Ready),
                "feature {} must not be ready when a fact is unknown",
                feature.name()
            );
        }
    }

    // ---- Report shape ----

    #[test]
    fn a_feature_that_was_not_requested_is_absent_from_the_report() {
        let mut input = base_input();
        input.features.retain(|f| f.feature != Feature::Bump);
        let report = evaluate(&input);
        assert_eq!(report.status(Feature::Bump), None);
        assert!(report.effective_mode(Feature::Bump).is_none());
    }

    #[test]
    fn the_first_request_for_a_feature_wins() {
        let mut input = base_input();
        input.features.push(feature_req(
            Feature::Bump,
            Mode::Live,
            vec![BUMP_CHANNEL],
            None,
            vec![],
        ));
        input.features.push(feature_req(
            Feature::Bump,
            Mode::Off,
            vec![BUMP_CHANNEL],
            None,
            vec![],
        ));
        let report = evaluate(&input);
        let bump_entries = report
            .entries
            .iter()
            .filter(|e| e.feature == Feature::Bump)
            .count();
        assert_eq!(bump_entries, 1);
        assert_eq!(report.status(Feature::Bump), Some(Status::Ready));
    }

    #[test]
    fn the_report_order_is_the_stable_feature_order() {
        let mut input = base_input();
        input.features.reverse();
        let report = evaluate(&input);
        let names: Vec<&str> = report.entries.iter().map(|e| e.feature.name()).collect();
        let expected: Vec<&str> = Feature::ALL.iter().map(|f| f.name()).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn the_reason_codes_are_sorted_and_free_of_duplicates() {
        let mut input = base_input();
        input.guild_permissions = 0;
        input.features[6].roles = vec![ROLE_ID, ROLE_ID_2];
        input.role_facts = vec![
            RoleFactsEntry::missing(ROLE_ID),
            RoleFactsEntry::missing(ROLE_ID_2),
        ];
        let report = evaluate(&input);
        let reasons = reasons_of(&report, Feature::RoleMenus);
        let mut sorted = reasons.clone();
        sorted.sort_by_key(|r| r.code());
        assert_eq!(reasons, sorted);
        let mut unique = reasons.clone();
        unique.dedup();
        assert_eq!(reasons.len(), unique.len());
    }

    #[test]
    fn the_reason_codes_are_stable() {
        assert_eq!(Reason::MissingManageRoles.code(), "missing_manage_roles");
        assert_eq!(Reason::RoleAboveBot.code(), "role_above_bot");
        assert_eq!(Reason::RoleMissing.code(), "role_missing");
        assert_eq!(
            Reason::RoleFactsUnavailable.code(),
            "role_facts_unavailable"
        );
        assert_eq!(
            Reason::BotTopPositionUnknown.code(),
            "bot_top_position_unknown"
        );
        assert_eq!(Reason::NoChannelsConfigured.code(), "no_channels_configured");
        assert_eq!(Reason::MissingManageMessages.code(), "missing_manage_messages");
        assert_eq!(Reason::MissingSendMessages.code(), "missing_send_messages");
        assert_eq!(Reason::MissingEmbedLinks.code(), "missing_embed_links");
        assert_eq!(Reason::MissingViewChannel.code(), "missing_view_channel");
        assert_eq!(
            Reason::MissingReadMessageHistory.code(),
            "missing_read_message_history"
        );
        assert_eq!(Reason::MissingManageChannels.code(), "missing_manage_channels");
        assert_eq!(Reason::MissingMoveMembers.code(), "missing_move_members");
        assert_eq!(Reason::MissingConnect.code(), "missing_connect");
        assert_eq!(Reason::PermissionsUnknown.code(), "permissions_unknown");
    }

    // ---- Privacy ----

    #[test]
    fn the_report_never_contains_ids_or_values() {
        // A sentinel id whose bytes spell "S3CR3T", used for every configured id.
        const SENTINEL: u64 = 0x5333_4352_3354;
        let mut input = base_input();
        input.features = vec![
            feature_req(Feature::Flash, Mode::Live, vec![SENTINEL], None, vec![]),
            feature_req(Feature::Safeword, Mode::Live, vec![], None, vec![SENTINEL]),
            feature_req(
                Feature::VoiceRooms,
                Mode::Live,
                vec![],
                Some(SENTINEL),
                vec![SENTINEL],
            ),
        ];
        input.channel_permissions = vec![PermissionLookup::unknown(SENTINEL)];
        input.category_permissions = vec![PermissionLookup::unknown(SENTINEL)];
        input.role_facts = vec![RoleFactsEntry::unavailable(SENTINEL)];
        let report = evaluate(&input);

        let lines = report.render_lines();
        let debug = format!("{:?}", report);
        let display = format!("{}", report);
        let decimal = format!("{}", SENTINEL);
        for output in [lines.join("\n"), debug, display] {
            assert!(!output.contains("S3CR3T"), "output leaked a sentinel name");
            assert!(
                !output.contains(&decimal),
                "output leaked the sentinel id in decimal"
            );
            assert!(
                !output.to_lowercase().contains("533343523354"),
                "output leaked the sentinel id in hex"
            );
        }
    }

    #[test]
    fn the_rendered_lines_hold_only_names_statuses_and_codes() {
        let mut input = base_input();
        input.channel_permissions[0] = PermissionLookup::unknown(FLASH_CHANNEL);
        let report = evaluate(&input);
        let lines = report.render_lines();
        assert_eq!(lines[0], "flash: disabled (permissions_unknown)");
        assert_eq!(lines[1], "safeword: ready");
        for line in &lines {
            assert!(line.starts_with("flash: ")
                || line.starts_with("safeword: ")
                || line.starts_with("welcome: ")
                || line.starts_with("embeds: ")
                || line.starts_with("bump: ")
                || line.starts_with("voice_rooms: ")
                || line.starts_with("role_menus: ")
                || line.starts_with("activity: "));
        }
    }

    // ---- Property test ----

    /// A small linear congruential generator. Seeded, so the sweep is reproducible.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 11
        }
    }

    fn random_mode(rng: &mut Lcg) -> Mode {
        match rng.next() % 3 {
            0 => Mode::Off,
            1 => Mode::Shadow,
            _ => Mode::Live,
        }
    }

    fn random_bits(rng: &mut Lcg) -> u64 {
        let high = rng.next() & 0xF;
        high << 32 | (rng.next() & 0xFFFF_FFFF)
    }

    #[test]
    fn property_effective_mode_never_exceeds_requested_and_unknowns_never_pass() {
        let mut rng = Lcg(0x5EED_1234_ABCD_0001);
        for round in 0..2000u32 {
            let mut input = base_input();
            for feature in &mut input.features {
                feature.mode = random_mode(&mut rng);
            }
            input.guild_permissions = random_bits(&mut rng);
            for lookup in &mut input.channel_permissions {
                lookup.permissions = if rng.next() % 4 == 0 {
                    None
                } else {
                    Some(random_bits(&mut rng))
                };
            }
            for lookup in &mut input.category_permissions {
                lookup.permissions = if rng.next() % 4 == 0 {
                    None
                } else {
                    Some(random_bits(&mut rng))
                };
            }
            input.bot_top_position = if rng.next() % 4 == 0 {
                None
            } else {
                Some((rng.next() % 32) as u16)
            };
            for entry in &mut input.role_facts {
                entry.outcome = match rng.next() % 3 {
                    0 => FactsOutcome::Unavailable,
                    1 => FactsOutcome::Missing,
                    _ => FactsOutcome::Found(RoleFacts {
                        id: ROLE_ID,
                        position: (rng.next() % 32) as u16,
                        managed: false,
                        permissions: 0,
                        exists: rng.next() % 8 != 0,
                    }),
                };
            }

            let report = evaluate(&input);
            for entry in &report.entries {
                assert!(
                    mode_rank(entry.effective_mode()) <= mode_rank(entry.requested_mode),
                    "round {}: feature {} effective mode exceeds the requested one",
                    round,
                    entry.feature.name()
                );
                assert_eq!(
                    entry.reasons.is_empty(),
                    entry.status == Status::Ready,
                    "round {}: feature {} has an inconsistent status",
                    round,
                    entry.feature.name()
                );
            }

            // With every input unknown, no feature may be ready.
            let mut unknown = base_input();
            unknown.guild_permissions = 0;
            unknown.channel_permissions = vec![PermissionLookup::unknown(FLASH_CHANNEL)];
            unknown.category_permissions = vec![PermissionLookup::unknown(VC_CATEGORY)];
            unknown.role_facts = vec![RoleFactsEntry::unavailable(ROLE_ID)];
            unknown.bot_top_position = None;
            let unknown_report = evaluate(&unknown);
            for entry in &unknown_report.entries {
                assert_ne!(
                    entry.status,
                    Status::Ready,
                    "round {}: feature {} was ready with every input unknown",
                    round,
                    entry.feature.name()
                );
            }
        }
    }
}

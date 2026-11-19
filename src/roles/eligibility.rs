//! Single decision function every role grant passes through.
//!
//! Rule precedence (first match wins; the returned reason is that rule's):
//! 1. role deleted -> `RoleDeleted`
//! 2. @everyone (role id == guild id) -> `EveryoneRole`; managed/integration -> `ManagedRole`
//! 3. role position >= bot top role -> `AtOrAboveBot`
//! 4. privileged permission bit on any automated path (all but `StaffCommand`) -> `PrivilegedPermission`
//! 5. role in protected set on any automated path -> `ProtectedRole`
//! 6. safeword Active + restricted role, on every path except `SafewordRestore` -> `SafewordRestricted`
//! 7. `StaffCommand`: actor must be strictly above role (`ActorNotAboveRole`) and target (`ActorNotAboveTarget`);
//!    missing staff context fails closed (`MissingStaffContext`).
//!
//! Removals use `decide_removal`: always allowed except when the bot cannot reach the role.

pub const ADMINISTRATOR: u64 = 1 << 3;
pub const MANAGE_GUILD: u64 = 1 << 5;
pub const MANAGE_ROLES: u64 = 1 << 28;
pub const BAN_MEMBERS: u64 = 1 << 2;
pub const KICK_MEMBERS: u64 = 1 << 1;
pub const MODERATE_MEMBERS: u64 = 1 << 40;
pub const PRIVILEGED_MASK: u64 =
    ADMINISTRATOR | MANAGE_GUILD | MANAGE_ROLES | BAN_MEMBERS | KICK_MEMBERS | MODERATE_MEMBERS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantPath {
    Reaction,
    Menu,
    JoinRole,
    XpReward,
    StaffCommand,
    VoiceRoomPermit,
    SafewordRestore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafewordState {
    Inactive,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleFacts {
    pub id: u64,
    pub position: u16,
    pub managed: bool,
    pub permissions: u64,
    pub exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaffContext {
    pub actor_top_position: u16,
    pub target_top_position: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct GrantRequest<'a> {
    pub path: GrantPath,
    pub role: RoleFacts,
    pub bot_top_position: u16,
    pub guild_id: u64,
    pub protected_role_ids: &'a [u64],
    pub safeword: SafewordState,
    pub safeword_restricted_role_ids: &'a [u64],
    /// Required for `StaffCommand`; ignored otherwise.
    pub staff: Option<StaffContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    RoleDeleted,
    EveryoneRole,
    ManagedRole,
    AtOrAboveBot,
    PrivilegedPermission,
    ProtectedRole,
    SafewordRestricted,
    MissingStaffContext,
    ActorNotAboveRole,
    ActorNotAboveTarget,
}

impl DenyReason {
    /// Stable machine code; never contains names or ids.
    pub fn code(self) -> &'static str {
        match self {
            DenyReason::RoleDeleted => "role_deleted",
            DenyReason::EveryoneRole => "everyone_role",
            DenyReason::ManagedRole => "managed_role",
            DenyReason::AtOrAboveBot => "at_or_above_bot",
            DenyReason::PrivilegedPermission => "privileged_permission",
            DenyReason::ProtectedRole => "protected_role",
            DenyReason::SafewordRestricted => "safeword_restricted",
            DenyReason::MissingStaffContext => "missing_staff_context",
            DenyReason::ActorNotAboveRole => "actor_not_above_role",
            DenyReason::ActorNotAboveTarget => "actor_not_above_target",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(DenyReason),
}

/// Decide whether a role may be granted.
pub fn decide(request: &GrantRequest) -> Decision {
    use DenyReason::*;
    let role = &request.role;
    let staff_path = request.path == GrantPath::StaffCommand;
    if !role.exists {
        return Decision::Deny(RoleDeleted);
    }
    if role.id == request.guild_id {
        return Decision::Deny(EveryoneRole);
    }
    if role.managed {
        return Decision::Deny(ManagedRole);
    }
    if role.position >= request.bot_top_position {
        return Decision::Deny(AtOrAboveBot);
    }
    if !staff_path {
        if role.permissions & PRIVILEGED_MASK != 0 {
            return Decision::Deny(PrivilegedPermission);
        }
        if request.protected_role_ids.contains(&role.id) {
            return Decision::Deny(ProtectedRole);
        }
    }
    if request.safeword == SafewordState::Active
        && request.path != GrantPath::SafewordRestore
        && request.safeword_restricted_role_ids.contains(&role.id)
    {
        return Decision::Deny(SafewordRestricted);
    }
    if staff_path {
        let Some(staff) = request.staff else {
            return Decision::Deny(MissingStaffContext);
        };
        if staff.actor_top_position <= role.position {
            return Decision::Deny(ActorNotAboveRole);
        }
        if staff.actor_top_position <= staff.target_top_position {
            return Decision::Deny(ActorNotAboveTarget);
        }
    }
    Decision::Allow
}

/// Decide whether a role may be removed. Safeword never blocks removals;
/// only the bot's own hierarchy can. A deleted role is allowed (nothing to reach).
pub fn decide_removal(request: &GrantRequest) -> Decision {
    if request.role.position >= request.bot_top_position {
        Decision::Deny(DenyReason::AtOrAboveBot)
    } else {
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: u64 = 1000;
    const ROLE: u64 = 2000;
    const AUTOMATED: [GrantPath; 6] = [
        GrantPath::Reaction,
        GrantPath::Menu,
        GrantPath::JoinRole,
        GrantPath::XpReward,
        GrantPath::VoiceRoomPermit,
        GrantPath::SafewordRestore,
    ];
    const ALL: [GrantPath; 7] = [
        GrantPath::Reaction,
        GrantPath::Menu,
        GrantPath::JoinRole,
        GrantPath::XpReward,
        GrantPath::StaffCommand,
        GrantPath::VoiceRoomPermit,
        GrantPath::SafewordRestore,
    ];
    const PRIV: [u64; 6] = [
        ADMINISTRATOR,
        MANAGE_GUILD,
        MANAGE_ROLES,
        BAN_MEMBERS,
        KICK_MEMBERS,
        MODERATE_MEMBERS,
    ];

    fn base(path: GrantPath) -> GrantRequest<'static> {
        GrantRequest {
            path,
            role: RoleFacts {
                id: ROLE,
                position: 5,
                managed: false,
                permissions: 0,
                exists: true,
            },
            bot_top_position: 10,
            guild_id: GUILD,
            protected_role_ids: &[],
            safeword: SafewordState::Inactive,
            safeword_restricted_role_ids: &[],
            staff: if path == GrantPath::StaffCommand {
                Some(StaffContext {
                    actor_top_position: 8,
                    target_top_position: 3,
                })
            } else {
                None
            },
        }
    }

    fn staff(actor: u16, target: u16) -> Option<StaffContext> {
        Some(StaffContext {
            actor_top_position: actor,
            target_top_position: target,
        })
    }

    #[test]
    fn baseline_allows_every_path() {
        for p in ALL {
            assert_eq!(decide(&base(p)), Decision::Allow, "{p:?}");
        }
    }

    #[test]
    fn bot_hierarchy_above_equal_below() {
        for p in ALL {
            let mut r = base(p);
            if p == GrantPath::StaffCommand {
                r.staff = staff(12, 3); // actor outranks every tested position
            }
            r.role.position = 9;
            assert_eq!(decide(&r), Decision::Allow);
            r.role.position = 10;
            assert_eq!(decide(&r), Decision::Deny(DenyReason::AtOrAboveBot));
            r.role.position = 11;
            assert_eq!(decide(&r), Decision::Deny(DenyReason::AtOrAboveBot));
        }
    }

    #[test]
    fn actor_hierarchy_on_staff_path() {
        let mut r = base(GrantPath::StaffCommand);
        r.staff = staff(5, 1);
        assert_eq!(decide(&r), Decision::Deny(DenyReason::ActorNotAboveRole));
        r.staff = staff(4, 1);
        assert_eq!(decide(&r), Decision::Deny(DenyReason::ActorNotAboveRole));
        r.staff = staff(8, 8);
        assert_eq!(decide(&r), Decision::Deny(DenyReason::ActorNotAboveTarget));
        r.staff = staff(8, 9);
        assert_eq!(decide(&r), Decision::Deny(DenyReason::ActorNotAboveTarget));
        r.staff = staff(6, 5);
        assert_eq!(decide(&r), Decision::Allow);
        r.staff = None;
        assert_eq!(decide(&r), Decision::Deny(DenyReason::MissingStaffContext));
    }

    #[test]
    fn staff_context_ignored_on_automated_paths() {
        let mut r = base(GrantPath::Reaction);
        r.staff = staff(0, 99);
        assert_eq!(decide(&r), Decision::Allow);
    }

    #[test]
    fn managed_denied_everywhere() {
        for p in ALL {
            let mut r = base(p);
            r.role.managed = true;
            assert_eq!(decide(&r), Decision::Deny(DenyReason::ManagedRole), "{p:?}");
        }
    }

    #[test]
    fn everyone_denied_everywhere() {
        for p in ALL {
            let mut r = base(p);
            r.role.id = GUILD;
            assert_eq!(
                decide(&r),
                Decision::Deny(DenyReason::EveryoneRole),
                "{p:?}"
            );
        }
    }

    #[test]
    fn privileged_bits_deny_automated_allow_staff() {
        for bit in PRIV {
            for p in AUTOMATED {
                let mut r = base(p);
                r.role.permissions = bit;
                assert_eq!(
                    decide(&r),
                    Decision::Deny(DenyReason::PrivilegedPermission),
                    "{p:?} {bit:#x}"
                );
            }
            let mut r = base(GrantPath::StaffCommand);
            r.role.permissions = bit | (1 << 10);
            assert_eq!(decide(&r), Decision::Allow, "{bit:#x}");
            r.staff = staff(5, 1);
            assert_eq!(decide(&r), Decision::Deny(DenyReason::ActorNotAboveRole));
        }
        let mut r = base(GrantPath::Reaction);
        r.role.permissions = !PRIVILEGED_MASK;
        assert_eq!(decide(&r), Decision::Allow, "non-privileged bits only");
    }

    #[test]
    fn privileged_bits_match_discord_wire_values() {
        for lit in [0x2u64, 0x4, 0x8, 0x20, 0x1000_0000, 0x100_0000_0000] {
            for p in AUTOMATED {
                let mut r = base(p);
                r.role.permissions = lit;
                assert_eq!(
                    decide(&r),
                    Decision::Deny(DenyReason::PrivilegedPermission),
                    "{p:?} {lit:#x}"
                );
            }
        }
        assert_eq!(
            PRIVILEGED_MASK,
            0x2 | 0x4 | 0x8 | 0x20 | 0x1000_0000 | 0x100_0000_0000
        );
    }

    #[test]
    fn protected_set_denies_automated() {
        let prot = [7u64, ROLE];
        for p in AUTOMATED {
            let mut r = base(p);
            r.protected_role_ids = &prot;
            assert_eq!(
                decide(&r),
                Decision::Deny(DenyReason::ProtectedRole),
                "{p:?}"
            );
        }
        let mut r = base(GrantPath::StaffCommand);
        r.protected_role_ids = &prot;
        assert_eq!(
            decide(&r),
            Decision::Allow,
            "outranking staff may grant protected"
        );
        r.staff = staff(5, 1);
        assert_eq!(decide(&r), Decision::Deny(DenyReason::ActorNotAboveRole));
        let mut r = base(GrantPath::Menu);
        r.protected_role_ids = &[7];
        assert_eq!(decide(&r), Decision::Allow, "other ids unaffected");
    }

    #[test]
    fn safeword_blocks_restricted_except_restore() {
        let restricted = [ROLE];
        for p in ALL {
            let mut r = base(p);
            r.safeword = SafewordState::Active;
            r.safeword_restricted_role_ids = &restricted;
            let want = if p == GrantPath::SafewordRestore {
                Decision::Allow
            } else {
                Decision::Deny(DenyReason::SafewordRestricted)
            };
            assert_eq!(decide(&r), want, "{p:?}");
            r.safeword_restricted_role_ids = &[1];
            assert_eq!(decide(&r), Decision::Allow, "unrestricted role {p:?}");
            r.safeword = SafewordState::Inactive;
            r.safeword_restricted_role_ids = &restricted;
            assert_eq!(decide(&r), Decision::Allow, "inactive {p:?}");
        }
    }

    #[test]
    fn removals_ignore_safeword_but_respect_bot_hierarchy() {
        for p in ALL {
            let mut r = base(p);
            r.safeword = SafewordState::Active;
            r.safeword_restricted_role_ids = &[ROLE];
            r.role.permissions = ADMINISTRATOR;
            r.protected_role_ids = &[ROLE];
            assert_eq!(decide_removal(&r), Decision::Allow, "{p:?}");
            r.role.position = 10;
            assert_eq!(decide_removal(&r), Decision::Deny(DenyReason::AtOrAboveBot));
            r.role.position = 11;
            assert_eq!(decide_removal(&r), Decision::Deny(DenyReason::AtOrAboveBot));
        }
    }

    #[test]
    fn deleted_role_denied() {
        for p in ALL {
            let mut r = base(p);
            r.role.exists = false;
            assert_eq!(decide(&r), Decision::Deny(DenyReason::RoleDeleted));
        }
    }

    #[test]
    fn precedence_table() {
        use DenyReason::*;
        // (deleted, everyone, managed, above_bot, privileged, protected, safeword, expected)
        let table = [
            (true, true, true, true, true, true, true, RoleDeleted),
            (false, true, true, true, true, true, true, EveryoneRole),
            (false, false, true, true, true, true, true, ManagedRole),
            (false, false, false, true, true, true, true, AtOrAboveBot),
            (
                false,
                false,
                false,
                false,
                true,
                true,
                true,
                PrivilegedPermission,
            ),
            (false, false, false, false, false, true, true, ProtectedRole),
            (
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                SafewordRestricted,
            ),
        ];
        for (del, ev, man, above, privd, prot, sw, want) in table {
            let mut r = base(GrantPath::Reaction);
            r.role.exists = !del;
            if ev {
                r.role.id = GUILD;
            }
            r.role.managed = man;
            if above {
                r.role.position = 10;
            }
            if privd {
                r.role.permissions = MANAGE_ROLES;
            }
            if prot {
                r.protected_role_ids = &[GUILD, ROLE];
            }
            if sw {
                r.safeword = SafewordState::Active;
                r.safeword_restricted_role_ids = &[GUILD, ROLE];
            }
            assert_eq!(decide(&r), Decision::Deny(want));
        }
        // Staff path: safeword (6) beats hierarchy (7).
        let mut r = base(GrantPath::StaffCommand);
        r.safeword = SafewordState::Active;
        r.safeword_restricted_role_ids = &[ROLE];
        r.staff = staff(1, 1);
        assert_eq!(decide(&r), Decision::Deny(SafewordRestricted));
    }

    #[test]
    fn deny_codes_are_stable_and_data_free() {
        use DenyReason::*;
        let all = [
            (RoleDeleted, "role_deleted"),
            (EveryoneRole, "everyone_role"),
            (ManagedRole, "managed_role"),
            (AtOrAboveBot, "at_or_above_bot"),
            (PrivilegedPermission, "privileged_permission"),
            (ProtectedRole, "protected_role"),
            (SafewordRestricted, "safeword_restricted"),
            (MissingStaffContext, "missing_staff_context"),
            (ActorNotAboveRole, "actor_not_above_role"),
            (ActorNotAboveTarget, "actor_not_above_target"),
        ];
        for (reason, code) in all {
            assert_eq!(reason.code(), code);
            assert!(!reason.code().chars().any(|c| c.is_ascii_digit()));
        }
        assert_eq!(std::mem::size_of::<DenyReason>(), 1);
    }

    #[test]
    fn generated_sweep_invariants() {
        // Deterministic LCG; no external randomness.
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s >> 33
        };
        let mut allows = 0;
        for _ in 0..20_000 {
            let path = ALL[(next() % 7) as usize];
            let role_id = [GUILD, ROLE, 3000][(next() % 3) as usize];
            let restricted: &[u64] = if next() % 2 == 0 { &[ROLE, 3000] } else { &[] };
            let protected: &[u64] = if next() % 4 == 0 { &[3000] } else { &[] };
            let r = GrantRequest {
                path,
                role: RoleFacts {
                    id: role_id,
                    position: (next() % 14) as u16,
                    managed: next() % 5 == 0,
                    permissions: if next() % 2 == 0 {
                        0
                    } else {
                        1u64 << (next() % 45)
                    },
                    exists: next() % 10 != 0,
                },
                bot_top_position: 10,
                guild_id: GUILD,
                protected_role_ids: protected,
                safeword: if next() % 2 == 0 {
                    SafewordState::Active
                } else {
                    SafewordState::Inactive
                },
                safeword_restricted_role_ids: restricted,
                staff: staff((next() % 14) as u16, (next() % 14) as u16),
            };
            if decide(&r) == Decision::Allow {
                allows += 1;
                assert!(r.role.exists);
                assert_ne!(r.role.id, GUILD);
                assert!(!r.role.managed);
                assert!(r.role.position < 10);
                if path != GrantPath::StaffCommand {
                    assert_eq!(r.role.permissions & PRIVILEGED_MASK, 0);
                    assert!(!protected.contains(&r.role.id));
                } else {
                    let st = r.staff.unwrap();
                    assert!(st.actor_top_position > r.role.position);
                    assert!(st.actor_top_position > st.target_top_position);
                }
                if r.safeword == SafewordState::Active && restricted.contains(&r.role.id) {
                    assert_eq!(path, GrantPath::SafewordRestore);
                }
            }
            let rm = decide_removal(&r);
            if r.role.position >= 10 {
                assert_eq!(rm, Decision::Deny(DenyReason::AtOrAboveBot));
            } else {
                assert_eq!(rm, Decision::Allow);
            }
        }
        assert!(
            allows > 100,
            "sweep must exercise Allow paths, got {allows}"
        );
    }
}

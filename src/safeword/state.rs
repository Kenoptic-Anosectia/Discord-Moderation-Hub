//! Pure, deterministic state machine for the safeword feature (SW1).
//!
//! Replaces the YAGPDB custom command with replay-safe snapshotting, configurable
//! restore filtering (struck, deleted, and protected roles), optional grant
//! deferral while active, observable per-action acknowledgements, and deterministic
//! crash-recovery actions. Holds no state, performs no I/O, and uses no Discord types.

/// Synthetic-friendly Discord role identifier.
pub type RoleId = u64;

/// Exact DM sent when safeword becomes active (YAGPDB line 94 parity).
pub const ACTIVE_DM_TEXT: &str = "Your Safeword is active. NSFW areas are now hidden.";

/// Exact DM sent when safeword is released (YAGPDB line 110 parity).
pub const RELEASED_DM_TEXT: &str =
    "Your Safeword is no longer active. Your normal channel visibility has been restored.";

/// Direct-message templates emitted by the safeword state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DmKind {
    Activated,
    Released,
}

impl DmKind {
    /// Returns the exact user-facing DM text for this notification kind.
    pub const fn text(self) -> &'static str {
        match self {
            Self::Activated => ACTIVE_DM_TEXT,
            Self::Released => RELEASED_DM_TEXT,
        }
    }
}

/// Stable identifier for an emitted action, used by `ActionDone` and `ActionFailed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActionId {
    SaveSnapshot,
    RemoveRoles,
    AddSafewordRole,
    SendActivatedDm,
    RemoveSafewordRole,
    RestoreRoles,
    ClearSnapshot,
    SendReleasedDm,
}

/// Pure, user-data-free failure reason code for `Event::ActionFailed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailureCode {
    StorageError,
    MissingPermissions,
    HierarchyRejected,
    RateLimited,
    DmClosed,
    TransientHttp,
    UnknownRole,
}

/// Declarative side-effect emitted by `step`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    SaveSnapshot,
    RemoveRoles(Vec<RoleId>),
    AddSafewordRole,
    SendDm(DmKind),
    RemoveSafewordRole,
    RestoreRoles(Vec<RoleId>),
    ClearSnapshot,
}

impl Action {
    /// Stable action identifier corresponding to this action.
    pub const fn id(&self) -> ActionId {
        match self {
            Self::SaveSnapshot => ActionId::SaveSnapshot,
            Self::RemoveRoles(_) => ActionId::RemoveRoles,
            Self::AddSafewordRole => ActionId::AddSafewordRole,
            Self::SendDm(DmKind::Activated) => ActionId::SendActivatedDm,
            Self::RemoveSafewordRole => ActionId::RemoveSafewordRole,
            Self::RestoreRoles(_) => ActionId::RestoreRoles,
            Self::ClearSnapshot => ActionId::ClearSnapshot,
            Self::SendDm(DmKind::Released) => ActionId::SendReleasedDm,
        }
    }
}

/// Input event driving the safeword state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    ReactionAdded,
    ReactionRemoved,
    Strike(RoleId),
    RoleDeleted(RoleId),
    GrantDuringActive(RoleId),
    ActionDone(ActionId),
    ActionFailed(ActionId, FailureCode),
}

/// Plain data snapshot tracking saved, struck, deleted, and deferred roles plus step progress.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Active saved roles to restore when `filter_restore` is enabled (shrinks only on Strike/RoleDeleted).
    pub saved_roles: Vec<RoleId>,
    /// Unfiltered union of initially snapshotted roles and deferred grants (used when `filter_restore` is false).
    pub original_roles: Vec<RoleId>,
    /// Roles explicitly struck by staff while safeword was active.
    pub struck_roles: Vec<RoleId>,
    /// Roles deleted from the guild while safeword was active.
    pub deleted_roles: Vec<RoleId>,
    /// Roles granted while safeword was active and deferred until release.
    pub deferred_roles: Vec<RoleId>,
    /// Computed role list to restore during the current `Releasing` phase.
    pub restore_plan: Vec<RoleId>,
    /// Activation step acknowledgements.
    pub snapshot_saved: bool,
    pub roles_removed: bool,
    pub safeword_added: bool,
    pub dm_sent: bool,
    /// True if `ReactionRemoved` arrived while still `Activating`.
    pub pending_release: bool,
    /// Release step acknowledgements.
    pub safeword_removed: bool,
    pub roles_restored: bool,
    pub snapshot_cleared: bool,
    pub release_dm_sent: bool,
    /// Most recent unrecovered action failure.
    pub last_failure: Option<(ActionId, FailureCode)>,
}

/// Safeword lifecycle state for one member.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Inactive,
    Activating(Snapshot),
    Active(Snapshot),
    Releasing(Snapshot),
}

impl State {
    /// Borrows the underlying snapshot when in `Activating`, `Active`, or `Releasing`.
    pub const fn snapshot(&self) -> Option<&Snapshot> {
        match self {
            Self::Inactive => None,
            Self::Activating(s) | Self::Active(s) | Self::Releasing(s) => Some(s),
        }
    }
}

/// Caller-supplied facts and policy flags for a state transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    /// Role ID of the safeword role.
    pub safeword_role: RoleId,
    /// Configured restricted role set hidden by safeword.
    pub restricted_roles: Vec<RoleId>,
    /// Roles currently marked protected (never restored when `filter_restore` is true).
    pub protected_roles: Vec<RoleId>,
    /// Roles the member currently holds.
    pub held_roles: Vec<RoleId>,
    /// When true (default), restore skips struck, deleted, and now-protected roles.
    pub filter_restore: bool,
    /// When true (default), `GrantDuringActive` defers the role into the snapshot.
    pub defer_grants: bool,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            safeword_role: 900,
            restricted_roles: Vec::new(),
            protected_roles: Vec::new(),
            held_roles: Vec::new(),
            filter_restore: true,
            defer_grants: true,
        }
    }
}

/// Single deterministic recovery action chosen after a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartAction {
    NoOp,
    Trigger(Event),
    Execute(Action),
}

/// True only when a reaction event came from a non-bot user on the configured message and emoji.
pub const fn is_actionable(
    _author_is_bot: bool,
    _message_id_matches: bool,
    _emoji_matches: bool,
) -> bool {
    false
}

/// Advances the pure safeword state machine by one event.
pub fn step(state: &State, _event: Event, _ctx: &Context) -> (State, Vec<Action>) {
    (state.clone(), Vec::new())
}

/// Computes the single deterministic next action on startup from stored state and live reaction presence.
pub fn restart_action(_state: &State, _reaction_present_now: bool) -> RestartAction {
    RestartAction::NoOp
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAFEWORD_ROLE: RoleId = 900;
    const ROLE_VERIFIED: RoleId = 101;
    const ROLE_MEMBER: RoleId = 102;
    const ROLE_MEDIA: RoleId = 103;
    const ROLE_FEMDOM: RoleId = 104;
    const ROLE_MALEDOM: RoleId = 105;
    const ROLE_UNRESTRICTED: RoleId = 201;
    const ROLE_DEFERRED: RoleId = 106;

    fn sample_ctx(held_roles: Vec<RoleId>) -> Context {
        Context {
            safeword_role: SAFEWORD_ROLE,
            restricted_roles: vec![
                ROLE_VERIFIED,
                ROLE_MEMBER,
                ROLE_MEDIA,
                ROLE_FEMDOM,
                ROLE_MALEDOM,
            ],
            protected_roles: Vec::new(),
            held_roles,
            filter_restore: true,
            defer_grants: true,
        }
    }

    fn acknowledge_all(mut state: State, actions: &[Action], ctx: &Context) -> (State, Vec<Action>) {
        let mut follow_up = Vec::new();
        for action in actions {
            let (next, emitted) = step(&state, Event::ActionDone(action.id()), ctx);
            state = next;
            follow_up.extend(emitted);
        }
        (state, follow_up)
    }

    #[test]
    fn activation_snapshots_held_roles_intersected_with_restricted_set() {
        let ctx = sample_ctx(vec![
            ROLE_MALEDOM,
            ROLE_UNRESTRICTED,
            ROLE_VERIFIED,
            ROLE_VERIFIED,
        ]);
        let (state, actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);

        let snap = state.snapshot().expect("must enter Activating with snapshot");
        assert_eq!(snap.saved_roles, vec![ROLE_VERIFIED, ROLE_MALEDOM]);
        assert_eq!(
            actions,
            vec![
                Action::SaveSnapshot,
                Action::RemoveRoles(vec![ROLE_VERIFIED, ROLE_MALEDOM]),
                Action::AddSafewordRole,
                Action::SendDm(DmKind::Activated),
            ]
        );

        // If safeword role is already held, AddSafewordRole is omitted (YAGPDB line 90).
        let ctx_already_holding = sample_ctx(vec![ROLE_VERIFIED, SAFEWORD_ROLE]);
        let (_, actions_held) = step(&State::Inactive, Event::ReactionAdded, &ctx_already_holding);
        assert!(!actions_held.contains(&Action::AddSafewordRole));
    }

    #[test]
    fn replayed_add_while_activating_or_active_does_not_overwrite_snapshot() {
        // YAGPDB line 68 runs `dbSet` unconditionally on every ReactionAdded, which overwrites
        // the saved snapshot with an empty list once the restricted roles have been removed.
        // This test fails on the YAGPDB behaviour and asserts our state machine preserves the snapshot.
        let initial_ctx = sample_ctx(vec![ROLE_VERIFIED, ROLE_FEMDOM]);
        let (activating, actions) = step(&State::Inactive, Event::ReactionAdded, &initial_ctx);

        // Simulate Discord state after roles were removed: member now holds only SAFEWORD_ROLE.
        let post_remove_ctx = sample_ctx(vec![SAFEWORD_ROLE]);

        let (still_activating, replay_actions_1) =
            step(&activating, Event::ReactionAdded, &post_remove_ctx);
        assert!(replay_actions_1.is_empty());
        assert_eq!(
            still_activating.snapshot().unwrap().saved_roles,
            vec![ROLE_VERIFIED, ROLE_FEMDOM]
        );

        let (active, _) = acknowledge_all(still_activating, &actions, &post_remove_ctx);
        assert!(matches!(active, State::Active(_)));

        let (still_active, replay_actions_2) =
            step(&active, Event::ReactionAdded, &post_remove_ctx);
        assert!(replay_actions_2.is_empty());
        assert_eq!(
            still_active.snapshot().unwrap().saved_roles,
            vec![ROLE_VERIFIED, ROLE_FEMDOM]
        );
    }

    #[test]
    fn remove_while_inactive_is_a_noop() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED]);
        let (state, actions) = step(&State::Inactive, Event::ReactionRemoved, &ctx);
        assert_eq!(state, State::Inactive);
        assert!(actions.is_empty());
    }

    #[test]
    fn remove_during_activating_completes_activation_steps_then_releases() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED, ROLE_MEMBER]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        assert!(matches!(activating, State::Activating(_)));

        // Acknowledge only the first activation step before ReactionRemoved arrives.
        let (activating, _) =
            step(&activating, Event::ActionDone(ActionId::SaveSnapshot), &ctx);
        let (activating_with_release, immediate) =
            step(&activating, Event::ReactionRemoved, &ctx);
        assert!(matches!(activating_with_release, State::Activating(_)));
        assert!(immediate.is_empty());

        // Completing the remaining activation steps transitions straight into Releasing.
        let (releasing, release_actions) =
            acknowledge_all(activating_with_release, &act_actions[1..], &ctx);
        assert!(matches!(releasing, State::Releasing(_)));
        assert_eq!(
            release_actions,
            vec![
                Action::RemoveSafewordRole,
                Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER]),
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ]
        );

        let (final_state, trailing) = acknowledge_all(releasing, &release_actions, &ctx);
        assert_eq!(final_state, State::Inactive);
        assert!(trailing.is_empty());
    }

    #[test]
    fn restore_skips_struck_deleted_and_now_protected_roles() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED, ROLE_MEMBER, ROLE_MEDIA, ROLE_FEMDOM]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating, &act_actions, &ctx);

        // Staff strikes ROLE_VERIFIED; guild deletes ROLE_MEMBER; ROLE_MEDIA becomes protected.
        let (active, _) = step(&active, Event::Strike(ROLE_VERIFIED), &ctx);
        let (active, _) = step(&active, Event::RoleDeleted(ROLE_MEMBER), &ctx);

        let mut release_ctx = ctx.clone();
        release_ctx.protected_roles = vec![ROLE_MEDIA];

        let (releasing, release_actions) =
            step(&active, Event::ReactionRemoved, &release_ctx);
        assert!(matches!(releasing, State::Releasing(_)));
        assert_eq!(
            release_actions,
            vec![
                Action::RemoveSafewordRole,
                Action::RestoreRoles(vec![ROLE_FEMDOM]),
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ]
        );

        // When filter_restore is false ('give everything back' flag), all saved roles return.
        let mut blind_ctx = release_ctx;
        blind_ctx.filter_restore = false;
        let (_, blind_actions) = step(&active, Event::ReactionRemoved, &blind_ctx);
        assert_eq!(
            blind_actions,
            vec![
                Action::RemoveSafewordRole,
                Action::RestoreRoles(vec![
                    ROLE_VERIFIED,
                    ROLE_MEMBER,
                    ROLE_MEDIA,
                    ROLE_FEMDOM,
                ]),
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ]
        );
    }

    #[test]
    fn empty_snapshot_still_toggles_safeword_role_and_sends_both_dms() {
        let ctx = sample_ctx(vec![ROLE_UNRESTRICTED]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        assert_eq!(activating.snapshot().unwrap().saved_roles, Vec::<RoleId>::new());
        assert_eq!(
            act_actions,
            vec![
                Action::SaveSnapshot,
                Action::AddSafewordRole,
                Action::SendDm(DmKind::Activated),
            ]
        );

        let (active, _) = acknowledge_all(activating, &act_actions, &ctx);
        let (releasing, rel_actions) = step(&active, Event::ReactionRemoved, &ctx);
        assert_eq!(
            rel_actions,
            vec![
                Action::RemoveSafewordRole,
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ]
        );

        let (inactive, _) = acknowledge_all(releasing, &rel_actions, &ctx);
        assert_eq!(inactive, State::Inactive);
    }

    #[test]
    fn grant_during_active_is_deferred_when_enabled_and_ignored_when_disabled() {
        let mut ctx = sample_ctx(vec![ROLE_MEMBER]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating, &act_actions, &ctx);

        // Deferral enabled (default true): ROLE_DEFERRED is recorded into snapshot.
        let (active_deferred, defer_actions) =
            step(&active, Event::GrantDuringActive(ROLE_DEFERRED), &ctx);
        assert_eq!(defer_actions, vec![Action::SaveSnapshot]);
        assert_eq!(
            active_deferred.snapshot().unwrap().saved_roles,
            vec![ROLE_MEMBER, ROLE_DEFERRED]
        );
        let (_, release_actions) = step(&active_deferred, Event::ReactionRemoved, &ctx);
        assert!(release_actions.contains(&Action::RestoreRoles(vec![
            ROLE_MEMBER,
            ROLE_DEFERRED
        ])));

        // Deferral disabled: GrantDuringActive is ignored.
        ctx.defer_grants = false;
        let (active_ignored, ignore_actions) =
            step(&active, Event::GrantDuringActive(ROLE_DEFERRED), &ctx);
        assert!(ignore_actions.is_empty());
        assert_eq!(active_ignored.snapshot().unwrap().saved_roles, vec![ROLE_MEMBER]);
    }

    #[test]
    fn restart_in_every_state_gives_one_deterministic_next_action() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating.clone(), &act_actions, &ctx);
        let (releasing, _) = step(&active, Event::ReactionRemoved, &ctx);

        // Inactive
        assert_eq!(
            restart_action(&State::Inactive, true),
            RestartAction::Trigger(Event::ReactionAdded)
        );
        assert_eq!(restart_action(&State::Inactive, false), RestartAction::NoOp);

        // Activating
        assert_eq!(
            restart_action(&activating, true),
            RestartAction::Execute(Action::SaveSnapshot)
        );
        assert_eq!(
            restart_action(&activating, false),
            RestartAction::Trigger(Event::ReactionRemoved)
        );

        // Active
        assert_eq!(restart_action(&active, true), RestartAction::NoOp);
        assert_eq!(
            restart_action(&active, false),
            RestartAction::Trigger(Event::ReactionRemoved)
        );

        // Releasing
        assert_eq!(
            restart_action(&releasing, false),
            RestartAction::Execute(Action::RemoveSafewordRole)
        );
        assert_eq!(
            restart_action(&releasing, true),
            RestartAction::Execute(Action::RemoveSafewordRole)
        );
    }

    #[test]
    fn action_failure_leaves_state_retryable_and_never_repeats_acknowledged_actions() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED]);
        let (activating, _) = step(&State::Inactive, Event::ReactionAdded, &ctx);

        // Acknowledge SaveSnapshot and RemoveRoles, then fail AddSafewordRole.
        let (activating, _) =
            step(&activating, Event::ActionDone(ActionId::SaveSnapshot), &ctx);
        let (activating, _) =
            step(&activating, Event::ActionDone(ActionId::RemoveRoles), &ctx);
        let (failed_state, fail_emitted) = step(
            &activating,
            Event::ActionFailed(ActionId::AddSafewordRole, FailureCode::TransientHttp),
            &ctx,
        );
        assert!(fail_emitted.is_empty());
        assert_eq!(
            failed_state.snapshot().unwrap().last_failure,
            Some((ActionId::AddSafewordRole, FailureCode::TransientHttp))
        );

        // Retry via ReactionAdded: only unacknowledged actions are emitted.
        let (retried_state, retry_actions) =
            step(&failed_state, Event::ReactionAdded, &ctx);
        assert_eq!(
            retry_actions,
            vec![Action::AddSafewordRole, Action::SendDm(DmKind::Activated)]
        );
        assert!(retried_state.snapshot().unwrap().last_failure.is_none());

        let (active, _) = acknowledge_all(retried_state, &retry_actions, &ctx);
        assert!(matches!(active, State::Active(_)));
    }

    #[test]
    fn dm_texts_equal_required_strings_byte_for_byte() {
        assert_eq!(
            DmKind::Activated.text().as_bytes(),
            b"Your Safeword is active. NSFW areas are now hidden."
        );
        assert_eq!(
            DmKind::Released.text().as_bytes(),
            b"Your Safeword is no longer active. Your normal channel visibility has been restored."
        );
    }

    #[test]
    fn bots_and_non_matching_reactions_are_ignored_by_is_actionable() {
        assert!(is_actionable(false, true, true));
        assert!(!is_actionable(true, true, true));
        assert!(!is_actionable(false, false, true));
        assert!(!is_actionable(false, true, false));
        assert!(!is_actionable(true, false, false));
    }

    #[test]
    fn table_driven_sweep_over_all_state_event_pairs_upholds_invariants() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED, ROLE_MEMBER]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating.clone(), &act_actions, &ctx);
        let (releasing, _) = step(&active, Event::ReactionRemoved, &ctx);

        let states = vec![State::Inactive, activating, active, releasing];
        let events = vec![
            Event::ReactionAdded,
            Event::ReactionRemoved,
            Event::Strike(ROLE_VERIFIED),
            Event::RoleDeleted(ROLE_MEMBER),
            Event::GrantDuringActive(ROLE_DEFERRED),
            Event::ActionDone(ActionId::SaveSnapshot),
            Event::ActionDone(ActionId::RemoveRoles),
            Event::ActionDone(ActionId::AddSafewordRole),
            Event::ActionDone(ActionId::SendActivatedDm),
            Event::ActionDone(ActionId::RemoveSafewordRole),
            Event::ActionDone(ActionId::RestoreRoles),
            Event::ActionDone(ActionId::ClearSnapshot),
            Event::ActionDone(ActionId::SendReleasedDm),
            Event::ActionFailed(ActionId::AddSafewordRole, FailureCode::TransientHttp),
            Event::ActionFailed(ActionId::RestoreRoles, FailureCode::RateLimited),
        ];

        for state in &states {
            for &event in &events {
                let (next_state, actions) = step(state, event, &ctx);
                let add_count = actions
                    .iter()
                    .filter(|a| matches!(a, Action::AddSafewordRole))
                    .count();
                assert!(add_count <= 1, "AddSafewordRole emitted more than once");

                if let (State::Active(before), State::Active(after)) = (state, &next_state) {
                    if !matches!(event, Event::Strike(_) | Event::RoleDeleted(_)) {
                        assert!(
                            after.saved_roles.len() >= before.saved_roles.len()
                                && before
                                    .saved_roles
                                    .iter()
                                    .all(|r| after.saved_roles.contains(r)),
                            "snapshot shrank while Active on {event:?}"
                        );
                    }
                }
            }
        }

        // Across a complete activation with replayed ReactionAdded in Activating and Active,
        // AddSafewordRole is emitted at most once.
        let mut total_adds = 0usize;
        let (s1, a1) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        total_adds += a1.iter().filter(|a| matches!(a, Action::AddSafewordRole)).count();
        let (s2, a2) = step(&s1, Event::ReactionAdded, &ctx);
        total_adds += a2.iter().filter(|a| matches!(a, Action::AddSafewordRole)).count();
        let (s3, a3) = acknowledge_all(s2, &a1, &ctx);
        total_adds += a3.iter().filter(|a| matches!(a, Action::AddSafewordRole)).count();
        let (_s4, a4) = step(&s3, Event::ReactionAdded, &ctx);
        total_adds += a4.iter().filter(|a| matches!(a, Action::AddSafewordRole)).count();
        assert_eq!(total_adds, 1);
    }
}

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
    /// True if `ReactionAdded` arrived while still `Releasing` (cleared by a later `ReactionRemoved`).
    pub pending_reactivate: bool,
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

impl Snapshot {
    fn new_from_context(ctx: &Context) -> Self {
        let mut saved: Vec<RoleId> = ctx
            .held_roles
            .iter()
            .copied()
            .filter(|r| *r != 0 && *r != ctx.safeword_role && ctx.restricted_roles.contains(r))
            .collect();
        saved.sort_unstable();
        saved.dedup();

        let already_holds_safeword =
            ctx.safeword_role != 0 && ctx.held_roles.contains(&ctx.safeword_role);

        Self {
            original_roles: saved.clone(),
            roles_removed: saved.is_empty(),
            safeword_added: already_holds_safeword,
            saved_roles: saved,
            struck_roles: Vec::new(),
            deleted_roles: Vec::new(),
            deferred_roles: Vec::new(),
            restore_plan: Vec::new(),
            snapshot_saved: false,
            dm_sent: false,
            pending_release: false,
            pending_reactivate: false,
            safeword_removed: false,
            roles_restored: true,
            snapshot_cleared: false,
            release_dm_sent: false,
            last_failure: None,
        }
    }

    fn activation_complete(&self) -> bool {
        self.snapshot_saved && self.roles_removed && self.safeword_added && self.dm_sent
    }

    fn release_complete(&self) -> bool {
        self.safeword_removed
            && self.roles_restored
            && self.snapshot_cleared
            && self.release_dm_sent
    }

    fn pending_activation_actions(&self) -> Vec<Action> {
        let mut actions = Vec::new();
        if !self.snapshot_saved {
            actions.push(Action::SaveSnapshot);
        }
        if !self.roles_removed && !self.saved_roles.is_empty() {
            actions.push(Action::RemoveRoles(self.saved_roles.clone()));
        }
        if !self.safeword_added {
            actions.push(Action::AddSafewordRole);
        }
        if !self.dm_sent {
            actions.push(Action::SendDm(DmKind::Activated));
        }
        actions
    }

    fn pending_release_actions(&self) -> Vec<Action> {
        let mut actions = Vec::new();
        if !self.safeword_removed {
            actions.push(Action::RemoveSafewordRole);
        }
        if !self.roles_restored && !self.restore_plan.is_empty() {
            actions.push(Action::RestoreRoles(self.restore_plan.clone()));
        }
        if !self.snapshot_cleared {
            actions.push(Action::ClearSnapshot);
        }
        if !self.release_dm_sent {
            actions.push(Action::SendDm(DmKind::Released));
        }
        actions
    }

    fn compute_restore_roles(&self, ctx: &Context) -> Vec<RoleId> {
        if !ctx.filter_restore {
            return self.original_roles.clone();
        }
        self.saved_roles
            .iter()
            .copied()
            .filter(|r| {
                !self.struck_roles.contains(r)
                    && !self.deleted_roles.contains(r)
                    && !ctx.protected_roles.contains(r)
            })
            .collect()
    }

    fn enter_releasing(mut self, ctx: &Context) -> (State, Vec<Action>) {
        let restore_plan = self.compute_restore_roles(ctx);
        self.pending_release = false;
        self.pending_reactivate = false;
        self.last_failure = None;
        self.safeword_removed = false;
        self.roles_restored = restore_plan.is_empty();
        self.snapshot_cleared = false;
        self.release_dm_sent = false;
        self.restore_plan = restore_plan;
        let actions = self.pending_release_actions();
        (State::Releasing(self), actions)
    }

    fn apply_strike(&mut self, role: RoleId) -> bool {
        if role == 0 {
            return false;
        }
        let mut changed = false;
        if !self.struck_roles.contains(&role) {
            self.struck_roles.push(role);
            self.struck_roles.sort_unstable();
            changed = true;
        }
        let before_saved = self.saved_roles.len();
        self.saved_roles.retain(|&r| r != role);
        if self.saved_roles.len() != before_saved {
            changed = true;
        }
        self.deferred_roles.retain(|&r| r != role);
        if self.saved_roles.is_empty() {
            self.roles_removed = true;
        }
        changed
    }

    fn apply_role_deleted(&mut self, role: RoleId) -> bool {
        if role == 0 {
            return false;
        }
        let mut changed = false;
        if !self.deleted_roles.contains(&role) {
            self.deleted_roles.push(role);
            self.deleted_roles.sort_unstable();
            changed = true;
        }
        let before_saved = self.saved_roles.len();
        self.saved_roles.retain(|&r| r != role);
        if self.saved_roles.len() != before_saved {
            changed = true;
        }
        self.deferred_roles.retain(|&r| r != role);
        if self.saved_roles.is_empty() {
            self.roles_removed = true;
        }
        changed
    }

    fn apply_deferred_grant(&mut self, role: RoleId, ctx: &Context) -> bool {
        if !ctx.defer_grants || role == 0 || role == ctx.safeword_role {
            return false;
        }
        let mut changed = false;
        if !self.saved_roles.contains(&role) {
            self.saved_roles.push(role);
            self.saved_roles.sort_unstable();
            changed = true;
            if !self.deferred_roles.contains(&role) {
                self.deferred_roles.push(role);
                self.deferred_roles.sort_unstable();
            }
        }
        if !self.original_roles.contains(&role) {
            self.original_roles.push(role);
            self.original_roles.sort_unstable();
            changed = true;
        }
        let before_struck = self.struck_roles.len();
        self.struck_roles.retain(|&r| r != role);
        if self.struck_roles.len() != before_struck {
            changed = true;
        }
        changed
    }

    fn is_activation_step_done(&self, id: ActionId) -> Option<bool> {
        match id {
            ActionId::SaveSnapshot => Some(self.snapshot_saved),
            ActionId::RemoveRoles => Some(self.roles_removed),
            ActionId::AddSafewordRole => Some(self.safeword_added),
            ActionId::SendActivatedDm => Some(self.dm_sent),
            _ => None,
        }
    }

    fn mark_activation_step_done(&mut self, id: ActionId) -> bool {
        match id {
            ActionId::SaveSnapshot => self.snapshot_saved = true,
            ActionId::RemoveRoles => self.roles_removed = true,
            ActionId::AddSafewordRole => self.safeword_added = true,
            ActionId::SendActivatedDm => self.dm_sent = true,
            _ => return false,
        }
        if self.last_failure.map(|(f, _)| f) == Some(id) {
            self.last_failure = None;
        }
        true
    }

    fn is_release_step_done(&self, id: ActionId) -> Option<bool> {
        match id {
            ActionId::RemoveSafewordRole => Some(self.safeword_removed),
            ActionId::RestoreRoles => Some(self.roles_restored),
            ActionId::ClearSnapshot => Some(self.snapshot_cleared),
            ActionId::SendReleasedDm => Some(self.release_dm_sent),
            _ => None,
        }
    }

    fn mark_release_step_done(&mut self, id: ActionId) -> bool {
        match id {
            ActionId::RemoveSafewordRole => self.safeword_removed = true,
            ActionId::RestoreRoles => self.roles_restored = true,
            ActionId::ClearSnapshot => self.snapshot_cleared = true,
            ActionId::SendReleasedDm => self.release_dm_sent = true,
            _ => return false,
        }
        if self.last_failure.map(|(f, _)| f) == Some(id) {
            self.last_failure = None;
        }
        true
    }
}

/// True only when a reaction event came from a non-bot user on the configured message and emoji.
pub const fn is_actionable(
    author_is_bot: bool,
    message_id_matches: bool,
    emoji_matches: bool,
) -> bool {
    !author_is_bot && message_id_matches && emoji_matches
}

/// Advances the pure safeword state machine by one event.
pub fn step(state: &State, event: Event, ctx: &Context) -> (State, Vec<Action>) {
    match state {
        State::Inactive => step_inactive(event, ctx),
        State::Activating(snapshot) => step_activating(snapshot.clone(), event, ctx),
        State::Active(snapshot) => step_active(snapshot.clone(), event, ctx),
        State::Releasing(snapshot) => step_releasing(snapshot.clone(), event, ctx),
    }
}

fn step_inactive(event: Event, ctx: &Context) -> (State, Vec<Action>) {
    match event {
        Event::ReactionAdded => {
            let snapshot = Snapshot::new_from_context(ctx);
            let actions = snapshot.pending_activation_actions();
            (State::Activating(snapshot), actions)
        }
        Event::ReactionRemoved
        | Event::Strike(_)
        | Event::RoleDeleted(_)
        | Event::GrantDuringActive(_)
        | Event::ActionDone(_)
        | Event::ActionFailed(_, _) => (State::Inactive, Vec::new()),
    }
}

fn step_activating(mut snapshot: Snapshot, event: Event, ctx: &Context) -> (State, Vec<Action>) {
    match event {
        Event::ReactionAdded => {
            // Replayed add never overwrites the snapshot.
            // If a prior action failed, retry only unacknowledged actions.
            if snapshot.last_failure.is_some() {
                snapshot.last_failure = None;
                let actions = snapshot.pending_activation_actions();
                (State::Activating(snapshot), actions)
            } else {
                snapshot.pending_release = false;
                (State::Activating(snapshot), Vec::new())
            }
        }
        Event::ReactionRemoved => {
            snapshot.pending_release = true;
            if snapshot.activation_complete() {
                snapshot.enter_releasing(ctx)
            } else if snapshot.last_failure.is_some() {
                snapshot.last_failure = None;
                let actions = snapshot.pending_activation_actions();
                (State::Activating(snapshot), actions)
            } else {
                (State::Activating(snapshot), Vec::new())
            }
        }
        Event::Strike(role) => {
            snapshot.apply_strike(role);
            (State::Activating(snapshot), Vec::new())
        }
        Event::RoleDeleted(role) => {
            snapshot.apply_role_deleted(role);
            (State::Activating(snapshot), Vec::new())
        }
        Event::GrantDuringActive(role) => {
            snapshot.apply_deferred_grant(role, ctx);
            (State::Activating(snapshot), Vec::new())
        }
        Event::ActionDone(id) => {
            if snapshot.mark_activation_step_done(id) && snapshot.activation_complete() {
                if snapshot.pending_release {
                    snapshot.enter_releasing(ctx)
                } else {
                    (State::Active(snapshot), Vec::new())
                }
            } else {
                (State::Activating(snapshot), Vec::new())
            }
        }
        Event::ActionFailed(id, code) => {
            if snapshot.is_activation_step_done(id) == Some(false) {
                snapshot.last_failure = Some((id, code));
            }
            (State::Activating(snapshot), Vec::new())
        }
    }
}

fn step_active(mut snapshot: Snapshot, event: Event, ctx: &Context) -> (State, Vec<Action>) {
    match event {
        Event::ReactionAdded => {
            // Replayed add while Active never overwrites the snapshot.
            if snapshot.last_failure.map(|(id, _)| id) == Some(ActionId::SaveSnapshot) {
                snapshot.last_failure = None;
                (State::Active(snapshot), vec![Action::SaveSnapshot])
            } else {
                (State::Active(snapshot), Vec::new())
            }
        }
        Event::ReactionRemoved => snapshot.enter_releasing(ctx),
        Event::Strike(role) => {
            let changed = snapshot.apply_strike(role);
            let actions = if changed {
                vec![Action::SaveSnapshot]
            } else {
                Vec::new()
            };
            (State::Active(snapshot), actions)
        }
        Event::RoleDeleted(role) => {
            let changed = snapshot.apply_role_deleted(role);
            let actions = if changed {
                vec![Action::SaveSnapshot]
            } else {
                Vec::new()
            };
            (State::Active(snapshot), actions)
        }
        Event::GrantDuringActive(role) => {
            let changed = snapshot.apply_deferred_grant(role, ctx);
            let actions = if changed {
                vec![Action::SaveSnapshot]
            } else {
                Vec::new()
            };
            (State::Active(snapshot), actions)
        }
        Event::ActionDone(id) => {
            if id == ActionId::SaveSnapshot
                && snapshot.last_failure.map(|(f, _)| f) == Some(ActionId::SaveSnapshot)
            {
                snapshot.last_failure = None;
            }
            (State::Active(snapshot), Vec::new())
        }
        Event::ActionFailed(id, code) => {
            if id == ActionId::SaveSnapshot || snapshot.is_activation_step_done(id) == Some(false) {
                snapshot.last_failure = Some((id, code));
            }
            (State::Active(snapshot), Vec::new())
        }
    }
}

fn step_releasing(mut snapshot: Snapshot, event: Event, ctx: &Context) -> (State, Vec<Action>) {
    match event {
        Event::ReactionRemoved => {
            snapshot.pending_reactivate = false;
            if snapshot.last_failure.is_some() {
                snapshot.last_failure = None;
                let actions = snapshot.pending_release_actions();
                (State::Releasing(snapshot), actions)
            } else {
                (State::Releasing(snapshot), Vec::new())
            }
        }
        Event::ReactionAdded => {
            snapshot.pending_reactivate = true;
            if snapshot.release_complete() {
                let fresh = Snapshot::new_from_context(ctx);
                let actions = fresh.pending_activation_actions();
                (State::Activating(fresh), actions)
            } else if snapshot.last_failure.is_some() {
                snapshot.last_failure = None;
                let actions = snapshot.pending_release_actions();
                (State::Releasing(snapshot), actions)
            } else {
                (State::Releasing(snapshot), Vec::new())
            }
        }
        Event::Strike(role) => {
            snapshot.apply_strike(role);
            if !snapshot.roles_restored && ctx.filter_restore {
                snapshot.restore_plan.retain(|&r| r != role);
                if snapshot.restore_plan.is_empty() {
                    snapshot.roles_restored = true;
                }
            }
            (State::Releasing(snapshot), Vec::new())
        }
        Event::RoleDeleted(role) => {
            snapshot.apply_role_deleted(role);
            if !snapshot.roles_restored && ctx.filter_restore {
                snapshot.restore_plan.retain(|&r| r != role);
                if snapshot.restore_plan.is_empty() {
                    snapshot.roles_restored = true;
                }
            }
            (State::Releasing(snapshot), Vec::new())
        }
        Event::GrantDuringActive(role) => {
            if (!snapshot.roles_restored
                || (!snapshot.safeword_removed && snapshot.restore_plan.is_empty()))
                && ctx.restricted_roles.contains(&role)
            {
                let changed = snapshot.apply_deferred_grant(role, ctx);
                if changed {
                    snapshot.restore_plan = snapshot.compute_restore_roles(ctx);
                    snapshot.roles_restored = snapshot.restore_plan.is_empty();
                }
            }
            (State::Releasing(snapshot), Vec::new())
        }
        Event::ActionDone(id) => {
            if snapshot.mark_release_step_done(id) && snapshot.release_complete() {
                if snapshot.pending_reactivate {
                    let fresh = Snapshot::new_from_context(ctx);
                    let actions = fresh.pending_activation_actions();
                    (State::Activating(fresh), actions)
                } else {
                    (State::Inactive, Vec::new())
                }
            } else {
                (State::Releasing(snapshot), Vec::new())
            }
        }
        Event::ActionFailed(id, code) => {
            if snapshot.is_release_step_done(id) == Some(false) {
                snapshot.last_failure = Some((id, code));
            }
            (State::Releasing(snapshot), Vec::new())
        }
    }
}

/// Computes the single deterministic next action on startup from stored state and live reaction presence.
pub fn restart_action(state: &State, reaction_present_now: bool) -> RestartAction {
    match state {
        State::Inactive => {
            if reaction_present_now {
                RestartAction::Trigger(Event::ReactionAdded)
            } else {
                RestartAction::NoOp
            }
        }
        State::Activating(snapshot) => {
            if !reaction_present_now && !snapshot.pending_release {
                return RestartAction::Trigger(Event::ReactionRemoved);
            }
            if let Some(next) = snapshot.pending_activation_actions().into_iter().next() {
                RestartAction::Execute(next)
            } else if reaction_present_now && !snapshot.pending_release {
                RestartAction::NoOp
            } else {
                RestartAction::Trigger(Event::ReactionRemoved)
            }
        }
        State::Active(snapshot) => {
            if !reaction_present_now {
                RestartAction::Trigger(Event::ReactionRemoved)
            } else if snapshot.last_failure.map(|(id, _)| id) == Some(ActionId::SaveSnapshot) {
                RestartAction::Execute(Action::SaveSnapshot)
            } else {
                RestartAction::NoOp
            }
        }
        State::Releasing(snapshot) => {
            if let Some(next) = snapshot.pending_release_actions().into_iter().next() {
                RestartAction::Execute(next)
            } else if reaction_present_now {
                RestartAction::Trigger(Event::ReactionAdded)
            } else {
                RestartAction::NoOp
            }
        }
    }
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

    fn acknowledge_all(
        mut state: State,
        actions: &[Action],
        ctx: &Context,
    ) -> (State, Vec<Action>) {
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

        let snap = state
            .snapshot()
            .expect("must enter Activating with snapshot");
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
        let (activating, _) = step(&activating, Event::ActionDone(ActionId::SaveSnapshot), &ctx);
        let (activating_with_release, immediate) = step(&activating, Event::ReactionRemoved, &ctx);
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

        let (releasing, release_actions) = step(&active, Event::ReactionRemoved, &release_ctx);
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
                Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER, ROLE_MEDIA, ROLE_FEMDOM,]),
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ]
        );
    }

    #[test]
    fn empty_snapshot_still_toggles_safeword_role_and_sends_both_dms() {
        let ctx = sample_ctx(vec![ROLE_UNRESTRICTED]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        assert_eq!(
            activating.snapshot().unwrap().saved_roles,
            Vec::<RoleId>::new()
        );
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
        assert!(release_actions.contains(&Action::RestoreRoles(vec![ROLE_MEMBER, ROLE_DEFERRED])));

        // Deferral disabled: GrantDuringActive is ignored.
        ctx.defer_grants = false;
        let (active_ignored, ignore_actions) =
            step(&active, Event::GrantDuringActive(ROLE_DEFERRED), &ctx);
        assert!(ignore_actions.is_empty());
        assert_eq!(
            active_ignored.snapshot().unwrap().saved_roles,
            vec![ROLE_MEMBER]
        );
    }

    #[test]
    fn restart_in_every_state_gives_one_deterministic_next_action() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating.clone(), &act_actions, &ctx);
        let (releasing, _) = step(&active, Event::ReactionRemoved, &ctx);

        for state in [&State::Inactive, &activating, &active, &releasing] {
            for present in [true, false] {
                let first = restart_action(state, present);
                let second = restart_action(state, present);
                assert_eq!(first, second);
            }
        }

        // Inactive
        assert_eq!(
            restart_action(&State::Inactive, true),
            RestartAction::Trigger(Event::ReactionAdded)
        );
        assert_eq!(restart_action(&State::Inactive, false), RestartAction::NoOp);

        // Activating (walks through each unacknowledged step in order)
        assert_eq!(
            restart_action(&activating, true),
            RestartAction::Execute(Action::SaveSnapshot)
        );
        assert_eq!(
            restart_action(&activating, false),
            RestartAction::Trigger(Event::ReactionRemoved)
        );
        let (act_after_save, _) =
            step(&activating, Event::ActionDone(ActionId::SaveSnapshot), &ctx);
        assert_eq!(
            restart_action(&act_after_save, true),
            RestartAction::Execute(Action::RemoveRoles(vec![ROLE_VERIFIED]))
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
        let (activating, _) = step(&activating, Event::ActionDone(ActionId::SaveSnapshot), &ctx);
        let (activating, _) = step(&activating, Event::ActionDone(ActionId::RemoveRoles), &ctx);
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

        // Stale failure on an already-acknowledged action does not un-acknowledge it.
        let (failed_state, _) = step(
            &failed_state,
            Event::ActionFailed(ActionId::SaveSnapshot, FailureCode::StorageError),
            &ctx,
        );
        assert_eq!(
            failed_state.snapshot().unwrap().last_failure,
            Some((ActionId::AddSafewordRole, FailureCode::TransientHttp))
        );

        // Retry via ReactionAdded: only unacknowledged actions are emitted.
        let (retried_state, retry_actions) = step(&failed_state, Event::ReactionAdded, &ctx);
        assert_eq!(
            retry_actions,
            vec![Action::AddSafewordRole, Action::SendDm(DmKind::Activated)]
        );
        assert!(retried_state.snapshot().unwrap().last_failure.is_none());

        let (active, _) = acknowledge_all(retried_state, &retry_actions, &ctx);
        assert!(matches!(active, State::Active(_)));

        // Also verify failure and retry during Releasing.
        let (releasing, _) = step(&active, Event::ReactionRemoved, &ctx);
        let (releasing, _) = step(
            &releasing,
            Event::ActionDone(ActionId::RemoveSafewordRole),
            &ctx,
        );
        let (releasing_failed, _) = step(
            &releasing,
            Event::ActionFailed(ActionId::RestoreRoles, FailureCode::RateLimited),
            &ctx,
        );
        let (releasing_retried, rel_retry_actions) =
            step(&releasing_failed, Event::ReactionRemoved, &ctx);
        assert_eq!(
            rel_retry_actions,
            vec![
                Action::RestoreRoles(vec![ROLE_VERIFIED]),
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ]
        );
        let (inactive, _) = acknowledge_all(releasing_retried, &rel_retry_actions, &ctx);
        assert_eq!(inactive, State::Inactive);
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
    fn boundary_and_adversarial_transitions_preserve_consistency() {
        // 1. Zero role IDs and safeword role in GrantDuringActive/Strike/RoleDeleted are ignored.
        let ctx = sample_ctx(vec![0, SAFEWORD_ROLE, ROLE_VERIFIED]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        assert_eq!(
            activating.snapshot().unwrap().saved_roles,
            vec![ROLE_VERIFIED]
        );
        let (active, _) = acknowledge_all(activating, &act_actions, &ctx);

        let (active, no_actions_zero) = step(&active, Event::GrantDuringActive(0), &ctx);
        assert!(no_actions_zero.is_empty());
        let (active, no_actions_sw) = step(&active, Event::GrantDuringActive(SAFEWORD_ROLE), &ctx);
        assert!(no_actions_sw.is_empty());
        let (active, no_actions_dup) = step(&active, Event::GrantDuringActive(ROLE_VERIFIED), &ctx);
        assert!(no_actions_dup.is_empty());

        // 2. If all saved roles are struck/deleted/protected, RestoreRoles is omitted and release still completes.
        let (active_all_struck, strike_actions) = step(&active, Event::Strike(ROLE_VERIFIED), &ctx);
        assert_eq!(strike_actions, vec![Action::SaveSnapshot]);
        let (releasing, rel_actions) = step(&active_all_struck, Event::ReactionRemoved, &ctx);
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
        total_adds += a1
            .iter()
            .filter(|a| matches!(a, Action::AddSafewordRole))
            .count();
        let (s2, a2) = step(&s1, Event::ReactionAdded, &ctx);
        total_adds += a2
            .iter()
            .filter(|a| matches!(a, Action::AddSafewordRole))
            .count();
        let (s3, a3) = acknowledge_all(s2, &a1, &ctx);
        total_adds += a3
            .iter()
            .filter(|a| matches!(a, Action::AddSafewordRole))
            .count();
        let (_s4, a4) = step(&s3, Event::ReactionAdded, &ctx);
        total_adds += a4
            .iter()
            .filter(|a| matches!(a, Action::AddSafewordRole))
            .count();
        assert_eq!(total_adds, 1);
    }

    #[test]
    fn reaction_added_during_releasing_reactivates_on_completion_and_respects_last_event_wins() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED, ROLE_MEMBER]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating, &act_actions, &ctx);

        // 1. remove -> add -> complete -> Activating/Active again with the safeword role re-added
        let (releasing, rel_actions) = step(&active, Event::ReactionRemoved, &ctx);
        let (releasing_flagged, add_actions) = step(&releasing, Event::ReactionAdded, &ctx);
        assert!(add_actions.is_empty());
        assert!(releasing_flagged.snapshot().unwrap().pending_reactivate);

        // Acknowledge all release steps except the last, then acknowledge the last with fresh ctx.held_roles
        let (releasing_mid, mid_actions) = acknowledge_all(
            releasing_flagged,
            &rel_actions[..rel_actions.len() - 1],
            &ctx,
        );
        assert!(mid_actions.is_empty());
        let reactivate_ctx = sample_ctx(vec![ROLE_VERIFIED, ROLE_FEMDOM]);
        let (reactivating, react_actions) = step(
            &releasing_mid,
            Event::ActionDone(rel_actions.last().unwrap().id()),
            &reactivate_ctx,
        );
        assert!(matches!(reactivating, State::Activating(_)));
        assert_eq!(
            reactivating.snapshot().unwrap().saved_roles,
            vec![ROLE_VERIFIED, ROLE_FEMDOM]
        );
        assert!(!reactivating.snapshot().unwrap().pending_reactivate);
        assert!(react_actions.contains(&Action::AddSafewordRole));
        let (re_active, _) = acknowledge_all(reactivating, &react_actions, &reactivate_ctx);
        assert!(matches!(re_active, State::Active(_)));

        // 2. remove -> add -> remove -> complete -> stays released (last event wins)
        let (rel2, rel2_actions) = step(&re_active, Event::ReactionRemoved, &reactivate_ctx);
        let (rel2_added, _) = step(&rel2, Event::ReactionAdded, &reactivate_ctx);
        assert!(rel2_added.snapshot().unwrap().pending_reactivate);
        let (rel2_removed, _) = step(&rel2_added, Event::ReactionRemoved, &reactivate_ctx);
        assert!(!rel2_removed.snapshot().unwrap().pending_reactivate);
        let (final_inactive, trailing) =
            acknowledge_all(rel2_removed, &rel2_actions, &reactivate_ctx);
        assert_eq!(final_inactive, State::Inactive);
        assert!(trailing.is_empty());

        // 3. pending_reactivate survives an action failure and retry
        let (rel3, _) = step(&active, Event::ReactionRemoved, &ctx);
        let (rel3, _) = step(&rel3, Event::ReactionAdded, &ctx);
        assert!(rel3.snapshot().unwrap().pending_reactivate);
        let (rel3, _) = step(&rel3, Event::ActionDone(ActionId::RemoveSafewordRole), &ctx);
        let (rel3_failed, _) = step(
            &rel3,
            Event::ActionFailed(ActionId::RestoreRoles, FailureCode::TransientHttp),
            &ctx,
        );
        assert!(rel3_failed.snapshot().unwrap().pending_reactivate);
        let (rel3_retried, retry_actions) = step(&rel3_failed, Event::ReactionAdded, &ctx);
        assert!(rel3_retried.snapshot().unwrap().pending_reactivate);
        assert_eq!(
            retry_actions,
            vec![
                Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER]),
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ]
        );
        let (reactivated_after_retry, react_after_retry_actions) =
            acknowledge_all(rel3_retried, &retry_actions, &ctx);
        assert!(matches!(reactivated_after_retry, State::Activating(_)));
        assert!(react_after_retry_actions.contains(&Action::AddSafewordRole));
    }

    #[test]
    fn remove_then_readd_during_activating_cancels_pending_release_and_ends_active() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED, ROLE_MEMBER]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (activating, _) = step(&activating, Event::ActionDone(ActionId::SaveSnapshot), &ctx);

        // ReactionRemoved sets pending_release = true.
        let (activating_removed, _) = step(&activating, Event::ReactionRemoved, &ctx);
        assert!(activating_removed.snapshot().unwrap().pending_release);

        // Re-adding the reaction clears pending_release (last reaction event wins).
        let (activating_readded, readd_actions) =
            step(&activating_removed, Event::ReactionAdded, &ctx);
        assert!(readd_actions.is_empty());
        assert!(!activating_readded.snapshot().unwrap().pending_release);

        // Completing activation transitions to Active with no release actions.
        let (final_state, follow_up) = acknowledge_all(activating_readded, &act_actions[1..], &ctx);
        assert!(matches!(final_state, State::Active(_)));
        assert!(follow_up.is_empty());
    }

    #[test]
    fn grant_during_releasing_defers_before_restore_ack_and_ignores_after_or_when_disabled() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating, &act_actions, &ctx);
        let (releasing, initial_rel_actions) = step(&active, Event::ReactionRemoved, &ctx);
        assert_eq!(
            initial_rel_actions,
            vec![
                Action::RemoveSafewordRole,
                Action::RestoreRoles(vec![ROLE_VERIFIED]),
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ]
        );

        // (a) Grant while RestoreRoles is in flight -> emits second RestoreRoles with late role,
        // and release does not complete after only the first RestoreRoles ack.
        let (rel_in_flight, _) = step(
            &releasing,
            Event::ActionDone(ActionId::RemoveSafewordRole),
            &ctx,
        );
        let (rel_in_flight_granted, in_flight_actions) =
            step(&rel_in_flight, Event::GrantDuringActive(ROLE_MEMBER), &ctx);
        assert_eq!(
            in_flight_actions,
            vec![Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER])]
        );
        let (rel_after_other_acks, _) = acknowledge_all(
            rel_in_flight_granted,
            &[Action::ClearSnapshot, Action::SendDm(DmKind::Released)],
            &ctx,
        );
        let (still_releasing_after_first_ack, first_ack_actions) = step(
            &rel_after_other_acks,
            Event::ActionDone(ActionId::RestoreRoles),
            &ctx,
        );
        assert!(first_ack_actions.is_empty());
        assert!(matches!(
            still_releasing_after_first_ack,
            State::Releasing(_)
        ));
        let (completed_after_second_ack, second_ack_actions) = step(
            &still_releasing_after_first_ack,
            Event::ActionDone(ActionId::RestoreRoles),
            &ctx,
        );
        assert!(second_ack_actions.is_empty());
        assert_eq!(completed_after_second_ack, State::Inactive);

        // (b) Grant before any release step is acked -> emitted RestoreRoles contains the late role.
        let (rel_before, actions_before) =
            step(&releasing, Event::GrantDuringActive(ROLE_MEMBER), &ctx);
        assert_eq!(
            actions_before,
            vec![Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER])]
        );
        // Also when initial restore_plan was empty, the first/only emitted RestoreRoles contains the late role.
        let empty_ctx = sample_ctx(vec![]);
        let (act_empty, act_empty_actions) =
            step(&State::Inactive, Event::ReactionAdded, &empty_ctx);
        let (active_empty, _) = acknowledge_all(act_empty, &act_empty_actions, &empty_ctx);
        let (rel_empty, rel_empty_actions) =
            step(&active_empty, Event::ReactionRemoved, &empty_ctx);
        assert!(!rel_empty_actions
            .iter()
            .any(|a| matches!(a, Action::RestoreRoles(_))));
        let (rel_empty_granted, empty_grant_actions) = step(
            &rel_empty,
            Event::GrantDuringActive(ROLE_VERIFIED),
            &empty_ctx,
        );
        assert_eq!(
            empty_grant_actions,
            vec![Action::RestoreRoles(vec![ROLE_VERIFIED])]
        );
        let (rel_empty_mid, _) = acknowledge_all(rel_empty_granted, &rel_empty_actions, &empty_ctx);
        assert!(matches!(rel_empty_mid, State::Releasing(_)));
        let (rel_empty_done, _) = step(
            &rel_empty_mid,
            Event::ActionDone(ActionId::RestoreRoles),
            &empty_ctx,
        );
        assert_eq!(rel_empty_done, State::Inactive);

        // Subject to struck/deleted/protected filters and restricted_roles check:
        let mut protected_ctx = ctx.clone();
        protected_ctx.protected_roles = vec![ROLE_FEMDOM];
        let (_rel_prot, prot_actions) = step(
            &rel_before,
            Event::GrantDuringActive(ROLE_FEMDOM),
            &protected_ctx,
        );
        assert!(prot_actions.is_empty());
        let (_rel_unrestricted, unres_actions) = step(
            &rel_before,
            Event::GrantDuringActive(ROLE_UNRESTRICTED),
            &ctx,
        );
        assert!(unres_actions.is_empty());

        // (e) Post-ack grant is still ignored after all outstanding RestoreRoles are acknowledged.
        let (rel_one_ack, _) = step(&rel_before, Event::ActionDone(ActionId::RestoreRoles), &ctx);
        let (rel_all_restore_acked, _) = step(
            &rel_one_ack,
            Event::ActionDone(ActionId::RestoreRoles),
            &ctx,
        );
        let (rel_after_grant, post_ack_actions) = step(
            &rel_all_restore_acked,
            Event::GrantDuringActive(ROLE_MEDIA),
            &ctx,
        );
        assert!(post_ack_actions.is_empty());
        assert_eq!(
            rel_after_grant.snapshot().unwrap().restore_plan,
            vec![ROLE_VERIFIED, ROLE_MEMBER]
        );

        // (f) When defer_grants = false: GrantDuringActive during Releasing is unchanged (emits nothing).
        let mut no_defer_ctx = ctx.clone();
        no_defer_ctx.defer_grants = false;
        let (rel_no_defer, no_defer_actions) = step(
            &releasing,
            Event::GrantDuringActive(ROLE_MEMBER),
            &no_defer_ctx,
        );
        assert!(no_defer_actions.is_empty());
        let (inactive_no_defer, _) =
            acknowledge_all(rel_no_defer, &initial_rel_actions, &no_defer_ctx);
        assert_eq!(inactive_no_defer, State::Inactive);
    }

    #[test]
    fn grant_during_releasing_two_late_grants_and_failure_retry_uses_grown_plan() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating, &act_actions, &ctx);

        // (c) Two late grants while RestoreRoles is unacknowledged emit growing role lists
        // and require all three RestoreRoles emissions to be acknowledged before completion.
        let (releasing, _) = step(&active, Event::ReactionRemoved, &ctx);
        let (releasing, _) = step(
            &releasing,
            Event::ActionDone(ActionId::RemoveSafewordRole),
            &ctx,
        );
        let (releasing, grant1_actions) =
            step(&releasing, Event::GrantDuringActive(ROLE_MEMBER), &ctx);
        assert_eq!(
            grant1_actions,
            vec![Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER])]
        );
        let (releasing, grant2_actions) =
            step(&releasing, Event::GrantDuringActive(ROLE_MEDIA), &ctx);
        assert_eq!(
            grant2_actions,
            vec![Action::RestoreRoles(vec![
                ROLE_VERIFIED,
                ROLE_MEMBER,
                ROLE_MEDIA
            ])]
        );
        let (releasing, _) = acknowledge_all(
            releasing,
            &[Action::ClearSnapshot, Action::SendDm(DmKind::Released)],
            &ctx,
        );
        let (releasing, _) = step(&releasing, Event::ActionDone(ActionId::RestoreRoles), &ctx);
        assert!(matches!(releasing, State::Releasing(_)));
        let (releasing, _) = step(&releasing, Event::ActionDone(ActionId::RestoreRoles), &ctx);
        assert!(matches!(releasing, State::Releasing(_)));
        let (done_three_acks, _) =
            step(&releasing, Event::ActionDone(ActionId::RestoreRoles), &ctx);
        assert_eq!(done_three_acks, State::Inactive);

        // (d) Failure of the first RestoreRoles then retry uses the grown plan (never the older list).
        let (rel_d, _) = step(&active, Event::ReactionRemoved, &ctx);
        let (rel_d, _) = step(
            &rel_d,
            Event::ActionDone(ActionId::RemoveSafewordRole),
            &ctx,
        );
        let (rel_d, late_grant_actions) = step(&rel_d, Event::GrantDuringActive(ROLE_MEMBER), &ctx);
        assert_eq!(
            late_grant_actions,
            vec![Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER])]
        );
        let (rel_d_failed, _) = step(
            &rel_d,
            Event::ActionFailed(ActionId::RestoreRoles, FailureCode::TransientHttp),
            &ctx,
        );
        assert_eq!(
            restart_action(&rel_d_failed, false),
            RestartAction::Execute(Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER]))
        );
        let (rel_d_retried, retry_actions) = step(&rel_d_failed, Event::ReactionRemoved, &ctx);
        assert_eq!(
            retry_actions,
            vec![
                Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER]),
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ]
        );
        let (rel_d_done, _) = acknowledge_all(rel_d_retried, &retry_actions, &ctx);
        assert_eq!(rel_d_done, State::Inactive);
    }

    #[test]
    fn grant_during_releasing_with_pending_reactivate_starts_only_after_all_restore_acks() {
        // (g) Combined with pending_reactivate: re-activation starts only after all restore acks.
        let ctx = sample_ctx(vec![ROLE_VERIFIED]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating, &act_actions, &ctx);
        let (releasing, _) = step(&active, Event::ReactionRemoved, &ctx);
        let (releasing, _) = step(&releasing, Event::ReactionAdded, &ctx);
        let (releasing, late_actions) =
            step(&releasing, Event::GrantDuringActive(ROLE_MEMBER), &ctx);
        assert_eq!(
            late_actions,
            vec![Action::RestoreRoles(vec![ROLE_VERIFIED, ROLE_MEMBER])]
        );
        let (releasing, _) = acknowledge_all(
            releasing,
            &[
                Action::RemoveSafewordRole,
                Action::ClearSnapshot,
                Action::SendDm(DmKind::Released),
            ],
            &ctx,
        );

        // First RestoreRoles ack must NOT trigger re-activation yet.
        let reactivate_ctx = sample_ctx(vec![ROLE_VERIFIED, ROLE_MEMBER]);
        let (still_releasing, first_ack_actions) = step(
            &releasing,
            Event::ActionDone(ActionId::RestoreRoles),
            &reactivate_ctx,
        );
        assert!(first_ack_actions.is_empty());
        assert!(matches!(still_releasing, State::Releasing(_)));

        // Second RestoreRoles ack completes the release and starts the fresh activation.
        let (reactivating, react_actions) = step(
            &still_releasing,
            Event::ActionDone(ActionId::RestoreRoles),
            &reactivate_ctx,
        );
        assert!(matches!(reactivating, State::Activating(_)));
        assert_eq!(
            react_actions,
            vec![
                Action::SaveSnapshot,
                Action::RemoveRoles(vec![ROLE_VERIFIED, ROLE_MEMBER]),
                Action::AddSafewordRole,
                Action::SendDm(DmKind::Activated),
            ]
        );
    }

    #[test]
    fn action_failed_in_active_records_last_failure_for_any_unacknowledged_step() {
        let ctx = sample_ctx(vec![ROLE_VERIFIED]);
        let (activating, act_actions) = step(&State::Inactive, Event::ReactionAdded, &ctx);
        let (active, _) = acknowledge_all(activating, &act_actions, &ctx);

        // Only SaveSnapshot can be pending in a reachable Active state (after Strike/RoleDeleted/GrantDuringActive).
        let (active_save_failed, _) = step(
            &active,
            Event::ActionFailed(ActionId::SaveSnapshot, FailureCode::StorageError),
            &ctx,
        );
        assert_eq!(
            active_save_failed.snapshot().unwrap().last_failure,
            Some((ActionId::SaveSnapshot, FailureCode::StorageError))
        );
        let (active_save_cleared, _) = step(
            &active_save_failed,
            Event::ActionDone(ActionId::SaveSnapshot),
            &ctx,
        );
        assert_eq!(active_save_cleared.snapshot().unwrap().last_failure, None);

        // All initial activation steps are already acknowledged before reaching Active, so failures
        // for them (or release-only steps) are ignored rather than leaving an unclearable flag.
        for id in [
            ActionId::RemoveRoles,
            ActionId::AddSafewordRole,
            ActionId::SendActivatedDm,
            ActionId::RemoveSafewordRole,
            ActionId::RestoreRoles,
            ActionId::ClearSnapshot,
            ActionId::SendReleasedDm,
        ] {
            let (still_active, _) = step(
                &active,
                Event::ActionFailed(id, FailureCode::TransientHttp),
                &ctx,
            );
            assert_eq!(still_active.snapshot().unwrap().last_failure, None);
        }
    }
}

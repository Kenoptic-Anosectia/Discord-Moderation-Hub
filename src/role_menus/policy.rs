//! Role menu selection policy: what a click on a menu option does.
//!
//! Pure and deterministic: no Discord types, I/O, clock or randomness.
//! Every role in `Plan::add` passed `roles::eligibility::decide` for
//! `GrantPath::Menu`; removals are checked with `decide_removal`.
//!
//! Group rules (see `docs/verification/ROLE-MENUS.md`):
//! - Exclusive: the member ends with only the clicked option; the other options
//!   of the group are removed. Clicking the held option is a no-op reported as
//!   `AlreadySelected` (clearing is not offered). If a removal is denied the
//!   whole swap is refused so the member keeps their current choice.
//! - Multi: options accumulate; clicking a held option removes it; `max`
//!   refuses an add when the member already holds `max` options of the group.

use crate::roles::eligibility::{
    decide, decide_removal, Decision, DenyReason, GrantPath, GrantRequest, RoleFacts, SafewordState,
};
use std::collections::BTreeSet;

/// Discord's limit of options in one select component.
pub const SELECT_OPTION_LIMIT: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupMode {
    Exclusive,
    Multi,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuOption {
    pub role_id: u64,
    pub label_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub mode: GroupMode,
    /// Only meaningful for `Multi`; ignored for `Exclusive`.
    pub max: Option<u32>,
    pub options: Vec<MenuOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub key: String,
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Click {
    pub member_roles: Vec<u64>,
    pub group_id: String,
    pub option_role_id: u64,
}

/// K03 inputs supplied by the caller for every role the menu may touch.
#[derive(Debug, Clone, Copy)]
pub struct GrantContext<'a> {
    pub guild_id: u64,
    pub bot_top_position: u16,
    /// Facts per role id. A role with no entry is treated as deleted.
    pub role_facts: &'a [RoleFacts],
    pub protected_role_ids: &'a [u64],
    pub safeword: SafewordState,
    pub safeword_restricted_role_ids: &'a [u64],
}

#[derive(Debug, Clone, Copy)]
pub struct PlanRequest<'a> {
    pub menu: &'a Menu,
    pub click: &'a Click,
    pub grant: GrantContext<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    UnknownGroup,
}

impl PlanError {
    pub fn code(self) -> &'static str {
        match self {
            PlanError::UnknownGroup => "unknown_group",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// K03 `decide` denied an add (carries the K03 reason).
    Denied(DenyReason),
    /// A removal could not be done (K03 `decide_removal`, or the role is gone).
    RemovalDenied(DenyReason),
    /// The clicked role is not an option of the clicked group.
    UnknownOption,
    /// Multi group already holds `max` options.
    MaxReached,
    /// Exclusive group: the clicked option is already the member's choice.
    AlreadySelected,
    /// Exclusive swap refused because a removal it needs was denied.
    SwapBlocked,
}

impl Reason {
    /// Stable machine code; never contains names or ids.
    pub fn code(self) -> &'static str {
        match self {
            Reason::Denied(r) => r.code(),
            Reason::RemovalDenied(DenyReason::AtOrAboveBot) => "removal_at_or_above_bot",
            Reason::RemovalDenied(DenyReason::RoleDeleted) => "removal_role_deleted",
            Reason::RemovalDenied(_) => "removal_denied",
            Reason::UnknownOption => "unknown_option",
            Reason::MaxReached => "max_reached",
            Reason::AlreadySelected => "already_selected",
            Reason::SwapBlocked => "swap_blocked",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub add: Vec<u64>,
    pub remove: Vec<u64>,
    pub skipped: Vec<(u64, Reason)>,
}

/// One select component: a slice of one group's options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub group_id: String,
    /// 0-based chunk number within the group.
    pub part: usize,
    pub options: Vec<MenuOption>,
}

/// Options of a group with repeated roles dropped (first occurrence wins).
fn distinct_options(group: &Group) -> Vec<&MenuOption> {
    let mut seen = BTreeSet::new();
    group
        .options
        .iter()
        .filter(|o| seen.insert(o.role_id))
        .collect()
}

/// Split every group into select components of at most `SELECT_OPTION_LIMIT`
/// options, in declared order, never mixing groups. Repeated roles inside a
/// group are dropped (Discord select values must be unique). Groups with no
/// options produce no component. This does not check how many components fit
/// in one message; the posting card owns layout.
pub fn split_components(menu: &Menu) -> Vec<Component> {
    let mut out = Vec::new();
    for group in &menu.groups {
        for (part, chunk) in distinct_options(group)
            .chunks(SELECT_OPTION_LIMIT)
            .enumerate()
        {
            out.push(Component {
                group_id: group.id.clone(),
                part,
                options: chunk.iter().map(|o| (*o).clone()).collect(),
            });
        }
    }
    out
}

fn facts_for(ctx: &GrantContext, role_id: u64) -> Option<RoleFacts> {
    ctx.role_facts.iter().find(|f| f.id == role_id).copied()
}

fn request<'a>(ctx: &GrantContext<'a>, role: RoleFacts) -> GrantRequest<'a> {
    GrantRequest {
        path: GrantPath::Menu,
        role,
        bot_top_position: ctx.bot_top_position,
        guild_id: ctx.guild_id,
        protected_role_ids: ctx.protected_role_ids,
        safeword: ctx.safeword,
        safeword_restricted_role_ids: ctx.safeword_restricted_role_ids,
        staff: None,
    }
}

/// K03 `decide` on the Menu path. A role with no facts counts as deleted.
fn check_add(ctx: &GrantContext, role_id: u64) -> Result<(), Reason> {
    let role = facts_for(ctx, role_id).unwrap_or(RoleFacts {
        id: role_id,
        position: 0,
        managed: false,
        permissions: 0,
        exists: false,
    });
    match decide(&request(ctx, role)) {
        Decision::Allow => Ok(()),
        Decision::Deny(reason) => Err(Reason::Denied(reason)),
    }
}

enum Removal {
    Remove,
    /// Nothing to remove: the role is deleted or unknown to the caller.
    Gone,
    Blocked(DenyReason),
}

fn check_removal(ctx: &GrantContext, role_id: u64) -> Removal {
    match facts_for(ctx, role_id) {
        Some(role) if role.exists => match decide_removal(&request(ctx, role)) {
            Decision::Allow => Removal::Remove,
            Decision::Deny(reason) => Removal::Blocked(reason),
        },
        _ => Removal::Gone,
    }
}

/// Remove one role if allowed, otherwise report why not.
fn remove_or_skip(plan: &mut Plan, ctx: &GrantContext, role_id: u64) {
    match check_removal(ctx, role_id) {
        Removal::Remove => plan.remove.push(role_id),
        Removal::Gone => plan
            .skipped
            .push((role_id, Reason::RemovalDenied(DenyReason::RoleDeleted))),
        Removal::Blocked(r) => plan.skipped.push((role_id, Reason::RemovalDenied(r))),
    }
}

/// Exclusive group, option not held: add it and remove the other held
/// options, all or nothing.
fn exclusive_swap(plan: &mut Plan, ctx: &GrantContext, option: u64, others: &[u64]) {
    if let Err(reason) = check_add(ctx, option) {
        plan.skipped.push((option, reason));
        return;
    }
    let checks: Vec<(u64, Removal)> = others
        .iter()
        .map(|r| (*r, check_removal(ctx, *r)))
        .collect();
    let blocked: Vec<(u64, DenyReason)> = checks
        .iter()
        .filter_map(|(r, c)| match c {
            Removal::Blocked(d) => Some((*r, *d)),
            _ => None,
        })
        .collect();
    if !blocked.is_empty() {
        for (r, d) in blocked {
            plan.skipped.push((r, Reason::RemovalDenied(d)));
        }
        plan.skipped.push((option, Reason::SwapBlocked));
        return;
    }
    plan.add.push(option);
    for (r, c) in checks {
        match c {
            Removal::Remove => plan.remove.push(r),
            _ => plan
                .skipped
                .push((r, Reason::RemovalDenied(DenyReason::RoleDeleted))),
        }
    }
}

/// Decide what a click does. `Err` only for an unknown group; everything
/// else is reported in `Plan::skipped`.
pub fn plan_click(request: &PlanRequest) -> Result<Plan, PlanError> {
    let ctx = &request.grant;
    let click = request.click;
    let group = request
        .menu
        .groups
        .iter()
        .find(|g| g.id == click.group_id)
        .ok_or(PlanError::UnknownGroup)?;
    let option = click.option_role_id;
    let mut plan = Plan::default();

    let group_roles: Vec<u64> = distinct_options(group).iter().map(|o| o.role_id).collect();
    if !group_roles.contains(&option) {
        plan.skipped.push((option, Reason::UnknownOption));
        return Ok(plan);
    }
    let member: BTreeSet<u64> = click.member_roles.iter().copied().collect();
    let held_in_group: Vec<u64> = group_roles
        .iter()
        .copied()
        .filter(|r| member.contains(r))
        .collect();
    let holds_option = member.contains(&option);

    match group.mode {
        GroupMode::Multi if holds_option => remove_or_skip(&mut plan, ctx, option),
        GroupMode::Multi => {
            if let Err(reason) = check_add(ctx, option) {
                plan.skipped.push((option, reason));
            } else if group.max.is_some_and(|m| held_in_group.len() >= m as usize) {
                plan.skipped.push((option, Reason::MaxReached));
            } else {
                plan.add.push(option);
            }
        }
        GroupMode::Exclusive => {
            let others: Vec<u64> = held_in_group
                .iter()
                .copied()
                .filter(|r| *r != option)
                .collect();
            if holds_option {
                // Not a toggle: the group keeps its choice. Extra options held
                // from before are removed on this click unless `decide_removal`
                // denies the removal; then the extra option stays and
                // `RemovalDenied` is reported, so the member can still hold two
                // options of the group.
                plan.skipped.push((option, Reason::AlreadySelected));
                for other in others {
                    remove_or_skip(&mut plan, ctx, other);
                }
            } else {
                exclusive_swap(&mut plan, ctx, option, &others);
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::{
        ADMINISTRATOR, BAN_MEMBERS, KICK_MEMBERS, MANAGE_GUILD, MANAGE_ROLES, MODERATE_MEMBERS,
    };
    use std::collections::BTreeSet;

    const GUILD: u64 = 1000;
    const BOT_TOP: u16 = 10;

    // Group "pick": exclusive, roles 2001..2004. Group "pron": multi, 2101..2103.
    fn opt(id: u64) -> MenuOption {
        MenuOption {
            role_id: id,
            label_key: format!("label_{id}"),
        }
    }

    fn group(id: &str, mode: GroupMode, max: Option<u32>, ids: &[u64]) -> Group {
        Group {
            id: id.to_string(),
            mode,
            max,
            options: ids.iter().map(|i| opt(*i)).collect(),
        }
    }

    fn facts(id: u64) -> RoleFacts {
        RoleFacts {
            id,
            position: 5,
            managed: false,
            permissions: 0,
            exists: true,
        }
    }

    struct Env {
        facts: Vec<RoleFacts>,
        protected: Vec<u64>,
        restricted: Vec<u64>,
        safeword: SafewordState,
    }

    impl Env {
        fn new(ids: &[u64]) -> Env {
            Env {
                facts: ids.iter().map(|i| facts(*i)).collect(),
                protected: vec![],
                restricted: vec![],
                safeword: SafewordState::Inactive,
            }
        }
        fn edit(&mut self, id: u64, f: impl FnOnce(&mut RoleFacts)) {
            let r = self.facts.iter_mut().find(|r| r.id == id).expect("facts");
            f(r);
        }
        fn ctx(&self) -> GrantContext<'_> {
            GrantContext {
                guild_id: GUILD,
                bot_top_position: BOT_TOP,
                role_facts: &self.facts,
                protected_role_ids: &self.protected,
                safeword: self.safeword,
                safeword_restricted_role_ids: &self.restricted,
            }
        }
    }

    fn click(held: &[u64], group: &str, option: u64) -> Click {
        Click {
            member_roles: held.to_vec(),
            group_id: group.to_string(),
            option_role_id: option,
        }
    }

    fn run(menu: &Menu, c: &Click, env: &Env) -> Plan {
        plan_click(&PlanRequest {
            menu,
            click: c,
            grant: env.ctx(),
        })
        .expect("plan")
    }

    fn menu() -> Menu {
        Menu {
            key: "main".into(),
            groups: vec![
                group(
                    "pick",
                    GroupMode::Exclusive,
                    None,
                    &[2001, 2002, 2003, 2004],
                ),
                group("pron", GroupMode::Multi, None, &[2101, 2102, 2103]),
                group("cap2", GroupMode::Multi, Some(2), &[2201, 2202, 2203]),
            ],
        }
    }

    fn env() -> Env {
        Env::new(&[2001, 2002, 2003, 2004, 2101, 2102, 2103, 2201, 2202, 2203])
    }

    fn sorted(v: &[u64]) -> Vec<u64> {
        let mut v = v.to_vec();
        v.sort_unstable();
        v
    }

    // ---- exclusive ----

    #[test]
    fn exclusive_swap_removes_the_others() {
        let p = run(&menu(), &click(&[2001, 2101], "pick", 2003), &env());
        assert_eq!(p.add, vec![2003]);
        assert_eq!(p.remove, vec![2001]);
        assert!(p.skipped.is_empty());
    }

    #[test]
    fn exclusive_swap_removes_every_other_option_held() {
        let p = run(&menu(), &click(&[2001, 2002, 2004], "pick", 2003), &env());
        assert_eq!(p.add, vec![2003]);
        assert_eq!(sorted(&p.remove), vec![2001, 2002, 2004]);
    }

    #[test]
    fn exclusive_first_pick_only_adds() {
        let p = run(&menu(), &click(&[2101], "pick", 2002), &env());
        assert_eq!(p.add, vec![2002]);
        assert!(p.remove.is_empty());
    }

    #[test]
    fn exclusive_click_on_held_option_is_a_reported_noop() {
        let p = run(&menu(), &click(&[2002], "pick", 2002), &env());
        assert!(p.add.is_empty() && p.remove.is_empty());
        assert_eq!(p.skipped, vec![(2002, Reason::AlreadySelected)]);
    }

    #[test]
    fn exclusive_click_on_held_option_repairs_extra_held_options() {
        let p = run(&menu(), &click(&[2002, 2004], "pick", 2002), &env());
        assert!(p.add.is_empty());
        assert_eq!(p.remove, vec![2004]);
        assert_eq!(p.skipped, vec![(2002, Reason::AlreadySelected)]);
    }

    #[test]
    fn exclusive_repair_with_denied_removal_keeps_extra_and_reports_it() {
        let mut e = env();
        e.edit(2001, |r| r.position = BOT_TOP); // bot cannot touch this role
        let p = run(&menu(), &click(&[2001, 2002], "pick", 2002), &e);
        assert!(p.add.is_empty());
        assert!(p.remove.is_empty(), "the unreachable extra option stays");
        assert_eq!(
            p.skipped,
            vec![
                (2002, Reason::AlreadySelected),
                (2001, Reason::RemovalDenied(DenyReason::AtOrAboveBot)),
            ]
        );
    }

    #[test]
    fn exclusive_ignores_max() {
        let mut m = menu();
        m.groups[0].max = Some(0);
        let p = run(&m, &click(&[], "pick", 2001), &env());
        assert_eq!(p.add, vec![2001]);
    }

    #[test]
    fn exclusive_swap_refused_when_a_needed_removal_is_denied() {
        let mut e = env();
        e.edit(2001, |r| r.position = BOT_TOP); // bot cannot touch the held role
        let p = run(&menu(), &click(&[2001], "pick", 2003), &e);
        assert!(p.add.is_empty(), "must not end with two options");
        assert!(p.remove.is_empty());
        assert!(p.skipped.contains(&(2003, Reason::SwapBlocked)));
        assert!(p
            .skipped
            .contains(&(2001, Reason::RemovalDenied(DenyReason::AtOrAboveBot))));
    }

    #[test]
    fn exclusive_swap_not_blocked_by_a_gone_held_role() {
        let mut e = env();
        e.edit(2001, |r| r.exists = false);
        let p = run(&menu(), &click(&[2001, 2002], "pick", 2003), &e);
        assert_eq!(p.add, vec![2003]);
        assert_eq!(p.remove, vec![2002]);
        assert_eq!(
            p.skipped,
            vec![(2001, Reason::RemovalDenied(DenyReason::RoleDeleted))]
        );
    }

    #[test]
    fn exclusive_denied_target_keeps_current_choice() {
        let mut e = env();
        e.edit(2003, |r| r.permissions = ADMINISTRATOR);
        let p = run(&menu(), &click(&[2001], "pick", 2003), &e);
        assert!(p.add.is_empty());
        assert!(p.remove.is_empty(), "member must keep their current option");
        assert_eq!(
            p.skipped,
            vec![(2003, Reason::Denied(DenyReason::PrivilegedPermission))]
        );
    }

    // ---- multi / max / toggle ----

    #[test]
    fn multi_keeps_several() {
        let p = run(&menu(), &click(&[2101, 2102], "pron", 2103), &env());
        assert_eq!(p.add, vec![2103]);
        assert!(p.remove.is_empty());
        assert!(p.skipped.is_empty());
    }

    #[test]
    fn multi_click_on_held_option_toggles_it_off() {
        let p = run(&menu(), &click(&[2101, 2102], "pron", 2101), &env());
        assert!(p.add.is_empty());
        assert_eq!(p.remove, vec![2101]);
        assert!(p.skipped.is_empty());
    }

    #[test]
    fn max_refuses_the_click_that_would_exceed_it() {
        let e = env();
        let m = menu();
        let below = run(&m, &click(&[2201], "cap2", 2202), &e);
        assert_eq!(below.add, vec![2202], "reaching max is allowed");
        let at = run(&m, &click(&[2201, 2202], "cap2", 2203), &e);
        assert!(at.add.is_empty() && at.remove.is_empty());
        assert_eq!(at.skipped, vec![(2203, Reason::MaxReached)]);
        assert_eq!(at.skipped[0].1.code(), "max_reached");
    }

    #[test]
    fn max_still_allows_toggling_off_at_the_limit() {
        let p = run(&menu(), &click(&[2201, 2202], "cap2", 2201), &env());
        assert_eq!(p.remove, vec![2201]);
        assert!(p.add.is_empty());
    }

    #[test]
    fn max_counts_only_roles_of_this_group_and_each_once() {
        let e = env();
        // Roles from other groups and duplicates of one role do not count.
        let p = run(
            &menu(),
            &click(&[2101, 2102, 2103, 2201, 2201], "cap2", 2202),
            &e,
        );
        assert_eq!(p.add, vec![2202]);
        // A group listing the same role twice does not double count either.
        let mut m = menu();
        m.groups[2].options.push(opt(2201));
        let p = run(&m, &click(&[2201], "cap2", 2202), &e);
        assert_eq!(p.add, vec![2202]);
    }

    #[test]
    fn max_zero_refuses_every_add() {
        let mut m = menu();
        m.groups[1].max = Some(0);
        let p = run(&m, &click(&[], "pron", 2101), &env());
        assert!(p.add.is_empty());
        assert_eq!(p.skipped, vec![(2101, Reason::MaxReached)]);
    }

    #[test]
    fn over_max_state_refuses_adds_but_allows_removals() {
        let e = env();
        let m = menu();
        let add = run(&m, &click(&[2201, 2202, 2203], "cap2", 2201), &e);
        assert_eq!(add.remove, vec![2201]);
        let mut m2 = menu();
        m2.groups[2].options.push(opt(2204));
        let e2 = Env::new(&[2201, 2202, 2203, 2204]);
        let p = run(&m2, &click(&[2201, 2202, 2203], "cap2", 2204), &e2);
        assert_eq!(p.skipped, vec![(2204, Reason::MaxReached)]);
    }

    #[test]
    fn toggle_off_denied_when_bot_cannot_reach_the_role() {
        let mut e = env();
        e.edit(2101, |r| r.position = BOT_TOP + 1);
        let p = run(&menu(), &click(&[2101], "pron", 2101), &e);
        assert!(p.remove.is_empty());
        assert_eq!(
            p.skipped,
            vec![(2101, Reason::RemovalDenied(DenyReason::AtOrAboveBot))]
        );
    }

    // ---- unknown / deleted ----

    #[test]
    fn unknown_group_is_an_error_without_ids() {
        let r = plan_click(&PlanRequest {
            menu: &menu(),
            click: &click(&[], "nope", 2001),
            grant: env().ctx(),
        });
        assert_eq!(r, Err(PlanError::UnknownGroup));
        assert_eq!(PlanError::UnknownGroup.code(), "unknown_group");
    }

    #[test]
    fn option_not_in_group_is_skipped_and_reported() {
        let p = run(&menu(), &click(&[], "pick", 2101), &env());
        assert!(p.add.is_empty() && p.remove.is_empty());
        assert_eq!(p.skipped, vec![(2101, Reason::UnknownOption)]);
        let p = run(&menu(), &click(&[], "pick", 9999), &env());
        assert_eq!(p.skipped, vec![(9999, Reason::UnknownOption)]);
    }

    #[test]
    fn deleted_option_role_is_skipped_and_reported_not_an_error() {
        let mut e = env();
        e.edit(2102, |r| r.exists = false);
        let m = menu();
        let p = run(&m, &click(&[2101], "pron", 2102), &e);
        assert!(p.add.is_empty() && p.remove.is_empty());
        assert_eq!(
            p.skipped,
            vec![(2102, Reason::Denied(DenyReason::RoleDeleted))]
        );
        // The other options of the same menu still work.
        let p = run(&m, &click(&[2101], "pron", 2103), &e);
        assert_eq!(p.add, vec![2103]);
    }

    #[test]
    fn role_with_no_facts_is_treated_as_deleted() {
        let e = Env::new(&[2001]);
        let p = run(&menu(), &click(&[], "pick", 2002), &e);
        assert!(p.add.is_empty());
        assert_eq!(
            p.skipped,
            vec![(2002, Reason::Denied(DenyReason::RoleDeleted))]
        );
    }

    #[test]
    fn facts_for_another_role_are_not_used() {
        // Only the entry whose id matches the clicked role counts.
        let mut e = Env::new(&[2002]);
        e.facts[0].id = 2002;
        e.facts[0].permissions = 0;
        let p = run(&menu(), &click(&[], "pick", 2001), &e);
        assert_eq!(
            p.skipped,
            vec![(2001, Reason::Denied(DenyReason::RoleDeleted))]
        );
    }

    // ---- >25 options ----

    fn big_menu(n: u64, mode: GroupMode) -> Menu {
        let ids: Vec<u64> = (0..n).map(|i| 5000 + i).collect();
        Menu {
            key: "big".into(),
            groups: vec![group("big", mode, None, &ids)],
        }
    }

    #[test]
    fn split_boundaries_25_26_50_51() {
        let sizes = |n: u64| -> Vec<usize> {
            split_components(&big_menu(n, GroupMode::Multi))
                .iter()
                .map(|c| c.options.len())
                .collect()
        };
        assert_eq!(sizes(0), Vec::<usize>::new());
        assert_eq!(sizes(1), vec![1]);
        assert_eq!(sizes(25), vec![25]);
        assert_eq!(sizes(26), vec![25, 1]);
        assert_eq!(sizes(50), vec![25, 25]);
        assert_eq!(sizes(51), vec![25, 25, 1]);
    }

    #[test]
    fn split_keeps_declared_order_and_numbers_parts() {
        let comps = split_components(&big_menu(60, GroupMode::Multi));
        assert_eq!(comps.len(), 3);
        for (i, c) in comps.iter().enumerate() {
            assert_eq!(c.part, i);
            assert_eq!(c.group_id, "big");
            assert!(c.options.len() <= SELECT_OPTION_LIMIT);
        }
        let flat: Vec<u64> = comps
            .iter()
            .flat_map(|c| c.options.iter().map(|o| o.role_id))
            .collect();
        assert_eq!(flat, (0..60).map(|i| 5000 + i).collect::<Vec<u64>>());
    }

    #[test]
    fn split_never_mixes_groups_and_is_deterministic() {
        let m = Menu {
            key: "k".into(),
            groups: vec![
                group("a", GroupMode::Multi, None, &(1..=30).collect::<Vec<u64>>()),
                group("empty", GroupMode::Multi, None, &[]),
                group("b", GroupMode::Exclusive, None, &[100, 101]),
            ],
        };
        let comps = split_components(&m);
        let shape: Vec<(&str, usize, usize)> = comps
            .iter()
            .map(|c| (c.group_id.as_str(), c.part, c.options.len()))
            .collect();
        assert_eq!(shape, vec![("a", 0, 25), ("a", 1, 5), ("b", 0, 2)]);
        assert_eq!(comps, split_components(&m));
    }

    #[test]
    fn split_drops_repeated_roles_within_a_group() {
        let m = Menu {
            key: "k".into(),
            groups: vec![group("a", GroupMode::Multi, None, &[1, 2, 1, 3, 2])],
        };
        let comps = split_components(&m);
        let ids: Vec<u64> = comps[0].options.iter().map(|o| o.role_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn policy_applies_to_the_whole_group_across_components() {
        // Option 5030 sits in the second component; the exclusive swap still
        // removes the option held from the first component.
        let m = big_menu(40, GroupMode::Exclusive);
        let ids: Vec<u64> = (0..40).map(|i| 5000 + i).collect();
        let e = Env::new(&ids);
        let p = run(&m, &click(&[5003], "big", 5030), &e);
        assert_eq!(p.add, vec![5030]);
        assert_eq!(p.remove, vec![5003]);
    }

    // ---- K03 ----

    #[test]
    fn every_add_is_denied_with_the_k03_code() {
        type Edit = fn(&mut Env);
        let cases: Vec<(&str, DenyReason, Edit)> = vec![
            (
                "privileged_permission",
                DenyReason::PrivilegedPermission,
                |e| e.edit(2101, |r| r.permissions = MANAGE_ROLES),
            ),
            ("managed_role", DenyReason::ManagedRole, |e| {
                e.edit(2101, |r| r.managed = true)
            }),
            ("protected_role", DenyReason::ProtectedRole, |e| {
                e.protected.push(2101)
            }),
            ("safeword_restricted", DenyReason::SafewordRestricted, |e| {
                e.restricted.push(2101);
                e.safeword = SafewordState::Active;
            }),
            ("at_or_above_bot", DenyReason::AtOrAboveBot, |e| {
                e.edit(2101, |r| r.position = BOT_TOP)
            }),
            ("role_deleted", DenyReason::RoleDeleted, |e| {
                e.edit(2101, |r| r.exists = false)
            }),
        ];
        for (code, reason, edit) in cases {
            for (mode_menu, g) in [(menu(), "pron"), (exclusive_pron_menu(), "pron")] {
                let mut e = env();
                edit(&mut e);
                let p = run(&mode_menu, &click(&[], g, 2101), &e);
                assert!(p.add.is_empty(), "{code}");
                assert_eq!(p.skipped, vec![(2101, Reason::Denied(reason))], "{code}");
                assert_eq!(p.skipped[0].1.code(), code);
            }
        }
    }

    fn exclusive_pron_menu() -> Menu {
        Menu {
            key: "x".into(),
            groups: vec![group(
                "pron",
                GroupMode::Exclusive,
                None,
                &[2101, 2102, 2103],
            )],
        }
    }

    #[test]
    fn each_privileged_bit_is_denied() {
        for bit in [
            ADMINISTRATOR,
            MANAGE_GUILD,
            MANAGE_ROLES,
            BAN_MEMBERS,
            KICK_MEMBERS,
            MODERATE_MEMBERS,
        ] {
            let mut e = env();
            e.edit(2102, |r| r.permissions = bit | 1);
            let p = run(&menu(), &click(&[], "pron", 2102), &e);
            assert!(p.add.is_empty(), "{bit:#x}");
            assert_eq!(p.skipped[0].1.code(), "privileged_permission");
        }
    }

    #[test]
    fn everyone_role_cannot_be_added_through_a_menu() {
        let mut m = menu();
        m.groups[1].options.push(opt(GUILD));
        let mut e = env();
        e.facts.push(facts(GUILD));
        let p = run(&m, &click(&[], "pron", GUILD), &e);
        assert!(p.add.is_empty());
        assert_eq!(p.skipped[0].1.code(), "everyone_role");
    }

    #[test]
    fn restricted_role_is_addable_when_safeword_is_inactive() {
        let mut e = env();
        e.restricted.push(2101);
        let p = run(&menu(), &click(&[], "pron", 2101), &e);
        assert_eq!(p.add, vec![2101]);
    }

    #[test]
    fn removals_are_allowed_during_safeword() {
        let mut e = env();
        e.restricted = vec![2101, 2001];
        e.protected = vec![2101];
        e.edit(2101, |r| r.permissions = ADMINISTRATOR);
        e.safeword = SafewordState::Active;
        // Multi toggle-off of a restricted, protected, privileged role.
        let p = run(&menu(), &click(&[2101], "pron", 2101), &e);
        assert_eq!(p.remove, vec![2101]);
        assert!(p.skipped.is_empty());
        // Exclusive swap away from a restricted option to an allowed one.
        let p = run(&menu(), &click(&[2001], "pick", 2002), &e);
        assert_eq!(p.add, vec![2002]);
        assert_eq!(p.remove, vec![2001]);
        // But swapping TO the restricted option is denied and nothing is removed.
        let p = run(&menu(), &click(&[2002], "pick", 2001), &e);
        assert!(p.add.is_empty() && p.remove.is_empty());
        assert_eq!(p.skipped[0].1.code(), "safeword_restricted");
    }

    // ---- codes ----

    #[test]
    fn reason_codes_are_stable() {
        let cases = [
            (Reason::Denied(DenyReason::ManagedRole), "managed_role"),
            (
                Reason::RemovalDenied(DenyReason::AtOrAboveBot),
                "removal_at_or_above_bot",
            ),
            (
                Reason::RemovalDenied(DenyReason::RoleDeleted),
                "removal_role_deleted",
            ),
            (Reason::UnknownOption, "unknown_option"),
            (Reason::MaxReached, "max_reached"),
            (Reason::AlreadySelected, "already_selected"),
            (Reason::SwapBlocked, "swap_blocked"),
        ];
        for (r, c) in cases {
            assert_eq!(r.code(), c);
        }
    }

    // ---- seeded sequence against a model member ----

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

    #[test]
    fn seeded_click_sequences_keep_every_invariant() {
        let m = Menu {
            key: "rand".into(),
            groups: vec![
                group("ex1", GroupMode::Exclusive, None, &[3001, 3002, 3003, 3004]),
                group("ex2", GroupMode::Exclusive, Some(1), &[3011, 3012, 3013]),
                group(
                    "multi",
                    GroupMode::Multi,
                    Some(3),
                    &[3021, 3022, 3023, 3024, 3025, 3026],
                ),
                group("free", GroupMode::Multi, None, &[3031, 3032, 3033]),
            ],
        };
        let all: Vec<u64> = m
            .groups
            .iter()
            .flat_map(|g| g.options.iter().map(|o| o.role_id))
            .collect();
        let mut base = Env::new(&all);
        base.edit(3003, |r| r.permissions = BAN_MEMBERS);
        base.edit(3012, |r| r.position = BOT_TOP); // bot cannot remove it
        base.edit(3024, |r| r.exists = false);
        base.edit(3026, |r| r.managed = true);
        base.protected = vec![3002];
        base.restricted = vec![3033, 3021];

        let mut rng = Lcg(0x5eed_1234);
        let mut clicks = 0u32;
        let mut effective = 0u32;
        for run_no in 0..200 {
            // Start: nothing, or the un-removable ex2 role, plus an unrelated role.
            let mut held: BTreeSet<u64> = BTreeSet::new();
            if run_no % 3 == 0 {
                held.insert(3012);
            }
            if run_no % 5 == 0 {
                held.insert(4000);
            }
            for _ in 0..100 {
                let mut e = Env::new(&[]);
                e.facts = base.facts.clone();
                e.protected = base.protected.clone();
                e.restricted = base.restricted.clone();
                e.safeword = if rng.next(2) == 0 {
                    SafewordState::Active
                } else {
                    SafewordState::Inactive
                };
                let gi = rng.next(m.groups.len() as u64 + 1) as usize;
                let gid = if gi == m.groups.len() {
                    "ghost".to_string()
                } else {
                    m.groups[gi].id.clone()
                };
                let opt_id = if rng.next(12) == 0 {
                    9999
                } else {
                    all[rng.next(all.len() as u64) as usize]
                };
                let c = Click {
                    member_roles: held.iter().copied().collect(),
                    group_id: gid,
                    option_role_id: opt_id,
                };
                clicks += 1;
                let Ok(p) = plan_click(&PlanRequest {
                    menu: &m,
                    click: &c,
                    grant: e.ctx(),
                }) else {
                    assert_eq!(c.group_id, "ghost");
                    continue;
                };
                if p.add.is_empty() && p.remove.is_empty() {
                    assert!(!p.skipped.is_empty(), "silent no-op for {c:?}");
                } else {
                    effective += 1;
                }
                // Every add re-checked independently through K03 (Menu path).
                for a in &p.add {
                    assert!(!held.contains(a));
                    let rf = e
                        .facts
                        .iter()
                        .find(|f| f.id == *a)
                        .copied()
                        .unwrap_or(RoleFacts {
                            id: *a,
                            position: 0,
                            managed: false,
                            permissions: 0,
                            exists: false,
                        });
                    let d = decide(&GrantRequest {
                        path: GrantPath::Menu,
                        role: rf,
                        bot_top_position: BOT_TOP,
                        guild_id: GUILD,
                        protected_role_ids: &e.protected,
                        safeword: e.safeword,
                        safeword_restricted_role_ids: &e.restricted,
                        staff: None,
                    });
                    assert_eq!(d, Decision::Allow, "add {a} not allowed by K03");
                }
                for r in &p.remove {
                    assert!(held.contains(r));
                    assert!(!p.add.contains(r));
                }
                for r in &p.remove {
                    held.remove(r);
                }
                for a in &p.add {
                    held.insert(*a);
                }
                for g in &m.groups {
                    let n = g
                        .options
                        .iter()
                        .filter(|o| held.contains(&o.role_id))
                        .count();
                    match g.mode {
                        GroupMode::Exclusive => assert!(n <= 1, "{} holds {n}", g.id),
                        GroupMode::Multi => {
                            if let Some(max) = g.max {
                                assert!(n as u32 <= max, "{} holds {n}", g.id);
                            }
                        }
                    }
                }
            }
        }
        assert!(clicks >= 20_000);
        assert!(effective > 1_000, "sequence too inert: {effective}");
    }
}

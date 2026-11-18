# Safeword State Machine (SW1)

Pure, Discord-free state machine (`src/safeword/{mod,state}.rs`) replacing the YAGPDB safeword custom command.

## Rules Enforced
- **Activation & replay safety:** Snapshots `held_roles ∩ restricted_roles`, removes them, adds the safeword role if not held, and DMs verbatim text. Replayed `ReactionAdded` while `Activating` or `Active` never overwrites the snapshot (fixes YAGPDB line 68 bug).
- **Restore filtering (D06):** By default (`filter_restore = true`), release skips roles struck via `Strike(role)`, deleted via `RoleDeleted(role)`, or present in `protected_roles`; setting `filter_restore = false` restores all originally saved/deferred roles.
- **Deferred grants (D06):** When `defer_grants = true` (default), `GrantDuringActive(role)` records the role into the snapshot for restore on release; when `false`, it is ignored.
- **Acknowledgements & crash recovery:** `ActionDone(id)` / `ActionFailed(id, code)` track per-step progress so retries never repeat acknowledged actions. `ReactionRemoved` during `Activating` completes activation then releases. `restart_action(state, reaction_present_now)` returns one deterministic `RestartAction` for every state.

## Tested & RED Evidence
- Committed failing tests first in `222701a` (RED: 10 of 12 `safeword::state::tests::*` failed against stub `step`/`restart_action`/`is_actionable`: `activation_snapshots_held_roles_intersected_with_restricted_set`, `replayed_add_while_activating_or_active_does_not_overwrite_snapshot`, `remove_during_activating_completes_activation_steps_then_releases`, `restore_skips_struck_deleted_and_now_protected_roles`, `empty_snapshot_still_toggles_safeword_role_and_sends_both_dms`, `grant_during_active_is_deferred_when_enabled_and_ignored_when_disabled`, `restart_in_every_state_gives_one_deterministic_next_action`, `action_failure_leaves_state_retryable_and_never_repeats_acknowledged_actions`, `bots_and_non_matching_reactions_are_ignored_by_is_actionable`, `table_driven_sweep_over_all_state_event_pairs_upholds_invariants`).
- GREEN: all 13 `safeword::state::tests::*` pass (including `boundary_and_adversarial_transitions_preserve_consistency`).

## Config Keys (Read by Wiring / SW3, Not Here)
- `SAFEWORD_MODE` (default `off`; any value other than `off|shadow|live` fails closed to `off`).
- `SAFEWORD_ROLE_ID`, `SAFEWORD_CHANNEL_ID`, `SAFEWORD_MESSAGE_ID`, `SAFEWORD_EMOJI_ID` (default `""`; missing/invalid ID fails closed and disables safeword).
- `SAFEWORD_RESTRICTED_ROLE_IDS`, `PROTECTED_ROLE_IDS` (default `""`; comma-separated role IDs; invalid entries rejected/fail closed).
- `SAFEWORD_DM` (default `true`; invalid fails closed to `true`), `SAFEWORD_DEFER_GRANTS` (default `true`; invalid fails closed to `true`), `SAFEWORD_FILTER_RESTORE` (default `true`; invalid fails closed to `true`).

## What Is NOT Done
No Discord runtime, no slash commands, no SQLite persistence (SW2), and no wiring beyond `mod safeword;` in `src/main.rs`.

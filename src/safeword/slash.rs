//! The `/safeword status|strike|clear` slash command: a thin shell over the SW4 command logic
//! (`super::command`, `docs/verification/SAFEWORD-COMMAND.md`).
//!
//! The shell only maps Discord input to SW4's plain requests and SW4's reply to a message. It
//! decides nothing: staff, guild, mode, confirmation and ephemeral rules are SW4's. Registration
//! with Discord is an owner step; `configured_commands` in `main.rs` lists the command only
//! while the safeword mode is on.

use super::command::{self, ClearRequest, Code, Env, Invoker, Reply, StrikeRequest};
use crate::{Context, Error};
use poise::serenity_prelude as serenity;

/// One subcommand call, in plain values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Status { member: u64 },
    Strike { member: u64, role: u64 },
    Clear { member: u64, confirm: bool },
}

/// What is sent back: the text and whether only the caller sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub text: String,
    pub ephemeral: bool,
}

/// The shell's whole job on a reply: SW4's text and SW4's ephemeral flag, unchanged.
pub fn answer(reply: &Reply) -> Answer {
    Answer {
        text: reply.text(),
        ephemeral: reply.is_ephemeral(),
    }
}

/// The answer when the feature is not running (switched off or dropped at start-up).
pub fn not_running() -> Answer {
    answer(&Reply::new(Code::Disabled))
}

/// The caller, in plain values. A caller whose roles are unknown holds none, so is not staff.
pub fn invoker(user: u64, guild: u64, role_ids: &[u64]) -> Invoker {
    Invoker {
        user,
        guild,
        role_ids: role_ids.to_vec(),
    }
}

/// `clear`'s confirm option: unset means false.
pub fn clear_call(member: u64, confirm: Option<bool>) -> Call {
    Call::Clear {
        member,
        confirm: confirm.unwrap_or(false),
    }
}

/// Runs one call through SW4 and maps the reply.
pub fn dispatch(env: &mut dyn Env, who: &Invoker, call: &Call) -> Answer {
    let _ = (env, who, call);
    Answer {
        text: String::new(),
        ephemeral: false,
    }
}

/// Runs a call on a blocking thread (SW4 and its collaborators are synchronous) and replies
/// ephemerally.
async fn run(ctx: Context<'_>, call: Call) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let answer = match (
        ctx.data().wired.safeword.clone(),
        ctx.guild_id(),
        ctx.author_member().await.map(|m| m.roles.clone()),
    ) {
        (Some(runtime), Some(guild), roles) => {
            let role_ids: Vec<u64> = roles.unwrap_or_default().iter().map(|r| r.get()).collect();
            let who = invoker(ctx.author().id.get(), guild.get(), &role_ids);
            tokio::task::spawn_blocking(move || {
                let mut env = runtime.command_env();
                dispatch(&mut env, &who, &call)
            })
            .await
            .unwrap_or_else(|_| Answer {
                text: "The safeword command failed [task_failed]".to_string(),
                ephemeral: true,
            })
        }
        _ => not_running(),
    };
    ctx.send(
        poise::CreateReply::default()
            .content(answer.text)
            .ephemeral(answer.ephemeral),
    )
    .await?;
    Ok(())
}

/// Safeword status, strike and clear (staff).
#[poise::command(
    slash_command,
    guild_only,
    subcommands("status", "strike", "clear"),
    subcommand_required
)]
pub async fn safeword(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Shows a member's safeword state (staff: anyone; others: yourself).
#[poise::command(slash_command, guild_only)]
pub async fn status(
    ctx: Context<'_>,
    #[description = "The member to look at"] member: serenity::User,
) -> Result<(), Error> {
    run(
        ctx,
        Call::Status {
            member: member.id.get(),
        },
    )
    .await
}

/// Staff: stop one role from being given back when the safeword is cleared.
#[poise::command(slash_command, guild_only)]
pub async fn strike(
    ctx: Context<'_>,
    #[description = "The member"] member: serenity::User,
    #[description = "The role to strike from the restore list"] role: serenity::Role,
) -> Result<(), Error> {
    run(
        ctx,
        Call::Strike {
            member: member.id.get(),
            role: role.id.get(),
        },
    )
    .await
}

/// Staff: clear a member's safeword and give the roles back (repeat with confirm).
#[poise::command(slash_command, guild_only)]
pub async fn clear(
    ctx: Context<'_>,
    #[description = "The member"] member: serenity::User,
    #[description = "Set on the second call to confirm"] confirm: Option<bool>,
) -> Result<(), Error> {
    run(ctx, clear_call(member.id.get(), confirm)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::runtime::FactsOutcome;
    use crate::safeword::command::{CommandConfig, ConfirmBook};
    use crate::safeword::config::SafewordConfig;
    use crate::safeword::runtime::{
        DmCode, DmSender, LookupCode, MemberRoles, Ops, ReactionUser, ReactionUsers,
        RoleFactsSource, StaffAlerter, StateStore, WriteCode,
    };
    use crate::safeword::state::{RoleId, Snapshot, State};
    use crate::settings::Mode;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    const HOME: u64 = 780_001;
    const OTHER: u64 = 780_002;
    const STAFF_ROLE: u64 = 780_010;
    const STAFF: u64 = 780_020;
    const MEMBER: u64 = 780_021;
    const STRANGER: u64 = 780_022;
    const SAVED: u64 = 780_030;

    #[derive(Default)]
    struct Tally {
        loads: usize,
        writes: usize,
        role_writes: usize,
        alerts: Vec<&'static str>,
    }

    type Shared = Arc<Mutex<Tally>>;

    struct Store(Shared, HashMap<(u64, u64), (State, u64)>);

    impl StateStore for Store {
        fn load(&mut self, g: u64, m: u64) -> Result<Option<(State, u64)>, &'static str> {
            self.0.lock().unwrap().loads += 1;
            Ok(self.1.get(&(g, m)).cloned())
        }
        fn store(
            &mut self,
            g: u64,
            m: u64,
            state: &State,
            expected: u64,
            _now: i64,
        ) -> Result<u64, &'static str> {
            self.0.lock().unwrap().writes += 1;
            let current = self.1.get(&(g, m)).map_or(0, |(_, v)| *v);
            if current != expected {
                return Err("stale");
            }
            self.1.insert((g, m), (state.clone(), current + 1));
            Ok(current + 1)
        }
        fn clear(&mut self, _g: u64, _m: u64, _e: u64, _n: i64) -> Result<u64, &'static str> {
            self.0.lock().unwrap().writes += 1;
            Err("not_used")
        }
        fn members_with_state(&mut self, _g: u64) -> Result<Vec<u64>, &'static str> {
            Ok(vec![])
        }
    }

    struct Roles(Shared);

    impl MemberRoles for Roles {
        fn member_roles(&mut self, _m: u64) -> Result<Vec<RoleId>, LookupCode> {
            Err(LookupCode::Unavailable)
        }
        fn add_role(&mut self, _m: u64, _r: RoleId) -> Result<(), WriteCode> {
            self.0.lock().unwrap().role_writes += 1;
            Err(WriteCode::Other)
        }
        fn remove_role(&mut self, _m: u64, _r: RoleId) -> Result<(), WriteCode> {
            self.0.lock().unwrap().role_writes += 1;
            Err(WriteCode::Other)
        }
    }

    struct Facts;

    impl RoleFactsSource for Facts {
        fn role_facts(&mut self, _r: RoleId) -> FactsOutcome {
            FactsOutcome::Unavailable
        }
        fn bot_top_position(&mut self) -> Option<u16> {
            None
        }
    }

    struct Dms;

    impl DmSender for Dms {
        fn send_dm(&mut self, _u: u64, _t: &str) -> Result<(), DmCode> {
            Err(DmCode::Other)
        }
    }

    struct Alerts(Shared);

    impl StaffAlerter for Alerts {
        fn alert(&mut self, code: &'static str) {
            self.0.lock().unwrap().alerts.push(code);
        }
    }

    struct Users;

    impl ReactionUsers for Users {
        fn reaction_users(
            &mut self,
            _c: u64,
            _m: u64,
            _e: u64,
            _a: Option<u64>,
            _l: u32,
        ) -> Result<Vec<ReactionUser>, &'static str> {
            Err("unused")
        }
    }

    struct NoSleep;

    impl crate::roles::grant::Sleeper for NoSleep {
        fn sleep_ms(&mut self, _ms: u64) {}
    }

    struct FakeEnv {
        config: CommandConfig,
        ops: Ops,
        confirms: ConfirmBook,
    }

    impl Env for FakeEnv {
        fn config(&self) -> &CommandConfig {
            &self.config
        }
        fn now(&self) -> i64 {
            1_000
        }
        fn ops(&mut self) -> &mut Ops {
            &mut self.ops
        }
        fn confirms(&mut self) -> &mut ConfirmBook {
            &mut self.confirms
        }
    }

    fn active() -> State {
        State::Active(Snapshot {
            saved_roles: vec![SAVED],
            original_roles: vec![SAVED],
            ..Default::default()
        })
    }

    fn env(mode: Mode, home: Option<u64>) -> (FakeEnv, Shared) {
        let tally: Shared = Arc::default();
        let mut rows = HashMap::new();
        rows.insert((HOME, MEMBER), (active(), 1));
        let config = CommandConfig {
            safeword: SafewordConfig {
                mode,
                role_id: 780_040,
                channel_id: 780_041,
                message_id: 780_042,
                emoji_id: 780_043,
                restricted_role_ids: vec![SAVED],
                dm: true,
                defer_grants: true,
            },
            home_guild: home,
            staff_role_id: Some(STAFF_ROLE),
        };
        let ops = Ops {
            members: Box::new(Roles(tally.clone())),
            facts: Box::new(Facts),
            dms: Box::new(Dms),
            alerter: Box::new(Alerts(tally.clone())),
            store: Box::new(Store(tally.clone(), rows)),
            reactions: Box::new(Users),
            sleeper: Box::new(NoSleep),
            protected_role_ids: vec![],
        };
        (
            FakeEnv {
                config,
                ops,
                confirms: ConfirmBook::new(),
            },
            tally,
        )
    }

    fn staff() -> Invoker {
        invoker(STAFF, HOME, &[STAFF_ROLE])
    }

    fn plain(user: u64) -> Invoker {
        invoker(user, HOME, &[])
    }

    #[test]
    fn a_non_staff_strike_is_refused_by_sw4_before_anything_is_read_or_written() {
        let (mut e, tally) = env(Mode::Live, Some(HOME));
        let got = dispatch(
            &mut e,
            &plain(STRANGER),
            &Call::Strike {
                member: MEMBER,
                role: SAVED,
            },
        );
        assert!(got.text.contains("[denied]"), "{}", got.text);
        assert!(got.text.contains("not_staff"), "{}", got.text);
        assert!(got.ephemeral);
        let t = tally.lock().unwrap();
        assert_eq!((t.loads, t.writes, t.role_writes), (0, 0, 0));
    }

    #[test]
    fn a_non_staff_clear_is_refused_and_arms_nothing() {
        let (mut e, tally) = env(Mode::Live, Some(HOME));
        let got = dispatch(&mut e, &plain(STRANGER), &clear_call(MEMBER, Some(true)));
        assert!(got.text.contains("[denied]"), "{}", got.text);
        assert!(got.ephemeral);
        assert!(e.confirms.is_empty());
        assert_eq!(tally.lock().unwrap().writes, 0);
    }

    #[test]
    fn staff_status_shows_the_state_and_the_role_count_to_staff_only() {
        let (mut e, _t) = env(Mode::Live, Some(HOME));
        let got = dispatch(&mut e, &staff(), &Call::Status { member: MEMBER });
        assert!(got.text.contains("[status]"), "{}", got.text);
        assert!(got.text.contains("state=active"), "{}", got.text);
        assert!(got.text.contains("roles=1"), "{}", got.text);
        assert!(got.ephemeral);
        // The member sees their own state but never the count; nobody else's.
        let own = dispatch(&mut e, &plain(MEMBER), &Call::Status { member: MEMBER });
        assert!(own.text.contains("state=active"), "{}", own.text);
        assert!(!own.text.contains("roles="), "{}", own.text);
        let other = dispatch(&mut e, &plain(STRANGER), &Call::Status { member: MEMBER });
        assert!(other.text.contains("[denied]"), "{}", other.text);
    }

    #[test]
    fn staff_strike_goes_through_sw4_and_changes_the_stored_row() {
        let (mut e, tally) = env(Mode::Live, Some(HOME));
        let got = dispatch(
            &mut e,
            &staff(),
            &Call::Strike {
                member: MEMBER,
                role: SAVED,
            },
        );
        assert!(got.text.contains("[struck]"), "{}", got.text);
        assert_eq!(tally.lock().unwrap().writes, 1);
        let again = dispatch(&mut e, &staff(), &Call::Status { member: MEMBER });
        assert!(again.text.contains("roles=0"), "{}", again.text);
    }

    #[test]
    fn clear_without_confirm_only_asks_and_the_flag_is_passed_through() {
        let (mut e, tally) = env(Mode::Live, Some(HOME));
        for confirm in [None, Some(false)] {
            let got = dispatch(&mut e, &staff(), &clear_call(MEMBER, confirm));
            assert!(got.text.contains("[confirm_required]"), "{}", got.text);
            assert!(got.ephemeral);
        }
        let t = tally.lock().unwrap();
        assert_eq!(t.writes, 0);
        assert_eq!(t.role_writes, 0, "nothing is restored without the confirm");
        assert_eq!(
            clear_call(MEMBER, None),
            Call::Clear {
                member: MEMBER,
                confirm: false
            }
        );
        assert_eq!(
            clear_call(MEMBER, Some(true)),
            Call::Clear {
                member: MEMBER,
                confirm: true
            }
        );
    }

    #[test]
    fn mode_guild_and_shadow_rules_are_sw4s_and_reach_the_user() {
        let (mut off, _t) = env(Mode::Off, Some(HOME));
        let got = dispatch(&mut off, &staff(), &Call::Status { member: MEMBER });
        assert!(got.text.contains("[disabled]"), "{}", got.text);
        let (mut shadow, t) = env(Mode::Shadow, Some(HOME));
        let got = dispatch(
            &mut shadow,
            &staff(),
            &Call::Strike {
                member: MEMBER,
                role: SAVED,
            },
        );
        assert!(got.text.contains("[shadow_mode]"), "{}", got.text);
        assert_eq!(t.lock().unwrap().writes, 0);
        let (mut elsewhere, t) = env(Mode::Live, Some(HOME));
        let from_other_guild = invoker(STAFF, OTHER, &[STAFF_ROLE]);
        let got = dispatch(
            &mut elsewhere,
            &from_other_guild,
            &Call::Status { member: MEMBER },
        );
        assert!(got.text.contains("other_guild"), "{}", got.text);
        assert_eq!(t.lock().unwrap().loads, 0);
        let (mut unset, _t) = env(Mode::Live, None);
        let got = dispatch(&mut unset, &staff(), &Call::Status { member: MEMBER });
        assert!(got.text.contains("no_home_guild"), "{}", got.text);
    }

    #[test]
    fn every_reply_kind_is_ephemeral_and_carries_no_id() {
        let calls = [
            Call::Status { member: MEMBER },
            Call::Status { member: 0 },
            Call::Strike {
                member: MEMBER,
                role: SAVED,
            },
            Call::Strike {
                member: MEMBER,
                role: 0,
            },
            Call::Clear {
                member: MEMBER,
                confirm: false,
            },
            Call::Clear {
                member: MEMBER,
                confirm: true,
            },
            Call::Clear {
                member: STRANGER,
                confirm: false,
            },
        ];
        for who in [staff(), plain(STRANGER), plain(0)] {
            for call in calls {
                let (mut e, _t) = env(Mode::Live, Some(HOME));
                let got = dispatch(&mut e, &who, &call);
                assert!(got.ephemeral, "{call:?}: {}", got.text);
                assert!(!got.text.is_empty(), "{call:?}");
                for id in [HOME, STAFF, MEMBER, STRANGER, SAVED, STAFF_ROLE] {
                    assert!(
                        !got.text.contains(&id.to_string()),
                        "{call:?}: {}",
                        got.text
                    );
                }
            }
        }
    }

    #[test]
    fn the_not_running_answer_is_sw4s_disabled_reply_and_ephemeral() {
        let got = not_running();
        assert!(got.text.contains("[disabled]"), "{}", got.text);
        assert!(got.ephemeral);
    }

    #[test]
    fn the_invoker_carries_the_caller_guild_and_roles_unchanged() {
        let who = invoker(STAFF, HOME, &[STAFF_ROLE, SAVED]);
        assert_eq!(
            who,
            Invoker {
                user: STAFF,
                guild: HOME,
                role_ids: vec![STAFF_ROLE, SAVED]
            }
        );
        assert!(invoker(STAFF, HOME, &[]).role_ids.is_empty());
    }
}

//! The Discord side of the startup preflight (W3a, `docs/verification/WIRING-C1.md`).
//!
//! `preflight.rs` decides from plain facts. This file gathers those facts: the bot's permission
//! bits in the guild and in each target channel, the facts of each configured role and the bot's
//! top role position. The gathering sits behind [`FactSource`] (permissions) and the RG2
//! `RoleSource` (roles), so it is tested offline; only [`SerenityFacts`] touches Discord and it
//! has never run against Discord. A failed lookup is an unknown fact, never a pass.

use crate::features::{gate_plan, preflight_requests, FeaturePlan, Gated};
use crate::preflight::{FeatureRequest, PermissionLookup, PreflightInput, RoleFactsEntry};
use crate::roles::runtime::{bot_top_position, role_facts, FactsOutcome, RoleSource};
use poise::serenity_prelude as s;
use std::sync::Arc;

/// Where the bot's permission bits come from.
#[s::async_trait]
pub trait FactSource: Send + Sync {
    /// The bot's guild-level permission bits, or `None` when they cannot be looked up.
    async fn guild_permissions(&self, guild_id: u64, bot_user_id: u64) -> Option<u64>;
    /// The bot's effective bits in one channel of that guild, or `None` when they cannot be
    /// looked up (including a channel that is not in the guild).
    async fn channel_permissions(
        &self,
        guild_id: u64,
        bot_user_id: u64,
        channel_id: u64,
    ) -> Option<u64>;
}

/// The facts over serenity's HTTP client: the guild, the bot's member and the channel are read
/// and the bits computed by serenity's own permission functions. An id of 0 is not an id and is
/// answered `None` before any call (serenity would panic on it).
pub struct SerenityFacts {
    pub http: Arc<s::Http>,
}

impl SerenityFacts {
    async fn guild_and_member(
        &self,
        guild_id: u64,
        bot_user_id: u64,
    ) -> Option<(s::PartialGuild, s::Member)> {
        if guild_id == 0 || bot_user_id == 0 {
            return None;
        }
        let guild_id = s::GuildId::new(guild_id);
        let guild = self.http.get_guild(guild_id).await.ok()?;
        let member = self
            .http
            .get_member(guild_id, s::UserId::new(bot_user_id))
            .await
            .ok()?;
        Some((guild, member))
    }
}

#[s::async_trait]
impl FactSource for SerenityFacts {
    async fn guild_permissions(&self, guild_id: u64, bot_user_id: u64) -> Option<u64> {
        let (guild, member) = self.guild_and_member(guild_id, bot_user_id).await?;
        Some(guild.member_permissions(&member).bits())
    }

    async fn channel_permissions(
        &self,
        guild_id: u64,
        bot_user_id: u64,
        channel_id: u64,
    ) -> Option<u64> {
        if channel_id == 0 {
            return None;
        }
        let (guild, member) = self.guild_and_member(guild_id, bot_user_id).await?;
        let channel = self
            .http
            .get_channel(s::ChannelId::new(channel_id))
            .await
            .ok()?;
        let s::Channel::Guild(channel) = channel else {
            return None;
        };
        // A channel of another guild is not a fact about this one.
        if channel.guild_id.get() != guild_id {
            return None;
        }
        Some(guild.user_permissions_in(&channel, &member).bits())
    }
}

/// Who is asked about: the home guild and the bot's own user.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub guild_id: u64,
    pub bot_user_id: u64,
}

/// Pushes `id` once.
fn push_unique(ids: &mut Vec<u64>, id: u64) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

/// Gathers the preflight input for these requests.
///
/// A permission lookup that fails is `None` for a channel (`permissions_unknown`) and zero bits
/// for the guild level (no permission: `missing_manage_roles`). A role list that cannot be read
/// makes every wanted role `Unavailable`; a bot whose own roles cannot be read has an unknown
/// top position, never zero. A zero guild or bot id asks nobody and everything stays unknown.
/// Roles are only looked up when some request names one.
pub async fn gather(
    target: Target,
    requests: Vec<FeatureRequest>,
    facts: &dyn FactSource,
    roles: &dyn RoleSource,
) -> PreflightInput {
    let mut channels: Vec<u64> = Vec::new();
    let mut wanted_roles: Vec<u64> = Vec::new();
    for request in &requests {
        for channel in &request.channels {
            push_unique(&mut channels, *channel);
        }
        for role in &request.roles {
            push_unique(&mut wanted_roles, *role);
        }
    }
    let askable = target.guild_id != 0 && target.bot_user_id != 0;

    let guild_permissions = if askable {
        facts
            .guild_permissions(target.guild_id, target.bot_user_id)
            .await
            .unwrap_or(0)
    } else {
        0
    };

    let mut channel_permissions = Vec::with_capacity(channels.len());
    for channel in channels {
        let bits = if askable {
            facts
                .channel_permissions(target.guild_id, target.bot_user_id, channel)
                .await
        } else {
            None
        };
        channel_permissions.push(match bits {
            Some(bits) => PermissionLookup::known(channel, bits),
            None => PermissionLookup::unknown(channel),
        });
    }

    let mut role_entries: Vec<RoleFactsEntry> = Vec::new();
    let mut bot_top: Option<u16> = None;
    let listed = if askable && !wanted_roles.is_empty() {
        roles.roles(target.guild_id).await.ok()
    } else {
        None
    };
    match listed {
        Some(list) => {
            bot_top = roles
                .member_roles(target.guild_id, target.bot_user_id)
                .await
                .ok()
                .map(|held| bot_top_position(&list, &held));
            for role_id in wanted_roles {
                role_entries.push(RoleFactsEntry {
                    role_id,
                    outcome: role_facts(&list, role_id),
                });
            }
        }
        None => {
            for role_id in wanted_roles {
                role_entries.push(RoleFactsEntry {
                    role_id,
                    outcome: FactsOutcome::Unavailable,
                });
            }
        }
    }

    PreflightInput {
        features: requests,
        guild_permissions,
        channel_permissions,
        category_permissions: Vec::new(),
        role_facts: role_entries,
        bot_top_position: bot_top,
    }
}

/// Runs the startup preflight for a plan: gathers the facts, evaluates, and returns the plan
/// with every feature that may not run removed, plus the lines to print.
///
/// With nothing requested (every feature off) nothing is asked of Discord, nothing is evaluated
/// and no line is returned: the plan comes back unchanged.
pub async fn startup_preflight(
    plan: &FeaturePlan,
    bot_user_id: u64,
    facts: &dyn FactSource,
    roles: &dyn RoleSource,
) -> Gated {
    let requests = preflight_requests(plan);
    if requests.is_empty() {
        return Gated {
            plan: plan.clone(),
            lines: Vec::new(),
        };
    }
    let target = Target {
        guild_id: plan.home_guild.unwrap_or(0),
        bot_user_id,
    };
    let input = gather(target, requests, facts, roles).await;
    gate_plan(plan, &input)
}

#[cfg(test)]
pub(crate) mod fakes {
    use super::*;
    use crate::roles::runtime::{RoleEntry, SourceError};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A permission source with fixed answers that counts its calls.
    #[derive(Default)]
    pub struct FakeFacts {
        pub guild: Option<u64>,
        pub channels: HashMap<u64, Option<u64>>,
        pub calls: AtomicUsize,
    }

    impl FakeFacts {
        pub fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[s::async_trait]
    impl FactSource for FakeFacts {
        async fn guild_permissions(&self, _guild_id: u64, _bot_user_id: u64) -> Option<u64> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.guild
        }

        async fn channel_permissions(
            &self,
            _guild_id: u64,
            _bot_user_id: u64,
            channel_id: u64,
        ) -> Option<u64> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.channels.get(&channel_id).copied().flatten()
        }
    }

    /// A role source with a fixed role list and fixed bot roles, which counts its calls.
    pub struct FakeRoles {
        pub roles: Result<Vec<RoleEntry>, SourceError>,
        pub bot_roles: Result<Vec<u64>, SourceError>,
        pub calls: AtomicUsize,
    }

    impl FakeRoles {
        pub fn new(roles: Vec<RoleEntry>, bot_roles: Vec<u64>) -> Self {
            FakeRoles {
                roles: Ok(roles),
                bot_roles: Ok(bot_roles),
                calls: AtomicUsize::new(0),
            }
        }

        pub fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[s::async_trait]
    impl RoleSource for FakeRoles {
        async fn roles(&self, _guild_id: u64) -> Result<Vec<RoleEntry>, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.roles.clone()
        }

        async fn member_roles(
            &self,
            _guild_id: u64,
            _member_id: u64,
        ) -> Result<Vec<u64>, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.bot_roles.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::{FakeFacts, FakeRoles};
    use super::*;
    use crate::preflight::{
        Feature, PermissionLookup, RoleFactsEntry, ADMINISTRATOR, MANAGE_ROLES, SEND_MESSAGES,
    };
    use crate::roles::eligibility::RoleFacts;
    use crate::roles::runtime::{FactsOutcome, RoleEntry, SourceError};
    use crate::settings::Mode;

    // Synthetic ids. They are obviously made up and are not real Discord ids.
    const GUILD: u64 = 790_001;
    const BOT: u64 = 790_002;
    const CH_A: u64 = 790_011;
    const CH_B: u64 = 790_012;
    const ROLE_LOW: u64 = 790_021;
    const ROLE_GONE: u64 = 790_029;
    const BOT_ROLE: u64 = 790_030;

    fn target() -> Target {
        Target {
            guild_id: GUILD,
            bot_user_id: BOT,
        }
    }

    fn role(id: u64, position: u16) -> RoleEntry {
        RoleEntry {
            id,
            position,
            managed: false,
            permissions: 0,
        }
    }

    fn roles() -> FakeRoles {
        FakeRoles::new(vec![role(BOT_ROLE, 20), role(ROLE_LOW, 5)], vec![BOT_ROLE])
    }

    fn request() -> FeatureRequest {
        let mut request = FeatureRequest::new(Feature::Welcome, Mode::Live);
        request.channels = vec![CH_A, CH_B, CH_A];
        request.roles = vec![ROLE_LOW, ROLE_GONE];
        request
    }

    #[tokio::test]
    async fn the_input_carries_the_requests_permissions_roles_and_bot_position() {
        let facts = FakeFacts {
            guild: Some(MANAGE_ROLES),
            channels: [(CH_A, Some(SEND_MESSAGES)), (CH_B, None)]
                .into_iter()
                .collect(),
            ..FakeFacts::default()
        };
        let input = gather(target(), vec![request()], &facts, &roles()).await;
        assert_eq!(input.features, vec![request()]);
        assert_eq!(input.guild_permissions, MANAGE_ROLES);
        assert_eq!(
            input.channel_permissions,
            vec![
                PermissionLookup::known(CH_A, SEND_MESSAGES),
                PermissionLookup::unknown(CH_B)
            ],
            "one entry per distinct channel, a failed lookup is unknown"
        );
        assert_eq!(input.bot_top_position, Some(20));
        assert_eq!(
            input.role_facts,
            vec![
                RoleFactsEntry::found(
                    ROLE_LOW,
                    RoleFacts {
                        id: ROLE_LOW,
                        position: 5,
                        managed: false,
                        permissions: 0,
                        exists: true
                    }
                ),
                RoleFactsEntry::missing(ROLE_GONE),
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_guild_lookup_holds_no_permission_at_all() {
        let facts = FakeFacts::default();
        let input = gather(target(), vec![request()], &facts, &roles()).await;
        assert_eq!(input.guild_permissions, 0, "unknown is never a pass");
        assert_eq!(ADMINISTRATOR & input.guild_permissions, 0);
    }

    #[tokio::test]
    async fn an_unavailable_role_list_makes_every_role_unavailable_and_the_bot_top_unknown() {
        let mut source = roles();
        source.roles = Err(SourceError::Unavailable);
        let input = gather(target(), vec![request()], &FakeFacts::default(), &source).await;
        assert_eq!(input.bot_top_position, None);
        assert_eq!(
            input.role_facts,
            vec![
                RoleFactsEntry {
                    role_id: ROLE_LOW,
                    outcome: FactsOutcome::Unavailable
                },
                RoleFactsEntry {
                    role_id: ROLE_GONE,
                    outcome: FactsOutcome::Unavailable
                },
            ]
        );
    }

    #[tokio::test]
    async fn the_bot_roles_failing_leaves_the_bot_top_unknown() {
        let mut source = roles();
        source.bot_roles = Err(SourceError::Unavailable);
        let input = gather(target(), vec![request()], &FakeFacts::default(), &source).await;
        assert_eq!(input.bot_top_position, None, "not zero, not a guess");
    }

    #[tokio::test]
    async fn a_zero_guild_asks_nobody_and_everything_is_unknown() {
        let facts = FakeFacts {
            guild: Some(u64::MAX),
            ..FakeFacts::default()
        };
        let source = roles();
        let zero = Target {
            guild_id: 0,
            bot_user_id: BOT,
        };
        let input = gather(zero, vec![request()], &facts, &source).await;
        assert_eq!(facts.calls() + source.calls(), 0);
        assert_eq!(input.guild_permissions, 0);
        assert!(input
            .channel_permissions
            .iter()
            .all(|lookup| lookup.permissions.is_none()));
        assert_eq!(input.bot_top_position, None);
    }

    #[tokio::test]
    async fn serenity_facts_answer_none_for_a_zero_id_without_a_call() {
        let facts = SerenityFacts {
            http: Arc::new(s::Http::new("synthetic-token")),
        };
        assert_eq!(facts.guild_permissions(0, BOT).await, None);
        assert_eq!(facts.guild_permissions(GUILD, 0).await, None);
        assert_eq!(facts.channel_permissions(GUILD, BOT, 0).await, None);
        assert_eq!(facts.channel_permissions(0, BOT, CH_A).await, None);
    }
}

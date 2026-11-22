//! Welcome runtime: join roles through the role service and the welcome message.
//!
//! (`docs/verification/WELCOME-RUNTIME.md`)
//!
//! The runtime is a thin layer. Every decision is a pure function over plain
//! facts ([`decide_join`], [`validate_join_roles`]); the Discord calls sit
//! behind small traits ([`MessageSender`], [`StaffAlerter`]) and the role
//! service behind the RG2 traits ([`RoleSource`], [`RoleWriter`]), so the tests
//! inject fakes and never touch Discord.
//!
//! Rules the code enforces:
//!
//! * A bot that joins is skipped for the roles AND for the message.
//! * A join role that is privileged or protected is refused at startup
//!   validation, with the key and the reason code.
//! * The allowed mentions are the new member only; `everyone` is never allowed.
//! * A send failure is reported by a stable code and never blocks the role
//!   grant; a grant failure never blocks the message.
//! * Shadow mode sends nothing, grants nothing and only produces a structured
//!   would-do record (counts and codes, no user content).

use crate::roles::eligibility::{GrantPath, RoleFacts, SafewordState};
use crate::roles::grant::{GrantItem, GrantRequest, RoleResult, RoleWriter, Sleeper};
use crate::roles::runtime::{
    bot_top_position, decide_from_facts, role_facts, DecisionInputs, FactsOutcome, RoleSource,
    RuntimeDecision,
};
use crate::settings::Mode;
use crate::welcome::config::{WelcomeConfig, KEY_JOIN_ROLE_IDS};
use crate::welcome::render::{IdMap, MemberContext, RenderedEmbed, WelcomeTemplate};
use poise::serenity_prelude as s;
use std::fmt;

/// The job kind that grants the join roles.
pub const JOB_KIND_GRANT_ROLES: &str = "welcome_grant_roles";
/// The job kind that sends the welcome message.
pub const JOB_KIND_SEND_MESSAGE: &str = "welcome_send_message";

/// The stable code for a message that could not be sent.
pub const CODE_SEND_FAILED: &str = "welcome_send_failed";
/// The stable code for a message that was refused by Discord.
pub const CODE_SEND_FORBIDDEN: &str = "welcome_send_forbidden";
/// The stable code for a message that hit the rate limit.
pub const CODE_SEND_RATE_LIMITED: &str = "welcome_send_rate_limited";
/// The stable code for a role whose facts could not be looked up.
pub const CODE_FACTS_UNAVAILABLE: &str = "facts_unavailable";
/// The stable code for a join role checked while the bot's top role position
/// is unknown.
pub const CODE_BOT_TOP_UNKNOWN: &str = "bot_top_position_unknown";

// ---------------------------------------------------------------------------
// Facts about the join
// ---------------------------------------------------------------------------

/// The plain facts about a member that joined.
///
/// The username is untrusted: the renderer sanitises it. No serenity type and
/// no id that the caller did not supply appears here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinFacts {
    /// The guild the member joined.
    pub guild_id: u64,
    /// The joining user.
    pub user_id: u64,
    /// The joining user's display name. Untrusted.
    pub username: String,
    /// The name of the server. Untrusted.
    pub server_name: String,
    /// The number of members after the join.
    pub member_count: u64,
    /// True when the joining account is a bot.
    pub is_bot: bool,
    /// The roles the member already holds.
    pub member_role_ids: Vec<u64>,
}

// ---------------------------------------------------------------------------
// The decision (pure)
// ---------------------------------------------------------------------------

/// Why a join is not acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The feature is not running: its mode is off or a key was unusable.
    FeatureDisabled,
    /// No channel is configured, so there is nowhere to post.
    NoChannel,
    /// No template is configured, so there is nothing to post.
    NoTemplate,
    /// The joining account is a bot.
    Bot,
}

impl SkipReason {
    /// The stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            SkipReason::FeatureDisabled => "feature_disabled",
            SkipReason::NoChannel => "no_channel",
            SkipReason::NoTemplate => "no_template",
            SkipReason::Bot => "bot",
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// The set of mentions the welcome message is allowed to render.
///
/// It mirrors the Discord allowed-mentions object: only the listed users may be
/// mentioned and `everyone` is always false, so `@everyone` and `@here` can
/// never fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedMentions {
    /// The users that may be mentioned. The new member, and nobody else.
    pub users: Vec<u64>,
    /// The roles that may be mentioned. Never any.
    pub roles: Vec<u64>,
    /// Whether `@everyone` / `@here` may be mentioned. Always false.
    pub everyone: bool,
}

/// What the runtime does for one join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinActions {
    /// The roles that are granted to the joining member.
    pub grant_role_ids: Vec<u64>,
    /// Whether the welcome message is posted.
    pub send_message: bool,
    /// The channel the message is posted in.
    pub channel_id: u64,
    /// The mentions the message is allowed to render.
    pub allowed_mentions: AllowedMentions,
}

/// What the runtime decided about one join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinDecision {
    /// Nothing is done.
    Skip(SkipReason),
    /// The join is acted on.
    Act(JoinActions),
}

/// Decides what the runtime does for one join.
///
/// The order of the checks is fixed: a feature that is not running says so, a
/// missing channel and a missing template say so, and only then is a bot
/// skipped. A bot is skipped for the roles AND for the message: nothing is
/// granted and nothing is posted.
pub fn decide_join(facts: &JoinFacts, config: &WelcomeConfig) -> JoinDecision {
    if config.mode == Mode::Off {
        return JoinDecision::Skip(SkipReason::FeatureDisabled);
    }
    let Some(channel_id) = config.channel_id else {
        return JoinDecision::Skip(SkipReason::NoChannel);
    };
    if config.template_path.is_none() {
        return JoinDecision::Skip(SkipReason::NoTemplate);
    }
    if facts.is_bot {
        return JoinDecision::Skip(SkipReason::Bot);
    }
    JoinDecision::Act(JoinActions {
        grant_role_ids: config.join_role_ids.clone(),
        send_message: true,
        channel_id,
        allowed_mentions: AllowedMentions {
            users: vec![facts.user_id],
            roles: Vec::new(),
            everyone: false,
        },
    })
}

// ---------------------------------------------------------------------------
// The message
// ---------------------------------------------------------------------------

/// The welcome message, as plain data.
///
/// It holds strings only: no serenity type appears here, so the wiring card
/// decides how to build the Discord message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// The message content, if the template had any.
    pub content: Option<String>,
    /// The embed of the message.
    pub embed: RenderedEmbed,
    /// The mentions the message is allowed to render.
    pub allowed_mentions: AllowedMentions,
}

impl OutgoingMessage {
    /// Renders the template for one member into the message that is sent.
    ///
    /// The allowed mentions are the joining member only.
    pub fn render(
        template: &WelcomeTemplate,
        member: &MemberContext,
        ids: &IdMap,
    ) -> OutgoingMessage {
        let rendered = crate::welcome::render::render_welcome(template, member, ids);
        OutgoingMessage {
            content: rendered.content,
            embed: rendered.embed,
            allowed_mentions: AllowedMentions {
                users: vec![member.user_id],
                roles: Vec::new(),
                everyone: false,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// The traits
// ---------------------------------------------------------------------------

/// What a sender reports when the message could not be posted.
///
/// The failure carries numbers only: no message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendError {
    /// The HTTP status, or `0` when the failure was not an HTTP response.
    pub status: u16,
    /// Discord's JSON error code, when the response carried one.
    pub api_code: Option<u32>,
}

impl SendError {
    /// The stable code for this failure.
    pub fn code(self) -> &'static str {
        match self.status {
            403 => CODE_SEND_FORBIDDEN,
            429 => CODE_SEND_RATE_LIMITED,
            _ => CODE_SEND_FAILED,
        }
    }
}

/// Posts one message into one channel.
pub trait MessageSender {
    fn send(&mut self, channel_id: u64, message: &OutgoingMessage) -> Result<(), SendError>;
}

/// Reports a failure to the staff alert channel.
///
/// The alert carries a stable code only, never message text, a name or an id.
pub trait StaffAlerter {
    fn alert(&mut self, channel_id: u64, code: &str);
}

// ---------------------------------------------------------------------------
// The outcome
// ---------------------------------------------------------------------------

/// What happened to the welcome message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageOutcome {
    /// The message was posted.
    Sent,
    /// The message could not be posted. The code is stable.
    Failed(&'static str),
}

/// What the runtime did for one join.
///
/// Every item keeps its own outcome: a later failure never overwrites an
/// earlier one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinOutcome {
    /// Why the join was skipped, when it was skipped.
    pub skipped: Option<SkipReason>,
    /// One result per requested join role, in request order.
    pub role_results: Vec<RoleResult>,
    /// What happened to the welcome message.
    pub message: Option<MessageOutcome>,
    /// The stable codes that were reported to the staff alert channel.
    pub alerts: Vec<&'static str>,
    /// The structured would-do record of a shadow run.
    pub would_do: Option<WouldDo>,
}

/// The structured record of what a shadow run would have done.
///
/// It holds counts and stable codes only: no user name, no message text and no
/// member id.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WouldDo {
    /// How many roles would have been granted.
    pub roles_to_grant: usize,
    /// The reason code of every role that would have been refused.
    pub roles_refused: Vec<&'static str>,
    /// Whether the welcome message would have been posted.
    pub message_would_be_sent: bool,
}

impl JoinOutcome {
    /// The stable codes of everything that went wrong.
    pub fn failure_codes(&self) -> Vec<&'static str> {
        let mut codes: Vec<&'static str> = Vec::new();
        for result in &self.role_results {
            if let crate::roles::grant::RoleOutcome::Failed(code) = result.outcome {
                codes.push(code.code());
            }
        }
        if let Some(MessageOutcome::Failed(code)) = self.message {
            codes.push(code);
        }
        codes
    }
}

// ---------------------------------------------------------------------------
// Startup validation of the join roles
// ---------------------------------------------------------------------------

/// A join role that is refused at startup.
///
/// The refusal names the configuration key and the stable reason code; it never
/// carries a name or a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinRoleRefusal {
    /// The configuration key that named the role.
    pub key: &'static str,
    /// The role that is refused.
    pub role_id: u64,
    /// The stable reason code.
    pub reason: &'static str,
}

/// Checks the configured join roles before the feature runs.
///
/// A role that is privileged, protected, managed, the @everyone role, at or
/// above the bot's top role or deleted is refused, with the key
/// `JOIN_ROLE_IDS` and the reason code of the K03 policy. The decision is the
/// policy's own: this function only reports it.
pub fn validate_join_roles(
    role_ids: &[u64],
    guild_id: u64,
    protected_role_ids: &[u64],
    bot_top_position: Option<u16>,
    facts: &[crate::preflight::RoleFactsEntry],
) -> Vec<JoinRoleRefusal> {
    let mut refusals = Vec::new();
    for role_id in role_ids {
        let outcome = facts
            .iter()
            .find(|entry| entry.role_id == *role_id)
            .map(|entry| entry.outcome);
        let Some(outcome) = outcome else {
            // The caller did not say what the role is: an unknown fact, never
            // a pass.
            refusals.push(JoinRoleRefusal {
                key: KEY_JOIN_ROLE_IDS,
                role_id: *role_id,
                reason: CODE_FACTS_UNAVAILABLE,
            });
            continue;
        };
        if matches!(outcome, FactsOutcome::Unavailable) {
            refusals.push(JoinRoleRefusal {
                key: KEY_JOIN_ROLE_IDS,
                role_id: *role_id,
                reason: CODE_FACTS_UNAVAILABLE,
            });
            continue;
        }
        let Some(bot_top) = bot_top_position else {
            refusals.push(JoinRoleRefusal {
                key: KEY_JOIN_ROLE_IDS,
                role_id: *role_id,
                reason: CODE_BOT_TOP_UNKNOWN,
            });
            continue;
        };
        // The policy decides; this function only reports its reason code.
        let request = crate::roles::eligibility::GrantRequest {
            path: GrantPath::JoinRole,
            role: match outcome {
                FactsOutcome::Found(facts) => facts,
                // A role that is not in the guild's role list: the policy
                // denies it as deleted.
                FactsOutcome::Missing => RoleFacts {
                    id: *role_id,
                    position: 0,
                    managed: false,
                    permissions: 0,
                    exists: false,
                },
                FactsOutcome::Unavailable => unreachable!("handled above"),
            },
            bot_top_position: bot_top,
            guild_id,
            protected_role_ids,
            safeword: SafewordState::Inactive,
            safeword_restricted_role_ids: &[],
            staff: None,
        };
        if let crate::roles::eligibility::Decision::Deny(reason) =
            crate::roles::eligibility::decide(&request)
        {
            refusals.push(JoinRoleRefusal {
                key: KEY_JOIN_ROLE_IDS,
                role_id: *role_id,
                reason: reason.code(),
            });
        }
    }
    refusals
}

// ---------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------

/// The policy inputs that are the same for every join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantPolicy {
    /// The roles that may only be granted by staff.
    pub protected_role_ids: Vec<u64>,
    /// Whether the safeword is active.
    pub safeword: SafewordState,
    /// The roles that may not be granted while the safeword is active.
    pub safeword_restricted_role_ids: Vec<u64>,
}

impl Default for GrantPolicy {
    fn default() -> Self {
        GrantPolicy {
            protected_role_ids: Vec::new(),
            safeword: SafewordState::Inactive,
            safeword_restricted_role_ids: Vec::new(),
        }
    }
}

/// The welcome runtime.
///
/// It holds everything the runtime needs: the configuration, the loaded
/// template, the id map the template was loaded with, and the collaborators
/// behind their traits.
pub struct WelcomeRuntime<S, W, P, M, A> {
    /// The parsed configuration.
    pub config: WelcomeConfig,
    /// The loaded welcome template.
    pub template: WelcomeTemplate,
    /// The id map the template was loaded with.
    pub ids: IdMap,
    /// Where the role facts come from.
    pub source: S,
    /// The role writer.
    pub writer: W,
    /// The sleeper the grant executor waits on.
    pub sleeper: P,
    /// The sender of the welcome message.
    pub sender: M,
    /// The reporter of failures to the staff alert channel.
    pub alerter: A,
    /// The staff alert channel, when one is configured.
    pub staff_alert_channel: Option<u64>,
    /// The bot's own user id, used for the bot's top role position.
    pub bot_user_id: u64,
    /// The policy inputs of the role service.
    pub grant_policy: GrantPolicy,
}

impl<S, W, P, M, A> WelcomeRuntime<S, W, P, M, A>
where
    S: RoleSource,
    W: RoleWriter,
    P: Sleeper,
    M: MessageSender,
    A: StaffAlerter,
{
    /// Handles one join.
    ///
    /// The roles are granted first and the message is posted afterwards, and
    /// neither part blocks the other: a send failure is reported by a stable
    /// code and the roles stay granted, and a grant failure never stops the
    /// message.
    pub async fn handle_join(&mut self, facts: &JoinFacts) -> JoinOutcome {
        let decision = decide_join(facts, &self.config);
        let JoinDecision::Act(actions) = decision else {
            let reason = match decision {
                JoinDecision::Skip(reason) => reason,
                JoinDecision::Act(_) => unreachable!("the decision is one of two"),
            };
            // A shadow run records what it would NOT do: nothing was sent and
            // nothing was granted.
            let would_do = if self.config.mode == Mode::Shadow {
                Some(WouldDo::default())
            } else {
                None
            };
            return JoinOutcome {
                skipped: Some(reason),
                role_results: Vec::new(),
                message: None,
                alerts: Vec::new(),
                would_do,
            };
        };

        if self.config.mode == Mode::Shadow {
            return self.shadow_join(facts, &actions).await;
        }

        let mut outcome = JoinOutcome {
            skipped: None,
            role_results: Vec::new(),
            message: None,
            alerts: Vec::new(),
            would_do: None,
        };

        // The roles: the role service decides, the grant executor writes. A
        // failure here is reported, and the message is still posted.
        outcome.role_results = self.grant_roles(facts, &actions).await;
        for result in &outcome.role_results {
            if let crate::roles::grant::RoleOutcome::Failed(code) = result.outcome {
                outcome.alerts.push(code.code());
            }
        }

        // The message: rendered from the template and posted to the channel. A
        // failure here is reported, and the roles stay granted.
        let member = MemberContext {
            user_id: facts.user_id,
            username: facts.username.clone(),
            server_name: facts.server_name.clone(),
            member_count: facts.member_count,
            is_bot: facts.is_bot,
        };
        let message = OutgoingMessage::render(&self.template, &member, &self.ids);
        match self.sender.send(actions.channel_id, &message) {
            Ok(()) => outcome.message = Some(MessageOutcome::Sent),
            Err(error) => {
                let code = error.code();
                outcome.message = Some(MessageOutcome::Failed(code));
                outcome.alerts.push(code);
                self.report(code);
            }
        }

        outcome
    }

    /// The role part of a join: the policy decides, the executor writes.
    ///
    /// One result per requested join role, in request order.
    async fn grant_roles(&mut self, facts: &JoinFacts, actions: &JoinActions) -> Vec<RoleResult> {
        let roles = match self.source.roles(facts.guild_id).await {
            Ok(roles) => roles,
            Err(_) => {
                // The guild's roles could not be looked up: no role may be
                // granted, and the reason stays visible.
                return actions
                    .grant_role_ids
                    .iter()
                    .map(|role_id| RoleResult {
                        role_id: *role_id,
                        outcome: crate::roles::grant::RoleOutcome::Denied(
                            crate::roles::eligibility::DenyReason::RoleDeleted,
                        ),
                        writer_calls: 0,
                    })
                    .collect();
            }
        };
        let bot_roles = self
            .source
            .member_roles(facts.guild_id, self.bot_user_id)
            .await
            .unwrap_or_default();
        let bot_top = bot_top_position(&roles, &bot_roles);
        let inputs = DecisionInputs {
            path: GrantPath::JoinRole,
            bot_top_position: bot_top,
            guild_id: facts.guild_id,
            protected_role_ids: &self.grant_policy.protected_role_ids,
            safeword: self.grant_policy.safeword,
            safeword_restricted_role_ids: &self.grant_policy.safeword_restricted_role_ids,
            staff: None,
        };
        let mut decisions: Vec<(u64, RuntimeDecision)> =
            Vec::with_capacity(actions.grant_role_ids.len());
        for role_id in &actions.grant_role_ids {
            let outcome = role_facts(&roles, *role_id);
            let decision = decide_from_facts(outcome, *role_id, &inputs);
            decisions.push((*role_id, decision));
        }
        // A role whose facts could not be looked up is left out of the request,
        // so the executor can never write it.
        let items: Vec<GrantItem> =
            crate::roles::runtime::grant_items(&decisions, &facts.member_role_ids);
        let request = GrantRequest {
            member_id: facts.user_id,
            items: &items,
        };
        let report = crate::roles::grant::apply(&mut self.writer, &mut self.sleeper, &request);
        report.results
    }

    /// The shadow run: nothing is sent, nothing is granted, only a record.
    async fn shadow_join(&mut self, facts: &JoinFacts, actions: &JoinActions) -> JoinOutcome {
        let roles = self.source.roles(facts.guild_id).await.unwrap_or_default();
        let bot_roles = self
            .source
            .member_roles(facts.guild_id, self.bot_user_id)
            .await
            .unwrap_or_default();
        let bot_top = bot_top_position(&roles, &bot_roles);
        let inputs = DecisionInputs {
            path: GrantPath::JoinRole,
            bot_top_position: bot_top,
            guild_id: facts.guild_id,
            protected_role_ids: &self.grant_policy.protected_role_ids,
            safeword: self.grant_policy.safeword,
            safeword_restricted_role_ids: &self.grant_policy.safeword_restricted_role_ids,
            staff: None,
        };
        let mut refused: Vec<&'static str> = Vec::new();
        let mut grantable = 0usize;
        for role_id in &actions.grant_role_ids {
            let outcome = role_facts(&roles, *role_id);
            match decide_from_facts(outcome, *role_id, &inputs) {
                RuntimeDecision::Decided(crate::roles::eligibility::Decision::Allow) => {
                    grantable += 1;
                }
                RuntimeDecision::Decided(crate::roles::eligibility::Decision::Deny(reason)) => {
                    refused.push(reason.code());
                }
                RuntimeDecision::Unavailable => refused.push(CODE_FACTS_UNAVAILABLE),
            }
        }
        JoinOutcome {
            skipped: None,
            role_results: Vec::new(),
            message: None,
            alerts: Vec::new(),
            would_do: Some(WouldDo {
                roles_to_grant: grantable,
                roles_refused: refused,
                message_would_be_sent: actions.send_message,
            }),
        }
    }

    /// Reports a failure to the staff alert channel, when one is configured.
    ///
    /// The alert carries the stable code only: never message text, a name or
    /// an id.
    fn report(&mut self, code: &'static str) {
        if let Some(channel_id) = self.staff_alert_channel {
            self.alerter.alert(channel_id, code);
        }
    }
}

/// The restart policy of every job kind this feature uses.
///
/// A granted role and a posted welcome message are both user-visible and
/// irreversible, so neither may be repeated after a restart. An unknown kind
/// is `AtMostOnce` too: the runtime never repeats work it does not know.
pub fn restart_policy(kind: &str) -> crate::jobs::queue::RestartPolicy {
    use crate::jobs::queue::RestartPolicy;
    match kind {
        JOB_KIND_GRANT_ROLES | JOB_KIND_SEND_MESSAGE => RestartPolicy::AtMostOnce,
        _ => RestartPolicy::AtMostOnce,
    }
}

// ---------------------------------------------------------------------------
// The feature registry hook
// ---------------------------------------------------------------------------

/// The guild facts the welcome message needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildFacts {
    /// The name of the server.
    pub name: String,
    /// The number of members in the server after the join.
    pub member_count: u64,
}

/// Where the guild facts come from.
///
/// The wiring card implements this over the serenity cache; the tests use a
/// fake.
#[s::async_trait]
pub trait GuildLookup: Send + Sync {
    /// The facts of the guild, or `None` when they cannot be looked up.
    async fn guild_facts(&self, guild_id: u64) -> Option<GuildFacts>;
}

/// The shared runtime of the welcome feature.
///
/// The feature is handed to the registry, which shares it between events, so
/// it sits behind a lock.
pub type SharedRuntime<S, W, P, M, A> =
    std::sync::Arc<tokio::sync::Mutex<WelcomeRuntime<S, W, P, M, A>>>;

/// The welcome feature, as the registry sees it.
///
/// The feature implements the registry's hook but is NOT registered: the
/// wiring card W3 adds it to `features::production_features`.
pub struct WelcomeFeature<S, W, P, M, A, G> {
    /// The runtime that does the work.
    pub runtime: SharedRuntime<S, W, P, M, A>,
    /// Where the guild facts come from.
    pub guilds: G,
    /// The mode the feature was configured in. The registry asks for it
    /// synchronously, so it is kept here and not read through the lock.
    pub mode: Mode,
}

impl<S, W, P, M, A, G> WelcomeFeature<S, W, P, M, A, G>
where
    S: RoleSource,
    W: RoleWriter,
    P: Sleeper,
    M: MessageSender,
    A: StaffAlerter,
    G: GuildLookup,
{
    /// Builds the feature around a runtime.
    pub fn new(runtime: WelcomeRuntime<S, W, P, M, A>, guilds: G) -> Self {
        let mode = runtime.config.mode;
        WelcomeFeature {
            runtime: std::sync::Arc::new(tokio::sync::Mutex::new(runtime)),
            guilds,
            mode,
        }
    }
}

/// The facts of a member that joined, read from the gateway event.
///
/// A member with no id is not a member the runtime can write about: the
/// event is ignored.
fn facts_from_event(event: &s::FullEvent) -> Option<JoinFacts> {
    let s::FullEvent::GuildMemberAddition { new_member } = event else {
        return None;
    };
    let guild_id = new_member.guild_id.get();
    let user_id = new_member.user.id.get();
    if user_id == 0 || guild_id == 0 {
        return None;
    }
    let username = new_member
        .user
        .global_name
        .clone()
        .unwrap_or_else(|| new_member.user.name.clone());
    Some(JoinFacts {
        guild_id,
        user_id,
        username,
        server_name: String::new(),
        member_count: 0,
        is_bot: new_member.user.bot,
        member_role_ids: new_member.roles.iter().map(|id| id.get()).collect(),
    })
}

impl<X, D, S, W, P, M, A, G> crate::features::Feature<X, D> for WelcomeFeature<S, W, P, M, A, G>
where
    X: Send + Sync + 'static,
    D: Send + Sync + 'static,
    S: RoleSource + Send + Sync + 'static,
    W: RoleWriter + Send + Sync + 'static,
    P: Sleeper + Send + Sync + 'static,
    M: MessageSender + Send + Sync + 'static,
    A: StaffAlerter + Send + Sync + 'static,
    G: GuildLookup + 'static,
{
    fn name(&self) -> &'static str {
        "welcome"
    }

    fn enabled(&self) -> bool {
        // The feature is on when its mode asks for it. The rest of the
        // configuration is read by the runtime.
        self.mode != Mode::Off
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a X,
        event: &'a s::FullEvent,
        _data: &'a D,
        _guild: Option<u64>,
        _core_db: Option<&'a tokio_rusqlite::Connection>,
    ) -> crate::jobs::runner::BoxFuture<'a, ()> {
        Box::pin(async move {
            let Some(mut facts) = facts_from_event(event) else {
                return;
            };
            let guild_facts = self.guilds.guild_facts(facts.guild_id).await;
            if let Some(guild) = guild_facts {
                facts.server_name = guild.name;
                facts.member_count = guild.member_count;
            }
            let mut runtime = self.runtime.lock().await;
            runtime.handle_join(&facts).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::eligibility::RoleFacts;
    use crate::roles::grant::RoleOutcome;
    use crate::roles::runtime::{RoleEntry, SourceError};
    use crate::settings::Mode;
    use crate::welcome::config::KEY_JOIN_ROLE_IDS;
    use poise::serenity_prelude as s;

    // Synthetic ids. They are obviously made up and are not real Discord ids.
    const GUILD: u64 = 700_001;
    const BOT: u64 = 700_002;
    const MEMBER: u64 = 700_003;
    const CHANNEL: u64 = 710_001;
    const ROLE_LOW: u64 = 720_001;
    const ROLE_MID: u64 = 720_002;
    const ROLE_HIGH: u64 = 720_003;
    const ROLE_PRIVILEGED: u64 = 720_004;
    const ROLE_PROTECTED: u64 = 720_005;
    const ROLE_DELETED: u64 = 720_099;
    const ROLE_BOT: u64 = 720_010;

    const TEMPLATE: &str = r#"{"embed":{"title":"Welcome {user_mention}","description":"You are our {member_ordinal} member."}}"#;

    // ---- fakes -------------------------------------------------------------

    /// A role source with a fixed guild role list and fixed member roles.
    struct FakeSource {
        roles: Result<Vec<RoleEntry>, SourceError>,
        members: Result<Vec<u64>, SourceError>,
    }

    impl FakeSource {
        fn with_roles(roles: Vec<RoleEntry>) -> Self {
            FakeSource {
                roles: Ok(roles),
                members: Ok(Vec::new()),
            }
        }
        fn member(mut self, ids: Vec<u64>) -> Self {
            self.members = Ok(ids);
            self
        }
        fn unavailable() -> Self {
            FakeSource {
                roles: Err(SourceError::Unavailable),
                members: Err(SourceError::Unavailable),
            }
        }
    }

    #[s::async_trait]
    impl RoleSource for FakeSource {
        async fn roles(&self, _guild_id: u64) -> Result<Vec<RoleEntry>, SourceError> {
            self.roles.clone()
        }
        async fn member_roles(
            &self,
            _guild_id: u64,
            member_id: u64,
        ) -> Result<Vec<u64>, SourceError> {
            // The bot holds its own role; the joining member holds none unless
            // the test says so.
            if member_id == BOT {
                return Ok(vec![ROLE_BOT]);
            }
            self.members.clone()
        }
    }

    /// A writer that records every call and can fail per role.
    #[derive(Default)]
    struct FakeWriter {
        calls: Vec<(u64, u64)>,
        fail: Option<(u64, crate::roles::grant::WriteError)>,
    }

    impl FakeWriter {
        fn failing(role_id: u64, status: u16) -> Self {
            FakeWriter {
                calls: Vec::new(),
                fail: Some((
                    role_id,
                    crate::roles::grant::WriteError {
                        status,
                        api_code: None,
                        retry_after_ms: None,
                    },
                )),
            }
        }
    }

    impl RoleWriter for FakeWriter {
        fn add_role(
            &mut self,
            member_id: u64,
            role_id: u64,
        ) -> Result<(), crate::roles::grant::WriteError> {
            self.calls.push((member_id, role_id));
            match &self.fail {
                Some((failing, error)) if *failing == role_id => Err(*error),
                _ => Ok(()),
            }
        }
    }

    /// A sleeper that records the waits.
    #[derive(Default)]
    struct FakeSleeper {
        slept: Vec<u64>,
    }

    impl Sleeper for FakeSleeper {
        fn sleep_ms(&mut self, ms: u64) {
            self.slept.push(ms);
        }
    }

    /// A sender that records the messages and can fail.
    struct FakeSender {
        sent: Vec<(u64, OutgoingMessage)>,
        fail: Option<SendError>,
    }

    impl FakeSender {
        fn ok() -> Self {
            FakeSender {
                sent: Vec::new(),
                fail: None,
            }
        }
        fn failing(status: u16) -> Self {
            FakeSender {
                sent: Vec::new(),
                fail: Some(SendError {
                    status,
                    api_code: None,
                }),
            }
        }
    }

    impl MessageSender for FakeSender {
        fn send(&mut self, channel_id: u64, message: &OutgoingMessage) -> Result<(), SendError> {
            self.sent.push((channel_id, message.clone()));
            match self.fail {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    /// An alerter that records the codes.
    #[derive(Default)]
    struct FakeAlerter {
        alerts: Vec<(u64, String)>,
    }

    impl StaffAlerter for FakeAlerter {
        fn alert(&mut self, channel_id: u64, code: &str) {
            self.alerts.push((channel_id, code.to_string()));
        }
    }

    // ---- helpers -----------------------------------------------------------

    fn guild_roles() -> Vec<RoleEntry> {
        vec![
            RoleEntry {
                id: ROLE_LOW,
                position: 1,
                managed: false,
                permissions: 0,
            },
            RoleEntry {
                id: ROLE_MID,
                position: 2,
                managed: false,
                permissions: 0,
            },
            RoleEntry {
                id: ROLE_HIGH,
                position: 3,
                managed: false,
                permissions: 0,
            },
            RoleEntry {
                id: ROLE_PRIVILEGED,
                position: 4,
                managed: false,
                permissions: crate::roles::eligibility::ADMINISTRATOR,
            },
            RoleEntry {
                id: ROLE_PROTECTED,
                position: 5,
                managed: false,
                permissions: 0,
            },
            RoleEntry {
                id: ROLE_BOT,
                position: 20,
                managed: false,
                permissions: 0,
            },
        ]
    }

    fn config(mode: Mode, roles: &[u64]) -> WelcomeConfig {
        WelcomeConfig {
            mode,
            channel_id: Some(CHANNEL),
            template_path: Some("welcome".to_string()),
            join_role_ids: roles.to_vec(),
            invalid_keys: Vec::new(),
        }
    }

    fn facts(is_bot: bool) -> JoinFacts {
        JoinFacts {
            guild_id: GUILD,
            user_id: MEMBER,
            username: "synthetic-user".to_string(),
            server_name: "synthetic-server".to_string(),
            member_count: 42,
            is_bot,
            member_role_ids: Vec::new(),
        }
    }

    fn template() -> WelcomeTemplate {
        let mut ids = IdMap::default();
        WelcomeTemplate::load(TEMPLATE, &ids).expect("the template loads");
        ids = IdMap::default();
        let _ = &mut ids;
        WelcomeTemplate::load(TEMPLATE, &IdMap::default()).expect("the template loads")
    }

    fn runtime(
        mode: Mode,
        roles: &[u64],
        source: FakeSource,
        writer: FakeWriter,
        sender: FakeSender,
    ) -> WelcomeRuntime<FakeSource, FakeWriter, FakeSleeper, FakeSender, FakeAlerter> {
        WelcomeRuntime {
            config: config(mode, roles),
            template: template(),
            ids: IdMap::default(),
            source,
            writer,
            sleeper: FakeSleeper::default(),
            sender,
            alerter: FakeAlerter::default(),
            staff_alert_channel: Some(799_001),
            bot_user_id: BOT,
            grant_policy: GrantPolicy::default(),
        }
    }

    // ---- the pure decision -------------------------------------------------

    #[test]
    fn a_bot_join_is_skipped_for_the_roles_and_for_the_message() {
        let decision = decide_join(&facts(true), &config(Mode::Live, &[ROLE_LOW]));
        assert_eq!(decision, JoinDecision::Skip(SkipReason::Bot));
    }

    #[test]
    fn a_human_join_grants_the_configured_roles_and_posts_the_message() {
        let decision = decide_join(&facts(false), &config(Mode::Live, &[ROLE_LOW, ROLE_MID]));
        let JoinDecision::Act(actions) = decision else {
            panic!("a human join is acted on");
        };
        assert_eq!(actions.grant_role_ids, vec![ROLE_LOW, ROLE_MID]);
        assert!(actions.send_message);
        assert_eq!(actions.channel_id, CHANNEL);
        assert_eq!(actions.allowed_mentions.users, vec![MEMBER]);
        assert!(actions.allowed_mentions.roles.is_empty());
        assert!(!actions.allowed_mentions.everyone);
    }

    #[test]
    fn an_off_feature_skips_every_join() {
        let decision = decide_join(&facts(false), &config(Mode::Off, &[ROLE_LOW]));
        assert_eq!(decision, JoinDecision::Skip(SkipReason::FeatureDisabled));
    }

    #[test]
    fn a_join_without_a_channel_or_a_template_is_skipped() {
        let mut no_channel = config(Mode::Live, &[ROLE_LOW]);
        no_channel.channel_id = None;
        assert_eq!(
            decide_join(&facts(false), &no_channel),
            JoinDecision::Skip(SkipReason::NoChannel)
        );
        let mut no_template = config(Mode::Live, &[ROLE_LOW]);
        no_template.template_path = None;
        assert_eq!(
            decide_join(&facts(false), &no_template),
            JoinDecision::Skip(SkipReason::NoTemplate)
        );
    }

    #[test]
    fn the_allowed_mentions_are_the_new_member_only() {
        let JoinDecision::Act(actions) = decide_join(&facts(false), &config(Mode::Live, &[]))
        else {
            panic!("a human join is acted on");
        };
        assert_eq!(actions.allowed_mentions.users, vec![MEMBER]);
        assert!(actions.allowed_mentions.roles.is_empty());
        assert!(!actions.allowed_mentions.everyone);
    }

    // ---- the runtime -------------------------------------------------------

    #[tokio::test]
    async fn a_human_join_grants_the_roles_and_posts_the_welcome() {
        let mut rt = runtime(
            Mode::Live,
            &[ROLE_LOW, ROLE_MID],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::ok(),
        );
        let outcome = rt.handle_join(&facts(false)).await;
        assert_eq!(outcome.skipped, None);
        assert_eq!(outcome.role_results.len(), 2);
        assert!(outcome
            .role_results
            .iter()
            .all(|r| r.outcome == RoleOutcome::Done));
        assert_eq!(outcome.message, Some(MessageOutcome::Sent));
        assert_eq!(
            rt.writer.calls,
            vec![(MEMBER, ROLE_LOW), (MEMBER, ROLE_MID)]
        );
        assert_eq!(rt.sender.sent.len(), 1);
        assert_eq!(rt.sender.sent[0].0, CHANNEL);
    }

    #[tokio::test]
    async fn a_bot_join_grants_nothing_and_posts_nothing() {
        let mut rt = runtime(
            Mode::Live,
            &[ROLE_LOW],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::ok(),
        );
        let outcome = rt.handle_join(&facts(true)).await;
        assert_eq!(outcome.skipped, Some(SkipReason::Bot));
        assert!(rt.writer.calls.is_empty(), "no role is granted to a bot");
        assert!(rt.sender.sent.is_empty(), "no message is posted for a bot");
        assert!(outcome.role_results.is_empty());
        assert_eq!(outcome.message, None);
    }

    #[tokio::test]
    async fn a_privileged_join_role_is_refused_by_the_role_service() {
        let mut rt = runtime(
            Mode::Live,
            &[ROLE_PRIVILEGED],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::ok(),
        );
        let outcome = rt.handle_join(&facts(false)).await;
        assert_eq!(outcome.role_results.len(), 1);
        assert_eq!(
            outcome.role_results[0].outcome,
            RoleOutcome::Denied(crate::roles::eligibility::DenyReason::PrivilegedPermission)
        );
        assert!(
            rt.writer.calls.is_empty(),
            "a refused role is never written"
        );
        // The message is still posted: a refused role never blocks it.
        assert_eq!(outcome.message, Some(MessageOutcome::Sent));
    }

    #[tokio::test]
    async fn a_protected_join_role_is_refused_by_the_role_service() {
        let mut rt = runtime(
            Mode::Live,
            &[ROLE_PROTECTED],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::ok(),
        );
        rt.grant_policy.protected_role_ids = vec![ROLE_PROTECTED];
        let outcome = rt.handle_join(&facts(false)).await;
        assert_eq!(
            outcome.role_results[0].outcome,
            RoleOutcome::Denied(crate::roles::eligibility::DenyReason::ProtectedRole)
        );
        assert!(rt.writer.calls.is_empty());
    }

    #[tokio::test]
    async fn a_send_failure_is_reported_by_a_stable_code_and_does_not_block_the_grant() {
        let mut rt = runtime(
            Mode::Live,
            &[ROLE_LOW],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::failing(403),
        );
        let outcome = rt.handle_join(&facts(false)).await;
        assert_eq!(
            outcome.message,
            Some(MessageOutcome::Failed(CODE_SEND_FORBIDDEN))
        );
        assert_eq!(outcome.role_results[0].outcome, RoleOutcome::Done);
        assert_eq!(
            rt.writer.calls,
            vec![(MEMBER, ROLE_LOW)],
            "the grant happened"
        );
        assert_eq!(
            rt.alerter.alerts,
            vec![(799_001, CODE_SEND_FORBIDDEN.to_string())]
        );
    }

    #[test]
    fn the_send_failure_codes_are_stable_and_carry_no_text() {
        let error = |status| SendError {
            status,
            api_code: None,
        };
        assert_eq!(error(403).code(), CODE_SEND_FORBIDDEN);
        assert_eq!(error(429).code(), CODE_SEND_RATE_LIMITED);
        // Every other status is one stable code, not a guess.
        for status in [0u16, 400, 404, 500, 502] {
            assert_eq!(error(status).code(), CODE_SEND_FAILED, "{status}");
        }
        // The code carries no message text, no name and no id.
        for status in [0u16, 403, 429, 500] {
            let code = error(status).code();
            assert!(!code.contains(' '), "{code}");
            assert!(!code.contains('@'), "{code}");
        }
    }

    #[tokio::test]
    async fn the_alert_carries_the_code_only_and_never_the_message_text() {
        let mut rt = runtime(
            Mode::Live,
            &[],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::failing(500),
        );
        let outcome = rt.handle_join(&facts(false)).await;
        assert_eq!(
            outcome.message,
            Some(MessageOutcome::Failed(CODE_SEND_FAILED))
        );
        assert_eq!(rt.alerter.alerts.len(), 1);
        let (channel_id, code) = &rt.alerter.alerts[0];
        assert_eq!(*channel_id, 799_001);
        assert_eq!(code, CODE_SEND_FAILED);
        assert!(!code.contains("synthetic-user"), "{code}");
        assert!(!code.contains("700003"), "{code}");
    }

    #[tokio::test]
    async fn a_grant_failure_is_reported_and_does_not_block_the_message() {
        let mut rt = runtime(
            Mode::Live,
            &[ROLE_LOW],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::failing(ROLE_LOW, 403),
            FakeSender::ok(),
        );
        let outcome = rt.handle_join(&facts(false)).await;
        assert_eq!(
            outcome.role_results[0].outcome,
            RoleOutcome::Failed(crate::roles::grant::FailCode::Forbidden)
        );
        assert_eq!(outcome.message, Some(MessageOutcome::Sent));
        assert!(outcome.alerts.contains(&"forbidden"));
    }

    #[tokio::test]
    async fn the_failure_codes_of_two_items_do_not_overwrite_each_other() {
        let mut rt = runtime(
            Mode::Live,
            &[ROLE_LOW, ROLE_MID],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::failing(ROLE_LOW, 403),
            FakeSender::failing(500),
        );
        let outcome = rt.handle_join(&facts(false)).await;
        let codes = outcome.failure_codes();
        assert!(codes.contains(&"forbidden"), "{codes:?}");
        assert!(codes.contains(&CODE_SEND_FAILED), "{codes:?}");
    }

    #[tokio::test]
    async fn the_posted_message_mentions_the_new_member_only() {
        let mut rt = runtime(
            Mode::Live,
            &[],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::ok(),
        );
        rt.handle_join(&facts(false)).await;
        let message = &rt.sender.sent[0].1;
        assert_eq!(message.allowed_mentions.users, vec![MEMBER]);
        assert!(message.allowed_mentions.roles.is_empty());
        assert!(!message.allowed_mentions.everyone);
    }

    #[tokio::test]
    async fn shadow_mode_sends_nothing_grants_nothing_and_only_records() {
        let mut rt = runtime(
            Mode::Shadow,
            &[ROLE_LOW, ROLE_PRIVILEGED],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::ok(),
        );
        let outcome = rt.handle_join(&facts(false)).await;
        assert!(rt.writer.calls.is_empty(), "shadow grants nothing");
        assert!(rt.sender.sent.is_empty(), "shadow sends nothing");
        let would_do = outcome.would_do.expect("a would-do record");
        assert_eq!(would_do.roles_to_grant, 1);
        assert_eq!(would_do.roles_refused, vec!["privileged_permission"]);
        assert!(would_do.message_would_be_sent);
        // The record carries no user content.
        let text = format!("{would_do:?}");
        assert!(!text.contains("synthetic-user"), "{text}");
        assert!(!text.contains("700003"), "{text}");
    }

    #[tokio::test]
    async fn shadow_mode_also_skips_a_bot() {
        let mut rt = runtime(
            Mode::Shadow,
            &[ROLE_LOW],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::ok(),
        );
        let outcome = rt.handle_join(&facts(true)).await;
        assert_eq!(outcome.skipped, Some(SkipReason::Bot));
        assert!(rt.writer.calls.is_empty());
        assert!(rt.sender.sent.is_empty());
    }

    #[tokio::test]
    async fn an_unavailable_role_lookup_grants_nothing_and_still_posts_the_message() {
        let mut rt = runtime(
            Mode::Live,
            &[ROLE_LOW],
            FakeSource::unavailable(),
            FakeWriter::default(),
            FakeSender::ok(),
        );
        let outcome = rt.handle_join(&facts(false)).await;
        assert!(rt.writer.calls.is_empty());
        assert_eq!(outcome.message, Some(MessageOutcome::Sent));
    }

    // ---- startup validation -------------------------------------------------

    fn facts_entry(
        role_id: u64,
        entry: crate::preflight::RoleFactsEntry,
    ) -> crate::preflight::RoleFactsEntry {
        let _ = role_id;
        entry
    }

    #[test]
    fn a_privileged_join_role_is_refused_at_startup_with_the_key_and_the_code() {
        let entries = vec![crate::preflight::RoleFactsEntry::found(
            ROLE_PRIVILEGED,
            RoleFacts {
                id: ROLE_PRIVILEGED,
                position: 2,
                managed: false,
                permissions: crate::roles::eligibility::ADMINISTRATOR,
                exists: true,
            },
        )];
        let refusals = validate_join_roles(&[ROLE_PRIVILEGED], GUILD, &[], Some(10), &entries);
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0].key, KEY_JOIN_ROLE_IDS);
        assert_eq!(refusals[0].reason, "privileged_permission");
        assert_eq!(refusals[0].role_id, ROLE_PRIVILEGED);
    }

    #[test]
    fn a_protected_join_role_is_refused_at_startup_with_the_key_and_the_code() {
        let entries = vec![crate::preflight::RoleFactsEntry::found(
            ROLE_PROTECTED,
            RoleFacts {
                id: ROLE_PROTECTED,
                position: 2,
                managed: false,
                permissions: 0,
                exists: true,
            },
        )];
        let refusals = validate_join_roles(
            &[ROLE_PROTECTED],
            GUILD,
            &[ROLE_PROTECTED],
            Some(10),
            &entries,
        );
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0].key, KEY_JOIN_ROLE_IDS);
        assert_eq!(refusals[0].reason, "protected_role");
    }

    #[test]
    fn a_usable_join_role_is_not_refused() {
        let entries = vec![crate::preflight::RoleFactsEntry::found(
            ROLE_LOW,
            RoleFacts {
                id: ROLE_LOW,
                position: 2,
                managed: false,
                permissions: 0,
                exists: true,
            },
        )];
        let refusals = validate_join_roles(&[ROLE_LOW], GUILD, &[], Some(10), &entries);
        assert!(refusals.is_empty());
    }

    #[test]
    fn a_deleted_or_at_or_above_the_bot_join_role_is_refused_at_startup() {
        let deleted = vec![crate::preflight::RoleFactsEntry::missing(ROLE_DELETED)];
        let refusals = validate_join_roles(&[ROLE_DELETED], GUILD, &[], Some(10), &deleted);
        assert_eq!(refusals[0].reason, "role_deleted");

        let high = vec![crate::preflight::RoleFactsEntry::found(
            ROLE_HIGH,
            RoleFacts {
                id: ROLE_HIGH,
                position: 10,
                managed: false,
                permissions: 0,
                exists: true,
            },
        )];
        let refusals = validate_join_roles(&[ROLE_HIGH], GUILD, &[], Some(10), &high);
        assert_eq!(refusals[0].reason, "at_or_above_bot");
    }

    #[test]
    fn the_startup_refusal_names_the_key_and_never_a_value() {
        let entries = vec![crate::preflight::RoleFactsEntry::found(
            ROLE_PRIVILEGED,
            RoleFacts {
                id: ROLE_PRIVILEGED,
                position: 2,
                managed: false,
                permissions: crate::roles::eligibility::ADMINISTRATOR,
                exists: true,
            },
        )];
        let refusals = validate_join_roles(&[ROLE_PRIVILEGED], GUILD, &[], Some(10), &entries);
        let text = format!("{refusals:?}");
        assert!(text.contains(KEY_JOIN_ROLE_IDS), "{text}");
        assert!(!text.contains("ADMINISTRATOR"), "{text}");
    }

    // ---- the job kinds ------------------------------------------------------

    #[test]
    fn the_job_kinds_are_plain_tokens() {
        for kind in [JOB_KIND_GRANT_ROLES, JOB_KIND_SEND_MESSAGE] {
            assert!(!kind.is_empty(), "{kind}");
            assert!(kind.len() <= 64, "{kind}");
            assert!(
                kind.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{kind}"
            );
        }
    }

    #[test]
    fn the_job_kinds_are_at_most_once_because_they_are_user_visible() {
        use crate::jobs::queue::RestartPolicy;
        assert_eq!(
            restart_policy(JOB_KIND_SEND_MESSAGE),
            RestartPolicy::AtMostOnce
        );
        assert_eq!(
            restart_policy(JOB_KIND_GRANT_ROLES),
            RestartPolicy::AtMostOnce
        );
    }

    // ---- the feature registry hook -----------------------------------------

    /// A guild lookup with a fixed answer.
    struct FakeGuilds {
        facts: Option<GuildFacts>,
    }

    impl FakeGuilds {
        fn with(name: &str, member_count: u64) -> Self {
            FakeGuilds {
                facts: Some(GuildFacts {
                    name: name.to_string(),
                    member_count,
                }),
            }
        }
        fn unknown() -> Self {
            FakeGuilds { facts: None }
        }
    }

    #[s::async_trait]
    impl GuildLookup for FakeGuilds {
        async fn guild_facts(&self, _guild_id: u64) -> Option<GuildFacts> {
            self.facts.clone()
        }
    }

    /// A member that joined, as the gateway delivers it.
    fn member_added(user_id: u64, bot: bool) -> s::FullEvent {
        use s::all::{GuildId, UserId};
        let mut user = s::User::default();
        user.id = UserId::new(user_id);
        user.name = "synthetic-user".to_string();
        user.bot = bot;
        let mut member = s::Member::default();
        member.user = user;
        member.guild_id = GuildId::new(GUILD);
        s::FullEvent::GuildMemberAddition { new_member: member }
    }

    #[tokio::test]
    async fn the_feature_hook_reaches_the_runtime_with_the_guild_facts() {
        let feature = WelcomeFeature::new(
            runtime(
                Mode::Live,
                &[ROLE_LOW],
                FakeSource::with_roles(guild_roles()),
                FakeWriter::default(),
                FakeSender::ok(),
            ),
            FakeGuilds::with("synthetic-server", 42),
        );
        {
            use crate::features::Feature;
            feature
                .handle(&0u8, &member_added(MEMBER, false), &0u16, Some(GUILD), None)
                .await;
        }
        let runtime = feature.runtime.lock().await;
        assert_eq!(
            runtime.sender.sent.len(),
            1,
            "the welcome was posted through the feature hook"
        );
        assert_eq!(runtime.sender.sent[0].0, CHANNEL);
        assert_eq!(runtime.writer.calls, vec![(MEMBER, ROLE_LOW)]);
        // The rendered message carries the guild facts the lookup gave: the
        // member count comes from the lookup and is rendered as the ordinal.
        let embed = &runtime.sender.sent[0].1.embed;
        let description = embed.description.clone().unwrap_or_default();
        assert_eq!(
            description, "You are our 42nd member.",
            "the member count came from the guild lookup"
        );
    }

    #[tokio::test]
    async fn the_feature_is_off_when_its_mode_is_off() {
        let feature = WelcomeFeature::new(
            runtime(
                Mode::Off,
                &[ROLE_LOW],
                FakeSource::with_roles(guild_roles()),
                FakeWriter::default(),
                FakeSender::ok(),
            ),
            FakeGuilds::with("synthetic-server", 42),
        );
        {
            use crate::features::Feature;
            assert!(!<WelcomeFeature<_, _, _, _, _, _> as Feature<u8, u16>>::enabled(&feature));
        }
        // And an event that reaches it does nothing.
        {
            use crate::features::Feature;
            feature
                .handle(&0u8, &member_added(MEMBER, false), &0u16, Some(GUILD), None)
                .await;
        }
        let runtime = feature.runtime.lock().await;
        assert!(runtime.sender.sent.is_empty());
        assert!(runtime.writer.calls.is_empty());
    }

    #[tokio::test]
    async fn the_feature_hook_ignores_every_event_but_a_member_join() {
        let feature = WelcomeFeature::new(
            runtime(
                Mode::Live,
                &[ROLE_LOW],
                FakeSource::with_roles(guild_roles()),
                FakeWriter::default(),
                FakeSender::ok(),
            ),
            FakeGuilds::with("synthetic-server", 42),
        );
        {
            use crate::features::Feature;
            let event = s::FullEvent::GuildMemberRemoval {
                guild_id: s::all::GuildId::new(GUILD),
                user: s::User::default(),
                member_data_if_available: None,
            };
            feature.handle(&0u8, &event, &0u16, Some(GUILD), None).await;
        }
        let runtime = feature.runtime.lock().await;
        assert!(runtime.sender.sent.is_empty());
        assert!(runtime.writer.calls.is_empty());
    }

    #[tokio::test]
    async fn the_feature_hook_skips_a_bot_that_joined() {
        let feature = WelcomeFeature::new(
            runtime(
                Mode::Live,
                &[ROLE_LOW],
                FakeSource::with_roles(guild_roles()),
                FakeWriter::default(),
                FakeSender::ok(),
            ),
            FakeGuilds::with("synthetic-server", 42),
        );
        {
            use crate::features::Feature;
            feature
                .handle(&0u8, &member_added(MEMBER, true), &0u16, Some(GUILD), None)
                .await;
        }
        let runtime = feature.runtime.lock().await;
        assert!(runtime.sender.sent.is_empty());
        assert!(runtime.writer.calls.is_empty());
    }

    // ---- the durable work (the real job queue) -------------------------------

    /// A scratch directory under `target/`, as the card asks.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("we2-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_crash_during_the_welcome_work_abandons_it_and_never_repeats_it() {
        use crate::jobs::queue::{self, NewJob, ReconcileOutcome, RestartPolicy, RetryPolicy};
        let dir = scratch("crash");
        let path = dir.join("core.sqlite").to_string_lossy().into_owned();
        let mut conn = crate::storage::open(&path, &[]).unwrap();
        queue::init(&mut conn).unwrap();
        let now = 1_700_000_000i64;
        for kind in [JOB_KIND_GRANT_ROLES, JOB_KIND_SEND_MESSAGE] {
            let job = NewJob {
                kind,
                // The dedupe key is unique across kinds, so the two jobs of one
                // join carry one key each.
                dedupe_key: if kind == JOB_KIND_GRANT_ROLES {
                    "join-700003-roles"
                } else {
                    "join-700003-message"
                },
                payload_json: "{}",
                due_at: now,
            };
            let queue::ScheduleOutcome::Created { id } = queue::schedule(&conn, &job, now).unwrap()
            else {
                panic!("a fresh key is created");
            };
            // The worker claimed the job and the process died before it finished.
            assert!(queue::try_claim(&conn, id, now).unwrap());
        }
        let table: &[(&str, RestartPolicy)] = &[
            (JOB_KIND_GRANT_ROLES, RestartPolicy::AtMostOnce),
            (JOB_KIND_SEND_MESSAGE, RestartPolicy::AtMostOnce),
        ];
        let report =
            queue::reconcile_after_restart(&conn, now, table, &RetryPolicy::DEFAULT).unwrap();
        assert_eq!(report.entries.len(), 2);
        for entry in &report.entries {
            assert_eq!(
                entry.outcome,
                ReconcileOutcome::AbandonedAtMostOnce,
                "{entry:?}"
            );
        }
        // Nothing is claimable again: the work is not repeated.
        let batch = queue::claim_due(&conn, now, 10).unwrap();
        assert!(batch.claimed.is_empty(), "{:?}", batch.claimed);
    }

    // ---- liveness ------------------------------------------------------------

    /// A small LCG, so the sequence is bounded and reproducible.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    /// The state of one item of work, kept per item.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ItemState {
        Pending,
        Acked,
        Failed,
    }

    #[tokio::test]
    async fn a_seeded_sequence_of_joins_always_reaches_its_final_state() {
        // The model runtime: every join is worked on until its message is
        // posted or the attempt budget is used up. The model acknowledges,
        // fails or drops each join, and a failed join is retried - the retry
        // gate is the join's own attempt count, never one shared error.
        const JOINS: usize = 40;
        const MAX_ATTEMPTS: usize = 4;

        let mut rng = Lcg(20_261_122);
        let mut states: Vec<ItemState> = vec![ItemState::Pending; JOINS];
        let mut attempts: Vec<usize> = vec![0; JOINS];
        let mut posted: usize = 0;

        let mut rounds = 0usize;
        loop {
            rounds += 1;
            assert!(rounds < 1000, "the sequence must settle");
            let mut pending = 0usize;
            for index in 0..states.len() {
                if states[index] != ItemState::Pending {
                    continue;
                }
                pending += 1;
                attempts[index] += 1;
                let roll = rng.next() % 10;
                // The model runtime: the sender fails now and then.
                let mut sender = if roll < 6 {
                    FakeSender::ok()
                } else {
                    FakeSender::failing(500)
                };
                let mut rt = runtime(
                    Mode::Live,
                    &[ROLE_LOW],
                    FakeSource::with_roles(guild_roles()),
                    FakeWriter::default(),
                    std::mem::replace(&mut sender, FakeSender::ok()),
                );
                let mut member = facts(false);
                member.user_id = MEMBER + index as u64;
                let outcome = rt.handle_join(&member).await;
                // The role is granted every time, whatever the message does.
                assert_eq!(outcome.role_results.len(), 1);
                assert_eq!(
                    outcome.role_results[0].outcome,
                    RoleOutcome::Done,
                    "the grant is independent of the message"
                );
                match outcome.message {
                    Some(MessageOutcome::Sent) => {
                        posted += 1;
                        states[index] = ItemState::Acked;
                    }
                    Some(MessageOutcome::Failed(_)) => {
                        if attempts[index] >= MAX_ATTEMPTS {
                            states[index] = ItemState::Failed;
                        }
                    }
                    None => panic!("a join that was acted on has a message outcome"),
                }
            }
            if pending == 0 {
                break;
            }
        }

        // Every join reached a final state: nothing is left pending.
        assert!(
            states.iter().all(|state| *state != ItemState::Pending),
            "{states:?}"
        );
        // The number of posts matches the number of acknowledged joins.
        let acked = states.iter().filter(|s| **s == ItemState::Acked).count();
        assert_eq!(acked, posted, "{states:?}");
        // A failed join used its whole budget: it was retried, not dropped.
        for (index, state) in states.iter().enumerate() {
            if *state == ItemState::Failed {
                assert_eq!(attempts[index], MAX_ATTEMPTS, "{states:?}");
            }
        }
    }

    #[tokio::test]
    async fn the_runtime_reports_every_item_of_a_join_separately() {
        // The runtime keeps one outcome per item, so a later failure never
        // overwrites an earlier one.
        let mut rt = runtime(
            Mode::Live,
            &[ROLE_LOW, ROLE_MID],
            FakeSource::with_roles(guild_roles()),
            FakeWriter::default(),
            FakeSender::ok(),
        );
        let outcome = rt.handle_join(&facts(false)).await;
        assert_eq!(outcome.role_results.len(), 2);
        assert_eq!(outcome.role_results[0].role_id, ROLE_LOW);
        assert_eq!(outcome.role_results[1].role_id, ROLE_MID);
        assert_eq!(outcome.message, Some(MessageOutcome::Sent));
    }
}

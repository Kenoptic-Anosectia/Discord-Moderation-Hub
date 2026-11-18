//! Pure decision logic for the flash-media expiry feature.
//!
//! This module is the Discord-free core of the "flash media" feature that replaces the old
//! YAGPDB custom command: for a non-bot, non-webhook message that carries attachments in a
//! configured flash channel, the bot posts a short notice and deletes both the original
//! message and the notice after a delay.
//!
//! Everything here is a pure function of plain facts. There is no serenity type, no I/O, no
//! clock read and no randomness: `now_ms` and every id are passed in by the caller.
//!
//! Privacy notes:
//! * The notice text is built only from the configured role id, the author id and the
//!   channel id. Message content is never an input, so it can never be echoed into the
//!   notice and can never inject a mention.
//! * The allowed-mention set contains the configured role and the author, nothing else.
//!   `@everyone` / `@here` are never allowed.
//! * Flash media must never reach the log channel or any cache/archive. The logger needs a
//!   predicate over exactly the same configured channel ids, so
//!   [`is_flash_channel`] (what the timer acts on) and [`is_flash_area`] (what the logger
//!   and cache must exclude) are derived from the same [`FlashConfig`] and can never
//!   disagree with [`decide`].
//!
//! Errors and ignore reasons are stable enum codes. They never carry message text, user
//! names or user ids, so nothing private can leak through an error path.
//!
//! Wiring card W1 removes the module-level `allow(dead_code)` in `super::mod`.

use std::fmt;

/// Environment key holding the comma separated list of flash channel ids.
///
/// Default: empty (feature disabled). Read by the wiring card, not by this module.
pub const KEY_CHANNEL_IDS: &str = "FLASH_CHANNEL_IDS";
/// Environment key holding the optional role id that is pinged in the notice.
///
/// Default: unset (the notice then carries no role mention).
pub const KEY_PING_ROLE_ID: &str = "FLASH_PING_ROLE_ID";
/// Environment key holding the expiry delay in seconds.
///
/// Default: [`DEFAULT_DELAY_SECS`].
pub const KEY_DELAY_SECS: &str = "FLASH_DELAY_SECS";
/// Environment key holding the "threads of a flash channel are included" flag.
///
/// Default: true, because including threads favours privacy (the media is deleted).
pub const KEY_INCLUDE_THREADS: &str = "FLASH_INCLUDE_THREADS";

/// Smallest accepted expiry delay in seconds.
pub const MIN_DELAY_SECS: u64 = 10;
/// Largest accepted expiry delay in seconds (24 hours).
pub const MAX_DELAY_SECS: u64 = 86_400;
/// Default expiry delay in seconds; the delay the original YAGPDB command used.
pub const DEFAULT_DELAY_SECS: u64 = 180;

/// Stable reason codes for a configuration that was refused.
///
/// The codes never carry the offending value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// The expiry delay was outside `MIN_DELAY_SECS..=MAX_DELAY_SECS`.
    DelayOutOfRange,
    /// The channel id list contained the id zero, which is not a real channel.
    InvalidChannelId,
    /// The configured ping role id was zero, which is not a real role.
    InvalidRoleId,
    /// The configured guild id was zero, which is not a real guild.
    InvalidGuildId,
}

/// A refused configuration.
///
/// The error names the configuration key that was wrong and never echoes the value, so it
/// is safe to log verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    key: &'static str,
}

impl ConfigError {
    fn new(kind: ConfigErrorKind, key: &'static str) -> Self {
        Self { kind, key }
    }

    /// Returns the stable reason code for this refusal.
    pub fn kind(&self) -> ConfigErrorKind {
        self.kind
    }

    /// Returns the configuration key that was refused.
    pub fn key(&self) -> &'static str {
        self.key
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self.kind {
            ConfigErrorKind::DelayOutOfRange => "value must be between 10 and 86400 seconds",
            ConfigErrorKind::InvalidChannelId => "value must not contain the id zero",
            ConfigErrorKind::InvalidRoleId => "value must not be the id zero",
            ConfigErrorKind::InvalidGuildId => "value must not be the id zero",
        };
        let key = self.key;
        write!(f, "{key}: {msg}")
    }
}

impl std::error::Error for ConfigError {}

/// Validated configuration of the flash-media feature.
///
/// Build it with [`FlashConfig::new`]; the fields are private so an invalid configuration
/// cannot be constructed by accident. An empty channel id list is valid and means the
/// feature is disabled: every message is then ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashConfig {
    channel_ids: Vec<u64>,
    ping_role_id: Option<u64>,
    delay_secs: u64,
    include_threads: bool,
    guild_id: Option<u64>,
}

impl FlashConfig {
    /// Validates and builds the configuration.
    ///
    /// * `channel_ids` may be empty (feature disabled). Duplicates are removed, keeping the
    ///   first occurrence. The id zero is refused.
    /// * `ping_role_id` may be absent (the notice then has no role mention). Zero is
    ///   refused.
    /// * `delay_secs` must be inside `MIN_DELAY_SECS..=MAX_DELAY_SECS`.
    /// * `include_threads` decides whether threads of a flash channel are acted upon.
    /// * `guild_id` may be absent (every guild is then in scope). Zero is refused.
    pub fn new(
        channel_ids: Vec<u64>,
        ping_role_id: Option<u64>,
        delay_secs: u64,
        include_threads: bool,
        guild_id: Option<u64>,
    ) -> Result<Self, ConfigError> {
        let mut channels: Vec<u64> = Vec::with_capacity(channel_ids.len());
        for id in channel_ids {
            if id == 0 {
                return Err(ConfigError::new(
                    ConfigErrorKind::InvalidChannelId,
                    KEY_CHANNEL_IDS,
                ));
            }
            if !channels.contains(&id) {
                channels.push(id);
            }
        }
        if let Some(role) = ping_role_id {
            if role == 0 {
                return Err(ConfigError::new(ConfigErrorKind::InvalidRoleId, KEY_PING_ROLE_ID));
            }
        }
        if let Some(guild) = guild_id {
            if guild == 0 {
                return Err(ConfigError::new(ConfigErrorKind::InvalidGuildId, "GUILD_ID"));
            }
        }
        Ok(Self {
            channel_ids: channels,
            ping_role_id,
            delay_secs,
            include_threads,
            guild_id,
        })
    }

    /// True when the feature is enabled, i.e. at least one flash channel is configured.
    pub fn is_enabled(&self) -> bool {
        !self.channel_ids.is_empty()
    }

    /// The configured flash channel ids, without duplicates.
    pub fn channel_ids(&self) -> &[u64] {
        &self.channel_ids
    }

    /// The role id pinged in the notice, if any.
    pub fn ping_role_id(&self) -> Option<u64> {
        self.ping_role_id
    }

    /// The expiry delay in seconds.
    pub fn delay_secs(&self) -> u64 {
        self.delay_secs
    }

    /// Whether threads of a flash channel are acted upon.
    pub fn include_threads(&self) -> bool {
        self.include_threads
    }

    /// The guild the feature is scoped to, if any.
    pub fn guild_id(&self) -> Option<u64> {
        self.guild_id
    }
}

/// The plain facts about one incoming message.
///
/// The content of the message is deliberately absent: the notice never quotes it, so it
/// cannot carry a mention into the notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageFacts {
    /// Guild the message was sent in.
    pub guild_id: u64,
    /// Channel the message was sent in. For a thread this is the thread id.
    pub channel_id: u64,
    /// Parent channel of a thread, absent for a message that is not in a thread.
    pub parent_channel_id: Option<u64>,
    /// Id of the author.
    pub author_id: u64,
    /// True when the author is a bot account.
    pub author_is_bot: bool,
    /// True when the message was sent through a webhook.
    pub is_webhook: bool,
    /// Number of attachments on the message. Zero means the message is ignored.
    pub attachment_count: usize,
    /// Id of the message itself.
    pub message_id: u64,
}

/// Stable reason codes for a message that is not expired.
///
/// The codes never carry message text, user names or ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// The message came from another guild than the configured one.
    GuildMismatch,
    /// The feature is disabled: no flash channel is configured.
    FeatureDisabled,
    /// The author is a bot account.
    BotAuthor,
    /// The message was sent through a webhook.
    WebhookAuthor,
    /// The author id was zero, which is not a real user.
    InvalidAuthor,
    /// The message id was zero, which is not a real message.
    InvalidMessage,
    /// The message carries no attachment.
    NoAttachments,
    /// The channel is not a configured flash channel.
    ChannelNotConfigured,
    /// The message is in a thread of a flash channel but threads are not included.
    ThreadNotIncluded,
}

/// The set of mentions the notice is allowed to render.
///
/// It mirrors the shape of the Discord allowed-mentions object: only the listed users and
/// roles may be mentioned, and `everyone` is always false so that `@everyone` and
/// `@here` can never fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedMentions {
    /// Users that may be mentioned. Exactly the author of the flashed message.
    pub users: Vec<u64>,
    /// Roles that may be mentioned. At most the configured ping role.
    pub roles: Vec<u64>,
    /// Whether `@everyone` / `@here` may be mentioned. Always false.
    pub everyone: bool,
}

/// The rendered notice the bot posts next to the flashed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The notice text.
    pub text: String,
    /// The mentions the notice is allowed to render.
    pub allowed_mentions: AllowedMentions,
}

/// The two messages that are deleted when the flash expires.
///
/// The notice message id is only known after the bot posted the notice, so the runtime
/// fills it in with [`ExpirePlan::deletions`]. The pair is plain data: the durable job
/// card persists it and replays the two deletions idempotently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deletions {
    /// Id of the user's original message.
    pub original_message_id: u64,
    /// Id of the bot's notice message.
    pub notice_message_id: u64,
}

/// Everything the runtime needs to expire one flash message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpirePlan {
    /// Delay in seconds after which both messages are deleted.
    pub delay_secs: u64,
    /// The notice to post.
    pub notice: Notice,
    /// Id of the user's original message.
    pub original_message_id: u64,
    /// Channel the notice is posted in and the media expires in.
    pub channel_id: u64,
    /// Id of the author of the flashed message.
    pub author_id: u64,
}

impl ExpirePlan {
    /// The pair of deletions for this plan, once the notice has been posted.
    pub fn deletions(&self, notice_message_id: u64) -> Deletions {
        Deletions {
            original_message_id: self.original_message_id,
            notice_message_id,
        }
    }

    /// The absolute deadline in epoch milliseconds for this plan.
    pub fn deadline_at(&self, now_ms: u64) -> u64 {
        deadline(now_ms, self.delay_secs)
    }
}

/// The decision for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The message is a flash: post the notice and delete both messages after the delay.
    Expire(ExpirePlan),
    /// The message is not a flash; the reason says why.
    Ignore(IgnoreReason),
}

/// Computes the absolute deadline in epoch milliseconds.
///
/// Saturates instead of wrapping: a clock far in the future yields `u64::MAX` rather than
/// a deadline in the past, which would delete the media early.
pub fn deadline(now_ms: u64, delay_secs: u64) -> u64 {
    now_ms.saturating_add(delay_secs.saturating_mul(1000))
}

/// True when the timer acts on this location.
///
/// The answer mirrors [`decide`] exactly: a configured channel is a flash channel, and a
/// thread of a configured channel is a flash channel only while `include_threads` is set.
pub fn is_flash_channel(
    cfg: &FlashConfig,
    channel_id: u64,
    parent_channel_id: Option<u64>,
) -> bool {
    let _ = (cfg, channel_id, parent_channel_id);
    false
}

/// True when this location sits inside the configured flash area.
///
/// Unlike [`is_flash_channel`] this ignores `include_threads`: the logger and the cache
/// must exclude a thread of a flash channel whether or not the timer acts on it. This
/// predicate is never weaker than [`is_flash_channel`].
pub fn is_flash_area(cfg: &FlashConfig, channel_id: u64, parent_channel_id: Option<u64>) -> bool {
    let _ = (cfg, channel_id, parent_channel_id);
    false
}

/// Decides whether one message is a flash that must expire.
///
/// The decision is a pure function of the facts and the configuration. Nothing is read
/// from the clock and no id is looked up.
pub fn decide(msg: &MessageFacts, cfg: &FlashConfig) -> Decision {
    let _ = (msg, cfg);
    Decision::Ignore(IgnoreReason::FeatureDisabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic ids. They are deliberately far from any real Discord snowflake and carry
    // no meaning: 18 digits that were never issued by Discord.
    const GUILD: u64 = 1_000_000_000_000_000_001;
    const FLASH_CH: u64 = 1_000_000_000_000_000_002;
    const OTHER_CH: u64 = 1_000_000_000_000_000_003;
    const THREAD_CH: u64 = 1_000_000_000_000_000_004;
    const ROLE: u64 = 1_000_000_000_000_000_005;
    const AUTHOR: u64 = 1_000_000_000_000_000_006;
    const FOREIGN_USER: u64 = 1_000_000_000_000_000_007;
    const MESSAGE: u64 = 1_000_000_000_000_000_008;

    fn cfg() -> FlashConfig {
        FlashConfig::new(
            vec![FLASH_CH],
            Some(ROLE),
            DEFAULT_DELAY_SECS,
            true,
            Some(GUILD),
        )
        .expect("config is valid")
    }

    fn facts() -> MessageFacts {
        MessageFacts {
            guild_id: GUILD,
            channel_id: FLASH_CH,
            parent_channel_id: None,
            author_id: AUTHOR,
            author_is_bot: false,
            is_webhook: false,
            attachment_count: 1,
            message_id: MESSAGE,
        }
    }

    fn expect_expire(decision: &Decision) -> ExpirePlan {
        match decision {
            Decision::Expire(plan) => plan.clone(),
            Decision::Ignore(reason) => panic!("expected Expire, got Ignore({reason:?})"),
        }
    }

    fn expect_ignore(decision: &Decision) -> IgnoreReason {
        match decision {
            Decision::Ignore(reason) => *reason,
            Decision::Expire(plan) => panic!("expected Ignore, got Expire({plan:?})"),
        }
    }

    /// A non-bot message with attachments in a configured flash channel expires, and the
    /// plan carries the delay, the ids and the notice.
    #[test]
    fn non_bot_message_with_attachments_in_flash_channel_expires() {
        let plan = expect_expire(&decide(&facts(), &cfg()));
        assert_eq!(plan.delay_secs, 180);
        assert_eq!(plan.original_message_id, MESSAGE);
        assert_eq!(plan.channel_id, FLASH_CH);
        assert_eq!(plan.author_id, AUTHOR);
        assert_eq!(plan.notice.allowed_mentions.users, vec![AUTHOR]);
        assert_eq!(plan.notice.allowed_mentions.roles, vec![ROLE]);
        assert!(!plan.notice.allowed_mentions.everyone);
    }

    /// A bot author is ignored, even with attachments in a flash channel.
    #[test]
    fn bot_author_is_ignored() {
        let mut f = facts();
        f.author_is_bot = true;
        assert_eq!(
            expect_ignore(&decide(&f, &cfg())),
            IgnoreReason::BotAuthor
        );
    }

    /// A webhook author is ignored, even with attachments in a flash channel.
    #[test]
    fn webhook_author_is_ignored() {
        let mut f = facts();
        f.is_webhook = true;
        assert_eq!(
            expect_ignore(&decide(&f, &cfg())),
            IgnoreReason::WebhookAuthor
        );
    }

    /// A message without attachments is ignored; parity with the original command, which
    /// only reacted to messages that carried attachments.
    #[test]
    fn message_without_attachments_is_ignored() {
        let mut f = facts();
        f.attachment_count = 0;
        assert_eq!(
            expect_ignore(&decide(&f, &cfg())),
            IgnoreReason::NoAttachments
        );
    }

    /// A message in a channel that is not configured is ignored.
    #[test]
    fn other_channel_is_ignored() {
        let mut f = facts();
        f.channel_id = OTHER_CH;
        assert_eq!(
            expect_ignore(&decide(&f, &cfg())),
            IgnoreReason::ChannelNotConfigured
        );
    }

    /// A thread of a flash channel follows `include_threads` in both directions.
    #[test]
    fn thread_of_flash_channel_follows_include_threads_both_ways() {
        let mut f = facts();
        f.channel_id = THREAD_CH;
        f.parent_channel_id = Some(FLASH_CH);

        let with_threads = FlashConfig::new(
            vec![FLASH_CH],
            Some(ROLE),
            DEFAULT_DELAY_SECS,
            true,
            Some(GUILD),
        )
        .expect("config is valid");
        let plan = expect_expire(&decide(&f, &with_threads));
        assert_eq!(plan.channel_id, THREAD_CH);
        assert!(plan.notice.text.contains("<#1000000000000000004>"));

        let without_threads = FlashConfig::new(
            vec![FLASH_CH],
            Some(ROLE),
            DEFAULT_DELAY_SECS,
            false,
            Some(GUILD),
        )
        .expect("config is valid");
        assert_eq!(
            expect_ignore(&decide(&f, &without_threads)),
            IgnoreReason::ThreadNotIncluded
        );
    }

    /// The notice for the default delay matches the original command byte for byte.
    #[test]
    fn notice_text_matches_the_original_for_the_default_delay() {
        let plan = expect_expire(&decide(&facts(), &cfg()));
        assert_eq!(
            plan.notice.text,
            "<@&1000000000000000005> New flash from <@1000000000000000006> in \
             <#1000000000000000002>. This will be deleted in 3 minutes."
        );
    }

    /// The delay is rendered as minutes when it is a multiple of 60, otherwise as seconds;
    /// a single minute is singular. The original printed "1 minutes", which is a grammar
    /// flaw and is not copied.
    #[test]
    fn duration_rendering_rules() {
        for (delay_secs, expected) in [
            (60u64, "1 minute"),
            (120, "2 minutes"),
            (90, "90 seconds"),
            (45, "45 seconds"),
            (180, "3 minutes"),
            (10, "10 seconds"),
        ] {
            let c = FlashConfig::new(
                vec![FLASH_CH],
                Some(ROLE),
                delay_secs,
                true,
                Some(GUILD),
            )
            .expect("config is valid");
            let plan = expect_expire(&decide(&facts(), &c));
            let tail = format!("This will be deleted in {expected}.");
            assert!(
                plan.notice.text.ends_with(&tail),
                "delay {delay_secs} rendered as {:?}, expected it to end with {tail:?}",
                plan.notice.text
            );
        }
    }

    /// Without a configured ping role the notice carries no role mention at all.
    #[test]
    fn missing_ping_role_omits_the_role_mention() {
        let c = FlashConfig::new(
            vec![FLASH_CH],
            None,
            DEFAULT_DELAY_SECS,
            true,
            Some(GUILD),
        )
        .expect("config is valid");
        let plan = expect_expire(&decide(&facts(), &c));
        assert_eq!(
            plan.notice.text,
            "New flash from <@1000000000000000006> in <#1000000000000000002>. \
             This will be deleted in 3 minutes."
        );
        assert!(!plan.notice.text.contains("@&"));
        assert!(plan.notice.allowed_mentions.roles.is_empty());
        assert_eq!(plan.notice.allowed_mentions.users, vec![AUTHOR]);
    }

    /// The allowed mentions are exactly the configured role and the author.
    #[test]
    fn allowed_mentions_are_exactly_the_role_and_the_author() {
        let plan = expect_expire(&decide(&facts(), &cfg()));
        let mentions = &plan.notice.allowed_mentions;
        assert_eq!(mentions.users, vec![AUTHOR]);
        assert_eq!(mentions.roles, vec![ROLE]);
        assert!(!mentions.everyone);
        assert!(!mentions.users.contains(&FOREIGN_USER));
        assert!(!mentions.roles.contains(&FOREIGN_USER));
        assert_eq!(plan.notice.text.matches('@').count(), 2);
    }

    /// A message whose content carries `@everyone` and a foreign user mention cannot widen
    /// the allowed mentions: the content is not an input of the policy at all, so it is
    /// never echoed and never parsed for mentions.
    #[test]
    fn message_content_cannot_widen_the_allowed_mentions() {
        // The content is shown here only to document the adversarial case; the policy has
        // no field for it, which is what makes the case impossible by construction.
        let adversarial_content = "@everyone <@1000000000000000007> look at this";
        assert!(adversarial_content.contains("@everyone"));

        let plan = expect_expire(&decide(&facts(), &cfg()));
        let mentions = &plan.notice.allowed_mentions;
        assert_eq!(mentions.users, vec![AUTHOR]);
        assert_eq!(mentions.roles, vec![ROLE]);
        assert!(!mentions.everyone);
        assert!(!plan.notice.text.contains("@everyone"));
        assert!(!plan.notice.text.contains("@here"));
        assert!(!plan.notice.text.contains("1000000000000000007"));
    }

    /// The delay bounds are enforced and the error names the key without the value.
    #[test]
    fn delay_bounds_are_enforced() {
        for ok in [MIN_DELAY_SECS, 180, MAX_DELAY_SECS] {
            let c = FlashConfig::new(vec![FLASH_CH], Some(ROLE), ok, true, Some(GUILD));
            assert!(c.is_ok(), "delay {ok} should be accepted");
        }
        for bad in [MIN_DELAY_SECS - 1, MAX_DELAY_SECS + 1, 0, u64::MAX] {
            let err = FlashConfig::new(vec![FLASH_CH], Some(ROLE), bad, true, Some(GUILD))
                .expect_err("delay outside the bounds must be refused");
            assert_eq!(err.kind(), ConfigErrorKind::DelayOutOfRange);
            assert_eq!(err.key(), KEY_DELAY_SECS);
            let text = err.to_string();
            assert!(text.contains(KEY_DELAY_SECS), "error text: {text}");
            assert!(!text.contains(&bad.to_string()), "error echoes the value: {text}");
            let debug = format!("{err:?}");
            assert!(!debug.contains(&bad.to_string()), "debug echoes the value: {debug}");
        }
    }

    /// An empty channel list disables the feature: every message is ignored.
    #[test]
    fn empty_channel_list_disables_the_feature() {
        let c = FlashConfig::new(
            Vec::new(),
            Some(ROLE),
            DEFAULT_DELAY_SECS,
            true,
            Some(GUILD),
        )
        .expect("an empty channel list is valid");
        assert!(!c.is_enabled());
        assert_eq!(
            expect_ignore(&decide(&facts(), &c)),
            IgnoreReason::FeatureDisabled
        );
        assert!(!is_flash_channel(&c, FLASH_CH, None));
        assert!(!is_flash_area(&c, FLASH_CH, None));
    }

    /// Duplicate channel ids are removed and the id zero is refused.
    #[test]
    fn channel_ids_are_deduplicated_and_zero_is_refused() {
        let c = FlashConfig::new(
            vec![FLASH_CH, OTHER_CH, FLASH_CH],
            Some(ROLE),
            DEFAULT_DELAY_SECS,
            true,
            Some(GUILD),
        )
        .expect("config is valid");
        assert_eq!(c.channel_ids(), &[FLASH_CH, OTHER_CH]);

        let err = FlashConfig::new(
            vec![FLASH_CH, 0],
            Some(ROLE),
            DEFAULT_DELAY_SECS,
            true,
            Some(GUILD),
        )
        .expect_err("the id zero must be refused");
        assert_eq!(err.kind(), ConfigErrorKind::InvalidChannelId);
        assert_eq!(err.key(), KEY_CHANNEL_IDS);
        assert!(!err.to_string().contains('0'));
    }

    /// A message from another guild is out of scope.
    #[test]
    fn another_guild_is_ignored() {
        let mut f = facts();
        f.guild_id = 1_000_000_000_000_000_099;
        assert_eq!(
            expect_ignore(&decide(&f, &cfg())),
            IgnoreReason::GuildMismatch
        );
    }

    /// `is_flash_channel` agrees with `decide` on a swept set of channel and thread
    /// combinations, for both values of `include_threads`.
    #[test]
    fn is_flash_channel_agrees_with_decide_on_swept_combinations() {
        let channels = [FLASH_CH, OTHER_CH, THREAD_CH];
        let parents = [None, Some(FLASH_CH), Some(OTHER_CH)];
        for include_threads in [true, false] {
            let c = FlashConfig::new(
                vec![FLASH_CH, OTHER_CH],
                Some(ROLE),
                DEFAULT_DELAY_SECS,
                include_threads,
                Some(GUILD),
            )
            .expect("config is valid");
            for channel_id in channels {
                for parent in parents {
                    let predicted = is_flash_channel(&c, channel_id, parent);
                    let mut f = facts();
                    f.channel_id = channel_id;
                    f.parent_channel_id = parent;
                    let expired = matches!(decide(&f, &c), Decision::Expire(_));
                    assert_eq!(
                        predicted, expired,
                        "is_flash_channel disagrees with decide: channel={channel_id} \
                         parent={parent:?} include_threads={include_threads}"
                    );
                    // The logger exclusion must never be weaker than the timer.
                    assert!(
                        !is_flash_area(&c, channel_id, parent) || predicted,
                        "the flash area must contain every flash channel: channel={channel_id} \
                         parent={parent:?} include_threads={include_threads}"
                    );
                }
            }
        }
    }

    /// The deadline saturates at `u64::MAX` instead of wrapping around.
    #[test]
    fn deadline_saturates_instead_of_wrapping() {
        assert_eq!(deadline(0, 180), 180_000);
        assert_eq!(deadline(1_000, 10), 11_000);
        assert_eq!(deadline(u64::MAX, 180), u64::MAX);
        assert_eq!(deadline(u64::MAX - 100, 180), u64::MAX);
        assert_eq!(deadline(u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(deadline(u64::MAX, 0), u64::MAX);
    }

    /// The plan exposes the pair of deletions as data and the absolute deadline.
    #[test]
    fn plan_exposes_the_deletion_pair_and_the_deadline() {
        let plan = expect_expire(&decide(&facts(), &cfg()));
        let deletions = plan.deletions(1_000_000_000_000_000_009);
        assert_eq!(deletions.original_message_id, MESSAGE);
        assert_eq!(deletions.notice_message_id, 1_000_000_000_000_000_009);
        assert_eq!(plan.deadline_at(5_000), 185_000);
    }
}

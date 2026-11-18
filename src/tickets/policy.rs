//! Ticket configuration, ticket kinds, and the exact private-channel payload.
//!
//! This module is pure: it holds no state, performs no I/O, and never talks to Discord.
//! It owns the settings struct, the two ticket kinds, the owner-only publish check, the
//! channel-creation payload (permission overwrites + topic marker), and the greeting text.
//! It deliberately does NOT decide whether opening is allowed (see `tickets::runtime` for the
//! hard gate) and does NOT touch the database.
//!
//! Trust boundary: every value here comes from configuration or from Discord ids supplied by
//! the caller, so all of it is untrusted input. `valid()` and `owner_can_publish` are the
//! fail-closed checks; a caller that skips them must not treat the result as authorisation.

use serenity::all::{
    ChannelId, CreateChannel, PermissionOverwrite, PermissionOverwriteType, Permissions, RoleId,
    UserId,
};

/// Button custom id for the support ticket request.
pub const SUPPORT: &str = "ticket:v1:support";
/// Button custom id for the age-verification ticket request.
pub const AGE: &str = "ticket:v1:age";
/// The two ticket types the bot offers. The `kind` column of a ticket row stores one of
/// `kind.key()`, never the custom id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Support,
    Age,
}
impl Kind {
    /// Parses a button custom id into a kind. Returns `None` for anything else, including
    /// other versions of the custom-id scheme, so a stale or foreign button is rejected
    /// rather than guessed at.
    pub fn parse(id: &str) -> Option<Self> {
        match id {
            SUPPORT => Some(Self::Support),
            AGE => Some(Self::Age),
            _ => None,
        }
    }
    /// The stable storage key for this kind (`support` / `age`). This is what is written to
    /// the `tickets.kind` column and what the schema's CHECK constraint accepts.
    pub fn key(self) -> &'static str {
        match self {
            Self::Support => "support",
            Self::Age => "age",
        }
    }
}

/// Ticket feature configuration, read once from the environment.
///
/// `guild` and `staff` are Discord ids; `support_category` / `age_category` are the channel
/// ids of the two ticket categories; `db_path` is the SQLite file path (`:memory:` in tests).
#[derive(Clone, Debug)]
pub struct Settings {
    /// Guild the ticket feature belongs to. Events from other guilds are out of scope.
    pub guild: u64,
    /// Staff role id, used both as a permission overwrite and in capability validation.
    pub staff: u64,
    /// Channel id of the support ticket category.
    pub support_category: u64,
    /// Channel id of the age-verification ticket category.
    pub age_category: u64,
    /// Path of the ticket SQLite database.
    pub db_path: String,
}
impl Settings {
    /// True when the configuration is internally consistent.
    ///
    /// Requires every id to be non-zero, `staff` to differ from `guild` (the payload uses the
    /// guild id as the @everyone role id, so a staff role equal to the guild would target
    /// @everyone instead of the staff), the two categories to differ, and `db_path` to be
    /// non-blank. Returns false rather than repairing anything: callers must treat false as
    /// "tickets are not usable".
    pub fn valid(&self) -> bool {
        self.guild != 0
            && self.staff != 0
            && self.staff != self.guild
            && self.support_category != 0
            && self.age_category != 0
            && self.support_category != self.age_category
            && !self.db_path.trim().is_empty()
    }
    /// The category channel id that tickets of this kind are created under.
    pub fn category(&self, kind: Kind) -> u64 {
        match kind {
            Kind::Support => self.support_category,
            Kind::Age => self.age_category,
        }
    }
}

/// True only when the guild owner may publish the ticket panel into this channel.
///
/// All of the following must hold: the configuration is valid, the command came from the
/// configured guild, the target channel is in that same guild, the actor is a non-zero id,
/// the actor is the guild owner, and the target is a text channel (not a category or voice
/// channel). Any failed condition returns false and the caller must abort; this function
/// grants nothing on its own.
pub fn owner_can_publish(
    settings: &Settings,
    guild: u64,
    actor: u64,
    owner: u64,
    channel_guild: u64,
    text: bool,
) -> bool {
    settings.valid()
        && guild == settings.guild
        && channel_guild == guild
        && actor != 0
        && actor == owner
        && text
}

/// Builds the channel-creation payload for a new ticket channel.
///
/// The channel is created as a text channel named `<kind>-<ticket number>` under the kind's
/// category, with the operation marker as its topic. The permission overwrites are exactly
/// four: @everyone is denied view, and the requester, the staff role and the bot are each
/// granted view/send/history/attach/embed (the bot additionally gets channel management).
/// The caller supplies already-authorised ids; this function does no permission checking of
/// its own and does not contact Discord.
pub fn channel_payload(
    settings: &Settings,
    kind: Kind,
    requester: u64,
    bot: u64,
    number: i64,
    operation: &str,
) -> CreateChannel<'static> {
    // The minimum a participant needs to use the channel; nothing wider is granted.
    let access = Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::ATTACH_FILES
        | Permissions::EMBED_LINKS;
    CreateChannel::new(format!("{}-{number}", kind.key()))
        .kind(serenity::all::ChannelType::Text)
        .category(ChannelId::new(settings.category(kind)))
        .topic(marker(operation))
        .permissions(vec![
            // @everyone is denied view; without this overwrite the channel would be
            // visible to the whole guild.
            PermissionOverwrite {
                allow: Permissions::empty(),
                deny: Permissions::VIEW_CHANNEL,
                kind: PermissionOverwriteType::Role(RoleId::new(settings.guild)),
            },
            PermissionOverwrite {
                allow: access,
                deny: Permissions::empty(),
                kind: PermissionOverwriteType::Member(UserId::new(requester)),
            },
            PermissionOverwrite {
                allow: access,
                deny: Permissions::empty(),
                kind: PermissionOverwriteType::Role(RoleId::new(settings.staff)),
            },
            PermissionOverwrite {
                allow: access | Permissions::MANAGE_CHANNELS,
                deny: Permissions::empty(),
                kind: PermissionOverwriteType::Member(UserId::new(bot)),
            },
        ])
}

/// Builds the channel topic marker for a ticket operation.
///
/// The marker embeds the opaque per-ticket operation id, so a channel can later be matched
/// back to its ticket. It is a marker, not a secret: it identifies, it does not authorise.
pub fn marker(operation: &str) -> String {
    format!("ticket-operation:v1:{operation}")
}

/// The first message posted into a new ticket channel.
///
/// The age greeting asks for live camera-on voice verification and explicitly refuses
/// uploads of identification, pictures or date of birth. Neither greeting asks the user to
/// post personal data in the channel.
pub fn greeting(kind: Kind) -> &'static str {
    match kind { Kind::Support => "Please explain your issue; staff will respond here.", Kind::Age => "A staff member will arrange live camera-on voice verification. Do not upload identification, pictures, or date of birth here. Voice verification is not recorded." }
}
#[cfg(test)]
mod tests {
    use super::*;
    pub fn settings() -> Settings {
        Settings {
            guild: 100,
            staff: 200,
            support_category: 300,
            age_category: 400,
            db_path: "owned-test.sqlite".into(),
        }
    }
    /// Proves the publish check accepts only the guild owner publishing into a text channel
    /// of the configured guild. Fails if a precondition is dropped or staff==guild passes.
    #[test]
    fn owner_only_and_config_validation() {
        let mut s = settings();
        assert!(s.valid());
        assert!(owner_can_publish(&s, 100, 9, 9, 100, true));
        for args in [
            (100, 8, 9, 100, true),
            (101, 9, 9, 100, true),
            (100, 9, 9, 101, true),
            (100, 9, 9, 100, false),
        ] {
            assert!(!owner_can_publish(
                &s, args.0, args.1, args.2, args.3, args.4
            ));
        }
        for staff in [0, 100] {
            s.staff = staff;
            assert!(!s.valid());
        }
    }
    /// Proves the created channel lands under the right category with the marker topic and
    /// exactly four overwrites with the expected ids and types. Fails if any differs.
    #[test]
    fn private_payload_is_explicit_and_category_bound() {
        let s = settings();
        for kind in [Kind::Support, Kind::Age] {
            let v =
                serde_json::to_value(channel_payload(&s, kind, 500, 600, 7, "opaque-operation"))
                    .unwrap();
            assert_eq!(v["parent_id"], s.category(kind).to_string());
            assert_eq!(v["topic"], marker("opaque-operation"));
            let p = v["permission_overwrites"].as_array().unwrap();
            assert_eq!(p.len(), 4);
            for (i, id, typ) in [(0, 100, 0), (1, 500, 1), (2, 200, 0), (3, 600, 1)] {
                assert_eq!(p[i]["id"], id.to_string());
                assert_eq!(p[i]["type"], typ);
                let allow = p[i]["allow"].as_str().unwrap().parse::<u64>().unwrap();
                let deny = p[i]["deny"].as_str().unwrap().parse::<u64>().unwrap();
                if i == 0 {
                    assert_eq!(allow, 0);
                    assert_eq!(deny, Permissions::VIEW_CHANNEL.bits());
                } else {
                    assert!(Permissions::from_bits_truncate(allow).contains(
                        Permissions::VIEW_CHANNEL
                            | Permissions::SEND_MESSAGES
                            | Permissions::READ_MESSAGE_HISTORY
                    ));
                    assert_eq!(deny, 0);
                }
            }
        }
    }
}

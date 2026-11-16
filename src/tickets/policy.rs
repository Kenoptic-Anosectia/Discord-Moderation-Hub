use serenity::all::{
    ChannelId, CreateChannel, PermissionOverwrite, PermissionOverwriteType, Permissions, RoleId,
    UserId,
};

pub const SUPPORT: &str = "ticket:v1:support";
pub const AGE: &str = "ticket:v1:age";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Support,
    Age,
}
impl Kind {
    pub fn parse(id: &str) -> Option<Self> {
        match id {
            SUPPORT => Some(Self::Support),
            AGE => Some(Self::Age),
            _ => None,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::Support => "support",
            Self::Age => "age",
        }
    }
}
#[derive(Clone, Debug)]
pub struct Settings {
    pub guild: u64,
    pub staff: u64,
    pub support_category: u64,
    pub age_category: u64,
    pub db_path: String,
}
impl Settings {
    pub fn valid(&self) -> bool {
        self.guild != 0
            && self.staff != 0
            && self.staff != self.guild
            && self.support_category != 0
            && self.age_category != 0
            && self.support_category != self.age_category
            && !self.db_path.trim().is_empty()
    }
    pub fn category(&self, kind: Kind) -> u64 {
        match kind {
            Kind::Support => self.support_category,
            Kind::Age => self.age_category,
        }
    }
}
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
pub fn channel_payload(
    settings: &Settings,
    kind: Kind,
    requester: u64,
    bot: u64,
    number: i64,
    operation: &str,
) -> CreateChannel<'static> {
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
pub fn marker(operation: &str) -> String {
    format!("ticket-operation:v1:{operation}")
}
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

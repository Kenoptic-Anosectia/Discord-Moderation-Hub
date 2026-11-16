use std::env;

use crate::Error;

#[derive(Clone, Debug)]
pub struct Config {
    pub discord_token: String,
    pub log_channel_id: u64,
    pub age_role_1: u64,
    pub age_role_2: u64,
    pub reaction_role_db_path: String,
    pub staff_role_id: Option<u64>,
    pub tickets: Option<crate::tickets::policy::Settings>,
    pub ticket_privacy_lockdown: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, Error> {
        Ok(Self {
            discord_token: required_string("DISCORD_TOKEN")?,
            log_channel_id: required_u64("LOG_CHANNEL_ID")?,
            age_role_1: required_u64("AGE_ROLE_1")?,
            age_role_2: required_u64("AGE_ROLE_2")?,
            reaction_role_db_path: optional_string("REACTION_ROLE_DB_PATH")
                .unwrap_or_else(|| "reaction_roles.sqlite".to_string()),
            // Invalid staff configuration disables the command, not the logger.
            staff_role_id: parse_staff_role(optional_string("STAFF_ROLE_ID").as_deref()),
            tickets: ticket_settings(optional_string),
            ticket_privacy_lockdown: optional_string("TICKETS_ENABLED").as_deref() == Some("true"),
        })
    }
}

fn ticket_settings(
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<crate::tickets::policy::Settings> {
    if lookup("TICKETS_ENABLED").as_deref() != Some("true") {
        return None;
    }
    let id = |key| lookup(key)?.parse::<u64>().ok();
    let settings = crate::tickets::policy::Settings {
        guild: id("TICKET_GUILD_ID")?,
        staff: id("STAFF_ROLE_ID")?,
        support_category: id("TICKET_CATEGORY_ID")?,
        age_category: id("AGE_VERIFICATION_CATEGORY_ID")?,
        db_path: lookup("TICKET_DB_PATH")?,
    };
    settings.valid().then_some(settings)
}

fn required_string(key: &str) -> Result<String, Error> {
    let value =
        env::var(key).map_err(|_| format!("Missing required environment variable: {}", key))?;

    if value.trim().is_empty() {
        return Err(format!("Environment variable {} cannot be empty", key).into());
    }

    Ok(value)
}

fn optional_string(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn required_u64(key: &str) -> Result<u64, Error> {
    let raw = required_string(key)?;
    raw.parse::<u64>()
        .map_err(|_| format!("Environment variable {} must be a valid u64", key).into())
}

fn parse_staff_role(raw: Option<&str>) -> Option<u64> {
    raw.and_then(|value| value.parse().ok())
        .filter(|id| *id != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tickets_are_optional_default_off_and_require_complete_valid_settings() {
        let lookup = |key: &str| {
            Some(
                match key {
                    "TICKETS_ENABLED" => "true",
                    "TICKET_GUILD_ID" => "100",
                    "STAFF_ROLE_ID" => "200",
                    "TICKET_CATEGORY_ID" => "300",
                    "AGE_VERIFICATION_CATEGORY_ID" => "400",
                    "TICKET_DB_PATH" => "tickets-owned.sqlite",
                    _ => return None,
                }
                .to_string(),
            )
        };
        assert!(ticket_settings(|_| None).is_none());
        assert!(ticket_settings(lookup).is_some());
        for missing in [
            "TICKETS_ENABLED",
            "TICKET_GUILD_ID",
            "STAFF_ROLE_ID",
            "TICKET_CATEGORY_ID",
            "AGE_VERIFICATION_CATEGORY_ID",
            "TICKET_DB_PATH",
        ] {
            assert!(
                ticket_settings(|key| if key == missing { None } else { lookup(key) }).is_none()
            );
        }
        for (key, value) in [
            ("TICKETS_ENABLED", "false"),
            ("TICKETS_ENABLED", "oops"),
            ("STAFF_ROLE_ID", "100"),
            ("STAFF_ROLE_ID", "0"),
            ("TICKET_CATEGORY_ID", "bad"),
        ] {
            assert!(ticket_settings(|k| if k == key {
                Some(value.into())
            } else {
                lookup(k)
            })
            .is_none());
        }
    }

    #[test]
    fn unusable_staff_configuration_disables_only_the_age_command() {
        for raw in [
            None,
            Some(""),
            Some("not-a-role"),
            Some("-1"),
            Some("18446744073709551616"),
            Some("0"),
        ] {
            assert_eq!(parse_staff_role(raw), None, "{raw:?}");
        }
        assert_eq!(parse_staff_role(Some("200")), Some(200));
    }
}

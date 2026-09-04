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
    pub ticket_category_id: Option<u64>,
    pub age_verification_category_id: Option<u64>,
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
            staff_role_id: optional_u64("STAFF_ROLE_ID")?,
            ticket_category_id: optional_u64("TICKET_CATEGORY_ID")?,
            age_verification_category_id: optional_u64("AGE_VERIFICATION_CATEGORY_ID")?,
        })
    }
}

fn required_string(key: &str) -> Result<String, Error> {
    let value = env::var(key)
        .map_err(|_| format!("Missing required environment variable: {}", key))?;

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

fn optional_u64(key: &str) -> Result<Option<u64>, Error> {
    match optional_string(key) {
        Some(raw) => Ok(Some(
            raw.parse::<u64>()
                .map_err(|_| format!("Environment variable {} must be a valid u64", key))?,
        )),
        None => Ok(None),
    }
}
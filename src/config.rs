use std::fmt;
use std::path::{Path, PathBuf};
use std::{env, ffi::OsStr};

use crate::tickets::{
    policy::Settings,
    retention::{parse_retention, RetentionPolicy, RETENTION_DAYS_KEY},
};
use crate::Error;

/// Name of the environment file the bot looks for.
pub const ENV_FILE_NAME: &str = ".env";

/// Why the environment file could not be loaded.
///
/// The variants are stable codes: they never carry the offending value or any
/// other user supplied text, so the reason can be logged safely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvFileError {
    /// No environment file was found in the start directory or any of its parents.
    NotFound,
    /// An environment file was found but it could not be read or parsed.
    Unreadable,
}

impl fmt::Display for EnvFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(
                f,
                "no {} file was found in the working directory or any parent directory",
                ENV_FILE_NAME
            ),
            Self::Unreadable => write!(f, "the {} file could not be read", ENV_FILE_NAME),
        }
    }
}

impl std::error::Error for EnvFileError {}

/// Loads the environment file for the current working directory.
///
/// See [`load_env_from`] for the precedence rules.
pub fn load_env() -> Result<PathBuf, EnvFileError> {
    let start = env::current_dir().map_err(|_| EnvFileError::Unreadable)?;
    load_env_from(&start)
}

/// Loads the nearest environment file, starting at `start` and walking up to the
/// filesystem root.
///
/// The real process environment wins: a value that is already set in the
/// environment is never replaced by the file. Only variables that the
/// environment does not hold are taken from the file.
pub fn load_env_from(start: &Path) -> Result<PathBuf, EnvFileError> {
    let path = find_env_file(start).ok_or(EnvFileError::NotFound)?;
    dotenvy::from_path(&path).map_err(|_| EnvFileError::Unreadable)?;
    Ok(path)
}

/// Finds the nearest environment file, starting at `start` and walking up to the
/// filesystem root.
fn find_env_file(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(directory) = current {
        let candidate = directory.join(ENV_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = directory.parent();
    }
    None
}

#[derive(Clone)]
pub struct Config {
    pub discord_token: String,
    pub log_channel_id: u64,
    pub age_role_1: u64,
    pub age_role_2: u64,
    pub reaction_role_db_path: String,
    pub staff_role_id: Option<u64>,
    pub tickets: Option<Settings>,
    pub ticket_archive_retention: RetentionPolicy,
    pub ticket_privacy_lockdown: bool,
}

/// Placeholder printed instead of a secret value.
const REDACTED: &str = "<redacted>";

impl fmt::Debug for Config {
    /// Prints the configuration without its secrets.
    ///
    /// The bot token is the only credential in this struct; it is replaced by a
    /// placeholder so that a `{:?}` of the configuration can never put the token
    /// into a log line, a panic message or a bug report. Every other field is
    /// shown, so the output stays useful for diagnosing a misconfigured bot.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("discord_token", &REDACTED)
            .field("log_channel_id", &self.log_channel_id)
            .field("age_role_1", &self.age_role_1)
            .field("age_role_2", &self.age_role_2)
            .field("reaction_role_db_path", &self.reaction_role_db_path)
            .field("staff_role_id", &self.staff_role_id)
            .field("tickets", &self.tickets)
            .field("ticket_archive_retention", &self.ticket_archive_retention)
            .field("ticket_privacy_lockdown", &self.ticket_privacy_lockdown)
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Result<Self, Error> {
        let (tickets, ticket_archive_retention) =
            match ticket_configuration(optional_string, env::var_os(RETENTION_DAYS_KEY).as_deref())
            {
                Ok(configuration) => configuration,
                Err(message) => {
                    eprintln!("{message}");
                    (None, RetentionPolicy::Disabled)
                }
            };
        Ok(Self {
            discord_token: required_string("DISCORD_TOKEN")?,
            log_channel_id: required_discord_id("LOG_CHANNEL_ID")?,
            age_role_1: required_u64("AGE_ROLE_1")?,
            age_role_2: required_u64("AGE_ROLE_2")?,
            reaction_role_db_path: optional_string("REACTION_ROLE_DB_PATH")
                .unwrap_or_else(|| "reaction_roles.sqlite".to_string()),
            // Invalid staff configuration disables the command, not the logger.
            staff_role_id: parse_staff_role(optional_string("STAFF_ROLE_ID").as_deref()),
            tickets,
            ticket_archive_retention,
            ticket_privacy_lockdown: optional_string("TICKETS_ENABLED").as_deref() == Some("true"),
        })
    }
}

fn ticket_settings(lookup: impl Fn(&str) -> Option<String>) -> Option<Settings> {
    if lookup("TICKETS_ENABLED").as_deref() != Some("true") {
        return None;
    }
    let id = |key| lookup(key)?.parse::<u64>().ok();
    let settings = Settings {
        guild: id("TICKET_GUILD_ID")?,
        staff: id("STAFF_ROLE_ID")?,
        support_category: id("TICKET_CATEGORY_ID")?,
        age_category: id("AGE_VERIFICATION_CATEGORY_ID")?,
        db_path: lookup("TICKET_DB_PATH")?,
    };
    settings.valid().then_some(settings)
}

fn ticket_configuration(
    lookup: impl Fn(&str) -> Option<String>,
    raw_retention: Option<&OsStr>,
) -> Result<(Option<Settings>, RetentionPolicy), String> {
    if lookup("TICKETS_ENABLED").as_deref() != Some("true") {
        return Ok((None, RetentionPolicy::Disabled));
    }
    let invalid = || format!("Ticket settings unavailable: invalid {RETENTION_DAYS_KEY}.");
    // A non-Unicode value is invalid, not equivalent to an absent variable.
    let value = raw_retention
        .map(|raw| raw.to_str().ok_or_else(invalid))
        .transpose()?;
    let retention = parse_retention(value).map_err(|_| invalid())?;
    Ok((ticket_settings(lookup), retention))
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

/// Stable reason why a configuration value was rejected.
///
/// The variants are codes, not messages: they never carry the offending value
/// or any other text taken from the environment, so they are safe to log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigValueCode {
    /// The value is not a usable Discord id: it is not a number, it is negative
    /// or it is zero.
    InvalidDiscordId,
}

/// A configuration value that could not be used.
///
/// Carries the name of the variable and a stable code. It never carries the
/// rejected value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigValueError {
    /// Name of the environment variable that held the unusable value.
    pub key: &'static str,
    /// Stable code describing why the value was rejected.
    pub code: ConfigValueCode,
}

impl fmt::Display for ConfigValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            ConfigValueCode::InvalidDiscordId => {
                write!(
                    f,
                    "Environment variable {} is not a valid Discord id",
                    self.key
                )
            }
        }
    }
}

impl std::error::Error for ConfigValueError {}

/// Reads a required Discord id from the environment.
///
/// A Discord id is a snowflake: a non-zero unsigned 64 bit number. Zero is not
/// a usable id, so a variable set to zero is treated the same way as a missing
/// or unparsable one: the configuration does not load and the bot refuses to
/// start instead of pointing its logger at a channel that does not exist.
fn required_discord_id(key: &'static str) -> Result<u64, Error> {
    let raw = required_string(key)?;
    parse_discord_id(&raw).map_err(|code| Box::new(ConfigValueError { key, code }) as Error)
}

/// Parses a Discord id.
///
/// Zero is not a usable id: it is rejected with the same code as a value that
/// is not a number at all.
fn parse_discord_id(raw: &str) -> Result<u64, ConfigValueCode> {
    match raw.parse::<u64>() {
        Ok(id) if id != 0 => Ok(id),
        _ => Err(ConfigValueCode::InvalidDiscordId),
    }
}

fn parse_staff_role(raw: Option<&str>) -> Option<u64> {
    raw.and_then(|value| value.parse().ok())
        .filter(|id| *id != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Command, Output};

    /// Set on the child process so it knows it is the helper and not the driver.
    const CHILD_MARKER: &str = "SF3_CONFIG_TEST_CHILD";

    /// Synthetic variable that the driver exports and the environment file also sets.
    const REAL_ENV_SENTINEL: &str = "SF3_REAL_ENV_SENTINEL";

    /// Synthetic variable that only the environment file sets.
    const FILE_ONLY_SENTINEL: &str = "SF3_FILE_ONLY_SENTINEL";

    /// Scratch directory for this card's temporary files.
    fn scratch_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("t_86edb88e-tests")
    }

    fn is_child() -> bool {
        env::var_os(CHILD_MARKER).is_some()
    }

    /// Runs one test of this binary in a child process with `vars` in its environment.
    ///
    /// The child process is used because the environment is process global state:
    /// reading and changing it in a dedicated process keeps the other tests of this
    /// binary unaffected.
    fn run_child(name: &str, vars: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env::current_exe().expect("test binary path"));
        command.args(["--exact", name, "--nocapture", "--test-threads=1"]);
        command.env(CHILD_MARKER, "1");
        command.env_remove("LOG_CHANNEL_ID");
        command.env_remove("TICKETS_ENABLED");
        command.env_remove(RETENTION_DAYS_KEY);
        command.env_remove(REAL_ENV_SENTINEL);
        command.env_remove(FILE_ONLY_SENTINEL);
        for (key, value) in vars {
            command.env(key, value);
        }
        command.output().expect("child process runs")
    }

    /// Asserts the child ran the expected single test and that it passed.
    fn assert_child_ok(output: &Output, label: &str) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "child {label} failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed"),
            "child {label} did not run exactly one test:\n{stdout}\n{stderr}"
        );
    }

    /// The real process environment must win over the values in the environment file.
    ///
    /// A stale `.env` must not be able to replace a deliberately exported variable.
    #[test]
    fn env_file_does_not_override_the_real_environment() {
        const NAME: &str = "config::tests::env_file_does_not_override_the_real_environment";

        let directory = scratch_dir().join("env-precedence");
        fs::create_dir_all(directory.join("nested")).expect("scratch directory");
        let env_file = directory.join(ENV_FILE_NAME);
        fs::write(
            &env_file,
            format!("{REAL_ENV_SENTINEL}=from_file\n{FILE_ONLY_SENTINEL}=from_file\n"),
        )
        .expect("environment file");

        if is_child() {
            assert_eq!(
                env::var(REAL_ENV_SENTINEL).as_deref(),
                Ok("from_environment"),
                "the child must start with the exported value"
            );
            let loaded = load_env_from(&directory.join("nested")).expect("environment file loads");
            assert_eq!(
                loaded, env_file,
                "the nearest environment file above the start directory is used"
            );
            assert_eq!(
                env::var(REAL_ENV_SENTINEL).as_deref(),
                Ok("from_environment"),
                "the exported environment must win over the environment file"
            );
            assert_eq!(
                env::var(FILE_ONLY_SENTINEL).as_deref(),
                Ok("from_file"),
                "values that are not exported must still be loaded from the environment file"
            );
            return;
        }

        let output = run_child(NAME, &[(REAL_ENV_SENTINEL, "from_environment")]);
        assert_child_ok(&output, NAME);
    }

    /// `LOG_CHANNEL_ID` must be a real Discord id.
    ///
    /// Zero, negative and non numeric values are rejected the same way a missing
    /// value is, and only a real id lets the configuration load.
    #[test]
    fn log_channel_id_must_be_a_valid_discord_id() {
        const NAME: &str = "config::tests::log_channel_id_must_be_a_valid_discord_id";
        const EXPECT_OK: &str = "SF3_EXPECT_OK";
        const EXPECT_SET: &str = "SF3_EXPECT_SET";
        const VALID: u64 = 1234567890123456789;

        // label, value to export (None means the variable is not set), loads?
        let cases: [(&str, Option<&str>, bool); 5] = [
            ("zero", Some("0"), false),
            ("negative", Some("-1"), false),
            ("non numeric", Some("not-a-number"), false),
            ("not set", None, false),
            ("valid", Some("1234567890123456789"), true),
        ];

        if is_child() {
            let expect_ok = env::var(EXPECT_OK).as_deref() == Ok("1");
            let is_set = env::var(EXPECT_SET).as_deref() == Ok("1");
            match Config::from_env() {
                Ok(config) => {
                    assert!(expect_ok, "the configuration must not load");
                    assert_eq!(config.log_channel_id, VALID);
                }
                Err(err) => {
                    assert!(
                        !expect_ok,
                        "the configuration must load but it failed: {err}"
                    );
                    let message = err.to_string();
                    assert!(
                        message.contains("LOG_CHANNEL_ID"),
                        "the error must name the variable: {message}"
                    );
                    if is_set {
                        // A value that was set but is not usable carries a stable
                        // code, and never the value itself.
                        let value_error = err
                            .downcast_ref::<ConfigValueError>()
                            .expect("the error carries a stable code");
                        assert_eq!(value_error.key, "LOG_CHANNEL_ID");
                        assert_eq!(value_error.code, ConfigValueCode::InvalidDiscordId);
                    } else {
                        // A missing variable keeps the existing missing-variable
                        // failure, which is not a value rejection.
                        assert!(
                            err.downcast_ref::<ConfigValueError>().is_none(),
                            "a missing variable is not an invalid value: {message}"
                        );
                    }
                }
            }
            return;
        }

        for (label, value, expect_ok) in cases {
            let mut vars = vec![
                ("DISCORD_TOKEN", "synthetic-token"),
                ("AGE_ROLE_1", "111"),
                ("AGE_ROLE_2", "222"),
                (EXPECT_OK, if expect_ok { "1" } else { "0" }),
                (EXPECT_SET, if value.is_some() { "1" } else { "0" }),
            ];
            if let Some(value) = value {
                vars.push(("LOG_CHANNEL_ID", value));
            }
            let output = run_child(NAME, &vars);
            assert_child_ok(&output, &format!("{NAME} [{label}]"));
        }
    }

    /// The `Debug` output of the configuration must never contain the bot token.
    #[test]
    fn config_debug_output_never_contains_the_token() {
        const TOKEN: &str = "synthetic-token-must-never-be-printed";
        let config = Config {
            discord_token: TOKEN.to_string(),
            log_channel_id: 1234567890123456789,
            age_role_1: 111,
            age_role_2: 222,
            reaction_role_db_path: "reaction_roles.sqlite".to_string(),
            staff_role_id: Some(333),
            tickets: Some(Settings {
                guild: 100,
                staff: 200,
                support_category: 300,
                age_category: 400,
                db_path: "tickets.sqlite".to_string(),
            }),
            ticket_archive_retention: RetentionPolicy::AfterDays(30),
            ticket_privacy_lockdown: true,
        };
        let rendered = format!("{:?}", config);
        assert!(
            !rendered.contains(TOKEN),
            "the token must not be printed: {rendered}"
        );
        assert!(
            rendered.contains("1234567890123456789"),
            "values that are not secret stay visible: {rendered}"
        );
        assert!(
            rendered.contains(REDACTED),
            "the secret is replaced by a placeholder: {rendered}"
        );
    }

    /// A Discord id must be a non-zero number, and a rejected value must carry a
    /// stable code rather than the value itself.
    #[test]
    fn discord_id_rejects_zero_and_reports_a_stable_code() {
        for rejected in ["0", "-1", "not-a-number", ""] {
            let error = parse_discord_id(rejected).expect_err("the value is rejected");
            assert_eq!(error, ConfigValueCode::InvalidDiscordId);
            // The code never carries the rejected value.
            assert!(!format!("{error:?}").contains(rejected) || rejected.is_empty());
        }
        assert_eq!(
            parse_discord_id("1234567890123456789"),
            Ok(1234567890123456789)
        );
    }

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
    fn archive_command_retention_config_validates_and_redacts() {
        use crate::tickets::retention::{RetentionPolicy::*, RETENTION_DAYS_KEY};
        use std::ffi::OsStr;
        let lookup = |key: &str| {
            Some(
                match key {
                    "TICKETS_ENABLED" => "true",
                    "TICKET_GUILD_ID" => "100",
                    "STAFF_ROLE_ID" => "200",
                    "TICKET_CATEGORY_ID" => "300",
                    "AGE_VERIFICATION_CATEGORY_ID" => "400",
                    "TICKET_DB_PATH" => "tickets.sqlite",
                    _ => return None,
                }
                .into(),
            )
        };
        for (raw, expected) in [
            (None, Disabled),
            (Some(""), Disabled),
            (Some(" \t"), Disabled),
            (Some(" 30 "), AfterDays(30)),
            (Some("36500"), AfterDays(36500)),
        ] {
            let (settings, policy) = ticket_configuration(lookup, raw.map(OsStr::new)).unwrap();
            assert!(settings.is_some());
            assert_eq!(policy, expected, "{raw:?}");
        }
        for invalid in [
            "0",
            "-91",
            "sensitive-invalid",
            "36501",
            "18446744073709551616",
        ] {
            let message = ticket_configuration(lookup, Some(OsStr::new(invalid))).unwrap_err();
            assert!(message.contains(RETENTION_DAYS_KEY));
            assert!(!message.contains(invalid));
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

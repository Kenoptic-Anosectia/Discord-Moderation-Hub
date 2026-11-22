use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::{env, ffi::OsStr};

use crate::settings::parse_id_list;
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
/// The search is the one of `dotenvy`: the nearest file named `.env` at or
/// above the working directory, walking up to the root of the drive. The search
/// is not reimplemented here on purpose. A hand written search decides with
/// `Path::is_file`, which answers `false` for every error and therefore keeps
/// walking up when the metadata of a candidate cannot be read. That fails open:
/// the bot would silently load a different file, maybe the one of another bot.
/// `dotenvy` stops with an error in that case, and the loader reports it.
///
/// The real process environment wins: a value that is already set in the
/// environment is never replaced by the file. Only variables that the
/// environment does not hold are taken from the file.
pub fn load_env() -> Result<PathBuf, EnvFileError> {
    dotenvy::dotenv().map_err(env_file_error)
}

/// Turns a failure of the environment file search into a stable reason.
///
/// The reason is a code: it never carries the path of the file or any text
/// that came from it.
fn env_file_error(error: dotenvy::Error) -> EnvFileError {
    if error.not_found() {
        EnvFileError::NotFound
    } else {
        EnvFileError::Unreadable
    }
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
    /// Core database file (`CORE_DB_PATH`); `None` when the value is unusable.
    pub core_db_path: Option<String>,
    /// Optional guild filter for feature dispatch (`GUILD_ID`).
    pub guild_id: Option<u64>,
    /// Optional staff alert channel (`STAFF_ALERT_CHANNEL_ID`).
    pub staff_alert_channel_id: Option<u64>,
    /// Channels whose messages are never cached or logged (`LOG_EXCLUDED_CHANNEL_IDS`).
    pub log_excluded_channel_ids: Vec<u64>,
    /// Categories whose messages are never cached or logged (`LOG_EXCLUDED_CATEGORY_IDS`).
    pub log_excluded_category_ids: Vec<u64>,
    /// Roles automated paths must never grant (`PROTECTED_ROLE_IDS`).
    pub protected_role_ids: Vec<u64>,
    /// Keys of the new core settings whose value was unusable (names only).
    pub invalid_core_keys: Vec<&'static str>,
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
    ///
    /// The fields are taken apart without a rest pattern, so a field that is
    /// added later does not compile until it is listed here and a decision is
    /// made about whether it has to be hidden.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Config {
            // The token is listed so that the list stays complete, and it is
            // never bound to a name that could reach the output.
            discord_token: _,
            log_channel_id,
            age_role_1,
            age_role_2,
            reaction_role_db_path,
            staff_role_id,
            tickets,
            ticket_archive_retention,
            ticket_privacy_lockdown,
            core_db_path,
            guild_id,
            staff_alert_channel_id,
            log_excluded_channel_ids,
            log_excluded_category_ids,
            protected_role_ids,
            invalid_core_keys,
        } = self;
        f.debug_struct("Config")
            .field("discord_token", &REDACTED)
            .field("log_channel_id", log_channel_id)
            .field("age_role_1", age_role_1)
            .field("age_role_2", age_role_2)
            .field("reaction_role_db_path", reaction_role_db_path)
            .field("staff_role_id", staff_role_id)
            .field("tickets", tickets)
            .field("ticket_archive_retention", ticket_archive_retention)
            .field("ticket_privacy_lockdown", ticket_privacy_lockdown)
            .field("core_db_path", core_db_path)
            .field("guild_id", guild_id)
            .field("staff_alert_channel_id", staff_alert_channel_id)
            .field("log_excluded_channel_ids", log_excluded_channel_ids)
            .field("log_excluded_category_ids", log_excluded_category_ids)
            .field("protected_role_ids", protected_role_ids)
            .field("invalid_core_keys", invalid_core_keys)
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
        let core = core_settings(|key| env::var_os(key));
        for message in core.messages() {
            eprintln!("{message}");
        }
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
            core_db_path: core.core_db_path,
            guild_id: core.guild_id,
            staff_alert_channel_id: core.staff_alert_channel_id,
            log_excluded_channel_ids: core.log_excluded_channel_ids,
            log_excluded_category_ids: core.log_excluded_category_ids,
            protected_role_ids: core.protected_role_ids,
            invalid_core_keys: core.invalid_keys,
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

/// Key of the core database path setting.
pub const CORE_DB_PATH_KEY: &str = "CORE_DB_PATH";
/// Key of the optional guild filter.
pub const GUILD_ID_KEY: &str = "GUILD_ID";
/// Key of the optional staff alert channel.
pub const STAFF_ALERT_CHANNEL_ID_KEY: &str = "STAFF_ALERT_CHANNEL_ID";
/// Key of the excluded channel id list.
pub const LOG_EXCLUDED_CHANNEL_IDS_KEY: &str = "LOG_EXCLUDED_CHANNEL_IDS";
/// Key of the excluded category id list.
pub const LOG_EXCLUDED_CATEGORY_IDS_KEY: &str = "LOG_EXCLUDED_CATEGORY_IDS";
/// Key of the protected role id list.
pub const PROTECTED_ROLE_IDS_KEY: &str = "PROTECTED_ROLE_IDS";
/// Default core database file.
pub const DEFAULT_CORE_DB_PATH: &str = "core.sqlite";

/// The six core settings after parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreSettings {
    pub core_db_path: Option<String>,
    pub guild_id: Option<u64>,
    pub staff_alert_channel_id: Option<u64>,
    pub log_excluded_channel_ids: Vec<u64>,
    pub log_excluded_category_ids: Vec<u64>,
    pub protected_role_ids: Vec<u64>,
    pub invalid_keys: Vec<&'static str>,
}

impl CoreSettings {
    /// Startup lines for every unusable setting. Each names the key and what
    /// stays off; none carries the value.
    pub fn messages(&self) -> Vec<String> {
        self.invalid_keys
            .iter()
            .map(|key| {
                let effect = match *key {
                    CORE_DB_PATH_KEY => "the core database and the features that need it stay off",
                    GUILD_ID_KEY => "optional features stay off until it is fixed",
                    LOG_EXCLUDED_CHANNEL_IDS_KEY | LOG_EXCLUDED_CATEGORY_IDS_KEY => {
                        "guild message caching and edit/delete logging are paused until it is fixed"
                    }
                    _ => "the setting is switched off",
                };
                format!("Setting {key} is unusable: {effect}.")
            })
            .collect()
    }
}

/// How one environment value reads before it is parsed.
enum Raw {
    /// Not set, or only whitespace.
    Unset,
    Text(String),
    /// Set, but not valid Unicode.
    NotUnicode,
}

fn read_raw(lookup: &impl Fn(&str) -> Option<OsString>, key: &str) -> Raw {
    match lookup(key) {
        None => Raw::Unset,
        Some(value) => match value.into_string() {
            Ok(text) if text.trim().is_empty() => Raw::Unset,
            Ok(text) => Raw::Text(text),
            Err(_) => Raw::NotUnicode,
        },
    }
}

/// Longest accepted `CORE_DB_PATH`, in bytes.
const MAX_CORE_DB_PATH_BYTES: usize = 1024;

/// A path is usable only when it is plainly a file path: no control
/// character, no `file:` URI, not the in-memory name, and not absurdly long.
/// The storage layer refuses the rest (reserved, UNC, stream names) at open.
fn plain_core_db_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_CORE_DB_PATH_BYTES
        && !path.chars().any(char::is_control)
        && path != ":memory:"
        && !path.to_ascii_lowercase().starts_with("file:")
}

/// One Discord id. A list (`1,2`) is not one id, even when it dedupes to one.
fn single_id(key: &str, text: &str) -> Option<u64> {
    if text.contains(',') {
        return None;
    }
    match parse_id_list(key, Some(text)).ok()?.as_slice() {
        [id] => Some(*id),
        _ => None,
    }
}

/// Parses the six core settings.
///
/// A blank value counts as unset. A value that cannot be used switches only
/// that setting off and its key is listed in `invalid_keys`; the value itself
/// is not kept anywhere.
pub fn core_settings(lookup: impl Fn(&str) -> Option<OsString>) -> CoreSettings {
    let mut invalid_keys: Vec<&'static str> = Vec::new();

    let core_db_path = match read_raw(&lookup, CORE_DB_PATH_KEY) {
        Raw::Unset => Some(DEFAULT_CORE_DB_PATH.to_string()),
        Raw::Text(text) if plain_core_db_path(text.trim()) => Some(text.trim().to_string()),
        Raw::Text(_) | Raw::NotUnicode => {
            invalid_keys.push(CORE_DB_PATH_KEY);
            None
        }
    };

    let mut one_id = |key: &'static str| match read_raw(&lookup, key) {
        Raw::Unset => None,
        Raw::Text(text) => {
            let id = single_id(key, text.trim());
            if id.is_none() {
                invalid_keys.push(key);
            }
            id
        }
        Raw::NotUnicode => {
            invalid_keys.push(key);
            None
        }
    };
    let guild_id = one_id(GUILD_ID_KEY);
    let staff_alert_channel_id = one_id(STAFF_ALERT_CHANNEL_ID_KEY);

    let mut id_list = |key: &'static str| match read_raw(&lookup, key) {
        Raw::Unset => Vec::new(),
        Raw::Text(text) => parse_id_list(key, Some(&text)).unwrap_or_else(|_| {
            invalid_keys.push(key);
            Vec::new()
        }),
        Raw::NotUnicode => {
            invalid_keys.push(key);
            Vec::new()
        }
    };
    let log_excluded_channel_ids = id_list(LOG_EXCLUDED_CHANNEL_IDS_KEY);
    let log_excluded_category_ids = id_list(LOG_EXCLUDED_CATEGORY_IDS_KEY);
    let protected_role_ids = id_list(PROTECTED_ROLE_IDS_KEY);

    CoreSettings {
        core_db_path,
        guild_id,
        staff_alert_channel_id,
        log_excluded_channel_ids,
        log_excluded_category_ids,
        protected_role_ids,
        invalid_keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
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

    /// Scratch directory of a single test.
    ///
    /// Every test gets its own subtree, so the environment file of one test can
    /// never be found by the search of another test.
    fn scratch_dir_of(test: &str) -> PathBuf {
        scratch_dir().join(test)
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
        run_child_in(name, None, vars)
    }

    /// Runs one test of this binary in a child process that starts in `directory`.
    ///
    /// The working directory decides where the environment file is searched for,
    /// so the child is the only way to exercise the search from a known place.
    fn run_child_in(name: &str, directory: Option<&Path>, vars: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env::current_exe().expect("test binary path"));
        command.args(["--exact", name, "--nocapture", "--test-threads=1"]);
        command.env(CHILD_MARKER, "1");
        command.env_remove("LOG_CHANNEL_ID");
        command.env_remove("TICKETS_ENABLED");
        command.env_remove(RETENTION_DAYS_KEY);
        command.env_remove(REAL_ENV_SENTINEL);
        command.env_remove(FILE_ONLY_SENTINEL);
        if let Some(directory) = directory {
            command.current_dir(directory);
        }
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

        let directory = scratch_dir_of("env-precedence");
        let nested = directory.join("nested");
        fs::create_dir_all(&nested).expect("scratch directory");
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
            let loaded = load_env().expect("environment file loads");
            assert_eq!(
                loaded, env_file,
                "the nearest environment file above the working directory is used"
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

        let output = run_child_in(
            NAME,
            Some(&nested),
            &[(REAL_ENV_SENTINEL, "from_environment")],
        );
        assert_child_ok(&output, NAME);
    }

    /// The environment file is searched for from the working directory, walking
    /// up to the nearest one, and the loader must agree with dotenvy about which
    /// file that is.
    ///
    /// The search itself belongs to dotenvy: the loader must not reimplement it,
    /// because a hand written search can fail open when the metadata of a
    /// candidate cannot be read.
    #[test]
    fn load_env_finds_the_nearest_file_above_the_working_directory() {
        const NAME: &str =
            "config::tests::load_env_finds_the_nearest_file_above_the_working_directory";

        let directory = scratch_dir_of("env-search");
        let nested = directory.join("nested");
        fs::create_dir_all(&nested).expect("scratch directory");
        let env_file = directory.join(ENV_FILE_NAME);
        fs::write(&env_file, format!("{FILE_ONLY_SENTINEL}=from_file\n"))
            .expect("environment file");

        if is_child() {
            let loaded = load_env().expect("environment file loads");
            let expected = dotenvy::dotenv().expect("dotenvy finds the same file");
            assert_eq!(
                loaded, expected,
                "the loader must use the file dotenvy finds"
            );
            assert_eq!(
                loaded, env_file,
                "the nearest environment file above the working directory is used"
            );
            assert_eq!(
                env::var(FILE_ONLY_SENTINEL).as_deref(),
                Ok("from_file"),
                "the values of the environment file are loaded"
            );
            return;
        }

        let output = run_child_in(NAME, Some(&nested), &[]);
        assert_child_ok(&output, NAME);
    }

    /// A broken environment file must stop the load instead of letting another
    /// file take its place.
    ///
    /// The nearest file is unusable, so the loader must report the failure. It
    /// must not keep walking up and load a different file: a file that cannot be
    /// read is a failure, not a reason to look somewhere else.
    #[test]
    fn load_env_fails_closed_when_the_environment_file_cannot_be_used() {
        const NAME: &str =
            "config::tests::load_env_fails_closed_when_the_environment_file_cannot_be_used";

        let directory = scratch_dir_of("env-broken");
        let tree = directory.join("tree");
        let nested = tree.join("nested");
        fs::create_dir_all(&nested).expect("scratch directory");
        // The nearest file cannot be parsed.
        fs::write(tree.join(ENV_FILE_NAME), "this line has no equals sign\n")
            .expect("environment file");
        // A usable file further up must not be used instead.
        fs::write(
            directory.join(ENV_FILE_NAME),
            format!("{FILE_ONLY_SENTINEL}=from_parent\n"),
        )
        .expect("environment file");

        if is_child() {
            let error = load_env().expect_err("the broken environment file must fail");
            assert_eq!(error, EnvFileError::Unreadable);
            assert!(
                env::var(FILE_ONLY_SENTINEL).is_err(),
                "no environment file may be loaded when the nearest one is broken"
            );
            return;
        }

        let output = run_child_in(NAME, Some(&nested), &[]);
        assert_child_ok(&output, NAME);
    }

    /// Without an environment file the loader must say so, and it must not
    /// invent a path.
    ///
    /// The search walks up to the root of the drive, so the machine may hold an
    /// environment file above the scratch tree. The reason the loader reports
    /// is therefore pinned where it is decided: the mapping of the error that
    /// the search reports when it finds nothing.
    #[test]
    fn a_missing_environment_file_is_reported_as_not_found() {
        let missing = dotenvy::Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "path not found",
        ));
        assert_eq!(env_file_error(missing), EnvFileError::NotFound);
    }

    /// Any other failure of the search is an unreadable file, not a missing one.
    #[test]
    fn an_unreadable_environment_file_is_reported_as_unreadable() {
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidData,
        ] {
            let error = dotenvy::Error::Io(std::io::Error::new(kind, "cannot read"));
            assert_eq!(env_file_error(error), EnvFileError::Unreadable);
        }
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
            core_db_path: Some("core.sqlite".to_string()),
            guild_id: None,
            staff_alert_channel_id: None,
            log_excluded_channel_ids: Vec::new(),
            log_excluded_category_ids: Vec::new(),
            protected_role_ids: Vec::new(),
            invalid_core_keys: Vec::new(),
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
        for rejected in [
            "0",
            "-1",
            "not-a-number",
            "",
            " 123",
            "123 ",
            "18446744073709551616",
        ] {
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

    /// The `Debug` output must mention every field of the configuration.
    ///
    /// The manual implementation lists the fields one by one, so a field that
    /// is added later shows up here as a missing entry instead of being
    /// silently dropped from the output.
    #[test]
    fn config_debug_output_lists_every_field() {
        let config = Config {
            discord_token: "synthetic-token".to_string(),
            log_channel_id: 1234567890123456789,
            age_role_1: 111,
            age_role_2: 222,
            reaction_role_db_path: "reaction_roles.sqlite".to_string(),
            staff_role_id: Some(333),
            tickets: None,
            ticket_archive_retention: RetentionPolicy::Disabled,
            ticket_privacy_lockdown: false,
            core_db_path: Some("core.sqlite".to_string()),
            guild_id: None,
            staff_alert_channel_id: None,
            log_excluded_channel_ids: Vec::new(),
            log_excluded_category_ids: Vec::new(),
            protected_role_ids: Vec::new(),
            invalid_core_keys: Vec::new(),
        };
        let rendered = format!("{config:?}");
        for field in [
            "discord_token",
            "log_channel_id",
            "age_role_1",
            "age_role_2",
            "reaction_role_db_path",
            "staff_role_id",
            "tickets",
            "ticket_archive_retention",
            "ticket_privacy_lockdown",
            "core_db_path",
            "guild_id",
            "staff_alert_channel_id",
            "log_excluded_channel_ids",
            "log_excluded_category_ids",
            "protected_role_ids",
            "invalid_core_keys",
        ] {
            assert!(
                rendered.contains(field),
                "the output must mention {field}: {rendered}"
            );
        }
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

    // ---- W1 core settings -------------------------------------------------

    /// Builds a lookup from literal (key, value) pairs.
    fn pairs(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(*v))
        }
    }

    #[test]
    fn core_settings_all_unset_is_the_documented_default() {
        let parsed = core_settings(|_| None);
        assert_eq!(parsed.core_db_path.as_deref(), Some("core.sqlite"));
        assert_eq!(parsed.guild_id, None);
        assert_eq!(parsed.staff_alert_channel_id, None);
        assert!(parsed.log_excluded_channel_ids.is_empty());
        assert!(parsed.log_excluded_category_ids.is_empty());
        assert!(parsed.protected_role_ids.is_empty());
        assert!(parsed.invalid_keys.is_empty());
        assert!(
            parsed.messages().is_empty(),
            "an unset configuration prints nothing"
        );
    }

    #[test]
    fn core_settings_blank_values_count_as_unset() {
        for blank in ["", "   ", "\t"] {
            let parsed = core_settings(|_| Some(OsString::from(blank)));
            assert_eq!(parsed.core_db_path.as_deref(), Some("core.sqlite"));
            assert_eq!(parsed.guild_id, None);
            assert!(parsed.invalid_keys.is_empty(), "{blank:?}");
            assert!(parsed.messages().is_empty(), "{blank:?}");
        }
    }

    #[test]
    fn core_settings_parse_valid_values() {
        let parsed = core_settings(pairs(&[
            ("CORE_DB_PATH", "data/core-test.sqlite"),
            ("GUILD_ID", "700001"),
            ("STAFF_ALERT_CHANNEL_ID", "700002"),
            ("LOG_EXCLUDED_CHANNEL_IDS", "30, 10,20,10"),
            ("LOG_EXCLUDED_CATEGORY_IDS", "50"),
            ("PROTECTED_ROLE_IDS", "9,8"),
        ]));
        assert_eq!(
            parsed.core_db_path.as_deref(),
            Some("data/core-test.sqlite")
        );
        assert_eq!(parsed.guild_id, Some(700001));
        assert_eq!(parsed.staff_alert_channel_id, Some(700002));
        assert_eq!(parsed.log_excluded_channel_ids, vec![10, 20, 30]);
        assert_eq!(parsed.log_excluded_category_ids, vec![50]);
        assert_eq!(parsed.protected_role_ids, vec![8, 9]);
        assert!(parsed.invalid_keys.is_empty());
        assert!(parsed.messages().is_empty());
    }

    #[test]
    fn bad_single_ids_switch_the_setting_off_and_name_only_the_key() {
        // GUILD_ID and STAFF_ALERT_CHANNEL_ID are single ids: a list, zero, a
        // sign, a Unicode digit or an overflow are all unusable.
        for bad in [
            "0",
            "-1",
            "+5",
            "12x",
            "1,2",
            "5,5",
            "18446744073709551616",
            "١٢٣",
            "S3CR3T-W1-VALUE",
        ] {
            let leaked: &'static str = Box::leak(bad.to_string().into_boxed_str());
            for (key, pick) in [(GUILD_ID_KEY, 0usize), (STAFF_ALERT_CHANNEL_ID_KEY, 1usize)] {
                let parsed = core_settings(move |k| (k == key).then(|| OsString::from(leaked)));
                let value = [parsed.guild_id, parsed.staff_alert_channel_id][pick];
                assert_eq!(value, None, "{key}={bad:?} must switch off");
                assert_eq!(parsed.invalid_keys, vec![key], "{key}={bad:?}");
                let messages = parsed.messages();
                assert_eq!(messages.len(), 1, "{key}={bad:?}");
                assert!(messages[0].contains(key), "{}", messages[0]);
                assert!(!messages[0].contains(bad), "value leaked: {}", messages[0]);
            }
        }
    }

    #[test]
    fn bad_id_lists_switch_the_setting_off_and_name_only_the_key() {
        // One bad entry makes the whole list unusable (K06: never drop it).
        for bad in ["10,,20", "10,0", "10,x", "-5", "10,S3CR3T-W1-VALUE", ","] {
            for key in [
                LOG_EXCLUDED_CHANNEL_IDS_KEY,
                LOG_EXCLUDED_CATEGORY_IDS_KEY,
                PROTECTED_ROLE_IDS_KEY,
            ] {
                let leaked: &'static str = Box::leak(bad.to_string().into_boxed_str());
                let parsed = core_settings(move |k| (k == key).then(|| OsString::from(leaked)));
                assert!(parsed.log_excluded_channel_ids.is_empty(), "{key}={bad:?}");
                assert!(parsed.log_excluded_category_ids.is_empty(), "{key}={bad:?}");
                assert!(parsed.protected_role_ids.is_empty(), "{key}={bad:?}");
                assert_eq!(parsed.invalid_keys, vec![key], "{key}={bad:?}");
                let messages = parsed.messages();
                assert_eq!(messages.len(), 1);
                assert!(messages[0].contains(key), "{}", messages[0]);
                assert!(!messages[0].contains(bad), "value leaked: {}", messages[0]);
            }
        }
    }

    #[test]
    fn bad_core_paths_switch_the_database_off_and_name_only_the_key() {
        let long = "p".repeat(1025);
        let leaked_long: &'static str = Box::leak(long.into_boxed_str());
        for bad in [
            "file:core.sqlite",
            "FILE:core.sqlite?mode=memory",
            ":memory:",
            "core\n.sqlite",
            "core\0.sqlite",
            "S3CR3T-W1-VALUE\u{7}",
            leaked_long,
        ] {
            let leaked: &'static str = Box::leak(bad.to_string().into_boxed_str());
            let parsed =
                core_settings(move |k| (k == CORE_DB_PATH_KEY).then(|| OsString::from(leaked)));
            assert_eq!(parsed.core_db_path, None, "{:?}", bad.len());
            assert_eq!(parsed.invalid_keys, vec![CORE_DB_PATH_KEY]);
            let messages = parsed.messages();
            assert_eq!(messages.len(), 1);
            assert!(messages[0].contains(CORE_DB_PATH_KEY));
            assert!(!messages[0].contains("S3CR3T"), "{}", messages[0]);
            assert!(!messages[0].contains("sqlite"), "{}", messages[0]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_non_unicode_value_is_unusable_not_unset() {
        use std::os::windows::ffi::OsStringExt;
        let raw = OsString::from_wide(&[0x0063, 0xD800, 0x0064]);
        for key in [CORE_DB_PATH_KEY, GUILD_ID_KEY, LOG_EXCLUDED_CHANNEL_IDS_KEY] {
            let raw = raw.clone();
            let parsed = core_settings(move |k| (k == key).then(|| raw.clone()));
            assert_eq!(parsed.invalid_keys, vec![key]);
        }
    }

    #[test]
    fn one_unusable_key_does_not_disturb_the_others() {
        let parsed = core_settings(pairs(&[
            ("GUILD_ID", "not-an-id"),
            ("STAFF_ALERT_CHANNEL_ID", "700002"),
            ("LOG_EXCLUDED_CHANNEL_IDS", "10,20"),
            ("CORE_DB_PATH", "data/core.sqlite"),
        ]));
        assert_eq!(parsed.guild_id, None);
        assert_eq!(parsed.staff_alert_channel_id, Some(700002));
        assert_eq!(parsed.log_excluded_channel_ids, vec![10, 20]);
        assert_eq!(parsed.core_db_path.as_deref(), Some("data/core.sqlite"));
        assert_eq!(parsed.invalid_keys, vec![GUILD_ID_KEY]);
    }

    #[test]
    fn every_unusable_key_gets_exactly_one_message_in_a_fixed_order() {
        let parsed = core_settings(pairs(&[
            ("PROTECTED_ROLE_IDS", "x"),
            ("LOG_EXCLUDED_CATEGORY_IDS", "x"),
            ("LOG_EXCLUDED_CHANNEL_IDS", "x"),
            ("STAFF_ALERT_CHANNEL_ID", "x"),
            ("GUILD_ID", "x"),
            ("CORE_DB_PATH", ":memory:"),
        ]));
        assert_eq!(
            parsed.invalid_keys,
            vec![
                CORE_DB_PATH_KEY,
                GUILD_ID_KEY,
                STAFF_ALERT_CHANNEL_ID_KEY,
                LOG_EXCLUDED_CHANNEL_IDS_KEY,
                LOG_EXCLUDED_CATEGORY_IDS_KEY,
                PROTECTED_ROLE_IDS_KEY,
            ]
        );
        assert_eq!(parsed.messages().len(), 6);
    }

    #[test]
    fn debug_of_config_never_prints_the_token_with_the_new_fields_set() {
        const TOKEN: &str = "synthetic-w1-token-must-not-print";
        let config = Config {
            discord_token: TOKEN.to_string(),
            log_channel_id: 700_010,
            age_role_1: 1,
            age_role_2: 2,
            reaction_role_db_path: "reaction_roles.sqlite".to_string(),
            staff_role_id: None,
            tickets: None,
            ticket_archive_retention: RetentionPolicy::Disabled,
            ticket_privacy_lockdown: false,
            core_db_path: Some("core-test.sqlite".to_string()),
            guild_id: Some(700_001),
            staff_alert_channel_id: Some(700_002),
            log_excluded_channel_ids: vec![700_003],
            log_excluded_category_ids: vec![700_004],
            protected_role_ids: vec![700_005],
            invalid_core_keys: vec![GUILD_ID_KEY],
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains(TOKEN), "{rendered}");
        for shown in [
            "core-test.sqlite",
            "700001",
            "700002",
            "700003",
            "700004",
            "700005",
            "GUILD_ID",
        ] {
            assert!(rendered.contains(shown), "{shown} missing: {rendered}");
        }
    }
}

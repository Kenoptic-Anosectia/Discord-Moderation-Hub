//! Activity settings parsed from the wiring lane's lookup, not the environment.
use super::policy::{
    ActivityConfig, KEY_AFK_CHANNEL_ID, KEY_COUNT_SELF_DEAFENED, KEY_EXCLUDED_CHANNEL_IDS, KEY_MODE,
};
use crate::settings::{parse_id_list, parse_mode, Mode, SettingError};

#[derive(Debug, Clone)]
pub struct Settings {
    pub(super) mode: Mode,
    pub(super) home: Option<u64>,
    pub(super) policy: ActivityConfig,
}

impl Settings {
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn home_guild(&self) -> Option<u64> {
        self.home
    }
}

/// A refused key returns no settings; the caller must leave activity off.
pub fn parse(
    lookup: impl Fn(&str) -> Option<String>,
    home: Option<u64>,
) -> Result<Settings, SettingError> {
    let invalid = |key: &str| SettingError::InvalidValue(key.into());
    let mode = parse_mode(KEY_MODE, lookup(KEY_MODE).as_deref())?;
    let home = home.filter(|id| *id != 0);
    if mode == Mode::Off && home.is_none() {
        return Ok(Settings {
            mode,
            home,
            policy: ActivityConfig::new(vec![], None, false).map_err(|e| invalid(e.key()))?,
        });
    }
    let excluded = parse_id_list(
        KEY_EXCLUDED_CHANNEL_IDS,
        lookup(KEY_EXCLUDED_CHANNEL_IDS).as_deref(),
    )
    .map_err(|_| invalid(KEY_EXCLUDED_CHANNEL_IDS))?;
    let afk = lookup(KEY_AFK_CHANNEL_ID);
    if afk.as_deref().is_some_and(|s| s.contains(',')) {
        return Err(invalid(KEY_AFK_CHANNEL_ID));
    }
    let afk = parse_id_list(KEY_AFK_CHANNEL_ID, afk.as_deref())
        .map_err(|_| invalid(KEY_AFK_CHANNEL_ID))?
        .first()
        .copied();
    let deafened = lookup(KEY_COUNT_SELF_DEAFENED);
    let count = match deafened.as_deref().map(str::trim).unwrap_or("") {
        "" | "false" => false,
        "true" => true,
        _ => return Err(invalid(KEY_COUNT_SELF_DEAFENED)),
    };
    Ok(Settings {
        mode: if home.is_some() { mode } else { Mode::Off },
        home,
        policy: ActivityConfig::new(excluded, afk, count).map_err(|e| invalid(e.key()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ac2_config_parses_and_fails_closed() {
        let defaults = parse(|_| None, Some(1)).unwrap();
        assert_eq!(defaults.mode, Mode::Off);
        assert!(!defaults.policy.count_self_deafened());
        assert_eq!(
            parse(|_| Some("typo".into()), None).unwrap().mode,
            Mode::Off
        );
        let cfg = parse(
            |key| match key {
                KEY_MODE => Some(" Live ".into()),
                KEY_EXCLUDED_CHANNEL_IDS => Some("4,3,4".into()),
                KEY_AFK_CHANNEL_ID => Some("5".into()),
                KEY_COUNT_SELF_DEAFENED => Some("true".into()),
                _ => None,
            },
            Some(1),
        )
        .unwrap();
        assert_eq!(cfg.mode, Mode::Live);
        assert_eq!(cfg.home, Some(1));
        assert_eq!(cfg.policy.excluded_channels(), &[3, 4]);
        assert_eq!(cfg.policy.afk_channel(), Some(5));
        assert!(cfg.policy.count_self_deafened());
        for (key, value) in [
            (KEY_EXCLUDED_CHANNEL_IDS, "3,,4"),
            (KEY_AFK_CHANNEL_ID, "5,5"),
            (KEY_AFK_CHANNEL_ID, "0"),
            (KEY_COUNT_SELF_DEAFENED, "invalid"),
        ] {
            assert_eq!(
                parse(|k| (k == key).then(|| value.into()), Some(1))
                    .unwrap_err()
                    .to_string(),
                format!("{key}: invalid value")
            );
        }
    }
}

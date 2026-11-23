//! Activity settings parsed from the wiring lane's lookup, not the environment.
use super::policy::ActivityConfig;
use crate::settings::{Mode, SettingError};

#[derive(Debug, Clone)]
pub struct Settings {
    pub(super) mode: Mode,
    pub(super) home: Option<u64>,
    pub(super) policy: ActivityConfig,
}

pub fn parse(_: impl Fn(&str) -> Option<String>, _: Option<u64>) -> Result<Settings, SettingError> {
    Ok(Settings {
        mode: Mode::Off,
        home: None,
        policy: ActivityConfig::new(vec![], None, false).unwrap(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::policy::*;

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

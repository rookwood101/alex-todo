//! Board configuration (task 170).
//!
//! A small JSON file in the board home holds the few settings a user can
//! change. Loading never panics: a missing file means defaults, and a bad
//! value falls back to its default with a warning the caller can log or show.
//! Nothing here reads the clock or opens the board.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::platform::Hotkey;
use crate::store::StorePaths;
use crate::time;

/// File name inside the board home.
pub const CONFIG_FILE_NAME: &str = "config.json";

/// Which Pi tool profile the app starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Board,
    Power,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::Power => "power",
        }
    }
}

/// The on-disk settings, with defaults for every field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Two-key chord such as "Ctrl+Space".
    pub hotkey: String,
    /// Explicit database path; None means the board home default.
    pub board_db: Option<PathBuf>,
    /// IANA zone for new deadlines; None means the system zone.
    pub tz: Option<String>,
    /// Initial tool profile.
    pub pi_profile: Profile,
    /// Reminder poll interval.
    pub reminder_minutes: u64,
    /// Hide done notes that have no open descendants.
    pub hide_done: bool,
    /// Show the capture syntax help above the focused composer.
    pub show_syntax_help: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "Ctrl+Space".to_owned(),
            board_db: None,
            tz: None,
            pi_profile: Profile::Board,
            reminder_minutes: 30,
            hide_done: false,
            show_syntax_help: true,
        }
    }
}

impl Config {
    /// Where the config lives for a board home.
    pub fn path_for(paths: &StorePaths) -> PathBuf {
        paths.home().join(CONFIG_FILE_NAME)
    }

    /// Load the config for a board home, returning any fallback warnings.
    pub fn load_for(paths: &StorePaths) -> (Self, Vec<String>) {
        Self::load(Self::path_for(paths))
    }

    /// Load a specific config file. A missing file is not a warning.
    pub fn load(path: impl AsRef<Path>) -> (Self, Vec<String>) {
        let path = path.as_ref();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), Vec::new());
            }
            Err(error) => {
                return (
                    Self::default(),
                    vec![format!(
                        "cannot read {}: {error}; using defaults",
                        path.display()
                    )],
                );
            }
        };

        match serde_json::from_str::<Self>(&text) {
            Ok(config) => config.validate(),
            Err(error) => (
                Self::default(),
                vec![format!(
                    "{} is not valid JSON ({error}); using defaults",
                    path.display()
                )],
            ),
        }
    }

    /// Validate each field, replacing bad values with their defaults.
    fn validate(mut self) -> (Self, Vec<String>) {
        let defaults = Self::default();
        let mut warnings = Vec::new();

        if Hotkey::parse(&self.hotkey).is_err() {
            warnings.push(format!(
                "invalid hotkey {:?}; using {:?}",
                self.hotkey, defaults.hotkey
            ));
            self.hotkey = defaults.hotkey;
        }

        if let Some(tz) = &self.tz
            && let Err(error) = time::validate_zone(tz)
        {
            warnings.push(format!(
                "invalid time zone {tz:?} ({error}); using the system zone"
            ));
            self.tz = None;
        }

        if !(1..=1440).contains(&self.reminder_minutes) {
            warnings.push(format!(
                "reminder_minutes {} is outside 1..=1440; clamping",
                self.reminder_minutes
            ));
            self.reminder_minutes = self.reminder_minutes.clamp(1, 1440);
        }

        (self, warnings)
    }

    /// The zone new deadlines should use.
    pub fn zone(&self) -> String {
        self.tz.clone().unwrap_or_else(time::system_zone)
    }

    /// The Pi profile as the agent module's enum.
    pub fn pi_profile(&self) -> crate::agent::PiProfile {
        match self.pi_profile {
            Profile::Board => crate::agent::PiProfile::Board,
            Profile::Power => crate::agent::PiProfile::Power,
        }
    }

    /// Serialize for the settings UI or a first-run file.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }

    /// Write the config atomically (used by the settings panel).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), String> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        }
        let text = self.to_json()?;
        let scratch = path.with_extension("json.tmp");
        fs::write(&scratch, text).map_err(|error| format!("{}: {error}", scratch.display()))?;
        fs::rename(&scratch, path).map_err(|error| format!("{}: {error}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::scratch_dir;

    fn config_path(name: &str) -> PathBuf {
        let dir = scratch_dir(name);
        dir.join(CONFIG_FILE_NAME)
    }

    #[test]
    fn a_missing_file_means_defaults_without_a_warning() {
        let (config, warnings) = Config::load(config_path("config-missing"));
        assert_eq!(config, Config::default());
        assert!(warnings.is_empty());
        assert_eq!(config.hotkey, "Ctrl+Space");
        assert_eq!(config.reminder_minutes, 30);
    }

    #[test]
    fn valid_values_are_loaded() {
        let path = config_path("config-valid");
        fs::write(
            &path,
            r#"{
                "hotkey": "Ctrl+Alt+Space",
                "board_db": "D:/boards/test.sqlite3",
                "tz": "Europe/London",
                "pi_model": "deepseek-flash",
                "pi_provider": "deepseek",
                "pi_profile": "power",
                "reminder_minutes": 15
            }"#,
        )
        .expect("write config");
        let (config, warnings) = Config::load(&path);
        assert!(
            warnings.is_empty(),
            "legacy pi_model/pi_provider keys are ignored: {warnings:?}"
        );
        assert_eq!(config.hotkey, "Ctrl+Alt+Space");
        assert_eq!(config.pi_profile, Profile::Power);
        assert_eq!(config.reminder_minutes, 15);
        assert_eq!(config.zone(), "Europe/London");
    }

    #[test]
    fn malformed_json_falls_back_to_defaults_with_a_warning() {
        let path = config_path("config-bad-json");
        fs::write(&path, "{ not json").expect("write");
        let (config, warnings) = Config::load(&path);
        assert_eq!(config, Config::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("defaults"));
    }

    #[test]
    fn bad_fields_fall_back_individually() {
        let path = config_path("config-bad-fields");
        fs::write(
            &path,
            r#"{
                "hotkey": "Space",
                "tz": "Mars/Olympus",
                "pi_model": "",
                "reminder_minutes": 100000
            }"#,
        )
        .expect("write");
        let (config, warnings) = Config::load(&path);
        assert_eq!(config.hotkey, "Ctrl+Space");
        assert_eq!(config.tz, None);
        assert_eq!(config.reminder_minutes, 1440);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
    }

    #[test]
    fn save_round_trips_atomically() {
        let path = config_path("config-save");
        let config = Config {
            pi_profile: Profile::Power,
            hotkey: "Ctrl+Shift+Space".to_owned(),
            ..Config::default()
        };
        config.save(&path).expect("save");
        let (loaded, warnings) = Config::load(&path);
        assert!(warnings.is_empty());
        assert_eq!(loaded, config);
        assert!(!path.with_extension("json.tmp").exists());
    }
}

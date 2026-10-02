use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::duck::DuckingSettings;
use crate::identity::ConfiguredSource;

/// Configuration file name inside the user's config directory.
const CONFIG_DIR: &str = "pw-duck";
const CONFIG_FILE: &str = "config.toml";

/// Lowest noise floor that still counts as a threshold.
pub const MIN_VAD_THRESHOLD: f32 = 0.0025;
/// At or above this value the voice detection is treated as switched off.
pub const MAX_VAD_THRESHOLD: f32 = 0.2;
/// Upper bound for `hold_ms` and `release_fade_ms`.
pub const MAX_TIME_MS: u64 = 4_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Target volume of the virtual sink while ducking is active, in percent.
    #[serde(default = "default_duck_percent")]
    pub duck_percent: u8,
    /// Base RMS noise floor. Voice activates above roughly 2x this value.
    #[serde(default = "default_vad_threshold")]
    pub vad_threshold: f32,
    /// How long ducking stays active after the voice drops below the release threshold.
    #[serde(default = "default_hold_ms")]
    pub hold_ms: u64,
    /// Duration of the volume ramp back to 100% when ducking ends. 0 = instant.
    #[serde(default = "default_release_fade_ms")]
    pub release_fade_ms: u64,
    /// Also duck while the local microphone picks up speech.
    #[serde(default)]
    pub duck_on_microphone: bool,
    /// The playback stream that carries the remote voice.
    #[serde(default)]
    pub voice_source: Option<ConfiguredSource>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            duck_percent: default_duck_percent(),
            vad_threshold: default_vad_threshold(),
            hold_ms: default_hold_ms(),
            release_fade_ms: default_release_fade_ms(),
            duck_on_microphone: false,
            voice_source: None,
        }
    }
}

impl Config {
    /// Path of the config file, `~/.config/pw-duck/config.toml`.
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("XDG config directory not found")?;
        Ok(base.join(CONFIG_DIR).join(CONFIG_FILE))
    }

    /// Load the config file, falling back to defaults when it does not exist yet.
    pub fn load_or_default() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::load_from(&path)
    }

    /// Load and parse a specific config file.
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))
    }

    /// Write the config file, creating the directory if needed.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    /// Write the config to a specific path.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("failed to encode config TOML")?;
        fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
    }

    /// The tunable ducking settings, clamped to the supported ranges.
    pub fn settings(&self) -> DuckingSettings {
        DuckingSettings {
            duck_percent: self.duck_percent,
            vad_threshold: self.vad_threshold,
            hold_ms: self.hold_ms,
            release_fade_ms: self.release_fade_ms,
            duck_on_microphone: self.duck_on_microphone,
        }
        .clamped()
    }

    /// Copy the tunable settings back into the config, clamped to the supported ranges.
    pub fn set_settings(&mut self, settings: DuckingSettings) {
        let settings = settings.clamped();
        self.duck_percent = settings.duck_percent;
        self.vad_threshold = settings.vad_threshold;
        self.hold_ms = settings.hold_ms;
        self.release_fade_ms = settings.release_fade_ms;
        self.duck_on_microphone = settings.duck_on_microphone;
    }

    /// Human-readable label of the configured voice source, if any.
    pub fn voice_source_label(&self) -> Option<&str> {
        self.voice_source
            .as_ref()
            .and_then(|source| source.label.as_deref())
    }
}

/// One tunable key of the config file.
///
/// Used by `pw-duck-smooth config` so the file can be inspected and edited
/// from scripts without knowing the TOML layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKey {
    DuckPercent,
    VadThreshold,
    HoldMs,
    ReleaseFadeMs,
    DuckOnMicrophone,
}

impl SettingKey {
    /// Every key in the order shown by `config show`.
    pub const ALL: [Self; 5] = [
        Self::DuckPercent,
        Self::VadThreshold,
        Self::HoldMs,
        Self::ReleaseFadeMs,
        Self::DuckOnMicrophone,
    ];

    /// The key as it appears in the config file.
    pub fn name(self) -> &'static str {
        match self {
            Self::DuckPercent => "duck_percent",
            Self::VadThreshold => "vad_threshold",
            Self::HoldMs => "hold_ms",
            Self::ReleaseFadeMs => "release_fade_ms",
            Self::DuckOnMicrophone => "duck_on_microphone",
        }
    }

    /// One-line explanation shown by `config show`.
    pub fn description(self) -> &'static str {
        match self {
            Self::DuckPercent => "volume of the virtual sink while ducking (0-100)",
            Self::VadThreshold => "RMS noise floor; voice starts above 2x this value",
            Self::HoldMs => "how long ducking stays after the voice stops",
            Self::ReleaseFadeMs => "fade-in time back to 100%; 0 = instant",
            Self::DuckOnMicrophone => "also duck while your own microphone is active",
        }
    }

    /// Read the current value as display string.
    pub fn get(self, config: &Config) -> String {
        match self {
            Self::DuckPercent => config.duck_percent.to_string(),
            Self::VadThreshold => format!("{:.4}", config.vad_threshold),
            Self::HoldMs => config.hold_ms.to_string(),
            Self::ReleaseFadeMs => config.release_fade_ms.to_string(),
            Self::DuckOnMicrophone => config.duck_on_microphone.to_string(),
        }
    }

    /// The built-in default value as display string.
    pub fn default_value(self) -> String {
        self.get(&Config::default())
    }

    /// Parse and write a value into the config, clamped to the supported range.
    pub fn set(self, config: &mut Config, value: &str) -> Result<()> {
        match self {
            Self::DuckPercent => {
                config.duck_percent = parse_number::<u8>(value, "duck_percent")?.min(100);
            }
            Self::VadThreshold => {
                let threshold = parse_number::<f32>(value, "vad_threshold")?;
                config.vad_threshold = threshold.clamp(MIN_VAD_THRESHOLD, MAX_VAD_THRESHOLD);
            }
            Self::HoldMs => {
                config.hold_ms = parse_number::<u64>(value, "hold_ms")?.min(MAX_TIME_MS);
            }
            Self::ReleaseFadeMs => {
                config.release_fade_ms =
                    parse_number::<u64>(value, "release_fade_ms")?.min(MAX_TIME_MS);
            }
            Self::DuckOnMicrophone => {
                config.duck_on_microphone = parse_bool(value)?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for SettingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for SettingKey {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|key| key.name() == value)
            .ok_or_else(|| anyhow::anyhow!("unknown setting `{value}`"))
    }
}

fn parse_number<T>(value: &str, key: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .trim()
        .parse::<T>()
        .map_err(|err| anyhow::anyhow!("invalid value `{value}` for {key}: {err}"))
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        other => bail!("invalid boolean value `{other}`; use true or false"),
    }
}

/// Load the tunable settings from the config file.
pub fn load_settings() -> Result<DuckingSettings> {
    Ok(Config::load_or_default()?.settings())
}

/// Persist the tunable settings to the config file, keeping the voice source intact.
pub fn save_settings(settings: DuckingSettings) -> Result<()> {
    let mut config = Config::load_or_default()?;
    config.set_settings(settings);
    config.save()
}

fn default_duck_percent() -> u8 {
    25
}

fn default_vad_threshold() -> f32 {
    0.01
}

fn default_hold_ms() -> u64 {
    700
}

pub fn default_release_fade_ms() -> u64 {
    600
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_ducking_is_off_by_default() {
        let config: Config = toml::from_str("duck_percent = 30").unwrap();

        assert!(!config.duck_on_microphone);
    }

    #[test]
    fn microphone_ducking_can_be_enabled() {
        let config: Config = toml::from_str("duck_on_microphone = true").unwrap();

        assert!(config.duck_on_microphone);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let config: Config = toml::from_str("").unwrap();

        assert_eq!(config, Config::default());
    }

    #[test]
    fn settings_round_trip_through_the_config() {
        let mut config = Config::default();
        config.set_settings(DuckingSettings {
            duck_percent: 42,
            vad_threshold: 0.02,
            hold_ms: 900,
            release_fade_ms: 250,
            duck_on_microphone: true,
        });

        assert_eq!(config.duck_percent, 42);
        assert_eq!(config.vad_threshold, 0.02);
        assert_eq!(config.hold_ms, 900);
        assert_eq!(config.release_fade_ms, 250);
        assert!(config.duck_on_microphone);
    }

    #[test]
    fn out_of_range_settings_are_clamped() {
        let settings = DuckingSettings {
            duck_percent: 200,
            vad_threshold: 1.0,
            hold_ms: 99_999,
            release_fade_ms: 99_999,
            duck_on_microphone: false,
        }
        .clamped();

        assert_eq!(settings.duck_percent, 100);
        assert_eq!(settings.vad_threshold, MAX_VAD_THRESHOLD);
        assert_eq!(settings.hold_ms, MAX_TIME_MS);
        assert_eq!(settings.release_fade_ms, MAX_TIME_MS);
    }

    #[test]
    fn setting_keys_parse_and_reject_unknown_names() {
        assert_eq!(
            "release_fade_ms".parse::<SettingKey>().unwrap(),
            SettingKey::ReleaseFadeMs
        );
        assert!("nope".parse::<SettingKey>().is_err());
    }

    #[test]
    fn setting_key_set_clamps_and_accepts_booleans() {
        let mut config = Config::default();

        SettingKey::DuckPercent.set(&mut config, "140").unwrap();
        SettingKey::DuckOnMicrophone
            .set(&mut config, "yes")
            .unwrap();
        SettingKey::HoldMs.set(&mut config, " 1200 ").unwrap();

        assert_eq!(config.duck_percent, 100);
        assert!(config.duck_on_microphone);
        assert_eq!(config.hold_ms, 1200);
    }

    #[test]
    fn setting_key_set_rejects_garbage() {
        let mut config = Config::default();

        assert!(SettingKey::HoldMs.set(&mut config, "soon").is_err());
        assert!(
            SettingKey::DuckOnMicrophone
                .set(&mut config, "maybe")
                .is_err()
        );
    }

    #[test]
    fn defaults_are_reported_for_every_key() {
        let config = Config::default();

        for key in SettingKey::ALL {
            assert_eq!(key.get(&config), key.default_value());
            assert!(!key.description().is_empty());
        }
    }
}

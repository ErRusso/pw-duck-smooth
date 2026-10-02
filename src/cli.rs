use clap::{Parser, Subcommand, ValueEnum};

use crate::config::SettingKey;
use crate::duck::DuckingSettings;

#[derive(Debug, Parser)]
#[command(
    name = "pw-duck-smooth",
    version,
    about = "Smooth audio ducking for PipeWire",
    long_about = "Ducks music, games and video while a voice call is active.\n\n\
                  pw-duck-smooth routes non-voice playback through a temporary virtual \
                  PipeWire sink and ducks that sink, then fades the volume back in when the \
                  voice stops. The voice stream itself is never routed and is only used for \
                  detection."
)]
pub struct Cli {
    /// Print machine-readable JSON instead of a human-readable table.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Show the audio state: default sink, playback streams and ducking setup.
    Status,

    /// Read and write `~/.config/pw-duck/config.toml` without opening a tuner.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Create a default config file if none exists yet.
    InitConfig,

    /// List current playback streams as selectable voice-source candidates.
    Sources,

    /// Store one current playback stream as the configured voice source.
    SelectSource {
        /// sink-input index shown by `pw-duck-smooth sources`
        sink_input_index: u32,
    },

    /// Open the terminal tuner for sensitivity, ducking volume, hold, release fade and mic ducking.
    Tune,

    /// Open the graphical tuner (requires a build with `--features gui`).
    TuneGui,

    /// Check the installation: `PipeWire`, `PulseAudio`, tools, tray host and config.
    Doctor,

    /// Start the `StatusNotifier` tray. Left click toggles ducking.
    Tray {
        /// Virtual sink volume while voice is active.
        #[arg(long)]
        duck_percent: Option<u8>,
        /// Base RMS noise threshold. Voice activates above roughly 2x this value.
        #[arg(long)]
        vad_threshold: Option<f32>,
        /// How long to keep ducking active after voice falls below the release threshold.
        #[arg(long)]
        hold_ms: Option<u64>,
        /// Fade-in time for the volume return after ducking ends. 0 = jump back to 100%.
        #[arg(long)]
        release_fade_ms: Option<u64>,
        /// Also duck while the local microphone picks up speech. Pass --duck-on-microphone=false to force it off.
        #[arg(long, num_args = 0..=1, default_missing_value = "true", value_name = "BOOL")]
        duck_on_microphone: Option<bool>,
    },

    /// Route streams through a temporary virtual sink until Ctrl+C.
    Route {
        #[command(flatten)]
        ducking: DuckingArgs,
        /// Required safety acknowledgement because this mutates the live audio graph.
        #[arg(long)]
        yes_really_route: bool,
    },

    /// Diagnostic path: route streams through a temporary virtual sink for N seconds.
    RouteOnce {
        /// How long to keep the temporary route alive.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Virtual sink volume while ducked.
        #[arg(long)]
        duck_percent: Option<u8>,
        /// Fade-in time for the volume return before teardown. 0 = jump back to 100%.
        #[arg(long)]
        release_fade_ms: Option<u64>,
        /// Required safety acknowledgement because this mutates the live audio graph.
        #[arg(long)]
        yes_really_route: bool,
    },
}

/// The tuning overrides shared by `tray` and `route`.
#[derive(Debug, Clone, clap::Args, Default)]
pub struct DuckingArgs {
    /// Virtual sink volume while voice is active.
    #[arg(long)]
    pub duck_percent: Option<u8>,
    /// Base RMS noise threshold. Voice activates above roughly 2x this value.
    #[arg(long)]
    pub vad_threshold: Option<f32>,
    /// How long to keep ducking active after voice falls below the release threshold.
    #[arg(long)]
    pub hold_ms: Option<u64>,
    /// Fade-in time for the volume return after ducking ends. 0 = jump back to 100%.
    #[arg(long)]
    pub release_fade_ms: Option<u64>,
    /// Also duck while the local microphone picks up speech. Pass --duck-on-microphone=false to force it off.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", value_name = "BOOL")]
    pub duck_on_microphone: Option<bool>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommand {
    /// Print the path of the config file.
    Path,
    /// Print every tunable setting, its current value and its default.
    Show,
    /// Set one tunable setting, e.g. `config set duck_percent 30`.
    Set {
        /// Setting to change.
        key: SettingKey,
        /// New value.
        value: String,
    },
    /// Restore one setting, or all of them, to the built-in default.
    Reset {
        /// Setting to restore. Omit to reset every tunable setting.
        key: Option<SettingKey>,
    },
}

impl ValueEnum for SettingKey {
    fn value_variants<'a>() -> &'a [Self] {
        &SettingKey::ALL
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.name()))
    }
}

impl Default for Command {
    fn default() -> Self {
        Self::Tray {
            duck_percent: None,
            vad_threshold: None,
            hold_ms: None,
            release_fade_ms: None,
            duck_on_microphone: None,
        }
    }
}

impl DuckingArgs {
    /// Apply the CLI overrides on top of the config file.
    pub fn resolve(self) -> anyhow::Result<DuckingSettings> {
        resolve_overrides(
            self.duck_percent,
            self.vad_threshold,
            self.hold_ms,
            self.release_fade_ms,
            self.duck_on_microphone,
        )
    }
}

/// Merge the CLI overrides with the config file and clamp the result.
pub fn resolve_overrides(
    duck_percent: Option<u8>,
    vad_threshold: Option<f32>,
    hold_ms: Option<u64>,
    release_fade_ms: Option<u64>,
    duck_on_microphone: Option<bool>,
) -> anyhow::Result<DuckingSettings> {
    let config = crate::config::Config::load_or_default()?;
    Ok(DuckingSettings {
        duck_percent: duck_percent.unwrap_or(config.duck_percent),
        vad_threshold: vad_threshold.unwrap_or(config.vad_threshold),
        hold_ms: hold_ms.unwrap_or(config.hold_ms),
        release_fade_ms: release_fade_ms.unwrap_or(config.release_fade_ms),
        duck_on_microphone: duck_on_microphone.unwrap_or(config.duck_on_microphone),
    }
    .clamped())
}

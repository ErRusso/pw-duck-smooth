mod cli;
mod config;
mod duck;
mod icons;
mod identity;
mod output;
mod pipewire_sink;
mod pulse;
mod routing;
mod shell;
mod tray;
mod tune;
#[cfg(feature = "gui")]
mod tune_gui;
mod vad;

use anyhow::{Result, bail};
use clap::Parser;
use std::fmt::Write as _;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::cli::{Cli, Command, ConfigCommand, DuckingArgs};
use crate::config::{Config, SettingKey};
use crate::duck::{DuckingEvent, DuckingOptions};
use crate::identity::ConfiguredSource;
use crate::output::{StatusReport, StreamRow, TuningSummary};
use crate::pulse::{PulseCtl, SinkInput};
use crate::shell::SystemRunner;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let runner = SystemRunner;
    let json = cli.json;

    let command = cli.command.unwrap_or_default();
    match command {
        Command::Status => print_status(&runner, json),
        Command::Config(inner) => run_config(inner, json),
        Command::InitConfig => init_config(),
        Command::Sources => print_sources(&runner, json),
        Command::SelectSource { sink_input_index } => select_source(&runner, sink_input_index),
        Command::Tune => tune::run(),
        Command::TuneGui => run_tune_gui(),
        Command::Doctor => doctor(&runner),
        Command::Tray {
            duck_percent,
            vad_threshold,
            hold_ms,
            release_fade_ms,
            duck_on_microphone,
        } => tray::run(tray::TrayOptions {
            settings: DuckingArgs {
                duck_percent,
                vad_threshold,
                hold_ms,
                release_fade_ms,
                duck_on_microphone,
            }
            .resolve()?,
        }),
        Command::Route {
            ducking,
            yes_really_route,
        } => run_route_until_interrupted(&runner, ducking.resolve()?, yes_really_route),
        Command::RouteOnce {
            seconds,
            duck_percent,
            release_fade_ms,
            yes_really_route,
        } => run_route_once(
            &runner,
            seconds,
            duck_percent.unwrap_or_else(config_default_duck_percent),
            release_fade_ms.unwrap_or_else(config_default_release_fade),
            yes_really_route,
        ),
    }
}

#[cfg(feature = "gui")]
fn run_tune_gui() -> Result<()> {
    tune_gui::run()
}

#[cfg(not(feature = "gui"))]
fn run_tune_gui() -> Result<()> {
    bail!(
        "this build has no graphical tuner: rebuild with `--features gui` or use `pw-duck-smooth tune`"
    )
}

fn config_default_duck_percent() -> u8 {
    Config::load_or_default()
        .map_or(25, |config| config.duck_percent)
        .min(100)
}

fn config_default_release_fade() -> u64 {
    Config::load_or_default()
        .map_or(config::default_release_fade_ms(), |config| {
            config.release_fade_ms
        })
        .min(config::MAX_TIME_MS)
}

fn run_config(command: ConfigCommand, json: bool) -> Result<()> {
    match command {
        ConfigCommand::Path => {
            let path = Config::path()?;
            output::emit(
                &path.display().to_string(),
                json.then(|| serde_json::json!({ "path": path.display().to_string() }))
                    .as_ref(),
            )
        }
        ConfigCommand::Show => {
            let config = Config::load_or_default()?;
            let path = Config::path()?;
            let summary = TuningSummary::from_config(&config);
            let mut text = output::settings_table(&config);
            if let Some(label) = config.voice_source_label() {
                let _ = writeln!(text, "\nvoice_source  {label}");
            }
            let _ = writeln!(text, "\nconfig file: {}", path.display());
            output::emit(
                &text,
                json.then(|| {
                    serde_json::json!({
                        "path": path.display().to_string(),
                        "exists": path.exists(),
                        "voice_source": config.voice_source_label(),
                        "settings": summary,
                    })
                })
                .as_ref(),
            )
        }
        ConfigCommand::Set { key, value } => {
            let mut config = Config::load_or_default()?;
            key.set(&mut config, &value)?;
            config.save()?;
            println!("{key} = {}", key.get(&config));
            Ok(())
        }
        ConfigCommand::Reset { key } => {
            let mut config = Config::load_or_default()?;
            let count = if let Some(key) = key {
                key.set(&mut config, &key.default_value())?;
                1
            } else {
                let defaults = Config::default().settings();
                let changed = config.settings() != defaults;
                config.set_settings(defaults);
                usize::from(changed) * SettingKey::ALL.len()
            };
            config.save()?;
            let mut text = output::settings_table(&config);
            let _ = writeln!(text, "reset {count} setting(s) to their defaults");
            println!("{text}");
            Ok(())
        }
    }
}

fn init_config() -> Result<()> {
    let path = Config::path()?;
    if path.exists() {
        println!("config already exists: {}", path.display());
    } else {
        Config::default().save()?;
        println!("created config: {}", path.display());
    }
    Ok(())
}

fn stream_rows(runner: &SystemRunner, config: &Config) -> Result<Vec<StreamRow>> {
    let pulse = PulseCtl::new(runner);
    Ok(pulse
        .sink_inputs()?
        .iter()
        .map(|input| {
            let identity = input.identity();
            StreamRow::from_input(
                input,
                &identity,
                config
                    .voice_source
                    .as_ref()
                    .is_some_and(|source| identity.matches_configured_source(source)),
            )
        })
        .collect())
}

fn print_status(runner: &SystemRunner, json: bool) -> Result<()> {
    let pulse = PulseCtl::new(runner);
    let default_sink = pulse.default_sink_name()?;
    let sink_info = pulse.sink_by_name(&default_sink)?;
    let config = Config::load_or_default()?;
    let path = Config::path()?;
    let rows = stream_rows(runner, &config)?;
    let voice_source_visible = rows.iter().any(|row| row.configured_source);

    let report = StatusReport {
        default_sink_description: sink_info.as_ref().and_then(|sink| sink.description.clone()),
        default_sink,
        playback_streams: rows.len(),
        streams: rows,
        config_path: path.display().to_string(),
        config_exists: path.exists(),
        voice_source: config.voice_source_label().map(str::to_string),
        voice_source_visible,
        tray_running: tray::tray_running(),
        tuning: TuningSummary::from_config(&config),
    };

    let text = format!(
        "Default sink : {}{}\nConfig       : {}{}\nVoice source : {}\nTray         : {}\nTuning       : duck {}% · sensitivity {:.4} · hold {} ms · release fade {} ms · mic ducking {}\n\n{}",
        report.default_sink,
        report
            .default_sink_description
            .as_deref()
            .map(|description| format!(" ({description})"))
            .unwrap_or_default(),
        report.config_path,
        if report.config_exists {
            ""
        } else {
            " (missing, using defaults)"
        },
        report
            .voice_source
            .as_deref()
            .unwrap_or("not selected — run `pw-duck-smooth sources`"),
        if report.tray_running {
            "running"
        } else {
            "not running"
        },
        report.tuning.duck_percent,
        report.tuning.vad_threshold,
        report.tuning.hold_ms,
        report.tuning.release_fade_ms,
        if report.tuning.duck_on_microphone {
            "on"
        } else {
            "off"
        },
        output::streams_table(&report.streams),
    );

    output::emit(&text, json.then_some(&report).as_ref())
}

fn print_sources(runner: &SystemRunner, json: bool) -> Result<()> {
    let config = Config::load_or_default()?;
    let rows = stream_rows(runner, &config)?;

    let text = format!(
        "{}\n\nSelect one with: pw-duck-smooth select-source <STREAM>\n",
        output::streams_table(&rows)
    );

    output::emit(&text, json.then_some(&rows).as_ref())
}

fn select_source(runner: &SystemRunner, sink_input_index: u32) -> Result<()> {
    let pulse = PulseCtl::new(runner);
    let input = find_playback_input(&pulse, sink_input_index)?;

    let label = source_label(&input);
    let mut config = Config::load_or_default()?;
    config.voice_source = Some(ConfiguredSource::from_identity(
        label.clone(),
        &input.identity(),
    ));
    config.save()?;

    println!("saved voice source: {label}");
    println!("config: {}", Config::path()?.display());
    Ok(())
}

fn find_playback_input(pulse: &PulseCtl<'_, SystemRunner>, index: u32) -> Result<SinkInput> {
    let input = pulse
        .sink_inputs()?
        .into_iter()
        .find(|input| input.index == index)
        .ok_or_else(|| anyhow::anyhow!("stream #{index} not found"))?;

    if !input.identity().is_playback_stream() {
        bail!("stream #{index} is not a playback stream");
    }

    Ok(input)
}

fn source_label(input: &SinkInput) -> String {
    let identity = input.identity();
    format!(
        "#{} {} / {} / {}",
        input.index,
        identity
            .application_name
            .as_deref()
            .unwrap_or("unknown-app"),
        identity
            .application_process_binary
            .as_deref()
            .unwrap_or("unknown-bin"),
        identity.media_name.as_deref().unwrap_or("unknown-media")
    )
}

fn run_route_once(
    runner: &SystemRunner,
    seconds: u64,
    duck_percent: u8,
    release_fade_ms: u64,
    yes_really_route: bool,
) -> Result<()> {
    duck::ensure_route_acknowledged("route-once", yes_really_route)?;

    let voice_source = duck::configured_voice_source()?;
    let mut engine = routing::RoutingEngine::new(runner, voice_source.clone());
    let mut session = engine.start(routing::RoutingOptions::default())?;
    session.set_duck_percent(duck_percent)?;

    println!("Routing {seconds}s with ducking at {duck_percent}% …");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        session.route_existing_outputs(&voice_source)?;
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    session.ramp_to_neutral(std::time::Duration::from_millis(release_fade_ms))?;
    session.stop()?;
    println!("Done, previous audio state restored.");
    Ok(())
}

fn run_route_until_interrupted(
    runner: &SystemRunner,
    settings: duck::DuckingSettings,
    yes_really_route: bool,
) -> Result<()> {
    duck::ensure_route_acknowledged("route", yes_really_route)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_handler = stop.clone();
    ctrlc::set_handler(move || {
        stop_for_handler.store(true, Ordering::SeqCst);
    })?;

    duck::run_until_stopped(
        runner,
        &stop,
        DuckingOptions {
            settings: duck::shared_settings(settings),
            reload_config: false,
        },
        |event| match event {
            DuckingEvent::WaitingForSource => {
                println!("waiting for the configured voice source …");
            }
            DuckingEvent::Started {
                label,
                threshold,
                start_threshold,
                hold_ms,
            } => println!(
                "routing active: {label}\n  vad threshold={threshold:.4} start={start_threshold:.4} hold={hold_ms}ms\n  press Ctrl+C to stop"
            ),
            DuckingEvent::VoiceActive {
                level,
                percent,
                microphone,
            } => {
                let trigger = if microphone {
                    "microphone"
                } else {
                    "remote voice"
                };
                println!("ducking {percent}% (trigger: {trigger}, level={level:.4})");
            }
            DuckingEvent::VoiceInactive { level } => {
                println!("voice quiet (level={level:.4}), releasing …");
            }
        },
    )?;
    println!("stopping routing and restoring the previous state …");
    Ok(())
}

/// One entry of the `doctor` report.
struct Check {
    name: &'static str,
    state: CheckState,
    detail: String,
}

enum CheckState {
    Ok,
    Warn,
    Fail,
}

impl CheckState {
    fn label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

fn doctor(runner: &SystemRunner) -> Result<()> {
    let mut checks = Vec::new();

    let pulse = PulseCtl::new(runner);
    match pulse.default_sink_name() {
        Ok(name) => checks.push(Check {
            name: "PulseAudio/PipeWire",
            state: CheckState::Ok,
            detail: format!("default sink: {name}"),
        }),
        Err(err) => checks.push(Check {
            name: "PulseAudio/PipeWire",
            state: CheckState::Fail,
            detail: format!("pactl failed: {err:#}"),
        }),
    }

    for tool in ["pw-link", "wpctl"] {
        let found = shell::command_exists(tool);
        checks.push(Check {
            name: tool,
            state: if found {
                CheckState::Ok
            } else {
                CheckState::Fail
            },
            detail: if found {
                "found in PATH".to_string()
            } else {
                "not found in PATH".to_string()
            },
        });
    }

    let config = Config::load_or_default()?;
    let path = Config::path()?;
    checks.push(Check {
        name: "config file",
        state: if path.exists() {
            CheckState::Ok
        } else {
            CheckState::Warn
        },
        detail: path.display().to_string(),
    });

    match config.voice_source.as_ref() {
        None => checks.push(Check {
            name: "voice source",
            state: CheckState::Fail,
            detail: "not selected yet — run `pw-duck-smooth sources`".to_string(),
        }),
        Some(source) => match pulse.sink_inputs() {
            Ok(inputs) => {
                let visible = inputs
                    .iter()
                    .any(|input| input.identity().matches_configured_source(source));
                checks.push(Check {
                    name: "voice source",
                    state: if visible {
                        CheckState::Ok
                    } else {
                        CheckState::Warn
                    },
                    detail: format!(
                        "{} — {}",
                        source.label.as_deref().unwrap_or("unnamed"),
                        if visible {
                            "stream is playing"
                        } else {
                            "no matching stream right now (join the call?)"
                        }
                    ),
                });
            }
            Err(err) => checks.push(Check {
                name: "voice source",
                state: CheckState::Warn,
                detail: format!("could not list streams: {err:#}"),
            }),
        },
    }

    checks.push(Check {
        name: "tray",
        state: if tray::tray_running() {
            CheckState::Ok
        } else {
            CheckState::Warn
        },
        detail: if tray::tray_running() {
            "a tray instance holds the runtime lock".to_string()
        } else {
            "not running".to_string()
        },
    });

    checks.push(Check {
        name: "StatusNotifier host",
        state: if tray::status_notifier_host_available() {
            CheckState::Ok
        } else {
            CheckState::Warn
        },
        detail: if cfg!(feature = "gui") {
            "tray and graphical tuner available in this build".to_string()
        } else {
            "built without the `gui` feature: no graphical tuner".to_string()
        },
    });

    let width = checks
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or(0);
    println!("pw-duck-smooth doctor\n");
    for check in &checks {
        println!(
            "  {:<width$}  {:<4}  {}",
            check.name,
            check.state.label(),
            check.detail
        );
    }

    let problems = checks
        .iter()
        .filter(|check| matches!(check.state, CheckState::Fail))
        .count();
    if problems > 0 {
        println!("\n{problems} blocking problem(s) found.");
        std::process::exit(1);
    }
    Ok(())
}

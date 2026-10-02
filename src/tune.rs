//! Terminal tuner: an arrow-key driven panel over the same config file the
//! graphical tuner uses. Every change is written immediately, so a running
//! tray picks it up within its config reload interval.

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use std::io::{Write, stdout};
use std::time::Duration;

use crate::config::{self, Config};
use crate::duck::DuckingSettings;
use crate::vad;

const LABEL_WIDTH: usize = 16;
const BAR_WIDTH: usize = 12;
const VALUE_WIDTH: usize = 18;
/// Poll interval; short enough to feel instant, long enough to stay idle.
const TICK: Duration = Duration::from_millis(120);

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Row {
    Sensitivity,
    DuckPercent,
    Hold,
    ReleaseFade,
    Microphone,
}

impl Row {
    const ALL: [Self; 5] = [
        Self::Sensitivity,
        Self::DuckPercent,
        Self::Hold,
        Self::ReleaseFade,
        Self::Microphone,
    ];

    fn previous(self) -> Self {
        match self {
            Self::Sensitivity => Self::Microphone,
            Self::DuckPercent => Self::Sensitivity,
            Self::Hold => Self::DuckPercent,
            Self::ReleaseFade => Self::Hold,
            Self::Microphone => Self::ReleaseFade,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Sensitivity => Self::DuckPercent,
            Self::DuckPercent => Self::Hold,
            Self::Hold => Self::ReleaseFade,
            Self::ReleaseFade => Self::Microphone,
            Self::Microphone => Self::Sensitivity,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Sensitivity => "Sensitivity",
            Self::DuckPercent => "Duck volume",
            Self::Hold => "Hold",
            Self::ReleaseFade => "Release fade",
            Self::Microphone => "Duck on mic",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Self::Sensitivity => "right = more sensitive (lower threshold), 0% = off",
            Self::DuckPercent => "volume of the virtual sink while ducking",
            Self::Hold => "keep ducking this long after the voice stops",
            Self::ReleaseFade => "fade back to 100%, 0 ms = instant",
            Self::Microphone => "also duck while your own microphone is active",
        }
    }

    /// Value text and bar fill, as shown on the right of the row.
    fn value(self, settings: &DuckingSettings) -> (String, u8) {
        match self {
            Self::Sensitivity => {
                let percent = sensitivity_percent(settings.vad_threshold);
                if vad::is_disabled_threshold(settings.vad_threshold) {
                    (format!("{percent:>3}%  off"), 0)
                } else {
                    (
                        format!("{percent:>3}%  {:.4}", settings.vad_threshold),
                        percent,
                    )
                }
            }
            Self::DuckPercent => (
                format!("{:>3}%", settings.duck_percent),
                settings.duck_percent,
            ),
            Self::Hold => (
                format!("{:>5} ms", settings.hold_ms),
                ratio_percent(settings.hold_ms, config::MAX_TIME_MS),
            ),
            Self::ReleaseFade => {
                let text = if settings.release_fade_ms == 0 {
                    "  off".to_string()
                } else {
                    format!("{:>5} ms", settings.release_fade_ms)
                };
                (
                    text,
                    ratio_percent(settings.release_fade_ms, config::MAX_TIME_MS),
                )
            }
            Self::Microphone => {
                let text = if settings.duck_on_microphone {
                    "on".to_string()
                } else {
                    "off".to_string()
                };
                (text, if settings.duck_on_microphone { 100 } else { 0 })
            }
        }
    }
}

/// Restores the terminal when the tuner exits or panics.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("enable terminal raw mode")?;
        execute!(stdout(), EnterAlternateScreen, Hide).context("enter tuner screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

pub fn run() -> Result<()> {
    let _terminal = TerminalGuard::enter()?;
    let mut selected = Row::Sensitivity;
    let mut settings = config::load_settings()?;
    let config_path = Config::path()?;

    loop {
        draw(&settings, selected, &config_path.display().to_string())?;

        let Ok(true) = event::poll(TICK).context("poll tuner input") else {
            continue;
        };

        let Event::Key(key) = event::read().context("read tuner input")? else {
            // Resize and focus events just trigger the next repaint.
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if should_quit(key) {
            break;
        }
        if let Some(next) = handle_key(key, &mut settings, selected) {
            selected = next;
        }
    }

    Ok(())
}

/// Applies one key press. Returns a new selection when the cursor moved.
fn handle_key(key: KeyEvent, settings: &mut DuckingSettings, selected: Row) -> Option<Row> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => Some(selected.previous()),
        KeyCode::Down | KeyCode::Char('j') => Some(selected.next()),
        KeyCode::Left | KeyCode::Char('h' | '-') => {
            if ctrl {
                return None;
            }
            adjust(settings, selected, -1);
            save(settings);
            None
        }
        KeyCode::Right | KeyCode::Char('l' | '+' | ' ') => {
            if ctrl {
                return None;
            }
            adjust(settings, selected, 1);
            save(settings);
            None
        }
        KeyCode::Char('r') => {
            reset_row(settings, selected);
            save(settings);
            None
        }
        _ => None,
    }
}

fn should_quit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn save(settings: &DuckingSettings) {
    if let Err(err) = config::save_settings(*settings) {
        eprintln!("could not save settings: {err:#}");
    }
}

fn reset_row(settings: &mut DuckingSettings, row: Row) {
    let defaults = Config::default().settings();
    match row {
        Row::Sensitivity => settings.vad_threshold = defaults.vad_threshold,
        Row::DuckPercent => settings.duck_percent = defaults.duck_percent,
        Row::Hold => settings.hold_ms = defaults.hold_ms,
        Row::ReleaseFade => settings.release_fade_ms = defaults.release_fade_ms,
        Row::Microphone => settings.duck_on_microphone = defaults.duck_on_microphone,
    }
    *settings = settings.clamped();
}

fn adjust(settings: &mut DuckingSettings, row: Row, direction: i32) {
    match row {
        Row::Sensitivity => {
            // Right = more sensitive, so a lower RMS threshold.
            let step = 0.0025 * direction as f32;
            settings.vad_threshold = (settings.vad_threshold - step)
                .clamp(config::MIN_VAD_THRESHOLD, config::MAX_VAD_THRESHOLD);
        }
        Row::DuckPercent => {
            let value = i32::from(settings.duck_percent) + direction;
            settings.duck_percent = value.clamp(0, 100) as u8;
        }
        Row::Hold => {
            let value = settings.hold_ms as i64 + i64::from(direction) * 50;
            settings.hold_ms = value.clamp(0, config::MAX_TIME_MS as i64) as u64;
        }
        Row::ReleaseFade => {
            let value = settings.release_fade_ms as i64 + i64::from(direction) * 100;
            settings.release_fade_ms = value.clamp(0, config::MAX_TIME_MS as i64) as u64;
        }
        Row::Microphone => {
            if direction > 0 {
                settings.duck_on_microphone = true;
            } else if direction < 0 {
                settings.duck_on_microphone = false;
            }
        }
    }
    *settings = settings.clamped();
}

fn draw(settings: &DuckingSettings, selected: Row, config_path: &str) -> Result<()> {
    let mut out = stdout();
    execute!(
        out,
        Clear(ClearType::All),
        MoveTo(0, 0),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print(" pw-duck-smooth "),
        SetAttribute(Attribute::Reset),
        SetForegroundColor(Color::DarkGrey),
        Print("· tuner"),
        ResetColor
    )
    .context("draw tuner header")?;

    write_line(&mut out, "")?;
    for row in Row::ALL {
        draw_row(&mut out, row, row == selected, settings)?;
    }

    write_line(&mut out, "")?;
    write_line(&mut out, &format!(" {}", selected.help()))?;

    write_line(&mut out, "")?;
    write_line(&mut out, " ↑/↓ select · ←/→ adjust · r reset row · q quit")?;
    write_line(&mut out, &format!(" saved to {config_path}"))?;
    out.flush().context("flush tuner")
}

fn draw_row(
    out: &mut impl Write,
    row: Row,
    selected: bool,
    settings: &DuckingSettings,
) -> Result<()> {
    let (value, percent) = row.value(settings);
    let marker = if selected { "›" } else { " " };
    let label = format!("{:<LABEL_WIDTH$}", row.label());

    if selected {
        execute!(
            out,
            SetForegroundColor(Color::Cyan),
            SetAttribute(Attribute::Bold)
        )?;
    }
    execute!(
        out,
        Print(format!(
            " {marker}{label}[{:<BAR_WIDTH$}] {value:<VALUE_WIDTH$}",
            bar(percent)
        ))
    )?;
    if selected {
        execute!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    }
    execute!(out, Print("\r\n"))?;
    Ok(())
}

fn write_line(out: &mut impl Write, text: &str) -> Result<()> {
    execute!(out, Print(text), Print("\r\n")).context("write tuner line")
}

fn bar(percent: u8) -> String {
    let filled = (usize::from(percent).min(100) + 5) / 10;
    let empty = BAR_WIDTH.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "·".repeat(empty))
}

fn ratio_percent(value: u64, max: u64) -> u8 {
    if max == 0 {
        return 0;
    }
    ((value.min(max) * 100) / max) as u8
}

fn sensitivity_percent(threshold: f32) -> u8 {
    let min = config::MIN_VAD_THRESHOLD;
    let max = config::MAX_VAD_THRESHOLD;
    let normalized = 1.0 - ((threshold.clamp(min, max) - min) / (max - min));
    (normalized * 100.0).round().clamp(0.0, 100.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> DuckingSettings {
        Config::default().settings()
    }

    #[test]
    fn adjust_clamps_at_the_limits() {
        let mut settings = settings();
        for _ in 0..200 {
            adjust(&mut settings, Row::DuckPercent, 1);
            adjust(&mut settings, Row::Hold, 1);
            adjust(&mut settings, Row::ReleaseFade, 1);
            adjust(&mut settings, Row::Sensitivity, 1);
        }
        assert_eq!(settings.duck_percent, 100);
        assert_eq!(settings.hold_ms, config::MAX_TIME_MS);
        assert_eq!(settings.release_fade_ms, config::MAX_TIME_MS);
        assert_eq!(settings.vad_threshold, config::MIN_VAD_THRESHOLD);

        for _ in 0..400 {
            adjust(&mut settings, Row::DuckPercent, -1);
            adjust(&mut settings, Row::Hold, -1);
            adjust(&mut settings, Row::ReleaseFade, -1);
            adjust(&mut settings, Row::Sensitivity, -1);
        }
        assert_eq!(settings.duck_percent, 0);
        assert_eq!(settings.hold_ms, 0);
        assert_eq!(settings.release_fade_ms, 0);
        assert_eq!(settings.vad_threshold, config::MAX_VAD_THRESHOLD);
    }

    #[test]
    fn microphone_row_toggles_both_ways() {
        let mut settings = settings();
        settings.duck_on_microphone = false;

        adjust(&mut settings, Row::Microphone, 1);
        assert!(settings.duck_on_microphone);
        adjust(&mut settings, Row::Microphone, -1);
        assert!(!settings.duck_on_microphone);
    }

    #[test]
    fn reset_row_restores_the_default() {
        let mut settings = settings();
        settings.duck_percent = 90;

        reset_row(&mut settings, Row::DuckPercent);

        assert_eq!(settings.duck_percent, Config::default().duck_percent);
    }

    #[test]
    fn row_navigation_wraps_around() {
        assert_eq!(Row::Sensitivity.next(), Row::DuckPercent);
        assert_eq!(Row::Microphone.next(), Row::Sensitivity);
        assert_eq!(Row::Sensitivity.previous(), Row::Microphone);
    }

    #[test]
    fn values_are_rendered_for_every_row() {
        let settings = settings();
        for row in Row::ALL {
            let (value, percent) = row.value(&settings);
            assert!(!value.trim().is_empty(), "{row:?} has no value");
            assert!(percent <= 100);
        }
    }

    #[test]
    fn sensitivity_percent_spans_the_range() {
        assert_eq!(sensitivity_percent(config::MIN_VAD_THRESHOLD), 100);
        assert_eq!(sensitivity_percent(config::MAX_VAD_THRESHOLD), 0);
    }
}

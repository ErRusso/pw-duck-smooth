//! Rendering helpers for the human-readable CLI output and the `--json` variant.

use anyhow::Result;
use serde::Serialize;
use std::fmt::Write as _;

use crate::config::{Config, SettingKey};
use crate::identity::AudioIdentity;
use crate::pulse::SinkInput;

/// One playback stream as shown by `status` and `sources`.
#[derive(Debug, Clone, Serialize)]
pub struct StreamRow {
    pub index: u32,
    pub sink: u32,
    pub app: String,
    pub binary: String,
    pub media: String,
    pub role: String,
    pub class: String,
    /// True when the stream metadata looks like a call or voice stream.
    pub looks_like_voice: bool,
    /// True when this stream matches the configured voice source.
    pub configured_source: bool,
}

impl StreamRow {
    pub fn from_input(input: &SinkInput, identity: &AudioIdentity, is_configured: bool) -> Self {
        Self {
            index: input.index,
            sink: input.sink,
            app: or_unknown(identity.application_name.as_deref()),
            binary: or_unknown(identity.application_process_binary.as_deref()),
            media: or_unknown(identity.media_name.as_deref()),
            role: or_unknown(identity.media_role.as_deref()),
            class: or_unknown(identity.media_class.as_deref()),
            looks_like_voice: crate::identity::looks_like_voice_source(identity),
            configured_source: is_configured,
        }
    }

    fn cells(&self) -> Vec<String> {
        let mut flags = Vec::new();
        if self.configured_source {
            flags.push("configured");
        } else if self.looks_like_voice {
            flags.push("likely voice");
        }
        vec![
            format!("#{}", self.index),
            self.sink.to_string(),
            self.app.clone(),
            self.binary.clone(),
            self.media.clone(),
            if flags.is_empty() {
                "-".to_string()
            } else {
                flags.join(", ")
            },
        ]
    }
}

/// The ducking setup as shown by `status`.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub default_sink: String,
    pub default_sink_description: Option<String>,
    pub playback_streams: usize,
    pub streams: Vec<StreamRow>,
    pub config_path: String,
    pub config_exists: bool,
    pub voice_source: Option<String>,
    pub voice_source_visible: bool,
    pub tray_running: bool,
    pub tuning: TuningSummary,
}

/// The effective tuning values as shown by `status` and `config show`.
#[derive(Debug, Clone, Serialize)]
pub struct TuningSummary {
    pub duck_percent: u8,
    pub vad_threshold: f32,
    pub hold_ms: u64,
    pub release_fade_ms: u64,
    pub duck_on_microphone: bool,
}

impl TuningSummary {
    pub fn from_config(config: &Config) -> Self {
        let settings = config.settings();
        Self {
            duck_percent: settings.duck_percent,
            vad_threshold: settings.vad_threshold,
            hold_ms: settings.hold_ms,
            release_fade_ms: settings.release_fade_ms,
            duck_on_microphone: settings.duck_on_microphone,
        }
    }
}

fn or_unknown(value: Option<&str>) -> String {
    value
        .filter(|value| !value.is_empty())
        .unwrap_or("-")
        .to_string()
}

/// A left-aligned, column-aligned text table.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|header| (*header).to_string()).collect(),
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, cells: Vec<String>) {
        debug_assert_eq!(cells.len(), self.headers.len());
        self.rows.push(cells);
    }

    pub fn render(&self) -> String {
        let columns = self.headers.len();
        let mut widths: Vec<usize> = self
            .headers
            .iter()
            .map(|header| header.chars().count())
            .collect();
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate().take(columns) {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }

        let mut out = String::new();
        push_row(&mut out, &self.headers, &widths);
        let dashes: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
        push_row(&mut out, &dashes, &widths);
        for row in &self.rows {
            push_row(&mut out, row, &widths);
        }
        out
    }
}

fn push_row(out: &mut String, cells: &[String], widths: &[usize]) {
    for (index, cell) in cells.iter().enumerate() {
        if index + 1 == cells.len() {
            out.push_str(cell);
        } else {
            let _ = write!(out, "{cell:<width$}  ", width = widths[index]);
        }
    }
    out.push('\n');
}

/// Print a value either as pretty JSON or as a ready-made text block.
pub fn emit(text: &str, json: Option<&impl Serialize>) -> Result<()> {
    match json {
        Some(value) => println!("{}", serde_json::to_string_pretty(value)?),
        None => println!("{text}"),
    }
    Ok(())
}

/// `status` and `sources` share the stream table.
pub fn streams_table(rows: &[StreamRow]) -> String {
    let mut table = Table::new(&["STREAM", "SINK", "APP", "BINARY", "MEDIA", "HINT"]);
    for row in rows {
        table.push(row.cells());
    }
    table.render()
}

/// `config show` lists every key with its current value and default.
pub fn settings_table(config: &Config) -> String {
    let mut table = Table::new(&["SETTING", "VALUE", "DEFAULT", "DESCRIPTION"]);
    for key in SettingKey::ALL {
        table.push(vec![
            key.name().to_string(),
            key.get(config),
            key.default_value(),
            key.description().to_string(),
        ]);
    }
    table.render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns() {
        let mut table = Table::new(&["A", "LONGER"]);
        table.push(vec!["1".into(), "x".into()]);
        table.push(vec!["22".into(), "yy".into()]);

        let rendered = table.render();

        assert_eq!(rendered, "A   LONGER\n--  ------\n1   x\n22  yy\n");
    }

    #[test]
    fn settings_table_lists_every_key() {
        let rendered = settings_table(&Config::default());

        for key in SettingKey::ALL {
            assert!(rendered.contains(key.name()), "missing {key}");
        }
    }
}

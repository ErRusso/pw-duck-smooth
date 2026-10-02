//! Graphical tuner (GTK4). Same config file and live-apply behaviour as the
//! terminal tuner: every widget change is written immediately and picked up by
//! a running tray within its config reload interval.

use anyhow::{Context, Result};
use gtk::prelude::*;
use gtk::{
    Adjustment, Application, ApplicationWindow, Box as GtkBox, Button, CheckButton, Frame,
    HeaderBar, Label, Orientation, Scale,
};
use std::cell::RefCell;
use std::rc::Rc;

use crate::config::{self, Config, MAX_TIME_MS, MAX_VAD_THRESHOLD, MIN_VAD_THRESHOLD};
use crate::duck::DuckingSettings;
use crate::icons;
use crate::vad;

const APP_ID: &str = "dev.pw_duck.Tune";
const HOLD_STEP_MS: f64 = 50.0;
const FADE_STEP_MS: f64 = 100.0;

pub fn run() -> Result<()> {
    gtk::init().context("initialize GTK")?;
    install_icon_theme_path();
    gtk::Window::set_default_icon_name(icons::APP_ICON_NAME);
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run_with_args(&["pw-duck-tune-gui"]);
    Ok(())
}

fn install_icon_theme_path() {
    if let Some(display) = gtk::gdk::Display::default() {
        if let Some(path) = icons::icon_theme_path() {
            gtk::IconTheme::for_display(&display).add_search_path(path);
        }
    }
}

/// A labelled slider with a live value read-out.
struct SliderRow {
    scale: Scale,
    value: Label,
}

impl SliderRow {
    fn new(root: &GtkBox, title: &str, subtitle: &str, adjustment: &Adjustment) -> Self {
        let scale = Scale::new(Orientation::Horizontal, Some(adjustment));
        scale.set_digits(0);
        scale.set_hexpand(true);
        scale.set_valign(gtk::Align::Center);

        let value = Label::new(None);
        value.set_xalign(1.0);
        value.set_width_chars(16);
        value.add_css_class("numeric");

        let header = GtkBox::new(Orientation::Horizontal, 8);
        let title_label = Label::new(Some(title));
        title_label.set_xalign(0.0);
        title_label.set_hexpand(true);
        title_label.add_css_class("heading");
        header.append(&title_label);
        header.append(&value);

        let subtitle_label = Label::new(Some(subtitle));
        subtitle_label.set_xalign(0.0);
        subtitle_label.set_wrap(true);
        subtitle_label.add_css_class("dim-label");

        let body = GtkBox::new(Orientation::Vertical, 4);
        body.set_margin_top(10);
        body.set_margin_bottom(12);
        body.set_margin_start(14);
        body.set_margin_end(14);
        body.append(&header);
        body.append(&subtitle_label);
        body.append(&scale);
        root.append(&body);

        Self { scale, value }
    }
}

fn build_ui(app: &Application) {
    install_icon_theme_path();

    let settings = config::load_settings().unwrap_or_else(|_| Config::default().settings());
    let config_path = Config::path().map_or_else(
        |_| "<unknown>".to_string(),
        |path| path.display().to_string(),
    );

    let window = ApplicationWindow::builder()
        .application(app)
        .title("pw-duck-smooth Tuner")
        .icon_name(icons::APP_ICON_NAME)
        .default_width(480)
        .default_height(440)
        .build();

    let header = HeaderBar::new();
    let title = Label::new(Some("Tuner"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));
    let reset = Button::with_label("Reset");
    reset.set_tooltip_text(Some("Restore every setting to its default"));
    reset.add_css_class("suggested-action");
    header.pack_start(&reset);
    window.set_titlebar(Some(&header));

    let root = GtkBox::new(Orientation::Vertical, 14);
    root.set_margin_top(14);
    root.set_margin_bottom(14);
    root.set_margin_start(14);
    root.set_margin_end(14);

    let ducking_frame = Frame::new(Some("Ducking"));
    let ducking_box = GtkBox::new(Orientation::Vertical, 0);
    ducking_frame.set_child(Some(&ducking_box));
    root.append(&ducking_frame);

    let sensitivity = SliderRow::new(
        &ducking_box,
        "Sensitivity",
        "How quiet a signal still counts as speech. 0% switches voice detection off.",
        &Adjustment::new(
            f64::from(sensitivity_percent(settings.vad_threshold)),
            0.0,
            100.0,
            1.0,
            10.0,
            0.0,
        ),
    );

    let duck_volume = SliderRow::new(
        &ducking_box,
        "Ducking volume",
        "Volume of the collected audio path while voice is active.",
        &Adjustment::new(f64::from(settings.duck_percent), 0.0, 100.0, 1.0, 10.0, 0.0),
    );

    let timing_frame = Frame::new(Some("Timing"));
    let timing_box = GtkBox::new(Orientation::Vertical, 0);
    timing_frame.set_child(Some(&timing_box));
    root.append(&timing_frame);

    let hold = SliderRow::new(
        &timing_box,
        "Hold",
        "Keep ducking this long after the voice stops, so short gaps do not pump the volume.",
        &Adjustment::new(
            settings.hold_ms as f64,
            0.0,
            MAX_TIME_MS as f64,
            HOLD_STEP_MS,
            250.0,
            0.0,
        ),
    );

    let release_fade = SliderRow::new(
        &timing_box,
        "Release fade",
        "Fade the volume back to 100%. 0 ms returns immediately, higher values glide.",
        &Adjustment::new(
            settings.release_fade_ms as f64,
            0.0,
            MAX_TIME_MS as f64,
            FADE_STEP_MS,
            500.0,
            0.0,
        ),
    );

    let trigger_frame = Frame::new(Some("Trigger"));
    let trigger_box = GtkBox::new(Orientation::Vertical, 6);
    trigger_box.set_margin_top(10);
    trigger_box.set_margin_bottom(12);
    trigger_box.set_margin_start(14);
    trigger_box.set_margin_end(14);
    trigger_frame.set_child(Some(&trigger_box));
    root.append(&trigger_frame);

    let microphone = CheckButton::with_label("Duck when I talk");
    microphone.set_active(settings.duck_on_microphone);
    trigger_box.append(&microphone);

    let mic_hint = Label::new(Some(
        "Also ducks the audio while your own microphone picks up speech, using the same sensitivity and hold.",
    ));
    mic_hint.set_xalign(0.0);
    mic_hint.set_wrap(true);
    mic_hint.add_css_class("dim-label");
    trigger_box.append(&mic_hint);

    let footer = GtkBox::new(Orientation::Vertical, 2);
    let save_note = Label::new(Some(
        "Changes are saved immediately and applied to the running tray.",
    ));
    save_note.set_xalign(0.0);
    save_note.set_wrap(true);
    save_note.add_css_class("dim-label");
    let path_label = Label::new(Some(&config_path));
    path_label.set_xalign(0.0);
    path_label.set_wrap(true);
    path_label.add_css_class("dim-label");
    path_label.set_selectable(true);
    footer.append(&save_note);
    footer.append(&path_label);
    root.append(&footer);

    let state = Rc::new(RefCell::new(TuneState {
        sensitivity: sensitivity.scale.clone(),
        duck_volume: duck_volume.scale.clone(),
        hold: hold.scale.clone(),
        release_fade: release_fade.scale.clone(),
        microphone: microphone.clone(),
    }));

    let state_for_update = state.clone();
    let update = {
        let sensitivity_value = sensitivity.value.clone();
        let duck_volume_value = duck_volume.value.clone();
        let hold_value = hold.value.clone();
        let release_fade_value = release_fade.value.clone();
        move || {
            let settings = settings_from_widgets(&state_for_update.borrow());
            sensitivity_value.set_text(&sensitivity_label(settings.vad_threshold));
            duck_volume_value.set_text(&format!("{}%", settings.duck_percent));
            hold_value.set_text(&format!("{} ms", settings.hold_ms));
            release_fade_value.set_text(&release_fade_label(settings.release_fade_ms));
            if let Err(err) = config::save_settings(settings) {
                eprintln!("could not save tuner settings: {err:#}");
            }
        }
    };

    for scale in [
        &sensitivity.scale,
        &duck_volume.scale,
        &hold.scale,
        &release_fade.scale,
    ] {
        let update = update.clone();
        scale.connect_value_changed(move |_| {
            update();
        });
    }
    {
        let update = update.clone();
        microphone.connect_toggled(move |_| {
            update();
        });
    }

    {
        let state = state.clone();
        let update = update.clone();
        reset.connect_clicked(move |_| {
            let defaults = Config::default().settings();
            {
                let state = state.borrow();
                state
                    .sensitivity
                    .set_value(f64::from(sensitivity_percent(defaults.vad_threshold)));
                state
                    .duck_volume
                    .set_value(f64::from(defaults.duck_percent));
                state.hold.set_value(defaults.hold_ms as f64);
                state
                    .release_fade
                    .set_value(defaults.release_fade_ms as f64);
                state.microphone.set_active(defaults.duck_on_microphone);
            }
            update();
        });
    }

    update();

    window.set_child(Some(&root));
    window.present();
}

/// The widgets the settings are read back from.
struct TuneState {
    sensitivity: Scale,
    duck_volume: Scale,
    hold: Scale,
    release_fade: Scale,
    microphone: CheckButton,
}

fn settings_from_widgets(state: &TuneState) -> DuckingSettings {
    DuckingSettings {
        duck_percent: state.duck_volume.value().round().clamp(0.0, 100.0) as u8,
        vad_threshold: threshold_from_sensitivity(state.sensitivity.value().round() as u8),
        hold_ms: ((state.hold.value() / HOLD_STEP_MS).round() * HOLD_STEP_MS) as u64,
        release_fade_ms: ((state.release_fade.value() / FADE_STEP_MS).round() * FADE_STEP_MS)
            as u64,
        duck_on_microphone: state.microphone.is_active(),
    }
    .clamped()
}

fn release_fade_label(release_fade_ms: u64) -> String {
    if release_fade_ms == 0 {
        "0 ms · off".to_string()
    } else {
        format!("{release_fade_ms} ms")
    }
}

fn sensitivity_label(threshold: f32) -> String {
    let percent = sensitivity_percent(threshold);
    if vad::is_disabled_threshold(threshold) {
        format!("{percent}% · off")
    } else {
        format!("{percent}% · {threshold:.4}")
    }
}

fn sensitivity_percent(threshold: f32) -> u8 {
    let normalized = 1.0
        - ((threshold.clamp(MIN_VAD_THRESHOLD, MAX_VAD_THRESHOLD) - MIN_VAD_THRESHOLD)
            / (MAX_VAD_THRESHOLD - MIN_VAD_THRESHOLD));
    (normalized * 100.0).round().clamp(0.0, 100.0) as u8
}

fn threshold_from_sensitivity(percent: u8) -> f32 {
    let normalized = f32::from(percent.min(100)) / 100.0;
    MIN_VAD_THRESHOLD + (1.0 - normalized) * (MAX_VAD_THRESHOLD - MIN_VAD_THRESHOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitivity_maps_to_the_threshold_range() {
        assert!(sensitivity_percent(MIN_VAD_THRESHOLD) >= 99);
        assert_eq!(sensitivity_percent(MAX_VAD_THRESHOLD), 0);
    }

    #[test]
    fn sensitivity_percent_is_invertible() {
        for percent in [0, 25, 50, 75, 100] {
            let threshold = threshold_from_sensitivity(percent);
            assert!(
                (i32::from(sensitivity_percent(threshold)) - i32::from(percent)).abs() <= 1,
                "{percent} -> {threshold}"
            );
        }
    }

    #[test]
    fn zero_fade_is_labelled_as_off() {
        assert_eq!(release_fade_label(0), "0 ms · off");
        assert_eq!(release_fade_label(600), "600 ms");
    }
}

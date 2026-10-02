use anyhow::{Context, Result, bail};
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, Disposition, StandardItem, SubMenu};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::duck::{self, DuckingEvent, DuckingOptions, DuckingSettings, SharedDuckingSettings};
use crate::icons;
use crate::identity::{AudioIdentity, ConfiguredSource};
use crate::pulse::{PulseCtl, SinkInput};
use crate::shell::{CommandRunner, SystemRunner};

#[derive(Debug, Copy, Clone)]
pub struct TrayOptions {
    pub settings: DuckingSettings,
}

#[derive(Debug, Clone)]
enum TrayCommand {
    Toggle,
    SelectSource(u32),
    OpenTuner,
    Quit,
}

#[derive(Debug, Clone)]
enum WorkerEvent {
    WaitingForSource,
    Started {
        label: String,
        threshold: f32,
        start_threshold: f32,
        hold_ms: u64,
    },
    VoiceActive {
        level: f32,
        percent: u8,
        microphone: bool,
    },
    VoiceInactive {
        level: f32,
    },
    Stopped,
    Error(String),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum TrayRunState {
    Idle,
    Waiting,
    Starting,
    Neutral,
    Ducked,
    Stopping,
    Error,
}

#[derive(Debug)]
struct DuckingWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct SingleInstanceGuard {
    path: PathBuf,
    pid: u32,
    _file: File,
}

impl SingleInstanceGuard {
    fn acquire() -> Result<Self> {
        let path = lock_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "create runtime directory for tray lock: {}",
                    parent.display()
                )
            })?;
        }

        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let pid = std::process::id();
                    writeln!(file, "{pid}")
                        .with_context(|| format!("write tray lock {}", path.display()))?;
                    return Ok(Self {
                        path,
                        pid,
                        _file: file,
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(pid) = read_lock_pid(&path) {
                        if pid_alive(pid) {
                            bail!(
                                "another pw-duck tray is already running as process {pid}; not starting a second one"
                            );
                        }
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("create tray lock {}", path.display()));
                }
            }
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if read_lock_pid(&self.path) == Some(self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn lock_path() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("pw-duck/tray.lock");
    }
    std::env::temp_dir().join(format!(
        "pw-duck-{}-tray.lock",
        std::env::var("USER").unwrap_or_else(|_| "user".into())
    ))
}

/// Whether a tray instance currently holds the runtime lock.
pub fn tray_running() -> bool {
    let Some(pid) = read_lock_pid(&lock_path()) else {
        return false;
    };
    pid_alive(pid)
}

fn read_lock_pid(path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

fn pid_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

impl DuckingWorker {
    fn spawn(settings: SharedDuckingSettings, tx: Sender<WorkerEvent>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::spawn(move || {
            let runner = SystemRunner;
            let result = duck::run_until_stopped(
                &runner,
                &thread_stop,
                DuckingOptions {
                    settings,
                    reload_config: true,
                },
                |event| {
                    let worker_event = match event {
                        DuckingEvent::WaitingForSource => WorkerEvent::WaitingForSource,
                        DuckingEvent::Started {
                            label,
                            threshold,
                            start_threshold,
                            hold_ms,
                        } => WorkerEvent::Started {
                            label,
                            threshold,
                            start_threshold,
                            hold_ms,
                        },
                        DuckingEvent::VoiceActive {
                            level,
                            percent,
                            microphone,
                        } => WorkerEvent::VoiceActive {
                            level,
                            percent,
                            microphone,
                        },
                        DuckingEvent::VoiceInactive { level } => {
                            WorkerEvent::VoiceInactive { level }
                        }
                    };
                    let _ = tx.send(worker_event);
                },
            );

            match result {
                Ok(()) => {
                    let _ = tx.send(WorkerEvent::Stopped);
                }
                Err(err) => {
                    let _ = tx.send(WorkerEvent::Error(format!("{err:#}")));
                }
            }
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    fn join_if_finished(&mut self) -> bool {
        if self
            .handle
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
struct PwDuckTray {
    command_tx: Sender<TrayCommand>,
    settings: SharedDuckingSettings,
    state: TrayRunState,
    source_label: Option<String>,
    /// The whole configured source, not just its label: the picker needs it to
    /// mark which entry is the one ducking actually follows.
    voice_source: Option<ConfiguredSource>,
    message: String,
    quitting: bool,
}

impl PwDuckTray {
    fn new(command_tx: Sender<TrayCommand>, settings: SharedDuckingSettings) -> Self {
        let voice_source = Config::load_or_default()
            .ok()
            .and_then(|config| config.voice_source);
        Self {
            command_tx,
            settings,
            state: TrayRunState::Idle,
            source_label: voice_source
                .as_ref()
                .and_then(|source| source.label.clone())
                .or_else(configured_source_label),
            voice_source,
            message: "Ready".to_string(),
            quitting: false,
        }
    }

    fn send(&self, command: TrayCommand) {
        let _ = self.command_tx.send(command);
    }

    fn set_pending_toggle_state(&mut self) {
        match self.state {
            TrayRunState::Idle | TrayRunState::Error => {
                self.state = TrayRunState::Starting;
                self.message = "Starting ducking …".to_string();
            }
            TrayRunState::Waiting
            | TrayRunState::Starting
            | TrayRunState::Neutral
            | TrayRunState::Ducked => {
                self.state = TrayRunState::Stopping;
                self.message = "Stopping ducking …".to_string();
            }
            TrayRunState::Stopping => {}
        }
    }

    /// The voice source picker, FLAT on purpose.
    ///
    /// It used to be a submenu per application. That reads well and is a trap:
    /// `ksni` gives a submenu row no action at all, so clicking the
    /// application does nothing on every host, and hosts disagree about whether
    /// a nested submenu opens on click or hover. Here each application is a
    /// normal row -- click it and it is the source -- followed by its streams,
    /// indented, for when the application runs more than one.
    fn source_menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let runner = SystemRunner;
        let pulse = PulseCtl::new(&runner);
        let inputs = pulse.sink_inputs();

        match inputs {
            Ok(inputs) => {
                let apps = group_sources(&inputs, self.voice_source.as_ref());
                if apps.is_empty() {
                    return vec![
                        StandardItem {
                            label: "No playback streams visible".into(),
                            enabled: false,
                            ..Default::default()
                        }
                        .into(),
                    ];
                }

                let mut items: Vec<ksni::MenuItem<Self>> = Vec::new();
                for app in apps {
                    let tx = self.command_tx.clone();
                    let index = app.primary;
                    items.push(
                        StandardItem {
                            label: app.label,
                            icon_name: app.icon.name,
                            icon_data: app.icon.data,
                            activate: Box::new(move |this: &mut Self| {
                                this.message = format!("Saving source #{index} …");
                                let _ = tx.send(TrayCommand::SelectSource(index));
                            }),
                            ..Default::default()
                        }
                        .into(),
                    );

                    for entry in app.entries {
                        let tx = self.command_tx.clone();
                        items.push(
                            StandardItem {
                                label: entry.label,
                                activate: Box::new(move |this: &mut Self| {
                                    this.message = format!("Saving source #{} …", entry.index);
                                    let _ = tx.send(TrayCommand::SelectSource(entry.index));
                                }),
                                ..Default::default()
                            }
                            .into(),
                        );
                    }
                }
                items
            }
            Err(err) => vec![
                StandardItem {
                    label: format!("Cannot read sources: {err}"),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            ],
        }
    }

    fn source_picker_label(&self) -> String {
        match self.source_label.as_deref() {
            Some(label) => format!("Choose voice source: {label}"),
            None => "Choose voice source (none selected)".to_string(),
        }
    }

    fn controls_summary(&self) -> String {
        let settings = read_settings(&self.settings);
        let trigger = if settings.duck_on_microphone {
            "voice + mic"
        } else {
            "voice"
        };
        format!(
            "{trigger} → {}% · sens {:.4} · hold {} ms · fade {} ms",
            settings.duck_percent,
            settings.vad_threshold,
            settings.hold_ms,
            settings.release_fade_ms
        )
    }

    fn visible_state_value(&self) -> &'static str {
        match self.state {
            TrayRunState::Waiting
            | TrayRunState::Starting
            | TrayRunState::Neutral
            | TrayRunState::Ducked => "ON",
            TrayRunState::Idle | TrayRunState::Stopping | TrayRunState::Error => "OFF",
        }
    }

    fn visible_state_label(&self) -> String {
        format!("Ducking {}", self.visible_state_value())
    }

    fn can_toggle(&self) -> bool {
        !matches!(self.state, TrayRunState::Starting | TrayRunState::Stopping) && !self.quitting
    }

    fn request_toggle(&mut self) {
        if !self.can_toggle() {
            return;
        }
        self.set_pending_toggle_state();
        self.send(TrayCommand::Toggle);
    }

    fn ducking_switch_checked(&self) -> bool {
        matches!(
            self.state,
            TrayRunState::Waiting
                | TrayRunState::Starting
                | TrayRunState::Neutral
                | TrayRunState::Ducked
        )
    }

    fn request_quit(&mut self) {
        self.quitting = true;
        self.state = TrayRunState::Stopping;
        self.message = "Quitting …".into();
        self.send(TrayCommand::Quit);
    }

    fn current_icon_pixmap(&self) -> Vec<ksni::Icon> {
        if self.ducking_switch_checked() {
            icons::tray_icon_pixmap_on()
        } else {
            icons::tray_icon_pixmap()
        }
    }
}

impl ksni::Tray for PwDuckTray {
    fn id(&self) -> String {
        "pw-duck".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn title(&self) -> String {
        self.visible_state_label()
    }

    fn status(&self) -> ksni::Status {
        if self.quitting {
            ksni::Status::Passive
        } else if self.state == TrayRunState::Error {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn icon_theme_path(&self) -> String {
        icons::icon_theme_path()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.current_icon_pixmap()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_pixmap: self.current_icon_pixmap(),
            title: self.title(),
            description: format!(
                "{}\n{}\\nVoice source: {}",
                self.visible_state_label(),
                self.message,
                self.source_label.as_deref().unwrap_or("not selected")
            ),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.request_toggle();
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.request_toggle();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Ducking:".into(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("  Status: {}", self.visible_state_value()),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("  {}", self.message),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!(
                    "  Voice source: {}",
                    self.source_label.as_deref().unwrap_or("not selected")
                ),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("  Tuning: {}", self.controls_summary()),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            CheckmarkItem {
                label: "Ducking".into(),
                enabled: self.can_toggle(),
                checked: self.ducking_switch_checked(),
                activate: Box::new(move |this: &mut Self| {
                    this.request_toggle();
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: tuner_menu_label().into(),
                enabled: tuner_menu_enabled(),
                activate: Box::new(move |this: &mut Self| {
                    this.message = "Opening tuner …".into();
                    let _ = this.command_tx.send(TrayCommand::OpenTuner);
                }),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: self.source_picker_label(),
                submenu: self.source_menu(),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                enabled: !self.quitting,
                disposition: Disposition::Alert,
                activate: Box::new(move |this: &mut Self| {
                    this.request_quit();
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn tuner_menu_label() -> &'static str {
    if cfg!(feature = "gui") {
        "Tuner…"
    } else {
        "Tuner (needs a build with --features gui)"
    }
}

fn tuner_menu_enabled() -> bool {
    cfg!(feature = "gui")
}

fn read_settings(settings: &SharedDuckingSettings) -> DuckingSettings {
    if let Ok(config) = Config::load_or_default() {
        return config.settings();
    }

    settings.lock().map_or_else(
        |_| Config::default().settings(),
        |settings| (*settings).clamped(),
    )
}

fn persist_settings(settings: DuckingSettings) -> Result<()> {
    config::save_settings(settings)
}

fn open_tuner(handle: &Handle<PwDuckTray>) {
    let result = spawn_tuner();
    handle.update(|tray: &mut PwDuckTray| match result {
        Ok(()) => {
            tray.message = "Tuner opened".into();
            if tray.state == TrayRunState::Error {
                tray.state = TrayRunState::Idle;
            }
        }
        Err(err) => {
            tray.state = TrayRunState::Error;
            tray.message = format!("Could not open tuner: {err:#}");
        }
    });
}

fn spawn_tuner() -> Result<()> {
    #[cfg(feature = "gui")]
    {
        let exe = std::env::current_exe().context("get current executable path")?;
        let mut child = std::process::Command::new(exe)
            .arg("tune-gui")
            .spawn()
            .context("start tuner GUI")?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    #[cfg(not(feature = "gui"))]
    {
        bail!(
            "the tuner GUI is not included in this build; use `pw-duck tune` in a terminal or build with `--features gui`"
        )
    }
}

pub fn run(options: TrayOptions) -> Result<()> {
    let _single_instance = SingleInstanceGuard::acquire()?;
    let (command_tx, command_rx) = mpsc::channel();
    let (worker_tx, worker_rx) = mpsc::channel();
    let settings = duck::shared_settings(options.settings);
    persist_settings(options.settings)?;
    let tray = PwDuckTray::new(command_tx.clone(), settings.clone());
    let handle = tray
        .disable_dbus_name(use_unique_name_sni())
        .assume_sni_available(true)
        .spawn()
        .context("start StatusNotifier tray")?;

    {
        let tx = command_tx.clone();
        ctrlc::set_handler(move || {
            let _ = tx.send(TrayCommand::Quit);
        })?;
    }

    let mut worker: Option<DuckingWorker> = None;
    let mut quit = false;
    let mut sources = SourceWatcher::new();

    while !quit {
        drain_worker_events(&handle, &worker_rx, &mut worker);

        // Applications appear and disappear while the tray runs. A cheap
        // fingerprint of the streams is enough to notice, and pushing the tray
        // an update makes the new rows show up even with the menu still closed.
        if sources.changed() {
            handle.update(|_| {});
        }

        match command_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(TrayCommand::Toggle) => {
                if worker.is_some() {
                    stop_worker(&handle, &mut worker);
                } else {
                    start_worker(&handle, &mut worker, settings.clone(), worker_tx.clone());
                }
            }
            Ok(TrayCommand::SelectSource(index)) => {
                select_source(index, &handle);
            }
            Ok(TrayCommand::OpenTuner) => {
                open_tuner(&handle);
            }
            Ok(TrayCommand::Quit) => {
                quit = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if worker.as_mut().is_some_and(DuckingWorker::join_if_finished) {
            worker = None;
        }
    }

    if let Some(worker) = worker.take() {
        handle.update(|tray: &mut PwDuckTray| {
            tray.state = TrayRunState::Stopping;
            tray.message = "Stopping ducking …".into();
        });
        worker.stop();
    }

    prepare_tray_shutdown(&handle);
    handle.shutdown().wait();
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

fn use_unique_name_sni() -> bool {
    if let Some(value) = std::env::var_os("PW_DUCK_SNI_UNIQUE_NAME") {
        return value != "0" && value != "false" && value != "no";
    }

    status_notifier_watcher_is_ashell()
}

/// Whether a `StatusNotifierItem` host is reachable on the user bus.
pub fn status_notifier_host_available() -> bool {
    let Ok(output) = Command::new("busctl")
        .args([
            "--user",
            "--no-pager",
            "status",
            "org.kde.StatusNotifierWatcher",
        ])
        .output()
    else {
        return false;
    };
    output.status.success()
}

fn status_notifier_watcher_is_ashell() -> bool {
    let Ok(output) = Command::new("busctl")
        .args([
            "--user",
            "--no-pager",
            "status",
            "org.kde.StatusNotifierWatcher",
        ])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().any(|line| {
        line == "Comm=ashell"
            || line == "CommandLine=ashell"
            || line.starts_with("CommandLine=ashell ")
    })
}

fn prepare_tray_shutdown(handle: &Handle<PwDuckTray>) {
    handle.update(|tray: &mut PwDuckTray| {
        tray.quitting = true;
        tray.state = TrayRunState::Idle;
        tray.message = "Quit".into();
    });
    std::thread::sleep(Duration::from_millis(150));
}

fn start_worker(
    handle: &Handle<PwDuckTray>,
    worker: &mut Option<DuckingWorker>,
    settings: SharedDuckingSettings,
    worker_tx: Sender<WorkerEvent>,
) {
    handle.update(|tray: &mut PwDuckTray| {
        tray.state = TrayRunState::Starting;
        tray.message = "Starting ducking …".into();
    });
    *worker = Some(DuckingWorker::spawn(settings, worker_tx));
}

fn stop_worker(handle: &Handle<PwDuckTray>, worker: &mut Option<DuckingWorker>) {
    if let Some(worker) = worker.take() {
        handle.update(|tray: &mut PwDuckTray| {
            tray.state = TrayRunState::Stopping;
            tray.message = "Stopping ducking …".into();
        });
        worker.request_stop();
        worker.stop();
        handle.update(|tray: &mut PwDuckTray| {
            tray.state = TrayRunState::Idle;
            tray.message = "Off".into();
        });
    }
}

fn drain_worker_events(
    handle: &Handle<PwDuckTray>,
    worker_rx: &Receiver<WorkerEvent>,
    worker: &mut Option<DuckingWorker>,
) {
    while let Ok(event) = worker_rx.try_recv() {
        match event {
            WorkerEvent::WaitingForSource => {
                handle.update(|tray: &mut PwDuckTray| {
                    tray.state = TrayRunState::Waiting;
                    tray.message = "Waiting for configured voice source".into();
                });
            }
            WorkerEvent::Started {
                label,
                threshold,
                start_threshold,
                hold_ms,
            } => {
                handle.update(|tray: &mut PwDuckTray| {
                    tray.state = TrayRunState::Neutral;
                    tray.message = format!(
                        "Ready: {label}, threshold={threshold:.4}, start={start_threshold:.4}, hold={hold_ms}ms"
                    );
                });
            }
            WorkerEvent::VoiceActive {
                level,
                percent,
                microphone,
            } => {
                let trigger = if microphone { "microphone" } else { "voice" };
                handle.update(|tray: &mut PwDuckTray| {
                    tray.state = TrayRunState::Ducked;
                    tray.message =
                        format!("{trigger} active: level={level:.4}, ducking {percent}%");
                });
            }
            WorkerEvent::VoiceInactive { level } => {
                handle.update(|tray: &mut PwDuckTray| {
                    tray.state = TrayRunState::Neutral;
                    tray.message = format!("Voice inactive: level={level:.4}");
                });
            }
            WorkerEvent::Stopped => {
                if let Some(running_worker) = worker.take() {
                    running_worker.stop();
                }
                handle.update(|tray: &mut PwDuckTray| {
                    tray.state = TrayRunState::Idle;
                    tray.message = "Off".into();
                });
            }
            WorkerEvent::Error(err) => {
                if let Some(running_worker) = worker.take() {
                    running_worker.stop();
                }
                handle.update(|tray: &mut PwDuckTray| {
                    tray.state = TrayRunState::Error;
                    tray.message = err;
                });
            }
        }
    }
}

fn select_source(index: u32, handle: &Handle<PwDuckTray>) {
    let runner = SystemRunner;
    let pulse = PulseCtl::new(&runner);
    let result = (|| -> Result<(String, ConfiguredSource)> {
        let input = pulse
            .sink_inputs()?
            .into_iter()
            .find(|input| input.index == index)
            .ok_or_else(|| anyhow::anyhow!("sink-input #{index} not found"))?;
        let identity = input.identity();
        if !identity.is_playback_stream() {
            anyhow::bail!("sink-input #{index} is not a playback stream");
        }
        let label = source_label(&identity);
        let source = ConfiguredSource::from_identity(label.clone(), &identity);
        let mut config = Config::load_or_default()?;
        config.voice_source = Some(source.clone());
        config.save()?;
        Ok((label, source))
    })();

    handle.update(|tray: &mut PwDuckTray| match result {
        Ok((label, source)) => {
            tray.source_label = Some(label.clone());
            // Keep the marker in the picker in step with what was just saved.
            tray.voice_source = Some(source);
            tray.message = format!("Source saved: {label}");
            if tray.state == TrayRunState::Error {
                tray.state = TrayRunState::Idle;
            }
        }
        Err(err) => {
            tray.state = TrayRunState::Error;
            tray.message = format!("Could not save source: {err:#}");
        }
    });
}

fn configured_source_label() -> Option<String> {
    Config::load_or_default()
        .ok()
        .and_then(|config| config.voice_source.and_then(|source| source.label))
}

/// One application in the picker: its own row, then one row per stream.
struct SourceApp {
    /// Application name, with a marker when the configured source is one of
    /// its streams.
    label: String,
    /// The application icon, both as a name and as PNG bytes: hosts disagree on
    /// which of the two they render.
    icon: AppIcon,
    /// Stream picked when the application row itself is clicked: the one that
    /// looks like a call, else the first. An application can run music and a
    /// conversation at once, and the conversation is what ducking wants.
    primary: u32,
    entries: Vec<SourceEntry>,
}

/// One stream of an application, shown under it.
struct SourceEntry {
    index: u32,
    label: String,
    /// Whether the configured voice source matches this stream.
    selected: bool,
    /// Whether the stream is the conversation rather than the music.
    call: bool,
}

/// Group the playback streams by application, alphabetically, and mark the
/// stream ducking currently follows. Streams without any application metadata
/// collect under one honest "unknown-app" group instead of vanishing.
fn group_sources(inputs: &[SinkInput], configured: Option<&ConfiguredSource>) -> Vec<SourceApp> {
    let mut groups: std::collections::BTreeMap<String, SourceApp> =
        std::collections::BTreeMap::new();

    for input in inputs {
        let identity = input.identity();
        if !identity.is_playback_stream() {
            continue;
        }

        let selected = configured.is_some_and(|source| identity.matches_configured_source(source));
        let app = source_app_name(&identity);
        let group = groups.entry(app.clone()).or_insert_with(|| SourceApp {
            label: app,
            icon: source_app_icon(&identity),
            primary: input.index,
            entries: Vec::new(),
        });
        group.entries.push(SourceEntry {
            index: input.index,
            label: source_entry_label(input.index, &identity, selected),
            selected,
            call: is_call_stream(&identity),
        });
    }

    groups
        .into_values()
        .map(|mut app| {
            let selected = app.entries.iter().any(|entry| entry.selected);
            app.entries.sort_by(|left, right| {
                // the conversation first, so it is also what the application row
                // picks and what the eye lands on
                right
                    .call
                    .cmp(&left.call)
                    .then_with(|| left.label.cmp(&right.label))
                    .then_with(|| left.index.cmp(&right.index))
            });
            app.primary = app.entries.iter().find(|entry| entry.call).map_or_else(
                || app.entries.first().map_or(app.primary, |entry| entry.index),
                |entry| entry.index,
            );
            if selected {
                app.label.push_str(" ✓");
            }
            app
        })
        .collect()
}

/// The application a stream belongs to, used as the folder name.
fn source_app_name(identity: &AudioIdentity) -> String {
    identity
        .application_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .or_else(|| {
            identity
                .application_process_binary
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
        })
        .unwrap_or("unknown-app")
        .to_string()
}

/// Notices when the set of playback streams changes, so the picker can be
/// refreshed. It only asks for a cheap `pactl list` and never parses the result:
/// what to show is decided when the menu is rebuilt.
struct SourceWatcher {
    last_check: Option<Instant>,
    last_fingerprint: String,
}

impl SourceWatcher {
    const INTERVAL: Duration = Duration::from_secs(2);

    fn new() -> Self {
        Self {
            last_check: None,
            last_fingerprint: String::new(),
        }
    }

    /// Whether the streams changed since the last check. The first call only
    /// takes the baseline: at startup everything is "new" and the host is told
    /// about a layout it already has.
    fn changed(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last_check
            .is_some_and(|last| now.duration_since(last) < Self::INTERVAL)
        {
            return false;
        }
        self.last_check = Some(now);

        let fingerprint = source_fingerprint();
        let changed = !self.last_fingerprint.is_empty() && self.last_fingerprint != fingerprint;
        self.last_fingerprint = fingerprint;
        changed
    }
}

fn source_fingerprint() -> String {
    CommandRunner::output_with_timeout(
        &SystemRunner,
        "pactl",
        &["list", "sink-inputs"],
        Duration::from_secs(5),
    )
    .unwrap_or_default()
}

/// An application icon, ready to hand to a menu item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AppIcon {
    /// Freedesktop icon name, for hosts that resolve names themselves.
    name: String,
    /// The PNG bytes, for hosts that only render `icon-data`. Quickshell
    /// ignores `icon-name` on menu items, which is why this exists at all.
    data: Vec<u8>,
}

/// Icon for a stream's application: guessed from the stream's application name
/// and binary, then confirmed against what is actually installed, so the menu
/// never shows an item with a dangling icon name. Empty when the application
/// ships no icon we can find, which simply means no icon.
fn source_app_icon(identity: &AudioIdentity) -> AppIcon {
    let mut cache = icon_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let candidates = icon_candidates(identity);
    if candidates.is_empty() {
        return AppIcon::default();
    }

    for candidate in &candidates {
        if let Some(found) = cache.get(candidate) {
            return found.clone();
        }
    }

    let found = candidates
        .iter()
        .find_map(|candidate| resolve_installed_icon(candidate))
        .unwrap_or_default();
    for candidate in &candidates {
        cache.insert(candidate.clone(), found.clone());
    }
    found
}

/// What the system calls this application's icon, if anything does. A desktop
/// entry is the reliable answer (Spotify ships `spotify.desktop` and
/// `Icon=spotify-client`, nothing named "spotify"); the plain icon file is the
/// fallback for applications without an entry.
fn resolve_installed_icon(candidate: &str) -> Option<AppIcon> {
    let name = match desktop_file_icon(candidate) {
        Some(name) if find_icon_file(&name).is_some() => name,
        _ => {
            find_icon_file(candidate)?;
            candidate.to_string()
        }
    };

    // Only raster icons can travel as `icon-data`; an SVG-only application
    // keeps its name and no bytes, which still works on hosts that resolve.
    let data = icon_png_path(&name)
        .and_then(|path| fs::read(path).ok())
        .unwrap_or_default();
    Some(AppIcon { name, data })
}

/// The `Icon=` key of `<candidate>.desktop`, so an application whose icon is
/// named differently from its desktop id still gets its own icon.
fn desktop_file_icon(candidate: &str) -> Option<String> {
    static APPLICATIONS: OnceLock<Vec<PathBuf>> = OnceLock::new();

    let directories = APPLICATIONS.get_or_init(|| {
        let mut roots = Vec::new();
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            roots.push(PathBuf::from(data_home));
        } else if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join(".local/share"));
        }
        roots.extend(
            std::env::var_os("XDG_DATA_DIRS")
                .map(|dirs| std::env::split_paths(&dirs).collect::<Vec<_>>())
                .unwrap_or_default(),
        );
        roots.push(PathBuf::from("/usr/share"));
        roots.push(PathBuf::from("/usr/local/share"));
        roots.iter().map(|root| root.join("applications")).collect()
    });

    let path = directories
        .iter()
        .map(|dir| dir.join(format!("{candidate}.desktop")))
        .find(|path| path.exists())?;
    let contents = fs::read_to_string(&path).ok()?;
    for line in contents.lines() {
        if let Some(icon) = line.strip_prefix("Icon=") {
            let icon = icon.trim();
            if !icon.is_empty() {
                return Some(icon.to_string());
            }
        }
    }
    None
}

/// Whether a stream is the conversation rather than the music.
///
/// The media role decides it, when there is one: an application labels its own
/// streams, and "Communication" means a call while "Music" does not. The
/// free-text hint cannot, inside Discord or a browser, where every stream says
/// "discord" -- it is only the fallback for a stream with no role at all.
fn is_call_stream(identity: &AudioIdentity) -> bool {
    match identity.media_role.as_deref().map(str::trim) {
        Some(role) if !role.is_empty() => matches!(
            role.to_ascii_lowercase().as_str(),
            "communication" | "phone" | "call" | "conference" | "voicechat"
        ),
        _ => looks_like_voice_source(identity),
    }
}

/// Names an application could plausibly be installed under, best first.
fn icon_candidates(identity: &AudioIdentity) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut push = |candidate: Option<String>| {
        if let Some(candidate) = candidate.filter(|name| !name.is_empty()) {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    };

    // The binary is the closest thing to a package name, so it comes first:
    // "LibreWolf" is installed as "librewolf", and its desktop id too.
    push(normalize_icon_name(
        identity.application_process_binary.as_deref(),
    ));
    push(normalize_icon_name(identity.application_name.as_deref()));
    // Firefox-style names carry spaces where the icon uses dashes.
    push(
        identity
            .application_name
            .as_deref()
            .map(|name| name.trim().to_lowercase().replace(' ', "-")),
    );
    push(normalize_icon_name(identity.node_name.as_deref()));
    candidates
}

fn normalize_icon_name(name: Option<&str>) -> Option<String> {
    let name = name?.trim().to_lowercase();
    let cleaned: String = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// The standard icon directories, in the order worth searching: a menu icon is
/// drawn at about 16-24 pixels, so a small raster comes before anything
/// scalable. Not a full icon-theme walk; what applications install is enough.
fn icon_directories() -> &'static [PathBuf] {
    static DIRECTORIES: OnceLock<Vec<PathBuf>> = OnceLock::new();

    DIRECTORIES.get_or_init(|| {
        let mut roots = Vec::new();
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            roots.push(PathBuf::from(data_home));
        } else if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join(".local/share"));
        }
        roots.extend(
            std::env::var_os("XDG_DATA_DIRS")
                .map(|dirs| std::env::split_paths(&dirs).collect::<Vec<_>>())
                .unwrap_or_default(),
        );
        roots.push(PathBuf::from("/usr/share"));
        roots.push(PathBuf::from("/usr/local/share"));

        // Every installed theme, not just hicolor: applications here install
        // into hicolor, but an icon theme may carry its own copy.
        let themes: Vec<PathBuf> = roots
            .iter()
            .filter_map(|root| std::fs::read_dir(root.join("icons")).ok())
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.path().is_dir().then_some(entry.path()))
            .collect();

        let mut directories = Vec::new();
        // Size first, theme second: a 22x22 PNG from any theme beats a 512x512.
        for size in [
            "22x22", "24x24", "16x16", "32x32", "48x48", "64x64", "128x128", "scalable",
        ] {
            for theme in &themes {
                directories.push(theme.join(size).join("apps"));
            }
        }
        for root in &roots {
            directories.push(root.join("pixmaps"));
        }
        directories
    })
}

/// Path of an installed icon, in any format a menu can carry.
fn find_icon_file(name: &str) -> Option<PathBuf> {
    icon_directories()
        .iter()
        .flat_map(|directory| {
            ["png", "svg", "xpm"].map(|extension| directory.join(format!("{name}.{extension}")))
        })
        .find(|path| path.exists())
}

/// Path of an installed PNG, which is the only form that can be sent as
/// `icon-data` bytes. Menu icons are tiny, so the small sizes come first and
/// a 512-pixel one is only a last resort.
fn icon_png_path(name: &str) -> Option<PathBuf> {
    icon_directories()
        .iter()
        .map(|directory| directory.join(format!("{name}.png")))
        .find(|path| path.exists())
}

fn icon_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, AppIcon>> {
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, AppIcon>>> =
        OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// A single row. The index stays visible because that index is what the tray
/// saves, and the media role tells a music stream apart from a call.
fn source_entry_label(index: u32, identity: &AudioIdentity, selected: bool) -> String {
    let media = identity
        .media_name
        .as_deref()
        .or(identity.media_role.as_deref())
        .unwrap_or("unknown-media");
    let role = identity
        .media_role
        .as_deref()
        .filter(|role| *role != media)
        .map(|role| format!(" ({role})"))
        .unwrap_or_default();
    let hint = if looks_like_voice_source(identity) {
        " voice?"
    } else {
        ""
    };
    // A check mark, not a checkbox: SNI hosts render checkable items
    // inconsistently, while a character in the label is always visible.
    let mark = if selected { "✓ " } else { "" };

    // Indented under its application row. The menu is flat, so the indent is
    // part of the label -- hosts do not agree on how a nested item is padded.
    format!("    {mark}{media}{role}{hint} · #{index}")
}

fn source_label(identity: &AudioIdentity) -> String {
    format!(
        "{} / {} / {}",
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

fn looks_like_voice_source(identity: &AudioIdentity) -> bool {
    let text = [
        identity.application_name.as_deref(),
        identity.application_process_binary.as_deref(),
        identity.media_name.as_deref(),
        identity.media_role.as_deref(),
        identity.node_name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();

    text.contains("voice")
        || text.contains("webrtc")
        || text.contains("discord")
        || text.contains("communication")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn input(index: u32, props: &[(&str, &str)]) -> SinkInput {
        SinkInput {
            index,
            sink: 0,
            properties: props
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect::<BTreeMap<String, String>>(),
        }
    }

    fn playback(app: &str, media: &str) -> SinkInput {
        input(
            1,
            &[
                ("application.name", app),
                ("media.name", media),
                ("media.class", "Stream/Output/Audio"),
            ],
        )
    }

    #[test]
    fn streams_are_folded_into_one_folder_per_application() {
        let mut discord = playback("Discord", "playStream");
        discord.index = 2;
        let mut chromium = playback("Chromium", "Audio");
        chromium.index = 1;

        let groups = group_sources(&[discord, chromium], None);

        let names: Vec<&str> = groups.iter().map(|group| group.label.as_str()).collect();
        assert_eq!(names, vec!["Chromium", "Discord"]);
        assert_eq!(groups[1].entries.len(), 1);
    }

    #[test]
    fn the_configured_source_is_marked_in_its_folder_and_row() {
        let selected = input(
            7,
            &[
                ("application.name", "Discord"),
                ("media.name", "playStream"),
                ("media.role", "Communication"),
                ("media.class", "Stream/Output/Audio"),
            ],
        );
        let other = input(
            8,
            &[
                ("application.name", "Chromium"),
                ("media.name", "Audio"),
                ("media.class", "Stream/Output/Audio"),
            ],
        );
        let configured = ConfiguredSource {
            application_name: Some("Discord".into()),
            media_name: Some("playStream".into()),
            media_class: Some("Stream/Output/Audio".into()),
            ..ConfiguredSource::default()
        };

        let groups = group_sources(&[selected, other], Some(&configured));

        assert_eq!(groups[0].label, "Chromium");
        assert!(!groups[0].entries[0].label.starts_with('✓'));
        assert_eq!(groups[1].label, "Discord ✓");
        // the row is indented under its application, then marked
        assert!(groups[1].entries[0].label.starts_with("    ✓ playStream"));
        assert!(groups[1].entries[0].label.contains("#7"));
    }

    #[test]
    fn nothing_is_marked_without_a_configured_source() {
        let groups = group_sources(&[playback("mpv", "Music")], None);

        assert_eq!(groups[0].label, "mpv");
        assert_eq!(groups[0].entries[0].label, "    Music · #1");
    }

    #[test]
    fn nameless_streams_land_in_an_unknown_app_folder() {
        let anonymous = input(
            3,
            &[
                ("media.class", "Stream/Output/Audio"),
                ("node.name", "alsa_output.pcm"),
            ],
        );
        let recording = input(
            4,
            &[("media.class", "Stream/Input/Audio"), ("node.name", "mic")],
        );

        let groups = group_sources(&[anonymous, recording], None);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "unknown-app");
        assert_eq!(groups[0].entries.len(), 1);
    }

    #[test]
    fn the_binary_names_the_folder_when_the_application_is_missing() {
        let mut anonymous = input(
            5,
            &[
                ("application.process.binary", "brave"),
                ("media.class", "Stream/Output/Audio"),
            ],
        );
        anonymous.index = 5;

        assert_eq!(group_sources(&[anonymous], None)[0].label, "brave");
    }

    #[test]
    fn the_application_row_picks_its_call_not_its_music() {
        // Discord labels its streams itself: the music one says "Music", the
        // conversation says "Communication". That role is the only thing that
        // tells them apart -- both mention discord in their metadata.
        let mut music = input(
            1,
            &[
                ("application.name", "Discord"),
                ("application.process.binary", "discord"),
                ("media.name", "Music"),
                ("media.role", "Music"),
                ("media.class", "Stream/Output/Audio"),
            ],
        );
        music.index = 1;
        let mut call = input(
            2,
            &[
                ("application.name", "Discord"),
                ("application.process.binary", "discord"),
                ("media.name", "playStream"),
                ("media.role", "Communication"),
                ("media.class", "Stream/Output/Audio"),
            ],
        );
        call.index = 2;

        let apps = group_sources(&[music, call], None);

        assert_eq!(apps.len(), 1);
        // clicking the application row must pick the conversation stream
        assert_eq!(apps[0].primary, 2);
        // and the conversation comes first in the list under it
        assert!(apps[0].entries[0].label.contains("#2"));
    }

    #[test]
    fn the_application_row_falls_back_to_the_only_stream() {
        let mut music = playback("Spotify", "Spotify");
        music.index = 7;

        let apps = group_sources(&[music], None);

        assert_eq!(apps[0].primary, 7);
    }

    #[test]
    fn every_application_is_its_own_selectable_row() {
        let mut one = playback("Spotify", "Spotify");
        one.index = 3;
        let mut two = playback("LibreWolf", "AudioStream");
        two.index = 4;

        let apps = group_sources(&[two, one], None);

        let labels: Vec<&str> = apps.iter().map(|app| app.label.as_str()).collect();
        assert_eq!(labels, vec!["LibreWolf", "Spotify"]);
        assert_eq!(apps[0].primary, 4);
        assert_eq!(apps[1].primary, 3);
    }

    #[test]
    fn icon_candidates_collapse_to_the_installed_binary_name() {
        let identity = AudioIdentity {
            application_name: Some("LibreWolf".into()),
            application_process_binary: Some("librewolf".into()),
            ..AudioIdentity::default()
        };

        assert_eq!(icon_candidates(&identity), vec!["librewolf"]);
    }

    #[test]
    fn icon_candidates_clean_the_application_name_and_try_the_dashed_form() {
        let identity = AudioIdentity {
            application_name: Some("Visual Studio Code".into()),
            ..AudioIdentity::default()
        };

        let candidates = icon_candidates(&identity);
        assert_eq!(candidates, vec!["visualstudiocode", "visual-studio-code"]);
    }

    #[test]
    fn a_real_installed_icon_is_found_on_disk() {
        // Only meaningful where the icon is really installed; elsewhere the
        // lookup is allowed to come back empty.
        let identity = AudioIdentity {
            application_name: Some("Firefox".into()),
            ..AudioIdentity::default()
        };
        let icon = source_app_icon(&identity);
        if find_icon_file("firefox").is_some() {
            assert_eq!(icon.name, "firefox");
        } else {
            assert_eq!(icon, AppIcon::default());
        }
    }

    #[test]
    fn a_found_icon_carries_png_bytes_for_the_host() {
        let identity = AudioIdentity {
            application_name: Some("Spotify".into()),
            application_process_binary: Some("spotify".into()),
            ..AudioIdentity::default()
        };
        let icon = source_app_icon(&identity);

        if let Some(name) = desktop_file_icon("spotify") {
            assert_eq!(icon.name, name, "the desktop entry decides the name");
            assert!(
                icon.data.starts_with(b"\x89PNG"),
                "a menu icon needs real PNG bytes, not just a name"
            );
        } else {
            assert_eq!(icon, AppIcon::default());
        }
    }

    #[test]
    fn an_uninstallable_application_gets_no_icon() {
        let identity = AudioIdentity {
            application_name: Some("pw-duck-nonexistent-app".into()),
            application_process_binary: Some("pw-duck-nonexistent-app".into()),
            ..AudioIdentity::default()
        };

        assert_eq!(source_app_icon(&identity), AppIcon::default());
    }

    #[test]
    fn the_desktop_entry_decides_the_icon_name() {
        // Spotify's entry is the interesting case: the file is "spotify.png"
        // nowhere, only "spotify-client", and nothing looks for it but here.
        if let Some(name) = desktop_file_icon("spotify") {
            assert!(find_icon_file(&name).is_some(), "{name} is not installed");
        }
    }
}

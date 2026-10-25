//! egui front end.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::analysis::Warnings;
use crate::audio::{self, CpalSource, InputDeviceInfo};
use crate::export::{self, ExportFormat};
use crate::recorder::{Recorder, RecorderConfig, TrackSpec, TrackState, TrackStatus};
use crate::session::{self, SessionMeta};

const SETTINGS_KEY: &str = "settings";
const GREEN: egui::Color32 = egui::Color32::from_rgb(60, 170, 60);
const ORANGE: egui::Color32 = egui::Color32::from_rgb(220, 150, 0);
const RED: egui::Color32 = egui::Color32::from_rgb(210, 60, 60);
const MAX_MESSAGES: usize = 200;

#[derive(Serialize, Deserialize)]
struct Settings {
    output_dir: String,
    format: ExportFormat,
    selected: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        let base = dirs::audio_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            output_dir: base.join("multirecorder").display().to_string(),
            format: ExportFormat::Flac,
            selected: BTreeSet::new(),
        }
    }
}

/// Progress of the background stop-and-export job.
#[derive(Default)]
struct Job {
    text: String,
    progress: f32,
    result: Option<Result<String, String>>,
}

enum Phase {
    Idle,
    Recording(Recorder),
    Finishing(Arc<Mutex<Job>>),
}

pub struct MultiRecorderApp {
    settings: Settings,
    devices: Vec<InputDeviceInfo>,
    phase: Phase,
    unexported: Vec<(PathBuf, SessionMeta)>,
    messages: Vec<String>,
    confirm_quit: bool,
    /// Most recent session folder, for the "Open last session" button.
    last_session: Option<PathBuf>,
}

impl MultiRecorderApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        let mut app = Self {
            settings,
            devices: Vec::new(),
            phase: Phase::Idle,
            unexported: Vec::new(),
            messages: Vec::new(),
            confirm_quit: false,
            last_session: None,
        };
        app.refresh_devices();
        app.rescan_sessions();
        if !app.unexported.is_empty() {
            app.message(format!(
                "Found {} session(s) that were not exported (possibly after a crash). See \"Sessions waiting for export\".",
                app.unexported.len()
            ));
        }
        app
    }

    fn message(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::info!("{msg}");
        let stamp = chrono::Local::now().format("%H:%M:%S");
        self.messages.push(format!("{stamp}  {msg}"));
        if self.messages.len() > MAX_MESSAGES {
            self.messages.remove(0);
        }
    }

    fn refresh_devices(&mut self) {
        self.devices = audio::list_devices();
    }

    fn rescan_sessions(&mut self) {
        self.unexported = session::find_unexported_sessions(Path::new(&self.settings.output_dir));
    }

    fn start_recording(&mut self) {
        let specs: Vec<TrackSpec> = self
            .devices
            .iter()
            .filter(|d| self.settings.selected.contains(&d.key()))
            .map(|d| TrackSpec {
                device_name: if d.loopback {
                    format!("{} (loopback)", d.name)
                } else {
                    d.name.clone()
                },
                device_id: d.id.clone(),
                sample_rate: d.sample_rate,
                channels: d.channels,
                loopback: d.loopback,
                source: Box::new(CpalSource {
                    device_id: d.id.clone(),
                    sample_rate: d.sample_rate,
                    channels: d.channels,
                    loopback: d.loopback,
                }),
            })
            .collect();
        match Recorder::start(
            Path::new(&self.settings.output_dir),
            specs,
            RecorderConfig::default(),
        ) {
            Ok(rec) => {
                self.message(format!("Recording to {}", rec.dir().display()));
                self.last_session = Some(rec.dir().to_path_buf());
                self.phase = Phase::Recording(rec);
            }
            Err(e) => self.message(format!("Could not start recording: {e:#}")),
        }
    }

    fn stop_recording(&mut self, ctx: &egui::Context) {
        let Phase::Recording(rec) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            return;
        };
        let job = Arc::new(Mutex::new(Job {
            text: "Stopping…".into(),
            ..Default::default()
        }));
        let format = self.settings.format;
        spawn_job(job.clone(), ctx.clone(), move |job| {
            let (dir, _) = rec
                .stop(Duration::from_secs(5))
                .map_err(|e| format!("{e:#}"))?;
            run_export(&dir, format, job)
        });
        self.phase = Phase::Finishing(job);
    }

    fn export_existing(&mut self, dir: PathBuf, ctx: &egui::Context) {
        self.last_session = Some(dir.clone());
        let job = Arc::new(Mutex::new(Job::default()));
        let format = self.settings.format;
        spawn_job(job.clone(), ctx.clone(), move |job| {
            run_export(&dir, format, job)
        });
        self.phase = Phase::Finishing(job);
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let idle = matches!(self.phase, Phase::Idle);
        ui.add_enabled_ui(idle, |ui| {
            ui.horizontal(|ui| {
                ui.label("Output folder:");
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.settings.output_dir)
                        .desired_width(ui.available_width() - 160.0),
                );
                if edit.lost_focus() {
                    self.rescan_sessions();
                }
                if ui.button("Browse…").clicked()
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_directory(&self.settings.output_dir)
                        .pick_folder()
                {
                    self.settings.output_dir = dir.display().to_string();
                    self.rescan_sessions();
                }
                if ui
                    .button("Open")
                    .on_hover_text("Open the output folder")
                    .clicked()
                {
                    self.open_folder(PathBuf::from(&self.settings.output_dir));
                }
            });
            ui.horizontal(|ui| {
                ui.label("Export format:");
                for f in [ExportFormat::Flac, ExportFormat::Wav] {
                    ui.radio_value(&mut self.settings.format, f, f.label());
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.strong("Input devices");
                if ui.button("Refresh").clicked() {
                    self.refresh_devices();
                }
            });
            if !self.devices.iter().any(|d| !d.loopback) {
                ui.label("No input devices found.");
            }
            let mut shown_outputs_heading = false;
            for d in &self.devices {
                if d.loopback && !shown_outputs_heading {
                    shown_outputs_heading = true;
                    ui.add_space(6.0);
                    ui.strong("Output devices (records what is playing)");
                }
                let key = d.key();
                let mut on = self.settings.selected.contains(&key);
                let label = format!(
                    "{}   ({} · {} ch · {} Hz · {})",
                    d.name, d.host, d.channels, d.sample_rate, d.sample_format
                );
                if ui.checkbox(&mut on, label).changed() {
                    if on {
                        self.settings.selected.insert(key);
                    } else {
                        self.settings.selected.remove(&key);
                    }
                }
            }
        });
    }

    fn open_folder(&mut self, path: PathBuf) {
        if let Err(e) = open_folder(&path) {
            self.message(format!("Could not open {}: {e}", path.display()));
        }
    }

    /// Record/stop controls; always visible at the top of the window.
    fn top_bar_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(4.0);
        match &self.phase {
            Phase::Idle => {
                let count = self
                    .devices
                    .iter()
                    .filter(|d| self.settings.selected.contains(&d.key()))
                    .count();
                let button = egui::Button::new(egui::RichText::new("●  Record").size(18.0));
                ui.horizontal(|ui| {
                    if ui.add_enabled(count > 0, button).clicked() {
                        self.start_recording();
                    }
                    ui.label(format!("{count} device(s) selected"));
                    if let Some(dir) = self.last_session.clone() {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Open last session").clicked() {
                                self.open_folder(dir);
                            }
                        });
                    }
                });
            }
            Phase::Recording(rec) => {
                let elapsed = rec.elapsed().as_secs();
                let dir = rec.dir().to_path_buf();
                let attention = rec
                    .statuses()
                    .iter()
                    .filter(|(_, st)| needs_attention(st))
                    .count();
                let (mut stop, mut open) = (false, false);
                ui.horizontal(|ui| {
                    stop = ui
                        .button(egui::RichText::new("■  Stop").size(18.0))
                        .clicked();
                    ui.label(
                        egui::RichText::new(format!(
                            "{:02}:{:02}:{:02}",
                            elapsed / 3600,
                            elapsed / 60 % 60,
                            elapsed % 60
                        ))
                        .size(18.0)
                        .monospace(),
                    );
                    if attention > 0 {
                        ui.colored_label(
                            RED,
                            egui::RichText::new(format!("⚠ {attention} track(s) need attention"))
                                .strong(),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        open = ui.button("Open session folder").clicked();
                    });
                });
                if open {
                    self.open_folder(dir);
                }
                if stop {
                    self.stop_recording(&ctx);
                }
            }
            Phase::Finishing(job) => {
                let (text, progress, result) = {
                    let j = job.lock().expect("job lock");
                    (j.text.clone(), j.progress, j.result.clone())
                };
                ui.label(text);
                ui.add(egui::ProgressBar::new(progress).show_percentage());
                if let Some(result) = result {
                    match result {
                        Ok(msg) | Err(msg) => self.message(msg),
                    }
                    self.phase = Phase::Idle;
                    self.rescan_sessions();
                }
            }
        }
        ui.add_space(4.0);
    }

    fn tracks_ui(&mut self, ui: &mut egui::Ui) {
        let Phase::Recording(rec) = &self.phase else {
            return;
        };
        let statuses = rec.statuses();
        ui.separator();
        egui::Grid::new("tracks")
            .striped(true)
            .num_columns(7)
            .show(ui, |ui| {
                for h in [
                    "Device",
                    "Level",
                    "Status",
                    "Warnings",
                    "Recorded",
                    "Gaps",
                    "Reconnects",
                ] {
                    ui.strong(h);
                }
                ui.end_row();
                for (meta, st) in &statuses {
                    let rate = f64::from(meta.sample_rate);
                    ui.allocate_ui(egui::vec2(220.0, 18.0), |ui| {
                        ui.add(egui::Label::new(&meta.device_name).truncate())
                            .on_hover_text(&meta.device_name);
                    });
                    level_meter(ui, &st.levels);
                    let (text, color) = match &st.state {
                        TrackState::Starting => ("starting".to_string(), egui::Color32::GRAY),
                        TrackState::Recording if st.idle => {
                            ("recording (idle)".to_string(), egui::Color32::GRAY)
                        }
                        TrackState::Recording => ("recording".to_string(), GREEN),
                        TrackState::Reconnecting(e) => (format!("reconnecting: {e}"), ORANGE),
                        TrackState::Stopped => ("stopped".to_string(), egui::Color32::GRAY),
                        TrackState::Failed(e) => (format!("failed: {e}"), RED),
                    };
                    ui.allocate_ui(egui::vec2(260.0, 18.0), |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&text).color(color)).truncate(),
                        )
                        .on_hover_text(&text);
                    });
                    warnings_label(ui, &st.warnings);
                    ui.label(format!("{:.1} s", st.frames_written as f64 / rate));
                    ui.label(format!("{:.1} s", st.silence_frames as f64 / rate))
                        .on_hover_text(
                            "Silence inserted while the device was missing, stalled or overloaded",
                        );
                    ui.label(st.reconnects.to_string());
                    ui.end_row();
                }
            });
    }

    fn unexported_ui(&mut self, ui: &mut egui::Ui) {
        if self.unexported.is_empty() || !matches!(self.phase, Phase::Idle) {
            return;
        }
        ui.separator();
        ui.strong("Sessions waiting for export");
        let mut export = None;
        let mut open = None;
        for (dir, meta) in &self.unexported {
            ui.horizontal(|ui| {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let note = if meta.state == session::SessionState::Recording {
                    " (interrupted)"
                } else {
                    ""
                };
                ui.label(format!("{name}{note} · {} track(s)", meta.tracks.len()));
                if ui
                    .button(format!(
                        "Export as {}",
                        self.settings.format.extension().to_uppercase()
                    ))
                    .clicked()
                {
                    export = Some(dir.clone());
                }
                if ui.button("Open").clicked() {
                    open = Some(dir.clone());
                }
            });
        }
        if let Some(dir) = open {
            self.open_folder(dir);
        }
        if let Some(dir) = export {
            self.export_existing(dir, ui.ctx());
        }
    }
}

fn needs_attention(st: &TrackStatus) -> bool {
    st.warnings.any_problem()
        || matches!(
            st.state,
            TrackState::Reconnecting(_) | TrackState::Failed(_)
        )
}

/// One horizontal bar per channel, -60..0 dBFS.
fn level_meter(ui: &mut egui::Ui, levels: &[f32]) {
    let n = levels.len().max(1);
    let bar = (18.0 / n as f32).clamp(2.0, 8.0);
    let size = egui::vec2(140.0, bar * n as f32 + (n - 1) as f32);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let db = |l: f32| 20.0 * l.max(1e-6).log10();
    for (i, &level) in levels.iter().enumerate() {
        let frac = ((db(level) + 60.0) / 60.0).clamp(0.0, 1.0);
        let color = if level >= 0.999 {
            RED
        } else if db(level) > -6.0 {
            ORANGE
        } else {
            GREEN
        };
        let top = rect.top() + i as f32 * (bar + 1.0);
        let fill = egui::Rect::from_min_size(
            egui::pos2(rect.left(), top),
            egui::vec2(rect.width() * frac, bar),
        );
        painter.rect_filled(fill, 1.0, color);
    }
    response.on_hover_text(
        levels
            .iter()
            .enumerate()
            .map(|(i, &l)| format!("ch {}: {:.1} dBFS", i + 1, db(l)))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

fn warnings_label(ui: &mut egui::Ui, w: &Warnings) {
    ui.horizontal(|ui| {
        let mut any = false;
        if w.choppy {
            any = true;
            ui.colored_label(RED, format!("⚠ choppy ({} dropouts)", w.dropouts))
                .on_hover_text(
                    "Repeated short gaps of digital silence: usually a driver, USB or \
                     buffer problem. Check the recording.",
                );
        }
        if w.clipping {
            any = true;
            ui.colored_label(RED, "⚠ clipping")
                .on_hover_text("Signal hits full scale and is distorted. Lower the input gain.");
        }
        if w.no_signal {
            any = true;
            ui.colored_label(egui::Color32::GRAY, "no signal")
                .on_hover_text("Nothing above -80 dBFS for 5 s. Fine if the source is silent.");
        }
        if !any {
            let past = w.dropouts > 0 || w.clip_events > 0;
            let text = if past {
                format!(
                    "ok ({} dropouts, {} clips earlier)",
                    w.dropouts, w.clip_events
                )
            } else {
                "—".to_string()
            };
            ui.weak(text);
        }
    });
}

/// Shows `path` in the system file browser, creating it first if needed.
fn open_folder(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let program = "xdg-open";
    std::process::Command::new(program).arg(path).spawn()?;
    Ok(())
}

fn spawn_job(
    job: Arc<Mutex<Job>>,
    ctx: egui::Context,
    work: impl FnOnce(&Arc<Mutex<Job>>) -> Result<String, String> + Send + 'static,
) {
    thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&job)))
            .unwrap_or_else(|_| Err("internal error during export".to_string()));
        job.lock().expect("job lock").result = Some(result);
        ctx.request_repaint();
    });
}

fn run_export(dir: &Path, format: ExportFormat, job: &Arc<Mutex<Job>>) -> Result<String, String> {
    job.lock().expect("job lock").text = format!("Exporting {}…", dir.display());
    let mut progress = |p: f32| job.lock().expect("job lock").progress = p;
    match export::export_session(dir, format, &mut progress) {
        Ok(files) => Ok(format!(
            "Exported {} track(s) to {}",
            files.len(),
            dir.display()
        )),
        Err(e) => Err(format!("{e:#}")),
    }
}

impl eframe::App for MultiRecorderApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !matches!(self.phase, Phase::Idle) {
            // Keep meters and status fresh; recording itself does not depend on the UI running.
            ctx.request_repaint_after(Duration::from_millis(66));
        }
        if ctx.input(|i| i.viewport().close_requested()) && !matches!(self.phase, Phase::Idle) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Fixed bars first so the record controls never scroll out of view.
        egui::Panel::top("controls").show(ui, |ui| self.top_bar_ui(ui));
        egui::Panel::bottom("messages")
            .resizable(true)
            .default_size(110.0)
            .min_size(50.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.strong("Messages");
                egui::ScrollArea::vertical()
                    .id_salt("messages")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for m in &self.messages {
                            ui.label(m);
                        }
                    });
            });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::both()
                .id_salt("main")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.settings_ui(ui);
                    self.tracks_ui(ui);
                    self.unexported_ui(ui);
                });
        });

        if self.confirm_quit {
            egui::Window::new("Recording in progress")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label("Stop the recording (and let the export finish) before closing.");
                    if ui.button("OK").clicked() {
                        self.confirm_quit = false;
                    }
                });
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }
}

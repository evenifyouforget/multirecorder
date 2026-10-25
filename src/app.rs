//! egui front end.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::audio::{self, CpalSource, InputDeviceInfo};
use crate::export::{self, ExportFormat};
use crate::recorder::{Recorder, RecorderConfig, TrackSpec, TrackState};
use crate::session::{self, SessionMeta};

const SETTINGS_KEY: &str = "settings";
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
        };
        app.refresh_devices();
        app.rescan_sessions();
        if !app.unexported.is_empty() {
            app.message(format!(
                "Found {} session(s) that were not exported (possibly after a crash). See below.",
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
        self.devices = audio::list_input_devices();
    }

    fn rescan_sessions(&mut self) {
        self.unexported = session::find_unexported_sessions(Path::new(&self.settings.output_dir));
    }

    fn start_recording(&mut self) {
        let specs: Vec<TrackSpec> = self
            .devices
            .iter()
            .filter(|d| self.settings.selected.contains(&d.id))
            .map(|d| TrackSpec {
                device_name: d.name.clone(),
                device_id: d.id.clone(),
                sample_rate: d.sample_rate,
                channels: d.channels,
                source: Box::new(CpalSource {
                    device_id: d.id.clone(),
                    sample_rate: d.sample_rate,
                    channels: d.channels,
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
                        .desired_width(ui.available_width() - 90.0),
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
            if self.devices.is_empty() {
                ui.label("No input devices found.");
            }
            for d in &self.devices {
                let mut on = self.settings.selected.contains(&d.id);
                let label = format!(
                    "{}   ({} · {} ch · {} Hz · {})",
                    d.name, d.host, d.channels, d.sample_rate, d.sample_format
                );
                if ui.checkbox(&mut on, label).changed() {
                    if on {
                        self.settings.selected.insert(d.id.clone());
                    } else {
                        self.settings.selected.remove(&d.id);
                    }
                }
            }
        });
    }

    fn controls_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        match &self.phase {
            Phase::Idle => {
                let count = self
                    .devices
                    .iter()
                    .filter(|d| self.settings.selected.contains(&d.id))
                    .count();
                let button = egui::Button::new(egui::RichText::new("●  Record").size(18.0));
                ui.horizontal(|ui| {
                    if ui.add_enabled(count > 0, button).clicked() {
                        self.start_recording();
                    }
                    ui.label(format!("{count} device(s) selected"));
                });
            }
            Phase::Recording(rec) => {
                let elapsed = rec.elapsed().as_secs();
                let statuses = rec.statuses();
                let mut stop = false;
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
                });
                egui::Grid::new("tracks")
                    .striped(true)
                    .num_columns(5)
                    .show(ui, |ui| {
                        for h in [
                            "Device",
                            "Status",
                            "Recorded",
                            "Silence inserted",
                            "Reconnects",
                        ] {
                            ui.strong(h);
                        }
                        ui.end_row();
                        for (meta, st) in &statuses {
                            let rate = f64::from(meta.sample_rate);
                            ui.label(&meta.device_name);
                            let (text, color) = match &st.state {
                                TrackState::Starting => {
                                    ("starting".to_string(), egui::Color32::GRAY)
                                }
                                TrackState::Recording => (
                                    "recording".to_string(),
                                    egui::Color32::from_rgb(60, 170, 60),
                                ),
                                TrackState::Reconnecting(e) => (
                                    format!("reconnecting: {e}"),
                                    egui::Color32::from_rgb(220, 150, 0),
                                ),
                                TrackState::Stopped => ("stopped".to_string(), egui::Color32::GRAY),
                                TrackState::Failed(e) => {
                                    (format!("failed: {e}"), egui::Color32::from_rgb(210, 60, 60))
                                }
                            };
                            ui.colored_label(color, text);
                            ui.label(format!("{:.1} s", st.frames_written as f64 / rate));
                            ui.label(format!("{:.1} s", st.silence_frames as f64 / rate));
                            ui.label(st.reconnects.to_string());
                            ui.end_row();
                        }
                    });
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
    }

    fn unexported_ui(&mut self, ui: &mut egui::Ui) {
        if self.unexported.is_empty() || !matches!(self.phase, Phase::Idle) {
            return;
        }
        ui.separator();
        ui.strong("Sessions waiting for export");
        let mut export = None;
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
            });
        }
        if let Some(dir) = export {
            self.export_existing(dir, ui.ctx());
        }
    }
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
            // Keep status fresh; recording itself does not depend on the UI running.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if ctx.input(|i| i.viewport().close_requested()) && !matches!(self.phase, Phase::Idle) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("main")
                .auto_shrink([false, true])
                .max_height(ui.available_height() * 0.7)
                .show(ui, |ui| {
                    self.settings_ui(ui);
                    ui.separator();
                    self.controls_ui(ui);
                    self.unexported_ui(ui);
                });
            ui.separator();
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

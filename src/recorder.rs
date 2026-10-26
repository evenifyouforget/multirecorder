//! Multi-device recorder.
//!
//! Every track runs on its own thread and owns its device stream and raw
//! capture file, so a failing device or a slow write only affects that track.
//!
//! All tracks share one timeline that starts when recording starts. Whenever a
//! track has no audio (device missing, stalled, ring buffer overflow) it is
//! padded with silence, so tracks stay aligned and have equal length at the end.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::analysis::{Analyzer, Warnings};
use crate::audio::{ActiveStream, InputSource, StreamEvent, StreamSink};
use crate::export::{ExportFormat, LiveExport, LiveExportControl};
use crate::session::{self, SessionLog, SessionMeta, SessionState, TrackMeta};

#[derive(Clone, Debug)]
pub struct RecorderConfig {
    /// Ring buffer length between audio callback and writer; absorbs disk lag.
    pub buffer: Duration,
    /// A stream that delivers no audio for this long is considered stalled and rebuilt.
    pub stall_timeout: Duration,
    /// Delay between attempts to reopen a missing device.
    pub retry_interval: Duration,
    /// How often captured audio is flushed to disk (bounds data lost in a crash).
    pub flush_interval: Duration,
    /// Writer loop period.
    pub poll_interval: Duration,
    /// Loopback sources send nothing while nothing plays; after this long
    /// without data their track is filled with silence up to real time.
    pub idle_gap: Duration,
    /// Export to this format alongside the recording, so stopping is quick.
    pub live_export: Option<ExportFormat>,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            buffer: Duration::from_secs(4),
            stall_timeout: Duration::from_secs(2),
            retry_interval: Duration::from_secs(2),
            flush_interval: Duration::from_secs(1),
            poll_interval: Duration::from_millis(15),
            idle_gap: Duration::from_millis(100),
            live_export: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackState {
    Starting,
    Recording,
    /// Device unavailable; padding silence and retrying. Holds the last error.
    Reconnecting(String),
    Stopped,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct TrackStatus {
    pub state: TrackState,
    pub frames_written: u64,
    /// Frames of silence inserted for gaps (device missing, stalls, overflows).
    pub silence_frames: u64,
    pub reconnects: u32,
    /// Loopback source currently delivering nothing (nothing is playing).
    pub idle: bool,
    /// Frames already exported during recording (live export only).
    pub exported_frames: u64,
    pub load: Load,
    /// Per-channel meter level (linear peak with decay), for display.
    pub levels: Vec<f32>,
    pub warnings: Warnings,
}

/// Share of one CPU core each stage of a track uses, as a fraction of real
/// time (1.0 = busy all the time, i.e. barely keeping up).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Load {
    /// Our code inside the real-time audio callback (hard deadline).
    pub callback: f32,
    /// Writer thread: draining, analysis and disk writes (4 s of slack).
    pub writer: f32,
    /// Live export encoding (only delays Stop if it falls behind).
    pub export: f32,
}

impl Load {
    /// The most loaded stage.
    pub fn max(&self) -> f32 {
        self.callback.max(self.writer).max(self.export)
    }
}

/// A device to record, with the format fixed at session start.
pub struct TrackSpec {
    pub device_name: String,
    pub device_id: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Output device recorded via loopback: silence is delivered as no data.
    pub loopback: bool,
    pub source: Box<dyn InputSource>,
}

struct Track {
    meta: TrackMeta,
    status: Arc<Mutex<TrackStatus>>,
    thread: Option<JoinHandle<()>>,
    export: Option<LiveExport>,
}

pub struct Recorder {
    dir: PathBuf,
    meta: SessionMeta,
    start: Instant,
    stop_at: Arc<OnceLock<Instant>>,
    tracks: Vec<Track>,
    log: Arc<SessionLog>,
}

impl Recorder {
    /// Creates a session directory under `root` and starts recording every track.
    pub fn start(root: &Path, specs: Vec<TrackSpec>, config: RecorderConfig) -> Result<Self> {
        anyhow::ensure!(!specs.is_empty(), "no devices selected");
        let dir = session::create_session_dir(root)?;
        let log = Arc::new(SessionLog::open(&dir));

        let metas: Vec<TrackMeta> = specs
            .iter()
            .enumerate()
            .map(|(i, s)| TrackMeta {
                raw_file: session::raw_file_name(i, &s.device_name),
                device_name: s.device_name.clone(),
                device_id: s.device_id.clone(),
                sample_rate: s.sample_rate,
                channels: s.channels,
                loopback: s.loopback,
            })
            .collect();
        let meta = SessionMeta::new(metas.clone());
        meta.save(&dir)?;

        let files = metas
            .iter()
            .map(|m| {
                let path = dir.join(&m.raw_file);
                File::create(&path).with_context(|| format!("creating {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;

        log.line(&format!("recording started in {}", dir.display()));
        let start = Instant::now();
        let stop_at = Arc::new(OnceLock::new());
        let mut tracks = Vec::new();
        for (i, ((spec, meta), file)) in specs.into_iter().zip(metas).zip(files).enumerate() {
            log.line(&format!(
                "track {}: {} ({} ch, {} Hz)",
                i + 1,
                meta.device_name,
                meta.channels,
                meta.sample_rate
            ));
            let status = Arc::new(Mutex::new(TrackStatus {
                state: TrackState::Starting,
                frames_written: 0,
                silence_frames: 0,
                reconnects: 0,
                idle: false,
                exported_frames: 0,
                load: Load::default(),
                levels: vec![0.0; usize::from(meta.channels)],
                warnings: Warnings::default(),
            }));
            // Export runs beside the recording; if it can't start, the track is
            // simply exported after Stop instead.
            let export = config.live_export.and_then(|format| {
                LiveExport::spawn(&dir, &meta, format)
                    .inspect_err(|e| log.line(&format!("live export unavailable: {e:#}")))
                    .ok()
            });
            let worker = TrackWorker {
                name: format!("track {} ({})", i + 1, meta.device_name),
                source: spec.source,
                writer: RawWriter::new(file, meta.channels),
                sample_rate: meta.sample_rate,
                channels: meta.channels,
                loopback: meta.loopback,
                start,
                stop_at: stop_at.clone(),
                status: status.clone(),
                log: log.clone(),
                config: config.clone(),
                export: export.as_ref().map(|e| e.control.clone()),
            };
            let thread = thread::Builder::new()
                .name(format!("track-{}", i + 1))
                .spawn(move || worker.run_guarded())
                .context("spawning track thread")?;
            tracks.push(Track {
                meta,
                status,
                thread: Some(thread),
                export,
            });
        }
        Ok(Self {
            dir,
            meta,
            start,
            stop_at,
            tracks,
            log,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn elapsed(&self) -> Duration {
        self.stop_at
            .get()
            .map_or_else(|| self.start.elapsed(), |t| *t - self.start)
    }

    pub fn statuses(&self) -> Vec<(TrackMeta, TrackStatus)> {
        self.tracks
            .iter()
            .map(|t| {
                (
                    t.meta.clone(),
                    t.status.lock().expect("status lock").clone(),
                )
            })
            .collect()
    }

    /// Stops all tracks and marks the session as stopped.
    ///
    /// A track thread stuck inside a driver call is abandoned after `timeout`;
    /// its raw file is still valid and gets exported with whatever it holds.
    pub fn stop(self, timeout: Duration) -> Result<(PathBuf, SessionMeta)> {
        self.stop_with_progress(timeout, &mut |_| {})
    }

    /// Like [`Recorder::stop`], then waits for live exports to finish the last
    /// few seconds. `progress` receives 0..=1 for that wait. Tracks whose live
    /// export failed keep their raw capture for [`crate::export::export_session`].
    pub fn stop_with_progress(
        mut self,
        timeout: Duration,
        progress: &mut dyn FnMut(f32),
    ) -> Result<(PathBuf, SessionMeta)> {
        let _ = self.stop_at.set(Instant::now());
        let deadline = Instant::now() + timeout;
        for track in &mut self.tracks {
            let Some(handle) = track.thread.take() else {
                continue;
            };
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                self.log.line(&format!(
                    "{} did not stop in time; abandoning it",
                    track.meta.device_name
                ));
                if let Some(e) = &track.export {
                    e.control.cancel.store(true, Ordering::Relaxed);
                }
            }
        }
        self.finish_exports(progress);
        self.meta.state = SessionState::Stopped;
        self.meta.save(&self.dir)?;
        self.log.line(&format!(
            "recording stopped after {:.1} s",
            self.elapsed().as_secs_f64()
        ));
        Ok((self.dir.clone(), self.meta.clone()))
    }
}

impl Recorder {
    fn finish_exports(&mut self, progress: &mut dyn FnMut(f32)) {
        let exports: Vec<(String, LiveExport)> = self
            .tracks
            .iter_mut()
            .filter_map(|t| Some((t.meta.device_name.clone(), t.export.take()?)))
            .collect();
        let total: u64 = exports
            .iter()
            .map(|(_, e)| e.control.final_frames.get().copied().unwrap_or(0))
            .sum::<u64>()
            .max(1);
        while !exports.iter().all(|(_, e)| e.is_finished()) {
            let done: u64 = exports
                .iter()
                .map(|(_, e)| e.control.done_frames.load(Ordering::Relaxed))
                .sum();
            progress(done as f32 / total as f32);
            thread::sleep(Duration::from_millis(50));
        }
        for (name, export) in exports {
            match export.join() {
                Ok(path) => self.log.line(&format!("exported {}", path.display())),
                Err(e) => self.log.line(&format!(
                    "{name}: live export did not complete ({e:#}); exporting from the raw capture"
                )),
            }
        }
        progress(1.0);
    }
}

/// Buffered writer for a raw capture file that tracks its length in frames.
struct RawWriter {
    file: BufWriter<File>,
    channels: usize,
    samples: u64,
    scratch: Vec<u8>,
}

impl RawWriter {
    fn new(file: File, channels: u16) -> Self {
        Self {
            file: BufWriter::with_capacity(256 * 1024, file),
            channels: usize::from(channels),
            samples: 0,
            scratch: Vec::new(),
        }
    }

    /// Whole frames written. Samples may arrive split mid-frame across calls.
    fn frames(&self) -> u64 {
        self.samples / self.channels as u64
    }

    fn write_samples(&mut self, samples: &[f32]) -> std::io::Result<()> {
        self.scratch.clear();
        self.scratch
            .extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        self.file.write_all(&self.scratch)?;
        self.samples += samples.len() as u64;
        Ok(())
    }

    fn write_silence(&mut self, frames: u64) -> std::io::Result<()> {
        static CHUNK: [u8; 64 * 1024] = [0; 64 * 1024];
        let mut bytes = frames * self.channels as u64 * 4;
        while bytes > 0 {
            let n = bytes.min(CHUNK.len() as u64) as usize;
            self.file.write_all(&CHUNK[..n])?;
            bytes -= n as u64;
        }
        self.samples += frames * self.channels as u64;
        Ok(())
    }

    /// Pads with silence up to `frames`; returns how many frames were added.
    fn pad_to(&mut self, frames: u64) -> std::io::Result<u64> {
        let missing = frames.saturating_sub(self.frames());
        if missing > 0 {
            self.write_silence(missing)?;
        }
        Ok(missing)
    }

    fn truncate_to(&mut self, frames: u64) -> std::io::Result<()> {
        if self.frames() > frames {
            self.file.flush()?;
            self.file
                .get_ref()
                .set_len(frames * self.channels as u64 * 4)?;
            self.samples = frames * self.channels as u64;
        }
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        self.file.get_ref().sync_data()
    }
}

struct OpenStream {
    _stream: ActiveStream,
    consumer: rtrb::Consumer<f32>,
    events: Receiver<StreamEvent>,
    last_data: Instant,
}

struct TrackWorker {
    name: String,
    source: Box<dyn InputSource>,
    writer: RawWriter,
    sample_rate: u32,
    channels: u16,
    loopback: bool,
    start: Instant,
    stop_at: Arc<OnceLock<Instant>>,
    status: Arc<Mutex<TrackStatus>>,
    log: Arc<SessionLog>,
    config: RecorderConfig,
    export: Option<Arc<LiveExportControl>>,
}

impl TrackWorker {
    fn run_guarded(mut self) {
        let status = self.status.clone();
        let log = self.log.clone();
        let name = self.name.clone();
        let export = self.export.clone();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| self.run()));
        let failure = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("disk write failed: {e}")),
            Err(_) => Some("internal error (panic)".to_string()),
        };
        if let Some(msg) = failure {
            if let Some(e) = &export {
                e.cancel.store(true, Ordering::Relaxed);
            }
            log.line(&format!("{name}: stopped recording: {msg}"));
            if let Ok(mut s) = status.lock() {
                s.state = TrackState::Failed(msg);
            }
        }
    }

    /// Frames that should exist on the shared timeline at `t`.
    fn timeline_frames(&self, t: Instant) -> u64 {
        (t.saturating_duration_since(self.start).as_secs_f64() * f64::from(self.sample_rate))
            .round() as u64
    }

    fn log_warning_changes(&self, old: &Warnings, new: &Warnings) {
        if new.choppy && !old.choppy {
            self.log.line(&format!(
                "{}: WARNING choppy audio (repeated dropouts; {} so far)",
                self.name, new.dropouts
            ));
        }
        if new.clipping && !old.clipping {
            self.log.line(&format!("{}: WARNING clipping", self.name));
        }
        if new.no_signal != old.no_signal {
            let what = if new.no_signal {
                "no signal"
            } else {
                "signal back"
            };
            self.log.line(&format!("{}: {what}", self.name));
        }
    }

    fn set_state(&self, state: TrackState) {
        self.status.lock().expect("status lock").state = state;
    }

    fn run(&mut self) -> std::io::Result<()> {
        let dropped = Arc::new(AtomicU64::new(0));
        let ring_len = (self.config.buffer.as_secs_f64() * f64::from(self.sample_rate)) as usize
            * usize::from(self.channels);
        // While the device is missing, stay slightly behind real time so audio
        // that arrives right after a reconnect is not preceded by too much silence.
        let slack = u64::from(self.sample_rate) / 4;
        // How far a track may run ahead of the shared timeline (device clock
        // drift). Anything beyond that is a misbehaving driver and is discarded,
        // so a device producing audio too fast cannot fill the disk.
        let max_ahead = (self.config.buffer.as_secs_f64() * f64::from(self.sample_rate)) as u64;
        let mut overflow_pending = 0u64;
        let mut discard_pending = 0u64;
        let mut last_report = Instant::now();
        let mut stream: Option<OpenStream> = None;
        let mut next_attempt = Instant::now();
        let mut last_error = String::new();
        let mut ever_connected = false;
        let mut last_sync = Instant::now();
        let mut silence = 0u64;
        let mut analyzer = Analyzer::new(self.channels, self.sample_rate);
        let mut warnings = Warnings::default();
        let mut levels = vec![0.0f32; usize::from(self.channels)];
        let mut last_meter = Instant::now();
        let mut idle = false;
        let callback_busy = Arc::new(AtomicU64::new(0));
        let mut writer_busy = Duration::ZERO;
        let mut load_window = (Instant::now(), 0u64, 0u64);
        let mut load = Load::default();

        loop {
            let now = Instant::now();
            let stop_at = self.stop_at.get().copied();

            if stream.is_none() && stop_at.is_none() && now >= next_attempt {
                let (producer, consumer) = rtrb::RingBuffer::new(ring_len.max(1));
                let (events_tx, events) = mpsc::channel();
                let sink = StreamSink {
                    producer,
                    dropped_samples: dropped.clone(),
                    events: events_tx,
                    busy_nanos: callback_busy.clone(),
                };
                match self.source.open(sink) {
                    Ok(s) => {
                        // Align the new stream with the shared timeline.
                        silence += self.writer.pad_to(self.timeline_frames(Instant::now()))?;
                        let verb = if ever_connected {
                            "reconnected"
                        } else {
                            "connected"
                        };
                        self.log.line(&format!("{}: {verb}", self.name));
                        let mut st = self.status.lock().expect("status lock");
                        st.state = TrackState::Recording;
                        if ever_connected {
                            st.reconnects += 1;
                        }
                        drop(st);
                        ever_connected = true;
                        last_error.clear();
                        analyzer.stream_restarted();
                        stream = Some(OpenStream {
                            _stream: s,
                            consumer,
                            events,
                            last_data: Instant::now(),
                        });
                    }
                    Err(e) => {
                        if e != last_error {
                            self.log.line(&format!(
                                "{}: cannot open device: {e} (retrying)",
                                self.name
                            ));
                            last_error = e.clone();
                        }
                        self.set_state(TrackState::Reconnecting(e));
                        next_attempt = now + self.config.retry_interval;
                    }
                }
            }

            let limit = self.timeline_frames(now) + max_ahead;
            if let Some(s) = stream.as_mut() {
                let (read, discarded) =
                    drain(&mut s.consumer, &mut self.writer, &mut analyzer, limit)?;
                if read > 0 {
                    s.last_data = now;
                }
                discard_pending += discarded;
            }
            let lost = dropped.swap(0, Ordering::Relaxed) / u64::from(self.channels);
            if lost > 0 {
                // Replace lost audio with silence, but never beyond real time.
                let target = (self.writer.frames() + lost).min(self.timeline_frames(now));
                silence += self.writer.pad_to(target)?;
                overflow_pending += lost;
            }
            if (overflow_pending > 0 || discard_pending > 0)
                && now.duration_since(last_report) >= Duration::from_secs(5)
            {
                if overflow_pending > 0 {
                    self.log.line(&format!(
                        "{}: buffer overflow, {overflow_pending} frames lost (gap filled with silence)",
                        self.name
                    ));
                }
                if discard_pending > 0 {
                    self.log.line(&format!(
                        "{}: device delivers audio faster than real time, discarded {discard_pending} frames",
                        self.name
                    ));
                }
                overflow_pending = 0;
                discard_pending = 0;
                last_report = now;
            }

            if let Some(s) = stream.as_mut() {
                let mut failure = None;
                while let Ok(ev) = s.events.try_recv() {
                    if ev.fatal {
                        failure = Some(ev.message);
                    } else {
                        self.log.line(&format!("{}: {}", self.name, ev.message));
                    }
                }
                // A quiet loopback source is not a stalled one.
                if failure.is_none()
                    && !self.loopback
                    && now.duration_since(s.last_data) > self.config.stall_timeout
                {
                    failure = Some("no audio received (stalled)".to_string());
                }
                if let Some(msg) = failure {
                    let mut dead = stream.take().expect("stream present");
                    drop(dead._stream);
                    drain(&mut dead.consumer, &mut self.writer, &mut analyzer, limit)?;
                    self.log.line(&format!("{}: lost device: {msg}", self.name));
                    self.set_state(TrackState::Reconnecting(msg.clone()));
                    last_error = msg;
                    next_attempt = now;
                }
            }

            let was_idle = idle;
            idle = self.loopback
                && stream
                    .as_ref()
                    .is_some_and(|s| now.duration_since(s.last_data) > self.config.idle_gap);
            if idle {
                // Nothing is playing: keep the track on the timeline so audio
                // that starts later lands in the right place.
                silence += self.writer.pad_to(self.timeline_frames(now))?;
                if !was_idle {
                    // Trailing zeros before the pause are not a dropout.
                    analyzer.stream_restarted();
                }
            }

            if stream.is_none() && stop_at.is_none() {
                silence += self
                    .writer
                    .pad_to(self.timeline_frames(now).saturating_sub(slack))?;
            }

            if let Some(t) = stop_at {
                if let Some(mut s) = stream.take() {
                    drop(s._stream);
                    drain(&mut s.consumer, &mut self.writer, &mut analyzer, limit)?;
                }
                let end = self.timeline_frames(t);
                silence += self.writer.pad_to(end)?;
                self.writer.truncate_to(end)?;
                self.writer.sync()?;
                if let Some(e) = &self.export {
                    let _ = e.final_frames.set(self.writer.frames());
                }
                let mut st = self.status.lock().expect("status lock");
                st.state = TrackState::Stopped;
                st.frames_written = self.writer.frames();
                st.silence_frames = silence;
                return Ok(());
            }

            if now.duration_since(last_sync) >= self.config.flush_interval {
                self.writer.sync()?;
                last_sync = now;
                if let Some(e) = &self.export {
                    // Frames up to the current timeline position can never be
                    // truncated at stop, so the exporter may consume them.
                    let ready = self.writer.frames().min(self.timeline_frames(now));
                    e.ready_frames.store(ready, Ordering::Release);
                }
            }
            // Meters fall by 20 dB per second after a peak.
            let decay = 10f32.powf(-now.duration_since(last_meter).as_secs_f32());
            last_meter = now;
            for (level, peak) in levels.iter_mut().zip(analyzer.take_peaks()) {
                *level = peak.max(*level * decay);
            }
            let current = analyzer.warnings();
            self.log_warning_changes(&warnings, &current);
            warnings = current;
            {
                let mut st = self.status.lock().expect("status lock");
                st.frames_written = self.writer.frames();
                st.silence_frames = silence;
                st.levels.clone_from(&levels);
                st.idle = idle;
                st.load = load;
                if let Some(e) = &self.export {
                    st.exported_frames = e.done_frames.load(Ordering::Relaxed);
                }
                st.warnings = warnings.clone();
            }
            writer_busy += now.elapsed();
            let window = now.duration_since(load_window.0);
            if window >= Duration::from_secs(1) {
                let cb = callback_busy.load(Ordering::Relaxed);
                let ex = self
                    .export
                    .as_ref()
                    .map_or(0, |e| e.busy_nanos.load(Ordering::Relaxed));
                let secs = window.as_secs_f32();
                load = Load {
                    callback: (cb - load_window.1) as f32 / 1e9 / secs,
                    writer: writer_busy.as_secs_f32() / secs,
                    export: (ex - load_window.2) as f32 / 1e9 / secs,
                };
                load_window = (now, cb, ex);
                writer_busy = Duration::ZERO;
            }
            thread::sleep(self.config.poll_interval);
        }
    }
}

/// Empties the ring buffer, writing to disk (and the analyzer) until the file
/// holds `max_frames` and discarding the rest. Returns (samples read, frames discarded).
fn drain(
    consumer: &mut rtrb::Consumer<f32>,
    writer: &mut RawWriter,
    analyzer: &mut Analyzer,
    max_frames: u64,
) -> std::io::Result<(usize, u64)> {
    let available = consumer.slots();
    let available = available - available % writer.channels;
    if available == 0 {
        return Ok((0, 0));
    }
    let room = max_frames.saturating_sub(writer.frames()) * writer.channels as u64;
    let keep = available.min(usize::try_from(room).unwrap_or(usize::MAX));
    let chunk = consumer.read_chunk(available).expect("slots checked");
    let (a, b) = chunk.as_slices();
    let from_a = keep.min(a.len());
    writer.write_samples(&a[..from_a])?;
    writer.write_samples(&b[..keep - from_a])?;
    analyzer.process(&a[..from_a]);
    analyzer.process(&b[..keep - from_a]);
    chunk.commit_all();
    Ok((available, ((available - keep) / writer.channels) as u64))
}

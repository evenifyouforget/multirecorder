//! Recorder behaviour under device failures, driven by scripted fake devices.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use multirecorder::audio::{ActiveStream, InputSource, StreamEvent, StreamSink};
use multirecorder::export::{ExportFormat, export_session};
use multirecorder::recorder::{Recorder, RecorderConfig, TrackSpec, TrackState};
use multirecorder::session::{self, SessionMeta, SessionState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Delivers audio in real time.
    Ok,
    /// Stream stays open but delivers nothing.
    Stalled,
    /// Device unplugged: running streams die, opening fails.
    Gone,
}

#[derive(Clone)]
struct Control(Arc<Mutex<Mode>>);

impl Control {
    fn new(mode: Mode) -> Self {
        Self(Arc::new(Mutex::new(mode)))
    }
    fn set(&self, mode: Mode) {
        *self.0.lock().unwrap() = mode;
    }
    fn get(&self) -> Mode {
        *self.0.lock().unwrap()
    }
}

struct FakeSource {
    control: Control,
    sample_rate: u32,
    channels: u16,
    /// Constant sample value, so tests can tell audio from inserted silence.
    value: f32,
}

struct FakeStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl InputSource for FakeSource {
    fn open(&mut self, mut sink: StreamSink) -> Result<ActiveStream, String> {
        if self.control.get() == Mode::Gone {
            return Err("device not found".into());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (control, rate, channels, value) = (
            self.control.clone(),
            self.sample_rate,
            self.channels,
            self.value,
        );
        let stop2 = stop.clone();
        let thread = thread::spawn(move || {
            let opened = Instant::now();
            let mut sent = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(5));
                let due = (opened.elapsed().as_secs_f64() * f64::from(rate)) as u64;
                match control.get() {
                    Mode::Ok => {
                        let frames = (due - sent) as usize;
                        sink.push(
                            &vec![value; frames * usize::from(channels)],
                            usize::from(channels),
                        );
                    }
                    Mode::Stalled => {}
                    Mode::Gone => {
                        let _ = sink.events.send(StreamEvent {
                            fatal: true,
                            message: "device removed".into(),
                        });
                        return;
                    }
                }
                sent = due;
            }
        });
        Ok(Box::new(FakeStream {
            stop,
            thread: Some(thread),
        }))
    }
}

fn spec(name: &str, control: &Control, sample_rate: u32, channels: u16, value: f32) -> TrackSpec {
    TrackSpec {
        device_name: name.into(),
        device_id: format!("fake:{name}"),
        sample_rate,
        channels,
        source: Box::new(FakeSource {
            control: control.clone(),
            sample_rate,
            channels,
            value,
        }),
    }
}

fn test_config() -> RecorderConfig {
    RecorderConfig {
        buffer: Duration::from_secs(1),
        stall_timeout: Duration::from_millis(300),
        retry_interval: Duration::from_millis(100),
        flush_interval: Duration::from_millis(100),
        poll_interval: Duration::from_millis(5),
    }
}

/// Reads a raw capture back as f32 samples.
fn read_raw(path: &std::path::Path) -> Vec<f32> {
    std::fs::read(path)
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

fn seconds_of_silence(samples: &[f32], channels: u16, rate: u32) -> f64 {
    let frames = samples
        .chunks_exact(usize::from(channels))
        .filter(|f| f.iter().all(|&s| s == 0.0))
        .count();
    frames as f64 / f64::from(rate)
}

#[test]
fn mixed_formats_stay_aligned() {
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    let rec = Recorder::start(
        root.path(),
        vec![
            spec("A", &ok, 48_000, 2, 0.5),
            spec("B", &ok, 44_100, 1, 0.25),
            spec("C", &ok, 96_000, 6, 0.1),
        ],
        test_config(),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(800));
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();
    assert_eq!(meta.state, SessionState::Stopped);

    let durations: Vec<f64> = meta
        .tracks
        .iter()
        .map(|t| {
            read_raw(&dir.join(&t.raw_file)).len() as f64
                / f64::from(t.channels)
                / f64::from(t.sample_rate)
        })
        .collect();
    for d in &durations {
        assert!(
            (d - durations[0]).abs() < 0.001,
            "durations differ: {durations:?}"
        );
        assert!(*d > 0.7 && *d < 1.5, "unexpected duration {d}");
    }
}

#[test]
fn disconnect_and_reconnect_is_padded_with_silence() {
    let root = tempfile::tempdir().unwrap();
    let steady = Control::new(Mode::Ok);
    let flaky = Control::new(Mode::Ok);
    let rec = Recorder::start(
        root.path(),
        vec![
            spec("steady", &steady, 48_000, 2, 0.5),
            spec("flaky", &flaky, 48_000, 2, 0.5),
        ],
        test_config(),
    )
    .unwrap();

    thread::sleep(Duration::from_millis(400));
    flaky.set(Mode::Gone);
    thread::sleep(Duration::from_millis(300));
    let statuses = rec.statuses();
    assert!(
        matches!(statuses[1].1.state, TrackState::Reconnecting(_)),
        "{:?}",
        statuses[1].1.state
    );
    assert_eq!(
        statuses[0].1.state,
        TrackState::Recording,
        "other devices keep recording"
    );
    thread::sleep(Duration::from_millis(300));
    flaky.set(Mode::Ok);
    thread::sleep(Duration::from_millis(500));

    let statuses = rec.statuses();
    assert_eq!(statuses[1].1.state, TrackState::Recording);
    assert_eq!(statuses[1].1.reconnects, 1);
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();

    let steady_raw = read_raw(&dir.join(&meta.tracks[0].raw_file));
    let flaky_raw = read_raw(&dir.join(&meta.tracks[1].raw_file));
    assert_eq!(
        steady_raw.len(),
        flaky_raw.len(),
        "tracks keep equal length"
    );
    let gap = seconds_of_silence(&flaky_raw, 2, 48_000);
    assert!(gap > 0.4 && gap < 0.9, "expected ~0.6 s gap, got {gap}");
    assert!(seconds_of_silence(&steady_raw, 2, 48_000) < 0.1);
}

#[test]
fn stalled_device_is_detected_and_padded() {
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    let staller = Control::new(Mode::Ok);
    let rec = Recorder::start(
        root.path(),
        vec![
            spec("ok", &ok, 48_000, 1, 0.5),
            spec("staller", &staller, 48_000, 1, 0.5),
        ],
        test_config(),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(300));
    staller.set(Mode::Stalled);
    thread::sleep(Duration::from_millis(1000));
    staller.set(Mode::Ok);
    thread::sleep(Duration::from_millis(600));
    let statuses = rec.statuses();
    assert_eq!(statuses[1].1.state, TrackState::Recording);
    assert!(statuses[1].1.reconnects >= 1);
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();

    let ok_raw = read_raw(&dir.join(&meta.tracks[0].raw_file));
    let stall_raw = read_raw(&dir.join(&meta.tracks[1].raw_file));
    assert_eq!(ok_raw.len(), stall_raw.len());
    let gap = seconds_of_silence(&stall_raw, 1, 48_000);
    assert!(gap > 0.7 && gap < 1.4, "expected ~1 s gap, got {gap}");
}

#[test]
fn missing_device_records_silence_without_blocking_others() {
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    let gone = Control::new(Mode::Gone);
    let rec = Recorder::start(
        root.path(),
        vec![
            spec("ok", &ok, 48_000, 2, 0.5),
            spec("gone", &gone, 48_000, 2, 0.5),
        ],
        test_config(),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(matches!(
        rec.statuses()[1].1.state,
        TrackState::Reconnecting(_)
    ));
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();
    let ok_raw = read_raw(&dir.join(&meta.tracks[0].raw_file));
    let gone_raw = read_raw(&dir.join(&meta.tracks[1].raw_file));
    assert_eq!(ok_raw.len(), gone_raw.len());
    assert!(gone_raw.iter().all(|&s| s == 0.0));
}

#[test]
fn late_device_is_aligned_to_session_start() {
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    let late = Control::new(Mode::Gone);
    let rec = Recorder::start(
        root.path(),
        vec![
            spec("ok", &ok, 48_000, 1, 0.5),
            spec("late", &late, 48_000, 1, 0.5),
        ],
        test_config(),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(500));
    late.set(Mode::Ok);
    thread::sleep(Duration::from_millis(500));
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();
    let late_raw = read_raw(&dir.join(&meta.tracks[1].raw_file));
    // Leading silence covers the time before the device appeared, audio follows.
    let first_audio = late_raw.iter().position(|&s| s != 0.0).unwrap() as f64 / 48_000.0;
    assert!(
        first_audio > 0.4 && first_audio < 0.75,
        "audio starts at {first_audio}"
    );
}

#[test]
fn data_on_disk_while_recording_is_recoverable() {
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    let rec = Recorder::start(
        root.path(),
        vec![spec("mic", &ok, 48_000, 2, 0.5)],
        test_config(),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(600));

    // Pretend the app just crashed: inspect the disk without stopping.
    let dir = rec.dir().to_path_buf();
    let found = session::find_unexported_sessions(root.path());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1.state, SessionState::Recording);
    let meta = SessionMeta::load(&dir).unwrap();
    let on_disk = std::fs::metadata(dir.join(&meta.tracks[0].raw_file))
        .unwrap()
        .len();
    let seconds = on_disk as f64 / (2.0 * 4.0 * 48_000.0);
    assert!(seconds > 0.3, "only {seconds} s flushed to disk");

    // A copy of the session as the crash left it exports fine.
    let copy = root.path().join("crashed");
    std::fs::create_dir(&copy).unwrap();
    for f in [session::META_FILE, &meta.tracks[0].raw_file] {
        std::fs::copy(dir.join(f), copy.join(f)).unwrap();
    }
    let outs = export_session(&copy, ExportFormat::Flac, &mut |_| {}).unwrap();
    assert!(std::fs::metadata(&outs[0]).unwrap().len() > 0);

    rec.stop(Duration::from_secs(5)).unwrap();
}

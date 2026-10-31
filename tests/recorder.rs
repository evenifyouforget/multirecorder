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
    /// Broken driver delivering audio far faster than real time.
    Flood,
    /// Glitching driver: a 20 ms gap of digital silence every 200 ms.
    Choppy,
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
                    Mode::Flood => {
                        let frames = 50 * (due - sent) as usize;
                        sink.push(
                            &vec![value; frames * usize::from(channels)],
                            usize::from(channels),
                        );
                    }
                    Mode::Choppy => {
                        let period = u64::from(rate) / 5;
                        let gap = u64::from(rate) / 50;
                        let frames: Vec<f32> = (sent..due)
                            .flat_map(|f| {
                                let s = if f % period < gap { 0.0 } else { value };
                                std::iter::repeat_n(s, usize::from(channels))
                            })
                            .collect();
                        sink.push(&frames, usize::from(channels));
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
        loopback: false,
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
        idle_gap: Duration::from_millis(100),
        live_export: None,
    }
}

/// Reads a raw capture back as f32 samples.
fn read_raw(path: &std::path::Path) -> Vec<f32> {
    std::fs::read(path)
        .unwrap()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
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

#[test]
fn runaway_device_cannot_fill_the_disk() {
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    let flood = Control::new(Mode::Flood);
    let config = test_config();
    let rec = Recorder::start(
        root.path(),
        vec![
            spec("ok", &ok, 48_000, 2, 0.5),
            spec("flood", &flood, 48_000, 2, 0.5),
        ],
        config.clone(),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(1500));
    let raw = rec.dir().join(&rec.statuses()[1].0.raw_file);
    let seconds_on_disk = std::fs::metadata(&raw).unwrap().len() as f64 / (2.0 * 4.0 * 48_000.0);
    let limit = rec.elapsed().as_secs_f64() + config.buffer.as_secs_f64() + 0.1;
    assert!(
        seconds_on_disk <= limit,
        "{seconds_on_disk} s on disk after {:?}",
        rec.elapsed()
    );
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();
    let ok_len = std::fs::metadata(dir.join(&meta.tracks[0].raw_file))
        .unwrap()
        .len();
    let flood_len = std::fs::metadata(dir.join(&meta.tracks[1].raw_file))
        .unwrap()
        .len();
    assert_eq!(ok_len, flood_len);
}

#[test]
fn choppy_device_raises_warning_and_meters_move() {
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    let choppy = Control::new(Mode::Choppy);
    let rec = Recorder::start(
        root.path(),
        vec![
            spec("ok", &ok, 48_000, 2, 0.5),
            spec("choppy", &choppy, 48_000, 2, 0.5),
        ],
        test_config(),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(1500));
    let statuses = rec.statuses();
    let (ok_st, choppy_st) = (&statuses[0].1, &statuses[1].1);
    assert!(!ok_st.warnings.any_problem(), "{:?}", ok_st.warnings);
    assert!(ok_st.levels.iter().all(|&l| l > 0.3), "{:?}", ok_st.levels);
    assert!(choppy_st.warnings.choppy, "{:?}", choppy_st.warnings);
    assert!(choppy_st.warnings.dropouts >= 4);
    let (dir, _) = rec.stop(Duration::from_secs(5)).unwrap();
    let log = std::fs::read_to_string(dir.join(session::LOG_FILE)).unwrap();
    assert!(log.contains("(choppy): WARNING choppy audio"), "{log}");
    // Individual dropouts are on record too, not only the choppy warning.
    let dropout_lines = |track: &str| {
        log.lines()
            .filter(|l| l.contains(track) && l.contains("dropout(s): short gap"))
            .count()
    };
    assert!(dropout_lines("(choppy)") >= 1, "{log}");
    assert_eq!(dropout_lines("(ok)"), 0, "{log}");
}

#[test]
fn monitor_checks_devices_without_recording() {
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    let choppy = Control::new(Mode::Choppy);
    let unplugged = Control::new(Mode::Ok);
    let rec = Recorder::monitor(
        root.path(),
        vec![
            spec("ok", &ok, 48_000, 2, 0.5),
            spec("choppy", &choppy, 48_000, 2, 0.5),
            spec("unplugged", &unplugged, 44_100, 1, 0.5),
        ],
        test_config(),
    )
    .unwrap();
    assert!(rec.is_monitor());
    thread::sleep(Duration::from_millis(400));
    unplugged.set(Mode::Gone);
    thread::sleep(Duration::from_millis(1100));

    let statuses = rec.statuses();
    assert!(!statuses[0].1.warnings.any_problem());
    assert!(statuses[0].1.levels.iter().all(|&l| l > 0.3));
    assert!(
        statuses[1].1.warnings.choppy,
        "{:?}",
        statuses[1].1.warnings
    );
    assert!(matches!(statuses[2].1.state, TrackState::Reconnecting(_)));
    let events = rec.take_events().join("\n");
    assert!(
        events.contains("monitoring started (not recording)"),
        "{events}"
    );
    assert!(
        events.contains("(choppy): WARNING choppy audio"),
        "{events}"
    );
    assert!(events.contains("(unplugged): lost device"), "{events}");
    assert!(rec.take_events().iter().all(|e| !e.contains("started")));

    let log_path = rec.stop_monitor(Duration::from_secs(5));
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("dropout(s)"), "{log}");
    assert!(log.contains("monitoring stopped after"), "{log}");
    // Nothing but the log was written: no session folder, no audio.
    let entries: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(entries, vec![log_path.clone()]);
    let name = log_path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("monitor-") && name.ends_with(".log"),
        "{name}"
    );
    assert!(session::find_unexported_sessions(root.path()).is_empty());
}

#[test]
fn quiet_loopback_is_padded_not_reconnected() {
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    // Windows loopback delivers no data at all while nothing is playing.
    let speakers = Control::new(Mode::Stalled);
    let mut loopback = spec("speakers", &speakers, 48_000, 2, 0.5);
    loopback.loopback = true;
    let rec = Recorder::start(
        root.path(),
        vec![spec("mic", &ok, 48_000, 2, 0.5), loopback],
        test_config(),
    )
    .unwrap();

    thread::sleep(Duration::from_millis(600));
    let st = rec.statuses()[1].1.clone();
    assert_eq!(st.state, TrackState::Recording);
    assert!(st.idle);
    // Kept up with the timeline while idle (minus idle_gap and poll jitter).
    assert!(
        st.frames_written as f64 / 48_000.0 > 0.4,
        "{}",
        st.frames_written
    );

    speakers.set(Mode::Ok);
    thread::sleep(Duration::from_millis(400));
    speakers.set(Mode::Stalled);
    thread::sleep(Duration::from_millis(500));
    speakers.set(Mode::Ok);
    thread::sleep(Duration::from_millis(300));

    let st = rec.statuses()[1].1.clone();
    assert!(!st.idle);
    assert_eq!(
        st.reconnects, 0,
        "quiet periods must not trigger reconnects"
    );
    assert_eq!(st.warnings.dropouts, 0);
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();

    let mic = read_raw(&dir.join(&meta.tracks[0].raw_file));
    let spk = read_raw(&dir.join(&meta.tracks[1].raw_file));
    assert_eq!(mic.len(), spk.len());
    // Playback started at ~0.6 s and resumed at ~1.5 s on the shared timeline.
    let frames: Vec<bool> = spk.as_chunks::<2>().0.iter().map(|f| f[0] != 0.0).collect();
    let first = frames.iter().position(|&a| a).unwrap() as f64 / 48_000.0;
    assert!((0.55..0.8).contains(&first), "audio starts at {first}");
    let resumed = frames[(1.2 * 48_000.0) as usize..]
        .iter()
        .position(|&a| a)
        .unwrap() as f64
        / 48_000.0
        + 1.2;
    assert!((1.45..1.7).contains(&resumed), "audio resumes at {resumed}");
}

fn live_config(format: ExportFormat) -> RecorderConfig {
    RecorderConfig {
        live_export: Some(format),
        ..test_config()
    }
}

#[test]
fn live_export_runs_during_recording_and_matches_timeline() {
    for format in [ExportFormat::Wav, ExportFormat::Flac] {
        let root = tempfile::tempdir().unwrap();
        let ok = Control::new(Mode::Ok);
        let flaky = Control::new(Mode::Ok);
        let rec = Recorder::start(
            root.path(),
            vec![
                spec("a", &ok, 48_000, 2, 0.5),
                spec("b", &flaky, 44_100, 1, 0.25),
            ],
            live_config(format),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(500));
        flaky.set(Mode::Gone);
        thread::sleep(Duration::from_millis(400));
        flaky.set(Mode::Ok);
        thread::sleep(Duration::from_millis(600));

        // Export is already under way while recording, a little behind real time.
        for (meta, st) in rec.statuses() {
            let behind =
                (st.frames_written - st.exported_frames) as f64 / f64::from(meta.sample_rate);
            assert!(st.exported_frames > 0 && behind < 0.5, "{format:?} {st:?}");
            // Load is measured for every stage and is far from saturated.
            let l = st.load;
            assert!(
                l.callback > 0.0 && l.writer > 0.0 && l.export > 0.0,
                "{l:?}"
            );
            assert!(l.max() < 0.5, "{l:?}");
        }

        let mut last = -1.0f32;
        let (dir, meta) = rec
            .stop_with_progress(Duration::from_secs(5), &mut |p| {
                assert!(p >= last && p <= 1.0);
                last = p;
            })
            .unwrap();
        assert_eq!(last, 1.0);
        for t in &meta.tracks {
            assert!(
                !dir.join(&t.raw_file).exists(),
                "raw removed after live export"
            );
        }
        let outs = export_session(&dir, format, &mut |_| {}).unwrap();
        assert_eq!(outs.len(), 2);
        assert_eq!(
            SessionMeta::load(&dir).unwrap().state,
            SessionState::Exported
        );

        // Equal duration on the shared timeline despite different rates and a gap.
        let secs: Vec<f64> = outs
            .iter()
            .map(|p| match format {
                ExportFormat::Wav => {
                    let r = hound::WavReader::open(p).unwrap();
                    f64::from(r.duration()) / f64::from(r.spec().sample_rate)
                }
                ExportFormat::Flac => {
                    let r = claxon::FlacReader::open(p).unwrap();
                    let i = r.streaminfo();
                    i.samples.unwrap() as f64 / f64::from(i.sample_rate)
                }
            })
            .collect();
        assert!((secs[0] - secs[1]).abs() < 0.001, "{format:?} {secs:?}");
        assert!(secs[0] > 1.3, "{secs:?}");
    }
}

#[test]
fn failed_live_export_falls_back_to_raw_capture() {
    // FLAC can't hold 10 channels, so the live export fails immediately.
    let root = tempfile::tempdir().unwrap();
    let ok = Control::new(Mode::Ok);
    let rec = Recorder::start(
        root.path(),
        vec![spec("wide", &ok, 48_000, 10, 0.5)],
        live_config(ExportFormat::Flac),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(400));
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();
    assert!(dir.join(&meta.tracks[0].raw_file).exists(), "raw kept");
    assert_eq!(meta.state, SessionState::Stopped);
    let log = std::fs::read_to_string(dir.join(session::LOG_FILE)).unwrap();
    assert!(log.contains("exporting from the raw capture"), "{log}");
    // The recording itself is unaffected and exports normally to WAV.
    let outs = export_session(&dir, ExportFormat::Wav, &mut |_| {}).unwrap();
    assert_eq!(
        hound::WavReader::open(&outs[0]).unwrap().spec().channels,
        10
    );
}

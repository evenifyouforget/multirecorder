//! App tracks run in a helper process. These tests use the helper's fake
//! backend to check that whatever the helper does (crash, hang, fail to
//! start, go quiet) only affects its own track, like an unplugged device.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use multirecorder::apps::{APP_CHANNELS, APP_SAMPLE_RATE, AppSource, FAKE_ARG};
use multirecorder::recorder::{Recorder, RecorderConfig, TrackSpec, TrackState, TrackStatus};
use multirecorder::session;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn config() -> RecorderConfig {
    RecorderConfig {
        retry_interval: Duration::from_millis(200),
        ..RecorderConfig::default()
    }
}

/// A fake app track whose behaviour is set by writing to `control`.
fn app(name: &str, control: &Path) -> TrackSpec {
    TrackSpec {
        device_name: name.into(),
        device_id: format!("app:{name}"),
        sample_rate: APP_SAMPLE_RATE,
        channels: APP_CHANNELS,
        loopback: true,
        source: Box::new(AppSource::with_command(
            PathBuf::from(env!("CARGO_BIN_EXE_multirecorder")),
            vec![FAKE_ARG.into(), control.display().to_string()],
        )),
    }
}

fn set(control: &Path, mode: &str) {
    std::fs::write(control, mode).unwrap();
}

/// Waits up to `secs` for a track to reach a state.
fn wait_for(
    rec: &Recorder,
    track: usize,
    secs: f64,
    ok: impl Fn(&TrackStatus) -> bool,
) -> TrackStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(secs);
    loop {
        let st = rec.statuses()[track].1.clone();
        if ok(&st) || std::time::Instant::now() > deadline {
            return st;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// The session log so far, for failure messages.
fn log_of(rec: &Recorder) -> String {
    std::fs::read_to_string(rec.dir().join(session::LOG_FILE)).unwrap_or_default()
}

fn audio_seconds(path: &Path) -> f64 {
    let bytes = std::fs::read(path).unwrap();
    let audio = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|b| f32::from_le_bytes(**b) != 0.0)
        .count();
    audio as f64 / f64::from(APP_CHANNELS) / f64::from(APP_SAMPLE_RATE)
}

#[test]
fn crashing_or_hung_helper_only_affects_its_track() {
    let _serial = serial();
    let root = tempfile::tempdir().unwrap();
    let (steady, flaky) = (root.path().join("steady"), root.path().join("flaky"));
    set(&steady, "ok");
    set(&flaky, "ok");
    let rec = Recorder::start(
        root.path(),
        vec![app("steady", &steady), app("flaky", &flaky)],
        config(),
    )
    .unwrap();
    let st = wait_for(&rec, 1, 5.0, |s| s.state == TrackState::Recording);
    assert_eq!(st.state, TrackState::Recording);
    thread::sleep(Duration::from_millis(500));

    // The helper crashes, then hangs (the fake does each once and works
    // again when restarted). Each time only its own track reconnects.
    set(&flaky, "crash");
    let st = wait_for(&rec, 1, 6.0, |s| {
        s.state == TrackState::Recording && s.reconnects >= 1
    });
    assert_eq!(
        (st.state, st.reconnects),
        (TrackState::Recording, 1),
        "{}",
        log_of(&rec)
    );
    thread::sleep(Duration::from_millis(300));
    set(&flaky, "hang");
    let st = wait_for(&rec, 1, 10.0, |s| {
        s.state == TrackState::Recording && s.reconnects >= 2
    });
    assert_eq!(
        (st.state, st.reconnects),
        (TrackState::Recording, 2),
        "{}",
        log_of(&rec)
    );
    thread::sleep(Duration::from_millis(500));

    let steady_st = rec.statuses()[0].1.clone();
    assert_eq!(steady_st.state, TrackState::Recording);
    assert_eq!(steady_st.reconnects, 0, "the other track never noticed");
    let elapsed = rec.elapsed().as_secs_f64();
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();

    // Equal length; the steady track is audio almost throughout.
    let len = |i: usize| {
        std::fs::metadata(dir.join(&meta.tracks[i].raw_file))
            .unwrap()
            .len()
    };
    assert_eq!(len(0), len(1));
    let steady_audio = audio_seconds(&dir.join(&meta.tracks[0].raw_file));
    assert!(
        steady_audio > elapsed - 0.5,
        "steady track has {steady_audio:.2} s of audio in {elapsed:.2} s"
    );
    let log = std::fs::read_to_string(dir.join(session::LOG_FILE)).unwrap();
    assert!(log.contains("capture helper not responding"), "{log}");
    assert!(log.contains("(flaky): reconnected"), "{log}");
    assert!(log.contains("fake capture started"), "{log}");
}

#[test]
fn app_not_available_waits_and_quiet_app_is_not_a_failure() {
    let _serial = serial();
    let root = tempfile::tempdir().unwrap();
    let control = root.path().join("app");
    set(&control, "fail Firefox is not running");
    let rec = Recorder::start(root.path(), vec![app("firefox", &control)], config()).unwrap();
    let st = wait_for(&rec, 0, 3.0, |s| {
        matches!(s.state, TrackState::Reconnecting(_))
    });
    assert_eq!(
        st.state,
        TrackState::Reconnecting("Firefox is not running".into())
    );
    thread::sleep(Duration::from_millis(600));

    // The app starts, then goes quiet (paused video): heartbeats keep the
    // helper alive and the track is padded with silence, not reconnected.
    set(&control, "ok");
    let st = wait_for(&rec, 0, 5.0, |s| s.state == TrackState::Recording);
    assert_eq!(st.state, TrackState::Recording);
    thread::sleep(Duration::from_millis(500));
    set(&control, "silent");
    thread::sleep(Duration::from_secs(4));
    let st = rec.statuses()[0].1.clone();
    assert_eq!(
        (st.state.clone(), st.reconnects),
        (TrackState::Recording, 0)
    );
    assert!(st.idle, "a quiet app is idle, like a quiet output device");
    let elapsed = rec.elapsed().as_secs_f64();
    let (dir, meta) = rec.stop(Duration::from_secs(5)).unwrap();
    let frames = std::fs::metadata(dir.join(&meta.tracks[0].raw_file))
        .unwrap()
        .len()
        / 4
        / u64::from(APP_CHANNELS);
    let secs = frames as f64 / f64::from(APP_SAMPLE_RATE);
    assert!(
        (secs - elapsed).abs() < 0.3,
        "track {secs:.2} s, session {elapsed:.2} s"
    );
    let log = std::fs::read_to_string(dir.join(session::LOG_FILE)).unwrap();
    assert_eq!(log.matches("Firefox is not running").count(), 1, "{log}");
}

/// The real Windows helper, on CI's Windows machine (which may have no audio
/// devices): it must answer cleanly, never crash or hang.
#[cfg(windows)]
#[test]
fn windows_helper_answers_cleanly() {
    use std::process::Command;
    let exe = env!("CARGO_BIN_EXE_multirecorder");
    let run = |args: &[&str]| {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code(), out.stdout)
    };
    // Listing: JSON lines, or one "error:" line if there's no audio service.
    let (code, stdout) = run(&[multirecorder::apps::LIST_ARG]);
    let text = String::from_utf8_lossy(&stdout);
    assert!(matches!(code, Some(0 | 1)), "{code:?}: {text}");
    for line in text.lines() {
        assert!(
            line.starts_with("error: ") || line.starts_with('{'),
            "unexpected line: {line}"
        );
    }
    // Capturing an app that isn't running fails with a readable message.
    let (code, stdout) = run(&[
        multirecorder::apps::CAPTURE_ARG,
        "include",
        "no-such-app-4f2a.exe",
    ]);
    assert_eq!(code, Some(1));
    assert_eq!(stdout[0], b'F');
    let msg = String::from_utf8_lossy(&stdout[5..]);
    assert_eq!(msg, "no-such-app-4f2a is not running");
}

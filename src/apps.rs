//! Recording individual apps (experimental, Windows).
//!
//! Capturing one program's sound needs low-level Windows calls, so it runs in
//! a separate helper process: this same executable started with
//! `--app-capture`. A bug there can only crash the helper; the recorder sees
//! that like an unplugged device (the track is filled with silence and the
//! helper is restarted). Nothing here runs unless an app is selected.
//!
//! The helper writes framed messages to stdout: a kind byte, a little-endian
//! u32 payload length, then the payload.
//! - `A`: interleaved f32 samples (little-endian), [`APP_CHANNELS`] channels
//!   at [`APP_SAMPLE_RATE`]. Nothing is sent while the app is silent.
//! - `H`: heartbeat, sent at least every [`HEARTBEAT`] while running.
//! - `I`: an informational message (UTF-8), logged by the recorder.
//! - `F`: a fatal message (UTF-8); the helper exits right after.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::audio::{ActiveStream, InputSource, StreamEvent, StreamSink};

#[cfg(windows)]
mod win;

/// Format every app track is captured in (Windows converts to it).
pub const APP_SAMPLE_RATE: u32 = 48_000;
pub const APP_CHANNELS: u16 = 2;
/// The helper sends something at least this often.
pub const HEARTBEAT: Duration = Duration::from_millis(250);
/// A helper silent for this long is considered hung and is killed.
const HUNG_AFTER: Duration = Duration::from_secs(3);
/// How long starting the helper may take before its first message.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest frame payload accepted (a corrupt length must not allocate GBs).
const MAX_FRAME: u32 = 16 * 1024 * 1024;

pub const CAPTURE_ARG: &str = "--app-capture";
pub const LIST_ARG: &str = "--list-apps";
/// Test-only backend producing a constant tone, controlled through a file.
pub const FAKE_ARG: &str = "--app-capture-fake";

/// Which sound an app track records.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppTarget {
    /// Executable file name, e.g. `firefox.exe` (matched case-insensitively).
    pub exe: String,
    /// Everything except this app (and its child processes).
    pub exclude: bool,
}

impl AppTarget {
    const KEY_INCLUDE: &str = "app:";
    const KEY_EXCLUDE: &str = "app-except:";

    /// Key for remembering the selection, next to device keys.
    pub fn key(&self) -> String {
        let prefix = if self.exclude {
            Self::KEY_EXCLUDE
        } else {
            Self::KEY_INCLUDE
        };
        format!("{prefix}{}", self.exe)
    }

    pub fn from_key(key: &str) -> Option<Self> {
        let (exe, exclude) = if let Some(exe) = key.strip_prefix(Self::KEY_EXCLUDE) {
            (exe, true)
        } else {
            (key.strip_prefix(Self::KEY_INCLUDE)?, false)
        };
        (!exe.is_empty()).then(|| Self {
            exe: exe.to_string(),
            exclude,
        })
    }

    /// Display name: the executable without `.exe`.
    pub fn app_name(&self) -> &str {
        let lower = self.exe.to_ascii_lowercase();
        if lower.ends_with(".exe") {
            &self.exe[..self.exe.len() - 4]
        } else {
            &self.exe
        }
    }

    /// Track (and file) name, e.g. "firefox (app)" or "All except discord".
    pub fn track_name(&self) -> String {
        if self.exclude {
            format!("All except {}", self.app_name())
        } else {
            format!("{} (app)", self.app_name())
        }
    }
}

/// A program currently known to Windows' audio sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub exe: String,
    /// Making sound right now.
    pub playing: bool,
}

/// Whether this platform can record apps.
pub fn supported() -> bool {
    cfg!(windows)
}

fn helper_command(program: &PathBuf, args: &[String]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Programs with audio sessions, from a short-lived helper. Blocking (up to a
/// few seconds); call from a background thread. Empty where unsupported.
pub fn list_apps() -> Result<Vec<AppInfo>, String> {
    if !supported() {
        return Ok(Vec::new());
    }
    let program = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = helper_command(&program, &[LIST_ARG.to_string()])
        .spawn()
        .map_err(|e| format!("cannot start helper: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped");
    let reader = thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).map(|_| text)
    });
    let deadline = Instant::now() + START_TIMEOUT;
    while child.try_wait().ok().flatten().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("listing apps timed out".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    let text = reader
        .join()
        .map_err(|_| "reader failed".to_string())?
        .map_err(|e| e.to_string())?;
    parse_app_list(&text)
}

fn parse_app_list(text: &str) -> Result<Vec<AppInfo>, String> {
    let mut apps: Vec<AppInfo> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if let Some(err) = line.strip_prefix("error: ") {
            return Err(err.to_string());
        }
        let app: AppInfo = serde_json::from_str(line).map_err(|e| e.to_string())?;
        match apps
            .iter_mut()
            .find(|a| a.exe.eq_ignore_ascii_case(&app.exe))
        {
            Some(a) => a.playing |= app.playing,
            None => apps.push(app),
        }
    }
    apps.sort_by_key(|a| (!a.playing, a.exe.to_ascii_lowercase()));
    Ok(apps)
}

/// Records one app (or everything but one app) through the helper process.
pub struct AppSource {
    program: PathBuf,
    args: Vec<String>,
}

impl AppSource {
    pub fn new(target: &AppTarget) -> Result<Self, String> {
        let program = std::env::current_exe().map_err(|e| e.to_string())?;
        let mode = if target.exclude { "exclude" } else { "include" };
        Ok(Self {
            program,
            args: vec![CAPTURE_ARG.into(), mode.into(), target.exe.clone()],
        })
    }

    /// A source running `program` with `args` as its helper (for tests).
    pub fn with_command(program: PathBuf, args: Vec<String>) -> Self {
        Self { program, args }
    }
}

/// A running helper; dropping it stops (kills) the helper.
struct AppStream {
    child: Arc<Mutex<Child>>,
    stopping: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Drop for AppStream {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

enum Started {
    Ok,
    Failed(String),
}

impl InputSource for AppSource {
    fn open(&mut self, mut sink: StreamSink) -> Result<ActiveStream, String> {
        let mut child = helper_command(&self.program, &self.args)
            .spawn()
            .map_err(|e| format!("cannot start capture helper: {e}"))?;
        let mut stdout = child.stdout.take().expect("piped");
        let child = Arc::new(Mutex::new(child));
        let stopping = Arc::new(AtomicBool::new(false));
        let opened = Instant::now();
        // Milliseconds since `opened` of the helper's last message.
        let last_seen = Arc::new(AtomicU64::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let watchdog_events = sink.events.clone();

        let reader = {
            let (child, stopping, last_seen) = (child.clone(), stopping.clone(), last_seen.clone());
            let events = sink.events.clone();
            thread::Builder::new()
                .name("app-capture-reader".into())
                .spawn(move || {
                    let mut started = Some(started_tx);
                    let mut fatal = None;
                    let mut samples = Vec::new();
                    loop {
                        let frame = match read_frame(&mut stdout) {
                            Ok(Some(f)) => f,
                            Ok(None) => break,
                            Err(e) => {
                                fatal = Some(format!("bad data from capture helper: {e}"));
                                break;
                            }
                        };
                        last_seen.store(opened.elapsed().as_millis() as u64, Ordering::Relaxed);
                        match frame.0 {
                            b'F' => {
                                fatal = Some(String::from_utf8_lossy(&frame.1).into_owned());
                                break;
                            }
                            kind => {
                                // Running once it delivers audio or a heartbeat;
                                // an info message may still precede a failure.
                                if matches!(kind, b'A' | b'H')
                                    && let Some(tx) = started.take()
                                {
                                    let _ = tx.send(Started::Ok);
                                }
                                match kind {
                                    b'A' => {
                                        samples.clear();
                                        samples.extend(
                                            frame
                                                .1
                                                .as_chunks::<4>()
                                                .0
                                                .iter()
                                                .map(|b| f32::from_le_bytes(*b)),
                                        );
                                        sink.push(&samples, usize::from(APP_CHANNELS));
                                    }
                                    b'I' => {
                                        let _ = events.send(StreamEvent {
                                            fatal: false,
                                            message: String::from_utf8_lossy(&frame.1).into_owned(),
                                        });
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    if stopping.load(Ordering::Relaxed) {
                        return;
                    }
                    let message = fatal.unwrap_or_else(|| {
                        let status = child.lock().ok().and_then(|mut c| c.wait().ok());
                        match status {
                            Some(s) => format!("capture helper stopped ({s})"),
                            None => "capture helper stopped".into(),
                        }
                    });
                    match started.take() {
                        Some(tx) => {
                            let _ = tx.send(Started::Failed(message));
                        }
                        None => {
                            let _ = events.send(StreamEvent {
                                fatal: true,
                                message,
                            });
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };

        let mut stream = AppStream {
            child: child.clone(),
            stopping: stopping.clone(),
            threads: vec![reader],
        };
        match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(Started::Ok) => {}
            Ok(Started::Failed(msg)) => return Err(msg),
            Err(_) => return Err("capture helper did not start".into()),
        }

        let watchdog = {
            let events = watchdog_events;
            thread::Builder::new()
                .name("app-capture-watchdog".into())
                .spawn(move || {
                    while !stopping.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(100));
                        let seen = Duration::from_millis(last_seen.load(Ordering::Relaxed));
                        if opened.elapsed().saturating_sub(seen) > HUNG_AFTER {
                            let _ = events.send(StreamEvent {
                                fatal: true,
                                message: "capture helper not responding".into(),
                            });
                            stopping.store(true, Ordering::Relaxed);
                            if let Ok(mut c) = child.lock() {
                                let _ = c.kill();
                            }
                            return;
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        stream.threads.push(watchdog);
        Ok(Box::new(stream))
    }
}

fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(header[1..].try_into().expect("4 bytes"));
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(Some((header[0], payload)))
}

/// Helper side: writes frames to stdout.
pub struct FrameWriter<W: Write> {
    out: W,
    buf: Vec<u8>,
}

impl<W: Write> FrameWriter<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            buf: Vec::new(),
        }
    }

    fn frame(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        self.buf.clear();
        self.buf.push(kind);
        self.buf
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(payload);
        self.out.write_all(&self.buf)?;
        self.out.flush()
    }

    pub fn audio(&mut self, samples: &[f32]) -> io::Result<()> {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        self.frame(b'A', &bytes)
    }

    pub fn heartbeat(&mut self) -> io::Result<()> {
        self.frame(b'H', &[])
    }

    pub fn info(&mut self, msg: &str) -> io::Result<()> {
        self.frame(b'I', msg.as_bytes())
    }

    pub fn fatal(&mut self, msg: &str) -> io::Result<()> {
        self.frame(b'F', msg.as_bytes())
    }
}

/// Runs the helper if the command line asks for it; returns its exit code.
/// Called first thing in `main`, before any window is created.
pub fn run_helper(args: &[String]) -> Option<i32> {
    let first = args.get(1)?.as_str();
    let stdout = io::stdout().lock();
    let code = match first {
        LIST_ARG => list_main(stdout),
        CAPTURE_ARG => {
            let mut out = FrameWriter::new(stdout);
            let target = match (args.get(2).map(String::as_str), args.get(3)) {
                (Some(mode @ ("include" | "exclude")), Some(exe)) => AppTarget {
                    exe: exe.clone(),
                    exclude: mode == "exclude",
                },
                _ => {
                    let _ = out.fatal("usage: --app-capture include|exclude <app.exe>");
                    return Some(2);
                }
            };
            capture_main(&target, &mut out)
        }
        FAKE_ARG => {
            let mut out = FrameWriter::new(stdout);
            match args.get(2) {
                Some(control) => fake_main(&PathBuf::from(control), &mut out),
                None => 2,
            }
        }
        _ => return None,
    };
    Some(code)
}

#[cfg(windows)]
fn list_main(mut out: impl Write) -> i32 {
    match win::list_apps() {
        Ok(apps) => {
            for app in apps {
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::to_string(&app).expect("serializable")
                );
            }
            0
        }
        Err(e) => {
            let _ = writeln!(out, "error: {e}");
            1
        }
    }
}

#[cfg(not(windows))]
fn list_main(_out: impl Write) -> i32 {
    0
}

#[cfg(windows)]
fn capture_main(target: &AppTarget, out: &mut FrameWriter<impl Write>) -> i32 {
    match win::capture(target, out) {
        Ok(()) => 0,
        Err(e) => {
            let _ = out.fatal(&e);
            1
        }
    }
}

#[cfg(not(windows))]
fn capture_main(_target: &AppTarget, out: &mut FrameWriter<impl Write>) -> i32 {
    let _ = out.fatal("recording apps is only supported on Windows");
    1
}

/// Test backend: a constant 0.25 tone in real time. The control file's first
/// word picks the behaviour and is re-read every tick:
/// `ok`, `silent` (heartbeats only, like a paused app), `hang` (stops
/// writing), `crash` (exits abruptly), `fail <message>` (fatal message).
/// `hang` and `crash` happen once: they reset the file to `ok` first, so the
/// restarted helper works.
fn fake_main(control: &PathBuf, out: &mut FrameWriter<impl Write>) -> i32 {
    let started = Instant::now();
    let rate = f64::from(APP_SAMPLE_RATE);
    let mut sent = 0u64;
    let mut last_beat = Instant::now();
    if out.info("fake capture started").is_err() {
        return 1;
    }
    loop {
        let mode = std::fs::read_to_string(control).unwrap_or_default();
        let mut words = mode.split_whitespace();
        let due = (started.elapsed().as_secs_f64() * rate) as u64;
        let result = match words.next().unwrap_or("ok") {
            "crash" => {
                let _ = std::fs::write(control, "ok");
                std::process::exit(3)
            }
            "hang" => {
                let _ = std::fs::write(control, "ok");
                loop {
                    thread::sleep(Duration::from_secs(1));
                }
            }
            "fail" => {
                let msg: Vec<&str> = words.collect();
                let _ = out.fatal(&msg.join(" "));
                return 1;
            }
            "silent" => {
                sent = due;
                Ok(())
            }
            _ => {
                let frames = (due - sent) as usize;
                sent = due;
                if frames > 0 {
                    out.audio(&vec![0.25; frames * usize::from(APP_CHANNELS)])
                } else {
                    Ok(())
                }
            }
        };
        let result = result.and_then(|()| {
            if last_beat.elapsed() >= HEARTBEAT {
                last_beat = Instant::now();
                out.heartbeat()
            } else {
                Ok(())
            }
        });
        if result.is_err() {
            // The recorder went away.
            return 0;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session;

    #[test]
    fn target_keys_round_trip() {
        for exclude in [false, true] {
            let t = AppTarget {
                exe: "Firefox.exe".into(),
                exclude,
            };
            assert_eq!(AppTarget::from_key(&t.key()), Some(t.clone()));
        }
        assert_eq!(AppTarget::from_key("wasapi:abc"), None);
        assert_eq!(AppTarget::from_key("app:"), None);
        let t = AppTarget::from_key("app-except:Discord.EXE").unwrap();
        assert_eq!(t.track_name(), "All except Discord");
        let t = AppTarget::from_key("app:firefox.exe").unwrap();
        assert_eq!(t.track_name(), "firefox (app)");
        assert_eq!(
            session::sanitize_file_name(&t.track_name()),
            "firefox (app)"
        );
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        let mut w = FrameWriter::new(&mut buf);
        w.info("hi").unwrap();
        w.audio(&[0.5, -0.5]).unwrap();
        w.heartbeat().unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r).unwrap(), Some((b'I', b"hi".to_vec())));
        let (kind, audio) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!((kind, audio.len()), (b'A', 8));
        assert_eq!(read_frame(&mut r).unwrap(), Some((b'H', vec![])));
        assert_eq!(read_frame(&mut r).unwrap(), None);
        // A truncated or absurd frame is an error, not a huge allocation.
        assert!(read_frame(&mut [b'A', 255, 255, 255, 255].as_slice()).is_err());
        assert!(read_frame(&mut [b'A', 4, 0, 0, 0, 1].as_slice()).is_err());
    }

    #[test]
    fn app_list_merges_and_sorts() {
        let text = "{\"exe\":\"spotify.exe\",\"playing\":false}\n\
                    {\"exe\":\"firefox.exe\",\"playing\":false}\n\
                    {\"exe\":\"Firefox.exe\",\"playing\":true}\n";
        let apps = parse_app_list(text).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(
            (apps[0].exe.as_str(), apps[0].playing),
            ("firefox.exe", true)
        );
        assert!(parse_app_list("error: no audio service").is_err());
    }
}

//! On-disk layout of a recording session.
//!
//! ```text
//! <output dir>/session-YYYYMMDD-HHMMSS/
//!     session.json        metadata, always replaced atomically
//!     session.log         human-readable event log
//!     01-<device>.f32     raw capture: interleaved little-endian f32 samples
//!     01-<device>.wav     exported track (or .flac)
//! ```
//!
//! Raw captures have no header, so a crash can never leave them unreadable:
//! the file length *is* the amount of recorded audio.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const META_FILE: &str = "session.json";
pub const LOG_FILE: &str = "session.log";
pub const RAW_EXT: &str = "f32";
const META_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Recording in progress, or the app died while recording.
    Recording,
    /// Recording finished; raw captures not yet exported.
    Stopped,
    /// Tracks exported and raw captures removed.
    Exported,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TrackMeta {
    /// Raw capture file name, relative to the session directory.
    pub raw_file: String,
    pub device_name: String,
    pub device_id: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Output device recorded via loopback. Absent in sessions from v0.1.
    #[serde(default)]
    pub loopback: bool,
}

impl TrackMeta {
    pub fn bytes_per_frame(&self) -> u64 {
        u64::from(self.channels) * 4
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SessionMeta {
    pub version: u32,
    pub started_at: String,
    pub state: SessionState,
    pub tracks: Vec<TrackMeta>,
}

impl SessionMeta {
    pub fn new(tracks: Vec<TrackMeta>) -> Self {
        Self {
            version: META_VERSION,
            started_at: chrono::Local::now().to_rfc3339(),
            state: SessionState::Recording,
            tracks,
        }
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(META_FILE);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Writes to a temporary file, syncs it, then renames over the old file so a
    /// crash leaves either the old or the new metadata, never a torn file.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let tmp = dir.join(format!("{META_FILE}.tmp"));
        let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, dir.join(META_FILE)).context("replacing session.json")?;
        Ok(())
    }
}

/// Makes a device name safe to use as part of a file name on every OS.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_.()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    let short: String = trimmed.chars().take(60).collect();
    if short.is_empty() {
        "device".to_string()
    } else {
        short
    }
}

/// Raw capture file name for the `index`-th (0-based) track.
pub fn raw_file_name(index: usize, device_name: &str) -> String {
    format!(
        "{:02}-{}.{RAW_EXT}",
        index + 1,
        sanitize_file_name(device_name)
    )
}

/// Creates a new, uniquely named session directory under `root`.
pub fn create_session_dir(root: &Path) -> Result<PathBuf> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let base = format!("session-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    for n in 1.. {
        let name = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let dir = root.join(name);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
    }
    unreachable!()
}

/// Sessions under `root` that still have raw captures waiting to be exported,
/// including sessions interrupted by a crash. Newest first.
pub fn find_unexported_sessions(root: &Path) -> Vec<(PathBuf, SessionMeta)> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, SessionMeta)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| SessionMeta::load(&p).ok().map(|m| (p, m)))
        .filter(|(p, m)| {
            m.state != SessionState::Exported
                && m.tracks.iter().any(|t| p.join(&t.raw_file).exists())
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found
}

/// Append-only event log stored next to the recording, flushed on every line.
pub struct SessionLog {
    file: Mutex<Option<File>>,
}

impl SessionLog {
    pub fn open(dir: &Path) -> Self {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(LOG_FILE));
        if let Err(e) = &file {
            log::warn!("cannot open session log: {e}");
        }
        Self {
            file: Mutex::new(file.ok()),
        }
    }

    pub fn line(&self, msg: &str) {
        log::info!("{msg}");
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        if let Ok(mut guard) = self.file.lock()
            && let Some(f) = guard.as_mut()
        {
            // Logging must never take recording down with it, so errors are ignored.
            let _ = writeln!(f, "{stamp} {msg}");
            let _ = f.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_file_name("Mic (USB) 2"), "Mic (USB) 2");
        assert_eq!(sanitize_file_name("a/b\\c:d*e?"), "a_b_c_d_e_");
        assert_eq!(sanitize_file_name("  ..."), "device");
        assert_eq!(sanitize_file_name("name."), "name");
        assert_eq!(raw_file_name(0, "Line In"), "01-Line In.f32");
    }

    #[test]
    fn reads_v1_metadata_without_loopback_field() {
        let json = r#"{"version":1,"started_at":"x","state":"stopped","tracks":[
            {"raw_file":"01-a.f32","device_name":"a","device_id":"i","sample_rate":48000,"channels":2}]}"#;
        let meta: SessionMeta = serde_json::from_str(json).unwrap();
        assert!(!meta.tracks[0].loopback);
    }

    #[test]
    fn meta_roundtrip_and_discovery() {
        let root = tempfile::tempdir().unwrap();
        let dir = create_session_dir(root.path()).unwrap();
        let dir2 = create_session_dir(root.path()).unwrap();
        assert_ne!(dir, dir2);

        let track = TrackMeta {
            raw_file: raw_file_name(0, "Mic"),
            device_name: "Mic".into(),
            device_id: "x".into(),
            sample_rate: 48000,
            channels: 2,
            loopback: false,
        };
        let mut meta = SessionMeta::new(vec![track.clone()]);
        meta.save(&dir).unwrap();
        assert_eq!(SessionMeta::load(&dir).unwrap(), meta);

        // No raw file yet -> nothing to export.
        assert!(find_unexported_sessions(root.path()).is_empty());
        fs::write(dir.join(&track.raw_file), [0u8; 8]).unwrap();
        assert_eq!(find_unexported_sessions(root.path()).len(), 1);

        meta.state = SessionState::Exported;
        meta.save(&dir).unwrap();
        assert!(find_unexported_sessions(root.path()).is_empty());
    }
}

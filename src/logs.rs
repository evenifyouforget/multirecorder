//! Finding and reading the event logs in an output folder, for the log viewer.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::session::{self, SessionMeta};

/// Logs larger than this are shown from the end.
const MAX_SHOWN_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct LogEntry {
    pub path: PathBuf,
    /// Session folder or monitor log file name.
    pub name: String,
    pub monitor: bool,
    /// From session.json; recordings only.
    pub started_at: Option<String>,
    modified: SystemTime,
}

/// Recording logs (`session-*/session.log`) and monitor logs
/// (`monitor-*.log`) directly under `root`, newest first.
pub fn find_logs(root: &Path) -> Vec<LogEntry> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<LogEntry> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let (log, monitor, started_at) = if path.is_dir() {
                let log = path.join(session::LOG_FILE);
                let started_at = SessionMeta::load(&path).ok().map(|m| m.started_at);
                (log, false, started_at)
            } else if name.starts_with("monitor-") && name.ends_with(".log") {
                (path, true, None)
            } else {
                return None;
            };
            let modified = fs::metadata(&log).ok()?.modified().ok()?;
            Some(LogEntry {
                path: log,
                name,
                monitor,
                started_at,
                modified,
            })
        })
        .collect();
    found.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.name.cmp(&a.name)));
    found
}

/// The log's text; for a very large log, only its end.
pub fn read_log(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut skipped = false;
    if len > MAX_SHOWN_BYTES {
        f.seek(SeekFrom::Start(len - MAX_SHOWN_BYTES))?;
        skipped = true;
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if skipped {
        // Start at a whole line.
        let cut = text.find('\n').map_or(0, |i| i + 1);
        text.replace_range(..cut, "… (start of the log not shown)\n");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_session_and_monitor_logs_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("monitor-20261101-100000.log");
        fs::write(&old, "monitoring started\n").unwrap();
        // Ensure a later modification time on coarse filesystems.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let dir = session::create_session_dir(root.path()).unwrap();
        let mut meta = SessionMeta::new(Vec::new());
        meta.started_at = "2026-11-11T14:03:22.517+01:00".into();
        meta.save(&dir).unwrap();
        session::SessionLog::open(&dir).line("recording started");
        // Not logs: other files, folders without a log.
        fs::write(root.path().join("notes.txt"), "x").unwrap();
        fs::create_dir(root.path().join("empty")).unwrap();

        let logs = find_logs(root.path());
        assert_eq!(logs.len(), 2, "{logs:?}");
        assert!(!logs[0].monitor);
        assert_eq!(logs[0].path, dir.join(session::LOG_FILE));
        assert_eq!(
            logs[0].started_at.as_deref(),
            Some("2026-11-11T14:03:22.517+01:00")
        );
        assert!(logs[1].monitor);
        assert_eq!(logs[1].path, old);
        assert!(
            read_log(&logs[0].path)
                .unwrap()
                .contains("recording started")
        );
        assert!(find_logs(&root.path().join("missing")).is_empty());
    }

    #[test]
    fn large_log_shows_its_end_from_a_line_start() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("monitor-x.log");
        let line = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde\n";
        let mut text = line.repeat((MAX_SHOWN_BYTES / 64 + 100) as usize);
        text.push_str("last line\n");
        fs::write(&path, &text).unwrap();
        let shown = read_log(&path).unwrap();
        assert!(shown.starts_with("… (start of the log not shown)\n0123"));
        assert!(shown.ends_with("last line\n"));
        assert!(shown.len() as u64 <= MAX_SHOWN_BYTES + 64);
    }
}

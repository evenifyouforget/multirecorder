//! Disk space and memory figures for the status bar.

use std::path::Path;
use std::time::Duration;

use crate::export::ExportFormat;

/// Free space for the volume holding `dir`, which may not exist yet.
pub fn free_space(dir: &Path) -> Option<u64> {
    let existing = dir.ancestors().find(|p| p.exists())?;
    fs4::available_space(existing).ok()
}

/// Physical memory used by this process.
pub fn process_ram() -> Option<u64> {
    memory_stats::memory_stats().map(|m| m.physical_mem as u64)
}

/// Bytes per second a track needs on disk while recording: the raw capture
/// plus the export growing beside it (raw files are only deleted at the end).
/// FLAC is assumed to compress 24-bit audio to 70%, which is conservative for
/// real input.
pub fn track_disk_rate(sample_rate: u32, channels: u16, format: ExportFormat) -> f64 {
    let samples = f64::from(sample_rate) * f64::from(channels);
    let export = match format {
        ExportFormat::Wav => 4.0,
        ExportFormat::Flac => 3.0 * 0.7,
    };
    samples * (4.0 + export)
}

/// How long `free` bytes last at `rate` bytes per second.
pub fn time_left(free: u64, rate: f64) -> Option<Duration> {
    (rate > 0.0).then(|| Duration::from_secs_f64(free as f64 / rate))
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_duration(d: Duration) -> String {
    let mins = d.as_secs() / 60;
    match mins {
        0 => "under a minute".to_string(),
        1..=119 => format!("{mins} min"),
        _ if mins < 60 * 48 => format!("{:.1} h", d.as_secs_f64() / 3600.0),
        _ => format!("{} days", mins / (60 * 24)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_rate_counts_raw_and_export() {
        // 48 kHz stereo: raw f32 is 384 kB/s, WAV doubles it.
        assert_eq!(track_disk_rate(48_000, 2, ExportFormat::Wav), 768_000.0);
        let flac = track_disk_rate(48_000, 2, ExportFormat::Flac);
        assert!(flac > 384_000.0 && flac < 768_000.0);
    }

    #[test]
    fn time_left_and_formatting() {
        // 100 GB at 9 stereo tracks of WAV is about 4 hours.
        let rate = 9.0 * track_disk_rate(48_000, 2, ExportFormat::Wav);
        let left = time_left(100_000_000_000, rate).unwrap();
        assert_eq!(format_duration(left), "4.0 h");
        assert_eq!(time_left(1, 0.0), None);
        assert_eq!(format_duration(Duration::from_secs(30)), "under a minute");
        assert_eq!(format_duration(Duration::from_secs(90 * 60)), "90 min");
        assert_eq!(format_duration(Duration::from_secs(5 * 86_400)), "5 days");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(123_400_000_000), "123.4 GB");
    }

    #[test]
    fn free_space_of_folder_not_created_yet() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not/yet/created");
        assert!(free_space(&missing).is_some_and(|b| b > 0));
        assert!(process_ram().is_some_and(|b| b > 0));
    }
}

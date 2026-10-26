//! Converts raw captures into WAV or FLAC files.
//!
//! Encoders are incremental: they accept samples as they come and patch their
//! headers when finished, so memory use does not depend on recording length
//! and export can run alongside the recording ([`LiveExport`]).

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::error::Verify;
use flacenc::source::{Context as FlacContext, Fill, FrameBuf};
use serde::{Deserialize, Serialize};

use crate::session::{SessionLog, SessionMeta, SessionState, TrackMeta};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    /// 32-bit float WAV: bit-exact copy of the capture (RF64 above 4 GiB).
    Wav,
    /// 24-bit FLAC: lossless at 24-bit, roughly half the size.
    Flac,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Wav => "WAV (32-bit float)",
            Self::Flac => "FLAC (24-bit)",
        }
    }
}

const CHUNK_FRAMES: usize = 4096;

/// Output path of a track's export.
pub fn output_path(dir: &Path, track: &TrackMeta, format: ExportFormat) -> PathBuf {
    dir.join(&track.raw_file).with_extension(format.extension())
}

/// Exports every track of a session that still has a raw capture, in parallel.
///
/// Tracks already exported during recording (raw gone, output present) are
/// counted as done. Raw captures are deleted only after every track exported
/// successfully, so a failed export can simply be retried. `progress` receives
/// values in 0..=1.
pub fn export_session(
    dir: &Path,
    format: ExportFormat,
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<PathBuf>> {
    let mut meta = SessionMeta::load(dir)?;
    let log = SessionLog::open(dir);
    let pending: Vec<&TrackMeta> = meta
        .tracks
        .iter()
        .filter(|t| dir.join(&t.raw_file).exists())
        .collect();
    let total: u64 = pending
        .iter()
        .map(|t| fs::metadata(dir.join(&t.raw_file)).map_or(0, |m| m.len()))
        .sum::<u64>()
        .max(1);
    let done = AtomicU64::new(0);
    let next = AtomicU64::new(0);
    let errors = Mutex::new(Vec::new());
    let workers = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(pending.len())
        .max(1);

    thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                        let Some(track) = pending.get(i) else { break };
                        let raw = dir.join(&track.raw_file);
                        let out = output_path(dir, track, format);
                        let mut last = 0u64;
                        let mut report = |bytes: u64| {
                            done.fetch_add(bytes - last, Ordering::Relaxed);
                            last = bytes;
                        };
                        match export_track(&raw, &out, track, format, &mut report) {
                            Ok(()) => log.line(&format!("exported {}", out.display())),
                            Err(e) => {
                                log.line(&format!("export of {} failed: {e:#}", raw.display()));
                                errors
                                    .lock()
                                    .expect("errors lock")
                                    .push(format!("{}: {e:#}", track.device_name));
                            }
                        }
                    }
                })
            })
            .collect();
        while !handles.iter().all(|h| h.is_finished()) {
            progress(done.load(Ordering::Relaxed) as f32 / total as f32);
            thread::sleep(Duration::from_millis(50));
        }
    });

    let errors = errors.into_inner().expect("errors lock");
    if !errors.is_empty() {
        return Err(anyhow!(
            "export failed (raw captures kept): {}",
            errors.join("; ")
        ));
    }
    for track in &meta.tracks {
        let _ = fs::remove_file(dir.join(&track.raw_file));
    }
    meta.state = SessionState::Exported;
    meta.save(dir)?;
    progress(1.0);
    Ok(meta
        .tracks
        .iter()
        .map(|t| output_path(dir, t, format))
        .filter(|p| p.exists())
        .collect())
}

/// Exports a complete raw capture in one go.
fn export_track(
    raw: &Path,
    out: &Path,
    track: &TrackMeta,
    format: ExportFormat,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let file = File::open(raw).with_context(|| format!("opening {}", raw.display()))?;
    // A crash can leave a partial frame at the end; ignore it.
    let frames = file.metadata()?.len() / track.bytes_per_frame();
    let mut encoder = TrackEncoder::create(out, format, track.channels, track.sample_rate)?;
    let mut reader = BufReader::new(file);
    let mut chunk = Vec::new();
    let mut remaining = frames;
    let mut read = 0u64;
    while remaining > 0 {
        let n = remaining.min(CHUNK_FRAMES as u64);
        read_frames(&mut reader, n, track.channels, &mut chunk)?;
        encoder.write(&chunk)?;
        remaining -= n;
        read += n * track.bytes_per_frame();
        progress(read);
    }
    encoder.finish()?;
    Ok(())
}

/// Reads exactly `frames` frames of raw little-endian f32 into `out`.
fn read_frames(
    reader: &mut impl Read,
    frames: u64,
    channels: u16,
    out: &mut Vec<f32>,
) -> Result<()> {
    let mut bytes = vec![0u8; frames as usize * usize::from(channels) * 4];
    reader.read_exact(&mut bytes)?;
    out.clear();
    out.extend(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b)),
    );
    Ok(())
}

/// Writes one track to `<out>.partial` and renames it to `out` when finished,
/// so a half-written export never looks complete.
pub struct TrackEncoder {
    inner: Encoder,
    tmp: PathBuf,
    out: PathBuf,
}

enum Encoder {
    Wav(WavEncoder),
    Flac(Box<FlacEncoder>),
}

impl TrackEncoder {
    pub fn create(
        out: &Path,
        format: ExportFormat,
        channels: u16,
        sample_rate: u32,
    ) -> Result<Self> {
        let tmp = out.with_extension(format!("{}.partial", format.extension()));
        let file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let w = BufWriter::with_capacity(256 * 1024, file);
        let inner = match format {
            ExportFormat::Wav => Encoder::Wav(WavEncoder::new(w, channels, sample_rate)?),
            ExportFormat::Flac => {
                Encoder::Flac(Box::new(FlacEncoder::new(w, channels, sample_rate)?))
            }
        };
        Ok(Self {
            inner,
            tmp,
            out: out.to_path_buf(),
        })
    }

    /// Appends interleaved samples; must be whole frames.
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        match &mut self.inner {
            Encoder::Wav(e) => e.write(samples),
            Encoder::Flac(e) => e.write(samples),
        }
    }

    pub fn finish(self) -> Result<PathBuf> {
        let file = match self.inner {
            Encoder::Wav(e) => e.finish()?,
            Encoder::Flac(e) => e.finish()?,
        };
        file.sync_all()?;
        drop(file);
        fs::rename(&self.tmp, &self.out)
            .with_context(|| format!("renaming to {}", self.out.display()))?;
        Ok(self.out)
    }
}

/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
const IEEE_FLOAT_GUID: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];
/// RIFF + JUNK/ds64 + fmt + fact + data chunk headers.
const WAV_HEADER_LEN: u64 = 12 + 36 + 48 + 12 + 8;

/// Float WAV whose final size is unknown up front. A JUNK chunk reserves room
/// for a ds64 chunk, which replaces it if the file ends up above 4 GiB (RF64).
struct WavEncoder {
    w: BufWriter<File>,
    channels: u16,
    frames: u64,
    scratch: Vec<u8>,
}

impl WavEncoder {
    fn new(mut w: BufWriter<File>, channels: u16, sample_rate: u32) -> Result<Self> {
        let block_align = u32::from(channels) * 4;
        let channel_mask: u32 = match channels {
            1 => 0x4,
            2 => 0x3,
            _ => 0,
        };
        w.write_all(b"RIFF")?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(b"WAVE")?;
        w.write_all(b"JUNK")?;
        w.write_all(&28u32.to_le_bytes())?;
        w.write_all(&[0u8; 28])?;
        w.write_all(b"fmt ")?;
        w.write_all(&40u32.to_le_bytes())?;
        w.write_all(&0xFFFEu16.to_le_bytes())?; // WAVE_FORMAT_EXTENSIBLE
        w.write_all(&channels.to_le_bytes())?;
        w.write_all(&sample_rate.to_le_bytes())?;
        w.write_all(&(sample_rate * block_align).to_le_bytes())?;
        w.write_all(&(block_align as u16).to_le_bytes())?;
        w.write_all(&32u16.to_le_bytes())?;
        w.write_all(&22u16.to_le_bytes())?;
        w.write_all(&32u16.to_le_bytes())?;
        w.write_all(&channel_mask.to_le_bytes())?;
        w.write_all(&IEEE_FLOAT_GUID)?;
        w.write_all(b"fact")?;
        w.write_all(&4u32.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(b"data")?;
        w.write_all(&0u32.to_le_bytes())?;
        Ok(Self {
            w,
            channels,
            frames: 0,
            scratch: Vec::new(),
        })
    }

    fn write(&mut self, samples: &[f32]) -> Result<()> {
        self.scratch.clear();
        self.scratch
            .extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        self.w.write_all(&self.scratch)?;
        self.frames += (samples.len() / usize::from(self.channels)) as u64;
        Ok(())
    }

    fn finish(mut self) -> Result<File> {
        let data_len = self.frames * u64::from(self.channels) * 4;
        let riff_len = WAV_HEADER_LEN - 8 + data_len;
        let rf64 = riff_len > u64::from(u32::MAX);
        let clamp = |v: u64| if rf64 { u32::MAX } else { v as u32 };
        self.w.seek(SeekFrom::Start(0))?;
        self.w.write_all(if rf64 { b"RF64" } else { b"RIFF" })?;
        self.w.write_all(&clamp(riff_len).to_le_bytes())?;
        if rf64 {
            self.w.seek(SeekFrom::Start(12))?;
            self.w.write_all(b"ds64")?;
            self.w.write_all(&28u32.to_le_bytes())?;
            self.w.write_all(&riff_len.to_le_bytes())?;
            self.w.write_all(&data_len.to_le_bytes())?;
            self.w.write_all(&self.frames.to_le_bytes())?;
            self.w.write_all(&0u32.to_le_bytes())?;
        }
        self.w.seek(SeekFrom::Start(12 + 36 + 48 + 8))?;
        self.w
            .write_all(&(self.frames.min(u64::from(u32::MAX)) as u32).to_le_bytes())?;
        self.w.seek(SeekFrom::Start(WAV_HEADER_LEN - 4))?;
        self.w.write_all(&clamp(data_len).to_le_bytes())?;
        self.w.flush()?;
        self.w.into_inner().map_err(|e| e.into_error().into())
    }
}

const FLAC_BITS: usize = 24;
const FLAC_BLOCK: usize = CHUNK_FRAMES;

fn to_i24(s: f32) -> i32 {
    const MAX: f32 = ((1 << (FLAC_BITS - 1)) - 1) as f32;
    if s.is_nan() {
        0
    } else {
        (s.clamp(-1.0, 1.0) * MAX).round() as i32
    }
}

/// 24-bit FLAC in fixed-size blocks; STREAMINFO (length, MD5) is patched at the end.
struct FlacEncoder {
    w: BufWriter<File>,
    channels: usize,
    config: flacenc::error::Verified<flacenc::config::Encoder>,
    info: StreamInfo,
    header_len: usize,
    buf: (FrameBuf, FlacContext),
    /// Interleaved samples waiting for a full block.
    pending: Vec<i32>,
    frame_number: usize,
    frames: u64,
}

impl FlacEncoder {
    fn new(mut w: BufWriter<File>, channels: u16, sample_rate: u32) -> Result<Self> {
        let ch = usize::from(channels);
        anyhow::ensure!(
            (1..=8).contains(&ch),
            "FLAC supports 1 to 8 channels, device has {ch}"
        );
        let config = flacenc::config::Encoder::default()
            .into_verified()
            .map_err(|(_, e)| anyhow!("flac config: {e:?}"))?;
        let mut info = StreamInfo::new(sample_rate as usize, ch, FLAC_BITS)
            .map_err(|e| anyhow!("flac: {e:?}"))?;
        info.set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
            .map_err(|e| anyhow!("flac: {e:?}"))?;
        // Placeholder header; rewritten with final sizes and MD5 at the end.
        let header_len = header_bytes(&info)?.len();
        w.write_all(&vec![0u8; header_len])?;
        Ok(Self {
            w,
            channels: ch,
            config,
            info,
            header_len,
            buf: (
                FrameBuf::with_size(ch, FLAC_BLOCK).map_err(|e| anyhow!("flac: {e:?}"))?,
                FlacContext::new(FLAC_BITS, ch),
            ),
            pending: Vec::with_capacity(FLAC_BLOCK * ch),
            frame_number: 0,
            frames: 0,
        })
    }

    fn write(&mut self, samples: &[f32]) -> Result<()> {
        let block = FLAC_BLOCK * self.channels;
        for chunk in samples.chunks(block) {
            let room = block - self.pending.len();
            let (now, later) = chunk.split_at(room.min(chunk.len()));
            self.pending.extend(now.iter().map(|&s| to_i24(s)));
            if self.pending.len() == block {
                self.encode_pending()?;
            }
            self.pending.extend(later.iter().map(|&s| to_i24(s)));
        }
        Ok(())
    }

    fn encode_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.buf
            .fill_interleaved(&self.pending)
            .map_err(|e| anyhow!("flac: {e:?}"))?;
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.buf.0,
            self.frame_number,
            &self.info,
        )
        .map_err(|e| anyhow!("flac encode: {e:?}"))?;
        self.info.update_frame_info(&frame);
        self.w.write_all(&to_bytes(&frame)?)?;
        self.frames += (self.pending.len() / self.channels) as u64;
        self.frame_number += 1;
        self.pending.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<File> {
        self.encode_pending()?;
        self.info.set_md5_digest(&self.buf.1.md5_digest());
        self.info.set_total_samples(self.frames as usize);
        if self.frames == 0 {
            self.info
                .set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
                .map_err(|e| anyhow!("flac: {e:?}"))?;
        }
        let header = header_bytes(&self.info)?;
        anyhow::ensure!(header.len() == self.header_len, "flac header size changed");
        self.w.seek(SeekFrom::Start(0))?;
        self.w.write_all(&header)?;
        self.w.flush()?;
        self.w.into_inner().map_err(|e| e.into_error().into())
    }
}

/// `fLaC` marker plus the STREAMINFO metadata block.
fn header_bytes(info: &StreamInfo) -> Result<Vec<u8>> {
    to_bytes(&Stream::with_stream_info(info.clone()))
}

fn to_bytes(component: &impl BitRepr) -> Result<Vec<u8>> {
    let mut sink = ByteSink::with_capacity(component.count_bits());
    component
        .write(&mut sink)
        .map_err(|e| anyhow!("flac: {e:?}"))?;
    Ok(sink.into_inner())
}

/// Shared between a recording track and its [`LiveExport`].
#[derive(Default)]
pub struct LiveExportControl {
    /// Frames of the raw capture that are on disk and will never be truncated.
    pub ready_frames: AtomicU64,
    /// Final length of the raw capture, set when the track has stopped.
    pub final_frames: OnceLock<u64>,
    /// Give up (e.g. the track thread was abandoned); the raw capture is kept.
    pub cancel: AtomicBool,
    /// Frames encoded so far.
    pub done_frames: AtomicU64,
}

/// Exports a track while it is being recorded by following its raw capture.
///
/// Recording never depends on this: if it fails or falls behind, the raw file
/// is still complete and [`export_session`] picks it up afterwards.
pub struct LiveExport {
    pub control: Arc<LiveExportControl>,
    thread: JoinHandle<Result<PathBuf>>,
}

impl LiveExport {
    pub fn spawn(dir: &Path, track: &TrackMeta, format: ExportFormat) -> Result<Self> {
        let control = Arc::new(LiveExportControl::default());
        let raw = dir.join(&track.raw_file);
        let out = output_path(dir, track, format);
        let (track, ctl) = (track.clone(), control.clone());
        let thread = thread::Builder::new()
            .name(format!("export-{}", track.raw_file))
            .spawn(move || follow(&raw, &out, &track, format, &ctl))
            .context("spawning export thread")?;
        Ok(Self { control, thread })
    }

    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Waits for the export to finish. On success the raw capture is deleted.
    pub fn join(self) -> Result<PathBuf> {
        self.thread
            .join()
            .unwrap_or_else(|_| Err(anyhow!("export thread panicked")))
    }
}

fn follow(
    raw: &Path,
    out: &Path,
    track: &TrackMeta,
    format: ExportFormat,
    ctl: &LiveExportControl,
) -> Result<PathBuf> {
    let mut encoder = TrackEncoder::create(out, format, track.channels, track.sample_rate)?;
    let mut reader =
        BufReader::new(File::open(raw).with_context(|| format!("opening {}", raw.display()))?);
    let mut chunk = Vec::new();
    let mut done = 0u64;
    loop {
        if ctl.cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        let final_frames = ctl.final_frames.get().copied();
        let target = final_frames.unwrap_or_else(|| ctl.ready_frames.load(Ordering::Acquire));
        if done < target {
            let n = (target - done).min(CHUNK_FRAMES as u64);
            read_frames(&mut reader, n, track.channels, &mut chunk)?;
            encoder.write(&chunk)?;
            done += n;
            ctl.done_frames.store(done, Ordering::Relaxed);
        } else if final_frames.is_some() {
            let path = encoder.finish()?;
            fs::remove_file(raw).with_context(|| format!("removing {}", raw.display()))?;
            return Ok(path);
        } else {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionMeta, raw_file_name};

    fn sine(frames: usize, channels: u16, rate: u32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                (0..channels).map(move |c| {
                    0.5 * (2.0 * std::f32::consts::PI * 440.0 * (c + 1) as f32 * i as f32
                        / rate as f32)
                        .sin()
                })
            })
            .collect()
    }

    fn make_session(
        channels: u16,
        rate: u32,
        samples: &[f32],
    ) -> (tempfile::TempDir, PathBuf, TrackMeta) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("s");
        fs::create_dir(&dir).unwrap();
        let track = TrackMeta {
            raw_file: raw_file_name(0, "Test Mic"),
            device_name: "Test Mic".into(),
            device_id: "fake".into(),
            sample_rate: rate,
            channels,
            loopback: false,
        };
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        fs::write(dir.join(&track.raw_file), bytes).unwrap();
        let mut meta = SessionMeta::new(vec![track.clone()]);
        meta.state = SessionState::Stopped;
        meta.save(&dir).unwrap();
        (root, dir, track)
    }

    #[test]
    fn wav_header_switches_to_rf64_above_4_gib() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.wav");
        let mut enc =
            WavEncoder::new(BufWriter::new(File::create(&path).unwrap()), 2, 48_000).unwrap();
        enc.write(&[0.5, -0.5]).unwrap();
        // Pretend 3 hours of 8-channel-sized data arrived (header logic only).
        enc.frames = 600_000_000;
        enc.finish().unwrap();
        let h = fs::read(&path).unwrap();
        let u32_at = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_le_bytes(h[i..i + 8].try_into().unwrap());
        let data_len = 600_000_000u64 * 8;
        assert_eq!(&h[0..4], b"RF64");
        assert_eq!(u32_at(4), u32::MAX);
        assert_eq!(&h[12..16], b"ds64");
        assert_eq!(u64_at(20), WAV_HEADER_LEN - 8 + data_len);
        assert_eq!(u64_at(28), data_len);
        assert_eq!(u64_at(36), 600_000_000);
        assert_eq!(&h[108..112], b"data");
        assert_eq!(u32_at(112), u32::MAX);
    }

    #[test]
    fn small_wav_keeps_riff_with_junk_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.wav");
        let mut enc = TrackEncoder::create(&path, ExportFormat::Wav, 1, 48_000).unwrap();
        enc.write(&[0.25; 100]).unwrap();
        enc.finish().unwrap();
        let h = fs::read(&path).unwrap();
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[12..16], b"JUNK");
        assert_eq!(h.len() as u64, WAV_HEADER_LEN + 400);
        assert!(!dir.path().join("small.wav.partial").exists());
    }

    #[test]
    fn wav_export_is_bit_exact() {
        let samples = sine(10_000, 3, 44_100);
        let (_root, dir, track) = make_session(3, 44_100, &samples);
        let outs = export_session(&dir, ExportFormat::Wav, &mut |_| {}).unwrap();
        assert_eq!(outs.len(), 1);
        assert!(
            !dir.join(&track.raw_file).exists(),
            "raw removed after export"
        );
        assert_eq!(
            SessionMeta::load(&dir).unwrap().state,
            SessionState::Exported
        );

        let mut r = hound::WavReader::open(&outs[0]).unwrap();
        let spec = r.spec();
        assert_eq!(
            (spec.channels, spec.sample_rate, spec.bits_per_sample),
            (3, 44_100, 32)
        );
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);
        let got: Vec<f32> = r.samples::<f32>().map(Result::unwrap).collect();
        assert_eq!(got, samples);
    }

    #[test]
    fn flac_export_decodes_to_24_bit() {
        // Not a multiple of the block size, to exercise the short last frame.
        let samples = sine(10_123, 2, 48_000);
        let (_root, dir, _) = make_session(2, 48_000, &samples);
        let outs = export_session(&dir, ExportFormat::Flac, &mut |_| {}).unwrap();

        let mut r = claxon::FlacReader::open(&outs[0]).unwrap();
        let info = r.streaminfo();
        assert_eq!(
            (info.channels, info.sample_rate, info.bits_per_sample),
            (2, 48_000, 24)
        );
        assert_eq!(info.samples, Some(10_123));
        let got: Vec<i32> = r.samples().map(Result::unwrap).collect();
        let want: Vec<i32> = samples.iter().map(|&s| to_i24(s)).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn truncated_raw_file_still_exports() {
        // Simulates a crash mid-write: a dangling half frame at the end.
        let mut samples = sine(1000, 2, 48_000);
        samples.push(0.25);
        let (_root, dir, _) = make_session(2, 48_000, &samples);
        let outs = export_session(&dir, ExportFormat::Wav, &mut |_| {}).unwrap();
        let r = hound::WavReader::open(&outs[0]).unwrap();
        assert_eq!(r.duration(), 1000);
    }

    #[test]
    fn empty_track_exports() {
        let (_root, dir, _) = make_session(1, 48_000, &[]);
        let outs = export_session(&dir, ExportFormat::Flac, &mut |_| {}).unwrap();
        assert!(outs[0].exists());
    }
}

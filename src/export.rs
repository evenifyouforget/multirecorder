//! Converts raw captures into WAV or FLAC files.
//!
//! Both encoders stream in fixed-size chunks, so memory use does not depend
//! on recording length.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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

/// Exports every track of a session next to its raw capture.
///
/// Raw captures are deleted only after every track exported successfully, so a
/// failed export can simply be retried. `progress` receives values in 0..=1.
pub fn export_session(
    dir: &Path,
    format: ExportFormat,
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<PathBuf>> {
    let mut meta = SessionMeta::load(dir)?;
    let log = SessionLog::open(dir);
    let total: u64 = meta
        .tracks
        .iter()
        .map(|t| fs::metadata(dir.join(&t.raw_file)).map_or(0, |m| m.len()))
        .sum::<u64>()
        .max(1);
    let mut done = 0u64;
    let mut outputs = Vec::new();
    let mut errors = Vec::new();

    for track in &meta.tracks {
        let raw = dir.join(&track.raw_file);
        if !raw.exists() {
            continue;
        }
        let out = raw.with_extension(format.extension());
        let mut report = |bytes: u64| progress((done + bytes) as f32 / total as f32);
        match export_track(&raw, &out, track, format, &mut report) {
            Ok(()) => {
                log.line(&format!("exported {}", out.display()));
                outputs.push(out);
            }
            Err(e) => {
                log.line(&format!("export of {} failed: {e:#}", raw.display()));
                errors.push(format!("{}: {e:#}", track.device_name));
            }
        }
        done += fs::metadata(&raw).map_or(0, |m| m.len());
    }

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
    Ok(outputs)
}

/// Writes `out` via a temporary file so a half-written export never looks finished.
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
    let reader = BufReader::new(file);
    let tmp = out.with_extension(format!("{}.partial", format.extension()));
    let writer =
        BufWriter::new(File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?);
    match format {
        ExportFormat::Wav => write_wav(
            reader,
            writer,
            frames,
            track.channels,
            track.sample_rate,
            progress,
        )?,
        ExportFormat::Flac => write_flac(
            reader,
            writer,
            frames,
            track.channels,
            track.sample_rate,
            progress,
        )?,
    }
    fs::rename(&tmp, out).with_context(|| format!("renaming to {}", out.display()))?;
    Ok(())
}

/// Calls `f` with successive chunks of interleaved samples, `frames` frames in total.
fn for_each_chunk(
    mut reader: impl Read,
    frames: u64,
    channels: u16,
    progress: &mut dyn FnMut(u64),
    mut f: impl FnMut(&[f32]) -> Result<()>,
) -> Result<()> {
    let channels = usize::from(channels);
    let mut bytes = vec![0u8; CHUNK_FRAMES * channels * 4];
    let mut samples = vec![0f32; CHUNK_FRAMES * channels];
    let mut remaining = frames;
    let mut read_bytes = 0u64;
    while remaining > 0 {
        let n = remaining.min(CHUNK_FRAMES as u64) as usize;
        let buf = &mut bytes[..n * channels * 4];
        reader.read_exact(buf)?;
        for (s, b) in samples.iter_mut().zip(buf.chunks_exact(4)) {
            *s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        f(&samples[..n * channels])?;
        remaining -= n as u64;
        read_bytes += buf.len() as u64;
        progress(read_bytes);
    }
    Ok(())
}

/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
const IEEE_FLOAT_GUID: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

fn write_wav(
    reader: impl Read,
    mut w: impl Write,
    frames: u64,
    channels: u16,
    sample_rate: u32,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let block_align = u32::from(channels) * 4;
    let data_len = frames * u64::from(block_align);
    let fmt_len = 40u64;
    let fact_len = 4u64;
    let body = 4 + (8 + fmt_len) + (8 + fact_len) + (8 + data_len);
    let rf64 = body + 8 + 28 > u64::from(u32::MAX);
    let riff_len = if rf64 { body + 8 + 28 } else { body };
    let clamp = |v: u64| if rf64 { u32::MAX } else { v as u32 };

    w.write_all(if rf64 { b"RF64" } else { b"RIFF" })?;
    w.write_all(&clamp(riff_len).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    if rf64 {
        w.write_all(b"ds64")?;
        w.write_all(&28u32.to_le_bytes())?;
        w.write_all(&riff_len.to_le_bytes())?;
        w.write_all(&data_len.to_le_bytes())?;
        w.write_all(&frames.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?;
    }
    let channel_mask: u32 = match channels {
        1 => 0x4,
        2 => 0x3,
        _ => 0,
    };
    w.write_all(b"fmt ")?;
    w.write_all(&(fmt_len as u32).to_le_bytes())?;
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
    w.write_all(&(fact_len as u32).to_le_bytes())?;
    w.write_all(&(frames.min(u64::from(u32::MAX)) as u32).to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&clamp(data_len).to_le_bytes())?;

    let mut out = Vec::new();
    for_each_chunk(reader, frames, channels, progress, |samples| {
        out.clear();
        out.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        w.write_all(&out)?;
        Ok(())
    })?;
    w.flush()?;
    Ok(())
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

fn write_flac<W: Write + Seek>(
    reader: impl Read,
    mut w: W,
    frames: u64,
    channels: u16,
    sample_rate: u32,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let ch = usize::from(channels);
    anyhow::ensure!(
        (1..=8).contains(&ch),
        "FLAC supports 1 to 8 channels, device has {ch}"
    );
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| anyhow!("flac config: {e:?}"))?;
    let mut info =
        StreamInfo::new(sample_rate as usize, ch, FLAC_BITS).map_err(|e| anyhow!("flac: {e:?}"))?;
    info.set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
        .map_err(|e| anyhow!("flac: {e:?}"))?;

    // Placeholder header; rewritten with final sizes and MD5 at the end.
    let header_len = header_bytes(&info)?.len();
    w.write_all(&vec![0u8; header_len])?;

    let mut buf = (
        FrameBuf::with_size(ch, FLAC_BLOCK).map_err(|e| anyhow!("flac: {e:?}"))?,
        FlacContext::new(FLAC_BITS, ch),
    );
    let mut ints = Vec::with_capacity(FLAC_BLOCK * ch);
    let mut frame_number = 0usize;
    for_each_chunk(reader, frames, channels, progress, |samples| {
        ints.clear();
        ints.extend(samples.iter().map(|&s| to_i24(s)));
        buf.fill_interleaved(&ints)
            .map_err(|e| anyhow!("flac: {e:?}"))?;
        let frame = flacenc::encode_fixed_size_frame(&config, &buf.0, frame_number, &info)
            .map_err(|e| anyhow!("flac encode: {e:?}"))?;
        info.update_frame_info(&frame);
        w.write_all(&to_bytes(&frame)?)?;
        frame_number += 1;
        Ok(())
    })?;

    info.set_md5_digest(&buf.1.md5_digest());
    info.set_total_samples(frames as usize);
    if frames == 0 {
        info.set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
            .map_err(|e| anyhow!("flac: {e:?}"))?;
    }
    let header = header_bytes(&info)?;
    anyhow::ensure!(header.len() == header_len, "flac header size changed");
    w.seek(SeekFrom::Start(0))?;
    w.write_all(&header)?;
    w.flush()?;
    Ok(())
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
        };
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        fs::write(dir.join(&track.raw_file), bytes).unwrap();
        let mut meta = SessionMeta::new(vec![track.clone()]);
        meta.state = SessionState::Stopped;
        meta.save(&dir).unwrap();
        (root, dir, track)
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

//! Live analysis of captured audio: level meters and problem detection.
//!
//! Runs on each track's writer thread (never in the audio callback) and only
//! sees audio that actually came from the device, not inserted silence. Time is
//! measured in frames, so the analysis is deterministic and testable.

use std::collections::VecDeque;

/// Dropouts are runs of exact digital zero between pieces of live audio. Real
/// analog inputs essentially never produce exact zeros, so short zero runs are a
/// strong sign of a driver or buffer glitch ("choppy audio").
const DROPOUT_MIN_SECS: f64 = 0.002;
const DROPOUT_MAX_SECS: f64 = 0.25;
/// This many dropouts within the window raise the choppy-audio warning.
const CHOPPY_COUNT: usize = 3;
const CHOPPY_WINDOW_SECS: f64 = 3.0;
/// Warnings stay visible for a while after the last occurrence.
const HOLD_SECS: f64 = 5.0;
/// Consecutive full-scale samples on one channel that count as clipping.
const CLIP_RUN: u32 = 3;
const CLIP_LEVEL: f32 = 0.999;
/// Below about -80 dBFS for this long counts as "no signal".
const SIGNAL_FLOOR: f32 = 1e-4;
const NO_SIGNAL_SECS: f64 = 5.0;

/// Snapshot of the problems currently detected on a track.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Warnings {
    /// Repeated short dropouts in the recent past.
    pub choppy: bool,
    /// Dropouts detected since recording started.
    pub dropouts: u64,
    /// Clipping in the recent past.
    pub clipping: bool,
    /// Clipping events since recording started.
    pub clip_events: u64,
    /// Input has been (near) silent for a while. Often expected, e.g. a muted mic.
    pub no_signal: bool,
}

impl Warnings {
    /// True for problems that likely damage the recording.
    pub fn any_problem(&self) -> bool {
        self.choppy || self.clipping
    }
}

pub struct Analyzer {
    channels: usize,
    rate: f64,
    /// Frames analyzed so far: the analyzer's clock.
    pos: u64,
    /// Samples of an incomplete frame carried over from the previous call.
    partial: Vec<f32>,
    peaks: Vec<f32>,
    zero_run: u64,
    /// Whether the current zero run was preceded by live audio.
    zero_run_after_signal: bool,
    recent_dropouts: VecDeque<u64>,
    dropouts: u64,
    choppy_until: u64,
    clip_runs: Vec<u32>,
    clip_events: u64,
    clipping_until: u64,
    last_signal: u64,
}

impl Analyzer {
    pub fn new(channels: u16, sample_rate: u32) -> Self {
        let channels = usize::from(channels.max(1));
        Self {
            channels,
            rate: f64::from(sample_rate),
            pos: 0,
            partial: Vec::with_capacity(channels),
            peaks: vec![0.0; channels],
            zero_run: 0,
            zero_run_after_signal: false,
            recent_dropouts: VecDeque::new(),
            dropouts: 0,
            choppy_until: 0,
            clip_runs: vec![0; channels],
            clip_events: 0,
            clipping_until: 0,
            last_signal: 0,
        }
    }

    fn frames(&self, secs: f64) -> u64 {
        (secs * self.rate) as u64
    }

    /// Call when the device stream is rebuilt, so a gap between two streams is
    /// not mistaken for a dropout inside one stream.
    pub fn stream_restarted(&mut self) {
        self.partial.clear();
        self.zero_run = 0;
        self.zero_run_after_signal = false;
        self.clip_runs.iter_mut().for_each(|r| *r = 0);
    }

    /// Analyzes interleaved samples. Frames may be split across calls.
    pub fn process(&mut self, mut samples: &[f32]) {
        if !self.partial.is_empty() {
            let need = self.channels - self.partial.len();
            let take = need.min(samples.len());
            self.partial.extend_from_slice(&samples[..take]);
            samples = &samples[take..];
            if self.partial.len() < self.channels {
                return;
            }
            let frame = std::mem::take(&mut self.partial);
            self.frame(&frame);
            self.partial = frame;
            self.partial.clear();
        }
        let mut frames = samples.chunks_exact(self.channels);
        for frame in &mut frames {
            self.frame(frame);
        }
        self.partial.extend_from_slice(frames.remainder());
    }

    fn frame(&mut self, frame: &[f32]) {
        let mut zero = true;
        let mut signal = false;
        for (c, &s) in frame.iter().enumerate() {
            let a = if s.is_finite() { s.abs() } else { 0.0 };
            zero &= s == 0.0;
            signal |= a > SIGNAL_FLOOR;
            self.peaks[c] = self.peaks[c].max(a);
            if a >= CLIP_LEVEL {
                self.clip_runs[c] += 1;
                if self.clip_runs[c] == CLIP_RUN {
                    self.clip_events += 1;
                    self.clipping_until = self.pos + self.frames(HOLD_SECS);
                }
            } else {
                self.clip_runs[c] = 0;
            }
        }

        if zero {
            self.zero_run += 1;
        } else {
            let len = self.zero_run;
            if self.zero_run_after_signal
                && len >= self.frames(DROPOUT_MIN_SECS).max(1)
                && len <= self.frames(DROPOUT_MAX_SECS)
            {
                self.dropout();
            }
            self.zero_run = 0;
            self.zero_run_after_signal = true;
        }
        if signal {
            self.last_signal = self.pos;
        }
        self.pos += 1;
    }

    fn dropout(&mut self) {
        self.dropouts += 1;
        self.recent_dropouts.push_back(self.pos);
        let window = self.frames(CHOPPY_WINDOW_SECS);
        while self
            .recent_dropouts
            .front()
            .is_some_and(|&p| p + window < self.pos)
        {
            self.recent_dropouts.pop_front();
        }
        if self.recent_dropouts.len() >= CHOPPY_COUNT {
            self.choppy_until = self.pos + self.frames(HOLD_SECS);
        }
    }

    /// Per-channel peak (linear, 0..=1+) since the previous call.
    pub fn take_peaks(&mut self) -> Vec<f32> {
        let out = self.peaks.clone();
        self.peaks.iter_mut().for_each(|p| *p = 0.0);
        out
    }

    pub fn warnings(&self) -> Warnings {
        Warnings {
            choppy: self.pos < self.choppy_until,
            dropouts: self.dropouts,
            clipping: self.pos < self.clipping_until,
            clip_events: self.clip_events,
            no_signal: self.pos - self.last_signal >= self.frames(NO_SIGNAL_SECS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn tone(frames: usize, channels: usize, amp: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                // Offset keeps the sine away from exact zero crossings.
                let s = amp * (0.3 + (i as f32 * 0.05).sin()) / 1.3;
                std::iter::repeat_n(s, channels)
            })
            .collect()
    }

    fn secs(s: f64) -> usize {
        (s * f64::from(RATE)) as usize
    }

    #[test]
    fn clean_audio_has_no_warnings() {
        let mut a = Analyzer::new(2, RATE);
        a.process(&tone(secs(6.0), 2, 0.5));
        assert_eq!(a.warnings(), Warnings::default());
        let peaks = a.take_peaks();
        assert!(peaks.iter().all(|&p| p > 0.4 && p <= 0.5), "{peaks:?}");
        assert_eq!(a.take_peaks(), vec![0.0, 0.0]);
    }

    #[test]
    fn periodic_dropouts_are_choppy() {
        // The reported failure: 5 short gaps per second.
        let mut a = Analyzer::new(2, RATE);
        for _ in 0..10 {
            a.process(&tone(secs(0.18), 2, 0.5));
            a.process(&vec![0.0; secs(0.02) * 2]);
        }
        a.process(&tone(secs(0.1), 2, 0.5));
        let w = a.warnings();
        assert!(w.choppy, "{w:?}");
        assert_eq!(w.dropouts, 10);
        // The warning holds for a while, then clears once audio is clean again.
        a.process(&tone(secs(2.0), 2, 0.5));
        assert!(a.warnings().choppy);
        a.process(&tone(secs(4.0), 2, 0.5));
        assert!(!a.warnings().choppy);
        assert_eq!(a.warnings().dropouts, 10);
    }

    #[test]
    fn isolated_gap_or_long_silence_is_not_choppy() {
        let mut a = Analyzer::new(1, RATE);
        a.process(&tone(secs(1.0), 1, 0.5));
        a.process(&vec![0.0; secs(0.05)]);
        a.process(&tone(secs(1.0), 1, 0.5));
        // A pause in playback (digital source) is long, not a dropout.
        a.process(&vec![0.0; secs(2.0)]);
        a.process(&tone(secs(1.0), 1, 0.5));
        let w = a.warnings();
        assert!(!w.choppy);
        assert_eq!(w.dropouts, 1);
    }

    #[test]
    fn leading_silence_is_not_a_dropout() {
        let mut a = Analyzer::new(1, RATE);
        a.process(&vec![0.0; secs(0.05)]);
        a.process(&tone(secs(0.5), 1, 0.5));
        assert_eq!(a.warnings().dropouts, 0);
    }

    #[test]
    fn gap_across_stream_restart_is_not_a_dropout() {
        let mut a = Analyzer::new(1, RATE);
        a.process(&tone(secs(0.5), 1, 0.5));
        a.process(&vec![0.0; secs(0.05)]);
        a.stream_restarted();
        a.process(&tone(secs(0.5), 1, 0.5));
        assert_eq!(a.warnings().dropouts, 0);
    }

    #[test]
    fn frames_split_across_calls() {
        let mut a = Analyzer::new(3, RATE);
        let data = tone(1000, 3, 0.5);
        for chunk in data.chunks(7) {
            a.process(chunk);
        }
        assert_eq!(a.pos, 1000);
        assert!(a.partial.is_empty());
    }

    #[test]
    fn clipping_detected_and_held() {
        let mut a = Analyzer::new(2, RATE);
        let mut data = tone(secs(0.5), 2, 0.5);
        // Right channel pinned at full scale for 5 frames.
        for f in 100..105 {
            data[f * 2 + 1] = 1.0;
        }
        a.process(&data);
        let w = a.warnings();
        assert!(w.clipping && w.any_problem());
        assert_eq!(w.clip_events, 1);
        a.process(&tone(secs(6.0), 2, 0.5));
        assert!(!a.warnings().clipping);
    }

    #[test]
    fn single_full_scale_sample_is_not_clipping() {
        let mut a = Analyzer::new(1, RATE);
        let mut data = tone(1000, 1, 0.5);
        data[500] = 1.0;
        a.process(&data);
        assert_eq!(a.warnings().clip_events, 0);
    }

    #[test]
    fn silence_reports_no_signal() {
        let mut a = Analyzer::new(1, RATE);
        a.process(&tone(secs(1.0), 1, 0.5));
        a.process(&vec![0.00001; secs(4.0)]);
        assert!(!a.warnings().no_signal);
        a.process(&vec![0.00001; secs(1.5)]);
        let w = a.warnings();
        assert!(w.no_signal && !w.any_problem());
        a.process(&tone(100, 1, 0.5));
        assert!(!a.warnings().no_signal);
    }
}

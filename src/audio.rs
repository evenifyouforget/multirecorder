//! Audio input abstraction plus its cpal implementation.
//!
//! The recorder only talks to [`InputSource`], which lets tests drive it with
//! fake devices that stall, disconnect and reconnect on demand.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{DeviceId, ErrorKind, FromSample, SampleFormat, SizedSample, StreamConfig};

/// Event reported by a running stream outside of the data path.
#[derive(Debug, Clone)]
pub struct StreamEvent {
    /// True if the stream is dead and has to be rebuilt.
    pub fatal: bool,
    pub message: String,
}

/// Everything the audio callback needs to hand samples to the recorder.
pub struct StreamSink {
    /// Interleaved f32 samples in the track's channel layout.
    pub producer: rtrb::Producer<f32>,
    /// Samples that did not fit into the ring buffer. The recorder replaces
    /// them with silence so the track stays aligned.
    pub dropped_samples: Arc<AtomicU64>,
    pub events: Sender<StreamEvent>,
    /// Time spent in `push`, i.e. our share of the real-time audio callback.
    pub busy_nanos: Arc<AtomicU64>,
}

impl StreamSink {
    /// Pushes one callback's worth of samples. Real-time safe: never blocks or allocates.
    pub fn push<T>(&mut self, data: &[T], channels: usize)
    where
        T: Copy,
        f32: FromSample<T>,
    {
        let started = Instant::now();
        let fit = data.len().min(self.producer.slots());
        let fit = fit - fit % channels.max(1);
        if fit > 0
            && let Ok(chunk) = self.producer.write_chunk_uninit(fit)
        {
            chunk.fill_from_iter(data[..fit].iter().map(|&s| f32::from_sample_(s)));
        }
        if fit < data.len() {
            self.dropped_samples
                .fetch_add((data.len() - fit) as u64, Ordering::Relaxed);
        }
        self.busy_nanos
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

/// A running stream; dropping it stops capture.
pub type ActiveStream = Box<dyn Any>;

/// A device the recorder can (re)open as often as needed.
pub trait InputSource: Send {
    /// Opens and starts the device with the track's sample rate and channel count.
    fn open(&mut self, sink: StreamSink) -> Result<ActiveStream, String>;
}

/// A recordable source as shown in the device list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDeviceInfo {
    /// Stable cpal device id (`host:id`), used to find the device again after a reconnect.
    pub id: String,
    pub name: String,
    pub host: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: String,
    /// An output device recorded via loopback (what is playing on it).
    pub loopback: bool,
}

impl InputDeviceInfo {
    /// Unique key for remembering the selection; an endpoint's input and its
    /// loopback are different sources.
    pub fn key(&self) -> String {
        if self.loopback {
            format!("loopback:{}", self.id)
        } else {
            self.id.clone()
        }
    }
}

/// Hosts whose output devices can be recorded by opening them as an input.
fn host_supports_loopback(host: cpal::HostId) -> bool {
    matches!(host.name(), "WASAPI" | "CoreAudio")
}

/// Lists input devices, then loopback-capable output devices, of every host.
pub fn list_devices() -> Vec<InputDeviceInfo> {
    let mut inputs = Vec::new();
    let mut loopbacks = Vec::new();
    for host_id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devices) = host.devices() else {
            continue;
        };
        for device in devices {
            let Ok(id) = device.id() else { continue };
            let info = |config: cpal::SupportedStreamConfig, loopback| InputDeviceInfo {
                id: id.to_string(),
                name: device.to_string(),
                host: host_id.name().to_string(),
                sample_rate: config.sample_rate(),
                channels: config.channels(),
                sample_format: config.sample_format().to_string(),
                loopback,
            };
            let input = device.default_input_config().ok();
            if let Some(config) = input {
                inputs.push(info(config, false));
            }
            // cpal only uses loopback for devices without inputs (macOS aggregates
            // can have both); WASAPI endpoints are one or the other anyway.
            if input.is_none()
                && host_supports_loopback(host_id)
                && let Ok(config) = device.default_output_config()
            {
                loopbacks.push(info(config, true));
            }
        }
    }
    inputs.extend(loopbacks);
    inputs
}

/// Opens a device by id through cpal, keeping the format chosen at session start.
pub struct CpalSource {
    pub device_id: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Record an output device through loopback.
    pub loopback: bool,
}

/// Preferred native sample formats, best first. Everything is converted to f32.
const FORMAT_PREFERENCE: [SampleFormat; 7] = [
    SampleFormat::F32,
    SampleFormat::I32,
    SampleFormat::I24,
    SampleFormat::I16,
    SampleFormat::F64,
    SampleFormat::U16,
    SampleFormat::U8,
];

impl InputSource for CpalSource {
    fn open(&mut self, sink: StreamSink) -> Result<ActiveStream, String> {
        let id: DeviceId = self
            .device_id
            .parse()
            .map_err(|e| format!("bad device id: {e:?}"))?;
        let host = cpal::host_from_id(id.host()).map_err(|e| e.to_string())?;
        let device = host.device_by_id(&id).ok_or("device not found")?;

        // Loopback streams use the output device's formats.
        let configs: Vec<_> = if self.loopback {
            device.supported_output_configs().map(Iterator::collect)
        } else {
            device.supported_input_configs().map(Iterator::collect)
        }
        .map_err(|e| describe_error(&e))?;
        let ranges: Vec<_> = configs
            .into_iter()
            .filter(|r| r.channels() == self.channels && r.contains_rate(self.sample_rate))
            .collect();
        let format = FORMAT_PREFERENCE
            .into_iter()
            .find(|f| ranges.iter().any(|r| r.sample_format() == *f))
            .ok_or_else(|| {
                format!(
                    "device no longer supports {} ch @ {} Hz",
                    self.channels, self.sample_rate
                )
            })?;

        let config = StreamConfig {
            channels: self.channels,
            sample_rate: self.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, sink),
            SampleFormat::I32 => build::<i32>(&device, config, sink),
            SampleFormat::I24 => build::<cpal::I24>(&device, config, sink),
            SampleFormat::I16 => build::<i16>(&device, config, sink),
            SampleFormat::F64 => build::<f64>(&device, config, sink),
            SampleFormat::U16 => build::<u16>(&device, config, sink),
            SampleFormat::U8 => build::<u8>(&device, config, sink),
            other => return Err(format!("unsupported sample format {other}")),
        }
        .map_err(|e| describe_error(&e))?;
        stream.play().map_err(|e| describe_error(&e))?;
        Ok(Box::new(stream))
    }
}

/// Turns cpal errors into something a user can act on; raw backend errors
/// are kept as detail for the log.
fn describe_error(err: &cpal::Error) -> String {
    match err.kind() {
        ErrorKind::DeviceBusy => format!(
            "device is in use by another application in exclusive mode; close that \
             application, or untick \"Allow applications to take exclusive control\" in the \
             device's Windows Sound properties ({err})"
        ),
        ErrorKind::PermissionDenied => {
            format!("access denied; check the system's microphone privacy settings ({err})")
        }
        ErrorKind::DeviceNotAvailable => "device not available (unplugged or disabled)".into(),
        _ => err.to_string(),
    }
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut sink: StreamSink,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let events = sink.events.clone();
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| sink.push(data, channels),
        move |err: cpal::Error| {
            let fatal = !matches!(
                err.kind(),
                ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
            );
            let _ = events.send(StreamEvent {
                fatal,
                message: err.to_string(),
            });
        },
        Some(std::time::Duration::from_secs(5)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_device_error_explains_exclusive_mode() {
        let err = cpal::Error::with_message(ErrorKind::DeviceBusy, "OS Error -2004287478");
        let msg = describe_error(&err);
        assert!(
            msg.contains("exclusive mode") && msg.contains("-2004287478"),
            "{msg}"
        );
    }

    #[test]
    fn push_converts_and_counts_overflow() {
        let (producer, mut consumer) = rtrb::RingBuffer::new(4);
        let (tx, _rx) = std::sync::mpsc::channel();
        let dropped = Arc::new(AtomicU64::new(0));
        let mut sink = StreamSink {
            producer,
            dropped_samples: dropped.clone(),
            events: tx,
            busy_nanos: Arc::default(),
        };

        sink.push(&[i16::MAX, 0, i16::MIN, 0, 1, 1], 2);
        // Only whole frames go in; the third frame overflows.
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
        let got: Vec<f32> = std::iter::from_fn(|| consumer.pop().ok()).collect();
        assert_eq!(got.len(), 4);
        assert!((got[0] - 1.0).abs() < 1e-3 && got[1] == 0.0 && got[2] == -1.0);
    }
}

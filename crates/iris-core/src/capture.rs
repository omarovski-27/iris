//! Microphone capture through WASAPI, via `cpal`.
//!
//! The audio callback is a real-time context: it runs on a thread WASAPI owns,
//! and anything slow in it shows up as a glitch. So the callback does exactly
//! three things — convert to `f32`, resample to 16 kHz mono, and hand the frame
//! to an unbounded channel — and never allocates a channel, locks a mutex, or
//! touches the network.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig};
use crossbeam_channel::{Receiver, Sender};

use crate::audio::{Resampler, FRAME_MS};
use crate::vlog;

/// A running capture stream. Dropping it stops the microphone.
pub struct Capture {
    _stream: cpal::Stream,
    pub device_name: String,
    pub input_rate: u32,
    pub input_channels: u16,
}

/// What a device looks like to `--list-devices`.
pub struct DeviceInfo {
    pub name: String,
    pub default: bool,
    pub rate: Option<u32>,
    pub channels: Option<u16>,
}

/// Enumerate input devices. Also the cheapest end-to-end check that the audio
/// stack is reachable at all.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .map(|d| device_name(&d))
        .unwrap_or_default();

    let mut out = Vec::new();
    for device in host.input_devices().context("enumerating input devices")? {
        let name = device_name(&device);
        let config = device.default_input_config().ok();
        out.push(DeviceInfo {
            default: name == default_name,
            name,
            rate: config.as_ref().map(|c| c.sample_rate()),
            channels: config.as_ref().map(|c| c.channels()),
        });
    }
    Ok(out)
}

/// cpal's `name()` is deprecated in favour of the richer `description()`; the
/// human-readable name is all we need from it.
fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unnamed>".into())
}

fn pick_device(wanted: Option<&str>) -> Result<Device> {
    let host = cpal::default_host();
    match wanted {
        None => host
            .default_input_device()
            .context("no default input device. Check Windows sound settings."),
        Some(want) => {
            let want_lower = want.to_ascii_lowercase();
            host.input_devices()
                .context("enumerating input devices")?
                .find(|d| device_name(d).to_ascii_lowercase().contains(&want_lower))
                .with_context(|| format!("no input device matching {want:?} (see --list-devices)"))
        }
    }
}

/// Open the microphone and start pushing 16 kHz mono frames to `sink`.
///
/// `device` is a case-insensitive substring of the device name; `None` uses the
/// Windows default.
pub fn start(device: Option<&str>, sink: Sender<Vec<i16>>) -> Result<Capture> {
    let device = pick_device(device)?;
    let device_name = device_name(&device);
    let supported = device
        .default_input_config()
        .context("querying the default input format")?;
    let sample_format = supported.sample_format();
    let input_rate = supported.sample_rate();
    let input_channels = supported.channels();

    let mut config: StreamConfig = supported.into();
    // Ask for a period close to our 20 ms frame. WASAPI shared mode runs on a
    // ~10 ms engine period and may ignore this, but where the request is
    // honoured it removes buffering we would otherwise pay for on every frame.
    config.buffer_size = requested_buffer_size(&device, input_rate);

    vlog!(
        "capture: {device_name} @ {input_rate} Hz x{input_channels} {sample_format:?}, buffer {:?}",
        config.buffer_size
    );

    let mut resampler = Resampler::new(input_rate, input_channels);
    // Reused across callbacks so the steady state does not allocate.
    let mut scratch: Vec<f32> = Vec::new();
    let mut frame: Vec<i16> = Vec::new();

    let on_error = |e| eprintln!("[iris] audio stream error: {e}");

    macro_rules! build {
        ($sample:ty) => {
            device.build_input_stream(
                &config,
                move |data: &[$sample], _: &cpal::InputCallbackInfo| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|s| to_f32(*s)));
                    frame.clear();
                    resampler.process(&scratch, &mut frame);
                    if !frame.is_empty() {
                        // Unbounded: send never blocks the audio thread. If the
                        // consumer is gone the send fails and we drop the frame.
                        let _ = sink.send(std::mem::take(&mut frame));
                    }
                },
                on_error,
                None,
            )
        };
    }

    let stream = match sample_format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::I16 => build!(i16),
        SampleFormat::U16 => build!(u16),
        other => anyhow::bail!("unsupported capture sample format {other:?}"),
    }
    .context("opening the input stream")?;

    stream.play().context("starting the input stream")?;

    Ok(Capture {
        _stream: stream,
        device_name,
        input_rate,
        input_channels,
    })
}

/// Prefer a buffer near [`FRAME_MS`], clamped to what the device supports.
fn requested_buffer_size(device: &Device, rate: u32) -> cpal::BufferSize {
    let target = (rate * FRAME_MS / 1000).max(1);
    let supported = device
        .supported_input_configs()
        .ok()
        .and_then(|mut c| c.next());
    match supported.map(|c| *c.buffer_size()) {
        Some(cpal::SupportedBufferSize::Range { min, max }) => {
            cpal::BufferSize::Fixed(target.clamp(min, max))
        }
        // `Unknown` means the backend will not honour a request anyway.
        _ => cpal::BufferSize::Default,
    }
}

/// Normalise cpal's sample types to `f32` in [-1, 1].
trait ToF32 {
    fn to_f32(self) -> f32;
}
impl ToF32 for f32 {
    fn to_f32(self) -> f32 {
        self
    }
}
impl ToF32 for i16 {
    fn to_f32(self) -> f32 {
        self as f32 / -(i16::MIN as f32)
    }
}
impl ToF32 for u16 {
    fn to_f32(self) -> f32 {
        (self as f32 - 32_768.0) / 32_768.0
    }
}

#[inline]
fn to_f32<S: ToF32>(s: S) -> f32 {
    s.to_f32()
}

/// Drain everything queued without blocking, concatenated into one frame.
///
/// The dictation loop calls this rather than taking frames one at a time so a
/// scheduling hiccup is caught up in a single `push` to the engine.
pub fn drain(rx: &Receiver<Vec<i16>>, out: &mut Vec<i16>) {
    out.clear();
    while let Ok(frame) = rx.try_recv() {
        out.extend_from_slice(&frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_conversion_covers_the_full_range() {
        assert_eq!(to_f32(0i16), 0.0);
        assert!((to_f32(i16::MAX) - 1.0).abs() < 1e-4);
        assert_eq!(to_f32(i16::MIN), -1.0);
        assert_eq!(to_f32(32_768u16), 0.0);
        assert_eq!(to_f32(0.5f32), 0.5);
    }

    #[test]
    fn drain_concatenates_queued_frames() {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(vec![1i16, 2]).unwrap();
        tx.send(vec![3i16]).unwrap();
        let mut out = vec![99];
        drain(&rx, &mut out);
        assert_eq!(out, vec![1, 2, 3]);
        drain(&rx, &mut out);
        assert!(out.is_empty());
    }
}

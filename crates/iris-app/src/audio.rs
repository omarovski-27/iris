//! Where 16 kHz mono frames come from.
//!
//! The dictation loop does not care whether frames come from a microphone or a
//! test, so it takes an [`AudioSource`]. That is what lets the whole state
//! machine run on Linux in CI: [`ChannelAudio`] is a plain channel a test
//! pushes into, and `MicAudio` — the only thing that opens a device — exists
//! on Windows only, exactly like `iris_core::capture`.
//!
//! # Warm vs cold
//!
//! Opening a WASAPI stream costs tens of milliseconds. Cold-opening it on the
//! hotkey press would put that cost on the one edge of this program where the
//! user can see latency, so `MicAudio` keeps the stream open by default and
//! discards frames captured while idle. `audio.warm = false` trades that
//! latency back for a microphone that is only live while dictating — the right
//! choice for anyone who wants the OS microphone indicator to mean something.

use crossbeam_channel::{Receiver, Sender};

/// A source of 16 kHz mono PCM frames.
///
/// # Contract
///
/// [`AudioSource::frames`] must return the *same* channel for the life of the
/// source, including across [`AudioSource::set_device`]. The dictation loop
/// clones the receiver once at startup and never asks again, so a source that
/// swapped channels on a device change would go silent instead of switching
/// microphones.
pub trait AudioSource: Send {
    /// The frame channel. Cloned by the loop, so this can be called once.
    fn frames(&self) -> &Receiver<Vec<i16>>;

    /// A dictation is starting. Opens the device if it is not already open.
    fn arm(&mut self) -> anyhow::Result<()>;

    /// The dictation is over. A warm source keeps the device open.
    fn disarm(&mut self);

    /// Switch input device, where that means anything. `None` is the system
    /// default.
    fn set_device(&mut self, device: Option<String>) -> anyhow::Result<()> {
        let _ = device;
        Ok(())
    }

    /// The microphone in use, named in plain language for the startup banner
    /// and the tray: a resolved device name, never internal stream state.
    fn describe(&self) -> String;
}

/// An audio source fed by whoever holds the [`Sender`].
///
/// Used by tests and by `--engine mock` runs on machines with no microphone.
#[derive(Debug)]
pub struct ChannelAudio {
    tx: Sender<Vec<i16>>,
    rx: Receiver<Vec<i16>>,
    armed_tx: Sender<()>,
    armed_rx: Receiver<()>,
}

impl Default for ChannelAudio {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelAudio {
    /// An empty source.
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (armed_tx, armed_rx) = crossbeam_channel::unbounded();
        Self {
            tx,
            rx,
            armed_tx,
            armed_rx,
        }
    }

    /// A handle for pushing frames in, as the audio callback would.
    pub fn sender(&self) -> Sender<Vec<i16>> {
        self.tx.clone()
    }

    /// Delivers one `()` per [`AudioSource::arm`] call.
    ///
    /// The dictation loop drains stale frames *before* arming, so a producer
    /// that waits on this channel before sending can never have real utterance
    /// audio discarded as stale — the synchronisation that makes `--speak-wav`
    /// and the loop tests deterministic rather than usually-right.
    pub fn armed(&self) -> Receiver<()> {
        self.armed_rx.clone()
    }
}

impl AudioSource for ChannelAudio {
    fn frames(&self) -> &Receiver<Vec<i16>> {
        &self.rx
    }

    fn arm(&mut self) -> anyhow::Result<()> {
        let _ = self.armed_tx.send(());
        Ok(())
    }

    fn disarm(&mut self) {}

    fn describe(&self) -> String {
        "none (frames supplied by the caller)".into()
    }
}

/// Below this RMS, in dBFS, a frame reads as the meter's floor (`0.0`) — room
/// tone or an open mic with nobody talking, not speech.
const SILENCE_FLOOR_DBFS: f64 = -50.0;

/// At or above this RMS, in dBFS, a frame reads as the meter's ceiling
/// (`1.0`) — loud but pre-clipping speech, close enough to full scale that
/// there is no more travel left to show.
const LOUD_CEILING_DBFS: f64 = -8.0;

/// Root-mean-square level of a frame, mapped to `0.0..=1.0` for a meter.
///
/// This used to be `sqrt(rms / i16::MAX)`: a single square root applied to
/// the *linear* RMS fraction. That is expansive (it lifts quiet values more
/// than loud ones), but not expansive enough — measured against synthetic
/// PCM at calibrated dBFS levels (see
/// `level_spans_most_of_its_range_across_realistic_speech_levels` below),
/// ordinary conversational speech (-23 dBFS RMS) landed at `0.27`; loud
/// speech (-13 dBFS RMS) only reached `0.46`. The whole dynamic range a
/// captain will ever produce sat inside a narrow band well short of `1.0`,
/// which then compounded with the overlay's own response curve
/// (`WAVE_RESPONSE_EXPONENT` in `iris-overlay`) crushing that narrow band
/// further toward its own floor — see
/// `crates/iris-overlay/docs/voice-level-evidence/README.md` for the visible
/// result, before and after this fix.
///
/// Human loudness perception — and every VU/level meter built on it — is
/// logarithmic, not a fixed power of the linear signal. Mapping dBFS linearly
/// between a calibrated floor and ceiling is the standard construction for
/// exactly this problem, and unlike a hand-picked exponent it is
/// parameterised by two numbers that describe real acoustic levels rather
/// than a curve shape tuned to whatever narrow band happened to be measured.
/// `SILENCE_FLOOR_DBFS` sits above typical unprocessed room tone so genuine
/// silence still reads as `0.0`; `LOUD_CEILING_DBFS` sits a little below full
/// scale so a mic being driven hard — but not clipping — still has room to
/// read as louder than ordinary conversation, without requiring the captain
/// to nearly saturate the input to ever see `1.0`.
pub fn level(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|s| (*s as f64).powi(2)).sum();
    let rms = (sum / pcm.len() as f64).sqrt() / f64::from(i16::MAX);
    if rms <= 0.0 {
        return 0.0;
    }
    let dbfs = 20.0 * rms.log10();
    let normalized = (dbfs - SILENCE_FLOOR_DBFS) / (LOUD_CEILING_DBFS - SILENCE_FLOOR_DBFS);
    normalized.clamp(0.0, 1.0) as f32
}

#[cfg(windows)]
pub use mic::MicAudio;

#[cfg(windows)]
mod mic {
    use anyhow::{Context, Result};
    use crossbeam_channel::{Receiver, Sender};
    use iris_core::capture::{self, Capture};

    use super::AudioSource;

    /// The microphone, via WASAPI.
    pub struct MicAudio {
        device: Option<String>,
        warm: bool,
        tx: Sender<Vec<i16>>,
        rx: Receiver<Vec<i16>>,
        stream: Option<Capture>,
    }

    impl MicAudio {
        /// Open a source for `device` (a case-insensitive substring of the
        /// device name, or `None` for the system default).
        ///
        /// A warm source opens the stream immediately, so the first dictation
        /// is as fast as the tenth.
        pub fn new(device: Option<String>, warm: bool) -> Result<Self> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let mut source = Self {
                device,
                warm,
                tx,
                rx,
                stream: None,
            };
            if warm {
                source.open()?;
            }
            Ok(source)
        }

        fn open(&mut self) -> Result<()> {
            if self.stream.is_some() {
                return Ok(());
            }
            let capture = capture::start(self.device.as_deref(), self.tx.clone())
                .context("opening the microphone")?;
            iris_core::vlog!(
                "microphone: {} @ {} Hz x{}",
                capture.device_name,
                capture.input_rate,
                capture.input_channels
            );
            self.stream = Some(capture);
            Ok(())
        }

        /// The device currently in use, if the stream is open.
        pub fn device_name(&self) -> Option<&str> {
            self.stream.as_ref().map(|c| c.device_name.as_str())
        }
    }

    impl AudioSource for MicAudio {
        fn frames(&self) -> &Receiver<Vec<i16>> {
            &self.rx
        }

        fn arm(&mut self) -> Result<()> {
            self.open()
        }

        fn disarm(&mut self) {
            if !self.warm {
                // Dropping the stream closes the device, which also turns off
                // the OS microphone indicator.
                self.stream = None;
            }
        }

        fn set_device(&mut self, device: Option<String>) -> Result<()> {
            let previous = self.device.take();
            self.device = device;
            // Close first: WASAPI is happy with two streams, but the frames
            // from the old device would interleave with the new one's.
            self.stream = None;
            if self.warm {
                if let Err(e) = self.open() {
                    // Fall back to what was working; a mistyped device name in
                    // the tray must not leave Iris with no microphone at all.
                    self.device = previous;
                    let _ = self.open();
                    return Err(e);
                }
            }
            Ok(())
        }

        fn describe(&self) -> String {
            match (self.device_name(), &self.device) {
                (Some(name), _) => name.to_string(),
                (None, Some(want)) => format!("the microphone matching \"{want}\""),
                (None, None) => "the default microphone".into(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_audio_delivers_what_is_pushed() {
        let mut source = ChannelAudio::new();
        let tx = source.sender();
        source.arm().unwrap();
        tx.send(vec![1, 2, 3]).unwrap();
        assert_eq!(source.frames().recv().unwrap(), vec![1, 2, 3]);
        source.disarm();
    }

    #[test]
    fn level_is_zero_for_silence_and_one_for_full_scale() {
        assert_eq!(level(&[]), 0.0);
        assert_eq!(level(&[0; 320]), 0.0);
        assert!((level(&[i16::MAX; 320]) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn level_rises_with_amplitude() {
        let quiet = level(&[500; 320]);
        let loud = level(&[8_000; 320]);
        assert!(quiet > 0.0);
        assert!(loud > quiet, "quiet {quiet} loud {loud}");
        assert!(loud <= 1.0);
    }

    /// A full-scale sine at the given peak amplitude fraction (`0.0..=1.0`),
    /// long enough (10 ms at 16 kHz) that the RMS calculation is not
    /// dominated by edge effects. RMS-based level detection only cares about
    /// a frame's energy, not its exact waveform, so a sine tone calibrated to
    /// a target dBFS is as representative as any other shape for exercising
    /// [`level`] — what matters is the RMS it produces, and a sine's RMS
    /// (`peak / sqrt(2)`) is exactly known, which lets each test case state
    /// the dBFS level it means to test rather than an opaque sample value.
    fn sine_at_peak_fraction(peak_fraction: f32) -> Vec<i16> {
        let peak = (f32::from(i16::MAX) * peak_fraction) as f64;
        (0..160)
            .map(|n| {
                let t = n as f64 / 16_000.0;
                (peak * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect()
    }

    /// Measured dBFS RMS for each case, computed as `peak_fraction / sqrt(2)`
    /// then to dB: quiet room ≈ -55 dBFS (below the floor, reads as silence),
    /// quiet speech ≈ -37 dBFS, normal conversational speech ≈ -23 dBFS, loud
    /// speech ≈ -13 dBFS, near-clipping ≈ -5 dBFS (above the ceiling, reads
    /// as full scale). These are the numbers the module doc on [`level`]
    /// cites as motivation for the dBFS mapping over the old double-`sqrt`.
    #[test]
    fn level_spans_most_of_its_range_across_realistic_speech_levels() {
        let room_noise = level(&sine_at_peak_fraction(0.002)); // ~-55 dBFS
        let quiet_speech = level(&sine_at_peak_fraction(0.02)); // ~-37 dBFS
        let normal_speech = level(&sine_at_peak_fraction(0.1)); // ~-23 dBFS
        let loud_speech = level(&sine_at_peak_fraction(0.3)); // ~-13 dBFS
        let near_clipping = level(&sine_at_peak_fraction(0.8)); // ~-5 dBFS

        assert_eq!(room_noise, 0.0, "room tone below the floor must read flat");
        assert!(
            (0.25..0.40).contains(&quiet_speech),
            "quiet speech {quiet_speech} should sit in the lower-middle of the range"
        );
        assert!(
            (0.55..0.72).contains(&normal_speech),
            "normal conversational speech {normal_speech} should clearly outrun quiet speech \
             and reach well past the middle of the range"
        );
        assert!(
            (0.80..0.95).contains(&loud_speech),
            "loud speech {loud_speech} should be near the top of the range"
        );
        assert_eq!(
            near_clipping, 1.0,
            "near-clipping audio should hit the meter's ceiling"
        );

        assert!(quiet_speech < normal_speech, "monotonic quiet < normal");
        assert!(normal_speech < loud_speech, "monotonic normal < loud");
        assert!(
            loud_speech < near_clipping,
            "monotonic loud < near-clipping"
        );
    }
}

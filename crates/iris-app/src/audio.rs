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

    /// What to show in the startup banner.
    fn describe(&self) -> String;
}

/// An audio source fed by whoever holds the [`Sender`].
///
/// Used by tests and by `--engine mock` runs on machines with no microphone.
#[derive(Debug)]
pub struct ChannelAudio {
    tx: Sender<Vec<i16>>,
    rx: Receiver<Vec<i16>>,
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
        Self { tx, rx }
    }

    /// A handle for pushing frames in, as the audio callback would.
    pub fn sender(&self) -> Sender<Vec<i16>> {
        self.tx.clone()
    }
}

impl AudioSource for ChannelAudio {
    fn frames(&self) -> &Receiver<Vec<i16>> {
        &self.rx
    }

    fn arm(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn disarm(&mut self) {}

    fn describe(&self) -> String {
        "none (frames supplied by the caller)".into()
    }
}

/// Root-mean-square level of a frame, mapped to `0.0..=1.0` for a meter.
///
/// Speech sits far below full scale, so a linear RMS reading would leave the
/// meter permanently near zero. The square root expands the quiet end, which is
/// where all the interesting variation in a voice actually is.
pub fn level(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|s| (*s as f64).powi(2)).sum();
    let rms = (sum / pcm.len() as f64).sqrt() / f64::from(i16::MAX);
    (rms.sqrt() as f32).clamp(0.0, 1.0)
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
            match (&self.stream, &self.device) {
                (Some(c), _) => format!("{} (kept open)", c.device_name),
                (None, Some(want)) => format!("{want:?}, opened on key-press"),
                (None, None) => "system default, opened on key-press".into(),
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
}

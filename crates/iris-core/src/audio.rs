//! Audio normalisation: whatever the capture device hands us becomes 16 kHz
//! mono `i16` PCM, which is what every engine in [`crate::engine`] consumes.
//!
//! 16 kHz is the sweet spot for speech recognition: it is what Deepgram's and
//! Whisper's acoustic front-ends expect, and it is 3× less audio to push over
//! the wire than the 48 kHz a typical WASAPI device runs at.

use std::path::Path;

use anyhow::{Context, Result};

/// The sample rate every [`crate::engine::Session`] is fed at.
pub const SAMPLE_RATE: u32 = 16_000;

/// Frame size in milliseconds. 20 ms is the standard streaming-ASR packet: big
/// enough that per-packet overhead is irrelevant, small enough that it adds no
/// perceptible latency.
pub const FRAME_MS: u32 = 20;

/// Samples per frame at [`SAMPLE_RATE`] (320).
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * FRAME_MS as usize) / 1000;

/// Duration in seconds of `n` samples at [`SAMPLE_RATE`].
pub fn secs(n_samples: usize) -> f64 {
    n_samples as f64 / SAMPLE_RATE as f64
}

/// Downmixes to mono and resamples to [`SAMPLE_RATE`].
///
/// Downsampling is done properly: a windowed-sinc low-pass runs first so that
/// energy above 8 kHz cannot alias down into the speech band, then linear
/// interpolation reads the filtered stream at the output rate. At 48 kHz the
/// filter costs ~3 M multiply-accumulates per second per stream, which is
/// nothing next to the audio callback's own overhead — but skipping it folds
/// sibilance and any switching noise straight onto the vowels, which measurably
/// hurts recognition.
///
/// When the input is already 16 kHz mono the whole thing degenerates to a copy.
pub struct Resampler {
    channels: usize,
    /// Ratio of input samples to output samples (`in_rate / 16000`).
    step: f64,
    /// Distance, in input samples, from the previous input sample to the next
    /// output sample. Always `> 0` between calls.
    acc: f64,
    taps: Vec<f32>,
    /// FIR delay line, used as a ring buffer.
    hist: Vec<f32>,
    hist_pos: usize,
    prev: f32,
    primed: bool,
}

impl Resampler {
    /// Number of FIR taps. 63 gives ~60 dB of stopband rejection with a
    /// Hamming window, and a fixed 31-sample (0.65 ms at 48 kHz) group delay.
    const TAPS: usize = 63;

    pub fn new(in_rate: u32, channels: u16) -> Self {
        let channels = channels.max(1) as usize;
        let step = in_rate as f64 / SAMPLE_RATE as f64;
        // Only downsampling can alias. Cut just below the 8 kHz output Nyquist,
        // leaving a little transition room.
        let taps = if in_rate > SAMPLE_RATE {
            let cutoff = 7_200.0 / in_rate as f64;
            lowpass_taps(Self::TAPS, cutoff)
        } else {
            Vec::new()
        };
        let hist_len = taps.len().max(1);
        Self {
            channels,
            step,
            acc: 1.0,
            taps,
            hist: vec![0.0; hist_len],
            hist_pos: 0,
            prev: 0.0,
            primed: false,
        }
    }

    /// True when input and output formats already match, so `process` is a
    /// straight conversion with no filtering.
    pub fn is_passthrough(&self) -> bool {
        self.channels == 1 && (self.step - 1.0).abs() < f64::EPSILON
    }

    /// Convert one capture-callback buffer of interleaved samples.
    ///
    /// Output is appended to `out`; the resampler keeps the filter state so
    /// consecutive buffers stitch together without clicks.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<i16>) {
        if self.is_passthrough() {
            out.extend(input.iter().map(|&s| to_i16(s)));
            return;
        }

        for chunk in input.chunks_exact(self.channels) {
            // Downmix by averaging. Summing would clip on correlated channels
            // and a content-dependent gain would be worse still; recognisers
            // normalise level internally, so predictability wins.
            let mono = chunk.iter().sum::<f32>() / self.channels as f32;
            self.push_sample(mono, out);
        }
    }

    fn push_sample(&mut self, x: f32, out: &mut Vec<i16>) {
        let y = if self.taps.is_empty() {
            x
        } else {
            self.hist[self.hist_pos] = x;
            self.hist_pos = (self.hist_pos + 1) % self.hist.len();
            let n = self.hist.len();
            let mut acc = 0.0f32;
            // `hist_pos` now points at the oldest sample, which pairs with
            // taps[0].
            for (i, tap) in self.taps.iter().enumerate() {
                acc += self.hist[(self.hist_pos + i) % n] * tap;
            }
            acc
        };

        if self.primed {
            while self.acc <= 1.0 {
                let s = self.prev + (y - self.prev) * self.acc as f32;
                out.push(to_i16(s));
                self.acc += self.step;
            }
            self.acc -= 1.0;
        } else {
            self.primed = true;
        }
        self.prev = y;
    }
}

/// Hamming-windowed sinc low-pass, normalised to unity DC gain.
///
/// `cutoff` is normalised to the input sample rate (i.e. `fc / fs`).
fn lowpass_taps(n: usize, cutoff: f64) -> Vec<f32> {
    let n = if n % 2 == 0 { n + 1 } else { n };
    let mid = (n / 2) as f64;
    let mut taps = Vec::with_capacity(n);
    let mut sum = 0.0f64;
    for i in 0..n {
        let t = i as f64 - mid;
        let sinc = if t.abs() < 1e-9 {
            2.0 * cutoff
        } else {
            (2.0 * std::f64::consts::PI * cutoff * t).sin() / (std::f64::consts::PI * t)
        };
        let window = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (n as f64 - 1.0)).cos();
        let v = sinc * window;
        sum += v;
        taps.push(v);
    }
    taps.into_iter().map(|v| (v / sum) as f32).collect()
}

#[inline]
fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// Read any WAV file and normalise it to 16 kHz mono `i16`.
pub fn read_wav(path: impl AsRef<Path>) -> Result<Vec<i16>> {
    let path = path.as_ref();
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening WAV {}", path.display()))?;
    let spec = reader.spec();

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()?
        }
    };

    let mut resampler = Resampler::new(spec.sample_rate, spec.channels);
    let mut out = Vec::with_capacity(samples.len());
    resampler.process(&samples, &mut out);
    Ok(out)
}

/// Encode 16 kHz mono PCM as a WAV file in memory.
///
/// Used by the Groq engine, whose HTTP API takes a container rather than a raw
/// PCM stream.
pub fn encode_wav(pcm: &[i16]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = std::io::Cursor::new(Vec::with_capacity(44 + pcm.len() * 2));
    {
        let mut writer = hound::WavWriter::new(&mut buf, spec)?;
        for &s in pcm {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }
    Ok(buf.into_inner())
}

/// Write 16 kHz mono PCM to a WAV file (used by `--save-wav`).
pub fn write_wav(path: impl AsRef<Path>, pcm: &[i16]) -> Result<()> {
    std::fs::write(path, encode_wav(pcm)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, secs: f64) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .map(|i| {
                (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32 * 0.5
            })
            .collect()
    }

    fn rms(pcm: &[i16]) -> f64 {
        if pcm.is_empty() {
            return 0.0;
        }
        let sum: f64 = pcm.iter().map(|&s| (s as f64).powi(2)).sum();
        (sum / pcm.len() as f64).sqrt()
    }

    #[test]
    fn passthrough_is_detected_for_16k_mono() {
        assert!(Resampler::new(16_000, 1).is_passthrough());
        assert!(!Resampler::new(48_000, 1).is_passthrough());
        assert!(!Resampler::new(16_000, 2).is_passthrough());
    }

    #[test]
    fn resamples_48k_stereo_to_the_expected_length() {
        let mono = sine(440.0, 48_000, 1.0);
        let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        let mut r = Resampler::new(48_000, 2);
        let mut out = Vec::new();
        r.process(&stereo, &mut out);
        // 1 s in, 1 s out, within a couple of samples of filter priming.
        assert!(
            (out.len() as i64 - 16_000).abs() <= 2,
            "expected ~16000 samples, got {}",
            out.len()
        );
    }

    #[test]
    fn streaming_in_small_buffers_matches_one_big_buffer() {
        let input = sine(440.0, 48_000, 0.5);

        let mut whole = Vec::new();
        Resampler::new(48_000, 1).process(&input, &mut whole);

        let mut streamed = Vec::new();
        let mut r = Resampler::new(48_000, 1);
        for chunk in input.chunks(480) {
            r.process(chunk, &mut streamed);
        }

        assert_eq!(whole, streamed, "buffer boundaries must not change output");
    }

    #[test]
    fn speech_band_survives_and_out_of_band_is_rejected() {
        // 1 kHz sits in the middle of the speech band and must come through.
        let mut in_band = Vec::new();
        Resampler::new(48_000, 1).process(&sine(1_000.0, 48_000, 0.5), &mut in_band);

        // 12 kHz is above the 8 kHz output Nyquist. Without the anti-alias
        // filter it would fold down to 4 kHz — right on top of speech.
        let mut out_of_band = Vec::new();
        Resampler::new(48_000, 1).process(&sine(12_000.0, 48_000, 0.5), &mut out_of_band);

        let passed = rms(&in_band);
        let rejected = rms(&out_of_band);
        assert!(passed > 8_000.0, "1 kHz tone was attenuated: rms {passed}");
        assert!(
            rejected < passed / 100.0,
            "12 kHz aliased through: rms {rejected} vs {passed}"
        );
    }

    #[test]
    fn upsampling_from_8k_produces_twice_as_many_samples() {
        let mut out = Vec::new();
        Resampler::new(8_000, 1).process(&sine(300.0, 8_000, 1.0), &mut out);
        assert!(
            (out.len() as i64 - 16_000).abs() <= 4,
            "expected ~16000 samples, got {}",
            out.len()
        );
    }

    #[test]
    fn wav_roundtrips_through_encode_and_read() {
        let pcm: Vec<i16> = (0..16_000)
            .map(|i| ((i % 200) as i16 - 100) * 100)
            .collect();
        let dir = std::env::temp_dir().join("iris-wav-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        write_wav(&path, &pcm).unwrap();
        let back = read_wav(&path).unwrap();
        assert_eq!(pcm.len(), back.len());
        // i16 -> f32 -> i16 costs at most one LSB of rounding.
        for (a, b) in pcm.iter().zip(&back) {
            assert!((a - b).abs() <= 1, "{a} != {b}");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fixture_is_readable_and_about_five_seconds() {
        let pcm = read_wav(crate::sample_wav_path()).expect("speech fixture");
        let d = secs(pcm.len());
        assert!((4.0..7.0).contains(&d), "fixture is {d:.2}s, expected ~5s");
    }
}

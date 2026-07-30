//! 16 kHz mono PCM helpers and WAV I/O.

use std::path::Path;

use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};

/// Iris local engines expect this sample rate.
pub const SAMPLE_RATE: u32 = 16_000;

/// Convenience for callers that need the constant as an `i32` (sherpa-onnx).
pub fn sample_rate() -> i32 {
    SAMPLE_RATE as i32
}

/// Convert signed 16-bit PCM samples to the f32 range `[-1, 1)` used by most
/// native ASR backends.
pub fn pcm16_to_f32(pcm: &[i16]) -> Vec<f32> {
    pcm.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// Read a WAV file and return 16 kHz mono PCM16 samples.
///
/// Accepts mono or stereo i16/f32 WAVs at any rate; stereo is averaged to mono
/// and non-16 kHz audio is rejected (callers should resample upstream — Iris
/// already normalises capture to 16 kHz).
pub fn read_wav_pcm16(path: &Path) -> Result<Vec<i16>> {
    let reader = WavReader::open(path)
        .with_context(|| format!("opening WAV {}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE {
        bail!(
            "WAV {} is {} Hz; local engines require {} Hz mono (resample upstream)",
            path.display(),
            spec.sample_rate,
            SAMPLE_RATE
        );
    }
    if spec.channels == 0 || spec.channels > 2 {
        bail!(
            "WAV {} has {} channels; expected mono or stereo",
            path.display(),
            spec.channels
        );
    }

    let channels = spec.channels as usize;
    let mono: Vec<i16> = match spec.sample_format {
        SampleFormat::Int => {
            let samples: Result<Vec<i16>, _> = reader.into_samples::<i16>().collect();
            let samples = samples.context("reading i16 samples")?;
            downmix_i16(&samples, channels)
        }
        SampleFormat::Float => {
            let samples: Result<Vec<f32>, _> = reader.into_samples::<f32>().collect();
            let samples = samples.context("reading f32 samples")?;
            let i16s: Vec<i16> = samples
                .into_iter()
                .map(|f| (f.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            downmix_i16(&i16s, channels)
        }
    };
    Ok(mono)
}

fn downmix_i16(samples: &[i16], channels: usize) -> Vec<i16> {
    if channels == 1 {
        return samples.to_vec();
    }
    samples
        .chunks_exact(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&s| s as i32).sum();
            (sum / channels as i32) as i16
        })
        .collect()
}

/// Write mono 16 kHz PCM16 samples to a WAV file (used by tests / harness).
pub fn write_wav_pcm16(path: &Path, pcm: &[i16]) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(path, spec)
        .with_context(|| format!("creating WAV {}", path.display()))?;
    for &s in pcm {
        writer.write_sample(s)?;
    }
    writer.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{silence_fixture, speech_fixture};

    #[test]
    fn pcm16_to_f32_scales() {
        assert!((pcm16_to_f32(&[0])[0] - 0.0).abs() < 1e-6);
        assert!((pcm16_to_f32(&[32767])[0] - 32767.0 / 32768.0).abs() < 1e-6);
        assert!((pcm16_to_f32(&[-32768])[0] - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn silence_fixture_is_16k_mono_zeros() {
        let pcm = read_wav_pcm16(&silence_fixture()).unwrap();
        assert_eq!(pcm.len(), SAMPLE_RATE as usize / 2); // 0.5 s
        assert!(pcm.iter().all(|&s| s == 0));
    }

    #[test]
    fn speech_fixture_loads() {
        let pcm = read_wav_pcm16(&speech_fixture()).unwrap();
        assert!(pcm.len() > SAMPLE_RATE as usize); // > 1 s
        assert!(pcm.iter().any(|&s| s != 0));
    }
}

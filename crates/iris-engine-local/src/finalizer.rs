//! Batch finalizer — transcript of record.
//!
//! **v1 choice:** whisper.cpp `base.en` q5_1 via `whisper-rs` 0.16, with
//! **mandatory Silero VAD gating**. Whisper hallucinates a word on pure silence
//! 100% of the time when ungated (see the local-ASR evaluation report).
//!
//! **Deferred:** Parakeet-TDT 0.6B q8_0 via whisper.cpp's new GGML backend —
//! preferred accuracy/hygiene, but no Rust binding exists yet (feature landed
//! the same day as the evaluation). Options for a fast-follow: thin FFI against
//! `parakeet.h`, or wait for `whisper-rs` to expose it. Documented in README.

use anyhow::Result;

#[cfg(feature = "whisper")]
use crate::audio::pcm16_to_f32;
#[cfg(feature = "whisper")]
use anyhow::Context;

/// Configuration for the whisper batch finalizer.
#[derive(Debug, Clone)]
pub struct FinalizerConfig {
    pub model_path: std::path::PathBuf,
    pub vad_path: std::path::PathBuf,
    pub language: Option<String>,
    pub num_threads: i32,
}

/// Batch finalizer interface (not streaming).
pub trait BatchFinalizer: Send + Sync {
    fn name(&self) -> &'static str;

    /// Transcribe a complete utterance. Returns empty string when VAD finds no
    /// speech (or the mock is configured for silence).
    fn transcribe(&self, pcm: &[i16]) -> Result<String>;
}

/// Offline mock finalizer for unit tests.
pub struct MockFinalizer {
    /// Transcript returned for non-silent audio.
    pub transcript: String,
    /// If true, any all-zero (or empty) input yields empty string.
    pub silence_aware: bool,
}

impl Default for MockFinalizer {
    fn default() -> Self {
        Self {
            transcript: "Hello, this is Iris dictation.".into(),
            silence_aware: true,
        }
    }
}

impl BatchFinalizer for MockFinalizer {
    fn name(&self) -> &'static str {
        "mock-finalizer"
    }

    fn transcribe(&self, pcm: &[i16]) -> Result<String> {
        if self.silence_aware && (pcm.is_empty() || pcm.iter().all(|&s| s == 0)) {
            return Ok(String::new());
        }
        Ok(self.transcript.clone())
    }
}

/// Real whisper.cpp finalizer. Requires feature `whisper`.
#[cfg(feature = "whisper")]
pub struct WhisperFinalizer {
    ctx: whisper_rs::WhisperContext,
    vad: std::sync::Mutex<whisper_rs::WhisperVadContext>,
    language: Option<String>,
    num_threads: i32,
}

#[cfg(feature = "whisper")]
impl WhisperFinalizer {
    pub fn load(config: FinalizerConfig) -> Result<Self> {
        use whisper_rs::{WhisperVadContext, WhisperVadContextParams};

        let params = whisper_rs::WhisperContextParameters::default();
        let ctx = whisper_rs::WhisperContext::new_with_params(
            config.model_path.to_str().context("model path UTF-8")?,
            params,
        )
        .map_err(|e| anyhow::anyhow!("loading whisper model: {e:?}"))?;

        let mut vad_params = WhisperVadContextParams::default();
        vad_params.set_n_threads(config.num_threads);
        vad_params.set_use_gpu(false);
        let vad = WhisperVadContext::new(
            config.vad_path.to_str().context("vad path UTF-8")?,
            vad_params,
        )
        .map_err(|e| anyhow::anyhow!("loading Silero VAD: {e:?}"))?;

        Ok(Self {
            ctx,
            vad: std::sync::Mutex::new(vad),
            language: config.language.or_else(|| Some("en".into())),
            num_threads: config.num_threads,
        })
    }

    /// Run Silero VAD; return concatenated speech samples, or empty if none.
    fn gate_with_vad(&self, samples: &[f32]) -> Result<Vec<f32>> {
        use whisper_rs::WhisperVadParams;

        let mut vad = self
            .vad
            .lock()
            .map_err(|_| anyhow::anyhow!("Silero VAD mutex poisoned"))?;

        let segs = vad
            .segments_from_samples(WhisperVadParams::new(), samples)
            .map_err(|e| anyhow::anyhow!("VAD segments: {e:?}"))?;

        if segs.num_segments() == 0 {
            return Ok(Vec::new());
        }

        let sr = crate::audio::SAMPLE_RATE as f32;
        let mut out = Vec::new();
        for seg in segs {
            // whisper-rs VAD timestamps are in centiseconds.
            let start = ((seg.start / 100.0) * sr) as usize;
            let end = ((seg.end / 100.0) * sr) as usize;
            let end = end.min(samples.len());
            let start = start.min(end);
            out.extend_from_slice(&samples[start..end]);
        }
        Ok(out)
    }
}

#[cfg(feature = "whisper")]
impl BatchFinalizer for WhisperFinalizer {
    fn name(&self) -> &'static str {
        "whisper-base.en"
    }

    fn transcribe(&self, pcm: &[i16]) -> Result<String> {
        if pcm.is_empty() {
            return Ok(String::new());
        }
        let samples = pcm16_to_f32(pcm);
        let speech = self.gate_with_vad(&samples)?;
        if speech.is_empty() {
            // Hard guarantee: no speech → empty transcript (no Whisper hallucinate).
            return Ok(String::new());
        }

        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| anyhow::anyhow!("whisper state: {e:?}"))?;

        let mut params =
            whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
        if let Some(lang) = &self.language {
            params.set_language(Some(lang.as_str()));
        }
        params.set_n_threads(self.num_threads);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        params.set_no_speech_thold(0.6);

        state
            .full(params, &speech)
            .map_err(|e| anyhow::anyhow!("whisper full: {e:?}"))?;

        let mut text = String::new();
        for segment in state.as_iter() {
            let s = segment.to_string();
            if text.is_empty() {
                text = s.trim().to_string();
            } else {
                text.push(' ');
                text.push_str(s.trim());
            }
        }
        // Strip common hallucination markers if VAD let something through.
        let cleaned = text
            .replace("[BLANK_AUDIO]", "")
            .replace("[Silence]", "")
            .trim()
            .to_string();
        Ok(cleaned)
    }
}

/// Construct the default finalizer for the current feature set.
pub fn default_finalizer(config: FinalizerConfig) -> Result<Box<dyn BatchFinalizer>> {
    #[cfg(feature = "whisper")]
    {
        return Ok(Box::new(WhisperFinalizer::load(config)?));
    }
    #[cfg(not(feature = "whisper"))]
    {
        let _ = config;
        anyhow::bail!(
            "whisper finalizer requires the `whisper` cargo feature \
             (enables whisper-rs). Use MockFinalizer for offline tests."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::read_wav_pcm16;
    use crate::{silence_fixture, speech_fixture};

    #[test]
    fn mock_finalizer_empty_on_silence_fixture() {
        let f = MockFinalizer::default();
        let pcm = read_wav_pcm16(&silence_fixture()).unwrap();
        assert_eq!(f.transcribe(&pcm).unwrap(), "");
    }

    #[test]
    fn mock_finalizer_text_on_speech_fixture() {
        let f = MockFinalizer::default();
        let pcm = read_wav_pcm16(&speech_fixture()).unwrap();
        let t = f.transcribe(&pcm).unwrap();
        assert!(!t.is_empty());
        assert!(t.contains("Iris") || t.contains("dictation") || t.contains("Hello"));
    }
}

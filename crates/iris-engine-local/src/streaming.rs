//! Streaming partials layer (ghost text).
//!
//! Preferred backend: sherpa-onnx streaming Zipformer transducer (int8) via the
//! official first-party `sherpa-onnx` crate (feature `streaming`).
//!
//! Without the feature, only [`MockStreamingEngine`] is available — used by
//! offline unit tests and as a stand-in when native deps are not built.

use anyhow::Result;

use crate::engine::{LocalEngine, LocalSession};
use crate::mock::{MockLocalEngine, MockLocalEngineConfig};

/// Configuration for the real Zipformer streaming engine.
#[derive(Debug, Clone)]
pub struct StreamingConfig {
    pub encoder: std::path::PathBuf,
    pub decoder: std::path::PathBuf,
    pub joiner: std::path::PathBuf,
    pub tokens: std::path::PathBuf,
    pub num_threads: i32,
    pub provider: String,
}

impl StreamingConfig {
    /// Build config from already-downloaded Zipformer paths.
    pub fn from_paths(paths: crate::models::ZipformerPaths) -> Self {
        Self {
            encoder: paths.encoder,
            decoder: paths.decoder,
            joiner: paths.joiner,
            tokens: paths.tokens,
            num_threads: 2,
            provider: "cpu".into(),
        }
    }
}

/// Trait-object-friendly streaming engine handle.
///
/// Under `streaming` this wraps sherpa-onnx; otherwise it is an alias for the
/// mock so callers can write feature-agnostic code.
pub struct StreamingEngine {
    inner: StreamingInner,
}

enum StreamingInner {
    Mock(MockLocalEngine),
    #[cfg(feature = "streaming")]
    Zipformer(ZipformerEngine),
}

impl StreamingEngine {
    /// Offline mock streaming engine (always available).
    pub fn mock(config: MockLocalEngineConfig) -> Self {
        Self {
            inner: StreamingInner::Mock(MockLocalEngine::new(config)),
        }
    }

    /// Real Zipformer streaming engine. Requires feature `streaming` and model files.
    #[cfg(feature = "streaming")]
    pub fn zipformer(config: StreamingConfig) -> Result<Self> {
        Ok(Self {
            inner: StreamingInner::Zipformer(ZipformerEngine::load(config)?),
        })
    }

    #[cfg(not(feature = "streaming"))]
    pub fn zipformer(_config: StreamingConfig) -> Result<Self> {
        anyhow::bail!(
            "Zipformer streaming requires the `streaming` cargo feature \
             (enables the official sherpa-onnx crate)"
        );
    }
}

impl LocalEngine for StreamingEngine {
    fn name(&self) -> &'static str {
        match &self.inner {
            StreamingInner::Mock(_) => "streaming-mock",
            #[cfg(feature = "streaming")]
            StreamingInner::Zipformer(_) => "zipformer-streaming",
        }
    }

    fn streams_partials(&self) -> bool {
        true
    }

    fn start(&self) -> Result<Box<dyn LocalSession>> {
        match &self.inner {
            StreamingInner::Mock(m) => m.start(),
            #[cfg(feature = "streaming")]
            StreamingInner::Zipformer(z) => z.start(),
        }
    }
}

/// Alias used by tests and the layered engine when only a mock is wanted.
pub type MockStreamingEngine = StreamingEngine;

// ─── sherpa-onnx Zipformer ───────────────────────────────────────────────────

#[cfg(feature = "streaming")]
use std::sync::Arc;

#[cfg(feature = "streaming")]
use anyhow::Context;
#[cfg(feature = "streaming")]
use crossbeam_channel::{Receiver, Sender};

#[cfg(feature = "streaming")]
use crate::audio::{pcm16_to_f32, SAMPLE_RATE};
#[cfg(feature = "streaming")]
use crate::engine::LocalEvent;

#[cfg(feature = "streaming")]
struct ZipformerEngine {
    recognizer: Arc<sherpa_onnx::OnlineRecognizer>,
}

#[cfg(feature = "streaming")]
impl ZipformerEngine {
    fn load(config: StreamingConfig) -> Result<Self> {
        use sherpa_onnx::{
            OnlineModelConfig, OnlineRecognizer, OnlineRecognizerConfig,
            OnlineTransducerModelConfig,
        };

        let mut model = OnlineModelConfig::default();
        model.transducer = OnlineTransducerModelConfig {
            encoder: Some(config.encoder.to_string_lossy().into_owned()),
            decoder: Some(config.decoder.to_string_lossy().into_owned()),
            joiner: Some(config.joiner.to_string_lossy().into_owned()),
        };
        model.tokens = Some(config.tokens.to_string_lossy().into_owned());
        model.num_threads = config.num_threads;
        model.provider = Some(config.provider);

        let mut cfg = OnlineRecognizerConfig::default();
        cfg.model_config = model;
        cfg.decoding_method = Some("greedy_search".into());
        cfg.enable_endpoint = false;

        let recognizer = OnlineRecognizer::create(&cfg)
            .context("creating sherpa-onnx OnlineRecognizer for Zipformer")?;
        Ok(Self {
            recognizer: Arc::new(recognizer),
        })
    }

    fn start(&self) -> Result<Box<dyn LocalSession>> {
        let stream = self.recognizer.create_stream();
        let (tx, rx) = crossbeam_channel::unbounded();
        let _ = tx.send(LocalEvent::Ready);
        Ok(Box::new(ZipformerSession {
            recognizer: Arc::clone(&self.recognizer),
            stream,
            tx,
            rx,
            last_partial: String::new(),
            finished: false,
        }))
    }
}

#[cfg(feature = "streaming")]
struct ZipformerSession {
    recognizer: Arc<sherpa_onnx::OnlineRecognizer>,
    stream: sherpa_onnx::OnlineStream,
    tx: Sender<LocalEvent>,
    rx: Receiver<LocalEvent>,
    last_partial: String,
    finished: bool,
}

#[cfg(feature = "streaming")]
impl ZipformerSession {
    fn decode_ready(&mut self) {
        while self.recognizer.is_ready(&self.stream) {
            self.recognizer.decode(&self.stream);
        }
        if let Some(result) = self.recognizer.get_result(&self.stream) {
            let text = result.text.trim().to_string();
            if !text.is_empty() && text != self.last_partial {
                self.last_partial = text.clone();
                let _ = self.tx.send(LocalEvent::Partial(text));
            }
        }
    }
}

#[cfg(feature = "streaming")]
impl LocalSession for ZipformerSession {
    fn feed(&mut self, pcm: &[i16]) -> Result<()> {
        if self.finished || pcm.is_empty() {
            return Ok(());
        }
        let samples = pcm16_to_f32(pcm);
        self.stream
            .accept_waveform(SAMPLE_RATE as i32, &samples);
        self.decode_ready();
        Ok(())
    }

    fn partials(&self) -> &Receiver<LocalEvent> {
        &self.rx
    }

    fn finalize(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.stream.input_finished();
        self.decode_ready();
        // Streaming layer's own "final" is still ghost text — emit as Final so a
        // streaming-only session works, but layered engine replaces this with
        // the batch transcript of record.
        let text = self.last_partial.clone();
        let _ = self.tx.send(LocalEvent::Final(text));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_streaming_engine_name() {
        let e = StreamingEngine::mock(MockLocalEngineConfig::default());
        assert_eq!(e.name(), "streaming-mock");
        assert!(e.streams_partials());
    }

    #[test]
    #[cfg(not(feature = "streaming"))]
    fn zipformer_without_feature_errors() {
        let err = StreamingEngine::zipformer(StreamingConfig {
            encoder: "e".into(),
            decoder: "d".into(),
            joiner: "j".into(),
            tokens: "t".into(),
            num_threads: 1,
            provider: "cpu".into(),
        });
        assert!(err.is_err());
        assert!(err.err().unwrap().to_string().contains("streaming"));
    }
}

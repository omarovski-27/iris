//! Local (offline/private) speech-to-text engines for Iris.
//!
//! # Architecture
//!
//! Two layered engines, matching the local-ASR evaluation report:
//!
//! 1. **Streaming partials** — sherpa-onnx streaming Zipformer transducer (int8).
//!    Paints live "ghost text" while the user speaks; finalization after the last
//!    frame is typically under 40 ms. Its own transcript is **never** the text of
//!    record (no punctuation/casing, weaker accuracy).
//! 2. **Transcript of record** — batch finalizer. v1 ships whisper.cpp
//!    `base.en` q5_1 via `whisper-rs`, **always gated by Silero VAD** (Whisper
//!    hallucinates on silence without it). Parakeet-TDT via whisper.cpp's new
//!    GGML backend is preferred long-term but has no Rust binding yet — see
//!    the crate README for the tradeoff.
//!
//! # Trait shape
//!
//! The core pipeline defines an async streaming `Engine` trait
//! (`open` → `push` → partials → `finish`). That crate is not yet merged into
//! this branch, so this crate mirrors it with [`LocalEngine`] /
//! [`LocalSession`] (`start` / `feed` / `partials` / `finalize`). Wiring an
//! adapter to the final core trait is a one-file follow-up.
//!
//! # Features
//!
//! | Feature      | What it enables                                      |
//! |--------------|------------------------------------------------------|
//! | *(default)*  | Trait, mock engine, model manager, audio helpers     |
//! | `streaming`  | sherpa-onnx Zipformer streaming engine               |
//! | `whisper`    | whisper-rs batch finalizer + Silero VAD              |
//! | `native`     | `streaming` + `whisper`                              |
//!
//! Offline unit tests never download models and never require native features.

pub mod audio;
pub mod engine;
pub mod finalizer;
pub mod layered;
pub mod mock;
pub mod models;
pub mod streaming;

pub use audio::{pcm16_to_f32, read_wav_pcm16, sample_rate, SAMPLE_RATE};
pub use engine::{LocalEngine, LocalEvent, LocalSession};
#[cfg(feature = "whisper")]
pub use finalizer::WhisperFinalizer;
pub use finalizer::{BatchFinalizer, FinalizerConfig, MockFinalizer};
pub use layered::{LayeredLocalEngine, LayeredLocalEngineConfig};
pub use mock::{MockLocalEngine, MockLocalEngineConfig};
pub use models::{
    ensure_model, ensure_models, ModelCatalog, ModelId, ModelSpec, ProgressFn,
    DEFAULT_MODEL_DIR_ENV,
};
pub use streaming::{MockStreamingEngine, StreamingConfig, StreamingEngine};

/// Directory containing committed test fixtures shipped with this crate.
pub fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Path to the committed silence WAV (0.5 s, 16 kHz mono PCM16).
pub fn silence_fixture() -> std::path::PathBuf {
    fixtures_dir().join("silence-0.5s-16k.wav")
}

/// Path to the committed short speech WAV (16 kHz mono PCM16, espeak-ng).
pub fn speech_fixture() -> std::path::PathBuf {
    fixtures_dir().join("speech-16k.wav")
}

//! Core of the Iris dictation pipeline.
//!
//! The pipeline is deliberately split so that everything except the three
//! OS-bound pieces (microphone capture, the global hotkey hook, and text
//! injection) compiles and is testable on any platform:
//!
//! ```text
//!   hotkey  ──►  capture  ──►  Resampler ──►  Engine::Session  ──►  inject
//!  (Windows)    (Windows)     (portable)       (portable)         (Windows)
//!                                  │                 │
//!                                  └──── Timeline ───┘   (portable)
//! ```
//!
//! [`dictation::Dictation`] is the portable driver that owns a
//! [`engine::Session`] and a [`latency::Timeline`]; the live Windows pipeline
//! and the CI latency harness both go through it, so the latency numbers come
//! from the same code path.

pub mod audio;
pub mod dictation;
pub mod engine;
pub mod hotkey;
pub mod inject;
pub mod latency;
pub mod log;
pub mod text;

/// Microphone capture. Windows-only: it is the one module that pulls in `cpal`,
/// and keeping it off other hosts means the harness and the test suite have no
/// audio-stack dependency at all.
#[cfg(windows)]
pub mod capture;

pub use audio::{Resampler, FRAME_SAMPLES, SAMPLE_RATE};
pub use dictation::{Dictation, DictationError, DictationOutcome};
pub use engine::{Engine, EngineSpec, Session, TranscriptEvent};
pub use latency::{Mark, Timeline};

/// Path to the speech fixture committed with the repository.
///
/// A ~5.4 s, 16 kHz mono clip synthesised with `espeak-ng` (see
/// `assets/README.md`). It lets the harness and the tests exercise the full
/// engine path with no microphone.
pub fn sample_wav_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/speech-16k.wav")
}

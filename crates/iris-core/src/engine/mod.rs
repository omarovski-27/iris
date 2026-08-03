//! The pluggable transcription engine.
//!
//! # Why this shape
//!
//! The whole product bet is that transcription happens *while* the user speaks,
//! so that key-release → text is a finalisation round-trip and not a
//! transcription. That rules out the obvious `fn transcribe(&self, pcm: &[i16])
//! -> String` interface, which forces record-then-transcribe and puts the entire
//! model inference after the key release. Every open-source dictation tool that
//! feels slow has that signature somewhere in it.
//!
//! So the trait is a session with a lifecycle:
//!
//! ```text
//!   Engine::open() ──► Session::push(pcm) ×N ──► Session::finish()
//!                            │                        │
//!                            └── TranscriptEvent ─────┴──► Final
//! ```
//!
//! Three properties matter and are load-bearing for the rest of the app:
//!
//! 1. **`open()` returns immediately.** A network engine connects in the
//!    background and buffers; the connection cost overlaps with speech instead
//!    of delaying capture.
//! 2. **`push()` never blocks.** It is called from the audio path, where
//!    blocking means dropped frames and an audible glitch.
//! 3. **`finish()` is non-blocking too**, and the final transcript arrives as an
//!    event. The caller decides how long to wait, and can keep painting an
//!    overlay while it does.
//!
//! An engine that can only do batch transcription (see [`groq`]) still fits: it
//! accumulates in `push` and does its work in `finish`. It just cannot hide any
//! of its latency, which is exactly what the latency report then shows.

use anyhow::{bail, Result};
use crossbeam_channel::Receiver;

pub mod deepgram;
pub mod groq;
pub mod mock;
pub mod net;

pub use deepgram::DeepgramEngine;
pub use groq::GroqEngine;
pub use mock::{MockConfig, MockEngine};

/// Everything an engine can tell us about a session, in arrival order.
///
/// Well-behaved engines emit at most one post-[`Session::finish`] terminal
/// event: [`TranscriptEvent::Final`] or [`TranscriptEvent::Error`]. Segment
/// hypotheses during the hold must be [`TranscriptEvent::Partial`] only —
/// mid-hold "finals" from endpointing are not session-terminal.
///
/// [`crate::dictation::Dictation`] defends the hold either way: a `Final`
/// before `finish()` is demoted to a sticky partial; after `finish()`, a
/// longer partial can still beat a short `Final`; and if the channel closes
/// (or errors / times out) with no usable `Final`, any non-empty partial is
/// salvaged rather than discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    /// The transport is up and audio is flowing. Engines with no transport emit
    /// this immediately.
    Connected,
    /// Interim text covering the audio so far. Successive partials replace one
    /// another; they are not appended. Streaming engines that segment on
    /// silence should still surface each committed segment through this
    /// variant (as running text), not as [`TranscriptEvent::Final`].
    Partial(String),
    /// The complete transcript for the session. Intended only after
    /// [`Session::finish`]; see the enum docs for how earlier arrivals are
    /// handled.
    Final(String),
    /// The session failed. After `finish()`, [`crate::dictation::Dictation`]
    /// may still salvage a non-empty partial instead of surfacing the error.
    Error(String),
}

/// A transcription backend.
pub trait Engine: Send + Sync {
    fn name(&self) -> &'static str;

    /// Whether this engine emits [`TranscriptEvent::Partial`] while audio
    /// streams. Engines that return `false` cannot hide latency behind speech,
    /// and the UI should not promise live text.
    fn streams_partials(&self) -> bool {
        true
    }

    /// How long a caller should wait after [`Session::finish`] before giving
    /// up on this engine's transcript.
    ///
    /// **Choose this consciously.** The default,
    /// [`crate::dictation::DEFAULT_FINAL_TIMEOUT`], is a *streaming* figure:
    /// it assumes the transcript is nearly complete by key-up and only a
    /// finalisation round-trip remains. An engine that does its real work in
    /// `finish` — upload and inference after the key comes up, as [`groq`]
    /// does — has a completely different distribution, and inheriting the
    /// streaming bound means cutting the user's words off mid-upload. Where
    /// [`Engine::streams_partials`] is `false` that loss is total, because
    /// there is no partial to salvage.
    ///
    /// The bound is per engine, asked for per dictation, so switching engines
    /// at runtime switches the wait with it.
    ///
    /// Choose it knowing what it costs: [`crate::dictation::Dictation::finish`]
    /// blocks its caller's loop for this long, which in the resident app means
    /// a frozen overlay and an unresponsive tray. An engine that needs a
    /// generous value should bound its own work from underneath (a request
    /// timeout, a connect timeout) and leave this as the backstop.
    fn final_timeout(&self) -> std::time::Duration {
        crate::dictation::DEFAULT_FINAL_TIMEOUT
    }

    /// Open a streaming session. Must return without waiting on the network.
    fn open(&self) -> Result<Box<dyn Session>>;
}

/// One dictation's worth of streaming state.
pub trait Session: Send {
    /// Feed 16 kHz mono PCM. Called from the audio path: must not block, must
    /// not allocate unboundedly, and must not fail on a slow network — drop or
    /// buffer instead.
    fn push(&mut self, pcm: &[i16]) -> Result<()>;

    /// Events from the engine. Drained by the caller.
    fn events(&self) -> &Receiver<TranscriptEvent>;

    /// End of speech. No more `push` calls will come. Non-blocking: the final
    /// transcript arrives on [`Session::events`].
    fn finish(&mut self) -> Result<()>;
}

/// Which engine to build, parsed from `--engine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineSpec {
    /// Deterministic, offline, instant. The default for tests and CI.
    Mock,
    /// Deepgram streaming websocket. Needs `IRIS_DEEPGRAM_KEY`.
    Deepgram,
    /// Groq Whisper, batch on key-release. Needs `IRIS_GROQ_KEY`.
    Groq,
}

impl std::str::FromStr for EngineSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mock" => Ok(EngineSpec::Mock),
            "deepgram" | "dg" => Ok(EngineSpec::Deepgram),
            "groq" => Ok(EngineSpec::Groq),
            other => bail!("unknown engine {other:?} (expected mock, deepgram or groq)"),
        }
    }
}

impl std::fmt::Display for EngineSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EngineSpec::Mock => "mock",
            EngineSpec::Deepgram => "deepgram",
            EngineSpec::Groq => "groq",
        })
    }
}

/// Optional knobs shared by the engine constructors.
#[derive(Debug, Clone, Default)]
pub struct EngineOptions {
    /// Override the engine's default model.
    pub model: Option<String>,
    /// Language hint, e.g. `en`. `None` lets the engine decide.
    pub language: Option<String>,
}

/// Build an engine, reading API keys from the environment.
///
/// Keys are never read from disk or arguments, so they cannot end up in a shell
/// history or a config file that gets committed.
pub fn build(spec: EngineSpec, opts: &EngineOptions) -> Result<Box<dyn Engine>> {
    match spec {
        EngineSpec::Mock => Ok(Box::new(MockEngine::new(MockConfig::default()))),
        EngineSpec::Deepgram => Ok(Box::new(DeepgramEngine::from_env(opts)?)),
        EngineSpec::Groq => Ok(Box::new(GroqEngine::from_env(opts)?)),
    }
}

/// Read an API key, failing with a message that says exactly what to do.
pub(crate) fn require_key(var: &str, engine: &str, signup: &str) -> Result<String> {
    match std::env::var(var) {
        Ok(k) if !k.trim().is_empty() => Ok(k.trim().to_string()),
        _ => bail!(
            "the {engine} engine needs an API key in ${var}, which is unset or empty.\n\
             Get one at {signup}, then:\n    export {var}=...\n\
             Or run with `--engine mock` to exercise the pipeline with no key and no network."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_spec_parses_and_round_trips() {
        for (s, e) in [
            ("mock", EngineSpec::Mock),
            ("MOCK", EngineSpec::Mock),
            ("deepgram", EngineSpec::Deepgram),
            ("dg", EngineSpec::Deepgram),
            (" groq ", EngineSpec::Groq),
        ] {
            assert_eq!(s.parse::<EngineSpec>().unwrap(), e);
        }
        assert_eq!(EngineSpec::Deepgram.to_string(), "deepgram");
        assert!("whisper".parse::<EngineSpec>().is_err());
    }

    #[test]
    fn missing_key_explains_the_fix() {
        let err = require_key("IRIS_TEST_KEY_THAT_IS_UNSET", "test", "https://example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("IRIS_TEST_KEY_THAT_IS_UNSET"));
        assert!(err.contains("--engine mock"));
    }

    #[test]
    fn mock_builds_without_any_environment() {
        let e = build(EngineSpec::Mock, &EngineOptions::default()).unwrap();
        assert_eq!(e.name(), "mock");
    }
}

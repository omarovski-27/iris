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
pub mod deepgram_balance;
mod failure;
pub mod groq;
pub mod mock;
pub mod net;

pub use deepgram::DeepgramEngine;
pub(crate) use failure::Failure;
pub use failure::FailureCause;
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
    /// The session failed for a specific, classified reason — a rejected key,
    /// an exhausted balance, a rate limit, an unreachable network, or a
    /// connect timeout — as opposed to [`TranscriptEvent::Error`], which
    /// covers every failure this project cannot currently classify (a
    /// mid-stream socket death, a malformed protocol message, `finish()`
    /// itself failing). Kept as its own variant rather than widening `Error`
    /// with an `Option<FailureCause>` so every existing `Error` call site —
    /// the local engine adapter, the mock engine, every test that constructs
    /// a plain string failure — keeps compiling unchanged; only the call
    /// sites in [`deepgram`] and [`groq`] that can actually name a cause opt
    /// in. See [`FailureCause`] for why this classification exists and how
    /// each engine derives it from the provider's own response.
    Failed {
        message: String,
        cause: FailureCause,
    },
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

    /// How long this engine allows itself to get its transport up, measured
    /// from key-down. `None` for an engine with no connect step of its own.
    ///
    /// Publishing it is what stops [`Engine::final_timeout`] — measured from
    /// key-up, so on a short hold it can expire first — from killing a connect
    /// that is still inside the budget this engine set for it.
    /// [`crate::dictation::Dictation::finish`] waits out this budget instead
    /// while the session has produced nothing to salvage — and, if the connect
    /// then succeeds after the plain deadline has already passed, gives it
    /// [`Engine::final_timeout`] from that instant to produce something, so
    /// the wait covers the whole path rather than ending exactly where the
    /// connection starts being useful. That whole window is the one case where
    /// giving up early costs the whole utterance rather than a tail of it. It
    /// is not a second final timeout, and it is not a place to buy an engine
    /// more room in general: a single partial stops any further extending. It
    /// does not shorten the window already bought, so a late connect that
    /// streams its first partial after that point still blocks for it.
    ///
    /// An engine whose connect happens *inside* `finish` (a batch upload, say)
    /// already has it covered by `final_timeout` and should leave this `None`.
    fn connect_budget(&self) -> Option<std::time::Duration> {
        None
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
    /// User-maintained words and phrases (names, jargon, acronyms) that
    /// should come back spelled the way the user meant. `Vec::is_empty` is
    /// the "no vocabulary configured" case, and every engine must treat it as
    /// a complete no-op: no extra request field, no added latency.
    ///
    /// Each engine consumes this the only way its own API allows, so the
    /// *effect* differs by engine, not just the wiring:
    /// - [`deepgram::DeepgramEngine`] sends it as `keyterm` query parameters —
    ///   nova-3's keyterm prompting
    ///   (<https://developers.deepgram.com/docs/keyterm>, checked 2026-08-11).
    ///   The older `keywords` parameter is nova-2-and-earlier only and does
    ///   not exist for the `nova-3` model this engine defaults to, so this is
    ///   the current mechanism for the model Iris actually ships, not a
    ///   downgrade from it.
    /// - [`groq::GroqEngine`] and the local Whisper finalizer
    ///   (`iris-engine-local`) have no keyword-list mechanism at all — their
    ///   APIs only accept one initial-prompt string — so both join the list
    ///   into a prompt via [`vocabulary_prompt`] instead.
    /// - The mock engine ignores this field entirely: it is a scripted,
    ///   offline stand-in, not a real transcriber a hint could improve.
    ///
    /// Every consumer also caps how much of the list it actually sends,
    /// because the providers cap it too (Deepgram: 500 tokens per keyterm
    /// request; Groq: a 224-token prompt) — see [`vocabulary_prompt`] and
    /// `deepgram`'s own token cap constant. An oversized list is quietly
    /// shortened, never a failed dictation.
    pub vocabulary: Vec<String>,
}

/// A word-count budget for [`vocabulary_prompt`], conservative enough to sit
/// under both `groq::GroqEngine`'s 224-token Whisper `prompt` field and the
/// local Whisper finalizer's initial prompt (`iris-engine-local`, which has
/// no independently documented limit but runs the same underlying model
/// family) — see [`vocabulary_prompt`]'s own doc comment for why word count
/// is a proxy for tokens, not an exact measure. Shared rather than
/// per-engine so the two prompt-based consumers do not silently drift apart
/// on how conservative they are.
pub const MAX_VOCABULARY_PROMPT_WORDS: usize = 150;

/// Turns a vocabulary list into a single initial-prompt string for engines
/// that only accept one (Groq, the local Whisper finalizer) rather than a
/// keyword list — see [`EngineOptions::vocabulary`]. Terms are joined with a
/// comma, Whisper's own documented convention for listing proper nouns in a
/// prompt.
///
/// `max_words` bounds the result by word count, a proxy for the provider's
/// token limit: no tokenizer is available here, and English words average
/// somewhat more than one token each, so a caller should pick `max_words`
/// with headroom under the real token cap rather than equal to it. Truncating
/// this way — quietly, before the request goes out — is what keeps an
/// oversized vocabulary from turning into a rejected request or a
/// mid-word-truncated prompt on the provider's side.
///
/// Returns `None` for an empty (or all-blank) vocabulary, so a caller can
/// tell "no prompt" from "empty string prompt" without inspecting the list
/// itself — the distinction a request builder needs to add no field at all
/// when nothing was configured.
#[must_use]
pub fn vocabulary_prompt(vocabulary: &[String], max_words: usize) -> Option<String> {
    let mut words_used = 0usize;
    let mut terms = Vec::new();
    for term in vocabulary {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        let words = term.split_whitespace().count().max(1);
        if words_used + words > max_words {
            break;
        }
        words_used += words;
        terms.push(term);
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(", "))
    }
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

    #[test]
    fn vocabulary_prompt_is_none_for_empty_or_blank_lists() {
        assert_eq!(vocabulary_prompt(&[], 100), None);
        assert_eq!(
            vocabulary_prompt(&["  ".into(), "".into()], 100),
            None,
            "whitespace-only entries must not produce an empty-string prompt"
        );
    }

    #[test]
    fn vocabulary_prompt_joins_trimmed_terms_with_commas() {
        let vocabulary = vec!["  Deepgram  ".into(), "Zipformer".into()];
        assert_eq!(
            vocabulary_prompt(&vocabulary, 100),
            Some("Deepgram, Zipformer".into())
        );
    }

    #[test]
    fn vocabulary_prompt_stops_once_the_word_budget_is_spent() {
        let vocabulary = vec!["one two".into(), "three".into(), "four".into()];
        // "one two" spends 2 of a 3-word budget, "three" spends the last word,
        // "four" no longer fits and must not appear.
        let prompt = vocabulary_prompt(&vocabulary, 3).unwrap();
        assert_eq!(prompt, "one two, three");
        assert!(!prompt.contains("four"));
    }
}

//! Text cleanup for [Iris](https://github.com/omarovski-27/iris) voice dictation.
//!
//! `iris-polish` turns a raw speech-to-text transcript into text that reads like
//! the speaker typed it carefully: punctuation and casing fixed, filler sounds and
//! false starts gone, meaning untouched.
//!
//! # The contract
//!
//! Everything here obeys one rule: **when uncertain, leave the text alone.** A
//! slightly messy transcript is a far better outcome than a tidy one that says
//! something the speaker did not say. Every heuristic in this crate is chosen so
//! that its failure mode is "did nothing", not "changed the meaning".
//!
//! # Where this sits
//!
//! Polishing runs between "user releases the hotkey" and "text appears in the
//! target app", so it is on the critical latency path. The whole step has a
//! budget of [`DEFAULT_LATENCY_BUDGET`] (150 ms). [`FallbackPolisher`] enforces
//! that budget: it computes the instant rule-based result up front, races the
//! slow-but-better polisher against the clock, and returns whichever is ready.
//! The user never waits longer than the budget, and never gets nothing.
//!
//! # The implementations
//!
//! | Polisher | Cost | Use |
//! |---|---|---|
//! | [`RulePolisher`] | microseconds, no I/O | baseline; offline; the fallback |
//! | [`LlmPolisher`] | one HTTP round trip | the quality path |
//! | [`MockPolisher`] | configurable | tests |
//!
//! # Example
//!
//! ```
//! use iris_polish::{PolishRequest, Polisher, RulePolisher};
//!
//! # async fn demo() {
//! let polisher = RulePolisher::default();
//! let request = PolishRequest::new("um, so i pushed the fix to main");
//! let polished = polisher.polish(&request).await.unwrap();
//! assert_eq!(polished.text, "So I pushed the fix to main.");
//! # }
//! ```
//!
//! # Cancellation
//!
//! [`Polisher::polish`] returns a plain future: drop it to cancel. For
//! [`LlmPolisher`] that aborts the in-flight HTTP request. Nothing in this crate
//! spawns detached work, so a dropped future leaves nothing running.
//!
//! # Features
//!
//! `llm` (default) pulls in the HTTP and TLS stack for [`LlmPolisher`]. Without
//! it the crate is dependency-light and entirely offline: [`RulePolisher`],
//! [`MockPolisher`], [`FallbackPolisher`], and the corpus.
#![cfg_attr(
    feature = "llm",
    doc = r##"
# Wiring the real thing

The LLM path guarded by the latency budget, backed by the rule engine — the
decision Iris makes at startup:

```no_run
use std::sync::Arc;
use iris_polish::{FallbackPolisher, LlmPolisher, PolishRequest, Polisher, RulePolisher};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let polisher: Arc<dyn Polisher> = match LlmPolisher::from_env() {
    Ok(llm) => Arc::new(FallbackPolisher::new(Arc::new(llm), Arc::new(RulePolisher::default()))),
    // No API key, or offline: the rule engine alone is a complete polisher.
    Err(_) => Arc::new(RulePolisher::default()),
};
let out = polisher.polish(&PolishRequest::new("um so uh it works")).await?;
println!("{} ({:?} via {})", out.text, out.duration, out.source);
# Ok(())
# }
```
"##
)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]

pub mod corpus;
mod error;
mod fallback;
#[cfg(feature = "llm")]
mod llm;
mod mock;
mod polisher;
#[cfg(feature = "llm")]
mod prompt;
mod rule;
mod types;

pub use error::PolishError;
pub use fallback::{FallbackPolisher, DEFAULT_LATENCY_BUDGET};
pub use mock::{MockBehavior, MockPolisher};
pub use polisher::{PolishFuture, Polisher};
pub use rule::{RuleConfig, RulePolisher, FILLER_PHRASES, FILLER_WORDS, STUTTER_WORDS};
pub use types::{ContextHints, FallbackReason, PolishRequest, PolishSource, Polished, TextStyle};

#[cfg(feature = "llm")]
pub use llm::{
    ChatTransport, LlmConfig, LlmPolisher, OutputGuards, TransportFuture, DEFAULT_BASE_URL,
    DEFAULT_MODEL, ENV_BASE_URL, ENV_GROQ_KEY, ENV_KEY, ENV_MODEL, ENV_TIMEOUT_MS,
};
#[cfg(feature = "llm")]
pub use prompt::{build_user_message, SYSTEM_PROMPT};

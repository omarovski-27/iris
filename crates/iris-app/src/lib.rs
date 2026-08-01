//! Iris: the resident push-to-talk dictation app.
//!
//! This crate is the product. It owns no algorithms — capture, transcription,
//! injection and latency instrumentation live in [`iris_core`], transcript
//! cleanup in [`iris_polish`], offline ASR in [`iris_engine_local`] — and
//! spends all of its own code on the three things only an application can do:
//! hold the pieces together, keep the user's settings, and be honest about what
//! happened.
//!
//! # The loop
//!
//! ```text
//!   hold hotkey ─► capture ─► Engine (streams while you speak)
//!                                   │
//!   release ────────────────────────┴─► final transcript
//!                                        │
//!                                        ├─► polish (LLM, rule-engine fallback)
//!                                        ├─► inject into the focused window
//!                                        └─► append to the session log
//! ```
//!
//! Everything before the release happens while the user is still talking and is
//! therefore free; see `docs/spike-findings.md`. The stages after it are the
//! whole latency budget, which is why polish is deadline-bounded
//! ([`iris_polish::FallbackPolisher`]) and why the session log is written after
//! the text has been injected, never before.
//!
//! # Threads
//!
//! | Thread | Owner | Rule |
//! |---|---|---|
//! | hotkey | [`iris_core::hotkey`] | pumps messages only; Windows uninstalls a slow hook |
//! | audio | WASAPI via cpal | never blocks; hands 16 kHz frames to a channel |
//! | tray | [`tray`] | pumps messages, sends [`Command`]s |
//! | overlay | [`iris_overlay`] via [`pill::OverlayPill`] | paints the pill; never blocks the loop |
//! | main | [`App`] | owns the engine, the timeline, injection and the log |
//!
//! Nothing is shared behind a lock, so no part of the latency budget is spent
//! waiting for another part of the program.
//!
//! # Testability
//!
//! [`App`] is generic over its audio source and holds its injector and pill
//! sink as trait objects, so the whole state machine runs offline against a
//! mock engine and a recording injector. **Real injection is never exercised by
//! a test**: synthetic keystrokes go to whoever is using the machine (see the
//! project's `CLAUDE.md`), so [`inject::SystemInjector`] is constructed by
//! `main` and nowhere else.

// Deny rather than forbid: a couple of Win32 calls have no safe wrapper — the
// tray's message pump, and `window::shell`'s one timezone query. Each opts
// back in explicitly, at the smallest scope that compiles, with the reason
// written above it; nothing else in the crate may.
#![deny(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]

pub mod app;
pub mod audio;
pub mod config;
pub mod engines;
pub mod history;
pub mod inject;
pub mod pill;
pub mod polish;
pub mod tray;
pub mod window;

pub use app::{App, Command, Dictated};
pub use config::{Config, EngineChoice, Theme};
pub use history::{DictationRecord, LatencyBreakdown, SessionLog};
pub use inject::{Injector, RecordingInjector};
pub use pill::{overlay_theme, LogPill, NoopPill, OverlayPill, PillEvent, PillSink, RecordingPill};
pub use window::{NoopWindow, RecordingWindow, WindowSink};

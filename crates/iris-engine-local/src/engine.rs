//! Internal streaming engine trait for the local ASR layer.
//!
//! Mirrors the (not-yet-merged) core `Engine` / `Session` shape so the eventual
//! adapter is a thin one-file mapping of names:
//!
//! | core `Engine` | this crate `LocalEngine` |
//! |---------------|--------------------------|
//! | `open`        | `start`                  |
//! | `push`        | `feed`                   |
//! | `events`      | `partials`               |
//! | `finish`      | `finalize`               |

use anyhow::Result;
use crossbeam_channel::Receiver;

/// Everything a local engine can emit about a session, in arrival order.
///
/// A session ends with exactly one [`LocalEvent::Final`] or
/// [`LocalEvent::Error`]. Channel close without either is treated as failure by
/// callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalEvent {
    /// Engine is ready (models loaded / session open). Engines with no setup
    /// emit this immediately.
    Ready,
    /// Interim ghost text covering audio so far. Successive partials **replace**
    /// one another; they are not appended.
    Partial(String),
    /// Transcript of record. Terminal.
    Final(String),
    /// Session failed. Terminal.
    Error(String),
}

/// A local transcription backend.
pub trait LocalEngine: Send + Sync {
    fn name(&self) -> &'static str;

    /// Whether this engine emits [`LocalEvent::Partial`] while audio streams.
    fn streams_partials(&self) -> bool {
        true
    }

    /// Open a session. Must return without waiting on the network; model load
    /// should already have been paid at construction / warm-up time when
    /// possible.
    fn start(&self) -> Result<Box<dyn LocalSession>>;
}

/// One dictation's worth of streaming state.
pub trait LocalSession: Send {
    /// Feed 16 kHz mono PCM16. Accepts arbitrary-size frames.
    ///
    /// Called from the audio path: should not block on network or long I/O.
    fn feed(&mut self, pcm: &[i16]) -> Result<()>;

    /// Events from the engine (Ready / Partial / Final / Error).
    fn partials(&self) -> &Receiver<LocalEvent>;

    /// End of speech. No more `feed` calls will come.
    ///
    /// May block on batch work (e.g. Whisper finalization). Engines may deliver
    /// [`LocalEvent::Final`] on [`LocalSession::partials`] before this returns
    /// (typical for batch engines) or shortly afterward on a worker thread
    /// (allowed for mocks / deferred delivery). Callers must not assume this is
    /// non-blocking and should avoid invoking it on a real-time audio callback
    /// thread when a batch finalizer is in use.
    fn finalize(&mut self) -> Result<()>;
}

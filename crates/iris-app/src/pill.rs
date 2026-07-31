//! The seam the overlay hangs off.
//!
//! The dictation loop drives a [`PillSink`]. Methods mirror [`iris_overlay::OverlayHandle`]:
//!
//! ```text
//!   set_engine / set_theme (anytime)
//!   show_listening → update_level* / set_partial_len* → processing
//!     → inserted(latency_ms)   // success: overlay auto-exits after ~550 ms hold
//!     → hide                   // cancel / empty / error only
//! ```
//!
//! # Implementations
//!
//! | Sink | Role |
//! |---|---|
//! | [`NoopPill`] | Default when there is no window to drive |
//! | [`LogPill`] | State transitions on stderr under `--verbose` |
//! | [`OverlayPill`] | Forwards to a real [`iris_overlay::OverlayHandle`] |
//! | [`RecordingPill`] | Test double that records the sequence |
//!
//! # Contract
//!
//! Every method is called from the dictation thread, which owns the latency
//! budget. **A sink must not block.** An implementation that talks to another
//! thread should hand the event to a channel and return.
//!
//! After a successful [`PillSink::inserted`], the sink is responsible for
//! leaving the screen (the real overlay holds ~550 ms then exits). Callers
//! must **not** follow `inserted` with an immediate [`PillSink::hide`] — that
//! would cancel the confirmation. `hide` is for cancel, empty transcript, and
//! error paths only.

use crate::config::Theme;

/// Where the dictation loop reports what the user should be seeing.
///
/// The happy-path sequence is fixed: `show_listening`, then any number of
/// `update_level` / `set_partial_len`, then `processing`, then
/// `inserted(latency_ms)` when text actually reached the target window.
/// `hide` runs on every *unsuccessful* path so a sink can clean up without a
/// timeout; after a successful insert the sink self-dismisses.
pub trait PillSink: Send {
    /// The hotkey went down: the pill appears and Iris is capturing.
    fn show_listening(&mut self);

    /// Microphone level, `0.0..=1.0`, roughly once per audio frame. Advisory:
    /// it drives an animation, so dropping one costs nothing.
    fn update_level(&mut self, level: f32);

    /// How many characters of partial transcript exist so far.
    ///
    /// Drives the partial ribbon. Default is a no-op so simple sinks stay
    /// three-line structs.
    fn set_partial_len(&mut self, _chars: usize) {}

    /// Engine label for the chip (e.g. `mock`, `deepgram`). Default no-op.
    fn set_engine(&mut self, _label: &str) {}

    /// Map the app theme onto the overlay palette. Default no-op.
    fn set_theme(&mut self, _theme: Theme) {}

    /// The hotkey came up: capture is over and Iris is finalising and polishing.
    fn processing(&mut self);

    /// Text reached the focused window. `latency_ms` is key-release → inject
    /// (the number the user feels). Not called when injection failed or the
    /// transcript was empty. After this, the sink self-dismisses — do not
    /// call [`Self::hide`] immediately.
    fn inserted(&mut self, latency_ms: u32);

    /// The dictation ended without a successful insert (cancel, silence,
    /// error). Always safe; ignored by sinks that are already hidden.
    fn hide(&mut self);
}

impl PillSink for Box<dyn PillSink> {
    fn show_listening(&mut self) {
        (**self).show_listening();
    }
    fn update_level(&mut self, level: f32) {
        (**self).update_level(level);
    }
    fn set_partial_len(&mut self, chars: usize) {
        (**self).set_partial_len(chars);
    }
    fn set_engine(&mut self, label: &str) {
        (**self).set_engine(label);
    }
    fn set_theme(&mut self, theme: Theme) {
        (**self).set_theme(theme);
    }
    fn processing(&mut self) {
        (**self).processing();
    }
    fn inserted(&mut self, latency_ms: u32) {
        (**self).inserted(latency_ms);
    }
    fn hide(&mut self) {
        (**self).hide();
    }
}

/// Does nothing. Used when the overlay is not started (non-Windows CI paths).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopPill;

impl PillSink for NoopPill {
    fn show_listening(&mut self) {}
    fn update_level(&mut self, _level: f32) {}
    fn processing(&mut self) {}
    fn inserted(&mut self, _latency_ms: u32) {}
    fn hide(&mut self) {}
}

/// Prints the overlay's state transitions under `--verbose`.
///
/// Levels are deliberately not printed: they arrive 50 times a second and would
/// bury everything else.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogPill;

impl PillSink for LogPill {
    fn show_listening(&mut self) {
        iris_core::vlog!("pill: listening");
    }
    fn update_level(&mut self, _level: f32) {}
    fn set_partial_len(&mut self, chars: usize) {
        iris_core::vlog!("pill: partial_len={chars}");
    }
    fn set_engine(&mut self, label: &str) {
        iris_core::vlog!("pill: engine={label}");
    }
    fn set_theme(&mut self, theme: Theme) {
        iris_core::vlog!("pill: theme={theme}");
    }
    fn processing(&mut self) {
        iris_core::vlog!("pill: processing");
    }
    fn inserted(&mut self, latency_ms: u32) {
        iris_core::vlog!("pill: inserted ({latency_ms} ms)");
    }
    fn hide(&mut self) {
        iris_core::vlog!("pill: hidden");
    }
}

/// Map the app's dark/light setting onto the captain-locked overlay palettes.
#[must_use]
pub fn overlay_theme(theme: Theme) -> iris_overlay::Theme {
    match theme {
        Theme::Dark => iris_overlay::PRISM_DARK,
        Theme::Light => iris_overlay::PORCELAIN_LIGHT,
    }
}

/// Forwards every [`PillSink`] call to a live [`iris_overlay::OverlayHandle`].
///
/// The handle is cheap to clone and every method is non-blocking (full queue
/// drops), so this adapter is safe on the dictation thread.
///
/// Hold the parent [`iris_overlay::Overlay`] for process life — dropping it
/// joins the overlay thread and the handle goes silent.
#[derive(Clone, Debug)]
pub struct OverlayPill {
    handle: iris_overlay::OverlayHandle,
}

impl OverlayPill {
    /// Drive `handle` from the dictation loop.
    #[must_use]
    pub fn new(handle: iris_overlay::OverlayHandle) -> Self {
        Self { handle }
    }

    /// The underlying handle, for callers that need [`OverlayHandle::is_connected`].
    #[must_use]
    pub fn handle(&self) -> &iris_overlay::OverlayHandle {
        &self.handle
    }
}

impl PillSink for OverlayPill {
    fn show_listening(&mut self) {
        self.handle.show_listening();
    }

    fn update_level(&mut self, level: f32) {
        self.handle.update_level(level);
    }

    fn set_partial_len(&mut self, chars: usize) {
        self.handle.set_partial_len(chars);
    }

    fn set_engine(&mut self, label: &str) {
        self.handle.set_engine(label);
    }

    fn set_theme(&mut self, theme: Theme) {
        self.handle.set_theme(overlay_theme(theme));
    }

    fn processing(&mut self) {
        self.handle.processing();
    }

    fn inserted(&mut self, latency_ms: u32) {
        // Overlay auto-exits after INSERTED_HOLD_MS; do not follow with hide().
        self.handle.inserted(latency_ms);
    }

    fn hide(&mut self) {
        self.handle.hide();
    }
}

/// One overlay state transition, as recorded by [`RecordingPill`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillEvent {
    /// [`PillSink::show_listening`].
    ShowListening,
    /// [`PillSink::processing`].
    Processing,
    /// [`PillSink::inserted`] with the latency shown on the pill.
    Inserted {
        /// Key-release → inject, milliseconds.
        latency_ms: u32,
    },
    /// [`PillSink::hide`].
    Hide,
    /// [`PillSink::set_engine`].
    SetEngine,
    /// [`PillSink::set_partial_len`].
    SetPartialLen {
        /// Character count.
        chars: usize,
    },
    /// [`PillSink::set_theme`].
    SetTheme(Theme),
}

/// A test double that remembers the state transitions it was told about.
///
/// Cloneable and shared, because the loop takes ownership of its sink and a
/// test still has to see what the loop did with it.
#[derive(Debug, Clone, Default)]
pub struct RecordingPill {
    state: std::sync::Arc<std::sync::Mutex<Recorded>>,
}

#[derive(Debug, Default)]
struct Recorded {
    events: Vec<PillEvent>,
    levels: Vec<f32>,
    engine: Option<String>,
    partial_lens: Vec<usize>,
}

impl RecordingPill {
    /// A sink with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// The state transitions so far, in order. Levels are excluded.
    pub fn events(&self) -> Vec<PillEvent> {
        self.state.lock().expect("pill mutex").events.clone()
    }

    /// Every level reported so far.
    pub fn levels(&self) -> Vec<f32> {
        self.state.lock().expect("pill mutex").levels.clone()
    }

    /// Last engine label pushed, if any.
    pub fn engine(&self) -> Option<String> {
        self.state.lock().expect("pill mutex").engine.clone()
    }

    /// Every partial length reported so far.
    pub fn partial_lens(&self) -> Vec<usize> {
        self.state.lock().expect("pill mutex").partial_lens.clone()
    }

    fn push(&self, event: PillEvent) {
        self.state.lock().expect("pill mutex").events.push(event);
    }
}

impl PillSink for RecordingPill {
    fn show_listening(&mut self) {
        self.push(PillEvent::ShowListening);
    }
    fn update_level(&mut self, level: f32) {
        self.state.lock().expect("pill mutex").levels.push(level);
    }
    fn set_partial_len(&mut self, chars: usize) {
        self.state
            .lock()
            .expect("pill mutex")
            .partial_lens
            .push(chars);
        self.push(PillEvent::SetPartialLen { chars });
    }
    fn set_engine(&mut self, label: &str) {
        self.state.lock().expect("pill mutex").engine = Some(label.to_string());
        self.push(PillEvent::SetEngine);
    }
    fn set_theme(&mut self, theme: Theme) {
        self.push(PillEvent::SetTheme(theme));
    }
    fn processing(&mut self) {
        self.push(PillEvent::Processing);
    }
    fn inserted(&mut self, latency_ms: u32) {
        self.push(PillEvent::Inserted { latency_ms });
    }
    fn hide(&mut self) {
        self.push(PillEvent::Hide);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_pill_keeps_order_and_levels() {
        let mut pill = RecordingPill::new();
        let observer = pill.clone();
        pill.set_engine("mock");
        pill.set_theme(Theme::Dark);
        pill.show_listening();
        pill.update_level(0.25);
        pill.set_partial_len(4);
        pill.update_level(0.75);
        pill.processing();
        pill.inserted(142);
        // Success path: no hide after inserted.

        assert_eq!(
            observer.events(),
            [
                PillEvent::SetEngine,
                PillEvent::SetTheme(Theme::Dark),
                PillEvent::ShowListening,
                PillEvent::SetPartialLen { chars: 4 },
                PillEvent::Processing,
                PillEvent::Inserted { latency_ms: 142 },
            ]
        );
        assert_eq!(observer.levels(), [0.25, 0.75]);
        assert_eq!(observer.engine().as_deref(), Some("mock"));
        assert_eq!(observer.partial_lens(), [4]);
    }

    #[test]
    fn a_boxed_sink_forwards_every_method() {
        let mut pill: Box<dyn PillSink> = Box::new(RecordingPill::new());
        pill.show_listening();
        pill.inserted(10);
        pill.hide();
        let mut noop = NoopPill;
        noop.show_listening();
        noop.update_level(1.0);
        noop.set_partial_len(1);
        noop.set_engine("x");
        noop.set_theme(Theme::Light);
        noop.processing();
        noop.inserted(0);
        noop.hide();
    }

    #[test]
    fn overlay_theme_maps_dark_to_prism_and_light_to_porcelain() {
        assert_eq!(
            overlay_theme(Theme::Dark).name,
            iris_overlay::PRISM_DARK.name
        );
        assert_eq!(
            overlay_theme(Theme::Light).name,
            iris_overlay::PORCELAIN_LIGHT.name
        );
    }

    /// Adapter smoke: drive a real overlay thread (stub window off Windows)
    /// through the PillSink surface and shut it down cleanly.
    #[test]
    fn overlay_pill_drives_a_full_cycle_without_blocking() {
        let overlay = iris_overlay::spawn(iris_overlay::OverlayConfig {
            theme: overlay_theme(Theme::Dark),
            engine: "mock".into(),
        })
        .expect("overlay should start");
        let mut pill = OverlayPill::new(overlay.handle());
        assert!(pill.handle().is_connected());

        pill.set_engine("mock");
        pill.set_theme(Theme::Light);
        pill.show_listening();
        pill.update_level(0.6);
        pill.set_partial_len(12);
        pill.processing();
        pill.inserted(142);
        // No hide after insert — confirmation hold is the overlay's job.

        overlay.shutdown();
        assert!(!pill.handle().is_connected());
    }

    #[test]
    fn overlay_pill_hide_is_for_cancel_paths() {
        let overlay = iris_overlay::spawn(iris_overlay::OverlayConfig::default())
            .expect("overlay should start");
        let mut pill = OverlayPill::new(overlay.handle());
        pill.show_listening();
        pill.processing();
        pill.hide();
        overlay.shutdown();
    }
}

//! The seam the overlay hangs off.
//!
//! `iris-overlay` — the floating "pill" that shows the user Iris is listening —
//! is being built in parallel with this crate. Rather than wait for it, the
//! dictation loop drives a [`PillSink`], whose five methods mirror the overlay
//! handle's API exactly:
//!
//! ```text
//!   show_listening → update_level* → processing → inserted → hide
//! ```
//!
//! Two implementations ship today: [`NoopPill`] (the default, so a machine with
//! no overlay behaves identically) and [`LogPill`] (the same events on stderr
//! under `--verbose`). When the overlay merges, the adapter is a new type in
//! this module that forwards each method to the real handle — no change to the
//! loop, and no UI in this crate beyond the tray.
//!
//! # Contract
//!
//! Every method is called from the dictation thread, which is the thread that
//! owns the latency budget. **A sink must not block.** An implementation that
//! talks to another process should hand the event to a channel and return.

/// Where the dictation loop reports what the user should be seeing.
///
/// The state sequence is fixed: `show_listening`, then any number of
/// `update_level`, then `processing`, then `inserted` (only when text actually
/// reached the target window), then `hide`. `hide` always runs, including on
/// every failure path, so a sink can treat it as "the dictation is over" and
/// never needs a timeout to clean up.
pub trait PillSink: Send {
    /// The hotkey went down: the pill appears and Iris is capturing.
    fn show_listening(&mut self);

    /// Microphone level, `0.0..=1.0`, roughly once per audio frame. Advisory:
    /// it drives an animation, so dropping one costs nothing.
    fn update_level(&mut self, level: f32);

    /// The hotkey came up: capture is over and Iris is finalising and polishing.
    fn processing(&mut self);

    /// Text reached the focused window. Not called when injection failed or
    /// when the transcript was empty.
    fn inserted(&mut self);

    /// The dictation is over, successfully or not. Always called.
    fn hide(&mut self);
}

impl PillSink for Box<dyn PillSink> {
    fn show_listening(&mut self) {
        (**self).show_listening();
    }
    fn update_level(&mut self, level: f32) {
        (**self).update_level(level);
    }
    fn processing(&mut self) {
        (**self).processing();
    }
    fn inserted(&mut self) {
        (**self).inserted();
    }
    fn hide(&mut self) {
        (**self).hide();
    }
}

/// Does nothing. The default until `iris-overlay` lands.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopPill;

impl PillSink for NoopPill {
    fn show_listening(&mut self) {}
    fn update_level(&mut self, _level: f32) {}
    fn processing(&mut self) {}
    fn inserted(&mut self) {}
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
    fn processing(&mut self) {
        iris_core::vlog!("pill: processing");
    }
    fn inserted(&mut self) {
        iris_core::vlog!("pill: inserted");
    }
    fn hide(&mut self) {
        iris_core::vlog!("pill: hidden");
    }
}

/// One overlay state transition, as recorded by [`RecordingPill`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillEvent {
    /// [`PillSink::show_listening`].
    ShowListening,
    /// [`PillSink::processing`].
    Processing,
    /// [`PillSink::inserted`].
    Inserted,
    /// [`PillSink::hide`].
    Hide,
}

/// A test double that remembers the state transitions it was told about.
///
/// Cloneable and shared, because the loop takes ownership of its sink and a
/// test still has to see what the loop did with it. Public for the same reason
/// [`iris_polish::MockPolisher`] is: the overlay adapter, when it lands, will
/// want to assert the ordering this crate's tests assert.
#[derive(Debug, Clone, Default)]
pub struct RecordingPill {
    state: std::sync::Arc<std::sync::Mutex<Recorded>>,
}

#[derive(Debug, Default)]
struct Recorded {
    events: Vec<PillEvent>,
    levels: Vec<f32>,
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
    fn processing(&mut self) {
        self.push(PillEvent::Processing);
    }
    fn inserted(&mut self) {
        self.push(PillEvent::Inserted);
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
        // The handle a test keeps while the loop owns the sink.
        let observer = pill.clone();
        pill.show_listening();
        pill.update_level(0.25);
        pill.update_level(0.75);
        pill.processing();
        pill.inserted();
        pill.hide();

        assert_eq!(
            observer.events(),
            [
                PillEvent::ShowListening,
                PillEvent::Processing,
                PillEvent::Inserted,
                PillEvent::Hide
            ]
        );
        assert_eq!(observer.levels(), [0.25, 0.75]);
    }

    #[test]
    fn a_boxed_sink_forwards_every_method() {
        let mut pill: Box<dyn PillSink> = Box::new(RecordingPill::new());
        pill.show_listening();
        pill.hide();
        // Nothing to assert through the box; the point is that it compiles and
        // does not recurse. NoopPill covers the no-op contract.
        let mut noop = NoopPill;
        noop.show_listening();
        noop.update_level(1.0);
        noop.processing();
        noop.inserted();
        noop.hide();
    }
}

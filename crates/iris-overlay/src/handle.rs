//! The public control surface: [`Overlay`] owns the thread, [`OverlayHandle`]
//! is the cheap, cloneable thing you hand to whoever is producing events.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crossbeam_channel::{Sender, TrySendError};

use crate::state::Command;
use crate::theme::{Theme, PRISM_DARK};
use crate::window;
use crate::OverlayError;

/// How many commands may queue up before the oldest are dropped.
///
/// The audio thread emits a level roughly every 10–20 ms and must never block
/// on the UI. If the overlay thread ever stalls this long, the right behaviour
/// is to drop stale frames' worth of levels, not to back-pressure capture.
const QUEUE_DEPTH: usize = 256;

/// Flips the liveness flag when the overlay thread ends, however it ends:
/// living on the thread's stack, its `Drop` runs on normal return and on
/// panic unwind alike, and the panic still propagates to `join()`.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Startup options.
#[derive(Clone, Debug)]
pub struct OverlayConfig {
    /// Palette to start in. Defaults to [`PRISM_DARK`], the locked v1 default.
    pub theme: Theme,
    /// Initial engine label for the chip, e.g. `groq · whisper-large-v3-turbo · en`.
    /// May be empty; the chip is simply not drawn.
    pub engine: String,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            theme: PRISM_DARK,
            engine: String::new(),
        }
    }
}

/// A running overlay: one thread, one window, one state machine.
///
/// Dropping this asks the thread to stop and waits for it, so the window is
/// gone by the time the drop returns.
#[derive(Debug)]
pub struct Overlay {
    handle: OverlayHandle,
    thread: Option<JoinHandle<()>>,
}

impl Overlay {
    /// A handle for driving the pill from any thread. Cheap to clone.
    #[must_use]
    pub fn handle(&self) -> OverlayHandle {
        self.handle.clone()
    }

    /// Stop the overlay and wait for its thread.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        // A *blocking* send, unlike every other command. `OverlayHandle::send`
        // drops on a full queue, which is right for a level update and fatal
        // here: dropping the shutdown would leave the join below waiting on a
        // thread nobody ever asked to stop. This blocks for at most one frame,
        // because the loop drains the queue every iteration, and returns
        // immediately with an error if the thread is already gone.
        let _ = self.handle.tx.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the overlay on its own thread.
///
/// On Windows this creates the layered, click-through, never-activating pill
/// window. Everywhere else it runs the same state machine with no window at
/// all, so a caller can be built and tested on Linux — see
/// [`crate::HeadlessOverlay`] if you want the pixels too.
///
/// # Errors
///
/// Returns [`OverlayError::Platform`] if the window cannot be created.
pub fn spawn(config: OverlayConfig) -> Result<Overlay, OverlayError> {
    let (tx, rx) = crossbeam_channel::bounded(QUEUE_DEPTH);
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
    let alive = Arc::new(AtomicBool::new(true));

    let thread = {
        let alive = Arc::clone(&alive);
        std::thread::Builder::new()
            .name("iris-overlay".to_string())
            .spawn(move || {
                let _alive = AliveGuard(alive);
                window::run(rx, config, &ready_tx);
            })
            .map_err(|e| OverlayError::Platform(format!("could not start overlay thread: {e}")))?
    };

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Overlay {
            handle: OverlayHandle { tx, alive },
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(OverlayError::Platform(
                "overlay thread exited before it started".to_string(),
            ))
        }
    }
}

/// The contract the rest of Iris drives the pill through.
///
/// Every method is non-blocking and infallible from the caller's point of view:
/// if the overlay thread has gone away, or the queue is momentarily full, the
/// command is dropped. An overlay is a display, and a display that back-pressures
/// the audio pipeline is worse than one that skips a frame.
///
/// The expected call sequence for one utterance:
///
/// ```no_run
/// # use iris_overlay::{spawn, OverlayConfig};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let overlay = spawn(OverlayConfig::default())?;
/// let pill = overlay.handle();
///
/// pill.set_engine("groq · whisper-large-v3-turbo · en");
/// pill.show_listening();          // hotkey down
/// pill.update_level(0.62);        // ... per audio frame
/// pill.set_partial_len(31);       // ... per partial transcript
/// pill.processing();              // hotkey up
/// pill.inserted(142);             // text landed; hides itself after ~550 ms
/// # Ok(())
/// # }
/// ```
///
/// [`OverlayHandle::hide`] cancels from any state.
#[derive(Clone, Debug)]
pub struct OverlayHandle {
    tx: Sender<Command>,
    alive: Arc<AtomicBool>,
}

impl OverlayHandle {
    /// Show the pill and start listening.
    ///
    /// Called while already listening this does nothing — in particular it does
    /// not restart the timer. Called while the inserted confirmation is still
    /// on screen it starts a fresh utterance, so holding the hotkey again
    /// immediately does the obvious thing.
    pub fn show_listening(&self) {
        self.send(Command::ShowListening);
    }

    /// Report the current microphone level, 0.0–1.0.
    ///
    /// Anything outside that range, including NaN, is clamped rather than
    /// rejected: the waveform is a display, not a validator. Call it as often
    /// as audio frames arrive; the overlay smooths and resamples to its own
    /// frame rate.
    pub fn update_level(&self, level: f32) {
        self.send(Command::Level(level));
    }

    /// Report how many characters of partial transcript exist so far.
    ///
    /// Drives the partial ribbon along the bottom of the pill. Deliberately a
    /// length and not the text: the overlay never holds transcript content, so
    /// there is nothing on screen to read over the user's shoulder and nothing
    /// in a crash dump.
    pub fn set_partial_len(&self, chars: usize) {
        self.send(Command::PartialLen(chars));
    }

    /// Set the engine label shown in the chip below the pill while listening.
    pub fn set_engine(&self, label: impl Into<String>) {
        self.send(Command::Engine(label.into()));
    }

    /// The hotkey is up and the engine is finishing.
    ///
    /// Ignored unless the pill is currently listening.
    pub fn processing(&self) {
        self.send(Command::Processing);
    }

    /// Text landed in the target application.
    ///
    /// Shows the check and prints `latency_ms`, then takes itself off screen
    /// after the hold. Ignored if the pill is already hidden. May be called
    /// straight from `listening` when a streaming engine has already finalised.
    pub fn inserted(&self, latency_ms: u32) {
        self.send(Command::Inserted { latency_ms });
    }

    /// Hide the pill now, playing the exit animation. Valid from any state.
    pub fn hide(&self) {
        self.send(Command::Hide);
    }

    /// Switch palettes at runtime. Geometry and motion are unaffected.
    pub fn set_theme(&self, theme: Theme) {
        self.send(Command::SetTheme(Box::new(theme)));
    }

    /// Whether the overlay thread is still running.
    ///
    /// Reports the thread being gone for any reason, including a panic: the
    /// flag is flipped by a guard on the thread's stack, so it drops on
    /// unwind too. Useful for a supervisor that wants to notice the pill has
    /// died; the send methods do not need it, they simply drop commands.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Send a raw command. Returns whether it was queued.
    ///
    /// The typed methods above are the supported API; this exists for callers
    /// putting the overlay behind their own transport.
    pub fn send(&self, command: Command) -> bool {
        match self.tx.try_send(command) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Model;
    use crate::theme::PORCELAIN_LIGHT;

    /// A handle wired to a plain receiver, so the typed methods can be checked
    /// without starting a window.
    fn wired() -> (OverlayHandle, crossbeam_channel::Receiver<Command>) {
        let (tx, rx) = crossbeam_channel::bounded(QUEUE_DEPTH);
        let handle = OverlayHandle {
            tx,
            alive: Arc::new(AtomicBool::new(true)),
        };
        (handle, rx)
    }

    #[test]
    fn the_typed_methods_map_onto_commands() {
        let (h, rx) = wired();
        h.show_listening();
        h.update_level(0.5);
        h.set_partial_len(7);
        h.set_engine("groq");
        h.processing();
        h.inserted(142);
        h.hide();
        h.set_theme(PORCELAIN_LIGHT);

        assert!(matches!(rx.recv().unwrap(), Command::ShowListening));
        assert!(matches!(rx.recv().unwrap(), Command::Level(v) if (v - 0.5).abs() < 1e-6));
        assert!(matches!(rx.recv().unwrap(), Command::PartialLen(7)));
        assert!(matches!(rx.recv().unwrap(), Command::Engine(s) if s == "groq"));
        assert!(matches!(rx.recv().unwrap(), Command::Processing));
        assert!(matches!(
            rx.recv().unwrap(),
            Command::Inserted { latency_ms: 142 }
        ));
        assert!(matches!(rx.recv().unwrap(), Command::Hide));
        assert!(
            matches!(rx.recv().unwrap(), Command::SetTheme(t) if t.name == PORCELAIN_LIGHT.name)
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_dead_overlay_does_not_panic_the_producer() {
        let (h, rx) = wired();
        drop(rx);
        // Whatever the audio thread is mid-way through must keep running.
        h.show_listening();
        h.update_level(0.9);
        h.inserted(10);
        assert!(!h.send(Command::Hide));
    }

    #[test]
    fn a_full_queue_drops_rather_than_blocks() {
        let (h, _rx) = wired();
        for _ in 0..QUEUE_DEPTH {
            assert!(h.send(Command::Level(0.1)));
        }
        // The next one is dropped, and the call still returns immediately.
        assert!(!h.send(Command::Level(0.1)));
    }

    /// A real overlay, started and stopped. On Linux this exercises the stub
    /// loop; the shutdown path it is testing is shared with Windows.
    #[test]
    fn an_overlay_starts_runs_a_cycle_and_stops() {
        let overlay = spawn(OverlayConfig {
            theme: PRISM_DARK,
            engine: "mock".into(),
        })
        .expect("overlay should start");
        let pill = overlay.handle();
        assert!(pill.is_connected());

        pill.show_listening();
        pill.update_level(0.5);
        pill.processing();
        pill.inserted(142);

        overlay.shutdown();
        // The thread has been joined by now, so the liveness flag is settled.
        assert!(!pill.is_connected(), "overlay thread outlived its shutdown");
    }

    /// Shutting down while a producer is hammering the queue must still return.
    ///
    /// This is a liveness test, not a reproduction: it does not reliably fill
    /// the queue, because the loop here drains faster than a producer can fill
    /// it and never renders. The failure it guards against — `stop()` dropping
    /// its shutdown command on a full queue and then joining a thread that was
    /// never told to stop — needs a consumer slow enough to fall 256 commands
    /// behind, which only the real Windows loop can be. `Overlay::stop` is
    /// written to be correct by construction instead; see the comment there.
    #[test]
    fn shutdown_survives_a_producer_hammering_the_queue() {
        let overlay = spawn(OverlayConfig::default()).expect("overlay should start");
        let flooding = Arc::new(AtomicBool::new(true));

        let producer = {
            let pill = overlay.handle();
            let flooding = Arc::clone(&flooding);
            std::thread::spawn(move || {
                while flooding.load(Ordering::Relaxed) {
                    for _ in 0..QUEUE_DEPTH {
                        pill.update_level(0.5);
                    }
                }
            })
        };
        // Give the producer a head start on the loop.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            overlay.shutdown();
            let _ = done_tx.send(());
        });
        let stopped = done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .is_ok();

        flooding.store(false, Ordering::Relaxed);
        producer.join().unwrap();
        assert!(stopped, "shutdown deadlocked behind a full command queue");
    }

    /// A panic on the overlay thread must still flip the liveness flag: the
    /// guard drops during unwind, exactly as it sits on the stack of the
    /// thread `spawn` starts.
    #[test]
    fn a_panicking_overlay_thread_is_observed_as_disconnected() {
        let (h, _rx) = wired();
        let thread = {
            let alive = Arc::clone(&h.alive);
            std::thread::Builder::new()
                .name("iris-overlay-panics".to_string())
                .spawn(move || {
                    let _alive = AliveGuard(alive);
                    panic!("overlay thread died mid-frame");
                })
                .unwrap()
        };
        assert!(thread.join().is_err(), "the panic should still propagate");
        assert!(!h.is_connected(), "a panicked thread must read as dead");
    }

    #[test]
    fn the_handle_is_cheap_to_clone_and_usable_from_another_thread() {
        let (h, rx) = wired();
        let worker = h.clone();
        std::thread::spawn(move || {
            worker.show_listening();
            worker.update_level(0.4);
        })
        .join()
        .unwrap();
        assert!(matches!(rx.recv().unwrap(), Command::ShowListening));
        assert!(matches!(rx.recv().unwrap(), Command::Level(_)));
    }

    /// The handle is the contract a sibling worker is coding against, so this
    /// pins the exact sequence an utterance produces and what the model does
    /// with it.
    #[test]
    fn a_full_utterance_drives_the_model_end_to_end() {
        let (h, rx) = wired();
        let mut model = Model::new(PRISM_DARK);
        model.tick(0);

        h.set_engine("deepgram · nova-3 · en");
        h.show_listening();
        for i in 0..10 {
            h.update_level(0.1 * i as f32);
            h.set_partial_len(i * 4);
        }
        h.processing();
        h.inserted(142);

        let mut now = 0;
        while let Ok(cmd) = rx.try_recv() {
            assert!(model.apply(cmd));
            now += 16;
            model.tick(now);
        }
        assert_eq!(model.state(), crate::OverlayState::Inserted);
        assert_eq!(model.latency_ms(), Some(142));
        assert_eq!(model.engine(), "deepgram · nova-3 · en");
    }

    #[test]
    fn the_default_config_is_the_locked_dark_default() {
        assert_eq!(OverlayConfig::default().theme.name, PRISM_DARK.name);
        assert!(OverlayConfig::default().engine.is_empty());
    }
}

//! The platform layer, and the only part of this crate that is not portable.
//!
//! `run` owns the overlay thread: it drives the [`Model`], renders when there is
//! something to draw, and parks on the command channel when there is not. On
//! Windows it also owns the layered pill window; on every other platform it is
//! the same loop with no window, which is what keeps the crate buildable and
//! testable on Linux.

use std::time::Instant;

use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};

use crate::state::{Command, Model};

#[cfg(windows)]
mod win32;

#[cfg(not(windows))]
mod stub;

#[cfg(windows)]
pub(crate) use win32::run;

#[cfg(not(windows))]
pub(crate) use stub::run;

/// Most commands applied in one pass of the loop.
///
/// The bound is the point. A producer calling `update_level` in a tight loop —
/// which is what a busy audio thread looks like — can hand the overlay work
/// faster than it can drain it, and an unbounded "drain until empty" would then
/// never reach the tick and render below it: the pill would freeze *because*
/// there was too much to say about it. Higher than the queue depth so an
/// ordinary burst still clears in one pass.
const MAX_COMMANDS_PER_PASS: usize = 512;

/// Apply queued commands to `model` without blocking.
///
/// Returns `false` when the overlay should stop, either because it was told to
/// or because every sender has gone away.
fn drain(rx: &Receiver<Command>, model: &mut Model) -> bool {
    for _ in 0..MAX_COMMANDS_PER_PASS {
        match rx.try_recv() {
            Ok(command) => {
                if !model.apply(command) {
                    return false;
                }
            }
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
    true
}

/// Apply commands until `deadline`, then hand control back.
///
/// This is how the loop paces itself, and it is deliberately *not* "wait for a
/// command". A caller reports the microphone level every 10–20 ms, so a loop
/// that woke on each command would render at whatever rate the audio thread
/// happens to run at rather than at the frame rate. Waking early buys a display
/// nothing: the state is already in the model, and it will be on screen within
/// one frame either way.
///
/// Returns `false` when the overlay should stop.
fn drain_until(rx: &Receiver<Command>, model: &mut Model, deadline: Instant) -> bool {
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(command) => {
                if !model.apply(command) {
                    return false;
                }
            }
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PRISM_DARK;
    use crate::OverlayState;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn draining_applies_everything_queued() {
        let (tx, rx) = crossbeam_channel::bounded(16);
        let mut model = Model::new(PRISM_DARK);
        tx.send(Command::ShowListening).unwrap();
        tx.send(Command::Engine("groq".into())).unwrap();
        assert!(drain(&rx, &mut model));
        assert_eq!(model.state(), OverlayState::Listening);
        assert_eq!(model.engine(), "groq");
    }

    #[test]
    fn draining_stops_on_shutdown_and_on_disconnect() {
        let (tx, rx) = crossbeam_channel::bounded(16);
        let mut model = Model::new(PRISM_DARK);
        tx.send(Command::Shutdown).unwrap();
        assert!(!drain(&rx, &mut model));

        let (tx, rx) = crossbeam_channel::bounded::<Command>(16);
        drop(tx);
        assert!(!drain(&rx, &mut model));
    }

    /// A producer that never stops must not be able to hold the loop hostage:
    /// one pass has to end so the frame after it can tick and render.
    #[test]
    fn a_relentless_producer_cannot_starve_the_frame() {
        let (tx, rx) = crossbeam_channel::bounded(MAX_COMMANDS_PER_PASS * 4);
        for _ in 0..MAX_COMMANDS_PER_PASS * 3 {
            tx.send(Command::Level(0.5)).unwrap();
        }
        let mut model = Model::new(PRISM_DARK);
        assert!(drain(&rx, &mut model));
        assert!(
            !rx.is_empty(),
            "the drain consumed everything and never yielded to the renderer"
        );
        assert_eq!(rx.len(), MAX_COMMANDS_PER_PASS * 2);
    }

    #[test]
    fn draining_to_a_deadline_waits_for_it() {
        let (_tx, rx) = crossbeam_channel::bounded::<Command>(4);
        let mut model = Model::new(PRISM_DARK);
        let started = Instant::now();
        assert!(drain_until(
            &rx,
            &mut model,
            started + Duration::from_millis(30)
        ));
        assert!(
            started.elapsed() >= Duration::from_millis(25),
            "returned early"
        );
    }

    /// The pacing property: a producer sending faster than the frame rate must
    /// not drag the loop round faster than the frame rate.
    #[test]
    fn a_fast_producer_does_not_shorten_the_frame() {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let mut model = Model::new(PRISM_DARK);
        let flooding = Arc::new(AtomicBool::new(true));
        let producer = {
            let flooding = Arc::clone(&flooding);
            std::thread::spawn(move || {
                while flooding.load(Ordering::Relaxed) {
                    let _ = tx.try_send(Command::Level(0.5));
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let started = Instant::now();
        assert!(drain_until(
            &rx,
            &mut model,
            started + Duration::from_millis(40)
        ));
        let elapsed = started.elapsed();

        flooding.store(false, Ordering::Relaxed);
        producer.join().unwrap();
        assert!(
            elapsed >= Duration::from_millis(35),
            "frame cut short: {elapsed:?}"
        );
    }

    #[test]
    fn draining_to_a_deadline_stops_on_shutdown_and_disconnect() {
        let (tx, rx) = crossbeam_channel::bounded(4);
        let mut model = Model::new(PRISM_DARK);
        tx.send(Command::Shutdown).unwrap();
        assert!(!drain_until(
            &rx,
            &mut model,
            Instant::now() + Duration::from_secs(5)
        ));

        let (tx, rx) = crossbeam_channel::bounded::<Command>(4);
        drop(tx);
        assert!(!drain_until(
            &rx,
            &mut model,
            Instant::now() + Duration::from_secs(5)
        ));
    }
}

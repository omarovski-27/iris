//! The non-Windows overlay loop: the real state machine, no window.
//!
//! This exists so the crate compiles and its consumers can be developed and
//! tested on Linux. It is not a second implementation of the pill — it does not
//! render — so an app wired against [`crate::OverlayHandle`] behaves correctly
//! here (commands are accepted, states advance, `inserted` still auto-hides)
//! without anything appearing on screen.
//!
//! To actually *look* at the pill from Linux, use [`crate::HeadlessOverlay`] or
//! `cargo run --example pill-demo -- --filmstrip <dir>`.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use crate::handle::OverlayConfig;
use crate::motion::{FRAME_INTERVAL_MS, IDLE_POLL_MS};
use crate::state::{Command, Model};
use crate::OverlayError;

pub(crate) fn run(
    rx: Receiver<Command>,
    config: OverlayConfig,
    ready: &Sender<Result<(), OverlayError>>,
) {
    let mut model = Model::new(config.theme);
    if !config.engine.is_empty() {
        model.apply(Command::Engine(config.engine));
    }
    let _ = ready.send(Ok(()));

    let start = Instant::now();
    loop {
        if !super::drain(&rx, &mut model) {
            return;
        }

        model.tick(elapsed_ms(start));

        // Nothing to animate: park for longer rather than spinning.
        let wait = if model.is_idle() {
            IDLE_POLL_MS
        } else {
            FRAME_INTERVAL_MS
        };
        if !super::drain_until(
            &rx,
            &mut model,
            Instant::now() + Duration::from_millis(wait),
        ) {
            return;
        }
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PRISM_DARK;

    #[test]
    fn the_loop_starts_signals_ready_and_stops_on_shutdown() {
        let (tx, rx) = crossbeam_channel::bounded(16);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            run(
                rx,
                OverlayConfig {
                    theme: PRISM_DARK,
                    engine: "mock".into(),
                },
                &ready_tx,
            );
        });

        assert!(ready_rx.recv().unwrap().is_ok());
        tx.send(Command::ShowListening).unwrap();
        tx.send(Command::Inserted { latency_ms: 12 }).unwrap();
        tx.send(Command::Shutdown).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn dropping_the_sender_stops_the_loop() {
        let (tx, rx) = crossbeam_channel::bounded(16);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || run(rx, OverlayConfig::default(), &ready_tx));
        assert!(ready_rx.recv().unwrap().is_ok());
        drop(tx);
        worker.join().unwrap();
    }
}

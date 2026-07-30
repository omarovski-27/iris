//! A three-line logger.
//!
//! The spike deliberately avoids a logging framework: the only thing we need is
//! a `--verbose` switch that turns on diagnostics on stderr, and stdout must
//! stay clean for the latency report.

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Print a diagnostic line to stderr when `--verbose` is on.
#[macro_export]
macro_rules! vlog {
    ($($arg:tt)*) => {
        if $crate::log::verbose() {
            eprintln!("[iris] {}", format_args!($($arg)*));
        }
    };
}

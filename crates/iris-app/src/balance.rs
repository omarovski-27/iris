//! Watching the optional Deepgram account balance in the background, and
//! warning once before it runs out.
//!
//! This is entirely off unless the user opts in with a `deepgram_management`
//! key (`crate::config::Keys::deepgram_management`) — a credential distinct
//! from the transcription key, see that field's doc comment and
//! `iris_core::engine::deepgram_balance`'s module docs for why. With no key,
//! [`BalanceMonitor::spawn`] starts no thread at all: [`BalanceMonitor::view`]
//! always reports `configured: false`, and nothing here ever runs.
//!
//! # Why a background thread, not a call from the window
//!
//! [`iris_core::engine::deepgram_balance::fetch`] blocks on a real HTTP round
//! trip — wrong to run on the settings window's own frame loop (it would
//! freeze the UI) and, more importantly, this must never share a thread with
//! anything the dictation loop touches: a billing lookup hanging must never
//! delay or block a dictation. So [`BalanceMonitor`] owns one dedicated
//! thread, independent of [`crate::App`] and the settings window alike,
//! mirroring `tray::spawn`/`iris_overlay::spawn`/`window::spawn` — started
//! once in `main` and outliving every window open/close cycle.
//!
//! # Scheduling: on demand, never per dictation
//!
//! The thread fetches once immediately (so the warning below can fire even
//! for a user who never opens Settings) and then re-fetches on
//! [`CHECK_INTERVAL`] — a billing balance for one person's own usage moves
//! slowly, so there is nothing to gain from checking more often, only load on
//! Deepgram's Management API for no benefit. [`BalanceMonitor::request_refresh`]
//! is the other trigger, for a "Refresh" button in Settings; either way the
//! fetch happens on this thread, never inline with a dictation.
//!
//! A failed fetch is deliberately quiet: [`BalanceView::check_failed`] is set
//! and the last known amount (if any) is left in place, but nothing pops a
//! dialog and nothing changes about transcription — see
//! `iris_core::engine::deepgram_balance::BalanceError`'s doc comment for why
//! a billing lookup failing must never look like a transcription failure.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use iris_core::engine::deepgram_balance::{self, Balance};

use crate::notify::FailureNotice;

/// How often the background thread re-checks the balance on its own, absent
/// a manual refresh. A personal Deepgram balance only moves when the user
/// dictates or tops up — both rare compared to how often Iris itself might be
/// running — so there is no latency or freshness reason to poll faster; this
/// is chosen to be "slow enough that nobody would call it a hot path; fast
/// enough that the warning below is not stale for days."
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Below this remaining balance, [`FailureNotice::low_balance`] fires once
/// (until the balance recovers above it — see [`run`]).
///
/// Chosen from Deepgram's own published Nova-3 streaming price of
/// $0.0048/minute (<https://deepgram.com/pricing>, checked 2026-08-13):
/// $5.00 buys upwards of 17 hours of streaming dictation, which for a single
/// person's own usage is comfortably more than a day or two of typical use —
/// enough runway to notice the warning and top up before a dictation is ever
/// the thing that fails. Not a round number chosen for its own sake: it is
/// sized against real usage, the same way the engines' own timeout constants
/// are reasoned from measurement rather than picked arbitrarily.
pub const LOW_BALANCE_THRESHOLD_USD: f64 = 5.0;

/// What the Settings window reads. Cloned out of the shared state on every
/// frame that asks — cheap, since this is a handful of scalars, not the
/// balance history.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BalanceView {
    /// Whether a `deepgram_management` key is configured at all. `false`
    /// means every other field is meaningless — the Settings view should
    /// show neither a balance nor an error, just how to opt in.
    pub configured: bool,
    /// The most recently *successfully* fetched balance, in `units`. `None`
    /// until the first successful fetch, which may be a moment after startup
    /// — the view should not read a `None` here as an error unless
    /// `check_failed` also says so.
    pub amount: Option<f64>,
    /// The currency of `amount` (Deepgram's Management API always reports
    /// `"USD"` in its documented examples). Empty exactly when `amount` is.
    pub units: String,
    /// When the most recent fetch attempt (successful or not) finished, RFC
    /// 3339 UTC — the same stamp shape `crate::history` writes, so the
    /// Settings view can reuse `window::ui::history_tab`'s existing
    /// local-time formatter. `None` before the first attempt completes.
    pub checked_at: Option<String>,
    /// Whether the most recent fetch attempt failed. `amount`/`units` still
    /// hold the last successful reading, if there was one — a transient
    /// failure must not make a known balance disappear, only mark it as
    /// possibly stale.
    pub check_failed: bool,
}

struct Shared {
    view: BalanceView,
}

/// Owns the background balance-check thread. See the module docs.
pub struct BalanceMonitor {
    shared: Arc<Mutex<Shared>>,
    /// `None` exactly when no management key was configured — the thread was
    /// never started, so there is nothing to wake.
    refresh_tx: Option<Sender<()>>,
}

impl BalanceMonitor {
    /// Start the background thread, or nothing at all if `management_key` is
    /// `None`. `notice` is the same [`FailureNotice`] the resident loop
    /// already uses for a failed dictation — see
    /// [`FailureNotice::low_balance`].
    #[must_use]
    pub fn spawn(management_key: Option<String>, notice: Arc<dyn FailureNotice>) -> Self {
        let configured = management_key.is_some();
        let shared = Arc::new(Mutex::new(Shared {
            view: BalanceView {
                configured,
                ..BalanceView::default()
            },
        }));

        let refresh_tx = management_key.map(|key| {
            let (tx, rx) = crossbeam_channel::unbounded();
            let thread_shared = Arc::clone(&shared);
            // Best-effort like every other background thread this crate
            // spawns (tray, overlay, window): a thread that fails to start
            // just leaves the feature looking unconfigured rather than
            // taking the app down.
            if let Err(e) = std::thread::Builder::new()
                .name("iris-balance".into())
                .spawn(move || {
                    run(
                        &thread_shared,
                        &rx,
                        notice.as_ref(),
                        deepgram_balance::fetch,
                        &key,
                    )
                })
            {
                eprintln!("  balance monitor unavailable: {e:#}");
            }
            tx
        });

        Self { shared, refresh_tx }
    }

    /// The current view, for the Settings tab to render.
    #[must_use]
    pub fn view(&self) -> BalanceView {
        self.shared.lock().expect("balance mutex").view.clone()
    }

    /// Ask the background thread to check again now, instead of waiting for
    /// [`CHECK_INTERVAL`]. A no-op when no key is configured.
    pub fn request_refresh(&self) {
        if let Some(tx) = &self.refresh_tx {
            let _ = tx.send(());
        }
    }
}

/// The background thread body. Fetches once immediately, then again on every
/// [`CHECK_INTERVAL`] tick or [`BalanceMonitor::request_refresh`] call, until
/// `refresh_rx` disconnects (the [`BalanceMonitor`] was dropped).
///
/// `fetch` is [`deepgram_balance::fetch`] in production and a stand-in in
/// tests — the one seam this function needs to make the warn-once-per-dip
/// state machine testable without a real network call.
fn run(
    shared: &Mutex<Shared>,
    refresh_rx: &Receiver<()>,
    notice: &dyn FailureNotice,
    fetch: impl Fn(&str) -> Result<Balance, deepgram_balance::BalanceError>,
    key: &str,
) {
    // Tracks whether the low-balance warning has already fired for the
    // *current* dip below the threshold, so a balance that stays low across
    // many checks warns once per dip rather than every `CHECK_INTERVAL` —
    // see `FailureNotice::low_balance`'s doc comment for why repeating it
    // would be the wrong trade. Reset the moment the balance recovers above
    // the threshold, so a later dip (a fresh top-up spent back down) warns
    // again. In-memory only, not persisted: a restart may re-warn once for
    // an unresolved low balance, which is a fair trade against the
    // complexity of persisting it for what is, in practice, a rare state.
    let mut warned = false;

    loop {
        let outcome = fetch(key);
        {
            let mut guard = shared.lock().expect("balance mutex");
            match &outcome {
                Ok(balance) => {
                    guard.view.amount = Some(balance.amount);
                    guard.view.units = balance.units.clone();
                    guard.view.check_failed = false;
                }
                Err(_) => guard.view.check_failed = true,
            }
            guard.view.checked_at = Some(now_rfc3339());
        }

        if let Ok(Balance { amount, units }) = &outcome {
            if *amount <= LOW_BALANCE_THRESHOLD_USD {
                if !warned {
                    warned = true;
                    notice.low_balance(&low_balance_message(*amount, units));
                }
            } else {
                warned = false;
            }
        }

        match refresh_rx.recv_timeout(CHECK_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// The message [`FailureNotice::low_balance`] shows, in the same actionable
/// style `iris_core::engine::FailureCause::message` already uses for a
/// failed dictation — name the account state and the fix, nothing vaguer.
fn low_balance_message(amount: f64, units: &str) -> String {
    format!(
        "Your Deepgram balance is {} — top up at {} to keep dictating without interruption.",
        format_amount(amount, units),
        deepgram_balance::CONSOLE_URL,
    )
}

/// `$4.32` for USD (every documented Deepgram example), `4.32 XYZ` for
/// anything else — shared between the warning message above and the
/// Settings tab's own readout so the two never drift apart on formatting.
pub(crate) fn format_amount(amount: f64, units: &str) -> String {
    if units.eq_ignore_ascii_case("usd") {
        format!("${amount:.2}")
    } else {
        format!("{amount:.2} {units}")
    }
}

/// The current time, RFC 3339 in UTC — identical in shape and reasoning to
/// `crate::history`'s own `timestamp()` (not reused directly: that one is
/// private to a module with no reason to expose it, and three lines is not
/// worth a shared helper for). UTC because a background thread has no safe
/// way to ask for the local offset — see `crate::history`'s module docs.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::RecordingFailureNotice;

    #[test]
    fn an_unconfigured_monitor_starts_no_thread_and_reports_unconfigured() {
        let monitor = BalanceMonitor::spawn(None, Arc::new(RecordingFailureNotice::new()));
        let view = monitor.view();
        assert!(!view.configured);
        assert_eq!(view.amount, None);
        assert_eq!(view.checked_at, None);
        // A no-op, not a panic or a hang.
        monitor.request_refresh();
    }

    #[test]
    fn format_amount_uses_a_dollar_sign_only_for_usd() {
        assert_eq!(format_amount(4.5, "USD"), "$4.50");
        assert_eq!(format_amount(4.5, "usd"), "$4.50");
        assert_eq!(format_amount(4.5, "EUR"), "4.50 EUR");
    }

    #[test]
    fn low_balance_message_names_the_amount_and_the_console() {
        let msg = low_balance_message(1.23, "USD");
        assert!(msg.contains("$1.23"), "{msg}");
        assert!(msg.contains("console.deepgram.com"), "{msg}");
    }

    /// Drives the real `run` loop with an injected fetch, so the warn-once-
    /// per-dip state machine is exercised directly rather than re-implemented
    /// in the test. Above threshold, then below (warns), still below (must
    /// not warn again), back above (resets), below again (warns again).
    #[test]
    fn run_warns_once_per_dip_and_again_after_a_recovery() {
        let shared = Mutex::new(Shared {
            view: BalanceView::default(),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let notice = RecordingFailureNotice::new();

        let amounts = std::sync::Mutex::new(vec![10.0, 1.0, 1.0, 10.0, 1.0].into_iter());
        let fetch = |_: &str| -> Result<Balance, deepgram_balance::BalanceError> {
            Ok(Balance {
                amount: amounts.lock().unwrap().next().unwrap(),
                units: "USD".into(),
            })
        };

        // Four more fetches after the first, then disconnect so `run`
        // returns instead of blocking forever on a fifth wait.
        for _ in 0..4 {
            tx.send(()).unwrap();
        }
        drop(tx);

        run(&shared, &rx, &notice, fetch, "irrelevant-in-this-test");

        assert_eq!(notice.low_balance_calls().len(), 2);
        // The last fetch (1.0) is what the view should end up holding.
        assert_eq!(shared.lock().unwrap().view.amount, Some(1.0));
    }
}

//! Reporting an injection failure somewhere the user will actually see it,
//! behind a trait for the same reason [`crate::inject::Injector`] is one.
//!
//! # Why this is a trait
//!
//! `iris` is a GUI-subsystem binary (see `main.rs`'s `windows_subsystem`
//! attribute), so a double-click, the Start Menu shortcut and the Startup
//! shortcut all give it no console at all. Before that change, a failed
//! injection printed the transcript itself to the console when history
//! logging was off — the console was the only place those words could still
//! be found. After it, that print reaches nowhere: a real dictation could go
//! silently missing on the one launch path most users actually take.
//!
//! [`SystemFailureNotice`] is the fix, and — like [`crate::inject::SystemInjector`]
//! — it is constructed only in `main`, real clipboard write and real dialog
//! and all. Every test, `--dry-run`, `--demo-dictation` and `--speak-wav` use
//! [`RecordingFailureNotice`] or [`NoopFailureNotice`] instead, so nothing
//! outside `main` can ever touch the real clipboard or pop a real dialog.

/// Told about a failed injection so it can put the words, and the failure
/// itself, somewhere the user will find them.
pub trait FailureNotice: Send + Sync {
    /// `text` is what would have been typed; `error` is why it was not.
    /// `history_enabled` says whether `text` is already durably recoverable
    /// from the session log — an implementation must never write `text` to
    /// disk itself when this is `false`, since the user turned history off
    /// deliberately and a silent write on top of that choice would trade a
    /// data-loss bug for a privacy one. Called once per failed injection, on
    /// the dictation thread, after `App::deliver` gives up.
    fn injection_failed(&self, text: &str, error: &str, history_enabled: bool);

    /// Told that a dictation produced no words at all because the engine
    /// either never reached a real connection to the transcription service
    /// (`iris_core::dictation::DictationOutcome::never_connected`) or the
    /// provider itself rejected the request for a specific, classified
    /// reason (`DictationOutcome::failure_cause`) — a bad key, an exhausted
    /// balance, a rate limit, an unreachable network, a timeout. `error` is
    /// already the true, specific story in every case — see
    /// `iris_core::engine::FailureCause::message` — so an implementation
    /// should show it as-is rather than wrapping it in an assumption (e.g.
    /// "check your internet connection") that may be wrong for the actual
    /// cause. There is no text to preserve here, unlike
    /// [`FailureNotice::injection_failed`]: nothing was ever transcribed.
    /// Called once per such dictation, on the dictation thread, from
    /// `App::failed`.
    fn connection_failed(&self, error: &str);
}

/// The real thing: the clipboard when history is off, and a visible dialog
/// always. **Never construct this in a test.** See the module docs.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemFailureNotice;

#[cfg(windows)]
impl FailureNotice for SystemFailureNotice {
    fn injection_failed(&self, text: &str, error: &str, history_enabled: bool) {
        // The clipboard write happens first: if it fails too, the dialog
        // below still shows and still names the error, but must not claim
        // the words are somewhere they are not.
        let clipboard_result = if history_enabled {
            None
        } else {
            Some(iris_core::inject::copy_to_clipboard(text))
        };

        let where_text = match (history_enabled, &clipboard_result) {
            (true, _) => "It's in your dictation history.".to_string(),
            (false, Some(Ok(()))) => "It's on your clipboard.".to_string(),
            (false, Some(Err(clip_err))) => {
                format!("It could not be copied to your clipboard either: {clip_err:#}")
            }
            (false, None) => unreachable!("clipboard_result is Some whenever history is off"),
        };

        crate::dialog::show(
            "Iris could not type your dictation",
            &format!("{where_text}\n\n{error}"),
        );
    }

    fn connection_failed(&self, error: &str) {
        // No blanket "check your internet connection" here: `error` already
        // names the true, specific cause (an exhausted balance, a rejected
        // key, a rate limit, or — when it really is a network problem — that
        // too), and prepending an assumption that might be wrong is exactly
        // the bug this exists to fix. See `FailureNotice::connection_failed`'s
        // doc comment.
        crate::dialog::show(
            "Iris could not reach the transcription service",
            &format!("Nothing was sent.\n\n{error}"),
        );
    }
}

/// Remembers every call, for tests. Never touches a real clipboard or shows a
/// real dialog — see the module docs for why nothing but `main` may.
#[derive(Debug, Default)]
pub struct RecordingFailureNotice {
    calls: std::sync::Mutex<Vec<(String, String, bool)>>,
    connection_calls: std::sync::Mutex<Vec<String>>,
}

impl RecordingFailureNotice {
    /// A fresh notifier with no calls recorded yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every call so far, in order: `(text, error, history_enabled)`.
    pub fn calls(&self) -> Vec<(String, String, bool)> {
        self.calls.lock().expect("notice mutex").clone()
    }

    /// Every [`FailureNotice::connection_failed`] call so far, in order.
    pub fn connection_calls(&self) -> Vec<String> {
        self.connection_calls.lock().expect("notice mutex").clone()
    }
}

impl FailureNotice for RecordingFailureNotice {
    fn injection_failed(&self, text: &str, error: &str, history_enabled: bool) {
        self.calls.lock().expect("notice mutex").push((
            text.to_string(),
            error.to_string(),
            history_enabled,
        ));
    }

    fn connection_failed(&self, error: &str) {
        self.connection_calls
            .lock()
            .expect("notice mutex")
            .push(error.to_string());
    }
}

/// Does nothing. [`crate::App::new`]'s default until `main` opts the
/// resident loop into [`SystemFailureNotice`], and what the `--dry-run` /
/// `--demo-dictation` / `--speak-wav` dev paths keep: a failure there is
/// either deliberately simulated or has no real desktop to notify.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopFailureNotice;

impl FailureNotice for NoopFailureNotice {
    fn injection_failed(&self, _text: &str, _error: &str, _history_enabled: bool) {}
    fn connection_failed(&self, _error: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_notice_collects_every_call() {
        let notice = RecordingFailureNotice::new();
        notice.injection_failed("hello", "boom", false);
        notice.injection_failed("world", "boom again", true);

        assert_eq!(
            notice.calls(),
            [
                ("hello".to_string(), "boom".to_string(), false),
                ("world".to_string(), "boom again".to_string(), true),
            ]
        );
    }

    #[test]
    fn recording_notice_collects_connection_failures_separately_from_injection_failures() {
        let notice = RecordingFailureNotice::new();
        notice.connection_failed("could not resolve host");
        notice.injection_failed("hello", "boom", false);

        assert_eq!(notice.connection_calls(), ["could not resolve host"]);
        assert_eq!(
            notice.calls(),
            [("hello".to_string(), "boom".to_string(), false)]
        );
    }

    #[test]
    fn noop_notice_does_not_panic() {
        NoopFailureNotice.injection_failed("hello", "boom", false);
        NoopFailureNotice.connection_failed("boom");
    }
}

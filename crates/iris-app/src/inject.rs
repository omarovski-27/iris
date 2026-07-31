//! Delivering the finished text, behind a trait so tests never touch the OS.
//!
//! # Why this is a trait
//!
//! Windows delivers synthetic keystrokes to the *input desktop* — the one the
//! user is looking at. There is no sandbox: an automated injection test types
//! into whoever is using the machine, and it has already disrupted real work
//! once during this project's development (see `CLAUDE.md`).
//!
//! So [`SystemInjector`] — the only implementation that reaches
//! [`iris_core::inject`] — is constructed in `main` and nowhere else. Every
//! test, and the `--dry-run` mode, uses [`RecordingInjector`] or
//! [`DryRunInjector`]. The trait exists to make that separation structural
//! rather than a rule someone has to remember.

use std::sync::Mutex;

use anyhow::Result;
use iris_core::inject::Method;

/// Something that can put text into the focused window.
pub trait Injector: Send + Sync {
    /// A short name for logs and the startup banner.
    fn name(&self) -> &'static str;

    /// Deliver `text`. Called once per dictation, on the dictation thread,
    /// after the transcript has been polished.
    fn inject(&self, text: &str) -> Result<()>;
}

/// The real thing: synthetic keystrokes or a clipboard paste, via
/// [`iris_core::inject`].
///
/// **Never construct this in a test.** See the module docs.
#[derive(Debug, Clone, Copy)]
pub struct SystemInjector {
    method: Method,
}

impl SystemInjector {
    /// Deliver text using `method`.
    pub fn new(method: Method) -> Self {
        Self { method }
    }
}

impl Injector for SystemInjector {
    fn name(&self) -> &'static str {
        match self.method {
            Method::SendInput => "sendinput",
            Method::Clipboard => "clipboard",
        }
    }

    fn inject(&self, text: &str) -> Result<()> {
        iris_core::inject::inject(text, self.method)
    }
}

/// Prints what would have been typed. The safe way to exercise the whole loop
/// on a machine someone is using.
#[derive(Debug, Clone, Copy, Default)]
pub struct DryRunInjector;

impl Injector for DryRunInjector {
    fn name(&self) -> &'static str {
        "dry-run"
    }

    fn inject(&self, text: &str) -> Result<()> {
        println!("  (dry run) would inject: {text:?}");
        Ok(())
    }
}

/// Remembers what it was asked to inject, and can be told to fail.
///
/// The failure mode matters as much as the success one: when injection fails
/// the transcript must still reach the session log, because that log is the
/// user's only way to recover the words they just spoke.
#[derive(Debug, Default)]
pub struct RecordingInjector {
    inserted: Mutex<Vec<String>>,
    failure: Option<String>,
}

impl RecordingInjector {
    /// An injector that accepts everything.
    pub fn new() -> Self {
        Self::default()
    }

    /// An injector that fails every call with `message`.
    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            inserted: Mutex::new(Vec::new()),
            failure: Some(message.into()),
        }
    }

    /// Everything injected so far, in order.
    pub fn inserted(&self) -> Vec<String> {
        self.inserted.lock().expect("injector mutex").clone()
    }
}

impl Injector for RecordingInjector {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn inject(&self, text: &str) -> Result<()> {
        if let Some(message) = &self.failure {
            anyhow::bail!("{message}");
        }
        self.inserted
            .lock()
            .expect("injector mutex")
            .push(text.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_injector_collects_text() {
        let injector = RecordingInjector::new();
        injector.inject("hello ").unwrap();
        injector.inject("world").unwrap();
        assert_eq!(injector.inserted(), ["hello ", "world"]);
    }

    #[test]
    fn failing_injector_reports_its_message_and_records_nothing() {
        let injector = RecordingInjector::failing("the focused window is elevated");
        let err = injector.inject("hello").unwrap_err().to_string();
        assert!(err.contains("elevated"), "{err}");
        assert!(injector.inserted().is_empty());
    }
}

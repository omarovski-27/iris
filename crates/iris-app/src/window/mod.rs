//! The Settings window: history, settings and insights, opened from the
//! tray's `Settings` item.
//!
//! # Toolkit choice
//!
//! This crate otherwise has no windowing framework by design — see
//! `tray.rs`'s module docs — but a real, readable, searchable window needs
//! one. The candidates and why `egui`/`eframe` won:
//!
//! * **`egui` + `eframe`** (chosen). Pure Rust, immediate mode, no C
//!   toolchain: a scratch crate pulling in the full `eframe` 0.32 stack
//!   (`egui`, `egui-winit`, `egui_glow`, `glutin`, `accesskit`, `arboard`)
//!   cross-compiled *and linked* to a real `x86_64-pc-windows-gnu` `.exe`
//!   with nothing beyond the mingw-w64 linker this project already requires
//!   — no cmake, no nasm, no libclang. It ships as one statically-linked
//!   `.exe`, matching how Iris already ships, and gives real widgets
//!   (`TextEdit`, `ScrollArea`, `ComboBox`, clipboard) plus a `Painter`
//!   escape hatch for porting `iris-overlay`'s `Rgba` tokens directly. `egui`
//!   itself (not `eframe`) has no OS dependency at all, so the entire view —
//!   [`ui`], `state`, `insights`, `search`, `egui_theme` — is a plain
//!   dependency (see `Cargo.toml`) that type-checks and unit-tests on Linux;
//!   only `shell` (the native window/GL bootstrap) needs `eframe`, and it
//!   is `[target.'cfg(windows)'.dependencies]`-gated the same way
//!   `tray-icon` already is, so nothing GUI-shaped compiles on a non-Windows
//!   `cargo check --workspace`.
//! * **`wry` + `tao`** (WebView2 shell). Also cross-compiles clean to
//!   `x86_64-pc-windows-gnu` with no C toolchain (proved the same way) — but
//!   it needs the Edge WebView2 runtime plus a `WebView2Loader.dll` shipped
//!   next to `iris.exe`, an external dependency this product does not
//!   otherwise have, and it would put HTML/CSS/JS in a Rust-only codebase for
//!   one window. Rejected on those grounds, not the cross-compile.
//! * **A retained Win32-controls toolkit** (e.g. `native-windows-gui`).
//!   Definitely cross-compiles (it is just `user32`/`comctl32` FFI) but native
//!   common controls are flat and opaque; reaching the glassy Prism look
//!   would mean owner-drawing nearly everything, at which point it is doing
//!   the same custom-rendering work as the option below with a thinner,
//!   less-tested widget layer glued on top.
//! * **Extending `iris-overlay`'s hand-rolled Win32 + tiny-skia renderer.**
//!   Guaranteed to cross-compile — it already ships — but that renderer has
//!   no input handling at all today (the pill "never activates, never
//!   hit-tests"): a search box, a scrollable list and dropdowns would mean
//!   hand-building text-cursor/selection, scroll physics and hit-testing from
//!   scratch. `egui` gives all of that, tested, for free.
//!
//! # Design
//!
//! `egui_theme` maps `iris_overlay::theme`'s `PRISM_DARK`/`PORCELAIN_LIGHT`
//! tokens onto egui, and [`ui::chrome`] paints the background wash and the
//! spectrum accent bar from the same tokens — the window and the pill are
//! meant to read as one product, not two.
//!
//! # Lifecycle
//!
//! [`spawn`] starts one OS thread that lives for the process, mirroring
//! `tray::spawn` and `iris_overlay::spawn`. The thread waits for an
//! [`WindowSink::open`] signal, runs the window until it is closed, then goes
//! back to waiting — so opening, closing and reopening never spawns a second
//! window, and closing it never touches the dictation loop, which owns its
//! own thread throughout.

mod egui_theme;
mod insights;
mod search;
mod state;
pub mod ui;

#[cfg(windows)]
mod shell;

pub use insights::{DayWindow, Insights, Ranked};
pub use state::{
    Env, InForce, RestartPending, Status, StatusLevel, Tab, WindowState, HISTORY_PAGE,
    REFRESH_INTERVAL,
};

/// What the dictation loop asks the settings window to do.
///
/// One method, because that is the whole interaction the loop initiates —
/// everything else (reading history, changing a setting) flows the other
/// way, straight from the window thread onto the same command channel the
/// tray uses; see the `state` module docs.
pub trait WindowSink: Send {
    /// Open the window, or bring it to the front if it is already open.
    fn open(&self);
}

/// Does nothing. The default until [`App::with_window`](crate::App::with_window)
/// gives the loop a real one, and the whole of it on a platform/build with no
/// window (tests, non-Windows).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopWindow;

impl WindowSink for NoopWindow {
    fn open(&self) {}
}

/// A test double that counts [`WindowSink::open`] calls.
///
/// Cloneable and shared, because the loop takes ownership of its window sink
/// and a test still has to see what the loop did with it — the same shape as
/// [`crate::pill::RecordingPill`].
#[derive(Debug, Clone, Default)]
pub struct RecordingWindow {
    opens: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RecordingWindow {
    /// A sink that has not been opened.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times [`WindowSink::open`] was called.
    #[must_use]
    pub fn opens(&self) -> usize {
        self.opens.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl WindowSink for RecordingWindow {
    fn open(&self) {
        self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The two settings that cannot change without a restart, as `main` saw them
/// at launch — both what the process actually runs on and what the file said.
///
/// Each is read exactly once, before this window can exist: the hotkey when
/// the hook is installed, `overlay_enabled` when the overlay is (or is not)
/// spawned. So the window is handed the running values, and names those
/// rather than the saved ones — without that the sidebar would confidently
/// name a key that does nothing.
///
/// The file values are here because "running ≠ saved" alone does not mean a
/// change is pending. A run-only CLI override (`iris --hotkey f9`) diverges
/// from the file by design and never converges, and flagging that as pending
/// would nag about an edit the user never made. Nor is "the file moved since
/// launch" enough on its own — it also fires when the user saves the value
/// that override already put in force. Both values are paired into an
/// [`InForce`] for the window, which is where the one comparison lives.
#[derive(Debug, Clone, Copy)]
pub struct Startup {
    /// The key the installed hook listens for.
    pub hotkey: iris_core::hotkey::Key,
    /// Whether `iris-overlay` was spawned this run.
    pub overlay_enabled: bool,
    /// `hotkey` as `config.toml` held it at launch, before any CLI override.
    pub saved_hotkey: iris_core::hotkey::Key,
    /// `overlay_enabled` as `config.toml` held it at launch, likewise.
    pub saved_overlay_enabled: bool,
}

/// Start the settings window on its own thread.
///
/// On Windows this is a real `eframe` window, opened lazily on the first
/// [`WindowSink::open`]. Elsewhere it is [`NoopWindow`], so a caller builds
/// and runs on Linux with the window simply absent — the same shape as
/// `tray::spawn` and `iris_overlay::spawn`.
///
/// `outcomes` is the other half of `commands`: the loop answers every command
/// the window sends on it — under the [`crate::CommandId`] it came in with —
/// so the window can report what actually happened rather than what it asked
/// for, and only for the commands it sent itself. See
/// [`crate::App::with_window_commands`].
pub fn spawn(
    config_path: std::path::PathBuf,
    commands: crossbeam_channel::Sender<(crate::app::CommandId, crate::app::Command)>,
    outcomes: crossbeam_channel::Receiver<(crate::app::CommandId, crate::app::CommandOutcome)>,
    startup: Startup,
) -> anyhow::Result<Box<dyn WindowSink>> {
    #[cfg(windows)]
    {
        shell::spawn(config_path, commands, outcomes, startup)
    }
    #[cfg(not(windows))]
    {
        let _ = (config_path, commands, outcomes, startup);
        Ok(Box::new(NoopWindow))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_window_open_does_not_panic() {
        NoopWindow.open();
        let boxed: Box<dyn WindowSink> = Box::new(NoopWindow);
        boxed.open();
    }

    #[test]
    fn recording_window_counts_opens_through_a_clone() {
        let window = RecordingWindow::new();
        let observer = window.clone();
        assert_eq!(observer.opens(), 0);
        window.open();
        window.open();
        assert_eq!(observer.opens(), 2);
    }
}

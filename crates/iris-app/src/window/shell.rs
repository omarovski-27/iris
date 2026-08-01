//! The native window: bootstraps `eframe` and calls [`ui::draw_root`] once a
//! frame. Everything that is actually *view* lives in [`super::ui`]; this
//! file is deliberately thin — see the module docs on [`crate::window`].
//!
//! # Lifecycle
//!
//! One thread, started by [`spawn`] and kept for the life of the process
//! (mirroring `tray::spawn` and `iris_overlay::spawn`). It waits for an
//! `open` signal, runs `eframe::run_native` — which blocks this thread until
//! the window is closed — then goes back to waiting. A second `open` while
//! the window is already showing does not race the waiting `recv`: it is
//! picked up by [`super::ui::draw_root`]'s per-frame drain of the same
//! signal and turned into [`egui::ViewportCommand::Focus`] instead, since
//! that drain is the only thing polling the channel while the window is up.

use std::path::PathBuf;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};

use crate::app::Command;
use crate::config::Config;

use super::state::{Env, WindowState};
use super::{ui, WindowSink};

/// Sends the `open` signal [`spawn`]'s thread waits on.
pub struct WindowHandle {
    open_tx: Sender<()>,
}

impl WindowSink for WindowHandle {
    fn open(&self) {
        // Unbounded and never awaited: a click that arrives while the window
        // is already mid-open queues harmlessly, per the module docs.
        let _ = self.open_tx.send(());
    }
}

/// Start the settings-window thread. Returns immediately; the window itself
/// only appears once [`WindowSink::open`] is called (from the tray's
/// `Settings` item).
pub fn spawn(config_path: PathBuf, commands: Sender<Command>) -> Result<Box<dyn WindowSink>> {
    let (open_tx, open_rx) = crossbeam_channel::unbounded::<()>();

    std::thread::Builder::new()
        .name("iris-window".into())
        .spawn(move || {
            while open_rx.recv().is_ok() {
                run_window(open_rx.clone(), config_path.clone(), commands.clone());
            }
        })
        .context("spawning the settings-window thread")?;

    Ok(Box::new(WindowHandle { open_tx }))
}

/// Build and run one window instance until it is closed.
fn run_window(reopen_signal: Receiver<()>, config_path: PathBuf, commands: Sender<Command>) {
    // Best-effort: a config that fails to load just draws the default-theme
    // icon rather than blocking the window from opening at all.
    let theme = Config::load(&config_path).unwrap_or_default().theme;
    let icon = crate::tray::icon_rgba(theme, 64);

    let options = eframe::NativeOptions {
        // winit refuses to build an event loop off the main thread by
        // default (a cross-platform compatibility guard); this window's
        // thread — like the tray's and the overlay's — is never the main
        // one. Safe here specifically because this process never creates a
        // second window on a second thread: `spawn`'s loop runs `run_window`
        // to completion (the window closes) before it can run again.
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        })),
        viewport: egui::ViewportBuilder::default()
            .with_title("Iris")
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(egui::IconData {
                rgba: icon,
                width: 64,
                height: 64,
            }),
        ..Default::default()
    };

    let app = SettingsApp {
        config_path,
        commands,
        reopen_signal,
        state: None,
    };

    if let Err(e) = eframe::run_native(
        "iris-settings",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    ) {
        eprintln!("  settings window error: {e}");
    }
}

/// The `eframe::App`: owns the window's lifetime state and hands each frame
/// straight to [`ui::draw_root`].
struct SettingsApp {
    config_path: PathBuf,
    commands: Sender<Command>,
    reopen_signal: Receiver<()>,
    /// Built lazily on the first frame — `Env` borrows the fields above, so
    /// it cannot be constructed until `self` exists.
    state: Option<WindowState>,
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let env = Env {
            config_path: &self.config_path,
            commands: &self.commands,
            list_devices: &list_devices,
            reopen_signal: &self.reopen_signal,
        };
        let state = self.state.get_or_insert_with(|| WindowState::new(&env));
        ui::draw_root(ctx, state, &env);
    }
}

/// Enumerate input devices for the Settings tab's microphone picker. Empty
/// (rather than propagating the error into the UI) on any failure — the
/// tray's own microphone menu takes the same view, and a picker with no
/// entries is a fine degraded state for a window that is not required for
/// dictation to work.
fn list_devices() -> Vec<String> {
    iris_core::capture::list_devices()
        .map(|devices| devices.into_iter().map(|d| d.name).collect())
        .unwrap_or_default()
}

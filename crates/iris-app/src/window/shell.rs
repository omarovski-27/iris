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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};

use crate::app::Command;
use crate::config::Config;

use super::state::{Env, WindowState};
use super::{ui, Startup, WindowSink};

/// Sends the `open` signal [`spawn`]'s thread waits on.
pub struct WindowHandle {
    open_tx: Sender<()>,
}

impl WindowSink for WindowHandle {
    fn open(&self) {
        // Unbounded and never awaited: a click that arrives while the window
        // is already mid-open queues harmlessly, per the module docs. A send
        // that fails is not that case — the only way it can fail is the
        // window thread being gone (an `egui` panic unwinds it alone, leaving
        // the dictation loop running), which turns the tray's `Settings` item
        // into a permanent no-op and is worth saying out loud.
        if self.open_tx.send(()).is_err() {
            eprintln!("  settings window unavailable: its thread is no longer running");
        }
    }
}

/// Start the settings-window thread. Returns immediately; the window itself
/// only appears once [`WindowSink::open`] is called (from the tray's
/// `Settings` item).
pub fn spawn(
    config_path: PathBuf,
    commands: Sender<Command>,
    startup: Startup,
) -> Result<Box<dyn WindowSink>> {
    let (open_tx, open_rx) = crossbeam_channel::unbounded::<()>();

    std::thread::Builder::new()
        .name("iris-window".into())
        .spawn(move || {
            while open_rx.recv().is_ok() {
                run_window(
                    open_rx.clone(),
                    config_path.clone(),
                    commands.clone(),
                    startup,
                );
            }
        })
        .context("spawning the settings-window thread")?;

    Ok(Box::new(WindowHandle { open_tx }))
}

/// Build and run one window instance until it is closed.
fn run_window(
    reopen_signal: Receiver<()>,
    config_path: PathBuf,
    commands: Sender<Command>,
    startup: Startup,
) {
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
        startup,
        // Asked once per window, not once per frame: a timezone change while
        // the window happens to be open is not worth a syscall every 16 ms,
        // and reopening the window picks up the new one.
        utc_offset_seconds: local_utc_offset_seconds(),
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
    startup: Startup,
    utc_offset_seconds: i32,
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
            open_config_file: &open_config_file,
            reopen_signal: &self.reopen_signal,
            utc_offset_seconds: self.utc_offset_seconds,
            hotkey: super::InForce {
                running: self.startup.hotkey,
                at_startup: self.startup.saved_hotkey,
            },
            overlay_enabled: super::InForce {
                running: self.startup.overlay_enabled,
                at_startup: self.startup.saved_overlay_enabled,
            },
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

/// Hand `config.toml` to whatever the desktop opens it with — the Settings
/// tab's one route to the API keys, which the window itself never renders.
///
/// The window is not a second config writer: this spawns and forgets, and
/// `WindowState::refresh` picks up whatever the user saved a moment later,
/// exactly as it picks up a hand edit made outside Iris.
fn open_config_file(path: &Path) -> Result<()> {
    // `start` is a cmd builtin, hence `cmd /C`. The empty argument is the
    // window title, which `start` otherwise steals from the path.
    std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(path)
        .spawn()
        .context("launching the editor")?;
    Ok(())
}

/// The machine's current offset east of UTC, in seconds.
///
/// Windows rather than `time`'s `local-offset`: `UtcOffset::current_local_offset`
/// is documented as unsound in a multi-threaded process, which is exactly why
/// `crate::history` stamps every record in UTC. This process is many threads
/// deep by the time a window opens, so the offset comes from the OS instead —
/// `GetTimeZoneInformation` reports it as a *bias* in minutes to add to local
/// time to reach UTC, plus a seasonal bias picked by the season the call says
/// we are in. Falls back to UTC on the documented `TIME_ZONE_ID_INVALID`,
/// which only degrades the "Dictations today" tile.
// `GetTimeZoneInformation` has no safe wrapper. The call fills a struct we
// own and outlives nothing, so the opt-in is one function wide.
#[allow(unsafe_code)]
fn local_utc_offset_seconds() -> i32 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};

    /// `TIME_ZONE_ID_UNKNOWN` — a real zone, with no seasonal rule in force.
    const UNKNOWN: u32 = 0;
    /// `TIME_ZONE_ID_STANDARD` — standard time is in effect.
    const STANDARD: u32 = 1;
    /// `TIME_ZONE_ID_DAYLIGHT` — daylight saving is in effect.
    const DAYLIGHT: u32 = 2;

    let mut info = TIME_ZONE_INFORMATION::default();
    let seasonal = match unsafe { GetTimeZoneInformation(&mut info) } {
        // `UNKNOWN` is still standard time — the zone simply has no
        // daylight-saving transition — so it carries `StandardBias` like
        // `STANDARD` does, not a bias of zero.
        UNKNOWN | STANDARD => info.StandardBias,
        DAYLIGHT => info.DaylightBias,
        // TIME_ZONE_ID_INVALID: the call failed. Nothing says what it left in
        // `info`, so read none of it and answer UTC.
        _ => return 0,
    };
    -(info.Bias.saturating_add(seasonal)).saturating_mul(60)
}

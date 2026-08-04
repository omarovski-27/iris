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
//! The handover between those two readers is closed at the other end too:
//! a click landing after the window's last frame would otherwise be waiting
//! for this thread's `recv` and reopen the window the user has just closed,
//! so a closed window's leftover signals are dropped before it waits again.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};

use crate::app::{Command, CommandId, CommandOutcome};
use crate::config::Config;

use super::state::{Env, WindowState};
use super::{ui, EditorWindow, Startup, WindowSink};

/// The live window's `egui` context, shared with [`WindowHandle`] so a reopen
/// can wake the event loop. `None` between windows.
type SharedContext = Arc<Mutex<Option<egui::Context>>>;

/// Sends the `open` signal [`spawn`]'s thread waits on.
pub struct WindowHandle {
    open_tx: Sender<()>,
    /// The context of the window that is up, if one is. Set on its first
    /// frame, cleared when it closes.
    ctx: SharedContext,
    /// Set once the window has proved it cannot run on this machine, by
    /// whichever side found that out. From then on every click takes
    /// `fallback` instead of waking a thread that would only fail again.
    unusable: Arc<AtomicBool>,
    /// Where the tray's `Settings` item goes when the window cannot.
    fallback: Arc<EditorWindow>,
}

impl WindowSink for WindowHandle {
    fn open(&self) {
        if self.unusable.load(Ordering::SeqCst) {
            self.fallback.open();
            return;
        }
        // Unbounded and never awaited: a click that arrives while the window
        // is already mid-open queues harmlessly, per the module docs. A send
        // that fails is not that case — the only way it can fail is the
        // window thread being gone (an `egui` panic unwinds it alone, leaving
        // the dictation loop running).
        if self.open_tx.send(()).is_err() {
            if !self.unusable.swap(true, Ordering::SeqCst) {
                eprintln!("  settings window unavailable: its thread is no longer running");
            }
            self.fallback.open();
            return;
        }
        // The signal above is only read by `ui::draw_root`'s per-frame drain
        // while a window is up, and that drain is paced by
        // `request_repaint_after` — up to `REFRESH_INTERVAL` away when nothing
        // else is asking for a frame (the window minimized, or behind
        // another). Ask for the frame directly, so the click that raises an
        // already-open window is answered now rather than seconds later.
        let live = self.ctx.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(ctx) = live.as_ref() {
            ctx.request_repaint();
        }
    }
}

/// Start the settings-window thread. Returns immediately; the window itself
/// only appears once [`WindowSink::open`] is called (from the tray's
/// `Settings` item).
pub fn spawn(
    config_path: PathBuf,
    commands: Sender<(CommandId, Command)>,
    outcomes: Receiver<(CommandId, CommandOutcome)>,
    startup: Startup,
) -> Result<Box<dyn WindowSink>> {
    let (open_tx, open_rx) = crossbeam_channel::unbounded::<()>();
    let ctx: SharedContext = Arc::new(Mutex::new(None));
    let unusable = Arc::new(AtomicBool::new(false));
    let fallback = Arc::new(EditorWindow::new(config_path.clone()));

    let thread_ctx = Arc::clone(&ctx);
    let thread_unusable = Arc::clone(&unusable);
    let thread_fallback = Arc::clone(&fallback);
    std::thread::Builder::new()
        .name("iris-window".into())
        .spawn(move || {
            while open_rx.recv().is_ok() {
                // A click queued before the failure below was seen, or while
                // this thread was busy failing: still owed the fallback.
                if thread_unusable.load(Ordering::SeqCst) {
                    thread_fallback.open();
                    continue;
                }
                match run_window(
                    open_rx.clone(),
                    config_path.clone(),
                    commands.clone(),
                    outcomes.clone(),
                    startup,
                    &thread_ctx,
                ) {
                    // The window this thread just ran was the other reader of
                    // this channel. A click it never got to drain — one that
                    // landed after its last frame — is a raise for a window
                    // that no longer exists, and reopening the one the user
                    // has just closed is worse than dropping it.
                    Ok(()) => while open_rx.try_recv().is_ok() {},
                    Err(e) => {
                        // `eframe` fails for reasons that do not change between
                        // clicks — no usable GL, no display — so the diagnosis is
                        // said once and the item is routed elsewhere from here
                        // on, rather than repeating into a console a resident
                        // tray app never shows.
                        if !thread_unusable.swap(true, Ordering::SeqCst) {
                            eprintln!("  settings window error: {e:#}");
                        }
                        thread_fallback.open();
                        // Nothing drained this channel while that failure ran:
                        // every queued click is still owed the fallback, and
                        // the `unusable` check above hands it to them.
                    }
                }
            }
        })
        .context("spawning the settings-window thread")?;

    Ok(Box::new(WindowHandle {
        open_tx,
        ctx,
        unusable,
        fallback,
    }))
}

/// Build and run one window instance until it is closed. An error here is the
/// window proving it cannot run on this machine at all.
fn run_window(
    reopen_signal: Receiver<()>,
    config_path: PathBuf,
    commands: Sender<(CommandId, Command)>,
    outcomes: Receiver<(CommandId, CommandOutcome)>,
    startup: Startup,
    ctx: &SharedContext,
) -> Result<()> {
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
        outcomes,
        reopen_signal,
        startup,
        // Asked once per window, not once per frame: a timezone change while
        // the window happens to be open is not worth a syscall every 16 ms,
        // and reopening the window picks up the new one.
        utc_offset_seconds: local_utc_offset_seconds(),
        ctx: Arc::clone(ctx),
        state: None,
    };

    let result = eframe::run_native(
        "iris-settings",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    );
    // This window's context dies with it; a later reopen publishes the next
    // one on its first frame.
    *ctx.lock().unwrap_or_else(PoisonError::into_inner) = None;
    result.map_err(|e| anyhow::anyhow!("{e}"))
}

/// The `eframe::App`: owns the window's lifetime state and hands each frame
/// straight to [`ui::draw_root`].
struct SettingsApp {
    config_path: PathBuf,
    commands: Sender<(CommandId, Command)>,
    /// The loop's answers to what went out on `commands`. A clone of the one
    /// receiver, so an outcome for a command an earlier window sent can still
    /// arrive here — [`WindowState::poll_outcomes`] recognises it by its
    /// [`CommandId`] as belonging to nobody and drops it.
    outcomes: Receiver<(CommandId, CommandOutcome)>,
    reopen_signal: Receiver<()>,
    startup: Startup,
    utc_offset_seconds: i32,
    /// Published on the first frame, for [`WindowHandle::open`] to wake this
    /// window with.
    ctx: SharedContext,
    /// Built lazily on the first frame — `Env` borrows the fields above, so
    /// it cannot be constructed until `self` exists.
    state: Option<WindowState>,
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.state.is_none() {
            *self.ctx.lock().unwrap_or_else(PoisonError::into_inner) = Some(ctx.clone());
        }
        let env = Env {
            config_path: &self.config_path,
            commands: &self.commands,
            outcomes: &self.outcomes,
            list_devices: &list_devices,
            open_config_file: &super::open_config_file,
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

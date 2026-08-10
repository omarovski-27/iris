//! `iris` — the resident dictation app.
//!
//! Startup order matters and is deliberate:
//!
//! 1. read the config, apply CLI overrides, **promote keys to the environment**
//!    (safe only while the process is still single-threaded);
//! 2. open the microphone, install the hotkey hook, start the tray;
//! 3. build [`iris_app::App`], and with it the engine — the one step that can
//!    fail for a reason the user must act on (a missing key);
//! 4. run the loop and never touch it again.

// GUI subsystem: stops Windows allocating a console for *any* launch of this
// binary — double-click, Start Menu, Startup folder, all of them. Before this
// attribute existed the linker defaulted to the console subsystem, so every
// one of those launches opened a terminal window whether or not the app ever
// wrote a line to it; a v0.1.0 release shipped exactly that regression. See
// `attach_console_for_cli_output` for how a real terminal invocation (`iris
// --print-config`, `--verbose`, …) still gets its output back, and
// `report_failure` for how a start-up failure reaches a user who now has no
// console to read stderr from.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
#[cfg(windows)]
use iris_app::audio::AudioSource;
use iris_app::config::{self, Config, EngineChoice};
use iris_app::inject::{DryRunInjector, Injector};
#[cfg(windows)]
use iris_app::notify::{FailureNotice, NoopFailureNotice};
use iris_app::pill::{overlay_theme, LogPill, NoopPill, OverlayPill, PillSink};
use iris_app::{App, DictationRecord, SessionLog};
use iris_core::hotkey::Key;
use iris_core::inject::Method;

/// Hold a key, speak, release: the text appears in whatever you were typing in.
#[derive(Debug, Parser)]
#[command(name = "iris", version, about, long_about = None)]
struct Args {
    /// Configuration file. Defaults to `iris/config.toml` in the platform
    /// config directory.
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

    /// Transcription engine for this run. Does not change the saved setting.
    #[arg(long, value_name = "ENGINE")]
    engine: Option<EngineChoice>,

    /// Push-to-talk key for this run.
    #[arg(long, value_name = "KEY")]
    hotkey: Option<Key>,

    /// Input device (a case-insensitive substring of its name).
    #[arg(long, value_name = "NAME")]
    device: Option<String>,

    /// How to deliver text: sendinput or clipboard.
    #[arg(long, value_name = "METHOD")]
    inject: Option<Method>,

    /// Skip transcript cleanup for this run.
    #[arg(long)]
    no_polish: bool,

    /// Print what would be typed instead of typing it.
    #[arg(long)]
    dry_run: bool,

    /// Print a latency breakdown after every dictation.
    #[arg(long)]
    report: bool,

    /// Diagnostics on stderr.
    #[arg(long, short)]
    verbose: bool,

    /// List input devices and exit.
    #[arg(long)]
    list_devices: bool,

    /// Print the last N dictations from the session log and exit.
    #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "10")]
    history: Option<usize>,

    /// Print the resolved configuration (never the keys) and exit.
    #[arg(long)]
    print_config: bool,

    /// Run one dictation from a WAV file instead of the microphone, then exit.
    ///
    /// The portable way to exercise the whole loop — engine, polish, session
    /// log — on a machine with no microphone and no hotkey. Implies --dry-run
    /// unless you ask for real injection.
    #[arg(long, value_name = "WAV")]
    speak_wav: Option<std::path::PathBuf>,

    /// With --speak-wav, really inject the text. Off by default: injected
    /// keystrokes go to whatever window happens to be focused.
    #[arg(long)]
    really_inject: bool,

    /// Drive one full mock dictation with synthetic levels and the real pill
    /// (headless off Windows). Never uses live SendInput — always dry-run
    /// inject so the session log records a mock insert safely.
    #[arg(long)]
    demo_dictation: bool,

    /// Open the Settings window against a seeded demo config and session
    /// log, isolated under the system temp directory, then keep running
    /// long enough to inspect or screenshot it. Never touches the hotkey,
    /// the microphone or injection — the manual verification path for the
    /// window, the same role `--demo-dictation` plays for the loop.
    #[arg(long)]
    demo_window: bool,

    /// Start resident without opening the settings window.
    ///
    /// Set only on the Startup-folder shortcut `install.ps1` creates: a
    /// boot-time autostart must go quietly to the tray, ready for the
    /// hotkey, not put a window on screen at every login. Every other launch
    /// path — the Start Menu shortcut, a Desktop shortcut, a plain
    /// double-click of the `.exe` — omits this flag, because opening Iris is
    /// exactly the deliberate action the window should answer.
    #[arg(long, hide = true)]
    background: bool,
}

fn main() -> Result<()> {
    // Before anything else can panic: every fallible startup/run step already
    // reaches `report_failure`'s dialog through `inspect_err`, but a panic
    // bypasses `Result` entirely, and the default hook only writes to
    // stderr — exactly the console a GUI-subsystem launch does not have. See
    // `install_panic_dialog`.
    install_panic_dialog();
    // Before anything prints, including clap's own --help/error output: a GUI
    // subsystem process starts with no console at all, so this is the only
    // chance to reconnect stdout/stderr to one before the first `println!`.
    #[cfg(windows)]
    attach_console_for_cli_output();

    let args = Args::parse();
    iris_core::log::set_verbose(args.verbose);

    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    // Only the plain default location is eligible: an explicit `--config` or
    // `$IRIS_CONFIG` means the caller already chose a layout on purpose, and
    // moving files underneath that would be a surprise, not a safety net.
    // Not fatal to startup — reported the same two ways as the migration-save
    // failure just below, then `Config::load_or_create_reporting` proceeds
    // against whatever `config_path` holds regardless.
    #[cfg(windows)]
    if args.config.is_none() && !config::path_overridden_by_env() {
        if let Err(e) = config::migrate_default_location_from_roaming() {
            let err = anyhow::Error::new(e);
            eprintln!("could not migrate settings out of the Roaming profile: {err:#}");
            report_failure("Iris could not migrate your settings", &err);
        }
    }
    // Kept as loaded: `config` below takes the CLI overrides, which are
    // run-only and must never be written back to the file.
    //
    // A migration-save failure is not fatal (see the doc comment on
    // `load_or_create_reporting`), so it never reaches the `inspect_err`
    // below; report it here, the same two ways as a fatal one — the
    // `eprintln!` a real terminal invocation now sees via
    // `attach_console_for_cli_output`, and the dialog a console-less launch
    // needs instead.
    let file_config = Config::load_or_create_reporting(&config_path, |e| {
        eprintln!("{}", Config::migration_save_error_message(&config_path, e));
        #[cfg(windows)]
        report_failure("Iris could not update its settings", e);
    })
    .with_context(|| format!("loading {}", config_path.display()))
    .inspect_err(report_startup_failure)?;
    let mut config = file_config.clone();
    apply_overrides(&mut config, &args);

    // Before any thread exists: see Config::promote_keys.
    for var in config.promote_keys() {
        iris_core::vlog!("using the {var} key from {}", config_path.display());
    }

    if args.print_config {
        println!("{}", config.to_redacted_toml()?);
        println!("# path: {}", config_path.display());
        return Ok(());
    }
    if args.list_devices {
        return list_devices();
    }
    if let Some(n) = args.history {
        return print_history(&config, &config_path, n);
    }
    if args.demo_dictation {
        return demo_dictation(config, &config_path, &args);
    }
    if args.demo_window {
        return demo_window();
    }
    if let Some(wav) = args.speak_wav.clone() {
        return speak_wav(config, &config_path, &args, &wav);
    }

    run(config, file_config, &config_path, &args)
}

fn apply_overrides(config: &mut Config, args: &Args) {
    if let Some(engine) = args.engine {
        config.engine = engine;
    }
    if let Some(hotkey) = args.hotkey {
        config.hotkey = hotkey;
    }
    if let Some(device) = &args.device {
        config.audio.device = Some(device.clone());
    }
    if let Some(method) = args.inject {
        config.inject.method = method;
    }
    if args.no_polish {
        config.polish.enabled = false;
    }
}

/// Build a pill sink. On Windows the resident path prefers a live overlay;
/// elsewhere (and when the overlay fails to start) fall back to log/noop so
/// CI and non-Windows paths stay green.
fn pill_for(
    args: &Args,
    config: &Config,
    overlay: Option<&iris_overlay::Overlay>,
) -> Box<dyn PillSink> {
    if let Some(overlay) = overlay {
        return Box::new(OverlayPill::new(overlay.handle(), config.show_live_text));
    }
    if args.verbose {
        Box::new(LogPill)
    } else {
        Box::new(NoopPill)
    }
}

/// Start the overlay thread. Returns `None` only if spawn fails (logged);
/// callers keep working with a Noop/Log pill in that case.
fn try_spawn_overlay(config: &Config) -> Option<iris_overlay::Overlay> {
    let engine_label = config.engine.as_str().to_string();
    match iris_overlay::spawn(iris_overlay::OverlayConfig {
        theme: overlay_theme(config.theme),
        engine: engine_label,
    }) {
        Ok(overlay) => Some(overlay),
        Err(e) => {
            eprintln!("  overlay unavailable: {e:#}");
            None
        }
    }
}

/// Everything the resident loop needs, built and ready to run.
///
/// The point of the struct is that [`start_resident`] is the single fallible
/// startup step `run` sees: every way starting up can fail leaves through one
/// `Result`, so [`report_startup_failure`] covers all of them and a step added
/// later is covered by construction.
#[cfg(windows)]
struct Resident {
    app: App<iris_app::audio::MicAudio>,
    keys: crossbeam_channel::Receiver<iris_core::hotkey::HotkeyEvent>,
    commands: crossbeam_channel::Receiver<iris_app::Command>,
    overlay: Option<iris_overlay::Overlay>,
    // Guards: dropping the listener uninstalls the hook, dropping the tray
    // stops its thread, dropping the single-instance guard releases the lock
    // a later launch checks. Declared after `app` so they outlive it.
    _listener: iris_core::hotkey::Listener,
    _tray: iris_app::tray::Tray,
    // `None` only when `single_instance::acquire` itself failed (logged at
    // the call site in `run`) — a degraded mode with no second-launch
    // detection, not a startup failure.
    _single_instance: Option<iris_app::single_instance::Guard>,
}

/// The resident loop.
#[cfg(windows)]
fn run(
    config: Config,
    file_config: Config,
    config_path: &std::path::Path,
    args: &Args,
) -> Result<()> {
    // Best-effort: a single-instance check that itself fails to start must
    // not be the reason dictation stops working, so a broken lock is treated
    // the same as "no other instance found" rather than aborting startup.
    let (single_instance, reopen) = match iris_app::single_instance::acquire() {
        Ok(iris_app::single_instance::Instance::AlreadyRunning) => {
            iris_core::vlog!("already running; asked it to show its window");
            return Ok(());
        }
        Ok(iris_app::single_instance::Instance::Primary { guard, reopen }) => (Some(guard), reopen),
        Err(e) => {
            eprintln!("  single-instance check unavailable: {e:#}");
            (None, crossbeam_channel::never())
        }
    };

    let mut resident = start_resident(
        config,
        file_config,
        config_path,
        args,
        single_instance,
        reopen,
    )
    .inspect_err(report_startup_failure)?;

    banner(&resident.app, config_path);
    // The Startup shortcut carries `--background` (see `Args::background`):
    // a boot-time autostart must stay in the tray, ready for the hotkey,
    // never put a window on screen unasked. Every other launch path is
    // exactly the deliberate "open Iris" action the window should answer.
    if !args.background {
        resident.app.open_window();
    }
    let result = resident.app.run(&resident.keys, &resident.commands);
    // Explicit shutdown so the window is gone before we exit — and before the
    // dialog below, which is modal.
    if let Some(overlay) = resident.overlay.take() {
        overlay.shutdown();
    }
    // A dictation whose finalise was still running when Quit arrived keeps
    // delivering (inject + log) on a detached thread instead of blocking
    // `run` above — see `App::capture`'s doc comment. Take the handle and
    // drop everything visible (tray icon, hotkey hook, single-instance lock)
    // *before* joining it, so the app already looks closed while this
    // invisibly waits for the words to actually land; joining before the
    // drop would bring back the exact lag this exists to fix.
    let pending_finalize = resident.app.take_pending_finalize();
    drop(resident);
    if let Some(handle) = pending_finalize {
        let _ = handle.join();
    }
    result.inspect_err(report_run_failure)
}

/// Open the microphone, start the tray, install the hook and build the app.
#[cfg(windows)]
fn start_resident(
    config: Config,
    file_config: Config,
    config_path: &std::path::Path,
    args: &Args,
    single_instance: Option<iris_app::single_instance::Guard>,
    reopen: crossbeam_channel::Receiver<()>,
) -> Result<Resident> {
    use iris_app::audio::MicAudio;
    use iris_app::inject::SystemInjector;
    use iris_app::notify::SystemFailureNotice;
    use iris_app::tray;

    let audio = MicAudio::new(config.audio.device.clone(), config.audio.warm)?;
    let injector: Arc<dyn Injector> = if args.dry_run {
        Arc::new(DryRunInjector)
    } else {
        Arc::new(SystemInjector::new(config.inject.method, config.hotkey))
    };
    // Same split as the injector above: `--dry-run` never really fails to
    // inject, but keeping the two in lockstep means a failure notice can
    // never reach a real clipboard or a real dialog on a run that promised
    // not to touch the desktop.
    let notice: Arc<dyn FailureNotice> = if args.dry_run {
        Arc::new(NoopFailureNotice)
    } else {
        Arc::new(SystemFailureNotice)
    };

    let devices = iris_core::capture::list_devices()
        .map(|d| d.into_iter().map(|d| d.name).collect())
        .unwrap_or_default();
    // Shared with `App` below (`App::with_quit_flag`) so the tray can flip it
    // the instant it sends `Command::Quit` — see `App::capture`'s doc comment
    // for why that needs to happen outside the `control` channel itself.
    let quit_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tray, commands) = tray::spawn(&config, config_path, devices, Arc::clone(&quit_flag))?;

    let (listener, keys) = iris_core::hotkey::listen(config.hotkey, config.suppress_hotkey)
        .context("installing the push-to-talk hook")?;

    // Overlay owns its thread for process life; App drives it via OverlayPill.
    let overlay = if config.overlay_enabled {
        try_spawn_overlay(&config)
    } else {
        None
    };
    let pill = pill_for(args, &config, overlay.as_ref());

    // The settings window's own thread, mirroring the tray and the overlay.
    // Its commands travel on a channel separate from the tray's `commands`
    // (App::run selects on both), so a window write can never race a tray
    // write to persist() — see `iris_app::window::state`'s module docs.
    //
    // `Startup` is what this process is *actually* doing — the hook above was
    // installed with `config.hotkey` and the overlay was spawned (or not)
    // just now, neither changing again without a restart — paired with what
    // the file said before `apply_overrides` ran. `file_config` is the
    // pre-override copy, so a run-only `--hotkey` reads as already in force
    // rather than as an edit waiting to be restarted into.
    let startup = iris_app::window::Startup {
        hotkey: config.hotkey,
        overlay_enabled: overlay.is_some(),
        saved_hotkey: file_config.hotkey,
        saved_overlay_enabled: file_config.overlay_enabled,
    };
    let (window_commands_tx, window_commands_rx) = crossbeam_channel::unbounded();
    let (window_outcomes_tx, window_outcomes_rx) = crossbeam_channel::unbounded();
    // Best-effort, on the same terms as the overlay above: dictation needs no
    // window, so one that cannot start costs the user the real Settings UI —
    // and falls back to `config.toml` in their editor — rather than the whole
    // application.
    let window = iris_app::window::spawn_or_editor(
        config_path.to_path_buf(),
        window_commands_tx,
        window_outcomes_rx,
        startup,
        Arc::clone(&quit_flag),
    );

    let app = App::new(config, config_path, audio, injector, pill)?
        .with_report(args.report)
        .with_file_config(file_config)
        .with_window(window)
        .with_window_commands(window_commands_rx, window_outcomes_tx)
        .with_reopen_signal(reopen)
        .with_failure_notice(notice)
        .with_quit_flag(quit_flag);

    Ok(Resident {
        app,
        keys,
        commands,
        overlay,
        _listener: listener,
        _tray: tray,
        _single_instance: single_instance,
    })
}

/// There is no hotkey, no microphone and no injection off Windows, so the
/// resident loop cannot do anything useful. Everything else — config, engines,
/// polish, the session log, `--speak-wav` — works, which is what keeps this
/// crate testable in CI.
#[cfg(not(windows))]
fn run(
    _config: Config,
    _file_config: Config,
    _config_path: &std::path::Path,
    _args: &Args,
) -> Result<()> {
    anyhow::bail!(
        "the resident loop needs Windows: the global hotkey hook, microphone capture and text \
         injection are all Win32.\nOn this host you can still use --demo-dictation, --speak-wav \
         to run a dictation from a file, --history, --list-devices and --print-config."
    )
}

/// One dictation from a WAV file, with no microphone and no hotkey.
///
/// The frames are fed from a thread and the "key release" follows them, so the
/// engine sees the same sequence a real utterance produces — including the
/// interim transcripts, which a pre-loaded channel would skip.
fn speak_wav(
    config: Config,
    config_path: &std::path::Path,
    args: &Args,
    wav: &std::path::Path,
) -> Result<()> {
    use iris_app::audio::ChannelAudio;
    use iris_core::hotkey::HotkeyEvent;

    let pcm =
        iris_core::audio::read_wav(wav).with_context(|| format!("reading {}", wav.display()))?;

    if args.really_inject && cfg!(not(windows)) {
        anyhow::bail!("--really-inject needs Windows");
    }
    let injector: Arc<dyn Injector> = match args.really_inject && !args.dry_run {
        #[cfg(windows)]
        true => Arc::new(iris_app::inject::SystemInjector::new(
            config.inject.method,
            config.hotkey,
        )),
        #[cfg(not(windows))]
        true => unreachable!("guarded above"),
        false => Arc::new(DryRunInjector),
    };

    // Optional real pill for offline demos; never required for correctness.
    let overlay = try_spawn_overlay(&config);
    let pill = pill_for(args, &config, overlay.as_ref());

    let audio = ChannelAudio::new();
    let frames_tx = audio.sender();
    let armed = audio.armed();
    let mut app = App::new(config, config_path, audio, injector, pill)?.with_report(args.report);
    let frames = app.frames();

    let (keys_tx, keys) = crossbeam_channel::unbounded();
    let pressed_at = std::time::Instant::now();
    let frame_time = std::time::Duration::from_millis(u64::from(iris_core::audio::FRAME_MS));
    let feeder = std::thread::spawn(move || {
        // Wait for the loop's stale-frame drain (which precedes arming), so no
        // utterance frame can be discarded as pre-key-press audio.
        if armed.recv().is_err() {
            return;
        }
        // Real-time pacing, not a burst: a live mic delivers one frame every
        // 20 ms, so Deepgram is still transcribing the tail when the key
        // comes up. Sending the whole file in one go finishes the utterance
        // long before Finalize/CloseStream, which hides exactly the race a
        // held key is meant to exercise. Deadline-based like
        // `dictation::run_offline`'s `Pace::Realtime`, so per-frame overhead
        // cannot accumulate into drift.
        let started = std::time::Instant::now();
        for (i, chunk) in pcm.chunks(iris_core::audio::FRAME_SAMPLES).enumerate() {
            let deadline = started + frame_time * i as u32;
            let now = std::time::Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
            if frames_tx.send(chunk.to_vec()).is_err() {
                return;
            }
        }
        // Let the engine turn the last frames into partials before the release,
        // exactly as a speaker's final syllable would.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = keys_tx.send(HotkeyEvent::Up(std::time::Instant::now()));
    });

    let dictated = app.dictate(pressed_at, &frames, &keys)?;
    let _ = feeder.join();
    // Let the inserted confirmation hold finish when a real overlay is up.
    if overlay.is_some() && dictated.record.injected {
        std::thread::sleep(std::time::Duration::from_millis(
            u64::from(iris_overlay::motion::INSERTED_HOLD_MS)
                + u64::from(iris_overlay::motion::EXIT_MS)
                + 50,
        ));
    }
    println!("  {}", dictated.record.text);
    if let Some(overlay) = overlay {
        overlay.shutdown();
    }
    Ok(())
}

/// Full mock dictation cycle: synthetic audio levels, dry-run inject, real pill.
///
/// Safe for automated smoke — never constructs `SystemInjector`. On Windows the
/// pill is visible; elsewhere the overlay runs headless so CI still exercises
/// the adapter path.
fn demo_dictation(mut config: Config, config_path: &std::path::Path, args: &Args) -> Result<()> {
    use iris_app::audio::ChannelAudio;
    use iris_core::hotkey::HotkeyEvent;

    // Force the offline mock so the demo never needs a network key.
    config.engine = EngineChoice::Mock;
    // Demo never injects live keystrokes, even if the caller forgot --dry-run.
    let injector: Arc<dyn Injector> = Arc::new(DryRunInjector);

    let pcm = synthetic_demo_pcm();
    let overlay = try_spawn_overlay(&config);
    let pill = pill_for(args, &config, overlay.as_ref());

    let audio = ChannelAudio::new();
    let frames_tx = audio.sender();
    let armed = audio.armed();
    let mut app = App::new(config, config_path, audio, injector, pill)?.with_report(args.report);
    let frames = app.frames();

    let (keys_tx, keys) = crossbeam_channel::unbounded();
    let pressed_at = std::time::Instant::now();
    let feeder = std::thread::spawn(move || {
        if armed.recv().is_err() {
            return;
        }
        for chunk in pcm.chunks(iris_core::audio::FRAME_SAMPLES) {
            if frames_tx.send(chunk.to_vec()).is_err() {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = keys_tx.send(HotkeyEvent::Up(std::time::Instant::now()));
    });

    let dictated = app.dictate(pressed_at, &frames, &keys)?;
    let _ = feeder.join();
    if overlay.is_some() && dictated.record.injected {
        std::thread::sleep(std::time::Duration::from_millis(
            u64::from(iris_overlay::motion::INSERTED_HOLD_MS)
                + u64::from(iris_overlay::motion::EXIT_MS)
                + 50,
        ));
    }
    println!("  demo: {}", dictated.record.text);
    println!(
        "  injected={} (dry-run)  engine={}",
        dictated.record.injected, dictated.record.engine
    );
    if let Some(overlay) = overlay {
        overlay.shutdown();
    }
    Ok(())
}

/// Open the Settings window against a seeded, isolated demo config and
/// session log. Never constructs a hotkey listener, an audio source or an
/// injector — this is a window-only demo, the counterpart `--demo-dictation`
/// is for the loop.
fn demo_window() -> Result<()> {
    use iris_app::window;

    let dir = std::env::temp_dir().join("iris-window-demo");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let config_path = dir.join("config.toml");
    let history_path = dir.join("history.jsonl");

    let config = Config::default();
    config
        .save(&config_path)
        .with_context(|| format!("writing {}", config_path.display()))?;

    // The path is fixed, so without this the ten demo records append to the
    // ten from the last run and the Insights figures — and any screenshot of
    // them — drift a little further every time.
    if history_path.exists() {
        std::fs::remove_file(&history_path)
            .with_context(|| format!("clearing {}", history_path.display()))?;
    }
    let mut log = SessionLog::open(&history_path, 500);
    // A real session log is appended to as dictations happen, so it is oldest
    // first on disk and the window reverses it. Seed it the same way round, or
    // the demo — the screenshot path — shows History backwards.
    for record in demo_records().into_iter().rev() {
        log.append(&record)?;
    }

    // The window reports "Saved" only once the loop says a change landed, and
    // `App` — absent here — is what turns that into a file *and* answers. With
    // no stand-in the demo would sit on "Saving…" forever and write nothing,
    // which is exactly the failure a screenshot of this path is supposed to
    // catch. So the demo plays the loop: it applies each command to the seeded
    // file and reports the same accept/reject `App::apply` would.
    let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
    let (outcomes_tx, outcomes_rx) = crossbeam_channel::unbounded();
    let demo_config_path = config_path.clone();
    std::thread::spawn(move || {
        for (id, command) in commands_rx {
            let outcome = match apply_demo_command(&demo_config_path, &command) {
                Ok(()) => iris_app::CommandOutcome::Applied,
                Err(e) => {
                    eprintln!("  demo: {command:?} not saved: {e:#}");
                    iris_app::CommandOutcome::Rejected(format!("{e:#}"))
                }
            };
            if outcomes_tx.send((id, outcome)).is_err() {
                break;
            }
        }
    });

    let handle = window::spawn(
        config_path.clone(),
        commands_tx,
        outcomes_rx,
        window::Startup {
            hotkey: config.hotkey,
            // No overlay process in the demo, whatever the seeded file says.
            overlay_enabled: false,
            saved_hotkey: config.hotkey,
            saved_overlay_enabled: config.overlay_enabled,
        },
        // No `App` is running in this demo to poll it, but closing the demo
        // window (`X`) still flips it and sends `Command::Quit` on
        // `commands_tx` — the stand-in loop above shrugs it off the same way
        // `Command::apply_to` does (`Quit` writes no field), same as every
        // other command this demo cannot really apply.
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?;
    handle.open();

    println!("iris --demo-window");
    println!(
        "  config   {} (settings changed in the window are written here)",
        config_path.display()
    );
    println!("  history  {}", history_path.display());
    if !cfg!(windows) {
        println!("  no window on this platform (Windows-only); the seeded files above are real.");
        return Ok(());
    }
    println!("  Settings window opened; exiting in 120s (Ctrl+C to stop sooner).");
    std::thread::sleep(std::time::Duration::from_secs(120));
    Ok(())
}

/// `--demo-window`'s stand-in for `App`'s command handling: persist one
/// setting change to the seeded config file.
///
/// Only the settings the window can change are handled — which is exactly
/// what [`Command::apply_to`](iris_app::app::Command::apply_to) writes, and
/// why the demo asks it rather than repeating the mapping. The rest of
/// [`Command`](iris_app::app::Command) belongs to the dictation loop, which
/// this demo deliberately
/// does not run — there is no engine to switch, no microphone to reopen and
/// nothing to quit.
fn apply_demo_command(
    config_path: &std::path::Path,
    command: &iris_app::app::Command,
) -> Result<()> {
    let mut config =
        Config::load(config_path).with_context(|| format!("reading {}", config_path.display()))?;
    if !command.apply_to(&mut config) {
        return Ok(());
    }
    config
        .save(config_path)
        .with_context(|| format!("writing {}", config_path.display()))
}

/// A handful of realistic-looking dictations spanning today and yesterday,
/// several engines, a failed injection and a silent tap, with enough
/// repeated words and phrases for the Insights tab to have something to show.
fn demo_records() -> Vec<DictationRecord> {
    let now = time::OffsetDateTime::now_utc();
    let stamp = |offset_hours: i64| {
        (now - time::Duration::hours(offset_hours))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    };
    let record = |engine: &str,
                  hours_ago: i64,
                  text: &str,
                  injected: bool,
                  error: Option<&str>,
                  ms: Option<f64>| {
        let mut r = DictationRecord::now(engine, text);
        r.timestamp = stamp(hours_ago);
        r.injected = injected;
        r.error = error.map(str::to_string);
        r.latency.perceived_ms = ms;
        r
    };

    vec![
        record(
            "deepgram",
            0,
            "let me know if the deploy looks good on your end",
            true,
            None,
            Some(210.0),
        ),
        record(
            "deepgram",
            1,
            "thanks so much for the quick review",
            true,
            None,
            Some(180.0),
        ),
        record(
            "groq",
            2,
            "the project timeline moved up a week, let me know if that works for the team",
            false,
            Some("injection failed: SendInput delivered 0 of 42 events"),
            None,
        ),
        record("deepgram", 3, "", false, None, None),
        record(
            "mock",
            5,
            "quick note to self, follow up on the deploy tomorrow",
            true,
            None,
            Some(90.0),
        ),
        record(
            "deepgram",
            20,
            "thanks so much, talk soon",
            true,
            None,
            Some(240.0),
        ),
        record(
            "groq",
            22,
            "let me know when the project is ready for review",
            true,
            None,
            Some(310.0),
        ),
        record(
            "deepgram",
            25,
            "the deploy went out clean, no rollbacks needed",
            false,
            Some("injection failed: the focused window rejected input"),
            None,
        ),
        record(
            "mock",
            30,
            "let me know if you need anything else from me",
            true,
            None,
            Some(120.0),
        ),
        record(
            "deepgram",
            48,
            "thanks so much for covering the project this week",
            true,
            None,
            Some(200.0),
        ),
    ]
}

/// About one second of a 220 Hz tone — enough for the mock engine to stream
/// partials and for the pill meter to move.
fn synthetic_demo_pcm() -> Vec<i16> {
    (0..16_000)
        .map(|i| {
            ((2.0 * std::f64::consts::PI * 220.0 * i as f64 / 16_000.0).sin() * 8_000.0) as i16
        })
        .collect()
}

fn list_devices() -> Result<()> {
    #[cfg(windows)]
    {
        let devices = iris_core::capture::list_devices()?;
        if devices.is_empty() {
            println!("No input devices found. Check Windows sound settings.");
            return Ok(());
        }
        println!("Input devices:");
        for d in devices {
            let marker = if d.default { "*" } else { " " };
            let format = match (d.rate, d.channels) {
                (Some(r), Some(c)) => format!("{r} Hz x{c}"),
                _ => "format unavailable".into(),
            };
            println!("  {marker} {:<48} {format}", d.name);
        }
        println!("\n  * = default. Pick another from the tray, or --device <substring>.");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("device enumeration needs Windows (cpal/WASAPI)")
    }
}

fn print_history(config: &Config, config_path: &std::path::Path, n: usize) -> Result<()> {
    let path = config.history_path(config_path);
    let records = SessionLog::read_all(&path)?;
    if records.is_empty() {
        println!("No dictations recorded yet ({}).", path.display());
        return Ok(());
    }
    for record in records.iter().rev().take(n).rev() {
        let latency = record
            .latency
            .perceived_ms
            .map(|ms| format!("{ms:.0} ms"))
            .unwrap_or_else(|| "—".into());
        let marker = if record.injected { " " } else { "!" };
        println!(
            "{marker} {}  {:<9} {:>8}  {}",
            record.timestamp, record.engine, latency, record.text
        );
        if let Some(error) = &record.error {
            println!("    {error}");
        }
    }
    println!("\n  {} ({} entries)", path.display(), records.len());
    Ok(())
}

/// Give a real terminal invocation (`iris --print-config`, `--verbose`,
/// `--list-devices`, `--history`, …) somewhere to print, without ever giving a
/// double-click or a shortcut a console of its own.
///
/// The `windows_subsystem = "windows"` attribute at the top of this file is
/// what stops Windows allocating a console for every launch; the cost is that
/// a GUI-subsystem process starts with no console and, unlike a
/// console-subsystem one, is *not* auto-attached to its parent's console when
/// that parent is an interactive shell. Left alone, running `iris.exe
/// --verbose` from PowerShell would print into the void exactly like a
/// double-click does. `AttachConsole(ATTACH_PARENT_PROCESS)` is the documented
/// fix: if the process that launched us owns a console, attach to it and
/// rebind stdout/stderr to `CONOUT$` so `println!`/`eprintln!` land there. If
/// there is no parent console — Explorer, the Start Menu, a Startup-folder
/// shortcut — the call fails and this does nothing, which is exactly the
/// silent, windowless launch the product requires; nothing here can make a
/// console appear where the parent had none.
///
/// Skipped, stream by stream, wherever stdout or stderr is already a real
/// handle: `iris.exe > out.txt`, `iris.exe 2> err.txt` and `iris.exe | more`
/// each hand the child valid, inherited handles for the redirected stream
/// through ordinary `CreateProcess` handle inheritance, regardless of
/// subsystem, and attaching here would tear one of those up and replace it
/// with the console instead. The two streams are judged independently and
/// only the unwired one is rebound — `iris.exe 2> err.txt` must still leave
/// the redirected stderr handle alone, and `iris.exe --verbose > out.txt`
/// must still attach and wire up stderr so `--verbose` diagnostics are not
/// silently dropped. Must run before the first `println!`/`eprintln!` —
/// including clap's own `--help`/error output — so it is the first thing
/// `main` does.
#[cfg(windows)]
fn attach_console_for_cli_output() {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    // SAFETY: querying our own current standard handles; no preconditions.
    let stdout_wired = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.is_ok();
    let stderr_wired = unsafe { GetStdHandle(STD_ERROR_HANDLE) }.is_ok();
    if stdout_wired && stderr_wired {
        return;
    }
    // SAFETY: no preconditions beyond a valid process; failure (no parent
    // console) is the expected, silent case for a double-click launch.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_err() {
        return;
    }

    let conout = HSTRING::from("CONOUT$");
    // SAFETY: `conout` is a NUL-terminated wide string that outlives the call.
    let Ok(console) = (unsafe {
        CreateFileW(
            &conout,
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }) else {
        return;
    };

    // SAFETY: `console` is a just-opened, valid handle. Rust's stdout/stderr
    // ask `GetStdHandle` again on every write rather than caching a handle
    // grabbed at process start, so this takes effect immediately. Only the
    // stream that was not already wired is rebound, so an inherited
    // redirection on the other stream survives untouched.
    unsafe {
        if !stdout_wired {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, console);
        }
        if !stderr_wired {
            let _ = SetStdHandle(STD_ERROR_HANDLE, console);
        }
    }
}

/// Show the same "something went wrong" dialog for a panic as for a returned
/// error, so a crash during startup or the resident loop is never the silent
/// "double-click did nothing" a GUI-subsystem binary would otherwise produce.
///
/// Every fallible step already reaches [`report_startup_failure`] or
/// [`report_run_failure`] through `?`/`inspect_err`, but a panic unwinds past
/// all of that — the default hook only writes to stderr, which a
/// console-less launch has nowhere to send. This chains onto the default
/// hook (so a real terminal invocation, or a test harness, still sees the
/// usual backtrace) and additionally calls [`iris_app::dialog::show`], which
/// is already a no-op whenever stdout/stderr are shared with a live console
/// (`GetConsoleProcessList`) — the same test that keeps `report_failure` from
/// popping up over a terminal invocation. Portable and unconditional (not
/// `#[cfg(windows)]`): `dialog::show` is itself the no-op off Windows, so
/// there is nothing platform-specific left to gate here.
///
/// Not unit-tested: replacing the global panic hook is process-wide and would
/// fight the test harness's own; the same gap `attach_console_for_cli_output`
/// and `report_failure` leave for the same reason.
fn install_panic_dialog() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        iris_app::dialog::show("Iris has stopped unexpectedly", &info.to_string());
    }));
}

/// Put a failed startup in front of the user when stderr has nowhere to land.
///
/// Covers the whole of starting up, in two calls that are each a whole phase
/// rather than a single fallible step: `Config::load_or_create` in `main`, and
/// [`start_resident`]. See [`report_failure`] for why a windowed binary needs
/// a dialog at all, and [`report_run_failure`] for the one way the loop itself
/// can end that needs the same treatment.
#[cfg(windows)]
fn report_startup_failure(err: &anyhow::Error) {
    report_failure("Iris could not start", err);
}

/// The same dialog for the resident loop giving up.
///
/// [`App::run`] returns `Err` in exactly one case — the hotkey channel closing,
/// which means the low-level hook is gone and there is no way left to dictate.
/// Windows uninstalls that hook itself if a callback ever runs long, so this is
/// a real end state and not only a bug path. Without the dialog it reads as Iris
/// silently disappearing partway through a session, which is precisely what
/// [`report_startup_failure`] exists to prevent one screen earlier.
#[cfg(windows)]
fn report_run_failure(err: &anyhow::Error) {
    report_failure("Iris has stopped", err);
}

/// Show `err` in a message box, unless someone is watching a console that
/// will outlive the process.
///
/// `iris` is a GUI-subsystem binary (see the `windows_subsystem` attribute at
/// the top of this file), so launching it from the Start Menu, the Startup
/// folder, or a plain double-click gives it no console at all — nothing for
/// `eprintln!` to reach. A missing key, an unparseable config file, a
/// microphone that is not there, a hook Windows took away: each is an
/// actionable sentence, and on that launch path each would otherwise vanish
/// with nothing on screen to say Iris failed. When a real terminal invocation
/// reattached a console via [`attach_console_for_cli_output`], the message
/// survives on stderr there instead and this dialog would be redundant.
///
/// A thin wrapper over `iris_app::dialog::show`, which lives in the library
/// crate rather than here so `App`'s injection-failure notice
/// (`iris_app::notify`) can show the same kind of dialog without a second
/// `MessageBoxW`/`GetConsoleProcessList` copy; see that module's doc comment
/// for the console-sharing check both paths share.
#[cfg(windows)]
fn report_failure(caption: &str, err: &anyhow::Error) {
    iris_app::dialog::show(caption, &format!("{err:#}"));
}

/// Off Windows there is no console-less launch path and no dialog to show, so
/// the caller in `main` keeps its unconditional shape and this does nothing.
#[cfg(not(windows))]
fn report_startup_failure(_err: &anyhow::Error) {}

#[cfg(windows)]
fn banner<A: AudioSource>(app: &App<A>, config_path: &std::path::Path) {
    let config = app.config();
    println!("Iris is running. Hold {} and speak.", config.hotkey);
    println!("Listening on {}.", app.describe_audio());
    println!(
        "Right-click the tray icon for settings ({}).",
        config_path.display()
    );

    // The full resolved-setting dump only serves a developer, so it moves
    // behind --verbose rather than greeting every user on launch.
    iris_core::vlog!("engine      {}", config.engine);
    iris_core::vlog!(
        "polish      {}",
        if config.polish.enabled {
            format!("on, {} ms budget", config.polish.budget_ms)
        } else {
            "off".into()
        }
    );
    iris_core::vlog!("inject      {}", config.inject.method);
}

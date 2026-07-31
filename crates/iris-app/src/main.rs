//! `iris` — the resident dictation app.
//!
//! Startup order matters and is deliberate:
//!
//! 1. read the config, apply CLI overrides, **promote keys to the environment**
//!    (safe only while the process is still single-threaded);
//! 2. build the engine, which is the one step that can fail for a reason the
//!    user must act on (a missing key);
//! 3. open the microphone, install the hotkey hook, start the tray;
//! 4. hand everything to [`iris_app::App`] and never touch it again.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
#[cfg(windows)]
use iris_app::audio::AudioSource;
use iris_app::config::{self, Config, EngineChoice};
use iris_app::inject::{DryRunInjector, Injector};
use iris_app::pill::{overlay_theme, LogPill, NoopPill, OverlayPill, PillSink};
use iris_app::{App, SessionLog};
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
}

fn main() -> Result<()> {
    let args = Args::parse();
    iris_core::log::set_verbose(args.verbose);

    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    // Kept as loaded: `config` below takes the CLI overrides, which are
    // run-only and must never be written back to the file.
    let file_config = Config::load_or_create(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
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
fn pill_for(args: &Args, overlay: Option<&iris_overlay::Overlay>) -> Box<dyn PillSink> {
    if let Some(overlay) = overlay {
        return Box::new(OverlayPill::new(overlay.handle()));
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

/// The resident loop.
#[cfg(windows)]
fn run(
    config: Config,
    file_config: Config,
    config_path: &std::path::Path,
    args: &Args,
) -> Result<()> {
    use iris_app::audio::MicAudio;
    use iris_app::inject::SystemInjector;
    use iris_app::tray;

    let audio = MicAudio::new(config.audio.device.clone(), config.audio.warm)?;
    let injector: Arc<dyn Injector> = if args.dry_run {
        Arc::new(DryRunInjector)
    } else {
        Arc::new(SystemInjector::new(config.inject.method, config.hotkey))
    };

    let devices = iris_core::capture::list_devices()
        .map(|d| d.into_iter().map(|d| d.name).collect())
        .unwrap_or_default();
    let (_tray, commands) = tray::spawn(&config, devices)?;

    // Held for the life of the loop; dropping it uninstalls the hook.
    let (_listener, keys) = iris_core::hotkey::listen(config.hotkey, config.suppress_hotkey)
        .context("installing the push-to-talk hook")?;

    // Overlay owns its thread for process life; App drives it via OverlayPill.
    let overlay = try_spawn_overlay(&config);
    let pill = pill_for(args, overlay.as_ref());

    let mut app = App::new(config, config_path, audio, injector, pill)?
        .with_report(args.report)
        .with_file_config(file_config);
    banner(&app, config_path);
    let result = app.run(&keys, &commands);
    // Explicit shutdown so the window is gone before we exit.
    if let Some(overlay) = overlay {
        overlay.shutdown();
    }
    result
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
    let pill = pill_for(args, overlay.as_ref());

    let audio = ChannelAudio::new();
    let frames_tx = audio.sender();
    let armed = audio.armed();
    let mut app = App::new(config, config_path, audio, injector, pill)?.with_report(true);
    let frames = app.frames();

    let (keys_tx, keys) = crossbeam_channel::unbounded();
    let pressed_at = std::time::Instant::now();
    let feeder = std::thread::spawn(move || {
        // Wait for the loop's stale-frame drain (which precedes arming), so no
        // utterance frame can be discarded as pre-key-press audio.
        if armed.recv().is_err() {
            return;
        }
        for chunk in pcm.chunks(iris_core::audio::FRAME_SAMPLES) {
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
    let pill = pill_for(args, overlay.as_ref());

    let audio = ChannelAudio::new();
    let frames_tx = audio.sender();
    let armed = audio.armed();
    let mut app = App::new(config, config_path, audio, injector, pill)?.with_report(true);
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

#[cfg(windows)]
fn banner<A: AudioSource>(app: &App<A>, config_path: &std::path::Path) {
    let config = app.config();
    println!("iris");
    println!("  engine      {}", config.engine);
    println!("  hotkey      {} (hold to talk)", config.hotkey);
    println!(
        "  polish      {}",
        if config.polish.enabled {
            format!("on, {} ms budget", config.polish.budget_ms)
        } else {
            "off".into()
        }
    );
    println!("  inject      {}", config.inject.method);
    println!("  microphone  {}", app.describe_audio());
    println!("  settings    {}", config_path.display());
    println!(
        "\n  Right-click the tray icon for settings. Hold {} and speak.",
        config.hotkey
    );
}

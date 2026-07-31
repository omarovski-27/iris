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
use iris_app::pill::{LogPill, NoopPill, PillSink};
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
}

fn main() -> Result<()> {
    let args = Args::parse();
    iris_core::log::set_verbose(args.verbose);

    let config_path = args.config.clone().unwrap_or_else(config::default_path);
    let mut config = Config::load_or_create(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
    apply_overrides(&mut config, &args);

    // Before any thread exists: see Config::promote_keys.
    for var in config.promote_keys() {
        iris_core::vlog!("using the {var} key from {}", config_path.display());
    }

    if args.print_config {
        println!("{}", config.to_toml()?);
        println!("# path: {}", config_path.display());
        return Ok(());
    }
    if args.list_devices {
        return list_devices();
    }
    if let Some(n) = args.history {
        return print_history(&config, &config_path, n);
    }
    if let Some(wav) = args.speak_wav.clone() {
        return speak_wav(config, &config_path, &args, &wav);
    }

    run(config, &config_path, &args)
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

fn pill_for(args: &Args) -> Box<dyn PillSink> {
    // Until `iris-overlay` merges there is nothing to show; --verbose at least
    // makes the state machine visible.
    if args.verbose {
        Box::new(LogPill)
    } else {
        Box::new(NoopPill)
    }
}

/// The resident loop.
#[cfg(windows)]
fn run(config: Config, config_path: &std::path::Path, args: &Args) -> Result<()> {
    use iris_app::audio::MicAudio;
    use iris_app::inject::SystemInjector;
    use iris_app::tray;

    let audio = MicAudio::new(config.audio.device.clone(), config.audio.warm)?;
    let injector: Arc<dyn Injector> = if args.dry_run {
        Arc::new(DryRunInjector)
    } else {
        Arc::new(SystemInjector::new(config.inject.method))
    };

    let devices = iris_core::capture::list_devices()
        .map(|d| d.into_iter().map(|d| d.name).collect())
        .unwrap_or_default();
    let (_tray, commands) = tray::spawn(&config, devices)?;

    // Held for the life of the loop; dropping it uninstalls the hook.
    let (_listener, keys) = iris_core::hotkey::listen(config.hotkey, config.suppress_hotkey)
        .context("installing the push-to-talk hook")?;

    let mut app =
        App::new(config, config_path, audio, injector, pill_for(args))?.with_report(args.report);
    banner(&app, config_path);
    app.run(&keys, &commands)
}

/// There is no hotkey, no microphone and no injection off Windows, so the
/// resident loop cannot do anything useful. Everything else — config, engines,
/// polish, the session log, `--speak-wav` — works, which is what keeps this
/// crate testable in CI.
#[cfg(not(windows))]
fn run(_config: Config, _config_path: &std::path::Path, _args: &Args) -> Result<()> {
    anyhow::bail!(
        "the resident loop needs Windows: the global hotkey hook, microphone capture and text \
         injection are all Win32.\nOn this host you can still use --speak-wav to run a dictation \
         from a file, --history, --list-devices and --print-config."
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
        true => Arc::new(iris_app::inject::SystemInjector::new(config.inject.method)),
        #[cfg(not(windows))]
        true => unreachable!("guarded above"),
        false => Arc::new(DryRunInjector),
    };

    let audio = ChannelAudio::new();
    let frames_tx = audio.sender();
    let mut app = App::new(config, config_path, audio, injector, pill_for(args))?.with_report(true);
    let frames = app.frames();

    let (keys_tx, keys) = crossbeam_channel::unbounded();
    let pressed_at = std::time::Instant::now();
    let feeder = std::thread::spawn(move || {
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
    println!("  {}", dictated.record.text);
    Ok(())
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

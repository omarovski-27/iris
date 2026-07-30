//! `iris-spike` — the end-to-end latency spike.
//!
//! Hold the hotkey, speak, release: the transcript is typed into whatever window
//! has focus, followed by a latency breakdown. See `crates/iris-spike/README.md`
//! for how to run it and how to read the numbers.

use anyhow::Result;
use clap::Parser;
use iris_core::engine::{EngineOptions, EngineSpec};
use iris_core::hotkey::Key;
use iris_core::inject::Method;

#[cfg(windows)]
mod pipeline;
#[cfg(windows)]
mod selftest;

#[derive(Parser, Debug)]
#[command(
    name = "iris-spike",
    about = "Push-to-talk dictation spike: hold a key, speak, release, text appears.",
    long_about = None,
    version
)]
pub struct Args {
    /// Transcription engine. `mock` needs no key and no network.
    #[arg(long, default_value = "mock")]
    pub engine: EngineSpec,

    /// Push-to-talk key.
    #[arg(long, default_value = "rctrl")]
    pub hotkey: Key,

    /// How to deliver text to the focused window.
    #[arg(long, default_value = "sendinput")]
    pub inject: Method,

    /// Let the hotkey through to the focused application as well.
    ///
    /// Off by default: holding right-Ctrl would otherwise arm every Ctrl
    /// shortcut in the app you are dictating into.
    #[arg(long)]
    pub no_suppress: bool,

    /// Input device name (substring match). Defaults to the Windows default.
    #[arg(long)]
    pub device: Option<String>,

    /// Keep the microphone stream open between dictations.
    ///
    /// Removes device-open time from the key-press path, at the cost of holding
    /// the mic (and its indicator light) the whole time.
    #[arg(long)]
    pub warm_capture: bool,

    /// Print the transcript instead of injecting it.
    #[arg(long)]
    pub dry_run: bool,

    /// Append a space after each transcript, so consecutive dictations do not
    /// run together.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub trailing_space: bool,

    /// Override the engine's default model.
    #[arg(long)]
    pub model: Option<String>,

    /// Language hint, e.g. `en`.
    #[arg(long)]
    pub language: Option<String>,

    /// Write each dictation's audio to this directory, for debugging.
    #[arg(long)]
    pub save_wav: Option<std::path::PathBuf>,

    /// List input devices and exit.
    #[arg(long)]
    pub list_devices: bool,

    /// Run the non-interactive checks (devices, hook, engine) and exit.
    #[arg(long)]
    pub self_test: bool,

    /// Include the text-injection checks in `--self-test`.
    ///
    /// Off by default, and deliberately so: Windows only delivers synthetic
    /// keystrokes on the desktop the user is looking at, so this check types
    /// into whatever session is live. Only pass it when you are at the machine
    /// and expect text to appear.
    #[arg(long)]
    pub injection_test: bool,

    /// Diagnostics on stderr.
    #[arg(long, short)]
    pub verbose: bool,
}

impl Args {
    pub fn engine_options(&self) -> EngineOptions {
        EngineOptions {
            model: self.model.clone(),
            language: self.language.clone(),
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    iris_core::log::set_verbose(args.verbose);
    run(args)
}

#[cfg(windows)]
fn run(args: Args) -> Result<()> {
    if args.list_devices {
        return pipeline::list_devices();
    }
    if args.self_test {
        return selftest::run(&args);
    }
    pipeline::run(&args)
}

#[cfg(not(windows))]
fn run(_args: Args) -> Result<()> {
    anyhow::bail!(
        "iris-spike drives Windows APIs (WASAPI capture, a low-level keyboard hook and \
         SendInput), so it only runs on Windows.\n\
         From WSL, build it and run the .exe directly — Windows interop executes it as a real \
         Windows process:\n    \
         cargo build --release --target x86_64-pc-windows-gnu\n    \
         ./target/x86_64-pc-windows-gnu/release/iris-spike.exe\n\
         See docs/dev-windows.md.\n\n\
         To measure latency without a microphone on any host, use the harness instead:\n    \
         cargo run --release --bin iris-harness -- --engine mock"
    )
}

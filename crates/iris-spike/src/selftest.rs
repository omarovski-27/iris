//! Non-interactive verification, for when there is nobody to hold the key.
//!
//! Checks, in order:
//!
//! 1. audio devices enumerate,
//! 2. the low-level keyboard hook installs and uninstalls,
//! 3. the engine path produces a transcript from the committed WAV fixture.
//!
//! Text injection is **not** among them, and cannot be: Windows delivers
//! synthetic keystrokes only on the *input* desktop — the one the user is
//! looking at — and `SendInput` returns `ERROR_ACCESS_DENIED` from any other,
//! including a purpose-created private one (this was tried, and measured).
//! Any automated injection check therefore types into somebody's live session,
//! which disrupted real work once during this spike. Injection is verified by
//! the interactive checklist in this crate's README instead; what can be
//! tested without the OS (UTF-16, surrogate pairs, control characters) is
//! unit-tested in `iris-core/src/text.rs`.

use std::time::Duration;

use anyhow::{Context, Result};
use iris_core::dictation::{run_offline, Pace};
use iris_core::engine::{self, EngineSpec};
use iris_core::{audio, capture, hotkey};

use crate::Args;

pub fn run(args: &Args) -> Result<()> {
    println!("iris-spike self-test\n");
    let mut failures = 0;

    failures += report("audio devices enumerate", check_devices());
    failures += report("keyboard hook installs", check_hook(args));
    failures += report("engine path transcribes the fixture", check_engine(args));

    // Never checked automatically. Synthetic keystrokes are only delivered on
    // the desktop the user is actually looking at (Windows returns
    // ERROR_ACCESS_DENIED anywhere else), so there is no way to exercise
    // SendInput without typing into somebody's live session. During this spike
    // an automated version of this check did exactly that and interrupted real
    // work. Injection is verified by the interactive checklist in the spike
    // README instead.
    report(
        "text injection",
        Ok(Outcome::Skip(
            "cannot be tested unattended — use the interactive checklist in the README".into(),
        )),
    );

    println!();
    if failures == 0 {
        println!("All checks passed.");
        Ok(())
    } else {
        anyhow::bail!("{failures} check(s) failed")
    }
}

/// A check either passes with a note, is skipped with a reason, or fails.
enum Outcome {
    Pass(String),
    Skip(String),
}

fn report(name: &str, result: Result<Outcome>) -> usize {
    match result {
        Ok(Outcome::Pass(note)) => {
            println!("  PASS  {name:<40} {note}");
            0
        }
        Ok(Outcome::Skip(why)) => {
            println!("  SKIP  {name:<40} {why}");
            0
        }
        Err(e) => {
            println!("  FAIL  {name:<40} {e:#}");
            1
        }
    }
}

fn check_devices() -> Result<Outcome> {
    let devices = capture::list_devices()?;
    if devices.is_empty() {
        anyhow::bail!("no input devices");
    }
    let default = devices
        .iter()
        .find(|d| d.default)
        .map(|d| d.name.as_str())
        .unwrap_or("<none marked default>");
    Ok(Outcome::Pass(format!(
        "{} device(s), default: {default}",
        devices.len()
    )))
}

fn check_hook(args: &Args) -> Result<Outcome> {
    let (listener, _rx) =
        hotkey::listen(args.hotkey, !args.no_suppress).context("SetWindowsHookExW")?;
    // Long enough that a hook rejected on install has surfaced.
    std::thread::sleep(Duration::from_millis(50));
    listener.stop();
    Ok(Outcome::Pass(format!("{} armed and released", args.hotkey)))
}

fn check_engine(args: &Args) -> Result<Outcome> {
    let engine = engine::build(args.engine, &args.engine_options())?;
    let pcm = audio::read_wav(iris_core::sample_wav_path()).context("reading the WAV fixture")?;
    // `Fast` because this is a correctness check, not a latency measurement —
    // that is what iris-harness is for.
    let outcome = run_offline(&*engine, &pcm, Pace::Fast, &mut |_| {})?;
    if outcome.text.trim().is_empty() {
        anyhow::bail!("engine returned an empty transcript");
    }
    let note = if args.engine == EngineSpec::Mock {
        format!(
            "{:.1}s of audio -> {} chars",
            outcome.timeline.audio_secs,
            outcome.text.len()
        )
    } else {
        format!("{:?}", truncate(&outcome.text, 40))
    };
    Ok(Outcome::Pass(note))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

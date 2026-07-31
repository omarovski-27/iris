//! The live Windows pipeline: hotkey → microphone → engine → focused window.
//!
//! # Threading
//!
//! Three threads, and the split is deliberate:
//!
//! * **Hotkey thread** (owned by [`iris_core::hotkey`]) does nothing but pump
//!   Windows messages, because a low-level hook that stalls gets uninstalled.
//! * **Audio thread** (owned by WASAPI, via cpal) resamples and hands frames to
//!   a channel. Never blocks.
//! * **This thread** owns the engine session, the timeline and injection. It
//!   blocks only in `select!`, so a frame or a key release is picked up the
//!   moment it happens.
//!
//! Nothing shares a lock, so no part of the latency budget is spent waiting on
//! another part of the program.

use anyhow::{Context, Result};
use crossbeam_channel::{select, Receiver, Sender};
use iris_core::audio;
use iris_core::capture::{self, Capture};
use iris_core::dictation::{Dictation, DEFAULT_FINAL_TIMEOUT};
use iris_core::engine::{self, Engine};
use iris_core::hotkey::{self, HotkeyEvent};
use iris_core::latency::Mark;
use iris_core::{inject, text, vlog};

use crate::Args;

pub fn list_devices() -> Result<()> {
    let devices = capture::list_devices()?;
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
    println!("\n  * = default. Select another with --device <substring>.");
    Ok(())
}

pub fn run(args: &Args) -> Result<()> {
    let engine = engine::build(args.engine, &args.engine_options())?;
    // Held for the lifetime of the loop; dropping it unhooks.
    let (_listener, keys) = hotkey::listen(args.hotkey, !args.no_suppress)
        .context("installing the push-to-talk hook")?;

    let (audio_tx, audio_rx) = crossbeam_channel::unbounded::<Vec<i16>>();
    let warm = if args.warm_capture {
        Some(open_capture(args, audio_tx.clone())?)
    } else {
        None
    };

    banner(args, &*engine, warm.as_ref());

    let mut count = 0usize;
    loop {
        // Idle. Discard anything the warm stream captured while we were not
        // dictating, so a session never starts with stale audio.
        let event = select! {
            recv(keys) -> ev => ev.context("the hotkey thread stopped")?,
            recv(audio_rx) -> _ => continue,
        };
        let HotkeyEvent::Down(pressed_at) = event else {
            continue;
        };

        count += 1;
        let ctx = Dictating {
            args,
            engine: &*engine,
            index: count,
            keys: &keys,
            audio_rx: &audio_rx,
            audio_tx: &audio_tx,
            warm: warm.is_some(),
        };
        if let Err(e) = dictate(ctx, pressed_at) {
            eprintln!("\n  dictation failed: {e:#}\n");
        }
        println!("Ready. Hold {} and speak.", args.hotkey);
    }
}

/// Everything one dictation needs from the outer loop.
struct Dictating<'a> {
    args: &'a Args,
    engine: &'a dyn Engine,
    index: usize,
    keys: &'a Receiver<HotkeyEvent>,
    audio_rx: &'a Receiver<Vec<i16>>,
    audio_tx: &'a Sender<Vec<i16>>,
    /// True when the microphone stream is already open and stays open.
    warm: bool,
}

fn dictate(ctx: Dictating<'_>, pressed_at: std::time::Instant) -> Result<()> {
    let Dictating {
        args,
        engine,
        index,
        keys,
        audio_rx,
        audio_tx,
        warm,
    } = ctx;

    // Open the engine session first: for a streaming engine this kicks off the
    // websocket handshake, which then overlaps with everything below.
    let mut dictation = Dictation::start_at(engine, pressed_at)?;

    // Dropped at the end of the dictation, which closes the device.
    let _cold = if warm {
        None
    } else {
        Some(open_capture(args, audio_tx.clone())?)
    };

    let mut recorded: Vec<i16> = Vec::new();
    let mut on_partial = |text: &str| {
        // \r keeps the line updating in place: this is the seam where the real
        // app's overlay renders instead.
        print!("\r  ~ {text:<78.78}");
        flush();
    };

    // Selecting over all three sources means nothing is ever waiting on a poll
    // tick: a frame, a transcript and the key release are each acted on the
    // moment they land.
    let events = dictation.events();
    let released_at = loop {
        select! {
            recv(audio_rx) -> frame => {
                let frame = frame.context("the audio thread stopped")?;
                if args.save_wav.is_some() {
                    recorded.extend_from_slice(&frame);
                }
                dictation.feed(&frame)?;
            }
            recv(events) -> ev => {
                match ev {
                    Ok(ev) => dictation.absorb_event(ev, &mut on_partial),
                    // The engine hung up; finish() turns that into a message.
                    Err(_) => break std::time::Instant::now(),
                }
            }
            recv(keys) -> ev => {
                if let HotkeyEvent::Up(at) = ev.context("the hotkey thread stopped")? {
                    break at;
                }
            }
        }
    };

    // Everything the device buffered but had not delivered yet is still the
    // user's speech; dropping it would truncate the last word.
    let mut tail = Vec::new();
    capture::drain(audio_rx, &mut tail);
    if !tail.is_empty() {
        if args.save_wav.is_some() {
            recorded.extend_from_slice(&tail);
        }
        dictation.feed(&tail)?;
    }

    dictation.timeline_mut().mark_at(Mark::KeyUp, released_at);
    let outcome = dictation.finish(DEFAULT_FINAL_TIMEOUT, &mut on_partial)?;
    let mut timeline = outcome.timeline;

    let payload = text::prepare(&outcome.text, args.trailing_space);
    if args.dry_run {
        println!("\r  (dry run) would inject: {payload:?}{:<40}", "");
    } else {
        inject::inject(&payload, args.inject).context("injecting the transcript")?;
        timeline.mark(Mark::Injected);
        print!("\r{:<80}\r", "");
    }

    println!("{}", timeline.report(index));

    if let Some(dir) = &args.save_wav {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("iris-{index}.wav"));
        audio::write_wav(&path, &recorded)?;
        println!("  audio saved to {}", path.display());
    }
    Ok(())
}

fn open_capture(args: &Args, sink: Sender<Vec<i16>>) -> Result<Capture> {
    let capture = capture::start(args.device.as_deref(), sink)?;
    vlog!(
        "microphone: {} @ {} Hz x{}",
        capture.device_name,
        capture.input_rate,
        capture.input_channels
    );
    Ok(capture)
}

fn banner(args: &Args, engine: &dyn Engine, warm: Option<&Capture>) {
    println!("iris-spike");
    println!(
        "  engine     {} ({})",
        engine.name(),
        if engine.streams_partials() {
            "streams partials while you speak"
        } else {
            "batch: transcribes after you release"
        }
    );
    println!("  hotkey     {} (hold to talk)", args.hotkey);
    println!(
        "  inject     {}{}",
        args.inject,
        if args.dry_run { " (dry run)" } else { "" }
    );
    match warm {
        Some(c) => println!("  microphone {} (kept open)", c.device_name),
        None => println!("  microphone opened on key-press"),
    }
    println!("\n  Ctrl+C to quit.");
    println!("Ready. Hold {} and speak.", args.hotkey);
}

fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

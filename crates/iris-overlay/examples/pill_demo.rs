//! Drive the pill through its full state cycle with synthetic audio.
//!
//! ```bash
//! # On Windows (or from WSL, against the cross-compiled exe) — the real pill:
//! cargo run --example pill-demo
//!
//! # Anywhere, including Linux CI — a PNG filmstrip of the same frames:
//! cargo run --example pill-demo -- --filmstrip /tmp/iris-pill
//! ```
//!
//! See `crates/iris-overlay/README.md` for the WSL loop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use iris_overlay::{
    spawn, Command, HeadlessOverlay, OverlayConfig, OverlayState, Theme, PORCELAIN_LIGHT,
    PRISM_DARK,
};

/// One pass through hidden → listening → processing → inserted → hidden.
const CYCLE: Cycle = Cycle {
    listen_ms: 3_400,
    process_ms: 180,
    latency_ms: 142,
    settle_ms: 900,
};

struct Cycle {
    listen_ms: u64,
    process_ms: u64,
    latency_ms: u32,
    settle_ms: u64,
}

impl Cycle {
    /// Total wall time of one pass.
    fn total_ms(&self) -> u64 {
        self.listen_ms + self.process_ms + self.settle_ms
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "pill-demo",
    about = "Drive the Iris pill overlay through its states with synthetic audio."
)]
struct Args {
    /// Which palette to show.
    #[arg(long, value_parser = ["prism", "porcelain"], default_value = "prism")]
    theme: String,

    /// How many times to run the cycle. In live mode 0 runs until
    /// interrupted; filmstrip mode always writes at least one cycle.
    #[arg(long, default_value_t = 3)]
    cycles: u32,

    /// Engine label for the chip.
    #[arg(long, default_value = "groq · whisper-large-v3-turbo · en")]
    engine: String,

    /// Monitor scale to render at in filmstrip mode (1.0 = 100 %, 1.5 = 150 %).
    #[arg(long, default_value_t = 1.0)]
    scale: f32,

    /// Write PNG frames to this directory instead of opening a window.
    ///
    /// Works on every platform, which is how the pill gets reviewed from WSL.
    #[arg(long)]
    filmstrip: Option<PathBuf>,

    /// Milliseconds between filmstrip frames.
    #[arg(long, default_value_t = 40)]
    filmstrip_step: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let theme = match args.theme.as_str() {
        "porcelain" => PORCELAIN_LIGHT,
        _ => PRISM_DARK,
    };

    match &args.filmstrip {
        Some(dir) => filmstrip(&args, theme, dir.clone()),
        None => live(&args, theme),
    }
}

/// The real overlay: spawn a window and drive it in real time.
fn live(args: &Args, theme: Theme) -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(windows) {
        eprintln!(
            "This is not Windows, so there is no overlay window to show.\n\
             The state machine below is real; for the pixels, run:\n\
             \n    cargo run --example pill-demo -- --filmstrip /tmp/iris-pill\n"
        );
    }

    let overlay = spawn(OverlayConfig {
        theme,
        engine: args.engine.clone(),
    })?;
    let pill = overlay.handle();

    println!("theme: {}", theme.name);
    println!(
        "cycle: listen {} ms -> processing {} ms -> inserted (latency {} ms) -> hidden",
        CYCLE.listen_ms, CYCLE.process_ms, CYCLE.latency_ms
    );

    let mut cycle = 0u32;
    while args.cycles == 0 || cycle < args.cycles {
        cycle += 1;
        println!("[{cycle}] listening");
        pill.show_listening();

        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(CYCLE.listen_ms) {
            let t = start.elapsed().as_millis() as u64;
            pill.update_level(synthetic_level(t));
            pill.set_partial_len(spoken_chars(t));
            std::thread::sleep(Duration::from_millis(16));
        }

        println!("[{cycle}] processing");
        pill.processing();
        std::thread::sleep(Duration::from_millis(CYCLE.process_ms));

        println!("[{cycle}] inserted ({} ms)", CYCLE.latency_ms);
        pill.inserted(CYCLE.latency_ms);
        // The pill hides itself; wait long enough to watch it go.
        std::thread::sleep(Duration::from_millis(CYCLE.settle_ms));

        // The overlay thread gives up if the surface stops accepting frames.
        // Without this the demo would print a happy transcript of a cycle
        // nobody could see.
        if !pill.is_connected() {
            return Err("the overlay thread stopped: no frames reached the screen".into());
        }
    }

    println!("done — the overlay was still presenting frames at the end");
    overlay.shutdown();
    Ok(())
}

/// The same cycle, rendered to PNGs with no window.
fn filmstrip(args: &Args, theme: Theme, dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&dir)?;

    let mut pill = HeadlessOverlay::new(args.scale, theme);
    pill.apply(Command::Engine(args.engine.clone()));
    pill.tick(0);

    let step = args.filmstrip_step.max(1);
    let cycles = args.cycles.max(1);
    let mut now = 0u64;
    let mut frame = 0u32;
    let mut written = 0u32;

    for cycle in 0..cycles {
        let base = now;
        pill.apply(Command::ShowListening);
        // Thresholds rather than equalities: the frame step will not land
        // exactly on a phase boundary, and an `==` would silently skip it.
        let (mut said_processing, mut said_inserted) = (false, false);
        loop {
            let elapsed = now - base;
            if elapsed >= CYCLE.listen_ms + CYCLE.process_ms && !said_inserted {
                said_inserted = true;
                pill.apply(Command::Inserted {
                    latency_ms: CYCLE.latency_ms,
                });
            } else if elapsed >= CYCLE.listen_ms && !said_processing {
                said_processing = true;
                pill.apply(Command::Processing);
            } else if elapsed < CYCLE.listen_ms {
                pill.apply(Command::Level(synthetic_level(elapsed)));
                pill.apply(Command::PartialLen(spoken_chars(elapsed)));
            }
            pill.tick(now);
            pill.render();

            let path = dir.join(format!("{frame:04}-{}.png", label(pill.state())));
            pill.write_png(&path)?;
            written += 1;
            frame += 1;

            now += step;
            if now - base >= CYCLE.total_ms() {
                break;
            }
        }
        if cycle + 1 < cycles {
            pill.apply(Command::Hide);
        }
    }

    println!(
        "{written} frames -> {} ({} x {} px, {} @ {:.0} %)",
        dir.display(),
        pill.frame().width,
        pill.frame().height,
        theme.name,
        args.scale * 100.0
    );
    println!("Open them in order; each filename carries the state it was in.");
    Ok(())
}

fn label(state: OverlayState) -> &'static str {
    match state {
        OverlayState::Hidden => "hidden",
        OverlayState::Listening => "listening",
        OverlayState::Processing => "processing",
        OverlayState::Inserted => "inserted",
    }
}

/// A speech-shaped envelope: syllables riding on a phrase-length swell, with a
/// short fade-in so the waveform does not slam open on the first frame.
///
/// This is the mockup's `envelope()` function, transcribed. It exists so the
/// demo looks like someone talking rather than like a test tone.
fn synthetic_level(ms: u64) -> f32 {
    let s = ms as f32 / 1000.0;
    let syllables = 0.54 + 0.44 * (s * 6.4).sin() + 0.17 * (s * 12.1 + 1.4).sin();
    let phrase = 0.6 + 0.4 * (s * 1.2 + 0.5).sin();
    let attack = (ms as f32 / 100.0).min(1.0);
    (syllables * phrase).clamp(0.10, 1.0) * attack
}

/// Roughly how many characters a person has said by `ms`, at ~13 chars/second.
fn spoken_chars(ms: u64) -> usize {
    (ms / 75) as usize
}

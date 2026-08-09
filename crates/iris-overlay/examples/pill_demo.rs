//! Drive the pill through its full state cycle with synthetic audio and a
//! scripted utterance.
//!
//! ```bash
//! # On Windows (or from WSL, against the cross-compiled exe) — the real pill:
//! cargo run --example pill-demo
//!
//! # Anywhere, including Linux CI — a PNG filmstrip of the same frames:
//! cargo run --example pill-demo -- --filmstrip /tmp/iris-pill
//!
//! # The committed review set, regenerated in place:
//! cargo run --example pill-demo -- --evidence crates/iris-overlay/docs/round5-evidence
//! ```
//!
//! See `crates/iris-overlay/README.md` for the WSL loop.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use iris_overlay::{
    spawn, Command, HeadlessOverlay, Layout, OverlayConfig, OverlayState, Theme, PORCELAIN_LIGHT,
    PRISM_DARK,
};
use tiny_skia::{Pixmap, PremultipliedColorU8};

/// A short utterance that fits comfortably inside the ribbon's max width —
/// the common case, no scrolling.
const SHORT_UTTERANCE: &str = "set a reminder for tomorrow";
/// A long utterance, chosen specifically to overflow the ribbon and exercise
/// the marquee-tail scroll (oldest words drop off the left, newest stay
/// pinned against the right padding).
const LONG_UTTERANCE: &str =
    "the quarterly report needs three more charts before it goes to the board on friday morning";

/// Word-by-word reveal cadence, modelling perceived speech rate (~150 wpm).
const WORD_MS: u64 = 380;
/// Silence before the first partial arrives, matching `docs/spike-findings.md`'s
/// modelled "first audio -> first partial" cost (~340 ms) with a little air so
/// the low/high-level resting-capsule frames read clearly before words start.
const FIRST_WORD_AT_MS: u64 = 950;

struct Cycle {
    listen_ms: u64,
    process_ms: u64,
    latency_ms: u32,
    settle_ms: u64,
}

impl Cycle {
    /// Long enough to reveal every word of `utterance` at [`WORD_MS`] plus a
    /// trailing buffer, whichever utterance was chosen.
    fn for_utterance(utterance: &str) -> Self {
        let words = utterance.split(' ').count() as u64;
        Self {
            listen_ms: FIRST_WORD_AT_MS + WORD_MS * words + 500,
            process_ms: 220,
            latency_ms: 142,
            settle_ms: 900,
        }
    }

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

    /// Engine label. Carried on the model but not rendered by this design —
    /// see `state::Command::Engine`.
    #[arg(long, default_value = "groq · whisper-large-v3-turbo · en")]
    engine: String,

    /// Which scripted utterance to speak.
    #[arg(long, value_parser = ["short", "long"], default_value = "long")]
    utterance: String,

    /// Whether the live transcript reaches the overlay at all.
    ///
    /// Defaults to `off`, matching `iris-app`'s shipped default
    /// (`Config::show_live_text = false`, round 3): no partial text reaches
    /// the shape, so it never widens past the resting capsule. Pass `on` to
    /// review the opt-in ribbon presentation instead.
    #[arg(long, value_parser = ["on", "off"], default_value = "off")]
    live_text: String,

    /// Monitor scale to render at in filmstrip and evidence modes (1.0 =
    /// 100 %, 1.5 = 150 %).
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

    /// Hold the microphone level at this value (0.0–1.0) instead of running
    /// the synthetic speech envelope. Live and filmstrip modes only;
    /// `--evidence` holds its own levels and rejects this flag.
    ///
    /// The envelope oscillates, so two arbitrary frames of it say nothing
    /// about whether the wave row answers volume at all — every frame of a
    /// review pass can land near a quiet moment by chance. A held level, given
    /// long enough for the one-pole level smoothing to settle, makes a quiet
    /// frame and a loud one directly comparable.
    #[arg(long, value_parser = unit_level)]
    hold_level: Option<f32>,

    /// Mark the listening session as a hands-free latch
    /// (`iris-app`'s double-tap feature), sent right after `ShowListening`.
    /// Live and filmstrip modes only; `--evidence` holds its own state and
    /// rejects this flag.
    #[arg(long)]
    latched: bool,

    /// Composite each written PNG over a synthetic busy desktop instead of
    /// leaving it transparent — in filmstrip and evidence modes.
    ///
    /// The shape is glass: on a transparent PNG (or a viewer's checkerboard)
    /// there is nothing behind it to refract, which is most of what there is
    /// to review. See [`backdrop_pixmap`]. Live mode writes no PNG and has a
    /// real desktop behind the window already, so there is nothing for this
    /// to stand in for there.
    #[arg(long)]
    backdrop: bool,

    /// Write the committed round-5 review set — the exact files under
    /// `docs/round5-evidence/` — to this directory. Implies `--backdrop`.
    ///
    /// The set is a fixed shot list: it picks its own themes, levels, phases
    /// and utterance, and honours only `--scale`. Every flag it would
    /// otherwise swallow is rejected rather than ignored, so a regeneration
    /// run cannot silently produce something other than what was asked for.
    #[arg(
        long,
        conflicts_with_all = [
            "theme", "utterance", "live_text", "hold_level", "engine",
            "cycles", "filmstrip", "filmstrip_step", "latched",
        ]
    )]
    evidence: Option<PathBuf>,
}

/// Parse a microphone level, rejecting anything outside 0.0–1.0 at the
/// command line rather than letting `Command::Level` quietly clamp it: a
/// `--hold-level 50` meaning "50 %" would otherwise render as a silent full
/// deflection and be read as the wave row ignoring the flag.
fn unit_level(raw: &str) -> Result<f32, String> {
    let v: f32 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number"))?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(format!("level must be between 0.0 and 1.0, got {v}"))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let theme = match args.theme.as_str() {
        "porcelain" => PORCELAIN_LIGHT,
        _ => PRISM_DARK,
    };
    let utterance = match args.utterance.as_str() {
        "short" => SHORT_UTTERANCE,
        _ => LONG_UTTERANCE,
    };
    let cycle = Cycle::for_utterance(utterance);

    let live_text = args.live_text == "on";

    match (&args.evidence, &args.filmstrip) {
        (Some(dir), _) => evidence(&args, dir.clone()),
        (None, Some(dir)) => filmstrip(&args, theme, utterance, live_text, &cycle, dir.clone()),
        (None, None) => live(&args, theme, utterance, live_text, &cycle),
    }
}

/// The real overlay: spawn a window and drive it in real time.
fn live(
    args: &Args,
    theme: Theme,
    utterance: &str,
    live_text: bool,
    cycle: &Cycle,
) -> Result<(), Box<dyn std::error::Error>> {
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
    if let Some(level) = args.hold_level {
        println!("level: held at {level:.2}, not the synthetic speech envelope");
    }
    println!(
        "cycle: listen {} ms -> processing {} ms -> inserted (latency {} ms) -> hidden",
        cycle.listen_ms, cycle.process_ms, cycle.latency_ms
    );

    let mut cycle_no = 0u32;
    while args.cycles == 0 || cycle_no < args.cycles {
        cycle_no += 1;
        println!(
            "[{cycle_no}] listening{}",
            if args.latched { " (latched)" } else { "" }
        );
        pill.show_listening();
        if args.latched {
            pill.set_latched(true);
        }

        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(cycle.listen_ms) {
            let t = start.elapsed().as_millis() as u64;
            pill.update_level(args.hold_level.unwrap_or_else(|| synthetic_level(t)));
            if live_text {
                pill.set_partial_text(words_said_by(t, utterance));
            }
            std::thread::sleep(Duration::from_millis(16));
        }

        println!("[{cycle_no}] processing");
        pill.processing();
        std::thread::sleep(Duration::from_millis(cycle.process_ms));

        println!("[{cycle_no}] inserted ({} ms)", cycle.latency_ms);
        pill.inserted(cycle.latency_ms);
        // The pill hides itself; wait long enough to watch it go.
        std::thread::sleep(Duration::from_millis(cycle.settle_ms));

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
fn filmstrip(
    args: &Args,
    theme: Theme,
    utterance: &str,
    live_text: bool,
    cycle: &Cycle,
    dir: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&dir)?;

    let mut pill = HeadlessOverlay::new(args.scale, theme);
    pill.apply(Command::Engine(args.engine.clone()));
    pill.tick(0);

    let step = args.filmstrip_step.max(1);
    let cycles = args.cycles.max(1);
    let mut now = 0u64;
    let mut frame = 0u32;
    let mut written = 0u32;

    for cycle_no in 0..cycles {
        let base = now;
        pill.apply(Command::ShowListening);
        if args.latched {
            pill.apply(Command::Latched(true));
        }
        // Thresholds rather than equalities: the frame step will not land
        // exactly on a phase boundary, and an `==` would silently skip it.
        let (mut said_processing, mut said_inserted) = (false, false);
        loop {
            let elapsed = now - base;
            if elapsed >= cycle.listen_ms + cycle.process_ms && !said_inserted {
                said_inserted = true;
                pill.apply(Command::Inserted {
                    latency_ms: cycle.latency_ms,
                });
            } else if elapsed >= cycle.listen_ms && !said_processing {
                said_processing = true;
                pill.apply(Command::Processing);
            } else if elapsed < cycle.listen_ms {
                pill.apply(Command::Level(
                    args.hold_level.unwrap_or_else(|| synthetic_level(elapsed)),
                ));
                if live_text {
                    pill.apply(Command::PartialText(words_said_by(elapsed, utterance)));
                }
            }
            pill.tick(now);
            pill.render();

            let path = dir.join(format!("{frame:04}-{}.png", label(pill.state())));
            write_frame(&pill, &path, args.backdrop)?;
            written += 1;
            frame += 1;

            now += step;
            if now - base >= cycle.total_ms() {
                break;
            }
        }
        if cycle_no + 1 < cycles {
            pill.apply(Command::Hide);
        }
    }

    println!(
        "{written} frames -> {} ({} x {} px, {} @ {:.0} %, {} utterance)",
        dir.display(),
        pill.frame().width,
        pill.frame().height,
        theme.name,
        args.scale * 100.0,
        args.utterance,
    );
    println!("Open them in order; each filename carries the state it was in.");
    Ok(())
}

/// The phase a single evidence frame is caught in.
#[derive(Clone, Copy)]
enum Phase {
    /// Still listening, at whatever level the shot asked for.
    Listening,
    /// A moment into processing: the spinner is up and the timer is frozen.
    Processing,
    /// Mid-check, before the pill starts leaving.
    Inserted,
}

/// How long each evidence shot listens before it is caught. Long enough to
/// put a readable `0:07` on the timer.
const EVIDENCE_LISTEN_MS: u64 = 7_000;

/// Drive a fresh pill to exactly one reviewable frame.
///
/// Ticks in real 16 ms steps rather than jumping the clock: `Renderer` carries
/// its own morph and smoothing state frame to frame, so a single tick to
/// `EVIDENCE_LISTEN_MS` would render a shape that never opened and a level
/// that never settled.
fn shot(
    theme: Theme,
    scale: f32,
    level: Option<f32>,
    phase: Phase,
    utterance: Option<&str>,
) -> HeadlessOverlay {
    let mut pill = HeadlessOverlay::new(scale, theme);
    pill.tick(0);
    pill.apply(Command::ShowListening);

    let mut now = 0;
    while now < EVIDENCE_LISTEN_MS {
        now += 16;
        pill.apply(Command::Level(
            level.unwrap_or_else(|| synthetic_level(now)),
        ));
        if let Some(utterance) = utterance {
            pill.apply(Command::PartialText(words_said_by(now, utterance)));
        }
        pill.tick(now);
        pill.render();
    }

    // Past STATE_CROSS_MS in both cases, so the frame is the state itself
    // rather than a crossfade out of the previous one; the inserted shot also
    // clears CHECK_DRAW_MS so the check is fully drawn.
    let settle = match phase {
        Phase::Listening => 0,
        Phase::Processing => 200,
        Phase::Inserted => 300,
    };
    match phase {
        Phase::Listening => {}
        Phase::Processing => {
            pill.apply(Command::Processing);
        }
        Phase::Inserted => {
            pill.apply(Command::Processing);
            pill.apply(Command::Inserted { latency_ms: 142 });
        }
    }
    let until = now + settle;
    while now < until {
        now += 16;
        pill.tick(now);
        pill.render();
    }
    pill.render();
    pill
}

/// Regenerate the committed round-5 review set.
///
/// Every file under `docs/round5-evidence/` comes out of here, which is the
/// point: the set is a backdrop-composited selection that a plain
/// `--filmstrip` run cannot produce, and evidence nobody can reproduce is
/// evidence nobody can check against a later change. The fixed single-frame
/// shots (`listening-natural`, `processing-frozen`, `inserted-frozen`) still
/// exist, but round 5's own requirement — the wave row shown as a *sequence*,
/// not a frozen frame — is [`wave_sequence_evidence`], appended per theme.
fn evidence(args: &Args, dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&dir)?;

    let shots: [(&str, Option<f32>, Phase); 3] = [
        ("listening-natural", None, Phase::Listening),
        ("processing-frozen", None, Phase::Processing),
        ("inserted-frozen", None, Phase::Inserted),
    ];

    let mut written = 0;
    for (theme_name, theme) in [("prism", PRISM_DARK), ("porcelain", PORCELAIN_LIGHT)] {
        for (name, level, phase) in shots {
            let pill = shot(theme, args.scale, level, phase, None);
            let path = dir.join(format!("{theme_name}-default-{name}.png"));
            write_frame(&pill, &path, true)?;
            written += 1;
        }
        written += wave_sequence_evidence(theme, args.scale, &dir, theme_name)?;
    }

    // The opt-in ribbon, for contrast: not the default any more, and the one
    // frame in the set where `theme.text_scrim` paints at all.
    let pill = shot(
        PRISM_DARK,
        args.scale,
        None,
        Phase::Listening,
        Some(LONG_UTTERANCE),
    );
    write_frame(&pill, &dir.join("prism-opt-in-livetext-open.png"), true)?;
    written += 1;

    println!("{written} evidence frames -> {}", dir.display());
    Ok(())
}

/// `WAVE_HISTORY_LEN * WAVE_SAMPLE_INTERVAL_MS` is private to the crate, so
/// this is that budget written out rather than imported: how far back a
/// still's visible row can possibly reach.
const WAVE_HISTORY_SPAN_MS: u64 = 2_800;

const SILENCE_MS: u64 = 3_000;
const RAMP_MS: u64 = 3_000;
const RAMP_END: u64 = SILENCE_MS + RAMP_MS;
/// Long enough that the speech phase has room for a genuinely quiet moment
/// and a genuinely loud one, each with a full [`WAVE_HISTORY_SPAN_MS`] of
/// scan room *after* `RAMP_END` so neither checkpoint's row still has ramp
/// samples bleeding in from the edge.
const SPEECH_MS: u64 = 6_000;
const SPEECH_END: u64 = RAMP_END + SPEECH_MS;
const DECAY_MS: u64 = 4_000;

/// Round 5's own required evidence
/// (`/home/omar/firstmate/data/iris-overlay-back-to-circle/round5-direction.md`):
/// "render the wave row across a sequence of level histories (silence,
/// ramp-up, speech-like varying, decay)... so the motion is judgeable from
/// stills" — a single frozen frame is exactly what round 3 and round 4 both
/// survived review on, and the direction calls that out by name as not
/// enough this time.
///
/// Four phases, run back to back through one continuous `ShowListening`:
/// near-silent, a smooth ramp to loud, [`synthetic_level`]'s speech-like
/// oscillation, and a decay back toward quiet.
fn evidence_level_profile(ms: u64) -> f32 {
    const QUIET: f32 = 0.04;
    const LOUD: f32 = 0.9;

    if ms < SILENCE_MS {
        QUIET
    } else if ms < RAMP_END {
        let t = (ms - SILENCE_MS) as f32 / RAMP_MS as f32;
        QUIET + t * (LOUD - QUIET)
    } else if ms < SPEECH_END {
        synthetic_level(ms - RAMP_END)
    } else {
        let t = ((ms - SPEECH_END) as f32 / DECAY_MS as f32).min(1.0);
        LOUD * (1.0 - t) + QUIET * t
    }
}

/// The timestamp in `search_from..=search_to` whose preceding
/// [`WAVE_HISTORY_SPAN_MS`] window has the highest (`want_loud`) or lowest
/// mean [`evidence_level_profile`] — a coarse brute-force scan, cheap
/// because the profile is a pure function and this runs once at generation
/// time, never per rendered frame. Exists because "speech-like varying"
/// oscillates, so a single hand-picked timestamp risks landing on a quiet
/// moment even during the loud-sounding phase, same as the frozen-frame
/// mistake this whole evidence set exists to avoid, one level up.
fn pick_checkpoint(search_from: u64, search_to: u64, want_loud: bool) -> u64 {
    const STEP_MS: u64 = 50;
    let mut best = search_from.max(WAVE_HISTORY_SPAN_MS);
    let mut best_avg = f32::NAN;
    let mut t = best;
    while t <= search_to {
        let start = t - WAVE_HISTORY_SPAN_MS;
        let (mut sum, mut n) = (0.0f32, 0u32);
        let mut s = start;
        while s <= t {
            sum += evidence_level_profile(s);
            n += 1;
            s += 70;
        }
        let avg = sum / n as f32;
        if best_avg.is_nan() || (want_loud && avg > best_avg) || (!want_loud && avg < best_avg) {
            best_avg = avg;
            best = t;
        }
        t += STEP_MS;
    }
    best
}

/// One still per checkpoint, captured along one continuous listening period
/// driven by [`evidence_level_profile`]. Silence and the ramp get one
/// checkpoint each (nothing to contrast within them); the speech phase gets
/// its loudest and quietest moments, found by [`pick_checkpoint`], so
/// "speech-like varying" is shown *varying* rather than at one arbitrary
/// instant; decay gets a still partway through (still visibly elevated) and
/// one near the end (settled), so the descent itself is visible across two
/// frames rather than asserted. Returns the number of frames written.
fn wave_sequence_evidence(
    theme: Theme,
    scale: f32,
    dir: &Path,
    theme_name: &str,
) -> Result<u32, Box<dyn std::error::Error>> {
    let mut pill = HeadlessOverlay::new(scale, theme);
    pill.tick(0);
    pill.apply(Command::ShowListening);

    let speech_scan_from = RAMP_END + WAVE_HISTORY_SPAN_MS;
    let mut checkpoints: [(u64, &str); 6] = [
        (SILENCE_MS - 100, "0-silence"),
        (RAMP_END - 100, "1-rampup"),
        (0, "2a-speech-quiet"),
        (0, "2b-speech-loud"),
        (SPEECH_END + 1_500, "3a-decay-partway"),
        (SPEECH_END + DECAY_MS - 100, "3b-decay-settled"),
    ];
    checkpoints[2].0 = pick_checkpoint(speech_scan_from, SPEECH_END, false);
    checkpoints[3].0 = pick_checkpoint(speech_scan_from, SPEECH_END, true);
    // The capture loop below ticks strictly forward, so the array has to be
    // in time order regardless of which of "quiet" or "loud" the scan found
    // first — swap the pair (time and label together) rather than just the
    // time, so each still is still filed under its own correct label.
    if checkpoints[2].0 > checkpoints[3].0 {
        checkpoints.swap(2, 3);
    }

    let mut now = 0u64;
    let mut written = 0u32;
    for (at_ms, label) in checkpoints {
        while now < at_ms {
            now += 16;
            pill.apply(Command::Level(evidence_level_profile(now)));
            pill.tick(now);
            pill.render();
        }
        let path = dir.join(format!("{theme_name}-wave-sequence-{label}.png"));
        write_frame(&pill, &path, true)?;
        written += 1;
    }
    Ok(written)
}

/// Write one frame, optionally composited over [`backdrop_pixmap`].
fn write_frame(
    pill: &HeadlessOverlay,
    path: &Path,
    backdrop: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !backdrop {
        pill.write_png(path)?;
        return Ok(());
    }
    let frame = pill.frame();
    let mut pixmap = backdrop_pixmap(frame.width, frame.height, pill.scale()).ok_or_else(|| {
        format!(
            "cannot allocate a {}x{} backdrop",
            frame.width, frame.height
        )
    })?;

    // The frame is premultiplied, so source-over is `src + dst * (1 - a)`, and
    // the backdrop is opaque, so the result stays opaque.
    let pixels = pixmap.pixels_mut();
    for (i, src) in frame.rgba.chunks_exact(4).enumerate() {
        let inv = 1.0 - f32::from(src[3]) / 255.0;
        let dst = pixels[i];
        let blend = |s: u8, d: u8| (f32::from(s) + f32::from(d) * inv).round().min(255.0) as u8;
        pixels[i] = PremultipliedColorU8::from_rgba(
            blend(src[0], dst.red()),
            blend(src[1], dst.green()),
            blend(src[2], dst.blue()),
            255,
        )
        .unwrap_or(dst);
    }
    pixmap.save_png(path)?;
    Ok(())
}

/// How far past the shape's own edges the two contrast bands run, in logical
/// pixels: enough that they read as something on the desktop rather than as a
/// second shape traced around the first, while still leaving the warm-to-cool
/// sweep visible above, below and beyond them.
const BAND_OVERSHOOT_X: f32 = 22.0;
const BAND_OVERSHOOT_Y: f32 = 14.0;
/// Where the seam between the dark band and the light one falls, measured in
/// logical pixels inward from the shape's right edge.
///
/// Not the shape's midpoint, and not a round number: it is chosen to land in
/// the middle of the four-character timer readout, which sits right-aligned
/// near that edge (`render::timer_right_edge` puts a `0:00` at roughly 12–48
/// logical px in from it at 100 %). The timer is the one run of text the
/// default presentation draws and the element whose legibility this evidence
/// has to answer for, so the seam is placed so a single frame shows the same
/// digits over both contrast directions rather than two frames each showing
/// one.
const BAND_SEAM_FROM_RIGHT: f32 = 28.0;

/// A synthetic busy desktop for the pill to sit on.
///
/// The overlay's own frames are transparent everywhere but the shape, and the
/// shape is glass — a PNG of one against nothing shows none of what the
/// treatment does. This is a stand-in desktop: a wide warm-to-cool sweep for
/// the spectrum ramp to sit against, plus a dark band and a light band so both
/// contrast directions appear in the same frame.
///
/// Both bands are anchored on the *shape's* real rectangle, rebuilt here from
/// the same [`Layout`] the renderer places it with, rather than on fractions
/// of the frame. An earlier version gated them on frame corners (`u < 0.30`,
/// `u > 0.55`) and neither one intersected the shape at all: every committed
/// frame had the capsule sitting on plain mid-gradient, so the set could not
/// show what its own caption claimed. Anchoring on the layout also means a
/// later change to `REST_W` or `MARGIN` moves the bands with the shape
/// instead of silently un-anchoring them again.
fn backdrop_pixmap(w: u32, h: u32, scale: f32) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(w, h)?;
    let (fw, fh) = (w.max(1) as f32, h.max(1) as f32);

    let l = Layout::new(scale);
    let shape_left = l.center_x - l.rest_w * 0.5;
    let shape_right = l.center_x + l.rest_w * 0.5;
    let band_left = shape_left - BAND_OVERSHOOT_X * l.scale;
    let band_right = shape_right + BAND_OVERSHOOT_X * l.scale;
    let band_top = l.center_y - l.shape_h * 0.5 - BAND_OVERSHOOT_Y * l.scale;
    let band_bottom = l.center_y + l.shape_h * 0.5 + BAND_OVERSHOOT_Y * l.scale;
    let seam = shape_right - BAND_SEAM_FROM_RIGHT * l.scale;

    let pixels = pixmap.pixels_mut();
    for y in 0..h {
        for x in 0..w {
            let (u, v) = (x as f32 / fw, y as f32 / fh);
            // Pixel centres, so a band edge lands between pixels rather than
            // on one and the two bands cannot both claim the seam column.
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut r = 240.0 - 150.0 * u;
            let mut g = 130.0 + 40.0 * v;
            let mut b = 70.0 + 170.0 * u;
            if (band_top..band_bottom).contains(&py) {
                if (band_left..seam).contains(&px) {
                    r *= 0.12;
                    g *= 0.12;
                    b *= 0.12;
                } else if (seam..band_right).contains(&px) {
                    r = 235.0;
                    g = 238.0;
                    b = 245.0;
                }
            }
            let clamp = |c: f32| c.clamp(0.0, 255.0) as u8;
            if let Some(px) = PremultipliedColorU8::from_rgba(clamp(r), clamp(g), clamp(b), 255) {
                pixels[(y * w + x) as usize] = px;
            }
        }
    }
    Some(pixmap)
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
/// This is the mockup's original `envelope()` function, transcribed. It
/// exists so the demo looks like someone talking rather than a test tone.
fn synthetic_level(ms: u64) -> f32 {
    let s = ms as f32 / 1000.0;
    let syllables = 0.54 + 0.44 * (s * 6.4).sin() + 0.17 * (s * 12.1 + 1.4).sin();
    let phrase = 0.6 + 0.4 * (s * 1.2 + 0.5).sin();
    let attack = (ms as f32 / 100.0).min(1.0);
    (syllables * phrase).clamp(0.10, 1.0) * attack
}

/// How much of `utterance` has been "said" by `ms`, one word at a time.
///
/// Empty until [`FIRST_WORD_AT_MS`], modelling the real first-partial latency
/// so the resting capsule has a moment of quiet before any text arrives — the
/// state a non-streaming engine leaves on screen throughout, and the shipped
/// default presentation, so it has to read as complete on its own, not as
/// "waiting for a bug fix".
fn words_said_by(ms: u64, utterance: &str) -> String {
    if ms < FIRST_WORD_AT_MS {
        return String::new();
    }
    let words: Vec<&str> = utterance.split(' ').collect();
    let said = (((ms - FIRST_WORD_AT_MS) / WORD_MS) as usize + 1).min(words.len());
    words[..said].join(" ")
}

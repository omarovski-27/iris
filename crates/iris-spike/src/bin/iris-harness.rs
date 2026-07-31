//! `iris-harness` — latency measurement with no microphone and no human.
//!
//! Feeds a WAV file through the real engine path at real-time pace, exactly as
//! the microphone would, and reports the same [`Timeline`] the live spike
//! prints. It runs on any platform, so it works in CI and in WSL as well as on
//! Windows.
//!
//! ```text
//! iris-harness --engine mock                 # no key, no network, no mic
//! iris-harness --engine deepgram --runs 10   # needs IRIS_DEEPGRAM_KEY
//! ```
//!
//! [`Timeline`]: iris_core::latency::Timeline

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use iris_core::dictation::{run_offline, Pace};
use iris_core::engine::{self, EngineOptions, EngineSpec, MockConfig, MockEngine};
use iris_core::latency::{ms, Mark, Stats};
use iris_core::{audio, latency};

#[derive(Parser, Debug)]
#[command(
    name = "iris-harness",
    about = "Measure Iris pipeline latency from a WAV file — no microphone required.",
    version
)]
struct Args {
    /// Engine to measure. `mock` needs no key and no network.
    #[arg(long, default_value = "mock")]
    engine: EngineSpec,

    /// WAV to stream. Defaults to the committed speech fixture.
    #[arg(long)]
    wav: Option<PathBuf>,

    /// How many dictations to measure.
    #[arg(long, default_value_t = 5)]
    runs: usize,

    /// Feed audio as fast as possible instead of at speaking speed.
    ///
    /// Latency numbers from a fast run are not meaningful — a streaming engine
    /// gets to work ahead of the clock — but it makes correctness runs quick.
    #[arg(long)]
    fast: bool,

    /// Machine-readable output.
    #[arg(long)]
    json: bool,

    /// Show interim transcripts as they arrive.
    #[arg(long)]
    show_partials: bool,

    /// Model a cloud engine with the mock: `connect_ms,finalize_ms`.
    ///
    /// Lets the harness demonstrate what the budget looks like at a given
    /// network cost without a key.
    #[arg(long, value_name = "CONNECT_MS,FINALIZE_MS")]
    simulate: Option<String>,

    /// Override the engine's default model.
    #[arg(long)]
    model: Option<String>,

    /// Language hint, e.g. `en`.
    #[arg(long)]
    language: Option<String>,

    /// Diagnostics on stderr.
    #[arg(long, short)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    iris_core::log::set_verbose(args.verbose);

    let wav = args.wav.clone().unwrap_or_else(iris_core::sample_wav_path);
    let pcm = audio::read_wav(&wav).with_context(|| format!("reading {}", wav.display()))?;
    let duration = audio::secs(pcm.len());

    let engine = build_engine(&args)?;
    let pace = if args.fast {
        Pace::Fast
    } else {
        Pace::Realtime
    };

    if !args.json {
        println!("iris-harness");
        println!("  engine   {}", engine.name());
        println!("  audio    {} ({duration:.2} s)", wav.display());
        println!(
            "  pace     {}",
            match pace {
                Pace::Realtime => "real time (as if spoken)",
                Pace::Fast => "as fast as possible (latency numbers not meaningful)",
            }
        );
        println!("  runs     {}\n", args.runs);
    }

    let mut perceived = Vec::new();
    let mut first_partial = Vec::new();
    let mut records = Vec::new();

    for run in 1..=args.runs {
        let mut on_partial = |text: &str| {
            if args.show_partials && !args.json {
                print!("\r  ~ {text:<78.78}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        };

        let outcome = run_offline(&*engine, &pcm, pace, &mut on_partial)
            .with_context(|| format!("run {run}"))?;
        let t = &outcome.timeline;

        if let Some(d) = t.perceived() {
            perceived.push(ms(d));
        }
        if let Some(d) = t.delta(Mark::CaptureStart, Mark::FirstPartial) {
            first_partial.push(ms(d));
        }

        if args.json {
            records.push(json_record(run, t));
        } else {
            print!("\r{:<80}\r", "");
            println!("{}", t.report(run));
        }
    }

    if args.json {
        let summary = serde_json::json!({
            "engine": engine.name(),
            "wav": wav.display().to_string(),
            "audio_secs": duration,
            "pace": if args.fast { "fast" } else { "realtime" },
            "runs": records,
            "perceived_ms": stats_json(&perceived),
            "first_partial_ms": stats_json(&first_partial),
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("── summary ─────────────────────────────────────");
        print_stats("perceived (key-release → transcript)", &perceived);
        print_stats("first audio → first partial", &first_partial);
        verdict(&perceived, engine.streams_partials());
    }

    Ok(())
}

fn build_engine(args: &Args) -> Result<Box<dyn engine::Engine>> {
    if let Some(spec) = &args.simulate {
        anyhow::ensure!(
            args.engine == EngineSpec::Mock,
            "--simulate only applies to --engine mock"
        );
        let (connect, finalize) = parse_simulate(spec)?;
        return Ok(Box::new(MockEngine::new(MockConfig {
            connect_delay: connect,
            finalize_delay: finalize,
            ..MockConfig::default()
        })));
    }
    engine::build(
        args.engine,
        &EngineOptions {
            model: args.model.clone(),
            language: args.language.clone(),
        },
    )
}

fn parse_simulate(spec: &str) -> Result<(Duration, Duration)> {
    let (a, b) = spec
        .split_once(',')
        .context("expected --simulate CONNECT_MS,FINALIZE_MS, e.g. --simulate 120,160")?;
    let parse = |s: &str| -> Result<Duration> {
        Ok(Duration::from_millis(
            s.trim()
                .parse::<u64>()
                .with_context(|| format!("{s:?} is not a number of milliseconds"))?,
        ))
    };
    Ok((parse(a)?, parse(b)?))
}

fn print_stats(label: &str, values: &[f64]) {
    match Stats::from_ms(values) {
        Some(s) => println!("  {label:<38} {s}"),
        None => println!("  {label:<38} (not measured)"),
    }
}

fn stats_json(values: &[f64]) -> serde_json::Value {
    match Stats::from_ms(values) {
        Some(s) => serde_json::json!({
            "n": s.n, "min": s.min, "p50": s.p50, "p95": s.p95, "max": s.max, "mean": s.mean
        }),
        None => serde_json::Value::Null,
    }
}

fn json_record(run: usize, t: &latency::Timeline) -> serde_json::Value {
    let span = |from: Mark, to: Mark| t.delta(from, to).map(ms);
    serde_json::json!({
        "run": run,
        "partials": t.partials,
        "audio_secs": t.audio_secs,
        "transcript": t.transcript,
        "key_to_session_open_ms": span(Mark::KeyDown, Mark::SessionOpen),
        "key_to_capture_start_ms": span(Mark::KeyDown, Mark::CaptureStart),
        "key_to_stream_ready_ms": span(Mark::KeyDown, Mark::StreamReady),
        "audio_to_first_partial_ms": span(Mark::CaptureStart, Mark::FirstPartial),
        "release_to_final_ms": span(Mark::KeyUp, Mark::FinalTranscript),
        "final_to_injected_ms": span(Mark::FinalTranscript, Mark::Injected),
        "perceived_ms": t.perceived().map(ms),
    })
}

/// The target the spike exists to test.
const TARGET_MS: f64 = 300.0;

fn verdict(perceived: &[f64], streams: bool) {
    let Some(stats) = Stats::from_ms(perceived) else {
        return;
    };
    println!();
    if stats.p95 < TARGET_MS {
        println!(
            "  ✓ p95 {:.0} ms is under the {TARGET_MS:.0} ms target.",
            stats.p95
        );
    } else {
        println!(
            "  ✗ p95 {:.0} ms misses the {TARGET_MS:.0} ms target.",
            stats.p95
        );
    }
    if !streams {
        println!(
            "    This engine cannot stream, so the whole transcription lands after key-release."
        );
    }
    println!("    Note: this excludes text injection, which the live spike measures separately.");
}

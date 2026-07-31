//! Feed a WAV file through the local engine stack and print streaming partials
//! plus a final transcript with a latency breakdown.
//!
//! ## Offline / default (no model download)
//!
//! ```bash
//! cargo run -p iris-engine-local --example transcribe_wav -- fixtures/speech-16k.wav
//! ```
//!
//! ## Real models (downloads on first use; needs network once)
//!
//! ```bash
//! cargo run -p iris-engine-local --example transcribe_wav --features native -- \
//!   --engine native fixtures/speech-16k.wav
//! ```
//!
//! Env:
//! - `IRIS_MODEL_DIR` — model cache (default `%LOCALAPPDATA%\Iris\models` on Windows, `~/.cache/iris/models` on Unix)

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;
use iris_engine_local::audio::read_wav_pcm16;
use iris_engine_local::engine::{LocalEngine, LocalEvent, LocalSession};
use iris_engine_local::layered::LayeredLocalEngine;
use iris_engine_local::models::{default_model_dir, ProgressFn};

#[derive(Parser, Debug)]
#[command(
    name = "transcribe_wav",
    about = "Local Iris STT demo: stream partials then finalize"
)]
struct Args {
    /// Path to a 16 kHz mono (or stereo) WAV file.
    wav: PathBuf,

    /// `mock` (offline) or `native` (Zipformer + whisper; requires --features native).
    #[arg(long, default_value = "mock")]
    engine: String,

    /// Model cache directory (overrides IRIS_MODEL_DIR).
    #[arg(long)]
    model_dir: Option<PathBuf>,

    /// Frame size in samples when feeding the stream (default 1600 = 100 ms @ 16 kHz).
    #[arg(long, default_value_t = 1600)]
    frame: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let pcm =
        read_wav_pcm16(&args.wav).with_context(|| format!("reading {}", args.wav.display()))?;
    let duration_s = pcm.len() as f64 / iris_engine_local::SAMPLE_RATE as f64;
    println!(
        "iris-engine-local example\n  wav      {}\n  duration {:.2}s ({} samples)\n  engine   {}\n",
        args.wav.display(),
        duration_s,
        pcm.len(),
        args.engine
    );

    let load_start = Instant::now();
    let engine: LayeredLocalEngine = match args.engine.as_str() {
        "mock" => LayeredLocalEngine::mock(),
        "native" => build_native(args.model_dir.as_deref())?,
        other => bail!("unknown --engine {other:?} (expected mock or native)"),
    };
    let load_ms = load_start.elapsed().as_millis() as u64;
    println!("  load     {load_ms} ms\n");

    let mut session = engine.start()?;
    let session_start = Instant::now();
    let mut first_partial_ms: Option<u64> = None;
    let mut last_partial = String::new();

    let frame = args.frame.max(1);
    for chunk in pcm.chunks(frame) {
        session.feed(chunk)?;
        drain_partials(
            &mut *session,
            session_start,
            &mut first_partial_ms,
            &mut last_partial,
        );
    }

    let fin_start = Instant::now();
    session.finalize()?;
    let final_text = loop {
        match session.partials().try_recv() {
            Ok(LocalEvent::Partial(t)) => {
                if first_partial_ms.is_none() {
                    first_partial_ms = Some(session_start.elapsed().as_millis() as u64);
                }
                last_partial = t.clone();
                println!(
                    "  [{:6.0} ms] partial: {t}",
                    session_start.elapsed().as_millis()
                );
            }
            Ok(LocalEvent::Final(t)) => break t,
            Ok(LocalEvent::Error(e)) => bail!("engine error: {e}"),
            Ok(LocalEvent::Ready) => {}
            Err(crossbeam_channel::TryRecvError::Empty) => {
                if fin_start.elapsed().as_secs() > 120 {
                    bail!("timed out waiting for Final");
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                bail!("event channel closed without Final");
            }
        }
    };
    let finalize_ms = fin_start.elapsed().as_millis() as u64;

    println!();
    println!("── result ──────────────────────────────────");
    println!("  final:    {final_text:?}");
    if !last_partial.is_empty() {
        println!("  last ~ :  {last_partial:?}");
    }
    println!("── latency ─────────────────────────────────");
    println!("  load:            {load_ms} ms");
    println!(
        "  first partial:   {}",
        first_partial_ms
            .map(|m| format!("{m} ms after session start"))
            .unwrap_or_else(|| "(none)".into())
    );
    println!("  finalization:    {finalize_ms} ms after last frame");
    println!("────────────────────────────────────────────");
    Ok(())
}

fn drain_partials(
    session: &mut dyn LocalSession,
    session_start: Instant,
    first_partial_ms: &mut Option<u64>,
    last_partial: &mut String,
) {
    for ev in session.partials().try_iter() {
        if let LocalEvent::Partial(t) = ev {
            if first_partial_ms.is_none() {
                *first_partial_ms = Some(session_start.elapsed().as_millis() as u64);
            }
            *last_partial = t.clone();
            println!(
                "  [{:6.0} ms] partial: {t}",
                session_start.elapsed().as_millis()
            );
        }
    }
}

fn build_native(model_dir: Option<&Path>) -> Result<LayeredLocalEngine> {
    let dir = model_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_model_dir);
    println!("  models   {}", dir.display());

    let on_progress = |done: u64, total: Option<u64>| {
        if let Some(t) = total {
            if t > 0 && done % (512 * 1024) < 64 * 1024 {
                eprint!("\r  download {:.1}%", 100.0 * done as f64 / t as f64);
            }
        }
    };
    let progress: &ProgressFn = &on_progress;

    #[cfg(all(feature = "streaming", feature = "whisper"))]
    {
        use std::sync::Arc;

        use iris_engine_local::layered::LayeredLocalEngineConfig;
        use iris_engine_local::models::{WhisperPaths, ZipformerPaths};
        use iris_engine_local::streaming::{StreamingConfig, StreamingEngine};

        let z_paths = ZipformerPaths::ensure(&dir, Some(progress))?;
        eprintln!();
        let w_paths = WhisperPaths::ensure(&dir, Some(progress))?;
        eprintln!();

        let streaming = StreamingEngine::zipformer(StreamingConfig::from_paths(z_paths))?;
        let finalizer =
            iris_engine_local::WhisperFinalizer::load(iris_engine_local::FinalizerConfig {
                model_path: w_paths.model,
                vad_path: w_paths.vad,
                language: Some("en".into()),
                num_threads: 4,
            })?;

        return Ok(LayeredLocalEngine::new(LayeredLocalEngineConfig {
            streaming: Arc::new(streaming),
            finalizer: Arc::new(finalizer),
        }));
    }

    #[cfg(not(all(feature = "streaming", feature = "whisper")))]
    {
        let _ = (dir, progress);
        bail!(
            "--engine native requires building with `--features native` \
             (streaming Zipformer + whisper finalizer)"
        );
    }
}

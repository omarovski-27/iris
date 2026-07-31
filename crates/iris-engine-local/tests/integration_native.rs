//! Real-model integration tests.
//!
//! **Not run in default CI.** Requires:
//! 1. Built with `--features native`
//! 2. Env `IRIS_LOCAL_MODELS=1`
//! 3. Network on first run (~132 MB into `$IRIS_MODEL_DIR`, or the OS default cache)
//!
//! ```bash
//! IRIS_LOCAL_MODELS=1 cargo test -p iris-engine-local --features native \
//!   --test integration_native -- --nocapture
//! ```

#[cfg(feature = "native")]
mod native {
    use std::sync::Arc;
    use std::time::Instant;

    use iris_engine_local::audio::read_wav_pcm16;
    use iris_engine_local::engine::{LocalEngine, LocalEvent};
    use iris_engine_local::finalizer::{BatchFinalizer, WhisperFinalizer};
    use iris_engine_local::layered::{LayeredLocalEngine, LayeredLocalEngineConfig};
    use iris_engine_local::models::{default_model_dir, WhisperPaths, ZipformerPaths};
    use iris_engine_local::streaming::{StreamingConfig, StreamingEngine};
    use iris_engine_local::{silence_fixture, speech_fixture, FinalizerConfig};

    fn models_enabled() -> bool {
        matches!(
            std::env::var("IRIS_LOCAL_MODELS").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
    }

    fn build_layered() -> LayeredLocalEngine {
        let dir = default_model_dir();
        let z = ZipformerPaths::ensure(&dir, None).expect("download zipformer models");
        let w = WhisperPaths::ensure(&dir, None).expect("download whisper models");
        let streaming =
            StreamingEngine::zipformer(StreamingConfig::from_paths(z)).expect("zipformer load");
        let finalizer = WhisperFinalizer::load(FinalizerConfig {
            model_path: w.model,
            vad_path: w.vad,
            language: Some("en".into()),
            num_threads: 4,
        })
        .expect("whisper load");
        LayeredLocalEngine::new(LayeredLocalEngineConfig {
            streaming: Arc::new(streaming),
            finalizer: Arc::new(finalizer),
        })
    }

    #[test]
    fn streaming_partials_and_fast_finalize_on_speech() {
        if !models_enabled() {
            eprintln!("skip: set IRIS_LOCAL_MODELS=1 to run real-model tests");
            return;
        }

        let engine = build_layered();
        let pcm = read_wav_pcm16(&speech_fixture()).unwrap();

        let mut session = engine.start().unwrap();
        for chunk in pcm.chunks(1600) {
            session.feed(chunk).unwrap();
        }

        let mut saw_partial = false;
        for ev in session.partials().try_iter() {
            if let LocalEvent::Partial(t) = ev {
                if !t.is_empty() {
                    saw_partial = true;
                }
            }
        }

        let t0 = Instant::now();
        session.finalize().unwrap();
        let mut final_text = None;
        for ev in session.partials().try_iter() {
            match ev {
                LocalEvent::Partial(_) => saw_partial = true,
                LocalEvent::Final(t) => final_text = Some(t),
                LocalEvent::Error(e) => panic!("error: {e}"),
                LocalEvent::Ready => {}
            }
        }
        let finalize_ms = t0.elapsed().as_millis() as u64;

        assert!(
            saw_partial,
            "expected at least one non-empty streaming partial on speech"
        );
        let text = final_text.expect("expected Final event");
        assert!(
            !text.is_empty(),
            "expected punctuated final transcript on speech, got empty"
        );
        assert!(
            finalize_ms < 30_000,
            "finalization took {finalize_ms} ms (budget 30s for whisper base.en)"
        );
        eprintln!("native speech final={text:?} finalize_ms={finalize_ms}");
    }

    #[test]
    fn finalizer_empty_on_silence_no_hallucination() {
        if !models_enabled() {
            eprintln!("skip: set IRIS_LOCAL_MODELS=1 to run real-model tests");
            return;
        }

        let dir = default_model_dir();
        let w = WhisperPaths::ensure(&dir, None).expect("download whisper models");
        let finalizer = WhisperFinalizer::load(FinalizerConfig {
            model_path: w.model,
            vad_path: w.vad,
            language: Some("en".into()),
            num_threads: 4,
        })
        .expect("whisper load");

        let pcm = read_wav_pcm16(&silence_fixture()).unwrap();
        let text = finalizer.transcribe(&pcm).expect("transcribe silence");
        assert_eq!(
            text, "",
            "VAD-gated whisper must not hallucinate on pure silence; got {text:?}"
        );
    }

    #[test]
    fn streaming_only_finalize_under_100ms() {
        if !models_enabled() {
            eprintln!("skip: set IRIS_LOCAL_MODELS=1 to run real-model tests");
            return;
        }

        let dir = default_model_dir();
        let z = ZipformerPaths::ensure(&dir, None).expect("zipformer");
        let engine =
            StreamingEngine::zipformer(StreamingConfig::from_paths(z)).expect("load zipformer");
        let pcm = read_wav_pcm16(&speech_fixture()).unwrap();

        let mut session = engine.start().unwrap();
        for chunk in pcm.chunks(1600) {
            session.feed(chunk).unwrap();
        }
        let t0 = Instant::now();
        session.finalize().unwrap();
        let ms = t0.elapsed().as_millis() as u64;
        assert!(
            ms < 100,
            "Zipformer finalization after last frame must be <100 ms (report ~4 ms); got {ms} ms"
        );
        eprintln!("zipformer-only finalize_ms={ms}");
    }
}

#[cfg(not(feature = "native"))]
#[test]
fn native_tests_require_feature() {
    // Placeholder so the integration target always has a test without native deps.
    eprintln!("native integration tests compile only with --features native");
}

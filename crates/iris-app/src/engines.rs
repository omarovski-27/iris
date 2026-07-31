//! Turning [`EngineChoice`] into a live [`Engine`].
//!
//! Three of the four choices are just [`iris_core::engine::build`]. The fourth,
//! `local`, needs an adapter: `iris-engine-local` mirrors the core trait rather
//! than implementing it (it was written on a branch where `iris-core` did not
//! exist yet), so [`LocalAdapter`] is the one-file mapping its README predicts.

use std::sync::Arc;

use anyhow::{Context, Result};
use iris_core::engine::{Engine, EngineOptions, EngineSpec};

use crate::config::{Config, EngineChoice};

/// Build the engine `config` asks for.
///
/// API keys are read from the environment, so [`Config::promote_keys`] must
/// have run first.
pub fn build(config: &Config) -> Result<Arc<dyn Engine>> {
    let options = EngineOptions::default();
    let engine: Arc<dyn Engine> = match config.engine {
        EngineChoice::Mock => iris_core::engine::build(EngineSpec::Mock, &options)?.into(),
        EngineChoice::Deepgram => iris_core::engine::build(EngineSpec::Deepgram, &options)?.into(),
        EngineChoice::Groq => iris_core::engine::build(EngineSpec::Groq, &options)?.into(),
        EngineChoice::Local => Arc::new(local::build()?),
    };
    Ok(engine)
}

#[cfg(feature = "local-native")]
mod local {
    use anyhow::{Context, Result};
    use iris_engine_local::{
        models, FinalizerConfig, LayeredLocalEngine, LayeredLocalEngineConfig, StreamingConfig,
        StreamingEngine,
    };
    use std::sync::Arc;

    /// Load the on-device engines, downloading the models on first use.
    ///
    /// Model download is not on any latency path — it happens when the engine
    /// is selected, not when the hotkey is pressed — but it is the one thing in
    /// this program that can take minutes, so it reports progress.
    pub fn build() -> Result<super::LocalAdapter> {
        let dir = models::default_model_dir();
        let progress: models::ProgressFn = Box::new(|name: &str, done: u64, total: u64| {
            eprintln!("[iris] model {name}: {done}/{total} bytes");
        });

        let zipformer = models::ZipformerPaths::ensure(&dir, Some(&progress))
            .context("fetching the streaming Zipformer models")?;
        let whisper = models::WhisperPaths::ensure(&dir, Some(&progress))
            .context("fetching the Whisper finalizer models")?;

        let streaming = StreamingEngine::zipformer(StreamingConfig::from_paths(zipformer))
            .context("loading the streaming Zipformer engine")?;
        let finalizer = iris_engine_local::finalizer::default_finalizer(FinalizerConfig {
            model_path: whisper.model,
            vad_path: whisper.vad,
            language: Some("en".into()),
            num_threads: 4,
        })
        .context("loading the Whisper finalizer")?;

        Ok(super::LocalAdapter::new(Arc::new(LayeredLocalEngine::new(
            LayeredLocalEngineConfig {
                streaming: Arc::new(streaming),
                finalizer: Arc::from(finalizer),
            },
        ))))
    }
}

#[cfg(not(feature = "local-native"))]
mod local {
    use anyhow::Result;

    /// Without the native engines there is nothing to run locally, and quietly
    /// substituting the mock would be a lie about where the user's audio goes.
    pub fn build() -> Result<super::LocalAdapter> {
        anyhow::bail!(
            "the local engine needs the `local-native` feature, which pulls in sherpa-onnx and \
             whisper.cpp:\n    cargo build --release --features local-native\n\
             Those do not cross-compile to x86_64-pc-windows-gnu from WSL; build them on native \
             Windows (see crates/iris-engine-local/README.md). Meanwhile `engine = \"mock\"`, \
             \"deepgram\" or \"groq\" all work."
        )
    }
}

/// Presents an [`iris_engine_local::LocalEngine`] as an [`Engine`].
///
/// The two traits are the same shape (`open`/`start`, `push`/`feed`,
/// `events`/`partials`, `finish`/`finalize`) with one substantive difference:
/// `LocalSession::finalize` is allowed to block — Whisper's batch pass runs
/// there — while [`iris_core::engine::Session::finish`] must return
/// immediately so the caller can keep painting an overlay. So the adapter moves
/// the session onto a thread at finalise time and lets the transcript arrive as
/// an event, which is what the core trait promises its callers.
pub struct LocalAdapter {
    inner: Arc<dyn iris_engine_local::LocalEngine>,
}

impl LocalAdapter {
    /// Wrap a local engine.
    pub fn new(inner: Arc<dyn iris_engine_local::LocalEngine>) -> Self {
        Self { inner }
    }
}

impl std::fmt::Debug for LocalAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAdapter")
            .field("engine", &self.inner.name())
            .finish()
    }
}

impl Engine for LocalAdapter {
    fn name(&self) -> &'static str {
        "local"
    }

    fn streams_partials(&self) -> bool {
        self.inner.streams_partials()
    }

    fn open(&self) -> Result<Box<dyn iris_core::engine::Session>> {
        let session = self.inner.start().context("starting the local engine")?;
        let (tx, rx) = crossbeam_channel::unbounded();

        // The pump thread owns nothing but a receiver clone, so the session
        // itself can be moved to the finalise thread later without a lock.
        let source = session.partials().clone();
        let pump_tx = tx.clone();
        let pump = std::thread::Builder::new()
            .name("iris-local-events".into())
            .spawn(move || {
                for event in source {
                    let translated = match event {
                        iris_engine_local::LocalEvent::Ready => {
                            iris_core::engine::TranscriptEvent::Connected
                        }
                        iris_engine_local::LocalEvent::Partial(text) => {
                            iris_core::engine::TranscriptEvent::Partial(text)
                        }
                        iris_engine_local::LocalEvent::Final(text) => {
                            iris_core::engine::TranscriptEvent::Final(text)
                        }
                        iris_engine_local::LocalEvent::Error(message) => {
                            iris_core::engine::TranscriptEvent::Error(message)
                        }
                    };
                    if pump_tx.send(translated).is_err() {
                        break;
                    }
                }
            })
            .context("spawning the local-engine event pump")?;

        Ok(Box::new(LocalAdapterSession {
            session: Some(session),
            events: rx,
            tx,
            pump: Some(pump),
        }))
    }
}

struct LocalAdapterSession {
    /// `None` once [`Session::finish`] has moved it to the finalise thread.
    session: Option<Box<dyn iris_engine_local::LocalSession>>,
    events: crossbeam_channel::Receiver<iris_core::engine::TranscriptEvent>,
    tx: crossbeam_channel::Sender<iris_core::engine::TranscriptEvent>,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl iris_core::engine::Session for LocalAdapterSession {
    fn push(&mut self, pcm: &[i16]) -> Result<()> {
        match &mut self.session {
            Some(session) => session.feed(pcm),
            // Audio after finalise is the tail of a dictation that has already
            // ended; dropping it is correct and must not be an error.
            None => Ok(()),
        }
    }

    fn events(&self) -> &crossbeam_channel::Receiver<iris_core::engine::TranscriptEvent> {
        &self.events
    }

    fn finish(&mut self) -> Result<()> {
        let Some(mut session) = self.session.take() else {
            return Ok(());
        };
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("iris-local-finalize".into())
            .spawn(move || {
                if let Err(e) = session.finalize() {
                    let _ = tx.send(iris_core::engine::TranscriptEvent::Error(format!("{e:#}")));
                }
                // Dropping the session closes its channel, which ends the pump
                // thread and, with it, this session's event stream.
                drop(session);
            })
            .context("spawning the local-engine finalise thread")?;
        // The finalise thread now owns the session, so the pump outlives this
        // adapter by exactly one Whisper batch. Detach it: joining from Drop
        // would park the dictation thread until finalise completes, which is
        // the very wait the finish-timeout exists to escape.
        self.pump = None;
        Ok(())
    }
}

impl Drop for LocalAdapterSession {
    fn drop(&mut self) {
        // Dropping a session that was never finished must not leave the pump
        // thread alive: dropping the local session closes its channel, the pump
        // sees the close and returns. After `finish` the pump is already
        // detached and this join is skipped.
        self.session = None;
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_core::engine::TranscriptEvent;

    /// The mock local engine is offline and available without the native
    /// features, so the adapter's event mapping is testable in CI on Linux.
    fn mock_local() -> LocalAdapter {
        LocalAdapter::new(Arc::new(iris_engine_local::MockLocalEngine::default()))
    }

    #[test]
    fn adapter_reports_itself_as_the_local_engine() {
        let engine = mock_local();
        assert_eq!(engine.name(), "local");
        assert!(format!("{engine:?}").contains("engine"));
    }

    #[test]
    fn adapter_maps_local_events_onto_core_events() {
        let engine = mock_local();
        let outcome = iris_core::dictation::run_offline(
            &engine,
            &vec![1_000i16; 16_000],
            iris_core::dictation::Pace::Fast,
            &mut |_| {},
        )
        .unwrap();
        assert!(
            !outcome.text.is_empty(),
            "the mock should produce a transcript"
        );
    }

    #[test]
    fn a_session_dropped_without_finishing_does_not_wedge() {
        let engine = mock_local();
        let mut session = engine.open().unwrap();
        session.push(&[0i16; 320]).unwrap();
        // The pump thread is joined in Drop; if the mapping leaked a sender
        // this would hang instead of returning.
        drop(session);
    }

    /// A local engine whose finalise takes long enough to trip the finish
    /// timeout — the case where the caller gives up and drops the session.
    struct SlowFinalizeEngine;

    struct SlowFinalizeSession {
        tx: crossbeam_channel::Sender<iris_engine_local::LocalEvent>,
        rx: crossbeam_channel::Receiver<iris_engine_local::LocalEvent>,
    }

    impl iris_engine_local::LocalEngine for SlowFinalizeEngine {
        fn name(&self) -> &'static str {
            "slow"
        }
        fn start(&self) -> Result<Box<dyn iris_engine_local::LocalSession>> {
            let (tx, rx) = crossbeam_channel::unbounded();
            tx.send(iris_engine_local::LocalEvent::Ready).unwrap();
            Ok(Box::new(SlowFinalizeSession { tx, rx }))
        }
    }

    impl iris_engine_local::LocalSession for SlowFinalizeSession {
        fn feed(&mut self, _pcm: &[i16]) -> Result<()> {
            Ok(())
        }
        fn partials(&self) -> &crossbeam_channel::Receiver<iris_engine_local::LocalEvent> {
            &self.rx
        }
        fn finalize(&mut self) -> Result<()> {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = self
                .tx
                .send(iris_engine_local::LocalEvent::Final("late".into()));
            Ok(())
        }
    }

    #[test]
    fn dropping_a_finished_session_does_not_wait_for_finalize() {
        // A finish timeout only means anything if giving up is fast: the
        // whole point of moving finalise to a thread is that a slow Whisper
        // batch never parks the dictation loop.
        let engine = LocalAdapter::new(Arc::new(SlowFinalizeEngine));
        let mut session = engine.open().unwrap();
        session.push(&[0i16; 320]).unwrap();
        session.finish().unwrap();

        let dropped_in = {
            let start = std::time::Instant::now();
            drop(session);
            start.elapsed()
        };
        assert!(
            dropped_in < std::time::Duration::from_secs(1),
            "drop blocked on finalise for {dropped_in:?}"
        );
    }

    #[test]
    fn connected_is_the_first_event() {
        let engine = mock_local();
        let session = engine.open().unwrap();
        assert_eq!(
            session.events().recv().unwrap(),
            TranscriptEvent::Connected,
            "Ready must map to Connected so the timeline stamps stream-ready"
        );
    }

    #[test]
    #[cfg(not(feature = "local-native"))]
    fn without_the_native_feature_local_explains_the_rebuild() {
        let config = Config {
            engine: EngineChoice::Local,
            ..Config::default()
        };
        let err = match build(&config) {
            Ok(_) => panic!("the local engine cannot exist without the native feature"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("local-native"), "{err}");
    }

    #[test]
    fn mock_builds_with_no_environment_at_all() {
        let engine = build(&Config::default()).unwrap();
        assert_eq!(engine.name(), "mock");
    }
}

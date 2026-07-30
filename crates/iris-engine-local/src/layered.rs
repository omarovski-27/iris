//! Layered local engine: streaming ghost text + batch transcript of record.
//!
//! While the user speaks, partials come from the streaming Zipformer (or mock).
//! On finalize, the batch finalizer re-transcribes the full buffered utterance
//! and that string is the only text that should be injected.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};

use crate::engine::{LocalEngine, LocalEvent, LocalSession};
use crate::finalizer::{BatchFinalizer, MockFinalizer};
use crate::mock::{MockLocalEngine, MockLocalEngineConfig};
use crate::streaming::StreamingEngine;

/// Latency marks collected for one layered session (for the CLI / harness).
#[derive(Debug, Clone, Default)]
pub struct SessionTimings {
    pub load_ms: Option<u64>,
    pub first_partial_ms: Option<u64>,
    pub finalize_ms: Option<u64>,
    pub streaming_finalize_ms: Option<u64>,
    pub batch_finalize_ms: Option<u64>,
}

/// Configuration for building a layered engine.
pub struct LayeredLocalEngineConfig {
    pub streaming: Arc<dyn LocalEngine>,
    pub finalizer: Arc<dyn BatchFinalizer>,
}

impl LayeredLocalEngineConfig {
    /// Fully offline layered engine for tests (mock stream + silence-aware mock finalizer).
    pub fn mock() -> Self {
        Self {
            streaming: Arc::new(MockLocalEngine::default()),
            finalizer: Arc::new(MockFinalizer::default()),
        }
    }

    /// Mock streaming with a custom mock finalizer.
    pub fn mock_with_finalizer(finalizer: Arc<dyn BatchFinalizer>) -> Self {
        Self {
            streaming: Arc::new(StreamingEngine::mock(MockLocalEngineConfig::default())),
            finalizer,
        }
    }
}

/// Combined local engine: Zipformer (or mock) partials + batch finalizer.
pub struct LayeredLocalEngine {
    streaming: Arc<dyn LocalEngine>,
    finalizer: Arc<dyn BatchFinalizer>,
}

impl LayeredLocalEngine {
    pub fn new(config: LayeredLocalEngineConfig) -> Self {
        Self {
            streaming: config.streaming,
            finalizer: config.finalizer,
        }
    }

    pub fn mock() -> Self {
        Self::new(LayeredLocalEngineConfig::mock())
    }
}

impl LocalEngine for LayeredLocalEngine {
    fn name(&self) -> &'static str {
        "local-layered"
    }

    fn streams_partials(&self) -> bool {
        self.streaming.streams_partials()
    }

    fn start(&self) -> Result<Box<dyn LocalSession>> {
        let stream_session = self.streaming.start()?;
        let (tx, rx) = crossbeam_channel::unbounded();
        let _ = tx.send(LocalEvent::Ready);
        Ok(Box::new(LayeredSession {
            stream: stream_session,
            finalizer: Arc::clone(&self.finalizer),
            tx,
            rx,
            buffer: Vec::new(),
            started: Instant::now(),
            first_partial_at: None,
            finished: false,
            stream_failed: false,
            timings: SessionTimings::default(),
        }))
    }
}

struct LayeredSession {
    stream: Box<dyn LocalSession>,
    finalizer: Arc<dyn BatchFinalizer>,
    tx: Sender<LocalEvent>,
    rx: Receiver<LocalEvent>,
    buffer: Vec<i16>,
    started: Instant,
    first_partial_at: Option<Instant>,
    finished: bool,
    stream_failed: bool,
    timings: SessionTimings,
}

impl LayeredSession {
    /// Drain streaming partials into our own channel (re-tagged).
    fn pump_stream_partials(&mut self) {
        for ev in self.stream.partials().try_iter() {
            match ev {
                LocalEvent::Partial(t) => {
                    if self.first_partial_at.is_none() {
                        self.first_partial_at = Some(Instant::now());
                        self.timings.first_partial_ms =
                            Some(self.started.elapsed().as_millis() as u64);
                    }
                    let _ = self.tx.send(LocalEvent::Partial(t));
                }
                LocalEvent::Ready => {}
                // Streaming Final is discarded — batch finalizer is the source of truth.
                LocalEvent::Final(_) => {}
                LocalEvent::Error(e) => {
                    self.stream_failed = true;
                    let _ = self.tx.send(LocalEvent::Error(e));
                }
            }
        }
    }

    fn emit_error(&mut self, msg: String) {
        self.stream_failed = true;
        let _ = self.tx.send(LocalEvent::Error(msg));
    }
}

impl LocalSession for LayeredSession {
    fn feed(&mut self, pcm: &[i16]) -> Result<()> {
        if self.finished || self.stream_failed {
            return Ok(());
        }
        self.buffer.extend_from_slice(pcm);
        self.stream.feed(pcm)?;
        self.pump_stream_partials();
        Ok(())
    }

    fn partials(&self) -> &Receiver<LocalEvent> {
        &self.rx
    }

    fn finalize(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;

        if self.stream_failed {
            anyhow::bail!("streaming layer failed");
        }

        let t0 = Instant::now();
        // Flush streaming layer (measures "ghost text settled" latency).
        let stream_result = self.stream.finalize();
        self.pump_stream_partials();
        if let Err(e) = stream_result {
            if !self.stream_failed {
                self.emit_error(format!("{e:#}"));
            }
            return Err(e);
        }
        if self.stream_failed {
            anyhow::bail!("streaming layer failed");
        }
        self.timings.streaming_finalize_ms = Some(t0.elapsed().as_millis() as u64);

        // Transcript of record from the batch finalizer (VAD-gated when real).
        let t1 = Instant::now();
        let text = match self.finalizer.transcribe(&self.buffer) {
            Ok(t) => t,
            Err(e) => {
                self.emit_error(format!("{e:#}"));
                return Err(e);
            }
        };
        self.timings.batch_finalize_ms = Some(t1.elapsed().as_millis() as u64);
        self.timings.finalize_ms = Some(t0.elapsed().as_millis() as u64);

        let _ = self.tx.send(LocalEvent::Final(text));
        Ok(())
    }
}

/// Access timings after finalize by downcasting is awkward with trait objects;
/// provide a helper that runs a full session and returns text + timings.
pub fn run_layered(
    engine: &LayeredLocalEngine,
    pcm: &[i16],
    frame_samples: usize,
) -> Result<(String, Vec<String>, SessionTimings)> {
    let mut session = engine.start()?;
    let started = Instant::now();
    let mut first_partial_ms = None;
    let mut partials = Vec::new();

    let frame = if frame_samples == 0 {
        1600
    } else {
        frame_samples
    };
    for chunk in pcm.chunks(frame) {
        session.feed(chunk)?;
        for ev in session.partials().try_iter() {
            if let LocalEvent::Partial(t) = ev {
                if first_partial_ms.is_none() {
                    first_partial_ms = Some(started.elapsed().as_millis() as u64);
                }
                partials.push(t);
            }
        }
    }

    let fin_start = Instant::now();
    session.finalize()?;
    let finalize_ms = fin_start.elapsed().as_millis() as u64;

    let mut final_text = String::new();
    for ev in session.partials().try_iter() {
        match ev {
            LocalEvent::Partial(t) => partials.push(t),
            LocalEvent::Final(t) => final_text = t,
            LocalEvent::Error(e) => anyhow::bail!("engine error: {e}"),
            LocalEvent::Ready => {}
        }
    }

    Ok((
        final_text,
        partials,
        SessionTimings {
            load_ms: None,
            first_partial_ms,
            finalize_ms: Some(finalize_ms),
            streaming_finalize_ms: None,
            batch_finalize_ms: None,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::read_wav_pcm16;
    use crate::engine::{LocalEngine, LocalEvent, LocalSession};
    use crate::{silence_fixture, speech_fixture};

    #[test]
    fn layered_mock_partials_and_final_on_speech() {
        let engine = LayeredLocalEngine::mock();
        let pcm = read_wav_pcm16(&speech_fixture()).unwrap();
        let (final_text, partials, _t) = run_layered(&engine, &pcm, 1600).unwrap();
        assert!(!partials.is_empty());
        assert!(!final_text.is_empty());
    }

    #[test]
    fn layered_mock_empty_final_on_silence() {
        let engine = LayeredLocalEngine::mock();
        let pcm = read_wav_pcm16(&silence_fixture()).unwrap();
        let (final_text, _partials, _t) = run_layered(&engine, &pcm, 1600).unwrap();
        assert_eq!(
            final_text, "",
            "silence fixture must produce empty transcript of record"
        );
    }

    /// Streaming engine that emits Error from finalize and must not be followed
    /// by a batch Final on the layered channel.
    struct FailingStreamEngine;

    struct FailingStreamSession {
        tx: Sender<LocalEvent>,
        rx: Receiver<LocalEvent>,
        finished: bool,
    }

    impl LocalEngine for FailingStreamEngine {
        fn name(&self) -> &'static str {
            "failing-stream"
        }

        fn start(&self) -> Result<Box<dyn LocalSession>> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let _ = tx.send(LocalEvent::Ready);
            Ok(Box::new(FailingStreamSession {
                tx,
                rx,
                finished: false,
            }))
        }
    }

    impl LocalSession for FailingStreamSession {
        fn feed(&mut self, _pcm: &[i16]) -> Result<()> {
            Ok(())
        }

        fn partials(&self) -> &Receiver<LocalEvent> {
            &self.rx
        }

        fn finalize(&mut self) -> Result<()> {
            if self.finished {
                return Ok(());
            }
            self.finished = true;
            let _ = self.tx.send(LocalEvent::Error("stream boom".into()));
            anyhow::bail!("stream boom")
        }
    }

    #[test]
    fn layered_streaming_error_is_terminal_no_final() {
        let engine = LayeredLocalEngine::new(LayeredLocalEngineConfig {
            streaming: Arc::new(FailingStreamEngine),
            finalizer: Arc::new(MockFinalizer::default()),
        });
        let mut session = engine.start().unwrap();
        session.feed(&[1, 2, 3, 4]).unwrap();
        let err = session.finalize().unwrap_err();
        assert!(err.to_string().contains("stream boom"), "{err}");

        let events: Vec<_> = session.partials().try_iter().collect();
        let errors = events
            .iter()
            .filter(|e| matches!(e, LocalEvent::Error(_)))
            .count();
        let finals = events
            .iter()
            .filter(|e| matches!(e, LocalEvent::Final(_)))
            .count();
        assert_eq!(errors, 1, "expected exactly one Error, got {events:?}");
        assert_eq!(finals, 0, "must not emit Final after streaming Error: {events:?}");
    }
}

//! Deterministic offline mock local engine — no models, no network.
//!
//! Partials are revealed as a function of how much *audio* has been fed, not
//! wall-clock time, so tests are deterministic on any machine.

use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};

use crate::audio::SAMPLE_RATE;
use crate::engine::{LocalEngine, LocalEvent, LocalSession};

/// Default transcript used by the mock (matches the committed speech fixture intent).
pub const DEFAULT_TRANSCRIPT: &str = "Hello, this is Iris dictation.";

#[derive(Debug, Clone)]
pub struct MockLocalEngineConfig {
    pub transcript: String,
    /// How much fed audio each additional word represents.
    pub audio_per_word: Duration,
    /// Optional deferred delivery of `Final` after `finalize()` returns.
    /// Zero (default) delivers `Final` before `finalize` returns, matching
    /// batch engines. Non-zero spawns a sleeper thread for timing tests only.
    pub finalize_delay: Duration,
}

impl Default for MockLocalEngineConfig {
    fn default() -> Self {
        Self {
            transcript: DEFAULT_TRANSCRIPT.to_string(),
            audio_per_word: Duration::from_millis(350),
            finalize_delay: Duration::ZERO,
        }
    }
}

pub struct MockLocalEngine {
    config: MockLocalEngineConfig,
}

impl MockLocalEngine {
    pub fn new(config: MockLocalEngineConfig) -> Self {
        Self { config }
    }
}

impl Default for MockLocalEngine {
    fn default() -> Self {
        Self::new(MockLocalEngineConfig::default())
    }
}

impl LocalEngine for MockLocalEngine {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn streams_partials(&self) -> bool {
        true
    }

    fn start(&self) -> Result<Box<dyn LocalSession>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let words: Vec<String> = self
            .config
            .transcript
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let _ = tx.send(LocalEvent::Ready);
        Ok(Box::new(MockSession {
            config: self.config.clone(),
            words,
            tx,
            rx,
            samples: 0,
            revealed: 0,
            finished: false,
        }))
    }
}

struct MockSession {
    config: MockLocalEngineConfig,
    words: Vec<String>,
    tx: Sender<LocalEvent>,
    rx: Receiver<LocalEvent>,
    samples: usize,
    revealed: usize,
    finished: bool,
}

impl MockSession {
    fn reveal_upto(&mut self) {
        let samples_per_word =
            (self.config.audio_per_word.as_secs_f64() * SAMPLE_RATE as f64) as usize;
        if samples_per_word == 0 {
            return;
        }
        let should = (self.samples / samples_per_word).min(self.words.len());
        while self.revealed < should {
            self.revealed += 1;
            let text = self.words[..self.revealed].join(" ");
            let _ = self.tx.send(LocalEvent::Partial(text));
        }
    }
}

impl LocalSession for MockSession {
    fn feed(&mut self, pcm: &[i16]) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.samples += pcm.len();
        self.reveal_upto();
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
        // Reveal any remaining words as partials, then Final.
        while self.revealed < self.words.len() {
            self.revealed += 1;
            let text = self.words[..self.revealed].join(" ");
            let _ = self.tx.send(LocalEvent::Partial(text));
        }
        let delay = self.config.finalize_delay;
        let tx = self.tx.clone();
        let transcript = self.config.transcript.clone();
        if delay.is_zero() {
            let _ = tx.send(LocalEvent::Final(transcript));
        } else {
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                let _ = tx.send(LocalEvent::Final(transcript));
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::read_wav_pcm16;
    use crate::speech_fixture;

    #[test]
    fn mock_streams_partials_then_final() {
        let engine = MockLocalEngine::default();
        let mut session = engine.start().unwrap();
        let pcm = read_wav_pcm16(&speech_fixture()).unwrap();
        // Feed in small frames.
        for chunk in pcm.chunks(1600) {
            session.feed(chunk).unwrap();
        }
        session.finalize().unwrap();

        let mut partials = Vec::new();
        let mut final_text = None;
        for ev in session.partials().try_iter() {
            match ev {
                LocalEvent::Partial(t) => partials.push(t),
                LocalEvent::Final(t) => final_text = Some(t),
                LocalEvent::Ready => {}
                LocalEvent::Error(e) => panic!("error: {e}"),
            }
        }
        assert!(!partials.is_empty(), "expected at least one partial");
        assert_eq!(final_text.as_deref(), Some(DEFAULT_TRANSCRIPT));
    }

    #[test]
    fn mock_silence_still_finalizes_transcript() {
        // The mock always returns its configured transcript — VAD is the
        // finalizer's job. This documents that split of responsibility.
        let engine = MockLocalEngine::default();
        let mut session = engine.start().unwrap();
        session.feed(&vec![0i16; SAMPLE_RATE as usize / 2]).unwrap();
        session.finalize().unwrap();
        let fin = session
            .partials()
            .try_iter()
            .find_map(|e| match e {
                LocalEvent::Final(t) => Some(t),
                _ => None,
            });
        assert_eq!(fin.as_deref(), Some(DEFAULT_TRANSCRIPT));
    }
}

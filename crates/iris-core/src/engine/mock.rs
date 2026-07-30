//! A deterministic engine that needs no key, no network and no clock.
//!
//! Partials are revealed as a function of how much *audio* has been pushed, not
//! of wall-clock time, so a test that feeds the same WAV always sees the same
//! sequence of events regardless of how fast the machine is.
//!
//! The optional delays in [`MockConfig`] exist so the harness can model a cloud
//! engine's finalisation cost without a key; they are zero by default, which
//! keeps `--engine mock` instant.

use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};

use crate::audio;

use super::{Engine, Session, TranscriptEvent};

/// What the mock pretends it heard. Matches `assets/speech-16k.wav`.
pub const DEFAULT_TRANSCRIPT: &str =
    "The quick brown fox jumps over the lazy dog. Iris turns speech into text instantly.";

#[derive(Debug, Clone)]
pub struct MockConfig {
    /// The final transcript.
    pub transcript: String,
    /// How much pushed audio each additional word represents.
    pub audio_per_word: Duration,
    /// Simulated transport setup, applied before `Connected`.
    pub connect_delay: Duration,
    /// Simulated finalisation cost, applied between `finish()` and `Final`.
    pub finalize_delay: Duration,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            transcript: DEFAULT_TRANSCRIPT.to_string(),
            audio_per_word: Duration::from_millis(350),
            connect_delay: Duration::ZERO,
            finalize_delay: Duration::ZERO,
        }
    }
}

pub struct MockEngine {
    config: MockConfig,
}

impl MockEngine {
    pub fn new(config: MockConfig) -> Self {
        Self { config }
    }

    /// Model a cloud engine's timing without a key: partials while speaking and
    /// a finalisation round-trip after key-release.
    pub fn simulating_cloud(connect: Duration, finalize: Duration) -> Self {
        Self::new(MockConfig {
            connect_delay: connect,
            finalize_delay: finalize,
            ..MockConfig::default()
        })
    }
}

impl Engine for MockEngine {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn open(&self) -> Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let words: Vec<String> = self
            .config
            .transcript
            .split_whitespace()
            .map(str::to_string)
            .collect();

        if self.config.connect_delay.is_zero() {
            let _ = tx.send(TranscriptEvent::Connected);
        } else {
            let tx = tx.clone();
            let delay = self.config.connect_delay;
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                let _ = tx.send(TranscriptEvent::Connected);
            });
        }

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
    config: MockConfig,
    words: Vec<String>,
    tx: Sender<TranscriptEvent>,
    rx: Receiver<TranscriptEvent>,
    samples: usize,
    revealed: usize,
    finished: bool,
}

impl Session for MockSession {
    fn push(&mut self, pcm: &[i16]) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.samples += pcm.len();

        let per_word = self.config.audio_per_word.as_secs_f64().max(1e-3);
        let due = (audio::secs(self.samples) / per_word).floor() as usize;
        let due = due.min(self.words.len());
        if due > self.revealed {
            self.revealed = due;
            let _ = self
                .tx
                .send(TranscriptEvent::Partial(self.words[..due].join(" ")));
        }
        Ok(())
    }

    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.rx
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let text = self.config.transcript.clone();
        if self.config.finalize_delay.is_zero() {
            let _ = self.tx.send(TranscriptEvent::Final(text));
        } else {
            let tx = self.tx.clone();
            let delay = self.config.finalize_delay;
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                let _ = tx.send(TranscriptEvent::Final(text));
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::SAMPLE_RATE;

    fn silence(secs: f64) -> Vec<i16> {
        vec![0i16; (SAMPLE_RATE as f64 * secs) as usize]
    }

    fn drain(session: &dyn Session) -> Vec<TranscriptEvent> {
        session.events().try_iter().collect()
    }

    #[test]
    fn connects_immediately_by_default() {
        let engine = MockEngine::new(MockConfig::default());
        let session = engine.open().unwrap();
        assert_eq!(drain(&*session), vec![TranscriptEvent::Connected]);
    }

    #[test]
    fn partials_grow_monotonically_with_pushed_audio() {
        let engine = MockEngine::new(MockConfig::default());
        let mut session = engine.open().unwrap();
        let _ = drain(&*session);

        let mut seen: Vec<String> = Vec::new();
        for _ in 0..15 {
            session.push(&silence(0.35)).unwrap();
            for ev in drain(&*session) {
                if let TranscriptEvent::Partial(t) = ev {
                    seen.push(t);
                }
            }
        }

        assert!(seen.len() > 3, "expected several partials, got {seen:?}");
        for pair in seen.windows(2) {
            assert!(
                pair[1].starts_with(&pair[0]),
                "partials must extend, not rewrite: {:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(DEFAULT_TRANSCRIPT.starts_with(seen.last().unwrap().as_str()));
    }

    #[test]
    fn same_audio_yields_the_same_events_every_run() {
        let run = || {
            let engine = MockEngine::new(MockConfig::default());
            let mut session = engine.open().unwrap();
            for _ in 0..10 {
                session.push(&silence(0.2)).unwrap();
            }
            session.finish().unwrap();
            drain(&*session)
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn finish_emits_the_full_transcript_and_is_idempotent() {
        let engine = MockEngine::new(MockConfig::default());
        let mut session = engine.open().unwrap();
        let _ = drain(&*session);
        session.finish().unwrap();
        session.finish().unwrap();
        assert_eq!(
            drain(&*session),
            vec![TranscriptEvent::Final(DEFAULT_TRANSCRIPT.to_string())]
        );
    }

    #[test]
    fn simulated_delays_are_observed() {
        let engine =
            MockEngine::simulating_cloud(Duration::from_millis(10), Duration::from_millis(40));
        let mut session = engine.open().unwrap();
        assert_eq!(
            session
                .events()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            TranscriptEvent::Connected
        );

        let t0 = std::time::Instant::now();
        session.finish().unwrap();
        let ev = session
            .events()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(ev, TranscriptEvent::Final(_)));
        assert!(
            t0.elapsed() >= Duration::from_millis(35),
            "delay not applied"
        );
    }
}

//! The portable driver: audio in, transcript out, timeline stamped.
//!
//! Deliberately knows nothing about hotkeys, microphones or `SendInput`. The
//! Windows pipeline wraps it with those; the harness wraps it with a WAV file.
//! Both produce a [`Timeline`] from the same code, so a number measured in CI is
//! the same number measured on a desk.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::audio;
use crate::engine::{Engine, Session, TranscriptEvent};
use crate::latency::{Mark, Timeline};
use crate::vlog;

/// How long to wait for the final transcript after `finish()` before giving up.
pub const DEFAULT_FINAL_TIMEOUT: Duration = Duration::from_secs(10);

/// A completed dictation.
#[derive(Debug, Clone)]
pub struct DictationOutcome {
    pub text: String,
    pub timeline: Timeline,
}

/// Owns one engine session for the duration of one dictation.
pub struct Dictation {
    session: Box<dyn Session>,
    timeline: Timeline,
    samples: usize,
    latest_partial: String,
    ended: Option<Ending>,
}

enum Ending {
    Final(String),
    Error(String),
    Closed,
}

impl Dictation {
    /// Open a session. Stamps [`Mark::KeyDown`] as the timeline origin and
    /// [`Mark::SessionOpen`] once the engine hands back a session.
    ///
    /// Call this the instant the hotkey goes down — for streaming engines the
    /// connection then sets itself up while the user is drawing breath.
    pub fn start(engine: &dyn Engine) -> Result<Self> {
        Self::start_at(engine, Instant::now())
    }

    /// As [`Dictation::start`], but with the key-press instant supplied by the
    /// hotkey hook rather than measured on arrival.
    pub fn start_at(engine: &dyn Engine, key_down: Instant) -> Result<Self> {
        let mut timeline = Timeline::start_at(engine.name(), key_down);
        let session = engine.open()?;
        timeline.mark(Mark::SessionOpen);
        Ok(Self {
            session,
            timeline,
            samples: 0,
            latest_partial: String::new(),
            ended: None,
        })
    }

    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    pub fn timeline_mut(&mut self) -> &mut Timeline {
        &mut self.timeline
    }

    /// Seconds of audio pushed so far.
    pub fn audio_secs(&self) -> f64 {
        audio::secs(self.samples)
    }

    /// The most recent interim transcript, for an overlay to render.
    pub fn latest_partial(&self) -> &str {
        &self.latest_partial
    }

    /// Push 16 kHz mono PCM. Stamps [`Mark::CaptureStart`] on the first frame.
    ///
    /// Deliberately does *not* consume engine events: draining them here would
    /// steal them from the caller's [`Dictation::poll`] or [`Dictation::events`]
    /// loop, and an interim transcript that nothing renders is worse than
    /// useless — it is the overlay silently going dead.
    pub fn feed(&mut self, pcm: &[i16]) -> Result<()> {
        if self.samples == 0 && !pcm.is_empty() {
            self.timeline.mark(Mark::CaptureStart);
        }
        self.samples += pcm.len();
        self.session.push(pcm)
    }

    /// A handle on the engine's event stream, for callers that want to `select!`
    /// over audio, hotkeys and transcripts at once.
    ///
    /// This is the seam the real app's overlay hangs off: an interim transcript
    /// is delivered the instant it arrives rather than at the next poll tick.
    /// Pair it with [`Dictation::absorb_event`].
    pub fn events(&self) -> crossbeam_channel::Receiver<TranscriptEvent> {
        self.session.events().clone()
    }

    /// Fold one event into the timeline. See [`Dictation::events`].
    pub fn absorb_event(&mut self, event: TranscriptEvent, on_partial: &mut dyn FnMut(&str)) {
        self.absorb(event, on_partial);
    }

    /// Drain whatever the engine has produced, stamping marks and invoking
    /// `on_partial` for each interim transcript. Never blocks.
    ///
    /// Simpler than [`Dictation::events`], at the cost of resolving partials no
    /// finer than the caller's polling interval.
    pub fn poll(&mut self, on_partial: &mut dyn FnMut(&str)) {
        while let Ok(event) = self.session.events().try_recv() {
            self.absorb(event, on_partial);
        }
    }

    fn absorb(&mut self, event: TranscriptEvent, on_partial: &mut dyn FnMut(&str)) {
        match event {
            TranscriptEvent::Connected => self.timeline.mark(Mark::StreamReady),
            TranscriptEvent::Partial(text) => {
                self.timeline.mark(Mark::FirstPartial);
                self.timeline.partials += 1;
                self.latest_partial = text;
                on_partial(&self.latest_partial);
            }
            TranscriptEvent::Final(text) => {
                self.timeline.mark(Mark::FinalTranscript);
                self.ended = Some(Ending::Final(text));
            }
            TranscriptEvent::Error(message) => {
                self.ended = Some(Ending::Error(message));
            }
        }
    }

    /// End of speech. Stamps [`Mark::KeyUp`], tells the engine to finalise, then
    /// waits up to `timeout` for the final transcript.
    ///
    /// The wait is where perceived latency lives, so `on_partial` keeps firing
    /// throughout: an overlay can keep updating right up to the moment the real
    /// text lands.
    pub fn finish(
        mut self,
        timeout: Duration,
        on_partial: &mut dyn FnMut(&str),
    ) -> Result<DictationOutcome> {
        self.timeline.mark(Mark::KeyUp);
        self.session.finish()?;
        vlog!("finalising after {:.2}s of audio", self.audio_secs());

        let deadline = Instant::now() + timeout;
        while self.ended.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.session.events().recv_timeout(remaining) {
                Ok(event) => self.absorb(event, on_partial),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    self.ended.get_or_insert(Ending::Closed);
                }
            }
        }

        self.timeline.audio_secs = self.audio_secs();

        match self.ended {
            Some(Ending::Final(text)) => {
                self.timeline.transcript = text.clone();
                Ok(DictationOutcome {
                    text,
                    timeline: self.timeline,
                })
            }
            Some(Ending::Error(message)) => {
                Err(anyhow!("{} engine: {message}", self.timeline.engine))
            }
            Some(Ending::Closed) => Err(anyhow!(
                "{} engine closed without returning a transcript",
                self.timeline.engine
            )),
            None => Err(anyhow!(
                "{} engine did not return a transcript within {:.1}s",
                self.timeline.engine,
                timeout.as_secs_f64()
            )),
        }
    }
}

/// Run a whole dictation from a slice of 16 kHz mono PCM.
///
/// `pace` controls whether frames are fed at wall-clock speed (what a person
/// speaking looks like, and the only way to measure streaming honestly) or as
/// fast as the CPU allows.
pub fn run_offline(
    engine: &dyn Engine,
    pcm: &[i16],
    pace: Pace,
    on_partial: &mut dyn FnMut(&str),
) -> Result<DictationOutcome> {
    let mut dictation = Dictation::start(engine)?;
    let events = dictation.events();
    let frame_time = Duration::from_millis(audio::FRAME_MS as u64);
    let started = Instant::now();

    for (i, chunk) in pcm.chunks(audio::FRAME_SAMPLES).enumerate() {
        if let Pace::Realtime = pace {
            // Frame `i` would have been captured at `i * 20 ms`. Waiting until
            // that absolute deadline stops per-frame overhead accumulating into
            // a drift that would fake a latency advantage.
            //
            // Blocking on the event channel rather than sleeping means an
            // interim transcript is timestamped when it arrives, not up to one
            // frame later.
            let deadline = started + frame_time * i as u32;
            while let Ok(event) = events.recv_deadline(deadline) {
                dictation.absorb_event(event, on_partial);
            }
        }
        dictation.feed(chunk)?;
        dictation.poll(on_partial);
    }

    dictation.finish(DEFAULT_FINAL_TIMEOUT, on_partial)
}

/// How [`run_offline`] feeds audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// Feed at 1× wall clock, like a person speaking.
    Realtime,
    /// Feed as fast as possible. Useful for correctness runs; meaningless for
    /// latency, because it lets a streaming engine work ahead.
    Fast,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{MockConfig, MockEngine};

    fn tone(secs: f64) -> Vec<i16> {
        let n = (audio::SAMPLE_RATE as f64 * secs) as usize;
        (0..n)
            .map(|i| {
                ((2.0 * std::f64::consts::PI * 220.0 * i as f64 / 16_000.0).sin() * 8000.0) as i16
            })
            .collect()
    }

    #[test]
    fn drives_the_mock_engine_to_a_final_transcript() {
        let engine = MockEngine::new(MockConfig::default());
        let mut partials = Vec::new();
        let outcome = run_offline(&engine, &tone(3.0), Pace::Fast, &mut |p| {
            partials.push(p.to_string())
        })
        .unwrap();

        assert_eq!(outcome.text, crate::engine::mock::DEFAULT_TRANSCRIPT);
        assert!(!partials.is_empty(), "partials should stream while feeding");
        assert_eq!(outcome.timeline.partials, partials.len());
    }

    #[test]
    fn timeline_marks_are_ordered() {
        let engine = MockEngine::new(MockConfig::default());
        let outcome = run_offline(&engine, &tone(2.0), Pace::Fast, &mut |_| {}).unwrap();
        let t = &outcome.timeline;

        for mark in [
            Mark::SessionOpen,
            Mark::CaptureStart,
            Mark::StreamReady,
            Mark::FirstPartial,
            Mark::KeyUp,
            Mark::FinalTranscript,
        ] {
            assert!(t.at(mark).is_some(), "{} was never stamped", mark.label());
        }
        assert!(t.at(Mark::KeyDown).unwrap() <= t.at(Mark::CaptureStart).unwrap());
        assert!(t.at(Mark::FirstPartial).unwrap() <= t.at(Mark::KeyUp).unwrap());
        assert!(t.at(Mark::KeyUp).unwrap() <= t.at(Mark::FinalTranscript).unwrap());
        assert!((t.audio_secs - 2.0).abs() < 0.05);
    }

    #[test]
    fn realtime_pacing_takes_about_as_long_as_the_audio() {
        let engine = MockEngine::new(MockConfig::default());
        let started = Instant::now();
        let outcome = run_offline(&engine, &tone(1.0), Pace::Realtime, &mut |_| {}).unwrap();
        let wall = started.elapsed().as_secs_f64();
        assert!(
            (0.9..1.6).contains(&wall),
            "1 s of audio should take ~1 s to stream, took {wall:.2}s"
        );
        // The point of streaming: the wait after key-release is a rounding
        // error next to the length of the utterance.
        let perceived = outcome.timeline.perceived().unwrap();
        assert!(perceived < Duration::from_millis(50), "{perceived:?}");
    }

    #[test]
    fn a_failing_engine_surfaces_its_message() {
        struct Failing;
        struct FailingSession {
            events: crossbeam_channel::Receiver<TranscriptEvent>,
        }
        impl Engine for Failing {
            fn name(&self) -> &'static str {
                "failing"
            }
            fn open(&self) -> Result<Box<dyn Session>> {
                let (tx, rx) = crossbeam_channel::unbounded();
                tx.send(TranscriptEvent::Error("no key".into())).unwrap();
                Ok(Box::new(FailingSession { events: rx }))
            }
        }
        impl Session for FailingSession {
            fn push(&mut self, _: &[i16]) -> Result<()> {
                Ok(())
            }
            fn events(&self) -> &crossbeam_channel::Receiver<TranscriptEvent> {
                &self.events
            }
            fn finish(&mut self) -> Result<()> {
                Ok(())
            }
        }

        let err = run_offline(&Failing, &tone(0.2), Pace::Fast, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("no key"), "{err}");
    }

    #[test]
    fn a_silent_engine_times_out_instead_of_hanging() {
        struct Silent;
        struct SilentSession {
            events: crossbeam_channel::Receiver<TranscriptEvent>,
            _keep: crossbeam_channel::Sender<TranscriptEvent>,
        }
        impl Engine for Silent {
            fn name(&self) -> &'static str {
                "silent"
            }
            fn open(&self) -> Result<Box<dyn Session>> {
                let (tx, rx) = crossbeam_channel::unbounded();
                Ok(Box::new(SilentSession {
                    events: rx,
                    _keep: tx,
                }))
            }
        }
        impl Session for SilentSession {
            fn push(&mut self, _: &[i16]) -> Result<()> {
                Ok(())
            }
            fn events(&self) -> &crossbeam_channel::Receiver<TranscriptEvent> {
                &self.events
            }
            fn finish(&mut self) -> Result<()> {
                Ok(())
            }
        }

        let dictation = Dictation::start(&Silent).unwrap();
        let err = dictation
            .finish(Duration::from_millis(50), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not return a transcript"), "{err}");
    }
}

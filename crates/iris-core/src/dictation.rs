//! The portable driver: audio in, transcript out, timeline stamped.
//!
//! Deliberately knows nothing about hotkeys, microphones or `SendInput`. The
//! Windows pipeline wraps it with those; the harness wraps it with a WAV file.
//! Both produce a [`Timeline`] from the same code, so a number measured in CI is
//! the same number measured on a desk.
//!
//! # Hold integrity
//!
//! A continuous key-hold is one utterance. [`TranscriptEvent::Final`] before
//! [`Dictation::finish`] is demoted to a partial so segment-oriented or buggy
//! engines cannot truncate mid-hold; `finish` always calls [`Session::finish`]
//! and waits. On timeout, close, or error, a non-empty `latest_partial` is
//! salvaged so words already shown on the overlay are not discarded. Prefer the
//! longer of Final vs latest partial when both exist.

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
    /// Set when [`Dictation::finish`] starts. Premature terminal events before
    /// this are not allowed to end the utterance (see [`Dictation::absorb`]).
    finishing: bool,
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
            finishing: false,
            ended: None,
        })
    }

    /// As [`Dictation::start_at`], but for a session opened *before* the key
    /// went down rather than because of it.
    ///
    /// A network engine's connect latency (TLS handshake, first round trip)
    /// is otherwise paid again on every single dictation. A caller that keeps
    /// one spare session open ahead of time — opened right after the previous
    /// dictation finished, so the wait overlaps idle time instead of speech —
    /// can hand it here instead, and a hold shorter than that connect latency
    /// still has a transport that is already up. [`Mark::SessionOpen`] is
    /// still stamped at `key_down`: the session existing earlier doesn't make
    /// the engine open faster, it just moves *when* that cost was paid.
    pub fn start_with_session(
        engine_name: &'static str,
        session: Box<dyn Session>,
        key_down: Instant,
    ) -> Self {
        let mut timeline = Timeline::start_at(engine_name, key_down);
        timeline.mark(Mark::SessionOpen);
        Self {
            session,
            timeline,
            samples: 0,
            latest_partial: String::new(),
            finishing: false,
            ended: None,
        }
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
                self.note_partial(text, on_partial);
            }
            TranscriptEvent::Final(text) => {
                // A streaming ASR may emit segment-level "finals" or even a
                // premature session Final while the user is still holding the
                // key. Accepting that as terminal makes `finish()` return
                // immediately and drops every later word. Until we have asked
                // the engine to finalise, treat Final as a (sticky) partial.
                if !self.finishing {
                    let text = best_transcript(text, &self.latest_partial);
                    self.note_partial(text, on_partial);
                    return;
                }
                self.timeline.mark(Mark::FinalTranscript);
                let text = best_transcript(text, &self.latest_partial);
                self.latest_partial = text.clone();
                self.ended = Some(Ending::Final(text));
            }
            TranscriptEvent::Error(message) => {
                // Errors are real even mid-hold (missing key, socket death).
                // Keep the richest partial we saw so finish can salvage words
                // when the engine died after producing useful text.
                self.ended = Some(Ending::Error(message));
            }
        }
    }

    fn note_partial(&mut self, text: String, on_partial: &mut dyn FnMut(&str)) {
        self.timeline.mark(Mark::FirstPartial);
        self.timeline.partials += 1;
        self.latest_partial = text;
        on_partial(&self.latest_partial);
    }

    /// End of speech. Stamps [`Mark::KeyUp`], tells the engine to finalise, then
    /// waits up to `timeout` for the final transcript.
    ///
    /// The wait is where perceived latency lives, so `on_partial` keeps firing
    /// throughout: an overlay can keep updating right up to the moment the real
    /// text lands.
    ///
    /// A terminal event that arrived *before* this call (a premature Final from
    /// a buggy or segment-oriented engine) does not short-circuit the wait:
    /// [`Session::finish`] is always invoked, and a later, richer Final or the
    /// best partial wins.
    pub fn finish(
        mut self,
        timeout: Duration,
        on_partial: &mut dyn FnMut(&str),
    ) -> Result<DictationOutcome> {
        self.timeline.mark(Mark::KeyUp);
        // Drop any premature terminal Final so we wait for the post-finish one.
        // Errors stay: the engine already failed and finish cannot un-fail it.
        if matches!(self.ended, Some(Ending::Final(_))) {
            self.ended = None;
        }
        // Absorb anything already queued while still pre-finish so a segment
        // Final sitting on the channel is demoted into latest_partial rather
        // than short-circuiting the wait below before session.finish() runs.
        self.poll(on_partial);
        self.finishing = true;
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

        // Prefer a real Final; otherwise salvage the best partial rather than
        // discarding words the user already saw on the overlay.
        let salvaged = (!self.latest_partial.trim().is_empty()).then(|| {
            let text = self.latest_partial.clone();
            self.timeline.mark(Mark::FinalTranscript);
            text
        });

        match self.ended {
            Some(Ending::Final(text)) => {
                let text = best_transcript(text, salvaged.as_deref().unwrap_or(""));
                self.timeline.transcript = text.clone();
                Ok(DictationOutcome {
                    text,
                    timeline: self.timeline,
                })
            }
            Some(Ending::Error(message)) => {
                if let Some(text) = salvaged {
                    vlog!(
                        "engine error after partial transcript; salvaging {} chars",
                        text.chars().count()
                    );
                    self.timeline.transcript = text.clone();
                    Ok(DictationOutcome {
                        text,
                        timeline: self.timeline,
                    })
                } else {
                    Err(anyhow!("{} engine: {message}", self.timeline.engine))
                }
            }
            Some(Ending::Closed) | None => {
                if let Some(text) = salvaged {
                    self.timeline.transcript = text.clone();
                    Ok(DictationOutcome {
                        text,
                        timeline: self.timeline,
                    })
                } else if matches!(self.ended, Some(Ending::Closed)) {
                    Err(anyhow!(
                        "{} engine closed without returning a transcript",
                        self.timeline.engine
                    ))
                } else {
                    Err(anyhow!(
                        "{} engine did not return a transcript within {:.1}s",
                        self.timeline.engine,
                        timeout.as_secs_f64()
                    ))
                }
            }
        }
    }
}

/// Prefer the longer hypothesis. Streaming engines sometimes emit a short
/// session Final while a richer partial still holds the rest of the utterance.
fn best_transcript(final_text: String, partial: &str) -> String {
    let final_text = final_text.trim();
    let partial = partial.trim();
    if partial.chars().count() > final_text.chars().count() {
        partial.to_string()
    } else {
        final_text.to_string()
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
    fn start_with_session_skips_open_but_still_stamps_session_open() {
        let engine = MockEngine::new(MockConfig::default());
        let session = engine.open().unwrap();
        let key_down = Instant::now();

        let dictation = Dictation::start_with_session("mock", session, key_down);

        assert_eq!(dictation.timeline().engine, "mock");
        assert!(dictation.timeline().at(Mark::KeyDown).is_some());
        assert!(
            dictation.timeline().at(Mark::SessionOpen).is_some(),
            "a pre-opened session must still stamp SessionOpen"
        );
    }

    #[test]
    fn a_prewarmed_session_drives_a_dictation_identically_to_a_fresh_one() {
        let engine = MockEngine::new(MockConfig::default());
        let session = engine.open().unwrap();

        let mut dictation = Dictation::start_with_session("mock", session, Instant::now());
        for chunk in tone(1.0).chunks(audio::FRAME_SAMPLES) {
            dictation.feed(chunk).unwrap();
        }
        let outcome = dictation
            .finish(Duration::from_secs(1), &mut |_| {})
            .unwrap();

        assert_eq!(outcome.text, crate::engine::mock::DEFAULT_TRANSCRIPT);
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

    /// Engine that emits a terminal Final after the first half-second of audio
    /// while more audio is still being fed — the Deepgram-segment / pump-exit
    /// failure mode that used to make `finish()` return "hello" and drop the rest.
    struct EarlyFinalEngine;

    struct EarlyFinalSession {
        tx: crossbeam_channel::Sender<TranscriptEvent>,
        rx: crossbeam_channel::Receiver<TranscriptEvent>,
        samples: usize,
        sent_early: bool,
        finished: bool,
    }

    impl Engine for EarlyFinalEngine {
        fn name(&self) -> &'static str {
            "early-final"
        }
        fn open(&self) -> Result<Box<dyn Session>> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let _ = tx.send(TranscriptEvent::Connected);
            Ok(Box::new(EarlyFinalSession {
                tx,
                rx,
                samples: 0,
                sent_early: false,
                finished: false,
            }))
        }
    }

    impl Session for EarlyFinalSession {
        fn push(&mut self, pcm: &[i16]) -> Result<()> {
            if self.finished {
                return Ok(());
            }
            self.samples += pcm.len();
            let secs = audio::secs(self.samples);
            if secs >= 0.5 && !self.sent_early {
                self.sent_early = true;
                let _ = self.tx.send(TranscriptEvent::Partial("hello".into()));
                let _ = self.tx.send(TranscriptEvent::Final("hello".into()));
            }
            if secs >= 1.5 && self.sent_early {
                let _ = self
                    .tx
                    .send(TranscriptEvent::Partial("hello world more speech".into()));
            }
            Ok(())
        }
        fn events(&self) -> &crossbeam_channel::Receiver<TranscriptEvent> {
            &self.rx
        }
        fn finish(&mut self) -> Result<()> {
            if self.finished {
                return Ok(());
            }
            self.finished = true;
            let _ = self.tx.send(TranscriptEvent::Final(
                "hello world more speech after keyup".into(),
            ));
            Ok(())
        }
    }

    #[test]
    fn early_final_does_not_truncate_the_utterance() {
        let mut partials = Vec::new();
        let outcome = run_offline(&EarlyFinalEngine, &tone(2.0), Pace::Fast, &mut |p| {
            partials.push(p.to_string())
        })
        .expect("dictation");

        assert!(
            outcome.text.contains("more speech"),
            "premature Final must not win; got {:?} (partials={partials:?})",
            outcome.text
        );
        assert!(
            partials.iter().any(|p| p.contains("more speech")),
            "later audio must still produce partials: {partials:?}"
        );
    }

    /// Engine that dies after an early Final and never emits another — finish
    /// must salvage the richest partial rather than return only the first word.
    struct EarlyFinalThenDie;

    struct EarlyFinalThenDieSession {
        tx: crossbeam_channel::Sender<TranscriptEvent>,
        rx: crossbeam_channel::Receiver<TranscriptEvent>,
        samples: usize,
        sent_early: bool,
        finished: bool,
    }

    impl Engine for EarlyFinalThenDie {
        fn name(&self) -> &'static str {
            "early-final-die"
        }
        fn open(&self) -> Result<Box<dyn Session>> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let _ = tx.send(TranscriptEvent::Connected);
            Ok(Box::new(EarlyFinalThenDieSession {
                tx,
                rx,
                samples: 0,
                sent_early: false,
                finished: false,
            }))
        }
    }

    impl Session for EarlyFinalThenDieSession {
        fn push(&mut self, pcm: &[i16]) -> Result<()> {
            if self.finished {
                return Ok(());
            }
            self.samples += pcm.len();
            if audio::secs(self.samples) >= 0.4 && !self.sent_early {
                self.sent_early = true;
                let _ = self.tx.send(TranscriptEvent::Partial("hello".into()));
                let _ = self.tx.send(TranscriptEvent::Final("hello".into()));
            }
            if audio::secs(self.samples) >= 1.0 {
                let _ = self
                    .tx
                    .send(TranscriptEvent::Partial("hello there full sentence".into()));
            }
            Ok(())
        }
        fn events(&self) -> &crossbeam_channel::Receiver<TranscriptEvent> {
            &self.rx
        }
        fn finish(&mut self) -> Result<()> {
            // No terminal event after finish — forces salvage of latest_partial.
            self.finished = true;
            Ok(())
        }
    }

    #[test]
    fn finish_salvages_latest_partial_when_final_never_arrives() {
        let outcome = run_offline(&EarlyFinalThenDie, &tone(1.5), Pace::Fast, &mut |_| {})
            .expect("should salvage partial");
        assert!(
            outcome.text.contains("full sentence"),
            "expected salvage of richest partial, got {:?}",
            outcome.text
        );
    }

    #[test]
    fn best_transcript_prefers_the_longer_hypothesis() {
        assert_eq!(best_transcript("hi".into(), "hello there"), "hello there");
        assert_eq!(best_transcript("hello there".into(), "hi"), "hello there");
        assert_eq!(best_transcript("  same  ".into(), "same"), "same");
    }

    /// Short premature Final after a longer interim must not erase the interim
    /// from `latest_partial` — finish salvages whatever is stored there.
    struct ShortFinalAfterLongPartial;

    struct ShortFinalAfterLongPartialSession {
        tx: crossbeam_channel::Sender<TranscriptEvent>,
        rx: crossbeam_channel::Receiver<TranscriptEvent>,
        samples: usize,
        phase: u8,
        finished: bool,
    }

    impl Engine for ShortFinalAfterLongPartial {
        fn name(&self) -> &'static str {
            "short-final-after-long-partial"
        }
        fn open(&self) -> Result<Box<dyn Session>> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let _ = tx.send(TranscriptEvent::Connected);
            Ok(Box::new(ShortFinalAfterLongPartialSession {
                tx,
                rx,
                samples: 0,
                phase: 0,
                finished: false,
            }))
        }
    }

    impl Session for ShortFinalAfterLongPartialSession {
        fn push(&mut self, pcm: &[i16]) -> Result<()> {
            if self.finished {
                return Ok(());
            }
            self.samples += pcm.len();
            let secs = audio::secs(self.samples);
            if secs >= 0.4 && self.phase == 0 {
                self.phase = 1;
                let _ = self
                    .tx
                    .send(TranscriptEvent::Partial("hello there full sentence".into()));
            }
            if secs >= 0.8 && self.phase == 1 {
                self.phase = 2;
                let _ = self.tx.send(TranscriptEvent::Final("hello".into()));
            }
            Ok(())
        }
        fn events(&self) -> &crossbeam_channel::Receiver<TranscriptEvent> {
            &self.rx
        }
        fn finish(&mut self) -> Result<()> {
            self.finished = true;
            Ok(())
        }
    }

    #[test]
    fn demoted_short_final_does_not_clobber_longer_partial() {
        let mut partials = Vec::new();
        let outcome = run_offline(
            &ShortFinalAfterLongPartial,
            &tone(1.2),
            Pace::Fast,
            &mut |p| partials.push(p.to_string()),
        )
        .expect("should salvage longer partial");

        assert!(
            outcome.text.contains("full sentence"),
            "short demoted Final must not erase longer interim; got {:?} (partials={partials:?})",
            outcome.text
        );
        assert!(
            partials.iter().any(|p| p.contains("full sentence")),
            "overlay should keep the longer hypothesis: {partials:?}"
        );
    }

    /// Queues a short Final before `finish()` is called, then emits a longer
    /// Final from `Session::finish`. Mimics the app `select!` race where key-up
    /// and a channel Final are both ready and the Final has not been absorb()ed
    /// yet — draining must demote it so the post-finish Final still wins.
    struct QueuedFinalBeforeFinish;

    struct QueuedFinalBeforeFinishSession {
        tx: crossbeam_channel::Sender<TranscriptEvent>,
        rx: crossbeam_channel::Receiver<TranscriptEvent>,
        finished: bool,
    }

    impl Engine for QueuedFinalBeforeFinish {
        fn name(&self) -> &'static str {
            "queued-final-before-finish"
        }
        fn open(&self) -> Result<Box<dyn Session>> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let _ = tx.send(TranscriptEvent::Connected);
            let _ = tx.send(TranscriptEvent::Partial("hello".into()));
            let _ = tx.send(TranscriptEvent::Final("hello".into()));
            Ok(Box::new(QueuedFinalBeforeFinishSession {
                tx,
                rx,
                finished: false,
            }))
        }
    }

    impl Session for QueuedFinalBeforeFinishSession {
        fn push(&mut self, _: &[i16]) -> Result<()> {
            Ok(())
        }
        fn events(&self) -> &crossbeam_channel::Receiver<TranscriptEvent> {
            &self.rx
        }
        fn finish(&mut self) -> Result<()> {
            if self.finished {
                return Ok(());
            }
            self.finished = true;
            // Yield so a buggy finish() that arms finishing before draining
            // would already have consumed the queued Final and returned.
            std::thread::sleep(Duration::from_millis(20));
            let _ = self.tx.send(TranscriptEvent::Final(
                "hello world more speech after keyup".into(),
            ));
            Ok(())
        }
    }

    #[test]
    fn finish_drains_queued_final_before_waiting_for_post_finish() {
        let mut dictation = Dictation::start(&QueuedFinalBeforeFinish).unwrap();
        // Deliberately do not poll/absorb — leave the short Final sitting on
        // the channel the way the app loop can when key-up wins select!.
        dictation.feed(&tone(0.2)).unwrap();

        let mut partials = Vec::new();
        let outcome = dictation
            .finish(Duration::from_secs(2), &mut |p| {
                partials.push(p.to_string())
            })
            .expect("dictation");

        assert!(
            outcome.text.contains("more speech"),
            "queued pre-finish Final must not short-circuit wait; got {:?} (partials={partials:?})",
            outcome.text
        );
    }
}

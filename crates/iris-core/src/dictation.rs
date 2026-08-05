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
//! and waits. On timeout, close, error, or a `finish` that fails outright, a
//! non-empty `latest_partial` is salvaged so words already shown on the overlay
//! are not discarded, and [`DictationOutcome::cause`] says which of those
//! produced the text. Prefer the longer of Final vs latest partial when both
//! exist.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::audio;
use crate::engine::{Engine, Session, TranscriptEvent};
use crate::latency::{Mark, Timeline};
use crate::vlog;

/// The default wait for the final transcript after `finish()`, for an engine
/// that has not chosen its own (see [`Engine::final_timeout`]).
///
/// Deliberately well under the engine-internal worst case a protocol failure
/// can take (for Deepgram, `FINALIZE_ACK_TIMEOUT` + `FINALIZE_TIMEOUT` ≈ 8s,
/// see `engine/deepgram.rs`) rather than padded above it: this bound exists to
/// cap how long a *stalled* engine can hold the user's screen empty, and every
/// final-transcript span actually observed in production on a healthy
/// connection has stayed under 2.5s. Was 10s until the 2026-08-02 regression
/// investigation found a degraded-network dictation whose engine session
/// never concluded at all, so only this outer bound ended the wait — at,
/// essentially, its full old value.
///
/// Both the value and that evidence are *streaming* evidence, so this is the
/// default rather than the rule: an engine whose work happens after key-up
/// overrides [`Engine::final_timeout`] instead of inheriting a budget measured
/// from a different architecture.
///
/// **This must never be what ends a session that is still legitimately
/// connecting.** Latency is not allowed to buy itself with the user's words,
/// and a connect cut off before the engine's own budget expires loses the
/// whole utterance: there is no partial to fall back on, because nothing has
/// streamed yet. The raw numbers cannot express that on their own — this
/// deadline runs from key-up and a connect budget from key-down (Deepgram's
/// `CONNECT_TIMEOUT` is 8s), so no hold length makes the ordering stable and
/// raising this above 8s would pay for it on every healthy dictation. The
/// relationship is enforced instead of asserted: [`Engine::connect_budget`]
/// publishes the ceiling and [`Dictation::finish`] extends its wait to cover
/// it — and, when that connect then succeeds late, re-bases the extension to
/// this value from the instant the stream came up, because a connection that
/// works is worth nothing if the wait ends at the moment it starts working.
/// A non-empty partial stops the extending, which is the point at which expiry
/// costs a tail instead of the utterance; a dictation that never had to buy an
/// extension therefore pays exactly this value, and that is where the latency
/// win lives. What a partial must never do is shorten a wait already bought —
/// see [`WaitBound`].
///
/// The cost is that once an extension has been bought this value is no longer
/// the bound on how long `finish` blocks, and a partial arriving afterwards
/// does not restore it: for Deepgram the worst case is 8s of connect from
/// key-down plus this 6s of finalise from the connect, ~14s, most of it with
/// nothing yet to show for the hold. That is deliberate — the alternative is
/// losing the utterance, or handing back an interim while the real `Final` is
/// milliseconds behind it — and it is the number to quote, not the 6s.
///
/// Anyone changing this number changes that pairing too, and it is not
/// optional. `fm/iris-silent-and-instant` is stacked on this work and moving
/// the same constant from another direction; a rebase that keeps one side and
/// drops the other reintroduces exactly the data loss the grace exists to
/// prevent. `finish`'s wait loop, [`WaitBound`],
/// [`Dictation::extend_while_nothing_to_salvage`],
/// `a_still_connecting_engine_is_not_cut_off_by_the_outer_bound`,
/// `a_late_connect_still_delivers_the_words_it_goes_on_to_produce` and
/// `a_late_connect_that_streams_first_still_waits_for_the_final` are what to
/// re-read before touching it.
pub const DEFAULT_FINAL_TIMEOUT: Duration = Duration::from_secs(6);

/// A completed dictation.
#[derive(Debug, Clone)]
pub struct DictationOutcome {
    pub text: String,
    pub timeline: Timeline,
    /// Why this text is not a clean [`TranscriptEvent::Final`], when it is not.
    ///
    /// `None` only when the engine really did conclude. Every other way words
    /// reach a caller — the engine erroring after a partial, the session
    /// closing, the wait running out, [`Session::finish`] itself failing — is
    /// a salvage, and the caller has to be able to say so: a truncated
    /// dictation logged as an ordinary one is indistinguishable from the user
    /// having said less, which is how a real engine failure hides.
    pub cause: Option<String>,
}

/// [`Dictation::finish`] failed to produce a transcript, but real capture may
/// have happened before the engine gave up — the timeline it stamped along
/// the way (audio fed, marks up to key-up) travels with the error instead of
/// being lost, so a caller logging diagnostics does not have to report a
/// failed dictation as if nothing had been captured at all.
#[derive(Debug)]
pub struct DictationError {
    /// What *this* layer knows, and only that: when there is a `source`, its
    /// text is the source's to supply, never restated here. A cause stored on
    /// both levels is printed on both levels by any caller that walks the
    /// chain.
    message: String,
    /// The engine's own error, when there was one, kept structured so a caller
    /// converting back into `anyhow::Error` still has the chain rather than
    /// only the flattened text in `message`.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    pub timeline: Timeline,
}

impl DictationError {
    /// The only way to build one: every failure arm has to hand over the
    /// timeline as it stood, so none can quietly report a dictation as if
    /// nothing had been captured.
    fn fail(
        timeline: Timeline,
        message: String,
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Box<Self> {
        Box::new(Self {
            message,
            source,
            timeline,
        })
    }
}

/// `{}` is this layer alone; `{:#}` is the whole story, the way `anyhow`
/// spells it.
///
/// Both are needed because the two callers ask differently: `iris-app` prints
/// the error itself and never walks a chain, so its `{e:#}` has to reach the
/// engine's own words; `run_offline` converts back into `anyhow::Error`, which
/// walks the chain and prints each link with `{}` — so a link that flattened
/// its own source would print every cause under it twice.
impl std::fmt::Display for DictationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if f.alternate() {
            let mut cause = std::error::Error::source(self);
            while let Some(e) = cause {
                write!(f, ": {e}")?;
                cause = e.source();
            }
        }
        Ok(())
    }
}

impl std::error::Error for DictationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// Owns one engine session for the duration of one dictation.
pub struct Dictation {
    session: Box<dyn Session>,
    timeline: Timeline,
    samples: usize,
    /// Taken from the engine at `start`, because [`Dictation::finish`] is
    /// handed a timeout rather than the engine and this must not depend on a
    /// caller remembering to pass it. See [`Engine::connect_budget`].
    connect_budget: Option<Duration>,
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

/// The single bound on [`Dictation::finish`]'s wait, and the one rule that
/// three separate regressions all turned out to be: **it only ever moves
/// forward.**
///
/// The wait used to be recomputed from scratch on every pass, which let a bound
/// that had already been extended silently drop back — sometimes to an instant
/// already in the past, ending the wait immediately. Each time the trigger
/// looked different and each time the fix was another special case:
///
/// - the outer deadline (from key-up) undercutting a connect budget (from
///   key-down), cutting off a session still legitimately connecting;
/// - `Connected` collapsing the extended bound back to the already-past plain
///   deadline, at the exact moment the connection became useful;
/// - the first partial doing the same, so a `Final` arriving milliseconds
///   behind an interim was never waited for and the less accurate text won.
///
/// They are one defect, so this is one type. An event may buy the session more
/// time; nothing can take time away. That makes the failure structurally
/// impossible rather than absent by inspection, and it is why [`WaitBound::0`]
/// is private and [`WaitBound::extend_to`] is the only mutator — a future event
/// wired in here cannot reintroduce the bug without deleting that method.
struct WaitBound(Instant);

impl WaitBound {
    fn new(limit: Instant) -> Self {
        Self(limit)
    }

    /// Push the bound out to `at`, if that is later than where it already
    /// stands. The only way to change it.
    fn extend_to(&mut self, at: Instant) {
        if at > self.0 {
            self.0 = at;
        }
    }

    /// How much of the wait is left. Zero means expired.
    fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }
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
            connect_budget: engine.connect_budget(),
            latest_partial: String::new(),
            finishing: false,
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

    /// Give up on this dictation without asking the engine to finalise, and
    /// take everything it can still show for itself.
    ///
    /// The counterpart to [`Dictation::finish`] for a hold that never reaches
    /// it at all (the microphone or the hotkey thread died, or
    /// [`Dictation::feed`] failed). It ends the utterance on the same terms:
    /// queued events are absorbed first, a non-empty `latest_partial` is
    /// salvaged as the transcript, and the timeline carries the audio and marks
    /// that really happened. An empty [`DictationOutcome::text`] means nothing
    /// was ever transcribed — that, and only that, is a dictation with no words
    /// in it.
    ///
    /// Words the user already watched appear on the overlay outlive the failure
    /// that ended the hold: a socket dying while the tail is fed must not cost
    /// more than the same socket dying one statement later, inside `finish`.
    pub fn abandon(mut self, on_partial: &mut dyn FnMut(&str)) -> DictationOutcome {
        self.poll(on_partial);
        self.timeline.mark(Mark::KeyUp);
        self.timeline.audio_secs = self.audio_secs();
        let text = self.salvage_partial().unwrap_or_default();
        self.timeline.transcript = text.clone();
        // The reason the hold ended belongs to whoever called this — it is the
        // one thing here the engine cannot know. An engine error seen on the
        // way out is the exception, and it travels rather than being dropped
        // in favour of the caller's account.
        let cause = match &self.ended {
            Some(Ending::Error(message)) => {
                Some(format!("{} engine: {message}", self.timeline.engine))
            }
            _ => None,
        };
        DictationOutcome {
            text,
            timeline: self.timeline,
            cause,
        }
    }

    /// Buy a session that has nothing to lose yet more time than the plain
    /// deadline, given the engine's own budget for producing a transcript
    /// after `finish` (`finalise`, i.e. `final_timeout`).
    ///
    /// Connecting was never the goal — getting the words was — so this covers
    /// the whole path rather than its first step. While nothing is salvageable
    /// there are two states, and the bound is extended to cover whichever
    /// applies:
    ///
    /// - *still connecting*: the engine's [`Engine::connect_budget`] measured
    ///   from key-down, which is the clock that budget was written against.
    /// - *connected, nothing streamed yet*: `finalise` from the instant the
    ///   stream came up. A connect that lands after the plain deadline has to
    ///   be given the same budget to answer as one that landed before it,
    ///   otherwise the wait ends at the exact moment it starts being useful
    ///   and the whole utterance is lost to a connection that worked.
    ///
    /// The moment a non-empty partial exists this stops buying time: expiry
    /// then costs a tail rather than the utterance, which is a degrade, not a
    /// loss. That is where the latency win lives, but only for a session whose
    /// connect was prompt enough that nothing was ever bought — this stops
    /// *buying*, and that is all it does. The wait already bought stands,
    /// even once there are words to fall back on, because the `Final` that
    /// would replace that partial with the accurate text is usually
    /// milliseconds behind it, and this method has no way to tell "there is now
    /// something to fall back on" from "there is nothing better coming". See
    /// [`WaitBound`], which is what makes the distinction impossible to get
    /// wrong from here.
    fn extend_while_nothing_to_salvage(&self, bound: &mut WaitBound, finalise: Duration) {
        let Some(budget) = self.connect_budget else {
            return;
        };
        if !self.latest_partial.trim().is_empty() {
            return;
        }
        match self.timeline.at(Mark::StreamReady) {
            Some(ready) => bound.extend_to(ready + finalise),
            None => {
                if let Some(down) = self.timeline.at(Mark::KeyDown) {
                    bound.extend_to(down + budget);
                }
            }
        }
    }

    /// The best transcript still available without a `Final`: whatever the
    /// overlay last showed. Stamps [`Mark::FinalTranscript`] when there is one,
    /// because for the user that partial *is* where the words landed.
    ///
    /// Shared by [`Dictation::finish`] and [`Dictation::abandon`] so the two
    /// exits cannot drift apart on what a failed dictation is allowed to lose.
    fn salvage_partial(&mut self) -> Option<String> {
        if self.latest_partial.trim().is_empty() {
            return None;
        }
        let text = self.latest_partial.clone();
        self.timeline.mark(Mark::FinalTranscript);
        Some(text)
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
    /// **This blocks the caller's whole loop for up to `timeout`.** In
    /// `iris-app` that loop is the resident one: while this waits, the pill
    /// holds its last frame and no tray command — Quit included — is serviced.
    /// An engine picking its [`Engine::final_timeout`] is choosing how long the
    /// application can stop responding, not just how long this dictation may
    /// take, which is why a generous bound belongs under an engine-internal
    /// timeout that ends a dead request sooner rather than on its own.
    ///
    /// The one thing that outlasts `timeout` is a session with nothing to
    /// salvage that is still inside its [`Engine::connect_budget`], or has
    /// just come up and been given `timeout` from there to answer: giving up
    /// at either point would trade the user's whole utterance for the
    /// difference between two clocks that do not start together (see
    /// [`DEFAULT_FINAL_TIMEOUT`] and
    /// [`Dictation::extend_while_nothing_to_salvage`]). A partial stops the
    /// wait being extended any further; it does not hand back time already
    /// bought, because [`WaitBound`] only ever grows. So a dictation whose
    /// connect was prompt enough never to need an extension pays nothing
    /// beyond `timeout`, while one that bought the extension first and
    /// streamed its first partial afterwards can still block for the whole
    /// extended ceiling — for Deepgram the ~14s of connect budget plus
    /// finalise, and on that path that ceiling, not `timeout`, is what the
    /// caller's loop actually blocks for. That is the deliberate price: the
    /// loop returns the instant a real `Final` lands, so the extra wait is
    /// only ever spent where the `Final` never comes, which is exactly where
    /// waiting is the correct answer — and giving it back the moment an
    /// interim arrived is what handed the user the less accurate text.
    ///
    /// A terminal event that arrived *before* this call (a premature Final from
    /// a buggy or segment-oriented engine) does not short-circuit the wait:
    /// [`Session::finish`] is always invoked, and a later, richer Final or the
    /// best partial wins.
    pub fn finish(
        mut self,
        timeout: Duration,
        on_partial: &mut dyn FnMut(&str),
    ) -> Result<DictationOutcome, Box<DictationError>> {
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
        if let Err(e) = self.session.finish() {
            self.timeline.audio_secs = self.audio_secs();
            // The same rule as every arm below: a socket that dies as the
            // Finalize goes out costs no more than the same socket dying one
            // statement later, inside the wait.
            if let Some(text) = self.salvage_partial() {
                self.timeline.transcript = text.clone();
                return Ok(DictationOutcome {
                    text,
                    timeline: self.timeline,
                    // Flattened, because a cause is a leaf: it is one string in
                    // the log with no chain behind it to supply the context.
                    cause: Some(format!("{e:#}")),
                });
            }
            // Says what this layer knows and stops there; the engine's account
            // travels as the source, so a caller walking the chain reads each
            // cause once.
            let message = format!("{} engine failed to finalise", self.timeline.engine);
            return Err(DictationError::fail(self.timeline, message, Some(e.into())));
        }
        vlog!("finalising after {:.2}s of audio", self.audio_secs());

        let waiting_since = Instant::now();
        // `timeout` is the bound until a pass with nothing salvageable extends
        // it: cutting a connect off inside the budget the engine set, or the
        // instant it succeeds, loses the whole utterance rather than a tail of
        // it. Extended in place every pass and never recomputed, so a partial
        // arriving later stops the extending without handing back what an
        // earlier pass already bought.
        let mut bound = WaitBound::new(waiting_since + timeout);
        while self.ended.is_none() {
            self.extend_while_nothing_to_salvage(&mut bound, timeout);
            let remaining = bound.remaining();
            if remaining.is_zero() {
                break;
            }
            match self.session.events().recv_timeout(remaining) {
                Ok(event) => self.absorb(event, on_partial),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    self.ended.get_or_insert(Ending::Closed);
                }
            }
        }
        let waited = waiting_since.elapsed();

        self.timeline.audio_secs = self.audio_secs();

        // Prefer a real Final; otherwise salvage the best partial rather than
        // discarding words the user already saw on the overlay.
        let salvaged = self.salvage_partial();

        // Everything but a Final is a salvage, and each one names itself the
        // same way whether it ends up as the transcript or as the error: the
        // caller decides what to do with the words, never what to call them.
        let ended = self.ended.take();
        let cause = match &ended {
            Some(Ending::Final(_)) => None,
            Some(Ending::Error(message)) => {
                Some(format!("{} engine: {message}", self.timeline.engine))
            }
            Some(Ending::Closed) => Some(format!(
                "{} engine closed without returning a transcript",
                self.timeline.engine
            )),
            // An engine still opening its session at the deadline is a
            // connection failure, and it reads as one in the log rather than as
            // a silent engine. `Connected` — and so `Mark::StreamReady` — is
            // the only signal needed for that, which keeps the diagnosis
            // engine-agnostic: an inner connect timeout that loses the race to
            // this one no longer takes the real cause down with it.
            None if self.timeline.at(Mark::StreamReady).is_some() => Some(format!(
                "{} engine did not return a transcript within {:.1}s",
                self.timeline.engine,
                waited.as_secs_f64()
            )),
            None => Some(format!(
                "{} engine did not return a transcript within {:.1}s: it never connected — the \
                 session was still opening when the wait ran out",
                self.timeline.engine,
                waited.as_secs_f64()
            )),
        };

        match (ended, salvaged) {
            (Some(Ending::Final(text)), salvaged) => {
                let text = best_transcript(text, salvaged.as_deref().unwrap_or(""));
                self.timeline.transcript = text.clone();
                Ok(DictationOutcome {
                    text,
                    timeline: self.timeline,
                    cause: None,
                })
            }
            (_, Some(text)) => {
                vlog!(
                    "no final transcript; salvaging {} chars ({})",
                    text.chars().count(),
                    cause.as_deref().unwrap_or("no cause")
                );
                self.timeline.transcript = text.clone();
                Ok(DictationOutcome {
                    text,
                    timeline: self.timeline,
                    cause,
                })
            }
            (_, None) => Err(DictationError::fail(
                self.timeline,
                cause.unwrap_or_default(),
                None,
            )),
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

    Ok(dictation.finish(engine.final_timeout(), on_partial)?)
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
        // An engine that never reported Connected was still opening its
        // session, and the log has to say so: an engine-internal connect
        // timeout longer than this wait never gets to name the cause itself.
        assert!(err.contains("never connected"), "{err}");
    }

    #[test]
    fn default_final_timeout_is_capped_well_below_the_old_ten_seconds() {
        // Regression guard for the 2026-08-02 latency investigation: this is
        // the bound that actually ended the captain's pathological 10015ms
        // dictation — not FINALIZE_ACK_TIMEOUT or FINALIZE_TIMEOUT in
        // engine/deepgram.rs, both of which stayed well inside their own
        // budget while the engine session itself never concluded. Keeping
        // this comfortably under the old 10s value is the fix for that worst
        // case; nothing in production has ever needed close to 6s on a
        // healthy connection (see the doc comment on the constant).
        assert!(
            DEFAULT_FINAL_TIMEOUT <= Duration::from_secs(6),
            "DEFAULT_FINAL_TIMEOUT crept back toward the old, unbounded-feeling \
             10s ceiling: {DEFAULT_FINAL_TIMEOUT:?}"
        );
    }

    #[test]
    fn the_wait_after_finish_is_the_engine_s_own() {
        // Every caller of `finish` asks the engine, so an engine that does its
        // work after key-up is never held to a bound measured from one that
        // does not.
        struct Patient;
        impl Engine for Patient {
            fn name(&self) -> &'static str {
                "patient"
            }
            fn final_timeout(&self) -> Duration {
                Duration::from_millis(30)
            }
            fn open(&self) -> Result<Box<dyn Session>> {
                let (tx, rx) = crossbeam_channel::unbounded();
                Ok(Box::new(SilentForever {
                    events: rx,
                    _keep: tx,
                }))
            }
        }
        struct SilentForever {
            events: crossbeam_channel::Receiver<TranscriptEvent>,
            _keep: crossbeam_channel::Sender<TranscriptEvent>,
        }
        impl Session for SilentForever {
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

        assert!(
            Duration::from_millis(30) < DEFAULT_FINAL_TIMEOUT,
            "the engine's bound has to differ from the default for this to prove anything"
        );
        let started = Instant::now();
        let err = run_offline(&Patient, &tone(0.1), Pace::Fast, &mut |_| {}).unwrap_err();
        let waited = started.elapsed();

        assert!(
            err.to_string().contains("did not return a transcript"),
            "{err}"
        );
        assert!(
            waited < DEFAULT_FINAL_TIMEOUT,
            "the default was applied over the engine's own bound: {waited:?}"
        );
    }

    #[test]
    fn a_stalled_engine_error_still_carries_the_real_timeline() {
        // Regression: the captain's session log carried several successful
        // dictations whose `perceived_ms` spiked to 6-10s (e.g. 6484ms,
        // 10128ms on 2026-08-02) with no engine-reported error at all — the
        // session simply never sent a close sign-off, so only
        // `DEFAULT_FINAL_TIMEOUT` ended the wait (see its doc comment). Had
        // that stall instead outlasted the timeout on a *failed* attempt, the
        // old code path would have logged `audio_secs: 0.0` regardless of how
        // much real audio was captured — an artifact of the caller
        // (iris-app's `capture()`) building a blank Timeline on any `finish()`
        // error instead of keeping the one `Dictation` had already stamped.
        // This drives an engine that connects, receives real audio, and then
        // never concludes — the shape of a session stalled by a network
        // failure — and checks the error still carries what actually
        // happened.
        struct StallsAfterConnect;
        struct StallsAfterConnectSession {
            events: crossbeam_channel::Receiver<TranscriptEvent>,
            _keep: crossbeam_channel::Sender<TranscriptEvent>,
        }
        impl Engine for StallsAfterConnect {
            fn name(&self) -> &'static str {
                "stalls-after-connect"
            }
            fn open(&self) -> Result<Box<dyn Session>> {
                let (tx, rx) = crossbeam_channel::unbounded();
                tx.send(TranscriptEvent::Connected).unwrap();
                Ok(Box::new(StallsAfterConnectSession {
                    events: rx,
                    _keep: tx,
                }))
            }
        }
        impl Session for StallsAfterConnectSession {
            fn push(&mut self, _: &[i16]) -> Result<()> {
                Ok(())
            }
            fn events(&self) -> &crossbeam_channel::Receiver<TranscriptEvent> {
                &self.events
            }
            fn finish(&mut self) -> Result<()> {
                // Never sends a terminal event, no matter how long finish()
                // waits — the network failure this models does not resolve
                // itself within any bound the app enforces.
                Ok(())
            }
        }

        let mut dictation = Dictation::start(&StallsAfterConnect).unwrap();
        dictation.feed(&tone(1.5)).unwrap();
        let err = dictation
            .finish(Duration::from_millis(50), &mut |_| {})
            .unwrap_err();

        assert!(err.to_string().contains("did not return a transcript"));
        assert!(
            !err.to_string().contains("never connected"),
            "this engine did connect; the timeout must not blame the connection: {err}"
        );
        assert!(
            (err.timeline.audio_secs - 1.5).abs() < 0.05,
            "the timeline on error must keep the real captured audio, not read as zero: {}",
            err.timeline.audio_secs
        );
        assert!(
            err.timeline.at(Mark::StreamReady).is_some(),
            "marks stamped before the stall must survive on the error path"
        );
    }

    #[test]
    fn a_finish_failure_keeps_the_engine_error_as_a_source() {
        // `run_offline` and the spike pipeline convert this straight back into
        // an anyhow::Error; flattening the engine's chain into one string at
        // this boundary would lose the cause for good.
        struct FinishFails;
        struct FinishFailsSession {
            events: crossbeam_channel::Receiver<TranscriptEvent>,
            _keep: crossbeam_channel::Sender<TranscriptEvent>,
        }
        impl Engine for FinishFails {
            fn name(&self) -> &'static str {
                "finish-fails"
            }
            fn open(&self) -> Result<Box<dyn Session>> {
                let (tx, rx) = crossbeam_channel::unbounded();
                Ok(Box::new(FinishFailsSession {
                    events: rx,
                    _keep: tx,
                }))
            }
        }
        impl Session for FinishFailsSession {
            fn push(&mut self, _: &[i16]) -> Result<()> {
                Ok(())
            }
            fn events(&self) -> &crossbeam_channel::Receiver<TranscriptEvent> {
                &self.events
            }
            fn finish(&mut self) -> Result<()> {
                Err(anyhow::anyhow!("the socket was already gone")
                    .context("sending Finalize to Deepgram"))
            }
        }

        let mut dictation = Dictation::start(&FinishFails).unwrap();
        dictation.feed(&tone(0.6)).unwrap();
        let err = dictation
            .finish(Duration::from_millis(50), &mut |_| {})
            .unwrap_err();

        let source = std::error::Error::source(&*err)
            .expect("the engine error must survive")
            .to_string();
        assert!(source.contains("sending Finalize"), "{source}");
        // Whoever prints this reads each cause once, whichever way they ask:
        // `{:#}` here walks the chain itself, and `iris-app` — which never
        // walks one — gets the same story out of `Display`.
        let flattened = format!("{err:#}");
        let as_anyhow = anyhow::Error::from(err);
        let chain = as_anyhow.chain().count();
        assert!(chain >= 2, "the cause chain was flattened: {chain} link(s)");
        let full = format!("{as_anyhow:#}");
        for story in [&flattened, &full] {
            assert_eq!(
                story.matches("sending Finalize").count(),
                1,
                "the cause is printed more than once: {story}"
            );
            assert!(story.contains("the socket was already gone"), "{story}");
        }
        assert_eq!(flattened, full, "the two spellings must agree");
    }

    /// As [`FinishFails`], but words reached the overlay first — the socket
    /// dying exactly as `Finalize` goes out, with a transcript already on
    /// screen.
    struct FinishFailsAfterPartial;

    struct FinishFailsAfterPartialSession {
        events: crossbeam_channel::Receiver<TranscriptEvent>,
        _keep: crossbeam_channel::Sender<TranscriptEvent>,
    }

    impl Engine for FinishFailsAfterPartial {
        fn name(&self) -> &'static str {
            "finish-fails-after-partial"
        }
        fn open(&self) -> Result<Box<dyn Session>> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let _ = tx.send(TranscriptEvent::Connected);
            let _ = tx.send(TranscriptEvent::Partial("hello there".into()));
            Ok(Box::new(FinishFailsAfterPartialSession {
                events: rx,
                _keep: tx,
            }))
        }
    }

    impl Session for FinishFailsAfterPartialSession {
        fn push(&mut self, _: &[i16]) -> Result<()> {
            Ok(())
        }
        fn events(&self) -> &crossbeam_channel::Receiver<TranscriptEvent> {
            &self.events
        }
        fn finish(&mut self) -> Result<()> {
            Err(anyhow::anyhow!("the socket was already gone"))
        }
    }

    #[test]
    fn a_finish_that_fails_still_hands_back_the_words_already_shown() {
        // The salvage rule cannot depend on which statement the socket died
        // on. One line later — inside the wait — the same death delivers
        // "hello there"; refusing to here would cost the user an utterance
        // they watched appear, for no reason they could ever see.
        let mut dictation = Dictation::start(&FinishFailsAfterPartial).unwrap();
        dictation.feed(&tone(0.6)).unwrap();
        dictation.poll(&mut |_| {});

        let outcome = dictation
            .finish(Duration::from_millis(50), &mut |_| {})
            .expect("the partial is still the user's text");

        assert_eq!(outcome.text, "hello there");
        assert_eq!(outcome.timeline.transcript, "hello there");
        assert!(
            outcome.timeline.at(Mark::FinalTranscript).is_some(),
            "the salvaged partial is where the words landed for the user"
        );
        assert!(
            outcome
                .cause
                .as_deref()
                .unwrap_or_default()
                .contains("gone"),
            "and it must not read as a clean dictation: {:?}",
            outcome.cause
        );
    }

    #[test]
    fn every_salvage_inside_finish_says_why_it_is_one() {
        // `cause` is the only thing separating a truncated dictation from a
        // short one in the log. An engine that errors after streaming words
        // returns them — and returns the error with them.
        let engine = MockEngine::new(MockConfig::default());
        let mut dictation = Dictation::start(&engine).unwrap();
        dictation.feed(&tone(1.5)).unwrap();
        dictation.poll(&mut |_| {});
        dictation.absorb_event(
            TranscriptEvent::Error("the socket died".into()),
            &mut |_| {},
        );

        let outcome = dictation
            .finish(Duration::from_millis(50), &mut |_| {})
            .expect("words were already transcribed");
        assert!(!outcome.text.is_empty());
        assert!(
            outcome
                .cause
                .as_deref()
                .unwrap_or_default()
                .contains("the socket died"),
            "{:?}",
            outcome.cause
        );
    }

    #[test]
    fn a_clean_final_carries_no_cause() {
        let engine = MockEngine::new(MockConfig::default());
        let outcome = run_offline(&engine, &tone(1.0), Pace::Fast, &mut |_| {}).unwrap();
        assert_eq!(outcome.cause, None);
    }

    /// Never connects and never says anything, but claims a connect budget far
    /// longer than any `final_timeout` a test would pass — the shape of a
    /// degraded-network connect that is still legitimately in progress when a
    /// short hold's outer deadline expires.
    struct SlowToConnect {
        budget: Option<Duration>,
        /// When true the session opens already up, with words on the channel.
        up: bool,
    }

    struct SlowToConnectSession {
        events: crossbeam_channel::Receiver<TranscriptEvent>,
        _keep: crossbeam_channel::Sender<TranscriptEvent>,
    }

    impl Engine for SlowToConnect {
        fn name(&self) -> &'static str {
            "slow-to-connect"
        }
        fn connect_budget(&self) -> Option<Duration> {
            self.budget
        }
        fn open(&self) -> Result<Box<dyn Session>> {
            let (tx, rx) = crossbeam_channel::unbounded();
            if self.up {
                let _ = tx.send(TranscriptEvent::Connected);
                let _ = tx.send(TranscriptEvent::Partial("hello there".into()));
            }
            Ok(Box::new(SlowToConnectSession {
                events: rx,
                _keep: tx,
            }))
        }
    }

    impl Session for SlowToConnectSession {
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

    #[test]
    fn a_still_connecting_engine_is_not_cut_off_by_the_outer_bound() {
        // The invariant behind `Engine::connect_budget`: the outer deadline
        // runs from key-up and a connect budget from key-down, so a short hold
        // can put the deadline first. Ending the session there kills a connect
        // the engine was still entitled to finish, and with nothing streamed
        // there is nothing to fall back on — the whole utterance goes. Latency
        // is never allowed to buy itself that way.
        let budget = Duration::from_millis(400);
        let engine = SlowToConnect {
            budget: Some(budget),
            up: false,
        };
        let dictation = Dictation::start(&engine).unwrap();
        let started = Instant::now();
        let err = dictation
            .finish(Duration::from_millis(20), &mut |_| {})
            .unwrap_err();
        let waited = started.elapsed();

        assert!(
            waited >= budget - Duration::from_millis(80),
            "the outer bound cut the connect off inside its own budget after {waited:?}"
        );
        assert!(
            waited < budget * 3,
            "and it must still end when that budget does: {waited:?}"
        );
        assert!(err.to_string().contains("never connected"), "{err}");
    }

    /// Connects only after the plain deadline has already gone by — inside the
    /// connect budget, but late — and then does exactly what a real socket
    /// does once it is up: flushes the audio it was holding and answers. The
    /// shape a short hold against a degraded network produces, and the one the
    /// whole grace exists for.
    struct ConnectsLate {
        budget: Duration,
        after: Duration,
        /// What the flush produces. A short hold really can have nothing but
        /// the post-`Finalize` `Final` (see `engine/deepgram.rs`), and an
        /// interim landing just ahead of that `Final` is the other real shape.
        streams_first: bool,
    }

    struct ConnectsLateSession {
        events: crossbeam_channel::Receiver<TranscriptEvent>,
        _worker: std::thread::JoinHandle<()>,
    }

    impl Engine for ConnectsLate {
        fn name(&self) -> &'static str {
            "connects-late"
        }
        fn connect_budget(&self) -> Option<Duration> {
            Some(self.budget)
        }
        fn open(&self) -> Result<Box<dyn Session>> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let after = self.after;
            let streams_first = self.streams_first;
            let worker = std::thread::spawn(move || {
                std::thread::sleep(after);
                let _ = tx.send(TranscriptEvent::Connected);
                std::thread::sleep(Duration::from_millis(50));
                if streams_first {
                    // The interim is a rough draft: the punctuated, capitalised
                    // text is milliseconds behind it, the way a real flush
                    // sends them.
                    let _ = tx.send(TranscriptEvent::Partial("no".into()));
                    std::thread::sleep(Duration::from_millis(30));
                    let _ = tx.send(TranscriptEvent::Final("No. No.".into()));
                } else {
                    let _ = tx.send(TranscriptEvent::Final("hello there".into()));
                }
            });
            Ok(Box::new(ConnectsLateSession {
                events: rx,
                _worker: worker,
            }))
        }
    }

    impl Session for ConnectsLateSession {
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

    #[test]
    fn a_late_connect_still_delivers_the_words_it_goes_on_to_produce() {
        // Connecting was never the goal; the words were. A grace that expires
        // at the instant the socket comes up spends the whole connect budget
        // and then throws away everything it bought — the utterance is lost to
        // a connection that *worked*. So the wait re-bases: a stream that
        // reports `Connected` after the deadline has passed gets the engine's
        // own finalise budget from there to produce something.
        //
        // A short hold (no audio fed, `finish` immediately) against a connect
        // that lands at 300ms — well past the 100ms deadline, well inside the
        // 3s budget — and a transcript 50ms later.
        let engine = ConnectsLate {
            budget: Duration::from_secs(3),
            after: Duration::from_millis(300),
            streams_first: false,
        };
        let dictation = Dictation::start(&engine).unwrap();
        let started = Instant::now();
        let outcome = dictation
            .finish(Duration::from_millis(100), &mut |_| {})
            .expect("a connect that succeeded must not lose the utterance");
        let waited = started.elapsed();

        assert_eq!(outcome.text, "hello there");
        assert!(
            waited < Duration::from_secs(2),
            "the re-based grace must still be bounded: {waited:?}"
        );
        assert!(
            outcome.timeline.at(Mark::StreamReady).is_some(),
            "the connect that saved it belongs on the timeline"
        );
    }

    #[test]
    fn a_late_connect_that_streams_first_still_waits_for_the_final() {
        // The other half of the rule, on the same late connect. A partial
        // arriving means the wait is no longer *extended* — from there expiry
        // costs a tail instead of the utterance. It does not mean the wait is
        // over: an interim is a rough draft, the accurate text is milliseconds
        // behind it, and ending on the partial hands the user "no" where the
        // engine said "No. No.". The loop returns the instant the `Final`
        // lands, so waiting for it costs nothing anyone can perceive.
        let engine = ConnectsLate {
            budget: Duration::from_secs(3),
            after: Duration::from_millis(300),
            streams_first: true,
        };
        let dictation = Dictation::start(&engine).unwrap();
        let started = Instant::now();
        let outcome = dictation
            .finish(Duration::from_millis(100), &mut |_| {})
            .expect("what the stream produced is still the user's");
        let waited = started.elapsed();

        assert_eq!(
            outcome.text, "No. No.",
            "the partial truncated the wait that remained and the interim won"
        );
        assert_eq!(
            outcome.cause, None,
            "the engine concluded; this is not a salvage"
        );
        assert!(
            waited < Duration::from_secs(2),
            "and the wait is still bounded: {waited:?}"
        );
    }

    #[test]
    fn the_connect_grace_stops_extending_once_there_is_something_to_lose() {
        // The grace exists only because a connect-only failure has nothing to
        // degrade to. Once words exist, expiry costs a tail rather than the
        // utterance, so nothing buys the wait more time and the fast bound is
        // the whole bound — otherwise the latency fix would be diluted on
        // every dictation. Here the partial is on the channel before the wait
        // even starts, so the 30s budget is never bought in the first place;
        // a partial arriving later cannot give back time already bought.
        let engine = SlowToConnect {
            budget: Some(Duration::from_secs(30)),
            up: true,
        };
        let mut dictation = Dictation::start(&engine).unwrap();
        dictation.feed(&tone(0.2)).unwrap();

        let started = Instant::now();
        let outcome = dictation
            .finish(Duration::from_millis(50), &mut |_| {})
            .expect("the partial is salvaged");
        let waited = started.elapsed();

        assert_eq!(outcome.text, "hello there");
        assert!(
            waited < Duration::from_secs(2),
            "a session with words to salvage waited out the connect budget: {waited:?}"
        );
    }

    #[test]
    fn the_wait_bound_only_ever_moves_forward() {
        // The rule three regressions were all violations of, pinned on its own
        // rather than only through the paths that happen to exercise it: an
        // event may buy the session more time, and nothing may take time away.
        let start = Instant::now();
        let mut bound = WaitBound::new(start + Duration::from_secs(10));

        bound.extend_to(start + Duration::from_secs(30));
        bound.extend_to(start + Duration::from_secs(20));
        bound.extend_to(start - Duration::from_secs(5));

        let remaining = bound.remaining();
        assert!(
            remaining > Duration::from_secs(28),
            "a later event clawed back a wait an earlier one had bought: {remaining:?}"
        );
    }

    #[test]
    fn an_engine_with_no_connect_budget_keeps_the_plain_bound() {
        // The default is `None`, and nothing may quietly extend a wait for an
        // engine that never asked for one.
        let engine = MockEngine::new(MockConfig::default());
        assert_eq!(engine.connect_budget(), None);

        let silent = SlowToConnect {
            budget: None,
            up: false,
        };
        let dictation = Dictation::start(&silent).unwrap();
        let started = Instant::now();
        let _ = dictation.finish(Duration::from_millis(50), &mut |_| {});
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn abandoning_a_hold_keeps_the_audio_it_captured() {
        // The caller-side counterpart of the error timeline: a hold that never
        // reaches finish() at all (dead microphone, dead hotkey thread) still
        // has to report what it captured.
        let engine = MockEngine::new(MockConfig::default());
        let mut dictation = Dictation::start(&engine).unwrap();
        dictation.feed(&tone(1.2)).unwrap();

        let outcome = dictation.abandon(&mut |_| {});
        let timeline = outcome.timeline;
        assert!(
            (timeline.audio_secs - 1.2).abs() < 0.05,
            "{}",
            timeline.audio_secs
        );
        assert!(timeline.at(Mark::CaptureStart).is_some());
        assert!(
            timeline.at(Mark::KeyUp).is_some(),
            "the hold is over either way; the log needs an end for it"
        );
    }

    #[test]
    fn abandoning_a_hold_salvages_the_words_already_transcribed() {
        // The module's standing promise — "on timeout, close, or error, a
        // non-empty latest_partial is salvaged" — has to hold for the exit
        // that skips finish() too. A socket dying while the tail is fed must
        // not cost more than the same socket dying one statement later.
        let engine = MockEngine::new(MockConfig::default());
        let mut dictation = Dictation::start(&engine).unwrap();
        let mut seen = Vec::new();
        dictation.feed(&tone(1.5)).unwrap();
        dictation.poll(&mut |p| seen.push(p.to_string()));
        assert!(!seen.is_empty(), "the mock should have streamed partials");

        let outcome = dictation.abandon(&mut |_| {});
        assert_eq!(outcome.text, *seen.last().unwrap());
        assert_eq!(outcome.timeline.transcript, outcome.text);
        assert!(
            outcome.timeline.at(Mark::FinalTranscript).is_some(),
            "the salvaged partial is where the words landed for the user"
        );
    }

    #[test]
    fn abandoning_absorbs_events_still_queued_before_giving_up() {
        // Whatever arrived while the caller was failing is as real as anything
        // it had already polled; dropping it under-reports the transcript and
        // the partial count both.
        let engine = MockEngine::new(MockConfig::default());
        let mut dictation = Dictation::start(&engine).unwrap();
        dictation.feed(&tone(1.5)).unwrap();

        let mut late = Vec::new();
        let outcome = dictation.abandon(&mut |p| late.push(p.to_string()));
        assert!(
            !late.is_empty(),
            "queued partials must still reach the caller"
        );
        assert!(
            !outcome.text.is_empty(),
            "and be salvaged as the transcript"
        );
        assert!(outcome.timeline.partials > 0);
    }

    #[test]
    fn abandoning_a_hold_that_transcribed_nothing_has_no_words_to_show() {
        let engine = MockEngine::new(MockConfig::default());
        let dictation = Dictation::start(&engine).unwrap();

        let outcome = dictation.abandon(&mut |_| {});
        assert!(outcome.text.is_empty());
        assert!(outcome.timeline.at(Mark::FinalTranscript).is_none());
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

//! The dictation loop: the state machine that is the product.
//!
//! ```text
//!         ┌──────── Command (tray) ────────┐
//!         ▼                                │
//!   ┌──────────┐  hotkey down   ┌───────────────┐  hotkey up   ┌────────────┐
//!   │   idle   │───────────────►│   capturing   │─────────────►│ finalising │
//!   └──────────┘                └───────────────┘              └────────────┘
//!         ▲                        frames → engine              polish, inject,
//!         │                                                     log session
//!         │                          ┌── inserted ── self-dismiss (~550 ms)
//!         └──────────────────────────┤
//!                                    └── cancel/empty/error ── hide pill
//! ```
//!
//! Two properties are worth defending when changing this file.
//!
//! **Idle frames are discarded.** With a warm microphone the audio channel
//! fills up between dictations; if the loop did not drain it while idle, a
//! session would open with a backlog of stale audio and transcribe the last
//! thing the user said to someone else in the room.
//!
//! **A dictation never dies silently.** Every path through
//! [`App::dictate`] ends in a [`DictationRecord`] appended to the session log —
//! including the paths where the engine failed and the one where injection
//! failed. That log is the user's only way to recover words that did not make
//! it onto the screen.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{select, Receiver};
use iris_core::dictation::{Dictation, DictationOutcome};
use iris_core::engine::Engine;
use iris_core::hotkey::HotkeyEvent;
use iris_core::latency::{ms, Mark, Timeline};
use iris_core::text;
use iris_polish::{PolishRequest, Polisher};

use crate::audio::{self, AudioSource};
use crate::config::{Config, EngineChoice, Theme};
use crate::history::{DictationRecord, LatencyBreakdown, PolishInfo, SessionLog};
use crate::inject::Injector;
use crate::pill::PillSink;
use crate::{engines, polish};

/// Something the tray (or any other UI) asks the loop to do.
///
/// Commands travel on a channel rather than mutating shared state, so the loop
/// applies them between dictations and never mid-utterance — changing the
/// engine while the user is speaking would drop the sentence they are in the
/// middle of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Switch transcription backend and persist the choice.
    SetEngine(EngineChoice),
    /// Switch microphone. `None` means the system default.
    SetDevice(Option<String>),
    /// Turn transcript cleanup on or off.
    SetPolish(bool),
    /// Switch theme (Dark → Prism, Light → Porcelain on the overlay).
    SetTheme(Theme),
    /// Open `config.toml` in the user's editor.
    OpenSettings,
    /// Re-read the config file, applying anything edited by hand.
    Reload,
    /// Leave the loop and exit.
    Quit,
}

/// What one dictation produced.
#[derive(Debug, Clone)]
pub struct Dictated {
    /// The record appended to the session log.
    pub record: DictationRecord,
    /// The full timeline, for `--report`.
    pub timeline: Timeline,
}

/// The resident application state.
///
/// Generic over the audio source so the whole loop can be driven offline from a
/// channel; the injector and the pill sink are trait objects because they are
/// chosen at startup and never swapped.
pub struct App<A: AudioSource> {
    config: Config,
    /// What belongs on disk. Distinct from `config` because CLI flags like
    /// `--engine` are documented as run-only: they change the config in force,
    /// and this shadow — updated by tray commands, written by `persist` — is
    /// how a tray theme toggle never smuggles one into the file.
    saved: Config,
    config_path: std::path::PathBuf,
    engine: Arc<dyn Engine>,
    polisher: Option<Arc<dyn Polisher>>,
    injector: Arc<dyn Injector>,
    pill: Box<dyn PillSink>,
    history: SessionLog,
    audio: A,
    count: usize,
    report: bool,
    /// Set only by [`App::with_final_timeout`]. Left `None`, every dictation
    /// asks the engine in force for its own bound, so switching engines at
    /// runtime switches the wait with it instead of keeping the one the app
    /// started with.
    final_timeout: Option<Duration>,
}

impl<A: AudioSource> App<A> {
    /// Build the app described by `config`, constructing the engine, the
    /// polisher and the session log.
    ///
    /// Fails only if the engine cannot be built — a missing key is a real,
    /// actionable error and starting anyway would mean every dictation failing
    /// one at a time. Polish never fails; see [`crate::polish::build`].
    pub fn new(
        config: Config,
        config_path: impl Into<std::path::PathBuf>,
        audio: A,
        injector: Arc<dyn Injector>,
        pill: Box<dyn PillSink>,
    ) -> Result<Self> {
        let config_path = config_path.into();
        let engine = engines::build(&config)?;
        let polisher = polish::build(&config);
        let history = open_history(&config, &config_path);
        let mut pill = pill;
        // Push initial engine label and theme so the pill matches config
        // before the first hotkey press.
        pill.set_engine(engine.name());
        pill.set_theme(config.theme);
        pill.set_show_live_text(config.show_live_text);
        Ok(Self {
            saved: config.clone(),
            config,
            config_path,
            engine,
            polisher,
            injector,
            pill,
            history,
            audio,
            count: 0,
            report: false,
            final_timeout: None,
        })
    }

    /// Print a latency breakdown after every dictation.
    #[must_use]
    pub fn with_report(mut self, report: bool) -> Self {
        self.report = report;
        self
    }

    /// Override how long to wait for the final transcript after the key comes
    /// up, in place of the bound the engine asks for.
    #[must_use]
    pub fn with_final_timeout(mut self, timeout: Duration) -> Self {
        self.final_timeout = Some(timeout);
        self
    }

    /// The wait in force for the engine currently selected.
    fn final_timeout(&self) -> Duration {
        self.final_timeout
            .unwrap_or_else(|| self.engine.final_timeout())
    }

    /// The configuration as it stands on disk, when CLI flags overrode parts
    /// of it for this run.
    ///
    /// `--engine`, `--device`, `--inject` and `--no-polish` are documented as
    /// not changing the saved setting, so a tray command persists this file
    /// config — with the tray's change applied — rather than the config in
    /// force. Without it the two are the same, which is what a test driving
    /// the app directly wants.
    #[must_use]
    pub fn with_file_config(mut self, saved: Config) -> Self {
        self.saved = saved;
        self
    }

    /// Replace the engine, bypassing [`Config`]. For tests that need an engine
    /// that fails on purpose.
    pub fn set_engine(&mut self, engine: Arc<dyn Engine>) {
        self.pill.set_engine(engine.name());
        self.engine = engine;
    }

    /// The configuration in force.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// A clone of the frame channel, for callers driving [`App::dictate`]
    /// themselves.
    pub fn frames(&self) -> Receiver<Vec<i16>> {
        self.audio.frames().clone()
    }

    /// How the audio source describes itself, for the startup banner.
    pub fn describe_audio(&self) -> String {
        self.audio.describe()
    }

    /// How many dictations have been attempted.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Run until [`Command::Quit`], a closed hotkey channel, or a closed
    /// command channel.
    ///
    /// The audio receiver is cloned up front: an [`AudioSource`] keeps the same
    /// channel across a device change, so one clone stays valid for the life of
    /// the process and the loop never has to borrow `self` while dictating.
    pub fn run(&mut self, keys: &Receiver<HotkeyEvent>, control: &Receiver<Command>) -> Result<()> {
        let frames = self.audio.frames().clone();
        loop {
            let pressed_at = select! {
                recv(keys) -> event => match event {
                    Ok(HotkeyEvent::Down(at)) => at,
                    Ok(HotkeyEvent::Up(_)) => continue,
                    // The hook is gone; without it there is no way to dictate.
                    Err(_) => anyhow::bail!("the hotkey thread stopped"),
                },
                recv(control) -> command => match command {
                    Ok(command) => {
                        if self.apply(command)?.is_break() {
                            return Ok(());
                        }
                        continue;
                    }
                    Err(_) => return Ok(()),
                },
                // Idle audio from a warm microphone. Dropped on purpose.
                recv(frames) -> _ => continue,
            };

            if let Err(e) = self.dictate(pressed_at, &frames, keys) {
                eprintln!("  dictation failed: {e:#}");
            }
        }
    }

    /// Apply a tray command, persisting anything the user would expect to
    /// survive a restart.
    fn apply(&mut self, command: Command) -> Result<std::ops::ControlFlow<()>> {
        match command {
            Command::Quit => return Ok(std::ops::ControlFlow::Break(())),
            Command::SetEngine(choice) => {
                if choice == self.config.engine {
                    return Ok(std::ops::ControlFlow::Continue(()));
                }
                let previous = self.config.engine;
                self.config.engine = choice;
                match engines::build(&self.config) {
                    Ok(engine) => {
                        self.pill.set_engine(engine.name());
                        self.engine = engine;
                        self.saved.engine = choice;
                        println!("  engine: {choice}");
                        self.persist();
                    }
                    Err(e) => {
                        // Keep dictating with what works rather than leaving the
                        // app in a state where every hotkey press fails.
                        self.config.engine = previous;
                        eprintln!("  cannot switch to {choice}: {e:#}");
                    }
                }
            }
            Command::SetDevice(device) => match self.audio.set_device(device.clone()) {
                Ok(()) => {
                    self.config.audio.device = device.clone();
                    self.saved.audio.device = device;
                    println!("  microphone: {}", self.audio.describe());
                    self.persist();
                }
                Err(e) => eprintln!("  cannot switch microphone: {e:#}"),
            },
            Command::SetPolish(enabled) => {
                self.config.polish.enabled = enabled;
                self.saved.polish.enabled = enabled;
                self.polisher = polish::build(&self.config);
                println!("  polish: {}", if enabled { "on" } else { "off" });
                self.persist();
            }
            Command::SetTheme(theme) => {
                self.config.theme = theme;
                self.saved.theme = theme;
                self.pill.set_theme(theme);
                self.persist();
            }
            Command::OpenSettings => {
                if let Err(e) = open_in_editor(&self.config_path) {
                    eprintln!("  cannot open {}: {e:#}", self.config_path.display());
                }
            }
            Command::Reload => match Config::load(&self.config_path) {
                Ok(loaded) => {
                    // The hotkey hook was installed in `main` and the audio
                    // source / injector were configured there too; none of
                    // those are rebuilt here, so a reload that changed them
                    // must say so instead of claiming they took effect.
                    // Keys are the same story: `promote_keys` ran once before
                    // any thread existed and cannot safely run again.
                    // `inject.trailing_space` is read live at inject time, so
                    // it does take effect without a restart.
                    let mut needs_restart = Vec::new();
                    if loaded.hotkey != self.config.hotkey {
                        needs_restart.push("hotkey");
                    }
                    if loaded.suppress_hotkey != self.config.suppress_hotkey {
                        needs_restart.push("suppress_hotkey");
                    }
                    if loaded.audio.device != self.config.audio.device {
                        needs_restart.push("audio.device");
                    }
                    if loaded.audio.warm != self.config.audio.warm {
                        needs_restart.push("audio.warm");
                    }
                    if loaded.keys != self.config.keys {
                        needs_restart.push("keys");
                    }
                    if loaded.inject.method != self.config.inject.method {
                        needs_restart.push("inject.method");
                    }

                    // `saved` tracks the file. `config` is what this process
                    // is actually running: keep the in-force values for
                    // anything that needs a restart so a second reload of the
                    // same file still warns, and so a tray persist cannot
                    // smuggle unapplied settings into later comparisons.
                    let mut saved = loaded.clone();
                    let mut config = loaded;
                    config.hotkey = self.config.hotkey;
                    config.suppress_hotkey = self.config.suppress_hotkey;
                    config.audio = self.config.audio.clone();
                    config.keys = self.config.keys.clone();
                    config.inject.method = self.config.inject.method;

                    // Same rollback as SetEngine: a choice that cannot be
                    // built must not land in `saved`, or the next theme
                    // toggle would write a cold-start failure into the file.
                    match engines::build(&config) {
                        Ok(engine) => {
                            self.pill.set_engine(engine.name());
                            self.engine = engine;
                        }
                        Err(e) => {
                            config.engine = self.config.engine;
                            saved.engine = self.saved.engine;
                            eprintln!("  keeping the previous engine: {e:#}");
                        }
                    }
                    self.polisher = polish::build(&config);
                    self.history = open_history(&config, &self.config_path);
                    self.pill.set_theme(config.theme);
                    // Same treatment as the theme, and for a stronger reason:
                    // this is the live transcript's opt-out, so "reloaded"
                    // has to mean it is already in force.
                    self.pill.set_show_live_text(config.show_live_text);
                    self.config = config;
                    self.saved = saved;
                    println!("  reloaded {}", self.config_path.display());
                    if !needs_restart.is_empty() {
                        println!(
                            "  {} changed: restart Iris for that to take effect",
                            needs_restart.join(", ")
                        );
                    }
                }
                Err(e) => eprintln!("  cannot reload the configuration: {e:#}"),
            },
        }
        Ok(std::ops::ControlFlow::Continue(()))
    }

    fn persist(&self) {
        if let Err(e) = self.saved.save(&self.config_path) {
            eprintln!("  cannot save {}: {e:#}", self.config_path.display());
        }
    }

    /// One dictation, from key-press to text on screen.
    ///
    /// Public so a harness can drive a single utterance; [`App::run`] is the
    /// normal entry point.
    pub fn dictate(
        &mut self,
        pressed_at: Instant,
        frames: &Receiver<Vec<i16>>,
        keys: &Receiver<HotkeyEvent>,
    ) -> Result<Dictated> {
        self.count += 1;
        self.pill.show_listening();

        let outcome = self.capture(pressed_at, frames, keys);

        // After a successful insert the overlay holds the confirmation (~550 ms)
        // then exits itself. Calling hide() immediately would cancel that.
        // Hide only on cancel / empty / error paths (no successful insert).
        let inserted_ok = matches!(&outcome, Ok(d) if d.record.injected);
        if !inserted_ok {
            self.pill.hide();
        }
        self.audio.disarm();

        let dictated = match outcome {
            Ok(dictated) => dictated,
            Err(e) => {
                // No transcript to save, but the failure still belongs in the
                // log: "nothing happened when I pressed the key" is the hardest
                // bug to report without one.
                //
                // Only opening the session or arming capture can land here —
                // both happen before any audio exists, so this timeline is
                // legitimately blank. Every failure *after* that point returns
                // its own real timeline through `capture` (see `App::failed`);
                // do not widen this fallback to cover them.
                let mut record = DictationRecord::now(self.engine.name(), "");
                record.error = Some(format!("{e:#}"));
                Dictated {
                    record,
                    timeline: Timeline::start_at(self.engine.name(), pressed_at),
                }
            }
        };

        if let Err(e) = self.history.append(&dictated.record) {
            eprintln!("  cannot write the session log: {e:#}");
        }
        if self.report {
            println!("{}", dictated.timeline.report(self.count));
        }

        // Delivery decides this, not `record.error`: a salvage can carry the
        // cause of an abnormal hold *and* have put the user's words on screen,
        // and the second of those is what the caller acted on. Only a dictation
        // that delivered nothing is a failure — the cause of one that delivered
        // belongs in the session log, which already has it, not in a console
        // line contradicting the confirmation the user just watched.
        match &dictated.record.error {
            Some(message) if !dictated.record.injected => anyhow::bail!("{message}"),
            _ => Ok(dictated),
        }
    }

    /// Capture → transcript → polish → injection. Errors here mean there is no
    /// transcript; a failure to *inject* one is recorded in the returned
    /// [`Dictated`] instead, because the words still exist and the user needs
    /// them.
    fn capture(
        &mut self,
        pressed_at: Instant,
        frames: &Receiver<Vec<i16>>,
        keys: &Receiver<HotkeyEvent>,
    ) -> Result<Dictated> {
        // The engine session first: for a streaming engine this starts the
        // websocket handshake, which then overlaps with everything below.
        //
        // A session prewarmed ahead of the key press was tried and measured
        // out: a live idle probe found Deepgram closes an unused connection
        // within roughly 12-15 s (see AGENTS.md), far short of the gaps
        // between real dictations, so a prewarmed session would almost
        // always be dead by the time it was needed. `deepgram.rs`'s
        // `from_finalize` wait addresses the same latency at its actual
        // source — a short hold's chance of outrunning Deepgram's first
        // response — without an idle connection to keep alive.
        let mut dictation = Dictation::start_at(&*self.engine, pressed_at)?;

        // A warm microphone keeps producing frames while the previous dictation
        // was polishing and injecting, and the idle loop only discards them one
        // select at a time. Anything queued now was captured before this key
        // press: transcribing it would put words the user said to someone else
        // in front of the ones they just dictated. Drained before `arm`, so a
        // producer that waits for the arm signal ([`ChannelAudio::armed`]) can
        // never have its frames mistaken for stale ones.
        let stale = frames.len();
        for _ in 0..stale {
            let _ = frames.try_recv();
        }
        if stale > 0 {
            iris_core::vlog!("discarded {stale} frames captured before the key press");
        }

        self.audio.arm().context("starting capture")?;

        let events = dictation.events();
        // Nothing that fails mid-hold may end the hold: the key is still down,
        // the user is still speaking, and finalising on a partial segment would
        // type truncated text into whatever they are looking at. Whichever
        // source dies — the engine's events, the microphone, the engine's
        // appetite for frames — we note it and keep waiting for the real
        // key-up, which is the only thing allowed to end an utterance.
        //
        // crossbeam `select!` has no `if` guard on `recv`, so a dead source is
        // swapped for a never-ready receiver and the loop carries on.
        let never_events = crossbeam_channel::never();
        let never_frames = crossbeam_channel::never();
        let mut engine_events_open = true;
        let mut frames_open = true;
        let mut mid_hold_failure: Option<anyhow::Error> = None;

        let held = loop {
            let event_rx = if engine_events_open {
                &events
            } else {
                &never_events
            };
            let frame_rx = if frames_open { frames } else { &never_frames };
            select! {
                recv(frame_rx) -> frame => {
                    // The microphone stopping and the engine refusing the frame
                    // are the same event to this hold: there is nothing left to
                    // feed, and the words already produced are still the user's.
                    // One handler, so the two cannot drift.
                    let fed = frame
                        .context("the audio thread stopped")
                        .and_then(|frame| {
                            self.pill.update_level(audio::level(&frame));
                            dictation.feed(&frame)
                        });
                    if let Err(e) = fed {
                        eprintln!("  capture failed mid-hold: {e:#}");
                        mid_hold_failure.get_or_insert(e);
                        frames_open = false;
                    }
                }
                recv(event_rx) -> event => match event {
                    Ok(event) => {
                        let pill = &mut self.pill;
                        dictation.absorb_event(event, &mut |text: &str| {
                            iris_core::vlog!("~ {text}");
                            pill.set_partial_text(text);
                        });
                    }
                    Err(_) => {
                        iris_core::vlog!(
                            "engine event channel closed mid-hold; waiting for key-up"
                        );
                        // Recorded like any other mid-hold failure: everything
                        // said from here on can only reach the transcript if
                        // the engine happens to send it after `finish`, so a
                        // dictation that ends this way is not an ordinary one
                        // however complete its words look.
                        mid_hold_failure.get_or_insert_with(|| {
                            anyhow::anyhow!("the engine stopped sending events mid-hold")
                        });
                        engine_events_open = false;
                    }
                },
                recv(keys) -> event => {
                    match event.context("the hotkey thread stopped") {
                        Ok(HotkeyEvent::Up(at)) => break Ok(at),
                        // A repeat press we never saw the release of. Ignore it
                        // rather than ending an utterance the user is still in.
                        Ok(HotkeyEvent::Down(_)) => {}
                        Err(e) => break Err(e),
                    }
                }
            }
        };

        let released_at = match held {
            Ok(at) => at,
            Err(e) => {
                // The hotkey channel is the only source a key-up can ever come
                // from, so this hold can never be confirmed as over. Salvaging
                // into an injection here would type into a window whose user
                // may still be mid-sentence; the words are reported, not typed.
                let outcome = dictation.abandon(&mut |_| {});
                let dictated = self.reported(outcome, format!("{e:#}"));
                return Ok(note_mid_hold(dictated, mid_hold_failure));
            }
        };

        // Stamped before the tail is fed so a failure down there still logs a
        // hold that reached key-up; `mark_at` records `released_at` itself, so
        // the number is the same wherever this sits.
        dictation.timeline_mut().mark_at(Mark::KeyUp, released_at);

        // Whatever the device buffered but had not delivered is still the
        // user's speech; dropping it would truncate the last word. Unless the
        // hold already gave up on that source: a backlog the loop deliberately
        // stopped reading is not a tail, and the thing that refused it will
        // only refuse it again.
        if frames_open {
            let mut tail = Vec::new();
            while let Ok(frame) = frames.try_recv() {
                tail.extend_from_slice(&frame);
            }
            if !tail.is_empty() {
                if let Err(e) = dictation.feed(&tail) {
                    return Ok(self.abandoned(dictation, e));
                }
            }
        }

        self.pill.processing();

        let finished = {
            let timeout = self.final_timeout();
            let pill = &mut self.pill;
            dictation.finish(timeout, &mut |text: &str| {
                iris_core::vlog!("~ {text}");
                pill.set_partial_text(text);
            })
        };
        let dictated = match finished {
            Ok(outcome) => self.deliver(outcome),
            Err(e) => {
                // The engine never produced a transcript, but real audio may
                // have been captured and marks stamped before it gave up —
                // `e.timeline` carries that, so the record below reports what
                // actually happened instead of reading as if the hold never
                // captured anything.
                let message = format!("{e:#}");
                self.failed(e.timeline, message)
            }
        };
        // Always, even when the hold went on to produce a transcript: what came
        // back is then the part of the utterance that survived, and a record
        // that does not say so reads as an ordinary dictation that happened to
        // be short. The words the microphone never captured are invisible here
        // by definition; the cause is the only trace they leave.
        Ok(note_mid_hold(dictated, mid_hold_failure))
    }

    /// Polish the transcript, put it on screen, and record what happened.
    ///
    /// Shared by every path that ends with words in hand — a clean `finish`, a
    /// partial salvaged from an engine that died, a partial salvaged from a
    /// hold [`App::abandoned`] before `finish` could run. What the user gets
    /// does not depend on which of those produced the text.
    ///
    /// [`DictationOutcome::cause`] is what keeps the *log* honest about the
    /// difference: a salvage produced inside `finish` never reaches the
    /// mid-hold bookkeeping in [`App::capture`], so without it an engine that
    /// errored after two partials would record as a clean, unusually short
    /// dictation.
    fn deliver(&mut self, outcome: DictationOutcome) -> Dictated {
        let DictationOutcome {
            text: raw,
            mut timeline,
            cause,
        } = outcome;
        let raw = raw.trim().to_string();

        let mut record = DictationRecord::now(self.engine.name(), &raw);
        let dictated = if raw.is_empty() {
            // Silence, or a key tapped by accident. Injecting nothing is right;
            // so is not pretending to the overlay that text appeared.
            record.latency = LatencyBreakdown::from_timeline(&timeline);
            Dictated { record, timeline }
        } else {
            let (text, polished_info, polish_ms) = self.polish(&raw);
            record.text = text.clone();
            record.raw = (text != raw).then(|| raw.clone());
            record.polish = polished_info;

            let payload = text::prepare(&text, self.config.inject.trailing_space);
            match self.injector.inject(&payload) {
                Ok(()) => {
                    timeline.mark(Mark::Injected);
                    record.injected = true;
                    // Key-release → text on screen: the number the pill prints.
                    let latency_ms = timeline
                        .perceived()
                        .map(ms)
                        .unwrap_or(0.0)
                        .round()
                        .clamp(0.0, f64::from(u32::MAX))
                        as u32;
                    self.pill.inserted(latency_ms);
                }
                Err(e) => {
                    // The transcript is good; only the delivery failed. Say so,
                    // and make sure the record below carries the text. With
                    // history off there is no file to point at, so the console
                    // gets the words themselves — they must be recoverable from
                    // somewhere.
                    eprintln!("  could not insert the text: {e:#}");
                    if self.history.enabled() {
                        eprintln!("  it is in {}", self.history.path().display());
                    } else {
                        eprintln!("  it was: {text}");
                    }
                    record.error = Some(format!("injection failed: {e:#}"));
                }
            }

            record.latency = LatencyBreakdown::from_timeline(&timeline);
            record.latency.polish_ms = polish_ms;
            Dictated { record, timeline }
        };

        note_salvage(dictated, cause)
    }

    /// A hold that produced no transcript, recorded against the timeline as it
    /// actually stood. Every no-transcript path inside [`App::capture`] goes
    /// through here: a blank timeline would log real audio as `audio_secs:
    /// 0.0` and make a network failure read as a broken microphone, which is
    /// exactly the false trail the 2026-08-02 investigation had to walk back.
    fn failed(&self, timeline: Timeline, message: String) -> Dictated {
        let mut record = DictationRecord::now(self.engine.name(), "");
        record.error = Some(message);
        record.latency = LatencyBreakdown::from_timeline(&timeline);
        Dictated { record, timeline }
    }

    /// A hold that has words but may never put them on screen: recorded in
    /// full, injected never.
    ///
    /// The dead-hotkey channel is why this exists. No key-up can arrive there,
    /// so the utterance cannot be confirmed over and [`App::deliver`] — which
    /// injects — is not allowed to run; but the words the user watched the
    /// overlay produce are real, and dropping them to an empty transcript the
    /// way [`App::failed`] does leaves them nowhere at all. This is that record
    /// with the text put back, sharing `failed`'s single place for building it
    /// so the failure half cannot drift.
    fn reported(&self, outcome: DictationOutcome, cause: String) -> Dictated {
        let DictationOutcome {
            text,
            timeline,
            cause: salvage_cause,
        } = outcome;
        let mut dictated = self.failed(timeline, cause);
        dictated.record.text = text.trim().to_string();
        note_salvage(dictated, salvage_cause)
    }

    /// The tail feed failed: the key is already up, so the utterance is over
    /// and the engine will never be asked to finalise it.
    ///
    /// This is the one path that delivers without [`Dictation::finish`], and it
    /// is deliberately the only one: the key-up is confirmed, so injecting what
    /// the overlay already showed cannot land mid-sentence. Words the user
    /// watched appear are not thrown away by the failure that ended the hold —
    /// but the record still names that failure, because a dictation that ended
    /// this way is not an ordinary one and the log has to say so.
    fn abandoned(&mut self, dictation: Dictation, error: anyhow::Error) -> Dictated {
        let outcome = {
            let pill = &mut self.pill;
            dictation.abandon(&mut |text: &str| {
                iris_core::vlog!("~ {text}");
                pill.set_partial_text(text);
            })
        };
        let cause = format!("{error:#}");
        if outcome.text.trim().is_empty() {
            return self.reported(outcome, cause);
        }
        eprintln!("  the hold ended early, delivering what was transcribed: {cause}");
        self.pill.processing();
        let dictated = self.deliver(outcome);
        note_cause(dictated, error)
    }

    /// Clean up the transcript, falling back to the raw text on any failure.
    ///
    /// Returns `(text, how it was polished, milliseconds spent)`.
    fn polish(&self, raw: &str) -> (String, Option<PolishInfo>, Option<f64>) {
        let Some(polisher) = &self.polisher else {
            return (raw.to_string(), None, None);
        };
        let request = PolishRequest::new(raw).with_hints(polish::hints(&self.config));
        match polish::run(&**polisher, &request) {
            Ok(polished) => {
                let info = PolishInfo {
                    source: polished.source.to_string(),
                    fallback: polished.fallback.as_ref().map(ToString::to_string),
                };
                (polished.text, Some(info), Some(ms(polished.duration)))
            }
            Err(e) => {
                // The transcript is still perfectly usable; polish is an
                // improvement, never a gate.
                eprintln!("  polish failed, inserting the raw transcript: {e:#}");
                (raw.to_string(), None, None)
            }
        }
    }
}

/// Name why a hold ended abnormally on the record it produced anyway.
///
/// A dictation can be both: the words were recovered *and* something went
/// wrong. Keeping only the words would log a dead microphone as an ordinary,
/// unusually fast dictation — and a dead microphone is the single most useful
/// thing in the file, because every hold after it will fail to arm capture.
fn note_cause(dictated: Dictated, cause: anyhow::Error) -> Dictated {
    note_reason(dictated, format!("{cause:#}"))
}

/// The mid-hold cause, when the hold had one. Every exit from [`App::capture`]
/// goes through this, so none of them can quietly return without it.
fn note_mid_hold(dictated: Dictated, cause: Option<anyhow::Error>) -> Dictated {
    match cause {
        Some(cause) => note_cause(dictated, cause),
        None => dictated,
    }
}

/// Why the words are a salvage rather than a transcript, when they are — see
/// [`DictationOutcome::cause`].
fn note_salvage(dictated: Dictated, cause: Option<String>) -> Dictated {
    match cause {
        Some(cause) => note_reason(dictated, cause),
        None => dictated,
    }
}

fn note_reason(mut dictated: Dictated, cause: String) -> Dictated {
    dictated.record.error = Some(match dictated.record.error.take() {
        Some(existing) => format!("{cause}; {existing}"),
        None => cause,
    });
    dictated
}

fn open_history(config: &Config, config_path: &std::path::Path) -> SessionLog {
    if config.history.enabled {
        SessionLog::open(config.history_path(config_path), config.history.max_entries)
    } else {
        SessionLog::disabled()
    }
}

/// Hand a path to whatever the desktop uses to open it.
///
/// v1's "settings window" is the config file in the user's editor: the file is
/// already the source of truth, it is commented, and a bespoke settings UI in a
/// crate whose brief says "no UI beyond the tray" would be the wrong thing to
/// build twice.
fn open_in_editor(path: &std::path::Path) -> Result<()> {
    #[cfg(windows)]
    {
        // `start` is a cmd builtin, hence `cmd /C`. The empty argument is the
        // window title, which `start` otherwise steals from the path.
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn()
            .context("launching the editor")?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .context("launching xdg-open")?;
    }
    Ok(())
}

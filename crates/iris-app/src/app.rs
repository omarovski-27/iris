//! The dictation loop: the state machine that is the product.
//!
//! ```text
//!         ┌──────── Command (tray) ────────┐
//!         ▼                                │
//!   ┌──────────┐  hotkey down   ┌───────────────┐  hotkey up   ┌────────────┐
//!   │   idle   │───────────────►│   capturing   │─────────────►│ finalising │
//!   └──────────┘                └───────────────┘              └────────────┘
//!         ▲                        frames → engine              polish, inject,
//!         └───────────────────────────────────────────────────  log, hide pill
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
use iris_core::dictation::{Dictation, DEFAULT_FINAL_TIMEOUT};
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
    /// Switch theme (the overlay's, once it lands).
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
    final_timeout: Duration,
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
            final_timeout: DEFAULT_FINAL_TIMEOUT,
        })
    }

    /// Print a latency breakdown after every dictation.
    #[must_use]
    pub fn with_report(mut self, report: bool) -> Self {
        self.report = report;
        self
    }

    /// How long to wait for the final transcript after the key comes up.
    #[must_use]
    pub fn with_final_timeout(mut self, timeout: Duration) -> Self {
        self.final_timeout = timeout;
        self
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
                self.persist();
            }
            Command::OpenSettings => {
                if let Err(e) = open_in_editor(&self.config_path) {
                    eprintln!("  cannot open {}: {e:#}", self.config_path.display());
                }
            }
            Command::Reload => match Config::load(&self.config_path) {
                Ok(config) => {
                    // The hotkey hook was installed in `main` and the audio
                    // source was configured there too; neither is rebuilt
                    // here, so a reload that changed them must say so instead
                    // of claiming they took effect.
                    let mut needs_restart = Vec::new();
                    if config.hotkey != self.config.hotkey {
                        needs_restart.push("hotkey");
                    }
                    if config.suppress_hotkey != self.config.suppress_hotkey {
                        needs_restart.push("suppress_hotkey");
                    }
                    if config.audio.device != self.config.audio.device {
                        needs_restart.push("audio.device");
                    }
                    if config.audio.warm != self.config.audio.warm {
                        needs_restart.push("audio.warm");
                    }
                    self.saved = config.clone();
                    self.config = config;
                    match engines::build(&self.config) {
                        Ok(engine) => self.engine = engine,
                        Err(e) => eprintln!("  keeping the previous engine: {e:#}"),
                    }
                    self.polisher = polish::build(&self.config);
                    self.history = open_history(&self.config, &self.config_path);
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

        self.pill.hide();
        self.audio.disarm();

        let dictated = match outcome {
            Ok(dictated) => dictated,
            Err(e) => {
                // No transcript to save, but the failure still belongs in the
                // log: "nothing happened when I pressed the key" is the hardest
                // bug to report without one.
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

        match &dictated.record.error {
            Some(message) => anyhow::bail!("{message}"),
            None => Ok(dictated),
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

        let mut on_partial = |text: &str| iris_core::vlog!("~ {text}");
        let events = dictation.events();

        let released_at = loop {
            select! {
                recv(frames) -> frame => {
                    let frame = frame.context("the audio thread stopped")?;
                    self.pill.update_level(audio::level(&frame));
                    dictation.feed(&frame)?;
                }
                recv(events) -> event => match event {
                    Ok(event) => dictation.absorb_event(event, &mut on_partial),
                    // The engine hung up; finish() turns that into a message.
                    Err(_) => break Instant::now(),
                },
                recv(keys) -> event => {
                    match event.context("the hotkey thread stopped")? {
                        HotkeyEvent::Up(at) => break at,
                        // A repeat press we never saw the release of. Ignore it
                        // rather than ending an utterance the user is still in.
                        HotkeyEvent::Down(_) => {}
                    }
                }
            }
        };

        // Whatever the device buffered but had not delivered is still the
        // user's speech; dropping it would truncate the last word.
        let mut tail = Vec::new();
        while let Ok(frame) = frames.try_recv() {
            tail.extend_from_slice(&frame);
        }
        if !tail.is_empty() {
            dictation.feed(&tail)?;
        }

        dictation.timeline_mut().mark_at(Mark::KeyUp, released_at);
        self.pill.processing();

        let outcome = dictation.finish(self.final_timeout, &mut on_partial)?;
        let mut timeline = outcome.timeline;
        let raw = outcome.text.trim().to_string();

        let mut record = DictationRecord::now(self.engine.name(), &raw);
        if raw.is_empty() {
            // Silence, or a key tapped by accident. Injecting nothing is right;
            // so is not pretending to the overlay that text appeared.
            record.latency = LatencyBreakdown::from_timeline(&timeline);
            return Ok(Dictated { record, timeline });
        }

        let (text, polished_info, polish_ms) = self.polish(&raw);
        record.text = text.clone();
        record.raw = (text != raw).then(|| raw.clone());
        record.polish = polished_info;

        let payload = text::prepare(&text, self.config.inject.trailing_space);
        match self.injector.inject(&payload) {
            Ok(()) => {
                timeline.mark(Mark::Injected);
                record.injected = true;
                self.pill.inserted();
            }
            Err(e) => {
                // The transcript is good; only the delivery failed. Say so, and
                // make sure the record below carries the text. With history off
                // there is no file to point at, so the console gets the words
                // themselves — they must be recoverable from somewhere.
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
        Ok(Dictated { record, timeline })
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

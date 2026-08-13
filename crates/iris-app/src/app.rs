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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{select, Receiver, Sender};
use iris_core::dictation::{Dictation, DictationOutcome};
use iris_core::engine::{Engine, FailureCause};
use iris_core::hotkey::{HotkeyEvent, Key};
use iris_core::latency::{ms, Mark, Timeline};
use iris_core::text;
use iris_polish::{PolishRequest, Polisher};

use crate::audio::{self, AudioSource};
use crate::config::{Config, EngineChoice, Theme};
use crate::history::{DictationRecord, LatencyBreakdown, PolishInfo, SessionLog};
use crate::inject::Injector;
use crate::notify::{FailureNotice, NoopFailureNotice};
use crate::pill::PillSink;
use crate::window::{NoopWindow, WindowSink};
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
    /// Rebind the push-to-talk key. Persisted immediately; needs a restart to
    /// take effect, because the hook is installed once in `main`.
    SetHotkey(Key),
    /// Show or hide the pill overlay. Persisted immediately; needs
    /// a restart, because the overlay is spawned once in `main`.
    SetOverlayEnabled(bool),
    /// Replace the vocabulary hint list and rebuild the active engine so it
    /// takes effect immediately — unlike the hotkey and overlay toggle, this
    /// is read at engine-build time, not once at startup.
    SetVocabulary(Vec<String>),
    /// Open the settings window, or focus it if already open.
    OpenSettings,
    /// Re-read the config file, applying anything edited by hand.
    Reload,
    /// Leave the loop and exit.
    Quit,
}

impl Command {
    /// Write this command's change into `config`, and say whether it wrote
    /// anything: `OpenSettings`, `Reload` and `Quit` set no field.
    ///
    /// This is the only place that knows which [`Config`] field each command
    /// stands for. Three callers need that mapping — the loop itself, the
    /// settings window moving its controls onto a confirmed change, and
    /// `--demo-window`'s stand-in for the loop — and a seventh settable field
    /// added to only two of them is a silently half-wired setting.
    pub fn apply_to(&self, config: &mut Config) -> bool {
        match self {
            Command::SetEngine(choice) => config.engine = *choice,
            Command::SetDevice(device) => config.audio.device = device.clone(),
            Command::SetPolish(enabled) => config.polish.enabled = *enabled,
            Command::SetTheme(theme) => config.theme = *theme,
            Command::SetHotkey(key) => config.hotkey = *key,
            Command::SetOverlayEnabled(enabled) => config.overlay_enabled = *enabled,
            Command::SetVocabulary(terms) => config.vocabulary = terms.clone(),
            Command::OpenSettings | Command::Reload | Command::Quit => return false,
        }
        true
    }
}

/// Names one command the settings window sent, so the answer to it can be
/// matched to the question rather than to whatever is at the head of a queue.
///
/// Ordering alone is not enough: the command channel outlives any one window,
/// and a change still in flight when the user closes the window is answered
/// after the next window has opened with an empty queue of its own. Pairing on
/// this id lets that answer be recognised as belonging to nobody and dropped —
/// see [`crate::window::WindowState::poll_outcomes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandId(u64);

impl CommandId {
    /// The next id, unique for the life of the process — which is the span
    /// that matters, because the window is rebuilt many times inside it.
    #[must_use]
    pub fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// What the loop did with a [`Command`] the settings window sent.
///
/// The window shows the user what happened, so a queued command is not yet an
/// answer: `App::apply` can decline a change and keep what works (a missing
/// API key, a microphone that will not open). One of these goes back for every
/// command the window sends, tagged with that command's [`CommandId`], so the
/// window can say "Saved" only for the ones that were, and say why for the
/// ones that were not — see `crate::window::state`'s `dispatch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The loop took the change (and persisted it, where it persists).
    Applied,
    /// The loop declined it and kept what it had, for this reason.
    Rejected(String),
}

/// Flip `quit_flag` — the one step every UI surface that can end the app
/// must perform *before* sending [`Command::Quit`] anywhere, never after.
///
/// A dictation whose finalise is already running polls this flag directly
/// (see [`App::capture`]'s doc comment) instead of waiting for its turn on a
/// command channel, so it must be able to observe the flip before it can
/// possibly observe the message that follows it. The tray's Quit item
/// (`tray::spawn`) calls this rather than storing the flag directly, so a
/// close path added later cannot reintroduce the narrower, tray-only fix
/// this replaces.
///
/// `crate::window::state::Env::request_quit` is the same sequence for a
/// window-driven quit and still exists for that purpose, but the settings
/// window's own close button (`X`, Alt+F4, the taskbar's "Close window") no
/// longer calls it — closing that window hides it and leaves Iris running,
/// deliberately, per the captain's 2026-08-11 request; see
/// `window::ui::draw_root`'s doc comment for the full story and `AGENTS.md`.
/// The tray's Quit item is today the only caller that actually ends the app.
pub fn flip_quit_flag(quit_flag: &AtomicBool) {
    quit_flag.store(true, Ordering::Release);
}

/// What one dictation produced.
#[derive(Debug, Clone)]
pub struct Dictated {
    /// The record appended to the session log.
    pub record: DictationRecord,
    /// The full timeline, for `--report`.
    pub timeline: Timeline,
}

/// What [`App::capture`] has to report back to [`App::dictate`].
///
/// A second variant beyond a plain `Dictated` only because a finalise can
/// lose its race with a tray Quit — see [`App::capture`]'s doc comment for
/// why that race exists and what wins it when.
enum CaptureOutcome {
    /// The ordinary case: a transcript (or a recorded failure) is in hand.
    /// Boxed: `QuitRequested` carries nothing, and clippy's `large_enum_variant`
    /// is right that leaving it unboxed would size every `CaptureOutcome` —
    /// including the common, no-quit case — to `Dictated`'s.
    Dictated(Box<Dictated>),
    /// Quit arrived before the finalise did. Delivery continues on a
    /// detached thread (see [`App::pending_finalize`]); this dictation adds
    /// nothing further to the pill or the session log itself.
    QuitRequested,
}

/// The longest gap between the release of one tap and the press of the next
/// that still counts as a hands-free-latch trigger — the double-click window
/// most desktop UIs use, picked from the middle of that convention's usual
/// 300-500 ms range.
///
/// Only a hold shorter than this window is even considered a candidate first
/// tap in the first place (see `App::capture`'s hold loop): an ordinary
/// hold-to-talk dictation is released well past it and finalises the instant
/// the key comes up, with nothing added to its wait — unchanged from before
/// this existed. The one case this constant does add latency to is a
/// dictation itself shorter than the window (a very fast single word said
/// and released quickly): it waits out the rest of the window to see whether
/// a second tap follows before finalising. That is an accepted, unavoidable
/// cost of any double-tap gesture — the same tradeoff every OS's
/// double-click detection makes — not a regression of ordinary
/// hold-to-talk, which this never touches.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

/// Safety bound on a hands-free latch: if nobody taps the hotkey again within
/// this long, the hold auto-stops and finalises through the exact same path
/// as a real key-up (see `LatchPhase::Latched` in `App::capture`'s hold loop)
/// rather than discarding what was captured. A forgotten latch is a live
/// hazard — an open microphone recording, and billing, indefinitely — so
/// this is generous but finite: several minutes, comfortably longer than any
/// real dictation, short enough that a forgotten latch does not run all day.
///
/// Overridable per-`App` by [`App::with_latch_cap`] so a test can exercise
/// the cap firing without a five-minute wait, the same way
/// [`App::with_final_timeout`] overrides the engine's own bound.
const MAX_LATCH_DURATION: Duration = Duration::from_secs(5 * 60);

/// How often `App::capture`'s hold loop checks [`App::quit_flag`] once the
/// key is no longer physically down — [`LatchPhase::AwaitingSecondTap`] and
/// [`LatchPhase::Latched`], never [`LatchPhase::Held`].
///
/// A held key bounds an ordinary hold-to-talk dictation to however long the
/// user's own finger stays down, so `App::run`'s outer loop missing a Quit
/// for that long (unread until this function returns) is the same
/// tolerated gap `App::capture`'s finalise wait had before the tray-Quit fix
/// — see `AGENTS.md`; `Held` keeps that same tolerated gap, unchanged. Once
/// the key is up, though, that justification is gone — the user is no longer
/// holding anything down — and a latch has no bound at all short of
/// `MAX_LATCH_DURATION`, which alone could leave Quit unread for minutes.
/// This tick is what closes that gap for both post-key-up phases — cheap,
/// since it is armed only there (`Held` selects on `never()` here, same as
/// every other conditional arm in this loop) — by breaking the hold exactly
/// as an ordinary key-up would the moment it sees the flag, and letting the
/// existing quit-during-finalise race (this function's doc comment) take it
/// from there.
const LATCH_QUIT_POLL: Duration = Duration::from_millis(100);

/// Bounded extra wait for the very first capture frames on a hold that ends
/// having produced none at all — see `App::capture`'s tail-feed comment.
///
/// A warm microphone (`Config::audio.warm`, on by default) keeps its capture
/// stream open for the app's whole life specifically so a dictation never
/// pays a cold-open cost — but "the stream object is open" is not the same
/// claim as "a frame is in the channel the instant the key comes up", and a
/// short hold is exactly where the gap between those two shows: real-world
/// session logs show the failures tagged "almost no audio reached the
/// transcription engine" cluster overwhelmingly under ~2s of held key, median
/// ~0.9s, while successful dictations run a median 9.3s — the same short-hold
/// population, not a random slice of all dictations. A 9s hold loses nothing
/// from a startup lag this size; a 1s hold can lose the entire utterance to
/// it. This constant does not explain *why* the first frame is sometimes
/// late (that needs live Windows telemetry this repo cannot gather — see
/// `AGENTS.md`'s "almost no audio" entry), only that a short, bounded second
/// chance for it costs nothing on every dictation that does not need it: the
/// tail-feed's normal non-blocking drain runs first and unconditionally, and
/// this extra wait is only entered when that drain still leaves the whole
/// dictation at exactly zero captured samples — the one situation where the
/// entire transcript is riding on whatever arrives next.
const CAPTURE_START_GRACE: Duration = Duration::from_millis(300);

/// Whether a hold released at `released_at` is short enough to be a
/// candidate first tap of a double-tap latch, rather than an ordinary
/// hold-to-talk release.
///
/// A free function so the exact boundary can be pinned by a unit test
/// against synthetic instants, without waiting on real time or spinning up
/// the full hold loop.
fn is_candidate_tap(pressed_at: Instant, released_at: Instant) -> bool {
    released_at.saturating_duration_since(pressed_at) <= DOUBLE_TAP_WINDOW
}

/// Where `App::capture`'s hold loop is in the tap sequence.
///
/// `Held` is the only phase an ordinary hold-to-talk dictation ever visits —
/// real speech almost always outlasts [`DOUBLE_TAP_WINDOW`], so its `Up`
/// breaks the loop immediately, exactly as before this existed. The other two
/// phases exist only for a hold that released *inside* that window, which
/// might be the first tap of a double-tap latch.
#[derive(Debug, Clone, Copy)]
enum LatchPhase {
    /// Waiting for the matching key-up (or a stray repeat key-down, ignored
    /// as before this existed).
    Held,
    /// The key came up within [`DOUBLE_TAP_WINDOW`] of the press. If a
    /// `Down` follows before `up_at + DOUBLE_TAP_WINDOW`, this becomes
    /// `Latched`; otherwise it was just a short, ordinary hold, and it
    /// finalises with `up_at` the same as `Held` would have.
    AwaitingSecondTap {
        /// When the first tap's key-up landed.
        up_at: Instant,
    },
    /// A double-tap landed: the hold continues with the key released. The
    /// next `Down` is a single tap that stops it and finalises with that
    /// instant; if none comes within the latch cap of `since`, the cap stops
    /// it instead — see [`MAX_LATCH_DURATION`].
    Latched {
        /// When the latch engaged, the deadline for the auto-stop cap.
        since: Instant,
    },
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
    /// Told when a delivery fails, so the words and the failure itself can
    /// reach the user somewhere other than a console that may not exist.
    /// [`NoopFailureNotice`](crate::notify::NoopFailureNotice) by default —
    /// only the real resident loop wires a live one; see
    /// [`App::with_failure_notice`].
    notice: Arc<dyn FailureNotice>,
    pill: Box<dyn PillSink>,
    /// Opens/focuses the settings window on [`Command::OpenSettings`].
    /// [`NoopWindow`] by default — only the real resident loop wires a live
    /// one; see [`App::with_window`].
    window: Box<dyn WindowSink>,
    /// A second command source the window sends on directly, so its changes
    /// reach [`App::apply`] the same way tray commands do. `never()` by
    /// default, which simply never becomes ready; see [`App::with_window_commands`].
    window_commands: Receiver<(CommandId, Command)>,
    /// Where the result of each of those commands goes back, tagged with the
    /// id it came in with. `None` until [`App::with_window_commands`] wires
    /// one, and only ever fed for commands that arrived on `window_commands`:
    /// the tray has no status line to put an answer in.
    window_outcomes: Option<Sender<(CommandId, CommandOutcome)>>,
    /// Fires once for every later launch that found this process already
    /// running, so it can raise the window instead of doing nothing visible.
    /// `never()` by default; see [`App::with_reopen_signal`].
    reopen: Receiver<()>,
    history: SessionLog,
    audio: A,
    count: usize,
    report: bool,
    /// Set only by [`App::with_final_timeout`]. Left `None`, every dictation
    /// asks the engine in force for its own bound, so switching engines at
    /// runtime switches the wait with it instead of keeping the one the app
    /// started with.
    final_timeout: Option<Duration>,
    /// Set only by [`App::with_latch_cap`]. Left `None`, every hands-free
    /// latch auto-stops after [`MAX_LATCH_DURATION`]; a test overrides this
    /// so it can exercise the cap firing without a five-minute wait.
    latch_cap: Option<Duration>,
    /// Flipped by whichever UI surface sends [`Command::Quit`] — today only
    /// the tray's Quit item; the settings window's close button no longer
    /// sends it, see [`flip_quit_flag`]'s doc comment — the instant it sends
    /// it, via [`flip_quit_flag`]. That flips before `App::run`'s own
    /// select loop ever gets a turn, because that loop is blocked inside
    /// `dictate` for as long as a finalise is running (see
    /// [`App::capture`]'s doc comment). Polling this from inside a finalise
    /// wait is what lets Quit cut that wait short without touching `control`
    /// or `window_commands` themselves, which stay untouched for `App::run`
    /// to break its loop on normally once `dictate` returns.
    quit_flag: Arc<AtomicBool>,
    /// A dictation whose finalise outlived a Quit: still delivering (inject +
    /// log) on a detached thread. `main` joins this after tearing down the
    /// tray/overlay/window — so the app looks closed immediately while the
    /// words are still guaranteed to land, never dropped for the sake of a
    /// fast exit. See [`App::capture`]'s doc comment.
    pending_finalize: Option<std::thread::JoinHandle<()>>,
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
        let engine = engines::build(&config).with_context(|| {
            format!(
                "starting the {} engine (edit {} to add a key or pick a different engine, \
                 then start Iris again)",
                config.engine,
                config_path.display()
            )
        })?;
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
            notice: Arc::new(NoopFailureNotice),
            pill,
            window: Box::new(NoopWindow),
            window_commands: crossbeam_channel::never(),
            window_outcomes: None,
            reopen: crossbeam_channel::never(),
            history,
            audio,
            count: 0,
            report: false,
            final_timeout: None,
            latch_cap: None,
            quit_flag: Arc::new(AtomicBool::new(false)),
            pending_finalize: None,
        })
    }

    /// Share the app's quit flag with this app, in place of the private one
    /// [`App::new`] creates. The real resident loop wires the same [`Arc`]
    /// into both `tray::spawn` and `window::spawn` (the window keeps a copy
    /// even though its close button no longer flips it — see
    /// [`flip_quit_flag`]'s doc comment), so a click on the tray's Quit is
    /// visible to a finalise wait already in progress; see [`App::capture`]'s
    /// doc comment. Tests that only send [`Command::Quit`] on `control` or
    /// `window_commands` do not need this — that path is unaffected.
    #[must_use]
    pub fn with_quit_flag(mut self, quit_flag: Arc<AtomicBool>) -> Self {
        self.quit_flag = quit_flag;
        self
    }

    /// The flag every close path flips via [`flip_quit_flag`]. Handed to
    /// `tray::spawn` and `window::spawn` by the real resident loop; see
    /// [`App::with_quit_flag`].
    #[must_use]
    pub fn quit_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.quit_flag)
    }

    /// Take the handle to a dictation still finishing in the background after
    /// a quit-during-finalize (`None` when the last dictation, if any, was
    /// already fully delivered before `run` returned).
    ///
    /// The caller — `main`'s resident `run`, right after tearing down the
    /// tray/overlay/window so the app already looks closed — must join this
    /// before the process actually exits, or the words the engine was still
    /// finalising are lost with it. See [`App::capture`]'s doc comment.
    pub fn take_pending_finalize(&mut self) -> Option<std::thread::JoinHandle<()>> {
        self.pending_finalize.take()
    }

    /// Give the loop a live settings window to open on [`Command::OpenSettings`].
    #[must_use]
    pub fn with_window(mut self, window: Box<dyn WindowSink>) -> Self {
        self.window = window;
        self
    }

    /// Raise the settings window whenever a later launch attempt signals it
    /// found this process already running, in place of the default
    /// `never()` receiver that simply never fires.
    ///
    /// See [`crate::single_instance`] for what sends on this.
    #[must_use]
    pub fn with_reopen_signal(mut self, reopen: Receiver<()>) -> Self {
        self.reopen = reopen;
        self
    }

    /// Open (or focus) the settings window right now, without waiting for a
    /// [`Command::OpenSettings`] to arrive on a channel.
    ///
    /// The one caller today is `main`, for the deliberate launches — the
    /// icon, the Start Menu — that must show the window immediately rather
    /// than leaving it to the tray's `Settings` item; the Startup shortcut
    /// does not call this, so a boot-time autostart stays in the tray.
    pub fn open_window(&self) {
        self.window.open();
    }

    /// Give the loop somewhere real to report a failed injection.
    ///
    /// Only the resident loop (`main.rs`'s `start_resident`) should ever pass
    /// [`crate::notify::SystemFailureNotice`] here — see the module docs on
    /// [`crate::notify`] for why every other caller keeps the default
    /// [`NoopFailureNotice`].
    #[must_use]
    pub fn with_failure_notice(mut self, notice: Arc<dyn FailureNotice>) -> Self {
        self.notice = notice;
        self
    }

    /// Drain [`Command`]s the settings window sends directly, alongside the
    /// tray's `control` channel passed to [`App::run`], and answer each one on
    /// `outcomes`.
    ///
    /// The two are one wiring step because they are one conversation: the
    /// window has to be told whether the change it asked for happened. Each
    /// answer carries the [`CommandId`] of the command it answers, so a window
    /// that has been closed and reopened in between cannot read someone else's
    /// answer as its own.
    #[must_use]
    pub fn with_window_commands(
        mut self,
        commands: Receiver<(CommandId, Command)>,
        outcomes: Sender<(CommandId, CommandOutcome)>,
    ) -> Self {
        self.window_commands = commands;
        self.window_outcomes = Some(outcomes);
        self
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

    /// Override the auto-stop cap on a hands-free latch, in place of
    /// [`MAX_LATCH_DURATION`]. Test-only in practice — a real cap this small
    /// would defeat the safety margin the constant exists for.
    #[must_use]
    pub fn with_latch_cap(mut self, cap: Duration) -> Self {
        self.latch_cap = Some(cap);
        self
    }

    /// The auto-stop cap in force for a hands-free latch.
    fn latch_cap(&self) -> Duration {
        self.latch_cap.unwrap_or(MAX_LATCH_DURATION)
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
        // A clone, not `&self.window_commands`, for the same reason `frames`
        // is cloned out above: `select!` would otherwise need to hold a
        // borrow of `self` for the arm body's `self.apply(..)` call too.
        // Swapped for a never-ready receiver on disconnect (mirroring
        // `capture`'s `engine_events_open`) so a window that has gone away
        // cannot turn this into a busy loop — unlike `control`, losing the
        // window must not end the whole dictation loop.
        let mut window_commands = self.window_commands.clone();
        // Cloned out for the same borrow-checker reason as `window_commands`
        // above: `select!`'s arm body still needs `&mut self` for `apply`,
        // even though this arm never calls it.
        let reopen = self.reopen.clone();
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
                        if self.apply(command, None)?.is_break() {
                            return Ok(());
                        }
                        continue;
                    }
                    Err(_) => return Ok(()),
                },
                recv(window_commands) -> command => match command {
                    Ok((id, command)) => {
                        if self.apply(command, Some(id))?.is_break() {
                            return Ok(());
                        }
                        continue;
                    }
                    Err(_) => {
                        window_commands = crossbeam_channel::never();
                        continue;
                    }
                },
                // A later launch found this instance already running: show
                // it, the same action `Command::OpenSettings` takes.
                recv(reopen) -> _ => {
                    self.window.open();
                    continue;
                }
                // Idle audio from a warm microphone. Dropped on purpose.
                recv(frames) -> _ => continue,
            };

            if let Err(e) = self.dictate(pressed_at, &frames, keys) {
                eprintln!("  dictation failed: {e:#}");
            }
        }
    }

    /// Apply a tray or window command, persisting anything the user would
    /// expect to survive a restart.
    ///
    /// `from_window` — the [`CommandId`] the window sent it under, `None` for
    /// the tray — decides whether the result is reported back on
    /// [`App::with_window_commands`]' channel, and under which id. Two of
    /// these commands can be declined — an engine that will not build, a
    /// microphone that will not open — and the loop keeping what works is only
    /// half the answer: the UI that asked has to be able to say so instead of
    /// showing "Saved" over a change that never happened.
    fn apply(
        &mut self,
        command: Command,
        from_window: Option<CommandId>,
    ) -> Result<std::ops::ControlFlow<()>> {
        let mut outcome = CommandOutcome::Applied;
        // Matched by reference so every arm that touches the config can go
        // through `Command::apply_to` rather than naming the field itself.
        match &command {
            Command::Quit => {
                flip_quit_flag(&self.quit_flag);
                return Ok(std::ops::ControlFlow::Break(()));
            }
            Command::SetEngine(choice) => {
                let choice = *choice;
                // Nothing to do only when the choice is *both* what is running
                // and what the file holds. Under `--engine` those diverge for
                // the whole session, and the window renders the file: comparing
                // against the running engine alone would answer `Applied`
                // without writing anything, and the next refresh would snap the
                // picker back.
                if choice == self.config.engine && choice == self.on_disk().engine {
                    self.report(from_window, outcome);
                    return Ok(std::ops::ControlFlow::Continue(()));
                }
                let previous = self.config.engine;
                command.apply_to(&mut self.config);
                match engines::build(&self.config) {
                    Ok(engine) => {
                        self.pill.set_engine(engine.name());
                        self.engine = engine;
                        match self.persist(&command) {
                            Ok(()) => println!("  engine: {choice}"),
                            Err(e) => outcome = Self::persist_failed(&e),
                        }
                    }
                    Err(e) => {
                        // Keep dictating with what works rather than leaving the
                        // app in a state where every hotkey press fails.
                        self.config.engine = previous;
                        eprintln!("  cannot switch to {choice}: {e:#}");
                        outcome =
                            CommandOutcome::Rejected(format!("cannot switch to {choice}: {e:#}"));
                    }
                }
            }
            Command::SetDevice(device) => match self.audio.set_device(device.clone()) {
                Ok(()) => {
                    command.apply_to(&mut self.config);
                    let description = self.audio.describe();
                    match self.persist(&command) {
                        Ok(()) => println!("  microphone: {description}"),
                        Err(e) => outcome = Self::persist_failed(&e),
                    }
                }
                Err(e) => {
                    eprintln!("  cannot switch microphone: {e:#}");
                    outcome = CommandOutcome::Rejected(format!("cannot switch microphone: {e:#}"));
                }
            },
            Command::SetPolish(enabled) => {
                let enabled = *enabled;
                command.apply_to(&mut self.config);
                self.polisher = polish::build(&self.config);
                match self.persist(&command) {
                    Ok(()) => println!("  polish: {}", if enabled { "on" } else { "off" }),
                    Err(e) => outcome = Self::persist_failed(&e),
                }
            }
            Command::SetTheme(theme) => {
                let theme = *theme;
                command.apply_to(&mut self.config);
                self.pill.set_theme(theme);
                if let Err(e) = self.persist(&command) {
                    outcome = Self::persist_failed(&e);
                }
            }
            Command::SetHotkey(key) => {
                // `self.config.hotkey` is deliberately left alone: the hook
                // was installed with the old key in `main` and stays that
                // way until a restart, exactly like a hand-edited hotkey
                // does through `Command::Reload`. Only `saved` — what the
                // *next* launch will read — takes the new value.
                let key = *key;
                match self.persist(&command) {
                    Ok(()) => {
                        println!("  hotkey: {key} (restart Iris for this to take effect)");
                    }
                    Err(e) => outcome = Self::persist_failed(&e),
                }
            }
            Command::SetOverlayEnabled(enabled) => {
                // Same reasoning as `SetHotkey`: the overlay is spawned (or
                // not) once in `main`, so `self.config.overlay_enabled` stays
                // as it was until a restart.
                let enabled = *enabled;
                match self.persist(&command) {
                    Ok(()) => println!(
                        "  overlay: {} (restart Iris for this to take effect)",
                        if enabled { "on" } else { "off" }
                    ),
                    Err(e) => outcome = Self::persist_failed(&e),
                }
            }
            Command::SetVocabulary(terms) => {
                let count = terms.len();
                let previous = self.config.vocabulary.clone();
                command.apply_to(&mut self.config);
                match engines::build(&self.config) {
                    Ok(engine) => {
                        self.pill.set_engine(engine.name());
                        self.engine = engine;
                        match self.persist(&command) {
                            // Term count only — never the terms themselves.
                            Ok(()) => println!("  vocabulary: {count} term(s)"),
                            Err(e) => outcome = Self::persist_failed(&e),
                        }
                    }
                    Err(e) => {
                        self.config.vocabulary = previous;
                        eprintln!("  cannot update vocabulary: {e:#}");
                        outcome =
                            CommandOutcome::Rejected(format!("cannot update vocabulary: {e:#}"));
                    }
                }
            }
            Command::OpenSettings => self.window.open(),
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
                    if loaded.overlay_enabled != self.config.overlay_enabled {
                        needs_restart.push("overlay_enabled");
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
                    config.overlay_enabled = self.config.overlay_enabled;
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
                    // this is the live transcript's opt-in, so "reloaded"
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
        self.report(from_window, outcome);
        Ok(std::ops::ControlFlow::Continue(()))
    }

    /// Tell the settings window what became of the command it sent.
    ///
    /// Best-effort: a window that has closed between sending and now is not a
    /// dictation-loop problem, and the change stands either way.
    fn report(&self, from_window: Option<CommandId>, outcome: CommandOutcome) {
        let Some(id) = from_window else {
            return;
        };
        if let Some(outcomes) = &self.window_outcomes {
            let _ = outcomes.send((id, outcome));
        }
    }

    /// Write the file with `command`'s one field changed and, only on
    /// success, make what was written [`App::saved`].
    ///
    /// The candidate is built on [`App::on_disk`] — the file as it stands
    /// *now* — with only this command's field applied on top, so a change
    /// made to `config.toml` by hand while Iris is running survives the next
    /// setting change instead of being overwritten by a snapshot taken before
    /// it. The settings window points the user straight at that file for the
    /// API keys, so the hand edit is the documented workflow, not an edge
    /// case. Nothing else in the candidate comes from memory, and in
    /// particular nothing comes from [`App::config`], which carries the CLI
    /// overrides this file must never learn about.
    ///
    /// `saved` moves only on a successful write, so a write failure leaves it
    /// exactly where it was instead of stranding the rejected value in memory
    /// to be smuggled onto disk by the *next* command's successful save. The
    /// failure is the caller's to answer for: a change the loop took in
    /// memory but could not write is not saved, and the UI that asked has to
    /// hear that rather than "Saved" over a value the next launch will not
    /// see. See [`App::persist_failed`], which every `Set*` arm routes it
    /// through.
    fn persist(&mut self, command: &Command) -> Result<()> {
        let mut candidate = self.on_disk();
        command.apply_to(&mut candidate);
        candidate
            .save(&self.config_path)
            .with_context(|| format!("cannot save {}", self.config_path.display()))?;
        self.saved = candidate;
        Ok(())
    }

    /// The configuration as the file holds it now, falling back to
    /// [`App::saved`] when there is nothing readable to merge with.
    ///
    /// The same last-good-snapshot rule the settings window applies to its own
    /// re-reads: a file that is unreadable for a moment — mid-write, or
    /// hand-edited into a parse error — must not turn a setting change into a
    /// reset to defaults. A *missing* file gets the same treatment for a
    /// stronger reason: [`Config::load`] reports it as the defaults it is, so
    /// merging onto that reading would quietly rewrite every setting — API
    /// keys included — out of a file someone deleted or a drive that dropped
    /// out. `main` creates the file before the loop starts, so the last
    /// snapshot is the better answer in both cases.
    fn on_disk(&self) -> Config {
        if !self.config_path.exists() {
            return self.saved.clone();
        }
        Config::load(&self.config_path).unwrap_or_else(|_| self.saved.clone())
    }

    /// Report a failed [`App::persist`] on the console and to the window.
    ///
    /// Unconditional, like every other delivery failure: the tray sends the
    /// same commands and has no status line to read.
    fn persist_failed(e: &anyhow::Error) -> CommandOutcome {
        eprintln!("  {e:#}");
        CommandOutcome::Rejected(format!("{e:#}"))
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

        let outcome = match self.capture(pressed_at, frames, keys) {
            Ok(CaptureOutcome::QuitRequested) => {
                // The finalise that was still running when Quit arrived is
                // being delivered on a detached thread instead (see
                // `App::capture`'s doc comment) — this call contributes
                // nothing further: no pill update (the overlay is on its way
                // down with the rest of the app) and no session-log append
                // (the background thread owns that record and appends it
                // itself once the engine answers).
                self.audio.disarm();
                anyhow::bail!("quitting: the pending dictation is finishing in the background");
            }
            Ok(CaptureOutcome::Dictated(dictated)) => Ok(*dictated),
            Err(e) => Err(e),
        };

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
    ///
    /// **The finalise wait (`dictation.finish`, below) is the one place a
    /// dictation can make the resident loop miss a Quit.** Everything
    /// before it — holding the key, feeding audio — is bounded by the user's
    /// own key hold, but `finish` blocks its caller for up to the engine's
    /// [`Engine::final_timeout`] (6s typical for Deepgram, ~14s when the
    /// connect grace had to be bought first — see `AGENTS.md`), and
    /// `App::run`'s select loop only services `control`/`window_commands`
    /// *between* calls to `dictate`. A Quit clicked while that wait is still
    /// running used to sit unread on the channel for the rest of it — visible
    /// on the real desktop as a tray icon (or a settings window) that does
    /// not go away, and exactly what `Engine::final_timeout`'s own doc
    /// comment already warned this shape would do.
    ///
    /// The fix does not touch `Dictation::finish` or its timeout — shortening
    /// that trades data loss for responsiveness, which is off the table (see
    /// `AGENTS.md`). Instead `finish` runs on a spawned thread here, and this
    /// function polls that thread's result against [`App::quit_flag`] rather
    /// than blocking on it outright: the flag is flipped via
    /// [`flip_quit_flag`] by whichever UI surface sends `Command::Quit` —
    /// today only the tray (`tray::spawn`); see `flip_quit_flag`'s doc
    /// comment for why the settings window's close button no longer does —
    /// at the same instant it sends it, so it can be noticed without waiting
    /// for `App::run`'s own turn.
    /// If the flag wins the race, this returns
    /// [`CaptureOutcome::QuitRequested`] immediately and hands the
    /// still-running finalise to a second, detached thread that delivers it
    /// (polish, inject, log) once the engine actually answers —
    /// [`App::pending_finalize`] is `main`'s handle to wait for that before
    /// the process truly exits, so the words are never lost, only delivered
    /// after the tray icon (and the window, if it was open) are already
    /// gone. If `finish` wins the race first, as it does on every dictation
    /// that is not racing a quit, behaviour is unchanged from before this
    /// existed.
    fn capture(
        &mut self,
        pressed_at: Instant,
        frames: &Receiver<Vec<i16>>,
        keys: &Receiver<HotkeyEvent>,
    ) -> Result<CaptureOutcome> {
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

        // See `LatchPhase` for the tap-sequence states this drives through.
        // `pressed_at` — this function's own parameter — is the very first
        // `Down` of whichever tap sequence started this dictation.
        let mut phase = LatchPhase::Held;

        let held = loop {
            let event_rx = if engine_events_open {
                &events
            } else {
                &never_events
            };
            // Paused (not failed — `frames_open` is untouched) while
            // `AwaitingSecondTap`: whatever is sitting in the channel at the
            // first tap's key-up is the tail an ordinary short dictation
            // drains and feeds as one batch below, exactly as a plain
            // hold-to-talk release always has. Feeding it here one frame at a
            // time instead — which is what continuing to select on this arm
            // during the wait would do — would drain that same backlog
            // through the per-frame path before the loop ever gets back
            // there, silently changing a real product behaviour (a batched
            // flush an engine can treat differently from steady per-frame
            // pushes) for every dictation short enough to reach
            // `AwaitingSecondTap`, latch or not.
            let frame_rx = if frames_open && !matches!(phase, LatchPhase::AwaitingSecondTap { .. })
            {
                frames
            } else {
                &never_frames
            };
            // Only `AwaitingSecondTap` and `Latched` ever arm a deadline — see
            // `LatchPhase`. `Held` selects on `never()`, so an ordinary
            // hold-to-talk dictation adds not one extra tick to its wait.
            let timeout_rx = match phase {
                LatchPhase::Held => crossbeam_channel::never(),
                LatchPhase::AwaitingSecondTap { up_at } => {
                    crossbeam_channel::at(up_at + DOUBLE_TAP_WINDOW)
                }
                LatchPhase::Latched { since } => crossbeam_channel::at(since + self.latch_cap()),
            };
            // Armed once the key is no longer physically down — see
            // `LATCH_QUIT_POLL`. `Held` never selects on this at all.
            let quit_poll_rx = if matches!(phase, LatchPhase::Held) {
                crossbeam_channel::never()
            } else {
                crossbeam_channel::after(LATCH_QUIT_POLL)
            };
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
                        Ok(HotkeyEvent::Up(at)) => match phase {
                            LatchPhase::Held => {
                                if is_candidate_tap(pressed_at, at) {
                                    // Short enough to maybe be the first tap
                                    // of a double-tap latch — give it the
                                    // window to find out. An ordinary hold
                                    // released past the window never reaches
                                    // this arm; see `DOUBLE_TAP_WINDOW`.
                                    phase = LatchPhase::AwaitingSecondTap { up_at: at };
                                } else {
                                    break Ok(at);
                                }
                            }
                            // The event that ends either of these phases is a
                            // `Down` (the second tap, or the tap that stops a
                            // latch), not an `Up`. A stray release here is
                            // ignored, the same way a repeat `Down` is
                            // ignored in `Held`.
                            LatchPhase::AwaitingSecondTap { .. } | LatchPhase::Latched { .. } => {}
                        },
                        Ok(HotkeyEvent::Down(at)) => match phase {
                            // A repeat press we never saw the release of.
                            // Ignore it rather than ending an utterance the
                            // user is still in.
                            LatchPhase::Held => {}
                            LatchPhase::AwaitingSecondTap { .. } => {
                                // The double-tap: latch on, hands-free.
                                self.pill.set_latched(true);
                                phase = LatchPhase::Latched { since: at };
                            }
                            // The single tap that stops a latched recording.
                            LatchPhase::Latched { .. } => break Ok(at),
                        },
                        Err(e) => match phase {
                            LatchPhase::Held => break Err(e),
                            // A confirmed `Up` already ended this utterance
                            // legitimately (the arm above) — losing the
                            // hotkey channel from here on cannot make that
                            // any less true, so finalise with what already
                            // happened instead of erroring an otherwise-
                            // ordinary short dictation. This is exactly the
                            // gap a single-shot channel that closes right
                            // after its one `Up` opens (`--demo-dictation`,
                            // `Rig::dictate` in the test harness) — real
                            // usage never sees this arm, because the
                            // production hotkey channel outlives every
                            // dictation.
                            LatchPhase::AwaitingSecondTap { up_at } => break Ok(up_at),
                            // No further tap can ever stop this latch;
                            // finalise with what was captured rather than
                            // losing it — the same "never discard audio"
                            // rule the auto-stop cap follows.
                            LatchPhase::Latched { .. } => break Ok(Instant::now()),
                        },
                    }
                }
                recv(timeout_rx) -> _ => {
                    match phase {
                        LatchPhase::Held => unreachable!("Held never arms a timeout"),
                        // Nobody double-tapped in time: this was an ordinary,
                        // if unusually short, hold-to-talk dictation.
                        LatchPhase::AwaitingSecondTap { up_at } => break Ok(up_at),
                        // Safety cap: finalise exactly like a real key-up
                        // rather than discarding what was captured — see
                        // `MAX_LATCH_DURATION`.
                        LatchPhase::Latched { .. } => break Ok(Instant::now()),
                    }
                }
                recv(quit_poll_rx) -> _ => {
                    // Never fires while `Held` — see `LATCH_QUIT_POLL`. Quit
                    // arriving while `AwaitingSecondTap` or `Latched` must not
                    // wait out the rest of the double-tap window or the latch
                    // cap: end the hold now, exactly as a real key-up would,
                    // and let the quit-during-finalise race this function's
                    // doc comment describes take over from here —
                    // `MAX_LATCH_DURATION` still finalises normally, never
                    // discards audio.
                    if self.quit_flag.load(Ordering::Acquire) {
                        break Ok(Instant::now());
                    }
                }
            }
        };
        // Whichever way the loop ended, the latch visual (if it was ever on)
        // must not survive past it: `processing()` is next, and a latch that
        // is still "on" in the overlay while the engine finalises would read
        // as still-recording.
        self.pill.set_latched(false);

        let released_at = match held {
            Ok(at) => at,
            Err(e) => {
                // The hotkey channel is the only source a key-up can ever come
                // from, so this hold can never be confirmed as over. Salvaging
                // into an injection here would type into a window whose user
                // may still be mid-sentence; the words are reported, not typed.
                let outcome = dictation.abandon(&mut |_| {});
                let dictated = self.reported(outcome, format!("{e:#}"));
                return Ok(CaptureOutcome::Dictated(Box::new(note_mid_hold(
                    dictated,
                    mid_hold_failure,
                ))));
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
            // Every frame already queued is drained above with no added
            // latency, win or lose. Only when that leaves this dictation at
            // literally zero captured samples — nothing fed during the whole
            // hold and nothing sitting in the channel at key-up either — is
            // there anything to gain from waiting: a hold with any real audio
            // already has words to lose nothing by finalising now, so this
            // never adds a millisecond to an ordinary dictation. See
            // `CAPTURE_START_GRACE`'s doc comment for why a short hold is
            // exactly the case this exists for.
            if dictation.audio_secs() == 0.0 && tail.is_empty() {
                let deadline = Instant::now() + CAPTURE_START_GRACE;
                while let Ok(frame) = frames.recv_deadline(deadline) {
                    tail.extend_from_slice(&frame);
                }
            }
            if !tail.is_empty() {
                if let Err(e) = dictation.feed(&tail) {
                    let dictated = self.abandoned(dictation, e);
                    return Ok(CaptureOutcome::Dictated(Box::new(note_mid_hold(
                        dictated,
                        mid_hold_failure,
                    ))));
                }
            }
        }

        self.pill.processing();

        // See this function's doc comment: `finish` runs off-thread so this
        // loop can poll `quit_flag` instead of blocking on it outright.
        let timeout = self.final_timeout();
        let (result_tx, result_rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("iris-finalize".into())
            .spawn(move || {
                // Live-text-during-finalize (`set_partial_text` here) is not
                // forwarded to the pill from this thread — plumbing it back
                // would need a second channel, and `show_live_text` is off by
                // default (`Config::show_live_text`), so nothing is visibly
                // lost for the shipped default. A dictation that wins the
                // race against `quit_flag` (the common case) never even
                // reaches this trade-off from the user's side, since nothing
                // upstream of the pill changed.
                let outcome = dictation.finish(timeout, &mut |_text: &str| {});
                let _ = result_tx.send(outcome);
            })
            .expect("spawning the finalize thread");

        const FINALIZE_POLL: Duration = Duration::from_millis(25);
        let finished = loop {
            match result_rx.recv_timeout(FINALIZE_POLL) {
                Ok(outcome) => break Some(outcome),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if self.quit_flag.load(Ordering::Acquire) {
                        break None;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    unreachable!("the finalize thread always sends before its sender drops")
                }
            }
        };

        let Some(finished) = finished else {
            // Quit won the race: hand the still-running finalise to a second,
            // detached thread rather than waiting for it or dropping it —
            // see this function's doc comment. Everything it needs is cloned
            // out now, while `self` is still here to clone it from.
            let engine_name = self.engine.name();
            let config = self.config.clone();
            let polisher = self.polisher.clone();
            let injector = Arc::clone(&self.injector);
            let notice = Arc::clone(&self.notice);
            let mut history = self.history.clone();
            let report = self.report;
            let count = self.count;
            let handle = std::thread::Builder::new()
                .name("iris-finalize-deliver".into())
                .spawn(move || {
                    let Ok(finished) = result_rx.recv() else {
                        return;
                    };
                    let dictated = match finished {
                        Ok(outcome) => deliver_outcome(
                            engine_name,
                            outcome,
                            &config,
                            polisher.as_deref(),
                            &*injector,
                            &*notice,
                            &history,
                            |_latency_ms| {},
                        ),
                        Err(e) => {
                            let message = format!("{e:#}");
                            let never_connected = e.never_connected;
                            let failure_cause = e.failure_cause;
                            failed_outcome(
                                engine_name,
                                &*notice,
                                e.timeline,
                                message,
                                never_connected,
                                failure_cause,
                            )
                        }
                    };
                    if let Err(e) = history.append(&dictated.record) {
                        eprintln!("  cannot write the session log: {e:#}");
                    }
                    if report {
                        println!("{}", dictated.timeline.report(count));
                    }
                    if let Some(message) = &dictated.record.error {
                        if !dictated.record.injected {
                            eprintln!("  dictation failed while quitting: {message}");
                        }
                    }
                })
                .expect("spawning the deferred-delivery thread");
            self.pending_finalize = Some(handle);
            return Ok(CaptureOutcome::QuitRequested);
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
                let never_connected = e.never_connected;
                let failure_cause = e.failure_cause;
                self.failed(e.timeline, message, never_connected, failure_cause)
            }
        };
        // Always, even when the hold went on to produce a transcript: what came
        // back is then the part of the utterance that survived, and a record
        // that does not say so reads as an ordinary dictation that happened to
        // be short. The words the microphone never captured are invisible here
        // by definition; the cause is the only trace they leave.
        Ok(CaptureOutcome::Dictated(Box::new(note_mid_hold(
            dictated,
            mid_hold_failure,
        ))))
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
        let pill = &mut self.pill;
        deliver_outcome(
            self.engine.name(),
            outcome,
            &self.config,
            self.polisher.as_deref(),
            &*self.injector,
            &*self.notice,
            &self.history,
            |latency_ms| pill.inserted(latency_ms),
        )
    }

    /// A hold that produced no transcript, recorded against the timeline as it
    /// actually stood. Every no-transcript path inside [`App::capture`] goes
    /// through here: a blank timeline would log real audio as `audio_secs:
    /// 0.0` and make a network failure read as a broken microphone, which is
    /// exactly the false trail the 2026-08-02 investigation had to walk back.
    ///
    /// `never_connected` (see [`DictationOutcome::never_connected`]) is this
    /// method's other job: it is the one signal that distinguishes "we could
    /// not reach the transcription service" from every other no-transcript
    /// cause, and this is the only place in `iris-app` that acts on it — the
    /// session log gets a distinct, greppable field
    /// ([`DictationRecord::connection_failed`]) instead of a message a future
    /// diagnosis has to pattern-match, and the user gets told promptly through
    /// [`FailureNotice::connection_failed`], the same dialog/clipboard
    /// mechanism [`App::deliver`] already uses for a failed injection — not a
    /// second, invented channel.
    ///
    /// `failure_cause` (see [`DictationOutcome::failure_cause`]) triggers the
    /// same notice independently of `never_connected`: a batch engine like
    /// Groq can name a specific, classified reason (a rejected key, an
    /// exhausted balance, a rate limit) without ever satisfying
    /// `never_connected`'s connect-budget gate, and the user deserves the
    /// true reason either way.
    fn failed(
        &self,
        timeline: Timeline,
        message: String,
        never_connected: bool,
        failure_cause: Option<FailureCause>,
    ) -> Dictated {
        failed_outcome(
            self.engine.name(),
            &*self.notice,
            timeline,
            message,
            never_connected,
            failure_cause,
        )
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
            never_connected,
            failure_cause,
        } = outcome;
        let empty = text.trim().is_empty();
        let never_connected = never_connected && empty;
        let failure_cause = failure_cause.filter(|_| empty);
        let mut dictated = self.failed(timeline, cause, never_connected, failure_cause);
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
}

/// Clean up the transcript, falling back to the raw text on any failure.
///
/// Returns `(text, how it was polished, milliseconds spent)`. A free function
/// rather than an `App` method so [`deliver_outcome`] can call it from a
/// detached delivery thread — see [`App::capture`]'s doc comment — with a
/// cloned `polisher`/`config` instead of `&self`.
fn polish_text(
    polisher: Option<&dyn Polisher>,
    config: &Config,
    raw: &str,
) -> (String, Option<PolishInfo>, Option<f64>) {
    let Some(polisher) = polisher else {
        return (raw.to_string(), None, None);
    };
    let request = PolishRequest::new(raw).with_hints(polish::hints(config));
    match polish::run(polisher, &request) {
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

/// The shared core of [`App::deliver`]: polish, inject, stamp latency, note a
/// salvage cause. A free function, not an `App` method, so the
/// quit-during-finalize delivery thread in [`App::capture`] can call it too,
/// with cloned Arc'd resources instead of `&mut self` — `on_inserted` is
/// where the two paths actually differ, since the background one has no pill
/// left to update.
#[allow(clippy::too_many_arguments)]
fn deliver_outcome(
    engine_name: &'static str,
    outcome: DictationOutcome,
    config: &Config,
    polisher: Option<&dyn Polisher>,
    injector: &dyn Injector,
    notice: &dyn FailureNotice,
    history: &SessionLog,
    mut on_inserted: impl FnMut(u32),
) -> Dictated {
    let DictationOutcome {
        text: raw,
        mut timeline,
        cause,
        never_connected,
        failure_cause,
    } = outcome;
    // `Dictation::finish` only ever produces `never_connected: true` — or a
    // classified `failure_cause` — on the branch with nothing to salvage,
    // which returns `Err` and never reaches here; see
    // `DictationOutcome::never_connected`'s and `::failure_cause`'s docs.
    // `failed_outcome` is the path that actually has to act on either.
    debug_assert!(!never_connected, "a delivered outcome must have connected");
    debug_assert!(
        failure_cause.is_none(),
        "a delivered outcome must have connected"
    );
    let raw = raw.trim().to_string();

    let mut record = DictationRecord::now(engine_name, &raw);
    let dictated = if raw.is_empty() {
        // Silence, or a key tapped by accident. Injecting nothing is right;
        // so is not pretending to the overlay that text appeared.
        record.latency = LatencyBreakdown::from_timeline(&timeline);
        Dictated { record, timeline }
    } else {
        let (text, polished_info, polish_ms) = polish_text(polisher, config, &raw);
        record.text = text.clone();
        record.raw = (text != raw).then(|| raw.clone());
        record.polish = polished_info;

        let payload = text::prepare(&text, config.inject.trailing_space);
        match injector.inject(&payload) {
            Ok(()) => {
                timeline.mark(Mark::Injected);
                record.injected = true;
                // Key-release → text on screen: the number the pill prints.
                let latency_ms = timeline
                    .perceived()
                    .map(ms)
                    .unwrap_or(0.0)
                    .round()
                    .clamp(0.0, f64::from(u32::MAX)) as u32;
                on_inserted(latency_ms);
            }
            Err(e) => {
                // The transcript is good; only the delivery failed. Say
                // so, and make sure the record below carries the text.
                // With history off there is no file to point at, so the
                // console gets the words themselves — they must be
                // recoverable from somewhere.
                eprintln!("  could not insert the text: {e:#}");
                if history.enabled() {
                    eprintln!("  it is in {}", history.path().display());
                } else {
                    eprintln!("  it was: {text}");
                }
                // The eprintln!s above are the whole story on a launch
                // with a console attached to read them; `notice` is the
                // same failure reaching a launch with none — the
                // GUI-subsystem double-click/Start Menu/Startup path most
                // users actually take. It decides for itself whether that
                // means the clipboard, a dialog, both or neither; see
                // `crate::notify`.
                notice.injection_failed(&text, &format!("{e:#}"), history.enabled());
                record.error = Some(format!("injection failed: {e:#}"));
            }
        }

        record.latency = LatencyBreakdown::from_timeline(&timeline);
        record.latency.polish_ms = polish_ms;
        Dictated { record, timeline }
    };

    note_salvage(dictated, cause)
}

/// The shared core of [`App::failed`] — see [`deliver_outcome`] for why this
/// is a free function rather than a method.
fn failed_outcome(
    engine_name: &'static str,
    notice: &dyn FailureNotice,
    timeline: Timeline,
    message: String,
    never_connected: bool,
    failure_cause: Option<FailureCause>,
) -> Dictated {
    let mut record = DictationRecord::now(engine_name, "");
    record.error = Some(message.clone());
    record.connection_failed = never_connected;
    record.connection_cause = failure_cause.map(|cause| cause.label().to_string());
    record.latency = LatencyBreakdown::from_timeline(&timeline);
    // Either signal is reason enough to tell the user promptly: `never_connected`
    // is "no real network connection was ever reached" (see its own doc);
    // `failure_cause` is "the provider itself named a specific reason", which
    // a batch engine like Groq can report without ever satisfying
    // `never_connected`'s connect-budget gate. One dialog either way — see
    // `FailureNotice::connection_failed`'s doc for why this is not a second,
    // invented notification path.
    if never_connected || failure_cause.is_some() {
        notice.connection_failed(&message);
    }
    Dictated { record, timeline }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_candidate_tap` is private, so the exact boundary the double-tap
    /// latch hinges on is pinned here rather than through the full hold loop
    /// — a synthetic `Instant` pair is deterministic and needs no real wait,
    /// unlike driving `App::capture` end to end would.
    #[test]
    fn is_candidate_tap_holds_exactly_through_the_window_and_no_further() {
        let pressed = Instant::now();
        assert!(
            is_candidate_tap(pressed, pressed + DOUBLE_TAP_WINDOW),
            "released exactly at the window boundary should still count"
        );
        assert!(
            !is_candidate_tap(
                pressed,
                pressed + DOUBLE_TAP_WINDOW + Duration::from_millis(1)
            ),
            "one millisecond past the window must not count"
        );
        assert!(
            is_candidate_tap(pressed, pressed + Duration::from_millis(50)),
            "well inside the window"
        );
        assert!(
            !is_candidate_tap(pressed, pressed + Duration::from_secs(2)),
            "well outside the window — an ordinary hold-to-talk release"
        );
        // A release at the exact same instant as the press (the fastest a
        // real tap can be) is trivially inside the window.
        assert!(is_candidate_tap(pressed, pressed));
    }
}

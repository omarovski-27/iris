//! Portable window state: which tab is showing, the config/history snapshot
//! in memory, and how a change gets back to the dictation loop. No `egui`
//! here — see `crate::window::ui` for the view this drives.
//!
//! # How a setting change reaches the loop
//!
//! The window never writes `config.toml` itself. Every setter below sends the
//! same [`Command`] the tray already sends for that field (`SetEngine`,
//! `SetDevice`, `SetTheme`, `SetPolish`) or, for the two fields new to this
//! window (`SetHotkey`, `SetOverlayEnabled`), a new variant that follows the
//! same shape. [`crate::App`] stays the sole writer of the config file, so a
//! setting changed here and one changed from the tray a moment later can
//! never race to overwrite each other. [`WindowState::refresh`] re-reads the
//! file and the session log periodically, so external changes — the tray, a
//! hand edit — show up here too, within one refresh interval; see the tray's
//! own "known limitations" for the same accepted trade-off.
//!
//! Sending is not applying. `App` can decline `SetEngine` (no API key) or
//! `SetDevice` (a microphone that will not open) and keep what works, so each
//! command is held in [`WindowState::inflight`] until a [`CommandOutcome`]
//! comes back on `Env::outcomes`, and only then does the control move and the
//! status line say "Saved". A rejected change says why instead, in the words
//! `App` already had — see [`WindowState::poll_outcomes`].

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, TryRecvError};

use crate::app::{flip_quit_flag, Command, CommandId, CommandOutcome};
use crate::config::{Config, EngineChoice, Theme};
use crate::history::{DictationRecord, SessionLog};
use iris_core::hotkey::Key;

use super::insights::{DayWindow, Insights};
use super::search;

/// How often the window re-reads `config.toml` and the session log while it
/// is open and visible. Also the slowest anything on screen can change, and
/// therefore what `crate::window::ui::draw_root` paces its repaints to.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// How long [`WindowState::status`] stays on screen after [`WindowState::flash`].
pub const STATUS_HOLD: Duration = Duration::from_secs(3);

/// How many History cards the tab lays out before it stops and offers the
/// rest a page at a time. See [`WindowState::visible_count`].
pub const HISTORY_PAGE: usize = 100;

/// Which section is showing, in the priority order the product brief gives
/// them: History first, because it is the recovery path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// The session log, newest first, searchable, copyable.
    History,
    /// Everything `Config` holds.
    Settings,
    /// Statistics computed from the session log.
    Insights,
}

impl Tab {
    /// Every tab, in display order.
    pub const ALL: [Tab; 3] = [Tab::History, Tab::Settings, Tab::Insights];

    /// The nav label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Tab::History => "History",
            Tab::Settings => "Settings",
            Tab::Insights => "Insights",
        }
    }
}

/// Everything the window needs from outside itself.
///
/// `list_devices`, `open_config_file` and `utc_offset_seconds` arrive as
/// callbacks/values rather than direct calls because each is a Windows-only
/// operation (WASAPI, `cmd /C start`, `GetTimeZoneInformation`) and this
/// module has to stay portable; see `crate::window::shell` for the real ones.
pub struct Env<'a> {
    /// Where `config.toml` lives.
    pub config_path: &'a Path,
    /// The channel [`crate::App::run`] also drains tray commands from. Each
    /// command goes out under a [`CommandId`] the answer comes back with.
    pub commands: &'a Sender<(CommandId, Command)>,
    /// What the loop did with each of those commands, one per command sent
    /// and tagged with its id. Outlives any one window, so it can carry an
    /// answer this one never asked for. See [`WindowState::poll_outcomes`].
    pub outcomes: &'a Receiver<(CommandId, CommandOutcome)>,
    /// Enumerate input devices. Empty on a platform/build with no capture.
    pub list_devices: &'a dyn Fn() -> Vec<String>,
    /// Hand `config.toml` to whatever the desktop opens `.toml` with. The one
    /// route to the API keys, which this window deliberately never renders —
    /// see `crate::window::ui::settings_tab`'s module docs.
    pub open_config_file: &'a dyn Fn(&Path) -> anyhow::Result<()>,
    /// Fires when [`crate::window::WindowSink::open`] is called again while
    /// this window is already showing — `ui::draw_root` drains it and asks
    /// the OS to focus the window instead of opening a second one.
    pub reopen_signal: &'a Receiver<()>,
    /// The local UTC offset, for [`DayWindow`] and the History timestamps.
    /// Zero (i.e. UTC) wherever the OS cannot be asked.
    pub utc_offset_seconds: i32,
    /// The push-to-talk key, running value and launch-time file value. The
    /// window names the running one: `App` deliberately leaves the installed
    /// hook alone until a restart, so `config.hotkey` alone would tell the
    /// user to hold a key that does nothing.
    pub hotkey: InForce<Key>,
    /// Whether the overlay is really up this run, on the same footing.
    pub overlay_enabled: InForce<bool>,
    /// The same flag `tray::spawn` flips on Quit — shared via [`Arc`] rather
    /// than borrowed so this window's own thread can hold it for the life of
    /// the process, the same way `tray::spawn` does. See
    /// [`Env::request_quit`].
    pub quit_flag: Arc<AtomicBool>,
    /// Show a native notice carrying `(title, message)`. The one caller is
    /// [`WindowState::note_hidden_to_tray`]; kept as a callback, like
    /// [`Env::open_config_file`], because showing it is a Windows-only
    /// operation and this module has to stay portable.
    pub show_close_hint: &'a dyn Fn(&str, &str),
}

impl Env<'_> {
    /// Which restart-gated settings a restart would actually change. See
    /// [`InForce::pending`], which is where the comparison lives.
    #[must_use]
    pub fn restart_pending(&self, saved: &Config) -> RestartPending {
        RestartPending {
            hotkey: self.hotkey.pending(&saved.hotkey),
            overlay_enabled: self.overlay_enabled.pending(&saved.overlay_enabled),
        }
    }

    /// Flip [`Env::quit_flag`] and hand the loop `Command::Quit` — the exact
    /// sequence the tray's Quit item already performs
    /// (`crate::app::flip_quit_flag`'s doc comment has the ordering rule),
    /// so the app ends the same way whichever one sent it. Not tracked in
    /// [`WindowState::inflight`] like an ordinary setting: `App::apply`
    /// answers `Quit` on neither channel, so there is no outcome to poll for.
    ///
    /// The one caller is `super::ui::draw_root`, once per frame it sees the
    /// window's close requested — the single signal behind the `X`, Alt+F4,
    /// and the taskbar's "Close window", so all three end the whole app
    /// exactly as the tray already does, instead of only hiding this one
    /// window.
    pub fn request_quit(&self) {
        flip_quit_flag(&self.quit_flag);
        let _ = self.commands.send((CommandId::next(), Command::Quit));
    }
}

/// The one-time hint's title and body — named constants so
/// [`WindowState::note_hidden_to_tray`] and its test say the same thing.
pub const CLOSE_HINT_TITLE: &str = "Iris is still running";
/// See [`CLOSE_HINT_TITLE`].
pub const CLOSE_HINT_MESSAGE: &str =
    "Closing this window keeps Iris running in the background. Look for the \
     prism icon in the system tray to reopen Settings or to Quit Iris.";

/// One restart-gated setting in the two forms the window has to tell apart:
/// what this process is really running on, and what `config.toml` held when
/// it launched.
///
/// Both are load-bearing, because neither answers "is a restart owed?" alone.
/// *Saved differs from running* is true all session under a run-only CLI
/// override (`iris --hotkey f9` over a `hotkey = "right-ctrl"` file) with
/// nothing pending. *The file moved since launch* is true the moment the user
/// picks, from the Settings picker, the very value that override already put
/// in force — where a restart would change nothing. Only the two together
/// mean a restart is owed, and that is [`InForce::pending`].
///
/// Everything the view says about these settings is derived here, from
/// [`InForce::running`], rather than from the launch-time file value: that is
/// what keeps the sidebar hint, the pending markers and the overlay's
/// "not running" disclosure from drifting apart again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InForce<T> {
    /// What this process actually uses. Read once in `main`, before this
    /// window could exist, and unchanged until a restart.
    pub running: T,
    /// What `config.toml` held for it at launch, before any CLI override.
    pub at_startup: T,
}

impl<T: PartialEq> InForce<T> {
    /// Whether the file value as it stands *now* is not what is running.
    ///
    /// True for a change waiting on a restart, and also for a divergence no
    /// restart caused: a run-only CLI override, or an overlay that was asked
    /// for and failed to start.
    #[must_use]
    pub fn diverged(&self, saved: &T) -> bool {
        *saved != self.running
    }

    /// Whether restarting would actually change how Iris behaves: the file
    /// has moved since launch *and* what it now holds is not already live.
    #[must_use]
    pub fn pending(&self, saved: &T) -> bool {
        *saved != self.at_startup && self.diverged(saved)
    }
}

/// Which restart-gated settings have been changed since launch, for the view
/// to mark. See [`Env::restart_pending`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPending {
    /// `hotkey` has been rebound and is waiting on a restart.
    pub hotkey: bool,
    /// `overlay_enabled` has been toggled and is waiting on a restart.
    pub overlay_enabled: bool,
}

/// How a flashed [`Status`] reads: ordinary feedback, or a failure the user
/// has to notice because the change did *not* happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    /// "Saved", "Copied" — the thing worked.
    Info,
    /// The thing did not work; the view paints this in the warning colour.
    Warn,
}

/// A transient message (e.g. "Copied", "Saved") and when it was set, so the
/// view can drop it after `STATUS_HOLD`.
#[derive(Debug, Clone)]
pub struct Status {
    /// What to show.
    pub message: String,
    /// How to show it.
    pub level: StatusLevel,
    /// When [`WindowState::flash`] set it.
    pub at: Instant,
}

/// A [`Command`] the loop has been sent and has not answered yet.
///
/// It carries the change itself, not a copy of the new value, so the one
/// place that knows what a command means to [`WindowState::config`] is
/// [`WindowState::absorb`] — and it only runs once the loop says the change
/// happened.
struct Inflight {
    /// What the loop will tag its answer with. See
    /// [`WindowState::poll_outcomes`].
    id: CommandId,
    command: Command,
    /// What to flash if the loop takes it.
    message: String,
}

/// The window's in-memory state. Built fresh each time the window opens.
pub struct WindowState {
    /// The active section.
    pub tab: Tab,
    /// The History tab's search box contents.
    pub search: String,
    /// The settings snapshot the Settings tab reads and mutates. Written
    /// through [`Command`]s, never saved to disk directly — see the module
    /// docs.
    pub config: Config,
    /// The session log, newest first.
    pub history: Vec<DictationRecord>,
    /// Rolled up from `history` whenever it is (re)loaded.
    pub insights: Insights,
    /// Input device names for the Settings picker.
    pub devices: Vec<String>,
    /// The current status line, if any. See [`WindowState::status_flash`].
    pub status: Option<Status>,
    last_refresh: Instant,
    /// Size and mtime of the session log as `history` was last read from it.
    /// See [`WindowState::refresh`].
    history_stamp: Option<(u64, Option<SystemTime>)>,
    /// Whether the last attempt to read the session log failed, and so whether
    /// the user has already been told about the failure now in progress. The
    /// read is retried every refresh; the warning is one per episode. See
    /// [`WindowState::refresh`].
    history_read_failing: bool,
    /// The local calendar day `insights` counts "today" as. Held because it
    /// moves on its own clock: a window left open across local midnight has
    /// to recount, with the log untouched. See [`WindowState::refresh`].
    insights_day: DayWindow,
    /// Commands sent to the loop that have not been answered yet, oldest
    /// first. See [`WindowState::poll_outcomes`].
    inflight: VecDeque<Inflight>,
    /// Positions in `history` matching `search`, as of `filtered_for`. See
    /// [`WindowState::sync_filter`].
    filtered: Vec<usize>,
    /// The query `filtered` was computed for; `None` while it is stale, which
    /// is how a reloaded `history` invalidates it.
    filtered_for: Option<String>,
    /// How many matches the History tab is currently drawing. See
    /// [`WindowState::visible_count`].
    visible: usize,
    /// The Settings tab's vocabulary text box: one term per line, edited
    /// freely before being sent as a [`Command::SetVocabulary`]. See
    /// [`WindowState::sync_vocabulary_input`] for how this stays in step
    /// with `config.vocabulary` without clobbering an edit in progress.
    pub vocabulary_input: String,
    /// The vocabulary list `vocabulary_input` was last derived from — not
    /// necessarily saved, just the last value the two agreed on. See
    /// [`WindowState::sync_vocabulary_input`].
    vocabulary_input_synced_with: Vec<String>,
}

impl WindowState {
    /// Load everything fresh: config, history, devices.
    ///
    /// A config that fails to parse is the one case [`WindowState::refresh`]
    /// cannot be matched exactly: there is no last good snapshot yet to keep,
    /// so the form has to fall back to defaults. It says so rather than
    /// showing them as if they were the file's — a window opened on stock
    /// values with nothing on screen to explain it reads as settings lost.
    /// A missing file is not that case; [`Config::load`] reports it as the
    /// defaults it is.
    ///
    /// A session log that cannot be read is the same story on the tab that
    /// opens first: an empty History is what "nothing recorded yet" looks
    /// like too, so a read failure has to say so rather than pass for one.
    #[must_use]
    pub fn new(env: &Env) -> Self {
        let (config, unreadable) = match Config::load(env.config_path) {
            Ok(config) => (config, None),
            Err(e) => (
                Config::default(),
                Some(format!(
                    "Could not read config.toml — showing defaults: {e:#}"
                )),
            ),
        };
        let history_path = config.history_path(env.config_path);
        let stamp = history_stamp(&history_path);
        // Same invariant `refresh` enforces: the stamp moves only on a read
        // that produced records, so a log that could be stat-ed but not read
        // is retried on the next tick rather than passing for one this window
        // has already seen.
        let (history, history_stamp, unreadable_history) = match load_history(&history_path) {
            Ok(history) => (history, stamp, None),
            Err(e) => (Vec::new(), None, Some(unreadable_log(&e))),
        };
        let insights_day = local_day(env.utc_offset_seconds);
        let insights = Insights::compute(&history, &insights_day);
        let devices = (env.list_devices)();
        let vocabulary_input = config.vocabulary.join("\n");
        let vocabulary_input_synced_with = config.vocabulary.clone();
        let mut state = Self {
            tab: Tab::History,
            search: String::new(),
            config,
            history,
            insights,
            devices,
            status: None,
            last_refresh: Instant::now(),
            history_stamp,
            history_read_failing: unreadable_history.is_some(),
            insights_day,
            inflight: VecDeque::new(),
            filtered: Vec::new(),
            filtered_for: None,
            visible: HISTORY_PAGE,
            vocabulary_input,
            vocabulary_input_synced_with,
        };
        // History last: it is the tab that opens, so when both files are
        // unreadable its failure is the one the user needs on screen.
        if let Some(message) = unreadable {
            state.flash_failure(message);
        }
        if let Some(message) = unreadable_history {
            state.flash_failure(message);
        }
        state
    }

    /// Re-read `config.toml` and the session log if `REFRESH_INTERVAL` has
    /// elapsed since the last read, or unconditionally when `force` (a
    /// manual refresh).
    ///
    /// Rereading the log is the expensive half — every record is parsed, then
    /// retokenised into [`Insights`]' n-gram maps, over a log whose cap the
    /// user sets — and most refreshes have nothing to read: dictations are
    /// minutes apart, this fires every two seconds. So a cheap `stat` of the
    /// log gates it, and only the config snapshot is taken every time (it is
    /// one small file, and the tray can move it at any moment). `force` reads
    /// regardless, so the Refresh button always means what it says.
    ///
    /// [`Insights`] is recomputed whenever *either* input moved, and the local
    /// day is an input: "Dictations today" is counted against a window that
    /// rolls over at local midnight whether or not anyone dictates, so a
    /// window left open overnight has to recount over the log it already has.
    pub fn refresh(&mut self, env: &Env, force: bool) {
        if !force && self.last_refresh.elapsed() < REFRESH_INTERVAL {
            return;
        }
        self.last_refresh = Instant::now();
        // A config that fails to parse (mid-write, or hand-edited badly)
        // keeps the last good snapshot rather than resetting the form to
        // defaults out from under the user.
        if let Ok(config) = Config::load(env.config_path) {
            self.config = config;
        }
        let history_path = self.config.history_path(env.config_path);
        let stamp = history_stamp(&history_path);
        let reload = force || stamp != self.history_stamp;
        let day = local_day(env.utc_offset_seconds);
        if !reload && day == self.insights_day {
            return;
        }
        if reload {
            // A log that cannot be read keeps the records already on screen —
            // the same last-good-snapshot rule the config gets above — and
            // says why, because a silently frozen recovery path is worse than
            // a stale one the user knows about. The stamp moves only on a read
            // that produced records: a failure is not a state this window has
            // seen, so keeping the last good one is what makes the next
            // refresh try again instead of going quietly stale once the
            // warning has expired.
            //
            // The retry is every refresh; the warning is once per episode.
            // Saying it again every two seconds would leave no other status —
            // "Copied", "Saved" — on screen for longer than that, and this is
            // the same failure the user was already told about. `force` is the
            // exception the rate-limit is not for: the Refresh button is the
            // user asking for this read, and a click that changes nothing on
            // screen and says nothing is a dead control.
            match load_history(&history_path) {
                Ok(history) => {
                    self.history = history;
                    self.filtered_for = None;
                    self.history_stamp = stamp;
                    self.history_read_failing = false;
                }
                Err(e) => {
                    if force || !self.history_read_failing {
                        self.flash_failure(unreadable_log(&e));
                    }
                    self.history_read_failing = true;
                }
            }
        }
        self.insights_day = day;
        self.insights = Insights::compute(&self.history, &self.insights_day);
    }

    /// Bring [`WindowState::filtered`] up to date with `search` and `history`,
    /// recomputing only when one of them has actually moved.
    ///
    /// The History tab calls this once per frame, and matching is not free:
    /// it lowercases every record it looks at, over a log whose cap
    /// (`history.max_entries`) the user sets. Recomputing that on every
    /// repaint made the window heavier the longer the log got — worst while
    /// typing in the search box, which is when it repaints continuously.
    pub fn sync_filter(&mut self) {
        if self.filtered_for.as_deref() == Some(self.search.as_str()) {
            return;
        }
        self.filtered = search::filter_indices(&self.history, &self.search);
        self.filtered_for = Some(self.search.clone());
    }

    /// Positions in [`WindowState::history`] matching the search box, as of
    /// the last [`WindowState::sync_filter`].
    #[must_use]
    pub fn filtered(&self) -> &[usize] {
        &self.filtered
    }

    /// How many of [`WindowState::filtered`] the History tab draws.
    ///
    /// A card is not cheap — a frame, a shadow, two chips, a painted marker
    /// and a wrapped body — and `egui` lays out every one it is handed, on
    /// every repaint, whether or not it is on screen. `egui`'s own row
    /// virtualisation needs a uniform row height, which these cards do not
    /// have (the body wraps to as many lines as the transcript takes), so
    /// what bounds the work here is the count: a page at a time, with the
    /// rest one click away, instead of all `history.max_entries` of them
    /// every frame the user types a character into the search box.
    #[must_use]
    pub fn visible_count(&self) -> usize {
        self.visible.min(self.filtered.len())
    }

    /// Draw one more page of matches.
    pub fn show_more(&mut self) {
        self.visible = self.visible.saturating_add(HISTORY_PAGE);
    }

    /// Go back to the first page — for a new search, whose results the user
    /// has not asked to see beyond.
    pub fn reset_paging(&mut self) {
        self.visible = HISTORY_PAGE;
    }

    /// Re-enumerate input devices (a manual "refresh" next to the picker).
    pub fn refresh_devices(&mut self, env: &Env) {
        self.devices = (env.list_devices)();
    }

    /// Open `config.toml` in whatever the desktop uses for it — the only
    /// route this window offers to the API keys, which it never renders. A
    /// pure OS action: nothing is read back, so no [`Command`] is sent and
    /// the next [`WindowState::refresh`] picks up a hand edit like any other.
    pub fn open_config_file(&mut self, env: &Env) {
        match (env.open_config_file)(env.config_path) {
            Ok(()) => self.flash("Opened config.toml"),
            Err(e) => self.flash_failure(format!("Could not open config.toml: {e:#}")),
        }
    }

    /// Show `message` in the status line for `STATUS_HOLD`.
    pub fn flash(&mut self, message: impl Into<String>) {
        self.set_status(message, StatusLevel::Info);
    }

    /// Like [`WindowState::flash`], for something that did not work.
    pub fn flash_failure(&mut self, message: impl Into<String>) {
        self.set_status(message, StatusLevel::Warn);
    }

    fn set_status(&mut self, message: impl Into<String>, level: StatusLevel) {
        self.status = Some(Status {
            message: message.into(),
            level,
            at: Instant::now(),
        });
    }

    /// The status message and how to paint it, if it has not yet expired.
    ///
    /// `STATUS_HOLD` does not run while the loop still owes an answer
    /// ([`WindowState::awaiting_loop`]). `App::run` drains commands between
    /// dictations, so a change clicked during one can wait longer than the
    /// hold; dropping "Saving…" while the control has not moved either would
    /// leave nothing on screen saying the click landed.
    #[must_use]
    pub fn status_flash(&self) -> Option<(&str, StatusLevel)> {
        self.status
            .as_ref()
            .filter(|status| self.awaiting_loop() || status.at.elapsed() < STATUS_HOLD)
            .map(|status| (status.message.as_str(), status.level))
    }

    /// The status message, if it has not yet expired.
    #[must_use]
    pub fn status_text(&self) -> Option<&str> {
        self.status_flash().map(|(message, _)| message)
    }

    /// Switch the transcription engine. Applied by the loop, which declines it
    /// and keeps the working engine if the new one cannot be built (no API
    /// key) — so the picker only moves once the loop says it did.
    pub fn set_engine(&mut self, env: &Env, choice: EngineChoice) {
        self.dispatch(env, Command::SetEngine(choice), "Saved");
    }

    /// Switch the input device. Applied by the loop, which declines it and
    /// keeps the working microphone if the new one will not open.
    pub fn set_device(&mut self, env: &Env, device: Option<String>) {
        self.dispatch(env, Command::SetDevice(device), "Saved");
    }

    /// Switch dark/light. Takes effect immediately, on this window too.
    pub fn set_theme(&mut self, env: &Env, theme: Theme) {
        self.dispatch(env, Command::SetTheme(theme), "Saved");
    }

    /// Turn transcript cleanup on or off. Takes effect immediately.
    pub fn set_polish(&mut self, env: &Env, enabled: bool) {
        self.dispatch(env, Command::SetPolish(enabled), "Saved");
    }

    /// Rebind the push-to-talk key. Saved now; needs a restart to take
    /// effect, because the hook is installed once in `main` before this
    /// window exists — the same restart the tray's own hotkey change needs.
    pub fn set_hotkey(&mut self, env: &Env, key: Key) {
        self.dispatch(
            env,
            Command::SetHotkey(key),
            "Saved — restart Iris to use the new key",
        );
    }

    /// Show or hide the pill overlay. Saved now; needs a restart —
    /// the overlay is spawned once in `main` before this window exists.
    pub fn set_overlay_enabled(&mut self, env: &Env, enabled: bool) {
        self.dispatch(
            env,
            Command::SetOverlayEnabled(enabled),
            "Saved — restart Iris for this to take effect",
        );
    }

    /// Parse [`WindowState::vocabulary_input`] into the list [`Command::SetVocabulary`]
    /// would send: one term per line, blank lines and surrounding whitespace
    /// dropped so the box's own formatting never reaches the engine or the
    /// saved file. The engines each apply their own provider-specific limit
    /// on top of this — see `iris_core::engine::EngineOptions::vocabulary`.
    #[must_use]
    pub fn parsed_vocabulary(&self) -> Vec<String> {
        self.vocabulary_input
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Whether the vocabulary box holds an edit that has not been saved —
    /// what the Save button in the Vocabulary section is gated on.
    #[must_use]
    pub fn vocabulary_dirty(&self) -> bool {
        self.parsed_vocabulary() != self.config.vocabulary
    }

    /// Keep [`WindowState::vocabulary_input`] in step with `config.vocabulary`
    /// without clobbering an edit in progress.
    ///
    /// `config` is reloaded from disk every [`REFRESH_INTERVAL`] (see
    /// [`WindowState::refresh`]) whether or not anything changed, so this
    /// cannot simply copy `config.vocabulary` into the box on every call —
    /// that would erase whatever the user is mid-typing every two seconds.
    /// Instead it only resyncs when `config.vocabulary` has actually moved
    /// since the last time the two matched: on open, after
    /// [`WindowState::set_vocabulary`] is confirmed applied (see
    /// [`WindowState::absorb`]), or if `config.toml` is edited by hand while
    /// the window is open. Called once a frame by the Settings tab, the same
    /// way [`WindowState::sync_filter`] keeps the History search in step.
    pub fn sync_vocabulary_input(&mut self) {
        if self.vocabulary_input_synced_with != self.config.vocabulary {
            self.vocabulary_input = self.config.vocabulary.join("\n");
            self.vocabulary_input_synced_with = self.config.vocabulary.clone();
        }
    }

    /// Send [`WindowState::parsed_vocabulary`] to the loop. Applied by the
    /// loop, which rebuilds the active engine so the new list reaches it
    /// immediately — unlike the hotkey and overlay toggle, this is not read
    /// once at startup and needs no restart.
    pub fn set_vocabulary(&mut self, env: &Env) {
        let terms = self.parsed_vocabulary();
        self.dispatch(env, Command::SetVocabulary(terms), "Saved");
    }

    /// Called once, the first time the window's close hides it rather than
    /// quitting the app (see `crate::window::ui::draw_root`'s doc comment for
    /// why closing no longer quits): shows the "still running in the tray"
    /// hint and remembers that it has been shown, so it never shows again.
    ///
    /// Not routed through [`WindowState::dispatch`] like an ordinary
    /// setting — the same reasoning as [`Env::request_quit`]: the window is
    /// disappearing on this same frame, so there is no picker to hold back
    /// and no status line worth flashing, and there is no answer worth
    /// polling for. `self.config.tray_close_hint_shown` is flipped locally
    /// before the command that persists it is even sent, so a second close
    /// later in the same run — before the file round-trip completes — still
    /// reads as "already shown" rather than firing twice.
    pub fn note_hidden_to_tray(&mut self, env: &Env) {
        if self.config.tray_close_hint_shown {
            return;
        }
        self.config.tray_close_hint_shown = true;
        (env.show_close_hint)(CLOSE_HINT_TITLE, CLOSE_HINT_MESSAGE);
        let _ = env
            .commands
            .send((CommandId::next(), Command::AcknowledgeTrayHint));
    }

    /// Hand `command` to the loop and wait to be told what became of it.
    ///
    /// Nothing here claims success: a queued command is not an applied one,
    /// and [`crate::App`] is both the sole writer of the file and the only
    /// place that can tell whether the change is possible. So the command
    /// waits in [`WindowState::inflight`] and [`WindowState::poll_outcomes`]
    /// finishes the job. A send that cannot even be delivered is the one
    /// failure this end can diagnose itself: the loop has already returned and
    /// this window is outliving it by a moment.
    fn dispatch(&mut self, env: &Env, command: Command, saved: &str) {
        let id = CommandId::next();
        match env.commands.send((id, command.clone())) {
            Ok(()) => {
                self.inflight.push_back(Inflight {
                    id,
                    command,
                    message: saved.to_string(),
                });
                self.flash("Saving…");
            }
            Err(_) => self.flash_failure(NOT_RUNNING),
        }
    }

    /// Take in whatever the loop has said about the commands sent so far.
    ///
    /// Called once a frame, before anything is drawn. One outcome answers the
    /// one command carrying its [`CommandId`] — which is what lets a rejection
    /// be reported against the change that was actually refused, with the
    /// loop's own reason for it.
    ///
    /// Pairing on the id rather than on arrival order is what keeps that true
    /// across window lifetimes. The channel is one long-lived pair the whole
    /// process shares while this state is rebuilt on every open, so a change
    /// still in flight when the user closes the window is answered into a
    /// window that never sent it: an id no one here is waiting for is a
    /// stranger's answer and is dropped. Ids only ever grow, so anything still
    /// queued ahead of the one just answered belongs to a window that is gone
    /// too, and goes with it rather than holding `awaiting_loop` open forever.
    pub fn poll_outcomes(&mut self, env: &Env) {
        loop {
            match env.outcomes.try_recv() {
                Ok((id, outcome)) => {
                    let Some(at) = self.inflight.iter().position(|p| p.id == id) else {
                        continue;
                    };
                    let Some(pending) = self.inflight.drain(..=at).next_back() else {
                        continue;
                    };
                    match outcome {
                        CommandOutcome::Applied => {
                            self.absorb(&pending.command);
                            self.flash(pending.message);
                        }
                        CommandOutcome::Rejected(reason) => {
                            self.flash_failure(format!("Not saved — {reason}"));
                        }
                    }
                }
                Err(TryRecvError::Empty) => return,
                // The loop has returned. Anything still waiting will never be
                // answered, and saying so beats a status line stuck on
                // "Saving…" over a change that is not going to happen.
                Err(TryRecvError::Disconnected) => {
                    if !self.inflight.is_empty() {
                        self.inflight.clear();
                        self.flash_failure(NOT_RUNNING);
                    }
                    return;
                }
            }
        }
    }

    /// Whether the loop still owes an answer, which is the one thing on screen
    /// that can change faster than [`REFRESH_INTERVAL`]. See
    /// `crate::window::ui::draw_root`.
    #[must_use]
    pub fn awaiting_loop(&self) -> bool {
        !self.inflight.is_empty()
    }

    /// Move [`WindowState::config`] onto a change the loop has confirmed.
    ///
    /// The next [`WindowState::refresh`] would read the same value back from
    /// the file; this only means the control stops lagging a click by up to a
    /// refresh interval.
    ///
    /// Which field each command stands for is [`Command::apply_to`]'s to know,
    /// not this window's — the commands that change no field (nothing this
    /// window sends anyway) simply do nothing here.
    fn absorb(&mut self, command: &Command) {
        command.apply_to(&mut self.config);
    }
}

/// What the status line says when the dictation loop is gone: the window can
/// outlive it by a moment, and nothing it asks for will be saved after that.
const NOT_RUNNING: &str = "Not saved — Iris is no longer running. Restart Iris.";

/// The session log, newest first.
///
/// The error is passed on rather than flattened to an empty log: a log that
/// is simply not there yet already reads as `Ok(vec![])`
/// (`crate::history::read_lines` maps `NotFound` to no lines), so anything
/// that reaches here as an `Err` is a real failure to read the one file this
/// window exists to show.
fn load_history(path: &Path) -> anyhow::Result<Vec<DictationRecord>> {
    let mut records = SessionLog::read_all(path)?;
    records.reverse(); // newest first: the recovery path reads top-down
    Ok(records)
}

/// What the status line says when the session log will not open.
fn unreadable_log(e: &anyhow::Error) -> String {
    format!("Could not read the session log: {e:#}")
}

/// A cheap fingerprint of the session log: its size and, where the platform
/// reports one, its modification time.
///
/// Size alone already moves on every append, which is the only way the log
/// grows; the mtime covers a rewrite that happens to land on the same length
/// (`SessionLog` trimming to `history.max_entries`). `None` means there is no
/// log yet — a state that compares equal to itself, so an absent log costs
/// nothing to keep checking.
fn history_stamp(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

/// The UTC range covering the *local* calendar day that is current now, given
/// a fixed offset east of UTC in seconds.
///
/// Pure arithmetic on a supplied offset, never a timezone lookup: reading the
/// current local offset from a multi-threaded process is unsound, which is why
/// `crate::history` stamps in UTC in the first place. The offset comes from
/// `crate::window::shell`, which asks Windows once; everything after it is
/// this function.
fn local_day(utc_offset_seconds: i32) -> DayWindow {
    let offset = time::Duration::seconds(i64::from(utc_offset_seconds));
    let local_midnight =
        (time::OffsetDateTime::now_utc() + offset).replace_time(time::Time::MIDNIGHT);
    let start = local_midnight - offset;
    DayWindow::new(
        utc_seconds(start),
        utc_seconds(start + time::Duration::days(1)),
    )
}

/// An instant as `"YYYY-MM-DDTHH:MM:SS"` — RFC 3339 without the zone or any
/// fractional part, the form [`DayWindow`] compares.
fn utc_seconds(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
        .get(..19)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::DictationRecord;

    /// Never actually launches anything: no test may spawn a process onto the
    /// user's desktop, the same rule injection lives under. Fails in the shape
    /// the real one does — a context over the underlying spawn error — so what
    /// the status line makes of a chain is testable.
    fn refuse_to_open(_path: &Path) -> anyhow::Result<()> {
        use anyhow::Context as _;
        Err(anyhow::anyhow!("no desktop in a test")).context("launching the editor")
    }

    /// Never pops a real notice — no test may depend on or exercise the real
    /// mechanism, the same rule `refuse_to_open` follows for the editor.
    fn no_close_hint(_title: &str, _message: &str) {}

    fn env_with<'a>(
        config_path: &'a Path,
        commands: &'a Sender<(CommandId, Command)>,
        outcomes: &'a Receiver<(CommandId, CommandOutcome)>,
        devices: &'a dyn Fn() -> Vec<String>,
        reopen_signal: &'a Receiver<()>,
    ) -> Env<'a> {
        Env {
            config_path,
            commands,
            outcomes,
            list_devices: devices,
            open_config_file: &refuse_to_open,
            reopen_signal,
            utc_offset_seconds: 0,
            hotkey: InForce {
                running: Key::default(),
                at_startup: Key::default(),
            },
            overlay_enabled: InForce {
                running: true,
                at_startup: true,
            },
            quit_flag: Arc::new(AtomicBool::new(false)),
            show_close_hint: &no_close_hint,
        }
    }

    /// A `reopen_signal` for tests that never fire it.
    fn no_reopen() -> Receiver<()> {
        crossbeam_channel::never()
    }

    /// An outcome channel for tests that never make the loop answer. `never()`
    /// rather than a dropped sender: a disconnected one is itself an answer
    /// ("the loop is gone"), which these tests are not asking about.
    fn no_outcomes() -> Receiver<(CommandId, CommandOutcome)> {
        crossbeam_channel::never()
    }

    /// Play the loop for everything the window has sent: take it all, the way
    /// `App::apply` does when nothing objects, then let the window notice —
    /// which is what `ui::draw_root` does once a frame.
    fn take_everything(
        state: &mut WindowState,
        env: &Env,
        outcomes: &Sender<(CommandId, CommandOutcome)>,
    ) {
        for id in state.inflight.iter().map(|p| p.id).collect::<Vec<_>>() {
            outcomes.send((id, CommandOutcome::Applied)).unwrap();
        }
        state.poll_outcomes(env);
    }

    /// The id the loop would answer the window's `nth`-oldest unanswered
    /// command under.
    fn inflight_id(state: &WindowState, nth: usize) -> CommandId {
        state.inflight[nth].id
    }

    #[test]
    fn new_loads_defaults_when_nothing_is_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);

        let state = WindowState::new(&env);
        assert_eq!(state.tab, Tab::History);
        assert_eq!(state.config, Config::default());
        assert!(state.history.is_empty());
        assert_eq!(state.insights.total_dictations, 0);
        assert_eq!(
            state.status_flash(),
            None,
            "a file that is merely absent is the defaults, not a failure"
        );
    }

    /// A `config.toml` that will not parse — a bad hand edit — cannot be
    /// shown, so the form falls back to defaults. It may not do so in
    /// silence: stock values on screen with nothing to explain them read as
    /// settings lost. `refresh` answers the same question by keeping the last
    /// good snapshot, which does not exist yet at this point.
    #[test]
    fn a_config_that_cannot_be_read_says_so_rather_than_defaulting_in_silence() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "engine = \"deepgram").unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);

        let state = WindowState::new(&env);
        assert_eq!(state.config, Config::default());
        let (message, level) = state.status_flash().unwrap();
        assert!(message.contains("Could not read config.toml"), "{message}");
        assert_eq!(level, StatusLevel::Warn);
        // The whole cause chain, not just `anyhow`'s outermost context: what
        // is wrong with the file is the only part of this the user can act on.
        assert!(
            message.contains("parsing the Iris configuration"),
            "{message}"
        );
    }

    #[test]
    fn new_loads_history_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let mut log = SessionLog::open(dir.path().join("history.jsonl"), 10);
        log.append(&DictationRecord::now("mock", "first")).unwrap();
        log.append(&DictationRecord::now("mock", "second")).unwrap();

        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let state = WindowState::new(&env);

        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0].text, "second");
        assert_eq!(state.history[1].text, "first");
    }

    /// An empty History is exactly what a log that has never been written
    /// looks like, so a log that cannot be *read* must not pass for one: the
    /// tab the window opens on would otherwise say the user has never
    /// dictated.
    #[test]
    fn a_session_log_that_cannot_be_read_says_so_rather_than_looking_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut config = Config::default();
        // The parent is a file, so opening the log fails with something other
        // than "not found" — which `SessionLog::read_all` reports as an empty
        // log, and rightly.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "").unwrap();
        config.history.path = Some(blocker.join("history.jsonl"));
        config.save(&config_path).unwrap();

        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let state = WindowState::new(&env);

        assert!(state.history.is_empty());
        let (message, level) = state.status_flash().unwrap();
        assert!(
            message.contains("Could not read the session log"),
            "{message}"
        );
        assert_eq!(level, StatusLevel::Warn);
    }

    /// The same failure on a refresh keeps what is already on screen: a
    /// recovery path that empties itself because one read failed is worse
    /// than a stale one the user is told about.
    #[test]
    fn a_refresh_that_cannot_read_the_log_keeps_the_records_it_has() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let nest = dir.path().join("nest");
        let log_path = nest.join("history.jsonl");
        let mut config = Config::default();
        config.history.path = Some(log_path.clone());
        config.save(&config_path).unwrap();
        let mut log = SessionLog::open(log_path.clone(), 10);
        log.append(&DictationRecord::now("mock", "first")).unwrap();

        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);
        assert_eq!(state.history.len(), 1);

        // The log's own directory becomes a file, so the next read of the
        // same path fails with something other than "not found".
        std::fs::remove_file(&log_path).unwrap();
        std::fs::remove_dir(&nest).unwrap();
        std::fs::write(&nest, "").unwrap();
        state.refresh(&env, true);

        assert_eq!(state.history.len(), 1, "the last good log stays on screen");
        let (message, level) = state.status_flash().unwrap();
        assert!(
            message.contains("Could not read the session log"),
            "{message}"
        );
        assert_eq!(level, StatusLevel::Warn);
    }

    /// ...and it keeps trying, quietly. The stamp is "what the records on
    /// screen were read from", so a failed read must not advance it: a failure
    /// that leaves the log's fingerprint sitting still would otherwise be seen
    /// once and never looked at again once its warning expired — a History
    /// frozen on old records with nothing on screen to say so.
    ///
    /// The warning that goes with it is one per episode, not one per retry:
    /// every two seconds it would be the only thing this window could ever
    /// show, overwriting a "Copied" or a "Saved" the moment either appeared.
    /// A failure that clears and later returns is a new episode and says so
    /// again.
    #[test]
    fn a_refresh_that_cannot_read_the_log_tries_again_on_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let nest = dir.path().join("nest");
        let log_path = nest.join("history.jsonl");
        let mut config = Config::default();
        config.history.path = Some(log_path.clone());
        config.save(&config_path).unwrap();
        let mut log = SessionLog::open(log_path.clone(), 10);
        log.append(&DictationRecord::now("mock", "first")).unwrap();

        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);
        assert_eq!(state.history.len(), 1);
        let read_from = state.history_stamp;

        // The log's own directory becomes a file, as above — a failure that
        // then stays exactly as it is, which is the case this is about.
        std::fs::remove_file(&log_path).unwrap();
        std::fs::remove_dir(&nest).unwrap();
        std::fs::write(&nest, "").unwrap();
        state.last_refresh = Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        state.refresh(&env, false);
        assert_eq!(
            state.history_stamp, read_from,
            "a read that failed is not the read the records came from"
        );
        assert!(
            state.status_flash().is_some(),
            "the first failure went silent"
        );

        // The next tick, with something else on screen — a "Copied" the user
        // is reading. The read is retried (the `Err` arm ran, which is what
        // `history_read_failing` records) and says nothing: it is the same
        // failure they were told about a moment ago.
        state.flash("Copied to clipboard");
        state.last_refresh = Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        state.refresh(&env, false);
        assert!(state.history_read_failing, "the read was not retried");
        assert_eq!(
            state.status_text(),
            Some("Copied to clipboard"),
            "the same failure took the status line back over"
        );
        assert_eq!(
            state.history_stamp, read_from,
            "the retry that failed moved the stamp"
        );

        // The Refresh button, mid-episode, is not the tick the rate-limit is
        // for: the user asked for this read, and a click that leaves the list
        // where it was and says nothing is a dead control.
        state.status = None;
        state.refresh(&env, true);
        let (message, level) = state
            .status_flash()
            .expect("the Refresh button said nothing at all");
        assert!(
            message.contains("Could not read the session log"),
            "{message}"
        );
        assert_eq!(level, StatusLevel::Warn);

        // And the moment the log opens again, an ordinary refresh has it.
        std::fs::remove_file(&nest).unwrap();
        let mut log = SessionLog::open(log_path.clone(), 10);
        log.append(&DictationRecord::now("mock", "second")).unwrap();
        state.last_refresh = Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        state.refresh(&env, false);
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].text, "second");
        assert!(!state.history_read_failing);

        // A second episode is not the first one still going: it is news, and
        // has to reach the user the same way.
        std::fs::remove_file(&log_path).unwrap();
        std::fs::remove_dir(&nest).unwrap();
        std::fs::write(&nest, "").unwrap();
        state.status = None;
        state.last_refresh = Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        state.refresh(&env, false);
        let (message, level) = state
            .status_flash()
            .expect("a failure that came back went unsaid");
        assert!(
            message.contains("Could not read the session log"),
            "{message}"
        );
        assert_eq!(level, StatusLevel::Warn);
    }

    /// The same rule at the other end: a log that could not be read when the
    /// window *opened* is not a state this window has seen either, so the
    /// stamp it would otherwise have recorded would tell every later tick
    /// there was nothing to re-read — History frozen empty, on the tab that
    /// opens first, until the log's size or mtime happened to move.
    ///
    /// Unix-only, and only because of how the failure has to be staged: it
    /// needs a log that `stat` answers for and `open` refuses, which on
    /// Windows a directory at the log's path is and here a mode-000 file is.
    /// The behaviour under test is portable.
    #[cfg(unix)]
    #[test]
    fn a_log_that_could_not_be_read_at_startup_is_read_again() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let log_path = dir.path().join("history.jsonl");
        let mut config = Config::default();
        config.history.path = Some(log_path.clone());
        config.save(&config_path).unwrap();
        let mut log = SessionLog::open(log_path.clone(), 10);
        log.append(&DictationRecord::now("mock", "first")).unwrap();
        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&log_path).is_ok() {
            // root ignores the mode, and there is no other way to stage this.
            return;
        }

        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);
        assert!(state.history.is_empty());
        assert!(state.history_read_failing);
        assert_eq!(
            state.history_stamp, None,
            "a read that failed is not the read the records came from"
        );
        let (message, level) = state.status_flash().expect("the failure went silent");
        assert!(
            message.contains("Could not read the session log"),
            "{message}"
        );
        assert_eq!(level, StatusLevel::Warn);

        // An ordinary tick — no Refresh click — has it the moment it opens.
        // The stamp is untouched by that: the size and mtime are the ones the
        // failed read already saw, which is exactly what the old code would
        // have recorded and then compared equal to for the rest of the
        // session.
        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        state.last_refresh = Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        state.refresh(&env, false);
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].text, "first");
        assert!(!state.history_read_failing);
        assert!(state.history_stamp.is_some());
    }

    /// `App::run` drains commands between dictations, so an answer can be
    /// minutes away. Expiring "Saving…" while the control it belongs to has
    /// not moved either would leave the click looking like it did nothing.
    #[test]
    fn a_status_waiting_on_the_loop_does_not_expire_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_theme(&env, Theme::Light);
        state.status.as_mut().unwrap().at = Instant::now() - STATUS_HOLD - Duration::from_secs(1);
        assert_eq!(
            state.status_text(),
            Some("Saving…"),
            "the loop still owes an answer"
        );

        take_everything(&mut state, &env, &outcomes_tx);
        assert_eq!(state.status_text(), Some("Saved"));
        state.status.as_mut().unwrap().at = Instant::now() - STATUS_HOLD - Duration::from_secs(1);
        assert_eq!(
            state.status_text(),
            None,
            "an answered change goes back to the normal hold"
        );
    }

    #[test]
    fn set_engine_sends_a_command_and_moves_only_once_the_loop_takes_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);
        let before = state.config.engine;

        state.set_engine(&env, EngineChoice::Groq);
        assert_eq!(
            rx.try_recv().unwrap().1,
            Command::SetEngine(EngineChoice::Groq)
        );
        assert_eq!(
            state.config.engine, before,
            "the picker may not move on a command that has only been queued"
        );

        take_everything(&mut state, &env, &outcomes_tx);
        assert_eq!(state.config.engine, EngineChoice::Groq);
        assert_eq!(state.status_text(), Some("Saved"));
        // No file was written — the loop is the sole writer.
        assert!(!config_path.exists());
    }

    #[test]
    fn set_hotkey_and_set_overlay_enabled_send_dedicated_commands() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_hotkey(&env, Key::F9);
        assert_eq!(rx.try_recv().unwrap().1, Command::SetHotkey(Key::F9));
        state.set_overlay_enabled(&env, false);
        assert_eq!(rx.try_recv().unwrap().1, Command::SetOverlayEnabled(false));
        take_everything(&mut state, &env, &outcomes_tx);
        assert!(state.status_text().unwrap().contains("restart"));
    }

    #[test]
    fn parsed_vocabulary_drops_blank_lines_and_trims_each_term() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.vocabulary_input = "  Deepgram  \n\n Zipformer\n   \n".to_string();
        assert_eq!(
            state.parsed_vocabulary(),
            vec!["Deepgram".to_string(), "Zipformer".to_string()]
        );
    }

    #[test]
    fn an_untouched_vocabulary_box_is_never_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let state = WindowState::new(&env);

        assert!(state.config.vocabulary.is_empty(), "an empty default config");
        assert!(state.vocabulary_input.is_empty());
        assert!(!state.vocabulary_dirty());
    }

    #[test]
    fn set_vocabulary_sends_a_command_and_moves_only_once_the_loop_takes_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.vocabulary_input = "Deepgram\nZipformer".to_string();
        assert!(state.vocabulary_dirty());

        state.set_vocabulary(&env);
        assert_eq!(
            rx.try_recv().unwrap().1,
            Command::SetVocabulary(vec!["Deepgram".to_string(), "Zipformer".to_string()])
        );
        assert!(
            state.config.vocabulary.is_empty(),
            "the list may not move on a command that has only been queued"
        );

        take_everything(&mut state, &env, &outcomes_tx);
        assert_eq!(
            state.config.vocabulary,
            vec!["Deepgram".to_string(), "Zipformer".to_string()]
        );
        assert_eq!(state.status_text(), Some("Saved"));
        assert!(
            !state.vocabulary_dirty(),
            "sync_vocabulary_input must catch up once the box is drawn again"
        );
    }

    #[test]
    fn sync_vocabulary_input_does_not_clobber_an_edit_in_progress() {
        // The refresh loop rereads config.toml every couple of seconds
        // whether or not anything changed; typing into the box must survive
        // that as long as the confirmed list has not actually moved.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.vocabulary_input = "still typing".to_string();
        state.sync_vocabulary_input();
        assert_eq!(state.vocabulary_input, "still typing");
    }

    /// The whole point of the outcome channel: a change the loop refuses says
    /// so, in the loop's own words, instead of flashing "Saved" over a picker
    /// that is about to snap back on its own.
    #[test]
    fn a_change_the_loop_rejects_is_reported_with_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);
        let before = state.config.engine;

        state.set_engine(&env, EngineChoice::Groq);
        outcomes_tx
            .send((
                inflight_id(&state, 0),
                CommandOutcome::Rejected("cannot switch to groq: IRIS_GROQ_KEY is not set".into()),
            ))
            .unwrap();
        state.poll_outcomes(&env);

        assert_eq!(state.config.engine, before, "a refused change may not show");
        let (message, level) = state.status_flash().unwrap();
        assert!(message.contains("IRIS_GROQ_KEY"), "{message}");
        assert_eq!(level, StatusLevel::Warn);
        assert!(!state.awaiting_loop());
    }

    /// One outcome answers the one command it names — so a rejection lands on
    /// the change that was actually refused and not on whatever was clicked
    /// next.
    #[test]
    fn outcomes_answer_the_commands_they_belong_to() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_device(&env, Some("Absent Mic".into()));
        state.set_theme(&env, Theme::Light);
        let device = inflight_id(&state, 0);
        let theme = inflight_id(&state, 1);
        outcomes_tx
            .send((
                device,
                CommandOutcome::Rejected("cannot switch microphone".into()),
            ))
            .unwrap();
        outcomes_tx.send((theme, CommandOutcome::Applied)).unwrap();
        state.poll_outcomes(&env);

        assert_eq!(state.config.audio.device, None, "the refused one");
        assert_eq!(state.config.theme, Theme::Light, "the accepted one");
        assert_eq!(state.status_flash(), Some(("Saved", StatusLevel::Info)));
    }

    /// The channel outlives the window, so a change still in flight when the
    /// user closes it is answered into the *next* window. That answer belongs
    /// to nobody here: reported against whatever this window has since sent,
    /// it would put a stale rejection over a change that did land.
    #[test]
    fn an_answer_owed_to_a_closed_window_is_not_reported_by_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);

        let mut closed = WindowState::new(&env);
        closed.set_engine(&env, EngineChoice::Groq);
        let owed = inflight_id(&closed, 0);
        drop(closed); // the user closed the window before the loop answered

        let mut state = WindowState::new(&env);
        state.set_theme(&env, Theme::Light);
        outcomes_tx
            .send((
                owed,
                CommandOutcome::Rejected("cannot switch to groq".into()),
            ))
            .unwrap();
        state.poll_outcomes(&env);

        assert_eq!(state.config.engine, EngineChoice::default());
        assert_eq!(state.status_text(), Some("Saving…"), "still its own answer");
        assert!(state.awaiting_loop(), "the theme change is still owed one");

        outcomes_tx
            .send((inflight_id(&state, 0), CommandOutcome::Applied))
            .unwrap();
        state.poll_outcomes(&env);

        assert_eq!(state.config.theme, Theme::Light);
        assert_eq!(state.status_flash(), Some(("Saved", StatusLevel::Info)));
    }

    /// A loop that returns while a change is in flight will never answer it.
    /// Leaving the status line on "Saving…" would be the same untruth as
    /// "Saved": nothing is being saved, and nothing will be.
    #[test]
    fn a_loop_that_goes_away_mid_change_stops_the_window_waiting_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded::<(CommandId, CommandOutcome)>();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_theme(&env, Theme::Light);
        assert!(state.awaiting_loop());
        drop(outcomes_tx); // the dictation loop has returned
        state.poll_outcomes(&env);

        assert!(!state.awaiting_loop());
        assert_eq!(state.config.theme, Theme::Dark);
        let (message, level) = state.status_flash().unwrap();
        assert!(message.contains("no longer running"), "{message}");
        assert_eq!(level, StatusLevel::Warn);
    }

    #[test]
    fn refresh_is_a_noop_before_the_interval_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        // Change the file on disk directly, simulating the loop persisting a
        // tray change, then confirm a non-forced refresh right away does not
        // pick it up but a forced one does.
        let changed = Config {
            theme: Theme::Light,
            ..Config::default()
        };
        changed.save(&config_path).unwrap();

        state.refresh(&env, false);
        assert_eq!(
            state.config.theme,
            Theme::Dark,
            "refreshed before the interval"
        );

        state.refresh(&env, true);
        assert_eq!(
            state.config.theme,
            Theme::Light,
            "forced refresh should pick it up"
        );
    }

    /// The log is reread only when it has actually moved: a window left open
    /// refreshes every two seconds, and reparsing an unchanged log — plus
    /// rebuilding the n-gram maps over it — is the one expensive thing here.
    #[test]
    fn refresh_rereads_the_log_only_when_it_has_changed() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let history_path = dir.path().join("history.jsonl");
        let mut log = SessionLog::open(&history_path, 10);
        log.append(&DictationRecord::now("mock", "first")).unwrap();

        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);
        assert_eq!(state.history.len(), 1);

        // A sentinel no reload would produce: it survives a refresh only if
        // the refresh decided there was nothing to read.
        state.history.push(DictationRecord::now("mock", "sentinel"));
        state.last_refresh = Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        state.refresh(&env, false);
        assert_eq!(state.history.len(), 2, "unchanged log was reread");

        log.append(&DictationRecord::now("mock", "second")).unwrap();
        state.last_refresh = Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        state.refresh(&env, false);
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0].text, "second", "newest first");

        // And the Refresh button reads regardless of what `stat` says.
        state.history.clear();
        state.refresh(&env, true);
        assert_eq!(state.history.len(), 2);
    }

    /// "Dictations today" is counted against a day that ends at local
    /// midnight, which arrives whether or not anyone dictates. A window left
    /// open across it must recount over the log it already has, rather than
    /// keep yesterday's figure until the next dictation appends a line.
    #[test]
    fn refresh_recounts_today_when_the_local_day_moves_under_an_unchanged_log() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let mut log = SessionLog::open(dir.path().join("history.jsonl"), 10);
        log.append(&DictationRecord::now("mock", "today")).unwrap();

        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);
        assert_eq!(state.insights.dictations_today, 1);

        // Yesterday, still on screen: what the window would be holding a
        // moment after local midnight, with nothing having touched the log.
        state.insights_day = DayWindow::new("1999-01-01T00:00:00", "1999-01-02T00:00:00");
        state.insights = Insights::compute(&state.history, &state.insights_day);
        assert_eq!(state.insights.dictations_today, 0);

        state.last_refresh = Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        state.refresh(&env, false);
        assert_eq!(
            state.insights.dictations_today, 1,
            "the day rolled over with the log untouched"
        );
    }

    #[test]
    fn status_text_expires() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.flash("hello");
        assert_eq!(state.status_text(), Some("hello"));
        state.status.as_mut().unwrap().at = Instant::now() - STATUS_HOLD - Duration::from_secs(1);
        assert_eq!(state.status_text(), None);
    }

    /// The loop is the only writer of `config.toml`, so a send that never
    /// arrives means nothing was saved. Claiming "Saved" and moving the form
    /// anyway would show a value no file holds until the next refresh quietly
    /// put it back.
    #[test]
    fn a_setting_change_the_loop_can_no_longer_receive_is_reported_as_unsaved() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);
        let before = state.config.clone();
        drop(rx); // the dictation loop has returned

        state.set_engine(&env, EngineChoice::Groq);
        state.set_theme(&env, Theme::Light);
        state.set_polish(&env, !before.polish.enabled);
        state.set_device(&env, Some("Some Mic".into()));
        state.set_hotkey(&env, Key::F9);
        state.set_overlay_enabled(&env, !before.overlay_enabled);

        assert_eq!(state.config, before, "nothing may move without the loop");
        let (message, level) = state.status_flash().unwrap();
        assert!(message.contains("Not saved"), "{message}");
        assert_eq!(level, StatusLevel::Warn);
    }

    #[test]
    fn a_successful_change_flashes_at_info_level() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_theme(&env, Theme::Light);
        take_everything(&mut state, &env, &outcomes_tx);
        assert_eq!(state.config.theme, Theme::Light);
        assert_eq!(state.status_flash(), Some(("Saved", StatusLevel::Info)));
    }

    #[test]
    fn sync_filter_recomputes_only_when_the_query_or_the_history_moves() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let mut log = SessionLog::open(dir.path().join("history.jsonl"), 10);
        log.append(&DictationRecord::now("mock", "apples")).unwrap();
        log.append(&DictationRecord::now("mock", "bananas"))
            .unwrap();

        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.sync_filter();
        assert_eq!(state.filtered(), [0, 1]);

        state.search = "apple".into();
        state.sync_filter();
        assert_eq!(state.filtered(), [1], "newest first, so 'apples' is last");

        // Cached: the same query over the same history costs nothing. Only
        // `refresh` replaces `history`, and it invalidates the cache.
        state.sync_filter();
        assert_eq!(state.filtered(), [1], "no recompute without a change");

        log.append(&DictationRecord::now("mock", "more apples"))
            .unwrap();
        state.refresh(&env, true);
        state.sync_filter();
        assert_eq!(state.filtered(), [0, 2]);
    }

    #[test]
    fn local_day_is_exactly_24_hours_wide_and_shifts_with_the_offset() {
        let utc = local_day(0);
        assert!(utc.contains(&utc_seconds(time::OffsetDateTime::now_utc())));
        // Every offset still covers *now*, and the window it covers moves.
        for hours in [-11, -8, 5, 13] {
            let day = local_day(hours * 3600);
            assert!(
                day.contains(&utc_seconds(time::OffsetDateTime::now_utc())),
                "offset {hours} lost the present moment"
            );
        }
        // A whole-hour offset shifts the boundary by that many hours, so the
        // two windows can only agree when the offset is zero.
        assert_ne!(utc, local_day(9 * 3600));
    }

    #[test]
    fn utc_seconds_drops_the_zone_and_any_fraction() {
        let at = time::OffsetDateTime::from_unix_timestamp(1_784_000_000).unwrap();
        let formatted = utc_seconds(at);
        assert_eq!(formatted.len(), 19, "{formatted}");
        assert!(!formatted.ends_with('Z'), "{formatted}");
        assert_eq!(formatted.matches(':').count(), 2, "{formatted}");
    }

    /// `set_hotkey` saves but must not pretend the change is live: the hook
    /// stays on the key `main` installed until a restart, and the view keys
    /// its restart-pending marker off exactly this divergence.
    #[test]
    fn a_saved_hotkey_diverges_from_the_in_force_one() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        assert_eq!(state.config.hotkey, env.hotkey.running);
        assert!(!env.restart_pending(&state.config).hotkey);
        state.set_hotkey(&env, Key::F9);
        take_everything(&mut state, &env, &outcomes_tx);
        assert_eq!(state.config.hotkey, Key::F9);
        assert_ne!(state.config.hotkey, env.hotkey.running);
        assert!(env.restart_pending(&state.config).hotkey);
    }

    /// `iris --hotkey f9` over a `hotkey = "right-ctrl"` file: `main` never
    /// writes the override back, so the running key and the saved one differ
    /// for the whole session with nothing pending. Marking that as a restart
    /// away from taking effect would nag about an edit nobody made.
    #[test]
    fn a_run_only_cli_override_is_not_a_pending_change() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let mut env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        // What `--hotkey f9` leaves behind, and what an overlay that failed
        // to spawn leaves behind: in force differs from the file, and the
        // file never moved.
        env.hotkey.running = Key::F9;
        env.overlay_enabled.running = false;

        let saved = Config::default();
        assert_eq!(saved.hotkey, env.hotkey.at_startup);
        let pending = env.restart_pending(&saved);
        assert!(!pending.hotkey);
        assert!(!pending.overlay_enabled);
        // Not pending, but not silent either: the view has to be able to say
        // that the file value is not the one in force.
        assert!(env.hotkey.diverged(&saved.hotkey));
        assert!(env.overlay_enabled.diverged(&saved.overlay_enabled));
    }

    /// The same override, and then the user picks that very key in the
    /// Settings picker: the file moves, but onto the value already running,
    /// so nothing is waiting on a restart. Marking it pending would put
    /// "f9 until restart" beside a picker showing f9.
    #[test]
    fn saving_the_value_a_cli_override_already_put_in_force_is_not_pending() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let mut env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        env.hotkey.running = Key::F9;
        env.hotkey.at_startup = Key::RightCtrl;

        let mut state = WindowState::new(&env);
        state.config.hotkey = Key::RightCtrl;
        state.set_hotkey(&env, Key::F9);
        take_everything(&mut state, &env, &outcomes_tx);

        assert_eq!(state.config.hotkey, Key::F9);
        assert!(!env.restart_pending(&state.config).hotkey);
        assert!(!env.hotkey.diverged(&state.config.hotkey));

        // Any *other* key is a real edit: it is neither what the file held at
        // launch nor what is running.
        state.set_hotkey(&env, Key::F8);
        take_everything(&mut state, &env, &outcomes_tx);
        assert!(env.restart_pending(&state.config).hotkey);
    }

    /// `overlay_enabled = true` with a spawn that failed: the file has not
    /// moved, so nothing is pending, but the checkbox must not read as though
    /// the overlay were up. That is `diverged`, not `pending`.
    #[test]
    fn an_overlay_that_failed_to_start_diverges_without_pending_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let mut env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        env.overlay_enabled.running = false;

        let saved = Config::default();
        assert!(saved.overlay_enabled);
        assert!(!env.restart_pending(&saved).overlay_enabled);
        assert!(env.overlay_enabled.diverged(&saved.overlay_enabled));
    }

    #[test]
    fn the_history_list_is_drawn_a_page_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let mut log = SessionLog::open(dir.path().join("history.jsonl"), 500);
        for i in 0..(HISTORY_PAGE * 2 + 5) {
            log.append(&DictationRecord::now("mock", format!("entry {i}")))
                .unwrap();
        }

        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);
        state.sync_filter();

        assert_eq!(state.filtered().len(), HISTORY_PAGE * 2 + 5);
        assert_eq!(state.visible_count(), HISTORY_PAGE);
        state.show_more();
        assert_eq!(state.visible_count(), HISTORY_PAGE * 2);
        state.show_more();
        assert_eq!(
            state.visible_count(),
            state.filtered().len(),
            "never more than there are matches"
        );
        state.reset_paging();
        assert_eq!(state.visible_count(), HISTORY_PAGE);

        // A refresh reloads the log; it must not yank back the pages the user
        // asked for while they are reading them.
        state.show_more();
        state.refresh(&env, true);
        state.sync_filter();
        assert_eq!(state.visible_count(), HISTORY_PAGE * 2);
    }

    #[test]
    fn toggling_the_overlay_is_pending_independently_of_the_hotkey() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let (outcomes_tx, outcomes) = crossbeam_channel::unbounded();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_overlay_enabled(&env, false);
        take_everything(&mut state, &env, &outcomes_tx);
        let pending = env.restart_pending(&state.config);
        assert!(pending.overlay_enabled);
        assert!(!pending.hotkey);
    }

    #[test]
    fn open_config_file_reports_a_failure_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.open_config_file(&env);
        let message = state.status_text().unwrap();
        assert!(message.contains("Could not open"), "{message}");
        assert!(message.contains("launching the editor"), "{message}");
        assert!(message.contains("no desktop in a test"), "{message}");
    }

    /// The window's close button (`super::ui::draw_root` calling this) must
    /// end the app exactly the way the tray's Quit item does: the flag flips
    /// before `Command::Quit` goes out, not after — see
    /// `crate::app::flip_quit_flag`'s doc comment for why the order is
    /// load-bearing to a finalise already polling it.
    #[test]
    fn request_quit_flips_the_flag_before_sending_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let mut env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let quit_flag = Arc::new(AtomicBool::new(false));
        env.quit_flag = Arc::clone(&quit_flag);

        env.request_quit();

        assert!(quit_flag.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(rx.try_recv().unwrap().1, Command::Quit);
    }

    #[test]
    fn note_hidden_to_tray_shows_the_hint_once_and_persists_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let mut env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let calls = std::cell::Cell::new(0u32);
        let hint = |_title: &str, _message: &str| calls.set(calls.get() + 1);
        env.show_close_hint = &hint;

        let mut state = WindowState::new(&env);
        assert!(!state.config.tray_close_hint_shown);

        state.note_hidden_to_tray(&env);
        assert_eq!(calls.get(), 1);
        assert!(state.config.tray_close_hint_shown);
        assert_eq!(
            rx.try_recv().unwrap().1,
            Command::AcknowledgeTrayHint,
            "the flag must be persisted, not just held in memory"
        );

        // A second close in the same run must not show the hint again, and
        // must not send a second command either.
        state.note_hidden_to_tray(&env);
        assert_eq!(calls.get(), 1);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn a_config_that_already_saw_the_hint_does_not_show_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "tray_close_hint_shown = true\n").unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let outcomes = no_outcomes();
        let mut env = env_with(&config_path, &tx, &outcomes, &devices, &reopen_signal);
        let calls = std::cell::Cell::new(0u32);
        let hint = |_title: &str, _message: &str| calls.set(calls.get() + 1);
        env.show_close_hint = &hint;

        let mut state = WindowState::new(&env);
        state.note_hidden_to_tray(&env);

        assert_eq!(calls.get(), 0);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }
}

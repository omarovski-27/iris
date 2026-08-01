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
//! hand edit, a rejected switch rolled back by the loop — show up here too,
//! within one refresh interval; see the tray's own "known limitations" for
//! the same accepted trade-off.

use std::path::Path;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use crate::app::Command;
use crate::config::{Config, EngineChoice, Theme};
use crate::history::{DictationRecord, SessionLog};
use iris_core::hotkey::Key;

use super::insights::{DayWindow, Insights};
use super::search;

/// How often the window re-reads `config.toml` and the session log while it
/// is open and visible.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// How long [`WindowState::status`] stays on screen after [`WindowState::flash`].
pub const STATUS_HOLD: Duration = Duration::from_secs(3);

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
    /// The channel [`crate::App::run`] also drains tray commands from.
    pub commands: &'a Sender<Command>,
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
    /// The local UTC offset, for [`DayWindow`]. Zero (i.e. UTC) wherever the
    /// OS cannot be asked.
    pub utc_offset_seconds: i32,
    /// The hotkey the running process actually listens for — a snapshot taken
    /// in `main`, not the saved setting. The two differ after a rebind, and
    /// the window has to say so: `App` deliberately leaves the installed hook
    /// alone until a restart, so `config.hotkey` alone would tell the user to
    /// hold a key that does nothing.
    pub in_force_hotkey: Key,
    /// Whether the overlay was actually spawned this run, on the same
    /// snapshot-vs-saved footing as [`Env::in_force_hotkey`].
    pub in_force_overlay_enabled: bool,
    /// What `config.toml` held for `hotkey` when the process launched. See
    /// [`Env::restart_pending`] for why the pending check needs this rather
    /// than [`Env::in_force_hotkey`].
    pub saved_hotkey_at_startup: Key,
    /// The same launch-time file value for `overlay_enabled`.
    pub saved_overlay_enabled_at_startup: bool,
}

impl Env<'_> {
    /// Whether the *file* has moved since launch — i.e. whether restarting
    /// would actually change how Iris behaves.
    ///
    /// Deliberately not "saved differs from running": a run-only CLI override
    /// (`iris --hotkey f9` over a `hotkey = "right-ctrl"` file) makes those
    /// two differ for the whole session by design, and marking it pending
    /// would nag about an edit nobody made. A real rebind moves the file, so
    /// it shows up here; the override never touches it, so it does not.
    #[must_use]
    pub fn restart_pending(&self, saved: &Config) -> RestartPending {
        RestartPending {
            hotkey: saved.hotkey != self.saved_hotkey_at_startup,
            overlay_enabled: saved.overlay_enabled != self.saved_overlay_enabled_at_startup,
        }
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
/// view can drop it after [`STATUS_HOLD`].
#[derive(Debug, Clone)]
pub struct Status {
    /// What to show.
    pub message: String,
    /// How to show it.
    pub level: StatusLevel,
    /// When [`WindowState::flash`] set it.
    pub at: Instant,
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
    /// Positions in `history` matching `search`, as of `filtered_for`. See
    /// [`WindowState::sync_filter`].
    filtered: Vec<usize>,
    /// The query `filtered` was computed for; `None` while it is stale, which
    /// is how a reloaded `history` invalidates it.
    filtered_for: Option<String>,
}

impl WindowState {
    /// Load everything fresh: config, history, devices.
    #[must_use]
    pub fn new(env: &Env) -> Self {
        let config = Config::load(env.config_path).unwrap_or_default();
        let history = load_history(&config, env.config_path);
        let insights = Insights::compute(&history, &local_day(env.utc_offset_seconds));
        let devices = (env.list_devices)();
        Self {
            tab: Tab::History,
            search: String::new(),
            config,
            history,
            insights,
            devices,
            status: None,
            last_refresh: Instant::now(),
            filtered: Vec::new(),
            filtered_for: None,
        }
    }

    /// Re-read `config.toml` and the session log if [`REFRESH_INTERVAL`] has
    /// elapsed since the last read, or unconditionally when `force` (a
    /// manual refresh).
    pub fn refresh(&mut self, env: &Env, force: bool) {
        if !force && self.last_refresh.elapsed() < REFRESH_INTERVAL {
            return;
        }
        // A config that fails to parse (mid-write, or hand-edited badly)
        // keeps the last good snapshot rather than resetting the form to
        // defaults out from under the user.
        if let Ok(config) = Config::load(env.config_path) {
            self.config = config;
        }
        self.history = load_history(&self.config, env.config_path);
        self.insights = Insights::compute(&self.history, &local_day(env.utc_offset_seconds));
        self.filtered_for = None;
        self.last_refresh = Instant::now();
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
            Err(e) => self.flash_failure(format!("Could not open config.toml: {e}")),
        }
    }

    /// Show `message` in the status line for [`STATUS_HOLD`].
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
    #[must_use]
    pub fn status_flash(&self) -> Option<(&str, StatusLevel)> {
        self.status
            .as_ref()
            .filter(|status| status.at.elapsed() < STATUS_HOLD)
            .map(|status| (status.message.as_str(), status.level))
    }

    /// The status message, if it has not yet expired.
    #[must_use]
    pub fn status_text(&self) -> Option<&str> {
        self.status_flash().map(|(message, _)| message)
    }

    /// Switch the transcription engine. Takes effect immediately; rolled
    /// back by the loop (and self-corrected here on the next refresh) if the
    /// engine cannot be built, e.g. no API key.
    pub fn set_engine(&mut self, env: &Env, choice: EngineChoice) {
        if self.dispatch(env, Command::SetEngine(choice), "Saved") {
            self.config.engine = choice;
        }
    }

    /// Switch the input device. Takes effect immediately.
    pub fn set_device(&mut self, env: &Env, device: Option<String>) {
        if self.dispatch(env, Command::SetDevice(device.clone()), "Saved") {
            self.config.audio.device = device;
        }
    }

    /// Switch dark/light. Takes effect immediately, on this window too.
    pub fn set_theme(&mut self, env: &Env, theme: Theme) {
        if self.dispatch(env, Command::SetTheme(theme), "Saved") {
            self.config.theme = theme;
        }
    }

    /// Turn transcript cleanup on or off. Takes effect immediately.
    pub fn set_polish(&mut self, env: &Env, enabled: bool) {
        if self.dispatch(env, Command::SetPolish(enabled), "Saved") {
            self.config.polish.enabled = enabled;
        }
    }

    /// Rebind the push-to-talk key. Saved now; needs a restart to take
    /// effect, because the hook is installed once in `main` before this
    /// window exists — the same restart the tray's own hotkey change needs.
    pub fn set_hotkey(&mut self, env: &Env, key: Key) {
        if self.dispatch(
            env,
            Command::SetHotkey(key),
            "Saved — restart Iris to use the new key",
        ) {
            self.config.hotkey = key;
        }
    }

    /// Show or hide the live-text pill overlay. Saved now; needs a restart —
    /// the overlay is spawned once in `main` before this window exists.
    pub fn set_overlay_enabled(&mut self, env: &Env, enabled: bool) {
        if self.dispatch(
            env,
            Command::SetOverlayEnabled(enabled),
            "Saved — restart Iris for this to take effect",
        ) {
            self.config.overlay_enabled = enabled;
        }
    }

    /// Hand `command` to the loop, and report what happened.
    ///
    /// Returns whether the loop took it, which is the only case where the
    /// local [`WindowState::config`] may move: [`crate::App`] writes the file,
    /// so if the receiver is gone (the loop has already returned, and this
    /// window is outliving it by a moment) nothing is saved. Mutating anyway
    /// would leave the form showing a value no file holds, until the next
    /// [`WindowState::refresh`] silently put it back with no explanation.
    fn dispatch(&mut self, env: &Env, command: Command, saved: &str) -> bool {
        match env.commands.send(command) {
            Ok(()) => {
                self.flash(saved);
                true
            }
            Err(_) => {
                self.flash_failure("Not saved — Iris is no longer running. Restart Iris.");
                false
            }
        }
    }
}

fn load_history(config: &Config, config_path: &Path) -> Vec<DictationRecord> {
    let path = config.history_path(config_path);
    let mut records = SessionLog::read_all(&path).unwrap_or_default();
    records.reverse(); // newest first: the recovery path reads top-down
    records
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
    /// user's desktop, the same rule injection lives under.
    fn refuse_to_open(_path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("no desktop in a test")
    }

    fn env_with<'a>(
        config_path: &'a Path,
        commands: &'a Sender<Command>,
        devices: &'a dyn Fn() -> Vec<String>,
        reopen_signal: &'a Receiver<()>,
    ) -> Env<'a> {
        Env {
            config_path,
            commands,
            list_devices: devices,
            open_config_file: &refuse_to_open,
            reopen_signal,
            utc_offset_seconds: 0,
            in_force_hotkey: Key::default(),
            in_force_overlay_enabled: true,
            saved_hotkey_at_startup: Key::default(),
            saved_overlay_enabled_at_startup: true,
        }
    }

    /// A `reopen_signal` for tests that never fire it.
    fn no_reopen() -> Receiver<()> {
        crossbeam_channel::never()
    }

    #[test]
    fn new_loads_defaults_when_nothing_is_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);

        let state = WindowState::new(&env);
        assert_eq!(state.tab, Tab::History);
        assert_eq!(state.config, Config::default());
        assert!(state.history.is_empty());
        assert_eq!(state.insights.total_dictations, 0);
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
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
        let state = WindowState::new(&env);

        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0].text, "second");
        assert_eq!(state.history[1].text, "first");
    }

    #[test]
    fn set_engine_updates_locally_and_sends_a_command() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_engine(&env, EngineChoice::Groq);
        assert_eq!(state.config.engine, EngineChoice::Groq);
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::SetEngine(EngineChoice::Groq)
        );
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
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_hotkey(&env, Key::F9);
        assert_eq!(rx.try_recv().unwrap(), Command::SetHotkey(Key::F9));
        state.set_overlay_enabled(&env, false);
        assert_eq!(rx.try_recv().unwrap(), Command::SetOverlayEnabled(false));
        assert!(state.status_text().unwrap().contains("restart"));
    }

    #[test]
    fn refresh_is_a_noop_before_the_interval_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        Config::default().save(&config_path).unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
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

    #[test]
    fn status_text_expires() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
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
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
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
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_theme(&env, Theme::Light);
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
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
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
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        assert_eq!(state.config.hotkey, env.in_force_hotkey);
        assert!(!env.restart_pending(&state.config).hotkey);
        state.set_hotkey(&env, Key::F9);
        assert_eq!(state.config.hotkey, Key::F9);
        assert_ne!(state.config.hotkey, env.in_force_hotkey);
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
        let mut env = env_with(&config_path, &tx, &devices, &reopen_signal);
        // What `--hotkey f9` and `--overlay`-style overrides leave behind:
        // in force differs from the file, and the file never moves.
        env.in_force_hotkey = Key::F9;
        env.in_force_overlay_enabled = false;

        let saved = Config::default();
        assert_eq!(saved.hotkey, env.saved_hotkey_at_startup);
        let pending = env.restart_pending(&saved);
        assert!(!pending.hotkey);
        assert!(!pending.overlay_enabled);
    }

    #[test]
    fn toggling_the_overlay_is_pending_independently_of_the_hotkey() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (tx, _rx) = crossbeam_channel::unbounded();
        let devices = || Vec::new();
        let reopen_signal = no_reopen();
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.set_overlay_enabled(&env, false);
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
        let env = env_with(&config_path, &tx, &devices, &reopen_signal);
        let mut state = WindowState::new(&env);

        state.open_config_file(&env);
        assert!(state.status_text().unwrap().contains("Could not open"));
    }
}

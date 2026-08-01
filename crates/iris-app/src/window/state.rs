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

use super::insights::Insights;

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
/// `list_devices` arrives as a callback rather than a direct call because
/// enumerating input devices is a Windows-only (WASAPI) operation and this
/// module has to stay portable; see `crate::window::shell` for the real one.
pub struct Env<'a> {
    /// Where `config.toml` lives.
    pub config_path: &'a Path,
    /// The channel [`crate::App::run`] also drains tray commands from.
    pub commands: &'a Sender<Command>,
    /// Enumerate input devices. Empty on a platform/build with no capture.
    pub list_devices: &'a dyn Fn() -> Vec<String>,
    /// Fires when [`crate::window::WindowSink::open`] is called again while
    /// this window is already showing — `ui::draw_root` drains it and asks
    /// the OS to focus the window instead of opening a second one.
    pub reopen_signal: &'a Receiver<()>,
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
    /// A transient message (e.g. "Copied", "Saved") and when it was set, so
    /// the view can fade it out after [`STATUS_HOLD`].
    pub status: Option<(String, Instant)>,
    last_refresh: Instant,
}

impl WindowState {
    /// Load everything fresh: config, history, devices.
    #[must_use]
    pub fn new(env: &Env) -> Self {
        let config = Config::load(env.config_path).unwrap_or_default();
        let history = load_history(&config, env.config_path);
        let insights = Insights::compute(&history, &today_utc());
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
        self.insights = Insights::compute(&self.history, &today_utc());
        self.last_refresh = Instant::now();
    }

    /// Re-enumerate input devices (a manual "refresh" next to the picker).
    pub fn refresh_devices(&mut self, env: &Env) {
        self.devices = (env.list_devices)();
    }

    /// Show `message` in the status line for [`STATUS_HOLD`].
    pub fn flash(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
    }

    /// The status message, if it has not yet expired.
    #[must_use]
    pub fn status_text(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STATUS_HOLD)
            .map(|(text, _)| text.as_str())
    }

    /// Switch the transcription engine. Takes effect immediately; rolled
    /// back by the loop (and self-corrected here on the next refresh) if the
    /// engine cannot be built, e.g. no API key.
    pub fn set_engine(&mut self, env: &Env, choice: EngineChoice) {
        self.config.engine = choice;
        let _ = env.commands.send(Command::SetEngine(choice));
        self.flash("Saved");
    }

    /// Switch the input device. Takes effect immediately.
    pub fn set_device(&mut self, env: &Env, device: Option<String>) {
        self.config.audio.device = device.clone();
        let _ = env.commands.send(Command::SetDevice(device));
        self.flash("Saved");
    }

    /// Switch dark/light. Takes effect immediately, on this window too.
    pub fn set_theme(&mut self, env: &Env, theme: Theme) {
        self.config.theme = theme;
        let _ = env.commands.send(Command::SetTheme(theme));
        self.flash("Saved");
    }

    /// Turn transcript cleanup on or off. Takes effect immediately.
    pub fn set_polish(&mut self, env: &Env, enabled: bool) {
        self.config.polish.enabled = enabled;
        let _ = env.commands.send(Command::SetPolish(enabled));
        self.flash("Saved");
    }

    /// Rebind the push-to-talk key. Saved now; needs a restart to take
    /// effect, because the hook is installed once in `main` before this
    /// window exists — the same restart the tray's own hotkey change needs.
    pub fn set_hotkey(&mut self, env: &Env, key: Key) {
        self.config.hotkey = key;
        let _ = env.commands.send(Command::SetHotkey(key));
        self.flash("Saved — restart Iris to use the new key");
    }

    /// Show or hide the live-text pill overlay. Saved now; needs a restart —
    /// the overlay is spawned once in `main` before this window exists.
    pub fn set_overlay_enabled(&mut self, env: &Env, enabled: bool) {
        self.config.overlay_enabled = enabled;
        let _ = env.commands.send(Command::SetOverlayEnabled(enabled));
        self.flash("Saved — restart Iris for this to take effect");
    }
}

fn load_history(config: &Config, config_path: &Path) -> Vec<DictationRecord> {
    let path = config.history_path(config_path);
    let mut records = SessionLog::read_all(&path).unwrap_or_default();
    records.reverse(); // newest first: the recovery path reads top-down
    records
}

/// Today's UTC date as `"YYYY-MM-DD"` — a prefix of the RFC 3339 timestamps
/// `crate::history` stamps records with.
fn today_utc() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
        .get(..10)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::DictationRecord;

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
            reopen_signal,
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
        state.status.as_mut().unwrap().1 = Instant::now() - STATUS_HOLD - Duration::from_secs(1);
        assert_eq!(state.status_text(), None);
    }

    #[test]
    fn today_utc_looks_like_a_date() {
        let today = today_utc();
        assert_eq!(today.len(), 10, "{today}");
        assert_eq!(today.matches('-').count(), 2, "{today}");
    }
}

//! The user's settings: `iris/config.toml` in the platform config directory.
//!
//! Three rules shape this module.
//!
//! 1. **A missing or partial file is not an error.** Every field has a default,
//!    so an empty file, a file from an older version, and no file at all all
//!    produce a working configuration. Iris is a resident app; refusing to
//!    start because one key is misspelt would be the wrong trade.
//! 2. **Keys never reach a log.** [`Keys`] has a hand-written [`fmt::Debug`]
//!    that redacts, the same guard [`iris_core::engine::DeepgramEngine`] uses,
//!    so a stray `{:?}` on the whole [`Config`] cannot leak one.
//! 3. **The environment wins.** Engine and polisher constructors upstream read
//!    keys from the environment only. [`Config::promote_keys`] copies keys from
//!    the file into the environment *without* overwriting anything already set,
//!    so `IRIS_DEEPGRAM_KEY=... iris` beats the file, and a key never has to be
//!    written to disk to be usable.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use iris_core::hotkey::Key;
use iris_core::inject::Method;
use serde::{Deserialize, Serialize};

/// Overrides the whole config file path. Mainly for tests and for running two
/// configurations side by side.
pub const CONFIG_PATH_ENV: &str = "IRIS_CONFIG";

/// Written above the settings whenever Iris saves the file, because the tray
/// writes this file behind the user's back and an unexplained TOML file in
/// `AppData` is hostile.
const HEADER: &str = "\
# Iris configuration.
#
# Edited by hand or by the tray menu; Iris rewrites this file when you change a
# setting from the tray, which drops any comments you added below this header.
#
#   engine   mock | deepgram | groq | local
#   hotkey   rctrl, lctrl, rshift, ralt, rwin, capslock, scrolllock, pause, f8, f9, f10
#   theme    dark | light
#
# engine, hotkey, input device, theme, polish and overlay_enabled can all be
# changed from the Settings window (tray icon -> Settings) as well as by hand.
#
# Iris starts on the mock engine (offline, no key needed) until you configure
# a real one. To use Deepgram:
#
#   1. Get a key at https://console.deepgram.com
#   2. In the settings below this comment, change engine = \"mock\" to
#      engine = \"deepgram\"
#   3. Add these two lines at the very END of this file, below every other
#      setting:
#
#        [keys]
#        deepgram = \"paste-your-key-here\"
#
#      TOML puts every line after a [keys] header inside that table, so a
#      [keys] block placed anywhere but the end swallows the settings that
#      follow it and Iris will refuse to start.
#   4. Save this file, then quit Iris from the tray and launch it again. A key
#      is read once at startup, so \"Reload settings\" does not apply one.
#
# Groq works the same way with engine = \"groq\" and a groq entry in that same
# [keys] table, from https://console.groq.com/keys.
#
# Keys may also come from the environment (IRIS_DEEPGRAM_KEY, IRIS_GROQ_KEY,
# IRIS_LLM_KEY) instead of this file; an environment variable takes precedence
# over anything set here. Either way, Iris never prints a key back — see
# --print-config.
#
# `version` records which Iris schema last wrote this file. Leave it alone;
# it exists so a changed default can reach a file that predates it.
";

/// The schema version Iris writes today.
///
/// This is a stamp, not a migration framework: it exists for exactly one
/// decision — the `show_live_text` reset in [`Config::migrate`] — and must not
/// grow a registry, per-field versioning or a generic engine. If a second
/// default ever has to reach existing files, that is the moment to decide
/// whether real migration machinery is warranted; do not build it here on
/// speculation.
const CURRENT_VERSION: u32 = 1;

/// What a file written before [`CURRENT_VERSION`] existed reads as.
///
/// The container-level `#[serde(default)]` would otherwise fill an absent
/// `version` from [`Config::default`] — which is the *current* stamp — and
/// every pre-stamp file would claim to be up to date.
fn legacy_version() -> u32 {
    0
}

/// Which transcription backend to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineChoice {
    /// Deterministic, offline, instant. The default until a key is configured.
    #[default]
    Mock,
    /// Deepgram streaming websocket. Needs a Deepgram key.
    Deepgram,
    /// Groq Whisper, batch on key-release. Needs a Groq key.
    Groq,
    /// On-device ASR from `iris-engine-local`. Needs the `local-native`
    /// feature; see that crate's README.
    Local,
}

impl EngineChoice {
    /// Every value, in menu order.
    pub const ALL: &'static [EngineChoice] = &[
        EngineChoice::Mock,
        EngineChoice::Deepgram,
        EngineChoice::Groq,
        EngineChoice::Local,
    ];

    /// The lowercase name used in the config file, the CLI and menu ids.
    pub fn as_str(self) -> &'static str {
        match self {
            EngineChoice::Mock => "mock",
            EngineChoice::Deepgram => "deepgram",
            EngineChoice::Groq => "groq",
            EngineChoice::Local => "local",
        }
    }

    /// What the tray menu shows.
    pub fn label(self) -> &'static str {
        match self {
            EngineChoice::Mock => "Mock (offline, instant)",
            EngineChoice::Deepgram => "Deepgram (streaming)",
            EngineChoice::Groq => "Groq Whisper (batch)",
            EngineChoice::Local => "Local (on-device)",
        }
    }
}

impl std::str::FromStr for EngineChoice {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mock" => Ok(EngineChoice::Mock),
            "deepgram" | "dg" => Ok(EngineChoice::Deepgram),
            "groq" => Ok(EngineChoice::Groq),
            "local" => Ok(EngineChoice::Local),
            other => {
                anyhow::bail!("unknown engine {other:?} (expected mock, deepgram, groq or local)")
            }
        }
    }
}

impl fmt::Display for EngineChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Light or dark. Maps onto the overlay palettes: Dark → Prism, Light → Porcelain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Prism dark — the locked v1 default.
    #[default]
    Dark,
    /// Porcelain light — day-one light theme, same geometry.
    Light,
}

impl Theme {
    /// The lowercase name used in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }
}

impl std::str::FromStr for Theme {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" => Ok(Theme::Dark),
            "light" => Ok(Theme::Light),
            other => anyhow::bail!("unknown theme {other:?} (expected dark or light)"),
        }
    }
}

impl fmt::Display for Theme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The register polished text should keep.
///
/// Mirrors [`iris_polish::TextStyle`], which is `#[non_exhaustive]` and carries
/// no serde derives; this is the serialisable projection of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// Documents, email, notes: full sentences and terminal punctuation.
    #[default]
    Prose,
    /// Chat and comment boxes: a missing final period is fine.
    Message,
    /// Editors and terminals: identifiers and paths are load-bearing.
    Technical,
}

impl From<Style> for iris_polish::TextStyle {
    fn from(style: Style) -> Self {
        match style {
            Style::Prose => iris_polish::TextStyle::Prose,
            Style::Message => iris_polish::TextStyle::Message,
            Style::Technical => iris_polish::TextStyle::Technical,
        }
    }
}

/// Transcript cleanup settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolishConfig {
    /// Whether to polish at all. Off means the engine's transcript is injected
    /// verbatim.
    pub enabled: bool,
    /// Try the LLM polisher when a key is available. Off pins the deterministic
    /// rule engine, which is offline and costs microseconds.
    pub llm: bool,
    /// How long the LLM may take before the rule-engine result is used instead.
    /// The user never waits longer than this.
    pub budget_ms: u64,
    /// The register the output should keep.
    pub style: Style,
}

impl Default for PolishConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            llm: true,
            budget_ms: iris_polish::DEFAULT_LATENCY_BUDGET.as_millis() as u64,
            style: Style::default(),
        }
    }
}

/// Microphone settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Case-insensitive substring of the input device name. `None` uses the
    /// system default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Keep the microphone stream open between dictations.
    ///
    /// On by default: opening a WASAPI stream costs tens of milliseconds, and
    /// that cost would land on the key-press, which is the one place in this
    /// program where latency is visible.
    pub warm: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: None,
            warm: true,
        }
    }
}

/// How the transcript reaches the focused window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InjectConfig {
    /// `sendinput` (synthesised keystrokes) or `clipboard` (Ctrl+V).
    #[serde(with = "as_str")]
    pub method: Method,
    /// Append a space after the transcript, so consecutive dictations do not
    /// run together.
    pub trailing_space: bool,
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            method: Method::default(),
            trailing_space: true,
        }
    }
}

/// The local dictation history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    /// Whether to record dictations at all.
    pub enabled: bool,
    /// How many entries to keep. The file is trimmed to the newest `max_entries`.
    pub max_entries: usize,
    /// Where to write. `None` is `history.jsonl` beside the config file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 500,
            path: None,
        }
    }
}

/// API keys read from the config file.
///
/// The [`fmt::Debug`] impl is hand-written so a key cannot reach a log line, a
/// panic message or a bug report through a stray `{:?}`.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keys {
    /// Deepgram key. Environment: `IRIS_DEEPGRAM_KEY`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deepgram: Option<String>,
    /// Groq key, used by the Groq engine and by the LLM polisher.
    /// Environment: `IRIS_GROQ_KEY`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groq: Option<String>,
    /// Key for an OpenAI-compatible polish endpoint that is not Groq.
    /// Environment: `IRIS_LLM_KEY`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm: Option<String>,
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mark = |k: &Option<String>| if k.is_some() { "<set>" } else { "<unset>" };
        f.debug_struct("Keys")
            .field("deepgram", &mark(&self.deepgram))
            .field("groq", &mark(&self.groq))
            .field("llm", &mark(&self.llm))
            .finish()
    }
}

impl Keys {
    /// True when no key carries a value.
    pub fn is_empty(&self) -> bool {
        self.pairs().next().is_none()
    }

    /// The `(environment variable, key)` pairs that carry a value.
    fn pairs(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("IRIS_DEEPGRAM_KEY", self.deepgram.as_deref()),
            ("IRIS_GROQ_KEY", self.groq.as_deref()),
            ("IRIS_LLM_KEY", self.llm.as_deref()),
        ]
        .into_iter()
        .filter_map(|(var, key)| {
            let key = key.map(str::trim).filter(|k| !k.is_empty())?;
            Some((var, key))
        })
    }
}

/// Everything Iris remembers between runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Which Iris schema last wrote this file. See [`CURRENT_VERSION`].
    ///
    /// Not a user setting, and first in the struct because
    /// `toml::to_string_pretty` writes fields in declaration order and a
    /// scalar emitted after `[polish]` would land inside that table.
    #[serde(default = "legacy_version")]
    pub version: u32,
    /// Which transcription backend to run.
    pub engine: EngineChoice,
    /// The push-to-talk key. Held, not tapped.
    #[serde(with = "as_str")]
    pub hotkey: Key,
    /// Stop the hotkey reaching the focused application.
    ///
    /// On by default, and the reason a bare modifier is usable at all: without
    /// it, holding right-Ctrl to dictate also arms every Ctrl shortcut in the
    /// app underneath.
    pub suppress_hotkey: bool,
    /// Light or dark.
    pub theme: Theme,
    /// Show the live partial transcript in the overlay's ribbon while
    /// listening.
    ///
    /// Off by default (captain feedback, round 3: "maybe we could remove
    /// that feature" — ambivalent, not certain, so the toggle stays and the
    /// feature stays reachable, but it no longer opts a user in by surprise).
    /// The overlay design that made live text the reason to ship (see
    /// `crates/iris-overlay/README.md` "The contract changed, and here is
    /// why") is still there behind this flag. Off leaves the overlay in its
    /// resting capsule presentation: a quiet shape that pulses with the
    /// microphone and never shows any transcript text, which is the default
    /// most users will ever see, not a degraded fallback. This authorises
    /// *displaying* the transcript; it does not change what is persisted or
    /// logged — the history file's own `enabled` setting is the control for
    /// that.
    ///
    /// Flipping this default is the one thing [`CURRENT_VERSION`] exists for;
    /// see [`Config::migrate`].
    pub show_live_text: bool,
    /// Show the live-text pill overlay while dictating.
    ///
    /// Read once at startup (`main` spawns the overlay before [`App`](crate::App)
    /// exists), so like `hotkey` a change here needs a restart to take effect.
    pub overlay_enabled: bool,
    /// Transcript cleanup.
    pub polish: PolishConfig,
    /// Microphone.
    pub audio: AudioConfig,
    /// Delivery of the finished text.
    pub inject: InjectConfig,
    /// The local dictation history.
    pub history: HistoryConfig,
    /// API keys. Prefer the environment; see the module docs.
    ///
    /// Skipped when empty, so a config Iris wrote itself has no `[keys]`
    /// section at all — nothing suggesting the user ought to paste one in.
    #[serde(skip_serializing_if = "Keys::is_empty")]
    pub keys: Keys,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            engine: EngineChoice::default(),
            hotkey: Key::default(),
            suppress_hotkey: true,
            theme: Theme::default(),
            show_live_text: false,
            overlay_enabled: true,
            polish: PolishConfig::default(),
            audio: AudioConfig::default(),
            inject: InjectConfig::default(),
            history: HistoryConfig::default(),
            keys: Keys::default(),
        }
    }
}

impl Config {
    /// Parse a config from TOML text.
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("parsing the Iris configuration")
    }

    /// Render to TOML, with the explanatory header.
    pub fn to_toml(&self) -> Result<String> {
        let body = toml::to_string_pretty(self).context("serialising the Iris configuration")?;
        Ok(format!("{HEADER}\n{body}"))
    }

    /// Render to TOML with every key value replaced by a placeholder.
    ///
    /// This is what `--print-config` shows: which keys are configured, never
    /// what they are. [`Config::save`] keeps the real values — that is how a
    /// key in the file survives a tray-driven save — so the redaction lives
    /// here, on the printing path, and nowhere near serialisation.
    pub fn to_redacted_toml(&self) -> Result<String> {
        let mut redacted = self.clone();
        for key in [
            &mut redacted.keys.deepgram,
            &mut redacted.keys.groq,
            &mut redacted.keys.llm,
        ] {
            *key = key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(|_| "<redacted>".into());
        }
        redacted.to_toml()
    }

    /// Load from `path`, or return the defaults if it does not exist.
    ///
    /// A file that exists but does not parse *is* an error: silently falling
    /// back to defaults would look exactly like Iris ignoring the user's
    /// settings, which is worse than a message that names the problem.
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::load_reporting_migration(path)?.0)
    }

    /// [`Config::load`], also saying whether [`Config::migrate`] changed
    /// anything — which is what decides whether the file is worth rewriting.
    fn load_reporting_migration(path: &Path) -> Result<(Self, bool)> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut config = Self::from_toml(&text)
                    .with_context(|| format!("reading {}", path.display()))?;
                let migrated = config.migrate();
                Ok((config, migrated))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Self::default(), false)),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Bring a file written by an older schema up to [`CURRENT_VERSION`].
    /// Returns whether anything changed.
    ///
    /// One decision lives here and no more. **This deliberately overrides a
    /// value the file states explicitly**, which nothing else in this module
    /// does: `show_live_text` shipped as `true` and every install that ever
    /// ran a previous build has `show_live_text = true` pinned on disk by
    /// [`Config::save`], whether or not the user ever wanted it. A `true` from
    /// before the stamp existed is therefore evidence of the old default, not
    /// of a choice, and leaving it would mean the captain's round-3 decision —
    /// live text is opt-in — never reached a single existing user. A `true`
    /// written at [`CURRENT_VERSION`] *is* a choice and is left alone; so the
    /// reset happens once per install and never again, because the stamp that
    /// this run writes is what makes the next load a no-op.
    fn migrate(&mut self) -> bool {
        if self.version >= CURRENT_VERSION {
            return false;
        }
        self.show_live_text = false;
        self.version = CURRENT_VERSION;
        true
    }

    /// Load from `path`, writing a documented default file first if it is
    /// missing, so that "open settings" always has something to open.
    ///
    /// This is also where a [`Config::migrate`] result is persisted: the stamp
    /// has to reach the disk or the reset repeats on every start and a user
    /// who then turns live text back on by hand loses it again. A failure to
    /// write is not fatal — the migration still holds for this run, and this
    /// module's first rule is that Iris starts — but it is reported the same
    /// way [`crate::app::App`] reports a failed save, unconditionally: a stamp
    /// that never lands means the reset runs again at every start, and a
    /// setting that keeps reverting with nothing on screen to explain it is
    /// exactly the failure a quiet console must not hide.
    ///
    /// A *successful* migration stays behind `--verbose`. It rewrites the
    /// user's file and overrides a value that file stated, which is worth a
    /// record; it is also a once-per-install event on a healthy path, and the
    /// console is quiet by product decision, so it is a diagnostic rather than
    /// a greeting. `iris --verbose` is what turns the one-time override into
    /// something a user who noticed live text disappear can find.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Self::default();
            config.save(path)?;
            return Ok(config);
        }
        let (config, migrated) = Self::load_reporting_migration(path)?;
        if migrated {
            match config.save(path) {
                Ok(()) => iris_core::vlog!(
                    "migrated {} to version {CURRENT_VERSION}: live text is now \
                     opt-in and was turned off; set show_live_text = true to \
                     restore it",
                    path.display()
                ),
                Err(e) => eprintln!("cannot save {}: {e:#}", path.display()),
            }
        }
        Ok(config)
    }

    /// Write to `path`, creating the directory if needed.
    ///
    /// Writes to a temporary file and renames, so a crash or a full disk cannot
    /// leave a half-written config that fails to parse on next start.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = self.to_toml()?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Copy configured keys into the process environment, never overwriting a
    /// variable that is already set.
    ///
    /// Upstream constructors ([`iris_core::engine::build`],
    /// [`iris_polish::LlmPolisher::from_env`]) read keys from the environment
    /// only, deliberately: it keeps keys out of shell history and out of
    /// argument lists. This is the one bridge from the config file to that
    /// rule, and it runs in `main` before any thread starts, because mutating
    /// the environment is only safe while the process is single-threaded.
    ///
    /// Returns the variables it set, for a `--verbose` line. Never the values.
    pub fn promote_keys(&self) -> Vec<&'static str> {
        let mut promoted = Vec::new();
        for (var, key) in self.keys.pairs() {
            let already_set = std::env::var(var).map(|v| !v.trim().is_empty()) == Ok(true);
            if !already_set {
                std::env::set_var(var, key);
                promoted.push(var);
            }
        }
        promoted
    }

    /// Where the history file lives for this configuration.
    pub fn history_path(&self, config_path: &Path) -> PathBuf {
        self.history.path.clone().unwrap_or_else(|| {
            config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("history.jsonl")
        })
    }
}

/// The config file Iris uses unless `--config` or [`CONFIG_PATH_ENV`] says
/// otherwise.
pub fn default_path() -> PathBuf {
    if let Some(explicit) = non_empty(std::env::var(CONFIG_PATH_ENV).ok().as_deref()) {
        return PathBuf::from(explicit);
    }
    config_dir().join("iris").join("config.toml")
}

/// The platform configuration directory.
///
/// - **Windows:** `%APPDATA%`, then `%USERPROFILE%\AppData\Roaming`
/// - **Unix:** `$XDG_CONFIG_HOME`, then `$HOME/.config`
///
/// Unix deliberately ignores the Windows variables: under WSL both are
/// inherited from the host, and a config written to a Windows path from a Linux
/// test run would be invisible to the app that reads it.
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        resolve_config_dir(
            std::env::var("APPDATA").ok().as_deref(),
            std::env::var("USERPROFILE").ok().as_deref(),
        )
    }
    #[cfg(not(windows))]
    {
        resolve_config_dir(
            std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Pure so the fallback chain is testable without touching the environment.
#[cfg(windows)]
fn resolve_config_dir(appdata: Option<&str>, userprofile: Option<&str>) -> PathBuf {
    if let Some(dir) = non_empty(appdata) {
        return PathBuf::from(dir);
    }
    if let Some(home) = non_empty(userprofile) {
        return PathBuf::from(home).join("AppData").join("Roaming");
    }
    PathBuf::from(".")
}

/// Pure so the fallback chain is testable without touching the environment.
#[cfg(not(windows))]
fn resolve_config_dir(xdg: Option<&str>, home: Option<&str>) -> PathBuf {
    if let Some(dir) = non_empty(xdg) {
        return PathBuf::from(dir);
    }
    if let Some(home) = non_empty(home) {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from(".")
}

/// Serde for types that already have `Display` + `FromStr`, which is how
/// [`Key`] and [`Method`] spell themselves everywhere else (CLI flags, error
/// messages). Keeping one spelling means `hotkey = "rctrl"` in the file behaves
/// exactly like `--hotkey rctrl`.
mod as_str {
    use std::fmt::Display;
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Display,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: Display,
    {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_captain_approved_ones() {
        let config = Config::default();
        assert_eq!(config.engine, EngineChoice::Mock);
        assert_eq!(config.hotkey, Key::RightCtrl);
        assert!(config.suppress_hotkey);
        assert_eq!(config.theme, Theme::Dark);
        assert!(
            !config.show_live_text,
            "live text is off by default per captain feedback round 3"
        );
        assert!(config.overlay_enabled);
        assert!(config.polish.enabled);
        assert_eq!(config.inject.method, Method::SendInput);
    }

    #[test]
    fn overlay_enabled_round_trips_and_defaults_on() {
        assert!(Config::from_toml("").unwrap().overlay_enabled);
        let config = Config::from_toml("overlay_enabled = false\n").unwrap();
        assert!(!config.overlay_enabled);
        assert!(config
            .to_toml()
            .unwrap()
            .contains("overlay_enabled = false"));
    }

    #[test]
    fn round_trips_through_toml() {
        let mut config = Config {
            engine: EngineChoice::Deepgram,
            hotkey: Key::F9,
            theme: Theme::Light,
            show_live_text: true,
            ..Config::default()
        };
        config.polish.budget_ms = 220;
        config.polish.style = Style::Technical;
        config.audio.device = Some("Yeti".into());
        config.inject.method = Method::Clipboard;
        config.history.max_entries = 42;
        config.keys.groq = Some("gsk_secret".into());

        let text = config.to_toml().unwrap();
        assert_eq!(Config::from_toml(&text).unwrap(), config);
    }

    /// Every *setting* defaults; the `version` stamp deliberately does not,
    /// because an empty file is a file that predates the stamp and has to be
    /// seen as one — see [`Config::migrate`].
    #[test]
    fn an_empty_file_is_the_default_config() {
        let empty = Config::from_toml("").unwrap();
        assert_eq!(empty.version, 0);
        assert_eq!(
            Config {
                version: CURRENT_VERSION,
                ..empty
            },
            Config::default()
        );
    }

    #[test]
    fn a_partial_file_keeps_the_other_defaults() {
        let config = Config::from_toml("engine = \"groq\"\n[polish]\nenabled = false\n").unwrap();
        assert_eq!(config.engine, EngineChoice::Groq);
        assert!(!config.polish.enabled);
        // Untouched by the file, so still the default.
        assert!(config.polish.llm);
        assert_eq!(config.hotkey, Key::RightCtrl);
        assert!(!config.show_live_text);
    }

    #[test]
    fn show_live_text_can_be_turned_on_from_the_file() {
        let config = Config::from_toml("show_live_text = true\n").unwrap();
        assert!(config.show_live_text);
        // Nothing else moves.
        assert_eq!(config.engine, EngineChoice::Mock);
    }

    #[test]
    fn a_misspelt_setting_is_reported_rather_than_ignored() {
        let err = Config::from_toml("engine = \"whisper\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("parsing"), "{err}");
        assert!(Config::from_toml("hotkey = \"hyper\"").is_err());
        // deny_unknown_fields: a typo is a mistake worth surfacing, not a
        // setting silently doing nothing.
        assert!(Config::from_toml("[polish]\nenabeld = true\n").is_err());
    }

    #[test]
    fn keys_never_appear_in_debug_output() {
        let config = Config {
            keys: Keys {
                deepgram: Some("dg_super_secret".into()),
                groq: Some("gsk_super_secret".into()),
                llm: Some("llm_super_secret".into()),
            },
            ..Config::default()
        };

        let rendered = format!("{config:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("<set>"));
        assert!(format!("{:?}", Keys::default()).contains("<unset>"));
    }

    #[test]
    fn the_print_path_redacts_keys_but_saving_keeps_them() {
        let mut config = Config::default();
        config.keys.deepgram = Some("dg_super_secret".into());
        config.keys.groq = Some("gsk_super_secret".into());

        let rendered = config.to_redacted_toml().unwrap();
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("deepgram = \"<redacted>\""), "{rendered}");
        assert!(rendered.contains("groq = \"<redacted>\""), "{rendered}");
        // The unset llm key is simply absent, so the output still says which
        // keys are configured.
        assert_eq!(rendered.matches("<redacted>").count(), 2, "{rendered}");

        // The file, unlike the print path, must carry the real values.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().keys, config.keys);
    }

    #[test]
    fn missing_file_loads_defaults_and_load_or_create_writes_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");

        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert!(!path.exists(), "load must not create the file");

        let created = Config::load_or_create(&path).unwrap();
        assert_eq!(created, Config::default());
        assert!(path.exists());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("# Iris configuration."));
    }

    /// The round-3 default flip has to reach machines that already have
    /// `show_live_text = true` written to disk by an earlier build, and it has
    /// to stop there: a `true` written since the stamp is a real choice.
    #[test]
    fn the_live_text_default_flip_reaches_a_pre_stamp_file_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // What a previous build left behind: no version key, live text on.
        std::fs::write(&path, "engine = \"groq\"\nshow_live_text = true\n").unwrap();
        let migrated = Config::load_or_create(&path).unwrap();
        assert!(!migrated.show_live_text, "the flip never reached the file");
        assert_eq!(migrated.version, CURRENT_VERSION);
        assert_eq!(
            migrated.engine,
            EngineChoice::Groq,
            "the rest was disturbed"
        );

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains(&format!("version = {CURRENT_VERSION}")),
            "the stamp was not written: {on_disk}"
        );

        // Stamped now, so a later hand-edited `true` is a choice and survives
        // — the reset must not run a second time.
        let mut chosen = Config::load(&path).unwrap();
        chosen.show_live_text = true;
        chosen.save(&path).unwrap();
        assert!(
            Config::load_or_create(&path).unwrap().show_live_text,
            "the migration repeated and ate a deliberate setting"
        );
        assert!(Config::load(&path).unwrap().show_live_text);
    }

    #[test]
    fn saving_replaces_the_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config::default();
        config.save(&path).unwrap();

        let config = Config {
            engine: EngineChoice::Local,
            ..config
        };
        config.save(&path).unwrap();

        assert_eq!(Config::load(&path).unwrap().engine, EngineChoice::Local);
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "temp file left behind"
        );
    }

    #[test]
    fn history_defaults_next_to_the_config() {
        let config = Config::default();
        let path = config.history_path(Path::new("/somewhere/iris/config.toml"));
        assert_eq!(path, Path::new("/somewhere/iris/history.jsonl"));

        let mut config = Config::default();
        config.history.path = Some(PathBuf::from("/elsewhere/log.jsonl"));
        assert_eq!(
            config.history_path(Path::new("/somewhere/iris/config.toml")),
            Path::new("/elsewhere/log.jsonl")
        );
    }

    #[test]
    fn engine_and_theme_parse_the_names_they_print() {
        for engine in EngineChoice::ALL {
            assert_eq!(engine.to_string().parse::<EngineChoice>().unwrap(), *engine);
            assert!(!engine.label().is_empty());
        }
        assert_eq!(
            "DG".parse::<EngineChoice>().unwrap(),
            EngineChoice::Deepgram
        );
        assert!("whisper".parse::<EngineChoice>().is_err());
        for theme in [Theme::Dark, Theme::Light] {
            assert_eq!(theme.to_string().parse::<Theme>().unwrap(), theme);
        }
        assert!("beige".parse::<Theme>().is_err());
    }

    #[test]
    fn only_non_empty_keys_are_promoted() {
        let keys = Keys {
            deepgram: Some("  dg  ".into()),
            groq: Some("   ".into()),
            llm: None,
        };
        let pairs: Vec<_> = keys.pairs().collect();
        assert_eq!(pairs, vec![("IRIS_DEEPGRAM_KEY", "dg")]);
    }

    #[test]
    #[cfg(not(windows))]
    fn config_dir_falls_back_through_xdg_then_home() {
        assert_eq!(
            resolve_config_dir(Some("/xdg"), Some("/home/u")),
            PathBuf::from("/xdg")
        );
        assert_eq!(
            resolve_config_dir(Some("  "), Some("/home/u")),
            PathBuf::from("/home/u/.config")
        );
        assert_eq!(resolve_config_dir(None, None), PathBuf::from("."));
    }
}

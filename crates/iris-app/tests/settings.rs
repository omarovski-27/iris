//! Configuration as the user experiences it: a file that survives a restart,
//! a file edited by hand, and keys that never reach the disk they came from.

use std::path::Path;

use iris_app::config::{self, Config, EngineChoice, Theme};
use iris_core::hotkey::Key;
use iris_core::inject::Method;

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).expect("writing the config");
}

#[test]
fn a_hand_written_file_is_read_exactly_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write(
        &path,
        r#"
engine = "groq"
hotkey = "f9"
theme = "light"
suppress_hotkey = false

[polish]
enabled = true
llm = false
budget_ms = 90
style = "technical"

[audio]
device = "Yeti"
warm = false

[inject]
method = "clipboard"
trailing_space = false

[history]
max_entries = 25
"#,
    );

    let config = Config::load(&path).expect("loading");
    assert_eq!(config.engine, EngineChoice::Groq);
    assert_eq!(config.hotkey, Key::F9);
    assert_eq!(config.theme, Theme::Light);
    assert!(!config.suppress_hotkey);
    assert!(!config.polish.llm);
    assert_eq!(config.polish.budget_ms, 90);
    assert_eq!(config.audio.device.as_deref(), Some("Yeti"));
    assert!(!config.audio.warm);
    assert_eq!(config.inject.method, Method::Clipboard);
    assert!(!config.inject.trailing_space);
    assert_eq!(config.history.max_entries, 25);
    // Not mentioned in the file, so still the default.
    assert!(config.history.enabled);
}

#[test]
fn saving_then_loading_is_the_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("iris").join("config.toml");

    let mut config = Config {
        engine: EngineChoice::Deepgram,
        hotkey: Key::CapsLock,
        ..Config::default()
    };
    config.audio.device = Some("USB Audio".into());
    config.keys.deepgram = Some("dg_key".into());
    config.save(&path).expect("saving");

    assert_eq!(Config::load(&path).expect("loading"), config);
}

#[test]
fn a_saved_file_explains_itself() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    Config::default().save(&path).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    // The tray rewrites this file behind the user's back; it must say so.
    assert!(text.contains("tray menu"), "{text}");
    assert!(text.contains("rctrl"), "{text}");
    assert!(text.contains("IRIS_DEEPGRAM_KEY"), "{text}");
}

#[test]
fn an_unwritable_key_is_never_written_by_accident() {
    // Keys supplied through the environment must not end up in the file just
    // because the tray saved a setting.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let config = Config::default();
    config.save(&path).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("deepgram ="), "{text}");
    assert!(!text.contains("[keys]\ngroq"), "{text}");
}

/// Both halves of the path resolution live in one test on purpose: they mutate
/// the same environment variable, and `cargo test` runs tests in parallel
/// threads within a binary.
#[test]
fn the_config_path_defaults_sensibly_and_can_be_redirected() {
    let previous = std::env::var(config::CONFIG_PATH_ENV).ok();

    std::env::remove_var(config::CONFIG_PATH_ENV);
    let path = config::default_path();
    assert!(path.ends_with("iris/config.toml"), "{}", path.display());

    // `IRIS_CONFIG` is how a second instance, or a test, gets its own settings.
    std::env::set_var(config::CONFIG_PATH_ENV, "/tmp/iris-test/elsewhere.toml");
    assert_eq!(
        config::default_path(),
        Path::new("/tmp/iris-test/elsewhere.toml")
    );

    match previous {
        Some(v) => std::env::set_var(config::CONFIG_PATH_ENV, v),
        None => std::env::remove_var(config::CONFIG_PATH_ENV),
    }
}

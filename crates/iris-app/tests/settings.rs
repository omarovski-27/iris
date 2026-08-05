//! Configuration as the user experiences it: a file that survives a restart,
//! a file edited by hand, and keys that never reach the disk they came from.

use std::path::Path;

use iris_app::config::{self, Config, EngineChoice, Theme};
use iris_app::tray;
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
    // because the tray saved a setting. Asserted on the parsed document, not
    // on the rendered text: the header comment documents a [keys] example, so
    // a substring check would be testing the wording of a comment.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let config = Config::default();
    config.save(&path).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(Config::from_toml(&text).unwrap().keys.is_empty(), "{text}");
    let settings: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!settings.contains("[keys]"), "{settings}");
}

/// The file Iris writes for a first-time user has to load again unchanged —
/// the header is instructions, and instructions that produce an unloadable
/// file are worse than no instructions.
#[test]
fn the_generated_default_file_loads_back_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let created = Config::load_or_create(&path).unwrap();

    assert_eq!(created, Config::default());
    assert_eq!(Config::load(&path).unwrap(), Config::default());
}

/// The header tells the user to append `[keys]` at the very end of the file.
/// This does exactly that, to the file Iris actually generates.
#[test]
fn the_documented_keys_example_appends_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    Config::default().save(&path).unwrap();

    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n[keys]\ndeepgram = \"paste-your-key-here\"\n");
    write(&path, &text);

    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.keys.deepgram.as_deref(), Some("paste-your-key-here"));
    assert_eq!(loaded.engine, Config::default().engine);
    assert_eq!(loaded.hotkey, Config::default().hotkey);
}

/// Every ```toml fence in a Markdown document, in order.
fn toml_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut body = String::new();
    let mut inside = false;
    for line in markdown.lines() {
        match line.trim() {
            "```toml" if !inside => inside = true,
            "```" if inside => {
                inside = false;
                blocks.push(std::mem::take(&mut body));
            }
            _ if inside => {
                body.push_str(line);
                body.push('\n');
            }
            _ => {}
        }
    }
    blocks
}

/// Every `name = "value"` assignment for `name` in `text`, in order.
///
/// The tray notice is prose with the edits embedded in it, so this is what
/// lets the test below *execute* that prose instead of re-typing what it hopes
/// the prose says.
fn assignments(text: &str, name: &str) -> Vec<String> {
    let needle = format!("{name} = \"");
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&needle) {
        let after = &rest[start + needle.len()..];
        let Some(end) = after.find('"') else { break };
        found.push(format!("{name} = \"{}\"", &after[..end]));
        rest = &after[end + 1..];
    }
    found
}

/// The zip's `README.md` is what a non-developer follows to add a key, and its
/// two TOML edits go in two different places for a reason: appended together,
/// `engine` lands inside the file's last table and `deny_unknown_fields`
/// refuses the file. So run the document rather than trusting it — read the
/// real file, apply exactly what it says to the config Iris actually
/// generates, and load the result.
///
/// The tray's demo notice (`tray::demo_notice`) is the same instruction on a
/// different surface — the one a user who never opened the zip sees — so it is
/// executed here too, against the same generated file, and pinned to the
/// README's own wording. Two copies of an instruction that can drift is how the
/// notice came to name only half of it.
#[test]
fn the_packaged_readme_and_the_tray_notice_give_the_same_working_instructions() {
    let readme_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/windows/README.md");
    let readme = std::fs::read_to_string(&readme_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", readme_path.display()));

    let blocks = toml_blocks(&readme);
    let engine_line = blocks
        .iter()
        .find(|b| b.starts_with("engine = "))
        .expect("a ```toml block that sets `engine`")
        .trim()
        .to_string();
    let keys_block = blocks
        .iter()
        .find(|b| b.starts_with("[keys]"))
        .expect("a ```toml block that adds `[keys]`")
        .clone();
    assert!(
        !engine_line.contains("[keys]") && !keys_block.contains("engine ="),
        "the two edits must stay in separate blocks, or the document invites \
         pasting them as one: {engine_line:?} / {keys_block:?}"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    Config::default().save(&path).unwrap();
    let generated = std::fs::read_to_string(&path).unwrap();

    let mut edits = 0;
    let with_engine = generated
        .lines()
        .map(|line| {
            if line.starts_with("engine = ") {
                edits += 1;
                engine_line.clone()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        edits, 1,
        "the README says to edit the existing `engine =` line in place, so the \
         generated file must contain exactly one:\n{generated}"
    );

    let followed = format!("{with_engine}\n\n{keys_block}");
    write(&path, &followed);
    let loaded = Config::load(&path).unwrap_or_else(|e| {
        panic!("following the packaged README produced a config Iris rejects: {e}\n\n{followed}")
    });
    assert_eq!(loaded.engine, EngineChoice::Deepgram);
    assert_eq!(loaded.keys.deepgram.as_deref(), Some("paste-your-key-here"));

    let pasted_as_one_block = format!("{generated}\n{engine_line}\n{keys_block}");
    write(&path, &pasted_as_one_block);
    assert!(
        Config::load(&path).is_err(),
        "appending both edits together is the hazard the README is arranged to \
         avoid; if it has become harmless, say so there instead of implying it"
    );

    // Now the tray, which is the same instruction for the user who never
    // extracted the zip's README — the minimized shortcut means it is all they
    // get. Same words as the README, and they have to work the same way.
    let notice = tray::demo_notice(&Config::default(), &path)
        .expect("the config Iris generates is on the mock engine");
    let spoken = notice.lines().join("\n");

    let engine_edits = assignments(&spoken, "engine");
    assert_eq!(
        engine_edits,
        vec!["engine = \"mock\"".to_string(), engine_line.clone()],
        "the notice must name the line to change and what the README changes it \
         to, in that order:\n{spoken}"
    );
    for line in keys_block.lines().map(str::trim).filter(|l| !l.is_empty()) {
        assert!(
            spoken.contains(line),
            "the notice must carry the README's second edit verbatim ({line:?}):\n{spoken}"
        );
    }
    assert!(
        spoken.contains(&path.display().to_string()),
        "the notice must say which file both edits go in:\n{spoken}"
    );

    // Follow it literally, taking every edit out of the notice's own text.
    let mut edits = 0;
    let tray_edited = generated
        .lines()
        .map(|line| {
            if line.trim() == engine_edits[0] {
                edits += 1;
                engine_edits[1].clone()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        edits, 1,
        "step 1 says to change the existing `engine = \"mock\"` line, so the \
         generated file must contain exactly one:\n{generated}"
    );
    let key_edits = assignments(&spoken, "deepgram");
    assert_eq!(key_edits.len(), 1, "{spoken}");

    let followed_from_the_tray = format!("{tray_edited}\n\n[keys]\n{}\n", key_edits[0]);
    write(&path, &followed_from_the_tray);
    let loaded = Config::load(&path).unwrap_or_else(|e| {
        panic!(
            "following the tray notice produced a config Iris rejects: {e}\n\n{followed_from_the_tray}"
        )
    });
    assert_eq!(loaded.engine, EngineChoice::Deepgram);
    assert_eq!(loaded.keys.deepgram.as_deref(), Some("paste-your-key-here"));
    // The point of the whole exercise: doing what the menu says leaves demo
    // mode, so the next launch is on the real engine and the notice is gone.
    assert_eq!(
        tray::demo_notice(&loaded, &path),
        None,
        "following the notice must actually get the user out of demo mode"
    );
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

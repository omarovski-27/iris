//! The dictation loop, end to end, entirely offline.
//!
//! Every test here drives the real [`App`] — the same code the Windows binary
//! runs — with three substitutions: a channel instead of a microphone, a mock
//! engine instead of a network, and a [`RecordingInjector`] instead of
//! `SendInput`.
//!
//! **Nothing in this file may ever inject text for real.** Windows delivers
//! synthetic keystrokes to whoever is looking at the screen; there is no
//! sandbox, and it has already interrupted real work once on this project. That
//! is why `SystemInjector` is never constructed here, and why the loop takes an
//! injector rather than calling `iris_core::inject` directly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use iris_app::app::Command;
use iris_app::audio::ChannelAudio;
use iris_app::config::{Config, EngineChoice};
use iris_app::pill::PillEvent;
use iris_app::{App, Injector, RecordingInjector, RecordingPill, SessionLog};
use iris_core::engine::{Engine, Session, TranscriptEvent};
use iris_core::hotkey::HotkeyEvent;

/// What the mock engine transcribes, before polish.
const TRANSCRIPT: &str = iris_core::engine::mock::DEFAULT_TRANSCRIPT;

/// A test rig: an app plus every handle needed to drive and observe it.
struct Rig {
    app: App<ChannelAudio>,
    frames: Sender<Vec<i16>>,
    keys: Sender<HotkeyEvent>,
    keys_rx: Receiver<HotkeyEvent>,
    commands: Sender<Command>,
    commands_rx: Receiver<Command>,
    injector: Arc<RecordingInjector>,
    pill: RecordingPill,
    config_path: std::path::PathBuf,
    history_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig_with(config: impl FnOnce(&mut Config)) -> Rig {
    rig_with_injector(config, Arc::new(RecordingInjector::new()))
}

fn rig_with_injector(configure: impl FnOnce(&mut Config), injector: Arc<RecordingInjector>) -> Rig {
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("config.toml");

    let mut config = Config::default();
    // Never let a test reach the network: an LLM key in the developer's
    // environment would otherwise turn `cargo test` into an API call.
    config.polish.llm = false;
    configure(&mut config);
    let history_path = config.history_path(&config_path);

    let audio = ChannelAudio::new();
    let frames = audio.sender();
    let pill = RecordingPill::new();

    let app = App::new(
        config,
        &config_path,
        audio,
        injector.clone() as Arc<dyn Injector>,
        Box::new(pill.clone()),
    )
    .expect("building the app")
    // Keep a failing test from hanging for ten seconds per dictation.
    .with_final_timeout(Duration::from_secs(5));

    let (keys, keys_rx) = crossbeam_channel::unbounded();
    let (commands, commands_rx) = crossbeam_channel::unbounded();

    Rig {
        app,
        frames,
        keys,
        keys_rx,
        commands,
        commands_rx,
        injector,
        pill,
        config_path,
        history_path,
        _dir: dir,
    }
}

fn rig() -> Rig {
    rig_with(|_| {})
}

/// One second of a 220 Hz tone: enough audio for the mock engine to emit
/// partials.
fn speech() -> Vec<i16> {
    (0..16_000)
        .map(|i| {
            ((2.0 * std::f64::consts::PI * 220.0 * i as f64 / 16_000.0).sin() * 8_000.0) as i16
        })
        .collect()
}

impl Rig {
    /// Speak, then release the key — from another thread, because
    /// [`App::dictate`] blocks on this one.
    ///
    /// The release waits until the loop has actually taken every frame.
    /// Queueing the audio and the key-up together would let `select!` pick the
    /// key-up first and turn the whole utterance into tail audio, which is a
    /// legitimate thing for the loop to do and a useless thing to test.
    fn speak(&self) -> std::thread::JoinHandle<()> {
        let frames = self.frames.clone();
        let keys = self.keys.clone();
        std::thread::spawn(move || {
            for chunk in speech().chunks(320) {
                frames.send(chunk.to_vec()).expect("frame");
            }
            while !frames.is_empty() {
                std::thread::sleep(Duration::from_millis(1));
            }
            keys.send(HotkeyEvent::Up(Instant::now())).expect("key up");
        })
    }

    /// Run one dictation to completion.
    fn dictate(&mut self) -> anyhow::Result<iris_app::Dictated> {
        let speaker = self.speak();
        let frames = self.app.frames();
        let outcome = self.app.dictate(Instant::now(), &frames, &self.keys_rx);
        speaker.join().expect("the speaker panicked");
        outcome
    }

    fn records(&self) -> Vec<iris_app::DictationRecord> {
        SessionLog::read_all(&self.history_path).expect("reading the session log")
    }
}

#[test]
fn a_dictation_goes_from_audio_to_injected_text() {
    let mut rig = rig();
    let dictated = rig.dictate().expect("dictation");

    // The rule polisher is deterministic, so this is an exact expectation:
    // the mock transcript is already clean prose and must survive untouched.
    assert_eq!(dictated.record.text, TRANSCRIPT);
    assert_eq!(rig.injector.inserted(), [format!("{TRANSCRIPT} ")]);
    assert!(dictated.record.injected);
    assert!(dictated.record.error.is_none());
}

#[test]
fn the_overlay_sees_the_whole_state_sequence() {
    let mut rig = rig();
    rig.dictate().expect("dictation");

    assert_eq!(
        rig.pill.events(),
        [
            PillEvent::ShowListening,
            PillEvent::Processing,
            PillEvent::Inserted,
            PillEvent::Hide,
        ]
    );
    let levels = rig.pill.levels();
    assert!(!levels.is_empty(), "the meter never moved");
    assert!(
        levels.iter().all(|l| (0.0..=1.0).contains(l)),
        "levels out of range: {levels:?}"
    );
    assert!(
        levels.iter().any(|l| *l > 0.1),
        "audible speech read as silence"
    );
}

#[test]
fn the_dictation_is_recorded_with_its_latency() {
    let mut rig = rig();
    rig.dictate().expect("dictation");

    let records = rig.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.engine, "mock");
    assert_eq!(record.text, TRANSCRIPT);
    assert!(record.injected);
    assert!(record.timestamp.contains('T'), "{}", record.timestamp);
    assert!(
        record.latency.perceived_ms.is_some(),
        "the number the user feels must be recorded"
    );
    assert!(record.latency.audio_secs > 0.5);
    assert!(record.latency.partials > 0, "the mock streams partials");
    assert!(
        record.latency.polish_ms.is_some(),
        "polish is on the budget"
    );
    // The mock transcript is already clean prose, so the rule engine reports
    // that it left it alone rather than claiming credit for it.
    assert_eq!(
        record.polish.as_ref().map(|p| p.source.as_str()),
        Some("unchanged")
    );
    assert!(
        record.raw.is_none(),
        "nothing changed, so there is no raw to keep"
    );
}

#[test]
fn a_failed_injection_still_saves_the_words() {
    // The whole reason the session log exists.
    let injector = Arc::new(RecordingInjector::failing("the focused window is elevated"));
    let mut rig = rig_with_injector(|_| {}, injector);

    let err = rig.dictate().expect_err("injection failed");
    assert!(err.to_string().contains("elevated"), "{err}");

    let records = rig.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].text, TRANSCRIPT, "the transcript must survive");
    assert!(!records[0].injected);
    assert!(records[0].error.as_deref().unwrap().contains("elevated"));

    // The overlay must not claim text appeared when it did not.
    assert_eq!(
        rig.pill.events(),
        [
            PillEvent::ShowListening,
            PillEvent::Processing,
            PillEvent::Hide
        ]
    );
}

#[test]
fn polish_off_injects_the_engine_transcript_verbatim() {
    let mut rig = rig_with(|config| {
        config.polish.enabled = false;
        config.inject.trailing_space = false;
    });
    let dictated = rig.dictate().expect("dictation");

    assert_eq!(dictated.record.text, TRANSCRIPT);
    assert_eq!(rig.injector.inserted(), [TRANSCRIPT.to_string()]);
    assert!(dictated.record.polish.is_none());
    assert!(dictated.record.raw.is_none());
}

#[test]
fn polish_cleans_the_transcript_and_records_what_it_changed() {
    let mut rig = rig_with(|_| {});
    rig.app
        .set_engine(Arc::new(FixedEngine("um so uh i pushed the fix")));

    let dictated = rig.dictate().expect("dictation");
    assert_eq!(dictated.record.text, "So I pushed the fix.");
    assert_eq!(
        dictated.record.raw.as_deref(),
        Some("um so uh i pushed the fix"),
        "the raw transcript is kept whenever polish changed it"
    );
    assert_eq!(
        dictated.record.polish.as_ref().map(|p| p.source.as_str()),
        Some("rule")
    );
    assert_eq!(rig.injector.inserted(), ["So I pushed the fix. "]);
}

#[test]
fn silence_injects_nothing_and_claims_nothing() {
    let mut rig = rig();
    rig.app.set_engine(Arc::new(FixedEngine("   ")));

    let dictated = rig.dictate().expect("dictation");
    assert!(dictated.record.text.is_empty());
    assert!(!dictated.record.injected);
    assert!(rig.injector.inserted().is_empty());
    assert_eq!(
        rig.pill.events(),
        [
            PillEvent::ShowListening,
            PillEvent::Processing,
            PillEvent::Hide
        ],
        "nothing was inserted, so the overlay must not say it was"
    );
    // Still recorded: "I pressed the key and nothing happened" needs evidence.
    assert_eq!(rig.records().len(), 1);
}

#[test]
fn an_engine_failure_is_reported_and_recorded() {
    let mut rig = rig();
    rig.app.set_engine(Arc::new(FailingEngine));

    let err = rig.dictate().expect_err("the engine failed");
    assert!(err.to_string().contains("no key"), "{err}");

    let records = rig.records();
    assert_eq!(records.len(), 1);
    assert!(records[0].text.is_empty());
    assert!(records[0].error.as_deref().unwrap().contains("no key"));
    assert!(rig.injector.inserted().is_empty());
    // The overlay must come down even when everything went wrong.
    assert_eq!(*rig.pill.events().last().unwrap(), PillEvent::Hide);
}

#[test]
fn back_to_back_dictations_do_not_leak_audio_into_each_other() {
    let mut rig = rig();
    rig.dictate().expect("first");
    rig.dictate().expect("second");

    assert_eq!(rig.injector.inserted().len(), 2);
    assert_eq!(rig.records().len(), 2);
    assert_eq!(rig.app.count(), 2);
    let audio: Vec<f64> = rig.records().iter().map(|r| r.latency.audio_secs).collect();
    assert!(
        audio.iter().all(|s| (0.9..1.3).contains(s)),
        "a dictation swallowed the other's audio: {audio:?}"
    );
}

#[test]
fn audio_captured_before_the_key_press_is_discarded() {
    // A warm microphone fills the channel while the previous dictation is still
    // polishing and injecting. That audio is whatever the user said to someone
    // else in the room, and it must not end up in front of what they dictate
    // next.
    let mut rig = rig();
    for chunk in speech().chunks(320) {
        rig.frames.send(chunk.to_vec()).unwrap();
    }

    rig.dictate().expect("dictation");
    let audio = rig.records()[0].latency.audio_secs;
    assert!(
        (0.9..1.3).contains(&audio),
        "stale audio reached the engine: {audio:.2}s of a 1 s utterance"
    );
}

#[test]
fn the_session_log_is_capped() {
    let mut rig = rig_with(|config| config.history.max_entries = 2);
    for _ in 0..4 {
        rig.dictate().expect("dictation");
    }
    assert_eq!(rig.records().len(), 2);
}

#[test]
fn history_disabled_writes_no_file() {
    let mut rig = rig_with(|config| config.history.enabled = false);
    rig.dictate().expect("dictation");
    assert!(!rig.history_path.exists());
}

#[test]
fn the_loop_runs_until_it_is_told_to_quit() {
    let rig = rig();
    let keys = rig.keys.clone();
    let frames = rig.frames.clone();
    let commands = rig.commands.clone();
    let injector = rig.injector.clone();
    let keys_rx = rig.keys_rx.clone();
    let commands_rx = rig.commands_rx.clone();

    let loop_thread = std::thread::spawn(move || {
        let mut app = rig.app;
        app.run(&keys_rx, &commands_rx).map(|()| app)
    });

    // Idle frames from a warm microphone: the loop must discard them rather
    // than let them pile up into the next session.
    frames.send(vec![0i16; 320]).unwrap();
    keys.send(HotkeyEvent::Down(Instant::now())).unwrap();
    for chunk in speech().chunks(320) {
        frames.send(chunk.to_vec()).unwrap();
    }
    keys.send(HotkeyEvent::Up(Instant::now())).unwrap();

    wait_for(|| !injector.inserted().is_empty());
    commands.send(Command::Quit).unwrap();

    let app = loop_thread
        .join()
        .expect("the loop panicked")
        .expect("the loop failed");
    assert_eq!(app.count(), 1);
    assert_eq!(injector.inserted(), [format!("{TRANSCRIPT} ")]);
}

#[test]
fn a_tray_command_changes_and_persists_the_setting() {
    let rig = rig();
    let keys_rx = rig.keys_rx.clone();
    let commands_rx = rig.commands_rx.clone();
    let commands = rig.commands.clone();
    let config_path = rig.config_path.clone();

    commands.send(Command::SetPolish(false)).unwrap();
    commands
        .send(Command::SetEngine(EngineChoice::Mock))
        .unwrap();
    commands.send(Command::Quit).unwrap();

    let loop_thread = std::thread::spawn(move || {
        let mut app = rig.app;
        app.run(&keys_rx, &commands_rx).map(|()| app)
    });
    let app = loop_thread.join().expect("the loop panicked").unwrap();

    assert!(!app.config().polish.enabled);
    // ...and it survives a restart.
    let saved = Config::load(&config_path).expect("the config was written");
    assert!(!saved.polish.enabled);
}

#[test]
fn a_command_channel_that_closes_ends_the_loop() {
    let mut rig = rig();

    let keys_rx = rig.keys_rx.clone();
    let commands_rx = rig.commands_rx.clone();
    drop(rig.commands);

    // A tray that died must not leave a resident process with no way out.
    rig.app.run(&keys_rx, &commands_rx).expect("clean exit");
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for the loop");
}

/// An engine that returns a fixed transcript the instant it is asked to
/// finish — the way to test what the loop does with a given transcript without
/// depending on the mock engine's own text.
struct FixedEngine(&'static str);

struct FixedSession {
    text: &'static str,
    tx: Sender<TranscriptEvent>,
    rx: Receiver<TranscriptEvent>,
}

impl Engine for FixedEngine {
    fn name(&self) -> &'static str {
        "fixed"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        Ok(Box::new(FixedSession {
            text: self.0,
            tx,
            rx,
        }))
    }
}

impl Session for FixedSession {
    fn push(&mut self, _pcm: &[i16]) -> anyhow::Result<()> {
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.rx
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        self.tx.send(TranscriptEvent::Final(self.text.into()))?;
        Ok(())
    }
}

/// An engine that fails the way a keyless cloud engine does.
struct FailingEngine;

impl Engine for FailingEngine {
    fn name(&self) -> &'static str {
        "failing"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Error("no key".into())).unwrap();
        Ok(Box::new(FixedSession { text: "", tx, rx }))
    }
}

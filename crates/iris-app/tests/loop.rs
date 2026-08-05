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
use iris_app::config::{Config, EngineChoice, Theme};
use iris_app::pill::PillEvent;
use iris_app::{
    App, CommandId, CommandOutcome, Injector, RecordingInjector, RecordingPill, RecordingWindow,
    SessionLog,
};
use iris_core::engine::{Engine, Session, TranscriptEvent};
use iris_core::hotkey::{HotkeyEvent, Key};

/// What the mock engine transcribes, before polish.
const TRANSCRIPT: &str = iris_core::engine::mock::DEFAULT_TRANSCRIPT;

/// How long a test waits for the loop to answer a window command. Generous:
/// it only has to be longer than a channel round-trip on a loaded machine.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// A test rig: an app plus every handle needed to drive and observe it.
struct Rig {
    app: App<ChannelAudio>,
    frames: Sender<Vec<i16>>,
    armed: Receiver<()>,
    keys: Sender<HotkeyEvent>,
    keys_rx: Receiver<HotkeyEvent>,
    commands: Sender<Command>,
    commands_rx: Receiver<Command>,
    injector: Arc<RecordingInjector>,
    pill: RecordingPill,
    window: RecordingWindow,
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
    let armed = audio.armed();
    let pill = RecordingPill::new();
    let window = RecordingWindow::new();

    let app = App::new(
        config,
        &config_path,
        audio,
        injector.clone() as Arc<dyn Injector>,
        Box::new(pill.clone()),
    )
    .expect("building the app")
    // Keep a failing test from hanging for ten seconds per dictation.
    .with_final_timeout(Duration::from_secs(5))
    .with_window(Box::new(window.clone()));

    let (keys, keys_rx) = crossbeam_channel::unbounded();
    let (commands, commands_rx) = crossbeam_channel::unbounded();

    Rig {
        app,
        frames,
        armed,
        keys,
        keys_rx,
        commands,
        commands_rx,
        injector,
        pill,
        window,
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
    /// The first frame waits for the arm signal, which the loop raises only
    /// after draining stale audio — otherwise the drain could race the feeder
    /// and discard real utterance frames as pre-key-press audio. The release
    /// then waits until the loop has actually taken every frame. Queueing the
    /// audio and the key-up together would let `select!` pick the key-up first
    /// and turn the whole utterance into tail audio, which is a legitimate
    /// thing for the loop to do and a useless thing to test.
    fn speak(&self) -> std::thread::JoinHandle<()> {
        let frames = self.frames.clone();
        let armed = self.armed.clone();
        let keys = self.keys.clone();
        std::thread::spawn(move || {
            armed.recv().expect("the dictation never armed capture");
            for chunk in speech().chunks(320) {
                frames.send(chunk.to_vec()).expect("frame");
            }
            while !frames.is_empty() {
                std::thread::sleep(Duration::from_millis(1));
            }
            keys.send(HotkeyEvent::Up(Instant::now())).expect("key up");
        })
    }

    /// Shorten the wait for a final transcript. For a test whose engine is
    /// never going to conclude, the default 5s is 5s of wall clock spent
    /// proving nothing the assertion depends on.
    fn with_final_timeout(mut self, timeout: Duration) -> Self {
        self.app = self.app.with_final_timeout(timeout);
        self
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

    let events = rig.pill.events();
    // Startup pushes engine + theme before any dictation.
    assert!(
        events.contains(&PillEvent::SetEngine),
        "engine label should reach the pill: {events:?}"
    );
    assert!(
        events.contains(&PillEvent::SetTheme(Theme::Dark)),
        "default theme should reach the pill: {events:?}"
    );
    // Happy path: listening → processing → inserted(ms). No hide after insert
    // (the overlay self-dismisses after the confirmation hold).
    let core: Vec<_> = events
        .iter()
        .copied()
        .filter(|e| {
            matches!(
                e,
                PillEvent::ShowListening
                    | PillEvent::Processing
                    | PillEvent::Inserted { .. }
                    | PillEvent::Hide
            )
        })
        .collect();
    assert!(
        matches!(
            core.as_slice(),
            [
                PillEvent::ShowListening,
                PillEvent::Processing,
                PillEvent::Inserted { .. },
            ]
        ),
        "unexpected core sequence: {core:?}"
    );
    assert!(
        !core.contains(&PillEvent::Hide),
        "hide must not follow a successful insert"
    );
    // Mock streams partials; at least one length should have been pushed.
    assert!(
        !rig.pill.partial_lens().is_empty(),
        "partial lengths should reach the pill"
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

    // The overlay must not claim text appeared when it did not, and hide
    // must run so the pill leaves the screen.
    let core: Vec<_> = rig
        .pill
        .events()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                PillEvent::ShowListening
                    | PillEvent::Processing
                    | PillEvent::Inserted { .. }
                    | PillEvent::Hide
            )
        })
        .collect();
    assert_eq!(
        core,
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
    let core: Vec<_> = rig
        .pill
        .events()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                PillEvent::ShowListening
                    | PillEvent::Processing
                    | PillEvent::Inserted { .. }
                    | PillEvent::Hide
            )
        })
        .collect();
    assert_eq!(
        core,
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
fn a_stalled_dictation_still_logs_the_real_audio_captured() {
    // Regression: the captain's session log on 2026-08-02 showed two
    // consecutive dictations with `audio_secs: 0.0` right after a pathological
    // 10-second one, which read as if capture itself had broken. The real
    // errors on those two entries were a Deepgram connect timeout and a DNS
    // failure — the audio_secs: 0.0 was an artifact of `App::capture`
    // building a blank Timeline for the log whenever `finish()` errored,
    // discarding whatever real audio and marks it had already stamped. This
    // drives a dictation through the real `App::dictate` path with an engine
    // that connects and captures normally but then never concludes, and
    // checks the logged record still shows the audio that was actually
    // captured instead of reading as an empty hold.
    let mut rig = rig().with_final_timeout(Duration::from_millis(300));
    rig.app.set_engine(Arc::new(NeverConcludesEngine));

    let err = rig.dictate().expect_err("the engine never concludes");
    assert!(
        err.to_string().contains("did not return a transcript"),
        "{err}"
    );

    let records = rig.records();
    assert_eq!(records.len(), 1);
    assert!(!records[0].injected);
    assert!(
        records[0].latency.audio_secs > 0.5,
        "the log must show the real captured audio, not read as an empty \
         hold: {:.2}s",
        records[0].latency.audio_secs
    );
}

#[test]
fn a_short_hold_against_a_slow_connect_still_gets_its_words_typed() {
    // The captain's two zero-audio dictations were connection failures. This
    // is the same shape that *succeeds*, late: the socket comes up after the
    // final-transcript deadline has already gone by but well inside the
    // engine's own connect budget, flushes the audio it was holding, and
    // answers. Every one of those words is recoverable, so every one of them
    // has to reach the screen — a wait that gives up at the moment the
    // connection starts working spends the whole budget and buys nothing.
    let mut rig = rig().with_final_timeout(Duration::from_millis(150));
    rig.app.set_engine(Arc::new(ConnectsLateEngine {
        streams_first: false,
    }));

    let dictated = rig.dictate().expect("a late connect is not a failure");

    assert!(
        dictated
            .record
            .text
            .to_ascii_lowercase()
            .contains("hello there"),
        "the transcript the late connect produced must be delivered: {:?}",
        dictated.record.text
    );
    assert!(dictated.record.injected);
    assert_eq!(rig.injector.inserted().len(), 1);
}

#[test]
fn a_slow_connect_that_streams_an_interim_still_types_the_final() {
    // The same late connect, in the shape that streams a rough interim just
    // ahead of the real transcript. Words existing means the wait is no longer
    // extended — expiry from here costs a tail, not the utterance — but it
    // must not mean the wait is *over*: the accurate text is milliseconds
    // behind the interim, and ending on the interim types the wrong words into
    // the user's document while the right ones are already on the wire.
    let mut rig = rig().with_final_timeout(Duration::from_millis(150));
    rig.app.set_engine(Arc::new(ConnectsLateEngine {
        streams_first: true,
    }));

    let dictated = rig.dictate().expect("a late connect is not a failure");

    assert!(
        dictated
            .record
            .text
            .to_ascii_lowercase()
            .contains("hello there"),
        "the interim was typed instead of the final that followed it: {:?}",
        dictated.record.text
    );
    assert!(dictated.record.injected);
    assert_eq!(rig.injector.inserted().len(), 1);
    assert!(
        dictated.record.error.is_none(),
        "the engine concluded; this is not a salvage: {:?}",
        dictated.record.error
    );
}

#[test]
fn the_wait_for_the_final_transcript_comes_from_the_engine() {
    // The 6s default was measured against a streaming engine. An engine whose
    // work happens after key-up gets to say how long that takes, and the app
    // must ask it rather than applying one architecture's budget to another —
    // built without `with_final_timeout`, so only the engine's own bound can
    // end this wait.
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("config.toml");
    let mut config = Config::default();
    config.polish.llm = false;
    let audio = ChannelAudio::new();
    let frames = audio.sender();
    let armed = audio.armed();
    let injector = Arc::new(RecordingInjector::new());
    let pill = RecordingPill::new();
    let mut app = App::new(
        config,
        &config_path,
        audio,
        injector as Arc<dyn Injector>,
        Box::new(pill),
    )
    .expect("app");
    app.set_engine(Arc::new(ImpatientEngine));

    let (keys_tx, keys_rx) = crossbeam_channel::unbounded();
    let app_frames = app.frames();
    let speaker = std::thread::spawn(move || {
        armed.recv().expect("arm");
        for chunk in speech().chunks(320) {
            frames.send(chunk.to_vec()).expect("frame");
        }
        while !frames.is_empty() {
            std::thread::sleep(Duration::from_millis(1));
        }
        keys_tx
            .send(HotkeyEvent::Up(Instant::now()))
            .expect("key up");
    });

    let started = Instant::now();
    let err = app
        .dictate(Instant::now(), &app_frames, &keys_rx)
        .expect_err("the engine never concludes");
    let waited = started.elapsed();
    speaker.join().expect("speaker");

    assert!(
        err.to_string().contains("did not return a transcript"),
        "{err}"
    );
    assert!(
        waited < Duration::from_secs(3),
        "the app waited out its own default instead of the engine's bound: {waited:?}"
    );
}

#[test]
fn a_mid_hold_failure_never_injects_while_the_key_is_still_held() {
    // The user is still speaking. Whatever died — the microphone, the engine's
    // appetite for frames — recovery must not fire yet: typing a mid-sentence
    // fragment into whatever they are looking at is worse than any delay, and
    // only the real key-up may end an utterance. Once it arrives the hold ends
    // normally, and the words already transcribed are delivered then.
    let mut rig = rig().with_final_timeout(Duration::from_millis(300));
    rig.app.set_engine(Arc::new(PartialThenPushFailsEngine));

    let frames = rig.frames.clone();
    let armed = rig.armed.clone();
    let keys = rig.keys.clone();
    let injector = rig.injector.clone();
    let pill = rig.pill.clone();
    let speaker = std::thread::spawn(move || {
        armed.recv().expect("the dictation never armed capture");
        for chunk in speech().chunks(320) {
            frames.send(chunk.to_vec()).expect("frame");
        }
        // Long past the failure (it fires at 0.5 s of the 1 s sent), with the
        // key still down.
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            injector.inserted().is_empty(),
            "a hold that is still running must never inject"
        );
        assert!(
            !pill.events().contains(&PillEvent::Processing),
            "the hold must still be waiting for the key-up, not finalising"
        );
        keys.send(HotkeyEvent::Up(Instant::now())).expect("key up");
    });

    let app_frames = rig.app.frames();
    let dictated = rig
        .app
        .dictate(Instant::now(), &app_frames, &rig.keys_rx)
        .expect("the hold produced words of its own after the key came up");
    speaker.join().expect("the speaker panicked");

    assert!(
        dictated.record.text.to_ascii_lowercase().contains("hello"),
        "the streamed partial must survive the failure: {:?}",
        dictated.record.text
    );
    assert!(dictated.record.injected);
    assert_eq!(rig.injector.inserted().len(), 1);
    assert!(
        rig.records()[0].latency.audio_secs > 0.4,
        "the log must still show the audio that was captured: {:.2}s",
        rig.records()[0].latency.audio_secs
    );
    assert!(
        rig.records()[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("died mid-hold"),
        "the words that never reached the engine leave no other trace: {:?}",
        rig.records()[0].error
    );
}

#[test]
fn a_dictation_that_survives_a_mid_hold_failure_still_names_it() {
    // The engine went on to return a real transcript through `finish()`, so
    // this is a successful, injected dictation — and it must report as one,
    // not as a failure. But it is a *truncated* one: everything said after the
    // microphone died was never captured, and a record that reads as an
    // ordinary short dictation hides that from whoever is debugging why their
    // words went missing.
    let mut rig = rig().with_final_timeout(Duration::from_millis(300));
    rig.app.set_engine(Arc::new(PushFailsThenFinalEngine));

    let frames = rig.frames.clone();
    let armed = rig.armed.clone();
    let keys = rig.keys.clone();
    let speaker = std::thread::spawn(move || {
        armed.recv().expect("the dictation never armed capture");
        for chunk in speech().chunks(320) {
            frames.send(chunk.to_vec()).expect("frame");
        }
        std::thread::sleep(Duration::from_millis(150));
        keys.send(HotkeyEvent::Up(Instant::now())).expect("key up");
    });

    let app_frames = rig.app.frames();
    let dictated = rig
        .app
        .dictate(Instant::now(), &app_frames, &rig.keys_rx)
        .expect("a delivered dictation is not a failure, whatever it survived");
    speaker.join().expect("the speaker panicked");

    assert!(dictated.record.injected);
    assert!(
        dictated.record.text.to_ascii_lowercase().contains("caught"),
        "the engine's own transcript is what gets delivered: {:?}",
        dictated.record.text
    );
    assert_eq!(rig.injector.inserted().len(), 1);

    let records = rig.records();
    assert_eq!(records.len(), 1);
    assert!(records[0].injected);
    assert!(
        records[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("died mid-hold"),
        "a dictation that lost audio must not read as an ordinary one: {:?}",
        records[0].error
    );
}

#[test]
fn a_mid_hold_failure_still_logs_the_real_audio_captured() {
    // The sibling of the test above for a hold that never transcribed
    // anything: with nothing to show for itself it is a failure, and the log
    // has to carry both what the hold captured — this used to read
    // `audio_secs: 0.0` for half a second of speech — and the mid-hold failure
    // that explains it, alongside the engine's own account of the finish.
    let mut rig = rig().with_final_timeout(Duration::from_millis(300));
    rig.app.set_engine(Arc::new(PushFailsMidHoldEngine));

    let frames = rig.frames.clone();
    let armed = rig.armed.clone();
    let keys = rig.keys.clone();
    let speaker = std::thread::spawn(move || {
        armed.recv().expect("the dictation never armed capture");
        for chunk in speech().chunks(320) {
            frames.send(chunk.to_vec()).expect("frame");
        }
        std::thread::sleep(Duration::from_millis(150));
        keys.send(HotkeyEvent::Up(Instant::now())).expect("key up");
    });

    let app_frames = rig.app.frames();
    let err = rig
        .app
        .dictate(Instant::now(), &app_frames, &rig.keys_rx)
        .expect_err("nothing was ever transcribed");
    speaker.join().expect("the speaker panicked");

    assert!(err.to_string().contains("died mid-hold"), "{err}");
    assert!(rig.injector.inserted().is_empty());

    let records = rig.records();
    assert_eq!(records.len(), 1);
    assert!(!records[0].injected);
    let error = records[0].error.as_deref().unwrap_or_default();
    assert!(error.contains("died mid-hold"), "{error}");
    assert!(
        error.contains("did not return a transcript"),
        "the engine's own account of the finish belongs in the log too: {error}"
    );
    assert!(
        records[0].latency.audio_secs > 0.4,
        "the log must show the audio captured before the failure, not read as \
         an empty hold: {:.2}s",
        records[0].latency.audio_secs
    );
}

#[test]
fn a_dead_hotkey_thread_reports_the_words_without_ever_injecting() {
    // The hotkey channel is the only place a key-up can come from, so with it
    // gone the key can never be confirmed up — and text that cannot be proven
    // to belong at the cursor must not be typed there, however much of it the
    // engine already produced. Not typed is not the same as not kept: the words
    // the user watched appear go in the log, which is the only place left they
    // can be recovered from.
    let mut rig = rig();
    rig.app.set_engine(Arc::new(PartialOnOpenEngine));

    // Already disconnected: there is no hotkey thread at all.
    let (keys_tx, keys_rx) = crossbeam_channel::unbounded::<HotkeyEvent>();
    drop(keys_tx);

    let app_frames = rig.app.frames();
    let err = rig
        .app
        .dictate(Instant::now(), &app_frames, &keys_rx)
        .expect_err("with no hotkey thread the hold cannot complete");

    assert!(err.to_string().contains("hotkey thread stopped"), "{err}");
    assert!(
        rig.injector.inserted().is_empty(),
        "no key-up was ever confirmed; nothing may be typed"
    );
    let records = rig.records();
    assert_eq!(records.len(), 1);
    assert!(!records[0].injected);
    assert!(
        records[0].text.to_ascii_lowercase().contains("hello there"),
        "the words the engine produced must survive in the log: {:?}",
        records[0].text
    );
    assert!(records[0]
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("hotkey thread stopped"));
}

#[test]
fn a_dead_hotkey_thread_keeps_an_earlier_mid_hold_failure_too() {
    // Two things went wrong in one hold: the microphone died, and then the
    // hotkey thread did. The second is what ends the hold, but a log that
    // reports only it hides the reason half the utterance is missing — and
    // the early return that reports it must not skip the mid-hold bookkeeping
    // the ordinary exit does.
    let mut rig = rig();
    rig.app.set_engine(Arc::new(PartialThenPushFailsEngine));

    let (keys_tx, keys_rx) = crossbeam_channel::unbounded::<HotkeyEvent>();
    let frames = rig.frames.clone();
    let armed = rig.armed.clone();
    let speaker = std::thread::spawn(move || {
        armed.recv().expect("the dictation never armed capture");
        for chunk in speech().chunks(320) {
            frames.send(chunk.to_vec()).expect("frame");
        }
        // Not `while !frames.is_empty()`: the hold stops reading this channel
        // the moment the push fails, so the backlog it leaves never drains.
        std::thread::sleep(Duration::from_millis(150));
        // The push failure has fired by now; the hotkey thread dies next.
        drop(keys_tx);
    });

    let app_frames = rig.app.frames();
    let err = rig
        .app
        .dictate(Instant::now(), &app_frames, &keys_rx)
        .expect_err("with no hotkey thread the hold cannot complete");
    speaker.join().expect("the speaker panicked");

    assert!(
        rig.injector.inserted().is_empty(),
        "no key-up was ever confirmed; nothing may be typed"
    );
    assert!(err.to_string().contains("died mid-hold"), "{err}");

    let records = rig.records();
    assert_eq!(records.len(), 1);
    let error = records[0].error.as_deref().unwrap_or_default();
    assert!(
        error.contains("died mid-hold"),
        "the microphone failure is the only trace of the words it cost: {error}"
    );
    assert!(
        error.contains("hotkey thread stopped"),
        "and the failure that actually ended the hold belongs there too: {error}"
    );
    assert!(
        records[0].text.to_ascii_lowercase().contains("hello there"),
        "the words transcribed before either failure are still the user's: {:?}",
        records[0].text
    );
}

#[test]
fn a_salvage_inside_finish_is_delivered_and_still_named_in_the_log() {
    // Nothing failed on the app's side of this hold — the microphone, the
    // frames and the key-up were all fine — so the mid-hold bookkeeping has
    // nothing to report. The engine erred after streaming words, and
    // `Dictation::finish` salvaged them. Delivered, therefore not a failure;
    // salvaged, therefore not an ordinary dictation either, and a log that
    // shows only a suspiciously short transcript is how a real engine failure
    // hid during the 2026-08-02 investigation.
    let mut rig = rig().with_final_timeout(Duration::from_millis(300));
    rig.app.set_engine(Arc::new(ErrorsOnFinishEngine));

    let dictated = rig
        .dictate()
        .expect("the words landed, so this is no failure");

    assert!(dictated.record.injected);
    assert!(
        dictated
            .record
            .text
            .to_ascii_lowercase()
            .contains("hello there"),
        "the streamed partial is what the user watched appear: {:?}",
        dictated.record.text
    );

    let records = rig.records();
    assert_eq!(records.len(), 1);
    assert!(
        records[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("the socket died finalising"),
        "a salvage must carry the engine's own account of why it is one: {:?}",
        records[0].error
    );
}

#[test]
fn a_tail_feed_failure_injects_the_words_and_still_records_the_cause() {
    // The key is already up, so the utterance is over and injecting what the
    // overlay showed cannot land mid-sentence — this is the one path allowed
    // to deliver without `finish()`. The words go in; the log still says the
    // hold ended abnormally, because a socket that died flushing the tail is
    // not an ordinary dictation and the next person reading the log needs it.
    let mut rig = rig().with_final_timeout(Duration::from_millis(300));
    rig.app.set_engine(Arc::new(TailFeedFailsEngine));

    let frames = rig.frames.clone();
    let armed = rig.armed.clone();
    let keys = rig.keys.clone();
    let speaker = std::thread::spawn(move || {
        armed.recv().expect("the dictation never armed capture");
        // Queued in a burst and released immediately, so frames are still
        // buffered when the key comes up — that backlog is the tail.
        for chunk in speech().chunks(320) {
            frames.send(chunk.to_vec()).expect("frame");
        }
        keys.send(HotkeyEvent::Up(Instant::now())).expect("key up");
    });

    let app_frames = rig.app.frames();
    let dictated = rig
        .app
        .dictate(Instant::now(), &app_frames, &rig.keys_rx)
        .expect("the words landed, so this is a dictation and not a failure");
    speaker.join().expect("the speaker panicked");

    assert!(dictated.record.injected);
    assert_eq!(
        rig.injector.inserted().len(),
        1,
        "the words the user watched appear must still be delivered"
    );

    let records = rig.records();
    assert_eq!(records.len(), 1);
    assert!(records[0].injected, "the text reached the cursor");
    assert!(records[0].text.to_ascii_lowercase().contains("hello"));
    assert!(
        records[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("flushing the tail"),
        "a salvage-driven completion must not read as an ordinary dictation: {:?}",
        records[0].error
    );
}

#[test]
fn a_tail_feed_failure_still_reports_an_earlier_mid_hold_cause() {
    // The engine went quiet mid-hold and the microphone stayed healthy, so the
    // tail block still ran — and then failed. That exit delivers and returns on
    // its own, so it has to fold in the cause the hold collected earlier or the
    // engine going quiet leaves no trace at all. The two used to be coupled by
    // accident (every mid-hold cause also closed the frame source); they are
    // not any more.
    let mut rig = rig().with_final_timeout(Duration::from_millis(300));
    rig.app
        .set_engine(Arc::new(GoesQuietThenTailFeedFailsEngine));

    let frames = rig.frames.clone();
    let armed = rig.armed.clone();
    let keys = rig.keys.clone();
    let speaker = std::thread::spawn(move || {
        armed.recv().expect("the dictation never armed capture");
        // First burst: the engine goes quiet on the opening frame and the loop
        // has time to notice, with the key still down.
        for chunk in speech().chunks(320) {
            frames.send(chunk.to_vec()).expect("frame");
        }
        std::thread::sleep(Duration::from_millis(100));
        // Second burst, released with the key: this is what the tail block
        // picks up, and what the engine refuses.
        for chunk in speech().chunks(320) {
            frames.send(chunk.to_vec()).expect("frame");
        }
        keys.send(HotkeyEvent::Up(Instant::now())).expect("key up");
    });

    let app_frames = rig.app.frames();
    let dictated = rig
        .app
        .dictate(Instant::now(), &app_frames, &rig.keys_rx)
        .expect("the words landed, so this is a dictation and not a failure");
    speaker.join().expect("the speaker panicked");

    assert!(dictated.record.injected);
    let records = rig.records();
    assert_eq!(records.len(), 1);
    let error = records[0].error.as_deref().unwrap_or_default();
    assert!(
        error.contains("flushing the tail"),
        "the failure that ended the hold: {error}"
    );
    assert!(
        error.contains("stopped sending events"),
        "and the one that cost the words nobody will ever see: {error}"
    );
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
    let armed = rig.armed.clone();
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
    // Same gate as Rig::speak: the loop drains stale audio before arming, so
    // utterance frames pushed earlier can be discarded under scheduling load.
    armed.recv().expect("the dictation never armed capture");
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

/// `SetHotkey` and `SetOverlayEnabled` are the two settings this window adds
/// beyond what the tray already exposed. Both are restart-gated (the hook
/// and the overlay are both set up once in `main`), so the acceptance bar
/// for them is narrower than a live setting: persisted, and the running
/// process's in-force config deliberately left alone until restart — proven
/// here the same way `reload_keeps_in_force_settings_that_need_a_restart`
/// proves it for a hand-edited file.
#[test]
fn hotkey_and_overlay_changes_persist_but_wait_for_a_restart() {
    let rig = rig();
    let keys_rx = rig.keys_rx.clone();
    let commands_rx = rig.commands_rx.clone();
    let commands = rig.commands.clone();
    let config_path = rig.config_path.clone();

    commands.send(Command::SetHotkey(Key::F9)).unwrap();
    commands.send(Command::SetOverlayEnabled(false)).unwrap();
    commands.send(Command::Quit).unwrap();

    let loop_thread = std::thread::spawn(move || {
        let mut app = rig.app;
        app.run(&keys_rx, &commands_rx).map(|()| app)
    });
    let app = loop_thread.join().expect("the loop panicked").unwrap();

    // In force: unchanged, because a restart is what applies these.
    assert_eq!(app.config().hotkey, Key::RightCtrl);
    assert!(app.config().overlay_enabled);

    // On disk: changed, so the next launch picks them up.
    let saved = Config::load(&config_path).expect("the config was written");
    assert_eq!(saved.hotkey, Key::F9);
    assert!(!saved.overlay_enabled);
}

/// The tray's `Settings` item (`Command::OpenSettings`) must reach the
/// window, not shell out to an editor — the whole point of this crate having
/// a real settings window at all.
#[test]
fn open_settings_opens_the_window() {
    let rig = rig();
    let keys_rx = rig.keys_rx.clone();
    let commands_rx = rig.commands_rx.clone();
    let commands = rig.commands.clone();
    let window = rig.window.clone();

    commands.send(Command::OpenSettings).unwrap();
    commands.send(Command::OpenSettings).unwrap();
    commands.send(Command::Quit).unwrap();

    let loop_thread = std::thread::spawn(move || {
        let mut app = rig.app;
        app.run(&keys_rx, &commands_rx).map(|()| app)
    });
    loop_thread.join().expect("the loop panicked").unwrap();

    assert_eq!(window.opens(), 2);
}

/// The window sends `Command`s on its own channel, distinct from the tray's,
/// and `App::run` must drain both — this is what lets a setting changed in
/// the window take effect without a second, window-owned copy of `App::apply`.
/// It must also answer, because the window shows the user what happened.
#[test]
fn a_window_command_is_applied_the_same_as_a_tray_command() {
    let rig = rig();
    let keys_rx = rig.keys_rx.clone();
    let commands_rx = rig.commands_rx.clone();
    let tray_commands = rig.commands.clone();
    let (window_commands_tx, window_commands_rx) = crossbeam_channel::unbounded();
    let (outcomes_tx, outcomes_rx) = crossbeam_channel::unbounded();

    let loop_thread = std::thread::spawn(move || {
        let mut app = rig
            .app
            .with_window_commands(window_commands_rx, outcomes_tx);
        app.run(&keys_rx, &commands_rx).map(|()| app)
    });

    // `Quit` goes out only once the answer is in hand. Sent up front it would
    // be racing the window command through `select!`, which picks between two
    // ready channels at random — the loop can end before the change it is
    // being tested on is ever drained.
    let id = CommandId::next();
    window_commands_tx
        .send((id, Command::SetTheme(Theme::Light)))
        .unwrap();
    let answer = outcomes_rx.recv_timeout(ANSWER_TIMEOUT).expect("an answer");
    tray_commands.send(Command::Quit).unwrap();
    let app = loop_thread.join().expect("the loop panicked").unwrap();

    assert_eq!(app.config().theme, Theme::Light);
    assert_eq!(answer, (id, CommandOutcome::Applied));
}

/// The loop declines an engine it cannot build and keeps dictating on the one
/// that works — and the window that asked has to hear *why*, or it flashes
/// "Saved" over a picker that snaps back on its own a moment later.
#[test]
fn a_window_command_the_loop_declines_comes_back_with_the_reason() {
    // The rejection under test is a missing key, so a machine that has one
    // cannot produce it. Skipped rather than mutating the environment: keys
    // are read from it, and `App` runs on another thread here.
    if std::env::var("IRIS_DEEPGRAM_KEY").is_ok() {
        return;
    }
    let rig = rig();
    let keys_rx = rig.keys_rx.clone();
    let commands_rx = rig.commands_rx.clone();
    let tray_commands = rig.commands.clone();
    let before = rig.app.config().engine;
    let (window_commands_tx, window_commands_rx) = crossbeam_channel::unbounded();
    let (outcomes_tx, outcomes_rx) = crossbeam_channel::unbounded();

    let loop_thread = std::thread::spawn(move || {
        let mut app = rig
            .app
            .with_window_commands(window_commands_rx, outcomes_tx);
        app.run(&keys_rx, &commands_rx).map(|()| app)
    });

    // Deepgram with no key in the environment: `engines::build` fails, so
    // `App::apply` rolls the choice back rather than leaving every hotkey
    // press failing. `Quit` waits for the answer, as above.
    let id = CommandId::next();
    window_commands_tx
        .send((id, Command::SetEngine(EngineChoice::Deepgram)))
        .unwrap();
    let (answered, outcome) = outcomes_rx.recv_timeout(ANSWER_TIMEOUT).expect("an answer");
    tray_commands.send(Command::Quit).unwrap();
    let app = loop_thread.join().expect("the loop panicked").unwrap();

    assert_eq!(app.config().engine, before, "the loop kept what works");
    assert_eq!(answered, id, "the answer names the command it answers");
    match outcome {
        CommandOutcome::Rejected(reason) => {
            assert!(reason.contains("IRIS_DEEPGRAM_KEY"), "{reason}");
        }
        CommandOutcome::Applied => panic!("an engine with no key must not report success"),
    }
}

/// `Applied` means the loop took the change *and wrote it*. A config file it
/// cannot write is the one way a change the loop had no objection to still
/// will not survive a restart, and the window must not show "Saved" over it.
#[test]
fn a_change_that_cannot_be_written_to_disk_is_not_reported_as_saved() {
    let dir = tempfile::tempdir().expect("temp dir");
    // The parent is a file, so every write under it fails — deterministically
    // and on any platform, unlike a permission bit.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, "").expect("seeding the blocker");
    let config_path = blocker.join("config.toml");

    let mut config = Config::default();
    config.polish.llm = false;

    let app = App::new(
        config,
        &config_path,
        ChannelAudio::new(),
        Arc::new(RecordingInjector::new()) as Arc<dyn Injector>,
        Box::new(RecordingPill::new()),
    )
    .expect("building the app");

    let (tray_commands, commands_rx) = crossbeam_channel::unbounded();
    let (_keys, keys_rx) = crossbeam_channel::unbounded::<HotkeyEvent>();
    let (window_commands_tx, window_commands_rx) = crossbeam_channel::unbounded();
    let (outcomes_tx, outcomes_rx) = crossbeam_channel::unbounded();

    let loop_thread = std::thread::spawn(move || {
        let mut app = app.with_window_commands(window_commands_rx, outcomes_tx);
        app.run(&keys_rx, &commands_rx).map(|()| app)
    });

    let id = CommandId::next();
    window_commands_tx
        .send((id, Command::SetTheme(Theme::Light)))
        .unwrap();
    let (answered, outcome) = outcomes_rx.recv_timeout(ANSWER_TIMEOUT).expect("an answer");
    tray_commands.send(Command::Quit).unwrap();
    let app = loop_thread.join().expect("the loop panicked").unwrap();

    assert_eq!(answered, id);
    match outcome {
        CommandOutcome::Rejected(reason) => {
            assert!(reason.contains("cannot save"), "{reason}");
            // The whole cause chain: which file, and what the OS said.
            assert!(reason.contains("config.toml"), "{reason}");
        }
        CommandOutcome::Applied => panic!("a change that was never written is not saved"),
    }
    assert_eq!(
        app.config().theme,
        Theme::Light,
        "the loop still runs on it; only the file is missing it"
    );
    assert!(!config_path.exists());
}

#[test]
fn a_tray_save_never_persists_a_cli_override() {
    // --no-polish and --device are documented as run-only. A tray change that
    // saves the config must write the file's values for those fields, not the
    // overridden ones the loop is running with.
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("config.toml");

    let mut file_config = Config::default();
    file_config.polish.llm = false;
    file_config.audio.device = Some("Yeti".into());
    file_config
        .save(&config_path)
        .expect("seeding the config file");

    // What main's apply_overrides would produce for `--no-polish --device USB`.
    let mut run_config = file_config.clone();
    run_config.polish.enabled = false;
    run_config.audio.device = Some("USB".into());

    let app = App::new(
        run_config,
        &config_path,
        ChannelAudio::new(),
        Arc::new(RecordingInjector::new()) as Arc<dyn Injector>,
        Box::new(RecordingPill::new()),
    )
    .expect("building the app")
    .with_file_config(file_config);

    let (commands, commands_rx) = crossbeam_channel::unbounded();
    let (_keys, keys_rx) = crossbeam_channel::unbounded::<HotkeyEvent>();
    commands.send(Command::SetTheme(Theme::Light)).unwrap();
    commands.send(Command::Quit).unwrap();

    let loop_thread = std::thread::spawn(move || {
        let mut app = app;
        app.run(&keys_rx, &commands_rx).map(|()| app)
    });
    let app = loop_thread.join().expect("the loop panicked").unwrap();

    // The loop keeps running with the overrides...
    assert!(!app.config().polish.enabled);
    assert_eq!(app.config().audio.device.as_deref(), Some("USB"));

    // ...but the file got the tray's change and nothing else.
    let saved = Config::load(&config_path).expect("the config was written");
    assert_eq!(saved.theme, Theme::Light);
    assert!(saved.polish.enabled, "--no-polish leaked into the file");
    assert_eq!(
        saved.audio.device.as_deref(),
        Some("Yeti"),
        "--device leaked into the file"
    );
}

/// `show_live_text` is the opt-in the live-text ribbon is reached through
/// (off by default since round 3), so editing it and reloading has to take
/// effect there and then — the same treatment `theme` already gets. Before
/// this, the flag was frozen into the overlay sink at startup and the reload
/// said "reloaded" while the transcript kept appearing on screen until the
/// process restarted.
///
/// What the loop owes the sink is the pushed value, at startup and again on
/// every reload, and that is exactly what this asserts. The other half — the
/// sink dropping partial text once it has been pushed `false` — belongs to
/// `OverlayPill` and is pinned there, against the real sink, by
/// `set_show_live_text_moves_the_real_gate_both_ways`.
#[test]
fn reload_pushes_the_live_text_opt_in_down_without_a_restart() {
    let mut rig = rig_with(|c| c.show_live_text = true);
    rig.dictate().expect("first dictation");
    assert!(
        !rig.pill.partial_texts().is_empty(),
        "the live ribbon never got any text to begin with"
    );
    let pushed = |pill: &RecordingPill| -> Vec<bool> {
        pill.events()
            .into_iter()
            .filter_map(|e| match e {
                PillEvent::SetShowLiveText(on) => Some(on),
                _ => None,
            })
            .collect()
    };
    assert_eq!(pushed(&rig.pill), [true], "startup never told the sink");

    // The user edits config.toml and asks for a reload — no restart.
    let mut off = Config::default();
    off.polish.llm = false;
    off.show_live_text = false;
    off.save(&rig.config_path).expect("writing the config file");
    rig.commands.send(Command::Reload).unwrap();
    rig.commands.send(Command::Quit).unwrap();
    rig.app
        .run(&rig.keys_rx, &rig.commands_rx)
        .expect("the loop should exit on Quit");

    assert!(!rig.app.config().show_live_text, "the reload did not land");
    assert_eq!(
        pushed(&rig.pill),
        [true, false],
        "the reloaded setting never reached the sink: {:?}",
        rig.pill.events()
    );

    // ... and the dictation itself is untouched: only the display changed.
    rig.dictate().expect("second dictation");
    assert_eq!(rig.injector.inserted().len(), 2);
}

#[test]
fn reload_keeps_in_force_settings_that_need_a_restart() {
    // Reload must not pretend hotkey/audio/keys/inject.method took effect:
    // the hook, mic and injector were configured at startup, and promote_keys
    // cannot run again. The in-memory config keeps what is actually in force
    // so a second reload of the same file still has something to compare
    // against. trailing_space is read live, so the file value does apply.
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("config.toml");

    let mut file_config = Config::default();
    file_config.polish.llm = false;
    file_config.hotkey = Key::F9;
    file_config.audio.device = Some("Yeti".into());
    file_config.audio.warm = false;
    file_config.keys.deepgram = Some("file-key".into());
    file_config.inject.method = iris_core::inject::Method::Clipboard;
    file_config.inject.trailing_space = false;
    file_config
        .save(&config_path)
        .expect("seeding the config file");

    let mut run_config = Config::default();
    run_config.polish.llm = false;
    // Distinct from the file so Reload has something to flag.
    run_config.hotkey = Key::RightCtrl;
    run_config.audio.device = Some("USB".into());
    run_config.audio.warm = true;
    run_config.keys.deepgram = Some("run-key".into());
    run_config.inject.method = iris_core::inject::Method::SendInput;
    run_config.inject.trailing_space = true;

    let app = App::new(
        run_config,
        &config_path,
        ChannelAudio::new(),
        Arc::new(RecordingInjector::new()) as Arc<dyn Injector>,
        Box::new(RecordingPill::new()),
    )
    .expect("building the app")
    .with_file_config(file_config.clone());

    let (commands, commands_rx) = crossbeam_channel::unbounded();
    let (_keys, keys_rx) = crossbeam_channel::unbounded::<HotkeyEvent>();
    commands.send(Command::Reload).unwrap();
    commands.send(Command::Quit).unwrap();

    let app = std::thread::spawn(move || {
        let mut app = app;
        app.run(&keys_rx, &commands_rx).map(|()| app)
    })
    .join()
    .expect("the loop panicked")
    .unwrap();

    assert_eq!(app.config().hotkey, Key::RightCtrl);
    assert_eq!(app.config().audio.device.as_deref(), Some("USB"));
    assert!(app.config().audio.warm);
    assert_eq!(app.config().keys.deepgram.as_deref(), Some("run-key"));
    assert_eq!(
        app.config().inject.method,
        iris_core::inject::Method::SendInput
    );
    assert!(!app.config().inject.trailing_space);
}

// Local without `local-native` always fails to build, independent of ambient
// API keys — Deepgram would succeed whenever IRIS_DEEPGRAM_KEY is set.
#[cfg(not(feature = "local-native"))]
#[test]
fn reload_does_not_persist_a_failed_engine_choice() {
    // A file that asks for an engine we cannot build must not poison `saved`:
    // the next tray persist would otherwise write a cold-start failure.
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("config.toml");

    let mut file_config = Config::default();
    file_config.polish.llm = false;
    file_config.engine = EngineChoice::Local;
    file_config
        .save(&config_path)
        .expect("seeding the config file");

    let mut run_config = Config::default();
    run_config.polish.llm = false;
    run_config.engine = EngineChoice::Mock;

    let app = App::new(
        run_config,
        &config_path,
        ChannelAudio::new(),
        Arc::new(RecordingInjector::new()) as Arc<dyn Injector>,
        Box::new(RecordingPill::new()),
    )
    .expect("building the app")
    .with_file_config({
        let mut saved = Config::default();
        saved.polish.llm = false;
        saved.engine = EngineChoice::Mock;
        saved
    });

    let (commands, commands_rx) = crossbeam_channel::unbounded();
    let (_keys, keys_rx) = crossbeam_channel::unbounded::<HotkeyEvent>();
    commands.send(Command::Reload).unwrap();
    // A tray change that persists: must not write local into the file.
    commands.send(Command::SetTheme(Theme::Light)).unwrap();
    commands.send(Command::Quit).unwrap();

    let app = std::thread::spawn(move || {
        let mut app = app;
        app.run(&keys_rx, &commands_rx).map(|()| app)
    })
    .join()
    .expect("the loop panicked")
    .unwrap();

    assert_eq!(app.config().engine, EngineChoice::Mock);
    let saved = Config::load(&config_path).expect("the config was written");
    assert_eq!(saved.engine, EngineChoice::Mock);
    assert_eq!(saved.theme, Theme::Light);
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

/// Connects normally but never sends a terminal event, no matter how long
/// `finish()` waits — models a Deepgram session stalled by a network failure
/// (the 2026-08-02 regression) rather than a connection that fails outright.
struct NeverConcludesEngine;

struct NeverConcludesSession {
    events: Receiver<TranscriptEvent>,
    _keep: Sender<TranscriptEvent>,
}

impl Engine for NeverConcludesEngine {
    fn name(&self) -> &'static str {
        "never-concludes"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        Ok(Box::new(NeverConcludesSession {
            events: rx,
            _keep: tx,
        }))
    }
}

impl Session for NeverConcludesSession {
    fn push(&mut self, _pcm: &[i16]) -> anyhow::Result<()> {
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.events
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Connects, takes half a second of audio, then refuses the rest — a socket
/// dying in the middle of a hold, which fails the *feed* rather than anything
/// `finish()` would ever see.
struct PushFailsMidHoldEngine;

struct PushFailsMidHoldSession {
    events: Receiver<TranscriptEvent>,
    _keep: Sender<TranscriptEvent>,
    samples: usize,
}

impl Engine for PushFailsMidHoldEngine {
    fn name(&self) -> &'static str {
        "push-fails-mid-hold"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        Ok(Box::new(PushFailsMidHoldSession {
            events: rx,
            _keep: tx,
            samples: 0,
        }))
    }
}

impl Session for PushFailsMidHoldSession {
    fn push(&mut self, pcm: &[i16]) -> anyhow::Result<()> {
        self.samples += pcm.len();
        // 8000 samples at 16 kHz: half a second of very real speech.
        if self.samples >= 8_000 {
            anyhow::bail!("the websocket died mid-hold");
        }
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.events
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Refuses audio mid-hold like [`PushFailsMidHoldEngine`], but still returns a
/// real transcript when asked to finalise: the hold loses its tail of audio and
/// succeeds anyway.
struct PushFailsThenFinalEngine;

struct PushFailsThenFinalSession {
    events: Receiver<TranscriptEvent>,
    tx: Sender<TranscriptEvent>,
    samples: usize,
}

impl Engine for PushFailsThenFinalEngine {
    fn name(&self) -> &'static str {
        "push-fails-then-final"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        Ok(Box::new(PushFailsThenFinalSession {
            events: rx,
            tx,
            samples: 0,
        }))
    }
}

impl Session for PushFailsThenFinalSession {
    fn push(&mut self, pcm: &[i16]) -> anyhow::Result<()> {
        self.samples += pcm.len();
        if self.samples >= 8_000 {
            anyhow::bail!("the websocket died mid-hold");
        }
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.events
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        let _ = self
            .tx
            .send(TranscriptEvent::Final("what it caught".into()));
        Ok(())
    }
}

/// Streams one partial the instant the session opens, then never concludes —
/// words are available from the very first moment, whatever else happens.
struct PartialOnOpenEngine;

impl Engine for PartialOnOpenEngine {
    fn name(&self) -> &'static str {
        "partial-on-open"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        tx.send(TranscriptEvent::Partial("hello there".into()))
            .unwrap();
        Ok(Box::new(NeverConcludesSession {
            events: rx,
            _keep: tx,
        }))
    }
}

/// Takes every frame the hold feeds it, then refuses the tail — the socket
/// dying in the gap between key-up and `finish()`.
struct TailFeedFailsEngine;

struct TailFeedFailsSession {
    events: Receiver<TranscriptEvent>,
    _keep: Sender<TranscriptEvent>,
}

impl Engine for TailFeedFailsEngine {
    fn name(&self) -> &'static str {
        "tail-feed-fails"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        // Streamed up front so the words exist however few frames the hold got
        // through before the key came up.
        tx.send(TranscriptEvent::Partial("hello there".into()))
            .unwrap();
        Ok(Box::new(TailFeedFailsSession {
            events: rx,
            _keep: tx,
        }))
    }
}

impl Session for TailFeedFailsSession {
    fn push(&mut self, pcm: &[i16]) -> anyhow::Result<()> {
        // The hold feeds one 320-sample frame at a time; only the tail arrives
        // as a single larger chunk, so this fails there and nowhere else.
        if pcm.len() > 320 {
            anyhow::bail!("the websocket died flushing the tail");
        }
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.events
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// As [`TailFeedFailsEngine`], but it also drops its event sender on the first
/// frame — the engine going quiet mid-hold while the microphone stays healthy,
/// so the tail block still runs and then fails with a cause already recorded.
struct GoesQuietThenTailFeedFailsEngine;

struct GoesQuietThenTailFeedFailsSession {
    events: Receiver<TranscriptEvent>,
    tx: Option<Sender<TranscriptEvent>>,
}

impl Engine for GoesQuietThenTailFeedFailsEngine {
    fn name(&self) -> &'static str {
        "goes-quiet-then-tail-feed-fails"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        tx.send(TranscriptEvent::Partial("hello there".into()))
            .unwrap();
        Ok(Box::new(GoesQuietThenTailFeedFailsSession {
            events: rx,
            tx: Some(tx),
        }))
    }
}

impl Session for GoesQuietThenTailFeedFailsSession {
    fn push(&mut self, pcm: &[i16]) -> anyhow::Result<()> {
        if pcm.len() > 320 {
            anyhow::bail!("the websocket died flushing the tail");
        }
        // Pump exit: the events channel disconnects, the frames keep flowing.
        self.tx = None;
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.events
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Takes a connect budget of its own and comes up only after the app's
/// final-transcript deadline has already passed, then answers the way a real
/// socket does once it is up. The degraded-network connect that succeeds late.
struct ConnectsLateEngine {
    /// Whether the flush puts a rough interim on the wire just ahead of the
    /// real transcript. Both shapes are real (see `engine/deepgram.rs`), and
    /// the one with an interim is the one that can hand back the wrong words.
    streams_first: bool,
}

struct ConnectsLateSession {
    events: Receiver<TranscriptEvent>,
    _worker: std::thread::JoinHandle<()>,
}

impl Engine for ConnectsLateEngine {
    fn name(&self) -> &'static str {
        "connects-late"
    }
    fn connect_budget(&self) -> Option<Duration> {
        Some(Duration::from_secs(3))
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let streams_first = self.streams_first;
        let worker = std::thread::spawn(move || {
            // Past the 150ms the app is prepared to wait from key-up, inside
            // the 3s the engine is allowed for its connect.
            std::thread::sleep(Duration::from_millis(400));
            let _ = tx.send(TranscriptEvent::Connected);
            std::thread::sleep(Duration::from_millis(50));
            if streams_first {
                let _ = tx.send(TranscriptEvent::Partial("hello ther".into()));
                std::thread::sleep(Duration::from_millis(30));
            }
            // A short hold can have nothing but the post-`Finalize` flush, in
            // which case the whole transcript arrives at once.
            let _ = tx.send(TranscriptEvent::Final("hello there".into()));
        });
        Ok(Box::new(ConnectsLateSession {
            events: rx,
            _worker: worker,
        }))
    }
}

impl Session for ConnectsLateSession {
    fn push(&mut self, _pcm: &[i16]) -> anyhow::Result<()> {
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.events
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Streams words, then answers the finalise with an error instead of a
/// transcript — an engine failure with nothing wrong on the app's side of the
/// hold, so `finish`'s own salvage is the only thing that can name it.
struct ErrorsOnFinishEngine;

struct ErrorsOnFinishSession {
    tx: Sender<TranscriptEvent>,
    rx: Receiver<TranscriptEvent>,
}

impl Engine for ErrorsOnFinishEngine {
    fn name(&self) -> &'static str {
        "errors-on-finish"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        tx.send(TranscriptEvent::Partial("hello there".into()))
            .unwrap();
        Ok(Box::new(ErrorsOnFinishSession { tx, rx }))
    }
}

impl Session for ErrorsOnFinishSession {
    fn push(&mut self, _pcm: &[i16]) -> anyhow::Result<()> {
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.rx
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        self.tx
            .send(TranscriptEvent::Error("the socket died finalising".into()))?;
        Ok(())
    }
}

/// Never concludes, and asks for a much shorter wait than the streaming
/// default — the shape of an engine that knows its own finalise cost.
struct ImpatientEngine;

impl Engine for ImpatientEngine {
    fn name(&self) -> &'static str {
        "impatient"
    }
    fn final_timeout(&self) -> Duration {
        Duration::from_millis(200)
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        Ok(Box::new(NeverConcludesSession {
            events: rx,
            _keep: tx,
        }))
    }
}

/// As [`PushFailsMidHoldEngine`], but it streams a partial first — so the hold
/// dies with words already on the user's screen.
struct PartialThenPushFailsEngine;

struct PartialThenPushFailsSession {
    events: Receiver<TranscriptEvent>,
    tx: Sender<TranscriptEvent>,
    samples: usize,
}

impl Engine for PartialThenPushFailsEngine {
    fn name(&self) -> &'static str {
        "partial-then-push-fails"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        Ok(Box::new(PartialThenPushFailsSession {
            events: rx,
            tx,
            samples: 0,
        }))
    }
}

impl Session for PartialThenPushFailsSession {
    fn push(&mut self, pcm: &[i16]) -> anyhow::Result<()> {
        let before = self.samples;
        self.samples += pcm.len();
        if before < 4_000 && self.samples >= 4_000 {
            let _ = self.tx.send(TranscriptEvent::Partial("hello there".into()));
        }
        if self.samples >= 8_000 {
            anyhow::bail!("the websocket died mid-hold");
        }
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.events
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Emits a premature Final after the first frames, then drops the event
/// sender — the Deepgram pump-exit shape that used to make the hold loop treat
/// channel disconnect as a key-up and inject only the first word.
struct PrematureFinalEngine;

struct PrematureFinalSession {
    tx: Option<Sender<TranscriptEvent>>,
    rx: Receiver<TranscriptEvent>,
    samples: usize,
    fired: bool,
}

impl Engine for PrematureFinalEngine {
    fn name(&self) -> &'static str {
        "premature-final"
    }
    fn open(&self) -> anyhow::Result<Box<dyn Session>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(TranscriptEvent::Connected).unwrap();
        Ok(Box::new(PrematureFinalSession {
            tx: Some(tx),
            rx,
            samples: 0,
            fired: false,
        }))
    }
}

impl Session for PrematureFinalSession {
    fn push(&mut self, pcm: &[i16]) -> anyhow::Result<()> {
        self.samples += pcm.len();
        if !self.fired && self.samples >= 3_200 {
            self.fired = true;
            if let Some(tx) = &self.tx {
                let _ = tx.send(TranscriptEvent::Partial("hello".into()));
                let _ = tx.send(TranscriptEvent::Final("hello".into()));
            }
            // Pump exit: dropping the sender disconnects the event channel.
            self.tx = None;
        }
        Ok(())
    }
    fn events(&self) -> &Receiver<TranscriptEvent> {
        &self.rx
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        // No further terminal event — dictation must salvage the partial.
        Ok(())
    }
}

#[test]
fn engine_disconnect_mid_hold_waits_for_key_up_and_keeps_the_words() {
    // Regression: event-channel disconnect used to break the capture loop as if
    // the key were released, finalising on the first segment while the user was
    // still holding. The loop must wait for the real key-up; dictation salvages
    // the richest partial so words are not silently dropped.
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("config.toml");
    let mut config = Config::default();
    config.polish.llm = false;
    let audio = ChannelAudio::new();
    let frames = audio.sender();
    let armed = audio.armed();
    let injector = Arc::new(RecordingInjector::new());
    let pill = RecordingPill::new();
    let mut app = App::new(
        config,
        &config_path,
        audio,
        injector.clone() as Arc<dyn Injector>,
        Box::new(pill.clone()),
    )
    .expect("app")
    .with_final_timeout(Duration::from_millis(300));
    app.set_engine(Arc::new(PrematureFinalEngine));

    let (keys_tx, keys_rx) = crossbeam_channel::unbounded();
    let app_frames = app.frames();

    let speaker = std::thread::spawn(move || {
        armed.recv().expect("arm");
        // More than enough audio that a mid-hold disconnect (at ~0.2 s of
        // samples) would under-count if the loop treated it as key-up.
        for chunk in speech().chunks(320) {
            frames.send(chunk.to_vec()).expect("frame");
        }
        while !frames.is_empty() {
            std::thread::sleep(Duration::from_millis(1));
        }
        // Deliberate pause after engine would have disconnected: proves the
        // loop is still in the hold, waiting on the real key-up.
        std::thread::sleep(Duration::from_millis(80));
        keys_tx
            .send(HotkeyEvent::Up(Instant::now()))
            .expect("key up");
    });

    let dictated = app
        .dictate(Instant::now(), &app_frames, &keys_rx)
        .expect("dictation should salvage the partial");
    speaker.join().expect("speaker");

    // Rule polish capitalises and punctuates; the raw words must still be there.
    assert!(
        dictated.record.text.to_ascii_lowercase().contains("hello"),
        "premature Final text must still be recovered, got {:?}",
        dictated.record.text
    );
    assert!(
        dictated.record.latency.audio_secs > 0.8,
        "must keep capturing until key-up, not stop at engine disconnect; got {:.2}s",
        dictated.record.latency.audio_secs
    );
    assert!(
        dictated.record.injected,
        "salvaged words must still be injected"
    );
    assert_eq!(injector.inserted().len(), 1);
}

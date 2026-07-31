//! End-to-end tests over the public API, with no microphone, no key and no
//! network — the committed WAV fixture stands in for a person speaking.
//!
//! These are the tests that guard the product claim: that streaming while
//! speaking makes the wait after key-release independent of how long the
//! utterance was.

use std::time::Duration;

use iris_core::audio;
use iris_core::dictation::{run_offline, Pace};
use iris_core::engine::{self, EngineOptions, EngineSpec, MockConfig, MockEngine};
use iris_core::latency::Mark;

fn fixture() -> Vec<i16> {
    audio::read_wav(iris_core::sample_wav_path()).expect("committed speech fixture")
}

#[test]
fn the_fixture_streams_through_the_mock_engine_to_a_transcript() {
    let engine = engine::build(EngineSpec::Mock, &EngineOptions::default()).unwrap();

    let mut partials = Vec::new();
    let outcome = run_offline(&*engine, &fixture(), Pace::Fast, &mut |p| {
        partials.push(p.to_string())
    })
    .expect("dictation");

    assert!(!outcome.text.trim().is_empty());
    assert!(
        partials.len() > 5,
        "expected interim transcripts while speaking, got {}",
        partials.len()
    );
    assert!((outcome.timeline.audio_secs - 5.38).abs() < 0.1);
}

#[test]
fn perceived_latency_does_not_grow_with_utterance_length() {
    // The whole thesis in one assertion. A record-then-transcribe design would
    // show the long utterance costing proportionally more after key-release.
    let engine = MockEngine::new(MockConfig::default());
    let pcm = fixture();
    let short = &pcm[..pcm.len() / 4];

    let long_wait = run_offline(&engine, &pcm, Pace::Fast, &mut |_| {})
        .unwrap()
        .timeline
        .perceived()
        .unwrap();
    let short_wait = run_offline(&engine, short, Pace::Fast, &mut |_| {})
        .unwrap()
        .timeline
        .perceived()
        .unwrap();

    assert!(
        long_wait < Duration::from_millis(50) && short_wait < Duration::from_millis(50),
        "streaming should make the post-release wait independent of length: \
         {long_wait:?} for 5.4 s vs {short_wait:?} for 1.3 s"
    );
}

#[test]
fn a_modelled_cloud_engine_stays_within_the_latency_target() {
    // 120 ms to connect, 160 ms to flush: a plausible streaming profile. The
    // connect cost must be hidden behind speech and only the flush felt.
    let engine =
        MockEngine::simulating_cloud(Duration::from_millis(120), Duration::from_millis(160));
    let outcome = run_offline(&engine, &fixture(), Pace::Realtime, &mut |_| {}).unwrap();
    let t = &outcome.timeline;

    let perceived = t.perceived().unwrap();
    assert!(
        perceived < Duration::from_millis(300),
        "perceived latency {perceived:?} missed the 300 ms target"
    );

    // The connection came up long before the key was released, so the user
    // never waited for it.
    let ready = t.at(Mark::StreamReady).unwrap();
    assert!(
        ready < t.at(Mark::KeyUp).unwrap(),
        "connection setup should overlap with speech, not delay the result"
    );
}

#[test]
fn realtime_pacing_actually_tracks_the_wall_clock() {
    // If this regressed, every latency number the harness prints would be
    // optimistic: a streaming engine would get to work ahead of the speaker.
    let engine = MockEngine::new(MockConfig::default());
    let pcm = fixture();
    let expected = audio::secs(pcm.len());

    let started = std::time::Instant::now();
    run_offline(&engine, &pcm, Pace::Realtime, &mut |_| {}).unwrap();
    let elapsed = started.elapsed().as_secs_f64();

    assert!(
        (elapsed - expected).abs() < 0.5,
        "{expected:.2} s of audio should take about that long to stream, took {elapsed:.2} s"
    );
}

#[test]
fn cloud_engines_refuse_to_run_without_a_key_rather_than_hanging() {
    // CI has no keys; the failure must be immediate and explain itself.
    for (spec, var) in [
        (EngineSpec::Deepgram, "IRIS_DEEPGRAM_KEY"),
        (EngineSpec::Groq, "IRIS_GROQ_KEY"),
    ] {
        if std::env::var(var).is_ok() {
            continue; // a developer machine with real credentials
        }
        let err = match engine::build(spec, &EngineOptions::default()) {
            Ok(_) => panic!("{spec} built without {var} set"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains(var), "{spec}: unhelpful error: {err}");
    }
}

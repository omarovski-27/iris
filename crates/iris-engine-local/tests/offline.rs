//! Offline unit/integration tests — no model download, no network, no native features.

use iris_engine_local::audio::{read_wav_pcm16, write_wav_pcm16, SAMPLE_RATE};
use iris_engine_local::engine::{LocalEngine, LocalEvent};
use iris_engine_local::finalizer::{BatchFinalizer, MockFinalizer};
use iris_engine_local::layered::{run_layered, LayeredLocalEngine};
use iris_engine_local::mock::{MockLocalEngine, MockLocalEngineConfig, DEFAULT_TRANSCRIPT};
use iris_engine_local::models::{ModelCatalog, ModelId};
use iris_engine_local::{silence_fixture, speech_fixture};

#[test]
fn silence_fixture_committed_and_all_zeros() {
    let pcm = read_wav_pcm16(&silence_fixture()).expect("silence fixture");
    assert_eq!(pcm.len(), SAMPLE_RATE as usize / 2);
    assert!(pcm.iter().all(|&s| s == 0));
}

#[test]
fn mock_finalizer_empty_on_pure_silence() {
    let f = MockFinalizer::default();
    let pcm = read_wav_pcm16(&silence_fixture()).unwrap();
    assert_eq!(f.transcribe(&pcm).unwrap(), "");
    assert_eq!(f.transcribe(&[]).unwrap(), "");
    assert_eq!(f.transcribe(&vec![0i16; 16000]).unwrap(), "");
}

#[test]
fn layered_engine_empty_transcript_on_silence_fixture() {
    let engine = LayeredLocalEngine::mock();
    let pcm = read_wav_pcm16(&silence_fixture()).unwrap();
    let (final_text, _, _) = run_layered(&engine, &pcm, 800).unwrap();
    assert_eq!(
        final_text, "",
        "transcript of record must be empty on pure silence"
    );
}

#[test]
fn layered_engine_partials_and_final_on_speech() {
    let engine = LayeredLocalEngine::mock();
    let pcm = read_wav_pcm16(&speech_fixture()).unwrap();
    let (final_text, partials, timings) = run_layered(&engine, &pcm, 1600).unwrap();
    assert!(
        !partials.is_empty(),
        "expected streaming partials on speech fixture"
    );
    assert!(!final_text.is_empty());
    assert!(
        timings.finalize_ms.unwrap_or(u64::MAX) < 100,
        "mock finalization should be << 100 ms, got {:?}",
        timings.finalize_ms
    );
}

#[test]
fn mock_engine_feed_arbitrary_frame_sizes() {
    let engine = MockLocalEngine::new(MockLocalEngineConfig::default());
    let mut session = engine.start().unwrap();
    // Odd sizes including empty.
    session.feed(&[]).unwrap();
    session.feed(&[1, 2, 3]).unwrap();
    session.feed(&vec![0i16; 1]).unwrap();
    session.feed(&vec![100i16; 777]).unwrap();
    session.feed(&vec![0i16; 16_000]).unwrap();
    session.finalize().unwrap();
    let got_final = session.partials().try_iter().any(|e| matches!(e, LocalEvent::Final(_)));
    assert!(got_final);
}

#[test]
fn catalog_documents_disk_costs() {
    let zip_total: u64 = ModelCatalog::zipformer_set()
        .iter()
        .map(|id| ModelCatalog::get(*id).expected_bytes)
        .sum();
    // Report: ~71 MB Zipformer int8.
    assert!(
        (60_000_000..90_000_000).contains(&zip_total),
        "zipformer total {zip_total}"
    );

    let whisper = ModelCatalog::get(ModelId::WhisperBaseEnQ5_1);
    assert!(
        (50_000_000..70_000_000).contains(&whisper.expected_bytes),
        "base.en size"
    );

    let parakeet = ModelCatalog::get(ModelId::ParakeetTdt06bQ8_0);
    assert!(parakeet.expected_bytes > 500_000_000);
    assert!(parakeet.disk_note.contains("638") || parakeet.disk_note.contains("Parakeet"));
}

#[test]
fn write_and_roundtrip_wav() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roundtrip.wav");
    let pcm = vec![0i16, 100, -100, 0, 200];
    write_wav_pcm16(&path, &pcm).unwrap();
    let back = read_wav_pcm16(&path).unwrap();
    assert_eq!(back, pcm);
}

#[test]
fn mock_transcript_constant() {
    assert!(!DEFAULT_TRANSCRIPT.is_empty());
}

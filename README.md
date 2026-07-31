# Iris

**Fast, minimal, open-source voice dictation.** Hold a key, speak, release — your words appear in whatever app you're using. Named for the Greek goddess of the rainbow, the swift messenger of the gods.

Iris is built around one obsession: **latency and smoothness**. Audio is transcribed while you speak, so text lands the instant you stop.

## Goals

- **Fast** — streaming transcription; target well under 300 ms from key-release to text.
- **Smooth** — instant visual feedback, fluid overlay, zero jank.
- **Minimal** — a pill, a tray icon, a settings window. Nothing else.
- **Yours** — pluggable engines: bring a cloud API key for maximum speed, or run a local model for full privacy. Open source, MIT.

## Status

Early development. Windows first; macOS and Linux planned.

`iris` (`crates/iris-app`) is the resident tray app: hold a hotkey, speak,
release — the transcript is polished and injected into the focused window. The
latency spike and harness that proved the pipeline live in `crates/iris-spike`.
The pill overlay crate (`iris-overlay`) is in; app wiring that drives it from
the live pipeline is not yet.

```bash
# Full loop offline — no API key, no microphone, no injection
cargo run -p iris-app -- --speak-wav assets/speech-16k.wav

# Latency harness
cargo run --release --bin iris-harness -- --engine mock
```

## Layout

| | |
| --- | --- |
| `crates/iris-app` | **the application**: `iris`, the resident tray app |
| `crates/iris-core` | the pipeline: audio, the `Engine` trait, injection, latency |
| `crates/iris-polish` | transcript cleanup: rule engine + LLM, deadline-bounded |
| `crates/iris-engine-local` | on-device ASR: streaming Zipformer + Whisper finalizer |
| `crates/iris-spike` | latency spike (`iris-spike`) and harness (`iris-harness`) |
| `crates/iris-overlay` | the pill HUD (always-on-top, click-through, never-activating) |

Everything except microphone capture, the hotkey hook, text injection, and the
overlay window is platform-independent, so the tests and the latency harness
run anywhere.

## Docs

- [`crates/iris-app/README.md`](crates/iris-app/README.md) — the app: the
  dictation loop, configuration, the tray, the session log
- [`crates/iris-spike/README.md`](crates/iris-spike/README.md) — running the
  spike, and how to read the latency report
- [`crates/iris-overlay/README.md`](crates/iris-overlay/README.md) — the pill
  overlay API, design lock, and demo
- [`docs/spike-findings.md`](docs/spike-findings.md) — measured latency, where
  the budget goes, architecture recommendation
- [`docs/dev-windows.md`](docs/dev-windows.md) — building for Windows from WSL2

## License

MIT

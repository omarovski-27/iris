# Iris

**Fast, minimal, open-source voice dictation.** Hold a key, speak, release — your words appear in whatever app you're using. Named for the Greek goddess of the rainbow, the swift messenger of the gods.

Iris is built around one obsession: **latency and smoothness**. Audio is transcribed while you speak, so text lands the instant you stop.

## Goals

- **Fast** — streaming transcription; target well under 300 ms from key-release to text.
- **Smooth** — instant visual feedback, fluid overlay, zero jank.
- **Minimal** — a pill, a tray icon, a settings file. Nothing else.
- **Yours** — pluggable engines: bring a cloud API key for maximum speed, or run a local model for full privacy. Open source, MIT.

## Quickstart

Windows first. From WSL2 (or native Windows with the MSVC/gnu toolchain — see [`docs/dev-windows.md`](docs/dev-windows.md)):

```bash
# Build the Windows binary
cargo build --release --target x86_64-pc-windows-gnu -p iris-app

# Run it (WSL can launch the .exe as a real Windows process)
./target/x86_64-pc-windows-gnu/release/iris.exe
```

First dictation:

1. A tray icon appears (prism triangle). Right-click for engine / theme / polish.
2. Hold **Right-Ctrl**, speak, release.
3. The Prism pill appears bottom-centre while you talk; text is polished and injected into the focused window.
4. Session history lands in `history.jsonl` beside the config.

### Config location

| Platform | Path |
| --- | --- |
| Windows | `%APPDATA%\iris\config.toml` |
| Linux / macOS | `$XDG_CONFIG_HOME/iris/config.toml` (or `~/.config/iris/config.toml`) |

Override with `--config <path>` or `IRIS_CONFIG`.

### API keys / local engine

```bash
# Cloud streaming (Deepgram) or batch (Groq Whisper)
export IRIS_DEEPGRAM_KEY=…
export IRIS_GROQ_KEY=…
# Optional LLM polish (falls back to the offline rule engine)
export IRIS_LLM_KEY=…

# Or put them under [keys] in config.toml — promoted into the env at startup.
```

Set `engine = "deepgram"` / `"groq"` / `"mock"` / `"local"` in the config (or `--engine`).

`local` needs `--features local-native` and a native Windows build (sherpa/whisper do not cross-compile from WSL). See [`crates/iris-engine-local/README.md`](crates/iris-engine-local/README.md).

### Offline smoke (any platform)

```bash
# One full loop: mock engine, dry-run inject, real pill adapter (headless off Windows)
cargo run -p iris-app -- --demo-dictation

# Same loop driven by a WAV file
cargo run -p iris-app -- --speak-wav assets/speech-16k.wav

# Latency harness
cargo run --release --bin iris-harness -- --engine mock
```

`--demo-dictation` and `--speak-wav` never use live `SendInput` unless you pass `--really-inject` (Windows only, and only with `--speak-wav`).

## Status

Windows first; macOS and Linux planned for the resident hotkey / mic / inject path. The pipeline, polish, overlay state machine, and session log are portable and CI-tested on Linux.

## Layout

| | |
| --- | --- |
| `crates/iris-app` | **the application**: `iris`, the resident tray app |
| `crates/iris-core` | the pipeline: audio, the `Engine` trait, injection, latency |
| `crates/iris-polish` | transcript cleanup: rule engine + LLM, deadline-bounded |
| `crates/iris-engine-local` | on-device ASR: streaming Zipformer + Whisper finalizer |
| `crates/iris-spike` | latency spike (`iris-spike`) and harness (`iris-harness`) |
| `crates/iris-overlay` | the Prism/Porcelain pill HUD |

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

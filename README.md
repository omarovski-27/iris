# Iris

**Fast, minimal, open-source voice dictation.** Hold a key, speak, release — your words appear in whatever app you're using. Named for the Greek goddess of the rainbow, the swift messenger of the gods.

Iris is built around one obsession: **latency and smoothness**. Audio is transcribed while you speak, so text lands the instant you stop.

## Goals

- **Fast** — streaming transcription; target well under 300 ms from key-release to text.
- **Smooth** — instant visual feedback, fluid overlay, zero jank.
- **Minimal** — a pill, a tray icon, one small settings window over a plain config file. Nothing else.
- **Yours** — pluggable engines: bring a cloud API key for maximum speed, or run a local model for full privacy. Open source, MIT.

## Install (Windows)

For someone who just wants to run Iris — no Rust, no build tools: get
`iris-<version>-windows-x64.zip` (build one yourself with
`scripts/package-windows.sh`, or take one someone already built), extract it,
and follow the `README.md` inside.

That document —
[`packaging/windows/README.md`](packaging/windows/README.md), staged into the
zip — is the single source of truth for the end-user path: the SmartScreen
"Windows protected your PC" prompt, running `install.ps1` (and its `-Desktop`
/ `-RunAtLogin` flags), where `%APPDATA%\iris\config.toml` lives, and how to
switch off the mock engine and add a Deepgram or Groq key. A recipient never
needs this repository. The rest of this README is the developer path.

### Why a zip and not an installer

Building an MSI/EXE installer needs either MSVC or the WiX toolset, both of
which pull in tooling this project deliberately avoids (see
[`docs/dev-windows.md`](docs/dev-windows.md) "Why gnu and not msvc") — the
whole point of the `x86_64-pc-windows-gnu` target is a cross-compile that
needs nothing but `mingw-w64`. A portable zip plus a per-user PowerShell
installer gets a Start Menu entry and a real `%LOCALAPPDATA%` install without
trading that away. A native Windows build (real MSVC on the machine, not
cross-compiled) would unlock a proper MSI if that's ever wanted.

### Building the zip yourself

```bash
scripts/package-windows.sh              # writes dist/iris-<version>-windows-x64.zip
scripts/package-windows.sh /some/dir     # or pick the output directory
```

Same toolchain as building from source below — nothing extra to install.

## Quickstart (build from source)

Windows first. From WSL2 (or native Windows with the MSVC/gnu toolchain — see [`docs/dev-windows.md`](docs/dev-windows.md)):

```bash
# Build the Windows binary
cargo build --release --target x86_64-pc-windows-gnu -p iris-app

# Run it (WSL can launch the .exe as a real Windows process)
./target/x86_64-pc-windows-gnu/release/iris.exe
```

First dictation:

1. A tray icon appears (prism triangle). Right-click for engine / theme / polish, or "Open settings…" for the settings window (history, settings, insights).
2. Hold **Right-Ctrl**, speak, release.
3. The Prism pill appears bottom-centre while you talk — a quiet glass capsule that pulses with your voice (set `show_live_text = true` to also open a ribbon showing your words as they are heard); text is polished and injected into the focused window.
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

Only the OS-bound layer is Windows-only — see
[`docs/dev-windows.md`](docs/dev-windows.md) for exactly which parts. The rest is
platform-independent, so the tests and the latency harness run anywhere.

## Docs

- [`crates/iris-app/README.md`](crates/iris-app/README.md) — the app: the
  dictation loop, configuration, tray, overlay adapter, settings window,
  session log
- [`crates/iris-spike/README.md`](crates/iris-spike/README.md) — running the
  spike, and how to read the latency report
- [`crates/iris-overlay/README.md`](crates/iris-overlay/README.md) — the pill
  overlay API, the design record, and the demo
- [`docs/spike-findings.md`](docs/spike-findings.md) — measured latency, where
  the budget goes, architecture recommendation
- [`docs/dev-windows.md`](docs/dev-windows.md) — building for Windows from WSL2
- [`docs/first-run-checklist.md`](docs/first-run-checklist.md) — what to
  verify by eye on a real Windows machine after installing a new build

## License

MIT

# Iris

**Fast, minimal, open-source voice dictation for Windows.** Hold a key, speak,
release, and Iris types into the app you were already using.

Iris is built around streaming dictation: audio is sent while you speak, so the
post-release wait depends mostly on the engine's final flush instead of the
length of the utterance.

## What it does

- **Streaming dictation** — CI guards a modelled sub-300 ms key-release path;
  real cloud latency depends on your engine, network, and target app.
- **Push-to-talk or hands-free** — hold Right-Ctrl, or double-tap it to latch recording on.
- **Small desktop surface** — tray icon, pill overlay, and one settings window.
- **Engine choice** — mock, Deepgram, Groq, and an experimental local path.
- **Plain files** — config and history live in your user profile. Iris has no
  account system; cloud engines use your provider API key.

## Install (Windows)

If you just want to run Iris, download `iris-<version>-windows-x64.zip` from
the [latest release](https://github.com/omarovski-27/iris/releases/latest),
extract it, and follow the `README.md` inside. No Rust or build tools needed.

The zip README is staged from
[`packaging/windows/README.md`](packaging/windows/README.md). It covers
SmartScreen, `install.ps1`, optional Desktop / run-at-login shortcuts, the
config path, and adding a Deepgram or Groq key. This repository README is for
development.

### Why a zip and not an installer

Building an MSI/EXE installer needs either MSVC or the WiX toolset, both of
which pull in tooling this project avoids (see
[`docs/dev-windows.md`](docs/dev-windows.md) "Why gnu and not msvc"). The
`x86_64-pc-windows-gnu` target keeps the release path to a `mingw-w64`
cross-compile. The zip plus per-user PowerShell installer still gives users a
Start Menu entry and a real `%LOCALAPPDATA%` install. A native Windows build
with MSVC could support an MSI later.

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

First run:

1. A tray icon appears (prism triangle). Right-click for engine / theme / polish, or "Open settings…" for the settings window (history, settings, insights).
2. Hold **Right-Ctrl**, speak, release.
3. The pill overlay appears bottom-centre while you talk. Set `show_live_text = true` to show live transcript text in the overlay.
4. Session history lands in `history.jsonl` beside the config.

### Hands-free dictation

Double-tap **Right-Ctrl** (within 400 ms) and Iris keeps listening with the key
up. Tap **Right-Ctrl** once more to stop; the text is inserted exactly as it
would be on a normal release. While latched, the pill's core dot changes colour
and gains a ring around it, so a live microphone is obvious at a glance even
from across the room. If you forget to stop it, Iris stops itself after 5
minutes and inserts what it captured. Hold-to-talk is unchanged if you never
double-tap.

### Config location

| Platform | Path |
| --- | --- |
| Windows | `%LOCALAPPDATA%\IrisConfig\iris\config.toml` |
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

### Custom vocabulary

Names, jargon, and acronyms Iris keeps mishearing go in the Settings window's
**Vocabulary** card — one term per line — or directly in `config.toml` as
`vocabulary = ["Term", "Another term"]`. Deepgram gets these as `keyterm`
hints (nova-3's keyterm prompting); Groq and the local Whisper engine have no
keyword-list mechanism, so they get the list folded into their one initial
prompt instead. Empty by default, which changes nothing; a very long list is
trimmed to whatever the active engine allows rather than failing the
dictation.

### Deepgram balance (optional)

If you use your own Deepgram balance, Settings can show what's left and warn
you once before it runs out. This needs a **second**, separate key for
Deepgram's Management API (`billing:read` scope, an Admin- or Owner-role key);
the ordinary transcription key cannot read billing data.

```bash
export IRIS_DEEPGRAM_MANAGEMENT_KEY=…
# or, under the same [keys] table in config.toml:
#   deepgram_management = "…"
```

Leave it unset and nothing changes. With it set, Iris checks on startup and
every few hours, shows the balance and last check time in Settings, and warns
once when it drops to $5 or below. Get a key with the right scope at
<https://console.deepgram.com>.

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

Iris source code is MIT licensed. Bundled third-party assets and optional
engine dependencies keep their own licenses; see
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

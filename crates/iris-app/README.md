# iris-app

**Iris**, the application: a resident tray app where you hold a key, speak,
release, and the text appears in whatever you were typing in.

This crate owns no algorithms. Capture, transcription, injection and latency
instrumentation are [`iris-core`](../iris-core); transcript cleanup is
[`iris-polish`](../iris-polish); offline ASR is
[`iris-engine-local`](../iris-engine-local). What lives here is the product:
the loop that holds them together, the settings, the tray, the session log.

```bash
# Windows (the real thing)
cargo build --release --target x86_64-pc-windows-gnu -p iris-app
./target/x86_64-pc-windows-gnu/release/iris.exe

# Anywhere (no microphone, no hotkey, no injection)
cargo run -p iris-app -- --speak-wav ../../assets/speech-16k.wav
cargo run -p iris-app -- --history
```

## The loop

```text
   hold Right-Ctrl ─► capture ─► Engine (streams while you speak)
                                       │
   release ────────────────────────────┴─► final transcript
                                            │
                                            ├─► polish   (LLM, rule fallback, 150 ms budget)
                                            ├─► inject   (SendInput / clipboard)
                                            ├─► overlay  (PillSink: inserted → hide)
                                            └─► session log (jsonl)
```

Everything left of the release happens while the user is still speaking and is
therefore free — that is the whole thesis, measured in
[`docs/spike-findings.md`](../../docs/spike-findings.md). Everything right of it
is the latency budget, which is why polish is deadline-bounded and why the log
is written *after* the text has been injected.

Threads: the hotkey hook and the tray each own one and do nothing on it but pump
Windows messages (a low-level hook whose callback takes >300 ms is silently
uninstalled); WASAPI owns the audio callback; the main thread owns the engine
session, injection and the log. Nothing is shared behind a lock.

## Configuration

`iris/config.toml` in the platform config directory (`%APPDATA%` on Windows,
`$XDG_CONFIG_HOME` on Unix). `--config <path>` or `IRIS_CONFIG` moves it.

```toml
engine = "mock"           # mock | deepgram | groq | local
hotkey = "right-ctrl"     # rctrl, lctrl, rshift, ralt, rwin, capslock, ...
suppress_hotkey = true    # stop the hotkey reaching the focused app
theme = "dark"            # dark | light

[polish]
enabled = true
llm = true                # false pins the offline rule engine
budget_ms = 150           # the longest the user ever waits for cleanup
style = "prose"           # prose | message | technical

[audio]
device = "Yeti"           # substring of the device name; omit for the default
warm = true               # keep the mic stream open (opening it costs ~30 ms)

[inject]
method = "sendinput"      # sendinput | clipboard
trailing_space = true

[history]
enabled = true
max_entries = 500

[keys]                    # optional; the environment always wins
groq = "gsk_..."
```

**Keys.** `IRIS_DEEPGRAM_KEY`, `IRIS_GROQ_KEY` and `IRIS_LLM_KEY` take
precedence over the file. Keys in the file are copied into the environment at
startup, before any thread exists, because the engine and polisher constructors
upstream read them from the environment only — deliberately, so a key cannot end
up in a shell history. `Keys` has a hand-written `Debug` that redacts, so a
stray `{:?}` on the config cannot leak one, and the config Iris writes itself
contains no `[keys]` section at all.

## Tray

`tray-icon` 0.24 (+ `muda` for the menu). Chosen because it is the maintained
extraction of Tauri's tray, it is pure Rust over Win32 — no C toolchain — so it
cross-compiles to `x86_64-pc-windows-gnu`, which is how this project builds from
WSL, and it drags in no windowing framework. `systray` is unmaintained (2021),
`trayicon` has no submenus, and pulling in `tao`/`winit` for their event loop
would put a UI framework in a crate whose only UI is meant to be the tray. On
Linux `tray-icon` needs GTK, so the dependency is `[target.'cfg(windows)']` and
the tray is simply absent elsewhere.

Menu: engine picker, microphone picker, theme, polish toggle, open settings,
reload settings, quit. "Open settings" opens `config.toml` in the user's editor
— the file is already the source of truth and is commented; a bespoke settings
window would be the same thing built twice.

The icon is drawn in code (`tray::icon_rgba`) rather than shipped as a `.ico`,
so there is no binary asset to keep in step with the theme and no file to fail
to find next to the `.exe`.

## Overlay seam

`iris-overlay` is being built in parallel. The loop drives a `PillSink`, whose
five methods mirror that crate's handle API exactly:

```rust
show_listening() → update_level(f32)* → processing() → inserted() → hide()
```

`hide()` runs on every path, including every failure, so a sink never needs a
timeout to clean up, and `inserted()` runs only when text actually reached the
window. Two implementations ship today — `NoopPill` (default) and `LogPill`
(`--verbose`) — plus `RecordingPill` for tests. The adapter to the real overlay
is a new type in `pill.rs` that forwards each method; no change to the loop.

## Session log

`history.jsonl` beside the config, one record per dictation, newest last, capped
at `max_entries`:

```json
{"timestamp":"2026-07-31T06:27:17Z","engine":"mock","text":"…","injected":true,
 "latency":{"final_transcript_ms":0.1,"polish_ms":0.2,"perceived_ms":0.4,"audio_secs":5.4,"partials":15}}
```

**Every** dictation is recorded, including the ones where injection failed —
that record is the user's only way to recover words that never made it onto the
screen. `iris --history` prints the tail of it.

## Engines

| `engine` | Needs | Notes |
|---|---|---|
| `mock` | nothing | deterministic, offline, instant; the default |
| `deepgram` | `IRIS_DEEPGRAM_KEY` | streaming; hides its latency behind speech |
| `groq` | `IRIS_GROQ_KEY` | batch on key-release; cannot hide latency |
| `local` | `--features local-native` | on-device; see below |

`local` wraps `iris-engine-local` through `engines::LocalAdapter`, the one-file
mapping that crate's README predicts (`start`/`open`, `feed`/`push`,
`partials`/`events`, `finalize`/`finish`). The one substantive difference is
that `finalize` may block on Whisper's batch pass while `Session::finish` must
not, so the adapter moves the session to a thread and lets the transcript arrive
as an event.

It is behind a feature because the native engines (sherpa-onnx, whisper.cpp) do
not cross-compile to `x86_64-pc-windows-gnu` from WSL. Without the feature,
selecting `local` fails with a message that says exactly that; it never
quietly substitutes the mock, because that would be a lie about where the user's
audio goes.

## Testing

```bash
cargo test -p iris-app                                       # Linux, offline, no network
cargo check -p iris-app --all-targets --target x86_64-pc-windows-gnu
```

The whole state machine is exercised on Linux: `tests/loop.rs` drives the real
`App` with a channel instead of a microphone, a mock engine instead of a
network, and a `RecordingInjector` instead of `SendInput`.

> **Injection is never executed by a test, in CI, or in a loop.** Windows
> delivers synthetic keystrokes to the *input desktop* — the one the user is
> looking at. There is no sandbox: an automated injection test types into
> whoever is using the machine, and it has already disrupted real work once on
> this project. `SystemInjector` is constructed in `main` and nowhere else; the
> `Injector` trait exists to make that structural rather than a rule someone has
> to remember. Real typing is verified by a person running the app.

`--speak-wav <file>` runs one full dictation — engine, polish, session log,
latency report — from a WAV, with `--dry-run` injection, on any platform. It is
the portable way to see the loop work end to end.

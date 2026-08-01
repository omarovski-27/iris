# iris-app

**Iris**, the application: a resident tray app where you hold a key, speak,
release, and the text appears in whatever you were typing in.

This crate owns no algorithms. Capture, transcription, injection and latency
instrumentation are [`iris-core`](../iris-core); transcript cleanup is
[`iris-polish`](../iris-polish); offline ASR is
[`iris-engine-local`](../iris-engine-local). What lives here is the product:
the loop that holds them together, the settings, the tray, the session log.

```bash
# Windows (the real thing — tray, hotkey, mic, Prism pill, inject)
cargo build --release --target x86_64-pc-windows-gnu -p iris-app
./target/x86_64-pc-windows-gnu/release/iris.exe

# Anywhere (no microphone, no hotkey; dry-run inject; real pill adapter)
cargo run -p iris-app -- --demo-dictation
cargo run -p iris-app -- --speak-wav assets/speech-16k.wav
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
                                            ├─► overlay  (PillSink: inserted(ms) self-exits)
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
show_live_text = true     # false keeps the overlay a quiet orb, no transcript

[polish]
enabled = true
llm = true                # false pins the offline rule engine
budget_ms = 150           # the longest the user ever waits for cleanup
style = "prose"           # prose | message | technical

[audio]
device = "Yeti"           # substring of the device name; omit for the default
warm = true               # keep the mic stream open (opening it costs ~30 ms)

[inject]
method = "sendinput"      # sendinput | clipboard; long transcripts paste anyway
trailing_space = true

[history]
enabled = true
max_entries = 500

[keys]                    # optional; the environment always wins
groq = "gsk_..."
```

**Hotkey.** `ralt` and `rwin` are excluded from the stuck-hotkey correction
`inject.rs` applies before every injection burst for the other choices, so they
behave differently there. They are also the two that cannot receive a clipboard
paste while still held, because Ctrl+V becomes Ctrl+Alt+V or Ctrl+Win+V; Iris
types the transcript instead when that happens.

**Injection method — and your clipboard.** `method` is a request, not a
guarantee. A transcript longer than 256 characters (roughly 30 seconds of
speech) is delivered as a **clipboard paste even under `sendinput`**, because a
keystroke burst that long arrives garbled in some apps — a reported 313-character
dictation reached Notepad as a handful of correct characters followed by one
repeated key and a run of spaces, while the same text typed into a terminal
fine.

That paste **overwrites whatever was on your clipboard, and the previous
contents are not restored.** This is a deliberate trade, not an oversight:
restoring the old contents is unsound rather than merely awkward — Windows
offers no signal for "the target has finished reading the clipboard", so any
restore either races the paste (and the app silently pastes your *old*
clipboard, which looks like it worked) or needs a delay long enough to cost the
sub-second latency this app exists for and to race the next dictation. The full
reasoning is in `win::paste`'s doc comment in `iris-core/src/inject.rs` (commit
`e2aac70`). If you keep something on the clipboard you cannot lose, copy it back
after a long dictation, or keep dictations short.

That automatic escalation skips windows that do not treat Ctrl+V as paste —
terminals (including ConEmu/Cmder, Hyper and Tabby), Remote Desktop and VM
client windows, vim, Emacs. They keep the keystrokes, sent in smaller groups
with a short pause between them so a long transcript still arrives intact. The
same keystroke fallback catches a clipboard that another application is holding
open, and a hotkey still held down that would turn Ctrl+V into a different
shortcut. All three fallbacks are logged under `--verbose`.

Those pauses have a flip side worth knowing: anything *you* type during them
lands in the middle of the transcript. Starting your next dictation while a long
one is still being typed out is the likely way to see it. Long transcripts were
already split into several bursts before the pauses existed, so this widens the
window rather than creating it — and unlike the garbling it prevents, it is
visible on screen.

All of that applies to the automatic escalation only — that is, to the default
`method = "sendinput"`. Setting `method = "clipboard"` is your own choice and is
honoured as one: every dictation is pasted, at any length, into whatever window
has focus, including the paste-hostile ones above. Iris never overrides an
explicit method in either direction, so a `clipboard` config is not filtered
through that list.

**Where a pasted transcript can end up.** This one is not escalation-specific:
it applies to *every* paste Iris makes, whether you were escalated into it or
chose `method = "clipboard"` yourself. Clobbering is not the only cost of going
through the clipboard — anything on it can be picked up by other software, and a
dictation is not necessarily something you want kept. So Iris asks Windows to
keep the item out of **Clipboard History (Win+V)** and off **Cloud Clipboard
sync**, using the three registered formats Windows documents for exactly that
(`ExcludeClipboardContentFromMonitorProcessing`, `CanIncludeInClipboardHistory`
and `CanUploadToCloudClipboard`; see `decline_history_and_cloud_sync` in
`iris-core/src/inject.rs`). If you were relying on Win+V to get a dictation
back, use `iris --history` instead — see below. Two limits on the opt-out, both
real:

- It is a request to the system, not a guarantee about other programs. **A
  third-party clipboard manager is separate software that is free to ignore
  it** — if you run one, assume it captures anything Iris pastes, and check its
  own settings if that matters to you.
- Iris cannot verify it from inside the app. This path only runs during real
  injection, which this project does not execute unattended (see `CLAUDE.md`),
  so the opt-out is the documented Windows mechanism applied as documented,
  not something a test on your machine has confirmed.

The transcript still sits on the live clipboard afterwards either way — that is
deliberate, and it is the recovery path described next.

**If a long dictation does not appear, it is not lost.** That list of
paste-hostile apps is best-effort and always will be — it can only name
application families that are actually identifiable, and no list can cover
every app that binds Ctrl+V to something of its own. So the escalated paste is
built to fail recoverably rather than silently:

- **The text is still on your clipboard.** Iris does not restore the previous
  contents (see above), which means the transcript is sitting there — press
  whatever paste key that app *does* use.
- **The text is in the session log**, with `[history] enabled = true`. That log
  is the durable record of every dictation, recorded whether or not delivery
  worked — which matters here, because a paste into a misidentified app *is*
  reported as delivered. Run `iris --history` to print the last ten dictations,
  or `iris --history 50` for more; it ends with the log file's own path, so it
  is also how you find the file to copy from. A Settings window with a History
  tab that lists them with one-click copy is in development; until it lands,
  `--history` is the way in.

Note that "delivered" here only ever means the keystrokes or the paste
shortcut reached Windows' input queue. Neither Windows nor Iris can confirm
that the app on the other end rendered them correctly — that gap is exactly
what the original bug was — so the timing shown on the pill is a delivery
time, not a receipt.

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

The icon is the captain-locked **prism triangle** (spectrum wedge on a plate),
drawn in code (`tray::icon_rgba`) from the same mark as
`iris-overlay/assets/iris-prism.svg`. No binary `.ico` to keep in step with the
theme and no file to fail to find next to the `.exe`.

### Known limitations

The menu's check marks and the tooltip are fire-and-forget. `muda` check items
toggle their own checked state on click and the engine / microphone / theme
submenus are not radio groups, so after a switch the previously selected item
stays checked; a rejected switch (e.g. deepgram with no key, which the loop
rolls back) can leave the wrong item checked; and the tooltip shows the state
at startup only. The config file and the loop remain the source of truth — the
menu is a remote control, not a display. Reconciling it would need the item
handles kept on the tray thread plus a state-update message from the loop, a
deliberate non-goal for v1.

## Overlay

The loop drives a `PillSink`. On Windows the resident app spawns
`iris-overlay` at startup and feeds it through `OverlayPill`:

```rust
set_engine / set_theme                     // startup + tray
show_listening() → update_level* / set_partial_text*
  → processing() → inserted(latency_ms)    // success: pill auto-exits after ~550 ms
  → hide()                                 // cancel / empty / error only
```

`Theme::Dark` maps to Prism, `Theme::Light` to Porcelain. After a successful
insert the overlay holds the confirmation then exits itself — the loop does
**not** call `hide()` immediately, or the hold would be cancelled.

`set_partial_text` is what opens the overlay's live-transcript ribbon.
`show_live_text = false` in the config makes `OverlayPill` swallow it, so the
overlay never sees a partial and stays the quiet orb — the opt-out for anyone
who does not want dictated words on screen. It is pushed through
`PillSink::set_show_live_text` at startup and again on every "reload
settings", the same way the theme is, so editing it takes effect without a
restart.

| Sink | When |
|---|---|
| `OverlayPill` | Overlay started (Windows resident; also `--demo-dictation` / `--speak-wav`, headless off Windows) |
| `LogPill` | `--verbose` when the overlay did not start |
| `NoopPill` | Default when the overlay did not start |
| `RecordingPill` | tests |

`--demo-dictation` forces the mock engine, dry-run inject, synthetic levels, and
the real pill adapter (visible on Windows; headless state machine elsewhere).
**Never** constructs `SystemInjector`.

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

A record with an `error` carries a zeroed `latency` block (`App::dictate`'s
`Err` arm builds a fresh record rather than the timeline it was tracking), so
`"audio_secs":0.0` on an errored row is consistent with *some* audio having
been captured before the failure — it is not proof that none was.

## Engines

| `engine` | Needs | Notes |
|---|---|---|
| `mock` | nothing | deterministic, offline, instant; the default |
| `deepgram` | `IRIS_DEEPGRAM_KEY` | streaming; hides its latency behind speech, except for a hold too short to get a first result back (see below) |
| `groq` | `IRIS_GROQ_KEY` | batch on key-release; cannot hide latency |
| `local` | `--features local-native` | on-device; see below |

A hold shorter than Deepgram's connect-plus-first-result latency (~1-3 s) has
nothing streamed back by key-release, so its whole transcript rides on the
finalisation flush the engine waits for before closing the socket. That
invariant, and the measurements behind it, live in `AGENTS.md` and the
`iris-core::engine::deepgram` module doc.

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

`--demo-dictation` and `--speak-wav <file>` run one full dictation — engine,
polish, session log, latency report, pill adapter — with dry-run injection, on
any platform. They are the portable way to see the loop work end to end.
`--speak-wav` feeds the file at real-time speed (one frame per frame-length,
like a live microphone), so the run takes about as long as the WAV: bursting it
would finish the utterance before the key came up and hide the finalisation
race a held key is meant to exercise.

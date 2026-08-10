# iris-app

**Iris**, the application: a resident tray app where you hold a key, speak,
release, and the text appears in whatever you were typing in.

This crate owns no algorithms. Capture, transcription, injection and latency
instrumentation are [`iris-core`](../iris-core); transcript cleanup is
[`iris-polish`](../iris-polish); offline ASR is
[`iris-engine-local`](../iris-engine-local). What lives here is the product:
the loop that holds them together, the settings, the tray, the settings
window, the session log.

```bash
# Windows (the real thing — tray, hotkey, mic, Prism pill, inject)
cargo build --release --target x86_64-pc-windows-gnu -p iris-app
./target/x86_64-pc-windows-gnu/release/iris.exe

# Anywhere (no microphone, no hotkey; dry-run inject; real pill adapter)
cargo run -p iris-app -- --demo-dictation
cargo run -p iris-app -- --speak-wav assets/speech-16k.wav
cargo run -p iris-app -- --history
cargo run -p iris-app -- --demo-window
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
uninstalled); the settings window owns one more; WASAPI owns the audio callback;
the main thread owns the engine session, injection and the log. Nothing is
shared behind a lock. The full inventory, with each thread's rule, is the table
in `lib.rs`'s crate docs.

## Configuration

`iris/config.toml` in the platform config directory (`%LOCALAPPDATA%\IrisConfig`
on Windows — deliberately not Roaming, and not the bare `%LOCALAPPDATA%` root
either, since that would collide with `install.ps1`'s `%LOCALAPPDATA%\Iris`
binary directory on NTFS's case-insensitive names; see
`config::config_dir`'s doc comment — `$XDG_CONFIG_HOME` on Unix).
`--config <path>` or `IRIS_CONFIG` moves it. A pre-existing install's
`%APPDATA%\iris\config.toml`/`history.jsonl` are migrated to the new location
automatically on first launch; see `config::migrate_from_roaming`.

```toml
version = 1               # written by Iris, not a setting; see below
engine = "mock"           # mock | deepgram | groq | local
hotkey = "right-ctrl"     # rctrl, lctrl, rshift, ralt, rwin, capslock, ...
suppress_hotkey = true    # stop the hotkey reaching the focused app
theme = "dark"            # dark | light
show_live_text = false    # true opens a live-text ribbon showing dictated words
overlay_enabled = true    # false suppresses the overlay entirely

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

**`version`, and the one thing it does.** A schema stamp Iris writes, not a
setting to edit; a file without it predates the stamp. It exists for exactly
one decision: `show_live_text` shipped as `true` and every install that ran an
earlier build has that pinned on disk whether or not the user chose it, so the
first start after upgrading resets an unstamped `true` to `false` (the round-3
default — see [Overlay](#overlay)) and writes `version = 1`. That happens once
per install: a `true` set after the stamp is a real choice and survives. If
the rewrite fails, Iris says so — on the console when a terminal launched it,
in an "Iris could not update its settings" dialog otherwise (`main.rs`'s
`Config::load_or_create_reporting` call) — and keeps running, but the reset
will repeat on the next start until the stamp lands. `Config::migrate` in
`src/config.rs` is the implementation and carries the full reasoning.

**Hotkey.** `ralt` and `rwin` are excluded from the stuck-hotkey correction
`inject.rs` applies before every injection burst for the other choices, so they
behave differently there: they are also the two that cannot receive a clipboard
paste while still held, because Ctrl+V becomes Ctrl+Alt+V or Ctrl+Win+V; Iris
types the transcript instead when that happens. Rebinding the hotkey — from the
settings window or by hand — needs a restart: the hook is installed once in
`main`.

**Injection method — what `method` decides.** `method` is a request, not a
guarantee. Under the default `sendinput`, two things must *both* be true before
Iris pastes anything. The first is length: a transcript longer than 256
characters (roughly 30 seconds of speech) is delivered as a **clipboard paste
even under `sendinput`**, because a keystroke burst that long arrives garbled in
some apps — a reported 313-character dictation reached Notepad as a handful of
correct characters followed by one repeated key and a run of spaces, while the
same text typed into a terminal fine. The second is the window: that escalation
is skipped for targets that do not treat Ctrl+V as paste — terminals (including
ConEmu/Cmder, Hyper and Tabby), Remote Desktop and VM client windows, vim,
Emacs — which keep the keystrokes instead. Anything under the threshold, or
aimed at one of those windows, is typed, and your clipboard is never touched.

**`method = "clipboard"` drops both of those gates, not just the window list.**
Setting it is your own choice and is honoured as one, but it is a larger change
than "also paste into terminals": *every* dictation is pasted, at any length,
into whatever window has focus. A five-word dictation that the default would
have typed without going near your clipboard now clobbers it, and so does the
next one. Everything in the two sections below then applies to you exactly as it
applies to an automatically escalated dictation — it all lives in the paste
itself, not in the decision to paste.

**What every paste does, however Iris got there.** A paste **overwrites whatever
was on your clipboard, and the previous contents are not restored.** This is a
deliberate trade, not an oversight: restoring the old contents is unsound rather
than merely awkward — Windows offers no signal for "the target has finished
reading the clipboard", so any restore either races the paste (and the app
silently pastes your *old* clipboard, which looks like it worked) or needs a
delay long enough to cost the sub-second latency this app exists for and to race
the next dictation. The full reasoning is in `win::paste`'s doc comment in
`iris-core/src/inject.rs`. If you keep something on the clipboard you cannot
lose, copy it back afterwards. Keeping dictations under the threshold avoids the
paste altogether, but only under the default — with `method = "clipboard"`
there is no length short enough to stay off the clipboard.

A paste can also decline itself and type the transcript instead. Two things
cause that, and neither depends on how the paste was chosen: a hotkey still
reading down that would turn Ctrl+V into a different shortcut (checked both
before and after the clipboard is written — the early check spares your
clipboard, the late one catches a key pressed while Iris was waiting for it),
and a clipboard another application is holding open — a collision that is
usually momentary, so Iris asks a few times over a few tens of milliseconds
before giving up and typing instead. Both are logged under `--verbose`, as is
the paste-hostile skip above.

Clobbering is not the only cost of going through the clipboard: anything on it
can be picked up by other software, and a dictation is not necessarily something
you want kept. So Iris asks Windows to keep the item out of **Clipboard History
(Win+V)** and off **Cloud Clipboard sync**, using the three registered formats
Windows documents for exactly that
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
deliberate, and it is the recovery path described further down.

**What every long keystroke burst does.** Whenever a transcript is typed rather
than pasted *and* it is long enough to need it, the keystrokes go out in smaller
groups with a short pause between them, so a slow app has room to keep up. This
covers every route onto the keystroke path — a paste-hostile window, either
hotkey veto, an unavailable clipboard — and is not specific to any of them.

Those pauses have a flip side worth knowing: anything *you* type during them
lands in the middle of the transcript. Starting your next dictation while a long
one is still being typed out is the likely way to see it. Long transcripts were
already split into several bursts before the pauses existed, so this widens the
window rather than creating it — and unlike the garbling it prevents, it is
visible on screen.

**If a long dictation does not appear, it is not lost.** That list of
paste-hostile apps is best-effort and always will be — it can only name
application families that are actually identifiable, and no list can cover
every app that binds Ctrl+V to something of its own. So a paste is built to fail
recoverably rather than silently:

- **The text is still on your clipboard.** Iris does not restore the previous
  contents (see above), which means the transcript is sitting there — press
  whatever paste key that app *does* use.
- **The text is in the session log**, with `[history] enabled = true`. That log
  is the durable record of every dictation, recorded whether or not delivery
  worked — which matters here, because a paste into a misidentified app *is*
  reported as delivered. Run `iris --history` to print the last ten dictations,
  or `iris --history 50` for more; it ends with the log file's own path, so it
  is also how you find the file to copy from. The Settings window's History
  tab (below) lists them with one-click copy and a search box.

Note that "delivered" here only ever means the keystrokes or the paste
shortcut reached Windows' input queue. Neither Windows nor Iris can confirm
that the app on the other end rendered them correctly — that gap is exactly
what the original bug was — so the latency Iris records for a dictation is a
delivery time, not a receipt. (The overlay shows no latency figure; its
readout is the elapsed recording time — see `crates/iris-overlay/README.md`.)

**Overlay.** `overlay_enabled` also needs a restart to take effect; `main`
spawns (or skips) `iris-overlay` once, before the loop exists.

**Keys.** `IRIS_DEEPGRAM_KEY`, `IRIS_GROQ_KEY` and `IRIS_LLM_KEY` take
precedence over the file. Keys in the file are copied into the environment at
startup, before any thread exists, because the engine and polisher constructors
upstream read them from the environment only — deliberately, so a key cannot end
up in a shell history. `Keys` has a hand-written `Debug` that redacts, so a
stray `{:?}` on the config cannot leak one, and the config Iris writes itself
contains no `[keys]` section at all.

## Tray

`tray-icon` 0.24 (+ `muda` for the menu): the maintained extraction of Tauri's
tray, pure Rust over Win32 — no C toolchain — so it cross-compiles to
`x86_64-pc-windows-gnu`, which is how this project builds from WSL, and it needs
no windowing framework of its own. The alternatives weighed against it are in
`tray.rs`'s module docs. On Linux `tray-icon` needs GTK, so the dependency is
`[target.'cfg(windows)']` and the tray is simply absent elsewhere.

Menu: engine picker, microphone picker, theme, polish toggle, open settings,
reload settings, quit. "Open settings" opens the settings window (below,
opened by default on launch too) — opening it twice focuses the existing
window rather than making a second one. The config file is still one click
away, from that window's Settings tab, so "Reload settings" keeps meaning what
it always did. A window that cannot start at all does not turn the item into a
no-op: `window::EditorWindow` takes over and every click opens `config.toml`
in the editor instead — what the item did before this window existed — saying
so each time.

While the mock engine is in force the menu opens with four disabled labels above
all of that (`tray::demo_notice`): what the mock engine means for the transcript,
both edits that leave it — the `engine` line changed in place, a `[keys]` block
appended at the end — and the config path they go in. They are there because
`iris.exe` is a GUI-subsystem binary (`main.rs`'s `windows_subsystem`
attribute) — a double-click, the Start Menu shortcut and the Startup shortcut
alike open no console at all — which is exactly the launch where the startup
banner's pointer at the same file is never read — leaving a first-run user on
stub transcripts with no explanation on screen. That also makes this the surface
for someone who never opened the zip's README, so it carries the whole
instruction rather than half of it; `iris-app/tests/settings.rs` executes both
documents against a generated config and pins them to each other. The tooltip
says the short version, for the same reason.

The labels reflect startup state and are never rebuilt, so switching engine from
the tray leaves them up until a restart — see "Known limitations" below.

The icon is the captain-locked **prism triangle** (spectrum wedge on a plate),
drawn in code (`tray::icon_rgba`) from the same mark as
`iris-overlay/assets/iris-prism.svg`. No binary `.ico` to keep in step with the
theme and no file to fail to find next to the `.exe`. The `.exe`'s own icon is
the same mark, generated at build time by `build.rs` and embedded — still
nothing committed, and a test in `tray` reads that generated `.ico` back and
compares it pixel by pixel, so the build script's hand-synced copy of the
geometry cannot drift.

### Known limitations

The menu's check marks and the tooltip are fire-and-forget. `muda` check items
toggle their own checked state on click and the engine / microphone / theme
submenus are not radio groups, so after a switch the previously selected item
stays checked; a rejected switch (e.g. deepgram with no key, which the loop
rolls back) can leave the wrong item checked; and the tooltip shows the state
at startup only. The demo notice above is the same story with a sharper edge:
after a tray switch off mock it keeps saying transcripts are stubs while real
ones are being injected, which understates a working app rather than hiding a
broken one. The config file and the loop remain the source of truth — the menu
is a remote control, not a display. Reconciling it would need the item handles
kept on the tray thread plus a state-update message from the loop, a deliberate
non-goal for v1.

## Single instance

`crate::single_instance`, checked at the very top of `main.rs`'s Windows
`run` — before the microphone, the hotkey hook or the tray exist — because a
tray-resident app that let a second launch run alongside the first would end
up with two of each. A named Win32 mutex marks one process as primary; a
named auto-reset event lets a later launch wake the primary's settings window
(`App::with_reopen_signal`, drained in `App::run` the same way
`Command::OpenSettings` is) instead of piling up behind it. Both names are
qualified with `USERNAME` — Remote Desktop / Terminal Services can put more
than one interactive user in one session, and the default, session-local
kernel-object namespace would conflate them. A second launch that finds the
lock held signals the primary and exits immediately, before touching anything
the primary already owns. Off Windows — and if the check itself fails to
start — every launch is treated as primary with no reopen signal, on the same
"an accessory must not be why dictation stops working" terms as the overlay
and the settings window elsewhere in this file.

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
`show_live_text = false` (the default) makes `OverlayPill` swallow it, so the
overlay never sees a partial and stays its resting capsule — the presentation
most users see, not a fallback. `show_live_text = true` is the opt-in for
anyone who wants dictated words on screen. It is pushed through
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

## Settings window

`crate::window`. Opened automatically on every deliberate launch — the icon,
the Start Menu, launching Iris again while it is already running (see
`single_instance` below) — so opening Iris shows something rather than
starting a background process with nothing on screen. Also reachable from the
tray's `Settings` item once closed; closing it never stops dictation, which
owns its own thread throughout. The Startup-folder shortcut is the one
exception: it carries `--background` (`Args::background` in `main.rs`), so a
boot-time autostart stays quietly in the tray rather than opening a window at
every login. Three sections, in priority order:

- **History** — the session log below, newest first, with a search box and a
  one-click copy per entry; a failed injection shows its reason in place, not
  buried, because this is the recovery path.
- **Settings** — engine, input device, theme, polish, the overlay toggle and
  hotkey rebinding, all written through `Config`. Never renders an API key;
  "Open config file" hands `config.toml` to the user's editor instead, which
  is still where the keys are set. `hotkey` and `overlay_enabled` are read
  once at startup, so the window is given the *running* values too — the
  sidebar always names the key that works right now — paired with the
  launch-time file values as an `InForce`. Either one is marked "until
  restart" only when `InForce::pending` holds: the file has moved since
  launch *and* what it now holds is not already running. Both halves matter.
  `--hotkey` is a run-only override that never reaches the file, so it is not
  an unsaved edit; and picking that same key in the picker does move the
  file, but onto the value already in force, where a restart would change
  nothing. `InForce::diverged` is the separate question — saved is not what
  is running, whatever moved — and it is what makes the overlay checkbox say
  "not running this session" when the overlay was asked for and failed to
  start.
- **Insights** — most repeated words/phrases (stopwords and filler stripped),
  dictations today/all-time, total words, average/median perceived latency
  (over the dictations that actually reached the screen — a record whose
  injection failed carries a shorter span that stops at the transcript),
  success-vs-failure rate — all computed from the session log on the window's
  own thread, off the dictation critical path. Not the AI speech analyzer
  (`iris-ai-analyzer`, personality/speaking-style) — that is separate and later.

**Toolkit: `egui` + `eframe`.** Chosen over a WebView shell (needs the
WebView2 runtime + a loader DLL, and HTML/CSS in an all-Rust codebase), a
retained Win32-controls toolkit (fights the glassy look without owner-drawing
everything), and extending `iris-overlay`'s renderer (no input handling to
build on — the pill "never activates, never hit-tests"). Full evidence and
the cross-compile proof are in `window/mod.rs`'s module docs.

**Portable view, thin native shell.** `window::ui` (and `state`/`insights`/
`search`/`egui_theme`) depend on plain `egui` only, so they type-check on
every platform, the same discipline the rest of this README describes for the
loop. Only `window::shell` — the `eframe`/`winit` bootstrap and the OS thread
— is `[target.'cfg(windows)']`, mirroring how `tray-icon` is gated.

**The window never writes `config.toml`.** A change sends a `Command` on a
channel `App::run` selects on alongside the tray's — the same commands the
tray sends for engine/device/theme/polish, plus two new ones (`SetHotkey`,
`SetOverlayEnabled`) that follow the same shape. `App` stays the one writer,
so a window change and a tray change can never race to overwrite each other.
`WindowState::refresh` re-reads the file and the log every couple of seconds,
so external changes (the tray, a hand edit) show up here too — the tray's own
known-limitations trade-off, inherited rather than solved differently.

**A queued change is not a saved one.** `App::apply` can decline two of these
commands and keep what works — an engine that will not build (no API key), a
microphone that will not open — so it answers every window command with a
`CommandOutcome` on a channel back. The control moves and the status line says
"Saved" only once that answer says the change landed; a refused one says why,
in the loop's own words, instead of flashing "Saved" over a picker that snaps
back on the next refresh.

Colour is `iris_overlay::theme`'s `PRISM_DARK`/`PORCELAIN_LIGHT` tokens,
mapped onto `egui::Visuals` by `egui_theme` and painted directly for the
background wash and the spectrum accent bar — the window and the pill are
meant to read as one product. A failed injection is the one warm thing on
screen (`theme.warn`, amber — not the banned rec-red), and History gives it a
square marker and bold label as well as the colour, so "failed" and
"injected" stay a pair of glances apart without relying on colour vision.

"Dictations today" counts the user's *local* calendar day, and History stamps
each card in local time, even though records are stored in UTC. The offset
comes from `GetTimeZoneInformation` in `window::shell`, not from `time`'s
local-offset lookup, which is unsound in a multi-threaded process — the same
reason `history.rs` stamps in UTC at all.

History draws a page of cards at a time (`window::state::HISTORY_PAGE`, 100)
with a `Show more` button for the rest. `egui` lays out every widget it is
handed, on screen or not, and its own row virtualisation wants a uniform row
height these cards do not have — so what bounds the work is the count, rather
than all `history.max_entries` cards being rebuilt on every repaint while the
user types in the search box.

`cargo run -p iris-app -- --demo-window` opens a real window against a
seeded config and session log under the system temp directory — no hotkey, no
microphone, no injector — the manual verification and screenshot path, the
window's counterpart to `--demo-dictation`. Settings changed in it are really
written to that seeded config: a demo that flashed "Saved" over a change
nothing persisted would hide the one bug this path exists to catch.

## Session log

`history.jsonl` beside the config, one record per dictation, newest last, capped
at `max_entries`:

```json
{"timestamp":"2026-07-31T06:27:17Z","engine":"mock","text":"…","injected":true,
 "latency":{"final_transcript_ms":0.1,"polish_ms":0.2,"perceived_ms":0.4,"audio_secs":5.4,"partials":15}}
```

**Every** dictation is recorded, including the ones where injection failed —
that record is how a user recovers words that never made it onto the screen, and
the durable half of it: the transcript a paste leaves on the clipboard survives
only until the next copy. `iris --history` prints the tail of it.

A record with an `error` carries the timeline as it actually stood, so
`audio_secs` and the spans on it are real: a failed dictation that captured
five seconds of speech says so, instead of reading as a microphone that never
delivered a frame. The one exception is a failure *before* any audio exists —
the engine session refusing to open, capture refusing to arm — which logs a
zeroed `latency` block, and there the zero is the truth.

An `error` and `"injected":true` can appear on the same row. That is a
dictation whose words reached the screen after something about the hold went
wrong — a partial salvaged from an engine that died, a microphone that stopped
mid-hold — and the `error` is the only trace of the part that was lost.

## Engines

| `engine` | Needs | Notes |
|---|---|---|
| `mock` | nothing | deterministic, offline, instant; the default |
| `deepgram` | `IRIS_DEEPGRAM_KEY` | streaming; hides its latency behind speech, except for a hold too short to get a first result back (see below) |
| `groq` | `IRIS_GROQ_KEY` | batch on key-release; cannot hide latency |
| `local` | `--features local-native` | on-device; see below |

**How long a stuck dictation holds the app depends on the engine.** After the
key comes up Iris waits for the final transcript on the main loop, so until
that wait ends the pill stays on "processing" and the tray — Quit included —
does not respond. Deepgram waits 6 s, or up to ~14 s when the socket was still
connecting at key-release with nothing streamed back yet — live text arriving
after that stops the longer wait growing but does not shorten it; `local` waits
20 s and `groq` 28 s. Those two get the longer waits on purpose: both do their real work after
the key comes up, and `groq` has no partial to fall back on at all, so cutting
the wait short would cost the whole utterance rather than its tail. The
constants and the reasoning behind each live in `AGENTS.md` and the engine
module docs.

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

`tests/console.rs` runs the real binary under `--demo-dictation` and asserts
what the terminal shows: the result line and nothing else — no latency table, no
millisecond figures, an empty stderr — unless `--report` or `--verbose` asks for
more.

> **Injection is never executed by a test, in CI, or in a loop.** Windows
> delivers synthetic keystrokes to the *input desktop* — the one the user is
> looking at. There is no sandbox: an automated injection test types into
> whoever is using the machine, and it has already disrupted real work once on
> this project. `SystemInjector` is constructed in `main` and nowhere else; the
> `Injector` trait exists to make that structural rather than a rule someone has
> to remember. Real typing is verified by a person running the app.

`--demo-dictation` and `--speak-wav <file>` run one full dictation — engine,
polish, session log, pill adapter — with dry-run injection, on any platform.
They are the portable way to see the loop work end to end. Each still prints its
own result line — the transcript for `--speak-wav`, the demo summary for
`--demo-dictation` — but the per-span latency table sits behind `--report`,
same as the resident loop, and diagnostics behind `--verbose`.
`--speak-wav` feeds the file at real-time speed (one frame per frame-length,
like a live microphone), so the run takes about as long as the WAV: bursting it
would finish the utterance before the key came up and hide the finalisation
race a held key is meant to exercise.
`--demo-window` is the equivalent for the settings window: a real window
against seeded, isolated demo data, no hotkey or microphone involved.

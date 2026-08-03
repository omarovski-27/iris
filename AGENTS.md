# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

## Build and test

Windows-first, developed from WSL2. Full setup and the reasoning behind the
toolchain choice: `docs/dev-windows.md`.

```bash
cargo test --workspace                                  # portable; the default loop
cargo check --workspace --target x86_64-pc-windows-gnu  # type-check the Windows-only code
cargo build --release --target x86_64-pc-windows-gnu    # produces runnable .exe
./target/x86_64-pc-windows-gnu/release/iris.exe         # WSL runs it as a real Windows process
```

Only the OS-bound layer is `#[cfg(windows)]` — `docs/dev-windows.md` keeps the
list. Everything else is portable, so tests and the latency harness run natively
on Linux. Keep it that way — it is what makes the project CI-testable at all.

## Packaging and release

`scripts/package-windows.sh [output-dir]` builds the release `.exe` and zips it
with `packaging/windows/install.ps1`, `packaging/windows/README.md` (staged as
the zip's `README.md`) and `LICENSE` into `dist/iris-<version>-windows-x64.zip`
— the repeatable path from source to something a non-developer installs.
`packaging/windows/README.md` is the *only* copy of the end-user walkthrough
(SmartScreen, install, config, keys); the repo README links to it rather than
restating it, so change the install flow there and nowhere else. See the repo
README's "Why a zip and not an installer" for why this stops at a zip + per-user
PowerShell script rather than an MSI: a real installer needs MSVC or the WiX
toolset, both of which trade away the no-C-toolchain / no-MSVC cross-compile
this project is built around (`docs/dev-windows.md`).

`crates/iris-app/build.rs` embeds the prism icon and version metadata into the
`.exe` via `winresource` (drives the mingw `windres` already required to link
this target — no new host dependency); an embed failure panics the build rather
than warning, because a warning ships a generic-icon exe. The icon geometry is a deliberate,
hand-synced duplicate of `tray::icon_rgba`'s dark plate — a build script cannot
depend on the crate it builds. The `.ico` is generated into `OUT_DIR` for
every target — not only Windows — so that
`tray::tests::the_embedded_exe_icon_still_matches_the_tray_mark` reads it back
in the portable `cargo test` loop and fails on drift; re-sync `build.rs` by
hand when that test goes red. The end-user install walkthrough is documented
in `packaging/windows/README.md`, and
`iris-app/tests/settings.rs` executes that file's TOML instructions, so an
edit there that would not load is a test failure rather than a bricked
config.

After packaging a new build, work through
`docs/first-run-checklist.md` on a real Windows machine — this repo has no WSL
Windows-interop in most sandboxes, so nothing Windows-specific (`#[cfg(windows)]`
paths: the banner, the hotkey hook, the overlay, injection) has ever actually
executed here; only compiled, cross-compiled, and been reviewed.

## The application

`crates/iris-app/` is the product — the resident tray app that wires the other
crates into a working dictation loop. Its README is the map: the loop, the
config file, the tray, the overlay, the settings window, the session log.

Load-bearing beyond that crate:

- **`Injector` is a trait so no test can ever type into the user's desktop.**
  `SystemInjector` is constructed in `main` and nowhere else; see the rule below.
- **Keys reach engines through the environment only.** `Config::promote_keys`
  copies file keys into the process environment in `main`, before any thread
  exists, because that is the only point at which mutating it is safe.
- **`PillSink` → `OverlayPill` → `OverlayHandle`.** On Windows, `main` spawns
  `iris-overlay` for process life and the loop drives it through `PillSink`.
  After a successful `inserted(latency_ms)` do **not** call `hide()` — the
  overlay self-exits after the ~550 ms confirmation hold. `hide` is for
  cancel/empty/error only. Theme: `Dark→PRISM_DARK`, `Light→PORCELAIN_LIGHT`.
- **The console is quiet by default; that is a product requirement, not an
  oversight.** No per-dictation output, no millisecond figures, no raw
  transcript — the captain rejected that as "AI-slop indicators" a finished
  product does not show a user. `--report` opts into `Timeline::report`'s
  full per-span table on stdout; `--verbose` opts into diagnostics on stderr
  (`iris_core::vlog!`, `iris-core/src/log.rs`). Keep new console output
  behind one of those two, not printed unconditionally — including in the
  `--demo-dictation` / `--speak-wav` dev paths, which gate the table on
  `--report` the same way the resident loop does. Errors and delivery
  failures are the exception and must stay visible unconditionally (see the
  injection-failure path in `App::capture`, `app.rs`, which points at the
  session log or echoes the text back when the log is off).
  `crates/iris-app/tests/console.rs` drives the real binary and holds this.
- **The settings window** (`iris-app::window`) is the History/Settings/Insights
  UI opened from the tray's `Settings` item. See "Settings window" below.

```bash
cargo run -p iris-app -- --demo-dictation                 # mock + dry-run + pill
cargo run -p iris-app -- --speak-wav assets/speech-16k.wav
cargo run -p iris-app -- --demo-window                    # seeded Settings window, isolated temp config
```

## iris-polish layout

- `crates/iris-polish/` is a workspace member (it was standalone until
  `iris-app` needed it as a path dependency). Its own `Cargo.lock` is now
  unused; the workspace lock is the real one.
- LLM path uses pure-Rust `ring` TLS (not aws-lc-rs) so `x86_64-pc-windows-gnu` cross-compiles cleanly. See crate `Cargo.toml` comments and `crates/iris-polish/README.md`.
- Parallel workers may own other crates and the workspace root; do not restructure the monorepo from a polish-only task.

## Never run text injection unattended

**Do not execute the text-injection path as a test, in CI, or in a loop.**
Windows delivers synthetic keystrokes only on the *input desktop* — the one the
user is looking at (`SendInput` returns `ERROR_ACCESS_DENIED` on any other
desktop, including a purpose-created private one). There is no sandbox: any
automated injection test types into whoever is using the machine. This has
already disrupted real work once during development.

`iris-spike --self-test` therefore never runs injection; the interactive
checklist in `crates/iris-spike/README.md` is the sole verification path for
it. In `iris-app` the same rule is structural: the loop takes an `Injector`,
and `SystemInjector` — the only implementation that reaches the OS — is built in
`main` and never in a test (`--dry-run`, `--demo-dictation`, and `--speak-wav`
are the safe paths). Injection logic that *can* be tested without the OS lives
in `iris-core/src/text.rs` (UTF-16, surrogate pairs, control characters) and is
unit-tested.

## Architecture

`Engine` (`iris-core/src/engine/mod.rs`) is the load-bearing abstraction: a
streaming session (`open → push → finish → Final`), never
`transcribe(pcm) -> String`. The batch signature forces record-then-transcribe
and puts the whole model inference after key-release, which is the thing this
product exists to avoid. Its doc comment explains the three properties that must
hold for any new engine.

`Dictation` (`iris-core/src/dictation.rs`) is the portable driver shared by the
live Windows pipeline and the CI harness, so latency measured in CI is
comparable to latency measured on a desk. `Dictation::events()` is the seam the
overlay UI hangs off.

Measured latency numbers, the budget breakdown, and open risks:
`docs/spike-findings.md`.

**A short hold can end before Deepgram says anything at all.** Connect + first
result is ~1-3 s; a hold shorter than that has nothing streamed back when the
key comes up, so the entire transcript depends on one post-`Finalize` flush,
and sending `CloseStream` immediately after `Finalize` can pull the socket out
from under that flush before Deepgram gets to it. `deepgram.rs`'s `pump` waits
for `from_finalize` — a boolean Deepgram itself sets on the Results message
that answers a `Finalize`, live-verified to arrive for any session that was
sent real audio, including holds under 0.5s and holds containing only silence
— before sending `CloseStream`; `FINALIZE_ACK_TIMEOUT` is a bounded safety net
under that for protocol failure only. The ack decides when `CloseStream` goes
out and nothing else: reporting `Final` on it too was built and reverted,
because the ack proves the flush *started*, not that it fit in one frame, and a
live cadence measurement found inter-message gaps too wide for any quiet window
to both cover them and fit the perceived-latency budget — so `Final` is gated on
the session close instead. Three earlier designs (unbounded coverage-catch-up, a
fixed ceiling, a stall detector) were tried and rejected first. The measured
figures and the full reasoning — including why an inferred "Deepgram is probably
done" always loses to this authoritative signal — live in the module doc of
`crates/iris-core/src/engine/deepgram.rs`, beside the constants they set. Session
prewarming was also tried and dropped: a live idle probe found Deepgram closes
an unused connection in roughly 12-15s (see Sharp edges), far short of real
gaps between dictations, so it protected against a race that barely occurred
in practice.

**A re-emitted final segment is discriminated by the audio span it covers, never
by when it arrived.** A guard keyed on arrival — anything landing after the
`Finalize` ack — was built and removed: the short hold above has *everything*
arrive there, including a genuine "No. No.". `Transcript::absorb` instead
withholds a new final only when the span it reports is *fully covered* by the
spans already accepted, and unusable timing keeps both candidates, because a
wrongly-deleted word is worse than a duplicated one. Containment rather than
overlap, and where the one tolerance constant may and may not be applied, are
load-bearing; the reasoning is in the same module doc.

**`FINALIZE_ACK_TIMEOUT` and `FINALIZE_TIMEOUT` do not bound how long a
dictation can hang.** They bound `deepgram.rs`'s own internal wait
(ack-before-`CloseStream`, then silence-after-`CloseStream` — the second is
re-armed on every inbound message, so it bounds *silence*, not the whole
finalisation). The actual outer bound on `key-up → transcript` is
`Dictation::DEFAULT_FINAL_TIMEOUT` (`iris-core/src/dictation.rs`), a plain
`recv_timeout` in `Dictation::finish` that wraps the entire engine session,
independent of and invisible from any per-engine timeout. A 2026-08-02
regression (perceived latency up to ~10s during a network blip) traced to
exactly this: a stalled Deepgram session kept trickling messages just often
enough to keep re-arming `FINALIZE_TIMEOUT` without ever sending the close
sign-off, so only the outer bound ended the wait. Diagnosing a hang from its
milliseconds: check the engine's bound before assuming a Deepgram-side constant
is misbehaving. **The outer bound is per engine, not global**:
`DEFAULT_FINAL_TIMEOUT` is only the default behind `Engine::final_timeout`, and
its 6s is streaming evidence. An engine that works after key-up — Groq's upload
+ inference, the local Whisper finalizer — overrides it with a deliberately
generous, provisional value, because there the expiry costs the whole
utterance (`streams_partials` is false, so nothing can be salvaged). Every
caller of `Dictation::finish` asks the engine; `App` re-asks per dictation, so
switching engines switches the wait. That per-engine bound is also a bound on
the whole UI: `finish` blocks the resident loop, so Groq (28s) or the local
engine (20s) can leave the pill frozen and Quit unserviced for that long. The
numbers are accepted, not trimmed — with `streams_partials` false an early
expiry costs the whole utterance — and on the default path, Deepgram, the
ordinary bound is 6s. Deepgram's worst case is not 6s: on a hold that was still
connecting with nothing salvageable, the connect grace below can hold the loop
for the connect budget (8s from key-down) *plus* a re-based 6s finalise from the
moment the socket comes up, ~14s — and a partial arriving after that grace was
bought stops it growing without giving any of it back, so a dictation that ends
up with words can pay it too. Quote that number, not the 6s, whenever this
exposure is weighed. A non-blocking finalise is the real fix and is tracked
elsewhere; do not approximate it by lowering a ceiling.

**The outer bound may never end a session that is still legitimately
connecting.** The two clocks do not start together — `CONNECT_TIMEOUT` (8s)
runs from key-down, the outer deadline from key-up — so no hold length makes
the raw ordering stable, and ranking the constants against each other was
tried and reverted. The relationship is enforced instead: `Engine::connect_budget`
publishes the engine's connect ceiling and `Dictation::finish` extends its wait
to cover it while there is nothing to salvage — the one case where giving up
early costs the whole utterance rather than a tail. **Connecting is not the
goal; the words are.** A grace that ended on `Connected` was built and rejected
for exactly that reason: it spent the whole connect budget and then stopped
waiting at the instant the connection became useful, losing the utterance to a
socket that *worked*. So `Dictation::extend_while_nothing_to_salvage` extends
rather than re-computing — still connecting buys key-down + connect budget,
just connected buys `Mark::StreamReady` + the engine's own `final_timeout`, and
a non-empty partial stops the buying, because from there expiry costs a tail.
It stops the buying and nothing more: what an earlier pass bought stands, since
the `Final` that would replace that interim with the accurate text is usually
milliseconds behind it. So the 6s win is real but belongs to the dictation that
never needed an extension; one that bought the grace and streamed its first
partial afterwards still pays the ~14s worst case named above. A connect that
fails on its own terms still reports as one (from `Mark::StreamReady`,
engine-agnostically).
`fm/iris-silent-and-instant` moves the same constant from another direction:
keep the constant and the grace together or the data loss comes back.

**Three regressions here were one defect: a wait bound that could move
backwards.** The outer deadline undercutting the connect budget, `Connected`
collapsing an extended bound to an already-past deadline, and the first partial
doing the same — each looked like its own special case, and the third one cost
the user accurate words (an interim typed while the engine's real `Final` was
milliseconds behind it on the wire). They are now structurally impossible
rather than absent by inspection: `WaitBound` (`dictation.rs`) is the single
bound on `finish`'s wait and `WaitBound::extend_to` is its only mutator, so an
event can buy the session more time and nothing can take time away. Stopping a
*trigger* from shortening the wait is not the fix and never was; if a fourth
one shows up, it belongs in the same monotonic bound, not in a fourth branch.

**A failed dictation keeps its words and its timeline.** `Dictation::finish`
and `Dictation::abandon` — the latter for a hold that ended without `finish`
ever running — share one salvage rule: a non-empty `latest_partial` becomes the
transcript, and the timeline carries the real `audio_secs` and marks. Every arm
obeys it, `session.finish()` failing included. So a socket dying while the tail
is fed costs no more than the same socket dying one statement later:
`App::capture` sends that salvaged text through `App::deliver` — polish,
injection, a normal record. Each salvage names itself: `DictationOutcome::cause`
is `None` only for a real `Final`, and `App` folds it into `record.error`
alongside any mid-hold cause, so a salvage never reads as an ordinary
dictation. A hold that transcribed nothing becomes an `App::failed` error
record; a hold whose words exist but may never be injected — only the dead
hotkey channel — becomes an `App::reported` one, which is `failed` with the
text put back. `record.error` and
the `Result` of `App::dictate` are deliberately allowed to disagree: the
`Result` follows delivery (`record.injected`), because a dictation whose words
reached the screen is not a failure however abnormally it got there, and the
console must not contradict the confirmation the user just watched. The blank `Timeline`
in `App::dictate` covers only failures before any audio exists; do not widen it
back, and do not let the two exits drift apart.

**Nothing but a confirmed key-up may end a hold, and nothing else may inject.**
A mid-hold failure — the microphone dying, the engine refusing a frame, the
event channel closing — is noted, its source swapped for
`crossbeam_channel::never()`, and the loop keeps waiting for the real
`HotkeyEvent::Up`; finalising early would type a mid-sentence fragment into
whatever the user is looking at while they are still speaking. Only two paths
may reach injection, both after key-up by construction: `Dictation::finish`'s
own salvage, and the tail-feed failure in `App::capture`. A dead hotkey channel
is the exception that proves it — no key-up can ever arrive, so that hold ends
at once and its words are recorded, with their real timeline and every cause
the hold collected, but never typed.

## Sharp edges

- API keys come from the environment only (`IRIS_DEEPGRAM_KEY`, `IRIS_GROQ_KEY`).
  The engine structs have hand-written `Debug` impls that redact the key; keep
  them if you add fields.
- rustls is pinned to the `ring` provider. The default (`aws-lc-rs`) needs cmake
  and nasm to cross-compile.
- The hotkey thread must only pump messages: Windows silently uninstalls a
  low-level hook whose callback exceeds ~300 ms.
- `winit` (via `eframe`) panics if its event loop is built off the main
  thread — a deliberate cross-platform guard, not a bug. The settings window
  runs on its own thread like the tray and the overlay, so `window::shell`
  opts out with `NativeOptions.event_loop_builder` +
  `EventLoopBuilderExtWindows::with_any_thread(true)`. Sound here specifically
  because this process only ever runs one `eframe` window at a time; a second
  window on a second thread would need more thought.
- `inject.rs` corrects the configured hotkey — and *only* the configured
  hotkey, never a broader modifier sweep — before every `SendInput` burst,
  per `SendInput`'s own warning that an already-pressed key can corrupt the
  events it generates. Three narrowings were each cut from a real failure
  mode found in review; do not simplify any of them back out:
  - two signals must disagree, never one reading alone: `GetAsyncKeyState`
    *and* `hotkey::is_held`, gated on `hotkey::is_listening`;
  - `RightAlt` and `RightWin` are never corrected, even on a genuine desync;
  - modifier and extended-flag knowledge lives on `Key`, beside `Key::vk`.

  The reasoning for each — including which parts are heuristic and which
  wiring is an accepted, permanently untestable gap — is in the doc comments
  on `inject::modifier_to_release`, `release_hotkey_if_stuck`,
  `Key::is_correctable_modifier` and `hotkey::is_listening`. Read those
  before touching this; none of it is dead code. The configured hotkey
  reaches the injector via `SystemInjector::new` (wired in `main.rs`), not
  via `app.rs`.
- A transcript needing more than one `SendInput` batch is rerouted to
  `Method::Clipboard` even under a `sendinput` config, so a long dictation
  can clobber the clipboard. Read `inject::effective_method`'s doc comment
  before changing the threshold, and keep the user-facing disclosure (the
  clobber, the Clipboard History/Cloud Clipboard opt-out and its limits, the
  recovery path) in `crates/iris-app/README.md`.
- Three vetoes send that paste back to keystrokes — `inject::accepts_paste`,
  `inject::paste_accelerator_survives`, an unavailable clipboard — and
  `inject::pacing` is what makes that landing safe. Their doc comments carry
  the reasoning; two rules are easy to break from outside them: the
  deny-list is best-effort and permanently incomplete, so adding entries is
  not progress towards coverage, and declining the paste is the *only*
  sanctioned way to cover `RightAlt`/`RightWin` — never widen the correction
  in `modifier_to_release`.
- Deepgram closes an idle websocket connection (no audio sent) in roughly
  12-15s, live-measured. Relevant to any future connection-reuse idea: it
  only pays off within that window of the last dictation, not across the
  minutes-apart gaps typical of real desktop use.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.

## Local ASR (`iris-engine-local`)

- Crate: `crates/iris-engine-local/`. Architecture and Windows link story:
  `crates/iris-engine-local/README.md`.
- Default features are offline (mock + model manager only). Real engines need
  `--features native` (or `streaming` / `whisper`). Integration tests need
  `IRIS_LOCAL_MODELS=1` and network on first model download.
- Cross-compile of **native** features to `x86_64-pc-windows-gnu` from WSL is
  blocked (sherpa prebuilt is MSVC/MT; whisper.cpp/ggml needs Windows SDK
  symbols MinGW headers lack). Default-feature windows-gnu check is fine.
  Prefer native Windows (MSVC/msys2) for shipping local engines.
- whisper-rs bindgen needs libclang: e.g. `LIBCLANG_PATH=/usr/lib/llvm-18/lib`.

## Pill overlay (`iris-overlay`)

- Crate: `crates/iris-overlay/`. App adapter: `iris-app::pill::OverlayPill`
  (implements `PillSink`). `OverlayHandle` is the low-level contract; see
  `crates/iris-overlay/README.md` for the full design record, rendering, and
  WSL notes — that file is the authority on exact geometry and colour tokens,
  not this one.
- The design is **captain-decided** (2026-07-31, superseding the prior
  captain-locked fixed capsule of the same date; round 3 on 2026-08-01
  superseded the default — see below): a shape-shifting shape that can open
  into a live-text ribbon, Prism dark default, Porcelain light,
  prism-triangle icon unchanged (tray uses the same mark via
  `tray::icon_rgba`). Full rationale and the two rejected alternatives:
  `data/iris-ui-directions/report.md` in the fleet's records.
- **Round 3 (2026-08-01):** `show_live_text` defaults to `false`
  (`iris-app::config::Config`) — most users now only ever see the resting
  capsule, never the ribbon. That capsule holds a wave row and an
  elapsed-recording timer side by side, not a bare circle. See
  `crates/iris-overlay/README.md`'s "Round 3" section before touching
  `layout::REST_W`, `render::draw_timer`, or the width formula in
  `Renderer::draw`.
- Geometry is one capsule whose width animates between the rest capsule
  (`layout::REST_W`) and an open ribbon (`layout::RIBBON_MAX_W`) — there is no
  fixed pill size and no true circle in normal operation any more.
  `layout::ORB_D` is now just the shape's constant height. No solid rec-red
  (mint/sky live core). Motion timings in `motion.rs` are unchanged and stay
  captain criteria; every one is imported by the new shape, not copied.
- Geometry and motion are single-sourced; a `Theme` is colour only. Keep it that
  way or "same geometry, swapped tokens" stops holding.
- The rasteriser (`render/`) is portable and the window (`window/win32.rs`) is
  the only `cfg(windows)` file. That is what lets `cargo run --example pill-demo
  -- --filmstrip <dir>` produce reviewable PNGs of the real frames from Linux.
- The pill is a display: it never activates, never hit-tests, and never
  injects input. It **can** hold the live transcript text on screen while the
  ribbon is open, gated behind the opt-in `show_live_text` config flag (off by
  default since round 3) — a deliberate, captain-approved reversal of the
  original "never holds transcript text" rule. See
  `crates/iris-overlay/README.md` "The contract changed, and here is why"
  before touching this again.

## Settings window (`iris-app::window`)

- Opened from the tray's `Settings` item (`Command::OpenSettings`); one
  `iris-window` thread for process life, mirroring `tray`/`iris-overlay`. The
  toolkit choice (`egui`/`eframe` over a WebView shell, a retained Win32
  toolkit, or extending `iris-overlay`'s renderer) and the evidence for it are
  in `window/mod.rs`'s module docs — read that before reconsidering it.
- **Portable view, `cfg(windows)` shell.** `window::ui` and everything it
  calls (`state`, `insights`, `search`, `egui_theme`) depend on plain `egui`
  only and type-check on Linux; only `window::shell` depends on `eframe` and
  is Windows-only, so `eframe`/`winit`/`glow` never enter a non-Windows build.
  Keep new window code on the `egui`-only side unless it genuinely needs a
  native window/GL call.
- **The window never writes `config.toml`.** Every setting change sends a
  `Command` — the same ones the tray sends (`SetEngine`/`SetDevice`/
  `SetTheme`/`SetPolish`) plus two new ones this window introduced
  (`SetHotkey`, `SetOverlayEnabled`) — on a channel `App::run` selects on
  alongside the tray's. `App` stays the sole writer; see `window::state`'s
  module docs for why a second writer would race it. `App` answers each of
  those commands with a `CommandOutcome`, and the window moves the control
  and says "Saved" only on that answer — `App::apply` can decline `SetEngine`
  and `SetDevice`, so a queued command is not an applied one.
- `Config::overlay_enabled` gates whether `main` spawns `iris-overlay` at all;
  like `hotkey`, changing it needs a restart (both are read once at startup).
  `main` therefore hands `window::spawn` a `Startup` snapshot: what the
  process is really running on *and* what the file held before CLI overrides.
  Both halves live in one `state::InForce<T>` (`running`, `at_startup`), and
  every claim the view makes comes from its two comparisons — do not answer
  either question anywhere else:
  - `InForce::pending` — "a restart is owed": the file moved since launch
    **and** what it now holds is not already running. Both conditions are
    load-bearing. `--hotkey f9` over a `right-ctrl` file diverges by design
    and must not read as an unsaved edit; picking `f9` in the picker moves
    the file *onto the running value*, where a restart would change nothing.
  - `InForce::diverged` — "asked for but not running": the saved value is not
    what is in force, whatever moved. This is what stops a ticked overlay
    checkbox from reading as a live overlay after `try_spawn_overlay` failed.
- Anything the window cannot reach the OS for crosses through `Env` as a
  callback or a plain value (`list_devices`, `open_config_file`, the local UTC
  offset from `GetTimeZoneInformation`), so `window::ui` stays `egui`-only.
  Note the offset: `time`'s `current_local_offset` is unsound in a
  multi-threaded process — that is why `history.rs` stamps UTC — so
  `window::shell` asks Windows and `insights::DayWindow` does the arithmetic.
- `cargo run -p iris-app -- --demo-window` opens the real window against a
  seeded config/session log under the system temp dir — no hotkey, no
  microphone, no injector — the manual verification and screenshot path.

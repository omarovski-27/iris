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

`iris` is a GUI-subsystem binary (`#![cfg_attr(windows, windows_subsystem =
"windows")]` in `main.rs`) — **no launch path may ever show a console
window**, a repeat product requirement after a v0.1.0 regression shipped
exactly that. Do not reach for a console-subsystem/minimized-shortcut
workaround again; that was tried and is what regressed. A real terminal
invocation (`--print-config`, `--verbose`, …) still gets its output back via
`attach_console_for_cli_output` (`AttachConsole(ATTACH_PARENT_PROCESS)` +
rebinding stdout/stderr to `CONOUT$`, skipped when stdio is already a real
inherited handle e.g. `iris.exe > out.txt`); a start-up or run failure reaches
a console-less launch through the existing `report_failure` message box
instead — see both functions' doc comments in `main.rs` before touching
either. Verify the subsystem field of a cross-compiled `.exe` with
`x86_64-w64-mingw32-objdump -p <path> | grep -i subsystem` (expect `Subsystem
00000002 (Windows GUI)`, not `00000003 (Windows CUI)`); there is no way to
verify "no window ever flashes" from this environment — that needs
`docs/first-run-checklist.md` on real Windows. The embedded icon and version
resource are also checkable from Linux without Windows interop: `icoutils`
(`wrestool`/`icotool`, `apt-get install icoutils`, not preinstalled) lists
and extracts them — `wrestool -l <exe>` for the resource table,
`wrestool -x --raw --type=16 --name=1 --language=1033 <exe>` piped through
`strings` for the `FileDescription`/`FileVersion`/`ProductName` strings, and
`wrestool -x --type=14 --name=1 --language=1033 -o icon.ico <exe>` +
`icotool -x icon.ico` to render the icon frames as PNGs and confirm it's the
real prism mark, not a generic/default one.

`install.ps1` is a **clean replace**, not an additive install: every run
quits a running Iris, deletes the Start Menu/Desktop/Startup shortcuts a
previous run created, and replaces `%LOCALAPPDATA%\Iris`, before copying the
new build in — so upgrading or re-running with different flags never leaves a
stale shortcut or a stale binary. `-Uninstall` does the removal half only.
Both are safe by construction because `%LOCALAPPDATA%\Iris` never holds
anything but `iris.exe` — `config.toml` and `history.jsonl` live in
`%LOCALAPPDATA%\IrisConfig`, a sibling directory the script never touches —
but `Assert-NoUserDataIn` checks that invariant before every recursive delete
rather than trust it blindly; keep that guard if this script changes again.
See "Config and history location" below for why the config directory moved
under `%LOCALAPPDATA%` at all, and why it is `IrisConfig` and not a bare
`%LOCALAPPDATA%` root or `%LOCALAPPDATA%\Iris` itself.

After packaging a new build, work through
`docs/first-run-checklist.md` on a real Windows machine — this repo has no WSL
Windows-interop in most sandboxes, so nothing Windows-specific (`#[cfg(windows)]`
paths: the banner, the hotkey hook, the overlay, injection) has ever actually
executed here; only compiled, cross-compiled, and been reviewed.

### Config and history location

`config::config_dir()` (`crates/iris-app/src/config.rs`) resolves
`%LOCALAPPDATA%\IrisConfig` on Windows, not `%APPDATA%` (Roaming) — a
2026-08-10 fix for a real exposure on domain-joined machines with roaming
profiles: Windows replicates the Roaming profile to a network share on every
logon/logoff, and `config.toml` carries a plaintext Deepgram/Groq API key
while `history.jsonl` is the user's full dictation history. Non-Windows paths
(`$XDG_CONFIG_HOME`/`$HOME/.config`) are unchanged.

**Not the bare `%LOCALAPPDATA%` root.** `default_path()` joins a literal
`"iris"` onto whatever `config_dir()` returns, and `install.ps1` already owns
`%LOCALAPPDATA%\Iris` for the binary, which it recursively deletes on every
clean-replace install (`Remove-PreviousInstall`). NTFS compares folder names
case-insensitively, so a bare root would make the config directory
(`...\Local\iris`) and the install directory (`...\Local\Iris`) literally the
same folder: `install.ps1`'s own `Assert-NoUserDataIn` guard would then
refuse every future reinstall, or — without that guard — a clean-replace
would silently delete the user's key and history on every upgrade, which is a
worse outcome than the Roaming leak this exists to fix. The extra
`IrisConfig` segment (`resolve_windows_config_dir` in `config.rs`) keeps the
two as siblings under `%LOCALAPPDATA%`;
`windows_config_dir_never_collides_with_the_install_dir` pins it. This
`resolve_windows_config_dir` (and its Roaming counterpart,
`legacy_windows_config_dir`) are deliberately **not** `#[cfg(windows)]`, so
`cargo test --workspace` exercises Windows' own path logic natively on
Linux — only the real env-var-reading call sites (`config_dir`,
`migrate_default_location_from_roaming`) are gated.

**Migration, not a silent switch.** `config::migrate_from_roaming` copies a
pre-existing install's `iris/config.toml` and `iris/history.jsonl` from the
legacy Roaming location to the new Local one on first launch after upgrading,
so an existing user's key keeps working untouched instead of appearing to
vanish. Runs per file, idempotently: a file already present at the new
location is left alone, a missing legacy file is not an error, and a legacy
file is only deleted once `fs::copy`'s reported byte count matches the
source's own length — copy-verified-then-remove, not remove-then-copy. Wired
into `main.rs` right before `Config::load_or_create_reporting`, and skipped
entirely when `--config` or `$IRIS_CONFIG` names an explicit path (the caller
already chose a layout; migrating underneath it would be a surprise). A
migration failure is reported (the same dialog/eprintln path as a migration
schema-version-save failure) but is not fatal to startup.

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
  transcript — the maintainer rejected that as "AI-slop indicators" a finished
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
- **No internet must fail visibly, not silently.** A dictation that never
  reaches a real connection (offline, DNS down, the endpoint unreachable) used
  to end in nothing but an `eprintln!` — invisible on the console-less launch
  path most users take, and indistinguishable in the session log from the
  separately-tracked "almost no audio reached the transcription engine"
  capture bug. `DictationOutcome::never_connected`
  (`iris-core/src/dictation.rs`) is the fix's signal: true only when
  `Mark::StreamReady` was never reached *and* the engine published a real
  `Engine::connect_budget` — gated that way because Groq/local send
  `Connected` immediately at `open()` with no real handshake behind it, so the
  mark means nothing for them and would misclassify an unrelated post-connect
  failure as "offline". Today only Deepgram publishes a connect budget, so
  this is effectively Deepgram-only; extending it to another engine means that
  engine's `Connected` event has to mean a real socket first. `App::failed`
  (`app.rs`) is the one place that acts on the flag: it tags
  `DictationRecord::connection_failed` (`history.rs`, `#[serde(default)]` for
  old log lines) and calls `FailureNotice::connection_failed`
  (`notify.rs`) — a second method on the *same* trait/dialog mechanism
  `injection_failed` already uses, not a second notification channel.
  `crates/iris-app/tests/loop.rs`'s `an_offline_dictation_notifies_promptly_and_is_tagged_in_the_log`
  and `a_stalled_but_connected_dictation_is_not_tagged_as_a_connection_failure`
  cover the offline case and the false-positive it must not create (a
  connected-but-stalled session, the 2026-08-02 regression shape). What this
  does *not* change: the length of the wait itself (Deepgram's `CONNECT_TIMEOUT`
  8s is an accepted tradeoff, not a bug — see the Deepgram section above), only
  that it now always ends in something the user can see.
- **The settings window's close (`X`, Alt+F4, the taskbar's "Close window")
  hides the window and leaves Iris running in the tray — it does not quit
  the app.** This is the 2026-08-11 state, and it reverses part of an
  intermediate fix described below for the historical record: for one day
  (2026-08-10) every close path *did* quit the whole app, built in direct
  response to a report read as "cannot be closed". The captain's next
  message ("I want it such that when I close the app it still runs in the
  background, just like Wispr Flow") made the actual complaint clear in
  hindsight — the 2026-08-10 report was about the app *freezing and lagging*
  on close, not about wanting it to quit; hiding to the tray was correct
  the whole time, and the freeze was the tray Quit finalise-race fixed
  separately below (unchanged by this entry). `window::ui::draw_root` now
  answers `close_requested` by sending `ViewportCommand::CancelClose` (or
  the root viewport — the whole process — would actually tear down) and
  `ViewportCommand::Visible(false)` instead of calling `Env::request_quit`;
  see its doc comment for the full reasoning and `window::shell`'s module
  docs for why `eframe::run_native` then never returns during the ordinary
  hide/show cycle. `Env::request_quit` and `app::flip_quit_flag` are
  unchanged and still exist — the tray's Quit item is today the only caller
  that reaches either. A one-time "Iris is still running" hint
  (`crate::dialog::show_info`, fired on a detached thread so the modal
  cannot block the hide it is announcing — see `window::shell::
  show_close_hint`'s doc comment) shows the first time a close hides the
  window, and `Config::tray_close_hint_shown` remembers that across
  restarts via a new `Command::AcknowledgeTrayHint`, following the window's
  usual "send a `Command`, never write `config.toml` directly" rule. Covered
  by `window::ui::tests::closing_the_window_hides_it_and_leaves_the_app_running`
  (headless `egui::Context`, synthetic `close_requested`, asserts
  `CancelClose`+`Visible(false)` are queued, the quit flag stays clear, and
  exactly one `Command::AcknowledgeTrayHint` goes out) and
  `window::state::tests::note_hidden_to_tray_*`. **Nothing here — nor
  anything else in this repo — can click a real window on real Windows**, so
  whether `Visible(false)` after `CancelClose` really keeps the native
  window alive rather than destroying it is reasoned from `eframe`'s own
  source, not exercised end-to-end.
- **Every close path — not only the tray — flips the same quit flag before
  sending `Command::Quit`, via the shared `app::flip_quit_flag`.** Superseded
  by the entry above for the settings window's own close button, which no
  longer calls `Env::request_quit` at all; kept here as the historical
  record of why `flip_quit_flag`, `Env::request_quit` and the
  `window_commands` plumbing exist; the App-level and Quit-during-finalise
  reasoning below is still exactly how a `Command::Quit` — from the tray, the
  only remaining sender — is handled. A 2026-08-10 follow-up report ("closing
  Iris still freezes and lags; only the tray works") landed *after* the
  tray-only fix below had already shipped, because that fix's scope matched
  the original diagnosis, not the actual bug: closing the settings window
  (`X`, Alt+F4, or the taskbar's "Close window" — all three arrive as the
  same `close_requested` signal on the root viewport, confirmed by reading
  `winit` 0.30's own `WM_CLOSE`/`WM_SYSCOMMAND` handling) only ever hid that
  window — nothing sent `Command::Quit` at all, so the resident app kept
  running with no visible way left to reach it except the tray icon.
  `window::state::Env::request_quit` (at the time, called from
  `window::ui::draw_root` once a frame reports the close) performed the
  identical flip-then-send ordering the tray-only fix introduced, just over
  `window_commands` instead of `control` — `App::apply` already treated
  `Command::Quit` the same regardless of which channel it arrived on, so the
  App-level half of this was already correct; the missing piece was purely
  that nothing on the window side ever exercised it.
  `crates/iris-app/tests/loop.rs`'s
  `a_window_close_does_not_wait_for_an_in_flight_finalise` still pins that
  App-level handling directly (sending `Command::Quit` over
  `window_commands`), since `App` must keep handling it correctly if
  anything else ever sends one, even though the settings window itself no
  longer does. Two close paths remain deliberately unaddressed, both
  reasoned rather than fixed: `WM_QUERYENDSESSION`/`WM_ENDSESSION`
  (Windows shutdown/sign-out) have no support in `winit` 0.30 at all (grepped
  its source; nothing references either message), and intercepting them would
  need an unsafe, untestable-from-here window-subclass hook, so a session end
  can still kill Iris without running any shutdown path; Task Manager "End
  task" on a tray-only process (no window open) most likely calls
  `TerminateProcess` directly rather than `WM_CLOSE`, which is an OS decision
  no application code can intercept. Both are named, not silently assumed
  fixed.
- **A tray Quit clicked while a dictation is finalising no longer waits for
  `Dictation::finish` to return.** `Engine::final_timeout`'s own doc comment
  used to be literally true — "no tray command, Quit included, is serviced" —
  because `App::run`'s select loop only reads `control`/`window_commands`
  *between* calls to `App::dictate`, and `finish` runs straight on that same
  thread inside `App::capture`. A 2026-08-03 assessment judged the exposure
  "nil" for Deepgram specifically because its bound looked short next to
  Groq's/local's; that undercounted it twice — even the 6s ordinary case is a
  visible freeze, and by the time this was revisited the connect-grace
  extension above had already pushed Deepgram's own worst case to ~14s. The
  fix does not touch `Dictation::finish`, any engine's `final_timeout`, or the
  finalize-ack/`WaitBound` machinery above — shortening any of those is still
  off the table. Instead `App::capture` runs `finish` on a spawned thread and
  polls its result against `App`'s `quit_flag` (an `Arc<AtomicBool>`) instead
  of blocking on it outright; `tray::spawn`'s menu handler flips that flag at
  the same instant it sends `Command::Quit` (not after — a flag flipped after
  the send could still lose the race to the poll noticing it), which is what
  lets it be seen without waiting for `App::run`'s own turn on `control`. If
  Quit wins, the still-running finalise is handed off to a second, detached
  thread — cloned `injector`/`polisher`/`notice`/`history`/`config`, no pill —
  that keeps delivering (polish, inject, log) once the engine actually
  answers; `App::pending_finalize` is the `JoinHandle` `main`'s `run` takes
  and joins *after* dropping the tray/overlay/window (in that order, so the
  app already looks closed before this invisible wait), which is what stops
  the words from being lost for the sake of a fast exit. Neither half is
  optional: joining before the drop reintroduces the exact lag this exists to
  fix, and not joining at all loses transcripts on every quit that races a
  finalise. `crates/iris-app/tests/loop.rs`'s
  `a_tray_quit_does_not_wait_for_an_in_flight_finalise` pins both — Quit
  returns in well under an artificially slow finalise, and the transcript
  still reaches the injector and the session log afterward — and reproduces
  the tray's own flag-then-send ordering rather than `App::apply`'s, since
  driving it the other way could not have caught the ordering bug this fix
  depends on getting right. What this does *not* change: a *second*
  dictation still cannot start until the first's finalise (and, now, its
  detached delivery) is done — only Quit was carved out, not general
  concurrency.
- **Hands-free latch: double-tap the hotkey to keep recording with the key
  released, single tap to stop.** Built entirely inside `App::capture`'s
  existing hold loop (`app.rs`) — one `dictate()`/`capture()` call still
  covers the whole tap sequence, never a second dictation path. A private
  `LatchPhase` (`Held` → `AwaitingSecondTap` → `Latched`) drives it: a
  key-up released within `DOUBLE_TAP_WINDOW` (400ms, `is_candidate_tap`) of
  its own press might be tap one of a double-tap, so the loop waits out the
  rest of the window for a following `Down` instead of finalising
  immediately; a `Down` that arrives in time latches (hands-free, no bound
  but `MAX_LATCH_DURATION`, 5 minutes, overridable per-`App` by
  `with_latch_cap` — the only way a test exercises the cap without a real
  five-minute wait); the next `Down` while latched — the press alone, not
  its release — stops it and finalises exactly like an ordinary key-up. An
  ordinary hold-to-talk release past the window is untouched: zero added
  latency, because `Held` never arms any of this phase's extra `select!`
  arms. Two non-obvious traps, both caught by test regressions during
  development, not by inspection:
  - **`AwaitingSecondTap` must stop feeding frames, not just delay
    breaking the loop.** Continuing to drain `frames` through the ordinary
    per-frame arm during the wait silently changes what an engine sees:
    whatever was still queued in the channel at the first tap's key-up used
    to arrive as one batched "tail" feed after the loop broke (the existing
    behaviour for *any* short, backlog-heavy hold, double-tap or not); left
    live, it drains one frame at a time before the loop ever gets there.
    `TailFeedFailsEngine`/`GoesQuietThenTailFeedFailsEngine`
    (`iris-app/tests/loop.rs`) — which fail specifically on a batched
    `push()` bigger than one frame — caught this immediately. Fixed by
    excluding `AwaitingSecondTap` from the frame-reading arm; `Latched`
    keeps reading normally, same as `Held`.
  - **The quit-during-finalise fix above only covers the finalise wait, not
    this loop.** A held key bounds `Held`'s exposure to the user's own
    finger, but `AwaitingSecondTap` and `Latched` both run with the key
    already up, so `LATCH_QUIT_POLL` (100ms, armed in both, never in
    `Held`) polls `App::quit_flag` there too and ends the hold the moment
    it sees Quit — otherwise a latch (or even an ordinary short hold
    sitting in `AwaitingSecondTap`) can leave a tray Quit unread for up to
    the double-tap window or the whole latch cap. Caught by a timing
    regression in the existing quit test once its hold happened to be short
    enough to enter `AwaitingSecondTap`.
  - A lost hotkey channel is handled the same asymmetric way `Held` already
    handles it, but the other two phases read it as *confirmed already
    over* rather than *never confirmed*: `AwaitingSecondTap` finalises with
    the tap's own key-up, `Latched` finalises with the words captured so
    far — never the hard error `Held`'s loss still is. This is what keeps
    `--demo-dictation`'s single-shot channel (dropped right after its one
    `Up`) working once a short synthetic hold lands in `AwaitingSecondTap`.
  The overlay visual is additive to the existing colour tokens, not a new
  `OverlayState`: `PillSink::set_latched`/`OverlayHandle::set_latched`/
  `Command::Latched(bool)` set an orthogonal flag on `iris-overlay`'s
  `Model` (parallel to `set_show_live_text`), reset by a fresh
  `ShowListening` the same way the transcript and timer are;
  `render::core_colour`/`render::glow_colour` read it and swap the core dot
  and halo from `theme.rec` (mint) to `theme.accent` (sky) — the same sky
  `Processing` already uses — while latched. No geometry, motion, wave-row,
  or timer-font change. **The colour swap alone was not enough** — a
  2026-08-10 review at real 1:1 desktop size found it not reliably distinct
  at a glance, and colour alone fails outright for colour-vision
  deficiency, which is the one failure mode this indicator most needs to
  survive (a missed latch is a live microphone the user believes is off).
  Fixed by adding a second, non-colour cue alongside the colour swap: a
  stroked ring drawn around the core dot only while latched
  (`LATCH_RING_R_FRAC` and friends, `render::draw_glyph`) — its *presence*,
  not its hue, is the signal, so it survives grayscale or any CVD
  simulation. Sized to sit clear of both the core dot and the halo's
  pulsing maximum, and shares its radius family with `Processing`'s
  spinner (`SPINNER_R_FRAC`) by design. Evidence (both themes, ring
  visible): `crates/iris-overlay/docs/handsfree-latch-evidence/`.
- **`iris-engine-local` is real, tested code, not a stub — but is not part of
  the shipped Windows binary.** `EngineChoice::Local` and a genuine
  `LocalAdapter` (`iris-app/src/engines.rs`) implementing `Engine`/`Session`
  over `iris_engine_local::LayeredLocalEngine` (streaming sherpa-onnx
  Zipformer partials + whisper.cpp `base.en` batch finalizer behind Silero
  VAD) exist end-to-end in the tray and settings UI today. It only compiles in
  behind the `local-native` feature (`iris-app/Cargo.toml`), which
  `scripts/package-windows.sh` does not pass — so selecting "local" in a
  released build hits a clear stub error, not a crash. Cross-compiling the
  real engines to `x86_64-pc-windows-gnu` from WSL fails at link time (sherpa's
  prebuilt is MSVC-only; whisper.cpp/ggml needs Windows SDK symbols MinGW
  headers lack) — confirmed by `cargo build` (not just `check`, which false-
  positives by never invoking the linker). Making this a real, shippable
  offline fallback needs a native-Windows (MSVC/msys2) build leg added to the
  release pipeline; the local-engine code itself is not the blocker.
- **The settings window** (`iris-app::window`) is the History/Settings/Insights
  UI, opened by default on every deliberate launch (`App::open_window`, called
  from `main`'s `run` unless `--background`) as well as from the tray's
  `Settings` item. See "Settings window" below.
- **A second launch wakes the first instance instead of starting a second
  one.** `iris_app::single_instance::acquire` (checked before the microphone,
  the hotkey hook or the tray exist) claims a named Win32 mutex; a launch that
  finds it held signals a named event and exits immediately. `App::run` drains
  that signal the same way it drains `Command::OpenSettings`
  (`App::with_reopen_signal`). See "Single instance" in `iris-app/README.md`.
- **A panic during startup or the resident loop must still show something.**
  `main::install_panic_dialog`, the first line of `main`, chains the default
  panic hook onto the same `dialog::show` message box a returned `Result::Err`
  already reaches — without it, a panic on a console-less launch (icon,
  Start Menu, Startup shortcut) is entirely silent, which is
  indistinguishable from the app never having started.

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
`crates/iris-core/src/engine/deepgram.rs`, beside the constants they set. Static
session prewarming (open one connection ahead of the key press and hope it is
still there) was tried and dropped: a live idle probe found Deepgram closes an
unused connection in roughly 12-15s, far short of real gaps between
dictations, so it protected against a race that barely occurred in practice.
That finding does not rule out *actively* holding a connection open — an
actively-kept-alive spare (`WarmPool`, Deepgram's `KeepAlive` control frame,
bounded to a few minutes' idle window) was built, tagged `warmpool-v1-withdrawn`
(pushed to origin, so it survives a squash-merge or branch deletion), and
withdrawn before ever reaching the maintainer: three
review rounds kept surfacing data-loss-class lifecycle defects in the same
abstraction (a stale-handoff replay that could still outrun the outer wait
bound, an ack window sized for the wrong traffic shape, a "fixed" spare-
replacement promptness bug that turned out not to be fixed), and a component
where fixes keep silently failing to hold does not get well from being
patched again — see `docs/spike-findings.md` §6 for the decision and the
redesign task it points to. Do not resurrect it piecemeal; a redesign starts
from measured latency on real Windows hardware, not from reintroducing the
same shape. A shared `rustls::ClientConfig` (`engine/net.rs::tls_connector`)
was also tried for TLS session resumption on every cold connect, independent
of the pool and kept: it works (live-verified against the real endpoint), but
live-measured wall-clock impact was negligible, because TLS 1.3's full
handshake is already 1-RTT and resumption without 0-RTT saves crypto compute,
not a round trip — kept as a harmless, real-but-modest win, not mistaken for
the fix.

**A re-emitted final segment is discriminated by the audio span it covers, never
by when it arrived.** A guard keyed on arrival — anything landing after the
`Finalize` ack — was built and removed: the short hold above has *everything*
arrive there, including a genuine "No. No.". `Transcript::absorb` instead
withholds a new final only when the span it reports is *fully covered* by the
spans already accepted, and unusable timing keeps both candidates, because a
wrongly-deleted word is worse than a duplicated one. Containment rather than
overlap, and where the one tolerance constant may and may not be applied, are
load-bearing; the reasoning is in the same module doc.

**Keeping a not-fully-covered re-emission whole can still leave an exact
duplicate at the seam — `strip_seam_duplicate` (`deepgram.rs`) closes that
specific gap, separately from the keep/suppress decision above.** A
2026-08-04 report of garbled multi-sentence dictation (several final
segments, each one an extra seam) traced — by code inspection and synthetic
reproduction, not the captain's real log; see below — to exactly the case the
containment module doc already named as its own accepted cost: `[0.0, 1.5]
"the quick brown fox"` followed by `[1.4, 5.0] "fox jumps over the lazy
dog"` is correctly kept whole (dropping it would lose six real words), but
the repeated `"fox"` at the seam used to survive into the transcript
unchanged. The fix only ever removes an *exact*, case/punctuation-insensitive
match, and only when the two segments' spans independently overlap by more
than `SPAN_TOLERANCE_SECS` — text equality alone is not trusted, same reason
`is_fully_covered` is not either, which is what keeps a genuine "No. No." (no
span overlap) or "Wait. Wait, that's not right." untouched. It never revises
`prev_text` (already shown to the user) and never touches `is_fully_covered`
or the keep/suppress threshold itself — tightening *that* needs live
Deepgram traffic this sandboxed environment cannot obtain, which is exactly
`iris-dedup-verify-live-spans`'s separately-tracked job, not this fix's. A
re-emission that *reworks* the overlapping words rather than repeating them
verbatim is still unaddressed by either mechanism.

**The captain's real `history.jsonl` is not reachable from a Linux dev
sandbox — treat any task that leans on it as needing a different diagnostic
path.** It lives under `%LOCALAPPDATA%\IrisConfig\iris\` on the captain's own
Windows machine; this repo's dev/CI environments have no Windows interop, so
a from-scratch filesystem search here finds nothing (confirmed, not merely
assumed, while investigating the report above). When a task brief cites that
log as the evidence source, expect to substitute code inspection plus a
synthetic reproduction (built from invented text, never real transcript
content) and say so explicitly rather than fabricating log-derived numbers.

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
switching engines switches the wait. That per-engine bound still freezes the
pill and blocks a *second* dictation from starting for its full length —
`finish` genuinely takes that long regardless of engine. It is no longer also
a bound on Quit: see `iris-app`'s "Quit during an in-flight finalise" entry
below for why a tray Quit clicked mid-wait (Groq's 28s, the local engine's
20s, Deepgram's 6-14s below, all included) no longer has to wait for it. The
numbers here are still accepted, not trimmed — with `streams_partials` false
an early expiry costs the whole utterance — and on the default path,
Deepgram, the ordinary bound is 6s. Deepgram's worst case is not 6s: on a hold
that was still connecting with nothing salvageable, the connect grace below
can hold the *finalise itself* for the connect budget (8s from key-down)
*plus* a re-based 6s finalise from the moment the socket comes up, ~14s — and
a partial arriving after that grace was bought stops it growing without
giving any of it back, so a dictation that ends up with words can pay it too.
Quote that number, not the 6s, whenever this dictation's own latency is
weighed. Shortening any of these remains off the table — see the Quit entry
below for why that is a different question from Quit's own responsiveness.

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
partial afterwards still pays the worst case named above. A connect that
fails on its own terms still reports as one (from `Mark::StreamReady`,
engine-agnostically).

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
  minutes-apart gaps typical of real desktop use — see the withdrawn
  `WarmPool` note above.
- The 24-of-31 "Deepgram returned no transcript" failures a 2026-08 session
  log audit set out to explain were not a live bug: every one predated the
  `from_finalize` gate (`5e53f96`, 2026-08-01T18:03:56Z) merging to `main`,
  and zero recurred in the ~42 hours of real use after it landed. Established
  by timestamp correlation against the merge time, not by re-deriving the
  mechanism — if a similar cluster resurfaces, check the build actually
  includes that commit before assuming a new regression.
- **"Almost no audio reached the transcription engine" is still a live,
  recurring failure as of 2026-08-04 — the dominant remaining silent-dictation
  cause, not a closed one.** The original 2026-08 audit left two such
  failures unexplained (right after a 36-minute idle gap, 1.3s apart — a
  retry pattern, not an accidental tap). A third landed 2026-08-04T18:17:23Z,
  28.8 minutes after the prior dictation — same signature, well after
  `5e53f96` (2026-08-01) — so this is not the already-fixed pre-`5e53f96`
  mechanism recurring; it is a distinct, still-open one. In the 17 real
  dictations since `5e53f96` landed (`history.jsonl`), 6 failed (35%); 3 of
  those 6 are this exact message and a 4th ("heard the audio but returned no
  words") is the same `conclude()` branch shape from `sent_secs` (the bytes
  `pump_inner` actually forwarded), not the pre-fix mechanism either. All
  three confirmed "almost no audio" occurrences follow a gap of at least
  ~29 minutes since the prior dictation (36.3min, then a 1.3s retry, then
  28.8min) — well past any plausible warm-connection window, so a kept-alive
  spare (see the withdrawn `WarmPool` note above) would not have been
  available for any of them regardless.
  `audio_secs: 0.0` on these rows is the real value, not a masked one: the
  message itself comes from `deepgram.rs`'s `conclude()`, which sees only a
  `Transcript` and `sent_secs` and never touches the timeline — the stamp is
  in `Dictation::finish` and `Dictation::abandon`
  (`iris-core/src/dictation.rs`), and those are the only paths that can carry
  this error into the log. Every exit of both sets `self.timeline.audio_secs
  = self.audio_secs()` from `self.samples` (populated by `feed()`) before
  returning — so zero here means capture genuinely produced zero samples,
  not that a real capture got lost in translation on the way to the log.
  That rules out the connect-latency theory (real samples captured but not
  flushed in time) as the explanation for *these* rows and points back at
  capture itself. Suspected but *not established*: `MicAudio`
  (`iris-app/src/audio.rs`) opens its WASAPI stream once and never
  revalidates or reopens it, so a stream gone stale after a long idle gap
  (sleep, a Bluetooth mic, Windows audio power management) could produce a
  session with a live socket but no real samples. `capture.rs`'s cpal
  `on_error` callback is unconditional `eprintln!`, unchanged from `main` —
  see the console rule above for why that matters — so a stream-level
  failure is at least visible without `--verbose`. (An earlier pass on this
  branch regressed it to `vlog!`, gated behind `--verbose`; caught and
  reverted before reaching `main`, so nothing shipped from that detour — do
  not describe this as a fix this branch delivers.) A *stale-but-not-erroring*
  stream would never trip `on_error` at all, so its absence in the log does
  not clear this theory, and the callback firing only to stderr never reaches
  the session log either — `on_error` and `history.jsonl` are two disconnected
  diagnostics today. Confirming the theory needs live
  telemetry from a real Windows session (frame counts and real amplitude on
  the first hold after a long idle gap) that this repo cannot gather; do not
  assume it is fixed, and do not read the pre-`5e53f96` root-cause finding
  above as covering this failure mode too — they are different mechanisms
  that happen to share an error family.

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
- The design is **maintainer-decided** (2026-07-31, superseding the prior
  maintainer-locked fixed capsule of the same date; round 3 on 2026-08-01
  changed the default and the resting shape; round 4 on 2026-08-07 reversed
  round 3's shape and wave-row decisions; round 5 on 2026-08-07/08 answered
  round 4's two escalated open questions and brought the wave row back on a
  different foundation — see below): a shape-shifting shape that can open
  into a live-text ribbon, Prism dark default, Porcelain light,
  prism-triangle icon unchanged (tray uses the same mark via
  `tray::icon_rgba`). Full rationale and the two rejected alternatives:
  an internal early-stage UI-direction report, not part of this repository.
- **Round 3 (2026-08-01):** `show_live_text` defaults to `false`
  (`iris-app::config::Config`) — unchanged since, still the shipped default.
  Round 3 also widened the resting shape into a capsule holding a wave row
  and a large timer; round 4 reversed both, round 5 (below) partly reversed
  round 4.
- **Round 4 (2026-08-07):** the maintainer used round 3's shipped capsule and
  rejected it — *"I don't like the dashes... I like the design of the
  previous circle... I don't want the huge font."* Deleted the wave row
  outright and moved the timer to its own small `layout::TIMER_FONT` (10, not
  the live-text `TEXT_FONT` 15). The maintainer had only ever seen round 3's
  build (PR #22 was still unmerged), which round 5 (immediately below)
  discovered and corrected for.
- **Round 5 (2026-08-07/08,
  an internal design-direction memo, not part of this repository):**
  answered round 4's two escalated questions from three rendered options —
  *"the timeline you asked for [is] the sound wave itself... the marks
  become a real audio waveform that moves with your voice."* The wave row is
  back, but rebuilt on a rolling history of `Model::level()`
  (`Renderer::wave_history`, sampled while `Listening`, frozen once it ends)
  instead of round 3's single-current-level fan-out — each bar now reads a
  different historical moment, which is what makes it read as sound rather
  than "dashes". `layout::REST_W` grew from round 4's 102 to 118 to give the
  row real room, still clearly short of round 3's 128. `layout::TIMER_FONT`
  is unchanged at 10. See `crates/iris-overlay/README.md`'s "Round 5" section
  before touching `render::draw_wave`, `wave_bar_scale`, `Renderer::
  wave_history`, or `layout::REST_W` — and do not reintroduce a
  single-current-level-fanned-out bar row; that shape is what "dashes" means
  in every round's own words.
- **Legibility retune (2026-08-09):** round 5 shipped and the maintainer,
  reviewing the real installed build rather than zoomed evidence stills,
  reported the wave row moved but was too small and "clear colored" to see.
  Root cause was three of round 5's own `WAVE_*` constants compounding: bar
  *alpha* was tied to the same per-bar `scale` that already shrinks *height*
  (`colour.fade(alpha * scale)`), so a quiet bar was short and faint from one
  number instead of two independent signals, and `WAVE_IDLE_FLOOR` (`0.05`)
  plus `WAVE_RESPONSE_EXPONENT` (`2.4`) were tuned tight enough alone to make
  that collapse total — a quiet bar rounded to roughly one device px at 100%
  DPI. Fixed by decoupling them: alpha now blends `WAVE_BAR_ALPHA_FLOOR`
  (`0.62`) up to full by `scale` rather than using `scale` as alpha outright,
  `WAVE_IDLE_FLOOR` rose to `0.22`, `WAVE_RESPONSE_EXPONENT` eased to `1.7`,
  and `WAVE_BAR_W_FRAC` widened back to `0.4` now that height no longer
  collapses close enough to width to read as a dot. Do not retune any of
  these four without re-rendering at true, unscaled 1:1 pixel size and
  actually looking — `crates/iris-overlay/examples/desktop_composite.rs`
  composites a rendered frame onto a large neutral canvas at real device-px
  size for exactly this, because every evidence PNG this crate produces is a
  small crop that a docs viewer silently upscales, which is the same trap
  that shipped round 5 unreadable. See
  `crates/iris-overlay/docs/wave-visibility-evidence/` for the 1:1 evidence
  (both themes, quiet and loud, plus before/after) and the "Legibility
  retune" section of `crates/iris-overlay/docs/round5-evidence/README.md` for
  the full writeup.
- **Per-appearance reset state must be reachable from `window/win32.rs`'s real
  loop, not gated on `presence <= `-some-epsilon during the exit fade.** That
  loop stops calling `Renderer::draw` at all once `Model::is_idle()`
  (`state == Hidden && presence <= 0.0`), so a reset written inside `draw`'s
  own near-zero-presence branch — `wave_history`'s original clear — was dead
  code in the shipped app: `is_idle` is strictly narrower than that branch's
  condition, and the discrete per-frame presence step almost always lands
  exactly on `0.0`, skipping the tiny window in between. The next dictation
  opened showing the previous one's waveform tail; PR #22 shipped it. Fixed by
  keying the reset off the frame `draw` first sees the model re-enter
  `OverlayState::Listening` (`Renderer::listening_last_frame`) instead — see
  the doc comment on `Renderer::wave_history` in `render/mod.rs` for the full
  reasoning and why `Model::previous_state` cannot substitute. A test that
  only calls `draw` directly (as the headless harness and most tests in that
  file do) cannot catch this class of bug — it has no idle short-circuit to
  skip. A regression test must drive `tick`/`draw` the way the real loop does,
  skipping `draw` whenever `Model::is_idle()`.
- **The wave row's height not tracking real speech volume (2026-08-09) was an
  `iris-app` bug, not an `iris-overlay` one — check the input before retuning
  render constants again.** The maintainer reported bars that moved but did not
  read as "louder = taller". Root cause was `iris-app::audio::level()`
  (`crates/iris-app/src/audio.rs`), which feeds the overlay: its old
  `sqrt(rms / i16::MAX)` mapped realistic conversational-to-loud speech into
  a narrow `0.27..0.46` band on the `0.0..=1.0` meter, so `iris-overlay`'s own
  expansive response curve (`WAVE_RESPONSE_EXPONENT`, unchanged by this fix)
  had almost no spread to work with regardless of its own tuning. Fixed by
  mapping dBFS RMS linearly between a calibrated silence floor (`-50 dBFS`)
  and a loud-but-not-clipping ceiling (`-8 dBFS`) — see the doc comment on
  `audio::level` for the full reasoning and
  `crates/iris-overlay/docs/voice-level-evidence/` for 1:1 before/after
  evidence across a level sweep, both themes. If a future "the wave doesn't
  track volume" report shows up again, measure `audio::level()`'s actual
  output against representative PCM first — the compounding shape (an
  upstream compressor plus a downstream expansive curve) is exactly what
  made this one hard to see from the render code alone.
- Geometry is one capsule whose width animates between the rest width
  (`layout::REST_W`) and an open ribbon (`layout::RIBBON_MAX_W`) —
  `layout::ORB_D` is the shape's constant height, at every width. No solid
  rec-red (mint/sky live core). Motion timings in `motion.rs` are unchanged
  and stay maintainer criteria; every one is imported by the new shape, not
  copied.
- Geometry and motion are single-sourced; a `Theme` is colour only. Keep it that
  way or "same geometry, swapped tokens" stops holding.
- The rasteriser (`render/`) is portable and the window (`window/win32.rs`) is
  the only `cfg(windows)` file. That is what lets `cargo run --example pill-demo
  -- --filmstrip <dir>` produce reviewable PNGs of the real frames from Linux.
- The pill is a display: it never activates, never hit-tests, and never
  injects input. It **can** hold the live transcript text on screen while the
  ribbon is open, gated behind the opt-in `show_live_text` config flag (off by
  default since round 3) — a deliberate, maintainer-approved reversal of the
  original "never holds transcript text" rule. See
  `crates/iris-overlay/README.md` "The contract changed, and here is why"
  before touching this again.

## Settings window (`iris-app::window`)

- Opened on every deliberate launch by default — `main`'s `run` calls
  `App::open_window` right after `start_resident` unless `--background` was
  passed — and also from the tray's `Settings` item (`Command::OpenSettings`)
  or a later launch attempt (`single_instance`, below) once closed. One
  `iris-window` thread for process life either way, mirroring
  `tray`/`iris-overlay`. `--background` is the one exception, carried only by
  the Startup-folder shortcut `install.ps1` creates (`-RunAtLogin`): a
  boot-time autostart must stay quietly in the tray, not put a window on
  screen at every login — a maintainer-decided split (2026-08-07), not left to
  guess. The toolkit choice (`egui`/`eframe` over a WebView shell, a retained
  Win32 toolkit, or extending `iris-overlay`'s renderer) and the evidence for
  it are in `window/mod.rs`'s module docs — read that before reconsidering it.
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

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

Everything except microphone capture, the hotkey hook, text injection, and the
overlay window is portable and `#[cfg(windows)]`-free, so tests and the latency
harness run natively on Linux. Keep it that way — it is what makes the project
CI-testable at all.

## The application

`crates/iris-app/` is the product — the resident tray app that wires the other
crates into a working dictation loop. Its README is the map: the loop, the
config file, the tray, the overlay, the session log.

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

```bash
cargo run -p iris-app -- --demo-dictation                 # mock + dry-run + pill
cargo run -p iris-app -- --speak-wav assets/speech-16k.wav
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
overlay UI hangs off. `Dictation::start_with_session` takes an already-open
`Session` instead of opening one — see `App::prewarm` below.

Measured latency numbers, the budget breakdown, and open risks:
`docs/spike-findings.md`.

**A short hold can end before Deepgram says anything at all.** Connect + first
result is ~1-3 s; a hold shorter than that has nothing streamed back when the
key comes up, so the entire transcript depends on one post-`Finalize` flush.
`deepgram.rs`'s `pump` tracks `sent_secs` (audio actually forwarded) against
`Transcript::covered_secs` (Deepgram's own reported `start + duration`) and
withholds `CloseStream` until they converge. That wait ends on *progress*, not
a fixed timer: coverage often never converges (trailing silence is audio
Deepgram will never report words for), so waiting out a ceiling would spend it
on every ordinary dictation against a ~300 ms perceived-latency bar. The socket
is polled on `CATCHUP_STALL`, renewed by any inbound frame; silence for that
long is the stop signal. `FINALIZE_TIMEOUT` is only the absolute cap on the
whole wait, for a socket that chatters without converging — reaching it in
ordinary use means the stall detection is wrong, not that the cap is too tight.
Note the resulting distinction between `closing` (Finalize sent) and
`closed_stream` (CloseStream sent) — only the latter, *snapshotted before the
current frame is handled*, makes an inbound Metadata frame the sign-off,
because a very short hold can drain its whole audio backlog before the socket
is polled once and read the *open-time* Metadata after Finalize. Deepgram also
re-emits an already-finalised segment sometimes; that duplicate is knowingly
left in, because nothing in the text tells it apart from a user saying
"No. No." and a deleted word is worse than a doubled one. See the module doc
for the full mechanism and
`a_hold_shorter_than_first_response_does_not_abandon_the_backlog` for the
adversarial-fake-server regression test. `App` (`iris-app/src/app.rs`)
attacks the same latency from the other side: `App::prewarm` opens the next
session at the *start* of each `capture` (and once at startup), so its connect
cost overlaps the current hold and the idle time after it rather than the next
hold; `capture` consumes it via `Dictation::start_with_session` when it is
fresh enough (`PREWARM_STALE_AFTER`) *and* still alive — that constructor
returns `None` for a session that died while waiting — falling back to opening
fresh otherwise.

## Sharp edges

- API keys come from the environment only (`IRIS_DEEPGRAM_KEY`, `IRIS_GROQ_KEY`).
  The engine structs have hand-written `Debug` impls that redact the key; keep
  them if you add fields.
- rustls is pinned to the `ring` provider. The default (`aws-lc-rs`) needs cmake
  and nasm to cross-compile.
- The hotkey thread must only pump messages: Windows silently uninstalls a
  low-level hook whose callback exceeds ~300 ms.
- A failed dictation's `DictationRecord` in `history.jsonl` gets a fresh,
  zeroed `Timeline` (`App::dictate`'s `Err` arm), not the one `capture` was
  actually tracking. `"audio_secs":0.0` on an errored record is consistent
  with *some* audio having been captured before the failure, not proof that
  literally none was — don't over-read that field on error rows.

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
  `crates/iris-overlay/README.md` for rendering/WSL notes.
- The design is **captain-locked** (2026-07-31): Prism dark default, Porcelain
  light, listening-only telemetry chip, prism-triangle icon (tray uses the same
  mark via `tray::icon_rgba`). Source mockups:
  `/home/omar/firstmate/data/iris-design/`.
- Geometry is a compact HUD chip (`168×34`, radius 17 in `layout.rs`) — desk
  feedback tightened it from the mockup recorder bar so it does not read as a
  digital recording strip. No solid rec-red (mint/sky live core); spectrum stays
  on the waveform only. Motion timings in `motion.rs` stay captain criteria.
- Geometry and motion are single-sourced; a `Theme` is colour only. Keep it that
  way or "same geometry, swapped tokens" stops holding.
- The rasteriser (`render/`) is portable and the window (`window/win32.rs`) is
  the only `cfg(windows)` file. That is what lets `cargo run --example pill-demo
  -- --filmstrip <dir>` produce reviewable PNGs of the real frames from Linux.
- The pill is a display: it never activates, never hit-tests, holds no
  transcript text, and never injects input.

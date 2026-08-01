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

## Sharp edges

- API keys come from the environment only (`IRIS_DEEPGRAM_KEY`, `IRIS_GROQ_KEY`).
  The engine structs have hand-written `Debug` impls that redact the key; keep
  them if you add fields.
- rustls is pinned to the `ring` provider. The default (`aws-lc-rs`) needs cmake
  and nasm to cross-compile.
- The hotkey thread must only pump messages: Windows silently uninstalls a
  low-level hook whose callback exceeds ~300 ms.
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
  captain-locked fixed capsule of the same date): a shape-shifting orb that
  opens into a live-text ribbon, Prism dark default, Porcelain light,
  prism-triangle icon unchanged (tray uses the same mark via
  `tray::icon_rgba`). Full rationale and the two rejected alternatives:
  `data/iris-ui-directions/report.md` in the fleet's records.
- Geometry is one capsule whose width animates between an orb (`layout::ORB_D`)
  and an open ribbon (`layout::RIBBON_MAX_W`) — there is no fixed pill size any
  more. No solid rec-red (mint/sky live core). Motion timings in `motion.rs`
  are unchanged and stay captain criteria; every one is imported by the new
  shape, not copied.
- Geometry and motion are single-sourced; a `Theme` is colour only. Keep it that
  way or "same geometry, swapped tokens" stops holding.
- The rasteriser (`render/`) is portable and the window (`window/win32.rs`) is
  the only `cfg(windows)` file. That is what lets `cargo run --example pill-demo
  -- --filmstrip <dir>` produce reviewable PNGs of the real frames from Linux.
- The pill is a display: it never activates, never hit-tests, and never
  injects input. It **does** now hold the live transcript text on screen while
  the ribbon is open — a deliberate, captain-approved reversal of the previous
  "never holds transcript text" rule, with a config opt-out
  (`iris-app::config::Config`) that falls back to the orb-only presentation.
  See `crates/iris-overlay/README.md` "The contract changed, and here is why"
  before touching this again.

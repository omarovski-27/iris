# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

## Build and test

Windows-first, developed from WSL2. Full setup and the reasoning behind the
toolchain choice: `docs/dev-windows.md`.

```bash
cargo test --workspace                                  # portable; the default loop
cargo check --workspace --target x86_64-pc-windows-gnu  # type-check the Windows-only code
cargo build --release --target x86_64-pc-windows-gnu    # produces runnable .exe
./target/x86_64-pc-windows-gnu/release/iris-spike.exe   # WSL runs it as a real Windows process
```

Everything except microphone capture, the hotkey hook and text injection is
portable and `#[cfg(windows)]`-free, so tests and the latency harness run
natively on Linux. Keep it that way — it is what makes the project CI-testable
at all.

## iris-polish layout

- `crates/iris-polish/` is a **standalone** crate (own `Cargo.toml` / `Cargo.lock`). It is not yet a workspace member — build and test with `cargo test` / `cargo build` **inside** that directory.
- Cross-check Windows: `cargo build --target x86_64-pc-windows-gnu` from `crates/iris-polish/`.
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
it. Injection logic that *can* be tested
without the OS lives in `iris-core/src/text.rs` (UTF-16, surrogate pairs,
control characters) and is unit-tested.

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

## Sharp edges

- API keys come from the environment only (`IRIS_DEEPGRAM_KEY`, `IRIS_GROQ_KEY`).
  The engine structs have hand-written `Debug` impls that redact the key; keep
  them if you add fields.
- rustls is pinned to the `ring` provider. The default (`aws-lc-rs`) needs cmake
  and nasm to cross-compile.
- The hotkey thread must only pump messages: Windows silently uninstalls a
  low-level hook whose callback exceeds ~300 ms.

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

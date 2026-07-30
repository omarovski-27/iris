# iris-engine-local

Local (offline / private) speech-to-text engine layer for **Iris**.

Holds two engines behind a small internal trait that mirrors the core pipeline’s
streaming `Engine` shape (`start` → `feed` → partials → `finalize`). Adapting to
the merged core trait is intentionally a one-file follow-up.

## Architecture

```text
  mic frames (16 kHz mono PCM16)
           │
           ├──────────────────────────────┐
           ▼                              ▼
  ┌─────────────────────┐      ┌──────────────────────┐
  │ Streaming partials  │      │ Buffer full utterance│
  │ sherpa-onnx         │      └──────────┬───────────┘
  │ Zipformer int8      │                 ▼
  │ (ghost text only)   │      ┌──────────────────────┐
  └─────────┬───────────┘      │ Batch finalizer      │
            │                  │ whisper base.en q5_1 │
            │ Partial("…")     │ + Silero VAD gate    │
            ▼                  └──────────┬───────────┘
     live overlay                         │ Final("…")
                                   transcript of record
```

| Layer | Backend | Role |
|-------|---------|------|
| **Streaming partials** | [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) streaming Zipformer transducer **int8**, via the **official first-party** `sherpa-onnx` crate (not archived `sherpa-rs`) | Live ghost text while the user speaks. Finalization after the last frame is typically **0–40 ms** (report measured ~4 ms). Output has **no punctuation/casing** and is **never** injected. |
| **Transcript of record** | [whisper-rs](https://codeberg.org/tazz4843/whisper-rs) 0.16 → whisper.cpp `base.en` q5_1 | Punctuated, cased text. **Mandatory Silero VAD** (ggml) in front — Whisper hallucinates on pure silence 100% of the time when ungated. |

### Engine choice rationale (Parakeet vs whisper)

The evaluation report’s preferred finalizer is **NVIDIA Parakeet-TDT 0.6B q8_0**
through whisper.cpp’s new GGML backend: best accuracy + hygiene, zero silence
hallucination, ~0.5 s load / ~0.8–2.4 s finalize. **There is no Rust binding yet**
(the C feature landed the same day as the evaluation).

| Option | Pros | Cons |
|--------|------|------|
| **(a) Thin FFI to `parakeet.h`** | Best model now | Own build of whisper.cpp `libparakeet`, no upstream Rust support, high maintenance for v1 |
| **(b) whisper-rs + base.en + VAD (shipped)** | Mature crate, Windows docs, VAD already in whisper.cpp | Slightly weaker than Parakeet; *must* keep VAD |

**v1 choice: (b).** Do not let perfect block shipped. Parakeet is catalogued in
`ModelId::ParakeetTdt06bQ8_0` for a fast-follow once `whisper-rs` (or a thin
FFI) exposes `parakeet.h`.

## Model logistics

Models are **lazy-downloaded from Hugging Face** on first use into a configurable
directory:

- Env: `IRIS_MODEL_DIR`
- Default: `~/.cache/iris/models`
- API: `ensure_model` / `ZipformerPaths::ensure` / `WhisperPaths::ensure`
- Progress hooks: `ProgressFn` callback `(bytes_done, total_opt)`
- Integrity: expected size (±1%); optional SHA-256 when catalogued

| Artefact | Disk (approx.) |
|----------|----------------:|
| Zipformer streaming int8 (encoder+decoder+joiner+tokens) | **~71 MB** |
| whisper `base.en` q5_1 | **~60 MB** |
| Silero VAD ggml | **~0.9 MB** |
| Parakeet-TDT 0.6B q8_0 (follow-up) | **638 MB** |

**Default recommended set today: ~132 MB** (Zipformer + base.en + VAD).

## Features / build

| Feature | Enables |
|---------|---------|
| *(default)* | Trait, mock engine, model manager, audio helpers, offline tests |
| `streaming` | Official `sherpa-onnx` Zipformer |
| `whisper` | `whisper-rs` finalizer + Silero VAD |
| `native` | `streaming` + `whisper` |

```bash
# Offline unit tests (no download, no native deps)
cargo test -p iris-engine-local

# CLI with mock engine
cargo run -p iris-engine-local --example transcribe_wav -- \
  crates/iris-engine-local/fixtures/speech-16k.wav

# Real models (first run downloads ~132 MB)
cargo run -p iris-engine-local --example transcribe_wav --features native -- \
  --engine native crates/iris-engine-local/fixtures/speech-16k.wav

# Real-model integration tests (env-gated; not default CI)
IRIS_LOCAL_MODELS=1 cargo test -p iris-engine-local --features native \
  --test integration_native -- --nocapture
```

## LocalEngine trait

```rust
pub trait LocalEngine: Send + Sync {
    fn name(&self) -> &'static str;
    fn streams_partials(&self) -> bool { true }
    fn start(&self) -> Result<Box<dyn LocalSession>>;
}

pub trait LocalSession: Send {
    fn feed(&mut self, pcm: &[i16]) -> Result<()>;      // 16 kHz mono PCM16, any frame size
    fn partials(&self) -> &Receiver<LocalEvent>;
    // May block on batch work (Whisper). Final is often already on partials()
    // when finalize returns; do not call from a real-time audio callback.
    fn finalize(&mut self) -> Result<()>;
}
```

Maps 1:1 to core `Engine::open/push/events/finish` when that crate merges.

## Windows notes

Iris is Windows-first. Native deps:

### sherpa-onnx (`streaming`)

- Official crate downloads **prebuilt static libraries** from GitHub releases when
  `SHERPA_ONNX_LIB_DIR` is unset (including
  `sherpa-onnx-v*-win-x64-static-MT-Release-lib.tar.bz2`).
- **Native Windows (MSVC)**: supported path for end users.
- **Cross-compile from Linux/WSL → `x86_64-pc-windows-gnu` (proven blocker):**
  prebuilt archive is **MSVC/MT** (`static-MT-Release`). Linking fails with:
  `could not find native static library sherpa-onnx-c-api` when the MinGW
  linker is used (MSVC `.lib` is not a MinGW archive). Options: build on
  Windows with MSVC; set `SHERPA_ONNX_LIB_DIR` to a MinGW-built tree; or use
  the crate's `shared` feature with MinGW DLLs. **Not worked around in this crate.**

### whisper-rs (`whisper`)

- Builds whisper.cpp from source via `whisper-rs-sys` (needs **CMake**, a C++
  toolchain, and **libclang** for bindgen — e.g. `LIBCLANG_PATH=/usr/lib/llvm-18/lib`
  on Ubuntu).
- Canonical repo: [codeberg.org/tazz4843/whisper-rs](https://codeberg.org/tazz4843/whisper-rs)
  (GitHub mirror is archived). crates.io `0.16.0`.
- **Windows (native)**: see upstream `BUILDING.md` — MSVC + optional CUDA/Clang, or
  msys2/mingw for CPU-only.
- **Cross-compile from WSL → `x86_64-pc-windows-gnu` (proven blocker):** even with
  `g++-mingw-w64-x86-64` and CMake installed, whisper.cpp's `ggml-cpu.c` fails
  under MinGW headers:
  `error: unknown type name 'THREAD_POWER_THROTTLING_STATE'` (Windows SDK
  symbols present in MSVC headers, missing/outdated in Ubuntu's mingw-w64
  headers). Prefer a **native Windows** build (MSVC or msys2 with current
  headers) rather than patching ggml in this crate.

### Default crate (no native features)

Compiles cleanly on Linux host tests **and** `x86_64-pc-windows-gnu` (mock +
model manager only) — this is the default CI surface.

### Proven matrix (this worktree)

| Target | Features | Result |
|--------|----------|--------|
| `x86_64-unknown-linux-gnu` | default | ✅ `cargo test` green |
| `x86_64-unknown-linux-gnu` | `native` | ✅ `cargo check` green (needs libclang) |
| `x86_64-pc-windows-gnu` | default | ✅ `cargo check` green |
| `x86_64-pc-windows-gnu` | `streaming` | ❌ MSVC prebuilt lib vs MinGW linker |
| `x86_64-pc-windows-gnu` | `whisper` / `native` | ❌ MinGW missing `THREAD_POWER_THROTTLING_STATE` |

## Fixtures

| File | Purpose |
|------|---------|
| `fixtures/silence-0.5s-16k.wav` | Pure digital silence — finalizer must return `""` |
| `fixtures/speech-16k.wav` | Short espeak-ng utterance for partials / smoke |

## License

MIT (Iris). Engine bindings: Apache-2.0 (`sherpa-onnx`), Unlicense (`whisper-rs`).

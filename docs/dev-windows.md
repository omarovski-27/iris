# Building Iris for Windows from WSL2

Iris is Windows-first, but it is comfortable to develop from WSL2: you get a Linux
shell and editor, cross-compile a real `.exe`, and — the part that makes this
pleasant — **run that `.exe` directly from the WSL prompt**. Windows interop
launches it as a genuine Windows process with real access to WASAPI, the
keyboard hook and `SendInput`. There is no copying to `/mnt/c` and no separate
Windows shell.

Verified on: WSL2 (Ubuntu 24.04) on Windows 11, Rust 1.97, mingw-w64 13.2.

## One-time setup

```bash
# 1. Rust, if you don't have it
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"

# 2. The Windows target
rustup target add x86_64-pc-windows-gnu

# 3. The mingw-w64 cross-linker
sudo apt-get update && sudo apt-get install -y gcc-mingw-w64-x86-64
```

That is the whole toolchain. `.cargo/config.toml` in the repo already points the
target at `x86_64-w64-mingw32-gcc`, so no environment variables are needed.

### Why `gnu` and not `msvc`

`x86_64-pc-windows-gnu` needs one apt package and nothing else. `cargo-xwin`
(for the `msvc` target) works too, but it downloads and caches the MSVC CRT and
Windows SDK headers, which is a bigger, slower, licence-encumbered dependency
for no benefit at this stage. The `windows` crate ships import libraries for the
`gnu` target, `cpal`'s WASAPI backend is pure Rust over those bindings, and both
build clean.

The one place this choice shows up is TLS. `rustls` 0.23 defaults to the
`aws-lc-rs` crypto provider, which wants `cmake` and `nasm` on the host to cross
compile. Iris pins the pure-Rust `ring` provider instead
(`crates/iris-core/Cargo.toml`), which builds with nothing but the mingw
toolchain. If you ever see a `aws-lc-sys` build failure, something has pulled in
the default provider — check for a dependency that enables `rustls/aws-lc-rs`.

## The build loop

```bash
# Portable code: tests, and the latency harness. Fast, no Windows needed.
cargo test --workspace
cargo run --release --bin iris-harness -- --engine mock

# The Windows binaries
cargo build --release --target x86_64-pc-windows-gnu

# The product app (tray, config, session log)
./target/x86_64-pc-windows-gnu/release/iris.exe --list-devices
./target/x86_64-pc-windows-gnu/release/iris.exe --speak-wav assets/speech-16k.wav

# The latency spike (interactive checklist still lives here)
./target/x86_64-pc-windows-gnu/release/iris-spike.exe --self-test
./target/x86_64-pc-windows-gnu/release/iris-spike.exe --engine mock
```

Most of the codebase is portable on purpose — only microphone capture, the
hotkey hook, text injection, and the overlay window are `#[cfg(windows)]`. So
the inner loop (`cargo test`, `cargo clippy`, the harness) runs natively on
Linux in seconds, and you only cross-compile when you need to touch the OS
layer.

To type-check the Windows-only code without a full build:

```bash
cargo check --workspace --target x86_64-pc-windows-gnu
```

## Gotchas

**Run the `.exe` by path, not through `cmd.exe`.** `cmd.exe` cannot use a
`\\wsl.localhost\...` UNC path as a working directory and will warn and fall
back to `C:\Windows`. Executing the binary directly (`./target/.../foo.exe`)
avoids this entirely — WSL's binfmt handler launches it with the WSL directory
as its cwd.

**Console output is Windows-side.** Stdout comes back to your WSL terminal, but
the process is a Windows process: it sees Windows paths, the Windows audio
stack, and the Windows foreground window.

**The `.exe` is large** (~33 MB release). That is debug symbols (`debug = 1` in
the workspace profile, kept for profiling the audio path) plus the statically
linked TLS stack. Set `debug = 0` and it drops considerably.

**Antivirus.** A program that installs a global keyboard hook and synthesises
keystrokes is, structurally, what a keylogger does. Real-time protection may
flag it. This is inherent to the product and will need code signing before
release.

**Text injection has no automated check.** `--self-test` never runs it.
Windows only delivers synthetic keystrokes on the desktop the user is actually
looking at — it returns `ERROR_ACCESS_DENIED` on any other desktop — so there
is no sandbox for it: any automated check would type into whatever session is
live. Injection is verified by the interactive checklist in
`crates/iris-spike/README.md`. See `docs/spike-findings.md`.

## Cross-compiling isn't the same as testing on Windows

The `.exe` produced here is a normal Windows binary and behaves like one, but a
few things genuinely differ from a desktop session and need a real machine:

- foreground-window behaviour and anything that depends on focus,
- per-device WASAPI buffer sizes on real hardware,
- how injection lands in specific target applications.

The interactive checklist in `crates/iris-spike/README.md` covers these.

# iris-spike

The end-to-end latency spike: hold a key, speak, release, text appears in
whatever window has focus — with a latency breakdown printed after each
dictation.

Two binaries:

| binary | what it is for | needs |
| --- | --- | --- |
| `iris-spike` | the real thing: hotkey, microphone, injection | Windows |
| `iris-harness` | latency measurement from a WAV file | nothing — runs anywhere |

## Quick start (no key, no network, no microphone)

```bash
cargo run --release --bin iris-harness -- --engine mock
```

This streams the committed speech fixture through the full engine path at
speaking speed and prints the same latency breakdown the live app does. It is
the CI check, and the fastest way to see what the pipeline measures.

## Running the spike on Windows

Build from WSL (see `docs/dev-windows.md` for the one-time setup), then run the
`.exe` directly:

```bash
cargo build --release --target x86_64-pc-windows-gnu
./target/x86_64-pc-windows-gnu/release/iris-spike.exe --engine mock
```

Then **hold Right-Ctrl, speak, release**. The transcript is typed into the
focused window and a breakdown is printed.

With a real engine:

```bash
export IRIS_DEEPGRAM_KEY=...     # streaming: the fast path
./target/x86_64-pc-windows-gnu/release/iris-spike.exe --engine deepgram

export IRIS_GROQ_KEY=...         # batch: the comparison point
./target/x86_64-pc-windows-gnu/release/iris-spike.exe --engine groq
```

Keys are read only from the environment, never from a file or an argument.

### Useful flags

| flag | |
| --- | --- |
| `--engine mock\|deepgram\|groq` | default `mock` |
| `--hotkey rctrl\|f9\|rshift\|ralt\|capslock\|…` | default `rctrl` |
| `--inject sendinput\|clipboard` | default `sendinput` |
| `--dry-run` | print the transcript instead of injecting it |
| `--warm-capture` | keep the microphone open between dictations |
| `--no-suppress` | let the hotkey reach the focused app too |
| `--device <substring>` | pick a microphone; `--list-devices` to see them |
| `--save-wav <dir>` | write each dictation's audio for debugging |
| `-v` | diagnostics on stderr |

## Reading the latency report

```
── dictation #1 ──────────────────────────────
  engine      deepgram
  audio       5.38 s
  partials    15
  transcript  "the quick brown fox jumps over the lazy dog…"

  key-press → session open               0.08 ms
  key-press → first audio in            12.40 ms
  key-press → stream ready             121.93 ms
  first audio → first partial          341.90 ms
  key-release → final transcript       160.58 ms
  final transcript → injected            3.10 ms
  ---------------------------------------------------------------------
  PERCEIVED key-release → text on screen                      163.68 ms
```

**Only the last line is the product metric.** Everything above the key release
happens while you are still talking, so it costs the user nothing — that is the
entire point of streaming while speaking. A `key-press → stream ready` of 120 ms
is invisible during a 5-second utterance.

The lines above it still matter, just for different reasons:

- **key-press → first audio in** is how much of your first word gets clipped.
- **first audio → first partial** is how quickly an overlay could show live text.
- **key-release → final transcript** is the engine's flush round-trip. This is
  what dominates perceived latency, and what a batch engine cannot avoid paying
  over the whole utterance.

## Non-interactive checks

```bash
./target/x86_64-pc-windows-gnu/release/iris-spike.exe --self-test
```

Verifies that audio devices enumerate, the low-level keyboard hook installs, and
the engine path produces a transcript from the WAV fixture.

It does **not** test text injection. Windows only delivers synthetic keystrokes
on the desktop the user is looking at — it returns `ERROR_ACCESS_DENIED`
anywhere else — so an automated injection test necessarily types into whoever is
using the machine. `--injection-test` opts in; only pass it when you are sitting
at the machine and expecting text to appear.

## Interactive test checklist

The things that cannot be verified without a person at the keyboard. Run
`--engine mock` first: it needs no key, and any problem you see is then
definitely in the OS layer rather than the network.

1. **Injection lands.** Focus Notepad, hold Right-Ctrl, say a sentence, release.
   The transcript should appear at the caret.
2. **Injection cost.** Watch how the text arrives. If it visibly *types itself*
   character by character rather than appearing at once, `--inject sendinput` is
   too slow for the budget — compare with `--inject clipboard`, and see the
   injection section of `docs/spike-findings.md`, which expects exactly this.
3. **The hotkey is invisible to the app.** In Notepad, holding Right-Ctrl should
   not trigger anything. Then try a browser: Right-Ctrl must not fire Ctrl-based
   shortcuts. (Pass `--no-suppress` to see the difference.)
4. **First word is not clipped.** Start speaking the instant you press. Compare
   with `--warm-capture`, which removes device-open time from the key path.
5. **Unicode.** Dictate something with an accent or an em dash and confirm it
   arrives intact on your keyboard layout.
6. **Repeat dictations.** Several in a row, with pauses, should each be clean —
   no stale audio, no leaked partials from the previous one.
7. **Elevated windows.** Injection into an elevated window will fail unless Iris
   is elevated too. Confirm the error message says so rather than failing
   silently.

## Measuring latency

```bash
# 10 runs at speaking speed
cargo run --release --bin iris-harness -- --engine deepgram --runs 10

# What the budget looks like at a given network cost, without a key:
#   --simulate <connect_ms>,<finalize_ms>
cargo run --release --bin iris-harness -- --engine mock --simulate 120,160

# Machine-readable
cargo run --release --bin iris-harness -- --engine mock --runs 20 --json
```

The harness feeds audio at **1× wall clock** by default, because that is the
only honest way to measure a streaming engine — `--fast` lets the engine work
ahead of the clock, which makes the numbers meaningless (it is there for quick
correctness runs).

The harness never injects text, so its `PERCEIVED` line stops at the final
transcript and says so.

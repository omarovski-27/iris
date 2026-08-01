# Latency spike: findings

**Verdict: the architecture works, and the risk is not where we expected.**
Streaming-while-speaking makes transcription latency essentially free, and the
pipeline's own overhead is ~2 ms. The unverified item is the cloud round-trip
(no API key was available). The *surprise* is that text injection, treated as an
afterthought in the brief, looks like the largest single item in the budget.

Everything below was measured on this machine unless it says otherwise.

---

## 1. What was measured

| | |
| --- | --- |
| Host | WSL2 (Ubuntu 24.04) on Windows 11, cross-compiled to `x86_64-pc-windows-gnu` |
| Audio | committed fixture `assets/speech-16k.wav`, 5.38 s, 16 kHz mono (espeak-ng) |
| Engines exercised | `mock` (measured), `deepgram` + `groq` (compile + unit-tested, **not** run) |
| Microphone | not used — no one could speak into it |

**No API key was available during this spike**, so the cloud numbers below are
modelled, not measured. That is the headline caveat and section 6 says exactly
what remains to verify.

## 2. Pipeline overhead is negligible

Harness, `--engine mock`, 5 runs at speaking speed. The mock is instant and
offline, so what is left is Iris's own cost:

```
perceived (key-release → transcript)   n=5 min=0.00 p50=0.00 p95=0.00 max=0.00 (ms)
key-press → session open                                          0.08 ms
key-press → first audio in                                        0.10 ms
```

Sub-millisecond. Resampling 48 kHz stereo → 16 kHz mono (63-tap FIR + linear
interpolation) costs ~3 M multiply-accumulates per second, invisible against the
audio callback itself. **Nothing we wrote is on the critical path**; the budget
belongs entirely to the network and the OS.

## 3. Streaming is what buys the target

Modelling a plausible Deepgram profile (120 ms connect, 160 ms flush) with
`--simulate 120,160`:

```
key-press → stream ready             121.93 ms      ← hidden behind speech
first audio → first partial          341.90 ms      ← hidden behind speech
key-release → final transcript       160.58 ms      ← the user waits for this
─────────────────────────────────────────────
PERCEIVED                            160.58 ms
```

The 122 ms connection setup lands *while the user is drawing breath* and costs
nothing. Only the flush round-trip is felt. That is the whole thesis, and it is
why `Engine` is a session (`open → push* → finish → Final`) rather than
`transcribe(pcm) -> String`: the batch signature structurally cannot hide any of
this, and it is the signature every slow open-source dictation tool has
somewhere in it.

Sensitivity — perceived latency tracks the engine's finalisation cost almost
exactly, since our overhead is ~1.5 ms:

| modelled flush | measured perceived (p95) | verdict vs 300 ms |
| --- | --- | --- |
| 160 ms | 163 ms | ✓ comfortable |
| 400 ms | 419 ms | ✗ misses |

So the target holds **iff Deepgram's finalisation flush is under roughly
280 ms.** That is the single number to measure the moment a key exists. (That
flush is now awaited explicitly: `finish()` waits for Deepgram's own
`from_finalize` acknowledgement before `CloseStream` rather than closing
immediately — see `AGENTS.md` and `crates/iris-core/src/engine/deepgram.rs`.
The ack plus a 150 ms quiet window for a flush split across frames *is* the
finish line: the final transcript is reported then, so `CloseStream` and the
Metadata sign-off are teardown and add no round trip to the number above. That
window is the one deliberate addition to the number, and it is inside the
budget headroom only while the flush latency itself stays well under the
280 ms bar.)

## 4. Injection is the unexpected risk

The brief treated `SendInput` as the default and clipboard paste as a fallback
"only if SendInput proves problematic". On this machine, it proved problematic.

Measured against an in-process `EDIT` control, median of 5, 83-character
transcript ≈ one sentence of dictation:

| transcript | SendInput (call / visible) | clipboard (call / visible) |
| --- | --- | --- |
| 10 chars | 49 ms / 32 ms | 8 ms / 12 ms |
| 40 chars | 107 ms / 100 ms | 7 ms / 11 ms |
| 83 chars | 129 ms / 145 ms | 7 ms / 9 ms |
| 200 chars | 266 ms / 378 ms | 7 ms / 11 ms |
| 500 chars | 554 ms / 1044 ms | 7 ms / 11 ms |

`SendInput` costs roughly **1 ms per character** and is linear; clipboard paste
is flat at under 15 ms regardless of length. A one-sentence dictation would
spend ~145 ms getting text on screen — comparable to the entire transcription
round-trip, and enough on its own to put a 200-character dictation over budget.

**Confidence: medium. This needs re-verification before it drives a decision.**
The measurement harness took foreground focus from the live session to do its
work, which disrupted real usage; it has been removed, and no injection has been
run since. The numbers above are the data it produced before removal. Two
specific reasons to re-check them: the target was an in-process control rather
than a separate application, and the run happened under WSL interop. The
*shape* (linear vs constant, and a ~30× gap) is robust across every run; the
absolute coefficient is what wants confirming.

**Recommendation:** default to clipboard paste for the real app, with
save-and-restore of the previous clipboard contents, keeping `SendInput` for the
apps that refuse paste. Do not act on this until step 1 of section 6 confirms it
on a real desktop — the spike still ships with `--inject sendinput` as the
default, per the brief, and both paths are implemented and switchable with one
flag.

### Why injection cannot be tested automatically

Windows delivers synthetic input only on the **input desktop** — the one the
user is looking at. `SendInput` from a thread on any other desktop returns
`ERROR_ACCESS_DENIED`; this was tried, on a purpose-created private desktop, and
that is what it returns. There is therefore no sandbox: any automated injection
test types into somebody's live session.

So `--self-test` does not test injection at all; the interactive checklist in
`crates/iris-spike/README.md` is the sole verification path for it. This is a real limitation, not an oversight, and
it applies to any future CI for this project.

## 5. Smaller findings

**Cold microphone open costs the first word.** Opening the WASAPI stream on
key-press delays first audio. `--warm-capture` keeps the stream open and removes
it from the key path, at the cost of holding the mic indicator on. The real app
should keep the stream warm and gate frames instead — the indicator is a
privacy-UX decision, not a technical one.

**Suppressing the hotkey is not optional.** A low-level hook that lets
Right-Ctrl through arms every Ctrl shortcut in the app being dictated into.
Iris returns non-zero from the hook to swallow it (`--no-suppress` to compare).

**Injected input must be filtered out of our own hook.** Synthetic events carry
`LLKHF_INJECTED`; without ignoring them, injecting a transcript can retrigger
dictation. Handled in `hotkey.rs`.

**A hook that stalls is silently uninstalled.** Windows drops a low-level hook
whose callback exceeds `LowLevelHooksTimeout` (300 ms default). Hence the
dedicated hotkey thread that does nothing but pump messages.

**`ring`, not `aws-lc-rs`.** rustls 0.23's default provider needs cmake and nasm
to cross-compile; `ring` builds with just mingw. Pinned in
`crates/iris-core/Cargo.toml` — see `docs/dev-windows.md`.

**Groq is the control group, not a fallback.** It is implemented behind the same
trait and is genuinely fast per-token, but being request/response it cannot
start until the audio is complete, so its whole cost lands after key-release.
`Engine::streams_partials()` returns false for it so the UI knows not to promise
live text. It is useful precisely because it shows the cost of the *architecture*
rather than of the vendor.

## 6. What remains to verify

In priority order:

1. **Injection cost on a real desktop, into a real app** (Notepad, VS Code, a
   browser). Decides SendInput vs clipboard, which is currently the largest
   uncertain item in the budget. Needs a person at the keyboard.
2. **Deepgram flush latency with a key.** `iris-harness --engine deepgram
   --runs 20`. The target holds if p95 of `key-release → final transcript` is
   under ~280 ms. Everything else is already proven. Informal signal, not a
   benchmark: live protocol verification of the `from_finalize` wait (see
   `AGENTS.md`) saw acks arrive in roughly 200-550 ms across a handful of hold
   shapes — which straddles the ~280 ms bar in section 3, so it is not safe to
   assume that bar holds for every hold. Ad-hoc observations on a few holds are
   not a p95; run the harness above to settle it.
3. **Real microphone end-to-end**, via the checklist in the spike README.

*Resolved since:* **connection reuse.** A pre-warmed spare connection was built
and then measured out — Deepgram closes an idle socket in roughly 12-15 s,
far short of the gaps between real dictations. The short-utterance case it was
meant to help is covered instead by the `from_finalize` wait. See `AGENTS.md`
(Sharp edges) for the measurement.

## 7. Architecture recommendation

**Keep the threading model.** Three threads, no shared locks:

- **hotkey thread** — pumps Windows messages, nothing else (see above);
- **audio thread** — owned by WASAPI; resamples and hands frames to a channel,
  never blocks, never allocates in the steady state;
- **dictation thread** — owns the engine session and the timeline; blocks only
  in `select!` over {audio, engine events, hotkey}, so every input is acted on
  the instant it arrives rather than at a poll tick.

Network I/O sits on a shared 2-thread Tokio runtime created once per process.
Nothing in the pipeline waits on anything else in the pipeline, which is why
section 2 reads the way it does.

**Where the overlay hooks in.** `Dictation::events()` hands out a receiver of
`TranscriptEvent`, and `absorb_event` folds one into the timeline. The spike
already uses this seam to print live partials to the terminal (`pipeline.rs`).
The pill itself lives in `crates/iris-overlay` (`OverlayHandle`) and never
holds or draws transcript text. App wiring is `iris-app::pill::OverlayPill`
(a `PillSink`); see `crates/iris-app/README.md`.

This matters for perceived speed beyond the measured numbers: the pill is on
screen for the whole utterance (spectrum + telemetry), so key-release is a
state change on something already visible rather than a surface appearing from
nothing, which reads as faster than it is.

**Keep the `Engine` trait as-is.** It survived three implementations with very
different shapes (streaming websocket, batch HTTP, synchronous mock) without
changing, which is the main evidence that it is the right abstraction. A local
Whisper engine is the same shape: `open` loads/reuses the model, `push` feeds
the ring buffer, `finish` runs the final decode.

**Risks carried forward.**

- *Injection*, as above — the one open architectural question.
- *Antivirus*. A global keyboard hook plus synthetic keystrokes is structurally
  a keylogger. Code signing will be needed before release.
- *Elevated windows*. Injection into a higher-integrity window fails; Iris
  reports this explicitly rather than silently dropping text, but the real fix
  is a user-visible explanation.
- *Endpointing*. The spike ends the utterance on key-release only. Deepgram's
  `endpointing` also segments on silence, which is what we want for accuracy but
  means a long trailing pause produces an extra segment to flush.

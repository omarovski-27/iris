# Voice-level evidence

The maintainer's report on the installed build (`iris-v0.1.0-2026-08-09`, main
`9ee805f`): *"Would be better if the waves are moving when there is speech...
the waves are higher whenever the volume is higher."* Round 5 and the
legibility retune (see `../round5-evidence/` and `../wave-visibility-evidence/`)
made the bars visible; they did not make their *height* track the speaker's
actual volume — normal-to-loud speech all landed in a narrow middle band of
the bar's travel, so louder speech barely looked louder.

## Root cause

Not the wave renderer. `crates/iris-app/src/audio.rs::level()` — the function
that turns a captured PCM frame into the `0.0..=1.0` number fed to
`pill.update_level()` — used `sqrt(rms / i16::MAX)`. Measured against
synthetic PCM at calibrated dBFS levels
(`crates/iris-app/src/audio.rs`'s `level_spans_most_of_its_range_across_realistic_speech_levels`
test):

| Scenario (RMS)         | Old `level()` | New `level()` |
|-------------------------|:---:|:---:|
| room noise (-55 dBFS)   | 0.06 | 0.00 |
| quiet speech (-37 dBFS) | 0.12 | 0.31 |
| normal speech (-23 dBFS)| 0.27 | 0.64 |
| loud speech (-13 dBFS)  | 0.46 | 0.88 |
| near-clipping (-5 dBFS) | 0.75 | 1.00 |

Ordinary conversational speech never got past `0.27` on a `0.0..=1.0` meter,
and even loud speech reached only `0.46` — half the bar's travel unclaimed
regardless of how loud the speaker got, short of nearly clipping the mic. The
overlay's own response curve (`WAVE_RESPONSE_EXPONENT` in
`crates/iris-overlay/src/render/mod.rs`, unchanged by this fix) then further
compresses whatever narrow band it is handed, compounding the problem instead
of causing it.

## The fix

`audio::level()` now maps dBFS RMS linearly between a calibrated silence floor
(`-50 dBFS`) and a loud-but-not-clipping ceiling (`-8 dBFS`) — the standard
construction for a loudness meter, and parameterised by two numbers that
describe real acoustic levels rather than a curve shape tuned to whatever
narrow band happened to be measured. See the doc comment on
`audio::level` for the full reasoning. Nothing in `iris-overlay` changed:
this is entirely an input-mapping fix, upstream of the renderer.

## Files

Steady-state (`--hold-level`, given several seconds for the one-pole level
smoothing to settle and the wave row's rolling history to fill — the same
method `../wave-visibility-evidence/` used) rather than the oscillating
synthetic-speech envelope, so frames at different levels are directly
comparable. Composited onto a 700×300 neutral desktop canvas at the
renderer's real, unscaled device-pixel output (`--scale 1.0`, 100% DPI) —
nothing here is zoomed; if it looks small, that is the actual size.

- `{prism,porcelain}-{silence,quiet,normal,loud}-1x1-desktop.png` — both
  themes, held at the new `level()`'s output for each scenario above
  (`0.00`, `0.31`, `0.64`, `0.88`).
- `*-BEFORE.png` — the same four scenarios, held at the *old* `level()`'s
  output for the identical PCM (`0.038`, `0.119`, `0.266`, `0.461`) —
  `iris-overlay` itself is unchanged, so this isolates exactly what the input
  mapping used to throw away. Not the fix; kept so the difference is a diff
  of pixels, not a claim in prose.

## What to look for

- AFTER: quiet, normal and loud are three visibly distinct heights, with loud
  reaching close to the capsule's full deflection.
- BEFORE: normal and loud are hard to tell apart, and loud alone looks about
  like AFTER's normal — the compression the fix removes.
- Silence stays low and flat in both — the fix widens the range above
  silence, it does not raise the floor.

## Reproducing

```bash
# Render steady-state frames (transparent PNGs) at a held mic level:
cargo run -p iris-overlay --example pill-demo -- --theme prism --hold-level 0.64 \
  --filmstrip /tmp/iris-wave --filmstrip-step 200 --cycles 1
# ...then pick a late frame (0030-listening.png here) once the level and the
# wave row's rolling history have both settled (~3.4 s at WAVE_SAMPLE_INTERVAL_MS
# x WAVE_HISTORY_LEN).

# Composite a frame onto a true-scale simulated desktop:
cargo run -p iris-overlay --example desktop_composite -- --bg dark --canvas 700x300 \
  /tmp/iris-wave-out /tmp/iris-wave/0030-listening.png

# The hold-level values above come from crates/iris-app/src/audio.rs::level()
# applied to synthetic PCM at calibrated dBFS levels — see
# level_spans_most_of_its_range_across_realistic_speech_levels in that file.
```

## What this does not claim

This host cannot run the Windows binary or a real microphone. Nothing here
demonstrates end-to-end behaviour against a live captured voice — only that
(a) `audio::level()` now measurably spreads realistic speech RMS across most
of `0.0..=1.0` rather than a narrow middle band (unit-tested), and (b) the
existing, unmodified wave renderer visibly reflects that wider input range at
true desktop scale (this evidence set). Confirming it against a real person
speaking needs `docs/first-run-checklist.md` on real Windows hardware.

# Wave visibility evidence

The captain's report on the installed build (`iris-v0.1.0-2026-08-08`, main
`99988fc`): *"I like that the font is smaller. But there are no sound waves.
The dashes are just plain and nothing is moving. Or no. They are moving, but
they are so small and they are clear colored. So they aren't visible."*

`../round5-evidence/` documents the wave row's motion and its own regenerated
stills, but every image there is 528×102 px composited large in whatever
viewer opens it — which is exactly the trap the captain's own feedback names:
a rendering that was actually judged zoomed in, where a faint bar looks
obvious, reads as invisible on a real monitor. These files exist to remove
that trap: each PNG is the same kind of frame, composited onto a
700×300 neutral desktop canvas at the renderer's real, unscaled device-pixel
output (`--scale 1.0`, i.e. 100% DPI — the same size math the README's `~224
x 106 device px window` comment describes). Nothing here is zoomed. If it
looks small in this file, that is the actual size.

## Files

Steady-state (`--hold-level`, given several seconds for the one-pole level
smoothing to settle and the wave row's rolling history to fill) rather than
the oscillating synthetic-speech envelope, so a quiet frame and a loud frame
are directly comparable and neither can land on an arbitrary point of a
moving signal by chance.

- `prism-quiet-1x1-desktop.png` / `prism-loud-1x1-desktop.png` — Prism (dark
  theme) on a dark desktop, held at level `0.04` / `0.85`.
- `porcelain-quiet-1x1-desktop.png` / `porcelain-loud-1x1-desktop.png` —
  Porcelain (light theme) on a light desktop, same levels. This is the pairing
  the captain's "clear colored" complaint most likely singles out: pastel
  spectrum stops against a near-white glass shell is the lowest-contrast
  combination this design has.
- `*-BEFORE.png` — the same four shots, rendered from the code as shipped in
  `a33769b` (before this fix), for direct comparison. Not the fix; kept only
  so the difference is a diff of pixels, not a claim in prose.

## What to look for

- Loud is unambiguously taller and bolder than quiet, in both themes.
- Quiet is still genuinely visible — a short row of ink, not a blank capsule.
  Compare against the matching `*-BEFORE.png`: there, quiet is at or past the
  edge of visible, and even *loud* is faint.
- The row still reads as bars, not dots or a solid smear.

## Reproducing

```bash
# Render steady-state frames (transparent PNGs) at a held mic level:
cargo run --example pill-demo -- --theme prism --hold-level 0.04 \
  --filmstrip /tmp/iris-wave --filmstrip-step 200 --cycles 1
# ...then pick a late frame (e.g. 0030-listening.png) once the level and the
# wave row's rolling history have both settled.

# Composite a frame onto a true-scale simulated desktop (scratch tool, not
# part of the crate — see crates/iris-overlay/examples/desktop_composite.rs):
cargo run --example desktop_composite -- --bg dark --canvas 700x300 \
  /tmp/iris-wave-out /tmp/iris-wave/0030-listening.png
```

## Root cause, for anyone re-tuning these constants again

See "Legibility retune" in `../round5-evidence/README.md` and the doc
comments on `WAVE_IDLE_FLOOR`, `WAVE_RESPONSE_EXPONENT`, `WAVE_BAR_W_FRAC`
and `WAVE_BAR_ALPHA_FLOOR` in `crates/iris-overlay/src/render/mod.rs`. In
short: round 5's alpha was tied to the same per-bar `scale` that already
shrinks height, so a quiet bar was short *and* faint from one number instead
of two independent signals, and the height floor and response curve were
tuned tight enough on their own to make that collapse total. Retuning any one
of the four constants without re-rendering at true 1:1 scale (not just
re-reading the numbers) is how this regressed the first time.

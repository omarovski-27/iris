# Hands-free latch evidence

`iris-app`'s double-tap-to-latch feature (double-tap the hotkey to keep
recording with the key released, single tap to stop) needed a visible cue: a
latched pill and an ordinary listening pill must read differently at a
glance, or a forgotten latch is a silently-still-recording microphone — the
exact hazard the feature's own safety cap exists to bound.

The fix is additive to the pill's existing colour tokens — no new
`OverlayState`, no geometry, no motion, no wave-row or timer-font change.
`PillSink::set_latched` / `OverlayHandle::set_latched` /
`Command::Latched(bool)` set an orthogonal flag on `Model` (parallel to
`set_show_live_text`), and `render::core_colour`/`render::glow_colour` read
it: while latched, the core dot and the halo swap from `theme.rec` (mint) to
`theme.accent` (sky) — the same sky `Processing` already uses — instead of
the ordinary listening mint.

**2026-08-10 legibility pass:** the colour swap alone was reviewed at real
1:1 desktop size and found not reliably distinct at a glance — and colour
alone fails outright for colour-vision deficiency, the one failure mode this
indicator most needs to survive (a CVD viewer must still be able to tell a
live latch from an ordinary listen). Fixed by adding a second, non-colour
cue: a stroked ring drawn around the core dot only while latched
(`LATCH_RING_R_FRAC` and friends, `render::draw_glyph`). The ring's
*presence*, not its hue, is the signal — a hollow ring around a dot is a
shape difference that survives grayscale or any CVD simulation, unlike a
colour-only cue. It sits clear of both the core dot and the halo's pulsing
maximum so it reads as a crisp ring rather than a blur folded into either,
and shares its radius family with `Processing`'s spinner
(`SPINNER_R_FRAC`) by design. The colour swap stays — this is "and", not
"instead of". See `AGENTS.md` and the doc comments on `Theme::glow_latched`,
`LATCH_RING_R_FRAC`, and `render::core_colour` for the full reasoning.

## Files

Both palettes, held at `--hold-level 0.6` for a few seconds so the wave
row's rolling history and the one-pole level smoothing have settled (the
same method `../voice-level-evidence/` used), composited onto a 700×300
neutral desktop canvas at the renderer's real, unscaled device-pixel output
(`--scale 1.0`, 100% DPI) — nothing here is zoomed.

- `{prism,porcelain}-ordinary-1x1-desktop.png` — an ordinary `Listening`
  pill, mint core dot, no ring.
- `{prism,porcelain}-latched-1x1-desktop.png` — the same moment, latched:
  sky core dot, a visibly stronger halo, and the ring around the dot —
  obvious by eye at true desktop size in both themes.

## Regenerating

```bash
cargo run --example pill-demo -- --theme prism --utterance short \
    --hold-level 0.6 --filmstrip /tmp/iris-pill/prism-ordinary
cargo run --example pill-demo -- --theme prism --utterance short \
    --hold-level 0.6 --filmstrip /tmp/iris-pill/prism-latched --latched
# ...same for --theme porcelain

# Pick a settled listening frame from each (e.g. 0060-listening.png, ~2.4s
# in) and composite:
cargo run --example desktop_composite -- --bg dark --canvas 700x300 \
    out/ /tmp/iris-pill/prism-ordinary/0060-listening.png \
         /tmp/iris-pill/prism-latched/0060-listening.png
cargo run --example desktop_composite -- --bg light --canvas 700x300 \
    out/ /tmp/iris-pill/porcelain-ordinary/0060-listening.png \
         /tmp/iris-pill/porcelain-latched/0060-listening.png
```

`--latched` sends `Command::Latched(true)` right after `ShowListening`; it is
rejected alongside every other filmstrip-only flag when combined with
`--evidence`, which has its own fixed shot list.

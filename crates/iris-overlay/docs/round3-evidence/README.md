# Round 3 evidence

Review screenshots for the captain's third round of feedback on the overlay
(default capsule + waves + timer, live text off by default, no black scrim).
Composited over a synthetic busy backdrop for visibility; not test fixtures
and not referenced by any code. Safe to delete once reviewed.

Every file here is written by one command, which overwrites the whole set:

```bash
cargo run --example pill-demo -- --evidence crates/iris-overlay/docs/round3-evidence
```

That mode exists because a plain `--filmstrip` run cannot produce these
frames: it drives the demo's oscillating speech envelope (so no two frames are
comparable at a fixed volume) and writes the overlay's own transparent PNGs
(so there is no desktop behind the glass). `--evidence` holds the level, walks
each shot to a chosen phase, and composites over the backdrop. The same two
pieces are available on their own for one-off review passes —
`--filmstrip <dir> --hold-level 1.0 --backdrop` — see the crate README's
"Running the demo".

**What the backdrop is arranged to show.** Its two contrast bands — one near
black, one near white — are anchored on the shape's own rectangle, rebuilt
from `Layout` rather than from fractions of the frame, and the seam between
them is placed to fall through the middle of the four-character timer
readout. So each default frame shows the same digits over both contrast
directions at once, which is what these frames have to answer for: the timer
is drawn straight onto the glass with no scrim behind it, and its legibility
rests on `theme.timer_edge` outlining it in the opposite end of the theme's
luminance range (see the crate README, "Round 3"). An earlier version of this
set gated the bands on frame corners; neither band intersected the shape, so
every capsule sat on plain mid-gradient and the set demonstrated none of this.

- `*-quiet-sustained.png` / `*-loud-sustained.png` — the volume response,
  judged by eye: `Command::Level` held constant (0.05 vs 1.0) long enough for
  the level smoothing to settle, so these two are directly comparable rather
  than two arbitrary moments of an oscillating synthetic envelope. This is
  what a first pass of this evidence was missing — every "listening" frame in
  it happened to land near a quiet moment of the envelope, which cannot show
  whether the waves actually get bigger with volume.
- `*-listening-natural.png` — one frame from the demo's oscillating envelope,
  for what a real, varying utterance looks like rather than only the two
  extremes above.
- `*-processing-frozen.png` / `*-inserted-frozen.png` — the spinner and the
  checkmark, with the elapsed-recording timer frozen at the moment listening
  stopped rather than continuing to count. These are also where the timer's
  clearance from the centred glyph is visible: the readout is held off the
  spinner's outer edge by `render::timer_right_edge`, and it cannot grow into
  that gap because `state::format_timer` saturates at `9:59`.
- `prism-opt-in-livetext-open.png` — the ribbon still open with live text
  (`show_live_text = true`), for contrast: this is not the default any more,
  and its wave-row geometry and text scrim are both untouched from before
  this round. The timer is absent by design here — it and the live text share
  one right-aligned anchor, so `render::timer_alpha` is zero for every
  openness at which text is drawn.

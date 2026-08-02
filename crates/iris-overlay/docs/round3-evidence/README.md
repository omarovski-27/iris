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

# Round 3 evidence

Review screenshots for the captain's third round of feedback on the overlay
(default capsule + waves + timer, live text off by default, no black scrim).
Composited over a synthetic busy backdrop for visibility; not test fixtures
and not referenced by any code. Safe to delete once reviewed — regenerate
with `cargo run --example pill-demo -- --filmstrip <dir>` (see the crate
README's "Running the demo").

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
  stopped rather than continuing to count.
- `prism-opt-in-livetext-open.png` — the ribbon still open with live text
  (`show_live_text = true`), for contrast: this is not the default any more,
  and its wave-row geometry and text scrim are both untouched from before
  this round.

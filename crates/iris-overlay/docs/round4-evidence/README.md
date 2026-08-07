# Round 4 evidence

Review screenshots for the captain's fourth round of feedback on the overlay:
*"I like the design of the previous circle. It was very minimalistic. I want
to add to it the timeline. That's it. I don't want the dashes. I don't want
the huge font. I don't want huge size."* Composited over a synthetic busy
backdrop for visibility; not test fixtures and not referenced by any code.
Safe to delete once reviewed.

This set replaces `round3-evidence`, deleted in the same change: round 3's
wide capsule, wave row and large timer are exactly what this round undoes,
so a screenshot of that design is no longer a useful review artefact.

Every file here is written by one command, which overwrites the whole set:

```bash
cargo run --example pill-demo -- --evidence crates/iris-overlay/docs/round4-evidence
```

That mode exists because a plain `--filmstrip` run cannot produce these
frames: it drives the demo's oscillating speech envelope and writes the
overlay's own transparent PNGs (so there is no desktop behind the glass).
`--evidence` walks each shot to a chosen phase and composites over the
backdrop. The same backdrop compositing is available on its own for one-off
review passes — `--filmstrip <dir> --backdrop` — see the crate README's
"Running the demo".

**What changed from round 3, concretely.** No wave row anywhere in this
design any more — round 3's `quiet-sustained` / `loud-sustained` pair existed
specifically to show the wave row's volume response, and there is nothing
left for that pair to demonstrate, so it is gone from this shot list too.
The resting shape is close to the pre-round-3 true circle again
(`layout::REST_W`, 102 logical px against round 3's 128), widened only as
far as clearing the core glyph for the timer takes. The timer itself is drawn
at its own small `layout::TIMER_FONT` (10 px) rather than the live-text size
round 3 reused for it.

- `*-listening-natural.png` — the resting circle-with-timer, the shipped
  default most users ever see.
- `*-processing-frozen.png` / `*-inserted-frozen.png` — the spinner and the
  checkmark, with the elapsed-recording timer frozen at the moment listening
  stopped rather than continuing to count. These are also where the timer's
  clearance from the centred glyph is visible: the readout is held off the
  spinner's outer edge by `render::timer_right_edge`, and it cannot grow into
  that gap because `state::format_timer` saturates at `9:59`.
- `prism-opt-in-livetext-open.png` — the ribbon still open with live text
  (`show_live_text = true`), for contrast: still off by default, unchanged by
  this round. The timer is absent by design here — it and the live text share
  one right-aligned anchor, so `render::timer_alpha` is zero for every
  openness at which text is drawn.

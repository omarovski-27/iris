# Round 5 evidence

Review screenshots for the captain's round-5 decision on the overlay
(`/home/omar/firstmate/data/iris-overlay-back-to-circle/round5-direction.md`):
*"Keep it small and minimal like the circle you liked, but the marks become a
real audio waveform that moves with your voice, with a small timer beside
it... this treats the 'timeline' you asked for as the sound wave itself."*
Composited over a synthetic busy backdrop for visibility; not test fixtures
and not referenced by any code. Safe to delete once reviewed.

This set replaces `round4-evidence`, deleted in the same change: round 4 had
no wave row at all, which round 5 supersedes.

Every file here is written by one command, which overwrites the whole set:

```bash
cargo run --example pill-demo -- --evidence crates/iris-overlay/docs/round5-evidence
```

**Why this set is a sequence, not a single frame per state.** Round 3 and
round 4 both survived review on frozen stills, and the round-5 direction
calls that out by name as not enough this time: a single frame cannot show
whether a row of bars is a real waveform or a static pattern that merely
looks textured. `*-wave-sequence-*.png` captures the row at six points along
one continuous listening period — silence, a ramp-up, the loudest and
quietest moments of a speech-like oscillation (found by scanning for them,
not hand-picked — see `pill_demo.rs`'s `pick_checkpoint`, because a single
guessed timestamp can land on a quiet moment of an oscillating signal purely
by chance), and two points of a decay back to quiet. Viewed in order, the
row's shape visibly changes with what the "microphone" was doing, which is
the property round 3's design never had: every bar there read the *same*
current level, so a sustained loud stretch produced a flat-topped plateau,
never a peak. That contrast is `round3-dashes-vs-round5-waveform.png`,
composited from this build's `2b-speech-loud` frame against a real screenshot
of round 3's shipped, rejected row at a sustained loud level — left is the
"dashes" the captain rejected, right is what replaced it.

**A firstmate visual review of the first cut of this set caught three
problems the renderer's own author had missed** — the same failure mode that
got rounds 3 and 4 rejected: frames that looked fine to whoever rendered them
and wrong to the person judging them. Fixed before this set was regenerated:

1. **Silence still read as a row of identical dashes.** Near-constant quiet
   samples were producing near-identical bar heights — real data, but with
   nothing to visually distinguish it from round 3's static row. Fixed two
   ways: a quiet real sample now blends in the same decorrelated ripple
   texture an unfilled bar already used (`wave_bar_scale`'s `Some` branch),
   and — the change that actually mattered once rendered and looked at — a
   bar's *opacity*, not only its height, now fades with how quiet it is
   (`draw_wave`). Height alone stopped being a legible signal at the 1-3
   device px silence produces; a quiet bar collapsing toward near-transparent
   is what actually reads as "a thin line" rather than "a row of small solid
   dots," which is the alternative the review itself named as acceptable.
2. **The bars read as rounded dots, not a waveform.** Too few, too wide
   relative to their pitch, capped rounded enough to read as circles.
   `WAVE_TARGET_PITCH` and `WAVE_BAR_W_FRAC` both dropped so the same compact
   width holds more, narrower bars, `WAVE_BAR_CORNER_FRAC` replaced full
   rounding with a fraction that keeps a soft edge without erasing the bar's
   rectangular silhouette, and `WAVE_RESPONSE_EXPONENT` rose from round 1's
   `1.6` for a stronger tall-to-short ratio at speech amplitudes.
3. **A possible timer collision in a rendered ramp-up frame.** Checked
   directly rather than assumed either way: `render::tests::the_wave_row_
   never_reaches_the_timers_zone_at_full_amplitude` renders the same shape at
   a sustained loud level and a silent one and asserts the timer's own zone
   is byte-identical between the two — if a bar ever reached in, that zone
   would differ with amplitude and nothing else does. It's identical, so this
   was a proximity/rendering artifact of the small preview at round 3-era bar
   widths, not a real collision — and the bars are visibly narrower now
   regardless, per fix 2.

- `*-default-listening-natural.png` — the resting shape at one arbitrary
  moment of a naturally speech-like level, the shipped default most users
  ever see.
- `*-default-processing-frozen.png` / `*-default-inserted-frozen.png` — the
  spinner and the checkmark, with the wave row and the elapsed-recording
  timer both frozen at the moment listening stopped (see `README.md`'s
  "Round 5", "Freeze, not fabricate, once recording stops") rather than
  continuing to move or count.
- `*-wave-sequence-0-silence.png` — near-silence, sampled well after the
  rolling history has filled with quiet — now a faint, mostly-collapsed row
  rather than a row of visible marks.
- `*-wave-sequence-1-rampup.png` — a smooth ramp from quiet to loud; the
  ascending bar heights left to right are the row directly showing its own
  time axis.
- `*-wave-sequence-2a-speech-quiet.png` / `*-wave-sequence-2b-speech-loud.png`
  — the quietest and loudest windows found within one continuous
  speech-like oscillation, so "varying" is shown *varying* rather than
  asserted from one frame.
- `*-wave-sequence-3a-decay-partway.png` / `*-wave-sequence-3b-decay-settled.png`
  — a decay back toward quiet, caught partway through and near settled, so
  the descent itself is visible across two frames.
- `prism-opt-in-livetext-open.png` — the ribbon still open with live text
  (`show_live_text = true`), for contrast: still off by default, unchanged by
  this round. The timer is absent by design here — it and the live text
  share one right-aligned anchor, so `render::timer_alpha` is zero for every
  openness at which text is drawn.

# iris-overlay

The Iris pill: a small always-on-top shape that appears bottom-centre while
you hold the dictation hotkey. By default it is a quiet glass capsule, close
to a circle — the core glyph, a compact scrolling audio waveform, and a small
elapsed-recording timer — collapsing into a checkmark the instant text lands,
and taking itself off screen a moment later. A config opt-in (`show_live_text`,
off by default) widens the same shape further into a ribbon that shows the
live transcript as words arrive.

It is the product's hero surface. It never takes focus, never accepts a
click, and **never types**: text injection lives in `iris-core` and is not
reachable from here.

```
listening, default          listening, opt-in text open        inserted
 ╭────────────╮              ╭────────────────────────────╮
 │⬤ ıllıılı 0:07│  ──────▶    │  ...the report needs three  │  ──▶   ⓥ
 ╰────────────╯              ╰────────────────────────────╯
```

## Using it

```rust
use iris_overlay::{spawn, OverlayConfig};

let overlay = spawn(OverlayConfig {
    engine: "groq · whisper-large-v3-turbo · en".into(),
    ..Default::default()                 // Prism dark
})?;
let pill = overlay.handle();             // Clone + Send

pill.show_listening();                   // hotkey down
pill.update_level(0.62);                 // per audio frame
pill.set_partial_text("the quarterly");  // per partial transcript
pill.processing();                       // hotkey up
pill.inserted(142);                      // text landed; hides itself
```

`OverlayHandle` is the contract. Every method is non-blocking and infallible
from the caller's side: if the overlay thread is gone or its queue is
momentarily full the command is dropped, because a display that
back-pressures the audio pipeline is worse than one that skips a frame.
`hide()` cancels from any state; `set_theme()` swaps palettes at runtime.

Two transitions are worth knowing about:

- `inserted()` may be called straight from `listening` — a streaming engine that
  has already finalised does not have to fake a processing phase.
- `processing()` and `inserted()` from `hidden` are **ignored**. A late engine
  event cannot flash a pill back onto a screen the user already dismissed.

The full transition table is on `Model::apply`.

## The contract changed, and here is why

Two things used to be true of this crate, stated plainly in this file and in
`CLAUDE.md`: the API was "intended to stay stable", and the overlay "never
holds transcript text… nothing to read over the user's shoulder and nothing
in a crash dump." Both changed with this design.

**Why.** The previous design — a fixed 168×34 capsule with a 28-bar spectrum
waveform — was signed off, then rejected on sight the same day: *"I don't
like the UI, we need to change it, transform it completely, but I like the
motions and smoothness of it."* Three redesigns were rendered for real
(`data/iris-ui-directions/report.md` in the fleet's records is the design
history, kept as the record of *why*, not duplicated here); the captain chose
this one — an orb that opens into a live-text ribbon — specifically *because*
it shows the words. Shipping the shape without the text would have been a
different, weaker direction than the one that was actually chosen.

**What changed, concretely:**

- `OverlayHandle::set_partial_len(usize)` is gone. `OverlayHandle::set_partial_text(impl Into<String>)`
  replaces it: the overlay now holds the live transcript for exactly as long
  as it is on screen.
- `Model` gained a `text: String` field. Nothing else on the contract moved —
  `show_listening`, `update_level`, `set_engine`, `processing`, `inserted`,
  `hide`, `set_theme` are all unchanged, so this is additive-with-one-removal,
  not a rewrite.
- This authorises **displaying** text on screen. It does not authorise
  persisting, transmitting, or logging it anywhere — nothing in this crate
  writes the transcript to disk, and that stays true.
- **Ship it with an opt-out.** `iris-app` gates `set_partial_text` behind a
  config setting that, when off, leaves the resting presentation running with
  no ribbon and no text ever reaching this crate — a complete, coherent design
  on its own, not a degraded fallback. See `iris-app/src/config.rs` and
  `pill.rs`. Round 3 flipped that setting's default to off, so this is now the
  shipped presentation rather than the opt-out — see "Round 3" and "Round 4",
  below.

If you are extending this crate: the bar for adding to `OverlayHandle` is
still "the smallest honest change", the same as before. This one addition
earned its way in because the alternative was silently shipping a narrower
feature than the one that was actually approved.

## The shape

A capsule whose corner radius is always exactly half its height. There is one
shape, not two: only its width animates. Height, placement, and every motion
timing are unchanged from before.

- `layout::ORB_D` (34) is the shape's constant height, at every width.
- `layout::REST_W` (118) is the width at rest — no live text on screen, which
  is the default and what most users ever see. Round 4 walked this back from
  round 3's 128 to 102 (the wave row deleted outright); round 5 (below) grew
  it back out a little to make room for the wave row's return as a compact,
  genuine waveform — still clearly short of round 3's 128, not creeping back
  toward it.
- `layout::TIMER_FONT` (10) is the timer's own font size — its own small
  constant, deliberately not `TEXT_FONT` (15), the live-text ribbon's size.
  See "Round 4", below.
- `layout::RIBBON_MAX_W` (460) is the widest the shape grows, once live text
  is opted back in, before new words start scrolling the oldest ones off the
  left edge (`render::text::trailing_fit`) instead of growing further.
- The window is **fixed-size**, sized for the widest state up front
  (`layout::WINDOW_W` / `WINDOW_H`). Only the shape drawn inside it animates —
  see `window/win32.rs`'s `Surface::present`, which already hands a fresh size
  to `UpdateLayeredWindow` every frame regardless of whether it changed. This
  was the lower-risk of two options (the other being a window that resizes
  live), and the fixed transparent margin around the shape costs nothing extra
  to composite.
- `layout::WORK_AREA_GAP` (58) is unchanged from the previous pill on purpose:
  this direction changes the shape, not where the eye has to look for it.

## Design provenance

The captain's decision, recorded 2026-07-31: orb → live-text ribbon, "make it
exceptionally beautiful", live text on by default with a config opt-out (round
3 kept the feature and flipped that default to off — see "Round 3", below). It
supersedes the earlier captain-locked pill geometry (168×34 fixed capsule,
28-bar spectrum, listening-only telemetry chip) recorded in `CLAUDE.md`'s
history — that geometry is gone from this crate; the Prism/Porcelain palettes
and the motion budget are not.

**What carried over intact, and what did not.**

| | Then | Now |
|---|---|---|
| Geometry | Fixed 168×34 capsule | One shape, width animates 118→460 (34 at rest between the two orb rounds; 128 during round 3; 102 during round 4; see "Round 5") |
| Motion | `motion.rs` timings and curves | **Identical** — every constant is imported, none copied |
| Colour | Prism dark / Porcelain light | Same two palettes, same tokens, no new colours needed |
| Waveform | 28-bar spectrum (`spectrum.rs`) | A history-driven bar row (`draw_wave`) — round 1 built a single-level fan-out version, round 4 deleted it, round 5 rebuilt it reading a rolling history of real levels instead. See "Glass" and "Round 5", below. `spectrum.rs` itself has been gone since round 1; nothing in the current design shares code with it. |
| Shell | Opaque | Translucent glass at a constant `GLASS_FILL_ALPHA`; legibility is carried per-run — `theme.text_scrim` behind the live text, `theme.timer_edge` around the timer's digits — never a text-linked opacity ramp on the shell |
| Transcript | Never held (`set_partial_len`, a count) | Held while on screen (`set_partial_text`, the string) — only when live text is opted in, since round 3 |
| Engine chip | Rendered below the pill | Carried on the model, not rendered — no room without competing with the words |

Geometry and motion are **still single-sourced** and a `Theme` is still
colour and nothing else — swapping `PRISM_DARK` for `PORCELAIN_LIGHT` moves
zero pixels, the same guarantee the previous design made, verified the same
way: `cargo run --example pill-demo -- --theme porcelain --filmstrip <dir>`
and `--theme prism` are the identical code path.

One idea has no counterpart in the previous design and is worth naming
because it very nearly shipped as a bug: the width morph is smoothed with a
one-pole filter (the same attack/release character the microphone level meter
already used), and one-pole smoothing is asymptotic — it approaches its
target but never exactly reaches it. A ribbon that looked fully grown but was
a sub-pixel short of fitting its text used to silently drop a leading
character. `render::mod.rs`'s `Renderer::draw` snaps the smoothed width to its
exact target once within ~1.5 px specifically to close that gap; there is a
regression test for it (`width_smoothing_does_not_drop_a_whole_character_once_settled`)
and it must not be "simplified" away.

## Glass

A first pass of this shell shipped nearly opaque and dropped the 28-bar
waveform for a plain pulsing dot. Direct captain feedback after living with
it on a real desktop, across two rounds: round 1 asked for glass, the waves
back, and a more dramatic volume response; round 2 rejected round 1's glass
outright ("just one colour, normal, boring") and asked for the under-pill
engine caption gone entirely. All addressed as refinements within this same
direction, not a new one — full detail and the rendered evidence for both
rounds: the design report.

**Glass, final (round 2).** The shell's fill is `theme.spectrum` sampled as a
horizontal gradient — mint through sky through periwinkle — plus a narrow
bright streak that sweeps across on a cycle (`render::fill_glass_shell`).
This is the survivor of three structurally different treatments rendered and
compared side by side over a busy backdrop (report: `glass-options/`); it won
because it is the only one of the three where colour genuinely shifts across
the surface rather than sitting at one tint. What this is **not**:
acrylic/Mica-style backdrop blur. A layered window does not get a read of
what is behind it for free, and this crate does not fake one — no sampling,
no guessed blur. Translucency instead comes from real per-pixel alpha (the
same compositing `UpdateLayeredWindow` was already doing every frame), a soft
`glass_sheen` streak, and the existing rim (`outer_ring`/`border`) plus the
crisp `inner_highlight` line. The colour ramp cannot itself promise contrast
with the live text at every point along it — round 2 found this by test, not
by eye, once the old near-black `shell_top`/`shell_bottom` (removed; the fill
is not a vertical gradient any more) stopped accidentally providing it — so a
dedicated `text_scrim` token paints a soft band behind the text run only,
sized to the text and fading with it, in `draw_ribbon`. That is what actually
guarantees legibility now; the shell fill is free to be purely aesthetic.

**The wave — round 1 brought it back, round 4 removed it, round 5 rebuilt it
on a different foundation.** Round 1's `draw_wave` was a new,
independently-tuned bar row, not a port of the deleted `spectrum.rs`: a taper
with a floor instead of one that hit zero at both ends (the old row's failure
the captain named as "the waves... get cut off about 75%"), and an expansive
`powf(1.6)` response curve so quiet and loud read as clearly different. Round
3 gave it two sizes, centred on the shape at rest and in a band above the
live text once the ribbon opened — but every bar in that row read the *same*
current level, with position the only thing that varied it, which is what
round 4 named "the dashes" and deleted outright. Round 5 rebuilt the row from
that same response curve, but each bar now reads one sample from a rolling
history of recent levels instead of the one shared current value — see
"Round 5", below, for the full mechanism. The scrim and timer legibility
mechanisms the row constrains (`text_band`'s ceiling, `draw_timer`'s
clearance) are unchanged from round 3.

## Round 3: text off by default, a narrower capsule, and a timer

Direct captain feedback after living with the orb-to-ribbon design on a real
desktop, round 3 (2026-08-01): *"I'm pretty sure it's better, to remove the
transcription, because it's very slow. ... I like it more if it's not just a
dot or a circle, more like the pipe thing. ... We need to narrow it down. But
the glassy part is really good looking. The part that is ruining it is the
highlights behind the wording, because it's also black."* A follow-up
confirmed the direction and added one thing: *"Maybe just have some sort of
waves that get bigger whenever the voice is louder. The timer beside it, of
course."*

**Superseded by round 4, below, on the shape and the wave row specifically —
kept here because the timer legibility mechanism it built is still exactly
what ships.** The captain lived with this round's wide capsule and its waves
and asked for both gone; "the pipe thing, not a circle or a dot" is no longer
the instruction. What follows is what round 3 actually built and why; treat
the shape and wave claims as history, not the current state.

- **`show_live_text` defaults to `false`** (`iris-app::config::Config`) —
  unchanged by round 4, still the shipped default.
- **An elapsed-recording timer** (`render::draw_timer`), built entirely from
  machinery that already existed unrendered in `state.rs`
  (`listen_started_at`, `freeze_timer`, `Model::listening_ms`,
  `format_timer`) — no second timer was built. Cascadia Mono is monospaced
  (`the_face_is_monospaced` pins this, with the timer named in the test
  itself), so the digits never jitter as seconds tick over.

  **Legibility, without a dark backing plate.** A plate is the captain's exact
  complaint this round, and `theme.text_scrim` — the token that does carry a
  contrast promise — is gated on live text and stays that way, so the timer
  cannot borrow it. The first attempt drew the run a second time at a
  sub-pixel offset *in `theme.ink` itself* at low alpha; that thickens the
  strokes and adds no contrast whatsoever, because a same-colour halo cannot
  separate a glyph from a backing at the same luminance. Prism's near-white
  digits over a white desktop showing through the glass scored a contrast
  ratio of about **1.03** — the readout effectively disappeared in the
  presentation most users now see. What replaced it is an outline rather than
  a plate: the run is traced `TIMER_EDGE_OFFSET` out in eight directions in a
  new `theme.timer_edge` token before the crisp `theme.ink` pass. The colour
  is the mechanism — `timer_edge` is the opposite end of each theme's
  luminance range from its `ink` (Prism: a saturated steel blue `#1B4D7A`;
  Porcelain: a cool near-white `#F4F8FF`), so whatever the desktop is doing,
  one of the two reads: the fill when the backing is far from `ink`, the
  outline when it is near. There is no third case, which is why this needs no
  backdrop sampling — which a layered window does not get anyway.
  `theme::tests::the_timer_edge_reads_against_any_desktop_the_ink_cannot`
  holds both halves against the real composited shell, its sibling
  `the_ink_alone_does_disappear_somewhere_which_is_why_the_outline_exists`
  keeps that from passing on the ink's own contribution, and
  `render::tests::the_timer_is_traced_in_an_outline_colour_that_is_not_its_ink`
  checks the outline actually reaches the pixmap — a gap the same-colour halo
  it replaced was invisible to by construction.

  **An outline made of eight passes cannot fade by scaling its per-pass
  alpha.** Source-over compositing leaves `(1 - p)^8` of the backdrop, so
  eight passes at `p = 0.5 * a` reach 0.90 at `a = 0.5` and 0.73 at `a = 0.3`
  while the single crisp `theme.ink` pass beside them is at 0.50 and 0.30 —
  the digits took the outline's colour for the whole of every enter
  (`ENTER_MS`) and exit (`EXIT_MS`), on every dictation, and looked correct in
  every settled screenshot. `accumulating_pass_alpha` inverts the
  accumulation instead: it solves the per-pass alpha that puts the *stack's*
  total at `a` times its settled opacity, reproducing the authored
  `TIMER_EDGE_PASS_ALPHA` exactly at `a == 1.0`, so the settled contrast
  guarantee above is untouched. `timer_edge_pass_alpha` is the single curve
  `draw_timer` and the test read.

  So the check is shared rather than one-off:
  `render::tests::assert_fades_in_proportion` takes any multi-pass element's
  per-pass alpha, its single-pass companion's alpha, and the pass count, and
  requires the ratio between them to hold at every intermediate presence, not
  just at the ends. Use it for the next one.
  `the_fade_proportionality_guard_rejects_the_naive_accumulation` runs the
  naive scheme through it under `#[should_panic]`, so the guard is known to
  bite rather than merely to pass.

  **The timer does not get to borrow from the glyph beside it.** It first
  shipped fading on `glyph_alpha(open)` and anchored on the ribbon's right
  padding, and both are wrong for a run that is neither centred nor bounded
  by the shape alone:
  - *The anchor is shared with the live text.* `draw_timer` and `draw_ribbon`
    both draw right-aligned at `x + w - text_pad_x`, so a crossfade window
    where `glyph_alpha` and `text_alpha` are both non-zero — every `open`
    between `HANDOFF_LO` and `HANDOFF_HI`, i.e. ~30–50 ms of each open and
    each collapse — composited elapsed digits over the newest word. The
    centred glyph can afford that overlap; this cannot. `render::timer_alpha`
    is a steeper ramp that reaches zero exactly at `HANDOFF_LO`, so the two
    supports are disjoint by construction and the timer still fades rather
    than popping.
    `the_timer_and_the_live_text_never_share_the_right_aligned_row` holds both
    halves.
  - *Nothing made it miss the glyph.* An unbounded format put a
    five-character `10:00` inside the checkmark; `state::format_timer` now
    saturates at `9:59` — a layout guarantee, so the reserved width is the
    width the run can ever have — and `render::timer_right_edge` pushes the
    run in from the resting padding by however much clearing `glyph_half_w`
    (the widest mark `draw_glyph` paints, the listening halo included) by
    `GLYPH_TIMER_GAP` takes, never past `TIMER_EDGE_PAD_MIN` of the capsule's
    edge. This round bought that clearance from the timer's own right
    padding at a fixed `REST_W`; round 4 grows `REST_W` itself instead — see
    below.
- **`theme.text_scrim` was already correctly gated** — `render::mod.rs`'s
  `draw_ribbon` (the only place it paints) is called only when the ribbon is
  meaningfully open *and* live text is non-empty, so turning `show_live_text`
  off makes it unreachable by construction rather than needing a fix.
  `text_scrim_never_paints_in_the_default_no_text_presentation` pins this
  against real pixels so a future change cannot reopen the gap.
- **The glass itself was not touched.** `fill_glass_shell`, the spectrum ramp,
  the sheen streak, the rim — none of it was retuned, and still is not, as of
  round 4.

## Round 4: back to the circle, the wave row gone, a small timer

Direct captain feedback after living with round 3's capsule on a real desktop:
*"First impressions, it looks hideous. I don't like it at all... For the
design the timer is very big. I don't like that. It should be smaller. I told
you we need a minimalistic design. And I don't like the dashes that are next
to it... I like the design of the previous circle. It was very minimalistic. I
want to add to it the timeline. That's it. I don't want the dashes. I don't
want the huge font. I don't want huge size."*

This reverses round 3's own instruction ("not just a dot or a circle, more
like the pipe thing"), not by accident: the captain used the round-3 capsule
and rejected the result, which supersedes the earlier pick. "The previous
circle" is the round-1/round-2 orb-to-ribbon resting shape — the only shape in
this crate's history actually described as a circle (`layout::ORB_D`-wide,
before round 3 introduced `REST_W`); the original 168×34 pill and round 3's
128-wide capsule are both, in their own commit messages, explicitly *not*
that. "The timeline" has no separate referent anywhere in the rendered
directions or design report — the only elapsed-time element this crate has
ever had is the timer round 3 built, and "I don't want the huge font"
modifying "the timeline" in the same breath confirms it is a text readout,
not a distinct graphic. Read together: timer and timeline are the same
feature, named twice.

Two changes:

- **The wave row is gone outright**, not hidden or gated off. `draw_wave`,
  `wave_geometry`, `wave_row_bottom`, `wave_bar_scale`, `wave_alpha`, and every
  `WAVE_*` constant are deleted from `render/mod.rs` — there is no bar row
  anywhere in this design any more, at any `open`. `text_band`'s ceiling,
  which used to clamp against the row's bottom edge, is now simply the face's
  own line box: nothing else shares the shape's vertical space above the live
  text any more.
- **The timer moved to its own small font, and `REST_W` shrank to match.**
  `layout::TIMER_FONT` (10, logical px) replaces `TEXT_FONT` (15) as what
  `draw_timer` and the live width measurement in `Renderer::draw` size the
  run at — matching the original signed-off pill's telemetry-text size
  (`data/iris-ui-directions/report.md`, "Typography"), the last time this
  crate shipped a small secondary readout. `layout::REST_W` drops from round
  3's 128 to 102: as close to the pre-round-3 true circle as clearing the
  core glyph for the timer's own run requires, with no wave row left to make
  room for. The glyph stays fixed at the shape's horizontal centre at every
  width the shape takes (unchanged since round 1), so the clearance
  requirement is symmetric and the capsule cannot be narrower than twice it
  — `the_timer_keeps_real_air_between_itself_and_the_centred_glyph` pins the
  exact number; `layout::tests::the_rest_width_stays_compact_not_round_3s_128`
  and `render::tests::the_resting_shape_stays_compact_not_round_3s_capsule`
  both regression-guard against drifting back toward round 3's width (their
  names and bounds moved again in round 5, below, once the wave row needed
  room too, but the guard is the same idea).

Everything else about the timer — the outline-not-plate legibility mechanism,
the fade-proportionality guard, the anchor separation from live text, the
saturating four-character format — is exactly what round 3 built, described
above; only the font size and the geometry it drives changed.

## Round 5: the timeline answered — a real scrolling waveform, not dashes

The captain, on round 4's PR while it was still unmerged, was looking at
round 3's build (static dashes, the 15px timer) and said so again — read at
face value this looked like a regression, but the direction that followed
(`/home/omar/firstmate/data/iris-overlay-back-to-circle/round5-direction.md`)
settled both open questions round 4 had escalated rather than guessed at,
from three rendered options: *"Keep it small and minimal like the circle you
liked, but the marks become a real audio waveform that moves with your
voice, with a small timer beside it... this treats the 'timeline' you asked
for as the sound wave itself."*

**What this settles, concretely.**

1. **"Timeline" = the sound wave**, not a separate element and not (as round
   4's README speculated) simply a second name for the timer. Round 4's
   textual analysis is superseded, not vindicated in different words — stop
   re-deriving this from the design report; the captain has now defined it
   directly.
2. **The marks come back** — but the captain was explicit that *what* comes
   back has to differ from round 3: "the reason round 3's was rejected as
   dashes is that it did not read as sound. It must read as a waveform."
3. **Shape stays compact.** `layout::REST_W` grows from round 4's 102 to 118
   to give the row a real, minimal usable span — still clearly short of
   round 3's 128, not creeping back toward it (`the_rest_width_stays_compact_
   not_round_3s_128`, `the_resting_shape_stays_compact_not_round_3s_capsule`).
4. **Timer unchanged.** `layout::TIMER_FONT` stays 10px; the captain's "the
   font of the timer is pretty big" was about the round-3 build they had, not
   this branch.

**Why round 3's row read as dashes, mechanically.** Every bar answered the
same question — "what is the current mic level?" — with only its position in
the row (`sample_ramp`'s colour aside) making one bar differ from its
neighbour. A row where every element carries the same one piece of
information, laid out identically, is a static pattern by construction,
whatever curve shapes each bar's height. It cannot read as sound no matter
how the taper or the response curve is tuned, because sound is a sequence of
different moments and the row had no memory of any moment but the current
one.

**The fix: give the row memory.** `Renderer` (`render/mod.rs`) now carries
`wave_history: VecDeque<f32>`, a rolling buffer of recent `Model::level()`
samples, one pushed every `WAVE_SAMPLE_INTERVAL_MS` (70ms) while — and only
while — `model.state() == Listening`. `draw_wave` reads one *different*
historical sample per bar rather than the one current level for all of them:
the newest sample lands at the row's right edge (the same "newest is on the
right" convention the live-text ribbon and the timer already use), and each
bar to its left is an older moment. `wave_bar_scale` keeps round 1's
expansive `powf(1.6)` response curve — quiet and loud still read as clearly
different — but takes an `Option<f32>` now: `Some(level)` for a bar with a
real sample, `None` for a column the history has not reached yet. There is
no positional taper any more; whatever shape the row has is the shape the
last few seconds of audio actually had, not a shape assigned by where a bar
sits.

**Freeze, not fabricate, once recording stops.** The old row fed a small
constant "ambient" level into `wave_bar_scale` during `Processing` and
`Inserted` so it had something to fade out from. Round 5 drops that: no new
samples are pushed once `Listening` ends, so the row simply holds its last
real shape while `wave_alpha` (unchanged) fades it out — a timeline of real
audio or nothing, never a synthesised placeholder standing in for one.
`the_history_stops_growing_the_moment_listening_ends` pins it.

**A believable idle ripple, not a flat line.** A column the history has not
reached yet — the first moments after `ShowListening`, before 70ms×(bar
count) of real samples exist — reads as a slow, per-bar-decorrelated ripple
well under the real-signal floor (`wave_bar_scale`'s `None` arm), rather than
a hard zero or one repeated value. A flat resting row is exactly the
"dashes" failure by another name; `an_unfilled_bar_ripples_quietly_instead_
of_sitting_flat` and `the_idle_ripple_moves_over_time` hold both halves —
quiet enough to never masquerade as a real loud moment, and never frozen.

**Every appearance starts from silence.** `wave_history` is cleared the
instant the shape is fully hidden (`presence <= 0.001`), so a fresh
`ShowListening` never opens with the previous utterance's waveform still on
screen — `a_fresh_utterance_starts_the_wave_row_from_silence` pins it.

**Geometry, retuned for compactness, not rebuilt.** `WAVE_TARGET_PITCH` drops
12→8 logical px and `WAVE_MIN_BARS` 7→5 — the row's height constants
(`WAVE_MAX_H_REST`, `WAVE_MAX_H_RIBBON`, `WAVE_Y_OFFSET_RIBBON`) are
untouched, so `the_wave_row_clears_the_live_text_ink_box`'s guarantee against
the live-text ink box still holds on the same numbers. The compactness this
round asks for comes entirely from the width side, matched to a smaller
`REST_W` and a smaller minimum bar count than round 3 ever needed.

Evidence for all of this has to be a sequence, not a single frame — round 3
and round 4 both survived review on frozen stills, and the direction calls
that out by name as not enough this time. `crates/iris-overlay/docs/round5-
evidence/` has the wave row captured across silence, a ramp-up, two moments
of speech-like variation (found by scanning for the loudest and quietest
windows within that phase, not hand-picked, since a single guessed timestamp
can land on a quiet moment purely by chance — see `pill_demo.rs`'s
`pick_checkpoint`), and two moments of decay, both themes.

## Why a CPU raster path

The pill is small — even at its widest (the open ribbon) the whole frame is a
few hundred thousand device pixels at 200% scale. It is rasterised on the CPU
with [tiny-skia] and blitted with `UpdateLayeredWindow`.

A GPU surface was considered and rejected: at this size a D3D/D2D path would
add adapter enumeration, device-lost handling, a swapchain that has to
cooperate with `WS_EX_LAYERED`, and a second code path that cannot be tested
anywhere but a real Windows desktop. A WebView2 pill was never on the table —
a browser process for a small HUD shape is exactly the "heavy" this design
rules out, and it cannot be made click-through and non-activating without
fighting it.

What the CPU path buys, beyond simplicity:

- **The renderer is portable.** `render/` has no Windows in it, so the exact
  frames the overlay shows can be produced, diffed and eyeballed on Linux. The
  pixel assertions in this crate's tests are assertions about the real thing.
- **It cross-compiles from WSL with nothing but mingw.** tiny-skia and fontdue
  are pure Rust. No cmake, no Windows SDK, no shader compiler. See
  `docs/dev-windows.md` for why that matters to this repository.
- **Zero cost when hidden.** The loop parks on the command channel and the
  window is hidden; there is no swapchain to keep alive and no compositor
  callback to service.

Blur is the one thing a CPU path has to earn. The drop shadow and the coloured
state halo are three-pass box blurs of an 8-bit mask (`render/shadow.rs`).
Unlike the previous fixed-width pill, these masks **cannot be cached
forever** — the shape's width changes on nearly every frame while the ribbon
is opening, closing, or the transcript is growing, so the cache key in
`ensure_masks` includes the current width, rounded to the nearest 4 device
px, so a settled ribbon still hits the cache between frames and only the
brief morph window pays full cost. This is not a backdrop filter and does not
read the desktop behind the window — the report forbids that, and
`UpdateLayeredWindow` could not do it anyway.

### Text

Cascadia Mono — the font the design spec names — is SIL OFL 1.1, so it ships
in `assets/fonts/` and is rasterised with [fontdue] rather than resolved from
the system font stack. That keeps the Windows and Linux renders
byte-identical, which is what makes a PNG a usable review artefact.

`render::text::FontAtlas` has no clip-mask parameter, so the live transcript's
overflow handling is a string trim, not a pixel clip: when the transcript is
wider than the ribbon's padded interior, `trailing_fit` finds the longest
*tail* that fits and it is drawn right-aligned. The newest word always sits
against the right padding; the oldest ones quietly drop off the left. No new
text-rendering primitive was needed for this.

[tiny-skia]: https://github.com/RazrFalcon/tiny-skia
[fontdue]: https://github.com/mooman219/fontdue

## The window

`window/win32.rs`, and it is the only non-portable file.

| Requirement (design report checklist) | How |
|---|---|
| Per-pixel alpha | `WS_EX_LAYERED` + `UpdateLayeredWindow(..., ULW_ALPHA)` |
| Click-through | `WS_EX_TRANSPARENT`, plus `HTTRANSPARENT` from `WM_NCHITTEST` |
| Never activates | `WS_EX_NOACTIVATE`, `SW_SHOWNOACTIVATE`, `SWP_NOACTIVATE` |
| Out of Alt-Tab | `WS_EX_TOOLWINDOW` |
| Always on top | `WS_EX_TOPMOST` |
| Per-monitor V2 DPI | `SetThreadDpiAwarenessContext` on the overlay thread only |

This holds even though the overlay now shows text: nothing in this design
changes how the window handles input, because nothing in this design needed
to. The shape shows what was heard; it still never listens for a click.

Two details worth calling out:

**DPI awareness is set per-thread, not per-process.** This is a library. The
host process may have no manifest or a different awareness level, and changing
that on its behalf would be rude and would break its own windows. On
`WM_DPICHANGED` the layout is rebuilt at the monitor's real scale and the pill
re-rasterised — nothing is drawn at 96 dpi and stretched.

**The pill follows the foreground window's monitor.** On a multi-monitor desk
it appears under the app you are dictating into, not always on the primary.

## Running the demo

```bash
# The real pill. Windows only; from WSL, build and run the exe (see below).
cargo run --example pill-demo
cargo run --example pill-demo -- --theme porcelain --utterance short --cycles 0   # until Ctrl-C

# A PNG filmstrip of the same frames — the shipped default (circle, small
# timer; no wave row, no live text). Works anywhere, including Linux CI.
cargo run --example pill-demo -- --filmstrip /tmp/iris-pill
cargo run --example pill-demo -- --filmstrip /tmp/iris-pill --utterance long --scale 1.5

# The opt-in ribbon, as `iris-app`'s show_live_text = true gives it.
cargo run --example pill-demo -- --filmstrip /tmp/iris-ribbon --live-text on

# Same frames composited over a synthetic busy desktop — the only way to
# actually see the glass treatment rather than assert it.
cargo run --example pill-demo -- --filmstrip /tmp/iris-glass --backdrop

# Regenerate the committed review set in place (both themes, every phase).
cargo run --example pill-demo -- --evidence crates/iris-overlay/docs/round5-evidence
```

The demo drives a full cycle with a synthetic speech envelope — syllables
riding on a phrase-length swell — and a scripted utterance revealed one word
at a time (`--utterance short` fits comfortably; `--utterance long`, the
default, overflows the ribbon on purpose so the marquee-tail scroll is easy to
review, when `--live-text on`). `--live-text off`, the demo's own default,
sends no partial text at all — exactly what `show_live_text = false` does to
this crate in the shipped app — so the default circle-with-small-timer
presentation is reviewable the same way. `--hold-level` still exists for
holding the microphone level meter at a fixed value — `iris-app` still calls
`update_level` every audio frame, unrendered — but nothing in the current
design draws differently at any level, so it produces the same frame as not
passing it.

`--backdrop` composites onto a synthetic desktop, without which a glass shape
is reviewed against nothing. `--evidence` adds a fixed shot list on top, and
is what regenerates `docs/round5-evidence/` — see that directory's README.

### From WSL

`docs/dev-windows.md` has the full toolchain story; the overlay-specific loop is:

```bash
# Portable: state machine, layout, tokens, and the rasteriser itself.
cargo test -p iris-overlay

# See it, without a Windows desktop.
cargo run --example pill-demo -- --filmstrip /tmp/iris-pill

# Type-check and build the Windows window layer.
cargo check -p iris-overlay --target x86_64-pc-windows-gnu
cargo build --release --example pill-demo --target x86_64-pc-windows-gnu

# Run it as a real Windows process, straight from the WSL prompt.
./target/x86_64-pc-windows-gnu/release/examples/pill-demo.exe
```

That last line is the point: WSL interop launches the `.exe` as a genuine
Windows process, so the pill appears on the actual desktop with real DPI, a real
work area and real z-order. Run it by path — `cmd.exe` cannot use a
`\\wsl.localhost\...` working directory.

## Layout of the crate

| File | What |
|---|---|
| `theme.rs` | Colour tokens. Two `const` palettes, and nothing but colour. One token, `warn`, is painted by the app's settings window rather than the pill; its own doc comment says why it is the palettes' single warm colour. |
| `motion.rs` | Timing constants and the two cubic-bezier curves. |
| `layout.rs` | Logical geometry, DPI scaling, window placement. |
| `state.rs` | States, commands, and the animated model. No clock, no window, no shape. |
| `render/` | tiny-skia rasteriser, including the width-morph tween. Portable. |
| `window/win32.rs` | The layered window. The only `cfg(windows)` file. |
| `window/stub.rs` | The same loop with no window, everywhere else. |
| `handle.rs` | `spawn`, `Overlay`, `OverlayHandle`. |
| `headless.rs` | Drive and rasterise the pill with no window, anywhere. |

## Licence

The crate is MIT, like the rest of Iris. `assets/fonts/CascadiaMono-Regular.ttf`
is Copyright Microsoft Corporation under SIL OFL 1.1; the licence text ships
alongside it in `assets/fonts/OFL.txt`.

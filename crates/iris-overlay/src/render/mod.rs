//! The rasteriser.
//!
//! Portable on purpose. Nothing in here knows what a window is, so the exact
//! frames the Windows overlay puts on screen can be produced — and inspected —
//! on Linux. That is what [`crate::HeadlessOverlay`] and `--filmstrip` in the
//! demo are for.
//!
//! # The morph
//!
//! The shape's width is animation state, not layout: [`Renderer`] owns
//! `openness` (an open/close tween, built the same way [`Model::presence`]
//! is — a linear ramp through [`crate::motion::EASE_IN`] /
//! [`crate::motion::EASE_OUT`]) and `measured_w` (the ribbon's target width,
//! smoothed with the same attack/release character
//! [`crate::motion::LEVEL_ATTACK_MS`] / [`crate::motion::LEVEL_RELEASE_MS`]
//! already give the microphone level meter). Both live here rather than on
//! [`Model`] because measuring text width needs the [`FontAtlas`] this type
//! owns; `Model` stays shape-agnostic.
//!
//! One subtlety worth keeping intact: one-pole smoothing is asymptotic — it
//! approaches `measured_w`'s target but never exactly reaches it. The
//! overflow trim in [`text::draw_trailing_fit`]-style logic is a hard
//! boundary, so a ribbon that looked fully grown but was a sub-pixel short of
//! its target used to drop a whole character. `Renderer::draw` snaps
//! `measured_w` to its target once within a pixel or so specifically to
//! avoid that — see the snap in the width-smoothing step below.

mod shadow;
mod shapes;
mod text;

pub use text::{Align, FontAtlas, TextPaint};

use std::collections::VecDeque;

use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Mask, Paint, Path, Pixmap, Point, Shader,
    SpreadMode, Stroke, StrokeDash, Transform,
};

use crate::layout::Layout;
use crate::motion::{
    one_pole, CHECK_DRAW_MS, EASE_IN, EASE_OUT, ENTER_MS, LEVEL_ATTACK_MS, LEVEL_RELEASE_MS,
    REC_PULSE_MS, SCAN_PERIOD_MS, SPINNER_PERIOD_MS,
};
use crate::state::{format_timer, Model, OverlayState};
use crate::theme::{sample_ramp, Rgba, Theme};

/// Alpha the glass body's spectrum ramp is painted at, at every moment of the
/// shape's life. Constant on purpose: the surface never has to trade its
/// glassiness for contrast, because each of the two runs of text drawn on it
/// carries its own guarantee instead — `theme.text_scrim`, a band behind the
/// live text only (see [`draw_ribbon`]), and `theme.timer_edge`, an outline
/// around the timer's digits (see [`draw_timer`]), which is what the default
/// no-live-text presentation needs since the scrim is gated off there.
/// `theme::tests` composites this over the spectrum to check both guarantees
/// against the real on-screen colour.
pub(crate) const GLASS_FILL_ALPHA: f32 = 0.20;

/// Standard deviation of the shape's drop-shadow blur, in logical pixels.
const SHADOW_SIGMA: f32 = 12.0;
/// Vertical offset of the ambient shadow, in logical pixels.
const SHADOW_DY: f32 = 7.0;
/// Negative spread of the ambient shadow, in logical pixels — the pre-blur
/// shape is inset by this before blurring, so the shadow reads as a
/// concentrated halo rather than a soft copy of the shape at full size.
const SHADOW_SPREAD: f32 = 6.0;
/// Standard deviation of the coloured state-glow blur.
const GLOW_SIGMA: f32 = 14.0;
/// Negative spread of the coloured state halo.
const GLOW_SPREAD: f32 = 3.0;

/// Breathing room above and below the live text's ink box before the text
/// scrim's rounded edge starts, in logical pixels.
const SCRIM_PAD_Y: f32 = 3.0;

// ---------------------------------------------------------------------------
// the wave — round 5
// ---------------------------------------------------------------------------
//
// Round 3 built a bar row that read as "dashes": every bar reacted to the
// *same* current level, with a positional taper doing all the work of
// varying bar to bar — a synchronised, static-looking fan, not sound. Round 4
// deleted it outright rather than retune it, on the wording it had at the
// time ("I don't want the dashes"). Round 5
// (`/home/omar/firstmate/data/iris-overlay-back-to-circle/round5-direction.md`)
// answered both open questions from that round at once: "the timeline" the
// captain asked for *is* a sound wave, and what makes it read as one is each
// bar showing a genuinely different moment, not a different position.
//
// So each bar now reads one sample from a short rolling history of recent
// `Model::level()` (`Renderer::wave_history`, sampled at `draw`-loop cadence
// while `Listening`, frozen — no new samples — the instant recording stops).
// The newest sample lands at the row's right edge, the same "newest is on
// the right" convention the live-text ribbon and the timer already use.
// There is no positional taper: whatever shape the row has is the shape the
// last few seconds of audio actually had.
const WAVE_INSET: f32 = 6.0;
/// A firstmate visual review of the first cut of this round found the bars
/// reading as a row of dots rather than a waveform: too few, too wide
/// relative to their pitch, capped rounded enough to read as circles. Pitch
/// and bar-width-fraction both dropped so the same compact width holds more,
/// narrower bars — `porcelain-wave-sequence-1-rampup.png` from that first
/// cut ("tall narrow vertical bars, obviously sound") is the target this
/// retune generalises to every frame, not just the ramp.
const WAVE_TARGET_PITCH: f32 = 5.0;
const WAVE_MIN_BARS: usize = 8;
const WAVE_MAX_BARS: usize = 48;
/// Narrow enough that a bar stays visibly a bar — width clearly under its
/// own height — even at the shortest heights silence ever produces, not
/// only at speech amplitudes. A first retune (`0.4`) fixed the wide-open
/// ribbon and loud frames but still read as round dots at rest, where
/// height and width were close enough that `WAVE_BAR_CORNER_FRAC`'s
/// rounding made the two indistinguishable.
const WAVE_BAR_W_FRAC: f32 = 0.3;
/// Corner radius as a fraction of bar width. Less than the `0.5` a fully
/// rounded (pill/circle) cap would use — at the narrow widths
/// [`WAVE_BAR_W_FRAC`] now draws, full rounding is indistinguishable from a
/// dot; this keeps a soft edge without erasing the bar's rectangular
/// silhouette.
const WAVE_BAR_CORNER_FRAC: f32 = 0.3;
/// Bar height at full deflection, and how far the row's centre sits above the
/// shape's — for the two ends of the `open` tween. Unchanged in value from
/// round 3: the compactness round 5 asks for comes from the *width* side
/// (`WAVE_TARGET_PITCH`, `WAVE_MIN_BARS`), not from shrinking the row's
/// height, and `_RIBBON`'s numbers are still what
/// `the_wave_row_clears_the_live_text_ink_box` pins against the real font.
const WAVE_MAX_H_REST: f32 = 22.0;
const WAVE_Y_OFFSET_REST: f32 = 0.0;
const WAVE_MAX_H_RIBBON: f32 = 6.0;
const WAVE_Y_OFFSET_RIBBON: f32 = 12.5;
/// Bar height never reaches exactly zero, real sample or idle ripple alike —
/// a completely flat bar is closer to the "dashes" failure than a very quiet
/// one.
const WAVE_IDLE_FLOOR: f32 = 0.05;
/// Exponent [`wave_bar_scale`] raises a real level to. Raised from round 1's
/// `1.6` in the same firstmate review that narrowed the bars: a stronger
/// tall-to-short ratio at speech amplitudes is what makes height, not just
/// bar count, read as sound.
const WAVE_RESPONSE_EXPONENT: f32 = 2.4;
/// How much of the idle ripple (see [`wave_ripple`]) blends into a real
/// sample that is itself near the floor, so silence reads as a live quiet
/// waveform rather than N identical stubs. Small relative to a genuinely
/// loud bar's own range (`1.0 - WAVE_IDLE_FLOOR`), so it is felt as texture
/// on a quiet bar and vanishes under a loud one, never the other way round.
/// Raised from an initial `0.07` once a rendered silence frame still read as
/// too uniform even with texture present — firstmate visual review.
const WAVE_TEXTURE_AMPLITUDE: f32 = 0.11;
/// Gap between the wave row's right edge and the timer's left edge, so the
/// two read as sharing the capsule rather than colliding. See [`draw_timer`].
const WAVE_TIMER_GAP: f32 = 6.0;
/// How often a fresh sample is rolled into the wave row's history, in ms.
/// Short enough that the row reads as continuous motion rather than visibly
/// stepping; long enough that neighbouring bars are genuinely different
/// moments rather than the same instant redrawn.
const WAVE_SAMPLE_INTERVAL_MS: u64 = 70;
/// How many samples the rolling history keeps. Sized to the widest row this
/// shape ever draws (`WAVE_MAX_BARS`) so nothing is buffered that could
/// never be shown.
const WAVE_HISTORY_LEN: usize = WAVE_MAX_BARS;

/// The wave row's `(max_h, y_offset)`, in logical px, at a given `open`
/// (0 = closed/rest, 1 = fully-open ribbon). Linear in `open`, the same shape
/// of interpolation [`draw`](Renderer::draw) already uses for the shape's
/// width, so the row's size and position track the ribbon's own morph rather
/// than snapping between the two states.
fn wave_geometry(open: f32) -> (f32, f32) {
    let open = open.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32| a + (b - a) * open;
    (
        lerp(WAVE_MAX_H_REST, WAVE_MAX_H_RIBBON),
        lerp(WAVE_Y_OFFSET_REST, WAVE_Y_OFFSET_RIBBON),
    )
}

/// The bottom edge of the wave row's full-deflection envelope, in device
/// pixels, for a shape whose top is `y` and height `h`, at a given `open`.
///
/// [`text_band`] is the only caller: this is how the scrim's ceiling reads the
/// row's real extent instead of a size the bars do not actually have yet.
/// [`wave_geometry`] — not this function — is what both sides share.
fn wave_row_bottom(l: &Layout, y: f32, h: f32, open: f32) -> f32 {
    let (max_h, y_offset) = wave_geometry(open);
    y + h * 0.5 - (y_offset - max_h * 0.5) * l.scale
}

/// A slow, per-bar-decorrelated ripple in `0.0..=1.0` — `i` only decorrelates
/// neighbouring bars, `now_ms` drives the animation. Shared by both the
/// idle branch of [`wave_bar_scale`] and the texture it blends into a real
/// sample near the noise floor, so "no sample yet" and "a real but very
/// quiet sample" read as the same kind of live quiet, not two different
/// mechanisms that happen to look similar.
fn wave_ripple(i: f32, now_ms: u64) -> f32 {
    let t = now_ms as f32;
    0.5 + 0.5 * ((t / 340.0 + i * 0.7).sin() * (t / 810.0 + i * 0.29).sin()).abs()
}

/// scaleY for one wave bar, given the historical level sample it represents
/// — `None` for a column the rolling history has not reached yet, meaning
/// the row is still filling in from silence since the hotkey went down. `i`
/// decorrelates the idle ripple (and the quiet-real-sample texture, below)
/// between neighbouring bars.
///
/// A firstmate visual review of the first cut of this caught silence
/// rendering as a row of *identical* marks — every bar reading the same
/// near-zero level with literally nothing to vary it, which is
/// indistinguishable from round 3's rejected "dashes" regardless of the
/// mechanism behind it. A real sample that is itself near
/// [`WAVE_IDLE_FLOOR`] now gets the same ripple texture blended in, scaled
/// by how much headroom is left below a genuinely loud response — so it
/// fades to nothing once the signal is unambiguously loud, and a loud bar is
/// still read purely from data, never dressed up as texture.
fn wave_bar_scale(sample: Option<f32>, i: f32, now_ms: u64) -> f32 {
    match sample {
        Some(level) => {
            // Expansive, not linear: widens the gap between quiet and loud
            // instead of compressing it (captain, round 1: "so it's showing
            // that it's clearly hearing you"), and pushed further still in
            // round 5 (firstmate review: bar-to-bar contrast read too weak
            // to look like sound rather than a row of similar dots).
            let response = level.clamp(0.0, 1.0).powf(WAVE_RESPONSE_EXPONENT);
            let base = WAVE_IDLE_FLOOR + response * (1.0 - WAVE_IDLE_FLOOR);
            let texture_room = (1.0 - response).max(0.0);
            base + WAVE_TEXTURE_AMPLITUDE * texture_room * wave_ripple(i, now_ms)
        }
        None => {
            // A believable resting state, not a flat line of identical
            // stubs — that flatness is exactly what read as "dashes": a
            // slow, per-bar-decorrelated ripple well under the real-signal
            // floor, so it never competes with an actual sample once one
            // arrives.
            WAVE_IDLE_FLOOR * (0.5 + 0.5 * wave_ripple(i, now_ms))
        }
    }
}

/// How visible the whole bar row is, given the three state weights.
///
/// The row belongs to listening and processing; `inserted` drives it down
/// twice over so the confirmation reads as a single check and nothing else.
/// That double suppression is only correct while `inserted` is *rising* —
/// which is why [`Ctx::state_alpha`] freezes the cross-fade on the way out
/// rather than letting it run back down to zero.
fn wave_alpha(listening: f32, processing: f32, inserted: f32) -> f32 {
    (1.0 - inserted).max(listening + processing).min(1.0) * (1.0 - inserted * 0.9)
}

/// The live waveform: a row of bars whose count and pitch are recomputed
/// from the shape's current width every frame — big and centred at rest,
/// where it is the primary content of the shape; smaller and offset once
/// live text opens the ribbon and needs the row below it. See
/// [`wave_geometry`] for the two sizes and why the row cannot simply grow.
///
/// `right_reserve` is the device-pixel zone [`draw_timer`] needs at the right
/// edge, in the default (no live text) presentation — the wave row's usable
/// width shrinks to leave it room rather than the two overlapping. It is
/// measured from where the run actually lands (see [`timer_right_edge`], which
/// may push the timer in past the resting padding to clear the centred glyph)
/// and scales by [`timer_alpha`], so it shrinks to zero in lockstep with the
/// timer's own fade as live text opens the ribbon and the row reclaims the
/// full width exactly as the timer vacates it.
///
/// `history` is [`Renderer::wave_history`]: the newest sample lands at index
/// `history.len() - 1` and is drawn at the row's right edge (index
/// `count - 1`); a bar older than the history's own length gets no sample at
/// all — see [`wave_bar_scale`] for how that column reads instead.
#[allow(clippy::too_many_arguments)]
fn draw_wave(
    pixmap: &mut Pixmap,
    ctx: &Ctx<'_>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    open: f32,
    right_reserve: f32,
    history: &VecDeque<f32>,
    clip: Option<&Mask>,
) {
    let l = ctx.layout;
    let theme = ctx.theme;

    let listening = ctx.state_alpha(OverlayState::Listening);
    let processing = ctx.state_alpha(OverlayState::Processing);
    let inserted = ctx.state_alpha(OverlayState::Inserted);
    let alpha = wave_alpha(listening, processing, inserted);
    if alpha <= 0.001 {
        return;
    }

    let inset = WAVE_INSET * l.scale;
    let usable = (w - 2.0 * inset - right_reserve.max(0.0)).max(0.0);
    if usable <= 0.0 {
        return;
    }
    let pitch_target = WAVE_TARGET_PITCH * l.scale;
    let count = ((usable / pitch_target).round() as usize).clamp(WAVE_MIN_BARS, WAVE_MAX_BARS);
    let pitch = usable / count as f32;
    let bar_w = (pitch * WAVE_BAR_W_FRAC).max(l.scale * 0.75);

    let (row_max_h, row_y_offset) = wave_geometry(open);
    let cy = y + h * 0.5 - row_y_offset * l.scale;
    let max_h = row_max_h * l.scale;
    let now_ms = ctx.model.now_ms();
    let hist_len = history.len();

    for i in 0..count {
        let p = if count > 1 {
            i as f32 / (count - 1) as f32
        } else {
            0.5
        };
        // 0 = the newest sample, which belongs at the row's right edge.
        let age = count - 1 - i;
        let sample = (age < hist_len).then(|| history[hist_len - 1 - age]);
        let scale = wave_bar_scale(sample, i as f32, now_ms);
        let bh = (max_h * scale).max(l.scale * 0.6);
        let bx = x + inset + pitch * i as f32 + (pitch - bar_w) * 0.5;
        let by = cy - bh * 0.5;
        if let Some(path) = shapes::round_rect(bx, by, bar_w, bh, bar_w * WAVE_BAR_CORNER_FRAC) {
            let colour = sample_ramp(theme.spectrum, p);
            // `scale` fades a bar's own opacity, not only its height. Height
            // alone stops being a legible signal at the couple of device px
            // silence produces — a firstmate visual review found a rendered
            // near-silent frame still read as a row of same-looking marks
            // even with real per-bar height variation present, just too
            // subtle to see at that size. A quiet bar fading toward
            // near-transparent alongside its short height is what actually
            // reads as "collapsing toward a thin line" rather than "a row of
            // small solid dots" — and a loud bar, at `scale` near `1.0`,
            // loses nothing.
            fill_clipped(pixmap, ctx, &path, ctx.c(colour.fade(alpha * scale)), clip);
        }
    }
}

/// Cached, transform- and width-dependent masks. Rebuilt whenever the pill
/// moves *or the shape's width changes*, which — unlike the fixed-width pill
/// this replaces — is common while the ribbon is opening, closing, or the
/// transcript is growing. The width component of the key is rounded to the
/// nearest 4 device px rather than compared exactly, so a settled ribbon
/// still hits the cache between frames and only the brief morph window pays
/// full cost.
struct Masks {
    key: (u32, i32, i32, i32),
    clip: Mask,
    ambient: Mask,
    glow: Mask,
}

impl std::fmt::Debug for Masks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Masks").field("key", &self.key).finish()
    }
}

/// A linear ramp through an easing curve, evaluated exactly like
/// [`Model::presence`] is — same shape of code, a different target and
/// duration. Drives the rest-capsule-to-ribbon width morph.
#[derive(Clone, Copy, Debug)]
struct Tween {
    linear: f32,
    open: bool,
}

impl Tween {
    const fn new() -> Self {
        Self {
            linear: 0.0,
            open: false,
        }
    }

    fn tick(&mut self, target_open: bool, dt: f32) {
        self.open = target_open;
        let duration = if target_open {
            ENTER_MS as f32
        } else {
            CHECK_DRAW_MS as f32
        };
        let step = if duration > 0.0 { dt / duration } else { 1.0 };
        self.linear = if target_open {
            (self.linear + step).min(1.0)
        } else {
            (self.linear - step).max(0.0)
        };
    }

    fn eval(&self) -> f32 {
        if self.open {
            EASE_IN.eval(self.linear)
        } else {
            1.0 - EASE_OUT.eval(1.0 - self.linear)
        }
    }
}

/// The open value below which the closed-state glyph (core / spinner /
/// check) is fully visible, and the ribbon text is fully invisible.
const HANDOFF_LO: f32 = 0.28;
/// The open value above which the ribbon text is fully visible.
const HANDOFF_HI: f32 = 0.55;

/// How visible the closed-state glyph is. Zero while the ribbon is
/// meaningfully open, one once it has mostly closed, straight-line crossfade
/// between — chosen so the glyph never has to share pixels with the
/// still-wide text. An earlier, linear version of this faded the checkmark in
/// from the very start of the collapse and it was visibly drawn *underneath*
/// text that hadn't finished closing; this handoff window is the fix, and it
/// must not regress back to a plain linear fade.
fn glyph_alpha(open: f32) -> f32 {
    ((HANDOFF_HI - open) / (HANDOFF_HI - HANDOFF_LO)).clamp(0.0, 1.0)
}

/// The complement of [`glyph_alpha`]. `glyph_alpha(open) + text_alpha(open)
/// == 1` for every `open`, so the handoff never has a gap or a double
/// exposure.
fn text_alpha(open: f32) -> f32 {
    ((open - HANDOFF_LO) / (HANDOFF_HI - HANDOFF_LO)).clamp(0.0, 1.0)
}

/// How visible the elapsed-recording timer is.
///
/// Deliberately *not* [`glyph_alpha`], which is what it used to borrow. The
/// centred glyph can afford a crossfade that overlaps [`text_alpha`]'s
/// because it sits in the middle of the shape; the timer is drawn at the
/// ribbon's own right-aligned anchor — the exact pixels the newest live word
/// occupies — so every `open` leaving both non-zero composites digits over
/// that word. Being a *steeper* ramp that reaches zero at [`HANDOFF_LO`], the
/// open value below which `text_alpha` is zero, is the whole mechanism: the
/// two supports are disjoint by construction, and the timer still fades
/// rather than popping.
/// `the_timer_and_the_live_text_never_share_the_right_aligned_row` walks the
/// tween and holds both halves of that.
fn timer_alpha(open: f32) -> f32 {
    ((HANDOFF_LO - open) / HANDOFF_LO).clamp(0.0, 1.0)
}

// The centred glyph's own measurements, as fractions of the shape's height.
// [`draw_glyph`] paints with them and [`glyph_half_w`] reserves clearance
// from them, so the zone the timer keeps out of cannot drift away from the
// ink it exists to keep clear of.
const CORE_R_FRAC: f32 = 0.15;
const CORE_PULSE_MAX: f32 = 1.12;
const HALO_R_FRAC: f32 = 0.16;
const HALO_GROW_FRAC: f32 = 0.22;
const SPINNER_R_FRAC: f32 = 0.32;
const SPINNER_STROKE_FRAC: f32 = 0.045;
const CHECK_SIZE_FRAC: f32 = 0.6;
const CHECK_STROKE_FRAC: f32 = 0.09;

/// Clear air between the centred glyph's ink and the timer's leading digit,
/// in logical pixels.
const GLYPH_TIMER_GAP: f32 = 5.0;
/// Least air the timer leaves between its last digit and the capsule's right
/// edge, in logical pixels — the bound on how far [`timer_right_edge`] may
/// push the run inward from its resting anchor to buy that clearance.
const TIMER_EDGE_PAD_MIN: f32 = 9.0;

/// How far the timer's outline passes are offset from the run, in logical
/// pixels, and the alpha each single pass is drawn at *at full presence*.
///
/// Sub-pixel on purpose: this is an outline traced around the digits in
/// `theme.timer_edge`, not a plate behind them (see [`draw_timer`]). Eight
/// passes around the compass at this alpha accumulate to a near-solid hairline
/// where they overlap at the glyph edge and to nothing a couple of pixels out,
/// so the run gains a rim without gaining a footprint — which is what keeps it
/// clear of the [`GLYPH_TIMER_GAP`] and [`TIMER_EDGE_PAD_MIN`] budgets
/// [`timer_right_edge`] measured against the crisp run alone.
///
/// "At full presence" is load-bearing: below it the per-pass alpha is not this
/// constant scaled, it is solved by [`accumulating_pass_alpha`]. See there.
const TIMER_EDGE_OFFSET: f32 = 0.9;
const TIMER_EDGE_PASS_ALPHA: f32 = 0.5;

/// The eight unit directions [`draw_timer`] traces its outline in, as a table
/// rather than a loop body, so the pass count the alpha solve needs is
/// `TIMER_EDGE_DIRS.len()` and cannot drift from the passes actually drawn.
const TIMER_EDGE_DIRS: [(f32, f32); 8] = {
    const Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
    [
        (-1.0, 0.0),
        (1.0, 0.0),
        (0.0, -1.0),
        (0.0, 1.0),
        (-Q, -Q),
        (Q, -Q),
        (-Q, Q),
        (Q, Q),
    ]
};

/// What `passes` overlapping passes, each composited at `pass_alpha`, add up
/// to where all of them land on the same pixel.
///
/// Source-over compositing multiplies what each pass leaves uncovered, so the
/// stack keeps `(1 - pass_alpha)^passes` of the backdrop. This is the closed
/// form of that, and it is the reason a multi-pass element cannot simply scale
/// its per-pass alpha to fade.
fn accumulated_alpha(pass_alpha: f32, passes: usize) -> f32 {
    1.0 - (1.0 - pass_alpha.clamp(0.0, 1.0)).powi(passes as i32)
}

/// The alpha one pass of a `passes`-deep accumulating stack has to composite at
/// for the whole stack to land at `a` times the opacity it reaches at full
/// presence — where `settled_pass_alpha` is the per-pass alpha that defines
/// that settled look.
///
/// The naive answer, `settled_pass_alpha * a`, is wrong everywhere except the
/// two endpoints, and wrong in the direction nobody notices from a settled
/// screenshot: [`accumulated_alpha`] is superlinear in the per-pass alpha, so
/// eight passes at `0.5 * a` reach 0.90 at `a = 0.5` and 0.73 at `a = 0.3`
/// against a single companion pass at 0.50 and 0.30. An outline meant to trace
/// ink therefore *becomes* the digits for the whole of every fade. Inverting
/// the accumulation instead makes the stack's total alpha exactly proportional
/// to `a`, so it fades in step with anything drawn in one pass beside it, and
/// reproduces `settled_pass_alpha` exactly at `a == 1.0`.
///
/// `tests::assert_fades_in_proportion` is the mechanical check for that
/// property, and applies to any future multi-pass element in this file.
fn accumulating_pass_alpha(settled_pass_alpha: f32, passes: usize, a: f32) -> f32 {
    let settled = accumulated_alpha(settled_pass_alpha, passes);
    let target = settled * a.clamp(0.0, 1.0);
    1.0 - (1.0 - target).powf(1.0 / passes as f32)
}

/// The alpha one of [`draw_timer`]'s outline passes composites at, for a run
/// drawn at presence `a`.
///
/// A function rather than an expression inline in [`draw_timer`] so the
/// renderer and `tests::the_timer_outline_fades_in_proportion_with_the_ink_it_traces`
/// read the same curve: retuning the rim cannot then leave a test agreeing
/// with a curve the renderer no longer draws.
fn timer_edge_pass_alpha(a: f32) -> f32 {
    accumulating_pass_alpha(TIMER_EDGE_PASS_ALPHA, TIMER_EDGE_DIRS.len(), a)
}

/// How far the centred glyph's ink can reach either side of the shape's
/// centre, in device pixels: the widest of everything [`draw_glyph`] paints
/// there — the listening halo at its outermost, the pulsing core at the top of
/// its pulse, the processing spinner's stroked outer edge, and the inserted
/// check.
///
/// The timer shares that row, so this is what it has to clear. Measuring the
/// halo at its *fully grown* radius rather than only the solid marks is
/// deliberate: the ring is faint by then, but a readout clearing the spinner
/// alone would still have it sweep through the digits once a pulse. The check
/// is measured from its own path bounds because its ink fills only the middle
/// of the box it is authored in — half the box would over-reserve by 5 px.
fn glyph_half_w(l: &Layout) -> f32 {
    let half_stroke = |frac: f32| (l.shape_h * frac).max(l.scale) * 0.5;
    let halo = l.shape_h * (HALO_R_FRAC + HALO_GROW_FRAC);
    let core = l.shape_h * CORE_R_FRAC * CORE_PULSE_MAX;
    let spinner = l.shape_h * SPINNER_R_FRAC + half_stroke(SPINNER_STROKE_FRAC);
    let check = shapes::check_mark(0.0, 0.0, l.shape_h * CHECK_SIZE_FRAC)
        .map_or(0.0, |(path, _)| path.bounds().right())
        + half_stroke(CHECK_STROKE_FRAC);
    halo.max(core).max(spinner).max(check)
}

/// The device-pixel x the timer's run is right-aligned on, for a shape that
/// starts at `x` and is `w` wide and a run that measures `timer_w`.
///
/// The resting answer is the ribbon's own right padding, so the timer sits in
/// the same padded interior live text would. It is pushed further in only as
/// far as clearing [`glyph_half_w`] by [`GLYPH_TIMER_GAP`] takes, and never
/// closer to the capsule's edge than [`TIMER_EDGE_PAD_MIN`].
///
/// This exists because the shape is as close to the pre-round-3 circle as a
/// four-character timer allows ([`crate::layout::REST_W`]) and the glyph is
/// centred on a fixed anchor inside it: at the resting padding alone a
/// four-character readout's leading digit landed *on* the spinner's outer
/// edge. Reserving against the glyph rather than widening the capsule
/// further is the trade this shape asks for — and the clamp is the honest
/// half of it, so a future wider run degrades to less clearance instead of
/// digits hanging off the glass.
fn timer_right_edge(l: &Layout, x: f32, w: f32, timer_w: f32) -> f32 {
    let clear_of_glyph = l.center_x + glyph_half_w(l) + GLYPH_TIMER_GAP * l.scale + timer_w;
    (x + w - l.text_pad_x)
        .max(clear_of_glyph)
        .min(x + w - TIMER_EDGE_PAD_MIN * l.scale)
}

/// Draws one pill frame into an RGBA pixmap.
#[derive(Debug)]
pub struct Renderer {
    layout: Layout,
    pixmap: Pixmap,
    atlas: FontAtlas,
    masks: Option<Masks>,

    openness: Tween,
    measured_w: f32,
    prev_now: u64,
    started: bool,

    /// Recent [`Model::level`] samples, oldest first, that [`draw_wave`]
    /// reads to draw the row as a genuine scrolling waveform rather than one
    /// current level fanned across every bar. Cleared whenever the shape is
    /// fully hidden, so every appearance starts from silence rather than
    /// picking up a stale utterance's shape. See the wave section's module
    /// doc, above.
    wave_history: VecDeque<f32>,
    /// `model.now_ms()` the last sample was rolled into `wave_history`, so a
    /// fresh one is taken only every [`WAVE_SAMPLE_INTERVAL_MS`], not every
    /// frame.
    wave_last_sample_ms: u64,
}

impl Renderer {
    /// A renderer for one monitor scale factor (`dpi / 96.0`).
    ///
    /// # Panics
    ///
    /// Panics if the pixmap for the resulting window size cannot be allocated,
    /// which at the shape's size means the process is already out of memory.
    #[must_use]
    pub fn new(scale: f32) -> Self {
        let layout = Layout::new(scale);
        let pixmap = Pixmap::new(layout.window_w, layout.window_h)
            .expect("overlay pixmap allocation failed");
        let measured_w = layout.rest_w;
        Self {
            layout,
            pixmap,
            atlas: FontAtlas::new(),
            masks: None,
            openness: Tween::new(),
            measured_w,
            prev_now: 0,
            started: false,
            wave_history: VecDeque::with_capacity(WAVE_HISTORY_LEN),
            wave_last_sample_ms: 0,
        }
    }

    /// Re-lay-out and re-allocate for a new scale factor. A no-op if the scale
    /// is unchanged, so it is safe to call on every `WM_DPICHANGED`. The
    /// in-flight morph width is rescaled proportionally rather than reset, so
    /// a DPI change mid-animation does not visibly snap the shape.
    pub fn set_scale(&mut self, scale: f32) {
        let layout = Layout::new(scale);
        let ratio = if self.layout.scale > 0.0 {
            layout.scale / self.layout.scale
        } else {
            1.0
        };
        self.measured_w *= ratio;
        if layout.window_w == self.layout.window_w && layout.window_h == self.layout.window_h {
            self.layout = layout;
            return;
        }
        if let Some(pixmap) = Pixmap::new(layout.window_w, layout.window_h) {
            self.pixmap = pixmap;
            self.layout = layout;
            self.masks = None;
        }
    }

    /// The geometry this renderer is drawing at.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The most recently drawn frame.
    #[must_use]
    pub fn pixmap(&self) -> &Pixmap {
        &self.pixmap
    }

    /// The wave row's rolling history, oldest first — test-only, so a
    /// regression can assert directly on the samples a frame actually
    /// accumulated instead of reasoning about it through pixels alone.
    #[cfg(test)]
    fn wave_history_snapshot(&self) -> Vec<f32> {
        self.wave_history.iter().copied().collect()
    }

    /// Draw `model` and return the frame. Premultiplied RGBA, ready for
    /// `UpdateLayeredWindow` after a channel swap.
    pub fn draw(&mut self, model: &Model) -> &Pixmap {
        let dt = if self.started {
            (model.now_ms().saturating_sub(self.prev_now) as f32).min(100.0)
        } else {
            self.started = true;
            0.0
        };
        self.prev_now = model.now_ms();

        let has_text = !model.text().is_empty();
        let target_open = has_text
            && matches!(
                model.state(),
                OverlayState::Listening | OverlayState::Processing
            );
        self.openness.tick(target_open, dt);

        let target_w = if has_text {
            // Capped at the widest run the ribbon can ever show: past that the
            // `min` below discards the answer anyway, and the transcript being
            // measured grows for as long as the user keeps talking.
            let budget = (self.layout.ribbon_max_w - 2.0 * self.layout.text_pad_x).max(0.0);
            let text_w =
                self.atlas
                    .measure_capped(model.text(), self.layout.text_font, 0.0, budget);
            // Floored at the rest width, not the bare shape height: the
            // ribbon never reads narrower than the capsule most users see at
            // rest, even for a single short word.
            (text_w + 2.0 * self.layout.text_pad_x)
                .max(self.layout.rest_w)
                .min(self.layout.ribbon_max_w)
        } else {
            self.layout.rest_w
        };
        let tau = if target_w > self.measured_w {
            LEVEL_ATTACK_MS
        } else {
            LEVEL_RELEASE_MS
        };
        self.measured_w += (target_w - self.measured_w) * one_pole(tau, dt);
        // One-pole smoothing never exactly reaches its target; snap once
        // close so the overflow trim below doesn't drop a whole character
        // over a sub-pixel gap. See the module doc.
        if (target_w - self.measured_w).abs() < 1.5 {
            self.measured_w = target_w;
        }

        // Disjoint field borrows: the drawing steps below are free functions so
        // the pixmap, the atlas and the mask cache can be borrowed at once.
        let Renderer {
            layout,
            pixmap,
            atlas,
            masks,
            openness,
            measured_w,
            wave_history,
            wave_last_sample_ms,
            ..
        } = self;

        pixmap.fill(Color::TRANSPARENT);
        let presence = model.presence();
        if presence <= 0.001 {
            // Every appearance starts from silence, not a stale utterance's
            // waveform left over from the last one.
            wave_history.clear();
            return &self.pixmap;
        }

        // Roll a fresh sample into the wave's history only while the mic is
        // actually live: `Processing` and `Inserted` freeze the row at
        // whatever shape it last had, rather than manufacturing an ambient
        // level for it to hold — the timeline it draws is real audio or
        // nothing, never a synthesised placeholder.
        if model.state() == OverlayState::Listening
            && model.now_ms().saturating_sub(*wave_last_sample_ms) >= WAVE_SAMPLE_INTERVAL_MS
        {
            *wave_last_sample_ms = model.now_ms();
            if wave_history.len() >= WAVE_HISTORY_LEN {
                wave_history.pop_front();
            }
            wave_history.push_back(model.level());
        }

        let theme = *model.theme();
        let open = openness.eval();
        // The shape's base width is the rest capsule, not a bare circle —
        // `open` only ever widens it further, toward the live-text ribbon.
        let w = layout.rest_w + (*measured_w - layout.rest_w).max(0.0) * open;
        let h = layout.shape_h;
        let r = h * 0.5;
        let x = layout.center_x - w * 0.5;
        let y = layout.center_y - h * 0.5;

        let (dy, content_scale) = model.enter_transform(layout.scale);
        let ox = layout.center_x;
        let oy = layout.center_y + h * 0.5;
        let xf = Transform::from_translate(ox, oy + dy)
            .pre_scale(content_scale, content_scale)
            .pre_translate(-ox, -oy);

        let Some(shape) = shapes::round_rect(x, y, w, h, r) else {
            return &self.pixmap;
        };

        let ctx = Ctx {
            layout,
            theme: &theme,
            xf,
            alpha: presence,
            model,
        };

        ensure_masks(masks, layout, xf, x, y, w, h, r);
        let cached = masks.as_ref();

        if let Some(m) = cached {
            fill_through(pixmap, &m.ambient, theme.ambient_shadow.fade(presence));
            let glow = glow_colour(&theme, model);
            fill_through(pixmap, &m.glow, glow.fade(presence));
        }

        // The timer and the live text are drawn on one shared right-aligned
        // anchor, so they must never both be visible: `timer_alpha` is zero at
        // every `open` where `text_alpha` is non-zero, which is what enforces
        // that — not the wider `glyph_alpha` window the centred glyph can
        // afford. `timer_zone` is measured from where the run actually lands
        // (`timer_right_edge` may push it in past the resting padding to clear
        // the glyph), so `draw_wave`'s `right_reserve` tracks the timer rather
        // than a nominal position.
        let glyph_a = glyph_alpha(open);
        let timer_a = timer_alpha(open);
        let timer_text = format_timer(model.listening_ms());
        let timer_w = atlas.measure(&timer_text, layout.timer_font, 0.0);
        let timer_right = timer_right_edge(layout, x, w, timer_w);
        let timer_zone =
            ((x + w - timer_right) + timer_w + WAVE_TIMER_GAP * layout.scale) * timer_a;

        draw_shell(pixmap, &ctx, &shape, x, y, w, h, r, cached.map(|m| &m.clip));
        draw_wave(
            pixmap,
            &ctx,
            x,
            y,
            w,
            h,
            open,
            timer_zone,
            wave_history,
            cached.map(|m| &m.clip),
        );
        draw_glyph(pixmap, &ctx, glyph_a);
        draw_timer(
            pixmap,
            &ctx,
            atlas,
            timer_right,
            y + h * 0.5,
            timer_a,
            &timer_text,
        );
        if open > 0.02 && has_text {
            draw_ribbon(
                pixmap,
                &ctx,
                atlas,
                x,
                y,
                w,
                h,
                open,
                cached.map(|m| &m.clip),
            );
        }

        &self.pixmap
    }
}

/// Everything the drawing steps need, gathered so they stay short.
struct Ctx<'a> {
    layout: &'a Layout,
    theme: &'a Theme,
    xf: Transform,
    alpha: f32,
    model: &'a Model,
}

impl Ctx<'_> {
    /// A colour faded by the pill's overall presence.
    fn c(&self, colour: Rgba) -> Rgba {
        colour.fade(self.alpha)
    }

    /// How present `state` is, cross-fading with whatever we came from.
    ///
    /// Reads [`shown_state`] / [`shown_cross`], so the cross-fade is frozen
    /// while the shape is *leaving*. Letting it run there means every term
    /// written as `1.0 - inserted` (the core dot) *inverts* over the 90 ms
    /// after the inserted hold expires — the checkmark dissolved into a
    /// re-emerging mint dot while the shape faded out. Holding the last
    /// visible state at full weight leaves presence as the single thing that
    /// animates an exit, which is what it was always meant to be. Enter, and
    /// every visible-to-visible transition, are untouched.
    fn state_alpha(&self, state: OverlayState) -> f32 {
        fade_between(
            shown_state(self.model) == state,
            self.model.previous_state() == state,
            shown_cross(self.model),
        )
    }

    /// Map a layout-space point through the enter transform.
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        let mut pts = [Point::from_xy(x, y)];
        self.xf.map_points(&mut pts);
        (pts[0].x, pts[0].y)
    }
}

/// The state whose *look* the frame is drawing.
///
/// [`OverlayState::Hidden`] has no look of its own: leaving is a fade of
/// whatever was last on screen, carried entirely by [`Model::presence`]. So
/// while the shape is on its way out this is the state it is leaving *from*,
/// and every appearance decision in this module reads it instead of
/// [`Model::state`] — one place, so an exit can never half-switch, with the
/// core dot holding while the glow, the core's colour or the processing
/// shimmer jump to their `Hidden` answers on the first exit frame.
///
/// What deliberately does *not* read it: anything measuring elapsed time in
/// the current state (the check's draw-on progress), and the ribbon's
/// open/closed target, which is geometry — the ribbon still collapses as the
/// shape leaves.
fn shown_state(model: &Model) -> OverlayState {
    if model.state().is_visible() {
        model.state()
    } else {
        model.previous_state()
    }
}

/// Progress of the cross-fade into [`shown_state`], frozen at 1.0 while the
/// shape is leaving — there is nothing to cross-fade into.
fn shown_cross(model: &Model) -> f32 {
    if model.state().is_visible() {
        model.cross()
    } else {
        1.0
    }
}

/// Cross-fade weight for something that is on in the current state, the
/// previous one, both, or neither.
fn fade_between(now: bool, before: bool, cross: f32) -> f32 {
    match (now, before) {
        (true, true) => 1.0,
        (true, false) => cross,
        (false, true) => 1.0 - cross,
        (false, false) => 0.0,
    }
}

// ---------------------------------------------------------------------------
// masks
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn ensure_masks(
    slot: &mut Option<Masks>,
    layout: &Layout,
    xf: Transform,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
) {
    let width_bucket = (w / 4.0).round() as i32;
    let key = (
        layout.scale.to_bits(),
        (xf.ty * 4.0).round() as i32,
        (xf.sx * 512.0).round() as i32,
        width_bucket,
    );
    if slot.as_ref().is_some_and(|m| m.key == key) {
        return;
    }

    let (ww, wh) = (layout.window_w, layout.window_h);
    let build = |inset: f32, offset_y: f32, blur_sigma: Option<f32>| -> Option<Mask> {
        let path = shapes::round_rect_inset(x, y, w, h, r, inset)?;
        let mut mask = Mask::new(ww, wh)?;
        mask.fill_path(
            &path,
            FillRule::Winding,
            true,
            xf.post_translate(0.0, offset_y),
        );
        if let Some(sigma) = blur_sigma {
            shadow::blur(&mut mask, sigma);
        }
        Some(mask)
    };

    let s = layout.scale;
    let (Some(clip), Some(ambient), Some(glow)) = (
        build(0.0, 0.0, None),
        build(SHADOW_SPREAD * s, SHADOW_DY * s, Some(SHADOW_SIGMA * s)),
        build(GLOW_SPREAD * s, 0.0, Some(GLOW_SIGMA * s)),
    ) else {
        *slot = None;
        return;
    };

    *slot = Some(Masks {
        key,
        clip,
        ambient,
        glow,
    });
}

/// Paint a flat colour everywhere `mask` allows.
fn fill_through(pixmap: &mut Pixmap, mask: &Mask, colour: Rgba) {
    if colour.a <= 0.001 {
        return;
    }
    let Some(rect) =
        tiny_skia::Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32)
    else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(colour.to_color());
    paint.anti_alias = false;
    pixmap.fill_rect(rect, &paint, Transform::identity(), Some(mask));
}

/// The state halo's colour, cross-fading between the state being left and the
/// one being shown.
fn glow_colour(theme: &Theme, model: &Model) -> Rgba {
    let pick = |state: OverlayState| match state {
        OverlayState::Listening => theme.glow_listening,
        OverlayState::Inserted => theme.glow_inserted,
        _ => theme.glow_idle,
    };
    pick(model.previous_state()).lerp(pick(shown_state(model)), shown_cross(model))
}

/// The core dot's colour: sky while the engine is working, mint otherwise.
fn core_colour(theme: &Theme, model: &Model) -> Rgba {
    if shown_state(model) == OverlayState::Processing {
        theme.accent
    } else {
        theme.rec
    }
}

// ---------------------------------------------------------------------------
// drawing steps
// ---------------------------------------------------------------------------

/// Fills `shape` with the glass body: a horizontal ramp across the full
/// `theme.spectrum` (literal refraction — light splitting into colour,
/// which is also this crate's own visual language) plus a narrow bright
/// streak that sweeps across the width on a steady cycle, light glinting
/// off an edge rather than a fixed decal.
///
/// This is the survivor of three structurally different treatments rendered
/// for the captain's second visual pass, after the first glass attempt was
/// rejected outright ("just one colour, normal, boring" — real translucency
/// was there, but the surface still read as a flat grey-black slab). The
/// other two — a soft mint wash with a drifting radial highlight, and a
/// bolder single-hue tint with a pulsing specular dot — are in the design
/// report's rendered comparison, not in this codebase; once a decision was
/// made there was no reason to ship the other two as dead code.
#[allow(clippy::too_many_arguments)]
fn fill_glass_shell(
    pixmap: &mut Pixmap,
    ctx: &Ctx<'_>,
    shape: &Path,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    now: f32,
    clip: Option<&Mask>,
) {
    let theme = ctx.theme;
    let base_a = GLASS_FILL_ALPHA;
    let last = (theme.spectrum.len() - 1).max(1) as f32;
    let stops: Vec<GradientStop> = theme
        .spectrum
        .iter()
        .enumerate()
        .map(|(i, c)| GradientStop::new(i as f32 / last, ctx.c(c.fade(base_a)).to_color()))
        .collect();
    let shader = LinearGradient::new(
        Point::from_xy(x, y + h * 0.5),
        Point::from_xy(x + w, y + h * 0.5),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(ctx.c(theme.spectrum[0].fade(base_a)).to_color()));
    let paint = Paint {
        shader,
        anti_alias: true,
        ..Paint::default()
    };
    pixmap.fill_path(shape, &paint, FillRule::Winding, ctx.xf, None);

    // A narrow bright streak sweeping across the width on a steady cycle —
    // light glinting off an edge, not a static highlight.
    let period = 3200.0;
    let phase = (now % period) / period;
    let band = 0.16;
    let lo = (phase - band).clamp(0.0, 1.0);
    let mid = phase.clamp(0.0, 1.0);
    let hi = (phase + band).clamp(0.0, 1.0);
    let streak_stops = vec![
        GradientStop::new(0.0, ctx.c(theme.glass_sheen.fade(0.0)).to_color()),
        GradientStop::new(lo, ctx.c(theme.glass_sheen.fade(0.0)).to_color()),
        GradientStop::new(mid, ctx.c(theme.glass_sheen.fade(2.0)).to_color()),
        GradientStop::new(hi, ctx.c(theme.glass_sheen.fade(0.0)).to_color()),
        GradientStop::new(1.0, ctx.c(theme.glass_sheen.fade(0.0)).to_color()),
    ];
    let streak = LinearGradient::new(
        Point::from_xy(x, y),
        Point::from_xy(x + w, y + h),
        streak_stops,
        SpreadMode::Pad,
        Transform::identity(),
    );
    if let Some(shader) = streak {
        let paint = Paint {
            shader,
            anti_alias: true,
            ..Paint::default()
        };
        pixmap.fill_path(shape, &paint, FillRule::Winding, ctx.xf, clip);
    }
}

/// The shell: translucent glass body, a soft top sheen, two rings, and the
/// lit top edge.
///
/// **On the glass.** The overlay is already a per-pixel-alpha layered window,
/// so `fill_glass_shell`'s body carries alpha straight from `theme.spectrum`
/// and the real desktop shows through underneath it — this is ordinary alpha
/// compositing, the same mechanism `UpdateLayeredWindow` already does every
/// frame, not a new capability. What this deliberately does *not* do is
/// sample or blur whatever is behind the window (acrylic / Mica-style
/// backdrop blur): a layered window does not get that behind-pixel read for
/// free, and faking it by, say, blurring a guess at the desktop would be
/// worse than not attempting it. The glass *impression* instead comes from
/// honest ingredients: translucency (the fill alpha itself), colour that
/// shifts across the surface instead of sitting at one flat tint (the
/// horizontal spectrum ramp), a moving specular streak (`glass_sheen`), and
/// the existing rim (`outer_ring`/`border`) plus the crisp `inner_highlight`
/// line.
///
/// **On legibility.** Live text sits directly on this surface once the ribbon
/// opens, and it has to read over an arbitrary desktop, light or dark. The
/// shell does *not* answer that: its fill stays at [`GLASS_FILL_ALPHA`], its
/// most transparent, whether the shape is at its resting width or wide open
/// with text. Pulling the whole surface back toward opaque whenever there is
/// text would trade the glass away at exactly the moment it is most visible, so
/// contrast is solved locally instead — `theme.text_scrim`, a soft band
/// painted behind the run only in [`draw_ribbon`], is the sole mechanism, and
/// `theme::tests` holds it to a measured ratio against the composited fill.
#[allow(clippy::too_many_arguments)]
fn draw_shell(
    pixmap: &mut Pixmap,
    ctx: &Ctx<'_>,
    shape: &Path,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    clip: Option<&Mask>,
) {
    let s = ctx.layout.scale;
    let now = ctx.model.now_ms() as f32;

    fill_glass_shell(pixmap, ctx, shape, x, y, w, h, now, clip);

    // `0 0 0 1px` — a hairline ring just outside the body.
    stroke(
        pixmap,
        ctx,
        shapes::round_rect_inset(x, y, w, h, r, -0.5 * s),
        ctx.c(ctx.theme.outer_ring),
        s,
    );
    // The 1 px border proper.
    stroke(
        pixmap,
        ctx,
        shapes::round_rect_inset(x, y, w, h, r, 0.5 * s),
        ctx.c(ctx.theme.border),
        s,
    );

    // Lit top edge along the flat part between the caps — absent exactly at
    // the circle, present at any wider width.
    let hl_w = w - r * 2.0;
    if hl_w > 0.0 {
        if let Some(path) =
            shapes::round_rect(x + r, y + 0.5 * s, hl_w, (1.0 * s).max(1.0), 0.5 * s)
        {
            fill(pixmap, ctx, &path, ctx.c(ctx.theme.inner_highlight));
        }
    }
}

/// The closed-state glyph at the shape's centre: pulsing core while
/// listening, two-arc spinner while processing with nothing captured yet,
/// drawn-on check once inserted. `alpha` is [`glyph_alpha`], so it fades out
/// as the ribbon opens and back in as it closes.
fn draw_glyph(pixmap: &mut Pixmap, ctx: &Ctx<'_>, alpha: f32) {
    if alpha <= 0.001 {
        return;
    }
    let l = ctx.layout;
    let theme = ctx.theme;
    let model = ctx.model;
    let (cx, cy) = (l.center_x, l.center_y);

    let listening = ctx.state_alpha(OverlayState::Listening);
    let processing = ctx.state_alpha(OverlayState::Processing);
    let inserted = ctx.state_alpha(OverlayState::Inserted);

    if listening > 0.001 {
        let t = (model.now_ms() % u64::from(REC_PULSE_MS)) as f32 / REC_PULSE_MS as f32;
        let grow = (t / 0.7).min(1.0);
        let halo_r = l.shape_h * HALO_R_FRAC + l.shape_h * HALO_GROW_FRAC * grow;
        let halo_a = 0.4 * (1.0 - grow) * listening * alpha;
        if let Some(p) = shapes::circle(cx, cy, halo_r) {
            fill(pixmap, ctx, &p, ctx.c(theme.rec.fade(halo_a)));
        }
    }

    let core_colour = core_colour(theme, model);
    let mut core_r = l.shape_h * CORE_R_FRAC;
    if listening > 0.0 {
        let t = (model.now_ms() % u64::from(REC_PULSE_MS)) as f32 / REC_PULSE_MS as f32;
        let swell = CORE_PULSE_MAX - 1.0;
        let pulse = if t < 0.7 {
            1.0 + swell * (t / 0.7)
        } else {
            CORE_PULSE_MAX - swell * ((t - 0.7) / 0.3)
        };
        core_r *= 1.0 + (pulse - 1.0) * listening;
    }
    let core_alpha = (1.0 - inserted) * alpha;
    if core_alpha > 0.001 {
        if let Some(p) = shapes::circle(cx, cy, core_r) {
            fill(pixmap, ctx, &p, ctx.c(core_colour.fade(core_alpha)));
        }
    }

    if processing > 0.001 {
        let turn = (model.now_ms() % u64::from(SPINNER_PERIOD_MS)) as f32
            / SPINNER_PERIOD_MS as f32
            * 360.0;
        let radius = l.shape_h * SPINNER_R_FRAC;
        for (offset, colour) in [(-45.0, theme.spinner.0), (45.0, theme.spinner.1)] {
            stroke(
                pixmap,
                ctx,
                shapes::arc(cx, cy, radius, turn + offset, 90.0),
                ctx.c(colour.fade(processing * alpha)),
                (l.shape_h * SPINNER_STROKE_FRAC).max(l.scale),
            );
        }
    }

    if inserted > 0.001 {
        if let Some((path, length)) = shapes::check_mark(cx, cy, l.shape_h * CHECK_SIZE_FRAC) {
            // `age_ms` restarts at every transition, so once the pill has left
            // `Inserted` it no longer measures how long the check has been
            // drawing itself — reading it during the exit made a finished
            // check erase and start over as the shape faded. It is finished by
            // construction there: INSERTED_HOLD_MS outlasts CHECK_DRAW_MS.
            let progress = if model.state() == OverlayState::Inserted {
                (model.age_ms() as f32 / CHECK_DRAW_MS as f32).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let mut paint = Paint::default();
            paint.set_color(ctx.c(theme.ok.fade(inserted * alpha)).to_color());
            paint.anti_alias = true;
            let stroke_style = Stroke {
                width: (l.shape_h * CHECK_STROKE_FRAC).max(l.scale),
                line_cap: tiny_skia::LineCap::Round,
                line_join: tiny_skia::LineJoin::Round,
                dash: StrokeDash::new(vec![length, length], length * (1.0 - progress)),
                ..Stroke::default()
            };
            pixmap.stroke_path(&path, &paint, &stroke_style, ctx.xf, None);
        }
    }
}

/// The elapsed-recording timer: `m:ss`, right-aligned in the same row the
/// live-text ribbon uses, sharing the capsule with the core glyph rather
/// than sitting under it — this design's second round rejected an under-pill
/// caption outright, and the fix here is not to resurrect that placement in
/// a new form. Drawn at [`crate::layout::TIMER_FONT`], its own small size —
/// round 4 (captain, live-desktop review of round 3's shipped capsule):
/// "the timer is very big... I don't want the huge font", the concrete
/// shape of round 3 having reused the live-text size for this run. `alpha`
/// is [`timer_alpha`], *not* the [`glyph_alpha`] the centred glyph uses:
/// this run and the ribbon's are drawn on one anchor, so only a fade that is
/// zero wherever [`text_alpha`] is non-zero keeps digits off the newest live
/// word. `right` is [`timer_right_edge`], which holds the run clear of the
/// centred glyph inside a capsule too narrow for the resting padding to do
/// it alone.
///
/// Cascadia Mono is monospaced (`the_face_is_monospaced` pins this), so every
/// digit and the colon share one advance width and the run never reshuffles
/// itself as the seconds tick. It cannot grow a character either:
/// [`format_timer`] saturates, so the width reserved against the glyph is the
/// width the run can ever have.
///
/// Legibility is solved without a dark backing plate — the captain's
/// complaint this round is specifically that something black behind text
/// ruins the glass, and `theme.text_scrim` (the token that does carry a
/// contrast promise) is gated on live text and stays that way. Instead the
/// run is traced: drawn [`TIMER_EDGE_OFFSET`] out in eight directions in
/// `theme.timer_edge` before the crisp full-alpha pass in `theme.ink`.
///
/// The colour is the whole mechanism, and it is why the first attempt at this
/// did not work: that one re-drew the run in `ink` itself, which thickens the
/// strokes but cannot separate them from a backing at `ink`'s own luminance —
/// Prism's near-white digits over a white desktop showing through the glass
/// scored a contrast ratio of about 1.03. `timer_edge` is the opposite end of
/// each theme's luminance range from its `ink`, so one of the two always
/// reads: the fill when the desktop is far from it, the outline when it is
/// near. No backdrop sampling is involved — a layered window does not get one
/// (see [`draw_shell`]) and this deliberately does not need one.
/// `theme::tests::the_timer_edge_reads_against_any_desktop_the_ink_cannot`
/// holds that against the real composited shell in both directions.
///
/// The outline is a stack of eight passes and the fill is one, so the two
/// only fade together if the stack's *accumulated* alpha is what tracks `a`;
/// scaling the per-pass alpha instead made the digits take the outline's
/// colour for the whole of every enter and exit. [`accumulating_pass_alpha`]
/// carries that solve and the reasoning, and
/// `tests::the_timer_outline_fades_in_proportion_with_the_ink_it_traces` pins
/// it mid-fade, where the defect lived.
fn draw_timer(
    pixmap: &mut Pixmap,
    ctx: &Ctx<'_>,
    atlas: &mut FontAtlas,
    right: f32,
    center_y: f32,
    alpha: f32,
    text: &str,
) {
    if alpha <= 0.001 {
        return;
    }
    let l = ctx.layout;
    let theme = ctx.theme;
    let a = ctx.alpha * alpha;

    let (tx, ty) = ctx.map(right, center_y);

    let edge_a = timer_edge_pass_alpha(a);
    if edge_a > 0.001 {
        let d = TIMER_EDGE_OFFSET * l.scale;
        for (ux, uy) in TIMER_EDGE_DIRS {
            atlas.draw(
                pixmap,
                text,
                l.timer_font,
                0.0,
                tx + ux * d,
                ty + uy * d,
                Align::Right,
                TextPaint::Solid(theme.timer_edge),
                edge_a,
            );
        }
    }
    atlas.draw(
        pixmap,
        text,
        l.timer_font,
        0.0,
        tx,
        ty,
        Align::Right,
        TextPaint::Solid(theme.ink),
        a,
    );
}

/// Top and bottom of the live text's scrim band, in device pixels.
///
/// Takes no text on purpose. The band and the run it backs are both placed
/// from the face's fixed line metrics (see [`FontAtlas::baseline_offset`]),
/// not from the ink of whichever suffix happens to be shown this frame, so
/// neither moves as words scroll off the left — the band would otherwise
/// breathe by a pixel or three every time a descender entered or left the
/// window, and the run with it. The top is held at or below the wave row's
/// bottom edge, which `the_wave_row_clears_the_live_text_ink_box` pins
/// against the real glyphs — round 5 restored this clamp along with the row
/// itself; see the wave section's module doc, above, and `wave_row_bottom`.
///
/// It takes `open` for exactly one reason: the row it has to stay clear of is
/// itself a function of `open` (see [`wave_geometry`]), and only reaches the
/// small `_RIBBON` size at `open == 1.0`. The scrim is already at full alpha
/// by [`HANDOFF_HI`], well before that, so pinning this ceiling to the
/// ribbon-end numbers painted the band straight over the still-large bars for
/// the whole handoff window. Both sides read the one `open` value the frame
/// already has; there is no second curve here.
fn text_band(atlas: &FontAtlas, l: &Layout, y: f32, h: f32, open: f32) -> (f32, f32) {
    let (ascent, descent) = atlas.line_extents(l.text_font);
    let baseline = y + h * 0.5 + atlas.baseline_offset(l.text_font);
    let pad_y = SCRIM_PAD_Y * l.scale;
    let top = (baseline - ascent - pad_y).max(wave_row_bottom(l, y, h, open));
    let bottom = (baseline - descent + pad_y).min(y + h);
    (top, bottom)
}

/// Live text and, while processing, a shimmer sweep — everything that only
/// exists once the ribbon has opened.
///
/// Overflow is not clipped to the shape (`FontAtlas` has no clip-mask
/// parameter): the shown string is pre-trimmed to the longest *tail* that
/// measures inside the padded interior and drawn right-aligned, so the
/// newest word always sits against the right padding and the oldest ones
/// quietly drop off the left. No new text-rendering primitive needed.
#[allow(clippy::too_many_arguments)]
fn draw_ribbon(
    pixmap: &mut Pixmap,
    ctx: &Ctx<'_>,
    atlas: &mut FontAtlas,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    open: f32,
    clip: Option<&Mask>,
) {
    let alpha = text_alpha(open);
    if alpha <= 0.001 {
        return;
    }
    let l = ctx.layout;
    let theme = ctx.theme;

    if shown_state(ctx.model) == OverlayState::Processing {
        let phase = (ctx.model.now_ms() % u64::from(SCAN_PERIOD_MS)) as f32 / SCAN_PERIOD_MS as f32;
        let band_w = w * 0.35;
        let band_x = x - band_w + (w + band_w) * phase;
        let edge = (phase / 0.15).min(1.0).min((1.0 - phase) / 0.15).max(0.0);
        if let Some(band) = shapes::round_rect(band_x, y, band_w, h, h * 0.5) {
            fill_ramp(
                pixmap,
                ctx,
                &band,
                crate::layout::Rect {
                    x: band_x,
                    y,
                    w: band_w,
                    h,
                },
                theme.scan,
                ctx.alpha * 0.5 * edge * open,
                clip,
            );
        }
    }

    let available = (w - 2.0 * l.text_pad_x).max(0.0);
    let shown = text::trailing_fit(atlas, ctx.model.text(), l.text_font, available);

    // A soft band behind the run only — not the whole shell. `theme.spectrum`
    // is a colour ramp chosen for glassy variety, and cannot itself promise
    // contrast with `ink` at every point along it; `text_scrim` is the token
    // that does, so this closes the legibility gap exactly where it matters
    // instead of pulling the whole surface back toward opaque.
    //
    // Vertically the band is the face's own line box rather than the shape or
    // the shown substring's ink, and its top edge is held at or below the
    // bottom of the wave row — see `text_band`.
    let text_w = atlas.measure(shown, l.text_font, 0.0);
    if text_w > 0.0 {
        let scrim_pad = l.text_pad_x * 0.4;
        let band_right = (x + w - l.text_pad_x + scrim_pad).min(x + w);
        let band_w = (text_w + 2.0 * scrim_pad).min(w);
        let band_x = (band_right - band_w).max(x);

        let (band_top, band_bottom) = text_band(atlas, l, y, h, open);
        let band_h = band_bottom - band_top;
        if band_h > 0.0 {
            if let Some(path) = shapes::round_rect(band_x, band_top, band_w, band_h, band_h * 0.5) {
                fill(pixmap, ctx, &path, ctx.c(theme.text_scrim.fade(alpha)));
            }
        }
    }

    let (tx, ty) = ctx.map(x + w - l.text_pad_x, y + h * 0.5);
    atlas.draw(
        pixmap,
        shown,
        l.text_font,
        0.0,
        tx,
        ty,
        Align::Right,
        TextPaint::Solid(theme.ink),
        ctx.alpha * alpha,
    );
}

// No caption: the captain's second visual pass rejected the under-pill
// engine/model line and its geometry outright — "developer information on a
// user surface" — and the latency figure that occupied the same slot went
// with it as a direct consequence, not a separate decision (there was only
// ever one line under the shape). `Model::latency_ms()` and `theme.latency`
// still exist and are unused by rendering now; see the design report for
// why they were left in place rather than removed unilaterally.

// ---------------------------------------------------------------------------
// paint helpers
// ---------------------------------------------------------------------------

fn fill(pixmap: &mut Pixmap, ctx: &Ctx<'_>, path: &Path, colour: Rgba) {
    fill_clipped(pixmap, ctx, path, colour, None);
}

fn fill_clipped(
    pixmap: &mut Pixmap,
    ctx: &Ctx<'_>,
    path: &Path,
    colour: Rgba,
    clip: Option<&Mask>,
) {
    if colour.a <= 0.001 {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color(colour.to_color());
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, FillRule::Winding, ctx.xf, clip);
}

/// Fill `path` with a horizontal ramp spanning `across`.
fn fill_ramp(
    pixmap: &mut Pixmap,
    ctx: &Ctx<'_>,
    path: &Path,
    across: crate::layout::Rect,
    stops: &[Rgba],
    alpha: f32,
    clip: Option<&Mask>,
) {
    if alpha <= 0.001 || stops.is_empty() || across.w <= 0.0 {
        return;
    }
    let last = (stops.len() - 1).max(1) as f32;
    let gradient: Vec<GradientStop> = stops
        .iter()
        .enumerate()
        .map(|(i, c)| GradientStop::new(i as f32 / last, c.fade(alpha).to_color()))
        .collect();
    let shader = LinearGradient::new(
        Point::from_xy(across.x, across.center_y()),
        Point::from_xy(across.right(), across.center_y()),
        gradient,
        SpreadMode::Pad,
        Transform::identity(),
    );
    let paint = Paint {
        shader: shader.unwrap_or_else(|| Shader::SolidColor(stops[0].fade(alpha).to_color())),
        anti_alias: true,
        ..Paint::default()
    };
    pixmap.fill_path(path, &paint, FillRule::Winding, ctx.xf, clip);
}

fn stroke(pixmap: &mut Pixmap, ctx: &Ctx<'_>, path: Option<Path>, colour: Rgba, width: f32) {
    let (Some(path), true) = (path, colour.a > 0.001) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(colour.to_color());
    paint.anti_alias = true;
    let stroke_style = Stroke {
        width: width.max(0.1),
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke_style, ctx.xf, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::{EXIT_MS, INSERTED_HOLD_MS, STATE_CROSS_MS};
    use crate::state::Command;
    use crate::theme::{PORCELAIN_LIGHT, PRISM_DARK};

    /// Draws every tick, not just the last one — unlike the old fixed-size
    /// pill, `Renderer` integrates its own morph state (`openness`,
    /// `measured_w`) frame to frame via a `dt` it derives from consecutive
    /// `draw` calls, exactly like the real window loop drives it. A helper
    /// that ticked the model forward and drew only once would leave that
    /// state at its initial (closed) value regardless of what the model says.
    fn drive(commands: &[Command], run_ms: u64, theme: Theme) -> (Renderer, Model) {
        let mut model = Model::new(theme);
        model.tick(0);
        for c in commands {
            model.apply(c.clone());
        }
        let mut renderer = Renderer::new(1.0);
        let mut t = 0;
        renderer.draw(&model);
        while t < run_ms {
            t = (t + 16).min(run_ms);
            model.tick(t);
            renderer.draw(&model);
        }
        (renderer, model)
    }

    fn lit_pixels(pixmap: &Pixmap) -> usize {
        pixmap.pixels().iter().filter(|p| p.alpha() > 0).count()
    }

    /// Alpha of the pixel at the centre of the shape.
    fn centre_alpha(r: &Renderer) -> u8 {
        let l = r.layout();
        let (x, y) = (l.center_x as u32, l.center_y as u32);
        r.pixmap().pixels()[(y * l.window_w + x) as usize].alpha()
    }

    /// Alpha of a pixel in the shell body itself: off-centre and low enough to
    /// be clear of the core glyph and its halo, and the timer's own zone,
    /// all of which paint opaque colours over the glass.
    fn body_alpha(r: &Renderer) -> u8 {
        let l = r.layout();
        let x = (l.center_x - 12.0 * l.scale) as u32;
        let y = (l.center_y + 8.0 * l.scale) as u32;
        r.pixmap().pixels()[(y * l.window_w + x) as usize].alpha()
    }

    #[test]
    fn a_hidden_pill_writes_no_pixels() {
        let (r, model) = drive(&[], 200, PRISM_DARK);
        assert!(model.is_idle());
        assert_eq!(lit_pixels(r.pixmap()), 0, "hidden state left ink behind");
    }

    /// The horizontal span of every meaningfully-visible pixel at `y`, in
    /// device px. `min_alpha` excludes the blurred ambient-shadow and glow
    /// halos' long, near-invisible tails (alpha in the single digits out of
    /// 255), which bleed many px past the shape's actual edge and would
    /// otherwise be measured as part of it.
    fn lit_span_at(r: &Renderer, y: u32, min_alpha: u8) -> Option<(u32, u32)> {
        let l = r.layout();
        let row = y * l.window_w;
        let mut span = None;
        for x in 0..l.window_w {
            if r.pixmap().pixels()[(row + x) as usize].alpha() > min_alpha {
                span = Some(match span {
                    None => (x, x),
                    Some((lo, _)) => (lo, x),
                });
            }
        }
        span
    }

    /// Round 5 (`round5-direction.md`): "compact... hold the line near
    /// [round 4's 102] and do not let the wave row push it back toward 128."
    /// The resting shape (no live text — the shipped default) has to hold
    /// three things now — glyph, wave row, timer — so it cannot be round 4's
    /// bare 102 any more, but this pins the actual drawn width to
    /// `layout.rest_w` (the same regression a stray `shape_h` left in
    /// `draw`'s width formula would reintroduce) and checks it stays clearly
    /// short of round 3's rejected 128, not creeping back toward it.
    #[test]
    fn the_resting_shape_stays_compact_not_round_3s_capsule() {
        let (r, _) = drive(&[Command::ShowListening], 400, PRISM_DARK);
        let l = r.layout();
        let (lo, hi) = lit_span_at(&r, l.center_y as u32, 30).expect("nothing drawn at centre row");
        let width = (hi - lo) as f32;
        const ROUND_3_CAPSULE_W: f32 = 128.0;
        assert!(
            width < ROUND_3_CAPSULE_W * 0.95,
            "resting width {width} is as wide as round 3's rejected capsule"
        );
        // Within a shadow-blur's worth of the exact rest width — the shell
        // itself, not the ambient shadow bleeding a few px wider.
        assert!(
            (width - l.rest_w).abs() < 8.0,
            "resting width {width} is not close to layout.rest_w {}",
            l.rest_w
        );
    }

    /// The round-3 bug, at the unit level: every bar reacted to the *same*
    /// instantaneous level, with a positional taper doing the work of
    /// varying bar to bar — decorrelated position, not decorrelated time.
    /// `wave_bar_scale` must not read `i` at all for a real sample; only the
    /// idle-ripple branch (no sample yet) is allowed to vary by position.
    #[test]
    fn a_loud_sample_reads_the_same_regardless_of_which_bar_it_lands_on() {
        // Loud enough that WAVE_TEXTURE_AMPLITUDE's headroom term is
        // negligible — this is checking for a reintroduced *height* taper,
        // not the deliberate quiet-sample texture below.
        let first = wave_bar_scale(Some(1.0), 0.0, 12_345);
        for i in [1.0f32, 7.0, 19.0, 39.0] {
            let got = wave_bar_scale(Some(1.0), i, 12_345);
            assert!(
                (got - first).abs() < 1e-6,
                "bar 0 read {first}, bar {i} read {got} — a positional taper crept back in"
            );
        }
    }

    /// The counterpart to the test above: a real sample near the floor
    /// *must* vary by position, on purpose — this is the fix for silence
    /// rendering as a row of identical marks (firstmate review of the first
    /// round-5 cut), and it would be indistinguishable from the bug it fixes
    /// if this ever collapsed back to one value.
    #[test]
    fn a_quiet_real_sample_still_varies_by_position_like_the_idle_ripple_does() {
        let first = wave_bar_scale(Some(0.0), 0.0, 12_345);
        let mut distinct = std::collections::HashSet::new();
        for i in 0..8 {
            let got = wave_bar_scale(Some(0.0), i as f32, 12_345);
            distinct.insert((got * 10_000.0) as i64);
            assert!(
                got < WAVE_IDLE_FLOOR + WAVE_TEXTURE_AMPLITUDE + 1e-6,
                "bar {i} read {got}, past the texture's own ceiling — it would read as signal"
            );
        }
        assert!(
            distinct.len() > 1,
            "every bar read the exact same near-zero value ({first}) — that is the row of \
             identical marks this exists to fix"
        );
    }

    /// The expansive response curve the captain asked for in round 1 ("so
    /// it's showing that it's clearly hearing you") must still hold per
    /// sample: loud reads clearly taller than quiet.
    #[test]
    fn a_loud_sample_reads_taller_than_a_quiet_one() {
        let quiet = wave_bar_scale(Some(0.05), 3.0, 0);
        let loud = wave_bar_scale(Some(1.0), 3.0, 0);
        assert!(
            loud > quiet * 2.0,
            "quiet {quiet}, loud {loud} — the response barely differs"
        );
    }

    /// A column the rolling history has not reached yet (the row is still
    /// filling in from silence) must read as a quiet, believable ripple —
    /// round 5's own words for what a flat resting row is not allowed to be
    /// again: "not a flat line of identical stubs." Two things pin that:
    /// it stays below the real-signal floor (so it never masquerades as an
    /// actual loud moment), and it is not one constant value stamped across
    /// every idle bar (that flatness is the "dashes" failure by another
    /// name).
    #[test]
    fn an_unfilled_bar_ripples_quietly_instead_of_sitting_flat() {
        let quietest_real_sample = wave_bar_scale(Some(0.0), 0.0, 0);
        let mut distinct = std::collections::HashSet::new();
        for i in 0..8 {
            let idle = wave_bar_scale(None, i as f32, 5_000);
            assert!(
                idle <= quietest_real_sample + 1e-6,
                "bar {i}: idle ripple {idle} reached as high as a real quiet sample \
                 {quietest_real_sample} — it would read as signal, not silence"
            );
            distinct.insert((idle * 10_000.0) as i64);
        }
        assert!(
            distinct.len() > 1,
            "every idle bar read the exact same value — that is a flat line of \
             identical stubs, the failure this exists to avoid"
        );
    }

    /// The same idle ripple must animate — a believable ripple moves; a
    /// value frozen in time is a flat line that merely differs by position
    /// instead of by neither.
    #[test]
    fn the_idle_ripple_moves_over_time() {
        let a = wave_bar_scale(None, 2.0, 0);
        let b = wave_bar_scale(None, 2.0, 4_000);
        assert!(
            (a - b).abs() > 1e-6,
            "the idle ripple read {a} at t=0 and {b} at t=4000 — it is not moving"
        );
    }

    /// The whole point of round 5: the row has to be a real rolling history
    /// of what the microphone actually measured, not a single current level
    /// re-read every frame. Feeding two very different levels, settled long
    /// enough apart to leave the smoothing behind, must leave *both* values
    /// somewhere in the buffer — round 3's design had no memory at all, so a
    /// buffer that collapsed to one repeated value would be that bug back.
    #[test]
    fn the_history_keeps_distinct_samples_not_one_collapsed_value() {
        let mut model = Model::new(PRISM_DARK);
        model.tick(0);
        model.apply(Command::ShowListening);
        let mut r = Renderer::new(1.0);
        let mut t = 0u64;
        while t < 1_400 {
            t += 16;
            // Long enough per phase (well past LEVEL_ATTACK_MS/RELEASE_MS)
            // that consecutive sampled levels are genuinely different, not
            // still mid-transition from the last toggle.
            let level = if (t / 350) % 2 == 0 { 0.05 } else { 0.95 };
            model.apply(Command::Level(level));
            model.tick(t);
            r.draw(&model);
        }
        let history = r.wave_history_snapshot();
        assert!(
            history.len() >= 4,
            "only {} samples accumulated over 1.4s of listening",
            history.len()
        );
        let quiet = history.iter().filter(|&&v| v < 0.3).count();
        let loud = history.iter().filter(|&&v| v > 0.6).count();
        assert!(
            quiet > 0 && loud > 0,
            "history never captured both a quiet and a loud moment: {history:?}"
        );
    }

    /// Recording stops the instant `Listening` ends; the row must freeze
    /// exactly there, not keep manufacturing an ambient level to hold. A
    /// timeline of real audio that quietly starts inventing frames the
    /// microphone never produced is worse than one that just stops.
    #[test]
    fn the_history_stops_growing_the_moment_listening_ends() {
        let mut model = Model::new(PRISM_DARK);
        model.tick(0);
        model.apply(Command::ShowListening);
        model.apply(Command::Level(0.8));
        let mut r = Renderer::new(1.0);
        let mut t = 0u64;
        while t < 600 {
            t += 16;
            model.tick(t);
            r.draw(&model);
        }
        let settled = r.wave_history_snapshot();
        assert!(!settled.is_empty(), "nothing accumulated while listening");

        model.apply(Command::Processing);
        for _ in 0..80 {
            t += 16;
            model.tick(t);
            r.draw(&model);
        }
        assert_eq!(
            r.wave_history_snapshot(),
            settled,
            "the history kept changing after listening ended"
        );
    }

    /// Every appearance starts from silence. A wave row that kept the last
    /// utterance's shape across a fresh `ShowListening` would draw a loud
    /// waveform before the microphone had captured a single new sample.
    #[test]
    fn a_fresh_utterance_starts_the_wave_row_from_silence() {
        let mut model = Model::new(PRISM_DARK);
        model.tick(0);
        model.apply(Command::ShowListening);
        model.apply(Command::Level(1.0));
        let mut r = Renderer::new(1.0);
        let mut t = 0u64;
        while t < 600 {
            t += 16;
            model.tick(t);
            r.draw(&model);
        }
        assert!(!r.wave_history_snapshot().is_empty());

        model.apply(Command::Hide);
        while !model.is_idle() {
            t += 16;
            model.tick(t);
            r.draw(&model);
        }
        assert!(
            r.wave_history_snapshot().is_empty(),
            "history survived past the shape going fully hidden"
        );
    }

    /// The text scrim is held below the wave row, so the row's bottom edge is
    /// what decides whether the scrim can cover the glyphs at all. Measured
    /// against every printable ASCII character, because a `$` or a brace rides
    /// higher than any letter and the scrim is sized to whatever the
    /// transcript actually contains.
    #[test]
    fn the_wave_row_clears_the_live_text_ink_box() {
        let printable: String = (0x20u8..0x7F).map(char::from).collect();
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let l = Layout::new(scale);
            let mut atlas = FontAtlas::new();
            let (ink_top, _) = atlas.ink_extents(&printable, l.text_font);
            let baseline = l.shape_h * 0.5 + atlas.baseline_offset(l.text_font);
            let ink_box_top = baseline - ink_top;
            let wave_bottom =
                l.shape_h * 0.5 - (WAVE_Y_OFFSET_RIBBON - WAVE_MAX_H_RIBBON * 0.5) * l.scale;
            assert!(
                wave_bottom < ink_box_top,
                "scale {scale}: wave row ends at {wave_bottom}, into an ink box starting at {ink_box_top}"
            );
            let wave_top =
                l.shape_h * 0.5 - (WAVE_Y_OFFSET_RIBBON + WAVE_MAX_H_RIBBON * 0.5) * l.scale;
            assert!(
                wave_top > 0.0,
                "scale {scale}: wave row starts at {wave_top}, outside the shape"
            );
        }
    }

    /// A visual review of the first round-5 cut found the row reading as
    /// nearly invisible at rest — this drives a sustained loud level and
    /// checks that some bar's ink actually reaches well away from the row's
    /// own centre, against a silent frame drawn the same way, so what is
    /// being measured is genuine response rather than the glass underneath.
    #[test]
    fn the_wave_row_has_real_presence_at_a_loud_sustained_level() {
        // Long enough for the rolling history to fill with the held level.
        let (loud, _) = drive(
            &[Command::ShowListening, Command::Level(1.0)],
            1_000,
            PRISM_DARK,
        );
        let (silent, _) = drive(
            &[Command::ShowListening, Command::Level(0.0)],
            1_000,
            PRISM_DARK,
        );

        let l = loud.layout();
        let px = |r: &Renderer, x: u32, y: u32| r.pixmap().pixels()[(y * l.window_w + x) as usize];
        let cy = l.center_y as u32;
        // The row's usable span excludes the centred glyph and the timer
        // zone on the right — scan only the left portion, clear of both.
        let lo = (l.center_x - l.rest_w * 0.5 + WAVE_INSET * l.scale) as u32;
        let hi = (l.center_x - l.shape_h * 0.3) as u32;
        let reach = |r: &Renderer| -> u32 {
            let mut furthest = 0;
            for x in lo..hi {
                let mut dy = 0;
                while dy < (l.shape_h * 0.5) as u32 && px(r, x, cy.saturating_sub(dy)).alpha() > 200
                {
                    dy += 1;
                }
                furthest = furthest.max(dy.saturating_sub(1));
            }
            furthest
        };

        let loud_reach = reach(&loud);
        let quiet_reach = reach(&silent);
        assert!(
            loud_reach > (WAVE_MAX_H_REST * loud.layout().scale * 0.3) as u32,
            "loudest bar only reached {loud_reach} px from the row's centre"
        );
        assert!(
            quiet_reach * 2 < loud_reach,
            "a silent frame measured {quiet_reach} px against the loud frame's {loud_reach} — \
             this is measuring the glass body, not the bars"
        );
    }

    /// The captain's round-3 complaint was specifically the black band behind
    /// live text ruining the glass. Turning live text off by default (the
    /// fix) only holds if the scrim genuinely never paints without it: this
    /// drives the shipped default (no `PartialText` ever sent) through a full
    /// listening period and asserts no pixel in the row the scrim would
    /// occupy is anywhere near the scrim's own near-black RGB.
    #[test]
    fn text_scrim_never_paints_in_the_default_no_text_presentation() {
        let (r, _) = drive(
            &[Command::ShowListening, Command::Level(0.9)],
            1500,
            PRISM_DARK,
        );
        let l = r.layout();
        let y = l.center_y as u32;
        let (lo, hi) = lit_span_at(&r, y, 30).expect("nothing drawn at centre row");
        // Excludes a few px at each edge: that is the shape's own 1 px
        // `outer_ring`/`border` hairline (near-black by design, `0 0 0 1px`
        // in the original CSS spec), not the scrim, which paints a wide band
        // in the *interior* sized to the text run.
        let (lo, hi) = (lo + 4, hi.saturating_sub(4));
        for x in lo..=hi {
            let p = r.pixmap().pixels()[(y * l.window_w + x) as usize];
            // Below this the pixel is an imperceptible tail of the blurred
            // ambient shadow or state glow, not anything a person would read
            // as "a dark band on the glass".
            if p.alpha() <= 30 {
                continue;
            }
            let luminance = u32::from(p.red()) + u32::from(p.green()) + u32::from(p.blue());
            assert!(
                luminance > 15,
                "near-black pixel at x={x}: rgb=({},{},{}) alpha={} — looks like text_scrim \
                 painted without live text",
                p.red(),
                p.green(),
                p.blue(),
                p.alpha()
            );
        }
    }

    /// An elapsed-recording timer shares the capsule with the core glyph in
    /// the default presentation. This does not assert exact digits (that's
    /// `state::tests::timer_formats_like_a_stopwatch` and
    /// `the_timer_freezes_when_speech_ends`) — just that glyph ink is drawn
    /// in the right-aligned zone `draw_timer` owns, in `theme.ink` and never
    /// the near-black scrim.
    ///
    /// Colour is the whole of the assertion, deliberately. The zone sits
    /// inside the capsule, so the glass body lights every pixel in it at
    /// [`GLASS_FILL_ALPHA`] whether or not `draw_timer` runs at all; only the
    /// crisp ink pass composites to a near-opaque pixel carrying `theme.ink`'s
    /// own near-white RGB.
    #[test]
    fn the_timer_draws_ink_coloured_pixels_in_its_own_zone() {
        let (r, _) = drive(
            &[Command::ShowListening, Command::Level(0.5)],
            1200,
            PRISM_DARK,
        );
        let l = r.layout();
        let ink = PRISM_DARK.ink;
        // The zone the run actually lands in, read from the same helper the
        // renderer places it with rather than a repeated formula — the two
        // drifted apart once the timer started being pushed in to clear the
        // glyph.
        let mut atlas = FontAtlas::new();
        let timer_w = atlas.measure(&format_timer(1_200), l.timer_font, 0.0);
        let x = l.center_x - l.rest_w * 0.5;
        let right_edge = timer_right_edge(l, x, l.rest_w, timer_w) as u32;
        let zone_start = (right_edge as f32 - timer_w) as u32;
        let mut ink_px = 0;
        for y in (l.center_y - 6.0 * l.scale) as u32..=(l.center_y + 6.0 * l.scale) as u32 {
            for x in zone_start..=right_edge {
                let p = r.pixmap().pixels()[(y * l.window_w + x) as usize];
                if p.alpha() < 200 {
                    continue;
                }
                // Premultiplied, but at this alpha that is a rounding error.
                let near = |got: u8, want: u8| i32::from(got).abs_diff(i32::from(want)) < 24;
                assert!(
                    i32::from(p.red()) + i32::from(p.green()) + i32::from(p.blue()) >= 120,
                    "near-black opaque pixel at ({x},{y}) in the timer zone — text_scrim \
                     painted where only ink belongs"
                );
                if near(p.red(), ink.r) && near(p.green(), ink.g) && near(p.blue(), ink.b) {
                    ink_px += 1;
                }
            }
        }
        assert!(
            ink_px > 3,
            "timer zone drew {ink_px} ink-coloured px — the timer is not rendering"
        );
    }

    /// A tall bar sits close to the timer's reserved zone by construction —
    /// the margin `draw_wave`'s `usable` computation leaves is exactly
    /// `WAVE_INSET`, not a generous one — and a firstmate visual review
    /// flagged what looked like a collision in a rendered ramp-up frame.
    ///
    /// A first cut of this test compared the timer zone's raw pixel colours
    /// against `theme.spectrum`'s stops directly and failed on Porcelain —
    /// a false positive, not a real bug: `fill_glass_shell` tints the
    /// *entire* shell, timer zone included, with that same ramp at low alpha
    /// by design, so "the pixel is a spectrum colour" is true everywhere on
    /// the shell and proves nothing about a bar specifically. The property
    /// that actually holds is narrower and provably correct instead of
    /// merely plausible: nothing in the timer zone depends on the wave
    /// row's amplitude at all, since `draw_glyph` and `draw_timer` take no
    /// level, so a loud frame and a silent frame — same elapsed time, same
    /// digits, same everything else — must render *byte-identical* pixels
    /// there. A real collision would fail that regardless of what colour it
    /// happened to be.
    #[test]
    fn the_wave_row_never_reaches_the_timers_zone_at_full_amplitude() {
        for theme in [PRISM_DARK, PORCELAIN_LIGHT] {
            let (loud, _) = drive(&[Command::ShowListening, Command::Level(1.0)], 2_000, theme);
            let (quiet, _) = drive(&[Command::ShowListening, Command::Level(0.0)], 2_000, theme);
            let l = loud.layout();
            let mut atlas = FontAtlas::new();
            let timer_w = atlas.measure(&format_timer(2_000), l.timer_font, 0.0);
            let x = l.center_x - l.rest_w * 0.5;
            let right_edge = timer_right_edge(l, x, l.rest_w, timer_w) as u32;
            let zone_start = (right_edge as f32 - timer_w) as u32;

            let mut checked = 0;
            for y in (l.center_y - 12.0 * l.scale) as u32..=(l.center_y + 12.0 * l.scale) as u32 {
                for x in zone_start..=right_edge {
                    let idx = (y * l.window_w + x) as usize;
                    let a = loud.pixmap().pixels()[idx];
                    let b = quiet.pixmap().pixels()[idx];
                    let diff = i32::from(a.red()).abs_diff(i32::from(b.red()))
                        + i32::from(a.green()).abs_diff(i32::from(b.green()))
                        + i32::from(a.blue()).abs_diff(i32::from(b.blue()))
                        + i32::from(a.alpha()).abs_diff(i32::from(b.alpha()));
                    assert!(
                        diff == 0,
                        "{}: pixel ({x},{y}) in the timer zone differs between a loud and a \
                         silent frame ({a:?} vs {b:?}) — something wave-level-dependent is \
                         painting there",
                        theme.name
                    );
                    checked += 1;
                }
            }
            assert!(
                checked > 10,
                "{}: only {checked} px sampled in the timer zone",
                theme.name
            );
        }
    }

    /// The theme side of the timer's contrast guarantee is pinned in
    /// `theme::tests::the_timer_edge_reads_against_any_desktop_the_ink_cannot`;
    /// this is the renderer's half of it — that `theme.timer_edge` reaches
    /// the pixmap at all, in the zone the run occupies, and at a strength
    /// that survives compositing.
    ///
    /// It exists because the mechanism it replaced was invisible to every
    /// test: the old halo re-drew the run in `theme.ink`, so a frame with it
    /// and a frame without it differed only in stroke weight. Un-premultiplying
    /// before comparing is what makes "the outline is there" separable from
    /// "the outline is faint", which is the regression a lowered pass alpha
    /// would be.
    #[test]
    fn the_timer_is_traced_in_an_outline_colour_that_is_not_its_ink() {
        let (r, _) = drive(
            &[Command::ShowListening, Command::Level(0.5)],
            1200,
            PRISM_DARK,
        );
        let l = r.layout();
        let edge = PRISM_DARK.timer_edge;
        let mut atlas = FontAtlas::new();
        let timer_w = atlas.measure(&format_timer(1_200), l.timer_font, 0.0);
        let x = l.center_x - l.rest_w * 0.5;
        let right_edge = timer_right_edge(l, x, l.rest_w, timer_w) as u32;
        let zone_start = (right_edge as f32 - timer_w - TIMER_EDGE_OFFSET * l.scale) as u32;
        let mut edge_px = 0;
        for y in (l.center_y - 8.0 * l.scale) as u32..=(l.center_y + 8.0 * l.scale) as u32 {
            for x in zone_start..=right_edge {
                let p = r.pixmap().pixels()[(y * l.window_w + x) as usize];
                if p.alpha() < 200 {
                    continue;
                }
                let straight = |c: u8| (u32::from(c) * 255 / u32::from(p.alpha())).min(255) as u8;
                let near = |got: u8, want: u8| i32::from(got).abs_diff(i32::from(want)) < 30;
                if near(straight(p.red()), edge.r)
                    && near(straight(p.green()), edge.g)
                    && near(straight(p.blue()), edge.b)
                {
                    edge_px += 1;
                }
            }
        }
        assert!(
            edge_px > 3,
            "timer zone drew {edge_px} px of theme.timer_edge — the readout is back to being \
             outlined in its own ink, which adds weight and no contrast"
        );
    }

    /// The shared guard for this file's recurring compositing bug: two things
    /// meant to fade together, reasoned about only at the endpoints, wrong
    /// everywhere in between. It has now bitten unrelated pairs on this
    /// shape — the elapsed timer against live text, and the timer's outline
    /// against its own ink.
    ///
    /// Use it for **any** element in this renderer drawn as several
    /// accumulating alpha passes that is supposed to fade in proportion with
    /// something drawn beside it: pass the element's per-pass alpha as a
    /// function of presence, its companion's alpha as another, and the number
    /// of passes that overlap. The check is the only one that matters, and it
    /// is the one an endpoint test cannot make: that the ratio between the
    /// stack's accumulated opacity and its companion's is the *same* at every
    /// intermediate presence as it is at full presence.
    ///
    /// It is not a convention to remember. `#[should_panic]` sibling
    /// `the_fade_proportionality_guard_rejects_the_naive_accumulation` runs
    /// the naive `pass_alpha * a` scheme through it and requires it to fail,
    /// so the guard is known to bite rather than merely to pass.
    fn assert_fades_in_proportion(
        what: &str,
        passes: usize,
        pass_alpha: impl Fn(f32) -> f32,
        companion_alpha: impl Fn(f32) -> f32,
    ) {
        let settled_companion = companion_alpha(1.0);
        assert!(
            settled_companion > 0.0,
            "{what}: the companion is invisible at full presence, so there is \
             no proportion to hold — check the arguments, not the renderer"
        );
        let settled = accumulated_alpha(pass_alpha(1.0), passes);
        let ratio = settled / settled_companion;
        for step in 0..=100 {
            let a = step as f32 / 100.0;
            let companion = companion_alpha(a);
            let got = accumulated_alpha(pass_alpha(a), passes);
            let want = companion * ratio;
            assert!(
                (got - want).abs() <= 0.01,
                "{what}: at presence {a} the {passes} accumulating passes reach \
                 {got} beside a companion at {companion}; proportional to the \
                 settled frame would be {want}. The stack outruns what it \
                 accompanies mid-fade — scale the accumulated alpha, not the \
                 per-pass alpha (see `accumulating_pass_alpha`)."
            );
        }
    }

    /// The guard above, run against the exact mistake it exists to catch, so a
    /// green run means it is discriminating and not just arithmetic that
    /// always agrees with itself.
    #[test]
    #[should_panic(expected = "outruns what it accompanies mid-fade")]
    fn the_fade_proportionality_guard_rejects_the_naive_accumulation() {
        assert_fades_in_proportion(
            "a stack compositing every pass at presence * TIMER_EDGE_PASS_ALPHA",
            TIMER_EDGE_DIRS.len(),
            |a| a * TIMER_EDGE_PASS_ALPHA,
            |a| a,
        );
    }

    /// Item (1): the timer's outline stack and its single crisp `theme.ink`
    /// pass fade as one.
    ///
    /// `a = ctx.alpha * timer_alpha(open)`, and `ctx.alpha` is the overlay's
    /// presence ramp, so every value below 1.0 here is a frame the user sees
    /// on every dictation — `ENTER_MS` in, `EXIT_MS` out. Sampling only the
    /// settled frame is exactly how this shipped: at full presence the stack
    /// is near-solid either way and the two schemes are indistinguishable.
    #[test]
    fn the_timer_outline_fades_in_proportion_with_the_ink_it_traces() {
        assert_fades_in_proportion(
            "theme.timer_edge outline vs the theme.ink fill it traces",
            TIMER_EDGE_DIRS.len(),
            timer_edge_pass_alpha,
            |a| a,
        );

        // The settled contrast guarantee the outline was added for, unchanged:
        // at full presence the solve has to return the authored per-pass alpha
        // itself, not an approximation of it.
        let settled = timer_edge_pass_alpha(1.0);
        assert!(
            (settled - TIMER_EDGE_PASS_ALPHA).abs() < 1e-4,
            "full presence composites each outline pass at {settled}, not the \
             authored {TIMER_EDGE_PASS_ALPHA} — the settled rim changed weight"
        );

        // The two mid-fade values the defect was reported at, named rather
        // than swept, because these are the numbers a future reader will
        // check the fix against.
        for a in [0.5f32, 0.3] {
            let reached = accumulated_alpha(timer_edge_pass_alpha(a), TIMER_EDGE_DIRS.len());
            assert!(
                (reached - a).abs() <= 0.01,
                "at presence {a} the outline accumulates to {reached} against \
                 ink at {a}"
            );
        }
    }

    /// The three things one listening frame has to get right at once, and the
    /// only one of them that used to be checked was the corner. The centre
    /// pixel is the live core dot, which is opaque `theme.rec`; the body
    /// around it is glass at [`GLASS_FILL_ALPHA`] and must stay well short of
    /// opaque, which is the whole point of the treatment and what a
    /// reintroduced text-driven opacity ramp would break.
    #[test]
    fn a_listening_capsule_is_glass_around_an_opaque_core_and_clear_at_the_corners() {
        let (r, _) = drive(
            &[Command::ShowListening, Command::Level(0.8)],
            400,
            PRISM_DARK,
        );
        assert!(centre_alpha(&r) > 240, "the live core dot is not opaque");
        let body = body_alpha(&r);
        assert!(body > 0, "the shell body drew nothing at all");
        assert!(
            body < 128,
            "the shell body reads as opaque, not glass: alpha {body}"
        );
        assert_eq!(
            r.pixmap().pixels()[0].alpha(),
            0,
            "corner is not transparent"
        );
    }

    /// What the scrim has to be for the whole of the morph, and what it has to
    /// cover once the ribbon is open.
    ///
    /// Deliberately *not* "the band starts below the wave row": `text_band`
    /// computes its top as `natural.max(wave_row_bottom(..))`, so re-deriving
    /// the row's bottom edge here and asserting the band clears it restates
    /// the same `max` and passes for any `open` and any value of the wave
    /// constants. That version could only fail by `text_band` dropping the
    /// call entirely — it read as coverage while checking nothing.
    ///
    /// These two can fail. The clamp raises the band's ceiling, so it can
    /// close the band onto its own floor and take the whole scrim out through
    /// `draw_ribbon`'s `band_h > 0.0` guard, silently, for the frames the
    /// clamp is tightest — that is the first assertion, swept across the tween
    /// rather than at its ends, because the ceiling only moves in between.
    /// The second is what the scrim exists for at all: with the ribbon open,
    /// the band still backs the real glyph ink of every printable character,
    /// measured from the face rather than assumed from the line box.
    #[test]
    fn the_text_scrim_is_a_real_band_across_the_morph_and_backs_the_ink_when_open() {
        let printable: String = (0x20u8..0x7F).map(char::from).collect();
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let l = Layout::new(scale);
            let mut atlas = FontAtlas::new();
            let y = l.center_y - l.shape_h * 0.5;

            let mut sampled = 0;
            for step in 0..=40 {
                let open = step as f32 / 40.0;
                if text_alpha(open) <= 0.0 {
                    continue;
                }
                let (band_top, band_bottom) = text_band(&atlas, &l, y, l.shape_h, open);
                assert!(
                    band_bottom - band_top > 1.0 * l.scale,
                    "scale {scale}, open {open}: the scrim collapsed to \
                     {band_top}..{band_bottom} — `draw_ribbon` drops a band \
                     that thin and the text is painted straight onto glass"
                );
                sampled += 1;
            }
            assert!(sampled > 10, "scale {scale}: swept only {sampled} frames");

            let (ink_top, ink_bottom) = atlas.ink_extents(&printable, l.text_font);
            let baseline = y + l.shape_h * 0.5 + atlas.baseline_offset(l.text_font);
            let (band_top, band_bottom) = text_band(&atlas, &l, y, l.shape_h, 1.0);
            assert!(
                band_top <= baseline - ink_top,
                "scale {scale}: with the ribbon open the scrim starts at \
                 {band_top}, below an ink box starting at {}",
                baseline - ink_top
            );
            assert!(
                band_bottom >= baseline - ink_bottom,
                "scale {scale}: with the ribbon open the scrim ends at \
                 {band_bottom}, above an ink box reaching {}",
                baseline - ink_bottom
            );
        }
    }

    /// The band the live text sits in must be a function of the ribbon's
    /// geometry and the face, and of nothing else. Anything derived from the
    /// shown substring moves as words scroll off the left, and the text moves
    /// with it — the whole run visibly bouncing as a descender or a tall
    /// ascender enters or leaves the window.
    #[test]
    fn the_text_band_does_not_move_with_the_text() {
        for scale in [1.0f32, 1.5, 2.0] {
            let l = Layout::new(scale);
            let mut atlas = FontAtlas::new();
            let y = l.center_y - l.shape_h * 0.5;
            let reference = text_band(&atlas, &l, y, l.shape_h, 1.0);
            // Every shape of run the marquee can leave behind: x-height only,
            // ascenders only, descenders only, tall punctuation, and empty.
            for text in [
                "acemnorsu",
                "the quarterly",
                "jumps over pygmy",
                "({[$#@|]})",
                "three more charts",
                "",
            ] {
                atlas.measure(text, l.text_font, 0.0);
                assert_eq!(
                    text_band(&atlas, &l, y, l.shape_h, 1.0),
                    reference,
                    "scale {scale}: band moved for {text:?}"
                );
            }
            let (top, bottom) = reference;
            assert!(bottom > top, "scale {scale}: empty band {top}..{bottom}");
        }
    }

    /// The exit that follows a successful insert: the checkmark fades out on
    /// its own. The bug this pins had the mint core dot fading back *in*
    /// underneath it, because the state cross-fade kept running toward
    /// `Hidden` — which draws nothing — while presence was already carrying
    /// the exit.
    #[test]
    fn nothing_fades_back_in_while_the_inserted_check_leaves() {
        let mut model = Model::new(PRISM_DARK);
        model.tick(0);
        model.apply(Command::ShowListening);
        model.apply(Command::Level(0.9));
        model.apply(Command::Inserted { latency_ms: 142 });

        let layout = Layout::new(1.0);
        let theme = PRISM_DARK;
        let mut t = 0u64;
        let mut sampled = 0;
        while t < u64::from(INSERTED_HOLD_MS) + u64::from(EXIT_MS) {
            t += 16;
            model.tick(t);
            if model.state().is_visible() {
                continue;
            }
            let ctx = Ctx {
                layout: &layout,
                theme: &theme,
                xf: Transform::identity(),
                alpha: model.presence(),
                model: &model,
            };
            let inserted = ctx.state_alpha(OverlayState::Inserted);
            assert!(
                (inserted - 1.0).abs() < 1e-6,
                "at {t} ms the inserted weight had already decayed to {inserted}"
            );
            assert!(
                (1.0 - inserted) * glyph_alpha(0.0) <= 0.001,
                "at {t} ms the live core dot came back"
            );
            sampled += 1;
        }
        assert!(sampled > 4, "only {sampled} exit frames sampled");
    }

    /// Every appearance decision has to agree about what is on screen while
    /// the shape leaves. The state weights were frozen first; the halo
    /// colour, the core dot's colour and the processing shimmer are the
    /// siblings that used to read the raw state and so answered `Hidden` from
    /// the first exit frame — the halo lerping mint → sky and 0.12 → 0.08
    /// alpha, and the core snapping sky → mint the instant a cancel from
    /// `Processing` landed, all while the shape they belong to was still
    /// fully drawn.
    #[test]
    fn colour_and_shimmer_hold_their_answers_through_the_exit() {
        for (name, setup, hide) in [
            (
                "inserted",
                vec![
                    Command::ShowListening,
                    Command::Inserted { latency_ms: 142 },
                ],
                false,
            ),
            (
                "cancelled while processing",
                vec![Command::ShowListening, Command::Processing],
                true,
            ),
            (
                "cancelled while listening",
                vec![Command::ShowListening],
                true,
            ),
        ] {
            for theme in [PRISM_DARK, PORCELAIN_LIGHT] {
                let mut model = Model::new(theme);
                model.tick(0);
                for c in &setup {
                    model.apply(c.clone());
                }
                let mut t = 0u64;
                // Settle the state cross-fade, then start leaving.
                while t < u64::from(STATE_CROSS_MS) + 32 {
                    t += 16;
                    model.tick(t);
                }
                let last_visible = (
                    shown_state(&model),
                    glow_colour(&theme, &model),
                    core_colour(&theme, &model),
                );
                if hide {
                    model.apply(Command::Hide);
                }

                let mut frames = 0;
                while !model.is_idle() {
                    t += 16;
                    model.tick(t);
                    if model.state().is_visible() {
                        continue;
                    }
                    assert_eq!(
                        (
                            shown_state(&model),
                            glow_colour(&theme, &model),
                            core_colour(&theme, &model)
                        ),
                        last_visible,
                        "{name} on {}: the look changed at {t} ms, mid-exit",
                        theme.name
                    );
                    frames += 1;
                }
                assert!(frames > 4, "{name}: only {frames} exit frames sampled");
            }
        }
    }

    #[test]
    fn every_visible_state_draws_something() {
        for (name, commands) in [
            ("listening", vec![Command::ShowListening]),
            (
                "processing",
                vec![Command::ShowListening, Command::Processing],
            ),
            (
                "inserted",
                vec![
                    Command::ShowListening,
                    Command::Processing,
                    Command::Inserted { latency_ms: 142 },
                ],
            ),
        ] {
            let (r, _) = drive(&commands, 200, PRISM_DARK);
            assert!(lit_pixels(r.pixmap()) > 500, "{name} drew almost nothing");
        }
    }

    #[test]
    fn an_open_ribbon_draws_more_than_the_resting_capsule() {
        let (closed, _) = drive(&[Command::ShowListening], 200, PRISM_DARK);
        let (open, _) = drive(
            &[
                Command::ShowListening,
                Command::PartialText("the quarterly report needs three more charts".into()),
            ],
            600,
            PRISM_DARK,
        );
        assert!(
            lit_pixels(open.pixmap()) > lit_pixels(closed.pixmap()) * 2,
            "ribbon did not visibly open: {} vs {}",
            lit_pixels(open.pixmap()),
            lit_pixels(closed.pixmap())
        );
    }

    #[test]
    fn the_frame_stays_premultiplied() {
        let (r, _) = drive(
            &[
                Command::ShowListening,
                Command::Level(1.0),
                Command::PartialText("hello there".into()),
            ],
            300,
            PRISM_DARK,
        );
        for p in r.pixmap().pixels() {
            assert!(p.red() <= p.alpha() && p.green() <= p.alpha() && p.blue() <= p.alpha());
        }
    }

    #[test]
    fn nothing_is_drawn_outside_the_window() {
        let mut model = Model::new(PRISM_DARK);
        model.tick(0);
        model.apply(Command::ShowListening);
        model.tick(40);
        let mut r = Renderer::new(1.5);
        r.draw(&model);
        assert_eq!(r.pixmap().width(), r.layout().window_w);
        assert_eq!(r.pixmap().height(), r.layout().window_h);
    }

    #[test]
    fn both_themes_render_and_differ() {
        let (dark, _) = drive(&[Command::ShowListening], 300, PRISM_DARK);
        let (light, _) = drive(&[Command::ShowListening], 300, PORCELAIN_LIGHT);
        assert_eq!(dark.pixmap().data().len(), light.pixmap().data().len());
        assert_ne!(
            dark.pixmap().data(),
            light.pixmap().data(),
            "themes identical"
        );

        let brightness = |r: &Renderer| {
            let l = r.layout();
            let i = ((l.center_y as u32) * l.window_w + l.center_x as u32) as usize;
            let p = r.pixmap().pixels()[i];
            u32::from(p.red()) + u32::from(p.green()) + u32::from(p.blue())
        };
        // Both show the live core dot at centre, so sample just off-centre
        // where the shell gradient itself is visible.
        let shell_brightness = |r: &Renderer| {
            let l = r.layout();
            let i = ((l.center_y as u32) * l.window_w + (l.center_x as i32 - 12).max(0) as u32)
                as usize;
            let p = r.pixmap().pixels()[i];
            u32::from(p.red()) + u32::from(p.green()) + u32::from(p.blue())
        };
        let _ = brightness;
        assert!(shell_brightness(&dark) < shell_brightness(&light));
    }

    #[test]
    fn re_scaling_resizes_the_frame() {
        let mut r = Renderer::new(1.0);
        assert_eq!((r.pixmap().width(), r.pixmap().height()), (528, 102));
        r.set_scale(2.0);
        assert_eq!((r.pixmap().width(), r.pixmap().height()), (1056, 204));
        assert!((r.layout().scale - 2.0).abs() < f32::EPSILON);
        r.set_scale(2.0);
        assert_eq!((r.pixmap().width(), r.pixmap().height()), (1056, 204));
    }

    #[test]
    fn a_full_cycle_never_panics_at_any_scale() {
        for scale in [1.0, 1.5, 2.0] {
            let mut model = Model::new(PRISM_DARK);
            let mut r = Renderer::new(scale);
            model.tick(0);
            let words = "the quick brown fox jumps over the lazy dog and keeps talking well past the edge of the ribbon";
            let mut t = 0u64;
            for step in 0..140 {
                match step {
                    5 => drop(model.apply(Command::ShowListening)),
                    100 => drop(model.apply(Command::Processing)),
                    115 => drop(model.apply(Command::Inserted { latency_ms: 142 })),
                    _ => {}
                }
                model.apply(Command::Level((step as f32 * 0.11).sin().abs()));
                let said = ((step * 2) as usize).min(words.len());
                model.apply(Command::PartialText(words[..said].to_string()));
                t += 16;
                model.tick(t);
                r.draw(&model);
            }
        }
    }

    #[test]
    fn mid_enter_frames_are_translucent() {
        let mut model = Model::new(PRISM_DARK);
        model.tick(0);
        model.apply(Command::ShowListening);
        model.tick(8);
        let mut r = Renderer::new(1.0);
        r.draw(&model);
        let a = centre_alpha(&r);
        assert!(a > 0, "nothing drawn mid-enter");
        assert!(a < 250, "shape was fully opaque only 8 ms in: {a}");
    }

    // -- the two animation bugs found during prototyping, pinned as regressions --

    /// One-pole width smoothing is asymptotic and never exactly reaches its
    /// target; a ribbon that looks fully grown but is a sub-pixel short of
    /// fitting the text used to silently drop a whole leading character. This
    /// drives the model at a coarse, uneven tick step (reproducing how it was
    /// originally caught) and asserts the shown text never loses more than
    /// the minimum characters needed to fit.
    #[test]
    fn width_smoothing_does_not_drop_a_whole_character_once_settled() {
        let mut model = Model::new(PRISM_DARK);
        model.tick(0);
        model.apply(Command::ShowListening);
        model.apply(Command::PartialText("the quarterly".into()));

        let mut r = Renderer::new(1.0);
        let mut t = 0u64;
        // Coarse, slightly uneven steps — the conditions that first surfaced
        // the bug — held long enough for the one-pole filter to fully settle.
        for step in [80u64, 80, 80, 80, 80, 80] {
            t += step;
            model.tick(t);
            r.draw(&model);
        }

        let available = r.measured_w - 2.0 * r.layout().text_pad_x;
        let text_font = r.layout().text_font;
        let shown = text::trailing_fit(&mut r.atlas, "the quarterly", text_font, available);
        assert_eq!(
            shown, "the quarterly",
            "settled ribbon dropped a character it had room for: {shown:?}"
        );
    }

    /// The closed-state glyph and the open-state text must be a true
    /// crossfade, not two independently-tuned linear fades that happen to
    /// overlap — otherwise the checkmark is visibly drawn underneath
    /// still-wide ribbon text during the collapse.
    #[test]
    fn glyph_and_text_alpha_are_a_true_crossfade() {
        // The original bug: the checkmark visibly drawn *underneath text that
        // had not yet started fading* — i.e. both near-fully-opaque at once.
        // That is what this pins, not "never simultaneously nonzero", which
        // is just what a crossfade is.
        assert!(
            glyph_alpha(0.0) > 0.99 && text_alpha(0.0) < 0.01,
            "fully closed"
        );
        assert!(
            glyph_alpha(1.0) < 0.01 && text_alpha(1.0) > 0.99,
            "fully open"
        );

        let mut open = 0.0f32;
        while open <= 1.0 {
            let sum = glyph_alpha(open) + text_alpha(open);
            assert!(
                (sum - 1.0).abs() < 1e-5,
                "open={open}: glyph+text={sum}, not a clean handoff"
            );
            assert!(
                glyph_alpha(open) < 0.95 || text_alpha(open) < 0.05,
                "open={open}: glyph is nearly opaque while text is not yet faded, the original bug"
            );
            open += 0.01;
        }
    }

    /// `draw_timer` and `draw_ribbon` draw on the *same* anchor — right
    /// aligned at the shape's text padding — so, unlike the centred glyph,
    /// they cannot share a crossfade window: any `open` leaving both visible
    /// composites the elapsed digits over the newest live word. That was real
    /// for the ~30-50 ms the ribbon spends between `HANDOFF_LO` and
    /// `HANDOFF_HI` on every open and every collapse.
    ///
    /// The second half matters as much as the first: mutual exclusion is
    /// trivial to get by hard-cutting the timer to zero the moment text
    /// exists, which would pop a fully-opaque readout off the glass. This
    /// pins that the timer still *fades*.
    #[test]
    fn the_timer_and_the_live_text_never_share_the_right_aligned_row() {
        let steps = 1_000;
        let mut previous = timer_alpha(0.0);
        assert!(previous > 0.99, "the resting capsule must show its timer");
        for i in 0..=steps {
            let open = i as f32 / steps as f32;
            let (timer, text) = (timer_alpha(open), text_alpha(open));
            assert!(
                timer <= 0.001 || text <= 0.001,
                "open={open}: timer at {timer} and live text at {text} over one anchor"
            );
            assert!(
                (timer - previous).abs() < 0.02,
                "open={open}: the timer's alpha jumped {} in one step — that is a pop, not a fade",
                (timer - previous).abs()
            );
            previous = timer;
        }
        assert!(
            timer_alpha(1.0) <= 0.001,
            "the open ribbon must not carry a timer"
        );
    }

    /// The capsule is narrow by decision, and both the glyph and the timer are
    /// centred on fixed anchors inside it, so nothing about the composition
    /// guarantees they miss each other — at the resting padding a four-character
    /// readout's leading digit landed on the spinner's outer edge, and an
    /// unbounded format put it inside the checkmark. Measured with the real
    /// face at every scale, against the widest run the format can produce.
    #[test]
    fn the_timer_keeps_real_air_between_itself_and_the_centred_glyph() {
        for scale in [1.0f32, 1.25, 1.5, 2.0, 3.0] {
            let l = Layout::new(scale);
            let mut atlas = FontAtlas::new();
            // Saturating format: this is the widest run that can ever be
            // drawn, whatever the model's clock says.
            let widest = format_timer(u64::MAX);
            let timer_w = atlas.measure(&widest, l.timer_font, 0.0);
            let x = l.center_x - l.rest_w * 0.5;
            let right = timer_right_edge(&l, x, l.rest_w, timer_w);
            let gap = (right - timer_w) - (l.center_x + glyph_half_w(&l));
            assert!(
                gap >= GLYPH_TIMER_GAP * scale - 0.01,
                "scale {scale}: only {gap} device px between the glyph's ink and {widest:?}"
            );
            assert!(
                right <= x + l.rest_w - TIMER_EDGE_PAD_MIN * scale + 0.01,
                "scale {scale}: the run was pushed to {right}, past the capsule's own padding"
            );
            assert!(
                right >= x + l.rest_w - l.text_pad_x - 0.01,
                "scale {scale}: the run moved left of its resting anchor"
            );
        }
    }

    /// `glyph_half_w` is what buys that clearance, so it has to cover every
    /// mark `draw_glyph` can paint — including the listening halo at its
    /// widest, which is the outermost of them and the easiest to forget
    /// because it is faint by the time it gets there.
    #[test]
    fn the_reserved_glyph_width_covers_every_mark_the_glyph_draws() {
        let l = Layout::new(1.0);
        let reserved = glyph_half_w(&l);
        let spinner = l.shape_h * SPINNER_R_FRAC + (l.shape_h * SPINNER_STROKE_FRAC) * 0.5;
        let halo = l.shape_h * (HALO_R_FRAC + HALO_GROW_FRAC);
        let core = l.shape_h * CORE_R_FRAC * CORE_PULSE_MAX;
        let (check, _) = shapes::check_mark(0.0, 0.0, l.shape_h * CHECK_SIZE_FRAC).unwrap();
        let check = check.bounds().right() + (l.shape_h * CHECK_STROKE_FRAC) * 0.5;
        for (name, reach) in [
            ("spinner", spinner),
            ("halo", halo),
            ("core", core),
            ("check", check),
        ] {
            assert!(
                reserved >= reach - 0.01,
                "the {name} reaches {reach} px from centre, but only {reserved} is reserved"
            );
        }
    }
}

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
/// shape's life. Constant on purpose: legibility of the live text is carried
/// by `theme.text_scrim` alone (see [`draw_ribbon`]), so the surface never has
/// to trade its glassiness for contrast. `theme::tests` composites this over
/// the spectrum to check that guarantee against the real on-screen colour.
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

// ---------------------------------------------------------------------------
// the wave
// ---------------------------------------------------------------------------
//
// Brought back after direct captain feedback on a first pass that dropped it
// in favour of a plain pulsing dot: "I like the waves, and I like how it
// moves." Two things are deliberately different from the previous 28-bar
// design it echoes:
//
// - The taper has a floor instead of hitting exactly zero at both ends. The
//   previous row's `sqrt(sin(pi*p))` taper measured out at under 75% of peak
//   height for the outer ~18% of bars each side even at maximum volume, and
//   the very end bars never moved at all regardless of signal — see the
//   design report for the exact numbers. That is what "the waves... get cut
//   off about 75%" was.
// - The response curve is expansive (`powf(1.6)`), not linear, so quiet and
//   loud read as clearly different at a glance rather than compressing
//   toward the middle — the captain's explicit ask, and a real functional
//   need: silent dictations were going unnoticed until the text failed to
//   land.
//
// Bar count and pitch are recomputed from the shape's *current* width every
// frame — the same reasoning as the mask cache above: a fixed bar count
// would either be sparse and thin at the wide-open ribbon or crowded past
// legibility at the orb's 34px, so density targets a constant pitch instead
// and the bar count follows.
const WAVE_INSET: f32 = 7.0;
const WAVE_TARGET_PITCH: f32 = 12.0;
const WAVE_MIN_BARS: usize = 7;
const WAVE_MAX_BARS: usize = 40;
const WAVE_BAR_W_FRAC: f32 = 0.46;
/// Bar height at full deflection, and how far the row's centre sits above the
/// shape's — for the two ends of the `open` tween.
///
/// Round 3 (captain, live-desktop review): "waves that get bigger whenever
/// the voice is louder" is the primary content of the default (no live text)
/// presentation, and the row was still sized for its old job — a decoration
/// sharing the shape with a wide text run — which reads as a few flat ticks
/// at this shape's size. But that old sizing cannot simply grow: `_RIBBON`'s
/// bottom edge — `h/2 - (WAVE_Y_OFFSET_RIBBON - WAVE_MAX_H_RIBBON/2)` — has to
/// stay clear of the top of the live text's ink box once the ribbon opens, or
/// the text scrim (held below the row, see [`draw_ribbon`]) cannot cover the
/// glyphs it exists to back; at this shape height that ceiling is only a
/// couple of px above the old numbers. So the row now has two sizes, crossfed
/// by `open` exactly like everything else that changes as the ribbon opens
/// (see [`wave_geometry`]): big and centred at rest, where nothing else
/// shares its vertical space but the core glyph (which paints over it, not
/// beside it) and the timer (off to the side, out of the row's x-range via
/// `right_reserve`); small and offset once real text needs the row below it.
/// `_RIBBON`'s numbers are untouched from before this round —
/// `the_wave_row_clears_the_live_text_ink_box` still pins them against the
/// real font at every scale, and that guard is the one to run before
/// retuning either `_RIBBON` constant.
const WAVE_MAX_H_REST: f32 = 22.0;
const WAVE_Y_OFFSET_REST: f32 = 0.0;
const WAVE_MAX_H_RIBBON: f32 = 6.0;
const WAVE_Y_OFFSET_RIBBON: f32 = 12.5;
const WAVE_IDLE_FLOOR: f32 = 0.05;
const WAVE_PROCESSING_ENV: f32 = 0.16;
const WAVE_RESTING_ENV: f32 = 0.05;

/// Breathing room above and below the live text's ink box before the text
/// scrim's rounded edge starts, in logical pixels.
const SCRIM_PAD_Y: f32 = 3.0;

/// Gap between the wave row's right edge and the timer's left edge, so the
/// two read as sharing the capsule rather than colliding. See [`draw_timer`].
const WAVE_TIMER_GAP: f32 = 8.0;

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
/// The one place both [`draw_wave`] and [`text_band`] read the row's extent
/// from, so the scrim's ceiling can never be pinned to a size the bars do not
/// actually have yet. [`draw_wave`] places its tallest bar at `cy ± max_h/2`
/// off the same [`wave_geometry`] pair; this is that lower edge, in closed
/// form.
fn wave_row_bottom(l: &Layout, y: f32, h: f32, open: f32) -> f32 {
    let (max_h, y_offset) = wave_geometry(open);
    y + h * 0.5 - (y_offset - max_h * 0.5) * l.scale
}

/// scaleY for one wave bar. `p` is its position across the row, 0.0–1.0;
/// `i` is its raw index, used only to decorrelate neighbours.
fn wave_bar_scale(p: f32, i: f32, env: f32, now_ms: u64) -> f32 {
    let t = now_ms as f32;
    // Floor of 0.4 rather than 0: a gentle lens taper without ever going
    // fully flat at the ends. See the module-level comment above.
    let taper = 0.4 + 0.6 * (std::f32::consts::PI * p).sin().max(0.0).sqrt();
    let wobble = 0.4 + 0.6 * ((t / 88.0 + i * 0.9).sin() * (t / 230.0 + i * 0.33).sin()).abs();
    // Expansive, not linear: widens the gap between quiet and loud instead
    // of compressing it.
    let response = env.clamp(0.0, 1.0).powf(1.6);
    WAVE_IDLE_FLOOR + response * taper * wobble * (1.0 - WAVE_IDLE_FLOOR)
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
/// width shrinks to leave it room rather than the two overlapping. It shrinks
/// to zero in lockstep with the timer's own fade as live text opens the
/// ribbon, so the row smoothly reclaims the full width exactly as the timer
/// vacates it.
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
    clip: Option<&Mask>,
) {
    let l = ctx.layout;
    let theme = ctx.theme;
    let model = ctx.model;

    let listening = ctx.state_alpha(OverlayState::Listening);
    let processing = ctx.state_alpha(OverlayState::Processing);
    let inserted = ctx.state_alpha(OverlayState::Inserted);
    let alpha = wave_alpha(listening, processing, inserted);
    if alpha <= 0.001 {
        return;
    }
    let env = model.level() * listening
        + WAVE_PROCESSING_ENV * processing
        + WAVE_RESTING_ENV * (1.0 - listening - processing).max(0.0);

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
    let now_ms = model.now_ms();

    for i in 0..count {
        let p = if count > 1 {
            i as f32 / (count - 1) as f32
        } else {
            0.5
        };
        let scale = wave_bar_scale(p, i as f32, env, now_ms);
        let bh = (max_h * scale).max(l.scale * 0.6);
        let bx = x + inset + pitch * i as f32 + (pitch - bar_w) * 0.5;
        let by = cy - bh * 0.5;
        if let Some(path) = shapes::round_rect(bx, by, bar_w, bh, bar_w * 0.5) {
            let colour = sample_ramp(theme.spectrum, p);
            fill_clipped(pixmap, ctx, &path, ctx.c(colour.fade(alpha)), clip);
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
/// duration. Drives the orb-to-ribbon width morph.
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
            ..
        } = self;

        pixmap.fill(Color::TRANSPARENT);
        let presence = model.presence();
        if presence <= 0.001 {
            return &self.pixmap;
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

        // The timer shares the closed-state glyph's crossfade: it fades out
        // exactly as live text fades in, so the two never fight over the
        // same right-aligned zone. See `draw_wave`'s `right_reserve`.
        let glyph_a = glyph_alpha(open);
        let timer_text = format_timer(model.listening_ms());
        let timer_w = atlas.measure(&timer_text, layout.text_font, 0.0);
        let timer_zone = (layout.text_pad_x + timer_w + WAVE_TIMER_GAP * layout.scale) * glyph_a;

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
            cached.map(|m| &m.clip),
        );
        draw_glyph(pixmap, &ctx, glyph_a);
        draw_timer(pixmap, &ctx, atlas, x, y, w, h, glyph_a, &timer_text);
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
    /// written as `1.0 - inserted` (the core dot) or `1.0 - inserted * 0.9`
    /// (the wave row) *inverts* over the 90 ms after the inserted hold
    /// expires — the checkmark dissolved into a re-emerging mint dot and a
    /// full bar row while the shape faded out. Holding the last visible state
    /// at full weight leaves presence as the single thing that animates an
    /// exit, which is what it was always meant to be. Enter, and every
    /// visible-to-visible transition, are untouched.
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
/// wave row and the core dot holding while the glow, the core's colour or the
/// processing shimmer jump to their `Hidden` answers on the first exit frame.
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
/// most transparent, whether the ribbon is a closed orb or wide open with
/// text. Pulling the whole surface back toward opaque whenever there is text
/// would trade the glass away at exactly the moment it is most visible, so
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
        let halo_r = l.shape_h * 0.16 + l.shape_h * 0.22 * grow;
        let halo_a = 0.4 * (1.0 - grow) * listening * alpha;
        if let Some(p) = shapes::circle(cx, cy, halo_r) {
            fill(pixmap, ctx, &p, ctx.c(theme.rec.fade(halo_a)));
        }
    }

    let core_colour = core_colour(theme, model);
    let mut core_r = l.shape_h * 0.15;
    if listening > 0.0 {
        let t = (model.now_ms() % u64::from(REC_PULSE_MS)) as f32 / REC_PULSE_MS as f32;
        let pulse = if t < 0.7 {
            1.0 + 0.12 * (t / 0.7)
        } else {
            1.12 - 0.12 * ((t - 0.7) / 0.3)
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
        let radius = l.shape_h * 0.32;
        for (offset, colour) in [(-45.0, theme.spinner.0), (45.0, theme.spinner.1)] {
            stroke(
                pixmap,
                ctx,
                shapes::arc(cx, cy, radius, turn + offset, 90.0),
                ctx.c(colour.fade(processing * alpha)),
                (l.shape_h * 0.045).max(l.scale),
            );
        }
    }

    if inserted > 0.001 {
        if let Some((path, length)) = shapes::check_mark(cx, cy, l.shape_h * 0.6) {
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
                width: (l.shape_h * 0.09).max(l.scale),
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
/// live-text ribbon used, sharing the capsule with the wave row rather than
/// sitting under it — this design's second round rejected an under-pill
/// caption outright, and the fix here is not to resurrect that placement in
/// a new form. `alpha` is [`glyph_alpha`], the same crossfade the closed-state
/// core glyph uses, so the timer is what occupies this row's default (no
/// live text) state and steps aside the instant real words arrive.
///
/// Cascadia Mono is monospaced (`the_face_is_monospaced` pins this), so every
/// digit and the colon share one advance width: the run never reshuffles
/// itself as the seconds tick, only grows a character every ten minutes of
/// continuous listening.
///
/// Legibility is solved without a dark backing plate — the captain's
/// complaint this round is specifically that something black behind text
/// ruins the glass. Instead the run is drawn a second time, offset by a
/// sub-pixel in four directions at low alpha in the same ink colour before
/// the crisp full-alpha pass: a soft, colour-matched glow that thickens the
/// strokes rather than a plate that sits on top of the surface.
#[allow(clippy::too_many_arguments)]
fn draw_timer(
    pixmap: &mut Pixmap,
    ctx: &Ctx<'_>,
    atlas: &mut FontAtlas,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    alpha: f32,
    text: &str,
) {
    if alpha <= 0.001 {
        return;
    }
    let l = ctx.layout;
    let theme = ctx.theme;
    let a = ctx.alpha * alpha;

    let (tx, ty) = ctx.map(x + w - l.text_pad_x, y + h * 0.5);

    let halo_a = a * 0.35;
    if halo_a > 0.001 {
        let d = 0.6 * l.scale;
        for (ox, oy) in [(-d, 0.0), (d, 0.0), (0.0, -d), (0.0, d)] {
            atlas.draw(
                pixmap,
                text,
                l.text_font,
                0.0,
                tx + ox,
                ty + oy,
                Align::Right,
                TextPaint::Solid(theme.ink),
                halo_a,
            );
        }
    }
    atlas.draw(
        pixmap,
        text,
        l.text_font,
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
/// window, and the run with it. The top is held at or below the waveform
/// row's bottom edge, which
/// `the_wave_row_clears_the_live_text_ink_box` pins against the real glyphs.
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
    // bottom of the waveform row: a shape-height band reached up into the bars
    // and visibly darkened them along the whole length of the text.
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
    /// be clear of the wave row, the core glyph and its halo, all of which
    /// paint opaque colours over the glass.
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

    /// Round 3 of captain feedback: the resting shape (no live text — the
    /// shipped default) must read as a capsule, not the old true circle. This
    /// pins the actual drawn width to `layout.rest_w`, not `layout.shape_h`,
    /// which is the exact regression a stray `shape_h` left in `draw`'s width
    /// formula would reintroduce.
    #[test]
    fn the_resting_shape_is_the_capsule_width_not_a_circle() {
        let (r, _) = drive(&[Command::ShowListening], 400, PRISM_DARK);
        let l = r.layout();
        let (lo, hi) = lit_span_at(&r, l.center_y as u32, 30).expect("nothing drawn at centre row");
        let width = (hi - lo) as f32;
        assert!(
            width > l.shape_h * 1.5,
            "resting width {width} reads as a circle (shape_h {})",
            l.shape_h
        );
        // Within a shadow-blur's worth of the exact rest width — the shell
        // itself, not the ambient shadow bleeding a few px wider.
        assert!(
            (width - l.rest_w).abs() < 8.0,
            "resting width {width} is not close to layout.rest_w {}",
            l.rest_w
        );
    }

    /// How far above the row's centre the wave's own ink reaches, in device
    /// px, at the loudest bar in the frame.
    ///
    /// Measures *bar ink*, not "anything lit": the glass body fills the whole
    /// capsule interior at [`GLASS_FILL_ALPHA`] with the ambient shadow and
    /// state glow under it, so a low alpha threshold here is satisfied by
    /// every interior pixel before a single bar is drawn. Bars are painted at
    /// full alpha (`wave_alpha` is 1.0 while listening), so they are the only
    /// thing in the row that composites to near-opaque. The run has to be
    /// *contiguous* upward from the centre for the same reason: the lit top
    /// edge (`inner_highlight`) is a near-opaque hairline of its own along the
    /// top of the shape, and a plain topmost-hit scan would find it instead of
    /// a bar.
    fn wave_reach(r: &Renderer) -> u32 {
        let l = r.layout();
        let px = |x: u32, y: u32| r.pixmap().pixels()[(y * l.window_w + x) as usize];
        let cy = l.center_y as u32;
        // Every column of the row except the core glyph's own opaque circle at
        // dead centre (radius ~0.17 * shape_h with its pulse) and the timer's
        // zone off to the right — so this measures bars, never the dot or a
        // digit. The loudest bar is whichever one the wobble has up at this
        // instant, so all of them are considered.
        let lo = (l.center_x - l.rest_w * 0.5 + WAVE_INSET * l.scale) as u32;
        let hi = (l.center_x - l.shape_h * 0.25) as u32;
        let mut furthest = 0;
        for x in lo..hi {
            let mut dy = 0;
            while dy < (l.shape_h * 0.5) as u32 && px(x, cy.saturating_sub(dy)).alpha() > 200 {
                dy += 1;
            }
            furthest = furthest.max(dy.saturating_sub(1));
        }
        furthest
    }

    /// A visual review of the first cut of this composition found the wave
    /// row reading as a few flat, barely-visible ticks: it was still sized
    /// for its old job (a decoration sharing the shape with a wide text run),
    /// not the primary content of a shape with nothing else in it. This
    /// drives a sustained loud level and checks that some bar's ink actually
    /// reaches well away from the row's own centre — and, against a silent
    /// frame drawn the same way, that what is being measured is the bars
    /// responding to level at all rather than the glass underneath them.
    #[test]
    fn the_wave_row_has_real_presence_at_a_loud_sustained_level() {
        // Long enough for the one-pole level smoothing to settle.
        let (loud, _) = drive(
            &[Command::ShowListening, Command::Level(1.0)],
            600,
            PRISM_DARK,
        );
        let (silent, _) = drive(
            &[Command::ShowListening, Command::Level(0.0)],
            600,
            PRISM_DARK,
        );

        let reach = wave_reach(&loud);
        let quiet_reach = wave_reach(&silent);
        // The bar the flat-ticks cut actually drew: `_RIBBON`-sized, so its
        // *whole* height was `WAVE_MAX_H_RIBBON`. Reaching further than that
        // above the row's centre alone is what "real presence" means here.
        // Deliberately not a fraction of `WAVE_MAX_H_REST` — a threshold
        // derived from the constant under test shrinks with it and passes the
        // regression it exists to catch.
        let min_reach = (WAVE_MAX_H_RIBBON * loud.layout().scale) as u32;
        assert!(
            reach > min_reach,
            "loudest bar only reached {reach} px from the row's centre, wanted more than \
             {min_reach} (the whole height of the `_RIBBON`-sized row) — the row reads as \
             flat ticks again"
        );
        assert!(
            quiet_reach * 2 < reach,
            "a silent frame measured {quiet_reach} px against the loud frame's {reach} — this \
             is measuring the glass body, not the bars"
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

    /// The addendum's second requirement: an elapsed-recording timer shares
    /// the capsule with the wave row in the default presentation. This does
    /// not assert exact digits (that's `state::tests::timer_formats_like_a_stopwatch`
    /// and `the_timer_freezes_when_speech_ends`) — just that glyph ink is
    /// drawn in the right-aligned zone `draw_timer` owns, in `theme.ink` and
    /// never the near-black scrim.
    ///
    /// Colour is the whole of the assertion, deliberately. The zone sits
    /// inside the capsule, so the glass body lights every pixel in it at
    /// [`GLASS_FILL_ALPHA`] whether or not `draw_timer` runs at all; only the
    /// crisp ink pass composites to a near-opaque pixel carrying `theme.ink`'s
    /// own near-white RGB.
    #[test]
    fn the_timer_draws_ink_coloured_pixels_beside_the_waves() {
        let (r, _) = drive(
            &[Command::ShowListening, Command::Level(0.5)],
            1200,
            PRISM_DARK,
        );
        let l = r.layout();
        let ink = PRISM_DARK.ink;
        // The run is right-aligned at `x + w - text_pad_x`; this is the zone
        // from that edge back across the widest `m:ss` the face can produce,
        // clear of the shape's own near-black rim at the very edge.
        let right_edge = (l.center_x + l.rest_w * 0.5 - l.text_pad_x) as u32;
        let zone_start = right_edge.saturating_sub(48);
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

    /// The three things one listening frame has to get right at once, and the
    /// only one of them that used to be checked was the corner. The centre
    /// pixel is the live core dot, which is opaque `theme.rec`; the body
    /// around it is glass at [`GLASS_FILL_ALPHA`] and must stay well short of
    /// opaque, which is the whole point of the treatment and what a
    /// reintroduced text-driven opacity ramp would break.
    #[test]
    fn a_listening_orb_is_glass_around_an_opaque_core_and_clear_at_the_corners() {
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

    /// The same relationship, held at *every* point of the morph rather than
    /// only at its ends — which is where it actually broke.
    ///
    /// `the_wave_row_clears_the_live_text_ink_box` above checks the ribbon-end
    /// constants, and the row is only that small at `open == 1.0`; the scrim
    /// is already at full alpha from `HANDOFF_HI` (0.55). Pinning the band's
    /// ceiling to the ribbon-end numbers therefore painted it straight through
    /// the still-large bars for the whole handoff window — invisible to a test
    /// that only samples the endpoints, because the defect corrects itself
    /// once the tween settles. The bar envelope here is recomputed from
    /// `wave_geometry` the way `draw_wave` places its bars, not read back out
    /// of `text_band`'s own input.
    #[test]
    fn the_text_scrim_stays_below_the_wave_row_at_every_point_of_the_morph() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let l = Layout::new(scale);
            let atlas = FontAtlas::new();
            let y = l.center_y - l.shape_h * 0.5;
            for step in 0..=40 {
                let open = step as f32 / 40.0;
                if text_alpha(open) <= 0.0 {
                    continue;
                }
                // Exactly `draw_wave`'s tallest bar: centred `y_offset` above
                // the shape's centre, `max_h` tall at full deflection.
                let (max_h, y_offset) = wave_geometry(open);
                let cy = y + l.shape_h * 0.5 - y_offset * l.scale;
                let bars_bottom = cy + max_h * l.scale * 0.5;

                let (band_top, _) = text_band(&atlas, &l, y, l.shape_h, open);
                assert!(
                    band_top >= bars_bottom - 0.01,
                    "scale {scale}, open {open}: scrim starts at {band_top}, \
                     into a wave row reaching down to {bars_bottom}"
                );
            }
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
    /// its own. The bug this pins had the mint core dot and the whole wave row
    /// fading back *in* underneath it, because the state cross-fade kept
    /// running toward `Hidden` — which draws nothing — while presence was
    /// already carrying the exit.
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
            let listening = ctx.state_alpha(OverlayState::Listening);
            let processing = ctx.state_alpha(OverlayState::Processing);
            let inserted = ctx.state_alpha(OverlayState::Inserted);
            assert!(
                (inserted - 1.0).abs() < 1e-6,
                "at {t} ms the inserted weight had already decayed to {inserted}"
            );
            assert!(
                wave_alpha(listening, processing, inserted) <= 0.001,
                "at {t} ms the wave row came back at {}",
                wave_alpha(listening, processing, inserted)
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

    /// The same thing at the pixel level, and the reason it is worth two
    /// tests: the row of bars is drawn *over* the glass, so ink reappearing
    /// there shows up as the band getting more opaque even while the shape as
    /// a whole is fading out.
    #[test]
    fn the_wave_band_only_ever_fades_during_the_exit() {
        let mut model = Model::new(PRISM_DARK);
        model.tick(0);
        model.apply(Command::ShowListening);
        model.apply(Command::Level(1.0));
        let mut r = Renderer::new(1.0);
        let mut t = 0u64;
        while t < 400 {
            t += 16;
            model.tick(t);
            r.draw(&model);
        }
        model.apply(Command::Inserted { latency_ms: 142 });

        // Sampled in *shape-local* space and mapped through the same enter
        // transform the frame was drawn with: the exit slides the shape up by
        // 10 px and shrinks it to 90 %, so a fixed window rectangle would
        // watch different parts of the shape drift past and prove nothing.
        //
        // No text is ever sent, so `wave_alpha` is a deterministic 0 for the
        // whole monitored window: once `Inserted` is entered directly from
        // `Listening` (skipping `Processing`, as a streaming engine's final
        // does), its cross-fade completes long before `INSERTED_HOLD_MS`
        // expires, and [`Ctx::state_alpha`] freezes every state weight at
        // that value for the rest of the exit — so by the time this closure
        // is ever sampled (only once `state()` has become `Hidden`) there is
        // no live path left for wave ink to reappear on its own account. What
        // this still protects is the more general property the docstring
        // above describes: the whole row's worth of ink — shell, frozen
        // glow, frozen checkmark, all fading only through `presence` — must
        // never read as *more* opaque frame over frame while leaving. The
        // sampled rectangle is the row's *entire* footprint (not a thin
        // slice of it): large enough that the ±1 device px a continuously
        // moving `map()` can round a rectangle edge to does not itself swing
        // the sum by a meaningful fraction, which a narrower strip here was
        // found to do.
        let band_alpha = |r: &Renderer, model: &Model| -> u64 {
            let l = r.layout();
            let (dy, s) = model.enter_transform(l.scale);
            let oy = l.center_y + l.shape_h * 0.5;
            let map = |x: f32, y: f32| -> (u32, u32) {
                (
                    (l.center_x + (x - l.center_x) * s).round() as u32,
                    (oy + dy + (y - oy) * s).round() as u32,
                )
            };
            let row = l.center_y - WAVE_Y_OFFSET_REST * l.scale;
            let (x0, y0) = map(
                l.center_x - l.shape_h * 0.45,
                row - WAVE_MAX_H_REST * 0.5 * l.scale,
            );
            let (x1, y1) = map(
                l.center_x + l.shape_h * 0.45,
                row + WAVE_MAX_H_REST * 0.5 * l.scale,
            );
            let mut sum = 0u64;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    sum += u64::from(r.pixmap().pixels()[(y * l.window_w + x) as usize].alpha());
                }
            }
            sum
        };

        let mut previous = None;
        let mut exit_frames = 0;
        let gone_by = t + u64::from(INSERTED_HOLD_MS) + u64::from(EXIT_MS) + 200;
        while t < gone_by {
            t += 16;
            model.tick(t);
            r.draw(&model);
            if model.state().is_visible() {
                continue;
            }
            let now = band_alpha(&r, &model);
            if let Some(before) = previous {
                assert!(
                    now <= before,
                    "at {t} ms the wave band got denser as the pill left: {before} → {now}"
                );
            }
            previous = Some(now);
            exit_frames += 1;
        }
        assert!(exit_frames > 4, "only {exit_frames} exit frames sampled");
        assert_eq!(previous, Some(0), "the pill never finished leaving");
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
    fn an_open_ribbon_draws_more_than_the_closed_orb() {
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
}

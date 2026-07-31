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
use crate::state::{Model, OverlayState};
use crate::theme::{sample_ramp, Rgba, Theme};

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
const WAVE_MAX_H: f32 = 8.0;
const WAVE_Y_OFFSET: f32 = 10.0;
const WAVE_IDLE_FLOOR: f32 = 0.05;
const WAVE_PROCESSING_ENV: f32 = 0.16;
const WAVE_RESTING_ENV: f32 = 0.05;

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

/// The live waveform: a row of bars whose count and pitch are recomputed
/// from the shape's current width every frame, sitting in a band above the
/// shape's centre so it coexists with the text and the core glyph rather
/// than competing with either.
fn draw_wave(
    pixmap: &mut Pixmap,
    ctx: &Ctx<'_>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    clip: Option<&Mask>,
) {
    let l = ctx.layout;
    let theme = ctx.theme;
    let model = ctx.model;

    let listening = ctx.state_alpha(OverlayState::Listening);
    let processing = ctx.state_alpha(OverlayState::Processing);
    let inserted = ctx.state_alpha(OverlayState::Inserted);
    let alpha = (1.0 - inserted).max(listening + processing).min(1.0) * (1.0 - inserted * 0.9);
    if alpha <= 0.001 {
        return;
    }
    let env = model.level() * listening
        + WAVE_PROCESSING_ENV * processing
        + WAVE_RESTING_ENV * (1.0 - listening - processing).max(0.0);

    let inset = WAVE_INSET * l.scale;
    let usable = (w - 2.0 * inset).max(0.0);
    if usable <= 0.0 {
        return;
    }
    let pitch_target = WAVE_TARGET_PITCH * l.scale;
    let count = ((usable / pitch_target).round() as usize).clamp(WAVE_MIN_BARS, WAVE_MAX_BARS);
    let pitch = usable / count as f32;
    let bar_w = (pitch * WAVE_BAR_W_FRAC).max(l.scale * 0.75);

    let cy = y + h * 0.5 - WAVE_Y_OFFSET * l.scale;
    let max_h = WAVE_MAX_H * l.scale;
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
            fill_clipped(pixmap, ctx, &path, colour.fade(alpha), clip);
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
        let measured_w = layout.shape_h;
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
            let text_w = self.atlas.measure(model.text(), self.layout.text_font, 0.0);
            (text_w + 2.0 * self.layout.text_pad_x)
                .max(self.layout.shape_h)
                .min(self.layout.ribbon_max_w)
        } else {
            self.layout.shape_h
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
        let w = layout.shape_h + (*measured_w - layout.shape_h).max(0.0) * open;
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

        draw_shell(
            pixmap,
            &ctx,
            &shape,
            x,
            y,
            w,
            h,
            r,
            open,
            cached.map(|m| &m.clip),
        );
        draw_wave(pixmap, &ctx, x, y, w, h, cached.map(|m| &m.clip));
        draw_glyph(pixmap, &ctx, glyph_alpha(open));
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
        draw_caption(pixmap, atlas, &ctx);

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
    fn state_alpha(&self, state: OverlayState) -> f32 {
        fade_between(
            self.model.state() == state,
            self.model.previous_state() == state,
            self.model.cross(),
        )
    }

    /// Map a layout-space point through the enter transform.
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        let mut pts = [Point::from_xy(x, y)];
        self.xf.map_points(&mut pts);
        (pts[0].x, pts[0].y)
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

fn glow_colour(theme: &Theme, model: &Model) -> Rgba {
    let pick = |state: OverlayState| match state {
        OverlayState::Listening => theme.glow_listening,
        OverlayState::Inserted => theme.glow_inserted,
        _ => theme.glow_idle,
    };
    pick(model.previous_state()).lerp(pick(model.state()), model.cross())
}

// ---------------------------------------------------------------------------
// drawing steps
// ---------------------------------------------------------------------------

/// The shell: translucent glass body, a soft top sheen, two rings, and the
/// lit top edge.
///
/// **On the glass.** The overlay is already a per-pixel-alpha layered window,
/// so the body's fill carries alpha straight from `theme.shell_top`/
/// `shell_bottom` and the real desktop shows through underneath it — this is
/// ordinary alpha compositing, the same mechanism `UpdateLayeredWindow`
/// already does every frame, not a new capability. What this deliberately
/// does *not* do is sample or blur whatever is behind the window (acrylic /
/// Mica-style backdrop blur): a layered window does not get that behind-pixel
/// read for free, and faking it by, say, blurring a guess at the desktop
/// would be worse than not attempting it. The glass *impression* instead
/// comes from three honest ingredients: translucency (the fill alpha
/// itself), a soft directional sheen (`glass_sheen`, below, brighter at the
/// top like light catching a curved glass surface), and the existing rim
/// (`outer_ring`/`border`) plus the crisp `inner_highlight` line.
///
/// **On legibility.** Live text sits directly on this surface once the
/// ribbon opens, and it has to read over an arbitrary desktop, light or
/// dark. Rather than pick one fixed opacity that compromises between "glassy
/// at rest" and "legible with text", the fill's alpha is boosted smoothly as
/// `open` increases — reusing [`text_alpha`], the exact curve that fades the
/// live text in, so the backing gets more opaque in lockstep with there
/// being something that needs a backing. At rest (the quiet orb, `open` at
/// or near 0) the shell stays at its most transparent.
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
    open: f32,
    clip: Option<&Mask>,
) {
    let s = ctx.layout.scale;
    let legibility = text_alpha(open);
    let boosted = |c: Rgba| Rgba {
        a: (c.a + (1.0 - c.a) * 0.62 * legibility).min(1.0),
        ..c
    };

    let shader = LinearGradient::new(
        Point::from_xy(x + w * 0.5, y),
        Point::from_xy(x + w * 0.5, y + h),
        vec![
            GradientStop::new(0.0, ctx.c(boosted(ctx.theme.shell_top)).to_color()),
            GradientStop::new(1.0, ctx.c(boosted(ctx.theme.shell_bottom)).to_color()),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(ctx.c(boosted(ctx.theme.shell_top)).to_color()));
    let paint = Paint {
        shader,
        anti_alias: true,
        ..Paint::default()
    };
    pixmap.fill_path(shape, &paint, FillRule::Winding, ctx.xf, None);

    // Glass sheen: a soft wash brighter at the top, fading out by the
    // vertical midpoint — the cue that reads as "curved glass catching
    // light" rather than "flat tinted panel". Clipped to the shape itself
    // (reusing the same cached mask the shadow/glow already built this
    // frame) so it never bleeds past the rounded ends.
    let sheen = LinearGradient::new(
        Point::from_xy(x + w * 0.5, y),
        Point::from_xy(x + w * 0.5, y + h * 0.62),
        vec![
            GradientStop::new(0.0, ctx.c(ctx.theme.glass_sheen).to_color()),
            GradientStop::new(1.0, ctx.c(ctx.theme.glass_sheen.fade(0.0)).to_color()),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    );
    if let Some(shader) = sheen {
        let paint = Paint {
            shader,
            anti_alias: true,
            ..Paint::default()
        };
        pixmap.fill_path(shape, &paint, FillRule::Winding, ctx.xf, clip);
    }

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

    let core_colour = if model.state() == OverlayState::Processing {
        theme.accent
    } else {
        theme.rec
    };
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
            let progress = (model.age_ms() as f32 / CHECK_DRAW_MS as f32).clamp(0.0, 1.0);
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

    if ctx.model.state() == OverlayState::Processing {
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

/// The latency caption below the shape, insert-only.
fn draw_caption(pixmap: &mut Pixmap, atlas: &mut FontAtlas, ctx: &Ctx<'_>) {
    let l = ctx.layout;
    let theme = ctx.theme;
    let model = ctx.model;

    if model.state() != OverlayState::Inserted {
        return;
    }
    let Some(latency) = model.latency_ms() else {
        return;
    };
    let settle = 0.35 + 0.65 * model.cross();
    let (x, y) = ctx.map(l.center_x, l.caption_y);
    atlas.draw(
        pixmap,
        &format!("{latency} ms"),
        l.caption_font,
        0.0,
        x,
        y,
        Align::Center,
        TextPaint::Gradient(theme.latency.0, theme.latency.1),
        ctx.alpha * settle,
    );
}

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

    #[test]
    fn a_hidden_pill_writes_no_pixels() {
        let (r, model) = drive(&[], 200, PRISM_DARK);
        assert!(model.is_idle());
        assert_eq!(lit_pixels(r.pixmap()), 0, "hidden state left ink behind");
    }

    #[test]
    fn a_listening_orb_is_opaque_in_the_middle_and_clear_at_the_corners() {
        let (r, _) = drive(
            &[Command::ShowListening, Command::Level(0.8)],
            400,
            PRISM_DARK,
        );
        assert!(centre_alpha(&r) > 240, "shape body is not opaque");
        assert_eq!(
            r.pixmap().pixels()[0].alpha(),
            0,
            "corner is not transparent"
        );
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
        assert_eq!((r.pixmap().width(), r.pixmap().height()), (528, 122));
        r.set_scale(2.0);
        assert_eq!((r.pixmap().width(), r.pixmap().height()), (1056, 244));
        assert!((r.layout().scale - 2.0).abs() < f32::EPSILON);
        r.set_scale(2.0);
        assert_eq!((r.pixmap().width(), r.pixmap().height()), (1056, 244));
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

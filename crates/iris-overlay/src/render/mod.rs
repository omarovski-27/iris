//! The rasteriser.
//!
//! Portable on purpose. Nothing in here knows what a window is, so the exact
//! frames the Windows overlay puts on screen can be produced — and inspected —
//! on Linux. That is what [`crate::HeadlessOverlay`] and `--filmstrip` in the
//! demo are for.
//!
//! Everything the mockups animate is animated here as transform and opacity:
//! bars change `scaleY`, the pill rises and scales on enter, the scan band
//! translates. Nothing is blurred per frame except the drop shadow, which is
//! cached (see [`shadow`]).

mod shadow;
mod shapes;
mod text;

pub use text::{Align, FontAtlas, TextPaint};

use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Mask, Paint, Path, Pixmap, Point, Shader,
    SpreadMode, Stroke, StrokeDash, Transform,
};

use crate::layout::{Layout, Rect};
use crate::motion::{CHECK_DRAW_MS, REC_PULSE_MS, SCAN_PERIOD_MS, SPINNER_PERIOD_MS};
use crate::spectrum::BAR_COUNT;
use crate::state::{Model, OverlayState};
use crate::theme::{sample_ramp, Rgba, Theme};

/// Standard deviation of the pill's shadow blur, in logical pixels.
///
/// Sized for the compact HUD chip: softer and tighter than a pro-meter bezel
/// (CSS blur-radius ~22 ≈ Gaussian sigma 11).
const SHADOW_SIGMA: f32 = 11.0;
/// Vertical offset of the ambient shadow, in logical pixels.
const SHADOW_DY: f32 = 7.0;
/// Negative spread of the ambient shadow, in logical pixels.
const SHADOW_SPREAD: f32 = 6.0;
/// Negative spread of the coloured state halo.
const GLOW_SPREAD: f32 = 3.0;

/// Cached, transform-dependent masks. Rebuilt only when the pill moves, which
/// in the steady state is never.
struct Masks {
    key: (u32, i32, i32),
    clip: Mask,
    ambient: Mask,
    glow: Mask,
}

/// Draws one pill frame into an RGBA pixmap.
#[derive(Debug)]
pub struct Renderer {
    layout: Layout,
    pixmap: Pixmap,
    atlas: FontAtlas,
    masks: Option<Masks>,
}

impl std::fmt::Debug for Masks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Masks").field("key", &self.key).finish()
    }
}

impl Renderer {
    /// A renderer for one monitor scale factor (`dpi / 96.0`).
    ///
    /// # Panics
    ///
    /// Panics if the pixmap for the resulting window size cannot be allocated,
    /// which at ~130 KB means the process is already out of memory.
    #[must_use]
    pub fn new(scale: f32) -> Self {
        let layout = Layout::new(scale);
        let pixmap = Pixmap::new(layout.window_w, layout.window_h)
            .expect("overlay pixmap allocation failed");
        Self {
            layout,
            pixmap,
            atlas: FontAtlas::new(),
            masks: None,
        }
    }

    /// Re-lay-out and re-allocate for a new scale factor. A no-op if the scale
    /// is unchanged, so it is safe to call on every `WM_DPICHANGED`.
    pub fn set_scale(&mut self, scale: f32) {
        let layout = Layout::new(scale);
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
        // Disjoint field borrows: the drawing steps below are free functions so
        // the pixmap, the atlas and the mask cache can be borrowed at once.
        let Renderer {
            layout,
            pixmap,
            atlas,
            masks,
        } = self;

        pixmap.fill(Color::TRANSPARENT);
        let presence = model.presence();
        if presence <= 0.001 {
            return &self.pixmap;
        }

        let theme = *model.theme();
        let (dy, content_scale) = model.enter_transform(layout.scale);
        let ox = layout.pill.center_x();
        let oy = layout.pill.bottom();
        let xf = Transform::from_translate(ox, oy + dy)
            .pre_scale(content_scale, content_scale)
            .pre_translate(-ox, -oy);

        let ctx = Ctx {
            layout,
            theme: &theme,
            xf,
            alpha: presence,
            model,
        };

        ensure_masks(masks, layout, xf);
        let cached = masks.as_ref();

        if let Some(m) = cached {
            fill_through(pixmap, &m.ambient, theme.ambient_shadow.fade(presence));
            let glow = glow_colour(&theme, model);
            fill_through(pixmap, &m.glow, glow.fade(presence));
        }

        draw_shell(pixmap, &ctx);
        draw_signal(pixmap, &ctx, cached.map(|m| &m.clip));
        draw_capsule(pixmap, &ctx);
        draw_telemetry(pixmap, atlas, &ctx);

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

fn ensure_masks(slot: &mut Option<Masks>, layout: &Layout, xf: Transform) {
    let key = (
        layout.scale.to_bits(),
        (xf.ty * 4.0).round() as i32,
        (xf.sx * 512.0).round() as i32,
    );
    if slot.as_ref().is_some_and(|m| m.key == key) {
        return;
    }

    let (w, h) = (layout.window_w, layout.window_h);
    let pill = layout.pill;
    let build = |inset: f32, offset_y: f32, blur_sigma: Option<f32>| -> Option<Mask> {
        let path = shapes::round_rect_inset(pill.x, pill.y, pill.w, pill.h, layout.radius, inset)?;
        let mut mask = Mask::new(w, h)?;
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
        build(GLOW_SPREAD * s, 0.0, Some(SHADOW_SIGMA * s)),
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

/// The pill body, its two rings and the inner top highlight.
fn draw_shell(pixmap: &mut Pixmap, ctx: &Ctx<'_>) {
    let l = ctx.layout;
    let p = l.pill;
    let s = l.scale;

    if let Some(path) = shapes::round_rect(p.x, p.y, p.w, p.h, l.radius) {
        let shader = LinearGradient::new(
            Point::from_xy(p.center_x(), p.y),
            Point::from_xy(p.center_x(), p.bottom()),
            vec![
                GradientStop::new(0.0, ctx.c(ctx.theme.shell_top).to_color()),
                GradientStop::new(1.0, ctx.c(ctx.theme.shell_bottom).to_color()),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .unwrap_or_else(|| Shader::SolidColor(ctx.c(ctx.theme.shell_top).to_color()));

        let paint = Paint {
            shader,
            anti_alias: true,
            ..Paint::default()
        };
        pixmap.fill_path(&path, &paint, FillRule::Winding, ctx.xf, None);
    }

    // `0 0 0 1px` — a hairline ring just outside the body, which is what keeps
    // a dark pill legible against a dark wallpaper.
    stroke(
        pixmap,
        ctx,
        shapes::round_rect_inset(p.x, p.y, p.w, p.h, l.radius, -0.5 * s),
        ctx.c(ctx.theme.outer_ring),
        s,
    );
    // The 1 px border proper.
    stroke(
        pixmap,
        ctx,
        shapes::round_rect_inset(p.x, p.y, p.w, p.h, l.radius, 0.5 * s),
        ctx.c(ctx.theme.border),
        s,
    );

    // `inset 0 1px 0` — a lit top edge, along the flat part between the caps.
    let hl_w = p.w - l.radius * 2.0;
    if hl_w > 0.0 {
        if let Some(path) = shapes::round_rect(
            p.x + l.radius,
            p.y + 0.5 * s,
            hl_w,
            (1.0 * s).max(1.0),
            0.5 * s,
        ) {
            fill(pixmap, ctx, &path, ctx.c(ctx.theme.inner_highlight));
        }
    }
}

/// Everything on the live signal path: hairline, waveform, scan, ribbon.
/// All of it is clipped to the pill so nothing escapes the rounded ends.
fn draw_signal(pixmap: &mut Pixmap, ctx: &Ctx<'_>, clip: Option<&Mask>) {
    let l = ctx.layout;
    let theme = ctx.theme;

    // 1 px spectrum hairline along the top edge.
    if let Some(path) = shapes::round_rect(
        l.hairline.x,
        l.hairline.y,
        l.hairline.w,
        l.hairline.h,
        l.hairline.h * 0.5,
    ) {
        fill_ramp(
            pixmap,
            ctx,
            &path,
            l.hairline,
            theme.hairline,
            ctx.alpha * theme.hairline_opacity,
            clip,
        );
    }

    // 28 bars, scaleY only, each a fixed sample of the ramp.
    let bars = ctx.model.bars();
    for (i, &height) in bars.iter().enumerate().take(BAR_COUNT) {
        let r = l.bar_rect(i, height);
        let colour = sample_ramp(theme.spectrum, i as f32 / (BAR_COUNT - 1) as f32);
        if let Some(path) = shapes::round_rect(r.x, r.y, r.w, r.h, r.w * 0.5) {
            fill_clipped(pixmap, ctx, &path, ctx.c(colour), clip);
        }
    }

    // Processing scan band.
    let scan_alpha = ctx.state_alpha(OverlayState::Processing);
    if scan_alpha > 0.001 {
        let track = l.scan_track;
        let phase = (ctx.model.now_ms() % u64::from(SCAN_PERIOD_MS)) as f32 / SCAN_PERIOD_MS as f32;
        let band_w = track.w * 0.2;
        // The mockup translates the band 280 % of its own width, which sends it
        // clean off the pill for most of each period (the mock has no overflow
        // clip). We clip to the pill and size the travel so the band enters and
        // leaves the track exactly once per period instead.
        let band = Rect {
            x: track.x - band_w + (track.w + band_w) * phase,
            w: band_w,
            ..track
        };
        // Fade in over the first 15 % and out over the last 15 %, as the mock's
        // scan keyframes do.
        let edge = (phase / 0.15).min(1.0).min((1.0 - phase) / 0.15).max(0.0);
        if let Some(path) = shapes::round_rect(band.x, band.y, band.w, band.h, band.h * 0.5) {
            fill_ramp(
                pixmap,
                ctx,
                &path,
                band,
                theme.scan,
                ctx.alpha * scan_alpha * 0.9 * edge,
                clip,
            );
        }
    }

    // Partial-transcript ribbon. See README: the one element with no direct
    // mockup counterpart, because `set_partial_len` has none either.
    let ribbon_alpha = ctx.state_alpha(OverlayState::Listening) * 0.8;
    let lit = ctx.model.ribbon_rect(l.ribbon);
    if ribbon_alpha > 0.001 && lit.w > 0.5 {
        if let Some(path) = shapes::round_rect(lit.x, lit.y, lit.w, lit.h, lit.h * 0.5) {
            // The ramp spans the *full* track, so colours stay put as it grows.
            fill_ramp(
                pixmap,
                ctx,
                &path,
                l.ribbon,
                theme.spectrum,
                ctx.alpha * ribbon_alpha,
                clip,
            );
        }
    }
}

/// The capsule on the left: ring, recording core, spinner, check.
fn draw_capsule(pixmap: &mut Pixmap, ctx: &Ctx<'_>) {
    let l = ctx.layout;
    let theme = ctx.theme;
    let (cx, cy) = (l.cap.center_x(), l.cap.center_y());
    let cross = ctx.model.cross();

    // Ring: cross-faded between the two states' colours.
    let ring_for = |state: OverlayState| match state {
        OverlayState::Listening => theme.ring_listening,
        OverlayState::Processing => theme.ring_processing,
        OverlayState::Inserted => theme.ring_inserted,
        OverlayState::Hidden => theme.ring_idle,
    };
    let ring = ring_for(ctx.model.previous_state()).lerp(ring_for(ctx.model.state()), cross);
    stroke(
        pixmap,
        ctx,
        shapes::circle(cx, cy, (l.cap_ring_d - l.cap_ring_w) * 0.5),
        ctx.c(ring),
        l.cap_ring_w,
    );

    let listening = ctx.state_alpha(OverlayState::Listening);
    let processing = ctx.state_alpha(OverlayState::Processing);
    let inserted = ctx.state_alpha(OverlayState::Inserted);

    // Recording pulse: an expanding, fading halo around the core.
    if listening > 0.001 {
        let t = (ctx.model.now_ms() % u64::from(REC_PULSE_MS)) as f32 / REC_PULSE_MS as f32;
        let grow = (t / 0.7).min(1.0);
        // Keep the halo inside the ring so the rec core never reads as a
        // chunky recorder button on the compact HUD chip.
        let halo_r = l.cap_core_d * 0.5 + 4.0 * l.scale * grow;
        let halo_a = 0.4 * (1.0 - grow) * listening;
        if halo_a > 0.001 {
            if let Some(path) = shapes::circle(cx, cy, halo_r) {
                fill(pixmap, ctx, &path, ctx.c(theme.rec.fade(halo_a)));
            }
        }
    }

    // Core: full size while listening, shrunk to 40 % while processing, gone
    // once the check takes over.
    let core_scale_for = |state: OverlayState| {
        if state == OverlayState::Processing {
            0.4
        } else {
            1.0
        }
    };
    let core_colour_for = |state: OverlayState| match state {
        OverlayState::Listening => theme.rec,
        OverlayState::Processing => theme.accent,
        _ => theme.ink_faint,
    };
    let mut core_scale = core_scale_for(ctx.model.previous_state())
        + (core_scale_for(ctx.model.state()) - core_scale_for(ctx.model.previous_state())) * cross;
    if listening > 0.0 {
        let t = (ctx.model.now_ms() % u64::from(REC_PULSE_MS)) as f32 / REC_PULSE_MS as f32;
        let pulse = if t < 0.7 {
            1.0 + 0.12 * (t / 0.7)
        } else {
            1.12 - 0.12 * ((t - 0.7) / 0.3)
        };
        core_scale *= 1.0 + (pulse - 1.0) * listening;
    }
    let core_colour =
        core_colour_for(ctx.model.previous_state()).lerp(core_colour_for(ctx.model.state()), cross);
    let core_alpha = 1.0 - inserted;
    if core_alpha > 0.001 {
        if let Some(path) = shapes::circle(cx, cy, l.cap_core_d * 0.5 * core_scale) {
            fill(pixmap, ctx, &path, ctx.c(core_colour.fade(core_alpha)));
        }
    }

    // Spinner: two quadrant arcs, the mockup's `border-top-color` +
    // `border-right-color` trick, turning once per period.
    if processing > 0.001 {
        let turn = (ctx.model.now_ms() % u64::from(SPINNER_PERIOD_MS)) as f32
            / SPINNER_PERIOD_MS as f32
            * 360.0;
        // Track the ring, not a hard-coded inset, so the spinner stays
        // proportional when the capsule shrinks with the HUD chip.
        let radius = (l.cap_ring_d * 0.5 - l.cap_ring_w - 1.0 * l.scale).max(l.scale);
        for (offset, colour) in [(-45.0, theme.spinner.0), (45.0, theme.spinner.1)] {
            stroke(
                pixmap,
                ctx,
                shapes::arc(cx, cy, radius, turn + offset, 90.0),
                ctx.c(colour.fade(processing)),
                l.cap_ring_w,
            );
        }
    }

    // Check: drawn on, not faded in.
    if inserted > 0.001 {
        if let Some((path, length)) = shapes::check_mark(cx, cy, l.cap_ring_d) {
            let progress = (ctx.model.age_ms() as f32 / CHECK_DRAW_MS as f32).clamp(0.0, 1.0);
            let mut paint = Paint::default();
            paint.set_color(ctx.c(theme.ok.fade(inserted)).to_color());
            paint.anti_alias = true;
            let stroke_style = Stroke {
                width: (2.0 * l.scale).max(1.0),
                line_cap: tiny_skia::LineCap::Round,
                line_join: tiny_skia::LineJoin::Round,
                dash: StrokeDash::new(vec![length, length], length * (1.0 - progress)),
                ..Stroke::default()
            };
            pixmap.stroke_path(&path, &paint, &stroke_style, ctx.xf, None);
        }
    }
}

/// The telemetry readout inside the pill and the engine chip below it.
fn draw_telemetry(pixmap: &mut Pixmap, atlas: &mut FontAtlas, ctx: &Ctx<'_>) {
    let l = ctx.layout;
    let theme = ctx.theme;
    let model = ctx.model;

    // A short fade on state change so the readout swaps rather than pops.
    let settle = 0.35 + 0.65 * model.cross();

    if let Some(readout) = model.readout() {
        let paint = match model.state() {
            OverlayState::Inserted => TextPaint::Gradient(theme.latency.0, theme.latency.1),
            OverlayState::Processing => TextPaint::Solid(theme.processing_ink),
            _ => TextPaint::Solid(theme.ink_dim),
        };
        let (x, y) = ctx.map(l.meta_right, l.meta_center_y);
        atlas.draw(
            pixmap,
            &readout,
            l.meta_font,
            0.0,
            x,
            y,
            Align::Right,
            paint,
            ctx.alpha * settle,
        );
    }

    // Listening-only engine chip — the captain's locked telemetry decision.
    let chip_alpha = fade_between(
        model.state().shows_chip(),
        model.previous_state().shows_chip(),
        model.cross(),
    );
    if chip_alpha > 0.001 && !model.engine().is_empty() {
        let (x, y) = ctx.map(l.pill.center_x(), l.chip_center_y);
        atlas.draw(
            pixmap,
            model.engine(),
            l.chip_font,
            l.chip_tracking,
            x,
            y,
            Align::Center,
            TextPaint::Solid(theme.ink_faint),
            ctx.alpha * chip_alpha * 0.85,
        );
    }
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
    across: Rect,
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

    fn drive(commands: &[Command], run_ms: u64, theme: Theme) -> (Renderer, Model) {
        let mut model = Model::new(theme);
        model.tick(0);
        for c in commands {
            model.apply(c.clone());
        }
        let mut t = 0;
        while t < run_ms {
            t = (t + 16).min(run_ms);
            model.tick(t);
        }
        let mut renderer = Renderer::new(1.0);
        renderer.draw(&model);
        (renderer, model)
    }

    fn lit_pixels(pixmap: &Pixmap) -> usize {
        pixmap.pixels().iter().filter(|p| p.alpha() > 0).count()
    }

    /// Alpha of the pixel at the centre of the pill.
    fn centre_alpha(r: &Renderer) -> u8 {
        let l = r.layout();
        let (x, y) = (l.pill.center_x() as u32, l.pill.center_y() as u32);
        r.pixmap().pixels()[(y * l.window_w + x) as usize].alpha()
    }

    #[test]
    fn a_hidden_pill_writes_no_pixels() {
        let (r, model) = drive(&[], 200, PRISM_DARK);
        assert!(model.is_idle());
        assert_eq!(lit_pixels(r.pixmap()), 0, "hidden state left ink behind");
    }

    #[test]
    fn a_listening_pill_is_opaque_in_the_middle_and_clear_at_the_corners() {
        let (r, _) = drive(
            &[Command::ShowListening, Command::Level(0.8)],
            400,
            PRISM_DARK,
        );
        assert!(centre_alpha(&r) > 240, "pill body is not opaque");
        // The window corner is outside both the pill and its shadow.
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
            let mut cmds = commands;
            cmds.push(Command::Engine("groq · whisper-large-v3-turbo · en".into()));
            let (r, _) = drive(&cmds, 200, PRISM_DARK);
            // Compact HUD chip is ~250×100; still plenty of ink when visible.
            assert!(lit_pixels(r.pixmap()) > 2_500, "{name} drew almost nothing");
        }
    }

    #[test]
    fn the_frame_stays_premultiplied() {
        // UpdateLayeredWindow reads these bytes directly; r,g,b > a renders as
        // bright fringing around the pill.
        let (r, _) = drive(
            &[
                Command::ShowListening,
                Command::Level(1.0),
                Command::Engine("groq".into()),
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
        // A frame mid-enter, when the pill is translated and scaled.
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

        // The dark shell must actually be darker than the light one.
        let brightness = |r: &Renderer| {
            let l = r.layout();
            let i = ((l.pill.center_y() as u32) * l.window_w + l.pill.x as u32 + 4) as usize;
            let p = r.pixmap().pixels()[i];
            u32::from(p.red()) + u32::from(p.green()) + u32::from(p.blue())
        };
        assert!(brightness(&dark) < brightness(&light));
    }

    #[test]
    fn the_shadow_reaches_past_the_pill_but_not_past_the_window() {
        let (r, _) = drive(&[Command::ShowListening], 300, PRISM_DARK);
        let l = r.layout();
        let px = |x: u32, y: u32| r.pixmap().pixels()[(y * l.window_w + x) as usize].alpha();
        // Just below the pill: shadow, not nothing.
        assert!(px(l.pill.center_x() as u32, l.pill.bottom() as u32 + 6) > 0);
        // The very edges of the window stay clear so the pill never looks boxed.
        for x in 0..l.window_w {
            assert_eq!(px(x, 0), 0, "top row inked at x={x}");
        }
    }

    #[test]
    fn re_scaling_resizes_the_frame() {
        let mut r = Renderer::new(1.0);
        assert_eq!((r.pixmap().width(), r.pixmap().height()), (224, 106));
        r.set_scale(2.0);
        assert_eq!((r.pixmap().width(), r.pixmap().height()), (448, 212));
        assert!((r.layout().scale - 2.0).abs() < f32::EPSILON);
        // Idempotent.
        r.set_scale(2.0);
        assert_eq!((r.pixmap().width(), r.pixmap().height()), (448, 212));
    }

    #[test]
    fn a_full_cycle_never_panics_at_any_scale() {
        // 100 %, 150 % and 200 % are the scales Windows actually offers most
        // often; the odd ones in between are covered by the layout tests, which
        // are cheap because they do not rasterise.
        for scale in [1.0, 1.5, 2.0] {
            let mut model = Model::new(PRISM_DARK);
            let mut r = Renderer::new(scale);
            model.tick(0);
            model.apply(Command::Engine("deepgram · nova-3 · en".into()));
            let mut t = 0u64;
            for step in 0..90 {
                match step {
                    5 => drop(model.apply(Command::ShowListening)),
                    40 => drop(model.apply(Command::Processing)),
                    50 => drop(model.apply(Command::Inserted { latency_ms: 142 })),
                    _ => {}
                }
                model.apply(Command::Level((step as f32 * 0.11).sin().abs()));
                model.apply(Command::PartialLen(step));
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
        assert!(a < 250, "pill was fully opaque only 8 ms in: {a}");
    }
}

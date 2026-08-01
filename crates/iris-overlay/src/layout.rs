//! Geometry: the pill's logical measurements, and the DPI-scaled device-pixel
//! rectangles the renderer draws into.
//!
//! Every constant here is in *logical* pixels at 100 % scale. [`Layout::new`]
//! multiplies them by the monitor's scale factor, which is the whole of this
//! crate's per-monitor-V2 DPI story: nothing is rasterised at 96 dpi and
//! stretched, the pill is re-laid-out and re-rasterised at the monitor's real
//! scale.
//!
//! # The shape
//!
//! A capsule whose corner radius is always exactly half its height — there is
//! no separate "orb shape" and "ribbon shape", only one shape whose width
//! animates. Height never changes. [`ORB_D`] is the height, at every width.
//!
//! At rest — no live text on screen, which is the default and what most
//! users ever see — the shape sits at [`REST_W`], a capsule holding the wave
//! row and an elapsed-recording timer side by side (captain feedback, round
//! 3: "not just a dot or a circle, more like the pipe thing", and a follow-up
//! asking for a timer beside the waves). Live text, when the config opts back
//! in, still widens the same shape further, up to [`RIBBON_MAX_W`].
//!
//! The **window is fixed-size**, sized for the *widest* state
//! ([`RIBBON_MAX_W`]) up front — see `window/win32.rs`'s `Surface::present`,
//! which already hands a fresh size to `UpdateLayeredWindow` every frame
//! regardless. Only the shape drawn inside that window animates; the window
//! itself never needs to resize or reposition more often than a DPI or
//! monitor change already causes today. This is deliberate: it was the
//! lower-risk of two options considered (the other being a window that
//! resizes live with the ribbon), and the fixed transparent margin around a
//! narrow orb costs nothing extra to composite.
//!
//! Geometry is deliberately *not* a theme property — see [`crate::theme`].

// ---------------------------------------------------------------------------
// Logical constants (100 % scale)
// ---------------------------------------------------------------------------

/// The shape's constant height, at every width. Unchanged across all three
/// rounds of feedback on this surface — see the module doc's "The shape".
pub const ORB_D: f32 = 34.0;
/// Width of the shape at rest: no live text on screen, just the wave row and
/// the elapsed-recording timer sharing the surface. Noticeably narrower than
/// the previous signed-off pill's 168×34 (captain, round 3: "it was really
/// wide, we need to narrow it down"), but wide enough relative to [`ORB_D`]'s
/// height to read unmistakably as a capsule, and wide enough to give the
/// timer its own zone beside the wave row without crowding either — see
/// `render/mod.rs`'s `draw_wave` (`right_reserve`) and `draw_timer` for how
/// the width splits between the two. Treated as a direction, not a number to
/// hit exactly (round-3 addendum): a composition that needs a few more pixels
/// to read well is worth more than a cramped one at a rounder number.
pub const REST_W: f32 = 128.0;
/// Widest the ribbon grows before new words start scrolling the oldest ones
/// off the leading edge instead of growing further.
pub const RIBBON_MAX_W: f32 = 460.0;
/// Inner padding each side of the live-text run.
pub const RIBBON_PAD_X: f32 = 18.0;
/// Live-text font size.
pub const TEXT_FONT: f32 = 15.0;

/// Transparent margin around the shape, every side, for the drop shadow and
/// state glow to bleed into.
///
/// The glow is a Gaussian of sigma 14 (see `render/mod.rs`), whose tail is
/// still faintly non-zero two-plus standard deviations out. This margin is
/// intentionally sized past that so the halo never reads as a hard-edged
/// rectangle against a dark wallpaper.
pub const MARGIN: f32 = 34.0;

/// Distance from the bottom of the shape to the bottom of the monitor's work
/// area. Unchanged from the previous pill design on purpose: this direction
/// changes the shape, not where the eye has to look for it.
pub const WORK_AREA_GAP: f32 = 58.0;

/// Overall window width in logical pixels: the widest ribbon state, plus
/// shadow margins on both sides.
pub const WINDOW_W: f32 = RIBBON_MAX_W + 2.0 * MARGIN;
/// Overall window height in logical pixels: the shape plus shadow margins
/// above and below. No caption row — see `render/mod.rs`'s module doc for
/// why there is no text anywhere near the shape but inside it.
pub const WINDOW_H: f32 = MARGIN + ORB_D + MARGIN;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// An axis-aligned rectangle in device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// Right edge.
    #[must_use]
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    /// Bottom edge.
    #[must_use]
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    /// Horizontal centre.
    #[must_use]
    pub fn center_x(&self) -> f32 {
        self.x + self.w * 0.5
    }
    /// Vertical centre.
    #[must_use]
    pub fn center_y(&self) -> f32 {
        self.y + self.h * 0.5
    }
}

/// A monitor's work area — the desktop minus the taskbar — in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkArea {
    /// Left edge, in virtual-desktop coordinates.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge, exclusive.
    pub right: i32,
    /// Bottom edge, exclusive.
    pub bottom: i32,
}

impl WorkArea {
    /// Width in physical pixels.
    #[must_use]
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    /// Height in physical pixels.
    #[must_use]
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// Where the overlay window goes, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Window left edge.
    pub x: i32,
    /// Window top edge.
    pub y: i32,
    /// Window width.
    pub width: u32,
    /// Window height.
    pub height: u32,
}

impl Placement {
    /// Bottom-centre of `work`, with the *shape's* bottom edge exactly
    /// [`WORK_AREA_GAP`] scaled pixels above the bottom of the work area.
    ///
    /// The window is taller than the shape (shadow margin above and below),
    /// so this is not simply "window bottom minus 58".
    /// The window is fixed-size (see the module doc), so this never needs to
    /// be recomputed more often than a DPI or monitor change already causes.
    #[must_use]
    pub fn compute(work: WorkArea, scale: f32) -> Self {
        let scale = sane_scale(scale);
        let width = (WINDOW_W * scale).round().max(1.0) as u32;
        let height = (WINDOW_H * scale).round().max(1.0) as u32;

        // Everything between the window's top edge and the shape's bottom edge.
        let above_shape_bottom = ((MARGIN + ORB_D) * scale).round() as i32;
        let gap = (WORK_AREA_GAP * scale).round() as i32;

        let x = work.left + (work.width() - width as i32) / 2;
        let y = work.bottom - gap - above_shape_bottom;
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Clamp a scale factor to something a monitor could plausibly report.
///
/// Windows reports 100–500 % in practice; a zero or NaN here would produce a
/// zero-sized window and a division by zero downstream.
fn sane_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(0.5, 8.0)
    } else {
        1.0
    }
}

/// Every rectangle the renderer needs, in device pixels, for one scale factor.
///
/// Recomputed on `WM_DPICHANGED`; never interpolated. Unlike the previous
/// pill layout, this holds no shape-specific rectangles beyond the anchor
/// point and font sizes — the shape's actual width is animation state that
/// lives on the `Renderer`, not geometry that lives here.
#[derive(Clone, Debug)]
pub struct Layout {
    /// The scale factor this layout was built for (`dpi / 96`).
    pub scale: f32,
    /// Window width in device pixels.
    pub window_w: u32,
    /// Window height in device pixels.
    pub window_h: u32,
    /// Horizontal centre of the shape, in window-local device pixels. Every
    /// width the shape ever takes is centred on this.
    pub center_x: f32,
    /// Vertical centre of the shape.
    pub center_y: f32,
    /// The shape's constant height, in device pixels.
    pub shape_h: f32,
    /// Width of the shape at rest (no live text), in device pixels.
    pub rest_w: f32,
    /// Widest the ribbon grows, in device pixels.
    pub ribbon_max_w: f32,
    /// Inner padding each side of the live-text run.
    pub text_pad_x: f32,
    /// Live-text font size, in device pixels.
    pub text_font: f32,
}

impl Layout {
    /// Build a layout for a monitor scale factor (`dpi / 96.0`).
    #[must_use]
    pub fn new(scale: f32) -> Self {
        let s = sane_scale(scale);
        let px = |v: f32| v * s;

        let window_w = (WINDOW_W * s).round().max(1.0) as u32;
        let window_h = (WINDOW_H * s).round().max(1.0) as u32;

        Self {
            scale: s,
            window_w,
            window_h,
            center_x: window_w as f32 * 0.5,
            center_y: px(MARGIN) + px(ORB_D) * 0.5,
            shape_h: px(ORB_D),
            rest_w: px(REST_W),
            ribbon_max_w: px(RIBBON_MAX_W),
            text_pad_x: px(RIBBON_PAD_X),
            text_font: px(TEXT_FONT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_geometry_matches_the_spec() {
        assert_eq!(ORB_D, 34.0);
        assert_eq!(RIBBON_MAX_W, 460.0);
        assert_eq!(WORK_AREA_GAP, 58.0, "placement anchor stays unchanged");
    }

    /// The captain's complaint was specifically about width ("it was really
    /// wide, we need to narrow it down"), not about the shape reading as a
    /// circle instead of a capsule — the rest width has to sit strictly
    /// between those two failure modes.
    #[test]
    fn rest_width_is_a_capsule_narrower_than_the_old_signed_off_pill() {
        const OLD_SIGNED_OFF_PILL_W: f32 = 168.0;
        const { assert!(REST_W < OLD_SIGNED_OFF_PILL_W) };
        const { assert!(REST_W > ORB_D * 1.5) };
    }

    #[test]
    fn window_is_bigger_than_the_widest_shape() {
        let l = Layout::new(1.0);
        assert_eq!((l.window_w, l.window_h), (528, 102));
        assert!(l.center_x > 0.0 && l.center_y > 0.0);
        assert!(l.center_x < l.window_w as f32);
        assert!(l.ribbon_max_w + 2.0 * MARGIN <= l.window_w as f32 + 0.5);
        assert!(
            l.rest_w < l.ribbon_max_w,
            "rest width must fit inside the window too"
        );
    }

    /// No caption means no reason for the shape to sit off-centre vertically
    /// any more — unlike the previous layout (shadow above, shape, caption,
    /// shadow below), the margin above and below the shape is now identical.
    #[test]
    fn the_shape_is_vertically_centred_in_the_window_with_no_caption_left_over() {
        let l = Layout::new(1.0);
        let above = l.center_y - l.shape_h * 0.5;
        let below = l.window_h as f32 - (l.center_y + l.shape_h * 0.5);
        assert!((above - below).abs() < 0.01, "above {above}, below {below}");
    }

    #[test]
    fn geometry_scales_linearly_with_dpi() {
        let one = Layout::new(1.0);
        let two = Layout::new(2.0);
        assert_eq!(two.window_w, one.window_w * 2);
        assert_eq!(two.window_h, one.window_h * 2);
        assert!((two.shape_h - one.shape_h * 2.0).abs() < 0.01);
        assert!((two.rest_w - one.rest_w * 2.0).abs() < 0.01);
        assert!((two.ribbon_max_w - one.ribbon_max_w * 2.0).abs() < 0.01);
        assert!((two.text_font - one.text_font * 2.0).abs() < 0.01);
    }

    #[test]
    fn absurd_scales_do_not_produce_absurd_windows() {
        for s in [0.0, -4.0, f32::NAN, f32::INFINITY, 1e9] {
            let l = Layout::new(s);
            assert!(l.window_w > 0 && l.window_h > 0, "scale {s}");
            assert!(l.scale.is_finite() && l.scale > 0.0, "scale {s}");
        }
    }

    #[test]
    fn placement_puts_the_shape_58_logical_px_above_the_work_area() {
        let work = WorkArea {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let p = Placement::compute(work, scale);
            let l = Layout::new(scale);
            let shape_bottom = p.y as f32 + l.center_y + l.shape_h * 0.5;
            let gap = work.bottom as f32 - shape_bottom;
            assert!(
                (gap - WORK_AREA_GAP * scale).abs() <= 1.0,
                "scale {scale}: gap {gap}, want {}",
                WORK_AREA_GAP * scale
            );
        }
    }

    #[test]
    fn placement_is_horizontally_centred() {
        let work = WorkArea {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let p = Placement::compute(work, 1.0);
        let left_margin = p.x - work.left;
        let right_margin = work.right - (p.x + p.width as i32);
        assert!(
            (left_margin - right_margin).abs() <= 1,
            "{left_margin} vs {right_margin}"
        );
    }

    #[test]
    fn placement_follows_a_secondary_monitor_origin() {
        // A monitor to the left of the primary has negative coordinates; the
        // pill must land on *that* monitor, not near the origin.
        let work = WorkArea {
            left: -2560,
            top: -200,
            right: -640,
            bottom: 880,
        };
        let p = Placement::compute(work, 1.0);
        assert!(p.x >= work.left && p.x + p.width as i32 <= work.right);
        assert!(p.y > work.top && p.y < work.bottom);
        let l = Layout::new(1.0);
        let gap = work.bottom as f32 - (p.y as f32 + l.center_y + l.shape_h * 0.5);
        assert!((gap - WORK_AREA_GAP).abs() <= 1.0, "gap {gap}");
    }

    #[test]
    fn placement_survives_a_taskbar_on_top() {
        // Work area top offset by a top-docked taskbar.
        let work = WorkArea {
            left: 0,
            top: 48,
            right: 1920,
            bottom: 1080,
        };
        let p = Placement::compute(work, 1.0);
        assert!(p.y > work.top);
        assert!(p.y + p.height as i32 <= work.bottom + 60);
    }
}

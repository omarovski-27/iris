//! Geometry: the pill's logical measurements, and the DPI-scaled device-pixel
//! rectangles the renderer draws into.
//!
//! Every constant here is in *logical* pixels at 100 % scale. The locked Prism
//! placement is still bottom-centre, 58 px above the work area; the body itself
//! is a compact HUD chip (`168 × 34`, radius 17) rather than the original mockup
//! recorder bar (`248 × 46`). [`Layout::new`] multiplies them by the monitor's
//! scale factor, which is the whole of this crate's per-monitor-V2 DPI story:
//! nothing is rasterised at 96 dpi and stretched, the pill is re-laid-out and
//! re-rasterised at the monitor's real scale.
//!
//! Geometry is deliberately *not* a theme property — see [`crate::theme`].

use crate::spectrum::BAR_COUNT;

// ---------------------------------------------------------------------------
// Logical constants (100 % scale)
// ---------------------------------------------------------------------------

/// Pill width.
pub const PILL_W: f32 = 168.0;
/// Pill height.
pub const PILL_H: f32 = 34.0;
/// Pill corner radius. Exactly half the height, so the ends are true semicircles.
pub const PILL_RADIUS: f32 = 17.0;

/// Distance from the bottom of the pill to the bottom of the monitor's work
/// area. Above the taskbar, below anything the user is reading.
pub const WORK_AREA_GAP: f32 = 58.0;

/// Transparent margin left and right of the pill, inside the window, for the
/// drop shadow to bleed into.
///
/// The shadow is a Gaussian of sigma 11 (CSS blur-radius ~22), whose tail is
/// still faintly non-zero two standard deviations out. Anything less than
/// ~2.5 sigma of margin and the halo is cut off square at the window edge,
/// which on a dark wallpaper reads as a visible rectangle around the pill.
pub const MARGIN_X: f32 = 28.0;
/// Transparent margin above the pill.
pub const MARGIN_TOP: f32 = 28.0;
/// Transparent margin below the engine chip.
pub const MARGIN_BOTTOM: f32 = 28.0;

/// Gap between the bottom of the pill and the top of the engine chip.
pub const CHIP_GAP: f32 = 7.0;
/// Height of the engine chip's text box.
pub const CHIP_H: f32 = 9.0;

/// Left padding inside the pill, before the capsule.
const PAD_L: f32 = 5.0;
/// The capsule's square box.
const CAP_BOX: f32 = 18.0;
/// Gap between the capsule box and the waveform.
const CAP_GAP: f32 = 2.0;
/// Diameter of the capsule ring.
const CAP_RING_D: f32 = 14.0;
/// Stroke width of the capsule ring.
const CAP_RING_W: f32 = 1.2;
/// Diameter of the capsule core (live indicator — not a rec-button red).
const CAP_CORE_D: f32 = 5.0;

/// Width reserved on the right for the telemetry readout, including the pill's
/// own right padding. Fixed, so the waveform never reflows when the readout
/// changes from `0:03` to `142 ms` — the report forbids layout thrash.
const META_SLOT: f32 = 42.0;
/// Distance from the pill's right edge to the right edge of the readout text.
const META_PAD_R: f32 = 10.0;

/// Height of the waveform box.
const WAVE_H: f32 = 18.0;
/// Inner padding at each end of the waveform box.
const WAVE_PAD: f32 = 4.0;
/// Width of one bar.
const BAR_W: f32 = 1.5;
/// Gap between bars.
const BAR_GAP: f32 = 1.6;
/// Height of a bar at `scaleY(1)`.
const BAR_H: f32 = 16.0;

/// Inset of the spectrum hairline from each end of the pill.
const HAIRLINE_INSET: f32 = 10.0;
/// Thickness of the spectrum hairline.
const HAIRLINE_H: f32 = 1.0;

/// Left edge of the processing scan track.
const SCAN_L: f32 = 26.0;
/// Distance from the pill's right edge to the right edge of the scan track.
const SCAN_R: f32 = 46.0;
/// Thickness of the scan band.
const SCAN_H: f32 = 1.5;

/// Distance from the pill's bottom edge to the partial-transcript ribbon.
const RIBBON_UP: f32 = 4.0;
/// Thickness of the partial-transcript ribbon.
const RIBBON_H: f32 = 1.25;

/// Font size of the telemetry readout.
const META_FONT: f32 = 10.0;
/// Font size of the engine chip.
const CHIP_FONT: f32 = 9.0;
/// Letter-spacing of the engine chip, in em.
const CHIP_TRACKING_EM: f32 = 0.08;

/// Overall window width in logical pixels: pill plus shadow margins.
pub const WINDOW_W: f32 = PILL_W + 2.0 * MARGIN_X;
/// Overall window height in logical pixels: pill, chip, and shadow margins.
pub const WINDOW_H: f32 = MARGIN_TOP + PILL_H + CHIP_GAP + CHIP_H + MARGIN_BOTTOM;

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
    /// Bottom-centre of `work`, with the *pill's* bottom edge exactly
    /// [`WORK_AREA_GAP`] scaled pixels above the bottom of the work area.
    ///
    /// The window is taller than the pill (shadow margin above, chip and shadow
    /// margin below), so this is not simply "window bottom minus 58".
    #[must_use]
    pub fn compute(work: WorkArea, scale: f32) -> Self {
        let scale = sane_scale(scale);
        let width = (WINDOW_W * scale).round().max(1.0) as u32;
        let height = (WINDOW_H * scale).round().max(1.0) as u32;

        // Everything between the window's top edge and the pill's bottom edge.
        let above_pill_bottom = ((MARGIN_TOP + PILL_H) * scale).round() as i32;
        let gap = (WORK_AREA_GAP * scale).round() as i32;

        let x = work.left + (work.width() - width as i32) / 2;
        let y = work.bottom - gap - above_pill_bottom;
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
/// Recomputed on `WM_DPICHANGED`; never interpolated.
#[derive(Clone, Debug)]
pub struct Layout {
    /// The scale factor this layout was built for (`dpi / 96`).
    pub scale: f32,
    /// Window width in device pixels.
    pub window_w: u32,
    /// Window height in device pixels.
    pub window_h: u32,
    /// The pill body.
    pub pill: Rect,
    /// The pill's corner radius.
    pub radius: f32,
    /// The capsule's square box on the left of the pill.
    pub cap: Rect,
    /// Diameter of the capsule ring.
    pub cap_ring_d: f32,
    /// Stroke width of the capsule ring.
    pub cap_ring_w: f32,
    /// Diameter of the capsule core.
    pub cap_core_d: f32,
    /// The waveform box between the capsule and the telemetry readout.
    pub wave: Rect,
    /// Height of a bar at full scale.
    pub bar_h: f32,
    /// Width of one bar.
    pub bar_w: f32,
    /// Pitch between bar left edges.
    pub bar_pitch: f32,
    /// Left edge of the first bar.
    pub bar_x0: f32,
    /// Right edge of the telemetry readout text.
    pub meta_right: f32,
    /// Vertical centre of the telemetry readout text.
    pub meta_center_y: f32,
    /// Font size of the telemetry readout, in device pixels.
    pub meta_font: f32,
    /// Vertical centre of the engine chip text.
    pub chip_center_y: f32,
    /// Font size of the engine chip, in device pixels.
    pub chip_font: f32,
    /// Letter-spacing of the engine chip, in device pixels.
    pub chip_tracking: f32,
    /// The 1 px spectrum hairline along the pill's top edge.
    pub hairline: Rect,
    /// The track the processing scan band sweeps along.
    pub scan_track: Rect,
    /// Thickness of the scan band.
    pub scan_h: f32,
    /// The partial-transcript ribbon at full extension.
    pub ribbon: Rect,
}

impl Layout {
    /// Build a layout for a monitor scale factor (`dpi / 96.0`).
    #[must_use]
    pub fn new(scale: f32) -> Self {
        let s = sane_scale(scale);
        let px = |v: f32| v * s;

        let window_w = (WINDOW_W * s).round().max(1.0) as u32;
        let window_h = (WINDOW_H * s).round().max(1.0) as u32;

        let pill = Rect {
            x: px(MARGIN_X),
            y: px(MARGIN_TOP),
            w: px(PILL_W),
            h: px(PILL_H),
        };

        let cap = Rect {
            x: pill.x + px(PAD_L),
            y: pill.center_y() - px(CAP_BOX) * 0.5,
            w: px(CAP_BOX),
            h: px(CAP_BOX),
        };

        let wave_left = pill.x + px(PAD_L + CAP_BOX + CAP_GAP);
        let wave_right = pill.right() - px(META_SLOT);
        let wave = Rect {
            x: wave_left,
            y: pill.center_y() - px(WAVE_H) * 0.5,
            w: wave_right - wave_left,
            h: px(WAVE_H),
        };

        // Bars are centred inside the waveform box's padded interior, matching
        // the mockup's `justify-content: center`.
        let bar_w = px(BAR_W);
        let bar_pitch = px(BAR_W + BAR_GAP);
        let bars_w = bar_pitch * (BAR_COUNT - 1) as f32 + bar_w;
        let bar_x0 = wave.x + px(WAVE_PAD) + ((wave.w - px(2.0 * WAVE_PAD)) - bars_w) * 0.5;

        let scan_l = pill.x + px(SCAN_L);
        let scan_r = pill.right() - px(SCAN_R);
        let scan_track = Rect {
            x: scan_l,
            y: pill.center_y() - px(SCAN_H) * 0.5,
            w: scan_r - scan_l,
            h: px(SCAN_H),
        };

        Self {
            scale: s,
            window_w,
            window_h,
            pill,
            radius: px(PILL_RADIUS),
            cap,
            cap_ring_d: px(CAP_RING_D),
            cap_ring_w: (px(CAP_RING_W)).max(1.0),
            cap_core_d: px(CAP_CORE_D),
            wave,
            bar_h: px(BAR_H),
            bar_w,
            bar_pitch,
            bar_x0,
            meta_right: pill.right() - px(META_PAD_R),
            meta_center_y: pill.center_y(),
            meta_font: px(META_FONT),
            chip_center_y: pill.bottom() + px(CHIP_GAP + CHIP_H * 0.5),
            chip_font: px(CHIP_FONT),
            chip_tracking: px(CHIP_FONT) * CHIP_TRACKING_EM,
            hairline: Rect {
                x: pill.x + px(HAIRLINE_INSET),
                y: pill.y,
                w: pill.w - px(2.0 * HAIRLINE_INSET),
                h: (px(HAIRLINE_H)).max(1.0),
            },
            scan_track,
            scan_h: (px(SCAN_H)).max(1.0),
            ribbon: Rect {
                x: bar_x0,
                y: pill.bottom() - px(RIBBON_UP),
                w: bars_w,
                h: (px(RIBBON_H)).max(1.0),
            },
        }
    }

    /// The rectangle for bar `index` at vertical scale `scale_y` (0.0–1.0).
    ///
    /// Bars grow from their vertical centre — `transform-origin: 50% 50%` in
    /// the mockup — so the row stays optically centred as it moves.
    ///
    /// # Panics
    ///
    /// Panics if `index >= BAR_COUNT`.
    #[must_use]
    pub fn bar_rect(&self, index: usize, scale_y: f32) -> Rect {
        assert!(index < BAR_COUNT, "bar index {index} out of range");
        let h = (self.bar_h * scale_y.clamp(0.0, 1.0)).max(self.scale.min(1.0));
        Rect {
            x: self.bar_x0 + self.bar_pitch * index as f32,
            y: self.wave.center_y() - h * 0.5,
            w: self.bar_w,
            h,
        }
    }

    /// Total width the bar row occupies.
    #[must_use]
    pub fn bars_width(&self) -> f32 {
        self.bar_pitch * (BAR_COUNT - 1) as f32 + self.bar_w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_geometry_is_the_hud_chip_spec() {
        // Compact HUD chip: smaller than the original Prism mockup bar so the
        // pill reads as status chrome, not a digital recorder strip.
        assert_eq!(PILL_W, 168.0);
        assert_eq!(PILL_H, 34.0);
        assert_eq!(PILL_RADIUS, 17.0);
        assert_eq!(WORK_AREA_GAP, 58.0);
        // Radius is exactly half the height: the ends are true semicircles.
        assert_eq!(PILL_RADIUS * 2.0, PILL_H);
        // Stay clearly smaller than the mockup recorder proportions.
        assert!(PILL_W < 180.0 && PILL_H < 40.0);
    }

    #[test]
    fn window_is_bigger_than_the_pill_it_contains() {
        let l = Layout::new(1.0);
        assert_eq!((l.window_w, l.window_h), (224, 106));
        assert!(l.pill.x > 0.0 && l.pill.y > 0.0);
        assert!(l.pill.right() < l.window_w as f32);
        // The chip lives below the pill and inside the window.
        assert!(l.chip_center_y > l.pill.bottom());
        assert!(l.chip_center_y < l.window_h as f32);
    }

    #[test]
    fn everything_stays_inside_the_pill() {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
            let l = Layout::new(scale);
            let inside = |r: Rect, what: &str| {
                assert!(r.x >= l.pill.x - 0.01, "{what} left at {scale}x");
                assert!(
                    r.right() <= l.pill.right() + 0.01,
                    "{what} right at {scale}x"
                );
                assert!(r.y >= l.pill.y - 0.01, "{what} top at {scale}x");
                assert!(
                    r.bottom() <= l.pill.bottom() + 0.01,
                    "{what} bottom at {scale}x"
                );
            };
            inside(l.cap, "capsule");
            inside(l.wave, "waveform");
            inside(l.hairline, "hairline");
            inside(l.scan_track, "scan track");
            inside(l.ribbon, "ribbon");
            inside(l.bar_rect(0, 1.0), "first bar");
            inside(l.bar_rect(BAR_COUNT - 1, 1.0), "last bar");
            assert!(l.meta_right <= l.pill.right(), "readout at {scale}x");
            assert!(
                l.meta_right > l.wave.right(),
                "readout overlaps wave at {scale}x"
            );
        }
    }

    #[test]
    fn the_bar_row_fits_its_padded_box() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let l = Layout::new(scale);
            let first = l.bar_rect(0, 1.0);
            let last = l.bar_rect(BAR_COUNT - 1, 1.0);
            assert!(first.x >= l.wave.x, "bars overflow left at {scale}x");
            assert!(
                last.right() <= l.wave.right(),
                "bars overflow right at {scale}x"
            );
            // Centred within the box.
            let lead = first.x - l.wave.x;
            let trail = l.wave.right() - last.right();
            assert!((lead - trail).abs() < 0.5, "bars off-centre at {scale}x");
        }
    }

    #[test]
    fn bars_grow_from_their_centre() {
        let l = Layout::new(1.0);
        let center = l.wave.center_y();
        for sy in [0.07, 0.5, 1.0] {
            let r = l.bar_rect(4, sy);
            assert!((r.center_y() - center).abs() < 0.01, "sy={sy}");
        }
        // Taller scaleY means a taller bar, monotonically.
        assert!(l.bar_rect(4, 1.0).h > l.bar_rect(4, 0.5).h);
        assert!(l.bar_rect(4, 0.5).h > l.bar_rect(4, 0.07).h);
        // Never zero-height: a bar that vanishes reads as a rendering bug.
        assert!(l.bar_rect(4, 0.0).h > 0.0);
    }

    #[test]
    fn geometry_scales_linearly_with_dpi() {
        let one = Layout::new(1.0);
        let two = Layout::new(2.0);
        assert_eq!(two.window_w, one.window_w * 2);
        assert_eq!(two.window_h, one.window_h * 2);
        assert!((two.pill.w - one.pill.w * 2.0).abs() < 0.01);
        assert!((two.radius - one.radius * 2.0).abs() < 0.01);
        assert!((two.bar_pitch - one.bar_pitch * 2.0).abs() < 0.01);
    }

    #[test]
    fn hairline_and_bars_never_disappear_at_low_scale() {
        // Sub-pixel strokes would round to nothing; they are floored at 1 px.
        let l = Layout::new(0.5);
        assert!(l.hairline.h >= 1.0);
        assert!(l.scan_h >= 1.0);
        assert!(l.ribbon.h >= 1.0);
        assert!(l.cap_ring_w >= 1.0);
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
    fn placement_puts_the_pill_58_logical_px_above_the_work_area() {
        let work = WorkArea {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let p = Placement::compute(work, scale);
            let l = Layout::new(scale);
            let pill_bottom = p.y as f32 + l.pill.bottom();
            let gap = work.bottom as f32 - pill_bottom;
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
        let gap = work.bottom as f32 - (p.y as f32 + l.pill.bottom());
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

//! Path construction helpers.
//!
//! tiny-skia ships rectangles, ovals and circles but no rounded rectangle and
//! no arc, and the pill is made of little else.

use std::f32::consts::PI;
use tiny_skia::{Path, PathBuilder};

/// Control-point ratio that makes a cubic Bézier approximate a quarter circle.
const KAPPA: f32 = 0.552_284_8;

/// A rounded rectangle. `r` is clamped to half the shorter side, so passing a
/// huge radius yields a capsule rather than a self-intersecting path.
#[must_use]
pub fn round_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    if !(w > 0.0 && h > 0.0) || !w.is_finite() || !h.is_finite() {
        return None;
    }
    let r = r.max(0.0).min(w * 0.5).min(h * 0.5);
    if r <= 0.0 {
        return PathBuilder::from_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?).into();
    }

    let c = r * KAPPA;
    let (x1, y1) = (x + w, y + h);
    let mut p = PathBuilder::new();
    p.move_to(x + r, y);
    p.line_to(x1 - r, y);
    p.cubic_to(x1 - r + c, y, x1, y + r - c, x1, y + r);
    p.line_to(x1, y1 - r);
    p.cubic_to(x1, y1 - r + c, x1 - r + c, y1, x1 - r, y1);
    p.line_to(x + r, y1);
    p.cubic_to(x + r - c, y1, x, y1 - r + c, x, y1 - r);
    p.line_to(x, y + r);
    p.cubic_to(x, y + r - c, x + r - c, y, x + r, y);
    p.close();
    p.finish()
}

/// A rounded rectangle inset by `inset` on every side, with its radius reduced
/// to match, so the inset shape stays concentric.
///
/// Used for the pill's inner border and for the drop shadow's negative spread.
#[must_use]
pub fn round_rect_inset(x: f32, y: f32, w: f32, h: f32, r: f32, inset: f32) -> Option<Path> {
    round_rect(
        x + inset,
        y + inset,
        w - inset * 2.0,
        h - inset * 2.0,
        r - inset,
    )
}

/// An open circular arc, for stroking.
///
/// Angles are in degrees, measured clockwise from 12 o'clock so they read the
/// same way as the CSS `border-top-color` / `border-right-color` spinner the
/// mockup builds.
#[must_use]
pub fn arc(cx: f32, cy: f32, radius: f32, start_deg: f32, sweep_deg: f32) -> Option<Path> {
    if !radius.is_finite() || radius <= 0.0 || sweep_deg == 0.0 {
        return None;
    }
    // Convert "clockwise from 12 o'clock" into standard maths angles.
    let to_rad = |deg: f32| (deg - 90.0) * PI / 180.0;
    let segments = ((sweep_deg.abs() / 90.0).ceil() as usize).max(1);
    let step = sweep_deg / segments as f32;

    let mut p = PathBuilder::new();
    let a0 = to_rad(start_deg);
    p.move_to(cx + radius * a0.cos(), cy + radius * a0.sin());

    for i in 0..segments {
        let a = to_rad(start_deg + step * i as f32);
        let b = to_rad(start_deg + step * (i + 1) as f32);
        let k = 4.0 / 3.0 * ((b - a) / 4.0).tan();

        let (p0x, p0y) = (cx + radius * a.cos(), cy + radius * a.sin());
        let (p3x, p3y) = (cx + radius * b.cos(), cy + radius * b.sin());
        p.cubic_to(
            p0x - k * radius * a.sin(),
            p0y + k * radius * a.cos(),
            p3x + k * radius * b.sin(),
            p3y - k * radius * b.cos(),
            p3x,
            p3y,
        );
    }
    p.finish()
}

/// A circle path.
#[must_use]
pub fn circle(cx: f32, cy: f32, radius: f32) -> Option<Path> {
    if !radius.is_finite() || radius <= 0.0 {
        return None;
    }
    let mut p = PathBuilder::new();
    p.push_circle(cx, cy, radius);
    p.finish()
}

/// The inserted check mark, authored in the mockup's 22x22 SVG viewBox and
/// scaled to `size`.
///
/// Returns the path and its total length, which the caller animates with a
/// stroke dash to make it draw itself.
#[must_use]
pub fn check_mark(cx: f32, cy: f32, size: f32) -> Option<(Path, f32)> {
    if !size.is_finite() || size <= 0.0 {
        return None;
    }
    // "M5.5 11.4 L9.2 15 L16.5 7.4" in a 22x22 box.
    const PTS: [(f32, f32); 3] = [(5.5, 11.4), (9.2, 15.0), (16.5, 7.4)];
    let s = size / 22.0;
    let map = |(x, y): (f32, f32)| (cx + (x - 11.0) * s, cy + (y - 11.0) * s);

    let mut p = PathBuilder::new();
    let start = map(PTS[0]);
    p.move_to(start.0, start.1);
    let mut length = 0.0;
    let mut prev = start;
    for pt in &PTS[1..] {
        let q = map(*pt);
        length += ((q.0 - prev.0).powi(2) + (q.1 - prev.1).powi(2)).sqrt();
        p.line_to(q.0, q.1);
        prev = q;
    }
    p.finish().map(|path| (path, length))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid_y(r: tiny_skia::Rect) -> f32 {
        (r.top() + r.bottom()) * 0.5
    }

    #[test]
    fn round_rect_bounds_are_exact() {
        let p = round_rect(10.0, 20.0, 100.0, 40.0, 20.0).unwrap();
        let b = p.bounds();
        assert!((b.left() - 10.0).abs() < 0.01);
        assert!((b.top() - 20.0).abs() < 0.01);
        assert!((b.right() - 110.0).abs() < 0.01);
        assert!((b.bottom() - 60.0).abs() < 0.01);
    }

    #[test]
    fn an_over_large_radius_becomes_a_capsule_not_a_knot() {
        let p = round_rect(0.0, 0.0, 100.0, 40.0, 900.0).unwrap();
        let b = p.bounds();
        assert!((b.right() - 100.0).abs() < 0.01);
        assert!((b.bottom() - 40.0).abs() < 0.01);
    }

    #[test]
    fn degenerate_shapes_are_none_not_panics() {
        assert!(round_rect(0.0, 0.0, 0.0, 10.0, 2.0).is_none());
        assert!(round_rect(0.0, 0.0, 10.0, -1.0, 2.0).is_none());
        assert!(round_rect(0.0, 0.0, f32::NAN, 10.0, 2.0).is_none());
        assert!(circle(0.0, 0.0, 0.0).is_none());
        assert!(arc(0.0, 0.0, 10.0, 0.0, 0.0).is_none());
        assert!(check_mark(0.0, 0.0, 0.0).is_none());
    }

    #[test]
    fn zero_radius_still_produces_a_rectangle() {
        let p = round_rect(0.0, 0.0, 10.0, 10.0, 0.0).unwrap();
        assert!((p.bounds().right() - 10.0).abs() < 0.01);
    }

    #[test]
    fn inset_shrinks_concentrically() {
        let outer = round_rect(0.0, 0.0, 100.0, 46.0, 23.0).unwrap();
        let inner = round_rect_inset(0.0, 0.0, 100.0, 46.0, 23.0, 4.0).unwrap();
        assert!((inner.bounds().left() - outer.bounds().left() - 4.0).abs() < 0.01);
        assert!((outer.bounds().right() - inner.bounds().right() - 4.0).abs() < 0.01);
        assert!(
            (mid_y(inner.bounds()) - mid_y(outer.bounds())).abs() < 0.01,
            "inset moved the centre"
        );
    }

    #[test]
    fn arcs_stay_on_their_circle() {
        // A quarter arc from 12 o'clock must span the top-right quadrant.
        let p = arc(50.0, 50.0, 10.0, 0.0, 90.0).unwrap();
        let b = p.bounds();
        assert!((b.top() - 40.0).abs() < 0.3, "top {}", b.top());
        assert!((b.right() - 60.0).abs() < 0.3, "right {}", b.right());
        assert!(b.left() >= 49.0, "left {}", b.left());
        assert!(b.bottom() <= 51.0, "bottom {}", b.bottom());
    }

    #[test]
    fn a_full_turn_covers_the_whole_circle() {
        let p = arc(0.0, 0.0, 10.0, 0.0, 360.0).unwrap();
        let b = p.bounds();
        assert!((b.left() + 10.0).abs() < 0.2);
        assert!((b.right() - 10.0).abs() < 0.2);
        assert!((b.top() + 10.0).abs() < 0.2);
        assert!((b.bottom() - 10.0).abs() < 0.2);
    }

    #[test]
    fn check_mark_is_centred_and_measured() {
        let (path, len) = check_mark(100.0, 100.0, 22.0).unwrap();
        // Authored length of the two segments, in viewBox units.
        assert!((len - 15.7).abs() < 0.2, "length {len}");
        let b = path.bounds();
        assert!(b.left() > 90.0 && b.right() < 110.0, "{b:?}");
        // Scaling scales the length with it.
        let (_, big) = check_mark(0.0, 0.0, 44.0).unwrap();
        assert!((big - len * 2.0).abs() < 0.1);
    }
}

//! Drop shadows, without a blur filter.
//!
//! The design rules forbid `backdrop-filter` and any GPU blur on the pill — the
//! report calls the coloured halo "a box-shadow colour, not a blur filter". What
//! is allowed, and what CSS `box-shadow` actually is, is a blurred copy of the
//! shape drawn underneath it. That is what this module does: rasterise the pill
//! into an 8-bit alpha mask, blur the mask on the CPU, and fill a colour through
//! it.
//!
//! Three box-blur passes approximate a Gaussian closely enough that no one can
//! tell at the overlay's 528×102 logical px window (1056×204 at 200 % DPI).
//!
//! Cost is per *rebuild*, not per frame: `render::ensure_masks` caches the
//! blurred masks and only rebuilds when the shape moves or its width crosses a
//! 4 device-px bucket. Under the fixed-width pill this module predates that
//! meant the enter and exit transforms only. The shape widens while the
//! transcript grows, so rebuilds recur for as long as words keep arriving —
//! two blurred masks over the full window each time. That only applies when
//! live text is opted in; in the default presentation the shape sits at
//! `layout::REST_W` for the whole session, so this is back to the enter and
//! exit transforms only. Coarsening the bucket would trade the growing case
//! against visible shadow lag during the morph; it is a deliberate follow-up,
//! not an oversight.

use tiny_skia::Mask;

/// Box-blur radius that best approximates a Gaussian of `sigma` in three passes.
///
/// From the standard "boxes for Gauss" derivation: an ideal box of width
/// `sqrt(12*sigma^2/n + 1)` over `n` passes matches the Gaussian's variance.
#[must_use]
pub fn radius_for_sigma(sigma: f32) -> usize {
    if !sigma.is_finite() || sigma <= 0.0 {
        return 0;
    }
    let ideal = (12.0 * sigma * sigma / 3.0 + 1.0).sqrt();
    (((ideal - 1.0) / 2.0).round().max(0.0) as usize).min(128)
}

/// Blur an alpha mask in place to approximate a Gaussian of `sigma`.
///
/// Areas outside the mask are treated as fully transparent, which is exactly
/// right for a shadow: there is no shape out there to bleed inwards.
pub fn blur(mask: &mut Mask, sigma: f32) {
    let radius = radius_for_sigma(sigma);
    if radius == 0 {
        return;
    }
    let (w, h) = (mask.width() as usize, mask.height() as usize);
    if w == 0 || h == 0 {
        return;
    }

    let data = mask.data_mut();
    let mut scratch = vec![0u8; w * h];
    for _ in 0..3 {
        box_blur_h(data, &mut scratch, w, h, radius);
        box_blur_v(&scratch, data, w, h, radius);
    }
}

fn box_blur_h(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    let divisor = (2 * r + 1) as u32;
    for y in 0..h {
        let row = &src[y * w..y * w + w];
        let out = &mut dst[y * w..y * w + w];
        let mut sum: u32 = 0;
        for &v in row.iter().take(r + 1).take(w) {
            sum += u32::from(v);
        }
        for x in 0..w {
            out[x] = (sum / divisor) as u8;
            if x >= r {
                sum -= u32::from(row[x - r]);
            }
            if x + r + 1 < w {
                sum += u32::from(row[x + r + 1]);
            }
        }
    }
}

fn box_blur_v(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    let divisor = (2 * r + 1) as u32;
    for x in 0..w {
        let mut sum: u32 = 0;
        for y in 0..=r.min(h.saturating_sub(1)) {
            sum += u32::from(src[y * w + x]);
        }
        for y in 0..h {
            dst[y * w + x] = (sum / divisor) as u8;
            if y >= r {
                sum -= u32::from(src[(y - r) * w + x]);
            }
            if y + r + 1 < h {
                sum += u32::from(src[(y + r + 1) * w + x]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::{FillRule, PathBuilder, Rect, Transform};

    fn square_mask(w: u32, h: u32, inset: f32) -> Mask {
        let mut mask = Mask::new(w, h).unwrap();
        let path = PathBuilder::from_rect(
            Rect::from_xywh(inset, inset, w as f32 - inset * 2.0, h as f32 - inset * 2.0).unwrap(),
        );
        mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
        mask
    }

    fn total(mask: &Mask) -> u64 {
        mask.data().iter().map(|&v| u64::from(v)).sum()
    }

    #[test]
    fn sigma_maps_to_a_sane_radius() {
        assert_eq!(radius_for_sigma(0.0), 0);
        assert_eq!(radius_for_sigma(-3.0), 0);
        assert_eq!(radius_for_sigma(f32::NAN), 0);
        // CSS blur-radius 28 is sigma 14; the classic result is a 13-14 px box.
        assert!(
            (13..=14).contains(&radius_for_sigma(14.0)),
            "{}",
            radius_for_sigma(14.0)
        );
        assert!(radius_for_sigma(1e9) <= 128, "unbounded radius");
    }

    #[test]
    fn blurring_spreads_the_shape_outwards() {
        let mut mask = square_mask(64, 64, 20.0);
        assert_eq!(mask.data()[0], 0, "corner should start empty");
        blur(&mut mask, 8.0);
        assert!(mask.data()[32 * 64 + 32] > 0, "centre lost");
        // A pixel just outside the original square now has some coverage.
        assert!(mask.data()[18 * 64 + 32] > 0, "shadow did not spread");
    }

    #[test]
    fn blurring_conserves_roughly_the_same_amount_of_ink() {
        // Zero-padded edges lose a little at the boundary, but a shape well
        // inside the mask must not gain or lose brightness wholesale.
        let mut mask = square_mask(128, 128, 40.0);
        let before = total(&mask);
        blur(&mut mask, 6.0);
        let after = total(&mask);
        let ratio = after as f64 / before as f64;
        assert!((0.9..=1.1).contains(&ratio), "ink ratio {ratio}");
    }

    #[test]
    fn a_zero_sigma_is_a_no_op() {
        let mut mask = square_mask(32, 32, 8.0);
        let before = mask.data().to_vec();
        blur(&mut mask, 0.0);
        assert_eq!(mask.data(), before.as_slice());
    }

    #[test]
    fn a_radius_larger_than_the_mask_does_not_panic() {
        let mut mask = square_mask(8, 8, 1.0);
        blur(&mut mask, 40.0);
        assert_eq!(mask.data().len(), 64);
    }

    #[test]
    fn an_empty_mask_stays_empty() {
        let mut mask = Mask::new(40, 40).unwrap();
        blur(&mut mask, 10.0);
        assert!(mask.data().iter().all(|&v| v == 0));
    }
}

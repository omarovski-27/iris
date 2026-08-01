//! Text for the ribbon's live transcript.
//!
//! Everything the pill draws is mono in the Prism mockup
//! (`--mono: "Cascadia Mono"`), so it needs exactly one face. Cascadia Mono is
//! the font the design spec names and it is SIL OFL 1.1, so it ships in-crate
//! rather than being looked up in the system font stack: the overlay then
//! rasterises identically on Windows and in the headless Linux tests, which is
//! what makes a rendered PNG a usable review artefact.
//!
//! See `assets/fonts/OFL.txt` for the licence.

use std::collections::HashMap;

use fontdue::{Font, FontSettings, Metrics};
use tiny_skia::{Pixmap, PremultipliedColorU8};

use crate::theme::Rgba;

/// Cascadia Mono Regular, SIL OFL 1.1. Copyright Microsoft Corporation.
const FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/CascadiaMono-Regular.ttf");

/// How the glyph coverage is coloured.
#[derive(Clone, Copy, Debug)]
pub enum TextPaint {
    /// One flat colour. The only variant the shipping design currently uses
    /// — the live ribbon text.
    Solid(Rgba),
    /// A left-to-right ramp across the whole run. Unused by the product
    /// since the latency caption that used it was removed (captain's second
    /// visual pass: "developer information on a user surface"), kept because
    /// it is a real capability of this general-purpose rasteriser, not
    /// design-specific dead weight — `render::text`'s own tests exercise it
    /// directly.
    #[allow(dead_code)]
    Gradient(Rgba, Rgba),
}

/// Horizontal alignment of a run against the x coordinate it is drawn at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    /// `x` is the centre of the run. Unused by the product for the same
    /// reason as [`TextPaint::Gradient`] — kept for the same reason.
    #[allow(dead_code)]
    Center,
    /// `x` is the right edge of the run — the live ribbon text, so its
    /// newest characters do not walk as the string grows.
    Right,
}

struct Glyph {
    metrics: Metrics,
    bitmap: Vec<u8>,
}

/// A font plus a rasterised-glyph cache.
///
/// The cache is keyed on character and eighth-of-a-pixel size, so a steady
/// state costs one hash lookup per glyph per frame and no rasterisation at all.
pub struct FontAtlas {
    font: Font,
    glyphs: HashMap<(char, u32), Glyph>,
}

impl std::fmt::Debug for FontAtlas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontAtlas")
            .field("cached_glyphs", &self.glyphs.len())
            .finish()
    }
}

impl FontAtlas {
    /// Load the embedded face.
    ///
    /// # Panics
    ///
    /// Panics if the embedded font fails to parse, which can only happen if the
    /// asset in this repository is corrupt — there is no runtime input here.
    #[must_use]
    pub fn new() -> Self {
        let font = Font::from_bytes(FONT_BYTES, FontSettings::default())
            .expect("embedded Cascadia Mono asset is corrupt");
        Self {
            font,
            glyphs: HashMap::new(),
        }
    }

    fn key(c: char, px: f32) -> (char, u32) {
        (c, (px.max(0.0) * 8.0).round() as u32)
    }

    fn ensure(&mut self, c: char, px: f32) -> (char, u32) {
        let key = Self::key(c, px);
        self.glyphs.entry(key).or_insert_with(|| {
            let (metrics, bitmap) = self.font.rasterize(c, px);
            Glyph { metrics, bitmap }
        });
        key
    }

    /// Advance width of one character, rasterising it into the cache first if
    /// it is not there yet.
    fn advance(&mut self, c: char, px: f32) -> f32 {
        let key = self.ensure(c, px);
        self.glyphs[&key].metrics.advance_width
    }

    /// Total advance width of `text`, including `tracking` between characters.
    pub fn measure(&mut self, text: &str, px: f32, tracking: f32) -> f32 {
        self.measure_capped(text, px, tracking, f32::INFINITY)
    }

    /// [`Self::measure`], but stopping as soon as the running width passes
    /// `budget` — the result is then only known to be *wider* than `budget`,
    /// which is all any caller here does with it.
    ///
    /// Every measurement in this crate is of the live transcript, which grows
    /// for as long as the user keeps speaking, and happens once or twice per
    /// frame. Both callers clamp to a width the ribbon cannot exceed, so
    /// measuring the part of the string past that clamp was pure waste that
    /// scaled with utterance length; this makes the per-frame cost depend on
    /// what is *shown* instead. Below the budget it is exactly `measure`, and
    /// `measure` is written in terms of it so the two cannot drift.
    pub fn measure_capped(&mut self, text: &str, px: f32, tracking: f32, budget: f32) -> f32 {
        let mut w = 0.0;
        for c in text.chars() {
            let key = self.ensure(c, px);
            w += self.glyphs[&key].metrics.advance_width + tracking;
            if w - tracking > budget {
                break;
            }
        }
        (w - tracking).max(0.0)
    }

    /// The face's own ascent and descent at `px`, both measured upwards from
    /// the baseline (so descent is negative).
    ///
    /// Falls back to a plausible split of the em box only if the face carries
    /// no horizontal line metrics at all, which the embedded one does.
    pub fn line_extents(&self, px: f32) -> (f32, f32) {
        self.font
            .horizontal_line_metrics(px)
            .map_or((px * 0.8, -px * 0.2), |m| (m.ascent, m.descent))
    }

    /// How far below a run's vertical centre its baseline sits, at `px`.
    ///
    /// Derived from [`Self::line_extents`] — the face's metrics — and so a
    /// function of the size alone. That is the point: the ribbon holds a live
    /// transcript whose *shown* substring changes every time a word scrolls
    /// off the left, and centring on that substring's own ink box (which is
    /// what [`Self::ink_extents`] measures) moved the whole run up or down by
    /// a pixel or three whenever a descender or a tall ascender entered or
    /// left it. A constant ribbon height must give a constant baseline.
    pub fn baseline_offset(&self, px: f32) -> f32 {
        let (ascent, descent) = self.line_extents(px);
        (ascent + descent) * 0.5
    }

    /// Distance from the baseline to the top and bottom of the run's inked
    /// area, both measured upwards.
    ///
    /// Deliberately *not* what positions the run any more — see
    /// [`Self::baseline_offset`] for why a live transcript cannot be centred
    /// on its own ink. It stays because measuring where a string's ink
    /// actually lands is the only way to check the drawn result against the
    /// geometry around it: `render`'s `the_wave_row_clears_the_live_text_ink_box`
    /// holds the waveform clear of the real glyphs at every DPI scale with it.
    #[allow(dead_code)]
    pub fn ink_extents(&mut self, text: &str, px: f32) -> (f32, f32) {
        let (mut top, mut bottom) = (f32::MIN, f32::MAX);
        for c in text.chars() {
            let key = self.ensure(c, px);
            let m = &self.glyphs[&key].metrics;
            if m.width == 0 || m.height == 0 {
                continue;
            }
            top = top.max(m.ymin as f32 + m.height as f32);
            bottom = bottom.min(m.ymin as f32);
        }
        if top == f32::MIN {
            (0.0, 0.0)
        } else {
            (top, bottom)
        }
    }

    /// Draw `text` with the face's line box centred vertically on `center_y`,
    /// so the baseline lands at the same place for every string of the same
    /// size — see [`Self::baseline_offset`].
    ///
    /// `alpha` scales the whole run, which is how every text fade in the pill is
    /// expressed.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        px: f32,
        tracking: f32,
        x: f32,
        center_y: f32,
        align: Align,
        paint: TextPaint,
        alpha: f32,
    ) {
        if text.is_empty() || alpha <= 0.001 || px <= 0.0 {
            return;
        }
        let width = self.measure(text, px, tracking);
        let left = match align {
            Align::Center => x - width * 0.5,
            Align::Right => x - width,
        };
        let baseline = center_y + self.baseline_offset(px);

        let mut pen = left;
        for c in text.chars() {
            let key = self.ensure(c, px);
            let glyph = &self.glyphs[&key];
            let m = &glyph.metrics;
            if m.width > 0 && m.height > 0 {
                let gx = (pen + m.xmin as f32).round() as i32;
                let gy = (baseline - m.ymin as f32 - m.height as f32).round() as i32;
                blit(
                    pixmap,
                    &glyph.bitmap,
                    m.width,
                    m.height,
                    gx,
                    gy,
                    paint,
                    (left, left + width),
                    alpha,
                );
            }
            pen += m.advance_width + tracking;
        }
    }
}

impl Default for FontAtlas {
    fn default() -> Self {
        Self::new()
    }
}

/// The longest suffix of `text`, on a char boundary, that measures no wider
/// than `max_w`. The overflow strategy for the live-transcript ribbon: rather
/// than clip pixels (this atlas has no clip-mask parameter to draw through),
/// the shown *string* is trimmed so the newest words stay against the right
/// padding and the oldest ones drop off cleanly.
///
/// It walks *backwards* from the end, accumulating one advance per step, and
/// its "does the whole thing already fit?" guard stops at `max_w` too, so the
/// cost is `O(shown)` rather than `O(len)`. That matters: the ribbon holds the
/// whole utterance, which grows for as long as the user keeps speaking, and
/// this runs once per frame inside a 16 ms budget. Re-measuring each candidate
/// suffix from the front made the cost quadratic; measuring the whole
/// transcript just to discover it overflows kept it linear in a string that
/// only ever gets longer.
pub(crate) fn trailing_fit<'a>(
    atlas: &mut FontAtlas,
    text: &'a str,
    px: f32,
    max_w: f32,
) -> &'a str {
    if atlas.measure_capped(text, px, 0.0, max_w) <= max_w {
        return text;
    }
    let mut w = 0.0;
    let mut start = text.len();
    for (byte_idx, c) in text.char_indices().rev() {
        w += atlas.advance(c, px);
        if w > max_w {
            break;
        }
        start = byte_idx;
    }
    &text[start..]
}

/// Alpha-blend a coverage bitmap into the pixmap.
#[allow(clippy::too_many_arguments)]
fn blit(
    pixmap: &mut Pixmap,
    bitmap: &[u8],
    bw: usize,
    bh: usize,
    x0: i32,
    y0: i32,
    paint: TextPaint,
    run: (f32, f32),
    alpha: f32,
) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let span = (run.1 - run.0).max(1.0);
    let pixels = pixmap.pixels_mut();

    for row in 0..bh as i32 {
        let py = y0 + row;
        if py < 0 || py >= ph {
            continue;
        }
        for col in 0..bw as i32 {
            let px = x0 + col;
            if px < 0 || px >= pw {
                continue;
            }
            let coverage = f32::from(bitmap[row as usize * bw + col as usize]) / 255.0;
            if coverage <= 0.0 {
                continue;
            }
            let colour = match paint {
                TextPaint::Solid(c) => c,
                TextPaint::Gradient(a, b) => a.lerp(b, (px as f32 - run.0) / span),
            };
            let a = coverage * colour.a * alpha;
            if a <= 0.0 {
                continue;
            }
            let slot = &mut pixels[(py * pw + px) as usize];
            *slot = over(*slot, colour, a);
        }
    }
}

/// Source-over compositing of a straight-alpha colour onto a premultiplied
/// destination pixel.
fn over(dst: PremultipliedColorU8, src: Rgba, alpha: f32) -> PremultipliedColorU8 {
    let a = alpha.clamp(0.0, 1.0);
    let inv = 1.0 - a;
    let chan = |s: u8, d: u8| {
        let s = f32::from(s) / 255.0 * a;
        let d = f32::from(d) / 255.0 * inv;
        ((s + d) * 255.0).round().clamp(0.0, 255.0) as u8
    };
    let out_a = ((a + f32::from(dst.alpha()) / 255.0 * inv) * 255.0)
        .round()
        .clamp(0.0, 255.0) as u8;
    let r = chan(src.r, dst.red()).min(out_a);
    let g = chan(src.g, dst.green()).min(out_a);
    let b = chan(src.b, dst.blue()).min(out_a);
    PremultipliedColorU8::from_rgba(r, g, b, out_a).unwrap_or(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atlas() -> FontAtlas {
        FontAtlas::new()
    }

    #[test]
    fn the_embedded_font_loads() {
        let mut a = atlas();
        assert!(a.measure("0", 11.0, 0.0) > 0.0);
    }

    #[test]
    fn the_face_is_monospaced() {
        // The timer relies on this: `0:03` must not shift as digits change.
        let mut a = atlas();
        let zero = a.measure("0", 11.0, 0.0);
        for c in ['1', '8', 'm', 'W', ':'] {
            let w = a.measure(&c.to_string(), 11.0, 0.0);
            assert!(
                (w - zero).abs() < 0.01,
                "{c} advances {w}, 0 advances {zero}"
            );
        }
        assert!((a.measure("0:00", 11.0, 0.0) - zero * 4.0).abs() < 0.05);
    }

    #[test]
    fn tracking_goes_between_glyphs_not_after_the_last_one() {
        let mut a = atlas();
        let plain = a.measure("abc", 10.0, 0.0);
        let tracked = a.measure("abc", 10.0, 2.0);
        assert!((tracked - plain - 4.0).abs() < 0.01, "{tracked} vs {plain}");
        // A single glyph gets no tracking at all.
        let one = a.measure("a", 10.0, 0.0);
        assert!((a.measure("a", 10.0, 5.0) - one).abs() < 0.01);
    }

    #[test]
    fn measuring_empty_text_is_zero_not_negative() {
        let mut a = atlas();
        assert_eq!(a.measure("", 11.0, 4.0), 0.0);
        assert_eq!(a.ink_extents("", 11.0), (0.0, 0.0));
        // Whitespace has advance but no ink.
        assert!(a.measure(" ", 11.0, 0.0) > 0.0);
        assert_eq!(a.ink_extents(" ", 11.0), (0.0, 0.0));
    }

    #[test]
    fn digits_sit_above_the_baseline() {
        let mut a = atlas();
        let (top, bottom) = a.ink_extents("0123456789", 11.0);
        assert!(top > 0.0, "top {top}");
        // -1 is antialiasing bleed under the baseline, not a descender.
        assert!(bottom >= -1.5, "digits should not descend: {bottom}");
    }

    #[test]
    fn glyphs_are_cached_not_re_rasterised() {
        let mut a = atlas();
        a.measure("0:00", 11.0, 0.0);
        let after_first = a.glyphs.len();
        a.measure("0:00", 11.0, 0.0);
        assert_eq!(a.glyphs.len(), after_first, "cache missed on a repeat");
        // A different size is a different entry.
        a.measure("0", 10.0, 0.0);
        assert!(a.glyphs.len() > after_first);
    }

    #[test]
    fn drawing_puts_ink_on_the_pixmap() {
        let mut a = atlas();
        let mut pm = Pixmap::new(120, 40).unwrap();
        a.draw(
            &mut pm,
            "142 ms",
            11.0,
            0.0,
            60.0,
            20.0,
            Align::Center,
            TextPaint::Solid(Rgba::hex(0xFF_FFFF)),
            1.0,
        );
        let lit = pm.pixels().iter().filter(|p| p.alpha() > 0).count();
        assert!(lit > 40, "only {lit} pixels inked");
    }

    #[test]
    fn alignment_moves_the_run() {
        let mut a = atlas();
        let bounds = |align: Align| {
            let mut pm = Pixmap::new(160, 40).unwrap();
            let mut atlas = FontAtlas::new();
            atlas.draw(
                &mut pm,
                "0:03",
                11.0,
                0.0,
                80.0,
                20.0,
                align,
                TextPaint::Solid(Rgba::hex(0xFF_FFFF)),
                1.0,
            );
            let mut min = i32::MAX;
            let mut max = i32::MIN;
            for (i, p) in pm.pixels().iter().enumerate() {
                if p.alpha() > 0 {
                    let x = (i % 160) as i32;
                    min = min.min(x);
                    max = max.max(x);
                }
            }
            (min, max)
        };
        let width = a.measure("0:03", 11.0, 0.0);
        let (c_min, c_max) = bounds(Align::Center);
        let (r_min, r_max) = bounds(Align::Right);
        assert!(r_max <= 81, "right-aligned run ends at {r_max}");
        assert!(
            (r_min as f32 - (80.0 - width)).abs() <= 2.0,
            "right-aligned run starts at {r_min}, expected {}",
            80.0 - width
        );
        assert!(
            ((c_min + c_max) / 2 - 80).abs() <= 2,
            "centre off at {c_min}..{c_max}"
        );
    }

    #[test]
    fn alpha_zero_draws_nothing() {
        let mut a = atlas();
        let mut pm = Pixmap::new(80, 30).unwrap();
        a.draw(
            &mut pm,
            "0:03",
            11.0,
            0.0,
            40.0,
            15.0,
            Align::Center,
            TextPaint::Solid(Rgba::hex(0xFF_FFFF)),
            0.0,
        );
        assert!(pm.pixels().iter().all(|p| p.alpha() == 0));
    }

    #[test]
    fn text_outside_the_pixmap_is_clipped_not_a_panic() {
        let mut a = atlas();
        let mut pm = Pixmap::new(20, 12).unwrap();
        for (x, y) in [(-500.0, 6.0), (500.0, 6.0), (10.0, -500.0), (10.0, 500.0)] {
            a.draw(
                &mut pm,
                "0123456789 ms",
                11.0,
                0.0,
                x,
                y,
                Align::Center,
                TextPaint::Solid(Rgba::hex(0xFF_FFFF)),
                1.0,
            );
        }
    }

    #[test]
    fn gradient_paint_varies_across_the_run() {
        let mut a = atlas();
        let mut pm = Pixmap::new(160, 40).unwrap();
        a.draw(
            &mut pm,
            "MMMMMMMM",
            16.0,
            0.0,
            80.0,
            20.0,
            Align::Center,
            TextPaint::Gradient(Rgba::hex(0xFF_0000), Rgba::hex(0x00_00FF)),
            1.0,
        );
        let sample = |from: usize, to: usize| {
            let mut r = 0u64;
            let mut b = 0u64;
            for (i, p) in pm.pixels().iter().enumerate() {
                let x = i % 160;
                if (from..to).contains(&x) && p.alpha() > 0 {
                    r += u64::from(p.red());
                    b += u64::from(p.blue());
                }
            }
            (r, b)
        };
        let (left_r, left_b) = sample(0, 60);
        let (right_r, right_b) = sample(90, 160);
        assert!(
            left_r > left_b,
            "left end should be red: {left_r} vs {left_b}"
        );
        assert!(
            right_b > right_r,
            "right end should be blue: {right_b} vs {right_r}"
        );
    }

    #[test]
    fn compositing_stays_premultiplied() {
        // Every output must satisfy r,g,b <= a or tiny-skia's invariant breaks
        // and UpdateLayeredWindow renders garbage.
        let opaque_white = PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap();
        let clear = PremultipliedColorU8::from_rgba(0, 0, 0, 0).unwrap();
        for dst in [opaque_white, clear] {
            for a in [0.0, 0.01, 0.5, 0.99, 1.0] {
                let out = over(dst, Rgba::hex(0xFF_FFFF), a);
                assert!(out.red() <= out.alpha());
                assert!(out.green() <= out.alpha());
                assert!(out.blue() <= out.alpha());
            }
        }
    }

    #[test]
    fn unknown_characters_do_not_panic() {
        let mut a = atlas();
        let mut pm = Pixmap::new(80, 30).unwrap();
        // A private-use codepoint the face certainly lacks, plus the middle dot
        // and em dash the mockup's chip actually uses.
        for s in ["\u{E000}", "·", "—", "日本語", "\u{1F600}"] {
            let _ = a.measure(s, 10.0, 0.5);
            a.draw(
                &mut pm,
                s,
                10.0,
                0.5,
                40.0,
                15.0,
                Align::Center,
                TextPaint::Solid(Rgba::hex(0xFF_FFFF)),
                1.0,
            );
        }
    }

    #[test]
    fn the_engine_chip_string_renders() {
        let mut a = atlas();
        let mut pm = Pixmap::new(300, 20).unwrap();
        a.draw(
            &mut pm,
            "groq · whisper-large-v3-turbo · en",
            10.0,
            0.8,
            150.0,
            10.0,
            Align::Center,
            TextPaint::Solid(Rgba::hex(0x5E_6778)),
            0.85,
        );
        let lit = pm.pixels().iter().filter(|p| p.alpha() > 0).count();
        assert!(lit > 200, "chip barely rendered: {lit} px");
    }

    #[test]
    fn trailing_fit_keeps_the_newest_words_and_fits_the_budget() {
        let mut a = atlas();
        let long = "the quick brown fox jumps over the lazy dog";
        let shown = trailing_fit(&mut a, long, 15.0, 60.0);
        assert!(shown.len() < long.len());
        assert!(long.ends_with(shown));
        assert!(a.measure(shown, 15.0, 0.0) <= 60.0);
    }

    /// The backwards walk must pick exactly the suffix the straightforward
    /// (quadratic) forward scan would, including on a multi-byte string and
    /// at a budget too small for even one character.
    #[test]
    fn trailing_fit_matches_a_front_to_back_reference() {
        let mut a = atlas();
        let reference = |a: &mut FontAtlas, text: &str, max_w: f32| -> String {
            text.char_indices()
                .map(|(i, _)| &text[i..])
                .find(|c| a.measure(c, 15.0, 0.0) <= max_w)
                .unwrap_or("")
                .to_string()
        };
        for text in [
            "the quick brown fox jumps over the lazy dog and keeps talking",
            "naïve café — 日本語 mixed in",
        ] {
            for max_w in [0.0, 3.0, 17.0, 60.0, 240.0] {
                assert_eq!(
                    trailing_fit(&mut a, text, 15.0, max_w),
                    reference(&mut a, text, max_w),
                    "text {text:?} at {max_w}"
                );
            }
        }
    }

    /// Below the budget the capped measure is the plain one; above it, it only
    /// promises "wider than the budget" — and it must stop early to be worth
    /// having, which is what the glyph-cache count checks.
    #[test]
    fn measuring_with_a_budget_stops_early_and_otherwise_agrees() {
        let mut a = atlas();
        let text = "the quick brown fox jumps over the lazy dog";
        let full = a.measure(text, 15.0, 0.0);
        assert_eq!(a.measure_capped(text, 15.0, 0.0, full), full);
        assert_eq!(a.measure_capped(text, 15.0, 0.0, full + 100.0), full);
        assert!(a.measure_capped(text, 15.0, 0.0, 40.0) > 40.0);
        assert!(a.measure_capped(text, 15.0, 0.0, 40.0) < full);
        assert_eq!(a.measure_capped("", 15.0, 4.0, 0.0), 0.0);

        // Distinct characters, so cache growth is a direct count of how much
        // of the string was walked.
        let mut fresh = FontAtlas::new();
        let distinct = "abcdefghijklmnopqrstuvwxyz";
        let three = fresh.measure("abc", 15.0, 0.0);
        let before = fresh.glyphs.len();
        fresh.measure_capped(distinct, 15.0, 0.0, three);
        assert!(
            fresh.glyphs.len() - before < 6,
            "walked {} extra glyphs for a three-glyph budget",
            fresh.glyphs.len() - before
        );
    }

    /// The live ribbon trims its text from the left as words scroll off, so a
    /// baseline derived from the shown substring's ink moves whenever a
    /// descender or a tall ascender enters or leaves it. The trailing glyph
    /// must land on exactly the same rows regardless of what precedes it.
    #[test]
    fn the_baseline_does_not_move_with_the_characters_in_the_run() {
        let rows = |text: &str| {
            let mut a = FontAtlas::new();
            let mut pm = Pixmap::new(200, 60).unwrap();
            a.draw(
                &mut pm,
                text,
                15.0,
                0.0,
                150.0,
                30.0,
                Align::Right,
                TextPaint::Solid(Rgba::hex(0xFF_FFFF)),
                1.0,
            );
            // Only the trailing glyph's column window, which right alignment
            // pins in place no matter what comes before it.
            let last_w = a.measure(&text.chars().last().unwrap().to_string(), 15.0, 0.0);
            let from = (150.0 - last_w).floor() as usize;
            let (mut top, mut bottom) = (usize::MAX, 0usize);
            for (i, p) in pm.pixels().iter().enumerate() {
                let (x, y) = (i % 200, i / 200);
                if x >= from && x < 150 && p.alpha() > 0 {
                    top = top.min(y);
                    bottom = bottom.max(y);
                }
            }
            (top, bottom)
        };
        let reference = rows("ax");
        assert!(reference.0 < reference.1, "nothing drawn: {reference:?}");
        for text in ["hx", "jx", "gpqx", "AWx", "({x", "x"] {
            assert_eq!(
                rows(text),
                reference,
                "trailing glyph moved when the run became {text:?}"
            );
        }
        // ... and the offset it comes from is a function of size alone.
        let a = atlas();
        assert!((a.baseline_offset(15.0) - a.baseline_offset(15.0)).abs() < f32::EPSILON);
        assert!(a.baseline_offset(30.0) > a.baseline_offset(15.0));
        let (ascent, descent) = a.line_extents(15.0);
        assert!(ascent > 0.0 && descent < 0.0, "{ascent} {descent}");
    }

    #[test]
    fn trailing_fit_returns_the_whole_string_when_it_already_fits() {
        let mut a = atlas();
        let short = "hello";
        let width = a.measure(short, 15.0, 0.0);
        assert_eq!(trailing_fit(&mut a, short, 15.0, width + 5.0), short);
    }
}

//! Colour tokens.
//!
//! Geometry and motion are *not* theme properties — they live in [`crate::layout`]
//! and [`crate::motion`] and are identical for every theme. The captain's locked
//! decision was "Prism dark default, Porcelain light day one, **same geometry,
//! swapped tokens**", so a [`Theme`] is colour and nothing else. Swapping themes
//! never moves a pixel.

/// A colour token: 8-bit RGB plus a floating-point alpha.
///
/// Alpha is `f32` because the design tokens are CSS `rgba()` literals with
/// fractional alpha (`rgba(255,255,255,.06)`), and because animation multiplies
/// alpha repeatedly — doing that in 8-bit quantises visibly on a 130 ms fade.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    /// Red, 0–255.
    pub r: u8,
    /// Green, 0–255.
    pub g: u8,
    /// Blue, 0–255.
    pub b: u8,
    /// Alpha, 0.0–1.0. Straight (not premultiplied).
    pub a: f32,
}

impl Rgba {
    /// Fully transparent black. Used as the end stops of the spectrum ramps.
    pub const TRANSPARENT: Rgba = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 0.0,
    };

    /// An opaque colour from a `0xRRGGBB` literal, so tokens read like the CSS
    /// they came from: `Rgba::hex(0xFF_6B8A)`.
    pub const fn hex(hex: u32) -> Self {
        Self {
            r: (hex >> 16) as u8,
            g: (hex >> 8) as u8,
            b: hex as u8,
            a: 1.0,
        }
    }

    /// A translucent colour from a `0xRRGGBB` literal plus alpha, matching a
    /// CSS `rgba()` token.
    pub const fn hex_a(hex: u32, a: f32) -> Self {
        Self {
            r: (hex >> 16) as u8,
            g: (hex >> 8) as u8,
            b: hex as u8,
            a,
        }
    }

    /// This colour with its alpha multiplied by `k` (clamped to 0.0–1.0).
    ///
    /// This is how every fade in the overlay is expressed: the renderer never
    /// mutates a token, it scales one.
    #[must_use]
    pub fn fade(self, k: f32) -> Self {
        Self {
            a: (self.a * k).clamp(0.0, 1.0),
            ..self
        }
    }

    /// Linear interpolation in straight (non-premultiplied) RGBA space.
    ///
    /// Used to build the spectrum ramp across the 28 bars from a handful of
    /// authored stops, matching the mockup's `mix()`/`ramp()` helpers.
    #[must_use]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let ch = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Self {
            r: ch(self.r, other.r),
            g: ch(self.g, other.g),
            b: ch(self.b, other.b),
            a: self.a + (other.a - self.a) * t,
        }
    }

    /// WCAG relative luminance of the RGB part, ignoring alpha.
    ///
    /// Only used by the token-integrity tests, which assert that each palette's
    /// ink actually contrasts with its own shell.
    #[must_use]
    pub fn relative_luminance(self) -> f32 {
        let lin = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    pub(crate) fn to_color(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba(
            f32::from(self.r) / 255.0,
            f32::from(self.g) / 255.0,
            f32::from(self.b) / 255.0,
            self.a.clamp(0.0, 1.0),
        )
        .unwrap_or(tiny_skia::Color::TRANSPARENT)
    }
}

/// Sample a multi-stop ramp at `t` in 0.0–1.0.
///
/// Stops are evenly spaced, which is how both mockups author their spectrum.
#[must_use]
pub fn sample_ramp(stops: &[Rgba], t: f32) -> Rgba {
    match stops.len() {
        0 => Rgba::TRANSPARENT,
        1 => stops[0],
        n => {
            let s = t.clamp(0.0, 1.0) * (n - 1) as f32;
            let i = (s.floor() as usize).min(n - 2);
            stops[i].lerp(stops[i + 1], s - i as f32)
        }
    }
}

/// The complete colour token set for one overlay skin.
///
/// Every field is `Copy`, so a theme can be sent across the command channel
/// without allocating. Slice fields are `&'static` because both shipping
/// palettes are `const`; a caller building a bespoke theme can point them at
/// their own `const` arrays.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Human-readable name, shown by the demo and used in tests.
    pub name: &'static str,
    /// True for dark skins. Only affects the demo's console output today; kept
    /// so a future system-theme follower has something to switch on.
    pub dark: bool,

    // ---- pill shell ----
    /// 1 px border stroked just inside the pill outline.
    pub border: Rgba,
    /// 1 px ring stroked just outside the pill outline (`0 0 0 1px` in CSS).
    pub outer_ring: Rgba,
    /// 1 px inner highlight along the top edge (`inset 0 1px 0` in CSS).
    pub inner_highlight: Rgba,
    /// Soft, broad highlight wash across the upper portion of the shell —
    /// the glass "sheen" a curved translucent surface catches light with,
    /// distinct from `inner_highlight`'s crisp 1 px line.
    pub glass_sheen: Rgba,
    /// Backing tint painted as a soft band behind live text only, guaranteed
    /// to contrast with `ink`. The shell's own fill is a colour ramp in
    /// service of looking like glass, not of legibility, so it cannot itself
    /// promise contrast at every point along it — this is the local fix for
    /// that, instead of pulling the whole surface back toward opaque.
    pub text_scrim: Rgba,
    /// Colour of the ambient drop shadow.
    pub ambient_shadow: Rgba,

    // ---- state glows (the coloured halo, a shadow colour — never a blur filter) ----
    /// Halo while idle / inserting has not happened yet.
    pub glow_idle: Rgba,
    /// Halo while listening.
    pub glow_listening: Rgba,
    /// Halo while the inserted confirmation is on screen.
    pub glow_inserted: Rgba,

    // ---- the live signal path: the only place the spectrum is allowed ----
    /// Stops of the ramp painted across the waveform bars.
    pub spectrum: &'static [Rgba],
    /// Stops of the processing scan band.
    pub scan: &'static [Rgba],

    // ---- ink ----
    /// Primary text.
    pub ink: Rgba,
    /// Secondary text. Belongs to the captions this design no longer draws
    /// (see the note beside `render::draw_ribbon`); kept as a palette token,
    /// not painted.
    pub ink_dim: Rgba,
    /// Tertiary text, and the idle capsule core. Same caption story as
    /// [`Self::ink_dim`].
    pub ink_faint: Rgba,
    /// Outline traced around the elapsed-recording timer's glyphs, and only
    /// those — `render::draw_timer` is the sole painter.
    ///
    /// The timer is the one run of text the default presentation draws, and
    /// it is drawn straight onto the glass: [`Self::text_scrim`] is gated on
    /// live text and must stay that way (round 3's whole complaint was a dark
    /// band on the glass), so the timer cannot borrow the promise that token
    /// carries. This is the timer-scoped substitute, and it works by being
    /// *opposite* [`Self::ink`] in luminance rather than by knowing anything
    /// about the desktop underneath: whatever is behind the shape either
    /// contrasts with `ink`, in which case the fill reads, or sits near
    /// `ink`'s own luminance, in which case this outline does. There is no
    /// third case, which is why no backdrop sampling is needed.
    /// `the_timer_edge_reads_against_any_desktop_the_ink_cannot` holds both
    /// halves of that against the real composited shell.
    pub timer_edge: Rgba,

    // ---- capsule ----
    /// Live core fill (mint/sky — never a rec-red cue).
    pub rec: Rgba,
    /// Success accent — the inserted check.
    pub ok: Rgba,
    /// The single non-spectrum accent this skin is allowed.
    pub accent: Rgba,
    /// Capsule ring while hidden.
    pub ring_idle: Rgba,
    /// Capsule ring while listening.
    pub ring_listening: Rgba,
    /// Capsule ring while processing.
    pub ring_processing: Rgba,
    /// Capsule ring while inserted.
    pub ring_inserted: Rgba,
    /// The two arc colours of the processing spinner.
    pub spinner: (Rgba, Rgba),

    // ---- telemetry ----
    /// Timer colour while processing (the mock tints it with the accent).
    pub processing_ink: Rgba,
    /// Horizontal gradient the latency figure is painted with on `inserted`.
    pub latency: (Rgba, Rgba),
}

/// Prism — the locked v1 dark default.
///
/// Glass, not flat black — the captain's own words on the first pass of this
/// shell: "it's now just black". Shell stops carry alpha rather than being
/// fully opaque, so the desktop shows through exactly the way a real layered
/// window composites (no backdrop sampling, no faked blur — see
/// `render/mod.rs`'s `draw_shell` for what that does and does not mean).
/// Solid UI accents stay cool mint/sky — never a red "recording" cue.
pub const PRISM_DARK: Theme = Theme {
    name: "prism-dark",
    dark: true,

    border: Rgba::hex_a(0xFF_FFFF, 0.10),
    outer_ring: Rgba::hex_a(0x00_0000, 0.22),
    inner_highlight: Rgba::hex_a(0xFF_FFFF, 0.16),
    glass_sheen: Rgba::hex_a(0xE4_F0_FF, 0.12),
    text_scrim: Rgba::hex_a(0x08_0A0D, 0.55),
    ambient_shadow: Rgba::hex_a(0x00_0000, 0.50),

    // Soft cool halos only — no purple blob, no crimson listening glow.
    glow_idle: Rgba::hex_a(0x6B_CBFF, 0.08),
    glow_listening: Rgba::hex_a(0x5C_E6A8, 0.10),
    glow_inserted: Rgba::hex_a(0x5C_E6A8, 0.12),

    // Live waveform only: muted cool instrument spectrum (no rose/red candy).
    spectrum: &[
        Rgba::hex(0xB8_C4_A0),
        Rgba::hex(0x8A_D4_B0),
        Rgba::hex(0x6B_E0_C0),
        Rgba::hex(0x6B_D0_D8),
        Rgba::hex(0x6B_CB_FF),
        Rgba::hex(0x7B_A8_E8),
        Rgba::hex(0x8A_9B_E0),
        Rgba::hex(0x9A_90_D0),
    ],
    scan: &[
        Rgba::TRANSPARENT,
        Rgba::hex(0x6B_E0_C0),
        Rgba::hex(0x6B_CB_FF),
        Rgba::hex(0x8A_9B_E0),
        Rgba::TRANSPARENT,
    ],

    ink: Rgba::hex(0xED_EFF5),
    ink_dim: Rgba::hex(0x9A_A3B5),
    ink_faint: Rgba::hex(0x5E_6778),
    // Ink is near-white here, so the timer's outline is the deep end of the
    // same cool family the shell already lives in — a saturated steel blue,
    // not black: it has to separate near-white digits from a near-white
    // desktop showing through the glass without putting anything on the
    // surface that reads as the scrim this round removed.
    timer_edge: Rgba::hex(0x1B_4D7A),

    // Live core: mint spectrum tip — never solid rec-red.
    rec: Rgba::hex(0x5C_E6A8),
    ok: Rgba::hex(0x5C_E6A8),
    accent: Rgba::hex(0x6B_CBFF),
    ring_idle: Rgba::hex_a(0xFF_FFFF, 0.12),
    ring_listening: Rgba::hex_a(0x5C_E6A8, 0.28),
    ring_processing: Rgba::hex_a(0xFF_FFFF, 0.06),
    ring_inserted: Rgba::hex_a(0x5C_E6A8, 0.30),
    spinner: (Rgba::hex(0x6B_CBFF), Rgba::hex(0x8A_9B_E0)),

    processing_ink: Rgba::hex(0x6B_CBFF),
    latency: (Rgba::hex(0x6B_E0_C0), Rgba::hex(0x6B_CBFF)),
};

/// Porcelain — the light theme, shipping day one.
///
/// Same geometry and motion as Prism; translucent white glass shell with a
/// cool mint→sky live path. No rose/rec-red accents.
pub const PORCELAIN_LIGHT: Theme = Theme {
    name: "porcelain-light",
    dark: false,

    border: Rgba::hex_a(0x1C_2430, 0.08),
    outer_ring: Rgba::hex_a(0x1C_2430, 0.05),
    inner_highlight: Rgba::hex_a(0xFF_FFFF, 0.90),
    glass_sheen: Rgba::hex_a(0xFF_FFFF, 0.38),
    text_scrim: Rgba::hex_a(0xFF_FFFF, 0.60),
    ambient_shadow: Rgba::hex_a(0x1C_2430, 0.14),

    glow_idle: Rgba::hex_a(0x7A_A8_C8, 0.07),
    glow_listening: Rgba::hex_a(0x3D_BF8A, 0.09),
    glow_inserted: Rgba::hex_a(0x3D_BF8A, 0.10),

    spectrum: &[
        Rgba::hex(0x9A_C8_B0),
        Rgba::hex(0x8E_C5_C8),
        Rgba::hex(0x8E_C5_E8),
        Rgba::hex(0x9E_B0_E0),
        Rgba::hex(0xA8_A8_D0),
    ],
    scan: &[
        Rgba::TRANSPARENT,
        Rgba::hex(0x8E_C5_C8),
        Rgba::hex(0x8E_C5_E8),
        Rgba::TRANSPARENT,
    ],

    ink: Rgba::hex(0x1C_2430),
    ink_dim: Rgba::hex(0x5A_6678),
    ink_faint: Rgba::hex(0x8B_97A8),
    // The mirror of Prism's: this ink is dark, so the outline is the light
    // end — a cool near-white that only becomes visible when the desktop
    // behind the glass is dark enough to swallow the digits.
    timer_edge: Rgba::hex(0xF4_F8FF),

    // Live core: mint — never rose/rec-red.
    rec: Rgba::hex(0x3D_BF8A),
    ok: Rgba::hex(0x3D_BF8A),
    accent: Rgba::hex(0x6E_9F_C8),
    ring_idle: Rgba::hex_a(0x1C_2430, 0.10),
    ring_listening: Rgba::hex_a(0x3D_BF8A, 0.26),
    ring_processing: Rgba::hex_a(0x1C_2430, 0.06),
    ring_inserted: Rgba::hex_a(0x3D_BF8A, 0.30),
    spinner: (Rgba::hex(0x6E_9F_C8), Rgba::hex_a(0x6E_9F_C8, 0.40)),

    processing_ink: Rgba::hex(0x6E_9F_C8),
    latency: (Rgba::hex(0x3D_BF8A), Rgba::hex(0x6E_CF_B0)),
};

/// Both shipping palettes, in the order they are offered to the user.
pub const THEMES: [Theme; 2] = [PRISM_DARK, PORCELAIN_LIGHT];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_literals_decompose() {
        let c = Rgba::hex(0xFF_6B8A);
        assert_eq!((c.r, c.g, c.b), (0xFF, 0x6B, 0x8A));
        assert!((c.a - 1.0).abs() < f32::EPSILON);
        assert!((Rgba::hex_a(0x00_0000, 0.5).a - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn fade_is_clamped_and_multiplicative() {
        let c = Rgba::hex_a(0xFF_FFFF, 0.5);
        assert!((c.fade(0.5).a - 0.25).abs() < 1e-6);
        assert!((c.fade(0.0).a).abs() < f32::EPSILON);
        assert!((c.fade(10.0).a - 1.0).abs() < f32::EPSILON);
        // fade never touches the chroma
        assert_eq!(
            (c.fade(0.3).r, c.fade(0.3).g, c.fade(0.3).b),
            (255, 255, 255)
        );
    }

    #[test]
    fn ramp_hits_its_endpoints_and_stays_in_gamut() {
        for theme in THEMES {
            let first = sample_ramp(theme.spectrum, 0.0);
            let last = sample_ramp(theme.spectrum, 1.0);
            assert_eq!(first, theme.spectrum[0], "{}", theme.name);
            assert_eq!(
                last,
                theme.spectrum[theme.spectrum.len() - 1],
                "{}",
                theme.name
            );
            for i in 0..=100 {
                let s = sample_ramp(theme.spectrum, i as f32 / 100.0);
                assert!((0.0..=1.0).contains(&s.a), "{}", theme.name);
            }
        }
    }

    #[test]
    fn ramp_degenerate_inputs_do_not_panic() {
        assert_eq!(sample_ramp(&[], 0.5), Rgba::TRANSPARENT);
        let one = Rgba::hex(0x123456);
        assert_eq!(sample_ramp(&[one], 0.5), one);
        // out-of-range t is clamped, not wrapped
        assert_eq!(sample_ramp(&[one, Rgba::TRANSPARENT], -3.0), one);
    }

    /// Every alpha in every token must be a sane 0..=1 — a stray `2.0` would
    /// silently over-saturate a blend rather than fail loudly.
    #[test]
    fn every_token_alpha_is_normalised() {
        for theme in THEMES {
            let mut all = vec![
                theme.border,
                theme.outer_ring,
                theme.inner_highlight,
                theme.glass_sheen,
                theme.text_scrim,
                theme.ambient_shadow,
                theme.glow_idle,
                theme.glow_listening,
                theme.glow_inserted,
                theme.ink,
                theme.ink_dim,
                theme.ink_faint,
                theme.timer_edge,
                theme.rec,
                theme.ok,
                theme.accent,
                theme.ring_idle,
                theme.ring_listening,
                theme.ring_processing,
                theme.ring_inserted,
                theme.spinner.0,
                theme.spinner.1,
                theme.processing_ink,
                theme.latency.0,
                theme.latency.1,
            ];
            all.extend_from_slice(theme.spectrum);
            all.extend_from_slice(theme.scan);
            for c in all {
                assert!(
                    (0.0..=1.0).contains(&c.a),
                    "{}: alpha {} out of range",
                    theme.name,
                    c.a
                );
            }
        }
    }

    /// Source-over compositing of a translucent colour onto an opaque one,
    /// the same arithmetic the rasteriser does per pixel.
    fn composite(src: Rgba, dst: Rgba) -> Rgba {
        let a = src.a.clamp(0.0, 1.0);
        let ch = |s: u8, d: u8| (f32::from(s) * a + f32::from(d) * (1.0 - a)).round() as u8;
        Rgba {
            r: ch(src.r, dst.r),
            g: ch(src.g, dst.g),
            b: ch(src.b, dst.b),
            a: 1.0,
        }
    }

    /// WCAG relative-contrast ratio between two opaque colours.
    fn contrast(a: Rgba, b: Rgba) -> f32 {
        let (la, lb) = (a.relative_luminance(), b.relative_luminance());
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// What the glass actually composites to over `desktop` at `stop` — the
    /// shell fill and nothing else, which is exactly what the timer is drawn
    /// onto in the default presentation.
    fn shell_over(stop: Rgba, desktop: Rgba) -> Rgba {
        composite(stop.fade(crate::render::GLASS_FILL_ALPHA), desktop)
    }

    /// The timer is the one run of text the *default* presentation draws, and
    /// `text_scrim` — the token that carries the legibility promise — is
    /// gated on live text and has to stay that way. `timer_edge` is the
    /// timer-scoped substitute, and this is the property it exists for:
    /// against any desktop, at any point of the shell's ramp, *something* in
    /// the drawn glyph separates from what is behind it.
    ///
    /// Two assertions, because either alone is satisfiable by a token that
    /// does not work. The outline has to be genuinely distinct from the ink
    /// it traces — the mechanism this replaced re-drew the run in `ink`
    /// itself, which thickened the strokes and added no contrast at all — and
    /// then, for every stop over both extreme desktops, at least one of the
    /// two has to clear the floor against the composited shell. There is no
    /// backdrop sampling anywhere in this crate, so "at least one" is the
    /// only form the guarantee can take: a backing near `ink`'s luminance is
    /// exactly where `timer_edge` carries it, and vice versa.
    #[test]
    fn the_timer_edge_reads_against_any_desktop_the_ink_cannot() {
        for theme in THEMES {
            let apart = contrast(theme.ink, theme.timer_edge);
            assert!(
                apart >= 4.5,
                "{}: timer_edge contrasts only {apart:.2} with the ink it outlines — an \
                 outline that close to the fill only adds weight",
                theme.name
            );

            let mut worst = f32::MAX;
            let mut worst_where = String::new();
            for desktop in [Rgba::hex(0x00_0000), Rgba::hex(0xFF_FFFF)] {
                for (i, stop) in theme.spectrum.iter().enumerate() {
                    let backing = shell_over(*stop, desktop);
                    let best =
                        contrast(theme.ink, backing).max(contrast(theme.timer_edge, backing));
                    if best < worst {
                        worst = best;
                        worst_where = format!(
                            "stop {i} over #{:02X}{:02X}{:02X}",
                            desktop.r, desktop.g, desktop.b
                        );
                    }
                }
            }
            assert!(
                worst >= 3.0,
                "{}: the timer's best-contrasting part scores only {worst:.2} against the \
                 composited glass ({worst_where}) — the readout disappears there",
                theme.name
            );
        }
    }

    /// The failure this round's fix answers, kept as a test so nobody
    /// "simplifies" the outline back out: the ink *alone* genuinely does
    /// vanish against one of the two extreme desktops in each theme. Without
    /// this, `the_timer_edge_reads_against_any_desktop_the_ink_cannot` above
    /// would keep passing on the ink's own contribution if the outline were
    /// deleted.
    #[test]
    fn the_ink_alone_does_disappear_somewhere_which_is_why_the_outline_exists() {
        for theme in THEMES {
            let mut worst = f32::MAX;
            for desktop in [Rgba::hex(0x00_0000), Rgba::hex(0xFF_FFFF)] {
                for stop in theme.spectrum {
                    worst = worst.min(contrast(theme.ink, shell_over(*stop, desktop)));
                }
            }
            assert!(
                worst < 3.0,
                "{}: ink now clears 3.0 ({worst:.2}) unaided — re-derive timer_edge rather \
                 than leaving a token nothing depends on",
                theme.name
            );
        }
    }

    /// The pill is drawn on top of an unknown desktop, and its own shell
    /// fill is a colour ramp (`theme.spectrum`) chosen for glassy variety,
    /// not for legibility — it does not itself promise contrast at every
    /// point along it. `text_scrim` is the token that carries that promise
    /// instead: a soft band painted behind live text only (see
    /// `render::draw_ribbon`). This test protects that guarantee, the one
    /// contrast the pill actually controls regardless of shell or desktop.
    ///
    /// It scores `ink` against what is *actually on screen* behind it, not
    /// against `text_scrim`'s bare RGB: the scrim is translucent, so its own
    /// `relative_luminance` (which ignores alpha) describes a colour that is
    /// never painted, and a test written against it would keep passing if the
    /// alpha were dropped to nothing. So the backing is rebuilt the way the
    /// renderer builds it — every `spectrum` stop at `GLASS_FILL_ALPHA` over
    /// the two extreme desktops, then `text_scrim` over that — and the worst
    /// stop/desktop combination has to clear the floor. Only `ink` is checked
    /// because only `ink` is ever painted on the scrim; `ink_dim` and
    /// `ink_faint` belong to captions this design no longer draws.
    #[test]
    fn ink_contrasts_with_its_own_text_scrim() {
        for theme in THEMES {
            let ink = theme.ink.relative_luminance();
            let mut worst = f32::MAX;
            let mut worst_where = String::new();
            for desktop in [Rgba::hex(0x00_0000), Rgba::hex(0xFF_FFFF)] {
                for (i, stop) in theme.spectrum.iter().enumerate() {
                    let shell = composite(stop.fade(crate::render::GLASS_FILL_ALPHA), desktop);
                    let backing = composite(theme.text_scrim, shell).relative_luminance();
                    let (hi, lo) = if ink > backing {
                        (ink, backing)
                    } else {
                        (backing, ink)
                    };
                    let ratio = (hi + 0.05) / (lo + 0.05);
                    if ratio < worst {
                        worst = ratio;
                        worst_where = format!(
                            "stop {i} over #{:02X}{:02X}{:02X}",
                            desktop.r, desktop.g, desktop.b
                        );
                    }
                }
            }
            assert!(
                worst >= 2.5,
                "{}: ink contrasts only {worst:.2} against the composited text_scrim ({worst_where})",
                theme.name
            );
        }
    }

    /// Spectrum ramps need at least two stops or `sample_ramp` degenerates to a
    /// flat colour and the "bars are always ramped" rule silently stops holding.
    #[test]
    fn every_theme_has_a_real_ramp() {
        for theme in THEMES {
            assert!(theme.spectrum.len() >= 2, "{}", theme.name);
            assert!(theme.scan.len() >= 3, "{}", theme.name);
            // Both ends of the scan band fade out, so it never collides with
            // the pill's rounded ends.
            assert_eq!(theme.scan[0].a, 0.0, "{}", theme.name);
            assert_eq!(theme.scan[theme.scan.len() - 1].a, 0.0, "{}", theme.name);
        }
    }

    #[test]
    fn theme_names_are_unique() {
        assert_ne!(PRISM_DARK.name, PORCELAIN_LIGHT.name);
        const { assert!(PRISM_DARK.dark) };
        const { assert!(!PORCELAIN_LIGHT.dark) };
    }

    /// Captain desk rule: solid UI accents must never read as rec-red.
    /// Spectrum may carry multi-hue on the live path only; the core, ring, and
    /// listening glow stay cool mint/sky.
    #[test]
    fn solid_accents_are_not_rec_red() {
        let not_red = |c: Rgba, label: &str, theme: &str| {
            // "Rec-red" = red channel clearly dominates green and blue.
            assert!(
                !(c.r > c.g.saturating_add(30) && c.r > c.b.saturating_add(30)),
                "{theme} {label} is rec-red: #{:02X}{:02X}{:02X}",
                c.r,
                c.g,
                c.b
            );
        };
        for theme in THEMES {
            not_red(theme.rec, "rec", theme.name);
            not_red(theme.glow_listening, "glow_listening", theme.name);
            not_red(theme.ring_listening, "ring_listening", theme.name);
        }
    }
}

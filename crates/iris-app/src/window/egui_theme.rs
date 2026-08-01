//! Map `iris-overlay`'s Prism/Porcelain colour tokens onto egui.
//!
//! The window and the pill are meant to read as the same product, so colour
//! is single-sourced from [`iris_overlay::theme`] rather than re-authored
//! here — see [`color32`] and [`visuals`]. Geometry is this window's own (a
//! resizable, decorated app window is not the pill's shape); the chrome that
//! *is* shared — the background wash, the card frames, the spectrum accent —
//! is painted directly from the same tokens in `crate::window::ui`, echoing
//! how `iris-overlay` itself keeps geometry and colour separate.

use egui::{Color32, CornerRadius, Stroke};
use iris_overlay::Rgba;

/// `iris_overlay`'s straight (non-premultiplied) colour token as an egui
/// [`Color32`].
#[must_use]
pub fn color32(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, unit_to_u8(c.a))
}

fn unit_to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The window's base surface — the page behind everything.
///
/// The pill has no opaque surface token to borrow: its shell is a colour ramp
/// painted at a glass alpha over whatever desktop happens to be behind it, so
/// nothing in [`iris_overlay::Theme`] is "the background" on its own. This
/// window is an ordinary decorated window with no desktop showing through, so
/// it needs one. `text_scrim` is the honest source: its documented job on the
/// pill is to be the backing that guarantees contrast with `ink`, which is
/// exactly what this window's page has to do. Taken opaque (there is nothing
/// underneath to composite against here) and nudged a hair toward the ink, so
/// the raised surfaces below have somewhere to rise from.
#[must_use]
pub fn surface(theme: &iris_overlay::Theme) -> Rgba {
    opaque(theme.text_scrim).lerp(theme.ink, 0.03)
}

/// The raised surface — cards, inset fields, the top of the background wash.
///
/// "Raised" means catching more light, which is a different direction on each
/// skin: on a dark theme the ink *is* the light end, so lift toward it; on a
/// light one the scrim is already the brightest thing the palette has, so
/// rise back to it.
#[must_use]
pub fn raised_surface(theme: &iris_overlay::Theme) -> Rgba {
    if theme.dark {
        surface(theme).lerp(theme.ink, 0.05)
    } else {
        opaque(theme.text_scrim)
    }
}

fn opaque(c: Rgba) -> Rgba {
    Rgba { a: 1.0, ..c }
}

/// Build an [`egui::Visuals`] tinted with `theme`'s tokens.
///
/// Starts from egui's own dark/light base (so every widget this file does
/// not touch — sliders, scrollbars, tooltips — still looks coherent) and
/// overrides the handful of fields that carry the palette: fills, strokes,
/// selection and the accent-tinted hover/active states.
#[must_use]
pub fn visuals(theme: &iris_overlay::Theme) -> egui::Visuals {
    let mut v = if theme.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.hyperlink_color = color32(theme.accent);
    v.selection.bg_fill = color32(theme.accent.fade(0.35));
    v.selection.stroke = Stroke::new(1.0_f32, color32(theme.accent));
    v.window_fill = color32(surface(theme));
    v.panel_fill = color32(surface(theme));
    v.extreme_bg_color = color32(raised_surface(theme));
    v.faint_bg_color = color32(raised_surface(theme));
    v.code_bg_color = color32(raised_surface(theme));
    v.warn_fg_color = color32(theme.accent);
    v.error_fg_color = warn_color(theme);
    v.window_stroke = Stroke::new(1.0_f32, color32(theme.border));
    v.window_shadow.color = color32(theme.ambient_shadow);
    v.popup_shadow.color = color32(theme.ambient_shadow);

    let corner = CornerRadius::same(10);
    v.widgets.noninteractive.corner_radius = corner;
    v.widgets.noninteractive.bg_fill = color32(raised_surface(theme));
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, color32(theme.border));
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, color32(theme.ink_dim));

    v.widgets.inactive.corner_radius = corner;
    v.widgets.inactive.bg_fill = color32(raised_surface(theme));
    v.widgets.inactive.weak_bg_fill = color32(raised_surface(theme));
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, color32(theme.border));
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, color32(theme.ink));

    v.widgets.hovered.corner_radius = corner;
    v.widgets.hovered.bg_fill = color32(theme.accent.fade(0.18));
    v.widgets.hovered.weak_bg_fill = color32(theme.accent.fade(0.12));
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, color32(theme.accent.fade(0.6)));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, color32(theme.ink));

    v.widgets.active.corner_radius = corner;
    v.widgets.active.bg_fill = color32(theme.accent.fade(0.30));
    v.widgets.active.weak_bg_fill = color32(theme.accent.fade(0.22));
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, color32(theme.accent));
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, color32(theme.ink));

    v.widgets.open.corner_radius = corner;
    v.widgets.open.bg_fill = color32(theme.accent.fade(0.22));
    v.widgets.open.bg_stroke = Stroke::new(1.0_f32, color32(theme.accent));
    v.widgets.open.fg_stroke = Stroke::new(1.0_f32, color32(theme.ink));

    v
}

/// A warm accent for destructive/error text. Neither palette defines a red —
/// "no rec-red" is a locked rule for the pill's live states — so this reuses
/// the warmest stop in the spectrum ramp rather than inventing an off-brand
/// red. `pub(crate)` so `ui::chrome::warn` can share it rather than
/// re-deriving the same colour.
pub(crate) fn warn_color(theme: &iris_overlay::Theme) -> Color32 {
    color32(theme.spectrum[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_overlay::{PORCELAIN_LIGHT, PRISM_DARK};

    #[test]
    fn color32_preserves_channels_and_scales_alpha() {
        // Color32 stores premultiplied internally (see its own docs), so the
        // straight channels are only recoverable via `to_srgba_unmultiplied`,
        // and even then only up to the rounding error that method's own docs
        // warn about for non-opaque colours — hence `within_one`, not `==`.
        let c = color32(Rgba::hex_a(0x6B_CBFF, 0.5));
        let [r, g, b, a] = c.to_srgba_unmultiplied();
        let within_one = |a: u8, b: u8| a.abs_diff(b) <= 1;
        assert!(
            within_one(r, 0x6B) && within_one(g, 0xCB) && within_one(b, 0xFF),
            "{r} {g} {b}"
        );
        assert_eq!(a, 128); // round(0.5 * 255)
    }

    #[test]
    fn opaque_tokens_stay_fully_opaque() {
        assert_eq!(color32(Rgba::hex(0xFFFFFF)).a(), 255);
    }

    #[test]
    fn dark_mode_flag_follows_the_theme() {
        assert!(visuals(&PRISM_DARK).dark_mode);
        assert!(!visuals(&PORCELAIN_LIGHT).dark_mode);
    }

    #[test]
    fn surfaces_are_opaque_and_the_raised_one_is_lighter_on_both_skins() {
        for theme in [PRISM_DARK, PORCELAIN_LIGHT] {
            let (base, raised) = (surface(&theme), raised_surface(&theme));
            assert_eq!(color32(base).a(), 255, "{}", theme.name);
            assert_eq!(color32(raised).a(), 255, "{}", theme.name);
            assert!(
                raised.relative_luminance() > base.relative_luminance(),
                "{} raises the wrong way",
                theme.name
            );
        }
    }

    #[test]
    fn the_two_themes_produce_different_panel_fills() {
        assert_ne!(
            visuals(&PRISM_DARK).panel_fill,
            visuals(&PORCELAIN_LIGHT).panel_fill
        );
    }
}

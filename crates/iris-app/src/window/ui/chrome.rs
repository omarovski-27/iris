//! Shared "glassy Prism" painting: the background wash, the card frame and
//! the spectrum accent bar every tab is built from.
//!
//! Colour is entirely [`iris_overlay::theme`] tokens — see the module docs on
//! [`crate::window`] for why. Flat, rounded fills and strokes go through
//! egui's native `Frame`/`Painter`, which supports rounding and a fill or a
//! stroke together natively; egui has no built-in *gradient* fill, so the two
//! places this window is genuinely a gradient (the window wash, the spectrum
//! bar) are hand-built two-triangle meshes with per-vertex colour, the same
//! primitive a GPU gradient always comes down to.

use egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, Stroke, Ui};
use iris_overlay::theme::sample_ramp;
use iris_overlay::Theme as PillTheme;

use super::super::egui_theme::{color32, raised_surface, surface, warn_color};

/// Paint a vertical raised-to-base surface wash across `rect` — the
/// window's own background, so it reads as one tinted surface rather than a
/// flat OS-grey window with coloured widgets dropped on top.
///
/// Takes a raw [`Painter`] rather than a [`Ui`] so the caller can paint it at
/// the background layer, behind every panel, in one call — see
/// `ui::draw_root`.
pub fn background_wash(painter: &Painter, theme: &PillTheme, rect: Rect) {
    let top = color32(raised_surface(theme));
    let bottom = color32(surface(theme));
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(Shape::mesh(mesh));
}

/// Paint `theme`'s spectrum ramp across `rect` — the one signature "colour
/// genuinely shifts across a surface" element, echoing the pill's own
/// hairline/waveform. Used as a thin accent bar, never as a body fill.
/// Orients along whichever side of `rect` is longer, so the same call works
/// for a horizontal divider under the nav and a vertical one beside it.
pub fn spectrum_bar(painter: &Painter, theme: &PillTheme, rect: Rect) {
    const SEGMENTS: usize = 32;
    let vertical = rect.height() > rect.width();
    let mut mesh = Mesh::default();
    for i in 0..=SEGMENTS {
        let t = i as f32 / SEGMENTS as f32;
        let color = color32(sample_ramp(theme.spectrum, t));
        if vertical {
            let y = rect.top() + rect.height() * t;
            mesh.colored_vertex(Pos2::new(rect.left(), y), color);
            mesh.colored_vertex(Pos2::new(rect.right(), y), color);
        } else {
            let x = rect.left() + rect.width() * t;
            mesh.colored_vertex(Pos2::new(x, rect.top()), color);
            mesh.colored_vertex(Pos2::new(x, rect.bottom()), color);
        }
    }
    for i in 0..SEGMENTS as u32 {
        let base = i * 2;
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base + 1, base + 3, base + 2);
    }
    painter.add(Shape::mesh(mesh));
}

/// A translucent, rounded, softly bordered card — the History entry, the
/// Settings group, the Insights tile.
pub fn card(theme: &PillTheme) -> egui::Frame {
    egui::Frame::new()
        .fill(color32(raised_surface(theme).fade(0.92)))
        .stroke(Stroke::new(1.0_f32, color32(theme.border)))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::same(14))
        .shadow(egui::Shadow {
            offset: [0, 3],
            blur: 10,
            spread: 0,
            color: color32(theme.ambient_shadow),
        })
}

/// A small caps-style section label (e.g. "DICTATION", "APPEARANCE") in the
/// theme's faint ink, the same role `ink_faint` plays on the pill's engine
/// chip.
pub fn section_label(ui: &mut Ui, theme: &PillTheme, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(11.0)
            .extra_letter_spacing(1.2)
            .color(color32(theme.ink_faint)),
    );
}

/// Primary ink text colour, for callers building a [`egui::RichText`] by hand.
#[must_use]
pub fn ink(theme: &PillTheme) -> Color32 {
    color32(theme.ink)
}

/// Secondary ink text colour.
#[must_use]
pub fn ink_dim(theme: &PillTheme) -> Color32 {
    color32(theme.ink_dim)
}

/// Tertiary ink text colour — captions, hints.
#[must_use]
pub fn ink_faint(theme: &PillTheme) -> Color32 {
    color32(theme.ink_faint)
}

/// The accent colour (cool mint/sky — never rec-red), for success states and
/// the selected nav item.
#[must_use]
pub fn accent(theme: &PillTheme) -> Color32 {
    color32(theme.accent)
}

/// The "ok" colour — the same one the pill's inserted-confirmation check
/// uses — for a successful injection.
#[must_use]
pub fn ok(theme: &PillTheme) -> Color32 {
    color32(theme.ok)
}

/// The warm failure tone — see [`warn_color`] for why it is amber rather
/// than a red, and why it is a token of its own rather than a spectrum stop.
#[must_use]
pub fn warn(theme: &PillTheme) -> Color32 {
    warn_color(theme)
}

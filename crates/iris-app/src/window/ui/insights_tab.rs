//! The Insights tab: statistics rolled up from the session log by
//! [`crate::window::insights`], never a second data collector. Deliberately
//! not the AI speech analyzer — see that module's docs.

use egui::{RichText, Ui};

use crate::window::WindowState;

use super::chrome;

pub fn draw(ui: &mut Ui, state: &WindowState, theme: &iris_overlay::Theme) {
    ui.label(
        RichText::new("Insights")
            .size(20.0)
            .strong()
            .color(chrome::ink(theme)),
    );
    ui.add_space(14.0);

    if state.history.is_empty() {
        chrome::card(theme).show(ui, |ui| {
            ui.label(
                RichText::new(
                    "Nothing to show yet — insights fill in once you've dictated a few times.",
                )
                .color(chrome::ink_dim(theme)),
            );
        });
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            stat_grid(ui, state, theme);
            ui.add_space(14.0);

            ui.columns(2, |columns| {
                ranked_card(
                    &mut columns[0],
                    theme,
                    "Most repeated words",
                    &state.insights.top_words,
                );
                ranked_card(
                    &mut columns[1],
                    theme,
                    "Most repeated phrases",
                    &state.insights.top_phrases,
                );
            });
        });
}

fn stat_grid(ui: &mut Ui, state: &WindowState, theme: &iris_overlay::Theme) {
    let insights = &state.insights;
    let latency = |ms: Option<f64>| ms.map_or_else(|| "—".to_string(), |v| format!("{v:.0} ms"));
    let percent =
        |r: Option<f64>| r.map_or_else(|| "—".to_string(), |v| format!("{:.0}%", v * 100.0));

    let tiles: [(&str, String); 6] = [
        ("Dictations today", insights.dictations_today.to_string()),
        ("Dictations all-time", insights.total_dictations.to_string()),
        ("Words dictated", insights.total_words.to_string()),
        ("Success rate", percent(insights.success_rate())),
        ("Avg. latency", latency(insights.avg_latency_ms)),
        ("Median latency", latency(insights.median_latency_ms)),
    ];

    ui.columns(3, |columns| {
        for (i, (label, value)) in tiles.iter().enumerate() {
            stat_tile(&mut columns[i % 3], theme, label, value);
        }
    });
}

fn stat_tile(ui: &mut Ui, theme: &iris_overlay::Theme, label: &str, value: &str) {
    chrome::card(theme).show(ui, |ui| {
        ui.set_min_height(56.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new(value)
                    .size(22.0)
                    .strong()
                    .color(chrome::accent(theme)),
            );
            ui.label(
                RichText::new(label)
                    .size(11.5)
                    .color(chrome::ink_dim(theme)),
            );
        });
    });
    ui.add_space(8.0);
}

fn ranked_card(
    ui: &mut Ui,
    theme: &iris_overlay::Theme,
    title: &str,
    ranked: &[crate::window::Ranked],
) {
    chrome::card(theme).show(ui, |ui| {
        chrome::section_label(ui, theme, title);
        ui.add_space(8.0);
        if ranked.is_empty() {
            ui.label(
                RichText::new("Nothing repeated yet.")
                    .size(12.0)
                    .italics()
                    .color(chrome::ink_faint(theme)),
            );
            return;
        }
        let max = ranked.iter().map(|r| r.count).max().unwrap_or(1).max(1);
        for entry in ranked {
            ui.horizontal(|ui| {
                let frac = entry.count as f32 / max as f32;
                bar(ui, theme, frac);
                ui.label(RichText::new(&entry.text).color(chrome::ink(theme)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(entry.count.to_string())
                            .size(11.0)
                            .color(chrome::ink_faint(theme)),
                    );
                });
            });
            ui.add_space(4.0);
        }
    });
}

/// A tiny horizontal fill bar (`frac` of full width, minimum 6.0), coloured
/// with the theme accent — a cheap, legible way to show relative counts.
fn bar(ui: &mut Ui, theme: &iris_overlay::Theme, frac: f32) {
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(28.0, 10.0), egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(3),
        chrome::ink_faint(theme).gamma_multiply(0.3),
    );
    let mut filled = rect;
    filled.set_width((rect.width() * frac.clamp(0.0, 1.0)).max(3.0));
    ui.painter()
        .rect_filled(filled, egui::CornerRadius::same(3), chrome::accent(theme));
}

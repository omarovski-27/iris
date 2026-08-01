//! The History tab: the recovery path. Every dictation, newest first, with a
//! one-click copy — a failed one is shown at least as readably as a
//! successful one, with the reason it failed front and centre.

use egui::{Color32, RichText, ScrollArea, TextEdit, Ui};

use crate::history::DictationRecord;
use crate::window::{Env, WindowState};

use super::chrome;

pub fn draw(ui: &mut Ui, state: &mut WindowState, env: &Env, theme: &iris_overlay::Theme) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("History")
                .size(20.0)
                .strong()
                .color(chrome::ink(theme)),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Refresh").clicked() {
                state.refresh(env, true);
            }
        });
    });
    ui.add_space(10.0);

    ui.horizontal(|ui| {
        ui.add(
            TextEdit::singleline(&mut state.search)
                .hint_text("Search dictations…")
                .desired_width(360.0),
        );
        if !state.search.is_empty() && ui.button("×").clicked() {
            state.search.clear();
        }
    });
    ui.add_space(12.0);

    // Matching lowercases every record it looks at, so it runs only when the
    // query or the log has actually moved — not on every repaint, which is
    // what typing in the box above causes.
    state.sync_filter();
    let matched = state.filtered().len();
    let count_label = if state.search.is_empty() {
        format!("{matched} dictation{}", if matched == 1 { "" } else { "s" })
    } else {
        format!("{matched} match{}", if matched == 1 { "" } else { "es" })
    };
    ui.label(
        RichText::new(count_label)
            .size(12.0)
            .color(chrome::ink_faint(theme)),
    );
    ui.add_space(6.0);

    let mut copy_action = None;
    if matched == 0 {
        chrome::card(theme).show(ui, |ui| {
            ui.label(
                RichText::new(if state.history.is_empty() {
                    "No dictations recorded yet. Hold the hotkey and speak, and it will show up here."
                } else {
                    "No dictations match that search."
                })
                .color(chrome::ink_dim(theme)),
            );
        });
    } else {
        let history = &state.history;
        let filtered = state.filtered();
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for record in filtered.iter().filter_map(|&i| history.get(i)) {
                    card(ui, theme, record, &mut copy_action);
                    ui.add_space(8.0);
                }
            });
    }

    if let Some(text) = copy_action {
        ui.ctx().copy_text(text);
        state.flash("Copied to clipboard");
    }
}

fn card(
    ui: &mut Ui,
    theme: &iris_overlay::Theme,
    record: &DictationRecord,
    copy_action: &mut Option<String>,
) {
    chrome::card(theme).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(friendly_timestamp(&record.timestamp))
                    .size(12.0)
                    .color(chrome::ink_dim(theme)),
            );
            engine_chip(ui, theme, &record.engine);
            status_chip(ui, theme, record);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Copy").clicked() {
                    *copy_action = Some(record.text.clone());
                }
                if let Some(ms) = record.latency.perceived_ms {
                    ui.label(
                        RichText::new(format!("{ms:.0} ms"))
                            .size(11.0)
                            .color(chrome::ink_faint(theme)),
                    );
                }
            });
        });

        ui.add_space(6.0);
        if record.text.is_empty() {
            ui.label(
                RichText::new("(no speech detected)")
                    .italics()
                    .color(chrome::ink_faint(theme)),
            );
        } else {
            ui.label(RichText::new(&record.text).color(chrome::ink(theme)));
        }

        if let Some(error) = &record.error {
            ui.add_space(6.0);
            ui.label(RichText::new(error).size(12.0).color(chrome::warn(theme)));
        }
    });
}

fn engine_chip(ui: &mut Ui, theme: &iris_overlay::Theme, engine: &str) {
    egui::Frame::new()
        .fill(chrome::ink_faint(theme).gamma_multiply(0.18))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(
                RichText::new(engine)
                    .size(11.0)
                    .color(chrome::ink_dim(theme)),
            );
        });
}

/// A small marker plus a label — painted rather than a text glyph, so it
/// never depends on the active font covering a checkmark/cross, and reads as
/// the same "coloured halo" language the pill's own state rings use.
///
/// Failure gets a filled *square* and bold text where the other two states
/// get a dot in regular weight. Colour alone would not carry it: this is the
/// recovery path, and the difference between "injected" and "failed" has to
/// survive a colour-blind reader and a bad monitor, not just the amber/mint
/// split.
fn status_chip(ui: &mut Ui, theme: &iris_overlay::Theme, record: &DictationRecord) {
    let (label, color, failed): (&str, Color32, bool) =
        if record.text.is_empty() && record.error.is_none() {
            ("idle", chrome::ink_faint(theme), false)
        } else if record.injected {
            ("injected", chrome::ok(theme), false)
        } else {
            ("failed", chrome::warn(theme), true)
        };
    let (marker, _response) = ui.allocate_exact_size(egui::vec2(9.0, 11.0), egui::Sense::hover());
    if failed {
        ui.painter().rect_filled(
            egui::Rect::from_center_size(marker.center(), egui::vec2(7.0, 7.0)),
            egui::CornerRadius::same(1),
            color,
        );
    } else {
        ui.painter().circle_filled(marker.center(), 3.0, color);
    }
    let text = RichText::new(label).size(11.0).color(color);
    ui.label(if failed { text.strong() } else { text });
}

/// `"2026-07-31T06:27:17Z"` -> `"2026-07-31 06:27:17 UTC"`.
fn friendly_timestamp(ts: &str) -> String {
    let body = ts.strip_suffix('Z').unwrap_or(ts);
    format!("{} UTC", body.replacen('T', " ", 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_timestamp_reads_naturally() {
        assert_eq!(
            friendly_timestamp("2026-07-31T06:27:17Z"),
            "2026-07-31 06:27:17 UTC"
        );
    }

    #[test]
    fn friendly_timestamp_tolerates_a_missing_z() {
        assert_eq!(
            friendly_timestamp("2026-07-31T06:27:17"),
            "2026-07-31 06:27:17 UTC"
        );
    }
}

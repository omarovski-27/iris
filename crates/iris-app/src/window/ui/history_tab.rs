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
        let edited = ui
            .add(
                TextEdit::singleline(&mut state.search)
                    .hint_text("Search dictations…")
                    .desired_width(360.0),
            )
            .changed();
        let cleared = !state.search.is_empty() && ui.button("×").clicked();
        if cleared {
            state.search.clear();
        }
        // A new result set is a new list; the pages the user opened on the
        // old one do not carry over.
        if edited || cleared {
            state.reset_paging();
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
        // An empty log has two quite different causes, and only one of them
        // is waiting to be filled: with `history.enabled = false` nothing
        // will ever be written, so telling the user to speak would leave
        // them waiting on the recovery path for entries that cannot come.
        let empty = match (state.history.is_empty(), state.config.history.enabled) {
            (false, _) => "No dictations match that search.",
            (true, true) => {
                "No dictations recorded yet. Hold the hotkey and speak, and it will show up here."
            }
            (true, false) => {
                "History logging is off, so nothing is being recorded. \
                 Set history.enabled = true in config.toml (Settings — Open config file), \
                 then pick “Reload settings” in the tray to start keeping dictations."
            }
        };
        chrome::card(theme).show(ui, |ui| {
            ui.label(RichText::new(empty).color(chrome::ink_dim(theme)));
        });
    } else {
        // Only the cards asked for are built: `egui` lays out every widget it
        // is handed whether or not it is on screen, and a card is a frame, a
        // shadow, two chips, a painted marker and a wrapped body. See
        // `WindowState::visible_count`.
        let mut show_more = false;
        let shown = state.visible_count();
        let history = &state.history;
        let filtered = state.filtered();
        let offset = env.utc_offset_seconds;
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for record in filtered.iter().take(shown).filter_map(|&i| history.get(i)) {
                    card(ui, theme, record, offset, &mut copy_action);
                    ui.add_space(8.0);
                }
                let older = filtered.len() - shown;
                if older > 0 {
                    ui.horizontal(|ui| {
                        if ui.button("Show more").clicked() {
                            show_more = true;
                        }
                        ui.label(
                            RichText::new(format!("{older} older not shown"))
                                .size(11.0)
                                .color(chrome::ink_faint(theme)),
                        );
                    });
                }
            });
        if show_more {
            state.show_more();
        }
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
    utc_offset_seconds: i32,
    copy_action: &mut Option<String>,
) {
    chrome::card(theme).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(friendly_timestamp(&record.timestamp, utc_offset_seconds))
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

/// `"2026-07-31T06:27:17Z"` -> `"2026-07-31 08:27:17"` two hours east of UTC.
///
/// The log stamps UTC because reading the local offset from a multi-threaded
/// process is unsound (see `crate::history`), so the shift belongs here, on
/// the offset `crate::window::shell` asked Windows for once — the same one
/// Insights uses for "today". Without it the newest card can read as
/// yesterday evening for anyone far enough west.
///
/// A stamp that will not parse is shown as it was written and labelled UTC,
/// rather than moved by an offset that may not apply to it.
fn friendly_timestamp(ts: &str, utc_offset_seconds: i32) -> String {
    let parsed = time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339);
    let offset = time::UtcOffset::from_whole_seconds(utc_offset_seconds);
    match (parsed, offset) {
        (Ok(at), Ok(offset)) => {
            let local = at.to_offset(offset);
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                local.year(),
                u8::from(local.month()),
                local.day(),
                local.hour(),
                local.minute(),
                local.second(),
            )
        }
        _ => format!(
            "{} UTC",
            ts.strip_suffix('Z').unwrap_or(ts).replacen('T', " ", 1)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_timestamp_reads_naturally_in_local_time() {
        assert_eq!(
            friendly_timestamp("2026-07-31T06:27:17Z", 0),
            "2026-07-31 06:27:17"
        );
        assert_eq!(
            friendly_timestamp("2026-07-31T06:27:17Z", 2 * 3600),
            "2026-07-31 08:27:17"
        );
    }

    /// The case the UTC label was actively wrong for: far enough west and the
    /// morning stamp belongs to the previous local day.
    #[test]
    fn friendly_timestamp_crosses_the_date_line_westwards() {
        assert_eq!(
            friendly_timestamp("2026-07-31T06:27:17Z", -8 * 3600),
            "2026-07-30 22:27:17"
        );
    }

    #[test]
    fn friendly_timestamp_tolerates_a_missing_z() {
        assert_eq!(
            friendly_timestamp("2026-07-31T06:27:17", 2 * 3600),
            "2026-07-31 06:27:17 UTC"
        );
    }

    /// An offset no `UtcOffset` can hold must not panic or silently shift the
    /// stamp by something else.
    #[test]
    fn friendly_timestamp_falls_back_on_an_impossible_offset() {
        assert_eq!(
            friendly_timestamp("2026-07-31T06:27:17Z", 99 * 3600),
            "2026-07-31 06:27:17 UTC"
        );
    }
}

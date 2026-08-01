//! The window's view: plain `egui`, no `eframe`, no OS calls — see the
//! module docs on [`crate::window`] for why that split exists. Everything
//! here type-checks on every platform; [`crate::window::shell`] is the only
//! `cfg(windows)` piece, and it does nothing but bootstrap `eframe` and call
//! [`draw_root`] once a frame.

pub mod chrome;
mod history_tab;
mod insights_tab;
mod settings_tab;

use egui::{Align, CentralPanel, Color32, Frame, Layout, RichText};

use crate::pill::overlay_theme;

use super::{egui_theme, Env, Tab, WindowState};

/// Draw one frame of the whole window.
///
/// Order: pick up a pending "focus me" request from a second `open()` call,
/// refresh from disk on the state's own timer, apply the theme, then paint
/// the background wash, the nav sidebar and the active tab.
pub fn draw_root(ctx: &egui::Context, state: &mut WindowState, env: &Env) {
    while env.reopen_signal.try_recv().is_ok() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
    state.refresh(env, false);
    // A dictation can land while the window is open; keep the view live
    // without the user having to touch anything.
    ctx.request_repaint_after(std::time::Duration::from_millis(500));

    let theme = overlay_theme(state.config.theme);
    ctx.set_visuals(egui_theme::visuals(&theme));
    chrome::background_wash(
        &ctx.layer_painter(egui::LayerId::background()),
        &theme,
        ctx.screen_rect(),
    );

    egui::SidePanel::left("iris_window_nav")
        .resizable(false)
        .exact_width(176.0)
        .frame(Frame::new().inner_margin(egui::Margin::symmetric(12, 16)))
        .show(ctx, |ui| nav(ui, state, env, &theme));

    egui::SidePanel::left("iris_window_divider")
        .resizable(false)
        .exact_width(2.0)
        .frame(Frame::new())
        .show(ctx, |ui| {
            chrome::spectrum_bar(ui.painter(), &theme, ui.max_rect());
        });

    // Before the `CentralPanel`, not after: egui lays panels out in the order
    // they are added and the central panel claims whatever is left, so a
    // bottom panel added afterwards paints over the last history card instead
    // of reserving a strip under it.
    if let Some(status) = state.status_text().map(str::to_string) {
        egui::TopBottomPanel::bottom("iris_window_status")
            .frame(Frame::new().inner_margin(egui::Margin::symmetric(24, 8)))
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(status)
                        .color(chrome::ink_dim(&theme))
                        .size(12.0),
                );
            });
    }

    CentralPanel::default()
        .frame(
            Frame::new()
                .inner_margin(egui::Margin::symmetric(24, 20))
                .fill(Color32::TRANSPARENT),
        )
        .show(ctx, |ui| match state.tab {
            Tab::History => history_tab::draw(ui, state, env, &theme),
            Tab::Settings => settings_tab::draw(ui, state, env, &theme),
            Tab::Insights => insights_tab::draw(ui, state, &theme),
        });
}

/// The left-hand section picker: History, Settings, Insights.
fn nav(ui: &mut egui::Ui, state: &mut WindowState, env: &Env, theme: &iris_overlay::Theme) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Iris")
                .size(18.0)
                .strong()
                .color(chrome::ink(theme)),
        );
    });
    ui.add_space(18.0);

    for tab in Tab::ALL {
        let selected = state.tab == tab;
        let text = RichText::new(tab.label()).size(14.5).color(if selected {
            chrome::ink(theme)
        } else {
            chrome::ink_dim(theme)
        });

        let button = egui::Button::new(text)
            .frame(true)
            .corner_radius(egui::CornerRadius::same(9))
            .min_size(egui::vec2(ui.available_width(), 32.0))
            .fill(if selected {
                chrome::accent(theme).gamma_multiply(0.22)
            } else {
                Color32::TRANSPARENT
            })
            .stroke(if selected {
                egui::Stroke::new(1.0_f32, chrome::accent(theme))
            } else {
                egui::Stroke::NONE
            });

        if ui.add(button).clicked() {
            state.tab = tab;
        }
        ui.add_space(4.0);
    }

    // The hint names the key that works *right now*, which is the one the
    // hook was installed with — not the saved one. After a rebind those
    // differ until a restart, and a footer that quietly switched to the new
    // key would be telling the user to hold something inert.
    let pending = env
        .restart_pending(&state.config)
        .hotkey
        .then(|| state.config.hotkey);
    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
        ui.add_space(4.0);
        if let Some(saved) = pending {
            ui.label(
                RichText::new(format!("{saved} after restart"))
                    .size(11.0)
                    .color(chrome::warn(theme)),
            );
        }
        ui.label(
            RichText::new(format!("hold {} to dictate", env.in_force_hotkey))
                .size(11.0)
                .color(chrome::ink_faint(theme)),
        );
    });
}

#[cfg(test)]
mod tests {
    // The view itself has no independent unit tests: it is exercised by
    // `crate::window::state`'s tests (the data it renders) and by eye —
    // rendered evidence lives in the PR, and `iris_window_demo.rs` is the
    // manual verification path, the same split `iris-overlay` uses for its
    // window shell.
}

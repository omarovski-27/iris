//! The window's view: plain `egui`, no `eframe`, no OS calls — see the
//! module docs on [`crate::window`] for why that split exists. Everything
//! here type-checks on every platform; `crate::window::shell` is the only
//! `cfg(windows)` piece, and it does nothing but bootstrap `eframe` and call
//! [`draw_root`] once a frame.
//!
//! The view has no independent unit tests: it is exercised by
//! `crate::window::state`'s tests (the data it renders) and by eye — rendered
//! evidence lives in the PR, and `iris --demo-window` (see `main.rs`) is the
//! manual verification path, the same split `iris-overlay` uses for its own
//! window shell.

pub mod chrome;
mod history_tab;
mod insights_tab;
mod settings_tab;

use std::time::Duration;

use egui::{Align, CentralPanel, Color32, Frame, Layout, RichText};

use crate::pill::overlay_theme;

use super::state::REFRESH_INTERVAL;
use super::{egui_theme, Env, StatusLevel, Tab, WindowState};

/// How soon to come back while the loop still owes an answer to a setting
/// change. Short, because it is the one thing the user is waiting on, and
/// needed at all because the loop applies commands between dictations — the
/// answer can be a whole utterance away.
const AWAITING_LOOP_POLL: Duration = Duration::from_millis(100);

/// Draw one frame of the whole window.
///
/// Order: pick up a pending "focus me" request from a second `open()` call,
/// take in what the loop did with the changes already sent, refresh from disk
/// on the state's own timer, apply the theme, then paint the background wash,
/// the nav sidebar and the active tab.
///
/// The reopen answer un-minimizes before it focuses, and the order is
/// load-bearing: winit declines to focus a minimized window outright, so
/// `Focus` alone would drain the signal and do nothing at all — the tray's
/// `Settings` item would read as dead until the user restored the window by
/// hand. Both commands run on the event loop thread in the order they are
/// queued, so the restore has already landed when the focus is attempted.
pub fn draw_root(ctx: &egui::Context, state: &mut WindowState, env: &Env) {
    while env.reopen_signal.try_recv().is_ok() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
    state.poll_outcomes(env);
    state.refresh(env, false);
    // A dictation can land while the window is open; keep the view live
    // without the user having to touch anything — at the rate the state can
    // actually move and no faster, since `refresh` is what re-reads the config
    // and the log and it does nothing in between. The exception is an answer
    // the loop still owes, which arrives on its own schedule.
    ctx.request_repaint_after(if state.awaiting_loop() {
        AWAITING_LOOP_POLL
    } else {
        REFRESH_INTERVAL
    });

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
    if let Some((status, level)) = state
        .status_flash()
        .map(|(message, level)| (message.to_string(), level))
    {
        // A failure — a change the loop never received — is the one status
        // the user has to act on, so it does not read like "Saved" in grey.
        let color = match level {
            StatusLevel::Info => chrome::ink_dim(&theme),
            StatusLevel::Warn => chrome::warn(&theme),
        };
        egui::TopBottomPanel::bottom("iris_window_status")
            .frame(Frame::new().inner_margin(egui::Margin::symmetric(24, 8)))
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.label(RichText::new(status).color(color).size(12.0));
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
        .then_some(state.config.hotkey);
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
            RichText::new(format!("hold {} to dictate", env.hotkey.running))
                .size(11.0)
                .color(chrome::ink_faint(theme)),
        );
    });
}

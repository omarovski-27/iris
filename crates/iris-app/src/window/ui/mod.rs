//! The window's view: plain `egui`, no `eframe`, no OS calls — see the
//! module docs on [`crate::window`] for why that split exists. Everything
//! here type-checks on every platform; `crate::window::shell` is the only
//! `cfg(windows)` piece, and it does nothing but bootstrap `eframe` and call
//! [`draw_root`] once a frame.
//!
//! The view has essentially no independent unit tests: it is exercised by
//! `crate::window::state`'s tests (the data it renders) and by eye — rendered
//! evidence lives in the PR, and `iris --demo-window` (see `main.rs`) is the
//! manual verification path, the same split `iris-overlay` uses for its own
//! window shell. The exceptions are
//! `tests::every_glyph_in_the_view_is_covered_by_the_embedded_fonts`, which
//! catches the single thing eyeballing the source reliably misses, and
//! `tests::closing_the_window_hides_it_and_leaves_the_app_running`, which
//! drives [`draw_root`] through a headless `egui::Context` to prove the
//! close-button wiring without a real OS window — see that test's doc
//! comment for exactly what it does and does not prove.

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
/// Order: hide the window if this frame is its close being requested, pick up
/// a pending "focus me" request from a second `open()` call, take in what the
/// loop did with the changes already sent, refresh from disk on the state's
/// own timer, apply the theme, then paint the background wash, the nav
/// sidebar and the active tab.
///
/// **The window's close hides it and leaves Iris running, the way Wispr Flow
/// does — it does not quit the app.** `X`, Alt+F4, and the taskbar's "Close
/// window" all reach here as the same `close_requested` signal on the root
/// viewport (there is exactly one), so all three take the same path: `egui`
/// is told to cancel the close outright (`ViewportCommand::CancelClose`, or
/// the window — and the whole process, since this is the root viewport —
/// would actually tear down) and the window is hidden instead
/// (`ViewportCommand::Visible(false)`). The dictation loop is never told
/// about this at all; the hotkey, the tray and dictation itself keep running
/// exactly as if the window had never been open. An earlier revision showed
/// a one-time "still running" notification the first time this happened;
/// removed 2026-08-12 at the captain's request (see `AGENTS.md`) — the hide
/// itself is unchanged, only the popup is gone.
///
/// This reverses part of an earlier fix that made every one of these three
/// gestures quit the whole app (see `AGENTS.md`'s "Every close path..."
/// entry). That fix was built on a misdiagnosis: the maintainer's report of
/// "cannot be closed" was actually about a *freeze* on close (fixed
/// separately — see `App::capture`'s Quit-during-finalise handling), not
/// about wanting the app to exit. Hiding to the tray was the correct
/// behaviour the whole time; [`Env::request_quit`] and `flip_quit_flag`
/// are unchanged and still exist for the tray's own Quit item, which is the
/// one place that must still end the process.
///
/// The reopen answer makes the window visible and un-minimizes before it
/// focuses, and the order is load-bearing: winit declines to focus a hidden
/// or minimized window outright, so `Focus` alone would drain the signal and
/// do nothing at all — the tray's `Settings` item would read as dead until
/// the user found the window some other way. All three commands run on the
/// event loop thread in the order they are queued, so each has already landed
/// when the next is attempted.
pub fn draw_root(ctx: &egui::Context, state: &mut WindowState, env: &Env) {
    if ctx.input(|i| i.viewport().close_requested()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
    while env.reopen_signal.try_recv().is_ok() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
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

#[cfg(test)]
mod tests {
    /// Every non-ASCII character this window can paint has to exist in the
    /// fonts `egui` embeds.
    ///
    /// There is no safety net under this one. The fonts are compiled into
    /// `iris.exe` (`egui` ships Ubuntu-Light plus two emoji faces and Iris
    /// installs none of its own), so there is no system font to fall back to
    /// and no substitution step: a codepoint none of the three covers is
    /// painted as a tofu box, identically on every machine. It also survives
    /// every kind of review that reads the *source*, where the character
    /// looks perfectly ordinary — a `→` in the History empty-state message
    /// shipped exactly that way, and only a rendered screenshot caught it.
    ///
    /// Whole source files rather than a hand-listed set of strings, so this
    /// keeps holding as copy is added: anything outside a comment is a
    /// candidate for the screen. A comment is skipped because none of it is.
    #[test]
    fn every_glyph_in_the_view_is_covered_by_the_embedded_fonts() {
        const SOURCES: &[(&str, &str)] = &[
            ("window/ui/mod.rs", include_str!("mod.rs")),
            ("window/ui/chrome.rs", include_str!("chrome.rs")),
            ("window/ui/history_tab.rs", include_str!("history_tab.rs")),
            ("window/ui/settings_tab.rs", include_str!("settings_tab.rs")),
            ("window/ui/insights_tab.rs", include_str!("insights_tab.rs")),
            // Not a tab, but where the status-line messages are written.
            ("window/state.rs", include_str!("../state.rs")),
        ];

        let ctx = egui::Context::default();
        // Fonts do not exist until a frame has run.
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        // Every size/family the window can ask for, since coverage is per
        // font family and the fallback chain differs between them.
        let fonts: Vec<egui::FontId> = ctx.style().text_styles.values().cloned().collect();

        let mut missing = Vec::new();
        for (file, source) in SOURCES {
            for (line, text) in source.lines().enumerate() {
                if text.trim_start().starts_with("//") {
                    continue;
                }
                for c in text.chars().filter(|c| !c.is_ascii()) {
                    if fonts
                        .iter()
                        .any(|font| !ctx.fonts(|f| f.has_glyph(font, c)))
                    {
                        missing.push(format!("{file}:{} '{c}' (U+{:04X})", line + 1, c as u32));
                    }
                }
            }
        }
        missing.dedup();
        assert!(
            missing.is_empty(),
            "these would render as tofu boxes; use a covered character instead \
             (·, ×, —, …, the curly quotes and the whole Latin-1 range are covered):\n  {}",
            missing.join("\n  ")
        );
    }

    /// Regression for the captain's 2026-08-11 request: closing the settings
    /// window ("click the X button") must leave Iris running in the tray,
    /// the way Wispr Flow behaves, rather than quitting the whole app (which
    /// is what an earlier fix, in response to a different report, made every
    /// close path do — see `draw_root`'s doc comment for the full story).
    /// `draw_root` is portable `egui`, no `eframe` window required, so this
    /// drives it through a headless `egui::Context::run` with a synthetic
    /// `close_requested` on the root viewport — the same signal `winit`
    /// reports for the window's `X`, Alt+F4, and the taskbar's "Close
    /// window" alike (confirmed by reading `winit` 0.30's own
    /// `WM_CLOSE`/`WM_SYSCOMMAND` handling; there is no Windows host here to
    /// click any of them for real) — and checks that it cancels the close,
    /// hides the viewport, sends no command, and never touches `Command::Quit`
    /// or the quit flag.
    ///
    /// What this does **not** prove: that a real `eframe`/`winit` window on
    /// real Windows actually reports `close_requested` for all three
    /// gestures, or that hiding via `ViewportCommand::Visible(false)` after
    /// `CancelClose` really keeps the native window alive rather than
    /// destroying it — that half is reasoned from `eframe`'s own source (see
    /// `window::shell`'s module docs), not exercised end to end, because
    /// there is no Windows host here to open a real window on.
    #[test]
    fn closing_the_window_hides_it_and_leaves_the_app_running() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        use crate::window::{Env, InForce, WindowState};

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let (_outcomes_tx, outcomes_rx) = crossbeam_channel::unbounded();
        let reopen_signal: crossbeam_channel::Receiver<()> = crossbeam_channel::never();
        let devices = || Vec::new();
        let open_config_file = |_: &std::path::Path| Ok(());
        let quit_flag = Arc::new(AtomicBool::new(false));

        let env = Env {
            config_path: &config_path,
            commands: &commands_tx,
            outcomes: &outcomes_rx,
            list_devices: &devices,
            open_config_file: &open_config_file,
            reopen_signal: &reopen_signal,
            utc_offset_seconds: 0,
            hotkey: InForce {
                running: iris_core::hotkey::Key::default(),
                at_startup: iris_core::hotkey::Key::default(),
            },
            overlay_enabled: InForce {
                running: true,
                at_startup: true,
            },
            quit_flag: Arc::clone(&quit_flag),
            balance: &crate::balance::BalanceView::default,
            refresh_balance: &|| {},
        };
        let mut state = WindowState::new(&env);

        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                events: vec![egui::ViewportEvent::Close],
                ..Default::default()
            },
        );
        let output = ctx.run(input, |ctx| super::draw_root(ctx, &mut state, &env));

        assert!(
            !quit_flag.load(Ordering::Acquire),
            "closing the window must not flip the quit flag — it only hides"
        );
        assert_eq!(
            commands_rx.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty),
            "closing the window must not send any command, especially not Quit"
        );
        let root_commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(
            root_commands.contains(&egui::ViewportCommand::CancelClose),
            "the close must be cancelled or the root viewport — the whole \
             process — would actually tear down"
        );
        assert!(
            root_commands.contains(&egui::ViewportCommand::Visible(false)),
            "and the window hidden instead"
        );
    }
}

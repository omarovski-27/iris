//! The Settings tab: everything the tray can change, plus hotkey rebinding
//! and the overlay toggle, all written through [`crate::config::Config`] via
//! the same [`crate::app::Command`]s the tray sends — see the `state`
//! module's docs for why the window never writes `config.toml` itself.
//!
//! Deliberately absent: API keys. `config.rs`'s redaction discipline is
//! load-bearing, and the product brief only asks for engine, device, theme,
//! polish, the overlay toggle and hotkey rebinding — so this tab has no
//! control that could ever put a key value in a text widget. What it does
//! offer is "Open config file", which hands the file itself to the user's
//! editor: the keys stay hand-edited, exactly as they were when the tray's
//! `Settings` item opened the file directly, and nothing in this process
//! ever reads one back to render it.
//!
//! [`balance_section`] is the one card that shows a number derived from a
//! key rather than nothing at all — the remaining Deepgram balance, never
//! the `deepgram_management` key itself, which stays exactly as unreadable
//! here as every other key. See `crate::balance`'s module docs.

use egui::{RichText, Ui};
use iris_core::hotkey::Key;

use crate::balance::BalanceView;
use crate::config::{EngineChoice, Theme};
use crate::window::{Env, WindowState};

use super::chrome;
use super::history_tab::friendly_timestamp;

pub fn draw(ui: &mut Ui, state: &mut WindowState, env: &Env, theme: &iris_overlay::Theme) {
    ui.label(
        RichText::new("Settings")
            .size(20.0)
            .strong()
            .color(chrome::ink(theme)),
    );
    ui.add_space(14.0);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            dictation_section(ui, state, env, theme);
            ui.add_space(12.0);
            vocabulary_section(ui, state, env, theme);
            ui.add_space(12.0);
            appearance_section(ui, state, env, theme);
            ui.add_space(12.0);
            cleanup_section(ui, state, env, theme);
            ui.add_space(12.0);
            overlay_section(ui, state, env, theme);
            ui.add_space(12.0);
            balance_section(ui, env, theme);
            ui.add_space(12.0);
            config_file_section(ui, state, env, theme);
        });
}

fn dictation_section(ui: &mut Ui, state: &mut WindowState, env: &Env, theme: &iris_overlay::Theme) {
    chrome::card(theme).show(ui, |ui| {
        chrome::section_label(ui, theme, "Dictation");
        ui.add_space(8.0);

        labeled_row(ui, theme, "Engine", |ui| {
            egui::ComboBox::from_id_salt("iris_settings_engine")
                .selected_text(state.config.engine.label())
                .show_ui(ui, |ui| {
                    for choice in EngineChoice::ALL {
                        let selected = state.config.engine == *choice;
                        if ui.selectable_label(selected, choice.label()).clicked() && !selected {
                            state.set_engine(env, *choice);
                        }
                    }
                });
        });

        let hotkey_pending = env.restart_pending(&state.config).hotkey;
        labeled_row(ui, theme, "Hotkey", |ui| {
            egui::ComboBox::from_id_salt("iris_settings_hotkey")
                .selected_text(state.config.hotkey.label())
                .show_ui(ui, |ui| {
                    for key in Key::ALL {
                        let selected = state.config.hotkey == *key;
                        if ui.selectable_label(selected, key.label()).clicked() && !selected {
                            state.set_hotkey(env, *key);
                        }
                    }
                });
            if hotkey_pending {
                restart_pending(ui, theme, &format!("{} until restart", env.hotkey.running));
            }
        });
        ui.add_space(2.0);
        caption(
            ui,
            theme,
            "Held, not tapped. Changing this needs a restart of Iris.",
        );

        ui.add_space(8.0);
        labeled_row(ui, theme, "Microphone", |ui| {
            let current = state.config.audio.device.clone();
            let label = current.as_deref().unwrap_or("System default");
            egui::ComboBox::from_id_salt("iris_settings_device")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(current.is_none(), "System default")
                        .clicked()
                        && current.is_some()
                    {
                        state.set_device(env, None);
                    }
                    for name in state.devices.clone() {
                        let selected = current.as_deref() == Some(name.as_str());
                        if ui.selectable_label(selected, &name).clicked() && !selected {
                            state.set_device(env, Some(name));
                        }
                    }
                });
            if ui
                .small_button("Refresh")
                .on_hover_text("Refresh device list")
                .clicked()
            {
                state.refresh_devices(env);
            }
        });
    });
}

/// Names, jargon and acronyms Iris should listen for. See
/// `crate::app::Command::SetVocabulary` for how a save reaches the engine and
/// `iris_core::engine::EngineOptions::vocabulary` for what each engine does
/// with the list.
fn vocabulary_section(
    ui: &mut Ui,
    state: &mut WindowState,
    env: &Env,
    theme: &iris_overlay::Theme,
) {
    state.sync_vocabulary_input();
    chrome::card(theme).show(ui, |ui| {
        chrome::section_label(ui, theme, "Vocabulary");
        ui.add_space(8.0);
        caption(
            ui,
            theme,
            "Names, jargon and acronyms Iris often mishears. One per line — sent to the \
             transcription engine as a hint, not forced into the transcript. A very long list \
             is trimmed to whatever the engine allows.",
        );
        ui.add_space(6.0);
        ui.add(
            egui::TextEdit::multiline(&mut state.vocabulary_input)
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text("Deepgram\nZipformer\nWhisper.cpp"),
        );
        ui.add_space(6.0);
        let dirty = state.vocabulary_dirty();
        ui.horizontal(|ui| {
            if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
                state.set_vocabulary(env);
            }
            let count = state.config.vocabulary.len();
            if count > 0 {
                ui.label(
                    RichText::new(format!(
                        "{count} term{} active",
                        if count == 1 { "" } else { "s" }
                    ))
                    .size(11.0)
                    .color(chrome::ink_faint(theme)),
                );
            }
        });
    });
}

fn appearance_section(
    ui: &mut Ui,
    state: &mut WindowState,
    env: &Env,
    theme: &iris_overlay::Theme,
) {
    chrome::card(theme).show(ui, |ui| {
        chrome::section_label(ui, theme, "Appearance");
        ui.add_space(8.0);
        labeled_row(ui, theme, "Theme", |ui| {
            for (choice, label) in [
                (Theme::Dark, "Dark · Prism"),
                (Theme::Light, "Light · Porcelain"),
            ] {
                let selected = state.config.theme == choice;
                if ui.selectable_label(selected, label).clicked() && !selected {
                    state.set_theme(env, choice);
                }
            }
        });
    });
}

fn cleanup_section(ui: &mut Ui, state: &mut WindowState, env: &Env, theme: &iris_overlay::Theme) {
    chrome::card(theme).show(ui, |ui| {
        chrome::section_label(ui, theme, "Cleanup");
        ui.add_space(8.0);
        let mut enabled = state.config.polish.enabled;
        if ui
            .checkbox(&mut enabled, "Polish transcripts before inserting them")
            .changed()
        {
            state.set_polish(env, enabled);
        }
        caption(
            ui,
            theme,
            "Filler words and false starts removed; punctuation and casing cleaned up.",
        );
    });
}

fn overlay_section(ui: &mut Ui, state: &mut WindowState, env: &Env, theme: &iris_overlay::Theme) {
    chrome::card(theme).show(ui, |ui| {
        chrome::section_label(ui, theme, "Overlay");
        ui.add_space(8.0);
        let mut enabled = state.config.overlay_enabled;
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut enabled, "Show the pill overlay while dictating")
                .changed()
            {
                state.set_overlay_enabled(env, enabled);
            }
            // Two different things a ticked box can be hiding, told apart by
            // the same in-force notion: a toggle waiting on a restart, and an
            // overlay that was asked for and is not up — `main` falls back to
            // a silent pill when the spawn fails, which the box alone would
            // report as a working overlay.
            let saved = state.config.overlay_enabled;
            if env.overlay_enabled.pending(&saved) {
                let running = if env.overlay_enabled.running {
                    "shown"
                } else {
                    "hidden"
                };
                restart_pending(ui, theme, &format!("{running} until restart"));
            } else if env.overlay_enabled.diverged(&saved) {
                restart_pending(ui, theme, "not running this session");
            }
        });
        caption(ui, theme, "The small on-screen indicator that appears while you hold the hotkey. Changing this needs a restart of Iris.");
    });
}

/// The optional Deepgram balance readout — see `crate::balance`'s module
/// docs for the background fetch this only ever reads a cached view of, and
/// `crate::config::Keys::deepgram_management` for how a user opts in. Never
/// more than this one card: no history, no chart, no spend-rate projection —
/// the brief this was built from is explicit that this is a readout, not a
/// billing dashboard.
fn balance_section(ui: &mut Ui, env: &Env, theme: &iris_overlay::Theme) {
    let view = (env.balance)();
    chrome::card(theme).show(ui, |ui| {
        chrome::section_label(ui, theme, "Deepgram balance");
        ui.add_space(8.0);

        if !view.configured {
            caption(
                ui,
                theme,
                "Optional: add a deepgram_management key to your config file's [keys] table to \
                 see your remaining Deepgram balance here, with a warning before it runs out. See \
                 the repo README for how to create one.",
            );
            return;
        }

        match balance_line(&view) {
            Some((text, warn)) => {
                let text = RichText::new(text).size(14.0);
                ui.label(if warn {
                    text.strong().color(chrome::warn(theme))
                } else {
                    text.color(chrome::ink(theme))
                });
            }
            None => caption(ui, theme, "Checking…"),
        }

        ui.horizontal(|ui| {
            if let Some(checked_at) = &view.checked_at {
                caption(
                    ui,
                    theme,
                    &format!(
                        "Checked {}{}",
                        friendly_timestamp(checked_at, env.utc_offset_seconds),
                        if view.check_failed {
                            " (last check failed — showing the last known balance)"
                        } else {
                            ""
                        },
                    ),
                );
            }
            if ui.small_button("Refresh").clicked() {
                (env.refresh_balance)();
            }
        });
    });
}

/// The main balance line's text and whether it should read as a warning —
/// `None` before the very first fetch has finished, so the view can show
/// "Checking…" instead of a blank line.
fn balance_line(view: &BalanceView) -> Option<(String, bool)> {
    match view.amount {
        Some(amount) => {
            let warn = amount <= crate::balance::LOW_BALANCE_THRESHOLD_USD;
            Some((crate::balance::format_amount(amount, &view.units), warn))
        }
        None if view.check_failed => Some(("Balance unknown (check failed)".to_string(), true)),
        None => None,
    }
}

/// The one place this window points at `config.toml` itself.
///
/// A direct OS action rather than a [`crate::app::Command`]: nothing is read
/// back and no state changes here, so there is nothing for `App` — the sole
/// config writer — to arbitrate. The next refresh picks up whatever the user
/// saved, the same way it picks up an edit made outside Iris entirely.
fn config_file_section(
    ui: &mut Ui,
    state: &mut WindowState,
    env: &Env,
    theme: &iris_overlay::Theme,
) {
    chrome::card(theme).show(ui, |ui| {
        chrome::section_label(ui, theme, "Config file");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Open config file").clicked() {
                state.open_config_file(env);
            }
            ui.label(
                RichText::new(env.config_path.display().to_string())
                    .size(11.0)
                    .color(chrome::ink_faint(theme)),
            );
        });
        ui.add_space(2.0);
        caption(
            ui,
            theme,
            "Everything above, plus the settings this window does not show — API keys most of \
             all, which Iris never displays. Edited by hand; changes are picked up here within \
             a couple of seconds.",
        );
    });
}

/// A short, always-visible "this is not live yet" marker beside a control
/// whose saved value has outrun the running one. Not a tooltip: the whole
/// problem it exists to fix is a difference the user cannot see.
fn restart_pending(ui: &mut Ui, theme: &iris_overlay::Theme, text: &str) {
    ui.label(
        RichText::new(text)
            .size(11.0)
            .strong()
            .color(chrome::warn(theme)),
    );
}

fn labeled_row(
    ui: &mut Ui,
    theme: &iris_overlay::Theme,
    label: &str,
    add_control: impl FnOnce(&mut Ui),
) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [110.0, ui.spacing().interact_size.y],
            egui::Label::new(RichText::new(label).color(chrome::ink_dim(theme))),
        );
        add_control(ui);
    });
}

fn caption(ui: &mut Ui, theme: &iris_overlay::Theme, text: &str) {
    ui.label(
        RichText::new(text)
            .size(11.0)
            .color(chrome::ink_faint(theme)),
    );
}

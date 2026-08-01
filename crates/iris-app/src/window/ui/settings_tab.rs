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

use egui::{RichText, Ui};
use iris_core::hotkey::Key;

use crate::config::{EngineChoice, Theme};
use crate::window::{Env, WindowState};

use super::chrome;

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
            appearance_section(ui, state, env, theme);
            ui.add_space(12.0);
            cleanup_section(ui, state, env, theme);
            ui.add_space(12.0);
            overlay_section(ui, state, env, theme);
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

        let hotkey_pending = state.config.hotkey != env.in_force_hotkey;
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
                restart_pending(ui, theme, &format!("{} until restart", env.in_force_hotkey));
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
                .checkbox(&mut enabled, "Show the live-text pill while dictating")
                .changed()
            {
                state.set_overlay_enabled(env, enabled);
            }
            if state.config.overlay_enabled != env.in_force_overlay_enabled {
                let running = if env.in_force_overlay_enabled {
                    "shown"
                } else {
                    "hidden"
                };
                restart_pending(ui, theme, &format!("{running} until restart"));
            }
        });
        caption(ui, theme, "The small on-screen indicator that appears while you hold the hotkey. Changing this needs a restart of Iris.");
    });
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

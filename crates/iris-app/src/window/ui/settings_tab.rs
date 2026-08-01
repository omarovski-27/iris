//! The Settings tab: everything the tray can change, plus hotkey rebinding
//! and the overlay toggle, all written through [`crate::config::Config`] via
//! the same [`crate::app::Command`]s the tray sends — see the `state`
//! module's docs for why the window never writes `config.toml` itself.
//!
//! Deliberately absent: API keys. `config.rs`'s redaction discipline is
//! load-bearing, and the product brief only asks for engine, device, theme,
//! polish, the overlay toggle and hotkey rebinding — so this tab has no
//! control that could ever put a key value in a text widget.

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
        if ui.checkbox(&mut enabled, "Show the live-text pill while dictating").changed() {
            state.set_overlay_enabled(env, enabled);
        }
        caption(ui, theme, "The small on-screen indicator that appears while you hold the hotkey. Changing this needs a restart of Iris.");
    });
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

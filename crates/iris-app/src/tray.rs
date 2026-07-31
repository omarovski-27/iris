//! The tray icon and its menu.
//!
//! # Why `tray-icon`
//!
//! It is the maintained extraction of Tauri's tray implementation (same authors
//! as `muda`, which provides the menu), it is pure Rust over Win32 — no C
//! toolchain — so it cross-compiles to `x86_64-pc-windows-gnu`, which is how
//! this project builds from WSL, and it does not drag in a windowing framework.
//! The alternatives were `systray` (unmaintained since 2021), `trayicon`
//! (Windows-only and thinner, no submenus) and pulling in `tao`/`winit` purely
//! for their event loop, which would be a UI framework in a crate whose brief
//! says the tray is the only UI.
//!
//! # Threading
//!
//! Win32 delivers tray notifications to the thread that created the icon, so
//! the tray owns a thread and does nothing on it but pump messages — the same
//! discipline the hotkey hook needs, for the same reason. Menu clicks arrive on
//! `muda`'s global channel; [`command_for`] turns a menu id into a
//! [`Command`] and the loop thread applies it.
//!
//! # Non-Windows
//!
//! [`spawn`] is a no-op that returns a dead command channel, so `main` compiles
//! and runs on Linux with the hotkey and the tray simply absent. `tray-icon`
//! needs GTK on Linux, which would put a system dependency on a crate that is
//! otherwise CI-testable anywhere — not worth it for a Windows-first product.

use crossbeam_channel::Receiver;

use crate::app::Command;
use crate::config::{Config, EngineChoice, Theme};

/// Menu ids. Strings rather than integers because `muda` ids are strings and
/// because a crash report containing `engine:deepgram` explains itself.
const ID_POLISH: &str = "polish";
const ID_SETTINGS: &str = "settings";
const ID_RELOAD: &str = "reload";
const ID_QUIT: &str = "quit";
const ENGINE_PREFIX: &str = "engine:";
const DEVICE_PREFIX: &str = "device:";
const THEME_PREFIX: &str = "theme:";
/// The device menu entry that means "whatever Windows is using".
const DEVICE_DEFAULT: &str = "device:*default*";

/// Translate a clicked menu id into a [`Command`].
///
/// Pure, and therefore the part of the tray that CI can test: the Windows half
/// below is only the plumbing that produces these ids and delivers the result.
/// `polish_now` is the current setting, because the check item reports the
/// state it is *leaving*.
pub fn command_for(id: &str, polish_now: bool) -> Option<Command> {
    match id {
        ID_QUIT => Some(Command::Quit),
        ID_SETTINGS => Some(Command::OpenSettings),
        ID_RELOAD => Some(Command::Reload),
        ID_POLISH => Some(Command::SetPolish(!polish_now)),
        DEVICE_DEFAULT => Some(Command::SetDevice(None)),
        _ => {
            if let Some(name) = id.strip_prefix(ENGINE_PREFIX) {
                return name.parse::<EngineChoice>().ok().map(Command::SetEngine);
            }
            if let Some(name) = id.strip_prefix(DEVICE_PREFIX) {
                return Some(Command::SetDevice(Some(name.to_string())));
            }
            if let Some(name) = id.strip_prefix(THEME_PREFIX) {
                return name.parse::<Theme>().ok().map(Command::SetTheme);
            }
            None
        }
    }
}

/// The menu id for an engine.
pub fn engine_id(engine: EngineChoice) -> String {
    format!("{ENGINE_PREFIX}{engine}")
}

/// The menu id for an input device.
pub fn device_id(name: &str) -> String {
    format!("{DEVICE_PREFIX}{name}")
}

/// The menu id for a theme.
pub fn theme_id(theme: Theme) -> String {
    format!("{THEME_PREFIX}{theme}")
}

/// A 32×32 RGBA tray icon: a filled dot in the accent colour on transparency.
///
/// Drawn in code rather than shipped as a `.ico` so there is no binary asset to
/// keep in step with the theme, and no file to fail to find next to the `.exe`.
pub fn icon_rgba(theme: Theme, size: u32) -> Vec<u8> {
    let (r, g, b) = match theme {
        // Iris violet on dark; a deeper shade on light, so it stays visible on
        // a white taskbar.
        Theme::Dark => (167u8, 139u8, 250u8),
        Theme::Light => (91u8, 33u8, 182u8),
    };
    let centre = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.42;

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - centre;
            let dy = y as f32 - centre;
            let distance = (dx * dx + dy * dy).sqrt();
            // One pixel of feather, so the dot does not look like a QR code at
            // 16 px, which is what the taskbar actually renders it at.
            let alpha = ((radius - distance).clamp(0.0, 1.0) * 255.0) as u8;
            rgba.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    rgba
}

/// The tooltip: what a user hovering the icon needs to know.
pub fn tooltip(config: &Config) -> String {
    format!(
        "Iris — hold {} to dictate\nengine: {}  polish: {}",
        config.hotkey,
        config.engine,
        if config.polish.enabled { "on" } else { "off" }
    )
}

/// A running tray. Dropping it removes the icon.
pub struct Tray {
    /// An RAII guard, never read: dropping it posts `WM_QUIT` to the tray
    /// thread, which removes the icon on its way out.
    #[cfg(windows)]
    _inner: win::TrayThread,
}

/// Start the tray, returning it and the channel its menu sends [`Command`]s on.
///
/// `devices` is the list of input device names for the microphone picker;
/// enumerating them is the caller's job because it is a Windows-only call and
/// this function has to compile everywhere.
pub fn spawn(config: &Config, devices: Vec<String>) -> anyhow::Result<(Tray, Receiver<Command>)> {
    #[cfg(windows)]
    {
        let (inner, commands) = win::spawn(config, devices)?;
        Ok((Tray { _inner: inner }, commands))
    }
    #[cfg(not(windows))]
    {
        let _ = (config, devices);
        // A channel with no sender: the loop selects on it forever and the app
        // is driven entirely by the hotkey, which is the right behaviour on a
        // platform where there is no tray to click.
        let (_tx, rx) = crossbeam_channel::bounded(0);
        Ok((Tray {}, rx))
    }
}

#[cfg(windows)]
// `GetMessageW`/`DispatchMessageW`/`PostThreadMessageW` are the message pump
// the tray icon needs, and there is no safe wrapper for them.
#[allow(unsafe_code)]
mod win {
    use anyhow::{Context, Result};
    use crossbeam_channel::{Receiver, Sender};
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::{Icon, TrayIconBuilder};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_QUIT,
    };

    use crate::app::Command;
    use crate::config::{Config, EngineChoice, Theme};

    use super::{
        command_for, device_id, engine_id, icon_rgba, theme_id, tooltip, DEVICE_DEFAULT, ID_POLISH,
        ID_QUIT, ID_RELOAD, ID_SETTINGS,
    };

    const ICON_SIZE: u32 = 32;

    pub struct TrayThread {
        thread_id: u32,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for TrayThread {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                unsafe {
                    let _ = PostThreadMessageW(
                        self.thread_id,
                        WM_QUIT,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(0),
                    );
                }
                let _ = handle.join();
            }
        }
    }

    pub fn spawn(config: &Config, devices: Vec<String>) -> Result<(TrayThread, Receiver<Command>)> {
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        // Everything the tray thread needs, copied rather than shared: the tray
        // must not hold a lock the dictation loop could ever wait on.
        let config = config.clone();

        let handle = std::thread::Builder::new()
            .name("iris-tray".into())
            .spawn(move || {
                let result = run(&config, devices, commands_tx);
                match result {
                    Ok(pump) => {
                        let _ = ready_tx.send(Ok(unsafe {
                            windows::Win32::System::Threading::GetCurrentThreadId()
                        }));
                        pump();
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .context("spawning the tray thread")?;

        let thread_id = ready_rx
            .recv()
            .context("the tray thread died during startup")??;

        Ok((
            TrayThread {
                thread_id,
                handle: Some(handle),
            },
            commands_rx,
        ))
    }

    /// Build the icon and menu, returning the message pump to run afterwards.
    ///
    /// The tray icon must be created *and* destroyed on this thread, so it is
    /// owned by the closure rather than returned.
    fn run(
        config: &Config,
        devices: Vec<String>,
        commands: Sender<Command>,
    ) -> Result<impl FnOnce()> {
        let menu = Menu::new();

        let engines = Submenu::new("Engine", true);
        for choice in EngineChoice::ALL {
            engines
                .append(&CheckMenuItem::with_id(
                    engine_id(*choice),
                    choice.label(),
                    true,
                    *choice == config.engine,
                    None,
                ))
                .context("building the engine menu")?;
        }

        let mics = Submenu::new("Microphone", true);
        mics.append(&CheckMenuItem::with_id(
            DEVICE_DEFAULT,
            "System default",
            true,
            config.audio.device.is_none(),
            None,
        ))
        .context("building the microphone menu")?;
        for name in &devices {
            let selected = config.audio.device.as_deref().is_some_and(|want| {
                name.to_ascii_lowercase()
                    .contains(&want.to_ascii_lowercase())
            });
            mics.append(&CheckMenuItem::with_id(
                device_id(name),
                name,
                true,
                selected,
                None,
            ))
            .context("building the microphone menu")?;
        }

        let themes = Submenu::new("Theme", true);
        for theme in [Theme::Dark, Theme::Light] {
            themes
                .append(&CheckMenuItem::with_id(
                    theme_id(theme),
                    match theme {
                        Theme::Dark => "Dark",
                        Theme::Light => "Light",
                    },
                    true,
                    theme == config.theme,
                    None,
                ))
                .context("building the theme menu")?;
        }

        let polish = CheckMenuItem::with_id(
            ID_POLISH,
            "Polish transcripts",
            true,
            config.polish.enabled,
            None,
        );

        menu.append_items(&[
            &MenuItem::new(format!("Hold {} to dictate", config.hotkey), false, None),
            &PredefinedMenuItem::separator(),
            &engines,
            &mics,
            &themes,
            &polish,
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(ID_SETTINGS, "Open settings…", true, None),
            &MenuItem::with_id(ID_RELOAD, "Reload settings", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(ID_QUIT, "Quit Iris", true, None),
        ])
        .context("building the tray menu")?;

        let icon = Icon::from_rgba(icon_rgba(config.theme, ICON_SIZE), ICON_SIZE, ICON_SIZE)
            .context("building the tray icon")?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip(config))
            .with_icon(icon)
            .with_menu_on_left_click(true)
            .build()
            .context("creating the tray icon")?;

        let mut polish_now = config.polish.enabled;

        Ok(move || {
            // Owned here so the icon is destroyed on the thread that made it.
            let _tray = tray;
            let menu_events = MenuEvent::receiver();
            let mut msg = MSG::default();

            // GetMessageW blocks, so menu events are drained on each wake-up;
            // `muda` posts a message to this thread when it fires one, which is
            // what makes that safe rather than a
            // busy-wait.
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                    while let Ok(event) = menu_events.try_recv() {
                        let Some(command) = command_for(event.id.as_ref(), polish_now) else {
                            continue;
                        };
                        if let Command::SetPolish(enabled) = command {
                            polish_now = enabled;
                        }
                        let quit = command == Command::Quit;
                        if commands.send(command).is_err() || quit {
                            return;
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_ids_round_trip_through_the_menu() {
        for engine in EngineChoice::ALL {
            let id = engine_id(*engine);
            assert_eq!(command_for(&id, true), Some(Command::SetEngine(*engine)));
        }
    }

    #[test]
    fn the_polish_item_toggles_rather_than_sets() {
        assert_eq!(
            command_for(ID_POLISH, true),
            Some(Command::SetPolish(false))
        );
        assert_eq!(
            command_for(ID_POLISH, false),
            Some(Command::SetPolish(true))
        );
    }

    #[test]
    fn device_ids_carry_the_name_and_the_default_is_none() {
        assert_eq!(
            command_for(DEVICE_DEFAULT, true),
            Some(Command::SetDevice(None))
        );
        assert_eq!(
            command_for(&device_id("Yeti Nano"), true),
            Some(Command::SetDevice(Some("Yeti Nano".into())))
        );
        // Device names contain colons and dashes; the prefix split must not.
        assert_eq!(
            command_for(&device_id("Mic (2- USB Audio): line 1"), true),
            Some(Command::SetDevice(Some(
                "Mic (2- USB Audio): line 1".into()
            )))
        );
    }

    #[test]
    fn theme_ids_round_trip() {
        for theme in [Theme::Dark, Theme::Light] {
            assert_eq!(
                command_for(&theme_id(theme), true),
                Some(Command::SetTheme(theme))
            );
        }
    }

    #[test]
    fn the_plain_items_map_to_their_commands() {
        assert_eq!(command_for(ID_QUIT, true), Some(Command::Quit));
        assert_eq!(command_for(ID_SETTINGS, true), Some(Command::OpenSettings));
        assert_eq!(command_for(ID_RELOAD, true), Some(Command::Reload));
    }

    #[test]
    fn unknown_ids_are_ignored_rather_than_guessed() {
        assert_eq!(command_for("", true), None);
        assert_eq!(command_for("engine:whisper", true), None);
        assert_eq!(command_for("theme:beige", true), None);
        assert_eq!(command_for("something-else", true), None);
    }

    #[test]
    fn the_icon_is_a_dot_of_the_right_size() {
        let size = 32;
        let rgba = icon_rgba(Theme::Dark, size);
        assert_eq!(rgba.len() as u32, size * size * 4);

        let pixel = |x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
        };
        // Opaque in the middle, transparent in the corner.
        assert_eq!(pixel(16, 16).3, 255);
        assert_eq!(pixel(0, 0).3, 0);
        // The two themes are visibly different colours.
        assert_ne!(icon_rgba(Theme::Light, size)[0], rgba[0]);
    }

    #[test]
    fn the_tooltip_says_what_the_hotkey_is() {
        let config = Config::default();
        let tip = tooltip(&config);
        assert!(tip.contains("right-ctrl"), "{tip}");
        assert!(tip.contains("mock"), "{tip}");
        assert!(tip.contains("polish: on"), "{tip}");
    }

    #[test]
    #[cfg(not(windows))]
    fn without_a_tray_the_command_channel_is_simply_empty() {
        let (_tray, commands) = spawn(&Config::default(), Vec::new()).unwrap();
        assert!(commands.try_recv().is_err());
    }
}

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
//! [`spawn`] is a no-op that returns a command channel nothing ever sends on,
//! so `main` compiles and runs on Linux with the hotkey and the tray simply
//! absent. `tray-icon` needs GTK on Linux, which would put a system dependency
//! on a crate that is otherwise CI-testable anywhere — not worth it for a
//! Windows-first product.

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

/// A 32×32 RGBA tray icon: the captain-locked **prism triangle** mark.
///
/// Rasterised in code from the same geometry as `iris-overlay`'s
/// `assets/iris-prism.svg` (the filled wedge only — at tray sizes the rays and
/// outline drop out). No violet filled-dot placeholder. Drawn rather than
/// shipped as a `.ico` so there is no binary asset to keep in step and no file
/// to fail to find next to the `.exe`.
///
/// `theme` picks the plate behind the wedge so the mark stays legible on both
/// dark and light taskbars; the spectrum fill is shared.
pub fn icon_rgba(theme: Theme, size: u32) -> Vec<u8> {
    // Plate matches the SVG's dark plate / a light inverse for light theme.
    let (plate_r, plate_g, plate_b, plate_a) = match theme {
        Theme::Dark => (20u8, 24u8, 34u8, 255u8),     // #141822
        Theme::Light => (237u8, 239u8, 245u8, 255u8), // soft porcelain plate
    };

    // Equilateral-ish wedge: apex at top centre, base near the bottom.
    // Coordinates in unit space matching the SVG wedge `M32 18 L45 45 L19 45`
    // inside a 64×64 viewBox, mapped into the icon with a little padding.
    let s = size as f32;
    let pad = s * 0.12;
    let apex = (s * 0.5, pad + s * 0.08);
    let bl = (pad + s * 0.08, s - pad);
    let br = (s - pad - s * 0.08, s - pad);

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let inside = point_in_triangle(px, py, apex, bl, br);
            if inside {
                // Spectrum along the base (left → right), matching the SVG
                // linearGradient rose → amber → mint → sky → violet.
                let t = ((px - bl.0) / (br.0 - bl.0)).clamp(0.0, 1.0);
                let (r, g, b) = spectrum_sample(t);
                // Soft vertical sheen: slightly brighter toward the apex.
                let sheen = 1.0 + 0.12 * (1.0 - ((py - apex.1) / (bl.1 - apex.1)).clamp(0.0, 1.0));
                let r = ((r as f32) * sheen).min(255.0) as u8;
                let g = ((g as f32) * sheen).min(255.0) as u8;
                let b = ((b as f32) * sheen).min(255.0) as u8;
                rgba.extend_from_slice(&[r, g, b, 255]);
            } else {
                // Rounded plate behind the wedge (reads as the SVG rounded rect
                // at small sizes when the wedge alone would be too thin).
                let cx = s * 0.5;
                let cy = s * 0.5;
                let half = s * 0.48;
                let dx = (px - cx).abs();
                let dy = (py - cy).abs();
                // Squircle-ish plate; feather the edge one pixel.
                let edge = (half - dx.max(dy)).clamp(0.0, 1.0);
                let alpha = (edge * f32::from(plate_a)) as u8;
                rgba.extend_from_slice(&[plate_r, plate_g, plate_b, alpha]);
            }
        }
    }
    rgba
}

/// Barycentric point-in-triangle test.
fn point_in_triangle(px: f32, py: f32, a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let v0 = (c.0 - a.0, c.1 - a.1);
    let v1 = (b.0 - a.0, b.1 - a.1);
    let v2 = (px - a.0, py - a.1);
    let dot00 = v0.0 * v0.0 + v0.1 * v0.1;
    let dot01 = v0.0 * v1.0 + v0.1 * v1.1;
    let dot02 = v0.0 * v2.0 + v0.1 * v2.1;
    let dot11 = v1.0 * v1.0 + v1.1 * v1.1;
    let dot12 = v1.0 * v2.0 + v1.1 * v2.1;
    let denom = dot00 * dot11 - dot01 * dot01;
    if denom.abs() < f32::EPSILON {
        return false;
    }
    let inv = 1.0 / denom;
    let u = (dot11 * dot02 - dot01 * dot12) * inv;
    let v = (dot00 * dot12 - dot01 * dot02) * inv;
    u >= 0.0 && v >= 0.0 && (u + v) <= 1.0
}

/// Sample the locked spectrum ramp (same stops as `iris-prism.svg`).
fn spectrum_sample(t: f32) -> (u8, u8, u8) {
    // stops: #FF6B8A, #FFB86B, #6BFFB8, #6BCBFF, #8B7BFF
    const STOPS: [(f32, u8, u8, u8); 5] = [
        (0.00, 255, 107, 138),
        (0.25, 255, 184, 107),
        (0.50, 107, 255, 184),
        (0.75, 107, 203, 255),
        (1.00, 139, 123, 255),
    ];
    let t = t.clamp(0.0, 1.0);
    for i in 0..STOPS.len() - 1 {
        let (t0, r0, g0, b0) = STOPS[i];
        let (t1, r1, g1, b1) = STOPS[i + 1];
        if t <= t1 {
            let u = if (t1 - t0).abs() < f32::EPSILON {
                0.0
            } else {
                (t - t0) / (t1 - t0)
            };
            let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * u) as u8;
            return (lerp(r0, r1), lerp(g0, g1), lerp(b0, b1));
        }
    }
    let last = STOPS[STOPS.len() - 1];
    (last.1, last.2, last.3)
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
    /// Keeps the command channel connected: a receiver whose only sender was
    /// dropped reads as *disconnected*, which the loop treats as "the tray
    /// died, exit" — the opposite of "there is no tray, run forever".
    #[cfg(not(windows))]
    _commands: crossbeam_channel::Sender<Command>,
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
        // A channel that never carries anything: the sender lives in the
        // returned `Tray`, so the loop selects on the receiver forever and the
        // app is driven entirely by the hotkey — the right behaviour on a
        // platform where there is no tray to click.
        let (tx, rx) = crossbeam_channel::bounded(0);
        Ok((Tray { _commands: tx }, rx))
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
    fn the_icon_is_a_prism_wedge_of_the_right_size() {
        let size = 32;
        let rgba = icon_rgba(Theme::Dark, size);
        assert_eq!(rgba.len() as u32, size * size * 4);

        let pixel = |x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
        };
        // Transparent in the corner (outside the plate).
        assert_eq!(pixel(0, 0).3, 0);
        // The wedge centre is opaque spectrum, not a flat violet filled-dot.
        let centre = pixel(16, 20);
        assert_eq!(centre.3, 255, "wedge centre should be opaque");
        // Not the old violet placeholder (167, 139, 250).
        assert!(
            !(centre.0 == 167 && centre.1 == 139 && centre.2 == 250),
            "tray icon must not be the violet filled-dot placeholder: {centre:?}"
        );
        // Spectrum has real colour variation across the base.
        let left = pixel(10, 24);
        let right = pixel(22, 24);
        assert_ne!(
            (left.0, left.1, left.2),
            (right.0, right.1, right.2),
            "spectrum should vary across the wedge base"
        );
        // Light theme plate differs from dark.
        assert_ne!(icon_rgba(Theme::Light, size)[0], rgba[0]);
    }

    /// `build.rs` re-implements this geometry, because a build script cannot
    /// depend on the crate it builds. Read back the `.ico` it actually wrote
    /// and compare pixels, so drift fails the build rather than shipping an
    /// `.exe` whose icon quietly stops matching the tray.
    #[test]
    #[cfg(windows)]
    fn the_embedded_exe_icon_still_matches_the_tray_mark() {
        const SIZE: u32 = 256;
        let ico_path = std::path::Path::new(env!("OUT_DIR")).join("iris.ico");
        let ico = std::fs::read(&ico_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", ico_path.display()));

        // ICONDIR: 6 bytes, then one 16-byte ICONDIRENTRY per image. A width
        // byte of 0 is how the format spells 256.
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        let entry = (0..count)
            .map(|i| 6 + 16 * i)
            .find(|&e| ico[e] == 0)
            .expect("the .ico carries a 256x256 image");
        let len = u32::from_le_bytes(ico[entry + 8..entry + 12].try_into().unwrap()) as usize;
        let off = u32::from_le_bytes(ico[entry + 12..entry + 16].try_into().unwrap()) as usize;
        // Past the 40-byte BITMAPINFOHEADER are the bottom-up BGRA pixels,
        // then the AND mask this comparison ignores.
        let pixels = &ico[off + 40..off + len];

        let rgba = icon_rgba(Theme::Dark, SIZE);
        let mut expected = Vec::with_capacity(rgba.len());
        for y in (0..SIZE).rev() {
            for x in 0..SIZE {
                let i = ((y * SIZE + x) * 4) as usize;
                expected.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
            }
        }

        assert!(
            pixels.len() >= expected.len(),
            "the .ico's 256x256 image is short: {} < {}",
            pixels.len(),
            expected.len()
        );
        let mismatch = pixels[..expected.len()]
            .iter()
            .zip(&expected)
            .position(|(a, b)| a != b);
        assert!(
            mismatch.is_none(),
            "build.rs's copy of the icon geometry has drifted from icon_rgba \
             (first differing byte at {}); re-sync build.rs with this function",
            mismatch.unwrap()
        );
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
    fn without_a_tray_the_command_channel_is_empty_but_not_disconnected() {
        let (_tray, commands) = spawn(&Config::default(), Vec::new()).unwrap();
        // Empty and Disconnected are both `Err`; the loop exits on the latter,
        // so the distinction is the whole point of this test.
        assert_eq!(
            commands.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        );
    }
}

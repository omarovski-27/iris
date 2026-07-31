//! Global push-to-talk via a low-level keyboard hook.
//!
//! `WH_KEYBOARD_LL` is the only way to see a key that is *held* regardless of
//! which window has focus. `RegisterHotKey` would be simpler, but it delivers a
//! single `WM_HOTKEY` per press with no release event, which cannot express
//! push-to-talk.
//!
//! Two constraints come with the hook and are handled here:
//!
//! * **The hook thread must stay responsive.** Windows silently removes a hook
//!   whose callback exceeds `LowLevelHooksTimeout` (300 ms by default). So the
//!   hook thread does nothing but pump messages, and the callback does nothing
//!   but a non-blocking send.
//! * **Our own `SendInput` comes back through the hook.** Injected events carry
//!   `LLKHF_INJECTED`; without filtering them, injecting a transcript
//!   containing the hotkey character would retrigger dictation.

use anyhow::{bail, Result};

/// A key usable as push-to-talk.
///
/// Modifier keys are the good choices: they are on every keyboard, they are
/// comfortable to hold, and the right-hand ones are nearly unused as bare
/// keypresses. Right-Ctrl is the default for that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Key {
    /// The default: on every keyboard, comfortable to hold, and almost never
    /// used as a bare keypress.
    #[default]
    RightCtrl,
    LeftCtrl,
    RightShift,
    RightAlt,
    RightWin,
    CapsLock,
    ScrollLock,
    Pause,
    F8,
    F9,
    F10,
}

impl Key {
    /// The Win32 virtual-key code. A low-level hook reports the *specific*
    /// side (`VK_RCONTROL`, not `VK_CONTROL`), which is what makes
    /// right-modifier hotkeys possible.
    pub fn vk(self) -> u32 {
        match self {
            Key::RightCtrl => 0xA3,  // VK_RCONTROL
            Key::LeftCtrl => 0xA2,   // VK_LCONTROL
            Key::RightShift => 0xA1, // VK_RSHIFT
            Key::RightAlt => 0xA5,   // VK_RMENU
            Key::RightWin => 0x5C,   // VK_RWIN
            Key::CapsLock => 0x14,   // VK_CAPITAL
            Key::ScrollLock => 0x91, // VK_SCROLL
            Key::Pause => 0x13,      // VK_PAUSE
            Key::F8 => 0x77,
            Key::F9 => 0x78,
            Key::F10 => 0x79,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Key::RightCtrl => "right-ctrl",
            Key::LeftCtrl => "left-ctrl",
            Key::RightShift => "right-shift",
            Key::RightAlt => "right-alt",
            Key::RightWin => "right-win",
            Key::CapsLock => "caps-lock",
            Key::ScrollLock => "scroll-lock",
            Key::Pause => "pause",
            Key::F8 => "f8",
            Key::F9 => "f9",
            Key::F10 => "f10",
        }
    }

    /// Every accepted `--hotkey` value, for help text and error messages.
    pub const NAMES: &'static [&'static str] = &[
        "rctrl",
        "lctrl",
        "rshift",
        "ralt",
        "rwin",
        "capslock",
        "scrolllock",
        "pause",
        "f8",
        "f9",
        "f10",
    ];
}

impl std::str::FromStr for Key {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let normalised: String = s
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        Ok(match normalised.as_str() {
            "rctrl" | "rightctrl" | "rcontrol" | "rightcontrol" => Key::RightCtrl,
            "lctrl" | "leftctrl" | "lcontrol" | "leftcontrol" => Key::LeftCtrl,
            "rshift" | "rightshift" => Key::RightShift,
            "ralt" | "rightalt" | "rmenu" => Key::RightAlt,
            "rwin" | "rightwin" => Key::RightWin,
            "capslock" | "caps" => Key::CapsLock,
            "scrolllock" | "scroll" => Key::ScrollLock,
            "pause" | "break" => Key::Pause,
            "f8" => Key::F8,
            "f9" => Key::F9,
            "f10" => Key::F10,
            _ => bail!(
                "unknown hotkey {s:?} (expected one of: {})",
                Key::NAMES.join(", ")
            ),
        })
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Push-to-talk transitions. Auto-repeat is collapsed, so a held key produces
/// exactly one `Down` and one `Up`.
///
/// Each event carries the instant the hook saw it. Reading the clock in the
/// callback rather than on the receiving end keeps the channel hop out of the
/// reported latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Down(std::time::Instant),
    Up(std::time::Instant),
}

impl HotkeyEvent {
    pub fn at(self) -> std::time::Instant {
        match self {
            HotkeyEvent::Down(at) | HotkeyEvent::Up(at) => at,
        }
    }
}

/// `VK_PACKET`: the virtual-key code a low-level hook sees for a
/// `KEYEVENTF_UNICODE` event (the character itself rides in `scanCode`
/// instead) — the one vkCode text injection can ever produce.
///
/// `cfg(test)`: the hook filters on `LLKHF_INJECTED` rather than on this code,
/// so it exists only for the tests that pin that reasoning down.
#[cfg(test)]
const VK_PACKET: u32 = 0xE7;

/// Whether a low-level keyboard message is a genuine press/release of the
/// configured hotkey — i.e. one the hook should act on, rather than a message
/// to wave through untouched via `CallNextHookEx`.
///
/// `injected` is checked unconditionally, not just as a tie-breaker: a
/// `KEYEVENTF_UNICODE` character always carries `vkCode == VK_PACKET`
/// (`0xE7`), never a configured hotkey's own code, so `vk_code == target_vk`
/// alone already excludes every character Iris injects. The `injected` check
/// is what also covers the case where Iris (or `paste`'s Ctrl+V) injects a
/// *virtual-key* event — `VK_RETURN`, `VK_TAB`, or generic `VK_CONTROL` — so
/// that a future hotkey choice sharing one of those codes can never be
/// mistaken for a real press either.
#[cfg(any(windows, test))]
fn is_hotkey_event(vk_code: u32, target_vk: u32, injected: bool) -> bool {
    vk_code == target_vk && !injected
}

#[cfg(windows)]
pub use hook::{is_held, listen, Listener};

#[cfg(windows)]
mod hook {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::OnceLock;

    use anyhow::{Context, Result};
    use crossbeam_channel::{Receiver, Sender};
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
        TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG,
        WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    use super::{is_hotkey_event, HotkeyEvent, Key};

    /// The hook callback is a bare `extern "system" fn`, so its state has to be
    /// global. One hook per process is all we want anyway.
    static SENDER: OnceLock<Sender<HotkeyEvent>> = OnceLock::new();
    static TARGET_VK: AtomicU32 = AtomicU32::new(0);
    static SUPPRESS: AtomicBool = AtomicBool::new(true);
    static HELD: AtomicBool = AtomicBool::new(false);

    /// A running hook. Dropping it stops the hook thread and unhooks.
    pub struct Listener {
        thread_id: u32,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Listener {
        pub fn stop(mut self) {
            self.shutdown();
        }

        fn shutdown(&mut self) {
            if let Some(handle) = self.handle.take() {
                // WM_QUIT breaks GetMessageW, and the thread unhooks on its way
                // out.
                unsafe {
                    let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                let _ = handle.join();
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    /// Install the hook and start pumping messages on a dedicated thread.
    ///
    /// `suppress` stops the hotkey reaching other applications, which is what
    /// you want for a bare modifier — otherwise holding right-Ctrl to dictate
    /// also arms every Ctrl shortcut in the focused app.
    pub fn listen(key: Key, suppress: bool) -> Result<(Listener, Receiver<HotkeyEvent>)> {
        let (tx, rx) = crossbeam_channel::unbounded();
        SENDER
            .set(tx)
            .map_err(|_| anyhow::anyhow!("a hotkey listener is already running"))?;
        TARGET_VK.store(key.vk(), Ordering::Relaxed);
        SUPPRESS.store(suppress, Ordering::Relaxed);
        HELD.store(false, Ordering::Relaxed);

        // The thread reports back whether SetWindowsHookExW succeeded, so
        // `listen` can fail loudly instead of returning a dead listener.
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let handle = std::thread::Builder::new()
            .name("iris-hotkey".into())
            .spawn(move || {
                let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), None, 0) };
                let hook: HHOOK = match hook {
                    Ok(h) => {
                        let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
                        h
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };

                // This loop is the hook's lifeline: a low-level hook is only
                // serviced while its owning thread pumps messages.
                let mut msg = MSG::default();
                unsafe {
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    let _ = UnhookWindowsHookEx(hook);
                }
            })
            .context("spawning the hotkey thread")?;

        let thread_id = ready_rx
            .recv()
            .context("the hotkey thread died during startup")?
            .context(
                "SetWindowsHookExW failed. Low-level keyboard hooks are blocked for processes \
                 running at a lower integrity level than the foreground window; try running from \
                 a normal (non-elevated) console, or elevate Iris to match.",
            )?;

        Ok((
            Listener {
                thread_id,
                handle: Some(handle),
            },
            rx,
        ))
    }

    /// Whether the hook currently believes the configured hotkey is held —
    /// driven only by real, non-injected key transitions it has actually
    /// seen (the same bookkeeping that decides whether to emit
    /// [`HotkeyEvent::Down`]/[`HotkeyEvent::Up`]), never by polling live
    /// keyboard state itself.
    ///
    /// This is the cross-check `inject.rs` needs before trusting
    /// `GetAsyncKeyState`: a hotkey release is followed by transcription and
    /// polish, easily hundreds of milliseconds, which is long enough for the
    /// user to have genuinely pressed the hotkey again for their *next*
    /// utterance. `GetAsyncKeyState` reading "down" at that point is not a
    /// stuck leftover — it is correct, current state — and this function is
    /// what lets the caller tell the difference without guessing at exactly
    /// how Windows' asynchronous key state interacts with a suppressing
    /// hook (a mechanism this project was not able to confirm; see
    /// `inject.rs`'s `modifier_to_release`).
    ///
    /// Returns `false` before `listen` has ever been called, which is the
    /// correct answer: nothing is held if there is no hook to hold it.
    pub fn is_held() -> bool {
        HELD.load(Ordering::Relaxed)
    }

    /// Runs on the hook thread for every keystroke in the system. Must be fast:
    /// see the module docs.
    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // HC_ACTION == 0; anything else must be passed straight through.
        if code != 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let injected = info.flags.0 & LLKHF_INJECTED.0 != 0;
        let is_ours = is_hotkey_event(info.vkCode, TARGET_VK.load(Ordering::Relaxed), injected);

        if is_ours {
            let now = std::time::Instant::now();
            let event = match wparam.0 as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    // Holding a key repeats it; report the first one only.
                    (!HELD.swap(true, Ordering::Relaxed)).then_some(HotkeyEvent::Down(now))
                }
                WM_KEYUP | WM_SYSKEYUP => HELD
                    .swap(false, Ordering::Relaxed)
                    .then_some(HotkeyEvent::Up(now)),
                _ => None,
            };
            if let Some(event) = event {
                if let Some(tx) = SENDER.get() {
                    // Unbounded: never blocks the hook.
                    let _ = tx.send(event);
                }
            }
            if SUPPRESS.load(Ordering::Relaxed) {
                // Non-zero swallows the key so the focused app never sees it.
                return LRESULT(1);
            }
        }

        CallNextHookEx(None, code, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_right_ctrl() {
        assert_eq!(Key::default(), Key::RightCtrl);
        assert_eq!(Key::default().vk(), 0xA3);
    }

    #[test]
    fn parses_the_documented_spellings() {
        for name in Key::NAMES {
            name.parse::<Key>()
                .unwrap_or_else(|e| panic!("advertised name {name:?} does not parse: {e}"));
        }
        assert_eq!("RCtrl".parse::<Key>().unwrap(), Key::RightCtrl);
        assert_eq!("right-ctrl".parse::<Key>().unwrap(), Key::RightCtrl);
        assert_eq!("Right Control".parse::<Key>().unwrap(), Key::RightCtrl);
        assert_eq!("F9".parse::<Key>().unwrap(), Key::F9);
    }

    #[test]
    fn unknown_keys_list_the_alternatives() {
        let err = "hyper".parse::<Key>().unwrap_err().to_string();
        assert!(err.contains("rctrl"), "{err}");
    }

    #[test]
    fn left_and_right_modifiers_are_distinct() {
        // The whole point of using a low-level hook rather than RegisterHotKey.
        assert_ne!(Key::RightCtrl.vk(), Key::LeftCtrl.vk());
    }

    #[test]
    fn a_genuine_press_of_the_configured_hotkey_is_recognised() {
        assert!(is_hotkey_event(
            Key::RightCtrl.vk(),
            Key::RightCtrl.vk(),
            false
        ));
    }

    #[test]
    fn other_real_keys_are_never_mistaken_for_the_hotkey() {
        assert!(!is_hotkey_event(
            Key::LeftCtrl.vk(),
            Key::RightCtrl.vk(),
            false
        ));
    }

    #[test]
    fn injected_unicode_keystrokes_are_never_treated_as_the_hotkey() {
        // This is the shape every character Iris injects actually takes: the
        // hook sees vkCode == VK_PACKET, not the character, with LLKHF_INJECTED
        // set. Proves the burst that types a transcript can never retrigger or
        // otherwise be treated as a hotkey press, for every configured hotkey.
        for key in [
            Key::RightCtrl,
            Key::LeftCtrl,
            Key::RightShift,
            Key::RightAlt,
            Key::RightWin,
            Key::CapsLock,
            Key::ScrollLock,
            Key::Pause,
            Key::F8,
            Key::F9,
            Key::F10,
        ] {
            assert!(!is_hotkey_event(VK_PACKET, key.vk(), true));
        }
    }

    #[test]
    fn the_injected_flag_is_authoritative_even_on_a_vk_collision() {
        // Even in the pathological case where a future hotkey's own vk code
        // happened to equal VK_PACKET, `injected` alone must still veto it —
        // the vk match is necessary but never sufficient.
        assert!(!is_hotkey_event(VK_PACKET, VK_PACKET, true));
        assert!(is_hotkey_event(VK_PACKET, VK_PACKET, false));
    }
}

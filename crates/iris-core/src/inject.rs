//! Getting text into whatever window has focus.
//!
//! Two strategies, because they fail in different places:
//!
//! * [`Method::SendInput`] synthesises Unicode keystrokes. It leaves the
//!   clipboard alone and works in essentially every control, including ones that
//!   refuse paste. It is the default.
//! * [`Method::Clipboard`] puts the text on the clipboard and sends Ctrl+V.
//!   It is O(1) in the length of the text rather than O(n) keystrokes, so it is
//!   the escape hatch for very long transcripts and for apps that throttle
//!   synthetic input — at the cost of clobbering the clipboard and of depending
//!   on the target app's paste shortcut.
//!
//! Both are measured; see `docs/spike-findings.md`.

use anyhow::{bail, Result};

/// How to deliver text to the focused window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Method {
    #[default]
    SendInput,
    Clipboard,
}

impl std::str::FromStr for Method {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_'], "")
            .as_str()
        {
            "sendinput" | "keys" | "type" => Ok(Method::SendInput),
            "clipboard" | "paste" => Ok(Method::Clipboard),
            other => bail!("unknown injection method {other:?} (expected sendinput or clipboard)"),
        }
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Method::SendInput => "sendinput",
            Method::Clipboard => "clipboard",
        })
    }
}

/// Deliver `text` to the focused window.
pub fn inject(text: &str, method: Method) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    imp(text, method)
}

#[cfg(windows)]
use win::inject as imp;

/// Stub so the harness and the portable tests build on non-Windows hosts.
#[cfg(not(windows))]
fn imp(_text: &str, _method: Method) -> Result<()> {
    bail!("text injection is only implemented on Windows")
}

/// Virtual-key codes for the eight standard modifiers (left/right Ctrl,
/// Shift, Alt, Win) — plain `u16`s rather than the `windows` crate's typed
/// constants so this list, and the decision below, stay portable.
///
/// `SendInput`'s own documentation warns: "This function does not reset the
/// keyboard's current state. Any keys that are already pressed when the
/// function is called might interfere with the events that this function
/// generates. To avoid this problem, check the keyboard's state with
/// `GetAsyncKeyState`... and correct as necessary." Iris's push-to-talk
/// hotkey is one of these on every modifier-based configuration, and the
/// low-level hook suppresses its key events end to end (see `hotkey.rs`), so
/// it is the one most likely to still read as down when injection starts.
///
/// `cfg(any(windows, test))`: the only non-test consumer is `mod win`, so a
/// plain non-Windows build has nothing that reaches this — but `cargo test`
/// must still see it, per this crate's rule that decision logic is testable
/// without the OS.
#[cfg(any(windows, test))]
const MODIFIER_VKS: [u16; 8] = [
    0xA2, // VK_LCONTROL
    0xA3, // VK_RCONTROL
    0xA0, // VK_LSHIFT
    0xA1, // VK_RSHIFT
    0xA4, // VK_LMENU (left Alt)
    0xA5, // VK_RMENU (right Alt)
    0x5B, // VK_LWIN
    0x5C, // VK_RWIN
];

/// Whether a modifier's scan code is 0xE0-prefixed, i.e. whether a synthesised
/// event for it must carry `KEYEVENTF_EXTENDEDKEY`.
///
/// `SendInput` fills in the scan code from `wVk` when none is supplied, and the
/// mapping is not injective: right Ctrl and right Alt share their base scan
/// code with the left-hand key and are told apart only by the extended flag,
/// while the two Win keys exist only in extended form. Without the flag the
/// corrective key-up below is delivered as (and decoded by the target as) the
/// *left* counterpart, so it would fail to clear `VK_RCONTROL` — the default
/// push-to-talk hotkey, and the one this whole correction exists for.
#[cfg(any(windows, test))]
fn is_extended_modifier(vk: u16) -> bool {
    matches!(
        vk,
        0xA3 // VK_RCONTROL
        | 0xA5 // VK_RMENU
        | 0x5B // VK_LWIN
        | 0x5C // VK_RWIN
    )
}

/// Which of `MODIFIER_VKS` need a corrective key-up before a keystroke burst,
/// given each one's current (virtual-key, is-down) state. Kept separate from
/// the `GetAsyncKeyState` calls that produce `state` so the decision is
/// testable without the OS.
#[cfg(any(windows, test))]
fn stuck_modifiers(state: &[(u16, bool)]) -> Vec<u16> {
    state
        .iter()
        .filter(|(_, down)| *down)
        .map(|(vk, _)| *vk)
        .collect()
}

#[cfg(windows)]
mod win {
    use anyhow::{bail, Context, Result};
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY,
    };

    use crate::text::{self, KeyUnit};
    use crate::vlog;

    use super::Method;

    const CF_UNICODETEXT: u32 = 13;
    const VK_CONTROL: u16 = 0x11;
    const VK_V: u16 = 0x56;

    /// `SendInput` takes an array; sending one call per keystroke would cost a
    /// syscall each and let other input interleave. Batches keep a transcript
    /// atomic from the target app's point of view. 512 events (256 characters)
    /// keeps the temporary array comfortably small.
    const BATCH: usize = 512;

    pub fn inject(text: &str, method: Method) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        match method {
            Method::SendInput => send_keystrokes(text),
            Method::Clipboard => paste(text),
        }
    }

    fn send_keystrokes(text: &str) -> Result<()> {
        release_stuck_modifiers()?;

        let mut inputs = Vec::with_capacity(BATCH);
        for unit in text::plan(text) {
            let (scan, vk, extra) = match unit {
                KeyUnit::Unicode(u) => (u, 0u16, KEYEVENTF_UNICODE),
                KeyUnit::Virtual(vk) => (0u16, vk, KEYBD_EVENT_FLAGS(0)),
            };
            inputs.push(key_event(vk, scan, extra));
            inputs.push(key_event(vk, scan, extra | KEYEVENTF_KEYUP));

            if inputs.len() >= BATCH && unit.may_end_batch() {
                flush(&mut inputs)?;
            }
        }
        flush(&mut inputs)
    }

    /// Correct any modifier Windows still thinks is down before typing.
    ///
    /// One dedicated `SendInput` call, issued and flushed before the text
    /// burst, so the correction is fully processed first rather than merely
    /// ordered first within a single array (see `super::MODIFIER_VKS`).
    fn release_stuck_modifiers() -> Result<()> {
        let state: Vec<(u16, bool)> = super::MODIFIER_VKS
            .iter()
            .map(|&vk| {
                let down = unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
                (vk, down)
            })
            .collect();

        let stuck = super::stuck_modifiers(&state);
        if stuck.is_empty() {
            return Ok(());
        }
        // The root cause is unreproducible off the user's desk, so the codes
        // themselves are the field signal that confirms or refutes it.
        let codes: Vec<String> = stuck.iter().map(|vk| format!("0x{vk:02X}")).collect();
        vlog!(
            "releasing {} modifier(s) still reported down before injecting: {}",
            stuck.len(),
            codes.join(", ")
        );

        let mut corrections: Vec<INPUT> = stuck
            .into_iter()
            .map(|vk| {
                let mut flags = KEYEVENTF_KEYUP;
                if super::is_extended_modifier(vk) {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                key_event(vk, 0, flags)
            })
            .collect();
        flush(&mut corrections)
    }

    fn key_event(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn flush(inputs: &mut Vec<INPUT>) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            // The usual cause is UIPI: a process cannot inject into a window
            // owned by a higher-integrity process.
            bail!(
                "SendInput delivered {sent} of {} events ({}). The focused window may be running \
                 elevated — run Iris elevated too, or focus a normal window.",
                inputs.len(),
                windows::core::Error::from_thread()
            );
        }
        inputs.clear();
        Ok(())
    }

    /// Set the clipboard, then send Ctrl+V.
    ///
    /// Known limitation, kept out of the default path for exactly this reason:
    /// the previous clipboard contents are lost. Restoring them requires waiting
    /// for the target app to finish pasting, which is unbounded.
    fn paste(text: &str) -> Result<()> {
        set_clipboard(text)?;

        let inputs = [
            key_event(VK_CONTROL, 0, KEYBD_EVENT_FLAGS(0)),
            key_event(VK_V, 0, KEYBD_EVENT_FLAGS(0)),
            key_event(VK_V, 0, KEYEVENTF_KEYUP),
            key_event(VK_CONTROL, 0, KEYEVENTF_KEYUP),
        ];
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            bail!("SendInput could not deliver Ctrl+V ({sent}/4)");
        }
        vlog!("pasted {} chars via the clipboard", text.chars().count());
        Ok(())
    }

    fn set_clipboard(text: &str) -> Result<()> {
        let mut utf16: Vec<u16> = text.encode_utf16().collect();
        utf16.push(0);
        let bytes = utf16.len() * std::mem::size_of::<u16>();

        unsafe {
            OpenClipboard(Some(HWND(std::ptr::null_mut())))
                .context("another application is holding the clipboard open")?;
            // From here on every path must close the clipboard.
            let result = (|| -> Result<()> {
                EmptyClipboard().context("emptying the clipboard")?;
                let handle =
                    GlobalAlloc(GMEM_MOVEABLE, bytes).context("allocating clipboard memory")?;
                let ptr = GlobalLock(handle);
                if ptr.is_null() {
                    let _ = GlobalFree(Some(handle));
                    bail!("locking clipboard memory failed");
                }
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr as *mut u16, utf16.len());
                let _ = GlobalUnlock(handle);
                // SetClipboardData takes ownership of the block only on
                // success, so it must not be freed there — but every failure
                // path from the allocation onwards must free it.
                if let Err(e) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(handle.0))) {
                    let _ = GlobalFree(Some(handle));
                    return Err(anyhow::Error::new(e).context("setting clipboard text"));
                }
                Ok(())
            })();
            let _ = CloseClipboard();
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_parses_and_defaults_to_sendinput() {
        assert_eq!(Method::default(), Method::SendInput);
        assert_eq!("sendinput".parse::<Method>().unwrap(), Method::SendInput);
        assert_eq!("send-input".parse::<Method>().unwrap(), Method::SendInput);
        assert_eq!("clipboard".parse::<Method>().unwrap(), Method::Clipboard);
        assert_eq!("paste".parse::<Method>().unwrap(), Method::Clipboard);
        assert!("magic".parse::<Method>().is_err());
    }

    #[test]
    fn method_round_trips_through_display() {
        for m in [Method::SendInput, Method::Clipboard] {
            assert_eq!(m.to_string().parse::<Method>().unwrap(), m);
        }
    }

    #[test]
    fn injecting_nothing_is_a_no_op_everywhere() {
        // Guards the early return: without it this would fail on Linux.
        assert!(inject("", Method::SendInput).is_ok());
    }

    #[test]
    fn stuck_modifiers_is_empty_when_nothing_is_held() {
        let state: Vec<(u16, bool)> = MODIFIER_VKS.iter().map(|&vk| (vk, false)).collect();
        assert!(stuck_modifiers(&state).is_empty());
    }

    #[test]
    fn stuck_modifiers_returns_only_the_ones_reported_down() {
        let state = [
            (0xA2u16, false), // VK_LCONTROL, up
            (0xA3u16, true),  // VK_RCONTROL, down — e.g. the push-to-talk hotkey
            (0xA0u16, false), // VK_LSHIFT, up
            (0x5Cu16, true),  // VK_RWIN, down
        ];
        assert_eq!(stuck_modifiers(&state), vec![0xA3, 0x5C]);
    }

    #[test]
    fn right_hand_and_win_modifiers_need_the_extended_flag() {
        // Without it their corrective key-up carries the left-hand key's scan
        // code — including VK_RCONTROL, the default push-to-talk hotkey.
        for vk in [0xA3u16, 0xA5, 0x5B, 0x5C] {
            assert!(is_extended_modifier(vk), "0x{vk:02X}");
        }
        for vk in [0xA2u16, 0xA0, 0xA1, 0xA4] {
            assert!(!is_extended_modifier(vk), "0x{vk:02X}");
        }
    }

    #[test]
    fn every_tracked_modifier_has_an_extended_verdict() {
        // Guards the pairing: a VK added to MODIFIER_VKS without a matching
        // decision here would silently default to non-extended.
        assert_eq!(
            MODIFIER_VKS
                .iter()
                .filter(|&&vk| is_extended_modifier(vk))
                .count(),
            4
        );
    }

    #[test]
    fn stuck_modifiers_covers_every_tracked_modifier() {
        let state: Vec<(u16, bool)> = MODIFIER_VKS.iter().map(|&vk| (vk, true)).collect();
        assert_eq!(stuck_modifiers(&state), MODIFIER_VKS.to_vec());
    }
}

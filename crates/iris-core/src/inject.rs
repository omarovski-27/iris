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

#[cfg(windows)]
mod win {
    use anyhow::{bail, Context, Result};
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE, VIRTUAL_KEY,
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
        let mut inputs = Vec::with_capacity(BATCH);
        for unit in text::plan(text) {
            let (scan, vk, extra) = match unit {
                KeyUnit::Unicode(u) => (u, 0u16, KEYEVENTF_UNICODE),
                KeyUnit::Virtual(vk) => (0u16, vk, KEYBD_EVENT_FLAGS(0)),
            };
            inputs.push(key_event(vk, scan, extra));
            inputs.push(key_event(vk, scan, extra | KEYEVENTF_KEYUP));

            if inputs.len() >= BATCH {
                flush(&mut inputs)?;
            }
        }
        flush(&mut inputs)
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
                    bail!("locking clipboard memory failed");
                }
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr as *mut u16, utf16.len());
                let _ = GlobalUnlock(handle);
                // SetClipboardData takes ownership of the block on success, so
                // it must not be freed here.
                SetClipboardData(CF_UNICODETEXT, Some(HANDLE(handle.0)))
                    .context("setting clipboard text")?;
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
}

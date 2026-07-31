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

use crate::hotkey::Key;

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
///
/// `hotkey` is the configured push-to-talk key. It is passed through so the
/// injector can correct it if it still reads as down and is eligible for
/// correction — see [`modifier_to_release`] and [`Key::is_correctable_modifier`].
pub fn inject(text: &str, method: Method, hotkey: Key) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    imp(text, method, hotkey)
}

#[cfg(windows)]
use win::inject as imp;

/// Stub so the harness and the portable tests build on non-Windows hosts.
#[cfg(not(windows))]
fn imp(_text: &str, _method: Method, _hotkey: Key) -> Result<()> {
    bail!("text injection is only implemented on Windows")
}

/// Whether the configured hotkey needs a corrective key-up before a keystroke
/// burst. Kept separate from the OS calls that produce `hotkey_down` and
/// `genuinely_held` so the decision is testable without the OS.
///
/// `SendInput`'s own documentation warns: "This function does not reset the
/// keyboard's current state. Any keys that are already pressed when the
/// function is called might interfere with the events that this function
/// generates. To avoid this problem, check the keyboard's state with
/// `GetAsyncKeyState`... and correct as necessary." That is `hotkey_down`.
///
/// It is not sufficient alone. Injection runs hundreds of milliseconds after
/// the hotkey's own `Up` event — transcription, then polish — which is long
/// enough for the user to have genuinely pressed the hotkey again for their
/// *next* utterance. `GetAsyncKeyState` reading down at that point is not a
/// stuck leftover then; it is correct, current state, and synthesising a
/// release for it would be Iris injecting a key-up the user never made. That
/// event carries `LLKHF_INJECTED`, so the hook's own `is_hotkey_event`
/// correctly does not suppress it, and it reaches the focused app. That is
/// exactly the case that matters most: this whole thing is push-to-talk, so
/// back-to-back dictations are the expected way to use it, not an edge case.
///
/// `genuinely_held` (`hotkey::is_held`, gated on `hotkey::is_listening` by
/// the caller — see `release_hotkey_if_stuck`) is Iris's own hook's answer
/// to "is the hotkey currently held," driven only by real, non-injected key
/// transitions it has actually seen — never by polling live keyboard state.
/// A correction fires only when the two *disagree*: `GetAsyncKeyState` says
/// down, but Iris's own hook says no real press is currently in progress.
/// That disagreement is the actual signature of a desync, whatever produces
/// it — which this project could not confirm without the live testing
/// `CLAUDE.md` forbids, so this deliberately does not depend on a specific
/// theory of the mechanism to be correct. (`GetAsyncKeyState` and the hook's
/// bookkeeping update via genuinely independent paths on different threads,
/// so the two can also disagree for a narrow instant during a real repress;
/// `release_hotkey_if_stuck` gives that a brief window to settle before
/// sampling, which this function has no way to do — it can only trust the
/// two readings it is handed.)
///
/// Never a broader sweep of every standard modifier: any *other* key reading
/// down is one the user is holding for their own reasons, unrelated to
/// Iris, and touching it would be Iris reaching into input it did not
/// cause.
///
/// Returns `None` when `hotkey` is not [`Key::is_correctable_modifier`] (F8,
/// CapsLock, ScrollLock, Pause, F9, F10 cannot desync `SendInput`'s
/// modifier-state assumptions, because they are not modifiers; RightAlt and
/// RightWin *are* modifiers but are excluded anyway — see that method's
/// docs for why correcting them is unsound even when the desync is real).
/// Nothing to correct there is the correct outcome, not a coverage gap.
#[cfg(any(windows, test))]
fn modifier_to_release(hotkey: Key, hotkey_down: bool, genuinely_held: bool) -> Option<Key> {
    (hotkey_down && !genuinely_held && hotkey.is_correctable_modifier()).then_some(hotkey)
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

    use super::{Key, Method};

    const CF_UNICODETEXT: u32 = 13;
    const VK_CONTROL: u16 = 0x11;
    const VK_V: u16 = 0x56;

    /// `SendInput` takes an array; sending one call per keystroke would cost a
    /// syscall each and let other input interleave. Batches keep a transcript
    /// atomic from the target app's point of view. 512 events (256 characters)
    /// keeps the temporary array comfortably small.
    const BATCH: usize = 512;

    pub fn inject(text: &str, method: Method, hotkey: Key) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        match method {
            Method::SendInput => send_keystrokes(text, hotkey),
            Method::Clipboard => paste(text, hotkey),
        }
    }

    fn send_keystrokes(text: &str, hotkey: Key) -> Result<()> {
        release_hotkey_if_stuck(hotkey)?;

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

    /// Sampling `GetAsyncKeyState` and `hotkey::is_held` at exactly the same
    /// instant can land in the gap between them: on a genuine repress they
    /// update via genuinely independent paths (see `super::modifier_to_release`),
    /// so there is a real window where the async key state has already
    /// flipped but the hook thread has not yet processed the event and
    /// updated `HELD`. This heuristic — not a guarantee, just a reasonable
    /// margin over ordinary dispatch latency for an idle hook thread — bounds
    /// that window. It costs latency only when the hotkey reads down at all,
    /// never on the common path where it does not.
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(5);

    /// Correct the configured hotkey if Windows still thinks it is down.
    ///
    /// One dedicated `SendInput` call, issued and flushed before the burst
    /// that follows (text keystrokes, or the paste accelerator), so the
    /// correction is fully processed first rather than merely ordered first
    /// within a single array. Shared by both injection methods: a stuck
    /// hotkey can corrupt a `KEYEVENTF_UNICODE` burst here, and can just as
    /// well turn `paste`'s Ctrl+V into a different accelerator (Ctrl+Shift+V,
    /// Ctrl+Alt+V, ...) if the configured hotkey is Shift.
    ///
    /// Scoped to `hotkey` alone, never a sweep of every standard modifier —
    /// see `super::modifier_to_release` for why that matters.
    ///
    /// Skips entirely when no hook is installed
    /// (`!hotkey::is_listening()`) — see that function's docs. Paths that
    /// construct an injector without a live hotkey (`--speak-wav
    /// --really-inject`) have no way to tell a genuine press from a stuck
    /// one, so the only sound choice is not to guess.
    ///
    /// **Known, accepted gap:** nothing verifies that callers actually reach
    /// this function with the right `hotkey`, or that the `INPUT` it builds
    /// carries the flags `super::modifier_to_release` and
    /// `Key::needs_extended_flag` imply. Verifying that would mean executing
    /// a real `SendInput` call, which this repo's `CLAUDE.md` forbids — it
    /// types into whichever desktop the user is looking at, with no sandbox.
    /// The decision logic itself is fully covered by tests; this wiring is
    /// not, and cannot be without relaxing that constraint. This is
    /// deliberate, not an oversight: recorded here rather than papered over
    /// with a test that would only appear to cover it.
    fn release_hotkey_if_stuck(hotkey: Key) -> Result<()> {
        if !crate::hotkey::is_listening() {
            return Ok(());
        }
        let vk = hotkey.vk() as u16;
        let is_down = || unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
        if !is_down() {
            return Ok(());
        }
        // The hotkey reads down: give the hook thread SETTLE to catch up
        // before trusting that against `is_held` (see SETTLE's docs), then
        // take fresh readings of both.
        std::thread::sleep(SETTLE);
        let held = crate::hotkey::is_held();
        let Some(hotkey) = super::modifier_to_release(hotkey, is_down(), held) else {
            return Ok(());
        };
        // The root cause is unreproducible off the user's desk, so the code
        // itself is the field signal that confirms or refutes it.
        vlog!("releasing the hotkey (0x{vk:02X}) still reported down before injecting");

        let mut flags = KEYEVENTF_KEYUP;
        if hotkey.needs_extended_flag() {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        let mut correction = vec![key_event(vk, 0, flags)];
        flush(&mut correction)
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
    fn paste(text: &str, hotkey: Key) -> Result<()> {
        release_hotkey_if_stuck(hotkey)?;
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
        assert!(inject("", Method::SendInput, Key::default()).is_ok());
    }

    #[test]
    fn a_genuinely_stuck_hotkey_modifier_needs_releasing() {
        // GetAsyncKeyState says down; Iris's own hook says no real press is
        // currently in progress. A genuine desync, whatever produces it.
        assert_eq!(
            modifier_to_release(Key::RightCtrl, true, false),
            Some(Key::RightCtrl)
        );
    }

    #[test]
    fn a_hotkey_modifier_reading_up_needs_nothing() {
        assert_eq!(modifier_to_release(Key::RightCtrl, false, false), None);
    }

    #[test]
    fn a_genuine_repress_for_the_next_utterance_is_left_alone() {
        // The race this two-signal design exists to prevent: GetAsyncKeyState
        // reads down because the user is legitimately holding the hotkey
        // again for their next dictation, and Iris's own hook agrees (it saw
        // a real, non-injected Down). Correcting here would inject an orphan
        // key-up into the focused app for a press the user is still making.
        assert_eq!(modifier_to_release(Key::RightCtrl, true, true), None);
    }

    #[test]
    fn every_correctable_modifier_can_be_the_configured_hotkey() {
        // Whichever of the three, a genuine desync is caught.
        for key in [Key::RightCtrl, Key::LeftCtrl, Key::RightShift] {
            assert_eq!(modifier_to_release(key, true, false), Some(key), "{key}");
        }
    }

    #[test]
    fn a_non_modifier_hotkey_needs_no_correction_even_when_held() {
        // F8 cannot interfere as a modifier because it is not one — clearing
        // nothing here is correct behaviour, not a coverage gap.
        for key in [
            Key::CapsLock,
            Key::ScrollLock,
            Key::Pause,
            Key::F8,
            Key::F9,
            Key::F10,
        ] {
            assert_eq!(modifier_to_release(key, true, false), None, "{key}");
        }
    }

    #[test]
    fn alt_and_win_are_never_corrected_even_on_a_genuine_desync() {
        // These ARE modifiers, and this IS the true-desync case (not a
        // repress) — the case the correction is supposed to catch. It must
        // still do nothing: a corrective key-up for either is an orphan
        // Alt/Win release with no matching press the focused app ever saw,
        // which is the exact live-desktop hazard this feature exists to
        // avoid, regardless of whether the correction is "working as
        // intended." See Key::is_correctable_modifier.
        for key in [Key::RightAlt, Key::RightWin] {
            assert_eq!(modifier_to_release(key, true, false), None, "{key}");
        }
    }

    /// One synthetic keyboard event, holding the fields of the `INPUT` that
    /// `win::key_event` fills in. `mod win` is `cfg(windows)` and its events
    /// can only ever be observed by calling `SendInput` for real — which this
    /// project forbids (see the crate docs and `CLAUDE.md`) — so the two
    /// `SendInput` calls below are reconstructed from the same portable
    /// decision functions the real code calls, exactly as `text.rs` already
    /// mirrors `send_keystrokes`'s batching loop. This proves the *decisions*
    /// are right; it cannot prove `mod win` actually wires them up this way,
    /// nor does it model `release_hotkey_if_stuck`'s `is_listening` gate or
    /// its settle-and-recheck retry — see that function's doc comment for
    /// those accepted gaps.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Event {
        vk: u16,
        scan: u16,
        unicode: bool,
        key_up: bool,
        extended: bool,
    }

    /// The two flushes `send_keystrokes` performs, in order: the corrective
    /// key-up for the configured hotkey if it reads down and Iris's own hook
    /// does not believe it is genuinely held, then the text burst.
    fn burst(
        text: &str,
        hotkey: Key,
        hotkey_down: bool,
        genuinely_held: bool,
    ) -> (Vec<Event>, Vec<Event>) {
        let corrections = modifier_to_release(hotkey, hotkey_down, genuinely_held)
            .into_iter()
            .map(|key| Event {
                vk: key.vk() as u16,
                scan: 0,
                unicode: false,
                key_up: true,
                extended: key.needs_extended_flag(),
            })
            .collect();

        let mut keystrokes = Vec::new();
        for unit in crate::text::plan(text) {
            let (scan, vk, unicode) = match unit {
                crate::text::KeyUnit::Unicode(u) => (u, 0u16, true),
                crate::text::KeyUnit::Virtual(vk) => (0u16, vk, false),
            };
            for key_up in [false, true] {
                keystrokes.push(Event {
                    vk,
                    scan,
                    unicode,
                    key_up,
                    extended: false,
                });
            }
        }
        (corrections, keystrokes)
    }

    /// What a target window would end up with, reading only the key-down
    /// Unicode events back out of the burst.
    fn typed(keystrokes: &[Event]) -> String {
        let units: Vec<u16> = keystrokes
            .iter()
            .filter(|e| e.unicode && !e.key_up)
            .map(|e| e.scan)
            .collect();
        String::from_utf16(&units).expect("the burst is valid UTF-16")
    }

    #[test]
    fn the_reported_failure_case_types_byte_for_byte_with_the_hotkey_still_down() {
        // The bug report: 25 characters spoken, 25 characters delivered, but
        // everything after the first word arrived as literal 0x20. This
        // reconstructs the scenario where push-to-talk (VK_RCONTROL) reads
        // down at injection time with no genuine repress behind it — a
        // desync `GetAsyncKeyState` alone cannot be trusted to explain, but
        // one `modifier_to_release`'s cross-check still catches and corrects.
        let text = crate::text::prepare("This is a full sentence.", true);
        let (corrections, keystrokes) = burst(&text, Key::RightCtrl, true, false);

        // The correction is one dedicated key-up, flushed before any text.
        assert_eq!(
            corrections,
            vec![Event {
                vk: 0xA3,
                scan: 0,
                unicode: false,
                key_up: true,
                extended: true,
            }],
            "RightCtrl needs KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY, or the \
             target decodes it as LeftCtrl and the hotkey stays down"
        );

        // The correction can never itself type: key-ups only, no Unicode.
        assert!(corrections.iter().all(|e| e.key_up && !e.unicode));

        // And the text that follows is the transcript, unaltered.
        assert_eq!(typed(&keystrokes), "This is a full sentence. ");
        assert_eq!(keystrokes.len(), 50, "25 characters, down + up each");

        // The specific corruption reported: everything past "This" became
        // 0x20. Only the five real spaces may be 0x20.
        let spaces = keystrokes
            .iter()
            .filter(|e| e.unicode && !e.key_up && e.scan == 0x20)
            .count();
        assert_eq!(spaces, 5, "no character was substituted with a space");
    }

    #[test]
    fn nothing_is_prepended_when_the_hotkey_is_not_down() {
        // The shorter dictation that came out byte-perfect on the same build:
        // with the hotkey not held there is no extra SendInput call at all,
        // so the correction cannot change the common path.
        let (corrections, keystrokes) = burst("hello ", Key::RightCtrl, false, false);
        assert!(corrections.is_empty());
        assert_eq!(typed(&keystrokes), "hello ");
    }

    #[test]
    fn a_genuine_back_to_back_dictation_gets_no_orphan_keyup() {
        // The race a single-signal design would get wrong: the user has
        // already pressed the hotkey again for their next utterance while
        // this one is still being injected. GetAsyncKeyState correctly reads
        // down, Iris's own hook agrees a real press is in progress, and the
        // burst must carry no correction at all — an orphan key-up here
        // would reach the focused app (menu-bar/Start-menu activation risk)
        // for a press the user is still actively making.
        let (corrections, keystrokes) = burst("go ", Key::RightCtrl, true, true);
        assert!(
            corrections.is_empty(),
            "a genuine repress must never be corrected"
        );
        assert_eq!(typed(&keystrokes), "go ");
    }

    #[test]
    fn every_correctable_modifier_is_corrected_when_configured_as_the_hotkey() {
        for key in [Key::RightCtrl, Key::LeftCtrl, Key::RightShift] {
            let (corrections, keystrokes) = burst("ok ", key, true, false);
            assert_eq!(corrections.len(), 1, "{key}");
            assert_eq!(corrections[0].vk, key.vk() as u16);
            assert_eq!(typed(&keystrokes), "ok ");
        }
    }

    #[test]
    fn alt_and_win_produce_no_correction_event_in_the_burst_either() {
        // Same guarantee as alt_and_win_are_never_corrected_even_on_a_genuine_desync,
        // through the full event-reconstruction path this time.
        for key in [Key::RightAlt, Key::RightWin] {
            let (corrections, keystrokes) = burst("ok ", key, true, false);
            assert!(corrections.is_empty(), "{key}");
            assert_eq!(typed(&keystrokes), "ok ");
        }
    }

    /// Renders the event stream above as a reviewable transcript. Run with
    /// `cargo test -p iris-core -- --nocapture injection_transcript`.
    #[test]
    fn injection_transcript_for_the_reported_failure_case() {
        fn hex(s: &str) -> String {
            s.encode_utf16()
                .map(|u| format!("{u:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        }

        let spoken = "This is a full sentence.";
        let text = crate::text::prepare(spoken, true);
        let (corrections, keystrokes) = burst(&text, Key::RightCtrl, true, false);
        let out = typed(&keystrokes);

        println!("\n=== Iris injection burst — reported failure case ===");
        println!("transcript to inject : {spoken:?}  (+1 trailing space)");
        println!("state at burst time  : VK_RCONTROL (0xA3) reads down via GetAsyncKeyState,");
        println!("                       but Iris's own hook reports no genuine press in");
        println!("                       progress — the desync `modifier_to_release` corrects\n");

        println!(
            "SendInput call 1 — correction, {} event(s):",
            corrections.len()
        );
        for e in &corrections {
            let flags = if e.extended {
                "KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY"
            } else {
                "KEYEVENTF_KEYUP"
            };
            println!("  wVk=0x{:02X} wScan=0x{:04X} {flags}", e.vk, e.scan);
        }
        println!(
            "\nSendInput call 2 — text, {} event(s), first 6 shown:",
            keystrokes.len()
        );
        for e in keystrokes.iter().take(6) {
            println!(
                "  wVk=0x{:02X} wScan=0x{:04X} KEYEVENTF_UNICODE{}   ({:?})",
                e.vk,
                e.scan,
                if e.key_up { " | KEYEVENTF_KEYUP" } else { "" },
                char::from_u32(e.scan as u32).unwrap()
            );
        }
        println!("  ... {} more", keystrokes.len() - 6);

        println!(
            "\nspoken   ({:2} chars): {}",
            text.chars().count(),
            hex(&text)
        );
        println!("delivered({:2} chars): {}", out.chars().count(), hex(&out));
        println!(
            "reported (25 chars): {}   <- the bug: 'This' then 21x 0x20",
            hex("This                     ")
        );
        println!(
            "\nresult: delivered == spoken -> {}\n",
            if out == text { "MATCH" } else { "MISMATCH" }
        );
        assert_eq!(out, text);
    }
}

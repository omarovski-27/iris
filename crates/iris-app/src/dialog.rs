//! A native message box for a launch with no console to print to.
//!
//! `iris` is a GUI-subsystem binary, so a double-click, the Start Menu
//! shortcut and the Startup shortcut all give it no console at all — nothing
//! for `eprintln!` to reach. [`show`] is the fallback, and it lives in this
//! library crate rather than in the `iris` binary specifically so both
//! `main`'s startup/run-failure reporting and [`crate::notify`]'s
//! injection-failure notice can reach the same mechanism instead of keeping
//! two copies that could drift.

/// Show `message` under `caption` in an error-styled message box, unless
/// someone is watching a console that will outlive the process.
///
/// A missing key, an unparseable config file, a microphone that is not
/// there, a hook Windows took away, a dictation that could not be typed:
/// each is an actionable sentence, and on a console-less launch each would
/// otherwise vanish with nothing on screen to say so. When a real terminal
/// invocation reattached a console (`main`'s `attach_console_for_cli_output`),
/// the message survives on stderr there instead and this dialog would be
/// redundant; `GetConsoleProcessList` reporting more than one process — us
/// and the shell that launched us — is what tells the two launch paths
/// apart.
#[cfg(windows)]
pub fn show(caption: &str, message: &str) {
    show_with_icon(caption, message);
}

/// Off Windows there is no console-less launch path and no dialog to show.
#[cfg(not(windows))]
pub fn show(_caption: &str, _message: &str) {}

// `MessageBoxW` and `GetConsoleProcessList` have no safe wrapper. Both calls
// are self-contained — one fills a stack buffer we own, the other shows a
// modal with NUL-terminated strings that outlive it — so the opt-in is one
// function wide.
#[allow(unsafe_code)]
#[cfg(windows)]
fn show_with_icon(caption: &str, message: &str) {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::System::Console::GetConsoleProcessList;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };

    let mut pids = [0u32; 2];
    // SAFETY: `pids` is a valid, writable buffer of the length passed in;
    // the call only fills it and returns the count.
    let sharing_console = unsafe { GetConsoleProcessList(&mut pids) } > 1;
    if sharing_console {
        return;
    }

    let text = HSTRING::from(message);
    let caption = HSTRING::from(caption);
    // MB_SETFOREGROUND and MB_TOPMOST are what make this visible at all in the
    // launch it exists for: a process started from the Startup folder has
    // never held the foreground, so Windows' foreground lock would otherwise
    // leave the box behind the active window with only a flashing taskbar
    // button, and there is no console window to notice instead.
    //
    // SAFETY: both strings are NUL-terminated and outlive the modal call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

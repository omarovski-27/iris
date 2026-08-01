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
//! [`inject`] reaches for that escape hatch itself for anything long enough to
//! need it — see [`effective_method`] — rather than leaving it a manual
//! opt-in. The failure it exists to avoid is silent: `SendInput` reports full
//! success even when a slow-consuming target garbles what it received.
//!
//! Neither strategy is safe everywhere, so nothing here is unconditional:
//!
//! * A window that does not treat Ctrl+V as paste — a terminal, an RDP
//!   client, vim — never receives the escape hatch ([`accepts_paste`]).
//! * A hotkey still reading down would spoil the paste chord, and is not
//!   always correctable ([`paste_accelerator_survives`]).
//! * The clipboard can simply be unavailable, held open by another process.
//!
//! Every one of those routes back to keystrokes, and a keystroke burst past
//! [`MAX_SEND_INPUT_CHARS`] is *paced* ([`pacing`]) rather than sent flat out.
//! What that is worth differs by route, and the difference matters: declining
//! to paste into a terminal puts the text where this project's evidence says
//! it already arrives intact, while a clipboard failure puts an *ordinary*
//! target — the kind the corruption was reported in — on a burst that is
//! merely paced, and the pacing constants are a reasoned guess, not a
//! measurement (see [`pacing`]). So nothing here is left on the *unpaced*
//! burst the escalation exists to avoid; that is the claim this module can
//! actually support.
//!
//! Nothing in this module can tell whether the target rendered the text. Every
//! success here means "delivered into the system's input stream" — the same
//! thing `SendInput` reported when it returned full success for the burst
//! Notepad garbled. There is no OS signal for the stronger claim, so the
//! recoverability of the text elsewhere (the clipboard it is left on, the
//! session log) is what bounds the damage, not a confirmation this layer
//! could check.
//!
//! Both are measured; see `docs/spike-findings.md`.

use anyhow::{bail, Result};

use crate::hotkey::Key;
use crate::vlog;

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
///
/// `method` is a request, not a guarantee: see [`effective_method`] for when
/// a long transcript overrides it, and [`pacing`] for what happens to a long
/// transcript that stays on keystrokes anyway.
pub fn inject(text: &str, method: Method, hotkey: Key) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    // Asked once, here, and then handed to both things that need it. Only a
    // transcript long enough to be escalated ever consults the target, so the
    // ordinary short dictation — the overwhelming common case — pays none of
    // the OS calls `focused_target` makes.
    let needs_second_batch = needs_more_than_one_batch(text);
    let target = needs_second_batch.then(focused_target).flatten();
    let effective = effective_method(needs_second_batch, method, target.as_ref());
    if let (Method::SendInput, Some(target)) = (effective, &target) {
        vlog!(
            "{} ({}) does not take Ctrl+V as paste; typing this {}-character \
             transcript in paced bursts instead",
            target.process,
            target.class,
            text.chars().count()
        );
    }
    imp(text, effective, hotkey)
}

/// The window that will receive the text, as far as [`accepts_paste`] needs
/// to know it: two strings, so the decision that reads them stays portable
/// and testable off Windows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Target {
    /// `GetClassNameW` of the foreground window, e.g. `ConsoleWindowClass`.
    class: String,
    /// The owning executable's file name, e.g. `nvim.exe`.
    process: String,
}

#[cfg(windows)]
fn focused_target() -> Option<Target> {
    win::foreground_target()
}

/// Off Windows nothing is ever injected (see [`imp`]), so there is nothing to
/// identify.
#[cfg(not(windows))]
fn focused_target() -> Option<Target> {
    None
}

/// Window classes that do not treat Ctrl+V as paste, or bind it to their own
/// meaning. Matched case-insensitively against [`Target::class`].
///
/// Best-effort and permanently incomplete — see [`accepts_paste`]. Entries
/// are added when a family is both known-hostile and actually identifiable;
/// the list is not working towards covering every such app, because it
/// cannot.
const PASTE_HOSTILE_CLASSES: &[&str] = &[
    "consolewindowclass",           // conhost: cmd, powershell, ssh sessions
    "cascadia_hosting_window_class", // Windows Terminal
    "virtualconsoleclass",          // ConEmu, and Cmder which embeds it
    "putty",
    "mintty",
    "tscshellcontainerclass", // mstsc, the Remote Desktop client
    "vmplayerframewindow",    // VMware Workstation Player
    "vmuiframe",              // VMware Workstation
    "vim",                    // gvim
    // C-v is scroll-up-command in default Emacs bindings, so an escalated
    // paste scrolls the buffer and inserts nothing at all.
    "emacs",
];

/// The same, by executable name — the class name of a window is up to its
/// author and several of these apps host themselves in a generic one.
///
/// This is the only way to reach an Electron terminal: Hyper and Tabby both
/// register the generic `Chrome_WidgetWin_1` class they share with every
/// other Electron app, including ones that paste perfectly well, so that
/// class must never go in the list above.
const PASTE_HOSTILE_PROCESSES: &[&str] = &[
    "vim.exe",
    "gvim.exe",
    "nvim.exe",
    "nvim-qt.exe",
    "neovide.exe",
    "emacs.exe",
    "conhost.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "windowsterminal.exe",
    "conemu64.exe",
    "cmder.exe",
    "hyper.exe",
    "tabby.exe",
    "mintty.exe",
    "putty.exe",
    "kitty.exe",
    "alacritty.exe",
    "wezterm-gui.exe",
    "mstsc.exe",
    "vmconnect.exe",
];

/// Whether Ctrl+V would actually paste into `target`.
///
/// [`Method::Clipboard`] gives up `SendInput`'s defining advantage — it
/// "works in essentially every control, including ones that refuse paste" —
/// in exchange for delivering any length in four events. That trade is only
/// worth making where the accelerator means paste. Where it does not, sending
/// it is not a slower delivery, it is a *different* corruption: vim in insert
/// mode takes Ctrl+V as literal-next-character and inserts a control
/// sequence; conhost, PuTTY and most RDP/VM client windows either bind it
/// elsewhere or forward it to the guest.
///
/// Terminals are the sharpest case, and also the reassuring one: they are the
/// targets that never reproduced the burst corruption in the first place (see
/// [`effective_method`]), so declining to paste into them costs nothing — the
/// keystroke path is exactly where the evidence says they are safe.
///
/// This is a deny-list, and it is **best-effort and permanently incomplete**,
/// not a completeness mechanism working towards full coverage. It cannot
/// enumerate every paste-hostile app, it cannot see a *mode* (vim run inside
/// a terminal is caught by the terminal, but an embedded terminal inside an
/// editor is not), and it can only name families that are actually
/// identifiable from a window class or an image name. Anything else would be
/// an arms race this project cannot win, and pretending otherwise would be
/// the more dangerous error.
///
/// What makes that acceptable is not the list's reach but the fact that a
/// miss is recoverable rather than silent. It is default-*allow*, so its two
/// failure directions are asymmetric and both bounded: a harmless app wrongly
/// listed gets a paced keystroke burst, which is correct, just slower; a
/// hostile app the list misses pastes nothing into the target — and the
/// transcript is still on the clipboard afterwards (`win::paste` never
/// restores the previous contents, so the user can paste it themselves with
/// whatever key that app does use) and still in the session log. That is the
/// documented recovery path, stated for users in
/// `crates/iris-app/README.md`, not an accident of the restore decision.
///
/// An unidentifiable window (`None` — no foreground window, or a process this
/// one may not query) is treated the same as an unlisted one, which is what
/// keeps the originally reported failure — a plain edit control — fixed.
fn accepts_paste(target: Option<&Target>) -> bool {
    let Some(target) = target else {
        return true;
    };
    let class = target.class.to_ascii_lowercase();
    let process = target.process.to_ascii_lowercase();
    !PASTE_HOSTILE_CLASSES.contains(&class.as_str())
        && !PASTE_HOSTILE_PROCESSES.contains(&process.as_str())
}

/// Whether Ctrl+V still means paste with `hotkey` reading down.
///
/// The paste accelerator is input surface the automatic escalation
/// introduced, and it needs the same protection `release_hotkey_if_stuck`
/// gives a keystroke burst — but it cannot get all of it from there.
/// [`modifier_to_release`] deliberately declines to correct `RightAlt` and
/// `RightWin` at all, and declines to correct anything during a genuine
/// repress; both exclusions are load-bearing (see
/// [`Key::is_correctable_modifier`]) and neither may be widened for this.
/// Yet a `ralt`-configured user dictating 300 characters would otherwise send
/// Ctrl+Alt+V, and a user starting their next push-to-talk on `rshift` while
/// the previous transcript injects would send Ctrl+Shift+V — back-to-back
/// dictation being the documented common case, not an edge.
///
/// So this extends the protection the only way that does not touch those
/// exclusions: it is sampled *after* the correction has had its chance, and
/// a hotkey still down that would spoil the chord ([`Key::alters_paste_chord`])
/// makes the paste unusable rather than the correction broader. The caller
/// then types the transcript instead — the same fallback a held clipboard
/// takes. Declining to paste injects nothing, so unlike a correction it is
/// safe for exactly the keys the correction must not touch.
///
/// "After the correction has had its chance" is a claim about the *reading*,
/// not merely about statement order, and it is the caller's to honour:
/// `GetAsyncKeyState` does not update synchronously for an injected key-up,
/// so a reading taken the instant a correction was flushed can still say
/// down and veto a paste the correction just made safe. `win::settled_reads_down`
/// is what makes the reading honest, and it is the only thing that should
/// produce `hotkey_down` here.
///
/// `win::paste` asks this twice, before and after writing the clipboard, for
/// reasons given there; both answers come from this one rule.
#[cfg(any(windows, test))]
fn paste_accelerator_survives(hotkey: Key, hotkey_down: bool) -> bool {
    !(hotkey_down && hotkey.alters_paste_chord())
}

/// How a keystroke burst is split, when it must be.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pace {
    /// Planned units per `SendInput` call.
    units: usize,
    /// How long to wait between calls.
    gap: std::time::Duration,
}

/// Units per `SendInput` call once a burst needs pacing, and the pause
/// between calls. Both are heuristics — the mechanism they answer to is a
/// target's message loop falling behind, which this project cannot measure
/// without the live injection `CLAUDE.md` forbids — chosen to be obviously
/// on the safe side of the numbers it does have: a quarter of the batch the
/// reported failure exceeded, with a gap several times an ordinary
/// message-dispatch interval. The whole cost lands only on transcripts that
/// already opted out of pasting: the reported 313-character one becomes five
/// calls and four gaps, some 32 ms against a sub-second budget.
#[cfg(any(windows, test))]
const PACED_UNITS: usize = 64;
#[cfg(any(windows, test))]
const PACE_GAP: std::time::Duration = std::time::Duration::from_millis(8);

/// How to deliver a `units`-long keystroke burst; `None` for one flat burst.
///
/// Pacing is what makes the keystroke path a legitimate fallback rather than
/// a return to the reported bug. Everything that declines to paste — a
/// paste-hostile target, a spoiled accelerator, an unavailable clipboard —
/// lands here with a transcript that may be arbitrarily long, and the failure
/// [`effective_method`] describes is a property of *sustained* burst length
/// against a slow consumer. Splitting the burst and yielding between the
/// pieces gives that consumer's message loop room to drain, which one
/// uninterrupted `SendInput` array never does.
///
/// Nothing at or under [`MAX_SEND_INPUT_CHARS`] is paced: that is the regime
/// this project has evidence is already sound, and it is the overwhelmingly
/// common one, so the ordinary dictation pays nothing for this.
///
/// The cost is not only wall-clock. Each gap is an open window in which the
/// user's own keystrokes can land *inside* the transcript — most plausibly
/// their next push-to-talk press, since back-to-back dictation is the
/// documented common case for this product (see [`modifier_to_release`]) and
/// a long transcript is exactly what is still injecting when it happens. A
/// long burst was already split into several `SendInput` calls before pacing
/// existed, so this widens that window rather than opening it; the trade is
/// accepted deliberately, because interleaved input is visible and
/// correctable where the silent garbling this guards against is neither.
#[cfg(any(windows, test))]
fn pacing(units: usize) -> Option<Pace> {
    (units > MAX_SEND_INPUT_CHARS).then_some(Pace {
        units: PACED_UNITS,
        gap: PACE_GAP,
    })
}

/// How many keystroke events `win::send_keystrokes` fits in one `SendInput`
/// batch before it must flush and start another. 512 events (256 characters)
/// keeps the temporary array comfortably small. Kept here, outside `mod win`,
/// so [`MAX_SEND_INPUT_CHARS`] and [`effective_method`] can reason about it
/// without an OS call.
const BATCH: usize = 512;

/// The longest transcript still delivered as [`Method::SendInput`] before
/// [`effective_method`] escalates to [`Method::Clipboard`].
///
/// Exactly `BATCH / 2`, not a rounder or larger number: it is the one line
/// this project has actual evidence for. Every Basic-Multilingual-Plane
/// character plans to one `KeyUnit` — a down and an up, two `SendInput`
/// events — so this is the largest text that still fits in the single batch
/// every previously analysed failure also fit inside (the hotkey desync
/// [`modifier_to_release`] corrects, not this one); this project's only
/// evidence of the failure [`effective_method`] guards against is a
/// transcript that needed a second batch. Drawing the line anywhere past
/// this would be a guess this project cannot verify without the live
/// testing `CLAUDE.md` forbids.
///
/// Characters outside the BMP (astral — emoji, for instance) are a
/// surrogate pair, two `KeyUnit`s, and so cross the same *event* threshold
/// at fewer *characters*; [`needs_more_than_one_batch`] — the one place the
/// length question is asked — compares against planned units, not
/// `chars().count()`, for exactly that reason. This constant names the
/// common-case BMP number for readability, not the literal comparison.
const MAX_SEND_INPUT_CHARS: usize = BATCH / 2;

/// Escalate `requested` to [`Method::Clipboard`] when the transcript needs
/// more than one `SendInput` batch to deliver.
///
/// `needs_second_batch` is that answer, not the text it was derived from:
/// [`needs_more_than_one_batch`] asks the length question, [`inject`] asks it
/// once and passes the result down here — see [`MAX_SEND_INPUT_CHARS`] for
/// where the line sits and why.
///
/// The reported failure this guards against: a 313-character transcript —
/// transcribed perfectly, `SendInput` reporting full success — arrived in
/// Notepad as "Test i" then "ing" then one character repeated some 70 times
/// then roughly 180 spaces, while the identical bytes from the identical code
/// path land correctly in a terminal every time. `SendInput`'s return value
/// only means the events entered the system's input stream, never that the
/// target's message loop drained them before more arrive; a slow,
/// per-keystroke consumer (a classic edit control does undo-buffer and
/// dirty-flag bookkeeping on every `WM_CHAR`) can fall behind a sustained
/// synthetic burst in a way a terminal's input queue — built to absorb
/// bursts, since that is what every paste or piped command already looks
/// like to it — does not. That is a property of the target and the burst's
/// length, not of any specific character or offset, which is why the
/// reported corruption started within the first ten characters rather than
/// at the batch boundary itself: the boundary is evidence the burst was long
/// enough to matter, not the mechanism. A repeated character with no matching
/// release is exactly the signature of a key-down whose key-up fell behind.
///
/// `Method::Clipboard` sidesteps the whole class: four events land regardless
/// of transcript length, so there is no sustained burst for a slow consumer
/// to fall behind and no down/up pairing to desync — at the cost of
/// clobbering the clipboard; see `win::paste`'s doc comment for why that
/// trade is accepted as-is rather than mitigated with a restore.
///
/// Not unconditional on the target: a window that does not take Ctrl+V as
/// paste keeps the keystroke path, where [`pacing`] handles the same burst
/// problem a different way. See [`accepts_paste`].
///
/// Never downgrades: a caller that already asked for `Clipboard` is
/// unaffected, and a text that fits in one batch keeps whatever the caller
/// requested.
///
/// **Wanted, and not yet expressible here:** a user who writes
/// `method = "sendinput"` in `config.toml` deliberately — to protect their
/// clipboard, or because their app is one no deny-list will ever name —
/// should be exempt from escalation entirely, while the default stays
/// automatic. That is the override that answers "my app is not on your list"
/// without a list. It cannot be decided in this function as it stands:
/// [`Method`] has two variants and `Method::SendInput` is both the default
/// and the explicit choice, so `requested` cannot carry the distinction. The
/// signal has to come from the config layer, which knows whether the key was
/// present in the file — `iris-app`'s `config.rs` and the `SystemInjector`
/// construction in `main.rs`, neither of which this change may touch (a
/// concurrent branch owns that surface, and `InjectConfig::default()` calls
/// `Method::default()`, so manufacturing the distinction by flipping the
/// `Default` impl would break a pinned test there instead of adding the
/// feature).
fn effective_method(
    needs_second_batch: bool,
    requested: Method,
    target: Option<&Target>,
) -> Method {
    if requested == Method::SendInput && needs_second_batch && accepts_paste(target) {
        Method::Clipboard
    } else {
        requested
    }
}

/// Whether `text` would need more than one `SendInput` batch — the single
/// length question this module asks.
///
/// Asked exactly once, by [`inject`], which uses the answer for both things
/// that depend on it: whether looking the target up is worth the OS calls,
/// and whether to escalate. [`effective_method`] is handed that same answer
/// rather than recomputing it, so the two cannot drift: were the lookup ever
/// gated more narrowly than the escalation, `effective_method` would be
/// handed `None` for a window it was supposed to examine and would escalate
/// into it — silently, since the log line that explains a declined escalation
/// only fires when there is a target to name. Walking [`crate::text::plan_len`]
/// once also keeps the cost off a path measured in milliseconds.
fn needs_more_than_one_batch(text: &str) -> bool {
    crate::text::plan_len(text) > MAX_SEND_INPUT_CHARS
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
    use windows::core::{w, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetClassNameW, GetForegroundWindow,
        GetWindowThreadProcessId, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };

    use crate::text::{self, KeyUnit};
    use crate::vlog;

    use super::{Key, Method, Target, BATCH};

    const CF_UNICODETEXT: u32 = 13;
    const VK_CONTROL: u16 = 0x11;
    const VK_V: u16 = 0x56;

    // `SendInput` takes an array; sending one call per keystroke would cost a
    // syscall each and let other input interleave. Batches keep a transcript
    // atomic from the target app's point of view. The batch size — and,
    // above it, whether to pace the calls apart at all — is decided outside
    // this module; see `super::effective_method` and `super::pacing` for why.

    pub fn inject(text: &str, method: Method, hotkey: Key) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        match method {
            Method::SendInput => send_keystrokes(text, hotkey),
            Method::Clipboard => paste(text, hotkey),
        }
    }

    /// Identify the window that will receive the text.
    ///
    /// Best-effort by design: every step here can fail for reasons that say
    /// nothing about the target (no foreground window during a focus change,
    /// a higher-integrity process this one may not open), and `None` is a
    /// sound answer to all of them — `super::accepts_paste` treats an
    /// unidentified window as an ordinary one.
    pub fn foreground_target() -> Option<Target> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return None;
            }
            let mut buf = [0u16; 256];
            let len = GetClassNameW(hwnd, &mut buf);
            let class = String::from_utf16_lossy(&buf[..len.max(0) as usize]);

            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let process = process_name(pid).unwrap_or_default();

            Some(Target { class, process })
        }
    }

    /// The file name of `pid`'s executable. `PROCESS_QUERY_LIMITED_INFORMATION`
    /// is the right of the two: it is granted for processes at a higher
    /// integrity level, which the focused window's often is.
    fn process_name(pid: u32) -> Option<String> {
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            let queried = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(process);
            queried.ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            Some(path.rsplit(['\\', '/']).next()?.to_string())
        }
    }

    fn send_keystrokes(text: &str, hotkey: Key) -> Result<()> {
        // Nothing here reads the key's state afterwards, so whether a
        // correction was made changes nothing about what follows.
        let _ = release_hotkey_if_stuck(hotkey)?;

        let units = text::plan(text);
        let pace = super::pacing(units.len());
        // Unpaced, this is the single batch every short transcript has always
        // been sent as; paced, it is one of several deliberately smaller ones.
        let per_call = pace.map_or(BATCH, |p| p.units * 2);

        let mut inputs = Vec::with_capacity(per_call);
        let last = units.len().saturating_sub(1);
        for (i, unit) in units.iter().enumerate() {
            let (scan, vk, extra) = match unit {
                KeyUnit::Unicode(u) => (*u, 0u16, KEYEVENTF_UNICODE),
                KeyUnit::Virtual(vk) => (0u16, *vk, KEYBD_EVENT_FLAGS(0)),
            };
            inputs.push(key_event(vk, scan, extra));
            inputs.push(key_event(vk, scan, extra | KEYEVENTF_KEYUP));

            if inputs.len() >= per_call && unit.may_end_batch() && i < last {
                flush(&mut inputs)?;
                if let Some(pace) = pace {
                    // The point of pacing: hand the target's message loop
                    // time to drain what it has before sending more.
                    std::thread::sleep(pace.gap);
                }
            }
        }
        flush(&mut inputs)
    }

    /// Reports whether Windows currently believes `key` is down.
    ///
    /// This is the raw `GetAsyncKeyState` reading that both
    /// `super::modifier_to_release` and `super::paste_accelerator_survives`
    /// are handed as their `hotkey_down` argument. It is only ever one of the
    /// two signals a correction needs — see `release_hotkey_if_stuck` for why
    /// it is never trusted alone.
    fn reads_down(key: Key) -> bool {
        unsafe { GetAsyncKeyState(key.vk() as i32) as u16 & 0x8000 != 0 }
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

    /// Whether `release_hotkey_if_stuck` actually injected a corrective
    /// key-up. Only the caller that goes on to *read* the key's state needs
    /// this, and it needs it for the same reason `SETTLE` exists: the reading
    /// and the thing that changed it update through independent paths.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Correction {
        None,
        Injected,
    }

    /// `reads_down`, but not poisoned by a correction this code just made.
    ///
    /// `GetAsyncKeyState` reflects an injected key-up no more synchronously
    /// than a real one, so sampling immediately after `release_hotkey_if_stuck`
    /// flushed a correction can report the very state the correction just
    /// cleared. Anything deciding on that reading would then act on a desync
    /// that no longer exists — `paste` would veto its own accelerator and
    /// drop a long transcript onto the keystroke path the escalation exists
    /// to get off, for no reason the user could see.
    ///
    /// So the settle is paid on exactly the readings that need it. A
    /// correction is rare (it needs a genuine desync, and `RightAlt` and
    /// `RightWin` never get one at all), and where none was injected there is
    /// nothing to wait for: the common path — hotkey never down — stays free,
    /// exactly as `SETTLE` already promises for the correction itself. Per
    /// *reading*, not per call into `inject`: `paste` reads twice and can then
    /// fall back to `send_keystrokes`, which corrects again — see `paste` for
    /// why that repetition is left alone.
    fn settled_reads_down(key: Key, corrected: Correction) -> bool {
        if corrected == Correction::Injected {
            std::thread::sleep(SETTLE);
        }
        reads_down(key)
    }

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
    /// Reports whether it injected anything. A caller that only sends a burst
    /// afterwards can ignore that; a caller that goes on to *read* the key's
    /// state cannot, because its own correction is the thing most likely to
    /// make that reading stale — see `settled_reads_down`.
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
    fn release_hotkey_if_stuck(hotkey: Key) -> Result<Correction> {
        if !crate::hotkey::is_listening() {
            return Ok(Correction::None);
        }
        let vk = hotkey.vk() as u16;
        let is_down = || reads_down(hotkey);
        if !is_down() {
            return Ok(Correction::None);
        }
        // The hotkey reads down: give the hook thread SETTLE to catch up
        // before trusting that against `is_held` (see SETTLE's docs), then
        // take fresh readings of both — the async key state first, so a
        // repress that begins after it reads `false` cannot fire a
        // correction, and one that began before it has the whole
        // `is_held` call's worth of extra time to reach `HELD`.
        std::thread::sleep(SETTLE);
        let down = is_down();
        let held = crate::hotkey::is_held();
        let Some(hotkey) = super::modifier_to_release(hotkey, down, held) else {
            return Ok(Correction::None);
        };
        // The root cause is unreproducible off the user's desk, so the code
        // itself is the field signal that confirms or refutes it.
        vlog!("releasing the hotkey (0x{vk:02X}) still reported down before injecting");

        let mut flags = KEYEVENTF_KEYUP;
        if hotkey.needs_extended_flag() {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        let mut correction = vec![key_event(vk, 0, flags)];
        flush(&mut correction)?;
        Ok(Correction::Injected)
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
    /// Known limitation, and now reached automatically past
    /// `super::MAX_SEND_INPUT_CHARS` rather than only as a manual opt-in: the
    /// previous clipboard contents are lost, with no restore. Restore was
    /// evaluated, not merely skipped, and rejected as unsound rather than
    /// just inconvenient:
    ///
    /// * Restoring as soon as `SendInput` returns races the target.
    ///   `SendInput` returning only means the Ctrl+V keystrokes entered the
    ///   input stream, not that the target has processed them, resolved its
    ///   paste accelerator, and called `GetClipboardData` yet. Restore before
    ///   that read happens and the target pastes the *old* contents —
    ///   silently, since nothing fails — which is worse than the disclosed
    ///   clobber: it looks like it worked.
    /// * There is no OS signal to wait on instead. `GetClipboardSequenceNumber`
    ///   and `AddClipboardFormatListener`/`WM_CLIPBOARDUPDATE` fire on a
    ///   *write* (`SetClipboardData`); a plain read touches neither, by
    ///   design, since the clipboard is meant to have multiple readers.
    ///   Nothing here can be polled or subscribed to for "a reader finished."
    /// * A fixed delay before restoring cannot be made both safe and free.
    ///   Short enough to preserve the sub-second latency this product exists
    ///   for, and it can still fire before a busy target has read the
    ///   clipboard — the same silent-wrong-paste failure above. Long enough
    ///   to be reliably safe, and it either blocks `inject` for that long or
    ///   needs a background timer, which then races the *next* dictation's
    ///   own `set_clipboard` call: back-to-back dictations are the documented
    ///   common case for push-to-talk (see `modifier_to_release`), not a rare
    ///   edge a timer could afford to ignore.
    ///
    /// So this clobbers the clipboard and says so — plainly, in the change
    /// that made escalation automatic, and in `crates/iris-app/README.md`
    /// where a user will see it — rather than trade one silent corruption bug
    /// for a subtler one.
    ///
    /// What *is* mitigated rather than only disclosed is persistence, which
    /// is a separate cost from the clobber: see
    /// `decline_history_and_cloud_sync`.
    ///
    /// Two things can still make the paste unusable after this is chosen, and
    /// neither may cost the user their words: the clipboard can be held open
    /// by another process — briefly retried before it counts, see
    /// `CLIPBOARD_OPEN_ATTEMPTS` — and the hotkey can still be down in a way
    /// that spoils the accelerator. Both fall back to `send_keystrokes`, which
    /// `super::pacing` makes a safe landing for a long transcript rather than
    /// a return to the reported bug.
    ///
    /// That fallback corrects the hotkey again before its own burst, so a
    /// genuinely stuck, correctable hotkey can be sent more than one
    /// corrective key-up — and pay more than one `SETTLE` — within a single
    /// call here. Accepted rather than threaded through: a duplicate key-up
    /// for a key the user is not holding is the harmless orphan release
    /// `super::modifier_to_release` already reasons about, `RightAlt` and
    /// `RightWin` never get one at all, and carrying a `Correction` across
    /// into the fallback would mean reusing a reading taken before the
    /// clipboard work that sits between them.
    ///
    /// The accelerator is therefore checked twice, and both are load-bearing.
    /// The early one is what keeps a `ralt`/`rwin` user — the hotkeys
    /// `super::modifier_to_release` may never correct, so the veto below is
    /// certain, not hypothetical — from spending their clipboard on a paste
    /// that was never going to be sent. The late one cannot be dropped in its
    /// favour: `set_clipboard` can block for as long as another process holds
    /// the clipboard, and a hotkey pressed during that wait would otherwise
    /// reach the burst unnoticed.
    ///
    /// Both readings come from `settled_reads_down`, never `reads_down`
    /// directly. Each follows a `release_hotkey_if_stuck` that may just have
    /// injected a key-up for this very key, and a raw reading taken then can
    /// still report the state that correction cleared — which would veto the
    /// paste on a desync that no longer exists.
    ///
    /// Returning `Ok` means the four events entered the input stream. Whether
    /// the target resolved Ctrl+V as paste, and pasted correctly, is not
    /// observable from here — see the module docs.
    fn paste(text: &str, hotkey: Key) -> Result<()> {
        let corrected = release_hotkey_if_stuck(hotkey)?;
        if !super::paste_accelerator_survives(hotkey, settled_reads_down(hotkey, corrected)) {
            vlog!(
                "the hotkey ({}) is down and would change Ctrl+V into another accelerator; \
                 typing the transcript instead, and leaving the clipboard alone",
                hotkey.label()
            );
            return send_keystrokes(text, hotkey);
        }
        // Clipboard managers, Office and RDP redirection all open the
        // clipboard briefly and routinely, so this fails transiently for
        // reasons that have nothing to do with the transcript. Losing a long
        // utterance to that would be the worst outcome of the whole feature.
        if let Err(e) = set_clipboard(text) {
            if e.cleared {
                vlog!(
                    "the clipboard was emptied but the transcript could not be written to it \
                     ({:#}); typing the transcript instead — the previous clipboard contents \
                     are gone",
                    e.source
                );
            } else {
                vlog!(
                    "clipboard unavailable ({:#}); typing the transcript instead, with the \
                     previous clipboard contents left untouched",
                    e.source
                );
            }
            return send_keystrokes(text, hotkey);
        }
        // Again, immediately before the burst: any gap between checking the
        // hotkey and using that reading is a window for the hotkey to change
        // state, and `set_clipboard` above is exactly such a gap. This
        // mirrors `send_keystrokes`, where the correction already immediately
        // precedes its burst.
        let corrected = release_hotkey_if_stuck(hotkey)?;
        if !super::paste_accelerator_survives(hotkey, settled_reads_down(hotkey, corrected)) {
            vlog!(
                "the hotkey ({}) went down while the clipboard was being written; typing \
                 the transcript instead",
                hotkey.label()
            );
            return send_keystrokes(text, hotkey);
        }

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
        vlog!(
            "sent Ctrl+V for {} chars left on the clipboard",
            text.chars().count()
        );
        Ok(())
    }

    /// A failed clipboard write, and what it cost.
    ///
    /// `cleared` is the part the caller cannot infer: once `EmptyClipboard`
    /// has succeeded the user's previous contents are gone whether or not the
    /// replacement lands, so a failure after that point is not the free
    /// fallback a failure before it is. `paste` says which happened rather
    /// than reporting both as "clipboard unavailable".
    struct ClipboardError {
        cleared: bool,
        source: anyhow::Error,
    }

    /// Copy `bytes` into the kind of block `SetClipboardData` accepts.
    ///
    /// The caller owns the returned handle until `set_clipboard_owned`
    /// succeeds for it.
    unsafe fn global_block(bytes: &[u8]) -> Result<HGLOBAL> {
        unsafe {
            let handle =
                GlobalAlloc(GMEM_MOVEABLE, bytes.len()).context("allocating clipboard memory")?;
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                let _ = GlobalFree(Some(handle));
                bail!("locking clipboard memory failed");
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            let _ = GlobalUnlock(handle);
            Ok(handle)
        }
    }

    /// Hand `handle` to the clipboard under `format`.
    ///
    /// `SetClipboardData` takes ownership of the block only on success, so it
    /// must not be freed there — but every failure path from the allocation
    /// onwards must free it.
    unsafe fn set_clipboard_owned(format: u32, handle: HGLOBAL) -> Result<()> {
        unsafe {
            if let Err(e) = SetClipboardData(format, Some(HANDLE(handle.0))) {
                let _ = GlobalFree(Some(handle));
                return Err(anyhow::Error::new(e));
            }
            Ok(())
        }
    }

    /// Ask Windows to keep this clipboard item out of Clipboard History
    /// (Win+V) and off Cloud Clipboard sync.
    ///
    /// Escalation put every long dictation on the clipboard automatically, so
    /// without this a transcript does not merely clobber the clipboard — it
    /// persists in the Win+V history after the dictation is over, and syncs
    /// to the user's other devices when Cloud Clipboard is on. That is a
    /// different and larger disclosure than the clobber, and Windows has a
    /// supported way to decline it: three registered formats whose names the
    /// system knows, documented under "Cloud Clipboard and Clipboard History
    /// Formats" in the Win32 clipboard-formats topic.
    /// `ExcludeClipboardContentFromMonitorProcessing` takes any data at all;
    /// `CanIncludeInClipboardHistory` and `CanUploadToCloudClipboard` take a
    /// serialised `DWORD` of zero to decline each individually. All three are
    /// set: they are documented as independent, and the first is the one that
    /// also asks clipboard monitors in general to leave the item alone.
    ///
    /// Deliberately best-effort and never fatal. Failing to register a
    /// marker must not cost the user the transcript that the clipboard write
    /// is there to deliver, and on a Windows build with no clipboard history
    /// there is nothing to decline in the first place. It is also not a
    /// guarantee against a third-party clipboard manager: that is a separate
    /// program free to ignore the request, which is why
    /// `crates/iris-app/README.md` discloses it as Windows' own history and
    /// sync being excluded rather than as the transcript being private.
    ///
    /// This does not touch the live clipboard, so the documented recovery
    /// path — the transcript still sitting there to be pasted by hand — is
    /// unaffected.
    unsafe fn decline_history_and_cloud_sync() {
        unsafe {
            for name in [
                w!("ExcludeClipboardContentFromMonitorProcessing"),
                w!("CanIncludeInClipboardHistory"),
                w!("CanUploadToCloudClipboard"),
            ] {
                let format = RegisterClipboardFormatW(name);
                if format == 0 {
                    vlog!(
                        "could not register a clipboard-history opt-out format ({})",
                        windows::core::Error::from_thread()
                    );
                    continue;
                }
                // A serialised DWORD of zero: "no" to both of the formats
                // that read a value, and acceptable as the "any data" the
                // third takes.
                let declined = global_block(&0u32.to_ne_bytes())
                    .and_then(|block| set_clipboard_owned(format, block));
                if let Err(e) = declined {
                    vlog!("could not opt out of clipboard history/cloud sync ({e:#})");
                }
            }
        }
    }

    /// A window to own the clipboard for the length of one `set_clipboard`
    /// call.
    ///
    /// Win32 requires one, and requires it of *every* write here rather than
    /// only of a delayed-rendering one. `SetClipboardData`'s summary opens
    /// with "The window must be the current clipboard owner", and its
    /// Remarks, `OpenClipboard`'s and `EmptyClipboard`'s each carry the same
    /// unqualified sentence: if an application calls `OpenClipboard` with
    /// `hwnd` set to `NULL`, `EmptyClipboard` sets the clipboard owner to
    /// `NULL`, and "this causes `SetClipboardData` to fail". Nothing in that
    /// is scoped to `hMem` = `NULL` — delayed rendering is a separate
    /// paragraph, on the parameter, and neither the transcript nor any marker
    /// in `decline_history_and_cloud_sync` uses it. Failing there is the one
    /// failure this code cannot make cheap: it lands past `EmptyClipboard`,
    /// so it costs the user their previous clipboard contents *and* the
    /// paste.
    ///
    /// Message-only (`HWND_MESSAGE`) and short-lived. Such a window is
    /// invisible, has no z-order, cannot be enumerated and — the part that
    /// matters on an injection thread that pumps nothing — "does not receive
    /// broadcast messages", so no other process can block on a message it
    /// will never answer. `STATIC` is the class so there is neither a class
    /// to register nor a window procedure to write.
    ///
    /// Destroying it as soon as the clipboard is closed does not take the
    /// transcript with it. `WM_RENDERALLFORMATS` — the message that makes a
    /// clipboard owner's destruction matter — is sent "if the clipboard owner
    /// has delayed rendering one or more clipboard formats", and every format
    /// written here handed over its data immediately, which the system owns
    /// from that point on.
    unsafe fn clipboard_owner() -> Result<HWND> {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("iris clipboard owner"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
            .context("creating a window to own the clipboard")
        }
    }

    /// How many times `set_clipboard` asks for the clipboard, and how long it
    /// waits between asks.
    ///
    /// `OpenClipboard` fails while another process has it open, and that is
    /// normally somebody else's brief, routine open — a clipboard manager,
    /// Office, RDP redirection — rather than a lasting condition. Treating the
    /// first refusal as final would drop an already-escalated transcript onto
    /// the keystroke path the escalation exists to get off, for a collision
    /// that was over before the fallback finished its first paced gap.
    ///
    /// Bounded, not a wait-until-free loop: a clipboard held open indefinitely
    /// must still end in that fallback rather than in `inject` never
    /// returning, and the whole retry window costs less than the pacing the
    /// fallback would pay anyway.
    const CLIPBOARD_OPEN_ATTEMPTS: u32 = 3;
    const CLIPBOARD_OPEN_GAP: std::time::Duration = std::time::Duration::from_millis(10);

    fn open_clipboard(owner: HWND) -> Result<()> {
        let mut attempt = 1;
        loop {
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => {
                    if attempt > 1 {
                        vlog!("the clipboard opened on attempt {attempt}");
                    }
                    return Ok(());
                }
                Err(e) if attempt == CLIPBOARD_OPEN_ATTEMPTS => {
                    return Err(e).context("another application is holding the clipboard open");
                }
                Err(_) => {}
            }
            attempt += 1;
            std::thread::sleep(CLIPBOARD_OPEN_GAP);
        }
    }

    fn set_clipboard(text: &str) -> std::result::Result<(), ClipboardError> {
        let mut utf16: Vec<u16> = text.encode_utf16().collect();
        utf16.push(0);
        let bytes: Vec<u8> = utf16.iter().flat_map(|unit| unit.to_ne_bytes()).collect();

        unsafe {
            // Allocated and filled before the clipboard is even opened. Every
            // step that can fail belongs on this side of `EmptyClipboard`:
            // past that point the user's previous contents are already gone,
            // and a failure there has nothing to put back. The owner window
            // is on this side for the same reason.
            let block = global_block(&bytes).map_err(|source| ClipboardError {
                cleared: false,
                source,
            })?;

            let owner = match clipboard_owner() {
                Ok(owner) => owner,
                Err(source) => {
                    let _ = GlobalFree(Some(block));
                    return Err(ClipboardError {
                        cleared: false,
                        source,
                    });
                }
            };

            if let Err(source) = open_clipboard(owner) {
                let _ = GlobalFree(Some(block));
                let _ = DestroyWindow(owner);
                return Err(ClipboardError {
                    cleared: false,
                    source,
                });
            }
            // From here on every path must close the clipboard and destroy
            // the owner window.
            let result = (|| -> std::result::Result<(), ClipboardError> {
                EmptyClipboard()
                    .context("emptying the clipboard")
                    .map_err(|source| {
                        let _ = GlobalFree(Some(block));
                        ClipboardError {
                            cleared: false,
                            source,
                        }
                    })?;
                // Before the text, so the transcript is never on the
                // clipboard for an instant without them.
                decline_history_and_cloud_sync();
                set_clipboard_owned(CF_UNICODETEXT, block).map_err(|source| ClipboardError {
                    cleared: true,
                    source: source.context("setting clipboard text"),
                })
            })();
            let _ = CloseClipboard();
            let _ = DestroyWindow(owner);
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

    /// A window nothing knows anything special about — the regime the
    /// reported failure was found in (Notepad is a plain edit control) and
    /// the one most targets fall into.
    fn ordinary_window() -> Target {
        Target {
            class: "Notepad".to_string(),
            process: "notepad.exe".to_string(),
        }
    }

    /// The method `inject` would settle on for `text`, wired the way `inject`
    /// wires it: the length predicate asked once and handed to
    /// `effective_method`, never recomputed inside it.
    fn method_for(text: &str, requested: Method, target: Option<&Target>) -> Method {
        effective_method(needs_more_than_one_batch(text), requested, target)
    }

    #[test]
    fn short_text_keeps_the_requested_send_input_method() {
        assert_eq!(
            method_for("hello", Method::SendInput, None),
            Method::SendInput
        );
    }

    #[test]
    fn text_filling_exactly_one_batch_stays_on_send_input() {
        // MAX_SEND_INPUT_CHARS is the largest text `win::send_keystrokes`
        // can still deliver in a single flush, which is the regime this
        // project has evidence is sound.
        let text = "a".repeat(MAX_SEND_INPUT_CHARS);
        assert_eq!(
            method_for(&text, Method::SendInput, None),
            Method::SendInput
        );
    }

    #[test]
    fn text_needing_a_second_batch_is_escalated_to_clipboard() {
        let text = "a".repeat(MAX_SEND_INPUT_CHARS + 1);
        assert_eq!(
            method_for(&text, Method::SendInput, None),
            Method::Clipboard
        );
        assert_eq!(
            method_for(&text, Method::SendInput, Some(&ordinary_window())),
            Method::Clipboard
        );
    }

    #[test]
    fn the_target_lookup_and_the_escalation_share_one_length_predicate() {
        // `inject` gates the target lookup on this predicate and hands the
        // same answer to `effective_method` rather than letting it ask again.
        // Were the lookup ever gated more narrowly than the escalation, a
        // window that should have been examined would be pasted into with no
        // target consulted and nothing logged — exactly the failure the
        // deny-list exists to prevent. Pinned across the boundary because the
        // threshold is where a drift would first show.
        for units in [
            0,
            1,
            MAX_SEND_INPUT_CHARS - 1,
            MAX_SEND_INPUT_CHARS,
            MAX_SEND_INPUT_CHARS + 1,
            MAX_SEND_INPUT_CHARS * 2,
        ] {
            let text = "a".repeat(units);
            assert_eq!(
                needs_more_than_one_batch(&text),
                method_for(&text, Method::SendInput, None) == Method::Clipboard,
                "{units} units"
            );
        }
    }

    #[test]
    fn an_explicit_clipboard_request_is_never_second_guessed() {
        // Nothing to protect either way: Clipboard already delivers in four
        // events regardless of length, so a short text must not be changed
        // any more than a long one already isn't.
        assert_eq!(
            method_for("hi", Method::Clipboard, None),
            Method::Clipboard
        );
        let long = "a".repeat(MAX_SEND_INPUT_CHARS * 2);
        assert_eq!(
            method_for(&long, Method::Clipboard, None),
            Method::Clipboard
        );
    }

    #[test]
    fn a_paste_hostile_target_keeps_the_keystroke_path_however_long_the_text() {
        // Ctrl+V is not paste in any of these: vim in insert mode takes it
        // as literal-next-character, and a terminal or RDP client either
        // binds it elsewhere or forwards it. Pasting here would swap one
        // corruption for another, so escalation must decline — and `pacing`
        // is what keeps declining safe.
        let long = "a".repeat(MAX_SEND_INPUT_CHARS * 4);
        for target in [
            Target {
                class: "ConsoleWindowClass".to_string(),
                process: "cmd.exe".to_string(),
            },
            Target {
                class: "CASCADIA_HOSTING_WINDOW_CLASS".to_string(),
                process: "WindowsTerminal.exe".to_string(),
            },
            Target {
                class: "TscShellContainerClass".to_string(),
                process: "mstsc.exe".to_string(),
            },
            Target {
                class: "Vim".to_string(),
                process: "gvim.exe".to_string(),
            },
        ] {
            assert_eq!(
                method_for(&long, Method::SendInput, Some(&target)),
                Method::SendInput,
                "{target:?}"
            );
            assert!(
                pacing(crate::text::plan_len(&long)).is_some(),
                "{target:?} must not be left on an unpaced burst"
            );
        }
    }

    #[test]
    fn the_named_gaps_from_review_are_covered() {
        // Emacs is the sharpest of these: C-v is scroll-up-command in the
        // default bindings, so an escalated paste scrolls and inserts
        // nothing. ConEmu/Cmder share a window class; Hyper and Tabby can
        // only be reached by image name, because the Electron class they
        // register is shared with apps that paste perfectly well.
        for target in [
            Target {
                class: "Emacs".to_string(),
                process: "emacs.exe".to_string(),
            },
            Target {
                class: "VirtualConsoleClass".to_string(),
                process: "ConEmu64.exe".to_string(),
            },
            Target {
                class: "VirtualConsoleClass".to_string(),
                process: "Cmder.exe".to_string(),
            },
            Target {
                class: "Chrome_WidgetWin_1".to_string(),
                process: "Hyper.exe".to_string(),
            },
            Target {
                class: "Chrome_WidgetWin_1".to_string(),
                process: "Tabby.exe".to_string(),
            },
        ] {
            assert!(!accepts_paste(Some(&target)), "{target:?}");
        }
    }

    #[test]
    fn the_shared_electron_class_alone_is_never_treated_as_hostile() {
        // Listing `Chrome_WidgetWin_1` would take every Electron app with
        // it — Slack, VS Code, Discord — none of which have any trouble
        // pasting. Only the terminal image names may match.
        assert!(accepts_paste(Some(&Target {
            class: "Chrome_WidgetWin_1".to_string(),
            process: "slack.exe".to_string(),
        })));
    }

    #[test]
    fn a_hostile_process_is_caught_even_in_an_anonymous_window_class() {
        // Window class names are up to the app; several terminals host
        // themselves in a generic one, so the executable is checked too.
        let long = "a".repeat(MAX_SEND_INPUT_CHARS + 1);
        let target = Target {
            class: "Window".to_string(),
            process: "nvim-qt.exe".to_string(),
        };
        assert_eq!(
            method_for(&long, Method::SendInput, Some(&target)),
            Method::SendInput
        );
    }

    #[test]
    fn target_matching_ignores_case() {
        // `GetClassNameW` and the image name report whatever casing the app
        // and the filesystem happen to use.
        assert!(!accepts_paste(Some(&Target {
            class: "consolewindowclass".to_string(),
            process: "CMD.EXE".to_string(),
        })));
        assert!(!accepts_paste(Some(&Target {
            class: "PUTTY".to_string(),
            process: String::new(),
        })));
    }

    #[test]
    fn an_unidentifiable_window_is_treated_as_an_ordinary_one() {
        // Default-allow, deliberately: the reported failure was a plain edit
        // control, and identification can fail for reasons that say nothing
        // about the target. Treating unknown as hostile would leave the
        // reported bug unfixed exactly where it was reported.
        assert!(accepts_paste(None));
        assert!(accepts_paste(Some(&ordinary_window())));
        assert!(accepts_paste(Some(&Target::default())));
    }

    #[test]
    fn astral_characters_count_as_two_units_toward_the_threshold() {
        // Each is a surrogate pair -- two KeyUnits, four SendInput events --
        // so far fewer *characters* are needed to cross the same *event*
        // threshold than for BMP text. Proves the check walks `text::plan`
        // rather than counting `chars()`.
        let chars_needed = MAX_SEND_INPUT_CHARS / 2 + 1;
        let text = "\u{1F600}".repeat(chars_needed);
        assert_eq!(
            method_for(&text, Method::SendInput, None),
            Method::Clipboard
        );
    }

    #[test]
    fn the_reported_long_dictation_is_escalated_to_clipboard() {
        // The reproduction: ~313 characters, transcribed perfectly,
        // `injected: true` in the session log — but Notepad received "Test
        // i" then "ing" then one character repeated some 70 times then
        // roughly 180 spaces, while the identical bytes land correctly in a
        // terminal every time. That length crosses BATCH (512 events / 256
        // characters), which is what this test pins down: the fix reaches
        // the actual reported transcript, not just a synthetic boundary
        // case. (This is a manual retyping of the quoted transcript from the
        // bug report, so it is asserted against the property that matters —
        // spanning more than one batch — rather than the exact character
        // count, which this reconstruction cannot promise to reproduce
        // byte-for-byte.)
        let spoken = "Test one, test two, test three. Although this is working fine. I'm currently testing it. And I was testing it on the terminal. It was working good. I'm now testing it because I want to see the bar Can I notice something that the waves and the coloring do not reach the end So, like, kind of gets cut off about 75%?";
        assert!(
            spoken.chars().count() > MAX_SEND_INPUT_CHARS,
            "the reported transcript must span more than one batch"
        );
        let text = crate::text::prepare(spoken, true);
        assert_eq!(
            method_for(&text, Method::SendInput, Some(&ordinary_window())),
            Method::Clipboard
        );
    }

    #[test]
    fn the_reported_long_dictation_is_paced_when_it_cannot_be_pasted() {
        // The other half of the same repro: the identical transcript into a
        // terminal, or with the clipboard unavailable, stays on keystrokes.
        // It must not go out as the one sustained burst that corrupted it —
        // that is what would leave a target on the known-bad path.
        let spoken = "Test one, test two, test three. Although this is working fine. I'm currently testing it. And I was testing it on the terminal. It was working good. I'm now testing it because I want to see the bar Can I notice something that the waves and the coloring do not reach the end So, like, kind of gets cut off about 75%?";
        let text = crate::text::prepare(spoken, true);
        let pace = pacing(crate::text::plan_len(&text)).expect("a burst this long must be paced");
        assert!(pace.units < MAX_SEND_INPUT_CHARS);
        assert!(!pace.gap.is_zero());
    }

    #[test]
    fn an_ordinary_dictation_is_never_paced() {
        // Pacing costs wall-clock, and the single-batch regime is the one
        // this project has evidence is already sound. Nothing at or under
        // the threshold may pay for the fallback path.
        assert_eq!(pacing(0), None);
        assert_eq!(pacing(crate::text::plan_len("hello there ")), None);
        assert_eq!(pacing(MAX_SEND_INPUT_CHARS), None);
        assert!(pacing(MAX_SEND_INPUT_CHARS + 1).is_some());
    }

    /// The events of one `SendInput` call, as `win::send_keystrokes` would
    /// fill it: reconstructed from the same portable `pacing` decision the
    /// real loop calls, for the same reason `burst` below is (see its docs).
    fn calls(text: &str) -> Vec<Vec<crate::text::KeyUnit>> {
        let units = crate::text::plan(text);
        let pace = pacing(units.len());
        let per_call = pace.map_or(MAX_SEND_INPUT_CHARS, |p| p.units);
        let last = units.len().saturating_sub(1);

        let mut out = vec![Vec::new()];
        for (i, unit) in units.iter().enumerate() {
            out.last_mut().expect("one call is always open").push(*unit);
            let full = out.last().expect("one call is always open").len() >= per_call;
            if full && unit.may_end_batch() && i < last {
                out.push(Vec::new());
            }
        }
        out
    }

    #[test]
    fn pacing_splits_a_long_burst_without_ever_splitting_a_surrogate_pair() {
        // Windows only reassembles an astral character when both halves
        // arrive in one call, so the extra flushes pacing introduces must
        // obey exactly the rule the single batch boundary already did.
        let text = format!("{}{}", "😀".repeat(200), "a".repeat(200));
        let calls = calls(&text);

        assert!(calls.len() > 1, "this must be long enough to be split");
        let rejoined: Vec<_> = calls.iter().flatten().copied().collect();
        assert_eq!(rejoined, crate::text::plan(&text), "no unit may be lost");
        for call in &calls {
            assert!(
                call.last().expect("no empty call").may_end_batch(),
                "a call may not end on a high surrogate"
            );
            assert!(call.len() <= PACED_UNITS + 1, "{} units", call.len());
        }
    }

    #[test]
    fn a_short_burst_is_still_exactly_one_send_input_call() {
        assert_eq!(calls("hello ").len(), 1);
        assert_eq!(calls(&"a".repeat(MAX_SEND_INPUT_CHARS)).len(), 1);
    }

    #[test]
    fn a_hotkey_that_would_spoil_the_paste_chord_blocks_the_accelerator() {
        // Ctrl+Shift+V, Ctrl+Alt+V and Ctrl+Win+V are not paste. With the
        // hotkey down, pasting is not a slower delivery, it is a failed one.
        for key in [Key::RightShift, Key::RightAlt, Key::RightWin] {
            assert!(!paste_accelerator_survives(key, true), "{key}");
        }
        // Ctrl is the one modifier the chord itself already presses and
        // releases, so a stuck Ctrl leaves paste meaning paste.
        for key in [Key::RightCtrl, Key::LeftCtrl, Key::F8, Key::CapsLock] {
            assert!(paste_accelerator_survives(key, true), "{key}");
        }
    }

    /// What `win::paste` does, in order, for a given pair of hotkey
    /// readings — the early one before the clipboard is written and the late
    /// one immediately before the burst. Reconstructed from the same portable
    /// decision function the real code calls, exactly as `burst` and `calls`
    /// are, and for the same reason: `mod win` is `cfg(windows)` and the only
    /// way to observe it for real is to execute the injection path, which
    /// this project forbids.
    #[derive(Debug, PartialEq, Eq)]
    enum PasteStep {
        Correct,
        WriteClipboard,
        SendAccelerator,
        FallBackToKeystrokes,
    }

    fn paste_steps(hotkey: Key, early_down: bool, late_down: bool) -> Vec<PasteStep> {
        let mut steps = vec![PasteStep::Correct];
        if !paste_accelerator_survives(hotkey, early_down) {
            steps.push(PasteStep::FallBackToKeystrokes);
            return steps;
        }
        steps.push(PasteStep::WriteClipboard);
        steps.push(PasteStep::Correct);
        if !paste_accelerator_survives(hotkey, late_down) {
            steps.push(PasteStep::FallBackToKeystrokes);
            return steps;
        }
        steps.push(PasteStep::SendAccelerator);
        steps
    }

    /// Which of two possible `GetAsyncKeyState` answers `win::paste` actually
    /// acts on, modelling `win::settled_reads_down`: `raw` is what the OS says
    /// the instant `release_hotkey_if_stuck` flushed a correction — which may
    /// still be the state that correction just cleared — and `settled` is what
    /// it says once the settle has been paid. Reconstructed from the real
    /// rule for the same reason `paste_steps` and `calls` are: `mod win` is
    /// `cfg(windows)` and observing it for real would mean executing the
    /// injection path, which this project forbids.
    fn reading_used(corrected: bool, raw: bool, settled: bool) -> bool {
        if corrected {
            settled
        } else {
            raw
        }
    }

    #[test]
    fn a_hotkey_the_correction_just_released_does_not_veto_the_paste() {
        // The race this closes: `release_hotkey_if_stuck` flushes a key-up for
        // a genuinely stuck RightShift and returns immediately.
        // `GetAsyncKeyState` does not update synchronously for an injected
        // event, so a reading taken right then can still say down. Vetoing on
        // it would decline the paste over a desync that no longer exists and
        // drop the long transcript back onto the keystroke path the escalation
        // exists to get off — a slower, worse delivery decided by timing the
        // user cannot see.
        let after_correction = reading_used(true, true, false);
        assert!(!after_correction, "the settled reading is the honest one");
        assert_eq!(
            paste_steps(Key::RightShift, after_correction, after_correction),
            vec![
                PasteStep::Correct,
                PasteStep::WriteClipboard,
                PasteStep::Correct,
                PasteStep::SendAccelerator
            ]
        );
    }

    #[test]
    fn a_hotkey_still_down_after_the_correction_settles_still_vetoes() {
        // The other direction, and the reason this is a settle rather than
        // "assume the correction worked": a correction that did not take
        // leaves Ctrl+V genuinely spoiled, and the settled reading is what
        // says so.
        assert!(reading_used(true, true, true));
        assert_eq!(
            paste_steps(Key::RightShift, true, true),
            vec![PasteStep::Correct, PasteStep::FallBackToKeystrokes]
        );
    }

    #[test]
    fn an_uncorrected_reading_is_never_made_to_wait() {
        // RightAlt and RightWin are exactly the keys `modifier_to_release`
        // refuses to correct, so nothing this code did can have staled their
        // reading: there is no correction in flight to settle for, the veto is
        // certain rather than racy, and the common path — a hotkey that never
        // read down at all — pays no latency for this.
        assert!(reading_used(false, true, false));
        assert!(!reading_used(false, false, true));
        for key in [Key::RightAlt, Key::RightWin] {
            assert_eq!(
                paste_steps(key, reading_used(false, true, false), true),
                vec![PasteStep::Correct, PasteStep::FallBackToKeystrokes],
                "{key}"
            );
        }
    }

    #[test]
    fn a_vetoed_paste_never_touches_the_clipboard() {
        // RightAlt and RightWin are exactly the hotkeys `modifier_to_release`
        // may never correct, so with either of them down the veto is
        // certain — and a certain veto must not cost the user their
        // clipboard on the way to typing the transcript anyway.
        for key in [Key::RightAlt, Key::RightWin, Key::RightShift] {
            let steps = paste_steps(key, true, true);
            assert_eq!(
                steps,
                vec![PasteStep::Correct, PasteStep::FallBackToKeystrokes],
                "{key}"
            );
            assert!(!steps.contains(&PasteStep::WriteClipboard), "{key}");
        }
    }

    #[test]
    fn a_hotkey_pressed_while_the_clipboard_is_written_still_vetoes_the_paste() {
        // The reason the late check cannot be dropped in favour of the early
        // one: `set_clipboard` blocks for as long as another process holds
        // the clipboard, and a hotkey pressed during that wait would reach
        // the burst uncaught. The clipboard is spent here, which is the
        // disclosed cost — but the transcript still arrives.
        assert_eq!(
            paste_steps(Key::RightShift, false, true),
            vec![
                PasteStep::Correct,
                PasteStep::WriteClipboard,
                PasteStep::Correct,
                PasteStep::FallBackToKeystrokes
            ]
        );
    }

    #[test]
    fn an_unobstructed_paste_writes_the_clipboard_then_sends_the_accelerator() {
        for (key, down) in [
            (Key::RightCtrl, true),
            (Key::LeftCtrl, true),
            (Key::RightShift, false),
            (Key::RightAlt, false),
            (Key::F8, true),
        ] {
            assert_eq!(
                paste_steps(key, down, down),
                vec![
                    PasteStep::Correct,
                    PasteStep::WriteClipboard,
                    PasteStep::Correct,
                    PasteStep::SendAccelerator
                ],
                "{key}"
            );
        }
    }

    #[test]
    fn a_hotkey_reading_up_never_blocks_the_accelerator() {
        for key in [
            Key::RightShift,
            Key::RightAlt,
            Key::RightWin,
            Key::RightCtrl,
        ] {
            assert!(paste_accelerator_survives(key, false), "{key}");
        }
    }

    #[test]
    fn protecting_the_accelerator_does_not_widen_pr_9s_exclusions() {
        // The regression guard for the delicate part: the paste chord is new
        // input surface and needs protecting, but the correction that
        // protects a keystroke burst must not grow to cover it. For every
        // key PR #9 deliberately refuses to correct — RightAlt and RightWin
        // on a *genuine desync*, the case the correction is otherwise for —
        // the answer stays None, and the accelerator is declined instead.
        for key in [Key::RightAlt, Key::RightWin] {
            assert_eq!(modifier_to_release(key, true, false), None, "{key}");
            assert!(!paste_accelerator_survives(key, true), "{key}");
        }
        // And a genuine repress is still left alone for every hotkey,
        // whichever path it is on: correcting it would inject an orphan
        // key-up for a press the user is still making.
        for key in [Key::RightCtrl, Key::LeftCtrl, Key::RightShift] {
            assert_eq!(modifier_to_release(key, true, true), None, "{key}");
        }
        // RightShift is the one that is correctable *and* spoils the chord.
        // It is still corrected exactly when PR #9 said it should be — the
        // accelerator check is sampled afterwards and changes nothing about
        // that decision.
        assert_eq!(
            modifier_to_release(Key::RightShift, true, false),
            Some(Key::RightShift)
        );
        assert_eq!(modifier_to_release(Key::RightShift, false, false), None);
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

    /// Renders every hotkey × signal combination the correction can face as
    /// one reviewable table, so the three guarantees that matter can be read
    /// off directly rather than inferred from a list of green test names:
    /// the reported desync is corrected, a genuine repress is not, and
    /// RightAlt/RightWin are never corrected even on a real desync. Run with
    /// `cargo test -p iris-core -- --nocapture correction_decision_matrix`.
    #[test]
    fn correction_decision_matrix_transcript() {
        const ALL: [Key; 11] = [
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
        ];

        println!("\n=== Iris hotkey-correction decision matrix ===");
        println!("Every configured hotkey against the two independent signals the");
        println!("correction cross-checks, and the SendInput events it emits before");
        println!("the text burst as a result.\n");
        println!(
            "{:<12} {:<16} {:<12}  {:<38} why",
            "hotkey", "GetAsyncKeyState", "hook is_held", "correction emitted"
        );
        println!("{}", "-".repeat(110));

        let mut fired = Vec::new();
        for key in ALL {
            for (down, held) in [(true, false), (true, true), (false, false)] {
                let (corrections, keystrokes) = burst("ok ", key, down, held);

                let emitted = match corrections.as_slice() {
                    [] => "(none)".to_string(),
                    [e] => format!(
                        "key-up 0x{:02X}{}",
                        e.vk,
                        if e.extended {
                            " | KEYEVENTF_EXTENDEDKEY"
                        } else {
                            ""
                        }
                    ),
                    more => format!("{} events", more.len()),
                };
                let is_modifier = matches!(
                    key,
                    Key::RightCtrl
                        | Key::LeftCtrl
                        | Key::RightShift
                        | Key::RightAlt
                        | Key::RightWin
                );
                let why = match (down, held, key) {
                    (false, _, _) => "hotkey not down — nothing to correct",
                    (true, _, _) if !is_modifier => "not a modifier — cannot desync SendInput",
                    (true, true, _) => "genuine repress for the next utterance — left alone",
                    (true, false, Key::RightAlt) => {
                        "genuine desync, but an orphan Alt release opens the menu bar"
                    }
                    (true, false, Key::RightWin) => {
                        "genuine desync, but an orphan Win release opens the Start menu"
                    }
                    (true, false, _) => "desync: the reported bug — corrected",
                };
                println!(
                    "{:<12} {:<16} {:<12}  {:<38} {why}",
                    key.label(),
                    if down { "down" } else { "up" },
                    if held { "yes" } else { "no" },
                    emitted,
                );

                // Whatever the decision, the transcript itself is untouched
                // and the correction can never type.
                assert_eq!(typed(&keystrokes), "ok ", "{key}");
                assert!(corrections.iter().all(|e| e.key_up && !e.unicode), "{key}");
                if !corrections.is_empty() {
                    fired.push((key, down, held));
                }
            }
        }

        println!("\nno hook installed (`--speak-wav --really-inject`): correction skipped");
        println!("entirely for every row above — with no hook there is no second signal,");
        println!("and one signal alone cannot tell a stuck key from a held one.");

        assert_eq!(
            fired,
            vec![
                (Key::RightCtrl, true, false),
                (Key::LeftCtrl, true, false),
                (Key::RightShift, true, false),
            ],
            "exactly three rows may emit a correction: a genuine desync on a \
             hotkey with no bare-tap meaning in the Windows shell"
        );
        println!("\ncorrections emitted: {} of 33 rows\n", fired.len());
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

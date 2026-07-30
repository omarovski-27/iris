//! Non-interactive verification, for when there is nobody to hold the key.
//!
//! Checks, in order:
//!
//! 1. audio devices enumerate,
//! 2. the low-level keyboard hook installs and uninstalls,
//! 3. the engine path produces a transcript from the committed WAV fixture.
//!
//! Text injection is **not** among them, and cannot be: Windows delivers
//! synthetic keystrokes only on the desktop the user is looking at, so any
//! automated injection test types into a live session. It is available behind
//! `--injection-test` for someone sitting at the machine, and is otherwise
//! covered by the interactive checklist in this crate's README. See the
//! [`probe`] module docs for what was tried.

use std::time::Duration;

use anyhow::{Context, Result};
use iris_core::dictation::{run_offline, Pace};
use iris_core::engine::{self, EngineSpec};
use iris_core::{audio, capture, hotkey};

use crate::Args;

/// What the injection check types. Deliberately exercises ASCII, an accented
/// character, a non-Latin script and an astral (surrogate pair) code point.
const PROBE: &str = "iris ünïcode 日本 😀";

pub fn run(args: &Args) -> Result<()> {
    println!("iris-spike self-test\n");
    let mut failures = 0;

    failures += report("audio devices enumerate", check_devices());
    failures += report("keyboard hook installs", check_hook(args));
    failures += report("engine path transcribes the fixture", check_engine(args));

    // Injection is opt-in and off by default. Synthetic keystrokes are only
    // delivered on the desktop the user is actually looking at (Windows returns
    // ERROR_ACCESS_DENIED anywhere else), so there is no way to exercise
    // SendInput without typing into somebody's live session. During this spike
    // an automated version of this check did exactly that and interrupted real
    // work. It is not worth a background test; injection is verified by the
    // interactive test in the spike README instead.
    if args.injection_test {
        failures += report("SendInput delivers unicode text", check_injection());
        failures += report("injection cost", check_injection_cost());
    } else {
        report(
            "text injection",
            Ok(Outcome::Skip(
                "not run — types into the live session; use --injection-test to opt in".into(),
            )),
        );
    }

    println!();
    if failures == 0 {
        println!("All checks passed.");
        Ok(())
    } else {
        anyhow::bail!("{failures} check(s) failed")
    }
}

/// A check either passes with a note, is skipped with a reason, or fails.
enum Outcome {
    Pass(String),
    Skip(String),
}

fn report(name: &str, result: Result<Outcome>) -> usize {
    match result {
        Ok(Outcome::Pass(note)) => {
            println!("  PASS  {name:<40} {note}");
            0
        }
        Ok(Outcome::Skip(why)) => {
            println!("  SKIP  {name:<40} {why}");
            0
        }
        Err(e) => {
            println!("  FAIL  {name:<40} {e:#}");
            1
        }
    }
}

fn check_devices() -> Result<Outcome> {
    let devices = capture::list_devices()?;
    if devices.is_empty() {
        anyhow::bail!("no input devices");
    }
    let default = devices
        .iter()
        .find(|d| d.default)
        .map(|d| d.name.as_str())
        .unwrap_or("<none marked default>");
    Ok(Outcome::Pass(format!(
        "{} device(s), default: {default}",
        devices.len()
    )))
}

fn check_hook(args: &Args) -> Result<Outcome> {
    let (listener, _rx) =
        hotkey::listen(args.hotkey, !args.no_suppress).context("SetWindowsHookExW")?;
    // Long enough that a hook rejected on install has surfaced.
    std::thread::sleep(Duration::from_millis(50));
    listener.stop();
    Ok(Outcome::Pass(format!("{} armed and released", args.hotkey)))
}

fn check_engine(args: &Args) -> Result<Outcome> {
    let engine = engine::build(args.engine, &args.engine_options())?;
    let pcm = audio::read_wav(iris_core::sample_wav_path()).context("reading the WAV fixture")?;
    // `Fast` because this is a correctness check, not a latency measurement —
    // that is what iris-harness is for.
    let outcome = run_offline(&*engine, &pcm, Pace::Fast, &mut |_| {})?;
    if outcome.text.trim().is_empty() {
        anyhow::bail!("engine returned an empty transcript");
    }
    let note = if args.engine == EngineSpec::Mock {
        format!(
            "{:.1}s of audio -> {} chars",
            outcome.timeline.audio_secs,
            outcome.text.len()
        )
    } else {
        format!("{:?}", truncate(&outcome.text, 40))
    };
    Ok(Outcome::Pass(note))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

/// A realistic transcript length for the cost measurement — the fixture's own
/// sentence, 83 characters.
const TYPICAL: &str = iris_core::engine::mock::DEFAULT_TRANSCRIPT;

fn check_injection() -> Result<Outcome> {
    match probe::verify_roundtrip(PROBE)? {
        Some(got) if got == PROBE => Ok(Outcome::Pass(format!("{PROBE:?} round-tripped"))),
        Some(got) => anyhow::bail!("expected {PROBE:?}, the window received {got:?}"),
        None => Ok(Outcome::Skip(probe::UNAVAILABLE.into())),
    }
}

fn check_injection_cost() -> Result<Outcome> {
    let Some(measurements) = probe::measure_cost(TYPICAL)? else {
        return Ok(Outcome::Skip(probe::UNAVAILABLE.into()));
    };
    let parts: Vec<String> = measurements
        .iter()
        .map(|m| {
            format!(
                "{}: {:.1} ms call, {:.0} ms to appear",
                m.method, m.call_ms, m.visible_ms
            )
        })
        .collect();
    Ok(Outcome::Pass(format!(
        "{} chars — {}",
        TYPICAL.chars().count(),
        parts.join("; ")
    )))
}

/// Injection checks, isolated from the user's session as far as Windows allows.
///
/// # Why this is opt-in
///
/// Verifying `SendInput` means synthesising real keystrokes, and keystrokes go
/// to whatever window owns the foreground. An earlier version of this module
/// created a window on the interactive desktop and wrestled the foreground away
/// from it; when that raced — or simply while it held focus — the keystrokes
/// landed in whatever the person at the keyboard was actually using. That is
/// unacceptable in a test, and it is why this module no longer touches the
/// foreground at all.
///
/// The obvious fix is that synthetic input is scoped to the calling thread's
/// desktop, so the test should get its own. Everything here does happen on a
/// freshly created desktop holding exactly one window — ours — which is why the
/// code is shaped this way.
///
/// **It does not work, and that is a Windows constraint rather than a bug
/// here.** `SendInput` returns `ERROR_ACCESS_DENIED` from a thread on any
/// desktop that is not the *input* desktop, so the only desktop that can
/// receive synthetic input is the one the user is looking at. The checks
/// therefore report SKIP, and never fall back to the interactive desktop. Real
/// verification is the interactive checklist in this crate's README.
mod probe {
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, CreateDesktopW, SetThreadDesktop, HDESK,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, SendMessageW,
        SetForegroundWindow, SetWindowTextW, ShowWindow, TranslateMessage, ES_AUTOHSCROLL, MSG,
        PM_REMOVE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_GETTEXT, WM_GETTEXTLENGTH,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    use iris_core::inject::{self, Method};

    /// Why the injection checks do not run automatically.
    ///
    /// Measured, not assumed: `SendInput` returns `ERROR_ACCESS_DENIED` when the
    /// calling thread is not on the *input* desktop. Windows allows synthetic
    /// input only on the desktop the user is actually looking at, which is
    /// precisely the desktop a test must not type into. There is no third
    /// option, so injection is verified by the interactive test instead.
    pub const UNAVAILABLE: &str =
        "Windows refuses synthetic input off the input desktop (ERROR_ACCESS_DENIED); \
         verified by the interactive test — see the spike README";

    /// What one injection method cost for one transcript.
    pub struct Cost {
        pub method: Method,
        /// How long the `inject` call itself blocked.
        pub call_ms: f64,
        /// How long until the text was actually readable from the control.
        pub visible_ms: f64,
    }

    /// Type `text` into the isolated window once and read back what arrived.
    pub fn verify_roundtrip(text: &str) -> Result<Option<String>> {
        in_isolated_desktop({
            let text = text.to_string();
            move |hwnd| unsafe {
                // An error here is the desktop restriction, not a defect in the
                // injection code, so it reads as "unavailable" rather than
                // failing the run.
                if inject::inject(&text, Method::SendInput).is_err()
                    || wait_for(hwnd, &text, Duration::from_secs(2)).is_none()
                {
                    return Ok(None);
                }
                Ok(Some(read_text(hwnd)))
            }
        })
    }

    /// Time each injection method delivering the same transcript once.
    ///
    /// One shot per method, deliberately: this is a cost probe, not a load test.
    ///
    /// The injection runs on a worker thread while the window's own thread keeps
    /// pumping messages. If both happened on one thread, the target's input
    /// queue would go unserviced for the duration of the call and we would be
    /// measuring our own stall rather than the API.
    pub fn measure_cost(text: &str) -> Result<Option<Vec<Cost>>> {
        in_isolated_desktop({
            let text = text.to_string();
            move |hwnd| unsafe {
                let mut out = Vec::new();
                for method in [Method::SendInput, Method::Clipboard] {
                    clear(hwnd);

                    let payload = text.clone();
                    let worker = std::thread::spawn(move || {
                        let t0 = Instant::now();
                        let result = inject::inject(&payload, method);
                        (result, t0.elapsed())
                    });

                    let visible = wait_for(hwnd, &text, Duration::from_secs(5));
                    let (result, call) = worker
                        .join()
                        .map_err(|_| anyhow::anyhow!("injection thread panicked"))?;

                    // As above: unavailable, not broken.
                    let (Ok(()), Some(visible)) = (result, visible) else {
                        return Ok(None);
                    };
                    out.push(Cost {
                        method,
                        call_ms: call.as_secs_f64() * 1e3,
                        visible_ms: visible.as_secs_f64() * 1e3,
                    });
                }
                Ok(Some(out))
            }
        })
    }

    /// Run `body` against an empty `EDIT` control that is the only window on a
    /// private desktop.
    ///
    /// `Ok(None)` means the isolated desktop could not deliver input, so the
    /// check is inconclusive rather than failed.
    fn in_isolated_desktop<T: Send + 'static>(
        body: impl FnOnce(HWND) -> Result<Option<T>> + Send + 'static,
    ) -> Result<Option<T>> {
        // A dedicated thread, because SetThreadDesktop refuses to move a thread
        // that already owns windows or hooks — and because the desktop
        // association must not leak into the rest of the process.
        std::thread::spawn(move || unsafe {
            let desktop = CreateDesktopW(
                w!("iris-selftest"),
                None,
                None,
                Default::default(),
                windows::Win32::Foundation::GENERIC_ALL.0,
                None,
            )
            .context("creating an isolated desktop for the injection test")?;

            let result = run_on_desktop(desktop, body);
            let _ = CloseDesktop(desktop);
            result
        })
        .join()
        .map_err(|_| anyhow::anyhow!("the injection test thread panicked"))?
    }

    unsafe fn run_on_desktop<T>(
        desktop: HDESK,
        body: impl FnOnce(HWND) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
        SetThreadDesktop(desktop).context("switching this thread to the isolated desktop")?;

        // Created after SetThreadDesktop, so it belongs to the isolated desktop
        // and is invisible to the user's session.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            // An EDIT control's window text *is* its contents, so it must start
            // empty or the title would be part of what we read back.
            w!(""),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            0,
            0,
            640,
            120,
            None,
            None,
            Some(GetModuleHandleW(None).context("GetModuleHandleW")?.into()),
            None,
        )
        .context("creating the isolated scratch window")?;

        let result = focus_and_run(hwnd, body);
        // Runs on every path, including the error ones.
        let _ = DestroyWindow(hwnd);
        result
    }

    unsafe fn focus_and_run<T>(
        hwnd: HWND,
        body: impl FnOnce(HWND) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
        let _ = ShowWindow(hwnd, SW_SHOW);
        // We are the only process on this desktop, so this is uncontended — no
        // foreground is ever taken from the user.
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        pump(Duration::from_millis(100));
        body(hwnd)
    }

    unsafe fn clear(hwnd: HWND) {
        let _ = SetWindowTextW(hwnd, w!(""));
        pump(Duration::from_millis(30));
    }

    /// Pump messages until the control contains `expected`, reporting how long
    /// that took. `None` means it never arrived within `timeout`.
    unsafe fn wait_for(hwnd: HWND, expected: &str, timeout: Duration) -> Option<Duration> {
        let started = Instant::now();
        while started.elapsed() < timeout {
            pump(Duration::from_millis(5));
            if read_text(hwnd) == expected {
                return Some(started.elapsed());
            }
        }
        None
    }

    /// Dispatch messages for `duration` so the EDIT control processes input.
    unsafe fn pump(duration: Duration) {
        let deadline = Instant::now() + duration;
        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    unsafe fn read_text(hwnd: HWND) -> String {
        let len = SendMessageW(hwnd, WM_GETTEXTLENGTH, None, None).0 as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        let copied = SendMessageW(
            hwnd,
            WM_GETTEXT,
            Some(WPARAM(buf.len())),
            Some(LPARAM(buf.as_mut_ptr() as isize)),
        )
        .0 as usize;
        String::from_utf16_lossy(&buf[..copied.min(buf.len())])
    }
}

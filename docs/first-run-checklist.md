# First-run checklist: verify by eye on real Windows

This build was produced and reviewed entirely from WSL2, which has no Windows
interop in this environment — nothing below has ever actually run. Everything
here compiles, cross-compiles, and is unit/integration-tested on the portable
half of the codebase (`cargo test --workspace`), but none of it substitutes
for someone looking at a real Windows desktop. Work through this list once,
on the machine you're actually going to use Iris on.

## Launch

- [ ] `iris.exe` never shows a console window on any launch path — double-click,
      Start Menu, Startup folder — not even a flash. (A silent crash on launch
      looks exactly like the old flash-and-vanish did, so absence of a window
      is not by itself proof of success; check the next item too.)
- [ ] From an already-open PowerShell or Command Prompt, `iris.exe
      --print-config` prints to that same window — confirming
      `attach_console_for_cli_output` (`main.rs`) reconnects stdout when a
      parent console exists, instead of the diagnostic output silently
      vanishing along with the console the GUI subsystem no longer allocates.
      A GUI-subsystem process does not block the shell the way a console
      binary does, so the prompt reappears immediately rather than waiting
      for `iris.exe` to exit — expect the output to land after the prompt has
      already come back (piping, e.g. `iris.exe --print-config | more`,
      behaves normally), and do not read that ordering as a failure. Try
      `--verbose` and `--list-devices` too.
- [ ] The startup banner (three lines — hotkey, listening device, settings
      path, nothing else: no per-dictation output, no millisecond figures)
      shows up in that same open-terminal run of plain `iris.exe`, and is
      never seen at all from a double-click or a shortcut, which is expected
      — there is no console there for it to print to. Compile- and
      review-verified only up to this point; `banner()` is `#[cfg(windows)]`
      and has never executed anywhere this build was produced.
- [ ] The `.exe` shows the prism-triangle icon (Explorer, taskbar, Alt-Tab) —
      not the default Rust/generic binary icon.
- [ ] Right-click `iris.exe` → Properties → Details shows the version and
      description ("Iris - push-to-talk dictation") instead of blank fields.

## Install script

- [ ] `install.ps1` actually runs from a double-click / "Run with
      PowerShell". Windows' default execution policy blocks unsigned scripts
      on many machines — if it refuses, confirm the `-ExecutionPolicy Bypass`
      fallback in the zip's `README.md` (`packaging/windows/README.md`) works
      and reads clearly to someone who has never used PowerShell.
- [ ] First launch of `iris.exe` from a downloaded (not locally built) zip
      shows SmartScreen's "Windows protected your PC" box. Confirm **More
      info → Run anyway** actually gets Iris running, and that the zip
      README's "Windows protected your PC" section reads clearly to someone
      who has never seen that dialog before. `iris.exe` is unsigned by design (no
      certificate) — this is expected, not a build defect.
- [ ] The installer's closing output is still readable after it finishes —
      it pauses on "Press Enter to close" rather than vanishing with the
      window.
- [ ] Run it redirected instead — `powershell -File .\install.ps1 >
      install.log` or with `-NonInteractive` — and confirm it does *not*
      hang on that same pause; both should exit on their own and leave the
      transcript in `install.log`. `Test-Interactive`'s job is telling these
      two cases apart, and getting it wrong either way is bad: hanging
      forever, or losing the failure message on a real double-click.
- [ ] Running it a second time while Iris is running is a clean replace, not
      an error: `install.ps1` prints "Stopping the running Iris so it can be
      replaced...", the tray icon disappears, and the install finishes and
      relaunches nothing on its own — confirm Iris is actually gone (no tray
      icon, no `iris.exe` in Task Manager) rather than just quiet.
- [ ] Re-running the installer with different flags is a genuine clean
      replace, not additive: install once with `-RunAtLogin`, confirm the
      Startup shortcut exists, then re-run *without* `-RunAtLogin` and confirm
      it is gone — not left behind autostarting Iris. Same check for
      `-Desktop`.
- [ ] The Start Menu shortcut appears and launches Iris with **no console
      window at all** — nothing to minimize, nothing to notice. (This is the
      GUI-subsystem check from the Launch section above, exercised through the
      installed shortcut specifically rather than a manual double-click.)
- [ ] `-Desktop` adds a working desktop shortcut.
- [ ] `-RunAtLogin` actually starts Iris after a real login (not just a
      shortcut sitting in the Startup folder) — log out and back in, or
      restart, to confirm.
- [ ] `install.ps1 -Uninstall` actually removes Iris in one step: quits it if
      running, deletes `%LOCALAPPDATA%\Iris`, and deletes every shortcut a
      previous install created (Start Menu, and Desktop/Startup if you made
      them) — confirm nothing of Iris is left running, launchable or listed
      (Settings → Apps → Installed apps stays empty; `install.ps1` writes no
      registry entry, uninstall or not).
- [ ] `install.ps1 -Uninstall` leaves `%APPDATA%\iris\config.toml` and
      `history.jsonl` untouched — a real key you pasted in and real dictation
      history must both survive an uninstall.
- [ ] Put a `config.toml` or a `*.jsonl` file directly inside
      `%LOCALAPPDATA%\Iris` by hand (simulating the one way user data could
      end up somewhere a clean replace would otherwise delete), then run
      `install.ps1` again: it must refuse and name the file rather than
      silently deleting it.

## First-run config

- [ ] `%APPDATA%\iris\config.toml` is created on first launch, with the
      header comment readable and the `[keys]` example easy to follow for
      someone who has never opened a TOML file.
- [ ] With no key configured, the app runs on the mock engine without
      crashing (dictation "works" but transcribes to a stub).
- [ ] Launched from the Start Menu shortcut — no console exists on this launch
      path at all, so the startup banner is never seen, which is the whole
      point — the tray menu opens with four
      disabled lines above everything else (`tray::demo_notice`): the "Demo
      mode: transcripts are stubs" headline, the two numbered edits (change
      the existing `engine = "mock"` line; add a `[keys]` block at the very
      end), and the line naming `%APPDATA%\iris\config.toml` and the restart.
      Check they fit and read rather than being clipped — the path line is the
      long one — and that hovering the icon says the short version. This is
      the only in-app explanation a first-run user gets on that launch path,
      so follow it *literally* on the real machine and confirm it lands you on
      the real engine; the wording is executed by
      `iris-app/tests/settings.rs`, but only against `Config::load`, never
      against a live install.
- [ ] Those lines are gone once a real engine is configured and Iris has been
      restarted.
- [ ] Known display defect, deliberately deferred — confirm it is still only
      this: switching Engine → Deepgram *from the tray submenu* leaves the
      "Demo mode" lines on screen for the rest of the session, and switching
      back to mock shows no warning. The switch itself genuinely works — it
      builds the engine, persists to `config.toml`, and injects real
      transcripts from that moment on — so this understates what the app is
      doing rather than hiding a failure. The menu is built once in
      `win::run` and there is no app→tray channel to refresh it; adding one is
      an app-behaviour change this packaging branch deliberately does not
      make, the same call the project already made for Reload not applying a
      new hotkey or key. A restart clears it. If instead the switch stops
      working, or dictation still returns stubs afterwards, that is a
      different bug and worth reporting.
- [ ] Deliberately misconfigure `engine = "deepgram"` with no key: the error
      is a clear sentence naming the config path and telling you to start Iris
      again after editing it — not a Rust panic or stack trace.
- [ ] Do the same again, but launched from the Start Menu shortcut: the
      message arrives as an "Iris could not start" dialog box — there is no
      console on this launch path at all for it to print to instead.
- [ ] The same misconfiguration launched from an open PowerShell prompt
      prints to that console (confirming `attach_console_for_cli_output`
      reconnected it) and shows *no* dialog — `GetConsoleProcessList` seeing
      more than one process (the shell and `iris.exe`) is what tells
      `report_failure` (`main.rs`) the two cases apart.
- [ ] Upgrading over a `config.toml` an older build wrote: the first start
      rewrites it, adding `version = 1` and turning `show_live_text` off once.
      Everything else in the file survives, and a `show_live_text = true` set
      by hand afterwards sticks across restarts. `iris --verbose` from an open
      PowerShell prompt is where that one-time rewrite is reported. If the
      rewrite fails, the cause is `%APPDATA%\iris` not being writable —
      permissions, a read-only location, or a full disk — and the symptom is
      wider than a missed stamp: *every* settings change goes through the same
      write, so a tray → Engine switch or anything else Iris persists silently
      fails too, and the `show_live_text` reset runs again on every launch.
      If settings will not stick, check that `%APPDATA%\iris` is writable.
      Going back to an older build needs the `version` line deleted first —
      the zip README's "Downgrading to an older Iris"
      (`packaging/windows/README.md`) is the copy of that path to keep
      accurate.
- [ ] Tray → Settings opens the settings window (see below), and its Settings
      tab's **Open config file** button is what opens `config.toml` in the
      default editor.
- [ ] After adding a real key and saving, a full restart is what puts it in
      force — not tray → Reload settings. Launched from an open PowerShell
      prompt so the console output is visible, Reload prints "keys changed:
      restart Iris for that to take effect" and stays on the engine it
      started with. Quit from the tray, launch again, and the key is live.

## Hotkey

- [ ] Holding the default (Right-Ctrl) push-to-talk key works from a cold
      start.
- [ ] The tray menu's top line reads "Hold right-ctrl to dictate" (lower-case
      — it is `Key::label()` verbatim, not a prettified name). It is a
      disabled label, not a menu — rebinding lives in the settings window, not
      the tray, and its absence here is not a broken build.
- [ ] Rebinding works the way it actually ships: pick the key in the settings
      window's Settings tab (or set `hotkey` in `config.toml` by hand), then
      quit from the tray and launch Iris again. The low-level hook is
      installed once at startup, so tray → Reload settings only reports
      "hotkey changed: restart Iris for that to take effect" and keeps the old
      binding (`suppress_hotkey` is the same story). `iris.exe --hotkey f9`
      does the rebind for one run without touching the file.
- [ ] Each accepted key binds and fires: `rctrl`, `lctrl`,
      `rshift`, `ralt`, `rwin`, `capslock`, `scrolllock`, `pause`, `f8`,
      `f9`, `f10`. That is the whole set (`iris_core::hotkey::Key`); anything
      else is rejected at load. None have been exercised on real hardware.
- [ ] After a rebind and the restart it needs, the old hotkey stops working
      and the new one starts, with no double trigger or stuck-key behavior.
- [ ] If Windows ever uninstalls the low-level hook mid-session (it does that
      to a callback that runs long), Iris says so in an "Iris has stopped"
      dialog rather than disappearing — the same treatment startup failures
      get, since a shortcut-launched app has no console to print to. There is
      no way to provoke this on purpose; note it if it ever happens.

## Overlay

- [ ] The pill appears and animates at real speed — the motion timings are
      unverified outside the filmstrip renderer (`pill-demo --filmstrip`),
      which is not the same as watching it live.
- [ ] The resting shape is a narrow glass capsule holding a wave row and an
      elapsed-recording timer side by side — not a circle, and not the old
      wide rectangle — and the bars visibly grow with your voice. Live text is
      off by default, so this is the *only* presentation you get until you set
      `show_live_text = true` in `config.toml`.
- [ ] The timer's digits stay readable over both a light and a dark desktop,
      with no dark plate behind them. Only their outline colour carries that
      (`theme.timer_edge`), and it has been judged only against the synthetic
      backdrop in `crates/iris-overlay/docs/round3-evidence/`, never a real
      one. The digits must not jitter as seconds tick over, and must stay
      clear of the centred dot / spinner / checkmark in every state.
- [ ] With `show_live_text = true` and a restart-free tray → Reload settings,
      the ribbon opens with the words again and the timer steps aside rather
      than the two overlapping on the right-hand edge.
- [ ] Dark and Light themes both render legibly against real desktop
      backgrounds.
- [ ] The confirmation hold and self-dismiss after a successful insert look
      right, not truncated or stuck.

## Settings window

Everything here runs through `window::shell` (`eframe`/`winit`/`glow`), the
one `#[cfg(windows)]` half of the window and the half no test touches;
`iris.exe --demo-window` is the same window against seeded demo data if you
want it without dictating first.

- [ ] Tray → **Open settings…** opens it, and doing it again focuses (and
      un-minimizes) that same window rather than opening a second one.
      Closing the window never stops dictation — the hotkey still works.
- [ ] It opens at a usable size and stays legible on a high-DPI / scaled
      display, in both Dark and Light: no clipped controls, no boxed "tofu"
      characters where a glyph should be.
- [ ] History lists real dictations newest first, the search box filters
      them, **Copy** puts the exact text on the clipboard, and a dictation
      whose injection failed shows its reason in place. With
      `history.enabled = false` it says logging is off and names that
      setting, rather than telling you to speak.
- [ ] Changing engine, microphone, theme and polish from the Settings tab
      says "Saved", is visible in `config.toml`, and is still in force after
      a restart. A change the loop refuses — Deepgram with no key, a
      microphone that will not open — says why instead of "Saved", and the
      control does not move.
- [ ] Editing an unrelated setting in `config.toml` by hand while Iris is
      running and then changing something in the window keeps the hand edit.
- [ ] Rebinding the hotkey or toggling the overlay is marked "until restart",
      and picking the key that is already running is not.
- [ ] Insights numbers are plausible against the same log (`iris --history`),
      and the latency figures cover only the dictations that reached the
      screen.

## Dictation quality

- [ ] Latency "feels" fast — the budget in `docs/spike-findings.md` was
      measured on different hardware and network conditions.
- [ ] Accuracy is reasonable for normal speech.
- [ ] Injection lands correctly in a few different real target apps
      (browser address bar, a chat app, a plain text editor, something that
      only accepts paste). `inject::effective_method`'s clipboard fallback
      for long transcripts is unit-tested but never watched land on a real
      focused window.
- [ ] Force an injection failure (e.g. hold the hotkey over an elevated
      window, which `SendInput` cannot reach) with `[history] enabled =
      false` in `config.toml`, launched from the Start Menu shortcut — no
      console on this launch path, and history off so the session log
      cannot recover the words either. Confirm both halves of
      `notify::SystemFailureNotice` fire: an "Iris could not type your
      dictation" dialog naming the error, and the transcript sitting on the
      clipboard, ready to paste by hand. Only mechanically verified here via
      the portable test double (`RecordingFailureNotice` in
      `tests/loop.rs`) — the real dialog and the real clipboard write have
      never executed anywhere this build was produced.
- [ ] The same failure with history left on: the dialog still appears (the
      failure itself must stay visible either way), but the message points
      at the session log instead of the clipboard, and the clipboard is left
      untouched — `SystemFailureNotice` must not write to it when the words
      are already durably recoverable from the log.

## What this build does not include

The round-3 overlay capsule (PR #15) and the settings window (PR #13) are both
in the tree now, and the Overlay, First-run config and Settings window items
above are written for them — repackage with `scripts/package-windows.sh`
before working through this list, or the zip you verify predates them. No
branch is still in flight; if one lands after this, this checklist and the
packaged zip are stale — repackage and re-run this list.

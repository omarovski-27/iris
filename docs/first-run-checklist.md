# First-run checklist: verify by eye on real Windows

This build was produced and reviewed entirely from WSL2, which has no Windows
interop in this environment — nothing below has ever actually run. Everything
here compiles, cross-compiles, and is unit/integration-tested on the portable
half of the codebase (`cargo test --workspace`), but none of it substitutes
for someone looking at a real Windows desktop. Work through this list once,
on the machine you're actually going to use Iris on.

## Launch

- [ ] `iris.exe` starts without a console flash-and-vanish (a silent crash on
      launch looks exactly like that).
- [ ] The startup banner prints exactly three lines — hotkey, listening
      device, and the settings path — and nothing else (no per-dictation
      output, no millisecond figures). Compile- and review-verified only;
      `banner()` is `#[cfg(windows)]` and has never executed anywhere this
      build was produced.
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
- [ ] Running it a second time while Iris is running says "Iris is running.
      Quit it first" instead of a raw file-in-use error.
- [ ] The Start Menu shortcut appears and launches Iris, with its console
      window minimized rather than sitting open on the desktop.
- [ ] `-Desktop` adds a working desktop shortcut.
- [ ] `-RunAtLogin` actually starts Iris after a real login (not just a
      shortcut sitting in the Startup folder) — log out and back in, or
      restart, to confirm.
- [ ] The zip README's **Uninstall** steps are complete: after deleting
      `%LOCALAPPDATA%\Iris` and the shortcuts it names, nothing of Iris is
      left running, launchable or listed. Confirm the claim they rest on —
      that Iris never appears in Settings → Apps → Installed apps, because
      `install.ps1` writes no registry entry.

## First-run config

- [ ] `%APPDATA%\iris\config.toml` is created on first launch, with the
      header comment readable and the `[keys]` example easy to follow for
      someone who has never opened a TOML file.
- [ ] With no key configured, the app runs on the mock engine without
      crashing (dictation "works" but transcribes to a stub).
- [ ] Launched from the Start Menu shortcut — minimized, so the startup banner
      is never seen, which is the whole point — the tray menu opens with four
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
      message arrives as an "Iris could not start" dialog box, not a console
      window that flashes and closes before it can be read.
- [ ] The same misconfiguration launched from an open PowerShell prompt
      prints to the console and shows *no* dialog.
- [ ] Upgrading over a `config.toml` an older build wrote: the first start
      rewrites it, adding `version = 1` and turning `show_live_text` off once.
      Everything else in the file survives, and a `show_live_text = true` set
      by hand afterwards sticks across restarts. `iris --verbose` from an open
      PowerShell prompt is where that one-time rewrite is reported; a failed
      rewrite prints unconditionally, but on a Start-Menu launch that lands in
      Iris's own minimized console rather than a dialog, so the reset quietly
      repeats on every start until the stamp reaches disk. Going back to an
      older build needs the `version` line deleted first — the zip README's
      "Downgrading to an older Iris" (`packaging/windows/README.md`) is the
      copy of that path to keep accurate.
- [ ] Tray → Settings opens `config.toml` in the default editor.
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
      disabled label, not a menu — there is no rebinding from the tray on this
      build, and its absence is not a broken build.
- [ ] Rebinding works the way it actually ships: set `hotkey` in
      `config.toml` (tray → Settings), save, then quit from the tray and
      launch Iris again. The low-level hook is installed once at startup, so
      tray → Reload settings only reports "hotkey changed: restart Iris for
      that to take effect" and keeps the old binding (`suppress_hotkey` is
      the same story). `iris.exe --hotkey f9` does the rebind for one run
      without touching the file.
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

## Dictation quality

- [ ] Latency "feels" fast — the budget in `docs/spike-findings.md` was
      measured on different hardware and network conditions.
- [ ] Accuracy is reasonable for normal speech.
- [ ] Injection lands correctly in a few different real target apps
      (browser address bar, a chat app, a plain text editor, something that
      only accepts paste). `inject::effective_method`'s clipboard fallback
      for long transcripts is unit-tested but never watched land on a real
      focused window.

## What this build does not include

The round-3 overlay capsule (PR #15) is in the tree now, and the Overlay and
First-run config items above are written for it — repackage with
`scripts/package-windows.sh` before working through this list, or the zip you
verify predates it. One branch is still in flight: the Settings window (PR
#13). If it has since merged, this checklist and the packaged zip are stale —
repackage and re-run this list.

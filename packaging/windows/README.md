# Iris — install and first run

**Fast, minimal voice dictation.** Hold a key, speak, release — your words
appear in whatever app you're using.

This folder is everything you need: `iris.exe`, `install.ps1`, this file and
the licence. Nothing else to download.

## "Windows protected your PC"

`iris.exe` is not code-signed — signing needs a paid certificate this project
does not have. If you downloaded this zip rather than building it yourself,
Windows marks it, and the first launch shows SmartScreen's blue **"Windows
protected your PC"** box, whose only obvious button is *Don't run*. Click
**More info**, then **Run anyway**. You can also head it off before
extracting: right-click the zip → Properties → tick **Unblock** → OK.

Only do this for a zip you built yourself or got from someone you trust — that
dialog is doing its job, and this section is telling you how to answer it, not
that it is wrong.

## Install

1. Extract this zip anywhere.
2. Right-click `install.ps1` → **Run with PowerShell**. If Windows refuses
   ("running scripts is disabled on this system"), open PowerShell in this
   folder instead and run:
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1
   ```
   Add `-Desktop` for a desktop shortcut and/or `-RunAtLogin` to start Iris
   automatically when you log in:
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1 -Desktop -RunAtLogin
   ```

This copies `iris.exe` into `%LOCALAPPDATA%\Iris` and adds a Start Menu
shortcut. No admin rights needed; nothing is written outside your user
profile. `-RunAtLogin` adds a shortcut in your Startup folder — delete it to
undo, there is no registry entry.

3. Launch **Iris** from the Start Menu (or your shortcut).

## First run

First launch writes `%APPDATA%\iris\config.toml` — commented defaults, no key
needed yet. Out of the box Iris runs the offline **mock** engine, so dictation
"works" but the transcript is a stub, not what you said.

To dictate for real, right-click the tray icon → **Open settings…**, which
opens `config.toml` in your editor. Its header comment says the same as what
follows. There are two separate edits, and they go in two different places —
pasting them together as one block leaves a file Iris refuses to load.

**1. Change the engine, in place.** Near the top of the file, among the other
settings, is a line reading `engine = "mock"`. Edit that line — do not add a
second one — so it reads:

```toml
engine = "deepgram"
```

Use `engine = "groq"` instead if you are using Groq.

**2. Add your key at the very end of the file.** Get a key from
<https://console.deepgram.com> (or <https://console.groq.com/keys> for Groq),
then scroll past every other setting, to the very bottom, and add:

```toml
[keys]
deepgram = "paste-your-key-here"
```

For Groq the entry is `groq = "paste-your-key-here"` in that same table.

This has to be the last thing in the file: TOML puts every line after a
`[keys]` header inside that table, so a `[keys]` block anywhere else swallows
the settings below it and Iris will not start.

Keys in this file are never printed back by Iris.

Save the file, then **restart Iris** — right-click the tray icon → **Quit
Iris**, and launch it again. A key is read once at startup, so **Reload
settings** does not apply one: it reports that the keys changed and keeps
running on the engine Iris started with.

Then hold **Right-Ctrl**, speak, release. Your words appear in whatever window
had focus.

## Downgrading to an older Iris

Going *back* to a release older than this one needs one edit first. Iris stamps
`config.toml` with a `version` line the first time a newer build reads it, and
older builds reject any setting they do not recognise — so an older Iris opens
an **Iris could not start** dialog naming an unknown `version` field instead of
starting.

Fix it in one step: open `%APPDATA%\iris\config.toml` and delete the line

```toml
version = 1
```

Then launch the older Iris. Nothing else in the file needs changing.

One related thing worth knowing. The newer build turns off the overlay's live
transcript text once, on the first start after you upgrade — the capsule still
pulses and times your recording, it just no longer shows words as you speak.
Setting `show_live_text = true` turns it back on, and the stamp is what stops
it being turned off again. The older build never writes a `version` line, so
upgrading again later repeats that one-time reset.

## Uninstall

Iris does not register itself with Windows, so it does **not** appear in
**Settings → Apps → Installed apps** — there is nothing to click there, and
that is not a bug. Removing it is deleting what `install.ps1` created:

1. Quit Iris — right-click the tray icon → **Quit Iris**.
2. Delete the folder `%LOCALAPPDATA%\Iris`. That is the whole app.
3. Delete the shortcuts. Find the Start Menu one the way you launch it —
   open the Start Menu, search for **Iris**, right-click the result →
   **Open file location**, and delete the `Iris.lnk` that Explorer highlights.
   If you installed with `-Desktop` there is one on your Desktop too, and with
   `-RunAtLogin` one more in your Startup folder (press Win+R and run
   `shell:startup` to open it). Deleting the Startup one is also how you stop
   Iris starting at login without removing the rest.

   Those folders are usually `%APPDATA%\Microsoft\Windows\Start Menu\Programs`
   and `...\Programs\Startup` if you would rather check directly — but
   Windows moves them on some machines (Group Policy folder redirection, some
   work laptops, some OEM images), which is why the installer asks Windows
   where they are rather than assuming, and why the search above is the
   instruction that always finds them.
4. Optional: delete `%APPDATA%\iris` as well. That folder holds your settings
   (including any key you pasted in) and your dictation history — leave it if
   you might reinstall, since a fresh install picks it straight back up.

Nothing is written outside your user profile and there are no registry
entries, so those deletions are the complete uninstall.

## Anything else

Troubleshooting, the full configuration reference, the source, and the
developer docs: <https://github.com/omarovski-27/iris>

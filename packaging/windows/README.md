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
opens `config.toml` in your editor. Its header comment walks through getting a
Deepgram or Groq key. Set the engine and paste the key:

```toml
engine = "deepgram"   # or "groq"

# Keep [keys] last: everything after a table header belongs to that table.
[keys]
deepgram = "your-deepgram-key"
# groq = "your-groq-key"
```

Keys in this file are never printed back by Iris.

Save the file, then **restart Iris** — right-click the tray icon → **Quit
Iris**, and launch it again. A key is read once at startup, so **Reload
settings** does not apply one: it reports that the keys changed and keeps
running on the engine Iris started with.

Then hold **Right-Ctrl**, speak, release. Your words appear in whatever window
had focus.

## Anything else

Troubleshooting, the full configuration reference, the source, and the
developer docs: <https://github.com/omarovski-27/iris>

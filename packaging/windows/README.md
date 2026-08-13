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
   automatically when you log in — quietly, in the tray, ready for the
   hotkey, without putting its window on screen at every login:
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1 -Desktop -RunAtLogin
   ```

This copies `iris.exe` into `%LOCALAPPDATA%\Iris` and adds a Start Menu
shortcut. No admin rights needed; nothing is written outside your user
profile. `-RunAtLogin` adds a shortcut in your Startup folder — delete it to
undo, there is no registry entry.

Re-running this script — to upgrade, or to add `-Desktop`/`-RunAtLogin` you
skipped the first time — is a **clean replace**, in one step: it quits any
Iris that is currently running, deletes whatever Start Menu, Desktop and
Startup shortcuts a previous run created, and replaces `%LOCALAPPDATA%\Iris`
before copying the new build in. You never need to quit Iris or delete
anything by hand first. Your settings and dictation history live in
`%LOCALAPPDATA%\IrisConfig`, a separate folder this script never touches, so an
upgrade never resets your key or your history — and if re-running with
different flags means a shortcut you had (say, the Startup one) is no longer
requested, it is removed rather than left stale.

3. Launch **Iris** from the Start Menu (or your shortcut). Its window (History,
   Settings, Insights) opens on screen — that is Iris, running; a tray icon
   also appears, and closing the window later leaves dictation running from
   there. Launching Iris again while it is already running just brings this
   same window back, rather than starting a second copy.

## First run

First launch writes `%LOCALAPPDATA%\IrisConfig\iris\config.toml` — commented
defaults, no key needed yet. Out of the box Iris runs the offline **mock**
engine, so dictation "works" but the transcript is a stub, not what you said.

To dictate for real, use the Iris window's **Settings** tab and its
**Open config file** button, which hands `config.toml` to your editor — the key
goes in the file, never in the window. (Closed the window? Right-click the
tray icon → **Open settings…** to bring it back.) The file's header comment
says the same as what follows. There are two separate edits, and they go in two
different places — pasting them together as one block leaves a file Iris
refuses to load.

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

## Custom vocabulary

If Iris keeps mishearing the same word — your name, a product, a technical
term, an acronym — you can teach it what to expect. Open the Iris window,
go to the **Settings** tab, and find the **Vocabulary** card: type each word
or phrase on its own line, then click **Save**. It takes effect on your next
dictation, no restart needed.

This is a hint, not a guarantee — it makes the engine more likely to spell a
name or a term the way you meant, not a rule that forces it into what you
say. If you add a very long list, Iris quietly uses as much of it as the
engine allows rather than failing your dictation.

You never need to edit `config.toml` for this — the Settings window does it
for you — but if you ever open the file by hand, this is what it looks like:

```toml
vocabulary = ["Deepgram", "Zipformer", "Kubernetes"]
```

## Seeing your remaining Deepgram balance (optional)

If you are paying for Deepgram out of your own balance, Iris can show what is
left in the Settings tab, and warn you once before it runs out — but only if
you give it a **second**, separate Deepgram key: your ordinary `deepgram` key
above cannot read this, because Deepgram's balance lookup needs a key with the
`billing:read` permission (an Admin- or Owner-role key; the Member role does
not carry it).

Create one at <https://console.deepgram.com> — your project's API keys page,
same place as your ordinary key — then add it as `deepgram_management` in the
same `[keys]` table at the bottom of `config.toml`, alongside `deepgram`:

```
[keys]
deepgram = "paste-your-key-here"
deepgram_management = "paste-your-billing-key-here"
```

This is entirely optional. Leave it out and nothing changes: no balance shown,
no warning, no error, and your dictation key keeps working exactly as before.
With it set, the Settings tab shows the balance and when it was last checked,
with a **Refresh** button, and Iris warns you once when it drops to $5 or
below.

## Downgrading to an older Iris

Going *back* to a release older than this one needs one edit first. Iris stamps
`config.toml` with a `version` line the first time a newer build reads it, and
older builds reject any setting they do not recognise — so an older Iris opens
an **Iris could not start** dialog naming an unknown `version` field instead of
starting.

Fix it in one step: open `%LOCALAPPDATA%\IrisConfig\iris\config.toml` and
delete the line

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
that is not a bug.

1. Open PowerShell in this folder (or the folder you originally extracted the
   zip into — any copy of `install.ps1` does the same removal) and run:
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall
   ```
   This quits Iris if it is running, deletes `%LOCALAPPDATA%\Iris`, and
   deletes every shortcut a previous install created — Start Menu, Desktop,
   and Startup (which is also what stops Iris starting at login). One step,
   nothing to hunt for by hand.
2. Optional: delete `%LOCALAPPDATA%\IrisConfig` as well. That folder holds
   your settings (including any key you pasted in) and your dictation history —
   `-Uninstall` deliberately leaves it alone, so a reinstall picks it straight
   back up. Delete it yourself if you want a full wipe.

Nothing is written outside your user profile and there are no registry
entries, so those two steps are the complete uninstall.

If you would rather not run the script, uninstalling by hand is exactly the
same two things: quit Iris (right-click the tray icon → **Quit Iris**),
delete the folder `%LOCALAPPDATA%\Iris`, and delete the shortcuts — the Start
Menu one is found by opening the Start Menu, searching for **Iris**,
right-clicking the result → **Open file location**, and deleting the
`Iris.lnk` Explorer highlights; a Desktop or Startup one (press Win+R,
`shell:startup`) the same way if you installed with `-Desktop` or
`-RunAtLogin`. Those shortcut folders are usually
`%APPDATA%\Microsoft\Windows\Start Menu\Programs` and `...\Programs\Startup`
if you would rather check directly — but Windows moves them on some machines
(Group Policy folder redirection, some work laptops, some OEM images), which
is why `install.ps1` asks Windows where they are rather than assuming.

## Anything else

Troubleshooting, the full configuration reference, the source, and the
developer docs: <https://github.com/omarovski-27/iris>

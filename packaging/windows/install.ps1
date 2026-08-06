<#
.SYNOPSIS
    Installs Iris for the current user, replacing any previous install in place.

.DESCRIPTION
    Copies iris.exe into %LOCALAPPDATA%\Iris, adds a Start Menu shortcut, and
    optionally a Desktop shortcut and a run-at-login entry. No admin rights
    needed; nothing is written outside the current user's profile.

    Every install is a clean replace, in one step: it quits any Iris that is
    currently running, deletes the Start Menu / Desktop / Startup shortcuts a
    previous run of this script created, and replaces %LOCALAPPDATA%\Iris
    before copying the new build in - so upgrading, or re-running with
    different flags, never leaves a stale shortcut or a stale binary behind.
    Your settings and dictation history live in %APPDATA%\iris, a separate
    folder this script never touches; see -Uninstall.

.PARAMETER Desktop
    Also add a Desktop shortcut.

.PARAMETER RunAtLogin
    Also start Iris automatically when you log in.

.PARAMETER Uninstall
    Remove Iris instead of installing it: quit it if running, delete
    %LOCALAPPDATA%\Iris and every shortcut this script creates, then exit.
    Does not touch %APPDATA%\iris (your settings and dictation history) -
    delete that yourself if you want a full wipe.

.EXAMPLE
    .\install.ps1
    .\install.ps1 -Desktop -RunAtLogin
    .\install.ps1 -Uninstall
#>
param(
    [switch]$Desktop,
    [switch]$RunAtLogin,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

# "Run with PowerShell" closes the window the moment the script returns, so
# every line below - success or failure - would otherwise flash and vanish.
# Only pause when there is actually a console keyboard to read from. The last
# two tests cover different callers and both are needed: a redirected or piped
# run (install.ps1 > install.log) redirects the streams but carries no
# -NonInteractive, and -NonInteractive in a real console redirects neither
# stream but makes the host refuse the prompt outright.
function Test-Interactive {
    if ([Environment]::UserInteractive -ne $true) { return $false }
    if ($Host.Name -eq "ServerRemoteHost") { return $false }
    if ([Environment]::GetCommandLineArgs() -contains "-NonInteractive") { return $false }
    if ([Console]::IsInputRedirected -or [Console]::IsOutputRedirected) { return $false }
    return $true
}

$installDir = Join-Path $env:LOCALAPPDATA "Iris"
$targetExe = Join-Path $installDir "iris.exe"

# Resolved through the shell, not as literal %APPDATA% subpaths: folder
# redirection (Group Policy, some OEM images) moves these, and a shortcut
# written to the unredirected path is never enumerated by Windows. Shared by
# both removal and (re)creation so the two always agree on where "the"
# shortcut is.
$startMenuShortcut = Join-Path ([Environment]::GetFolderPath("Programs")) "Iris.lnk"
$desktopShortcut = Join-Path ([Environment]::GetFolderPath("Desktop")) "Iris.lnk"
$startupShortcut = Join-Path ([Environment]::GetFolderPath("Startup")) "Iris.lnk"

# config.toml and the *.jsonl dictation history only ever live in
# %APPDATA%\iris, never in $installDir - this script owns $installDir
# completely and normally puts nothing there but iris.exe. That separation is
# what makes a clean-replace safe by construction, but "normally" is not a
# promise: a custom --config pointing here, or a future mistake, would put
# real user data in the path of a recursive delete. Check for it every time
# rather than trust the invariant blindly, and refuse to remove anything
# rather than guess.
function Assert-NoUserDataIn($dir) {
    if (-not (Test-Path -LiteralPath $dir)) { return }
    $userFiles = Get-ChildItem -LiteralPath $dir -Recurse -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -eq "config.toml" -or $_.Extension -eq ".jsonl" }
    if ($userFiles) {
        $names = ($userFiles | ForEach-Object { $_.FullName }) -join "`n  "
        throw ("Refusing to remove $dir - it contains what looks like Iris settings or " +
            "dictation history:`n  $names`nMove or back up those files, then run this script again.")
    }
}

# Windows locks a running image against writes, so upgrading over a live Iris
# fails with a raw "used by another process" - and a clean-replace has no
# manual "quit it first" step for the caller to follow, so this does it.
# Killing a resident tray app is safe here: every dictation is written to the
# session log and every setting change to config.toml as it happens (see
# CLAUDE.md), so there is no in-memory state that a graceful shutdown would
# have saved and a forceful one loses.
function Stop-RunningIris {
    $running = Get-Process -Name "iris" -ErrorAction SilentlyContinue
    if ($running) {
        Write-Host "Stopping the running Iris so it can be replaced..."
        $running | Stop-Process -Force
        $running | Wait-Process -Timeout 5 -ErrorAction SilentlyContinue
    }
}

function Remove-PreviousInstall {
    Assert-NoUserDataIn $installDir
    Stop-RunningIris
    $found = $false
    if (Test-Path -LiteralPath $installDir) {
        Remove-Item -LiteralPath $installDir -Recurse -Force
        $found = $true
    }
    foreach ($shortcut in @($startMenuShortcut, $desktopShortcut, $startupShortcut)) {
        if (Test-Path -LiteralPath $shortcut) {
            Remove-Item -LiteralPath $shortcut -Force
            $found = $true
        }
    }
    return $found
}

try {
    if ($Uninstall) {
        if (Remove-PreviousInstall) {
            Write-Host "Iris removed: $installDir and its shortcuts are gone."
        } else {
            Write-Host "Nothing to remove - Iris was not installed."
        }
        Write-Host "Your settings and dictation history are untouched, at %APPDATA%\iris."
        return
    }

    $sourceExe = Join-Path $PSScriptRoot "iris.exe"
    if (-not (Test-Path $sourceExe)) {
        throw "iris.exe not found next to install.ps1 - run this script from inside the extracted Iris folder."
    }

    # Extracting the zip straight into %LOCALAPPDATA%\Iris makes source and
    # target the same file. Check for that *before* any removal runs below -
    # a clean-replace that deletes $installDir would otherwise delete the very
    # file it is about to copy from. Compare resolved paths, since either side
    # can arrive via a link or a short name.
    $resolvedSource = (Resolve-Path -LiteralPath $sourceExe).Path
    $resolvedTarget = if (Test-Path -LiteralPath $targetExe) {
        (Resolve-Path -LiteralPath $targetExe).Path
    } else {
        $null
    }

    if ($resolvedTarget -eq $resolvedSource) {
        Write-Host "Already installed at $targetExe - keeping it in place."
    } else {
        Assert-NoUserDataIn $installDir
        Stop-RunningIris
        if (Test-Path -LiteralPath $installDir) {
            Remove-Item -LiteralPath $installDir -Recurse -Force
        }
        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
        Copy-Item -LiteralPath $resolvedSource -Destination $targetExe -Force
    }

    # Clean-replace the shortcuts too: delete whatever a previous run left,
    # then create only what *this* run asks for. Otherwise re-running without
    # -RunAtLogin (or -Desktop) leaves the old one behind, silently still
    # autostarting or sitting on the desktop.
    foreach ($shortcut in @($startMenuShortcut, $desktopShortcut, $startupShortcut)) {
        if (Test-Path -LiteralPath $shortcut) {
            Remove-Item -LiteralPath $shortcut -Force
        }
    }

    $shell = New-Object -ComObject WScript.Shell

    function New-IrisShortcut {
        param([string]$Path)
        $parent = Split-Path -Parent $Path
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
        $shortcut = $shell.CreateShortcut($Path)
        $shortcut.TargetPath = $targetExe
        $shortcut.WorkingDirectory = $installDir
        $shortcut.Description = "Iris - push-to-talk dictation"
        $shortcut.Save()
    }

    New-IrisShortcut -Path $startMenuShortcut
    Write-Host "Start Menu shortcut created."

    if ($Desktop) {
        New-IrisShortcut -Path $desktopShortcut
        Write-Host "Desktop shortcut created."
    }

    if ($RunAtLogin) {
        New-IrisShortcut -Path $startupShortcut
        Write-Host "Iris will now start automatically at login."
        Write-Host "To undo: delete `"$startupShortcut`""
    }

    Write-Host ""
    Write-Host "Installed to $targetExe"
    Write-Host "Launch it from the Start Menu, or directly:"
    Write-Host "  & `"$targetExe`""
    Write-Host ""
    Write-Host "First run creates %APPDATA%\iris\config.toml with a commented"
    Write-Host "example for adding a Deepgram or Groq key. See the README.md"
    Write-Host "next to this script, section `"First run`", for the full"
    Write-Host "walkthrough."
}
catch {
    Write-Host ""
    Write-Host "Install failed: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
finally {
    # PowerShell runs this before the `exit` above, so one copy covers the
    # failure path, the success path, and any exit added later.
    if (Test-Interactive) {
        Write-Host ""
        Read-Host "Press Enter to close"
    }
}

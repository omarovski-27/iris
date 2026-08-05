<#
.SYNOPSIS
    Installs Iris for the current user.

.DESCRIPTION
    Copies iris.exe into %LOCALAPPDATA%\Iris, adds a Start Menu shortcut, and
    optionally a Desktop shortcut and a run-at-login entry. No admin rights
    needed; nothing is written outside the current user's profile. Run-at-login
    is a shortcut in the per-user Startup folder, not a registry key, so
    removing it is one file delete.

.PARAMETER Desktop
    Also add a Desktop shortcut.

.PARAMETER RunAtLogin
    Also start Iris automatically when you log in.

.EXAMPLE
    .\install.ps1
    .\install.ps1 -Desktop -RunAtLogin
#>
param(
    [switch]$Desktop,
    [switch]$RunAtLogin
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

try {
    $sourceExe = Join-Path $PSScriptRoot "iris.exe"
    if (-not (Test-Path $sourceExe)) {
        throw "iris.exe not found next to install.ps1 - run this script from inside the extracted Iris folder."
    }

    $installDir = Join-Path $env:LOCALAPPDATA "Iris"
    $targetExe = Join-Path $installDir "iris.exe"

    # Windows locks a running image against writes, so upgrading over a live
    # Iris fails with a raw "used by another process". Say what to do instead.
    if (Get-Process -Name "iris" -ErrorAction SilentlyContinue) {
        throw "Iris is running. Quit it first (right-click the tray icon -> Quit), then run this installer again."
    }

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null

    # Extracting the zip straight into %LOCALAPPDATA%\Iris makes source and
    # target the same file, and Copy-Item refuses to copy a file onto itself -
    # which would abort an install that is in fact already correct. Compare
    # resolved paths, since either side can arrive via a link or a short name.
    $resolvedSource = (Resolve-Path -LiteralPath $sourceExe).Path
    $resolvedTarget = if (Test-Path -LiteralPath $targetExe) {
        (Resolve-Path -LiteralPath $targetExe).Path
    } else {
        $null
    }
    if ($resolvedTarget -eq $resolvedSource) {
        Write-Host "Already installed at $targetExe - keeping it in place."
    } else {
        Copy-Item -LiteralPath $resolvedSource -Destination $targetExe -Force
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
        # iris.exe is a console binary (the startup banner is deliberate), so
        # without this every launch pops a console window and leaves it open
        # for the life of the app. 7 = minimized.
        $shortcut.WindowStyle = 7
        $shortcut.Save()
    }

    # Resolved through the shell, not as literal %APPDATA% subpaths: folder
    # redirection (Group Policy, some OEM images) moves these, and a shortcut
    # written to the unredirected path is never enumerated by Windows.
    $startMenuDir = [Environment]::GetFolderPath("Programs")
    New-IrisShortcut -Path (Join-Path $startMenuDir "Iris.lnk")
    Write-Host "Start Menu shortcut created."

    if ($Desktop) {
        $desktopDir = [Environment]::GetFolderPath("Desktop")
        New-IrisShortcut -Path (Join-Path $desktopDir "Iris.lnk")
        Write-Host "Desktop shortcut created."
    }

    if ($RunAtLogin) {
        $startupDir = [Environment]::GetFolderPath("Startup")
        $startupShortcut = Join-Path $startupDir "Iris.lnk"
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

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

$sourceExe = Join-Path $PSScriptRoot "iris.exe"
if (-not (Test-Path $sourceExe)) {
    Write-Error "iris.exe not found next to install.ps1 - run this script from inside the extracted Iris folder."
    exit 1
}

$installDir = Join-Path $env:LOCALAPPDATA "Iris"
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
$targetExe = Join-Path $installDir "iris.exe"
Copy-Item -Path $sourceExe -Destination $targetExe -Force

$shell = New-Object -ComObject WScript.Shell

function New-IrisShortcut {
    param([string]$Path)
    $shortcut = $shell.CreateShortcut($Path)
    $shortcut.TargetPath = $targetExe
    $shortcut.WorkingDirectory = $installDir
    $shortcut.Description = "Iris - push-to-talk dictation"
    $shortcut.Save()
}

$startMenuDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs"
New-IrisShortcut -Path (Join-Path $startMenuDir "Iris.lnk")
Write-Host "Start Menu shortcut created."

if ($Desktop) {
    $desktopDir = [Environment]::GetFolderPath("Desktop")
    New-IrisShortcut -Path (Join-Path $desktopDir "Iris.lnk")
    Write-Host "Desktop shortcut created."
}

if ($RunAtLogin) {
    $startupDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Startup"
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
Write-Host "First run creates %APPDATA%\iris\config.toml with a commented-out"
Write-Host "example for adding a Deepgram or Groq key. See README.md's INSTALL"
Write-Host "section for the full first-run walkthrough."

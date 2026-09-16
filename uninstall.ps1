#!/usr/bin/env pwsh
#
# Remove the MDM install that install.ps1 made.
#
#   .\uninstall.ps1          remove the app, keep downloads and settings
#   .\uninstall.ps1 -Purge   remove those too
#
# Only for an install made by install.ps1. A machine set up from the packaged
# setup.exe has a real uninstall entry in Windows' "Installed apps" — use that
# instead, so its own uninstaller runs.
[CmdletBinding()]
param(
    # Also delete %APPDATA%\mdm: the database, settings.toml, and the yt-dlp
    # and QuickJS binaries the app fetched for itself.
    [switch]$Purge
)
$ErrorActionPreference = "Continue"

$BinDir   = "$env:LOCALAPPDATA\Programs\mdm"
$DataDir  = "$env:APPDATA\mdm"
$HostName = "io.mdm.host"

function Say($msg)  { Write-Host "==> $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host " warning: $msg" -ForegroundColor Yellow }

# ------------------------------------------------------------------ processes

# The app has to be stopped before its .exe can be deleted, and mdm-host has to
# go with it: a running browser restarts the host the moment it dies, and the
# host launches the app again beside itself. Killing them in the other order
# leaves the pair alive.
Say "Stopping the app"
foreach ($name in @("mdm-host", "mdm")) {
    Get-Process $name -ErrorAction SilentlyContinue | ForEach-Object {
        # Only ours. Another program called "mdm" is not this one.
        if ($_.Path -and $_.Path.StartsWith($BinDir, "OrdinalIgnoreCase")) {
            Stop-Process -Id $_.Id -Force -Confirm:$false -ErrorAction SilentlyContinue
        }
    }
}
Start-Sleep -Milliseconds 800

# ------------------------------------------------------------------- registry

Say "Removing the registry entries"
$keys = @(
    "HKCU:\Software\Mozilla\NativeMessagingHosts\$HostName",
    "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName",
    "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\$HostName",
    "HKCU:\Software\Chromium\NativeMessagingHosts\$HostName",
    "HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\$HostName",
    "HKCU:\Software\Vivaldi\NativeMessagingHosts\$HostName",
    "HKCU:\Software\Classes\mdm"
)
foreach ($key in $keys) {
    if (Test-Path $key) {
        Remove-Item $key -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# ----------------------------------------------------------------- start menu

Say "Removing the Start Menu shortcut"
$Shortcut = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\My Download Manager.lnk"
if (Test-Path $Shortcut) { Remove-Item $Shortcut -Force -ErrorAction SilentlyContinue }

# ------------------------------------------------------------------- binaries

Say "Removing $BinDir"
if (Test-Path $BinDir) {
    try {
        Remove-Item $BinDir -Recurse -Force -ErrorAction Stop
    } catch {
        Warn "could not remove $BinDir ($($_.Exception.Message)). Something is
  probably still running -- close your browser and run this again."
    }
}

# ----------------------------------------------------------------- extensions

# The manifests go whether or not the data is being kept: they name a host
# binary that no longer exists, and a browser that reads one gets a connection
# failure rather than a clean "not installed".
foreach ($file in @("$DataDir\$HostName.json", "$DataDir\$HostName.chrome.json")) {
    if (Test-Path $file) { Remove-Item $file -Force -ErrorAction SilentlyContinue }
}
# Both browsers' copies, from the one directory an install keeps them in --
# and from the flat layout an older install.ps1 used, which is still on any
# machine that has not been re-installed since. They are install artefacts
# rather than anything the user made, so they go whether or not the data is
# being kept; the Chromium folder especially, because a browser reads it at
# every start and would otherwise be running an extension that outlives the
# app it talks to.
foreach ($path in @("$DataDir\extension", "$DataDir\mdm-firefox.xpi", "$DataDir\mdm-chrome")) {
    if (Test-Path $path) { Remove-Item $path -Recurse -Force -ErrorAction SilentlyContinue }
}

# ----------------------------------------------------------------- user data

if ($Purge) {
    Say "Removing $DataDir"
    if (Test-Path $DataDir) {
        Remove-Item $DataDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host ""
Say "Uninstalled"
Write-Host ""
if ($Purge) {
    Write-Host "  Everything is gone, downloads database included."
} else {
    Write-Host "  Kept: $DataDir"
    Write-Host "        the downloads database, settings.toml, and the yt-dlp and"
    Write-Host "        QuickJS binaries the app fetched for itself. Re-run with"
    Write-Host "        -Purge to remove those as well."
}
Write-Host ""
Write-Host "  Files already downloaded are untouched, wherever you saved them."
Write-Host ""
Write-Host "The browser extension is not removed by this script -- a browser only"
Write-Host "lets you remove an extension from inside it:"
Write-Host ""
Write-Host "  Firefox   about:addons -> Extensions -> My Download Manager -> Remove"
Write-Host "  Chromium  chrome://extensions -> My Download Manager -> Remove"
Write-Host "            (edge://extensions, brave://extensions -- same page)"

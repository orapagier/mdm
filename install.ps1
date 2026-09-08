#!/usr/bin/env pwsh
#
# Build and install MDM (My Download Manager) for the current user on Windows.
#
#   .\install.ps1               build and install on this machine
#   .\install.ps1 -Installer    also build the redistributable setup.exe
#   .\install.ps1 -BundleOnly   build the setup.exe and install nothing
#
[CmdletBinding()]
param(
    # Produce target\release\bundle\nsis\*-setup.exe as well as installing.
    # Separate because it needs the Tauri CLI and adds a couple of minutes, and
    # most runs of this script are "put my own build on my own machine".
    [switch]$Installer,

    # Build the setup.exe and leave this machine untouched: no binaries copied,
    # no registry values, no Start Menu entry. For building the thing you are
    # about to test *as a user would install it*, which cannot be done honestly
    # on a machine the script has already set up by hand.
    [switch]$BundleOnly
)
# -BundleOnly implies -Installer: it is the same build, minus the install.
if ($BundleOnly) { $Installer = $true }
# Nothing here needs an elevated (Administrator) prompt, and nothing is
# installed from anywhere: the binaries, the registry keys and the Start Menu
# entry are all per-user, and the app fetches the tools it needs itself. Not
# even winget is required. This is the Windows counterpart to install.sh.

# Deliberately not "Stop": several steps below are native executables (cargo,
# winget) whose normal progress output goes to stderr, which PowerShell would
# otherwise promote into a script-terminating error the moment one printed a
# line — even on success. Every step that actually must not be ignored checks
# $LASTEXITCODE (native commands) or is wrapped in its own try/catch instead.
$ErrorActionPreference = "Continue"

# Resolve-Path also drops the "Microsoft.PowerShell.Core\FileSystem::" prefix
# that a UNC working directory leaves on this path, which cargo -- and every
# other native tool this script drives -- would not understand.
$Repo     = (Resolve-Path (Split-Path -Parent $MyInvocation.MyCommand.Path)).ProviderPath
$BinDir   = "$env:LOCALAPPDATA\Programs\mdm"
$ExtId    = "mdm@ramlej.local"
# Chromium derives an extension's id from its public key, so pinning the key in
# manifest.chrome.json pins the id -- which is what lets the native messaging
# manifest name it before the extension has ever been installed, and what keeps
# it the same whether it is loaded unpacked or from a store listing.
$ChromeId = "pegdlonllkokelfmdafooihklghlkimh"
$HostName = "io.mdm.host"

function Say($msg)  { Write-Host "==> $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host " warning: $msg" -ForegroundColor Yellow }
function Die($msg)  { Write-Host " error: $msg" -ForegroundColor Red; exit 1 }

# Remove a directory tree.
#
# Not Remove-Item -Recurse: over an RDP redirected drive the redirector cannot
# be asked to walk a tree while it is deleting it. It gives up with "There are
# no more files" (ERROR_NO_MORE_FILES) -- even when handed a directory that is
# already empty -- having removed the files but not the directories, and then
# reports the parent as already gone.
#
# What made that expensive rather than merely untidy was the line that used to
# follow each call in the packaging below: Copy-Item onto a destination that
# still exists copies the source *into* it, so extension\ landed as
# mdm-chrome\extension\ and the manifest the next step wanted was a level
# deeper than it looked. The Chromium folder was left with no manifest of its
# own at all, and a browser handed the nested folder instead read the Firefox
# one -- which carries no pinned key, so it derived an id from the path, and
# the native host, which allows exactly one id, refused it. Nothing said so:
# the popup simply sat on "Checking..." for good.
#
# So: the files one at a time, then the directories deepest first -- ordered by
# how many separators a path has rather than by how long it is, which would
# sort a long name above a deeper one -- each already empty by the time its
# turn comes. Directory::Delete rather than Remove-Item because Remove-Item
# asks before removing a directory with anything left in it, and -Force does
# not suppress that prompt; only -Recurse does, which is the one thing that
# cannot be used here. Under -NonInteractive the prompt is an error, not a
# pause.
function Remove-Tree($path) {
    if (-not (Test-Path $path)) { return }
    Get-ChildItem $path -Recurse -Force -File -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item $_.FullName -Force -Confirm:$false -ErrorAction SilentlyContinue }
    $dirs = Get-ChildItem $path -Recurse -Force -Directory -ErrorAction SilentlyContinue |
        Sort-Object { $_.FullName.Split([System.IO.Path]::DirectorySeparatorChar).Count } -Descending
    foreach ($d in $dirs) {
        try { [System.IO.Directory]::Delete($d.FullName, $false) } catch { }
    }
    try { [System.IO.Directory]::Delete($path, $false) } catch { }
}

# The same, then hand back an empty directory ready to be filled. Callers copy
# *contents* into it, which behaves the same whichever filesystem it lands on.
function Reset-Dir($path) {
    Remove-Tree $path
    if (Test-Path $path) { Die "could not clear $path" }
    New-Item -ItemType Directory -Force -Path $path | Out-Null
}

# Zip the contents of a directory, with the entry names a zip is meant to have.
#
# Not Compress-Archive, which writes entry names using the *platform* separator:
# an archive built here says "src\background.js" where the format requires
# "src/background.js" (APPNOTE 4.4.17.1). unzip says so out loud -- "appears to
# use backslashes as path separators" -- and these two archives are exactly the
# ones submitted to addons.mozilla.org and the Chrome Web Store. The copy that
# came back signed from AMO carries forward slashes; every copy built here
# carried backslashes.
#
# Entry by entry rather than ZipFile::CreateFromDirectory, which takes the
# separator from the platform in the same way and would only move the bug.
#
# Writing straight to the destination name is also why the .xpi no longer has
# to be built as a .zip and renamed: Compress-Archive refuses any extension but
# .zip, and nothing here cares.
function New-Zip($sourceDir, $destination) {
    Add-Type -AssemblyName System.IO.Compression | Out-Null
    Add-Type -AssemblyName System.IO.Compression.FileSystem | Out-Null
    if (Test-Path $destination) { Remove-Item $destination -Force }
    $sep  = [System.IO.Path]::DirectorySeparatorChar
    $root = (Resolve-Path $sourceDir).ProviderPath.TrimEnd($sep)
    $zip = [System.IO.Compression.ZipFile]::Open(
        $destination, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($file in Get-ChildItem $root -Recurse -Force -File) {
            $name = $file.FullName.Substring($root.Length + 1).Replace($sep, [char]'/')
            [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $zip, $file.FullName, $name,
                [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
        }
    } finally {
        $zip.Dispose()
    }
}

# ------------------------------------------------------------- dependencies

# There are none left to install.
#
# aria2 went first: plain HTTP downloads are fetched in process, so the daemon
# bought a stock install nothing at all.
#
# yt-dlp is fetched by the app itself, into %APPDATA%\mdm\bin, on first run and
# kept current after that. A tool that has to move weekly cannot be left to
# whatever someone installed once; a copy already on PATH is used as it is and
# never written to.
#
# ffmpeg became a fallback rather than a dependency once the app could merge
# two streams itself — MP4 in stream::mp4, WebM in stream::mkv — which is what
# the default format now asks for.
#
# A JavaScript runtime, for YouTube. chr(39) .s player challenge, used to mean Node:
# ninety-eight megabytes of toolchain for a few milliseconds of arithmetic. The
# app fetches QuickJS instead, two megabytes, beside its yt-dlp. A Deno or Node
# the user installed themselves is still preferred over it.


# ------------------------------------------------------------- rust toolchain

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Die "Rust was not found. Install it from https://rustup.rs, then re-run this script."
}

$msrvMatch = Select-String -Path "$Repo\Cargo.toml" -Pattern 'rust-version\s*=\s*"([0-9.]+)"'
$msrv = $msrvMatch.Matches[0].Groups[1].Value
$haveRaw = (cargo --version)
$have = if ($haveRaw -match 'cargo ([0-9.]+)') { $Matches[1] } else { "0.0.0" }
if ([version]$have -lt [version]$msrv) {
    Die "cargo $have is older than the $msrv this workspace needs. Update with: rustup update"
}

# --------------------------------------------------------- the build directory

# Where cargo puts its object files: $Repo\target for an ordinary checkout, and
# somewhere on local disk when the checkout itself is not on one.
#
# rustc cannot build on a network path. std's remove_dir_all asks for a delete
# with POSIX semantics (FILE_DISPOSITION_INFORMATION_EX), and the RDP drive
# redirector behind \\tsclient\... does not implement it, so every temporary
# directory rustc and the build scripts create fails to clean up with "The
# parameter is incorrect. (os error 87)". That reads as a compiler bug rather
# than as a filesystem being asked for something it cannot do, and the first
# crate in the graph is enough to stop the build -- unicode-ident, in practice,
# which makes it look even less like a filesystem problem than it is.
#
# Only the writes are affected: the sources read over the share perfectly well,
# so nothing has to be copied anywhere first. Moving the build directory is the
# whole fix. It is keyed by the repo path so two checkouts reached over the same
# share cannot quietly share one directory and rebuild each other's work.
#
# Exported as CARGO_TARGET_DIR rather than passed as --target-dir because
# `cargo tauri build` further down runs a cargo of its own and asks it where the
# target directory is; the environment reaches both. A CARGO_TARGET_DIR the
# caller set always wins, network path or not.
if ($env:CARGO_TARGET_DIR) {
    $Target = $env:CARGO_TARGET_DIR
} else {
    $onNetwork = $Repo.StartsWith("\\")
    if (-not $onNetwork) {
        # A mapped drive letter is the same share wearing a shorter name, and
        # fails in exactly the same way. An unrecognised drive is no reason to
        # stop: assume local, and let cargo be the one to complain if it isn't.
        try {
            $drive = New-Object System.IO.DriveInfo `
                -ArgumentList ((Split-Path -Qualifier $Repo) + "\")
            $onNetwork = $drive.DriveType -eq "Network"
        } catch { }
    }
    if ($onNetwork) {
        $md5 = [System.Security.Cryptography.MD5]::Create()
        $sum = $md5.ComputeHash([System.Text.Encoding]::Unicode.GetBytes($Repo.ToLower()))
        $tag = [System.BitConverter]::ToString($sum).Replace("-", "").Substring(0, 8)
        $Target = "$env:LOCALAPPDATA\mdm\build\$(Split-Path -Leaf $Repo)-$tag"
        $env:CARGO_TARGET_DIR = $Target
        Warn ("$Repo is on a network drive, which rustc cannot build on: it " +
              "cleans up its temporary directories in a way the drive " +
              "redirector rejects (os error 87). Building in $Target instead " +
              "-- set CARGO_TARGET_DIR to that path to build there by hand too.")
    } else {
        $Target = "$Repo\target"
    }
}

# ------------------------------------------------------------------- build

Say "Building (release)"
Push-Location $Repo
try {
    cargo build --release --workspace
    if ($LASTEXITCODE -ne 0) { Die "cargo build failed" }
} finally {
    Pop-Location
}

$AppBin  = "$Target\release\mdm.exe"
$HostBin = "$Target\release\mdm-host.exe"
if (-not (Test-Path $AppBin))  { Die "build did not produce $AppBin" }
if (-not (Test-Path $HostBin)) { Die "build did not produce $HostBin" }

if (-not $BundleOnly) {

# ---------------------------------------------------------------- install

Say "Installing binaries to $BinDir"
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

# Windows will not overwrite a running .exe, and stopping the app first does
# not help: Firefox restarts the native messaging host the moment it dies, and
# the host launches the app again beside itself, so the two processes are back
# within a second. What Windows *does* allow is renaming a running image --
# the open handle follows the file -- which frees the name for the new build
# while the old one keeps running until it is next closed.
#
# The failure this replaces was silent: both copies threw,
# $ErrorActionPreference is deliberately Continue for the native tools this
# script drives, and it went on to print "Installed" over binaries it had not
# touched. A build that reports success and changes nothing is worse than one
# that fails, because the next thing anyone does is run the old app and
# conclude the fix did not work.
foreach ($exe in @("mdm.exe", "mdm-host.exe")) {
    $src = "$Target\release\$exe"
    $dest = "$BinDir\$exe"
    if (Test-Path $dest) {
        try {
            Copy-Item $src $dest -Force -ErrorAction Stop
        } catch {
            # Left beside the new binary rather than deleted: the old image is
            # still mapped by a running process, so it cannot go until that
            # process does. The sweep below collects them next time.
            $parked = "$dest.old-$([guid]::NewGuid().ToString('N').Substring(0,8))"
            try {
                Rename-Item $dest $parked -Force -ErrorAction Stop
                Copy-Item $src $dest -Force -ErrorAction Stop
                Warn "$exe was running; the new build is installed and takes effect when you next start the app."
            } catch {
                Die "could not install $dest : $($_.Exception.Message)"
            }
        }
    } else {
        Copy-Item $src $dest -Force -ErrorAction Stop
    }
}

# Images parked by an earlier run, now that whatever was holding them has
# exited. Anything still mapped simply refuses to be deleted and waits again.
Get-ChildItem "$BinDir\*.old-*" -ErrorAction SilentlyContinue | ForEach-Object {
    Remove-Item $_.FullName -Force -ErrorAction SilentlyContinue
}

if (($env:Path -split ";") -notcontains $BinDir) {
    # Parenthesised, because in argument mode `Warn "a" + "b"` is not
    # concatenation. PowerShell binds "a" to $msg and reads the `+ "b"` on the
    # next line as a *separate statement*, then echoes that statement's value
    # as plain output. So the warning stopped at "find each other", and the
    # rest of the sentence turned up underneath it as unformatted text that had
    # never been through Warn at all -- no colour, no " warning:" prefix. On
    # one line it is worse and quieter: the fragments bind to $args instead and
    # are dropped without trace.
    Warn ("$BinDir is not on your PATH; the app and browser find each other " +
          "without it (mdm-host looks for mdm.exe right beside itself), but " +
          "typing `"mdm`" in a terminal won't work until you add it.")
}

Say "Creating a Start Menu shortcut"
$StartMenuDir = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs"
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut("$StartMenuDir\My Download Manager.lnk")
$shortcut.TargetPath = "$BinDir\mdm.exe"
$shortcut.IconLocation = "$BinDir\mdm.exe,0"
$shortcut.Description = "My Download Manager -- accelerated downloads with browser capture"
$shortcut.Save()

Say "Registering the mdm:// URI scheme"
# Mirrors the Linux desktop entry's MimeType=x-scheme-handler/mdm, so links
# built as mdm://https://... (the CLI already accepts either mdm:https://...
# or mdm://https://...) can launch the app the same way on both platforms.
New-Item -Path "HKCU:\Software\Classes\mdm" -Force | Out-Null
Set-ItemProperty -Path "HKCU:\Software\Classes\mdm" -Name "(default)" -Value "URL:MDM Protocol"
Set-ItemProperty -Path "HKCU:\Software\Classes\mdm" -Name "URL Protocol" -Value ""
New-Item -Path "HKCU:\Software\Classes\mdm\shell\open\command" -Force | Out-Null
Set-ItemProperty -Path "HKCU:\Software\Classes\mdm\shell\open\command" -Name "(default)" `
    -Value "`"$BinDir\mdm.exe`" `"%1`""

# ------------------------------------------------- native messaging host

# Firefox on Linux finds a native-messaging manifest by searching a handful of
# fixed directories; on Windows it instead reads a registry value that points
# at wherever the manifest actually lives -- so the file can go anywhere, and
# the registry key is what does the "registering".
Say "Registering the native messaging host for Firefox"
$NmDir = "$env:APPDATA\mdm"
New-Item -ItemType Directory -Force -Path $NmDir | Out-Null
$ManifestPath = "$NmDir\$HostName.json"
$manifest = [ordered]@{
    name                = $HostName
    description         = "My Download Manager native host"
    path                = "$BinDir\mdm-host.exe"
    type                = "stdio"
    allowed_extensions  = @($ExtId)
}
($manifest | ConvertTo-Json) | Set-Content -Path $ManifestPath -Encoding utf8

New-Item -Path "HKCU:\Software\Mozilla\NativeMessagingHosts\$HostName" -Force | Out-Null
Set-ItemProperty -Path "HKCU:\Software\Mozilla\NativeMessagingHosts\$HostName" `
    -Name "(default)" -Value $ManifestPath

# Chromium wants its own manifest: the key naming who may connect is
# `allowed_origins` with a chrome-extension:// URL, where Firefox's is
# `allowed_extensions` with a bare id. Same host binary, same protocol, two
# files -- and one registry value per browser family, since Chrome, Edge and
# the rest each read their own hive.
Say "Registering the native messaging host for Chrome and Edge"
$ChromeManifestPath = "$NmDir\$HostName.chrome.json"
$chromeManifest = [ordered]@{
    name            = $HostName
    description     = "My Download Manager native host"
    path            = "$BinDir\mdm-host.exe"
    type            = "stdio"
    allowed_origins = @("chrome-extension://$ChromeId/")
}
($chromeManifest | ConvertTo-Json) | Set-Content -Path $ChromeManifestPath -Encoding utf8

# Every Chromium browser that is actually installed. Writing the key for one
# that is not does no harm -- it is read only by that browser -- so this makes
# no attempt to detect them.
$ChromiumHives = @(
    "HKCU:\Software\Google\Chrome\NativeMessagingHosts",
    "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts",
    "HKCU:\Software\Chromium\NativeMessagingHosts",
    "HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
    "HKCU:\Software\Vivaldi\NativeMessagingHosts"
)
foreach ($hive in $ChromiumHives) {
    New-Item -Path "$hive\$HostName" -Force | Out-Null
    Set-ItemProperty -Path "$hive\$HostName" -Name "(default)" -Value $ChromeManifestPath
}


}
# ---------------------------------------------------------------- extension

Say "Packaging the extension"
# $Repo\target here, not $Target: these are packages rather than build output,
# and tauri.bundle.conf.json names ..\target\mdm-chrome relative to itself --
# a path inside the repo whatever CARGO_TARGET_DIR happens to say. Created
# explicitly because a build that was redirected elsewhere never makes it.
New-Item -ItemType Directory -Force -Path "$Repo\target" | Out-Null
$Xpi = "$Repo\target\mdm-firefox.xpi"
if (Test-Path $Xpi) { Remove-Item $Xpi -Force }

# The test directory is developer-only; shipping it would put dead code in
# front of AMO reviewers and bloat the package -- and nothing here has an
# exclude filter, so it is staged out of a scratch copy instead. The Chromium
# manifest and its service worker go the same way: Firefox reads neither, and
# a reviewer should not have to wonder why they are there.
$Stage = Join-Path $env:TEMP "mdm-xpi-stage"
Reset-Dir $Stage
Copy-Item "$Repo\extension\*" $Stage -Recurse -Force
Remove-Tree "$Stage\test"
Remove-Item "$Stage\manifest.chrome.json" -Force -ErrorAction SilentlyContinue
Remove-Item "$Stage\src\sw.js" -Force -ErrorAction SilentlyContinue
# An .xpi is exactly a .zip under a different name, and New-Zip writes to the
# name it is given, so it is built as one directly.
New-Zip $Stage $Xpi
Remove-Tree $Stage

# ------------------------------------------------- the Chromium build
#
# Same source, one file swapped: manifest.chrome.json becomes the manifest, and
# the Firefox one is dropped. Left unpacked as well as zipped because Chrome
# and Edge load an *unzipped folder* in developer mode, which is how this gets
# used before a store listing exists, while the zip is what the Edge Add-ons
# and Chrome Web Store dashboards want uploaded.
Say "Packaging the extension for Chrome and Edge"
$ChromeDir = "$Repo\target\mdm-chrome"
Reset-Dir $ChromeDir
Copy-Item "$Repo\extension\*" $ChromeDir -Recurse -Force
Remove-Tree "$ChromeDir\test"
Move-Item "$ChromeDir\manifest.chrome.json" "$ChromeDir\manifest.json" -Force
# Staging this folder wrong is silent and expensive, so it is checked rather
# than assumed. The folder still loads with the Firefox manifest in it; what
# breaks is downstream and mute -- Chromium derives the id from the path when
# no key is pinned, the native host allows exactly one id and refuses every
# other, and the popup then sits on "Checking..." for good with nothing said
# anywhere about why. It is also what the installer bundles, so a bad staging
# ships. Cheaper to stop here.
if (-not (Test-Path "$ChromeDir\manifest.json")) {
    Die "staging $ChromeDir produced no manifest.json"
}
if (-not (Select-String -Path "$ChromeDir\manifest.json" -Pattern '"key"' -Quiet)) {
    Die "$ChromeDir\manifest.json carries no pinned key, so it is the Firefox
  manifest rather than the Chromium one. A browser would derive an id from the
  path and the native host would refuse it."
}
$ChromeZip = "$Repo\target\mdm-chrome.zip"
New-Zip $ChromeDir $ChromeZip

# Install the Chromium folder onto local disk, and load it from there.
#
# Two reasons, and the second is the one that actually bites.
#
# First, installer.nsh copies this folder to %APPDATA%\mdm\mdm-chrome, so that
# is where it lives once anyone has run a setup.exe. Writing it here too keeps
# the installed copy in step with a rebuild instead of leaving the browser on
# whatever the last installer carried -- the id is pinned in both, so a stale
# copy loads, connects, and works, and there is no symptom at all beyond fixes
# that appear to do nothing however often the extension is reloaded.
#
# Second: a browser cannot load an unpacked extension from a network path with
# any reliability. Chromium watches the extension directory and reads its files
# lazily, and this repo is routinely opened over an RDP redirected drive, which
# refuses ReadDirectoryChangesW outright (error 50) along with much else. The
# service worker then fails to start, which shows up as an extension that is
# present and enabled and does nothing: no capture, "MDM unavailable" in the
# popup, and every symbol undefined in its console.
#
# So target\mdm-chrome stays the build output -- it is what the .zip and the
# installer are made from -- and this copy on local disk is the one to load.
if (-not $BundleOnly) {
    $InstalledChromeDir = "$NmDir\mdm-chrome"
    Say "Installing the Chromium extension to $InstalledChromeDir"
    Reset-Dir $InstalledChromeDir
    Copy-Item "$ChromeDir\*" $InstalledChromeDir -Recurse -Force
    if (-not (Test-Path "$InstalledChromeDir\manifest.json")) {
        Die "could not install the Chromium extension to $InstalledChromeDir"
    }
} else {
    $InstalledChromeDir = $ChromeDir
}

# Where the signed package lives, if one has been fetched back from AMO.
#
# Out here rather than beside its first use below, because the installer
# section further down reads it too -- and that section runs under -BundleOnly
# while the block below does not. Assigned inside that block, a -BundleOnly
# build reached its Test-Path with nothing to test, which errored and then
# warned that the signed extension was missing while it sat in packaging\.
$SignedXpi = if ($env:MDM_XPI) { $env:MDM_XPI } else { "$Repo\packaging\mdm-firefox-signed.xpi" }

if (-not $BundleOnly) {

# Firefox installs nothing Mozilla has not signed, and signing happens at AMO
# rather than here. The package built above is what to submit, and whatever
# comes back signed is what to install: leave it at SIGNED_XPI (or point
# MDM_XPI at it) and every run from then on offers the one-click install below
# instead of an add-on that vanishes at the next restart.
#
# The signature is also what tells the two apart: a signed .xpi carries
# META-INF/mozilla.rsa, and a zip keeps its member names uncompressed, so the
# name can be found in the raw bytes without unpacking it.
$signed = $false
if (Test-Path $SignedXpi) {
    $bytes = [System.IO.File]::ReadAllBytes($SignedXpi)
    $text = [System.Text.Encoding]::GetEncoding("ISO-8859-1").GetString($bytes)
    if ($text.Contains("META-INF/mozilla.rsa")) {
        $signed = $true
    } elseif ($env:MDM_XPI) {
        Warn ("MDM_XPI=$SignedXpi is missing a signature. Firefox would refuse it, " +
              "so the temporary add-on is what gets offered below instead.")
    }
}

if ($signed) {
    $InstalledXpi = "$env:APPDATA\mdm\mdm-firefox.xpi"
    Copy-Item $SignedXpi $InstalledXpi -Force
    $extLine = "  Extension     $InstalledXpi"
    $extHelp = @"
Install the extension in Firefox -- open this and click "Add":

  file:///$($InstalledXpi -replace '\\','/')

It stays installed across restarts, and it keeps the id the host manifest
above allows, so the app and the browser find each other straight away.
"@
} else {
    $extLine = "  Extension     $Xpi"
    $extHelp = @"
Load the extension in Firefox:

  1. Open  about:debugging#/runtime/this-firefox
  2. Click "Load Temporary Add-on..."
  3. Select  $Repo\extension\manifest.json

Temporary add-ons are removed when Firefox restarts. For one that stays,
Firefox wants a signed package: submit $Xpi to addons.mozilla.org
(self-distribution signing is free and unlisted), save what comes back as
packaging\mdm-firefox-signed.xpi, and re-run this script -- it installs that
in one click. Firefox Developer Edition with
xpinstall.signatures.required=false takes the unsigned package as it is.
"@
}

Write-Host ""
Say "Installed"
Write-Host ""
Write-Host "  App           $BinDir\mdm.exe"
Write-Host "  Native host   $BinDir\mdm-host.exe"
Write-Host "  Host manifest $ManifestPath"
Write-Host "  Chrome/Edge   $ChromeManifestPath"
Write-Host "$extLine"
Write-Host "  Chromium      $InstalledChromeDir"
Write-Host ""
Write-Host $extHelp
Write-Host ""
Write-Host @"
Load the extension in Chrome, Edge, Brave or Vivaldi:

  1. Open  chrome://extensions  (edge://extensions on Edge)
  2. Turn on "Developer mode"
  3. Click "Load unpacked" and select
     $InstalledChromeDir

The id is pinned, so it stays $ChromeId however it is
loaded -- the native host is already registered to accept exactly that one,
and it survives reloads and re-installs. For an entry that needs no developer
mode, upload $ChromeZip to the Edge Add-ons
dashboard (free) or the Chrome Web Store (`$`5 one-off); the pinned key keeps the
id the same, so the registration above keeps working.
"@
Write-Host ""
Write-Host "Then start the app:  $BinDir\mdm.exe"
Write-Host "The extension launches it automatically on the first captured download."
if (-not $Installer) {
    Write-Host ""
    Write-Host "To build a redistributable setup.exe as well, re-run as:"
    Write-Host "  .\install.ps1 -Installer"
}


} else {
    Write-Host ""
    Say "Packaged -- nothing was installed on this machine"
}
# ------------------------------------------------------- redistributable

if ($Installer) {
    Write-Host ""
    Say "Building the redistributable installer"

    if (-not (Get-Command cargo-tauri -ErrorAction SilentlyContinue)) {
        Die "the Tauri CLI was not found. Install it with: cargo install tauri-cli --locked"
    }

    # The updater's signing key, as the bundler wants it: the key *contents* in
    # TAURI_SIGNING_PRIVATE_KEY, not a path. tauri.conf.json carries the public
    # half, and a bundle built without the private half fails outright rather
    # than quietly shipping an update nobody can verify -- so this reads the
    # file if it is there, and says plainly what is missing if it is not.
    $UpdaterKey = "$Repo\packaging\mdm-updater.key"
    if (-not $env:TAURI_SIGNING_PRIVATE_KEY) {
        if (Test-Path $UpdaterKey) {
            $env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content $UpdaterKey -Raw).Trim()

            # The passphrase, from the file beside the key.
            #
            # The key has one at all because of a Windows limitation, not a
            # security choice: Windows cannot hold an *empty* environment
            # variable -- setting one to "" deletes it -- so a key generated
            # with no passphrase can never have that fact communicated to the
            # bundler from PowerShell. It then decides to ask interactively and
            # blocks forever on a prompt no scripted build answers, *after*
            # writing the .exe, which reads as a slow build rather than a stuck
            # one. A real passphrase in a real variable has none of that
            # ambiguity.
            $UpdaterPassword = "$Repo\packaging\mdm-updater.password"
            if (-not $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) {
                if (Test-Path $UpdaterPassword) {
                    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD =
                        (Get-Content $UpdaterPassword -Raw).Trim()
                } else {
                    Die "packaging\mdm-updater.password is missing. It holds the passphrase
  for packaging\mdm-updater.key, and the bundler cannot decrypt the key without
  it -- it will stop on a password prompt and hang. Restore it from your backup,
  or generate a fresh key pair:
    openssl rand -base64 24 > packaging\mdm-updater.password
    cargo tauri signer generate --ci -p (Get-Content packaging\mdm-updater.password) ``
      -w packaging\mdm-updater.key -f
  then paste packaging\mdm-updater.key.pub into tauri.conf.json as
  plugins.updater.pubkey. A new pair orphans every copy already installed."
                }
            }
        } else {
            Die "packaging\mdm-updater.key is missing, and tauri.conf.json names its
  public half -- so the bundler will refuse to build. Either restore the key
  from your backup, or generate a new pair with
    cargo tauri signer generate --ci -w packaging\mdm-updater.key
  and paste the new public key into tauri.conf.json.
  --ci matters: without it the generator puts a passphrase on the key, and the
  bundler then stops on an interactive password prompt a scripted build cannot
  answer -- it does not fail, it hangs.
  Note that a new pair orphans every copy already installed: they verify
  updates against the old public key and refuse anything signed by the new one."
        }
    }

    # Tauri copies a sidecar by looking for `<name>-<target triple>.exe` and
    # installs it beside the app as `<name>.exe`. The triple has to come from
    # the toolchain rather than be assumed: a machine building for aarch64
    # would otherwise ship an x86 host binary that silently never starts.
    $triple = (rustc -vV | Select-String '^host: (.+)$').Matches[0].Groups[1].Value
    $sidecarDir = "$Repo\src-tauri\binaries"
    New-Item -ItemType Directory -Force -Path $sidecarDir | Out-Null
    Copy-Item "$Target\release\mdm-host.exe" "$sidecarDir\mdm-host-$triple.exe" -Force
    Say "Staged the native host as mdm-host-$triple.exe"

    # The installer carries the *signed* extension. Without it the bundle would
    # ship an add-on Firefox refuses, which is worse than shipping none.
    if (-not (Test-Path $SignedXpi)) {
        Warn ("packaging\mdm-firefox-signed.xpi is missing, so the installer will " +
              "carry no extension. Sign the package at addons.mozilla.org and save " +
              "it there, then build again.")
    }

    Push-Location $Repo
    try {
        # The overlay carries the sidecar, the extension and the NSIS hooks.
        #
        # They are deliberately not in tauri.conf.json: `externalBin` is
        # checked on *every* build, and the binary it names is produced by that
        # same build, so putting it in the base config makes a plain
        # `cargo build` fail on any tree where it has not been staged — which
        # is every clean checkout. Bundling is the only thing that needs them.
        cargo tauri build --bundles nsis --config src-tauri/tauri.bundle.conf.json
        if ($LASTEXITCODE -ne 0) { Die "cargo tauri build failed" }
    } finally {
        Pop-Location
    }

    $setup = Get-ChildItem "$Target\release\bundle\nsis\*-setup.exe" `
        -ErrorAction SilentlyContinue | Sort-Object LastWriteTime | Select-Object -Last 1
    Write-Host ""
    if ($setup) {
        Say "Installer built"
        Write-Host "  $($setup.FullName)"
        Write-Host "  $([math]::Round($setup.Length / 1MB, 1)) MB"
        Write-Host ""
        Write-Host "It installs the app and the native host, registers both with"
        Write-Host "Firefox, and needs no Administrator prompt. yt-dlp is fetched by"
        Write-Host "the app itself on first run and kept current after that, so there"
        Write-Host "is nothing else to install."
    } else {
        Die "the bundler reported success but produced no setup.exe"
    }
}

if ($signed -and (Get-Command firefox -ErrorAction SilentlyContinue)) {
    $answer = Read-Host "Open Firefox now to install the extension? [Y/n]"
    if ($answer -notmatch "^[Nn]") {
        Start-Process firefox "file:///$($InstalledXpi -replace '\\','/')"
        Say "Firefox is opening the package -- click `"Add`" there to finish."
    }
}

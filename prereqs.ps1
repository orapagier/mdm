#!/usr/bin/env pwsh
#
# Install everything needed to build MDM's Windows installer.
#
#   .\prereqs.ps1                      install the toolchain
#   .\prereqs.ps1 -GenerateSigningKey  also create the updater key pair
#
# Installs, when missing: Visual Studio C++ build tools, Rust (MSVC), the
# Tauri CLI, and the WebView2 runtime. Idempotent -- anything already present
# is left alone. Needs an elevated shell for the machine-wide pieces.
#
[CmdletBinding()]
param(
    [switch]$GenerateSigningKey,
    [switch]$Force
)

$ErrorActionPreference = "Continue"

$Repo = (Resolve-Path (Split-Path -Parent $MyInvocation.MyCommand.Path)).ProviderPath
$UpdaterKey = "$Repo\packaging\mdm-updater.key"
$UpdaterPassword = "$Repo\packaging\mdm-updater.password"
$VcWorkload = "Microsoft.VisualStudio.Workload.VCTools"
$WebView2Id = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

function Say($msg)  { Write-Host "==> $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host " warning: $msg" -ForegroundColor Yellow }
function Die($msg)  { Write-Host " error: $msg" -ForegroundColor Red; exit 1 }

function Test-Program($name) {
    return $null -ne (Get-Command $name -ErrorAction SilentlyContinue)
}

function Refresh-Path {
    $machine = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $user    = [Environment]::GetEnvironmentVariable("Path", "User")
    $env:Path = (@($machine, $user) | Where-Object { $_ }) -join ";"
}

function Test-VcTools {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $false }
    $install = & $vswhere -products "*" -requires "Microsoft.VisualStudio.Component.VC.Tools.x86.x64" -property installationPath 2>$null
    return [bool]$install
}

function Test-WebView2 {
    $roots = @("HKLM:\SOFTWARE\WOW6432Node", "HKLM:\SOFTWARE", "HKCU:\SOFTWARE")
    foreach ($root in $roots) {
        $path = "$root\Microsoft\EdgeUpdate\Clients\$WebView2Id"
        if (Test-Path $path) {
            $pv = (Get-ItemProperty -Path $path -Name "pv" -ErrorAction SilentlyContinue).pv
            if ($pv) { return $true }
        }
    }
    return $false
}

function Test-MsvcHost {
    $hostLine = (& rustc -vV 2>$null | Select-String "^host: (.+)$").Matches
    if (-not $hostLine) { return $false }
    return $hostLine[0].Groups[1].Value -match "windows-msvc$"
}

function Install-Winget($id, $override) {
    $wingetArgs = @(
        "install", "--id", $id, "-e", "--source", "winget",
        "--accept-package-agreements", "--accept-source-agreements"
    )
    if ($override) { $wingetArgs += @("--override", $override) }
    & winget @wingetArgs
    return $LASTEXITCODE
}

$Winget = if (Test-Program winget) { (Get-Command winget).Source } else { $null }
if (-not $Winget) {
    Warn "winget is not available; falling back to direct downloads where possible."
}

Say "Checking the C++ build tools"
if (Test-VcTools) {
    Say "Visual Studio C++ build tools are already installed"
} elseif ($Winget) {
    Say "Installing Visual Studio 2022 Build Tools (this is several GB and can take a while)"
    $code = Install-Winget "Microsoft.VisualStudio.2022.BuildTools" `
        "--wait --passive --add $VcWorkload --includeRecommended"
    if ($code -ne 0) {
        Die "Visual Studio Build Tools install failed (winget exit $code).
  Install manually from https://visualstudio.microsoft.com/visual-cpp-build-tools/
  and select the 'Desktop development with C++' workload."
    }
    Say "Visual Studio C++ build tools installed"
} else {
    Die "the C++ build tools are missing and winget is unavailable.
  Install them from https://visualstudio.microsoft.com/visual-cpp-build-tools/
  and select the 'Desktop development with C++' workload."
}

Say "Checking Rust"
Refresh-Path
if ((Test-Program cargo) -and (Test-Program rustc)) {
    if (Test-MsvcHost) {
        Say "Rust is already installed with the MSVC toolchain"
    } else {
        Warn "Rust is installed but does not target MSVC. Fix it with:
  rustup toolchain install stable-x86_64-pc-windows-msvc
  rustup default stable-x86_64-pc-windows-msvc"
    }
} elseif ($Winget) {
    Say "Installing Rust"
    $code = Install-Winget "Rustlang.Rustup" $null
    if ($code -ne 0) {
        Die "Rust install failed (winget exit $code). Install it from https://rustup.rs"
    }
    Refresh-Path
    $rustup = "$env:USERPROFILE\.cargo\bin\rustup.exe"
    if (Test-Path $rustup) {
        & $rustup toolchain install stable --profile minimal
        & $rustup default stable
    }
    Refresh-Path
    if (-not (Test-Program cargo)) {
        Die "Rust was installed but cargo is still not on PATH. Open a new shell and re-run this script."
    }
} else {
    Say "Installing Rust from rustup.rs"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $rustupInit = Join-Path $env:TEMP "rustup-init.exe"
    Invoke-WebRequest "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe" -OutFile $rustupInit
    & $rustupInit -y --default-toolchain stable --profile minimal
    if ($LASTEXITCODE -ne 0) { Die "Rust install failed" }
    Refresh-Path
    if (-not (Test-Program cargo)) {
        Die "Rust was installed but cargo is still not on PATH. Open a new shell and re-run this script."
    }
}

Say "Checking the Tauri CLI"
& cargo tauri --version *> $null
if ($LASTEXITCODE -eq 0) {
    Say "The Tauri CLI is already installed"
} else {
    Say "Installing the Tauri CLI (compiles for a few minutes)"
    & cargo install tauri-cli --locked
    if ($LASTEXITCODE -ne 0) { Die "cargo install tauri-cli failed" }
}

Say "Checking the WebView2 runtime"
if (Test-WebView2) {
    Say "WebView2 is already installed"
} elseif ($Winget) {
    $code = Install-Winget "Microsoft.EdgeWebView2Runtime" $null
    if ($code -ne 0) {
        Warn "WebView2 install failed; the generated installer fetches it at install time anyway."
    } else {
        Say "WebView2 installed"
    }
} else {
    Warn "WebView2 is missing; the generated installer fetches it at install time anyway."
}

Say "Checking the updater signing key"
if ((Test-Path $UpdaterKey) -and (Test-Path $UpdaterPassword)) {
    Say "The updater key and password are present"
} elseif ((Test-Path $UpdaterKey) -and -not (Test-Path $UpdaterPassword)) {
    Warn "packaging\mdm-updater.password is missing. Restore it, or the bundle will hang on a password prompt."
} elseif ($GenerateSigningKey) {
    if ((Test-Path $UpdaterKey) -and -not $Force) {
        Warn "packaging\mdm-updater.key already exists; pass -Force to replace it."
    } else {
        $chars = [char[]]((48..57) + (65..90) + (97..122) | ForEach-Object { [char]$_ })
        $password = -join (1..32 | ForEach-Object { $chars | Get-Random })
        Set-Content -LiteralPath $UpdaterPassword -Value $password -NoNewline -Encoding ASCII
        & cargo tauri signer generate --ci -p $password -w $UpdaterKey -f
        if ($LASTEXITCODE -ne 0) { Die "cargo tauri signer generate failed" }
        Warn ("A new key pair was written. Paste packaging\mdm-updater.key.pub into " +
              "tauri.conf.json as plugins.updater.pubkey. Copies already installed " +
              "verify updates against the old key and will refuse this one.")
    }
} else {
    Warn ("packaging\mdm-updater.key is missing, so the bundler will refuse to build. " +
          "Restore it from backup, or re-run with -GenerateSigningKey to create a new pair.")
}

Write-Host ""
Say "Done"
Write-Host "Open a new shell so cargo is on PATH, then build the installer:"
Write-Host "  .\install.ps1 -BundleOnly"

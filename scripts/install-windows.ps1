# Windows-side installer for network-deck.
#
# What this does:
#   1. Downloads + installs usbip-win2 if not already installed.
#   2. Copies the prebuilt client-win.exe into %LOCALAPPDATA%\NetworkDeck.
#   3. The tray app self-registers HKCU\...\Run on first launch — no
#      registry writes here.
#
# Run from an admin PowerShell so the usbip-win2 driver install can
# accept a signed-driver dialog.

[CmdletBinding()]
param(
    [string]$ReleaseUrl = "https://github.com/vadimgrn/usbip-win2/releases/download/v.0.9.7.7/USBip-0.9.7.7-x64.exe",
    [string]$BinarySource = (Join-Path $PSScriptRoot "..\target\release\client-win.exe")
)

$ErrorActionPreference = "Stop"

function Require-Admin {
    $current = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object System.Security.Principal.WindowsPrincipal($current)
    if (-not $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run from an elevated PowerShell."
    }
}

Require-Admin

$usbipExe = "C:\Program Files\USBip\usbip.exe"
if (-not (Test-Path $usbipExe)) {
    Write-Host ">> Downloading usbip-win2 installer..."
    $tmp = Join-Path $env:TEMP "usbip-installer.exe"
    Invoke-WebRequest $ReleaseUrl -OutFile $tmp
    Write-Host ">> Running usbip-win2 installer (accept any driver-signature prompt)..."
    Start-Process $tmp -Wait
    if (-not (Test-Path $usbipExe)) {
        throw "usbip-win2 install did not place usbip.exe at the expected path."
    }
} else {
    Write-Host "usbip-win2 already installed."
}

if (-not (Test-Path $BinarySource)) {
    throw "Build client-win first: cargo build --release -p client-win"
}

$installDir = Join-Path $env:LOCALAPPDATA "NetworkDeck"
$null = New-Item -ItemType Directory -Path $installDir -Force
Write-Host ">> Copying client-win.exe to $installDir"
Copy-Item $BinarySource (Join-Path $installDir "client-win.exe") -Force

Write-Host ""
Write-Host "Done. Next steps:"
Write-Host "  1. & '$installDir\client-win.exe' pair    # one-shot pairing"
Write-Host "  2. & '$installDir\client-win.exe'         # tray app"
Write-Host "     (autostart entry registers itself on first run)"

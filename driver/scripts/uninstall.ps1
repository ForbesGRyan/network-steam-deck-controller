# Uninstall NetworkDeckController.
#
# What this does:
#   1. Removes the root-enumerated device via devcon (stops the running
#      .sys and tears down the virtual USB device).
#   2. Looks up the OEM driver package id (oemNN.inf) and removes it via
#      pnputil so it no longer ships with the system.
#
# Requires: Administrator shell.

[CmdletBinding()]
param(
    [string]$HardwareId = "root\NetworkDeckController",
    [string]$ProviderName = "ForbesGRyan"
)

$ErrorActionPreference = "Stop"

function Require-Admin {
    $current = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object System.Security.Principal.WindowsPrincipal($current)
    if (-not $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run from an elevated shell. (Open PowerShell as Administrator.)"
    }
}

function Find-Devcon {
    $devcon = Get-Command devcon.exe -ErrorAction SilentlyContinue
    if ($devcon) { return $devcon.Source }
    $wdkRoot = "C:\Program Files (x86)\Windows Kits\10\Tools"
    if (Test-Path $wdkRoot) {
        $candidate = Get-ChildItem -Path $wdkRoot -Recurse -Filter devcon.exe -ErrorAction SilentlyContinue |
                     Where-Object { $_.FullName -match "x64" } |
                     Sort-Object FullName -Descending |
                     Select-Object -First 1
        if ($candidate) { return $candidate.FullName }
    }
    throw "devcon.exe not found. Install WDK or add devcon to PATH."
}

Require-Admin

$devcon = Find-Devcon
Write-Host "Removing PnP node ($HardwareId)"
& $devcon remove $HardwareId
if ($LASTEXITCODE -ne 0) {
    Write-Warning "devcon remove returned $LASTEXITCODE (device may already be gone)"
}

# Find the staged oem*.inf for our driver. pnputil enumerates by provider
# in the modern output; we grep for our package's signature line.
Write-Host "Locating staged driver package"
$enum = pnputil /enum-drivers
$publishedName = $null
$lines = $enum -split "`r?`n"
for ($i = 0; $i -lt $lines.Length; $i++) {
    if ($lines[$i] -match "Provider Name:\s*$([regex]::Escape($ProviderName))") {
        # Walk back to find the most recent "Published Name" key.
        for ($j = $i; $j -ge 0; $j--) {
            if ($lines[$j] -match "Published Name:\s*(oem\d+\.inf)") {
                $publishedName = $matches[1]
                break
            }
        }
        if ($publishedName) { break }
    }
}

if ($publishedName) {
    Write-Host "Removing driver package $publishedName"
    pnputil /delete-driver $publishedName /uninstall /force
    if ($LASTEXITCODE -ne 0) { Write-Warning "pnputil delete-driver returned $LASTEXITCODE" }
} else {
    Write-Warning "Could not locate staged driver for provider '$ProviderName'. Run 'pnputil /enum-drivers' and remove the matching oem*.inf manually."
}

Write-Host "Done."

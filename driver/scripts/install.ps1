# Install NetworkDeckController on this host.
#
# What this does:
#   1. Stages the driver package via pnputil (signs the WDK store).
#   2. Creates the root-enumerated PnP node via devcon, so Windows actually
#      starts the .sys and presents a virtual Steam Deck Controller.
#
# Requires:
#   - Test signing on (`bcdedit /set testsigning on`) until you have a real
#     EV cert / Microsoft Partner Center attestation signature.
#   - Administrator shell.
#   - The .sys / .cat / .inf in $InfDir (default: this script's dir parent
#     output\bin\$Configuration\$Platform\NetworkDeckController, the layout
#     `inf2cat` and the VS WDK build produces).
#   - devcon.exe on PATH or under the WDK Tools dir.
#
# Run:
#   pwsh -ExecutionPolicy Bypass -File driver\scripts\install.ps1
#   pwsh -ExecutionPolicy Bypass -File driver\scripts\install.ps1 -InfDir C:\path\to\driver-pkg
[CmdletBinding()]
param(
    [string]$InfDir = (Join-Path $PSScriptRoot "..\NetworkDeckController\x64\Debug\NetworkDeckController"),
    [string]$HardwareId = "root\NetworkDeckController"
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

    # WDK ships devcon under: Windows Kits\10\Tools\<sdk-ver>\x64\devcon.exe
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

$inf = Join-Path $InfDir "NetworkDeckController.inf"
if (-not (Test-Path $inf)) {
    throw "INF not found at: $inf  (build the driver project first, or pass -InfDir)"
}

Write-Host "Staging driver package: $inf"
pnputil /add-driver $inf /install
if ($LASTEXITCODE -ne 0) { throw "pnputil failed with exit code $LASTEXITCODE" }

$devcon = Find-Devcon
Write-Host "Creating PnP node ($HardwareId) via $devcon"
& $devcon install $inf $HardwareId
if ($LASTEXITCODE -ne 0) { throw "devcon install failed with exit code $LASTEXITCODE" }

Write-Host ""
Write-Host "Installed. Verify with:"
Write-Host "  Get-PnpDevice -FriendlyName 'Steam Deck Controller'"
Write-Host "  pnputil /enum-devices /class System | Select-String NetworkDeck"

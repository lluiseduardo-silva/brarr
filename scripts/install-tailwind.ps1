# Download the Tailwind v4 standalone binary into .\tools\tailwindcss.exe.
# Idempotent: re-runs are no-ops once the binary exists.

$ErrorActionPreference = 'Stop'

$Version = if ($env:TAILWIND_VERSION) { $env:TAILWIND_VERSION } else { 'v4.1.16' }
$Repo    = 'https://github.com/tailwindlabs/tailwindcss/releases/download'

$Root      = (Resolve-Path -Path (Join-Path $PSScriptRoot '..')).Path
$ToolsDir  = Join-Path $Root 'tools'
$null      = New-Item -ItemType Directory -Force -Path $ToolsDir
$Dest      = Join-Path $ToolsDir 'tailwindcss.exe'

if (Test-Path $Dest) {
    Write-Host "Tailwind binary already present at $Dest."
    & $Dest --help | Out-Null
    exit 0
}

# Resolve the target asset for the current architecture.
$Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
$Target = switch ($Arch) {
    'X64'   { 'tailwindcss-windows-x64.exe' }
    'Arm64' { 'tailwindcss-windows-arm64.exe' }
    default { throw "Unsupported architecture: $Arch" }
}

$Url = "$Repo/$Version/$Target"
Write-Host "Downloading Tailwind $Version ($Target) ..."

# Use TLS 1.2+ explicitly — older PowerShell defaults often refuse modern GitHub releases.
[System.Net.ServicePointManager]::SecurityProtocol = [System.Net.SecurityProtocolType]::Tls12

Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing

& $Dest --help | Out-Null
Write-Host "Installed: $Dest"

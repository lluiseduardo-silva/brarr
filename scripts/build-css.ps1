# Compile the orchestrator's Tailwind v4 source into static/app.css.
# Run before `cargo run` whenever you touch styles/input.css or any template.

param(
    [ValidateSet('build', 'watch')]
    [string] $Mode = 'build'
)

$ErrorActionPreference = 'Stop'

$Root  = (Resolve-Path -Path (Join-Path $PSScriptRoot '..')).Path
$Orch  = Join-Path $Root 'crates/brarr-orchestrator'
$Bin   = Join-Path $Root 'tools/tailwindcss.exe'

if (-not (Test-Path $Bin)) {
    Write-Error "Tailwind binary missing at $Bin. Run scripts\install-tailwind.ps1 first."
    exit 1
}

$Input  = Join-Path $Orch 'styles/input.css'
$Output = Join-Path $Orch 'static/app.css'

switch ($Mode) {
    'build' {
        Write-Host "Building $Output (minified) ..."
        & $Bin --input $Input --output $Output --minify
    }
    'watch' {
        Write-Host "Watching $Input -> $Output ..."
        & $Bin --input $Input --output $Output --watch
    }
}

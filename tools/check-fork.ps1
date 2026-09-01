<#
.SYNOPSIS
    Quality gate for the vendored egui fork at vendor/egui.

.DESCRIPTION
    `cargo clippy --workspace` in the parent repo does NOT deny warnings in an excluded
    path dependency: the `-- -D warnings` arguments only reach the packages cargo selected,
    and vendor/egui is its own workspace (root Cargo.toml `exclude`). Without this script
    the ~3,900 lines we add to the fork are outside the quality gate entirely.

    Runs fmt + clippy + tests for the crates taskman actually ships from the fork. It does
    not lint egui's demo, plot, extras or kittest crates -- we never build those, they carry
    upstream's own lint debt, and failing on it would make every rebase a cleanup project.

.PARAMETER Fix
    Apply `cargo fmt` instead of checking it.

.EXAMPLE
    pwsh tools/check-fork.ps1
#>
[CmdletBinding()]
param(
    [switch]$Fix
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
$forkRoot = Join-Path $repoRoot 'vendor/egui'

if (-not (Test-Path (Join-Path $forkRoot 'Cargo.toml'))) {
    throw "vendored egui not found at $forkRoot -- see vendor/egui/TASKMAN-FORK.md"
}

# The crates taskman ships from the fork. `egui_software` is ours; the rest are upstream's
# but carry our edits. Keep this list in sync with [patch.crates-io] in the root Cargo.toml.
$packages = @(
    'emath', 'ecolor', 'epaint', 'epaint_default_fonts',
    'egui', 'egui-winit', 'egui_glow', 'egui-wgpu', 'eframe'
)
# egui_software only exists from phase 1 onward; include it once it does.
if (Test-Path (Join-Path $forkRoot 'crates/egui_software/Cargo.toml')) {
    $packages += 'egui_software'
}

$pkgArgs = $packages | ForEach-Object { '-p', $_ }

Push-Location $forkRoot
try {
    if ($Fix) {
        Write-Host '==> cargo fmt (fork)' -ForegroundColor Cyan
        cargo fmt @pkgArgs
    } else {
        Write-Host '==> cargo fmt --check (fork)' -ForegroundColor Cyan
        cargo fmt @pkgArgs -- --check
    }
    if ($LASTEXITCODE -ne 0) { throw "fork: cargo fmt failed ($LASTEXITCODE)" }

    # Only our own crate is held to -D warnings. The upstream crates are checked (so a real
    # breakage surfaces) but their pre-existing lint debt is not our gate to pass.
    Write-Host '==> cargo clippy (fork, upstream crates)' -ForegroundColor Cyan
    cargo clippy @pkgArgs --all-targets
    if ($LASTEXITCODE -ne 0) { throw "fork: cargo clippy failed ($LASTEXITCODE)" }

    if ($packages -contains 'egui_software') {
        Write-Host '==> cargo clippy -D warnings (egui_software)' -ForegroundColor Cyan
        cargo clippy -p egui_software --all-targets -- -D warnings
        if ($LASTEXITCODE -ne 0) { throw "fork: egui_software clippy failed ($LASTEXITCODE)" }
    }

    Write-Host '==> cargo test (fork)' -ForegroundColor Cyan
    cargo test @pkgArgs
    if ($LASTEXITCODE -ne 0) { throw "fork: cargo test failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}

Write-Host 'fork gate: OK' -ForegroundColor Green

<#
.SYNOPSIS
    Measure taskman's own CPU cost, per renderer.

.DESCRIPTION
    Runs the release binary hidden to the tray (so nothing steals focus), samples the
    process's total processor time before and after a fixed interval, and reports the
    result in cores and in percent of one core.

    Sampling the process's own `TotalProcessorTime` rather than a performance counter
    keeps the measurement out of the thing being measured: no extra collector process, no
    counter subsystem, and the number is cumulative so a missed sample cannot skew it.

    Two workloads:
      idle    -- the real repaint policy: event driven, a sample tick every ~1 s.
      stress  -- TASKMAN_FPS_PROBE=1, continuous repaint. This is a deliberately
                 unrealistic worst case; the app never does this on its own.

.EXAMPLE
    pwsh tools/measure-cpu.ps1
    pwsh tools/measure-cpu.ps1 -Seconds 60 -Renderers software,wgpu
#>
[CmdletBinding()]
param(
    [int]$Seconds = 20,
    [string[]]$Renderers = @('software', 'wgpu', 'glow'),
    [string[]]$Workloads = @('idle', 'stress')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repoRoot 'target/release/taskman.exe'
if (-not (Test-Path $exe)) {
    throw "$exe not found -- run `python build.py --host-only` first"
}

# Keep the measurement away from the developer's real settings and data.
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) "tm-cpu-$PID"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

$results = @()

foreach ($renderer in $Renderers) {
    foreach ($workload in $Workloads) {
        $env:TASKMAN_DATA_DIR = $tmp
        $env:TASKMAN_CONFIG_DIR = $tmp
        $env:TASKMAN_RENDERER = $renderer
        if ($workload -eq 'stress') { $env:TASKMAN_FPS_PROBE = '1' } else { $env:TASKMAN_FPS_PROBE = $null }

        # `--minimized-to-tray` keeps the window off screen, so the measurement cannot be
        # perturbed by (or perturb) whatever the user is doing.
        $proc = Start-Process -FilePath $exe -ArgumentList '--minimized-to-tray' -PassThru
        try {
            Start-Sleep -Seconds 3   # let startup, font loading and the first frames settle
            $proc.Refresh()
            if ($proc.HasExited) {
                Write-Host ("{0,-9} {1,-7} FAILED TO START" -f $renderer, $workload) -ForegroundColor Red
                continue
            }
            $before = $proc.TotalProcessorTime
            Start-Sleep -Seconds $Seconds
            $proc.Refresh()
            if ($proc.HasExited) {
                Write-Host ("{0,-9} {1,-7} EXITED EARLY" -f $renderer, $workload) -ForegroundColor Red
                continue
            }
            $after = $proc.TotalProcessorTime
            $cores = ($after - $before).TotalSeconds / $Seconds
            $results += [pscustomobject]@{
                Renderer = $renderer
                Workload = $workload
                Cores    = [math]::Round($cores, 4)
                PctOfOne = [math]::Round($cores * 100, 2)
            }
        } finally {
            if (-not $proc.HasExited) { $proc.Kill(); $proc.WaitForExit(5000) | Out-Null }
        }
    }
}

$env:TASKMAN_FPS_PROBE = $null
Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue

Write-Host ''
Write-Host ("CPU cost over {0}s, hidden window" -f $Seconds) -ForegroundColor Cyan
$results | Format-Table -AutoSize

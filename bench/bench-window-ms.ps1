# Measure time from process spawn until the OS reports a visible main window,
# plus working set at that moment. Usage: powershell -File bench-window-ms.ps1 <exe> [args...]
param([string]$Exe, [string]$ExeArgs = "--bench")

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$p = Start-Process -FilePath $Exe -ArgumentList $ExeArgs -PassThru `
    -RedirectStandardOutput "$env:TEMP\bench_out.txt" -RedirectStandardError "$env:TEMP\bench_err.txt"
$deadline = $sw.ElapsedMilliseconds + 30000
while ($sw.ElapsedMilliseconds -lt $deadline) {
    if ($p.HasExited) { break }
    try { $p.Refresh() } catch {}
    if ($p.MainWindowHandle -ne 0) {
        $ms = $sw.ElapsedMilliseconds
        $ws = 0
        try { $ws = [math]::Round($p.WorkingSet64 / 1MB, 1) } catch {}
        Write-Output ("WINDOW_MS=" + $ms)
        Write-Output ("WORKINGSET_MB=" + $ws)
        Start-Sleep -Milliseconds 50
        Get-Content "$env:TEMP\bench_out.txt" | Select-String "PAINT_MS|WINDOW_MS" | ForEach-Object { Write-Output $_ }
        if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
        exit 0
    }
    Start-Sleep -Milliseconds 4
}
Write-Output "TIMEOUT"
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
exit 1

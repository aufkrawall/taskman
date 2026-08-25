# Capture a window screenshot of taskman.exe for a given tab.
# Usage: powershell -NoProfile -File capture.ps1 -Tab processes -Out out.png [-WaitSeconds 6] [-Width 1600] [-Height 900]
param(
    [Parameter(Mandatory=$true)][string]$Tab,
    [Parameter(Mandatory=$true)][string]$Out,
    [int]$WaitSeconds = 6,
    [int]$Width = 1600,
    [int]$Height = 900,
    [string]$Lang = ""
)

$ErrorActionPreference = "Stop"
$exe = Join-Path $PSScriptRoot "..\target\debug\taskman.exe"
$exe = [System.IO.Path]::GetFullPath($exe)

Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WinEnum {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    public struct RECT { public int Left, Top, Right, Bottom; }

    public static List<IntPtr> Found = new List<IntPtr>();
    public static string TargetTitle = "";

    public static List<IntPtr> WindowsOfProcess(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint wpid; GetWindowThreadProcessId(h, out wpid);
            if (wpid == pid) list.Add(h);
            return true;
        }, IntPtr.Zero);
        return list;
    }

    public static string TitleOf(IntPtr h) {
        var sb = new StringBuilder(512);
        GetWindowText(h, sb, 512);
        return sb.ToString();
    }

    public static IntPtr FindByTitle(string title) {
        IntPtr found = IntPtr.Zero;
        long best = -1;
        EnumWindows((h, l) => {
            if (!IsWindowVisible(h)) return true;
            if (TitleOf(h) != title) return true;
            RECT r; GetWindowRect(h, out r);
            long area = (r.Right - r.Left) * (long)(r.Bottom - r.Top);
            if (area > best) { best = area; found = h; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
"@
[WinEnum]::SetProcessDPIAware() | Out-Null

# Pre-seed settings so every tab screenshots at the same window size.
$settingsDir = Join-Path $env:LOCALAPPDATA "taskman"
New-Item -ItemType Directory -Force -Path $settingsDir | Out-Null
$settingsPath = Join-Path $settingsDir "settings.json"
$seed = @{ window_size = @($Width, $Height); theme = "Dark"; update_speed = "High"; graph_seconds = 60; always_on_top = $false }
if ($Lang -ne "") { $seed.language = $Lang }
$seed | ConvertTo-Json | Set-Content -Path $settingsPath -Encoding UTF8

$p = Start-Process -FilePath $exe -ArgumentList "--tab=$Tab" -PassThru -WindowStyle Minimized
$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 150; $i++) {
    Start-Sleep -Milliseconds 200
    if ($p.HasExited) { throw "taskman exited early with code $($p.ExitCode)" }
    $hwnd = [WinEnum]::FindByTitle("Task-Manager")
    if ($hwnd -eq [IntPtr]::Zero) { $hwnd = [WinEnum]::FindByTitle("Task Manager") }
    if ($hwnd -ne [IntPtr]::Zero) { break }
}
if ($hwnd -eq [IntPtr]::Zero) { Stop-Process -Id $p.Id -Force; throw "no window appeared" }

# Hide any leftover console windows of our process (debug build).
Start-Sleep -Milliseconds 300
foreach ($h in [WinEnum]::WindowsOfProcess([uint32]$p.Id)) {
    if ($h -ne $hwnd) { [WinEnum]::ShowWindow($h, 0) | Out-Null }  # SW_HIDE
}

# Let the sampling engine collect history so charts have data.
Start-Sleep -Seconds $WaitSeconds
[WinEnum]::ShowWindow($hwnd, 3) | Out-Null   # SW_SHOWMAXIMIZED
[WinEnum]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 900

$rect = New-Object WinEnum+RECT
[WinEnum]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top
if ($w -lt 100 -or $h -lt 100) { Stop-Process -Id $p.Id -Force; throw "window rect too small: ${w}x${h}" }

Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
# PW_RENDERFULLCONTENT (2): capture GPU-composited window content even if occluded.
[WinEnum]::PrintWindow($hwnd, $hdc, 2) | Out-Null
$g.ReleaseHdc($hdc)
$full = [System.IO.Path]::GetFullPath((Join-Path $PWD $Out))
New-Item -ItemType Directory -Force -Path ([System.IO.Path]::GetDirectoryName($full)) | Out-Null
$bmp.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()

Stop-Process -Id $p.Id -Force
Write-Output "saved $full ($w x $h)"

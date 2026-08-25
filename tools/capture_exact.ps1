# Capture taskman at an exact (non-maximized) size, optionally with a dialog open.
# Usage: powershell -NoProfile -File tools/capture_exact.ps1 -Tab processes -Out out.png -Width 700 -Height 480 [-Dialog settings]
param(
    [Parameter(Mandatory=$true)][string]$Tab,
    [Parameter(Mandatory=$true)][string]$Out,
    [int]$Width = 700,
    [int]$Height = 480,
    [string]$Dialog = "",
    [int]$WaitSeconds = 5,
    [switch]$Maximize
)

$ErrorActionPreference = "Stop"
$exe = Join-Path $PSScriptRoot "..\target\debug\taskman.exe"
$exe = [System.IO.Path]::GetFullPath($exe)

Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WinEnum2 {
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
    public static List<IntPtr> WindowsOfProcess(uint pid) {
        var list = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint wpid; GetWindowThreadProcessId(h, out wpid);
            if (wpid == pid && IsWindowVisible(h)) list.Add(h);
            return true;
        }, IntPtr.Zero);
        return list;
    }
    public static IntPtr MainOfProcess(uint pid) {
        IntPtr found = IntPtr.Zero; long best = -1;
        EnumWindows((h, l) => {
            uint wpid; GetWindowThreadProcessId(h, out wpid);
            if (wpid != pid || !IsWindowVisible(h)) return true;
            var sb = new StringBuilder(512);
            GetWindowText(h, sb, 512);
            string title = sb.ToString();
            if (title != "Task-Manager" && title != "Task Manager") return true;
            RECT r; GetWindowRect(h, out r);
            long area = (r.Right - r.Left) * (long)(r.Bottom - r.Top);
            if (area > best) { best = area; found = h; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
"@
[WinEnum2]::SetProcessDPIAware() | Out-Null

# Isolated settings dir so the user's real config.ini is not touched.
$sandbox = Join-Path $env:TEMP ("tm-shots-" + [guid]::NewGuid().ToString("N").Substring(0,8))
New-Item -ItemType Directory -Force -Path (Join-Path $sandbox "taskman") | Out-Null
$env:LOCALAPPDATA = $sandbox
# Seed the sandbox config with the requested window size.
@"
[general]
theme=dark
update_speed=high
window_size=${Width}x${Height}
"@ | Set-Content -Path (Join-Path $sandbox "taskman\config.ini") -Encoding ASCII

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.Arguments = "--tab=$Tab --size=${Width}x${Height}"
$psi.UseShellExecute = $false
$psi.EnvironmentVariables["LOCALAPPDATA"] = $sandbox
if ($Dialog -ne "") { $psi.EnvironmentVariables["TASKMAN_DIALOG"] = $Dialog }
if ($env:TASKMAN_PERF) { $psi.EnvironmentVariables["TASKMAN_PERF"] = $env:TASKMAN_PERF }
$psi.EnvironmentVariables["TASKMAN_UPDATE"] = "high"
$p = [System.Diagnostics.Process]::Start($psi)

$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 150; $i++) {
    Start-Sleep -Milliseconds 200
    if ($p.HasExited) { throw "taskman exited early with code $($p.ExitCode)" }
    $hwnd = [WinEnum2]::MainOfProcess([uint32]$p.Id)
    if ($hwnd -ne [IntPtr]::Zero) { break }
}
if ($hwnd -eq [IntPtr]::Zero) { $p.Kill(); throw "no window appeared" }

if ($Maximize) {
    [WinEnum2]::ShowWindow($hwnd, 3) | Out-Null   # SW_SHOWMAXIMIZED
} else {
    [WinEnum2]::ShowWindow($hwnd, 5) | Out-Null   # SW_SHOW
}
[WinEnum2]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Seconds $WaitSeconds

$rect = New-Object WinEnum2+RECT
[WinEnum2]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top

Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
[WinEnum2]::PrintWindow($hwnd, $hdc, 2) | Out-Null
$g.ReleaseHdc($hdc)
$full = [System.IO.Path]::GetFullPath((Join-Path $PWD $Out))
New-Item -ItemType Directory -Force -Path ([System.IO.Path]::GetDirectoryName($full)) | Out-Null
$bmp.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()

$p.Kill()
Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
Write-Output "saved $full ($w x $h)"

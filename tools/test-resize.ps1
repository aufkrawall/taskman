# End-to-end test: drag a column separator in taskman and verify it moved.
param(
    [int]$Width = 1600,
    [int]$Height = 900
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$exe = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\target\debug\taskman.exe"))

Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WinDrag {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref POINT p);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);

    public struct RECT { public int Left, Top, Right, Bottom; }
    public struct POINT { public int X, Y; }
    [StructLayout(LayoutKind.Sequential)]
    public struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr extraInfo; }
    [StructLayout(LayoutKind.Sequential)]
    public struct INPUT { public uint type; public MOUSEINPUT mi; }

    public const uint MOVE = 0x0001, LEFTDOWN = 0x0002, LEFTUP = 0x0004;
    public const uint ABSOLUTE = 0x8000;

    public static IntPtr FindByTitle(string title) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((h, l) => {
            if (!IsWindowVisible(h)) return true;
            var sb = new StringBuilder(256);
            GetWindowText(h, sb, 256);
            if (sb.ToString() == title) found = h;
            return true;
        }, IntPtr.Zero);
        return found;
    }

    public static void Mouse(uint flags, int x, int y) {
        var inputs = new INPUT[1];
        inputs[0].type = 0; // INPUT_MOUSE
        inputs[0].mi = new MOUSEINPUT { dx = x, dy = y, dwFlags = flags | ABSOLUTE };
        SendInput(1, inputs, Marshal.SizeOf(typeof(INPUT)));
    }

    // absolute coords are 0..65535 over the virtual screen
    public static void MoveTo(int sx, int sy) {
        int vx = (sx - GetSystemMetrics(76)) * 65535 / (GetSystemMetrics(78) - 1);  // X origin, CX width
        int vy = (sy - GetSystemMetrics(77)) * 65535 / (GetSystemMetrics(79) - 1);  // Y origin, CY height
        Mouse(MOVE, vx, vy);
    }
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
}
"@
[WinDrag]::SetProcessDPIAware() | Out-Null

# fresh settings (no saved column widths)
$settingsPath = Join-Path $env:LOCALAPPDATA "taskman\settings.json"
New-Item -ItemType Directory -Force -Path (Split-Path $settingsPath) | Out-Null
Remove-Item $settingsPath -ErrorAction SilentlyContinue
Remove-Item (Join-Path $env:LOCALAPPDATA "taskman\settings.json.bad") -ErrorAction SilentlyContinue

$p = Start-Process -FilePath $exe -PassThru -WindowStyle Minimized
$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 150; $i++) {
    Start-Sleep -Milliseconds 200
    if ($p.HasExited) { throw "taskman exited early" }
    $hwnd = [WinDrag]::FindByTitle("Task-Manager")
    if ($hwnd -ne [IntPtr]::Zero) { break }
}
if ($hwnd -eq [IntPtr]::Zero) { throw "no window" }
[WinDrag]::ShowWindow($hwnd, 9) | Out-Null   # SW_RESTORE
Start-Sleep -Milliseconds 800
# Pin to a known position and keep on top (HWND_TOPMOST) so the test's clicks land on taskman.
[WinDrag]::SetWindowPos($hwnd, [IntPtr](-1), 40, 40, 0, 0, 0x0001 -bor 0x0010) | Out-Null # SWP_NOSIZE|SWP_NOACTIVATE
Start-Sleep -Milliseconds 400
[WinDrag]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 600

function Save-Shot($path) {
    $r = New-Object WinDrag+RECT
    [WinDrag]::GetWindowRect($hwnd, [ref]$r) | Out-Null
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [WinDrag]::PrintWindow($hwnd, $hdc, 2) | Out-Null
    $g.ReleaseHdc($hdc); $g.Dispose()
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    return ,@($r.Left, $r.Top, $w, $h)
}

$before = Save-Shot "$PSScriptRoot\..\shots\drag-before.png"

# ---- locate a vertical separator stroke in the table header band ----
# Scan the bitmap loaded from disk for vertical lines.
$bmp = New-Object System.Drawing.Bitmap("$PSScriptRoot\..\shots\drag-before.png")
function Find-Separators($bmp) {
    $W = $bmp.Width; $H = $bmp.Height
    # header band: search y from 150..400 for columns where a grayish vertical
    # stroke spans >= 60 px; header separators are ~ (56..50) tall lines.
    $hits = @()
    for ($x = 500; $x -lt $W - 20; $x++) {
        $best = 0; $run = 0; $bestStart = 0
        for ($y = 100; $y -lt [Math]::Min(500, $H); $y++) {
            $c = $bmp.GetPixel($x, $y)
            $isLine = [Math]::Abs($c.R - $c.G) -lt 8 -and [Math]::Abs($c.G - $c.B) -lt 8 -and $c.R -gt 40 -and $c.R -lt 110
            if ($isLine) { if ($run -eq 0) { $start = $y }; $run++ }
            else { if ($run -gt $best) { $best = $run; $bestStart = $start }; $run = 0 }
        }
        if ($run -gt $best) { $best = $run }
        if ($best -ge 70) { $hits += $x }
    }
    # cluster adjacent x
    $out = @(); $prev = -10
    foreach ($h in $hits) { if ($h - $prev -gt 4) { $out += $h }; $prev = $h }
    return ,$out
}
$seps = Find-Separators $bmp
$bmp.Dispose()
Write-Output ("separators found at x: " + ($seps -join ","))
if ($seps.Count -lt 1) { Stop-Process -Id $p.Id -Force; throw "no separator found" }

$dragX = $seps[0] + 1
$r = New-Object WinDrag+RECT
[WinDrag]::GetWindowRect($hwnd, [ref]$r) | Out-Null
$c = New-Object WinDrag+RECT
[WinDrag]::GetClientRect($hwnd, [ref]$c) | Out-Null
$pt = New-Object WinDrag+POINT
[WinDrag]::ClientToScreen($hwnd, [ref]$pt) | Out-Null
# window border offset: client origin relative to window rect
$borderX = $pt.X - $r.Left
$borderY = $pt.Y - $r.Top
# header separator y: middle of the detected stroke; approximate with 190px in window coords
$screenX = $r.Left + $borderX + $dragX
$screenY = $r.Top + $borderY + 210

Write-Output "dragging from screen ($screenX,$screenY) by +180px"
[WinDrag]::MoveTo($screenX, $screenY) | Out-Null
Start-Sleep -Milliseconds 400
$cp = New-Object WinDrag+POINT
[WinDrag]::GetCursorPos([ref]$cp) | Out-Null
Write-Output "cursor now at ($($cp.X),$($cp.Y))"
[WinDrag]::Mouse([WinDrag]::LEFTDOWN, 0, 0) | Out-Null
Start-Sleep -Milliseconds 120
for ($i = 1; $i -le 18; $i++) {
    [WinDrag]::MoveTo($screenX + $i * 10, $screenY) | Out-Null
    Start-Sleep -Milliseconds 25
}
Start-Sleep -Milliseconds 150
[WinDrag]::Mouse([WinDrag]::LEFTUP, 0, 0) | Out-Null
# Nudge the mouse so the app definitely processes frames after the release.
for ($i = 0; $i -lt 6; $i++) {
    [WinDrag]::MoveTo($screenX + 180 + ($i % 2) * 3, $screenY + ($i % 2) * 2) | Out-Null
    Start-Sleep -Milliseconds 120
}
Start-Sleep -Milliseconds 500

$after = Save-Shot "$PSScriptRoot\..\shots\drag-after.png"
$bmp2 = New-Object System.Drawing.Bitmap("$PSScriptRoot\..\shots\drag-after.png")
$seps2 = Find-Separators $bmp2
$bmp2.Dispose()
Write-Output ("separators after drag: " + ($seps2 -join ","))

# Close gracefully (WM_CLOSE) so the app saves settings in on_exit.
Add-Type -Name U32 -Namespace W -MemberDefinition '[DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);'
[W.U32]::PostMessage($hwnd, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null  # WM_CLOSE
for ($i = 0; $i -lt 30 -and -not $p.HasExited; $i++) { Start-Sleep -Milliseconds 200 }
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
# Dragging boundary k resizes column k (whose LEFT edge sits there): that
# boundary stays, the following ones shift. Verify any boundary moved.
$moved = 0
$min = [Math]::Min($seps.Count, $seps2.Count)
for ($i = 0; $i -lt $min; $i++) { $moved = [Math]::Max($moved, [Math]::Abs($seps2[$i] - $seps[$i])) }
if ($moved -gt 60) {
    Write-Output ("RESIZE WORKS: boundaries moved by up to {0}px (before: {1}; after: {2})" -f $moved, ($seps -join ","), ($seps2 -join ","))
    $sw = Get-Content (Join-Path $env:LOCALAPPDATA "taskman\settings.json") -Raw -ErrorAction SilentlyContinue
    if ($sw -match "col_widths") { Write-Output "PERSISTED: settings.json contains col_widths" }
    exit 0
} else {
    Write-Output "RESIZE FAILED: no boundary moved"
    exit 1
}

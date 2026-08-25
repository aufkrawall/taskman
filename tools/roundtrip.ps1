$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
$exe = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\target\debug\taskman.exe"))
Add-Type @"
using System;using System.Text;using System.Runtime.InteropServices;
public class W2 {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int m);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr d, uint f);
    public static IntPtr Find(string t){ IntPtr f=IntPtr.Zero; EnumWindows((h,l)=>{ if(!IsWindowVisible(h)) return true; var sb=new StringBuilder(256); GetWindowText(h,sb,256); if(sb.ToString()==t) f=h; return true; },IntPtr.Zero); return f; }
}
"@
[W2]::SetProcessDPIAware() | Out-Null
$p = Start-Process -FilePath $exe -PassThru -WindowStyle Minimized
$h = [IntPtr]::Zero
for ($i=0; $i -lt 100; $i++) { Start-Sleep -Milliseconds 200; if ($p.HasExited) { throw "exited" }; $h=[W2]::Find("Task-Manager"); if ($h -ne [IntPtr]::Zero) { break } }
[W2]::ShowWindow($h, 9) | Out-Null
Start-Sleep -Milliseconds 800
[W2]::SetWindowPos($h, [IntPtr](-1), 40, 40, 0, 0, 0x1 -bor 0x10) | Out-Null
Start-Sleep -Seconds 3
$r = New-Object "System.Drawing.Bitmap" -ArgumentList 1,1
# capture

$g = $null
$bmp = New-Object System.Drawing.Bitmap(1672,1136)
$gr = [System.Drawing.Graphics]::FromImage($bmp)
$hd = $gr.GetHdc()
[W2]::PrintWindow($h, $hd, 2) | Out-Null
$gr.ReleaseHdc($hd); $gr.Dispose()
$bmp.Save("$PSScriptRoot\..\shots\roundtrip.png", [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Stop-Process -Id $p.Id -Force
Write-Output "captured"

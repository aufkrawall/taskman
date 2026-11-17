//! Window identity helpers (class names, shell-host overlays, cloaking,
//! transparency, frame bounds) shared by occlusion and fullscreen detection.

use super::PixelRect;
use super::zorder::window_band;
use std::sync::OnceLock;
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetClassNameW, GetLayeredWindowAttributes, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, WS_EX_LAYERED,
    WS_EX_TRANSPARENT,
};
use windows::core::PWSTR;

pub(super) fn is_window_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .is_ok();
    ok && cloaked != 0
}

pub(super) fn is_transparent_overlay(hwnd: HWND) -> bool {
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    if (ex_style & WS_EX_LAYERED.0) == 0 {
        return false;
    }
    if (ex_style & WS_EX_TRANSPARENT.0) != 0 {
        return true;
    }
    let mut alpha = 255u8;
    let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
    unsafe { GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)) }.is_ok()
        && (flags & LWA_ALPHA) != LAYERED_WINDOW_ATTRIBUTES_FLAGS(0)
        && alpha == 0
}

pub(super) fn is_below_band(hwnd: HWND, tray_band: Option<u32>) -> bool {
    tray_band.is_some_and(|tb| window_band(hwnd).is_some_and(|wb| wb < tb))
}

pub(super) struct ClassNameBuf {
    pub(super) buf: [u16; 64],
    pub(super) len: usize,
}

impl ClassNameBuf {
    pub(super) fn read(hwnd: HWND) -> Option<Self> {
        let mut buf = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut buf) };
        (len > 0).then_some(Self {
            buf,
            len: len as usize,
        })
    }

    pub(super) fn slice(&self) -> &[u16] {
        &self.buf[..self.len]
    }

    pub(super) fn eq_ascii(&self, s: &str) -> bool {
        self.len == s.len()
            && self
                .slice()
                .iter()
                .zip(s.bytes())
                .all(|(&w, b)| w == u16::from(b))
    }

    pub(super) fn eq_ascii_ignore_case(&self, s: &str) -> bool {
        self.len == s.len()
            && self
                .slice()
                .iter()
                .zip(s.bytes())
                .all(|(&w, b)| u8::try_from(w).is_ok_and(|c| c.eq_ignore_ascii_case(&b)))
    }

    pub(super) fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(self.slice())
    }

    #[cfg(test)]
    pub(super) fn from_ascii(s: &str) -> Self {
        let mut buf = [0u16; 64];
        let len = s.len().min(buf.len());
        for (slot, byte) in buf.iter_mut().zip(s.bytes().take(len)) {
            *slot = u16::from(byte);
        }
        Self { buf, len }
    }
}

pub(super) fn read_window_class(hwnd: HWND) -> Option<String> {
    ClassNameBuf::read(hwnd).map(|c| c.to_string_lossy())
}

pub(super) fn is_desktop_shell_class_buf(cls: &ClassNameBuf) -> bool {
    cls.eq_ascii("Progman")
        || cls.eq_ascii("WorkerW")
        || cls.eq_ascii("Shell_TrayWnd")
        || cls.eq_ascii("Shell_SecondaryTrayWnd")
}

pub(super) fn is_desktop_shell_class(hwnd: HWND) -> bool {
    ClassNameBuf::read(hwnd).is_some_and(|c| is_desktop_shell_class_buf(&c))
}

pub(super) fn is_tooltip_class_buf(cls: &ClassNameBuf) -> bool {
    cls.eq_ascii_ignore_case("tooltips_class32")
}

pub(super) fn is_shell_host_overlay_class_buf(cls: &ClassNameBuf) -> bool {
    cls.eq_ascii("Windows.UI.Core.CoreWindow")
        || cls.eq_ascii("XamlExplorerHostIslandWindow")
        || cls.eq_ascii("MultitaskingViewFrame")
}

pub(super) fn windows_root_dir() -> &'static str {
    static ROOT: OnceLock<String> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut buf = [0u16; 260];
        let len = unsafe { GetSystemWindowsDirectoryW(Some(&mut buf)) } as usize;
        let raw = if len > 0 && len <= buf.len() {
            String::from_utf16_lossy(&buf[..len])
        } else {
            r"C:\Windows".to_string()
        };
        raw.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    })
}

pub(super) fn process_image_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    unsafe {
        let _ = CloseHandle(handle);
    }
    if !ok || len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

pub(super) fn is_shell_host_overlay_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "Windows.UI.Core.CoreWindow" | "XamlExplorerHostIslandWindow" | "MultitaskingViewFrame"
    )
}

pub(super) fn is_shell_host_overlay_identity(
    class_name: &str,
    image_path: &str,
    windows_root: &str,
) -> bool {
    // Windows 11 Start, Search, tray flyouts, and Task View (`Win+Tab`) use
    // `Windows.UI.Core.CoreWindow`, `XamlExplorerHostIslandWindow`, or
    // `MultitaskingViewFrame` hosts backed by DirectComposition canvases whose
    // HWND bounds can span the screen or overlap the taskbar top edge even though
    // the taskbar remains visible and unobscured.
    if !is_shell_host_overlay_class(class_name) {
        return false;
    }
    let root = windows_root
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    let path = image_path.replace('/', "\\").to_ascii_lowercase();
    let Some(rel) = path.strip_prefix(&root) else {
        return false;
    };
    if rel == r"\explorer.exe" || rel == r"\system32\shellhost.exe" {
        return true;
    }
    let Some(app_rel) = rel.strip_prefix(r"\systemapps\") else {
        return false;
    };
    let Some(file) = app_rel.rsplit('\\').next() else {
        return false;
    };
    matches!(
        file,
        "startmenuexperiencehost.exe"
            | "searchhost.exe"
            | "searchapp.exe"
            | "shellexperiencehost.exe"
    )
}

pub(super) fn is_non_occluding_shell_host_with_class(hwnd: HWND, cls: &ClassNameBuf) -> bool {
    if !is_shell_host_overlay_class_buf(cls) {
        return false;
    }
    let mut pid = 0u32;
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    let Some(image_path) = process_image_path(pid) else {
        return false;
    };
    let class_name = cls.to_string_lossy();
    is_shell_host_overlay_identity(&class_name, &image_path, windows_root_dir())
}

pub(super) fn is_non_occluding_shell_host_window(hwnd: HWND) -> bool {
    ClassNameBuf::read(hwnd).is_some_and(|c| is_non_occluding_shell_host_with_class(hwnd, &c))
}

pub(super) fn is_taskbar_flyout_class(cls: Option<&ClassNameBuf>) -> bool {
    cls.is_some_and(|class| {
        class.eq_ascii_ignore_case("Xaml_WindowedPopupClass")
            || class.eq_ascii_ignore_case("TopLevelWindowForOverflowXamlIsland")
    })
}

pub(super) fn visible_frame_rect(hwnd: HWND) -> Option<PixelRect> {
    let mut r = RECT::default();
    if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut r as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .is_ok()
    {
        let pr = PixelRect::from_win32(r);
        if !pr.is_empty() {
            return Some(pr);
        }
    }
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok() {
        let pr = PixelRect::from_win32(r);
        if !pr.is_empty() {
            return Some(pr);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_host_overlay_identity_matches_only_trusted_shell_core_windows() {
        let root = r"C:\Windows";
        for path in [
            r"C:\WINDOWS\SystemApps\Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy\StartMenuExperienceHost.exe",
            r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\SearchHost.exe",
            r"C:\Windows\SystemApps\Microsoft.Windows.Search_cw5n1h2txyewy\SearchApp.exe",
            r"C:\Windows\SystemApps\ShellExperienceHost_cw5n1h2txyewy\ShellExperienceHost.exe",
            r"C:\Windows\System32\ShellHost.exe",
            r"C:\Windows\explorer.exe",
        ] {
            for overlay_class in [
                "Windows.UI.Core.CoreWindow",
                "XamlExplorerHostIslandWindow",
                "MultitaskingViewFrame",
            ] {
                assert!(
                    is_shell_host_overlay_identity(overlay_class, path, root),
                    "expected {overlay_class} overlay exemption for {path}"
                );
            }
            for non_exempt_class in [
                "CabinetWClass",
                "TopLevelWindowForOverflowXamlIsland",
                "Xaml_WindowedPopupClass",
                "#32768",
                "tooltips_class32",
            ] {
                assert!(
                    !is_shell_host_overlay_identity(non_exempt_class, path, root),
                    "{non_exempt_class} inside {path} must still participate in occlusion/fullscreen checks"
                );
            }
        }

        for untrusted in [
            r"C:\Users\Julian\StartMenuExperienceHost.exe",
            r"C:\Temp\explorer.exe",
            r"D:\Windows\explorer.exe",
            r"D:\Windows\SystemApps\Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy\StartMenuExperienceHost.exe",
            r"C:\Windows\System32\notepad.exe",
            r"C:\Windows\SystemApps\Evil_pkg\App.exe",
            r"C:\Windows\SystemApps\Evil_pkg\explorer.exe",
            r"C:\Program Files\WindowsApps\App\Game.exe",
        ] {
            for overlay_class in [
                "Windows.UI.Core.CoreWindow",
                "XamlExplorerHostIslandWindow",
                "MultitaskingViewFrame",
            ] {
                assert!(
                    !is_shell_host_overlay_identity(overlay_class, untrusted, root),
                    "untrusted or non-shell image {untrusted} ({overlay_class}) must not be exempted"
                );
            }
        }
    }
}

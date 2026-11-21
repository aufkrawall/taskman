//! Real per-file icons via the Windows shell (SHGetFileInfo), decoded to
//! RGBA so the GUI can upload them as egui textures. Mirrors what Task
//! Manager shows in its process lists.

use windows::Win32::Graphics::Gdi::BI_RGB;
use windows::Win32::Graphics::Gdi::BITMAP;
use windows::Win32::Graphics::Gdi::BITMAPINFO;
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Graphics::Gdi::CreateCompatibleDC;
use windows::Win32::Graphics::Gdi::DIB_RGB_COLORS;
use windows::Win32::Graphics::Gdi::DeleteDC;
use windows::Win32::Graphics::Gdi::DeleteObject;
use windows::Win32::Graphics::Gdi::GetBitmapBits;
use windows::Win32::Graphics::Gdi::GetDIBits;
use windows::Win32::Graphics::Gdi::GetObjectW;
use windows::Win32::Graphics::Gdi::SelectObject;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::SHFILEINFOW;
use windows::Win32::UI::Shell::SHGFI_FLAGS;
use windows::Win32::UI::Shell::SHGFI_ICON;
use windows::Win32::UI::Shell::SHGFI_USEFILEATTRIBUTES;
use windows::Win32::UI::Shell::SHGetFileInfoW;
use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
use windows::Win32::UI::WindowsAndMessaging::GetIconInfoExW;
use windows::Win32::UI::WindowsAndMessaging::HICON;
use windows::Win32::UI::WindowsAndMessaging::ICONINFOEXW;
use windows::core::PCWSTR;

/// Give the calling thread a COM apartment before asking the shell for icons.
///
/// `SHGetFileInfo` loads icons through the shell's image lists, which need COM
/// on the calling thread; without it extraction intermittently answers with no
/// icon at all — a process row then lost its icon for no visible reason.
/// Initializes once per thread, and a thread that already has an apartment
/// (`RPC_E_CHANGED_MODE`) is left alone.
fn initialize_com() {
    thread_local! {
        static INIT: () = {
            // SAFETY: no interface is created or stored here; the shell only
            // needs an apartment to exist for the duration of the call.
            let _ = unsafe {
                windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                )
            };
        };
    }
    INIT.with(|_| ());
}

/// Decoded icon: converted to straight-alpha RGBA, `w*h*4` bytes.
pub struct IconRgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Extract the per-user 32x32 icon the shell shows for `path` (an executable
/// or .lnk).
///
/// An image without an icon resource of its own (lsass.exe, ntoskrnl.exe, and
/// with them every kernel pseudo-process) yields the shell's generic
/// application icon — the same "no icon" glyph Explorer draws. There is no
/// substitute drawn here: the icon theme is the shell's to decide.
pub fn extract_icon_rgba(path: &str) -> Option<IconRgba> {
    initialize_com();
    let _serial = extract_lock();
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let info = shell_icon_info(&wide, Default::default(), SHGFI_ICON)?;
    icon_rgba(info.hIcon)
}

/// The shell's default application icon: what Explorer shows for an `.exe`
/// that has no icon resource of its own.
///
/// `SHGFI_USEFILEATTRIBUTES` makes the shell answer from the file-type
/// association instead of touching the disk, so no file has to exist — the
/// result is the OS's generic application glyph for the current
/// theme/DPI/icon settings. The GUI paints this wherever a process has no
/// icon it could extract its own.
pub fn default_app_icon_rgba() -> Option<IconRgba> {
    initialize_com();
    let _serial = extract_lock();
    const PROBE: &str = "application.exe";
    let wide: Vec<u16> = PROBE.encode_utf16().chain(std::iter::once(0)).collect();
    let info = shell_icon_info(
        &wide,
        FILE_ATTRIBUTE_NORMAL,
        SHGFI_ICON | SHGFI_USEFILEATTRIBUTES,
    )?;
    icon_rgba(info.hIcon)
}

/// Serializes shell icon extraction process-wide.
///
/// The shell's icon cache is not safe for concurrent extraction: two threads
/// asking at the same time intermittently get "no icon" for a file that has
/// one (reproducible with the icon tests below, gone once the calls are
/// serialized). Extraction is ~ms and runs on one worker thread anyway, so the
/// lock costs nothing it does not already serialize.
fn extract_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `SHGetFileInfoW(..., SHGFI_ICON)` into a `SHFILEINFOW`; the caller owns the
/// returned `hIcon`.
fn shell_icon_info(
    wide_path: &[u16],
    attributes: FILE_FLAGS_AND_ATTRIBUTES,
    flags: SHGFI_FLAGS,
) -> Option<SHFILEINFOW> {
    let mut info = SHFILEINFOW {
        hIcon: Default::default(),
        iIcon: 0,
        dwAttributes: 0,
        szDisplayName: [0; 260],
        szTypeName: [0; 80],
    };
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide_path.as_ptr()),
            attributes,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    (ok != 0 && !info.hIcon.is_invalid()).then_some(info)
}

/// Convert a shell `HICON` to straight-alpha RGBA and release it.
fn icon_rgba(hicon: HICON) -> Option<IconRgba> {
    let result = (|| {
        let mut ii = ICONINFOEXW {
            cbSize: std::mem::size_of::<ICONINFOEXW>() as u32,
            ..Default::default()
        };
        if !unsafe { GetIconInfoExW(hicon, &mut ii) }.as_bool() {
            return None;
        }
        let out = unsafe { bitmap_to_rgba(ii.hbmColor, ii.hbmMask) };
        if !ii.hbmColor.is_invalid() {
            let _ = unsafe { DeleteObject(ii.hbmColor.into()) };
        }
        if !ii.hbmMask.is_invalid() {
            let _ = unsafe { DeleteObject(ii.hbmMask.into()) };
        }
        out
    })();
    let _ = unsafe { DestroyIcon(hicon) };
    result
}

/// Read a 32-bpp copy of `color`, apply the AND `mask` for icons without
/// alpha, and return straight-alpha RGBA.
unsafe fn bitmap_to_rgba(
    color: windows::Win32::Graphics::Gdi::HBITMAP,
    mask: windows::Win32::Graphics::Gdi::HBITMAP,
) -> Option<IconRgba> {
    unsafe {
        if color.is_invalid() {
            return None;
        }
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let old = SelectObject(hdc, color.into());

        // Query dimensions first.
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if GetDIBits(hdc, color, 0, 0, None, &mut bi, DIB_RGB_COLORS) == 0 {
            SelectObject(hdc, old);
            let _ = DeleteDC(hdc);
            return None;
        }
        let (w, h) = (
            bi.bmiHeader.biWidth.max(0) as u32,
            bi.bmiHeader.biHeight.unsigned_abs(),
        );
        if w == 0 || h == 0 || w > 256 || h > 256 {
            SelectObject(hdc, old);
            let _ = DeleteDC(hdc);
            return None;
        }

        // 32-bpp top-down read.
        let mut bi2 = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let ok = GetDIBits(
            hdc,
            color,
            0,
            h,
            Some(pixels.as_mut_ptr().cast()),
            &mut bi2,
            DIB_RGB_COLORS,
        );
        SelectObject(hdc, old);
        let _ = DeleteDC(hdc);
        if ok == 0 {
            return None;
        }

        // Some legacy icons carry garbage/zero alpha; derive coverage from the
        // AND mask in that case.
        let has_alpha = pixels.as_chunks::<4>().0.iter().any(|px| px[3] != 0);

        // Read the mask bitmap (1 bpp) for the fallback path.
        let mut mask_bits: Vec<u8> = Vec::new();
        let mut mask_stride = 0usize;
        if !has_alpha
            && !mask.is_invalid()
            && let Some(stride) = ddb_row_bytes(mask, w, h)
        {
            mask_stride = stride;
            let mdc = CreateCompatibleDC(None);
            let mold = SelectObject(mdc, mask.into());
            mask_bits = vec![0u8; mask_stride * h as usize];
            let got: i32 =
                GetBitmapBits(mask, mask_bits.len() as i32, mask_bits.as_mut_ptr().cast());
            SelectObject(mdc, mold);
            let _ = DeleteDC(mdc);
            if got == 0 {
                mask_bits.clear();
            }
        }

        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for (i, px) in pixels.as_chunks::<4>().0.iter().enumerate() {
            let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
            let (oa, or, og, ob) = if has_alpha {
                (a, r, g, b)
            } else {
                // Opaque unless the AND mask bit is set (transparent).
                let bit = if mask_bits.is_empty() {
                    false
                } else {
                    let y = i / w as usize;
                    let x = i % w as usize;
                    let byte = mask_bits[y * mask_stride + x / 8];
                    (byte >> (7 - (x % 8))) & 1 == 1
                };
                if bit {
                    (0u8, 0u8, 0u8, 0u8)
                } else {
                    (255u8, r, g, b)
                }
            };
            rgba[i * 4] = or;
            rgba[i * 4 + 1] = og;
            rgba[i * 4 + 2] = ob;
            rgba[i * 4 + 3] = oa;
        }
        Some(IconRgba {
            width: w,
            height: h,
            rgba,
        })
    }
}

/// Scan-line length in bytes of a device-dependent `bitmap`, as
/// `GetBitmapBits` lays it out, when the bitmap covers `w`x`h` pixels.
///
/// DDB scan lines are WORD-aligned (`BITMAP::bmWidthBytes` is only promised
/// to be even), unlike the DWORD-aligned rows of a DIB. Assuming DWORD rows
/// sheared the transparency mask of every legacy (alpha-less) icon whose
/// width is an odd multiple of 16, such as 16 or 48 px.
unsafe fn ddb_row_bytes(
    bitmap: windows::Win32::Graphics::Gdi::HBITMAP,
    w: u32,
    h: u32,
) -> Option<usize> {
    let mut bm = BITMAP::default();
    let got = unsafe {
        GetObjectW(
            bitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&raw mut bm).cast()),
        )
    };
    let row = usize::try_from(bm.bmWidthBytes).ok()?;
    (got != 0
        && bm.bmBitsPixel == 1
        && row >= w.div_ceil(8) as usize
        && bm.bmHeight.unsigned_abs() >= h)
        .then_some(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::CreateBitmap;

    /// The OS default application icon exists on a real Windows host and is a
    /// 32x32 RGBA image.
    #[test]
    fn default_app_icon_extracts() {
        let ic = default_app_icon_rgba().expect("the shell always has a default application icon");
        assert_eq!((ic.width, ic.height), (32, 32));
        assert_eq!(ic.rgba.len(), (32 * 32 * 4) as usize);
        assert!(
            ic.rgba.as_chunks::<4>().0.iter().any(|px| px[3] != 0),
            "the default icon is not fully transparent"
        );
    }

    /// An image without an icon resource resolves to exactly that default, so
    /// rows for kernel pseudo-processes (no image path at all) and rows for
    /// icon-less system images show the same OS glyph instead of two
    /// different placeholders.
    #[test]
    fn icon_less_image_matches_the_default_app_icon() {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let image = extract_icon_rgba(&format!(r"{system_root}\System32\lsass.exe"))
            .expect("lsass.exe has no icon resource, the shell returns the generic one");
        let default = default_app_icon_rgba().expect("default icon");
        assert_eq!(
            (image.width, image.height, image.rgba),
            (default.width, default.height, default.rgba),
            "the generic shell icon must be what a path-less row paints"
        );
    }

    /// Pins the layout fact the mask decoder depends on: a monochrome DDB
    /// pads rows to 16 bits, not 32 (48 px -> 6 bytes, not 8).
    #[test]
    fn monochrome_ddb_rows_are_word_aligned() {
        for (w, expected) in [(16u32, 2usize), (32, 4), (48, 6), (20, 4)] {
            let bitmap = unsafe { CreateBitmap(w as i32, w as i32, 1, 1, None) };
            assert!(!bitmap.is_invalid());
            let row = unsafe { ddb_row_bytes(bitmap, w, w) };
            let _ = unsafe { DeleteObject(bitmap.into()) };
            assert_eq!(row, Some(expected), "{w} px mask row");
        }
    }
}

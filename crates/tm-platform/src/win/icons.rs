//! Real per-file icons via the Windows shell (SHGetFileInfo), decoded to
//! RGBA so the GUI can upload them as egui textures. Mirrors what Task
//! Manager shows in its process lists.

use windows::Win32::Graphics::Gdi::BI_RGB;
use windows::Win32::Graphics::Gdi::BITMAPINFO;
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Graphics::Gdi::CreateCompatibleDC;
use windows::Win32::Graphics::Gdi::DIB_RGB_COLORS;
use windows::Win32::Graphics::Gdi::DeleteDC;
use windows::Win32::Graphics::Gdi::DeleteObject;
use windows::Win32::Graphics::Gdi::GetBitmapBits;
use windows::Win32::Graphics::Gdi::GetDIBits;
use windows::Win32::Graphics::Gdi::SelectObject;
use windows::Win32::UI::Shell::SHFILEINFOW;
use windows::Win32::UI::Shell::SHGFI_ICON;
use windows::Win32::UI::Shell::SHGetFileInfoW;
use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
use windows::Win32::UI::WindowsAndMessaging::GetIconInfoExW;
use windows::Win32::UI::WindowsAndMessaging::ICONINFOEXW;
use windows::core::PCWSTR;

/// Decoded icon: converted to straight-alpha RGBA, `w*h*4` bytes.
pub struct IconRgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Extract the per-user 32x32 icon for `path` (an executable or lnk).
pub fn extract_icon_rgba(path: &str) -> Option<IconRgba> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut info = SHFILEINFOW {
            hIcon: Default::default(),
            iIcon: 0,
            dwAttributes: 0,
            szDisplayName: [0; 260],
            szTypeName: [0; 80],
        };
        let ok = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            Default::default(),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON,
        );
        if ok == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let hicon = info.hIcon;
        let result = (|| {
            let mut ii = ICONINFOEXW {
                cbSize: std::mem::size_of::<ICONINFOEXW>() as u32,
                ..Default::default()
            };
            if !GetIconInfoExW(hicon, &mut ii).as_bool() {
                return None;
            }
            let out = bitmap_to_rgba(ii.hbmColor, ii.hbmMask);
            if !ii.hbmColor.is_invalid() {
                let _ = DeleteObject(ii.hbmColor.into());
            }
            if !ii.hbmMask.is_invalid() {
                let _ = DeleteObject(ii.hbmMask.into());
            }
            out
        })();
        let _ = DestroyIcon(hicon);
        result
    }
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
        let mask_stride = (w.div_ceil(32) * 4) as usize;
        if !has_alpha && !mask.is_invalid() {
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

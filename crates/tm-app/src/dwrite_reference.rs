//! Native DirectWrite text rendering, as the ground truth for `text_compare`'s weight tests.
//!
//! Renders a string with the system's own rasterizer and blender into an
//! `IDWriteBitmapRenderTarget` (a GDI DIB in memory, no window, no GPU), so "TaskMan text
//! weighs like native Windows text" is a measured claim rather than a by-eye one.
//!
//! The blend parameters are Windows' documented defaults (gamma 1.8, enhanced contrast 0.5,
//! ClearType level 1, RGB), not this machine's `cttune.exe` values: the harness renders the
//! app with the same defaults (no platform query runs under `cargo test`), and the comparison
//! must not depend on who ran it.

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_RUN,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PIXEL_GEOMETRY_RGB, DWRITE_RENDERING_MODE,
    DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE, DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE,
    DWriteCreateFactory, IDWriteBitmapRenderTarget1, IDWriteFactory, IDWriteFontCollection,
    IDWriteFontFace,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DIBSECTION, DeleteObject, FillRect, GetCurrentObject, GetObjectW, HGDIOBJ,
    OBJ_BITMAP,
};
use windows::core::{BOOL, Interface, PCWSTR, w};

/// How the reference rasterizes.
#[derive(Clone, Copy, Debug)]
pub enum Antialias {
    /// ClearType with the given DirectWrite rendering mode.
    ClearType(DWRITE_RENDERING_MODE),
    /// Grayscale antialiasing with the given rendering mode.
    Grayscale(DWRITE_RENDERING_MODE),
}

/// Render `text` at `em_px` pixels in `family` (falling back to Segoe UI), top-left at
/// `(x, y)` pixels, into a `width`×`height` `0x00RRGGBB` buffer cleared to `bg`.
///
/// Returns `None` when DirectWrite or the face is unavailable, so a caller can fail with
/// the reason instead of silently comparing against nothing.
pub fn render(
    text: &str,
    em_px: f32,
    (x, y): (f32, f32),
    (width, height): (u32, u32),
    bg: [u8; 3],
    fg: [u8; 3],
    antialias: Antialias,
) -> Option<Vec<u32>> {
    let factory: IDWriteFactory =
        unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ok()?;
    let face = system_face(&factory)?;

    let mut font_metrics = DWRITE_FONT_METRICS::default();
    unsafe { face.GetMetrics(&mut font_metrics) };
    let per_unit = em_px / f32::from(font_metrics.designUnitsPerEm);

    let codepoints: Vec<u32> = text.chars().map(u32::from).collect();
    let mut glyphs = vec![0u16; codepoints.len()];
    unsafe {
        face.GetGlyphIndices(
            codepoints.as_ptr(),
            codepoints.len() as u32,
            glyphs.as_mut_ptr(),
        )
    }
    .ok()?;
    let mut metrics = vec![DWRITE_GLYPH_METRICS::default(); glyphs.len()];
    unsafe {
        face.GetDesignGlyphMetrics(
            glyphs.as_ptr(),
            glyphs.len() as u32,
            metrics.as_mut_ptr(),
            false,
        )
    }
    .ok()?;
    let advances: Vec<f32> = metrics
        .iter()
        .map(|m| m.advanceWidth as f32 * per_unit)
        .collect();

    let interop = unsafe { factory.GetGdiInterop() }.ok()?;
    let target = unsafe { interop.CreateBitmapRenderTarget(None, width, height) }.ok()?;
    unsafe { target.SetPixelsPerDip(1.0) }.ok()?;
    let (mode, aa) = match antialias {
        Antialias::ClearType(mode) => (mode, DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE),
        Antialias::Grayscale(mode) => (mode, DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE),
    };
    let target1: IDWriteBitmapRenderTarget1 = target.cast().ok()?;
    unsafe { target1.SetTextAntialiasMode(aa) }.ok()?;

    let dc = unsafe { target.GetMemoryDC() };
    let colorref =
        |[r, g, b]: [u8; 3]| COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16));
    let brush = unsafe { CreateSolidBrush(colorref(bg)) };
    let rect = RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    };
    unsafe {
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }

    let params = unsafe {
        factory.CreateCustomRenderingParams(1.8, 0.5, 1.0, DWRITE_PIXEL_GEOMETRY_RGB, mode)
    }
    .ok()?;
    let ascent = f32::from(font_metrics.ascent) * per_unit;
    let run = DWRITE_GLYPH_RUN {
        fontFace: std::mem::ManuallyDrop::new(Some(face.clone())),
        fontEmSize: em_px,
        glyphCount: glyphs.len() as u32,
        glyphIndices: glyphs.as_ptr(),
        glyphAdvances: advances.as_ptr(),
        glyphOffsets: std::ptr::null(),
        isSideways: BOOL(0),
        bidiLevel: 0,
    };
    let drawn = unsafe {
        target.DrawGlyphRun(
            x,
            y + ascent,
            DWRITE_MEASURING_MODE_NATURAL,
            &run,
            &params,
            colorref(fg),
            None,
        )
    };
    drop(std::mem::ManuallyDrop::into_inner(run.fontFace));
    drawn.ok()?;

    // The render target's bitmap is a 32-bpp top-down DIB selected into its memory DC.
    let bitmap = unsafe { GetCurrentObject(dc, OBJ_BITMAP) };
    let mut dib = DIBSECTION::default();
    let got = unsafe {
        GetObjectW(
            bitmap,
            std::mem::size_of::<DIBSECTION>() as i32,
            Some((&raw mut dib).cast()),
        )
    };
    if got == 0 || dib.dsBm.bmBits.is_null() || dib.dsBm.bmBitsPixel != 32 {
        return None;
    }
    let stride = dib.dsBm.bmWidthBytes as usize;
    let bits = dib.dsBm.bmBits.cast::<u8>();
    let mut out = Vec::with_capacity((width * height) as usize);
    for row in 0..height as usize {
        let line =
            unsafe { std::slice::from_raw_parts(bits.add(row * stride), width as usize * 4) };
        for px in line.as_chunks::<4>().0 {
            // BGRX in memory.
            out.push((u32::from(px[2]) << 16) | (u32::from(px[1]) << 8) | u32::from(px[0]));
        }
    }
    Some(out)
}

/// Segoe UI Variable Text (the `wght 400`, `opsz 10.5` instance TaskMan renders), else
/// Segoe UI.
fn system_face(factory: &IDWriteFactory) -> Option<IDWriteFontFace> {
    let mut collection: Option<IDWriteFontCollection> = None;
    unsafe { factory.GetSystemFontCollection(&mut collection, false) }.ok()?;
    let collection = collection?;
    for family in [w!("Segoe UI Variable Text"), w!("Segoe UI")] {
        if let Some(face) = face_in(&collection, family) {
            return Some(face);
        }
    }
    None
}

fn face_in(collection: &IDWriteFontCollection, family: PCWSTR) -> Option<IDWriteFontFace> {
    let mut index = 0u32;
    let mut exists = BOOL(0);
    unsafe { collection.FindFamilyName(family, &mut index, &mut exists) }.ok()?;
    if !exists.as_bool() {
        return None;
    }
    let family = unsafe { collection.GetFontFamily(index) }.ok()?;
    let font = unsafe {
        family.GetFirstMatchingFont(
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
        )
    }
    .ok()?;
    unsafe { font.CreateFontFace() }.ok()
}

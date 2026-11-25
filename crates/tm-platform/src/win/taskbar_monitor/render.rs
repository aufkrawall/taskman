//! Software Direct2D/DirectWrite rendering of the widget into a premultiplied
//! DIB section, plus the taskbar light/dark contrast probe.

use super::PixelRect;
use crate::taskbar_monitor::{Config, Metrics};
use tm_core::format::{format_taskbar_pct, format_taskbar_rate};
use tm_core::settings::{TaskbarColorMode, TaskbarNetUnit};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1DCRenderTarget,
    ID2D1Factory, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_FEATURE, DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_PIXEL_GEOMETRY_BGR, DWRITE_PIXEL_GEOMETRY_RGB,
    DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS,
    DWRITE_TEXT_RANGE, DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory, IDWriteFactory,
    IDWriteRenderingParams, IDWriteTextFormat, IDWriteTextLayout, IDWriteTypography,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, MONITOR_DEFAULTTOPRIMARY, MonitorFromWindow,
    SelectObject,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::{PCWSTR, w};

pub(super) const MIN_ADAPTIVE_FONT_PT: f32 = 6.5;

#[derive(Clone, Copy, Debug)]
pub(super) struct LayoutBounds {
    pub(super) horizontal: bool,
    pub(super) max_dip_w: f32,
    pub(super) max_dip_h: f32,
}

impl LayoutBounds {
    pub(super) fn from_taskbar(taskbar_rect: PixelRect, dpi: u32) -> Self {
        let scale = dpi.max(96) as f32 / 96.0;
        Self {
            horizontal: taskbar_rect.width() >= taskbar_rect.height(),
            max_dip_w: taskbar_rect.width() as f32 / scale,
            max_dip_h: taskbar_rect.height() as f32 / scale,
        }
    }

    #[cfg(test)]
    pub(super) fn horizontal(max_dip_h: f32) -> Self {
        Self {
            horizontal: true,
            max_dip_w: f32::INFINITY,
            max_dip_h,
        }
    }

    pub(super) fn avail_dip_h(self) -> f32 {
        if self.max_dip_h > 16.0 {
            (self.max_dip_h - 4.0).max(12.0)
        } else {
            (self.max_dip_h - 2.0).max(1.0)
        }
    }
}

pub(super) fn layout_fits_cross_axis(layout: &MeasuredLayout, bounds: LayoutBounds) -> bool {
    if bounds.horizontal {
        layout.content_h + 2.0 <= bounds.avail_dip_h()
    } else {
        layout.dip_w <= (bounds.max_dip_w - 2.0).max(1.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RenderedDisplay {
    pub(super) show_net: bool,
    pub(super) net_sent: Option<String>,
    pub(super) net_recv: Option<String>,
    pub(super) net_sample: String,
    pub(super) cpu_label: Option<String>,
    pub(super) cpu_val: Option<String>,
    pub(super) ram_label: Option<String>,
    pub(super) ram_val: Option<String>,
    pub(super) sys_val_sample: String,
    pub(super) font_family: String,
    pub(super) font_size_pt: f32,
    pub(super) text_rgb: [u8; 3],
    pub(super) light_taskbar: bool,
    pub(super) hovered: bool,
    pub(super) dpi: u32,
    pub(super) compact: bool,
}

pub(super) struct DibSurface {
    pub(super) hdc: HDC,
    pub(super) bitmap: HBITMAP,
    pub(super) old_obj: HGDIOBJ,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) bits: *const u8,
    pub(super) width: i32,
    pub(super) height: i32,
}

impl DibSurface {
    pub(super) fn new(width: i32, height: i32) -> Option<Self> {
        let width = width.max(1);
        let height = height.max(1);
        unsafe {
            let hdc = CreateCompatibleDC(None);
            if hdc.is_invalid() {
                return None;
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    // Negative height -> top-down DIB.
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let created =
                CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
            let Ok(bitmap) = created else {
                // The device context is ours to release, and this exit does not
                // go through `Drop`.
                let _ = DeleteDC(hdc);
                return None;
            };
            if bitmap.is_invalid() || bits.is_null() {
                if !bitmap.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(bitmap.0));
                }
                let _ = DeleteDC(hdc);
                return None;
            }
            let old_obj = SelectObject(hdc, HGDIOBJ(bitmap.0));
            Some(Self {
                hdc,
                bitmap,
                old_obj,
                bits: bits.cast::<u8>(),
                width,
                height,
            })
        }
    }

    #[cfg(test)]
    pub(super) fn pixels(&self) -> &[u8] {
        let len = (self.width.max(0) as usize)
            .saturating_mul(self.height.max(0) as usize)
            .saturating_mul(4);
        unsafe { std::slice::from_raw_parts(self.bits, len) }
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        unsafe {
            if !self.old_obj.is_invalid() {
                let _ = SelectObject(self.hdc, self.old_obj);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            }
            if !self.hdc.is_invalid() {
                let _ = DeleteDC(self.hdc);
            }
        }
    }
}

pub(super) struct CachedFormatLayout {
    pub(super) family: String,
    pub(super) dip_size_bits: u32,
    pub(super) fmt: IDWriteTextFormat,
    pub(super) up: (f32, f32, Option<IDWriteTextLayout>),
    pub(super) down: (f32, f32, Option<IDWriteTextLayout>),
    pub(super) cpu_label: (f32, f32, Option<IDWriteTextLayout>),
    pub(super) c_label: (f32, f32, Option<IDWriteTextLayout>),
    pub(super) ram_label: (f32, f32, Option<IDWriteTextLayout>),
    pub(super) r_label: (f32, f32, Option<IDWriteTextLayout>),
    pub(super) net_sample: Option<(String, f32)>,
    pub(super) sys_val_sample: Option<(String, f32)>,
}

pub(super) struct Renderer {
    pub(super) d2d_factory: ID2D1Factory,
    pub(super) dwrite_factory: IDWriteFactory,
    pub(super) tabular_typography: Option<IDWriteTypography>,
    pub(super) dc_rt: ID2D1DCRenderTarget,
    pub(super) surface: Option<DibSurface>,
    pub(super) format_cache: Vec<CachedFormatLayout>,
    pub(super) rendering_params: Option<(isize, IDWriteRenderingParams)>,
    pub(super) text_brush: Option<([u8; 3], ID2D1SolidColorBrush)>,
    pub(super) hover_brush: Option<(bool, ID2D1SolidColorBrush)>,
}

impl Renderer {
    pub(super) fn new() -> Option<Self> {
        unsafe {
            let d2d_factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let dwrite_factory: IDWriteFactory =
                DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
            let tabular_typography = dwrite_factory.CreateTypography().ok().filter(|typo| {
                typo.AddFontFeature(DWRITE_FONT_FEATURE {
                    nameTag: DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES,
                    parameter: 1,
                })
                .is_ok()
            });
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let dc_rt = d2d_factory.CreateDCRenderTarget(&props).ok()?;
            dc_rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            Some(Self {
                d2d_factory,
                dwrite_factory,
                tabular_typography,
                dc_rt,
                surface: None,
                format_cache: Vec::new(),
                rendering_params: None,
                text_brush: None,
                hover_brush: None,
            })
        }
    }

    pub(super) fn invalidate_rendering_params(&mut self) {
        self.rendering_params = None;
    }

    pub(super) fn create_text_format(
        &self,
        font_family: &str,
        font_size_pt: f32,
    ) -> Option<IDWriteTextFormat> {
        let family = if font_family.trim().is_empty() {
            "Segoe UI"
        } else {
            font_family.trim()
        };
        let wide_family: Vec<u16> = family.encode_utf16().chain([0]).collect();
        let dip_size = (font_size_pt.clamp(MIN_ADAPTIVE_FONT_PT, 18.0)) * (96.0 / 72.0);
        unsafe {
            let fmt = self
                .dwrite_factory
                .CreateTextFormat(
                    PCWSTR(wide_family.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    dip_size,
                    w!(""),
                )
                .ok()?;
            let _ = fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
            Some(fmt)
        }
    }

    pub(super) fn measure_text(
        &self,
        fmt: &IDWriteTextFormat,
        text: &str,
    ) -> (f32, f32, Option<IDWriteTextLayout>) {
        self.measure_text_inner(fmt, text, false)
    }

    pub(super) fn measure_numeric_text(
        &self,
        fmt: &IDWriteTextFormat,
        text: &str,
    ) -> (f32, f32, Option<IDWriteTextLayout>) {
        self.measure_text_inner(fmt, text, true)
    }

    fn measure_text_inner(
        &self,
        fmt: &IDWriteTextFormat,
        text: &str,
        tabular_figures: bool,
    ) -> (f32, f32, Option<IDWriteTextLayout>) {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self
                .dwrite_factory
                .CreateTextLayout(&wide, fmt, 2000.0, 200.0)
            else {
                return (0.0, 0.0, None);
            };
            if tabular_figures && let Some(typography) = &self.tabular_typography {
                let _ = layout.SetTypography(
                    typography,
                    DWRITE_TEXT_RANGE {
                        startPosition: 0,
                        length: wide.len() as u32,
                    },
                );
            }
            let mut metrics = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut metrics).is_err() {
                return (0.0, 0.0, Some(layout));
            }
            (
                metrics.widthIncludingTrailingWhitespace.ceil(),
                metrics.height.ceil(),
                Some(layout),
            )
        }
    }

    pub(super) fn cached_format_index(
        &mut self,
        font_family: &str,
        font_size_pt: f32,
    ) -> Option<usize> {
        let family = if font_family.trim().is_empty() {
            "Segoe UI"
        } else {
            font_family.trim()
        };
        let dip_size = (font_size_pt.clamp(MIN_ADAPTIVE_FONT_PT, 18.0)) * (96.0 / 72.0);
        let dip_size_bits = dip_size.to_bits();
        if let Some(idx) = self
            .format_cache
            .iter()
            .position(|c| c.dip_size_bits == dip_size_bits && c.family == family)
        {
            return Some(idx);
        }

        let fmt = self
            .create_text_format(family, font_size_pt)
            .or_else(|| self.create_text_format("Segoe UI", font_size_pt))?;
        let up = self.measure_text(&fmt, "↑");
        let down = self.measure_text(&fmt, "↓");
        let cpu_label = self.measure_text(&fmt, "CPU");
        let c_label = self.measure_text(&fmt, "C");
        let ram_label = self.measure_text(&fmt, "RAM");
        let r_label = self.measure_text(&fmt, "R");
        if self.format_cache.len() >= 6 {
            self.format_cache.remove(0);
        }
        self.format_cache.push(CachedFormatLayout {
            family: family.to_string(),
            dip_size_bits,
            fmt,
            up,
            down,
            cpu_label,
            c_label,
            ram_label,
            r_label,
            net_sample: None,
            sys_val_sample: None,
        });
        Some(self.format_cache.len() - 1)
    }

    pub(super) fn apply_text_rendering_params(&mut self, hwnd: Option<isize>) {
        let monitor_raw = unsafe {
            MonitorFromWindow(
                HWND(hwnd.unwrap_or(0) as *mut std::ffi::c_void),
                MONITOR_DEFAULTTOPRIMARY,
            )
        }
        .0 as isize;
        if let Some((cached_mon, _)) = &self.rendering_params
            && *cached_mon == monitor_raw
        {
            return;
        }
        let params = crate::win::text_rendering::query(hwnd);
        let geom = if params.bgr {
            DWRITE_PIXEL_GEOMETRY_BGR
        } else {
            DWRITE_PIXEL_GEOMETRY_RGB
        };
        unsafe {
            // UpdateLayeredWindow uses scalar alpha, so D2D grayscale AA must stay active
            // while inheriting the monitor's ClearType gamma/contrast calibration. Natural
            // symmetric rendering matches `CreateTextLayout`'s natural advances at all DPIs.
            if let Ok(rp) = self.dwrite_factory.CreateCustomRenderingParams(
                params.gamma,
                params.contrast,
                0.0,
                geom,
                DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
            ) {
                let rp_base: IDWriteRenderingParams = rp;
                self.dc_rt.SetTextRenderingParams(&rp_base);
                self.rendering_params = Some((monitor_raw, rp_base));
            }
        }
    }

    pub(super) fn measure_layout(
        &mut self,
        display: &RenderedDisplay,
        bounds: LayoutBounds,
    ) -> Option<MeasuredLayout> {
        let mut best = self.measure_layout_at_size(display, display.font_size_pt, bounds)?;
        if layout_fits_cross_axis(&best, bounds) || display.font_size_pt <= MIN_ADAPTIVE_FONT_PT {
            return Some(best);
        }

        // Step down gently in 0.5 pt increments when a short horizontal bar or narrow
        // vertical bar cannot fit the user's configured font size without clipping.
        let mut size =
            ((display.font_size_pt * 2.0).floor() / 2.0 - 0.5).min(display.font_size_pt - 0.25);
        while size >= MIN_ADAPTIVE_FONT_PT {
            if let Some(candidate) = self.measure_layout_at_size(display, size, bounds) {
                let fits = layout_fits_cross_axis(&candidate, bounds);
                best = candidate;
                if fits {
                    return Some(best);
                }
            }
            size -= 0.5;
        }
        if (size + 0.5 - MIN_ADAPTIVE_FONT_PT).abs() > 0.05
            && let Some(candidate) =
                self.measure_layout_at_size(display, MIN_ADAPTIVE_FONT_PT, bounds)
        {
            best = candidate;
        }
        Some(best)
    }

    pub(super) fn measure_layout_at_size(
        &mut self,
        display: &RenderedDisplay,
        font_size_pt: f32,
        bounds: LayoutBounds,
    ) -> Option<MeasuredLayout> {
        let _ = &self.d2d_factory;
        let cache_idx = self.cached_format_index(&display.font_family, font_size_pt)?;
        let fmt = self.format_cache[cache_idx].fmt.clone();

        let has_net = display.show_net && display.net_sent.is_some() && display.net_recv.is_some();
        let has_cpu = display.cpu_label.is_some() && display.cpu_val.is_some();
        let has_ram = display.ram_label.is_some() && display.ram_val.is_some();
        let has_sys = has_cpu || has_ram;
        if !has_net && !has_sys {
            return None;
        }

        let (up_w, arrow_h1, up_layout) = if has_net {
            self.format_cache[cache_idx].up.clone()
        } else {
            (0.0, 0.0, None)
        };
        let (down_w, arrow_h2, down_layout) = if has_net {
            self.format_cache[cache_idx].down.clone()
        } else {
            (0.0, 0.0, None)
        };
        let arrow_w = up_w.max(down_w);

        let (sent_w, line_h1, sent_layout) = match display.net_sent.as_deref() {
            Some(s) if has_net => self.measure_numeric_text(&fmt, s),
            _ => (0.0, 0.0, None),
        };
        let (recv_w, line_h2, recv_layout) = match display.net_recv.as_deref() {
            Some(s) if has_net => self.measure_numeric_text(&fmt, s),
            _ => (0.0, 0.0, None),
        };
        let net_sample_w = if has_net {
            if let Some((cached_s, cached_w)) = &self.format_cache[cache_idx].net_sample
                && cached_s == &display.net_sample
            {
                *cached_w
            } else {
                let (w, _, _) = self.measure_numeric_text(&fmt, &display.net_sample);
                self.format_cache[cache_idx].net_sample = Some((display.net_sample.clone(), w));
                w
            }
        } else {
            0.0
        };
        let net_arrow_gap = if has_net {
            if display.compact { 1.0 } else { 1.5 }
        } else {
            0.0
        };
        let net_val_w = if has_net {
            sent_w.max(recv_w).max(net_sample_w)
        } else {
            0.0
        };
        let net_col_w = if has_net {
            arrow_w + net_arrow_gap + net_val_w
        } else {
            0.0
        };

        let (cpu_lw, cpu_lh, cpu_label_layout) = match display.cpu_label.as_deref() {
            Some("CPU") => self.format_cache[cache_idx].cpu_label.clone(),
            Some("C") => self.format_cache[cache_idx].c_label.clone(),
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (cpu_vw, line_h3, cpu_val_layout) = match display.cpu_val.as_deref() {
            Some(s) => self.measure_numeric_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (ram_lw, ram_lh, ram_label_layout) = match display.ram_label.as_deref() {
            Some("RAM") => self.format_cache[cache_idx].ram_label.clone(),
            Some("R") => self.format_cache[cache_idx].r_label.clone(),
            Some(s) => self.measure_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let (ram_vw, line_h4, ram_val_layout) = match display.ram_val.as_deref() {
            Some(s) => self.measure_numeric_text(&fmt, s),
            None => (0.0, 0.0, None),
        };
        let sys_val_sample_w = if has_sys {
            if let Some((cached_s, cached_w)) = &self.format_cache[cache_idx].sys_val_sample
                && cached_s == &display.sys_val_sample
            {
                *cached_w
            } else {
                let (w, _, _) = self.measure_numeric_text(&fmt, &display.sys_val_sample);
                self.format_cache[cache_idx].sys_val_sample =
                    Some((display.sys_val_sample.clone(), w));
                w
            }
        } else {
            0.0
        };
        let sys_label_w = if has_sys { cpu_lw.max(ram_lw) } else { 0.0 };
        let sys_label_gap = if has_sys {
            if display.compact { 1.5 } else { 2.5 }
        } else {
            0.0
        };
        let sys_val_w = if has_sys {
            cpu_vw.max(ram_vw).max(sys_val_sample_w)
        } else {
            0.0
        };
        let sys_col_w = if has_sys {
            sys_label_w + sys_label_gap + sys_val_w
        } else {
            0.0
        };

        let row_h = line_h1
            .max(line_h2)
            .max(line_h3)
            .max(line_h4)
            .max(arrow_h1)
            .max(arrow_h2)
            .max(cpu_lh)
            .max(ram_lh)
            .max(10.0);
        let avail_h = bounds.avail_dip_h();
        let pad_x = if display.compact { 3.0 } else { 6.0 };
        let col_gap = if has_net && has_sys {
            if display.compact { 4.0 } else { 7.0 }
        } else {
            0.0
        };
        let side_by_side_w = pad_x * 2.0 + net_col_w + col_gap + sys_col_w;
        // On a vertical taskbar, stack Network above CPU/RAM when side-by-side
        // columns exceed the bar's cross-axis width.
        let stacked = !bounds.horizontal
            && has_net
            && has_sys
            && side_by_side_w > (bounds.max_dip_w - 2.0).max(1.0);

        let two_rows = has_net || (has_cpu && has_ram);
        let mut row_gap = if two_rows { 1.0 } else { 0.0 };
        if !stacked && two_rows && row_h * 2.0 + row_gap + 4.0 > avail_h {
            row_gap = 0.0;
        }
        let section_gap = if stacked {
            if display.compact { 2.0 } else { 3.0 }
        } else {
            0.0
        };
        let net_rows_h = if has_net { row_h * 2.0 + row_gap } else { 0.0 };
        let sys_rows_h = if has_cpu && has_ram {
            row_h * 2.0 + row_gap
        } else if has_sys {
            row_h
        } else {
            0.0
        };
        let content_h = if stacked {
            net_rows_h + section_gap + sys_rows_h
        } else if two_rows {
            row_h * 2.0 + row_gap
        } else {
            row_h
        };
        let content_w = if stacked {
            net_col_w.max(sys_col_w)
        } else {
            net_col_w + col_gap + sys_col_w
        };
        let pad_y = ((avail_h - content_h) * 0.5).clamp(1.0, 4.0);
        let dip_w = (pad_x * 2.0 + content_w).ceil();
        let min_h = 22.0_f32.min(avail_h);
        let dip_h = (content_h + pad_y * 2.0).clamp(min_h, avail_h).ceil();

        let dpi_scale = (display.dpi.max(96) as f32) / 96.0;
        let px_w = (dip_w * dpi_scale).round().max(1.0) as i32;
        let px_h = (dip_h * dpi_scale).round().max(1.0) as i32;

        Some(MeasuredLayout {
            px_w,
            px_h,
            dip_w,
            dip_h,
            dpi_scale,
            pad_x,
            row_h,
            row_gap,
            section_gap,
            content_h,
            col_gap,
            stacked,
            has_net,
            has_sys,
            has_cpu,
            has_ram,
            arrow_w,
            net_arrow_gap,
            net_val_w,
            net_col_w,
            sys_label_w,
            sys_label_gap,
            sys_val_w,
            up_layout,
            down_layout,
            sent_layout,
            recv_layout,
            cpu_label_layout,
            cpu_val_layout,
            ram_label_layout,
            ram_val_layout,
        })
    }

    pub(super) fn render_layout(
        &mut self,
        display: &RenderedDisplay,
        layout: MeasuredLayout,
        hwnd: Option<isize>,
    ) -> Option<(i32, i32, HDC)> {
        let px_w = layout.px_w;
        let px_h = layout.px_h;
        let needs_alloc = self
            .surface
            .as_ref()
            .is_none_or(|s| s.width != px_w || s.height != px_h);
        if needs_alloc {
            self.surface = DibSurface::new(px_w, px_h);
        }
        let surface_hdc = self.surface.as_ref()?.hdc;

        let bind_rect = RECT {
            left: 0,
            top: 0,
            right: px_w,
            bottom: px_h,
        };
        let dpi_f = display.dpi.max(96) as f32;
        let snap_dip = |v: f32| -> f32 { (v * layout.dpi_scale).round() / layout.dpi_scale };
        self.apply_text_rendering_params(hwnd);
        unsafe {
            self.dc_rt.SetDpi(dpi_f, dpi_f);
            self.dc_rt.BindDC(surface_hdc, &bind_rect).ok()?;
            self.dc_rt.BeginDraw();

            // 1/255 alpha makes the full widget box hit-testable for hover and
            // clicks while remaining visually indistinguishable from transparent.
            self.dc_rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0 / 255.0,
            }));

            if display.hovered {
                let hover_brush = match &self.hover_brush {
                    Some((light, b)) if *light == display.light_taskbar => Some(b.clone()),
                    _ => {
                        let hover_color = if display.light_taskbar {
                            D2D1_COLOR_F {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 0.06,
                            }
                        } else {
                            D2D1_COLOR_F {
                                r: 1.0,
                                g: 1.0,
                                b: 1.0,
                                a: 0.08,
                            }
                        };
                        self.dc_rt
                            .CreateSolidColorBrush(&hover_color, None)
                            .ok()
                            .inspect(|b| {
                                self.hover_brush = Some((display.light_taskbar, b.clone()));
                            })
                    }
                };
                if let Some(hover_brush) = hover_brush {
                    let rr = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: 0.5,
                            top: 0.5,
                            right: layout.dip_w - 0.5,
                            bottom: layout.dip_h - 0.5,
                        },
                        radiusX: 4.0,
                        radiusY: 4.0,
                    };
                    self.dc_rt.FillRoundedRectangle(&rr, &hover_brush);
                }
            }

            let brush: ID2D1SolidColorBrush = match &self.text_brush {
                Some((rgb, b)) if *rgb == display.text_rgb => b.clone(),
                _ => {
                    let text_color = D2D1_COLOR_F {
                        r: f32::from(display.text_rgb[0]) / 255.0,
                        g: f32::from(display.text_rgb[1]) / 255.0,
                        b: f32::from(display.text_rgb[2]) / 255.0,
                        a: 1.0,
                    };
                    match self.dc_rt.CreateSolidColorBrush(&text_color, None) {
                        Ok(b) => {
                            self.text_brush = Some((display.text_rgb, b.clone()));
                            b
                        }
                        Err(_) => {
                            let _ = self.dc_rt.EndDraw(None, None);
                            self.text_brush = None;
                            self.hover_brush = None;
                            return None;
                        }
                    }
                }
            };

            let draw_text = |tl: Option<IDWriteTextLayout>, x: f32, y: f32, max_w: f32| {
                if let Some(tl) = tl {
                    let _ = tl.SetMaxWidth(max_w);
                    let _ = tl.SetMaxHeight(layout.row_h);
                    let _ = tl.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                    self.dc_rt.DrawTextLayout(
                        windows_numerics::Vector2 { X: x, Y: y },
                        &tl,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            };

            let top_y = snap_dip(((layout.dip_h - layout.content_h) * 0.5).max(0.0));
            let row2_y = snap_dip(top_y + layout.row_h + layout.row_gap);
            let single_y = snap_dip(((layout.dip_h - layout.row_h) * 0.5).max(0.0));

            if layout.has_net {
                let arrow_x = snap_dip(layout.pad_x);
                let val_x = snap_dip(layout.pad_x + layout.arrow_w + layout.net_arrow_gap);
                draw_text(layout.up_layout, arrow_x, top_y, layout.arrow_w);
                draw_text(layout.sent_layout, val_x, top_y, layout.net_val_w);
                draw_text(layout.down_layout, arrow_x, row2_y, layout.arrow_w);
                draw_text(layout.recv_layout, val_x, row2_y, layout.net_val_w);
            }

            if layout.has_sys {
                let label_x = snap_dip(if layout.stacked {
                    layout.pad_x
                } else {
                    layout.pad_x
                        + layout.net_col_w
                        + if layout.has_net { layout.col_gap } else { 0.0 }
                });
                let val_x = snap_dip(label_x + layout.sys_label_w + layout.sys_label_gap);
                let draw_sys_row =
                    |lbl: Option<IDWriteTextLayout>, val: Option<IDWriteTextLayout>, y: f32| {
                        draw_text(lbl, label_x, y, layout.sys_label_w);
                        draw_text(val, val_x, y, layout.sys_val_w);
                    };

                let (sys_y1, sys_y2, sys_single_y) = if layout.stacked {
                    let net_block_h = layout.row_h * 2.0 + layout.row_gap;
                    let sys_top = snap_dip(top_y + net_block_h + layout.section_gap);
                    (
                        sys_top,
                        snap_dip(sys_top + layout.row_h + layout.row_gap),
                        sys_top,
                    )
                } else {
                    (top_y, row2_y, single_y)
                };

                if layout.has_cpu && layout.has_ram {
                    draw_sys_row(layout.cpu_label_layout, layout.cpu_val_layout, sys_y1);
                    draw_sys_row(layout.ram_label_layout, layout.ram_val_layout, sys_y2);
                } else if layout.has_cpu {
                    draw_sys_row(layout.cpu_label_layout, layout.cpu_val_layout, sys_single_y);
                } else if layout.has_ram {
                    draw_sys_row(layout.ram_label_layout, layout.ram_val_layout, sys_single_y);
                }
            }

            if self.dc_rt.EndDraw(None, None).is_err() {
                self.text_brush = None;
                self.hover_brush = None;
                self.rendering_params = None;
                return None;
            }
            Some((px_w, px_h, surface_hdc))
        }
    }

    #[cfg(test)]
    pub(super) fn measure_and_render(
        &mut self,
        display: &RenderedDisplay,
        max_taskbar_dip_h: f32,
        hwnd: Option<isize>,
    ) -> Option<(i32, i32, HDC)> {
        self.measure_and_render_in(display, LayoutBounds::horizontal(max_taskbar_dip_h), hwnd)
    }

    #[cfg(test)]
    pub(super) fn measure_and_render_in(
        &mut self,
        display: &RenderedDisplay,
        bounds: LayoutBounds,
        hwnd: Option<isize>,
    ) -> Option<(i32, i32, HDC)> {
        let layout = self.measure_layout(display, bounds)?;
        self.render_layout(display, layout, hwnd)
    }
}

pub(super) struct MeasuredLayout {
    pub(super) px_w: i32,
    pub(super) px_h: i32,
    pub(super) dip_w: f32,
    pub(super) dip_h: f32,
    pub(super) dpi_scale: f32,
    pub(super) pad_x: f32,
    pub(super) row_h: f32,
    pub(super) row_gap: f32,
    pub(super) section_gap: f32,
    pub(super) content_h: f32,
    pub(super) col_gap: f32,
    pub(super) stacked: bool,
    pub(super) has_net: bool,
    pub(super) has_sys: bool,
    pub(super) has_cpu: bool,
    pub(super) has_ram: bool,
    pub(super) arrow_w: f32,
    pub(super) net_arrow_gap: f32,
    pub(super) net_val_w: f32,
    pub(super) net_col_w: f32,
    pub(super) sys_label_w: f32,
    pub(super) sys_label_gap: f32,
    pub(super) sys_val_w: f32,
    pub(super) up_layout: Option<IDWriteTextLayout>,
    pub(super) down_layout: Option<IDWriteTextLayout>,
    pub(super) sent_layout: Option<IDWriteTextLayout>,
    pub(super) recv_layout: Option<IDWriteTextLayout>,
    pub(super) cpu_label_layout: Option<IDWriteTextLayout>,
    pub(super) cpu_val_layout: Option<IDWriteTextLayout>,
    pub(super) ram_label_layout: Option<IDWriteTextLayout>,
    pub(super) ram_val_layout: Option<IDWriteTextLayout>,
}

pub(super) fn build_display(
    config: &Config,
    metrics: &Metrics,
    light_taskbar: bool,
    hovered: bool,
    dpi: u32,
    compact: bool,
) -> RenderedDisplay {
    let (net_sent, net_recv, net_sample) = if config.show_network {
        let up = format_taskbar_rate(
            metrics.net_sent_bps,
            config.net_unit_bits,
            config.net_unit_mode,
        );
        let down = format_taskbar_rate(
            metrics.net_recv_bps,
            config.net_unit_bits,
            config.net_unit_mode,
        );
        let sample = match (config.net_unit_bits, config.net_unit_mode) {
            (false, TaskbarNetUnit::FixedKB) | (false, TaskbarNetUnit::Auto) => "999.9 KB/s",
            (false, TaskbarNetUnit::FixedMB) => "99.99 MB/s",
            (true, TaskbarNetUnit::FixedKB) | (true, TaskbarNetUnit::Auto) => "999.9 Kbps",
            (true, TaskbarNetUnit::FixedMB) => "99.99 Mbps",
        };
        (Some(up), Some(down), sample.to_string())
    } else {
        (None, None, String::new())
    };

    let cpu_label = config
        .show_cpu
        .then(|| if compact { "C" } else { "CPU" }.to_string());
    let cpu_val = config.show_cpu.then(|| format_taskbar_pct(metrics.cpu_pct));
    let ram_label = config
        .show_ram
        .then(|| if compact { "R" } else { "RAM" }.to_string());
    let ram_val = config.show_ram.then(|| format_taskbar_pct(metrics.ram_pct));

    let text_rgb = match config.color_mode {
        TaskbarColorMode::Auto => {
            if light_taskbar {
                [24, 24, 24]
            } else {
                [255, 255, 255]
            }
        }
        TaskbarColorMode::Custom => config.custom_color,
    };

    RenderedDisplay {
        show_net: config.show_network,
        net_sent,
        net_recv,
        net_sample,
        cpu_label,
        cpu_val,
        ram_label,
        ram_val,
        sys_val_sample: "100%".to_string(),
        font_family: config.font_family.clone(),
        font_size_pt: config.font_size,
        text_rgb,
        light_taskbar,
        hovered,
        dpi,
        compact,
    }
}

pub(super) fn read_hkcu_dword(subkey: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut std::ffi::c_void),
            Some(&mut size),
        )
    };
    rc.is_ok().then_some(data)
}

pub(super) fn is_rgb_light([r, g, b]: [u8; 3]) -> bool {
    // Matches Windows Shell's `IsColorLight` luminance threshold.
    (5 * u32::from(g) + 2 * u32::from(r) + u32::from(b)) > 8 * 128
}

pub(super) fn resolve_light_taskbar(
    system_uses_light: bool,
    color_prevalence: bool,
    accent_rgb: Option<[u8; 3]>,
) -> bool {
    if color_prevalence && let Some(rgb) = accent_rgb {
        return is_rgb_light(rgb);
    }
    system_uses_light
}

pub(super) fn read_taskbar_is_light() -> bool {
    let personalize = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let dwm = w!("Software\\Microsoft\\Windows\\DWM");
    let system_uses_light =
        read_hkcu_dword(personalize, w!("SystemUsesLightTheme")).is_some_and(|v| v != 0);
    let color_prevalence =
        read_hkcu_dword(personalize, w!("ColorPrevalence")).is_some_and(|v| v != 0);
    // `DWM\AccentColor` is stored as 0xAABBGGRR; `ColorizationColor` is 0xAARRGGBB.
    let accent_rgb = read_hkcu_dword(dwm, w!("AccentColor"))
        .map(|raw| [raw as u8, (raw >> 8) as u8, (raw >> 16) as u8])
        .or_else(|| {
            read_hkcu_dword(dwm, w!("ColorizationColor"))
                .map(|raw| [(raw >> 16) as u8, (raw >> 8) as u8, raw as u8])
        });
    resolve_light_taskbar(system_uses_light, color_prevalence, accent_rgb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::taskbar_monitor::placement::solve_taskbar_placement;
    use crate::win::taskbar_monitor::test_support::rect;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

    /// Regression: `CreateDIBSection` failing is the one exit of `DibSurface::new`
    /// that does not go through `Drop`, and it used to return before the device
    /// context was released, leaving one GDI object behind per refusal.
    #[test]
    fn a_refused_dib_section_does_not_leak_its_device_context() {
        use windows::Win32::System::Threading::{
            GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources,
        };

        let gdi_objects = || unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };

        // Control: a live `DibSurface` owns a device context and a bitmap, so
        // the counter must move. Without this the check below would also pass
        // on a build where the query answers nothing at all.
        let baseline = gdi_objects();
        let held = DibSurface::new(8, 8).expect("small DIB section");
        let with_surface = gdi_objects();
        drop(held);
        assert!(
            with_surface > baseline,
            "GetGuiResources(GR_GDIOBJECTS) must count GDI objects"
        );

        // A DIB section larger than any address space is refused by
        // `CreateDIBSection`, which is the exit that leaked. Other tests run in
        // parallel and may hold GDI objects of their own, so the count is
        // allowed to drift; 32 leaks are not drift.
        let before = gdi_objects();
        for _ in 0..32 {
            assert!(
                DibSurface::new(i32::MAX / 2, i32::MAX / 2).is_none(),
                "a DIB section larger than any address space must be refused"
            );
        }
        let leaked = gdi_objects().saturating_sub(before);
        assert!(
            leaked < 16,
            "refusing 32 DIB sections leaked {leaked} GDI object(s)"
        );
    }

    #[test]
    fn rendered_text_pixels_fall_inside_layered_bitmap_bounds() {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut renderer = Renderer::new().expect("Direct2D/DirectWrite renderer");
        let cfg = Config {
            enabled: true,
            show_cpu: true,
            show_ram: true,
            show_network: true,
            ..Config::default()
        };
        let metrics = Metrics {
            cpu_pct: Some(23.0),
            ram_pct: Some(48.0),
            net_sent_bps: Some(12_300.0),
            net_recv_bps: Some(456_700.0),
        };
        let display = build_display(&cfg, &metrics, false, false, 96, false);
        let (w, h, _hdc) = renderer
            .measure_and_render(&display, 48.0, None)
            .expect("rendered surface");
        assert!(w > 40 && h > 20, "unexpected widget size {w}x{h}");

        let pixels = renderer.surface.as_ref().unwrap().pixels();
        // Text color is white [255, 255, 255] while background clear is alpha 1/255.
        // Every quadrant (top-left upload, bottom-left download, top-right CPU,
        // bottom-right RAM) must contain rendered glyph pixels, while background
        // corner pixels remain nearly transparent.
        assert_eq!(
            &pixels[0..4],
            &[0, 0, 0, 1],
            "unexpected non-hovered corner pixel"
        );
        let mut quadrants = [0usize; 4];
        let half_w = (w / 2) as usize;
        let half_h = (h / 2) as usize;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let idx = (y * w as usize + x) * 4;
                if pixels[idx + 3] >= 64 && pixels[idx + 2] >= 64 {
                    let q = usize::from(y >= half_h) * 2 + usize::from(x >= half_w);
                    quadrants[q] += 1;
                }
            }
        }
        assert!(
            quadrants.iter().all(|&c| c >= 20),
            "expected >= 20 glyph pixels in all 4 quadrants of {w}x{h} bitmap, got {quadrants:?}"
        );

        let hi_dpi = build_display(&cfg, &metrics, false, false, 144, false);
        let (hw, hh, _hdc2) = renderer
            .measure_and_render(&hi_dpi, 48.0, None)
            .expect("hi-dpi rendered surface");
        assert!(hw > w && hh > h, "144 DPI should scale widget size");
        let hi_glyph_pixels = renderer
            .surface
            .as_ref()
            .unwrap()
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[3] >= 64 && px[2] >= 64)
            .count();
        assert!(
            hi_glyph_pixels >= 100,
            "expected >= 100 rendered glyph pixels at 144 DPI inside {hw}x{hh} bitmap, got {hi_glyph_pixels}"
        );
        unsafe { CoUninitialize() };
    }

    #[test]
    fn cpu_label_position_stays_fixed_across_digit_width_changes() {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut renderer = Renderer::new().expect("Direct2D/DirectWrite renderer");
        let cfg = Config {
            enabled: true,
            show_cpu: true,
            show_ram: false,
            show_network: true,
            ..Config::default()
        };
        let low_metrics = Metrics {
            cpu_pct: Some(1.0),
            ram_pct: None,
            net_sent_bps: Some(0.0),
            net_recv_bps: Some(0.0),
        };
        let high_metrics = Metrics {
            cpu_pct: Some(100.0),
            ram_pct: None,
            net_sent_bps: Some(0.0),
            net_recv_bps: Some(0.0),
        };

        let (w1, h1, _) = renderer
            .measure_and_render(
                &build_display(&cfg, &low_metrics, false, false, 96, false),
                48.0,
                None,
            )
            .expect("low load render");
        let snap1 = renderer.surface.as_ref().unwrap().pixels().to_vec();

        let (w2, h2, _) = renderer
            .measure_and_render(
                &build_display(&cfg, &high_metrics, false, false, 96, false),
                48.0,
                None,
            )
            .expect("high load render");
        let snap2 = renderer.surface.as_ref().unwrap().pixels().to_vec();

        assert_eq!((w1, h1), (w2, h2));

        // Locate the first lit glyph pixel inside the right-hand CPU column (x >= w/2):
        // that edge belongs to the "CPU" label and must not shift when "1%" becomes "100%".
        let first_cpu_x = |buf: &[u8]| -> usize {
            (w1 as usize / 2..w1 as usize)
                .find(|&x| (0..h1 as usize).any(|y| buf[(y * w1 as usize + x) * 4 + 3] >= 64))
                .expect("CPU label pixels present")
        };
        assert_eq!(first_cpu_x(&snap1), first_cpu_x(&snap2));

        // Ensure "1%" starts right after the "CPU"/"RAM" label slot without the wide
        // right-aligned padding void: total CPU-label-to-1%-end span stays compact.
        let last_cpu_x = (w1 as usize / 2..w1 as usize)
            .rfind(|&x| (0..h1 as usize).any(|y| snap1[(y * w1 as usize + x) * 4 + 3] >= 64))
            .expect("CPU percent pixels present");
        let cpu_span = last_cpu_x - first_cpu_x(&snap1);
        assert!(
            cpu_span <= 42,
            "CPU label + value should stay compact without right-align padding, got {cpu_span}px"
        );
        unsafe { CoUninitialize() };
    }

    #[test]
    fn compact_mode_renders_narrower_bitmap_with_all_quadrants_lit() {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut renderer = Renderer::new().expect("Direct2D/DirectWrite renderer");
        let cfg = Config {
            enabled: true,
            show_cpu: true,
            show_ram: true,
            show_network: true,
            ..Config::default()
        };
        let metrics = Metrics {
            cpu_pct: Some(23.0),
            ram_pct: Some(48.0),
            net_sent_bps: Some(12_300.0),
            net_recv_bps: Some(456_700.0),
        };
        let normal = build_display(&cfg, &metrics, false, false, 96, false);
        let compact = build_display(&cfg, &metrics, false, false, 96, true);
        let (nw, _nh, _) = renderer
            .measure_and_render(&normal, 48.0, None)
            .expect("normal render");
        let (cw, ch, _) = renderer
            .measure_and_render(&compact, 48.0, None)
            .expect("compact render");
        assert!(
            cw < nw,
            "compact widget width ({cw}) must be strictly narrower than normal ({nw})"
        );
        let pixels = renderer.surface.as_ref().unwrap().pixels();
        let mut quadrants = [0usize; 4];
        let half_w = (cw / 2) as usize;
        let half_h = (ch / 2) as usize;
        for y in 0..ch as usize {
            for x in 0..cw as usize {
                let idx = (y * cw as usize + x) * 4;
                if pixels[idx + 3] >= 64 && pixels[idx + 2] >= 64 {
                    let q = usize::from(y >= half_h) * 2 + usize::from(x >= half_w);
                    quadrants[q] += 1;
                }
            }
        }
        assert!(
            quadrants.iter().all(|&c| c >= 12),
            "compact layout should render all 4 quadrants cleanly inside {cw}x{ch}, got {quadrants:?}"
        );
        unsafe { CoUninitialize() };
    }

    #[test]
    fn vertical_taskbar_stacks_sections_to_fit_cross_axis_and_rejects_too_narrow() {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut renderer = Renderer::new().expect("Direct2D/DirectWrite renderer");
        let cfg = Config {
            enabled: true,
            show_cpu: true,
            show_ram: true,
            show_network: true,
            ..Config::default()
        };
        let metrics = Metrics {
            cpu_pct: Some(23.0),
            ram_pct: Some(48.0),
            net_sent_bps: Some(12_300.0),
            net_recv_bps: Some(456_700.0),
        };
        let display = build_display(&cfg, &metrics, false, false, 96, false);

        let vert_bounds = LayoutBounds {
            horizontal: false,
            max_dip_w: 80.0,
            max_dip_h: 1080.0,
        };
        let layout = renderer
            .measure_layout(&display, vert_bounds)
            .expect("vertical layout");
        assert!(
            layout.stacked,
            "vertical 80px taskbar must stack Net above CPU/RAM"
        );
        assert!(
            layout.px_w <= 80,
            "stacked vertical width ({}) must fit inside 80px taskbar",
            layout.px_w
        );
        let (vw, vh, _) = renderer
            .render_layout(&display, layout, None)
            .expect("vertical render");
        let pixels = renderer.surface.as_ref().unwrap().pixels();
        let top_lit = (0..(vh / 2) as usize)
            .any(|y| (0..vw as usize).any(|x| pixels[(y * vw as usize + x) * 4 + 3] >= 64));
        let bottom_lit = ((vh / 2) as usize..vh as usize)
            .any(|y| (0..vw as usize).any(|x| pixels[(y * vw as usize + x) * 4 + 3] >= 64));
        assert!(
            top_lit && bottom_lit,
            "stacked layout must render both top and bottom sections"
        );

        // Placement must reject a widget that still exceeds cross-axis taskbar width.
        let narrow_bar = rect(0, 0, 50, 1080);
        let narrow_notify = rect(0, 900, 50, 1080);
        let narrow_uia = [rect(0, 0, 50, 300)];
        assert_eq!(
            solve_taskbar_placement(
                narrow_bar,
                Some(narrow_notify),
                Some(&narrow_uia),
                (vw, vh),
                96,
                None,
                false,
            ),
            None,
            "oversized widget must hide instead of overflowing left-pinned taskbar"
        );
        unsafe { CoUninitialize() };
    }

    #[test]
    fn small_horizontal_taskbar_clamps_safely_without_panicking() {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut renderer = Renderer::new().expect("Direct2D/DirectWrite renderer");
        let cfg = Config {
            enabled: true,
            show_cpu: true,
            show_ram: true,
            show_network: true,
            font_size: 12.0,
            ..Config::default()
        };
        let metrics = Metrics {
            cpu_pct: Some(15.0),
            ram_pct: Some(50.0),
            net_sent_bps: Some(1_024.0),
            net_recv_bps: Some(2_048.0),
        };
        let display = build_display(&cfg, &metrics, false, false, 96, false);

        // Previously panicked when max_dip_h in (16.0, 26.0) made max_h < 22.0 in clamp(22.0, max_h).
        let tiny_bounds = LayoutBounds::horizontal(20.0);
        let (tw, th, _) = renderer
            .measure_and_render_in(&display, tiny_bounds, None)
            .expect("tiny horizontal taskbar must not panic");
        assert!(tw > 0 && th <= 20, "tiny bar height {th} must stay <= 20px");

        // Windows 10 small taskbar (~30 DIPs): larger configured font steps down to fit cleanly.
        let small_bounds = LayoutBounds::horizontal(30.0);
        let small_layout = renderer
            .measure_layout(&display, small_bounds)
            .expect("small taskbar layout");
        assert!(
            small_layout.px_h <= 30 && small_layout.content_h <= small_layout.dip_h,
            "small taskbar layout {}x{} (content_h={}) must fit 30px height",
            small_layout.px_w,
            small_layout.px_h,
            small_layout.content_h
        );
        unsafe { CoUninitialize() };
    }

    #[test]
    fn light_taskbar_contrast_respects_color_prevalence_and_accent_luminance() {
        // Without ColorPrevalence, SystemUsesLightTheme drives light/dark contrast.
        assert!(resolve_light_taskbar(true, false, Some([0, 120, 212])));
        assert!(!resolve_light_taskbar(false, false, Some([255, 210, 0])));

        // With ColorPrevalence enabled, accent color luminance overrides SystemUsesLightTheme.
        assert!(
            !resolve_light_taskbar(true, true, Some([0, 120, 212])),
            "dark blue accent taskbar needs white text even when SystemUsesLightTheme is true"
        );
        assert!(
            resolve_light_taskbar(false, true, Some([255, 185, 0])),
            "bright yellow accent taskbar needs dark text even when SystemUsesLightTheme is false"
        );

        // Missing accent value falls back safely to SystemUsesLightTheme.
        assert!(resolve_light_taskbar(true, true, None));
        assert!(!resolve_light_taskbar(false, true, None));
    }

    #[test]
    fn numeric_layouts_use_tabular_figures_so_digits_have_stable_width() {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let renderer = Renderer::new().expect("Direct2D/DirectWrite renderer");
        assert!(
            renderer.tabular_typography.is_some(),
            "DirectWrite tabular figures typography must be initialized"
        );
        let fmt = renderer
            .create_text_format("Segoe UI", 9.0)
            .expect("Segoe UI text format");
        let (w_ones, _, _) = renderer.measure_numeric_text(&fmt, "11.1 KB/s");
        let (w_eights, _, _) = renderer.measure_numeric_text(&fmt, "88.8 KB/s");
        assert_eq!(
            w_ones, w_eights,
            "tabular figures should keep digit widths equal across 1 and 8"
        );
        unsafe { CoUninitialize() };
    }
}

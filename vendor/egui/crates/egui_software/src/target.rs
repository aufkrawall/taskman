//! The pixel buffer the software renderer draws into.

use emath::Rect;

/// A borrowed 32-bit-per-pixel render target in `softbuffer`'s layout.
///
/// One `u32` per pixel, `0x00RRGGBB` — the high byte is unused and written as 0. This is
/// what `softbuffer` hands back on every platform, and it is the layout of the
/// `CreateDIBSection` bitmap on Win32, so the renderer writes straight into the memory
/// GDI will blit. Channels are extracted with shifts rather than by transmuting to bytes,
/// which keeps the code endian-independent.
///
/// Rows are `stride` pixels apart, which is not necessarily `width`: a platform may pad
/// scanlines for alignment.
pub struct Target<'a> {
    pixels: &'a mut [u32],
    width: u32,
    height: u32,
    stride: u32,
}

impl<'a> Target<'a> {
    /// Wrap a buffer whose rows are tightly packed.
    ///
    /// Returns `None` if `pixels` is too small to hold `width * height`.
    pub fn new(pixels: &'a mut [u32], width: u32, height: u32) -> Option<Self> {
        Self::with_stride(pixels, width, height, width)
    }

    /// Wrap a buffer whose rows are `stride` pixels apart.
    pub fn with_stride(
        pixels: &'a mut [u32],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Option<Self> {
        if stride < width {
            return None;
        }
        let needed = (stride as usize).checked_mul(height as usize)?;
        // The final row only needs `width`, not the full `stride`, so a buffer that is
        // short by the last row's padding is still usable.
        let needed = needed
            .checked_sub((stride - width) as usize)
            .unwrap_or(needed);
        if pixels.len() < needed {
            return None;
        }
        Some(Self {
            pixels,
            width,
            height,
            stride,
        })
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// The whole target as an integer rectangle.
    #[inline]
    pub fn bounds(&self) -> PixelRect {
        PixelRect {
            min_x: 0,
            min_y: 0,
            max_x: self.width as i32,
            max_y: self.height as i32,
        }
    }

    /// One row of pixels, `width` long (padding excluded).
    ///
    /// Slicing a row once and iterating it lets the bounds check be hoisted out of the
    /// inner loop, which is why the rasterizer needs no `unsafe`.
    #[inline]
    pub fn row_mut(&mut self, y: u32) -> Option<&mut [u32]> {
        if y >= self.height {
            return None;
        }
        let start = (y as usize) * (self.stride as usize);
        self.pixels.get_mut(start..start + self.width as usize)
    }

    #[inline]
    pub fn row(&self, y: u32) -> Option<&[u32]> {
        if y >= self.height {
            return None;
        }
        let start = (y as usize) * (self.stride as usize);
        self.pixels.get(start..start + self.width as usize)
    }

    /// Fill an already-clipped rectangle with an opaque colour.
    pub fn fill_rect(&mut self, rect: PixelRect, bgrx: u32) {
        for y in rect.min_y.max(0)..rect.max_y.min(self.height as i32) {
            let (Some(lo), Some(hi)) = (usize::try_from(rect.min_x).ok(), {
                usize::try_from(rect.max_x).ok()
            }) else {
                continue;
            };
            if let Some(row) = self.row_mut(y as u32)
                && let Some(span) = row.get_mut(lo..hi)
            {
                span.fill(bgrx);
            }
        }
    }

    /// Copy the target out as an `epaint::ColorImage`, opaque.
    ///
    /// Used by the golden-image parity tests and by screenshot capture.
    pub fn to_color_image(&self) -> epaint::ColorImage {
        let mut pixels = Vec::with_capacity((self.width as usize) * (self.height as usize));
        for y in 0..self.height {
            let Some(row) = self.row(y) else { continue };
            pixels.extend(
                row.iter().map(|&px| {
                    ecolor::Color32::from_rgb((px >> 16) as u8, (px >> 8) as u8, px as u8)
                }),
            );
        }
        epaint::ColorImage::new([self.width as usize, self.height as usize], pixels)
    }
}

/// An integer, half-open pixel rectangle: `min` inclusive, `max` exclusive.
///
/// Everything downstream of clipping works in these, never in floats. The GPU's scissor
/// is also integer, so a clip never cuts a pixel in half and clipping is an exact
/// rectangle intersection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub min_x: i32,
    pub min_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

impl PixelRect {
    pub const NOTHING: Self = Self {
        min_x: 0,
        min_y: 0,
        max_x: 0,
        max_y: 0,
    };

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.max_x <= self.min_x || self.max_y <= self.min_y
    }

    #[inline]
    pub fn intersect(self, other: Self) -> Self {
        Self {
            min_x: self.min_x.max(other.min_x),
            min_y: self.min_y.max(other.min_y),
            max_x: self.max_x.min(other.max_x),
            max_y: self.max_y.min(other.max_y),
        }
    }

    #[inline]
    pub fn width(&self) -> i32 {
        (self.max_x - self.min_x).max(0)
    }

    #[inline]
    pub fn height(&self) -> i32 {
        (self.max_y - self.min_y).max(0)
    }
}

/// Convert an egui clip rect (in points) to integer physical pixels.
///
/// This reproduces `egui_glow`'s `set_clip_rect` **exactly**, including its use of
/// `round()` rather than floor/ceil on the outer edges. Diverging here would shift
/// clipped content by a pixel relative to the GPU backends and break the parity tests,
/// so any change must be made in both places.
pub fn clip_rect_to_pixels(clip_rect: Rect, pixels_per_point: f32, bounds: PixelRect) -> PixelRect {
    let min_x = (pixels_per_point * clip_rect.min.x).round() as i32;
    let min_y = (pixels_per_point * clip_rect.min.y).round() as i32;
    let max_x = (pixels_per_point * clip_rect.max.x).round() as i32;
    let max_y = (pixels_per_point * clip_rect.max.y).round() as i32;

    let min_x = min_x.clamp(bounds.min_x, bounds.max_x);
    let min_y = min_y.clamp(bounds.min_y, bounds.max_y);
    let max_x = max_x.clamp(min_x, bounds.max_x);
    let max_y = max_y.clamp(min_y, bounds.max_y);

    PixelRect {
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

/// Pack an opaque colour into the target's `0x00RRGGBB` layout.
#[inline]
pub fn pack_rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_buffer_is_rejected_rather_than_panicking_later() {
        let mut small = vec![0u32; 10];
        assert!(Target::new(&mut small, 4, 4).is_none());
        let mut ok = vec![0u32; 16];
        assert!(Target::new(&mut ok, 4, 4).is_some());
    }

    /// The last row does not need its stride padding, only its `width` pixels. A buffer
    /// sized exactly for that must be accepted, because that is what a padded platform
    /// surface actually hands us.
    #[test]
    fn the_last_row_does_not_need_its_padding() {
        let mut buf = vec![0u32; 4 * 3 + 2]; // 3 full rows of stride 4, plus 2 of the last
        assert!(Target::with_stride(&mut buf, 2, 4, 4).is_some());
    }

    #[test]
    fn stride_smaller_than_width_is_rejected() {
        let mut buf = vec![0u32; 64];
        assert!(Target::with_stride(&mut buf, 8, 4, 4).is_none());
    }

    /// Clip conversion must match `egui_glow::set_clip_rect`, including the clamp that
    /// keeps `max >= min` so an off-screen rect degenerates to empty instead of negative.
    #[test]
    fn clip_conversion_matches_the_gpu_and_clamps_to_the_target() {
        let bounds = PixelRect {
            min_x: 0,
            min_y: 0,
            max_x: 100,
            max_y: 50,
        };
        let r = clip_rect_to_pixels(
            Rect::from_min_max(emath::pos2(10.0, 5.0), emath::pos2(20.0, 15.0)),
            2.0,
            bounds,
        );
        assert_eq!(
            r,
            PixelRect {
                min_x: 20,
                min_y: 10,
                max_x: 40,
                max_y: 30
            }
        );

        // Entirely off the right edge: clamps to an empty rect at the border, never
        // to max < min.
        let off = clip_rect_to_pixels(
            Rect::from_min_max(emath::pos2(500.0, 5.0), emath::pos2(600.0, 15.0)),
            1.0,
            bounds,
        );
        assert!(off.is_empty());
        assert!(off.max_x >= off.min_x && off.max_y >= off.min_y);
    }

    #[test]
    fn rounding_follows_the_gpu_rather_than_expanding_outward() {
        let bounds = PixelRect {
            min_x: 0,
            min_y: 0,
            max_x: 100,
            max_y: 100,
        };
        // 10.4 -> 10 and 20.6 -> 21: round(), not floor()/ceil().
        let r = clip_rect_to_pixels(
            Rect::from_min_max(emath::pos2(10.4, 10.4), emath::pos2(20.6, 20.6)),
            1.0,
            bounds,
        );
        assert_eq!(r.min_x, 10);
        assert_eq!(r.max_x, 21);
    }
}

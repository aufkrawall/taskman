//! Fast path for pixel-aligned, axis-aligned rectangles.
//!
//! This is not a micro-optimisation. taskman's UI is overwhelmingly rectangles -- panel
//! and window backgrounds, every table row, every heat cell, chart backgrounds, menu rows
//! -- and the panel fills alone cover the entire window. Sending those through the
//! tessellator and the generic triangle rasterizer costs roughly fifty operations per
//! pixel (three edge tests, barycentric weights, a gouraud shade, a blend) to compute a
//! constant. At 1920x1200 that is the difference between a frame that costs a millisecond
//! and one that costs a hundred.
//!
//! # Why this is allowed to be exact rather than approximate
//!
//! epaint rounds rectangles to the pixel grid by default
//! (`TessellationOptions::round_rects_to_pixels`), and its anti-aliasing is a feathered
//! fringe baked into the geometry. For a *pixel-aligned* rect that fringe lands exactly on
//! the boundary, so the triangle rasterizer -- sampling at pixel centres -- produces a
//! fully opaque interior and leaves the outside untouched. There is no partial coverage to
//! reproduce. Filling the rounded integer rectangle is therefore not an approximation of
//! the tessellated result; it *is* the tessellated result.
//!
//! That equivalence is asserted rather than trusted: `tests/spans.rs` renders every
//! accepted rect both ways and requires the buffers to match bit for bit.
//!
//! # What is deliberately excluded
//!
//! The guard in [`fast_rect`] is narrow on purpose. Rounded corners, blur (shadows),
//! textured brushes, rotation, strokes and sub-pixel-sized rects all have real geometry
//! that the tessellator handles correctly and this does not. Widening the guard to "look
//! close enough" is how a fast path starts drawing subtly wrong pixels.

use ecolor::Color32;
use emath::GuiRounding as _;
use epaint::RectShape;

use crate::target::{PixelRect, Target};

/// The integer pixel rectangle to fill, if this shape qualifies for the fast path.
///
/// Returns `None` whenever the tessellator would produce anything other than a solid
/// axis-aligned block of one colour.
pub fn fast_rect(
    shape: &RectShape,
    pixels_per_point: f32,
    round_default: bool,
) -> Option<PixelRect> {
    let RectShape {
        rect,
        corner_radius,
        fill,
        stroke,
        blur_width,
        brush,
        angle,
        round_to_pixels,
        ..
    } = shape;

    if !corner_radius.is_same() || corner_radius.nw != 0 {
        return None; // rounded corners have arcs
    }
    if *blur_width != 0.0 || brush.is_some() || *angle != 0.0 {
        return None; // shadows, textures and rotation are real geometry
    }
    if !stroke.is_empty() {
        return None; // the stroke is a separate band; let the tessellator place it
    }
    if fill.a() == 0 {
        return None; // nothing to draw, but let the normal path decide that
    }
    if !round_to_pixels.unwrap_or(round_default) {
        return None; // not snapped, so the edges genuinely are anti-aliased
    }
    if !rect.is_finite() || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return None;
    }

    // Same rounding the tessellator applies, then into physical pixels.
    let r = rect.round_to_pixels(pixels_per_point);
    let min_x = (r.min.x * pixels_per_point).round();
    let min_y = (r.min.y * pixels_per_point).round();
    let max_x = (r.max.x * pixels_per_point).round();
    let max_y = (r.max.y * pixels_per_point).round();
    if ![min_x, min_y, max_x, max_y].iter().all(|v| v.is_finite()) {
        return None;
    }

    // A rect thinner than the feathering is drawn by the tessellator as a faded line
    // rather than a solid block -- that is how sub-pixel-width chart gridlines stay light
    // -- so anything that small must not take this path.
    let feather_px = 1.0;
    if max_x - min_x < 2.0 * feather_px || max_y - min_y < 2.0 * feather_px {
        return None;
    }

    Some(PixelRect {
        min_x: min_x as i32,
        min_y: min_y as i32,
        max_x: max_x as i32,
        max_y: max_y as i32,
    })
}

/// Fill `rect` with a premultiplied colour, clipped.
pub fn fill(target: &mut Target<'_>, clip: PixelRect, rect: PixelRect, color: Color32) {
    let bounds = clip.intersect(target.bounds()).intersect(rect);
    if bounds.is_empty() {
        return;
    }
    let [r, g, b, a] = color.to_array();

    if a == 255 {
        // Opaque: a plain store. `slice::fill` on `[u32]` lowers to a vectorised
        // memset, which is as fast as this can be.
        let packed = crate::target::pack_rgb(r, g, b);
        for y in bounds.min_y..bounds.max_y {
            let Some(row) = target.row_mut(y as u32) else {
                continue;
            };
            if let Some(span) = row.get_mut(bounds.min_x as usize..bounds.max_x as usize) {
                span.fill(packed);
            }
        }
        return;
    }

    // Translucent: premultiplied `src + dst * (1 - a)`, the same arithmetic the triangle
    // rasterizer's blend performs, with the source constant so it is hoisted out.
    let inv_a = 255 - a as u32;
    for y in bounds.min_y..bounds.max_y {
        let Some(row) = target.row_mut(y as u32) else {
            continue;
        };
        let Some(span) = row.get_mut(bounds.min_x as usize..bounds.max_x as usize) else {
            continue;
        };
        for dst in span {
            let d = *dst;
            // `(x * inv_a + 127) / 255` per channel, matching `blend_over`'s rounding.
            let blend = |src: u8, dc: u32| -> u32 {
                let t = dc * inv_a + 127;
                let t = (t + (t >> 8)) >> 8;
                (src as u32 + t).min(255)
            };
            let nr = blend(r, (d >> 16) & 0xff);
            let ng = blend(g, (d >> 8) & 0xff);
            let nb = blend(b, d & 0xff);
            *dst = (nr << 16) | (ng << 8) | nb;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::pack_rgb;
    use emath::{Rect, pos2};
    use epaint::{CornerRadius, Stroke, StrokeKind};

    fn plain(rect: Rect, fill: Color32) -> RectShape {
        RectShape::filled(rect, CornerRadius::ZERO, fill)
    }

    #[test]
    fn a_plain_pixel_aligned_rect_qualifies() {
        let s = plain(
            Rect::from_min_max(pos2(4.0, 8.0), pos2(20.0, 24.0)),
            Color32::WHITE,
        );
        assert_eq!(
            fast_rect(&s, 1.0, true),
            Some(PixelRect {
                min_x: 4,
                min_y: 8,
                max_x: 20,
                max_y: 24
            })
        );
    }

    #[test]
    fn pixels_per_point_scales_the_result() {
        let s = plain(
            Rect::from_min_max(pos2(4.0, 8.0), pos2(20.0, 24.0)),
            Color32::WHITE,
        );
        let r = fast_rect(&s, 2.0, true).expect("qualifies");
        assert_eq!((r.min_x, r.min_y, r.max_x, r.max_y), (8, 16, 40, 48));
    }

    /// Everything with real geometry must be refused, or the fast path draws the wrong
    /// shape. Each of these is a distinct way to get it wrong.
    #[test]
    fn shapes_with_real_geometry_are_refused() {
        let base = Rect::from_min_max(pos2(4.0, 4.0), pos2(20.0, 20.0));

        let rounded = RectShape::filled(base, CornerRadius::same(4), Color32::WHITE);
        assert!(fast_rect(&rounded, 1.0, true).is_none(), "rounded corners");

        let mut blurred = plain(base, Color32::WHITE);
        blurred.blur_width = 4.0;
        assert!(fast_rect(&blurred, 1.0, true).is_none(), "blur / shadow");

        let mut rotated = plain(base, Color32::WHITE);
        rotated.angle = 0.3;
        assert!(fast_rect(&rotated, 1.0, true).is_none(), "rotation");

        let stroked = RectShape::new(
            base,
            CornerRadius::ZERO,
            Color32::WHITE,
            Stroke::new(1.0, Color32::RED),
            StrokeKind::Inside,
        );
        assert!(fast_rect(&stroked, 1.0, true).is_none(), "stroke");

        let mut unsnapped = plain(base, Color32::WHITE);
        unsnapped.round_to_pixels = Some(false);
        assert!(fast_rect(&unsnapped, 1.0, true).is_none(), "not snapped");

        let transparent = plain(base, Color32::TRANSPARENT);
        assert!(fast_rect(&transparent, 1.0, true).is_none(), "no fill");
    }

    /// A rect narrower than the feathering is drawn by the tessellator as a *faded* line,
    /// which is how taskman's 0.75px gridlines stay light. Taking the fast path would
    /// snap them to a solid pixel and make the chrome heavier than intended.
    #[test]
    fn sub_pixel_sized_rects_are_refused() {
        let thin = plain(
            Rect::from_min_max(pos2(4.0, 4.0), pos2(20.0, 4.75)),
            Color32::WHITE,
        );
        assert!(fast_rect(&thin, 1.0, true).is_none());
    }

    #[test]
    fn degenerate_rects_are_refused() {
        for r in [
            Rect::from_min_max(pos2(4.0, 4.0), pos2(4.0, 20.0)),
            Rect::from_min_max(pos2(20.0, 4.0), pos2(4.0, 20.0)),
            Rect::from_min_max(pos2(f32::NAN, 4.0), pos2(20.0, 20.0)),
        ] {
            assert!(
                fast_rect(&plain(r, Color32::WHITE), 1.0, true).is_none(),
                "{r:?}"
            );
        }
    }

    #[test]
    fn opaque_fill_writes_exactly_the_rect() {
        let mut buf = vec![0u32; 64];
        let mut t = Target::new(&mut buf, 8, 8).unwrap();
        let clip = t.bounds();
        fill(
            &mut t,
            clip,
            PixelRect {
                min_x: 2,
                min_y: 2,
                max_x: 6,
                max_y: 6,
            },
            Color32::from_rgb(0x4c, 0xc2, 0xff),
        );
        assert_eq!(buf[2 * 8 + 2], pack_rgb(0x4c, 0xc2, 0xff));
        assert_eq!(buf[5 * 8 + 5], pack_rgb(0x4c, 0xc2, 0xff));
        assert_eq!(buf[8 + 2], 0, "row above untouched");
        assert_eq!(buf[2 * 8 + 6], 0, "max is exclusive");
    }

    /// The translucent path must agree with the triangle rasterizer's blend exactly, or a
    /// hover highlight drawn by one and a row fill drawn by the other will not match.
    #[test]
    fn translucent_fill_matches_the_triangle_blend() {
        let src = Color32::from_rgba_premultiplied(0x20, 0x40, 0x60, 0x80);
        let dst = pack_rgb(0x19, 0x40, 0xff);

        let mut buf = vec![dst; 4];
        let mut t = Target::new(&mut buf, 2, 2).unwrap();
        let clip = t.bounds();
        fill(
            &mut t,
            clip,
            PixelRect {
                min_x: 0,
                min_y: 0,
                max_x: 2,
                max_y: 2,
            },
            src,
        );

        let expected = crate::raster::blend_over(
            [
                src.r() as f32,
                src.g() as f32,
                src.b() as f32,
                src.a() as f32,
            ],
            dst,
        );
        assert_eq!(buf[0], expected);
    }

    #[test]
    fn the_clip_rect_is_respected() {
        let mut buf = vec![0u32; 64];
        let mut t = Target::new(&mut buf, 8, 8).unwrap();
        fill(
            &mut t,
            PixelRect {
                min_x: 4,
                min_y: 0,
                max_x: 8,
                max_y: 8,
            },
            PixelRect {
                min_x: 0,
                min_y: 0,
                max_x: 8,
                max_y: 8,
            },
            Color32::WHITE,
        );
        for y in 0..8 {
            for x in 0..4 {
                assert_eq!(buf[y * 8 + x], 0, "wrote left of the clip at ({x},{y})");
            }
        }
    }
}

//! Direct glyph blitting.
//!
//! Text could go through the triangle rasterizer like everything else -- it did in the
//! first version of this crate, and the output was correct. This path exists for one
//! reason that matters and one that is merely nice:
//!
//! * **Sub-pixel (`ClearType`) rendering needs it.** Per-channel coverage has to be blended
//!   per channel, which is not something a generic `vertex * texel` fragment path can
//!   express. A dedicated glyph blit is where that happens.
//! * A glyph is an axis-aligned, 1:1 texel-to-pixel blit, so setting up two gouraud
//!   triangles and interpolating barycentrics for it is pure overhead.
//!
//! # Why 1:1 is guaranteed, not hoped for
//!
//! `epaint::text::text_layout::tessellate_glyphs` builds each glyph quad as
//!
//! ```text
//! left_top   = round_to_pixel(glyph.pos + uv_rect.offset)
//! rect       = Rect::from_min_max(left_top, left_top + uv_rect.size)
//! ```
//!
//! with `uv_rect.size == texel_size / pixels_per_point`. Multiplying back by
//! `pixels_per_point` therefore yields an integer origin and an exactly-integer extent
//! equal to the texel extent. The mapping is 1:1 by construction, at any DPI scale
//! including fractional ones.
//!
//! That still has to be *checked* rather than assumed: italics shear the quad
//! (`text_layout.rs` offsets the top edge by `height * 0.25`), and a rotated
//! [`epaint::TextShape`] rotates it. [`blit_quad`] returns `false` for anything that is
//! not a clean axis-aligned 1:1 blit, and the caller falls back to the triangle path --
//! which is why the output is identical either way.

use ecolor::Color32;
use epaint::text::SubpixelMode;

use crate::gamma::TextGamma;
use crate::raster::ScreenVertex;
use crate::target::{PixelRect, Target};
use crate::texture::Texture;

/// How glyph coverage is turned into pixels.
#[derive(Clone, Copy)]
pub struct TextBlend<'a> {
    /// `Off` means the atlas holds grayscale coverage and the ordinary premultiplied
    /// blend applies. Otherwise each channel carries its own coverage.
    pub subpixel: SubpixelMode,
    /// Gamma and contrast tables. Only consulted on the sub-pixel path: the grayscale
    /// path must stay bit-identical to the triangle rasterizer, and applying gamma there
    /// would silently change every existing rendering.
    pub gamma: &'a TextGamma,
}

/// How close a coordinate must be to an integer to count as pixel-aligned.
///
/// The values arrive as `f32` after a multiply by `pixels_per_point`, so exact equality
/// is too strict; a sixteenth of a pixel is far tighter than any real misalignment and
/// far looser than float noise.
const SNAP_EPS: f32 = 1.0 / 64.0;

/// Try to draw a glyph quad as a direct 1:1 blit.
///
/// `v` is in `add_rect_with_uv` order: left-top, right-top, left-bottom, right-bottom,
/// with `pos` in physical pixels and `uv` in **texels** (not normalised -- the caller
/// must not divide by the atlas size, which would throw away exactness).
///
/// Returns `false` when the quad is not an axis-aligned 1:1 rectangle, leaving the caller
/// to tessellate it instead.
pub fn blit_quad(
    target: &mut Target<'_>,
    clip: PixelRect,
    v: &[ScreenVertex; 4],
    atlas: &Texture,
    blend: TextBlend<'_>,
) -> bool {
    let (lt, rt, lb, rb) = (&v[0], &v[1], &v[2], &v[3]);

    // Axis-aligned in both position and texture space?
    let aligned = near(lt.x, lb.x)
        && near(rt.x, rb.x)
        && near(lt.y, rt.y)
        && near(lb.y, rb.y)
        && near(lt.u, lb.u)
        && near(rt.u, rb.u)
        && near(lt.v, rt.v)
        && near(lb.v, rb.v);
    if !aligned {
        return false;
    }

    // A glyph quad carries one colour on all four vertices; anything else is not a glyph.
    if lt.color != rt.color || lt.color != lb.color || lt.color != rb.color {
        return false;
    }

    // Integer origin and integer, matching extents.
    let (x0, y0) = (round_if_snapped(lt.x), round_if_snapped(lt.y));
    let (x1, y1) = (round_if_snapped(rt.x), round_if_snapped(lb.y));
    let (u0, v0) = (round_if_snapped(lt.u), round_if_snapped(lt.v));
    let (u1, v1) = (round_if_snapped(rt.u), round_if_snapped(lb.v));
    let (Some(x0), Some(y0), Some(x1), Some(y1)) = (x0, y0, x1, y1) else {
        return false;
    };
    let (Some(u0), Some(v0), Some(u1), Some(v1)) = (u0, v0, u1, v1) else {
        return false;
    };
    if x1 - x0 != u1 - u0 || y1 - y0 != v1 - v0 {
        return false; // scaled, not 1:1
    }
    if x1 <= x0 || y1 <= y0 {
        return true; // empty glyph (a space); nothing to draw, but handled
    }

    blit_rect(
        target,
        clip,
        (x0, y0, x1, y1),
        (u0, v0),
        lt.color,
        atlas,
        blend,
    );
    true
}

/// The blit itself, over an already-validated 1:1 rectangle.
fn blit_rect(
    target: &mut Target<'_>,
    clip: PixelRect,
    (x0, y0, x1, y1): (i32, i32, i32, i32),
    (u0, v0): (i32, i32),
    color: Color32,
    atlas: &Texture,
    blend: TextBlend<'_>,
) {
    let bounds = clip.intersect(target.bounds()).intersect(PixelRect {
        min_x: x0,
        min_y: y0,
        max_x: x1,
        max_y: y1,
    });
    if bounds.is_empty() {
        return;
    }

    let [cr, cg, cb, ca] = color.to_array();
    if ca == 0 {
        return;
    }
    // The gamma blend needs the *straight* text colour: it interpolates between source
    // and destination, with coverage carrying the alpha. Using the premultiplied value
    // would darken faded text twice over.
    let [sr, sg, sb, _] = color.to_srgba_unmultiplied();

    for y in bounds.min_y..bounds.max_y {
        let src_y = v0 + (y - y0);
        if src_y < 0 || src_y as usize >= atlas.height {
            continue;
        }
        let src_row_start = (src_y as usize) * atlas.width;
        let Some(dst_row) = target.row_mut(y as u32) else {
            continue;
        };

        for x in bounds.min_x..bounds.max_x {
            let src_x = u0 + (x - x0);
            if src_x < 0 || src_x as usize >= atlas.width {
                continue;
            }
            let Some(&texel) = atlas.pixels.get(src_row_start + src_x as usize) else {
                continue;
            };
            let [tr, tg, tb, ta] = texel.to_array();
            if ta == 0 && tr == 0 && tg == 0 && tb == 0 {
                continue; // fully uncovered: the majority of a glyph's bounding box
            }
            let Some(dst) = dst_row.get_mut(x as usize) else {
                continue;
            };

            if blend.subpixel.is_off() {
                // Identical arithmetic to the triangle path's fragment shader and blend:
                // multiply in gamma space, then premultiplied `src + dst * (1 - src.a)`.
                // This must stay bit-identical -- `tests/text.rs` compares the two paths.
                let f = |c: u8, t: u8| c as f32 * t as f32 * (1.0 / 255.0);
                let src = [f(cr, tr), f(cg, tg), f(cb, tb), f(ca, ta)];
                *dst = crate::raster::blend_over(src, *dst);
            } else {
                // Per-channel coverage. This is the whole reason a CPU renderer can do
                // ClearType and a GPU one cannot: each channel is blended against its own
                // coverage, which on a GPU would need dual-source blending.
                //
                // The atlas is always written in RGB stripe order (the filter is
                // symmetric, so BGR panels are handled by swapping here rather than by
                // rebuilding the atlas).
                let (cov_r, cov_b) = if blend.subpixel.swaps_rb() {
                    (tb, tr)
                } else {
                    (tr, tb)
                };
                let scale = |cov: u8| {
                    // Text alpha (from `opacity_factor`, or a translucent colour) scales
                    // coverage; it is not a separate blend stage.
                    ((u32::from(cov) * u32::from(ca) + 127) / 255) as u8
                };
                let (dr, dg, db) = (
                    ((*dst >> 16) & 0xff) as u8,
                    ((*dst >> 8) & 0xff) as u8,
                    (*dst & 0xff) as u8,
                );
                let r = blend.gamma.blend_channel(sr, dr, scale(cov_r));
                let g = blend.gamma.blend_channel(sg, dg, scale(tg));
                let b = blend.gamma.blend_channel(sb, db, scale(cov_b));
                *dst = crate::target::pack_rgb(r, g, b);
            }
        }
    }
}

#[inline]
fn near(a: f32, b: f32) -> bool {
    (a - b).abs() <= SNAP_EPS
}

/// Round to an integer, or `None` if the value is not close enough to one.
#[inline]
fn round_if_snapped(v: f32) -> Option<i32> {
    let r = v.round();
    if (v - r).abs() <= SNAP_EPS && r.abs() < i32::MAX as f32 {
        Some(r as i32)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::pack_rgb;
    use epaint::textures::TextureOptions;

    fn vert(x: f32, y: f32, u: f32, v: f32, color: Color32) -> ScreenVertex {
        ScreenVertex { x, y, u, v, color }
    }

    fn gamma_tables() -> &'static TextGamma {
        static G: std::sync::OnceLock<TextGamma> = std::sync::OnceLock::new();
        G.get_or_init(TextGamma::default)
    }

    fn gray() -> TextBlend<'static> {
        TextBlend {
            subpixel: SubpixelMode::Off,
            gamma: gamma_tables(),
        }
    }

    fn lcd(mode: SubpixelMode) -> TextBlend<'static> {
        TextBlend {
            subpixel: mode,
            gamma: gamma_tables(),
        }
    }

    /// A 4x4 atlas whose texels encode their own coordinates in the alpha channel, so a
    /// blit that is off by a texel produces a different, detectable result.
    fn ramp_atlas() -> Texture {
        let mut pixels = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                let a = ((y * 4 + x) * 17) as u8;
                pixels.push(Color32::from_rgba_premultiplied(a, a, a, a));
            }
        }
        Texture {
            pixels,
            width: 4,
            height: 4,
            options: TextureOptions::LINEAR,
        }
    }

    fn quad(x: f32, y: f32, w: f32, h: f32, u: f32, v: f32, color: Color32) -> [ScreenVertex; 4] {
        [
            vert(x, y, u, v, color),
            vert(x + w, y, u + w, v, color),
            vert(x, y + h, u, v + h, color),
            vert(x + w, y + h, u + w, v + h, color),
        ]
    }

    fn full_clip(w: u32, h: u32) -> PixelRect {
        PixelRect {
            min_x: 0,
            min_y: 0,
            max_x: w as i32,
            max_y: h as i32,
        }
    }

    #[test]
    fn a_pixel_aligned_quad_blits_texel_for_texel() {
        let atlas = ramp_atlas();
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        let q = quad(2.0, 2.0, 4.0, 4.0, 0.0, 0.0, Color32::WHITE);
        assert!(blit_quad(&mut target, full_clip(8, 8), &q, &atlas, gray()));

        // Texel (0,0) has alpha 0 -> untouched. Texel (3,3) has alpha 255 -> full white.
        assert_eq!(buf[2 * 8 + 2], 0, "the fully-transparent texel was skipped");
        assert_eq!(buf[5 * 8 + 5], pack_rgb(255, 255, 255), "the opaque texel");
    }

    /// A sheared quad -- what italics produce -- must be refused so the caller falls back
    /// to the triangle path rather than silently drawing it upright.
    #[test]
    fn a_sheared_quad_is_refused() {
        let atlas = ramp_atlas();
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        let mut q = quad(2.0, 2.0, 4.0, 4.0, 0.0, 0.0, Color32::WHITE);
        q[0].x += 1.0; // shear the top edge, as `format.italics` does
        q[1].x += 1.0;
        assert!(!blit_quad(&mut target, full_clip(8, 8), &q, &atlas, gray()));
        assert!(buf.iter().all(|&p| p == 0), "nothing was drawn");
    }

    /// A quad whose destination is larger than its source is scaled, not 1:1, and must be
    /// refused.
    #[test]
    fn a_scaled_quad_is_refused() {
        let atlas = ramp_atlas();
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        let mut q = quad(1.0, 1.0, 6.0, 6.0, 0.0, 0.0, Color32::WHITE);
        q[1].u = 4.0; // 6px wide from 4 texels
        q[3].u = 4.0;
        assert!(!blit_quad(&mut target, full_clip(8, 8), &q, &atlas, gray()));
    }

    /// A quad landing on a half-pixel is not snapped and must be refused, so it keeps the
    /// smoother triangle-path result instead of jumping by half a pixel.
    #[test]
    fn a_half_pixel_offset_quad_is_refused() {
        let atlas = ramp_atlas();
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        let q = quad(2.5, 2.0, 4.0, 4.0, 0.0, 0.0, Color32::WHITE);
        assert!(!blit_quad(&mut target, full_clip(8, 8), &q, &atlas, gray()));
    }

    #[test]
    fn the_clip_rect_is_respected() {
        let atlas = ramp_atlas();
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        let clip = PixelRect {
            min_x: 4,
            min_y: 0,
            max_x: 8,
            max_y: 8,
        };
        let q = quad(2.0, 2.0, 4.0, 4.0, 0.0, 0.0, Color32::WHITE);
        assert!(blit_quad(&mut target, clip, &q, &atlas, gray()));
        for y in 0..8 {
            for x in 0..4 {
                assert_eq!(buf[y * 8 + x], 0, "wrote left of the clip at ({x},{y})");
            }
        }
    }

    /// An empty glyph -- a space has a zero-size `uv_rect` -- is handled, not refused,
    /// so it does not fall through to the tessellator for nothing.
    #[test]
    fn an_empty_glyph_is_accepted_and_draws_nothing() {
        let atlas = ramp_atlas();
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        let q = quad(2.0, 2.0, 0.0, 0.0, 0.0, 0.0, Color32::WHITE);
        assert!(blit_quad(&mut target, full_clip(8, 8), &q, &atlas, gray()));
        assert!(buf.iter().all(|&p| p == 0));
    }

    /// A source rectangle that runs off the atlas must clip rather than wrap onto the
    /// next texel row or panic.
    #[test]
    fn a_source_rect_outside_the_atlas_is_clipped() {
        let atlas = ramp_atlas();
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        let q = quad(0.0, 0.0, 6.0, 6.0, 2.0, 2.0, Color32::WHITE);
        assert!(blit_quad(&mut target, full_clip(8, 8), &q, &atlas, gray()));
    }

    /// A texel whose channels differ must produce a pixel whose channels differ -- that
    /// is sub-pixel rendering. If this ever returns a gray pixel, the per-channel path is
    /// not being taken and there is no `ClearType`.
    #[test]
    fn subpixel_coverage_produces_a_tinted_pixel() {
        let atlas = Texture {
            // Red fully covered, green half, blue not at all.
            pixels: vec![Color32::from_rgba_premultiplied(255, 128, 0, 255)],
            width: 1,
            height: 1,
            options: TextureOptions::NEAREST,
        };
        let mut buf = vec![0u32; 4];
        let mut target = Target::new(&mut buf, 2, 2).unwrap();
        let q = quad(0.0, 0.0, 1.0, 1.0, 0.0, 0.0, Color32::WHITE);
        assert!(blit_quad(
            &mut target,
            full_clip(2, 2),
            &q,
            &atlas,
            lcd(SubpixelMode::Rgb)
        ));

        let (r, g, b) = ((buf[0] >> 16) & 0xff, (buf[0] >> 8) & 0xff, buf[0] & 0xff);
        assert!(r > g && g > b, "expected r > g > b, got {r} {g} {b}");
    }

    /// BGR panels are handled by swapping at blend time rather than by rebuilding the
    /// atlas, because the LCD filter is symmetric. The two orders must therefore be exact
    /// mirrors of each other.
    #[test]
    fn bgr_mirrors_rgb() {
        let atlas = Texture {
            pixels: vec![Color32::from_rgba_premultiplied(255, 128, 0, 255)],
            width: 1,
            height: 1,
            options: TextureOptions::NEAREST,
        };
        let render = |mode| {
            let mut buf = vec![0u32; 4];
            let mut target = Target::new(&mut buf, 2, 2).unwrap();
            let q = quad(0.0, 0.0, 1.0, 1.0, 0.0, 0.0, Color32::WHITE);
            blit_quad(&mut target, full_clip(2, 2), &q, &atlas, lcd(mode));
            buf[0]
        };
        let rgb = render(SubpixelMode::Rgb);
        let bgr = render(SubpixelMode::Bgr);
        assert_eq!(
            (rgb >> 16) & 0xff,
            bgr & 0xff,
            "R of RGB must equal B of BGR"
        );
        assert_eq!(
            rgb & 0xff,
            (bgr >> 16) & 0xff,
            "B of RGB must equal R of BGR"
        );
        assert_eq!((rgb >> 8) & 0xff, (bgr >> 8) & 0xff, "green is unaffected");
    }

    /// Equal coverage on all three channels must produce a neutral pixel -- no tint where
    /// there is no sub-pixel difference. A stray channel swap or an off-by-one in the
    /// tables would show up here as a coloured cast across all text.
    #[test]
    fn equal_coverage_produces_no_tint() {
        let atlas = Texture {
            pixels: vec![Color32::from_rgba_premultiplied(128, 128, 128, 128)],
            width: 1,
            height: 1,
            options: TextureOptions::NEAREST,
        };
        let mut buf = vec![0u32; 4];
        let mut target = Target::new(&mut buf, 2, 2).unwrap();
        let q = quad(0.0, 0.0, 1.0, 1.0, 0.0, 0.0, Color32::WHITE);
        blit_quad(
            &mut target,
            full_clip(2, 2),
            &q,
            &atlas,
            lcd(SubpixelMode::Rgb),
        );
        let (r, g, b) = ((buf[0] >> 16) & 0xff, (buf[0] >> 8) & 0xff, buf[0] & 0xff);
        assert_eq!(r, g);
        assert_eq!(g, b);
    }

    /// Faded text must fade, not shift hue: alpha scales coverage on every channel
    /// equally.
    #[test]
    fn text_alpha_scales_coverage_without_tinting() {
        let atlas = Texture {
            pixels: vec![Color32::from_rgba_premultiplied(255, 255, 255, 255)],
            width: 1,
            height: 1,
            options: TextureOptions::NEAREST,
        };
        let render = |color| {
            let mut buf = vec![0u32; 4];
            let mut target = Target::new(&mut buf, 2, 2).unwrap();
            let q = quad(0.0, 0.0, 1.0, 1.0, 0.0, 0.0, color);
            blit_quad(
                &mut target,
                full_clip(2, 2),
                &q,
                &atlas,
                lcd(SubpixelMode::Rgb),
            );
            buf[0]
        };
        let full = render(Color32::WHITE);
        let half = render(Color32::from_rgba_premultiplied(128, 128, 128, 128));
        assert!(half < full, "half-alpha text should be dimmer");
        let (r, g, b) = ((half >> 16) & 0xff, (half >> 8) & 0xff, half & 0xff);
        assert_eq!(r, g, "fading must not tint");
        assert_eq!(g, b, "fading must not tint");
    }
}

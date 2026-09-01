//! The triangle rasterizer.
//!
//! This is a CPU implementation of what egui's GPU backends do, and it is deliberately
//! nothing more than that. `egui_glow`'s pipeline is:
//!
//! * vertex shader: `v_rgba_in_gamma = a_srgba / 255.0`, positions in points mapped
//!   through `u_screen_size` onto a viewport measured in physical pixels — so
//!   `physical = point * pixels_per_point`;
//! * fragment shader: `frag = v_rgba_in_gamma * texture_in_gamma`, i.e. a plain
//!   component-wise multiply in **gamma (sRGB) space**, with `FRAMEBUFFER_SRGB` disabled;
//! * blending: `blend_func_separate(ONE, ONE_MINUS_SRC_ALPHA, ONE_MINUS_DST_ALPHA, ONE)`
//!   over premultiplied alpha.
//!
//! Reproducing that exactly is what makes a golden-image diff against the GPU a valid
//! correctness test, and that test is worth far more than any cleverness here.
//!
//! ## Why anti-aliasing is absent on purpose
//!
//! epaint's tessellator already bakes anti-aliasing into the geometry: it emits a 1-px
//! feathered fringe whose vertex alpha ramps to zero. Sampling at pixel centres with no
//! coverage AA evaluates that ramp at ±0.5 from the boundary, which yields a fully opaque
//! pixel inside and a fully transparent one outside. Adding coverage AA here would
//! double-count the fringe and make everything look soft.
//!
//! ## Watertightness
//!
//! Edge functions are evaluated in 28.4 fixed point with `i64` accumulators and a strict
//! top-left fill rule. Floating-point edge tests leak: adjacent triangles either both
//! claim a boundary pixel (a double-blend, which shows up as a dark seam through the
//! chart area fills) or neither does (a crack). Both are exactly the artefacts a
//! screenshot diff catches and a casual look does not.

use ecolor::Color32;

use crate::target::{PixelRect, Target};
use crate::texture::Texture;

/// Sub-pixel precision of the edge functions: 1/16 px.
const SUB_BITS: u32 = 4;
const SUB_ONE: i32 = 1 << SUB_BITS;

/// A vertex after transformation into physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct ScreenVertex {
    /// Position in physical pixels.
    pub x: f32,
    pub y: f32,
    pub u: f32,
    pub v: f32,
    pub color: Color32,
}

/// Snap a coordinate to the 28.4 grid.
#[inline]
fn to_fixed(v: f32) -> i32 {
    // Clamp before converting: a NaN or a wildly out-of-range coordinate (egui can emit
    // these for degenerate layouts) must not wrap into a bogus in-range value.
    let scaled = (v * SUB_ONE as f32).round();
    if scaled.is_nan() {
        0
    } else {
        scaled.clamp(i32::MIN as f32 / 4.0, i32::MAX as f32 / 4.0) as i32
    }
}

/// Twice the signed area of the triangle `a, b, c`, in 28.4 units squared.
#[inline]
fn orient2d(ax: i32, ay: i32, bx: i32, by: i32, cx: i32, cy: i32) -> i64 {
    (bx as i64 - ax as i64) * (cy as i64 - ay as i64)
        - (by as i64 - ay as i64) * (cx as i64 - ax as i64)
}

/// Is the edge `a -> b` a top or left edge of a positively-wound triangle?
///
/// Screen space has y growing downward, and the caller has normalised the winding so
/// `orient2d(v0, v1, v2) > 0`. Under that convention a horizontal edge running rightward
/// is the top of the triangle, and any edge running upward has the interior to its right,
/// making it a left edge. Top and left edges own their boundary pixels; the others yield,
/// so a shared edge is rasterized exactly once.
#[inline]
fn is_top_left(dx: i32, dy: i32) -> bool {
    (dy == 0 && dx > 0) || dy < 0
}

/// Rasterize one triangle, blending into `target` within `clip`.
///
/// `texture` is `None` for untextured geometry, which egui expresses by pointing at the
/// font atlas's white texel; skipping the fetch for it is both faster and exact.
pub fn triangle(
    target: &mut Target<'_>,
    clip: PixelRect,
    v: [ScreenVertex; 3],
    texture: Option<&Texture>,
) {
    let mut v = v;

    let (mut x0, mut y0) = (to_fixed(v[0].x), to_fixed(v[0].y));
    let (mut x1, mut y1) = (to_fixed(v[1].x), to_fixed(v[1].y));
    let (x2, y2) = (to_fixed(v[2].x), to_fixed(v[2].y));

    let mut area = orient2d(x0, y0, x1, y1, x2, y2);
    if area == 0 {
        return; // degenerate: zero pixels, and the barycentric divide would be undefined
    }
    if area < 0 {
        // egui is explicitly inconsistent about winding order ("turn off backface
        // culling"), so normalise rather than cull.
        v.swap(0, 1);
        std::mem::swap(&mut x0, &mut x1);
        std::mem::swap(&mut y0, &mut y1);
        area = -area;
    }

    // Bounding box, snapped outward to whole pixels, then clipped.
    let bbox = PixelRect {
        min_x: x0.min(x1).min(x2) >> SUB_BITS,
        min_y: y0.min(y1).min(y2) >> SUB_BITS,
        max_x: (x0.max(x1).max(x2) >> SUB_BITS) + 1,
        max_y: (y0.max(y1).max(y2) >> SUB_BITS) + 1,
    };
    let bounds = clip.intersect(target.bounds()).intersect(bbox);
    if bounds.is_empty() {
        return;
    }

    // Edge i is opposite vertex i.
    let edges = [(x1, y1, x2, y2), (x2, y2, x0, y0), (x0, y0, x1, y1)];
    let mut bias = [0i64; 3];
    let mut step_x = [0i64; 3];
    let mut step_y = [0i64; 3];
    for (i, &(ax, ay, bx, by)) in edges.iter().enumerate() {
        bias[i] = if is_top_left(bx - ax, by - ay) { 0 } else { -1 };
        // d/dx of orient2d(a, b, p) is (a.y - b.y); one whole pixel is SUB_ONE units.
        step_x[i] = (ay as i64 - by as i64) * SUB_ONE as i64;
        step_y[i] = (bx as i64 - ax as i64) * SUB_ONE as i64;
    }

    // Evaluate at the centre of the top-left pixel of the bounding box.
    let px0 = (bounds.min_x << SUB_BITS) + SUB_ONE / 2;
    let py0 = (bounds.min_y << SUB_BITS) + SUB_ONE / 2;
    let mut row_w = [0i64; 3];
    for (i, &(ax, ay, bx, by)) in edges.iter().enumerate() {
        row_w[i] = orient2d(ax, ay, bx, by, px0, py0) + bias[i];
    }

    let inv_area = 1.0 / area as f32;

    for y in bounds.min_y..bounds.max_y {
        let mut w = row_w;
        let Some(row) = target.row_mut(y as u32) else {
            for k in 0..3 {
                row_w[k] += step_y[k];
            }
            continue;
        };

        for x in bounds.min_x..bounds.max_x {
            if w[0] >= 0 && w[1] >= 0 && w[2] >= 0 {
                // Barycentrics. The bias shifted the edge values by at most one unit to
                // implement the fill rule; using the unbiased weights for interpolation
                // would cost a subtract per pixel for a sub-LSB difference in the result,
                // so the biased values are used directly.
                let l0 = w[0] as f32 * inv_area;
                let l1 = w[1] as f32 * inv_area;
                let l2 = 1.0 - l0 - l1;

                let src = shade(&v, l0, l1, l2, texture);
                if src[3] > 0.0
                    && let Some(dst) = row.get_mut(x as usize)
                {
                    *dst = blend_over(src, *dst);
                }
            }
            for k in 0..3 {
                w[k] += step_x[k];
            }
        }

        for k in 0..3 {
            row_w[k] += step_y[k];
        }
    }
}

/// The fragment shader: interpolate the vertex colour, fetch the texel, multiply in
/// gamma space. Returns premultiplied RGBA in `0..=255` as floats.
///
/// Phase 1 deliberately works in `f32` end to end and rounds exactly once, in
/// [`blend_over`]. A GPU computes in float too, so this is the shortest path to matching
/// it bit-for-bit; the integer and SIMD fast paths come later, guarded by the parity test
/// this enables.
#[inline]
fn shade(v: &[ScreenVertex; 3], l0: f32, l1: f32, l2: f32, texture: Option<&Texture>) -> [f32; 4] {
    let c0 = v[0].color.to_array();
    let c1 = v[1].color.to_array();
    let c2 = v[2].color.to_array();
    let mut out = [0.0f32; 4];
    for i in 0..4 {
        out[i] = c0[i] as f32 * l0 + c1[i] as f32 * l1 + c2[i] as f32 * l2;
    }

    if let Some(tex) = texture {
        let u = v[0].u * l0 + v[1].u * l1 + v[2].u * l2;
        let vv = v[0].v * l0 + v[1].v * l1 + v[2].v * l2;
        let texel = tex.sample(u, vv).to_array();
        for i in 0..4 {
            out[i] = out[i] * texel[i] as f32 * (1.0 / 255.0);
        }
    }
    out
}

/// `dst = src + dst * (1 - src.a)` over premultiplied gamma-space bytes.
///
/// The framebuffer is opaque, so the separate destination-alpha blend factor the GPU uses
/// has no observable effect and is not computed.
#[inline]
fn blend_over(src: [f32; 4], dst: u32) -> u32 {
    let inv_a = 1.0 - src[3] * (1.0 / 255.0);
    let dr = ((dst >> 16) & 0xff) as f32;
    let dg = ((dst >> 8) & 0xff) as f32;
    let db = (dst & 0xff) as f32;
    let r = (src[0] + dr * inv_a).round().clamp(0.0, 255.0) as u32;
    let g = (src[1] + dg * inv_a).round().clamp(0.0, 255.0) as u32;
    let b = (src[2] + db * inv_a).round().clamp(0.0, 255.0) as u32;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::pack_rgb;

    fn vert(x: f32, y: f32, color: Color32) -> ScreenVertex {
        ScreenVertex {
            x,
            y,
            u: 0.0,
            v: 0.0,
            color,
        }
    }

    fn blank(w: u32, h: u32) -> Vec<u32> {
        vec![0u32; (w * h) as usize]
    }

    fn full_clip(w: u32, h: u32) -> PixelRect {
        PixelRect {
            min_x: 0,
            min_y: 0,
            max_x: w as i32,
            max_y: h as i32,
        }
    }

    /// Two triangles meeting along a shared diagonal must cover every pixel of the
    /// rectangle exactly once. Counting *hits* rather than inspecting colours is what
    /// distinguishes a crack (0 hits) from a double-blend (2 hits) -- with opaque paint
    /// both look identical in the output, but with egui's translucent fringes the
    /// double-blend is a visible seam.
    #[test]
    fn a_shared_edge_is_rasterized_exactly_once() {
        let (w, h) = (32u32, 32u32);
        let mut buf = blank(w, h);
        let mut target = Target::new(&mut buf, w, h).unwrap();
        let clip = full_clip(w, h);

        // Half-transparent white: each hit adds ~50% toward white, so 1 hit and 2 hits
        // land on clearly different values.
        let c = Color32::from_rgba_premultiplied(128, 128, 128, 128);
        let (a, b, cc, d) = (
            vert(4.0, 4.0, c),
            vert(28.0, 4.0, c),
            vert(28.0, 28.0, c),
            vert(4.0, 28.0, c),
        );
        triangle(&mut target, clip, [a, b, cc], None);
        triangle(&mut target, clip, [a, cc, d], None);

        let once = blend_over([128.0, 128.0, 128.0, 128.0], 0);
        for y in 5..27u32 {
            for x in 5..27u32 {
                let px = target.row(y).unwrap()[x as usize];
                assert_eq!(
                    px, once,
                    "pixel ({x},{y}) was covered zero or twice (a crack or a double-blend)"
                );
            }
        }
    }

    /// Adjacent triangles that share a vertical edge -- the shape `area_strip_mesh`
    /// produces for every chart fill. Thin, near-vertical, and the case most likely to
    /// crack.
    #[test]
    fn thin_vertical_strips_tile_without_cracks_or_overlap() {
        let (w, h) = (16u32, 64u32);
        let mut buf = blank(w, h);
        let mut target = Target::new(&mut buf, w, h).unwrap();
        let clip = full_clip(w, h);
        let c = Color32::from_rgba_premultiplied(128, 128, 128, 128);

        // Four 1-px-wide, 60-px-tall columns, each two triangles, sharing edges.
        for i in 0..4 {
            let x0 = 2.0 + i as f32;
            let x1 = x0 + 1.0;
            let (tl, tr, br, bl) = (
                vert(x0, 2.0, c),
                vert(x1, 2.0, c),
                vert(x1, 62.0, c),
                vert(x0, 62.0, c),
            );
            triangle(&mut target, clip, [tl, tr, br], None);
            triangle(&mut target, clip, [tl, br, bl], None);
        }

        let once = blend_over([128.0, 128.0, 128.0, 128.0], 0);
        for y in 2..62u32 {
            for x in 2..6u32 {
                assert_eq!(
                    target.row(y).unwrap()[x as usize],
                    once,
                    "strip pixel ({x},{y}) not covered exactly once"
                );
            }
        }
    }

    /// Winding order must not matter: egui emits both.
    #[test]
    fn both_windings_produce_the_same_pixels() {
        let (w, h) = (16u32, 16u32);
        let c = Color32::WHITE;
        let (a, b, cc) = (vert(2.0, 2.0, c), vert(12.0, 2.0, c), vert(2.0, 12.0, c));

        let mut buf1 = blank(w, h);
        let mut t1 = Target::new(&mut buf1, w, h).unwrap();
        triangle(&mut t1, full_clip(w, h), [a, b, cc], None);

        let mut buf2 = blank(w, h);
        let mut t2 = Target::new(&mut buf2, w, h).unwrap();
        triangle(&mut t2, full_clip(w, h), [a, cc, b], None);

        assert_eq!(buf1, buf2);
    }

    #[test]
    fn degenerate_triangles_draw_nothing_and_do_not_panic() {
        let (w, h) = (8u32, 8u32);
        let mut buf = blank(w, h);
        let mut target = Target::new(&mut buf, w, h).unwrap();
        let clip = full_clip(w, h);
        let c = Color32::WHITE;

        // Zero area: all three collinear, and all three identical.
        triangle(
            &mut target,
            clip,
            [vert(0.0, 0.0, c), vert(4.0, 4.0, c), vert(8.0, 8.0, c)],
            None,
        );
        triangle(
            &mut target,
            clip,
            [vert(3.0, 3.0, c), vert(3.0, 3.0, c), vert(3.0, 3.0, c)],
            None,
        );
        assert!(buf.iter().all(|&p| p == 0));
    }

    #[test]
    fn non_finite_coordinates_do_not_panic() {
        let (w, h) = (8u32, 8u32);
        let mut buf = blank(w, h);
        let mut target = Target::new(&mut buf, w, h).unwrap();
        let clip = full_clip(w, h);
        let c = Color32::WHITE;
        triangle(
            &mut target,
            clip,
            [
                vert(f32::NAN, 0.0, c),
                vert(4.0, f32::INFINITY, c),
                vert(8.0, 8.0, c),
            ],
            None,
        );
    }

    /// Nothing may be written outside the clip rect, even when the triangle covers the
    /// whole target.
    #[test]
    fn the_clip_rect_is_respected_exactly() {
        let (w, h) = (16u32, 16u32);
        let mut buf = blank(w, h);
        let mut target = Target::new(&mut buf, w, h).unwrap();
        let clip = PixelRect {
            min_x: 4,
            min_y: 4,
            max_x: 12,
            max_y: 12,
        };
        let c = Color32::WHITE;
        triangle(
            &mut target,
            clip,
            [vert(0.0, 0.0, c), vert(16.0, 0.0, c), vert(0.0, 16.0, c)],
            None,
        );
        for y in 0..h {
            for x in 0..w {
                let inside = (4..12).contains(&(x as i32)) && (4..12).contains(&(y as i32));
                if !inside {
                    assert_eq!(
                        target.row(y).unwrap()[x as usize],
                        0,
                        "wrote outside clip at ({x},{y})"
                    );
                }
            }
        }
    }

    /// An opaque fragment must land on the destination exactly, with no rounding drift.
    #[test]
    fn opaque_paint_is_exact() {
        assert_eq!(
            blend_over([17.0, 34.0, 51.0, 255.0], pack_rgb(200, 100, 50)),
            pack_rgb(17, 34, 51)
        );
    }

    #[test]
    fn a_fully_transparent_fragment_leaves_the_destination_untouched() {
        let dst = pack_rgb(200, 100, 50);
        assert_eq!(blend_over([0.0, 0.0, 0.0, 0.0], dst), dst);
    }

    #[test]
    fn top_left_rule_classifies_edges_of_a_positively_wound_triangle() {
        // v0=(0,0) v1=(10,0) v2=(0,10) has positive area with y down.
        assert!(is_top_left(10, 0), "horizontal, rightward => top edge");
        assert!(is_top_left(0, -10), "upward => left edge");
        assert!(!is_top_left(-10, 10), "the hypotenuse is neither");
        assert!(!is_top_left(0, 10), "downward => right edge");
    }
}

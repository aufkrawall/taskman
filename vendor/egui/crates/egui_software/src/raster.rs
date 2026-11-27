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

use crate::gamma::TextGamma;
use crate::target::{PixelRect, Target};
use crate::texture::{LcdAtlasSample, Texture};

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
    let scaled = round_exact(v * SUB_ONE as f32);
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
    triangle_impl(target, clip, v, texture, None);
}

/// Rasterize transformed text whose font-atlas mesh contains LCD coverage.
///
/// LCD texels are reduced to their mean coverage before filtering and blended through
/// `gamma` (matching upright LCD text weight and `egui_lcd.wgsl`'s off-grid fallback);
/// color glyphs and ordinary texels in the same atlas pass through unchanged.
pub(crate) fn triangle_gray_lcd(
    target: &mut Target<'_>,
    clip: PixelRect,
    v: [ScreenVertex; 3],
    texture: Option<&Texture>,
    gamma: &TextGamma,
) {
    triangle_impl(target, clip, v, texture, Some(gamma));
}

fn triangle_impl(
    target: &mut Target<'_>,
    clip: PixelRect,
    v: [ScreenVertex; 3],
    texture: Option<&Texture>,
    lcd_gamma: Option<&TextGamma>,
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

    // Fast path: uniform untextured geometry (chart area fills, line strokes, polygons).
    if texture.is_none() && v[0].color == v[1].color && v[1].color == v[2].color {
        let color = v[0].color;
        if color.a() == 0 {
            return;
        }
        if color.a() == 255 {
            // Opaque uniform triangle: direct store, no blending or float barycentrics.
            let packed = crate::target::pack_rgb(color.r(), color.g(), color.b());
            for y in bounds.min_y..bounds.max_y {
                if let Some((xs, xe)) = row_span(row_w, step_x, bounds)
                    && let Some(row) = target.row_mut(y as u32)
                    && let Some(span) = row.get_mut(xs..xe.min(row.len()))
                {
                    span.fill(packed);
                }
                for k in 0..3 {
                    row_w[k] += step_y[k];
                }
            }
            return;
        }

        // Translucent uniform triangle (e.g. chart fills): hoist src and inv_a.
        let c = color.to_array();
        let src = [c[0] as f32, c[1] as f32, c[2] as f32, c[3] as f32];
        let inv_a = 1.0 - src[3] * (1.0 / 255.0);
        for y in bounds.min_y..bounds.max_y {
            if let Some((xs, xe)) = row_span(row_w, step_x, bounds)
                && let Some(row) = target.row_mut(y as u32)
                && let Some(span) = row.get_mut(xs..xe.min(row.len()))
            {
                for dst in span {
                    let d = *dst;
                    let dr = ((d >> 16) & 0xff) as f32;
                    let dg = ((d >> 8) & 0xff) as f32;
                    let db = (d & 0xff) as f32;
                    let r = round_to_u8(src[0] + dr * inv_a);
                    let g = round_to_u8(src[1] + dg * inv_a);
                    let b = round_to_u8(src[2] + db * inv_a);
                    *dst = (r << 16) | (g << 8) | b;
                }
            }
            for k in 0..3 {
                row_w[k] += step_y[k];
            }
        }
        return;
    }

    // General path: textured and/or gouraud-interpolated triangles.
    if let (Some(tex), Some(gamma)) = (texture, lcd_gamma) {
        let uniform_color = v[0].color == v[1].color && v[1].color == v[2].color;
        let [u_sr, u_sg, u_sb, u_ca] = v[0].color.to_srgba_unmultiplied();
        let u_s_lin_r = gamma.linear_u16(u_sr);
        let u_s_lin_g = gamma.linear_u16(u_sg);
        let u_s_lin_b = gamma.linear_u16(u_sb);

        for y in bounds.min_y..bounds.max_y {
            let span = row_span(row_w, step_x, bounds);
            let (Some((xs, xe)), Some(row)) = (span, target.row_mut(y as u32)) else {
                for k in 0..3 {
                    row_w[k] += step_y[k];
                }
                continue;
            };
            let lead = (xs as i64) - bounds.min_x as i64;
            let mut w = [
                row_w[0] + step_x[0] * lead,
                row_w[1] + step_x[1] * lead,
                row_w[2] + step_x[2] * lead,
            ];

            for x in xs..xe {
                let l0 = w[0] as f32 * inv_area;
                let l1 = w[1] as f32 * inv_area;
                let l2 = 1.0 - l0 - l1;
                let u = v[0].u * l0 + v[1].u * l1 + v[2].u * l2;
                let vv = v[0].v * l0 + v[1].v * l1 + v[2].v * l2;

                match tex.sample_lcd_or_ordinary(u, vv) {
                    LcdAtlasSample::LcdCoverage(cov) => {
                        if cov > 0
                            && let Some(dst) = row.get_mut(x)
                        {
                            let (sr, sg, sb, ca, s_lin_r, s_lin_g, s_lin_b) = if uniform_color {
                                (u_sr, u_sg, u_sb, u_ca, u_s_lin_r, u_s_lin_g, u_s_lin_b)
                            } else {
                                let premul = interpolated_vertex_color(&v, l0, l1, l2);
                                let c = Color32::from_rgba_premultiplied(
                                    round_to_u8(premul[0]) as u8,
                                    round_to_u8(premul[1]) as u8,
                                    round_to_u8(premul[2]) as u8,
                                    round_to_u8(premul[3]) as u8,
                                );
                                let [sr, sg, sb, ca] = c.to_srgba_unmultiplied();
                                (
                                    sr,
                                    sg,
                                    sb,
                                    ca,
                                    gamma.linear_u16(sr),
                                    gamma.linear_u16(sg),
                                    gamma.linear_u16(sb),
                                )
                            };
                            if ca > 0 {
                                let d = *dst;
                                let dr = ((d >> 16) & 0xff) as u8;
                                let dg = ((d >> 8) & 0xff) as u8;
                                let db = (d & 0xff) as u8;
                                let r = gamma.blend_channel_alpha_fast(s_lin_r, sr, dr, cov, ca);
                                let g = gamma.blend_channel_alpha_fast(s_lin_g, sg, dg, cov, ca);
                                let b = gamma.blend_channel_alpha_fast(s_lin_b, sb, db, cov, ca);
                                *dst = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                            }
                        }
                    }
                    LcdAtlasSample::Ordinary(texel) => {
                        let t = texel.to_array();
                        if t[3] > 0
                            && let Some(dst) = row.get_mut(x)
                        {
                            let mut src = interpolated_vertex_color(&v, l0, l1, l2);
                            for i in 0..4 {
                                src[i] = src[i] * t[i] as f32 * (1.0 / 255.0);
                            }
                            if src[3] > 0.0 {
                                *dst = blend_over(src, *dst);
                            }
                        }
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
        return;
    }

    for y in bounds.min_y..bounds.max_y {
        let span = row_span(row_w, step_x, bounds);
        let (Some((xs, xe)), Some(row)) = (span, target.row_mut(y as u32)) else {
            for k in 0..3 {
                row_w[k] += step_y[k];
            }
            continue;
        };
        // The edge values at the span's first pixel; inside the span all three are
        // non-negative by construction, so no pixel needs the inside test.
        let lead = (xs as i64) - bounds.min_x as i64;
        let mut w = [
            row_w[0] + step_x[0] * lead,
            row_w[1] + step_x[1] * lead,
            row_w[2] + step_x[2] * lead,
        ];

        for x in xs..xe {
            // Barycentrics. The bias shifted the edge values by at most one unit to
            // implement the fill rule; using the unbiased weights for interpolation
            // would cost a subtract per pixel for a sub-LSB difference in the result,
            // so the biased values are used directly.
            let l0 = w[0] as f32 * inv_area;
            let l1 = w[1] as f32 * inv_area;
            let l2 = 1.0 - l0 - l1;

            let src = shade(&v, l0, l1, l2, texture);
            if src[3] > 0.0
                && let Some(dst) = row.get_mut(x)
            {
                *dst = blend_over(src, *dst);
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

#[inline]
fn interpolated_vertex_color(v: &[ScreenVertex; 3], l0: f32, l1: f32, l2: f32) -> [f32; 4] {
    let c0 = v[0].color.to_array();
    let mut out = [0.0f32; 4];
    if v[0].color == v[1].color && v[1].color == v[2].color {
        // Uniform colour, which is most of what egui emits -- solid fills, and every
        // glyph quad. Interpolating a constant is not free of error: the barycentrics
        // sum to 1 only up to float rounding, so `c*l0 + c*l1 + c*l2` can land an LSB
        // below `c` and produce a pixel that differs from the direct blit. Taking the
        // colour as-is is both exact and cheaper.
        for i in 0..4 {
            out[i] = c0[i] as f32;
        }
    } else {
        let c1 = v[1].color.to_array();
        let c2 = v[2].color.to_array();
        for i in 0..4 {
            out[i] = c0[i] as f32 * l0 + c1[i] as f32 * l1 + c2[i] as f32 * l2;
        }
    }
    out
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
    let mut out = interpolated_vertex_color(v, l0, l1, l2);

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
pub(crate) fn blend_over(src: [f32; 4], dst: u32) -> u32 {
    let inv_a = 1.0 - src[3] * (1.0 / 255.0);
    let dr = ((dst >> 16) & 0xff) as f32;
    let dg = ((dst >> 8) & 0xff) as f32;
    let db = (dst & 0xff) as f32;
    let r = round_to_u8(src[0] + dr * inv_a);
    let g = round_to_u8(src[1] + dg * inv_a);
    let b = round_to_u8(src[2] + db * inv_a);
    (r << 16) | (g << 8) | b
}

/// `x.round().clamp(0.0, 255.0) as u32`, bit for bit, without the libm call.
///
/// Baseline x86-64 has no rounding instruction (`roundss` is SSE4.1), so `f32::round`
/// is a call into libm, and it ran three or four times for every blended pixel and every
/// bilinear sample: 12-14 % of a scrolling frame's whole CPU time went to `roundf`. For
/// `0 < x < 255` the truncation is exact and so is `x - i` (Sterbenz: `i <= x < i + 1 <=
/// 2i`, or `i == 0`), so comparing the fraction against one half reproduces
/// round-half-away-from-zero exactly; NaN and everything at or below zero map to 0 as
/// the clamp did.
#[inline(always)]
pub(crate) fn round_to_u8(x: f32) -> u32 {
    if x >= 255.0 {
        255
    } else if x > 0.0 {
        let i = x as u32;
        i + u32::from(x - i as f32 >= 0.5)
    } else {
        0
    }
}

/// `x.round()` without the libm call, equal to it for every input but the sign of a zero
/// (`-0.3` gives `+0.0`, which converts to the same integer).
///
/// Below 2^23 the truncation and `x - t` are exact, so the fraction decides
/// round-half-away-from-zero exactly; larger magnitudes are already integral and NaN
/// takes the library path. Three of these run per triangle, for its vertices.
#[inline(always)]
fn round_exact(x: f32) -> f32 {
    if x.abs() < 8_388_608.0 {
        let t = x as i32 as f32;
        let f = x - t;
        if f >= 0.5 {
            t + 1.0
        } else if f <= -0.5 {
            t - 1.0
        } else {
            t
        }
    } else {
        x.round()
    }
}

/// The pixels of one row that lie inside the triangle, as `xs..xe` in target columns,
/// clipped to `bounds`; `None` when the row misses it.
///
/// `w` holds the three edge values at the row's first pixel (`bounds.min_x`) and `step_x`
/// what each gains per pixel to the right. A pixel is inside when all three are
/// non-negative, and since each is linear in `x` the boundary of each is one integer
/// division away: this is the same per-pixel test, solved in closed form over exact
/// integers, so the covered pixels are identical. What it saves is the scan. Every
/// triangle used to visit its whole bounding box, and rejecting the outside half of a
/// rect's two triangles, the sliver of a feathered edge's diagonal box, or the dozens of
/// overlapping boxes of a rounded rect's triangle fan was most of the rasterizer's time
/// on a scrolling frame.
#[inline]
fn row_span(w: [i64; 3], step_x: [i64; 3], bounds: PixelRect) -> Option<(usize, usize)> {
    let mut lo = 0i64;
    let mut hi = (bounds.max_x - bounds.min_x) as i64; // exclusive
    for k in 0..3 {
        let (w, s) = (w[k], step_x[k]);
        if s > 0 {
            // w + s*x >= 0  <=>  x >= ceil(-w / s)
            if w < 0 {
                lo = lo.max((-w + s - 1) / s);
            }
        } else if s < 0 {
            // w + s*x >= 0  <=>  x <= floor(w / -s)
            if w < 0 {
                return None;
            }
            hi = hi.min(w / -s + 1);
        } else if w < 0 {
            return None;
        }
    }
    (lo < hi).then(|| {
        let base = bounds.min_x as i64;
        ((base + lo) as usize, (base + hi) as usize)
    })
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

    /// The libm-free rounding must be `round().clamp(0, 255)` bit for bit: every pixel
    /// of the software renderer goes through it, and the GPU-parity tests are only as
    /// good as this.
    #[test]
    fn round_to_u8_matches_round_and_clamp_exactly() {
        let reference = |x: f32| x.round().clamp(0.0, 255.0) as u32;
        let check = |x: f32| assert_eq!(round_to_u8(x), reference(x), "x = {x:?}");
        for special in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -0.0,
            0.0,
            f32::MAX,
        ] {
            check(special);
        }
        // Every 1/1024 step over the range plus margin, and the neighbouring ulps of
        // each half-integer, where round-half-away-from-zero decides.
        for i in -4096..(260 * 1024) {
            check(i as f32 / 1024.0);
        }
        for k in -2..260 {
            let half = k as f32 + 0.5;
            check(half);
            check(f32::from_bits(half.to_bits() + 1));
            check(f32::from_bits(half.to_bits() - 1));
        }
    }

    #[test]
    fn round_exact_matches_round() {
        for special in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, 1e20, -1e20] {
            let (got, want) = (round_exact(special), special.round());
            assert!(
                got == want || (got.is_nan() && want.is_nan()),
                "{special:?}"
            );
        }
        for i in -400_000..400_000 {
            let x = i as f32 / 1024.0;
            assert_eq!(round_exact(x), x.round(), "x = {x:?}");
        }
        for k in -3000..3000 {
            let half = k as f32 + 0.5;
            for x in [
                half,
                f32::from_bits(half.to_bits() + 1),
                f32::from_bits(half.to_bits() - 1),
            ] {
                assert_eq!(round_exact(x), x.round(), "x = {x:?}");
            }
        }
        for big in [8_388_607.5f32, 8_388_608.0, -8_388_607.5, 16_777_215.0] {
            assert_eq!(round_exact(big), big.round(), "x = {big:?}");
        }
    }

    /// Brute force: the pixels of a row with all three edge values non-negative --
    /// exactly the per-pixel test the closed-form span replaced.
    fn covered(w: [i64; 3], step_x: [i64; 3], width: i64) -> Vec<i64> {
        (0..width)
            .filter(|&x| (0..3).all(|k| w[k] + step_x[k] * x >= 0))
            .collect()
    }

    /// `row_span` must cover the same pixels as testing every pixel of the bounding
    /// box, for every sign combination and at the boundaries where an edge value is
    /// exactly zero (the fill rule lives there).
    #[test]
    fn row_span_covers_exactly_the_pixels_the_per_pixel_test_accepts() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |range: i64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % (2 * range as u64 + 1)) as i64 - range
        };
        for case in 0..200_000 {
            let width = 1 + (next(40).abs());
            let min_x = next(20);
            let bounds = PixelRect {
                min_x: min_x as i32,
                min_y: 0,
                max_x: (min_x + width) as i32,
                max_y: 1,
            };
            // Small steps hit exact zeros often; some cases use the 16x scale real
            // edge steps have.
            let scale = if case % 3 == 0 { SUB_ONE as i64 } else { 1 };
            let step_x = [next(5) * scale, next(5) * scale, next(5) * scale];
            let w = [next(120), next(120), next(120)];
            let expected = covered(w, step_x, width);
            let got: Vec<i64> = row_span(w, step_x, bounds)
                .map(|(xs, xe)| (xs as i64 - min_x..xe as i64 - min_x).collect())
                .unwrap_or_default();
            assert_eq!(got, expected, "w={w:?} step_x={step_x:?} width={width}");
        }
    }

    #[test]
    fn top_left_rule_classifies_edges_of_a_positively_wound_triangle() {
        // v0=(0,0) v1=(10,0) v2=(0,10) has positive area with y down.
        assert!(is_top_left(10, 0), "horizontal, rightward => top edge");
        assert!(is_top_left(0, -10), "upward => left edge");
        assert!(!is_top_left(-10, 10), "the hypotenuse is neither");
        assert!(!is_top_left(0, 10), "downward => right edge");
    }

    #[test]
    fn triangle_gray_lcd_applies_text_gamma_to_lcd_texels_and_leaves_color_glyphs_ordinary() {
        use epaint::textures::TextureOptions;

        let lcd_tex = Texture {
            pixels: vec![Color32::from_rgba_premultiplied(128, 128, 128, 0)],
            width: 1,
            height: 1,
            options: TextureOptions::NEAREST,
        };
        let color_tex = Texture {
            pixels: vec![Color32::from_rgba_premultiplied(200, 40, 10, 220)],
            width: 1,
            height: 1,
            options: TextureOptions::NEAREST,
        };
        let tri = [
            ScreenVertex {
                x: 0.0,
                y: 0.0,
                u: 0.5,
                v: 0.5,
                color: Color32::WHITE,
            },
            ScreenVertex {
                x: 4.0,
                y: 0.0,
                u: 0.5,
                v: 0.5,
                color: Color32::WHITE,
            },
            ScreenVertex {
                x: 0.0,
                y: 4.0,
                u: 0.5,
                v: 0.5,
                color: Color32::WHITE,
            },
        ];

        let gamma = TextGamma::new(1.8, 0.5);
        let mut lcd_buf = vec![pack_rgb(0x19, 0x19, 0x19); 16];
        let mut lcd_target = Target::new(&mut lcd_buf, 4, 4).unwrap();
        triangle_gray_lcd(
            &mut lcd_target,
            full_clip(4, 4),
            tri,
            Some(&lcd_tex),
            &gamma,
        );
        let expected_channel = gamma.blend_channel(255, 0x19, 128);
        assert_eq!(
            lcd_buf[0],
            pack_rgb(expected_channel, expected_channel, expected_channel),
            "transformed LCD triangle must blend mean coverage through TextGamma"
        );

        let mut color_buf = vec![pack_rgb(0x19, 0x19, 0x19); 16];
        let mut color_target = Target::new(&mut color_buf, 4, 4).unwrap();
        triangle_gray_lcd(
            &mut color_target,
            full_clip(4, 4),
            tri,
            Some(&color_tex),
            &gamma,
        );
        assert_eq!(
            color_buf[0],
            blend_over([200.0, 40.0, 10.0, 220.0], pack_rgb(0x19, 0x19, 0x19)),
            "premultiplied color glyphs must stay on ordinary blend_over"
        );
    }
}

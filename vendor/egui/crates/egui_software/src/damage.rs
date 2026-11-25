//! Tile-based damage tracking for [`crate::Painter`].
//!
//! Splits the render target into `TILE_SIZE x TILE_SIZE` pixel tiles and
//! computes an order-sensitive 64-bit hash per tile from the clipped shapes
//! touching that tile. Only tiles whose hash changed (or all tiles when
//! `force_full` or global render state changed) are cleared, rasterized, and
//! handed back as disjoint [`PixelRect`]s for `softbuffer::Buffer::present_with_damage`.

use std::sync::Arc;

use ecolor::Color32;
use emath::{GuiRounding as _, Pos2, Rect, Rot2};
use epaint::text::Galley;
use epaint::{
    CircleShape, ClippedShape, ColorMode, CubicBezierShape, EllipseShape, PathShape, PathStroke,
    QuadraticBezierShape, RectShape, Shape, Stroke, StrokeKind, TextShape, TextureId,
};

use crate::span;
use crate::target::{PixelRect, clip_rect_to_pixels};
use crate::texture::TextureStore;
use crate::{ShapeContext, TextMode};

pub const TILE_SIZE: u32 = 64;

const TILE_SEED: u64 = 0x517c_c1b7_2722_0a95;
const MIX_MUL: u64 = 0x9E37_79B9_7F4A_7C15;

/// Result of [`crate::Painter::paint_shapes_damaged`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DamageStats {
    /// Disjoint pixel rectangles that were cleared and repainted this frame.
    /// Empty when the frame produced the exact same tile hashes as the last
    /// presented frame (and `force_full` was false).
    pub dirty_rects: Vec<PixelRect>,
    /// Number of tiles marked dirty this frame.
    pub dirty_tiles: usize,
    /// Total number of tiles covering the target (`tiles_x * tiles_y`).
    pub total_tiles: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameKey {
    pub width: u32,
    pub height: u32,
    pub pixels_per_point_bits: u32,
    pub clear_bgrx: u32,
    pub text_mode: u8,
    pub subpixel: u8,
    pub gamma_bits: u32,
    pub contrast_bits: u32,
    pub round_rects_to_pixels: bool,
    pub round_text_to_pixels: bool,
    pub round_line_segments_to_pixels: bool,
    pub coarse_tessellation_culling: bool,
    pub feathering: bool,
    pub feathering_size_bits: u32,
    pub prerasterized_discs: bool,
    pub debug_paint_clip_rects: bool,
    pub debug_ignore_clip_rects: bool,
    pub debug_paint_text_rects: bool,
    pub bezier_tolerance_bits: u32,
    pub epsilon_bits: u32,
    pub font_tex_size: [usize; 2],
}

impl FrameKey {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        ctx: &ShapeContext,
        clear_bgrx: u32,
        text_mode: TextMode,
        subpixel: epaint::text::SubpixelMode,
        gamma: f32,
        contrast: f32,
    ) -> Self {
        Self {
            width,
            height,
            pixels_per_point_bits: f32_bits(ctx.pixels_per_point),
            clear_bgrx,
            text_mode: match text_mode {
                TextMode::Blit => 0,
                TextMode::Tessellate => 1,
            },
            subpixel: subpixel as u8,
            gamma_bits: f32_bits(gamma),
            contrast_bits: f32_bits(contrast),
            // Every tessellation option that changes pixels for unmodified shapes, not just
            // the ones the app happens to set at startup: a tile hash is only sound if two
            // frames with equal hashes and an equal key really do produce equal pixels.
            // `parallel_tessellation` is absent because it only reorders work, and
            // `validate_meshes` because it only affects meshes that are already invalid.
            round_rects_to_pixels: ctx.options.round_rects_to_pixels,
            round_text_to_pixels: ctx.options.round_text_to_pixels,
            round_line_segments_to_pixels: ctx.options.round_line_segments_to_pixels,
            coarse_tessellation_culling: ctx.options.coarse_tessellation_culling,
            feathering: ctx.options.feathering,
            feathering_size_bits: f32_bits(ctx.options.feathering_size_in_pixels),
            prerasterized_discs: ctx.options.prerasterized_discs,
            // The three debug toggles are all applied after `plan()` hashed the input shapes,
            // so the shape hash cannot see them and a runtime toggle has to invalidate the
            // frame here, or nothing repaints and the new primitives paint outside the tiles
            // that claim to be up to date. Only `debug_paint_text_rects` changes pixels on the
            // default path: `tessellate_shape` injects its stroke while tessellating, and
            // rotated text -- which cannot be blitted -- is tessellated there. The two
            // clip-rect toggles are applied at the end of `tessellate_shapes` instead, which
            // `Blit` reaches only for underlined text (its underline stroke is tessellated
            // there); everything else on that path goes through `tessellate_clipped_shape`.
            // They therefore bite exactly when underlined or rotated text is on screen, which
            // is why all three are keyed.
            debug_paint_clip_rects: ctx.options.debug_paint_clip_rects,
            debug_ignore_clip_rects: ctx.options.debug_ignore_clip_rects,
            debug_paint_text_rects: ctx.options.debug_paint_text_rects,
            bezier_tolerance_bits: f32_bits(ctx.options.bezier_tolerance),
            epsilon_bits: f32_bits(ctx.options.epsilon),
            font_tex_size: ctx.font_tex_size,
        }
    }
}

#[derive(Default)]
pub(crate) struct DamageTracker {
    last_key: Option<FrameKey>,
    tile_hashes: Vec<u64>,
    curr_hashes: Vec<u64>,
    dirty_flags: Vec<bool>,
    // Keep the previous frame's Arc<Galley> alive until after the next frame's
    // tile hashes are computed so `Arc::as_ptr` addresses cannot suffer ABA
    // allocator reuse between consecutive frames. (`Mesh`es are hashed by
    // content instead, because callers rebuild identical `Arc<Mesh>` charts.)
    prev_galleys: Vec<Arc<Galley>>,
    frame_nonce: u64,
}

impl DamageTracker {
    #[expect(clippy::too_many_arguments)]
    pub fn plan(
        &mut self,
        key: FrameKey,
        flat_shapes: &[ClippedShape],
        ctx: &ShapeContext,
        textures: &TextureStore,
        force_full: bool,
        bounds: PixelRect,
        out_shape_bounds: &mut Vec<PixelRect>,
    ) -> DamageStats {
        out_shape_bounds.clear();
        out_shape_bounds.reserve(flat_shapes.len());

        let tiles_x = key.width.div_ceil(TILE_SIZE) as usize;
        let tiles_y = key.height.div_ceil(TILE_SIZE) as usize;
        let total_tiles = tiles_x * tiles_y;

        if total_tiles == 0 {
            self.last_key = Some(key);
            self.tile_hashes.clear();
            self.prev_galleys.clear();
            return DamageStats::default();
        }

        self.frame_nonce = self.frame_nonce.wrapping_add(1).max(1);
        let frame_nonce = self.frame_nonce;

        self.curr_hashes.clear();
        self.curr_hashes.resize(total_tiles, TILE_SEED);

        let mut curr_galleys = Vec::new();

        for clipped in flat_shapes {
            let sbounds = shape_pixel_bounds(clipped, ctx, bounds);
            out_shape_bounds.push(sbounds);
            if sbounds.is_empty() {
                continue;
            }

            if let Shape::Text(t) = &clipped.shape {
                curr_galleys.push(Arc::clone(&t.galley));
            }

            let hash = hash_clipped_shape(clipped, sbounds, textures, frame_nonce);
            let tx0 = (sbounds.min_x.max(0) as u32 / TILE_SIZE) as usize;
            let ty0 = (sbounds.min_y.max(0) as u32 / TILE_SIZE) as usize;
            let tx1 = ((sbounds.max_x.max(0) as u32).div_ceil(TILE_SIZE) as usize).min(tiles_x);
            let ty1 = ((sbounds.max_y.max(0) as u32).div_ceil(TILE_SIZE) as usize).min(tiles_y);
            for ty in ty0..ty1 {
                let row = &mut self.curr_hashes[ty * tiles_x..ty * tiles_x + tiles_x];
                for tile_h in &mut row[tx0..tx1] {
                    *tile_h = tile_h.rotate_left(13).wrapping_mul(MIX_MUL) ^ hash;
                }
            }
        }

        let full_invalidate =
            force_full || self.last_key != Some(key) || self.tile_hashes.len() != total_tiles;

        self.dirty_flags.clear();
        let dirty_tiles = if full_invalidate {
            self.dirty_flags.resize(total_tiles, true);
            total_tiles
        } else {
            let mut count = 0usize;
            self.dirty_flags
                .extend(
                    self.curr_hashes
                        .iter()
                        .zip(&self.tile_hashes)
                        .map(|(a, b)| {
                            let dirty = a != b;
                            if dirty {
                                count += 1;
                            }
                            dirty
                        }),
                );
            count
        };

        let dirty_rects = if dirty_tiles == 0 {
            Vec::new()
        } else if dirty_tiles == total_tiles {
            vec![bounds]
        } else {
            merge_dirty_tiles(&self.dirty_flags, tiles_x, tiles_y, key.width, key.height)
        };

        std::mem::swap(&mut self.tile_hashes, &mut self.curr_hashes);
        self.last_key = Some(key);
        self.prev_galleys = curr_galleys;

        DamageStats {
            dirty_rects,
            dirty_tiles,
            total_tiles,
        }
    }
}

/// Merge dirty tiles into strictly disjoint pixel rectangles.
///
/// Row runs `[tx0, tx1)` of contiguous dirty tiles are merged vertically across
/// consecutive tile rows whenever the `[tx0, tx1)` span matches, so every dirty
/// tile belongs to exactly one output rectangle.
fn merge_dirty_tiles(
    dirty: &[bool],
    tiles_x: usize,
    tiles_y: usize,
    width: u32,
    height: u32,
) -> Vec<PixelRect> {
    #[derive(Clone, Copy)]
    struct OpenRect {
        tx0: usize,
        tx1: usize,
        ty0: usize,
        ty1: usize,
    }

    let mut open: Vec<OpenRect> = Vec::new();
    let mut next_open: Vec<OpenRect> = Vec::new();
    let mut finished: Vec<OpenRect> = Vec::new();

    for ty in 0..tiles_y {
        let row = &dirty[ty * tiles_x..(ty + 1) * tiles_x];
        next_open.clear();
        let mut tx = 0usize;
        while tx < tiles_x {
            if !row[tx] {
                tx += 1;
                continue;
            }
            let tx0 = tx;
            while tx < tiles_x && row[tx] {
                tx += 1;
            }
            let tx1 = tx;
            if let Some(pos) = open.iter().position(|r| r.tx0 == tx0 && r.tx1 == tx1) {
                let mut rect = open.swap_remove(pos);
                rect.ty1 = ty + 1;
                next_open.push(rect);
            } else {
                next_open.push(OpenRect {
                    tx0,
                    tx1,
                    ty0: ty,
                    ty1: ty + 1,
                });
            }
        }
        finished.append(&mut open);
        std::mem::swap(&mut open, &mut next_open);
    }
    finished.append(&mut open);

    finished
        .into_iter()
        .map(|r| PixelRect {
            min_x: (r.tx0 as i32) * (TILE_SIZE as i32),
            min_y: (r.ty0 as i32) * (TILE_SIZE as i32),
            max_x: ((r.tx1 as i32) * (TILE_SIZE as i32)).min(width as i32),
            max_y: ((r.ty1 as i32) * (TILE_SIZE as i32)).min(height as i32),
        })
        .collect()
}

fn shape_pixel_bounds(
    clipped: &ClippedShape,
    ctx: &ShapeContext,
    target_bounds: PixelRect,
) -> PixelRect {
    let ppp = ctx.pixels_per_point;
    let clip = clip_rect_to_pixels(clipped.clip_rect, ppp, target_bounds);
    if clip.is_empty() || !ppp.is_finite() || ppp <= 0.0 {
        return PixelRect::NOTHING;
    }

    let points_bounds = |rect: Rect, margin_px: i32| -> PixelRect {
        if !rect.is_positive() && (rect.width() < 0.0 || rect.height() < 0.0) {
            return PixelRect::NOTHING;
        }
        if !rect.min.x.is_finite()
            || !rect.min.y.is_finite()
            || !rect.max.x.is_finite()
            || !rect.max.y.is_finite()
        {
            return clip;
        }
        // Clamp in points before the cast, not after: `f32 as i32` saturates at
        // both ends, so the margin added below would wrap a saturated edge to the
        // opposite side. That is a debug-build panic and, once wrapped, a rect
        // covering the whole target becomes an empty one -- which the paint loop
        // then skips, so the shape silently disappears. Ten million physical
        // pixels is far outside any real target and far inside `i32`, and the
        // intersect with the clip below is what decides the actual edges.
        const PX_LIMIT: f32 = 1.0e7;
        let min_x = (rect.min.x * ppp).clamp(-PX_LIMIT, PX_LIMIT).floor() as i32 - margin_px;
        let min_y = (rect.min.y * ppp).clamp(-PX_LIMIT, PX_LIMIT).floor() as i32 - margin_px;
        let max_x = (rect.max.x * ppp).clamp(-PX_LIMIT, PX_LIMIT).ceil() as i32 + margin_px;
        let max_y = (rect.max.y * ppp).clamp(-PX_LIMIT, PX_LIMIT).ceil() as i32 + margin_px;
        PixelRect {
            min_x,
            min_y,
            max_x,
            max_y,
        }
        .intersect(clip)
    };

    match &clipped.shape {
        Shape::Noop | Shape::Vec(_) => PixelRect::NOTHING,
        Shape::Rect(r) => {
            if let Some(fast) = span::fast_rect(r, ppp, ctx.options.round_rects_to_pixels) {
                return fast.intersect(clip);
            }
            if r.fill == Color32::TRANSPARENT && r.stroke.is_empty() && r.brush.is_none() {
                return PixelRect::NOTHING;
            }
            let stroke_expand = match r.stroke_kind {
                StrokeKind::Inside => 0.0,
                StrokeKind::Middle => r.stroke.width * 0.5,
                StrokeKind::Outside => r.stroke.width,
            };
            let expand = stroke_expand + r.blur_width + 2.0 / ppp;
            let base = r.rect.expand(expand);
            let pts = if r.angle == 0.0 {
                base
            } else {
                let center = r.rect.center();
                Rect::from_center_size(Pos2::ZERO, base.size())
                    .rotate_bb(Rot2::from_angle(r.angle))
                    .translate(center.to_vec2())
            };
            points_bounds(pts, 2)
        }
        Shape::Text(t) => {
            if t.galley.is_empty() || t.opacity_factor <= 0.0 {
                return PixelRect::NOTHING;
            }
            let galley_pos = if ctx.options.round_text_to_pixels {
                t.pos.round_to_pixels(ppp)
            } else {
                t.pos
            };
            let pts = t
                .galley
                .mesh_bounds
                .rotate_bb(Rot2::from_angle(t.angle))
                .translate(galley_pos.to_vec2())
                .expand(t.underline.width + 1.0);
            points_bounds(pts, 2)
        }
        Shape::Mesh(m) => {
            if m.indices.is_empty() {
                return PixelRect::NOTHING;
            }
            points_bounds(m.calc_bounds(), 2)
        }
        Shape::Circle(c) => {
            let r = c.visual_bounding_rect();
            if r.is_negative() {
                PixelRect::NOTHING
            } else {
                points_bounds(r.expand(2.0 / ppp), 2)
            }
        }
        Shape::Ellipse(e) => {
            let r = e.visual_bounding_rect();
            if r.is_negative() {
                PixelRect::NOTHING
            } else {
                points_bounds(r.expand(2.0 / ppp), 2)
            }
        }
        Shape::LineSegment { points, stroke } => {
            if stroke.is_empty() {
                PixelRect::NOTHING
            } else {
                let r = Rect::from_two_pos(points[0], points[1]).expand(stroke.width + 2.0 / ppp);
                points_bounds(r, 2)
            }
        }
        Shape::Path(p) => {
            let r = p.visual_bounding_rect();
            if r.is_negative() {
                PixelRect::NOTHING
            } else {
                points_bounds(r.expand(p.stroke.width + 2.0 / ppp), 2)
            }
        }
        Shape::QuadraticBezier(b) => {
            let r = b.visual_bounding_rect();
            if r.is_negative() {
                PixelRect::NOTHING
            } else {
                points_bounds(r.expand(b.stroke.width + 2.0 / ppp), 2)
            }
        }
        Shape::CubicBezier(b) => {
            if b.fill == Color32::TRANSPARENT && b.stroke.is_empty() {
                PixelRect::NOTHING
            } else {
                let r = Rect::from_points(&b.points).expand(b.stroke.width + 2.0 / ppp);
                points_bounds(r, 2)
            }
        }
        Shape::Callback(cb) => points_bounds(cb.rect, 0),
    }
}

struct Hasher64(u64);

impl Hasher64 {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0 ^ v).wrapping_mul(0x1000_0000_01b3);
        self.0 ^= self.0 >> 32;
    }

    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.write_u64(u64::from(v));
    }

    #[inline]
    fn write_u8(&mut self, v: u8) {
        self.write_u64(u64::from(v));
    }

    #[inline]
    fn write_f32(&mut self, v: f32) {
        self.write_u32(f32_bits(v));
    }

    #[inline]
    fn write_pos2(&mut self, p: Pos2) {
        self.write_f32(p.x);
        self.write_f32(p.y);
    }

    #[inline]
    fn write_rect(&mut self, r: Rect) {
        self.write_pos2(r.min);
        self.write_pos2(r.max);
    }

    #[inline]
    fn write_color(&mut self, c: Color32) {
        self.write_u32(u32::from_le_bytes(c.to_array()));
    }

    #[inline]
    fn write_stroke(&mut self, s: Stroke) {
        self.write_f32(s.width);
        self.write_color(s.color);
    }

    #[inline]
    fn write_path_stroke(&mut self, s: &PathStroke, frame_nonce: u64) {
        self.write_f32(s.width);
        self.write_u8(s.kind as u8);
        match &s.color {
            ColorMode::Solid(c) => {
                self.write_u8(0);
                self.write_color(*c);
            }
            ColorMode::UV(_) => {
                self.write_u8(1);
                self.write_u64(frame_nonce);
            }
        }
    }

    #[inline]
    fn write_texture_id(&mut self, id: TextureId, textures: &TextureStore) {
        match id {
            TextureId::Managed(n) => {
                self.write_u8(0);
                self.write_u64(n);
            }
            TextureId::User(n) => {
                self.write_u8(1);
                self.write_u64(n);
            }
        }
        self.write_u64(textures.generation(id));
    }
}

#[inline]
fn f32_bits(v: f32) -> u32 {
    if v == 0.0 { 0 } else { v.to_bits() }
}

fn hash_clipped_shape(
    clipped: &ClippedShape,
    sbounds: PixelRect,
    textures: &TextureStore,
    frame_nonce: u64,
) -> u64 {
    let mut h = Hasher64::new();
    h.write_u32(sbounds.min_x as u32);
    h.write_u32(sbounds.min_y as u32);
    h.write_u32(sbounds.max_x as u32);
    h.write_u32(sbounds.max_y as u32);

    match &clipped.shape {
        Shape::Noop => h.write_u8(0),
        Shape::Vec(_) => h.write_u8(1),
        Shape::Rect(RectShape {
            rect,
            corner_radius,
            fill,
            stroke,
            stroke_kind,
            round_to_pixels,
            blur_width,
            brush,
            angle,
        }) => {
            h.write_u8(2);
            h.write_rect(*rect);
            h.write_u8(corner_radius.nw);
            h.write_u8(corner_radius.ne);
            h.write_u8(corner_radius.sw);
            h.write_u8(corner_radius.se);
            h.write_color(*fill);
            h.write_stroke(*stroke);
            h.write_u8(*stroke_kind as u8);
            h.write_u8(match round_to_pixels {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            });
            h.write_f32(*blur_width);
            h.write_f32(*angle);
            match brush {
                None => h.write_u8(0),
                Some(b) => {
                    h.write_u8(1);
                    h.write_texture_id(b.fill_texture_id, textures);
                    h.write_rect(b.uv);
                }
            }
        }
        Shape::Text(TextShape {
            pos,
            galley,
            underline,
            fallback_color,
            override_text_color,
            opacity_factor,
            angle,
        }) => {
            h.write_u8(3);
            // `clip_points` affects per-row coarse_tessellation_culling in `paint_text`.
            h.write_rect(clipped.clip_rect);
            h.write_pos2(*pos);
            h.write_u64(Arc::as_ptr(galley) as usize as u64);
            h.write_stroke(*underline);
            h.write_color(*fallback_color);
            match override_text_color {
                None => h.write_u8(0),
                Some(c) => {
                    h.write_u8(1);
                    h.write_color(*c);
                }
            }
            h.write_f32(*opacity_factor);
            h.write_f32(*angle);
            h.write_texture_id(TextureId::Managed(0), textures);
        }
        Shape::Mesh(mesh) => {
            h.write_u8(4);
            h.write_texture_id(mesh.texture_id, textures);
            h.write_u64(mesh.indices.len() as u64);
            for &idx in &mesh.indices {
                h.write_u32(idx);
            }
            h.write_u64(mesh.vertices.len() as u64);
            for v in &mesh.vertices {
                h.write_pos2(v.pos);
                h.write_pos2(v.uv);
                h.write_color(v.color);
            }
        }
        Shape::Circle(CircleShape {
            center,
            radius,
            fill,
            stroke,
        }) => {
            h.write_u8(5);
            h.write_pos2(*center);
            h.write_f32(*radius);
            h.write_color(*fill);
            h.write_stroke(*stroke);
            // A small filled circle can be tessellated through a prerasterized disc that
            // lives in the font atlas, so its pixels may depend on that atlas even though the
            // shape names no texture. Hashing the atlas here keeps that invariant local
            // instead of relying on the atlas size and gamma/contrast being keyed too.
            h.write_texture_id(TextureId::Managed(0), textures);
        }
        Shape::Ellipse(EllipseShape {
            center,
            radius,
            fill,
            stroke,
            angle,
        }) => {
            h.write_u8(6);
            h.write_pos2(*center);
            h.write_f32(radius.x);
            h.write_f32(radius.y);
            h.write_color(*fill);
            h.write_stroke(*stroke);
            h.write_f32(*angle);
        }
        Shape::LineSegment { points, stroke } => {
            h.write_u8(7);
            h.write_pos2(points[0]);
            h.write_pos2(points[1]);
            h.write_stroke(*stroke);
        }
        Shape::Path(PathShape {
            points,
            closed,
            fill,
            stroke,
        }) => {
            h.write_u8(8);
            h.write_u64(points.len() as u64);
            for &p in points {
                h.write_pos2(p);
            }
            h.write_u8(u8::from(*closed));
            h.write_color(*fill);
            h.write_path_stroke(stroke, frame_nonce);
        }
        Shape::QuadraticBezier(QuadraticBezierShape {
            points,
            closed,
            fill,
            stroke,
        }) => {
            h.write_u8(9);
            for &p in points {
                h.write_pos2(p);
            }
            h.write_u8(u8::from(*closed));
            h.write_color(*fill);
            h.write_path_stroke(stroke, frame_nonce);
        }
        Shape::CubicBezier(CubicBezierShape {
            points,
            closed,
            fill,
            stroke,
        }) => {
            h.write_u8(10);
            for &p in points {
                h.write_pos2(p);
            }
            h.write_u8(u8::from(*closed));
            h.write_color(*fill);
            h.write_path_stroke(stroke, frame_nonce);
        }
        Shape::Callback(cb) => {
            h.write_u8(11);
            h.write_rect(cb.rect);
        }
    }
    h.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Painter, Target};
    use emath::pos2;
    use epaint::{
        ColorImage, CornerRadius, ImageDelta, TessellationOptions, textures::TextureOptions,
    };

    const W: u32 = 256;
    const H: u32 = 192;
    const BG: Color32 = Color32::from_rgb(0x19, 0x19, 0x19);

    fn shape_ctx() -> ShapeContext {
        ShapeContext {
            pixels_per_point: 1.0,
            options: TessellationOptions::default(),
            font_tex_size: [1, 1],
            prepared_discs: Vec::new(),
        }
    }

    fn full_clip() -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(W as f32, H as f32))
    }

    fn rect_shape(x0: f32, y0: f32, x1: f32, y1: f32, fill: Color32, radius: u8) -> ClippedShape {
        ClippedShape {
            clip_rect: full_clip(),
            shape: Shape::rect_filled(
                Rect::from_min_max(pos2(x0, y0), pos2(x1, y1)),
                CornerRadius::same(radius),
                fill,
            ),
        }
    }

    fn clipped_rect(clip: Rect, rect: Rect, fill: Color32) -> ClippedShape {
        ClippedShape {
            clip_rect: clip,
            shape: Shape::rect_filled(rect, CornerRadius::ZERO, fill),
        }
    }

    fn setup_painter() -> Painter {
        let mut p = Painter::new();
        p.set_texture(
            TextureId::Managed(0),
            &ImageDelta::full(
                ColorImage::new([1, 1], vec![Color32::WHITE]),
                TextureOptions::NEAREST,
            ),
        );
        p
    }

    /// With no shapes at all every tile hash is identical from frame to frame, so only the
    /// frame key can force the repaint. Anything that changes pixels but is missing from
    /// the key leaves stale tiles behind.
    #[test]
    fn global_render_state_changes_force_full_damage() {
        let mut painter = setup_painter();
        let mut ctx = shape_ctx();
        let mut buf = vec![0u32; (W * H) as usize];
        let mut target = Target::new(&mut buf, W, H).unwrap();

        let first = painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), BG, true);
        assert_eq!(first.dirty_tiles, first.total_tiles);
        let same = painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), BG, false);
        assert_eq!(same.dirty_tiles, 0, "an identical frame must not repaint");

        // Each step below changes exactly one input of the frame key, so the full
        // invalidation can only come from that input.
        ctx.pixels_per_point = 2.0;
        let scaled = painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), BG, false);
        assert_eq!(scaled.dirty_tiles, scaled.total_tiles, "pixels per point");

        let white =
            painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), Color32::WHITE, false);
        assert_eq!(white.dirty_tiles, white.total_tiles, "clear colour");

        ctx.options.feathering = false;
        let plain =
            painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), Color32::WHITE, false);
        assert_eq!(plain.dirty_tiles, plain.total_tiles, "feathering");

        ctx.font_tex_size = [2048, 64];
        let atlas =
            painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), Color32::WHITE, false);
        assert_eq!(atlas.dirty_tiles, atlas.total_tiles, "font atlas size");

        ctx.options.bezier_tolerance = 0.5;
        let curves =
            painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), Color32::WHITE, false);
        assert_eq!(curves.dirty_tiles, curves.total_tiles, "bezier tolerance");

        ctx.options.epsilon = 1.0e-3;
        let epsilon =
            painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), Color32::WHITE, false);
        assert_eq!(epsilon.dirty_tiles, epsilon.total_tiles, "float epsilon");

        // The debug toggles inject primitives that no input shape hash covers, so a toggle
        // missing from the key leaves them on tiles the hashes still call clean.
        ctx.options.debug_paint_clip_rects = true;
        let clip =
            painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), Color32::WHITE, false);
        assert_eq!(clip.dirty_tiles, clip.total_tiles, "clip rects");

        ctx.options.debug_ignore_clip_rects = true;
        let ignore =
            painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), Color32::WHITE, false);
        assert_eq!(ignore.dirty_tiles, ignore.total_tiles, "ignore clips");

        ctx.options.debug_paint_text_rects = true;
        let text =
            painter.paint_shapes_damaged(&mut target, &ctx, Vec::new(), Color32::WHITE, false);
        assert_eq!(text.dirty_tiles, text.total_tiles, "text rects");
    }

    /// Damage rects are physical pixels and must stay inside the target even when the window
    /// size is not a multiple of the tile size: the present path turns each one into a
    /// `softbuffer::Rect`, where an out-of-range rect is a present error, and a gap would
    /// leave painted tiles unpresented.
    #[test]
    fn odd_sized_targets_get_in_bounds_disjoint_damage_rects() {
        const OW: u32 = 200; // four tiles wide; the last one is 8 px
        const OH: u32 = 130; // three tile rows; the last one is 2 px
        let clip = Rect::from_min_max(pos2(0.0, 0.0), pos2(OW as f32, OH as f32));
        let bg = clipped_rect(clip, clip, Color32::from_rgb(30, 30, 30));
        let dot = Rect::from_min_max(pos2(188.0, 118.0), pos2(200.0, 130.0));
        let dot_a = clipped_rect(clip, dot, Color32::from_rgb(60, 60, 60));
        let dot_b = clipped_rect(clip, dot, Color32::from_rgb(0, 120, 212));
        let frame_a = vec![bg.clone(), dot_a];
        let frame_b = vec![bg, dot_b];

        let mut painter = setup_painter();
        let ctx = shape_ctx();
        let mut buf = vec![0u32; (OW * OH) as usize];
        {
            let mut target = Target::new(&mut buf, OW, OH).unwrap();
            let first = painter.paint_shapes_damaged(&mut target, &ctx, frame_a, BG, true);
            assert_eq!(first.dirty_tiles, first.total_tiles);
            assert_eq!(first.total_tiles, 12, "4 x 3 tiles at 200 x 130");

            let second =
                painter.paint_shapes_damaged(&mut target, &ctx, frame_b.clone(), BG, false);
            assert!(
                second.dirty_tiles > 0 && second.dirty_tiles < second.total_tiles,
                "expected partial damage, got {}/{} tiles",
                second.dirty_tiles,
                second.total_tiles
            );
            let mut clipped_corner = false;
            for rect in &second.dirty_rects {
                assert!(!rect.is_empty(), "empty damage rect {rect:?}");
                assert!(rect.min_x >= 0 && rect.min_y >= 0, "negative {rect:?}");
                assert!(
                    rect.max_x <= OW as i32 && rect.max_y <= OH as i32,
                    "past the edge {rect:?}"
                );
                clipped_corner |= rect.max_x == OW as i32 && rect.max_y == OH as i32;
            }
            assert!(clipped_corner, "the clipped corner tile is missing");
            for (i, a) in second.dirty_rects.iter().enumerate() {
                for b in &second.dirty_rects[i + 1..] {
                    assert!(a.intersect(*b).is_empty(), "{a:?} overlaps {b:?}");
                }
            }
        }

        let mut full_painter = setup_painter();
        let mut full_buf = vec![0u32; (OW * OH) as usize];
        {
            let mut target = Target::new(&mut full_buf, OW, OH).unwrap();
            Painter::clear(&mut target, BG);
            full_painter.paint_shapes(&mut target, &ctx, frame_b);
        }

        assert_eq!(
            buf, full_buf,
            "a damaged repaint at an odd size must match a full repaint bit-for-bit"
        );
    }

    /// An edge past the `i32` pixel range must still report the clip, not vanish.
    ///
    /// `f32 as i32` saturates, so the two-pixel margin added after the cast wraps a
    /// saturated edge to the far side; the intersect then makes the rect empty, the
    /// damaged paint skips the shape, and only a full repaint ever shows it. A debug
    /// build panics on the same arithmetic instead.
    #[test]
    fn a_shape_beyond_the_i32_pixel_range_still_covers_the_target() {
        let ctx = shape_ctx();
        let bounds = PixelRect {
            min_x: 0,
            min_y: 0,
            max_x: W as i32,
            max_y: H as i32,
        };

        // Rounded corners keep the rect off `span::fast_rect`, whose casts saturate and add
        // no margin afterwards; this is the general `points_bounds` path, which adds a
        // two-pixel margin and therefore clamps in points before the cast. The second case
        // saturates only the `max` edges, so it catches a clamp applied to one side only.
        for rect in [
            Rect::from_min_max(pos2(-3.0e9, -3.0e9), pos2(3.0e9, 3.0e9)),
            Rect::from_min_max(pos2(0.0, 0.0), pos2(3.0e9, 3.0e9)),
        ] {
            let clipped = ClippedShape {
                clip_rect: full_clip(),
                shape: Shape::rect_filled(rect, CornerRadius::same(4), Color32::WHITE),
            };
            assert_eq!(
                shape_pixel_bounds(&clipped, &ctx, bounds),
                bounds,
                "{rect:?} must report the whole target"
            );
        }

        // The clamp must not drag a genuinely off-screen rect into view.
        let off_screen = ClippedShape {
            clip_rect: full_clip(),
            shape: Shape::rect_filled(
                Rect::from_min_max(pos2(3.0e9, 100.0), pos2(3.1e9, 200.0)),
                CornerRadius::same(4),
                Color32::WHITE,
            ),
        };
        assert!(
            shape_pixel_bounds(&off_screen, &ctx, bounds).is_empty(),
            "a rect far to the right of the target is still empty"
        );
    }

    /// A mesh's pixels come from its texture, so a patch to that texture has to re-dirty the
    /// tiles under the mesh even though the mesh itself is unchanged.
    #[test]
    fn patching_a_texture_dirties_the_tiles_that_draw_it() {
        let mut painter = setup_painter();
        let texture = TextureId::User(7);
        let upload = ImageDelta::full(
            ColorImage::new([2, 2], vec![Color32::RED; 4]),
            TextureOptions::NEAREST,
        );
        painter.set_texture(texture, &upload);
        let patch = ImageDelta::partial(
            [0, 0],
            ColorImage::new([1, 1], vec![Color32::GREEN]),
            TextureOptions::NEAREST,
        );
        let ctx = shape_ctx();
        let quad = Rect::from_min_max(pos2(0.0, 0.0), pos2(64.0, 64.0));
        let mut mesh = epaint::Mesh::default();
        mesh.add_colored_rect(quad, Color32::WHITE);
        mesh.texture_id = texture;
        let shapes = vec![ClippedShape {
            clip_rect: full_clip(),
            shape: Shape::Mesh(mesh.into()),
        }];

        let mut buf = vec![0u32; (W * H) as usize];
        let mut target = Target::new(&mut buf, W, H).unwrap();
        painter.paint_shapes_damaged(&mut target, &ctx, shapes.clone(), BG, true);
        let same = painter.paint_shapes_damaged(&mut target, &ctx, shapes.clone(), BG, false);
        assert_eq!(same.dirty_tiles, 0, "an identical frame must not repaint");

        painter.set_texture(texture, &patch);
        let patched = painter.paint_shapes_damaged(&mut target, &ctx, shapes, BG, false);
        assert!(
            patched.dirty_tiles > 0,
            "a texture patch left the mesh's tiles clean"
        );
    }

    #[test]
    fn identical_frames_produce_zero_dirty_tiles() {
        let mut painter = setup_painter();
        let ctx = shape_ctx();
        let shapes = vec![
            rect_shape(0.0, 0.0, 256.0, 192.0, Color32::from_rgb(30, 30, 30), 0),
            rect_shape(10.0, 10.0, 80.0, 40.0, Color32::from_rgb(0, 120, 212), 4),
        ];

        let mut buf = vec![0u32; (W * H) as usize];
        let mut target = Target::new(&mut buf, W, H).unwrap();

        let s1 = painter.paint_shapes_damaged(&mut target, &ctx, shapes.clone(), BG, true);
        assert_eq!(s1.dirty_tiles, s1.total_tiles);

        let s2 = painter.paint_shapes_damaged(&mut target, &ctx, shapes, BG, false);
        assert_eq!(s2.dirty_tiles, 0);
        assert!(s2.dirty_rects.is_empty());
    }

    #[test]
    fn changing_one_row_dirties_only_overlapping_tiles_and_matches_full_paint() {
        let mut dmg_painter = setup_painter();
        let mut full_painter = setup_painter();
        let ctx = shape_ctx();

        let frame_a = vec![
            rect_shape(0.0, 0.0, 256.0, 192.0, Color32::from_rgb(30, 30, 30), 0),
            rect_shape(10.0, 10.0, 100.0, 30.0, Color32::from_rgb(60, 60, 60), 4),
            rect_shape(10.0, 140.0, 120.0, 170.0, Color32::from_rgb(80, 80, 80), 4),
        ];
        // Frame B changes only the bottom rounded rect + adds a circle inside it.
        let frame_b = vec![
            rect_shape(0.0, 0.0, 256.0, 192.0, Color32::from_rgb(30, 30, 30), 0),
            rect_shape(10.0, 10.0, 100.0, 30.0, Color32::from_rgb(60, 60, 60), 4),
            rect_shape(10.0, 140.0, 120.0, 170.0, Color32::from_rgb(0, 120, 212), 4),
            ClippedShape {
                clip_rect: full_clip(),
                shape: Shape::circle_filled(pos2(115.0, 155.0), 8.0, Color32::WHITE),
            },
        ];

        let mut dmg_buf = vec![0u32; (W * H) as usize];
        {
            let mut target = Target::new(&mut dmg_buf, W, H).unwrap();
            let s1 = dmg_painter.paint_shapes_damaged(&mut target, &ctx, frame_a, BG, true);
            assert_eq!(s1.dirty_tiles, s1.total_tiles);

            let s2 =
                dmg_painter.paint_shapes_damaged(&mut target, &ctx, frame_b.clone(), BG, false);
            assert!(
                s2.dirty_tiles > 0 && s2.dirty_tiles < s2.total_tiles,
                "expected partial damage, got {}/{} tiles",
                s2.dirty_tiles,
                s2.total_tiles
            );
            // Row y=10..30 sits in tile row 0 (y=0..64), which must stay clean.
            assert!(
                s2.dirty_rects.iter().all(|r| r.min_y >= 128),
                "unexpected top-row damage: {:?}",
                s2.dirty_rects
            );
        }

        let mut full_buf = vec![0u32; (W * H) as usize];
        {
            let mut target = Target::new(&mut full_buf, W, H).unwrap();
            Painter::clear(&mut target, BG);
            full_painter.paint_shapes(&mut target, &ctx, frame_b);
        }

        assert_eq!(
            dmg_buf, full_buf,
            "damaged repaint over frame A must match full repaint of frame B bit-for-bit"
        );
    }

    #[test]
    fn draw_order_swap_dirties_overlap_and_matches_full_paint() {
        let mut dmg_painter = setup_painter();
        let mut full_painter = setup_painter();
        let ctx = shape_ctx();

        let red = rect_shape(40.0, 40.0, 120.0, 120.0, Color32::RED, 0);
        let green = rect_shape(60.0, 60.0, 140.0, 140.0, Color32::GREEN, 0);

        let frame_a = vec![red.clone(), green.clone()];
        let frame_b = vec![green, red];

        let mut dmg_buf = vec![0u32; (W * H) as usize];
        {
            let mut target = Target::new(&mut dmg_buf, W, H).unwrap();
            dmg_painter.paint_shapes_damaged(&mut target, &ctx, frame_a, BG, true);
            let s2 =
                dmg_painter.paint_shapes_damaged(&mut target, &ctx, frame_b.clone(), BG, false);
            assert!(s2.dirty_tiles > 0);
        }

        let mut full_buf = vec![0u32; (W * H) as usize];
        {
            let mut target = Target::new(&mut full_buf, W, H).unwrap();
            Painter::clear(&mut target, BG);
            full_painter.paint_shapes(&mut target, &ctx, frame_b);
        }

        assert_eq!(dmg_buf, full_buf);
    }

    #[test]
    fn merge_dirty_tiles_produces_disjoint_rects_covering_exact_tiles() {
        // 4x3 grid of tiles (256x192)
        let dirty = vec![
            true, true, false, false, // row 0: [0..2)
            true, true, false, true, // row 1: [0..2) (merges with row 0!) + [3..4)
            false, false, false, true, // row 2: [3..4) (merges with row 1!)
        ];
        let rects = merge_dirty_tiles(&dirty, 4, 3, W, H);
        assert_eq!(rects.len(), 2);
        // Verify disjointness and exact pixel coverage.
        for ty in 0..3 {
            for tx in 0..4 {
                let cx = (tx * TILE_SIZE as usize + 1) as i32;
                let cy = (ty * TILE_SIZE as usize + 1) as i32;
                let hits = rects
                    .iter()
                    .filter(|r| cx >= r.min_x && cx < r.max_x && cy >= r.min_y && cy < r.max_y)
                    .count();
                let want = usize::from(dirty[ty * 4 + tx]);
                assert_eq!(hits, want, "tile ({tx}, {ty}) coverage mismatch");
            }
        }
    }
}

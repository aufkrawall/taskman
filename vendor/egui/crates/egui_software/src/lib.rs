//! A CPU (software) renderer for [`egui`](https://github.com/emilk/egui).
//!
//! This crate paints egui's output into a plain 32-bit pixel buffer with no GPU, no
//! driver, and no shader compiler. It exists for taskman, where two requirements meet:
//!
//! * **Sub-pixel (`ClearType`) text.** Per-channel glyph coverage needs per-channel alpha
//!   blending. On a GPU that means dual-source blending, which egui's backends do not
//!   have; on a CPU it is a per-channel multiply.
//! * **No GPU at all**, without falling back to a software *driver* like WARP or
//!   lavapipe, which emulate a whole graphics stack and cost orders of magnitude more
//!   than drawing a 2D UI needs to.
//!
//! # Correctness strategy
//!
//! This renderer is a reimplementation of `egui_glow`'s pipeline, not a new one. Every
//! decision that could differ -- the gamma-space multiply, premultiplied `ONE /
//! ONE_MINUS_SRC_ALPHA` blending, the `round()` in clip-rect conversion, the half-texel
//! offset in bilinear sampling -- is matched deliberately, so that a golden-image diff
//! against the GPU backend is a valid and very sharp correctness test. Where a comment
//! says "matches the GPU", it is load-bearing.
//!
//! # Usage
//!
//! ```no_run
//! # use egui_software::{Painter, Target};
//! # let ctx = egui::Context::default();
//! # let mut pixels = vec![0u32; 800 * 600];
//! # let full_output = ctx.run_ui(Default::default(), |_ui| {});
//! let mut painter = Painter::new();
//! // A texture can have several deltas queued in one frame (a full upload followed by
//! // atlas patches); they must be applied in order.
//! for (id, deltas) in &full_output.textures_delta.set {
//!     for delta in deltas {
//!         painter.set_texture(*id, delta);
//!     }
//! }
//!
//! let primitives = ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
//! let mut target = Target::new(&mut pixels, 800, 600).expect("buffer large enough");
//! Painter::clear(&mut target, egui::Color32::BLACK);
//! painter.paint(&mut target, full_output.pixels_per_point, &primitives);
//!
//! for id in &full_output.textures_delta.free {
//!     painter.free_texture(*id);
//! }
//! ```

mod raster;
mod target;
mod text;
mod texture;

pub use target::{PixelRect, Target, clip_rect_to_pixels, pack_rgb};
pub use texture::{Texture, TextureStore};

use ecolor::Color32;
use emath::GuiRounding as _;
use epaint::text::Galley;
use epaint::{
    ClippedPrimitive, ClippedShape, ImageDelta, Mesh, PreparedDisc, Primitive, Shape,
    TessellationOptions, Tessellator, TextShape, TextureId,
};

use raster::ScreenVertex;

/// Paints egui primitives into a [`Target`].
///
/// Owns the resident textures, so it must outlive the frames that reference them and be
/// fed every [`epaint::textures::TexturesDelta`] in order.
/// How text is drawn.
///
/// Both modes produce the same pixels for ordinary text; `blit_glyphs_never_matches`
/// asserts it. The setting exists so that test can compare them, and so a bug in the fast
/// path can be worked around without a rebuild of the tessellated one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextMode {
    /// Blit each glyph directly from the atlas. Required for sub-pixel rendering.
    #[default]
    Blit,
    /// Send glyph quads through the triangle rasterizer like any other geometry.
    Tessellate,
}

/// The inputs [`Painter::paint_shapes`] needs to tessellate non-text shapes itself.
///
/// These are exactly what `egui::Context::tessellate` reads out of the context, and a
/// caller holding an `egui::Context` can build one with
/// `ctx.fonts(|f| f.texture_atlas())` plus the memory's tessellation options.
#[derive(Clone)]
pub struct ShapeContext {
    pub pixels_per_point: f32,
    pub options: TessellationOptions,
    /// Size of the font atlas in texels, for UV normalization.
    pub font_tex_size: [usize; 2],
    pub prepared_discs: Vec<PreparedDisc>,
}

#[derive(Default)]
pub struct Painter {
    textures: TextureStore,
    text_mode: TextMode,
    /// Primitives that named a texture we do not have. Counted rather than logged per
    /// occurrence, because a missing texture usually means *every* glyph is missing and
    /// per-primitive logging would bury the actual cause.
    missing_texture_draws: usize,
}

impl Painter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_text_mode(&mut self, mode: TextMode) {
        self.text_mode = mode;
    }

    pub fn text_mode(&self) -> TextMode {
        self.text_mode
    }

    /// Apply one `set` entry from a [`epaint::textures::TexturesDelta`].
    pub fn set_texture(&mut self, id: TextureId, delta: &ImageDelta) {
        self.textures.set(id, delta);
    }

    /// Apply one `free` entry from a [`epaint::textures::TexturesDelta`].
    ///
    /// Must be called *after* painting the frame that the delta came from: egui frees a
    /// texture only once nothing in the finished frame references it any more.
    pub fn free_texture(&mut self, id: TextureId) {
        self.textures.free(id);
    }

    pub fn textures(&self) -> &TextureStore {
        &self.textures
    }

    /// How many primitives so far named a texture that was never uploaded.
    ///
    /// Non-zero means textures are being fed out of order, or freed too early. It is
    /// surfaced rather than logged so a caller can assert on it in tests.
    pub fn missing_texture_draws(&self) -> usize {
        self.missing_texture_draws
    }

    /// Fill the whole target with an opaque colour.
    pub fn clear(target: &mut Target<'_>, color: Color32) {
        let bounds = target.bounds();
        target.fill_rect(bounds, pack_rgb(color.r(), color.g(), color.b()));
    }

    /// Paint tessellated primitives.
    ///
    /// `pixels_per_point` must be the value the primitives were tessellated with;
    /// vertex positions are in points and are scaled by it here, exactly as the GPU does
    /// via `u_screen_size` and the physical-pixel viewport.
    pub fn paint(
        &mut self,
        target: &mut Target<'_>,
        pixels_per_point: f32,
        primitives: &[ClippedPrimitive],
    ) {
        let bounds = target.bounds();
        for ClippedPrimitive {
            clip_rect,
            primitive,
        } in primitives
        {
            let clip = clip_rect_to_pixels(*clip_rect, pixels_per_point, bounds);
            if clip.is_empty() {
                continue;
            }
            match primitive {
                Primitive::Mesh(mesh) => self.paint_mesh(target, clip, pixels_per_point, mesh),
                Primitive::Callback(_) => {
                    // `PaintCallback` hands a raw GPU render pass to the application.
                    // There is no CPU equivalent, and silently dropping it is the same
                    // thing `egui_glow` does when its callback feature is off.
                }
            }
        }
    }

    /// Paint **untessellated** shapes, intercepting text so glyphs are blitted directly.
    ///
    /// This is the entry point the `Software` renderer uses. [`Painter::paint`] remains
    /// available for callers that already have tessellated primitives, and produces the
    /// same output for everything except that it cannot do sub-pixel text.
    ///
    /// Draw order is preserved exactly: non-text shapes accumulate into a batch that is
    /// flushed through the tessellator whenever a text shape interrupts it, so a label
    /// drawn over a panel still lands over that panel.
    pub fn paint_shapes(
        &mut self,
        target: &mut Target<'_>,
        ctx: &ShapeContext,
        shapes: Vec<ClippedShape>,
    ) {
        if self.text_mode == TextMode::Tessellate {
            let primitives = Self::tessellate(ctx, shapes);
            self.paint(target, ctx.pixels_per_point, &primitives);
            return;
        }

        let bounds = target.bounds();
        let mut batch: Vec<ClippedShape> = Vec::new();

        for clipped in shapes {
            // Only unrotated text can be blitted; a rotated `TextShape` is not an
            // axis-aligned quad, so it goes through the tessellator like any other
            // geometry and comes out grayscale.
            let is_blittable_text = matches!(&clipped.shape, Shape::Text(t) if t.angle == 0.0);

            if !is_blittable_text {
                batch.push(clipped);
                continue;
            }

            // Flush everything queued before this text, so ordering is preserved.
            if !batch.is_empty() {
                let primitives = Self::tessellate(ctx, std::mem::take(&mut batch));
                self.paint(target, ctx.pixels_per_point, &primitives);
            }

            let clip = clip_rect_to_pixels(clipped.clip_rect, ctx.pixels_per_point, bounds);
            let Shape::Text(text) = &clipped.shape else {
                unreachable!("checked above")
            };
            self.paint_text(target, clip, clipped.clip_rect, ctx, text);
        }

        if !batch.is_empty() {
            let primitives = Self::tessellate(ctx, batch);
            self.paint(target, ctx.pixels_per_point, &primitives);
        }
    }

    fn tessellate(ctx: &ShapeContext, shapes: Vec<ClippedShape>) -> Vec<ClippedPrimitive> {
        Tessellator::new(
            ctx.pixels_per_point,
            ctx.options,
            ctx.font_tex_size,
            ctx.prepared_discs.clone(),
        )
        .tessellate_shapes(shapes)
    }

    /// Draw one [`TextShape`], blitting its glyphs and rasterizing everything else in the
    /// row mesh (selection backgrounds, strike-through) in the original order.
    ///
    /// The transforms mirror `Tessellator::tessellate_text` line for line: the galley
    /// position rounding, the per-row cull, and the three colour rules. Any divergence
    /// would put text a pixel away from where the GPU puts it.
    fn paint_text(
        &mut self,
        target: &mut Target<'_>,
        clip: PixelRect,
        clip_points: emath::Rect,
        ctx: &ShapeContext,
        text: &TextShape,
    ) {
        let galley: &Galley = &text.galley;
        if galley.is_empty() || text.opacity_factor <= 0.0 {
            return;
        }

        let ppp = ctx.pixels_per_point;
        let galley_pos = if ctx.options.round_text_to_pixels {
            text.pos.round_to_pixels(ppp)
        } else {
            text.pos
        };

        let Some(atlas) = self.textures.get(TextureId::Managed(0)) else {
            self.missing_texture_draws += 1;
            return;
        };

        for row in &galley.rows {
            if row.visuals.mesh.is_empty() {
                continue;
            }
            let final_row_pos = galley_pos + row.pos.to_vec2();
            let row_rect = row.visuals.mesh_bounds.translate(final_row_pos.to_vec2());
            if ctx.options.coarse_tessellation_culling && !clip_points.intersects(row_rect) {
                // Culling per row matters: one `Shape::Text` can span hundreds of lines.
                continue;
            }

            let mesh = &row.visuals.mesh;
            let glyphs = &row.visuals.glyph_vertex_range;

            // The galley mesh stores UVs in TEXELS, and the two consumers want different
            // units: the blitter needs texels (normalizing and multiplying back would
            // throw away the exactness the 1:1 blit relies on), while the triangle
            // rasterizer samples with normalized coordinates like the GPU does. Feeding
            // texels to the latter samples far off the edge of the atlas and silently
            // draws nothing, so the two are kept deliberately separate.
            let uv_scale = emath::vec2(
                1.0 / ctx.font_tex_size[0] as f32,
                1.0 / ctx.font_tex_size[1] as f32,
            );
            let resolve = |i: usize, normalize_uv: bool| -> ScreenVertex {
                let v = mesh.vertices[i];
                let mut color = v.color;
                if let Some(override_color) = text.override_text_color {
                    // Only the glyphs, not backgrounds or strike-through.
                    if glyphs.contains(&i) {
                        color = override_color;
                    }
                } else if color == Color32::PLACEHOLDER {
                    color = text.fallback_color;
                }
                if text.opacity_factor < 1.0 {
                    color = color.gamma_multiply(text.opacity_factor);
                }
                let pos = final_row_pos + v.pos.to_vec2();
                let (u, vv) = if normalize_uv {
                    (v.uv.x * uv_scale.x, v.uv.y * uv_scale.y)
                } else {
                    (v.uv.x, v.uv.y)
                };
                ScreenVertex {
                    x: pos.x * ppp,
                    y: pos.y * ppp,
                    u,
                    v: vv,
                    color,
                }
            };

            // Walk triangles in emission order so backgrounds stay under the glyphs and
            // strike-through stays over them. A glyph quad is four consecutive vertices,
            // so it is blitted once, when its first triangle appears.
            let mut last_quad_base: Option<usize> = None;
            for tri in mesh.indices.chunks_exact(3) {
                let idx = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
                if idx.iter().any(|i| *i >= mesh.vertices.len()) {
                    continue;
                }

                if idx.iter().all(|i| glyphs.contains(i)) {
                    let lowest = idx.iter().copied().min().unwrap_or(glyphs.start);
                    let base = glyphs.start + ((lowest - glyphs.start) / 4) * 4;
                    if last_quad_base == Some(base) {
                        continue; // the second triangle of a quad already blitted
                    }
                    if base + 3 < mesh.vertices.len() {
                        let quad = [
                            resolve(base, false),
                            resolve(base + 1, false),
                            resolve(base + 2, false),
                            resolve(base + 3, false),
                        ];
                        if text::blit_quad(target, clip, &quad, atlas) {
                            // Only mark it consumed once the blit actually happened.
                            // Marking it before would swallow the quads second triangle
                            // on the fallback path and draw half of every italic glyph.
                            last_quad_base = Some(base);
                            continue;
                        }
                    }
                    // Not a clean 1:1 quad (italics, an odd scale): fall through so both
                    // of its triangles draw through the general path.
                }

                raster::triangle(
                    target,
                    clip,
                    [
                        resolve(idx[0], true),
                        resolve(idx[1], true),
                        resolve(idx[2], true),
                    ],
                    Some(atlas),
                );
            }
        }

        // The underline is a separate stroke, drawn after the rows like the tessellator
        // does. Routed through the batch path so its geometry is epaint's, not ours.
        if text.underline != epaint::Stroke::NONE {
            let mut shapes = Vec::new();
            for row in &galley.rows {
                if row.visuals.mesh.is_empty() {
                    continue;
                }
                let final_row_pos = galley_pos + row.pos.to_vec2();
                let row_rect = row.visuals.mesh_bounds.translate(final_row_pos.to_vec2());
                shapes.push(ClippedShape {
                    clip_rect: clip_points,
                    shape: Shape::line_segment(
                        [row_rect.left_bottom(), row_rect.right_bottom()],
                        text.underline,
                    ),
                });
            }
            let primitives = Self::tessellate(ctx, shapes);
            self.paint(target, ppp, &primitives);
        }
    }

    fn paint_mesh(
        &mut self,
        target: &mut Target<'_>,
        clip: PixelRect,
        pixels_per_point: f32,
        mesh: &Mesh,
    ) {
        if mesh.indices.is_empty() {
            return;
        }
        let Some(texture) = self.textures.get(mesh.texture_id) else {
            self.missing_texture_draws += 1;
            return;
        };

        // egui draws untextured geometry by pointing every vertex at the atlas's white
        // texel (`WHITE_UV`). Detecting that lets the whole fetch-and-multiply be skipped
        // for the majority of the UI -- panels, table rows, heat cells -- and the result
        // is not merely close but exactly equal, because multiplying by an opaque white
        // texel is the identity.
        let all_white_uv = mesh.vertices.iter().all(|v| v.uv == epaint::WHITE_UV);
        let sampler = if all_white_uv { None } else { Some(texture) };

        for tri in mesh.indices.chunks_exact(3) {
            let (Some(&a), Some(&b), Some(&c)) = (tri.first(), tri.get(1), tri.get(2)) else {
                continue;
            };
            let (Some(va), Some(vb), Some(vc)) = (
                mesh.vertices.get(a as usize),
                mesh.vertices.get(b as usize),
                mesh.vertices.get(c as usize),
            ) else {
                // Out-of-range index. epaint never emits these, but a malformed
                // user-built `Mesh` would otherwise panic deep in the rasterizer.
                continue;
            };
            let to_screen = |v: &epaint::Vertex| ScreenVertex {
                x: v.pos.x * pixels_per_point,
                y: v.pos.y * pixels_per_point,
                u: v.uv.x,
                v: v.uv.y,
                color: v.color,
            };
            raster::triangle(
                target,
                clip,
                [to_screen(va), to_screen(vb), to_screen(vc)],
                sampler,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emath::{Rect, pos2};
    use epaint::{ColorImage, Vertex, textures::TextureOptions};

    fn white_atlas() -> (TextureId, ImageDelta) {
        // A 1x1 opaque white texture standing in for the font atlas, so `WHITE_UV`
        // (which is (0,0)) resolves to white.
        (
            TextureId::Managed(0),
            ImageDelta::full(
                ColorImage::new([1, 1], vec![Color32::WHITE]),
                TextureOptions::NEAREST,
            ),
        )
    }

    fn quad(rect: Rect, color: Color32) -> Mesh {
        let mut mesh = Mesh::default();
        for pos in [
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ] {
            mesh.vertices.push(Vertex {
                pos,
                uv: epaint::WHITE_UV,
                color,
            });
        }
        mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
        mesh
    }

    fn paint_one(mesh: Mesh, ppp: f32, w: u32, h: u32) -> Vec<u32> {
        let mut painter = Painter::new();
        let (id, delta) = white_atlas();
        painter.set_texture(id, &delta);
        let mut buf = vec![0u32; (w * h) as usize];
        let mut target = Target::new(&mut buf, w, h).unwrap();
        painter.paint(
            &mut target,
            ppp,
            &[ClippedPrimitive {
                clip_rect: Rect::from_min_max(pos2(0.0, 0.0), pos2(w as f32, h as f32)),
                primitive: Primitive::Mesh(mesh),
            }],
        );
        assert_eq!(painter.missing_texture_draws(), 0);
        buf
    }

    #[test]
    fn an_opaque_quad_lands_on_exact_pixel_bounds() {
        let w = 16;
        let h = 16;
        let buf = paint_one(
            quad(
                Rect::from_min_max(pos2(4.0, 4.0), pos2(12.0, 12.0)),
                Color32::WHITE,
            ),
            1.0,
            w,
            h,
        );
        let at = |x: u32, y: u32| buf[(y * w + x) as usize];
        assert_eq!(at(4, 4), pack_rgb(255, 255, 255), "first covered pixel");
        assert_eq!(at(11, 11), pack_rgb(255, 255, 255), "last covered pixel");
        assert_eq!(at(3, 4), 0, "just outside on the left");
        assert_eq!(at(12, 11), 0, "max is exclusive");
    }

    /// Vertex positions are in points; `pixels_per_point` scales them. A quad of 4..12
    /// points at ppp 2 must cover 8..24 physical pixels.
    #[test]
    fn pixels_per_point_scales_geometry_like_the_gpu_viewport() {
        let w = 32;
        let h = 32;
        let buf = paint_one(
            quad(
                Rect::from_min_max(pos2(4.0, 4.0), pos2(12.0, 12.0)),
                Color32::WHITE,
            ),
            2.0,
            w,
            h,
        );
        let at = |x: u32, y: u32| buf[(y * w + x) as usize];
        assert_eq!(at(8, 8), pack_rgb(255, 255, 255));
        assert_eq!(at(23, 23), pack_rgb(255, 255, 255));
        assert_eq!(at(7, 8), 0);
        assert_eq!(at(24, 23), 0);
    }

    /// A mesh whose indices point past the end of its vertex list must be skipped, not
    /// panic. epaint never emits this, but application code can build a `Mesh` by hand.
    #[test]
    fn out_of_range_indices_are_skipped() {
        let mut mesh = quad(
            Rect::from_min_max(pos2(2.0, 2.0), pos2(6.0, 6.0)),
            Color32::WHITE,
        );
        mesh.indices.extend_from_slice(&[0, 1, 99]);
        let buf = paint_one(mesh, 1.0, 8, 8);
        assert!(
            buf.iter().any(|&p| p != 0),
            "the valid triangles still drew"
        );
    }

    #[test]
    fn a_mesh_naming_an_unknown_texture_is_counted_not_drawn() {
        let mut painter = Painter::new();
        let mut mesh = quad(
            Rect::from_min_max(pos2(0.0, 0.0), pos2(8.0, 8.0)),
            Color32::WHITE,
        );
        mesh.texture_id = TextureId::Managed(7);
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        painter.paint(
            &mut target,
            1.0,
            &[ClippedPrimitive {
                clip_rect: Rect::from_min_max(pos2(0.0, 0.0), pos2(8.0, 8.0)),
                primitive: Primitive::Mesh(mesh),
            }],
        );
        assert_eq!(painter.missing_texture_draws(), 1);
        assert!(buf.iter().all(|&p| p == 0));
    }

    #[test]
    fn clear_fills_the_whole_target() {
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        Painter::clear(&mut target, Color32::from_rgb(0x19, 0x19, 0x19));
        assert!(buf.iter().all(|&p| p == pack_rgb(0x19, 0x19, 0x19)));
    }

    /// Translucent paint over a known background must follow `src + dst*(1-a)` in gamma
    /// space -- the same arithmetic the GPU's blend unit performs.
    #[test]
    fn translucent_paint_blends_in_gamma_space() {
        let mut painter = Painter::new();
        let (id, delta) = white_atlas();
        painter.set_texture(id, &delta);
        let mut buf = vec![0u32; 64];
        let mut target = Target::new(&mut buf, 8, 8).unwrap();
        Painter::clear(&mut target, Color32::from_rgb(100, 100, 100));
        // Premultiplied 50% white.
        let c = Color32::from_rgba_premultiplied(128, 128, 128, 128);
        painter.paint(
            &mut target,
            1.0,
            &[ClippedPrimitive {
                clip_rect: Rect::from_min_max(pos2(0.0, 0.0), pos2(8.0, 8.0)),
                primitive: Primitive::Mesh(quad(
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(8.0, 8.0)),
                    c,
                )),
            }],
        );
        // 128 + 100 * (1 - 128/255) = 128 + 100*0.498 = 177.8 -> 178
        let expected: f32 = 128.0 + 100.0 * (1.0 - 128.0 / 255.0);
        let got = (buf[0] >> 16) & 0xff;
        assert_eq!(got, expected.round() as u32);
    }
}

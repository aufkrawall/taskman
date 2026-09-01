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
mod texture;

pub use target::{PixelRect, Target, clip_rect_to_pixels, pack_rgb};
pub use texture::{Texture, TextureStore};

use ecolor::Color32;
use epaint::{ClippedPrimitive, ImageDelta, Mesh, Primitive, TextureId};

use raster::ScreenVertex;

/// Paints egui primitives into a [`Target`].
///
/// Owns the resident textures, so it must outlive the frames that reference them and be
/// fed every [`epaint::textures::TexturesDelta`] in order.
#[derive(Default)]
pub struct Painter {
    textures: TextureStore,
    /// Primitives that named a texture we do not have. Counted rather than logged per
    /// occurrence, because a missing texture usually means *every* glyph is missing and
    /// per-primitive logging would bury the actual cause.
    missing_texture_draws: usize,
}

impl Painter {
    pub fn new() -> Self {
        Self::default()
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

//! Texture storage and sampling, matching what the GPU backends ask their samplers for.

use ecolor::Color32;
use epaint::textures::{TextureFilter, TextureOptions, TextureWrapMode};
use epaint::{ImageData, ImageDelta, TextureId};

/// One resident texture: premultiplied sRGB texels plus the sampler state egui asked for.
pub struct Texture {
    pub pixels: Vec<Color32>,
    pub width: usize,
    pub height: usize,
    pub options: TextureOptions,
}

impl Texture {
    /// Sample with the semantics of an OpenGL sampler configured the way `egui_glow`
    /// configures it.
    ///
    /// `magnification` is used unconditionally. egui's textures are drawn at or near 1:1
    /// (the font atlas is exactly 1:1, and images are sized by the layout), and neither
    /// GPU backend uploads mipmaps by default, so a separate minification path would only
    /// differ where the GPU is itself aliasing.
    pub fn sample(&self, u: f32, v: f32) -> Color32 {
        self.sample_impl(u, v, false)
    }

    /// Sample the font atlas with LCD texels reduced to their mean grayscale coverage.
    ///
    /// Rotated, scaled or otherwise non-1:1 text cannot use the direct per-channel blit.
    /// Its triangles still sample the font atlas, so converting the LCD texels before
    /// filtering prevents tinted fringes while preserving a premultiplied grayscale blend.
    /// Non-LCD texels (including premultiplied colour glyphs) pass through unchanged.
    pub(crate) fn sample_gray_lcd(&self, u: f32, v: f32) -> Color32 {
        self.sample_impl(u, v, true)
    }

    fn sample_impl(&self, u: f32, v: f32, gray_lcd: bool) -> Color32 {
        if self.width == 0 || self.height == 0 {
            return Color32::TRANSPARENT;
        }
        if self.options.magnification == TextureFilter::Linear {
            self.bilinear(u, v, gray_lcd)
        } else {
            // GL samples the texel whose cell contains the coordinate.
            let x = (u * self.width as f32).floor();
            let y = (v * self.height as f32).floor();
            self.texel_wrapped(x as i64, y as i64, gray_lcd)
        }
    }

    fn bilinear(&self, u: f32, v: f32, gray_lcd: bool) -> Color32 {
        // Texel centres sit at (i + 0.5) / size, so the continuous coordinate of texel i
        // is `uv * size - 0.5`. Getting this half-texel shift wrong is the classic cause
        // of software renderers looking subtly blurrier than the GPU.
        let x = u * self.width as f32 - 0.5;
        let y = v * self.height as f32 - 0.5;
        let x0 = x.floor();
        let y0 = y.floor();
        let fx = x - x0;
        let fy = y - y0;
        let (x0, y0) = (x0 as i64, y0 as i64);

        let c00 = self.texel_wrapped(x0, y0, gray_lcd);
        let c10 = self.texel_wrapped(x0 + 1, y0, gray_lcd);
        let c01 = self.texel_wrapped(x0, y0 + 1, gray_lcd);
        let c11 = self.texel_wrapped(x0 + 1, y0 + 1, gray_lcd);

        let lerp = |a: u8, b: u8, t: f32| a as f32 + (b as f32 - a as f32) * t;
        let mut out = [0u8; 4];
        for (i, o) in out.iter_mut().enumerate() {
            let top = lerp(c00[i], c10[i], fx);
            let bottom = lerp(c01[i], c11[i], fx);
            *o = (top + (bottom - top) * fy).round().clamp(0.0, 255.0) as u8;
        }
        Color32::from_rgba_premultiplied(out[0], out[1], out[2], out[3])
    }

    /// Fetch a texel, applying the texture's wrap mode to out-of-range coordinates.
    fn texel_wrapped(&self, x: i64, y: i64, gray_lcd: bool) -> Color32 {
        let x = wrap(x, self.width as i64, self.options.wrap_mode);
        let y = wrap(y, self.height as i64, self.options.wrap_mode);
        let idx = (y as usize) * self.width + (x as usize);
        let texel = self
            .pixels
            .get(idx)
            .copied()
            .unwrap_or(Color32::TRANSPARENT);
        if gray_lcd {
            gray_lcd_texel(texel)
        } else {
            texel
        }
    }
}

fn gray_lcd_texel(texel: Color32) -> Color32 {
    if !epaint::text::subpixel::is_lcd_texel(texel) {
        return texel;
    }
    let [r, g, b, _] = texel.to_array();
    let gray = ((u32::from(r) + u32::from(g) + u32::from(b) + 1) / 3) as u8;
    Color32::from_rgba_premultiplied(gray, gray, gray, gray)
}

#[inline]
fn wrap(v: i64, size: i64, mode: TextureWrapMode) -> i64 {
    if size <= 0 {
        return 0;
    }
    match mode {
        TextureWrapMode::ClampToEdge => v.clamp(0, size - 1),
        TextureWrapMode::Repeat => v.rem_euclid(size),
        TextureWrapMode::MirroredRepeat => {
            let period = 2 * size;
            let m = v.rem_euclid(period);
            if m < size { m } else { period - 1 - m }
        }
    }
}

/// Every texture egui has asked us to keep resident.
#[derive(Default)]
pub struct TextureStore {
    textures: std::collections::HashMap<TextureId, Texture>,
    generations: std::collections::HashMap<TextureId, u64>,
    next_gen: u64,
}

impl TextureStore {
    pub fn get(&self, id: TextureId) -> Option<&Texture> {
        self.textures.get(&id)
    }

    /// Monotonic generation counter for `id` (bumped on full upload, on any partial update
    /// of a non-atlas texture, on an atlas partial update that changes texels which already
    /// held content, and on free), used by tile damage tracking.
    pub fn generation(&self, id: TextureId) -> u64 {
        self.generations.get(&id).copied().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.textures.len()
    }

    pub fn is_empty(&self) -> bool {
        self.textures.is_empty()
    }

    fn bump_generation(&mut self, id: TextureId) {
        self.next_gen = self.next_gen.wrapping_add(1).max(1);
        self.generations.insert(id, self.next_gen);
    }

    /// Apply one entry of a [`epaint::textures::TexturesDelta`].
    ///
    /// A delta with `pos: None` replaces the whole texture; with `pos: Some` it patches a
    /// sub-rectangle of an existing one. The font atlas grows by full replacement and is
    /// then updated with partial patches as new glyphs are rasterized, so both paths are
    /// exercised constantly.
    pub fn set(&mut self, id: TextureId, delta: &ImageDelta) {
        let ImageData::Color(image) = &delta.image;
        let [w, h] = image.size;

        match delta.pos {
            None => {
                self.textures.insert(
                    id,
                    Texture {
                        pixels: image.pixels.clone(),
                        width: w,
                        height: h,
                        options: delta.options,
                    },
                );
                self.bump_generation(id);
            }
            Some([x, y]) => {
                let Some(tex) = self.textures.get_mut(&id) else {
                    // A patch for a texture we never received. Dropping it would leave the
                    // atlas silently wrong, so say so rather than failing quietly.
                    log_missing_patch(id);
                    return;
                };
                tex.options = delta.options;
                let mut overwrote_live_texels = false;
                for row in 0..h {
                    let dst_y = y + row;
                    if dst_y >= tex.height {
                        break;
                    }
                    let dst_start = dst_y * tex.width + x;
                    let copy_w = w.min(tex.width.saturating_sub(x));
                    let src = &image.pixels[row * w..row * w + copy_w];
                    if let Some(dst) = tex.pixels.get_mut(dst_start..dst_start + copy_w) {
                        if id == TextureId::Managed(0) {
                            // Only the atlas can rewrite texels an existing shape still
                            // samples, so only the atlas needs the compare.
                            for (dst_px, src_px) in dst.iter_mut().zip(src) {
                                if *dst_px != *src_px {
                                    overwrote_live_texels |= *dst_px != Color32::TRANSPARENT;
                                    *dst_px = *src_px;
                                }
                            }
                        } else {
                            dst.copy_from_slice(src);
                        }
                    }
                }
                // Managed(0) is the font atlas, where a patch that changes a texel already
                // holding content is not a fresh allocation: epaint reuses slots when the
                // atlas overflows (the cursor restarts at x = 0 inside the image and live
                // glyph cells are overwritten), so shapes sampling those texels no longer
                // describe the pixels they draw.
                //
                // Bumping only for that case keeps a newly rasterized glyph free, and the
                // emptiness test is sound because of an allocator property, not by luck: an
                // allocated cell is always written in full -- both rasterizers loop over the
                // whole width x height rect, zero coverage included -- and a zero-sized glyph
                // is never allocated, so a cell cannot be partly occupied when it is reused.
                // One case still escapes it: right after an overflow restart a new cell of a
                // different size can overlap a still-live glyph cell and write content into
                // that cell's transparent texels, which compares as "empty" and does not bump.
                // That state cannot outlive the next pass: overflow pins `fill_ratio()` at 1.0,
                // so `Fonts::begin_pass` recreates the whole atlas and the upload that follows
                // is a full one that does bump -- at worst one frame of stale glyphs, in the
                // state epaint already logs a warning about. Any other texture patched in place
                // may change pixels of existing meshes.
                if id != TextureId::Managed(0) || overwrote_live_texels {
                    self.bump_generation(id);
                }
            }
        }
    }

    pub fn free(&mut self, id: TextureId) {
        if self.textures.remove(&id).is_some() {
            self.bump_generation(id);
        }
    }
}

fn log_missing_patch(id: TextureId) {
    // epaint has no logging dependency of its own here; a debug assertion turns this into
    // a loud failure in tests and dev builds while staying silent in release.
    debug_assert!(false, "software painter: patch for unknown texture {id:?}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use epaint::ColorImage;

    fn solid(w: usize, h: usize, c: Color32) -> ColorImage {
        ColorImage::new([w, h], vec![c; w * h])
    }

    #[test]
    fn a_full_delta_replaces_and_a_patch_updates_in_place() {
        let mut store = TextureStore::default();
        let id = TextureId::Managed(0);
        store.set(
            id,
            &ImageDelta::full(solid(4, 4, Color32::RED), TextureOptions::NEAREST),
        );
        assert_eq!(store.get(id).unwrap().pixels[0], Color32::RED);

        store.set(
            id,
            &ImageDelta::partial([1, 1], solid(2, 2, Color32::GREEN), TextureOptions::NEAREST),
        );
        let tex = store.get(id).unwrap();
        assert_eq!(tex.pixels[0], Color32::RED, "outside the patch");
        assert_eq!(tex.pixels[4 + 1], Color32::GREEN, "inside the patch");
        assert_eq!(tex.pixels[4 + 3], Color32::RED, "right of the patch");
    }

    /// A patch that runs past the right or bottom edge must clip, not panic and not wrap
    /// onto the next row.
    #[test]
    fn an_oversized_patch_is_clipped_to_the_texture() {
        let mut store = TextureStore::default();
        let id = TextureId::Managed(0);
        store.set(
            id,
            &ImageDelta::full(solid(4, 4, Color32::RED), TextureOptions::NEAREST),
        );
        store.set(
            id,
            &ImageDelta::partial([3, 3], solid(4, 4, Color32::GREEN), TextureOptions::NEAREST),
        );
        let tex = store.get(id).unwrap();
        assert_eq!(
            tex.pixels[3 * 4 + 3],
            Color32::GREEN,
            "the one texel that fits"
        );
        assert_eq!(
            tex.pixels[3 * 4],
            Color32::RED,
            "no wrap onto the row start"
        );
    }

    /// A patch that only fills empty atlas space adds a glyph. Bumping the generation for
    /// that would re-dirty every text tile in the window, so a newly rasterized glyph has
    /// to stay cheap.
    #[test]
    fn a_patch_into_empty_atlas_space_keeps_the_generation() {
        let mut store = TextureStore::default();
        let id = TextureId::Managed(0);
        let blank = solid(4, 4, Color32::TRANSPARENT);
        store.set(id, &ImageDelta::full(blank, TextureOptions::NEAREST));
        let before = store.generation(id);

        let glyph = solid(2, 2, Color32::WHITE);
        store.set(
            id,
            &ImageDelta::partial([1, 1], glyph, TextureOptions::NEAREST),
        );

        assert_eq!(store.get(id).unwrap().pixels[4 + 1], Color32::WHITE);
        assert_eq!(
            store.generation(id),
            before,
            "a fresh glyph slot must not invalidate the tiles showing other text"
        );
    }

    /// epaint reuses atlas slots once the atlas overflows, so a patch can rewrite texels
    /// that galleys in the same frame still sample. Those tiles keep their hashes unless
    /// the generation moves, and then they never repaint: stale glyphs on screen.
    #[test]
    fn a_patch_that_rewrites_a_live_atlas_texel_bumps_the_generation() {
        let mut store = TextureStore::default();
        let id = TextureId::Managed(0);
        let blank = solid(4, 4, Color32::TRANSPARENT);
        store.set(id, &ImageDelta::full(blank, TextureOptions::NEAREST));
        let glyph = solid(2, 2, Color32::WHITE);
        store.set(
            id,
            &ImageDelta::partial([1, 1], glyph, TextureOptions::NEAREST),
        );
        let before = store.generation(id);

        // Same slot, different coverage: one glyph evicted and replaced by another.
        let evicted = solid(2, 2, Color32::from_white_alpha(96));
        store.set(
            id,
            &ImageDelta::partial([1, 1], evicted, TextureOptions::NEAREST),
        );

        assert_eq!(
            store.get(id).unwrap().pixels[4 + 1],
            Color32::from_white_alpha(96)
        );
        assert_ne!(
            store.generation(id),
            before,
            "rewriting a live glyph slot must invalidate the tiles sampling it"
        );
    }

    #[test]
    fn nearest_sampling_picks_the_texel_containing_the_coordinate() {
        let tex = Texture {
            pixels: vec![Color32::RED, Color32::GREEN, Color32::BLUE, Color32::WHITE],
            width: 2,
            height: 2,
            options: TextureOptions::NEAREST,
        };
        assert_eq!(tex.sample(0.25, 0.25), Color32::RED);
        assert_eq!(tex.sample(0.75, 0.25), Color32::GREEN);
        assert_eq!(tex.sample(0.25, 0.75), Color32::BLUE);
        assert_eq!(tex.sample(0.75, 0.75), Color32::WHITE);
    }

    /// At a texel centre, bilinear filtering must return that texel exactly. If the
    /// half-texel shift is wrong this returns a blend and every image looks soft.
    #[test]
    fn bilinear_at_a_texel_centre_is_exact() {
        let tex = Texture {
            pixels: vec![Color32::RED, Color32::GREEN, Color32::BLUE, Color32::WHITE],
            width: 2,
            height: 2,
            options: TextureOptions::LINEAR,
        };
        assert_eq!(tex.sample(0.25, 0.25), Color32::RED);
        assert_eq!(tex.sample(0.75, 0.75), Color32::WHITE);
    }

    #[test]
    fn gray_lcd_sampling_neutralizes_coverage_but_preserves_color_glyphs() {
        let subpixel = Color32::from_rgba_premultiplied(255, 128, 0, 0);
        let neutral = Color32::from_rgba_premultiplied(128, 128, 128, 0);
        let color_glyph = Color32::from_rgba_premultiplied(200, 40, 10, 220);
        let tex = Texture {
            pixels: vec![subpixel, neutral, color_glyph],
            width: 3,
            height: 1,
            options: TextureOptions::NEAREST,
        };

        let gray = tex.sample_gray_lcd(1.0 / 6.0, 0.5).to_array();
        assert!(
            gray[0] == gray[1] && gray[1] == gray[2] && gray[2] == gray[3],
            "LCD texel must become neutral premultiplied coverage: {gray:?}"
        );
        assert!(gray[0] > 0);
        assert_eq!(tex.sample_gray_lcd(0.5, 0.5).to_array()[0], 128);
        assert_eq!(tex.sample_gray_lcd(5.0 / 6.0, 0.5), color_glyph);
    }

    #[test]
    fn wrap_modes_behave_like_gl() {
        assert_eq!(wrap(-3, 4, TextureWrapMode::ClampToEdge), 0);
        assert_eq!(wrap(9, 4, TextureWrapMode::ClampToEdge), 3);
        assert_eq!(wrap(-1, 4, TextureWrapMode::Repeat), 3);
        assert_eq!(wrap(5, 4, TextureWrapMode::Repeat), 1);
        // Mirrored: 0,1,2,3, 3,2,1,0, 0,1,...
        assert_eq!(wrap(4, 4, TextureWrapMode::MirroredRepeat), 3);
        assert_eq!(wrap(7, 4, TextureWrapMode::MirroredRepeat), 0);
        assert_eq!(wrap(-1, 4, TextureWrapMode::MirroredRepeat), 0);
    }

    #[test]
    fn a_zero_sized_texture_samples_transparent_instead_of_panicking() {
        let tex = Texture {
            pixels: vec![],
            width: 0,
            height: 0,
            options: TextureOptions::LINEAR,
        };
        assert_eq!(tex.sample(0.5, 0.5), Color32::TRANSPARENT);
    }
}

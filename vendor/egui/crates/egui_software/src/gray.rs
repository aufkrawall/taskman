//! TASKMAN-FORK: render-time grayscale glyph weight.
//!
//! With [`epaint::FontColorTransferFunction::PolarityGamma`] the font atlas stores raw
//! coverage and the renderer turns it into alpha per draw: `alpha = coverage^e`, where `e`
//! is the light-text exponent for light-on-dark text and the dark-text exponent otherwise
//! ([`epaint::text_is_light`]: luma above [`epaint::polarity_split`]). A curve baked into the shared
//! atlas could serve only one polarity; this is what lets the white label on a Light-theme
//! accent button keep its weight while dark body text stays lean.
//!
//! The WGPU (`egui.wgsl`'s `gray_text_texel`) and Glow fragment shaders implement the same
//! rule in float; the two LUTs here are the integer form of it, so outputs agree to rounding.

use ecolor::Color32;

/// The two coverage-to-alpha tables for one `(light, dark, split)` triple.
pub struct GrayTextCurves {
    exponents: [f32; 2],
    /// [`epaint::polarity_split`] of the page and inverse-surface lumas.
    split: f32,
    light: [u8; 256],
    dark: [u8; 256],
}

impl GrayTextCurves {
    /// `exponents` is `[light_text, dark_text]`, already sanitized by
    /// [`epaint::FontColorTransferFunction::render_time_exponents`]; `split` is
    /// [`epaint::polarity_split`] of the expected surfaces.
    pub fn new(exponents: [f32; 2], split: f32) -> Self {
        let table = |e: f32| {
            let mut lut = [0u8; 256];
            for (c, slot) in lut.iter_mut().enumerate() {
                let alpha = (c as f32 / 255.0).powf(e);
                *slot = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
            }
            // `powf` keeps both ends exact, but the endpoints are what keep solid glyph
            // interiors and `WHITE_UV` geometry bit-identical, so pin them explicitly.
            lut[0] = 0;
            lut[255] = 255;
            lut
        };
        Self {
            exponents,
            split,
            light: table(exponents[0]),
            dark: table(exponents[1]),
        }
    }

    pub fn matches(&self, exponents: [f32; 2], split: f32) -> bool {
        self.exponents[0].to_bits() == exponents[0].to_bits()
            && self.exponents[1].to_bits() == exponents[1].to_bits()
            && self.split.to_bits() == split.to_bits()
    }

    pub fn exponents(&self) -> [f32; 2] {
        self.exponents
    }

    pub fn split(&self) -> f32 {
        self.split
    }

    #[inline]
    fn lut_for_rgb(&self, [r, g, b]: [f32; 3]) -> &[u8; 256] {
        // `epaint::text_is_light` against a precomputed split (an unknown inverse surface
        // makes `polarity_split(split, -1) == split`).
        if epaint::text_is_light([r, g, b], self.split, -1.0) {
            &self.light
        } else {
            &self.dark
        }
    }

    /// The table for text of this unmultiplied sRGB colour.
    #[inline]
    pub fn lut_for_straight(&self, [r, g, b]: [u8; 3]) -> &[u8; 256] {
        self.lut_for_rgb([
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
        ])
    }

    /// The table for text of this premultiplied colour.
    #[inline]
    pub fn lut_for_premultiplied(&self, color: Color32) -> &[u8; 256] {
        let [r, g, b, _] = color.to_srgba_unmultiplied();
        self.lut_for_straight([r, g, b])
    }

    /// The table for a premultiplied colour in `0..=255` floats (an interpolated vertex
    /// colour), unmultiplied in float like the GPU's `color.rgb / color.a`.
    #[inline]
    pub fn lut_for_premultiplied_f32(&self, [r, g, b, a]: [f32; 4]) -> &[u8; 256] {
        let rgb = if a > 0.0 {
            [
                (r / a).clamp(0.0, 1.0),
                (g / a).clamp(0.0, 1.0),
                (b / a).clamp(0.0, 1.0),
            ]
        } else {
            [0.0; 3]
        };
        self.lut_for_rgb(rgb)
    }
}

/// Apply a table to one font-atlas texel.
///
/// Only premultiplied white coverage `(c, c, c, c)` -- a grayscale glyph, a prepared disc,
/// the `WHITE_UV` texel -- is coverage; anything else (an LCD-tagged texel, a colour glyph)
/// is returned untouched.
#[inline]
pub fn apply(lut: &[u8; 256], texel: Color32) -> Color32 {
    let [r, g, b, a] = texel.to_array();
    if r == a && g == a && b == a {
        let alpha = lut[a as usize];
        Color32::from_rgba_premultiplied(alpha, alpha, alpha, alpha)
    } else {
        texel
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_follow_text_polarity_and_keep_the_endpoints() {
        // The Light theme: a 0.96 page and 0.13 dark toasts.
        let curves = GrayTextCurves::new([0.8, 1.5], epaint::polarity_split(0.96, 0.13));
        // White toast text is lifted, and so is light-gray toast text -- darker than the
        // page, but far brighter than the toast it sits on.
        let white = curves.lut_for_straight([255, 255, 255]);
        assert!(
            white[128] > 128,
            "light text must be lifted: {}",
            white[128]
        );
        assert_eq!(curves.lut_for_straight([0xc8, 0xc8, 0xc8]), white);
        // Dark body text on the page is thinned.
        let dark = curves.lut_for_straight([0x1a, 0x1a, 0x1a]);
        assert!(dark[128] < 128, "dark text must be thinned: {}", dark[128]);
        for lut in [white, dark] {
            assert_eq!(lut[0], 0);
            assert_eq!(lut[255], 255);
        }
        // Translucent white is still white.
        let faded = curves.lut_for_premultiplied(Color32::from_white_alpha(100));
        assert_eq!(faded, white);
    }

    #[test]
    fn only_premultiplied_white_coverage_is_remapped() {
        let curves = GrayTextCurves::new([0.5, 2.0], 0.5);
        let lut = curves.lut_for_straight([255, 255, 255]);
        assert_eq!(
            apply(lut, Color32::from_rgba_premultiplied(64, 64, 64, 64)).to_array(),
            [lut[64]; 4]
        );
        let lcd = Color32::from_rgba_premultiplied(90, 60, 30, 0);
        assert_eq!(apply(lut, lcd), lcd);
        let color_glyph = Color32::from_rgba_premultiplied(100, 30, 20, 200);
        assert_eq!(apply(lut, color_glyph), color_glyph);
        assert_eq!(apply(lut, Color32::WHITE), Color32::WHITE);
        assert_eq!(apply(lut, Color32::TRANSPARENT), Color32::TRANSPARENT);
    }
}

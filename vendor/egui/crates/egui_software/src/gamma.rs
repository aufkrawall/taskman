//! Gamma-corrected, contrast-enhanced text blending.
//!
//! Blending glyph coverage straight into an sRGB framebuffer is wrong, and wrong in a way
//! that is obvious once you know to look: light-on-dark text comes out bloated and
//! dark-on-light text comes out anaemic. sRGB values are not proportional to light, so a
//! 50%-covered pixel blended at 50% does not look half-covered.
//!
//! Windows blends text in a **gamma-corrected space** with a **contrast boost**, and
//! exposes both through `IDWriteRenderingParams` (defaults: gamma 1.8, enhanced contrast
//! 0.5). Reproducing that is what makes sub-pixel output look like `ClearType` rather than
//! like coloured mush.
//!
//! # The model
//!
//! Per channel, with coverage `a` in 0..=1:
//!
//! ```text
//! a' = a * (1 + k * (1 - a))                      # enhanced contrast, k = contrast
//! out = ( src^g * a' + dst^g * (1 - a') ) ^ (1/g) # gamma-space blend, g = gamma
//! ```
//!
//! `a'` is monotone, fixes both 0 and 1, and peaks its boost at `a = 0.5` (adding `k/4`),
//! which is exactly the "half-covered pixels look too thin" correction.
//!
//! **Honest limitation:** Microsoft has never published `DirectWrite`'s exact curve. This is
//! the model every non-Microsoft implementation converged on because it matches measured
//! output, and it is an empirical match rather than a specification. What is *not*
//! guesswork is the parameters -- `GetAlphaBlendParams` returns the real gamma, contrast
//! and `ClearType` level for the user's monitor and their `ClearType`-tuner calibration.
//!
//! # Cost
//!
//! Three table lookups and two multiplies per channel. The tables are built once per
//! (gamma, contrast) pair, not per glyph or per frame.

/// Precomputed tables for one (gamma, contrast) pair.
pub struct TextGamma {
    gamma: f32,
    contrast: f32,
    /// `to_linear[v] = round(65535 * (v/255)^gamma)`
    to_linear: [u16; 256],
    /// `from_linear[x] = round(255 * (x*16/65535)^(1/gamma))`, indexed by `linear >> 4`.
    from_linear: [u8; 4096],
    /// `contrast_lut[c] = round(255 * enhance(c/255))`
    contrast_lut: [u8; 256],
}

impl TextGamma {
    pub fn new(gamma: f32, contrast: f32) -> Self {
        // A gamma of 0 or a negative one would produce NaNs that then propagate into the
        // framebuffer as black or garbage; clamp to a sane range rather than trusting the
        // platform to hand us something reasonable.
        let gamma = if gamma.is_finite() {
            gamma.clamp(1.0, 3.0)
        } else {
            1.8
        };
        let contrast = if contrast.is_finite() {
            contrast.clamp(0.0, 1.0)
        } else {
            0.5
        };

        let mut to_linear = [0u16; 256];
        for (v, slot) in to_linear.iter_mut().enumerate() {
            let x = (v as f32 / 255.0).powf(gamma);
            *slot = (x * 65535.0).round().clamp(0.0, 65535.0) as u16;
        }

        let mut from_linear = [0u8; 4096];
        for (i, slot) in from_linear.iter_mut().enumerate() {
            // Index i represents linear value i*16 (we drop the low 4 bits).
            let x = (i as f32 * 16.0) / 65535.0;
            *slot = (x.powf(1.0 / gamma) * 255.0).round().clamp(0.0, 255.0) as u8;
        }

        let mut contrast_lut = [0u8; 256];
        for (c, slot) in contrast_lut.iter_mut().enumerate() {
            let a = c as f32 / 255.0;
            let boosted = a * (1.0 + contrast * (1.0 - a));
            *slot = (boosted * 255.0).round().clamp(0.0, 255.0) as u8;
        }

        Self {
            gamma,
            contrast,
            to_linear,
            from_linear,
            contrast_lut,
        }
    }

    #[inline]
    pub fn matches(&self, gamma: f32, contrast: f32) -> bool {
        (self.gamma - gamma).abs() < 1e-4 && (self.contrast - contrast).abs() < 1e-4
    }

    /// Blend one channel: `src` over `dst` at coverage `cov`, gamma-corrected.
    #[inline]
    pub fn blend_channel(&self, src: u8, dst: u8, cov: u8) -> u8 {
        let a = u32::from(self.contrast_lut[cov as usize]);
        if a == 0 {
            return dst;
        }
        if a >= 255 {
            return src;
        }
        let s = u32::from(self.to_linear[src as usize]);
        let d = u32::from(self.to_linear[dst as usize]);
        let mixed = (s * a + d * (255 - a)) / 255;
        self.from_linear[(mixed >> 4) as usize]
    }
}

impl Default for TextGamma {
    fn default() -> Self {
        // Windows' defaults, which are also a sane neutral choice elsewhere.
        Self::new(1.8, 0.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_coverage_leaves_the_destination_and_full_coverage_takes_the_source() {
        let g = TextGamma::default();
        for dst in [0u8, 0x19, 0x80, 0xff] {
            for src in [0u8, 0x40, 0xff] {
                assert_eq!(
                    g.blend_channel(src, dst, 0),
                    dst,
                    "cov 0 must not touch dst"
                );
                assert_eq!(g.blend_channel(src, dst, 255), src, "cov 255 must be src");
            }
        }
    }

    /// The blend must be monotone in coverage: more coverage always moves further toward
    /// the text colour. A non-monotone ramp shows up as banding along glyph edges.
    #[test]
    fn coverage_moves_monotonically_toward_the_source() {
        let g = TextGamma::default();
        let (src, dst) = (255u8, 0x19u8);
        let mut previous = g.blend_channel(src, dst, 0);
        for cov in 1..=255u8 {
            let now = g.blend_channel(src, dst, cov);
            assert!(
                now >= previous,
                "coverage {cov} went backwards: {previous} -> {now}"
            );
            previous = now;
        }
        assert_eq!(previous, src);
    }

    /// Contrast enhancement must *lighten* light-on-dark text relative to a plain blend,
    /// which is the correction it exists for. With contrast 0 the two agree.
    #[test]
    fn contrast_boosts_partial_coverage_and_zero_contrast_does_not() {
        let plain = TextGamma::new(1.8, 0.0);
        let boosted = TextGamma::new(1.8, 0.5);
        let (src, dst) = (255u8, 0u8);
        assert!(
            boosted.blend_channel(src, dst, 128) > plain.blend_channel(src, dst, 128),
            "enhanced contrast did not boost half-coverage"
        );
        // ...but never past the endpoints.
        assert_eq!(boosted.blend_channel(src, dst, 0), dst);
        assert_eq!(boosted.blend_channel(src, dst, 255), src);
    }

    /// Gamma correction must place half-coverage *above* the linear midpoint for
    /// light-on-dark text. If it does not, the gamma is being applied the wrong way round
    /// and text will look too thin.
    #[test]
    fn half_coverage_is_gamma_corrected_not_linear() {
        let g = TextGamma::new(1.8, 0.0);
        let mid = g.blend_channel(255, 0, 128);
        assert!(
            mid > 140,
            "half coverage produced {mid}, which is at or below the linear midpoint -- \
             gamma is inverted or not applied"
        );
    }

    #[test]
    fn degenerate_parameters_do_not_produce_garbage() {
        for (gamma, contrast) in [(0.0, 0.0), (f32::NAN, f32::NAN), (-1.0, 5.0), (99.0, 99.0)] {
            let g = TextGamma::new(gamma, contrast);
            let v = g.blend_channel(255, 0, 128);
            assert!(v > 0, "produced {v} for gamma={gamma} contrast={contrast}");
        }
    }

    #[test]
    fn matches_reports_parameter_identity() {
        let g = TextGamma::new(1.8, 0.5);
        assert!(g.matches(1.8, 0.5));
        assert!(!g.matches(2.2, 0.5));
        assert!(!g.matches(1.8, 0.0));
    }
}

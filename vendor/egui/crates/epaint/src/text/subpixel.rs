//! TASKMAN-FORK: sub-pixel (LCD / `ClearType`) glyph coverage.
//!
//! An LCD panel does not have square pixels. Each one is three vertically-striped
//! sub-pixels -- red, green, blue, left to right on the overwhelmingly common layout --
//! and each can be lit independently. Rendering text against that stripe rather than
//! against the whole pixel triples the effective horizontal resolution, which is what
//! makes Windows' `ClearType` text look sharp at 9-13 px where grayscale anti-aliasing
//! looks soft.
//!
//! The cost is colour fringing: a stem that covers only the red sub-pixel of a pixel
//! lights that sub-pixel alone, and the eye can see the tint. Done properly the fringes
//! are faint and read as sharpness; done carelessly they read as rainbow noise. Two
//! things separate the two, and both are here:
//!
//! * **The filter.** Raw 3x samples produce heavy fringing. A short symmetric FIR filter
//!   spreads each sub-pixel's energy across its neighbours, trading a little sharpness
//!   for a lot less colour. This is the same job `FreeType`'s `FT_LCD_FILTER_*` does.
//! * **Gamma and contrast.** Blending coverage linearly in sRGB makes light-on-dark text
//!   look bloated and dark-on-light text look anaemic. Windows blends in a
//!   gamma-corrected space with a contrast boost; [`crate::text::TextOptions`] carries
//!   those parameters and the *renderer* applies them, because they depend on the text
//!   and background colours, which the rasterizer does not know.
//!
//! # Why the atlas needs no new format
//!
//! `TextureAtlas` already stores `Color32`. Per-channel coverage goes straight into R, G
//! and B, with the alpha channel carrying a representative coverage for consumers that
//! cannot do per-channel blending. Only a renderer that knows the texel is sub-pixel
//! coverage may use the RGB channels that way -- see [`SubpixelMode`].

/// Whether glyphs are rasterized with per-channel (sub-pixel) coverage.
///
/// **This is a contract with the renderer, not just a quality knob.** With
/// [`Self::Off`] an atlas texel is `(c, c, c, c)` and any backend can multiply it by the
/// text colour. With sub-pixel coverage the texel is `(cov_r, cov_g, cov_b, cov_max)`,
/// which is only meaningful to a renderer that blends each channel against its own
/// coverage. A GPU backend cannot: that needs dual-source blending, which egui's
/// pipelines do not use. Handing a sub-pixel atlas to `egui_glow` or `egui-wgpu` produces
/// rainbow-tinted text, so the choice must be gated on the active renderer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SubpixelMode {
    /// Grayscale coverage; every backend understands it.
    #[default]
    Off,

    /// Per-channel coverage for a horizontal RGB stripe -- the usual desktop LCD.
    Rgb,

    /// Per-channel coverage for a horizontal BGR stripe.
    ///
    /// Note that the *atlas* is always written in RGB order. Because the filter is
    /// symmetric, filtering in RGB and swapping R with B at blend time is algebraically
    /// identical to filtering in BGR, so panel order is a renderer-side flag and does not
    /// force an atlas rebuild. That matters on a multi-monitor desk with mixed panels.
    Bgr,
}

impl SubpixelMode {
    #[inline]
    pub fn is_off(self) -> bool {
        self == Self::Off
    }

    /// Does the renderer need to swap the red and blue coverage channels?
    #[inline]
    pub fn swaps_rb(self) -> bool {
        self == Self::Bgr
    }
}

/// A symmetric 5-tap FIR filter over sub-pixel samples, in fixed point summing to 256.
///
/// Five taps span 5/3 of a pixel, so a stem's energy reaches at most one pixel either
/// side. Longer filters suppress colour further but visibly blur; shorter ones fringe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LcdFilter(pub [u16; 5]);

impl LcdFilter {
    /// `[1, 2, 3, 2, 1] / 9` -- the filter described in the original `ClearType` work.
    ///
    /// Softer, and suppresses colour more aggressively.
    pub const CLASSIC: Self = Self([28, 57, 86, 57, 28]);

    /// `FreeType`'s `FT_LCD_FILTER_DEFAULT`, tuned by eye *against* `ClearType` output.
    ///
    /// Noticeably sharper than [`Self::CLASSIC`] with slightly more colour. This is the
    /// default because the reference we are matching -- Windows 11 -- reads sharp.
    pub const FREETYPE_DEFAULT: Self = Self([8, 77, 86, 77, 8]);

    /// No filtering: raw 3x samples. Maximum sharpness, heavy fringing.
    ///
    /// Present for A/B comparison; not a sensible default on any real panel.
    pub const NONE: Self = Self([0, 0, 256, 0, 0]);

    /// The taps must sum to exactly 256, or a fully-covered glyph does not reach full
    /// coverage and all text renders too light.
    #[inline]
    pub fn is_normalized(self) -> bool {
        self.0.iter().map(|&t| u32::from(t)).sum::<u32>() == 256
    }
}

impl Default for LcdFilter {
    fn default() -> Self {
        Self::FREETYPE_DEFAULT
    }
}

/// Filter a row of 3x horizontal coverage samples down to per-pixel RGB coverage.
///
/// `samples` holds `3 * width` sub-pixel coverages. Output is `width` RGB triples. Taps
/// reaching outside the row read as zero, which is correct: the glyph bitmap was expanded
/// by one pixel on each side precisely so the filter tail has room.
pub fn filter_row(samples: &[u8], width: usize, filter: LcdFilter, out: &mut Vec<[u8; 3]>) {
    debug_assert!(
        filter.is_normalized(),
        "LCD filter taps must sum to 256, or text renders too light"
    );
    let taps = filter.0;
    let n = samples.len();
    out.clear();
    out.reserve(width);

    for x in 0..width {
        let mut rgb = [0u8; 3];
        for (channel, slot) in rgb.iter_mut().enumerate() {
            let centre = 3 * x + channel;
            let mut acc = 0u32;
            for (t, &tap) in taps.iter().enumerate() {
                // Tap t is centred at offset (t - 2).
                let idx = centre as isize + t as isize - 2;
                if idx < 0 || idx as usize >= n {
                    continue;
                }
                acc += u32::from(tap) * u32::from(samples[idx as usize]);
            }
            *slot = ((acc + 128) >> 8).min(255) as u8;
        }
        out.push(rgb);
    }
}

/// Blend per-channel coverage toward its grayscale average.
///
/// `level` is `DirectWrite`'s `ClearType` level: 1.0 keeps full sub-pixel coverage, 0.0
/// collapses to grayscale. Users who find fringing distracting turn this down in the
/// `ClearType` tuner, and honouring it is part of "done properly".
#[inline]
pub fn apply_cleartype_level(rgb: [u8; 3], level: f32) -> [u8; 3] {
    if level >= 1.0 {
        return rgb;
    }
    let level = level.clamp(0.0, 1.0);
    let gray = (u32::from(rgb[0]) + u32::from(rgb[1]) + u32::from(rgb[2]) + 1) / 3;
    let mut out = [0u8; 3];
    for (i, slot) in out.iter_mut().enumerate() {
        let c = f32::from(rgb[i]);
        *slot = (gray as f32 + level * (c - gray as f32))
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_filters_are_normalized() {
        // A filter that does not sum to 256 makes every glyph too light -- a mistake that
        // is easy to make when hand-tuning taps and hard to spot by eye.
        assert!(LcdFilter::CLASSIC.is_normalized());
        assert!(LcdFilter::FREETYPE_DEFAULT.is_normalized());
        assert!(LcdFilter::NONE.is_normalized());
        assert!(!LcdFilter([1, 2, 3, 2, 1]).is_normalized());
    }

    #[test]
    fn the_filters_are_symmetric() {
        // An asymmetric filter shifts the glyph horizontally by a fraction of a pixel and
        // makes one side's fringe warmer than the other's.
        for f in [
            LcdFilter::CLASSIC,
            LcdFilter::FREETYPE_DEFAULT,
            LcdFilter::NONE,
        ] {
            assert_eq!(f.0[0], f.0[4], "{f:?}");
            assert_eq!(f.0[1], f.0[3], "{f:?}");
        }
    }

    #[test]
    fn full_coverage_filters_to_full_coverage() {
        let samples = vec![255u8; 3 * 8];
        let mut out = Vec::new();
        for f in [
            LcdFilter::CLASSIC,
            LcdFilter::FREETYPE_DEFAULT,
            LcdFilter::NONE,
        ] {
            filter_row(&samples, 8, f, &mut out);
            // The outermost pixels lose part of the filter tail to the row edge; the
            // interior must be fully opaque.
            for (x, rgb) in out.iter().enumerate().take(6).skip(2) {
                assert_eq!(*rgb, [255, 255, 255], "{f:?} at x={x}");
            }
        }
    }

    #[test]
    fn empty_coverage_stays_empty() {
        let samples = vec![0u8; 3 * 4];
        let mut out = Vec::new();
        filter_row(&samples, 4, LcdFilter::FREETYPE_DEFAULT, &mut out);
        assert!(out.iter().all(|&rgb| rgb == [0, 0, 0]));
    }

    /// A single lit sub-pixel must produce a *tinted* pixel, not a gray one -- that is
    /// the entire point -- and the filter must spread some of it to the neighbours.
    #[test]
    fn one_lit_subpixel_tints_its_channel_and_bleeds_to_neighbours() {
        let mut samples = vec![0u8; 3 * 5];
        samples[3 * 2] = 255; // the RED sub-pixel of pixel 2
        let mut out = Vec::new();
        filter_row(&samples, 5, LcdFilter::FREETYPE_DEFAULT, &mut out);

        assert!(
            out[2][0] > out[2][1],
            "the red channel must dominate its own pixel"
        );
        assert!(
            out[1][1] > 0 || out[1][2] > 0,
            "the filter must bleed into the previous pixel, or it is not filtering"
        );
    }

    /// With `NONE`, sub-pixel samples map one-to-one onto channels. This is the clearest
    /// statement of what the 3x layout means.
    #[test]
    fn the_unfiltered_mapping_is_one_sample_per_channel() {
        let mut samples = vec![0u8; 3 * 2];
        samples[0] = 255; // pixel 0, red
        samples[4] = 128; // pixel 1, green
        let mut out = Vec::new();
        filter_row(&samples, 2, LcdFilter::NONE, &mut out);
        assert_eq!(out[0], [255, 0, 0]);
        assert_eq!(out[1], [0, 128, 0]);
    }

    #[test]
    fn cleartype_level_interpolates_toward_grayscale() {
        let rgb = [255, 128, 0];
        assert_eq!(apply_cleartype_level(rgb, 1.0), rgb, "1.0 is a no-op");
        let gray = apply_cleartype_level(rgb, 0.0);
        assert_eq!(gray[0], gray[1], "0.0 must be fully gray");
        assert_eq!(gray[1], gray[2], "0.0 must be fully gray");
        let half = apply_cleartype_level(rgb, 0.5);
        assert!(half[0] < rgb[0] && half[0] > gray[0], "0.5 sits between");
    }

    #[test]
    fn subpixel_mode_reports_channel_order() {
        assert!(SubpixelMode::Off.is_off());
        assert!(!SubpixelMode::Rgb.swaps_rb());
        assert!(SubpixelMode::Bgr.swaps_rb());
    }
}

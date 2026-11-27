//! Everything related to text, fonts, text layout, cursors etc.

pub mod cursor;
// TASKMAN-FORK: sub-pixel (LCD) glyph coverage.
pub mod subpixel;
pub use subpixel::{LcdFilter, SubpixelMode};
mod font;
mod fonts;
mod index;
mod text_layout;
mod text_layout_types;

pub use {
    fonts::{
        FontData, FontDefinitions, FontFamily, FontId, FontInsert, FontPriority, FontTweak,
        FontVariationAxis, Fonts, FontsImpl, FontsView, HintingTarget, InsertFontFamily,
        SmoothHinting,
    },
    index::{ByteIndex, ByteRange, ByteRangeExt, CharIndex, CharRange, CharRangeExt},
    text_layout::*,
    text_layout_types::*,
};

/// Suggested character to use to replace those in password text fields.
pub const PASSWORD_REPLACEMENT_CHAR: char = '•';

/// Controls how we render text
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct TextOptions {
    /// Maximum size of the font texture.
    pub max_texture_side: usize,

    /// Controls how to convert glyph colors when writing to the font atlas.
    pub color_transfer_function: crate::FontColorTransferFunction,

    /// Whether to enable font hinting
    ///
    /// (round some font coordinates to pixels for sharper text).
    ///
    /// Default is `true`.
    pub font_hinting: bool,

    /// Enable sub-pixel binning for glyphs.
    ///
    /// Sub-pixel binning renders each glyph at up to four fractional horizontal offsets,
    /// giving more even kerning at the cost of more atlas space.
    ///
    /// It also lead to text looking more blurry.
    ///
    /// This is always disabled for CJK characters (which have too many unique glyphs).
    ///
    /// Can be overridden per font with [`FontTweak::subpixel_binning`].
    ///
    /// Default: `true`.
    pub subpixel_binning: bool,

    /// TASKMAN-FORK: rasterize glyphs with per-channel (LCD sub-pixel) coverage.
    ///
    /// This changes what an atlas texel *means*, so it is a contract with the renderer
    /// rather than a quality setting -- see [`crate::text::SubpixelMode`]. Only enable it
    /// when the active backend blends each channel against its own coverage. The software
    /// renderer always can; WGPU needs dual-source blending. A renderer that multiplies
    /// the atlas as an ordinary premultiplied texture draws rainbow-tinted text. LCD
    /// coverage uses `(cov_r, cov_g, cov_b, 0)`; alpha is a tag, never coverage.
    ///
    /// Default: [`SubpixelMode::Off`].
    pub subpixel: SubpixelMode,

    /// TASKMAN-FORK: the FIR filter applied across sub-pixel samples.
    ///
    /// Unused when [`Self::subpixel`] is off.
    pub lcd_filter: LcdFilter,

    /// TASKMAN-FORK: blend per-channel coverage toward grayscale.
    ///
    /// `1.0` is full sub-pixel rendering, `0.0` is grayscale. Mirrors `DirectWrite`'s
    /// `ClearType` level, which users set in the `ClearType` tuner.
    ///
    /// Default: `1.0`.
    pub cleartype_level: f32,

    /// TASKMAN-FORK: gamma of the text blend space.
    ///
    /// Strictly a *blend*-time value -- it does not affect how glyphs are rasterized, so
    /// changing it invalidates the atlas for nothing. It lives here anyway because this
    /// is the one channel a renderer can read the text configuration from, and it is only
    /// ever changed when the user picks a different smoothing profile, which rebuilds the
    /// atlas regardless.
    ///
    /// Windows' default is 1.8. Lower renders thinner on dark surfaces and heavier on light
    /// surfaces. The software renderer applies the full nonlinear destination-gamma model;
    /// the WGPU dual-source path calibrates both `Unorm` and `sRGB` blend weights against
    /// [`Self::text_background_luma`] (or foreground polarity when negative) to match it
    /// without framebuffer reads.
    pub text_gamma: f32,

    /// TASKMAN-FORK: enhanced-contrast boost applied to coverage before blending.
    ///
    /// Windows' default is 0.5. Zero renders thinner.
    pub text_contrast: f32,

    /// TASKMAN-FORK: expected sRGB background luminance (`0.0..=1.0`), or `-1.0` when
    /// unknown (polarity is then judged against `0.5`).
    ///
    /// Two consumers: the GPU fixed-function LCD blend calibrates against it, and
    /// [`crate::FontColorTransferFunction::PolarityGamma`] uses it to decide whether a text
    /// colour is light-on-dark or dark-on-light.
    pub text_background_luma: f32,

    /// TASKMAN-FORK: expected sRGB luminance (`0.0..=1.0`) of the surface under text whose
    /// polarity opposes [`Self::text_background_luma`] -- light text in a light theme sits on
    /// a dark toast or badge, not on the page -- or `-1.0` when unknown (the GPU LCD blend
    /// then assumes the pure black/white endpoint). When known, text polarity splits at the
    /// midpoint of the two surfaces ([`crate::polarity_split`]).
    pub text_inverse_background_luma: f32,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            max_texture_side: 2048, // Small but portable
            color_transfer_function: crate::FontColorTransferFunction::default(),
            font_hinting: true,
            subpixel_binning: true,
            subpixel: SubpixelMode::Off,
            lcd_filter: LcdFilter::FREETYPE_DEFAULT,
            cleartype_level: 1.0,
            text_gamma: 1.8,
            text_contrast: 0.5,
            text_background_luma: -1.0,
            text_inverse_background_luma: -1.0,
        }
    }
}

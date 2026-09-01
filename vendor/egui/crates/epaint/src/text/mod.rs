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
    /// when the active backend blends each channel against its own coverage; a GPU
    /// backend cannot, and will draw rainbow-tinted text.
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
        }
    }
}

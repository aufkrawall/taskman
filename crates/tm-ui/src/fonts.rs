//! Font setup: prefer OS-native fonts for a native look (Segoe UI Variable
//! on Windows 11, SF Pro via SFNS on macOS, Fontconfig-selected faces on
//! Linux), fall back to known distro paths and then egui's bundled defaults.
//!
//! System font files are read on a background thread so startup stays fast.
//!
//! ## Why the hinting tweak matters more than anything else here
//!
//! epaint rasterizes through `skrifa`, which runs the font's real TrueType
//! instructions — but epaint's DEFAULT `SmoothHinting` sets both
//! `preserve_linear_metrics: true` and `symmetric_rendering: true`, and both
//! of those switch horizontal grid-fitting OFF. Vertical stems then land on
//! fractional pixel columns and get split across two grey columns, which is
//! the "blurry, fat" look. Windows grid-fits horizontally, which is what puts
//! a stem fully inside one pixel column. [`hinting_target`] turns that back on
//! for the sharp profile.

use egui::epaint::text::{FontTweak, HintingTarget, SmoothHinting, VariationCoords};
use egui::{self, FontData, FontDefinitions, FontFamily};
use std::sync::{Arc, OnceLock};
use tm_core::settings::TextSmoothing;
use tm_platform::text_rendering::HintStyleChoice;

pub const FAMILY_SEMIBOLD: &str = "semibold";
pub const FAMILY_DISPLAY: &str = "display";
pub const FAMILY_SMALL: &str = "small";

const FAMILIES_READY_KEY: &str = "tm-fonts-families-ready";

/// Ensure the named semantic families (`semibold`, `display`, `small`) exist on
/// `ctx` before the first frame paints (before async system fonts finish loading,
/// or in headless tests that skip `install_async`).
pub fn ensure_named_families(ctx: &egui::Context) {
    let ready = ctx.data(|d| d.get_temp::<bool>(egui::Id::new(FAMILIES_READY_KEY)));
    if ready != Some(true) {
        ctx.set_fonts(initial_definitions());
        ctx.data_mut(|d| d.insert_temp(egui::Id::new(FAMILIES_READY_KEY), true));
    }
}

/// Default bundled definitions with our semantic named families registered as
/// aliases to `Proportional` so Frame 1 never panics before system fonts arrive.
pub fn initial_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let base = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for name in [FAMILY_SEMIBOLD, FAMILY_DISPLAY, FAMILY_SMALL] {
        fonts
            .families
            .insert(FontFamily::Name(Arc::from(name)), base.clone());
    }
    fonts
}

pub fn install_async(ctx: egui::Context) {
    ensure_named_families(&ctx);
    let ctx2 = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("tm-fonts".into())
        .spawn(move || {
            let defs = build_definitions();
            *apply_result().lock().unwrap_or_else(|e| e.into_inner()) = Some(defs);
            ctx2.request_repaint();
        });
    if spawned.is_err() {
        let defs = build_definitions();
        ctx.set_fonts(defs);
        ctx.data_mut(|d| d.insert_temp(egui::Id::new(FAMILIES_READY_KEY), true));
    }
}

/// Re-apply the system fonts with the current [`TextSmoothing`] tweak.
///
/// The hinting target lives in each face's [`FontTweak`], not in the global
/// text options, so changing the smoothing profile has to hand egui a fresh
/// [`FontDefinitions`]. The raw font bytes are cached once in process memory
/// so re-applying never re-reads disk or duplicates the font files.
pub fn reapply(ctx: &egui::Context) {
    ctx.set_fonts(build_definitions());
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(FAMILIES_READY_KEY), true));
}

/// Grid-fitting profile for the active smoothing choice and platform hinting preference.
///
/// `preserve_linear_metrics: false` + `symmetric_rendering: false` is the
/// combination epaint documents as "lets the (auto)hinter snap horizontally
/// for crisper stems"; egui positions glyphs from the shaper's advances, so
/// this changes sharpness, not layout.
fn hinting_target(smoothing: TextSmoothing, hint_style: HintStyleChoice) -> HintingTarget {
    match smoothing {
        TextSmoothing::Sharp => HintingTarget::Smooth(SmoothHinting {
            light: false,
            symmetric_rendering: false,
            preserve_linear_metrics: false,
        }),
        TextSmoothing::Standard => match hint_style {
            HintStyleChoice::Slight => HintingTarget::Smooth(SmoothHinting {
                light: true,
                symmetric_rendering: true,
                preserve_linear_metrics: true,
            }),
            HintStyleChoice::Full => HintingTarget::Smooth(SmoothHinting {
                light: false,
                symmetric_rendering: false,
                preserve_linear_metrics: false,
            }),
            HintStyleChoice::Default | HintStyleChoice::None | HintStyleChoice::Medium => {
                HintingTarget::default()
            }
        },
        TextSmoothing::Smooth => HintingTarget::default(),
    }
}

/// Tweak applied to every face we install.
fn tweak(
    smoothing: TextSmoothing,
    hint_style: HintStyleChoice,
    coords: VariationCoords,
) -> FontTweak {
    FontTweak {
        hinting_target: hinting_target(smoothing, hint_style),
        coords,
        ..Default::default()
    }
}

fn apply_result() -> &'static std::sync::Mutex<Option<FontDefinitions>> {
    static SLOT: OnceLock<std::sync::Mutex<Option<FontDefinitions>>> = OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

pub fn poll_async_apply(ctx: &egui::Context) {
    let ready = apply_result()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(defs) = ready {
        tracing::info!("system fonts applied after first frame");
        ctx.set_fonts(defs);
        ctx.data_mut(|d| d.insert_temp(egui::Id::new(FAMILIES_READY_KEY), true));
    }
}

#[derive(Clone, Copy)]
struct LoadedFace {
    name: &'static str,
    /// Process-lifetime raw font bytes leaked once inside
    /// [`system_font_cache`]. Sharing `&'static [u8]` lets multiple `FontData`
    /// instances (e.g. Regular, Semibold, Display, Small of `SegUIVar.ttf`) and
    /// repeated `fonts::reapply` calls share one allocation with zero copies.
    bytes: &'static [u8],
    index: u32,
    is_segoe_var: bool,
}

impl LoadedFace {
    fn to_font_data(
        self,
        smoothing: TextSmoothing,
        hint_style: HintStyleChoice,
        coords: VariationCoords,
    ) -> Arc<FontData> {
        let mut data = FontData::from_static(self.bytes);
        data.index = self.index;
        Arc::new(data.tweak(tweak(smoothing, hint_style, coords)))
    }
}

struct SystemFontCache {
    proportional: Option<LoadedFace>,
    semibold: Option<LoadedFace>,
    symbol: Option<LoadedFace>,
    monospace: Option<LoadedFace>,
}

fn system_font_cache() -> &'static SystemFontCache {
    static CACHE: OnceLock<SystemFontCache> = OnceLock::new();
    CACHE.get_or_init(load_system_fonts)
}

type FontSource = (String, u32);

struct ProportionalCandidate {
    name: &'static str,
    sources: Vec<FontSource>,
    is_segoe_var: bool,
}

fn load_system_fonts() -> SystemFontCache {
    let mut proportional_candidates: Vec<ProportionalCandidate> = Vec::new();
    #[cfg(target_os = "linux")]
    if let Some(face) = fontconfig_match_face("sans-serif") {
        proportional_candidates.push(ProportionalCandidate {
            name: "FontconfigSans",
            sources: vec![face],
            is_segoe_var: false,
        });
    }
    proportional_candidates.extend([
        // Windows 11's actual UI face first; `segoeui.ttf` is the Windows 10
        // static fallback.
        ProportionalCandidate {
            name: "SegoeUIVariable",
            sources: vec![(r"C:\Windows\Fonts\SegUIVar.ttf".into(), 0)],
            is_segoe_var: true,
        },
        ProportionalCandidate {
            name: "SegoeUI",
            sources: vec![(r"C:\Windows\Fonts\segoeui.ttf".into(), 0)],
            is_segoe_var: false,
        },
        ProportionalCandidate {
            name: "SFNS",
            sources: vec![("/System/Library/Fonts/SFNS.ttf".into(), 0)],
            is_segoe_var: false,
        },
        ProportionalCandidate {
            name: "NotoSans",
            sources: vec![
                (
                    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf".into(),
                    0,
                ),
                ("/usr/share/fonts/TTF/NotoSans-Regular.ttf".into(), 0),
                ("/usr/share/noto-cjk/NotoSansCJK-Regular.ttc".into(), 0),
            ],
            is_segoe_var: false,
        },
        ProportionalCandidate {
            name: "DejaVuSans",
            sources: vec![("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into(), 0)],
            is_segoe_var: false,
        },
    ]);

    let proportional = proportional_candidates.iter().find_map(|candidate| {
        let (bytes, index) = load_first_face(&candidate.sources)?;
        tracing::info!(
            font = candidate.name,
            index,
            "using system proportional font"
        );
        Some(LoadedFace {
            name: candidate.name,
            bytes,
            index,
            is_segoe_var: candidate.is_segoe_var,
        })
    });

    // When Segoe UI Variable is available, its `wght: 600` axis provides Semibold
    // from the exact same file bytes — no need to read or parse `segoeuib.ttf`.
    let semibold = if proportional.is_some_and(|p| p.is_segoe_var) {
        None
    } else {
        let mut bold_candidates: Vec<(&'static str, Vec<FontSource>)> = Vec::new();
        #[cfg(target_os = "linux")]
        if let Some(face) = fontconfig_match_face("sans-serif:weight=demibold") {
            bold_candidates.push(("FontconfigSemibold", vec![face]));
        }
        bold_candidates.extend([
            (
                "SegoeUI-Semibold",
                vec![
                    (r"C:\Windows\Fonts\seguisb.ttf".into(), 0),
                    (r"C:\Windows\Fonts\segoeuib.ttf".into(), 0),
                ],
            ),
            (
                "NotoSans-Bold",
                vec![("/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf".into(), 0)],
            ),
            (
                "DejaVuSans-Bold",
                vec![(
                    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf".into(),
                    0,
                )],
            ),
        ]);
        bold_candidates.iter().find_map(|(name, paths)| {
            let (bytes, index) = load_first_face(paths)?;
            Some(LoadedFace {
                name,
                bytes,
                index,
                is_segoe_var: false,
            })
        })
    };

    // Symbol fallback: on Windows, Segoe UI relies on Segoe UI Symbol for
    // math, symbols, and punctuation (like one-dot leader U+2024).
    let symbol_candidates: &[(&'static str, Vec<FontSource>)] = &[
        (
            "SegoeUISymbol",
            vec![(r"C:\Windows\Fonts\seguisym.ttf".into(), 0)],
        ),
        (
            "NotoSansSymbols",
            vec![
                (
                    "/usr/share/fonts/truetype/noto/NotoSansSymbols-Regular.ttf".into(),
                    0,
                ),
                (
                    "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf".into(),
                    0,
                ),
            ],
        ),
    ];
    let symbol = symbol_candidates.iter().find_map(|(name, paths)| {
        let (bytes, index) = load_first_face(paths)?;
        Some(LoadedFace {
            name,
            bytes,
            index,
            is_segoe_var: false,
        })
    });

    let mut mono_candidates: Vec<(&'static str, Vec<FontSource>)> = Vec::new();
    #[cfg(target_os = "linux")]
    if let Some(face) = fontconfig_match_face("monospace") {
        mono_candidates.push(("FontconfigMono", vec![face]));
    }
    mono_candidates.extend([
        (
            "CascadiaMono",
            vec![(r"C:\Windows\Fonts\CascadiaMono.ttf".into(), 0)],
        ),
        (
            "Consolas",
            vec![(r"C:\Windows\Fonts\consola.ttf".into(), 0)],
        ),
        (
            "JetBrainsMono",
            vec![(
                "/usr/share/fonts/truetype/JetBrainsMono/JetBrainsMono-Regular.ttf".into(),
                0,
            )],
        ),
        (
            "DejaVuSansMono",
            vec![(
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf".into(),
                0,
            )],
        ),
        (
            "Menlo",
            vec![
                ("/System/Library/Fonts/Menlo.ttc".into(), 0),
                ("/System/Library/Fonts/Monaco.ttf".into(), 0),
            ],
        ),
    ]);
    let monospace = mono_candidates.iter().find_map(|(name, paths)| {
        let (bytes, index) = load_first_face(paths)?;
        tracing::info!(font = *name, index, "using system monospace font");
        Some(LoadedFace {
            name,
            bytes,
            index,
            is_segoe_var: false,
        })
    });

    SystemFontCache {
        proportional,
        semibold,
        symbol,
        monospace,
    }
}

fn build_definitions() -> FontDefinitions {
    let smoothing = crate::theme::text_smoothing();
    let hint_style = crate::theme::subpixel_params().hint_style;
    let cache = system_font_cache();
    let mut fonts = FontDefinitions::default();

    // Drop a bundled face from the chain the system face replaces, so epaint
    // does not keep parsing a font nothing draws with. One FAMILY at a time:
    // epaint also lists `Ubuntu-Light` in the Monospace chain as its maths and
    // symbol fallback ("fallback for √ etc"), which a system monospace face
    // need not cover, so removing it from every family would trade a few KB of
    // skrifa metadata for tofu in every monospace cell.
    if cache.proportional.is_some() {
        remove_font_from(&mut fonts, FontFamily::Proportional, "Ubuntu-Light");
    }
    if cache.monospace.is_some() {
        remove_font_from(&mut fonts, FontFamily::Monospace, "Hack");
    }

    if let Some(prop) = cache.proportional {
        if prop.is_segoe_var {
            // Variable-font axes for Segoe UI Variable: one file in RAM supplies
            // Text (wght 400, opsz 10.5), Semibold (wght 600, opsz 10.5),
            // Display (wght 400, opsz 36), and Small (wght 400, opsz 8).
            let text_coords = VariationCoords::new([(b"wght", 400.0), (b"opsz", 10.5)]);
            let semibold_coords = VariationCoords::new([(b"wght", 600.0), (b"opsz", 10.5)]);
            let display_coords = VariationCoords::new([(b"wght", 400.0), (b"opsz", 36.0)]);
            let small_coords = VariationCoords::new([(b"wght", 400.0), (b"opsz", 8.0)]);

            fonts.font_data.insert(
                prop.name.to_string(),
                prop.to_font_data(smoothing, hint_style, text_coords),
            );
            fonts.font_data.insert(
                "SegoeUIVariable-Semibold".into(),
                prop.to_font_data(smoothing, hint_style, semibold_coords),
            );
            fonts.font_data.insert(
                "SegoeUIVariable-Display".into(),
                prop.to_font_data(smoothing, hint_style, display_coords),
            );
            fonts.font_data.insert(
                "SegoeUIVariable-Small".into(),
                prop.to_font_data(smoothing, hint_style, small_coords),
            );
        } else {
            fonts.font_data.insert(
                prop.name.to_string(),
                prop.to_font_data(smoothing, hint_style, VariationCoords::default()),
            );
        }

        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, prop.name.to_string());
    }

    if let Some(bold) = cache.semibold {
        fonts.font_data.insert(
            bold.name.to_string(),
            bold.to_font_data(smoothing, hint_style, VariationCoords::default()),
        );
    }

    if let Some(sym) = cache.symbol {
        fonts.font_data.insert(
            sym.name.to_string(),
            sym.to_font_data(smoothing, hint_style, VariationCoords::default()),
        );
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .push(sym.name.to_string());
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push(sym.name.to_string());
    }

    if let Some(mono) = cache.monospace {
        fonts.font_data.insert(
            mono.name.to_string(),
            mono.to_font_data(smoothing, hint_style, VariationCoords::default()),
        );
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, mono.name.to_string());
    }

    // Populate the semantic named families (`semibold`, `display`, `small`) with
    // the full Proportional fallback chain behind their primary face.
    let prop_chain = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let mut semibold_chain = prop_chain.clone();
    if cache.proportional.is_some_and(|p| p.is_segoe_var) {
        semibold_chain.insert(0, "SegoeUIVariable-Semibold".into());
    } else if let Some(bold) = cache.semibold {
        semibold_chain.insert(0, bold.name.to_string());
    }
    fonts
        .families
        .insert(FontFamily::Name(Arc::from(FAMILY_SEMIBOLD)), semibold_chain);

    let mut display_chain = prop_chain.clone();
    if cache.proportional.is_some_and(|p| p.is_segoe_var) {
        display_chain.insert(0, "SegoeUIVariable-Display".into());
    }
    fonts
        .families
        .insert(FontFamily::Name(Arc::from(FAMILY_DISPLAY)), display_chain);

    let mut small_chain = prop_chain;
    if cache.proportional.is_some_and(|p| p.is_segoe_var) {
        small_chain.insert(0, "SegoeUIVariable-Small".into());
    }
    fonts
        .families
        .insert(FontFamily::Name(Arc::from(FAMILY_SMALL)), small_chain);

    fonts
}

/// Drop `name` from ONE family's chain, and from the font table only once no
/// chain still lists it.
///
/// The table entry is what epaint parses and holds, so it has to go for the
/// RAM saving to be real — but only when nothing else still asks for it (see
/// the caller for why one font file can serve two families).
fn remove_font_from(fonts: &mut FontDefinitions, family: FontFamily, name: &str) {
    if let Some(list) = fonts.families.get_mut(&family) {
        list.retain(|entry| entry != name);
    }
    let still_listed = fonts.families.values().flatten().any(|listed| listed == name);
    if !still_listed {
        fonts.font_data.remove(name);
    }
}

/// Ask Fontconfig for the user's configured generic family and collection face index.
#[cfg(target_os = "linux")]
fn fontconfig_match_face(pattern: &str) -> Option<(String, u32)> {
    tm_platform::fontconfig::match_face(pattern)
}

fn load_first_face(paths: &[(String, u32)]) -> Option<(&'static [u8], u32)> {
    for (path, index) in paths {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // epaint panics on a face it cannot parse, and Fontconfig can answer
        // with PCF, Type 1 or other non-sfnt files; try the next candidate.
        if is_loadable_font_face(&bytes, *index) {
            let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
            return Some((leaked, *index));
        }
        tracing::warn!(
            path = %path,
            index = *index,
            "skipping font that is not a loadable TrueType/OpenType face"
        );
    }
    None
}

/// Whether `bytes` pass the header check epaint's parser (skrifa) starts
/// with: an sfnt table directory (TrueType or CFF-flavoured OpenType), or a
/// TrueType collection whose face at `index` is one.
fn is_loadable_font_face(bytes: &[u8], index: u32) -> bool {
    match bytes.get(..4) {
        Some(b"ttcf") => {
            let Some(num_fonts) = bytes
                .get(8..12)
                .and_then(|raw| Some(u32::from_be_bytes(raw.try_into().ok()?)))
            else {
                return false;
            };
            if index >= num_fonts {
                return false;
            }
            let Some(offset_pos) = (index as usize)
                .checked_mul(4)
                .and_then(|o| o.checked_add(12))
            else {
                return false;
            };
            bytes
                .get(offset_pos..offset_pos + 4)
                .and_then(|raw| usize::try_from(u32::from_be_bytes(raw.try_into().ok()?)).ok())
                .is_some_and(|offset| is_sfnt_face(bytes, offset))
        }
        _ => index == 0 && is_sfnt_face(bytes, 0),
    }
}

#[cfg(test)]
fn is_loadable_font(bytes: &[u8]) -> bool {
    is_loadable_font_face(bytes, 0)
}

fn is_sfnt_face(bytes: &[u8], at: usize) -> bool {
    let Some(header) = at.checked_add(12).and_then(|end| bytes.get(at..end)) else {
        return false;
    };
    let version_ok = matches!(&header[..4], b"\0\x01\0\0" | b"true" | b"OTTO");
    let tables = usize::from(u16::from_be_bytes([header[4], header[5]]));
    version_ok
        && tables > 0
        && (at + 12)
            .checked_add(tables * 16)
            .is_some_and(|end| end <= bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal sfnt header with `tables` directory records.
    fn sfnt(version: &[u8; 4], tables: u16) -> Vec<u8> {
        let mut out = version.to_vec();
        out.extend(tables.to_be_bytes());
        out.extend([0; 6]);
        out.extend(vec![0; usize::from(tables) * 16]);
        out
    }

    #[test]
    fn truetype_and_opentype_faces_are_loadable() {
        assert!(is_loadable_font(&sfnt(b"\0\x01\0\0", 3)));
        assert!(is_loadable_font(&sfnt(b"OTTO", 1)));
        assert!(is_loadable_font(&sfnt(b"true", 1)));
    }

    #[test]
    fn non_sfnt_and_truncated_files_are_rejected() {
        // PCF bitmap, Type 1 (PFB and PFA), WOFF2, empty file.
        assert!(!is_loadable_font(b"\x01fcp\x0f\0\0\0"));
        assert!(!is_loadable_font(b"\x80\x01\x10\x07\0\0%!PS-AdobeFont-1.0"));
        assert!(!is_loadable_font(b"%!PS-AdobeFont-1.0: Utopia"));
        assert!(!is_loadable_font(&sfnt(b"wOF2", 1)));
        assert!(!is_loadable_font(b""));
        // Table directory claims more records than the file holds.
        let mut short = sfnt(b"\0\x01\0\0", 2);
        short.truncate(short.len() - 1);
        assert!(!is_loadable_font(&short));
        assert!(!is_loadable_font(&sfnt(b"\0\x01\0\0", 0)));
    }

    #[test]
    fn collection_is_judged_by_its_first_face() {
        let collection = |first_face: &[u8]| {
            let mut out = b"ttcf\0\x01\0\0\0\0\0\x01".to_vec();
            out.extend(16u32.to_be_bytes());
            out.extend(first_face);
            out
        };
        assert!(is_loadable_font(&collection(&sfnt(b"\0\x01\0\0", 1))));
        assert!(!is_loadable_font(&collection(b"\x01fcp")));
        assert!(!is_loadable_font(
            b"ttcf\0\x01\0\0\0\0\0\x01\xff\xff\xff\xff"
        ));
    }

    #[test]
    fn collection_validates_requested_face_index() {
        let face0 = sfnt(b"\0\x01\0\0", 1);
        let face1 = sfnt(b"OTTO", 1);
        let off0 = 20u32; // 12-byte ttcf header + 2 * 4-byte offsets
        let off1 = off0 + face0.len() as u32;
        let mut ttc = b"ttcf\0\x01\0\0".to_vec();
        ttc.extend(2u32.to_be_bytes());
        ttc.extend(off0.to_be_bytes());
        ttc.extend(off1.to_be_bytes());
        ttc.extend(&face0);
        ttc.extend(&face1);

        assert!(is_loadable_font_face(&ttc, 0));
        assert!(is_loadable_font_face(&ttc, 1));
        assert!(!is_loadable_font_face(&ttc, 2));
        // Non-collection font files only accept face index 0.
        assert!(!is_loadable_font_face(&face0, 1));
    }

    #[test]
    fn semantic_named_families_are_registered_before_and_after_system_load() {
        let initial = initial_definitions();
        for name in [FAMILY_SEMIBOLD, FAMILY_DISPLAY, FAMILY_SMALL] {
            let chain = initial
                .families
                .get(&FontFamily::Name(Arc::from(name)))
                .expect("semantic family must exist in initial_definitions");
            assert!(!chain.is_empty());
        }

        let built = build_definitions();
        for name in [FAMILY_SEMIBOLD, FAMILY_DISPLAY, FAMILY_SMALL] {
            let chain = built
                .families
                .get(&FontFamily::Name(Arc::from(name)))
                .expect("semantic family must exist in build_definitions");
            assert!(!chain.is_empty());
        }
        // A system proportional face replaces the bundled one — but Ubuntu-Light
        // STAYS in the monospace chain, where epaint lists it as the maths and
        // symbol fallback ("fallback for √ etc"), and a system monospace face
        // need not cover those codepoints.
        if system_font_cache().proportional.is_some() {
            let proportional = built
                .families
                .get(&FontFamily::Proportional)
                .expect("proportional family must exist");
            assert!(
                !proportional.iter().any(|entry| entry == "Ubuntu-Light"),
                "the bundled proportional face must leave the proportional chain"
            );
            let monospace = built
                .families
                .get(&FontFamily::Monospace)
                .expect("monospace family must exist");
            assert!(
                monospace.iter().any(|entry| entry == "Ubuntu-Light"),
                "the monospace maths fallback must survive"
            );
        }
        if system_font_cache().monospace.is_some() {
            assert!(!built.font_data.contains_key("Hack"));
        }
    }

    #[test]
    fn fontconfig_hintstyle_maps_cleanly_under_standard_smoothing() {
        assert_eq!(
            hinting_target(TextSmoothing::Standard, HintStyleChoice::Slight),
            HintingTarget::Smooth(SmoothHinting {
                light: true,
                symmetric_rendering: true,
                preserve_linear_metrics: true,
            })
        );
        assert_eq!(
            hinting_target(TextSmoothing::Standard, HintStyleChoice::Full),
            HintingTarget::Smooth(SmoothHinting {
                light: false,
                symmetric_rendering: false,
                preserve_linear_metrics: false,
            })
        );
        assert_eq!(
            hinting_target(TextSmoothing::Sharp, HintStyleChoice::Slight),
            HintingTarget::Smooth(SmoothHinting {
                light: false,
                symmetric_rendering: false,
                preserve_linear_metrics: false,
            })
        );
    }
}

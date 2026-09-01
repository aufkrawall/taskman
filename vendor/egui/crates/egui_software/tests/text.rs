//! The glyph blitter must be indistinguishable from the triangle path.
//!
//! [`Painter::paint_shapes`] intercepts `Shape::Text` and blits each glyph straight from
//! the atlas instead of setting up two gouraud triangles. That is only a legitimate
//! optimisation if it is *invisible*, and the sub-pixel work in a later phase is only
//! trustworthy if the grayscale baseline it replaces was exact.
//!
//! So: render the same text both ways and require the results to agree.
//!
//! "Agree" is deliberately not "bit-identical", and the reason matters. The blit is the
//! *exact* computation: an integer texel fetch at an integer offset. The triangle path
//! interpolates UVs in floating point and then samples bilinearly, so its coordinate can
//! land at 1e-7 instead of 0 and blend in a sliver of the neighbouring texel. The two
//! therefore differ by at most one LSB, on a handful of pixels, and it is the *triangle*
//! path that carries the error. Demanding bit-identity would mean adding a fudge factor
//! to the general sampler so it matches the fast path -- backwards, and it would hide
//! exactly the kind of real divergence this test exists to catch.
//!
//! What is asserted instead: no channel may differ by more than 1, and only a negligible
//! fraction of painted pixels may differ at all. A placement bug, a colour-resolution bug
//! or a blend bug all produce differences far outside that.
//!
//! Everything is driven through a real `egui::Context`, so the galley, the atlas and the
//! texture deltas arrive exactly as they do in an application.

use egui::{Color32, FontId, Rect, Stroke, TextureId, pos2};
use egui_software::{Painter, ShapeContext, Target, TextMode, pack_rgb};

const W: u32 = 360;
const H: u32 = 140;
const BG: Color32 = Color32::from_rgb(0x19, 0x19, 0x19);

/// One frame's worth of state: the shapes to draw and the textures they need.
struct Frame {
    shapes: Vec<egui::epaint::ClippedShape>,
    /// Flattened out of the `TexturesDelta`, in order. epaint panics if a
    /// `TexturesDelta` is dropped with unapplied entries, and this frame is painted
    /// more than once, so the deltas are extracted and the original cleared right away.
    textures: Vec<(TextureId, egui::epaint::ImageDelta)>,
    ctx: ShapeContext,
}

/// Run a UI closure and capture everything needed to paint it twice.
fn capture(ppp: f32, build: impl FnMut(&mut egui::Ui)) -> Frame {
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(ppp);

    let mut build = build;
    let input = || egui::RawInput {
        screen_rect: Some(Rect::from_min_max(
            pos2(0.0, 0.0),
            pos2(W as f32 / ppp, H as f32 / ppp),
        )),
        ..Default::default()
    };

    // A warm-up pass so fonts and layout are resolved, then the pass we keep. The
    // warm-up's texture delta is merged into ours rather than dropped -- that is where
    // the glyph atlas is first built, and epaint panics on a dropped delta.
    let mut first = ctx.run_ui(input(), &mut build);
    let mut out = ctx.run_ui(input(), &mut build);
    first
        .textures_delta
        .append(std::mem::take(&mut out.textures_delta));
    let mut textures = Vec::new();
    // Iteration order over the delta map does not matter here: each texture id gets its
    // own ordered list, and only the order *within* a list is significant.
    #[expect(clippy::iter_over_hash_type)]
    for (id, deltas) in &first.textures_delta.set {
        for delta in deltas {
            textures.push((*id, delta.clone()));
        }
    }
    first.textures_delta.clear();

    let shape_ctx = ShapeContext {
        pixels_per_point: out.pixels_per_point,
        options: ctx.tessellation_options(|o| *o),
        font_tex_size: ctx.fonts(|f| f.font_image_size()),
        prepared_discs: ctx.fonts(|f| f.fonts.texture_atlas().prepared_discs()),
    };

    Frame {
        shapes: out.shapes,
        textures,
        ctx: shape_ctx,
    }
}

fn render(frame: &Frame, mode: TextMode) -> Vec<u32> {
    let mut painter = Painter::new();
    painter.set_text_mode(mode);
    for (id, delta) in &frame.textures {
        painter.set_texture(*id, delta);
    }
    assert!(
        painter.textures().get(TextureId::Managed(0)).is_some(),
        "the font atlas never arrived"
    );

    let mut buf = vec![0u32; (W * H) as usize];
    let mut target = Target::new(&mut buf, W, H).expect("target");
    Painter::clear(&mut target, BG);
    painter.paint_shapes(&mut target, &frame.ctx, frame.shapes.clone());
    assert_eq!(
        painter.missing_texture_draws(),
        0,
        "a primitive referenced a texture that was never uploaded"
    );
    buf
}

fn assert_identical(frame: &Frame, what: &str) {
    let blit = render(frame, TextMode::Blit);
    let tess = render(frame, TextMode::Tessellate);

    let bg = pack_rgb(BG.r(), BG.g(), BG.b());
    let painted = blit.iter().filter(|&&p| p != bg).count();
    assert!(
        painted > 50,
        "{what}: only {painted} pixels drawn -- the comparison would be vacuous"
    );

    // Largest per-channel deviation, and where it happened.
    let channels = |p: u32| [(p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff];
    let mut worst = (0u32, 0usize);
    let mut differing = 0usize;
    for (i, (a, b)) in blit.iter().zip(&tess).enumerate() {
        if a == b {
            continue;
        }
        differing += 1;
        let (ca, cb) = (channels(*a), channels(*b));
        let delta = (0..3).map(|k| ca[k].abs_diff(cb[k])).max().unwrap_or(0);
        if delta > worst.0 {
            worst = (delta, i);
        }
    }

    let (delta, at) = worst;
    assert!(
        delta <= 1,
        "{what}: a channel differs by {delta} at x={}, y={} ({:06x} blitted vs {:06x} \
         tessellated). More than one LSB means a placement, colour or blend bug, not \
         float noise in UV interpolation.",
        at as u32 % W,
        at as u32 / W,
        blit[at],
        tess[at],
    );
    assert!(
        differing * 200 <= painted,
        "{what}: {differing} of {painted} painted pixels differ (>0.5%). Individually \
         they are within one LSB, but that many means something systematic, not rounding."
    );
}

fn label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(FontId::proportional(13.0)));
}

#[test]
fn blitted_and_tessellated_text_are_bit_identical() {
    let frame = capture(1.0, |ui| {
        label(ui, "Brave Browser (43)");
        label(ui, "18.4%   2410 MB   1.2 MB/s");
        label(ui, "gjpqy AVWX .,;:!? 0123456789");
    });
    assert_identical(&frame, "plain text");
}

/// The 1:1 blit is safe because epaint rounds each glyph origin to a *physical* pixel,
/// so it holds at fractional DPI too. Verify rather than assume -- this is the property
/// most likely to be quietly wrong.
#[test]
fn identical_at_fractional_dpi_scales() {
    for ppp in [1.25f32, 1.5, 1.75, 2.0] {
        let frame = capture(ppp, |ui| {
            label(ui, "Sharp at any scale 0123");
            label(ui, "Second line for good measure");
        });
        assert_identical(&frame, &format!("ppp {ppp}"));
    }
}

/// Selection backgrounds sit *under* the glyphs in the same row mesh, so the blitter has
/// to interleave them in the right order rather than drawing all quads first.
#[test]
fn identical_with_a_background_behind_the_glyphs() {
    let frame = capture(1.0, |ui| {
        let mut job = egui::text::LayoutJob::default();
        job.append(
            "highlighted",
            0.0,
            egui::TextFormat {
                font_id: FontId::proportional(14.0),
                color: Color32::WHITE,
                background: Color32::from_rgb(0x2e, 0x6f, 0xc4),
                ..Default::default()
            },
        );
        job.append(
            " plain",
            0.0,
            egui::TextFormat {
                font_id: FontId::proportional(14.0),
                color: Color32::from_gray(0xd0),
                ..Default::default()
            },
        );
        ui.label(job);
    });
    assert_identical(&frame, "background behind glyphs");
}

/// Strike-through is in the same mesh but *after* the glyph range, so it must end up on
/// top. Underline is a separate stroke the blitter routes back through the tessellator.
#[test]
fn identical_with_underline_and_strikethrough() {
    let frame = capture(1.0, |ui| {
        ui.label(
            egui::RichText::new("underlined")
                .font(FontId::proportional(14.0))
                .underline(),
        );
        ui.label(
            egui::RichText::new("struck through")
                .font(FontId::proportional(14.0))
                .strikethrough(),
        );
    });
    assert_identical(&frame, "underline and strikethrough");
}

/// Italic glyphs are sheared, so they are not 1:1 quads. The blitter must decline them
/// and let the triangle path draw them -- if it drew them upright instead, this fails.
#[test]
fn italics_fall_back_to_the_triangle_path() {
    let frame = capture(1.0, |ui| {
        ui.label(
            egui::RichText::new("italic text is sheared")
                .font(FontId::proportional(14.0))
                .italics(),
        );
    });
    assert_identical(&frame, "italics");
}

/// A weak/faded label exercises `opacity_factor` and `Color32::PLACEHOLDER` resolution.
#[test]
fn identical_with_faded_and_coloured_text() {
    let frame = capture(1.0, |ui| {
        ui.label(egui::RichText::new("dimmed").weak().size(13.0));
        ui.label(
            egui::RichText::new("accent")
                .color(Color32::from_rgb(0x4c, 0xc2, 0xff))
                .size(13.0),
        );
        ui.scope(|ui| {
            ui.set_opacity(0.45);
            ui.label(egui::RichText::new("half transparent").size(13.0));
        });
    });
    assert_identical(&frame, "faded and coloured text");
}

/// Text clipped mid-glyph must be clipped identically in both modes.
#[test]
fn identical_when_clipped_through_a_glyph() {
    let frame = capture(1.0, |ui| {
        // A clip boundary that deliberately falls inside a glyph.
        let clip = Rect::from_min_max(pos2(0.0, 0.0), pos2(73.5, H as f32));
        ui.set_clip_rect(clip);
        label(ui, "Clipped halfway through a glyph");
    });
    assert_identical(&frame, "clipped mid-glyph");

    // And nothing may be painted past the clip, in either mode.
    let bg = pack_rgb(BG.r(), BG.g(), BG.b());
    for mode in [TextMode::Blit, TextMode::Tessellate] {
        let buf = render(&frame, mode);
        for y in 0..H {
            for x in 75..W {
                assert_eq!(
                    buf[(y * W + x) as usize],
                    bg,
                    "{mode:?} painted past the clip at ({x},{y})"
                );
            }
        }
    }
}

/// Text drawn over a filled panel must stay on top: the blitter flushes the queued
/// non-text batch before each text shape, and a bug there would put the panel over the
/// label instead.
#[test]
fn text_stays_on_top_of_shapes_drawn_before_it() {
    let frame = capture(1.0, |ui| {
        let rect = Rect::from_min_size(pos2(4.0, 4.0), egui::vec2(300.0, 40.0));
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::ZERO,
            Color32::from_rgb(0x2b, 0x2b, 0x2b),
        );
        ui.painter().text(
            pos2(10.0, 12.0),
            egui::Align2::LEFT_TOP,
            "on top of the panel",
            FontId::proportional(14.0),
            Color32::WHITE,
        );
        // A second shape after the text, which must land above it.
        ui.painter().rect_filled(
            Rect::from_min_size(pos2(4.0, 60.0), egui::vec2(60.0, 20.0)),
            egui::CornerRadius::ZERO,
            Color32::from_rgb(0x4c, 0xc2, 0xff),
        );
    });
    assert_identical(&frame, "text over a panel");

    // The label must be visible against the panel, not buried by it.
    let buf = render(&frame, TextMode::Blit);
    let panel = pack_rgb(0x2b, 0x2b, 0x2b);
    let bright = (4..44)
        .flat_map(|y| (4..300).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let px = buf[(y * W + x) as usize];
            px != panel && ((px >> 16) & 0xff) > 0x80
        })
        .count();
    assert!(bright > 100, "the label is not visible over the panel");

    // And the blue rect drawn after the text is fully present.
    assert_eq!(buf[(70 * W + 30) as usize], pack_rgb(0x4c, 0xc2, 0xff));
}

/// A stroke drawn *after* text must sit above it -- the batch flush must not reorder.
#[test]
fn shapes_drawn_after_text_stay_above_it() {
    let frame = capture(1.0, |ui| {
        ui.painter().text(
            pos2(10.0, 10.0),
            egui::Align2::LEFT_TOP,
            "covered",
            FontId::proportional(20.0),
            Color32::WHITE,
        );
        ui.painter().rect_filled(
            Rect::from_min_size(pos2(8.0, 8.0), egui::vec2(120.0, 30.0)),
            egui::CornerRadius::ZERO,
            Color32::from_rgb(0xff, 0x00, 0x00),
        );
    });
    assert_identical(&frame, "shape after text");

    let buf = render(&frame, TextMode::Blit);
    // The red rect covers the text completely: every pixel inside it is pure red.
    for y in 9..37u32 {
        for x in 9..127u32 {
            assert_eq!(
                buf[(y * W + x) as usize],
                pack_rgb(0xff, 0, 0),
                "text showed through the rect drawn after it at ({x},{y})"
            );
        }
    }
}

/// Text that is entirely outside the clip must be culled, and identically so.
#[test]
fn identical_when_text_is_fully_culled() {
    let frame = capture(1.0, |ui| {
        ui.set_clip_rect(Rect::from_min_max(pos2(0.0, 0.0), pos2(10.0, 10.0)));
        ui.painter().text(
            pos2(200.0, 100.0),
            egui::Align2::LEFT_TOP,
            "far outside the clip",
            FontId::proportional(14.0),
            Color32::WHITE,
        );
        // Something inside the clip so the frame is not empty.
        ui.painter().rect_filled(
            Rect::from_min_max(pos2(0.0, 0.0), pos2(10.0, 10.0)),
            egui::CornerRadius::ZERO,
            Color32::WHITE,
        );
    });
    let blit = render(&frame, TextMode::Blit);
    let tess = render(&frame, TextMode::Tessellate);
    assert_eq!(blit, tess, "culling differs between text modes");
}

/// An empty string, and a shape with a zero-opacity text, must not panic or draw.
#[test]
fn degenerate_text_is_handled() {
    let frame = capture(1.0, |ui| {
        ui.label("");
        ui.scope(|ui| {
            ui.set_opacity(0.0);
            ui.label("invisible");
        });
        ui.painter().rect_filled(
            Rect::from_min_max(pos2(0.0, 0.0), pos2(20.0, 20.0)),
            egui::CornerRadius::ZERO,
            Color32::WHITE,
        );
    });
    let blit = render(&frame, TextMode::Blit);
    let tess = render(&frame, TextMode::Tessellate);
    assert_eq!(blit, tess);
}

/// The underline stroke is not part of the glyph mesh; confirm it is actually drawn and
/// not silently dropped when the blitter takes over.
#[test]
fn the_underline_is_actually_drawn_by_the_blitter() {
    let frame = capture(1.0, |ui| {
        let mut shape = egui::epaint::TextShape::new(
            pos2(8.0, 8.0),
            ui.painter().layout_no_wrap(
                "underlined".to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
            ),
            Color32::WHITE,
        );
        shape.underline = Stroke::new(1.0, Color32::from_rgb(0xff, 0x00, 0x00));
        ui.painter().add(shape);
    });
    let buf = render(&frame, TextMode::Blit);
    let reddish = buf
        .iter()
        .filter(|&&p| {
            let (r, g, b) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff);
            r > 0x80 && g < 0x40 && b < 0x40
        })
        .count();
    assert!(
        reddish > 20,
        "the underline was not drawn (only {reddish} red pixels)"
    );
}

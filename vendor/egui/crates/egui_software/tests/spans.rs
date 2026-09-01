//! The rectangle fast path must be invisible.
//!
//! `span::fast_rect` skips the tessellator and the triangle rasterizer for plain
//! pixel-aligned rectangles, which is the single biggest cost saving in the renderer --
//! panel backgrounds alone cover every pixel of the window. A fast path is only
//! legitimate if its output is *identical*, so every rectangle it accepts is rendered
//! both ways here and the buffers must match bit for bit.
//!
//! Unlike the glyph blitter, bit-identity is the right bar here rather than one LSB: both
//! paths compute the same constant colour with the same blend, so any difference at all is
//! a bug rather than float noise.

use egui_software::{Painter, ShapeContext, Target, TextMode, pack_rgb};
use epaint::{
    ClippedShape, Color32, ColorImage, CornerRadius, ImageDelta, Rect, RectShape, Shape, Stroke,
    StrokeKind, TessellationOptions, TextureId, pos2, textures::TextureOptions,
};

const W: u32 = 96;
const H: u32 = 72;
const BG: Color32 = Color32::from_rgb(0x19, 0x19, 0x19);

fn ctx(ppp: f32) -> ShapeContext {
    ShapeContext {
        pixels_per_point: ppp,
        options: TessellationOptions::default(),
        font_tex_size: [1, 1],
        prepared_discs: vec![],
    }
}

/// Render shapes through `paint_shapes` (fast paths live) or through the tessellator only.
fn render(shapes: &[Shape], ppp: f32, fast: bool) -> Vec<u32> {
    let atlas = ImageDelta::full(
        ColorImage::new([1, 1], vec![Color32::WHITE]),
        TextureOptions::NEAREST,
    );
    let mut painter = Painter::new();
    painter.set_text_mode(TextMode::Blit);
    painter.set_texture(TextureId::Managed(0), &atlas);

    let clip = Rect::from_min_max(pos2(0.0, 0.0), pos2(W as f32 / ppp, H as f32 / ppp));
    let clipped: Vec<ClippedShape> = shapes
        .iter()
        .cloned()
        .map(|shape| ClippedShape {
            clip_rect: clip,
            shape,
        })
        .collect();

    let mut buf = vec![0u32; (W * H) as usize];
    let mut target = Target::new(&mut buf, W, H).expect("target");
    Painter::clear(&mut target, BG);

    let c = ctx(ppp);
    if fast {
        painter.paint_shapes(&mut target, &c, clipped);
    } else {
        // The reference: straight through epaint's tessellator, no fast paths.
        let primitives = epaint::Tessellator::new(
            c.pixels_per_point,
            c.options,
            c.font_tex_size,
            c.prepared_discs.clone(),
        )
        .tessellate_shapes(clipped);
        painter.paint(&mut target, ppp, &primitives);
    }
    assert_eq!(painter.missing_texture_draws(), 0, "textures resolved");
    buf
}

fn assert_identical(shapes: &[Shape], ppp: f32, what: &str) {
    let fast = render(shapes, ppp, true);
    let slow = render(shapes, ppp, false);

    let bg = pack_rgb(BG.r(), BG.g(), BG.b());
    let painted = fast.iter().filter(|&&p| p != bg).count();
    assert!(painted > 20, "{what}: only {painted} pixels drawn");

    let diff: Vec<usize> = fast
        .iter()
        .zip(&slow)
        .enumerate()
        .filter_map(|(i, (a, b))| (a != b).then_some(i))
        .collect();
    assert!(
        diff.is_empty(),
        "{what}: the fast path differs at {} of {} pixels (first at x={}, y={}: \
         {:06x} fast vs {:06x} tessellated)",
        diff.len(),
        fast.len(),
        diff[0] as u32 % W,
        diff[0] as u32 / W,
        fast[diff[0]],
        slow[diff[0]],
    );
}

fn filled(x0: f32, y0: f32, x1: f32, y1: f32, c: Color32) -> Shape {
    Shape::rect_filled(
        Rect::from_min_max(pos2(x0, y0), pos2(x1, y1)),
        CornerRadius::ZERO,
        c,
    )
}

#[test]
fn an_opaque_rect_is_identical() {
    assert_identical(
        &[filled(
            8.0,
            8.0,
            80.0,
            60.0,
            Color32::from_rgb(0x4c, 0xc2, 0xff),
        )],
        1.0,
        "opaque rect",
    );
}

/// The heat band and the row-hover highlight are translucent, so the blended path has to
/// agree too -- and its rounding is the easiest thing to get subtly wrong.
#[test]
fn a_translucent_rect_is_identical() {
    assert_identical(
        &[
            filled(0.0, 0.0, 96.0, 72.0, Color32::from_rgb(0x2b, 0x2b, 0x2b)),
            filled(
                8.0,
                8.0,
                80.0,
                60.0,
                Color32::from_rgba_premultiplied(0x20, 0x40, 0x60, 0x80),
            ),
        ],
        1.0,
        "translucent rect",
    );
}

/// Every alpha value must round the same way in both paths.
#[test]
fn every_alpha_rounds_identically() {
    for a in [1u8, 7, 32, 64, 100, 127, 128, 200, 254] {
        assert_identical(
            &[
                filled(0.0, 0.0, 96.0, 72.0, Color32::from_rgb(0x40, 0x18, 0x77)),
                filled(
                    4.0,
                    4.0,
                    90.0,
                    68.0,
                    Color32::from_rgba_premultiplied(a / 2, a / 3, a, a),
                ),
            ],
            1.0,
            &format!("alpha {a}"),
        );
    }
}

#[test]
fn fractional_dpi_scales_are_identical() {
    for ppp in [1.25f32, 1.5, 2.0] {
        assert_identical(
            &[filled(
                4.0,
                4.0,
                40.0,
                30.0,
                Color32::from_rgb(0x6f, 0xc2, 0x68),
            )],
            ppp,
            &format!("ppp {ppp}"),
        );
    }
}

/// A row of adjacent rects, as `heat_cells` paints: they must tile with no seam and no
/// double-blend, in both paths.
#[test]
fn adjacent_rects_tile_identically() {
    let mut shapes = vec![filled(
        0.0,
        0.0,
        96.0,
        72.0,
        Color32::from_rgb(0x19, 0x19, 0x19),
    )];
    for i in 0..6 {
        let x = 6.0 + i as f32 * 14.0;
        shapes.push(filled(
            x,
            20.0,
            x + 14.0,
            40.0,
            Color32::from_rgba_premultiplied(0x14 + i * 4, 0x27, 0x40, 0xc0),
        ));
    }
    assert_identical(&shapes, 1.0, "tiled cells");
}

/// Shapes the fast path refuses must still render, and identically -- this is what proves
/// the refusals fall through rather than being dropped.
#[test]
fn refused_shapes_still_render_identically() {
    let r = Rect::from_min_max(pos2(8.0, 8.0), pos2(80.0, 60.0));

    assert_identical(
        &[Shape::rect_filled(r, CornerRadius::same(8), Color32::WHITE)],
        1.0,
        "rounded corners",
    );

    assert_identical(
        &[Shape::Rect(RectShape::new(
            r,
            CornerRadius::ZERO,
            Color32::from_rgb(0x2b, 0x2b, 0x2b),
            Stroke::new(1.0, Color32::from_rgb(0x4c, 0xc2, 0xff)),
            StrokeKind::Inside,
        ))],
        1.0,
        "stroked rect",
    );

    let mut unsnapped = RectShape::filled(r, CornerRadius::ZERO, Color32::WHITE);
    unsnapped.round_to_pixels = Some(false);
    assert_identical(&[Shape::Rect(unsnapped)], 1.0, "unsnapped rect");
}

/// Interleaving fast-path rects with tessellated shapes must not reorder anything: the
/// batch has to be flushed before each fast rect.
#[test]
fn draw_order_survives_the_fast_path() {
    let shapes = vec![
        // A rounded rect (tessellated) under...
        Shape::rect_filled(
            Rect::from_min_max(pos2(8.0, 8.0), pos2(80.0, 60.0)),
            CornerRadius::same(6),
            Color32::from_rgb(0xff, 0x00, 0x00),
        ),
        // ...an opaque fast rect, which must cover it...
        filled(16.0, 16.0, 70.0, 50.0, Color32::from_rgb(0x00, 0xff, 0x00)),
        // ...and another rounded rect on top of that.
        Shape::rect_filled(
            Rect::from_min_max(pos2(24.0, 24.0), pos2(60.0, 44.0)),
            CornerRadius::same(4),
            Color32::from_rgb(0x00, 0x00, 0xff),
        ),
    ];
    assert_identical(&shapes, 1.0, "interleaved order");

    // And confirm the middle layer really did cover the one below it.
    let buf = render(&shapes, 1.0, true);
    assert_eq!(
        buf[(18 * W + 18) as usize],
        pack_rgb(0, 0xff, 0),
        "the fast rect did not cover the shape drawn before it"
    );
}

/// A rect larger than the clip must be clipped identically.
#[test]
fn oversized_rects_clip_identically() {
    let atlas = ImageDelta::full(
        ColorImage::new([1, 1], vec![Color32::WHITE]),
        TextureOptions::NEAREST,
    );
    let inner = Rect::from_min_max(pos2(20.0, 16.0), pos2(60.0, 48.0));
    let shape = filled(-50.0, -50.0, 200.0, 200.0, Color32::WHITE);

    let mut bufs = Vec::new();
    for fast in [true, false] {
        let mut painter = Painter::new();
        painter.set_texture(TextureId::Managed(0), &atlas);
        let mut buf = vec![0u32; (W * H) as usize];
        let mut target = Target::new(&mut buf, W, H).expect("target");
        Painter::clear(&mut target, BG);
        let c = ctx(1.0);
        let clipped = vec![ClippedShape {
            clip_rect: inner,
            shape: shape.clone(),
        }];
        if fast {
            painter.paint_shapes(&mut target, &c, clipped);
        } else {
            let prims =
                epaint::Tessellator::new(1.0, c.options, [1, 1], vec![]).tessellate_shapes(clipped);
            painter.paint(&mut target, 1.0, &prims);
        }
        bufs.push(buf);
    }
    assert_eq!(bufs[0], bufs[1], "clipping differs between the paths");

    let bg = pack_rgb(BG.r(), BG.g(), BG.b());
    for y in 0..H {
        for x in 0..W {
            let inside = (20..60).contains(&x) && (16..48).contains(&y);
            if !inside {
                assert_eq!(
                    bufs[0][(y * W + x) as usize],
                    bg,
                    "painted outside the clip"
                );
            }
        }
    }
}

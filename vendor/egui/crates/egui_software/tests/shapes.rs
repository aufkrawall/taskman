//! End-to-end tests: real `epaint` shapes, tessellated by `epaint::Tessellator`, painted
//! by the software renderer, then checked at the pixel level.
//!
//! The unit tests inside the crate cover the arithmetic in isolation. These cover the
//! thing that actually matters and cannot be checked by reading source: that egui's
//! *anti-aliased* geometry lands on the right pixels.
//!
//! epaint bakes anti-aliasing into the geometry as a 1-px feathered fringe with a vertex
//! alpha ramp, and rounds rects and right-angled lines to the pixel grid
//! (`round_rects_to_pixels` / `round_line_segments_to_pixels`, both default-on). Together
//! that means a pixel-aligned rect must come out with **hard** edges: fully opaque inside,
//! untouched outside, and no half-covered row anywhere. If the rasterizer samples off
//! centre, or applies coverage AA of its own on top of the fringe, these tests fail —
//! and the visible symptom would be a UI that looks uniformly soft next to the GPU one.

use egui_software::{Painter, Target, pack_rgb};
use epaint::{
    ClippedShape, Color32, ColorImage, CornerRadius, ImageDelta, Pos2, Rect, Shape, Stroke,
    StrokeKind, TessellationOptions, Tessellator, TextureId, WHITE_UV, pos2,
    textures::TextureOptions,
};

const W: u32 = 64;
const H: u32 = 64;
const BG: Color32 = Color32::from_rgb(0x19, 0x19, 0x19);

/// Render shapes at `pixels_per_point`, on the standard dark background.
fn render(shapes: Vec<Shape>, ppp: f32) -> Vec<u32> {
    render_sized(shapes, ppp, W, H)
}

fn render_sized(shapes: Vec<Shape>, ppp: f32, w: u32, h: u32) -> Vec<u32> {
    // A 1x1 white atlas: `WHITE_UV` is (0,0), so untextured geometry multiplies by white.
    let atlas = ImageDelta::full(
        ColorImage::new([1, 1], vec![Color32::WHITE]),
        TextureOptions::NEAREST,
    );

    let clip = Rect::from_min_max(pos2(0.0, 0.0), pos2(w as f32 / ppp, h as f32 / ppp));
    let clipped: Vec<ClippedShape> = shapes
        .into_iter()
        .map(|shape| ClippedShape {
            clip_rect: clip,
            shape,
        })
        .collect();

    let mut tess = Tessellator::new(ppp, TessellationOptions::default(), [1, 1], vec![]);
    let primitives = tess.tessellate_shapes(clipped);

    let mut painter = Painter::new();
    painter.set_texture(TextureId::Managed(0), &atlas);

    let mut buf = vec![0u32; (w * h) as usize];
    let mut target = Target::new(&mut buf, w, h).expect("target");
    Painter::clear(&mut target, BG);
    painter.paint(&mut target, ppp, &primitives);
    assert_eq!(painter.missing_texture_draws(), 0, "all textures resolved");
    buf
}

fn at(buf: &[u32], w: u32, x: u32, y: u32) -> u32 {
    buf[(y * w + x) as usize]
}

/// Distinct colour values in a row, for spotting unwanted intermediate (anti-aliased)
/// values along what should be a hard edge.
fn distinct_in_column(buf: &[u32], w: u32, h: u32, x: u32) -> Vec<u32> {
    let mut v: Vec<u32> = (0..h).map(|y| at(buf, w, x, y)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// A pixel-aligned opaque rect must have hard edges and an exact interior.
///
/// This is the single most common thing taskman draws -- every table row, heat cell,
/// panel and chart background -- so if it is soft or off by a pixel, everything is.
#[test]
fn a_pixel_aligned_rect_has_hard_edges_and_an_exact_interior() {
    let fill = Color32::from_rgb(0x4c, 0xc2, 0xff);
    let buf = render(
        vec![Shape::rect_filled(
            Rect::from_min_max(pos2(16.0, 16.0), pos2(48.0, 48.0)),
            CornerRadius::ZERO,
            fill,
        )],
        1.0,
    );

    let want = pack_rgb(fill.r(), fill.g(), fill.b());
    let bg = pack_rgb(BG.r(), BG.g(), BG.b());

    assert_eq!(at(&buf, W, 16, 16), want, "first covered pixel");
    assert_eq!(at(&buf, W, 47, 47), want, "last covered pixel");
    assert_eq!(
        at(&buf, W, 15, 32),
        bg,
        "column left of the rect is untouched"
    );
    assert_eq!(at(&buf, W, 48, 32), bg, "max is exclusive");

    // Down a column through the rect there must be exactly two values: background and
    // fill. A third value would be a feathered row, i.e. a soft edge.
    let seen = distinct_in_column(&buf, W, H, 32);
    assert_eq!(
        seen.len(),
        2,
        "expected only background and fill down the rect, got {seen:02x?} -- a third \
         value means the edge is anti-aliased when it should be crisp"
    );
}

/// The same rect at a fractional zoom must still be crisp: epaint rounds to *physical*
/// pixels, so `pixels_per_point` 1.5 keeps hard edges rather than smearing them.
#[test]
fn pixel_snapping_survives_fractional_scaling() {
    let fill = Color32::WHITE;
    let buf = render(
        vec![Shape::rect_filled(
            Rect::from_min_max(pos2(8.0, 8.0), pos2(24.0, 24.0)),
            CornerRadius::ZERO,
            fill,
        )],
        1.5,
    );
    let seen = distinct_in_column(&buf, W, H, 24);
    assert_eq!(
        seen.len(),
        2,
        "fractional ppp produced a soft edge: {seen:02x?}"
    );
}

/// A rounded rect must be crisp along its straight edges and anti-aliased at its corners.
/// Losing the corner AA would make every dialog and menu look jagged; losing the straight
/// edge crispness would make them look blurry.
#[test]
fn a_rounded_rect_is_crisp_on_the_sides_and_smooth_in_the_corners() {
    let fill = Color32::WHITE;
    let buf = render(
        vec![Shape::rect_filled(
            Rect::from_min_max(pos2(16.0, 16.0), pos2(48.0, 48.0)),
            CornerRadius::same(8),
            fill,
        )],
        1.0,
    );

    // Mid-height, well away from the corners: hard edge.
    let seen = distinct_in_column(&buf, W, H, 32);
    assert_eq!(seen.len(), 2, "straight edge is not crisp: {seen:02x?}");

    // The corner region must contain intermediate values -- that is the anti-aliasing.
    let mut corner_shades = std::collections::BTreeSet::new();
    for y in 16..24 {
        for x in 16..24 {
            corner_shades.insert(at(&buf, W, x, y));
        }
    }
    assert!(
        corner_shades.len() > 4,
        "corner has only {} distinct values -- the arc is not anti-aliased",
        corner_shades.len()
    );
}

/// A 1-px horizontal line, the chart gridline case. epaint snaps right-angled line
/// segments to the pixel grid, so this must land on exactly one fully-covered row rather
/// than bleeding half-intensity across two.
#[test]
fn a_one_pixel_horizontal_line_lands_on_a_single_row() {
    let color = Color32::WHITE;
    let buf = render(
        vec![Shape::line_segment(
            [pos2(8.0, 32.0), pos2(56.0, 32.0)],
            Stroke::new(1.0, color),
        )],
        1.0,
    );

    let x = 32;
    let touched: Vec<u32> = (0..H)
        .filter(|&y| at(&buf, W, x, y) != pack_rgb(BG.r(), BG.g(), BG.b()))
        .collect();
    assert_eq!(
        touched.len(),
        1,
        "a 1px line touched {} rows ({touched:?}); it should occupy exactly one",
        touched.len()
    );
    assert_eq!(
        at(&buf, W, x, touched[0]),
        pack_rgb(255, 255, 255),
        "the single row is not fully covered"
    );
}

/// A sub-pixel-width line is *deliberately* drawn faint and spread, not snapped to a full
/// pixel: epaint scales the colour by `width / feathering`. taskman's 0.75-wide chart
/// gridlines and column separators rely on this, and it is what keeps the chrome lighter
/// than a 1px rule would be. Locked in so nobody "fixes" it into a hard line.
#[test]
fn a_sub_pixel_line_stays_faint_rather_than_snapping_to_full_intensity() {
    let buf = render(
        vec![Shape::line_segment(
            [pos2(8.0, 32.0), pos2(56.0, 32.0)],
            Stroke::new(0.75, Color32::WHITE),
        )],
        1.0,
    );
    let bg = pack_rgb(BG.r(), BG.g(), BG.b());
    let touched: Vec<u32> = (0..H)
        .map(|y| at(&buf, W, 32, y))
        .filter(|&px| px != bg)
        .collect();
    assert!(!touched.is_empty(), "the line vanished entirely");
    assert!(
        touched.iter().all(|&px| px != pack_rgb(255, 255, 255)),
        "a 0.75px line reached full intensity; the width-to-opacity scaling was lost"
    );
}

/// Filled circles come from the tessellator's pre-rasterized discs or from a polygon
/// path; either way the interior must be solid with no unpainted pixels.
#[test]
fn a_filled_circle_has_no_holes() {
    let buf = render(
        vec![Shape::circle_filled(pos2(32.0, 32.0), 12.0, Color32::WHITE)],
        1.0,
    );
    let bg = pack_rgb(BG.r(), BG.g(), BG.b());
    for y in 26..38 {
        for x in 26..38 {
            assert_ne!(at(&buf, W, x, y), bg, "hole in the disc at ({x},{y})");
        }
    }
}

/// A stroked rect drawn `Inside` must stay within its bounds -- egui's table and chart
/// borders depend on it, and a renderer that rounded outward would overdraw neighbours.
#[test]
fn an_inside_stroke_stays_within_the_rect() {
    let buf = render(
        vec![Shape::rect_stroke(
            Rect::from_min_max(pos2(16.0, 16.0), pos2(48.0, 48.0)),
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::WHITE),
            StrokeKind::Inside,
        )],
        1.0,
    );
    let bg = pack_rgb(BG.r(), BG.g(), BG.b());
    for y in 0..H {
        for x in 0..W {
            let inside = (16..48).contains(&x) && (16..48).contains(&y);
            if !inside {
                assert_eq!(
                    at(&buf, W, x, y),
                    bg,
                    "stroke escaped the rect at ({x},{y})"
                );
            }
        }
    }
}

/// Overlapping translucent fills must compose in draw order, the way the chart series
/// fills do. Painting A then B must differ from B then A when they have different colours,
/// and both must be a blend rather than either input.
#[test]
fn translucent_fills_compose_in_draw_order() {
    let a = Color32::from_rgba_premultiplied(0x40, 0x00, 0x00, 0x80);
    let b = Color32::from_rgba_premultiplied(0x00, 0x00, 0x40, 0x80);
    let rect = Rect::from_min_max(pos2(16.0, 16.0), pos2(48.0, 48.0));
    let ab = render(
        vec![
            Shape::rect_filled(rect, CornerRadius::ZERO, a),
            Shape::rect_filled(rect, CornerRadius::ZERO, b),
        ],
        1.0,
    );
    let ba = render(
        vec![
            Shape::rect_filled(rect, CornerRadius::ZERO, b),
            Shape::rect_filled(rect, CornerRadius::ZERO, a),
        ],
        1.0,
    );
    assert_ne!(
        at(&ab, W, 32, 32),
        at(&ba, W, 32, 32),
        "draw order was not respected"
    );
}

/// Everything outside the clip rect must be untouched, even when the shape is far larger.
#[test]
fn clipping_is_exact() {
    let atlas = ImageDelta::full(
        ColorImage::new([1, 1], vec![Color32::WHITE]),
        TextureOptions::NEAREST,
    );
    let mut tess = Tessellator::new(1.0, TessellationOptions::default(), [1, 1], vec![]);
    let primitives = tess.tessellate_shapes(vec![ClippedShape {
        clip_rect: Rect::from_min_max(pos2(16.0, 16.0), pos2(48.0, 48.0)),
        shape: Shape::rect_filled(
            Rect::from_min_max(pos2(0.0, 0.0), pos2(64.0, 64.0)),
            CornerRadius::ZERO,
            Color32::WHITE,
        ),
    }]);

    let mut painter = Painter::new();
    painter.set_texture(TextureId::Managed(0), &atlas);
    let mut buf = vec![0u32; (W * H) as usize];
    let mut target = Target::new(&mut buf, W, H).expect("target");
    Painter::clear(&mut target, BG);
    painter.paint(&mut target, 1.0, &primitives);

    let bg = pack_rgb(BG.r(), BG.g(), BG.b());
    for y in 0..H {
        for x in 0..W {
            let inside = (16..48).contains(&x) && (16..48).contains(&y);
            let px = at(&buf, W, x, y);
            if inside {
                assert_eq!(
                    px,
                    pack_rgb(255, 255, 255),
                    "unpainted inside clip ({x},{y})"
                );
            } else {
                assert_eq!(px, bg, "painted outside clip at ({x},{y})");
            }
        }
    }
}

/// A textured quad must sample the atlas rather than the white texel. This is the path
/// every glyph and every process icon takes.
#[test]
fn a_textured_quad_samples_the_atlas() {
    // 2x2 atlas: red, green / blue, white.
    let atlas = ImageDelta::full(
        ColorImage::new(
            [2, 2],
            vec![Color32::RED, Color32::GREEN, Color32::BLUE, Color32::WHITE],
        ),
        TextureOptions::NEAREST,
    );

    let mut mesh = epaint::Mesh::with_texture(TextureId::Managed(0));
    // Map the whole atlas across a 32x32 quad.
    for (pos, uv) in [
        (pos2(16.0, 16.0), pos2(0.0, 0.0)),
        (pos2(48.0, 16.0), pos2(1.0, 0.0)),
        (pos2(48.0, 48.0), pos2(1.0, 1.0)),
        (pos2(16.0, 48.0), pos2(0.0, 1.0)),
    ] {
        mesh.vertices.push(epaint::Vertex {
            pos,
            uv,
            color: Color32::WHITE,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);

    let mut painter = Painter::new();
    painter.set_texture(TextureId::Managed(0), &atlas);
    let mut buf = vec![0u32; (W * H) as usize];
    let mut target = Target::new(&mut buf, W, H).expect("target");
    Painter::clear(&mut target, BG);
    painter.paint(
        &mut target,
        1.0,
        &[epaint::ClippedPrimitive {
            clip_rect: Rect::from_min_max(pos2(0.0, 0.0), pos2(64.0, 64.0)),
            primitive: epaint::Primitive::Mesh(mesh),
        }],
    );

    assert_eq!(at(&buf, W, 20, 20), pack_rgb(255, 0, 0), "top-left texel");
    assert_eq!(at(&buf, W, 44, 20), pack_rgb(0, 255, 0), "top-right texel");
    assert_eq!(
        at(&buf, W, 20, 44),
        pack_rgb(0, 0, 255),
        "bottom-left texel"
    );
    assert_eq!(at(&buf, W, 44, 44), pack_rgb(255, 255, 255), "bottom-right");
    // `WHITE_UV` must still be the white shortcut, not a texel fetch.
    assert_eq!(WHITE_UV, pos2(0.0, 0.0));
}

/// The chart area fill: `taskman`'s `area_strip_mesh` builds a triangle strip between a
/// polyline and a baseline. Rendered with a translucent fill, any double-blend along the
/// shared vertical edges shows up as a bright seam and any crack as a dark one. Every
/// covered column must therefore carry exactly the same value.
#[test]
fn a_chart_area_strip_has_no_seams() {
    let fill = Color32::from_rgba_premultiplied(0x18, 0x40, 0x55, 0x55);
    let mut mesh = epaint::Mesh::default();
    let baseline = 56.0;
    // A sawtooth, so consecutive segments have very different slopes.
    let pts: Vec<Pos2> = (0..=16)
        .map(|i| {
            let x = 8.0 + i as f32 * 3.0;
            let y = if i % 2 == 0 { 16.0 } else { 40.0 };
            pos2(x, y)
        })
        .collect();
    for p in &pts {
        let top = mesh.vertices.len() as u32;
        mesh.colored_vertex(*p, fill);
        mesh.colored_vertex(pos2(p.x, baseline), fill);
        if top >= 2 {
            mesh.add_triangle(top - 2, top, top - 1);
            mesh.add_triangle(top - 1, top, top + 1);
        }
    }

    let mut painter = Painter::new();
    painter.set_texture(
        TextureId::Managed(0),
        &ImageDelta::full(
            ColorImage::new([1, 1], vec![Color32::WHITE]),
            TextureOptions::NEAREST,
        ),
    );
    let mut buf = vec![0u32; (W * H) as usize];
    let mut target = Target::new(&mut buf, W, H).expect("target");
    Painter::clear(&mut target, BG);
    painter.paint(
        &mut target,
        1.0,
        &[epaint::ClippedPrimitive {
            clip_rect: Rect::from_min_max(pos2(0.0, 0.0), pos2(64.0, 64.0)),
            primitive: epaint::Primitive::Mesh(mesh),
        }],
    );

    // Row 50 is below the sawtooth's lowest point and above the baseline, so every
    // column from 9..55 is covered by exactly one layer of fill.
    let bg = pack_rgb(BG.r(), BG.g(), BG.b());
    let expected = at(&buf, W, 30, 50);
    assert_ne!(expected, bg, "the strip did not paint at all");
    for x in 9..55 {
        assert_eq!(
            at(&buf, W, x, 50),
            expected,
            "seam at column {x}: the shared edge was covered twice or not at all"
        );
    }
}

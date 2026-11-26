//! Incremental painting into a buffer that is older than the last frame.
//!
//! Wayland's `wl_shm` swapchain alternates two buffers (three while the compositor
//! holds one), so the buffer `softbuffer` hands back holds the frame from two or three
//! presents ago. Painting only this frame's changed tiles into it leaves the previous
//! frame's changes missing -- the black boxes and stale regions Linux once showed --
//! while repainting everything made every frame a full-window repaint. The painter
//! repaints the tiles every frame since that buffer's frame changed; here a simulated
//! ring of 2 and 3 buffers is driven through a sequence of frames, and every presented
//! buffer must be bit-identical to painting that frame from scratch.

use egui_software::{Painter, ShapeContext, Target, TextMode};
use epaint::{
    ClippedShape, Color32, ColorImage, CornerRadius, ImageDelta, Rect, RectShape, Shape,
    TessellationOptions, TextureId, pos2, textures::TextureOptions, vec2,
};

const W: u32 = 320;
const H: u32 = 200;
const BG: Color32 = Color32::from_rgb(0x19, 0x19, 0x19);

fn ctx() -> ShapeContext {
    ShapeContext {
        pixels_per_point: 1.0,
        options: TessellationOptions::default(),
        font_tex_size: [1, 1],
        prepared_discs: vec![],
    }
}

fn painter() -> Painter {
    let atlas = ImageDelta::full(
        ColorImage::new([1, 1], vec![Color32::WHITE]),
        TextureOptions::NEAREST,
    );
    let mut painter = Painter::new();
    painter.set_text_mode(TextMode::Blit);
    painter.set_texture(TextureId::Managed(0), &atlas);
    painter
}

/// Frame `k`: a static panel, a box that moves across tiles, a rounded translucent
/// highlight that appears on some frames, and frames that repeat their predecessor.
fn frame(k: usize) -> Vec<ClippedShape> {
    // Frames 3-4 and 8-9 repeat frames 2 and 7. In a swapchain the first repeat
    // still brings the older buffer up to date; the second has nothing left to paint.
    let k = match k {
        3 | 4 => 2,
        8 | 9 => 7,
        k => k,
    };
    let clip = Rect::from_min_max(pos2(0.0, 0.0), pos2(W as f32, H as f32));
    let mut shapes = vec![Shape::Rect(RectShape::filled(
        Rect::from_min_size(pos2(0.0, 0.0), vec2(80.0, H as f32)),
        CornerRadius::ZERO,
        Color32::from_rgb(40, 44, 52),
    ))];
    let x = 20.0 + 37.0 * k as f32;
    shapes.push(Shape::Rect(RectShape::filled(
        Rect::from_min_size(pos2(x % 280.0, 30.0 + 11.0 * k as f32), vec2(30.0, 24.0)),
        CornerRadius::ZERO,
        Color32::from_rgb(200, 60, 40),
    )));
    if k % 3 != 1 {
        shapes.push(Shape::Rect(RectShape::filled(
            Rect::from_min_size(pos2(150.0 - 9.0 * k as f32, 120.0), vec2(90.0, 40.5)),
            CornerRadius::same(6),
            Color32::from_rgba_unmultiplied(90, 140, 255, 120),
        )));
    }
    shapes
        .into_iter()
        .map(|shape| ClippedShape {
            clip_rect: clip,
            shape,
        })
        .collect()
}

/// Frame `k` painted from scratch: the reference every presented buffer must equal.
fn reference(k: usize) -> Vec<u32> {
    let mut buf = vec![0u32; (W * H) as usize];
    let mut target = Target::new(&mut buf, W, H).expect("target");
    painter().paint_shapes_damaged(&mut target, &ctx(), frame(k), BG, true);
    buf
}

/// Drive `frames` frames through a ring of `count` buffers that ages them the way
/// `softbuffer` does: the presented buffer becomes age 1, every other painted buffer
/// ages by one, a never-painted buffer stays at 0. Returns the tiles painted per
/// presented frame, with the age of the buffer each was painted into.
fn run_ring(count: usize, frames: usize, present_unchanged: bool) -> Vec<(u8, usize, usize)> {
    let mut painter = painter();
    let mut buffers = vec![vec![0u32; (W * H) as usize]; count];
    let mut ages = vec![0u8; count];
    let mut back = 0usize;
    let mut painted = Vec::new();
    for k in 0..frames {
        let age = ages[back];
        let mut target = Target::new(&mut buffers[back], W, H).expect("target");
        let stats =
            painter.paint_shapes_damaged_aged(&mut target, &ctx(), frame(k), BG, k == 0, age);
        if stats.dirty_rects.is_empty() {
            assert!(
                matches!(k, 3 | 4 | 8 | 9),
                "frame {k} changed but painted nothing into a buffer of age {age}"
            );
            // eframe commits an unchanged frame on Wayland with a token damage rect, so
            // redraws stay paced; elsewhere (or while hidden) it skips the present and
            // takes the frame off the record. Either way the buffer already holds it.
            if !present_unchanged {
                painter.forget_unpresented_frame();
                continue;
            }
        }
        assert_eq!(
            buffers[back],
            reference(k),
            "frame {k}: the age-{age} buffer differs from a full repaint"
        );
        painted.push((age, stats.dirty_tiles, stats.total_tiles));
        for (i, a) in ages.iter_mut().enumerate() {
            if i == back {
                *a = 1;
            } else if *a != 0 {
                *a = a.saturating_add(1);
            }
        }
        back = (back + 1) % count;
    }
    painted
}

#[test]
fn a_two_buffer_ring_presents_exact_frames_without_repainting_everything() {
    let painted = run_ring(2, 12, true);
    // Once both buffers hold a frame, each one is age 2: it is painted incrementally,
    // and never as the whole window -- the full repaint every Wayland frame used to be.
    // (The first of them still lacks the previous frame's full paint of the other buffer.)
    let incremental: Vec<_> = painted
        .iter()
        .filter(|(age, ..)| *age == 2)
        .skip(1)
        .collect();
    assert!(incremental.len() >= 6, "{painted:?}");
    for (age, dirty, total) in incremental {
        assert!(
            dirty < total,
            "an age-{age} buffer was repainted in full: {painted:?}"
        );
    }
}

#[test]
fn a_three_buffer_ring_presents_exact_frames() {
    run_ring(3, 12, true);
}

/// An unchanged frame that is not presented ages no buffer; taking it off the record
/// keeps the next frames exact.
#[test]
fn skipping_the_present_of_an_unchanged_frame_stays_exact() {
    run_ring(2, 12, false);
    run_ring(3, 12, false);
}

/// A buffer older than the record reaches is repainted in full, not guessed at.
#[test]
fn a_buffer_older_than_the_record_is_painted_in_full() {
    let mut painter = painter();
    let mut buf = vec![0u32; (W * H) as usize];
    let mut target = Target::new(&mut buf, W, H).expect("target");
    painter.paint_shapes_damaged_aged(&mut target, &ctx(), frame(0), BG, true, 1);
    let stats = painter.paint_shapes_damaged_aged(
        &mut target,
        &ctx(),
        frame(1),
        BG,
        false,
        egui_software::MAX_BUFFER_AGE + 1,
    );
    assert_eq!(stats.dirty_tiles, stats.total_tiles);
    // Age 0 is undefined contents, whatever the record says.
    let stats = painter.paint_shapes_damaged_aged(&mut target, &ctx(), frame(2), BG, false, 0);
    assert_eq!(stats.dirty_tiles, stats.total_tiles);
}

//! Headless CPU rendering of taskman's real widgets.
//!
//! Drives the actual `tablekit`, `chart` and `icons` code through a windowless
//! [`egui::Context`], tessellates the result, and paints it with [`egui_software`] --
//! the same painter the `Software` renderer will use. No window is opened and no GPU is
//! touched, so this runs in CI and in a normal `cargo test`.
//!
//! It serves three purposes:
//!
//! * **A rasterizer regression gate.**
//!   [`taskman_widgets_render_without_gaps_or_panics`] asserts structural properties of
//!   the output (the chrome painted, the heat band is a continuous run of blues, the
//!   charts drew, nothing is left at the clear colour where content belongs). These catch
//!   a rasterizer regression without pinning down every pixel, which would make the test
//!   fail on any innocuous layout tweak.
//! * **A chart-geometry regression gate,**
//!   [`a_core_tile_breaks_its_series_at_missing_samples`] and
//!   [`an_out_of_window_stamp_breaks_the_line_instead_of_bridging_it`]. These drive the
//!   real per-core tile and multi-series widgets with adversarial telemetry -- a NaN,
//!   both infinities, a stamp that steps backwards, a stamp from outside the axis -- and
//!   assert on the TESSELLATED geometry: every vertex finite, nothing painted further
//!   outside the tile than the documented lead-in overhang, and no stroke edge longer
//!   than one sample interval. They reproduce the artifact users reported on the
//!   Performance tab's per-logical-processor grid: thin straight strokes in the series
//!   colour that belong to no data run, bridge the regions where the tile paints no area
//!   fill, and run diagonally over thirty sample intervals -- measured off a screenshot,
//!   one stroke went from 45% of the card's width at full height down to 95% at 10% with
//!   no fill anywhere under it. epaint has no gap support, so a `Shape::line` through a
//!   non-finite sample is not culled: it reaches the rasterizer as garbage strokes and as
//!   an area fill that paints nothing. Where a pixel assertion is used at all it stays
//!   structural (the series colour must not leave the tile that owns it, and both tiles
//!   must actually paint their series), never a pinned count or a pinned y for a value.
//! * **A deliverable.** Set `TASKMAN_RENDER_SNAPSHOT=<path>` to write the frame out as a
//!   PNG and actually look at it:
//!
//!   ```text
//!   TASKMAN_RENDER_SNAPSHOT=target/cpu-frame.png cargo test -p tm-app render_snapshot
//!   ```
//!
//! Fonts are epaint's bundled defaults rather than Segoe UI, deliberately: the output is
//! then identical on every machine, so the structural assertions mean the same thing in
//! CI as they do locally.

use eframe::egui::{self, FontId, Pos2, Rect, pos2, vec2};
use egui_software::{Painter, ShapeContext, Target, pack_rgb};

use crate::theme::{self, Palette};
use crate::widgets::chart::{self, MultiSeries};
use crate::widgets::tablekit::{HeatCell, TmColumn, TmTable};

const WIDTH: u32 = 900;
const HEIGHT: u32 = 560;

/// A plausible Processes-tab column set, matching what the real tab registers.
fn columns() -> Vec<TmColumn> {
    vec![
        TmColumn::text("name", "Name", 300.0),
        TmColumn::text("status", "Status", 90.0),
        TmColumn::num("cpu", "CPU", 90.0),
        TmColumn::num("mem", "Memory", 110.0),
        TmColumn::num("disk", "Disk", 100.0),
        TmColumn::num("net", "Network", 110.0),
    ]
}

/// One row's worth of made-up but realistically-shaped process data.
struct Row {
    name: &'static str,
    status: &'static str,
    cpu: f64,
    mem: f64,
    disk: f64,
    net: f64,
}

const ROWS: &[Row] = &[
    Row {
        name: "Brave Browser (43)",
        status: "",
        cpu: 18.4,
        mem: 2410.0,
        disk: 1.2,
        net: 480.0,
    },
    Row {
        name: "Code.exe",
        status: "",
        cpu: 9.1,
        mem: 1180.0,
        disk: 0.4,
        net: 12.0,
    },
    Row {
        name: "Explorer.exe",
        status: "",
        cpu: 1.3,
        mem: 190.0,
        disk: 0.1,
        net: 0.0,
    },
    Row {
        name: "taskman.exe",
        status: "",
        cpu: 0.6,
        mem: 62.0,
        disk: 0.0,
        net: 0.0,
    },
    Row {
        name: "SearchIndexer.exe",
        status: "Suspended",
        cpu: 0.0,
        mem: 88.0,
        disk: 3.4,
        net: 0.0,
    },
    Row {
        name: "Steam.exe",
        status: "",
        cpu: 2.7,
        mem: 540.0,
        disk: 0.2,
        net: 96.0,
    },
    Row {
        name: "System",
        status: "",
        cpu: 0.9,
        mem: 24.0,
        disk: 0.8,
        net: 0.0,
    },
    Row {
        name: "dwm.exe",
        status: "",
        cpu: 3.2,
        mem: 210.0,
        disk: 0.0,
        net: 0.0,
    },
];

/// Deterministic pseudo-random-ish sample series, so the charts have real shape without
/// pulling in an RNG.
fn series(n: usize, seed: u64, scale: f64) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let t = i as f64 * 0.37 + seed as f64;
            let v = (t.sin() * 0.5 + (t * 0.31).cos() * 0.3 + 0.55).clamp(0.02, 1.0);
            v * scale
        })
        .collect()
}

/// Lay out a representative taskman frame: sidebar, table, and a chart grid.
fn build_frame(ctx: &egui::Context) -> egui::FullOutput {
    build_frame_with_input(ctx, WIDTH, HEIGHT, Vec::new(), 0.0)
}

fn build_frame_with_input(
    ctx: &egui::Context,
    width: u32,
    height: u32,
    events: Vec<egui::Event>,
    time: f64,
) -> egui::FullOutput {
    let pal: Palette = theme::DARK;
    let mut table = TmTable::new("snapshot", columns(), None);

    ctx.run_ui(
        egui::RawInput {
            time: Some(time),
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                vec2(width as f32, height as f32),
            )),
            events,
            ..Default::default()
        },
        |root| {
            egui::Panel::left(egui::Id::new("sidebar"))
                .resizable(false)
                .min_size(48.0)
                .max_size(48.0)
                .frame(egui::Frame::NONE.fill(pal.sidebar_bg))
                .show(root, |ui| {
                    ui.add_space(8.0);
                    for icon in [
                        crate::icons::Icon::Processes,
                        crate::icons::Icon::Performance,
                        crate::icons::Icon::History,
                        crate::icons::Icon::Startup,
                        crate::icons::Icon::Users,
                        crate::icons::Icon::Details,
                        crate::icons::Icon::Services,
                    ] {
                        let (rect, _) =
                            ui.allocate_exact_size(vec2(32.0, 32.0), egui::Sense::hover());
                        crate::icons::draw(ui, icon, rect, pal.text_dim);
                        ui.add_space(6.0);
                    }
                });

            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(pal.window_bg))
                .show(root, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add_space(6.0);

                    // --- the chart row: a multi-series graph and four core tiles ---
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        chart::chart_multi(
                            ui,
                            vec2(420.0, 120.0),
                            &[
                                MultiSeries::new("CPU", series(120, 3, 90.0), pal.cpu_graph),
                                MultiSeries::new("Memory", series(120, 11, 55.0), pal.memory_graph),
                            ],
                            100.0,
                            None,
                            chart::fmt_percent,
                        );
                        ui.add_space(12.0);
                        egui::Grid::new("cores")
                            .spacing(vec2(4.0, 4.0))
                            .show(ui, |ui| {
                                for i in 0..8 {
                                    let s = series(60, i * 7 + 1, 95.0);
                                    let k = series(60, i * 7 + 4, 35.0);
                                    chart::core_chart(
                                        ui,
                                        vec2(88.0, 56.0),
                                        &s,
                                        Some((&k, pal.cpu_kernel_graph)),
                                        pal.cpu_graph,
                                        &format!("CPU {i}"),
                                        None,
                                    );
                                    if i % 4 == 3 {
                                        ui.end_row();
                                    }
                                }
                            });
                    });

                    ui.add_space(10.0);

                    // --- the table: header, then rows with the blue heat band ---
                    let max =
                        |f: fn(&Row) -> f64| ROWS.iter().map(f).fold(0.0f64, f64::max).max(1.0);
                    let (mcpu, mmem, mdisk, mnet) = (
                        max(|r| r.cpu),
                        max(|r| r.mem),
                        max(|r| r.disk),
                        max(|r| r.net),
                    );

                    table.header(ui, &pal, Some((2, false)), None);
                    for (i, r) in ROWS.iter().enumerate() {
                        let (rect, _) = table.row(ui, &pal, i == 1, i);
                        table.text_cell(ui, rect, 0, r.name, &pal, false);
                        table.text_cell(ui, rect, 1, r.status, &pal, true);
                        table.heat_cells(
                            ui,
                            &pal,
                            rect,
                            2,
                            &[
                                HeatCell::new(norm(r.cpu, mcpu), format!("{:.1}%", r.cpu)),
                                HeatCell::new(norm(r.mem, mmem), format!("{:.0} MB", r.mem)),
                                HeatCell::new(norm(r.disk, mdisk), format!("{:.1} MB/s", r.disk)),
                                HeatCell::new(norm(r.net, mnet), format!("{:.0} KB/s", r.net)),
                            ],
                        );
                    }

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        let (rect, _) =
                            ui.allocate_exact_size(vec2(62.0, 40.0), egui::Sense::hover());
                        chart::paint_sparkline(
                            ui,
                            rect,
                            &series(60, 21, 80.0),
                            pal.disk_graph,
                            pal.card_bg,
                        );
                        ui.add_space(10.0);
                        ui.painter().text(
                            pos2(ui.cursor().left(), rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            "Rendered entirely on the CPU \u{2014} no GPU, no driver.",
                            FontId::proportional(13.0),
                            pal.text,
                        );
                    });
                });
        },
    )
}

fn norm(v: f64, max: f64) -> f32 {
    crate::widgets::tablekit::norm(v, max)
}

/// Render one frame with the software painter and return the pixel buffer.
fn render() -> Vec<u32> {
    render_with(egui::epaint::text::SubpixelMode::Off)
}

/// Render one frame, optionally with sub-pixel (`ClearType`) glyph coverage.
///
/// The mode has to be set in two places that must agree: `TextOptions` decides how the
/// glyph atlas is *rasterized*, and the painter decides how those texels are *blended*.
/// Setting one without the other is the failure mode worth guarding against -- it does
/// not error, it just draws wrong colours.
fn render_with(subpixel: egui::epaint::text::SubpixelMode) -> Vec<u32> {
    let ctx = egui::Context::default();
    theme::install_visuals(&ctx);
    ctx.set_theme(egui::ThemePreference::Dark);
    if !subpixel.is_off() {
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            ctx.style_mut_of(theme, |style| {
                style.visuals.text_options.subpixel = subpixel;
            });
        }
    }

    let mut painter = Painter::new();
    if !subpixel.is_off() {
        // Windows' defaults. The real values come from the platform at run time; here
        // they only have to be representative.
        painter.set_subpixel(subpixel, 1.8, 0.5);
    }

    // Two passes: egui resolves sizes and hover state from the previous frame, so the
    // first pass is a layout warm-up and the second is the one worth painting. Both
    // frames' texture deltas must still be applied, in order -- the warm-up is where the
    // glyph atlas is first built, and epaint panics if a `TexturesDelta` is dropped
    // unapplied. A real backend does exactly this every frame.
    let apply = |painter: &mut Painter, output: &mut egui::FullOutput| {
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                painter.set_texture(*id, delta);
            }
        }
        for id in &output.textures_delta.free {
            painter.free_texture(*id);
        }
        output.textures_delta.clear();
    };

    let mut warmup = build_frame(&ctx);
    apply(&mut painter, &mut warmup);
    let mut output = build_frame(&ctx);
    apply(&mut painter, &mut output);

    // Paint from untessellated shapes so the glyph blitter is exercised -- that is the
    // path the Software renderer uses, and the one sub-pixel text will hang off.
    let shape_ctx = ShapeContext {
        pixels_per_point: output.pixels_per_point,
        options: ctx.tessellation_options(|o| *o),
        font_tex_size: ctx.fonts(|f| f.font_image_size()),
        prepared_discs: ctx.fonts(|f| f.fonts.texture_atlas().prepared_discs()),
    };

    let mut buf = vec![0u32; (WIDTH * HEIGHT) as usize];
    let mut target = Target::new(&mut buf, WIDTH, HEIGHT).expect("target fits");
    Painter::clear(&mut target, theme::DARK.window_bg);
    painter.paint_shapes(&mut target, &shape_ctx, output.shapes);
    assert_eq!(
        painter.missing_texture_draws(),
        0,
        "a primitive referenced a texture that was never uploaded"
    );

    buf
}

fn write_png(path: &std::path::Path, buf: &[u32]) {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).expect("create snapshot directory");
    }
    let mut rgba = Vec::with_capacity(buf.len() * 4);
    for &px in buf {
        rgba.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, px as u8, 0xff]);
    }
    let file = std::fs::File::create(path).expect("create snapshot png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("png header")
        .write_image_data(&rgba)
        .expect("png data");
    eprintln!("wrote {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(buf: &[u32], x: u32, y: u32) -> u32 {
        buf[(y * WIDTH + x) as usize]
    }

    /// The whole frame must render: real widgets, real tessellation, CPU rasterization,
    /// no panics, and no texture referenced before it was uploaded (asserted inside
    /// [`render`]).
    ///
    /// The assertions are structural rather than per-pixel on purpose. A golden-image
    /// comparison would fail on every harmless layout change and would train people to
    /// re-bless it without looking; these check the properties that would actually be
    /// broken by a rasterizer bug.
    #[test]
    fn taskman_widgets_render_without_gaps_or_panics() {
        let buf = render();
        let bg = pack_rgb(
            theme::DARK.window_bg.r(),
            theme::DARK.window_bg.g(),
            theme::DARK.window_bg.b(),
        );

        // Something was drawn at all.
        let painted = buf.iter().filter(|&&p| p != bg).count();
        assert!(
            painted > (WIDTH * HEIGHT) as usize / 10,
            "only {painted} of {} pixels differ from the clear colour -- the frame is \
             essentially empty",
            WIDTH * HEIGHT
        );

        // The sidebar is a solid vertical band of its own colour on the far left.
        let sidebar = pack_rgb(
            theme::DARK.sidebar_bg.r(),
            theme::DARK.sidebar_bg.g(),
            theme::DARK.sidebar_bg.b(),
        );
        let sidebar_hits = (0..HEIGHT).filter(|&y| at(&buf, 2, y) == sidebar).count();
        assert!(
            sidebar_hits > (HEIGHT as usize * 3) / 4,
            "the sidebar band is missing: only {sidebar_hits} of {HEIGHT} rows match"
        );
    }

    /// The blue heat band must be a continuous painted run across the numeric columns.
    ///
    /// This is the property a cracking rasterizer breaks first: `heat_cells` paints one
    /// rect per numeric cell, edge to edge, so a single unpainted column between two
    /// cells means adjacent rectangles are not meeting.
    ///
    /// The band is located by the *longest continuous* run of painted pixels, not by the
    /// total painted count -- the chart grid above it paints more pixels overall but in
    /// separate tiles, so a total-count search finds the charts and then trips over the
    /// legitimate gaps between them.
    #[test]
    fn the_heat_band_has_no_unpainted_columns_between_cells() {
        let buf = render();
        let bg = pack_rgb(
            theme::DARK.window_bg.r(),
            theme::DARK.window_bg.g(),
            theme::DARK.window_bg.b(),
        );

        let longest_run = |y: u32| {
            let (mut best, mut best_start, mut run, mut start) = (0u32, 0u32, 0u32, 0u32);
            for x in 0..WIDTH {
                if at(&buf, x, y) == bg {
                    run = 0;
                } else {
                    if run == 0 {
                        start = x;
                    }
                    run += 1;
                    if run > best {
                        best = run;
                        best_start = start;
                    }
                }
            }
            (best, best_start)
        };

        let (run, start, y) = (0..HEIGHT)
            .map(|y| {
                let (run, start) = longest_run(y);
                (run, start, y)
            })
            .max_by_key(|&(run, _, _)| run)
            .expect("frame has rows");

        assert!(
            run > 380,
            "no continuous heat band found; longest painted run was {run}px on row {y}"
        );

        // Every pixel across that run must be painted -- no holes between cells.
        let holes: Vec<u32> = (start..start + run)
            .filter(|&x| at(&buf, x, y) == bg)
            .collect();
        assert!(
            holes.is_empty(),
            "unpainted columns inside the heat band on row {y}: {holes:?} -- adjacent              cell rectangles are not meeting"
        );
    }

    /// The chart tiles must actually contain their series colour, not just a border.
    #[test]
    fn the_charts_paint_their_series() {
        let buf = render();
        let accent = theme::DARK.cpu_graph;
        // Count pixels that are noticeably blue-dominant, as the CPU series is.
        let bluish = buf
            .iter()
            .filter(|&&p| {
                let (r, g, b) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff);
                b > 90 && b > r + 20 && g >= r && b as i64 - r as i64 > 30
            })
            .count();
        assert!(
            bluish > 500,
            "only {bluish} pixels carry the series colour {accent:?}; the charts did not \
             paint their fills or lines"
        );
    }

    /// Per-pixel RGB spread: how far apart the channels are.
    ///
    /// This is the measurement `llm-wiki/known-debt.md` records as the way to tell
    /// sub-pixel text from grayscale. Grayscale coverage cannot produce any spread of its
    /// own, because every channel receives the same coverage.
    fn spread(px: u32) -> u32 {
        let (r, g, b) = ((px >> 16) & 0xff, (px >> 8) & 0xff, px & 0xff);
        r.max(g).max(b) - r.min(g).min(b)
    }

    /// Enabling sub-pixel rendering must introduce colour fringing on glyph edges.
    ///
    /// This is the end-to-end proof that the whole chain works: `TextOptions.subpixel`
    /// reaches the rasterizer, the rasterizer writes per-channel coverage into the atlas,
    /// the blitter recognises those texels, and the per-channel gamma blend reaches the
    /// framebuffer. A break anywhere collapses the fringing to nothing.
    ///
    /// The comparison is made *between the two renders* rather than against an absolute
    /// threshold in some region: taskman's UI is full of genuinely coloured pixels (the
    /// blue heat band, the chart series, the selected row), so "this area should be
    /// grey" is not a claim that holds. What does hold is that switching the mode on
    /// changes glyph edges and nothing else.
    #[test]
    fn subpixel_rendering_adds_colour_fringing_to_glyph_edges() {
        use egui::epaint::text::SubpixelMode;

        let gray = render_with(SubpixelMode::Off);
        let lcd = render_with(SubpixelMode::Rgb);
        assert_eq!(gray.len(), lcd.len());

        let mut changed = 0usize;
        let mut more_fringed = 0usize;
        let mut worst_gain = 0u32;
        for (g, l) in gray.iter().zip(&lcd) {
            if g == l {
                continue;
            }
            changed += 1;
            let (sg, sl) = (spread(*g), spread(*l));
            if sl > sg {
                more_fringed += 1;
                worst_gain = worst_gain.max(sl - sg);
            }
        }

        assert!(
            changed > 1000,
            "only {changed} pixels changed when sub-pixel was enabled -- the mode is \
             not reaching the rasterizer or the blitter"
        );
        assert!(
            more_fringed > 500,
            "{more_fringed} of {changed} changed pixels gained channel spread; sub-pixel \
             coverage is not surviving to the framebuffer"
        );
        assert!(
            worst_gain > 8,
            "the strongest fringe gained only {worst_gain} of spread, which is not \
             distinguishable from rounding"
        );
    }

    /// Sub-pixel text must stay *text*: the fringing belongs on glyph edges, not smeared
    /// across the whole frame. Solid fills have no coverage gradient, so they must come
    /// out identical in both modes.
    #[test]
    fn subpixel_rendering_leaves_solid_fills_untouched() {
        use egui::epaint::text::SubpixelMode;

        let gray = render_with(SubpixelMode::Off);
        let lcd = render_with(SubpixelMode::Rgb);

        // The middle of the sidebar band: a solid fill with no text over it.
        for y in (100..500u32).step_by(37) {
            let i = (y * WIDTH + 2) as usize;
            assert_eq!(
                gray[i], lcd[i],
                "solid sidebar fill changed at y={y} when sub-pixel was enabled"
            );
        }
    }

    #[test]
    fn damaged_repaint_matches_full_paint_on_real_widgets() {
        let apply = |painter: &mut Painter, output: &mut egui::FullOutput| {
            for (id, deltas) in &output.textures_delta.set {
                for delta in deltas {
                    painter.set_texture(*id, delta);
                }
            }
            for id in &output.textures_delta.free {
                painter.free_texture(*id);
            }
            output.textures_delta.clear();
        };

        let ctx_dmg = egui::Context::default();
        theme::install_visuals(&ctx_dmg);
        ctx_dmg.set_theme(egui::ThemePreference::Dark);
        let mut dmg_painter = Painter::new();

        // Warmup + Frame A (idle pointer).
        let mut out_w = build_frame_with_input(&ctx_dmg, WIDTH, HEIGHT, Vec::new(), 0.0);
        apply(&mut dmg_painter, &mut out_w);
        let mut out_a = build_frame_with_input(&ctx_dmg, WIDTH, HEIGHT, Vec::new(), 0.1);
        apply(&mut dmg_painter, &mut out_a);
        let shape_ctx_a = ShapeContext {
            pixels_per_point: out_a.pixels_per_point,
            options: ctx_dmg.tessellation_options(|o| *o),
            font_tex_size: ctx_dmg.fonts(|f| f.font_image_size()),
            prepared_discs: ctx_dmg.fonts(|f| f.fonts.texture_atlas().prepared_discs()),
        };

        let bg = theme::DARK.window_bg;
        let mut dmg_buf = vec![0u32; (WIDTH * HEIGHT) as usize];
        {
            let mut target = Target::new(&mut dmg_buf, WIDTH, HEIGHT).unwrap();
            let s1 =
                dmg_painter.paint_shapes_damaged(&mut target, &shape_ctx_a, out_a.shapes, bg, true);
            assert_eq!(s1.dirty_tiles, s1.total_tiles);
        }

        // Frame B: pointer moves onto the chart (marker + cursor readout) and a row.
        let mut out_b = build_frame_with_input(
            &ctx_dmg,
            WIDTH,
            HEIGHT,
            vec![egui::Event::PointerMoved(pos2(200.0, 70.0))],
            0.2,
        );
        apply(&mut dmg_painter, &mut out_b);
        let shape_ctx_b = ShapeContext {
            pixels_per_point: out_b.pixels_per_point,
            options: ctx_dmg.tessellation_options(|o| *o),
            font_tex_size: ctx_dmg.fonts(|f| f.font_image_size()),
            prepared_discs: ctx_dmg.fonts(|f| f.fonts.texture_atlas().prepared_discs()),
        };
        let shapes_b_for_full = out_b.shapes.clone();
        {
            let mut target = Target::new(&mut dmg_buf, WIDTH, HEIGHT).unwrap();
            let s2 = dmg_painter.paint_shapes_damaged(
                &mut target,
                &shape_ctx_b,
                out_b.shapes,
                bg,
                false,
            );
            assert!(
                s2.dirty_tiles > 0 && s2.dirty_tiles < s2.total_tiles,
                "expected partial tile damage on row hover, got {}/{}",
                s2.dirty_tiles,
                s2.total_tiles
            );
        }

        // Reference full paint of Frame B using the exact same textures/galleys.
        let mut full_buf = vec![0u32; (WIDTH * HEIGHT) as usize];
        {
            let mut target = Target::new(&mut full_buf, WIDTH, HEIGHT).unwrap();
            Painter::clear(&mut target, bg);
            dmg_painter.paint_shapes(&mut target, &shape_ctx_b, shapes_b_for_full);
        }
        assert_eq!(
            dmg_buf, full_buf,
            "damaged repaint of real tablekit/chart widgets must be pixel-identical to a full paint"
        );

        // Identical steady-state frame C -> zero dirty tiles.
        let mut out_c = build_frame_with_input(&ctx_dmg, WIDTH, HEIGHT, Vec::new(), 0.2);
        apply(&mut dmg_painter, &mut out_c);
        {
            let mut target = Target::new(&mut dmg_buf, WIDTH, HEIGHT).unwrap();
            let s3 = dmg_painter.paint_shapes_damaged(
                &mut target,
                &shape_ctx_b,
                out_c.shapes,
                bg,
                false,
            );
            assert_eq!(
                s3.dirty_tiles, 0,
                "identical frame must produce zero dirty tiles: {:?}",
                s3.dirty_rects
            );
        }
    }

    /// Headless benchmark for a 2288x1575 Processes-like frame (full vs 1-row hover damage).
    #[test]
    #[ignore = "timing benchmark; run explicitly with --ignored --nocapture"]
    fn bench_2288x1575_processes_frame() {
        const BW: u32 = 2288;
        const BH: u32 = 1575;
        let ctx = egui::Context::default();
        theme::install_visuals(&ctx);
        ctx.set_theme(egui::ThemePreference::Dark);
        let mut painter = Painter::new();

        let apply = |painter: &mut Painter, output: &mut egui::FullOutput| {
            for (id, deltas) in &output.textures_delta.set {
                for delta in deltas {
                    painter.set_texture(*id, delta);
                }
            }
            for id in &output.textures_delta.free {
                painter.free_texture(*id);
            }
            output.textures_delta.clear();
        };

        let mut warmup = build_frame_with_input(&ctx, BW, BH, Vec::new(), 0.0);
        apply(&mut painter, &mut warmup);

        let t_pass = std::time::Instant::now();
        let mut out = build_frame_with_input(&ctx, BW, BH, Vec::new(), 0.1);
        let pass_ms = t_pass.elapsed().as_secs_f64() * 1000.0;
        apply(&mut painter, &mut out);
        let shape_ctx = ShapeContext {
            pixels_per_point: out.pixels_per_point,
            options: ctx.tessellation_options(|o| *o),
            font_tex_size: ctx.fonts(|f| f.font_image_size()),
            prepared_discs: ctx.fonts(|f| f.fonts.texture_atlas().prepared_discs()),
        };

        let mut buf = vec![0u32; (BW * BH) as usize];
        let mut target = Target::new(&mut buf, BW, BH).unwrap();
        let t_full = std::time::Instant::now();
        painter.paint_shapes_damaged(
            &mut target,
            &shape_ctx,
            out.shapes,
            theme::DARK.window_bg,
            true,
        );
        let full_paint_ms = t_full.elapsed().as_secs_f64() * 1000.0;

        let mut out_hover = build_frame_with_input(
            &ctx,
            BW,
            BH,
            vec![egui::Event::PointerMoved(pos2(200.0, 70.0))],
            0.2,
        );
        apply(&mut painter, &mut out_hover);
        let t_dmg = std::time::Instant::now();
        let stats = painter.paint_shapes_damaged(
            &mut target,
            &shape_ctx,
            out_hover.shapes,
            theme::DARK.window_bg,
            false,
        );
        let dmg_paint_ms = t_dmg.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "2288x1575 bench: pass={pass_ms:.2}ms full_paint={full_paint_ms:.2}ms dmg_paint={dmg_paint_ms:.2}ms dirty_tiles={}/{}",
            stats.dirty_tiles, stats.total_tiles
        );
    }

    // ------------------------------------------------- chart geometry regression
    //
    // The gates below pin the artifact users reported on the Performance tab's
    // per-logical-processor grid: thin straight strokes in the series colour
    // that belong to no data run, bridge the regions where the tile paints no
    // area fill, and run diagonally over thirty sample intervals. Measured off
    // the screenshot, one tile carried a stroke from 45% of its width at full
    // height down to 95% at 10%, with no fill anywhere under it.
    //
    // Two defects produced it, and both show up as GEOMETRY rather than as a
    // wrong colour:
    //
    // * `chart::core_chart` mapped the raw `&[f64]` straight into `Pos2`, so a
    //   NaN sample became `Pos2::new(x, NaN)`. epaint has no gap support: a
    //   path is not culled for holding a non-finite point, it reaches the
    //   rasterizer as garbage.
    // * `chart::x_on_axis` clamps its fraction to `[-1, 1]`, so a stamp from
    //   outside the axis is pinned one rect width off the plot instead of
    //   breaking the line -- a long bridging segment across live data.
    //
    // These assert on tessellated geometry wherever they can: that is what the
    // rasterizer consumes, and unlike a pixel comparison it does not care where
    // egui happened to lay the tile out.

    /// Frame size a single-tile gate lays its chart out in.
    const GATE_W: f32 = 420.0;
    const GATE_H: f32 = 220.0;

    /// Tile size, close to a real per-core grid cell (about 88x56 at the
    /// default 4x4 layout, which is where the artifact was seen).
    const TILE: egui::Vec2 = vec2(160.0, 90.0);

    /// Values with four runs and three gaps, in twelve samples: a run, a NaN, a
    /// run, +inf, a run, -inf, and a further run.
    ///
    /// `f64::clamp` returns NaN for NaN and saturates the infinities, so the
    /// old `y()` turned this series into one path through all twelve samples:
    /// a NaN vertex, and a spike pinned to the ceiling and the floor.
    const GAP_VALUES: [f64; 12] = [
        20.0,
        35.0,
        50.0,
        f64::NAN,
        60.0,
        45.0,
        f64::INFINITY,
        70.0,
        30.0,
        f64::NEG_INFINITY,
        55.0,
        25.0,
    ];

    /// Kernel series for the same frame, with its gap at a DIFFERENT index than
    /// the user series'. The kernel band is filled from its own slice while the
    /// x positions come from the value series' timestamps, so a fix that reuses
    /// the value runs' boundaries for the overlay leaves this one bridging.
    const GAP_KERNEL: [f64; 12] = [
        4.0,
        4.0,
        5.0,
        5.0,
        5.0,
        f64::NAN,
        6.0,
        6.0,
        6.0,
        7.0,
        7.0,
        7.0,
    ];

    /// Which chart widget a gate frame paints. Both have to break at a gap.
    #[derive(Clone, Copy, Debug)]
    enum Widget {
        Core,
        Multi,
    }

    /// The axis a gate frame builds: `(timestamps, window in seconds)`, or
    /// `None` for a chart drawn without one.
    type GateAxis<'a> = Option<(&'a [u64], u32)>;

    /// One case for [`a_short_or_tiny_series_stays_inside_its_tile`]: its
    /// samples and the axis they are plotted on.
    type SeriesCase<'a> = (&'a [f64], GateAxis<'a>);

    /// One windowless chart frame: the rects the widgets claimed, the shapes
    /// egui recorded, and the same frame after tessellation.
    struct Frame {
        ctx: egui::Context,
        tiles: Vec<Rect>,
        shapes: Vec<egui::epaint::ClippedShape>,
        primitives: Vec<egui::ClippedPrimitive>,
        textures: egui::TexturesDelta,
        pixels_per_point: f32,
    }

    impl Drop for Frame {
        /// epaint debug-asserts that every `TexturesDelta` was applied or
        /// explicitly cleared, and a frame the geometry gates only inspect is
        /// never painted, so its deltas would otherwise panic on drop.
        fn drop(&mut self) {
            self.textures.clear();
        }
    }

    impl Frame {
        /// Tessellate a finished pass and keep everything the gates assert on.
        fn finish(ctx: egui::Context, tiles: Vec<Rect>, mut output: egui::FullOutput) -> Self {
            let primitives = ctx.tessellate(output.shapes.clone(), output.pixels_per_point);
            Self {
                ctx,
                tiles,
                shapes: std::mem::take(&mut output.shapes),
                primitives,
                textures: std::mem::take(&mut output.textures_delta),
                pixels_per_point: output.pixels_per_point,
            }
        }

        /// The renderer inputs a real backend gathers from the context.
        fn shape_context(&self) -> ShapeContext {
            ShapeContext {
                pixels_per_point: self.pixels_per_point,
                options: self.ctx.tessellation_options(|o| *o),
                font_tex_size: self.ctx.fonts(|f| f.font_image_size()),
                prepared_discs: self.ctx.fonts(|f| f.fonts.texture_atlas().prepared_discs()),
            }
        }

        /// Every vertex of every tessellated primitive: exactly what the
        /// rasterizer would consume, before any clipping.
        fn vertices(&self) -> Vec<Pos2> {
            let mut out = Vec::new();
            for primitive in &self.primitives {
                if let egui::epaint::Primitive::Mesh(mesh) = &primitive.primitive {
                    out.extend(mesh.vertices.iter().map(|v| v.pos));
                }
            }
            out
        }

        /// Paint the frame with the software painter, exactly as the CPU
        /// renderer does: upload the atlas the pass extracted, then rasterize
        /// the untessellated shapes (the path the `Software` renderer uses, and
        /// the only renderer this project ships).
        fn paint(&mut self, width: u32, height: u32) -> Vec<u32> {
            let mut painter = Painter::new();
            for (id, deltas) in &self.textures.set {
                for delta in deltas {
                    painter.set_texture(*id, delta);
                }
            }
            for id in &self.textures.free {
                painter.free_texture(*id);
            }
            self.textures.clear();

            let mut buf = vec![0u32; (width * height) as usize];
            let mut target = Target::new(&mut buf, width, height).expect("target fits");
            Painter::clear(&mut target, theme::DARK.window_bg);
            painter.paint_shapes(&mut target, &self.shape_context(), self.shapes.clone());
            assert_eq!(
                painter.missing_texture_draws(),
                0,
                "a primitive referenced a texture that was never uploaded"
            );
            buf
        }
    }

    /// Build one headless frame that contains nothing but chart widgets.
    ///
    /// `stamps` is the axis `(timestamps, window in seconds)`, or `None` for a
    /// chart without one.
    fn chart_frame(
        widget: Widget,
        samples: &[f64],
        kernel: Option<&[f64]>,
        stamps: GateAxis<'_>,
    ) -> Frame {
        let ctx = egui::Context::default();
        theme::install_visuals(&ctx);
        ctx.set_theme(egui::ThemePreference::Dark);
        let axis = stamps.map(|(ts, window_s)| chart::TimeAxis::new(ts, window_s));
        let mut tiles = Vec::new();
        let output = ctx.run_ui(
            egui::RawInput {
                time: Some(0.0),
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(GATE_W, GATE_H))),
                ..Default::default()
            },
            |ui| {
                let response = match widget {
                    Widget::Core => chart::core_chart(
                        ui,
                        TILE,
                        samples,
                        kernel.map(|k| (k, theme::DARK.cpu_kernel_graph)),
                        theme::DARK.cpu_graph,
                        "CPU 0",
                        axis,
                    ),
                    Widget::Multi => chart::chart_multi(
                        ui,
                        TILE,
                        &[MultiSeries::new(
                            "CPU",
                            samples.to_vec(),
                            theme::DARK.cpu_graph,
                        )],
                        100.0,
                        axis,
                        chart::fmt_percent,
                    ),
                };
                tiles.push(response.rect);
            },
        );
        Frame::finish(ctx, tiles, output)
    }

    /// x of sample `i` on a window-anchored axis, as [`chart::TimeAxis`] places
    /// it.
    ///
    /// This restates the axis's documented contract rather than borrowing the
    /// widget's own arithmetic: the window opens at `newest - window`, a sample
    /// outside it is pinned to the axis edge, and a timestamp slice shorter
    /// than the series falls back to even spacing. The gates need it to know
    /// what ONE sample interval is in points, which is the width a stroke edge
    /// is allowed to span.
    fn axis_x(rect: Rect, stamps: &[u64], window_ms: u64, i: usize, n: usize) -> f32 {
        let even = rect.left() + rect.width() * i as f32 / (n - 1) as f32;
        let (Some(&last), Some(&stamp)) = (stamps.last(), stamps.get(i)) else {
            return even;
        };
        let window = window_ms.max(1);
        let t0 = last.saturating_sub(window);
        let frac = (stamp as f64 - t0 as f64) / window as f64;
        rect.left() + rect.width() * frac.clamp(-1.0, 1.0) as f32
    }

    /// The widest span between two consecutive samples on the axis, in points.
    ///
    /// This is the "one sample interval" a stroke edge may cover: a segment
    /// joining two neighbouring samples is at most this wide, so anything wider
    /// jumped over data that does not exist.
    fn one_interval(rect: Rect, stamps: &[u64], window_ms: u64, n: usize) -> f32 {
        let xs: Vec<f32> = (0..n)
            .map(|i| axis_x(rect, stamps, window_ms, i, n))
            .collect();
        xs.windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0, f32::max)
    }

    /// Every painted polyline, one point list per `Shape::Path`.
    ///
    /// A chart frame contains no other paths -- the chrome is rects and line
    /// segments -- so this counts SERIES RUNS, which is the property the gate
    /// is after: a gap has to end the line and start a new one, because epaint
    /// cannot express a break inside a path.
    fn polyline_runs(shapes: &[egui::epaint::ClippedShape]) -> Vec<Vec<Pos2>> {
        shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Path(path) => Some(path.points.clone()),
                _ => None,
            })
            .collect()
    }

    /// The x extent of every triangle of every mesh in the frame: the area
    /// fills.
    ///
    /// The strip builder emits one triangle pair per segment, so a triangle
    /// wider than one sample interval means the fill was closed over a gap.
    /// NaN propagates through the fold on purpose -- `f32::min`/`max` ignore
    /// NaN, which would silently excuse a poisoned triangle.
    fn mesh_triangle_x_spans(shapes: &[egui::epaint::ClippedShape]) -> Vec<f32> {
        let mut spans = Vec::new();
        for clipped in shapes {
            let egui::Shape::Mesh(mesh) = &clipped.shape else {
                continue;
            };
            for triangle in mesh.indices.chunks_exact(3) {
                let x: Vec<f32> = triangle
                    .iter()
                    .map(|&i| mesh.vertices[i as usize].pos.x)
                    .collect();
                let mut span = 0.0f32;
                for pair in [[x[0], x[1]], [x[1], x[2]], [x[2], x[0]]] {
                    let d = (pair[0] - pair[1]).abs();
                    if !(d <= span) {
                        span = d;
                    }
                }
                spans.push(span);
            }
        }
        spans
    }

    /// The tile grown by its own size: the most a chart may paint outside its
    /// rect.
    ///
    /// The window-anchored axis deliberately carries one sample from just
    /// before the window so the polyline can enter at the true slope, which
    /// puts one vertex LEFT of the tile; `x_on_axis` clamps that lead-in to at
    /// most one rect width out. Anything further is a bridge to nowhere.
    fn allowed_rect(tile: Rect) -> Rect {
        tile.expand2(tile.size())
    }

    /// GATE: a missing sample -- NaN, +inf or -inf -- must END the series line
    /// and start a new one, never be drawn through.
    ///
    /// `chart_multi` already did this (`measured_runs`); `core_chart` did not,
    /// and the per-core grid is where the garbage strokes were seen. On the old
    /// code this frame carries ONE path through all twelve samples, including
    /// the NaN that epaint turns into a stroke to nowhere, and the kernel fill
    /// is poisoned the same way at its own gap.
    #[test]
    fn a_core_tile_breaks_its_series_at_missing_samples() {
        let stamps: Vec<u64> = (0..GAP_VALUES.len() as u64).map(|i| i * 1_000).collect();
        // Three runs of values, so four runs painted.
        let runs_expected = 4;
        for widget in [Widget::Core, Widget::Multi] {
            let frame = chart_frame(
                widget,
                &GAP_VALUES,
                Some(&GAP_KERNEL),
                Some((stamps.as_slice(), 12)),
            );
            let tile = frame.tiles[0];
            let interval = one_interval(tile, stamps.as_slice(), 12_000, GAP_VALUES.len());

            // 1. Nothing poisoned reached the tessellator.
            for pos in frame.vertices() {
                assert!(
                    pos.x.is_finite() && pos.y.is_finite(),
                    "{widget:?}: egui emitted the non-finite vertex {pos:?} -- a NaN \
                     sample was plotted instead of breaking the line"
                );
            }

            // 2. A gap separates runs instead of being drawn through.
            let runs = polyline_runs(&frame.shapes);
            assert!(
                runs.len() >= runs_expected,
                "{widget:?}: {} painted polylines for {runs_expected} measured runs -- a \
                 stroke spans a gap in the data",
                runs.len()
            );

            // 3. No stroke edge bridges more than one sample interval.
            for run in &runs {
                for pair in run.windows(2) {
                    let dx = (pair[1].x - pair[0].x).abs();
                    assert!(
                        dx <= interval + 2.0,
                        "{widget:?}: a stroke edge spans {dx:.1} px, more than one sample \
                         interval ({interval:.1} px) -- it joined samples across a gap"
                    );
                }
            }

            // 4. Neither does the area fill, which is built one triangle pair
            //    per segment.
            for span in mesh_triangle_x_spans(&frame.shapes) {
                assert!(
                    span <= interval + 2.0,
                    "{widget:?}: a fill triangle spans {span:.1} px, more than one sample \
                     interval ({interval:.1} px) -- the fill was closed over a gap"
                );
            }

            // 5. Nothing paints further outside the tile than the lead-in
            //    overhang the axis documents.
            let allowed = allowed_rect(tile);
            for pos in frame.vertices() {
                assert!(
                    allowed.contains(pos),
                    "{widget:?}: the chart painted {pos:?}, further outside the tile \
                     {tile:?} than its own size -- that is the bridging stroke"
                );
            }
        }
    }

    /// GATE: a sample whose timestamp falls outside the axis must break the
    /// line, not be pinned a rect width off the plot and joined to live data.
    ///
    /// This is the other half of the reported artifact: a stroke that starts at
    /// the tile's own left edge and runs diagonally down to 10%. A wall clock
    /// that steps backwards -- an NTP correction, a resume, a duplicated or
    /// stale stamp -- is enough to produce one.
    #[test]
    fn an_out_of_window_stamp_breaks_the_line_instead_of_bridging_it() {
        // Epoch milliseconds, because the axis is anchored to the newest stamp
        // and only then does a stale one fall outside it.
        const NEWEST: u64 = 1_797_000_000_000;
        let regular: Vec<u64> = (0..11).map(|i| NEWEST - 10_000 + i * 1_000).collect();
        let mut stamps = regular.clone();
        // Sample 5 carries a stamp from an hour earlier.
        stamps[5] = NEWEST - 3_600_000;
        let values: Vec<f64> = (0..11).map(|i| 30.0 + (i % 5) as f64 * 8.0).collect();

        for widget in [Widget::Core, Widget::Multi] {
            let frame = chart_frame(
                widget,
                values.as_slice(),
                None,
                Some((stamps.as_slice(), 10)),
            );
            let tile = frame.tiles[0];
            let slack = one_interval(tile, regular.as_slice(), 10_000, regular.len()) + 2.0;
            let runs = polyline_runs(&frame.shapes);
            assert!(
                !runs.is_empty(),
                "{widget:?}: the chart painted no polyline at all"
            );

            // The stale sample is clamped a whole rect width left of the plot.
            // On its own that is the documented lead-in; what must not happen
            // is that it is JOINED to the samples inside the window, which is
            // what turns it into a stroke across the graph.
            let straddling = runs
                .iter()
                .filter(|run| run.iter().any(|p| p.x < tile.left() - slack))
                .count();
            assert!(
                straddling == 0 || runs.len() >= 2,
                "{widget:?}: {straddling} of {} run(s) join a sample from outside the axis \
                 to samples inside it -- the pinned sample is drawn as a bridging stroke",
                runs.len()
            );

            // ...and no single edge may cross more plot than the plot is wide.
            for run in &runs {
                for pair in run.windows(2) {
                    let dx = (pair[1].x - pair[0].x).abs();
                    assert!(
                        dx <= tile.width() + 2.0,
                        "{widget:?}: a stroke edge spans {dx:.1} px across a {:.1} px tile",
                        tile.width()
                    );
                }
            }

            let allowed = allowed_rect(tile);
            for pos in frame.vertices() {
                assert!(
                    pos.x.is_finite() && pos.y.is_finite(),
                    "{widget:?}: egui emitted the non-finite vertex {pos:?}"
                );
                assert!(
                    allowed.contains(pos),
                    "{widget:?}: the chart painted {pos:?}, further outside the tile \
                     {tile:?} than its own size"
                );
            }
        }
    }

    /// A stamp that steps BACKWARDS but stays inside the axis must not move a
    /// vertex off the tile or poison the geometry.
    ///
    /// Coverage rather than detection: the sample is real data at a real
    /// instant, so a chart may legitimately draw the little Z the ordering
    /// implies. What it may not do is leave the tile while handling it.
    #[test]
    fn a_backwards_stamp_stays_inside_its_tile() {
        let stamps: &[u64] = &[0, 1_000, 2_000, 1_500, 3_000];
        let values: &[f64] = &[10.0, 20.0, 30.0, 25.0, 40.0];
        for widget in [Widget::Core, Widget::Multi] {
            let frame = chart_frame(widget, values, None, Some((stamps, 2)));
            let tile = frame.tiles[0];
            let allowed = allowed_rect(tile);
            for pos in frame.vertices() {
                assert!(
                    pos.x.is_finite() && pos.y.is_finite(),
                    "{widget:?}: egui emitted the non-finite vertex {pos:?}"
                );
                assert!(
                    allowed.contains(pos),
                    "{widget:?}: a backwards stamp put a vertex at {pos:?}, outside the \
                     tile {tile:?} by more than its own size"
                );
            }
            assert!(
                !polyline_runs(&frame.shapes).is_empty(),
                "{widget:?}: the chart painted no polyline at all"
            );
        }
    }

    /// A series longer than its timestamp slice, and the shortest series a
    /// chart can be asked to draw, must stay on screen: no panic in the paint
    /// path, no vertex outside the tile.
    ///
    /// The slice going short of the series is a desynchronization this code has
    /// already seen once, and in the paint path it used to be a panic. The
    /// two-sample cases cover the boundaries that remain: a run of one sample
    /// is a dot, not a one-point path (epaint tessellates a path from two
    /// points up), and a gap at the end must not leave a half-built one behind.
    #[test]
    fn a_short_or_tiny_series_stays_inside_its_tile() {
        let stamps: Vec<u64> = (0..4).map(|i| i * 1_000).collect();
        let long: Vec<f64> = (0..12).map(|i| 20.0 + (i % 5) as f64 * 6.0).collect();
        let two: &[f64] = &[10.0, 20.0];
        let two_gapped: &[f64] = &[10.0, f64::NAN];
        let cases: [SeriesCase<'_>; 3] = [
            (long.as_slice(), Some((stamps.as_slice(), 12))),
            (two, None),
            (two_gapped, None),
        ];

        for widget in [Widget::Core, Widget::Multi] {
            for (values, axis) in &cases {
                let frame = chart_frame(widget, *values, None, *axis);
                let tile = frame.tiles[0];
                let allowed = allowed_rect(tile);
                for pos in frame.vertices() {
                    assert!(
                        pos.x.is_finite() && pos.y.is_finite(),
                        "{widget:?}: egui emitted the non-finite vertex {pos:?} for a \
                         series of {} samples",
                        values.len()
                    );
                    assert!(
                        allowed.contains(pos),
                        "{widget:?}: the chart painted {pos:?} for a series of {} samples, \
                         outside the tile {tile:?} by more than its own size",
                        values.len()
                    );
                }
                // Two painted samples are a polyline; one is a dot, which is
                // not a path and must not be asserted as one.
                if values.iter().filter(|v| v.is_finite()).count() >= 2 {
                    assert!(
                        !polyline_runs(&frame.shapes).is_empty(),
                        "{widget:?}: a series of {} samples painted no polyline",
                        values.len()
                    );
                }
            }
        }
    }

    /// A gap series must not paint outside the tile that owns it.
    ///
    /// The pixel half of the gate, deliberately structural: two tiles side by
    /// side, the first fed the adversarial series, and the only claims are that
    /// the series colour stays inside the two tile rects and that both tiles
    /// actually painted their series. A bridging stroke that escapes its clip,
    /// or a tile that paints nothing where a curve belongs, fails it; a layout
    /// change does not.
    #[test]
    fn gap_series_pixels_stay_inside_their_own_tile() {
        const PW: u32 = 420;
        const PH: u32 = 140;
        let stamps: Vec<u64> = (0..GAP_VALUES.len() as u64).map(|i| i * 1_000).collect();
        let healthy: Vec<f64> = (0..GAP_VALUES.len())
            .map(|i| 15.0 + (i % 5) as f64 * 7.0)
            .collect();

        let ctx = egui::Context::default();
        theme::install_visuals(&ctx);
        ctx.set_theme(egui::ThemePreference::Dark);
        let mut tiles = Vec::new();
        let output = ctx.run_ui(
            egui::RawInput {
                time: Some(0.0),
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(PW as f32, PH as f32))),
                ..Default::default()
            },
            |ui| {
                // A gap wide enough that a stroke leaving one tile has somewhere
                // to land rather than immediately hitting its neighbour.
                ui.spacing_mut().item_spacing = vec2(24.0, 4.0);
                ui.horizontal(|ui| {
                    let axis = chart::TimeAxis::new(&stamps, 12);
                    for (values, label) in [
                        (GAP_VALUES.as_slice(), "CPU 0"),
                        (healthy.as_slice(), "CPU 1"),
                    ] {
                        let response = chart::core_chart(
                            ui,
                            TILE,
                            values,
                            None,
                            theme::DARK.cpu_graph,
                            label,
                            Some(axis),
                        );
                        tiles.push(response.rect);
                    }
                });
            },
        );
        let mut frame = Frame::finish(ctx, tiles, output);
        let buf = frame.paint(PW, PH);
        assert_eq!(frame.tiles.len(), 2, "two tiles are expected");

        // The series colour, as [`the_charts_paint_their_series`] defines it: a
        // pixel that is clearly blue-dominant. The chrome around it -- card
        // fill, grid, border -- is neutral grey and cannot be mistaken for one.
        let tinted = |px: u32| {
            let (r, g, b) = ((px >> 16) & 0xff, (px >> 8) & 0xff, px & 0xff);
            b > 70 && b > r + 30
        };
        let at_px = |x: u32, y: u32| buf[(y * PW + x) as usize];

        // 1. Nothing of the series escapes the tile that drew it. The two-pixel
        //    margin covers the tile's own antialiasing and the half-pixel the
        //    painter snaps its clip rect by.
        let mut outside = 0usize;
        let mut first = None;
        for y in 0..PH {
            for x in 0..PW {
                let p = pos2(x as f32 + 0.5, y as f32 + 0.5);
                if frame.tiles.iter().any(|tile| tile.expand(2.0).contains(p)) {
                    continue;
                }
                if tinted(at_px(x, y)) {
                    outside += 1;
                    if first.is_none() {
                        first = Some(p);
                    }
                }
            }
        }
        assert_eq!(
            outside, 0,
            "{outside} series-coloured pixels outside every tile (first at {first:?}) -- a \
             stroke escaped the chart that owns it"
        );

        // 2. Both tiles painted their series, so the check above is not passing
        //    on an empty frame.
        for (i, tile) in frame.tiles.iter().enumerate() {
            let mut hit = 0usize;
            for y in 0..PH {
                for x in 0..PW {
                    if tile.contains(pos2(x as f32 + 0.5, y as f32 + 0.5)) && tinted(at_px(x, y)) {
                        hit += 1;
                    }
                }
            }
            assert!(
                hit > 50,
                "tile {i} painted only {hit} series-coloured pixels -- the chart did not \
                 draw its series at all"
            );
        }
    }
}

/// Write the CPU-rendered frame to a PNG so it can actually be looked at.
///
/// Off by default -- a test that writes files on every `cargo test` is a nuisance. Set
/// `TASKMAN_RENDER_SNAPSHOT` to a path to enable it. A relative path resolves against
/// this crate's directory (cargo's test working directory), so prefer an absolute one.
#[cfg(test)]
#[test]
fn write_cpu_frame_snapshot_when_requested() {
    let Ok(raw) = std::env::var("TASKMAN_RENDER_SNAPSHOT") else {
        return;
    };
    let path = std::path::PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        // Resolve against the workspace root rather than crates/tm-app, which is what
        // anyone typing `target/frame.png` actually means.
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    };
    write_png(&path, &render());

    // ...and the sub-pixel version alongside it, for an A/B look.
    let lcd = path.with_file_name(format!(
        "{}-cleartype.png",
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "frame".to_owned())
    ));
    write_png(&lcd, &render_with(egui::epaint::text::SubpixelMode::Rgb));
}

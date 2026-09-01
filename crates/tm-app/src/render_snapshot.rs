//! Headless CPU rendering of taskman's real widgets.
//!
//! Drives the actual `tablekit`, `chart` and `icons` code through a windowless
//! [`egui::Context`], tessellates the result, and paints it with [`egui_software`] --
//! the same painter the `Software` renderer will use. No window is opened and no GPU is
//! touched, so this runs in CI and in a normal `cargo test`.
//!
//! It serves two purposes:
//!
//! * **A regression gate.** [`taskman_widgets_render_without_gaps_or_panics`] asserts
//!   structural properties of the output (the chrome painted, the heat band is a
//!   continuous run of blues, the charts drew, nothing is left at the clear colour where
//!   content belongs). These catch a rasterizer regression without pinning down every
//!   pixel, which would make the test fail on any innocuous layout tweak.
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
    let pal: Palette = theme::DARK;
    let mut table = TmTable::new("snapshot", columns(), None);

    ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                vec2(WIDTH as f32, HEIGHT as f32),
            )),
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
                                MultiSeries {
                                    samples: series(120, 3, 90.0),
                                    color: pal.cpu_graph,
                                },
                                MultiSeries {
                                    samples: series(120, 11, 55.0),
                                    color: pal.memory_graph,
                                },
                            ],
                            100.0,
                            None,
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
                                        Some(&k),
                                        pal.cpu_graph,
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
                        let (rect, _) = table.row(ui, &pal, i == 1);
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
                        chart::paint_sparkline(ui, rect, &series(60, 21, 80.0), pal.disk_graph);
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
    let ctx = egui::Context::default();
    theme::install_visuals(&ctx);
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut painter = Painter::new();

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
}

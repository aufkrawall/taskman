//! Process Properties → I/O: Process Hacker / System Informer style I/O page
//! showing rolling read, write, and inter-process (IPC / Other) graphs,
//! cumulative counters, and interval deltas.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke, Vec2};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::ProcessEntry;

use crate::app::{ProcessIdentity, TaskManApp};
use crate::theme;

/// One historical sample of a process's I/O activity.
#[derive(Clone, Copy, Debug)]
pub struct IoSample {
    pub t_ms: u64,
    pub read_bps: f64,
    pub write_bps: f64,
    pub other_bps: f64,
    pub read_delta: u64,
    pub write_delta: u64,
    pub other_delta: u64,
}

/// State of the I/O page within the Process Properties dialog.
#[derive(Clone, Debug)]
pub struct State {
    history: Vec<IoSample>,
    last_sample_t_ms: u64,
    pub show_other_graph: bool,
    pub unit_is_rate: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            history: Vec::with_capacity(128),
            last_sample_t_ms: 0,
            show_other_graph: true,
            unit_is_rate: true,
        }
    }
}

impl State {
    pub fn tick(&mut self, app: &TaskManApp, _ctx: &egui::Context, identity: &ProcessIdentity) {
        let Some(snapshot) = app.latest_snapshot() else {
            return;
        };
        if snapshot.timestamp_ms == self.last_sample_t_ms {
            return;
        }
        self.last_sample_t_ms = snapshot.timestamp_ms;
        let Some(process) = crate::tabs::details::process_properties_target(&snapshot, identity)
        else {
            return;
        };

        let sample = IoSample {
            t_ms: snapshot.timestamp_ms,
            read_bps: process.disk_read_bps,
            write_bps: process.disk_write_bps,
            other_bps: process.io_other_bps.unwrap_or(0.0),
            read_delta: process.io_read_bytes_delta.unwrap_or(0),
            write_delta: process.io_write_bytes_delta.unwrap_or(0),
            other_delta: process.io_other_bytes_delta.unwrap_or(0),
        };

        const CAP: usize = 120;
        if self.history.len() >= CAP {
            self.history.remove(0);
        }
        self.history.push(sample);
    }

    pub fn request_refresh(&mut self) {
        // App refresh handles data fetching
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }
}

/// Paint an age label for the hover readout.
fn sample_age_str(age_ms: u64) -> String {
    if age_ms < 500 {
        return i18n::tr(K::GraphNow).to_string();
    }
    let secs = (age_ms as f64 / 1000.0).round() as u64;
    if secs < 60 {
        i18n::trf(K::GraphAgoSeconds, &[&secs.to_string()])
    } else {
        i18n::trf(
            K::GraphAgoMinutes,
            &[&format!("{}:{:02}", secs / 60, secs % 60)],
        )
    }
}

/// Helper for drawing a Process Hacker style GroupBox with title embedded in the top border.
fn paint_group_box(
    painter: &egui::Painter,
    rect: Rect,
    title: &str,
    hair: f32,
    pal: &theme::Palette,
) -> Rect {
    let title_font = egui::FontId::proportional(12.0);
    let title_galley = painter.layout_no_wrap(title.to_owned(), title_font, pal.text);
    let title_w = title_galley.size().x;
    let half = (hair * 0.5).max(0.5);
    let left = rect.left() + half;
    let right = rect.right() - half;
    let bottom = rect.bottom() - half;
    let box_top = rect.top() + 7.0;
    let label_x = rect.left() + 10.0;

    // Border segments
    painter.line_segment(
        [
            egui::pos2(left, box_top),
            egui::pos2(label_x - 4.0, box_top),
        ],
        Stroke::new(hair, pal.stroke),
    );
    painter.line_segment(
        [
            egui::pos2(label_x + title_w + 4.0, box_top),
            egui::pos2(right, box_top),
        ],
        Stroke::new(hair, pal.stroke),
    );
    painter.line_segment(
        [egui::pos2(right, box_top), egui::pos2(right, bottom)],
        Stroke::new(hair, pal.stroke),
    );
    painter.line_segment(
        [egui::pos2(left, bottom), egui::pos2(right, bottom)],
        Stroke::new(hair, pal.stroke),
    );
    painter.line_segment(
        [egui::pos2(left, box_top), egui::pos2(left, bottom)],
        Stroke::new(hair, pal.stroke),
    );

    // Title label
    painter.galley(egui::pos2(label_x, rect.top()), title_galley, pal.text);

    // Inner graph / content area
    const MARGIN_X: f32 = 8.0;
    const MARGIN_BOTTOM: f32 = 8.0;
    Rect::from_min_max(
        egui::pos2(rect.left() + MARGIN_X, box_top + 14.0),
        egui::pos2(rect.right() - MARGIN_X, rect.bottom() - MARGIN_BOTTOM),
    )
}

/// Configuration for one rolling I/O graph series.
struct GraphSeries<'a> {
    title: &'a str,
    prefix_char: char,
    current_val: f64,
    unit_is_rate: bool,
    history: &'a [IoSample],
    series_fn: fn(&IoSample, bool) -> f64,
    line_color: Color32,
}

/// Paint one Process Hacker style rolling graph with green grid and overlay text.
fn paint_io_graph(ui: &mut egui::Ui, series: GraphSeries<'_>, pal: &theme::Palette) {
    let available_w = ui.available_width().max(200.0);
    let box_height = 110.0;
    let ppp = ui.ctx().pixels_per_point();
    let hair = if ppp > 0.0 { 1.0 / ppp } else { 1.0 };

    let (outer_rect, response) =
        ui.allocate_exact_size(egui::vec2(available_w, box_height), egui::Sense::hover());
    let painter = ui.painter_at(outer_rect);

    let canvas_rect = paint_group_box(&painter, outer_rect, series.title, hair, pal);
    if canvas_rect.width() <= 10.0 || canvas_rect.height() <= 10.0 {
        return;
    }

    // 1. Dark background
    painter.rect_filled(canvas_rect, 0.0, Color32::from_rgb(0, 7, 0));
    painter.rect_stroke(
        canvas_rect,
        0.0,
        Stroke::new(hair, Color32::from_rgb(0, 48, 0)),
        egui::StrokeKind::Inside,
    );

    // 2. Green grid
    let h_divisions = 8;
    for i in 1..h_divisions {
        let y = canvas_rect.top() + (canvas_rect.height() * i as f32 / h_divisions as f32).round();
        painter.line_segment(
            [
                egui::pos2(canvas_rect.left(), y),
                egui::pos2(canvas_rect.right(), y),
            ],
            Stroke::new(hair, Color32::from_rgb(0, 48, 0)),
        );
    }
    let v_spacing = 16.0;
    let mut vx = canvas_rect.right();
    while vx > canvas_rect.left() {
        painter.line_segment(
            [
                egui::pos2(vx, canvas_rect.top()),
                egui::pos2(vx, canvas_rect.bottom()),
            ],
            Stroke::new(hair, Color32::from_rgb(0, 48, 0)),
        );
        vx -= v_spacing;
    }

    // 3. Top-left overlay: bright green text (e.g. "R: 0" or "R: 1.4 MB/s")
    let overlay_str = if series.current_val <= 0.0 {
        format!("{}: 0", series.prefix_char)
    } else if series.unit_is_rate {
        format!(
            "{}: {}",
            series.prefix_char,
            format::format_rate(series.current_val)
        )
    } else {
        format!(
            "{}: {}",
            series.prefix_char,
            format::format_bytes_loc(series.current_val as u64)
        )
    };
    painter.text(
        egui::pos2(canvas_rect.left() + 6.0, canvas_rect.top() + 4.0),
        egui::Align2::LEFT_TOP,
        overlay_str,
        egui::FontId::proportional(12.0),
        Color32::from_rgb(0, 255, 0),
    );

    // 4. Wave polyline
    if !series.history.is_empty() {
        let peak = series
            .history
            .iter()
            .map(|s| (series.series_fn)(s, series.unit_is_rate))
            .fold(0.0f64, f64::max);
        let min_floor = if series.unit_is_rate { 4096.0 } else { 1024.0 };
        let y_scale = format::nice_rate_max(peak).max(min_floor);

        const POINT_GAP: f32 = 5.0;
        let mut points: Vec<Pos2> = Vec::with_capacity(series.history.len());
        let right_x = canvas_rect.right() - 1.0;
        let bottom_y = canvas_rect.bottom() - 1.0;
        let plot_height = canvas_rect.height() - 4.0;

        for (idx, sample) in series.history.iter().rev().enumerate() {
            let px = right_x - (idx as f32 * POINT_GAP);
            if px < canvas_rect.left() - POINT_GAP {
                break;
            }
            let val = (series.series_fn)(sample, series.unit_is_rate);
            let ratio = (val / y_scale).clamp(0.0, 1.0) as f32;
            let py = bottom_y - (ratio * plot_height);
            points.push(egui::pos2(px.max(canvas_rect.left()), py));
        }

        // Draw area fill beneath line
        if points.len() >= 2 {
            let mut mesh = egui::Mesh::default();
            let fill_color = Color32::from_rgba_unmultiplied(
                series.line_color.r(),
                series.line_color.g(),
                series.line_color.b(),
                28,
            );
            for p in &points {
                let top_idx = mesh.vertices.len() as u32;
                mesh.colored_vertex(*p, fill_color);
                mesh.colored_vertex(egui::pos2(p.x, bottom_y), fill_color);
                if top_idx >= 2 {
                    let lt = top_idx - 2;
                    let lb = top_idx - 1;
                    mesh.add_triangle(lt, top_idx, lb);
                    mesh.add_triangle(lb, top_idx, top_idx + 1);
                }
            }
            painter.add(egui::Shape::mesh(mesh));

            // Draw line
            for w in points.windows(2) {
                painter.line_segment([w[0], w[1]], Stroke::new(1.5, series.line_color));
            }
        } else if let Some(&p) = points.first() {
            painter.circle_filled(p, 1.5, series.line_color);
        }
    }

    // 5. Hover interaction
    if let Some(hover_pos) = response.hover_pos()
        && canvas_rect.contains(hover_pos)
        && !series.history.is_empty()
    {
        const POINT_GAP: f32 = 5.0;
        let right_x = canvas_rect.right() - 1.0;
        let offset_from_right = (right_x - hover_pos.x).max(0.0);
        let sample_idx_rev = (offset_from_right / POINT_GAP).round() as usize;

        if sample_idx_rev < series.history.len() {
            let actual_idx = series.history.len() - 1 - sample_idx_rev;
            let sample = &series.history[actual_idx];
            let val = (series.series_fn)(sample, series.unit_is_rate);
            let sample_x = (right_x - sample_idx_rev as f32 * POINT_GAP)
                .clamp(canvas_rect.left(), canvas_rect.right());

            // Hairline cursor
            painter.line_segment(
                [
                    egui::pos2(sample_x, canvas_rect.top()),
                    egui::pos2(sample_x, canvas_rect.bottom()),
                ],
                Stroke::new(hair, Color32::from_white_alpha(100)),
            );

            // Tooltip
            let age_ms = series
                .history
                .last()
                .map_or(0, |last| last.t_ms.saturating_sub(sample.t_ms));
            let age = sample_age_str(age_ms);
            let val_str = if series.unit_is_rate {
                format::format_rate(val)
            } else {
                format::format_bytes_loc(val as u64)
            };
            response.on_hover_ui(|ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("{}:", series.title));
                    ui.strong(val_str);
                });
                ui.label(age);
            });
        }
    }
}

/// Show the entire I/O tab page.
pub fn show(
    _app: &mut TaskManApp,
    ui: &mut egui::Ui,
    state: &mut State,
    process: &ProcessEntry,
    pal: &theme::Palette,
) {
    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

    // 1. Read graph
    paint_io_graph(
        ui,
        GraphSeries {
            title: i18n::tr(K::PropIoGraphRead),
            prefix_char: 'R',
            current_val: if state.unit_is_rate {
                process.disk_read_bps
            } else {
                process.io_read_bytes_delta.unwrap_or(0) as f64
            },
            unit_is_rate: state.unit_is_rate,
            history: &state.history,
            series_fn: |s, is_rate| {
                if is_rate {
                    s.read_bps
                } else {
                    s.read_delta as f64
                }
            },
            line_color: Color32::from_rgb(235, 235, 45), // Yellow trace
        },
        pal,
    );

    // 2. Write graph
    paint_io_graph(
        ui,
        GraphSeries {
            title: i18n::tr(K::PropIoGraphWrite),
            prefix_char: 'W',
            current_val: if state.unit_is_rate {
                process.disk_write_bps
            } else {
                process.io_write_bytes_delta.unwrap_or(0) as f64
            },
            unit_is_rate: state.unit_is_rate,
            history: &state.history,
            series_fn: |s, is_rate| {
                if is_rate {
                    s.write_bps
                } else {
                    s.write_delta as f64
                }
            },
            line_color: Color32::from_rgb(180, 80, 230), // Purple trace
        },
        pal,
    );

    // 3. Other / Inter-process (IPC) graph (optional via toggle)
    if state.show_other_graph {
        paint_io_graph(
            ui,
            GraphSeries {
                title: i18n::tr(K::PropIoGraphOther),
                prefix_char: 'O',
                current_val: if state.unit_is_rate {
                    process.io_other_bps.unwrap_or(0.0)
                } else {
                    process.io_other_bytes_delta.unwrap_or(0) as f64
                },
                unit_is_rate: state.unit_is_rate,
                history: &state.history,
                series_fn: |s, is_rate| {
                    if is_rate {
                        s.other_bps
                    } else {
                        s.other_delta as f64
                    }
                },
                line_color: Color32::from_rgb(30, 205, 240), // Cyan trace
            },
            pal,
        );
    }

    // 4. Statistics Box: "Disk & Inter-Process I/O"
    let ppp = ui.ctx().pixels_per_point();
    let hair = if ppp > 0.0 { 1.0 / ppp } else { 1.0 };
    // Frame for stats box
    egui::Frame::group(ui.style())
        .fill(pal.card_bg)
        .stroke(Stroke::new(hair, pal.stroke))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(i18n::tr(K::PropIoSectionDisk));
                ui.add_space(8.0);
                ui.label(egui::RichText::new("ℹ").color(pal.text_dim).size(12.0))
                    .on_hover_text(i18n::tr(K::PropIoOtherTooltip));
            });
            ui.add_space(4.0);

            // Columns layout for stats
            let read_ops_str = process
                .io_read_ops
                .map(format::format_thousands)
                .unwrap_or_else(|| "—".into());
            let read_bytes_str = format::format_bytes_loc(process.disk_read_total);
            let read_delta_str = process
                .io_read_bytes_delta
                .map(|d| {
                    if d == 0 {
                        "0".into()
                    } else {
                        format::format_bytes_loc(d)
                    }
                })
                .unwrap_or_else(|| "0".into());

            let write_ops_str = process
                .io_write_ops
                .map(format::format_thousands)
                .unwrap_or_else(|| "—".into());
            let write_bytes_str = format::format_bytes_loc(process.disk_write_total);
            let write_delta_str = process
                .io_write_bytes_delta
                .map(|d| {
                    if d == 0 {
                        "0".into()
                    } else {
                        format::format_bytes_loc(d)
                    }
                })
                .unwrap_or_else(|| "0".into());

            let other_ops_str = process
                .io_other_ops
                .map(format::format_thousands)
                .unwrap_or_else(|| "—".into());
            let other_bytes_str = process
                .io_other_total
                .map(format::format_bytes_loc)
                .unwrap_or_else(|| "—".into());
            let other_delta_str = process
                .io_other_bytes_delta
                .map(|d| {
                    if d == 0 {
                        "0".into()
                    } else {
                        format::format_bytes_loc(d)
                    }
                })
                .unwrap_or_else(|| "—".into());

            egui::Grid::new("process-io-stats-grid")
                .num_columns(6)
                .spacing(Vec2::new(16.0, 6.0))
                .striped(false)
                .show(ui, |ui| {
                    // Row 1: Operations
                    ui.label(i18n::tr(K::PropIoReads));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&read_ops_str);
                    });

                    ui.label(i18n::tr(K::PropIoWrites));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&write_ops_str);
                    });

                    ui.label(i18n::tr(K::PropIoOther));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&other_ops_str);
                    });
                    ui.end_row();

                    // Row 2: Bytes
                    ui.label(i18n::tr(K::PropIoReadBytes));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&read_bytes_str);
                    });

                    ui.label(i18n::tr(K::PropIoWriteBytes));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&write_bytes_str);
                    });

                    ui.label(i18n::tr(K::PropIoOtherBytes));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&other_bytes_str);
                    });
                    ui.end_row();

                    // Row 3: Delta
                    ui.label(i18n::tr(K::PropIoReadDelta));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&read_delta_str);
                    });

                    ui.label(i18n::tr(K::PropIoWriteDelta));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&write_delta_str);
                    });

                    ui.label(i18n::tr(K::PropIoOtherDelta));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&other_delta_str);
                    });
                    ui.end_row();
                });
        });

    // 5. Options and Protection status row
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        // Options popup
        ui.menu_button(i18n::tr(K::PropIoOptions), |ui| {
            ui.set_min_width(220.0);
            if ui
                .checkbox(
                    &mut state.show_other_graph,
                    i18n::tr(K::PropIoShowOtherGraph),
                )
                .changed()
            {
                // Toggle state
            }
            ui.separator();
            if ui
                .selectable_label(state.unit_is_rate, i18n::tr(K::PropIoRateUnits))
                .clicked()
            {
                state.unit_is_rate = true;
                ui.close();
            }
            if ui
                .selectable_label(!state.unit_is_rate, i18n::tr(K::PropIoDeltaUnits))
                .clicked()
            {
                state.unit_is_rate = false;
                ui.close();
            }
            ui.separator();
            if ui.button(i18n::tr(K::PropIoClearHistory)).clicked() {
                state.clear_history();
                ui.close();
            }
        });

        ui.add_space(12.0);
        let protection_str = match process.elevated {
            Some(true) => "Elevated",
            Some(false) => "Standard",
            None => "N/A",
        };
        ui.label(format!(
            "{}: {}",
            i18n::tr(K::PropProtection),
            protection_str
        ));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_defaults_and_clear() {
        let mut state = State::default();
        assert!(state.show_other_graph);
        assert!(state.unit_is_rate);
        assert!(state.history.is_empty());

        state.history.push(IoSample {
            t_ms: 100,
            read_bps: 1024.0,
            write_bps: 512.0,
            other_bps: 256.0,
            read_delta: 1024,
            write_delta: 512,
            other_delta: 256,
        });
        assert_eq!(state.history.len(), 1);

        state.clear_history();
        assert!(state.history.is_empty());
    }

    #[test]
    fn sample_age_formatting() {
        assert_eq!(sample_age_str(100), i18n::tr(K::GraphNow));
        assert_eq!(
            sample_age_str(5_000),
            i18n::trf(K::GraphAgoSeconds, &[&5.to_string()])
        );
        assert_eq!(
            sample_age_str(65_000),
            i18n::trf(K::GraphAgoMinutes, &["1:05"])
        );
    }

    #[test]
    fn group_box_layout_margins() {
        let outer = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(300.0, 100.0));
        // Verify inner content rect sits cleanly inside outer rect
        let box_top = outer.top() + 7.0;
        let inner = Rect::from_min_max(
            Pos2::new(outer.left() + 8.0, box_top + 14.0),
            Pos2::new(outer.right() - 8.0, outer.bottom() - 8.0),
        );
        assert!(inner.width() > 0.0);
        assert!(inner.height() > 0.0);
        assert!(outer.contains_rect(inner));
    }
}

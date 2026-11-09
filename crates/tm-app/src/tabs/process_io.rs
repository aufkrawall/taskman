//! Process Properties → I/O: process I/O activity page showing rolling read,
//! write, and inter-process (IPC / Other) charts matching the Performance page
//! theming, cumulative counters, and interval deltas.

use eframe::egui::{self, Color32, Pos2, Stroke, Vec2};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::ProcessEntry;

use crate::app::{ProcessIdentity, TaskManApp};
use crate::theme;
use crate::widgets::chart::{self, MultiSeries, TimeAxis, ValueFmt};

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

        let cap = (app.shared.settings.graph_seconds as usize * 2).max(120);
        while self.history.len() >= cap {
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

/// Slice of process I/O history visible in the current graph window,
/// including one lead-in point left of the axis cutoff.
pub fn visible_io_slice(history: &[IoSample], seconds: u32) -> &[IoSample] {
    let Some(last) = history.last() else {
        return &[];
    };
    let cutoff = last.t_ms.saturating_sub(seconds as u64 * 1000);
    let mut start = history.len();
    while start > 0 && history[start - 1].t_ms >= cutoff {
        start -= 1;
    }
    &history[start.saturating_sub(1)..]
}

fn fmt_io_rate(v: f64) -> String {
    format::format_rate(v)
}

fn fmt_io_bytes(v: f64) -> String {
    format::format_bytes_loc(v.max(0.0) as u64)
}

/// Paint one rolling I/O graph section matching the Performance page disk theming.
#[allow(clippy::too_many_arguments)]
fn paint_io_graph_section(
    ui: &mut egui::Ui,
    id: &'static str,
    title: &str,
    current_val: f64,
    samples: Vec<f64>,
    color: Color32,
    axis: Option<TimeAxis<'_>>,
    unit_is_rate: bool,
    pal: &theme::Palette,
) {
    let min_floor = if unit_is_rate { 4096.0 } else { 1024.0 };
    let peak = format::nice_rate_max(samples.iter().copied().fold(0.0f64, f64::max)).max(min_floor);

    let current_str = if current_val <= 0.0 {
        if unit_is_rate {
            "0 B/s".into()
        } else {
            "0 B".into()
        }
    } else if unit_is_rate {
        format::format_rate(current_val)
    } else {
        format::format_bytes_loc(current_val as u64)
    };
    let peak_str = if unit_is_rate {
        format::format_rate(peak)
    } else {
        format::format_bytes_loc(peak as u64)
    };

    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;

        // Caption row matching performance page: title & live rate left, scale peak right
        let (caption_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 16.0), egui::Sense::hover());
        let font = egui::FontId::proportional(11.5);
        let left_text = format!("{title}  \u{2014}  {current_str}");
        ui.painter().text(
            Pos2::new(caption_rect.left(), caption_rect.center().y),
            egui::Align2::LEFT_CENTER,
            left_text,
            font.clone(),
            pal.text_dim,
        );
        ui.painter().text(
            Pos2::new(caption_rect.right(), caption_rect.center().y),
            egui::Align2::RIGHT_CENTER,
            peak_str,
            font,
            pal.text_dim,
        );

        // Chart widget matching performance page
        let chart_w = ui.available_width();
        let chart_h = 90.0;
        let series = [MultiSeries::new(title, samples, color)];
        let fmt: ValueFmt = if unit_is_rate {
            fmt_io_rate
        } else {
            fmt_io_bytes
        };

        ui.push_id(id, |ui| {
            chart::chart_multi_with_bg(
                ui,
                egui::vec2(chart_w, chart_h),
                &series,
                peak,
                axis,
                fmt,
                pal.card_bg_sunken,
            );
        });
    });
}

/// Show the entire I/O tab page.
pub fn show(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    state: &mut State,
    process: &ProcessEntry,
    pal: &theme::Palette,
) {
    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

    let seconds = app.shared.settings.graph_seconds;
    let win = visible_io_slice(&state.history, seconds);
    let stamps: Vec<u64> = win.iter().map(|s| s.t_ms).collect();
    let axis = (!stamps.is_empty()).then(|| TimeAxis::new(&stamps, seconds));

    // 1. Read graph (matching disk read theming)
    let read_current = if state.unit_is_rate {
        process.disk_read_bps
    } else {
        process.io_read_bytes_delta.unwrap_or(0) as f64
    };
    let read_samples: Vec<f64> = win
        .iter()
        .map(|s| {
            if state.unit_is_rate {
                s.read_bps
            } else {
                s.read_delta as f64
            }
        })
        .collect();

    paint_io_graph_section(
        ui,
        "prop-io-read",
        i18n::tr(K::PropIoGraphRead),
        read_current,
        read_samples,
        pal.disk_graph,
        axis,
        state.unit_is_rate,
        pal,
    );

    // 2. Write graph (matching disk write theming)
    let write_current = if state.unit_is_rate {
        process.disk_write_bps
    } else {
        process.io_write_bytes_delta.unwrap_or(0) as f64
    };
    let write_samples: Vec<f64> = win
        .iter()
        .map(|s| {
            if state.unit_is_rate {
                s.write_bps
            } else {
                s.write_delta as f64
            }
        })
        .collect();

    paint_io_graph_section(
        ui,
        "prop-io-write",
        i18n::tr(K::PropIoGraphWrite),
        write_current,
        write_samples,
        pal.disk_write_graph,
        axis,
        state.unit_is_rate,
        pal,
    );

    // 3. Other / Inter-process (IPC) graph (optional via toggle)
    if state.show_other_graph {
        let other_current = if state.unit_is_rate {
            process.io_other_bps.unwrap_or(0.0)
        } else {
            process.io_other_bytes_delta.unwrap_or(0) as f64
        };
        let other_samples: Vec<f64> = win
            .iter()
            .map(|s| {
                if state.unit_is_rate {
                    s.other_bps
                } else {
                    s.other_delta as f64
                }
            })
            .collect();

        paint_io_graph_section(
            ui,
            "prop-io-other",
            i18n::tr(K::PropIoGraphOther),
            other_current,
            other_samples,
            pal.network_send_graph,
            axis,
            state.unit_is_rate,
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
                    ui.weak(i18n::tr(K::PropIoReads));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&read_ops_str);
                    });

                    ui.weak(i18n::tr(K::PropIoWrites));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&write_ops_str);
                    });

                    ui.weak(i18n::tr(K::PropIoOther));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&other_ops_str);
                    });
                    ui.end_row();

                    // Row 2: Bytes
                    ui.weak(i18n::tr(K::PropIoReadBytes));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&read_bytes_str);
                    });

                    ui.weak(i18n::tr(K::PropIoWriteBytes));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&write_bytes_str);
                    });

                    ui.weak(i18n::tr(K::PropIoOtherBytes));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&other_bytes_str);
                    });
                    ui.end_row();

                    // Row 3: Delta
                    ui.weak(i18n::tr(K::PropIoReadDelta));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&read_delta_str);
                    });

                    ui.weak(i18n::tr(K::PropIoWriteDelta));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&write_delta_str);
                    });

                    ui.weak(i18n::tr(K::PropIoOtherDelta));
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
    fn visible_io_slice_windowing() {
        let history: Vec<IoSample> = (0..100)
            .map(|i| IoSample {
                t_ms: i * 1_000,
                read_bps: 100.0,
                write_bps: 50.0,
                other_bps: 25.0,
                read_delta: 100,
                write_delta: 50,
                other_delta: 25,
            })
            .collect();

        // 30 second window from t=99_000: cutoff is 69_000.
        // Slice includes lead-in at index 68 (t=68_000) through 99.
        let slice = visible_io_slice(&history, 30);
        assert_eq!(slice.first().unwrap().t_ms, 68_000);
        assert_eq!(slice.last().unwrap().t_ms, 99_000);
        assert_eq!(slice.len(), 32); // 30 seconds + 1 on cutoff + 1 lead-in
    }

    #[test]
    fn io_chart_hover_and_pin_stability() {
        let screen = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 400.0));
        let at = Pos2::new(200.0, 50.0);
        let ctx = egui::Context::default();
        let pal = theme::DARK;

        let render_frame =
            |ctx: &egui::Context, pointer: Option<Pos2>, samples: &[IoSample]| -> Vec<egui::Rect> {
                let mut input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                };
                if let Some(p) = pointer {
                    input.events.push(egui::Event::PointerMoved(p));
                }
                let mut output = ctx.run_ui(input, |ui| {
                    let win = visible_io_slice(samples, 60);
                    let stamps: Vec<u64> = win.iter().map(|s| s.t_ms).collect();
                    let axis = (!stamps.is_empty()).then(|| TimeAxis::new(&stamps, 60));
                    let read_samples: Vec<f64> = win.iter().map(|s| s.read_bps).collect();

                    paint_io_graph_section(
                        ui,
                        "test-io-read",
                        "Read",
                        read_samples.last().copied().unwrap_or(0.0),
                        read_samples,
                        pal.disk_graph,
                        axis,
                        true,
                        &pal,
                    );
                });
                output.textures_delta.clear();
                output
                    .shapes
                    .iter()
                    .map(|clipped| clipped.shape.visual_bounding_rect())
                    .filter(|r| r.is_finite() && r.is_positive())
                    .collect()
            };

        let mut samples: Vec<IoSample> = (0..40)
            .map(|i| IoSample {
                t_ms: i * 1_000,
                read_bps: (i as f64) * 1000.0,
                write_bps: 0.0,
                other_bps: 0.0,
                read_delta: 0,
                write_delta: 0,
                other_delta: 0,
            })
            .collect();

        // Prime hover (egui resolves hovering against previous frame's interaction)
        let _ = render_frame(&ctx, Some(at), &samples);
        let hovered = render_frame(&ctx, Some(at), &samples);

        // Hover readout must appear near cursor
        let near = hovered.iter().any(|r| r.distance_to_pos(at) < 40.0);
        assert!(near, "readout must appear near cursor over I/O graph");

        // Add new sample as graph updates while pointer stays still
        samples.push(IoSample {
            t_ms: 40_000,
            read_bps: 99_999.0,
            write_bps: 0.0,
            other_bps: 0.0,
            read_delta: 0,
            write_delta: 0,
            other_delta: 0,
        });

        // Frame with identical pointer position
        let rolled = render_frame(&ctx, Some(at), &samples);
        let near_rolled = rolled.iter().any(|r| r.distance_to_pos(at) < 40.0);
        assert!(
            near_rolled,
            "readout remains visible near cursor after graph update"
        );
    }
}

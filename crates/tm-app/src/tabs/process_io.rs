//! Process Properties → I/O: process I/O activity page showing rolling read,
//! write, and inter-process (IPC / Other) charts matching the Performance page
//! theming, cumulative counters, interval deltas, and per-file read/written activity.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Stroke, Vec2};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::ProcessEntry;
use tm_platform::actions::ProcessFileIo;

use crate::app::{ProcessIdentity, TaskManApp};
use crate::tabs::inspector::{self, Poll};
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

/// Column to sort the file I/O list by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileSortColumn {
    File,
    Read,
    Written,
    Total,
    Size,
}

/// One tracked file for the inspected process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackedFileIo {
    pub path: String,
    pub name: String,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub total_bytes: u64,
    pub file_size: Option<u64>,
    pub is_read: bool,
    pub is_write: bool,
    pub open_handles: u32,
}

/// State of the I/O page within the Process Properties dialog.
#[derive(Debug)]
pub struct State {
    history: Vec<IoSample>,
    last_sample_t_ms: u64,
    pub show_other_graph: bool,
    pub unit_is_rate: bool,

    poll: Poll<Vec<ProcessFileIo>>,
    pub files: Vec<TrackedFileIo>,
    pub file_filter: String,
    pub file_sort_column: FileSortColumn,
    pub file_sort_ascending: bool,
    pub selected_path: Option<String>,
    pub file_error: Option<String>,
    pub file_stale: Option<String>,
    pub file_loading: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            history: Vec::with_capacity(128),
            last_sample_t_ms: 0,
            show_other_graph: true,
            unit_is_rate: true,
            poll: Poll::default(),
            files: Vec::new(),
            file_filter: String::new(),
            file_sort_column: FileSortColumn::Written,
            file_sort_ascending: false,
            selected_path: None,
            file_error: None,
            file_stale: None,
            file_loading: false,
        }
    }
}

pub(crate) fn extract_file_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if let Some(pos) = trimmed.rfind(['/', '\\']) {
        trimmed[pos + 1..].to_string()
    } else {
        trimmed.to_string()
    }
}

impl State {
    pub fn tick(
        &mut self,
        app: &TaskManApp,
        ctx: &egui::Context,
        identity: &ProcessIdentity,
        active: bool,
    ) {
        if let Some(snapshot) = app.latest_snapshot()
            && snapshot.timestamp_ms != self.last_sample_t_ms
        {
            self.last_sample_t_ms = snapshot.timestamp_ms;
            if let Some(process) =
                crate::tabs::details::process_properties_target(&snapshot, identity)
            {
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
        }

        // Poll file I/O results
        if let Some(result) = self.poll.take() {
            self.file_loading = false;
            match result {
                Ok(new_files) => {
                    self.file_error = None;
                    self.file_stale = None;
                    self.merge_files(new_files);
                }
                Err(err) => {
                    if self.files.is_empty() {
                        self.file_error = Some(err);
                    } else {
                        self.file_stale = Some(err);
                    }
                }
            }
        }

        // Only enumerate files if the I/O tab is actively viewed and supported
        if !active || !app.actions.capabilities().process_file_io || !app.identity_is_live(identity)
        {
            return;
        }

        let every =
            inspector::page_interval(app.shared.settings.update_speed, Duration::from_secs(1));
        if self.poll.due(Instant::now(), every) {
            let actions = app.actions.clone();
            let pid = identity.pid;
            let start_epoch = identity.start_epoch_s;
            self.file_loading = self.files.is_empty();
            self.poll.start(ctx, "tm-process-file-io", move || {
                actions
                    .list_process_file_io(pid, start_epoch)
                    .map_err(|e| e.to_string())
            });
        }
        if let Some(every) = every {
            ctx.request_repaint_after(every);
        }
    }

    pub fn request_refresh(&mut self) {
        self.poll.request();
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    pub fn merge_files(&mut self, new_files: Vec<ProcessFileIo>) {
        let mut seen = HashSet::new();
        for file in new_files {
            seen.insert(file.path.clone());
            if let Some(existing) = self.files.iter_mut().find(|f| f.path == file.path) {
                existing.open_handles = file.open_handles;
                existing.is_read |= file.is_read;
                existing.is_write |= file.is_write;
                existing.read_bytes = existing.read_bytes.max(file.read_bytes);
                existing.write_bytes = existing.write_bytes.max(file.write_bytes);
                existing.total_bytes = existing.read_bytes.saturating_add(existing.write_bytes);
                if let Some(sz) = file.file_size {
                    existing.file_size = Some(existing.file_size.map_or(sz, |prev| prev.max(sz)));
                }
            } else {
                let name = extract_file_name(&file.path);
                self.files.push(TrackedFileIo {
                    path: file.path,
                    name,
                    read_bytes: file.read_bytes,
                    write_bytes: file.write_bytes,
                    total_bytes: file.total_bytes,
                    file_size: file.file_size,
                    is_read: file.is_read,
                    is_write: file.is_write,
                    open_handles: file.open_handles,
                });
            }
        }
        for file in &mut self.files {
            if !seen.contains(&file.path) {
                file.open_handles = 0;
            }
        }
    }

    pub fn sorted_and_filtered_files(&self) -> Vec<&TrackedFileIo> {
        let filter = self.file_filter.trim().to_lowercase();
        let mut result: Vec<&TrackedFileIo> = self
            .files
            .iter()
            .filter(|f| {
                if filter.is_empty() {
                    true
                } else {
                    f.name.to_lowercase().contains(&filter)
                        || f.path.to_lowercase().contains(&filter)
                }
            })
            .collect();

        let col = self.file_sort_column;
        let asc = self.file_sort_ascending;
        result.sort_by(|a, b| {
            let ord = match col {
                FileSortColumn::Written => a
                    .write_bytes
                    .cmp(&b.write_bytes)
                    .then_with(|| a.read_bytes.cmp(&b.read_bytes))
                    .then_with(|| a.name.cmp(&b.name)),
                FileSortColumn::Read => a
                    .read_bytes
                    .cmp(&b.read_bytes)
                    .then_with(|| a.write_bytes.cmp(&b.write_bytes))
                    .then_with(|| a.name.cmp(&b.name)),
                FileSortColumn::Total => a
                    .total_bytes
                    .cmp(&b.total_bytes)
                    .then_with(|| a.name.cmp(&b.name)),
                FileSortColumn::Size => a
                    .file_size
                    .unwrap_or(0)
                    .cmp(&b.file_size.unwrap_or(0))
                    .then_with(|| a.name.cmp(&b.name)),
                FileSortColumn::File => a
                    .name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.path.cmp(&b.path)),
            };
            if asc { ord } else { ord.reverse() }
        });
        result
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
                .unwrap_or_else(|| "0".into());

            let font = egui::FontId::proportional(13.0);
            let text_w = |s: &str| {
                ui.painter()
                    .layout_no_wrap(s.to_owned(), font.clone(), pal.text)
                    .size()
                    .x
            };
            let read_val_w = text_w(&read_ops_str)
                .max(text_w(&read_bytes_str))
                .max(text_w(&read_delta_str))
                .ceil();
            let write_val_w = text_w(&write_ops_str)
                .max(text_w(&write_bytes_str))
                .max(text_w(&write_delta_str))
                .ceil();
            let other_val_w = text_w(&other_ops_str)
                .max(text_w(&other_bytes_str))
                .max(text_w(&other_delta_str))
                .ceil();

            let show_val = |ui: &mut egui::Ui, text: &str, width: f32| {
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 18.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.label(text);
                    },
                );
            };

            egui::Grid::new("process-io-stats-grid")
                .num_columns(6)
                .spacing(Vec2::new(16.0, 6.0))
                .striped(false)
                .show(ui, |ui| {
                    // Row 1: Operations
                    ui.weak(i18n::tr(K::PropIoReads));
                    show_val(ui, &read_ops_str, read_val_w);

                    ui.weak(i18n::tr(K::PropIoWrites));
                    show_val(ui, &write_ops_str, write_val_w);

                    ui.weak(i18n::tr(K::PropIoOther));
                    show_val(ui, &other_ops_str, other_val_w);
                    ui.end_row();

                    // Row 2: Bytes
                    ui.weak(i18n::tr(K::PropIoReadBytes));
                    show_val(ui, &read_bytes_str, read_val_w);

                    ui.weak(i18n::tr(K::PropIoWriteBytes));
                    show_val(ui, &write_bytes_str, write_val_w);

                    ui.weak(i18n::tr(K::PropIoOtherBytes));
                    show_val(ui, &other_bytes_str, other_val_w);
                    ui.end_row();

                    // Row 3: Delta
                    ui.weak(i18n::tr(K::PropIoReadDelta));
                    show_val(ui, &read_delta_str, read_val_w);

                    ui.weak(i18n::tr(K::PropIoWriteDelta));
                    show_val(ui, &write_delta_str, write_val_w);

                    ui.weak(i18n::tr(K::PropIoOtherDelta));
                    show_val(ui, &other_delta_str, other_val_w);
                    ui.end_row();
                });
        });

    // 5. Options and Protection status row
    ui.add_space(2.0);
    ui.horizontal(|ui| {
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

    // 6. File I/O Section: list of files read from and written to
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);

    // Section header with title, summary totals and search filter
    ui.horizontal(|ui| {
        let title = if state.files.is_empty() {
            i18n::tr(K::PropIoFiles).to_string()
        } else {
            format!("{} ({})", i18n::tr(K::PropIoFiles), state.files.len())
        };
        ui.strong(title);

        let total_read: u64 = state.files.iter().map(|f| f.read_bytes).sum();
        let total_written: u64 = state.files.iter().map(|f| f.write_bytes).sum();
        if total_read > 0 || total_written > 0 {
            ui.add_space(8.0);
            let summary = format!(
                "({}: {}, {}: {})",
                i18n::tr(K::PropIoColRead),
                format::format_bytes_loc(total_read),
                i18n::tr(K::PropIoColWritten),
                format::format_bytes_loc(total_written),
            );
            ui.label(egui::RichText::new(summary).color(pal.text_dim).size(12.0));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !state.file_filter.is_empty() && ui.small_button("✖").clicked() {
                state.file_filter.clear();
            }
            ui.add(
                egui::TextEdit::singleline(&mut state.file_filter)
                    .hint_text(i18n::tr(K::PropIoSearchPlaceholder))
                    .desired_width(180.0),
            );
        });
    });

    if let Some(stale) = state.file_stale.as_deref() {
        ui.add_space(2.0);
        ui.label(egui::RichText::new(stale).small().color(pal.heat_high));
    }

    if state.file_loading {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(i18n::tr(K::GatheringData));
        });
        return;
    }

    if let Some(err) = state.file_error.as_deref() {
        ui.add_space(6.0);
        ui.colored_label(pal.heat_high, err);
        return;
    }

    let display_files = state.sorted_and_filtered_files();
    if display_files.is_empty() {
        ui.add_space(6.0);
        ui.weak(i18n::tr(K::PropIoNoFiles));
        return;
    }

    ui.add_space(4.0);

    let mut sort_change = None;
    let mut selected_clicked = None;
    let selected_path = state.selected_path.clone();

    egui::Grid::new("process-io-files-grid")
        .num_columns(5)
        .spacing(Vec2::new(14.0, 6.0))
        .striped(true)
        .min_col_width(70.0)
        .show(ui, |ui| {
            // Header Row
            let file_arrow = if state.file_sort_column == FileSortColumn::File {
                if state.file_sort_ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            } else {
                ""
            };
            let file_btn = ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("{}{}", i18n::tr(K::PropIoColFile), file_arrow))
                        .strong(),
                )
                .sense(egui::Sense::click()),
            );
            if file_btn
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                sort_change = Some(FileSortColumn::File);
            }

            let read_arrow = if state.file_sort_column == FileSortColumn::Read {
                if state.file_sort_ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            } else {
                ""
            };
            let read_btn = ui
                .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{}{}",
                                i18n::tr(K::PropIoColRead),
                                read_arrow
                            ))
                            .strong(),
                        )
                        .sense(egui::Sense::click()),
                    )
                })
                .inner;
            if read_btn
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                sort_change = Some(FileSortColumn::Read);
            }

            let write_arrow = if state.file_sort_column == FileSortColumn::Written {
                if state.file_sort_ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            } else {
                ""
            };
            let write_btn = ui
                .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{}{}",
                                i18n::tr(K::PropIoColWritten),
                                write_arrow
                            ))
                            .strong(),
                        )
                        .sense(egui::Sense::click()),
                    )
                })
                .inner;
            if write_btn
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                sort_change = Some(FileSortColumn::Written);
            }

            let total_arrow = if state.file_sort_column == FileSortColumn::Total {
                if state.file_sort_ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            } else {
                ""
            };
            let total_btn = ui
                .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{}{}",
                                i18n::tr(K::PropIoColTotal),
                                total_arrow
                            ))
                            .strong(),
                        )
                        .sense(egui::Sense::click()),
                    )
                })
                .inner;
            if total_btn
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                sort_change = Some(FileSortColumn::Total);
            }

            let size_arrow = if state.file_sort_column == FileSortColumn::Size {
                if state.file_sort_ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            } else {
                ""
            };
            let size_btn = ui
                .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{}{}",
                                i18n::tr(K::PropIoColSize),
                                size_arrow
                            ))
                            .strong(),
                        )
                        .sense(egui::Sense::click()),
                    )
                })
                .inner;
            if size_btn
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                sort_change = Some(FileSortColumn::Size);
            }

            ui.end_row();

            // Rows
            for file in display_files {
                let is_selected = selected_path.as_deref() == Some(&file.path);
                let row_resp = ui
                    .horizontal(|ui| {
                        let name_text = if is_selected {
                            egui::RichText::new(&file.name).strong().color(pal.accent)
                        } else {
                            egui::RichText::new(&file.name).strong()
                        };
                        ui.label(name_text);
                        if file.open_handles == 0 {
                            ui.weak("(closed)");
                        } else if file.open_handles > 1 {
                            ui.weak(format!("({} handles)", file.open_handles));
                        }
                    })
                    .response
                    .on_hover_text(&file.path);

                if row_resp.clicked() {
                    selected_clicked = Some(file.path.clone());
                }

                row_resp.context_menu(|ui| {
                    if ui.button(i18n::tr(K::OpenFileLocation)).clicked() {
                        let _ = app.actions.open_file_location(&file.path);
                        ui.close();
                    }
                    if ui.button(i18n::tr(K::CopyPath)).clicked() {
                        ui.ctx().copy_text(file.path.clone());
                        ui.close();
                    }
                    if ui.button(i18n::tr(K::FileProperties)).clicked() {
                        let _ = app.actions.open_properties(&file.path);
                        ui.close();
                    }
                });

                // Read bytes
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let s = if file.read_bytes == 0 && !file.is_read {
                        "—".to_string()
                    } else {
                        format::format_bytes_loc(file.read_bytes)
                    };
                    ui.label(s);
                });

                // Written bytes
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let s = if file.write_bytes == 0 && !file.is_write {
                        "—".to_string()
                    } else {
                        format::format_bytes_loc(file.write_bytes)
                    };
                    ui.label(s);
                });

                // Total bytes
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let s = if file.total_bytes == 0 {
                        "—".to_string()
                    } else {
                        format::format_bytes_loc(file.total_bytes)
                    };
                    ui.label(egui::RichText::new(s).strong());
                });

                // File size
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let s = file
                        .file_size
                        .map_or_else(|| "—".to_string(), format::format_bytes_loc);
                    ui.label(s);
                });

                ui.end_row();
            }
        });

    if let Some(col) = sort_change {
        if state.file_sort_column == col {
            state.file_sort_ascending = !state.file_sort_ascending;
        } else {
            state.file_sort_column = col;
            state.file_sort_ascending = col == FileSortColumn::File;
        }
    }
    if let Some(path) = selected_clicked {
        state.selected_path = Some(path);
    }
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
        assert_eq!(state.file_sort_column, FileSortColumn::Written);
        assert!(!state.file_sort_ascending);

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
    fn extract_file_name_handles_various_paths() {
        assert_eq!(extract_file_name(r"C:\Windows\System32\cmd.exe"), "cmd.exe");
        assert_eq!(extract_file_name(r"/usr/bin/bash"), "bash");
        assert_eq!(extract_file_name(r"C:\dir\sub\"), "sub");
        assert_eq!(extract_file_name("single_name"), "single_name");
    }

    #[test]
    fn file_merge_accumulates_and_preserves_closed_handles() {
        let mut state = State::default();
        let batch1 = vec![
            ProcessFileIo {
                path: r"C:\data\input.txt".to_string(),
                read_bytes: 1000,
                write_bytes: 0,
                total_bytes: 1000,
                file_size: Some(5000),
                is_read: true,
                is_write: false,
                open_handles: 1,
            },
            ProcessFileIo {
                path: r"C:\data\output.log".to_string(),
                read_bytes: 0,
                write_bytes: 500,
                total_bytes: 500,
                file_size: Some(500),
                is_read: false,
                is_write: true,
                open_handles: 1,
            },
        ];
        state.merge_files(batch1);
        assert_eq!(state.files.len(), 2);
        assert_eq!(state.files[0].read_bytes, 1000);
        assert_eq!(state.files[0].open_handles, 1);
        assert_eq!(state.files[1].write_bytes, 500);

        // Batch 2: input.txt closed, output.log grew, new file opened
        let batch2 = vec![
            ProcessFileIo {
                path: r"C:\data\output.log".to_string(),
                read_bytes: 0,
                write_bytes: 1500,
                total_bytes: 1500,
                file_size: Some(1500),
                is_read: false,
                is_write: true,
                open_handles: 1,
            },
            ProcessFileIo {
                path: r"C:\data\config.json".to_string(),
                read_bytes: 200,
                write_bytes: 0,
                total_bytes: 200,
                file_size: Some(200),
                is_read: true,
                is_write: false,
                open_handles: 1,
            },
        ];
        state.merge_files(batch2);
        assert_eq!(state.files.len(), 3);

        let input = state
            .files
            .iter()
            .find(|f| f.path.contains("input.txt"))
            .unwrap();
        assert_eq!(input.read_bytes, 1000);
        assert_eq!(
            input.open_handles, 0,
            "closed file must have 0 open handles"
        );

        let output = state
            .files
            .iter()
            .find(|f| f.path.contains("output.log"))
            .unwrap();
        assert_eq!(output.write_bytes, 1500);
        assert_eq!(output.open_handles, 1);
    }

    #[test]
    fn file_sorting_and_filtering() {
        let mut state = State {
            files: vec![
                TrackedFileIo {
                    path: r"C:\alpha\doc.pdf".to_string(),
                    name: "doc.pdf".to_string(),
                    read_bytes: 5000,
                    write_bytes: 100,
                    total_bytes: 5100,
                    file_size: Some(10000),
                    is_read: true,
                    is_write: false,
                    open_handles: 1,
                },
                TrackedFileIo {
                    path: r"C:\beta\heavy_write.dat".to_string(),
                    name: "heavy_write.dat".to_string(),
                    read_bytes: 100,
                    write_bytes: 9000,
                    total_bytes: 9100,
                    file_size: Some(9000),
                    is_read: false,
                    is_write: true,
                    open_handles: 1,
                },
                TrackedFileIo {
                    path: r"C:\gamma\notes.txt".to_string(),
                    name: "notes.txt".to_string(),
                    read_bytes: 2000,
                    write_bytes: 2000,
                    total_bytes: 4000,
                    file_size: Some(2000),
                    is_read: true,
                    is_write: true,
                    open_handles: 1,
                },
            ],
            ..Default::default()
        };

        // Sort by Written descending (written most first)
        state.file_sort_column = FileSortColumn::Written;
        state.file_sort_ascending = false;
        let sorted = state.sorted_and_filtered_files();
        assert_eq!(sorted[0].name, "heavy_write.dat");
        assert_eq!(sorted[1].name, "notes.txt");
        assert_eq!(sorted[2].name, "doc.pdf");

        // Sort by Read descending (read from most first)
        state.file_sort_column = FileSortColumn::Read;
        state.file_sort_ascending = false;
        let sorted_read = state.sorted_and_filtered_files();
        assert_eq!(sorted_read[0].name, "doc.pdf");
        assert_eq!(sorted_read[1].name, "notes.txt");
        assert_eq!(sorted_read[2].name, "heavy_write.dat");

        // Sort by Total descending
        state.file_sort_column = FileSortColumn::Total;
        state.file_sort_ascending = false;
        let sorted_total = state.sorted_and_filtered_files();
        assert_eq!(sorted_total[0].name, "heavy_write.dat");
        assert_eq!(sorted_total[1].name, "doc.pdf");
        assert_eq!(sorted_total[2].name, "notes.txt");

        // Filter by "write"
        state.file_filter = "write".to_string();
        let filtered = state.sorted_and_filtered_files();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "heavy_write.dat");
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
        assert_eq!(slice.len(), 32);
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

    #[test]
    fn stats_grid_other_column_spacing_is_bounded() {
        let screen = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(900.0, 600.0));
        let ctx = egui::Context::default();
        let mut other_lbl_rect = None;
        let mut other_val_rect = None;

        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let font = egui::FontId::proportional(13.0);
                let read_ops_str = "48.705";
                let write_ops_str = "5.290";
                let other_ops_str = "135.251";
                let text_w = |s: &str| {
                    ui.painter()
                        .layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE)
                        .size()
                        .x
                };
                let read_val_w = text_w(read_ops_str).ceil();
                let write_val_w = text_w(write_ops_str).ceil();
                let other_val_w = text_w(other_ops_str).ceil();

                let show_val = |ui: &mut egui::Ui, text: &str, width: f32| -> egui::Response {
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 18.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| ui.label(text),
                    )
                    .response
                };

                egui::Grid::new("test-stats-grid")
                    .num_columns(6)
                    .spacing(Vec2::new(16.0, 6.0))
                    .show(ui, |ui| {
                        ui.weak("Reads");
                        show_val(ui, read_ops_str, read_val_w);

                        ui.weak("Writes");
                        show_val(ui, write_ops_str, write_val_w);

                        let lbl_resp = ui.weak("Other (IPC)");
                        other_lbl_rect = Some(lbl_resp.rect);
                        let val_resp = show_val(ui, other_ops_str, other_val_w);
                        other_val_rect = Some(val_resp.rect);
                        ui.end_row();
                    });
            });
            output.textures_delta.clear();
        }

        let lbl = other_lbl_rect.unwrap();
        let val = other_val_rect.unwrap();
        let gap = val.left() - lbl.right();
        assert!(
            (gap - 16.0).abs() < 2.0,
            "gap between Other label and value must match grid spacing (16 px), was {gap}"
        );
    }

    #[test]
    fn file_io_grid_renders_without_panics() {
        let screen = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(900.0, 700.0));
        let ctx = egui::Context::default();
        let state = State {
            files: vec![
                TrackedFileIo {
                    path: r"C:\alpha\doc.pdf".to_string(),
                    name: "doc.pdf".to_string(),
                    read_bytes: 5000,
                    write_bytes: 100,
                    total_bytes: 5100,
                    file_size: Some(10000),
                    is_read: true,
                    is_write: false,
                    open_handles: 1,
                },
                TrackedFileIo {
                    path: r"C:\beta\heavy_write.dat".to_string(),
                    name: "heavy_write.dat".to_string(),
                    read_bytes: 100,
                    write_bytes: 9000,
                    total_bytes: 9100,
                    file_size: Some(9000),
                    is_read: false,
                    is_write: true,
                    open_handles: 2,
                },
            ],
            ..Default::default()
        };

        let input = egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        };

        let mut output = ctx.run_ui(input, |ui| {
            let display_files = state.sorted_and_filtered_files();
            assert_eq!(display_files.len(), 2);
            egui::Grid::new("test-file-grid")
                .num_columns(5)
                .striped(true)
                .show(ui, |ui| {
                    ui.label("File");
                    ui.label("Read");
                    ui.label("Written");
                    ui.label("Total");
                    ui.label("Size");
                    ui.end_row();

                    for f in display_files {
                        ui.label(&f.name);
                        ui.label(format::format_bytes_loc(f.read_bytes));
                        ui.label(format::format_bytes_loc(f.write_bytes));
                        ui.label(format::format_bytes_loc(f.total_bytes));
                        ui.label(f.file_size.map_or("—".into(), format::format_bytes_loc));
                        ui.end_row();
                    }
                });
        });
        output.textures_delta.clear();
        assert!(!output.shapes.is_empty());
    }
}

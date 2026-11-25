//! Process Properties → I/O: process I/O activity page showing rolling read,
//! write, and inter-process (IPC / Other) charts matching the Performance page
//! theming, cumulative counters, interval deltas, and per-file read/written activity.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Stroke, Vec2};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::ProcessEntry;
use tm_platform::actions::{FileIoAccounting, FileIoAccountingState, ProcessFileIoSnapshot};

use crate::app::{ProcessIdentity, TaskManApp};
use crate::tabs::inspector::{self, Poll};
use crate::theme;
use crate::widgets::chart::{self, MultiSeries, TimeAxis, ValueFmt};

const MAX_RETAINED_IO_ROWS: usize = 4096;

/// One historical sample of a process's I/O activity.
#[derive(Clone, Copy, Debug)]
pub struct IoSample {
    pub t_ms: u64,
    pub read_bps: Option<f64>,
    pub write_bps: Option<f64>,
    pub other_bps: Option<f64>,
    pub read_delta: Option<u64>,
    pub write_delta: Option<u64>,
    pub other_delta: Option<u64>,
}

/// Column to sort the file I/O list by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileSortColumn {
    File,
    Path,
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
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub offset: Option<u64>,
    pub file_size: Option<u64>,
    pub access_known: bool,
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

    poll: Poll<ProcessFileIoSnapshot>,
    pub files: Vec<TrackedFileIo>,
    pub accounting: Option<FileIoAccounting>,
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
            accounting: None,
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
                    read_bps: Some(process.disk_read_bps),
                    write_bps: Some(process.disk_write_bps),
                    other_bps: process.io_other_bps,
                    read_delta: process.io_read_bytes_delta,
                    write_delta: process.io_write_bytes_delta,
                    other_delta: process.io_other_bytes_delta,
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
                Ok(snapshot) => {
                    self.file_error = None;
                    self.file_stale = None;
                    self.merge_files(snapshot);
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

    #[allow(dead_code)]
    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    pub fn merge_files(&mut self, snapshot: ProcessFileIoSnapshot) {
        let handles_complete = snapshot.accounting.handles_complete;
        self.accounting = Some(snapshot.accounting);
        let mut seen = HashSet::new();
        let mut index_by_path: HashMap<String, usize> = self
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.path.clone(), index))
            .collect();
        for file in snapshot.files {
            seen.insert(file.path.clone());
            if let Some(&index) = index_by_path.get(&file.path) {
                let existing = &mut self.files[index];
                if handles_complete {
                    existing.open_handles = file.open_handles;
                    existing.is_read = file.is_read;
                    existing.is_write = file.is_write;
                    existing.offset = file.offset;
                    existing.access_known = file.access_known;
                } else {
                    // A denied/incomplete enumeration may omit or default
                    // individual handles. Only apply positive facts.
                    existing.open_handles = existing.open_handles.max(file.open_handles);
                    existing.is_read |= file.is_read;
                    existing.is_write |= file.is_write;
                    existing.access_known |= file.access_known;
                    if let Some(offset) = file.offset {
                        existing.offset = Some(
                            existing
                                .offset
                                .map_or(offset, |current| current.max(offset)),
                        );
                    }
                }
                existing.read_bytes = match (existing.read_bytes, file.read_bytes) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                existing.write_bytes = match (existing.write_bytes, file.write_bytes) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                existing.total_bytes = measured_total(existing.read_bytes, existing.write_bytes);
                if handles_complete {
                    existing.file_size = file.file_size;
                } else if let Some(size) = file.file_size {
                    existing.file_size = Some(
                        existing
                            .file_size
                            .map_or(size, |previous| previous.max(size)),
                    );
                }
            } else {
                let name = extract_file_name(&file.path);
                let total_bytes = measured_total(file.read_bytes, file.write_bytes);
                index_by_path.insert(file.path.clone(), self.files.len());
                self.files.push(TrackedFileIo {
                    path: file.path,
                    name,
                    read_bytes: file.read_bytes,
                    write_bytes: file.write_bytes,
                    total_bytes,
                    offset: file.offset,
                    file_size: file.file_size,
                    access_known: file.access_known,
                    is_read: file.is_read,
                    is_write: file.is_write,
                    open_handles: file.open_handles,
                });
            }
        }
        if handles_complete {
            for file in &mut self.files {
                if !seen.contains(&file.path) {
                    file.open_handles = 0;
                    file.access_known = true;
                    file.is_read = false;
                    file.is_write = false;
                    file.offset = None;
                }
            }
        }
        let per_file_measured = self
            .accounting
            .as_ref()
            .is_some_and(|accounting| accounting.state == FileIoAccountingState::Measured);
        if !per_file_measured {
            // Without per-file counters, a closed row carries no history worth
            // retaining. Keeping it would let temporary-file churn grow the
            // page without bound.
            self.files.retain(|file| file.open_handles > 0);
        }
        if self.files.len() > MAX_RETAINED_IO_ROWS {
            if let Some(accounting) = self.accounting.as_mut() {
                // The cap is a presentation/retention limit, so the table is
                // intentionally incomplete even when the backend enumeration
                // itself succeeded.
                accounting.handles_complete = false;
            }
            let mut open = Vec::new();
            let mut closed = Vec::new();
            for file in self.files.drain(..) {
                if file.open_handles > 0 {
                    open.push(file);
                } else if per_file_measured {
                    closed.push(file);
                }
            }
            open.truncate(MAX_RETAINED_IO_ROWS);
            let remaining = MAX_RETAINED_IO_ROWS.saturating_sub(open.len());
            closed.truncate(remaining);
            open.extend(closed);
            self.files = open;
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
        result.sort_by(|a, b| match col {
            FileSortColumn::Written => cmp_known_last(a.write_bytes, b.write_bytes, asc)
                .then_with(|| cmp_known_last(a.read_bytes, b.read_bytes, asc))
                .then_with(|| a.name.cmp(&b.name)),
            FileSortColumn::Read => cmp_known_last(a.read_bytes, b.read_bytes, asc)
                .then_with(|| cmp_known_last(a.write_bytes, b.write_bytes, asc))
                .then_with(|| a.name.cmp(&b.name)),
            FileSortColumn::Total => {
                cmp_known_last(a.total_bytes, b.total_bytes, asc).then_with(|| a.name.cmp(&b.name))
            }
            FileSortColumn::Size => {
                cmp_known_last(a.file_size, b.file_size, asc).then_with(|| a.name.cmp(&b.name))
            }
            FileSortColumn::Path => {
                let ord = a
                    .path
                    .to_lowercase()
                    .cmp(&b.path.to_lowercase())
                    .then_with(|| a.name.cmp(&b.name));
                if asc { ord } else { ord.reverse() }
            }
            FileSortColumn::File => {
                let ord = a
                    .name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.path.cmp(&b.path));
                if asc { ord } else { ord.reverse() }
            }
        });
        result
    }
}

fn measured_total(read: Option<u64>, written: Option<u64>) -> Option<u64> {
    match (read, written) {
        (Some(read), Some(written)) => Some(read.saturating_add(written)),
        _ => None,
    }
}

/// One per-file counter summed over every listed file, or `None` when ANY of
/// them is unmeasured.
///
/// Adding up only the rows that happen to report a counter prints a smaller
/// number than the process actually moved, and the page then states it as THE
/// total. The rows already say "—" for what they never measured, so the total
/// has to say the same instead of quietly understating it. A sum that does not
/// fit in `u64` is unknown rather than saturated for the same reason.
fn measured_sum(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let mut total = 0u64;
    let mut measured = false;
    for value in values {
        total = total.checked_add(value?)?;
        measured = true;
    }
    measured.then_some(total)
}

/// Whether raising the token can give the per-file byte columns a source.
///
/// The line explains a privilege gap, so it is only true where that remedy
/// exists. Windows has two hosts for the per-file trace — the installed
/// service and an elevated GUI. The Linux backend reports per-file bytes as
/// [`FileIoAccountingState::Unavailable`] for every snapshot by construction
/// and has no elevated host at all, so the line there would name a remedy that
/// does not exist on a table that is not broken.
const ELEVATION_IS_A_REMEDY: bool = cfg!(windows);

/// What the page says above the file table about per-file byte accounting, or
/// `None` when the table speaks for itself.
///
/// Every state but [`FileIoAccountingState::Measured`] means the Read and
/// Written columns hold "—" for reasons the table cannot show, and the three of
/// them are different answers:
///
/// * `Unavailable` — no source at all. This is the one a user can act on: a
///   per-file trace requires elevation and either the installed service or an
///   elevated process, so without them the columns are a gap in privilege, not
///   in time. `elevation_is_a_remedy` keeps the line off a platform where that
///   remedy does not exist, where it would blame the token for a platform
///   without a trace.
/// * `Undelivered` — a trace is running and has not decoded a completed
///   transfer anywhere yet. A silent window cannot be told apart from a broken
///   subscription, so the page reports which one it is stuck in instead of an
///   unexplained dash.
/// * `Undecodable` — records arrived and none of them parsed: the payload layout
///   is not what this build expects.
/// * `KernelLoggerInUse` — no session could be started because another
///   application owns the single global NT Kernel Logger, the fallback when the
///   private file session is refused. Nothing is running, so this must not
///   read as a silent window: it is the one state here that names an owner the
///   user can close.
///
/// Before, one line covered `Unavailable` only, so a trace that started and
/// delivered nothing looked exactly like a working session on a quiet process.
///
/// `rows_measured` suppresses the line when a row still carries a count.
/// [`State::merge_files`] replaces the accounting wholesale while keeping the
/// larger of two row counts, so a snapshot without a source can arrive over
/// rows measured a moment earlier; the rows are the measurement then, and a "no
/// trace" or "nothing delivered yet" line under them would contradict what the
/// table shows.
///
/// Language is a parameter rather than the process-global active one so the
/// rendered text is pinned by tests in both languages.
fn per_file_accounting_note(
    accounting: Option<&FileIoAccounting>,
    rows_measured: bool,
    elevation_is_a_remedy: bool,
    lang: i18n::Lang,
) -> Option<String> {
    if rows_measured {
        return None;
    }
    let accounting = accounting?;
    let state = match accounting.state {
        FileIoAccountingState::Measured => return None,
        FileIoAccountingState::Unavailable if !elevation_is_a_remedy => return None,
        FileIoAccountingState::Unavailable => i18n::tr_in(lang, K::PropIoPerFileNoTrace),
        FileIoAccountingState::Undelivered => i18n::tr_in(lang, K::PropIoPerFileUndelivered),
        FileIoAccountingState::Undecodable => i18n::tr_in(lang, K::PropIoPerFileUndecodable),
        FileIoAccountingState::KernelLoggerInUse => {
            i18n::tr_in(lang, K::PropIoPerFileKernelLoggerInUse)
        }
    };
    match accounting_losses(accounting, lang) {
        Some(losses) => Some(format!("{state} ({losses})")),
        None => Some(state.to_string()),
    }
}

/// The non-zero loss and cap counters of one answer as one phrase.
///
/// Each of them makes the counts on screen lower bounds: events ETW reports as
/// dropped, paths never tracked because a capacity limit was full, and names
/// discarded from the trace's map (whose later transfers land in the unnamed
/// remainder). A zero or an unqueried `events_lost` is left out rather than
/// printed, so a healthy session keeps a plain line and an unknown loss count
/// stays unknown.
fn accounting_losses(accounting: &FileIoAccounting, lang: i18n::Lang) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(lost) = accounting.events_lost.filter(|lost| *lost > 0) {
        parts.push(format!(
            "{} {}",
            format::format_thousands(lost),
            i18n::tr_in(lang, K::PropIoEventsLost)
        ));
    }
    if accounting.dropped_paths > 0 {
        parts.push(format!(
            "{} {}",
            format::format_thousands(accounting.dropped_paths),
            i18n::tr_in(lang, K::PropIoPathsDropped)
        ));
    }
    if accounting.cleared_file_objects > 0 {
        parts.push(format!(
            "{} {}",
            format::format_thousands(accounting.cleared_file_objects),
            i18n::tr_in(lang, K::PropIoFileNamesCleared)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn cmp_known_last<T: Ord>(a: Option<T>, b: Option<T>, ascending: bool) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) if ascending => a.cmp(&b),
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
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

fn fmt_optional_count(value: Option<u64>) -> String {
    value.map_or_else(|| "\u{2014}".into(), format::format_thousands)
}

fn fmt_optional_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "\u{2014}".into(), format::format_bytes_loc)
}

/// Summary totals beside the file-list title, or `None` while nothing was
/// measured (an empty list, or every row's counters unknown).
///
/// A measured zero is a number and prints "0 B"; the summary disappears only
/// when there is nothing to state, never to hide a zero. A list measured on
/// one side only states that side and "\u{2014}" for the other.
fn files_summary(read: Option<u64>, written: Option<u64>) -> Option<String> {
    if read.is_none() && written.is_none() {
        return None;
    }
    Some(format!(
        "({}: {}, {}: {})",
        i18n::tr(K::PropIoColRead),
        fmt_optional_bytes(read),
        i18n::tr(K::PropIoColWritten),
        fmt_optional_bytes(written),
    ))
}

/// Paint one rolling I/O graph section matching the Performance page disk theming.
#[allow(clippy::too_many_arguments)]
fn paint_io_graph_section(
    ui: &mut egui::Ui,
    id: &'static str,
    title: &str,
    current_val: Option<f64>,
    samples: Vec<Option<f64>>,
    color: Color32,
    axis: Option<TimeAxis<'_>>,
    unit_is_rate: bool,
    pal: &theme::Palette,
) {
    let min_floor = if unit_is_rate { 4096.0 } else { 1024.0 };
    let peak = format::nice_rate_max(
        samples
            .iter()
            .flatten()
            .copied()
            .filter(|value| value.is_finite())
            .fold(0.0f64, f64::max),
    )
    .max(min_floor);

    let current_str = match current_val.filter(|value| value.is_finite()) {
        None => "\u{2014}".into(),
        Some(value) if value <= 0.0 => {
            if unit_is_rate {
                "0 B/s".into()
            } else {
                "0 B".into()
            }
        }
        Some(value) if unit_is_rate => format::format_rate(value),
        Some(value) => format::format_bytes_loc(value as u64),
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
        let series = [MultiSeries::with_gaps(title, samples, color)];
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
        Some(process.disk_read_bps)
    } else {
        process.io_read_bytes_delta.map(|value| value as f64)
    };
    let read_samples: Vec<Option<f64>> = win
        .iter()
        .map(|s| {
            if state.unit_is_rate {
                s.read_bps
            } else {
                s.read_delta.map(|value| value as f64)
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
        Some(process.disk_write_bps)
    } else {
        process.io_write_bytes_delta.map(|value| value as f64)
    };
    let write_samples: Vec<Option<f64>> = win
        .iter()
        .map(|s| {
            if state.unit_is_rate {
                s.write_bps
            } else {
                s.write_delta.map(|value| value as f64)
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
            process.io_other_bps
        } else {
            process.io_other_bytes_delta.map(|value| value as f64)
        };
        let other_samples: Vec<Option<f64>> = win
            .iter()
            .map(|s| {
                if state.unit_is_rate {
                    s.other_bps
                } else {
                    s.other_delta.map(|value| value as f64)
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
            let read_ops_str = fmt_optional_count(process.io_read_ops);
            let read_bytes_str = format::format_bytes_loc(process.disk_read_total);
            let read_delta_str = fmt_optional_bytes(process.io_read_bytes_delta);

            let write_ops_str = fmt_optional_count(process.io_write_ops);
            let write_bytes_str = format::format_bytes_loc(process.disk_write_total);
            let write_delta_str = fmt_optional_bytes(process.io_write_bytes_delta);

            let other_ops_str = fmt_optional_count(process.io_other_ops);
            let other_bytes_str = fmt_optional_bytes(process.io_other_total);
            let other_delta_str = fmt_optional_bytes(process.io_other_bytes_delta);

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

    // 5. File I/O Section: list of files read from and written to
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(4.0);

    // Section header with title, summary totals and search filter
    ui.horizontal(|ui| {
        let open_count = state.files.iter().filter(|f| f.open_handles > 0).count();
        let title = if state.files.is_empty() {
            i18n::tr(K::PropIoFiles).to_string()
        } else {
            format!("{} ({} open)", i18n::tr(K::PropIoFiles), open_count)
        };
        ui.strong(title)
            .on_hover_text(i18n::tr(K::PropIoFilesTooltip));

        let total_read = measured_sum(state.files.iter().map(|f| f.read_bytes));
        let total_written = measured_sum(state.files.iter().map(|f| f.write_bytes));
        if let Some(summary) = files_summary(total_read, total_written) {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(summary).color(pal.text_dim).size(12.0));
        }

        ui.add_space(16.0);
        ui.add(
            egui::TextEdit::singleline(&mut state.file_filter)
                .hint_text(i18n::tr(K::PropIoSearchPlaceholder))
                .desired_width(160.0),
        );
        if !state.file_filter.is_empty() && ui.small_button("✖").clicked() {
            state.file_filter.clear();
        }
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
    let handles_incomplete = state
        .accounting
        .as_ref()
        .is_some_and(|accounting| !accounting.handles_complete);
    if handles_incomplete && !display_files.is_empty() {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(i18n::tr(K::PropIoFilesUnavailable))
                .small()
                .color(pal.text_dim),
        );
    }
    if display_files.is_empty() {
        ui.add_space(6.0);
        // Per-file byte/op accounting is normally Unavailable, but the file
        // LIST is still a measured fact when handle enumeration completed.
        let unavailable = handles_incomplete || state.accounting.is_none();
        let msg = if unavailable {
            i18n::tr(K::PropIoFilesUnavailable)
        } else if !state.file_filter.trim().is_empty() {
            i18n::tr(K::NoMatches)
        } else {
            i18n::tr(K::PropIoNoFiles)
        };
        ui.weak(msg);
        if process.disk_read_bps > 0.0
            || process.disk_write_bps > 0.0
            || process.io_other_bps.unwrap_or(0.0) > 0.0
        {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(i18n::tr(K::PropIoNonFileNote))
                    .small()
                    .color(pal.text_dim),
            );
        }
        return;
    }

    // Above the table, so the Read and Written columns are explained rather
    // than left to look broken: without a trace there is nothing to show in
    // them for any row, and a trace that has delivered or decoded nothing says
    // which of the two it is.
    let rows_measured = display_files
        .iter()
        .any(|file| file.read_bytes.is_some() || file.write_bytes.is_some());
    if let Some(note) = per_file_accounting_note(
        state.accounting.as_ref(),
        rows_measured,
        ELEVATION_IS_A_REMEDY,
        i18n::lang(),
    ) {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(note).small().color(pal.text_dim));
    }

    ui.add_space(4.0);

    const COL_FILE_MIN_W: f32 = 160.0;
    const COL_PATH_MIN_W: f32 = 260.0;
    const COL_READ_W: f32 = 85.0;
    const COL_WRITE_W: f32 = 95.0;
    const COL_TOTAL_W: f32 = 105.0;
    const COL_SIZE_W: f32 = 90.0;

    let mut sort_change = None;
    let mut selected_clicked = None;
    let selected_path = state.selected_path.clone();

    egui::Grid::new("process-io-files-grid")
        .num_columns(6)
        .spacing(Vec2::new(16.0, 6.0))
        .striped(true)
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
            let file_btn = ui
                .allocate_ui_with_layout(
                    egui::vec2(COL_FILE_MIN_W, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_min_size(egui::vec2(COL_FILE_MIN_W, 18.0));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "{}{}",
                                    i18n::tr(K::PropIoColFile),
                                    file_arrow
                                ))
                                .font(crate::theme::font_semibold_in(ui, 13.0)),
                            )
                            .sense(egui::Sense::click()),
                        )
                    },
                )
                .inner;
            if file_btn
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                sort_change = Some(FileSortColumn::File);
            }

            let path_arrow = if state.file_sort_column == FileSortColumn::Path {
                if state.file_sort_ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            } else {
                ""
            };
            let path_btn = ui
                .allocate_ui_with_layout(
                    egui::vec2(COL_PATH_MIN_W, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_min_size(egui::vec2(COL_PATH_MIN_W, 18.0));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "{}{}",
                                    i18n::tr(K::ColPath),
                                    path_arrow
                                ))
                                .font(crate::theme::font_semibold_in(ui, 13.0)),
                            )
                            .sense(egui::Sense::click()),
                        )
                    },
                )
                .inner;
            if path_btn
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                sort_change = Some(FileSortColumn::Path);
            }

            let mut show_num_header =
                |ui: &mut egui::Ui, label: &str, col: FileSortColumn, width: f32| {
                    let arrow = if state.file_sort_column == col {
                        if state.file_sort_ascending {
                            " ▲"
                        } else {
                            " ▼"
                        }
                    } else {
                        ""
                    };
                    let text = format!("{label}{arrow}");
                    let resp = ui
                        .allocate_ui_with_layout(
                            egui::vec2(width, 18.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.set_min_size(egui::vec2(width, 18.0));
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(text)
                                            .font(crate::theme::font_semibold_in(ui, 13.0)),
                                    )
                                    .sense(egui::Sense::click()),
                                )
                            },
                        )
                        .inner;
                    if resp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        sort_change = Some(col);
                    }
                };

            show_num_header(
                ui,
                i18n::tr(K::PropIoColRead),
                FileSortColumn::Read,
                COL_READ_W,
            );
            show_num_header(
                ui,
                i18n::tr(K::PropIoColWritten),
                FileSortColumn::Written,
                COL_WRITE_W,
            );
            show_num_header(
                ui,
                i18n::tr(K::PropIoColTotal),
                FileSortColumn::Total,
                COL_TOTAL_W,
            );
            show_num_header(
                ui,
                i18n::tr(K::PropIoColSize),
                FileSortColumn::Size,
                COL_SIZE_W,
            );

            ui.end_row();

            // Rows
            for file in display_files {
                let is_selected = selected_path.as_deref() == Some(&file.path);

                let hover_info = format!(
                    "{}\nAccess: {}{}\nPosition: {}\nSize: {}",
                    file.path,
                    if !file.access_known {
                        "Unknown"
                    } else if file.is_read && file.is_write {
                        "Read / Write"
                    } else if file.is_write {
                        "Write"
                    } else if file.is_read {
                        "Read"
                    } else {
                        "None"
                    },
                    if file.open_handles == 0 {
                        " (closed)"
                    } else {
                        ""
                    },
                    file.offset
                        .map_or("—".to_string(), format::format_bytes_loc),
                    file.file_size
                        .map_or("—".to_string(), format::format_bytes_loc),
                );

                // File column
                let name_cell = ui.allocate_ui_with_layout(
                    egui::vec2(COL_FILE_MIN_W, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_min_size(egui::vec2(COL_FILE_MIN_W, 18.0));
                        let name_text = if is_selected {
                            egui::RichText::new(&file.name)
                                .font(crate::theme::font_semibold_in(ui, 13.0))
                                .color(pal.accent)
                        } else {
                            egui::RichText::new(&file.name)
                                .font(crate::theme::font_semibold_in(ui, 13.0))
                        };
                        let l_resp =
                            ui.add(egui::Label::new(name_text).selectable(true).truncate());
                        if file.open_handles == 0 {
                            ui.weak("(closed)");
                        } else if file.open_handles > 1 {
                            ui.weak(format!("({} handles)", file.open_handles));
                        }
                        l_resp
                    },
                );
                let name_label_resp = name_cell.inner;
                let name_cell_resp = name_cell.response.interact(egui::Sense::click());
                if name_cell_resp.clicked()
                    || name_label_resp.clicked()
                    || name_cell_resp.secondary_clicked()
                    || name_label_resp.secondary_clicked()
                {
                    selected_clicked = Some(file.path.clone());
                }
                name_cell_resp.clone().on_hover_text(&hover_info);
                name_label_resp.clone().on_hover_text(&hover_info);
                file_entry_cell_context_menu(
                    app,
                    &name_cell_resp,
                    &name_label_resp,
                    &file.name,
                    &file.path,
                );
                file_entry_cell_context_menu(
                    app,
                    &name_label_resp,
                    &name_label_resp,
                    &file.name,
                    &file.path,
                );

                // Path column
                let path_cell = ui.allocate_ui_with_layout(
                    egui::vec2(COL_PATH_MIN_W, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_min_size(egui::vec2(COL_PATH_MIN_W, 18.0));
                        let path_text = if is_selected {
                            egui::RichText::new(&file.path).color(pal.accent)
                        } else {
                            egui::RichText::new(&file.path)
                        };
                        ui.add(egui::Label::new(path_text).selectable(true).truncate())
                    },
                );
                let path_label_resp = path_cell.inner;
                let path_cell_resp = path_cell.response.interact(egui::Sense::click());
                if path_cell_resp.clicked()
                    || path_label_resp.clicked()
                    || path_cell_resp.secondary_clicked()
                    || path_label_resp.secondary_clicked()
                {
                    selected_clicked = Some(file.path.clone());
                }
                path_cell_resp.clone().on_hover_text(&hover_info);
                path_label_resp.clone().on_hover_text(&hover_info);
                file_entry_cell_context_menu(
                    app,
                    &path_cell_resp,
                    &path_label_resp,
                    &file.path,
                    &file.path,
                );
                file_entry_cell_context_menu(
                    app,
                    &path_label_resp,
                    &path_label_resp,
                    &file.path,
                    &file.path,
                );

                // Numeric columns helper
                let mut show_num_cell =
                    |ui: &mut egui::Ui, text: egui::RichText, raw_str: &str, width: f32| {
                        let num_cell = ui.allocate_ui_with_layout(
                            egui::vec2(width, 18.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.set_min_size(egui::vec2(width, 18.0));
                                ui.add(egui::Label::new(text).selectable(true))
                            },
                        );
                        let num_label_resp = num_cell.inner;
                        let num_cell_resp = num_cell.response.interact(egui::Sense::click());
                        if num_cell_resp.clicked()
                            || num_label_resp.clicked()
                            || num_cell_resp.secondary_clicked()
                            || num_label_resp.secondary_clicked()
                        {
                            selected_clicked = Some(file.path.clone());
                        }
                        num_cell_resp.clone().on_hover_text(&hover_info);
                        num_label_resp.clone().on_hover_text(&hover_info);
                        file_entry_cell_context_menu(
                            app,
                            &num_cell_resp,
                            &num_label_resp,
                            raw_str,
                            &file.path,
                        );
                        file_entry_cell_context_menu(
                            app,
                            &num_label_resp,
                            &num_label_resp,
                            raw_str,
                            &file.path,
                        );
                    };

                // Read bytes
                let s = fmt_optional_bytes(file.read_bytes);
                show_num_cell(ui, egui::RichText::new(&s), &s, COL_READ_W);

                // Written bytes
                let s = fmt_optional_bytes(file.write_bytes);
                show_num_cell(ui, egui::RichText::new(&s), &s, COL_WRITE_W);

                // Total bytes
                let s = fmt_optional_bytes(file.total_bytes);
                show_num_cell(
                    ui,
                    egui::RichText::new(&s).font(crate::theme::font_semibold_in(ui, 13.0)),
                    &s,
                    COL_TOTAL_W,
                );

                // File size
                let s = file
                    .file_size
                    .map_or_else(|| "—".to_string(), format::format_bytes_loc);
                show_num_cell(ui, egui::RichText::new(&s), &s, COL_SIZE_W);

                ui.end_row();
            }
        });

    if let Some(col) = sort_change {
        if state.file_sort_column == col {
            state.file_sort_ascending = !state.file_sort_ascending;
        } else {
            state.file_sort_column = col;
            state.file_sort_ascending = col == FileSortColumn::File || col == FileSortColumn::Path;
        }
    }
    if let Some(path) = selected_clicked {
        state.selected_path = Some(path);
    }
}

fn file_entry_cell_context_menu(
    app: &mut TaskManApp,
    resp: &egui::Response,
    label_resp: &egui::Response,
    fallback_text: &str,
    file_path: &str,
) {
    // The item only reports the pick: dispatching it needs `app` mutably,
    // which the toast callback below has borrowed for the menu's lifetime.
    let reveal: std::cell::Cell<Option<String>> = std::cell::Cell::new(None);
    file_entry_cell_context_menu_actions(
        &|msg| app.shared.toast(msg),
        &|path| reveal.set(Some(path.to_owned())),
        resp,
        label_resp,
        fallback_text,
        file_path,
    );
    if let Some(path) = reveal.take() {
        let actions = app.actions.clone();
        app.run_shell_action(&resp.ctx, move || actions.open_file_location(&path));
    }
}

fn file_entry_cell_context_menu_actions(
    toast: &dyn Fn(&str),
    reveal_in_explorer: &dyn Fn(&str),
    resp: &egui::Response,
    label_resp: &egui::Response,
    fallback_text: &str,
    file_path: &str,
) {
    crate::widgets::menu::context_menu(resp, |ui| {
        let active_sel = egui::text_selection::LabelSelectionState::active_selection(ui.ctx());
        if crate::widgets::menu::item(ui, i18n::tr(K::Copy)).clicked() {
            if let Some(sel) = active_sel {
                ui.ctx().copy_text(sel);
            } else {
                ui.ctx().copy_text(fallback_text.to_owned());
            }
            toast(i18n::tr(K::Copied));
            ui.close();
        }
        if crate::widgets::menu::item(ui, i18n::tr(K::SelectAll)).clicked() {
            egui::text_selection::LabelSelectionState::select_all_in_widget(
                ui.ctx(),
                label_resp.layer_id,
                label_resp.id,
            );
            ui.close();
        }
        let has_path = !file_path.trim().is_empty();
        if crate::widgets::menu::item_enabled(ui, i18n::tr(K::OpenFileLocation), has_path).clicked()
        {
            reveal_in_explorer(file_path);
            ui.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_platform::actions::ProcessFileIo;

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
            read_bps: Some(1024.0),
            write_bps: Some(512.0),
            other_bps: Some(256.0),
            read_delta: Some(1024),
            write_delta: Some(512),
            other_delta: Some(256),
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

    /// The header total covers the whole list or states "—".
    ///
    /// Regression: it summed `filter_map(read_bytes)`, so a list in which one
    /// file reported its counters and the next did not printed the first
    /// file's traffic as the page total — a smaller number than the process
    /// moved, presented as the measured one. A measured zero is still a number.
    #[test]
    fn the_file_totals_are_unavailable_rather_than_partial() {
        use std::iter;
        assert_eq!(measured_sum(iter::empty()), None);
        assert_eq!(measured_sum([Some(0), Some(0)].into_iter()), Some(0));
        assert_eq!(measured_sum([Some(1000), Some(24)].into_iter()), Some(1024));
        assert_eq!(
            measured_sum([Some(1000), None, Some(24)].into_iter()),
            None,
            "one unknown counter makes the whole total unknown"
        );
        assert_eq!(
            measured_sum([Some(u64::MAX), Some(1)].into_iter()),
            None,
            "an overflowing sum is unknown, never saturated"
        );
    }

    /// The summary beside the file-list title renders only when at least one
    /// total was measured: a measured zero prints "0 B", a list with nothing
    /// measured prints no summary at all (the list body states the
    /// unavailable form).
    ///
    /// Pins both ends of that condition. The rule used to be `total > 0`,
    /// which hid a measured zero; it is now "any total known", which shows
    /// it — a visible change that has to stay deliberate.
    #[test]
    fn the_file_summary_states_measured_totals_and_skips_an_unmeasured_list() {
        use std::iter;

        // An empty list and an all-unknown list have nothing to state.
        assert_eq!(files_summary(measured_sum(iter::empty()), None), None);
        assert_eq!(files_summary(None, None), None);

        // Measured zeros are numbers, so both totals print.
        let zero_bytes = fmt_optional_bytes(Some(0));
        let zero = files_summary(Some(0), Some(0)).expect("a measured zero states its totals");
        assert_eq!(zero.matches(zero_bytes.as_str()).count(), 2);
        assert!(!zero.contains('\u{2014}'));

        // One side unknown still states the other, and marks the gap.
        let read_bytes = fmt_optional_bytes(Some(1024));
        let half = files_summary(Some(1024), None).expect("a measured read total is stated");
        assert!(half.contains(read_bytes.as_str()));
        assert!(half.contains('\u{2014}'));
    }

    /// Every accounting state the file table can be in renders one exact line,
    /// and only `Measured` renders none.
    ///
    /// Regression: one line covered `Unavailable` and the other two states got
    /// nothing at all, so a trace that started and delivered nothing printed
    /// exactly what a working session on a quiet process prints — a column of
    /// "—" with no explanation. Text is pinned literally, not against the
    /// translation table, so a reworded key or a broken composition fails here.
    #[test]
    fn the_file_table_renders_one_line_per_accounting_state() {
        use tm_core::i18n::Lang;

        let plain = |state| FileIoAccounting {
            state,
            ..Default::default()
        };
        let note = |state| per_file_accounting_note(Some(&plain(state)), false, true, Lang::En);

        let cases = [
            (
                FileIoAccountingState::Unavailable,
                Some("no trace is running: needs administrator rights or the installed service"),
            ),
            (
                FileIoAccountingState::Undelivered,
                Some("a trace is running but has not delivered a file event yet"),
            ),
            (
                FileIoAccountingState::Undecodable,
                Some("file events arrived but could not be decoded"),
            ),
            (
                FileIoAccountingState::KernelLoggerInUse,
                Some(
                    "another application is using the NT Kernel Logger, \
                     so per-file I/O cannot be traced",
                ),
            ),
            (FileIoAccountingState::Measured, None),
        ];
        for (state, expected) in cases {
            assert_eq!(note(state).as_deref(), expected, "{state:?}");
        }

        // The German wording is pinned once, with a counter, because the line is
        // the only place a capped session is visible in that language too.
        let capped = FileIoAccounting {
            state: FileIoAccountingState::Unavailable,
            events_lost: Some(12),
            ..Default::default()
        };
        assert_eq!(
            per_file_accounting_note(Some(&capped), false, true, Lang::De).as_deref(),
            Some(
                "Es läuft keine Ablaufverfolgung: erfordert Administratorrechte oder den \
                 installierten Dienst (12 Ereignisse verloren)"
            )
        );
    }

    /// The loss and cap counters ride on the same line, and only when they are
    /// non-zero: a healthy session keeps a plain line, an unqueried loss count
    /// stays unknown rather than becoming a zero.
    #[test]
    fn the_file_table_appends_losses_and_caps_to_the_state() {
        use tm_core::i18n::Lang;

        let undelivered = FileIoAccounting {
            state: FileIoAccountingState::Undelivered,
            ..Default::default()
        };
        let line = |accounting: &FileIoAccounting| {
            per_file_accounting_note(Some(accounting), false, true, Lang::En)
        };
        assert_eq!(
            line(&undelivered).as_deref(),
            Some("a trace is running but has not delivered a file event yet")
        );

        // One counter at a time, then all three in a fixed order.
        let mut lost = undelivered.clone();
        lost.events_lost = Some(12);
        assert_eq!(
            line(&lost).as_deref(),
            Some("a trace is running but has not delivered a file event yet (12 events lost)")
        );

        let mut dropped = undelivered.clone();
        dropped.dropped_paths = 3;
        assert_eq!(
            line(&dropped).as_deref(),
            Some("a trace is running but has not delivered a file event yet (3 paths dropped)")
        );

        let mut cleared = undelivered.clone();
        cleared.cleared_file_objects = 5;
        assert_eq!(
            line(&cleared).as_deref(),
            Some(
                "a trace is running but has not delivered a file event yet (5 file names cleared)"
            )
        );

        let mut all = undelivered.clone();
        all.events_lost = Some(12);
        all.dropped_paths = 3;
        all.cleared_file_objects = 5;
        assert_eq!(
            line(&all).as_deref(),
            Some(
                "a trace is running but has not delivered a file event yet \
                 (12 events lost, 3 paths dropped, 5 file names cleared)"
            )
        );

        // Zeros are not losses, and a counter that could not be queried is not a
        // zero: neither may print.
        let mut healthy = undelivered.clone();
        healthy.events_lost = Some(0);
        assert_eq!(line(&healthy), line(&undelivered));

        // `Measured` carries no line at all, counters or not.
        let measured_with_losses = FileIoAccounting {
            state: FileIoAccountingState::Measured,
            events_lost: Some(12),
            dropped_paths: 3,
            cleared_file_objects: 5,
            ..Default::default()
        };
        assert_eq!(line(&measured_with_losses), None);
    }

    /// A line is suppressed by a measured row, and by a platform on which
    /// elevation is not the remedy — but only for `Unavailable`, which is the
    /// state that names it.
    #[test]
    fn a_measured_row_or_a_remediless_platform_suppresses_the_line() {
        use tm_core::i18n::Lang;

        let with = |state| FileIoAccounting {
            state,
            ..Default::default()
        };
        for state in [
            FileIoAccountingState::Unavailable,
            FileIoAccountingState::Undelivered,
            FileIoAccountingState::Undecodable,
            FileIoAccountingState::KernelLoggerInUse,
        ] {
            // merge_files keeps the larger of two row counts while replacing the
            // accounting, so an unmeasured snapshot can arrive over measured
            // rows. Those rows ARE the source; a line would deny them.
            assert_eq!(
                per_file_accounting_note(Some(&with(state)), true, true, Lang::En),
                None,
                "{state:?} must stay silent under a measured row"
            );
        }

        // Linux reports Unavailable for every snapshot and has no elevated host,
        // so the line would name a remedy the platform does not have.
        assert_eq!(
            per_file_accounting_note(
                Some(&with(FileIoAccountingState::Unavailable)),
                false,
                false,
                Lang::En
            ),
            None
        );
        // The remaining states describe what the trace could not do, not that
        // the token needs rights, so they do not depend on the remedy.
        for state in [
            FileIoAccountingState::Undelivered,
            FileIoAccountingState::Undecodable,
            FileIoAccountingState::KernelLoggerInUse,
        ] {
            assert!(
                per_file_accounting_note(Some(&with(state)), false, false, Lang::En).is_some(),
                "{state:?} states what the trace did, not what it needs"
            );
        }

        // No answer yet is not the same as no source.
        assert_eq!(
            per_file_accounting_note(None, false, true, Lang::En),
            None,
            "an unchecked process has nothing to explain yet"
        );
    }

    #[test]
    fn file_merge_accumulates_and_preserves_closed_handles() {
        let mut state = State::default();
        let batch1 = vec![
            ProcessFileIo {
                path: r"C:\data\input.txt".to_string(),
                read_bytes: Some(1000),
                write_bytes: Some(0),
                read_ops: None,
                write_ops: None,
                offset: Some(1000),
                file_size: Some(5000),
                access_known: true,
                is_read: true,
                is_write: false,
                open_handles: 1,
            },
            ProcessFileIo {
                path: r"C:\data\output.log".to_string(),
                read_bytes: Some(0),
                write_bytes: Some(500),
                read_ops: None,
                write_ops: None,
                offset: Some(500),
                file_size: Some(500),
                access_known: true,
                is_read: false,
                is_write: true,
                open_handles: 1,
            },
        ];
        state.merge_files(ProcessFileIoSnapshot {
            files: batch1,
            accounting: FileIoAccounting {
                state: FileIoAccountingState::Measured,
                handles_complete: true,
                ..Default::default()
            },
        });
        assert_eq!(state.files.len(), 2);
        assert_eq!(state.files[0].read_bytes, Some(1000));
        assert_eq!(state.files[0].open_handles, 1);
        assert_eq!(state.files[1].write_bytes, Some(500));

        // Batch 2: input.txt closed, output.log grew, new file opened
        let batch2 = vec![
            ProcessFileIo {
                path: r"C:\data\output.log".to_string(),
                read_bytes: Some(0),
                write_bytes: Some(1500),
                read_ops: None,
                write_ops: None,
                offset: Some(1500),
                file_size: Some(1500),
                access_known: true,
                is_read: false,
                is_write: true,
                open_handles: 1,
            },
            ProcessFileIo {
                path: r"C:\data\config.json".to_string(),
                read_bytes: Some(200),
                write_bytes: Some(0),
                read_ops: None,
                write_ops: None,
                offset: Some(200),
                file_size: Some(200),
                access_known: true,
                is_read: true,
                is_write: false,
                open_handles: 1,
            },
        ];
        state.merge_files(ProcessFileIoSnapshot {
            files: batch2,
            accounting: FileIoAccounting {
                state: FileIoAccountingState::Measured,
                handles_complete: true,
                ..Default::default()
            },
        });
        assert_eq!(state.files.len(), 3);

        let input = state
            .files
            .iter()
            .find(|f| f.path.contains("input.txt"))
            .unwrap();
        assert_eq!(input.read_bytes, Some(1000));
        assert_eq!(
            input.open_handles, 0,
            "closed file must have 0 open handles"
        );

        let output = state
            .files
            .iter()
            .find(|f| f.path.contains("output.log"))
            .unwrap();
        assert_eq!(output.write_bytes, Some(1500));
        assert_eq!(output.open_handles, 1);
    }

    #[test]
    fn file_sorting_and_filtering() {
        let mut state = State {
            files: vec![
                TrackedFileIo {
                    path: r"C:\alpha\doc.pdf".to_string(),
                    name: "doc.pdf".to_string(),
                    read_bytes: Some(5000),
                    write_bytes: Some(100),
                    total_bytes: Some(5100),
                    offset: Some(5000),
                    file_size: Some(10000),
                    access_known: true,
                    is_read: true,
                    is_write: false,
                    open_handles: 1,
                },
                TrackedFileIo {
                    path: r"C:\beta\heavy_write.dat".to_string(),
                    name: "heavy_write.dat".to_string(),
                    read_bytes: Some(100),
                    write_bytes: Some(9000),
                    total_bytes: Some(9100),
                    offset: Some(9000),
                    file_size: Some(9000),
                    access_known: true,
                    is_read: false,
                    is_write: true,
                    open_handles: 1,
                },
                TrackedFileIo {
                    path: r"C:\gamma\notes.txt".to_string(),
                    name: "notes.txt".to_string(),
                    read_bytes: Some(2000),
                    write_bytes: Some(2000),
                    total_bytes: Some(4000),
                    offset: Some(2000),
                    file_size: Some(2000),
                    access_known: true,
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

        // Sort by Path ascending
        state.file_filter.clear();
        state.file_sort_column = FileSortColumn::Path;
        state.file_sort_ascending = true;
        let sorted_path = state.sorted_and_filtered_files();
        assert_eq!(sorted_path[0].name, "doc.pdf"); // C:\alpha
        assert_eq!(sorted_path[1].name, "heavy_write.dat"); // C:\beta
        assert_eq!(sorted_path[2].name, "notes.txt"); // C:\gamma

        // Sort by Path descending
        state.file_sort_ascending = false;
        let sorted_path_desc = state.sorted_and_filtered_files();
        assert_eq!(sorted_path_desc[0].name, "notes.txt"); // C:\gamma
        assert_eq!(sorted_path_desc[1].name, "heavy_write.dat"); // C:\beta
        assert_eq!(sorted_path_desc[2].name, "doc.pdf"); // C:\alpha
    }

    #[test]
    fn unavailable_file_io_never_uses_offset_as_total() {
        assert_eq!(measured_total(None, None), None);
        assert_eq!(measured_total(Some(10), None), None);
        assert_eq!(measured_total(None, Some(20)), None);
        assert_eq!(measured_total(Some(10), Some(20)), Some(30));
        assert_eq!(fmt_optional_bytes(None), "\u{2014}");
        assert_eq!(fmt_optional_bytes(Some(0)), "0 B");
        assert_eq!(fmt_optional_count(None), "\u{2014}");
        assert_eq!(fmt_optional_count(Some(0)), "0");
    }

    #[test]
    fn incomplete_snapshot_does_not_close_or_retype_unseen_handles() {
        let mut state = State::default();
        state.files.push(TrackedFileIo {
            path: "/tmp/open".into(),
            name: "open".into(),
            read_bytes: None,
            write_bytes: None,
            total_bytes: None,
            offset: Some(7),
            file_size: Some(10),
            access_known: true,
            is_read: true,
            is_write: false,
            open_handles: 1,
        });
        state.merge_files(ProcessFileIoSnapshot {
            files: Vec::new(),
            accounting: FileIoAccounting {
                handles_complete: false,
                ..Default::default()
            },
        });
        let file = &state.files[0];
        assert_eq!(file.open_handles, 1);
        assert!(file.is_read);
        assert_eq!(file.offset, Some(7));

        state.merge_files(ProcessFileIoSnapshot {
            files: Vec::new(),
            accounting: FileIoAccounting {
                handles_complete: true,
                ..Default::default()
            },
        });
        assert!(
            state.files.is_empty(),
            "an unmeasured closed row is dropped instead of being retained forever"
        );
    }

    #[test]
    fn visible_io_slice_windowing() {
        let history: Vec<IoSample> = (0..100)
            .map(|i| IoSample {
                t_ms: i * 1_000,
                read_bps: Some(100.0),
                write_bps: Some(50.0),
                other_bps: Some(25.0),
                read_delta: Some(100),
                write_delta: Some(50),
                other_delta: Some(25),
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
                    let read_samples: Vec<Option<f64>> = win.iter().map(|s| s.read_bps).collect();

                    paint_io_graph_section(
                        ui,
                        "test-io-read",
                        "Read",
                        read_samples.last().copied().flatten(),
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
                read_bps: Some((i as f64) * 1000.0),
                write_bps: Some(0.0),
                other_bps: Some(0.0),
                read_delta: Some(0),
                write_delta: Some(0),
                other_delta: Some(0),
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
            read_bps: Some(99_999.0),
            write_bps: Some(0.0),
            other_bps: Some(0.0),
            read_delta: Some(0),
            write_delta: Some(0),
            other_delta: Some(0),
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
                    read_bytes: Some(5000),
                    write_bytes: Some(100),
                    total_bytes: Some(5100),
                    offset: Some(5000),
                    file_size: Some(10000),
                    access_known: true,
                    is_read: true,
                    is_write: false,
                    open_handles: 1,
                },
                TrackedFileIo {
                    path: r"C:\beta\heavy_write.dat".to_string(),
                    name: "heavy_write.dat".to_string(),
                    read_bytes: Some(100),
                    write_bytes: Some(9000),
                    total_bytes: Some(9100),
                    offset: Some(9000),
                    file_size: Some(9000),
                    access_known: true,
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
                .num_columns(6)
                .striped(true)
                .show(ui, |ui| {
                    ui.label("File");
                    ui.label("Path");
                    ui.label("Read");
                    ui.label("Written");
                    ui.label("Total");
                    ui.label("Size");
                    ui.end_row();

                    for f in display_files {
                        ui.label(&f.name);
                        ui.label(&f.path);
                        ui.label(f.read_bytes.map_or("—".into(), format::format_bytes_loc));
                        ui.label(f.write_bytes.map_or("—".into(), format::format_bytes_loc));
                        ui.label(f.total_bytes.map_or("—".into(), format::format_bytes_loc));
                        ui.label(f.file_size.map_or("—".into(), format::format_bytes_loc));
                        ui.end_row();
                    }
                });
        });
        output.textures_delta.clear();
        assert!(!output.shapes.is_empty());
    }

    #[test]
    fn empty_files_with_incomplete_handles_reports_unavailable() {
        let mut state = State::default();
        state.merge_files(ProcessFileIoSnapshot {
            files: Vec::new(),
            accounting: FileIoAccounting {
                handles_complete: false,
                counter_read_bytes: Some(1024),
                counter_write_bytes: Some(2048),
                ..Default::default()
            },
        });
        assert!(state.files.is_empty());
        assert_eq!(
            state.accounting.as_ref().map(|a| a.handles_complete),
            Some(false)
        );
    }

    #[test]
    fn retained_row_cap_marks_the_presented_snapshot_incomplete() {
        let files = (0..=MAX_RETAINED_IO_ROWS)
            .map(|index| tm_platform::actions::ProcessFileIo {
                path: format!("/tmp/file-{index}"),
                read_bytes: None,
                write_bytes: None,
                read_ops: None,
                write_ops: None,
                file_size: None,
                offset: None,
                access_known: false,
                is_read: false,
                is_write: false,
                open_handles: 1,
            })
            .collect();
        let mut state = State::default();
        state.merge_files(ProcessFileIoSnapshot {
            files,
            accounting: FileIoAccounting {
                handles_complete: true,
                ..Default::default()
            },
        });

        assert_eq!(state.files.len(), MAX_RETAINED_IO_ROWS);
        assert_eq!(
            state.accounting.as_ref().map(|a| a.handles_complete),
            Some(false)
        );
    }

    #[test]
    fn file_entry_cell_context_menu_renders_options() {
        let ctx = egui::Context::default();
        let val_rect = std::cell::Cell::new(egui::Rect::NOTHING);
        let toast_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let toast_cb = {
            let tc = toast_called.clone();
            move |_: &str| {
                tc.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        };
        let reveal_requested = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let reveal_cb = {
            let rr = reveal_requested.clone();
            move |path: &str| {
                *tm_core::sync::lock(&rr) = Some(path.to_owned());
            }
        };

        let frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                let resp = ui.add(egui::Label::new("C:\\data\\test.txt").selectable(true));
                val_rect.set(resp.rect);
                file_entry_cell_context_menu_actions(
                    &toast_cb,
                    &reveal_cb,
                    &resp,
                    &resp,
                    "test.txt",
                    "C:\\data\\test.txt",
                );
            });
            output.drop_without_applying_deltas();
        };

        // Frame 1: layout
        frame(Vec::new());

        // Frame 2: pointer move
        let at = val_rect.get().center();
        frame(vec![egui::Event::PointerMoved(at)]);

        // Frame 3: right click
        frame(vec![
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: Default::default(),
            },
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: Default::default(),
            },
        ]);

        // Frame 4: popup sizing pass
        frame(Vec::new());

        assert!(
            ctx.any_popup_open(),
            "right clicking file entry opens context menu"
        );
    }
}

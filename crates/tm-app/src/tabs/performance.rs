//! Performance tab: device cards with mini graphs on the left, TM-style
//! detail pages on the right (per-core grid for CPU, big rolling charts,
//! big-value stats + key/value list below).

use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Pos2};
use tm_core::format;
use tm_core::i18n::{self, K};

use crate::app::{HistoryPoint, TaskManApp};
use crate::search;
use crate::theme::{self, Palette};
use crate::widgets::chart::{MultiSeries, chart_multi, core_chart};
use crate::widgets::menu;

/// Time-based visible slice: every point whose timestamp lies inside the
/// configured window (§14.3). Works identically at High/Normal/Low update
/// speeds and with irregular gaps.
pub fn visible_slice(history: &[HistoryPoint], seconds: u32) -> &[HistoryPoint] {
    let Some(last) = history.last() else {
        return &[];
    };
    let cutoff = last.t_ms.saturating_sub(seconds as u64 * 1000);
    // Backward scan instead of `partition_point`: history is appended in
    // sample order, but a wall-clock step BACKWARD (NTP correction after
    // resume) leaves older points with larger timestamps. A binary search
    // over that temporarily-unsorted buffer can drop the newest points and
    // freeze the charts until they age out; scanning back from the newest
    // sample always keeps the latest data in view (at worst the window is
    // briefly too wide until the stale points are evicted).
    let mut start = history.len();
    while start > 0 && history[start - 1].t_ms >= cutoff {
        start -= 1;
    }
    &history[start..]
}

/// Human label for the graph window ("30 Sekunden"/"30 seconds", "2 min")
/// instead of a hardcoded caption (§14.5).
pub fn window_label(seconds: u32) -> String {
    if seconds < 60 {
        format!(
            "{seconds} {}",
            tm_core::i18n::tr(tm_core::i18n::K::SecondsSuffix)
        )
    } else {
        format!(
            "{} {}",
            seconds / 60,
            tm_core::i18n::tr(tm_core::i18n::K::MinShort)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(t_ms: u64) -> HistoryPoint {
        HistoryPoint {
            t_ms,
            ..Default::default()
        }
    }

    #[test]
    fn graph_window_is_time_based_at_high_normal_low_intervals() {
        // 500 ms sampling, 60 s window.
        let fast: Vec<HistoryPoint> = (0..=120).map(|i| pt(i * 500)).collect();
        assert_eq!(visible_slice(&fast, 60).len(), 121);

        // 1 s sampling.
        let normal: Vec<HistoryPoint> = (0..=60).map(|i| pt(i * 1000)).collect();
        assert_eq!(visible_slice(&normal, 60).len(), 61);

        // 4 s sampling: only points within the last 60 s survive even though
        // the buffer holds far fewer samples than a count-based window would.
        let slow: Vec<HistoryPoint> = (0..=15).map(|i| pt(i * 4000)).collect();
        let slice = visible_slice(&slow, 60);
        let span_ms = slice.last().unwrap().t_ms - slice.first().unwrap().t_ms;
        assert!((56_000..=60_000).contains(&span_ms), "span {span_ms}");
    }

    #[test]
    fn graph_window_handles_irregular_timestamps() {
        // A delayed sample must plot by its true time, not shift everything:
        // points at 0s, 2s, 40s(delay), 41s → 60 s window keeps all four.
        let irr: Vec<HistoryPoint> = [0u64, 2000, 40_000, 41_000]
            .iter()
            .map(|t| pt(*t))
            .collect();
        assert_eq!(visible_slice(&irr, 60).len(), 4);
        // Window of 10 s keeps only the last two.
        assert_eq!(visible_slice(&irr, 10).len(), 2);
    }

    /// Regression: a wall-clock step BACKWARD (e.g. NTP correction after
    /// resume) leaves older points with larger timestamps than the newest
    /// ones. The slice must still keep the newest samples in view — a
    /// binary search over the temporarily-unsorted buffer could not
    /// guarantee that, and the charts degraded with it.
    #[test]
    fn visible_slice_keeps_newest_point_after_backward_clock_step() {
        let mut hist: Vec<HistoryPoint> = (0..120u64).map(|i| pt(i * 1000)).collect();
        // Clock jumps back 30 s; new samples append with smaller timestamps.
        hist.push(pt(90_000));
        hist.push(pt(91_000));
        let win = visible_slice(&hist, 60);
        assert_eq!(
            win.last().unwrap().t_ms,
            91_000,
            "newest point must stay in view"
        );
        assert!(win.len() >= 2, "window must still hold plottable data");
    }

    #[test]
    fn window_label_formats_seconds_and_minutes() {
        assert!(window_label(30).contains('3'));
        assert!(window_label(60).contains('1'));
        assert!(window_label(120).contains('2'));
    }

    #[test]
    fn logical_grid_adapts_to_core_count_and_width() {
        let (cols_16, width_16, height_16) = logical_grid_layout(800.0, 16);
        assert_eq!(cols_16, 4);
        assert!(width_16 > height_16);

        let (cols_64, _, _) = logical_grid_layout(800.0, 64);
        assert!(cols_64 >= 8, "64 threads should not become a 16-row strip");

        let (narrow, _, _) = logical_grid_layout(360.0, 16);
        assert!(narrow < cols_16);
    }

    /// Regression: the series extractors used to SKIP window points without
    /// the requested device (and net_series even dropped zero-traffic
    /// samples). The shortened series then desynchronized from the shared
    /// timestamp vector, plotting samples at wrong x positions.
    #[test]
    fn series_extractors_stay_aligned_with_window() {
        let mut hit = pt(500);
        hit.nets = vec![("eth".into(), 10.0, 20.0)];
        hit.disks = vec![("C:".into(), 50.0, 1.0, 2.0)];
        let win = vec![pt(0), hit, pt(1000)];

        let net = net_series(&win, "eth", 1);
        assert_eq!(net, vec![0.0, 10.0, 0.0], "len must equal window len");
        let sent = net_series(&win, "eth", 2);
        assert_eq!(sent, vec![0.0, 20.0, 0.0]);
        let disk = disk_series(&win, "C:", |d| d.1 as f64);
        assert_eq!(disk, vec![0.0, 50.0, 0.0]);
        let gpu = gpu_series(&win, "0", 1);
        assert_eq!(gpu, vec![0.0, 0.0, 0.0]);
    }

    /// The adapter's own utilization is the BUSIEST engine, so charting one
    /// specific engine has to read the per-engine samples — otherwise "Video
    /// Encode" would silently plot the 3D load.
    #[test]
    fn a_gpu_engine_series_follows_that_engine_only() {
        let mut busy = pt(500);
        busy.gpus = vec![(0, 90.0, 0)];
        busy.gpu_engines = vec![
            (0, "3D".into(), 90.0),
            (0, "VideoEncode".into(), 12.0),
            (1, "3D".into(), 5.0),
        ];
        let win = vec![pt(0), busy, pt(1000)];

        assert_eq!(
            gpu_engine_series(&win, "0", "VideoEncode"),
            vec![0.0, 12.0, 0.0]
        );
        assert_eq!(gpu_engine_series(&win, "0", "3D"), vec![0.0, 90.0, 0.0]);
        // Another adapter's engine of the same name must not bleed in.
        assert_eq!(gpu_engine_series(&win, "1", "3D"), vec![0.0, 5.0, 0.0]);
        // An engine this adapter never reported is a flat, honest zero.
        assert_eq!(gpu_engine_series(&win, "0", "Copy"), vec![0.0, 0.0, 0.0]);
    }

    /// An engine that has gone idle drops out of the snapshot's truncated,
    /// utilization-sorted list — but it must stay in the menu, because
    /// "what was the encoder doing" is asked after the encoding stops.
    #[test]
    fn the_engine_menu_remembers_engines_seen_in_the_window() {
        let mut earlier = pt(0);
        earlier.gpu_engines = vec![(0, "VideoEncode".into(), 40.0)];
        let win = vec![earlier, pt(1000)];
        let snapshot = [tm_core::model::GpuEngine {
            name: "3D".into(),
            util_pct: 7.0,
        }];

        let names = gpu_engine_names(&win, &snapshot, "0");
        assert_eq!(names, vec!["3D".to_string(), "VideoEncode".to_string()]);
        // Ordering is by role, not by the order they were discovered in.
        let mut reversed = names.clone();
        reversed.reverse();
        assert_eq!(gpu_engine_names(&win, &snapshot, "0"), names);
        assert_ne!(reversed, names);
    }

    #[test]
    fn engine_labels_are_spelled_the_way_people_read_them() {
        assert_eq!(engine_label("VideoEncode"), "Video Encode");
        assert_eq!(engine_label("VideoDecode"), "Video Decode");
        assert_eq!(engine_label("3D"), "3D");
        assert_eq!(engine_label("Compute"), "Compute");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Cpu,
    Memory,
    Disk,
    Network,
    Gpu,
}

fn resource_color(pal: &Palette, kind: ResourceKind) -> Color32 {
    match kind {
        ResourceKind::Cpu => pal.cpu_graph,
        ResourceKind::Memory => pal.memory_graph,
        ResourceKind::Disk => pal.disk_graph,
        ResourceKind::Network => pal.network_graph,
        ResourceKind::Gpu => pal.gpu_graph,
    }
}

#[derive(Clone)]
pub struct ResourceEntry {
    pub kind: ResourceKind,
    pub key: String,
    pub title: String,
    pub subtitle: String,
    pub value_line: String,
}

/// Gutter between the page edges and the content — captions, titles, stats
/// and charts all share it so their edges line up.
const GUTTER: f32 = 16.0;

/// Kernel-time shade: a darker tone of the accent hue — Task Manager draws
/// the kernel share as a deeper band of the SAME color, not a clashing
/// second hue (§14.4). `pal`-derived, so it stays readable in both themes
/// (the old hardcoded dark-theme green washed out in light mode).
fn kernel_color(pal: &Palette) -> Color32 {
    pal.cpu_graph.gamma_multiply(0.55)
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let Some(_snap) = app.latest_snapshot() else {
        ui.centered_and_justified(|ui| ui.label(i18n::tr(K::GatheringData)));
        return;
    };

    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |_app, _ui| {},
        |app, ui| {
            if menu::item(ui, i18n::tr(K::RefreshNow)).clicked() {
                // Regression fix (audit §9): this used to close the menu
                // without doing anything — Refresh now must actually force a
                // fresh sample plus tab-local cache invalidation.
                app.refresh_all();
                ui.close();
            }
        },
    );

    // Selection persists by stable resource key so adapter/device
    // reordering cannot silently switch the selected page (§14.2).
    let entries = build_resource_list(app);
    if !entries.iter().any(|e| e.key == app.perf_selected_key) {
        app.perf_selected_key = entries
            .first()
            .map_or_else(|| "cpu".into(), |e| e.key.clone());
    }

    // Task-Manager-style type navigation over the resource cards: a plain
    // letter selects the next card whose title begins with it, and the card
    // column scrolls vertically to keep it in view.
    if let Some(typed) = search::list_type_ahead(ui.ctx(), "performance") {
        let selected = Some(app.perf_selected_key.clone());
        let candidates = entries
            .iter()
            .map(|e| (e.key.clone(), e.title.as_str()))
            .collect::<Vec<_>>();
        if let Some(key) = search::type_ahead_match(candidates, selected, &typed) {
            app.perf_selected_key = key.clone();
            app.perf_jump_to = Some(key);
        }
    }

    ui.horizontal_top(|ui| {
        // ---------------- left column of cards (user-resizable) -------------
        let mut card_w = app.shared.settings.perf_card_width.clamp(180.0, 520.0);
        egui::ScrollArea::vertical()
            .id_salt("perf-cards")
            .show(ui, |ui| {
                ui.set_width(card_w);
                ui.vertical(|ui| {
                    // Sparkline data is extracted in one immutable pass so
                    // the mutable UI pass below never fights the borrow.
                    let win = window(app);
                    let card_series: Vec<Vec<f64>> = entries
                        .iter()
                        .map(|e| match e.kind {
                            ResourceKind::Cpu => series(win, |h| h.cpu_total as f64),
                            ResourceKind::Memory => {
                                series(win, |h| pct_of(h.mem_used_bytes, h.mem_total_bytes))
                            }
                            ResourceKind::Disk => disk_series(win, &e.key, |d| d.1 as f64),
                            ResourceKind::Network => net_series(win, &e.key, 1),
                            ResourceKind::Gpu => gpu_series(win, &e.key, 1),
                        })
                        .collect();
                    for (e, samples) in entries.iter().zip(card_series.iter()) {
                        let selected = e.key == app.perf_selected_key;
                        card_ui(app, ui, &pal, e, selected, samples);
                    }
                    ui.add_space(8.0);
                });
            });

        // Drag splitter between the card column and the detail area.
        // egui's `drag_delta()` is movement since LAST FRAME, so it must
        // accumulate onto the LIVE width each frame. A frozen drag-start
        // value plus one frame of delta reset `card_w` to ~its starting
        // value every frame, so the splitter never followed the cursor
        // (the same root cause as the table column resize bug).
        let split_h = ui.available_height();
        let (srect, sresp) = ui.allocate_exact_size(egui::vec2(10.0, split_h), egui::Sense::drag());
        if sresp.hovered() || sresp.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if sresp.dragged() {
            card_w = (card_w + sresp.drag_delta().x).clamp(180.0, 520.0);
            app.shared.settings.perf_card_width = card_w;
            ui.painter().line_segment(
                [
                    Pos2::new(srect.center().x, srect.top()),
                    Pos2::new(srect.center().x, srect.bottom()),
                ],
                egui::Stroke::new(2.0, pal.accent),
            );
        }

        ui.add_space(6.0);

        // ---------------- right detail area ---------------------------------
        if let Some(sel) = entries
            .iter()
            .find(|e| e.key == app.perf_selected_key)
            .cloned()
        {
            egui::ScrollArea::vertical()
                .id_salt("perf-detail")
                .auto_shrink(false)
                .show(ui, |ui| {
                    ui.vertical(|ui| match sel.kind {
                        ResourceKind::Cpu => cpu_page(app, ui, &pal),
                        ResourceKind::Memory => memory_page(app, ui, &pal),
                        ResourceKind::Disk => disk_page(app, ui, &pal, &sel),
                        ResourceKind::Network => network_page(app, ui, &pal, &sel),
                        ResourceKind::Gpu => gpu_page(app, ui, &pal, &sel),
                    });
                });
        }
    });
}

// ---------------------------------------------------------------- cards

fn card_ui(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &Palette,
    e: &ResourceEntry,
    selected: bool,
    samples: &[f64],
) {
    let size = egui::vec2(ui.available_width(), 64.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 3.0, pal.card_bg);
        ui.painter().rect_stroke(
            rect,
            3.0,
            egui::Stroke::new(1.2, pal.text_dim),
            egui::StrokeKind::Inside,
        );
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, pal.card_bg);
    }
    if resp.clicked() {
        app.perf_selected_key = e.key.clone();
    }

    // Consume a one-shot type-ahead scroll request for this card. The card
    // list is fully rendered (not virtualized), so the response always
    // exists; `None` align scrolls minimally on the vertical axis only.
    if app.perf_jump_to.as_deref() == Some(e.key.as_str()) {
        resp.scroll_to_me(None);
        app.perf_jump_to = None;
    }

    // Mini graph on the left, painted into the card rect.
    let chart_rect = egui::Rect::from_min_size(
        Pos2::new(rect.left() + 10.0, rect.top() + 12.0),
        egui::vec2(62.0, 40.0),
    );
    let color = resource_color(pal, e.kind);
    crate::widgets::chart::paint_sparkline(ui, chart_rect, samples, color);

    // Text block: title / subtitle / value. Ellipsized — painter text is
    // drawn unclipped, so long adapter names would bleed past the card.
    let tx = rect.left() + 84.0;
    let max_text_w = rect.right() - 8.0 - tx;
    let title_font = FontId::proportional(17.0);
    let small_font = FontId::proportional(13.0);
    ui.painter().text(
        Pos2::new(tx, rect.top() + 13.0),
        Align2::LEFT_CENTER,
        ellipsize(ui, &e.title, &title_font, max_text_w),
        title_font,
        pal.text,
    );
    if !e.subtitle.is_empty() {
        ui.painter().text(
            Pos2::new(tx, rect.top() + 30.0),
            Align2::LEFT_CENTER,
            ellipsize(ui, &e.subtitle, &small_font, max_text_w),
            small_font.clone(),
            pal.text_dim,
        );
    }
    ui.painter().text(
        Pos2::new(tx, rect.top() + 47.0),
        Align2::LEFT_CENTER,
        ellipsize(ui, &e.value_line, &small_font, max_text_w),
        small_font,
        pal.text_dim,
    );
}

fn pct_of(v: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        v as f64 / total as f64 * 100.0
    }
}

fn build_resource_list(app: &TaskManApp) -> Vec<ResourceEntry> {
    let mut out = Vec::new();
    let Some(snap) = app.latest_snapshot() else {
        return out;
    };
    let _ = app;

    out.push(ResourceEntry {
        kind: ResourceKind::Cpu,
        key: "cpu".into(),
        title: "CPU".into(),
        subtitle: String::new(),
        value_line: format!(
            "{}  {}",
            format::format_pct_hdr(snap.cpu.utilization_pct),
            format::format_freq_ghz(snap.cpu.freq_mhz)
        ),
    });

    out.push(ResourceEntry {
        kind: ResourceKind::Memory,
        key: "mem".into(),
        title: i18n::tr(K::MemTitle).into(),
        subtitle: String::new(),
        value_line: format!(
            "{}/{} ({})",
            format::format_bytes_loc(snap.memory.used_bytes),
            format::format_bytes_loc(snap.memory.total_bytes),
            format::format_pct_hdr(snap.memory.used_pct())
        ),
    });

    for d in &snap.disks {
        out.push(ResourceEntry {
            kind: ResourceKind::Disk,
            key: d.mount.clone(),
            title: format!("{} {}", i18n::tr(K::DiskTitle), d.id),
            subtitle: disk_media_label(d),
            value_line: format::format_pct_hdr(d.active_pct),
        });
    }

    for n in &snap.networks {
        if n.kind == "Loopback" {
            continue;
        }
        // Card subtitle: adapter model when known, else the interface name.
        let subtitle = if !n.desc.is_empty() {
            n.desc.clone()
        } else if n.kind.is_empty() {
            String::new()
        } else {
            n.name.clone()
        };
        out.push(ResourceEntry {
            kind: ResourceKind::Network,
            key: n.name.clone(),
            title: if n.kind.is_empty() {
                n.name.clone()
            } else {
                n.kind.clone()
            },
            subtitle,
            value_line: i18n::trf(
                K::CardSentRecv,
                &[
                    &format::format_kbit(n.sent_bps),
                    &format::format_kbit(n.recv_bps),
                ],
            ),
        });
    }

    for g in &snap.gpus {
        out.push(ResourceEntry {
            kind: ResourceKind::Gpu,
            key: g.id.to_string(),
            title: format!("GPU {}", g.id),
            subtitle: g.name.clone(),
            value_line: format!(
                "{}{}",
                format::format_pct_hdr(g.util_pct),
                g.temperature_c
                    .map(|t| format!("  ({t:.0} °C)"))
                    .unwrap_or_default()
            ),
        });
    }
    out
}

fn disk_media_label(d: &tm_core::model::DiskInfo) -> String {
    let s = d.media.label().to_string();
    if s.is_empty() { d.mount.clone() } else { s }
}

// ---------------------------------------------------------------- helpers

/// Ellipsize `s` so it fits `max_w` at `font`. Painter text is drawn
/// unclipped, so overlong strings must be trimmed before painting or they
/// bleed into neighboring elements.
fn ellipsize(ui: &egui::Ui, s: &str, font: &FontId, max_w: f32) -> String {
    if max_w <= 0.0 {
        return String::new();
    }
    let fits = |t: &str| {
        ui.painter()
            .layout_no_wrap(t.to_owned(), font.clone(), Color32::WHITE)
            .size()
            .x
            <= max_w
    };
    if fits(s) {
        return s.to_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let t: String = chars[..mid].iter().collect::<String>() + "…";
        if fits(&t) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut t: String = chars[..lo].iter().collect();
    while t.ends_with(' ') {
        t.pop();
    }
    t.push('…');
    t
}

/// Chart block inset by the page gutter so chart edges align with the
/// caption text above (the old code drew charts flush-left at avail−64 —
/// misaligned on BOTH sides).
fn page_chart(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    series: &[MultiSeries],
    y_max: f64,
    ts: Option<&[u64]>,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.add_space(GUTTER);
        chart_multi(ui, egui::vec2(width, height), series, y_max, ts)
    })
    .inner
}

/// The time-windowed history slice shared by all pages this frame.
fn window(app: &TaskManApp) -> &[HistoryPoint] {
    visible_slice(&app.history, app.shared.settings.graph_seconds)
}

fn series(win: &[HistoryPoint], f: impl Fn(&HistoryPoint) -> f64) -> Vec<f64> {
    win.iter().map(f).collect()
}

fn timestamps(win: &[HistoryPoint]) -> Vec<u64> {
    win.iter().map(|h| h.t_ms).collect()
}

fn disk_series(
    win: &[HistoryPoint],
    key: &str,
    pick: impl Fn(&(String, f32, f64, f64)) -> f64,
) -> Vec<f64> {
    // One value PER window point (0.0 when absent): series indices must stay
    // aligned with the shared timestamps, otherwise samples plot at wrong
    // x positions (cliffs and shifted curves).
    win.iter()
        .map(|h| h.disks.iter().find(|(m, ..)| m == key).map_or(0.0, &pick))
        .collect()
}

fn net_series(win: &[HistoryPoint], key: &str, idx: usize) -> Vec<f64> {
    // No zero-filtering here either: dropping samples shortens the series
    // and desynchronizes it from `timestamps_ms`. Zero traffic plots as
    // zero — honest and aligned.
    win.iter()
        .map(|h| {
            h.nets
                .iter()
                .find(|(m, ..)| m == key)
                .map_or(0.0, |t| if idx == 1 { t.1 } else { t.2 })
        })
        .collect()
}

/// Utilization history of ONE engine type on one adapter.
///
/// A tick where the adapter reported no instance of that engine is a real
/// zero, not a gap: PDH only publishes an engine instance while something is
/// scheduled on it, so "no NVENC instance" means nothing was encoding.
fn gpu_engine_series(win: &[HistoryPoint], key: &str, engine: &str) -> Vec<f64> {
    win.iter()
        .map(|h| {
            h.gpu_engines
                .iter()
                .filter(|(id, name, _)| id.to_string() == key && name == engine)
                .map(|(.., util)| f64::from(*util))
                .fold(0.0f64, f64::max)
        })
        .collect()
}

/// Engine types this adapter has reported at any point in the visible window,
/// in a stable order.
///
/// Taken from the history and not just the newest snapshot on purpose: the
/// snapshot's engine list is sorted by utilization and truncated, so an engine
/// that has just gone idle would drop out of the menu the moment the user
/// wanted to look at what it had been doing.
fn gpu_engine_names(
    win: &[HistoryPoint],
    snapshot: &[tm_core::model::GpuEngine],
    key: &str,
) -> Vec<String> {
    let mut names: Vec<String> = snapshot.iter().map(|e| e.name.clone()).collect();
    for (id, name, _) in win.iter().flat_map(|h| h.gpu_engines.iter()) {
        if id.to_string() == key && !names.iter().any(|known| known == name) {
            names.push(name.clone());
        }
    }
    names.sort_by(|a, b| engine_rank(a).cmp(&engine_rank(b)).then_with(|| a.cmp(b)));
    names
}

/// Task Manager's engine ordering: the ones a person looks for first.
/// Anything unrecognized keeps its name order after them.
fn engine_rank(name: &str) -> u8 {
    match name {
        "3D" => 0,
        "Compute" => 1,
        "Copy" => 2,
        "VideoDecode" => 3,
        "VideoEncode" => 4,
        "VideoProcessing" => 5,
        "Security" => 6,
        _ => 7,
    }
}

/// Human-readable engine label. PDH reports the raw D3D engine-type names, and
/// "VideoEncode" in the graph caption of a task manager should read the way it
/// does in every other task manager.
fn engine_label(name: &str) -> String {
    match name {
        "VideoDecode" => "Video Decode".to_string(),
        "VideoEncode" => "Video Encode".to_string(),
        "VideoProcessing" => "Video Processing".to_string(),
        other => other.to_string(),
    }
}

fn gpu_series(win: &[HistoryPoint], key: &str, idx: usize) -> Vec<f64> {
    win.iter()
        .map(|h| {
            h.gpus
                .iter()
                .find(|(id, ..)| id.to_string() == key)
                .map_or(0.0, |t| if idx == 1 { t.1 as f64 } else { t.2 as f64 })
        })
        .collect()
}

/// Page title row: big name left, detail right.
fn page_title(ui: &mut egui::Ui, pal: &Palette, title: &str, right: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 44.0), egui::Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left() + GUTTER, rect.center().y),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(31.0),
        pal.text,
    );
    ui.painter().text(
        Pos2::new(rect.right() - GUTTER, rect.center().y),
        Align2::RIGHT_CENTER,
        right,
        FontId::proportional(17.0),
        pal.text,
    );
}

/// Caption row: dim caption left, scale max right.
fn caption(ui: &mut egui::Ui, pal: &Palette, left: &str, right: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left() + GUTTER, rect.center().y),
        Align2::LEFT_CENTER,
        left,
        FontId::proportional(11.5),
        pal.text_dim,
    );
    ui.painter().text(
        Pos2::new(rect.right() - GUTTER, rect.center().y),
        Align2::RIGHT_CENTER,
        right,
        FontId::proportional(11.5),
        pal.text_dim,
    );
}

/// Big-value stat (label above, large number below).
fn big_stat(ui: &mut egui::Ui, pal: &Palette, label: &str, value: &str, w: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 56.0), egui::Sense::hover());
    let label_font = FontId::proportional(13.0);
    let value_font = FontId::proportional(23.0);
    let label = ellipsize(ui, label, &label_font, w - 4.0);
    let value = ellipsize(ui, value, &value_font, w - 4.0);
    ui.painter().text(
        Pos2::new(rect.left(), rect.top() + 9.0),
        Align2::LEFT_CENTER,
        &label,
        label_font,
        pal.text_dim,
    );
    ui.painter().text(
        Pos2::new(rect.left(), rect.bottom() - 12.0),
        Align2::LEFT_CENTER,
        &value,
        value_font,
        pal.text,
    );
}

/// Medium stat (label above, medium number below).
fn med_stat(ui: &mut egui::Ui, pal: &Palette, label: &str, value: &str, w: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 48.0), egui::Sense::hover());
    let label_font = FontId::proportional(13.0);
    let value_font = FontId::proportional(23.0);
    let label = ellipsize(ui, label, &label_font, w - 4.0);
    let value = ellipsize(ui, value, &value_font, w - 4.0);
    ui.painter().text(
        Pos2::new(rect.left(), rect.top() + 9.0),
        Align2::LEFT_CENTER,
        &label,
        label_font,
        pal.text_dim,
    );
    ui.painter().text(
        Pos2::new(rect.left(), rect.bottom() - 12.0),
        Align2::LEFT_CENTER,
        &value,
        value_font,
        pal.text,
    );
}

/// Key/value row for the right-hand details list. Fixed 190 px key column
/// (like TM); both sides ellipsized so long localized keys can never
/// collide with the value and long values stay inside the page gutter.
fn kv_row(ui: &mut egui::Ui, pal: &Palette, key: &str, value: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 23.0), egui::Sense::hover());
    let key_font = FontId::proportional(13.0);
    let value_font = FontId::proportional(13.0);
    let value_x = rect.left() + 190.0;
    let key = ellipsize(ui, key, &key_font, 190.0 - 8.0);
    let value = ellipsize(ui, value, &value_font, rect.right() - GUTTER - value_x);
    ui.painter().text(
        Pos2::new(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        &key,
        key_font,
        pal.text_dim,
    );
    ui.painter().text(
        Pos2::new(value_x, rect.center().y),
        Align2::LEFT_CENTER,
        &value,
        value_font,
        pal.text,
    );
}

fn content_width(ui: &egui::Ui) -> f32 {
    ui.available_width() - 2.0 * GUTTER
}

/// Page-bottom layout: big stats on the left, key/value details on the
/// right — or stacked vertically when the detail area is too narrow for
/// both (otherwise egui squeezes the kv column to zero width and the
/// details silently vanish).
fn stats_block(
    ui: &mut egui::Ui,
    stats_w: f32,
    stats: impl FnOnce(&mut egui::Ui),
    details: impl FnOnce(&mut egui::Ui),
) {
    const KV_MIN: f32 = 260.0;
    const GAP: f32 = 30.0;
    if ui.available_width() >= GUTTER + stats_w + GAP + KV_MIN {
        ui.horizontal_top(|ui| {
            ui.add_space(GUTTER);
            stats(ui);
            ui.add_space(GAP);
            details(ui);
        });
    } else {
        ui.add_space(12.0);
        ui.horizontal_top(|ui| {
            ui.add_space(GUTTER);
            stats(ui);
        });
        ui.add_space(12.0);
        ui.horizontal_top(|ui| {
            ui.add_space(GUTTER);
            details(ui);
        });
    }
}

// ---------------------------------------------------------------- CPU page

fn logical_grid_layout(width: f32, cores: usize) -> (usize, f32, f32) {
    const GAP: f32 = 6.0;
    if cores == 0 {
        return (1, width.max(60.0), 48.0);
    }
    let min_cols = if cores > 1 { 2 } else { 1 };
    let max_cols = (((width + GAP) / (78.0 + GAP)).floor() as usize)
        .max(min_cols)
        .min(cores);
    let page_aspect = (width / 420.0).clamp(1.0, 2.4);
    let target =
        ((cores as f32 * page_aspect / 1.8).sqrt().round() as usize).clamp(min_cols, max_cols);

    // Prefer an even final row when a nearby column count divides the core
    // count. This keeps 16→4×4 and 64→8×8 while still adapting on narrow UI.
    let start = target.saturating_sub(2).max(min_cols);
    let end = (target + 2).min(max_cols);
    let cols = (start..=end)
        .min_by(|a, b| {
            let score = |cols: usize| {
                let blanks = (cols - cores % cols) % cols;
                (cols.abs_diff(target) as f32) + blanks as f32 * 0.45
            };
            score(*a)
                .partial_cmp(&score(*b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(target);
    let cell_width = ((width - GAP * (cols - 1) as f32) / cols as f32).max(42.0);
    let cell_height = (cell_width / 1.9).clamp(48.0, 86.0);
    (cols, cell_width, cell_height)
}

fn cpu_page(app: &mut TaskManApp, ui: &mut egui::Ui, pal: &Palette) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    // Immutable extraction phase: everything the page renders is pulled out
    // of `app` up front so interaction callbacks can take &mut freely.
    let win = window(app);
    let ts = timestamps(win);
    let total_series = series(win, |h| h.cpu_total as f64);
    let kernel_series = series(win, |h| h.cpu_kernel as f64);
    let cores = snap.cpu.per_core_pct.len();
    let mut core_hist: Vec<Vec<f64>> = (0..cores).map(|_| Vec::with_capacity(win.len())).collect();
    let mut core_kern: Vec<Vec<f64>> = (0..cores).map(|_| Vec::with_capacity(win.len())).collect();
    for h in win {
        for (dst, v) in core_hist.iter_mut().zip(h.per_core.iter()) {
            dst.push(*v as f64);
        }
        for (dst, v) in core_kern.iter_mut().zip(h.per_core_kernel.iter()) {
            dst.push(*v as f64);
        }
    }
    let logical_mode = app.shared.settings.cpu_graph_mode == "logical";
    let kernels = app.shared.settings.show_kernel_times;

    page_title(ui, pal, "CPU", &snap.cpu.brand);

    caption(
        ui,
        pal,
        &format!(
            "{}, {}",
            i18n::tr(K::Utilization60sPct)
                .split(' ')
                .next()
                .unwrap_or(""),
            window_label(app.shared.settings.graph_seconds)
        ),
        "100 %",
    );

    let width = content_width(ui);

    if logical_mode && cores > 0 {
        // Responsive per-logical-processor grid. Four columns remains the
        // common 16-thread layout; high-core-count CPUs gain columns instead
        // of creating a needlessly tall page.
        let (cols, cell_w, cell_h) = logical_grid_layout(width, cores);
        let gap = 6.0;
        ui.horizontal_top(|ui| {
            ui.add_space(GUTTER);
            egui::Grid::new("core-grid")
                .spacing([gap, gap])
                .start_row(0)
                .show(ui, |ui| {
                    for i in 0..cores {
                        let response = core_chart(
                            ui,
                            egui::vec2(cell_w, cell_h),
                            &core_hist[i],
                            kernels.then_some(&core_kern[i]),
                            pal.cpu_graph,
                        );
                        cpu_graph_context_menu(app, &response);
                        if (i + 1) % cols == 0 {
                            ui.end_row();
                        }
                    }
                });
        });
    } else {
        // Overall utilization: one aggregate series (+ kernel overlay).
        // Kernel first so the primary total series paints on top.
        let mut chart_series = Vec::with_capacity(2);
        if kernels {
            chart_series.push(MultiSeries {
                samples: kernel_series,
                color: kernel_color(pal),
            });
        }
        chart_series.push(MultiSeries {
            samples: total_series,
            color: pal.cpu_graph,
        });
        let resp = page_chart(ui, width, 180.0, &chart_series, 100.0, Some(&ts));
        cpu_graph_context_menu(app, &resp);
    }

    ui.add_space(10.0);

    // Stats: left big values, right key/value list.
    stats_block(
        ui,
        490.0,
        |ui| {
            ui.vertical(|ui| {
                let w = 150.0;
                ui.horizontal_top(|ui| {
                    big_stat(
                        ui,
                        pal,
                        i18n::tr(K::StatUtilization),
                        &format::format_pct_hdr(snap.cpu.utilization_pct),
                        w,
                    );
                    big_stat(
                        ui,
                        pal,
                        i18n::tr(K::StatSpeed),
                        &format::format_freq_ghz(snap.cpu.freq_mhz),
                        w + 30.0,
                    );
                });
                ui.horizontal_top(|ui| {
                    med_stat(
                        ui,
                        pal,
                        i18n::tr(K::StatProcesses),
                        &snap.system.process_count.to_string(),
                        w,
                    );
                    med_stat(
                        ui,
                        pal,
                        i18n::tr(K::StatThreads),
                        &snap.system.thread_count.to_string(),
                        w,
                    );
                    med_stat(
                        ui,
                        pal,
                        i18n::tr(K::StatHandles),
                        &snap.system.handle_count.to_string(),
                        w,
                    );
                });
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::StatUptime),
                    &format::format_uptime(snap.system.uptime_s),
                    w + 60.0,
                );
            });
        },
        |ui| {
            ui.vertical(|ui| {
                let gb = |kb: u64| -> String {
                    if kb == 0 {
                        "\u{2014}".into()
                    } else {
                        format::format_bytes_loc(kb * 1024)
                    }
                };
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvBaseSpeed),
                    &if snap.cpu.freq_base_mhz > 0.0 {
                        format::format_freq_ghz(snap.cpu.freq_base_mhz)
                    } else {
                        "\u{2014}".into()
                    },
                );
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvSockets),
                    &snap.cpu.sockets.to_string(),
                );
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvCores),
                    &snap.cpu.physical_cores.to_string(),
                );
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvLogical),
                    &snap.cpu.logical_count.to_string(),
                );
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvVirtualization),
                    match snap.cpu.virtualization.as_str() {
                        "Enabled" => i18n::tr(K::VirtEnabled),
                        "Disabled" => i18n::tr(K::VirtDisabled),
                        other => other,
                    },
                );
                kv_row(ui, pal, "L1-Cache:", &gb(snap.cpu.l1_kb));
                kv_row(ui, pal, "L2-Cache:", &gb(snap.cpu.l2_kb));
                kv_row(ui, pal, "L3-Cache:", &gb(snap.cpu.l3_kb));
            });
        },
    );
    ui.add_space(16.0);
}

/// Right-click menu on the CPU graphs: change graph to overall/logical and
/// toggle the kernel-times overlay (§14.4).
fn cpu_graph_context_menu(app: &mut TaskManApp, resp: &egui::Response) {
    menu::context_menu(resp, |ui| {
        for (mode, key) in [
            ("overall", K::CpuGraphOverall),
            ("logical", K::CpuGraphLogical),
        ] {
            let current = app.shared.settings.cpu_graph_mode == mode;
            if menu::check(ui, i18n::tr(key), current).clicked() {
                app.shared.settings.cpu_graph_mode = mode.into();
                app.save_settings();
                ui.close();
            }
        }
        menu::separator(ui);
        let mut k = app.shared.settings.show_kernel_times;
        if menu::toggle(ui, i18n::tr(K::ShowKernelTimes), &mut k).changed() {
            app.shared.settings.show_kernel_times = k;
            app.save_settings();
        }
    });
}

/// Right-click menu on the GPU utilization graph: "change graph to", the way
/// Task Manager exposes the same choice.
///
/// The adapter's own utilization number is the BUSIEST engine, not a sum, so
/// "Overall" and a single engine are answers to different questions —
/// switching to Video Encode is the only way to see whether NVENC is busy
/// while the 3D engine is pinned.
fn gpu_graph_context_menu(app: &mut TaskManApp, resp: &egui::Response, engines: &[String]) {
    let current = app.shared.settings.gpu_graph_mode.clone();
    let mut chosen = None;
    menu::context_menu(resp, |ui| {
        menu::title(ui, i18n::tr(K::ChangeGraphTo));
        menu::separator(ui);
        if menu::check(ui, i18n::tr(K::CpuGraphOverall), current == "overall").clicked() {
            chosen = Some("overall".to_string());
            ui.close();
        }
        if menu::check(ui, i18n::tr(K::GpuAllEngines), current == "all").clicked() {
            chosen = Some("all".to_string());
            ui.close();
        }
        if !engines.is_empty() {
            menu::separator(ui);
        }
        for engine in engines {
            if menu::check(ui, &engine_label(engine), current == *engine).clicked() {
                chosen = Some(engine.clone());
                ui.close();
            }
        }
    });
    if let Some(mode) = chosen {
        app.shared.settings.gpu_graph_mode = mode;
        app.save_settings();
    }
}

// ---------------------------------------------------------------- Memory page

fn memory_page(app: &mut TaskManApp, ui: &mut egui::Ui, pal: &Palette) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    let total_gb = snap.memory.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    page_title(
        ui,
        pal,
        i18n::tr(K::MemTitle),
        &format!(
            "{}/{} ({})",
            format::format_bytes_loc(snap.memory.used_bytes),
            format::format_bytes_loc(snap.memory.total_bytes),
            format::format_pct_hdr(snap.memory.used_pct())
        ),
    );
    caption(
        ui,
        pal,
        i18n::tr(K::MemUsage60s),
        &format::format_bytes_loc(snap.memory.total_bytes),
    );

    let win = window(app);
    let ts = timestamps(win);
    let width = content_width(ui);
    let used: Vec<f64> = series(win, |h| h.mem_used_bytes as f64 / 1024.0 / 1024.0 / 1024.0);
    page_chart(
        ui,
        width,
        180.0,
        &[MultiSeries {
            samples: used,
            color: pal.memory_graph,
        }],
        total_gb.max(0.1),
        Some(&ts),
    );

    caption(
        ui,
        pal,
        i18n::tr(K::CommittedMem),
        &format::format_bytes_loc(snap.memory.commit_total_bytes),
    );
    let commit: Vec<f64> = series(win, |h| {
        h.commit_used_bytes as f64 / 1024.0 / 1024.0 / 1024.0
    });
    let commit_limit = snap.memory.commit_total_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    page_chart(
        ui,
        width,
        120.0,
        &[MultiSeries {
            samples: commit,
            color: pal.memory_graph.gamma_multiply(0.62),
        }],
        commit_limit.max(0.1),
        Some(&ts),
    );

    ui.add_space(10.0);
    let m = &snap.memory;
    stats_block(
        ui,
        590.0,
        |ui| {
            ui.vertical(|ui| {
                let w = 170.0;
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::StatInUse),
                    &format::format_bytes_loc(m.used_bytes),
                    w + 20.0,
                );
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::StatCommitted),
                    &format!(
                        "{}/{}",
                        format::format_bytes_loc(m.commit_used_bytes),
                        format::format_bytes_loc(m.commit_total_bytes)
                    ),
                    w + 60.0,
                );
                ui.horizontal_top(|ui| {
                    med_stat(
                        ui,
                        pal,
                        i18n::tr(K::StatCached),
                        &format::format_bytes_loc(m.cached_bytes),
                        w,
                    );
                    med_stat(
                        ui,
                        pal,
                        i18n::tr(K::StatPagedPool),
                        &format::format_bytes_loc(m.paged_pool_bytes),
                        w,
                    );
                    med_stat(
                        ui,
                        pal,
                        i18n::tr(K::StatNonPagedPool),
                        &format::format_bytes_loc(m.non_paged_pool_bytes),
                        w + 40.0,
                    );
                });
            });
        },
        |ui| {
            ui.vertical(|ui| {
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvTotal),
                    &format::format_bytes_loc(m.total_bytes),
                );
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvAvailable),
                    &format::format_bytes_loc(m.available_bytes),
                );
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvCommitLimit),
                    &format::format_bytes_loc(m.commit_total_bytes),
                );
                if m.swap_total_bytes > 0 {
                    kv_row(
                        ui,
                        pal,
                        i18n::tr(K::KvPagefile),
                        &format::format_bytes_loc(m.swap_used_bytes),
                    );
                }
                // Hardware facts (SMBIOS) — the original Task Manager's
                // Speed / Slots used / Form factor / Hardware reserved rows.
                if m.speed_mts > 0 {
                    kv_row(
                        ui,
                        pal,
                        i18n::tr(K::KvRamSpeed),
                        &format!("{} MT/s", m.speed_mts),
                    );
                }
                if m.slots_total > 0 {
                    kv_row(
                        ui,
                        pal,
                        i18n::tr(K::KvSlotsUsed),
                        &format!("{} / {}", m.slots_used, m.slots_total),
                    );
                }
                if !m.form_factor.is_empty() {
                    kv_row(ui, pal, i18n::tr(K::KvFormFactor), &m.form_factor);
                }
                if m.installed_bytes > 0 {
                    kv_row(
                        ui,
                        pal,
                        i18n::tr(K::KvHwReserved),
                        &format::format_bytes_loc(m.hw_reserved_bytes),
                    );
                }
            });
        },
    );
    ui.add_space(16.0);
}

// ---------------------------------------------------------------- Disk page

fn disk_page(app: &mut TaskManApp, ui: &mut egui::Ui, pal: &Palette, entry: &ResourceEntry) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    let Some(disk) = snap.disks.iter().find(|d| d.mount == entry.key) else {
        return;
    };
    page_title(ui, pal, &entry.title, &disk_media_label(disk));
    caption(ui, pal, i18n::tr(K::ActiveTime60s), "100 %");

    let win = window(app);
    let ts = timestamps(win);
    let width = content_width(ui);
    let active = disk_series(win, &entry.key, |d| d.1 as f64);
    page_chart(
        ui,
        width,
        160.0,
        &[MultiSeries {
            samples: active,
            color: pal.disk_graph,
        }],
        100.0,
        Some(&ts),
    );

    let read = disk_series(win, &entry.key, |d| d.2 / 1024.0);
    let write = disk_series(win, &entry.key, |d| d.3 / 1024.0);
    let peak = read
        .iter()
        .chain(write.iter())
        .cloned()
        .fold(0.0f64, f64::max);
    caption(
        ui,
        pal,
        i18n::tr(K::TransferRate60s),
        &format!("{:.0}", peak.max(1.0)),
    );
    page_chart(
        ui,
        width,
        160.0,
        &[
            MultiSeries {
                samples: read,
                color: pal.disk_graph,
            },
            MultiSeries {
                samples: write,
                color: pal.disk_graph.gamma_multiply(0.62),
            },
        ],
        peak.max(1.0),
        Some(&ts),
    );

    ui.add_space(10.0);
    stats_block(
        ui,
        560.0,
        |ui| {
            ui.vertical(|ui| {
                let w = 170.0;
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::StatActiveTime),
                    &format::format_pct_hdr(disk.active_pct),
                    w,
                );
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::StatRead),
                    &format::format_rate_mb(disk.read_bps),
                    w,
                );
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::StatWrite),
                    &format::format_rate_mb(disk.write_bps),
                    w,
                );
            });
        },
        |ui| {
            ui.vertical(|ui| {
                if disk.avg_resp_ms > 0.0 {
                    kv_row(
                        ui,
                        pal,
                        i18n::tr(K::KvAvgResponse),
                        &format!("{:.1} ms", disk.avg_resp_ms),
                    );
                }
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvCapacity),
                    &format::format_bytes_loc(disk.total_bytes),
                );
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvUsedSpace),
                    &format::format_bytes_loc(disk.total_bytes - disk.free_bytes),
                );
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvFreeSpace),
                    &format::format_bytes_loc(disk.free_bytes),
                );
            });
        },
    );
    ui.add_space(16.0);
}

// ---------------------------------------------------------------- Network page

fn network_page(app: &mut TaskManApp, ui: &mut egui::Ui, pal: &Palette, entry: &ResourceEntry) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    let Some(net) = snap.networks.iter().find(|n| n.name == entry.key) else {
        return;
    };
    page_title(ui, pal, &entry.title, &net.name);

    let win = window(app);
    let ts = timestamps(win);
    let width = content_width(ui);

    let recv = net_series(win, &entry.key, 1);
    let sent = net_series(win, &entry.key, 2);
    let peak = recv
        .iter()
        .chain(sent.iter())
        .copied()
        .fold(0.0f64, f64::max)
        .max(1.0);
    let window = window_label(app.shared.settings.graph_seconds);
    caption(
        ui,
        pal,
        &i18n::trf(K::ThroughputWindow, &[&window]),
        &format::format_mbit(peak),
    );
    page_chart(
        ui,
        width,
        230.0,
        &[
            MultiSeries {
                samples: recv,
                color: pal.network_graph,
            },
            MultiSeries {
                samples: sent,
                color: pal.network_graph.gamma_multiply(0.62),
            },
        ],
        peak,
        Some(&ts),
    );

    ui.add_space(10.0);
    stats_block(
        ui,
        400.0,
        |ui| {
            ui.vertical(|ui| {
                let w = 180.0;
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::StatReceive),
                    &format::format_kbit(net.recv_bps),
                    w,
                );
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::StatSend),
                    &format::format_kbit(net.sent_bps),
                    w,
                );
            });
        },
        |ui| {
            ui.vertical(|ui| {
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvTotalReceived),
                    &format::format_bytes_loc(net.total_recv_bytes),
                );
                kv_row(
                    ui,
                    pal,
                    i18n::tr(K::KvTotalSent),
                    &format::format_bytes_loc(net.total_sent_bytes),
                );
                if !net.desc.is_empty() {
                    kv_row(ui, pal, i18n::tr(K::KvAdapter), &net.desc);
                }
                if net.link_bps > 0 {
                    kv_row(
                        ui,
                        pal,
                        i18n::tr(K::KvLinkSpeed),
                        &format::format_mbit(net.link_bps as f64),
                    );
                }
                if let Some(ssid) = &net.ssid {
                    kv_row(ui, pal, "SSID:", ssid);
                }
                if let Some(signal) = net.signal_quality_pct {
                    kv_row(
                        ui,
                        pal,
                        i18n::tr(K::KvSignal),
                        &format!("{} %", signal.min(100)),
                    );
                }
                if let Some(ipv4) = &net.ipv4 {
                    kv_row(ui, pal, i18n::tr(K::KvIpv4), ipv4);
                }
                if let Some(ipv6) = &net.ipv6 {
                    kv_row(ui, pal, i18n::tr(K::KvIpv6), ipv6);
                }
            });
        },
    );
    ui.add_space(16.0);
}

// ---------------------------------------------------------------- GPU page

fn gpu_page(app: &mut TaskManApp, ui: &mut egui::Ui, pal: &Palette, entry: &ResourceEntry) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    let Some(gpu) = snap.gpus.iter().find(|g| g.id.to_string() == entry.key) else {
        return;
    };
    page_title(ui, pal, &entry.title, &gpu.name);

    let win = window(app);
    let ts = timestamps(win);
    let width = content_width(ui);

    // Which engine the top graph shows. An engine this adapter never reported
    // falls back to the overall (busiest-engine) view rather than drawing a
    // flat zero the user cannot tell from an idle engine.
    let engines = gpu_engine_names(win, &gpu.engines, &entry.key);
    let mode = app.shared.settings.gpu_graph_mode.clone();
    let mode = if mode == "overall" || mode == "all" || engines.contains(&mode) {
        mode
    } else {
        "overall".to_string()
    };

    let (series_list, title) = match mode.as_str() {
        "all" => (
            engines
                .iter()
                .enumerate()
                .map(|(index, name)| MultiSeries {
                    samples: gpu_engine_series(win, &entry.key, name),
                    // One hue, stepped in brightness: these are shares of the
                    // same adapter, and a second accent colour on this page
                    // would read as a different resource.
                    color: pal
                        .gpu_graph
                        .gamma_multiply(1.0 - 0.13 * (index.min(5) as f32)),
                })
                .collect::<Vec<_>>(),
            i18n::tr(K::GpuAllEngines).to_string(),
        ),
        "overall" => (
            vec![MultiSeries {
                samples: gpu_series(win, &entry.key, 1),
                color: pal.gpu_graph,
            }],
            i18n::tr(K::Utilization60sPct)
                .split(' ')
                .next()
                .unwrap_or("")
                .to_string(),
        ),
        engine => (
            vec![MultiSeries {
                samples: gpu_engine_series(win, &entry.key, engine),
                color: pal.gpu_graph,
            }],
            engine_label(engine),
        ),
    };
    caption(
        ui,
        pal,
        &format!(
            "{title}, {}",
            window_label(app.shared.settings.graph_seconds)
        ),
        "100 %",
    );
    // The menu mutates settings, so it runs once the history borrow that
    // built these series has ended — at the bottom of this function.
    let chart = page_chart(ui, width, 150.0, &series_list, 100.0, Some(&ts));

    let mem = gpu_series(win, &entry.key, 2);
    let mem_max = gpu
        .mem_total_bytes
        .max(mem.iter().cloned().fold(0.0f64, f64::max) as u64)
        .max(1);
    caption(
        ui,
        pal,
        i18n::tr(K::GpuMem60s),
        &format::format_bytes_loc(mem_max),
    );
    let mem_gb: Vec<f64> = mem.iter().map(|v| v / 1024.0 / 1024.0).collect();
    let max_gb = mem_max as f64 / 1024.0 / 1024.0;
    page_chart(
        ui,
        width,
        150.0,
        &[MultiSeries {
            samples: mem_gb,
            color: pal.gpu_graph.gamma_multiply(0.62),
        }],
        max_gb,
        Some(&ts),
    );

    ui.add_space(10.0);
    stats_block(
        ui,
        400.0,
        |ui| {
            ui.vertical(|ui| {
                let w = 170.0;
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::StatUtilization),
                    &format::format_pct_hdr(gpu.util_pct),
                    w,
                );
                big_stat(
                    ui,
                    pal,
                    i18n::tr(K::GpuMemStat),
                    &format::format_bytes_loc(gpu.mem_used_bytes),
                    w,
                );
            });
        },
        |ui| {
            ui.vertical(|ui| {
                if gpu.mem_total_bytes > 0 {
                    kv_row(
                        ui,
                        pal,
                        i18n::tr(K::KvDedicatedMem),
                        &format::format_bytes_loc(gpu.mem_total_bytes),
                    );
                }
                if let Some(t) = gpu.temperature_c {
                    kv_row(ui, pal, i18n::tr(K::KvTemperature), &format!("{t:.0} °C"));
                }
                if !gpu.driver_version.is_empty() {
                    kv_row(ui, pal, i18n::tr(K::KvDriverVersion), &gpu.driver_version);
                }
                for e in gpu.engines.iter().take(4) {
                    kv_row(
                        ui,
                        pal,
                        &format!("{} {}:", i18n::tr(K::KvEnginePrefix), engine_label(&e.name)),
                        &format::format_pct_hdr(e.util_pct),
                    );
                }
            });
        },
    );
    gpu_graph_context_menu(app, &chart, &engines);
    ui.add_space(16.0);
}

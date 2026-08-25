//! Performance tab: device cards with mini graphs on the left, TM-style
//! detail pages on the right (per-core grid for CPU, big rolling charts,
//! big-value stats + key/value list below).

use eframe::egui::{self, Align2, CursorIcon, FontId, Pos2};
use tm_core::format;
use tm_core::i18n::{self, K};

use crate::app::{HistoryPoint, TaskManApp};
use crate::theme::{self, Palette};
use crate::widgets::chart::{MultiSeries, chart_multi, core_chart};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Cpu,
    Memory,
    Disk,
    Network,
    Gpu,
}

#[derive(Clone)]
pub struct ResourceEntry {
    pub kind: ResourceKind,
    pub key: String,
    pub title: String,
    pub subtitle: String,
    pub value_line: String,
}

fn visible_points(app: &TaskManApp) -> usize {
    app.shared.settings.graph_seconds.clamp(10, 240) as usize
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
        |_app, ui| {
            if ui.button(i18n::tr(K::RefreshNow)).clicked() {
                ui.close();
            }
        },
    );

    let entries = build_resource_list(app);
    if app.perf_selected >= entries.len() {
        app.perf_selected = 0;
    }

    ui.horizontal_top(|ui| {
        // ---------------- left column of cards (user-resizable) -------------
        let mut card_w = app.shared.settings.perf_card_width.clamp(180.0, 520.0);
        egui::ScrollArea::vertical()
            .id_salt("perf-cards")
            .show(ui, |ui| {
                ui.set_width(card_w);
                ui.vertical(|ui| {
                    for (i, e) in entries.iter().enumerate() {
                        card_ui(app, ui, &pal, i, e);
                    }
                    ui.add_space(8.0);
                });
            });

        // Drag splitter between the card column and the detail area.
        let split_h = ui.available_height();
        let (srect, sresp) =
            ui.allocate_exact_size(egui::vec2(10.0, split_h), egui::Sense::drag());
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
        if sresp.drag_stopped() {
            app.shared.settings.save();
        }

        ui.add_space(6.0);

        // ---------------- right detail area ---------------------------------
        if let Some(sel) = entries.get(app.perf_selected).cloned() {
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

fn card_ui(app: &mut TaskManApp, ui: &mut egui::Ui, pal: &Palette, i: usize, e: &ResourceEntry) {
    let size = egui::vec2(ui.available_width(), 64.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let selected = i == app.perf_selected;
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
        app.perf_selected = i;
    }

    // Mini graph on the left, painted into the card rect.
    let chart_rect = egui::Rect::from_min_size(
        Pos2::new(rect.left() + 10.0, rect.top() + 12.0),
        egui::vec2(62.0, 40.0),
    );
    let n = visible_points(app);
    let color = pal.accent;
    let samples = match e.kind {
        ResourceKind::Cpu => series(app, n, |h| h.cpu_total as f64),
        ResourceKind::Memory => series(app, n, |h| pct_of(h.mem_used_bytes, h.mem_total_bytes)),
        ResourceKind::Disk => disk_series(app, &e.key, n, |d| d.1 as f64),
        ResourceKind::Network => net_series(app, &e.key, n, 1),
        ResourceKind::Gpu => gpu_series(app, &e.key, n, 1),
    };
    crate::widgets::chart::paint_sparkline(ui, chart_rect, &samples, color);

    // Text block: title / subtitle / value.
    let tx = rect.left() + 84.0;
    ui.painter().text(
        Pos2::new(tx, rect.top() + 13.0),
        Align2::LEFT_CENTER,
        &e.title,
        FontId::proportional(13.5),
        pal.text,
    );
    if !e.subtitle.is_empty() {
        ui.painter().text(
            Pos2::new(tx, rect.top() + 30.0),
            Align2::LEFT_CENTER,
            &e.subtitle,
            FontId::proportional(11.0),
            pal.text_dim,
        );
    }
    ui.painter().text(
        Pos2::new(tx, rect.top() + 47.0),
        Align2::LEFT_CENTER,
        &e.value_line,
        FontId::proportional(11.0),
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
        out.push(ResourceEntry {
            kind: ResourceKind::Network,
            key: n.name.clone(),
            title: if n.kind.is_empty() {
                n.name.clone()
            } else {
                n.kind.clone()
            },
            subtitle: if n.kind.is_empty() {
                String::new()
            } else {
                n.name.clone()
            },
            value_line: i18n::trf(
                K::CardSentRecv,
                &[&format::format_kbit(n.sent_bps), &format::format_kbit(n.recv_bps)],
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

/// Iterator over the newest `n` history points without cloning them.
fn tail(app: &TaskManApp, n: usize) -> impl DoubleEndedIterator<Item = &HistoryPoint> {
    let skip = app.history.len().saturating_sub(n);
    app.history.iter().skip(skip)
}

/// Extract one numeric series from the newest `n` points.
fn series(app: &TaskManApp, n: usize, f: impl Fn(&HistoryPoint) -> f64) -> Vec<f64> {
    tail(app, n).map(f).collect()
}

fn disk_series(
    app: &TaskManApp,
    key: &str,
    n: usize,
    pick: impl Fn(&(String, f32, f64, f64)) -> f64,
) -> Vec<f64> {
    tail(app, n)
        .filter_map(|h| h.disks.iter().find(|(m, ..)| m == key))
        .map(pick)
        .collect()
}

fn net_series(app: &TaskManApp, key: &str, n: usize, idx: usize) -> Vec<f64> {
    tail(app, n)
        .filter_map(|h| h.nets.iter().find(|(m, ..)| m == key))
        .filter(|t| t.1 > 0.0 || t.2 > 0.0)
        .map(|t| if idx == 1 { t.1 } else { t.2 })
        .collect()
}

fn gpu_series(app: &TaskManApp, key: &str, n: usize, idx: usize) -> Vec<f64> {
    tail(app, n)
        .filter_map(|h| h.gpus.iter().find(|(id, ..)| id.to_string() == key))
        .map(|t| if idx == 1 { t.1 as f64 } else { t.2 as f64 })
        .collect()
}

/// Page title row: big name left, detail right.
fn page_title(ui: &mut egui::Ui, pal: &Palette, title: &str, right: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left() + 16.0, rect.center().y),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(26.0),
        pal.text,
    );
    ui.painter().text(
        Pos2::new(rect.right() - 16.0, rect.center().y),
        Align2::RIGHT_CENTER,
        right,
        FontId::proportional(14.0),
        pal.text,
    );
}

/// Caption row: dim caption left, scale max right.
fn caption(ui: &mut egui::Ui, pal: &Palette, left: &str, right: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left() + 16.0, rect.center().y),
        Align2::LEFT_CENTER,
        left,
        FontId::proportional(11.5),
        pal.text_dim,
    );
    ui.painter().text(
        Pos2::new(rect.right() - 16.0, rect.center().y),
        Align2::RIGHT_CENTER,
        right,
        FontId::proportional(11.5),
        pal.text_dim,
    );
}

/// Big-value stat (label above, large number below).
fn big_stat(ui: &mut egui::Ui, pal: &Palette, label: &str, value: &str, w: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 46.0), egui::Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left(), rect.top() + 8.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(11.5),
        pal.text_dim,
    );
    ui.painter().text(
        Pos2::new(rect.left(), rect.bottom() - 8.0),
        Align2::LEFT_CENTER,
        value,
        FontId::proportional(21.0),
        pal.text,
    );
}

/// Medium stat (label above, medium number below).
fn med_stat(ui: &mut egui::Ui, pal: &Palette, label: &str, value: &str, w: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 40.0), egui::Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left(), rect.top() + 8.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(11.5),
        pal.text_dim,
    );
    ui.painter().text(
        Pos2::new(rect.left(), rect.bottom() - 8.0),
        Align2::LEFT_CENTER,
        value,
        FontId::proportional(16.0),
        pal.text,
    );
}

/// Key/value row for the right-hand details list.
fn kv_row(ui: &mut egui::Ui, pal: &Palette, key: &str, value: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 21.0), egui::Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        key,
        FontId::proportional(12.0),
        pal.text_dim,
    );
    ui.painter().text(
        Pos2::new(rect.left() + 190.0, rect.center().y),
        Align2::LEFT_CENTER,
        value,
        FontId::proportional(12.0),
        pal.text,
    );
}

fn content_width(ui: &egui::Ui) -> f32 {
    ui.available_width() - 32.0
}

// ---------------------------------------------------------------- CPU page

fn cpu_page(app: &mut TaskManApp, ui: &mut egui::Ui, pal: &Palette) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    page_title(ui, pal, "CPU", &snap.cpu.brand);
    caption(ui, pal, i18n::tr(K::Utilization60sPct), "100 %");

    let n = visible_points(app);
    let width = content_width(ui);

    // Per-logical-processor grid: 4 columns like TM.
    let cores = snap.cpu.per_core_pct.len();
    if cores > 0 {
        let cols = 4usize;
        let gap = 6.0;
        let cell_w = ((width - 32.0 - gap * (cols - 1) as f32) / cols as f32).max(60.0);
        let cell_h = 84.0;
        // One pass over the history tail fills every core's series.
        let mut history: Vec<Vec<f64>> = (0..cores).map(|_| Vec::with_capacity(n)).collect();
        for h in tail(app, n) {
            for (dst, v) in history.iter_mut().zip(h.per_core.iter()) {
                dst.push(*v as f64);
            }
        }
        egui::Grid::new("core-grid")
            .spacing([gap, gap])
            .start_row(0)
            .show(ui, |ui| {
                for (i, samples) in history.iter().enumerate() {
                    core_chart(ui, egui::vec2(cell_w, cell_h), samples, pal.accent);
                    if (i + 1) % cols == 0 {
                        ui.end_row();
                    }
                }
            });
    }

    ui.add_space(10.0);

    // Stats: left big values, right key/value list.
    ui.horizontal_top(|ui| {
        ui.add_space(16.0);
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

        ui.add_space(30.0);
        ui.vertical(|ui| {
            let gb = |kb: u64| -> String {
                if kb == 0 {
                    "—".into()
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
                    "—".into()
                },
            );
            kv_row(ui, pal, i18n::tr(K::KvSockets), &snap.cpu.sockets.to_string());
            kv_row(ui, pal, i18n::tr(K::KvCores), &snap.cpu.physical_cores.to_string());
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
    });
    ui.add_space(16.0);
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

    let n = visible_points(app);
    let width = content_width(ui);
    let used: Vec<f64> = series(app, n, |h| {
        h.mem_used_bytes as f64 / 1024.0 / 1024.0 / 1024.0
    });
    chart_multi(
        ui,
        egui::vec2(width - 32.0, 180.0),
        &[MultiSeries {
            samples: used,
            color: pal.accent,
        }],
        total_gb.max(0.1),
    );

    caption(
        ui,
        pal,
        i18n::tr(K::CommittedMem),
        &format::format_bytes_loc(snap.memory.commit_total_bytes),
    );
    let commit: Vec<f64> = series(app, n, |h| {
        h.commit_used_bytes as f64 / 1024.0 / 1024.0 / 1024.0
    });
    let commit_limit = snap.memory.commit_total_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    chart_multi(
        ui,
        egui::vec2(width - 32.0, 120.0),
        &[MultiSeries {
            samples: commit,
            color: theme::DARK.ok_green,
        }],
        commit_limit.max(0.1),
    );

    ui.add_space(10.0);
    let m = &snap.memory;
    ui.horizontal_top(|ui| {
        ui.add_space(16.0);
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
        ui.add_space(30.0);
        ui.vertical(|ui| {
            kv_row(ui, pal, i18n::tr(K::KvTotal), &format::format_bytes_loc(m.total_bytes));
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
        });
    });
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

    let n = visible_points(app);
    let width = content_width(ui);
    let active = disk_series(app, &entry.key, n, |d| d.1 as f64);
    chart_multi(
        ui,
        egui::vec2(width - 32.0, 160.0),
        &[MultiSeries {
            samples: active,
            color: pal.accent,
        }],
        100.0,
    );

    let read = disk_series(app, &entry.key, n, |d| d.2 / 1024.0);
    let write = disk_series(app, &entry.key, n, |d| d.3 / 1024.0);
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
    chart_multi(
        ui,
        egui::vec2(width - 32.0, 160.0),
        &[
            MultiSeries {
                samples: read,
                color: pal.accent,
            },
            MultiSeries {
                samples: write,
                color: theme::DARK.ok_green,
            },
        ],
        peak.max(1.0),
    );

    ui.add_space(10.0);
    ui.horizontal_top(|ui| {
        ui.add_space(16.0);
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
        ui.add_space(30.0);
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
    });
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

    let n = visible_points(app);
    let width = content_width(ui);

    let recv = net_series(app, &entry.key, n, 1);
    let r_max = recv.iter().cloned().fold(0.0f64, f64::max).max(1.0);
    caption(
        ui,
        pal,
        i18n::tr(K::Receive60s),
        &format!("{:.1}", r_max * 8.0 / 1024.0),
    );
    chart_multi(
        ui,
        egui::vec2(width - 32.0, 150.0),
        &[MultiSeries {
            samples: recv,
            color: pal.accent,
        }],
        r_max,
    );

    let sent = net_series(app, &entry.key, n, 2);
    let s_max = sent.iter().cloned().fold(0.0f64, f64::max).max(1.0);
    caption(
        ui,
        pal,
        i18n::tr(K::Send60s),
        &format!("{:.1}", s_max * 8.0 / 1024.0),
    );
    chart_multi(
        ui,
        egui::vec2(width - 32.0, 150.0),
        &[MultiSeries {
            samples: sent,
            color: theme::DARK.ok_green,
        }],
        s_max,
    );

    ui.add_space(10.0);
    ui.horizontal_top(|ui| {
        ui.add_space(16.0);
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
        ui.add_space(30.0);
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
        });
    });
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

    let n = visible_points(app);
    let width = content_width(ui);

    let util = gpu_series(app, &entry.key, n, 1);
    caption(ui, pal, i18n::tr(K::Utilization60sPct), "100 %");
    chart_multi(
        ui,
        egui::vec2(width - 32.0, 150.0),
        &[MultiSeries {
            samples: util,
            color: pal.accent,
        }],
        100.0,
    );

    let mem = gpu_series(app, &entry.key, n, 2);
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
    chart_multi(
        ui,
        egui::vec2(width - 32.0, 150.0),
        &[MultiSeries {
            samples: mem_gb,
            color: theme::DARK.ok_green,
        }],
        max_gb,
    );

    ui.add_space(10.0);
    ui.horizontal_top(|ui| {
        ui.add_space(16.0);
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
        ui.add_space(30.0);
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
                    &format!("{} {}:", i18n::tr(K::KvEnginePrefix), e.name),
                    &format::format_pct_hdr(e.util_pct),
                );
            }
        });
    });
    ui.add_space(16.0);
}

//! Performance tab: resource cards on the left, big rolling charts on the
//! right, per-logical-processor grid for CPU — mirrors Win11 Task Manager.

use eframe::egui;
use tm_core::format;

use crate::app::TaskManApp;
use crate::theme;
use crate::widgets::chart::LineChart;

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
    /// mount / adapter name / gpu id as applicable
    pub key: String,
    pub title: String,
    #[allow(dead_code)]
    pub subtitle: String,
    pub value_line: String,
}

/// How many history points to show given settings (samples are 1 Hz-ish).
fn visible_points(app: &TaskManApp) -> usize {
    app.shared.settings.graph_seconds.clamp(10, 240) as usize
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let Some(_snap) = app.latest_snapshot() else {
        ui.centered_and_justified(|ui| ui.label("Sammle Daten…"));
        return;
    };

    // Build the left resource list from latest snapshot + history.
    let entries = build_resource_list(app);
    if app.perf_selected >= entries.len() {
        app.perf_selected = 0;
    }

    ui.horizontal_top(|ui| {
        // ---------------- left column of cards ------------------------------
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.set_width(230.0);
            for (i, e) in entries.iter().enumerate() {
                let selected = i == app.perf_selected;
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 52.0),
                    egui::Sense::click(),
                );
                if resp.clicked() {
                    app.perf_selected = i;
                }
                let bg = if selected {
                    pal.accent.gamma_multiply(0.20)
                } else if resp.hovered() {
                    pal.card_bg_hover
                } else {
                    pal.card_bg
                };
                ui.painter().rect_filled(rect, 6.0, bg);
                if selected {
                    ui.painter().rect_stroke(
                        rect,
                        6.0,
                        egui::Stroke::new(1.2, pal.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                let icon = match e.kind {
                    ResourceKind::Cpu => crate::icons::Icon::Cpu,
                    ResourceKind::Memory => crate::icons::Icon::Memory,
                    ResourceKind::Disk => crate::icons::Icon::Disk,
                    ResourceKind::Network => crate::icons::Icon::Network,
                    ResourceKind::Gpu => crate::icons::Icon::Gpu,
                };
                crate::icons::draw_at(
                    ui,
                    egui::Rect::from_center_size(
                        rect.left_center() + egui::vec2(18.0, 0.0),
                        egui::vec2(22.0, 22.0),
                    ),
                    icon,
                    pal.text_dim,
                );
                let text_x = rect.left() + 40.0;
                ui.painter().text(
                    egui::pos2(text_x, rect.top() + 12.0),
                    egui::Align2::LEFT_CENTER,
                    &e.title,
                    egui::FontId::proportional(13.5),
                    pal.text,
                );
                ui.painter().text(
                    egui::pos2(text_x, rect.bottom() - 12.0),
                    egui::Align2::LEFT_CENTER,
                    &e.value_line,
                    egui::FontId::proportional(11.5),
                    pal.text_dim,
                );
            }
            ui.add_space(8.0);
        });

        ui.separator();

        // ---------------- right detail area ---------------------------------
        if let Some(sel) = entries.get(app.perf_selected).cloned() {
            egui::ScrollArea::vertical()
                .id_salt("perf-detail")
                .show(ui, |ui| {
                    match sel.kind {
                        ResourceKind::Cpu => cpu_detail(app, ui, &sel),
                        ResourceKind::Memory => memory_detail(app, ui, &sel),
                        ResourceKind::Disk => disk_detail(app, ui, &sel),
                        ResourceKind::Network => network_detail(app, ui, &sel),
                        ResourceKind::Gpu => gpu_detail(app, ui, &sel),
                    }
                    let _ = pal;
                });
        }
    });
}

fn build_resource_list(app: &TaskManApp) -> Vec<ResourceEntry> {
    let mut out = Vec::new();
    let Some(snap) = app.latest_snapshot() else {
        return out;
    };

    // CPU.
    out.push(ResourceEntry {
        kind: ResourceKind::Cpu,
        key: "cpu".into(),
        title: "CPU".into(),
        subtitle: snap.cpu.brand.clone(),
        value_line: format!(
            "{:.0} %  {:.2} GHz",
            snap.cpu.utilization_pct,
            snap.cpu.freq_mhz / 1000.0
        ),
    });

    // Memory.
    let used_gb = snap.memory.used_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    let total_gb = snap.memory.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    out.push(ResourceEntry {
        kind: ResourceKind::Memory,
        key: "mem".into(),
        title: "Arbeitsspeicher".into(),
        subtitle: format!("{used_gb:.1}/{total_gb:.1} GB"),
        value_line: format!("{:.0} %", snap.memory.used_pct()),
    });

    // Disks.
    for d in &snap.disks {
        let label = if d.label.is_empty() {
            d.mount.clone()
        } else {
            d.label.clone()
        };
        out.push(ResourceEntry {
            kind: ResourceKind::Disk,
            key: d.mount.clone(),
            title: format!("Datenträger {}", d.id),
            subtitle: media_label(d),
            value_line: format!("{:.0} % aktiv", d.active_pct),
        });
        let _ = label;
    }

    // Networks.
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
            subtitle: n.name.clone(),
            value_line: format!(
                "S: {}  E: {}",
                format::format_rate_short(n.sent_bps),
                format::format_rate_short(n.recv_bps)
            ),
        });
    }

    // GPUs.
    for g in &snap.gpus {
        out.push(ResourceEntry {
            kind: ResourceKind::Gpu,
            key: g.id.to_string(),
            title: format!("GPU {}", g.id),
            subtitle: g.name.clone(),
            value_line: format!(
                "{:.0} %{}",
                g.util_pct,
                g.temperature_c
                    .map(|t| format!("  {t:.0} °C"))
                    .unwrap_or_default()
            ),
        });
    }
    out
}

fn media_label(d: &tm_core::model::DiskInfo) -> String {
    let mut s = d.media.label().to_string();
    if s.is_empty() {
        s = d.mount.clone();
    }
    s
}

// ------------------------------------------------------------------ details

fn header(ui: &mut egui::Ui, icon: crate::icons::Icon, title: &str, sub: &str) {
    let pal = theme::palette(ui);
    ui.horizontal(|ui| {
        crate::icons::draw_at(
            ui,
            egui::Rect::from_center_size(
                ui.cursor().left_center() + egui::vec2(14.0, 0.0),
                egui::vec2(26.0, 26.0),
            ),
            icon,
            pal.accent,
        );
        ui.add_space(30.0);
        ui.vertical(|ui| {
            ui.heading(egui::RichText::new(title).size(19.0));
            ui.label(egui::RichText::new(sub).size(12.0).color(pal.text_dim));
        });
    });
    ui.add_space(8.0);
}

fn stat_cell(ui: &mut egui::Ui, label: &str, value: &str) {
    let pal = theme::palette(ui);
    ui.vertical(|ui| {
        ui.set_min_width(120.0);
        ui.label(egui::RichText::new(label).size(11.5).color(pal.text_dim));
        ui.label(egui::RichText::new(value).size(15.0));
    });
}

fn slice_last<T>(v: &[T], n: usize) -> &[T] {
    if v.len() <= n { v } else { &v[v.len() - n..] }
}

fn hist_vec(app: &TaskManApp) -> std::vec::Vec<crate::app::HistoryPoint> {
    app.history.iter().cloned().collect()
}

/// Take the most recent `n` points as owned vec.
fn recent(app: &TaskManApp, n: usize) -> std::vec::Vec<crate::app::HistoryPoint> {
    let mut v = hist_vec(app);
    if v.len() <= n {
        v
    } else {
        v.split_off(v.len() - n)
    }
}

fn accent_color(ui: &egui::Ui) -> eframe::egui::Color32 {
    theme::palette_ctx(&ui.ctx().clone()).accent
}

// ---- CPU ---------------------------------------------------------------

fn cpu_detail(app: &mut TaskManApp, ui: &mut egui::Ui, entry: &ResourceEntry) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    header(ui, crate::icons::Icon::Cpu, "CPU", &snap.cpu.brand);

    let n = visible_points(app);
    let samples: Vec<f64> = recent(app, n).iter().map(|h| h.cpu_total as f64).collect();

    LineChart::new(&samples, accent_color(ui), |v| format!("{v:.0} %"))
        .show_sized(ui, egui::vec2(ui.available_width(), 200.0));

    ui.add_space(8.0);

    // Per-logical-processor mini grid.
    ui.label(
        egui::RichText::new("Logische Prozessoren")
            .size(13.0)
            .color(theme::palette(ui).text_dim),
    );
    let cores = app.history.back().map_or(0, |h| h.per_core.len());
    let cols = ((ui.available_width() / 110.0).floor() as usize).clamp(4, 8);
    let rows = cores.div_ceil(cols.max(1));
    egui::Grid::new("core-grid")
        .spacing([6.0, 6.0])
        .show(ui, |ui| {
            for r in 0..rows {
                for c in 0..cols {
                    let idx = r * cols + c;
                    if idx >= cores {
                        continue;
                    }
                    let samples: Vec<f64> = recent(app, n)
                        .iter()
                        .filter_map(|h| h.per_core.get(idx).map(|v| *v as f64))
                        .collect();
                    crate::widgets::chart::mini_chart(
                        ui,
                        egui::vec2(96.0, 56.0),
                        &samples,
                        accent_color(ui),
                    );
                }
                ui.end_row();
            }
        });

    ui.add_space(10.0);
    stats_grid(ui, |ui| {
        stat_cell(
            ui,
            "Auslastung",
            &format!("{:.0} %", snap.cpu.utilization_pct),
        );
        stat_cell(
            ui,
            "Geschwindigkeit",
            &format!("{:.2} GHz", snap.cpu.freq_mhz / 1000.0),
        );
        stat_cell(
            ui,
            "Basisgeschwindigkeit",
            &if snap.cpu.freq_base_mhz > 0.0 {
                format!("{:.2} GHz", snap.cpu.freq_base_mhz / 1000.0)
            } else {
                "—".into()
            },
        );
        stat_cell(ui, "Prozesse", &snap.system.process_count.to_string());
        stat_cell(ui, "Threads", &snap.system.thread_count.to_string());
        stat_cell(ui, "Handles", &snap.system.handle_count.to_string());
        stat_cell(
            ui,
            "Betriebszeit",
            &format::format_uptime(snap.system.uptime_s),
        );
        stat_cell(ui, "Sockets", &snap.cpu.sockets.to_string());
        stat_cell(ui, "Kerne", &snap.cpu.physical_cores.to_string());
        stat_cell(
            ui,
            "Logische Prozessoren",
            &snap.cpu.logical_count.to_string(),
        );
        stat_cell(ui, "Virtualisierung", &snap.cpu.virtualization);
        cache_cells(ui, &snap.cpu);
    });
    let _ = entry;
}

fn cache_cells(ui: &mut egui::Ui, cpu: &tm_core::model::CpuInfo) {
    let kb = |kb: u64| -> String {
        if kb == 0 {
            "—".into()
        } else {
            format::format_bytes(kb * 1024)
        }
    };
    stat_cell(ui, "L1-Cache", &kb(cpu.l1_kb));
    stat_cell(ui, "L2-Cache", &kb(cpu.l2_kb));
    stat_cell(ui, "L3-Cache", &kb(cpu.l3_kb));
}

// ---- Memory -------------------------------------------------------------

fn memory_detail(app: &mut TaskManApp, ui: &mut egui::Ui, _entry: &ResourceEntry) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    header(ui, crate::icons::Icon::Memory, "Arbeitsspeicher", "");

    let used = format::format_bytes(snap.memory.used_bytes);
    let total = format::format_bytes(snap.memory.total_bytes);
    ui.label(format!("{used} von {total}"));

    let n = visible_points(app);
    let used_samples: Vec<f64> = recent(app, n)
        .iter()
        .map(|h| bytes_to_g(h.mem_used_bytes))
        .collect();
    let commit_samples: Vec<f64> = recent(app, n)
        .iter()
        .map(|h| bytes_to_g(h.commit_used_bytes))
        .collect();
    let max_used = recent(app, n)
        .iter()
        .map(|h| bytes_to_g(h.mem_total_bytes))
        .fold(0.0f64, f64::max);

    LineChart::new(&used_samples, accent_color(ui), gb_fmt)
        .y_max(max_used)
        .show_sized(ui, egui::vec2(ui.available_width(), 170.0));
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Zugesicherter Speicher")
            .size(12.0)
            .color(theme::palette(ui).text_dim),
    );
    let commit_max = recent(app, n)
        .iter()
        .map(|h| bytes_to_g(h.commit_limit_bytes))
        .fold(0.0f64, f64::max)
        .max(commit_samples.iter().cloned().fold(0.0, f64::max));
    LineChart::new(&commit_samples, theme::palette(ui).ok_green, gb_fmt)
        .y_max(commit_max.max(1.0))
        .grid(true)
        .show_sized(ui, egui::vec2(ui.available_width(), 130.0));

    ui.add_space(10.0);
    let m = &snap.memory;
    stats_grid(ui, |ui| {
        stat_cell(ui, "In Verwendung", &used);
        stat_cell(ui, "Verfügbar", &format::format_bytes(m.available_bytes));
        stat_cell(
            ui,
            "Zwischengespeichert",
            &format::format_bytes(m.cached_bytes),
        );
        stat_cell(
            ui,
            "Zugesichert",
            &format!(
                "{}/{}",
                format::format_bytes(m.commit_used_bytes),
                format::format_bytes(m.commit_total_bytes)
            ),
        );
        stat_cell(
            ui,
            "Ausgelagerter Pool",
            &format::format_bytes(m.paged_pool_bytes),
        );
        stat_cell(
            ui,
            "Nicht ausgelagerter Pool",
            &format::format_bytes(m.non_paged_pool_bytes),
        );
        if m.swap_total_bytes > 0 {
            stat_cell(
                ui,
                "Auslagerungsdatei",
                &format::format_bytes(m.swap_used_bytes),
            );
        }
        stat_cell(ui, "Gesamt", &total);
    });
}

fn bytes_to_g(b: u64) -> f64 {
    b as f64 / 1024.0 / 1024.0 / 1024.0
}
fn gb_fmt(v: f64) -> String {
    format!("{v:.1} GB")
}

// ---- Disk ---------------------------------------------------------------

fn disk_detail(app: &mut TaskManApp, ui: &mut egui::Ui, entry: &ResourceEntry) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    let Some(disk) = snap.disks.iter().find(|d| d.mount == entry.key) else {
        return;
    };
    header(
        ui,
        crate::icons::Icon::Disk,
        &entry.title,
        &media_label(disk),
    );

    let n = visible_points(app);
    let active: Vec<f64> = app
        .history
        .iter()
        .filter(|h| h.disks.iter().any(|(m, ..)| m == &entry.key))
        .map(|h| h.disks.iter().find(|(m, ..)| m == &entry.key).unwrap().1 as f64)
        .collect();
    let read: Vec<f64> = app
        .history
        .iter()
        .filter(|h| h.disks.iter().any(|(m, ..)| m == &entry.key))
        .map(|h| h.disks.iter().find(|(m, ..)| m == &entry.key).unwrap().2 / 1024.0)
        .collect();
    let write: Vec<f64> = app
        .history
        .iter()
        .filter(|h| h.disks.iter().any(|(m, ..)| m == &entry.key))
        .map(|h| h.disks.iter().find(|(m, ..)| m == &entry.key).unwrap().3 / 1024.0)
        .collect();
    let active = slice_last(&active, n).to_vec();
    let read = slice_last(&read, n).to_vec();
    let write = slice_last(&write, n).to_vec();

    LineChart::new(&active, accent_color(ui), |v| format!("{v:.0} %"))
        .y_max(100.0)
        .show_sized(ui, egui::vec2(ui.available_width(), 140.0));
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Übertragungsrate (KB/s)")
            .size(12.0)
            .color(theme::palette(ui).text_dim),
    );
    let peak = read.iter().chain(write.iter()).cloned().fold(0.0, f64::max);
    LineChart::new(&read, accent_color(ui), kb_fmt)
        .y_max(peak.max(1.0))
        .show_sized(ui, egui::vec2(ui.available_width(), 90.0));
    LineChart::new(&write, theme::palette(ui).ok_green, kb_fmt)
        .y_max(peak.max(1.0))
        .show_sized(ui, egui::vec2(ui.available_width(), 90.0));

    ui.add_space(10.0);
    stats_grid(ui, |ui| {
        stat_cell(ui, "Aktiv", &format!("{:.0} %", disk.active_pct));
        stat_cell(ui, "Lesen", &format::format_rate(disk.read_bps));
        stat_cell(ui, "Schreiben", &format::format_rate(disk.write_bps));
        if disk.avg_resp_ms > 0.0 {
            stat_cell(
                ui,
                "Durchschn. Reaktionszeit",
                &format!("{:.1} ms", disk.avg_resp_ms),
            );
        }
        stat_cell(ui, "Kapazität", &format::format_bytes(disk.total_bytes));
        stat_cell(
            ui,
            "Belegt",
            &format::format_bytes(disk.total_bytes - disk.free_bytes),
        );
        stat_cell(ui, "Frei", &format::format_bytes(disk.free_bytes));
    });
}

fn kb_fmt(v: f64) -> String {
    format!("{v:.0} KB/s")
}

// ---- Network ------------------------------------------------------------

fn network_detail(app: &mut TaskManApp, ui: &mut egui::Ui, entry: &ResourceEntry) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    let Some(net) = snap.networks.iter().find(|n| n.name == entry.key) else {
        return;
    };
    header(ui, crate::icons::Icon::Network, &entry.title, &net.name);

    let n = visible_points(app);
    let recv: Vec<f64> = app
        .history
        .iter()
        .filter(|h| h.nets.iter().any(|(m, ..)| m == &entry.key))
        .map(|h| h.nets.iter().find(|(m, ..)| m == &entry.key).unwrap().1)
        .collect();
    let sent: Vec<f64> = app
        .history
        .iter()
        .filter(|h| h.nets.iter().any(|(m, ..)| m == &entry.key))
        .map(|h| h.nets.iter().find(|(m, ..)| m == &entry.key).unwrap().2)
        .collect();
    let recv = slice_last(&recv, n).to_vec();
    let sent = slice_last(&sent, n).to_vec();

    ui.label(
        egui::RichText::new("Empfangen")
            .size(12.0)
            .color(theme::palette(ui).text_dim),
    );
    LineChart::new(&recv, accent_color(ui), tm_core::format::format_rate)
        .show_sized(ui, egui::vec2(ui.available_width(), 130.0));
    ui.label(
        egui::RichText::new("Gesendet")
            .size(12.0)
            .color(theme::palette(ui).text_dim),
    );
    LineChart::new(
        &sent,
        theme::palette(ui).ok_green,
        tm_core::format::format_rate,
    )
    .show_sized(ui, egui::vec2(ui.available_width(), 130.0));

    ui.add_space(10.0);
    stats_grid(ui, |ui| {
        stat_cell(ui, "Empfangen (Rate)", &format::format_rate(net.recv_bps));
        stat_cell(ui, "Gesendet (Rate)", &format::format_rate(net.sent_bps));
        stat_cell(
            ui,
            "Insgesamt empfangen",
            &format::format_bytes(net.total_recv_bytes),
        );
        stat_cell(
            ui,
            "Insgesamt gesendet",
            &format::format_bytes(net.total_sent_bytes),
        );
        if net.link_bps > 0 {
            stat_cell(
                ui,
                "Verbindungsgeschwindigkeit",
                &format!("{:.1} Gbit/s", net.link_bps as f64 / 1e9),
            );
        }
    });
}

// ---- GPU ------------------------------------------------------------------

fn gpu_detail(app: &mut TaskManApp, ui: &mut egui::Ui, entry: &ResourceEntry) {
    let Some(snap) = app.latest_snapshot() else {
        return;
    };
    let Some(gpu) = snap.gpus.iter().find(|g| g.id.to_string() == entry.key) else {
        return;
    };
    header(ui, crate::icons::Icon::Gpu, &gpu.name, "");

    let n = visible_points(app);
    let util: Vec<f64> = app
        .history
        .iter()
        .filter(|h| h.gpus.iter().any(|(id, _, _)| *id == gpu.id))
        .map(|h| h.gpus.iter().find(|(id, ..)| *id == gpu.id).unwrap().1 as f64)
        .collect();
    let mem: Vec<f64> = app
        .history
        .iter()
        .filter(|h| h.gpus.iter().any(|(id, _, _)| *id == gpu.id))
        .map(|h| bytes_to_mb(h.gpus.iter().find(|(id, ..)| *id == gpu.id).unwrap().2))
        .collect();
    let util = slice_last(&util, n).to_vec();
    let mem = slice_last(&mem, n).to_vec();

    LineChart::new(&util, accent_color(ui), |v| format!("{v:.0} %"))
        .y_max(100.0)
        .show_sized(ui, egui::vec2(ui.available_width(), 160.0));
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("GPU-Speicher")
            .size(12.0)
            .color(theme::palette(ui).text_dim),
    );
    LineChart::new(&mem, theme::palette(ui).ok_green, mb_fmt)
        .show_sized(ui, egui::vec2(ui.available_width(), 120.0));

    ui.add_space(10.0);
    stats_grid(ui, |ui| {
        stat_cell(ui, "Auslastung", &format!("{:.0} %", gpu.util_pct));
        if gpu.mem_total_bytes > 0 {
            stat_cell(
                ui,
                "VRAM",
                &format!(
                    "{}/{}",
                    format::format_bytes(gpu.mem_used_bytes),
                    format::format_bytes(gpu.mem_total_bytes)
                ),
            );
        }
        if let Some(t) = gpu.temperature_c {
            stat_cell(ui, "Temperatur", &format!("{t:.0} °C"));
        }
        if !gpu.driver_version.is_empty() {
            stat_cell(ui, "Treiber", &gpu.driver_version);
        }
        // Engine breakdown.
        for e in gpu.engines.iter().take(4) {
            stat_cell(
                ui,
                &format!("Engine {}", e.name),
                &format!("{:.0} %", e.util_pct),
            );
        }
    });
}

fn bytes_to_mb(b: u64) -> f64 {
    b as f64 / 1024.0 / 1024.0
}
fn mb_fmt(v: f64) -> String {
    format!("{v:.0} MB")
}

fn stats_grid(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.separator();
    egui::Grid::new("stats")
        .num_columns(4)
        .spacing([16.0, 10.0])
        .show(ui, add);
}

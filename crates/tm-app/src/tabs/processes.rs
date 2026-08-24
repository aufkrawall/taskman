//! Processes tab: grouped Apps/Background/System table with heat-mapped
//! resource columns, search filter and End-Task actions.

use eframe::egui;
use tm_core::format;
use tm_core::model::{ProcCategory, ProcStatus, ProcessEntry};

use crate::app::TaskManApp;
use crate::theme;
use crate::widgets::tablekit::{self, ProcColumn};

#[derive(Default)]
pub struct State {
    pub sort_col: Option<ProcColumn>,
    pub ascending: bool,
    pub search: String,
    /// Collapsed group set.
    pub collapsed: [bool; 3],
}


pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let Some(snap) = app.latest_snapshot() else {
        ui.centered_and_justified(|ui| ui.label("Sammle Daten…"));
        return;
    };

    // Toolbar.
    toolbar(app, ui);

    // Column maxima for heat normalization (over all visible rows).
    let maxima = column_maxima(&snap.processes);

    let mut state = std::mem::take(&mut app.processes_state);
    egui::ScrollArea::both().show(ui, |ui| {
        // Table header.
        egui::Grid::new("proc-header")
            .num_columns(ProcColumn::ALL.len())
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                for col in ProcColumn::ALL {
                    let label = col.header();
                    let resp = ui
                        .allocate_ui(egui::vec2(col.width() - 12.0, 20.0), |ui| {
                            ui.horizontal(|ui| {
                                if col.is_heat() || matches!(col, ProcColumn::Name) {
                                    ui.label(
                                        egui::RichText::new(label)
                                            .strong()
                                            .color(pal.text_dim)
                                            .size(11.5),
                                    );
                                } else {
                                    ui.label(
                                        egui::RichText::new(label)
                                            .color(pal.text_dim)
                                            .size(11.5),
                                    );
                                }
                            });
                        })
                        .response;
                    if resp.clicked() {
                        if state.sort_col == Some(*col) {
                            state.ascending = !state.ascending;
                        } else {
                            state.sort_col = Some(*col);
                            state.ascending = false; // TM defaults to descending for numeric columns
                        }
                    }
                    if state.sort_col == Some(*col) {
                        tablekit::sort_arrow(ui, state.ascending, true);
                    }
                    let _ = resp;
                }
                ui.end_row();
            });

        ui.separator();

        // Groups in fixed order with collapsible headers.
        let groups = [
            (ProcCategory::App, "Apps"),
            (ProcCategory::Background, "Hintergrund"),
            (ProcCategory::System, "System"),
        ];
        for (gi, (cat, label)) in groups.iter().enumerate() {
            let procs = filtered_group(&snap.processes, *cat, &state.search);
            if procs.is_empty() && !state.search.is_empty() {
                continue;
            }
            let total_cpu: f32 = procs.iter().map(|p| p.cpu_pct).sum();
            let collapsed = state.collapsed[gi];

            // Group header row.
            egui::Grid::new(format!("grp-{gi}"))
                .num_columns(ProcColumn::ALL.len())
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    let icon = if collapsed {
                        crate::icons::Icon::ChevronRight
                    } else {
                        crate::icons::Icon::ChevronDown
                    };
                    let (btn_rect, btn_resp) =
                        ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
                    crate::icons::draw_at(ui, btn_rect, icon, pal.text);
                    if btn_resp.clicked() {
                        state.collapsed[gi] = !collapsed;
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "{label} ({})",
                            snap.processes.iter().filter(|p| p.category == *cat).count()
                        ))
                        .size(13.5),
                    );
                    ui.weak(format!("{:.1} %", total_cpu));
                    ui.end_row();
                });

            if collapsed {
                continue;
            }

            // Sorted process rows.
            let mut sorted = procs;
            sort_procs(&mut sorted, state.sort_col.unwrap_or(ProcColumn::Cpu), state.ascending);
            for p in sorted {
                proc_row(app, ui, &pal, p, &maxima, &state.search);
            }
            ui.add_space(4.0);
        }
    });
    app.processes_state = state;
}

fn toolbar(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut app.processes_state.search)
                .hint_text("Suchen…")
                .desired_width(220.0),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Task beenden").clicked() {
                if let Some(pid) = app.selected_pid {
                    match app.actions.kill_process(pid, false) {
                        Ok(()) => app.shared.toast(format!("Prozess {pid} beendet")),
                        Err(e) => app.shared.toast(format!("Fehler: {e}")),
                    }
                    app.selected_pid = None;
                } else {
                    app.shared.toast("Kein Prozess ausgewählt");
                }
            }
            if ui.button("Alle reduzieren").clicked() {
                app.processes_state.collapsed = [true, true, true];
            }
            if ui.button("Alle erweitern").clicked() {
                app.processes_state.collapsed = [false, false, false];
            }
        });
        let _ = pal;
    });
    ui.add_space(2.0);
}

struct Maxima {
    cpu: f32,
    mem: u64,
    disk: f64,
    net: f64,
    gpu: f32,
}

fn column_maxima(procs: &[tm_core::model::ProcessEntry]) -> Maxima {
    Maxima {
        cpu: procs.iter().map(|p| p.cpu_pct).fold(0.0f32, f32::max),
        mem: procs.iter().map(|p| p.mem_bytes).max().unwrap_or(0),
        disk: procs
            .iter()
            .map(|p| p.disk_read_bps + p.disk_write_bps)
            .fold(0.0f64, f64::max),
        net: procs
            .iter()
            .map(|p| p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0))
            .fold(0.0f64, f64::max),
        gpu: procs
            .iter()
            .filter_map(|p| p.gpu_util_pct)
            .fold(0.0f32, f32::max),
    }
}

fn filtered_group<'a>(
    procs: &'a [ProcessEntry],
    cat: ProcCategory,
    search: &str,
) -> Vec<&'a ProcessEntry> {
    let q = search.to_lowercase();
    procs
        .iter()
        .filter(|p| p.category == cat && (q.is_empty() || p.name.to_lowercase().contains(&q)))
        .collect()
}

fn sort_procs(v: &mut [&ProcessEntry], col: ProcColumn, asc: bool) {
    let cmp = |a: &ProcessEntry, b: &ProcessEntry| -> std::cmp::Ordering {
        match col {
            ProcColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            ProcColumn::Status => format!("{:?}", a.status).cmp(&format!("{:?}", b.status)),
            ProcColumn::User => a.user.clone().unwrap_or_default().cmp(&b.user.clone().unwrap_or_default()),
            ProcColumn::Cpu => a.cpu_pct.partial_cmp(&b.cpu_pct).unwrap_or(std::cmp::Ordering::Equal),
            ProcColumn::Memory => a.mem_bytes.cmp(&b.mem_bytes),
            ProcColumn::Disk => (a.disk_read_bps + a.disk_write_bps)
                .partial_cmp(&(b.disk_read_bps + b.disk_write_bps))
                .unwrap_or(std::cmp::Ordering::Equal),
            ProcColumn::Network => {
                let an = a.net_recv_bps.unwrap_or(0.0) + a.net_sent_bps.unwrap_or(0.0);
                let bn = b.net_recv_bps.unwrap_or(0.0) + b.net_sent_bps.unwrap_or(0.0);
                an.partial_cmp(&bn).unwrap_or(std::cmp::Ordering::Equal)
            }
            ProcColumn::Gpu => a
                .gpu_util_pct
                .partial_cmp(&b.gpu_util_pct)
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    };
    v.sort_by(|a, b| {
        let o = cmp(a, b);
        if asc { o } else { o.reverse() }
    });
}

fn proc_row(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &theme::Palette,
    p: &ProcessEntry,
    m: &Maxima,
    search: &str,
) {
    let selected = app.selected_pid == Some(p.pid);
    let row_h = 24.0;

    let response = ui.allocate_ui(egui::vec2(ui.available_width(), row_h), |ui| {
        ui.horizontal(|ui| {
            // Name cell with selection highlight spanning full width is complex;
            // highlight via small accent dot instead.
            if selected {
                let rect = ui.available_rect_before_wrap();
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(rect.left_top(), egui::vec2(rect.width(), row_h)),
                    3.0,
                    pal.accent.gamma_multiply(0.18),
                );
            }

            // Icon-ish dot colored by category.
            let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, row_h), egui::Sense::hover());
            let color = match p.category {
                ProcCategory::App => pal.accent,
                ProcCategory::Background => pal.text_dim.gamma_multiply(0.7),
                ProcCategory::System => pal.ok_green.gamma_multiply(0.9),
            };
            ui.painter().circle_filled(rect.center(), 3.0, color);

            // Name (indented under its group).
            ui.label(
                egui::RichText::new(highlight_name(p, search)).size(12.5).color(pal.text),
            );

            // Status.
            let status_label = match p.status {
                ProcStatus::Suspended => "Angehalten",
                ProcStatus::NotResponding => "Reagiert nicht",
                ProcStatus::Running => "",
            };
            ui.monospace(status_label);

            // User.
            ui.weak(p.user.clone().unwrap_or_default());

            // Numeric cells.
            tablekit::heat_cell_r(ui, pal, heat_t(p.cpu_pct, m.cpu), format::format_pct(p.cpu_pct));
            tablekit::heat_cell_r(ui, pal, bytes_t(p.mem_bytes, m.mem), format::format_bytes(p.mem_bytes));
            let d_total = p.disk_read_bps + p.disk_write_bps;
            tablekit::heat_cell_r(ui, pal, rate_t(d_total, m.disk), format::format_rate_short(d_total));
            let n_total = p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0);
            let net_text = if p.net_recv_bps.is_none() && p.net_sent_bps.is_none() {
                "".to_string()
            } else {
                format::format_rate_short(n_total)
            };
            tablekit::heat_cell_r(ui, pal, rate_t(n_total, m.net), net_text);
            let gpu_text = p
                .gpu_util_pct
                .map(|g| format::format_pct(g))
                .unwrap_or_default();
            tablekit::heat_cell_r(ui, pal, p.gpu_util_pct.map(|g| heat_t(g, m.gpu)).unwrap_or(0.0), gpu_text);
        });
    }).response;

    // Row interaction.
    let row_id = response.id.with(p.pid);
    let response = response.interact(egui::Sense::click());
    if response.clicked() {
        app.selected_pid = Some(p.pid);
    }
    response.context_menu(|ui| {
        ui.set_min_width(180.0);
        if ui.button("Task beenden").clicked() {
            match app.actions.kill_process(p.pid, false) {
                Ok(()) => app.shared.toast(format!("{} beendet", p.shown_name())),
                Err(e) => app.shared.toast(format!("Fehler: {e}")),
            }
            ui.close();
        }
        #[cfg(target_os = "windows")]
        if ui.button("Struktur beenden").clicked() {
            match app.actions.kill_process(p.pid, true) {
                Ok(()) => app.shared.toast(format!("Struktur von {} beendet", p.shown_name())),
                Err(e) => app.shared.toast(format!("Fehler: {e}")),
            }
            ui.close();
        }
        ui.separator();
        let suspended = p.status == ProcStatus::Suspended;
        if ui.button(if suspended { "Fortsetzen" } else { "Anhalten" }).clicked() {
            match app.actions.suspend_process(p.pid, !suspended) {
                Ok(()) => app.shared.toast(if suspended { "Fortgesetzt" } else { "Angehalten" }),
                Err(e) => app.shared.toast(format!("Fehler: {e}")),
            }
            ui.close();
        }
        #[cfg(target_os = "windows")]
        {
            let eco_on = app.efficiency_pids.contains(&p.pid);
            if ui.button(if eco_on { "Effizienzmodus deaktivieren" } else { "Effizienzmodus" }).clicked() {
                match app.actions.set_efficiency_mode(p.pid, !eco_on) {
                    Ok(()) => {
                        if eco_on { app.efficiency_pids.remove(&p.pid); } else { app.efficiency_pids.insert(p.pid); }
                        app.shared.toast("Effizienzmodus geändert");
                    }
                    Err(e) => app.shared.toast(format!("Fehler: {e}")),
                }
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Im Detail anzeigen").clicked() {
            app.details_filter = p.name.clone();
            app.tab = crate::app::Tab::Details;
            ui.close();
        }
        let _ = row_id;
    });
}

fn highlight_name(p: &ProcessEntry, search: &str) -> String {
    if search.is_empty() {
        p.shown_name().to_string()
    } else {
        p.shown_name().to_string()
    }
}

fn heat_t(v: f32, max: f32) -> f32 {
    if max > 0.001 { (v / max).sqrt() } else { 0.0 }
}
fn bytes_t(v: u64, max: u64) -> f32 {
    if max > 0 { ((v as f32 / max as f32)).sqrt() } else { 0.0 }
}
fn rate_t(v: f64, max: f64) -> f32 {
    if max > 0.001 { ((v / max) as f32).sqrt() } else { 0.0 }
}

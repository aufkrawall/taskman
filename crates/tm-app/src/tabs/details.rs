//! Details tab: dense flat process table with many columns, priority and
//! affinity controls.

use eframe::egui;
use tm_core::format;
use tm_core::model::{PriorityClass, ProcessEntry};

use crate::app::TaskManApp;
use crate::theme;

pub const COLUMNS: &[DetailColumn] = &[
    DetailColumn::Name,
    DetailColumn::Pid,
    DetailColumn::Status,
    DetailColumn::User,
    DetailColumn::Session,
    DetailColumn::Cpu,
    DetailColumn::CpuTime,
    DetailColumn::Memory,
    DetailColumn::Commit,
    DetailColumn::Peak,
    DetailColumn::Handles,
    DetailColumn::Threads,
    DetailColumn::DiskTotal,
    DetailColumn::Priority,
    DetailColumn::Wow64,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailColumn {
    Name,
    Pid,
    Status,
    User,
    Session,
    Cpu,
    CpuTime,
    Memory,
    Commit,
    Peak,
    Handles,
    Threads,
    DiskTotal,
    Priority,
    Wow64,
}

impl DetailColumn {
    pub fn header(self) -> &'static str {
        match self {
            DetailColumn::Name => "Name",
            DetailColumn::Pid => "PID",
            DetailColumn::Status => "Status",
            DetailColumn::User => "Benutzername",
            DetailColumn::Session => "Sitzungs-ID",
            DetailColumn::Cpu => "CPU",
            DetailColumn::CpuTime => "CPU-Zeit",
            DetailColumn::Memory => "Arbeitsspeicher (aktiv)",
            DetailColumn::Commit => "Zugesicherter Speicher",
            DetailColumn::Peak => "Spitzenwert",
            DetailColumn::Handles => "Handles",
            DetailColumn::Threads => "Threads",
            DetailColumn::DiskTotal => "E/A gesamt",
            DetailColumn::Priority => "Basispriorität",
            DetailColumn::Wow64 => "Plattform",
        }
    }

    pub fn width(self) -> f32 {
        match self {
            DetailColumn::Name => 220.0,
            DetailColumn::User | DetailColumn::Status => 100.0,
            DetailColumn::Priority | DetailColumn::Wow64 => 90.0,
            _ => 96.0,
        }
    }
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let Some(snap) = app.latest_snapshot() else {
        ui.centered_and_justified(|ui| ui.label("Sammle Daten…"));
        return;
    };

    // Toolbar: filter + column hint.
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut app.details_filter)
                .hint_text("Nach Name filtern…")
                .desired_width(220.0),
        );
        if !app.details_filter.is_empty() && ui.small_button("✕").clicked() {
            app.details_filter.clear();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Prozess beenden").clicked() && app.selected_pid.is_some() {
                let pid = app.selected_pid.unwrap();
                match app.actions.kill_process(pid, false) {
                    Ok(()) => app.shared.toast(format!("Prozess {pid} beendet")),
                    Err(e) => app.shared.toast(format!("Fehler: {e}")),
                }
            }
        });
    });
    ui.separator();

    let q = app.details_filter.to_lowercase();
    let mut rows: Vec<&ProcessEntry> = snap
        .processes
        .iter()
        .filter(|p| q.is_empty() || p.name.to_lowercase().contains(&q))
        .collect();
    sort_rows(&mut rows, app.details_sort_col, app.details_ascending);

    let max_cpu = rows.iter().map(|p| p.cpu_pct).fold(0.0f32, f32::max);
    let max_mem = rows.iter().map(|p| p.mem_bytes).max().unwrap_or(0);

    egui::ScrollArea::both()
        .id_salt("details-table")
        .show(ui, |ui| {
            // Header row.
            egui::Grid::new("det-header")
                .num_columns(COLUMNS.len())
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    for (ci, col) in COLUMNS.iter().enumerate() {
                        let resp = ui
                            .allocate_ui(egui::vec2(col.width() - 10.0, 20.0), |ui| {
                                ui.label(
                                    egui::RichText::new(col.header()).size(11.5).color(pal.text_dim),
                                );
                            })
                            .response;
                        if resp.clicked() {
                            if app.details_sort_col == ci {
                                app.details_ascending = !app.details_ascending;
                            } else {
                                app.details_sort_col = ci;
                                app.details_ascending = true;
                            }
                        }
                        let _ = resp;
                    }
                    ui.end_row();
                });
            ui.separator();

            egui::Grid::new("det-rows")
                .num_columns(COLUMNS.len())
                .spacing([8.0, 1.0])
                .show(ui, |ui| {
                    for p in rows {
                        for col in COLUMNS {
                            cell(app, ui, &pal, p, *col, max_cpu, max_mem);
                        }
                        ui.end_row();
                    }
                });

            // Keep the grid tall enough to scroll smoothly.
            ui.add_space(20.0);
        });
}

fn cell(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &theme::Palette,
    p: &ProcessEntry,
    col: DetailColumn,
    max_cpu: f32,
    max_mem: u64,
) {
    let selected = app.selected_pid == Some(p.pid);
    let mut weak = |s: String| {
        ui.add_sized(
            [col.width() - 10.0, 20.0],
            egui::Label::new(egui::RichText::new(s).size(12.0).color(pal.text_dim))
                .selectable(false)
                .sense(egui::Sense::click())
                .truncate(),
        )
    };

    let resp = match col {
        DetailColumn::Name => {
            let r = mk_text(ui, col, &pal, p.shown_name().to_string());
            if selected {
                let rect = r.rect.expand2(egui::vec2(200.0, 0.0));
                ui.painter().rect_filled(rect, 3.0, pal.accent.gamma_multiply(0.15));
            }
            r
        }
        DetailColumn::Pid => weak(p.pid.to_string()),
        DetailColumn::Status => weak(
            match p.status {
                tm_core::model::ProcStatus::Running => "Wird ausgeführt".into(),
                tm_core::model::ProcStatus::Suspended => "Angehalten".into(),
                tm_core::model::ProcStatus::NotResponding => "Reagiert nicht".into(),
            },
        ),
        DetailColumn::User => weak(p.user.clone().unwrap_or_default()),
        DetailColumn::Session => weak(p.session_id.map(|s| s.to_string()).unwrap_or_default()),
        DetailColumn::Cpu => {
            let intensity = if max_cpu > 0.001 { (p.cpu_pct / max_cpu).sqrt() } else { 0.0 };
            heat_text(ui, pal, intensity, format::format_pct(p.cpu_pct), col.width())
        }
        DetailColumn::CpuTime => weak(p.cpu_time_s.map(format::format_cpu_time).unwrap_or_default()),
        DetailColumn::Memory => {
            let intensity = if max_mem > 0 { ((p.mem_bytes as f32 / max_mem as f32)).sqrt() } else { 0.0 };
            heat_text(ui, pal, intensity, format::format_bytes(p.mem_bytes), col.width())
        }
        DetailColumn::Commit => weak(p.commit_bytes.map(|b| format::format_bytes(b)).unwrap_or_default()),
        DetailColumn::Peak => weak(p.peak_mem_bytes.map(|b| format::format_bytes(b)).unwrap_or_default()),
        DetailColumn::Handles => weak(p.handles.map(|h| h.to_string()).unwrap_or_default()),
        DetailColumn::Threads => weak(p.threads.map(|t| t.to_string()).unwrap_or_default()),
        DetailColumn::DiskTotal => weak(format!(
            "{} / {}",
            format::format_bytes(p.disk_read_total),
            format::format_bytes(p.disk_write_total)
        )),
        DetailColumn::Priority => weak(
            match p.priority {
                PriorityClass::Realtime => "Echtzeit",
                PriorityClass::High => "Hoch",
                PriorityClass::AboveNormal => "Über normal",
                PriorityClass::Normal => "Normal",
                PriorityClass::BelowNormal => "Unter normal",
                PriorityClass::Low => "Niedrig",
                PriorityClass::Unknown => "",
            }
            .into(),
        ),
        DetailColumn::Wow64 => weak(
            p.wow64
                .map(|w| if w { "32 Bit" } else { "64 Bit" })
                .unwrap_or_default()
                .into(),
        ),
    };

    let resp = resp.interact(egui::Sense::click());
    if resp.clicked() {
        app.selected_pid = Some(p.pid);
    }
    resp.context_menu(|ui| {
        context_menu(app, ui, p);
    });
}

fn mk_text(ui: &mut egui::Ui, col: DetailColumn, pal: &theme::Palette, s: String) -> egui::Response {
    ui.add_sized(
        [col.width() - 10.0, 20.0],
        egui::Label::new(egui::RichText::new(s).size(12.0).color(pal.text))
            .selectable(false)
            .sense(egui::Sense::click())
            .truncate(),
    )
}

#[allow(dead_code)]
fn mk_weak(ui: &mut egui::Ui, col: DetailColumn, pal: &theme::Palette, s: String) -> egui::Response {
    ui.add_sized(
        [col.width() - 10.0, 20.0],
        egui::Label::new(egui::RichText::new(s).size(12.0).color(pal.text_dim))
            .selectable(false)
            .sense(egui::Sense::click())
            .truncate(),
    )
}

fn heat_text(ui: &mut egui::Ui, pal: &theme::Palette, t: f32, s: String, w: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size([w - 10.0, 20.0].into(), egui::Sense::hover());
    if t > 0.02 {
        ui.painter().rect_filled(rect, 3.0, theme::heat_color(pal, t.clamp(0.0, 1.0)));
    }
    ui.painter().text(
        rect.right_center() + egui::vec2(-4.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        s,
        egui::FontId::proportional(12.0),
        pal.text,
    );
    resp
}

fn context_menu(app: &mut TaskManApp, ui: &mut egui::Ui, p: &ProcessEntry) {
    ui.set_min_width(190.0);
    ui.label(egui::RichText::new(p.shown_name()).strong().size(12.5));
    ui.separator();

    if ui.button("Prozess beenden").clicked() {
        match app.actions.kill_process(p.pid, false) {
            Ok(()) => app.shared.toast(format!("{} beendet", p.shown_name())),
            Err(e) => app.shared.toast(format!("Fehler: {e}")),
        }
        ui.close();
    }
    #[cfg(target_os = "windows")]
    if ui.button("Struktur beenden").clicked() {
        match app.actions.kill_process(p.pid, true) {
            Ok(()) => app.shared.toast("Struktur beendet"),
            Err(e) => app.shared.toast(format!("Fehler: {e}")),
        }
        ui.close();
    }

    ui.separator();
    ui.menu_button("Priorität", |ui| {
        for (cls, label) in [
            (PriorityClass::Realtime, "Echtzeit"),
            (PriorityClass::High, "Hoch"),
            (PriorityClass::AboveNormal, "Über normal"),
            (PriorityClass::Normal, "Normal"),
            (PriorityClass::BelowNormal, "Unter normal"),
            (PriorityClass::Low, "Niedrig"),
        ] {
            if ui.button(label).clicked() {
                match app.actions.set_priority(p.pid, cls) {
                    Ok(()) => app.shared.toast(format!("Priorität: {label}")),
                    Err(e) => app.shared.toast(format!("Fehler: {e}")),
                }
                ui.close();
            }
        }
    });

    if ui.button("Affinität festlegen…").clicked() {
        let mask = app.actions.get_affinity_mask(p.pid).unwrap_or(u64::MAX);
        app.affinity_dialog = Some((p.pid, mask));
        ui.close();
    }

    let suspended = p.status == tm_core::model::ProcStatus::Suspended;
    if ui.button(if suspended { "Fortsetzen" } else { "Anhalten" }).clicked() {
        match app.actions.suspend_process(p.pid, !suspended) {
            Ok(()) => {}
            Err(e) => app.shared.toast(format!("Fehler: {e}")),
        }
        ui.close();
    }

    #[cfg(target_os = "windows")]
    {
        ui.separator();
        let eco_on = app.efficiency_pids.contains(&p.pid);
        if ui
            .button(if eco_on { "Effizienzmodus aus" } else { "Effizienzmodus an" })
            .clicked()
        {
            match app.actions.set_efficiency_mode(p.pid, !eco_on) {
                Ok(()) => {
                    if eco_on {
                        app.efficiency_pids.remove(&p.pid);
                    } else {
                        app.efficiency_pids.insert(p.pid);
                    }
                    app.shared.toast("Effizienzmodus geändert");
                }
                Err(e) => app.shared.toast(format!("Fehler: {e}")),
            }
            ui.close();
        }
    }
}

/// Affinity checkbox dialog (up to 64 logical processors).
pub fn affinity_dialog(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    pid: u32,
    mask: u64,
    _pal: &theme::Palette,
) {
    let mut open = true;
    egui::Window::new(format!("Prozessoraffinität — PID {pid}"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let sys_mask = app.actions.system_affinity_mask().unwrap_or(u64::MAX);
            let mut new_mask = mask;
            egui::Grid::new("affinity")
                .num_columns(8)
                .spacing([6.0, 6.0])
                .show(ui, |ui| {
                    for cpu in 0..64usize {
                        let allowed = sys_mask & (1u64 << cpu) != 0;
                        let mut on = mask & (1u64 << cpu) != 0;
                        if ui.add_enabled(allowed, egui::Checkbox::new(&mut on, cpu.to_string())).changed() {
                            if on {
                                new_mask |= 1u64 << cpu;
                            } else {
                                new_mask &= !(1u64 << cpu);
                            }
                        }
                        if (cpu + 1) % 8 == 0 {
                            ui.end_row();
                        }
                    }
                });
            if new_mask == 0 {
                ui.label(egui::RichText::new("Mindestens ein Prozessor muss ausgewählt sein.").color(theme::LIGHT.heat_high));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Abbrechen").clicked() {
                    app.affinity_dialog = None;
                }
                if ui
                    .add_enabled(new_mask != 0, egui::Button::new("Übernehmen"))
                    .clicked()
                {
                    match app.actions.set_affinity_mask(pid, new_mask) {
                        Ok(()) => app.shared.toast("Affinität gesetzt"),
                        Err(e) => app.shared.toast(format!("Fehler: {e}")),
                    }
                    app.affinity_dialog = None;
                }
            });
        });
    if !open {
        app.affinity_dialog = None;
    }
}

fn sort_rows(v: &mut [&ProcessEntry], col_idx: usize, asc: bool) {
    let Some(col) = COLUMNS.get(col_idx) else { return };
    use std::cmp::Ordering;
    v.sort_by(|a, b| {
        let o: Ordering = match col {
            DetailColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            DetailColumn::Pid => a.pid.cmp(&b.pid),
            DetailColumn::Status => format!("{:?}", a.status).cmp(&format!("{:?}", b.status)),
            DetailColumn::User => a.user.clone().unwrap_or_default().cmp(&b.user.clone().unwrap_or_default()),
            DetailColumn::Session => a.session_id.cmp(&b.session_id),
            DetailColumn::Cpu => a.cpu_pct.partial_cmp(&b.cpu_pct).unwrap_or(Ordering::Equal),
            DetailColumn::CpuTime => a.cpu_time_s.partial_cmp(&b.cpu_time_s).unwrap_or(Ordering::Equal),
            DetailColumn::Memory => a.mem_bytes.cmp(&b.mem_bytes),
            DetailColumn::Commit => a.commit_bytes.cmp(&b.commit_bytes),
            DetailColumn::Peak => a.peak_mem_bytes.cmp(&b.peak_mem_bytes),
            DetailColumn::Handles => a.handles.cmp(&b.handles),
            DetailColumn::Threads => a.threads.cmp(&b.threads),
            DetailColumn::DiskTotal => (a.disk_read_total + a.disk_write_total)
                .cmp(&(b.disk_read_total + b.disk_write_total)),
            DetailColumn::Priority => (a.priority as u8).cmp(&(b.priority as u8)),
            DetailColumn::Wow64 => a.wow64.cmp(&b.wow64),
        };
        if asc { o } else { o.reverse() }
    });
}

//! Details tab: dense flat process table with the Win11 TM default columns —
//! Name, PID, Status, Benutzername, CPU, Arbeitsspeicher, Plattform,
//! Heraufgestuft, UAC-Virtualisierung, GPU-Modul.

use eframe::egui;
use tm_core::format;
use tm_core::model::{PriorityClass, ProcStatus, ProcessEntry};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::theme;
use crate::widgets::tablekit::{TmColumn, TmTable};

const COLS: [TmColumn; 10] = [
    TmColumn::text("name", "Name", 0.0),
    TmColumn::text("pid", "PID", 90.0),
    TmColumn::text("status", "Status", 150.0),
    TmColumn::text("user", "Benutzername", 120.0),
    TmColumn::text("cpu", "CPU", 64.0),
    TmColumn::num("mem", "Arbeitsspeicher", 130.0),
    TmColumn::text("platform", "Plattform", 90.0),
    TmColumn::text("elevated", "Heraufgestuft", 110.0),
    TmColumn::text("uac", "UAC-Virtualisierung", 160.0),
    TmColumn::text("gpu", "GPU-Modul", 110.0),
];

#[derive(Default)]
pub struct State {
    pub sort_col: usize,
    pub ascending: bool,
    pub filter: String,
    cache: Option<Cache>,
}

struct Cache {
    key: (u64, String, usize, bool),
    rows: Vec<Row>,
}

pub struct Row {
    pub pid: u32,
    pub name: String,
    pub icon_path: Option<String>,
    pub pid_s: String,
    pub status: &'static str,
    pub user: String,
    pub cpu_s: String,
    pub mem_s: String,
    pub platform: &'static str,
    pub elevated: &'static str,
    pub uac: &'static str,
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let Some(snap) = app.latest_snapshot() else {
        ui.centered_and_justified(|ui| ui.label("Sammle Daten…"));
        return;
    };

    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app: &mut TaskManApp, ui| {
            if crate::app_ui::cmd_button(ui, &pal, Icon::Close, "Task beenden", app.selected_pid.is_some())
            {
                app.end_selected();
            }
        },
        |_app, ui| {
            if ui.button("Jetzt aktualisieren (F5)").clicked() {
                ui.close();
            }
        },
    );

    let table = TmTable::new(COLS.to_vec(), 340.0);

    // Rebuild the row model only when the snapshot/search/sort changes.
    let key = (
        snap.timestamp_ms,
        app.search.clone(),
        app.details_state.sort_col,
        app.details_state.ascending,
    );
    let mut cache = app.details_state.cache.take();
    let stale = cache.as_ref().is_none_or(|c| c.key != key);
    if stale {
        cache = Some(Cache { key: key.clone(), rows: build_rows(&snap, &key.1, key.2, key.3) });
    }
    let rows = &cache.as_ref().expect("cache").rows;

    let avail = ui.available_width();
    if let Some(col) = table.header(
        ui,
        &pal,
        avail,
        Some((app.details_state.sort_col, app.details_state.ascending)),
        None,
    ) {
        if app.details_state.sort_col == col {
            app.details_state.ascending = !app.details_state.ascending;
        } else {
            app.details_state.sort_col = col;
            app.details_state.ascending = !COLS[col.min(COLS.len() - 1)].numeric;
        }
    }

    egui::ScrollArea::vertical()
        .id_salt("details-table")
        .auto_shrink(false)
        .show(ui, |ui| {
            for row in rows {
                let selected = app.selected_pid == Some(row.pid);
                let (rect, resp) = table.row(ui, &pal, avail, selected);

                let tex = row
                    .icon_path
                    .as_ref()
                    .and_then(|p| app.shared.icons.get(ui.ctx(), app.actions.as_ref(), p, 6));
                table.icon_cell(ui, rect, tex.as_ref(), pal.accent);
                let name_rect = table.col_rect(0, avail, rect);
                ui.painter().text(
                    egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &row.name,
                    egui::FontId::proportional(12.5),
                    pal.text,
                );

                table.text_cell(ui, avail, rect, 1, &row.pid_s, &pal, false);
                table.text_cell(ui, avail, rect, 2, row.status, &pal, false);
                table.text_cell(ui, avail, rect, 3, &row.user, &pal, false);
                table.text_cell(ui, avail, rect, 4, &row.cpu_s, &pal, false);
                // Memory: right-aligned like TM.
                let mem_rect = table.col_rect(5, avail, rect);
                ui.painter().text(
                    egui::Pos2::new(mem_rect.right() - 10.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    &row.mem_s,
                    egui::FontId::proportional(12.5),
                    pal.text,
                );
                table.text_cell(ui, avail, rect, 6, row.platform, &pal, false);
                table.text_cell(ui, avail, rect, 7, row.elevated, &pal, false);
                table.text_cell(ui, avail, rect, 8, row.uac, &pal, false);
                table.text_cell(ui, avail, rect, 9, "", &pal, false);

                if resp.clicked() {
                    app.selected_pid = Some(row.pid);
                }
                resp.context_menu(|ui| {
                    // Re-fetch the live entry for the context menu actions.
                    if let Some(p) = snap.process(row.pid) {
                        context_menu(app, ui, p);
                    }
                });
            }
            ui.add_space(12.0);
        });
    app.details_state.cache = cache;
}

fn build_rows(snap: &tm_core::model::Snapshot, search: &str, sort_col: usize, ascending: bool) -> Vec<Row> {
    let q = search.trim().to_lowercase();
    let mut list: Vec<&ProcessEntry> = snap
        .processes
        .iter()
        .filter(|p| q.is_empty() || p.name.to_lowercase().contains(&q) || p.shown_name().to_lowercase().contains(&q))
        .collect();

    let sv = |p: &ProcessEntry, i: usize| -> f64 {
        match i {
            4 => p.cpu_pct as f64,
            5 => p.mem_bytes as f64,
            _ => 0.0,
        }
    };
    list.sort_by(|a, b| {
        let o = match sort_col {
            1 => a.pid.cmp(&b.pid),
            2 => format!("{:?}", a.status).cmp(&format!("{:?}", b.status)),
            3 => a.user.clone().unwrap_or_default().cmp(&b.user.clone().unwrap_or_default()),
            4 => sv(a, 4).partial_cmp(&sv(b, 4)).unwrap_or(std::cmp::Ordering::Equal),
            5 => sv(a, 5).partial_cmp(&sv(b, 5)).unwrap_or(std::cmp::Ordering::Equal),
            6 => a.wow64.cmp(&b.wow64),
            7 => a.elevated.cmp(&b.elevated),
            8 => a.elevated.cmp(&b.elevated),
            _ => a.shown_name().to_lowercase().cmp(&b.shown_name().to_lowercase()),
        };
        if ascending { o } else { o.reverse() }
    });

    list.into_iter()
        .map(|p| {
            let status = match p.status {
                ProcStatus::Running => "Wird ausgeführt",
                ProcStatus::Suspended => "Angehalten",
                ProcStatus::NotResponding => "Nicht reagiert",
            };
            let platform = match p.wow64 {
                Some(true) => "32 Bit",
                _ => "64 Bit",
            };
            let elevated = match p.elevated {
                Some(true) => "Ja",
                _ if p.user.as_deref() == Some("SYSTEM") || matches!(p.pid, 4 | 0) => "Ja",
                _ => "Nein",
            };
            let uac = if p.user.as_deref() == Some("SYSTEM") || matches!(p.pid, 4 | 0) {
                "Nicht zugelassen"
            } else {
                "Deaktiviert"
            };
            Row {
                pid: p.pid,
                name: p.shown_name().to_string(),
                icon_path: p.exe_path.as_ref().map(|x| x.to_string_lossy().into_owned()),
                pid_s: p.pid.to_string(),
                status,
                user: p.user.clone().unwrap_or_default(),
                cpu_s: format::format_cpu_detail(p.cpu_pct),
                mem_s: format::format_k_de(p.mem_bytes),
                platform,
                elevated,
                uac,
            }
        })
        .collect()
}

fn context_menu(app: &mut TaskManApp, ui: &mut egui::Ui, p: &ProcessEntry) {
    ui.set_min_width(200.0);
    ui.label(egui::RichText::new(p.shown_name()).strong().size(12.5));
    ui.separator();

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

    let suspended = p.status == ProcStatus::Suspended;
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
                        if ui
                            .add_enabled(allowed, egui::Checkbox::new(&mut on, cpu.to_string()))
                            .changed()
                        {
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
                ui.label(
                    egui::RichText::new("Mindestens ein Prozessor muss ausgewählt sein.")
                        .color(theme::DARK.heat_high),
                );
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

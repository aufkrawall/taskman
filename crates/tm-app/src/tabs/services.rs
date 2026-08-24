//! Services tab: SCM-backed list (Name/PID/Beschreibung/Status/Gruppe) with
//! German status labels, row selection and the Starten/Beenden/Neu starten/
//! Dienste öffnen command bar.

use eframe::egui;
use std::time::{Duration, Instant};
use tm_core::model::ServiceStatus;

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::theme;
use crate::widgets::tablekit::{TmColumn, TmTable};

const COLS: [TmColumn; 5] = [
    TmColumn::text("name", "Name", 0.0),
    TmColumn::text("pid", "PID", 90.0),
    TmColumn::text("desc", "Beschreibung", 460.0),
    TmColumn::text("status", "Status", 130.0),
    TmColumn::text("group", "Gruppe", 150.0),
];

pub struct Cache {
    pub items: Vec<tm_core::model::ServiceInfo>,
    pub fetched: Instant,
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);

    // Lazy refresh every 5 s.
    {
        let cache = app.shared.services_cache.clone();
        let mut guard = cache.lock();
        let stale = match guard.as_ref() {
            Some(c) => c.fetched.elapsed() > Duration::from_secs(5),
            None => true,
        };
        if stale {
            match app.actions.list_services() {
                Ok(items) => {
                    *guard = Some(Cache { items, fetched: Instant::now() });
                }
                Err(e) => {
                    app.shared.toast(format!("Dienste nicht verfügbar: {e}"));
                    *guard = Some(Cache { items: vec![], fetched: Instant::now() });
                }
            }
        }
    }

    let selected_status = app
        .services_selected_name
        .as_ref()
        .and_then(|name| {
            app.shared
                .services_cache
                .lock()
                .as_ref()
                .and_then(|c| c.items.iter().find(|s| &s.name == name))
                .map(|s| s.status)
        });

    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app, ui| {
            let running = selected_status == Some(ServiceStatus::Running);
            let stopped = selected_status == Some(ServiceStatus::Stopped);
            if crate::app_ui::cmd_button(ui, &pal, Icon::Play, "Starten", stopped) {
                control(app, tm_platform::actions::ServiceAction::Start);
            }
            if crate::app_ui::cmd_button(ui, &pal, Icon::StopSquare, "Beenden", running) {
                control(app, tm_platform::actions::ServiceAction::Stop);
            }
            if crate::app_ui::cmd_button(ui, &pal, Icon::Restart, "Neu starten", running) {
                control(app, tm_platform::actions::ServiceAction::Restart);
            }
            crate::app_ui::vsep(ui, &pal);
            if crate::app_ui::cmd_button(ui, &pal, Icon::OpenExternal, "Dienste öffnen", true) {
                let _ = app.actions.run_new_task("services.msc", false);
            }
        },
        |_app, ui| {
            if ui.button("Jetzt aktualisieren (F5)").clicked() {
                ui.close();
            }
        },
    );

    let guard = app.shared.services_cache.clone();
    let cache = guard.lock();
    let Some(ref c) = *cache else { return };

    let q = app.search.trim().to_lowercase();
    let mut rows: Vec<&tm_core::model::ServiceInfo> = c
        .items
        .iter()
        .filter(|s| {
            q.is_empty()
                || s.name.to_lowercase().contains(&q)
                || s.display_name.to_lowercase().contains(&q)
                || s.description.to_lowercase().contains(&q)
        })
        .collect();
    rows.sort_by_key(|a| a.name.to_lowercase());

    let table = TmTable::new(COLS.to_vec(), 340.0);
    let avail = ui.available_width();
    table.header(ui, &pal, avail, None, None);

    egui::ScrollArea::vertical()
        .id_salt("svc-table")
        .auto_shrink(false)
        .show(ui, |ui| {
            for s in rows {
                let selected = app.services_selected_name.as_deref() == Some(s.name.as_str());
                let (rect, resp) = table.row(ui, &pal, avail, selected);

                // Gear glyph per row.
                let icon_rect = egui::Rect::from_center_size(
                    egui::Pos2::new(rect.left() + 38.0, rect.center().y),
                    egui::vec2(16.0, 16.0),
                );
                crate::icons::draw_at(ui, icon_rect, Icon::Properties, pal.text_dim);
                let name_rect = table.col_rect(0, avail, rect);
                ui.painter().text(
                    egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &s.name,
                    egui::FontId::proportional(12.5),
                    pal.text,
                );
                table.text_cell(ui, avail, rect, 1, &s.pid.map(|p| p.to_string()).unwrap_or_default(), &pal, false);
                table.text_cell(ui, avail, rect, 2, &s.display_name, &pal, false);
                table.text_cell(ui, avail, rect, 3, status_label(s.status), &pal, false);
                table.text_cell(ui, avail, rect, 4, &s.group, &pal, false);

                if resp.clicked() {
                    app.services_selected_name = Some(s.name.clone());
                }
                resp.context_menu(|ui| {
                    ui.set_min_width(160.0);
                    if ui.button("Starten").clicked() {
                        app.services_selected_name = Some(s.name.clone());
                        control(app, tm_platform::actions::ServiceAction::Start);
                        ui.close();
                    }
                    if ui.button("Beenden").clicked() {
                        app.services_selected_name = Some(s.name.clone());
                        control(app, tm_platform::actions::ServiceAction::Stop);
                        ui.close();
                    }
                    if ui.button("Neu starten").clicked() {
                        app.services_selected_name = Some(s.name.clone());
                        control(app, tm_platform::actions::ServiceAction::Restart);
                        ui.close();
                    }
                });
            }
            ui.add_space(12.0);
        });
}

fn status_label(st: ServiceStatus) -> &'static str {
    match st {
        ServiceStatus::Running => "Wird ausgeführt",
        ServiceStatus::Stopped => "Beendet",
        ServiceStatus::StartPending => "Startet",
        ServiceStatus::StopPending => "Wird beendet",
        ServiceStatus::ContinuePending => "Wird fortgesetzt",
        ServiceStatus::PausePending => "Wird angehalten",
        ServiceStatus::Paused => "Angehalten",
        ServiceStatus::Unknown => "",
    }
}

fn control(app: &mut TaskManApp, action: tm_platform::actions::ServiceAction) {
    if let Some(name) = app.services_selected_name.clone() {
        let result = app.actions.control_service(&name, action);
        // Invalidate the cache so the status refreshes immediately.
        *app.shared.services_cache.lock() = None;
        match result {
            Ok(()) => app.shared.toast(format!("'{name}' ausgeführt")),
            Err(e) => app.shared.toast(format!("Fehler: {e}")),
        }
    }
}

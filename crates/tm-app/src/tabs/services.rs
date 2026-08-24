//! Services tab: SCM-backed list with status filter, search and control
//! actions (start/stop/restart).

use eframe::egui;
use std::time::{Duration, Instant};
use tm_core::model::ServiceStatus;

use crate::app::TaskManApp;
use crate::theme;

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
                    *guard = Some(Cache {
                        items,
                        fetched: Instant::now(),
                    });
                }
                Err(e) => {
                    app.shared.toast(format!("Dienste nicht verfügbar: {e}"));
                    // Avoid toast spam: pretend we just fetched.
                    *guard = Some(Cache {
                        items: vec![],
                        fetched: Instant::now(),
                    });
                }
            }
        }
    }

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut app.services_search)
                .hint_text("Suchen…")
                .desired_width(220.0),
        );
        if ui
            .selectable_label(app.services_running_filter, "Wird ausgeführt")
            .clicked()
        {
            app.services_running_filter = !app.services_running_filter;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(if app.actions.is_elevated() {
                    "Administrator"
                } else {
                    ""
                })
                .size(11.5)
                .color(pal.text_dim),
            );
        });
    });
    ui.separator();

    let guard = app.shared.services_cache.clone();
    let cache = guard.lock();
    let Some(ref c) = *cache else { return };

    let q = app.services_search.to_lowercase();
    let rows: Vec<&tm_core::model::ServiceInfo> = c
        .items
        .iter()
        .filter(|s| !app.services_running_filter || s.status == ServiceStatus::Running)
        .filter(|s| {
            q.is_empty()
                || s.name.to_lowercase().contains(&q)
                || s.display_name.to_lowercase().contains(&q)
                || s.description.to_lowercase().contains(&q)
        })
        .collect();

    egui::ScrollArea::both()
        .id_salt("svc-table")
        .show(ui, |ui| {
            egui::Grid::new("svc-header")
                .num_columns(5)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    for h in ["Name", "PID", "Beschreibung", "Status", "Gruppe"] {
                        ui.label(egui::RichText::new(h).size(11.5).color(pal.text_dim));
                    }
                    ui.end_row();
                });
            ui.separator();

            egui::Grid::new("svc-rows")
                .num_columns(5)
                .spacing([10.0, 1.0])
                .show(ui, |ui| {
                    for s in rows {
                        ui.monospace(&s.name);
                        ui.label(s.pid.map(|p| p.to_string()).unwrap_or_default());
                        ui.add_sized(
                            [380.0, 20.0],
                            egui::Label::new(truncate(&s.display_name, 60)).truncate(),
                        );
                        status_badge(ui, &pal, s.status);
                        ui.weak(&s.group);
                        ui.end_row();
                    }
                    // context menus per row need ids; simpler: buttons on hover are heavy.
                });
            ui.add_space(20.0);
        });

    // Control actions via a simple selected-service pattern:
    // clicking a row stores its name; buttons act on it.
    if let Some(name) = app.services_selected_name.clone() {
        egui::Area::new(egui::Id::new("svc-actions"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-14.0, -40.0])
            .show(&ctx_of(ui), |ui| {
                egui::Frame::window(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&name).strong());
                        if ui.button("Starten").clicked() {
                            report(
                                app,
                                name.clone(),
                                app.actions.control_service(
                                    &name,
                                    tm_platform::actions::ServiceAction::Start,
                                ),
                            );
                        }
                        if ui.button("Beenden").clicked() {
                            report(
                                app,
                                name.clone(),
                                app.actions.control_service(
                                    &name,
                                    tm_platform::actions::ServiceAction::Stop,
                                ),
                            );
                        }
                        if ui.button("Neu starten").clicked() {
                            report(
                                app,
                                name.clone(),
                                app.actions.control_service(
                                    &name,
                                    tm_platform::actions::ServiceAction::Restart,
                                ),
                            );
                        }
                        if ui.small_button("✕").clicked() {
                            app.services_selected_name = None;
                        }
                    });
                });
            });
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn status_badge(ui: &mut egui::Ui, pal: &theme::Palette, st: ServiceStatus) {
    let color = match st {
        ServiceStatus::Running => pal.ok_green,
        ServiceStatus::Stopped => pal.text_dim,
        _ => pal.accent,
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.0, color);
        ui.label(st.label());
    });
}

fn ctx_of(ui: &egui::Ui) -> egui::Context {
    ui.ctx().clone()
}

fn report(app: &mut TaskManApp, name: String, result: Result<(), tm_core::TmError>) {
    match result {
        Ok(()) => app.shared.toast(format!("'{name}' ausgeführt")),
        Err(e) => app.shared.toast(format!("Fehler: {e}")),
    }
}

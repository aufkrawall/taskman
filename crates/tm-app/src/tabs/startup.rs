//! Startup apps tab: registry/folder/autostart entries with enable/disable.

use eframe::egui;
use std::time::{Duration, Instant};

use crate::app::TaskManApp;
use crate::theme;

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);

    // Lazy refresh.
    {
        let cache = app.shared.startup_cache.clone();
        let mut guard = cache.lock();
        let stale = match guard.as_ref() {
            Some((_, t)) => t.elapsed() > Duration::from_secs(10),
            None => true,
        };
        if stale {
            match app.actions.list_startup() {
                Ok(items) => *guard = Some((items, Instant::now())),
                Err(e) => {
                    app.shared.toast(format!("Autostart nicht verfügbar: {e}"));
                    *guard = Some((vec![], Instant::now()));
                }
            }
        }
    }

    ui.label(
        egui::RichText::new("Apps, die beim Systemstart ausgeführt werden. Rechtsklick zum Aktivieren/Deaktivieren.")
            .size(12.0)
            .color(pal.text_dim),
    );
    ui.separator();

    let guard = app.shared.startup_cache.clone();
    let mut cache = guard.lock();
    let Some((items, _)) = cache.as_mut() else {
        return;
    };

    egui::ScrollArea::both()
        .id_salt("startup-table")
        .show(ui, |ui| {
            egui::Grid::new("startup-header")
                .num_columns(4)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for h in ["Name", "Herausgeber", "Status", "Auswirkung"] {
                        ui.label(egui::RichText::new(h).size(11.5).color(pal.text_dim));
                    }
                    ui.end_row();
                });
            ui.separator();

            egui::Grid::new("startup-rows")
                .num_columns(4)
                .spacing([12.0, 1.0])
                .show(ui, |ui| {
                    for item in items.iter_mut() {
                        let resp = ui
                            .allocate_ui(egui::vec2(280.0, 22.0), |ui| {
                                ui.vertical_centered_justified(|ui| {
                                    ui.add_sized(
                                        [270.0, 18.0],
                                        egui::Label::new(
                                            egui::RichText::new(&item.name)
                                                .size(12.5)
                                                .color(pal.text),
                                        )
                                        .truncate(),
                                    );
                                });
                            })
                            .response;
                        let resp = resp.on_hover_text(format!(
                            "{}
{}",
                            item.command, item.location
                        ));

                        ui.label(item.publisher.clone().unwrap_or_default());
                        status_text(ui, item.enabled);
                        impact_label(ui, item.impact);

                        resp.context_menu(|ui| {
                            ui.set_min_width(160.0);
                            if ui
                                .button(if item.enabled {
                                    "Deaktivieren"
                                } else {
                                    "Aktivieren"
                                })
                                .clicked()
                            {
                                let new_enabled = !item.enabled;
                                match app.actions.set_startup_enabled(
                                    &item.id,
                                    &item.location,
                                    new_enabled,
                                ) {
                                    Ok(()) => {
                                        item.enabled = new_enabled;
                                        app.shared.toast(if new_enabled {
                                            "Aktiviert"
                                        } else {
                                            "Deaktiviert"
                                        });
                                    }
                                    Err(e) => app.shared.toast(format!("Fehler: {e}")),
                                }
                                ui.close();
                            }
                            ui.separator();
                            ui.label(egui::RichText::new(&item.command).size(10.5).weak());
                        });
                        ui.end_row();
                    }
                });
            ui.add_space(20.0);
        });

    // Keep the lock alive until here.
    drop(cache);
}

fn status_text(ui: &mut egui::Ui, enabled: bool) {
    let pal = theme::palette(ui);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(
            rect.center(),
            3.0,
            if enabled { pal.ok_green } else { pal.text_dim },
        );
        ui.label(if enabled { "Aktiviert" } else { "Deaktiviert" });
    });
}

fn impact_label(ui: &mut egui::Ui, impact: tm_core::model::StartupImpact) {
    ui.label(impact.label());
}

//! Startup apps tab: "Autostart von Apps" — Name/Herausgeber/Status/
//! Startauswirkung table with "Letzte BIOS-Zeit" top right and the
//! Aktivieren/Deaktivieren/Eigenschaften command bar.

use eframe::egui;
use std::time::{Duration, Instant};
use tm_core::format;
use tm_core::model::{StartupImpact, StartupItem};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::theme;
use crate::widgets::tablekit::{TmColumn, TmTable};

const COLS: [TmColumn; 4] = [
    TmColumn::text("name", "Name", 0.0),
    TmColumn::text("pub", "Herausgeber", 240.0),
    TmColumn::text("status", "Status", 140.0),
    TmColumn::text("impact", "Startauswirkung", 150.0),
];

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

    let selected_idx = app.selected_startup_idx;
    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app, ui| {
            let sel: Option<StartupItem> = selected_idx.and_then(|i| {
                let guard = app.shared.startup_cache.lock();
                guard.as_ref().and_then(|(v, _)| v.get(i)).cloned()
            });
            let can_enable = sel.as_ref().is_some_and(|s| !s.enabled);
            let can_disable = sel.as_ref().is_some_and(|s| s.enabled);
            if crate::app_ui::cmd_button(ui, &pal, Icon::Check, "Aktivieren", can_enable) {
                toggle_selected(app, true);
            }
            if crate::app_ui::cmd_button(ui, &pal, Icon::SlashCircle, "Deaktivieren", can_disable) {
                toggle_selected(app, false);
            }
            if crate::app_ui::cmd_button(ui, &pal, Icon::Properties, "Eigenschaften", sel.is_some()) {
                app.startup_props = sel.clone();
            }
            let _ = &sel;
        },
        |_app, ui| {
            if ui.button("Jetzt aktualisieren (F5)").clicked() {
                ui.close();
            }
        },
    );

    // "Letzte BIOS-Zeit:  17,0 Sekunden" — top right, like TM.
    if let Some(ms) = app.actions.last_bios_time_ms() {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 22.0),
            egui::Sense::hover(),
        );
        let text = format!(
            "Letzte BIOS-Zeit:   {} Sekunden",
            format::format_seconds_de(ms as f64 / 1000.0)
        );
        ui.painter().text(
            egui::Pos2::new(rect.right() - 16.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            text,
            egui::FontId::proportional(12.5),
            pal.text,
        );
    }

    let guard = app.shared.startup_cache.clone();
    let mut cache = guard.lock();
    let Some((items, _)) = cache.as_mut() else { return };

    let q = app.search.trim().to_lowercase();
    let table = TmTable::new(COLS.to_vec(), 340.0);
    let avail = ui.available_width();
    table.header(ui, &pal, avail, None, None);

    egui::ScrollArea::vertical()
        .id_salt("startup-table")
        .auto_shrink(false)
        .show(ui, |ui| {
            for (i, item) in items.iter_mut().enumerate() {
                if !q.is_empty() && !item.name.to_lowercase().contains(&q) {
                    continue;
                }
                let selected = app.selected_startup_idx == Some(i);
                let (rect, resp) = table.row(ui, &pal, avail, selected);

                // Icon: real shell icon from the command's executable.
                let exe = exe_from_command(&item.command);
                let tex = exe
                    .as_deref()
                    .and_then(|p| app.shared.icons.get(ui.ctx(), app.actions.as_ref(), p, 4));
                table.icon_cell(ui, rect, tex.as_ref(), pal.accent);
                let name_rect = table.col_rect(0, avail, rect);
                ui.painter().text(
                    egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &item.name,
                    egui::FontId::proportional(12.5),
                    pal.text,
                );

                table.text_cell(ui, avail, rect, 1, item.publisher.as_deref().unwrap_or(""), &pal, false);
                table.text_cell(
                    ui,
                    avail,
                    rect,
                    2,
                    if item.enabled { "Aktiviert" } else { "Deaktiviert" },
                    &pal,
                    false,
                );
                table.text_cell(ui, avail, rect, 3, impact_label(item.impact), &pal, false);

                if resp.clicked() {
                    app.selected_startup_idx = Some(i);
                }
                resp.context_menu(|ui| {
                    ui.set_min_width(160.0);
                    let label = if item.enabled { "Deaktivieren" } else { "Aktivieren" };
                    if ui.button(label).clicked() {
                        let new_enabled = !item.enabled;
                        match app
                            .actions
                            .set_startup_enabled(&item.id, &item.location, new_enabled)
                        {
                            Ok(()) => {
                                item.enabled = new_enabled;
                                app.shared.toast(if new_enabled { "Aktiviert" } else { "Deaktiviert" });
                            }
                            Err(e) => app.shared.toast(format!("Fehler: {e}")),
                        }
                        ui.close();
                    }
                    if ui.button("Eigenschaften").clicked() {
                        app.startup_props = Some(item.clone());
                        ui.close();
                    }
                });
            }
            ui.add_space(12.0);
        });
}

fn toggle_selected(app: &mut TaskManApp, enable: bool) {
    let guard = app.shared.startup_cache.clone();
    let mut cache = guard.lock();
    if let Some((items, _)) = cache.as_mut()
        && let Some(idx) = app.selected_startup_idx
        && let Some(item) = items.get_mut(idx)
    {
        match app.actions.set_startup_enabled(&item.id, &item.location, enable) {
            Ok(()) => {
                item.enabled = enable;
                app.shared.toast(if enable { "Aktiviert" } else { "Deaktiviert" });
            }
            Err(e) => app.shared.toast(format!("Fehler: {e}")),
        }
    }
}

/// German impact labels ("Keine", "Nicht gemessen", ...).
fn impact_label(impact: StartupImpact) -> &'static str {
    match impact {
        StartupImpact::None => "Keine",
        StartupImpact::Low => "Niedrig",
        StartupImpact::Medium => "Mittel",
        StartupImpact::High => "Hoch",
        StartupImpact::Unknown => "Nicht gemessen",
    }
}

/// Best-effort executable path out of a startup command line.
fn exe_from_command(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix('"')
        && let Some(exe) = rest.split('"').next()
    {
        return Some(exe.to_string());
    }
    // First whitespace-separated token that looks like a path.
    cmd.split_whitespace().next().map(str::to_string)
}

/// Properties dialog ("Eigenschaften").
pub fn properties_dialog(app: &mut TaskManApp, ctx: &egui::Context, _pal: &theme::Palette) {
    let mut open = true;
    egui::Window::new("Eigenschaften")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let Some(item) = app.startup_props.clone() else {
                app.startup_props = None;
                return;
            };
            ui.set_min_width(420.0);
            egui::Grid::new("startup-props")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.weak("Name:");
                    ui.label(&item.name);
                    ui.end_row();
                    ui.weak("Herausgeber:");
                    ui.label(item.publisher.clone().unwrap_or_default());
                    ui.end_row();
                    ui.weak("Befehl:");
                    ui.label(&item.command);
                    ui.end_row();
                    ui.weak("Speicherort:");
                    ui.label(&item.location);
                    ui.end_row();
                    ui.weak("Status:");
                    ui.label(if item.enabled { "Aktiviert" } else { "Deaktiviert" });
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Schließen").clicked() {
                        app.startup_props = None;
                    }
                });
            });
        });
    if !open {
        app.startup_props = None;
    }
}

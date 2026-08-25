//! Services tab: SCM-backed list (Name/PID/Beschreibung/Status/Gruppe) with
//! localized status labels, row selection and the Starten/Beenden/Neu starten/
//! Dienste öffnen command bar.
//!
//! All SCM work (enumeration with per-service enrichment, start/stop/restart
//! waits) runs on worker threads — the UI thread never blocks on it.

use eframe::egui;
use std::time::{Duration, Instant};
use tm_core::i18n::{self, K};
use tm_core::model::{ServiceInfo, ServiceStatus};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::theme;
use crate::widgets::tablekit::{TmColumn};

fn columns() -> Vec<TmColumn> {
    vec![
        TmColumn::text("name", i18n::tr(K::ColName), 0.0),
        TmColumn::text("pid", i18n::tr(K::ColPid), 90.0),
        TmColumn::text("desc", i18n::tr(K::ColDescription), 460.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 130.0),
        TmColumn::text("group", i18n::tr(K::ColGroup), 150.0),
    ]
}

pub struct Cache {
    pub items: Vec<ServiceInfo>,
    pub fetched: Instant,
}

/// Kick off a background refresh when the cache is stale; returns whether a
/// fetch is currently running (for the subtle status hint).
fn ensure_fresh(app: &TaskManApp) {
    let stale = {
        let guard = tm_core::sync::lock(&app.shared.services_cache);
        match guard.as_ref() {
            Some(c) => c.fetched.elapsed() > Duration::from_secs(5),
            None => true,
        }
    };
    if stale && app.shared.services_fetch.begin() {
        let cache = app.shared.services_cache.clone();
        let done = app.shared.services_fetch.flag();
        let toasts = app.shared.toasts.clone();
        let actions = app.actions.clone();
        let _ = std::thread::Builder::new()
            .name("tm-svc-fetch".into())
            .spawn(move || {
                let items = actions.list_services();
                let fetched = Instant::now();
                if let Err(e) = &items {
                    crate::app::toast_from(&toasts, i18n::trf(K::ServicesUnavailable, &[&e.to_string()]));
                }
                *tm_core::sync::lock(&cache) = Some(Cache {
                    items: items.unwrap_or_default(),
                    fetched,
                });
                done.store(false, std::sync::atomic::Ordering::Relaxed);
            });
    }
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    ensure_fresh(app);

    // Consume a cross-tab jump ("Gehe zu Dienst(en)").
    if let Some(name) = tm_core::sync::lock(&app.svc_jump).take() {
        app.services_selected_name = Some(name);
        // Make sure the target row exists in a fresh cache.
        *tm_core::sync::lock(&app.shared.services_cache) = None;
        ensure_fresh(app);
    }

    // Selected row's live status (for enabling Start/Stop buttons).
    let selected_status = {
        let guard = tm_core::sync::lock(&app.shared.services_cache);
        guard
            .as_ref()
            .and_then(|c| {
                app.services_selected_name
                    .as_ref()
                    .and_then(|name| c.items.iter().find(|s| &s.name == name))
            })
            .map(|s| s.status)
    };

    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app, ui| {
            let busy = app.shared.service_control_busy();
            let running = selected_status == Some(ServiceStatus::Running);
            let stopped = selected_status == Some(ServiceStatus::Stopped);
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Play,
                i18n::tr(K::StartService),
                stopped && !busy,
            ) {
                control(app, tm_platform::actions::ServiceAction::Start);
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::StopSquare,
                i18n::tr(K::StopService),
                running && !busy,
            ) {
                control(app, tm_platform::actions::ServiceAction::Stop);
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Restart,
                i18n::tr(K::RestartService),
                running && !busy,
            ) {
                control(app, tm_platform::actions::ServiceAction::Restart);
            }
            crate::app_ui::vsep(ui, &pal);
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::OpenExternal,
                i18n::tr(K::OpenServicesApp),
                true,
            ) {
                let _ = app.actions.run_new_task("services.msc", false);
            }
        },
        |_app, ui| {
            if ui.button(i18n::tr(K::RefreshNow)).clicked() {
                *tm_core::sync::lock(&_app.shared.services_cache) = None;
                ensure_fresh(_app);
                ui.close();
            }
        },
    );

    // Clone the Arc so the guard never borrows from `app` itself — closures
    // below need `&mut TaskManApp`.
    let cache_arc = app.shared.services_cache.clone();
    let guard = tm_core::sync::lock(&cache_arc);
    let Some(ref c) = *guard else { return };

    let q = app.search.trim().to_lowercase();
    let mut rows: Vec<&ServiceInfo> = c
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

    let mut table = app.make_table("services", columns(), 340.0);
    let avail = crate::widgets::tablekit::table_avail(ui);
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
                table.text_cell(
                    ui,
                    avail,
                    rect,
                    1,
                    &s.pid.map(|p| p.to_string()).unwrap_or_default(),
                    &pal,
                    false,
                );
                table.text_cell(ui, avail, rect, 2, &s.display_name, &pal, false);
                table.text_cell(ui, avail, rect, 3, status_label(app, s.status), &pal, false);
                table.text_cell(ui, avail, rect, 4, &s.group, &pal, false);

                if resp.clicked() {
                    app.services_selected_name = Some(s.name.clone());
                }
                resp.context_menu(|ui| {
                    ui.set_min_width(170.0);
                    if ui.button(i18n::tr(K::StartService)).clicked() {
                        app.services_selected_name = Some(s.name.clone());
                        control(app, tm_platform::actions::ServiceAction::Start);
                        ui.close();
                    }
                    if ui.button(i18n::tr(K::StopService)).clicked() {
                        app.services_selected_name = Some(s.name.clone());
                        control(app, tm_platform::actions::ServiceAction::Stop);
                        ui.close();
                    }
                    if ui.button(i18n::tr(K::RestartService)).clicked() {
                        app.services_selected_name = Some(s.name.clone());
                        control(app, tm_platform::actions::ServiceAction::Restart);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(i18n::tr(K::OpenServicesApp)).clicked() {
                        let _ = app.actions.run_new_task("services.msc", false);
                        ui.close();
                    }
                    if ui.button(i18n::tr(K::CopyName)).clicked() {
                        ui.ctx().copy_text(s.name.clone());
                        app.shared.toast(i18n::tr(K::Copied));
                        ui.close();
                    }
                });
            }
            ui.add_space(12.0);
        });
    app.persist_table(&mut table);
}

fn status_label(app: &TaskManApp, st: ServiceStatus) -> &'static str {
    match st {
        ServiceStatus::Running => i18n::tr_in(app.lang(), K::StRunning),
        ServiceStatus::Stopped => i18n::tr_in(app.lang(), K::StStopped),
        ServiceStatus::StartPending => i18n::tr_in(app.lang(), K::StStartPending),
        ServiceStatus::StopPending => i18n::tr_in(app.lang(), K::StStopPending),
        ServiceStatus::ContinuePending => i18n::tr_in(app.lang(), K::StContinuePending),
        ServiceStatus::PausePending => i18n::tr_in(app.lang(), K::StPausePending),
        ServiceStatus::Paused => i18n::tr_in(app.lang(), K::StSuspended),
        ServiceStatus::Unknown => "",
    }
}

/// Run the service action on a worker thread (start/stop can take many
/// seconds); the result arrives as a toast, the cache invalidates itself.
fn control(app: &mut TaskManApp, action: tm_platform::actions::ServiceAction) {
    if !app.shared.service_control.begin() {
        return; // already one in flight
    }
    let Some(name) = app.services_selected_name.clone() else {
        app.shared.service_control.end();
        return;
    };
    let actions = app.actions.clone();
    let toasts = app.shared.toasts.clone();

    // Invalidate immediately so the UI reflects the transition.
    *tm_core::sync::lock(&app.shared.services_cache) = None;

    let done_flag = app.shared.service_control.flag();
    let services_cache = app.shared.services_cache.clone();
    let spawned = std::thread::Builder::new()
        .name("tm-svc-ctl".into())
        .spawn(move || {
            let result = actions.control_service(&name, action);
            match &result {
                Ok(()) => crate::app::toast_from(
                    &toasts,
                    format!("'{}' {}", name, i18n::tr(K::ServiceDoneToast)),
                ),
                Err(e) => crate::app::toast_from(
                    &toasts,
                    i18n::trf(K::ErrMsg, &[&e.to_string()]),
                ),
            }
            // Force the next frame's lazy refresh to pick up the new state.
            *tm_core::sync::lock(&services_cache) = None;
            done_flag.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    if spawned.is_err() {
        app.shared.toast(i18n::tr(K::ActionFailed));
        app.shared.service_control.end();
    }
}

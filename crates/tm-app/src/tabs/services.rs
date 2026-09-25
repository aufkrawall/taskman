//! Services tab: SCM-backed list (Name/PID/Beschreibung/Status/Gruppe) with
//! localized status labels, row selection and service controls.

use eframe::egui;
use std::cmp::Ordering;
use std::time::{Duration, Instant};
use tm_core::i18n::{self, K};
use tm_core::model::{ServiceInfo, ServiceStatus};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::search;
use crate::theme;
use crate::widgets::menu;
use crate::widgets::tablekit::{self, TmColumn};

fn columns() -> Vec<TmColumn> {
    vec![
        TmColumn::text("name", i18n::tr(K::ColName), 240.0),
        TmColumn::num("pid", i18n::tr(K::ColPid), 90.0),
        TmColumn::text("desc", i18n::tr(K::ColDescription), 460.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 130.0),
        TmColumn::text("group", i18n::tr(K::ColGroup), 150.0),
    ]
}

/// The candidate fields the Services page matches the global search against —
/// name, display name, description, group and pid. The ONE predicate the
/// visible table and the search commit both use, so Enter can never land on
/// a row the table would not show.
pub(crate) fn matches_search(q: &search::Query, s: &ServiceInfo) -> bool {
    let pid = s.pid.map(|pid| pid.to_string()).unwrap_or_default();
    q.matches_any([
        s.name.as_str(),
        s.display_name.as_str(),
        s.description.as_str(),
        s.group.as_str(),
        pid.as_str(),
    ])
}

/// The service the global search commits to: the FIRST row of the table's
/// current model — filtered by `q`, sorted by the live `sort` — i.e. exactly
/// the row the user sees at the top of the matches. Returns the service NAME
/// (the row-owner key the page selects by). Split out so the commit in
/// `app.rs` reuses this module's row model instead of keeping a sort-order
/// mirror of it in sync.
pub(crate) fn first_search_match_in_display_order(
    items: &[ServiceInfo],
    q: &search::Query,
    sort: tablekit::SortState,
) -> Option<String> {
    let mut rows: Vec<&ServiceInfo> = items.iter().filter(|s| matches_search(q, s)).collect();
    rows.sort_by(|a, b| compare_services(a, b, sort));
    rows.first().map(|s| s.name.clone())
}

pub struct Cache {
    pub items: Vec<ServiceInfo>,
    pub fetched: Instant,
}

fn ensure_fresh(app: &TaskManApp, ctx: &egui::Context) {
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
        let wake = {
            let c = ctx.clone();
            move || c.request_repaint()
        };
        let job = move || {
            let items = actions.list_services();
            let fetched = Instant::now();
            if let Err(e) = &items {
                crate::app::toast_from(
                    &toasts,
                    i18n::trf(K::ServicesUnavailable, &[&e.to_string()]),
                );
            }
            *tm_core::sync::lock(&cache) = Some(Cache {
                items: items.unwrap_or_default(),
                fetched,
            });
            done.store(false, std::sync::atomic::Ordering::Relaxed);
            wake();
        };
        match &app.shared.executor {
            Some(executor) => {
                if !executor.run_quiet(|| {}, job) {
                    app.shared.services_fetch.end();
                    app.shared.toast(i18n::tr(K::ActionQueueFull));
                }
            }
            None => {
                drop(job);
                app.shared.services_fetch.end();
                app.shared.toast(i18n::tr(K::ActionFailed));
            }
        }
    }
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let frame_ctx = ui.ctx().clone();
    ensure_fresh(app, &frame_ctx);

    if let Some(name) = tm_core::sync::lock(&app.svc_jump).take() {
        app.services_selected_name = Some(name);
        *tm_core::sync::lock(&app.shared.services_cache) = None;
        ensure_fresh(app, &frame_ctx);
    }

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
                control(app, &frame_ctx, tm_platform::actions::ServiceAction::Start);
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::StopSquare,
                i18n::tr(K::StopService),
                running && !busy,
            ) {
                control(app, &frame_ctx, tm_platform::actions::ServiceAction::Stop);
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Restart,
                i18n::tr(K::RestartService),
                running && !busy,
            ) {
                control(
                    app,
                    &frame_ctx,
                    tm_platform::actions::ServiceAction::Restart,
                );
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
            if menu::item(ui, i18n::tr(K::RefreshNow)).clicked() {
                *tm_core::sync::lock(&_app.shared.services_cache) = None;
                ensure_fresh(_app, &ui.ctx().clone());
                ui.close();
            }
        },
    );

    let cache_arc = app.shared.services_cache.clone();
    let guard = tm_core::sync::lock(&cache_arc);
    let Some(ref c) = *guard else {
        ui.centered_and_justified(|ui| ui.label(i18n::tr(K::GatheringData)));
        return;
    };

    let q = crate::search::Query::new(&app.search);
    let mut rows: Vec<&ServiceInfo> = c.items.iter().filter(|s| matches_search(&q, s)).collect();
    let sort = app.services_sort;
    rows.sort_by(|a, b| compare_services(a, b, sort));

    // While a dialog is up it owns the keyboard: the page's nav, row keys and
    // menu key all stand down (`TaskManApp::modal_open`).
    let dialog_open = app.modal_open();
    // Arrow/Home/End/Page selection movement over the displayed services.
    // The service NAME is the row-owner key: the one-shot scroll request is
    // parked under that identity and resolved to a row index per frame, so a
    // re-sorted list can never hand the scroll to the wrong row.
    if search::nav_gate(&frame_ctx, dialog_open) {
        let page_rows = tablekit::page_rows(&frame_ctx, "services", tablekit::ROW_H_DENSE)
            .unwrap_or_else(|| {
                (frame_ctx.content_rect().height() / tablekit::ROW_H_DENSE)
                    .floor()
                    .max(1.0) as usize
            });
        if let Some(nav) = search::list_nav(&frame_ctx, dialog_open)
            .filter(|_| !tablekit::header_has_focus(&frame_ctx, "services"))
            && let Some(next) =
                next_selected_name(&rows, app.services_selected_name.as_deref(), nav, page_rows)
        {
            app.services_selected_name = Some(next.to_owned());
            tablekit::request_row_scroll(&frame_ctx, "services", tablekit::stable_key(next));
        }
    }
    let focus_row = tablekit::take_row_scroll(&frame_ctx, "services").and_then(|key| {
        rows.iter()
            .position(|s| tablekit::stable_key(s.name.as_str()) == key)
    });

    let mut table = app
        .make_table("services", columns())
        .with_row_height(tablekit::ROW_H_DENSE);
    let mut fit: Vec<f32> = table
        .cols
        .iter()
        .map(|c| tablekit::text_width(ui, c.label, tablekit::FONT_HDR_LABEL) + 28.0)
        .collect();
    for s in &rows {
        let values = [
            s.name.as_str(),
            "",
            s.display_name.as_str(),
            status_label(app, s.status),
            s.group.as_str(),
        ];
        fit[0] = fit[0].max(tablekit::text_width(ui, values[0], tablekit::FONT_ROW) + 66.0);
        let pid = s.pid.map_or_else(|| "—".to_string(), |p| p.to_string());
        fit[1] = fit[1].max(tablekit::text_width(ui, &pid, tablekit::FONT_ROW) + 22.0);
        for i in 2..5 {
            fit[i] = fit[i].max(tablekit::text_width(ui, values[i], tablekit::FONT_ROW) + 22.0);
        }
    }
    table.apply_auto_fit(fit);

    let avail = crate::widgets::tablekit::table_avail(ui);
    let clicked = tablekit::scrolled_rows(
        "services",
        ui,
        &pal,
        &mut table,
        avail,
        Some((app.services_sort.column, app.services_sort.ascending)),
        None,
        rows.len(),
        (!q.is_empty()).then_some(i18n::tr(K::NoMatches)),
        focus_row,
        None,
        |ui, table, _avail, _content_w, range| {
            for ri in range {
                let s = rows[ri];
                let selected = app.services_selected_name.as_deref() == Some(s.name.as_str());
                let (rect, resp) = table.row(ui, &pal, selected, s.name.as_str());

                let icon_rect = egui::Rect::from_center_size(
                    egui::Pos2::new(rect.left() + 38.0, rect.center().y),
                    egui::vec2(16.0, 16.0),
                );
                crate::icons::draw_at(ui, icon_rect, Icon::Properties, pal.text_dim);
                let name_rect = table.col_rect(0, rect);
                ui.painter_at(name_rect).text(
                    egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &s.name,
                    egui::FontId::proportional(tablekit::FONT_ROW),
                    pal.text,
                );
                let pid_cell = table.col_rect(1, rect);
                ui.painter_at(pid_cell).text(
                    egui::pos2(pid_cell.right() - 10.0, pid_cell.center().y),
                    egui::Align2::RIGHT_CENTER,
                    // A stopped service has no PID; say so instead of leaving a
                    // blank cell that reads like missing data.
                    s.pid.map_or_else(|| "—".to_string(), |pid| pid.to_string()),
                    egui::FontId::proportional(tablekit::FONT_ROW),
                    pal.text,
                );
                let display_truncated = table.text_cell(ui, rect, 2, &s.display_name, &pal, false);
                table.text_cell(ui, rect, 3, status_label(app, s.status), &pal, false);
                let group_truncated = table.text_cell(ui, rect, 4, &s.group, &pal, false);

                // A clipped cell cannot show its content; the full value
                // becomes the row tooltip (before the context menu attaches).
                let resp = if display_truncated {
                    resp.on_hover_text(s.display_name.as_str())
                } else if group_truncated {
                    resp.on_hover_text(s.group.as_str())
                } else {
                    resp
                };

                if resp.clicked() {
                    app.services_selected_name = Some(s.name.clone());
                }
                // Enter (with nothing else holding focus) opens the same menu
                // as the Menu key — there is no clearer primary action on a
                // service row than its Start/Stop/Restart command list. Both
                // stay dead while a dialog is up: a menu opened over a dialog
                // would strand there once the dialog contract consumes Escape.
                let selected_row = app.services_selected_name.as_deref() == Some(s.name.as_str());
                let enter_open = crate::search::row_action_gate(ui.ctx(), app.modal_open())
                    && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter))
                    && selected_row;
                let keyboard_open = (enter_open
                    || (!ui.ctx().any_popup_open() && menu::keyboard_menu_requested(ui.ctx())))
                    && !app.modal_open()
                    && selected_row;
                menu::context_menu_kb(&resp, keyboard_open, |ui| {
                    ui.set_min_width(170.0);
                    let mctx = ui.ctx().clone();
                    // State-aware like the toolbar: Start on a running service
                    // (or Stop on a stopped one) is a guaranteed error toast,
                    // not an action.
                    let busy = app.shared.service_control_busy();
                    let running = s.status == ServiceStatus::Running;
                    let stopped = s.status == ServiceStatus::Stopped;
                    let state_tip = |tip: K| {
                        if busy {
                            i18n::tr(K::ActionAlreadyRunning)
                        } else {
                            i18n::tr(tip)
                        }
                    };
                    if menu::item_enabled(ui, i18n::tr(K::StartService), stopped && !busy)
                        .on_disabled_hover_text(state_tip(K::ServiceNotRunning))
                        .clicked()
                    {
                        app.services_selected_name = Some(s.name.clone());
                        control(app, &mctx, tm_platform::actions::ServiceAction::Start);
                        ui.close();
                    }
                    if menu::item_enabled(ui, i18n::tr(K::StopService), running && !busy)
                        .on_disabled_hover_text(state_tip(K::ServiceRunning))
                        .clicked()
                    {
                        app.services_selected_name = Some(s.name.clone());
                        control(app, &mctx, tm_platform::actions::ServiceAction::Stop);
                        ui.close();
                    }
                    if menu::item_enabled(ui, i18n::tr(K::RestartService), running && !busy)
                        .on_disabled_hover_text(state_tip(K::ServiceRunning))
                        .clicked()
                    {
                        app.services_selected_name = Some(s.name.clone());
                        control(app, &mctx, tm_platform::actions::ServiceAction::Restart);
                        ui.close();
                    }
                    menu::separator(ui);
                    if let Some(pid) = s.pid
                        && menu::item(ui, i18n::tr(K::GoToDetails)).clicked()
                    {
                        let start_epoch_s = app
                            .latest_snapshot()
                            .as_ref()
                            .and_then(|snapshot| snapshot.process(pid))
                            .and_then(|process| process.start_epoch_s);
                        app.pending_details_focus = Some(crate::app::PendingDetailsFocus(vec![
                            crate::app::ProcessIdentity { pid, start_epoch_s },
                        ]));
                        app.tab = crate::app::Tab::Details;
                        ui.close();
                    }
                    if menu::item(ui, i18n::tr(K::OpenServicesApp)).clicked() {
                        let _ = app.actions.run_new_task("services.msc", false);
                        ui.close();
                    }
                    if menu::item(ui, i18n::tr(K::CopyName)).clicked() {
                        ui.ctx().copy_text(s.name.clone());
                        app.shared.toast(i18n::tr(K::Copied));
                        ui.close();
                    }
                });
            }
        },
    );
    if let Some(column) = clicked {
        app.services_sort.clicked(column, column == 1);
        let ids = ["name", "pid", "desc", "status", "group"];
        app.persist_sort("services", ids[column], app.services_sort.ascending);
    }
    app.persist_table(&table);
}

/// Next service selection for one keyboard movement, keyed by the row OWNER
/// (the service name — also the row response id). `None` selection starts at
/// the nearest edge for the direction; the result is clamped to a valid row.
fn next_selected_name<'a>(
    rows: &[&'a ServiceInfo],
    selected: Option<&str>,
    nav: crate::search::ListNav,
    page_rows: usize,
) -> Option<&'a str> {
    let current = selected.and_then(|name| rows.iter().position(|s| s.name == name));
    search::moved_index(rows.len(), current, nav, page_rows)
        .and_then(|index| rows.get(index))
        .map(|s| s.name.as_str())
}

fn compare_services(a: &ServiceInfo, b: &ServiceInfo, sort: tablekit::SortState) -> Ordering {
    if sort.column == 1 && a.pid.is_some() != b.pid.is_some() {
        return b.pid.is_some().cmp(&a.pid.is_some());
    }
    let primary = match sort.column {
        0 => tablekit::cmp_ignore_case(&a.name, &b.name),
        1 => a.pid.cmp(&b.pid),
        2 => tablekit::cmp_ignore_case(&a.display_name, &b.display_name),
        3 => service_status_rank(a.status).cmp(&service_status_rank(b.status)),
        _ => tablekit::cmp_ignore_case(&a.group, &b.group),
    };
    tablekit::directed(primary, sort.ascending)
        .then_with(|| tablekit::cmp_ignore_case(&a.name, &b.name))
}

fn service_status_rank(status: ServiceStatus) -> u8 {
    match status {
        ServiceStatus::Running => 0,
        ServiceStatus::StartPending => 1,
        ServiceStatus::ContinuePending => 2,
        ServiceStatus::PausePending => 3,
        ServiceStatus::Paused => 4,
        ServiceStatus::StopPending => 5,
        ServiceStatus::Stopped => 6,
        ServiceStatus::Unknown => 7,
    }
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

/// Entry point for every service control. Stop and restart can take down
/// functionality the user depends on, so they park behind an explicit
/// confirmation (`control_confirm_dialog`); Start is harmless and stays
/// one-click — the same asymmetry the Users page applies to Disconnect vs.
/// Logoff.
fn control(app: &mut TaskManApp, ctx: &egui::Context, action: tm_platform::actions::ServiceAction) {
    if matches!(
        action,
        tm_platform::actions::ServiceAction::Stop | tm_platform::actions::ServiceAction::Restart
    ) {
        if let Some(name) = app.services_selected_name.clone() {
            app.pending_service_control = Some((name, action));
        }
        return;
    }
    dispatch_control(app, ctx, action);
}

fn dispatch_control(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    action: tm_platform::actions::ServiceAction,
) {
    if !app.shared.service_control.begin() {
        return;
    }
    let Some(name) = app.services_selected_name.clone() else {
        app.shared.service_control.end();
        return;
    };
    let actions = app.actions.clone();
    let toasts = app.shared.toasts.clone();
    *tm_core::sync::lock(&app.shared.services_cache) = None;

    let done_flag = app.shared.service_control.flag();
    let services_cache = app.shared.services_cache.clone();
    let wake = {
        let c = ctx.clone();
        move || c.request_repaint()
    };
    let spawned = std::thread::Builder::new()
        .name("tm-svc-ctl".into())
        .spawn(move || {
            let result = actions.control_service(&name, action);
            match &result {
                Ok(()) => crate::app::toast_from(
                    &toasts,
                    format!("'{}' {}", name, i18n::tr(K::ServiceDoneToast)),
                ),
                Err(e) => crate::app::toast_from(&toasts, i18n::trf(K::ErrMsg, &[&e.to_string()])),
            }
            *tm_core::sync::lock(&services_cache) = None;
            done_flag.store(false, std::sync::atomic::Ordering::Relaxed);
            wake();
        });
    if spawned.is_err() {
        app.shared.toast(i18n::tr(K::ActionFailed));
        app.shared.service_control.end();
    }
}

/// Confirmation dialog for a parked Stop/Restart (`control` parks instead of
/// dispatching right away). The safe action owns the keyboard default.
pub fn control_confirm_dialog(app: &mut TaskManApp, ctx: &egui::Context, pal: &theme::Palette) {
    let Some((name, action)) = app.pending_service_control.clone() else {
        return;
    };
    let display = {
        let guard = tm_core::sync::lock(&app.shared.services_cache);
        guard
            .as_ref()
            .and_then(|c| c.items.iter().find(|s| s.name == name))
            .map(|s| s.display_name.clone())
            .unwrap_or_else(|| name.clone())
    };
    let title = match action {
        tm_platform::actions::ServiceAction::Stop => i18n::tr(K::StopService),
        tm_platform::actions::ServiceAction::Restart => i18n::tr(K::RestartService),
        tm_platform::actions::ServiceAction::Start => i18n::tr(K::StartService),
    };
    let body = match action {
        tm_platform::actions::ServiceAction::Stop => i18n::trf(K::ServiceStopConfirm, &[&display]),
        tm_platform::actions::ServiceAction::Restart => {
            i18n::trf(K::ServiceRestartConfirm, &[&display])
        }
        tm_platform::actions::ServiceAction::Start => String::new(),
    };

    let restore_id = egui::Id::new("svc-confirm-restore-focus");
    let first_frame = ctx
        .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
        .is_none();
    if first_frame {
        // Remember what held keyboard focus before the dialog took it, so
        // closing can hand it back.
        let captured = crate::app_ui::capture_focus(ctx);
        ctx.data_mut(|d| d.insert_temp(restore_id, captured));
    }
    let focus_id = egui::Id::new("svc-confirm-focus-primary");
    let mut open = true;
    let keys = crate::app_ui::consume_dialog_keys(ctx, true);
    let mut focused: bool = ctx.data(|d| d.get_temp(focus_id)).unwrap_or(false);
    focused = crate::app_ui::update_end_task_dialog_focus(
        focused,
        keys.tab,
        keys.shift_tab,
        keys.left,
        keys.right,
    );
    let key_decision = crate::app_ui::dialog_key_decision(keys, focused, true);
    let mut clicked = crate::app_ui::DialogButtonClick::None;
    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(body);
            ui.add_space(8.0);
            clicked = crate::app_ui::dialog_button_row(
                ui,
                ctx,
                pal,
                i18n::tr(K::Cancel),
                Some((i18n::tr(K::Ok), true)),
                &mut focused,
            );
        });
    ctx.data_mut(|d| d.insert_temp(focus_id, focused));
    let cancel = !open
        || matches!(key_decision, Some(crate::app_ui::DialogDecision::Safe))
        || matches!(clicked, crate::app_ui::DialogButtonClick::Safe);
    let confirm = matches!(key_decision, Some(crate::app_ui::DialogDecision::Primary))
        || matches!(clicked, crate::app_ui::DialogButtonClick::Primary);
    if cancel || confirm {
        // Whichever way it resolved, hand keyboard focus back to what held
        // it before the dialog took it.
        let saved = ctx
            .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
            .flatten();
        crate::app_ui::restore_focus(ctx, saved);
        ctx.data_mut(|d| d.remove_temp::<Option<egui::Id>>(restore_id));
        ctx.data_mut(|d| d.remove_temp::<bool>(focus_id));
        app.pending_service_control = None;
    }
    if confirm {
        app.services_selected_name = Some(name);
        dispatch_control(app, ctx, action);
    }
}

/// The commit must mirror the VISIBLE table: same filter fields, same live
/// sort — the old app-side mirror filtered identically but sorted by name
/// alone, so a user-chosen sort made Enter land somewhere the user was not
/// looking.
#[test]
fn search_commit_follows_the_live_sort() {
    let items = vec![
        ServiceInfo {
            name: "DnsCache".into(),
            pid: Some(10),
            ..Default::default()
        },
        ServiceInfo {
            name: "Bfe".into(),
            pid: Some(9),
            ..Default::default()
        },
    ];
    let q = crate::search::Query::new("e");
    assert!(matches_search(&q, &items[0]), "matches the name");
    assert!(matches_search(&q, &items[1]));

    // Name ascending: Bfe sorts ahead of DnsCache.
    assert_eq!(
        first_search_match_in_display_order(&items, &q, tablekit::SortState::new(0, true))
            .as_deref(),
        Some("Bfe")
    );
    // Same query, name descending: DnsCache now sits in the first visible row.
    assert_eq!(
        first_search_match_in_display_order(&items, &q, tablekit::SortState::new(0, false))
            .as_deref(),
        Some("DnsCache")
    );
    assert_eq!(
        first_search_match_in_display_order(
            &items,
            &crate::search::Query::new("none"),
            tablekit::SortState::new(0, true)
        ),
        None,
        "no match, no commit"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, pid: Option<u32>, status: ServiceStatus) -> ServiceInfo {
        ServiceInfo {
            name: name.to_string(),
            display_name: format!("Display {name}"),
            description: String::new(),
            pid,
            status,
            group: String::new(),
            startup_type: String::new(),
            account: String::new(),
        }
    }

    fn sorted(items: &mut [ServiceInfo], column: usize, ascending: bool) {
        let sort = tablekit::SortState::new(column, ascending);
        items.sort_by(|a, b| compare_services(a, b, sort));
    }

    #[test]
    fn name_sort_is_case_insensitive_and_reverses_with_direction() {
        let mut items = vec![
            svc("beta", None, ServiceStatus::Unknown),
            svc("Alpha", None, ServiceStatus::Unknown),
            svc("gamma", None, ServiceStatus::Unknown),
        ];
        sorted(&mut items, 0, true);
        let names: Vec<_> = items.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "beta", "gamma"]);

        sorted(&mut items, 0, false);
        let names: Vec<_> = items.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["gamma", "beta", "Alpha"]);
    }

    #[test]
    fn pidless_services_sort_last_in_either_direction() {
        // Stopped services have no PID; None must never float above a real
        // row (matches the Details "unavailable sorts last" rule).
        let mut items = vec![
            svc("has-pid-a", Some(100), ServiceStatus::Unknown),
            svc("no-pid", None, ServiceStatus::Unknown),
            svc("has-pid-b", Some(50), ServiceStatus::Unknown),
        ];
        for ascending in [true, false] {
            sorted(&mut items, 1, ascending);
            assert_eq!(
                items.last().map(|s| s.name.as_str()),
                Some("no-pid"),
                "ascending={ascending}"
            );
            // Present PIDs keep numeric order independent of the None rule.
            let pids: Vec<u32> = items[..2].iter().filter_map(|s| s.pid).collect();
            if ascending {
                assert_eq!(pids, [50, 100]);
            } else {
                assert_eq!(pids, [100, 50]);
            }
        }
    }

    #[test]
    fn status_sort_follows_native_rank_running_before_stopped() {
        let mut items = vec![
            svc("s", None, ServiceStatus::Stopped),
            svc("r", None, ServiceStatus::Running),
            svc("p", None, ServiceStatus::Paused),
        ];
        sorted(&mut items, 3, true);
        let statuses: Vec<_> = items.iter().map(|s| s.status).collect();
        assert_eq!(
            statuses,
            [
                ServiceStatus::Running,
                ServiceStatus::Paused,
                ServiceStatus::Stopped
            ]
        );
    }

    fn nav_rows() -> Vec<ServiceInfo> {
        vec![
            svc("alpha", None, ServiceStatus::Running),
            svc("beta", Some(1), ServiceStatus::Stopped),
            svc("gamma", None, ServiceStatus::Running),
        ]
    }

    fn by_name(rows: &[ServiceInfo]) -> Vec<&ServiceInfo> {
        rows.iter().collect()
    }

    /// Keyboard selection walks the DISPLAYED rows by the service-name owner
    /// key, starts at the nearest edge without a selection, clamps at both
    /// ends and pages by the table's visible-row span.
    #[test]
    fn keyboard_selection_walks_displayed_services_by_name() {
        let items = nav_rows();
        let rows = by_name(&items);
        let next = search::ListNav::Next;
        let prev = search::ListNav::Previous;

        assert_eq!(next_selected_name(&rows, None, next, 2), Some("alpha"));
        assert_eq!(
            next_selected_name(&rows, Some("alpha"), next, 2),
            Some("beta")
        );
        assert_eq!(
            next_selected_name(&rows, Some("gamma"), next, 2),
            Some("gamma"),
            "the last row stays put"
        );
        assert_eq!(
            next_selected_name(&rows, Some("beta"), prev, 2),
            Some("alpha")
        );
        assert_eq!(
            next_selected_name(&rows, Some("alpha"), prev, 2),
            Some("alpha"),
            "the first row stays put"
        );
        assert_eq!(
            next_selected_name(&rows, Some("alpha"), search::ListNav::PageDown, 2),
            Some("gamma")
        );
        assert_eq!(
            next_selected_name(&rows, Some("beta"), search::ListNav::Last, 2),
            Some("gamma")
        );
        // An exited/filtered-out selection starts from the top again.
        assert_eq!(
            next_selected_name(&rows, Some("vanished"), next, 2),
            Some("alpha")
        );
        // An empty list has nothing to select.
        assert_eq!(next_selected_name(&[], Some("alpha"), next, 2), None);
    }
}

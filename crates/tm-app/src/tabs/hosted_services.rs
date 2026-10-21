//! Process Properties → Services: the services one process hosts, from one
//! service-manager query when the page is first shown (Refresh repeats it).

use eframe::egui;
use std::time::Instant;
use tm_core::i18n::{self, K};
use tm_core::model::ServiceInfo;

use crate::app::{ProcessIdentity, TaskManApp};
use crate::search;
use crate::tabs::inspector::Poll;
use crate::theme;
use crate::widgets::menu;
use crate::widgets::tablekit::{self, TmColumn};

const TABLE_ID: &str = "proc_services";

#[derive(Default)]
pub struct State {
    poll: Poll<Vec<ServiceInfo>>,
    rows: Option<Result<Vec<ServiceInfo>, String>>,
    selected: Option<String>,
}

pub fn tick(state: &mut State, app: &TaskManApp, ctx: &egui::Context, identity: &ProcessIdentity) {
    if let Some(result) = state.poll.take() {
        state.rows = Some(result);
    }
    if app.actions.capabilities().services_list && state.poll.due(Instant::now(), None) {
        start(state, app, ctx, identity);
    }
}

fn start(state: &mut State, app: &TaskManApp, ctx: &egui::Context, identity: &ProcessIdentity) {
    let actions = app.actions.clone();
    let pid = identity.pid;
    state.poll.start(ctx, "tm-hosted-services", move || {
        let mut hosted: Vec<ServiceInfo> = actions
            .list_services()
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|service| service.pid == Some(pid))
            .collect();
        hosted.sort_by(|a, b| tablekit::cmp_ignore_case(&a.name, &b.name));
        Ok(hosted)
    });
}

/// Returns the service the user asked to jump to on the Services page.
pub fn show(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    state: &mut State,
    identity: &ProcessIdentity,
    pal: &theme::Palette,
) -> Option<String> {
    let ctx = ui.ctx().clone();
    let mut jump = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !state.poll.busy(),
                egui::Button::new(i18n::tr(K::RefreshNow)),
            )
            .clicked()
        {
            start(state, app, &ctx, identity);
        }
        if state.poll.busy() {
            ui.spinner();
        }
    });
    ui.add_space(4.0);
    let rows = match &state.rows {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(i18n::tr(K::GatheringData));
            });
            return None;
        }
        Some(Err(error)) => {
            ui.colored_label(pal.heat_high, error);
            return None;
        }
        Some(Ok(rows)) => rows,
    };

    let dialog_open = app.modal_open();
    search::set_active_content(&ctx, TABLE_ID);
    if let Some(nav) = search::list_nav(&ctx, dialog_open) {
        let current = state
            .selected
            .as_deref()
            .and_then(|name| rows.iter().position(|row| row.name == name));
        let page_rows = tablekit::page_rows(&ctx, TABLE_ID, tablekit::ROW_H_DENSE).unwrap_or(10);
        if let Some(index) = search::moved_index(rows.len(), current, nav, page_rows) {
            state.selected = rows.get(index).map(|row| row.name.clone());
        }
    }

    let cols = vec![
        TmColumn::text("name", i18n::tr(K::ColName), 180.0),
        TmColumn::text("desc", i18n::tr(K::ColDescription), 320.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 120.0),
        TmColumn::text("group", i18n::tr(K::ColGroup), 150.0),
    ];
    let mut table = app
        .make_table(TABLE_ID, cols)
        .with_row_height(tablekit::ROW_H_DENSE);
    let avail = tablekit::table_avail(ui);
    let mut clicked_name = None;
    tablekit::scrolled_rows(
        TABLE_ID,
        ui,
        pal,
        &mut table,
        avail,
        None,
        None,
        rows.len(),
        Some(i18n::tr(K::NoHostedServices)),
        None,
        None,
        |ui, table, _avail, _content_width, range| {
            for index in range {
                let Some(service) = rows.get(index) else {
                    continue;
                };
                let selected = state.selected.as_deref() == Some(service.name.as_str());
                let (rect, response) = table.row(ui, pal, selected, service.name.as_str());
                table.describe_row(ui, &response, &service.display_name, index);
                table.text_cell(ui, rect, 0, &service.name, pal, false);
                let desc_truncated =
                    table.text_cell(ui, rect, 1, &service.display_name, pal, false);
                table.text_cell(
                    ui,
                    rect,
                    2,
                    crate::tabs::services::status_label(app, service.status),
                    pal,
                    false,
                );
                table.text_cell(ui, rect, 3, &service.group, pal, false);
                let response = if desc_truncated {
                    response.on_hover_text(service.display_name.as_str())
                } else {
                    response
                };
                if response.clicked() || response.secondary_clicked() {
                    clicked_name = Some(service.name.clone());
                }
                if response.double_clicked() {
                    jump = Some(service.name.clone());
                }
                let keyboard_open = !dialog_open
                    && search::content_has_focus(ui.ctx())
                    && menu::keyboard_menu_requested(ui.ctx())
                    && selected;
                menu::context_menu_kb(&response, keyboard_open, |ui| {
                    if menu::item(ui, i18n::tr(K::GoToServices)).clicked() {
                        jump = Some(service.name.clone());
                        ui.close();
                    }
                    if menu::item(ui, i18n::tr(K::CopyName)).clicked() {
                        ui.ctx().copy_text(service.name.clone());
                        app.shared.toast(i18n::tr(K::Copied));
                        ui.close();
                    }
                });
            }
        },
    );
    if clicked_name.is_some() {
        state.selected = clicked_name;
    }
    app.persist_table(&table);
    jump
}

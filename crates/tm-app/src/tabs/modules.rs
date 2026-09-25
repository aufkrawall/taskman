//! On-demand per-process module inspector.
//!
//! Enumeration and unloading never run on the UI thread. Unload is an
//! intentionally advanced action with a second confirmation; the platform
//! layer revalidates both process identity and the exact module base/path.

use eframe::egui;
use std::cmp::Ordering;
use std::sync::{Arc, Mutex};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_platform::actions::{MODULE_UNLOAD_SINGLE_RELEASE_MARKER, PlatformActions, ProcessModule};

use crate::app::{InFlight, ProcessIdentity, TaskManApp};
use crate::search;
use crate::theme;
use crate::widgets::menu;
use crate::widgets::tablekit::{self, TmColumn};

const MAX_FORCE_UNLOAD_RELEASES: u32 = 64;

#[derive(Debug, Clone)]
enum LoadState {
    Loading,
    Ready(Vec<ProcessModule>),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Name,
    Base,
    Size,
    Path,
}

impl SortColumn {
    fn id(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Base => "base",
            Self::Size => "size",
            Self::Path => "path",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        match id {
            "name" => Some(Self::Name),
            "base" => Some(Self::Base),
            "size" => Some(Self::Size),
            "path" => Some(Self::Path),
            _ => None,
        }
    }
}

pub struct State {
    identity: ProcessIdentity,
    process_name: String,
    filter: String,
    sort: SortColumn,
    ascending: bool,
    selected_base: Option<u64>,
    scroll_to_base: Option<u64>,
    load: Arc<Mutex<LoadState>>,
    fetch: InFlight,
    unload: InFlight,
    pending_unload: Option<ProcessModule>,
}

impl State {
    fn new(
        identity: ProcessIdentity,
        process_name: String,
        sort: SortColumn,
        ascending: bool,
    ) -> Self {
        Self {
            identity,
            process_name,
            filter: String::new(),
            sort,
            ascending,
            selected_base: None,
            scroll_to_base: None,
            load: Arc::new(Mutex::new(LoadState::Loading)),
            fetch: InFlight::default(),
            unload: InFlight::default(),
            pending_unload: None,
        }
    }
}

pub fn open(app: &mut TaskManApp, process: &tm_core::model::ProcessEntry, ctx: &egui::Context) {
    let identity = ProcessIdentity {
        pid: process.pid,
        start_epoch_s: process.start_epoch_s,
    };
    if !app.identity_is_live(&identity) {
        app.shared.toast(i18n::tr(K::ProcessExited));
        return;
    }
    let saved_sort = app.shared.settings.table_sort.get("modules");
    let sort = saved_sort
        .and_then(|saved| SortColumn::from_id(&saved.column))
        .unwrap_or(SortColumn::Name);
    let ascending = saved_sort.is_none_or(|saved| saved.ascending);
    let mut state = State::new(identity, process.shown_name().to_string(), sort, ascending);
    begin_fetch(&mut state, app.actions.clone(), ctx);
    app.module_dialog = Some(state);
}

fn begin_fetch(state: &mut State, actions: Arc<dyn PlatformActions>, ctx: &egui::Context) {
    // The load Arc is shared by both workers. Never let a manual refresh race
    // an unload's mandatory post-action refresh and overwrite newer state.
    if state.unload.busy() || !state.fetch.begin() {
        return;
    }
    *tm_core::sync::lock(&state.load) = LoadState::Loading;
    let load = state.load.clone();
    let identity = state.identity.clone();
    let in_flight = state.fetch.clone();
    let wake = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("tm-modules".into())
        .spawn(move || {
            let result = actions.list_process_modules(identity.pid, identity.start_epoch_s);
            *tm_core::sync::lock(&load) = match result {
                Ok(mut modules) => {
                    modules.sort_by(|a, b| compare_modules(a, b, SortColumn::Name));
                    LoadState::Ready(modules)
                }
                Err(error) => LoadState::Error(error.to_string()),
            };
            in_flight.end();
            wake.request_repaint();
        });
    if let Err(error) = spawned {
        state.fetch.end();
        *tm_core::sync::lock(&state.load) = LoadState::Error(error.to_string());
    }
}

fn force_still_mapped_message(releases: u32, module_name: &str) -> String {
    match i18n::lang() {
        i18n::Lang::De => format!(
            "Force-Unload nach {releases} Entladeanforderungen gestoppt: {module_name} bleibt geladen. Das Modul ist möglicherweise angeheftet oder wird weiterhin referenziert."
        ),
        i18n::Lang::En => format!(
            "Force unload stopped after {releases} release requests: {module_name} remains loaded. The module may be pinned or still referenced."
        ),
    }
}

fn force_refresh_failed_message(releases: u32, error: &str) -> String {
    match i18n::lang() {
        i18n::Lang::De => format!(
            "Force-Unload nach {releases} Entladeanforderungen gestoppt, weil die Modulliste nicht zuverlässig aktualisiert werden konnte: {error}"
        ),
        i18n::Lang::En => format!(
            "Force unload stopped after {releases} release requests because the module list could not be refreshed reliably: {error}"
        ),
    }
}

fn begin_unload(app: &TaskManApp, state: &mut State, module: ProcessModule, ctx: &egui::Context) {
    if !app.identity_is_live(&state.identity) {
        app.shared.toast(i18n::tr(K::ProcessExited));
        return;
    }
    // Serialize module inventory and mutation. Besides avoiding stale UI, this
    // prevents a refresh from racing the action-time base/path revalidation.
    if state.fetch.busy() || !state.unload.begin() {
        app.shared.toast(i18n::tr(K::ModuleBusy));
        return;
    }
    let actions = app.actions.clone();
    let identity = state.identity.clone();
    let load = state.load.clone();
    let in_flight = state.unload.clone();
    let toasts = app.shared.toasts.clone();
    let wake = ctx.clone();
    let module_name = module.name.clone();
    let spawned = std::thread::Builder::new()
        .name("tm-module-unload".into())
        .spawn(move || {
            // Each platform/broker request is deliberately one FreeLibrary
            // call. The GUI makes the confirmed action forceful by issuing
            // another request only after the exact selected base/path has
            // been re-enumerated and proven to still be mapped. This keeps
            // the mixed-version safety marker and avoids the old blind loop.
            let request_path = format!("{}{}", module.path, MODULE_UNLOAD_SINGLE_RELEASE_MARKER);
            let mut releases = 0u32;
            let mut observed_still_mapped = None;
            let mut action_error: Option<String> = None;
            let mut refresh_error: Option<String> = None;

            for _ in 0..MAX_FORCE_UNLOAD_RELEASES {
                let outcome = match actions.unload_process_module(
                    identity.pid,
                    identity.start_epoch_s,
                    module.base_address,
                    &request_path,
                ) {
                    Ok(outcome) => {
                        releases += 1;
                        outcome
                    }
                    Err(error) => {
                        action_error = Some(error.to_string());
                        // Even a timeout can complete remotely after the wait
                        // expires, so always refresh once before reporting it.
                        match actions.list_process_modules(identity.pid, identity.start_epoch_s) {
                            Ok(modules) => {
                                let still_mapped = modules.iter().any(|candidate| {
                                    candidate.base_address == module.base_address
                                        && candidate.path.eq_ignore_ascii_case(&module.path)
                                });
                                *tm_core::sync::lock(&load) = LoadState::Ready(modules);
                                observed_still_mapped = Some(still_mapped);
                            }
                            Err(error) => {
                                let detail = error.to_string();
                                *tm_core::sync::lock(&load) = LoadState::Error(detail.clone());
                                refresh_error = Some(detail);
                            }
                        }
                        break;
                    }
                };

                // Re-enumerate between EVERY release. This is the guard that
                // makes force mode materially safer than repeatedly calling
                // FreeLibrary against a stale HMODULE: the next request only
                // happens if the same sampled process still has the same
                // module at the same base/path.
                match actions.list_process_modules(identity.pid, identity.start_epoch_s) {
                    Ok(modules) => {
                        let still_mapped = modules.iter().any(|candidate| {
                            candidate.base_address == module.base_address
                                && candidate.path.eq_ignore_ascii_case(&module.path)
                        });
                        *tm_core::sync::lock(&load) = LoadState::Ready(modules);
                        observed_still_mapped = Some(still_mapped);
                        if !still_mapped {
                            break;
                        }
                    }
                    Err(error) => {
                        let detail = error.to_string();
                        *tm_core::sync::lock(&load) = LoadState::Error(detail.clone());
                        refresh_error = Some(detail);
                        // The platform may already have verified the result of
                        // this one request. Preserve that observation for the
                        // toast, but never issue another release without a
                        // fresh inventory from the GUI side.
                        observed_still_mapped = outcome.still_mapped;
                        break;
                    }
                }
            }

            let message = if observed_still_mapped == Some(false) {
                i18n::trf(K::ModuleUnloadedMsg, &[&module_name])
            } else if let Some(error) = action_error.as_deref() {
                i18n::trf(K::ErrMsg, &[error])
            } else if let Some(error) = refresh_error.as_deref() {
                force_refresh_failed_message(releases, error)
            } else if observed_still_mapped == Some(true) {
                force_still_mapped_message(releases, &module_name)
            } else {
                i18n::tr(K::ActionFailed).to_string()
            };
            crate::app::toast_from(&toasts, message);
            in_flight.end();
            wake.request_repaint();
        });
    if spawned.is_err() {
        state.unload.end();
        app.shared.toast(i18n::tr(K::ActionFailed));
    }
}

fn compare_modules(a: &ProcessModule, b: &ProcessModule, sort: SortColumn) -> Ordering {
    let primary = match sort {
        SortColumn::Name => tablekit::cmp_ignore_case(&a.name, &b.name),
        SortColumn::Base => a.base_address.cmp(&b.base_address),
        SortColumn::Size => a.size_bytes.cmp(&b.size_bytes),
        SortColumn::Path => tablekit::cmp_ignore_case(&a.path, &b.path),
    };
    primary.then_with(|| a.base_address.cmp(&b.base_address))
}

fn visible_modules(state: &State, modules: &[ProcessModule]) -> Vec<ProcessModule> {
    let needle = state.filter.trim().to_ascii_lowercase();
    let mut rows = modules
        .iter()
        .filter(|module| module_matches_filter(module, &needle))
        .cloned()
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        let order = compare_modules(a, b, state.sort);
        if state.ascending {
            order
        } else {
            order.reverse()
        }
    });
    rows
}

fn module_matches_filter(module: &ProcessModule, needle: &str) -> bool {
    needle.is_empty()
        || module.name.to_ascii_lowercase().contains(needle)
        || module.path.to_ascii_lowercase().contains(needle)
}

pub fn dialog(app: &mut TaskManApp, ctx: &egui::Context, pal: &theme::Palette) {
    let Some(mut state) = app.module_dialog.take() else {
        return;
    };
    let title = format!(
        "{} — {} ({})",
        i18n::tr(K::Modules),
        state.process_name,
        state.identity.pid
    );
    let mut open = true;
    let mut request_refresh = false;
    let mut request_unload = None;
    let can_unload = app.actions.capabilities().unload_module;
    let snapshot = tm_core::sync::lock(&state.load).clone();
    let selected_module = match &snapshot {
        LoadState::Ready(modules) => {
            // Initial preselect, and a fallback when the selection died with
            // its module (it was just unloaded, or the target dropped it) —
            // otherwise the unload button dead-ends right after a success.
            let selection_alive = state
                .selected_base
                .is_some_and(|base| modules.iter().any(|m| m.base_address == base));
            if !selection_alive {
                state.selected_base = modules.first().map(|m| m.base_address);
            }
            state
                .selected_base
                .and_then(|base| modules.iter().find(|module| module.base_address == base))
                .filter(|module| {
                    module_matches_filter(module, &state.filter.trim().to_ascii_lowercase())
                })
                .cloned()
        }
        _ => None,
    };

    // Esc closes the dialog like the X button does — but only while the
    // unload confirmation is not up; that window owns the keyboard then.
    let close_now = state.pending_unload.is_none()
        && ctx.input_mut(|input| input.consume_key(Default::default(), egui::Key::Escape));

    // Tab is never consumed here, so egui's focus system moves between the
    // filter field, the toolbar buttons and (where the platform keeps rows
    // focusable) the table. The first frame anchors focus on the filter and
    // remembers what held focus before, so closing hands it back.
    let restore_id = egui::Id::new("module-dialog-restore-focus");
    let first_frame = ctx
        .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
        .is_none();
    if first_frame {
        let captured = crate::app_ui::capture_focus(ctx);
        ctx.data_mut(|d| d.insert_temp(restore_id, captured));
    }

    egui::Window::new(title)
        .modal(true)
        .open(&mut open)
        .default_size([850.0, 520.0])
        .min_size([560.0, 320.0])
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                let filter_resp = ui.add(
                    egui::TextEdit::singleline(&mut state.filter)
                        .hint_text(i18n::tr(K::SearchHint))
                        .desired_width(300.0),
                );
                // The inspector opens ready to type: the filter narrows the
                // list immediately, and Tab moves on through the toolbar.
                if first_frame {
                    filter_resp.request_focus();
                }
                if ui
                    .add_enabled(
                        !state.fetch.busy() && !state.unload.busy(),
                        egui::Button::new(i18n::tr(K::RefreshNow)),
                    )
                    .clicked()
                {
                    request_refresh = true;
                }
                // Which module to unload is the user's call; every selected
                // module can be attempted. Only the capability (module
                // unload unavailable on this platform) and a running module
                // inventory/action grey the button out.
                let unload = ui
                    .add_enabled(
                        can_unload
                            && selected_module.is_some()
                            && !state.unload.busy()
                            && !state.fetch.busy(),
                        egui::Button::new(i18n::tr(K::UnloadModule)),
                    )
                    .on_disabled_hover_text(if selected_module.is_none() {
                        i18n::tr(K::SelectModuleFirst)
                    } else {
                        i18n::tr(K::ModuleBusy)
                    });
                if unload.clicked() {
                    request_unload = selected_module.clone();
                }
                if state.fetch.busy() || state.unload.busy() {
                    ui.spinner();
                }
            });
            ui.label(
                egui::RichText::new(i18n::tr(K::UnloadModuleWarning))
                    .small()
                    .color(pal.text_dim),
            );
            ui.add_space(4.0);

            match &snapshot {
                LoadState::Loading => {
                    ui.centered_and_justified(|ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(i18n::tr(K::GatheringData));
                        });
                    });
                }
                LoadState::Error(error) => {
                    ui.colored_label(pal.heat_high, error);
                }
                LoadState::Ready(modules) => {
                    let rows = visible_modules(&state, modules);
                    // The inspector IS the dialog here: `app.modal_open()` runs
                    // while `module_dialog` is taken, so it stays false for the
                    // inspector itself — this list nav and type-ahead are the
                    // dialog's own keyboard layer. The only thing to stand
                    // down for is the unload confirmation, a separate modal
                    // on top.
                    let dialog_open = app.modal_open() || state.pending_unload.is_some();
                    search::set_active_content(ctx, "modules");
                    let typed = search::list_type_ahead(ctx, "modules", dialog_open);
                    if let Some(typed) = typed
                        && let Some(base) = search::type_ahead_match(
                            rows.iter()
                                .map(|module| (module.base_address, module.name.as_str()))
                                .collect::<Vec<_>>(),
                            state.selected_base,
                            &typed,
                        )
                    {
                        state.selected_base = Some(base);
                        state.scroll_to_base = Some(base);
                    }
                    if let Some(nav) = search::list_nav(ctx, dialog_open) {
                        let current = state.selected_base.and_then(|base| {
                            rows.iter().position(|module| module.base_address == base)
                        });
                        let page_rows = (ui.available_height() / tablekit::ROW_H_DENSE)
                            .floor()
                            .max(1.0) as usize;
                        if let Some(index) =
                            search::moved_index(rows.len(), current, nav, page_rows)
                            && let Some(module) = rows.get(index)
                        {
                            state.selected_base = Some(module.base_address);
                            state.scroll_to_base = Some(module.base_address);
                        }
                    }

                    let cols = vec![
                        TmColumn::text("name", i18n::tr(K::ColName), 180.0),
                        TmColumn::num("base", i18n::tr(K::ColBaseAddress), 150.0),
                        TmColumn::num("size", i18n::tr(K::ColSize), 110.0),
                        TmColumn::text("path", i18n::tr(K::ColPath), 390.0),
                    ];
                    let mut table = app
                        .make_table("modules", cols)
                        .with_row_height(tablekit::ROW_H_DENSE);
                    let sorted = match state.sort {
                        SortColumn::Name => 0,
                        SortColumn::Base => 1,
                        SortColumn::Size => 2,
                        SortColumn::Path => 3,
                    };
                    let focus = state.scroll_to_base.take().and_then(|base| {
                        rows.iter().position(|module| module.base_address == base)
                    });
                    let avail = tablekit::table_avail(ui);
                    let clicked = tablekit::scrolled_rows(
                        "modules",
                        ui,
                        pal,
                        &mut table,
                        avail,
                        Some((sorted, state.ascending)),
                        None,
                        rows.len(),
                        (!state.filter.trim().is_empty()).then_some(i18n::tr(K::NoMatches)),
                        focus,
                        None,
                        |ui, table, _avail, _content_width, range| {
                            for index in range {
                                let Some(module) = rows.get(index) else {
                                    continue;
                                };
                                let selected = state.selected_base == Some(module.base_address);
                                let (rect, response) =
                                    table.row(ui, pal, selected, module.base_address);
                                table.describe_row(ui, &response, &module.name, index);
                                let name_truncated =
                                    table.text_cell(ui, rect, 0, &module.name, pal, false);
                                let base = format!("0x{:016X}", module.base_address);
                                let base_cell = table.col_rect(1, rect);
                                ui.painter_at(base_cell).text(
                                    egui::pos2(base_cell.right() - 10.0, base_cell.center().y),
                                    egui::Align2::RIGHT_CENTER,
                                    base,
                                    egui::FontId::monospace(tablekit::FONT_ROW),
                                    pal.text,
                                );
                                let size = format::format_bytes_loc(module.size_bytes);
                                let size_cell = table.col_rect(2, rect);
                                ui.painter_at(size_cell).text(
                                    egui::pos2(size_cell.right() - 10.0, size_cell.center().y),
                                    egui::Align2::RIGHT_CENTER,
                                    size,
                                    egui::FontId::proportional(tablekit::FONT_ROW),
                                    pal.text,
                                );
                                let path_truncated =
                                    table.text_cell(ui, rect, 3, &module.path, pal, true);
                                // A clipped cell cannot show its content; the
                                // full value becomes the row tooltip (before
                                // the context menu attaches).
                                let response = if path_truncated {
                                    response.on_hover_text(module.path.as_str())
                                } else if name_truncated {
                                    response.on_hover_text(module.name.as_str())
                                } else {
                                    response
                                };
                                if response.clicked() || response.secondary_clicked() {
                                    state.selected_base = Some(module.base_address);
                                }
                                // While the unload confirmation is up, a row
                                // menu opened by the Menu key would strand
                                // over it; right-click keeps working.
                                let keyboard_open = !dialog_open
                                    && crate::search::content_has_focus(ui.ctx())
                                    && menu::keyboard_menu_requested(ui.ctx())
                                    && state.selected_base == Some(module.base_address);
                                menu::context_menu_kb(&response, keyboard_open, |ui| {
                                    if menu::item(ui, i18n::tr(K::CopyPath)).clicked() {
                                        ui.ctx().copy_text(module.path.clone());
                                        app.shared.toast(i18n::tr(K::Copied));
                                        ui.close();
                                    }
                                    if menu::item(ui, i18n::tr(K::OpenFileLocation)).clicked() {
                                        if let Err(error) =
                                            app.actions.open_file_location(&module.path)
                                        {
                                            app.shared
                                                .toast(i18n::trf(K::ErrMsg, &[&error.to_string()]));
                                        }
                                        ui.close();
                                    }
                                    if menu::item(ui, i18n::tr(K::Properties)).clicked() {
                                        if let Err(error) =
                                            app.actions.open_properties(&module.path)
                                        {
                                            app.shared
                                                .toast(i18n::trf(K::ErrMsg, &[&error.to_string()]));
                                        }
                                        ui.close();
                                    }
                                    menu::separator(ui);
                                    let unload = menu::item_enabled(
                                        ui,
                                        i18n::tr(K::UnloadModule),
                                        can_unload && !state.unload.busy() && !state.fetch.busy(),
                                    )
                                    .on_disabled_hover_text(i18n::tr(K::ModuleBusy));
                                    if unload.clicked() {
                                        request_unload = Some(module.clone());
                                        ui.close();
                                    }
                                });
                            }
                        },
                    );
                    if let Some(column) = clicked {
                        let next = match column {
                            0 => SortColumn::Name,
                            1 => SortColumn::Base,
                            2 => SortColumn::Size,
                            _ => SortColumn::Path,
                        };
                        if state.sort == next {
                            state.ascending = !state.ascending;
                        } else {
                            state.sort = next;
                            state.ascending = !matches!(next, SortColumn::Base | SortColumn::Size);
                        }
                        app.persist_sort("modules", state.sort.id(), state.ascending);
                    }
                    app.persist_table(&table);
                }
            }
        });

    if request_refresh {
        begin_fetch(&mut state, app.actions.clone(), ctx);
    }
    if let Some(module) = request_unload {
        state.pending_unload = Some(module);
    }

    let mut confirmed = None;
    let confirm_restore_id = egui::Id::new("unload-confirm-restore-focus");
    if let Some(module) = state.pending_unload.as_ref() {
        let confirm_focus_id = egui::Id::new("unload-confirm-focus-primary");
        let mut confirm_open = true;
        // Unloading can crash the target: the SAFE action owns the default,
        // so Escape and an unfocused Enter both cancel. This two-button
        // destructive confirm stays a hard keyboard trap (Tab toggles its two
        // buttons via the app-side focus flag).
        let keys = crate::app_ui::consume_dialog_keys(ctx, true);
        // First frame of the confirmation: remember what held focus in the
        // inspector behind, so dismissing hands it back.
        if ctx
            .data(|d| d.get_temp::<Option<egui::Id>>(confirm_restore_id))
            .is_none()
        {
            let captured = crate::app_ui::capture_focus(ctx);
            ctx.data_mut(|d| d.insert_temp(confirm_restore_id, captured));
        }
        let mut focused: bool = ctx.data(|d| d.get_temp(confirm_focus_id)).unwrap_or(false);
        focused = crate::app_ui::update_end_task_dialog_focus(
            focused,
            keys.tab,
            keys.shift_tab,
            keys.left,
            keys.right,
        );
        let key_decision = crate::app_ui::dialog_key_decision(keys, focused, true);
        egui::Window::new(i18n::tr(K::UnloadModule))
            .modal(true)
            .open(&mut confirm_open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(i18n::trf(
                    K::UnloadModuleConfirm,
                    &[&module.name, &state.process_name],
                ));
                ui.add_space(6.0);
                ui.colored_label(pal.heat_high, i18n::tr(K::UnloadModuleWarning));
                ui.add_space(10.0);
                match crate::app_ui::dialog_button_row(
                    ui,
                    ctx,
                    pal,
                    i18n::tr(K::Cancel),
                    Some((i18n::tr(K::UnloadModule), true)),
                    &mut focused,
                ) {
                    crate::app_ui::DialogButtonClick::Safe => confirmed = Some(false),
                    crate::app_ui::DialogButtonClick::Primary => confirmed = Some(true),
                    crate::app_ui::DialogButtonClick::None => {}
                }
            });
        ctx.data_mut(|d| d.insert_temp(confirm_focus_id, focused));
        match key_decision {
            Some(crate::app_ui::DialogDecision::Safe) => confirmed = Some(false),
            Some(crate::app_ui::DialogDecision::Primary) => confirmed = Some(true),
            None => {}
        }
        if !confirm_open {
            confirmed = Some(false);
        }
    }
    if let Some(confirm) = confirmed {
        let saved = ctx
            .data(|d| d.get_temp::<Option<egui::Id>>(confirm_restore_id))
            .flatten();
        crate::app_ui::restore_focus(ctx, saved);
        ctx.data_mut(|d| d.remove_temp::<Option<egui::Id>>(confirm_restore_id));
        let module = state.pending_unload.take();
        if confirm && let Some(module) = module {
            begin_unload(app, &mut state, module, ctx);
        }
    }

    if open && !close_now {
        app.module_dialog = Some(state);
    } else {
        let saved = ctx
            .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
            .flatten();
        crate::app_ui::restore_focus(ctx, saved);
        ctx.data_mut(|d| d.remove_temp::<Option<egui::Id>>(restore_id));
        // An X on the inspector closes it while the unload confirmation may
        // still be parked without a decision; its restore temp must not
        // outlive the dialog, or the next confirmation would restore a stale
        // widget instead of capturing its own opener.
        ctx.data_mut(|d| d.remove_temp::<Option<egui::Id>>(confirm_restore_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str, base: u64, size: u64) -> ProcessModule {
        ProcessModule {
            name: name.into(),
            path: format!("C:\\Test\\{name}"),
            base_address: base,
            size_bytes: size,
        }
    }

    #[test]
    fn module_sort_uses_the_selected_field() {
        let a = module("z.dll", 1, 10);
        let b = module("a.dll", 2, 20);
        assert_eq!(compare_modules(&a, &b, SortColumn::Name), Ordering::Greater);
        assert_eq!(compare_modules(&a, &b, SortColumn::Base), Ordering::Less);
        assert_eq!(compare_modules(&a, &b, SortColumn::Size), Ordering::Less);
    }

    #[test]
    fn module_filter_matches_name_or_full_path() {
        let mut state = State::new(
            ProcessIdentity {
                pid: 1,
                start_epoch_s: None,
            },
            "target.exe".into(),
            SortColumn::Name,
            true,
        );
        let modules = vec![module("alpha.dll", 1, 1), module("beta.dll", 2, 2)];
        state.filter = "alpha".into();
        assert_eq!(visible_modules(&state, &modules).len(), 1);
        state.filter = "test\\beta".into();
        assert_eq!(visible_modules(&state, &modules)[0].name, "beta.dll");
    }
}

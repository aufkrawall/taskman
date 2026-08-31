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
use tm_platform::actions::{PlatformActions, ProcessModule};

use crate::app::{InFlight, ProcessIdentity, TaskManApp};
use crate::search;
use crate::theme;
use crate::widgets::tablekit::{self, TmColumn};

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
    if !state.fetch.begin() {
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

fn begin_unload(app: &TaskManApp, state: &mut State, module: ProcessModule, ctx: &egui::Context) {
    if !app.identity_is_live(&state.identity) {
        app.shared.toast(i18n::tr(K::ProcessExited));
        return;
    }
    if !state.unload.begin() {
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
            let result = actions.unload_process_module(
                identity.pid,
                identity.start_epoch_s,
                module.base_address,
                &module.path,
            );
            let message = match result {
                Ok(()) => {
                    *tm_core::sync::lock(&load) =
                        match actions.list_process_modules(identity.pid, identity.start_epoch_s) {
                            Ok(modules) => LoadState::Ready(modules),
                            Err(error) => LoadState::Error(error.to_string()),
                        };
                    i18n::trf(K::ModuleUnloadedMsg, &[&module_name])
                }
                Err(error) => i18n::trf(K::ErrMsg, &[&error.to_string()]),
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
        SortColumn::Name => a
            .name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase()),
        SortColumn::Base => a.base_address.cmp(&b.base_address),
        SortColumn::Size => a.size_bytes.cmp(&b.size_bytes),
        SortColumn::Path => a
            .path
            .to_ascii_lowercase()
            .cmp(&b.path.to_ascii_lowercase()),
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
        LoadState::Ready(modules) => state
            .selected_base
            .and_then(|base| modules.iter().find(|module| module.base_address == base))
            .filter(|module| {
                module_matches_filter(module, &state.filter.trim().to_ascii_lowercase())
            })
            .cloned(),
        _ => None,
    };

    egui::Window::new(title)
        .open(&mut open)
        .default_size([850.0, 520.0])
        .min_size([560.0, 320.0])
        .resizable(true)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.filter)
                        .hint_text(i18n::tr(K::SearchHint))
                        .desired_width(300.0),
                );
                if ui
                    .add_enabled(
                        !state.fetch.busy(),
                        egui::Button::new(i18n::tr(K::RefreshNow)),
                    )
                    .clicked()
                {
                    request_refresh = true;
                }
                let unload = ui
                    .add_enabled(
                        can_unload
                            && selected_module
                                .as_ref()
                                .is_some_and(|module| module.unloadable)
                            && !state.unload.busy(),
                        egui::Button::new(i18n::tr(K::UnloadModule)),
                    )
                    .on_disabled_hover_text(if selected_module.is_none() {
                        i18n::tr(K::SelectModuleFirst)
                    } else {
                        i18n::tr(K::ModuleProtected)
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
                    if let Some(initial) = search::list_initial(ctx)
                        && let Some(base) = search::cycle_match(
                            rows.iter()
                                .map(|module| (module.base_address, module.name.as_str())),
                            state.selected_base,
                            initial,
                        )
                    {
                        state.selected_base = Some(base);
                        state.scroll_to_base = Some(base);
                    }
                    if let Some(nav) = search::list_nav(ctx) {
                        let current = state.selected_base.and_then(|base| {
                            rows.iter().position(|module| module.base_address == base)
                        });
                        let page_rows =
                            (ui.available_height() / tablekit::ROW_H).floor().max(1.0) as usize;
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
                    let mut table = app.make_table("modules", cols);
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
                        focus,
                        |ui, table, _avail, _content_width, range| {
                            for index in range {
                                let Some(module) = rows.get(index) else {
                                    continue;
                                };
                                let selected = state.selected_base == Some(module.base_address);
                                let (rect, response) = table.row(ui, pal, selected);
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
                                table.text_cell(ui, rect, 3, &module.path, pal, true);
                                if response.clicked() {
                                    state.selected_base = Some(module.base_address);
                                }
                                response.context_menu(|ui| {
                                    if ui.button(i18n::tr(K::CopyPath)).clicked() {
                                        ui.ctx().copy_text(module.path.clone());
                                        app.shared.toast(i18n::tr(K::Copied));
                                        ui.close();
                                    }
                                    if ui.button(i18n::tr(K::OpenFileLocation)).clicked() {
                                        if let Err(error) =
                                            app.actions.open_file_location(&module.path)
                                        {
                                            app.shared
                                                .toast(i18n::trf(K::ErrMsg, &[&error.to_string()]));
                                        }
                                        ui.close();
                                    }
                                    if ui.button(i18n::tr(K::Properties)).clicked() {
                                        if let Err(error) =
                                            app.actions.open_properties(&module.path)
                                        {
                                            app.shared
                                                .toast(i18n::trf(K::ErrMsg, &[&error.to_string()]));
                                        }
                                        ui.close();
                                    }
                                    ui.separator();
                                    let unload = ui
                                        .add_enabled(
                                            can_unload && module.unloadable && !state.unload.busy(),
                                            egui::Button::new(i18n::tr(K::UnloadModule)),
                                        )
                                        .on_disabled_hover_text(i18n::tr(K::ModuleProtected));
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
    if let Some(module) = state.pending_unload.as_ref() {
        let mut confirm_open = true;
        egui::Window::new(i18n::tr(K::UnloadModule))
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
                ui.horizontal(|ui| {
                    if ui.button(i18n::tr(K::Cancel)).clicked() {
                        confirmed = Some(false);
                    }
                    if ui.button(i18n::tr(K::UnloadModule)).clicked() {
                        confirmed = Some(true);
                    }
                });
            });
        if !confirm_open {
            confirmed = Some(false);
        }
    }
    if let Some(confirm) = confirmed {
        let module = state.pending_unload.take();
        if confirm && let Some(module) = module {
            begin_unload(app, &mut state, module, ctx);
        }
    }

    if open {
        app.module_dialog = Some(state);
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
            unloadable: true,
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

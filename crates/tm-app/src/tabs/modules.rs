//! Process Properties → Modules: the on-demand module list of one process.
//!
//! Enumeration and unloading never run on the UI thread. Unload is an
//! intentionally advanced action with a second confirmation; the platform
//! layer revalidates both process identity and the exact module base/path.
//!
//! Each module also shows the threads that START in it and whether those are
//! dormant (see `tabs::threads`). A module's code mostly runs on other
//! modules' threads, so a module that starts no thread is never called
//! dormant — there is no evidence either way.

use eframe::egui;
use std::cmp::Ordering;
use std::sync::{Arc, Mutex};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_platform::actions::{MODULE_UNLOAD_SINGLE_RELEASE_MARKER, PlatformActions, ProcessModule};

use crate::app::{InFlight, ProcessIdentity, TaskManApp};
use crate::search;
use crate::tabs::inspector;
use crate::tabs::threads::{Activity, ThreadRow};
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
    Threads,
    Activity,
    Path,
}

impl SortColumn {
    /// Display order of the columns.
    const ALL: [Self; 6] = [
        Self::Name,
        Self::Base,
        Self::Size,
        Self::Threads,
        Self::Activity,
        Self::Path,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Base => "base",
            Self::Size => "size",
            Self::Threads => "threads",
            Self::Activity => "activity",
            Self::Path => "path",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|column| column.id() == id)
    }
}

/// What the thread list proves about one module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleActivity {
    /// The thread list, or some thread's start address, is unavailable.
    Unknown,
    /// No thread starts here, and every start address is known.
    NoThreads,
    /// Its threads have not been watched long enough.
    Observing,
    /// At least one thread that started here ran recently.
    Active,
    /// Every thread that started here is dormant.
    Dormant,
}

impl ModuleActivity {
    fn rank(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Observing => 1,
            Self::Dormant => 2,
            Self::NoThreads => 3,
            Self::Unknown => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Unknown | Self::Observing => "—",
            Self::NoThreads => "",
            Self::Active => crate::tabs::threads::activity_label(Activity::Active),
            Self::Dormant => crate::tabs::threads::activity_label(Activity::Dormant {
                idle: crate::tabs::threads::DORMANT_AFTER,
            }),
        }
    }

    fn tip(self) -> &'static str {
        match self {
            Self::Unknown => i18n::tr(K::ThreadsNoAccessTip),
            Self::NoThreads => i18n::tr(K::ModuleNoThreadsTip),
            Self::Observing | Self::Active | Self::Dormant => i18n::tr(K::ModuleActivityTip),
        }
    }
}

/// Threads started in `module` (a lower bound when some start address is
/// unknown, flagged by the bool) and what they prove about it.
fn module_activity(
    module: &ProcessModule,
    threads: Option<&[ThreadRow]>,
) -> (Option<(usize, bool)>, ModuleActivity) {
    let Some(threads) = threads else {
        return (None, ModuleActivity::Unknown);
    };
    let unknown_starts = threads.iter().any(|row| row.thread.start_address.is_none());
    let started: Vec<&ThreadRow> = threads
        .iter()
        .filter(|row| {
            row.thread.start_address.is_some_and(|address| {
                address >= module.base_address && address - module.base_address < module.size_bytes
            })
        })
        .collect();
    if started.is_empty() {
        return if unknown_starts {
            (None, ModuleActivity::Unknown)
        } else {
            (Some((0, false)), ModuleActivity::NoThreads)
        };
    }
    let count = Some((started.len(), unknown_starts));
    let activity = if started.iter().any(|row| row.activity == Activity::Active) {
        ModuleActivity::Active
    } else if started
        .iter()
        .all(|row| matches!(row.activity, Activity::Dormant { .. }))
    {
        // A thread whose start is unknown might belong here and be running.
        if unknown_starts {
            ModuleActivity::Unknown
        } else {
            ModuleActivity::Dormant
        }
    } else {
        ModuleActivity::Observing
    };
    (count, activity)
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
    /// Whether the first enumeration was started.
    started: bool,
}

impl State {
    /// The page of `identity`. Nothing is enumerated until a page that needs
    /// the list is first shown ([`ensure_loaded`]).
    pub fn for_process(app: &TaskManApp, identity: ProcessIdentity, process_name: String) -> Self {
        let saved_sort = app.shared.settings.table_sort.get("modules");
        let sort = saved_sort
            .and_then(|saved| SortColumn::from_id(&saved.column))
            .unwrap_or(SortColumn::Name);
        let ascending = saved_sort.is_none_or(|saved| saved.ascending);
        Self::new(identity, process_name, sort, ascending)
    }

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
            started: false,
        }
    }

    /// The loaded module list, for the Threads page's start addresses.
    pub fn modules(&self) -> Option<Vec<ProcessModule>> {
        match &*tm_core::sync::lock(&self.load) {
            LoadState::Ready(modules) => Some(modules.clone()),
            _ => None,
        }
    }

    /// The unload confirmation is up and owns the keyboard.
    pub fn confirming(&self) -> bool {
        self.pending_unload.is_some()
    }
}

/// Start the first enumeration the first time a page needs the list.
pub fn ensure_loaded(app: &TaskManApp, state: &mut State, ctx: &egui::Context) {
    if !state.started && app.actions.capabilities().process_modules {
        state.started = true;
        begin_fetch(state, app.actions.clone(), ctx);
    }
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
        // Thread-derived columns are ordered by `compare_rows`.
        SortColumn::Threads | SortColumn::Activity => Ordering::Equal,
    };
    primary.then_with(|| a.base_address.cmp(&b.base_address))
}

struct Row {
    module: ProcessModule,
    threads: Option<(usize, bool)>,
    activity: ModuleActivity,
}

fn compare_rows(a: &Row, b: &Row, sort: SortColumn) -> Ordering {
    match sort {
        SortColumn::Threads => a
            .threads
            .map(|(count, _)| count)
            .cmp(&b.threads.map(|(count, _)| count))
            .then_with(|| compare_modules(&a.module, &b.module, SortColumn::Name)),
        SortColumn::Activity => a
            .activity
            .rank()
            .cmp(&b.activity.rank())
            .then_with(|| compare_modules(&a.module, &b.module, SortColumn::Name)),
        _ => compare_modules(&a.module, &b.module, sort),
    }
}

fn visible_modules(
    state: &State,
    modules: &[ProcessModule],
    threads: Option<&[ThreadRow]>,
) -> Vec<Row> {
    let needle = state.filter.trim().to_ascii_lowercase();
    let mut rows = modules
        .iter()
        .filter(|module| module_matches_filter(module, &needle))
        .map(|module| {
            let (threads, activity) = module_activity(module, threads);
            Row {
                module: module.clone(),
                threads,
                activity,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| tablekit::directed(compare_rows(a, b, state.sort), state.ascending));
    rows
}

fn module_matches_filter(module: &ProcessModule, needle: &str) -> bool {
    needle.is_empty()
        || module.name.to_ascii_lowercase().contains(needle)
        || module.path.to_ascii_lowercase().contains(needle)
}

/// The Modules page of Process Properties. `threads` is the Threads page's
/// newest list (the dialog keeps it current while this page is shown).
pub fn show(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    state: &mut State,
    threads: Option<&[ThreadRow]>,
    pal: &theme::Palette,
) {
    let ctx = ui.ctx().clone();
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

    // Tab is never consumed here, so egui's focus system moves between the
    // filter field, the toolbar buttons and the table.
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.filter)
                .hint_text(i18n::tr(K::SearchHint))
                .desired_width(300.0),
        );
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
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(i18n::tr(K::GatheringData));
            });
        }
        LoadState::Error(error) => {
            ui.colored_label(pal.heat_high, error);
        }
        LoadState::Ready(modules) => {
            let rows = visible_modules(state, modules, threads);
            // `app.modal_open()` runs while the properties dialog is taken,
            // so it stays false for the dialog itself — this list nav and
            // type-ahead are the page's own keyboard layer. The only thing
            // to stand down for is the unload confirmation, a separate
            // modal on top.
            let dialog_open = app.modal_open() || state.pending_unload.is_some();
            search::set_active_content(&ctx, "modules");
            let typed = search::list_type_ahead(&ctx, "modules", dialog_open);
            if let Some(typed) = typed
                && let Some(base) = search::type_ahead_match(
                    rows.iter()
                        .map(|row| (row.module.base_address, row.module.name.as_str()))
                        .collect::<Vec<_>>(),
                    state.selected_base,
                    &typed,
                )
            {
                state.selected_base = Some(base);
                state.scroll_to_base = Some(base);
            }
            if let Some(nav) = search::list_nav(&ctx, dialog_open) {
                let current = state
                    .selected_base
                    .and_then(|base| rows.iter().position(|row| row.module.base_address == base));
                let page_rows =
                    tablekit::page_rows(&ctx, "modules", tablekit::ROW_H_DENSE).unwrap_or(10);
                if let Some(index) = search::moved_index(rows.len(), current, nav, page_rows)
                    && let Some(row) = rows.get(index)
                {
                    state.selected_base = Some(row.module.base_address);
                    state.scroll_to_base = Some(row.module.base_address);
                }
            }

            let cols = vec![
                TmColumn::text("name", i18n::tr(K::ColName), 180.0),
                TmColumn::num("base", i18n::tr(K::ColBaseAddress), 150.0),
                TmColumn::num("size", i18n::tr(K::ColSize), 100.0),
                TmColumn::num("threads", i18n::tr(K::ColThreadCount), 80.0),
                TmColumn::text("activity", i18n::tr(K::ColActivity), 100.0),
                TmColumn::text("path", i18n::tr(K::ColPath), 360.0),
            ];
            let mut table = app
                .make_table("modules", cols)
                .with_row_height(tablekit::ROW_H_DENSE);
            let sorted = SortColumn::ALL
                .iter()
                .position(|column| *column == state.sort)
                .unwrap_or(0);
            let focus = state
                .scroll_to_base
                .take()
                .and_then(|base| rows.iter().position(|row| row.module.base_address == base));
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
                        let Some(row) = rows.get(index) else {
                            continue;
                        };
                        let module = &row.module;
                        let dim = row.activity == ModuleActivity::Dormant;
                        let selected = state.selected_base == Some(module.base_address);
                        let (rect, response) = table.row(ui, pal, selected, module.base_address);
                        table.describe_row(ui, &response, &module.name, index);
                        let name_truncated = table.text_cell(ui, rect, 0, &module.name, pal, dim);
                        let base_cell = table.col_rect(1, rect);
                        ui.painter_at(base_cell).text(
                            egui::pos2(base_cell.right() - 10.0, base_cell.center().y),
                            egui::Align2::RIGHT_CENTER,
                            format!("0x{:016X}", module.base_address),
                            egui::FontId::monospace(tablekit::FONT_ROW),
                            if dim { pal.text_dim } else { pal.text },
                        );
                        let size = format::format_bytes_loc(module.size_bytes);
                        inspector::num_cell(ui, table, rect, 2, &size, pal, dim);
                        let count = match row.threads {
                            Some((count, false)) => count.to_string(),
                            Some((count, true)) => format!("{count}+"),
                            None => "—".to_string(),
                        };
                        inspector::num_cell(ui, table, rect, 3, &count, pal, dim);
                        table.text_cell(ui, rect, 4, row.activity.label(), pal, dim);
                        let path_truncated = table.text_cell(ui, rect, 5, &module.path, pal, dim);
                        // A clipped cell cannot show its content; the full
                        // value becomes the row tooltip (before the context
                        // menu attaches). The thread-derived cells explain
                        // themselves.
                        let pointer = ui.ctx().pointer_latest_pos();
                        let over = |column: usize| {
                            pointer.is_some_and(|p| table.col_rect(column, rect).contains(p))
                        };
                        let response = if over(3) || over(4) {
                            response.on_hover_text(row.activity.tip())
                        } else if path_truncated {
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
                                if let Err(error) = app.actions.open_file_location(&module.path) {
                                    app.shared
                                        .toast(i18n::trf(K::ErrMsg, &[&error.to_string()]));
                                }
                                ui.close();
                            }
                            if menu::item(ui, i18n::tr(K::Properties)).clicked() {
                                if let Err(error) = app.actions.open_properties(&module.path) {
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
            if let Some(column) = clicked.and_then(|index| SortColumn::ALL.get(index).copied()) {
                if state.sort == column {
                    state.ascending = !state.ascending;
                } else {
                    state.sort = column;
                    state.ascending = !matches!(
                        column,
                        SortColumn::Base | SortColumn::Size | SortColumn::Threads
                    );
                }
                app.persist_sort("modules", state.sort.id(), state.ascending);
            }
            app.persist_table(&table);
        }
    }

    if request_refresh {
        begin_fetch(state, app.actions.clone(), &ctx);
    }
    if let Some(module) = request_unload {
        state.pending_unload = Some(module);
    }
}

/// The second confirmation of an unload, drawn on top of the properties
/// dialog after it. Owns the keyboard while it is up (see
/// [`State::confirming`]).
pub fn unload_confirmation(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    state: &mut State,
    pal: &theme::Palette,
) {
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
        // page behind, so dismissing hands it back.
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
            begin_unload(app, state, module, ctx);
        }
    }
}

/// The properties dialog closed: a confirmation parked without a decision
/// must not leave its restore temp behind, or the next confirmation would
/// restore a stale widget instead of capturing its own opener.
pub fn forget_confirmation(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.remove_temp::<Option<egui::Id>>(egui::Id::new("unload-confirm-restore-focus"))
    });
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
        assert_eq!(visible_modules(&state, &modules, None).len(), 1);
        state.filter = "test\\beta".into();
        assert_eq!(
            visible_modules(&state, &modules, None)[0].module.name,
            "beta.dll"
        );
    }

    fn thread_row(tid: u32, start: Option<u64>, activity: Activity) -> ThreadRow {
        ThreadRow {
            thread: tm_platform::actions::ProcessThread {
                tid,
                start_address: start,
                priority: 8,
                base_priority: 8,
                context_switches: 0,
                kernel_time_100ns: 0,
                user_time_100ns: 0,
                created_epoch_s: Some(1),
                state: tm_platform::actions::ThreadState::Waiting,
                wait_reason: 6,
                cycles: None,
            },
            cpu_pct: None,
            cycles_delta: None,
            activity,
        }
    }

    const DORMANT: Activity = Activity::Dormant {
        idle: crate::tabs::threads::DORMANT_AFTER,
    };

    #[test]
    fn a_module_is_dormant_only_when_every_thread_it_started_is() {
        let dll = module("worker.dll", 0x1000, 0x1000);
        let threads = [
            thread_row(1, Some(0x1100), DORMANT),
            thread_row(2, Some(0x1200), DORMANT),
            thread_row(3, Some(0x9000), Activity::Active),
        ];
        assert_eq!(
            module_activity(&dll, Some(&threads)),
            (Some((2, false)), ModuleActivity::Dormant)
        );
        let threads = [
            thread_row(1, Some(0x1100), DORMANT),
            thread_row(2, Some(0x1200), Activity::Active),
        ];
        assert_eq!(
            module_activity(&dll, Some(&threads)).1,
            ModuleActivity::Active
        );
        let threads = [
            thread_row(1, Some(0x1100), DORMANT),
            thread_row(2, Some(0x1200), Activity::Observing),
        ];
        assert_eq!(
            module_activity(&dll, Some(&threads)).1,
            ModuleActivity::Observing
        );
    }

    #[test]
    fn a_module_without_threads_or_evidence_is_never_called_dormant() {
        let dll = module("helper.dll", 0x1000, 0x1000);
        let elsewhere = [thread_row(1, Some(0x9000), DORMANT)];
        assert_eq!(
            module_activity(&dll, Some(&elsewhere)),
            (Some((0, false)), ModuleActivity::NoThreads)
        );
        // An unreadable start address could belong to this module.
        let unknown = [
            thread_row(1, Some(0x1100), DORMANT),
            thread_row(2, None, DORMANT),
        ];
        assert_eq!(
            module_activity(&dll, Some(&unknown)),
            (Some((1, true)), ModuleActivity::Unknown)
        );
        assert_eq!(module_activity(&dll, None), (None, ModuleActivity::Unknown));
    }
}

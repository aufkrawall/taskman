//! Startup apps tab: "Autostart von Apps" — Name/Herausgeber/Status/
//! Startauswirkung table with "Letzte BIOS-Zeit" top right and the
//! Aktivieren/Deaktivieren/Eigenschaften command bar.

use eframe::egui;
use std::cmp::Ordering;
use std::time::{Duration, Instant};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::{StartupImpact, StartupItem};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::search;
use crate::theme;
use crate::widgets::menu;
use crate::widgets::tablekit::{self, TmColumn};

fn columns() -> Vec<TmColumn> {
    vec![
        TmColumn::text("name", i18n::tr(K::ColName), 280.0),
        TmColumn::text("pub", i18n::tr(K::ColPublisher), 240.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 140.0),
        TmColumn::text("impact", i18n::tr(K::ColImpact), 150.0),
    ]
}

/// The candidate fields the Startup page matches the global search against —
/// name, publisher, command and location. The ONE predicate the visible table
/// and the search commit both use, so Enter can never land on a row the table
/// would not show.
pub(crate) fn matches_search(q: &search::Query, item: &StartupItem) -> bool {
    q.matches_any([
        item.name.as_str(),
        item.publisher.as_deref().unwrap_or(""),
        item.command.as_str(),
        item.location.as_str(),
    ])
}

/// The startup entry the global search commits to: the FIRST row of the
/// table's current model — filtered by `q`, sorted by the live `sort` — i.e.
/// exactly the row the user sees at the top of the matches. Returns the item
/// id (the row-owner key the page selects by). Split out so the commit in
/// `app.rs` reuses this module's row model instead of keeping a sort-order
/// mirror of it in sync.
pub(crate) fn first_search_match_in_display_order(
    items: &[StartupItem],
    q: &search::Query,
    sort: tablekit::SortState,
) -> Option<String> {
    let mut visible: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| matches_search(q, item))
        .map(|(i, _)| i)
        .collect();
    visible.sort_by(|a, b| compare_items(&items[*a], &items[*b], sort));
    visible.first().map(|&i| items[i].id.clone())
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let frame_ctx = ui.ctx().clone();

    {
        let stale = {
            let guard = tm_core::sync::lock(&app.shared.startup_cache);
            match guard.as_ref() {
                Some((_, t)) => t.elapsed() > Duration::from_secs(10),
                None => true,
            }
        };
        if stale && app.shared.startup_fetch.begin() {
            let cache = app.shared.startup_cache.clone();
            let toasts = app.shared.toasts.clone();
            let done = app.shared.startup_fetch.flag();
            let actions = app.actions.clone();
            let wake = {
                let ctx = frame_ctx.clone();
                move || ctx.request_repaint()
            };
            let job = move || {
                let items = actions.list_startup();
                if let Err(e) = &items {
                    crate::app::toast_from(
                        &toasts,
                        i18n::trf(K::StartupUnavailable, &[&e.to_string()]),
                    );
                }
                *tm_core::sync::lock(&cache) = Some((items.unwrap_or_default(), Instant::now()));
                done.store(false, std::sync::atomic::Ordering::Relaxed);
                wake();
            };
            match &app.shared.executor {
                Some(executor) => {
                    if !executor.run_quiet(|| {}, job) {
                        app.shared.startup_fetch.end();
                        app.shared.toast(i18n::tr(K::ActionQueueFull));
                    }
                }
                None => {
                    drop(job);
                    app.shared.startup_fetch.end();
                    app.shared.toast(i18n::tr(K::ActionFailed));
                }
            }
        }
    }

    let selected_id = app.selected_startup_id.clone();
    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app, ui| {
            let sel: Option<StartupItem> = selected_id.as_ref().and_then(|id| {
                let guard = tm_core::sync::lock(&app.shared.startup_cache);
                guard
                    .as_ref()
                    .and_then(|(v, _)| v.iter().find(|it| &it.id == id))
                    .cloned()
            });
            let can_enable = sel.as_ref().is_some_and(|s| !s.enabled);
            let can_disable = sel.as_ref().is_some_and(|s| s.enabled);
            if crate::app_ui::cmd_button(ui, &pal, Icon::Check, i18n::tr(K::EnableCmd), can_enable)
            {
                toggle_selected(app, true, ui.ctx());
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::SlashCircle,
                i18n::tr(K::DisableCmd),
                can_disable,
            ) {
                toggle_selected(app, false, ui.ctx());
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Properties,
                i18n::tr(K::Properties),
                sel.is_some(),
            ) {
                app.startup_props = sel.clone();
            }
        },
        |app, ui| {
            if menu::item(ui, i18n::tr(K::RefreshNow)).clicked() {
                *tm_core::sync::lock(&app.shared.startup_cache) = None;
                app.refresh_all();
                ui.close();
            }
        },
    );

    if let Some(ms) = app.actions.last_bios_time_ms() {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
        let text = format!(
            "{}   {} {}",
            i18n::tr(K::LastBiosTime),
            format::format_seconds(ms as f64 / 1000.0),
            i18n::tr(K::SecondsSuffix)
        );
        ui.painter().text(
            egui::Pos2::new(rect.right() - 6.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            text,
            egui::FontId::proportional(13.0),
            pal.text,
        );
    }

    let cache_arc = app.shared.startup_cache.clone();
    let mut guard = tm_core::sync::lock(&cache_arc);
    let Some((items, _)) = guard.as_mut() else {
        ui.centered_and_justified(|ui| ui.label(i18n::tr(K::GatheringData)));
        return;
    };

    let q = crate::search::Query::new(&app.search);
    let mut table = app.make_table("startup", columns());
    let mut visible: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches_search(&q, it))
        .map(|(i, _)| i)
        .collect();
    let sort = app.startup_sort;
    visible.sort_by(|a, b| compare_items(&items[*a], &items[*b], sort));

    // While a dialog is up it owns the keyboard: the page's nav, row keys and
    // menu key all stand down (`TaskManApp::modal_open`).
    let dialog_open = app.modal_open();
    // Arrow/Home/End/Page selection movement over the displayed startup
    // items. The item id is the row-owner key: the one-shot scroll request
    // parks under that identity and resolves to a row index per frame.
    if search::nav_gate(&frame_ctx, dialog_open) {
        let page_rows =
            tablekit::page_rows(&frame_ctx, "startup", tablekit::ROW_H).unwrap_or_else(|| {
                (frame_ctx.content_rect().height() / tablekit::ROW_H)
                    .floor()
                    .max(1.0) as usize
            });
        let current = app
            .selected_startup_id
            .as_ref()
            .and_then(|id| visible.iter().position(|&i| items[i].id == *id));
        if let Some(nav) = search::list_nav(&frame_ctx, dialog_open)
            .filter(|_| !tablekit::header_has_focus(&frame_ctx, "startup"))
            && let Some(next) = search::moved_index(visible.len(), current, nav, page_rows)
            && let Some(&i) = visible.get(next)
        {
            app.selected_startup_id = Some(items[i].id.clone());
            tablekit::request_row_scroll(
                &frame_ctx,
                "startup",
                tablekit::stable_key(items[i].id.as_str()),
            );
        }
    }
    let focus_row = tablekit::take_row_scroll(&frame_ctx, "startup").and_then(|key| {
        visible
            .iter()
            .position(|&i| tablekit::stable_key(items[i].id.as_str()) == key)
    });

    let mut fit: Vec<f32> = table
        .cols
        .iter()
        .map(|c| tablekit::text_width(ui, c.label, tablekit::FONT_HDR_LABEL) + 28.0)
        .collect();
    for &i in &visible {
        let item = &items[i];
        let status = if item.enabled {
            i18n::tr(K::EnabledWord)
        } else {
            i18n::tr(K::DisabledWord)
        };
        let values = [
            item.name.as_str(),
            item.publisher.as_deref().unwrap_or(""),
            status,
            impact_label(app.lang(), item.impact),
        ];
        fit[0] = fit[0].max(tablekit::text_width(ui, values[0], tablekit::FONT_ROW) + 66.0);
        for i in 1..4 {
            fit[i] = fit[i].max(tablekit::text_width(ui, values[i], tablekit::FONT_ROW) + 22.0);
        }
    }
    table.apply_auto_fit(fit);

    let avail = crate::widgets::tablekit::table_avail(ui);
    let clicked = tablekit::scrolled_rows(
        "startup",
        ui,
        &pal,
        &mut table,
        avail,
        Some((app.startup_sort.column, app.startup_sort.ascending)),
        None,
        visible.len(),
        (!q.is_empty()).then_some(i18n::tr(K::NoMatches)),
        focus_row,
        None,
        |ui, table, _avail, _content_w, range| {
            for vi in range {
                let i = visible[vi];
                let item = &mut items[i];
                let selected = app.selected_startup_id.as_deref() == Some(item.id.as_str());
                let (rect, resp) = table.row(ui, &pal, selected, item.id.as_str());

                let exe = exe_from_command(&item.command);
                let tex = exe
                    .as_deref()
                    .and_then(|p| app.shared.icons.get(ui.ctx(), &app.actions, p, 4));
                table.icon_cell(ui, rect, tex.as_ref(), pal.accent);
                let name_rect = table.col_rect(0, rect);
                ui.painter_at(name_rect).text(
                    egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &item.name,
                    egui::FontId::proportional(tablekit::FONT_ROW),
                    pal.text,
                );

                let pub_truncated = table.text_cell(
                    ui,
                    rect,
                    1,
                    item.publisher.as_deref().unwrap_or(""),
                    &pal,
                    false,
                );
                table.text_cell(
                    ui,
                    rect,
                    2,
                    if item.enabled {
                        i18n::tr(K::EnabledWord)
                    } else {
                        i18n::tr(K::DisabledWord)
                    },
                    &pal,
                    false,
                );
                let measured = item.enabled.then(|| measured_impact(app, item)).flatten();
                table.text_cell(
                    ui,
                    rect,
                    3,
                    match &measured {
                        Some(sample) => impact_label(app.lang(), sample.classify()),
                        None => impact_label(app.lang(), item.impact),
                    },
                    &pal,
                    false,
                );

                // A clipped cell cannot show its content; the full value
                // becomes the row tooltip (before the context menu attaches).
                let resp = if pub_truncated && item.publisher.is_some() {
                    resp.on_hover_text(item.publisher.as_deref().unwrap_or_default())
                } else if let Some(sample) = &measured {
                    resp.on_hover_text(impact_tooltip(sample))
                } else {
                    resp
                };

                if resp.clicked() {
                    app.selected_startup_id = Some(item.id.clone());
                }
                // Enter (with nothing else holding focus) opens the same menu
                // as the Menu key: enable/disable is the closest thing to a
                // primary action a startup entry has. Both stand down while a
                // dialog is up — a menu opened over a dialog would strand
                // there once the dialog contract consumes Escape.
                let selected_row = app.selected_startup_id.as_deref() == Some(item.id.as_str());
                let enter_open = search::row_action_gate(ui.ctx(), app.modal_open())
                    && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter))
                    && selected_row;
                let keyboard_open = (enter_open
                    || (!ui.ctx().any_popup_open() && menu::keyboard_menu_requested(ui.ctx())))
                    && !app.modal_open()
                    && selected_row;
                menu::context_menu_kb(&resp, keyboard_open, |ui| {
                    ui.set_min_width(180.0);
                    let ctx = ui.ctx().clone();
                    let label = if item.enabled {
                        i18n::tr(K::DisableCmd)
                    } else {
                        i18n::tr(K::EnableCmd)
                    };
                    if menu::item(ui, label).clicked() {
                        let new_enabled = !item.enabled;
                        let id = item.id.clone();
                        let location = item.location.clone();
                        if toggle_item(app, &ctx, id, location, new_enabled) {
                            item.enabled = new_enabled;
                        }
                        ui.close();
                    }
                    if menu::item(ui, i18n::tr(K::Properties)).clicked() {
                        app.startup_props = Some(item.clone());
                        ui.close();
                    }
                    if menu::item(ui, i18n::tr(K::OnlineSearch)).clicked() {
                        let url = crate::search::online_search_url(&item.name);
                        if let Err(e) = app.actions.open_url(&url) {
                            app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()]));
                        }
                        ui.close();
                    }
                    if menu::item(ui, i18n::tr(K::OpenFileLocation)).clicked()
                        && let Some(exe) = exe_from_command(&item.command)
                        && let Err(e) = app.actions.open_file_location(&exe)
                    {
                        app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()]));
                    }
                });
            }
            ui.add_space(12.0);
        },
    );
    if let Some(column) = clicked {
        app.startup_sort.clicked(column, false);
        let ids = ["name", "pub", "status", "impact"];
        app.persist_sort("startup", ids[column], app.startup_sort.ascending);
    }
    app.persist_table(&table);
}

fn compare_items(a: &StartupItem, b: &StartupItem, sort: tablekit::SortState) -> Ordering {
    let primary = match sort.column {
        0 => tablekit::cmp_ignore_case(&a.name, &b.name),
        1 => tablekit::cmp_ignore_case(
            a.publisher.as_deref().unwrap_or(""),
            b.publisher.as_deref().unwrap_or(""),
        ),
        2 => a.enabled.cmp(&b.enabled),
        _ => impact_rank(a.impact).cmp(&impact_rank(b.impact)),
    };
    tablekit::directed(primary, sort.ascending)
        .then_with(|| tablekit::cmp_ignore_case(&a.name, &b.name))
}

fn impact_rank(impact: StartupImpact) -> u8 {
    match impact {
        StartupImpact::None => 0,
        StartupImpact::Low => 1,
        StartupImpact::Medium => 2,
        StartupImpact::High => 3,
        StartupImpact::Unknown => 4,
    }
}

/// Dispatch one enable/disable write. Returns whether the job was dispatched
/// (the caller flips the cached row optimistically then). The JOB rolls the
/// cached row back when the SCM write fails, so the table never shows a state
/// the error toast calls a failure — the revert runs on the worker thread,
/// where the UI-side cache lock is not held.
///
/// `id` and `location` are captured at DISPATCH time. Both callers hold the
/// startup cache lock for the whole frame, so a job that looked them up itself
/// would block on that lock — and would silently substitute an empty location
/// for a row a refresh had meanwhile replaced.
fn toggle_item(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    id: String,
    location: String,
    new_enabled: bool,
) -> bool {
    let actions = app.actions.clone();
    let cache = app.shared.startup_cache.clone();
    let ok_msg = move || {
        if new_enabled {
            i18n::tr(K::EnabledWord).to_string()
        } else {
            i18n::tr(K::DisabledWord).to_string()
        }
    };
    app.run_action(ctx, ok_msg, move || {
        let result = actions.set_startup_enabled(&id, &location, new_enabled);
        if result.is_err() {
            let mut guard = tm_core::sync::lock(&cache);
            if let Some((items, _)) = guard.as_mut()
                && let Some(item) = items.iter_mut().find(|it| it.id == id)
            {
                item.enabled = !new_enabled;
            }
        }
        result
    })
}

fn toggle_selected(app: &mut TaskManApp, enable: bool, ctx: &egui::Context) {
    let guard = app.shared.startup_cache.clone();
    let mut cache = tm_core::sync::lock(&guard);
    if let Some((items, _)) = cache.as_mut()
        && let Some(id) = app.selected_startup_id.clone()
        && let Some(item) = items.iter_mut().find(|it| it.id == id)
    {
        let location = item.location.clone();
        if toggle_item(app, ctx, id, location, enable) {
            item.enabled = enable;
        }
    }
}

/// Measured impact for a startup item, when this machine has a measurement
/// for the image its command launches.
///
/// The store is keyed by normalized image path (the same key per-image rules
/// use), so the command has to resolve to a file that exists. A command that
/// cannot be resolved simply has no measurement — the column then keeps
/// "Not measured" instead of guessing which process an item launched.
fn measured_impact(
    app: &TaskManApp,
    item: &tm_core::model::StartupItem,
) -> Option<tm_core::ImpactSample> {
    let target = resolve_startup_target(&item.command)?;
    let key = tm_core::settings::process_rule_key(std::path::Path::new(&target));
    app.startup_impact.get(&key).copied()
}

/// Resolve the image a startup command launches.
///
/// Windows resolves quoted paths, environment variables and `.lnk` targets;
/// the other backends only split off the leading token, which is enough for
/// their own startup lists.
fn resolve_startup_target(command: &str) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        tm_platform::win::startup_command_target(command)
    }
    #[cfg(not(target_os = "windows"))]
    {
        exe_from_command(command)
    }
}

/// Tooltip under a measured impact: the raw numbers, so the Low/Medium/High
/// judgement can be checked rather than trusted.
fn impact_tooltip(sample: &tm_core::ImpactSample) -> String {
    i18n::trf(
        K::ImpactMeasuredTip,
        &[
            &format::format_cpu_time(sample.cpu_ms / 1000.0),
            &format::format_bytes_loc(sample.disk_bytes),
        ],
    )
}

fn impact_label(lang: tm_core::i18n::Lang, impact: StartupImpact) -> &'static str {
    match impact {
        StartupImpact::None => i18n::tr_in(lang, K::ImpactNone),
        StartupImpact::Low => i18n::tr_in(lang, K::ImpactLow),
        StartupImpact::Medium => i18n::tr_in(lang, K::ImpactMedium),
        StartupImpact::High => i18n::tr_in(lang, K::ImpactHigh),
        StartupImpact::Unknown => i18n::tr_in(lang, K::ImpactUnknown),
    }
}

fn exe_from_command(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix('"')
        && let Some(exe) = rest.split('"').next()
    {
        return Some(exe.to_string());
    }
    cmd.split_whitespace().next().map(str::to_string)
}

pub fn properties_dialog(app: &mut TaskManApp, ctx: &egui::Context, _pal: &theme::Palette) {
    let restore_id = egui::Id::new("startup-props-restore-focus");
    let first_frame = ctx
        .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
        .is_none();
    if first_frame {
        // Remember what held keyboard focus before the dialog took it, so
        // closing can hand it back.
        let captured = crate::app_ui::capture_focus(ctx);
        ctx.data_mut(|d| d.insert_temp(restore_id, captured));
    }
    let mut open = true;
    // Esc and Enter close, mirroring the Close button.
    let keys = crate::app_ui::consume_dialog_keys(ctx, false);
    let close_now = keys.escape || keys.enter;
    egui::Window::new(i18n::tr(K::Properties))
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
                    ui.weak(i18n::tr(K::ColName));
                    ui.label(&item.name);
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColPublisher));
                    ui.label(item.publisher.clone().unwrap_or_default());
                    ui.end_row();
                    ui.weak(i18n::tr(K::PropCommand));
                    ui.label(&item.command);
                    ui.end_row();
                    ui.weak(i18n::tr(K::PropLocation));
                    ui.label(&item.location);
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColStatus));
                    ui.label(if item.enabled {
                        i18n::tr(K::EnabledWord)
                    } else {
                        i18n::tr(K::DisabledWord)
                    });
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(i18n::tr(K::Close)).clicked() {
                        app.startup_props = None;
                    }
                });
            });
        });
    if !open || close_now {
        // Hand keyboard focus back to what held it before the dialog took it.
        let saved = ctx
            .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
            .flatten();
        crate::app_ui::restore_focus(ctx, saved);
        ctx.data_mut(|d| d.remove_temp::<Option<egui::Id>>(restore_id));
        app.startup_props = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The global search commit must mirror the VISIBLE startup table: same
    /// filter fields, same live sort — name ascending and descending lead to
    /// different first rows for one and the same query.
    #[test]
    fn search_commit_follows_the_live_sort() {
        let items = vec![
            StartupItem {
                id: "alpha".into(),
                name: "Alpha".into(),
                ..Default::default()
            },
            StartupItem {
                id: "beta".into(),
                name: "Beta".into(),
                ..Default::default()
            },
        ];
        let q = crate::search::Query::new("a");
        assert!(matches_search(&q, &items[0]));
        assert!(
            matches_search(&q, &items[1]),
            "Beta matches via its name too"
        );
        assert_eq!(
            first_search_match_in_display_order(&items, &q, tablekit::SortState::new(0, true))
                .as_deref(),
            Some("alpha")
        );
        assert_eq!(
            first_search_match_in_display_order(&items, &q, tablekit::SortState::new(0, false))
                .as_deref(),
            Some("beta")
        );
        assert_eq!(
            first_search_match_in_display_order(
                &items,
                &crate::search::Query::new("zzz"),
                tablekit::SortState::new(0, true)
            ),
            None,
            "no match, no commit"
        );
    }
}

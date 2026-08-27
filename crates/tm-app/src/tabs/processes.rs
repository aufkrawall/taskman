//! Processes tab: Apps / Background processes / Windows process groups,
//! expandable parent→child trees of arbitrary depth with O(n) subtree
//! aggregates, blue heat-mapped resource columns and the aggregate header.
//! Rows are flattened into a display model and rendered through the fixed-
//! height virtualizer, so row count does not affect frame cost.
//!
//! Correctness notes from the 2026 audit:
//! * App rows are presentation groups, not raw parent-process trees. Shell
//!   launchers such as Explorer do not absorb programs the user launched;
//!   each visible app family is promoted to its own top-level row.
//! * Group header counts never depend on expansion state (P0.4). Apps counts
//!   top-level app groups like native Task Manager; Background/Windows count
//!   their unflattened process members.
//! * Grouped labels like `Brave Browser (43)` report the WHOLE display
//!   subtree process count, computed in one O(n) pass, not direct children
//!   (P0.5).
//! * Heat intensities are normalized per COLUMN over the whole display
//!   model before virtualization (audit P0.2) — `heat_cells` only paints.

use eframe::egui;
use std::collections::{HashMap, HashSet};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::{ProcCategory, ProcStatus, ProcessEntry, Snapshot};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::search;
use crate::theme;
use crate::widgets::tablekit::{self, Aggregates, HeatCell, TmColumn};

fn columns() -> Vec<TmColumn> {
    vec![
        TmColumn::text("name", i18n::tr(K::ColName), 340.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 190.0),
        TmColumn::num("cpu", i18n::tr(K::ColCpu), 110.0),
        TmColumn::num("mem", i18n::tr(K::ColMemory), 110.0),
        TmColumn::num("disk", i18n::tr(K::ColDisk), 110.0),
        // Per-process network is unsupported on Windows; the column renders
        // an honest "—" instead of a fake zero (implement.md §10/§16.6).
        TmColumn::num("net", i18n::tr(K::ColNetwork), 110.0),
    ]
}

/// Flattened display row — group headers and process rows share one fixed
/// height so `show_rows` can virtualize the whole list.
#[derive(Debug, Clone)]
pub enum DisplayRow {
    /// Group index (0=Apps 1=Background 2=Windows) + the native-style group
    /// count, computed before flattening so expansion state cannot change it.
    GroupHeader(u8, usize),
    Process(RowData),
}

#[derive(Debug, Clone)]
pub struct RowData {
    pub pid: u32,
    pub start_epoch_s: Option<i64>,
    pub depth: usize,
    pub name: String,
    pub icon_path: Option<String>,
    pub children: bool,
    /// cpu %, mem bytes, disk bps, net bps (aggregated over the display subtree).
    pub values: [f64; 4],
    /// Per-column heat intensity normalized against the WHOLE display model
    /// before virtualization (audit P0.2): exactly 1.0 marks the column's
    /// top consumer.
    pub heat: [f32; 4],
    pub net_available: bool,
    pub suspended: bool,
    /// True when the OS reports this process as power throttled
    /// (Efficiency mode) — rendered straight from the snapshot, never from
    /// client-side bookkeeping (audit §8).
    pub power_throttled: bool,
}

#[derive(Default)]
pub struct State {
    pub sort_col: usize,
    pub ascending: bool,
    /// Expanded parent pids.
    pub expanded: HashSet<u32>,
    /// Expanded user rows (Benutzer tab reuses the same state).
    pub expanded_users: HashSet<u32>,
    /// Collapsed group headers [Apps, Background, Windows].
    pub group_collapsed: [bool; 3],
    /// Process selected by keyboard type-navigation and waiting to be scrolled
    /// into the virtualized visible range.
    scroll_to_pid: Option<u32>,
    cache: Option<Cache>,
    view_generation: u64,
}

impl State {
    /// TM default: sorted by name ascending.
    pub fn new() -> Self {
        Self {
            ascending: true,
            ..Default::default()
        }
    }

    /// Any change that affects the flattened display model bumps this
    /// generation so caches rebuild immediately instead of lagging one tick
    /// (expand/collapse, group toggles).
    pub fn invalidate(&mut self) {
        self.view_generation += 1;
    }

    pub fn toggle_expanded(&mut self, pid: u32) {
        if !self.expanded.remove(&pid) {
            self.expanded.insert(pid);
        }
        self.invalidate();
    }

    pub fn toggle_group(&mut self, gi: usize) {
        if gi < self.group_collapsed.len() {
            self.group_collapsed[gi] = !self.group_collapsed[gi];
            self.invalidate();
        }
    }
}

struct Cache {
    key: (u64, u64, String, usize, bool),
    rows: Vec<DisplayRow>,
}

/// Issue an Efficiency-mode toggle driven by the SNAPSHOT's OS-reported
/// state, then force one fresh sample so the UI re-renders truth (audit §8):
/// no client-side bookkeeping can claim a state Windows does not confirm.
pub(crate) fn toggle_efficiency_mode(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    target: &crate::app::ProcessIdentity,
) {
    // Same identity discipline as End Task (audit §7): never act on a
    // recycled PID.
    if !app.identity_is_live(target) {
        app.shared.toast(i18n::tr(K::ProcessExited));
        return;
    }
    let snap = app.latest_snapshot();
    let Some(p) = snap.as_ref().and_then(|s| s.process(target.pid)) else {
        return;
    };
    let on = !p.power_throttled.unwrap_or(false);
    let actions = app.actions.clone();
    let pid = target.pid;
    app.run_action(
        ctx,
        || i18n::tr(K::EfficiencyChanged).to_string(),
        move || actions.set_efficiency_mode(pid, on),
    );
    // Even when sampling is paused, produce exactly one fresh sample so the
    // leaf/state re-render from returned OS state immediately.
    app.engine.request_refresh();
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let Some(snap) = app.latest_snapshot() else {
        ui.centered_and_justified(|ui| ui.label(i18n::tr(K::GatheringData)));
        return;
    };

    let caps = app.actions.capabilities();

    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app, ui| {
            let ctx = ui.ctx().clone();
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Leaf,
                i18n::tr(K::EfficiencyMode),
                caps.efficiency_mode && app.selected_process.is_some(),
            ) && let Some(identity) = app.selected_process.clone()
            {
                toggle_efficiency_mode(app, &ctx, &identity);
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Close,
                i18n::tr(K::EndTask),
                app.selected_process.is_some(),
            ) {
                app.end_selected(&ctx);
            }
        },
        |app, ui| {
            if ui.button(i18n::tr(K::ExpandAll)).clicked() {
                if let Some(snap) = app.latest_snapshot() {
                    for p in &snap.processes {
                        app.processes_state.expanded.insert(p.pid);
                    }
                }
                app.processes_state.invalidate();
                ui.close();
            }
            if ui.button(i18n::tr(K::CollapseAll)).clicked() {
                app.processes_state.expanded.clear();
                app.processes_state.invalidate();
                ui.close();
            }
        },
    );

    let mut table = app.make_table("processes", columns());

    // Rebuild the flattened model only when snapshot/search/sort/view-state
    // changes — expansion is part of the key (§11.1).
    let key = (
        snap.timestamp_ms,
        app.processes_state.view_generation,
        app.search.clone(),
        app.processes_state.sort_col,
        app.processes_state.ascending,
    );
    let mut cache = app.processes_state.cache.take();
    let cache_stale = cache.as_ref().is_none_or(|c| c.key != key);
    if cache_stale {
        let expanded = app.processes_state.expanded.clone();
        let groups = app.processes_state.group_collapsed;
        cache = Some(Cache {
            key: key.clone(),
            rows: build_display_rows(&snap, &key.2, key.3, key.4, &expanded, &groups),
        });
    }
    let rows = cache.as_ref().expect("cache").rows.clone();

    // Task-Manager-style type navigation: a plain letter selects the next
    // visible process beginning with that letter. Repeated presses cycle and
    // wrap in the exact flattened/sorted order shown on screen.
    if let Some(initial) = search::list_initial(ui.ctx()) {
        let selected = app.selected_process.as_ref().map(|p| p.pid);
        if let Some(pid) = search::cycle_process_initial(
            rows.iter().filter_map(|row| match row {
                DisplayRow::Process(row) => Some((row.pid, row.name.as_str())),
                DisplayRow::GroupHeader(..) => None,
            }),
            selected,
            initial,
        ) && let Some(row) = rows.iter().find_map(|row| match row {
            DisplayRow::Process(row) if row.pid == pid => Some(row),
            _ => None,
        }) {
            app.selected_process = Some(crate::app::ProcessIdentity {
                pid: row.pid,
                start_epoch_s: row.start_epoch_s,
            });
            app.processes_state.scroll_to_pid = Some(row.pid);
        }
    }

    let agg = Aggregates::from_snapshot(&snap);
    let aggs = agg.strings();
    prepare_auto_fit_widths(ui, &mut table, &rows, &aggs);

    let avail = tablekit::table_avail(ui);
    let clicked = tablekit::scrolled_rows(
        "processes",
        ui,
        &pal,
        &mut table,
        avail,
        Some((app.processes_state.sort_col, app.processes_state.ascending)),
        Some(&aggs),
        rows.len(),
        |ui, table, _avail, content_w, range| {
            for i in range {
                match rows.get(i) {
                    Some(DisplayRow::GroupHeader(gi, total)) => {
                        group_header(app, ui, &pal, *gi, *total, content_w);
                    }
                    Some(DisplayRow::Process(row)) => {
                        row_ui(app, ui, &pal, table, row);
                    }
                    None => {}
                }
            }
        },
    );
    if let Some(col) = clicked {
        if app.processes_state.sort_col == col {
            app.processes_state.ascending = !app.processes_state.ascending;
        } else {
            app.processes_state.sort_col = col;
            // Numeric columns default descending, name ascending.
            app.processes_state.ascending = col == 0 || !table.cols[col].numeric;
        }
    }
    app.persist_table(&table);
    app.processes_state.cache = cache;
}

/// Intrinsic widths come from the complete flattened display model, never
/// the virtualized visible range. This makes separator double-click stable
/// regardless of scroll position and accounts for tree indentation/icons.
fn prepare_auto_fit_widths(
    ui: &egui::Ui,
    table: &mut tablekit::TmTable,
    rows: &[DisplayRow],
    aggs: &[String; 4],
) {
    let header = |i: usize| tablekit::text_width(ui, table.cols[i].label, tablekit::FONT_HDR_LABEL) + 28.0;
    let mut widths = (0..table.cols.len()).map(header).collect::<Vec<_>>();

    for row in rows {
        let DisplayRow::Process(row) = row else { continue };
        widths[0] = widths[0].max(
            tablekit::text_width(ui, &row.name, tablekit::FONT_ROW)
                + 66.0
                + row.depth as f32 * 22.0,
        );
        if row.suspended {
            widths[1] = widths[1].max(
                tablekit::text_width(ui, i18n::tr(K::StSuspended), tablekit::FONT_ROW) + 42.0,
            );
        }
        let values = [
            format::format_pct_cell(row.values[0].min(100.0) as f32),
            format::format_mb(row.values[1] as u64),
            format::format_rate_mb(row.values[2]),
            if row.net_available {
                format::format_mbit(row.values[3])
            } else {
                "—".to_string()
            },
        ];
        for (i, text) in values.iter().enumerate() {
            widths[i + 2] = widths[i + 2]
                .max(tablekit::text_width(ui, text, tablekit::FONT_ROW) + 22.0);
        }
    }
    for (i, agg) in aggs.iter().enumerate() {
        widths[i + 2] = widths[i + 2]
            .max(tablekit::text_width(ui, agg, tablekit::FONT_AGG) + 36.0);
    }
    for (i, width) in widths.into_iter().enumerate() {
        table.set_auto_fit_width(i, width.ceil());
    }
}

/// Collapsible group header ("Apps (5)") at standard row height so the list
/// stays virtualizable.
fn group_header(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &theme::Palette,
    gi: u8,
    total: usize,
    width: f32,
) {
    let label = match gi {
        0 => i18n::tr(K::GroupApps),
        1 => i18n::tr(K::GroupBackground),
        _ => i18n::tr(K::GroupWindows),
    };
    // Exactly the table's content width — matching the rows keeps the
    // horizontal scroll extents (and thus header/body alignment) identical.
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(width, tablekit::ROW_H), egui::Sense::click());
    // TM group headers sit directly on the window background — no fill band
    // (the old subtle tint read as a misplaced stripe).
    let cx = rect.left() + 14.0;
    let caret_rect =
        egui::Rect::from_center_size(egui::Pos2::new(cx, rect.center().y), egui::vec2(16.0, 16.0));
    crate::icons::draw_at(
        ui,
        caret_rect,
        if app.processes_state.group_collapsed[gi as usize] {
            Icon::ChevronRight
        } else {
            Icon::ChevronDown
        },
        pal.text_dim,
    );
    ui.painter().text(
        egui::Pos2::new(rect.left() + 28.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{label} ({total})"),
        egui::FontId::proportional(20.0),
        pal.text,
    );
    if resp.clicked() {
        app.processes_state.toggle_group(gi as usize);
    }
}

fn row_ui(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &theme::Palette,
    table: &tablekit::TmTable,
    row: &RowData,
) {
    let selected = app
        .selected_process
        .as_ref()
        .is_some_and(|sp| sp.pid == row.pid);
    let scroll_hint = app.processes_state.scroll_to_pid == Some(row.pid);
    let (rect, resp) = table.row(ui, pal, selected);
    if scroll_hint {
        resp.scroll_to_me(Some(egui::Align::Center));
        app.processes_state.scroll_to_pid = None;
    }

    // Chevron + icon + name.
    let expanded = app.processes_state.expanded.contains(&row.pid);
    let seed = egui::Id::new(("proc-chev", row.pid, row.start_epoch_s.unwrap_or(0)));
    let toggled = row.children && table.chevron(ui, rect, expanded, true, pal, seed);
    if toggled {
        app.processes_state.toggle_expanded(row.pid);
    }

    let tex = row
        .icon_path
        .as_ref()
        .and_then(|p| app.shared.icons.get(ui.ctx(), &app.actions, p, 6));
    table.icon_cell(
        ui,
        rect.translate(egui::vec2(row.depth as f32 * 22.0, 0.0)),
        tex.as_ref(),
        pal.accent,
    );
    let name_rect = table.col_rect(0, rect);
    ui.painter_at(name_rect).text(
        egui::Pos2::new(
            name_rect.left() + 56.0 + row.depth as f32 * 22.0,
            rect.center().y,
        ),
        egui::Align2::LEFT_CENTER,
        &row.name,
        egui::FontId::proportional(tablekit::FONT_ROW),
        pal.text,
    );

    // Status text + efficiency leaf sourced from the SNAPSHOT (audit §8).
    if row.suspended {
        table.text_cell(ui, rect, 1, i18n::tr(K::StSuspended), pal, false);
    }
    if row.power_throttled {
        let status_rect = table.col_rect(1, rect);
        crate::icons::draw_at(
            ui,
            egui::Rect::from_center_size(
                egui::Pos2::new(status_rect.right() - 16.0, rect.center().y),
                egui::vec2(16.0, 16.0),
            ),
            Icon::Leaf,
            pal.ok_green,
        );
    }

    // Heat cells: intensities were normalized per column across the whole
    // display model during cache build (audit P0.2); here we only paint.
    let texts = [
        format::format_pct_cell(row.values[0].min(100.0) as f32),
        format::format_mb(row.values[1] as u64),
        format::format_rate_mb(row.values[2]),
        if row.net_available {
            format::format_mbit(row.values[3])
        } else {
            "—".to_string()
        },
    ];
    let cells: Vec<HeatCell> = texts
        .iter()
        .zip(row.heat.iter())
        .map(|(s, t)| HeatCell::new(*t, s.clone()))
        .collect();
    let cells_active = row.values.iter().any(|&v| v > 0.0);
    table.heat_cells(ui, pal, rect, 2, &cells, cells_active);

    if resp.clicked() {
        app.selected_process = Some(crate::app::ProcessIdentity {
            pid: row.pid,
            start_epoch_s: row.start_epoch_s,
        });
    }
    resp.context_menu(|ui| context_menu(app, ui, row));
}

fn context_menu(app: &mut TaskManApp, ui: &mut egui::Ui, row: &RowData) {
    let ctx = ui.ctx().clone();
    ui.set_min_width(210.0);
    if ui.button(i18n::tr(K::EndTask)).clicked() {
        let identity = crate::app::ProcessIdentity {
            pid: row.pid,
            start_epoch_s: row.start_epoch_s,
        };
        end_process_checked(app, &ctx, &identity, false, &row.name);
        ui.close();
    }
    #[cfg(target_os = "windows")]
    if ui.button(i18n::tr(K::EndTree)).clicked() {
        let identity = crate::app::ProcessIdentity {
            pid: row.pid,
            start_epoch_s: row.start_epoch_s,
        };
        end_process_checked(app, &ctx, &identity, true, &row.name);
        ui.close();
    }
    ui.separator();
    if ui.button(i18n::tr(K::EfficiencyMode)).clicked() {
        let identity = crate::app::ProcessIdentity {
            pid: row.pid,
            start_epoch_s: row.start_epoch_s,
        };
        toggle_efficiency_mode(app, &ctx, &identity);
        ui.close();
    }
    if ui.button(i18n::tr(K::GoToDetails)).clicked() {
        app.pending_details_focus = Some(crate::app::PendingDetailsFocus(
            crate::app::ProcessIdentity {
                pid: row.pid,
                start_epoch_s: row.start_epoch_s,
            },
        ));
        app.tab = crate::app::Tab::Details;
        ui.close();
    }
    #[cfg(target_os = "windows")]
    if ui.button(i18n::tr(K::GoToServices)).clicked() {
        app.goto_services_for_pid(row.pid, &ctx);
        ui.close();
    }
    ui.separator();
    if ui.button(i18n::tr(K::OpenFileLocation)).clicked() {
        match row.icon_path.as_deref() {
            Some(path) => {
                let actions = app.actions.clone();
                let path = path.to_string();
                app.run_action(&ctx, String::new, move || actions.open_file_location(&path));
            }
            None => app.shared.toast(i18n::tr(K::NoFileForProcess)),
        }
        ui.close();
    }
    if ui.button(i18n::tr(K::Properties)).clicked() {
        app.proc_props = Some(row.pid);
        ui.close();
    }
}

fn end_process_checked(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    identity: &crate::app::ProcessIdentity,
    tree: bool,
    name: &str,
) {
    let live_ok = app.identity_is_live(identity);
    if !live_ok {
        app.shared.toast(i18n::tr(K::ProcessExited));
        return;
    }
    let pid = identity.pid;
    let actions = app.actions.clone();
    let msg_name = name.to_string();
    app.run_action(
        ctx,
        move || {
            if tree {
                i18n::trf(K::TreeOfEndedToast, &[&msg_name])
            } else {
                i18n::trf(K::NameEndedToast, &[&msg_name])
            }
        },
        move || actions.kill_process(pid, tree),
    );
}

/// Task Manager's Processes page is not a literal PPID tree. In particular,
/// Explorer is a launcher boundary: programs started from the shell become
/// independent app groups even though their real parent PID is explorer.exe.
/// We derive a presentation-only category/root model and leave Snapshot/PPID
/// data untouched so Details and process actions continue to see OS truth.
struct DisplayGroups {
    category: HashMap<u32, ProcCategory>,
    app_roots: HashSet<u32>,
}

fn build_display_rows(
    snap: &Snapshot,
    raw_search: &str,
    sort_col: usize,
    ascending: bool,
    expanded: &HashSet<u32>,
    group_collapsed: &[bool; 3],
) -> Vec<DisplayRow> {
    let q = search::Query::new(raw_search);
    let all: Vec<&ProcessEntry> = snap.processes.iter().collect();
    let grouping = derive_display_groups(&all);
    let children_all = display_children_map(&all, &grouping.category, &grouping.app_roots);
    let (subtree, subtree_count) = subtree_values_and_counts(&all, &children_all);
    let mut out = Vec::new();

    if !q.is_empty() {
        let mut matched: Vec<&ProcessEntry> = all
            .iter()
            .copied()
            .filter(|p| q.matches_process(p))
            .collect();
        sort_entries(&mut matched, sort_col, ascending, &subtree);
        for p in matched {
            out.push(make_flat_row(p, &subtree));
        }
        normalize_heat(&mut out);
        return out;
    }

    for gi in 0..3u8 {
        let cat = match gi {
            0 => ProcCategory::App,
            1 => ProcCategory::Background,
            _ => ProcCategory::System,
        };
        let members: Vec<&ProcessEntry> = all
            .iter()
            .copied()
            .filter(|p| grouping.category.get(&p.pid).copied().unwrap_or(p.category) == cat)
            .collect();
        let children = display_children_map(&members, &grouping.category, &grouping.app_roots);
        let roots = tree_roots(&members, &children);
        // Native Task Manager's "Apps (N)" counts top-level app groups, while
        // Background/Windows counters count processes. This is why Brave (28)
        // contributes one to Apps, not twenty-eight.
        let total = if cat == ProcCategory::App {
            roots.len()
        } else {
            members.len()
        };
        out.push(DisplayRow::GroupHeader(gi, total));
        if group_collapsed[gi as usize] {
            continue;
        }
        emit_tree(
            &mut out,
            &roots,
            &children,
            &subtree,
            &subtree_count,
            sort_col,
            ascending,
            expanded,
        );
    }
    normalize_heat(&mut out);
    out
}

fn derive_display_groups(all: &[&ProcessEntry]) -> DisplayGroups {
    let by_pid: HashMap<u32, &ProcessEntry> = all.iter().map(|p| (p.pid, *p)).collect();

    // Non-Windows/mock backends may not provide window ownership. Preserve
    // their collector classification rather than demoting every App simply
    // because the Windows-specific signal is absent.
    if !all.iter().any(|p| p.has_window) {
        let category: HashMap<u32, ProcCategory> = all.iter().map(|p| (p.pid, p.category)).collect();
        let mut app_roots: HashSet<u32> = all
            .iter()
            .copied()
            .filter(|p| p.category == ProcCategory::App && p.app_root)
            .map(|p| p.pid)
            .collect();
        if app_roots.is_empty() {
            for p in all.iter().copied().filter(|p| p.category == ProcCategory::App) {
                let parent_is_app = p
                    .ppid
                    .filter(|ppid| *ppid != p.pid)
                    .and_then(|ppid| by_pid.get(&ppid).copied())
                    .is_some_and(|parent| parent.category == ProcCategory::App);
                if !parent_is_app {
                    app_roots.insert(p.pid);
                }
            }
        }
        return DisplayGroups {
            category,
            app_roots,
        };
    }

    // The platform category is authoritative for Windows/system processes.
    // For user/session processes, rebuild App membership from visible-window
    // ownership so a shell parent cannot turn all of its descendants into
    // foreground Apps.
    let mut category: HashMap<u32, ProcCategory> = all
        .iter()
        .map(|p| {
            let system = p.category == ProcCategory::System || is_system_boundary(&p.name);
            (
                p.pid,
                if system {
                    ProcCategory::System
                } else {
                    ProcCategory::Background
                },
            )
        })
        .collect();

    let mut raw_children: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
    for p in all {
        if let Some(ppid) = p.ppid
            && ppid != p.pid
            && by_pid.contains_key(&ppid)
        {
            raw_children.entry(ppid).or_default().push(*p);
        }
    }

    let mut app_roots = HashSet::new();
    for p in all.iter().copied().filter(|p| p.has_window) {
        if category.get(&p.pid) == Some(&ProcCategory::System) {
            continue;
        }
        let mut cur = p;
        let mut seen = HashSet::new();
        seen.insert(cur.pid);
        while let Some(ppid) = cur.ppid {
            if ppid == cur.pid || !seen.insert(ppid) {
                break;
            }
            let Some(parent) = by_pid.get(&ppid).copied() else {
                break;
            };
            if category.get(&parent.pid) == Some(&ProcCategory::System)
                || is_launch_boundary(&parent.name)
            {
                break;
            }
            // Multiple visible windows owned by the same executable still
            // belong to one app family. A different visible parent is an
            // independent foreground app and therefore a hard boundary.
            if parent.has_window && !same_process_family(parent, cur) {
                break;
            }
            cur = parent;
        }
        app_roots.insert(cur.pid);
    }

    // App roots own their helper descendants, except for shell roots that are
    // launch surfaces rather than application families. Never cross another
    // independently discovered app root or a system-process boundary.
    let mut stack: Vec<u32> = app_roots.iter().copied().collect();
    let mut seen_app = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen_app.insert(pid) || category.get(&pid) == Some(&ProcCategory::System) {
            continue;
        }
        category.insert(pid, ProcCategory::App);
        let Some(proc) = by_pid.get(&pid).copied() else {
            continue;
        };
        if is_non_owning_shell(&proc.name) {
            continue;
        }
        if let Some(kids) = raw_children.get(&pid) {
            for child in kids {
                if app_roots.contains(&child.pid)
                    || category.get(&child.pid) == Some(&ProcCategory::System)
                {
                    continue;
                }
                stack.push(child.pid);
            }
        }
    }

    DisplayGroups {
        category,
        app_roots,
    }
}

fn same_process_family(a: &ProcessEntry, b: &ProcessEntry) -> bool {
    a.name.eq_ignore_ascii_case(&b.name)
}

/// Parent processes that launch independent foreground applications rather
/// than semantically owning them. Explorer is the important case; console
/// shells cover GUI programs launched from Terminal/cmd/PowerShell.
fn is_launch_boundary(name: &str) -> bool {
    const LAUNCHERS: &[&str] = &[
        "explorer.exe",
        "cmd.exe",
        "powershell.exe",
        "pwsh.exe",
        "wsl.exe",
        "wt.exe",
        "SearchHost.exe",
        "SearchApp.exe",
        "StartMenuExperienceHost.exe",
        "ShellExperienceHost.exe",
    ];
    LAUNCHERS.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// Explorer's desktop/taskbar process can own a visible window itself, but
/// its arbitrary launched children must never be folded into that row.
fn is_non_owning_shell(name: &str) -> bool {
    name.eq_ignore_ascii_case("explorer.exe") || name.eq_ignore_ascii_case("explorer")
}

fn is_system_boundary(name: &str) -> bool {
    const SYSTEM: &[&str] = &[
        "system",
        "registry",
        "memory compression",
        "secure system",
        "smss.exe",
        "csrss.exe",
        "wininit.exe",
        "services.exe",
        "lsass.exe",
        "lsaiso.exe",
        "svchost.exe",
        "winlogon.exe",
        "dwm.exe",
        "fontdrvhost.exe",
        "system idle process",
    ];
    SYSTEM.iter().any(|n| name.eq_ignore_ascii_case(n))
}

fn normalize_heat(rows: &mut [DisplayRow]) {
    let mut max = [0.0f64; 4];
    for r in rows.iter() {
        let DisplayRow::Process(d) = r else { continue };
        for (i, v) in d.values.iter().enumerate() {
            if !(i == 3 && !d.net_available) {
                max[i] = max[i].max(*v);
            }
        }
    }
    for r in rows.iter_mut() {
        let DisplayRow::Process(d) = r else { continue };
        for (i, v) in d.values.iter().enumerate() {
            d.heat[i] = if i == 3 && !d.net_available {
                0.0
            } else {
                tablekit::norm(*v, max[i])
            };
        }
    }
}

fn make_flat_row(p: &ProcessEntry, subtree: &HashMap<u32, [f64; 4]>) -> DisplayRow {
    DisplayRow::Process(RowData {
        pid: p.pid,
        start_epoch_s: p.start_epoch_s,
        depth: 0,
        name: p.shown_name().to_string(),
        icon_path: p
            .exe_path
            .as_ref()
            .map(|x| x.to_string_lossy().into_owned()),
        children: false,
        values: subtree.get(&p.pid).copied().unwrap_or([0.0; 4]),
        heat: [0.0; 4],
        net_available: p.net_recv_bps.is_some() || p.net_sent_bps.is_some(),
        suspended: p.status == ProcStatus::Suspended,
        power_throttled: p.power_throttled == Some(true),
    })
}

/// Build the parent→child topology used only by the Processes presentation.
/// Cross-category edges are cut, and every independently discovered App root
/// is detached from its real PPID so Explorer-launched programs stay peers.
fn display_children_map<'a>(
    list: &[&'a ProcessEntry],
    category: &HashMap<u32, ProcCategory>,
    app_roots: &HashSet<u32>,
) -> HashMap<u32, Vec<&'a ProcessEntry>> {
    let pids: HashSet<u32> = list.iter().map(|p| p.pid).collect();
    let mut m: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
    for p in list {
        let child_cat = category.get(&p.pid).copied().unwrap_or(p.category);
        if child_cat == ProcCategory::App && app_roots.contains(&p.pid) {
            continue;
        }
        if let Some(ppid) = p.ppid
            && ppid != p.pid
            && pids.contains(&ppid)
            && category.get(&ppid).copied() == Some(child_cat)
        {
            m.entry(ppid).or_default().push(*p);
        }
    }
    m
}

/// Roots for every connected display component. The second pass also keeps
/// malformed/cyclic PPID graphs visible instead of silently dropping them.
fn tree_roots<'a>(
    members: &[&'a ProcessEntry],
    children: &HashMap<u32, Vec<&'a ProcessEntry>>,
) -> Vec<&'a ProcessEntry> {
    let linked: HashSet<u32> = children
        .values()
        .flat_map(|kids| kids.iter().map(|p| p.pid))
        .collect();
    let mut roots: Vec<&ProcessEntry> = members
        .iter()
        .copied()
        .filter(|p| !linked.contains(&p.pid))
        .collect();

    let mut covered = HashSet::new();
    let mut stack: Vec<u32> = roots.iter().map(|p| p.pid).collect();
    while let Some(pid) = stack.pop() {
        if !covered.insert(pid) {
            continue;
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().map(|p| p.pid));
        }
    }
    for p in members {
        if covered.contains(&p.pid) {
            continue;
        }
        roots.push(*p);
        stack.push(p.pid);
        while let Some(pid) = stack.pop() {
            if !covered.insert(pid) {
                continue;
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().map(|kid| kid.pid));
            }
        }
    }
    roots
}

#[allow(clippy::too_many_arguments)]
fn emit_tree<'a>(
    out: &mut Vec<DisplayRow>,
    roots: &[&'a ProcessEntry],
    children: &HashMap<u32, Vec<&'a ProcessEntry>>,
    subtree: &HashMap<u32, [f64; 4]>,
    subtree_count: &HashMap<u32, u32>,
    sort_col: usize,
    ascending: bool,
    expanded: &HashSet<u32>,
) {
    let mut sorted_roots: Vec<&ProcessEntry> = roots.to_vec();
    sort_entries(&mut sorted_roots, sort_col, ascending, subtree);
    let mut stack: Vec<(&ProcessEntry, usize)> =
        sorted_roots.iter().rev().map(|&r| (r, 0usize)).collect();
    let mut visited: HashSet<u32> = HashSet::new();
    let mut sorted_children: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();

    while let Some((proc, depth)) = stack.pop() {
        if !visited.insert(proc.pid) {
            continue;
        }
        let kids = sorted_children
            .entry(proc.pid)
            .or_insert_with(|| {
                let mut v = children.get(&proc.pid).cloned().unwrap_or_default();
                sort_entries(&mut v, sort_col, ascending, subtree);
                v
            })
            .clone();
        let has_children = !kids.is_empty();
        let count = subtree_count.get(&proc.pid).copied().unwrap_or(1);
        let name = if count > 1 {
            format!("{} ({})", proc.shown_name(), count)
        } else {
            proc.shown_name().to_string()
        };
        out.push(DisplayRow::Process(RowData {
            pid: proc.pid,
            start_epoch_s: proc.start_epoch_s,
            depth,
            name,
            icon_path: proc
                .exe_path
                .as_ref()
                .map(|x| x.to_string_lossy().into_owned()),
            children: has_children,
            values: subtree.get(&proc.pid).copied().unwrap_or([0.0; 4]),
            heat: [0.0; 4],
            net_available: proc.net_recv_bps.is_some() || proc.net_sent_bps.is_some(),
            suspended: proc.status == ProcStatus::Suspended,
            power_throttled: proc.power_throttled == Some(true),
        }));
        if has_children && expanded.contains(&proc.pid) {
            for k in kids.into_iter().rev() {
                stack.push((k, depth + 1));
            }
        }
    }
}

fn subtree_values_and_counts<'a>(
    all: &[&'a ProcessEntry],
    children: &HashMap<u32, Vec<&'a ProcessEntry>>,
) -> (HashMap<u32, [f64; 4]>, HashMap<u32, u32>) {
    let own = |p: &ProcessEntry| {
        [
            p.cpu_pct as f64,
            p.mem_bytes as f64,
            p.disk_read_bps + p.disk_write_bps,
            p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0),
        ]
    };
    let mut out: HashMap<u32, [f64; 4]> = HashMap::with_capacity(all.len());
    let mut counts: HashMap<u32, u32> = HashMap::with_capacity(all.len());
    let by_pid: HashMap<u32, &'a ProcessEntry> = all.iter().map(|p| (p.pid, *p)).collect();

    enum Frame<'b> {
        Enter(u32),
        Combine(u32, Vec<&'b ProcessEntry>, [f64; 4]),
    }
    let mut done: HashSet<u32> = HashSet::with_capacity(all.len());
    let mut in_progress: HashSet<u32> = HashSet::with_capacity(all.len());

    for root in all {
        if done.contains(&root.pid) {
            continue;
        }
        let mut stack: Vec<Frame> = vec![Frame::Enter(root.pid)];
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Combine(pid, kids, mut acc) => {
                    let mut cnt: u32 = 1;
                    for k in &kids {
                        if let Some(v) = out.get(&k.pid) {
                            for i in 0..4 {
                                acc[i] += v[i];
                            }
                            cnt += counts.get(&k.pid).copied().unwrap_or(1);
                        }
                    }
                    out.insert(pid, acc);
                    counts.insert(pid, cnt);
                    done.insert(pid);
                    in_progress.remove(&pid);
                }
                Frame::Enter(pid) => {
                    if done.contains(&pid) || in_progress.contains(&pid) {
                        continue;
                    }
                    let Some(p) = by_pid.get(&pid) else { continue };
                    in_progress.insert(pid);
                    let kids: Vec<&'a ProcessEntry> = children
                        .get(&pid)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[])
                        .to_vec();
                    let pending: Vec<&ProcessEntry> = kids
                        .iter()
                        .copied()
                        .filter(|k| {
                            !done.contains(&k.pid)
                                && !in_progress.contains(&k.pid)
                                && by_pid.contains_key(&k.pid)
                        })
                        .collect();
                    if pending.is_empty() {
                        out.insert(pid, own(p));
                        counts.insert(pid, 1);
                        done.insert(pid);
                        in_progress.remove(&pid);
                    } else {
                        stack.push(Frame::Combine(pid, kids, own(p)));
                        for k in pending {
                            stack.push(Frame::Enter(k.pid));
                        }
                    }
                }
            }
        }
    }
    for p in all {
        out.entry(p.pid).or_insert_with(|| own(p));
        counts.entry(p.pid).or_insert(1);
    }
    (out, counts)
}

fn sort_entries(v: &mut [&ProcessEntry], col: usize, asc: bool, subtree: &HashMap<u32, [f64; 4]>) {
    let sv = |p: &ProcessEntry, i: usize| subtree.get(&p.pid).map_or(0.0, |s| s[i]);
    v.sort_by(|a, b| {
        let o = match col {
            2 => sv(a, 0)
                .partial_cmp(&sv(b, 0))
                .unwrap_or(std::cmp::Ordering::Equal),
            3 => sv(a, 1)
                .partial_cmp(&sv(b, 1))
                .unwrap_or(std::cmp::Ordering::Equal),
            4 => sv(a, 2)
                .partial_cmp(&sv(b, 2))
                .unwrap_or(std::cmp::Ordering::Equal),
            5 => sv(a, 3)
                .partial_cmp(&sv(b, 3))
                .unwrap_or(std::cmp::Ordering::Equal),
            _ => cmp_ignore_case(a.shown_name(), b.shown_name()),
        };
        if asc { o } else { o.reverse() }
    });
}

fn cmp_ignore_case(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().flat_map(char::to_lowercase);
    let mut bi = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => match x.cmp(&y) {
                std::cmp::Ordering::Equal => continue,
                other => return other,
            },
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: Option<u32>, name: &str, cat: ProcCategory) -> ProcessEntry {
        let mut p = ProcessEntry::new(pid, name);
        p.ppid = ppid;
        p.category = cat;
        p.has_window = cat == ProcCategory::App && ppid.is_none();
        p.cpu_pct = 1.0 * pid as f32;
        p.mem_bytes = 1000 * pid as u64;
        p
    }

    fn members_under(rows: &[DisplayRow], nth_header: usize) -> Option<usize> {
        let start = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| matches!(r, DisplayRow::GroupHeader(..)).then_some(i))
            .nth(nth_header)?;
        Some(
            rows[start + 1..]
                .iter()
                .take_while(|r| !matches!(r, DisplayRow::GroupHeader(..)))
                .filter(|r| matches!(r, DisplayRow::Process(_)))
                .count(),
        )
    }

    fn snap_of(ps: Vec<ProcessEntry>) -> Snapshot {
        Snapshot {
            timestamp_ms: 1,
            processes: ps,
            ..Default::default()
        }
    }

    #[test]
    fn process_groups_system_separate_from_background() {
        let snap = snap_of(vec![
            proc(1, None, "explorer.exe", ProcCategory::App),
            proc(2, None, "updatehelper.exe", ProcCategory::Background),
            proc(3, None, "svchost.exe", ProcCategory::System),
        ]);
        let empty_expanded = HashSet::new();
        let groups = [false; 3];
        let rows = build_display_rows(&snap, "", 0, true, &empty_expanded, &groups);
        let headers: Vec<(u8, usize)> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::GroupHeader(g, n) => Some((*g, *n)),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec![(0, 1), (1, 1), (2, 1)]);
        assert_eq!(
            (
                members_under(&rows, 0),
                members_under(&rows, 1),
                members_under(&rows, 2)
            ),
            (Some(1), Some(1), Some(1))
        );
    }

    #[test]
    fn group_totals_are_independent_of_expansion_state() {
        let snap = snap_of(vec![
            proc(1, None, "app", ProcCategory::App),
            proc(2, Some(1), "child", ProcCategory::App),
            proc(3, Some(2), "grandchild", ProcCategory::App),
            proc(4, None, "bg", ProcCategory::Background),
        ]);
        let groups = [false; 3];
        let mut expanded = HashSet::new();
        expanded.insert(1u32);
        expanded.insert(2u32);
        let collapsed_rows = build_display_rows(&snap, "", 0, true, &HashSet::new(), &groups);
        let expanded_rows = build_display_rows(&snap, "", 0, true, &expanded, &groups);
        let header_total = |rows: &[DisplayRow]| -> usize {
            rows.iter()
                .find_map(|r| match r {
                    DisplayRow::GroupHeader(g, n) if *g == 0 => Some(*n),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(header_total(&collapsed_rows), 1);
        assert_eq!(header_total(&expanded_rows), header_total(&collapsed_rows));
        let groups_closed = [true, false, false];
        let closed = build_display_rows(&snap, "", 0, true, &HashSet::new(), &groups_closed);
        assert_eq!(header_total(&closed), 1);
    }

    #[test]
    fn explorer_launched_programs_are_independent_app_groups() {
        let explorer = proc(1, None, "explorer.exe", ProcCategory::App);
        let mut brave = proc(2, Some(1), "brave.exe", ProcCategory::App);
        brave.has_window = false;
        let mut brave_window = proc(3, Some(2), "brave.exe", ProcCategory::App);
        brave_window.has_window = true;
        let brave_helper = proc(4, Some(2), "brave.exe", ProcCategory::App);
        let mut code = proc(5, Some(1), "Code.exe", ProcCategory::App);
        code.has_window = true;
        // Simulate the old collector result: every Explorer descendant was
        // labeled App even though this tray helper has no foreground window.
        let tray = proc(6, Some(1), "trayhelper.exe", ProcCategory::App);

        let rows = build_display_rows(
            &snap_of(vec![
                explorer,
                brave,
                brave_window,
                brave_helper,
                code,
                tray,
            ]),
            "",
            0,
            true,
            &HashSet::new(),
            &[false; 3],
        );

        let app_total = rows
            .iter()
            .find_map(|r| match r {
                DisplayRow::GroupHeader(0, n) => Some(*n),
                _ => None,
            })
            .unwrap();
        assert_eq!(app_total, 3);

        let app_rows: Vec<&RowData> = rows
            .iter()
            .skip_while(|r| !matches!(r, DisplayRow::GroupHeader(0, _)))
            .skip(1)
            .take_while(|r| !matches!(r, DisplayRow::GroupHeader(..)))
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(app_rows.len(), 3);
        assert!(app_rows.iter().all(|r| r.depth == 0));

        let explorer_row = app_rows.iter().find(|r| r.pid == 1).unwrap();
        assert_eq!(explorer_row.name, "explorer.exe");
        assert_eq!(explorer_row.values[0], 1.0);

        let brave_row = app_rows.iter().find(|r| r.pid == 2).unwrap();
        assert_eq!(brave_row.name, "brave.exe (3)");
        assert_eq!(brave_row.values[0], 9.0);

        let bg_pids: Vec<u32> = rows
            .iter()
            .skip_while(|r| !matches!(r, DisplayRow::GroupHeader(1, _)))
            .skip(1)
            .take_while(|r| !matches!(r, DisplayRow::GroupHeader(..)))
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d.pid),
                _ => None,
            })
            .collect();
        assert_eq!(bg_pids, vec![6]);
    }

    #[test]
    fn shell_launched_gui_is_not_folded_into_terminal_group() {
        let terminal = proc(10, None, "WindowsTerminal.exe", ProcCategory::App);
        let shell = proc(11, Some(10), "powershell.exe", ProcCategory::App);
        let mut notepad = proc(12, Some(11), "notepad.exe", ProcCategory::App);
        notepad.has_window = true;
        let rows = build_display_rows(
            &snap_of(vec![terminal, shell, notepad]),
            "",
            0,
            true,
            &HashSet::new(),
            &[false; 3],
        );
        let app_total = rows
            .iter()
            .find_map(|r| match r {
                DisplayRow::GroupHeader(0, n) => Some(*n),
                _ => None,
            })
            .unwrap();
        assert_eq!(app_total, 2);
        let app_rows: Vec<&RowData> = rows
            .iter()
            .skip_while(|r| !matches!(r, DisplayRow::GroupHeader(0, _)))
            .skip(1)
            .take_while(|r| !matches!(r, DisplayRow::GroupHeader(..)))
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(app_rows.len(), 2);
        assert!(app_rows.iter().all(|r| r.depth == 0));
        assert_eq!(app_rows.iter().find(|r| r.pid == 10).unwrap().name, "WindowsTerminal.exe (2)");
        assert_eq!(app_rows.iter().find(|r| r.pid == 12).unwrap().name, "notepad.exe");
    }

    #[test]
    fn process_tree_supports_three_plus_levels() {
        let snap = snap_of(vec![
            proc(1, None, "root", ProcCategory::App),
            proc(2, Some(1), "child", ProcCategory::App),
            proc(3, Some(2), "grandchild", ProcCategory::App),
            proc(4, Some(3), "great", ProcCategory::App),
        ]);
        let mut expanded = HashSet::new();
        expanded.extend([1u32, 2u32, 3u32]);
        let groups = [false; 3];
        let rows = build_display_rows(&snap, "", 0, true, &expanded, &groups);
        let depths: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d.depth),
                _ => None,
            })
            .collect();
        assert_eq!(depths, vec![0, 1, 2, 3]);
    }

    #[test]
    fn process_tree_cycle_terminates_and_remains_visible() {
        let snap = snap_of(vec![
            proc(1, Some(2), "a", ProcCategory::Background),
            proc(2, Some(1), "b", ProcCategory::Background),
        ]);
        let mut expanded = HashSet::new();
        expanded.insert(1u32);
        expanded.insert(2u32);
        let groups = [false; 3];
        let rows = build_display_rows(&snap, "", 0, true, &expanded, &groups);
        let procs: Vec<u32> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d.pid),
                _ => None,
            })
            .collect();
        assert_eq!(procs.len(), 2);
        assert_eq!(
            procs.len(),
            procs.iter().collect::<std::collections::HashSet<_>>().len()
        );
    }

    #[test]
    fn grouped_label_counts_entire_subtree() {
        let snap = snap_of(vec![
            proc(1, None, "Brave", ProcCategory::App),
            proc(2, Some(1), "Child", ProcCategory::App),
            proc(3, Some(2), "GC", ProcCategory::App),
            proc(4, Some(3), "GGC", ProcCategory::App),
        ]);
        let groups = [false; 3];
        let mut expanded = HashSet::new();
        expanded.insert(1u32);
        let rows = build_display_rows(&snap, "", 0, true, &expanded, &groups);
        let labels: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels[0], "Brave (4)");
        let collapsed = build_display_rows(&snap, "", 0, true, &HashSet::new(), &groups);
        let collapsed_labels: Vec<&str> = collapsed
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(collapsed_labels[0], "Brave (4)");
        let mut e2 = HashSet::new();
        e2.insert(1u32);
        e2.insert(2u32);
        let more = build_display_rows(&snap, "", 0, true, &e2, &groups);
        let labels2: Vec<&str> = more
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels2[0], "Brave (4)");
        assert_eq!(labels2[1], "Child (3)");
    }

    #[test]
    fn subtree_aggregation_counts_all_descendants() {
        let snap = snap_of(vec![
            proc(1, None, "root", ProcCategory::Background),
            proc(2, Some(1), "mid", ProcCategory::Background),
            proc(3, Some(2), "leaf", ProcCategory::Background),
        ]);
        let all: Vec<&ProcessEntry> = snap.processes.iter().collect();
        let grouping = derive_display_groups(&all);
        let children = display_children_map(&all, &grouping.category, &grouping.app_roots);
        let (st, cnt) = subtree_values_and_counts(&all, &children);
        assert_eq!(st[&1][0], 6.0);
        assert_eq!(st[&1][1], 6000.0);
        assert_eq!(st[&2][0], 5.0);
        assert_eq!(st[&3][0], 3.0);
        assert_eq!((cnt[&3], cnt[&2], cnt[&1]), (1, 2, 3));
    }

    #[test]
    fn heat_normalizes_per_column_across_the_whole_model() {
        let mut heavy = proc(10, None, "heavy.exe", ProcCategory::Background);
        heavy.cpu_pct = 90.0;
        heavy.mem_bytes = 800_000_000;
        let mut light = proc(11, None, "light.exe", ProcCategory::Background);
        light.cpu_pct = 9.0;
        light.mem_bytes = 400_000_000;
        let mut zero = proc(12, None, "zero.exe", ProcCategory::Background);
        zero.cpu_pct = 0.0;
        zero.mem_bytes = 100;

        let groups = [false; 3];
        let empty = HashSet::new();
        let rows = build_display_rows(
            &snap_of(vec![heavy, light, zero]),
            "",
            0,
            true,
            &empty,
            &groups,
        );
        let heat: Vec<[f32; 4]> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d.heat),
                _ => None,
            })
            .collect();
        assert_eq!(
            heat.iter().filter(|h| h[0] >= 1.0 - f32::EPSILON).count(),
            1
        );
        assert_eq!(
            heat.iter().filter(|h| h[1] >= 1.0 - f32::EPSILON).count(),
            1
        );
        assert!(heat[0][0] > heat[1][0]);
        assert!(heat[1][0] > heat[2][0] || heat[2][0] == 0.0);
    }

    #[test]
    fn missing_network_renders_unavailable_not_zero() {
        let mut p = proc(9, None, "no-net", ProcCategory::Background);
        p.net_recv_bps = None;
        p.net_sent_bps = None;
        let snap = snap_of(vec![p]);
        let empty = HashSet::new();
        let groups = [false; 3];
        let rows = build_display_rows(&snap, "", 0, true, &empty, &groups);
        let d = rows
            .iter()
            .find_map(|r| match r {
                DisplayRow::Process(d) => Some(d.clone()),
                _ => None,
            })
            .unwrap();
        assert!(!d.net_available);
        assert_eq!(d.heat[3], 0.0);
        assert_eq!(
            if d.net_available {
                format::format_mbit(d.values[3])
            } else {
                "—".to_string()
            },
            "—"
        );
    }

    #[test]
    fn search_matches_pid_and_publisher() {
        let mut p = ProcessEntry::new(4242, "codestrings.exe");
        p.company = Some("ExampleCorp GmbH".into());
        let snap = snap_of(vec![p]);

        for q in ["4242", "examplecorp", "codestrings"] {
            let rows = build_display_rows(&snap, q, 0, true, &HashSet::new(), &[false; 3]);
            let n = rows
                .iter()
                .filter(|r| matches!(r, DisplayRow::Process(_)))
                .count();
            assert_eq!(n, 1, "query '{q}' must find the process");
        }
        let rows = build_display_rows(
            &snap_of(snap.processes.clone()),
            "zzz-not-there",
            0,
            true,
            &HashSet::new(),
            &[false; 3],
        );
        assert!(rows.is_empty());
    }
}

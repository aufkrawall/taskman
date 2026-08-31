//! Processes tab: Apps / Background processes / Windows process groups,
//! expandable application groups with O(n) subtree
//! aggregates, blue heat-mapped resource columns and the aggregate header.
//! Rows are flattened into a display model and rendered through the fixed-
//! height virtualizer, so row count does not affect frame cost.
//!
//! Correctness notes from the 2026 audit:
//! * App rows are presentation groups, not raw parent-process trees. Shell
//!   launchers such as Explorer do not absorb programs the user launched;
//!   each visible app family is promoted to its own top-level row. Shell-
//!   session brokers (sihost, RuntimeBroker, dllhost, ...) broker Start-menu
//!   /COM launches and are launch boundaries too, as are browsers.
//! * A visible window folds into a windowless ancestor's family only when
//!   they are plausibly the same application (same image or same publisher);
//!   otherwise the windowed process is its own app row. Busy absorbed
//!   external helpers (no window, >= 1% CPU, different image) are promoted
//!   to individually visible Background rows; windowed processes are never
//!   demoted to Background.
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
    pub status: ProcStatus,
    /// True when the OS reports this process as power throttled
    /// (Efficiency mode) — rendered straight from the snapshot, never from
    /// client-side bookkeeping (audit §8).
    pub power_throttled: bool,
    /// Pseudo-row ("System interrupts", "Terminated processes"): no OS
    /// process behind it — destructive actions are withheld.
    pub synthetic: bool,
    /// Localized hover explanation (why CPU is unattributable), if any.
    pub tooltip: Option<String>,
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
    let start = target.start_epoch_s;
    app.run_action(
        ctx,
        || i18n::tr(K::EfficiencyChanged).to_string(),
        move || actions.set_efficiency_mode_checked(pid, start, on),
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

    // Task-Manager-style type navigation: typed letters accumulate into a
    // word, so "svc" lands on svchost.exe instead of jumping to whatever
    // starts with "c". One letter (or the same letter repeated) still cycles
    // and wraps in the exact flattened/sorted order shown on screen.
    if let Some(typed) = search::list_type_ahead(ui.ctx(), "processes") {
        let selected = app.selected_process.as_ref().map(|p| p.pid);
        let candidates = rows
            .iter()
            .filter_map(|row| match row {
                DisplayRow::Process(row) if !row.synthetic => Some((row.pid, row.name.as_str())),
                DisplayRow::GroupHeader(..) => None,
                DisplayRow::Process(_) => None,
            })
            .collect::<Vec<_>>();
        if let Some(pid) = search::type_ahead_match(candidates, selected, &typed)
            && let Some(row) = rows.iter().find_map(|row| match row {
                DisplayRow::Process(row) if row.pid == pid => Some(row),
                _ => None,
            })
        {
            app.selected_process = Some(crate::app::ProcessIdentity {
                pid: row.pid,
                start_epoch_s: row.start_epoch_s,
            });
            app.processes_state.scroll_to_pid = Some(row.pid);
        }
    }

    handle_keyboard_navigation(app, ui.ctx(), &rows);

    let agg = Aggregates::from_snapshot(&snap);
    let aggs = agg.strings();
    prepare_auto_fit_widths(ui, &mut table, &rows, &aggs);

    let avail = tablekit::table_avail(ui);
    // Consume any pending scroll request as a flat display-row index so the
    // table can bring it into view vertically even when it lies outside the
    // currently rendered virtualization window.
    let focus_row = app.processes_state.scroll_to_pid.take().and_then(|pid| {
        rows.iter()
            .position(|r| matches!(r, DisplayRow::Process(p) if p.pid == pid))
    });

    let clicked = tablekit::scrolled_rows(
        "processes",
        ui,
        &pal,
        &mut table,
        avail,
        Some((app.processes_state.sort_col, app.processes_state.ascending)),
        Some(&aggs),
        rows.len(),
        focus_row,
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
        app.persist_sort(
            "processes",
            table.cols[col].id,
            app.processes_state.ascending,
        );
    }
    app.persist_table(&table);
    app.processes_state.cache = cache;
}

/// Arrow/Home/End/Page navigation for the virtualized process model, plus
/// tree-aware Left/Right behavior. Selection is always an exact identity;
/// synthetic accounting rows are deliberately skipped.
fn handle_keyboard_navigation(app: &mut TaskManApp, ctx: &egui::Context, rows: &[DisplayRow]) {
    let process_rows: Vec<(usize, &RowData)> = rows
        .iter()
        .enumerate()
        .filter_map(|(display_idx, row)| match row {
            DisplayRow::Process(row) if !row.synthetic => Some((display_idx, row)),
            _ => None,
        })
        .collect();
    let selected_pid = app.selected_process.as_ref().map(|p| p.pid);
    let selected_pos =
        selected_pid.and_then(|pid| process_rows.iter().position(|(_, row)| row.pid == pid));
    let page_rows = (ctx.content_rect().height() / tablekit::ROW_H)
        .floor()
        .max(1.0) as usize;
    if let Some(nav) = search::list_nav(ctx)
        && let Some(next) = search::moved_index(process_rows.len(), selected_pos, nav, page_rows)
        && let Some((_, row)) = process_rows.get(next)
    {
        select_row(app, row);
    }

    if ctx.egui_wants_keyboard_input() {
        return;
    }
    let (left, right) = ctx.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowLeft),
            i.key_pressed(egui::Key::ArrowRight),
        )
    });
    let Some(pid) = app.selected_process.as_ref().map(|p| p.pid) else {
        return;
    };
    let Some(display_idx) = rows
        .iter()
        .position(|row| matches!(row, DisplayRow::Process(p) if p.pid == pid))
    else {
        return;
    };
    let DisplayRow::Process(current) = &rows[display_idx] else {
        return;
    };

    if right && current.children {
        if !app.processes_state.expanded.contains(&pid) {
            app.processes_state.toggle_expanded(pid);
        } else if let Some(DisplayRow::Process(child)) = rows.get(display_idx + 1)
            && child.depth > current.depth
            && !child.synthetic
        {
            select_row(app, child);
        }
    } else if left {
        if current.children && app.processes_state.expanded.contains(&pid) {
            app.processes_state.toggle_expanded(pid);
        } else if current.depth > 0
            && let Some(parent) = rows[..display_idx].iter().rev().find_map(|row| match row {
                DisplayRow::Process(parent)
                    if !parent.synthetic && parent.depth < current.depth =>
                {
                    Some(parent)
                }
                _ => None,
            })
        {
            select_row(app, parent);
        }
    }
}

fn select_row(app: &mut TaskManApp, row: &RowData) {
    app.selected_process = Some(crate::app::ProcessIdentity {
        pid: row.pid,
        start_epoch_s: row.start_epoch_s,
    });
    app.processes_state.scroll_to_pid = Some(row.pid);
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
    let header =
        |i: usize| tablekit::text_width(ui, table.cols[i].label, tablekit::FONT_HDR_LABEL) + 28.0;
    let mut widths = (0..table.cols.len()).map(header).collect::<Vec<_>>();

    for row in rows {
        let DisplayRow::Process(row) = row else {
            continue;
        };
        widths[0] = widths[0].max(
            tablekit::text_width(ui, &row.name, tablekit::FONT_ROW)
                + 66.0
                + row.depth as f32 * 22.0,
        );
        // The Status cell renders glyphs plus at most the "not responding"
        // text, so auto-fit measures exactly that, not the tooltip wording.
        let glyphs = u8::from(row.status == ProcStatus::Suspended) + u8::from(row.power_throttled);
        let mut status_w = 20.0 + f32::from(glyphs) * STATUS_GLYPH_W;
        if row.status == ProcStatus::NotResponding {
            status_w += tablekit::text_width(ui, i18n::tr(K::StNotResponding), tablekit::FONT_ROW);
        }
        widths[1] = widths[1].max(status_w);
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
            widths[i + 2] =
                widths[i + 2].max(tablekit::text_width(ui, text, tablekit::FONT_ROW) + 22.0);
        }
    }
    for (i, agg) in aggs.iter().enumerate() {
        widths[i + 2] = widths[i + 2].max(tablekit::text_width(ui, agg, tablekit::FONT_AGG) + 36.0);
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
    let (rect, resp) = table.row(ui, pal, selected);

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

    // Status column, sourced from the SNAPSHOT (audit §8). Native Task
    // Manager draws glyphs here — an orange pause for suspended, a green leaf
    // for efficiency mode — and puts the words in a tooltip; only
    // "not responding" stays spelled out.
    let status_tip = status_cell(ui, pal, table, rect, row);

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
    table.heat_cells(ui, pal, rect, 2, &cells);
    let net_tip = (!row.net_available)
        .then(|| unavailable_network_tip(ui, table, rect))
        .flatten();

    if resp.clicked() {
        if row.synthetic {
            app.selected_process = None;
        } else {
            app.selected_process = Some(crate::app::ProcessIdentity {
                pid: row.pid,
                start_epoch_s: row.start_epoch_s,
            });
        }
    }
    // on_hover_text/context_menu consume the response (builder style). A
    // status glyph under the cursor explains itself first; otherwise the
    // row's own explanation (e.g. unattributable CPU) applies.
    let resp = match (status_tip, net_tip, &row.tooltip) {
        (Some(tip), _, _) => resp.on_hover_text(tip),
        (None, Some(tip), _) => resp.on_hover_text(tip),
        (None, None, Some(tip)) => resp.on_hover_text(tip),
        (None, None, None) => resp,
    };
    // Pseudo-rows carry no killable process — no action menu at all.
    if !row.synthetic {
        resp.context_menu(|ui| context_menu(app, ui, row));
    }
}

/// Explain the "—" in the Network column instead of leaving the user to
/// wonder. Per-process bytes come from an ETW session, which Windows only
/// grants to administrators; without it the honest answer is "unknown", and
/// this says why.
fn unavailable_network_tip(
    ui: &egui::Ui,
    table: &tablekit::TmTable,
    rect: egui::Rect,
) -> Option<&'static str> {
    let cell = table.col_rect(5, rect);
    let pointer = ui.ctx().pointer_latest_pos()?;
    cell.contains(pointer)
        .then_some(i18n::tr(K::NetPerProcessUnavailable))
}

/// Width the status glyph strip needs; also the auto-fit contribution when
/// a row has no "not responding" text.
const STATUS_GLYPH_W: f32 = 22.0;

/// Paint the Status cell: glyphs first, then any remaining text after them,
/// and report what the glyph under the cursor means.
///
/// The word behind each glyph stays discoverable through the ROW's tooltip
/// rather than a widget of its own: an extra hover target stacked on the row
/// would steal the row's own hover state and make the highlight flicker off
/// whenever the cursor crossed an icon.
fn status_cell(
    ui: &egui::Ui,
    pal: &theme::Palette,
    table: &tablekit::TmTable,
    rect: egui::Rect,
    row: &RowData,
) -> Option<&'static str> {
    let cell = table.col_rect(1, rect);
    let pointer = ui.ctx().pointer_latest_pos();
    let mut x = cell.left() + 10.0;
    let mut hovered = None;
    let mut glyph = |icon: Icon, color: egui::Color32, tip: &'static str| {
        let r = egui::Rect::from_center_size(
            egui::Pos2::new(x + 8.0, rect.center().y),
            egui::vec2(16.0, 16.0),
        );
        if cell.contains(r.right_center()) {
            crate::icons::draw_at(ui, r, icon, color);
            if pointer.is_some_and(|p| r.expand(3.0).contains(p)) {
                hovered = Some(tip);
            }
        }
        x += STATUS_GLYPH_W;
    };
    if row.status == ProcStatus::Suspended {
        glyph(Icon::Pause, pal.warn_orange, i18n::tr(K::StSuspended));
    }
    if row.power_throttled {
        glyph(Icon::Leaf, pal.ok_green, i18n::tr(K::StEfficiencyMode));
    }
    if row.status == ProcStatus::NotResponding {
        let text_rect = egui::Rect::from_min_max(egui::Pos2::new(x, cell.top()), cell.max);
        ui.painter_at(text_rect).text(
            egui::Pos2::new(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            i18n::tr(K::StNotResponding),
            egui::FontId::proportional(tablekit::FONT_ROW),
            pal.text,
        );
    }
    hovered
}

fn context_menu(app: &mut TaskManApp, ui: &mut egui::Ui, row: &RowData) {
    let ctx = ui.ctx().clone();
    ui.set_min_width(210.0);
    if row.children {
        let expanded = app.processes_state.expanded.contains(&row.pid);
        if ui
            .button(if expanded {
                i18n::tr(K::Collapse)
            } else {
                i18n::tr(K::Expand)
            })
            .clicked()
        {
            app.processes_state.toggle_expanded(row.pid);
            ui.close();
        }
        ui.separator();
    }
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
    if ui.button(i18n::tr(K::CopyName)).clicked() {
        ui.ctx().copy_text(row.name.clone());
        app.shared.toast(i18n::tr(K::Copied));
        ui.close();
    }
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
    #[cfg(target_os = "windows")]
    if ui.button(i18n::tr(K::CreateDumpFile)).clicked() {
        let process = app
            .latest_snapshot()
            .as_ref()
            .and_then(|snapshot| snapshot.process(row.pid))
            .cloned();
        if let Some(process) = process {
            crate::tabs::details::create_dump(app, &ctx, &process);
        } else {
            app.shared.toast(i18n::tr(K::ProcessExited));
        }
        ui.close();
    }
    if app.actions.capabilities().process_modules && ui.button(i18n::tr(K::ViewModules)).clicked() {
        let process = app
            .latest_snapshot()
            .as_ref()
            .and_then(|snapshot| snapshot.process(row.pid))
            .cloned();
        if let Some(process) = process {
            crate::tabs::modules::open(app, &process, &ctx);
        } else {
            app.shared.toast(i18n::tr(K::ProcessExited));
        }
        ui.close();
    }
    if ui.button(i18n::tr(K::OnlineSearch)).clicked() {
        let url = search::online_search_url(&row.name);
        if let Err(error) = app.actions.open_url(&url) {
            app.shared
                .toast(i18n::trf(K::ErrMsg, &[&error.to_string()]));
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
    app.end_process_identity(ctx, identity.clone(), tree, name.to_string());
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

/// Share of total machine capacity at which an external task absorbed into an
/// app family is promoted to an individually visible Background row. High
/// enough to never list idle helpers, low enough that any single-core-heavy
/// workload (builds, compilers, CLI tools) surfaces on every realistic core
/// count.
const PROMOTE_CPU_PCT: f32 = 1.0;

/// True when no ancestor inside the process's app family shares its image
/// name — i.e. the process is an external task the family spawned, not part
/// of the application's own process group (renderer/helper processes reuse
/// the main executable and stay folded like Task Manager's app children).
fn is_external_family_member(
    p: &ProcessEntry,
    by_pid: &HashMap<u32, &ProcessEntry>,
    category: &HashMap<u32, ProcCategory>,
) -> bool {
    let mut cur = p;
    let mut seen: HashSet<u32> = HashSet::new();
    seen.insert(cur.pid);
    while let Some(ppid) = cur.ppid {
        if ppid == cur.pid || !seen.insert(ppid) {
            break;
        }
        let Some(parent) = by_pid.get(&ppid).copied() else {
            break;
        };
        if category.get(&parent.pid) != Some(&ProcCategory::App) {
            break;
        }
        if parent.name.eq_ignore_ascii_case(&p.name) {
            return false;
        }
        cur = parent;
    }
    true
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
    let subtree = subtree_values_and_counts(&all, &children_all);
    let mut out = Vec::new();

    if !q.is_empty() {
        let mut matched: Vec<&ProcessEntry> = all
            .iter()
            .copied()
            .filter(|p| q.matches_process(p))
            .collect();
        sort_entries(&mut matched, sort_col, ascending, &subtree.values);
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
        // TM parity: the three group sections exist only in the Name/Status
        // sorted views. Any resource sort (CPU/memory/disk/network) flattens
        // the list into ONE globally sorted sequence, so the top consumer is
        // always the first row no matter which category it belongs to. App
        // families and same-image groups stay collapsible inside that flat
        // list.
        if sort_col < 2 {
            out.push(DisplayRow::GroupHeader(gi, total));
            if group_collapsed[gi as usize] {
                continue;
            }
        }
        if cat == ProcCategory::App {
            emit_tree(
                &mut out, &roots, &children, &subtree, sort_col, ascending, expanded,
            );
        } else {
            // Task Manager parity: Background/Windows groups are flat lists
            // in which connected same-image families collapse into one
            // expandable "Name (N)" row ("Dropbox (7)"). Mixed-image trees
            // stay fully flat so a busy build tool under a console shell is
            // never hidden inside an unexpanded parent.
            emit_flat_with_family_groups(
                &mut out, &members, &children, &subtree, sort_col, ascending, expanded,
            );
        }
    }
    if sort_col >= 2 {
        sort_blocks_globally(&mut out, sort_col, ascending);
    }
    normalize_heat(&mut out);
    out
}

/// Resource-sorted view: reorder the per-section emission into ONE global
/// order. The emitters above produce self-contained blocks (a depth-0 head
/// row plus its expanded, nested children); sorting whole blocks keeps
/// expanded families attached to their head while the heads compete
/// globally by the chosen resource column — exactly native Task Manager's
/// behavior. Group headers are dropped here (they only exist for Name
/// sorts).
fn sort_blocks_globally(rows: &mut Vec<DisplayRow>, sort_col: usize, ascending: bool) {
    debug_assert!((2..=5).contains(&sort_col), "resource column expected");
    let mut blocks: Vec<Vec<DisplayRow>> = Vec::new();
    for r in rows.drain(..) {
        let DisplayRow::Process(d) = r else { continue };
        if d.depth == 0 {
            blocks.push(vec![DisplayRow::Process(d)]);
        } else {
            // Expanded children were emitted directly after their head;
            // they keep riding along with it.
            if let Some(last) = blocks.last_mut() {
                last.push(DisplayRow::Process(d));
            }
        }
    }
    let vi = sort_col - 2; // columns cpu/mem/disk/net map onto values[0..=3]
    blocks.sort_by(|a, b| {
        let (Some(DisplayRow::Process(x)), Some(DisplayRow::Process(y))) = (a.first(), b.first())
        else {
            return std::cmp::Ordering::Equal;
        };
        let o = x.values[vi]
            .partial_cmp(&y.values[vi])
            .unwrap_or(std::cmp::Ordering::Equal);
        if ascending { o } else { o.reverse() }
    });
    rows.extend(blocks.into_iter().flatten());
}

/// All processes of the connected same-image family rooted at `p`, or None
/// when any descendant runs a different executable image.
fn same_image_family<'a>(
    p: &'a ProcessEntry,
    children: &HashMap<u32, Vec<&'a ProcessEntry>>,
) -> Option<Vec<&'a ProcessEntry>> {
    let mut out = vec![p];
    let mut seen: HashSet<u32> = HashSet::new();
    seen.insert(p.pid);
    let mut stack = vec![p];
    while let Some(cur) = stack.pop() {
        for kid in children.get(&cur.pid).into_iter().flatten() {
            if !seen.insert(kid.pid) {
                continue;
            }
            if !kid.name.eq_ignore_ascii_case(&p.name) {
                return None;
            }
            out.push(*kid);
            stack.push(kid);
        }
    }
    Some(out)
}

/// Flat Background/Windows rendering with same-image family collapse: every
/// process gets its own row (own values, no nesting), except families whose
/// members all share the root's image — they render as one expandable
/// "Name (N)" row with the family aggregate, expanding to member rows.
fn emit_flat_with_family_groups(
    out: &mut Vec<DisplayRow>,
    members: &[&ProcessEntry],
    children: &HashMap<u32, Vec<&ProcessEntry>>,
    subtree: &Subtree,
    sort_col: usize,
    ascending: bool,
    expanded: &HashSet<u32>,
) {
    let mut family_heads: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
    let mut swallowed: HashSet<u32> = HashSet::new();
    for p in members {
        if let Some(fam) = same_image_family(p, children)
            && fam.len() > 1
        {
            family_heads.insert(p.pid, fam.clone());
            for member in fam.iter().skip(1) {
                swallowed.insert(member.pid);
            }
        }
    }

    // Representative values: family heads sort by their aggregate, plain
    // rows by their own values.
    let mut repr: HashMap<u32, [f64; 4]> = HashMap::with_capacity(members.len());
    for p in members {
        if family_heads.contains_key(&p.pid) {
            repr.insert(p.pid, subtree.values(p.pid));
        } else if !swallowed.contains(&p.pid) {
            repr.insert(p.pid, own_values(p));
        }
    }
    let mut display: Vec<&ProcessEntry> = members
        .iter()
        .copied()
        .filter(|p| !swallowed.contains(&p.pid))
        .collect();
    sort_entries(&mut display, sort_col, ascending, &repr);

    for p in display {
        if let Some(fam) = family_heads.get(&p.pid) {
            let net_available = fam
                .iter()
                .any(|k| k.net_recv_bps.is_some() || k.net_sent_bps.is_some());
            out.push(DisplayRow::Process(RowData {
                pid: p.pid,
                start_epoch_s: p.start_epoch_s,
                depth: 0,
                name: format!("{} ({})", p.shown_name(), fam.len()),
                icon_path: p
                    .exe_path
                    .as_ref()
                    .map(|x| x.to_string_lossy().into_owned()),
                children: true,
                values: repr.get(&p.pid).copied().unwrap_or([0.0; 4]),
                heat: [0.0; 4],
                net_available,
                status: p.status,
                // The family row stands for every member, so it reports the
                // family's efficiency state — not just the head's.
                power_throttled: fam.iter().any(|k| k.power_throttled == Some(true)),
                synthetic: p.synthetic,
                tooltip: synthetic_tooltip(p),
            }));
            if expanded.contains(&p.pid) {
                let mut kids: Vec<&ProcessEntry> = fam.iter().skip(1).copied().collect();
                let own: HashMap<u32, [f64; 4]> =
                    kids.iter().map(|k| (k.pid, own_values(k))).collect();
                sort_entries(&mut kids, sort_col, ascending, &own);
                for k in kids {
                    out.push(make_own_row(k, 1));
                }
            }
        } else {
            out.push(make_own_row(p, 0));
        }
    }
}

fn derive_display_groups(all: &[&ProcessEntry]) -> DisplayGroups {
    let by_pid: HashMap<u32, &ProcessEntry> = all.iter().map(|p| (p.pid, *p)).collect();

    // Non-Windows/mock backends may not provide window ownership. Preserve
    // their collector classification rather than demoting every App simply
    // because the Windows-specific signal is absent.
    if !all.iter().any(|p| p.has_window) {
        let category: HashMap<u32, ProcCategory> =
            all.iter().map(|p| (p.pid, p.category)).collect();
        let mut app_roots: HashSet<u32> = all
            .iter()
            .copied()
            .filter(|p| p.category == ProcCategory::App && p.app_root)
            .map(|p| p.pid)
            .collect();
        if app_roots.is_empty() {
            for p in all
                .iter()
                .copied()
                .filter(|p| p.category == ProcCategory::App)
            {
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
            // A visible window only folds into an ancestor's family when
            // they are plausibly the same application (same executable or
            // same publisher). Start-menu/COM launches are brokered by
            // windowless shell-session processes (sihost, RuntimeBroker,
            // dllhost, ...); without this check those brokers adopt the
            // launched app and hide it inside an anonymous family.
            if !plausibly_same_application(parent, cur) {
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
        category: promote_busy_external_tasks(all, &by_pid, &raw_children, category, &app_roots),
        app_roots,
    }
}

/// Two processes plausibly belong to the same application when they share
/// the executable image or the publisher (company) from version metadata.
/// Unknown publisher data falls back to the permissive default so missing
/// version info never splits an existing family.
fn plausibly_same_application(a: &ProcessEntry, b: &ProcessEntry) -> bool {
    if a.name.eq_ignore_ascii_case(&b.name) {
        return true;
    }
    match (&a.company, &b.company) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => true,
    }
}

/// High-CPU external tasks must stay recognizable on the Processes page even
/// though idle helpers of an app are folded into its family row. Everything
/// a family spawned that is not one of its own images and that burns real
/// CPU is lifted out into the Background group (with its own descendants),
/// where it appears as an ordinary top-level tree. Decisions are made
/// against the pre-promotion categories so the result never depends on
/// iteration order.
fn promote_busy_external_tasks<'a>(
    all: &[&'a ProcessEntry],
    by_pid: &HashMap<u32, &'a ProcessEntry>,
    raw_children: &HashMap<u32, Vec<&'a ProcessEntry>>,
    mut category: HashMap<u32, ProcCategory>,
    app_roots: &HashSet<u32>,
) -> HashMap<u32, ProcCategory> {
    // Decide against the pre-promotion categories so the result never
    // depends on iteration order.
    let busy_external: Vec<u32> = all
        .iter()
        .copied()
        .filter(|p| {
            // A process with a visible window is a foreground app — it may
            // be mis-absorbed, but demoting it to Background would mislabel
            // it worse (HitmanPro started from the Start menu). Mis-absorbed
            // windowed processes are handled by plausibly_same_application
            // instead, which makes them their own app root.
            !p.has_window
                && p.cpu_pct >= PROMOTE_CPU_PCT
                && category.get(&p.pid) == Some(&ProcCategory::App)
                && !app_roots.contains(&p.pid)
                && is_external_family_member(p, by_pid, &category)
        })
        .map(|p| p.pid)
        .collect();
    // Move the promoted task and its absorbed descendants wholesale so the
    // family does not split across groups.
    let mut stack: Vec<u32> = busy_external;
    while let Some(pid) = stack.pop() {
        category.insert(pid, ProcCategory::Background);
        if let Some(kids) = raw_children.get(&pid) {
            for kid in kids {
                // Windowed children stay Apps: detached from their promoted
                // parent they surface as their own app root instead of a
                // GUI window landing in Background.
                if !kid.has_window
                    && category.get(&kid.pid) == Some(&ProcCategory::App)
                    && !app_roots.contains(&kid.pid)
                {
                    stack.push(kid.pid);
                }
            }
        }
    }
    category
}

/// Parent processes that launch independent foreground applications rather
/// than semantically owning them. Explorer is the important case; console
/// shells cover GUI programs launched from Terminal/cmd/PowerShell. Browsers
/// are launch surfaces for downloaded/opened executables. The shell-session
/// brokers cover Start-menu/COM activation, which reports the launching
/// broker (not explorer) as the parent process.
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
        "sihost.exe",
        "RuntimeBroker.exe",
        "dllhost.exe",
        "applicationframehost.exe",
        "backgroundtaskhost.exe",
        "textinputhost.exe",
        "smartscreen.exe",
        "msedge.exe",
        "chrome.exe",
        "brave.exe",
        "chromium.exe",
        "firefox.exe",
        "opera.exe",
        "vivaldi.exe",
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

fn make_flat_row(p: &ProcessEntry, subtree: &Subtree) -> DisplayRow {
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
        values: subtree.values(p.pid),
        heat: [0.0; 4],
        net_available: p.net_recv_bps.is_some() || p.net_sent_bps.is_some(),
        status: p.status,
        power_throttled: p.power_throttled == Some(true),
        synthetic: p.synthetic,
        tooltip: synthetic_tooltip(p),
    })
}

/// The process's own resource values (no subtree aggregation).
fn own_values(p: &ProcessEntry) -> [f64; 4] {
    [
        p.cpu_pct as f64,
        p.mem_bytes as f64,
        p.disk_read_bps + p.disk_write_bps,
        p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0),
    ]
}

/// Flat Background/Windows row: plain name, own values, no expand handle.
fn make_own_row(p: &ProcessEntry, depth: usize) -> DisplayRow {
    DisplayRow::Process(RowData {
        pid: p.pid,
        start_epoch_s: p.start_epoch_s,
        depth,
        name: p.shown_name().to_string(),
        icon_path: p
            .exe_path
            .as_ref()
            .map(|x| x.to_string_lossy().into_owned()),
        children: false,
        values: own_values(p),
        heat: [0.0; 4],
        net_available: p.net_recv_bps.is_some() || p.net_sent_bps.is_some(),
        status: p.status,
        power_throttled: p.power_throttled == Some(true),
        synthetic: p.synthetic,
        tooltip: synthetic_tooltip(p),
    })
}

/// Localized hover text for the CPU pseudo-rows explaining WHICH programs
/// account for the unattributable load.
fn synthetic_tooltip(p: &ProcessEntry) -> Option<String> {
    if !p.synthetic {
        return None;
    }
    p.description
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(|d| i18n::trf(K::TerminatedTooltip, &[d]))
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

fn emit_tree<'a>(
    out: &mut Vec<DisplayRow>,
    roots: &[&'a ProcessEntry],
    children: &HashMap<u32, Vec<&'a ProcessEntry>>,
    subtree: &Subtree,
    sort_col: usize,
    ascending: bool,
    expanded: &HashSet<u32>,
) {
    let mut sorted_roots: Vec<&ProcessEntry> = roots.to_vec();
    sort_entries(&mut sorted_roots, sort_col, ascending, &subtree.values);
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
                sort_entries(&mut v, sort_col, ascending, &subtree.values);
                v
            })
            .clone();
        let has_children = !kids.is_empty();
        let count = subtree.count(proc.pid);
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
            values: subtree.values(proc.pid),
            heat: [0.0; 4],
            net_available: proc.net_recv_bps.is_some() || proc.net_sent_bps.is_some(),
            status: proc.status,
            // An app row summarizes its whole family (see [`Subtree`]), so a
            // collapsed browser shows the leaf its renderers earned.
            power_throttled: subtree.efficiency(proc.pid),
            synthetic: proc.synthetic,
            tooltip: synthetic_tooltip(proc),
        }));
        if has_children && expanded.contains(&proc.pid) {
            for k in kids.into_iter().rev() {
                stack.push((k, depth + 1));
            }
        }
    }
}

/// Per-pid subtree rollups shared by every Processes row builder.
struct Subtree {
    /// cpu %, mem bytes, disk bps, net bps summed over the display subtree.
    values: HashMap<u32, [f64; 4]>,
    counts: HashMap<u32, u32>,
    /// True when the process OR any of its display descendants runs in
    /// efficiency mode. A collapsed `Brave Browser (24)` row summarizes its
    /// children's resources, so it must summarize their power state too —
    /// that is where native Task Manager shows the leaf.
    efficiency: HashMap<u32, bool>,
}

impl Subtree {
    fn values(&self, pid: u32) -> [f64; 4] {
        self.values.get(&pid).copied().unwrap_or([0.0; 4])
    }

    fn count(&self, pid: u32) -> u32 {
        self.counts.get(&pid).copied().unwrap_or(1)
    }

    fn efficiency(&self, pid: u32) -> bool {
        self.efficiency.get(&pid).copied().unwrap_or(false)
    }
}

fn subtree_values_and_counts<'a>(
    all: &[&'a ProcessEntry],
    children: &HashMap<u32, Vec<&'a ProcessEntry>>,
) -> Subtree {
    let mut out: HashMap<u32, [f64; 4]> = HashMap::with_capacity(all.len());
    let mut counts: HashMap<u32, u32> = HashMap::with_capacity(all.len());
    let mut eco: HashMap<u32, bool> = HashMap::with_capacity(all.len());
    let by_pid: HashMap<u32, &'a ProcessEntry> = all.iter().map(|p| (p.pid, *p)).collect();

    enum Frame<'b> {
        Enter(u32),
        Combine(u32, Vec<&'b ProcessEntry>, [f64; 4], bool),
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
                Frame::Combine(pid, kids, mut acc, mut throttled) => {
                    let mut cnt: u32 = 1;
                    for k in &kids {
                        if let Some(v) = out.get(&k.pid) {
                            for i in 0..4 {
                                acc[i] += v[i];
                            }
                            cnt += counts.get(&k.pid).copied().unwrap_or(1);
                            throttled |= eco.get(&k.pid).copied().unwrap_or(false);
                        }
                    }
                    out.insert(pid, acc);
                    counts.insert(pid, cnt);
                    eco.insert(pid, throttled);
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
                        out.insert(pid, own_values(p));
                        counts.insert(pid, 1);
                        eco.insert(pid, p.power_throttled == Some(true));
                        done.insert(pid);
                        in_progress.remove(&pid);
                    } else {
                        stack.push(Frame::Combine(
                            pid,
                            kids,
                            own_values(p),
                            p.power_throttled == Some(true),
                        ));
                        for k in pending {
                            stack.push(Frame::Enter(k.pid));
                        }
                    }
                }
            }
        }
    }
    for p in all {
        out.entry(p.pid).or_insert_with(|| own_values(p));
        counts.entry(p.pid).or_insert(1);
    }
    Subtree {
        values: out,
        counts,
        efficiency: eco,
    }
}

fn sort_entries(v: &mut [&ProcessEntry], col: usize, asc: bool, subtree: &HashMap<u32, [f64; 4]>) {
    let sv = |p: &ProcessEntry, i: usize| subtree.get(&p.pid).map_or(0.0, |s| s[i]);
    v.sort_by(|a, b| {
        let o = match col {
            1 => process_status_rank(a).cmp(&process_status_rank(b)),
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

fn process_status_rank(process: &ProcessEntry) -> (u8, bool) {
    let status = match process.status {
        ProcStatus::Running => 0,
        ProcStatus::Suspended => 1,
        ProcStatus::NotResponding => 2,
    };
    (status, process.power_throttled == Some(true))
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
    fn status_column_sorts_status_instead_of_name() {
        let mut running = proc(1, None, "z-running.exe", ProcCategory::Background);
        let mut suspended = proc(2, None, "a-suspended.exe", ProcCategory::Background);
        let mut hung = proc(3, None, "b-hung.exe", ProcCategory::Background);
        running.status = ProcStatus::Running;
        suspended.status = ProcStatus::Suspended;
        hung.status = ProcStatus::NotResponding;
        let mut rows = vec![&hung, &suspended, &running];
        sort_entries(&mut rows, 1, true, &HashMap::new());
        assert_eq!(
            rows.iter().map(|process| process.pid).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn group_totals_are_independent_of_expansion_state() {
        let mut snap_procs = vec![
            proc(1, None, "app", ProcCategory::App),
            proc(2, Some(1), "child", ProcCategory::App),
            proc(3, Some(2), "grandchild", ProcCategory::App),
            proc(4, None, "bg", ProcCategory::Background),
        ];
        for p in &mut snap_procs {
            p.cpu_pct = 0.0;
        }
        let snap = snap_of(snap_procs);
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
        // Browser main owns a window; its windowless helper stays folded.
        // brave.exe is a launch boundary: a same-image secondary window
        // (like a PWA) starts its own group, and DIFFERENT-image programs
        // launched by the browser are never absorbed at all.
        let mut brave = proc(2, Some(1), "brave.exe", ProcCategory::App);
        brave.has_window = true;
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
        assert_eq!(app_total, 4);

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
        assert_eq!(app_rows.len(), 4);
        assert!(app_rows.iter().all(|r| r.depth == 0));

        let explorer_row = app_rows.iter().find(|r| r.pid == 1).unwrap();
        assert_eq!(explorer_row.name, "explorer.exe");
        assert_eq!(explorer_row.values[0], 1.0);

        // Browser main + helper are one family; the secondary same-image
        // window is its own group (Task Manager shows PWAs separately).
        let brave_row = app_rows.iter().find(|r| r.pid == 2).unwrap();
        assert_eq!(brave_row.name, "brave.exe (2)");
        assert_eq!(brave_row.values[0], 6.0);
        let pwa_row = app_rows.iter().find(|r| r.pid == 3).unwrap();
        assert_eq!(pwa_row.name, "brave.exe");
        assert_eq!(pwa_row.values[0], 3.0);

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
        let mut shell = proc(11, Some(10), "powershell.exe", ProcCategory::App);
        // Real shells idle below the promotion threshold; keep the fixture
        // faithful to that so the shell stays folded into the terminal group.
        shell.cpu_pct = 0.0;
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
        assert_eq!(
            app_rows.iter().find(|r| r.pid == 10).unwrap().name,
            "WindowsTerminal.exe (2)"
        );
        assert_eq!(
            app_rows.iter().find(|r| r.pid == 12).unwrap().name,
            "notepad.exe"
        );
    }

    #[test]
    fn process_tree_supports_three_plus_levels() {
        let mut snap_procs = vec![
            proc(1, None, "root", ProcCategory::App),
            proc(2, Some(1), "child", ProcCategory::App),
            proc(3, Some(2), "grandchild", ProcCategory::App),
            proc(4, Some(3), "great", ProcCategory::App),
        ];
        for p in &mut snap_procs {
            p.cpu_pct = 0.0;
        }
        let snap = snap_of(snap_procs);
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
        let mut snap_procs = vec![
            proc(1, None, "Brave", ProcCategory::App),
            proc(2, Some(1), "Child", ProcCategory::App),
            proc(3, Some(2), "GC", ProcCategory::App),
            proc(4, Some(3), "GGC", ProcCategory::App),
        ];
        for p in &mut snap_procs {
            p.cpu_pct = 0.0;
        }
        let snap = snap_of(snap_procs);
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
        let st = subtree_values_and_counts(&all, &children);
        assert_eq!(st.values(1)[0], 6.0);
        assert_eq!(st.values(1)[1], 6000.0);
        assert_eq!(st.values(2)[0], 5.0);
        assert_eq!(st.values(3)[0], 3.0);
        assert_eq!((st.count(3), st.count(2), st.count(1)), (1, 2, 3));
    }

    /// A collapsed family row stands for its members, so one efficiency-mode
    /// descendant must light the leaf on the head row — that is where native
    /// Task Manager shows it for a browser.
    #[test]
    fn efficiency_mode_rolls_up_to_the_group_row() {
        let mut root = proc(1, None, "brave.exe", ProcCategory::App);
        root.has_window = true;
        let mut renderer = proc(2, Some(1), "brave.exe", ProcCategory::App);
        renderer.power_throttled = Some(true);
        let quiet = proc(3, Some(1), "brave.exe", ProcCategory::App);
        let snap = snap_of(vec![root, renderer, quiet]);
        let all: Vec<&ProcessEntry> = snap.processes.iter().collect();
        let grouping = derive_display_groups(&all);
        let children = display_children_map(&all, &grouping.category, &grouping.app_roots);
        let st = subtree_values_and_counts(&all, &children);
        assert!(st.efficiency(1), "head inherits its renderer's leaf");
        assert!(st.efficiency(2));
        assert!(!st.efficiency(3), "an untouched sibling stays plain");

        let rows = build_display_rows(&snap, "", 0, true, &HashSet::new(), &[false; 3]);
        let head = rows
            .iter()
            .find_map(|row| match row {
                DisplayRow::Process(row) if row.pid == 1 => Some(row),
                _ => None,
            })
            .expect("head row");
        assert!(head.power_throttled, "collapsed app row shows the leaf");
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

    /// The counterpart to `missing_network_renders_unavailable_not_zero`:
    /// once the ETW trace supplies rates they must reach the Network column
    /// as a real value, and a family row must sum its members.
    #[test]
    fn measured_network_reaches_the_column_and_aggregates() {
        let mut root = proc(1, None, "svc.exe", ProcCategory::Background);
        root.net_recv_bps = Some(1_000.0);
        root.net_sent_bps = Some(250.0);
        let mut child = proc(2, Some(1), "svc.exe", ProcCategory::Background);
        child.net_recv_bps = Some(500.0);
        child.net_sent_bps = Some(0.0);
        let snap = snap_of(vec![root, child]);
        let rows = build_display_rows(&snap, "", 0, true, &HashSet::new(), &[false; 3]);
        let head = rows
            .iter()
            .find_map(|row| match row {
                DisplayRow::Process(row) if row.pid == 1 => Some(row.clone()),
                _ => None,
            })
            .expect("family head row");
        assert!(head.net_available, "a measured rate is not 'unavailable'");
        // 1000 + 250 (own) + 500 + 0 (child) bytes/s over the family.
        assert!((head.values[3] - 1_750.0).abs() < f64::EPSILON);
        // ...and the column renders a rate, not the unavailable dash.
        let rendered = format::format_mbit(head.values[3]);
        assert!(rendered != "—" && rendered.starts_with('0'), "{rendered}");
        // A zero rate is a MEASUREMENT, so it still counts as available.
        let mut quiet = proc(3, None, "quiet.exe", ProcCategory::Background);
        quiet.net_recv_bps = Some(0.0);
        quiet.net_sent_bps = Some(0.0);
        let rows = build_display_rows(
            &snap_of(vec![quiet]),
            "",
            0,
            true,
            &HashSet::new(),
            &[false; 3],
        );
        let row = rows
            .iter()
            .find_map(|row| match row {
                DisplayRow::Process(row) => Some(row.clone()),
                _ => None,
            })
            .unwrap();
        assert!(row.net_available, "measured zero must not read as unknown");
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

    fn rows_in_group(rows: &[DisplayRow], gi: u8) -> Vec<&RowData> {
        rows.iter()
            .skip_while(|r| !matches!(r, DisplayRow::GroupHeader(g, _) if *g == gi))
            .skip(1)
            .take_while(|r| !matches!(r, DisplayRow::GroupHeader(..)))
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn background_same_image_family_collapses_into_expandable_group() {
        // Task Manager shows e.g. "Dropbox (7)": a connected family whose
        // members all share the image collapses into one expandable row with
        // the family aggregate.
        let mut main = proc(1, Some(99), "Dropbox.exe", ProcCategory::Background);
        main.cpu_pct = 1.0;
        let mut sub1 = proc(2, Some(1), "Dropbox.exe", ProcCategory::Background);
        sub1.cpu_pct = 2.0;
        let sub2 = proc(3, Some(1), "Dropbox.exe", ProcCategory::Background);
        let sub3 = proc(4, Some(2), "Dropbox.exe", ProcCategory::Background);
        let groups = [false; 3];
        let collapsed = build_display_rows(
            &snap_of(vec![main.clone(), sub1.clone(), sub2.clone(), sub3.clone()]),
            "",
            0,
            true,
            &HashSet::new(),
            &groups,
        );
        let bg = rows_in_group(&collapsed, 1);
        assert_eq!(bg.len(), 1, "family renders as one row");
        assert_eq!(bg[0].name, "Dropbox.exe (4)");
        assert_eq!(bg[0].values[0], 10.0, "family aggregate (1+2+3+4)");
        assert!(bg[0].children, "group row is expandable");

        let mut expanded = HashSet::new();
        expanded.insert(1u32);
        let open = build_display_rows(
            &snap_of(vec![main, sub1.clone(), sub2.clone(), sub3.clone()]),
            "",
            0,
            true,
            &expanded,
            &groups,
        );
        let bg_open = rows_in_group(&open, 1);
        assert_eq!(bg_open.len(), 4, "group row plus members");
        assert_eq!(bg_open[0].name, "Dropbox.exe (4)");
        assert!(bg_open[1..].iter().all(|r| r.depth == 1));
        assert!(bg_open[1..].iter().all(|r| !r.children));
    }

    #[test]
    fn background_same_name_separate_families_stay_separate_rows() {
        // Task Manager does NOT merge unrelated same-name processes
        // (EpicWebHelper appears twice ungrouped): only connected families
        // collapse.
        let a = proc(1, None, "helper.exe", ProcCategory::Background);
        let b = proc(2, None, "helper.exe", ProcCategory::Background);
        let groups = [false; 3];
        let rows = build_display_rows(&snap_of(vec![a, b]), "", 0, true, &HashSet::new(), &groups);
        let bg = rows_in_group(&rows, 1);
        assert_eq!(bg.len(), 2);
        assert!(bg.iter().all(|r| !r.children));
        assert!(bg.iter().all(|r| !r.name.contains('(')));
    }

    #[test]
    fn background_group_renders_flat_so_busy_children_stay_visible() {
        // A console-shell chain classified Background (e.g. cmd from the
        // shell): the busy build tool must be its own row without needing to
        // expand anything — this was the "100% CPU not identifiable" bug.
        let mut shell = proc(1, Some(99), "cmd.exe", ProcCategory::Background);
        shell.cpu_pct = 0.0;
        let mut build = proc(2, Some(1), "cargo.exe", ProcCategory::Background);
        build.cpu_pct = 6.0;
        let mut leaf = proc(3, Some(2), "rustc.exe", ProcCategory::Background);
        leaf.cpu_pct = 0.0;
        let groups = [false; 3];
        let rows = build_display_rows(
            &snap_of(vec![shell, build, leaf]),
            "",
            0,
            true,
            &HashSet::new(),
            &groups,
        );
        let bg = rows_in_group(&rows, 1);
        // Name-sorted (default); every process is a flat top-level row.
        assert_eq!(
            bg.iter().map(|r| (r.pid, r.depth)).collect::<Vec<_>>(),
            vec![(2, 0), (1, 0), (3, 0)]
        );
        let cargo = bg.iter().find(|r| r.pid == 2).unwrap();
        assert_eq!(cargo.values[0], 6.0, "row shows the process's own CPU");
        assert!(!cargo.children, "no expand handle needed to see it");
    }

    #[test]
    fn browser_or_shell_launched_windowed_app_is_its_own_app_row() {
        // A windowed program whose parent chain runs through shell/session
        // brokers (Start menu, COM activation, browser download) must become
        // its own app row — not be absorbed into the broker's family.
        let mut broker = proc(1, None, "shellbroker.exe", ProcCategory::Background);
        broker.company = Some("Microsoft Corporation".into());
        let mut app = proc(2, Some(1), "HitmanPro_x64.exe", ProcCategory::App);
        app.has_window = true;
        app.company = Some("Sophos Limited".into());
        let groups = [false; 3];
        let rows = build_display_rows(
            &snap_of(vec![broker, app]),
            "",
            0,
            true,
            &HashSet::new(),
            &groups,
        );
        let apps = rows_in_group(&rows, 0);
        assert_eq!(
            apps.iter().map(|r| (r.pid, r.depth)).collect::<Vec<_>>(),
            vec![(2, 0)],
            "the launched app is its own top-level app row"
        );
        // The broker stays Background (windowless, no window of its own).
        let bg = rows_in_group(&rows, 1);
        assert!(bg.iter().any(|r| r.pid == 1));
        assert!(!bg.iter().any(|r| r.pid == 2), "app must not be Background");
    }

    #[test]
    fn shell_broker_parent_is_a_launch_boundary_even_without_publisher_data() {
        // Same scenario, but version metadata is unavailable: the well-known
        // broker names must still act as launch boundaries.
        let broker = proc(1, None, "runtimebroker.exe", ProcCategory::Background);
        let mut app = proc(2, Some(1), "app.exe", ProcCategory::App);
        app.has_window = true;
        let groups = [false; 3];
        let rows = build_display_rows(
            &snap_of(vec![broker, app]),
            "",
            0,
            true,
            &HashSet::new(),
            &groups,
        );
        let apps = rows_in_group(&rows, 0);
        assert_eq!(apps.iter().map(|r| r.pid).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn windowed_busy_family_member_is_never_demoted_to_background() {
        // steamwebhelper owns the Steam window and burns CPU; it is part of
        // the Steam family (same publisher) and must stay in Apps — busy or
        // not, a GUI process is never a Background row.
        let mut steam = proc(1, None, "steam.exe", ProcCategory::App);
        steam.has_window = false;
        steam.company = Some("Valve Corporation".into());
        let mut helper = proc(2, Some(1), "steamwebhelper.exe", ProcCategory::App);
        helper.has_window = true;
        helper.company = Some("Valve Corporation".into());
        helper.cpu_pct = 40.0;
        let groups = [false; 3];
        let rows = build_display_rows(
            &snap_of(vec![steam, helper]),
            "",
            0,
            true,
            &HashSet::new(),
            &groups,
        );
        let bg = rows_in_group(&rows, 1);
        assert!(bg.is_empty(), "busy windowed family member stays Apps");
        let apps = rows_in_group(&rows, 0);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "steam.exe (2)");
        assert_eq!(apps[0].values[0], 41.0, "family aggregate keeps helper");
    }

    #[test]
    fn busy_external_helper_is_promoted_to_background() {
        let mut code = proc(1, None, "Code.exe", ProcCategory::App);
        code.has_window = true;
        let mut build = proc(2, Some(1), "cargo.exe", ProcCategory::App);
        build.cpu_pct = 8.0;
        let mut watcher = proc(3, Some(1), "watcher.exe", ProcCategory::App);
        watcher.cpu_pct = 0.0;
        let groups = [false; 3];
        let rows = build_display_rows(
            &snap_of(vec![code, build, watcher]),
            "",
            0,
            true,
            &HashSet::new(),
            &groups,
        );

        let bg = rows_in_group(&rows, 1);
        assert_eq!(bg.iter().map(|r| r.pid).collect::<Vec<_>>(), vec![2]);
        assert_eq!(bg[0].depth, 0, "promoted task is a top-level row");
        assert_eq!(bg[0].values[0], 8.0);

        // The idle helper stays folded into the app family (no spam).
        let apps = rows_in_group(&rows, 0);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].pid, 1);
        assert_eq!(apps[0].name, "Code.exe (2)");
        // Promoted CPU no longer counts into the family aggregate (only the
        // root's own 1.0% from the fixture remains).
        assert_eq!(apps[0].values[0], 1.0);
    }

    #[test]
    fn busy_same_image_helper_stays_in_family() {
        let mut brave = proc(1, None, "brave.exe", ProcCategory::App);
        brave.has_window = true;
        let mut renderer = proc(2, Some(1), "brave.exe", ProcCategory::App);
        renderer.cpu_pct = 40.0;
        let groups = [false; 3];
        let mut expanded = HashSet::new();
        expanded.insert(1u32);
        let rows = build_display_rows(
            &snap_of(vec![brave, renderer]),
            "",
            0,
            true,
            &expanded,
            &groups,
        );
        let bg = rows_in_group(&rows, 1);
        assert!(bg.is_empty(), "same-image helpers must not be promoted");
        let apps = rows_in_group(&rows, 0);
        assert_eq!(apps.len(), 2, "family row plus expandable child");
        assert_eq!(apps[0].values[0], 41.0, "family aggregate keeps child");
    }

    #[test]
    fn promoted_task_brings_absorbed_descendants_to_background() {
        let mut code = proc(1, None, "Code.exe", ProcCategory::App);
        code.has_window = true;
        let mut cargo = proc(2, Some(1), "cargo.exe", ProcCategory::App);
        cargo.cpu_pct = 8.0;
        let mut rustc = proc(3, Some(2), "rustc.exe", ProcCategory::App);
        rustc.cpu_pct = 0.0; // idle: must follow via wholesale descent
        let groups = [false; 3];
        let mut expanded = HashSet::new();
        expanded.insert(2u32);
        let rows = build_display_rows(
            &snap_of(vec![code, cargo, rustc]),
            "",
            0,
            true,
            &expanded,
            &groups,
        );
        let bg = rows_in_group(&rows, 1);
        // Background is flat: promoted task and its descendants are separate
        // top-level rows.
        assert_eq!(
            bg.iter().map(|r| (r.pid, r.depth)).collect::<Vec<_>>(),
            vec![(2, 0), (3, 0)],
            "promoted task and its descendants are individually visible"
        );
        let apps = rows_in_group(&rows, 0);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "Code.exe", "family aggregate loses subtree");
    }

    fn pseudo(pid: u32, name: &str, cat: ProcCategory) -> ProcessEntry {
        let mut p = proc(pid, None, name, cat);
        p.synthetic = true;
        p
    }

    #[test]
    fn cpu_sort_flattens_groups_and_puts_top_consumer_first() {
        let mut term = pseudo(
            u32::MAX - 1,
            "Terminated Processes",
            ProcCategory::Background,
        );
        term.display = "Terminated processes (3)".into();
        term.cpu_pct = 45.0;
        term.description = Some("rustc.exe \u{d7}2, cargo.exe \u{d7}1".into());
        let mut irq = pseudo(u32::MAX, "System Interrupts", ProcCategory::System);
        irq.cpu_pct = 3.0;
        let mut app = proc(2, None, "app.exe", ProcCategory::App);
        app.cpu_pct = 5.0;
        app.has_window = true;

        // Sorted by CPU descending (column 2): native TM flattens the group
        // sections and sorts ALL categories together — the terminated-
        // processes row (45 %) must be the very first row.
        let rows = build_display_rows(
            &snap_of(vec![app.clone(), term.clone(), irq.clone()]),
            "",
            2,
            false,
            &HashSet::new(),
            &[false; 3],
        );
        let flat: Vec<&RowData> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d),
                DisplayRow::GroupHeader(..) => None,
            })
            .collect();
        assert!(
            rows.iter()
                .all(|r| !matches!(r, DisplayRow::GroupHeader(..))),
            "resource sorts have no group sections"
        );
        assert_eq!(
            flat.iter().map(|d| d.pid).collect::<Vec<_>>(),
            vec![u32::MAX - 1, 2, u32::MAX],
            "global order by CPU desc across all categories"
        );
        assert!(flat[0].synthetic);
        assert_eq!(flat[0].values[0], 45.0);
        assert!(!flat[1].synthetic);

        // Name sort (column 0) keeps the TM group sections.
        let rows = build_display_rows(
            &snap_of(vec![app, term, irq]),
            "",
            0,
            true,
            &HashSet::new(),
            &[false; 3],
        );
        let headers: Vec<u8> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::GroupHeader(g, _) => Some(*g),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec![0, 1, 2]);
        let bg = rows_in_group(&rows, 1);
        assert_eq!(bg.len(), 1);
        assert!(bg[0].synthetic);
        let win = rows_in_group(&rows, 2);
        assert_eq!(win.len(), 1);
        assert_eq!(win[0].values[0], 3.0);
    }

    #[test]
    fn expanded_family_stays_attached_when_blocks_resort() {
        // A same-image app family with a busy child (same image → never
        // promoted out of the family): in the CPU-sorted flat view the
        // family head competes with its subtree aggregate; expanding it
        // must keep the child directly below the head even though the head
        // alone would sort far lower.
        let mut head = proc(1, None, "fam.exe", ProcCategory::App);
        head.has_window = true;
        head.cpu_pct = 0.0;
        let mut worker = proc(2, Some(1), "fam.exe", ProcCategory::App);
        worker.cpu_pct = 30.0;
        let mut other = proc(3, None, "other.exe", ProcCategory::App);
        other.has_window = true;
        other.cpu_pct = 10.0;
        let mut expanded = HashSet::new();
        expanded.insert(1u32);
        let rows = build_display_rows(
            &snap_of(vec![head, worker, other]),
            "",
            2,
            false,
            &expanded,
            &[false; 3],
        );
        let pids: Vec<u32> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Process(d) => Some(d.pid),
                _ => None,
            })
            .collect();
        // fam.exe family aggregates to 30 % → first block; its expanded
        // worker rides along; other.exe (10 %) follows. Headers are gone
        // in this view.
        assert_eq!(pids, vec![1, 2, 3]);
        let DisplayRow::Process(head_row) = &rows[0] else {
            panic!("first row must be a process row");
        };
        assert!(head_row.children, "family head stays expandable");
    }

    #[test]
    fn terminated_pseudo_row_carries_localized_tooltip() {
        let mut term = pseudo(
            u32::MAX - 1,
            "Terminated Processes",
            ProcCategory::Background,
        );
        term.description = Some("rustc.exe \u{d7}2".into());
        term.cpu_pct = 9.0;
        // Name sort → grouped sections (rows_in_group needs headers).
        let rows = build_display_rows(
            &snap_of(vec![term]),
            "",
            0,
            true,
            &HashSet::new(),
            &[false; 3],
        );
        let bg = rows_in_group(&rows, 1);
        assert_eq!(bg.len(), 1);
        let tip = bg[0].tooltip.as_deref().expect("tooltip set");
        assert!(
            tip.contains("rustc.exe \u{d7}2"),
            "tooltip names the image: {tip}"
        );

        // Real processes never get the pseudo-row tooltip.
        let mut real = proc(9, None, "bg.exe", ProcCategory::Background);
        real.description = Some("whatever".into());
        let rows = build_display_rows(
            &snap_of(vec![real]),
            "",
            0,
            true,
            &HashSet::new(),
            &[false; 3],
        );
        assert!(rows_in_group(&rows, 1)[0].tooltip.is_none());
    }
}

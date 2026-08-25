//! Processes tab: Apps/Hintergrundprozesse groups, expandable parent→child
//! trees with aggregated values, blue heat-mapped resource columns and the
//! aggregate header — mirroring Win11 Task Manager.

use eframe::egui;
use std::collections::{HashMap, HashSet};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::{ProcStatus, ProcessEntry, Snapshot};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::theme;
use crate::widgets::tablekit::{Aggregates, TmColumn};

fn columns() -> Vec<TmColumn> {
    vec![
        TmColumn::text("name", i18n::tr(K::ColName), 0.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 190.0),
        TmColumn::num("cpu", i18n::tr(K::ColCpu), 110.0),
        TmColumn::num("mem", i18n::tr(K::ColMemory), 110.0),
        TmColumn::num("disk", i18n::tr(K::ColDisk), 110.0),
        TmColumn::num("net", i18n::tr(K::ColNetwork), 110.0),
    ]
}

#[derive(Default)]
pub struct State {
    pub sort_col: usize,
    pub ascending: bool,
    /// Expanded parent pids.
    pub expanded: HashSet<u32>,
    /// Expanded user rows (Benutzer tab reuses the same state).
    pub expanded_users: HashSet<u32>,
    /// Collapsed group headers [Apps, Hintergrundprozesse].
    pub group_collapsed: [bool; 2],
    cache: Option<Cache>,
}

impl State {
    /// TM default: sorted by name ascending.
    pub fn new() -> Self {
        Self {
            ascending: true,
            ..Default::default()
        }
    }
}

struct Cache {
    key: (u64, String, usize, bool),
    rows: Vec<Row>,
}

pub struct Row {
    pub pid: u32,
    pub depth: usize,
    pub name: String,
    pub icon_path: Option<String>,
    pub children: bool,
    pub group: usize,
    /// cpu, mem bytes, disk bps, net bps (aggregated over the subtree).
    pub values: [f64; 4],
    pub suspended: bool,
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
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Leaf,
                i18n::tr(K::EfficiencyMode),
                caps.efficiency_mode && app.selected_pid.is_some(),
            ) && let Some(pid) = app.selected_pid
            {
                let on = !app.efficiency_pids.contains(&pid);
                match app.actions.set_efficiency_mode(pid, on) {
                    Ok(()) => {
                        if on {
                            app.efficiency_pids.insert(pid);
                        } else {
                            app.efficiency_pids.remove(&pid);
                        }
                    }
                    Err(e) => app
                        .shared
                        .toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
                }
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Close,
                i18n::tr(K::EndTask),
                app.selected_pid.is_some(),
            ) {
                app.end_selected();
            }
        },
        |app, ui| {
            if ui.button(i18n::tr(K::ExpandAll)).clicked() {
                if let Some(snap) = app.latest_snapshot() {
                    for p in &snap.processes {
                        app.processes_state.expanded.insert(p.pid);
                    }
                }
                ui.close();
            }
            if ui.button(i18n::tr(K::CollapseAll)).clicked() {
                app.processes_state.expanded.clear();
                ui.close();
            }
        },
    );

    let mut table = app.make_table("processes", columns(), 340.0);

    // Rebuild the row model only when the snapshot/search/sort changes.
    let key = (
        snap.timestamp_ms,
        app.search.clone(),
        app.processes_state.sort_col,
        app.processes_state.ascending,
    );
    let mut cache = app.processes_state.cache.take();
    let cache_stale = cache.as_ref().is_none_or(|c| c.key != key);
    if cache_stale {
        cache = Some(Cache {
            key: key.clone(),
            rows: build_rows(&snap, &key.1, key.2, key.3, &app.processes_state.expanded),
        });
    }
    let rows = &cache.as_ref().expect("cache").rows;

    let agg = Aggregates::from_snapshot(&snap);
    let aggs = agg.strings();

    let avail = crate::widgets::tablekit::table_avail(ui);
    if let Some(col) = table.header(
        ui,
        &pal,
        avail,
        Some((app.processes_state.sort_col, app.processes_state.ascending)),
        Some(&aggs),
    ) {
        if app.processes_state.sort_col == col {
            app.processes_state.ascending = !app.processes_state.ascending;
        } else {
            app.processes_state.sort_col = col;
            // Numeric columns default descending, name ascending.
            app.processes_state.ascending = col == 0 || !table.cols[col].numeric;
        }
    }

    egui::ScrollArea::vertical()
        .id_salt("proc-table")
        .auto_shrink(false)
        .show(ui, |ui| {
            let searching = !app.search.trim().is_empty();
            let maxima = column_maxima(rows);

            if searching {
                for row in rows.iter().filter(|r| r.group < 2) {
                    row_ui(app, ui, &pal, &table, avail, row, &maxima);
                }
            } else {
                for gi in 0..2usize {
                    let label = if gi == 0 {
                        i18n::tr(K::GroupApps)
                    } else {
                        i18n::tr(K::GroupBackground)
                    };
                    let total = if gi == 0 {
                        rows.iter().filter(|r| r.group == 0 && r.depth == 0).count()
                    } else {
                        rows.iter().filter(|r| r.group == 1).count()
                    };
                    group_header(app, ui, &pal, label, total);
                    if app.processes_state.group_collapsed[gi] {
                        continue;
                    }
                    for row in rows.iter().filter(|r| r.group == gi) {
                        row_ui(app, ui, &pal, &table, avail, row, &maxima);
                    }
                    ui.add_space(6.0);
                }
            }
            ui.add_space(12.0);
        });
    app.persist_table(&mut table);
    app.processes_state.cache = cache;
}

/// Collapsible group header ("Apps (5)").
fn group_header(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &theme::Palette,
    label: &str,
    count: usize,
) {
    let gi = if label == i18n::tr(K::GroupApps) { 0 } else { 1 };
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, egui::Color32::from_white_alpha(8));
    }
    ui.painter().text(
        egui::Pos2::new(rect.left() + 18.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{label} ({count})"),
        egui::FontId::proportional(15.0),
        pal.text,
    );
    if resp.clicked() {
        app.processes_state.group_collapsed[gi] = !app.processes_state.group_collapsed[gi];
    }
}

#[allow(clippy::too_many_arguments)]
fn row_ui(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &theme::Palette,
    table: &crate::widgets::tablekit::TmTable,
    avail: f32,
    row: &Row,
    maxima: &[f64; 4],
) {
    let selected = app.selected_pid == Some(row.pid);
    let (rect, resp) = table.row(ui, pal, avail, selected);

    // Chevron + icon + name.
    let toggled = row.children
        && table.chevron(
            ui,
            rect,
            app.processes_state.expanded.contains(&row.pid),
            true,
            pal,
        );
    if toggled {
        if app.processes_state.expanded.contains(&row.pid) {
            app.processes_state.expanded.remove(&row.pid);
        } else {
            app.processes_state.expanded.insert(row.pid);
        }
    }

    let tex = row
        .icon_path
        .as_ref()
        .and_then(|p| app.shared.icons.get(ui.ctx(), app.actions.as_ref(), p, 6));
    table.icon_cell(
        ui,
        rect.translate(egui::vec2(row.depth as f32 * 22.0, 0.0)),
        tex.as_ref(),
        pal.accent,
    );
    let name_rect = table.col_rect(0, avail, rect);
    ui.painter().text(
        egui::Pos2::new(
            name_rect.left() + 58.0 + row.depth as f32 * 22.0,
            rect.center().y,
        ),
        egui::Align2::LEFT_CENTER,
        &row.name,
        egui::FontId::proportional(12.5),
        pal.text,
    );

    // Status text + efficiency leaf.
    if row.suspended {
        table.text_cell(ui, avail, rect, 1, i18n::tr(K::StSuspended), pal, false);
    }
    if app.efficiency_pids.contains(&row.pid) {
        let status_rect = table.col_rect(1, avail, rect);
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

    // Heat cells.
    let texts = [
        format::format_pct_cell(row.values[0].min(100.0) as f32),
        format::format_mb(row.values[1] as u64),
        format::format_rate_mb(row.values[2]),
        format::format_mbit(row.values[3]),
    ];
    let intensity = [
        (row.values[0].min(100.0) / 100.0) as f32,
        (row.values[1] / maxima[1].max(1.0)) as f32,
        (row.values[2] / maxima[2].max(1.0)) as f32,
        (row.values[3] / maxima[3].max(1.0)) as f32,
    ];
    let active = row.values.iter().any(|&v| v > 0.0);
    let cells: Vec<(f32, String)> = intensity
        .iter()
        .zip(texts.iter())
        .map(|(t, s)| (*t, s.clone()))
        .collect();
    table.heat_cells(ui, pal, avail, rect, 2, &cells, active);

    if resp.clicked() {
        app.selected_pid = Some(row.pid);
    }
    resp.context_menu(|ui| context_menu(app, ui, row));
}

fn context_menu(app: &mut TaskManApp, ui: &mut egui::Ui, row: &Row) {
    ui.set_min_width(210.0);
    if ui.button(i18n::tr(K::EndTask)).clicked() {
        match app.actions.kill_process(row.pid, false) {
            Ok(()) => app
                .shared
                .toast(i18n::trf(K::NameEndedToast, &[&row.name])),
            Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
        }
        ui.close();
    }
    #[cfg(target_os = "windows")]
    if ui.button(i18n::tr(K::EndTree)).clicked() {
        match app.actions.kill_process(row.pid, true) {
            Ok(()) => app
                .shared
                .toast(i18n::trf(K::TreeOfEndedToast, &[&row.name])),
            Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
        }
        ui.close();
    }
    ui.separator();
    if ui.button(i18n::tr(K::EfficiencyMode)).clicked() {
        let on = !app.efficiency_pids.contains(&row.pid);
        match app.actions.set_efficiency_mode(row.pid, on) {
            Ok(()) => {
                if on {
                    app.efficiency_pids.insert(row.pid);
                } else {
                    app.efficiency_pids.remove(&row.pid);
                }
            }
            Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
        }
        ui.close();
    }
    if ui.button(i18n::tr(K::GoToDetails)).clicked() {
        app.details_state.filter = row.name.clone();
        app.tab = crate::app::Tab::Details;
        ui.close();
    }
    #[cfg(target_os = "windows")]
    {
        if ui.button(i18n::tr(K::GoToServices)).clicked() {
            app.goto_services_for_pid(row.pid);
            ui.close();
        }
    }
    ui.separator();
    if ui.button(i18n::tr(K::OpenFileLocation)).clicked() {
        match row.icon_path.as_deref() {
            Some(path) => match app.actions.open_file_location(path) {
                Ok(()) => {}
                Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
            },
            None => app.shared.toast(i18n::tr(K::NoFileForProcess)),
        }
        ui.close();
    }
    if ui.button(i18n::tr(K::Properties)).clicked() {
        app.proc_props = Some(row.pid);
        ui.close();
    }
    let _ = row.suspended;
}

// ---------------------------------------------------------------- row model

fn column_maxima(rows: &[Row]) -> [f64; 4] {
    let mut m = [1.0f64; 4];
    for r in rows {
        for (m_i, v) in m.iter_mut().zip(r.values) {
            *m_i = (*m_i).max(v);
        }
    }
    m
}

/// Build the visible row list: apps tree + background tree, sorted.
fn build_rows(
    snap: &Snapshot,
    search: &str,
    sort_col: usize,
    ascending: bool,
    expanded: &HashSet<u32>,
) -> Vec<Row> {
    let q = search.trim().to_lowercase();
    let all: Vec<&ProcessEntry> = snap.processes.iter().collect();
    // Subtree aggregates computed once per process (used for sorting + rows).
    let all_children = children_map(&all);
    let subtree = subtree_values(&all, &all_children);

    // The sampler marks whole app-subtrees as `App`; the topmost process of
    // each subtree is the root (TM's "Steam (2)" style grouping).
    let app_pids: HashSet<u32> = snap
        .processes
        .iter()
        .filter(|p| p.category == tm_core::model::ProcCategory::App)
        .map(|p| p.pid)
        .collect();

    let mut rows = Vec::new();

    // ---- Apps group (tree by ppid, aggregated parent values).
    let app_list: Vec<&ProcessEntry> = snap
        .processes
        .iter()
        .filter(|p| app_pids.contains(&p.pid))
        .collect();
    let app_children = children_map(&app_list);
    let roots: Vec<&ProcessEntry> = app_list
        .iter()
        .copied()
        .filter(|p| p.app_root || p.ppid.is_none_or(|pp| !app_pids.contains(&pp)))
        .collect();
    emit_tree(
        &mut rows,
        &roots,
        &app_children,
        &subtree,
        0,
        sort_col,
        ascending,
        expanded,
    );

    // ---- Background group (remaining processes, also tree-shaped).
    let bg_list: Vec<&ProcessEntry> = snap
        .processes
        .iter()
        .filter(|p| !app_pids.contains(&p.pid))
        .collect();
    let bg_children = children_map(&bg_list);
    let bg_roots: Vec<&ProcessEntry> = bg_list
        .iter()
        .copied()
        .filter(|p| p.ppid.is_none_or(|pp| !bg_children.contains_key(&pp)))
        .collect();
    emit_tree(
        &mut rows,
        &bg_roots,
        &bg_children,
        &subtree,
        1,
        sort_col,
        ascending,
        expanded,
    );

    if !q.is_empty() {
        // Search: flat list of direct name matches only.
        rows.retain(|r| r.name.to_lowercase().contains(&q));
    }
    rows
}

fn children_map<'a>(list: &[&'a ProcessEntry]) -> HashMap<u32, Vec<&'a ProcessEntry>> {
    let pids: HashSet<u32> = list.iter().map(|p| p.pid).collect();
    let mut m: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
    for p in list {
        if let Some(ppid) = p.ppid
            && ppid != p.pid
            && pids.contains(&ppid)
        {
            m.entry(ppid).or_default().push(p);
        }
    }
    m
}
#[allow(clippy::too_many_arguments)]
fn emit_tree<'a>(
    out: &mut Vec<Row>,
    roots: &[&'a ProcessEntry],
    children: &HashMap<u32, Vec<&'a ProcessEntry>>,
    subtree: &HashMap<u32, [f64; 4]>,
    group: usize,
    sort_col: usize,
    ascending: bool,
    expanded: &HashSet<u32>,
) {
    let mut roots: Vec<&ProcessEntry> = roots.to_vec();
    sort_entries(&mut roots, sort_col, ascending, subtree);
    for root in roots {
        let n_children = children.get(&root.pid).map_or(0, |v| v.len());
        let name = if n_children > 0 {
            format!("{} ({})", root.shown_name(), n_children + 1)
        } else {
            root.shown_name().to_string()
        };
        out.push(make_row(
            root,
            subtree.get(&root.pid).unwrap_or(&[0.0; 4]),
            group,
            0,
            n_children > 0,
            name,
        ));
        if n_children > 0 && !expanded.contains(&root.pid) {
            continue;
        }
        if let Some(kids) = children.get(&root.pid) {
            let mut kids: Vec<&ProcessEntry> = kids.to_vec();
            sort_entries(&mut kids, sort_col, ascending, subtree);
            for k in kids {
                let kn = children.get(&k.pid).map_or(0, |v| v.len());
                let kname = if kn > 0 {
                    format!("{} ({})", k.shown_name(), kn + 1)
                } else {
                    k.shown_name().to_string()
                };
                out.push(make_row(
                    k,
                    subtree.get(&k.pid).unwrap_or(&[0.0; 4]),
                    group,
                    1,
                    kn > 0,
                    kname,
                ));
            }
        }
    }
}

fn make_row(
    p: &ProcessEntry,
    values: &[f64; 4],
    group: usize,
    depth: usize,
    children: bool,
    name: String,
) -> Row {
    Row {
        pid: p.pid,
        depth,
        name,
        icon_path: p
            .exe_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        children,
        group,
        values: *values,
        suspended: p.status == ProcStatus::Suspended,
    }
}

/// Aggregate cpu/mem/disk/net over each process and all its descendants.
///
/// Iterative with memoization (O(n)) and a cycle guard — a malformed ppid
/// loop can never recurse to a stack overflow. Returns an empty map when the
/// children graph is empty.
fn subtree_values(
    all: &[&ProcessEntry],
    children: &HashMap<u32, Vec<&ProcessEntry>>,
) -> HashMap<u32, [f64; 4]> {
    let own = |p: &ProcessEntry| {
        [
            p.cpu_pct as f64,
            p.mem_bytes as f64,
            p.disk_read_bps + p.disk_write_bps,
            p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0),
        ]
    };
    let mut out: HashMap<u32, [f64; 4]> = HashMap::with_capacity(all.len());
    if children.is_empty() {
        for p in all {
            out.insert(p.pid, own(p));
        }
        return out;
    }
    // Post-order traversal state: 0 = unseen, 1 = in progress, 2 = done.
    let by_pid: HashMap<u32, &ProcessEntry> = all.iter().map(|p| (p.pid, *p)).collect();
    let mut state: HashMap<u32, u8> = HashMap::with_capacity(all.len());
    for root in all {
        let mut stack: Vec<(u32, bool)> = vec![(root.pid, false)];
        while let Some((pid, expanded)) = stack.pop() {
            if expanded {
                let Some(p) = by_pid.get(&pid) else { continue };
                let mut v = own(p);
                if let Some(kids) = children.get(&pid) {
                    for k in kids {
                        if let Some(kv) = out.get(&k.pid) {
                            for i in 0..4 {
                                v[i] += kv[i];
                            }
                        }
                    }
                }
                out.insert(pid, v);
                continue;
            }
            match state.get(&pid) {
                Some(2) => continue,
                Some(1) => {
                    // Cycle: treat the node as its own subtree root.
                    continue;
                }
                _ => {}
            }
            state.insert(pid, 1);
            stack.push((pid, true));
            if let Some(kids) = children.get(&pid) {
                for k in kids {
                    if !matches!(state.get(&k.pid), Some(1 | 2)) {
                        stack.push((k.pid, false));
                    }
                }
            }
        }
        // Any node still missing (cycle members) gets its own values only.
        for p in all {
            out.entry(p.pid).or_insert_with(|| own(p));
        }
    }
    out
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
            _ => a
                .shown_name()
                .to_lowercase()
                .cmp(&b.shown_name().to_lowercase()),
        };
        if asc { o } else { o.reverse() }
    });
}

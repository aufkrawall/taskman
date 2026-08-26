//! Details tab: dense flat process table with stable column ids.
//!
//! Sorting is keyed by [`ColumnId`], never by positional index — adding or
//! reordering a column cannot silently break comparator mappings (§12.1).
//! Elevation and UAC virtualization come from real token queries; missing
//! telemetry renders as "Unknown", never fabricated values. GPU engine shows
//! the dominant engine label, not a percentage.

use eframe::egui;
use std::cmp::Ordering as CmpOrdering;
use std::collections::BTreeSet;
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::{PriorityClass, ProcStatus, ProcessEntry, UacVirtualization};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::search;
use crate::theme;
use crate::widgets::tablekit::{self, TmColumn};

/// Stable detail-column identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ColumnId {
    Name,
    Pid,
    Status,
    User,
    Cpu,
    Memory,
    Platform,
    Elevated,
    Uac,
    GpuUtil,
    GpuEngine,
}

impl ColumnId {
    /// Typed comparator for THIS column's own field — the single source of
    /// truth for both sorting behavior and the every-column-sorts-its-field
    /// regression test.
    pub fn compare(self, a: &ProcessEntry, b: &ProcessEntry) -> CmpOrdering {
        match self {
            ColumnId::Name => cmp_ignore_case(a.shown_name(), b.shown_name()),
            ColumnId::Pid => a.pid.cmp(&b.pid),
            ColumnId::Status => status_rank(a.status).cmp(&status_rank(b.status)),
            ColumnId::User => cmp_option_str(a.user.as_deref(), b.user.as_deref()),
            ColumnId::Cpu => a
                .cpu_pct
                .partial_cmp(&b.cpu_pct)
                .unwrap_or(CmpOrdering::Equal),
            ColumnId::Memory => a.mem_bytes.cmp(&b.mem_bytes),
            ColumnId::Platform => a.wow64.cmp(&b.wow64),
            ColumnId::Elevated => a.elevated.cmp(&b.elevated),
            // UAC virtualization must sort by its own value, never priority.
            ColumnId::Uac => uac_rank(a.uac_virtualization).cmp(&uac_rank(b.uac_virtualization)),
            ColumnId::GpuUtil => a
                .gpu_util_pct
                .partial_cmp(&b.gpu_util_pct)
                .unwrap_or(CmpOrdering::Equal),
            // Engine labels sort lexically; processes without an engine go last.
            ColumnId::GpuEngine => {
                cmp_option_str(a.gpu_engine_label.as_deref(), b.gpu_engine_label.as_deref())
            }
        }
    }
}

fn status_rank(s: ProcStatus) -> u8 {
    match s {
        ProcStatus::Running => 0,
        ProcStatus::Suspended => 1,
        ProcStatus::NotResponding => 2,
    }
}

fn uac_rank(v: Option<UacVirtualization>) -> u8 {
    match v {
        None => 3,
        Some(UacVirtualization::Disabled) => 0,
        Some(UacVirtualization::Enabled) => 1,
        Some(UacVirtualization::NotAllowed) => 2,
        Some(UacVirtualization::Unknown) => 3,
    }
}

fn cmp_ignore_case(a: &str, b: &str) -> CmpOrdering {
    let mut ai = a.chars().flat_map(char::to_lowercase);
    let mut bi = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => match x.cmp(&y) {
                CmpOrdering::Equal => continue,
                other => return other,
            },
            (None, None) => return CmpOrdering::Equal,
            (None, Some(_)) => return CmpOrdering::Less,
            (Some(_), None) => return CmpOrdering::Greater,
        }
    }
}

fn cmp_option_str(a: Option<&str>, b: Option<&str>) -> CmpOrdering {
    match (a, b) {
        (Some(x), Some(y)) => cmp_ignore_case(x, y),
        (Some(_), None) => CmpOrdering::Less,
        (None, Some(_)) => CmpOrdering::Greater,
        (None, None) => CmpOrdering::Equal,
    }
}

#[derive(Clone, Copy)]
struct ColSpec {
    cid: ColumnId,
    col: fn() -> TmColumn,
}

impl ColSpec {
    /// Localized label of this column (Select-columns dialog).
    fn label(self) -> &'static str {
        match self.cid {
            ColumnId::Name => i18n::tr(K::ColName),
            ColumnId::Pid => i18n::tr(K::ColPid),
            ColumnId::Status => i18n::tr(K::ColStatus),
            ColumnId::User => i18n::tr(K::ColUsername),
            ColumnId::Cpu => i18n::tr(K::ColCpu),
            ColumnId::Memory => i18n::tr(K::ColMemory),
            ColumnId::Platform => i18n::tr(K::ColPlatform),
            ColumnId::Elevated => i18n::tr(K::ColElevated),
            ColumnId::Uac => i18n::tr(K::ColUac),
            ColumnId::GpuUtil => i18n::tr(K::ColGpu),
            ColumnId::GpuEngine => i18n::tr(K::ColGpuEngine),
        }
    }
}

/// The registered catalog. Adding a column here automatically participates
/// in sorting, persistence and rendering order.
const COLUMNS: &[ColSpec] = &[
    ColSpec {
        cid: ColumnId::Name,
        // Audit P0.1 parity: configured width instead of viewport fill.
        col: || TmColumn::text("name", i18n::tr(K::ColName), 340.0),
    },
    ColSpec {
        cid: ColumnId::Pid,
        col: || TmColumn::text("pid", i18n::tr(K::ColPid), 90.0),
    },
    ColSpec {
        cid: ColumnId::Status,
        col: || TmColumn::text("status", i18n::tr(K::ColStatus), 150.0),
    },
    ColSpec {
        cid: ColumnId::User,
        col: || TmColumn::text("user", i18n::tr(K::ColUsername), 120.0),
    },
    ColSpec {
        cid: ColumnId::Cpu,
        col: || TmColumn::num("cpu", i18n::tr(K::ColCpu), 64.0),
    },
    ColSpec {
        cid: ColumnId::Memory,
        col: || TmColumn::num("mem", i18n::tr(K::ColMemory), 130.0),
    },
    ColSpec {
        cid: ColumnId::Platform,
        col: || TmColumn::text("platform", i18n::tr(K::ColPlatform), 90.0),
    },
    ColSpec {
        cid: ColumnId::Elevated,
        col: || TmColumn::text("elevated", i18n::tr(K::ColElevated), 110.0),
    },
    ColSpec {
        cid: ColumnId::Uac,
        col: || TmColumn::text("uac", i18n::tr(K::ColUac), 160.0),
    },
    // Separate GPU utilization (%) and GPU engine label columns (§12.4).
    ColSpec {
        cid: ColumnId::GpuUtil,
        col: || TmColumn::num("gpu", i18n::tr(K::ColGpu), 80.0),
    },
    ColSpec {
        cid: ColumnId::GpuEngine,
        col: || TmColumn::text("gpuengine", i18n::tr(K::ColGpuEngine), 170.0),
    },
];

pub struct State {
    /// Sorted column expressed by id (stable across catalog changes).
    pub sort_col: ColumnId,
    pub ascending: bool,
    pub filter: String,
    pub cache: Option<Cache>,
    /// Which registered columns are currently displayed. This set IS the
    /// single source of truth: telemetry demand derives from it (audit P0.3)
    /// — no separate boolean can drift from what the table actually shows.
    pub visible: BTreeSet<ColumnId>,
    /// Select-columns dialog open flag.
    pub select_columns_open: bool,
}

impl State {
    /// True when any VISIBLE column requires GPU-family telemetry. Deriving
    /// demand from the real column state closes the old mismatch where GPU
    /// columns could render while their sampling was never requested.
    pub fn requires_gpu_telemetry(&self) -> bool {
        self.visible
            .iter()
            .any(|cid| matches!(cid, ColumnId::GpuUtil | ColumnId::GpuEngine))
    }

    /// Whether this column id should be painted this frame.
    pub fn is_visible(&self, cid: ColumnId) -> bool {
        self.visible.contains(&cid)
    }

    /// Toggle one column's visibility (Select-columns dialog).
    pub fn set_visible(&mut self, cid: ColumnId, on: bool) {
        // Never allow hiding EVERY column.
        if !on && self.visible.len() <= 1 && self.visible.contains(&cid) {
            return;
        }
        if on {
            self.visible.insert(cid);
        } else {
            self.visible.remove(&cid);
        }
    }

    /// Language generation participates in the row-cache key so labels
    /// rebuild on live language switches.
    pub fn lang_generation(&self) -> u64 {
        i18n::lang() as u64
    }

    /// Drop the display cache (F5 / language change).
    pub fn invalidate(&mut self) {
        self.cache = None;
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            sort_col: ColumnId::Name,
            ascending: true,
            filter: String::new(),
            cache: None,
            visible: COLUMNS.iter().map(|c| c.cid).collect(),
            select_columns_open: false,
        }
    }
}

pub struct Cache {
    pub key: (u64, u64, String, ColumnId, bool),
    pub rows: Vec<Row>,
}

pub struct Row {
    pub gpu_util_s: String,
    pub pid: u32,
    /// Process identity guard, carried into destructive actions.
    pub start_epoch_s: Option<i64>,
    pub name: String,
    pub icon_path: Option<String>,
    pub pid_s: String,
    pub status: &'static str,
    pub user: String,
    pub cpu_s: String,
    pub mem_s: String,
    pub platform: &'static str,
    pub elevated: &'static str,
    pub uac: &'static str,
    pub gpu_engine_s: String,
}

impl Row {
    /// The display string for a registered column id — the mapping painted
    /// by the dynamic visible-column renderer.
    pub fn field(&self, cid: ColumnId) -> &str {
        match cid {
            ColumnId::Name => &self.name,
            ColumnId::Pid => &self.pid_s,
            ColumnId::Status => self.status,
            ColumnId::User => &self.user,
            ColumnId::Cpu => &self.cpu_s,
            ColumnId::Memory => &self.mem_s,
            ColumnId::Platform => self.platform,
            ColumnId::Elevated => self.elevated,
            ColumnId::Uac => self.uac,
            ColumnId::GpuUtil => &self.gpu_util_s,
            ColumnId::GpuEngine => &self.gpu_engine_s,
        }
    }
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let Some(snap) = app.latest_snapshot() else {
        ui.centered_and_justified(|ui| ui.label(i18n::tr(K::GatheringData)));
        return;
    };

    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app: &mut TaskManApp, ui| {
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Close,
                i18n::tr(K::EndTask),
                app.selected_process.is_some(),
            ) {
                let ctx = ui.ctx().clone();
                app.end_selected(&ctx);
            }
        },
        |app, ui| {
            if ui.button(i18n::tr(K::RefreshNow)).clicked() {
                app.refresh_all();
                ui.close();
            }
            // Select columns (audit P0.3/§14 groundwork): visibility set
            // doubles as the telemetry-demand source, so the dialog and the
            // sampling engine can never disagree.
            if ui.button(i18n::tr(K::SelectColumns)).clicked() {
                app.details_state.select_columns_open = true;
                ui.close();
            }
        },
    );

    // Live search filter from the details tab itself.
    if !app.details_state.filter.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(
                egui::RichText::new(format!("\"{}\"", app.details_state.filter))
                    .size(13.0)
                    .color(pal.accent),
            );
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("✕").size(12.0).color(pal.text_dim))
                        .frame(false),
                )
                .clicked()
            {
                app.details_state.filter.clear();
            }
        });
    }

    // Consume a pending cross-tab navigation: select the EXACT process
    // identity and scroll it into view (§11.5).
    if let Some(focus) = app.pending_details_focus.take() {
        app.selected_process = Some(focus.0.clone());
        app.search.clear();
        app.details_state.filter.clear();
        app.scroll_to_pid = Some(focus.0.pid);
    }

    // Visible column list for THIS frame (hidden columns are skipped in both
    // header and body so indices always agree).
    let visible_cols: Vec<ColSpec> = COLUMNS
        .iter()
        .copied()
        .filter(|c| app.details_state.is_visible(c.cid))
        .collect();
    let cols_rendered: Vec<TmColumn> = visible_cols.iter().map(|c| (c.col)()).collect();

    let mut table = app.make_table("details", cols_rendered);

    // Rebuild the row model only when snapshot/search/sort/lang changes.
    let key = (
        snap.timestamp_ms,
        app.details_state.lang_generation(),
        effective_search(app),
        app.details_state.sort_col,
        app.details_state.ascending,
    );
    let mut cache = app.details_state.cache.take();
    let stale = cache.as_ref().is_none_or(|c| c.key != key);
    if stale {
        cache = Some(Cache {
            key: key.clone(),
            rows: build_rows(&snap, &key.2, key.3, key.4),
        });
    }
    let rows = &cache.as_ref().expect("cache").rows;

    let avail = tablekit::table_avail(ui);
    let sorted_pos = visible_cols
        .iter()
        .position(|c| c.cid == app.details_state.sort_col);
    let clicked = tablekit::scrolled_rows(
        "details",
        ui,
        &pal,
        &mut table,
        avail,
        sorted_pos.map(|p| (p, app.details_state.ascending)),
        None,
        rows.len(),
        |ui, table, _avail, _content_w, range| {
            for i in range {
                let Some(row) = rows.get(i) else { continue };
                let selected = app
                    .selected_process
                    .as_ref()
                    .is_some_and(|sp| sp.pid == row.pid);
                let scroll_hint = app.scroll_to_pid.is_some_and(|p| p == row.pid);
                let (rect, resp) = table.row(ui, &pal, selected);
                if scroll_hint {
                    resp.scroll_to_me(Some(egui::Align::Center));
                    app.scroll_to_pid = None;
                }

                let tex_visible = visible_cols
                    .first()
                    .is_some_and(|c| c.cid == ColumnId::Name);
                if tex_visible {
                    let tex = row
                        .icon_path
                        .as_ref()
                        .and_then(|p| app.shared.icons.get(ui.ctx(), &app.actions, p, 6));
                    table.icon_cell(ui, rect, tex.as_ref(), pal.accent);
                    let name_rect = table.col_rect(0, rect);
                    ui.painter().text(
                        egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        &row.name,
                        egui::FontId::proportional(tablekit::FONT_ROW),
                        pal.text,
                    );
                }

                // Remaining VISIBLE columns paint by position; memory stays
                // right-aligned like TM.
                let first_text_col = if tex_visible { 1 } else { 0 };
                for (pos, spec) in visible_cols.iter().enumerate().skip(first_text_col) {
                    let cid = spec.cid;
                    let text = row.field(cid);
                    if cid == ColumnId::Memory {
                        let mem_rect = table.col_rect(pos, rect);
                        ui.painter().text(
                            egui::Pos2::new(mem_rect.right() - 10.0, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            text,
                            egui::FontId::proportional(tablekit::FONT_ROW),
                            pal.text,
                        );
                    } else {
                        table.text_cell(ui, rect, pos, text, &pal, false);
                    }
                }

                if resp.clicked() {
                    app.selected_process = Some(crate::app::ProcessIdentity {
                        pid: row.pid,
                        start_epoch_s: row.start_epoch_s,
                    });
                }
                resp.context_menu(|ui| {
                    // Re-fetch the live entry for the context menu actions.
                    if let Some(p) = snap.process(row.pid) {
                        context_menu(app, ui, p);
                    }
                });
            }
        },
    );
    if let Some(display_idx) = clicked
        && let Some(spec) = visible_cols.get(display_idx)
    {
        let cid = spec.cid;
        if app.details_state.sort_col == cid {
            app.details_state.ascending = !app.details_state.ascending;
        } else {
            app.details_state.sort_col = cid;
            app.details_state.ascending = !cid_is_numeric(cid);
        }
    }
    app.persist_table(&table);
    app.details_state.cache = cache;

    select_columns_dialog(app, &ctx_from(ui), &pal);
}

/// Small helper: clone the context out of the frame UI.
fn ctx_from(ui: &egui::Ui) -> egui::Context {
    ui.ctx().clone()
}

/// Select-columns dialog (audit P0.3 / §14 groundwork): one checkbox per
/// registered column. The resulting visibility set IS the source of truth
/// for both rendering and telemetry demand, so the two can never drift.
fn select_columns_dialog(app: &mut TaskManApp, ctx: &egui::Context, pal: &theme::Palette) {
    if !app.details_state.select_columns_open {
        return;
    }
    let mut open = true;
    egui::Window::new(i18n::tr(K::SelectColumns))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_min_width(260.0);
            egui::ScrollArea::vertical()
                .max_height(340.0)
                .show(ui, |ui| {
                    for spec in COLUMNS.iter().copied() {
                        let mut on = app.details_state.is_visible(spec.cid);
                        if crate::widgets::controls::checkbox(ui, &mut on, spec.label(), pal)
                            .changed()
                        {
                            app.details_state.set_visible(spec.cid, on);
                        }
                    }
                });
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(i18n::tr(K::Close)).clicked() {
                    app.details_state.select_columns_open = false;
                }
            });
        });
    if !open {
        app.details_state.select_columns_open = false;
    }
}

fn cid_is_numeric(cid: ColumnId) -> bool {
    matches!(cid, ColumnId::Pid | ColumnId::Cpu | ColumnId::Memory)
}

fn effective_search(app: &TaskManApp) -> String {
    if !app.details_state.filter.is_empty() {
        app.details_state.filter.clone()
    } else {
        app.search.clone()
    }
}

fn build_rows(
    snap: &tm_core::model::Snapshot,
    raw_search: &str,
    sort_col: ColumnId,
    ascending: bool,
) -> Vec<Row> {
    // Shared matcher (audit §5): binary name, display name, PID and
    // publisher/company — identical semantics to the Processes tab.
    let q = search::Query::new(raw_search);
    let mut list: Vec<&ProcessEntry> = snap
        .processes
        .iter()
        .filter(|p| q.matches_process(p))
        .collect();

    list.sort_by(|a, b| {
        let o = sort_col.compare(a, b);
        if ascending { o } else { o.reverse() }
    });

    list.into_iter()
        .map(|p| {
            let status = match p.status {
                ProcStatus::Running => "",
                ProcStatus::Suspended => i18n::tr(K::StSuspended),
                ProcStatus::NotResponding => i18n::tr(K::StNotResponding),
            };
            let platform = match p.wow64 {
                Some(true) => i18n::tr(K::Bit32),
                _ => i18n::tr(K::Bit64),
            };
            // Real token elevation when queried; SYSTEM/Idle fallback stays
            // because their tokens cannot be opened at all.
            let elevated = match p.elevated {
                Some(true) => i18n::tr(K::Yes),
                Some(false) => i18n::tr(K::No),
                None if p.user.as_deref() == Some("SYSTEM") || matches!(p.pid, 4 | 0) => {
                    i18n::tr(K::Yes)
                }
                None => i18n::tr(K::UacUnknown),
            };
            // Token virtualization state — never inferred from user/pid.
            let uac = match p.uac_virtualization {
                Some(UacVirtualization::Enabled) => i18n::tr(K::EnabledWord),
                Some(UacVirtualization::Disabled) => i18n::tr(K::DisabledWord),
                Some(UacVirtualization::NotAllowed) => i18n::tr(K::NotAllowed),
                _ => i18n::tr(K::UacUnknown),
            };
            Row {
                gpu_util_s: p
                    .gpu_util_pct
                    .map(format::format_pct_cell)
                    .unwrap_or_default(),
                pid: p.pid,
                start_epoch_s: p.start_epoch_s,
                name: p.shown_name().to_string(),
                icon_path: p
                    .exe_path
                    .as_ref()
                    .map(|x| x.to_string_lossy().into_owned()),
                pid_s: p.pid.to_string(),
                status,
                user: p.user.clone().unwrap_or_default(),
                cpu_s: format::format_cpu_detail(p.cpu_pct),
                mem_s: format::format_k(p.mem_bytes),
                platform,
                elevated,
                uac,
                // The engine column shows the dominant engine label — not a
                // percentage (§12.4).
                gpu_engine_s: p.gpu_engine_label.clone().unwrap_or_default(),
            }
        })
        .collect()
}

/// Context menu mirroring the Win11 TM Details tab.
pub fn context_menu(app: &mut TaskManApp, ui: &mut egui::Ui, p: &ProcessEntry) {
    let ctx = ui.ctx().clone();
    ui.set_min_width(230.0);
    ui.label(egui::RichText::new(p.shown_name()).strong().size(13.0));
    ui.separator();

    if ui.button(i18n::tr(K::CopyName)).clicked() {
        ui.ctx().copy_text(p.shown_name().to_string());
        app.shared.toast(i18n::tr(K::Copied));
        ui.close();
    }
    if ui.button(i18n::tr(K::EndTask)).clicked() {
        end_process(app, &ctx, p.pid, p.start_epoch_s, false, p.shown_name());
        ui.close();
    }
    #[cfg(target_os = "windows")]
    if ui.button(i18n::tr(K::EndTree)).clicked() {
        end_process(app, &ctx, p.pid, p.start_epoch_s, true, p.shown_name());
        ui.close();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = p.name.as_str();
    }

    ui.menu_button(i18n::tr(K::Priority), |ui| {
        for (cls, key) in [
            (PriorityClass::Realtime, K::PrioRealtime),
            (PriorityClass::High, K::PrioHigh),
            (PriorityClass::AboveNormal, K::PrioAboveNormal),
            (PriorityClass::Normal, K::PrioNormal),
            (PriorityClass::BelowNormal, K::PrioBelowNormal),
            (PriorityClass::Low, K::PrioLow),
        ] {
            if ui.button(i18n::tr(key)).clicked() {
                let actions = app.actions.clone();
                let pid = p.pid;
                let key_copy = key;
                let msg = move || i18n::trf(K::PrioritySetMsg, &[i18n::tr(key_copy)]);
                app.run_action(&ctx, msg, move || actions.set_priority(pid, cls));
                ui.close();
            }
        }
    });

    if ui.button(i18n::tr(K::SetAffinity)).clicked() {
        let mask = app.actions.get_affinity_mask(p.pid).unwrap_or(u64::MAX);
        app.affinity_dialog = Some((p.pid, mask));
        ui.close();
    }

    let suspended = p.status == ProcStatus::Suspended;
    if ui
        .button(if suspended {
            i18n::tr(K::ResumeProc)
        } else {
            i18n::tr(K::SuspendProc)
        })
        .clicked()
    {
        let actions = app.actions.clone();
        let pid = p.pid;
        let target_suspended = !suspended;
        app.run_action(&ctx, String::new, move || {
            actions.suspend_process(pid, target_suspended)
        });
        ui.close();
    }

    ui.separator();

    #[cfg(target_os = "windows")]
    {
        // Efficiency-mode state comes straight from the OS snapshot (audit
        // §8) — power_throttled as reported by the sampler.
        let eco_on = p.power_throttled == Some(true);
        if ui
            .button(if eco_on {
                i18n::tr(K::EfficiencyModeOff)
            } else {
                i18n::tr(K::EfficiencyModeOn)
            })
            .clicked()
        {
            crate::tabs::processes::toggle_efficiency_mode(
                app,
                &ctx,
                &crate::app::ProcessIdentity {
                    pid: p.pid,
                    start_epoch_s: p.start_epoch_s,
                },
            );
            ui.close();
        }
        // Jump to the service hosted by this process (svchost.exe etc.).
        if ui.button(i18n::tr(K::GoToServices)).clicked() {
            app.goto_services_for_pid(p.pid, &ctx);
            ui.close();
        }
    }

    if let Some(path) = p
        .exe_path
        .as_ref()
        .map(|x| x.to_string_lossy().into_owned())
    {
        if ui.button(i18n::tr(K::OpenFileLocation)).clicked() {
            let actions = app.actions.clone();
            let path2 = path.clone();
            app.run_action(&ctx, String::new, move || {
                actions.open_file_location(&path2)
            });
            ui.close();
        }
        if ui.button(i18n::tr(K::CreateDumpFile)).clicked() {
            create_dump(app, &ctx, p);
            ui.close();
        }
        if ui.button(i18n::tr(K::Properties)).clicked() {
            if let Err(e) = app.actions.open_properties(&path) {
                // Fall back to our own read-only dialog.
                tracing::debug!(error = %e, "shell properties failed; using built-in dialog");
                app.proc_props = Some(p.pid);
            }
            ui.close();
        }
    }
}

fn end_process(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    pid: u32,
    start: Option<i64>,
    tree: bool,
    name: &str,
) {
    // Identity validation against the live snapshot before a destructive op.
    let live_ok = app
        .latest_snapshot()
        .as_ref()
        .and_then(|s| s.process(pid))
        .is_none_or(|p| p.start_epoch_s.is_none() || p.start_epoch_s == start);
    if !live_ok {
        app.shared.toast(i18n::tr(K::ProcessExited));
        return;
    }
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

/// Ask where to save, then write a minidump on the action executor.
fn create_dump(app: &mut TaskManApp, ctx: &egui::Context, p: &ProcessEntry) {
    let default_name = format!("{}.dmp", p.shown_name());
    let Some(path) = rfd::FileDialog::new()
        .set_file_name(&default_name)
        .save_file()
    else {
        return;
    };
    let actions = app.actions.clone();
    let pid = p.pid;
    let path_s = path.clone();
    app.run_action(
        ctx,
        move || i18n::trf(K::DumpWrittenMsg, &[&path_s.to_string_lossy()]),
        move || actions.create_dump_file(pid, &path),
    );
}

/// Built-in read-only process properties dialog (fallback when the shell's
/// own Properties dialog is unavailable).
pub fn process_properties_dialog(app: &mut TaskManApp, ctx: &egui::Context) {
    let mut open = true;
    egui::Window::new(i18n::tr(K::Properties))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let pid = app.proc_props.unwrap_or(0);
            let entry = app.latest_snapshot().and_then(|s| s.process(pid).cloned());
            let Some(p) = entry else {
                ui.label(i18n::tr(K::ProcessExited));
                ui.add_space(8.0);
                if ui.button(i18n::tr(K::Close)).clicked() {
                    app.proc_props = None;
                }
                return;
            };
            ui.set_min_width(430.0);
            let status = match p.status {
                ProcStatus::Running => i18n::tr(K::StRunning),
                ProcStatus::Suspended => i18n::tr(K::StSuspended),
                ProcStatus::NotResponding => i18n::tr(K::StNotResponding),
            };
            let path = p
                .exe_path
                .as_ref()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_else(|| i18n::tr(K::NoFileForProcess).to_string());
            egui::Grid::new("proc-props")
                .num_columns(2)
                .spacing([14.0, 5.0])
                .show(ui, |ui| {
                    ui.weak(i18n::tr(K::ColName));
                    ui.label(p.shown_name());
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColPid));
                    ui.label(p.pid.to_string());
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColStatus));
                    ui.label(status);
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColUsername));
                    ui.label(p.user.clone().unwrap_or_default());
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColPlatform));
                    ui.label(match p.wow64 {
                        Some(true) => i18n::tr(K::Bit32),
                        _ => i18n::tr(K::Bit64),
                    });
                    ui.end_row();
                    ui.weak(i18n::tr(K::PropPath));
                    // Read-only display + explicit copy button: the old code
                    // edited `&mut path.clone()`, so edits vanished silently
                    // while looking editable.
                    egui::ScrollArea::horizontal()
                        .id_salt("proc-path-ro")
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(&path).size(13.0).monospace());
                        });
                    if ui.button(i18n::tr(K::CopyName)).clicked() {
                        ui.ctx().copy_text(path.clone());
                    }
                    ui.end_row();
                });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::OpenFileLocation)).clicked()
                    && let Err(e) = app.actions.open_file_location(&path)
                {
                    app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()]));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(i18n::tr(K::Close)).clicked() {
                        app.proc_props = None;
                    }
                });
            });
        });
    if !open {
        app.proc_props = None;
    }
}

/// Affinity checkbox dialog (up to 64 logical processors).
///
/// Known limitation (documented in known-debt): machines with multiple
/// processor groups (>64 logical CPUs) cannot express affinity beyond group
/// zero through this dialog; the platform API is u64-mask based and needs a
/// processor-group-aware redesign before it can be trusted there.
pub fn affinity_dialog(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    pid: u32,
    mask: u64,
    _pal: &theme::Palette,
) {
    let mut open = true;
    egui::Window::new(format!("{} {pid}", i18n::tr(K::AffinityTitle)))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let sys_mask = app.actions.system_affinity_mask().unwrap_or(u64::MAX);
            let mut new_mask = mask;
            let pal = theme::palette_ctx(ctx);
            egui::Grid::new("affinity")
                .num_columns(8)
                .spacing([6.0, 6.0])
                .show(ui, |ui| {
                    for cpu in 0..64usize {
                        let allowed = sys_mask & (1u64 << cpu) != 0;
                        let mut on = mask & (1u64 << cpu) != 0;
                        if crate::widgets::controls::checkbox_enabled(
                            ui,
                            &mut on,
                            &cpu.to_string(),
                            allowed,
                            &pal,
                        )
                        .changed()
                        {
                            if on {
                                new_mask |= 1u64 << cpu;
                            } else {
                                new_mask &= !(1u64 << cpu);
                            }
                        }
                        if (cpu + 1) % 8 == 0 {
                            ui.end_row();
                        }
                    }
                });
            if new_mask == 0 {
                ui.label(
                    egui::RichText::new(i18n::tr(K::AffinityWarn)).color(theme::DARK.heat_high),
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::Cancel)).clicked() {
                    app.affinity_dialog = None;
                }
                if ui
                    .add_enabled(new_mask != 0, egui::Button::new(i18n::tr(K::Apply)))
                    .clicked()
                {
                    let actions = app.actions.clone();
                    let toast_msg = || i18n::tr(K::AffinitySet).to_string();
                    app.run_action(&ctx.clone(), toast_msg, move || {
                        actions.set_affinity_mask(pid, new_mask)
                    });
                    app.affinity_dialog = None;
                }
            });
        });
    if !open {
        app.affinity_dialog = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registered sortable column must compare its OWN field: two
    /// entries differing only in that field must order differently (and two
    /// identical ones must tie). This kills the class of bugs where UAC
    /// sorted by priority or GPU fell through to name (§25.6).
    #[test]
    fn details_every_column_sorts_its_own_field() {
        // Fixtures start FULLY identical; each arm below then differs exactly
        // one field so a wrong comparator cannot hide behind other deltas.
        let mk = || ProcessEntry::new(100, "same.exe");
        for spec in COLUMNS {
            let (mut a, mut b) = (mk(), mk());
            // First: identical entries tie on every column...
            assert_eq!(
                spec.cid.compare(&a, &b),
                CmpOrdering::Equal,
                "{:?} must tie for identical entries",
                spec.cid
            );
            // ...then differ exactly one field per column.
            match spec.cid {
                ColumnId::Name => {
                    a.display = "aaa".into();
                    a.name = "aaa".into();
                    b.display = "zzz".into();
                    b.name = "zzz".into();
                }
                ColumnId::Pid => {
                    a.pid = 1;
                    b.pid = 2;
                }
                ColumnId::Status => {
                    a.status = ProcStatus::Running;
                    b.status = ProcStatus::NotResponding;
                }
                ColumnId::User => {
                    a.user = Some("alice".into());
                    b.user = Some("bob".into());
                }
                ColumnId::Cpu => {
                    a.cpu_pct = 5.0;
                    b.cpu_pct = 50.0;
                }
                ColumnId::Memory => {
                    a.mem_bytes = 100;
                    b.mem_bytes = 200;
                }
                ColumnId::Platform => {
                    a.wow64 = Some(false);
                    b.wow64 = Some(true);
                }
                ColumnId::Elevated => {
                    a.elevated = Some(false);
                    b.elevated = Some(true);
                }
                ColumnId::Uac => {
                    a.uac_virtualization = Some(UacVirtualization::Disabled);
                    b.uac_virtualization = Some(UacVirtualization::NotAllowed);
                }
                ColumnId::GpuUtil => {
                    a.gpu_util_pct = None;
                    b.gpu_util_pct = Some(30.0);
                }
                ColumnId::GpuEngine => {
                    a.gpu_engine_label = Some("GPU 0 - 3D".into());
                    b.gpu_engine_label = Some("GPU 0 - VideoDecode".into());
                }
            }
            assert_ne!(
                spec.cid.compare(&a, &b),
                CmpOrdering::Equal,
                "{:?} must detect a difference in its own field",
                spec.cid
            );
        }
    }

    /// Regression: UAC virtualization no longer sorts by priority class.
    #[test]
    fn uac_sort_ignores_priority() {
        let (mut a, mut b) = (mk_proc(1, "x.exe"), mk_proc(2, "x.exe"));
        a.priority = PriorityClass::Realtime;
        b.priority = PriorityClass::Low;
        a.uac_virtualization = Some(UacVirtualization::Enabled);
        b.uac_virtualization = Some(UacVirtualization::Enabled);
        assert_eq!(
            ColumnId::Uac.compare(&a, &b),
            CmpOrdering::Equal,
            "equal virtualization must tie even with different priorities"
        );
    }

    /// Regression: GPU utilization sorts numerically, not by name —
    /// deliberately with names ordered opposite to the values.
    #[test]
    fn gpu_util_sorts_by_value() {
        let (mut a, mut b) = (mk_proc(1, "z.exe"), mk_proc(2, "a.exe"));
        a.gpu_util_pct = Some(80.0);
        b.gpu_util_pct = Some(10.0);
        assert_eq!(ColumnId::GpuUtil.compare(&a, &b), CmpOrdering::Greater);
    }

    fn mk_proc(pid: u32, name: &str) -> ProcessEntry {
        ProcessEntry::new(pid, name)
    }
}

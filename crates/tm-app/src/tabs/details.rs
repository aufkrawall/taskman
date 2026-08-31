//! Details tab: dense process table with stable column ids and an optional
//! System Informer-style literal parent/child tree.
//!
//! Sorting is keyed by [`ColumnId`], never by positional index. Missing
//! telemetry renders as "—"/"Unknown", never as a fabricated zero.

use eframe::egui;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::{PriorityClass, ProcStatus, ProcessEntry, UacVirtualization};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::search;
use crate::theme;
use crate::widgets::menu;
use crate::widgets::tablekit::{self, TmColumn};

type AffinityLoadResult = Arc<Mutex<Option<std::result::Result<(u64, u64), String>>>>;

#[derive(Debug, Clone)]
pub struct AffinityDialog {
    pub identity: crate::app::ProcessIdentity,
    pub mask: Option<u64>,
    pub system_mask: Option<u64>,
    pub error: Option<String>,
    pub load_result: AffinityLoadResult,
    pub save_for_program: bool,
    pub rule_key: Option<String>,
}

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
    Priority,
    Threads,
    Handles,
    CpuTime,
    Commit,
    PeakMemory,
    GpuDedicated,
    GpuShared,
    Description,
    Publisher,
    ParentPid,
    SessionId,
    ImagePath,
    PageFaults,
    IoRead,
    IoWrite,
    CommandLine,
}

impl ColumnId {
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
            ColumnId::Uac => uac_rank(a.uac_virtualization).cmp(&uac_rank(b.uac_virtualization)),
            ColumnId::GpuUtil => a
                .gpu_util_pct
                .partial_cmp(&b.gpu_util_pct)
                .unwrap_or(CmpOrdering::Equal),
            ColumnId::GpuEngine => {
                cmp_option_str(a.gpu_engine_label.as_deref(), b.gpu_engine_label.as_deref())
            }
            ColumnId::Priority => a.priority.cmp(&b.priority),
            ColumnId::Threads => a.threads.cmp(&b.threads),
            ColumnId::Handles => a.handles.cmp(&b.handles),
            ColumnId::CpuTime => cmp_option_f64(a.cpu_time_s, b.cpu_time_s),
            ColumnId::Commit => a.commit_bytes.cmp(&b.commit_bytes),
            ColumnId::PeakMemory => a.peak_mem_bytes.cmp(&b.peak_mem_bytes),
            ColumnId::GpuDedicated => a.gpu_dedicated_bytes.cmp(&b.gpu_dedicated_bytes),
            ColumnId::GpuShared => a.gpu_shared_bytes.cmp(&b.gpu_shared_bytes),
            ColumnId::Description => cmp_option_str(
                a.description.as_deref().or(Some(a.display.as_str())),
                b.description.as_deref().or(Some(b.display.as_str())),
            ),
            ColumnId::Publisher => cmp_option_str(a.company.as_deref(), b.company.as_deref()),
            ColumnId::ParentPid => a.ppid.cmp(&b.ppid),
            ColumnId::SessionId => a.session_id.cmp(&b.session_id),
            ColumnId::ImagePath => cmp_option_str(
                a.exe_path.as_ref().and_then(|path| path.to_str()),
                b.exe_path.as_ref().and_then(|path| path.to_str()),
            ),
            ColumnId::PageFaults => a.page_faults_per_s.cmp(&b.page_faults_per_s),
            ColumnId::IoRead => a.disk_read_total.cmp(&b.disk_read_total),
            ColumnId::IoWrite => a.disk_write_total.cmp(&b.disk_write_total),
            ColumnId::CommandLine => {
                cmp_option_str(a.command_line.as_deref(), b.command_line.as_deref())
            }
        }
    }
}

/// Order the Details list is presented in.
///
/// The Name column has a third state that no direction flag can express:
/// children strictly under their parent in creation order, with no
/// alphabetical (or any other) reordering at all. That state IS the process
/// tree — there is no separate tree switch. System Informer works the same
/// way: click Name until the arrow disappears and the hierarchy appears, and
/// click any other column to leave it and sort by that column alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortOrder {
    Ascending,
    Descending,
    /// No column sort at all — the literal parent/child tree. Reachable only
    /// from [`ColumnId::Name`]; every other column is a plain flat list, so
    /// clicking one leaves the tree.
    Hierarchical,
}

impl SortOrder {
    /// Direction to sort in. Hierarchical order sorts nothing, but siblings
    /// still need a deterministic tie-break, and ascending creation order is
    /// what "hierarchical" means.
    pub fn ascending(self) -> bool {
        !matches!(self, SortOrder::Descending)
    }

    /// Arrow the header should draw, or `None` when no column is sorted.
    pub fn arrow(self) -> Option<bool> {
        match self {
            SortOrder::Ascending => Some(true),
            SortOrder::Descending => Some(false),
            SortOrder::Hierarchical => None,
        }
    }

    /// Next state when the user clicks the column that is ALREADY sorted.
    /// `offers_hierarchy` is true only for the Name column.
    fn next(self, offers_hierarchy: bool) -> Self {
        match (self, offers_hierarchy) {
            (SortOrder::Ascending, _) => SortOrder::Descending,
            (SortOrder::Descending, true) => SortOrder::Hierarchical,
            (SortOrder::Descending, false) => SortOrder::Ascending,
            (SortOrder::Hierarchical, _) => SortOrder::Ascending,
        }
    }

    /// Whether `cid` can reach [`SortOrder::Hierarchical`] at all.
    fn offered_by(cid: ColumnId) -> bool {
        cid == ColumnId::Name
    }

    /// Persisted form. The hierarchy flag is only honored for the column
    /// that can produce it, so a hand-edited config cannot leave the table
    /// unsorted under, say, the CPU column.
    pub fn from_saved(cid: ColumnId, ascending: bool, hierarchical: bool) -> Self {
        match (hierarchical && Self::offered_by(cid), ascending) {
            (true, _) => SortOrder::Hierarchical,
            (false, true) => SortOrder::Ascending,
            (false, false) => SortOrder::Descending,
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

fn cmp_option_f64(a: Option<f64>, b: Option<f64>) -> CmpOrdering {
    match (a, b) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(CmpOrdering::Equal),
        (Some(_), None) => CmpOrdering::Less,
        (None, Some(_)) => CmpOrdering::Greater,
        (None, None) => CmpOrdering::Equal,
    }
}

#[derive(Clone, Copy)]
struct ColSpec {
    cid: ColumnId,
    col: fn() -> TmColumn,
    default_visible: bool,
}

impl ColSpec {
    /// Stable schema id (the `TmColumn` id widths are persisted under).
    fn id(self) -> &'static str {
        (self.col)().id
    }

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
            ColumnId::Priority => i18n::tr(K::Priority),
            ColumnId::Threads => i18n::tr(K::StatThreads),
            ColumnId::Handles => i18n::tr(K::StatHandles),
            ColumnId::CpuTime => "CPU time",
            ColumnId::Commit => "Commit size",
            ColumnId::PeakMemory => "Peak working set",
            ColumnId::GpuDedicated => "Dedicated GPU memory",
            ColumnId::GpuShared => "Shared GPU memory",
            ColumnId::Description => i18n::tr(K::ColDescription),
            ColumnId::Publisher => i18n::tr(K::ColPublisher),
            ColumnId::ParentPid => i18n::tr(K::ColParentPid),
            ColumnId::SessionId => i18n::tr(K::ColSessionId),
            ColumnId::ImagePath => i18n::tr(K::ColImagePath),
            ColumnId::PageFaults => i18n::tr(K::ColPageFaults),
            ColumnId::IoRead => i18n::tr(K::ColIoRead),
            ColumnId::IoWrite => i18n::tr(K::ColIoWrite),
            ColumnId::CommandLine => "Command line",
        }
    }
}

const COLUMNS: &[ColSpec] = &[
    ColSpec {
        cid: ColumnId::Name,
        col: || TmColumn::text("name", i18n::tr(K::ColName), 340.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::Pid,
        col: || TmColumn::text("pid", i18n::tr(K::ColPid), 90.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::Status,
        col: || TmColumn::text("status", i18n::tr(K::ColStatus), 150.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::User,
        col: || TmColumn::text("user", i18n::tr(K::ColUsername), 120.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::Cpu,
        col: || TmColumn::num("cpu", i18n::tr(K::ColCpu), 64.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::Memory,
        col: || TmColumn::num("mem", i18n::tr(K::ColMemory), 130.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::Platform,
        col: || TmColumn::text("platform", i18n::tr(K::ColPlatform), 90.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::Elevated,
        col: || TmColumn::text("elevated", i18n::tr(K::ColElevated), 110.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::Uac,
        col: || TmColumn::text("uac", i18n::tr(K::ColUac), 160.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::GpuUtil,
        col: || TmColumn::num("gpu", i18n::tr(K::ColGpu), 80.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::GpuEngine,
        col: || TmColumn::text("gpuengine", i18n::tr(K::ColGpuEngine), 170.0),
        default_visible: true,
    },
    ColSpec {
        cid: ColumnId::Priority,
        col: || TmColumn::text("priority", i18n::tr(K::Priority), 130.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::Threads,
        col: || TmColumn::num("threads", i18n::tr(K::StatThreads), 90.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::Handles,
        col: || TmColumn::num("handles", i18n::tr(K::StatHandles), 90.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::CpuTime,
        col: || TmColumn::num("cputime", "CPU time", 110.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::Commit,
        col: || TmColumn::num("commit", "Commit size", 130.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::PeakMemory,
        col: || TmColumn::num("peakmem", "Peak working set", 145.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::GpuDedicated,
        col: || TmColumn::num("gpudedicated", "Dedicated GPU memory", 165.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::GpuShared,
        col: || TmColumn::num("gpushared", "Shared GPU memory", 155.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::Description,
        col: || TmColumn::text("description", i18n::tr(K::ColDescription), 260.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::Publisher,
        col: || TmColumn::text("publisher", i18n::tr(K::ColPublisher), 220.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::ParentPid,
        col: || TmColumn::num("ppid", i18n::tr(K::ColParentPid), 110.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::SessionId,
        col: || TmColumn::num("session", i18n::tr(K::ColSessionId), 100.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::ImagePath,
        col: || TmColumn::text("imagepath", i18n::tr(K::ColImagePath), 360.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::PageFaults,
        col: || TmColumn::num("pagefaults", i18n::tr(K::ColPageFaults), 120.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::IoRead,
        col: || TmColumn::num("ioread", i18n::tr(K::ColIoRead), 145.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::IoWrite,
        col: || TmColumn::num("iowrite", i18n::tr(K::ColIoWrite), 155.0),
        default_visible: false,
    },
    ColSpec {
        cid: ColumnId::CommandLine,
        col: || TmColumn::text("commandline", "Command line", 360.0),
        default_visible: false,
    },
];

fn spec_for(cid: ColumnId) -> ColSpec {
    *COLUMNS
        .iter()
        .find(|spec| spec.cid == cid)
        .expect("known Details column")
}

fn id_of(cid: ColumnId) -> &'static str {
    spec_for(cid).id()
}

fn cid_from_id(id: &str) -> Option<ColumnId> {
    COLUMNS
        .iter()
        .find(|spec| spec.id() == id)
        .map(|spec| spec.cid)
}

/// Built-in display order, for order-persistence diffing.
fn default_order_ids() -> Vec<String> {
    COLUMNS.iter().map(|spec| spec.id().to_string()).collect()
}

pub struct State {
    pub sort_col: ColumnId,
    pub sort_order: SortOrder,
    pub filter: String,
    pub cache: Option<Cache>,
    pub visible: BTreeSet<ColumnId>,
    /// Stable display order. Hidden columns remain here so toggling them back
    /// on never destroys the user's relative ordering of active columns.
    pub order: Vec<ColumnId>,
    pub select_columns_open: bool,
    /// Expanded process identities for the literal Details tree.
    /// Pids the user explicitly COLLAPSED. Everything else is expanded, so
    /// the tree always shows the complete parent/child hierarchy — including
    /// processes that started after the page was first opened. Tracking the
    /// exceptions instead of the expansions is what keeps it that way.
    pub collapsed: HashSet<u32>,
    view_generation: u64,
}

impl State {
    pub fn requires_gpu_telemetry(&self) -> bool {
        self.visible.iter().any(|cid| {
            matches!(
                cid,
                ColumnId::GpuUtil
                    | ColumnId::GpuEngine
                    | ColumnId::GpuDedicated
                    | ColumnId::GpuShared
            )
        })
    }

    pub fn is_visible(&self, cid: ColumnId) -> bool {
        self.visible.contains(&cid)
    }

    pub fn set_visible(&mut self, cid: ColumnId, on: bool) {
        if !on && self.visible.len() <= 1 && self.visible.contains(&cid) {
            return;
        }
        if on {
            self.visible.insert(cid);
        } else {
            self.visible.remove(&cid);
            if self.sort_col == cid {
                self.sort_col = self
                    .order
                    .iter()
                    .copied()
                    .find(|candidate| self.visible.contains(candidate))
                    .unwrap_or(ColumnId::Name);
                self.sort_order = SortOrder::Ascending;
            }
        }
        self.invalidate();
    }

    /// Restore persisted user preferences: visibility overrides
    /// (`column id -> on`; absent ids keep their built-in default) and
    /// display order (ids unknown to this build are skipped; columns missing
    /// from the file keep their built-in position). Guards keep the table
    /// usable even for a hand-edited file: at least one column stays visible
    /// and the sort column is always visible.
    pub fn apply_saved_prefs(
        &mut self,
        visible: Option<&BTreeMap<String, bool>>,
        order: Option<&[String]>,
    ) {
        if let Some(order) = order {
            let mut next: Vec<ColumnId> = Vec::with_capacity(order.len());
            for id in order {
                if let Some(cid) = cid_from_id(id)
                    && !next.contains(&cid)
                {
                    next.push(cid);
                }
            }
            for spec in COLUMNS {
                if !next.contains(&spec.cid) {
                    next.push(spec.cid);
                }
            }
            self.order = next;
        }
        if let Some(visible) = visible {
            let mut next: BTreeSet<ColumnId> = COLUMNS
                .iter()
                .filter(|c| c.default_visible)
                .map(|c| c.cid)
                .collect();
            for (id, on) in visible {
                match cid_from_id(id) {
                    Some(cid) if *on => {
                        next.insert(cid);
                    }
                    Some(cid) => {
                        next.remove(&cid);
                    }
                    None => {}
                }
            }
            // A hand-edited file may hide everything; keep the defaults
            // rather than rendering an empty table.
            if !next.is_empty() {
                self.visible = next;
            }
        }
        if !self.visible.contains(&self.sort_col) {
            self.sort_col = self
                .order
                .iter()
                .copied()
                .find(|candidate| self.visible.contains(candidate))
                .unwrap_or(ColumnId::Name);
            self.sort_order = SortOrder::Ascending;
        }
        self.invalidate();
    }

    /// Visibility entries that differ from the built-in defaults, for
    /// persistence (empty = nothing to store).
    pub fn saved_visibility(&self) -> BTreeMap<String, bool> {
        COLUMNS
            .iter()
            .filter(|c| self.visible.contains(&c.cid) != c.default_visible)
            .map(|c| (c.id().to_string(), self.visible.contains(&c.cid)))
            .collect()
    }

    /// Current display order as stable ids (for persistence).
    pub fn saved_order(&self) -> Vec<String> {
        self.order
            .iter()
            .copied()
            .map(id_of)
            .map(str::to_string)
            .collect()
    }

    fn ordered_visible(&self) -> Vec<ColSpec> {
        self.order
            .iter()
            .copied()
            .filter(|cid| self.visible.contains(cid))
            .map(spec_for)
            .collect()
    }

    fn visible_rank(&self, cid: ColumnId) -> Option<usize> {
        self.order
            .iter()
            .copied()
            .filter(|candidate| self.visible.contains(candidate))
            .position(|candidate| candidate == cid)
    }

    fn visible_count(&self) -> usize {
        self.order
            .iter()
            .filter(|candidate| self.visible.contains(candidate))
            .count()
    }

    fn move_visible(&mut self, cid: ColumnId, delta: isize) -> bool {
        let visible: Vec<ColumnId> = self
            .order
            .iter()
            .copied()
            .filter(|candidate| self.visible.contains(candidate))
            .collect();
        let Some(pos) = visible.iter().position(|candidate| *candidate == cid) else {
            return false;
        };
        let target = pos as isize + delta;
        if target < 0 || target >= visible.len() as isize {
            return false;
        }
        let other = visible[target as usize];
        let a = self
            .order
            .iter()
            .position(|candidate| *candidate == cid)
            .unwrap();
        let b = self
            .order
            .iter()
            .position(|candidate| *candidate == other)
            .unwrap();
        self.order.swap(a, b);
        self.invalidate();
        true
    }

    pub fn lang_generation(&self) -> u64 {
        i18n::lang() as u64
    }

    pub fn invalidate(&mut self) {
        self.cache = None;
    }

    pub fn is_expanded(&self, pid: u32) -> bool {
        !self.collapsed.contains(&pid)
    }

    pub fn toggle_expanded(&mut self, pid: u32) {
        if !self.collapsed.remove(&pid) {
            self.collapsed.insert(pid);
        }
        self.view_generation = self.view_generation.wrapping_add(1);
        self.invalidate();
    }

    pub fn apply_saved_sort(&mut self, column: &str, ascending: bool, hierarchical: bool) {
        if let Some(cid) = cid_from_id(column)
            && self.visible.contains(&cid)
        {
            self.sort_col = cid;
            self.sort_order = SortOrder::from_saved(cid, ascending, hierarchical);
            self.invalidate();
        }
    }

    /// Whether the page currently shows the parent/child tree. There is no
    /// separate switch: the tree IS the Name column's third sort state.
    pub fn is_tree(&self) -> bool {
        self.sort_order == SortOrder::Hierarchical
    }

    /// Apply a header click on `cid` and report the resulting order.
    ///
    /// Clicking any column other than Name always lands on a plain
    /// direction, which is what leaves the tree: the user asked for an
    /// ordering by THAT column, and a tree would keep children pinned under
    /// parents instead.
    pub fn clicked_column(&mut self, cid: ColumnId) -> SortOrder {
        if self.sort_col == cid {
            self.sort_order = self.sort_order.next(SortOrder::offered_by(cid));
        } else {
            self.sort_col = cid;
            self.sort_order = if cid_is_numeric(cid) {
                SortOrder::Descending
            } else {
                SortOrder::Ascending
            };
        }
        self.invalidate();
        self.sort_order
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            sort_col: ColumnId::Name,
            sort_order: SortOrder::Ascending,
            filter: String::new(),
            cache: None,
            visible: COLUMNS
                .iter()
                .filter(|c| c.default_visible)
                .map(|c| c.cid)
                .collect(),
            order: COLUMNS.iter().map(|c| c.cid).collect(),
            select_columns_open: false,
            collapsed: HashSet::new(),
            view_generation: 0,
        }
    }
}

pub struct Cache {
    pub key: (u64, u64, String, ColumnId, SortOrder, u64),
    pub rows: Vec<Row>,
}

pub struct Row {
    pub pid: u32,
    pub start_epoch_s: Option<i64>,
    pub name: String,
    pub icon_path: Option<String>,
    pub depth: usize,
    pub children: bool,
    pub pid_s: String,
    pub status: String,
    pub user: String,
    pub cpu_s: String,
    pub mem_s: String,
    pub platform: String,
    pub elevated: String,
    pub uac: String,
    pub gpu_util_s: String,
    pub gpu_engine_s: String,
    pub priority_s: String,
    pub threads_s: String,
    pub handles_s: String,
    pub cpu_time_s: String,
    pub commit_s: String,
    pub peak_mem_s: String,
    pub gpu_dedicated_s: String,
    pub gpu_shared_s: String,
    pub description_s: String,
    pub publisher_s: String,
    pub ppid_s: String,
    pub session_id_s: String,
    pub image_path_s: String,
    pub page_faults_s: String,
    pub io_read_s: String,
    pub io_write_s: String,
    pub command_line_s: String,
}

impl Row {
    pub fn field(&self, cid: ColumnId) -> &str {
        match cid {
            ColumnId::Name => &self.name,
            ColumnId::Pid => &self.pid_s,
            ColumnId::Status => &self.status,
            ColumnId::User => &self.user,
            ColumnId::Cpu => &self.cpu_s,
            ColumnId::Memory => &self.mem_s,
            ColumnId::Platform => &self.platform,
            ColumnId::Elevated => &self.elevated,
            ColumnId::Uac => &self.uac,
            ColumnId::GpuUtil => &self.gpu_util_s,
            ColumnId::GpuEngine => &self.gpu_engine_s,
            ColumnId::Priority => &self.priority_s,
            ColumnId::Threads => &self.threads_s,
            ColumnId::Handles => &self.handles_s,
            ColumnId::CpuTime => &self.cpu_time_s,
            ColumnId::Commit => &self.commit_s,
            ColumnId::PeakMemory => &self.peak_mem_s,
            ColumnId::GpuDedicated => &self.gpu_dedicated_s,
            ColumnId::GpuShared => &self.gpu_shared_s,
            ColumnId::Description => &self.description_s,
            ColumnId::Publisher => &self.publisher_s,
            ColumnId::ParentPid => &self.ppid_s,
            ColumnId::SessionId => &self.session_id_s,
            ColumnId::ImagePath => &self.image_path_s,
            ColumnId::PageFaults => &self.page_faults_s,
            ColumnId::IoRead => &self.io_read_s,
            ColumnId::IoWrite => &self.io_write_s,
            ColumnId::CommandLine => &self.command_line_s,
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
            // The process tree has no switch of its own: it is the Name
            // column's third sort state. This entry is the discoverable way
            // in and out of it, and it names the gesture so the header click
            // does not have to be guessed.
            let hierarchical = app.details_state.is_tree();
            if menu::check(ui, i18n::tr(K::ProcessTreeView), hierarchical).clicked() {
                let order = if hierarchical {
                    app.details_state.sort_col = ColumnId::Name;
                    SortOrder::Ascending
                } else {
                    app.details_state.sort_col = ColumnId::Name;
                    SortOrder::Hierarchical
                };
                app.details_state.sort_order = order;
                app.details_state.invalidate();
                persist_sort_pref(app, ColumnId::Name, order);
                ui.close();
            }
            if app.details_state.is_tree() {
                if menu::item(ui, i18n::tr(K::ExpandAll)).clicked() {
                    app.details_state.collapsed.clear();
                    app.details_state.view_generation =
                        app.details_state.view_generation.wrapping_add(1);
                    app.details_state.invalidate();
                    ui.close();
                }
                if menu::item(ui, i18n::tr(K::CollapseAll)).clicked() {
                    if let Some(snapshot) = app.latest_snapshot() {
                        app.details_state
                            .collapsed
                            .extend(snapshot.processes.iter().map(|process| process.pid));
                    }
                    app.details_state.view_generation =
                        app.details_state.view_generation.wrapping_add(1);
                    app.details_state.invalidate();
                    ui.close();
                }
            }
            menu::separator(ui);
            if menu::item(ui, i18n::tr(K::RefreshNow)).clicked() {
                app.refresh_all();
                ui.close();
            }
            if menu::item(ui, i18n::tr(K::SelectColumns)).clicked() {
                app.details_state.select_columns_open = true;
                ui.close();
            }
        },
    );

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

    if let Some(focus) = app.pending_details_focus.take() {
        app.selected_process = Some(focus.0.clone());
        app.search.clear();
        app.details_state.filter.clear();
        app.scroll_to_pid = Some(focus.0.pid);
    }

    let visible_cols = app.details_state.ordered_visible();
    let cols_rendered: Vec<TmColumn> = visible_cols.iter().map(|c| (c.col)()).collect();
    // Native TM packs the Details list; the 32 px app-list row height reads
    // as broken gaps between entries here.
    let mut table = app
        .make_table("details", cols_rendered)
        .with_row_height(tablekit::ROW_H_DENSE);

    let key = (
        snap.timestamp_ms,
        app.details_state.lang_generation(),
        effective_search(app),
        app.details_state.sort_col,
        app.details_state.sort_order,
        app.details_state.view_generation,
    );
    let mut cache = app.details_state.cache.take();
    let stale = cache.as_ref().is_none_or(|c| c.key != key);
    if stale {
        cache = Some(Cache {
            key: key.clone(),
            rows: build_rows(&snap, &key.2, key.3, key.4, &app.details_state.collapsed),
        });
    }
    let rows = &cache.as_ref().expect("cache").rows;

    // Native-style type navigation follows the current filtered/sorted list,
    // accumulates fast keystrokes into one word, cycles repeated initials,
    // and scrolls the virtual row into view.
    if let Some(typed) = search::list_type_ahead(ui.ctx(), "details") {
        let selected = app.selected_process.as_ref().map(|p| p.pid);
        let candidates = rows
            .iter()
            .map(|row| (row.pid, row.name.as_str()))
            .collect::<Vec<_>>();
        if let Some(pid) = search::type_ahead_match(candidates, selected, &typed)
            && let Some(row) = rows.iter().find(|row| row.pid == pid)
        {
            app.selected_process = Some(crate::app::ProcessIdentity {
                pid: row.pid,
                start_epoch_s: row.start_epoch_s,
            });
            app.scroll_to_pid = Some(row.pid);
        }
    }

    handle_keyboard_navigation(app, ui.ctx(), rows);

    prepare_auto_fit_widths(ui, &mut table, &visible_cols, rows);

    let avail = tablekit::table_avail(ui);
    let sorted_pos = visible_cols
        .iter()
        .position(|c| c.cid == app.details_state.sort_col);

    // Capture the on-screen header bounds before the table consumes the body.
    // Secondary-click in this area opens the same visibility dialog as the
    // overflow command, matching Task Manager's column-header affordance.
    let header_min = ui.cursor().min;
    let header_rect = egui::Rect::from_min_size(
        header_min,
        egui::vec2(avail.min(table.total_width()), tablekit::HEADER_H1),
    );

    // Consume any pending scroll request (type-ahead or cross-tab focus) as
    // a row index for the table's vertical-only scroll-to-row.
    let focus_row = app
        .scroll_to_pid
        .take()
        .and_then(|pid| rows.iter().position(|row| row.pid == pid));

    let clicked = tablekit::scrolled_rows(
        "details",
        ui,
        &pal,
        &mut table,
        avail,
        sorted_pos.and_then(|p| app.details_state.sort_order.arrow().map(|asc| (p, asc))),
        None,
        rows.len(),
        focus_row,
        |ui, table, _avail, _content_w, range| {
            for i in range {
                let Some(row) = rows.get(i) else { continue };
                let selected = app
                    .selected_process
                    .as_ref()
                    .is_some_and(|sp| sp.pid == row.pid);
                let (rect, resp) = table.row(ui, &pal, selected);

                // Name decorations follow the Name column even after it has
                // been moved away from the first position.
                for (pos, spec) in visible_cols.iter().enumerate() {
                    let cid = spec.cid;
                    let text = row.field(cid);
                    if cid == ColumnId::Name {
                        let cell = table.col_rect(pos, rect);
                        let indented_cell =
                            cell.translate(egui::vec2(row.depth as f32 * TREE_INDENT, 0.0));
                        let expanded = app.details_state.is_expanded(row.pid);
                        let seed = egui::Id::new((
                            "details-chev",
                            row.pid,
                            row.start_epoch_s.unwrap_or(0),
                        ));
                        if row.children
                            && table.chevron(ui, indented_cell, expanded, true, &pal, seed)
                        {
                            app.details_state.toggle_expanded(row.pid);
                        }
                        let tex = row
                            .icon_path
                            .as_ref()
                            .and_then(|p| app.shared.icons.get(ui.ctx(), &app.actions, p, 6));
                        table.icon_cell(ui, indented_cell, tex.as_ref(), pal.accent);
                        ui.painter_at(cell).text(
                            egui::Pos2::new(
                                cell.left() + 56.0 + row.depth as f32 * TREE_INDENT,
                                rect.center().y,
                            ),
                            egui::Align2::LEFT_CENTER,
                            &row.name,
                            egui::FontId::proportional(tablekit::FONT_ROW),
                            pal.text,
                        );
                    } else if cid_is_numeric(cid) {
                        let cell = table.col_rect(pos, rect);
                        ui.painter_at(cell).text(
                            egui::Pos2::new(cell.right() - 10.0, rect.center().y),
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
                menu::context_menu(&resp, |ui| {
                    if let Some(p) = snap.process(row.pid) {
                        context_menu(app, ui, p, row.children);
                    }
                });
            }
        },
    );

    let header_secondary = ui.ctx().input(|i| {
        i.pointer.button_clicked(egui::PointerButton::Secondary)
            && i.pointer
                .interact_pos()
                .is_some_and(|p| header_rect.contains(p))
    });
    if header_secondary {
        app.details_state.select_columns_open = true;
    }

    if let Some(display_idx) = clicked
        && let Some(spec) = visible_cols.get(display_idx)
    {
        let cid = spec.cid;
        let order = app.details_state.clicked_column(cid);
        persist_sort_pref(app, cid, order);
    }
    app.persist_table(&table);
    app.details_state.cache = cache;

    select_columns_dialog(app, &ctx_from(ui), &pal);
}

fn select_detail_row(app: &mut TaskManApp, row: &Row) {
    app.selected_process = Some(crate::app::ProcessIdentity {
        pid: row.pid,
        start_epoch_s: row.start_epoch_s,
    });
    app.scroll_to_pid = Some(row.pid);
}

/// Native list navigation plus tree-aware Left/Right movement. The flattened
/// rows remain virtualized, so selection also records a one-shot scroll target.
fn handle_keyboard_navigation(app: &mut TaskManApp, ctx: &egui::Context, rows: &[Row]) {
    let selected_pos = app
        .selected_process
        .as_ref()
        .and_then(|selected| rows.iter().position(|row| row.pid == selected.pid));
    // Page movement must count the rows this page actually renders.
    let page_rows = (ctx.content_rect().height() / tablekit::ROW_H_DENSE)
        .floor()
        .max(1.0) as usize;
    if let Some(nav) = search::list_nav(ctx)
        && let Some(index) = search::moved_index(rows.len(), selected_pos, nav, page_rows)
        && let Some(row) = rows.get(index)
    {
        select_detail_row(app, row);
    }

    if !app.details_state.is_tree() || ctx.egui_wants_keyboard_input() {
        return;
    }
    let (left, right) = ctx.input(|input| {
        (
            input.key_pressed(egui::Key::ArrowLeft),
            input.key_pressed(egui::Key::ArrowRight),
        )
    });
    let Some(index) = app
        .selected_process
        .as_ref()
        .and_then(|selected| rows.iter().position(|row| row.pid == selected.pid))
    else {
        return;
    };
    let row = &rows[index];
    if right && row.children {
        if !app.details_state.is_expanded(row.pid) {
            app.details_state.toggle_expanded(row.pid);
        } else if let Some(child) = rows.get(index + 1)
            && child.depth > row.depth
        {
            select_detail_row(app, child);
        }
    } else if left {
        if row.children && app.details_state.is_expanded(row.pid) {
            app.details_state.toggle_expanded(row.pid);
        } else if row.depth > 0
            && let Some(parent) = rows[..index]
                .iter()
                .rev()
                .find(|candidate| candidate.depth < row.depth)
        {
            select_detail_row(app, parent);
        }
    }
}

fn prepare_auto_fit_widths(
    ui: &egui::Ui,
    table: &mut tablekit::TmTable,
    visible_cols: &[ColSpec],
    rows: &[Row],
) {
    for (pos, spec) in visible_cols.iter().enumerate() {
        let mut width = tablekit::text_width(ui, spec.label(), tablekit::FONT_HDR_LABEL) + 28.0;
        for row in rows {
            let extra = if spec.cid == ColumnId::Name {
                66.0 + row.depth as f32 * TREE_INDENT
            } else {
                22.0
            };
            width = width
                .max(tablekit::text_width(ui, row.field(spec.cid), tablekit::FONT_ROW) + extra);
        }
        table.set_auto_fit_width(pos, width.ceil());
    }
}

fn ctx_from(ui: &egui::Ui) -> egui::Context {
    ui.ctx().clone()
}

/// Copy the current details column preferences into the settings snapshot
/// and queue a debounced autosave — the same path as column widths. The
/// order is only stored while it differs from the built-in order, and
/// visibility entries matching the defaults are dropped, so fresh installs
/// and never-touched tables keep following schema changes.
fn persist_column_prefs(app: &mut TaskManApp) {
    const TABLE: &str = "details";
    let visibility = app.details_state.saved_visibility();
    if visibility.is_empty() {
        app.shared.settings.col_visible.remove(TABLE);
    } else {
        app.shared
            .settings
            .col_visible
            .insert(TABLE.to_string(), visibility);
    }
    if app.details_state.saved_order() == default_order_ids() {
        app.shared.settings.col_order.remove(TABLE);
    } else {
        let order = app.details_state.saved_order();
        app.shared
            .settings
            .col_order
            .insert(TABLE.to_string(), order);
    }
    app.save_settings();
}

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
            ui.set_min_width(330.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            egui::ScrollArea::vertical()
                .max_height(330.0)
                .show(ui, |ui| {
                    // Iterate a snapshot because arrow clicks mutate the live
                    // ordering. The updated order is visible next frame.
                    let order = app.details_state.order.clone();
                    for cid in order {
                        let spec = spec_for(cid);
                        let mut on = app.details_state.is_visible(cid);
                        let rank = app.details_state.visible_rank(cid);
                        let count = app.details_state.visible_count();
                        ui.horizontal(|ui| {
                            if crate::widgets::controls::checkbox(ui, &mut on, spec.label(), pal)
                                .changed()
                            {
                                app.details_state.set_visible(cid, on);
                                persist_column_prefs(app);
                                let sort_col = app.details_state.sort_col;
                                let order = app.details_state.sort_order;
                                persist_sort_pref(app, sort_col, order);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if on {
                                        let can_down = rank.is_some_and(|r| r + 1 < count);
                                        let can_up = rank.is_some_and(|r| r > 0);
                                        ui.add_space(4.0);
                                        if crate::widgets::controls::icon_button(
                                            ui,
                                            crate::icons::Icon::ChevronDown,
                                            can_down,
                                            pal,
                                        )
                                        .on_hover_text("Move column down")
                                        .clicked()
                                        {
                                            app.details_state.move_visible(cid, 1);
                                            persist_column_prefs(app);
                                        }
                                        if crate::widgets::controls::icon_button(
                                            ui,
                                            crate::icons::Icon::ChevronUp,
                                            can_up,
                                            pal,
                                        )
                                        .on_hover_text("Move column up")
                                        .clicked()
                                        {
                                            app.details_state.move_visible(cid, -1);
                                            persist_column_prefs(app);
                                        }
                                    }
                                },
                            );
                        });
                    }
                });
            ui.separator();
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

/// Persist the Details sort.
///
/// The strict-hierarchy state is not expressible as an ascending flag, so it
/// rides in `[general]` beside the tree-view switch rather than corrupting
/// the `[sort]` line format every other table shares.
fn persist_sort_pref(app: &mut TaskManApp, cid: ColumnId, order: SortOrder) {
    let hierarchical = order == SortOrder::Hierarchical;
    if app.shared.settings.details_tree_hierarchical != hierarchical {
        app.shared.settings.details_tree_hierarchical = hierarchical;
        app.save_settings();
    }
    app.persist_sort("details", id_of(cid), order.ascending());
}

fn cid_is_numeric(cid: ColumnId) -> bool {
    matches!(
        cid,
        ColumnId::Pid
            | ColumnId::Cpu
            | ColumnId::Memory
            | ColumnId::GpuUtil
            | ColumnId::Threads
            | ColumnId::Handles
            | ColumnId::CpuTime
            | ColumnId::Commit
            | ColumnId::PeakMemory
            | ColumnId::GpuDedicated
            | ColumnId::GpuShared
            | ColumnId::ParentPid
            | ColumnId::SessionId
            | ColumnId::PageFaults
            | ColumnId::IoRead
            | ColumnId::IoWrite
    )
}

fn effective_search(app: &TaskManApp) -> String {
    if !app.details_state.filter.is_empty() {
        app.details_state.filter.clone()
    } else {
        app.search.clone()
    }
}

fn priority_label(p: PriorityClass) -> &'static str {
    match p {
        PriorityClass::Realtime => i18n::tr(K::PrioRealtime),
        PriorityClass::High => i18n::tr(K::PrioHigh),
        PriorityClass::AboveNormal => i18n::tr(K::PrioAboveNormal),
        PriorityClass::Normal => i18n::tr(K::PrioNormal),
        PriorityClass::BelowNormal => i18n::tr(K::PrioBelowNormal),
        PriorityClass::Low => i18n::tr(K::PrioLow),
        PriorityClass::Unknown => "—",
    }
}

fn opt_u64_bytes(v: Option<u64>) -> String {
    v.map(format::format_k).unwrap_or_else(|| "—".into())
}

fn build_rows(
    snap: &tm_core::model::Snapshot,
    raw_search: &str,
    sort_col: ColumnId,
    order: SortOrder,
    collapsed: &HashSet<u32>,
) -> Vec<Row> {
    let q = search::Query::new(raw_search);
    // The tree is not a mode: it is exactly the Name column's unsorted
    // state. Any other order is a plain flat list.
    if order != SortOrder::Hierarchical {
        let mut list: Vec<&ProcessEntry> = snap
            .processes
            .iter()
            .filter(|process| !process.synthetic && q.matches_process(process))
            .collect();
        sort_processes(&mut list, sort_col, order);
        return list
            .into_iter()
            .map(|process| row_from_process(process, 0, false))
            .collect();
    }

    let all: Vec<&ProcessEntry> = snap
        .processes
        .iter()
        .filter(|process| !process.synthetic)
        .collect();
    let by_pid: HashMap<u32, &ProcessEntry> =
        all.iter().map(|process| (process.pid, *process)).collect();
    let mut visible: HashSet<u32> = all
        .iter()
        .copied()
        .filter(|process| q.matches_process(process))
        .map(|process| process.pid)
        .collect();
    if !q.is_empty() {
        for matched in visible.clone() {
            let mut cursor = matched;
            let mut seen = HashSet::new();
            while seen.insert(cursor) {
                let Some(parent) = by_pid
                    .get(&cursor)
                    .and_then(|process| process.ppid)
                    .and_then(|pid| by_pid.get(&pid).copied())
                else {
                    break;
                };
                visible.insert(parent.pid);
                cursor = parent.pid;
            }
        }
    }

    let members: Vec<&ProcessEntry> = all
        .into_iter()
        .filter(|process| visible.contains(&process.pid))
        .collect();
    let mut children: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
    for process in &members {
        if let Some(parent) = process.ppid
            && parent != process.pid
            && visible.contains(&parent)
            && by_pid
                .get(&parent)
                .is_some_and(|parent| is_plausible_parent(parent, process))
        {
            children.entry(parent).or_default().push(*process);
        }
    }
    for siblings in children.values_mut() {
        sort_processes(siblings, sort_col, order);
    }

    let attached: HashSet<u32> = children.values().flatten().map(|child| child.pid).collect();
    let mut roots: Vec<&ProcessEntry> = members
        .iter()
        .copied()
        .filter(|process| !attached.contains(&process.pid))
        .collect();
    // Malformed/cyclic parent graphs can have no natural root. Pick one
    // representative per still-uncovered component so every process remains
    // reachable while the render walk's visited set prevents loops.
    let mut covered = HashSet::new();
    for root in roots.clone() {
        mark_tree_component(root.pid, &children, &mut covered);
    }
    for process in &members {
        if !covered.contains(&process.pid) {
            roots.push(*process);
            mark_tree_component(process.pid, &children, &mut covered);
        }
    }
    sort_processes(&mut roots, sort_col, order);

    let mut rows = Vec::with_capacity(members.len());
    let mut visited = HashSet::with_capacity(members.len());
    let mut stack: Vec<(&ProcessEntry, usize)> =
        roots.into_iter().rev().map(|root| (root, 0)).collect();
    while let Some((process, depth)) = stack.pop() {
        if !visited.insert(process.pid) {
            continue;
        }
        let kids = children
            .get(&process.pid)
            .into_iter()
            .flatten()
            .copied()
            .filter(|child| !visited.contains(&child.pid))
            .collect::<Vec<_>>();
        let has_children = !kids.is_empty();
        rows.push(row_from_process(process, depth, has_children));
        if has_children && (!collapsed.contains(&process.pid) || !q.is_empty()) {
            stack.extend(kids.into_iter().rev().map(|child| (child, depth + 1)));
        }
    }
    rows
}

/// Horizontal step per tree level, shared by rendering and auto-fit. Matched
/// to System Informer's compact indentation so deep trees stay on screen.
const TREE_INDENT: f32 = 18.0;

/// A PID may be recycled the moment its process exits, so a raw parent link
/// can point at a process that started LATER than its supposed child. System
/// Informer rejects those links and shows the child as a root; so do we.
/// Unknown timestamps are never treated as evidence — the link stands.
fn is_plausible_parent(parent: &ProcessEntry, child: &ProcessEntry) -> bool {
    match (parent.start_epoch_s, child.start_epoch_s) {
        (Some(parent_start), Some(child_start)) => parent_start <= child_start,
        _ => true,
    }
}

/// Order one sibling list.
///
/// [`SortOrder::Hierarchical`] deliberately ignores `sort_col` entirely and
/// falls back to CREATION order. It cannot fall back to the snapshot's own
/// order: processes come out of a hash map, so "unsorted" would reshuffle
/// the whole tree on every tick. Start time plus PID is stable, and it is
/// the order the OS actually created the children in.
fn sort_processes(list: &mut [&ProcessEntry], sort_col: ColumnId, order: SortOrder) {
    if order == SortOrder::Hierarchical {
        list.sort_by(|a, b| {
            a.start_epoch_s
                .cmp(&b.start_epoch_s)
                .then_with(|| a.pid.cmp(&b.pid))
        });
        return;
    }
    let ascending = order.ascending();
    list.sort_by(|a, b| {
        let ordering = sort_col.compare(a, b).then_with(|| a.pid.cmp(&b.pid));
        if ascending {
            ordering
        } else {
            ordering.reverse()
        }
    });
}

fn mark_tree_component(
    root: u32,
    children: &HashMap<u32, Vec<&ProcessEntry>>,
    covered: &mut HashSet<u32>,
) {
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !covered.insert(pid) {
            continue;
        }
        stack.extend(
            children
                .get(&pid)
                .into_iter()
                .flatten()
                .map(|child| child.pid),
        );
    }
}

fn process_status_text(process: &ProcessEntry) -> String {
    let base = match process.status {
        ProcStatus::Running => "",
        ProcStatus::Suspended => i18n::tr(K::StSuspended),
        ProcStatus::NotResponding => i18n::tr(K::StNotResponding),
    };
    if process.power_throttled == Some(true) {
        if base.is_empty() {
            i18n::tr(K::StEfficiencyMode).to_string()
        } else {
            format!("{base}, {}", i18n::tr(K::StEfficiencyMode))
        }
    } else {
        base.to_string()
    }
}

fn row_from_process(p: &ProcessEntry, depth: usize, children: bool) -> Row {
    let platform = match p.wow64 {
        Some(true) => i18n::tr(K::Bit32).to_string(),
        Some(false) => i18n::tr(K::Bit64).to_string(),
        None => "—".to_string(),
    };
    let elevated = match p.elevated {
        Some(true) => i18n::tr(K::Yes).to_string(),
        Some(false) => i18n::tr(K::No).to_string(),
        None => i18n::tr(K::UacUnknown).to_string(),
    };
    let uac = match p.uac_virtualization {
        Some(UacVirtualization::Enabled) => i18n::tr(K::EnabledWord).to_string(),
        Some(UacVirtualization::Disabled) => i18n::tr(K::DisabledWord).to_string(),
        Some(UacVirtualization::NotAllowed) => i18n::tr(K::NotAllowed).to_string(),
        _ => i18n::tr(K::UacUnknown).to_string(),
    };
    Row {
        pid: p.pid,
        start_epoch_s: p.start_epoch_s,
        name: p.shown_name().to_string(),
        icon_path: p
            .exe_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        depth,
        children,
        pid_s: p.pid.to_string(),
        status: process_status_text(p),
        user: p.user.clone().unwrap_or_else(|| "—".into()),
        cpu_s: format::format_cpu_detail(p.cpu_pct),
        mem_s: format::format_k(p.mem_bytes),
        platform,
        elevated,
        uac,
        gpu_util_s: p
            .gpu_util_pct
            .map(format::format_pct_cell)
            .unwrap_or_else(|| "—".into()),
        gpu_engine_s: p.gpu_engine_label.clone().unwrap_or_else(|| "—".into()),
        priority_s: priority_label(p.priority).to_string(),
        threads_s: p
            .threads
            .map_or_else(|| "—".into(), |value| value.to_string()),
        handles_s: p
            .handles
            .map_or_else(|| "—".into(), |value| value.to_string()),
        cpu_time_s: p
            .cpu_time_s
            .map(|value| format!("{value:.2} s"))
            .unwrap_or_else(|| "—".into()),
        commit_s: opt_u64_bytes(p.commit_bytes),
        peak_mem_s: opt_u64_bytes(p.peak_mem_bytes),
        gpu_dedicated_s: opt_u64_bytes(p.gpu_dedicated_bytes),
        gpu_shared_s: opt_u64_bytes(p.gpu_shared_bytes),
        description_s: p
            .description
            .as_ref()
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| p.display.clone()),
        publisher_s: p.company.clone().unwrap_or_else(|| "—".into()),
        ppid_s: p.ppid.map_or_else(|| "—".into(), |value| value.to_string()),
        session_id_s: p
            .session_id
            .map_or_else(|| "—".into(), |value| value.to_string()),
        image_path_s: p
            .exe_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "—".into()),
        page_faults_s: p
            .page_faults_per_s
            .map(|value| format::format_thousands(value.into()))
            .unwrap_or_else(|| "—".into()),
        io_read_s: format::format_bytes_loc(p.disk_read_total),
        io_write_s: format::format_bytes_loc(p.disk_write_total),
        command_line_s: p.command_line.clone().unwrap_or_else(|| "—".into()),
    }
}

/// Identity gate shared by every destructive context-menu action (audit:
/// End Task and Efficiency mode had one; priority/suspend/affinity did not).
/// The rendered row can be stale — especially while sampling is paused — so
/// re-check the start time against the latest snapshot before dispatching.
fn identity_still_live(app: &TaskManApp, p: &ProcessEntry) -> bool {
    app.identity_is_live(&crate::app::ProcessIdentity {
        pid: p.pid,
        start_epoch_s: p.start_epoch_s,
    })
}

fn scheduling_rule_key(process: &ProcessEntry) -> Option<String> {
    process
        .exe_path
        .as_deref()
        .map(tm_core::settings::process_rule_key)
}

fn update_saved_priority(app: &mut TaskManApp, key: &str, priority: Option<PriorityClass>) {
    let empty = {
        let rule = app
            .shared
            .settings
            .process_rules
            .entry(key.to_string())
            .or_default();
        rule.priority = priority;
        rule.is_empty()
    };
    if empty {
        app.shared.settings.process_rules.remove(key);
    }
    app.save_settings();
}

fn update_saved_affinity(app: &mut TaskManApp, key: &str, affinity_mask: Option<u64>) {
    let empty = {
        let rule = app
            .shared
            .settings
            .process_rules
            .entry(key.to_string())
            .or_default();
        rule.affinity_mask = affinity_mask;
        rule.is_empty()
    };
    if empty {
        app.shared.settings.process_rules.remove(key);
    }
    app.save_settings();
}

/// Context menu mirroring the Win11 TM Details tab.
pub fn context_menu(app: &mut TaskManApp, ui: &mut egui::Ui, p: &ProcessEntry, has_children: bool) {
    let ctx = ui.ctx().clone();
    ui.set_min_width(230.0);
    menu::title(ui, p.shown_name());
    menu::separator(ui);

    if app.details_state.is_tree() && has_children {
        let expanded = app.details_state.is_expanded(p.pid);
        let label = if expanded {
            i18n::tr(K::Collapse)
        } else {
            i18n::tr(K::Expand)
        };
        if menu::item(ui, label).clicked() {
            app.details_state.toggle_expanded(p.pid);
            ui.close();
        }
        menu::separator(ui);
    }

    if menu::item(ui, i18n::tr(K::CopyName)).clicked() {
        ui.ctx().copy_text(p.shown_name().to_string());
        app.shared.toast(i18n::tr(K::Copied));
        ui.close();
    }
    if menu::item(ui, i18n::tr(K::OnlineSearch)).clicked() {
        let url = search::online_search_url(p.shown_name());
        if let Err(error) = app.actions.open_url(&url) {
            app.shared
                .toast(i18n::trf(K::ErrMsg, &[&error.to_string()]));
        }
        ui.close();
    }
    if app.actions.capabilities().process_modules
        && menu::item(ui, i18n::tr(K::ViewModules)).clicked()
    {
        crate::tabs::modules::open(app, p, &ctx);
        ui.close();
    }
    if menu::item(ui, i18n::tr(K::EndTask)).clicked() {
        end_process(app, &ctx, p.pid, p.start_epoch_s, false, p.shown_name());
        ui.close();
    }
    #[cfg(target_os = "windows")]
    if menu::item(ui, i18n::tr(K::EndTree)).clicked() {
        end_process(app, &ctx, p.pid, p.start_epoch_s, true, p.shown_name());
        ui.close();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = p.name.as_str();
    }

    menu::submenu(ui, i18n::tr(K::Priority), |ui| {
        let rule_key = scheduling_rule_key(p);
        let mut save_priority = rule_key.as_ref().is_some_and(|key| {
            app.shared
                .settings
                .process_rules
                .get(key)
                .is_some_and(|rule| rule.priority.is_some())
        });
        for (cls, key) in [
            (PriorityClass::Realtime, K::PrioRealtime),
            (PriorityClass::High, K::PrioHigh),
            (PriorityClass::AboveNormal, K::PrioAboveNormal),
            (PriorityClass::Normal, K::PrioNormal),
            (PriorityClass::BelowNormal, K::PrioBelowNormal),
            (PriorityClass::Low, K::PrioLow),
        ] {
            let current = p.priority == cls;
            if menu::check(ui, i18n::tr(key), current).clicked() {
                if !identity_still_live(app, p) {
                    app.shared.toast(i18n::tr(K::ProcessExited));
                    ui.close();
                    return;
                }
                let actions = app.actions.clone();
                let pid = p.pid;
                let start = p.start_epoch_s;
                let key_copy = key;
                let msg = move || i18n::trf(K::PrioritySetMsg, &[i18n::tr(key_copy)]);
                app.run_action_refreshing(&ctx, msg, move || {
                    actions.set_priority_checked(pid, start, cls)
                });
                if save_priority && let Some(rule_key) = rule_key.as_deref() {
                    update_saved_priority(app, rule_key, Some(cls));
                }
                ui.close();
            }
        }
        menu::separator(ui);
        let save = menu::toggle_enabled(
            ui,
            i18n::tr(K::SavePriorityForProgram),
            &mut save_priority,
            rule_key.is_some() && p.priority != PriorityClass::Unknown,
        )
        .on_disabled_hover_text(i18n::tr(K::SaveRuleNeedsPath));
        if save.changed()
            && let Some(rule_key) = rule_key.as_deref()
        {
            update_saved_priority(app, rule_key, save_priority.then_some(p.priority));
        }
    });

    if menu::item(ui, i18n::tr(K::SetAffinity)).clicked() {
        if !identity_still_live(app, p) {
            app.shared.toast(i18n::tr(K::ProcessExited));
        } else {
            let identity = crate::app::ProcessIdentity {
                pid: p.pid,
                start_epoch_s: p.start_epoch_s,
            };
            let rule_key = scheduling_rule_key(p);
            let save_for_program = rule_key.as_ref().is_some_and(|key| {
                app.shared
                    .settings
                    .process_rules
                    .get(key)
                    .is_some_and(|rule| rule.affinity_mask.is_some())
            });
            let load_result = Arc::new(Mutex::new(None));
            app.affinity_dialog = Some(AffinityDialog {
                identity: identity.clone(),
                mask: None,
                system_mask: None,
                error: None,
                load_result: load_result.clone(),
                save_for_program,
                rule_key,
            });
            let actions = app.actions.clone();
            let worker_result = load_result.clone();
            let job = move || {
                let result = actions
                    .get_affinity_mask_checked(identity.pid, identity.start_epoch_s)
                    .and_then(|mask| {
                        actions
                            .system_affinity_mask()
                            .map(|system_mask| (mask, system_mask))
                    })
                    .map_err(|error| error.to_string());
                *tm_core::sync::lock(&worker_result) = Some(result);
            };
            let wake = {
                let ctx = ctx.clone();
                move || ctx.request_repaint()
            };
            match &app.shared.executor {
                Some(executor) => {
                    if !executor.run_quiet(wake, job) {
                        *tm_core::sync::lock(&load_result) =
                            Some(Err(i18n::tr(K::ActionQueueFull).to_string()));
                        ctx.request_repaint();
                    }
                }
                None => {
                    drop(job);
                    *tm_core::sync::lock(&load_result) =
                        Some(Err(i18n::tr(K::ActionFailed).to_string()));
                    ctx.request_repaint();
                }
            }
        }
        ui.close();
    }

    let suspended = p.status == ProcStatus::Suspended;
    let suspend_label = if suspended {
        i18n::tr(K::ResumeProc)
    } else {
        i18n::tr(K::SuspendProc)
    };
    if menu::item(ui, suspend_label).clicked() {
        if !identity_still_live(app, p) {
            app.shared.toast(i18n::tr(K::ProcessExited));
            ui.close();
            return;
        }
        let actions = app.actions.clone();
        let pid = p.pid;
        let start = p.start_epoch_s;
        let target_suspended = !suspended;
        app.run_action_refreshing(&ctx, String::new, move || {
            actions.suspend_process_checked(pid, start, target_suspended)
        });
        ui.close();
    }

    menu::separator(ui);

    #[cfg(target_os = "windows")]
    {
        let eco_on = p.power_throttled == Some(true);
        // A tick on one "Efficiency mode" entry, not two opposite verbs:
        // the menu shows the process's CURRENT state, the way Task Manager
        // and every other Windows menu toggle does.
        if menu::check_enabled(
            ui,
            i18n::tr(K::EfficiencyMode),
            eco_on,
            p.power_throttled.is_some(),
        )
        .on_disabled_hover_text(i18n::tr(K::NotAllowed))
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
        let uac_enabled = p.uac_virtualization == Some(UacVirtualization::Enabled);
        let uac_changeable = matches!(
            p.uac_virtualization,
            Some(UacVirtualization::Enabled | UacVirtualization::Disabled)
        );
        if menu::check_enabled(
            ui,
            i18n::tr(K::UacVirtualization),
            uac_enabled,
            uac_changeable,
        )
        .on_disabled_hover_text(i18n::tr(K::NotAllowed))
        .clicked()
        {
            if !identity_still_live(app, p) {
                app.shared.toast(i18n::tr(K::ProcessExited));
            } else {
                app.pending_uac_virtualization = Some(crate::app::PendingUacVirtualization {
                    identity: crate::app::ProcessIdentity {
                        pid: p.pid,
                        start_epoch_s: p.start_epoch_s,
                    },
                    name: p.shown_name().to_string(),
                    enabled: !uac_enabled,
                });
            }
            ui.close();
        }
        if menu::item(ui, i18n::tr(K::GoToServices)).clicked() {
            app.goto_services_for_pid(p.pid, &ctx);
            ui.close();
        }
    }

    if let Some(path) = p
        .exe_path
        .as_ref()
        .map(|x| x.to_string_lossy().into_owned())
    {
        if menu::item(ui, i18n::tr(K::OpenFileLocation)).clicked() {
            let actions = app.actions.clone();
            let path2 = path.clone();
            app.run_action(&ctx, String::new, move || {
                actions.open_file_location(&path2)
            });
            ui.close();
        }
        if menu::item(ui, i18n::tr(K::CreateDumpFile)).clicked() {
            create_dump(app, &ctx, p);
            ui.close();
        }
        if menu::item(ui, i18n::tr(K::Properties)).clicked() {
            if let Err(e) = app.actions.open_properties(&path) {
                tracing::debug!(error = %e, "shell properties failed; using built-in dialog");
                app.proc_props = Some(p.pid);
            }
            ui.close();
        }
    }
}

pub fn uac_virtualization_dialog(app: &mut TaskManApp, ctx: &egui::Context) {
    let Some(pending) = app.pending_uac_virtualization.clone() else {
        return;
    };
    let mut open = true;
    let mut decision = ctx.input(|input| {
        if input.key_pressed(egui::Key::Escape) {
            Some(false)
        } else if input.key_pressed(egui::Key::Enter) {
            Some(true)
        } else {
            None
        }
    });
    egui::Window::new(i18n::tr(K::UacVirtualization))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ctx, |ui| {
            ui.set_width(430.0);
            ui.label(i18n::trf(K::UacVirtualizationConfirm, &[&pending.name]));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::Cancel)).clicked() {
                    decision = Some(false);
                }
                if ui.button(i18n::tr(K::Apply)).clicked() {
                    decision = Some(true);
                }
            });
        });
    if !open {
        decision = Some(false);
    }
    if let Some(confirm) = decision {
        app.pending_uac_virtualization = None;
        if confirm {
            if !app.identity_is_live(&pending.identity) {
                app.shared.toast(i18n::tr(K::ProcessExited));
                return;
            }
            let actions = app.actions.clone();
            let identity = pending.identity;
            let enabled = pending.enabled;
            app.run_action_refreshing(
                ctx,
                || i18n::tr(K::UacVirtualizationChanged).to_string(),
                move || {
                    actions.set_uac_virtualization_checked(
                        identity.pid,
                        identity.start_epoch_s,
                        enabled,
                    )
                },
            );
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
    app.end_process_identity(
        ctx,
        crate::app::ProcessIdentity {
            pid,
            start_epoch_s: start,
        },
        tree,
        name.to_string(),
    );
}

pub(crate) fn create_dump(app: &mut TaskManApp, ctx: &egui::Context, p: &ProcessEntry) {
    if !identity_still_live(app, p) {
        app.shared.toast(i18n::tr(K::ProcessExited));
        return;
    }
    if !app.shared.dump_write.begin() {
        app.shared.toast(i18n::tr(K::DumpAlreadyRunning));
        return;
    }
    let default_name = format!("{}.dmp", p.shown_name());
    let Some(path) = rfd::FileDialog::new()
        .set_file_name(&default_name)
        .save_file()
    else {
        app.shared.dump_write.end();
        return;
    };
    let actions = app.actions.clone();
    let pid = p.pid;
    let start = p.start_epoch_s;
    let path_s = path.to_string_lossy().into_owned();
    let toasts = app.shared.toasts.clone();
    let in_flight = app.shared.dump_write.clone();
    let wake = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("tm-dump".into())
        .spawn(move || {
            let message = match actions.create_dump_file(pid, start, &path) {
                Ok(()) => i18n::trf(K::DumpWrittenMsg, &[&path_s]),
                Err(error) => i18n::trf(K::ErrMsg, &[&error.to_string()]),
            };
            crate::app::toast_from(&toasts, message);
            in_flight.end();
            wake.request_repaint();
        });
    if spawned.is_err() {
        app.shared.dump_write.end();
        app.shared.toast(i18n::tr(K::ActionFailed));
    }
}

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
                    ui.label(p.user.clone().unwrap_or_else(|| "—".into()));
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColPlatform));
                    ui.label(match p.wow64 {
                        Some(true) => i18n::tr(K::Bit32),
                        Some(false) => i18n::tr(K::Bit64),
                        None => "—",
                    });
                    ui.end_row();
                    ui.weak(i18n::tr(K::PropPath));
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

pub fn affinity_dialog(app: &mut TaskManApp, ctx: &egui::Context, pal: &theme::Palette) {
    let Some(mut dialog) = app.affinity_dialog.take() else {
        return;
    };
    if dialog.mask.is_none()
        && dialog.error.is_none()
        && let Some(result) = tm_core::sync::lock(&dialog.load_result).take()
    {
        match result {
            Ok((mask, system_mask)) => {
                dialog.mask = Some(mask);
                dialog.system_mask = Some(system_mask);
            }
            Err(error) => dialog.error = Some(error),
        }
    }
    let pid = dialog.identity.pid;
    let mut open = true;
    let mut apply = false;
    let mut cancel = false;
    egui::Window::new(format!("{} {pid}", i18n::tr(K::AffinityTitle)))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let controls_pal = theme::palette_ctx(ctx);
            match (
                dialog.mask.as_mut(),
                dialog.system_mask,
                dialog.error.as_deref(),
            ) {
                (_, _, Some(error)) => {
                    ui.colored_label(pal.heat_high, error);
                }
                (Some(mask), Some(system_mask), None) => {
                    egui::Grid::new("affinity")
                        .num_columns(8)
                        .spacing([6.0, 6.0])
                        .show(ui, |ui| {
                            for cpu in 0..64usize {
                                let allowed = system_mask & (1u64 << cpu) != 0;
                                let mut on = *mask & (1u64 << cpu) != 0;
                                if crate::widgets::controls::checkbox_enabled(
                                    ui,
                                    &mut on,
                                    &cpu.to_string(),
                                    allowed,
                                    &controls_pal,
                                )
                                .changed()
                                {
                                    if on {
                                        *mask |= 1u64 << cpu;
                                    } else {
                                        *mask &= !(1u64 << cpu);
                                    }
                                }
                                if (cpu + 1) % 8 == 0 {
                                    ui.end_row();
                                }
                            }
                        });
                    if *mask == 0 {
                        ui.label(
                            egui::RichText::new(i18n::tr(K::AffinityWarn))
                                .color(theme::DARK.heat_high),
                        );
                    }
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(i18n::tr(K::GatheringData));
                    });
                }
            }
            if dialog.error.is_none() {
                let save = crate::widgets::controls::checkbox_enabled(
                    ui,
                    &mut dialog.save_for_program,
                    i18n::tr(K::SaveAffinityForProgram),
                    dialog.rule_key.is_some() && dialog.mask.is_some(),
                    &controls_pal,
                )
                .on_disabled_hover_text(i18n::tr(K::SaveRuleNeedsPath));
                let _ = save;
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::Cancel)).clicked() {
                    cancel = true;
                }
                if ui
                    .add_enabled(
                        dialog.mask.is_some_and(|mask| mask != 0) && dialog.error.is_none(),
                        egui::Button::new(i18n::tr(K::Apply)),
                    )
                    .clicked()
                {
                    apply = true;
                }
            });
        });
    if apply {
        let actions = app.actions.clone();
        let identity = dialog.identity.clone();
        let mask = dialog.mask.expect("apply requires a loaded affinity mask");
        let toast_msg = || i18n::tr(K::AffinitySet).to_string();
        app.run_action(ctx, toast_msg, move || {
            actions.set_affinity_mask_checked(identity.pid, identity.start_epoch_s, mask)
        });
        if let Some(rule_key) = dialog.rule_key.as_deref() {
            update_saved_affinity(app, rule_key, dialog.save_for_program.then_some(mask));
        }
    } else if open && !cancel {
        app.affinity_dialog = Some(dialog);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_process(pid: u32, ppid: Option<u32>, name: &str) -> ProcessEntry {
        let mut process = ProcessEntry::new(pid, name);
        process.ppid = ppid;
        process.start_epoch_s = Some(pid as i64);
        process
    }

    fn tree_snapshot(processes: Vec<ProcessEntry>) -> tm_core::model::Snapshot {
        tm_core::model::Snapshot {
            timestamp_ms: 1,
            processes,
            ..Default::default()
        }
    }

    /// The tree is expanded by DEFAULT: an empty state must show the whole
    /// hierarchy, including subtrees whose parents appeared long after the
    /// page was first opened. Only explicit collapses hide anything.
    #[test]
    fn details_tree_is_fully_expanded_until_the_user_collapses() {
        let snapshot = tree_snapshot(vec![
            tree_process(10, None, "root.exe"),
            tree_process(11, Some(10), "child.exe"),
            tree_process(12, Some(11), "leaf.exe"),
        ]);
        let depths = |collapsed: HashSet<u32>| {
            build_rows(
                &snapshot,
                "",
                ColumnId::Name,
                SortOrder::Hierarchical,
                &collapsed,
            )
            .iter()
            .map(|row| (row.pid, row.depth))
            .collect::<Vec<_>>()
        };
        assert_eq!(depths(HashSet::new()), [(10, 0), (11, 1), (12, 2)]);
        assert_eq!(depths(HashSet::from([11])), [(10, 0), (11, 1)]);
        assert_eq!(depths(HashSet::from([10])), [(10, 0)]);
    }

    /// A recycled PID can make a process look like its own ancestor's child.
    /// A parent that started AFTER the child is not a parent.
    #[test]
    fn details_tree_rejects_parents_that_started_after_their_child() {
        let mut child = tree_process(500, Some(900), "orphan.exe");
        child.start_epoch_s = Some(100);
        let mut recycled = tree_process(900, None, "recycled.exe");
        recycled.start_epoch_s = Some(400);
        let rows = build_rows(
            &tree_snapshot(vec![recycled, child]),
            "",
            ColumnId::Name,
            SortOrder::Hierarchical,
            &HashSet::new(),
        );
        assert_eq!(
            rows.iter()
                .map(|row| (row.pid, row.depth))
                .collect::<Vec<_>>(),
            [(500, 0), (900, 0)],
            "both stay roots; the stale link must not nest them"
        );

        // The same shape with a plausible timeline DOES nest.
        let mut child = tree_process(500, Some(900), "child.exe");
        child.start_epoch_s = Some(400);
        let mut parent = tree_process(900, None, "parent.exe");
        parent.start_epoch_s = Some(100);
        let rows = build_rows(
            &tree_snapshot(vec![parent, child]),
            "",
            ColumnId::Name,
            SortOrder::Hierarchical,
            &HashSet::new(),
        );
        assert_eq!(
            rows.iter()
                .map(|row| (row.pid, row.depth))
                .collect::<Vec<_>>(),
            [(900, 0), (500, 1)]
        );
    }

    #[test]
    fn details_tree_filter_keeps_ancestors_and_breaks_cycles() {
        let snapshot = tree_snapshot(vec![
            tree_process(10, None, "root.exe"),
            tree_process(11, Some(10), "child.exe"),
            tree_process(12, Some(11), "needle.exe"),
            tree_process(20, None, "unrelated.exe"),
        ]);
        let filtered = build_rows(
            &snapshot,
            "needle",
            ColumnId::Name,
            SortOrder::Hierarchical,
            &HashSet::new(),
        );
        assert_eq!(
            filtered.iter().map(|row| row.pid).collect::<Vec<_>>(),
            [10, 11, 12]
        );

        let mut a = tree_process(30, Some(31), "a.exe");
        let mut b = tree_process(31, Some(30), "b.exe");
        // Same start time on both, so neither link is rejected as stale and
        // only the render walk's visited set can break the cycle.
        a.start_epoch_s = Some(1);
        b.start_epoch_s = Some(1);
        let cyclic = tree_snapshot(vec![a, b]);
        let rows = build_rows(
            &cyclic,
            "",
            ColumnId::Name,
            SortOrder::Hierarchical,
            &HashSet::new(),
        );
        let unique = rows.iter().map(|row| row.pid).collect::<HashSet<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(unique, HashSet::from([30, 31]));
    }

    /// The Name column's third state must be a LITERAL hierarchy: the sort
    /// column is ignored entirely and siblings appear in creation order. The
    /// other two states are plain FLAT lists — that is what "clicking a
    /// column leaves the tree" means, and it is why the same snapshot must
    /// come out in three visibly different orders.
    #[test]
    fn hierarchical_order_is_a_tree_and_every_other_order_is_flat() {
        let mut root = tree_process(10, None, "root.exe");
        root.start_epoch_s = Some(1);
        // Children created zulu-then-alpha — the reverse of the alphabet, so
        // creation order and name order cannot accidentally agree.
        let mut zulu = tree_process(11, Some(10), "zulu.exe");
        zulu.start_epoch_s = Some(2);
        let mut alpha = tree_process(12, Some(10), "alpha.exe");
        alpha.start_epoch_s = Some(3);
        let snapshot = tree_snapshot(vec![root, alpha, zulu]);

        let rows = |col, order| {
            build_rows(&snapshot, "", col, order, &HashSet::new())
                .iter()
                .map(|row| (row.pid, row.depth))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rows(ColumnId::Name, SortOrder::Hierarchical),
            [(10, 0), (11, 1), (12, 1)],
            "hierarchical order must nest and follow creation, not the alphabet"
        );
        assert_eq!(
            rows(ColumnId::Name, SortOrder::Ascending),
            [(12, 0), (10, 0), (11, 0)],
            "name ascending is a flat alphabetical list"
        );
        assert_eq!(
            rows(ColumnId::Name, SortOrder::Descending),
            [(11, 0), (10, 0), (12, 0)]
        );
        // Sorting by another column is flat too: the busiest process must be
        // able to reach the top instead of staying pinned under its parent.
        assert!(
            rows(ColumnId::Pid, SortOrder::Descending)
                .iter()
                .all(|(_, depth)| *depth == 0),
            "a non-Name column must never render a tree"
        );
    }

    /// Clicking Name cycles ascending -> descending -> hierarchy; clicking
    /// any other column is a plain two-way toggle AND leaves the tree, so the
    /// ordering is purely by that column.
    #[test]
    fn only_the_name_column_reaches_the_hierarchical_state() {
        let mut s = State::default();
        assert_eq!(s.sort_order, SortOrder::Ascending);
        assert_eq!(s.clicked_column(ColumnId::Name), SortOrder::Descending);
        assert_eq!(s.clicked_column(ColumnId::Name), SortOrder::Hierarchical);
        assert!(s.is_tree());
        assert_eq!(s.clicked_column(ColumnId::Name), SortOrder::Ascending);
        assert!(!s.is_tree());

        // Any other column: two-way only, never a tree.
        let mut n = State::default();
        assert_eq!(n.clicked_column(ColumnId::Cpu), SortOrder::Descending);
        assert_eq!(n.clicked_column(ColumnId::Cpu), SortOrder::Ascending);
        assert_eq!(n.clicked_column(ColumnId::Cpu), SortOrder::Descending);
        assert!(!n.is_tree());

        // Leaving the tree by clicking a different column.
        let mut t = State {
            sort_col: ColumnId::Name,
            sort_order: SortOrder::Hierarchical,
            ..State::default()
        };
        assert_eq!(t.clicked_column(ColumnId::Cpu), SortOrder::Descending);
        assert!(!t.is_tree(), "clicking CPU must leave the tree");
        assert_eq!(t.sort_col, ColumnId::Cpu);

        // No column is sorted in the hierarchical state, so no header arrow.
        assert_eq!(SortOrder::Hierarchical.arrow(), None);
        assert_eq!(SortOrder::Ascending.arrow(), Some(true));
        assert_eq!(SortOrder::Descending.arrow(), Some(false));

        // A persisted hierarchy flag under a column that cannot produce it
        // (hand-edited config) degrades to a normal direction.
        assert_eq!(
            SortOrder::from_saved(ColumnId::Cpu, false, true),
            SortOrder::Descending
        );
        assert_eq!(
            SortOrder::from_saved(ColumnId::Name, true, true),
            SortOrder::Hierarchical
        );
    }

    #[test]
    fn saved_sort_cannot_restore_a_hidden_column() {
        let mut state = State::default();
        state.set_visible(ColumnId::Cpu, false);
        let fallback = state.sort_col;
        state.apply_saved_sort("cpu", false, false);
        assert_eq!(state.sort_col, fallback);
        assert!(state.visible.contains(&state.sort_col));
    }

    #[test]
    fn details_every_column_sorts_its_own_field() {
        let mk = || ProcessEntry::new(100, "same.exe");
        for spec in COLUMNS {
            let (mut a, mut b) = (mk(), mk());
            assert_eq!(spec.cid.compare(&a, &b), CmpOrdering::Equal);
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
                ColumnId::Priority => {
                    a.priority = PriorityClass::Low;
                    b.priority = PriorityClass::High;
                }
                ColumnId::Threads => {
                    a.threads = Some(1);
                    b.threads = Some(2);
                }
                ColumnId::Handles => {
                    a.handles = Some(1);
                    b.handles = Some(2);
                }
                ColumnId::CpuTime => {
                    a.cpu_time_s = Some(1.0);
                    b.cpu_time_s = Some(2.0);
                }
                ColumnId::Commit => {
                    a.commit_bytes = Some(1);
                    b.commit_bytes = Some(2);
                }
                ColumnId::PeakMemory => {
                    a.peak_mem_bytes = Some(1);
                    b.peak_mem_bytes = Some(2);
                }
                ColumnId::GpuDedicated => {
                    a.gpu_dedicated_bytes = Some(1);
                    b.gpu_dedicated_bytes = Some(2);
                }
                ColumnId::GpuShared => {
                    a.gpu_shared_bytes = Some(1);
                    b.gpu_shared_bytes = Some(2);
                }
                ColumnId::Description => {
                    a.description = Some("a".into());
                    b.description = Some("b".into());
                }
                ColumnId::Publisher => {
                    a.company = Some("a".into());
                    b.company = Some("b".into());
                }
                ColumnId::ParentPid => {
                    a.ppid = Some(1);
                    b.ppid = Some(2);
                }
                ColumnId::SessionId => {
                    a.session_id = Some(1);
                    b.session_id = Some(2);
                }
                ColumnId::ImagePath => {
                    a.exe_path = Some("a".into());
                    b.exe_path = Some("b".into());
                }
                ColumnId::PageFaults => {
                    a.page_faults_per_s = Some(1);
                    b.page_faults_per_s = Some(2);
                }
                ColumnId::IoRead => {
                    a.disk_read_total = 1;
                    b.disk_read_total = 2;
                }
                ColumnId::IoWrite => {
                    a.disk_write_total = 1;
                    b.disk_write_total = 2;
                }
                ColumnId::CommandLine => {
                    a.command_line = Some("a".into());
                    b.command_line = Some("b".into());
                }
            }
            assert_ne!(
                spec.cid.compare(&a, &b),
                CmpOrdering::Equal,
                "{:?}",
                spec.cid
            );
        }
    }

    #[test]
    fn advanced_columns_are_hidden_by_default() {
        let s = State::default();
        assert!(!s.is_visible(ColumnId::Priority));
        assert!(!s.is_visible(ColumnId::GpuDedicated));
        assert!(s.is_visible(ColumnId::Name));
    }

    #[test]
    fn visibility_prefs_roundtrip_through_defaults() {
        let mut s = State::default();
        // Showing a default-hidden advanced column must persist…
        s.set_visible(ColumnId::Threads, true);
        assert_eq!(s.saved_visibility().get("threads"), Some(&true));
        let mut restored = State::default();
        restored.apply_saved_prefs(Some(&s.saved_visibility()), None);
        assert!(restored.is_visible(ColumnId::Threads));
        // …and so must hiding a default-visible one.
        s.set_visible(ColumnId::Uac, false);
        let vis = s.saved_visibility();
        assert_eq!(vis.get("uac"), Some(&false));
        // Unchanged columns produce no entry.
        assert!(!vis.contains_key("name"));
        let mut restored2 = State::default();
        restored2.apply_saved_prefs(Some(&vis), None);
        assert!(!restored2.is_visible(ColumnId::Uac));
        assert!(restored2.is_visible(ColumnId::Name));
    }

    #[test]
    fn order_prefs_survive_and_reorder() {
        let mut s = State::default();
        assert_eq!(s.saved_order(), default_order_ids(), "untouched = no entry");
        assert!(s.move_visible(ColumnId::Pid, -1));
        let order = s.saved_order();
        assert_ne!(order, default_order_ids());
        assert_eq!(order[0], "pid");
        let mut restored = State::default();
        restored.apply_saved_prefs(None, Some(&order));
        assert_eq!(restored.saved_order(), order);
        assert_eq!(restored.ordered_visible()[0].cid, ColumnId::Pid);
    }

    #[test]
    fn saved_order_skips_unknown_ids_and_appends_new_columns() {
        let mut s = State::default();
        let order = vec![
            "pid".to_string(),
            "name".to_string(),
            "future_column".to_string(),
        ];
        s.apply_saved_prefs(None, Some(&order));
        assert_eq!(s.ordered_visible()[0].cid, ColumnId::Pid);
        assert_eq!(s.ordered_visible()[1].cid, ColumnId::Name);
        // Columns missing from the file keep their built-in relative order.
        let rest: Vec<_> = s.order[2..].iter().copied().map(id_of).collect();
        assert_eq!(rest, default_order_ids()[2..]);
    }

    #[test]
    fn saved_prefs_never_empties_the_table_or_sorts_by_hidden_column() {
        // A hand-edited file that hides every column falls back to defaults.
        let all_hidden: BTreeMap<String, bool> = COLUMNS
            .iter()
            .map(|c| (c.id().to_string(), false))
            .collect();
        let mut s = State::default();
        s.apply_saved_prefs(Some(&all_hidden), None);
        assert!(!s.visible.is_empty(), "defaults survive an empty override");

        // Hiding the current sort column via prefs falls back to a visible
        // one, same as hiding it through the dialog.
        let mut s2 = State {
            sort_col: ColumnId::Uac,
            ..State::default()
        };
        s2.apply_saved_prefs(Some(&BTreeMap::from([("uac".to_string(), false)])), None);
        assert!(!s2.is_visible(ColumnId::Uac));
        assert!(s2.is_visible(s2.sort_col));
        assert_eq!(s2.sort_order, SortOrder::Ascending);
    }

    #[test]
    fn hiding_sorted_column_uses_a_visible_fallback() {
        let mut s = State::default();
        s.set_visible(ColumnId::Priority, true);
        s.sort_col = ColumnId::Priority;
        s.set_visible(ColumnId::Priority, false);
        assert!(s.is_visible(s.sort_col));
        assert_eq!(s.sort_order, SortOrder::Ascending);
    }

    #[test]
    fn active_columns_can_be_reordered() {
        let mut s = State::default();
        assert_eq!(s.ordered_visible()[0].cid, ColumnId::Name);
        assert!(s.move_visible(ColumnId::Pid, -1));
        assert_eq!(s.ordered_visible()[0].cid, ColumnId::Pid);
        assert!(!s.move_visible(ColumnId::Pid, -1));
    }

    #[test]
    fn uac_sort_ignores_priority() {
        let (mut a, mut b) = (mk_proc(1, "x.exe"), mk_proc(2, "x.exe"));
        a.priority = PriorityClass::Realtime;
        b.priority = PriorityClass::Low;
        a.uac_virtualization = Some(UacVirtualization::Enabled);
        b.uac_virtualization = Some(UacVirtualization::Enabled);
        assert_eq!(ColumnId::Uac.compare(&a, &b), CmpOrdering::Equal);
    }

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

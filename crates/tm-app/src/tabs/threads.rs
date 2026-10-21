//! Process Properties → Threads: the live thread list of one process, with a
//! "dormant" mark that rests on evidence rather than a guess.
//!
//! A thread is dormant when, for at least [`DORMANT_AFTER`], it was not on a
//! processor and neither its context-switch count, its CPU time nor its cycle
//! counter advanced between two kernel snapshots. Every scheduling of a thread
//! increments its context-switch count, so an unchanged count proves it never
//! got a processor; the CPU-time and cycle checks cover a thread that kept one
//! processor without ever being switched out. Until a thread has been watched
//! that long its activity reads "—": never observed is not dormant.

use eframe::egui;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_platform::actions::{ProcessModule, ProcessThread, ProcessThreads, ThreadState};

use crate::app::{ProcessIdentity, TaskManApp};
use crate::search;
use crate::tabs::inspector::{self, Poll};
use crate::theme;
use crate::widgets::tablekit::{self, TmColumn};

/// How long a thread must be watched without any sign of running before it
/// is called dormant.
pub const DORMANT_AFTER: Duration = Duration::from_secs(5);
const REFRESH_EVERY: Duration = Duration::from_secs(1);
const TABLE_ID: &str = "proc_threads";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// Not watched long enough to judge.
    Observing,
    /// Ran within the last [`DORMANT_AFTER`].
    Active,
    /// Provably did not run for at least `idle`.
    Dormant { idle: Duration },
}

impl Activity {
    fn rank(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Observing => 1,
            Self::Dormant { .. } => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThreadRow {
    pub thread: ProcessThread,
    /// Share of the machine's total CPU capacity since the previous
    /// snapshot — Task Manager's semantics. `None` on a thread's first sight.
    pub cpu_pct: Option<f32>,
    pub cycles_delta: Option<u64>,
    pub activity: Activity,
}

struct Track {
    created_epoch_s: Option<i64>,
    context_switches: u32,
    cpu_100ns: u64,
    cycles: Option<u64>,
    sampled: Instant,
    first_seen: Instant,
    last_active: Option<Instant>,
}

/// Per-thread history between snapshots: the deltas and the dormancy
/// evidence.
#[derive(Default)]
struct Tracker {
    tracks: HashMap<u32, Track>,
}

impl Tracker {
    fn apply(
        &mut self,
        threads: Vec<ProcessThread>,
        taken: Instant,
        logical_cpus: usize,
    ) -> Vec<ThreadRow> {
        let mut next = HashMap::with_capacity(threads.len());
        let rows = threads
            .into_iter()
            .map(|thread| {
                let cpu_100ns = thread
                    .kernel_time_100ns
                    .saturating_add(thread.user_time_100ns);
                // A TID names the same thread only while its creation time
                // matches; a recycled TID starts a fresh history.
                let previous = self
                    .tracks
                    .remove(&thread.tid)
                    .filter(|track| track.created_epoch_s == thread.created_epoch_s);
                let on_cpu = matches!(
                    thread.state,
                    ThreadState::Running
                        | ThreadState::Ready
                        | ThreadState::Standby
                        | ThreadState::DeferredReady
                );
                let (first_seen, mut last_active, cpu_pct, cycles_delta) = match &previous {
                    Some(prev) => {
                        let advanced = thread.context_switches != prev.context_switches
                            || cpu_100ns != prev.cpu_100ns
                            || matches!(
                                (thread.cycles, prev.cycles),
                                (Some(now), Some(before)) if now != before
                            );
                        let wall = taken.saturating_duration_since(prev.sampled).as_secs_f64();
                        let cpu_pct = (wall > 0.0 && logical_cpus > 0).then(|| {
                            let busy = cpu_100ns.saturating_sub(prev.cpu_100ns) as f64 / 1e7;
                            (busy / (wall * logical_cpus as f64) * 100.0).min(100.0) as f32
                        });
                        let cycles_delta = match (thread.cycles, prev.cycles) {
                            (Some(now), Some(before)) => Some(now.saturating_sub(before)),
                            _ => None,
                        };
                        let last_active = if advanced {
                            Some(taken)
                        } else {
                            prev.last_active
                        };
                        (prev.first_seen, last_active, cpu_pct, cycles_delta)
                    }
                    None => (taken, None, None, None),
                };
                if on_cpu {
                    last_active = Some(taken);
                }
                let idle = taken.saturating_duration_since(last_active.unwrap_or(first_seen));
                let activity = if last_active == Some(taken) {
                    Activity::Active
                } else if idle >= DORMANT_AFTER {
                    Activity::Dormant { idle }
                } else if last_active.is_some() {
                    Activity::Active
                } else {
                    Activity::Observing
                };
                next.insert(
                    thread.tid,
                    Track {
                        created_epoch_s: thread.created_epoch_s,
                        context_switches: thread.context_switches,
                        cpu_100ns,
                        cycles: thread.cycles,
                        sampled: taken,
                        first_seen,
                        last_active,
                    },
                );
                ThreadRow {
                    thread,
                    cpu_pct,
                    cycles_delta,
                    activity,
                }
            })
            .collect();
        self.tracks = next;
        rows
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Tid,
    Cpu,
    Cycles,
    Start,
    Priority,
    State,
    Activity,
}

impl SortColumn {
    const ALL: [Self; 7] = [
        Self::Tid,
        Self::Cpu,
        Self::Cycles,
        Self::Start,
        Self::Priority,
        Self::State,
        Self::Activity,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::Tid => "tid",
            Self::Cpu => "cpu",
            Self::Cycles => "cycles",
            Self::Start => "start",
            Self::Priority => "priority",
            Self::State => "state",
            Self::Activity => "activity",
        }
    }

    fn numeric(self) -> bool {
        matches!(self, Self::Tid | Self::Cpu | Self::Cycles)
    }
}

pub struct State {
    poll: Poll<(ProcessThreads, Instant)>,
    tracker: Tracker,
    rows: Vec<ThreadRow>,
    process_base_priority: Option<i32>,
    error: Option<String>,
    loaded: bool,
    selected: Option<u32>,
    scroll_to: Option<u32>,
    sort: SortColumn,
    ascending: bool,
    hide_dormant: bool,
}

impl State {
    pub fn new(app: &TaskManApp) -> Self {
        let saved = app.shared.settings.table_sort.get(TABLE_ID);
        let sort = saved
            .and_then(|saved| SortColumn::ALL.into_iter().find(|c| c.id() == saved.column))
            .unwrap_or(SortColumn::Tid);
        Self {
            poll: Poll::default(),
            tracker: Tracker::default(),
            rows: Vec::new(),
            process_base_priority: None,
            error: None,
            loaded: false,
            selected: None,
            scroll_to: None,
            sort,
            ascending: saved.is_none_or(|saved| saved.ascending),
            hide_dormant: false,
        }
    }

    /// The newest rows, for the Modules page's per-module activity.
    pub fn rows(&self) -> Option<&[ThreadRow]> {
        (self.loaded && self.error.is_none()).then_some(self.rows.as_slice())
    }
}

/// Keep the list current while a page that reads it is on screen.
pub fn tick(state: &mut State, app: &TaskManApp, ctx: &egui::Context, identity: &ProcessIdentity) {
    if let Some(result) = state.poll.take() {
        match result {
            Ok((snapshot, taken)) => {
                let cpus = app
                    .latest_snapshot()
                    .map(|snapshot| snapshot.cpu.logical_count)
                    .filter(|count| *count > 0)
                    .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
                state.process_base_priority = Some(snapshot.process_base_priority);
                state.rows = state.tracker.apply(snapshot.threads, taken, cpus);
                state.error = None;
            }
            Err(error) => state.error = Some(error),
        }
        state.loaded = true;
    }
    if !app.actions.capabilities().process_threads {
        return;
    }
    if state.poll.due(Instant::now(), Some(REFRESH_EVERY)) {
        let actions = app.actions.clone();
        let identity = identity.clone();
        state.poll.start(ctx, "tm-threads", move || {
            actions
                .list_process_threads(identity.pid, identity.start_epoch_s)
                .map(|threads| (threads, Instant::now()))
                .map_err(|error| error.to_string())
        });
    }
    ctx.request_repaint_after(REFRESH_EVERY);
}

/// `module.dll+0x1A2B` when the address lies inside a mapped module,
/// otherwise the bare address. No symbol files are consulted: export names
/// would mislabel the non-exported routines most threads start in.
pub fn symbolize(address: u64, modules: &[ProcessModule]) -> String {
    modules
        .iter()
        .find(|module| {
            address >= module.base_address && address - module.base_address < module.size_bytes
        })
        .map_or_else(
            || format!("0x{address:X}"),
            |module| format!("{}+0x{:X}", module.name, address - module.base_address),
        )
}

/// Windows' symbolic thread priority: the thread's base priority relative to
/// its process's base, with the saturated Idle/Time-critical levels.
fn priority_label(base: i32, process_base: Option<i32>) -> String {
    let relative = process_base.and_then(|process| match base - process {
        -2 => Some(K::ThreadPrioLowest),
        -1 => Some(K::PrioBelowNormal),
        0 => Some(K::PrioNormal),
        1 => Some(K::PrioAboveNormal),
        2 => Some(K::ThreadPrioHighest),
        _ => None,
    });
    let key = relative.or(match base {
        15 | 31 => Some(K::ThreadPrioTimeCritical),
        1 | 16 => Some(K::ThreadPrioIdle),
        _ => None,
    });
    match key {
        Some(key) => format!("{} ({base})", i18n::tr(key)),
        None => base.to_string(),
    }
}

fn state_label(thread: &ProcessThread) -> String {
    let key = match thread.state {
        ThreadState::Initialized => K::ThreadStateInitialized,
        ThreadState::Ready => K::ThreadStateReady,
        ThreadState::Running => K::ThreadStateRunning,
        ThreadState::Standby => K::ThreadStateStandby,
        ThreadState::Terminated => K::ThreadStateTerminated,
        ThreadState::Transition => K::ThreadStateTransition,
        ThreadState::DeferredReady => K::ThreadStateDeferredReady,
        ThreadState::Waiting => {
            let waiting = i18n::tr(K::ThreadStateWaiting);
            return match tm_platform::actions::wait_reason_name(thread.wait_reason) {
                Some(reason) => format!("{waiting} ({reason})"),
                None => waiting.to_string(),
            };
        }
        ThreadState::Other(raw) => return raw.to_string(),
    };
    i18n::tr(key).to_string()
}

pub fn activity_label(activity: Activity) -> &'static str {
    match activity {
        Activity::Observing => "—",
        Activity::Active => i18n::tr(K::ActivityActive),
        Activity::Dormant { .. } => i18n::tr(K::ActivityDormant),
    }
}

fn activity_tip(activity: Activity) -> String {
    match activity {
        Activity::Observing => i18n::trf(
            K::ActivityObservingTip,
            &[&DORMANT_AFTER.as_secs().to_string()],
        ),
        Activity::Active => i18n::trf(K::ThreadActiveTip, &[&DORMANT_AFTER.as_secs().to_string()]),
        Activity::Dormant { idle } => {
            i18n::trf(K::ThreadDormantTip, &[&idle.as_secs().to_string()])
        }
    }
}

struct View<'a> {
    row: &'a ThreadRow,
    start: String,
    priority: String,
    state: String,
}

fn compare(a: &View<'_>, b: &View<'_>, sort: SortColumn) -> Ordering {
    let primary = match sort {
        SortColumn::Tid => Ordering::Equal,
        SortColumn::Cpu => a
            .row
            .cpu_pct
            .unwrap_or(-1.0)
            .total_cmp(&b.row.cpu_pct.unwrap_or(-1.0)),
        SortColumn::Cycles => a.row.cycles_delta.cmp(&b.row.cycles_delta),
        SortColumn::Start => tablekit::cmp_ignore_case(&a.start, &b.start),
        SortColumn::Priority => a.row.thread.base_priority.cmp(&b.row.thread.base_priority),
        SortColumn::State => tablekit::cmp_ignore_case(&a.state, &b.state),
        SortColumn::Activity => a.row.activity.rank().cmp(&b.row.activity.rank()),
    };
    primary.then_with(|| a.row.thread.tid.cmp(&b.row.thread.tid))
}

pub fn show(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    state: &mut State,
    modules: Option<&[ProcessModule]>,
    pal: &theme::Palette,
) {
    let ctx = ui.ctx().clone();
    let dormant = state
        .rows
        .iter()
        .filter(|row| matches!(row.activity, Activity::Dormant { .. }))
        .count();
    ui.horizontal(|ui| {
        ui.label(i18n::trf(
            K::ThreadsSummary,
            &[&state.rows.len().to_string(), &dormant.to_string()],
        ));
        ui.add_space(12.0);
        let controls_pal = theme::palette_ctx(&ctx);
        let _ = crate::widgets::controls::checkbox_enabled(
            ui,
            &mut state.hide_dormant,
            i18n::tr(K::HideDormant),
            true,
            &controls_pal,
        );
        if state.poll.busy() && !state.loaded {
            ui.spinner();
        }
    });
    if let Some(error) = state.error.as_deref() {
        ui.colored_label(pal.heat_high, error);
    }
    if !state.loaded {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(i18n::tr(K::GatheringData));
        });
        return;
    }

    let no_modules: &[ProcessModule] = &[];
    let modules = modules.unwrap_or(no_modules);
    let mut views: Vec<View<'_>> = state
        .rows
        .iter()
        .filter(|row| !state.hide_dormant || !matches!(row.activity, Activity::Dormant { .. }))
        .map(|row| View {
            row,
            start: row
                .thread
                .start_address
                .map_or_else(|| "—".to_string(), |address| symbolize(address, modules)),
            priority: priority_label(row.thread.base_priority, state.process_base_priority),
            state: state_label(&row.thread),
        })
        .collect();
    views.sort_by(|a, b| tablekit::directed(compare(a, b, state.sort), state.ascending));

    let dialog_open = app.modal_open();
    search::set_active_content(&ctx, TABLE_ID);
    if let Some(nav) = search::list_nav(&ctx, dialog_open) {
        let current = state
            .selected
            .and_then(|tid| views.iter().position(|view| view.row.thread.tid == tid));
        let page_rows = tablekit::page_rows(&ctx, TABLE_ID, tablekit::ROW_H_DENSE).unwrap_or(10);
        if let Some(index) = search::moved_index(views.len(), current, nav, page_rows)
            && let Some(view) = views.get(index)
        {
            state.selected = Some(view.row.thread.tid);
            state.scroll_to = Some(view.row.thread.tid);
        }
    }

    let cols = vec![
        TmColumn::num("tid", i18n::tr(K::ColTid), 80.0),
        TmColumn::num("cpu", i18n::tr(K::ColCpu), 70.0),
        TmColumn::num("cycles", i18n::tr(K::ColCyclesDelta), 120.0),
        TmColumn::text("start", i18n::tr(K::ColStartAddress), 260.0),
        TmColumn::text("priority", i18n::tr(K::Priority), 140.0),
        TmColumn::text("state", i18n::tr(K::ColThreadState), 200.0),
        TmColumn::text("activity", i18n::tr(K::ColActivity), 100.0),
    ];
    let mut table = app
        .make_table(TABLE_ID, cols)
        .with_row_height(tablekit::ROW_H_DENSE);
    let sorted = SortColumn::ALL
        .iter()
        .position(|column| *column == state.sort)
        .unwrap_or(0);
    let focus = state
        .scroll_to
        .take()
        .and_then(|tid| views.iter().position(|view| view.row.thread.tid == tid));
    let avail = tablekit::table_avail(ui);
    let mut clicked_tid = None;
    let clicked = tablekit::scrolled_rows(
        TABLE_ID,
        ui,
        pal,
        &mut table,
        avail,
        Some((sorted, state.ascending)),
        None,
        views.len(),
        state.hide_dormant.then_some(i18n::tr(K::NoMatches)),
        focus,
        None,
        |ui, table, _avail, _content_width, range| {
            for index in range {
                let Some(view) = views.get(index) else {
                    continue;
                };
                let thread = &view.row.thread;
                let dim = matches!(view.row.activity, Activity::Dormant { .. });
                let selected = state.selected == Some(thread.tid);
                let (rect, response) = table.row(ui, pal, selected, ("thread", thread.tid));
                table.describe_row(ui, &response, &thread.tid.to_string(), index);
                inspector::num_cell(ui, table, rect, 0, &thread.tid.to_string(), pal, dim);
                let cpu = view
                    .row
                    .cpu_pct
                    .map_or_else(|| "—".to_string(), format::format_pct_cell);
                inspector::num_cell(ui, table, rect, 1, &cpu, pal, dim);
                let cycles = view
                    .row
                    .cycles_delta
                    .map_or_else(|| "—".to_string(), format::format_thousands);
                inspector::num_cell(ui, table, rect, 2, &cycles, pal, dim);
                let start_truncated = table.text_cell(ui, rect, 3, &view.start, pal, dim);
                table.text_cell(ui, rect, 4, &view.priority, pal, dim);
                let state_truncated = table.text_cell(ui, rect, 5, &view.state, pal, dim);
                table.text_cell(ui, rect, 6, activity_label(view.row.activity), pal, dim);

                let pointer = ui.ctx().pointer_latest_pos();
                let over = |column: usize| {
                    pointer.is_some_and(|p| table.col_rect(column, rect).contains(p))
                };
                let unreadable = thread.start_address.is_none() || thread.cycles.is_none();
                let tip = if over(6) {
                    Some(activity_tip(view.row.activity))
                } else if unreadable && (over(2) || over(3)) {
                    Some(i18n::tr(K::ThreadsNoAccessTip).to_string())
                } else if start_truncated && over(3) {
                    Some(view.start.clone())
                } else if state_truncated && over(5) {
                    Some(view.state.clone())
                } else {
                    None
                };
                let response = match tip {
                    Some(tip) => response.on_hover_text(tip),
                    None => response,
                };
                if response.clicked() || response.secondary_clicked() {
                    clicked_tid = Some(thread.tid);
                }
            }
        },
    );
    if let Some(tid) = clicked_tid {
        state.selected = Some(tid);
    }
    if let Some(column) = clicked.and_then(|index| SortColumn::ALL.get(index).copied()) {
        if state.sort == column {
            state.ascending = !state.ascending;
        } else {
            state.sort = column;
            // Measurements read best biggest-first, names A→Z.
            state.ascending = !column.numeric() || column == SortColumn::Tid;
        }
        app.persist_sort(TABLE_ID, state.sort.id(), state.ascending);
    }
    app.persist_table(&table);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(tid: u32, switches: u32, cpu: u64, state: ThreadState) -> ProcessThread {
        ProcessThread {
            tid,
            start_address: Some(0x1000),
            priority: 8,
            base_priority: 8,
            context_switches: switches,
            kernel_time_100ns: cpu,
            user_time_100ns: 0,
            created_epoch_s: Some(100),
            state,
            wait_reason: 6,
            cycles: Some(u64::from(switches) * 1000),
        }
    }

    const WAITING: ThreadState = ThreadState::Waiting;

    #[test]
    fn a_thread_is_dormant_only_after_being_watched_long_enough() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        let rows = tracker.apply(vec![thread(1, 5, 0, WAITING)], t0, 4);
        assert_eq!(rows[0].activity, Activity::Observing);
        assert_eq!(rows[0].cpu_pct, None);
        let rows = tracker.apply(
            vec![thread(1, 5, 0, WAITING)],
            t0 + Duration::from_secs(2),
            4,
        );
        assert_eq!(rows[0].activity, Activity::Observing);
        assert_eq!(rows[0].cycles_delta, Some(0));
        let rows = tracker.apply(vec![thread(1, 5, 0, WAITING)], t0 + DORMANT_AFTER, 4);
        assert_eq!(
            rows[0].activity,
            Activity::Dormant {
                idle: DORMANT_AFTER
            }
        );
    }

    #[test]
    fn any_sign_of_running_makes_a_thread_active_again() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        tracker.apply(vec![thread(1, 5, 0, WAITING)], t0, 4);
        tracker.apply(vec![thread(1, 5, 0, WAITING)], t0 + DORMANT_AFTER, 4);
        // A context switch.
        let at = t0 + DORMANT_AFTER + Duration::from_secs(1);
        let rows = tracker.apply(vec![thread(1, 6, 0, WAITING)], at, 4);
        assert_eq!(rows[0].activity, Activity::Active);
        // Still within the window after it ran: active, not observing.
        let rows = tracker.apply(
            vec![thread(1, 6, 0, WAITING)],
            at + Duration::from_secs(1),
            4,
        );
        assert_eq!(rows[0].activity, Activity::Active);
        // CPU time alone (no switch: it kept its processor).
        let mut busy = thread(2, 1, 0, WAITING);
        tracker.apply(vec![busy.clone()], t0, 4);
        busy.kernel_time_100ns = 10_000_000;
        let rows = tracker.apply(vec![busy], t0 + Duration::from_secs(1), 4);
        assert_eq!(rows[0].activity, Activity::Active);
        // 1 s of CPU over 1 s on 4 processors = 25 % of the machine.
        assert_eq!(rows[0].cpu_pct, Some(25.0));
        // Being on a processor right now is active on first sight.
        let rows = tracker.apply(vec![thread(3, 1, 0, ThreadState::Running)], t0, 4);
        assert_eq!(rows[0].activity, Activity::Active);
    }

    #[test]
    fn a_recycled_tid_starts_a_fresh_history() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        tracker.apply(vec![thread(1, 5, 0, WAITING)], t0, 4);
        let mut reborn = thread(1, 5, 0, WAITING);
        reborn.created_epoch_s = Some(200);
        let rows = tracker.apply(vec![reborn], t0 + DORMANT_AFTER, 4);
        assert_eq!(rows[0].activity, Activity::Observing);
        assert_eq!(rows[0].cycles_delta, None);
    }

    #[test]
    fn start_addresses_resolve_to_module_offsets() {
        let modules = [ProcessModule {
            name: "ntdll.dll".into(),
            path: "C:\\Windows\\System32\\ntdll.dll".into(),
            base_address: 0x7FF8_0000_0000,
            size_bytes: 0x20_0000,
        }];
        assert_eq!(symbolize(0x7FF8_0000_2C4D, &modules), "ntdll.dll+0x2C4D");
        assert_eq!(symbolize(0x7FF8_0020_0000, &modules), "0x7FF800200000");
    }

    #[test]
    fn priority_is_symbolic_relative_to_the_process() {
        assert!(priority_label(8, Some(8)).contains("(8)"));
        assert_eq!(
            priority_label(10, Some(8)),
            format!("{} (10)", i18n::tr(K::ThreadPrioHighest))
        );
        assert_eq!(
            priority_label(15, Some(8)),
            format!("{} (15)", i18n::tr(K::ThreadPrioTimeCritical))
        );
        assert_eq!(priority_label(12, None), "12");
    }
}

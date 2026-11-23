//! Process Properties → Threads: the live thread list of one process, with
//! activity statuses that rest on evidence rather than a guess.
//!
//! A thread is idle when, for at least [`DORMANT_AFTER`], it was not in the
//! kernel's Running state and neither its context-switch count, its CPU time
//! nor its cycle counter advanced between two kernel snapshots. Every
//! scheduling of a thread increments its context-switch count, so an unchanged
//! count proves it never got a processor; the CPU-time and cycle checks cover a
//! thread that kept one processor without ever being switched out. Ready and
//! Standby are runnable states, not proof that a thread executed.
//! Linux's `R` state is likewise only "runnable", not proof of current
//! execution. When neither a context-switch pair nor a cycle pair is
//! available, activity is reported as Unknown and never inferred as idle.
//!
//! Activity evidence is continuous only inside a bounded observation window.
//! If counters advanced across a long sampling gap (Paused, another properties
//! tab, sleep or a slow query), the exact last run is unknown and the thread is
//! re-observed instead of being called recent.

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
/// is called idle.
pub const DORMANT_AFTER: Duration = Duration::from_secs(5);
/// Fastest refresh; slower update speeds slow it down, Paused stops it.
pub(crate) const REFRESH_FLOOR: Duration = Duration::from_secs(1);
const TABLE_ID: &str = "proc_threads";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservingReason {
    FirstObservation,
    AfterGap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// The kernel reported this thread as using a processor in this sample.
    Running,
    /// Execution was observed inside a continuous recent sampling window.
    Recent,
    /// Evidence is still being collected.
    Observing {
        elapsed: Duration,
        reason: ObservingReason,
    },
    /// Scheduling evidence needed to distinguish recent activity from idle
    /// was unavailable for this sample.
    Unknown,
    /// No execution was observed for at least `idle`.
    Idle { idle: Duration },
}

impl Activity {
    fn rank(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Recent => 1,
            Self::Observing { .. } => 2,
            Self::Unknown => 3,
            Self::Idle { .. } => 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThreadRow {
    pub thread: ProcessThread,
    /// Share of the machine's total CPU capacity since the previous
    /// snapshot — Task Manager's semantics. `None` on a thread's first sight.
    pub cpu_pct: Option<f32>,
    /// Average share of machine CPU capacity across the whole observation
    /// window. `None` on a thread's first sight.
    pub avg_cpu_pct: Option<f32>,
    pub cycles_delta: Option<u64>,
    pub activity: Activity,
}

struct Track {
    created_epoch_s: Option<i64>,
    context_switches: Option<u64>,
    cpu_100ns: u64,
    cycles: Option<u64>,
    sampled: Instant,
    first_seen: Instant,
    first_cpu_100ns: u64,
    last_active: Option<Instant>,
    /// Start of the current continuously observed interval with usable
    /// scheduling evidence. Unknown samples reset it so their duration is not
    /// reported as proven idle time.
    idle_anchor: Instant,
    observing: Option<(Instant, ObservingReason)>,
}

/// Per-thread history between snapshots: the deltas and activity evidence.
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
                let running = thread.state == ThreadState::Running;
                let (
                    first_seen,
                    first_cpu_100ns,
                    mut last_active,
                    mut observing,
                    mut idle_anchor,
                    cpu_pct,
                    avg_cpu_pct,
                    cycles_delta,
                    evidence_unavailable,
                ) = match &previous {
                    Some(prev) => {
                        let switch_pair_available = matches!(
                            (thread.context_switches, prev.context_switches),
                            (Some(_), Some(_))
                        );
                        let cycle_pair_available =
                            matches!((thread.cycles, prev.cycles), (Some(_), Some(_)));
                        let advanced = matches!(
                            (thread.context_switches, prev.context_switches),
                            (Some(now), Some(before)) if now != before
                        ) || cpu_100ns != prev.cpu_100ns
                            || matches!(
                                (thread.cycles, prev.cycles),
                                (Some(now), Some(before)) if now != before
                            );
                        let wall = taken.saturating_duration_since(prev.sampled).as_secs_f64();
                        let cpu_pct = (wall > 0.0 && logical_cpus > 0).then(|| {
                            let busy = cpu_100ns.saturating_sub(prev.cpu_100ns) as f64 / 1e7;
                            (busy / (wall * logical_cpus as f64) * 100.0).min(100.0) as f32
                        });
                        let observed_wall = taken
                            .saturating_duration_since(prev.first_seen)
                            .as_secs_f64();
                        let avg_cpu_pct = (observed_wall > 0.0 && logical_cpus > 0).then(|| {
                            let busy = cpu_100ns.saturating_sub(prev.first_cpu_100ns) as f64 / 1e7;
                            (busy / (observed_wall * logical_cpus as f64) * 100.0).clamp(0.0, 100.0)
                                as f32
                        });
                        let cycles_delta = match (thread.cycles, prev.cycles) {
                            (Some(now), Some(before)) => Some(now.saturating_sub(before)),
                            _ => None,
                        };
                        let mut last_active = prev.last_active;
                        let mut observing = prev.observing;
                        let mut idle_anchor = prev.idle_anchor;
                        let sampling_gap =
                            taken.saturating_duration_since(prev.sampled) > DORMANT_AFTER;
                        let evidence_became_available = (thread.context_switches.is_some()
                            && prev.context_switches.is_none())
                            || (thread.cycles.is_some() && prev.cycles.is_none());
                        if advanced {
                            last_active = Some(taken);
                            idle_anchor = taken;
                            if sampling_gap {
                                observing = Some((taken, ObservingReason::AfterGap));
                            } else {
                                observing = None;
                            }
                        } else if evidence_became_available {
                            idle_anchor = taken;
                            observing = Some((taken, ObservingReason::AfterGap));
                        }
                        if evidence_unavailable {
                            idle_anchor = taken;
                        }
                        (
                            prev.first_seen,
                            prev.first_cpu_100ns,
                            last_active,
                            observing,
                            idle_anchor,
                            cpu_pct,
                            avg_cpu_pct,
                            cycles_delta,
                            !advanced && !switch_pair_available && !cycle_pair_available,
                        )
                    }
                    None => (
                        taken,
                        cpu_100ns,
                        None,
                        Some((taken, ObservingReason::FirstObservation)),
                        taken,
                        None,
                        None,
                        None,
                        false,
                    ),
                };
                if running {
                    last_active = Some(taken);
                    idle_anchor = taken;
                    observing = None;
                }
                let idle = taken.saturating_duration_since(idle_anchor);
                let activity = if running {
                    Activity::Running
                } else if evidence_unavailable {
                    Activity::Unknown
                } else if let Some((started, reason)) = observing {
                    let elapsed = taken.saturating_duration_since(started);
                    if elapsed < DORMANT_AFTER {
                        Activity::Observing { elapsed, reason }
                    } else if idle >= DORMANT_AFTER {
                        Activity::Idle { idle }
                    } else {
                        Activity::Recent
                    }
                } else if idle >= DORMANT_AFTER {
                    Activity::Idle { idle }
                } else if last_active.is_some() {
                    Activity::Recent
                } else {
                    Activity::Observing {
                        elapsed: Duration::ZERO,
                        reason: ObservingReason::FirstObservation,
                    }
                };
                let observing = if matches!(activity, Activity::Observing { .. }) {
                    observing
                } else {
                    None
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
                        first_cpu_100ns,
                        last_active,
                        idle_anchor,
                        observing,
                    },
                );
                ThreadRow {
                    thread,
                    cpu_pct,
                    avg_cpu_pct,
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
    CpuTime,
    Start,
    Priority,
    State,
    Activity,
}

impl SortColumn {
    const ALL: [Self; 7] = [
        Self::Tid,
        Self::Cpu,
        Self::CpuTime,
        Self::Start,
        Self::Priority,
        Self::State,
        Self::Activity,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::Tid => "tid",
            Self::Cpu => "cpu",
            Self::CpuTime => "cputime",
            Self::Start => "start",
            Self::Priority => "priority",
            Self::State => "state",
            Self::Activity => "activity",
        }
    }

    fn numeric(self) -> bool {
        matches!(self, Self::Tid | Self::Cpu | Self::CpuTime)
    }
}

pub struct State {
    poll: Poll<(ProcessThreads, Instant)>,
    tracker: Tracker,
    rows: Vec<ThreadRow>,
    process_base_priority: Option<i32>,
    error: Option<String>,
    last_sample: Option<Instant>,
    loaded: bool,
    selected: Option<u32>,
    scroll_to: Option<u32>,
    sort: SortColumn,
    ascending: bool,
    hide_idle: bool,
    filter: String,
    /// A new snapshot replaced the rows since the table last drew them.
    fresh: bool,
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
            last_sample: None,
            loaded: false,
            selected: None,
            scroll_to: None,
            sort,
            ascending: saved.is_none_or(|saved| saved.ascending),
            hide_idle: false,
            filter: String::new(),
            fresh: false,
        }
    }

    /// F5: query again now, whatever the update speed.
    pub fn request_refresh(&mut self) {
        self.poll.request();
    }

    /// The newest evidence, for the Modules page's per-module activity.
    pub fn evidence(&self) -> ThreadEvidence<'_> {
        if !self.loaded {
            return ThreadEvidence::Loading;
        }
        if let Some(error) = self.error.as_deref() {
            return ThreadEvidence::Unavailable {
                error,
                sampled_at: self.last_sample,
            };
        }
        match self.last_sample {
            Some(sampled_at) => ThreadEvidence::Ready {
                rows: &self.rows,
                sampled_at,
            },
            None => ThreadEvidence::Loading,
        }
    }
}

/// What the Modules page can use as thread-origin evidence.
#[derive(Debug, Clone, Copy)]
pub enum ThreadEvidence<'a> {
    Loading,
    Unavailable {
        error: &'a str,
        sampled_at: Option<Instant>,
    },
    Ready {
        rows: &'a [ThreadRow],
        sampled_at: Instant,
    },
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
                state.last_sample = Some(taken);
                state.fresh = true;
            }
            Err(error) => state.error = Some(error),
        }
        state.loaded = true;
    }
    if !app.actions.capabilities().process_threads {
        return;
    }
    let every = inspector::page_interval(app.shared.settings.update_speed, REFRESH_FLOOR);
    if state.poll.due(Instant::now(), every) {
        let actions = app.actions.clone();
        let identity = identity.clone();
        state.poll.start(ctx, "tm-threads", move || {
            let sampled_at = Instant::now();
            actions
                .list_process_threads(identity.pid, identity.start_epoch_s)
                .map(|threads| (threads, sampled_at))
                .map_err(|error| error.to_string())
        });
    }
    if let Some(every) = every {
        ctx.request_repaint_after(every);
    }
}

/// `module.dll+0x1A2B` when the address lies inside a mapped module,
/// otherwise the bare address. No symbol files are consulted: export names
/// would mislabel the non-exported routines most threads start in.
fn module_at(address: u64, modules: &[ProcessModule]) -> Option<&ProcessModule> {
    modules.iter().find(|module| {
        address >= module.base_address && address - module.base_address < module.size_bytes
    })
}

pub fn symbolize(address: u64, modules: &[ProcessModule]) -> String {
    module_at(address, modules).map_or_else(
        || format!("0x{address:X}"),
        |module| format!("{}+0x{:X}", module.name, address - module.base_address),
    )
}

/// Linux's scheduler policy is expressed directly as nice. Windows has a
/// richer symbolic priority ladder, shown relative to the process base.
fn priority_label(base: i32, process_base: Option<i32>, nice: Option<i32>) -> String {
    if let Some(nice) = nice {
        return format!("{} ({nice})", i18n::tr(K::ThreadPrioNice));
    }
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
        ThreadState::Runnable => K::ThreadStateRunnable,
        ThreadState::Standby => K::ThreadStateStandby,
        ThreadState::Terminated => K::ThreadStateTerminated,
        ThreadState::Transition => K::ThreadStateTransition,
        ThreadState::DeferredReady => K::ThreadStateDeferredReady,
        ThreadState::Waiting => {
            let waiting = i18n::tr(K::ThreadStateWaiting);
            return match thread
                .wait_reason
                .and_then(tm_platform::actions::wait_reason_name)
            {
                Some(reason) => format!("{waiting} ({reason})"),
                None => waiting.to_string(),
            };
        }
        ThreadState::Sleeping => K::ThreadStateSleeping,
        ThreadState::Stopped => K::ThreadStateStopped,
        ThreadState::Idle => K::ThreadStateIdle,
        ThreadState::Parked => K::ThreadStateParked,
        ThreadState::Other(raw) => return raw.to_string(),
    };
    i18n::tr(key).to_string()
}

fn cpu_100ns(thread: &ProcessThread) -> u64 {
    thread
        .kernel_time_100ns
        .saturating_add(thread.user_time_100ns)
}

fn cpu_time_secs(thread: &ProcessThread) -> f64 {
    cpu_100ns(thread) as f64 / 1e7
}

pub fn activity_label(activity: Activity) -> String {
    match activity {
        Activity::Running => i18n::tr(K::ActivityRunning).to_string(),
        Activity::Recent => i18n::tr(K::ActivityRecent).to_string(),
        Activity::Observing { elapsed, .. } => {
            i18n::trf(K::ActivityObservingFor, &[&elapsed.as_secs().to_string()])
        }
        Activity::Unknown => i18n::tr(K::ThreadActivityUnknown).to_string(),
        Activity::Idle { idle } => i18n::trf(K::ActivityIdleFor, &[&idle.as_secs().to_string()]),
    }
}

fn activity_tip(activity: Activity) -> String {
    match activity {
        Activity::Running => i18n::tr(K::ThreadRunningTip).to_string(),
        Activity::Recent => i18n::trf(K::ThreadRecentTip, &[&DORMANT_AFTER.as_secs().to_string()]),
        Activity::Observing {
            elapsed,
            reason: ObservingReason::FirstObservation,
        } => i18n::trf(
            K::ActivityObservingTip,
            &[
                &elapsed.as_secs().to_string(),
                &DORMANT_AFTER.as_secs().to_string(),
            ],
        ),
        Activity::Observing {
            reason: ObservingReason::AfterGap,
            ..
        } => i18n::tr(K::ActivityObservingGapTip).to_string(),
        Activity::Unknown => i18n::tr(K::ThreadActivityUnknownTip).to_string(),
        Activity::Idle { idle } => i18n::trf(K::ThreadIdleTip, &[&idle.as_secs().to_string()]),
    }
}

fn cpu_time_tip(row: &ThreadRow) -> String {
    let thread = &row.thread;
    let cycles = thread
        .cycles
        .map(format::format_thousands)
        .unwrap_or_else(|| "—".into());
    let cycles_delta = row
        .cycles_delta
        .map(format::format_thousands)
        .unwrap_or_else(|| "—".into());
    i18n::trf(
        K::ThreadCpuTimeTip,
        &[
            &format::format_cpu_time(cpu_time_secs(thread)),
            &format::format_cpu_time(thread.kernel_time_100ns as f64 / 1e7),
            &format::format_cpu_time(thread.user_time_100ns as f64 / 1e7),
            &cycles,
            &cycles_delta,
        ],
    )
}

fn cpu_pct_tip(row: &ThreadRow) -> String {
    let current = row
        .cpu_pct
        .map(format::format_pct_cell)
        .unwrap_or_else(|| "—".into());
    let average = row
        .avg_cpu_pct
        .map(format::format_pct_cell)
        .unwrap_or_else(|| "—".into());
    i18n::trf(K::ThreadCpuPercentTip, &[&current, &average])
}

struct View<'a> {
    row: &'a ThreadRow,
    start: String,
    priority: String,
    state: String,
    activity: String,
    cpu: String,
    cpu_time: String,
    filter_text: String,
}

impl View<'_> {
    fn matches_filter(&self, needle: &str) -> bool {
        self.filter_text.contains(needle)
    }
}

fn view_for<'a>(
    row: &'a ThreadRow,
    modules: &[ProcessModule],
    process_base_priority: Option<i32>,
) -> View<'a> {
    let start = row
        .thread
        .start_address
        .map_or_else(|| "—".to_string(), |address| symbolize(address, modules));
    let priority = priority_label(
        row.thread.base_priority,
        process_base_priority,
        row.thread.nice,
    );
    let state = state_label(&row.thread);
    let activity = activity_label(row.activity);
    let cpu = row
        .cpu_pct
        .map_or_else(|| "—".to_string(), format::format_pct_cell);
    let cpu_time = format::format_cpu_time(cpu_time_secs(&row.thread));
    let start_hex = row
        .thread
        .start_address
        .map(|address| format!("{address:x}"))
        .unwrap_or_default();
    let start_hex_prefixed = row
        .thread
        .start_address
        .map(|address| format!("0x{address:x}"))
        .unwrap_or_default();
    let module_path = row
        .thread
        .start_address
        .and_then(|address| module_at(address, modules))
        .map_or("", |module| module.path.as_str());
    let filter_text = format!(
        "{} {:x} 0x{:x} {start_hex} {start_hex_prefixed} {start} {module_path} {priority} {state} {activity} {cpu} {cpu_time}",
        row.thread.tid, row.thread.tid, row.thread.tid
    )
    .to_ascii_lowercase();
    View {
        row,
        start,
        priority,
        state,
        activity,
        cpu,
        cpu_time,
        filter_text,
    }
}

fn compare(a: &View<'_>, b: &View<'_>, sort: SortColumn) -> Ordering {
    let primary = match sort {
        SortColumn::Tid => Ordering::Equal,
        SortColumn::Cpu => a
            .row
            .cpu_pct
            .unwrap_or(-1.0)
            .total_cmp(&b.row.cpu_pct.unwrap_or(-1.0)),
        SortColumn::CpuTime => cpu_100ns(&a.row.thread).cmp(&cpu_100ns(&b.row.thread)),
        // Sort the address itself, not the module+offset presentation. Names
        // differ in length and alphabetic order bears no relation to address
        // order once addresses span multiple modules.
        SortColumn::Start => a.row.thread.start_address.cmp(&b.row.thread.start_address),
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
    let mut running = 0usize;
    let mut recent = 0usize;
    let mut idle = 0usize;
    let mut observing = 0usize;
    let mut unknown_activity = 0usize;
    let mut unavailable_details = 0usize;
    for row in &state.rows {
        if row.thread.start_address.is_none() || row.thread.cycles.is_none() {
            unavailable_details += 1;
        }
        match row.activity {
            Activity::Running => running += 1,
            Activity::Recent => recent += 1,
            Activity::Idle { .. } => idle += 1,
            Activity::Observing { .. } => observing += 1,
            Activity::Unknown => unknown_activity += 1,
        }
    }
    let now = Instant::now();
    let sample_age = state.last_sample.map(|sampled| {
        let age = now.saturating_duration_since(sampled);
        let paused = app.shared.settings.update_speed == tm_core::settings::UpdateSpeed::Paused;
        let expected = inspector::page_interval(app.shared.settings.update_speed, REFRESH_FLOOR)
            .map_or(DORMANT_AFTER, |interval| interval.saturating_mul(2));
        let text = if paused {
            i18n::trf(
                K::SamplePausedAge,
                &[&format::format_seconds(age.as_secs_f64())],
            )
        } else {
            i18n::trf(K::SampleAge, &[&format::format_seconds(age.as_secs_f64())])
        };
        (text, !paused && age > expected)
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.filter)
                .hint_text(i18n::tr(K::SearchHint))
                .desired_width(300.0),
        );
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(i18n::trf(
            K::ThreadsSummary,
            &[
                &state.rows.len().to_string(),
                &running.to_string(),
                &recent.to_string(),
                &idle.to_string(),
                &observing.to_string(),
            ],
        ));
        if unknown_activity > 0 {
            ui.separator();
            ui.label(
                egui::RichText::new(i18n::trf(
                    K::ThreadActivityUnknownSummary,
                    &[&unknown_activity.to_string(), &state.rows.len().to_string()],
                ))
                .small()
                .color(pal.text_dim),
            )
            .on_hover_text(i18n::tr(K::ThreadActivityUnknownTip));
        }
        if let Some((text, stale)) = &sample_age {
            ui.separator();
            let color = if *stale { pal.heat_high } else { pal.text_dim };
            ui.label(egui::RichText::new(text).small().color(color));
        }
        ui.add_space(8.0);
        let controls_pal = theme::palette_ctx(&ctx);
        let _ = crate::widgets::controls::checkbox_enabled(
            ui,
            &mut state.hide_idle,
            i18n::tr(K::HideIdle),
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
    if state.loaded && unavailable_details > 0 {
        ui.label(
            egui::RichText::new(i18n::trf(
                K::ThreadEvidencePartial,
                &[
                    &unavailable_details.to_string(),
                    &state.rows.len().to_string(),
                ],
            ))
            .small()
            .color(pal.text_dim),
        )
        .on_hover_text(i18n::tr(K::ThreadStartAddressTip));
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
    let filter = state.filter.trim().to_ascii_lowercase();
    let mut views: Vec<View<'_>> = state
        .rows
        .iter()
        .filter(|row| !state.hide_idle || !matches!(row.activity, Activity::Idle { .. }))
        .map(|row| view_for(row, modules, state.process_base_priority))
        .filter(|view| filter.is_empty() || view.matches_filter(&filter))
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
        TmColumn::num("cputime", i18n::tr(K::ColCpuTime), 110.0),
        TmColumn::text("start", i18n::tr(K::ColStartAddress), 260.0),
        TmColumn::text("priority", i18n::tr(K::Priority), 140.0),
        TmColumn::text("state", i18n::tr(K::ColThreadState), 200.0),
        TmColumn::text("activity", i18n::tr(K::ColActivity), 150.0),
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
    // Threads come and go every refresh; keep the rows on screen where they
    // are, preferring the selected one.
    let key_of = |index: usize| {
        views
            .get(index)
            .map_or(u64::MAX, |view| tablekit::stable_key(view.row.thread.tid))
    };
    let anchor = tablekit::ScrollAnchor {
        model_changed: std::mem::take(&mut state.fresh),
        prefer_key: state.selected.map(tablekit::stable_key),
        key_of: &key_of,
    };
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
        (!filter.is_empty() || state.hide_idle).then_some(i18n::tr(K::NoMatches)),
        focus,
        Some(anchor),
        |ui, table, _avail, _content_width, range| {
            for index in range {
                let Some(view) = views.get(index) else {
                    continue;
                };
                let thread = &view.row.thread;
                let dim = matches!(view.row.activity, Activity::Idle { .. });
                let selected = state.selected == Some(thread.tid);
                let (rect, response) = table.row(ui, pal, selected, ("thread", thread.tid));
                if response.hovered() {
                    table.set_hovered_cell_stable_rect(ui.ctx(), rect);
                }
                table.describe_row(ui, &response, &thread.tid.to_string(), index);
                inspector::num_cell(ui, table, rect, 0, &thread.tid.to_string(), pal, dim);
                inspector::num_cell(ui, table, rect, 1, &view.cpu, pal, dim);
                inspector::num_cell(ui, table, rect, 2, &view.cpu_time, pal, dim);
                let start_truncated = table.text_cell(ui, rect, 3, &view.start, pal, dim);
                table.text_cell(ui, rect, 4, &view.priority, pal, dim);
                let state_truncated = table.text_cell(ui, rect, 5, &view.state, pal, dim);
                table.text_cell(ui, rect, 6, &view.activity, pal, dim);

                let pointer = ui.ctx().pointer_latest_pos();
                let over = |column: usize| {
                    pointer.is_some_and(|p| table.col_rect(column, rect).contains(p))
                };
                let tip = if over(6) {
                    Some(activity_tip(view.row.activity))
                } else if over(2) {
                    Some(cpu_time_tip(view.row))
                } else if over(1) {
                    Some(cpu_pct_tip(view.row))
                } else if thread.start_address.is_none() && over(3) {
                    Some(i18n::tr(K::ThreadStartAddressTip).to_string())
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
            nice: None,
            context_switches: Some(u64::from(switches)),
            kernel_time_100ns: cpu,
            user_time_100ns: 0,
            created_epoch_s: Some(100),
            state,
            wait_reason: Some(6),
            cycles: Some(u64::from(switches) * 1000),
        }
    }

    const WAITING: ThreadState = ThreadState::Waiting;

    #[test]
    fn a_thread_is_idle_only_after_being_watched_long_enough() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        let rows = tracker.apply(vec![thread(1, 5, 0, WAITING)], t0, 4);
        assert_eq!(
            rows[0].activity,
            Activity::Observing {
                elapsed: Duration::ZERO,
                reason: ObservingReason::FirstObservation,
            }
        );
        assert_eq!(rows[0].cpu_pct, None);
        assert_eq!(rows[0].avg_cpu_pct, None);
        let rows = tracker.apply(
            vec![thread(1, 5, 0, WAITING)],
            t0 + Duration::from_secs(2),
            4,
        );
        assert_eq!(
            rows[0].activity,
            Activity::Observing {
                elapsed: Duration::from_secs(2),
                reason: ObservingReason::FirstObservation,
            }
        );
        assert_eq!(rows[0].cycles_delta, Some(0));
        let rows = tracker.apply(vec![thread(1, 5, 0, WAITING)], t0 + DORMANT_AFTER, 4);
        assert_eq!(
            rows[0].activity,
            Activity::Idle {
                idle: DORMANT_AFTER
            }
        );
    }

    #[test]
    fn missing_switch_and_cycle_evidence_never_becomes_idle() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        let mut unknown = thread(9, 5, 0, WAITING);
        unknown.context_switches = None;
        unknown.cycles = None;
        tracker.apply(vec![unknown.clone()], t0, 4);
        let t1 = t0 + DORMANT_AFTER + Duration::from_secs(1);
        let rows = tracker.apply(vec![unknown.clone()], t1, 4);
        assert_eq!(rows[0].activity, Activity::Unknown);

        // Once both samples carry a switch counter, begin a fresh bounded
        // observation window instead of inferring five seconds of idleness
        // from the unavailable interval.
        unknown.context_switches = Some(5);
        let t2 = t1 + Duration::from_secs(1);
        tracker.apply(vec![unknown.clone()], t2, 4);
        unknown.context_switches = Some(6);
        let t3 = t2 + Duration::from_secs(1);
        let rows = tracker.apply(vec![unknown.clone()], t3, 4);
        assert_eq!(rows[0].activity, Activity::Recent);
        let rows = tracker.apply(vec![unknown], t3 + DORMANT_AFTER, 4);
        assert_eq!(
            rows[0].activity,
            Activity::Idle {
                idle: DORMANT_AFTER
            }
        );
    }

    #[test]
    fn runnable_is_not_execution_evidence() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        let mut runnable = thread(10, 1, 0, ThreadState::Runnable);
        tracker.apply(vec![runnable.clone()], t0, 4);
        let rows = tracker.apply(vec![runnable.clone()], t0 + Duration::from_secs(1), 4);
        assert!(matches!(rows[0].activity, Activity::Observing { .. }));
        runnable.context_switches = Some(2);
        let rows = tracker.apply(vec![runnable], t0 + Duration::from_secs(2), 4);
        assert_eq!(rows[0].activity, Activity::Recent);
    }

    #[test]
    fn execution_evidence_makes_a_thread_recent_or_running() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        tracker.apply(vec![thread(1, 5, 0, WAITING)], t0, 4);
        tracker.apply(vec![thread(1, 5, 0, WAITING)], t0 + DORMANT_AFTER, 4);
        // A context switch.
        let at = t0 + DORMANT_AFTER + Duration::from_secs(1);
        let rows = tracker.apply(vec![thread(1, 6, 0, WAITING)], at, 4);
        assert_eq!(rows[0].activity, Activity::Recent);
        // Still within the window after it ran: recent, not observing.
        let rows = tracker.apply(
            vec![thread(1, 6, 0, WAITING)],
            at + Duration::from_secs(1),
            4,
        );
        assert_eq!(rows[0].activity, Activity::Recent);
        // CPU time alone (no switch: it kept its processor).
        let mut busy = thread(2, 1, 0, WAITING);
        tracker.apply(vec![busy.clone()], t0, 4);
        busy.kernel_time_100ns = 10_000_000;
        let rows = tracker.apply(vec![busy], t0 + Duration::from_secs(1), 4);
        assert_eq!(rows[0].activity, Activity::Recent);
        // 1 s of CPU over 1 s on 4 processors = 25 % of the machine.
        assert_eq!(rows[0].cpu_pct, Some(25.0));
        assert_eq!(rows[0].avg_cpu_pct, Some(25.0));
        // Running is execution evidence on first sight.
        let rows = tracker.apply(vec![thread(3, 1, 0, ThreadState::Running)], t0, 4);
        assert_eq!(rows[0].activity, Activity::Running);
    }

    #[test]
    fn runnable_states_are_not_execution_evidence() {
        for state in [
            ThreadState::Ready,
            ThreadState::Standby,
            ThreadState::DeferredReady,
        ] {
            let mut tracker = Tracker::default();
            let t0 = Instant::now();
            let rows = tracker.apply(vec![thread(1, 5, 0, state)], t0, 4);
            assert!(matches!(rows[0].activity, Activity::Observing { .. }));
            let rows = tracker.apply(vec![thread(1, 5, 0, state)], t0 + Duration::from_secs(1), 4);
            assert!(matches!(rows[0].activity, Activity::Observing { .. }));
            let rows = tracker.apply(vec![thread(1, 5, 0, state)], t0 + DORMANT_AFTER, 4);
            assert!(matches!(rows[0].activity, Activity::Idle { .. }));
        }

        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        tracker.apply(vec![thread(1, 5, 0, ThreadState::Ready)], t0, 4);
        let rows = tracker.apply(
            vec![thread(1, 6, 0, ThreadState::Ready)],
            t0 + Duration::from_secs(1),
            4,
        );
        assert_eq!(rows[0].activity, Activity::Recent);
    }

    #[test]
    fn activity_after_a_sampling_gap_is_reobserved() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        tracker.apply(vec![thread(1, 5, 0, WAITING)], t0, 4);
        let after_gap = t0 + Duration::from_secs(30);
        let rows = tracker.apply(vec![thread(1, 6, 0, WAITING)], after_gap, 4);
        assert_eq!(
            rows[0].activity,
            Activity::Observing {
                elapsed: Duration::ZERO,
                reason: ObservingReason::AfterGap,
            }
        );
        let rows = tracker.apply(
            vec![thread(1, 6, 0, WAITING)],
            after_gap + Duration::from_secs(1),
            4,
        );
        assert!(matches!(
            rows[0].activity,
            Activity::Observing {
                reason: ObservingReason::AfterGap,
                ..
            }
        ));
        let rows = tracker.apply(vec![thread(1, 6, 0, WAITING)], after_gap + DORMANT_AFTER, 4);
        assert_eq!(
            rows[0].activity,
            Activity::Idle {
                idle: DORMANT_AFTER
            }
        );
    }

    #[test]
    fn unchanged_counters_across_a_gap_keep_idle_evidence() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        tracker.apply(vec![thread(1, 5, 0, WAITING)], t0, 4);
        let rows = tracker.apply(
            vec![thread(1, 5, 0, WAITING)],
            t0 + Duration::from_secs(30),
            4,
        );
        assert_eq!(
            rows[0].activity,
            Activity::Idle {
                idle: Duration::from_secs(30)
            }
        );
    }

    #[test]
    fn a_recycled_tid_starts_a_fresh_history() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        tracker.apply(vec![thread(1, 5, 0, WAITING)], t0, 4);
        let mut reborn = thread(1, 5, 0, WAITING);
        reborn.created_epoch_s = Some(200);
        let rows = tracker.apply(vec![reborn], t0 + DORMANT_AFTER, 4);
        assert_eq!(
            rows[0].activity,
            Activity::Observing {
                elapsed: Duration::ZERO,
                reason: ObservingReason::FirstObservation,
            }
        );
        assert_eq!(rows[0].cycles_delta, None);
    }

    #[test]
    fn thread_cpu_time_is_kernel_plus_user_time() {
        let mut sample = thread(1, 1, 7_500_000, WAITING);
        sample.user_time_100ns = 2_500_000;
        assert_eq!(format::format_cpu_time(cpu_time_secs(&sample)), "0:00:01");
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
    fn start_address_sort_uses_the_numeric_address_not_the_module_name() {
        let mut numeric_low = thread(2, 1, 0, WAITING);
        numeric_low.start_address = Some(0x2_000);
        let mut numeric_high = thread(1, 1, 0, WAITING);
        numeric_high.start_address = Some(0x10_000);
        let row = |thread| ThreadRow {
            thread,
            cpu_pct: None,
            avg_cpu_pct: None,
            cycles_delta: None,
            activity: Activity::Idle {
                idle: DORMANT_AFTER,
            },
        };
        let low = row(numeric_low);
        let high = row(numeric_high);
        fn view<'a>(row: &'a ThreadRow, start: &str) -> View<'a> {
            View {
                row,
                start: start.to_string(),
                priority: String::new(),
                state: String::new(),
                activity: String::new(),
                cpu: String::new(),
                cpu_time: String::new(),
                filter_text: String::new(),
            }
        }
        // The alphabetic presentation strings deliberately disagree with
        // the numeric order.
        let low = view(&low, "z.dll+0x0");
        let high = view(&high, "a.dll+0x0");
        assert_eq!(compare(&low, &high, SortColumn::Start), Ordering::Less);
    }

    #[test]
    fn priority_is_symbolic_relative_to_the_process() {
        assert!(priority_label(8, Some(8), None).contains("(8)"));
        assert_eq!(
            priority_label(10, Some(8), None),
            format!("{} (10)", i18n::tr(K::ThreadPrioHighest))
        );
        assert_eq!(
            priority_label(15, Some(8), None),
            format!("{} (15)", i18n::tr(K::ThreadPrioTimeCritical))
        );
        assert_eq!(priority_label(12, None, None), "12");
        assert_eq!(
            priority_label(0, Some(0), Some(0)),
            format!("{} (0)", i18n::tr(K::ThreadPrioNice))
        );
    }

    #[test]
    fn thread_filter_matches_identity_state_activity_module_and_cpu_text() {
        let row = ThreadRow {
            thread: thread(4_660, 3, 0, ThreadState::Running),
            cpu_pct: Some(2.5),
            avg_cpu_pct: Some(1.25),
            cycles_delta: Some(100),
            activity: Activity::Recent,
        };
        let modules = [ProcessModule {
            name: "rtsshooks64.dll".into(),
            path: r"C:\Program Files\RTSS\Plugins\rtsshooks64.dll".into(),
            base_address: 0x1000,
            size_bytes: 0x5000,
        }];
        let view = view_for(&row, &modules, Some(8));
        assert_eq!(view.start, "rtsshooks64.dll+0x0");
        for needle in [
            "4660",
            "1234",
            "0x1234",
            "1000",
            "0x1000",
            "rtsshooks",
            "program files\\rtss\\plugins",
            &view.priority.to_ascii_lowercase(),
            &view.state.to_ascii_lowercase(),
            &view.activity.to_ascii_lowercase(),
            &view.cpu.to_ascii_lowercase(),
            &view.cpu_time.to_ascii_lowercase(),
        ] {
            assert!(view.matches_filter(needle), "{needle} should match");
        }
        assert!(!view.matches_filter("idle"));
    }
}

//! Root application state & layout: navigation rail, top search, per-tab
//! command bars, dialogs, toasts.
//!
//! Startup/responsiveness architecture (implement.md §4/§7):
//! * No collector construction here — the GUI gets actions cheaply and hands
//!   a lazy factory to the engine, which starts **after the first frame**.
//! * Engine publications and background-worker completions wake the UI via
//!   `Context::request_repaint`; there is no interval-based polling loop.
//! * App history loads asynchronously; settings/history writes run on single
//!   dedicated writer threads so no disk hiccup can hitch a frame.
//! * Short platform control actions (kill/priority/service/session/...) run
//!   on two bounded action lanes; long dump/module work gets lazy, dedicated
//!   workers so it cannot block unrelated controls.

use crate::action_executor::ActionExecutor;
use crate::app_ui::apply_theme;
use crate::widgets::tablekit::{TmColumn, TmTable};
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tm_core::demand::TelemetryDemand;
use tm_core::engine::EngineHandle;
use tm_core::i18n::{self, K};
use tm_core::model::Snapshot;
use tm_core::settings::{Settings, SettingsWriter};
use tm_platform::actions::PlatformActions;

/// One tick of history used to drive all Performance-tab charts.
#[derive(Debug, Clone, Default)]
pub struct HistoryPoint {
    pub t_ms: u64,
    pub cpu_total: f32,
    pub cpu_kernel: f32,
    pub per_core: Vec<f32>,
    pub per_core_kernel: Vec<f32>,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub commit_used_bytes: u64,
    #[allow(dead_code)] // kept for future committed-limit charts
    pub commit_limit_bytes: u64,
    pub disks: Vec<(String, f32, f64, f64)>, // mount, active%, read bps, write bps
    pub nets: Vec<(String, f64, f64)>,       // name, recv bps, sent bps
    pub gpus: Vec<(usize, f32, u64)>,        // id, util%, mem used
    /// Per-adapter, per-engine utilization: `(gpu id, engine name, util %)`.
    ///
    /// Kept alongside `gpus` rather than inside it because the engine set is
    /// discovered from PDH and can differ between ticks — a laptop's discrete
    /// GPU has no engine instances at all until something uses it. Names are
    /// owned per point for the same reason `disks` and `nets` own theirs: the
    /// alternative is an index into a registry that must then be kept in sync
    /// with a history buffer that outlives any single snapshot.
    pub gpu_engines: Vec<(usize, String, f32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Processes,
    Performance,
    AppHistory,
    Startup,
    Users,
    Details,
    Services,
}

impl Tab {
    pub const ALL: [Tab; 7] = [
        Tab::Processes,
        Tab::Performance,
        Tab::AppHistory,
        Tab::Startup,
        Tab::Users,
        Tab::Details,
        Tab::Services,
    ];

    /// Canonical key used by `--tab=`, the default-start-page setting and
    /// persistence.
    pub fn key(self) -> &'static str {
        match self {
            Tab::Processes => "processes",
            Tab::Performance => "performance",
            Tab::AppHistory => "apphistory",
            Tab::Startup => "startup",
            Tab::Users => "users",
            Tab::Details => "details",
            Tab::Services => "services",
        }
    }

    fn from_key(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.key() == s)
    }

    /// Localized label for the active UI language.
    pub fn label(self) -> &'static str {
        match self {
            Tab::Processes => i18n::tr(K::TabProcesses),
            Tab::Performance => i18n::tr(K::TabPerformance),
            Tab::AppHistory => i18n::tr(K::TabAppHistory),
            Tab::Startup => i18n::tr(K::TabStartup),
            Tab::Users => i18n::tr(K::TabUsers),
            Tab::Details => i18n::tr(K::TabDetails),
            Tab::Services => i18n::tr(K::TabServices),
        }
    }

    pub fn icon(self) -> crate::icons::Icon {
        use crate::icons::Icon::*;
        match self {
            Tab::Processes => Processes,
            Tab::Performance => Performance,
            Tab::AppHistory => History,
            Tab::Startup => Startup,
            Tab::Users => Users,
            Tab::Details => Details,
            Tab::Services => Services,
        }
    }
}

/// Exact process identity for selection/navigation/destructive actions —
/// never a bare PID (PID reuse, implement.md §11.5/§19.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_epoch_s: Option<i64>,
}

// ---------------------------------------------------------------- toasts

static TOAST_SEQ: AtomicU64 = AtomicU64::new(1);

/// A toast with a stable monotonic id (the old identity derived from elapsed
/// time changed across frames, breaking egui layout state).
#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub msg: String,
    pub born: std::time::Instant,
}

pub const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(4);
const MAX_TOASTS: usize = 6;

pub type ToastQueue = Mutex<Vec<Toast>>;

/// Push a toast from any thread.
pub fn toast_from(queue: &ToastQueue, msg: impl Into<String>) {
    let mut t = tm_core::sync::lock(queue);
    t.push(Toast {
        id: TOAST_SEQ.fetch_add(1, Ordering::Relaxed),
        msg: msg.into(),
        born: std::time::Instant::now(),
    });
    if t.len() > MAX_TOASTS {
        t.remove(0);
    }
}

// ---------------------------------------------------------------- guards

type CachedVec<T> = Arc<Mutex<Option<(Vec<T>, std::time::Instant)>>>;

/// Guard against duplicate background operations for one resource.
/// Cloneable so worker threads can clear their own flag when done.
#[derive(Default, Clone)]
pub struct InFlight(Arc<AtomicBool>);

impl InFlight {
    /// Returns true only for the caller that won the race.
    pub fn begin(&self) -> bool {
        !self.0.swap(true, Ordering::Relaxed)
    }
    pub fn end(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
    pub fn busy(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    /// Handle for a worker thread to call [`InFlight::end`] itself.
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}

/// Shared UI-side caches that individual tabs mutate. All mutable state lives
/// behind locks so worker threads can publish results without blocking frames.
pub struct SharedState {
    pub settings: Settings,
    /// Debounced disk writer for settings (single worker thread).
    pub settings_writer: SettingsWriter,
    /// Cached service/startup/user lists refreshed lazily in the background.
    pub services_cache: Arc<Mutex<Option<crate::tabs::services::Cache>>>,
    pub startup_cache: CachedVec<tm_core::model::StartupItem>,
    pub sessions_cache: CachedVec<tm_core::model::UserSession>,
    /// Shell icon textures keyed by executable path (lazy worker inside).
    pub icons: crate::icon_cache::IconCache,
    /// Toast queue. Workers push, the UI drains.
    pub toasts: Arc<ToastQueue>,
    /// Bounded executor for platform control actions (never the UI thread).
    /// `None` only if no worker thread could be created; controls then fail
    /// visibly while the monitor remains responsive.
    pub executor: Option<ActionExecutor>,
    // In-flight markers so slow platform queries run off the UI thread exactly once.
    pub services_fetch: InFlight,
    pub startup_fetch: InFlight,
    pub sessions_fetch: InFlight,
    pub service_control: InFlight,
    /// Guard for the dedicated, lazily spawned long-running dump worker.
    pub dump_write: InFlight,
}

impl SharedState {
    /// Push a toast from UI code.
    pub fn toast(&self, msg: impl Into<String>) {
        crate::app::toast_from(&self.toasts, msg);
    }

    pub fn service_control_busy(&self) -> bool {
        self.service_control.busy()
    }
}

impl Drop for SharedState {
    fn drop(&mut self) {
        // Final bounded flush so the last changes survive shutdown without
        // blocking arbitrarily long (implement.md §17.1).
        self.settings_writer.flush();
    }
}

/// Cross-tab navigation target: select exactly this process in Details.
#[derive(Debug, Clone)]
pub struct PendingDetailsFocus(pub ProcessIdentity);

/// Destructive action parked behind an explicit confirmation.
///
/// Carries every target, because a multi-row selection is exactly when a
/// confirmation earns its place: the Delete key over 30 selected rows would
/// otherwise be one keystroke away from ending 30 processes.
#[derive(Debug, Clone)]
pub struct PendingProcessEnd {
    /// Identity plus the display name to report, per target.
    pub targets: Vec<(ProcessIdentity, String)>,
    pub tree: bool,
}

/// Token virtualization changes can alter legacy file/registry behavior, so
/// the Details context menu parks them behind an explicit warning.
#[derive(Debug, Clone)]
pub struct PendingUacVirtualization {
    pub identity: ProcessIdentity,
    pub name: String,
    pub enabled: bool,
}

struct ProcessRuleResult {
    identity: ProcessIdentity,
    error: Option<String>,
}

#[cfg(target_os = "windows")]
struct AdvancedStateResult {
    core_service: tm_platform::actions::CoreServiceState,
    task_manager: tm_platform::actions::TaskManagerReplacementState,
}

pub struct TaskManApp {
    pub engine: EngineHandle,

    pub actions: Arc<dyn PlatformActions>,
    /// Elevation status of THIS process; fixed at process creation, so it is
    /// queried exactly once (settings dialog shows it and offers an elevated
    /// restart).
    pub is_elevated: bool,
    pub shared: SharedState,
    /// Rolling tick history for all Performance-tab charts. MUST stay a
    /// contiguous, append-ordered buffer (plain `Vec`): the Performance tab
    /// slices it directly. The former `VecDeque` wrapped its ring buffer
    /// after `history_cap` pop/push cycles, so the newest points moved into
    /// the discarded back half of `as_slices()` and the graphs froze (only
    /// rendering one stale-to-fresh blip per full ring cycle).
    pub history: Vec<HistoryPoint>,
    /// Cap derived from the configured graph window at the fastest interval.
    /// Recomputed whenever `graph_seconds` changes (audit §10) — a wider
    /// window setting must never silently truncate the visible history.
    pub history_cap: usize,
    pub app_history_db: tm_core::AppHistoryDb,
    pub tab: Tab,
    pub show_settings: bool,
    pub run_dialog_open: bool,
    pub run_dialog_text: String,
    pub run_elevated: bool,
    pub ticks_seen: u64,
    pub affinity_dialog: Option<crate::tabs::details::AffinityDialog>,
    pub startup_props: Option<tm_core::model::StartupItem>,
    /// Selection by stable item id (list indexes shift on refresh).
    pub selected_startup_id: Option<String>,
    /// Process-properties dialog target pid.
    pub proc_props: Option<u32>,
    /// On-demand System Informer-style loaded-module inspector.
    pub module_dialog: Option<crate::tabs::modules::State>,
    /// Cross-tab jump: services tab should select this service name.
    pub svc_jump: Arc<Mutex<Option<String>>>,

    /// Global top search ("Nach Namen, Herausgeber oder PID suchen").
    pub search: String,

    /// Caption appearance last pushed to DWM (`(caption rgb, dark)`).
    /// The native title bar is not repainted per frame — each DWM attribute
    /// recomposes the window frame — so it is only written when the theme
    /// actually changes.
    title_bar_applied: Option<([u8; 3], bool)>,

    // Tab states.
    pub processes_state: crate::tabs::processes::State,
    pub perf_selected_key: String,
    /// One-shot type-ahead scroll target for the Performance card column.
    pub perf_jump_to: Option<String>,
    pub details_state: crate::tabs::details::State,
    pub startup_sort: crate::widgets::tablekit::SortState,
    pub services_sort: crate::widgets::tablekit::SortState,
    pub users_sort: crate::widgets::tablekit::SortState,
    pub app_history_sort: crate::widgets::tablekit::SortState,
    /// Selected process by EXACT identity (audit §7): the toolbar's End Task
    /// and Efficiency commands validate start-time identity against the live
    /// snapshot before dispatching, so a recycled PID is never targeted.
    /// The process rows selected on Processes/Details. Shared by both tables
    /// so a selection survives switching between them.
    pub selection: crate::selection::Selection,
    /// "Select all" requested from an overflow menu. The menu runs before the
    /// row model for this frame exists, so the tab consumes the request once
    /// it knows what "all" currently means.
    pub select_all_requested: bool,
    pub selected_user: Option<u32>,
    /// Pending sign-out awaiting confirmation (session id + display name).
    /// Logoff is a high-blast-radius action and must never be one-click.
    pub pending_session_logoff: Option<(u32, String)>,
    /// Delete-key termination awaits confirmation here.
    pub pending_process_end: Option<PendingProcessEnd>,
    /// Details context-menu UAC virtualization change awaiting confirmation.
    pub pending_uac_virtualization: Option<PendingUacVirtualization>,
    // Services tab.
    pub services_selected_name: Option<String>,

    /// Pending cross-tab navigation ("Go to details" with exact identity).
    pub pending_details_focus: Option<PendingDetailsFocus>,
    /// Pid to scroll into view on the details tab (consumed when reached).
    pub scroll_to_pid: Option<u32>,

    /// Engine starts after the first presented frame (lazy start).
    engine_started: bool,
    /// Last demand bitmask shipped to the engine (send only on change).
    last_demand_bits: u64,

    /// Frame-rate diagnostics (TASKMAN_FPS_PROBE=1): forces continuous
    /// repaints, measures achieved fps against the display's refresh rate.
    pub fps_probe: bool,
    pub last_frame: Option<std::time::Instant>,
    pub fps_window_start: std::time::Instant,
    pub fps_frames: u32,
    pub fps_ema_ms: f64,
    pub display_hz: Option<f32>,
    /// App-history disk snapshots are intentionally less frequent than
    /// telemetry samples; shutdown still performs a synchronous flush.
    last_app_history_save: std::time::Instant,
    /// Exact process identities that already had their saved scheduling rule
    /// evaluated. Pruned every snapshot so the set stays bounded.
    process_rules_applied: HashSet<ProcessIdentity>,
    process_rules_inflight: HashSet<ProcessIdentity>,
    process_rule_failures: HashMap<ProcessIdentity, u8>,
    process_rule_results: Arc<Mutex<Vec<ProcessRuleResult>>>,
    process_rules_enabled: bool,
    /// Machine-wide integration state is queried off the UI thread. A stuck
    /// service/SCM must never freeze the Settings window.
    #[cfg(target_os = "windows")]
    pub core_service_state: Option<tm_platform::actions::CoreServiceState>,
    #[cfg(target_os = "windows")]
    pub task_manager_replacement_state: Option<tm_platform::actions::TaskManagerReplacementState>,
    #[cfg(target_os = "windows")]
    advanced_state_result: Arc<Mutex<Option<AdvancedStateResult>>>,
    #[cfg(target_os = "windows")]
    advanced_state_inflight: bool,
    #[cfg(target_os = "windows")]
    advanced_state_last_refresh: Option<std::time::Instant>,
    #[cfg(target_os = "windows")]
    pub core_service_change_inflight: Arc<AtomicBool>,
}

impl TaskManApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        mock: bool,
        settings: Settings,
        initial_tab: Option<String>,
    ) -> Self {
        // Publish the glyph-weight choice BEFORE the first visuals install,
        // so the very first frame already rasterizes at the chosen weight.
        crate::theme::set_text_smoothing(settings.text_smoothing);
        apply_theme(&cc.egui_ctx, settings.theme);
        if settings.ui_zoom != 1.0 {
            cc.egui_ctx.set_zoom_factor(settings.ui_zoom);
        }
        if settings.always_on_top {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                    egui::WindowLevel::AlwaysOnTop,
                ));
        }

        // Active UI language: resolved from the persisted choice against the
        // OS-detected locale.
        i18n::set_lang(settings.language.resolve());

        let ctx = cc.egui_ctx.clone();

        // ---- engine: parked until the first frame completes ----------------
        // The notifier makes sampling event-driven: every publication wakes
        // the UI exactly once instead of the old double-poll per interval.
        let interval = settings.update_speed.interval();
        let notifier: tm_core::engine::NotifyFn = {
            let ctx = ctx.clone();
            Arc::new(move || ctx.request_repaint())
        };
        let factory: tm_core::engine::CollectorFactory = if mock {
            Box::new(|| Box::new(tm_core::mock::MockCollector::new()))
        } else {
            Box::new(tm_platform::create_collector)
        };
        let (engine, _join) = tm_core::engine::spawn_lazy(factory, interval, Some(notifier))
            .expect("failed to spawn sampling engine");
        if settings.update_speed == tm_core::settings::UpdateSpeed::Paused {
            engine.pause();
        }
        crate::StartupTrace::mark("engine_spawned_lazy");

        // ---- cheap platform surface only -----------------------------------
        // Never construct a collector just to get actions (§4.3).
        let actions: Arc<dyn PlatformActions> = Arc::from(tm_platform::create_actions());
        let is_elevated = actions.is_elevated();

        // Frame-rate diagnostics: TASKMAN_FPS_PROBE=1 forces a continuous
        // frame stream and overlays/logs the achieved rate vs display Hz.
        let fps_probe = std::env::var("TASKMAN_FPS_PROBE")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let display_hz = if fps_probe {
            tm_platform::display_refresh_hz()
        } else {
            None
        };
        if fps_probe {
            tracing::info!(?display_hz, "fps probe active (continuous repaints)");
            // Keep the window visible: an occluded window gets its presents
            // throttled by the compositor, which would corrupt the
            // measurement.
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                    egui::WindowLevel::AlwaysOnTop,
                ));
        }
        // Diagnostics: open a dialog right away (TASKMAN_DIALOG=settings|run)
        // or select a Performance resource by key (TASKMAN_PERF=<key>) so UI
        // tests can capture them without input automation.
        let open_dialog = std::env::var("TASKMAN_DIALOG").unwrap_or_default();
        let (show_settings, run_dialog_open) = match open_dialog.as_str() {
            "settings" => (true, false),
            "run" => (false, true),
            _ => (false, false),
        };
        let perf_selected_key = std::env::var("TASKMAN_PERF").unwrap_or_else(|_| "cpu".into());

        // History capacity sized for the largest configured window at the
        // fastest interval (implement.md §14.3); recomputed on changes.
        let min_interval_s = history_min_interval_s();
        let history_cap = history_cap_for(settings.graph_seconds, min_interval_s);

        // App history loads on a worker; observations wait for it (ms-scale).
        let history_path = tm_core::settings::taskman_data_dir().join("app-history.json");
        let app_history_db = tm_core::AppHistoryDb::open_deferred(history_path);

        // Details tab column visibility/order live in the settings file;
        // apply them before the first frame so telemetry demand and the
        // first render already match the user's saved layout.
        let mut details_state = crate::tabs::details::State::default();
        details_state.apply_saved_prefs(
            settings.col_visible.get("details"),
            settings.col_order.get("details").map(Vec::as_slice),
        );
        // The Details tree is the Name column's third sort state, and that
        // state has no ascending flag to live in, so it rides in the general
        // section instead. It must be applied even when no `[sort]` entry
        // exists — a config migrated from the old `details_tree_view` switch
        // has exactly that shape, and dropping it would silently discard the
        // user's tree preference.
        let saved = settings.table_sort.get("details");
        details_state.apply_saved_sort(
            saved.map_or("name", |sort| sort.column.as_str()),
            saved.is_none_or(|sort| sort.ascending),
            settings.details_tree_hierarchical,
        );

        let mut processes_state = crate::tabs::processes::State::new();
        if let Some(sort) = restored_sort(
            &settings,
            "processes",
            &["name", "status", "cpu", "mem", "disk", "net"],
        ) {
            processes_state.sort_col = sort.column;
            processes_state.ascending = sort.ascending;
        }
        let startup_sort =
            restored_sort(&settings, "startup", &["name", "pub", "status", "impact"])
                .unwrap_or_else(|| crate::widgets::tablekit::SortState::new(0, true));
        let services_sort = restored_sort(
            &settings,
            "services",
            &["name", "pid", "desc", "status", "group"],
        )
        .unwrap_or_else(|| crate::widgets::tablekit::SortState::new(0, true));
        let users_sort = restored_sort(
            &settings,
            "users",
            &["user", "status", "cpu", "mem", "disk", "net"],
        )
        .unwrap_or_else(|| crate::widgets::tablekit::SortState::new(0, true));
        let app_history_sort =
            restored_sort(&settings, "apphistory", &["name", "cpu", "net", "notif"])
                .unwrap_or_else(|| crate::widgets::tablekit::SortState::new(1, false));

        // Start page: CLI/diagnostic override wins, otherwise the setting.
        let tab = initial_tab
            .as_deref()
            .and_then(tab_from_cli)
            .or_else(|| Tab::from_key(&settings.default_start_page))
            .unwrap_or(Tab::Processes);

        let toasts: Arc<ToastQueue> = Arc::new(Mutex::new(Vec::new()));
        let executor = ActionExecutor::start();
        if executor.is_none() {
            tracing::error!("action workers could not start; blocking controls are disabled");
        }

        Self {
            engine,
            actions,
            is_elevated,
            shared: SharedState {
                settings_writer: SettingsWriter::start(),
                settings,
                services_cache: Arc::new(Mutex::new(None)),
                startup_cache: Arc::new(Mutex::new(None)),
                sessions_cache: Arc::new(Mutex::new(None)),
                icons: crate::icon_cache::IconCache::default(),
                toasts,
                executor,
                services_fetch: InFlight::default(),
                startup_fetch: InFlight::default(),
                sessions_fetch: InFlight::default(),
                service_control: InFlight::default(),
                dump_write: InFlight::default(),
            },
            history: Vec::with_capacity(history_cap + 8),
            history_cap,
            app_history_db,
            tab,
            show_settings,
            run_dialog_open,
            run_dialog_text: String::new(),
            run_elevated: false,
            ticks_seen: 0,
            affinity_dialog: None,
            startup_props: None,
            selected_startup_id: None,
            proc_props: None,
            module_dialog: None,
            svc_jump: Arc::new(Mutex::new(None)),
            search: String::new(),
            processes_state,
            perf_selected_key,
            perf_jump_to: None,
            details_state,
            startup_sort,
            services_sort,
            users_sort,
            app_history_sort,
            selection: crate::selection::Selection::default(),
            select_all_requested: false,
            title_bar_applied: None,
            selected_user: None,
            pending_session_logoff: None,
            pending_process_end: None,
            pending_uac_virtualization: None,
            services_selected_name: None,
            pending_details_focus: None,
            scroll_to_pid: None,
            engine_started: false,
            last_demand_bits: 0,
            fps_probe,
            last_frame: None,
            fps_window_start: std::time::Instant::now(),
            fps_frames: 0,
            fps_ema_ms: 0.0,
            display_hz,
            last_app_history_save: std::time::Instant::now(),
            process_rules_applied: HashSet::new(),
            process_rules_inflight: HashSet::new(),
            process_rule_failures: HashMap::new(),
            process_rule_results: Arc::new(Mutex::new(Vec::new())),
            process_rules_enabled: !mock,
            #[cfg(target_os = "windows")]
            core_service_state: None,
            #[cfg(target_os = "windows")]
            task_manager_replacement_state: None,
            #[cfg(target_os = "windows")]
            advanced_state_result: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "windows")]
            advanced_state_inflight: false,
            #[cfg(target_os = "windows")]
            advanced_state_last_refresh: None,
            #[cfg(target_os = "windows")]
            core_service_change_inflight: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(target_os = "windows")]
    pub fn poll_advanced_state(&mut self, ctx: &egui::Context) {
        if let Some(result) = tm_core::sync::lock(&self.advanced_state_result).take() {
            self.core_service_state = Some(result.core_service);
            self.task_manager_replacement_state = Some(result.task_manager);
            self.advanced_state_inflight = false;
            self.advanced_state_last_refresh = Some(std::time::Instant::now());
        }

        let due = self
            .advanced_state_last_refresh
            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(3));
        if due && !self.advanced_state_inflight {
            let actions = self.actions.clone();
            let result = self.advanced_state_result.clone();
            let wake = ctx.clone();
            match std::thread::Builder::new()
                .name("tm-advanced-state".into())
                .spawn(move || {
                    let state = AdvancedStateResult {
                        core_service: actions.core_service_state(),
                        task_manager: actions.task_manager_replacement_state(),
                    };
                    *tm_core::sync::lock(&result) = Some(state);
                    wake.request_repaint();
                }) {
                Ok(_) => self.advanced_state_inflight = true,
                Err(error) => tracing::warn!(%error, "cannot start advanced-state query"),
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(3));
    }

    /// Pull the newest snapshot into local history buffers (called once per
    /// actual repaint — the engine notifier guarantees freshness).
    fn poll_engine(&mut self, ctx: &egui::Context) -> Option<Arc<Snapshot>> {
        let latest = self.engine.latest()?;
        if latest.timestamp_ms != self.history.last().map_or(0, |h| h.t_ms) {
            let pt = HistoryPoint {
                t_ms: latest.timestamp_ms,
                cpu_total: latest.cpu.utilization_pct,
                cpu_kernel: latest.cpu.kernel_pct,
                per_core: latest.cpu.per_core_pct.clone(),
                per_core_kernel: latest.cpu.per_core_kernel_pct.clone(),
                mem_used_bytes: latest.memory.used_bytes,
                mem_total_bytes: latest.memory.total_bytes,
                commit_used_bytes: latest.memory.commit_used_bytes,
                commit_limit_bytes: latest.memory.commit_total_bytes,
                disks: latest
                    .disks
                    .iter()
                    .map(|d| (d.mount.clone(), d.active_pct, d.read_bps, d.write_bps))
                    .collect(),
                nets: latest
                    .networks
                    .iter()
                    .map(|n| (n.name.clone(), n.recv_bps, n.sent_bps))
                    .collect(),
                gpus: latest
                    .gpus
                    .iter()
                    .map(|g| (g.id, g.util_pct, g.mem_used_bytes))
                    .collect(),
                gpu_engines: latest
                    .gpus
                    .iter()
                    .flat_map(|g| {
                        g.engines
                            .iter()
                            .map(move |e| (g.id, e.name.clone(), e.util_pct))
                    })
                    .collect(),
            };
            push_history_point(&mut self.history, self.history_cap, pt);

            // Feed the persistent app-history database.
            let interval_s = self.engine.interval().as_secs_f64().max(0.05);
            self.app_history_db.observe(&latest, interval_s);
            self.apply_saved_process_rules(&latest, ctx);
            // Rows that exited leave the selection with them, so the toolbar
            // never offers to end a process that is already gone and a
            // recycled pid can never inherit a selection.
            let live: HashSet<u32> = latest
                .processes
                .iter()
                .filter(|process| !process.synthetic)
                .map(|process| process.pid)
                .collect();
            if !self.selection.is_empty() {
                let snapshot = latest.clone();
                self.selection.retain_live(|identity| {
                    live.contains(&identity.pid)
                        && snapshot.process(identity.pid).is_some_and(|process| {
                            process.start_epoch_s.is_none()
                                || process.start_epoch_s == identity.start_epoch_s
                        })
                });
            }

            self.ticks_seen += 1;
            Some(latest)
        } else {
            None
        }
    }

    fn apply_saved_process_rules(&mut self, snapshot: &Snapshot, ctx: &egui::Context) {
        if !self.process_rules_enabled {
            return;
        }

        let live = snapshot
            .processes
            .iter()
            .filter_map(|process| {
                process.start_epoch_s.map(|start_epoch_s| ProcessIdentity {
                    pid: process.pid,
                    start_epoch_s: Some(start_epoch_s),
                })
            })
            .collect::<HashSet<_>>();
        self.process_rules_applied
            .retain(|identity| live.contains(identity));
        self.process_rules_inflight
            .retain(|identity| live.contains(identity));
        self.process_rule_failures
            .retain(|identity, _| live.contains(identity));

        let completed = std::mem::take(&mut *tm_core::sync::lock(&self.process_rule_results));
        for result in completed {
            self.process_rules_inflight.remove(&result.identity);
            if let Some(error) = result.error {
                let failures = self
                    .process_rule_failures
                    .entry(result.identity.clone())
                    .or_default();
                *failures = failures.saturating_add(1);
                if *failures == 3 {
                    tracing::warn!(
                        pid = result.identity.pid,
                        %error,
                        "saved process scheduling rule failed after three attempts"
                    );
                    self.shared.toast(i18n::trf(
                        K::SavedProcessRuleFailed,
                        &[&result.identity.pid.to_string(), &error],
                    ));
                }
            } else {
                self.process_rule_failures.remove(&result.identity);
                self.process_rules_applied.insert(result.identity);
            }
        }

        let mut pending = Vec::new();
        for process in &snapshot.processes {
            let (Some(start_epoch_s), Some(path)) = (process.start_epoch_s, &process.exe_path)
            else {
                continue;
            };
            let identity = ProcessIdentity {
                pid: process.pid,
                start_epoch_s: Some(start_epoch_s),
            };
            if self.process_rules_applied.contains(&identity)
                || self.process_rules_inflight.contains(&identity)
                || self
                    .process_rule_failures
                    .get(&identity)
                    .copied()
                    .unwrap_or(0)
                    >= 3
            {
                continue;
            }
            let key = tm_core::settings::process_rule_key(path);
            if let Some(rule) = self.shared.settings.process_rules.get(&key)
                && !rule.is_empty()
            {
                pending.push((identity, rule.clone()));
            }
        }

        for (identity, rule) in pending {
            self.process_rules_inflight.insert(identity.clone());
            let actions = self.actions.clone();
            let results = self.process_rule_results.clone();
            let dispatch_results = self.process_rule_results.clone();
            let completed_identity = identity.clone();
            let queued_identity = identity.clone();
            let job = move || {
                let outcome = (|| -> Result<(), tm_core::TmError> {
                    if let Some(priority) = rule.priority {
                        actions.set_priority_checked(
                            identity.pid,
                            identity.start_epoch_s,
                            priority,
                        )?;
                    }
                    if let Some(saved_mask) = rule.affinity_mask {
                        let system_mask = actions.system_affinity_mask()?;
                        let mask = saved_mask & system_mask;
                        if mask == 0 {
                            return Err(tm_core::TmError::platform(
                                "saved process affinity",
                                "saved mask has no processors available on this system",
                            ));
                        }
                        actions.set_affinity_mask_checked(
                            identity.pid,
                            identity.start_epoch_s,
                            mask,
                        )?;
                    }
                    Ok(())
                })();
                tm_core::sync::lock(&results).push(ProcessRuleResult {
                    identity: completed_identity,
                    error: outcome.err().map(|error| error.to_string()),
                });
            };
            let wake_ctx = ctx.clone();
            match &self.shared.executor {
                Some(executor) => {
                    if !executor.run_quiet(move || wake_ctx.request_repaint(), job) {
                        self.process_rules_inflight.remove(&queued_identity);
                        tm_core::sync::lock(&dispatch_results).push(ProcessRuleResult {
                            identity: queued_identity,
                            error: Some(i18n::tr(K::ActionQueueFull).to_string()),
                        });
                    }
                }
                None => {
                    drop(job);
                    self.process_rules_inflight.remove(&queued_identity);
                    tm_core::sync::lock(&dispatch_results).push(ProcessRuleResult {
                        identity: queued_identity,
                        error: Some(i18n::tr(K::ActionFailed).to_string()),
                    });
                }
            }
        }
    }

    /// Derive telemetry demand from the visible surface and ship it when it
    /// changes (implement.md §6.3). Cheap: one atomic command on change.
    fn update_demand(&mut self) {
        let mut d = TelemetryDemand::core(); // core + adapter rates + tokens
        // The Processes and App History pages both show per-process network,
        // which is an ETW session on Windows — only keep it running while one
        // of those pages is actually on screen.
        if matches!(self.tab, Tab::Processes | Tab::AppHistory) {
            d = d.union(TelemetryDemand::PROCESS_NET);
        }
        match self.tab {
            Tab::Performance => {
                d = d
                    .union(TelemetryDemand::DISK_RATE)
                    .union(TelemetryDemand::GPU_ADAPTER)
                    .union(TelemetryDemand::CPU_SPEED);
            }
            Tab::Details if self.details_state.requires_gpu_telemetry() => {
                d = d
                    .union(TelemetryDemand::PROCESS_GPU)
                    .union(TelemetryDemand::PROCESS_GPU_MEMORY)
                    .union(TelemetryDemand::GPU_ADAPTER);
            }
            _ => {}
        }
        if d.bits() != self.last_demand_bits {
            self.last_demand_bits = d.bits();
            self.engine.set_demand(d);
        }
    }

    /// Invalidate every tab-local cache and force one fresh sample (F5 /
    /// "Refresh now" must do real work in running AND paused mode).
    pub fn refresh_all(&mut self) {
        self.engine.request_refresh();
        *tm_core::sync::lock(&self.shared.services_cache) = None;
        *tm_core::sync::lock(&self.shared.startup_cache) = None;
        *tm_core::sync::lock(&self.shared.sessions_cache) = None;
        // Display caches rebuild on the next snapshot/tick generation.
        self.processes_state.invalidate();
        self.details_state.invalidate();
    }

    /// Queue a debounced autosave honoring the master switch.
    pub fn save_settings(&mut self) {
        if self.shared.settings.save_config {
            let snap = self.shared.settings.clone();
            self.shared.settings_writer.enqueue(&snap);
        }
    }

    /// Queue a save that ignores the autosave gate (reset, explicit close,
    /// toggling autosave itself).
    pub fn save_settings_forced(&mut self) {
        let snap = self.shared.settings.clone();
        self.shared.settings_writer.force(&snap);
    }

    /// Run a platform action on the executor with a localized result toast;
    /// completion wakes the UI. If worker creation failed, preserve UI
    /// responsiveness and report the action unavailable rather than running
    /// a potentially blocking platform call inline.
    pub fn run_action(
        &mut self,
        ctx: &egui::Context,
        success_msg: impl FnOnce() -> String + Send + 'static,
        job: impl FnOnce() -> Result<(), tm_core::TmError> + Send + 'static,
    ) -> bool {
        let toasts = self.shared.toasts.clone();
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        match &self.shared.executor {
            Some(executor) => executor.run(toasts, wake, success_msg, job),
            None => {
                drop(job);
                drop(success_msg);
                crate::app::toast_from(&toasts, i18n::tr(K::ActionFailed));
                wake();
                false
            }
        }
    }

    /// Like [`TaskManApp::run_action`], but forces one out-of-band sample
    /// once the action has COMPLETED.
    ///
    /// Process attributes the sampler caches per PID (priority, efficiency
    /// mode, UAC virtualization) are what a context menu ticks, so a control
    /// action must be followed by a fresh sample or the menu keeps showing
    /// the old value. Refreshing beside the job instead of behind it is a
    /// race that samples the still-unchanged process — actions run on the
    /// executor's worker threads, not here.
    pub fn run_action_refreshing(
        &mut self,
        ctx: &egui::Context,
        success_msg: impl FnOnce() -> String + Send + 'static,
        job: impl FnOnce() -> Result<(), tm_core::TmError> + Send + 'static,
    ) -> bool {
        let engine = self.engine.clone();
        self.run_action(ctx, success_msg, move || {
            let result = job();
            if result.is_ok() {
                engine.request_refresh();
            }
            result
        })
    }

    /// Shared shutdown path for both eframe trait signatures (the optional
    /// Glow context changes the signature at compile time).
    pub(crate) fn shutdown(&mut self) {
        // Persist final settings (honors the autosave gate; the gate choice
        // itself was already force-persisted when toggled).
        self.save_settings();
        self.shared.settings_writer.flush();
        // Final synchronous flush so history is not lost at shutdown.
        self.app_history_db.save();
    }
}

/// Parse a `--tab=` value (accepts both English and German aliases).
fn tab_from_cli(name: &str) -> Option<Tab> {
    match name.to_ascii_lowercase().as_str() {
        "processes" | "prozesse" => Some(Tab::Processes),
        "performance" | "leistung" => Some(Tab::Performance),
        "apphistory" | "history" | "appverlauf" => Some(Tab::AppHistory),
        "startup" | "autostart" => Some(Tab::Startup),
        "users" | "benutzer" => Some(Tab::Users),
        "details" => Some(Tab::Details),
        "services" | "dienste" => Some(Tab::Services),
        _ => None,
    }
}

fn restored_sort(
    settings: &Settings,
    table: &str,
    column_ids: &[&str],
) -> Option<crate::widgets::tablekit::SortState> {
    let saved = settings.table_sort.get(table)?;
    let column = column_ids.iter().position(|id| *id == saved.column)?;
    Some(crate::widgets::tablekit::SortState::new(
        column,
        saved.ascending,
    ))
}

impl eframe::App for TaskManApp {
    /// Data pass: runs before each `ui`, and while the window is hidden.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Adopt asynchronously loaded app history as soon as it arrives.
        self.app_history_db.poll_load();

        // Single poll per repaint — publications wake us via the notifier,
        // so no periodic request_repaint_after is needed anymore.
        // Recompute the history capacity when the graph window setting
        // changes (audit §10): a wider window must never be truncated by a
        // deque sized at startup.
        let want_cap =
            history_cap_for(self.shared.settings.graph_seconds, history_min_interval_s());
        if want_cap != self.history_cap {
            self.history_cap = want_cap;
            while self.history.len() > want_cap {
                self.history.remove(0);
            }
            tracing::debug!(cap = want_cap, "graph window changed; history resized");
        }
        if self.poll_engine(ctx).is_some()
            && self.last_app_history_save.elapsed() >= std::time::Duration::from_secs(30)
        {
            self.app_history_db.save_async();
            self.last_app_history_save = std::time::Instant::now();
        }

        // Ship telemetry-demand changes (tab switches etc.).
        self.update_demand();

        // Only animations need timed repaints; everything else is woken by
        // events (engine publication, worker completion, input).
        if self.fps_probe {
            ctx.request_repaint_after(std::time::Duration::from_millis(1));
        } else {
            let next_expiry = tm_core::sync::lock(&self.shared.toasts)
                .iter()
                .map(|t| TOAST_TTL.saturating_sub(t.born.elapsed()))
                .min();
            if let Some(wait) = next_expiry {
                ctx.request_repaint_after(wait.min(std::time::Duration::from_millis(33)));
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        crate::theme::ensure_visuals(&ctx);
        crate::fonts::poll_async_apply(&ctx);
        self.poll_engine(&ctx);

        let pal = crate::theme::palette_ctx(&ctx);
        self.sync_title_bar(&ctx, &pal, _frame);

        // ------------------------------------------------ top-level panels
        crate::app_ui::top_search_panel(self, ui, &pal);
        crate::app_ui::sidebar(self, ui, &pal);

        // ------------------------------------------------ main content
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(pal.window_bg))
            .show(ui, |ui| match self.tab {
                Tab::Processes => crate::tabs::processes::show(self, ui),
                Tab::Performance => crate::tabs::performance::show(self, ui),
                Tab::AppHistory => crate::tabs::apphistory::show(self, ui),
                Tab::Startup => crate::tabs::startup::show(self, ui),
                Tab::Users => crate::tabs::users::show(self, ui),
                Tab::Details => crate::tabs::details::show(self, ui),
                Tab::Services => crate::tabs::services::show(self, ui),
            });

        // ------------------------------------------------ dialogs & toasts
        if self.show_settings {
            crate::app_ui::settings_dialog(self, &ctx, &pal);
        }
        if self.run_dialog_open {
            crate::app_ui::run_task_dialog(self, &ctx, &pal);
        }
        if self.affinity_dialog.is_some() {
            crate::tabs::details::affinity_dialog(self, &ctx, &pal);
        }
        if self.pending_session_logoff.is_some() {
            crate::tabs::users::session_logoff_dialog(self, &ctx, &pal);
        }
        if self.pending_process_end.is_some() {
            crate::app_ui::process_end_dialog(self, &ctx);
        }
        if self.pending_uac_virtualization.is_some() {
            crate::tabs::details::uac_virtualization_dialog(self, &ctx);
        }
        if self.startup_props.is_some() {
            crate::tabs::startup::properties_dialog(self, &ctx, &pal);
        }
        if self.proc_props.is_some() {
            crate::tabs::details::process_properties_dialog(self, &ctx);
        }
        if self.module_dialog.is_some() {
            crate::tabs::modules::dialog(self, &ctx, &pal);
        }
        crate::app_ui::draw_toasts(self, &ctx);

        // Frame-rate diagnostics overlay (TASKMAN_FPS_PROBE=1).
        if self.fps_probe {
            self.update_fps_probe(&ctx);
        }

        // Global shortcuts: F5 refreshes data AND tab-local caches even when
        // sampling is paused (the engine forces exactly one sample).
        if ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            self.refresh_all();
        }

        // Native shortcut: Delete on Processes/Details requests process
        // termination. It is confirmation-only and never steals Delete from
        // a text editor or from another modal operation.
        let modal_open = self.show_settings
            || self.run_dialog_open
            || self.affinity_dialog.is_some()
            || self.pending_session_logoff.is_some()
            || self.pending_process_end.is_some()
            || self.pending_uac_virtualization.is_some()
            || self.startup_props.is_some()
            || self.proc_props.is_some()
            || self.module_dialog.is_some()
            || self.details_state.select_columns_open;
        if !modal_open
            && matches!(self.tab, Tab::Processes | Tab::Details)
            && !ctx.egui_wants_keyboard_input()
            && ctx.input(|i| i.key_pressed(egui::Key::Delete))
        {
            self.confirm_selected_process_end();
        }

        // Global search shortcut (audit §5): Alt+F as documented by native
        // Task Manager, plus Ctrl+F as most users expect. egui ignores
        // ctrl-modified characters inside text edits, so this cannot leak
        // an 'f' into whatever field currently holds focus.
        let search_focus =
            ctx.input(|i| i.key_pressed(egui::Key::F) && (i.modifiers.alt || i.modifiers.ctrl));
        if search_focus {
            let id = egui::Id::new("global-search");
            ctx.memory_mut(|m| m.request_focus(id));
        }

        // Track window size for persistence (only while remembering). A
        // maximized window's inner size is the monitor's, not the restore
        // geometry — keep the last normal size so a relaunch does not open
        // fullscreen-sized; maximized state itself persists separately.
        if self.shared.settings.remember_window {
            let (size, maximized) = ctx.input(|i| {
                (
                    i.viewport().inner_rect.map(|r| r.size()),
                    i.viewport().maximized.unwrap_or(false),
                )
            });
            if !maximized && let Some(sz) = size {
                self.shared.settings.window_size = [sz.x, sz.y];
            }
        }

        // First frame fully submitted → NOW start sampling. Everything below
        // this line in the process lifetime happens after the shell painted.
        if !self.engine_started {
            self.engine_started = true;
            self.engine.start();
            tracing::info!("engine started after first frame");
        }
    }

    #[cfg(feature = "glow")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }

    #[cfg(not(feature = "glow"))]
    fn on_exit(&mut self) {
        self.shutdown();
    }
}

/// Fastest sampling interval the engine can run at; used to size the
/// graph-history buffer conservatively (never more points than needed).
pub(crate) fn history_min_interval_s() -> f64 {
    tm_core::settings::UpdateSpeed::High
        .interval()
        .as_secs_f64()
}

/// Deque capacity for `seconds` of visible history sampled at the fastest
/// configured speed. Split out for unit testing (audit §10).
pub(crate) fn history_cap_for(seconds: u32, min_interval_s: f64) -> usize {
    ((seconds as f64 / min_interval_s).ceil() as usize).clamp(120, 1440)
}

/// Append one tick to the rolling history, honoring the cap. Split out so
/// the ring-wrap regression test exercises the REAL retention code path.
/// The history must stay a contiguous, append-ordered `Vec` — see the field
/// doc on [`TaskManApp::history`].
pub(crate) fn push_history_point(history: &mut Vec<HistoryPoint>, cap: usize, pt: HistoryPoint) {
    if history.len() >= cap {
        history.remove(0);
    }
    history.push(pt);
}

impl TaskManApp {
    pub fn latest_snapshot(&self) -> Option<Arc<Snapshot>> {
        self.engine.latest()
    }

    /// Whether a stored process identity still matches the live snapshot
    /// (start-time check; audit §7). `None` start times degrade gracefully:
    /// presence alone is then accepted.
    pub fn identity_is_live(&self, identity: &ProcessIdentity) -> bool {
        self.latest_snapshot()
            .as_ref()
            .and_then(|s| s.process(identity.pid))
            .is_some_and(|p| {
                !p.synthetic
                    && (p.start_epoch_s.is_none() || p.start_epoch_s == identity.start_epoch_s)
            })
    }

    /// The active UI language.
    pub fn lang(&self) -> i18n::Lang {
        i18n::lang()
    }

    /// Build a table with this tab's persisted column widths restored.
    pub fn make_table(&self, id: &'static str, cols: Vec<TmColumn>) -> TmTable {
        let saved = self.shared.settings.col_widths.get(id);
        TmTable::new(id, cols, saved)
    }

    /// Persist resized column widths through the debounced settings writer.
    ///
    /// Every logical width change marks the snapshot dirty; coalescing in
    /// the writer (~250 ms) replaces the old mouse-up save, which depended
    /// on fragile previous-frame gesture state.
    pub fn persist_table(&mut self, table: &TmTable) {
        if table.changed_this_frame() {
            self.shared
                .settings
                .col_widths
                .insert(table.id.to_string(), table.stored_widths());
            self.save_settings();
        }
    }

    /// Persist a table sort by stable identifiers (never by display index,
    /// because optional/reordered columns can change that index).
    pub fn persist_sort(&mut self, table: &str, column: &str, ascending: bool) {
        let next = tm_core::settings::SortPreference {
            column: column.to_string(),
            ascending,
        };
        if self.shared.settings.table_sort.get(table) == Some(&next) {
            return;
        }
        self.shared
            .settings
            .table_sort
            .insert(table.to_string(), next);
        self.save_settings();
    }

    /// Jump to the services tab and highlight the service hosted by `pid`.
    /// Windows-only callers today (service→PID lookup); kept compiled on all
    /// targets so the API surface stays identical.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn goto_services_for_pid(&mut self, pid: u32, ctx: &egui::Context) {
        self.tab = crate::app::Tab::Services;
        let actions = self.actions.clone();
        let jump = self.svc_jump.clone();
        let toasts = self.shared.toasts.clone();
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        let job = move || match actions.list_services() {
            Ok(list) => {
                if let Some(svc) = list.iter().find(|s| s.pid == Some(pid)) {
                    *tm_core::sync::lock(&jump) = Some(svc.name.clone());
                } else {
                    crate::app::toast_from(
                        &toasts,
                        tm_core::i18n::trf(tm_core::i18n::K::NoServiceForPid, &[&pid.to_string()]),
                    );
                }
            }
            Err(e) => crate::app::toast_from(
                &toasts,
                tm_core::i18n::trf(tm_core::i18n::K::ErrMsg, &[&e.to_string()]),
            ),
        };
        match &self.shared.executor {
            Some(executor) => {
                if !executor.run_quiet(wake, job) {
                    self.shared.toast(i18n::tr(K::ActionQueueFull));
                }
            }
            None => {
                drop(job);
                self.shared.toast(i18n::tr(K::ActionFailed));
            }
        }
    }

    /// End one exact process identity on the short-action executor.
    pub fn end_process_identity(
        &mut self,
        ctx: &egui::Context,
        identity: ProcessIdentity,
        tree: bool,
        name: String,
    ) {
        if !self.identity_is_live(&identity) {
            self.shared.toast(i18n::tr(K::ProcessExited));
            return;
        }
        let pid = identity.pid;
        let start = identity.start_epoch_s;
        let actions = self.actions.clone();
        self.run_action(
            ctx,
            move || {
                if tree {
                    i18n::trf(K::TreeOfEndedToast, &[&name])
                } else {
                    i18n::trf(K::NameEndedToast, &[&name])
                }
            },
            move || actions.kill_process(pid, start, tree),
        );
    }

    /// Resolve every selected row to a live identity plus its display name,
    /// dropping the ones that exited (and the synthetic CPU-attribution
    /// pseudo-rows, which own no process at all).
    pub fn live_selection_targets(&self) -> Vec<(ProcessIdentity, String)> {
        let Some(snapshot) = self.latest_snapshot() else {
            return Vec::new();
        };
        self.selection
            .all()
            .iter()
            .filter(|identity| self.identity_is_live(identity))
            .filter_map(|identity| {
                snapshot
                    .process(identity.pid)
                    .filter(|process| !process.synthetic)
                    .map(|process| (identity.clone(), process.shown_name().to_string()))
            })
            .collect()
    }

    /// Park the selected live processes behind the Delete-key confirmation.
    fn confirm_selected_process_end(&mut self) {
        let targets = self.live_selection_targets();
        if targets.is_empty() {
            if !self.selection.is_empty() {
                self.selection.clear();
                self.shared.toast(i18n::tr(K::ProcessExited));
            }
            return;
        }
        self.pending_process_end = Some(PendingProcessEnd {
            targets,
            tree: false,
        });
    }

    /// End a whole confirmed batch.
    ///
    /// ONE job on the executor, not one per target. The executor is bounded at
    /// 64 queued jobs on purpose, so a 200-row selection would overflow it and
    /// answer with a toast storm; a single job also means a single summary
    /// toast and a single post-action refresh. Individual refusals are counted
    /// rather than aborting the batch — a protected process in the middle of a
    /// selection must not stop the rest.
    pub fn end_process_batch(
        &mut self,
        ctx: &egui::Context,
        mut targets: Vec<(ProcessIdentity, String)>,
        tree: bool,
    ) {
        if targets.len() == 1 {
            let (identity, name) = targets.remove(0);
            self.end_process_identity(ctx, identity, tree, name);
            return;
        }
        let total = targets.len();
        let actions = self.actions.clone();
        let ended = Arc::new(AtomicU64::new(0));
        let counter = ended.clone();
        self.run_action_refreshing(
            ctx,
            move || {
                // Evaluated on the worker AFTER the job, so the count is final.
                let ok = ended.load(Ordering::Relaxed) as usize;
                if ok == total {
                    i18n::trf(K::ProcessesEndedToast, &[&total.to_string()])
                } else {
                    i18n::trf(
                        K::ProcessesEndedPartial,
                        &[&ok.to_string(), &total.to_string()],
                    )
                }
            },
            move || {
                for (identity, _) in targets {
                    if actions
                        .kill_process(identity.pid, identity.start_epoch_s, tree)
                        .is_ok()
                    {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(())
            },
        );
    }

    /// Set efficiency mode on a whole selection, to ONE state.
    ///
    /// Toggling each row against its own current state would leave a mixed
    /// selection mixed the other way round, which is never what "turn this on
    /// for these" means. The state comes from `on`; callers derive it from the
    /// row the user acted on. Batched into one executor job for the same
    /// reasons as [`Self::end_process_batch`].
    pub fn set_efficiency_mode_batch(
        &mut self,
        ctx: &egui::Context,
        targets: Vec<ProcessIdentity>,
        on: bool,
    ) {
        let total = targets.len();
        if total == 0 {
            return;
        }
        let actions = self.actions.clone();
        self.run_action_refreshing(
            ctx,
            move || i18n::trf(K::EfficiencyChangedCount, &[&total.to_string()]),
            move || {
                for identity in targets {
                    let _ = actions.set_efficiency_mode_checked(
                        identity.pid,
                        identity.start_epoch_s,
                        on,
                    );
                }
                Ok(())
            },
        );
    }

    /// Whether the primary selected row currently has efficiency mode on.
    /// A batch toggle flips away from this one answer.
    pub fn primary_efficiency_mode(&self) -> bool {
        self.selection
            .primary()
            .and_then(|identity| {
                self.latest_snapshot()
                    .as_ref()
                    .and_then(|snapshot| snapshot.process(identity.pid))
                    .and_then(|process| process.power_throttled)
            })
            .unwrap_or(false)
    }

    /// Make Windows paint its caption in the app's own colors.
    ///
    /// The strip immediately below the caption is the search panel, which
    /// fills with `window_bg`; matching the caption to it is what turns two
    /// visibly different bands into one surface. Without this the caption
    /// also keeps light-mode glyphs over a dark UI, because the button
    /// highlights are drawn by DWM and only `IMMERSIVE_DARK_MODE` reaches
    /// them.
    fn sync_title_bar(
        &mut self,
        ctx: &egui::Context,
        pal: &crate::theme::Palette,
        frame: &eframe::Frame,
    ) {
        let dark = matches!(ctx.theme(), egui::Theme::Dark);
        let caption = [pal.window_bg.r(), pal.window_bg.g(), pal.window_bg.b()];
        if self.title_bar_applied == Some((caption, dark)) {
            return;
        }
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = frame.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            return;
        };
        tm_platform::apply_title_bar(
            win32.hwnd.get(),
            caption,
            [pal.text.r(), pal.text.g(), pal.text.b()],
            [pal.window_bg.r(), pal.window_bg.g(), pal.window_bg.b()],
            dark,
        );
        self.title_bar_applied = Some((caption, dark));
    }

    /// End the selected processes (toolbar "Task beenden") — on the executor.
    /// Uses the stored exact identities; vanished or recycled PIDs are
    /// refused. A single row keeps Task Manager's one-click behavior; several
    /// rows go through the confirmation, because that is a different question.
    pub fn end_selected(&mut self, ctx: &egui::Context) {
        let mut targets = self.live_selection_targets();
        if targets.is_empty() {
            self.selection.clear();
            self.shared.toast(i18n::tr(K::ProcessExited));
            return;
        }
        if targets.len() == 1 {
            let (identity, name) = targets.remove(0);
            self.end_process_identity(ctx, identity, false, name);
            return;
        }
        self.pending_process_end = Some(PendingProcessEnd {
            targets,
            tree: false,
        });
    }

    /// Frame-rate diagnostics (TASKMAN_FPS_PROBE=1): measures the interval
    /// between consecutive frames, keeps a short-window average and draws a
    /// compact overlay comparing the achieved rate with the display's
    /// refresh rate. Once per second the numbers are also logged.
    fn update_fps_probe(&mut self, ctx: &egui::Context) {
        const EMA: f64 = 0.15; // ~7-frame time constant at 144 Hz
        const OUTLIER_MS: f64 = 500.0; // window hidden / machine slept

        let now = std::time::Instant::now();
        if let Some(prev) = self.last_frame {
            let dt_ms = (now - prev).as_secs_f64() * 1000.0;
            if dt_ms < OUTLIER_MS {
                self.fps_frames += 1;
                self.fps_ema_ms = if self.fps_ema_ms <= 0.0 {
                    dt_ms
                } else {
                    self.fps_ema_ms + EMA * (dt_ms - self.fps_ema_ms)
                };
            }
        }
        self.last_frame = Some(now);

        let elapsed = now.duration_since(self.fps_window_start);
        if elapsed.as_secs_f64() >= 1.0 {
            let fps = self.fps_frames as f64 / elapsed.as_secs_f64();
            tracing::info!(
                fps,
                frame_ms = format_args!("{:.2}", self.fps_ema_ms),
                display_hz = self.display_hz.unwrap_or(0.0),
                "fps probe"
            );
            self.fps_frames = 0;
            self.fps_window_start = now;
        }

        // Overlay: top-right, unobtrusive, only in probe mode.
        let hz = self
            .display_hz
            .map_or_else(|| "?".to_string(), |h| format!("{h:.0}"));
        let line = if self.fps_ema_ms > 0.0 {
            format!(
                "{:.1} fps · {:.1} ms · {} Hz",
                1000.0 / self.fps_ema_ms.max(0.01),
                self.fps_ema_ms,
                hz
            )
        } else {
            format!("… fps · {hz} Hz")
        };
        egui::Area::new(egui::Id::new("tm_fps_overlay"))
            .anchor(egui::Align2::RIGHT_TOP, [-8.0, 26.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.monospace(line);
                });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit §10: history capacity derives from the configured graph window
    /// sampled at the fastest speed, clamped to sane bounds.
    #[test]
    fn history_cap_matches_window_and_speed() {
        // 60 s at 0.5 s fastest interval = 120 points.
        assert_eq!(history_cap_for(60, 0.5), 120);
        // 120 s window doubles the requirement.
        assert_eq!(history_cap_for(120, 0.5), 240);
        // Clamp floor.
        assert_eq!(history_cap_for(10, 0.5), 120);
        // Clamp ceiling (600 s max setting -> 1200 < 1440 cap).
        assert_eq!(history_cap_for(600, 0.5), 1200);
        // Non-divisible windows round UP so the requested span always fits
        // (240 s / 1.5 s = exactly 160 here).
        assert_eq!(history_cap_for(241, 1.5), 161);
    }

    /// Regression ("graphs stop updating"): the history was a `VecDeque`
    /// whose ring buffer wraps after `history_cap` pop/push cycles. The
    /// Performance tab then read only `as_slices().0` — the OLD half of the
    /// ring — so charts froze on stale data for ~cap ticks per ring cycle,
    /// with a single stale-to-fresh blip in between. History is a contiguous
    /// `Vec` now; this pins that the newest point stays visible through
    /// arbitrarily many retention cycles via the real retention code path.
    #[test]
    fn history_retention_keeps_newest_point_visible() {
        let cap = history_cap_for(60, 0.5); // 120 — default window at fastest speed
        let mut history: Vec<HistoryPoint> = Vec::with_capacity(cap + 8);
        let ticks = cap as u64 * 3 + 8; // well past the former ring-wrap point
        for t in 0..ticks {
            push_history_point(
                &mut history,
                cap,
                HistoryPoint {
                    t_ms: t,
                    ..Default::default()
                },
            );
            let win = crate::tabs::performance::visible_slice(&history, 60);
            assert_eq!(
                win.last().map(|p| p.t_ms),
                Some(t),
                "newest point lost at tick {t}"
            );
        }
    }
}

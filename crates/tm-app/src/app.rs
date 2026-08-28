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
//! * Platform control actions (kill/priority/service/session/...) run on one
//!   shared action-executor thread.

use crate::action_executor::ActionExecutor;
use crate::app_ui::apply_theme;
use crate::widgets::tablekit::{TmColumn, TmTable};
use eframe::egui;
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Single executor for platform control actions (never the UI thread).
    /// `None` only if the worker thread could not spawn; actions then run
    /// inline (degraded but functional).
    pub executor: Option<ActionExecutor>,
    // In-flight markers so slow platform queries run off the UI thread exactly once.
    pub services_fetch: InFlight,
    pub startup_fetch: InFlight,
    pub sessions_fetch: InFlight,
    pub service_control: InFlight,
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
    pub affinity_dialog: Option<(u32, u64)>, // pid, current mask
    pub startup_props: Option<tm_core::model::StartupItem>,
    /// Selection by stable item id (list indexes shift on refresh).
    pub selected_startup_id: Option<String>,
    /// Process-properties dialog target pid.
    pub proc_props: Option<u32>,
    /// Cross-tab jump: services tab should select this service name.
    pub svc_jump: Arc<Mutex<Option<String>>>,

    /// Global top search ("Nach Namen, Herausgeber oder PID suchen").
    pub search: String,

    // Tab states.
    pub processes_state: crate::tabs::processes::State,
    pub perf_selected_key: String,
    /// One-shot type-ahead scroll target for the Performance card column.
    pub perf_jump_to: Option<String>,
    pub details_state: crate::tabs::details::State,
    /// Selected process by EXACT identity (audit §7): the toolbar's End Task
    /// and Efficiency commands validate start-time identity against the live
    /// snapshot before dispatching, so a recycled PID is never targeted.
    pub selected_process: Option<ProcessIdentity>,
    pub selected_user: Option<u32>,
    /// Pending sign-out awaiting confirmation (session id + display name).
    /// Logoff is a high-blast-radius action and must never be one-click.
    pub pending_session_logoff: Option<(u32, String)>,
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
}

impl TaskManApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        mock: bool,
        settings: Settings,
        initial_tab: Option<String>,
    ) -> Self {
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

        // Start page: CLI/diagnostic override wins, otherwise the setting.
        let tab = initial_tab
            .as_deref()
            .and_then(tab_from_cli)
            .or_else(|| Tab::from_key(&settings.default_start_page))
            .unwrap_or(Tab::Processes);

        let toasts: Arc<ToastQueue> = Arc::new(Mutex::new(Vec::new()));
        let executor = ActionExecutor::start();

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
            svc_jump: Arc::new(Mutex::new(None)),
            search: String::new(),
            processes_state: crate::tabs::processes::State::new(),
            perf_selected_key,
            perf_jump_to: None,
            details_state,
            selected_process: None,
            selected_user: None,
            pending_session_logoff: None,
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
        }
    }

    /// Pull the newest snapshot into local history buffers (called once per
    /// actual repaint — the engine notifier guarantees freshness).
    fn poll_engine(&mut self) -> Option<Arc<Snapshot>> {
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
            };
            push_history_point(&mut self.history, self.history_cap, pt);

            // Feed the persistent app-history database.
            let interval_s = self.engine.interval().as_secs_f64().max(0.05);
            self.app_history_db.observe(&latest, interval_s);

            self.ticks_seen += 1;
            Some(latest)
        } else {
            None
        }
    }

    /// Derive telemetry demand from the visible surface and ship it when it
    /// changes (implement.md §6.3). Cheap: one atomic command on change.
    fn update_demand(&mut self) {
        let mut d = TelemetryDemand::core(); // core + adapter rates + tokens
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
    /// completion wakes the UI. Falls back to running inline when no
    /// executor exists.
    pub fn run_action(
        &mut self,
        ctx: &egui::Context,
        success_msg: impl FnOnce() -> String + Send + 'static,
        job: impl FnOnce() -> Result<(), tm_core::TmError> + Send + 'static,
    ) {
        let toasts = self.shared.toasts.clone();
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        match &self.shared.executor {
            Some(executor) => executor.run(toasts, wake, success_msg, job),
            None => {
                let res = job();
                let msg = match res {
                    Ok(()) => success_msg(),
                    Err(e) => i18n::trf(K::ErrMsg, &[&e.to_string()]),
                };
                crate::app::toast_from(&toasts, msg);
            }
        }
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
        if self.poll_engine().is_some() {
            self.app_history_db.save_async();
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
        self.poll_engine();

        let pal = crate::theme::palette_ctx(&ctx);

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
        if let Some((pid, mask)) = self.affinity_dialog {
            crate::tabs::details::affinity_dialog(self, &ctx, pid, mask, &pal);
        }
        if self.pending_session_logoff.is_some() {
            crate::tabs::users::session_logoff_dialog(self, &ctx, &pal);
        }
        if self.startup_props.is_some() {
            crate::tabs::startup::properties_dialog(self, &ctx, &pal);
        }
        if self.proc_props.is_some() {
            crate::tabs::details::process_properties_dialog(self, &ctx);
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

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Persist final settings (honors the autosave gate; the gate choice
        // itself was already force-persisted when toggled).
        self.save_settings();
        self.shared.settings_writer.flush();
        // Final synchronous flush so history is not lost at shutdown.
        self.app_history_db.save();
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
            .is_none_or(|p| p.start_epoch_s.is_none() || p.start_epoch_s == identity.start_epoch_s)
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
            Some(executor) => executor.run_quiet(wake, job),
            None => job(),
        }
    }

    /// End the selected process (toolbar "Task beenden") — on the executor.
    ///
    /// Uses the stored EXACT process identity and validates it against the
    /// live snapshot before dispatching (audit §7): a PID recycled between
    /// selection and command can never be killed by mistake.
    pub fn end_selected(&mut self, ctx: &egui::Context) {
        let Some(identity) = self.selected_process.take() else {
            return;
        };
        if !self.identity_is_live(&identity) {
            self.shared.toast(i18n::tr(K::ProcessExited));
            return;
        }
        let pid = identity.pid;
        let actions = self.actions.clone();
        let start = identity.start_epoch_s;
        self.run_action(
            ctx,
            move || i18n::trf(K::ProcessEndedToast, &[&pid.to_string()]),
            move || actions.kill_process(pid, start, false),
        );
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

//! Root application state & layout: navigation rail, top search, per-tab
//! command bars, dialogs, toasts.

use crate::app_ui::apply_theme;
use crate::widgets::tablekit::{TmColumn, TmTable};
use eframe::egui;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tm_core::engine::EngineHandle;
use tm_core::i18n::{self, K};
use tm_core::model::Snapshot;
use tm_core::settings::Settings;
use tm_platform::actions::PlatformActions;

const HISTORY_CAP: usize = 240;

/// One tick of history used to drive all Performance-tab charts.
#[derive(Debug, Clone, Default)]
pub struct HistoryPoint {
    pub t_ms: u64,
    pub cpu_total: f32,
    pub per_core: Vec<f32>,
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

    pub fn key(self) -> K {
        match self {
            Tab::Processes => K::TabProcesses,
            Tab::Performance => K::TabPerformance,
            Tab::AppHistory => K::TabAppHistory,
            Tab::Startup => K::TabStartup,
            Tab::Users => K::TabUsers,
            Tab::Details => K::TabDetails,
            Tab::Services => K::TabServices,
        }
    }

    /// Localized label for the active UI language.
    pub fn label(self) -> &'static str {
        i18n::tr(self.key())
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
    /// Cached service/startup/user lists refreshed lazily in the background.
    pub services_cache: Arc<Mutex<Option<crate::tabs::services::Cache>>>,
    pub startup_cache: CachedVec<tm_core::model::StartupItem>,
    pub sessions_cache: CachedVec<tm_core::model::UserSession>,
    /// Shell icon textures keyed by executable path.
    pub icons: crate::icon_cache::IconCache,
    /// Toast queue (message, born instant). Workers push, the UI drains.
    pub toasts: Arc<Mutex<Vec<(String, std::time::Instant)>>>,
    // In-flight markers so slow platform queries run off the UI thread exactly once.
    pub services_fetch: InFlight,
    pub startup_fetch: InFlight,
    pub sessions_fetch: InFlight,
    pub service_control: InFlight,
}

impl SharedState {
    pub fn toast(&self, msg: impl Into<String>) {
        let mut t = tm_core::sync::lock(&self.toasts);
        t.push((msg.into(), std::time::Instant::now()));
        if t.len() > 6 {
            t.remove(0);
        }
    }

    pub fn service_control_busy(&self) -> bool {
        self.service_control.busy()
    }
}

/// Push a toast from any thread.
pub fn toast_from(toasts: &Mutex<Vec<(String, std::time::Instant)>>, msg: String) {
    let mut t = tm_core::sync::lock(toasts);
    t.push((msg, std::time::Instant::now()));
    if t.len() > 6 {
        t.remove(0);
    }
}

pub struct TaskManApp {
    pub engine: EngineHandle,
    pub actions: Arc<dyn PlatformActions>,
    pub shared: SharedState,
    pub history: VecDeque<HistoryPoint>,
    pub app_history_db: tm_core::AppHistoryDb,
    pub tab: Tab,
    pub show_settings: bool,
    pub run_dialog_open: bool,
    pub run_dialog_text: String,
    pub run_elevated: bool,
    pub ticks_seen: u64,
    pub affinity_dialog: Option<(u32, u64)>, // pid, current mask
    pub startup_props: Option<tm_core::model::StartupItem>,
    pub selected_startup_idx: Option<usize>,
    pub last_save: std::time::Instant,
    /// Process-properties dialog target pid.
    pub proc_props: Option<u32>,
    /// Cross-tab jump: services tab should select this service name.
    pub svc_jump: Arc<Mutex<Option<String>>>,

    /// Global top search ("Nach Namen, Herausgeber oder PID suchen").
    pub search: String,

    // Tab states.
    pub processes_state: crate::tabs::processes::State,
    pub perf_selected: usize,
    pub details_state: crate::tabs::details::State,
    pub selected_pid: Option<u32>,
    pub selected_user: Option<u32>,
    pub efficiency_pids: std::collections::HashSet<u32>,
    // Services tab.
    pub services_selected_name: Option<String>,
}

fn dirs_data() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var("LOCALAPPDATA")
            .map_or_else(|_| std::path::PathBuf::from("."), std::path::PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| std::path::PathBuf::from(h).join(".local/share"))
            })
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
    }
}

impl TaskManApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        engine: EngineHandle,
        mock: bool,
        settings: Settings,
    ) -> Self {
        // Mock mode still wants a real action surface for testing menus;
        // platform actions are harmless against a mock snapshot.
        let (_c, actions) = tm_platform::create_stack();
        let _ = mock;
        let actions: Arc<dyn PlatformActions> = Arc::from(actions);
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

        let mut icons = crate::icon_cache::IconCache::default();
        icons.start_worker(actions.clone());

        // Active UI language: resolved from the persisted choice against the
        // OS-detected locale.
        i18n::set_lang(settings.language.resolve());

        let history_path = dirs_data().join("app-history.json");
        Self {
            engine,
            actions,
            shared: SharedState {
                settings,
                services_cache: Arc::new(Mutex::new(None)),
                startup_cache: Arc::new(Mutex::new(None)),
                sessions_cache: Arc::new(Mutex::new(None)),
                icons,
                toasts: Arc::new(Mutex::new(Vec::new())),
                services_fetch: InFlight::default(),
                startup_fetch: InFlight::default(),
                sessions_fetch: InFlight::default(),
                service_control: InFlight::default(),
            },
            history: VecDeque::with_capacity(HISTORY_CAP),
            app_history_db: tm_core::AppHistoryDb::open(history_path),
            tab: Tab::Processes,
            show_settings: false,
            run_dialog_open: false,
            run_dialog_text: String::new(),
            run_elevated: false,
            ticks_seen: 0,
            affinity_dialog: None,
            startup_props: None,
            selected_startup_idx: None,
            last_save: std::time::Instant::now(),
            proc_props: None,
            svc_jump: Arc::new(Mutex::new(None)),
            search: String::new(),
            processes_state: crate::tabs::processes::State::new(),
            perf_selected: 0,
            details_state: Default::default(),
            selected_pid: None,
            selected_user: None,
            efficiency_pids: std::collections::HashSet::new(),
            services_selected_name: None,
        }
    }

    /// Pull the newest snapshot into local history buffers.
    fn poll_engine(&mut self) -> Option<Arc<Snapshot>> {
        let latest = self.engine.latest()?;
        if latest.timestamp_ms != self.history.back().map_or(0, |h| h.t_ms)
            && self.ticks_seen != u64::MAX
        {
            let pt = HistoryPoint {
                t_ms: latest.timestamp_ms,
                cpu_total: latest.cpu.utilization_pct,
                per_core: latest.cpu.per_core_pct.clone(),
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
            if self.history.len() == HISTORY_CAP {
                self.history.pop_front();
            }
            self.history.push_back(pt);

            // Feed the persistent app-history database.
            let interval_s = self.engine.interval().as_secs_f64().max(0.05);
            self.app_history_db.observe(&latest, interval_s);

            self.ticks_seen += 1;
            Some(latest)
        } else {
            None
        }
    }

    fn maybe_save_app_history(&mut self) {
        if self.last_save.elapsed() > std::time::Duration::from_secs(30) {
            // Serialize + file I/O happen on a worker thread; the UI keeps
            // running from the in-memory copy meanwhile.
            self.app_history_db.save_async();
            self.last_save = std::time::Instant::now();
        }
    }
}

impl TaskManApp {
    pub fn set_initial_tab(&mut self, name: &str) {
        self.tab = match name.to_ascii_lowercase().as_str() {
            "processes" | "prozesse" => Tab::Processes,
            "performance" | "leistung" => Tab::Performance,
            "apphistory" | "history" | "appverlauf" => Tab::AppHistory,
            "startup" | "autostart" => Tab::Startup,
            "users" | "benutzer" => Tab::Users,
            "details" => Tab::Details,
            "services" | "dienste" => Tab::Services,
            _ => self.tab,
        };
        tracing::info!(tab = ?self.tab, "initial tab applied");
    }
}

impl eframe::App for TaskManApp {
    /// Data pass: runs before each `ui`, and while the window is hidden.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.poll_engine().is_some() {
            self.maybe_save_app_history();
        }
        // Wake up for the next sample.
        ctx.request_repaint_after(
            self.engine
                .interval()
                .div_f64(2.0)
                .max(std::time::Duration::from_millis(50)),
        );
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        crate::theme::ensure_visuals(&ctx);
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
        if self.startup_props.is_some() {
            crate::tabs::startup::properties_dialog(self, &ctx, &pal);
        }
        if self.proc_props.is_some() {
            crate::tabs::details::process_properties_dialog(self, &ctx);
        }
        crate::app_ui::draw_toasts(self, &ctx);

        // Global shortcuts.
        if ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            self.engine.request_refresh();
        }

        // Track window size for persistence.
        let size = ctx.input(|i| i.viewport().inner_rect.map(|r| r.size()));
        if let Some(sz) = size {
            self.shared.settings.window_size = [sz.x, sz.y];
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shared.settings.save();
        // Final synchronous flush so nothing is lost at shutdown.
        self.app_history_db.save();
    }
}
impl TaskManApp {
    pub fn latest_snapshot(&self) -> Option<Arc<Snapshot>> {
        self.engine.latest()
    }

    /// The active UI language.
    pub fn lang(&self) -> i18n::Lang {
        i18n::lang()
    }

    /// Translate a key into the active UI language.
    #[allow(dead_code)]
    pub fn tr(&self, key: K) -> &'static str {
        i18n::tr_in(self.lang(), key)
    }

    /// Build a table with this tab's persisted column widths restored.
    pub fn make_table(&self, id: &'static str, cols: Vec<TmColumn>, name_min: f32) -> TmTable {
        let saved = self
            .shared
            .settings
            .col_widths
            .get(id)
            .map(|v| v.as_slice());
        TmTable::new(id, cols, saved, name_min)
    }

    /// Persist resized column widths.
    ///
    /// The table object is rebuilt every frame from the stored widths, so the
    /// in-memory settings map must be updated on EVERY frame with changes —
    /// otherwise the drag delta would be discarded on the next rebuild and
    /// the column would never move. The (atomic) disk write happens only
    /// when the drag gesture finishes.
    pub fn persist_table(&mut self, table: &TmTable) {
        if table.changed_this_frame() {
            self.shared
                .settings
                .col_widths
                .insert(table.id.to_string(), table.stored_widths());
            // Single-shot changes (double-click reset) finish immediately.
            if !table.dragging() {
                self.shared.settings.save();
            }
        }
        if table.drag_just_ended() {
            self.shared.settings.save();
        }
    }

    /// Jump to the services tab and highlight the service hosted by `pid`
    /// once the SCM query completes (worker fills [`Self::svc_jump`]).
    pub fn goto_services_for_pid(&mut self, pid: u32) {
        self.tab = crate::app::Tab::Services;
        let actions = self.actions.clone();
        let jump = self.svc_jump.clone();
        let toasts = self.shared.toasts.clone();
        let _ = std::thread::Builder::new()
            .name("tm-svc-jump".into())
            .spawn(move || match actions.list_services() {
                Ok(list) => {
                    if let Some(svc) = list.iter().find(|s| s.pid == Some(pid)) {
                        *tm_core::sync::lock(&jump) = Some(svc.name.clone());
                    } else {
                        crate::app::toast_from(
                            &toasts,
                            tm_core::i18n::trf(
                                tm_core::i18n::K::NoServiceForPid,
                                &[&pid.to_string()],
                            ),
                        );
                    }
                }
                Err(e) => crate::app::toast_from(
                    &toasts,
                    tm_core::i18n::trf(tm_core::i18n::K::ErrMsg, &[&e.to_string()]),
                ),
            });
    }

    /// End the selected process (toolbar "Task beenden").
    pub fn end_selected(&mut self) {
        if let Some(pid) = self.selected_pid.take() {
            match self.actions.kill_process(pid, false) {
                Ok(()) => self
                    .shared
                    .toast(tm_core::i18n::trf(K::ProcessEndedToast, &[&pid.to_string()])),
                Err(e) => self
                    .shared
                    .toast(tm_core::i18n::trf(K::ErrMsg, &[&e.to_string()])),
            }
        }
    }
}

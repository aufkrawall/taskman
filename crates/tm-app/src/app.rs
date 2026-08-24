//! Root application state & layout: navigation rail, tab routing, status bar,
//! settings dialog, run-new-task dialog, toast notifications.

use crate::app_ui::apply_theme;
use eframe::egui;
use parking_lot::Mutex as PlMutex;
use std::collections::VecDeque;
use std::sync::Arc;
use tm_core::engine::EngineHandle;
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

    pub fn label(self) -> &'static str {
        match self {
            Tab::Processes => "Prozesse",
            Tab::Performance => "Leistung",
            Tab::AppHistory => "App-Verlauf",
            Tab::Startup => "Autostart",
            Tab::Users => "Benutzer",
            Tab::Details => "Details",
            Tab::Services => "Dienste",
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

type CachedVec<T> = Arc<PlMutex<Option<(Vec<T>, std::time::Instant)>>>;

/// Shared UI-side caches that individual tabs mutate.
pub struct SharedState {
    pub settings: Settings,
    /// Cached service/startup/user lists refreshed lazily.
    pub services_cache: Arc<PlMutex<Option<crate::tabs::services::Cache>>>,
    pub startup_cache: CachedVec<tm_core::model::StartupItem>,
    pub sessions_cache: CachedVec<tm_core::model::UserSession>,
    /// Toast queue (message, born instant).
    pub toasts: Arc<PlMutex<Vec<(String, std::time::Instant)>>>,
}

impl SharedState {
    pub fn toast(&self, msg: impl Into<String>) {
        let mut t = self.toasts.lock();
        t.push((msg.into(), std::time::Instant::now()));
        if t.len() > 6 {
            t.remove(0);
        }
    }
}

pub struct TaskManApp {
    pub engine: EngineHandle,
    pub actions: Box<dyn PlatformActions>,
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
    pub last_save: std::time::Instant,

    // Tab states.
    pub processes_state: crate::tabs::processes::State,
    pub perf_selected: usize, // index into resource list of Performance tab
    pub details_sort_col: usize,
    pub details_ascending: bool,
    pub details_filter: String,
    pub selected_pid: Option<u32>,
    pub efficiency_pids: std::collections::HashSet<u32>,
    // Services tab.
    pub services_search: String,
    pub services_running_filter: bool,
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
    pub fn new(cc: &eframe::CreationContext<'_>, engine: EngineHandle, mock: bool) -> Self {
        let (_c, actions) = if mock {
            // Mock mode still wants a real action surface for testing menus;
            // platform actions are harmless against a mock snapshot.
            tm_platform::create_stack()
        } else {
            tm_platform::create_stack()
        };
        let settings = Settings::load();
        apply_theme(&cc.egui_ctx, settings.theme);
        if settings.always_on_top {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                    egui::WindowLevel::AlwaysOnTop,
                ));
        }

        let history_path = dirs_data().join("app-history.json");
        Self {
            engine,
            actions,
            shared: SharedState {
                settings,
                services_cache: Arc::new(PlMutex::new(None)),
                startup_cache: Arc::new(PlMutex::new(None)),
                sessions_cache: Arc::new(PlMutex::new(None)),
                toasts: Arc::new(PlMutex::new(Vec::new())),
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
            last_save: std::time::Instant::now(),
            processes_state: Default::default(),
            perf_selected: 0,
            details_sort_col: 0,
            details_ascending: false,
            details_filter: String::new(),
            selected_pid: None,
            efficiency_pids: std::collections::HashSet::new(),
            services_search: String::new(),
            services_running_filter: false,
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

    fn save_app_history(&mut self) {
        if self.last_save.elapsed() > std::time::Duration::from_secs(30) {
            self.app_history_db.save();
            self.last_save = std::time::Instant::now();
        }
    }
}

impl TaskManApp {
    pub fn set_initial_tab(&mut self, name: &str) {
        self.tab = match name.to_ascii_lowercase().as_str() {
            "processes" | "prozesse" => Tab::Processes,
            "performance" | "leistung" => Tab::Performance,
            "history" | "appverlauf" => Tab::AppHistory,
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
            self.save_app_history();
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
        if self.ticks_seen == 2 {
            tracing::info!(tab = ?self.tab, "render state check");
        }

        let pal = crate::theme::palette_ctx(&ctx);

        // ------------------------------------------------ top-level panels
        self.sidebar(ui, &pal);
        self.status_bar(ui, &pal);

        // ------------------------------------------------ main content
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(pal.window_bg)
                    .inner_margin(egui::Margin::same(10)),
            )
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
            self.settings_dialog(&ctx, &pal);
        }
        if self.run_dialog_open {
            self.run_task_dialog(&ctx, &pal);
        }
        if let Some((pid, mask)) = self.affinity_dialog {
            crate::tabs::details::affinity_dialog(self, &ctx, pid, mask, &pal);
        }
        self.draw_toasts(&ctx);

        // Global shortcuts.
        if ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            self.engine.sample_now();
        }

        // Track window size for persistence.
        let size = ctx.input(|i| i.viewport().inner_rect.map(|r| r.size()));
        if let Some(sz) = size {
            self.shared.settings.window_size = [sz.x, sz.y];
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shared.settings.save();
        self.app_history_db.save();
    }
}
impl TaskManApp {
    pub fn latest_snapshot(&self) -> Option<Arc<Snapshot>> {
        self.engine.latest()
    }
}

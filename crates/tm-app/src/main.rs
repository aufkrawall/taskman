//! taskman — a cross-platform task manager.
//!
//! Windows note: the release build hides the console (`windows` subsystem);
//! CLI output (--selfcheck) re-attaches to the parent console when present.

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod action_executor;
mod app;
mod app_ui;
mod fonts;
mod icon_cache;
mod icons;
mod search;
mod selfcheck;
mod tabs;
mod theme;
mod ui_state;
mod widgets;

use std::time::Instant;

const APP_ID: &str = "io.github.aufkrawall.Taskman";
const DEFAULT_WINDOW_SIZE: [f32; 2] = [1280.0, 800.0];

pub struct StartupTrace;

impl StartupTrace {
    pub fn mark(name: &'static str) {
        static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let elapsed = T0.get_or_init(Instant::now).elapsed().as_millis();
        tracing::info!(ms = elapsed as u64, phase = name, "startup");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Short-lived elevated helper for the HKLM IFEO integration. Handle this
    // before GUI/logging startup so a UAC helper never flashes the app window.
    #[cfg(target_os = "windows")]
    if let Some(action) = args
        .iter()
        .find_map(|a| a.strip_prefix("--taskmgr-integration="))
    {
        let enabled = match action {
            "enable" => true,
            "disable" => false,
            _ => std::process::exit(2),
        };
        let code = match tm_platform::win::set_task_manager_replacement_direct(enabled) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("taskman: Task Manager integration failed: {e}");
                1
            }
        };
        std::process::exit(code);
    }

    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
    let mock = args.iter().any(|a| a == "--mock");
    let selfcheck = args.iter().any(|a| a == "--selfcheck");

    #[cfg(all(target_os = "windows", not(debug_assertions)))]
    if selfcheck || verbose {
        attach_parent_console();
    }

    if selfcheck || verbose {
        let _log_guard = tm_core::logging::init(tm_core::logging::LogConfig {
            console: true,
            level: verbose.then(|| "debug".parse().expect("static")),
        });
    } else {
        tm_core::logging::init_early(false);
    }
    StartupTrace::mark("args_parsed");
    tracing::info!(args = ?args, "taskman starting");

    if selfcheck {
        let code = selfcheck::run(mock);
        std::process::exit(code);
    }

    run_gui(mock, &args);
}

fn run_gui(mock: bool, args: &[String]) {
    tm_core::locale::init(tm_platform::detect_locale());

    // A genuinely fresh install gets a roomier first window. Existing users
    // keep their recorded dimensions, including legacy-JSON migrations.
    let config_dir = tm_core::settings::taskman_config_dir();
    let has_saved_settings =
        config_dir.join("config.ini").exists() || config_dir.join("settings.json").exists();
    let mut settings = tm_core::settings::Settings::load();
    if !has_saved_settings {
        settings.window_size = DEFAULT_WINDOW_SIZE;
    }
    StartupTrace::mark("minimal_config_loaded");

    // "Always start elevated" policy (Windows): when the persisted setting
    // asks for it and this launch is unelevated, re-exec with the runas verb
    // before any window exists. A declined UAC prompt degrades to a normal
    // unelevated start (logged; retried on the next launch). Isolated
    // test/config-override contexts never auto-elevate.
    #[cfg(target_os = "windows")]
    if settings.start_elevated
        && std::env::var_os("TASKMAN_CONFIG_DIR").is_none()
        && !tm_platform::win::is_elevated()
    {
        match tm_platform::win::relaunch_elevated_with_args(args) {
            Ok(()) => {
                tracing::info!("start_elevated: re-execing elevated");
                std::process::exit(0);
            }
            Err(e) => {
                tracing::warn!(error = %e, "auto-elevation failed; starting unelevated");
            }
        }
    }

    if let Some(sz) = args
        .iter()
        .find_map(|a| a.strip_prefix("--size=").map(|s| s.to_string()))
        .and_then(|s| parse_size_arg(&s))
    {
        settings.window_size = sz;
    }
    tm_core::i18n::set_lang(settings.language.resolve());
    let window_size = [settings.window_size[0], settings.window_size[1]];
    let restore_position = has_saved_settings && settings.remember_window;
    let window_position = restore_position.then(ui_state::window_position).flatten();
    let restore_maximized = restore_position && ui_state::window_maximized();

    let initial_tab_arg = args
        .iter()
        .find_map(|a| a.strip_prefix("--tab=").map(|t| t.to_string()))
        .or_else(|| std::env::var("TASKMAN_TAB").ok());
    if let Some(t) = &initial_tab_arg {
        eprintln!("initial tab requested: {t}");
    }

    let title = tm_core::i18n::tr(tm_core::i18n::K::WindowTitle).to_string();

    let options = |renderer: eframe::Renderer| {
        let mut viewport = eframe::egui::ViewportBuilder::default()
            .with_title(title.clone())
            // Wayland compositors use app_id to associate windows with
            // the matching desktop entry/icon and group them correctly.
            .with_app_id(APP_ID)
            .with_inner_size(window_size)
            .with_min_inner_size([720.0, 480.0])
            .with_icon(icon_data());
        if let Some(pos) = window_position {
            viewport = viewport.with_position(pos);
        }
        if restore_maximized {
            viewport = viewport.with_maximized(true);
        }
        let mut opts = eframe::NativeOptions {
            renderer,
            viewport,
            ..Default::default()
        };
        #[cfg(feature = "wgpu")]
        {
            opts.wgpu_options =
                eframe::WgpuConfiguration::default().with_surface_config(eframe::SurfaceConfig {
                    present_mode: eframe::wgpu::PresentMode::Fifo,
                    desired_maximum_frame_latency: Some(1),
                });
        }
        opts
    };

    let renderer_pref = std::env::var("TASKMAN_RENDERER").unwrap_or_default();
    StartupTrace::mark("run_native_enter");

    let mut last_err = None;
    for renderer in preferred_renderers(&renderer_pref) {
        tracing::info!(?renderer, "trying renderer");
        let use_mock = mock;
        let initial_tab = initial_tab_arg.clone();
        let app_settings = settings.clone();
        let creator = move |cc: &eframe::CreationContext<'_>| {
            StartupTrace::mark("creation_context_enter");
            theme::apply_startup(&cc.egui_ctx);
            fonts::install_async(cc.egui_ctx.clone());
            let application = app::TaskManApp::new(cc, use_mock, app_settings, initial_tab);
            Ok(Box::new(NativeApp::new(application)) as Box<dyn eframe::App>)
        };
        match eframe::run_native("Task-Manager", options(renderer), Box::new(creator)) {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!(error = %e, "renderer failed; falling back");
                last_err = Some(e);
            }
        }
    }
    eprintln!(
        "taskman: no usable GPU renderer found: {:?}",
        last_err.map(|e| e.to_string())
    );
    std::process::exit(1);

    #[allow(clippy::vec_init_then_push)]
    fn preferred_renderers(pref: &str) -> Vec<eframe::Renderer> {
        let mut all = Vec::<eframe::Renderer>::with_capacity(2);
        #[cfg(feature = "wgpu")]
        all.push(eframe::Renderer::Wgpu);
        #[cfg(feature = "glow")]
        all.push(eframe::Renderer::Glow);
        let want_wgpu = pref.eq_ignore_ascii_case("wgpu");
        let want_glow = pref.eq_ignore_ascii_case("glow");
        all.into_iter()
            .filter(|r| match r {
                #[cfg(feature = "wgpu")]
                eframe::Renderer::Wgpu => !want_glow,
                #[cfg(feature = "glow")]
                eframe::Renderer::Glow => !want_wgpu,
                #[allow(unreachable_patterns)]
                _ => true,
            })
            .collect()
    }
}

/// Thin native-shell wrapper. TaskManApp already owns all UI/application
/// state; this layer only records the viewport's desktop-space position and
/// delegates every application hook unchanged.
struct NativeApp {
    inner: app::TaskManApp,
}

impl NativeApp {
    fn new(inner: app::TaskManApp) -> Self {
        Self { inner }
    }
}

impl eframe::App for NativeApp {
    fn logic(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        <app::TaskManApp as eframe::App>::logic(&mut self.inner, ctx, frame);
    }

    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        <app::TaskManApp as eframe::App>::ui(&mut self.inner, ui, frame);
        if self.inner.shared.settings.remember_window {
            let (pos, maximized) = ui.ctx().input(|i| {
                (
                    i.viewport().outer_rect.map(|r| r.min),
                    i.viewport().maximized.unwrap_or(false),
                )
            });
            // A maximized window's outer rect is the monitor's, not the
            // restore geometry — keep the last normal position instead.
            if !maximized && let Some(pos) = pos {
                ui_state::set_window_position([pos.x, pos.y]);
            }
            ui_state::set_window_maximized(maximized);
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if self.inner.shared.settings.save_config && self.inner.shared.settings.remember_window {
            ui_state::save();
        }
        <app::TaskManApp as eframe::App>::on_exit(&mut self.inner, gl);
    }
}

fn parse_size_arg(s: &str) -> Option<[f32; 2]> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?);
    (w >= 200.0 && h >= 150.0).then_some([w, h])
}

fn icon_data() -> eframe::egui::IconData {
    const S: usize = 64;
    let mut rgba = vec![0u8; S * S * 4];
    let accent = [0u8, 120, 212];
    for y in 0..S {
        for x in 0..S {
            let i = (y * S + x) * 4;
            let in_chip = (16..48).contains(&x) && (16..48).contains(&y);
            let pin_v = (24..40).contains(&x) && !(12..52).contains(&y);
            let pin_h = (24..40).contains(&y) && !(12..52).contains(&x);
            let color = if in_chip {
                accent
            } else if pin_v || pin_h {
                [90u8, 170, 240]
            } else {
                [0, 0, 0]
            };
            rgba[i] = color[0];
            rgba[i + 1] = color[1];
            rgba[i + 2] = color[2];
            rgba[i + 3] = 255;
        }
    }
    eframe::egui::IconData {
        width: S as u32,
        height: S as u32,
        rgba,
    }
}

#[cfg(all(target_os = "windows", not(debug_assertions)))]
fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(any(not(target_os = "windows"), debug_assertions))]
#[allow(dead_code)]
fn attach_parent_console() {}

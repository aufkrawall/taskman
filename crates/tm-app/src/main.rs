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
/// Headless CPU rendering of the real widgets; see the module docs.
#[cfg(test)]
mod render_snapshot;
mod search;
mod selection;
mod selfcheck;
mod tabs;
/// Side-by-side comparison of the text-smoothing profiles; see the module docs.
#[cfg(test)]
mod text_compare;
mod theme;
mod ui_state;
mod widgets;

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

/// The rendering path this process actually started on. The renderer and
/// adapter are chosen once, before the window exists, so the settings dialog
/// compares the live value against the persisted one to decide whether to
/// show its "takes effect at the next start" note.
static ACTIVE_RENDER_MODE: AtomicU8 = AtomicU8::new(0);

fn store_render_mode(mode: tm_core::settings::RenderMode) {
    use tm_core::settings::RenderMode;
    ACTIVE_RENDER_MODE.store(
        match mode {
            RenderMode::Auto => 0,
            RenderMode::Compatibility => 1,
            RenderMode::Software => 2,
        },
        Ordering::Relaxed,
    );
}

pub fn active_render_mode() -> tm_core::settings::RenderMode {
    use tm_core::settings::RenderMode;
    match ACTIVE_RENDER_MODE.load(Ordering::Relaxed) {
        1 => RenderMode::Compatibility,
        2 => RenderMode::Software,
        _ => RenderMode::Auto,
    }
}

/// Effective rendering path: the persisted setting unless
/// `TASKMAN_GPU=auto|compatibility|software` (or the legacy `0`/`1`)
/// overrides it for diagnostics.
fn effective_render_mode(settings: &tm_core::settings::Settings) -> tm_core::settings::RenderMode {
    use tm_core::settings::RenderMode;
    match std::env::var("TASKMAN_GPU")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" => settings.render_mode,
        "0" | "off" | "false" | "software" | "cpu" | "warp" => RenderMode::Software,
        "gl" | "opengl" | "compatibility" => RenderMode::Compatibility,
        "1" | "on" | "true" | "auto" => RenderMode::Auto,
        _ => settings.render_mode,
    }
}

const APP_ID: &str = "io.github.aufkrawall.Taskman";
const DEFAULT_WINDOW_SIZE: [f32; 2] = [1280.0, 800.0];

#[cfg(target_os = "windows")]
static PROGRAMMATIC_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn request_programmatic_exit() {
    #[cfg(target_os = "windows")]
    PROGRAMMATIC_EXIT.store(true, std::sync::atomic::Ordering::Release);
}

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

    // Elevated install/remove helper for the protected core service. Like the
    // IFEO helper above, this path performs no GUI or renderer initialization.
    #[cfg(target_os = "windows")]
    if let Some(operation) = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--core-service="))
    {
        let authorized_user_sid = args
            .iter()
            .find_map(|argument| argument.strip_prefix("--core-service-user="));
        // Never let an elevated helper attach the ordinary per-user file
        // logger: that path is intentionally user-writable. The bounded early
        // sink is memory/console-only; the installed service attaches files
        // only after validating its protected ProgramData directory.
        tm_core::logging::init_early(true);
        let code =
            match tm_platform::win::core_service::handle_helper(operation, authorized_user_sid) {
                Ok(()) => 0,
                Err(error) => {
                    tracing::error!(%error, %operation, "core service helper failed");
                    eprintln!("taskman: core service {operation} failed: {error}");
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

    // A service install pins the trusted GUI under Program Files. Future
    // portable/package launches hand off to that copy before creating a
    // renderer or window, preserving the broker's strict image-path policy.
    #[cfg(all(target_os = "windows", not(debug_assertions)))]
    if std::env::var_os("TASKMAN_CONFIG_DIR").is_none() {
        match tm_platform::win::core_service::redirect_to_installed_gui(&args) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "protected GUI redirect unavailable"),
        }
    }

    run_gui(mock, &args);
}

fn run_gui(mock: bool, args: &[String]) {
    // FIRST, before locale, settings or anything else that can block on a
    // loaded disk: a launch that only has to raise the window this session
    // already shows should do nothing else. Doing it before the elevation
    // policy below also keeps Ctrl+Shift+Esc from raising a UAC prompt on
    // every press once "always start elevated" is on, for a window that is
    // already open. The elevated replacement of an explicit "restart
    // elevated" is the one launch that must not defer to its predecessor.
    #[cfg(target_os = "windows")]
    let elevation_handoff = args
        .iter()
        .any(|argument| argument == "--single-instance-handoff");
    #[cfg(target_os = "windows")]
    if !elevation_handoff
        && tm_platform::win::instance::activate_existing()
            == tm_platform::win::instance::Activation::Activated
    {
        tracing::info!("handed this launch to the running instance");
        return;
    }

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

    // Register only after the optional elevation handoff. Otherwise the
    // unelevated parent would still own the mutex when its elevated child
    // starts, causing that child to signal the parent and exit immediately.
    // An instance that cannot be reached leaves this launch UNCOORDINATED
    // rather than dead: a task manager that refuses to open because another
    // copy is wedged fails at the one moment it is needed.
    #[cfg(target_os = "windows")]
    let _single_instance = match tm_platform::win::instance::acquire(elevation_handoff, || {
        signal_tray(TRAY_ACTION_OPEN)
    }) {
        tm_platform::win::instance::Role::Primary(primary) => Some(primary),
        tm_platform::win::instance::Role::Deferred => return,
        tm_platform::win::instance::Role::Uncoordinated => {
            tracing::warn!("starting without single-instance coordination");
            None
        }
    };
    #[cfg(target_os = "windows")]
    tm_platform::win::prioritize_control_plane();

    if let Some(sz) = args
        .iter()
        .find_map(|a| a.strip_prefix("--size=").map(|s| s.to_string()))
        .and_then(|s| parse_size_arg(&s))
    {
        settings.window_size = sz;
    }
    tm_core::i18n::set_lang(settings.language.resolve());
    let render_mode = effective_render_mode(&settings);
    store_render_mode(render_mode);
    let window_size = [settings.window_size[0], settings.window_size[1]];
    let restore_position = has_saved_settings && settings.remember_window;
    let window_position = restore_position.then(ui_state::window_position).flatten();
    let restore_maximized = restore_position && ui_state::window_maximized();

    let initial_tab_arg = args
        .iter()
        .find_map(|a| a.strip_prefix("--tab=").map(|t| t.to_string()))
        .or_else(|| std::env::var("TASKMAN_TAB").ok());
    // Windows appends taskmgr's own command line to the IFEO debugger
    // command, so a hotkey launch carries arbitrary trailing arguments. Only
    // the marker is read from them, and a launch through the hotkey always
    // shows its window: whatever the autostart entry asks for, the user just
    // asked for a task manager.
    #[cfg(target_os = "windows")]
    let replacement_launch = args
        .iter()
        .any(|argument| argument == tm_platform::win::REPLACEMENT_LAUNCH_ARG);
    #[cfg(not(target_os = "windows"))]
    let replacement_launch = false;
    if replacement_launch {
        tracing::info!("launched as the Windows Task Manager replacement");
    }
    let initially_hidden = !replacement_launch
        && args
            .iter()
            .any(|argument| argument == "--minimized-to-tray");
    if let Some(tab) = &initial_tab_arg {
        // Startup diagnostics belong in the log, not on a stderr the windows
        // subsystem has no console for.
        tracing::info!(tab = %tab, "initial tab requested");
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
            .with_visible(!initially_hidden)
            .with_icon(icon_data());
        if let Some(pos) = window_position {
            viewport = viewport.with_position(pos);
        }
        if restore_maximized {
            viewport = viewport.with_maximized(true);
        }
        let opts = eframe::NativeOptions {
            renderer,
            viewport,
            ..Default::default()
        };
        #[cfg(feature = "wgpu")]
        let opts = {
            let mut opts = opts;
            let mut config =
                eframe::WgpuConfiguration::default().with_surface_config(eframe::SurfaceConfig {
                    present_mode: present_mode_pref(),
                    desired_maximum_frame_latency: frame_latency_pref(),
                });
            let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup else {
                unreachable!("default wgpu configuration creates a fresh instance")
            };
            // Compile and enumerate only the native backend for each host.
            // On Windows this makes D3D12 explicit and avoids loading the
            // Vulkan stack before the existing Glow fallback is considered.
            setup.instance_descriptor.backends =
                eframe::wgpu::Backends::from_env().unwrap_or_else(compiled_wgpu_backends);
            // This is a 2D monitor, so prefer the low-power adapter by
            // default rather than waking a discrete GPU. WGPU_POWER_PREF
            // remains an opt-in override for diagnostics.
            setup.power_preference = eframe::wgpu::PowerPreference::from_env()
                .unwrap_or(eframe::wgpu::PowerPreference::LowPower);
            // "GPU acceleration off" means the platform's software
            // rasterizer (WARP on Windows, lavapipe on Linux), which
            // enumerates as a CPU-type adapter on the same backend. If the
            // host has none, keep rendering rather than refusing to start —
            // the settings dialog reports what actually happened.
            if render_mode == tm_core::settings::RenderMode::Software {
                setup.native_adapter_selector = Some(std::sync::Arc::new(select_software_adapter));
            }
            opts.wgpu_options = config;
            opts
        };
        opts
    };

    // Compatibility mode IS the OpenGL backend; an explicit TASKMAN_RENDERER
    // still wins so the diagnostic override keeps working.
    let renderer_pref = match std::env::var("TASKMAN_RENDERER") {
        Ok(v) if !v.is_empty() => v,
        // "No GPU" now means the native CPU rasterizer, not a WARP adapter emulating a
        // D3D12 driver. Compatibility mode still means OpenGL.
        _ if render_mode == tm_core::settings::RenderMode::Software => "software".to_string(),
        _ if render_mode == tm_core::settings::RenderMode::Compatibility => "glow".to_string(),
        _ => String::new(),
    };
    StartupTrace::mark("run_native_enter");

    let mut last_err = None;
    for renderer in preferred_renderers(&renderer_pref) {
        tracing::info!(?renderer, "trying renderer");
        let use_mock = mock;
        let initial_tab = initial_tab_arg.clone();
        let app_settings = settings.clone();
        let start_hidden = initially_hidden;
        #[cfg(feature = "software")]
        let is_software = matches!(renderer, eframe::Renderer::Software);
        #[cfg(not(feature = "software"))]
        let is_software = false;

        let creator = move |cc: &eframe::CreationContext<'_>| {
            StartupTrace::mark("creation_context_enter");
            // Now the window exists, so the DPI-awareness gate reports the truth. Doing
            // this before `apply_startup` means the very first frame already rasterizes
            // at the right coverage rather than rebuilding the atlas a frame later.
            // `None` means the primary monitor. eframe keeps the raw handle private, and the
            // gate that actually mattered here -- per-monitor DPI awareness -- is a property
            // of the thread rather than of the window, so it is unaffected. The only thing
            // lost is picking up a *secondary* monitor's own ClearType calibration, which
            // differs from the primary's only if the user tuned them separately.
            theme::set_subpixel_capable(is_software, tm_platform::text_rendering::query(None));
            theme::apply_startup(&cc.egui_ctx);
            fonts::install_async(cc.egui_ctx.clone());
            let application = app::TaskManApp::new(cc, use_mock, app_settings, initial_tab);
            Ok(
                Box::new(NativeApp::new(application, &cc.egui_ctx, start_hidden))
                    as Box<dyn eframe::App>,
            )
        };
        let opts = options(renderer);

        match eframe::run_native("Task-Manager", opts, Box::new(creator)) {
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

    /// Renderers to try, in order.
    ///
    /// The native CPU renderer goes first: it needs no driver, starts without enumerating
    /// adapters or compiling shaders, and is the only backend that can do sub-pixel
    /// (ClearType) text. The GPU backends remain as fallbacks so an unusual display
    /// stack -- or a bug in the new path -- still yields a usable window.
    ///
    /// `TASKMAN_RENDERER` picks exactly one; anything else is filtered out.
    #[allow(clippy::vec_init_then_push)]
    fn preferred_renderers(pref: &str) -> Vec<eframe::Renderer> {
        let mut all = Vec::<eframe::Renderer>::with_capacity(3);
        #[cfg(feature = "software")]
        all.push(eframe::Renderer::Software);
        #[cfg(feature = "wgpu")]
        all.push(eframe::Renderer::Wgpu);
        #[cfg(feature = "glow")]
        all.push(eframe::Renderer::Glow);
        all.into_iter()
            .filter(|r| {
                if pref.is_empty() {
                    return true;
                }
                match r {
                    #[cfg(feature = "software")]
                    eframe::Renderer::Software => pref.eq_ignore_ascii_case("software"),
                    #[cfg(feature = "wgpu")]
                    eframe::Renderer::Wgpu => pref.eq_ignore_ascii_case("wgpu"),
                    #[cfg(feature = "glow")]
                    eframe::Renderer::Glow => pref.eq_ignore_ascii_case("glow"),
                    #[allow(unreachable_patterns)]
                    _ => true,
                }
            })
            .collect()
    }
}

/// Pick the software rasterizer among the enumerated adapters, preferring
/// one that can actually present to our surface. Falls back to any adapter so
/// a machine without a software rasterizer still gets a window.
#[cfg(feature = "wgpu")]
fn select_software_adapter(
    adapters: &[eframe::wgpu::Adapter],
    surface: Option<&eframe::wgpu::Surface<'_>>,
) -> Result<eframe::wgpu::Adapter, String> {
    let compatible = |a: &eframe::wgpu::Adapter| surface.is_none_or(|s| a.is_surface_supported(s));
    let is_cpu =
        |a: &eframe::wgpu::Adapter| a.get_info().device_type == eframe::wgpu::DeviceType::Cpu;
    let chosen = adapters
        .iter()
        .find(|a| is_cpu(a) && compatible(a))
        .or_else(|| adapters.iter().find(|a| is_cpu(a)))
        .inspect(|a| {
            tracing::info!(adapter = %a.get_info().name, "software rendering active");
        })
        .or_else(|| {
            // Report what actually happened instead of pretending the choice
            // took effect; the settings dialog reads this back.
            store_render_mode(tm_core::settings::RenderMode::Auto);
            tracing::warn!("no software rasterizer available; staying on the GPU adapter");
            adapters.iter().find(|a| compatible(a))
        })
        .ok_or_else(|| "no usable wgpu adapter".to_string())?;
    Ok(chosen.clone())
}

/// `TASKMAN_PRESENT=fifo|immediate|mailbox|autovsync` (diagnostics).
#[cfg(feature = "wgpu")]
fn present_mode_pref() -> eframe::wgpu::PresentMode {
    use eframe::wgpu::PresentMode;
    match std::env::var("TASKMAN_PRESENT")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "immediate" => PresentMode::Immediate,
        "mailbox" => PresentMode::Mailbox,
        "autovsync" => PresentMode::AutoVsync,
        _ => PresentMode::Fifo,
    }
}

/// `TASKMAN_FRAME_LATENCY=n` (diagnostics); 0 means "leave it to wgpu".
#[cfg(feature = "wgpu")]
fn frame_latency_pref() -> Option<u32> {
    match std::env::var("TASKMAN_FRAME_LATENCY")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(1),
    }
}

#[cfg(feature = "wgpu")]
fn compiled_wgpu_backends() -> eframe::wgpu::Backends {
    #[cfg(target_os = "windows")]
    {
        eframe::wgpu::Backends::DX12
    }
    #[cfg(target_os = "linux")]
    {
        eframe::wgpu::Backends::VULKAN
    }
    #[cfg(target_os = "macos")]
    {
        eframe::wgpu::Backends::METAL
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        eframe::wgpu::Backends::PRIMARY
    }
}

/// Thin native-shell wrapper. TaskManApp already owns all UI/application
/// state; this layer only records the viewport's desktop-space position and
/// delegates every application hook unchanged.
struct NativeApp {
    inner: app::TaskManApp,
    #[cfg(target_os = "windows")]
    tray: Option<TrayShell>,
    #[cfg(target_os = "windows")]
    tray_init_attempted: bool,
    hidden_to_tray: bool,
    initial_hide_pending: bool,
    #[cfg(target_os = "windows")]
    exit_requested: bool,
    /// Native window handle, cached the first time a frame hands it over.
    #[cfg(target_os = "windows")]
    hwnd: Option<isize>,
    /// Frames still owed before the window is uncloaked after a restore.
    /// See [`NativeApp::restore_window`]; 0 means "not cloaked by us".
    #[cfg(target_os = "windows")]
    uncloak_in: u8,
}

impl NativeApp {
    fn new(inner: app::TaskManApp, ctx: &eframe::egui::Context, initially_hidden: bool) -> Self {
        #[cfg(target_os = "windows")]
        {
            // The single-instance show event needs a repaint target even when
            // close-to-tray is disabled and no tray icon has been created.
            *tm_core::sync::lock(TRAY_CONTEXT.get_or_init(Default::default)) = Some(ctx.clone());
        }
        #[cfg(target_os = "windows")]
        let tray_requested = initially_hidden || inner.shared.settings.close_to_tray;
        #[cfg(target_os = "windows")]
        let tray = if tray_requested {
            TrayShell::new(true)
        } else {
            None
        };
        #[cfg(not(target_os = "windows"))]
        let _ = ctx;
        Self {
            inner,
            #[cfg(target_os = "windows")]
            tray,
            #[cfg(target_os = "windows")]
            tray_init_attempted: tray_requested,
            hidden_to_tray: initially_hidden,
            initial_hide_pending: initially_hidden,
            #[cfg(target_os = "windows")]
            exit_requested: false,
            #[cfg(target_os = "windows")]
            hwnd: None,
            #[cfg(target_os = "windows")]
            uncloak_in: 0,
        }
    }

    fn shutdown(&mut self) {
        #[cfg(target_os = "windows")]
        if let Some(tray) = &mut self.tray {
            tray.shutdown();
        }
        if self.inner.shared.settings.save_config && self.inner.shared.settings.remember_window {
            ui_state::save();
        }
        self.inner.shutdown();
    }
}

impl eframe::App for NativeApp {
    fn logic(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        #[cfg(target_os = "windows")]
        poll_show_event_fallback(ctx);
        // Before the tray actions, so a restore requested THIS frame cannot
        // spend one of the frames it is owed on the frame that requested it.
        #[cfg(target_os = "windows")]
        self.finish_restore(ctx);
        #[cfg(target_os = "windows")]
        self.handle_tray_actions(ctx);
        if self.initial_hide_pending {
            #[cfg(target_os = "windows")]
            if self.tray.is_some() {
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(false));
            } else {
                self.hidden_to_tray = false;
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
            }
            #[cfg(not(target_os = "windows"))]
            {
                self.hidden_to_tray = false;
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
            }
            self.initial_hide_pending = false;
        }
        <app::TaskManApp as eframe::App>::logic(&mut self.inner, ctx, frame);
    }

    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        #[cfg(target_os = "windows")]
        if self.hwnd.is_none() {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(handle) = frame.window_handle()
                && let RawWindowHandle::Win32(win32) = handle.as_raw()
            {
                self.hwnd = Some(win32.hwnd.get());
                // Later launches read this to tell a slow instance from a
                // wedged one.
                tm_platform::win::instance::publish_window(win32.hwnd.get());
            }
        }
        <app::TaskManApp as eframe::App>::ui(&mut self.inner, ui, frame);
        #[cfg(target_os = "windows")]
        {
            self.handle_tray_actions(ui.ctx());
            let tray_enabled = self.inner.shared.settings.close_to_tray || self.hidden_to_tray;
            if tray_enabled && self.tray.is_none() && !self.tray_init_attempted {
                self.tray_init_attempted = true;
                self.tray = TrayShell::new(tray_enabled);
            }
            if let Some(tray) = &mut self.tray {
                tray.set_visible(tray_enabled);
                tray.sync_menu_theme(ui.ctx().theme());
            }
            let close_requested = ui.ctx().input(|input| input.viewport().close_requested());
            if close_requested && PROGRAMMATIC_EXIT.load(std::sync::atomic::Ordering::Acquire) {
                self.exit_requested = true;
            }
            if close_requested
                && !self.exit_requested
                && self.inner.shared.settings.close_to_tray
                && self.tray.is_some()
            {
                ui.ctx()
                    .send_viewport_cmd(eframe::egui::ViewportCommand::CancelClose);
                ui.ctx()
                    .send_viewport_cmd(eframe::egui::ViewportCommand::Visible(false));
                self.hidden_to_tray = true;
            }
        }
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

    #[cfg(feature = "glow")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }

    #[cfg(not(feature = "glow"))]
    fn on_exit(&mut self) {
        self.shutdown();
    }
}

#[cfg(target_os = "windows")]
const TRAY_ACTION_OPEN: u8 = 1;
#[cfg(target_os = "windows")]
const TRAY_ACTION_EXIT: u8 = 2;
#[cfg(target_os = "windows")]
static TRAY_ACTION: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
#[cfg(target_os = "windows")]
static TRAY_CONTEXT: std::sync::OnceLock<std::sync::Mutex<Option<eframe::egui::Context>>> =
    std::sync::OnceLock::new();
#[cfg(target_os = "windows")]
static TRAY_HANDLERS: std::sync::Once = std::sync::Once::new();

#[cfg(target_os = "windows")]
fn signal_tray(action: u8) {
    TRAY_ACTION.fetch_max(action, std::sync::atomic::Ordering::AcqRel);
    if let Some(context) = TRAY_CONTEXT.get()
        && let Some(context) = tm_core::sync::lock(context).as_ref()
    {
        context.request_repaint();
    }
}

/// Only armed when the show-request listener thread could not be created;
/// the UI then carries the request on its own repaint cadence.
#[cfg(target_os = "windows")]
fn poll_show_event_fallback(ctx: &eframe::egui::Context) {
    if !tm_platform::win::instance::show_polling_armed() {
        return;
    }
    if tm_platform::win::instance::poll_show_request() {
        signal_tray(TRAY_ACTION_OPEN);
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(250));
}

/// Make Win32 popup menus (the notification-area menu is one) follow a
/// light or dark theme.
///
/// There is no supported API for this. Windows themes menus from a
/// PROCESS-wide preference that Explorer sets through two undocumented
/// uxtheme exports available by ordinal only: 135 `SetPreferredAppMode` and
/// 136 `FlushMenuThemes`. muda's `MenuTheme` is no substitute — it documents
/// itself as affecting the menu BAR of a window, not popup or context menus.
///
/// Everything here fails soft: a missing export or an older Windows simply
/// leaves the menu in its default (light) colors.
#[cfg(target_os = "windows")]
fn set_popup_menu_theme(dark: bool) {
    use windows::Win32::Foundation::{FARPROC, HMODULE};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::{PCSTR, w};

    /// `PreferredAppMode`: 0 Default, 1 AllowDark, 2 ForceDark, 3 ForceLight.
    /// Force rather than allow, because the app's own theme setting may
    /// deliberately disagree with the system one.
    const FORCE_DARK: i32 = 2;
    const FORCE_LIGHT: i32 = 3;

    static UXTHEME: std::sync::OnceLock<Option<(usize, usize)>> = std::sync::OnceLock::new();
    let exports = *UXTHEME.get_or_init(|| unsafe {
        let module: HMODULE = LoadLibraryW(w!("uxtheme.dll")).ok()?;
        // Ordinals, not names: these two are exported without names.
        let by_ordinal = |ordinal: u16| -> FARPROC {
            GetProcAddress(module, PCSTR(ordinal as usize as *const u8))
        };
        let set_mode = by_ordinal(135)? as usize;
        let flush = by_ordinal(136)? as usize;
        Some((set_mode, flush))
    });
    let Some((set_mode, flush)) = exports else {
        return;
    };
    unsafe {
        let set_preferred_app_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
        let flush_menu_themes: extern "system" fn() = std::mem::transmute(flush);
        set_preferred_app_mode(if dark { FORCE_DARK } else { FORCE_LIGHT });
        flush_menu_themes();
    }
}

/// Handle of the dedicated tray thread; 0 until that thread has created the
/// icon window — which is also what gives the thread its message queue.
#[cfg(target_os = "windows")]
static TRAY_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Visibility the tray thread should apply. The UI thread writes it, the
/// tray thread reads it once at startup and again on every apply message.
#[cfg(target_os = "windows")]
static TRAY_DESIRED_VISIBLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Thread message asking the tray thread to re-apply the desired visibility.
/// Thread messages carry no window, so it never reaches `tray_proc`.
#[cfg(target_os = "windows")]
const TRAY_MSG_APPLY_VISIBLE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

#[cfg(target_os = "windows")]
static TRAY_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Show the notification-area context menu.
///
/// This always runs on the dedicated tray thread, never on the UI thread:
/// `TrackPopupMenuEx` pumps a modal message loop, and running that loop
/// inside the UI thread's egui/winit dispatch re-entered the frame pipeline
/// — repaints, input and tray messages arrived in orders the framework is
/// not built for, which froze the app in ways that were hard to reproduce.
/// On the tray thread the only thing a open menu blocks is a thread whose
/// one other job (delivering tray clicks) belongs to the open menu anyway.
#[cfg(target_os = "windows")]
fn show_native_tray_menu() {
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreatePopupMenu, DestroyMenu, GetCursorPos, InsertMenuW, MF_BYPOSITION, MF_STRING,
        PostMessageW, SetForegroundWindow, SetMenuDefaultItem, TPM_BOTTOMALIGN, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_NULL,
    };
    use windows::core::PCWSTR;

    // The handler that calls this fires from the icon's own window
    // procedure, so the icon window exists and lives on this thread —
    // exactly what `TrackPopupMenuEx` requires of its hwnd argument.
    let tray_raw = TRAY_HWND.load(std::sync::atomic::Ordering::Acquire);
    if tray_raw == 0 {
        return;
    }
    let hwnd = HWND(tray_raw as *mut _);

    let Ok(hmenu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };

    let open_text = tm_core::i18n::tr(tm_core::i18n::K::TrayOpen);
    let exit_text = tm_core::i18n::tr(tm_core::i18n::K::TrayExit);
    let wide_open: Vec<u16> = open_text.encode_utf16().chain([0]).collect();
    let wide_exit: Vec<u16> = exit_text.encode_utf16().chain([0]).collect();

    unsafe {
        let _ = InsertMenuW(
            hmenu,
            0,
            MF_BYPOSITION | MF_STRING,
            1,
            PCWSTR(wide_open.as_ptr()),
        );
        let _ = InsertMenuW(
            hmenu,
            1,
            MF_BYPOSITION | MF_STRING,
            2,
            PCWSTR(wide_exit.as_ptr()),
        );
        let _ = SetMenuDefaultItem(hmenu, 1, 0);

        // Explorer grants the foreground right for a tray click to the
        // window registered with the icon; without this the menu would not
        // dismiss on outside clicks.
        let _ = SetForegroundWindow(hwnd);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Bottom-aligned: the cursor sits inside the taskbar, so the menu is
        // anchored with its bottom edge there and grows UPWARD, over the
        // taskbar instead of overlapping (and fighting with) it.
        let cmd = TrackPopupMenuEx(
            hmenu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(hmenu);

        match cmd.0 {
            1 => signal_tray(TRAY_ACTION_OPEN),
            2 => signal_tray(TRAY_ACTION_EXIT),
            _ => {}
        }
    }
}

/// Entry of the dedicated tray thread: build the icon here, serve tray
/// events (whose handler runs on this thread, inside the icon's window
/// procedure), and apply visibility changes asked for by the UI thread.
///
/// The icon is created on this thread on purpose: a `tray_icon::TrayIcon`
/// window is bound to the thread that made it, so the popup menu — the only
/// blocking part of the tray — pumps its modal loop HERE and never inside
/// the UI thread's event dispatch.
#[cfg(target_os = "windows")]
fn tray_thread_main() {
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, TranslateMessage,
    };

    TRAY_HANDLERS.call_once(|| {
        tray_icon::TrayIconEvent::set_event_handler(Some(|event: tray_icon::TrayIconEvent| {
            match event {
                tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                }
                | tray_icon::TrayIconEvent::DoubleClick {
                    button: tray_icon::MouseButton::Left,
                    ..
                } => {
                    signal_tray(TRAY_ACTION_OPEN);
                }
                tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Right,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                } => {
                    show_native_tray_menu();
                }
                _ => {}
            }
        }));
    });

    let icon_data = icon_data();
    let icon = match tray_icon::Icon::from_rgba(icon_data.rgba, icon_data.width, icon_data.height) {
        Ok(icon) => icon,
        Err(error) => {
            tracing::warn!(%error, "cannot create tray icon image");
            return;
        }
    };
    let icon = match tray_icon::TrayIconBuilder::new()
        .with_tooltip(tm_core::i18n::tr(tm_core::i18n::K::WindowTitle))
        .with_icon(icon)
        .build()
    {
        Ok(icon) => icon,
        Err(error) => {
            tracing::warn!(%error, "cannot create notification-area icon");
            return;
        }
    };
    TRAY_HWND.store(
        icon.window_handle() as isize,
        std::sync::atomic::Ordering::Release,
    );
    // Publish the thread id only after the icon window exists: the window
    // creation is what gives this thread a message queue, and a post to a
    // queue-less thread is lost. `TrayShell::set_visible` documents why the
    // visibility handoff stays race-free around this publish.
    TRAY_THREAD_ID.store(
        unsafe { GetCurrentThreadId() },
        std::sync::atomic::Ordering::SeqCst,
    );
    let _ = icon.set_visible(TRAY_DESIRED_VISIBLE.load(std::sync::atomic::Ordering::SeqCst));

    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // 0 = WM_QUIT (posted by `TrayShell::shutdown`), -1 = error.
        if result.0 <= 0 {
            break;
        }
        if msg.hwnd.0.is_null() && msg.message == TRAY_MSG_APPLY_VISIBLE {
            let _ =
                icon.set_visible(TRAY_DESIRED_VISIBLE.load(std::sync::atomic::Ordering::SeqCst));
            continue;
        }
        let _ = unsafe { TranslateMessage(&msg) };
        unsafe { DispatchMessageW(&msg) };
    }

    // Dropping the icon on its own thread removes the notification-area
    // entry and destroys the window; `DestroyWindow` would fail from any
    // other thread.
    TRAY_HWND.store(0, std::sync::atomic::Ordering::Release);
    TRAY_THREAD_ID.store(0, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(target_os = "windows")]
struct TrayShell {
    /// Last value this handle pushed into `TRAY_DESIRED_VISIBLE`.
    visible: Option<bool>,
    /// Theme the popup menu is currently themed for; `None` until applied.
    menu_dark: Option<bool>,
    shutdown: bool,
}

#[cfg(target_os = "windows")]
impl TrayShell {
    /// Spawn the tray thread and remember what it should show first. The
    /// icon appears as soon as the thread has created it; a failed spawn
    /// means no tray, the same visible outcome as a failed icon creation.
    fn new(initially_visible: bool) -> Option<Self> {
        TRAY_DESIRED_VISIBLE.store(initially_visible, std::sync::atomic::Ordering::SeqCst);
        std::thread::Builder::new()
            .name("tm-tray".into())
            .spawn(tray_thread_main)
            .ok()?;
        Some(Self {
            visible: Some(initially_visible),
            menu_dark: None,
            shutdown: false,
        })
    }

    fn set_visible(&mut self, visible: bool) {
        if self.shutdown || self.visible == Some(visible) {
            return;
        }
        self.visible = Some(visible);
        // Store BEFORE reading the thread id; the tray thread publishes its
        // id BEFORE its first read of the desired state. SeqCst on both
        // sides makes the two overlap cases mutually exclusive: if this
        // load still sees 0, our store is ordered before the thread's first
        // read and the initial apply already covers it; otherwise the apply
        // message below does. A failed post (queue not up yet) is fine for
        // the same reason.
        TRAY_DESIRED_VISIBLE.store(visible, std::sync::atomic::Ordering::SeqCst);
        let tid = TRAY_THREAD_ID.load(std::sync::atomic::Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                use windows::Win32::Foundation::{LPARAM, WPARAM};
                use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
                let _ = PostThreadMessageW(tid, TRAY_MSG_APPLY_VISIBLE, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Keep the notification-area menu on the app's effective theme, which
    /// follows Windows unless the user pinned light or dark in settings.
    fn sync_menu_theme(&mut self, theme: eframe::egui::Theme) {
        let dark = theme == eframe::egui::Theme::Dark;
        if self.menu_dark == Some(dark) {
            return;
        }
        set_popup_menu_theme(dark);
        self.menu_dark = Some(dark);
    }

    /// Hide the icon and end the tray thread, so a clean exit removes the
    /// notification-area entry immediately instead of leaving a ghost icon
    /// until Explorer notices the dead process. The thread drops the icon
    /// on its own thread, which is the only thread allowed to destroy its
    /// window.
    fn shutdown(&mut self) {
        if self.shutdown {
            return;
        }
        self.shutdown = true;
        self.visible = Some(false);
        TRAY_DESIRED_VISIBLE.store(false, std::sync::atomic::Ordering::SeqCst);
        // Claim the id so late visibility changes cannot post anywhere, then
        // ask the thread to quit; it still finishes any open menu first.
        let tid = TRAY_THREAD_ID.swap(0, std::sync::atomic::Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                use windows::Win32::Foundation::{LPARAM, WPARAM};
                use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

#[cfg(target_os = "windows")]
impl NativeApp {
    /// Bring the window back from the tray without the white flash.
    ///
    /// `ShowWindow` alone composes an unpainted window: a hidden window gets
    /// no `WM_PAINT`, so the app cannot have a frame ready before it appears,
    /// and DWM fills the gap with the empty surface. Cloaking first means the
    /// window is shown — and therefore painting — while still invisible to the
    /// compositor; [`Self::finish_restore`] uncloaks it once a real frame has
    /// been presented.
    fn restore_window(&mut self, ctx: &eframe::egui::Context) {
        let was_hidden = self.hidden_to_tray;
        self.hidden_to_tray = false;
        // A launch that arrives while this instance is still starting
        // minimized must win: the pending hide would otherwise put the
        // window straight back into the tray in the same frame.
        self.initial_hide_pending = false;
        // Only a window that is genuinely hidden needs the cloak: it is the
        // one that would otherwise be composed before it has painted. A
        // minimized window keeps its last frame, and cloaking it would only
        // add a blink to the restore animation.
        if was_hidden && let Some(hwnd) = self.hwnd {
            tm_platform::set_window_cloaked(hwnd, true);
            // Two frames. `Visible(true)` below is queued and applied after
            // THIS frame, so the first frame that can present into a visible
            // window is the next one; the one after that reveals it.
            self.uncloak_in = 2;
        }
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
        // Un-minimize BEFORE asking for focus. Focus is a no-op on a
        // minimized window (winit checks that explicitly), which is why a
        // hotkey press used to leave a minimized task manager exactly where
        // it was.
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
        // Tell a waiting launch that the request has been processed by the
        // thread that actually draws: that, and not the request arriving, is
        // what proves this instance is alive enough to keep the request.
        #[cfg(target_os = "windows")]
        tm_platform::win::instance::acknowledge_show();
        // The UI is event driven and the engine may be paused, so the frames
        // the countdown needs have to be asked for explicitly.
        ctx.request_repaint();
    }

    /// Uncloak once the frames owed by [`Self::restore_window`] have gone by.
    ///
    /// Driven from the START of every frame rather than a one-shot callback,
    /// for two reasons. A frame's paint has been presented by the time the
    /// next one begins, so counting here means the window is only revealed
    /// once real content is behind it. And running unconditionally every
    /// frame means the window can never be left invisible: whatever happens
    /// in between, the countdown keeps ticking and reaches zero.
    fn finish_restore(&mut self, ctx: &eframe::egui::Context) {
        if self.uncloak_in == 0 {
            return;
        }
        self.uncloak_in -= 1;
        if self.uncloak_in == 0 {
            if let Some(hwnd) = self.hwnd {
                tm_platform::set_window_cloaked(hwnd, false);
            }
        } else {
            // The countdown must not stall on an idle, event-driven UI.
            ctx.request_repaint();
        }
    }

    fn handle_tray_actions(&mut self, ctx: &eframe::egui::Context) {
        match TRAY_ACTION.swap(0, std::sync::atomic::Ordering::AcqRel) {
            TRAY_ACTION_OPEN => {
                self.restore_window(ctx);
            }
            TRAY_ACTION_EXIT => {
                self.exit_requested = true;
                self.hidden_to_tray = false;
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
            }
            _ => {}
        }
    }
}

fn parse_size_arg(s: &str) -> Option<[f32; 2]> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?);
    (w >= 200.0 && h >= 150.0).then_some([w, h])
}

fn icon_data() -> eframe::egui::IconData {
    const S: usize = 64;
    const RAW: &[u8] = include_bytes!("../assets/icon_64.raw");
    eframe::egui::IconData {
        width: S as u32,
        height: S as u32,
        rgba: RAW.to_vec(),
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

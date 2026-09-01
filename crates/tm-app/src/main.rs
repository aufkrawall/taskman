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
mod selfcheck;
mod tabs;
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

    // Acquire only after the optional elevation handoff. Otherwise the
    // unelevated parent would still own the mutex when its elevated child
    // starts, causing that child to signal the parent and exit immediately.
    #[cfg(target_os = "windows")]
    let elevation_handoff = args
        .iter()
        .any(|argument| argument == "--single-instance-handoff");
    let _single_instance = match SingleInstance::acquire(elevation_handoff) {
        Ok(Some(instance)) => instance,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, "single-instance coordination unavailable");
            SingleInstance::uncoordinated()
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
    let initially_hidden = args
        .iter()
        .any(|argument| argument == "--minimized-to-tray");
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
        let creator = move |cc: &eframe::CreationContext<'_>| {
            StartupTrace::mark("creation_context_enter");
            theme::apply_startup(&cc.egui_ctx);
            fonts::install_async(cc.egui_ctx.clone());
            let application = app::TaskManApp::new(cc, use_mock, app_settings, initial_tab);
            Ok(
                Box::new(NativeApp::new(application, &cc.egui_ctx, start_hidden))
                    as Box<dyn eframe::App>,
            )
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
        let tray = tray_requested.then(|| TrayShell::new(ctx)).flatten();
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
        }
    }

    fn shutdown(&mut self) {
        if self.inner.shared.settings.save_config && self.inner.shared.settings.remember_window {
            ui_state::save();
        }
        self.inner.shutdown();
    }
}

/// Per-session single-instance guard. A second Ctrl+Shift+Esc/shortcut launch
/// signals the existing process to restore from the tray instead of starting
/// another sampler and renderer under heavy load.
#[cfg(target_os = "windows")]
struct SingleInstance {
    _mutex: windows::Win32::Foundation::HANDLE,
    _show_event: windows::Win32::Foundation::HANDLE,
    listener: Option<std::thread::JoinHandle<()>>,
    owns_mutex: bool,
}

#[cfg(target_os = "windows")]
impl SingleInstance {
    fn acquire(elevation_handoff: bool) -> windows::core::Result<Option<Self>> {
        use windows::Win32::Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, ERROR_TIMEOUT, GetLastError, HANDLE, WAIT_ABANDONED,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        use windows::Win32::System::Threading::{
            CreateEventW, CreateMutexW, INFINITE, SetEvent, WaitForSingleObject,
        };
        use windows::core::PCWSTR;

        let mutex_name: Vec<u16> = "Local\\TaskMan.Instance.v1"
            .encode_utf16()
            .chain([0])
            .collect();
        let event_name: Vec<u16> = "Local\\TaskMan.Show.v1".encode_utf16().chain([0]).collect();
        let mutex = unsafe { CreateMutexW(None, true, PCWSTR(mutex_name.as_ptr()))? };
        let already_exists = unsafe { GetLastError() == ERROR_ALREADY_EXISTS };
        let event = match unsafe { CreateEventW(None, false, false, PCWSTR(event_name.as_ptr())) } {
            Ok(event) => event,
            Err(error) => {
                unsafe {
                    let _ = CloseHandle(mutex);
                }
                return Err(error);
            }
        };
        if already_exists {
            if elevation_handoff {
                // The old instance queues its close immediately after the
                // elevated process is launched. Wait for it to release/abandon
                // ownership so the replacement cannot be rejected as a
                // duplicate. On a pathological timeout, return an error and
                // let the caller choose the uncoordinated reliability fallback.
                let wait = unsafe { WaitForSingleObject(mutex, 15_000) };
                if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
                    let error = if wait == WAIT_TIMEOUT {
                        ERROR_TIMEOUT.to_hresult()
                    } else {
                        windows::core::HRESULT::from_thread()
                    };
                    unsafe {
                        let _ = CloseHandle(event);
                        let _ = CloseHandle(mutex);
                    }
                    return Err(windows::core::Error::from_hresult(error));
                }
                // Continue into the common listener setup: the replacement is
                // now the primary instance and must respond to later launches.
            } else {
                unsafe {
                    if let Err(error) = SetEvent(event) {
                        let _ = CloseHandle(event);
                        let _ = CloseHandle(mutex);
                        return Err(error);
                    }
                    let _ = CloseHandle(event);
                    let _ = CloseHandle(mutex);
                }
                return Ok(None);
            }
        }

        SHOW_LISTENER_SHUTDOWN.store(false, std::sync::atomic::Ordering::Release);
        let event_raw = event.0 as usize;
        let listener = match std::thread::Builder::new()
            .name("tm-show-listener".into())
            .spawn(move || {
                loop {
                    let event = HANDLE(event_raw as *mut core::ffi::c_void);
                    if unsafe { WaitForSingleObject(event, INFINITE) } != WAIT_OBJECT_0 {
                        break;
                    }
                    if SHOW_LISTENER_SHUTDOWN.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    signal_tray(TRAY_ACTION_OPEN);
                }
            }) {
            Ok(listener) => Some(listener),
            Err(error) => {
                // Extremely low-resource fallback: the UI polls this event on
                // its existing repaint cadence, so a failed listener allocation
                // does not permanently break Ctrl+Shift+Esc restore behavior.
                tracing::warn!(%error, "single-instance listener could not start; using UI polling");
                SHOW_EVENT_FALLBACK.store(event.0 as usize, std::sync::atomic::Ordering::Release);
                None
            }
        };
        Ok(Some(Self {
            _mutex: mutex,
            _show_event: event,
            listener,
            owns_mutex: true,
        }))
    }

    fn uncoordinated() -> Self {
        Self {
            _mutex: windows::Win32::Foundation::HANDLE::default(),
            _show_event: windows::Win32::Foundation::HANDLE::default(),
            listener: None,
            owns_mutex: false,
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for SingleInstance {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{ReleaseMutex, SetEvent};

        unsafe {
            SHOW_EVENT_FALLBACK.store(0, std::sync::atomic::Ordering::Release);
            if let Some(listener) = self.listener.take() {
                SHOW_LISTENER_SHUTDOWN.store(true, std::sync::atomic::Ordering::Release);
                if !self._show_event.is_invalid() {
                    let _ = SetEvent(self._show_event);
                }
                let _ = listener.join();
            }
            if self.owns_mutex && !self._mutex.is_invalid() {
                let _ = ReleaseMutex(self._mutex);
            }
            if !self._show_event.is_invalid() {
                let _ = CloseHandle(self._show_event);
            }
            if !self._mutex.is_invalid() {
                let _ = CloseHandle(self._mutex);
            }
        }
    }
}

impl eframe::App for NativeApp {
    fn logic(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        #[cfg(target_os = "windows")]
        poll_show_event_fallback(ctx);
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
        <app::TaskManApp as eframe::App>::ui(&mut self.inner, ui, frame);
        #[cfg(target_os = "windows")]
        {
            self.handle_tray_actions(ui.ctx());
            let tray_enabled = self.inner.shared.settings.close_to_tray || self.hidden_to_tray;
            if tray_enabled && self.tray.is_none() && !self.tray_init_attempted {
                self.tray_init_attempted = true;
                self.tray = TrayShell::new(ui.ctx());
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
static SHOW_EVENT_FALLBACK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(target_os = "windows")]
static SHOW_LISTENER_SHUTDOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
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

#[cfg(target_os = "windows")]
fn poll_show_event_fallback(ctx: &eframe::egui::Context) {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::WaitForSingleObject;

    let raw = SHOW_EVENT_FALLBACK.load(std::sync::atomic::Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let event = HANDLE(raw as *mut core::ffi::c_void);
    if unsafe { WaitForSingleObject(event, 0) } == WAIT_OBJECT_0 {
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

#[cfg(target_os = "windows")]
struct TrayShell {
    icon: tray_icon::TrayIcon,
    visible: bool,
    /// Theme the popup menu is currently themed for; `None` until applied.
    menu_dark: Option<bool>,
}

#[cfg(target_os = "windows")]
impl TrayShell {
    fn new(ctx: &eframe::egui::Context) -> Option<Self> {
        *tm_core::sync::lock(TRAY_CONTEXT.get_or_init(Default::default)) = Some(ctx.clone());
        TRAY_HANDLERS.call_once(|| {
            tray_icon::menu::MenuEvent::set_event_handler(Some(
                |event: tray_icon::menu::MenuEvent| match event.id.0.as_str() {
                    "taskman-open" => signal_tray(TRAY_ACTION_OPEN),
                    "taskman-exit" => signal_tray(TRAY_ACTION_EXIT),
                    _ => {}
                },
            ));
            tray_icon::TrayIconEvent::set_event_handler(Some(|event: tray_icon::TrayIconEvent| {
                // A SINGLE left click restores the window, like every other
                // notification-area icon on Windows. Double click keeps
                // working (it also emits two Click events, and reopening an
                // already open window is a no-op). The right button belongs
                // to the context menu, so it is deliberately not matched.
                let opens = matches!(
                    event,
                    tray_icon::TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    } | tray_icon::TrayIconEvent::DoubleClick {
                        button: tray_icon::MouseButton::Left,
                        ..
                    }
                );
                if opens {
                    signal_tray(TRAY_ACTION_OPEN);
                }
            }));
        });

        let menu = tray_icon::menu::Menu::new();
        let open = tray_icon::menu::MenuItem::with_id(
            "taskman-open",
            tm_core::i18n::tr(tm_core::i18n::K::TrayOpen),
            true,
            None,
        );
        let exit = tray_icon::menu::MenuItem::with_id(
            "taskman-exit",
            tm_core::i18n::tr(tm_core::i18n::K::TrayExit),
            true,
            None,
        );
        if let Err(error) = menu.append_items(&[&open, &exit]) {
            tracing::warn!(%error, "cannot create tray menu");
            return None;
        }
        let icon_data = icon_data();
        let icon =
            match tray_icon::Icon::from_rgba(icon_data.rgba, icon_data.width, icon_data.height) {
                Ok(icon) => icon,
                Err(error) => {
                    tracing::warn!(%error, "cannot create tray icon image");
                    return None;
                }
            };
        match tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(tm_core::i18n::tr(tm_core::i18n::K::WindowTitle))
            .with_icon(icon)
            .build()
        {
            Ok(icon) => {
                let _ = icon.set_visible(false);
                Some(Self {
                    icon,
                    visible: false,
                    menu_dark: None,
                })
            }
            Err(error) => {
                tracing::warn!(%error, "cannot create notification-area icon");
                None
            }
        }
    }

    fn set_visible(&mut self, visible: bool) {
        if visible == self.visible {
            return;
        }
        match self.icon.set_visible(visible) {
            Ok(()) => self.visible = visible,
            Err(error) => tracing::warn!(%error, visible, "cannot update tray visibility"),
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
}

#[cfg(target_os = "windows")]
impl NativeApp {
    fn handle_tray_actions(&mut self, ctx: &eframe::egui::Context) {
        match TRAY_ACTION.swap(0, std::sync::atomic::Ordering::AcqRel) {
            TRAY_ACTION_OPEN => {
                self.hidden_to_tray = false;
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
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
            rgba[i + 3] = if in_chip || pin_v || pin_h { 255 } else { 0 };
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

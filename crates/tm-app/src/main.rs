//! taskman — a cross-platform task manager.
//!
//! Windows note: the release build hides the console (`windows` subsystem);
//! CLI output (--selfcheck) re-attaches to the parent console when present.

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
// Win32 interop needs `unsafe`; keep every unsafe operation in an explicit
// block so it stays reviewable.
#![deny(unsafe_op_in_unsafe_fn)]

// A build with no renderer feature fails deep inside eframe with a confusing
// "no `run_native` in `eframe`" error; say what is actually wrong instead.
#[cfg(not(any(feature = "software", feature = "wgpu", feature = "glow")))]
compile_error!("tm-app requires at least one renderer feature: `software`, `wgpu`, or `glow`");

mod action_executor;
mod app;
mod app_ui;
mod dialogs;
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
/// System tray icon: the contract both desktops share plus the Linux icon.
mod tray;
mod ui_state;
mod widgets;

// Theme palette and font setup live in `tm-ui` so the setup installer renders
// the exact same Windows 11 look; re-exported here so the existing
// `crate::theme` / `crate::fonts` paths keep working unchanged.
pub use tm_ui::fonts;
pub use tm_ui::theme;

use std::time::Instant;

use tm_core::settings::RenderMode;

#[cfg(target_os = "linux")]
use tray::TrayShell;
#[cfg(any(target_os = "windows", target_os = "linux"))]
use tray::{INITIAL_HIDE_TRAY_WAIT, InitialHideAction, initial_hide_action};

/// The backend that initialized the current window.
static ACTIVE_RENDER_MODE: std::sync::Mutex<RenderMode> = std::sync::Mutex::new(RenderMode::Cpu);

pub fn active_render_mode() -> RenderMode {
    *tm_core::sync::lock(&ACTIVE_RENDER_MODE)
}

fn effective_render_mode(settings: &tm_core::settings::Settings) -> RenderMode {
    let requested = std::env::var("TASKMAN_GPU")
        .ok()
        .and_then(|value| RenderMode::from_cfg(&value))
        .unwrap_or(settings.render_mode);
    configured_render_mode(requested)
}

/// A configuration copied from a different OS falls back to the local default.
fn configured_render_mode(requested: RenderMode) -> RenderMode {
    if renderer_choices().contains(&requested) {
        requested
    } else {
        renderer_choices().first().copied().unwrap_or_default()
    }
}

pub(crate) fn renderer_choices() -> &'static [RenderMode] {
    const CHOICES: &[RenderMode] = &[
        #[cfg(feature = "software")]
        RenderMode::Cpu,
        #[cfg(feature = "wgpu")]
        native_gpu_mode(),
        #[cfg(all(feature = "glow", not(any(target_os = "windows", target_os = "linux"))))]
        RenderMode::OpenGl,
        #[cfg(all(
            feature = "glow",
            not(any(feature = "software", feature = "wgpu")),
            any(target_os = "windows", target_os = "linux")
        ))]
        RenderMode::OpenGl,
    ];
    CHOICES
}

#[cfg(feature = "wgpu")]
const fn native_gpu_mode() -> RenderMode {
    #[cfg(target_os = "windows")]
    {
        RenderMode::D3d12
    }
    #[cfg(target_os = "linux")]
    {
        RenderMode::Vulkan
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        RenderMode::Metal
    }
}

const APP_ID: &str = "io.github.aufkrawall.Taskman";
const DEFAULT_WINDOW_SIZE: [f32; 2] = [1280.0, 800.0];

/// Set by a path that closes this window as part of a handover (a relaunch, the
/// core-service switch) rather than because the user closed it: close-to-tray
/// must let that close through, or the old instance would linger with no window
/// while the replacement waits for it.
static PROGRAMMATIC_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark the next close as a handover rather than a user close.
///
/// Deliberately not platform-gated: every desktop that can close to a tray has
/// to tell those two apart, and a desktop without one simply never reads the
/// flag. A gated setter with an ungated caller is how this invariant broke once.
pub(crate) fn request_programmatic_exit() {
    PROGRAMMATIC_EXIT.store(true, std::sync::atomic::Ordering::Release);
}

pub struct StartupTrace;

impl StartupTrace {
    /// Log one startup milestone.
    ///
    /// `ms` counts from the first mark (`process_start`, the first statement of
    /// `main`), and `since_ms` is the gap from the mark before it — the number
    /// that answers "what delayed this phase". Callers mark one-shot
    /// milestones only: `info` is the highest level the log file keeps, and a
    /// per-frame phase would bury the ones that matter.
    pub fn mark(name: &'static str) {
        static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let elapsed = T0.get_or_init(Instant::now).elapsed().as_millis() as u64;
        let previous = LAST_MS.swap(elapsed, std::sync::atomic::Ordering::Relaxed);
        let gap = elapsed.saturating_sub(previous);
        tracing::info!(ms = elapsed, since_ms = gap, phase = name, "startup");
    }
}

fn main() {
    // First statement: this mark is the origin every later `ms` is measured
    // from. The record itself is dropped (no subscriber yet in the early-log
    // configuration), which is the point — the loader and CRT time before it
    // are not ours to attribute.
    StartupTrace::mark("process_start");
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Root-only diagnostic/helper entry point retained for a future protected
    // Linux install. The portable GUI never asks pkexec to execute this
    // user-writable binary; trusted service elevation launches systemctl.
    // Handle it before logging, locale, settings or renderer initialization.
    #[cfg(target_os = "linux")]
    if let Some(code) = tm_platform::linux::polkit::handle_helper(&args) {
        std::process::exit(code);
    }

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

    // Short-lived elevated helper for opening the real Windows Task Manager
    // while our IFEO replacement is enabled. The platform routine performs
    // the full delete/launch/restore transaction before this process exits.
    #[cfg(target_os = "windows")]
    if args
        .iter()
        .any(|argument| argument == "--native-taskmgr-helper")
    {
        let code = match tm_platform::win::launch_native_task_manager_direct() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("taskman: native Task Manager launch failed: {error}");
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
    let code_sample_selfcheck = args
        .iter()
        .any(|argument| argument == "--selfcheck-code-samples");

    #[cfg(all(target_os = "windows", not(debug_assertions)))]
    if selfcheck || code_sample_selfcheck || verbose {
        attach_parent_console();
    }

    if selfcheck || code_sample_selfcheck || verbose {
        // The appender guard is owned by `tm_core::logging` for the process
        // lifetime. It used to be returned here and bound to a `let _log_guard`
        // inside this block, which dropped it one line later and shut the
        // writer thread down: every log file this app produced held exactly
        // one line, the "logging initialized" record emitted before the drop.
        tm_core::logging::init(tm_core::logging::LogConfig {
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
    if code_sample_selfcheck {
        let code = selfcheck::run_code_samples();
        std::process::exit(code);
    }

    #[cfg(target_os = "windows")]
    if args
        .iter()
        .any(|argument| argument == tm_platform::win::instance::UNRESPONSIVE_ARG)
    {
        tm_platform::win::instance::mark_unresponsive();
    }

    // Fast path: if this session already runs an instance, hand off to it
    // immediately. This runs BEFORE redirect_to_installed_gui, settings
    // loading, or renderer setup to avoid file hashing, SCM queries, and
    // redundant process spawning for a window that is already open.
    #[cfg(target_os = "windows")]
    let elevation_handoff = args
        .iter()
        .any(|argument| argument == "--single-instance-handoff");
    #[cfg(target_os = "windows")]
    if !elevation_handoff
        && !tm_platform::win::instance::is_unresponsive_marked()
        && tm_platform::win::instance::activate_existing()
            == tm_platform::win::instance::Activation::Activated
    {
        tracing::info!("handed this launch to the running instance");
        return;
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
        && !tm_platform::win::instance::is_unresponsive_marked()
        && tm_platform::win::instance::activate_existing()
            == tm_platform::win::instance::Activation::Activated
    {
        tracing::info!("handed this launch to the running instance");
        return;
    }

    // Detect and hand off to an existing Linux instance before settings,
    // locale, or renderer initialization. A wedged owner is not killed and
    // cannot make this launch fail: the handoff deadline exits as an ordinary
    // uncoordinated GUI process instead.
    #[cfg(target_os = "linux")]
    let (_linux_activation_owner, native_activation) = if mock {
        (None, NativeActivation::default())
    } else {
        match tm_platform::linux::activation::acquire() {
            tm_platform::linux::activation::ActivationAcquisition::HandedOff => {
                tracing::info!("handed this launch to the running Linux instance");
                return;
            }
            tm_platform::linux::activation::ActivationAcquisition::Primary { owner, inbox } => {
                // Marked here, not after the match: the other two outcomes
                // acquired nothing (mock) or failed (uncoordinated), and a
                // phase the trace claims but nobody performed is worse than a
                // missing one.
                StartupTrace::mark("linux_instance_acquired");
                (Some(owner), NativeActivation { inbox: Some(inbox) })
            }
            tm_platform::linux::activation::ActivationAcquisition::Uncoordinated { detail } => {
                tracing::warn!(%detail, "starting without Linux single-instance coordination");
                (None, NativeActivation::default())
            }
        }
    };
    #[cfg(not(target_os = "linux"))]
    let native_activation = NativeActivation::default();

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
        // A failed token query is unknown, not a reason to request a redundant
        // UAC handoff.
        && tm_platform::win::elevation_status() == Some(false)
    {
        let mut forwarded_args = args.to_vec();
        if tm_platform::win::instance::is_unresponsive_marked() {
            forwarded_args.push(tm_platform::win::instance::UNRESPONSIVE_ARG.to_string());
        }
        match tm_platform::win::relaunch_elevated_with_args(&forwarded_args) {
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
    // Coerce only the value actually used on this platform. Rewriting the
    // stored setting would silently destroy an explicit choice (a v0.1.31
    // `compatibility`/OpenGL preference on Windows) and make it unrecoverable
    // if the user moves the config to a platform that still offers it.
    let configured = configured_render_mode(settings.render_mode);
    if configured != settings.render_mode {
        tracing::warn!(
            stored = ?settings.render_mode,
            using = ?configured,
            "stored renderer is unavailable on this platform; keeping the stored choice"
        );
    }
    let render_mode = effective_render_mode(&settings);
    let window_size = [settings.window_size[0], settings.window_size[1]];
    // The egui context still has zoom 1.0 while the window is created
    // (`TaskManApp::new` applies `ui_zoom` afterwards), but the first frame
    // reads the window size back in zoomed points. Creating the window at
    // `size * zoom` makes that read-back equal the saved size, so a non-1.0
    // zoom no longer shrinks or grows the window on every restart.
    let creation_zoom = if settings.ui_zoom.is_finite() && settings.ui_zoom > 0.0 {
        settings.ui_zoom
    } else {
        1.0
    };
    let creation_size = [
        window_size[0] * creation_zoom,
        window_size[1] * creation_zoom,
    ];
    let creation_min_size = [720.0 * creation_zoom, 480.0 * creation_zoom];
    let restore_position = has_saved_settings && settings.remember_window;
    let window_position = restore_position.then(ui_state::window_position).flatten();
    let window_position_px = restore_position
        .then(ui_state::window_position_px)
        .flatten();
    let restore_maximized = restore_position && ui_state::window_maximized();
    // The last file read before the window exists; everything after this point
    // is renderer/adapter work and the app's own construction.
    StartupTrace::mark("window_state_loaded");

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
            .with_inner_size(creation_size)
            .with_min_inner_size(creation_min_size)
            // EVERY launch starts hidden, on every path: a window created
            // visible is composed by the desktop before the app has ever run a
            // frame, which is the white flash. Only eframe's reveal after the
            // first painted frame may show it, and the present gate keeps it
            // cloaked until that frame is really behind it.
            .with_visible(false)
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
            setup.instance_descriptor.backends = compiled_wgpu_backends();
            // This is a 2D monitor, so prefer the low-power adapter by
            // default rather than waking a discrete GPU. WGPU_POWER_PREF
            // remains an opt-in override for diagnostics.
            let power_preference = eframe::wgpu::PowerPreference::from_env()
                .unwrap_or(eframe::wgpu::PowerPreference::LowPower);
            setup.power_preference = power_preference;
            setup.native_adapter_selector = Some(std::sync::Arc::new(move |adapters, surface| {
                adapters
                    .iter()
                    .filter_map(|adapter| {
                        let rank = hardware_adapter_rank(
                            adapter.get_info().device_type,
                            power_preference,
                        )?;
                        surface
                            .is_none_or(|surface| adapter.is_surface_supported(surface))
                            .then_some((rank, adapter))
                    })
                    .min_by_key(|(rank, _)| *rank)
                    .map(|(_, adapter)| adapter.clone())
                    .ok_or_else(|| {
                        format!(
                            "no surface-compatible hardware {} adapter",
                            native_gpu_mode().label()
                        )
                    })
            }));
            opts.wgpu_options = config;
            opts
        };
        opts
    };

    let explicit_renderer = std::env::var("TASKMAN_RENDERER")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    StartupTrace::mark("run_native_enter");

    // The Start-menu-proof window band is chosen when the window is created,
    // so it reflects the persisted setting at launch only. A band-16 window
    // is permanently topmost and cannot be demoted, so this stays conditional.
    let topmost_band = settings.always_on_top;
    let mut failures: Vec<(eframe::Renderer, String)> = Vec::new();
    // A post-start event-loop failure (ExitFailure) is not a startup failure:
    // the app may have run for hours before the display server or event loop
    // failed, and reporting it as "could not start" hides that.
    let mut post_start_failure = false;
    for renderer in preferred_renderers(render_mode, explicit_renderer.as_deref()) {
        // Every renderer attempt creates a new window; re-request the band in
        // case a previous attempt cleared it after its first frame.
        if topmost_band {
            tm_platform::request_topmost_band();
        }
        tracing::info!(?renderer, "trying renderer");
        let use_mock = mock;
        let initial_tab = initial_tab_arg.clone();
        let app_settings = settings.clone();
        let start_hidden = initially_hidden;
        let native_activation = native_activation.clone();
        let creator = move |cc: &eframe::CreationContext<'_>| {
            *tm_core::sync::lock(&ACTIVE_RENDER_MODE) = mode_for_renderer(renderer);
            StartupTrace::mark("creation_context_enter");
            // `creation_size` is the saved size times `ui_zoom`, because the
            // window is created at the unzoomed context's scale. eframe's
            // persistence is off, so `zoom_factor` is 1.0 here; if that ever
            // changes (persistence on, or a zoom restored into the context),
            // the size would be scaled twice — say so instead of silently
            // growing the window on every start.
            let context_zoom = cc.egui_ctx.zoom_factor();
            if (context_zoom - 1.0).abs() > f32::EPSILON {
                tracing::warn!(
                    context_zoom,
                    ui_zoom = creation_zoom,
                    "egui context already zoomed at window creation; the saved size may be scaled twice"
                );
            }
            // Now the window exists, so the DPI-awareness gate reports the truth. Doing
            // this before `apply_startup` means the very first frame already rasterizes
            // at the right coverage rather than rebuilding the atlas a frame later.
            // The window handle selects the monitor whose ClearType calibration is
            // used; without one (`None`) the primary monitor's is.
            #[cfg(target_os = "windows")]
            let creation_hwnd = {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                match cc.window_handle().map(|handle| handle.as_raw()) {
                    Ok(RawWindowHandle::Win32(win32)) => Some(win32.hwnd.get()),
                    _ => None,
                }
            };
            #[cfg(not(target_os = "windows"))]
            let creation_hwnd: Option<isize> = {
                let _ = window_position_px;
                None
            };
            #[cfg(target_os = "windows")]
            if let Some(hwnd) = creation_hwnd {
                // The logical position the window was created with is
                // scaled by the primary monitor's DPI whatever monitor it
                // names; the physical position saved at the last exit is
                // exact. Then pull the window onto a monitor that exists.
                if let Some(pos) = window_position_px {
                    tm_platform::set_window_outer_position_px(hwnd, pos);
                }
                tm_platform::ensure_window_on_screen(hwnd);
            }
            // The dual-source answer has to be the pre-pipeline capability, not
            // `renderer.has_subpixel_pipeline()`: pipelines are compiled on the
            // first paint, and this runs in the creator, before that. The
            // capability predicate fails closed (device feature AND the LCD
            // shader validating), so it can only claim a pipeline that
            // `Renderer::new` will actually be able to build. A backend that
            // still refuses it is logged once by `Painter::ensure_renderer`.
            #[cfg(feature = "wgpu")]
            let wgpu_dual_source = cc
                .wgpu_render_state
                .as_ref()
                .is_some_and(|state| state.supports_subpixel_text());
            #[cfg(not(feature = "wgpu"))]
            let wgpu_dual_source = false;
            let subpixel_capable = renderer_supports_subpixel(renderer, wgpu_dual_source);
            tracing::debug!(
                ?renderer,
                wgpu_dual_source,
                subpixel_capable,
                "resolved sub-pixel text capability"
            );
            // Publish the user's saved smoothing preference BEFORE `apply_startup` and
            // `fonts::install_async` so Frame 1 and the async font load both use the
            // real hinting/weight profile instead of racing against `TaskManApp::new`.
            theme::set_text_smoothing(app_settings.text_smoothing);
            theme::set_subpixel_capable(
                subpixel_capable,
                tm_platform::text_rendering::query(creation_hwnd),
            );
            StartupTrace::mark("subpixel_capability_resolved");
            theme::apply_startup(&cc.egui_ctx);
            // Cloak the window and colour the native caption BEFORE the desktop
            // can compose it, so the first surface the user can see is the
            // app's own instead of an empty white one with a light-mode
            // caption. The gate re-cloaks on every later show and un-minimize;
            // `NativeApp::finish_reveal` uncovers the window once a frame has
            // painted into it.
            #[cfg(target_os = "windows")]
            if let Some(hwnd) = creation_hwnd {
                let palette = theme::palette_ctx(&cc.egui_ctx);
                let background = [
                    palette.window_bg.r(),
                    palette.window_bg.g(),
                    palette.window_bg.b(),
                ];
                tm_platform::install_present_gate(
                    hwnd,
                    background,
                    std::sync::Arc::new({
                        let ctx = cc.egui_ctx.clone();
                        move || ctx.request_repaint()
                    }),
                );
                // What `TaskManApp::sync_title_bar` would apply in the first
                // `ui` — one frame too late to be there when the window is
                // first composed.
                tm_platform::apply_title_bar(
                    hwnd,
                    background,
                    [palette.text.r(), palette.text.g(), palette.text.b()],
                    background,
                    matches!(cc.egui_ctx.theme(), eframe::egui::Theme::Dark),
                );
            } else {
                tracing::warn!(
                    "no native window handle at creation; the present gate is unavailable"
                );
            }
            fonts::install_async(cc.egui_ctx.clone());
            StartupTrace::mark("visuals_installed");
            let application = app::TaskManApp::new(cc, use_mock, app_settings, initial_tab);
            let shell = NativeApp::new(application, &cc.egui_ctx, start_hidden, native_activation);
            StartupTrace::mark("native_shell_ready");
            Ok(Box::new(shell) as Box<dyn eframe::App>)
        };
        let opts = options(renderer);
        StartupTrace::mark("window_options_built");

        match eframe::run_native("Task-Manager", opts, Box::new(creator)) {
            Ok(()) => return,
            Err(e) => {
                // A renderer attempt that got as far as creating its window
                // installed a present gate on that HWND. Drop it with the
                // window: the next attempt installs its own, and a failed one
                // must leave no subclass or cloak behind.
                #[cfg(target_os = "windows")]
                tm_platform::remove_present_gate();
                let abort = should_abort_renderer_fallback(&e);
                let detail = e.to_string();
                if abort {
                    post_start_failure = event_loop_failed_after_start(&e);
                    if post_start_failure {
                        tracing::error!(
                            ?renderer,
                            error = %detail,
                            "event loop failed after startup; not trying fallback renderers"
                        );
                    } else {
                        tracing::error!(
                            ?renderer,
                            error = %detail,
                            "display/event loop initialization failed; not trying fallback renderers"
                        );
                    }
                    failures.push((renderer, detail));
                    break;
                }
                tracing::warn!(?renderer, error = %detail, "renderer failed; falling back");
                failures.push((renderer, detail));
            }
        }
    }
    let error_message = if post_start_failure {
        format_event_loop_failure(&failures)
    } else {
        format_renderer_startup_error(explicit_renderer.as_deref(), &failures)
    };
    tracing::error!("{error_message}");
    tm_core::logging::attach_file_logging(tm_core::logging::LogConfig {
        console: false,
        level: None,
    });
    tm_core::logging::shutdown();
    eprintln!("{error_message}");
    std::process::exit(1);
}

/// The configured backend comes first. Keep CPU as the first fallback for an
/// unavailable GPU. OpenGL is also available through the diagnostic override.
#[allow(clippy::vec_init_then_push)]
fn preferred_renderers(mode: RenderMode, explicit_pref: Option<&str>) -> Vec<eframe::Renderer> {
    let mut compiled = Vec::with_capacity(3);
    #[cfg(feature = "software")]
    compiled.push(("software", eframe::Renderer::Software));
    #[cfg(feature = "wgpu")]
    compiled.push(("wgpu", eframe::Renderer::Wgpu));
    #[cfg(feature = "glow")]
    compiled.push(("glow", eframe::Renderer::Glow));

    if let Some(pref) = explicit_pref.map(str::trim).filter(|p| !p.is_empty()) {
        let mode = match pref.to_ascii_lowercase().as_str() {
            "cpu" => Some(RenderMode::Cpu),
            "d3d12" | "dx12" => Some(RenderMode::D3d12),
            "vulkan" => Some(RenderMode::Vulkan),
            "metal" => Some(RenderMode::Metal),
            "opengl" => Some(RenderMode::OpenGl),
            _ => None,
        };
        return compiled
            .into_iter()
            .filter_map(|(name, renderer)| {
                (pref.eq_ignore_ascii_case(name) || mode == Some(mode_for_renderer(renderer)))
                    .then_some(renderer)
            })
            .collect();
    }

    compiled.sort_by_key(|&(_, renderer)| mode_for_renderer(renderer) != mode);
    compiled.into_iter().map(|(_, renderer)| renderer).collect()
}

fn mode_for_renderer(renderer: eframe::Renderer) -> RenderMode {
    match renderer {
        #[cfg(feature = "software")]
        eframe::Renderer::Software => RenderMode::Cpu,
        #[cfg(feature = "wgpu")]
        eframe::Renderer::Wgpu => native_gpu_mode(),
        #[cfg(feature = "glow")]
        eframe::Renderer::Glow => RenderMode::OpenGl,
    }
}

/// Sub-pixel atlas coverage is only safe when the active backend can blend each channel
/// independently. The software painter can; WGPU can only after its device actually
/// enabled dual-source blending; Glow and unsupported WGPU fail closed to grayscale.
fn renderer_supports_subpixel(_renderer: eframe::Renderer, _wgpu_dual_source: bool) -> bool {
    #[cfg(feature = "software")]
    if matches!(_renderer, eframe::Renderer::Software) {
        return true;
    }
    #[cfg(feature = "wgpu")]
    if matches!(_renderer, eframe::Renderer::Wgpu) {
        return _wgpu_dual_source;
    }
    false
}

/// Event loop initialization/execution errors belong to the display server
/// connection rather than the chosen renderer, so trying fallback renderers
/// cannot succeed and previously masked the primary OS error behind winit's
/// `EventLoop can't be recreated`.
fn should_abort_renderer_fallback(err: &eframe::Error) -> bool {
    matches!(err, eframe::Error::WinitEventLoop(_))
}

/// `ExitFailure` means the event loop ran and then exited with a failure
/// status; the GUI was up, so "failed to start" would be wrong.
fn event_loop_failed_after_start(err: &eframe::Error) -> bool {
    matches!(
        err,
        eframe::Error::WinitEventLoop(egui_winit::winit::error::EventLoopError::ExitFailure(_))
    )
}

fn format_event_loop_failure(failures: &[(eframe::Renderer, String)]) -> String {
    let details = failures
        .iter()
        .map(|(renderer, err)| format!("{renderer}: {err}"))
        .collect::<Vec<_>>()
        .join("; ");
    format!("taskman: event loop failed after startup ({details})")
}

fn format_renderer_startup_error(
    explicit_pref: Option<&str>,
    failures: &[(eframe::Renderer, String)],
) -> String {
    let base = match failures {
        [] => {
            let pref = explicit_pref.map(str::trim).unwrap_or("");
            format!("taskman: no compiled renderer matches TASKMAN_RENDERER={pref:?}")
        }
        [(renderer, err)] => {
            format!("taskman: failed to start GUI ({renderer}: {err})")
        }
        many => {
            let details = many
                .iter()
                .map(|(renderer, err)| format!("{renderer}: {err}"))
                .collect::<Vec<_>>()
                .join("; ");
            format!("taskman: failed to start GUI (all renderers failed — {details})")
        }
    };
    if failures
        .iter()
        .any(|(_, err)| err.contains("Dynamic loading not supported"))
    {
        format!(
            "{base}\nnote: static musl builds lack dlopen() and cannot load X11/Wayland GUI libraries; use the glibc linux-x86_64 build for the desktop GUI (or run with --selfcheck for headless verification)"
        )
    } else {
        base
    }
}

/// GPU backends accept only an identified GPU. WARP and lavapipe report
/// Cpu, which leaves the native software renderer as the fallback.
#[cfg(feature = "wgpu")]
fn hardware_adapter_rank(
    device_type: eframe::wgpu::DeviceType,
    power: eframe::wgpu::PowerPreference,
) -> Option<u8> {
    use eframe::wgpu::{DeviceType, PowerPreference};
    match device_type {
        DeviceType::IntegratedGpu => Some(u8::from(power != PowerPreference::LowPower)),
        DeviceType::DiscreteGpu => Some(u8::from(power == PowerPreference::LowPower)),
        DeviceType::VirtualGpu => Some(2),
        DeviceType::Cpu | DeviceType::Other => None,
    }
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

/// Upper bound on frames a reveal waits for a painted frame before it
/// uncloaks anyway, so the window can never stay invisible.
#[cfg(target_os = "windows")]
const RESTORE_MAX_TICKS: u8 = 8;
/// ...and on the wall time it waits regardless. The waiting launch or hotkey
/// worker gave up long before this; it only releases the state.
#[cfg(target_os = "windows")]
const RESTORE_ACK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Frames a reveal waits for its own painted frame.
///
/// One frame's paint has been presented by the time the next one starts, so
/// releasing in the pass right after the first painted frame is still one frame
/// boundary too early for the surface the window is about to show.
#[cfg(target_os = "windows")]
const REVEAL_MIN_TICKS: u8 = 2;

/// Decide whether a held reveal may be released.
///
/// `painted` is the app's proof that a frame really painted into the window
/// while the gate held it — the only frame that can have real content in it —
/// `ticks` counts the frames since the hold began, `needs_frame` says the
/// platform gate still wants a frame of its own (its background fill is all the
/// window holds) and `waited` is the wall time since the hold began. The two
/// bounds are what keeps a reveal whose frame never comes from leaving the
/// window invisible.
#[cfg(target_os = "windows")]
fn reveal_due(painted: bool, ticks: u8, needs_frame: bool, waited: std::time::Duration) -> bool {
    if ticks >= RESTORE_MAX_TICKS || waited >= RESTORE_ACK_DEADLINE {
        return true;
    }
    painted && ticks >= REVEAL_MIN_TICKS && !needs_frame
}

/// Does a restore have to take the present gate?
///
/// A window hidden to the tray has no painted surface at all, and eframe
/// suspends its painter once no viewport is visible, so a minimized window can
/// come back with no surface either. Both would be composed out of nothing. A
/// window that is visible and not minimized still has its last frame; cloaking
/// that one would only make it blink.
#[cfg(target_os = "windows")]
fn restore_needs_present_gate(was_hidden: bool, minimized: bool) -> bool {
    was_hidden || minimized
}

/// Has a restored window's first frame been handed to the backend?
///
/// `armed` is whether a restore still waits for its acknowledgement; the
/// deadline clears that wait when it gives up, so a late pass claims nothing.
/// `marked` says whether this restore already logged, `painted` is the app's
/// painted-frame counter and `armed_at` the value `restore_window` recorded.
/// `ui` bumps that counter before eframe tessellates and paints the frame it
/// just laid out, so a frame counted there has no paint behind it yet; the
/// pass that begins after it is the one whose frame was handed over, the same
/// one-frame boundary `REVEAL_MIN_TICKS` exists for. Only the `ui` pass is
/// observable from here: whether the present succeeded cannot be seen, and a
/// failed one leaves the claim standing.
///
/// The mark lives in `logic` rather than in `ui` because this check runs
/// before `finish_reveal`, which clears the wait it reads.
#[cfg(target_os = "windows")]
fn restore_paint_seen(armed: bool, marked: bool, painted: u32, armed_at: u32) -> bool {
    armed && !marked && painted > armed_at
}

/// Platform-neutral holder for a native instance-activation channel.
#[derive(Clone, Default)]
struct NativeActivation {
    #[cfg(target_os = "linux")]
    inbox: Option<tm_platform::linux::activation::ActivationInbox>,
}

#[cfg(target_os = "linux")]
const LINUX_ACTIVATION_CONFIRM_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(700);

/// What the running session can do with the window.
///
/// The tray promises that a hidden window comes back, so it may only hide the
/// window where hiding works. Wayland's winit backend ignores
/// `ViewportCommand::Visible(false)` (and the un-minimize that would undo it),
/// so on Wayland a "hidden" window would stay on screen while the app believed
/// it was in the tray.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxDisplay {
    /// X11/XCB: show and hide really take the window off screen.
    X11,
    /// Wayland: the window stays composed, whatever the app asks for.
    Wayland,
}

#[cfg(target_os = "linux")]
struct LinuxActivationPending {
    request: tm_platform::linux::activation::ActivationRequest,
    queued_ui_seq: u64,
    deadline: std::time::Instant,
}

impl NativeActivation {
    fn install_waker(&self, ctx: &eframe::egui::Context) {
        #[cfg(target_os = "linux")]
        if let Some(inbox) = &self.inbox {
            let ctx = ctx.clone();
            inbox.install_waker(move || ctx.request_repaint());
        }
        #[cfg(not(target_os = "linux"))]
        let _ = ctx;
    }

    /// Queue show/unminimize/focus commands for every activation on the UI
    /// thread. The caller decides whether the actual window backend can prove
    /// that the resulting focus transition happened.
    #[cfg(target_os = "linux")]
    fn apply_pending(
        &self,
        ctx: &eframe::egui::Context,
    ) -> Vec<tm_platform::linux::activation::ActivationRequest> {
        let mut pending = Vec::new();
        if let Some(inbox) = &self.inbox {
            while let Some(request) = inbox.try_recv() {
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::RequestUserAttention(
                    eframe::egui::UserAttentionType::Critical,
                ));
                pending.push(request);
            }
        }
        pending
    }
}

#[cfg(target_os = "linux")]
fn linux_activation_can_confirm_focus(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    matches!(
        frame.window_handle().map(|handle| handle.as_raw()),
        Ok(RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_))
    )
}

#[cfg(target_os = "linux")]
fn linux_activation_restore_confirmed(
    painted_after_request: bool,
    minimized: Option<bool>,
    focused: Option<bool>,
) -> bool {
    painted_after_request && minimized == Some(false) && focused == Some(true)
}

/// Thin native-shell wrapper. TaskManApp already owns all UI/application
/// state; this layer only records the viewport's desktop-space position and
/// delegates every application hook unchanged.
struct NativeApp {
    inner: app::TaskManApp,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    activation: NativeActivation,
    #[cfg(target_os = "linux")]
    linux_activation_acks: Vec<LinuxActivationPending>,
    #[cfg(target_os = "linux")]
    linux_activation_ui_seq: u64,
    /// Display backend of this session, resolved once from the window.
    #[cfg(target_os = "linux")]
    linux_display: Option<LinuxDisplay>,
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    tray: Option<TrayShell>,
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    tray_init_attempted: bool,
    hidden_to_tray: bool,
    initial_hide_pending: bool,
    /// Startup diagnostics: `logic` was entered and the first `ui` returned.
    /// The marks must fire once, so the flags exist only to keep them from
    /// repeating on every later frame.
    first_logic_logged: bool,
    first_ui_logged: bool,
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    exit_requested: bool,
    /// Native window handle, cached the first time a frame hands it over.
    #[cfg(target_os = "windows")]
    hwnd: Option<isize>,
    /// A show request was accepted but not yet acknowledged: the waiting
    /// launch or hotkey worker gets its answer only once a frame has really
    /// been painted into the restored window.
    #[cfg(target_os = "windows")]
    show_ack_pending: bool,
    /// `frames_painted` when the show request was accepted.
    #[cfg(target_os = "windows")]
    show_ack_at_frame: u32,
    /// When the show request was accepted; bounds the acknowledgement so a
    /// frame that never comes cannot leave the waiting launch unanswered.
    #[cfg(target_os = "windows")]
    show_ack_started: std::time::Instant,
    /// Passes whose `ui` really ran, i.e. that painted into the window. eframe
    /// runs no `ui` at all for a root viewport the app marked invisible or that
    /// is minimized (it calls `update_logic_only` instead), so a pass counted
    /// here painted something. The one exception is the very first pass, which
    /// eframe runs before it reveals the window
    /// (`EpiIntegration::is_first_frame`): its paint lands in a still hidden
    /// window, which is why the gate is not adopted until the counter has
    /// moved — see `adoptable` in `finish_reveal`. A wait records this counter
    /// when it starts and is over once the counter has moved past it: one
    /// frame's paint has been presented by the time the next frame begins, so
    /// "a later frame painted" is exactly "a presented frame exists".
    #[cfg(target_os = "windows")]
    frames_painted: u32,
    /// Set once the marked restore's first painted frame has been traced, so
    /// `restored_window_painted` is logged once per restore, not per frame.
    #[cfg(target_os = "windows")]
    restore_paint_marked: bool,
    /// The present gate holds the window cloaked after a show, an un-minimize,
    /// the initial reveal or a requested restore, until a frame has painted
    /// into the window it is about to show.
    #[cfg(target_os = "windows")]
    present_gate_held: bool,
    /// `frames_painted` when the current hold began.
    #[cfg(target_os = "windows")]
    present_gate_at_frame: u32,
    /// Logic passes seen since the current hold began.
    #[cfg(target_os = "windows")]
    present_gate_ticks: u8,
    #[cfg(target_os = "windows")]
    present_gate_started: Option<std::time::Instant>,
    /// Latest monitor size egui reported; a change means the display layout
    /// changed under the window.
    #[cfg(target_os = "windows")]
    last_monitor_size: Option<eframe::egui::Vec2>,
    /// Last window placement whose DirectWrite policy was applied.
    #[cfg(target_os = "windows")]
    last_text_rendering_key: Option<tm_platform::text_rendering::WindowPlacementKey>,
    /// A close request arrived (seen in the raw input, so it is also seen
    /// while the window is minimized and `ui` does not run).
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    close_request_seen: bool,
    /// Give up waiting for the tray icon at this time and show the window.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    initial_hide_deadline: std::time::Instant,
}

impl NativeApp {
    fn new(
        inner: app::TaskManApp,
        ctx: &eframe::egui::Context,
        initially_hidden: bool,
        activation: NativeActivation,
    ) -> Self {
        activation.install_waker(ctx);
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            // The single-instance show event (a second launch, a tray click)
            // needs a repaint target even when close-to-tray is disabled and no
            // tray icon has been created.
            *tm_core::sync::lock(TRAY_CONTEXT.get_or_init(Default::default)) = Some(ctx.clone());
        }
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        let tray_requested = initially_hidden
            || inner.shared.settings.close_to_tray
            || inner.shared.settings.taskbar_monitor_enabled;
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        let tray = if tray_requested {
            TrayShell::new(true)
        } else {
            None
        };
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let _ = ctx;
        Self {
            inner,
            activation,
            #[cfg(target_os = "linux")]
            linux_activation_acks: Vec::new(),
            #[cfg(target_os = "linux")]
            linux_activation_ui_seq: 0,
            #[cfg(target_os = "linux")]
            linux_display: None,
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            tray,
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            tray_init_attempted: tray_requested,
            hidden_to_tray: initially_hidden,
            initial_hide_pending: initially_hidden,
            first_logic_logged: false,
            first_ui_logged: false,
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            exit_requested: false,
            #[cfg(target_os = "windows")]
            hwnd: None,
            #[cfg(target_os = "windows")]
            show_ack_pending: false,
            #[cfg(target_os = "windows")]
            show_ack_at_frame: 0,
            #[cfg(target_os = "windows")]
            show_ack_started: std::time::Instant::now(),
            #[cfg(target_os = "windows")]
            frames_painted: 0,
            #[cfg(target_os = "windows")]
            restore_paint_marked: false,
            #[cfg(target_os = "windows")]
            present_gate_held: false,
            #[cfg(target_os = "windows")]
            present_gate_at_frame: 0,
            #[cfg(target_os = "windows")]
            present_gate_ticks: 0,
            #[cfg(target_os = "windows")]
            present_gate_started: None,
            #[cfg(target_os = "windows")]
            last_monitor_size: None,
            #[cfg(target_os = "windows")]
            last_text_rendering_key: None,
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            close_request_seen: false,
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            initial_hide_deadline: std::time::Instant::now() + INITIAL_HIDE_TRAY_WAIT,
        }
    }

    /// Decide once what this session's display server can do with the window.
    ///
    /// The window's own display handle is the only honest source; when it is
    /// not available (an eframe backend that does not hand one out), the
    /// environment is the fallback and a set `WAYLAND_DISPLAY` counts as
    /// Wayland: mistaking a Wayland session for X11 would promise a hide that
    /// cannot happen, while the other way round only withholds a hide that
    /// would have worked.
    #[cfg(target_os = "linux")]
    fn resolve_linux_display(&mut self, frame: &eframe::Frame) {
        if self.linux_display.is_some() {
            return;
        }
        use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        let from_handle = match frame.display_handle().map(|handle| handle.as_raw()) {
            Ok(RawDisplayHandle::Wayland(_)) => Some(LinuxDisplay::Wayland),
            Ok(RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_)) => Some(LinuxDisplay::X11),
            _ => None,
        };
        let backend = from_handle.unwrap_or_else(|| {
            let wayland =
                std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
            if wayland {
                LinuxDisplay::Wayland
            } else {
                LinuxDisplay::X11
            }
        });
        tracing::info!(
            backend = ?backend,
            from_display_handle = from_handle.is_some(),
            "resolved the Linux display backend"
        );
        self.linux_display = Some(backend);
    }

    #[cfg(target_os = "linux")]
    fn resolve_linux_activations(&mut self, ctx: &eframe::egui::Context) {
        if self.linux_activation_acks.is_empty() {
            return;
        }
        let (minimized, focused) =
            ctx.input(|input| (input.viewport().minimized, input.viewport().focused));
        let now = std::time::Instant::now();
        let ui_seq = self.linux_activation_ui_seq;
        let mut still_pending = Vec::with_capacity(self.linux_activation_acks.len());
        let mut next_wake = None;
        for pending in self.linux_activation_acks.drain(..) {
            let painted_after_request = ui_seq > pending.queued_ui_seq;
            if linux_activation_restore_confirmed(painted_after_request, minimized, focused) {
                pending.request.acknowledge();
                tracing::info!("confirmed Linux activation restore and focus");
            } else if now >= pending.deadline {
                tracing::info!(
                    minimized = ?minimized,
                    focused = ?focused,
                    painted_after_request,
                    "Linux activation could not be confirmed; new window will open"
                );
                pending.request.reject();
            } else {
                next_wake = Some(
                    next_wake.map_or(pending.deadline, |current: std::time::Instant| {
                        current.min(pending.deadline)
                    }),
                );
                still_pending.push(pending);
            }
        }
        self.linux_activation_acks = still_pending;
        if let Some(deadline) = next_wake {
            ctx.request_repaint_after(deadline.saturating_duration_since(now));
        }
    }

    fn shutdown(&mut self) {
        // Before the window goes away: the gate subclasses the window
        // procedure and owns a class brush, both of which must be gone before
        // the HWND dies so nothing is left behind for the shell to paint with.
        #[cfg(target_os = "windows")]
        tm_platform::remove_present_gate();
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        if let Some(tray) = &mut self.tray {
            tray.shutdown();
        }
        if self.inner.shared.settings.save_config && self.inner.shared.settings.remember_window {
            ui_state::save();
        }
        self.inner.shutdown();
    }

    #[cfg(target_os = "windows")]
    fn sync_text_rendering(&mut self, ctx: &eframe::egui::Context, force_requery: bool) {
        let Some(hwnd) = self.hwnd else {
            return;
        };
        let Some(key) = tm_platform::text_rendering::window_placement_key(hwnd) else {
            let changed =
                theme::set_subpixel_params(tm_platform::text_rendering::Params::default());
            self.last_text_rendering_key = None;
            if changed {
                theme::refresh_text_rendering(ctx);
            }
            return;
        };
        if !force_requery && self.last_text_rendering_key == Some(key) {
            return;
        }

        let params = tm_platform::text_rendering::query(Some(hwnd));
        let changed = theme::set_subpixel_params(params);
        self.last_text_rendering_key = Some(key);
        if changed {
            theme::refresh_text_rendering(ctx);
            tracing::debug!(
                window_rect = ?key.window_rect,
                monitor = key.monitor,
                monitor_rect = ?key.monitor_rect,
                effective_dpi = key.effective_dpi,
                force_requery,
                ?params,
                "updated text-rendering policy for window placement"
            );
        }
    }
}

impl eframe::App for NativeApp {
    fn logic(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        // First data pass after the creator returned. The renderer's own setup
        // is NOT in this gap: eframe creates the wgpu instance, adapter, device
        // and surface in `egui_wgpu::winit::Painter::new` and `set_window`,
        // both before it calls the creator, so that cost lands between
        // `window_options_built` and `creation_context_enter`. What runs here
        // is eframe's bookkeeping and the event loop's first turn into the app.
        if !self.first_logic_logged {
            self.first_logic_logged = true;
            StartupTrace::mark("first_logic_enter");
        }
        #[cfg(target_os = "linux")]
        self.resolve_linux_display(frame);
        #[cfg(target_os = "linux")]
        self.resolve_linux_activations(ctx);
        #[cfg(target_os = "linux")]
        let activation_requests = self.activation.apply_pending(ctx);
        #[cfg(not(target_os = "linux"))]
        let activation_requests: Vec<()> = Vec::new();
        if !activation_requests.is_empty() {
            self.hidden_to_tray = false;
            self.initial_hide_pending = false;
            #[cfg(target_os = "linux")]
            {
                let can_confirm = linux_activation_can_confirm_focus(frame);
                let ui_seq = self.linux_activation_ui_seq;
                for request in activation_requests {
                    if can_confirm {
                        self.linux_activation_acks.push(LinuxActivationPending {
                            request,
                            queued_ui_seq: ui_seq,
                            deadline: std::time::Instant::now() + LINUX_ACTIVATION_CONFIRM_DEADLINE,
                        });
                    } else {
                        tracing::info!(
                            "Linux activation cannot confirm restoration on Wayland; new window will open"
                        );
                        request.reject();
                    }
                }
                if !self.linux_activation_acks.is_empty() {
                    ctx.request_repaint_after(LINUX_ACTIVATION_CONFIRM_DEADLINE);
                }
            }
            #[cfg(not(target_os = "linux"))]
            let _ = activation_requests;
            ctx.request_repaint();
        }
        #[cfg(target_os = "windows")]
        if self.hwnd.is_none() {
            let published = tm_platform::win::instance::published_window();
            if published != 0 {
                self.hwnd = Some(published);
            } else {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                if let Ok(handle) = frame.window_handle()
                    && let RawWindowHandle::Win32(win32) = handle.as_raw()
                {
                    let hwnd = win32.hwnd.get();
                    self.hwnd = Some(hwnd);
                    tm_platform::win::instance::publish_window(hwnd);
                }
            }
        }
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = self.hwnd
            && tm_platform::sync_foreground_focus(hwnd)
        {
            ctx.input_mut(|i| i.focused = true);
        }
        #[cfg(target_os = "windows")]
        poll_show_event_fallback(ctx);
        // The first frame of a restore. It is marked HERE and not in `ui` for
        // the reason `frames_painted` documents: this pass begins after `ui`
        // ran for a frame, so that frame was handed to the backend. Whether
        // the present itself succeeded is not observable from here — the mark
        // means "handed over", not "on screen", and a failed present leaves it
        // set. `restore_window` arms the wait and only a new restore re-arms
        // it, so this fires once per restore. Before `finish_reveal`, which
        // clears the wait it reads.
        #[cfg(target_os = "windows")]
        if restore_paint_seen(
            self.show_ack_pending,
            self.restore_paint_marked,
            self.frames_painted,
            self.show_ack_at_frame,
        ) {
            self.restore_paint_marked = true;
            StartupTrace::mark("restored_window_painted");
        }
        // Before the tray actions, so a transition drained here cannot spend
        // one of the frames it is owed on the frame that requested it.
        #[cfg(target_os = "windows")]
        self.finish_reveal(ctx);
        // In `logic`, not `ui`: a window hidden to the tray runs no `ui` at
        // all, and the icon is exactly how such a window comes back.
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        self.maintain_tray(ctx);
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        self.handle_close_request(ctx);
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        self.rescue_window_hidden_without_tray(ctx);
        if self.initial_hide_pending {
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                let tray_state = self
                    .tray
                    .as_ref()
                    .map(|tray| (tray.is_ready(), tray.has_failed()));
                let action = initial_hide_action(
                    tray_state,
                    std::time::Instant::now() < self.initial_hide_deadline,
                );
                // A session that cannot hide the window must not ask to: the
                // window would stay on screen with the app believing it is in
                // the tray.
                #[cfg(target_os = "linux")]
                let action = self.linux_initial_hide_action(action);
                match action {
                    // The icon exists: it is now safe to disappear into it.
                    InitialHideAction::Hide => {
                        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(false));
                        self.initial_hide_pending = false;
                    }
                    // Still being created: stay hidden (the window was created
                    // invisible) and look again shortly, but not forever. The
                    // command is still sent so eframe's first-frame reveal
                    // cannot override the request while we wait.
                    InitialHideAction::Wait => {
                        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(false));
                        ctx.request_repaint_after(std::time::Duration::from_millis(25));
                    }
                    InitialHideAction::Show => {
                        tracing::warn!(
                            "tray icon unavailable for a start-minimized launch; showing the window"
                        );
                        self.hidden_to_tray = false;
                        // The window has never been shown, so it has nothing to
                        // show: take the gate before asking for it.
                        #[cfg(target_os = "windows")]
                        if let Some(hwnd) = self.hwnd {
                            self.begin_present_gate(hwnd, "start-minimized fallback");
                        }
                        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
                        self.initial_hide_pending = false;
                    }
                }
            }
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            {
                self.hidden_to_tray = false;
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
                self.initial_hide_pending = false;
            }
        }
        <app::TaskManApp as eframe::App>::logic(&mut self.inner, ctx, frame);
    }

    fn raw_input_hook(
        &mut self,
        ctx: &eframe::egui::Context,
        raw_input: &mut eframe::egui::RawInput,
    ) {
        // This hook runs before egui begins the pass, so a monitor-policy or
        // live ClearType-tuner change on focus return affects the same frame
        // instead of leaving one frame on the previous stripe order.
        #[cfg(target_os = "windows")]
        {
            let focus_gained = raw_input
                .events
                .iter()
                .any(|event| matches!(event, eframe::egui::Event::WindowFocused(true)));
            self.sync_text_rendering(ctx, focus_gained);
        }
        // `ui` does not run while the window is minimized or hidden, and
        // `logic` cannot read the viewport's close request from the context
        // then; the raw input reaches both paths.
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        if raw_input.viewport().close_requested() {
            self.close_request_seen = true;
        }
        <app::TaskManApp as eframe::App>::raw_input_hook(&mut self.inner, ctx, raw_input);
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
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = self.hwnd
            && tm_platform::sync_foreground_focus(hwnd)
        {
            ui.ctx().input_mut(|i| i.focused = true);
        }
        <app::TaskManApp as eframe::App>::ui(&mut self.inner, ui, frame);
        #[cfg(target_os = "linux")]
        {
            self.linux_activation_ui_seq = self.linux_activation_ui_seq.wrapping_add(1);
            if !self.linux_activation_acks.is_empty() {
                ui.ctx().request_repaint();
            }
        }
        #[cfg(target_os = "windows")]
        {
            // eframe runs `ui` only for a viewport that is on screen and not
            // minimized (apart from the first pass, which it runs before the
            // first reveal), so this pass really painted into the window.
            // Every wait below is over once this counter has moved past the
            // value it started at.
            self.frames_painted = self.frames_painted.saturating_add(1);
            // A changed monitor size means the display layout changed under
            // the window (resolution switch by a game, monitor unplugged or
            // re-docked): pull the window back inside the visible desktop.
            if let Some(monitor_size) = ui.ctx().input(|i| i.viewport().monitor_size) {
                if let Some(hwnd) = self.hwnd
                    && self
                        .last_monitor_size
                        .is_some_and(|prev| prev != monitor_size)
                {
                    tm_platform::ensure_window_on_screen(hwnd);
                }
                self.last_monitor_size = Some(monitor_size);
            }
        }
        if self.inner.shared.settings.remember_window {
            let (pos, maximized, minimized) = ui.ctx().input(|i| {
                (
                    i.viewport().outer_rect.map(|r| r.min),
                    i.viewport().maximized.unwrap_or(false),
                    i.viewport().minimized.unwrap_or(false),
                )
            });
            // Never persist iconic/minimized geometry. On Windows a minimized
            // window can expose shell/off-screen placement while simultaneously
            // reporting maximized=false; recording that as the normal restore
            // position can strand the next restored window off-screen.
            if !minimized {
                // A maximized window's outer rect is the monitor's, not the
                // restore geometry — keep the last normal position instead.
                if !maximized && let Some(pos) = pos {
                    ui_state::set_window_position([pos.x, pos.y]);
                }
                #[cfg(target_os = "windows")]
                if !maximized
                    && let Some(hwnd) = self.hwnd
                    && let Some(px) = tm_platform::window_outer_position_px(hwnd)
                {
                    ui_state::set_window_position_px(px);
                }
                ui_state::set_window_maximized(maximized);
            }
        }
        // The first UI pass is complete. eframe tessellates, paints and
        // only then reveals the window, so this mark ends the phase the
        // renderer pays for before anything is on screen.
        if !self.first_ui_logged {
            self.first_ui_logged = true;
            StartupTrace::mark("first_ui_left");
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

/// Actions a tray menu hands to the UI thread. The codes come from
/// [`tray::TrayAction`], so every desktop's menu maps to the same requests
/// however it numbers its own rows.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) const TRAY_ACTION_OPEN: u8 = tray::TrayAction::Open.signal_code();
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) const TRAY_ACTION_SETTINGS: u8 = tray::TrayAction::Settings.signal_code();
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) const TRAY_ACTION_EXIT: u8 = tray::TrayAction::Exit.signal_code();
#[cfg(any(target_os = "windows", target_os = "linux"))]
static TRAY_ACTION: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
#[cfg(any(target_os = "windows", target_os = "linux"))]
static TRAY_CONTEXT: std::sync::OnceLock<std::sync::Mutex<Option<eframe::egui::Context>>> =
    std::sync::OnceLock::new();
#[cfg(target_os = "windows")]
static TRAY_HANDLERS: std::sync::Once = std::sync::Once::new();

/// Queue one tray request for the UI thread and wake it.
///
/// Called from whichever thread owns the menu: the Windows tray thread, the
/// taskbar-monitor callback, or the StatusNotifierItem method on the session
/// bus. Only the highest request is kept, which is why `Exit` outranks the
/// others — it ends the app either way.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) fn signal_tray(action: u8) {
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

/// Lifecycle of the tray thread's icon: the UI only trusts the tray as a way
/// back to the window once the icon actually exists.
#[cfg(target_os = "windows")]
static TRAY_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(TRAY_STARTING);
#[cfg(target_os = "windows")]
const TRAY_STARTING: u8 = 0;
#[cfg(target_os = "windows")]
const TRAY_READY: u8 = 1;
#[cfg(target_os = "windows")]
const TRAY_FAILED: u8 = 2;

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
            TRAY_STATE.store(TRAY_FAILED, std::sync::atomic::Ordering::Release);
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
            TRAY_STATE.store(TRAY_FAILED, std::sync::atomic::Ordering::Release);
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
    // Only now is there an icon the user can click. The UI must not hide the
    // window into the tray before this: a window hidden behind a tray icon
    // that never appeared cannot be brought back.
    TRAY_STATE.store(TRAY_READY, std::sync::atomic::Ordering::Release);

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
    TRAY_STATE.store(TRAY_FAILED, std::sync::atomic::Ordering::Release);
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
        TRAY_STATE.store(TRAY_STARTING, std::sync::atomic::Ordering::Release);
        if std::thread::Builder::new()
            .name("tm-tray".into())
            .spawn(tray_thread_main)
            .is_err()
        {
            TRAY_STATE.store(TRAY_FAILED, std::sync::atomic::Ordering::Release);
            return None;
        }
        Some(Self {
            visible: Some(initially_visible),
            menu_dark: None,
            shutdown: false,
        })
    }

    /// The icon exists and can bring the window back.
    fn is_ready(&self) -> bool {
        !self.shutdown && TRAY_STATE.load(std::sync::atomic::Ordering::Acquire) == TRAY_READY
    }

    /// The icon could not be created (or its thread ended): nothing will
    /// ever appear in the notification area.
    fn has_failed(&self) -> bool {
        TRAY_STATE.load(std::sync::atomic::Ordering::Acquire) == TRAY_FAILED
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
    /// Bring the window back from the tray, the hotkey or another launch
    /// without the white flash.
    ///
    /// `ShowWindow` alone composes an unpainted window: a hidden window never
    /// painted at all, and eframe suspends its painter once no viewport is
    /// visible, so a minimized window can come back with no surface either.
    /// Either way the compositor fills the gap with the empty surface. The
    /// present gate cloaks the window first, so it is shown — and therefore
    /// painting — while still invisible to the compositor;
    /// [`Self::finish_reveal`] uncovers it once a real frame is behind it.
    fn restore_window(&mut self, ctx: &eframe::egui::Context) {
        let was_hidden = self.hidden_to_tray;
        self.hidden_to_tray = false;
        // A launch that arrives while this instance is still starting
        // minimized must win: the pending hide would otherwise put the
        // window straight back into the tray in the same frame.
        self.initial_hide_pending = false;
        // A window that is visible and not minimized still has its last frame,
        // so taking the gate there would only make a healthy window blink.
        let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        if let Some(hwnd) = self.hwnd
            && restore_needs_present_gate(was_hidden, minimized)
        {
            self.begin_present_gate(hwnd, "restore requested");
        }
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
        // Un-minimize before activating. Activation is a no-op on a minimized
        // window (winit checks that explicitly), which is why a hotkey press
        // used to leave a minimized task manager exactly where it was.
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(false));
        // NO `ViewportCommand::Focus` here, and it must not come back.
        // winit's `focus_window` ends in `force_window_active`, which SYNTHESIZES
        // a left-Alt press and release into the system input stream with
        // `SendInput` to shake off the foreground lock. That Alt is delivered to
        // whichever application still holds focus — and on this path that is by
        // definition another one, usually the fullscreen game the user just hit
        // Ctrl+Shift+Esc in. A bare Alt opens the menu bar in Explorer, in
        // browsers and in most Win32 apps, which then swallows the user's next
        // keystroke. `force_foreground` below does strictly more than winit's
        // `SetForegroundWindow` and injects nothing, so the command bought
        // nothing but that side effect. (The guard `if !window.has_focus()`
        // does not save it: `has_focus` is a cached flag updated by window
        // messages, and this thread has not pumped since `force_foreground`
        // ran, so it still reads false.)
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = self.hwnd {
            // Put a window that Windows or a display change left off-screen
            // back on a monitor before it is shown.
            tm_platform::ensure_window_on_screen(hwnd);
            // The non-attaching activation: this is the UI thread inside
            // `update()`, which does not pump its message queue until the
            // frame ends. The hotkey worker does the attaching one, and it is
            // the only caller that may.
            tm_platform::force_foreground(hwnd);
        }
        // The request is NOT acknowledged here. The waiting launch or hotkey
        // worker treats an acknowledgement as "the window is usable", and at
        // this point the show/restore commands above are only queued and the
        // window is still cloaked by the gate. `finish_reveal` acknowledges
        // once a frame has been painted into the window and it is uncovered.
        self.show_ack_pending = true;
        self.show_ack_at_frame = self.frames_painted;
        self.show_ack_started = std::time::Instant::now();
        self.restore_paint_marked = false;
        // The UI is event driven and the engine may be paused, so the frames
        // the countdown needs have to be asked for explicitly.
        ctx.request_repaint();
    }

    /// Arm the present gate: cloak the window now and owe it a painted frame
    /// before it may be uncovered.
    ///
    /// Called from every path that makes the window visible — a requested
    /// restore, the start-minimized fallback, and the platform's own report of
    /// a show or un-minimize this app never asked for. An armed gate is never
    /// re-armed, so a burst of show messages cannot keep pushing the reveal
    /// back.
    fn begin_present_gate(&mut self, hwnd: isize, reason: &'static str) {
        if self.present_gate_held {
            return;
        }
        tm_platform::present_gate_hold(hwnd);
        self.present_gate_held = true;
        self.present_gate_at_frame = self.frames_painted;
        self.present_gate_ticks = 0;
        self.present_gate_started = Some(std::time::Instant::now());
        tracing::debug!(
            hwnd,
            reason,
            frames_painted = self.frames_painted,
            "present gate: holding the window until a frame paints"
        );
    }

    /// Uncover the window and, for a requested show, make it the foreground
    /// window again.
    fn release_present_gate(&mut self, hwnd: isize, painted: bool, waited: std::time::Duration) {
        let ticks = self.present_gate_ticks;
        let released = tm_platform::present_gate_release(hwnd);
        self.present_gate_held = false;
        self.present_gate_started = None;
        // Everything below repairs the window itself, which needs its handle;
        // the release above already uncloaked it through the gate's own copy.
        if hwnd == 0 {
            tracing::debug!(
                ticks,
                released,
                painted,
                waited_ms = waited.as_millis() as u64,
                "present gate: released through the gate's own handle"
            );
            return;
        }
        // A window another party (virtual desktops, the shell) left cloaked
        // would be "shown" yet invisible: repair that before anything is
        // answered. A cloak that outlived the gate would leave the window
        // unreachable, so it is cleared whichever side set it.
        if tm_platform::present_gate_held() || tm_platform::is_window_cloaked(hwnd) {
            tm_platform::set_window_cloaked(hwnd, false);
        }
        // Re-fit: the restore ran before the window had its final geometry, so
        // a monitor that went away can leave it at its old off-screen place.
        tm_platform::ensure_window_on_screen(hwnd);
        // Only a show someone asked for has to fight for the foreground; the
        // initial reveal and a taskbar click already have it. It also has to
        // be a window that is still meant to be on screen: `force_foreground`
        // ends in `ShowWindow(SW_SHOW)` and `SWP_SHOWWINDOW`, so doing it for
        // a window the app has since put back in the tray would show it again
        // with no frame behind it — `ui` does not run for a viewport the app
        // marked invisible, so nothing would repaint it either.
        if self.show_ack_pending && !self.hidden_to_tray && !self.initial_hide_pending {
            tm_platform::force_foreground(hwnd);
        }
        if painted {
            tracing::debug!(
                hwnd,
                ticks,
                released,
                waited_ms = waited.as_millis() as u64,
                "present gate: uncovered the window behind a painted frame"
            );
        } else {
            tracing::warn!(
                hwnd,
                ticks,
                released,
                waited_ms = waited.as_millis() as u64,
                "present gate: uncovered the window without a painted frame"
            );
        }
    }

    /// Drive the present gate every frame: hold every show Windows performs and
    /// uncover the window only once a frame has painted into it.
    ///
    /// The gate is driven from the START of each frame rather than from the
    /// calls that ask for a window, because a minimized window is restored by
    /// Windows itself (taskbar click, Alt-Tab, a shell restore) and this app
    /// only learns about it from the platform gate. A frame's paint has been
    /// presented by the time the next frame begins, so a reveal here always
    /// uncovers real content. The bounds ([`RESTORE_MAX_TICKS`],
    /// [`RESTORE_ACK_DEADLINE`]) mean a frame that never comes can neither
    /// leave the window invisible nor leave a waiting launch unanswered.
    fn finish_reveal(&mut self, ctx: &eframe::egui::Context) {
        // A missing cached handle must not stop the reveal loop: the gate
        // holds the window cloaked and knows the handle it was installed on, so
        // zero means "ask the gate". Returning here instead would let a hold
        // outlive every bound and leave an invisible window, which is the one
        // failure worse than the flash. The window repairs below are the only
        // things that need the handle here, and they are skipped without it.
        let hwnd = self.hwnd.unwrap_or(0);
        // The platform re-cloaks on every show and un-minimize, including the
        // ones nobody here asked for, and it is still holding the cloak it
        // installed at creation. Either way the window is invisible until a
        // frame proves it has content, so both arm the hold. A window that is
        // not meant to be on screen has nothing to reveal yet; the show that
        // brings it back arms the gate itself.
        let transitioned = tm_platform::present_gate_take_transition();
        let on_screen = !self.hidden_to_tray && !self.initial_hide_pending;
        // Adopting waits for one real pass: the paint of the very first frame
        // happened before eframe could reveal the window, so it proves nothing
        // about the surface the window is about to show.
        let adoptable = on_screen && self.frames_painted > 0;
        if (transitioned || (adoptable && tm_platform::present_gate_held()))
            && !self.present_gate_held
        {
            self.begin_present_gate(
                hwnd,
                if transitioned {
                    "window shown"
                } else {
                    "gate still holding"
                },
            );
        }
        if self.present_gate_held {
            self.present_gate_ticks = self.present_gate_ticks.saturating_add(1);
            let painted = self.frames_painted > self.present_gate_at_frame;
            let waited = self
                .present_gate_started
                .map_or(std::time::Duration::ZERO, |started| started.elapsed());
            if reveal_due(
                painted,
                self.present_gate_ticks,
                tm_platform::present_gate_needs_frame(),
                waited,
            ) {
                self.release_present_gate(hwnd, painted, waited);
            }
        }
        // A show is answered only once the window is really up: the frame that
        // proves it has painted has also been presented by now.
        let answered = self.frames_painted > self.show_ack_at_frame
            || self.show_ack_started.elapsed() >= RESTORE_ACK_DEADLINE;
        if self.show_ack_pending && !self.present_gate_held && answered {
            tm_platform::win::instance::acknowledge_show();
            self.show_ack_pending = false;
        }
        if self.show_ack_pending || self.present_gate_held {
            // The countdown must not stall on an idle, event-driven UI.
            ctx.request_repaint();
        }
    }
}

/// The tray lifecycle both desktops share: create the icon when the settings or
/// a start-minimized launch ask for it, keep it in step, and run the paths that
/// depend on it — restoring the window from a menu entry, closing into the tray,
/// and rescuing a window whose icon failed.
///
/// Everything here is called from `logic`, because a window hidden to the tray
/// runs no `ui` at all and the icon is exactly how such a window comes back.
#[cfg(any(target_os = "windows", target_os = "linux"))]
impl NativeApp {
    /// Handle what the icon asked for and keep it in step with the settings.
    fn maintain_tray(&mut self, ctx: &eframe::egui::Context) {
        self.handle_tray_actions(ctx);
        // Close-to-tray and the taskbar monitor both need an icon to come back
        // from; a window that is already in the tray keeps it too.
        let keep_in_tray = self.inner.shared.settings.close_to_tray
            || self.inner.shared.settings.taskbar_monitor_enabled;
        let tray_enabled = keep_in_tray || self.hidden_to_tray;
        if tray_enabled && self.tray.is_none() && !self.tray_init_attempted {
            self.tray_init_attempted = true;
            self.tray = TrayShell::new(tray_enabled);
        }
        if let Some(tray) = &mut self.tray {
            // A passive icon is one a panel that hides passive items may drop,
            // so the window is never hidden behind an icon nobody can see.
            tray.set_visible(tray_enabled);
            tray.sync_menu_theme(ctx.theme());
        }
    }

    /// Bring the window back from the tray icon.
    ///
    /// Windows has to cloak and repaint the window first, otherwise the shell
    /// composes an empty surface (`restore_window`); Linux uses the same three
    /// viewport commands as its activation path.
    #[cfg(target_os = "windows")]
    fn show_window_from_tray(&mut self, ctx: &eframe::egui::Context) {
        self.restore_window(ctx);
    }

    #[cfg(target_os = "linux")]
    fn show_window_from_tray(&mut self, ctx: &eframe::egui::Context) {
        self.hidden_to_tray = false;
        // A launch that arrives while this instance is still starting minimized
        // must win: the pending hide would otherwise put the window straight
        // back into the tray in the same frame.
        self.initial_hide_pending = false;
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    /// Act on a close request: close to the tray when that is configured AND
    /// the icon really exists, otherwise let the window close.
    fn handle_close_request(&mut self, ctx: &eframe::egui::Context) {
        if !std::mem::take(&mut self.close_request_seen) {
            return;
        }
        if PROGRAMMATIC_EXIT.load(std::sync::atomic::Ordering::Acquire) {
            self.exit_requested = true;
        }
        let keep_in_tray = self.inner.shared.settings.close_to_tray
            || self.inner.shared.settings.taskbar_monitor_enabled;
        if self.exit_requested || !keep_in_tray {
            return;
        }
        if !self.can_hide_window() {
            // Cancelling the close here would leave a window that is still on
            // screen while the app believes it is in the tray: the X button
            // would look broken and the icon would offer to "restore" a window
            // that never left. Closing is what the user asked for.
            tracing::warn!(
                "close-to-tray needs a window that can be hidden, which this display server does not support; closing"
            );
            return;
        }
        if self.tray.as_ref().is_some_and(TrayShell::is_ready) {
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(false));
            self.hidden_to_tray = true;
        } else {
            // Hiding behind an icon that does not exist would strand the
            // window; closing is the honest outcome.
            tracing::warn!("close requested but the tray icon is not available; closing");
        }
    }

    /// Whether this desktop can really take the window off screen.
    ///
    /// Everything that hides the window behind the icon goes through this: a
    /// hide that the display server ignores would strand the window on screen
    /// with no way for the user to tell that the app thinks it is in the tray.
    #[cfg(target_os = "windows")]
    fn can_hide_window(&self) -> bool {
        true
    }

    /// Wayland ignores the hide request (and the un-minimize that would undo
    /// it), so only a positively identified X11 session may hide the window; an
    /// unresolved backend keeps the window where the user can see it.
    #[cfg(target_os = "linux")]
    fn can_hide_window(&self) -> bool {
        self.linux_display == Some(LinuxDisplay::X11)
    }

    /// A start that asked to minimize keeps the window visible on a session
    /// that cannot hide it, instead of hiding a window nobody can restore.
    #[cfg(target_os = "linux")]
    fn linux_initial_hide_action(&self, action: InitialHideAction) -> InitialHideAction {
        if self.can_hide_window() || action == InitialHideAction::Show {
            return action;
        }
        tracing::warn!(
            "this display server ignores hiding the window; showing it instead of minimising to the tray"
        );
        InitialHideAction::Show
    }

    /// A window hidden "to the tray" whose icon failed (or whose thread
    /// ended) cannot be restored by the user: bring it back.
    fn rescue_window_hidden_without_tray(&mut self, ctx: &eframe::egui::Context) {
        if self.hidden_to_tray
            && !self.initial_hide_pending
            && self.tray.as_ref().is_none_or(TrayShell::has_failed)
        {
            tracing::warn!("window is hidden but the tray icon is unavailable; restoring it");
            self.show_window_from_tray(ctx);
        }
    }

    fn handle_tray_actions(&mut self, ctx: &eframe::egui::Context) {
        match TRAY_ACTION.swap(0, std::sync::atomic::Ordering::AcqRel) {
            TRAY_ACTION_OPEN => {
                self.show_window_from_tray(ctx);
            }
            TRAY_ACTION_SETTINGS => {
                self.show_window_from_tray(ctx);
                self.inner.show_settings = true;
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

/// Parse `--size=<width>x<height>`.
///
/// The result becomes the window's client rect before the first frame, so it is
/// bounded the way the persisted size is: a size below the app's minimum is not
/// a size at all (the flag is ignored and the saved size stands), a finite size
/// above the storable maximum is clamped into it, and a non-finite one — `inf`,
/// which used to reach the window as an infinite rect — is rejected.
fn parse_size_arg(s: &str) -> Option<[f32; 2]> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?);
    if !w.is_finite() || !h.is_finite() || w < 200.0 || h < 150.0 {
        return None;
    }
    Some([w.clamp(200.0, 16384.0), h.clamp(150.0, 16384.0)])
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `--size=` reaches the window rect before anything else runs, so it is a
    /// hostile-input surface: the parse only checked the minimum, which `inf`
    /// passes, so `--size=infxinf` created an infinite window.
    #[test]
    fn a_command_line_size_is_bounded_and_must_be_finite() {
        assert_eq!(parse_size_arg("1280x800"), Some([1280.0, 800.0]));
        assert_eq!(parse_size_arg(" 900 x 700 "), Some([900.0, 700.0]));
        assert_eq!(parse_size_arg("1024X768"), Some([1024.0, 768.0]));
        // Below the minimum: not a size this app creates, so the saved one stands.
        assert_eq!(parse_size_arg("199x700"), None);
        assert_eq!(parse_size_arg("900x149"), None);
        // Non-finite: rejected rather than clamped into the window.
        assert_eq!(parse_size_arg("infxinf"), None);
        assert_eq!(parse_size_arg("nanxnan"), None);
        assert_eq!(parse_size_arg("-infx600"), None);
        // Finite but absurd: clamped to what the settings file can also hold.
        assert_eq!(parse_size_arg("99999x99999"), Some([16384.0, 16384.0]));
        assert_eq!(parse_size_arg("900"), None);
        assert_eq!(parse_size_arg("x800"), None);
    }

    #[test]
    fn renderer_choices_start_with_their_backend() {
        for &mode in renderer_choices() {
            let list = preferred_renderers(mode, None);
            assert_eq!(mode_for_renderer(list[0]), mode);
            assert_eq!(list, preferred_renderers(mode, Some("  ")));
        }
    }

    #[test]
    fn renderer_capability_fails_closed_for_subpixel_text() {
        #[cfg(feature = "software")]
        assert!(renderer_supports_subpixel(
            eframe::Renderer::Software,
            false
        ));
        #[cfg(feature = "wgpu")]
        {
            assert!(renderer_supports_subpixel(eframe::Renderer::Wgpu, true));
            assert!(!renderer_supports_subpixel(eframe::Renderer::Wgpu, false));
        }
        #[cfg(feature = "glow")]
        assert!(!renderer_supports_subpixel(eframe::Renderer::Glow, true));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn same_sized_moves_and_dpi_changes_produce_different_placement_keys() {
        use tm_platform::text_rendering::WindowPlacementKey;

        let original = WindowPlacementKey {
            window_rect: [0, 0, 800, 600],
            monitor: 1,
            monitor_rect: [0, 0, 1920, 1080],
            effective_dpi: 96,
        };
        assert_ne!(
            original,
            WindowPlacementKey {
                window_rect: [1920, 0, 2720, 600],
                ..original
            },
            "a same-sized move to another monitor must be detected"
        );
        assert_ne!(
            original,
            WindowPlacementKey {
                effective_dpi: 144,
                monitor_rect: [0, 0, 2880, 1620],
                ..original
            },
            "a DPI/topology change must be detected"
        );
    }

    #[cfg(all(feature = "software", feature = "wgpu", feature = "glow"))]
    #[test]
    fn desktop_backends_prefer_cpu_and_fall_back_from_gpu_to_cpu() {
        use eframe::Renderer::{Glow, Software, Wgpu};
        #[cfg(target_os = "windows")]
        assert_eq!(renderer_choices(), [RenderMode::Cpu, RenderMode::D3d12]);
        #[cfg(target_os = "linux")]
        assert_eq!(renderer_choices(), [RenderMode::Cpu, RenderMode::Vulkan]);
        assert_eq!(
            preferred_renderers(RenderMode::Cpu, None),
            [Software, Wgpu, Glow]
        );
        assert_eq!(
            preferred_renderers(native_gpu_mode(), None),
            [Wgpu, Software, Glow]
        );
        assert_eq!(
            preferred_renderers(RenderMode::Cpu, Some(" CPU ")),
            [Software]
        );
        assert_eq!(preferred_renderers(RenderMode::Cpu, Some(" GLOW ")), [Glow]);
        assert_eq!(preferred_renderers(RenderMode::Cpu, Some("wgpu")), [Wgpu]);
        assert!(preferred_renderers(RenderMode::Cpu, Some("unknown")).is_empty());
        let foreign = if cfg!(target_os = "linux") {
            RenderMode::D3d12
        } else {
            RenderMode::Vulkan
        };
        assert_eq!(configured_render_mode(foreign), RenderMode::Cpu);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn a_reveal_waits_for_a_painted_frame_inside_its_bounds() {
        let quick = std::time::Duration::from_millis(16);

        // Nothing has painted into the window yet, and the pass that armed the
        // gate cannot have presented anything either.
        assert!(!reveal_due(false, 0, false, quick));
        assert!(!reveal_due(true, 0, false, quick));
        assert!(!reveal_due(true, 1, false, quick));
        // A presented frame two passes in is the whole point: uncover it.
        assert!(reveal_due(true, REVEAL_MIN_TICKS, false, quick));
        // The platform gate still owning the surface keeps the window cloaked.
        assert!(!reveal_due(true, REVEAL_MIN_TICKS, true, quick));
        // A frame that never comes must not leave the window invisible.
        assert!(!reveal_due(false, RESTORE_MAX_TICKS - 1, true, quick));
        assert!(reveal_due(false, RESTORE_MAX_TICKS, true, quick));
        assert!(reveal_due(false, 1, true, RESTORE_ACK_DEADLINE));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn only_a_restore_that_may_have_no_surface_takes_the_present_gate() {
        // Hidden to the tray: it never painted at all.
        assert!(restore_needs_present_gate(true, false));
        // Minimized: eframe suspends the painter, so the surface may be gone.
        assert!(restore_needs_present_gate(false, true));
        assert!(restore_needs_present_gate(true, true));
        // Already showing its last frame: a cloak here would only blink.
        assert!(!restore_needs_present_gate(false, false));
    }

    /// `restored_window_painted` claims a frame was handed to the backend, so
    /// it may not be logged on the pass that armed the restore: `ui` bumps the
    /// painted-frame counter before that frame is painted, and only the pass
    /// after it has handed a frame over.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_restore_paint_is_marked_only_after_a_frame_was_handed_over() {
        // Nothing armed: a taskbar un-minimize the app never asked for.
        assert!(!restore_paint_seen(false, false, 3, 2));
        // Armed this pass: `ui` has not bumped the counter since.
        assert!(!restore_paint_seen(true, false, 2, 2));
        // The pass after that frame ran: it has been handed over by now.
        assert!(restore_paint_seen(true, false, 3, 2));
        // Once per restore, however many frames follow.
        assert!(!restore_paint_seen(true, true, 4, 2));
        // A deadline that gave up clears the wait, so it claims nothing later.
        assert!(!restore_paint_seen(false, false, 9, 2));
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn gpu_adapter_selection_excludes_software_and_respects_power_preference() {
        use eframe::wgpu::{DeviceType, PowerPreference};
        for power in [
            PowerPreference::None,
            PowerPreference::LowPower,
            PowerPreference::HighPerformance,
        ] {
            assert_eq!(hardware_adapter_rank(DeviceType::Cpu, power), None);
            assert_eq!(hardware_adapter_rank(DeviceType::Other, power), None);
            assert!(hardware_adapter_rank(DeviceType::VirtualGpu, power).is_some());
        }
        for (power, first, second) in [
            (
                PowerPreference::LowPower,
                DeviceType::IntegratedGpu,
                DeviceType::DiscreteGpu,
            ),
            (
                PowerPreference::HighPerformance,
                DeviceType::DiscreteGpu,
                DeviceType::IntegratedGpu,
            ),
        ] {
            assert_eq!(hardware_adapter_rank(first, power), Some(0));
            assert_eq!(hardware_adapter_rank(second, power), Some(1));
        }
    }

    #[test]
    fn event_loop_error_aborts_renderer_fallback_and_preserves_primary_error() {
        let loop_err = eframe::Error::WinitEventLoop(
            egui_winit::winit::error::EventLoopError::RecreationAttempt,
        );
        assert!(should_abort_renderer_fallback(&loop_err));

        let renderer_err = eframe::Error::AppCreation("softbuffer surface failed".into());
        assert!(!should_abort_renderer_fallback(&renderer_err));
    }

    #[test]
    fn an_exit_failure_is_reported_as_a_runtime_failure_not_a_startup_failure() {
        let exit =
            eframe::Error::WinitEventLoop(egui_winit::winit::error::EventLoopError::ExitFailure(1));
        assert!(should_abort_renderer_fallback(&exit));
        assert!(event_loop_failed_after_start(&exit));
        let recreate = eframe::Error::WinitEventLoop(
            egui_winit::winit::error::EventLoopError::RecreationAttempt,
        );
        assert!(!event_loop_failed_after_start(&recreate));
        assert_eq!(
            format_event_loop_failure(&[(eframe::Renderer::Software, "exit 1".into())]),
            "taskman: event loop failed after startup (software: exit 1)"
        );
    }

    #[cfg(all(feature = "software", feature = "wgpu", feature = "glow"))]
    #[test]
    fn format_renderer_startup_error_is_accurate_and_human_readable() {
        let missing = format_renderer_startup_error(Some("unknown"), &[]);
        assert!(
            missing.contains("TASKMAN_RENDERER=\"unknown\""),
            "{missing}"
        );
        assert!(!missing.contains("GPU renderer"), "{missing}");
        assert!(!missing.contains("Some("), "{missing}");

        let single = format_renderer_startup_error(
            None,
            &[(
                eframe::Renderer::Software,
                "winit EventLoopError: os error: Failed to open connection to X server".into(),
            )],
        );
        assert!(single.contains("software:"), "{single}");
        assert!(
            single.contains("Failed to open connection to X server"),
            "{single}"
        );
        assert!(!single.contains("GPU renderer"), "{single}");
        assert!(!single.contains("Some("), "{single}");

        let multi = format_renderer_startup_error(
            None,
            &[
                (eframe::Renderer::Software, "softbuffer failed".into()),
                (eframe::Renderer::Wgpu, "no adapter".into()),
                (eframe::Renderer::Glow, "no GL config".into()),
            ],
        );
        assert!(
            multi.contains("software: softbuffer failed; wgpu: no adapter; glow: no GL config"),
            "{multi}"
        );
        assert!(!multi.contains("GPU renderer"), "{multi}");
        assert!(!multi.contains("Some("), "{multi}");

        let musl = format_renderer_startup_error(
            None,
            &[(
                eframe::Renderer::Software,
                "winit EventLoopError: os error: LibraryOpenError(Dlopen { desc: \"Dynamic loading not supported\" })".into(),
            )],
        );
        assert!(musl.contains("Dynamic loading not supported"), "{musl}");
        assert!(
            musl.contains("note: static musl builds lack dlopen()"),
            "{musl}"
        );
        assert!(musl.contains("glibc linux-x86_64"), "{musl}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn activation_requires_a_post_request_paint_restore_and_focus() {
        assert!(linux_activation_restore_confirmed(
            true,
            Some(false),
            Some(true)
        ));
        assert!(!linux_activation_restore_confirmed(
            false,
            Some(false),
            Some(true)
        ));
        assert!(!linux_activation_restore_confirmed(
            true,
            Some(true),
            Some(true)
        ));
        assert!(!linux_activation_restore_confirmed(
            true,
            Some(false),
            Some(false)
        ));
        assert!(!linux_activation_restore_confirmed(true, None, None));
    }
}

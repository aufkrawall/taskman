//! taskman — a cross-platform task manager.
//!
//! Windows note: the release build hides the console (`windows` subsystem);
//! CLI output (--selfcheck) re-attaches to the parent console when present.
//!
//! Startup architecture (implement.md §4): parse args → attach console only
//! for console modes → install no-disk-IO early logging → read settings →
//! hand a *lazy collector factory* to the GUI. The engine thread constructs
//! the sampler only after the first frame has been painted, so renderer/GPU
//! initialization never competes with heavy sampling.

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
//!
//! CLI:
//!   taskman                 open the GUI
//!   taskman --selfcheck     sample the system headlessly, print a summary, exit
//!   taskman --mock          use the deterministic mock collector (with GUI or --selfcheck)
//!   taskman --verbose       debug logging to console + file

mod action_executor;
mod app;
mod app_ui;
mod fonts;
mod icon_cache;
mod icons;
mod selfcheck;
mod tabs;
mod theme;
mod widgets;

use std::time::Instant;

/// Compact startup trace: one record per phase, emitted through tracing as
/// soon as logging exists (implement.md §3.1). Process-global so both main
/// and the eframe creator can mark phases without threading state through
/// non-Send closures.
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
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
    let mock = args.iter().any(|a| a == "--mock");
    let selfcheck = args.iter().any(|a| a == "--selfcheck");

    // Console attachment happens only for explicit console modes — a normal
    // GUI launch must not pay for it (implement.md §5.1).
    #[cfg(all(target_os = "windows", not(debug_assertions)))]
    if selfcheck || verbose {
        attach_parent_console();
    }

    if selfcheck || verbose {
        // CLI paths keep synchronous logging; they live or die by their output.
        let _log_guard = tm_core::logging::init(tm_core::logging::LogConfig {
            console: true,
            level: verbose.then(|| "debug".parse().expect("static")),
        });
    } else {
        // GUI: bounded in-memory ring first, disk after the first frame.
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
    // Locale first: number/date formats + default UI language follow the OS.
    tm_core::locale::init(tm_platform::detect_locale());

    let mut settings = tm_core::settings::Settings::load();
    StartupTrace::mark("minimal_config_loaded");

    // Diagnostics/UI tests: --size=WxH overrides the persisted window size
    // for this run.
    if let Some(sz) = args
        .iter()
        .find_map(|a| a.strip_prefix("--size=").map(|s| s.to_string()))
        .and_then(|s| parse_size_arg(&s))
    {
        settings.window_size = sz;
    }
    tm_core::i18n::set_lang(settings.language.resolve());
    let window_size = [settings.window_size[0], settings.window_size[1]];

    // The engine is NOT started here. The app receives a lazy factory and
    // starts it on its own engine thread after the first frame (§4.4).
    let initial_tab_arg = args
        .iter()
        .find_map(|a| a.strip_prefix("--tab=").map(|t| t.to_string()))
        .or_else(|| std::env::var("TASKMAN_TAB").ok());
    if let Some(t) = &initial_tab_arg {
        eprintln!("initial tab requested: {t}");
    }

    let title = tm_core::i18n::tr(tm_core::i18n::K::WindowTitle).to_string();

    // Renderer config: explicit FIFO present mode — always vsync'd through the
    // desktop compositor at the display's refresh rate. Unlike Mailbox/
    // Immediate this never bypasses composition, so it cannot tear or trip up
    // VRR (G-Sync/FreeSync) displays or DWM fullscreen optimizations.
    let options = |renderer: eframe::Renderer| {
        let mut opts = eframe::NativeOptions {
            renderer,
            viewport: eframe::egui::ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size(window_size)
                .with_min_inner_size([720.0, 480.0])
                .with_icon(icon_data()),
            ..Default::default()
        };
        #[cfg(feature = "wgpu")]
        {
            opts.wgpu_options =
                eframe::WgpuConfiguration::default().with_surface_config(eframe::SurfaceConfig {
                    present_mode: eframe::wgpu::PresentMode::Fifo,
                    desired_maximum_frame_latency: Some(2),
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
            // Fonts load asynchronously off the UI thread; the first frame(s)
            // render with egui's embedded defaults (§5.3).
            fonts::install_async(cc.egui_ctx.clone());
            let application = app::TaskManApp::new(cc, use_mock, app_settings, initial_tab);
            Ok(Box::new(application) as Box<dyn eframe::App>)
        };
        match eframe::run_native("Task-Manager", options(renderer), Box::new(creator)) {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!(error = %e, "renderer failed; falling back");
                last_err = Some(e);
                // Fallback safety: any workers/persistence started by the
                // failed attempt are owned per-TaskManApp and dropped with
                // it; nothing process-global leaks between attempts.
            }
        }
    }
    eprintln!(
        "taskman: no usable GPU renderer found: {:?}",
        last_err.map(|e| e.to_string())
    );
    std::process::exit(1);

    // Conditional element lists can't be expressed with `vec![]` because of
    // the cfg gates, so keep push-style construction here.
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

/// Parse a `WxH` CLI argument (diagnostics `--size=`).
fn parse_size_arg(s: &str) -> Option<[f32; 2]> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?);
    (w >= 200.0 && h >= 150.0).then_some([w, h])
}

/// Generate the app icon at runtime — a simple CPU-chip glyph on a blue square.
fn icon_data() -> eframe::egui::IconData {
    const S: usize = 64;
    let mut rgba = vec![0u8; S * S * 4];
    let accent = [0u8, 120, 212]; // #0078D4
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

/// Best-effort console attachment so CLI output is visible when launched from
/// an existing terminal despite the GUI subsystem.
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

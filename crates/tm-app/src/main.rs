//! taskman — a cross-platform task manager.
//!
//! Windows note: the release build hides the console (`windows` subsystem);
//! CLI output (--selfcheck) re-attaches to the parent console when present.
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

mod app;
mod app_ui;
mod fonts;
mod icon_cache;
mod icons;
mod selfcheck;
mod tabs;
mod theme;
mod widgets;

use std::time::Duration;

fn main() {
    #[cfg(all(target_os = "windows", not(debug_assertions)))]
    attach_parent_console();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
    let mock = args.iter().any(|a| a == "--mock");
    let selfcheck = args.iter().any(|a| a == "--selfcheck");

    // Logging: file always; console when verbose or selfcheck.
    let _log_guard = tm_core::logging::init(tm_core::logging::LogConfig {
        console: verbose || selfcheck,
        level: if verbose {
            Some("debug".parse().expect("static"))
        } else {
            None
        },
    });

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

    // Engine starts BEFORE the window so the first frame already has data.
    // (The collector itself initializes lazily on the engine thread.)
    let settings = tm_core::settings::Settings::load();
    tm_core::i18n::set_lang(settings.language.resolve());
    let window_size = [settings.window_size[0], settings.window_size[1]];
    let engine = spawn_engine(mock, settings.update_speed.interval());
    if settings.update_speed == tm_core::settings::UpdateSpeed::Paused {
        engine.pause();
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
            opts.wgpu_options = eframe::WgpuConfiguration::default().with_surface_config(
                eframe::SurfaceConfig {
                    present_mode: eframe::wgpu::PresentMode::Fifo,
                    desired_maximum_frame_latency: Some(2),
                },
            );
        }
        opts
    };

    let renderer_pref = std::env::var("TASKMAN_RENDERER").unwrap_or_default();
    let initial_tab = args
        .iter()
        .find_map(|a| a.strip_prefix("--tab=").map(|t| t.to_string()))
        .or_else(|| std::env::var("TASKMAN_TAB").ok());
    if let Some(t) = &initial_tab {
        eprintln!("initial tab requested: {t}");
    }

    let mut last_err = None;
    for renderer in preferred_renderers(&renderer_pref) {
        tracing::info!(?renderer, "trying renderer");
        let engine_handle = engine.clone();
        let use_mock = mock;
        let initial_tab = initial_tab.clone();
        let app_settings = settings.clone();
        let creator = move |cc: &eframe::CreationContext<'_>| {
            fonts::install(cc.egui_ctx.clone());
            theme::apply_startup(&cc.egui_ctx);
            let mut application = app::TaskManApp::new(cc, engine_handle, use_mock, app_settings);
            if let Some(t) = &initial_tab {
                application.set_initial_tab(t);
            }
            Ok(Box::new(application) as Box<dyn eframe::App>)
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

fn spawn_engine(mock: bool, interval: Duration) -> tm_core::EngineHandle {
    let (collector, _actions) = if mock {
        (
            Box::new(tm_core::mock::MockCollector::new())
                as Box<dyn tm_core::engine::SystemCollector>,
            None,
        )
    } else {
        let (c, a) = tm_platform::create_stack();
        (c, Some(a))
    };
    let (handle, _join) =
        tm_core::engine::spawn(collector, interval).expect("failed to spawn sampling engine");
    handle
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

/// Best-effort console attachment so `--selfcheck` output is visible when
/// launched from an existing terminal despite the GUI subsystem.
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

//! Setup wizard UI.
//!
//! Rendered with `tm-ui` - the task manager's own theme module - so the
//! installer mirrors the app's Windows 11 look exactly (proper dark and light
//! mode, same palette, same typography, same product icon). Structurally it
//! follows the green-curve setup: a header band with the product icon and
//! accent-colored version, an MIT license review that gates the Next button,
//! and a footer button row (Back / Next / Cancel).
//!
//! Layout note: the header and footer are `Panel::top` / `Panel::bottom`
//! chrome, NOT content in one vertical stack. A `ScrollArea` with
//! `auto_shrink(false)` consumes the whole remaining height, which pushes a
//! stacked footer out of the clip rect - that is exactly how the first
//! version of this wizard lost its buttons.

use std::path::PathBuf;
use std::sync::mpsc;

use eframe::egui;
use tm_ui::{fonts, theme};

use crate::install::{self, Event, StepState};
use crate::options::{Mode, Options};

/// Product icon (64x64 RGBA), shared with the app (`icon_data()` in
/// `tm-app/src/main.rs` uses the same asset).
const ICON_RAW: &[u8] = include_bytes!("../../tm-app/assets/icon_64.raw");

/// License text shown in the wizard, straight from the repository LICENSE.
const LICENSE_TEXT: &str = include_str!("../../../LICENSE");

pub fn run(opts: Options) -> tm_core::error::Result<()> {
    let title = window_title();
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title.clone())
            .with_inner_size([640.0, 560.0])
            .with_min_inner_size([640.0, 560.0])
            .with_resizable(false)
            .with_icon(icon_data()),
        // Only the CPU renderer is compiled into the installer: the wizard is
        // a short-lived window and this backend does sub-pixel text.
        renderer: eframe::Renderer::Software,
        ..Default::default()
    };
    eframe::run_native(
        &title,
        native,
        Box::new(|cc| Ok(Box::new(SetupApp::new(cc, opts)))),
    )
    .map_err(|error| tm_core::error::TmError::platform("setup window", error.to_string()))
}

fn window_title() -> String {
    format!("Task Manager {} Setup", env!("CARGO_PKG_VERSION"))
}

fn icon_data() -> egui::IconData {
    egui::IconData {
        width: 64,
        height: 64,
        rgba: ICON_RAW.to_vec(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Welcome,
    Options,
    Working,
    Done,
}

struct SetupApp {
    page: Page,
    opts: Options,
    existing: Option<PathBuf>,
    license_accepted: bool,
    icon: egui::TextureHandle,
    steps: Vec<(String, StepState, String)>,
    finished: Option<std::result::Result<(), String>>,
    worker: Option<mpsc::Receiver<Event>>,
    ctx: egui::Context,
}

impl SetupApp {
    fn new(cc: &eframe::CreationContext<'_>, opts: Options) -> Self {
        // Match the app's text rendering: the software renderer blends per
        // channel, so ClearType-style rasterization is enabled exactly as the
        // app enables it.
        theme::set_subpixel_capable(true, tm_platform::text_rendering::query(None));
        theme::apply_startup(&cc.egui_ctx);
        fonts::install_async(cc.egui_ctx.clone());
        let icon = cc.egui_ctx.load_texture(
            "setup-product-icon",
            egui::ColorImage::from_rgba_unmultiplied([64, 64], ICON_RAW),
            egui::TextureOptions::LINEAR,
        );
        let existing = install::existing_install();
        let page = match opts.mode {
            Mode::Install => Page::Welcome,
            Mode::Uninstall => Page::Options,
        };
        let steps = plan_steps(&opts);
        SetupApp {
            page,
            opts,
            existing,
            license_accepted: false,
            icon,
            steps,
            finished: None,
            worker: None,
            ctx: cc.egui_ctx.clone(),
        }
    }

    fn start_work(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.worker = Some(rx);
        let opts = self.opts.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let mut emit = |event: Event| {
                let _ = tx.send(event);
                ctx.request_repaint();
            };
            let result = match opts.mode {
                Mode::Install => install::install(&opts, &mut emit),
                Mode::Uninstall => install::uninstall(&opts, &mut emit),
            };
            emit(Event::Finished(result.map_err(|error| error.to_string())));
        });
    }

    fn pump_events(&mut self) {
        let mut finished = None;
        if let Some(rx) = &self.worker {
            while let Ok(event) = rx.try_recv() {
                match event {
                    Event::Step(index, state, detail) => {
                        if let Some(step) = self.steps.get_mut(index) {
                            step.1 = state;
                            step.2 = detail;
                        }
                    }
                    Event::Log(_) => {}
                    Event::Finished(result) => finished = Some(result),
                }
            }
        }
        if let Some(result) = finished {
            self.worker = None;
            self.finished = Some(result);
            self.page = Page::Done;
        }
    }
}

fn plan_steps(opts: &Options) -> Vec<(String, StepState, String)> {
    let steps = match opts.mode {
        Mode::Install => install::install_steps(opts),
        Mode::Uninstall => install::uninstall_steps(),
    };
    steps
        .into_iter()
        .map(|label| (label.to_string(), StepState::Pending, String::new()))
        .collect()
}

impl eframe::App for SetupApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        theme::ensure_visuals(&ctx);
        fonts::poll_async_apply(&ctx);
        self.pump_events();
        let pal = theme::palette_ctx(&ctx);

        header_band(ui, &pal, self);
        footer_band(ui, &pal, self);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(pal.window_bg))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        ui.add_space(16.0);
                        match self.page {
                            Page::Welcome => welcome_page(ui, &pal, self),
                            Page::Options => options_page(ui, &pal, self),
                            Page::Working => working_page(ui, &pal, self),
                            Page::Done => done_page(ui, &pal, self),
                        }
                        ui.add_space(16.0);
                    });
            });
    }
}

/// Chrome band on top: product icon, name, accent-colored version - the
/// green-curve layout, in task man's palette.
fn header_band(ui: &mut egui::Ui, pal: &theme::Palette, app: &SetupApp) {
    egui::Panel::top(egui::Id::new("setup-header"))
        .resizable(false)
        .frame(
            egui::Frame::NONE
                .fill(pal.sidebar_bg)
                .inner_margin(egui::Margin::symmetric(18, 14)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Image::new(egui::load::SizedTexture::new(
                    app.icon.id(),
                    egui::vec2(36.0, 36.0),
                )));
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("Task Manager")
                            .size(22.0)
                            .strong()
                            .color(pal.text),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                            .size(12.0)
                            .color(pal.accent),
                    );
                });
            });
        });
}

/// Chrome band on the bottom: the wizard button row.
fn footer_band(ui: &mut egui::Ui, pal: &theme::Palette, app: &mut SetupApp) {
    egui::Panel::bottom(egui::Id::new("setup-footer"))
        .resizable(false)
        .frame(
            egui::Frame::NONE
                .fill(pal.sidebar_bg)
                .inner_margin(egui::Margin::symmetric(18, 12)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Left side: the secondary "already installed" affordance.
                if app.page == Page::Welcome
                    && app.existing.is_some()
                    && ui.button("Uninstall...").clicked()
                {
                    app.opts.mode = Mode::Uninstall;
                    app.steps = plan_steps(&app.opts);
                    app.page = Page::Options;
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Right side, right to left: Cancel is the rightmost
                    // button, matching the green-curve setup row.
                    match app.page {
                        Page::Welcome => {
                            if button(ui, "Cancel", true).clicked() {
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            if button(ui, "Next >", app.license_accepted).clicked() {
                                app.page = Page::Options;
                            }
                            button(ui, "Back", false);
                        }
                        Page::Options => {
                            if button(ui, "Cancel", true).clicked() {
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            let label = if app.opts.mode == Mode::Uninstall {
                                "Uninstall"
                            } else {
                                "Install"
                            };
                            if button(ui, label, true).clicked() {
                                app.start_work();
                                app.page = Page::Working;
                            }
                            let can_go_back = app.opts.mode == Mode::Install;
                            if button(ui, "Back", can_go_back).clicked() {
                                app.page = Page::Welcome;
                            }
                        }
                        Page::Working => {
                            // No cancel once files move: a half-applied
                            // install is worse than a finished one.
                            button(ui, "Working...", false);
                        }
                        Page::Done => {
                            if button(ui, "Close", true).clicked() {
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            let success = matches!(&app.finished, Some(Ok(())));
                            if success
                                && app.opts.mode == Mode::Install
                                && button(ui, "Launch Task Manager", true).clicked()
                            {
                                if let Some(dir) = install::install_dir_for_display() {
                                    let _ = crate::win::launch(&dir.join("taskman.exe"));
                                }
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                    }
                });
            });
        });
}

/// A footer button with the row's standard size. Disabled buttons keep the
/// same footprint, so the row never reflows between pages.
fn button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(label).min_size(egui::vec2(116.0, 28.0)),
    )
}

fn welcome_page(ui: &mut egui::Ui, pal: &theme::Palette, app: &mut SetupApp) {
    let body = if app.existing.is_some() {
        "An existing installation was found. Installing again updates it in \
         place; your settings are preserved."
    } else {
        "This wizard installs Task Manager on this computer. The background \
         service is installed along with it, so the app works without UAC \
         prompts on first start."
    };
    ui.label(egui::RichText::new(body).size(13.0).color(pal.text));
    if let Some(dir) = &app.existing {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("Installed at: {}", dir.display()))
                .size(11.0)
                .color(pal.text_dim),
        );
    }

    // License review, like the green-curve setup: the terms are shown before
    // anything is installed, and Next stays disabled until they are accepted.
    ui.add_space(14.0);
    ui.label(
        egui::RichText::new(
            "This program is released under the MIT license. Please review \
             the terms before continuing.",
        )
        .size(13.0)
        .color(pal.text),
    );
    ui.add_space(8.0);
    egui::Frame::NONE
        .fill(pal.card_bg_sunken)
        .stroke(egui::Stroke::new(1.0, pal.stroke))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(230.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(LICENSE_TEXT)
                                .monospace()
                                .size(11.0)
                                .color(pal.text),
                        )
                        .wrap(),
                    );
                });
        });
    ui.add_space(10.0);
    ui.checkbox(
        &mut app.license_accepted,
        egui::RichText::new("I accept the terms of the MIT license").size(13.0),
    );
}

fn options_page(ui: &mut egui::Ui, pal: &theme::Palette, app: &mut SetupApp) {
    if app.opts.mode == Mode::Uninstall {
        ui.label(
            egui::RichText::new("Remove Task Manager")
                .size(15.5)
                .color(pal.text),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "This removes the program files, the background service and \
                 the shortcuts. Your personal settings are kept.",
            )
            .size(13.0)
            .color(pal.text),
        );
        return;
    }

    ui.label(
        egui::RichText::new("Installation options")
            .size(15.5)
            .color(pal.text),
    );
    ui.add_space(8.0);
    ui.add(egui::Checkbox::new(
        &mut app.opts.service,
        egui::RichText::new("Install background service (recommended)").size(13.0),
    ));
    ui.label(
        egui::RichText::new(
            "Registers the TaskManCore service so Task Manager can control \
             processes without UAC prompts.",
        )
        .size(11.0)
        .color(pal.text_dim),
    );
    ui.add_space(6.0);
    ui.add(egui::Checkbox::new(
        &mut app.opts.start_menu,
        egui::RichText::new("Create a Start menu shortcut").size(13.0),
    ));
    ui.add(egui::Checkbox::new(
        &mut app.opts.desktop,
        egui::RichText::new("Create a desktop shortcut").size(13.0),
    ));
    ui.add(egui::Checkbox::new(
        &mut app.opts.launch,
        egui::RichText::new("Launch Task Manager when setup completes").size(13.0),
    ));
    ui.add_space(10.0);
    if let Some(dir) = install::install_dir_for_display() {
        ui.label(
            egui::RichText::new(format!(
                "Install location: {} (fixed - the service requires a \
                 protected directory)",
                dir.display()
            ))
            .size(11.0)
            .color(pal.text_dim),
        );
    }
}

fn working_page(ui: &mut egui::Ui, pal: &theme::Palette, app: &SetupApp) {
    ui.label(
        egui::RichText::new(if app.opts.mode == Mode::Uninstall {
            "Removing Task Manager..."
        } else {
            "Installing Task Manager..."
        })
        .size(15.5)
        .color(pal.text),
    );
    ui.add_space(10.0);
    for (label, state, detail) in &app.steps {
        ui.horizontal(|ui| {
            let (glyph, color) = match state {
                StepState::Pending => ("○", pal.text_dim),
                StepState::Running => ("…", pal.accent),
                StepState::Done => ("✓", pal.ok_green),
                StepState::Skipped => ("–", pal.text_dim),
                StepState::Failed => ("✕", pal.warn_orange),
            };
            ui.label(egui::RichText::new(glyph).size(13.0).color(color));
            ui.label(egui::RichText::new(label).size(13.0).color(pal.text));
        });
        if !detail.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label(egui::RichText::new(detail).size(11.0).color(pal.text_dim));
            });
        }
        ui.add_space(3.0);
    }
}

fn done_page(ui: &mut egui::Ui, pal: &theme::Palette, app: &SetupApp) {
    let success = matches!(&app.finished, Some(Ok(())));
    let (title, color) = if success {
        ("Completed", pal.ok_green)
    } else {
        ("Setup failed", pal.warn_orange)
    };
    ui.label(egui::RichText::new(title).size(15.5).color(color));
    ui.add_space(8.0);
    match &app.finished {
        Some(Ok(())) if app.opts.mode == Mode::Uninstall => {
            ui.label(
                egui::RichText::new("Task Manager was removed from this computer.")
                    .size(13.0)
                    .color(pal.text),
            );
        }
        Some(Ok(())) => {
            ui.label(
                egui::RichText::new("Task Manager was installed successfully.")
                    .size(13.0)
                    .color(pal.text),
            );
            if app.opts.service {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "The background service is installed and running; the \
                         app will not ask for elevation on first start.",
                    )
                    .size(11.0)
                    .color(pal.text_dim),
                );
            }
        }
        Some(Err(error)) => {
            ui.label(egui::RichText::new(error).size(13.0).color(pal.text));
        }
        None => {}
    }
}

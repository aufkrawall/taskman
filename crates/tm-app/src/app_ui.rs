//! UI chrome of the root app: top search bar, navigation rail (hamburger
//! collapsible), per-tab command header, dialogs, toasts. Fully localized
//! (DE/EN) via tm-core::i18n.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke};
use tm_core::i18n::{self, K};
use tm_core::settings::{Settings, ThemeMode};

use crate::app::TaskManApp;
use crate::icons;
use crate::icons::Icon;
use crate::theme::{self, Palette};

pub fn apply_theme(ctx: &egui::Context, mode: ThemeMode) {
    ctx.set_theme(match mode {
        ThemeMode::System => egui::ThemePreference::System,
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::Dark => egui::ThemePreference::Dark,
    });
}

pub const SIDEBAR_W: f32 = 212.0;
pub const SIDEBAR_W_COLLAPSED: f32 = 54.0;

// ---------------------------------------------------------------- top search

/// Centered search field spanning the top of the window.
pub fn top_search_panel(app: &mut TaskManApp, ui_root: &mut egui::Ui, pal: &Palette) {
    egui::Panel::top(egui::Id::new("topsearch"))
        .resizable(false)
        .frame(
            egui::Frame::NONE
                .fill(pal.window_bg)
                .inner_margin(egui::Margin::symmetric(0, 6)),
        )
        .show(ui_root, |ui| {
            let box_w = 560.0f32.min(ui.available_width() * 0.7);
            let x = (ui.available_width() - box_w) / 2.0;
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), Sense::hover());
            let box_rect = Rect::from_min_size(
                Pos2::new(rect.left() + x, rect.top()),
                egui::vec2(box_w, 34.0),
            );
            ui.painter().rect_filled(box_rect, 16.0, pal.card_bg);
            ui.painter().rect_stroke(
                box_rect,
                16.0,
                Stroke::new(1.0, pal.stroke),
                egui::StrokeKind::Inside,
            );
            crate::icons::draw_at(
                ui,
                Rect::from_center_size(
                    Pos2::new(box_rect.left() + 18.0, box_rect.center().y),
                    egui::vec2(15.0, 15.0),
                ),
                Icon::Search,
                pal.text_dim,
            );

            let edit_rect = Rect::from_min_max(
                Pos2::new(box_rect.left() + 34.0, box_rect.top() + 3.0),
                Pos2::new(box_rect.right() - 10.0, box_rect.bottom() - 3.0),
            );
            let mut edit_ui = ui.new_child(egui::UiBuilder::new().max_rect(edit_rect));
            edit_ui.add(
                egui::TextEdit::singleline(&mut app.search)
                    .hint_text(i18n::tr(K::SearchHint))
                    .font(FontId::proportional(12.5))
                    .frame(egui::Frame::NONE)
                    .desired_width(edit_rect.width()),
            );
        });
}

// ---------------------------------------------------------------- sidebar

pub fn sidebar(app: &mut TaskManApp, ui_root: &mut egui::Ui, pal: &Palette) {
    let collapsed = app.shared.settings.sidebar_collapsed;
    let w = if collapsed {
        SIDEBAR_W_COLLAPSED
    } else {
        SIDEBAR_W
    };
    egui::Panel::left(egui::Id::new("nav"))
        .resizable(false)
        .min_size(w)
        .max_size(w)
        .frame(
            egui::Frame::NONE
                .fill(pal.sidebar_bg)
                .inner_margin(egui::Margin {
                    left: 8,
                    right: 8,
                    top: 4,
                    bottom: 8,
                }),
        )
        .show(ui_root, |ui| {
            // Hamburger toggle.
            if icon_button(ui, pal, Icon::Hamburger, 32.0) {
                app.shared.settings.sidebar_collapsed = !collapsed;
            }
            ui.add_space(8.0);

            for tab in crate::app::Tab::ALL {
                let selected = app.tab == tab;
                let resp = nav_item(ui, pal, tab.icon(), tab.label(), selected, collapsed);
                if resp.clicked() {
                    app.tab = tab;
                }
                if collapsed && resp.hovered() {
                    resp.on_hover_text(tab.label());
                }
            }

            // Bottom: settings gear.
            ui.add_space(ui.available_height() - 36.0);
            let resp = nav_item(
                ui,
                pal,
                Icon::Settings,
                i18n::tr(K::Settings),
                false,
                collapsed,
            );
            if resp.clicked() {
                app.show_settings = true;
            }
            if collapsed && resp.hovered() {
                resp.on_hover_text(i18n::tr(K::Settings));
            }
        });
}

fn nav_item(
    ui: &mut egui::Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
    selected: bool,
    collapsed: bool,
) -> egui::Response {
    let h = 34.0;
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), h),
        Sense::click().union(Sense::hover()),
    );
    let painter = ui.painter();
    if selected {
        painter.rect_filled(
            rect,
            4.0,
            Color32::from_white_alpha(if pal.sidebar_bg == theme::LIGHT.sidebar_bg {
                255 - 30
            } else {
                26
            }),
        );
        let bar = Rect::from_min_size(
            Pos2::new(rect.left(), rect.center().y - 10.0),
            egui::vec2(3.0, 20.0),
        );
        painter.rect_filled(bar, 2.0, pal.accent);
    } else if resp.hovered() {
        painter.rect_filled(rect, 4.0, Color32::from_white_alpha(10));
    }

    if collapsed {
        let icon_rect = Rect::from_center_size(rect.center(), egui::vec2(20.0, 20.0));
        icons::draw_at(ui, icon_rect, icon, pal.text);
    } else {
        let icon_rect = Rect::from_center_size(
            Pos2::new(rect.left() + 22.0, rect.center().y),
            egui::vec2(20.0, 20.0),
        );
        icons::draw_at(ui, icon_rect, icon, pal.text);
        painter.text(
            Pos2::new(rect.left() + 42.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            pal.text,
        );
    }
    resp
}

fn icon_button(ui: &mut egui::Ui, pal: &Palette, icon: Icon, size: f32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, Color32::from_white_alpha(12));
    }
    crate::icons::draw_at(
        ui,
        Rect::from_center_size(rect.center(), egui::vec2(18.0, 18.0)),
        icon,
        pal.text,
    );
    resp.clicked()
}

// ---------------------------------------------------------------- tab header

/// Per-tab command bar: bold title left; right-aligned command buttons
/// ("Neuen Task ausführen", tab-specific ones, "…" overflow).
pub fn tab_header(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &Palette,
    extra: impl FnOnce(&mut TaskManApp, &mut egui::Ui),
    menu: impl FnOnce(&mut TaskManApp, &mut egui::Ui),
) {
    let title = app.tab.label();
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(egui::RichText::new(title).size(15.5).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            ellipsis_menu(app, ui, pal, menu);
            extra(app, ui);
            vsep(ui, pal);
            if cmd_button(ui, pal, Icon::RunTask, i18n::tr(K::RunNewTask), true) {
                app.run_dialog_open = true;
            }
        });
        ui.add_space(4.0);
    });
    ui.add_space(2.0);
}

/// Flat command button with icon + label, Win11 toolbar style.
pub fn cmd_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
    enabled: bool,
) -> bool {
    let w = 28.0 + label.chars().count() as f32 * 7.1 + 6.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 30.0), Sense::click());
    let mut clicked = false;
    if enabled {
        if resp.hovered() {
            ui.painter().rect_filled(rect, 4.0, pal.card_bg_hover);
        }
        if resp.clicked() {
            clicked = true;
        }
    }
    let color = if enabled {
        pal.text
    } else {
        pal.text_dim.gamma_multiply(0.55)
    };
    crate::icons::draw_at(
        ui,
        Rect::from_center_size(
            Pos2::new(rect.left() + 14.0, rect.center().y),
            egui::vec2(17.0, 17.0),
        ),
        icon,
        color,
    );
    ui.painter().text(
        Pos2::new(rect.left() + 28.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.5),
        color,
    );
    clicked && enabled
}

/// Thin vertical separator between button groups.
pub fn vsep(ui: &mut egui::Ui, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 26.0), Sense::hover());
    ui.painter().line_segment(
        [
            Pos2::new(rect.center().x, rect.top() + 3.0),
            Pos2::new(rect.center().x, rect.bottom() - 3.0),
        ],
        Stroke::new(1.0, pal.stroke),
    );
}

/// The "…" overflow menu (egui handles opening/closing/positioning).
pub fn ellipsis_menu(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    _pal: &Palette,
    items: impl FnOnce(&mut TaskManApp, &mut egui::Ui),
) {
    ui.menu_button(egui::RichText::new("…").size(16.0), |ui| {
        ui.set_min_width(170.0);
        items(app, ui);
    });
}

// ---------------------------------------------------------------- dialogs

/// Every switch applies instantly AND persists right away (tiny atomic JSON
/// write, ~1 ms) so a crash can never lose the last change.
pub fn settings_dialog(app: &mut TaskManApp, ctx: &egui::Context, _pal: &theme::Palette) {
    let mut open = true;
    egui::Window::new(i18n::tr(K::Settings))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_width(380.0);

            ui.heading(i18n::tr(K::DesignHeading));
            ui.horizontal(|ui| {
                for (mode, key) in [
                    (ThemeMode::System, K::ThemeSystem),
                    (ThemeMode::Light, K::ThemeLight),
                    (ThemeMode::Dark, K::ThemeDark),
                ] {
                    if ui
                        .selectable_label(app.shared.settings.theme == mode, i18n::tr(key))
                        .clicked()
                    {
                        app.shared.settings.theme = mode;
                        apply_theme(ctx, mode);
                        app.shared.settings.save();
                    }
                }
            });

            ui.add_space(10.0);
            ui.heading(i18n::tr(K::UpdateSpeedHeading));
            ui.horizontal_wrapped(|ui| {
                for speed in [
                    tm_core::settings::UpdateSpeed::High,
                    tm_core::settings::UpdateSpeed::Normal,
                    tm_core::settings::UpdateSpeed::Low,
                    tm_core::settings::UpdateSpeed::Paused,
                ] {
                    let key = match speed {
                        tm_core::settings::UpdateSpeed::High => K::SpdHigh,
                        tm_core::settings::UpdateSpeed::Normal => K::SpdNormal,
                        tm_core::settings::UpdateSpeed::Low => K::SpdLow,
                        tm_core::settings::UpdateSpeed::Paused => K::SpdPaused,
                    };
                    if ui
                        .selectable_label(app.shared.settings.update_speed == speed, i18n::tr(key))
                        .clicked()
                    {
                        app.shared.settings.update_speed = speed;
                        match speed {
                            tm_core::settings::UpdateSpeed::Paused => app.engine.pause(),
                            _ => {
                                app.engine.resume();
                                app.engine.set_interval(speed.interval());
                            }
                        }
                        app.shared.settings.save();
                    }
                }
            });

            ui.add_space(10.0);
            // ---- language switcher (applies live, persists immediately)
            ui.heading(i18n::tr(K::LanguageLabel));
            ui.horizontal(|ui| {
                for (choice, label) in [
                    (
                        tm_core::i18n::LangChoice::System,
                        i18n::tr(K::ThemeSystem),
                    ),
                    (tm_core::i18n::LangChoice::De, "Deutsch"),
                    (tm_core::i18n::LangChoice::En, "English"),
                ] {
                    if ui
                        .selectable_label(app.shared.settings.language == choice, label)
                        .clicked()
                    {
                        app.shared.settings.language = choice;
                        i18n::set_lang(choice.resolve());
                        ctx.send_viewport_cmd(egui::ViewportCommand::Title(
                            i18n::tr(K::WindowTitle).to_string(),
                        ));
                        app.shared.settings.save();
                    }
                }
            });

            ui.add_space(10.0);
            let mut on_top = app.shared.settings.always_on_top;
            if ui.checkbox(&mut on_top, i18n::tr(K::AlwaysOnTop)).changed() {
                app.shared.settings.always_on_top = on_top;
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if on_top {
                    egui::WindowLevel::AlwaysOnTop
                } else {
                    egui::WindowLevel::Normal
                }));
                app.shared.settings.save();
            }

            ui.add_space(10.0);
            ui.label(i18n::tr(K::GraphWindowLabel));
            ui.horizontal(|ui| {
                for secs in [30u32, 60, 120] {
                    if ui
                        .selectable_label(
                            app.shared.settings.graph_seconds == secs,
                            format!("{secs} s"),
                        )
                        .clicked()
                    {
                        app.shared.settings.graph_seconds = secs;
                        app.shared.settings.save();
                    }
                }
            });

            ui.add_space(10.0);
            ui.label(i18n::tr(K::ScaleLabel));
            ui.horizontal(|ui| {
                for (zoom, label) in [
                    (0.8f32, "80 %"),
                    (0.9, "90 %"),
                    (1.0, "100 %"),
                    (1.1, "110 %"),
                    (1.25, "125 %"),
                ] {
                    if ui
                        .selectable_label((app.shared.settings.ui_zoom - zoom).abs() < 0.01, label)
                        .clicked()
                    {
                        app.shared.settings.ui_zoom = zoom;
                        ctx.set_zoom_factor(zoom);
                        app.shared.settings.save();
                    }
                }
            });

            ui.add_space(14.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::Reset)).clicked() {
                    let defaults = Settings::default();
                    apply_theme(ctx, defaults.theme);
                    ctx.set_zoom_factor(defaults.ui_zoom);
                    app.engine.resume();
                    app.engine.set_interval(defaults.update_speed.interval());
                    // Language follows System again after a reset.
                    i18n::set_lang(defaults.language.resolve());
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(
                        i18n::tr(K::WindowTitle).to_string(),
                    ));
                    app.shared.settings = defaults;
                    app.shared.settings.save();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(i18n::tr(K::Close)).clicked() {
                        app.show_settings = false;
                        app.shared.settings.save();
                    }
                });
            });
        });
    if !open {
        app.show_settings = false;
    }
}

pub fn run_task_dialog(app: &mut TaskManApp, ctx: &egui::Context, _pal: &theme::Palette) {
    let mut open = true;
    egui::Window::new(i18n::tr(K::RunDialogTitle))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(i18n::tr(K::RunPrompt));
            ui.add_space(4.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut app.run_dialog_text)
                    .hint_text(i18n::tr(K::RunHint))
                    .desired_width(f32::INFINITY),
            );
            resp.request_focus();
            ui.add_space(4.0);
            ui.checkbox(&mut app.run_elevated, i18n::tr(K::RunElevated));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::Cancel)).clicked() {
                    app.run_dialog_open = false;
                }
                // Browse for an executable/document to run.
                if ui.button(i18n::tr(K::Browse)).clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_file()
                {
                    app.run_dialog_text = path.to_string_lossy().into_owned();
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button(i18n::tr(K::Ok)).clicked() || enter)
                    && !app.run_dialog_text.trim().is_empty()
                {
                    // Launch (including the brief failure-probe wait) happens on
                    // a worker thread; the dialog closes immediately.
                    let actions = app.actions.clone();
                    let cmdline = app.run_dialog_text.trim().to_string();
                    let elevated = app.run_elevated;
                    let toasts = app.shared.toasts.clone();
                    let spawned =
                        std::thread::Builder::new()
                            .name("tm-run".into())
                            .spawn(move || {
                                let result = actions.run_new_task(&cmdline, elevated);
                                let mut t = toasts.lock().unwrap_or_else(|e| e.into_inner());
                                let msg = match result {
                                    Ok(()) => i18n::trf(K::StartedMsg, &[&cmdline]),
                                    Err(e) => i18n::trf(K::ErrMsg, &[&e.to_string()]),
                                };
                                t.push((msg, std::time::Instant::now()));
                                if t.len() > 6 {
                                    t.remove(0);
                                }
                            });
                    if spawned.is_err() {
                        app.shared.toast(i18n::tr(K::LaunchFailed));
                    }
                    app.run_dialog_open = false;
                }
            });
        });
    if !open {
        app.run_dialog_open = false;
    }
}

pub fn draw_toasts(app: &TaskManApp, ctx: &egui::Context) {
    let mut toasts = tm_core::sync::lock(&app.shared.toasts);
    toasts.retain(|(_, born)| born.elapsed() < std::time::Duration::from_secs(4));
    let mut y_offset = 0.0f32;
    for (msg, born) in toasts.iter() {
        let age = born.elapsed().as_secs_f32();
        let alpha = ((4.0 - age) * 255.0).clamp(90.0, 255.0) as u8;
        let id = egui::Id::new(format!("toast-{}", born.elapsed().as_nanos()));
        egui::Area::new(id)
            .anchor(Align2::RIGHT_BOTTOM, [-12.0, -12.0 - y_offset])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::window(ui.style())
                    .fill(Color32::from_black_alpha(alpha.min(220)))
                    .stroke(Stroke::new(1.0, theme::LIGHT.stroke))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_max_width(380.0);
                        ui.label(
                            egui::RichText::new(msg)
                                .size(12.5)
                                .color(Color32::from_white_alpha(alpha)),
                        );
                    });
            });
        y_offset += 46.0;
    }
}

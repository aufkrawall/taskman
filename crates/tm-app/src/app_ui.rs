//! UI chrome of the root app: top search bar, navigation rail (hamburger
//! collapsible), per-tab command header, dialogs, toasts. Fully localized
//! (DE/EN) via tm-core::i18n.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke};
use tm_core::i18n::{self, K};
use tm_core::settings::{RenderMode, Settings, TextSmoothing, ThemeMode};

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

/// Centered search field spanning the top of the window. The blank strip on
/// either side behaves as an additional native titlebar drag region while the
/// search box remains a normal interactive text control.
///
/// This strip fills with `window_bg`, which is also what the native caption
/// directly above it is painted with (`TaskManApp::sync_title_bar`) — the two
/// are meant to read as one surface.
pub fn top_search_panel(app: &mut TaskManApp, ui_root: &mut egui::Ui, pal: &Palette) {
    egui::Panel::top(egui::Id::new("topsearch"))
        .resizable(false)
        .frame(
            egui::Frame::NONE
                .fill(pal.window_bg)
                .inner_margin(egui::Margin::symmetric(0, 6)),
        )
        .show(ui_root, |ui| {
            let box_w = 495.0f32.min(ui.available_width() * 0.7);
            let x = (ui.available_width() - box_w) / 2.0;
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), Sense::hover());
            let box_rect = Rect::from_min_size(
                Pos2::new(rect.left() + x, rect.top()),
                egui::vec2(box_w, 34.0),
            );

            // Register only the blank left/right pieces as drag handles. Do
            // not put a transparent drag widget over the search box: that
            // would steal focus, text selection and double-click gestures.
            let left_drag = Rect::from_min_max(rect.min, Pos2::new(box_rect.left(), rect.bottom()));
            let right_drag = Rect::from_min_max(Pos2::new(box_rect.right(), rect.top()), rect.max);
            for (id, drag_rect) in [("top-drag-left", left_drag), ("top-drag-right", right_drag)] {
                if drag_rect.width() > 0.0 {
                    titlebar_drag_region(ui, egui::Id::new(id), drag_rect);
                }
            }

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

            // The clear button only exists while there is something to clear,
            // so the field's text never has to end short of the rounded edge
            // for a control that is not there.
            let has_text = !app.search.is_empty();
            let clear_rect = Rect::from_center_size(
                Pos2::new(box_rect.right() - 18.0, box_rect.center().y),
                egui::vec2(24.0, 24.0),
            );
            let text_right = if has_text {
                clear_rect.left() - 4.0
            } else {
                box_rect.right() - 10.0
            };

            let edit_rect = Rect::from_min_max(
                Pos2::new(box_rect.left() + 34.0, box_rect.top() + 3.0),
                Pos2::new(text_right, box_rect.bottom() - 3.0),
            );
            let mut edit_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(edit_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            let edit = edit_ui.add(
                egui::TextEdit::singleline(&mut app.search)
                    .hint_text(i18n::tr(K::SearchHint))
                    .font(FontId::proportional(15.0))
                    .frame(egui::Frame::NONE)
                    .desired_width(edit_rect.width())
                    .id(egui::Id::new("global-search")),
            );

            if has_text {
                let resp = ui
                    .interact(
                        clear_rect,
                        egui::Id::new("global-search-clear"),
                        Sense::click(),
                    )
                    .on_hover_text(i18n::tr(K::ClearSearch));
                if resp.hovered() {
                    ui.painter()
                        .circle_filled(clear_rect.center(), 11.0, pal.card_bg_hover);
                }
                crate::icons::draw_at(
                    ui,
                    Rect::from_center_size(clear_rect.center(), egui::vec2(11.0, 11.0)),
                    Icon::Close,
                    if resp.hovered() {
                        pal.text
                    } else {
                        pal.text_dim
                    },
                );
                if resp.clicked() {
                    app.search.clear();
                }
            }

            // Escape clears rather than only unfocusing: with the field empty
            // it is the same keystroke that leaves it, and a stale filter is
            // the one thing a user cannot see the cause of.
            if edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                app.search.clear();
            }
        });
}

/// Make `rect` behave like the native title bar: press-and-move drags the
/// window, double-click maximizes/restores it.
///
/// The window move starts on the BUTTON PRESS, not on egui's `drag_started()`.
/// egui only reports a drag once the pointer has travelled past its drag
/// threshold, and everything up to that point is movement the window did not
/// follow — so the window jumped to catch up the moment the drag was
/// recognized, and dragging here felt worse than dragging the real caption.
/// `StartDrag` hands the gesture to the window manager, which then owns the
/// whole move, so issuing it early costs nothing.
fn titlebar_drag_region(ui: &egui::Ui, id: egui::Id, rect: Rect) {
    /// Double-click window, in seconds. Windows' own is configurable
    /// (`SPI_GETDOUBLECLICKTIME`, 500 ms by default); this only decides
    /// between "maximize" and "move", so the default is close enough.
    const DOUBLE_CLICK_S: f64 = 0.5;

    let resp = ui.interact(rect, id, Sense::click_and_drag());
    let (pressed, now, pos) = ui.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.time,
            i.pointer.interact_pos(),
        )
    });
    if !pressed || !resp.contains_pointer() {
        return;
    }

    // Double-click is detected here rather than through `Response`: handing
    // the gesture to the window manager below ends egui's view of the press,
    // so its own click/double-click bookkeeping never completes.
    let previous = ui.ctx().data(|d| d.get_temp::<(f64, Pos2)>(id));
    let position = pos.unwrap_or(rect.center());
    ui.ctx().data_mut(|d| d.insert_temp(id, (now, position)));
    if let Some((last, last_pos)) = previous
        && now - last <= DOUBLE_CLICK_S
        && last_pos.distance(position) <= 8.0
    {
        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        return;
    }

    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
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
            if icon_button(ui, pal, Icon::Hamburger, 32.0, collapsed) {
                app.shared.settings.sidebar_collapsed = !collapsed;
                app.shared.settings.save();
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
    let h = 38.0;
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
            Pos2::new(rect.left(), rect.center().y - 9.0),
            egui::vec2(3.0, 18.0),
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
            FontId::proportional(15.0),
            pal.text,
        );
    }
    resp
}

fn icon_button(ui: &mut egui::Ui, pal: &Palette, icon: Icon, size: f32, center: bool) -> bool {
    let w = if center {
        ui.available_width().max(size)
    } else {
        size
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, size), Sense::click());
    if resp.hovered() {
        let hover = if center {
            Rect::from_center_size(rect.center(), egui::vec2(size, size))
        } else {
            rect
        };
        ui.painter()
            .rect_filled(hover, 4.0, Color32::from_white_alpha(12));
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

pub fn tab_header(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &Palette,
    extra: impl FnOnce(&mut TaskManApp, &mut egui::Ui),
    menu: impl FnOnce(&mut TaskManApp, &mut egui::Ui),
) {
    let title = app.tab.label();
    // How many rows a multi-select command would act on. The toolbar buttons
    // are the same size either way, so without this the difference between
    // ending one process and ending thirty is invisible until the dialog.
    let selected = matches!(
        app.tab,
        crate::app::Tab::Processes | crate::app::Tab::Details
    )
    .then(|| app.selection.len())
    .filter(|count| *count > 1);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(egui::RichText::new(title).size(15.5).strong());
        if let Some(count) = selected {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(i18n::trf(K::SelectedCount, &[&count.to_string()]))
                    .size(12.5)
                    .color(pal.text_dim),
            );
        }
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

pub fn cmd_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
    enabled: bool,
) -> bool {
    let text_w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), FontId::proportional(13.0), Color32::WHITE)
        .size()
        .x;
    let w = 28.0 + text_w + 6.0;
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
        FontId::proportional(13.0),
        color,
    );
    clicked && enabled
}

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

pub fn ellipsis_menu(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    _pal: &Palette,
    items: impl FnOnce(&mut TaskManApp, &mut egui::Ui),
) {
    crate::widgets::menu::menu_button(
        ui,
        egui::Button::new(egui::RichText::new("…").size(16.0)),
        |ui| {
            ui.set_min_width(180.0);
            items(app, ui);
        },
    );
}

// ---------------------------------------------------------------- dialogs

pub fn settings_dialog(app: &mut TaskManApp, ctx: &egui::Context, _pal: &theme::Palette) {
    let mut open = true;
    egui::Window::new(i18n::tr(K::Settings))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([420.0, 640.0])
        .min_size([400.0, 360.0])
        .vscroll(true)
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
            ui.heading(i18n::tr(K::LanguageLabel));
            ui.horizontal(|ui| {
                for (choice, label) in [
                    (tm_core::i18n::LangChoice::System, i18n::tr(K::ThemeSystem)),
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
            ui.label(i18n::tr(K::DefaultStartPageLabel));
            ui.horizontal_wrapped(|ui| {
                for tab in crate::app::Tab::ALL {
                    if ui
                        .selectable_label(
                            app.shared.settings.default_start_page == tab.key(),
                            tab.label(),
                        )
                        .clicked()
                    {
                        app.shared.settings.default_start_page = tab.key().to_string();
                        app.save_settings();
                    }
                }
            });

            ui.add_space(10.0);
            ui.label(i18n::tr(K::TextSmoothingLabel));
            ui.horizontal(|ui| {
                for (mode, key) in [
                    (TextSmoothing::Sharp, K::SmoothingSharp),
                    (TextSmoothing::Standard, K::SmoothingStandard),
                    (TextSmoothing::Smooth, K::SmoothingSmooth),
                ] {
                    if ui
                        .selectable_label(
                            app.shared.settings.text_smoothing == mode,
                            i18n::tr(key),
                        )
                        .clicked()
                    {
                        app.shared.settings.text_smoothing = mode;
                        // Two halves have to be re-pushed: the coverage ramp
                        // lives in the visuals' text options, the grid-fitting
                        // target in each face's FontTweak.
                        theme::set_text_smoothing(mode);
                        theme::refresh_text_rendering(ctx);
                        crate::fonts::reapply(ctx);
                        app.save_settings();
                    }
                }
            });
            ui.label(
                egui::RichText::new(i18n::tr(K::TextSmoothingHint))
                    .size(11.0)
                    .color(_pal.text_dim),
            );

            ui.add_space(10.0);
            ui.label(i18n::tr(K::RenderModeLabel));
            ui.horizontal_wrapped(|ui| {
                for (mode, key) in [
                    (RenderMode::Auto, K::RenderAuto),
                    (RenderMode::Compatibility, K::RenderCompat),
                    (RenderMode::Software, K::RenderSoftware),
                ] {
                    if ui
                        .selectable_label(app.shared.settings.render_mode == mode, i18n::tr(key))
                        .clicked()
                    {
                        app.shared.settings.render_mode = mode;
                        app.shared.settings.save();
                    }
                }
            });
            ui.label(
                egui::RichText::new(i18n::tr(K::RenderModeHint))
                    .size(11.0)
                    .color(_pal.text_dim),
            );
            if app.shared.settings.render_mode == RenderMode::Software {
                // Informational, not a warning: this used to select WARP, a D3D12 driver
                // emulated on the CPU at ~3 fps. It now selects a native rasterizer, so
                // the orange caution colour would be actively misleading.
                ui.label(
                    egui::RichText::new(i18n::tr(K::RenderSoftwareWarning))
                        .size(11.0)
                        .color(_pal.text_dim),
                );
            }
            if app.shared.settings.render_mode != crate::active_render_mode() {
                ui.label(
                    egui::RichText::new(i18n::tr(K::RestartRequired))
                        .size(11.0)
                        .color(_pal.text_dim),
                );
            }

            ui.add_space(10.0);
            let mut on_top = app.shared.settings.always_on_top;
            if crate::widgets::controls::checkbox(ui, &mut on_top, i18n::tr(K::AlwaysOnTop), _pal)
                .changed()
            {
                app.shared.settings.always_on_top = on_top;
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if on_top {
                    egui::WindowLevel::AlwaysOnTop
                } else {
                    egui::WindowLevel::Normal
                }));
                app.shared.settings.save();
            }

            let mut autosave = app.shared.settings.save_config;
            if crate::widgets::controls::checkbox(
                ui,
                &mut autosave,
                i18n::tr(K::SaveConfigAuto),
                _pal,
            )
            .changed()
            {
                app.shared.settings.save_config = autosave;
                app.save_settings_forced();
            }

            let mut remember = app.shared.settings.remember_window;
            if crate::widgets::controls::checkbox(
                ui,
                &mut remember,
                i18n::tr(K::RememberWindow),
                _pal,
            )
            .changed()
            {
                app.shared.settings.remember_window = remember;
                app.shared.settings.save();
            }

            #[cfg(target_os = "windows")]
            {
                let mut close_to_tray = app.shared.settings.close_to_tray;
                if crate::widgets::controls::checkbox(
                    ui,
                    &mut close_to_tray,
                    i18n::tr(K::CloseToTray),
                    _pal,
                )
                .changed()
                {
                    app.shared.settings.close_to_tray = close_to_tray;
                    app.save_settings();
                }

                let mut start_with_windows = app.shared.settings.start_with_windows;
                if crate::widgets::controls::checkbox(
                    ui,
                    &mut start_with_windows,
                    i18n::tr(K::StartWithWindows),
                    _pal,
                )
                .changed()
                {
                    match app
                        .actions
                        .set_start_with_windows(start_with_windows, true)
                    {
                        Ok(()) => {
                            app.shared.settings.start_with_windows = start_with_windows;
                            app.save_settings();
                        }
                        Err(error) => app.shared.toast(i18n::trf(
                            K::ErrMsg,
                            &[&error.to_string()],
                        )),
                    }
                }
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

            #[cfg(target_os = "windows")]
            {
                use tm_platform::actions::{CoreServiceState, TaskManagerReplacementState};
                ui.add_space(14.0);
                ui.heading("Advanced");

                ui.heading(i18n::tr(K::CoreServiceHeading));
                app.poll_advanced_state(ctx);
                let core_state = app.core_service_state.clone();
                let state_text = match core_state.as_ref() {
                    None => i18n::tr(K::CheckingAdvancedState).into(),
                    Some(CoreServiceState::Unsupported) => {
                        i18n::tr(K::CoreServiceNotInstalled).into()
                    }
                    Some(CoreServiceState::NotInstalled) => {
                        i18n::tr(K::CoreServiceNotInstalled).into()
                    }
                    Some(CoreServiceState::Stopped) => i18n::tr(K::CoreServiceStopped).into(),
                    Some(CoreServiceState::Starting) => i18n::tr(K::CoreServiceStarting).into(),
                    Some(CoreServiceState::Running { version }) => {
                        i18n::trf(K::CoreServiceRunning, &[version])
                    }
                    Some(CoreServiceState::ForeignClient) => {
                        i18n::tr(K::CoreServiceForeignClient).into()
                    }
                    Some(CoreServiceState::Degraded(detail)) => {
                        i18n::trf(K::CoreServiceDegraded, &[detail])
                    }
                };
                ui.label(
                    egui::RichText::new(state_text)
                        .size(11.5)
                        .color(_pal.text_dim),
                );
                let install = matches!(
                    core_state,
                    Some(
                        CoreServiceState::NotInstalled
                            | CoreServiceState::Stopped
                            | CoreServiceState::Degraded(_)
                    )
                );
                let supported = core_state
                    .as_ref()
                    .is_some_and(|state| {
                        !matches!(
                            state,
                            CoreServiceState::Unsupported | CoreServiceState::Starting
                        )
                    })
                    && !app
                        .core_service_change_inflight
                        .load(std::sync::atomic::Ordering::Acquire);
                let button_key = match core_state.as_ref() {
                    Some(CoreServiceState::NotInstalled) => K::InstallCoreService,
                    Some(CoreServiceState::Stopped | CoreServiceState::Degraded(_)) => {
                        K::RepairCoreService
                    }
                    Some(CoreServiceState::ForeignClient) => K::SwitchToInstalledCoreService,
                    _ => K::RemoveCoreService,
                };
                let foreign = matches!(core_state.as_ref(), Some(CoreServiceState::ForeignClient));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(supported, egui::Button::new(i18n::tr(button_key)))
                        .clicked()
                    {
                        if foreign {
                            // No reinstall can make a foreign image pass the
                            // broker's client authorization; hand the session
                            // to the installed GUI instead.
                            dispatch_core_service_switch(app, ctx);
                        } else {
                            dispatch_core_service_change(app, ctx, install);
                        }
                    }
                    if foreign {
                        // Repair stays reachable: it is how a newer
                        // portable/dev build upgrades the protected generation
                        // before switching.
                        if ui
                            .add_enabled(
                                supported,
                                egui::Button::new(i18n::tr(K::RepairCoreService)),
                            )
                            .clicked()
                        {
                            dispatch_core_service_repair_and_switch(app, ctx);
                        }
                    }
                });

                ui.add_space(10.0);
                if let Some(state) = app.task_manager_replacement_state.clone() {
                    let mut replace = matches!(
                        state,
                        TaskManagerReplacementState::Enabled
                            | TaskManagerReplacementState::Stale(_)
                    );
                    if crate::widgets::controls::checkbox(
                        ui,
                        &mut replace,
                        "Replace Windows Task Manager",
                        _pal,
                    )
                    .changed()
                    {
                        let actions = app.actions.clone();
                        app.run_action(
                            ctx,
                            || "Task Manager integration requested".to_string(),
                            move || actions.set_task_manager_replacement(replace),
                        );
                    }
                    match state {
                        TaskManagerReplacementState::Stale(_) => {
                            ui.label(
                                egui::RichText::new(
                                    "The registered Taskman path is stale. Toggle off/on to repair it.",
                                )
                                .size(11.5)
                                .color(_pal.text_dim),
                            );
                        }
                        TaskManagerReplacementState::Conflict(value) => {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Another application currently replaces Task Manager: {value}"
                                ))
                                .size(11.5)
                                .color(_pal.text_dim),
                            );
                        }
                        _ => {}
                    }
                } else {
                    ui.label(i18n::tr(K::CheckingAdvancedState));
                }

                ui.add_space(10.0);
                ui.heading(i18n::tr(K::ElevatedHeading));
                ui.label(
                    egui::RichText::new(if app.is_elevated {
                        i18n::tr(K::ElevatedRunning)
                    } else {
                        i18n::tr(K::ElevatedNotRunning)
                    })
                    .size(11.5)
                    .color(_pal.text_dim),
                );
                if !app.is_elevated && ui.button(i18n::tr(K::RestartElevated)).clicked() {
                    let actions = app.actions.clone();
                    let close_ctx = ctx.clone();
                    app.run_action(
                        ctx,
                        || i18n::tr(K::RelaunchElevatedToast).to_string(),
                        move || {
                            actions.relaunch_elevated()?;
                            // ShellExecuteExW returns only after UAC consent
                            // succeeded and the elevated instance is spawning;
                            // shut this one down gracefully so on_exit flushes
                            // settings and history. A declined prompt surfaces
                            // as an error toast instead.
                            crate::request_programmatic_exit();
                            close_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            Ok(())
                        },
                    );
                }
                let mut start_elevated = app.shared.settings.start_elevated;
                if crate::widgets::controls::checkbox(
                    ui,
                    &mut start_elevated,
                    i18n::tr(K::StartElevated),
                    _pal,
                )
                .changed()
                {
                    // Policy for FUTURE launches: startup re-execs elevated
                    // when unelevated (main.rs); the current session is not
                    // touched — use the restart button above to elevate now.
                    app.shared.settings.start_elevated = start_elevated;
                    app.shared.settings.save();
                }
            }

            ui.add_space(14.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::ResetColWidths)).clicked() {
                    app.shared.settings.col_widths.clear();
                    app.save_settings();
                    app.shared.toast(i18n::tr(K::ColWidthsResetToast));
                }
                if ui.button(i18n::tr(K::Reset)).clicked() {
                    let mut defaults = Settings::default();
                    #[cfg(target_os = "windows")]
                    if let Err(error) = app.actions.set_start_with_windows(false, true)
                    {
                        // The registry is authoritative for autostart. If it
                        // could not be cleared, keep the matching setting and
                        // surface the mismatch instead of claiming a reset.
                        defaults.start_with_windows = app.shared.settings.start_with_windows;
                        app.shared
                            .toast(i18n::trf(K::ErrMsg, &[&error.to_string()]));
                    }
                    apply_theme(ctx, defaults.theme);
                    theme::set_text_smoothing(defaults.text_smoothing);
                    theme::refresh_text_rendering(ctx);
                    crate::fonts::reapply(ctx);
                    ctx.set_zoom_factor(defaults.ui_zoom);
                    app.engine.resume();
                    app.engine.set_interval(defaults.update_speed.interval());
                    i18n::set_lang(defaults.language.resolve());
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(
                        i18n::tr(K::WindowTitle).to_string(),
                    ));
                    let keep_autosave = app.shared.settings.save_config;
                    app.shared.settings = defaults;
                    app.shared.settings.save_config = keep_autosave;
                    app.save_settings_forced();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(i18n::tr(K::Close)).clicked() {
                        app.show_settings = false;
                        app.save_settings_forced();
                    }
                });
            });
        });
    if !open {
        app.show_settings = false;
    }
}

/// Confirmation for the Delete-key shortcut and for any termination that
/// covers more than one selected row. A single-row toolbar or context-menu
/// termination retains its native one-click behavior.
pub fn process_end_dialog(app: &mut TaskManApp, ctx: &egui::Context) {
    let Some(pending) = app.pending_process_end.clone() else {
        return;
    };
    let mut open = true;
    let mut decision = ctx.input(|input| {
        if input.key_pressed(egui::Key::Escape) {
            Some(false)
        } else if input.key_pressed(egui::Key::Enter) {
            Some(true)
        } else {
            None
        }
    });
    egui::Window::new(i18n::tr(K::EndTask))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ctx, |ui| {
            ui.set_width(430.0);
            match pending.targets.as_slice() {
                [(identity, name)] => {
                    ui.label(i18n::trf(
                        K::EndProcessConfirm,
                        &[name, &identity.pid.to_string()],
                    ));
                }
                targets => {
                    ui.label(i18n::trf(
                        K::EndProcessesConfirm,
                        &[&targets.len().to_string()],
                    ));
                    ui.add_space(6.0);
                    // Name every target: "end 27 processes" is not informed
                    // consent, and the list is the only place the user can
                    // see what the range selection actually caught.
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .show(ui, |ui| {
                            for (identity, name) in targets {
                                ui.label(format!("{name}  ({})", identity.pid));
                            }
                        });
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::Cancel)).clicked() {
                    decision = Some(false);
                }
                if ui.button(i18n::tr(K::EndTask)).clicked() {
                    decision = Some(true);
                }
            });
        });
    if !open {
        decision = Some(false);
    }
    if let Some(confirm) = decision {
        app.pending_process_end = None;
        if confirm {
            app.end_process_batch(ctx, pending.targets, pending.tree);
        }
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
            crate::widgets::controls::checkbox(
                ui,
                &mut app.run_elevated,
                i18n::tr(K::RunElevated),
                _pal,
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::Cancel)).clicked() {
                    app.run_dialog_open = false;
                }
                if ui.button(i18n::tr(K::Browse)).clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_file()
                {
                    app.run_dialog_text = path.to_string_lossy().into_owned();
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button(i18n::tr(K::Ok)).clicked() || enter)
                    && !app.run_dialog_text.trim().is_empty()
                {
                    let actions = app.actions.clone();
                    let cmdline = app.run_dialog_text.trim().to_string();
                    let elevated = app.run_elevated;
                    let toasts = app.shared.toasts.clone();
                    let spawned =
                        std::thread::Builder::new()
                            .name("tm-run".into())
                            .spawn(move || {
                                let result = actions.run_new_task_probe(&cmdline, elevated);
                                let msg = match result {
                                    Ok(()) => i18n::trf(K::StartedMsg, &[&cmdline]),
                                    Err(e) => i18n::trf(K::ErrMsg, &[&e.to_string()]),
                                };
                                crate::app::toast_from(&toasts, msg);
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

/// Dispatch the core-service install/remove change on an action lane. The
/// inflight flag disables the buttons until the operation completes.
fn dispatch_core_service_change(app: &mut TaskManApp, ctx: &egui::Context, install: bool) {
    let actions = app.actions.clone();
    let inflight = app.core_service_change_inflight.clone();
    inflight.store(true, std::sync::atomic::Ordering::Release);
    let completion = inflight.clone();
    let dispatched = app.run_action(
        ctx,
        move || {
            i18n::tr(if install {
                K::CoreServiceInstallRequested
            } else {
                K::CoreServiceRemoveRequested
            })
            .to_string()
        },
        move || {
            let outcome = actions.set_core_service_installed(install);
            completion.store(false, std::sync::atomic::Ordering::Release);
            outcome
        },
    );
    if !dispatched {
        inflight.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Dispatch a repair from a foreign session: install this build as the
/// protected generation, then hand the session over to the installed copy —
/// the running session's image path stays rejected until it switches, so a
/// bare repair would leave the user in the same "not the installed client"
/// state they tried to leave.
fn dispatch_core_service_repair_and_switch(app: &mut TaskManApp, ctx: &egui::Context) {
    let actions = app.actions.clone();
    let inflight = app.core_service_change_inflight.clone();
    inflight.store(true, std::sync::atomic::Ordering::Release);
    let completion = inflight.clone();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let close_ctx = ctx.clone();
    let dispatched = app.run_action(
        ctx,
        || i18n::tr(K::CoreServiceRepairSwitchRequested).to_string(),
        move || {
            actions.set_core_service_installed(true)?;
            if actions.switch_to_installed_gui(&args)? {
                crate::request_programmatic_exit();
                close_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            completion.store(false, std::sync::atomic::Ordering::Release);
            Ok(())
        },
    );
    if !dispatched {
        inflight.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Dispatch the handover to the protected installed GUI. Shutting down
/// gracefully lets on_exit flush settings and history while the installed
/// replacement waits on the single-instance handoff.
fn dispatch_core_service_switch(app: &mut TaskManApp, ctx: &egui::Context) {
    let actions = app.actions.clone();
    let inflight = app.core_service_change_inflight.clone();
    inflight.store(true, std::sync::atomic::Ordering::Release);
    let completion = inflight.clone();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let close_ctx = ctx.clone();
    let dispatched = app.run_action(
        ctx,
        || i18n::tr(K::CoreServiceSwitchRequested).to_string(),
        move || {
            let switched = actions.switch_to_installed_gui(&args)?;
            completion.store(false, std::sync::atomic::Ordering::Release);
            if switched {
                crate::request_programmatic_exit();
                close_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Ok(())
        },
    );
    if !dispatched {
        inflight.store(false, std::sync::atomic::Ordering::Release);
    }
}

pub fn draw_toasts(app: &TaskManApp, ctx: &egui::Context) {
    let mut toasts = tm_core::sync::lock(&app.shared.toasts);
    toasts.retain(|t| t.born.elapsed() < crate::app::TOAST_TTL);
    let mut y_offset = 0.0f32;
    for toast in toasts.iter() {
        let age = toast.born.elapsed().as_secs_f32();
        let alpha = (((4.0f32 - age) * 255.0).clamp(90.0, 255.0)) as u8;
        let id = egui::Id::new(("toast", toast.id));
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
                            egui::RichText::new(&toast.msg)
                                .size(13.0)
                                .color(Color32::from_white_alpha(alpha)),
                        );
                    });
            });
        y_offset += 46.0;
    }
}

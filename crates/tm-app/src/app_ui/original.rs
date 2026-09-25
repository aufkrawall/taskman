//! UI chrome of the root app: top search bar, navigation rail (hamburger
//! collapsible), per-tab command header, dialogs, toasts. Fully localized
//! (DE/EN) via tm-core::i18n.

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke};
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
    // The Performance page has nothing to search (its cards navigate by
    // type-ahead), and a permanently visible but dead control reads as a bug.
    // The strip itself stays on every tab: it keeps the sidebar inset and the
    // extra native-title-bar drag regions identical, so the chrome does not
    // jump when switching tabs.
    let searchable = app.tab != crate::app::Tab::Performance;
    egui::Panel::top(egui::Id::new("topsearch"))
        .resizable(false)
        .frame(
            egui::Frame::NONE
                .fill(pal.window_bg)
                .inner_margin(egui::Margin::symmetric(0, 6)),
        )
        .show(ui_root, |ui| {
            let box_w = if searchable {
                495.0f32.min(ui.available_width() * 0.7)
            } else {
                0.0
            };
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

            if !searchable {
                return;
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
            // Tab or Down Arrow jumps directly to the process / table list:
            // commits the search match, surrenders keyboard focus from the
            // chrome, and leaves the table row selection active and visible.
            // Intercepting BEFORE edit_ui.add prevents TextEdit from triggering
            // egui's Tab traversal to the sidebar.
            let search_id = egui::Id::new("global-search");
            commit_search_on_tab_or_down(ui.ctx(), search_id, app.modal_open(), || {
                app.commit_search_selection(ui.ctx());
            });
            let edit = edit_ui.add(
                egui::TextEdit::singleline(&mut app.search)
                    .hint_text(i18n::tr(K::SearchHint))
                    .font(FontId::proportional(15.0))
                    .frame(egui::Frame::NONE)
                    .desired_width(edit_rect.width())
                    .id(search_id),
            );

            // Keyboard-focus ring for the frame-less text edit: egui paints
            // no focus outline of its own here, so the accent stroke goes
            // over the card's neutral one while the field owns the keyboard.
            // Painting only — no layout, no hover change.
            if edit.has_focus() {
                focus_ring(ui, box_rect, 16.0, pal);
            }

            // Enter commits the search: the page's selection lands on the
            // match (kept as-is when it already is one), and the field gives
            // the keyboard back — egui's singleline text edit surrenders
            // focus on Enter by itself — so the next Arrow key moves the
            // table selection instead of the caret.
            commit_search_on_enter(ui.ctx(), edit.lost_focus(), app.modal_open(), || {
                app.commit_search_selection(ui.ctx());
            });

            // A click anywhere outside the field hands the keyboard back:
            // the user taps a row and the next Arrow key must move the
            // selection, not the caret. egui already surrenders focus on
            // clicks over other widgets (`InputOptions::surrender_focus_on`
            // defaults to `Clicks`); this catches the remaining dead-space
            // clicks where no widget is interacted at all.
            let outside_click = ui.input(|i| {
                i.pointer
                    .any_click()
                    .then(|| i.pointer.interact_pos())
                    .flatten()
            });
            if click_surrenders_search(edit.has_focus(), box_rect, outside_click) {
                edit.surrender_focus();
            }

            if has_text {
                let resp = ui
                    .interact(
                        clear_rect,
                        egui::Id::new("global-search-clear"),
                        Sense::CLICK,
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
                    // A cleared field has nothing left to edit; never strand
                    // the keyboard on the (about to disappear) button.
                    resp.ctx
                        .memory_mut(|mem| mem.surrender_focus(egui::Id::new("global-search")));
                }
            }

            // Escape clears the search instead of only unfocusing it: a
            // stale filter is the one thing a user cannot see the cause of.
            // egui core drops widget focus on Escape before the widgets of a
            // frame run, so this fires with the field already unfocused; the
            // gates keep the keystroke owned by dialogs, menus and the F1
            // help overlay, which all handle Escape themselves.
            if ui.input(|i| i.key_pressed(egui::Key::Escape))
                && !app.modal_open()
                && !app.show_help
                && !egui::Popup::is_any_open(ui.ctx())
                && !app.search.is_empty()
            {
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

    // Click+drag WITHOUT the focusable bit: `Sense::click_and_drag()` also
    // sets `FOCUSABLE`, which put invisible dead Tab stops on the empty
    // titlebar strips. The raw CLICK|DRAG union senses the same pointer
    // gestures (press-to-move, double-click maximize) but is never focused.
    let resp = ui.interact(rect, id, Sense::CLICK | Sense::DRAG);
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

/// Keyboard-focus ring for the custom shell widgets (nav entries, command
/// buttons, the search box and its clear button, the toast close button).
/// egui only paints focus for its own styled widgets; these allocate raw
/// responses, so the ring is painted by hand — the same accent stroke the
/// dialogs use on their focused buttons. Painting only: no layout, and the
/// selected/hovered visuals stay untouched.
fn focus_ring(ui: &egui::Ui, rect: Rect, corner_radius: f32, pal: &Palette) {
    ui.painter().rect_stroke(
        rect,
        corner_radius,
        Stroke::new(1.5, pal.accent),
        egui::StrokeKind::Inside,
    );
}

/// Whether a pointer click should take the keyboard focus away from the
/// global search field: any click that did not land inside the field box
/// (the clear button sits inside it and handles itself). Pure decision, so
/// the focus contract is testable without a window.
fn click_surrenders_search(field_has_focus: bool, field_rect: Rect, click: Option<Pos2>) -> bool {
    field_has_focus && click.is_some_and(|pos| !field_rect.contains(pos))
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
                app.save_settings();
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
    // Keyboard-focus ring, painted last so it sits on top of the fill.
    if resp.has_focus() {
        focus_ring(ui, rect, 4.0, pal);
        let drop_to_content = ui.input(|i| {
            (i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::Escape))
                && !i.modifiers.shift
                && !i.modifiers.ctrl
                && !i.modifiers.alt
        });
        if drop_to_content {
            resp.surrender_focus();
            ui.ctx().memory_mut(|m| {
                m.surrender_focus(resp.id);
                m.move_focus(egui::FocusDirection::None);
            });
            ui.input_mut(|i| {
                i.consume_key(Default::default(), egui::Key::ArrowRight);
                i.consume_key(Default::default(), egui::Key::Escape);
            });
        }
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
    // The hover highlight (and the focus ring) wrap the icon square when the
    // hamburger button spans the collapsed sidebar's full width.
    let badge_rect = if center {
        Rect::from_center_size(rect.center(), egui::vec2(size, size))
    } else {
        rect
    };
    if resp.hovered() {
        ui.painter()
            .rect_filled(badge_rect, 4.0, Color32::from_white_alpha(12));
    }
    if resp.has_focus() {
        focus_ring(ui, badge_rect, 4.0, pal);
        let drop_to_content = ui.input(|i| {
            (i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::Escape))
                && !i.modifiers.shift
                && !i.modifiers.ctrl
                && !i.modifiers.alt
        });
        if drop_to_content {
            resp.surrender_focus();
            ui.ctx().memory_mut(|m| {
                m.surrender_focus(resp.id);
                m.move_focus(egui::FocusDirection::None);
            });
            ui.input_mut(|i| {
                i.consume_key(Default::default(), egui::Key::ArrowRight);
                i.consume_key(Default::default(), egui::Key::Escape);
            });
        }
    }
    if icon == Icon::Hamburger {
        let first_id_key = egui::Id::new("tm-first-sidebar-item");
        if ui
            .ctx()
            .data(|d| d.get_temp::<Option<egui::Id>>(first_id_key))
            .flatten()
            .is_none()
        {
            ui.ctx()
                .data_mut(|d| d.insert_temp(first_id_key, Some(resp.id)));
        }
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
    // Disabled buttons leave the Tab order entirely: with the click sense
    // they would be focusable dead stops the keyboard has to walk around on
    // every table header. Hover-only sensing keeps the pointer behavior; the
    // disabled look is painted below either way.
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let id = ui.id().with(("cmd-button", label));
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 30.0), Sense::hover());
    let resp = ui.interact(rect, id, sense);
    if enabled {
        let first_id_key = egui::Id::new("tm-first-toolbar-button");
        if ui
            .ctx()
            .data(|d| d.get_temp::<Option<egui::Id>>(first_id_key))
            .flatten()
            .is_none()
        {
            ui.ctx().data_mut(|d| d.insert_temp(first_id_key, Some(id)));
        }
    }
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
    if resp.has_focus() {
        focus_ring(ui, rect, 4.0, pal);
        let drop_down = ui.input(|i| {
            (i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::Escape))
                && !i.modifiers.shift
                && !i.modifiers.ctrl
                && !i.modifiers.alt
        });
        if drop_down {
            resp.surrender_focus();
            ui.ctx().memory_mut(|m| {
                m.surrender_focus(resp.id);
                m.move_focus(egui::FocusDirection::None);
            });
            ui.input_mut(|i| {
                i.consume_key(Default::default(), egui::Key::ArrowDown);
                i.consume_key(Default::default(), egui::Key::Escape);
            });
        }
    }
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

/// Commit the global search on Enter and CONSUME the keystroke: exactly one
/// handler may win the frame. The singleline text edit surrenders focus
/// during its own pass, so later in the SAME frame the tables' row-action
/// gate — which yields to nothing focused and no popup — sees the same Enter
/// still queued and would fire the row action (e.g. Processes
/// "Go to details") on top of the commit. Consuming here is what keeps the
/// commit keystroke single-purpose. While a dialog is up the commit stands
/// down and the keystroke stays queued for the dialog's own contract.
/// Split out from the search panel so the contract is testable headlessly.
fn commit_search_on_enter(
    ctx: &egui::Context,
    edit_lost_focus: bool,
    dialog_open: bool,
    commit: impl FnOnce(),
) -> bool {
    if !(edit_lost_focus && ctx.input(|i| i.key_pressed(egui::Key::Enter)) && !dialog_open) {
        return false;
    }
    commit();
    ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::Enter));
    true
}

/// Tab or Down Arrow while the search field is focused (or just lost focus)
/// surrenders keyboard focus from chrome, drops directly into table rows,
/// and commits the search selection.
/// Split out from the search panel so the contract is testable headlessly.
fn commit_search_on_tab_or_down(
    ctx: &egui::Context,
    search_id: egui::Id,
    dialog_open: bool,
    commit: impl FnOnce(),
) -> bool {
    let is_focused = ctx.memory(|m| m.has_focus(search_id));
    if dialog_open || !is_focused {
        return false;
    }
    let tab = ctx.input(|i| {
        i.key_pressed(egui::Key::Tab) && !i.modifiers.shift && !i.modifiers.ctrl && !i.modifiers.alt
    });
    let down = ctx.input(|i| {
        i.key_pressed(egui::Key::ArrowDown)
            && !i.modifiers.shift
            && !i.modifiers.ctrl
            && !i.modifiers.alt
    });
    if !(tab || down) {
        return false;
    }
    ctx.input_mut(|i| {
        i.consume_key(Default::default(), egui::Key::Tab);
        i.consume_key(Default::default(), egui::Key::ArrowDown);
    });
    ctx.memory_mut(|mem| {
        mem.surrender_focus(egui::Id::new("global-search"));
        mem.move_focus(egui::FocusDirection::None);
    });
    if let Some(held) = ctx.memory(|m| m.focused()) {
        ctx.memory_mut(|m| m.surrender_focus(held));
    }
    commit();
    true
}

// ---------------------------------------------------------------- dialogs

pub fn settings_dialog(app: &mut TaskManApp, ctx: &egui::Context, _pal: &theme::Palette) {
    let mut open = true;
    // A tab-through dialog: the controls must stay keyboard-reachable, so Tab
    // is left to egui's focus system and Enter only closes while no control
    // holds focus (a focused control activates natively instead). Esc always
    // closes. Keys are consumed before the window so egui's built-in
    // focused-button activation cannot double-fire; Space stays ordinary text
    // so the keyboard still toggles focused checkboxes via egui.
    let restore_id = egui::Id::new("settings-dialog-restore-focus");
    let first_frame = ctx
        .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
        .is_none();
    if first_frame {
        // Remember what held focus before the dialog took over, so closing
        // can hand it back.
        let captured = capture_focus(ctx);
        ctx.data_mut(|d| d.insert_temp(restore_id, captured));
    }
    let keys = consume_dialog_keys_tab_through(ctx, false);
    let close_now = keys.escape || keys.enter;
    let mut close_clicked = false;
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
            let mut anchor: Option<egui::Response> = None;
            ui.horizontal(|ui| {
                for (mode, key) in [
                    (ThemeMode::System, K::ThemeSystem),
                    (ThemeMode::Light, K::ThemeLight),
                    (ThemeMode::Dark, K::ThemeDark),
                ] {
                    let choice =
                        ui.selectable_label(app.shared.settings.theme == mode, i18n::tr(key));
                    if anchor.is_none() {
                        anchor = Some(choice.clone());
                    }
                    if choice.clicked() {
                        app.shared.settings.theme = mode;
                        apply_theme(ctx, mode);
                        app.save_settings();
                    }
                }
            });
            // The first interactive control is the keyboard anchor of the
            // dialog: requested once on the opening frame, from where egui's
            // own Tab traversal takes over.
            if first_frame && let Some(anchor) = anchor.filter(|resp| resp.enabled()) {
                anchor.request_focus();
            }

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
                        app.save_settings();
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
                        app.save_settings();
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
                        .selectable_label(app.shared.settings.text_smoothing == mode, i18n::tr(key))
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
                        app.save_settings();
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
                app.save_settings();
            }
            if app.shared.settings.always_on_top != app.startup_always_on_top {
                // The Start-menu-proof band is chosen when the window is
                // created (and a band-16 window cannot be demoted), so the
                // live toggle is only a best effort until the next start.
                ui.label(
                    egui::RichText::new(i18n::tr(K::RestartRequired))
                        .size(11.0)
                        .color(_pal.text_dim),
                );
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
                app.save_settings();
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
                    match app.actions.set_start_with_windows(start_with_windows, true) {
                        Ok(()) => {
                            app.shared.settings.start_with_windows = start_with_windows;
                            app.save_settings();
                        }
                        Err(error) => app
                            .shared
                            .toast(i18n::trf(K::ErrMsg, &[&error.to_string()])),
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
                        app.save_settings();
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
                        app.save_settings();
                    }
                }
            });

            #[cfg(target_os = "windows")]
            {
                use tm_platform::actions::{CoreServiceState, TaskManagerReplacementState};
                ui.add_space(14.0);
                ui.heading(i18n::tr(K::AdvancedHeading));

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
                let supported = core_state.as_ref().is_some_and(|state| {
                    !matches!(
                        state,
                        CoreServiceState::Unsupported | CoreServiceState::Starting
                    )
                }) && !app
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
                            crate::action_executor::dispatch_core_service_switch(app, ctx);
                        } else {
                            crate::action_executor::dispatch_core_service_change(app, ctx, install);
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
                            crate::action_executor::dispatch_core_service_repair_and_switch(
                                app, ctx,
                            );
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
                        i18n::tr(K::ReplaceTaskManager),
                        _pal,
                    )
                    .changed()
                    {
                        let actions = app.actions.clone();
                        app.run_action(
                            ctx,
                            || i18n::tr(K::TmIntegrationRequested).to_string(),
                            move || actions.set_task_manager_replacement(replace),
                        );
                    }
                    match state {
                        TaskManagerReplacementState::Stale(value) => {
                            // A registered path that no longer exists is not
                            // a mismatch to live with: Windows cannot launch
                            // it, so the hotkey opens nothing at all — the
                            // built-in Task Manager included.
                            let missing = tm_platform::win::replacement_target_missing(&value);
                            ui.label(
                                egui::RichText::new(if missing {
                                    i18n::tr(K::TmRegistrationMissing)
                                } else {
                                    i18n::tr(K::TmRegistrationForeign)
                                })
                                .size(11.5)
                                .color(_pal.text_dim),
                            );
                            if ui.button(i18n::tr(K::TmRepairButton)).clicked() {
                                let actions = app.actions.clone();
                                app.run_action(
                                    ctx,
                                    || i18n::tr(K::TmIntegrationRequested).to_string(),
                                    move || actions.set_task_manager_replacement(true),
                                );
                            }
                        }
                        TaskManagerReplacementState::Conflict(value) => {
                            ui.label(
                                egui::RichText::new(i18n::trf(K::TmReplacedByOther, &[&value]))
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
                    app.save_settings();
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
                    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
                    let mut defaults = Settings::default();
                    #[cfg(target_os = "windows")]
                    if let Err(error) = app.actions.set_start_with_windows(false, true) {
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
                        close_clicked = true;
                    }
                });
            });

            // Keyboard focus just landed on a control: bring it into view.
            // One read at the end of the content pass covers every control
            // above, and the scroll target is consumed by the vertical scroll
            // area that wraps this content.
            if let Some(id) = ctx.memory(|mem| mem.focused())
                && let Some(focus_resp) = ctx.read_response(id)
                && focus_resp.gained_focus()
            {
                focus_resp.scroll_to_me(None);
            }
        });
    if !open || close_now || close_clicked {
        let saved = ctx
            .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
            .flatten();
        restore_focus(ctx, saved);
        ctx.data_mut(|d| d.remove_temp::<Option<egui::Id>>(restore_id));
        app.show_settings = false;
        if close_now || close_clicked {
            app.save_settings_forced();
        }
    }
}

/// Confirmation for the Delete-key shortcut and for any termination that
/// covers more than one selected row. A single-row toolbar or context-menu
/// termination retains its native one-click behavior.
pub fn process_end_dialog(app: &mut TaskManApp, ctx: &egui::Context) {
    let Some(pending) = app.pending_process_end.clone() else {
        return;
    };
    const END_TASK_LIST_MAX_HEIGHT: f32 = 168.0;
    let dialog_height = if pending.targets.len() <= 1 {
        124.0
    } else {
        123.0 + (pending.targets.len() as f32 * 21.0).min(END_TASK_LIST_MAX_HEIGHT)
    };
    let mut open = true;
    let focus_id = egui::Id::new("end_task_dialog_focus_end");
    let restore_id = egui::Id::new("end-task-dialog-restore-focus");
    // The SAFE action owns the default: focus starts on Cancel, so Enter and
    // Space confirm nothing until the user deliberately moves focus to the
    // End task button. The dialog exists to guard accidental kills; its
    // keyboard default must not undo that guard.
    let mut end_focused: bool = ctx.data(|d| d.get_temp(focus_id)).unwrap_or(false);
    // First frame of this dialog session (the focus flag above only exists
    // while it stays open): remember what held focus before the dialog.
    if ctx.data(|d| d.get_temp::<bool>(focus_id)).is_none() {
        let captured = capture_focus(ctx);
        ctx.data_mut(|d| d.insert_temp(restore_id, captured));
    }

    let keys = consume_dialog_keys(ctx, true);
    end_focused =
        update_end_task_dialog_focus(end_focused, keys.tab, keys.shift_tab, keys.left, keys.right);

    let mut decision =
        dialog_key_decision(keys, end_focused, true).map(|d| matches!(d, DialogDecision::Primary));

    let pal = crate::theme::palette_ctx(ctx);
    egui::Window::new(i18n::tr(K::EndTask))
        .id(egui::Id::new("end-task-dialog"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .fixed_size([420.0, dialog_height])
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ctx, |ui| {
            ui.set_width(400.0);
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
                    ui.add_space(8.0);
                    let box_stroke = egui::Stroke::new(
                        1.0,
                        if ui.visuals().dark_mode {
                            Color32::from_rgb(0x3e, 0x3e, 0x3e)
                        } else {
                            Color32::from_rgb(0xd8, 0xd8, 0xd8)
                        },
                    );
                    let box_bg = if ui.visuals().dark_mode {
                        Color32::from_rgb(0x1f, 0x1f, 0x1f)
                    } else {
                        Color32::from_rgb(0xf5, 0xf5, 0xf5)
                    };
                    egui::Frame::NONE
                        .fill(box_bg)
                        .stroke(box_stroke)
                        .corner_radius(CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(END_TASK_LIST_MAX_HEIGHT)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing.y = 3.0;
                                    for (identity, name) in targets {
                                        ui.horizontal(|ui| {
                                            ui.label(name);
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.label(
                                                        egui::RichText::new(format!(
                                                            "PID {}",
                                                            identity.pid
                                                        ))
                                                        .color(pal.text_dim),
                                                    );
                                                },
                                            );
                                        });
                                    }
                                });
                        });
                }
            }
            ui.add_space(12.0);

            match dialog_button_row(
                ui,
                ctx,
                &pal,
                i18n::tr(K::Cancel),
                Some((i18n::tr(K::EndTask), true)),
                &mut end_focused,
            ) {
                DialogButtonClick::Safe => decision = Some(false),
                DialogButtonClick::Primary => decision = Some(true),
                DialogButtonClick::None => {}
            }
        });

    ctx.data_mut(|d| d.insert_temp(focus_id, end_focused));

    if !open {
        decision = Some(false);
    }
    if let Some(confirm) = decision {
        let saved = ctx
            .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
            .flatten();
        restore_focus(ctx, saved);
        ctx.data_mut(|d| d.remove_temp::<Option<egui::Id>>(restore_id));
        ctx.data_mut(|d| d.remove_temp::<bool>(focus_id));
        app.pending_process_end = None;
        if confirm {
            app.end_process_batch(ctx, pending.targets, pending.tree);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RunTaskDialogFocus {
    #[default]
    Command,
    Elevated,
    Cancel,
    Browse,
    Ok,
}

impl RunTaskDialogFocus {
    fn next(self, backwards: bool) -> Self {
        use RunTaskDialogFocus::*;
        const ORDER: [RunTaskDialogFocus; 5] = [Command, Elevated, Cancel, Browse, Ok];
        let index = ORDER
            .iter()
            .position(|candidate| *candidate == self)
            .expect("known run-dialog focus target");
        let next = if backwards {
            (index + ORDER.len() - 1) % ORDER.len()
        } else {
            (index + 1) % ORDER.len()
        };
        ORDER[next]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunTaskDialogAction {
    Submit,
    Cancel,
    Browse,
    ToggleElevated,
}

/// Native-dialog keyboard semantics: Enter activates the focused push button,
/// otherwise it invokes the default OK action. Space toggles the checkbox or
/// activates a focused push button, but remains ordinary text in the command
/// field.
fn run_task_dialog_key_action(
    focus: RunTaskDialogFocus,
    enter: bool,
    space: bool,
) -> Option<RunTaskDialogAction> {
    if enter {
        return Some(match focus {
            RunTaskDialogFocus::Cancel => RunTaskDialogAction::Cancel,
            RunTaskDialogFocus::Browse => RunTaskDialogAction::Browse,
            RunTaskDialogFocus::Command | RunTaskDialogFocus::Elevated | RunTaskDialogFocus::Ok => {
                RunTaskDialogAction::Submit
            }
        });
    }
    if space {
        return match focus {
            RunTaskDialogFocus::Command => None,
            RunTaskDialogFocus::Elevated => Some(RunTaskDialogAction::ToggleElevated),
            RunTaskDialogFocus::Cancel => Some(RunTaskDialogAction::Cancel),
            RunTaskDialogFocus::Browse => Some(RunTaskDialogAction::Browse),
            RunTaskDialogFocus::Ok => Some(RunTaskDialogAction::Submit),
        };
    }
    None
}

pub fn run_task_dialog(app: &mut TaskManApp, ctx: &egui::Context, _pal: &theme::Palette) {
    let focus_id = egui::Id::new("run-task-dialog-focus");
    let restore_id = egui::Id::new("run-task-dialog-restore-focus");
    let mut focus = ctx
        .data(|data| data.get_temp::<RunTaskDialogFocus>(focus_id))
        .unwrap_or_default();
    // First frame of this dialog session: remember what held focus before
    // the dialog took over, so closing can hand it back.
    if ctx
        .data(|data| data.get_temp::<RunTaskDialogFocus>(focus_id))
        .is_none()
    {
        let captured = capture_focus(ctx);
        ctx.data_mut(|data| data.insert_temp(restore_id, captured));
    }

    // Own the dialog's focus traversal instead of requesting text focus every
    // frame. The old unconditional `request_focus()` made Tab immediately snap
    // back into the command field and made Enter depend on `lost_focus()`.
    let shift_tab =
        ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab));
    let tab = ctx.input_mut(|input| input.consume_key(Default::default(), egui::Key::Tab));
    if shift_tab || tab {
        focus = focus.next(shift_tab);
    }

    let escape = ctx.input_mut(|input| input.consume_key(Default::default(), egui::Key::Escape));
    let enter = ctx.input_mut(|input| input.consume_key(Default::default(), egui::Key::Enter));
    // Do not consume Space while editing the command line.
    let space = focus != RunTaskDialogFocus::Command
        && ctx.input_mut(|input| input.consume_key(Default::default(), egui::Key::Space));
    let mut action = if escape {
        Some(RunTaskDialogAction::Cancel)
    } else {
        run_task_dialog_key_action(focus, enter, space)
    };

    let mut open = true;
    egui::Window::new(i18n::tr(K::RunDialogTitle))
        .id(egui::Id::new("run-task-dialog"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .fixed_size([440.0, 174.0])
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(i18n::tr(K::RunPrompt));
            ui.add_space(4.0);
            let command = ui.add(
                egui::TextEdit::singleline(&mut app.run_dialog_text)
                    .hint_text(i18n::tr(K::RunHint))
                    .desired_width(f32::INFINITY),
            );
            if command.clicked() {
                focus = RunTaskDialogFocus::Command;
            }
            if focus == RunTaskDialogFocus::Command {
                command.request_focus();
            }

            ui.add_space(4.0);
            let elevated = crate::widgets::controls::checkbox(
                ui,
                &mut app.run_elevated,
                i18n::tr(K::RunElevated),
                _pal,
            );
            if elevated.clicked() {
                focus = RunTaskDialogFocus::Elevated;
            }
            if focus == RunTaskDialogFocus::Elevated {
                elevated.request_focus();
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let mut cancel_button = egui::Button::new(i18n::tr(K::Cancel));
                if focus == RunTaskDialogFocus::Cancel {
                    cancel_button = cancel_button.stroke(egui::Stroke::new(1.5, _pal.accent));
                }
                let cancel = ui.add(cancel_button);
                if cancel.clicked() {
                    focus = RunTaskDialogFocus::Cancel;
                    action = Some(RunTaskDialogAction::Cancel);
                }
                if focus == RunTaskDialogFocus::Cancel {
                    cancel.request_focus();
                }

                let mut browse_button = egui::Button::new(i18n::tr(K::Browse));
                if focus == RunTaskDialogFocus::Browse {
                    browse_button = browse_button.stroke(egui::Stroke::new(1.5, _pal.accent));
                }
                let browse = ui.add(browse_button);
                if browse.clicked() {
                    focus = RunTaskDialogFocus::Browse;
                    action = Some(RunTaskDialogAction::Browse);
                }
                if focus == RunTaskDialogFocus::Browse {
                    browse.request_focus();
                }

                let can_submit = !app.run_dialog_text.trim().is_empty();
                let mut ok_button = egui::Button::new(i18n::tr(K::Ok));
                if focus == RunTaskDialogFocus::Ok {
                    ok_button = ok_button.stroke(egui::Stroke::new(1.5, _pal.accent));
                }
                let ok = ui.add_enabled(can_submit, ok_button);
                if ok.clicked() {
                    focus = RunTaskDialogFocus::Ok;
                    action = Some(RunTaskDialogAction::Submit);
                }
                if focus == RunTaskDialogFocus::Ok {
                    ok.request_focus();
                }
            });
        });

    if !open {
        action = Some(RunTaskDialogAction::Cancel);
    }

    match action {
        Some(RunTaskDialogAction::Cancel) => {
            app.run_dialog_open = false;
        }
        Some(RunTaskDialogAction::Browse) => {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                app.run_dialog_text = path.to_string_lossy().into_owned();
                // A successful browse has completed the input step; make the
                // default action the next keyboard stop rather than reopening
                // the file picker on a second Enter.
                focus = RunTaskDialogFocus::Ok;
            }
        }
        Some(RunTaskDialogAction::ToggleElevated) => {
            app.run_elevated = !app.run_elevated;
        }
        Some(RunTaskDialogAction::Submit) => {
            if !app.run_dialog_text.trim().is_empty() {
                let actions = app.actions.clone();
                let cmdline = app.run_dialog_text.trim().to_string();
                let elevated = app.run_elevated;
                let toasts = app.shared.toasts.clone();
                let wake = ctx.clone();
                let spawned = std::thread::Builder::new()
                    .name("tm-run".into())
                    .spawn(move || {
                        let result = actions.run_new_task_probe(&cmdline, elevated);
                        let msg = match result {
                            Ok(()) => i18n::trf(K::StartedMsg, &[&cmdline]),
                            Err(error) => i18n::trf(K::ErrMsg, &[&error.to_string()]),
                        };
                        crate::app::toast_from(&toasts, msg);
                        wake.request_repaint();
                    });
                if spawned.is_err() {
                    app.shared.toast(i18n::tr(K::LaunchFailed));
                }
                app.run_dialog_open = false;
            } else {
                focus = RunTaskDialogFocus::Command;
            }
        }
        None => {}
    }

    if app.run_dialog_open {
        ctx.data_mut(|data| data.insert_temp(focus_id, focus));
    } else {
        let saved = ctx
            .data(|data| data.get_temp::<Option<egui::Id>>(restore_id))
            .flatten();
        restore_focus(ctx, saved);
        ctx.data_mut(|data| data.remove_temp::<Option<egui::Id>>(restore_id));
        ctx.data_mut(|data| data.remove_temp::<RunTaskDialogFocus>(focus_id));
    }
}

pub fn draw_toasts(app: &TaskManApp, ctx: &egui::Context) {
    draw_toasts_queue(&app.shared.toasts, ctx);
}

pub fn draw_toasts_queue(toasts_queue: &crate::app::ToastQueue, ctx: &egui::Context) {
    /// Gap between stacked toasts.
    const GAP: f32 = 8.0;
    let mut toasts = tm_core::sync::lock(toasts_queue);
    // A toast the user never attended to expires on its own; the close button
    // and "clear all" are for dismissing one sooner.
    toasts.retain(|t| t.born.elapsed() < crate::app::TOAST_TTL);
    if toasts.is_empty() {
        return;
    }

    // Stack by MEASURED heights, not a fixed step: a wrapped two-line message
    // must not overlap the toast below it. Toasts are painted oldest-first
    // from the anchored corner, so each offset is computed from the real
    // heights painted earlier in THIS frame — no lag, no estimate.
    let mut y_offset = 0.0f32;
    let mut closed_toast = None;
    for toast in toasts.iter() {
        let alpha = toast_alpha(toast.born.elapsed());
        let id = egui::Id::new(("toast", toast.id));
        let response = egui::Area::new(id)
            .anchor(Align2::RIGHT_BOTTOM, [-12.0, -12.0 - y_offset])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::window(ui.style())
                    .fill(Color32::from_black_alpha(scale_alpha(220, alpha)))
                    .stroke(Stroke::new(1.0, theme::LIGHT.stroke.gamma_multiply(alpha)))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        let text_color = Color32::from_white_alpha(scale_alpha(255, alpha));
                        let font = egui::FontId::proportional(13.0);
                        let close_btn_size = egui::vec2(18.0, 18.0);
                        let spacing = 8.0;
                        let max_text_width = 380.0 - close_btn_size.x - spacing;
                        let galley = ui.painter().layout(
                            toast.msg.clone(),
                            font,
                            text_color,
                            max_text_width,
                        );
                        let content_w = galley.size().x + spacing + close_btn_size.x;
                        ui.set_max_width(content_w);
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = spacing;
                            let (text_rect, _) =
                                ui.allocate_exact_size(galley.size(), Sense::hover());
                            ui.painter().galley(text_rect.min, galley, text_color);

                            let (btn_rect, btn_resp) =
                                ui.allocate_exact_size(close_btn_size, Sense::click());
                            let btn_resp = btn_resp.on_hover_text(i18n::tr(K::Close));
                            if btn_resp.hovered() {
                                ui.painter().rect_filled(
                                    btn_rect,
                                    3.0,
                                    Color32::from_white_alpha(scale_alpha(40, alpha)),
                                );
                            }
                            // Keyboard-focus ring. Toasts do not run through
                            // the theme palette (see the stroke below), so
                            // the ring uses the accent that reads on both
                            // themes over the black toast card.
                            if btn_resp.has_focus() {
                                ui.painter().rect_stroke(
                                    btn_rect,
                                    3.0,
                                    Stroke::new(1.5, theme::LIGHT.accent),
                                    egui::StrokeKind::Inside,
                                );
                            }
                            let icon_color = if btn_resp.hovered() {
                                text_color
                            } else {
                                Color32::from_gray(180).gamma_multiply(alpha)
                            };
                            crate::icons::draw(ui, Icon::Close, btn_rect, icon_color);
                            if btn_resp.clicked() {
                                closed_toast = Some(toast.id);
                            }
                        });
                    });
            });
        y_offset += response.response.rect.height() + GAP;
    }

    // Dismissing a stack one toast at a time is busywork, so the stack itself
    // carries the bulk action - above the newest toast, where it cannot be hit
    // while aiming for a close button.
    if toasts.len() > 1 && clear_all_button(ctx, y_offset) {
        toasts.clear();
        ctx.request_repaint();
    } else if let Some(id) = closed_toast {
        toasts.retain(|t| t.id != id);
        ctx.request_repaint();
    }
}

/// Opacity of a toast at `age`: fully opaque until [`crate::app::TOAST_FADE`]
/// is left of its lifetime, then down to zero as it expires.
fn toast_alpha(age: std::time::Duration) -> f32 {
    let left = crate::app::TOAST_TTL.saturating_sub(age);
    if left >= crate::app::TOAST_FADE {
        return 1.0;
    }
    (left.as_secs_f32() / crate::app::TOAST_FADE.as_secs_f32()).clamp(0.0, 1.0)
}

fn scale_alpha(base: u8, alpha: f32) -> u8 {
    (f32::from(base) * alpha).round().clamp(0.0, 255.0) as u8
}

/// The "clear all" affordance above the toast stack. True on the frame it is
/// clicked.
fn clear_all_button(ctx: &egui::Context, y_offset: f32) -> bool {
    egui::Area::new(egui::Id::new("toast-clear-all"))
        .anchor(Align2::RIGHT_BOTTOM, [-12.0, -12.0 - y_offset])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(Color32::from_black_alpha(220))
                .stroke(Stroke::new(1.0, theme::LIGHT.stroke))
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.add(
                        egui::Button::new(
                            egui::RichText::new(i18n::tr(K::ClearAllToasts))
                                .size(12.0)
                                .color(Color32::WHITE),
                        )
                        .frame(false),
                    )
                    .clicked()
                })
                .inner
        })
        .inner
}

// ---------------------------------------------------------------- F1 help

/// The F1 shortcut help overlay. A foreground area with nothing focusable
/// inside: the keyboard stays where it is (Tab passes straight through) and
/// Esc closes the overlay from the global shortcut block in `app.rs`.
pub fn help_overlay(ctx: &egui::Context, pal: &Palette) {
    egui::Area::new(egui::Id::new("shortcut-help"))
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::window(ui.style())
                .fill(pal.card_bg)
                .stroke(Stroke::new(1.0, pal.accent))
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.set_max_width(430.0);
                    ui.label(
                        egui::RichText::new(i18n::tr(K::HelpTitle))
                            .size(16.0)
                            .strong(),
                    );
                    ui.add_space(8.0);
                    egui::Grid::new("shortcut-help-grid")
                        .num_columns(2)
                        .spacing([18.0, 5.0])
                        .show(ui, |ui| {
                            for (combo, description) in shortcut_rows(i18n::lang()) {
                                ui.label(
                                    egui::RichText::new(combo)
                                        .size(13.0)
                                        .strong()
                                        .color(pal.accent),
                                );
                                ui.label(egui::RichText::new(description).size(13.0));
                                ui.end_row();
                            }
                        });
                });
        });
}

/// One row of the F1 overlay: the key combination as the user would say it
/// (key names follow the UI language, e.g. "Strg+F" in German) and what it
/// does. Takes the language explicitly so it stays testable without touching
/// the process-global language. The table/row gestures (arrows, Enter, Space,
/// Menu key) word what the tables implement.
fn shortcut_rows(lang: i18n::Lang) -> Vec<(String, &'static str)> {
    let tr = |key: K| i18n::tr_in(lang, key);
    let ctrl = tr(K::KeyCtrl);
    let shift = tr(K::KeyShift);
    vec![
        ("F5".to_owned(), tr(K::HelpRefresh)),
        (format!("{ctrl}+F / {}+F", tr(K::KeyAlt)), tr(K::HelpSearch)),
        (format!("{ctrl}+{}", tr(K::KeyTabKey)), tr(K::HelpNextPage)),
        (
            format!("{ctrl}+{shift}+{}", tr(K::KeyTabKey)),
            tr(K::HelpPrevPage),
        ),
        (format!("{ctrl}+1…9"), tr(K::HelpJumpPage)),
        (format!("{ctrl}+,"), tr(K::Settings)),
        (format!("{ctrl}+N"), tr(K::RunNewTask)),
        (tr(K::KeyDel).to_owned(), tr(K::HelpEndTask)),
        (
            format!("{} / {shift}+F10", tr(K::KeyMenuKey)),
            tr(K::HelpContextMenu),
        ),
        (tr(K::KeyArrows).to_owned(), tr(K::HelpMoveSelection)),
        (
            format!("{shift}+{}", tr(K::KeyArrows)),
            tr(K::HelpExtendSelection),
        ),
        (format!("{ctrl}+A"), tr(K::HelpSelectAll)),
        (tr(K::KeyPageKeys).to_owned(), tr(K::HelpPageSelection)),
        (tr(K::KeyArrows).to_owned(), tr(K::HelpResizeColumn)),
        (tr(K::KeyEnter).to_owned(), tr(K::HelpRowAction)),
        (tr(K::KeySpace).to_owned(), tr(K::HelpToggleSelect)),
        (tr(K::KeyTabKey).to_owned(), tr(K::HelpTabGeneral)),
        (tr(K::KeyEsc).to_owned(), tr(K::HelpEscGeneral)),
        ("F1".to_owned(), tr(K::HelpToggleHelp)),
    ]
}

// ---------------------------------------------------------------- dialogs

/// Keyboard events a confirmation-style dialog owns. Consume these BEFORE
/// `Window::show`, exactly like `process_end_dialog` does: egui activates a
/// focused click-sensing button on Enter/Space by itself, so consuming first
/// is what keeps the manual decision from double-firing.
#[derive(Debug, Clone, Copy, Default)]
pub struct DialogKeys {
    pub escape: bool,
    pub enter: bool,
    pub space: bool,
    pub tab: bool,
    pub shift_tab: bool,
    pub left: bool,
    pub right: bool,
}

/// Consume the dialog's keys for this frame. `space_is_action == false` keeps
/// Space as ordinary text (dialogs whose text field can hold focus).
pub fn consume_dialog_keys(ctx: &egui::Context, space_is_action: bool) -> DialogKeys {
    DialogKeys {
        escape: ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::Escape)),
        enter: ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::Enter)),
        space: space_is_action
            && ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::Space)),
        tab: ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::Tab)),
        shift_tab: ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab)),
        left: ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::ArrowLeft)),
        right: ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::ArrowRight)),
    }
}

/// Same contract as [`consume_dialog_keys`], for dialogs whose controls must
/// stay reachable with Tab (settings, inspector toolbars): Tab/Shift+Tab are
/// left for egui's focus system, which cycles the dialog's widgets on its own.
///
/// Enter and Space are only consumed while NO widget holds keyboard focus, so
/// a focused control keeps its native Enter/Space activation
/// (`FAKE_PRIMARY_CLICKED`) instead of the dialog acting on the key; Escape and
/// the arrows are consumed unconditionally, exactly like
/// [`consume_dialog_keys`]. The returned `tab`/`shift_tab` are always false.
pub fn consume_dialog_keys_tab_through(ctx: &egui::Context, space_is_action: bool) -> DialogKeys {
    let control_focused = ctx.memory(|mem| mem.focused().is_some());
    DialogKeys {
        escape: ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::Escape)),
        enter: !control_focused
            && ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::Enter)),
        space: space_is_action
            && !control_focused
            && ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::Space)),
        tab: false,
        shift_tab: false,
        left: ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::ArrowLeft)),
        right: ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::ArrowRight)),
    }
}

/// Which button of a two-button dialog row a key resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogDecision {
    Safe,
    Primary,
}

/// Resolve Escape/Enter/Space against a two-button row's focus flag. The SAFE
/// button (Cancel/Close) is the default: Escape and an unfocused Enter both
/// resolve to it, and a focused but DISABLED primary falls back to it, so a
/// keyboard-only user can never confirm a destructive action by accident.
pub fn dialog_key_decision(
    keys: DialogKeys,
    primary_focused: bool,
    primary_enabled: bool,
) -> Option<DialogDecision> {
    if keys.escape {
        Some(DialogDecision::Safe)
    } else if keys.enter || keys.space {
        Some(if primary_focused && primary_enabled {
            DialogDecision::Primary
        } else {
            DialogDecision::Safe
        })
    } else {
        None
    }
}

/// The widget that held keyboard focus, captured before a dialog takes it
/// (call on the dialog's FIRST frame, before `Window::show`). `None` when
/// nothing held focus.
pub fn capture_focus(ctx: &egui::Context) -> Option<egui::Id> {
    ctx.memory(|mem| mem.focused())
}

/// Give keyboard focus back to a widget saved by [`capture_focus`]. A saved
/// widget that no longer exists is dropped by egui's focus dead-man switch,
/// so restoring is always safe.
pub fn restore_focus(ctx: &egui::Context, saved: Option<egui::Id>) {
    if let Some(id) = saved {
        ctx.memory_mut(|mem| mem.request_focus(id));
    }
}

/// Which button the user clicked in [`dialog_button_row`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogButtonClick {
    Safe,
    Primary,
    None,
}

/// The standard dialog button row: primary (accent-filled) rightmost, safe
/// button left of it, matching the end-task dialog styling. `focused` selects
/// which button carries the focus ring; pointer-drag hover follow and the
/// per-frame `request_focus` mirror it too. `primary: None` renders a single
/// plain safe button (its keys are handled at the `consume_dialog_keys` level).
pub fn dialog_button_row(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    pal: &Palette,
    safe_label: &str,
    primary: Option<(&str, bool)>,
    focused: &mut bool,
) -> DialogButtonClick {
    let btn_size = egui::vec2(85.0, 24.0);
    let mut click = DialogButtonClick::None;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let primary_resp = primary.map(|(label, enabled)| {
            let mut button =
                egui::Button::new(egui::RichText::new(label).color(pal.accent_text).strong())
                    .min_size(btn_size)
                    .fill(pal.accent);
            if *focused {
                button = button.stroke(Stroke::new(2.0, Color32::WHITE));
            }
            let resp = ui.add_enabled(enabled, button);
            ui.add_space(8.0);
            resp
        });

        let mut safe_button = egui::Button::new(safe_label).min_size(btn_size);
        if primary_resp.is_some() && !*focused {
            safe_button = safe_button.stroke(Stroke::new(2.0, pal.accent));
        }
        let safe_resp = ui.add(safe_button);

        if let Some(primary_resp) = primary_resp {
            // Mouse dragging selection: keeping the button pressed and moving
            // over the row moves the keyboard focus with the pointer.
            if ctx.input(|i| i.pointer.primary_down()) {
                if safe_resp.hovered() {
                    *focused = false;
                } else if primary_resp.hovered() {
                    *focused = true;
                }
            }

            if safe_resp.clicked() {
                click = DialogButtonClick::Safe;
            } else if primary_resp.clicked() {
                click = DialogButtonClick::Primary;
            }

            if *focused && primary_resp.enabled() {
                primary_resp.request_focus();
            } else {
                *focused = false;
                safe_resp.request_focus();
            }
        } else if safe_resp.clicked() {
            click = DialogButtonClick::Safe;
        }
    });
    click
}

/// The two-button row for dialogs whose whole control set is keyboard-reachable
/// (tab-through dialogs with checkboxes above the row): unlike
/// [`dialog_button_row`] it never pins focus, so Tab keeps cycling through the
/// dialog's other controls, and the app-side `focused` flag is synced FROM the
/// real egui focus of the primary button each frame. The safe button claims
/// focus only while nothing at all holds it (the dialog's opening frame) —
/// if another widget may hold focus when the dialog opens, anchor focus in the
/// dialog body instead. [`dialog_key_decision`] keeps resolving Escape and an
/// unfocused/disabled Enter to the safe action.
// Wired up by the details-tab dialogs (the affinity confirmation is on this
// contract); the two-button destructive confirms deliberately keep the pinned
// [`dialog_button_row`], whose app-side focus flag is the keyboard trap.
pub fn dialog_button_row_mirror(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    pal: &Palette,
    safe_label: &str,
    primary: Option<(&str, bool)>,
    focused: &mut bool,
) -> DialogButtonClick {
    let btn_size = egui::vec2(85.0, 24.0);
    let mut click = DialogButtonClick::None;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let primary_resp = primary.map(|(label, enabled)| {
            let mut button =
                egui::Button::new(egui::RichText::new(label).color(pal.accent_text).strong())
                    .min_size(btn_size)
                    .fill(pal.accent);
            if *focused {
                button = button.stroke(Stroke::new(2.0, Color32::WHITE));
            }
            let resp = ui.add_enabled(enabled, button);
            ui.add_space(8.0);
            resp
        });

        let mut safe_button = egui::Button::new(safe_label).min_size(btn_size);
        if primary_resp.is_some() && !*focused {
            safe_button = safe_button.stroke(Stroke::new(2.0, pal.accent));
        }
        let safe_resp = ui.add(safe_button);

        if let Some(primary_resp) = primary_resp {
            if safe_resp.clicked() {
                click = DialogButtonClick::Safe;
            } else if primary_resp.clicked() {
                click = DialogButtonClick::Primary;
            }

            // Real focus is the source of truth; the flag only mirrors it for
            // the focus ring and `dialog_key_decision`. With neither button
            // focused (the opening frame, or after focus was dropped) the
            // safe default reclaims focus — once, not per frame.
            if ctx.memory(|mem| mem.focused().is_none()) {
                safe_resp.request_focus();
            }
            *focused = primary_resp.has_focus();
        } else if safe_resp.clicked() {
            click = DialogButtonClick::Safe;
        }
    });
    click
}

#[inline]
pub(crate) fn update_end_task_dialog_focus(
    current: bool,
    tab: bool,
    shift_tab: bool,
    left: bool,
    right: bool,
) -> bool {
    if shift_tab || tab {
        !current
    } else if left {
        false
    } else if right {
        true
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Regression (search-commit seam): the Enter that commits the global
    /// search must be CONSUMED. The singleline edit surrenders focus during
    /// its own pass, so the tables' row-action gate — which only yields to
    /// focused widgets and popups — sees the same Enter later in the frame
    /// and would fire the row action (e.g. Processes "Go to details") on top
    /// of the commit. While a dialog is up the commit stands down instead and
    /// leaves the keystroke for the dialog's own contract.
    #[test]
    fn committing_the_search_consumes_the_enter_keystroke() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let enter_frame = |ctx: &egui::Context, dialog_open: bool, text: &mut String| {
            let enter = egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                events: vec![enter],
                focused: true,
                ..Default::default()
            };
            let mut committed = false;
            let mut row_layer_saw_enter = false;
            let mut out = ctx.run_ui(raw, |ui| {
                let edit = ui.text_edit_singleline(text);
                let ran = commit_search_on_enter(ui.ctx(), edit.lost_focus(), dialog_open, || {
                    committed = true
                });
                // The same-frame row-action layer, exactly what the tables do.
                row_layer_saw_enter = crate::search::row_action_gate(ui.ctx(), false)
                    && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
                assert_eq!(
                    ran, committed,
                    "the helper's verdict must match the commit it ran"
                );
            });
            out.textures_delta.clear();
            (committed, row_layer_saw_enter)
        };

        // Frame 1: the search field holds keyboard focus.
        let mut text = String::new();
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            focused: true,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            ui.text_edit_singleline(&mut text).request_focus();
        });
        out.textures_delta.clear();

        // Frame 2: Enter commits — and must not leak to the row layer.
        let (committed, row_layer_saw_enter) = enter_frame(&ctx, false, &mut text);
        assert!(committed, "Enter on the focused search field commits");
        assert!(
            !row_layer_saw_enter,
            "the commit keystroke must not reach the row-action layer"
        );

        // Frame 3: while a dialog is up, the commit stands down and the
        // keystroke stays queued for the dialog's contract. The field does
        // not hold focus anymore, so the helper reports nothing either way —
        // park focus first to keep the premise of the probe intact.
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            focused: true,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            ui.text_edit_singleline(&mut text).request_focus();
        });
        out.textures_delta.clear();
        let (committed, row_layer_saw_enter) = enter_frame(&ctx, true, &mut text);
        assert!(!committed, "a dialog owns the keyboard: no commit");
        assert!(
            row_layer_saw_enter,
            "the unconsumed keystroke stays with the dialog contract"
        );
    }

    /// Tab or Down Arrow on the focused search field commits the search and
    /// surrenders keyboard focus so table rows immediately take navigation.
    #[test]
    fn committing_the_search_on_tab_or_down_consumes_keystroke_and_clears_focus() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let tab_frame = |ctx: &egui::Context,
                         key: egui::Key,
                         shift: bool,
                         dialog_open: bool,
                         text: &mut String| {
            let modifiers = if shift {
                egui::Modifiers::SHIFT
            } else {
                egui::Modifiers::NONE
            };
            let mut events = vec![egui::Event::ModifiersChanged(modifiers)];
            events.push(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            });
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                events,
                focused: true,
                ..Default::default()
            };
            let mut committed = false;
            let mut out = ctx.run_ui(raw, |ui| {
                let edit_id = egui::Id::new("test-search-id");
                let ran = commit_search_on_tab_or_down(ui.ctx(), edit_id, dialog_open, || {
                    committed = true;
                });
                let _edit = ui.add(egui::TextEdit::singleline(text).id(edit_id));
                assert_eq!(ran, committed);
            });
            out.textures_delta.clear();
            committed
        };

        // Frame 1: request focus on the search field.
        let mut text = String::from("svc");
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            focused: true,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).id(egui::Id::new("test-search-id")))
                .request_focus();
        });
        out.textures_delta.clear();

        // Frame 2: pressing Tab commits and clears focus.
        let committed = tab_frame(&ctx, egui::Key::Tab, false, false, &mut text);
        assert!(committed, "Tab on focused search field commits to table");
        assert!(
            ctx.memory(|m| m.focused()).is_none(),
            "focus surrendered from chrome"
        );
        assert!(
            !ctx.input(|i| i.key_pressed(egui::Key::Tab)),
            "Tab keystroke consumed"
        );

        // Park focus again.
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).id(egui::Id::new("test-search-id")))
                .request_focus();
        });
        out.textures_delta.clear();

        // Frame 3: pressing Down Arrow also commits and clears focus.
        let committed = tab_frame(&ctx, egui::Key::ArrowDown, false, false, &mut text);
        assert!(
            committed,
            "Down Arrow on focused search field commits to table"
        );
        assert!(ctx.memory(|m| m.focused()).is_none());
        assert!(
            !ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)),
            "Down Arrow consumed"
        );

        // Park focus again.
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).id(egui::Id::new("test-search-id")))
                .request_focus();
        });
        out.textures_delta.clear();

        // Frame 4: Shift+Tab does not trigger list jump (reserved for reverse navigation).
        let committed = tab_frame(&ctx, egui::Key::Tab, true, false, &mut text);
        assert!(!committed, "Shift+Tab must not trigger list jump");

        // Frame 5: when a dialog is open, Tab belongs to the dialog, no commit.
        let committed = tab_frame(&ctx, egui::Key::Tab, false, true, &mut text);
        assert!(!committed, "modal dialog keeps Tab key");
    }

    /// The clear button in the search field must not take keyboard focus via Tab
    /// (Sense::CLICK has no focusable bit, unlike Sense::click()).
    #[test]
    fn search_clear_button_sense_is_not_tab_focusable() {
        assert!(Sense::click().is_focusable());
        assert!(!Sense::CLICK.is_focusable());
    }

    /// The search field gives the keyboard back exactly when a click lands
    /// outside its box: a click on a row (or on dead window space) must let
    /// the next Arrow key move the table selection, while a click inside the
    /// box — including on its clear button — and frames without clicks keep
    /// the focus where it is.
    #[test]
    fn search_focus_leaves_on_outside_click_only() {
        let field = Rect::from_min_size(Pos2::new(100.0, 0.0), egui::vec2(495.0, 34.0));
        // Click on a row somewhere left of the centered field.
        assert!(click_surrenders_search(
            true,
            field,
            Some(Pos2::new(50.0, 17.0))
        ));
        // Click inside the field, or on its clear button, keeps it.
        assert!(!click_surrenders_search(true, field, Some(field.center())));
        assert!(!click_surrenders_search(
            true,
            field,
            Some(Pos2::new(field.right() - 18.0, field.center().y))
        ));
        // No click this frame, or a field that does not own focus: nothing.
        assert!(!click_surrenders_search(true, field, None));
        assert!(!click_surrenders_search(false, field, Some(Pos2::ZERO)));
    }

    /// Every F1 row exists in BOTH languages, with the key names the user's
    /// keyboard actually prints ("Strg" on German layouts) and stable anchor
    /// rows for the entries that read the same everywhere.
    #[test]
    fn help_overlay_rows_are_complete_and_localized() {
        for lang in [i18n::Lang::De, i18n::Lang::En] {
            let rows = shortcut_rows(lang);
            assert_eq!(rows.len(), 19, "{lang:?}: every documented shortcut");
            assert!(
                rows.iter()
                    .all(|(combo, text)| !combo.trim().is_empty() && !text.trim().is_empty()),
                "{lang:?}: no empty combo or description"
            );
            assert_eq!(rows[0].0, "F5");
            assert_eq!(rows[18].0, "F1");
            let expected_ctrl = i18n::tr_in(lang, K::KeyCtrl);
            assert_eq!(
                rows[1].0,
                format!("{expected_ctrl}+F / {}+F", i18n::tr_in(lang, K::KeyAlt))
            );
            assert_eq!(
                rows[7].0,
                i18n::tr_in(lang, K::KeyDel),
                "Delete is a single localized key name"
            );
        }
        // The German and English key names differ where it matters, so this
        // really pins localization rather than two copies of one string.
        assert_eq!(i18n::tr_in(i18n::Lang::De, K::KeyCtrl), "Strg");
        assert_eq!(i18n::tr_in(i18n::Lang::En, K::KeyCtrl), "Ctrl");
    }

    #[test]
    fn run_task_dialog_focus_cycles_forward_and_backward() {
        use RunTaskDialogFocus::*;
        assert_eq!(RunTaskDialogFocus::default(), Command);
        let mut focus = Command;
        for expected in [Elevated, Cancel, Browse, Ok, Command] {
            focus = focus.next(false);
            assert_eq!(focus, expected);
        }
        for expected in [Ok, Browse, Cancel, Elevated, Command] {
            focus = focus.next(true);
            assert_eq!(focus, expected);
        }
    }

    #[test]
    fn run_task_dialog_keyboard_actions_match_native_dialog_semantics() {
        use RunTaskDialogAction as Action;
        use RunTaskDialogFocus as Focus;
        assert_eq!(
            run_task_dialog_key_action(Focus::Command, true, false),
            Some(Action::Submit)
        );
        assert_eq!(
            run_task_dialog_key_action(Focus::Elevated, true, false),
            Some(Action::Submit)
        );
        assert_eq!(
            run_task_dialog_key_action(Focus::Cancel, true, false),
            Some(Action::Cancel)
        );
        assert_eq!(
            run_task_dialog_key_action(Focus::Browse, true, false),
            Some(Action::Browse)
        );
        assert_eq!(
            run_task_dialog_key_action(Focus::Ok, true, false),
            Some(Action::Submit)
        );
        assert_eq!(
            run_task_dialog_key_action(Focus::Elevated, false, true),
            Some(Action::ToggleElevated)
        );
        assert_eq!(
            run_task_dialog_key_action(Focus::Command, false, true),
            None
        );
    }

    #[test]
    fn test_end_task_dialog_focus_keyboard_transitions() {
        // Initially Cancel is focused (false) — the safe default.
        let mut focus = false;

        // Pressing Tab toggles focus to End task (true).
        focus = update_end_task_dialog_focus(focus, true, false, false, false);
        assert!(focus);

        // Releasing Tab on subsequent frame (no keys) MUST preserve End task focus (true).
        focus = update_end_task_dialog_focus(focus, false, false, false, false);
        assert!(focus);

        // Pressing Tab again toggles back to Cancel (false).
        focus = update_end_task_dialog_focus(focus, true, false, false, false);
        assert!(!focus);

        // Next frame preserves Cancel.
        focus = update_end_task_dialog_focus(focus, false, false, false, false);
        assert!(!focus);

        // Pressing Shift+Tab toggles to End task (true).
        focus = update_end_task_dialog_focus(focus, false, true, false, false);
        assert!(focus);

        // Pressing Left arrow explicitly focuses Cancel (left button).
        focus = update_end_task_dialog_focus(focus, false, false, true, false);
        assert!(!focus);

        // Pressing Right arrow explicitly focuses End task (right button).
        focus = update_end_task_dialog_focus(focus, false, false, false, true);
        assert!(focus);
    }

    #[test]
    fn test_end_task_dialog_focus_persistence_in_context() {
        let ctx = egui::Context::default();
        let focus_id = egui::Id::new("end_task_dialog_focus_end");

        // Frame 1: Initial dialog opening defaults to Cancel (false), the
        // safe action — Enter must not confirm the kill.
        let mut focus: bool = ctx.data(|d| d.get_temp(focus_id)).unwrap_or(false);
        assert!(!focus);

        // User pressed Tab.
        focus = update_end_task_dialog_focus(focus, true, false, false, false);
        ctx.data_mut(|d| d.insert_temp(focus_id, focus));
        assert!(focus);

        // Frame 2: Next tick without keys, must stay on End task.
        let mut focus_frame2: bool = ctx.data(|d| d.get_temp(focus_id)).unwrap_or(false);
        focus_frame2 = update_end_task_dialog_focus(focus_frame2, false, false, false, false);
        ctx.data_mut(|d| d.insert_temp(focus_id, focus_frame2));
        assert!(focus_frame2);

        // Frame 3: Dialog closes -> temp data removed.
        ctx.data_mut(|d| d.remove_temp::<bool>(focus_id));
        let reset = ctx.data(|d| d.get_temp::<bool>(focus_id));
        assert_eq!(reset, None);
    }

    /// The tab-through contract leaves Tab/Shift+Tab to egui's focus system
    /// and leaves Enter with a focused control (native activation must still
    /// fire), while Escape and the arrows stay consumed dialog keys.
    #[test]
    fn tab_through_variant_consumption_set() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let raw = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            events,
            focused: true,
            ..Default::default()
        };
        let key = |key: egui::Key, modifiers: egui::Modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let none = egui::Modifiers::default();

        // Frame 1: a control gains focus inside the dialog.
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            ui.button("Checkbox stand-in").request_focus();
        });
        out.textures_delta.clear();

        // Frame 2: Enter arrives while a control holds focus. The contract
        // must leave it with the control, whose native keyboard activation
        // then fires (the button is painted AFTER the consume call, exactly
        // like a real dialog body).
        let mut keys = DialogKeys::default();
        let mut activated = false;
        let mut out = ctx.run_ui(raw(vec![key(egui::Key::Enter, none)]), |ui| {
            keys = consume_dialog_keys_tab_through(ui.ctx(), false);
            activated = ui.button("Ok").clicked();
        });
        out.textures_delta.clear();
        assert!(!keys.enter, "Enter belongs to the focused control");
        assert!(activated, "the focused control must activate natively");
        assert!(!keys.escape && !keys.left && !keys.right);

        // Frame 3: Tab/Shift+Tab are never consumed by this contract — egui's
        // focus system cycles the dialog's controls with them.
        let mut keys = DialogKeys::default();
        let mut out = ctx.run_ui(
            raw(vec![
                key(egui::Key::Tab, none),
                key(egui::Key::Tab, egui::Modifiers::SHIFT),
            ]),
            |ui| {
                keys = consume_dialog_keys_tab_through(ui.ctx(), false);
                let _ = ui.button("Ok");
            },
        );
        out.textures_delta.clear();
        assert!(!keys.tab && !keys.shift_tab, "Tab stays with egui");

        // Nothing focused (fresh dialog session): Enter, Escape and the
        // arrows are the dialog's keys again, and nothing activates.
        let fresh = egui::Context::default();
        let mut keys = DialogKeys::default();
        let mut activated = false;
        let mut out = fresh.run_ui(
            raw(vec![
                key(egui::Key::Escape, none),
                key(egui::Key::ArrowLeft, none),
                key(egui::Key::Enter, none),
            ]),
            |ui| {
                keys = consume_dialog_keys_tab_through(ui.ctx(), false);
                activated = ui.button("Ok").clicked();
            },
        );
        out.textures_delta.clear();
        assert!(keys.escape && keys.left && keys.enter);
        assert!(!activated, "nothing is focused, so nothing activates");

        // The hard-trap contract is unchanged: it still consumes Tab so the
        // app-side two-button toggle keeps working.
        let trapped = egui::Context::default();
        let mut tab_keys = DialogKeys::default();
        let mut out = trapped.run_ui(raw(vec![key(egui::Key::Tab, none)]), |ui| {
            tab_keys = consume_dialog_keys(ui.ctx(), true);
        });
        out.textures_delta.clear();
        assert!(
            tab_keys.tab,
            "the hard-trap contract still consumes Tab (Shift+Tab is folded into it by the pre-existing matches_logically ordering, which both toggle buttons identically)"
        );
    }

    /// The mirror row follows REAL egui focus instead of pinning it: the
    /// opening frame puts focus on the safe button, focus moved elsewhere is
    /// not stolen back, and the app-side flag mirrors the primary button.
    #[test]
    fn dialog_button_row_mirror_follows_real_focus() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let raw = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            events,
            focused: true,
            ..Default::default()
        };
        let pal = crate::theme::palette_ctx(&ctx);
        let key = |key: egui::Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };

        // Opening frame with nothing focused: the safe default claims focus.
        let mut app_focused = false;
        let mut focused_after = None;
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            dialog_button_row_mirror(
                ui,
                &ctx,
                &pal,
                "Cancel",
                Some(("Confirm", true)),
                &mut app_focused,
            );
            focused_after = ctx.memory(|mem| mem.focused());
        });
        out.textures_delta.clear();
        assert!(!app_focused, "the safe button owns the opening frame");
        assert!(focused_after.is_some(), "the safe button claimed focus");
        let safe_id = focused_after;

        // Focus moved to a widget outside the row: the mirror must not steal
        // it back, and the flag mirrors the (unfocused) primary.
        let mut stray_id = None;
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            dialog_button_row_mirror(
                ui,
                &ctx,
                &pal,
                "Cancel",
                Some(("Confirm", true)),
                &mut app_focused,
            );
            let stray = ui.button("Outside");
            if ctx.memory(|mem| mem.focused()) == safe_id {
                stray.request_focus();
                stray_id = Some(stray.id);
            }
        });
        out.textures_delta.clear();
        assert!(!app_focused);
        let stray_id = stray_id.expect("the stray widget took focus");
        assert_eq!(ctx.memory(|mem| mem.focused()), Some(stray_id));

        // Still on the stray widget: the safe button stays unclaimed.
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            dialog_button_row_mirror(
                ui,
                &ctx,
                &pal,
                "Cancel",
                Some(("Confirm", true)),
                &mut app_focused,
            );
            let _ = ui.button("Outside");
        });
        out.textures_delta.clear();
        assert!(!app_focused, "the unfocused primary mirrors to false");
        assert_eq!(
            ctx.memory(|mem| mem.focused()),
            Some(stray_id),
            "the mirror must not re-pin the safe button"
        );

        // Tab navigation reaches the primary button (egui cycles focus; the
        // mirror follows), and `dialog_key_decision` then confirms.
        let mut out = ctx.run_ui(raw(vec![key(egui::Key::Tab)]), |ui| {
            dialog_button_row_mirror(
                ui,
                &ctx,
                &pal,
                "Cancel",
                Some(("Confirm", true)),
                &mut app_focused,
            );
            let _ = ui.button("Outside");
        });
        out.textures_delta.clear();
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            dialog_button_row_mirror(
                ui,
                &ctx,
                &pal,
                "Cancel",
                Some(("Confirm", true)),
                &mut app_focused,
            );
            let _ = ui.button("Outside");
        });
        out.textures_delta.clear();
        assert!(app_focused, "real focus on the primary must sync the flag");
        assert_eq!(
            dialog_key_decision(
                DialogKeys {
                    enter: true,
                    ..DialogKeys::default()
                },
                app_focused,
                true
            ),
            Some(DialogDecision::Primary),
        );
    }

    /// Focus capture/restore: restoring a live widget sticks, restoring a
    /// vanished one is dropped by egui's dead-man switch, and `None` is a
    /// no-op.
    #[test]
    fn capture_and_restore_focus_round_trip() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            focused: true,
            ..Default::default()
        };

        assert_eq!(capture_focus(&ctx), None, "nothing focused yet");

        let mut out = ctx.run_ui(raw.clone(), |ui| {
            ui.button("Target").request_focus();
        });
        out.textures_delta.clear();
        let saved = capture_focus(&ctx);
        assert!(saved.is_some());

        // The widget still exists: the restore sticks.
        restore_focus(&ctx, saved);
        let mut has_focus = false;
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            has_focus = ui.button("Target").has_focus();
        });
        out.textures_delta.clear();
        assert!(has_focus, "restored focus must stick on a live widget");

        // The widget is gone: the dead-man switch drops the restore.
        restore_focus(&ctx, Some(egui::Id::new("vanished-widget")));
        let mut has_focus = false;
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            has_focus = ui.button("Target").has_focus();
        });
        out.textures_delta.clear();
        assert!(!has_focus, "a vanished widget must not keep focus");

        // `None` restores nothing and must not panic.
        restore_focus(&ctx, None);
    }

    /// The dialog keyboard contract: Escape and an unfocused Enter resolve to
    /// the SAFE action; only a deliberately focused, enabled primary button
    /// lets Enter/Space confirm.
    #[test]
    fn test_dialog_key_decision_defaults_to_safe() {
        let none = DialogKeys::default();
        assert_eq!(dialog_key_decision(none, false, true), None);

        assert_eq!(
            dialog_key_decision(
                DialogKeys {
                    escape: true,
                    ..none
                },
                true,
                true
            ),
            Some(DialogDecision::Safe)
        );
        // Unfocused Enter/Space -> safe.
        assert_eq!(
            dialog_key_decision(
                DialogKeys {
                    enter: true,
                    ..none
                },
                false,
                true
            ),
            Some(DialogDecision::Safe)
        );
        assert_eq!(
            dialog_key_decision(
                DialogKeys {
                    space: true,
                    ..none
                },
                false,
                true
            ),
            Some(DialogDecision::Safe)
        );
        // Focused primary -> primary.
        assert_eq!(
            dialog_key_decision(
                DialogKeys {
                    enter: true,
                    ..none
                },
                true,
                true
            ),
            Some(DialogDecision::Primary)
        );
        // Focused but DISABLED primary still falls back to safe.
        assert_eq!(
            dialog_key_decision(
                DialogKeys {
                    enter: true,
                    ..none
                },
                true,
                false
            ),
            Some(DialogDecision::Safe)
        );
    }

    /// A toast is fully opaque for almost all of its life and fades only over
    /// the last stretch, so it never vanishes mid-glance.
    #[test]
    fn toast_alpha_is_opaque_until_the_fade_and_reaches_zero_at_expiry() {
        use crate::app::{TOAST_FADE, TOAST_TTL};
        assert_eq!(toast_alpha(Duration::ZERO), 1.0);
        assert_eq!(toast_alpha(TOAST_TTL - TOAST_FADE), 1.0);
        let half = toast_alpha(TOAST_TTL - TOAST_FADE / 2);
        assert!((half - 0.5).abs() < 0.01, "mid-fade alpha: {half}");
        assert_eq!(toast_alpha(TOAST_TTL), 0.0);
        // Past expiry the toast is already gone; alpha must not go negative.
        assert_eq!(toast_alpha(TOAST_TTL * 2), 0.0);
    }

    /// The bulk action appears only once there is a stack to clear, and one
    /// click empties it.
    #[test]
    fn clear_all_appears_for_a_stack_and_empties_it() {
        use std::sync::Mutex;
        let clear_all_id = egui::Id::new("toast-clear-all");
        let ctx = egui::Context::default();
        let screen_rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let frame = |ctx: &egui::Context, toasts: &crate::app::ToastQueue, events| {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    ..Default::default()
                },
                |_| draw_toasts_queue(toasts, ctx),
            );
            out.textures_delta.clear();
        };

        // A single toast has nothing to bulk-dismiss.
        let toasts: crate::app::ToastQueue = Mutex::new(Vec::new());
        crate::app::toast_from(&toasts, "Only one");
        frame(&ctx, &toasts, Vec::new());
        assert!(
            ctx.memory(|m| m.area_rect(clear_all_id)).is_none(),
            "a lone toast must not carry a clear-all button"
        );

        // A second one brings the button out, above the whole stack. The first
        // frame of a new Area lays it out before the anchor applies, so its
        // resting place is only readable from the frame after.
        crate::app::toast_from(&toasts, "And another");
        frame(&ctx, &toasts, Vec::new());
        frame(&ctx, &toasts, Vec::new());
        let rect = ctx
            .memory(|m| m.area_rect(clear_all_id))
            .expect("clear-all must be laid out for a stack");

        let click = |pos: Pos2, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let at = rect.center();
        // The pointer has to arrive before it presses: egui resolves a click
        // against the widget under `interact_pos`, and a press alone from the
        // headless default position lands nowhere.
        frame(&ctx, &toasts, vec![egui::Event::PointerMoved(at)]);
        frame(&ctx, &toasts, vec![click(at, true), click(at, false)]);
        assert!(
            tm_core::sync::lock(&toasts).is_empty(),
            "clear all must empty the stack"
        );
    }

    /// An unattended toast expires on its own, and a fresh one is untouched by
    /// the same pass.
    #[test]
    fn toasts_expire_after_the_ttl_and_the_young_ones_survive() {
        use std::sync::Mutex;
        let toasts: crate::app::ToastQueue = Mutex::new(Vec::new());
        crate::app::toast_from(&toasts, "Stale action");
        crate::app::toast_from(&toasts, "Fresh action");
        // `born` is the only thing that ages a toast; reaching past the TTL is
        // what the clock would do, without making the test wait for it.
        tm_core::sync::lock(&toasts)[0].born = Instant::now()
            .checked_sub(crate::app::TOAST_TTL + Duration::from_secs(1))
            .expect("instant far enough from the epoch");

        let ctx = egui::Context::default();
        let screen_rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |_| draw_toasts_queue(&toasts, &ctx),
        );
        out.textures_delta.clear();

        let left = tm_core::sync::lock(&toasts);
        assert_eq!(left.len(), 1, "the expired toast must be gone");
        assert_eq!(left[0].msg, "Fresh action");
    }

    #[test]
    fn clicking_toast_body_does_not_close_but_x_button_closes() {
        use std::sync::Mutex;
        let toasts: crate::app::ToastQueue = Mutex::new(Vec::new());
        crate::app::toast_from(&toasts, "Action completed");

        let ctx = egui::Context::default();
        let screen_rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));

        let raw1 = egui::RawInput {
            screen_rect: Some(screen_rect),
            time: Some(0.0),
            ..Default::default()
        };
        let mut out1 = ctx.run_ui(raw1, |_| {
            draw_toasts_queue(&toasts, &ctx);
        });
        out1.textures_delta.clear();
        assert_eq!(tm_core::sync::lock(&toasts).len(), 1);

        // Click on the text area / body of the toast
        let raw2 = egui::RawInput {
            screen_rect: Some(screen_rect),
            time: Some(1.0),
            events: vec![
                egui::Event::PointerButton {
                    pos: egui::pos2(700.0, 570.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                egui::Event::PointerButton {
                    pos: egui::pos2(700.0, 570.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            ..Default::default()
        };
        let mut out2 = ctx.run_ui(raw2, |_| {
            draw_toasts_queue(&toasts, &ctx);
        });
        out2.textures_delta.clear();
        assert_eq!(
            tm_core::sync::lock(&toasts).len(),
            1,
            "body click must not close toast"
        );

        // Click the close button
        let raw3 = egui::RawInput {
            screen_rect: Some(screen_rect),
            time: Some(2.0),
            events: vec![
                egui::Event::PointerButton {
                    pos: egui::pos2(771.0, 571.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                egui::Event::PointerButton {
                    pos: egui::pos2(771.0, 571.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            ..Default::default()
        };
        let mut out3 = ctx.run_ui(raw3, |_| {
            draw_toasts_queue(&toasts, &ctx);
        });
        out3.textures_delta.clear();
        assert_eq!(
            tm_core::sync::lock(&toasts).len(),
            0,
            "x button click must close toast"
        );
    }

    #[test]
    fn sidebar_nav_item_surrenders_focus_on_arrow_right_and_escape() {
        let pal = theme::DARK;
        for key in [egui::Key::ArrowRight, egui::Key::Escape] {
            let ctx = egui::Context::default();
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
            // Frame 1: draw nav item and request focus.
            let raw1 = egui::RawInput {
                screen_rect: Some(screen),
                focused: true,
                ..Default::default()
            };
            let mut out1 = ctx.run_ui(raw1, |ui| {
                let resp = nav_item(ui, &pal, Icon::Processes, "Processes", true, false);
                resp.request_focus();
            });
            out1.textures_delta.clear();
            assert!(
                ctx.memory(|m| m.focused()).is_some(),
                "nav item gained focus"
            );

            // Frame 2: send ArrowRight or Escape.
            let raw2 = egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Default::default(),
                }],
                focused: true,
                ..Default::default()
            };
            let mut out2 = ctx.run_ui(raw2, |ui| {
                let _resp = nav_item(ui, &pal, Icon::Processes, "Processes", true, false);
            });
            out2.textures_delta.clear();
            assert!(
                ctx.memory(|m| m.focused()).is_none(),
                "focus surrendered to table rows on {key:?}"
            );
            if key != egui::Key::Escape {
                assert!(!ctx.input(|i| i.key_pressed(key)), "{key:?} was consumed");
            }
        }
    }

    #[test]
    fn cmd_button_surrenders_focus_on_arrow_down_and_escape() {
        let pal = theme::DARK;
        for key in [egui::Key::ArrowDown, egui::Key::Escape] {
            let ctx = egui::Context::default();
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
            // Frame 1: draw cmd_button and request focus.
            let raw1 = egui::RawInput {
                screen_rect: Some(screen),
                focused: true,
                ..Default::default()
            };
            let mut out1 = ctx.run_ui(raw1, |ui| {
                let id = ui.id().with(("cmd-button", "End task"));
                ui.ctx().memory_mut(|m| m.request_focus(id));
                let _ = cmd_button(ui, &pal, Icon::Close, "End task", true);
            });
            out1.textures_delta.clear();
            assert!(
                ctx.memory(|m| m.focused()).is_some(),
                "cmd button gained focus"
            );

            // Frame 2: send ArrowDown or Escape.
            let raw2 = egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Default::default(),
                }],
                focused: true,
                ..Default::default()
            };
            let mut out2 = ctx.run_ui(raw2, |ui| {
                let _ = cmd_button(ui, &pal, Icon::Close, "End task", true);
            });
            out2.textures_delta.clear();
            assert!(
                ctx.memory(|m| m.focused()).is_none(),
                "focus surrendered to table rows on {key:?}"
            );
            if key != egui::Key::Escape {
                assert!(!ctx.input(|i| i.key_pressed(key)), "{key:?} was consumed");
            }
        }
    }
}

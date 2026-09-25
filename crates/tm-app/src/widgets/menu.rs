//! Classic Windows-style context menus.
//!
//! egui's own menu style packs entries at `button_padding = (2, 0)` and then
//! separates them by the global `item_spacing.y` (6 px here). That reads as a
//! column of small labels with holes between them, not as a menu: the click
//! target is only as tall as the text, and the gaps are dead pixels that do
//! not highlight and do not activate the entry under the cursor.
//!
//! Everything in this module paints ONE uniform, full-width, gapless entry
//! row — [`ITEM_H`] tall, with a left check gutter and an optional submenu
//! arrow — the way Explorer and Task Manager draw their menus. Menu state
//! (checked / "this is the current value") is a hand-painted TICK in that
//! gutter, never a checkbox widget: a boxed control inside a menu looks like
//! a form, and the gutter is what keeps every label on the same left edge
//! whether or not it is ticked.

use eframe::egui::{self, Color32, CornerRadius, FontId, Pos2, Response, Sense, Stroke, Ui};

use crate::theme::{self, Palette};

/// Height of one menu entry. Matches the Win11 Explorer context menu at
/// 100 % scaling; also the minimum interact size we install for anything
/// egui itself lays out inside a menu.
pub const ITEM_H: f32 = 28.0;

/// Left gutter reserved on EVERY entry for the state tick, so ticked and
/// unticked labels share one left edge.
const GUTTER_W: f32 = 26.0;

/// Right gutter reserved on entries that open a submenu.
const ARROW_W: f32 = 20.0;

/// Trailing padding after the label.
const TEXT_PAD_RIGHT: f32 = 14.0;

const FONT_SIZE: f32 = 13.0;

/// Horizontal inset of the hover highlight inside the popup frame.
const HIGHLIGHT_INSET: f32 = 2.0;

/// Height of a separator row (the line is centered inside it).
const SEP_H: f32 = 7.0;

/// Style installed on every popup opened through this module.
///
/// Note `item_spacing.y = 0`: menu entries must touch, both so the menu reads
/// as one list and so egui's submenu hover bridge (which expands the button
/// rect by half the item spacing) has nothing left to bridge.
pub fn style(style: &mut egui::Style) {
    egui::containers::menu::menu_style(style);
    style.spacing.item_spacing = egui::vec2(0.0, 0.0);
    style.spacing.menu_margin = egui::Margin::symmetric(4, 4);
    // Anything egui lays out by itself inside a menu (a stray `ui.button`,
    // a text edit) then still gets menu-sized rows instead of 18 px ones.
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.interact_size.y = ITEM_H;
}

/// Open a right-click menu on `resp` in the classic style.
#[allow(dead_code)]
pub fn context_menu(resp: &Response, add: impl FnOnce(&mut Ui)) {
    context_menu_kb(resp, false, add);
}

/// Open a dropdown menu on `resp` in the classic style:
/// left-click toggles the menu below `resp`, while right-click opens it at the pointer.
pub fn dropdown_menu(resp: &Response, add: impl FnOnce(&mut Ui)) {
    let popup = egui::Popup::menu(resp);
    let popup = if resp.secondary_clicked() {
        popup
            .open_memory(Some(egui::containers::SetOpenCommand::Bool(true)))
            .at_pointer_fixed()
    } else {
        popup
    };
    popup.style(style).show(add);
    // Same stale-handoff hygiene as the row context menus.
    drop_stale_kb_handoff(&resp.ctx);
}

/// True for the one frame on which the user asked for the context menu of
/// the current selection with the keyboard: the Menu/Application key (next
/// to Right Ctrl), or its standard Shift+F10 accelerator.
///
/// The gate only excludes a focused TEXT EDIT: while the user is typing,
/// the Menu key belongs to the field. Any other focused widget (a table row
/// after Tab navigation, a button) must not swallow the key — that is exactly
/// how a keyboard user opens the menu for the selection.
pub fn keyboard_menu_requested(ctx: &egui::Context) -> bool {
    !ctx.text_edit_focused()
        && ctx.input(|input| {
            input.key_pressed(egui::Key::ContextMenu)
                || (input.key_pressed(egui::Key::F10) && input.modifiers.shift)
        })
}

/// Memory key holding the id of the popup whose keyboard-opened menu still
/// owes first-entry focus. Stored as the POPUP id, not a bare flag: the
/// popup's first frame is a SIZING PASS in which `ui.is_enabled()` is false
/// for every entry, so the request cannot be consumed until the next frame —
/// and binding it to the popup is what keeps a menu that never had an enabled
/// entry from handing focus to whatever menu opens later.
const KB_INITIAL_FOCUS: &str = "tm-menu-kb-initial-focus";

/// The temp-data key of a pending first-entry focus handoff (see
/// [`KB_INITIAL_FOCUS`]).
fn kb_initial_focus_key() -> egui::Id {
    egui::Id::new(KB_INITIAL_FOCUS)
}

/// Drop a pending handoff whose owning popup is gone. Both root menus
/// (`context_menu_kb`/`dropdown_menu`/`menu_button`) and keyboard-opened
/// submenus (`submenu`) park a POPUP id there; the root cleanup originally
/// compared against its own popup id only, so a submenu opened from the
/// keyboard whose entries were all disabled left a pending handoff behind —
/// and a later purely-mouse open of that submenu auto-focused its first
/// entry. Checking `is_id_open` on the PENDING id itself covers both levels
/// and runs even on frames where the menus no longer render their content.
fn drop_stale_kb_handoff(ctx: &egui::Context) {
    let key = kb_initial_focus_key();
    if ctx.data(|d| d.get_temp::<egui::Id>(key)).is_none() {
        return;
    }
    // egui tracks ONE open popup per viewport — the ROOT of the menu tree —
    // so a pending SUBMENU id is never the popup `is_id_open` would report,
    // and a root-only check would drop a live submenu handoff the very next
    // frame. A handoff is stale exactly when NO menu is open any more: while
    // the tree is up its own entries consume the pending id, and once the
    // tree is gone nothing can ever claim it again.
    if !ctx.any_popup_open() {
        ctx.data_mut(|d| d.remove::<egui::Id>(key));
    }
}

/// Context menu that also opens from the keyboard: when `keyboard_open` is
/// set (the row is the current selection and [`keyboard_menu_requested`]
/// fired this frame), the same popup is forced open, anchored to the row —
/// which is where Windows puts a keyboard-invoked menu — instead of waiting
/// for a secondary click. The first enabled entry receives keyboard focus,
/// and from there egui's own focus system takes over: bare arrow keys move
/// between entries (disabled ones are skipped) and Enter/Space activate the
/// focused entry through focused-button activation, which `clicked()` reports.
pub fn context_menu_kb(resp: &Response, keyboard_open: bool, add: impl FnOnce(&mut Ui)) {
    let popup_id = egui::Popup::default_response_id(resp);
    let popup = egui::Popup::context_menu(resp);
    let popup = if keyboard_open {
        resp.ctx
            .data_mut(|d| d.insert_temp(egui::Id::new(KB_INITIAL_FOCUS), popup_id));
        popup
            .open_memory(egui::containers::SetOpenCommand::Bool(true))
            .at_position(resp.rect.left_bottom())
    } else {
        popup
    };
    popup.style(style).show(add);
    // Once this menu is gone, a handoff nothing consumed (every entry was
    // greyed out — root OR submenu) must not outlive it.
    drop_stale_kb_handoff(&resp.ctx);
}

/// A drop-down button in the app chrome that opens a menu in the same style.
pub fn menu_button(
    ui: &mut Ui,
    button: egui::Button<'_>,
    content: impl FnOnce(&mut Ui),
) -> Response {
    let response = egui::containers::menu::MenuButton::from_button(button)
        .config(egui::containers::menu::MenuConfig::new().style(style))
        .ui(ui, content)
        .0;
    // Same stale-handoff hygiene as the row context menus.
    drop_stale_kb_handoff(ui.ctx());
    response
}

/// What an entry draws besides its label.
#[derive(Default, Clone, Copy)]
struct Marks {
    /// Draw the state tick in the left gutter.
    checked: bool,
    /// Reserve the right gutter and draw the submenu arrow.
    arrow: bool,
    /// Paint as an open submenu parent (stays lit while the child is up).
    open: bool,
}

/// Natural (unjustified) width an entry wants. The popup sizes itself to the
/// widest entry during egui's sizing pass; the justified layout then stretches
/// every row to that width.
fn desired_width(ui: &Ui, text: &str, marks: Marks) -> f32 {
    let text_w = ui
        .painter()
        .layout_no_wrap(
            text.to_owned(),
            FontId::proportional(FONT_SIZE),
            Color32::WHITE,
        )
        .size()
        .x;
    GUTTER_W + text_w + TEXT_PAD_RIGHT + if marks.arrow { ARROW_W } else { 0.0 }
}

/// Paint one full-width menu row and return its response.
fn entry(ui: &mut Ui, text: &str, marks: Marks) -> Response {
    let pal = theme::palette(ui);
    let want = egui::vec2(desired_width(ui, text, marks), ITEM_H);
    // `allocate_at_least` (not `allocate_exact_size`) is what returns the
    // JUSTIFIED rect: menus lay out top-down-justified, and the exact variant
    // re-aligns the desired size back inside the justified frame, which would
    // leave every row only as wide as its own label.
    let (rect, resp) = ui.allocate_at_least(want, Sense::click());
    let enabled = ui.is_enabled();

    // A keyboard-opened menu hands focus to its first enabled entry so the
    // arrow keys have somewhere to start. The popup's FIRST frame is a sizing
    // pass where no entry is enabled yet, so the request lives in memory until
    // the frame it can actually be consumed.
    if enabled {
        let key = kb_initial_focus_key();
        let pending = ui.ctx().data(|d| d.get_temp::<egui::Id>(key));
        if pending == Some(ui.layer_id().id) {
            ui.ctx().data_mut(|d| d.remove::<egui::Id>(key));
            resp.request_focus();
        }
    }

    // Inside a menu popup, Tab closes the popup instead of stranding an open
    // menu behind the focus. Focus itself lands NOWHERE: the focused entry
    // disappears with the popup, so egui's dead-man switch (`Memory::
    // end_pass` drops a focused widget that was not painted) clears it, and
    // Tab traversal restarts from the top on the NEXT Tab press — which is
    // exactly the behavior when Tab is pressed in a native menu. Shift+Tab
    // closes too — `key_pressed` ignores modifiers.
    if egui::containers::menu::is_in_menu(ui) && ui.input(|input| input.key_pressed(egui::Key::Tab))
    {
        ui.close();
    }

    // Own the horizontal arrows inside menus: egui's spatial navigation would
    // otherwise jump OUT of the popup at whatever widget happens to lie to the
    // side. `submenu` turns ArrowRight into "open this submenu" and ArrowLeft
    // into "close it again"; this filter makes egui's focus system ignore bare
    // horizontal arrows while a menu entry is focused. The vendor's
    // `Memory::set_focus_lock_filter` only STICKS when the widget held focus
    // LAST frame too (`had_focus_last_frame`), so this call is a no-op on the
    // very frame the handoff lands focus and the filter takes effect from the
    // frame AFTER focus landed — one frame of spatial-arrow gap, which is
    // what the "focus applies from the next frame" tests above pin.
    if enabled && resp.has_focus() {
        ui.ctx().memory_mut(|mem| {
            mem.set_focus_lock_filter(
                resp.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    ..Default::default()
                },
            );
        });
    }

    let fill = if !enabled {
        None
    } else if resp.is_pointer_button_down_on() {
        Some(pressed_fill(&pal))
    } else if resp.hovered() || marks.open || resp.has_focus() {
        Some(pal.card_bg_hover)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(
            rect.shrink2(egui::vec2(HIGHLIGHT_INSET, 0.0)),
            CornerRadius::same(4),
            fill,
        );
    }

    let text_color = if enabled { pal.text } else { pal.text_dim };
    if marks.checked {
        draw_tick(
            ui,
            Pos2::new(rect.left() + GUTTER_W / 2.0, rect.center().y),
            text_color,
        );
    }
    ui.painter().text(
        Pos2::new(rect.left() + GUTTER_W, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::proportional(FONT_SIZE),
        text_color,
    );
    if marks.arrow {
        draw_arrow(
            ui,
            Pos2::new(rect.right() - ARROW_W / 2.0, rect.center().y),
            pal.text_dim,
        );
    }

    let label = text.to_owned();
    resp.widget_info(|| {
        if marks.checked {
            egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, true, label.clone())
        } else {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label.clone())
        }
    });
    resp
}

/// One step past the hover fill, toward the accent, so a click reads as a
/// press on both light and dark surfaces.
fn pressed_fill(pal: &Palette) -> Color32 {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * 0.35).round() as u8;
    Color32::from_rgb(
        mix(pal.card_bg_hover.r(), pal.accent.r()),
        mix(pal.card_bg_hover.g(), pal.accent.g()),
        mix(pal.card_bg_hover.b(), pal.accent.b()),
    )
}

/// The state mark: a hand-painted tick, not a glyph. `✓` is not present in
/// every installed UI font, and a missing glyph renders as a tofu BOX — which
/// is exactly what a menu tick must never look like.
fn draw_tick(ui: &Ui, center: Pos2, color: Color32) {
    let s = 4.5f32;
    let a = Pos2::new(center.x - s, center.y + 0.5);
    let b = Pos2::new(center.x - s * 0.25, center.y + s * 0.8);
    let c = Pos2::new(center.x + s * 1.05, center.y - s * 0.85);
    ui.painter()
        .add(egui::Shape::line(vec![a, b, c], Stroke::new(1.8, color)));
}

fn draw_arrow(ui: &Ui, center: Pos2, color: Color32) {
    let s = 3.4f32;
    let a = Pos2::new(center.x - s * 0.6, center.y - s);
    let b = Pos2::new(center.x + s * 0.6, center.y);
    let c = Pos2::new(center.x - s * 0.6, center.y + s);
    ui.painter()
        .add(egui::Shape::line(vec![a, b, c], Stroke::new(1.5, color)));
}

/// A plain command entry.
pub fn item(ui: &mut Ui, text: &str) -> Response {
    entry(ui, text, Marks::default())
}

/// A command entry that can be greyed out. Chain `.on_disabled_hover_text`
/// on the result to explain why.
pub fn item_enabled(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    ui.add_enabled_ui(enabled, |ui| entry(ui, text, Marks::default()))
        .inner
}

/// An entry whose state is shown as a tick in the left gutter — the menu form
/// both of a checkbox and of a "this is the current value" radio mark.
pub fn check(ui: &mut Ui, text: &str, checked: bool) -> Response {
    entry(
        ui,
        text,
        Marks {
            checked,
            ..Default::default()
        },
    )
}

/// Ticked entry that can be greyed out.
pub fn check_enabled(ui: &mut Ui, text: &str, checked: bool, enabled: bool) -> Response {
    ui.add_enabled_ui(enabled, |ui| {
        entry(
            ui,
            text,
            Marks {
                checked,
                ..Default::default()
            },
        )
    })
    .inner
}

/// A ticked entry bound to a `bool`, mirroring `ui.checkbox` semantics
/// (`changed()` after a click) without drawing a boxed control inside a menu.
pub fn toggle(ui: &mut Ui, text: &str, on: &mut bool) -> Response {
    let mut resp = check(ui, text, *on);
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp
}

/// Same as [`toggle`], greyed out when `enabled` is false.
pub fn toggle_enabled(ui: &mut Ui, text: &str, on: &mut bool, enabled: bool) -> Response {
    let mut resp = check_enabled(ui, text, *on, enabled);
    if enabled && resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp
}

/// An entry that opens a submenu built by `content`.
pub fn submenu(ui: &mut Ui, text: &str, content: impl FnOnce(&mut Ui)) -> Response {
    // The open state must be known BEFORE painting so the parent entry stays
    // lit while its child menu is up; egui keys the submenu off this entry's
    // own widget id, which `next_auto_id` predicts.
    let my_id = ui.next_auto_id();
    let open = egui::containers::menu::MenuState::from_ui(ui, |state, _| {
        state.open_item == Some(egui::containers::menu::SubMenu::id_from_widget_id(my_id))
    });
    let resp = entry(
        ui,
        text,
        Marks {
            arrow: true,
            open,
            ..Default::default()
        },
    );
    // Keyboard: ArrowRight on the focused parent opens the submenu and hands
    // focus to its first enabled entry (the KB_INITIAL_FOCUS handoff consumed
    // by `entry`); ArrowLeft inside the open submenu — or on the parent —
    // closes it again and refocuses this entry. egui's spatial navigation
    // stays out of the way because `entry` locks horizontal arrows while a
    // menu entry is focused.
    let sub_id = egui::containers::menu::SubMenu::id_from_widget_id(resp.id);
    if ui.is_enabled() {
        if resp.has_focus() && !open && ui.input(|input| input.key_pressed(egui::Key::ArrowRight)) {
            egui::containers::menu::MenuState::from_ui(ui, |state, _| {
                state.open_item = Some(sub_id);
            });
            // `MenuState::from_id` self-heals an open item away when its
            // submenu has never rendered (no fresh MenuState of its own), so
            // the state just set would be wiped before `SubMenu::show` reads
            // it. Mark the submenu shown to carry the entry over to the
            // popup that renders this very frame.
            egui::containers::menu::MenuState::mark_shown(ui.ctx(), sub_id);
            ui.ctx()
                .data_mut(|d| d.insert_temp(kb_initial_focus_key(), sub_id));
        }
        if ui.input(|input| input.key_pressed(egui::Key::ArrowLeft)) {
            let focused_inside = open
                && ui.ctx().memory(|mem| mem.focused()).is_some_and(|id| {
                    ui.ctx()
                        .read_response(id)
                        .is_some_and(|focused| focused.layer_id.id == sub_id)
                });
            if focused_inside || resp.has_focus() {
                egui::containers::menu::MenuState::from_ui(ui, |state, _| {
                    state.open_item = None;
                });
                resp.request_focus();
            }
        }
    }
    egui::containers::menu::SubMenu::new().show(ui, &resp, content);
    // Mirror the root-level cleanup for the submenu's own handoff: when the
    // submenu (or the whole menu tree) closed, a pending id nothing consumed
    // must not survive it.
    drop_stale_kb_handoff(ui.ctx());
    resp
}

/// A full-width divider between groups of entries.
pub fn separator(ui: &mut Ui) {
    let color = hairline(ui.visuals().window_fill, theme::palette(ui).text);
    let (rect, _) = ui.allocate_at_least(egui::vec2(GUTTER_W, SEP_H), Sense::hover());
    let y = rect.center().y.round() + 0.5;
    ui.painter().line_segment(
        [
            Pos2::new(rect.left() + 8.0, y),
            Pos2::new(rect.right() - 8.0, y),
        ],
        Stroke::new(1.0, color),
    );
}

/// Hairline colour for rules drawn ON the popup surface. `pal.stroke` is mixed
/// against the window background, which is several shades darker than the
/// popup: on dark it lands two values off the fill (0x2d on 0x2b) and the rule
/// reads as a dead gap. Deriving it from the surface keeps one visible
/// hairline in both themes.
fn hairline(popup: Color32, text: Color32) -> Color32 {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * 0.11).round() as u8;
    Color32::from_rgb(
        mix(popup.r(), text.r()),
        mix(popup.g(), text.g()),
        mix(popup.b(), text.b()),
    )
}

/// A non-interactive caption row naming the subject of the menu (the process
/// it was opened on).
///
/// Full-strength text over the bare popup surface, closed off by the
/// [`separator`] its callers draw underneath. Every painted shape tried here —
/// an outlined chip, a filled band — borrowed the look of a control (a text
/// field, a selected row) and read as more clickable than the plain label it
/// replaced; dimming the text instead only made the one line the user came to
/// read the hardest one to read. What marks the row as inert is that it never
/// lights up under the cursor and that a visible rule separates it from the
/// commands below.
pub fn title(ui: &mut Ui, text: &str) {
    let pal = theme::palette(ui);
    let want = egui::vec2(desired_width(ui, text, Marks::default()), ITEM_H);
    let (rect, _) = ui.allocate_at_least(want, Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left() + GUTTER_W, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::proportional(FONT_SIZE + 0.5),
        pal.text,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Align, Layout, Rect, UiBuilder};

    /// Every entry in one menu must be the same height, sit flush against its
    /// neighbours (no dead gap) and span the full menu width — the three
    /// things egui's default menu style got wrong here. In particular this
    /// pins `allocate_at_least`: the "exact" variant silently un-justifies
    /// every row back to its own label width.
    #[test]
    fn entries_are_uniform_full_width_and_gapless() {
        let ctx = egui::Context::default();
        let rects: std::cell::RefCell<Vec<Rect>> = std::cell::RefCell::new(Vec::new());
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |root| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(root, |ui| {
                    style(ui.style_mut());
                    ui.scope_builder(
                        UiBuilder::new().layout(Layout::top_down_justified(Align::Min)),
                        |ui| {
                            rects.borrow_mut().push(item(ui, "Short").rect);
                            rects
                                .borrow_mut()
                                .push(item(ui, "A considerably longer entry").rect);
                            rects.borrow_mut().push(check(ui, "Ticked", true).rect);
                            separator(ui);
                            rects
                                .borrow_mut()
                                .push(submenu(ui, "Priority", |_| {}).rect);
                        },
                    );
                });
        });
        out.textures_delta.clear();

        let rects = rects.into_inner();
        assert_eq!(rects.len(), 4);
        for r in &rects {
            assert!(
                (r.height() - ITEM_H).abs() < 0.51,
                "entry height {} is not {ITEM_H}",
                r.height()
            );
        }
        let widths: Vec<f32> = rects.iter().map(|r| r.width()).collect();
        assert!(
            widths.windows(2).all(|w| (w[0] - w[1]).abs() < 0.51),
            "entries are not equally wide: {widths:?}"
        );
        // The first three are consecutive; the fourth follows a separator.
        for pair in rects[..3].windows(2) {
            assert!(
                (pair[1].top() - pair[0].bottom()).abs() < 0.51,
                "gap of {} px between entries",
                pair[1].top() - pair[0].bottom()
            );
        }
        assert!(
            (rects[3].top() - rects[2].bottom() - SEP_H).abs() < 0.51,
            "separator does not occupy exactly its own row"
        );
    }

    #[test]
    fn context_menu_kb_opens_and_stays_open() {
        let ctx = egui::Context::default();
        let raw1 = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
            ..Default::default()
        };
        let mut opened_frame_1 = false;
        let mut opened_frame_2 = false;
        let mut opened_frame_3 = false;

        let mut out1 = ctx.run_ui(raw1, |ui| {
            let resp = ui.label("row");
            context_menu_kb(&resp, true, |ui| {
                opened_frame_1 = true;
                item(ui, "Action 1");
            });
        });
        out1.textures_delta.clear();

        let raw2 = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
            ..Default::default()
        };
        let mut out2 = ctx.run_ui(raw2, |ui| {
            let resp = ui.label("row");
            context_menu_kb(&resp, false, |ui| {
                opened_frame_2 = true;
                item(ui, "Action 1");
            });
        });
        out2.textures_delta.clear();

        let raw3 = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
            ..Default::default()
        };
        let mut out3 = ctx.run_ui(raw3, |ui| {
            let resp = ui.label("row");
            context_menu_kb(&resp, false, |ui| {
                opened_frame_3 = true;
                item(ui, "Action 1");
            });
        });
        out3.textures_delta.clear();

        assert!(opened_frame_1, "frame 1 should be opened");
        assert!(opened_frame_2, "frame 2 should be opened");
        assert!(opened_frame_3, "frame 3 should be opened");

        // Frame 4: Escape key closes the context menu.
        let mut opened_frame_4 = false;
        let raw4 = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };
        let mut out4 = ctx.run_ui(raw4, |ui| {
            let resp = ui.label("row");
            context_menu_kb(&resp, false, |_ui| {
                opened_frame_4 = true;
            });
        });
        out4.textures_delta.clear();

        // Frame 5: Should now be closed.
        let mut opened_frame_5 = false;
        let raw5 = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
            ..Default::default()
        };
        let mut out5 = ctx.run_ui(raw5, |ui| {
            let resp = ui.label("row");
            context_menu_kb(&resp, false, |_ui| {
                opened_frame_5 = true;
            });
        });
        out5.textures_delta.clear();

        assert!(!opened_frame_5, "menu should be closed after Escape");
    }

    #[test]
    fn keyboard_opened_menu_focuses_and_navigates() {
        fn paint(ui: &mut Ui, focus: &mut [bool; 3], activated: &mut bool) {
            let a = item(ui, "Action 1");
            let b = item(ui, "Action 2");
            let c = item(ui, "Action 3");
            focus[0] = a.has_focus();
            focus[1] = b.has_focus();
            focus[2] = c.has_focus();
            if b.clicked() {
                *activated = true;
            }
        }
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        // `focused: true` matters: `Response::has_focus` also requires the
        // WINDOW to have OS focus, which headless input does not claim by
        // default.
        let raw = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            events,
            focused: true,
            ..Default::default()
        };
        let key = |key: egui::Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };
        let run = |raw: egui::RawInput,
                   keyboard_open: bool,
                   f: &mut [bool; 3],
                   act: &mut bool|
         -> egui::FullOutput {
            ctx.run_ui(raw, |ui| {
                let resp = ui.label("row");
                context_menu_kb(&resp, keyboard_open, |ui| paint(ui, f, act));
            })
        };

        // Frame 1: keyboard-open. Focus applies from the NEXT frame on.
        let (mut f, mut act) = ([false; 3], false);
        let mut out = run(raw(vec![]), true, &mut f, &mut act);
        out.textures_delta.clear();
        assert!(
            !f[0] && !f[1] && !f[2],
            "frame 1 paints before focus applies"
        );

        // Frame 2: the first entry owns the focus.
        let mut out = run(raw(vec![]), false, &mut f, &mut act);
        out.textures_delta.clear();
        assert!(f[0] && !f[1] && !f[2], "first entry must hold focus: {f:?}");

        // Frame 3: ArrowDown is READ this frame (focus still on entry 1)...
        let mut out = run(
            raw(vec![key(egui::Key::ArrowDown)]),
            false,
            &mut f,
            &mut act,
        );
        out.textures_delta.clear();
        assert!(f[0], "focus must not jump mid-frame");

        // Frame 4: ...and the second entry owns it now.
        let mut out = run(raw(vec![]), false, &mut f, &mut act);
        out.textures_delta.clear();
        assert!(
            f[1] && !f[0] && !f[2],
            "ArrowDown must move focus down: {f:?}"
        );

        // Frame 5: Enter activates the focused entry.
        let mut out = run(raw(vec![key(egui::Key::Enter)]), false, &mut f, &mut act);
        out.textures_delta.clear();
        assert!(act, "Enter must activate the focused entry");
    }

    #[test]
    fn keyboard_menu_requested_triggers() {
        let ctx = egui::Context::default();

        // 1. ContextMenu key triggers
        let raw = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::ContextMenu,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };
        let mut triggered = false;
        let mut out = ctx.run_ui(raw, |ui| {
            triggered = keyboard_menu_requested(ui.ctx());
        });
        out.textures_delta.clear();
        assert!(triggered, "ContextMenu key should trigger keyboard menu");

        // 2. Shift+F10 triggers
        let raw = egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(egui::Modifiers {
                    shift: true,
                    ..Default::default()
                }),
                egui::Event::Key {
                    key: egui::Key::F10,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                },
            ],
            ..Default::default()
        };
        let mut triggered = false;
        let mut out = ctx.run_ui(raw, |ui| {
            triggered = keyboard_menu_requested(ui.ctx());
        });
        out.textures_delta.clear();
        assert!(triggered, "Shift+F10 should trigger keyboard menu");

        // 3. F10 without shift does NOT trigger
        let raw = egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(egui::Modifiers::default()),
                egui::Event::Key {
                    key: egui::Key::F10,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            ..Default::default()
        };
        let mut triggered = false;
        let mut out = ctx.run_ui(raw, |ui| {
            triggered = keyboard_menu_requested(ui.ctx());
        });
        out.textures_delta.clear();
        assert!(!triggered, "Plain F10 should not trigger keyboard menu");
    }

    /// The Menu key must keep working while a non-text widget holds focus
    /// (that is how a keyboard user opens the selection's menu after Tab
    /// navigation); only a focused text edit keeps it for the field.
    #[test]
    fn keyboard_menu_requested_gate_only_blocks_text_edits() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let raw = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            events,
            focused: true,
            ..Default::default()
        };
        let menu_key = || {
            vec![egui::Event::Key {
                key: egui::Key::ContextMenu,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }]
        };

        // A focused button (e.g. a table row reached with Tab).
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            ui.button("Toolbar").request_focus();
        });
        out.textures_delta.clear();
        let mut triggered = false;
        let mut out = ctx.run_ui(raw(menu_key()), |ui| {
            triggered = keyboard_menu_requested(ui.ctx());
            let _ = ui.button("Toolbar");
        });
        out.textures_delta.clear();
        assert!(
            triggered,
            "the Menu key must work while a button holds focus"
        );

        // While a text edit owns the keyboard, the key belongs to the field.
        let mut text = String::new();
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            ui.text_edit_singleline(&mut text).request_focus();
        });
        out.textures_delta.clear();
        let mut triggered = false;
        let mut out = ctx.run_ui(raw(menu_key()), |ui| {
            triggered = keyboard_menu_requested(ui.ctx());
            ui.text_edit_singleline(&mut text);
        });
        out.textures_delta.clear();
        assert!(!triggered, "a focused text edit must keep the Menu key");
    }

    /// Tab while a menu popup is open closes the menu instead of stranding an
    /// orphaned popup; egui's own focus traversal proceeds untouched.
    #[test]
    fn tab_closes_an_open_menu() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let raw = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            events,
            focused: true,
            ..Default::default()
        };
        let key = |key: egui::Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };

        let frame = |keyboard_open: bool, events: Vec<egui::Event>, menu_open: &mut bool| {
            *menu_open = false;
            let mut out = ctx.run_ui(raw(events), |ui| {
                let resp = ui.label("row");
                context_menu_kb(&resp, keyboard_open, |ui| {
                    *menu_open = true;
                    item(ui, "Action 1");
                });
            });
            out.textures_delta.clear();
        };

        let mut menu_open = false;
        // Keyboard-open the menu and let focus settle on the entry.
        frame(true, vec![], &mut menu_open);
        assert!(menu_open, "keyboard-open must open the menu");
        frame(false, vec![], &mut menu_open);
        assert!(menu_open);

        // The frame Tab is pressed still paints the closing menu...
        frame(false, vec![key(egui::Key::Tab)], &mut menu_open);
        assert!(menu_open, "the closing frame still runs the menu content");
        // ...and the menu is gone afterwards.
        frame(false, vec![], &mut menu_open);
        assert!(!menu_open, "Tab must close the menu");
    }

    /// ArrowRight on a focused submenu parent opens the submenu and focuses
    /// its first entry; ArrowLeft inside the submenu closes it again, keeps
    /// the parent menu open and refocuses the parent entry.
    #[test]
    fn arrow_right_opens_submenu_and_arrow_left_returns_to_the_parent() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0));
        let raw = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            events,
            focused: true,
            ..Default::default()
        };
        let key = |key: egui::Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };

        struct Frame {
            root_open: bool,
            sub_open: bool,
            parent_focused: bool,
            sub_focused: bool,
        }
        let frame = |keyboard_open: bool, events: Vec<egui::Event>| {
            let mut f = Frame {
                root_open: false,
                sub_open: false,
                parent_focused: false,
                sub_focused: false,
            };
            let mut out = ctx.run_ui(raw(events), |ui| {
                let resp = ui.label("row");
                context_menu_kb(&resp, keyboard_open, |ui| {
                    f.root_open = true;
                    let parent = submenu(ui, "Priority", |ui| {
                        f.sub_open = true;
                        let high = item(ui, "High");
                        item(ui, "Low");
                        f.sub_focused = high.has_focus();
                    });
                    f.parent_focused = parent.has_focus();
                });
            });
            out.textures_delta.clear();
            f
        };

        // Frame 1: keyboard-open; the popup's first frame is a sizing pass.
        let f = frame(true, vec![]);
        assert!(f.root_open && !f.sub_open && !f.parent_focused && !f.sub_focused);

        // Frame 2: the first entry — the submenu parent — owns the focus.
        let f = frame(false, vec![]);
        assert!(f.parent_focused && !f.sub_focused, "parent must be focused");

        // Frame 3: ArrowRight opens the submenu in the same frame.
        let f = frame(false, vec![key(egui::Key::ArrowRight)]);
        assert!(f.sub_open, "ArrowRight must open the submenu");
        assert!(f.parent_focused, "focus has not moved yet");

        // Frame 4: the submenu's first entry owns the focus.
        let f = frame(false, vec![]);
        assert!(f.sub_open && f.sub_focused && !f.parent_focused);

        // Frame 5: ArrowLeft closes the submenu in the same frame and hands
        // focus back to the parent entry.
        let f = frame(false, vec![key(egui::Key::ArrowLeft)]);
        assert!(!f.sub_open, "ArrowLeft must close the submenu");
        assert!(
            f.root_open,
            "the parent menu must survive the submenu closing"
        );
        assert!(f.parent_focused, "the parent entry must be refocused");

        // Frame 6: the state sticks — submenu closed, root menu still up.
        let f = frame(false, vec![]);
        assert!(f.root_open && !f.sub_open && f.parent_focused && !f.sub_focused);
    }
}

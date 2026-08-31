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
pub fn context_menu(resp: &Response, add: impl FnOnce(&mut Ui)) {
    egui::Popup::context_menu(resp).style(style).show(add);
}

/// A drop-down button in the app chrome that opens a menu in the same style.
pub fn menu_button(
    ui: &mut Ui,
    button: egui::Button<'_>,
    content: impl FnOnce(&mut Ui),
) -> Response {
    egui::containers::menu::MenuButton::from_button(button)
        .config(egui::containers::menu::MenuConfig::new().style(style))
        .ui(ui, content)
        .0
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

    let fill = if !enabled {
        None
    } else if resp.is_pointer_button_down_on() {
        Some(pressed_fill(&pal))
    } else if resp.hovered() || marks.open {
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
    egui::containers::menu::SubMenu::new().show(ui, &resp, content);
    resp
}

/// A full-width divider between groups of entries.
pub fn separator(ui: &mut Ui) {
    let pal = theme::palette(ui);
    let (rect, _) = ui.allocate_at_least(egui::vec2(GUTTER_W, SEP_H), Sense::hover());
    let y = rect.center().y.round() + 0.5;
    ui.painter().line_segment(
        [
            Pos2::new(rect.left() + 8.0, y),
            Pos2::new(rect.right() - 8.0, y),
        ],
        Stroke::new(1.0, pal.stroke),
    );
}

/// A non-interactive caption row naming the subject of the menu (the process
/// it was opened on).
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
}

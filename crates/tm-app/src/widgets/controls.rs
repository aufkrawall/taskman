//! Win11-style controls painted by hand so their look never depends on
//! egui's state-derived widget styles (which left checkboxes invisible in
//! dark mode until the first hover).

use eframe::egui::{self, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Vec2};

use crate::theme::Palette;

/// A Task-Manager-style checkbox: 18×18 rounded box, 1 px border; checked =
/// accent fill with a white check mark; disabled = dimmed. Fully painted
/// here, so it renders identically on the first frame — no hover required.
pub fn checkbox(
    ui: &mut egui::Ui,
    checked: &mut bool,
    text: &str,
    pal: &Palette,
) -> egui::Response {
    checkbox_enabled(ui, checked, text, true, pal)
}

/// Disabled variant (grayed out, not clickable).
pub fn checkbox_enabled(
    ui: &mut egui::Ui,
    checked: &mut bool,
    text: &str,
    enabled: bool,
    pal: &Palette,
) -> egui::Response {
    const BOX: f32 = 18.0;
    let spacing = ui.spacing().item_spacing.x;
    let text_w = if text.is_empty() {
        0.0
    } else {
        ui.painter()
            .layout_no_wrap(text.to_owned(), FontId::proportional(12.5), Color32::WHITE)
            .size()
            .x
    };
    let w = if text.is_empty() {
        BOX
    } else {
        BOX + spacing + text_w
    };
    let h = BOX.max(ui.spacing().interact_size.y);
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());

    let box_rect = Rect::from_center_size(
        Pos2::new(rect.left() + BOX / 2.0, rect.center().y),
        Vec2::splat(BOX),
    );

    if enabled && resp.clicked() {
        *checked = !*checked;
        resp.mark_changed();
    }

    let hovered = enabled && resp.hovered();
    let pressed = enabled && resp.is_pointer_button_down_on();

    let (fill, border) = if *checked {
        (
            if pressed {
                pal.accent.gamma_multiply(0.75)
            } else {
                pal.accent
            },
            Stroke::NONE,
        )
    } else if pressed {
        (
            Color32::from_rgba_premultiplied(0, 0, 0, 20),
            Stroke::new(1.0, pal.text_dim),
        )
    } else if hovered {
        (
            Color32::from_rgba_premultiplied(255, 255, 255, 14),
            Stroke::new(1.0, pal.text),
        )
    } else {
        (
            Color32::from_rgba_premultiplied(0, 0, 0, 12),
            Stroke::new(1.0, pal.text_dim),
        )
    };
    let fill = if enabled {
        fill
    } else {
        Color32::from_rgba_premultiplied(0, 0, 0, 8)
    };

    let radius = CornerRadius::same(4);
    let p = ui.painter();
    p.rect_filled(box_rect, radius, fill);
    if border != Stroke::NONE {
        p.rect_stroke(box_rect, radius, border, egui::StrokeKind::Inside);
    }

    if *checked {
        // Check mark, white on accent (dim when disabled).
        let c = box_rect.center();
        let s = 4.5f32;
        let a = Pos2::new(c.x - s, c.y + 0.5);
        let b = Pos2::new(c.x - s * 0.25, c.y + s * 0.75);
        let d = Pos2::new(c.x + s, c.y - s * 0.75);
        let color = if enabled {
            pal.accent_text
        } else {
            pal.text_dim
        };
        p.add(egui::Shape::line(vec![a, b, d], Stroke::new(2.0, color)));
    }

    if !text.is_empty() {
        let color = if enabled { pal.text } else { pal.text_dim };
        ui.painter().text(
            Pos2::new(box_rect.right() + spacing, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            FontId::proportional(12.5),
            color,
        );
    }

    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            enabled && ui.is_enabled(),
            *checked,
            text,
        )
    });
    resp
}

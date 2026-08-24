//! Custom chart widgets: filled rolling line charts like Windows Task
//! Manager's Performance tab, plus a compact per-core grid variant.

use eframe::egui::{self, Color32, Pos2, Response, Shape, Stroke, Vec2};

/// Sparkline painted into an explicit rect (no allocation) — used inside
/// hand-laid cards.
pub fn paint_sparkline(ui: &egui::Ui, rect: egui::Rect, samples: &[f64], color: Color32) {
    let painter = ui.painter_at(rect);
    let pal = crate::theme::palette(ui);

    painter.rect_filled(rect, 2.0, pal.card_bg);
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(0.75, pal.stroke),
        egui::StrokeKind::Inside,
    );

    if samples.len() < 2 {
        return;
    }

    let n = samples.len();
    let x = |i: usize| rect.left() + rect.width() * i as f32 / (n - 1) as f32;
    let y = |v: f64| rect.bottom() - (v.clamp(0.0, 100.0) / 100.0) as f32 * rect.height();

    let pts: Vec<Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| Pos2::new(x(i), y(*v)))
        .collect();

    let fill = Color32::from_rgba_premultiplied(color.r(), color.g(), color.b(), 55);
    let mut area = pts.clone();
    area.push(Pos2::new(pts[pts.len() - 1].x, rect.bottom()));
    area.push(Pos2::new(pts[0].x, rect.bottom()));
    painter.add(Shape::convex_polygon(area, fill, Stroke::NONE));
    painter.add(Shape::line(pts, Stroke::new(1.0, color)));
}

/// Per-logical-processor tile: bordered, faint horizontal grid, filled area.
pub fn core_chart(ui: &mut egui::Ui, size: Vec2, samples: &[f64], color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect.expand(1.0));
    let pal = crate::theme::palette(ui);

    painter.rect_filled(rect, 0.0, pal.window_bg);
    // Quarter gridlines.
    for k in 1..4 {
        let y = rect.top() + rect.height() * k as f32 / 4.0;
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, pal.chart_grid),
        );
    }
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(1.0, pal.stroke),
        egui::StrokeKind::Inside,
    );

    if samples.len() < 2 {
        return response;
    }
    let n = samples.len();
    let x = |i: usize| rect.left() + rect.width() * i as f32 / (n - 1) as f32;
    let y = |v: f64| rect.bottom() - (v.clamp(0.0, 100.0) / 100.0) as f32 * rect.height();
    let pts: Vec<Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| Pos2::new(x(i), y(*v)))
        .collect();
    let fill = Color32::from_rgba_premultiplied(color.r(), color.g(), color.b(), 60);
    let mut area = pts.clone();
    area.push(Pos2::new(rect.right(), rect.bottom()));
    area.push(Pos2::new(rect.left(), rect.bottom()));
    painter.add(Shape::convex_polygon(area, fill, Stroke::NONE));
    painter.add(Shape::line(pts, Stroke::new(1.2, color)));

    // Hover value.
    if let Some(pos) = ui.input(|i| i.pointer.hover_pos())
        && rect.contains(pos)
    {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let idx = ((frac * (n - 1) as f32).round()) as usize;
        if let Some(v) = samples.get(idx) {
            response.clone().on_hover_text(format!("{v:.1}%"));
        }
    }
    response
}

/// One owned series for [`chart_multi`].
pub struct MultiSeries {
    pub samples: Vec<f64>,
    pub color: Color32,
}

/// Bordered chart with several filled series sharing one y scale
/// (disk read+write, memory+commit, ...).
pub fn chart_multi(
    ui: &mut egui::Ui,
    size: Vec2,
    series: &[MultiSeries],
    y_max: f64,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect.expand(1.0));
    let pal = crate::theme::palette(ui);

    painter.rect_filled(rect, 0.0, pal.window_bg);
    for k in 1..4 {
        let y = rect.top() + rect.height() * k as f32 / 4.0;
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, pal.chart_grid),
        );
    }
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(1.0, pal.stroke),
        egui::StrokeKind::Inside,
    );

    let y = |v: f64| rect.bottom() - (v.clamp(0.0, y_max) / y_max) as f32 * rect.height();
    for s in series {
        let n = s.samples.len();
        if n < 2 {
            continue;
        }
        let pts: Vec<Pos2> = s
            .samples
            .iter()
            .enumerate()
            .map(|(i, v)| Pos2::new(rect.left() + rect.width() * i as f32 / (n - 1) as f32, y(*v)))
            .collect();
        let fill = Color32::from_rgba_premultiplied(s.color.r(), s.color.g(), s.color.b(), 45);
        let mut area = pts.clone();
        area.push(Pos2::new(rect.right(), rect.bottom()));
        area.push(Pos2::new(rect.left(), rect.bottom()));
        painter.add(Shape::convex_polygon(area, fill, Stroke::NONE));
        painter.add(Shape::line(pts, Stroke::new(1.2, s.color)));
    }

    // Hover: values of every series at the pointer.
    if let Some(pos) = ui.input(|i| i.pointer.hover_pos())
        && rect.contains(pos)
    {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let mut tip = String::new();
        for s in series {
            let n = s.samples.len();
            if n < 2 {
                continue;
            }
            let idx = ((frac * (n - 1) as f32).round()) as usize;
            if let Some(v) = s.samples.get(idx) {
                if !tip.is_empty() {
                    tip.push_str(" \u{b7} ");
                }
                tip.push_str(&format!("{v:.1}"));
            }
        }
        if !tip.is_empty() {
            response.clone().on_hover_text(tip);
        }
    }
    response
}

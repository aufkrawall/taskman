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
    painter.add(Shape::line(pts, Stroke::new(1.25, color)));
}

/// Per-logical-processor tile: bordered, faint horizontal grid, filled area.
/// With `kernels` (same length), the kernel-time share is overlaid darker —
/// Task Manager's "Show kernel times" (§14.4).
pub fn core_chart(
    ui: &mut egui::Ui,
    size: Vec2,
    samples: &[f64],
    kernels: Option<&[f64]>,
    color: Color32,
) -> Response {
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
    let fill = Color32::from_rgba_premultiplied(color.r(), color.g(), color.b(), 75);
    let mut area = pts.clone();
    area.push(Pos2::new(rect.right(), rect.bottom()));
    area.push(Pos2::new(rect.left(), rect.bottom()));
    painter.add(Shape::convex_polygon(area, fill, Stroke::NONE));

    // Kernel overlay: darker band under the user portion. Painted AFTER the
    // user fill but BEFORE the line, so it darkens the lower region without
    // burying the series line (the old order drew the line first).
    if let Some(k) = kernels {
        let kpts: Vec<Pos2> = k
            .iter()
            .enumerate()
            .map(|(i, v)| Pos2::new(x(i), y(*v)))
            .collect();
        let kfill =
            Color32::from_rgba_premultiplied(color.r() / 2, color.g() / 2, color.b() / 2, 110);
        let mut karea = kpts.clone();
        karea.push(Pos2::new(rect.right(), rect.bottom()));
        karea.push(Pos2::new(rect.left(), rect.bottom()));
        painter.add(Shape::convex_polygon(karea, kfill, Stroke::NONE));
    }
    painter.add(Shape::line(pts, Stroke::new(1.4, color)));

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
/// (disk read+write, memory+commit, ...). When `timestamps_ms` is given the
/// x positions follow real sample times, so delayed/irregular samples plot
/// at their true elapsed offset instead of compressing evenly (§14.3).
pub fn chart_multi(
    ui: &mut egui::Ui,
    size: Vec2,
    series: &[MultiSeries],
    y_max: f64,
    timestamps_ms: Option<&[u64]>,
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
    // Time-proportional x mapping over the shared window.
    let t_span = timestamps_ms.and_then(|ts| {
        let first = *ts.first()?;
        let last = *ts.last()?;
        Some((first, (last - first).max(1)))
    });
    let x_at = |i: usize, n: usize| match t_span {
        Some((t0, span)) => {
            let t = timestamps_ms.unwrap()[i].saturating_sub(t0);
            rect.left() + rect.width() * (t as f32 / span as f32).clamp(0.0, 1.0)
        }
        None => rect.left() + rect.width() * i as f32 / (n - 1) as f32,
    };
    for s in series {
        let n = s.samples.len();
        if n < 2 {
            continue;
        }
        let pts: Vec<Pos2> = s
            .samples
            .iter()
            .enumerate()
            .map(|(i, v)| Pos2::new(x_at(i, n), y(*v)))
            .collect();
        let fill = Color32::from_rgba_premultiplied(s.color.r(), s.color.g(), s.color.b(), 60);
        let mut area = pts.clone();
        area.push(Pos2::new(rect.right(), rect.bottom()));
        area.push(Pos2::new(rect.left(), rect.bottom()));
        painter.add(Shape::convex_polygon(area, fill, Stroke::NONE));
    }
    // Lines in a SECOND pass: an outer series' translucent fill must never
    // dim an inner series' line (the old single-pass order made overlapping
    // series muddy and hard to read).
    for s in series {
        let n = s.samples.len();
        if n < 2 {
            continue;
        }
        let pts: Vec<Pos2> = s
            .samples
            .iter()
            .enumerate()
            .map(|(i, v)| Pos2::new(x_at(i, n), y(*v)))
            .collect();
        painter.add(Shape::line(pts, Stroke::new(1.5, s.color)));
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

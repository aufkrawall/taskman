//! Custom chart widgets: filled rolling line charts like Windows Task
//! Manager's Performance tab, plus a compact per-core grid variant.

use eframe::egui::{self, Color32, Pos2, Rect, Response, Shape, Stroke, Vec2};
use tm_core::format::nice_scale;

/// One rendered chart series.
pub struct Series<'a> {
    pub samples: &'a [f64],
    pub color: Color32,
    pub label: String,
}

pub struct LineChart<'a> {
    pub series: Series<'a>,
    /// Fixed y maximum (e.g. 100 for percent). None = auto "nice" scale.
    pub y_max: Option<f64>,
    /// Formatter for y values (tooltip + axis labels).
    pub fmt: fn(f64) -> String,
    /// Show horizontal grid + right-edge labels.
    pub grid: bool,
}

impl<'a> LineChart<'a> {
    pub fn new(samples: &'a [f64], color: Color32, fmt: fn(f64) -> String) -> Self {
        Self {
            series: Series {
                samples,
                color,
                label: String::new(),
            },
            y_max: None,
            fmt,
            grid: true,
        }
    }

    pub fn y_max(mut self, m: f64) -> Self {
        self.y_max = Some(m);
        self
    }

    pub fn grid(mut self, on: bool) -> Self {
        self.grid = on;
        self
    }

    /// Full-area variant kept for embedded/dashboard use.
    #[allow(dead_code)]
    pub fn show(self, ui: &mut egui::Ui) -> Response {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
        self.paint(ui, rect);
        response
    }

    pub fn show_sized(self, ui: &mut egui::Ui, size: Vec2) -> Response {
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
        self.paint(ui, rect);
        response
    }

    fn paint(self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter_at(rect.expand(1.0));
        let pal = crate::theme::palette(ui);
        let samples = self.series.samples;
        let color = self.series.color;

        // Background card.
        painter.rect_filled(rect, 4.0, pal.card_bg);
        if !self.series.label.is_empty() {
            painter.text(
                Pos2::new(rect.right() - 6.0, rect.top() + 4.0),
                egui::Align2::RIGHT_TOP,
                &self.series.label,
                egui::FontId::proportional(10.5),
                pal.text_dim,
            );
        }
        painter.rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0, pal.stroke),
            egui::StrokeKind::Inside,
        );

        if samples.is_empty() {
            return;
        }

        let peak = samples.iter().cloned().fold(0.0f64, f64::max);
        let (y_max, step) = match self.y_max {
            Some(m) => (m, m / 4.0),
            None => nice_scale(peak),
        };

        // Grid.
        if self.grid {
            let mut y = 0.0f64;
            while y <= y_max * 1.001 {
                let fy = rect.bottom() - (y / y_max) as f32 * rect.height();
                painter.line_segment(
                    [Pos2::new(rect.left(), fy), Pos2::new(rect.right(), fy)],
                    Stroke::new(1.0, pal.chart_grid),
                );
                painter.text(
                    Pos2::new(rect.left() + 5.0, fy - 8.0),
                    egui::Align2::LEFT_TOP,
                    (self.fmt)(y),
                    egui::FontId::monospace(9.0),
                    pal.text_dim,
                );
                y += step;
            }
        }

        // Area + line path.
        let n = samples.len().max(2);
        let x = |i: usize| rect.left() + rect.width() * i as f32 / (n - 1) as f32;
        let yy = |v: f64| rect.bottom() - (v.clamp(0.0, y_max) / y_max) as f32 * rect.height();

        let mut pts: Vec<Pos2> = samples
            .iter()
            .enumerate()
            .map(|(i, v)| Pos2::new(x(i), yy(*v)))
            .collect();

        if !pts.is_empty() {
            let fill_color = Color32::from_rgba_premultiplied(
                color.r(),
                color.g(),
                color.b(),
                match () {
                    _ if ui.visuals().dark_mode => 70,
                    _ => 60,
                },
            );
            let mut area_pts = pts.clone();
            area_pts.push(Pos2::new(pts[pts.len() - 1].x, rect.bottom()));
            area_pts.push(Pos2::new(pts[0].x, rect.bottom()));

            // Fill only when we have at least a triangle worth of points.
            if pts.len() >= 2 {
                painter.add(Shape::convex_polygon(area_pts, fill_color, Stroke::NONE));
            }

            // Thin the polyline when very dense to keep painting cheap.
            if pts.len() > 600 {
                let keep = pts.len() / 600 + 1;
                pts = pts.into_iter().step_by(keep).collect();
                if let Some(last) = samples.last() {
                    pts.push(Pos2::new(rect.right(), yy(*last)));
                }
            }
            painter.add(Shape::line(pts, Stroke::new(1.6, color)));

            // Current value marker on the right edge.
            let last = *samples.last().unwrap_or(&0.0);
            let ly = yy(last);
            painter.circle_filled(Pos2::new(rect.right() - 3.0, ly), 2.5, color);
        }

        // Hover crosshair + tooltip.
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos())
            && rect.contains(pos)
        {
            let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let idx = ((frac * (n - 1) as f32).round()) as usize;
            if let Some(v) = samples.get(idx) {
                let py = yy(*v);
                painter.line_segment(
                    [
                        Pos2::new(pos.x, rect.top()),
                        Pos2::new(pos.x, rect.bottom()),
                    ],
                    Stroke::new(1.0, pal.chart_grid.gamma_multiply(1.6)),
                );
                painter.circle_filled(Pos2::new(x(idx.min(n - 1)), py), 3.0, color);

                let text = format!("{} · {}s ago", (self.fmt)(*v), n.saturating_sub(idx + 1));
                painter.text(
                    Pos2::new(pos.x + 8.0, rect.top() + 4.0),
                    egui::Align2::LEFT_TOP,
                    text,
                    egui::FontId::proportional(11.0),
                    pal.text,
                );
            }
        }
    }
}

/// A tiny sparkline-style chart for the logical-processor grid.
pub fn mini_chart(ui: &mut egui::Ui, size: Vec2, samples: &[f64], color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect.expand(1.0));
    let pal = crate::theme::palette(ui);

    painter.rect_filled(rect, 2.0, pal.card_bg);
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(0.75, pal.stroke),
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

    let fill = Color32::from_rgba_premultiplied(color.r(), color.g(), color.b(), 55);
    let mut area = pts.clone();
    area.push(Pos2::new(pts[pts.len() - 1].x, rect.bottom()));
    area.push(Pos2::new(pts[0].x, rect.bottom()));
    painter.add(Shape::convex_polygon(area, fill, Stroke::NONE));
    painter.add(Shape::line(pts, Stroke::new(1.0, color)));

    // Hover shows exact value.
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

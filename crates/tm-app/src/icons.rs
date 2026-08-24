//! Small vector icons drawn with egui's painter — crisp at any DPI, zero
//! binary size, theme-aware stroke color.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // full glyph set kept for future tabs
pub enum Icon {
    Processes,
    Performance,
    History,
    Startup,
    Users,
    Details,
    Services,
    Settings,
    Cpu,
    Memory,
    Disk,
    Network,
    Gpu,
    Search,
    Close,
    ChevronRight,
    ChevronDown,
    Plus,
    Check,
}

/// Draw an icon centered in `rect`, stroked with `color`.
pub fn draw(ui: &egui::Ui, icon: Icon, rect: Rect, color: Color32) {
    let painter = ui.painter();
    let c = rect.center();
    let s = rect.size().min_elem() * 0.5; // half-size for geometry
    let stroke = Stroke::new((rect.size().min_elem() * 0.055).max(1.0), color);

    let line = |a: Pos2, b: Pos2| painter.line_segment([a, b], stroke);
    let circle = |center: Pos2, r: f32, filled: bool| {
        if filled {
            painter.circle_filled(center, r, color)
        } else {
            painter.circle_stroke(center, r, stroke)
        }
    };
    let p = |dx: f32, dy: f32| Pos2::new(c.x + dx * s / 10.0, c.y + dy * s / 10.0);

    match icon {
        Icon::Processes => {
            // Stacked list rows.
            for dy in [-6.0f32, 0.0, 6.0] {
                line(p(-8.0, dy), p(8.0, dy));
            }
            circle(p(-8.0, -6.0), 1.4, true);
            circle(p(-8.0, 0.0), 1.4, true);
            circle(p(-8.0, 6.0), 1.4, true);
        }
        Icon::Performance => {
            // Pulse/heartbeat line.
            let pts = [
                p(-9.0, 0.0),
                p(-4.0, 0.0),
                p(-2.0, -7.0),
                p(1.0, 7.0),
                p(3.5, 0.0),
                p(9.0, 0.0),
            ];
            painter.add(egui::Shape::line(pts.to_vec(), stroke));
        }
        Icon::History => {
            circle(c, 8.0 * s / 10.0, false);
            line(p(0.0, -5.0), p(0.0, 0.5));
            line(p(0.0, 0.5), p(4.5, 3.0));
        }
        Icon::Startup => {
            // Rocket-ish arrow out of box.
            line(p(-8.0, -8.0), p(-8.0, 8.0));
            line(p(-8.0, 8.0), p(8.0, 8.0));
            line(p(-4.0, 4.0), p(6.0, -6.0));
            line(p(6.0, -6.0), p(6.0, -1.0));
            line(p(6.0, -6.0), p(1.0, -6.0));
        }
        Icon::Users => {
            circle(p(-3.0, -3.0), 3.5, false);
            // shoulders as two short strokes
            line(p(-7.5, 8.0), p(-5.0, 5.0));
            line(p(1.5, 5.0), p(4.0, 8.0));
            circle(p(5.5, -3.5), 3.0, false);
        }
        Icon::Details => {
            // Three horizontal sliders.
            for (dy, kx) in [(-5.0f32, 2.0), (0.0, -3.0), (5.0, 4.0)] {
                line(p(-8.0, dy), p(8.0, dy));
                circle(p(kx, dy), 2.0, true);
            }
        }
        Icon::Services => {
            // Gear simplified: circle + 6 teeth.
            circle(c, 4.5, false);
            circle(c, 1.6, true);
            for a in 0..6 {
                let angle = a as f32 * std::f32::consts::TAU / 6.0;
                let dir = Vec2::new(angle.cos(), angle.sin());
                let a1 = c + dir * 5.5 * s / 10.0;
                let a2 = c + dir * 8.5 * s / 10.0;
                painter.line_segment([a1, a2], stroke);
            }
        }
        Icon::Settings => draw(ui, Icon::Services, rect, color),
        Icon::Cpu => {
            // Chip.
            painter.rect_stroke(
                Rect::from_min_max(p(-5.0, -5.0), p(5.0, 5.0)),
                1.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            for i in [-3.0, 0.0, 3.0] {
                line(p(i, -5.0), p(i, -8.0));
                line(p(i, 5.0), p(i, 8.0));
                line(p(-5.0, i), p(-8.0, i));
                line(p(5.0, i), p(8.0, i));
            }
        }
        Icon::Memory => {
            // Memory stick.
            painter.rect_stroke(
                Rect::from_min_max(p(-9.0, -4.0), p(9.0, 4.0)),
                1.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            for i in [-6.0, -2.0, 2.0, 6.0] {
                line(p(i, -4.0), p(i, 1.0));
                line(p(i, 4.0), p(i, 6.5));
            }
        }
        Icon::Disk => {
            // Drive cylinder simplified to rounded rect + dot.
            painter.rect_stroke(
                Rect::from_min_max(p(-8.0, -5.0), p(8.0, 5.0)),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            circle(p(4.5, 1.5), 1.5, true);
            line(p(-5.0, -1.5), p(1.0, -1.5));
        }
        Icon::Network => {
            // Two nodes + link.
            circle(p(-6.0, -5.0), 3.0, false);
            circle(p(6.0, 5.0), 3.0, false);
            line(p(-4.0, -3.5), p(4.0, 3.5));
        }
        Icon::Gpu => {
            painter.rect_stroke(
                Rect::from_min_max(p(-9.0, -5.0), p(9.0, 5.0)),
                1.5,
                stroke,
                egui::StrokeKind::Inside,
            );
            circle(p(-3.0, 0.0), 2.5, false);
            circle(p(3.5, 0.0), 2.5, false);
            line(p(-9.0, 5.0), p(-9.0, 8.0));
            line(p(9.0, 5.0), p(9.0, 8.0));
        }
        Icon::Search => {
            circle(p(-2.0, -2.0), 5.0, false);
            line(p(1.5, 1.5), p(7.0, 7.0));
        }
        Icon::Close => {
            line(p(-5.0, -5.0), p(5.0, 5.0));
            line(p(5.0, -5.0), p(-5.0, 5.0));
        }
        Icon::ChevronRight => {
            line(p(-3.0, -6.0), p(4.0, 0.0));
            line(p(4.0, 0.0), p(-3.0, 6.0));
        }
        Icon::ChevronDown => {
            line(p(-6.0, -3.0), p(0.0, 4.0));
            line(p(0.0, 4.0), p(6.0, -3.0));
        }
        Icon::Plus => {
            line(p(0.0, -6.0), p(0.0, 6.0));
            line(p(-6.0, 0.0), p(6.0, 0.0));
        }
        Icon::Check => {
            line(p(-6.0, 0.0), p(-1.5, 5.0));
            line(p(-1.5, 5.0), p(6.5, -5.0));
        }
    }
}

/// Draw an icon at an absolute rect (convenience wrapper).
pub fn draw_at(ui: &egui::Ui, rect: egui::Rect, icon: Icon, color: Color32) {
    draw(ui, icon, rect, color);
}

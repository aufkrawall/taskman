//! Task-Manager-style table building blocks: bordered two-line headers with
//! aggregate values, full-row selection, blue heat-mapped value cells,
//! chevrons and sort carets.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke};
use tm_core::format;

use crate::icons;
use crate::theme::Palette;

/// Row height used by all TM tables.
pub const ROW_H: f32 = 33.0;
/// Header height for tables with aggregates (two-line).
pub const HEADER_H: f32 = 56.0;
/// Header height for single-line headers (Details/Services/Startup).
pub const HEADER_H1: f32 = 30.0;

#[derive(Debug, Clone)]
pub struct TmColumn {
    pub id: &'static str,
    pub label: &'static str,
    pub width: f32,
    /// Numeric columns get a right-aligned two-line header (aggregate on top).
    pub numeric: bool,
}

impl TmColumn {
    pub const fn text(id: &'static str, label: &'static str, width: f32) -> Self {
        Self { id, label, width, numeric: false }
    }
    pub const fn num(id: &'static str, label: &'static str, width: f32) -> Self {
        Self { id, label, width, numeric: true }
    }
}

pub struct TmTable {
    pub cols: Vec<TmColumn>,
    /// Minimum width of the flexible first (name) column.
    pub name_min: f32,
}

impl TmTable {
    pub fn new(cols: Vec<TmColumn>, name_min: f32) -> Self {
        Self { cols, name_min }
    }

    /// Resolved width of the name (first) column given the available width.
    pub fn name_width(&self, avail: f32) -> f32 {
        let fixed: f32 = self.cols[1..].iter().map(|c| c.width).sum();
        (avail - fixed).max(self.name_min)
    }

    pub fn total_width(&self, avail: f32) -> f32 {
        self.name_width(avail) + self.cols[1..].iter().map(|c| c.width).sum::<f32>()
    }

    pub fn col_rect(&self, i: usize, avail: f32, row: Rect) -> Rect {
        let name_w = self.name_width(avail);
        let mut x = row.left();
        for (ci, c) in self.cols.iter().enumerate() {
            let w = if ci == 0 { name_w } else { c.width };
            if ci == i {
                return Rect::from_min_max(Pos2::new(x, row.top()), Pos2::new(x + w, row.bottom()));
            }
            x += w;
        }
        row
    }

    fn numeric_span(&self, avail: f32, row: Rect, from: usize) -> Rect {
        let left = self.col_rect(from, avail, row).left();
        Rect::from_min_max(Pos2::new(left, row.top()), Pos2::new(row.right(), row.bottom()))
    }

    /// Paint the header. Returns the clicked column index.
    pub fn header(
        &self,
        ui: &mut egui::Ui,
        pal: &Palette,
        avail: f32,
        sort: Option<(usize, bool)>,
        aggregates: Option<&[String]>,
    ) -> Option<usize> {
        let h = if aggregates.is_some() { HEADER_H } else { HEADER_H1 };
        let total_w = self.total_width(avail);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(total_w, h), Sense::hover());
        let painter = ui.painter_at(rect.expand(2.0));
        let name_w = self.name_width(avail);
        let mut clicked = None;
        let mut x = rect.left();
        let mut agg_idx = 0usize;

        for (i, col) in self.cols.iter().enumerate() {
            let w = if i == 0 { name_w } else { col.width };
            let cell = Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(x + w, rect.bottom()));

            // Hover + click.
            let resp = ui.interact(cell, egui::Id::new(("hdr", col.id)), Sense::click());
            if resp.hovered() {
                painter.rect_filled(cell, 0.0, Color32::from_white_alpha(6));
            }
            if resp.clicked() {
                clicked = Some(i);
            }

            // Vertical separators between header cells + bottom border.
            if i > 0 {
                painter.line_segment(
                    [Pos2::new(x, rect.top() + 4.0), Pos2::new(x, rect.bottom() - 4.0)],
                    Stroke::new(1.0, pal.stroke),
                );
            }

            let two_line = aggregates.is_some() && col.numeric;
            let label_y = if two_line { cell.bottom() - 14.0 } else { cell.center().y };
            let align = if col.numeric { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER };
            let tx = if col.numeric { cell.right() - 10.0 } else { cell.left() + 10.0 };
            if col.numeric {
                agg_idx += 1;
            }

            // Aggregate value above numeric labels (agg_idx-1 indexes the
            // aggregates slice for this numeric column).
            if two_line
                && agg_idx > 0
                && let Some(agg) = aggregates.and_then(|a| a.get(agg_idx - 1))
            {
                painter.text(
                    Pos2::new(cell.right() - 10.0, cell.top() + 14.0),
                    Align2::RIGHT_CENTER,
                    agg,
                    FontId::proportional(12.5),
                    pal.text,
                );
            }

            // Sort caret above the label of the sorted column.
            if let Some((si, asc)) = sort
                && si == i {
                    let cx = if col.numeric { cell.right() - 16.0 } else { cell.center().x };
                    let cy = if two_line { cell.top() + 14.0 } else { cell.top() + 10.0 };
                    if two_line
                        && agg_idx > 0
                        && let Some(agg) = aggregates.and_then(|a| a.get(agg_idx - 1))
                    {
                        painter.text(
                            Pos2::new(cell.right() - 26.0, cell.top() + 14.0),
                            Align2::RIGHT_CENTER,
                            agg,
                            FontId::proportional(12.5),
                            pal.text,
                        );
                    }
                    caret(painter.clone(), Pos2::new(cx, cy), asc, pal.text_dim);
                }

            painter.text(
                Pos2::new(tx, label_y),
                align,
                col.label,
                FontId::proportional(12.5),
                pal.text_dim,
            );
            x += w;
        }

        // Bottom border of the header + box around the name cell (TM look).
        painter.line_segment(
            [Pos2::new(rect.left(), rect.bottom()), Pos2::new(rect.right(), rect.bottom())],
            Stroke::new(1.0, pal.stroke),
        );
        painter.rect_stroke(
            self.col_rect(0, avail, rect).shrink(0.5),
            0.0,
            Stroke::new(1.0, pal.stroke),
            egui::StrokeKind::Inside,
        );
        clicked
    }

    /// Allocate + paint a row background. Returns the row rect and response.
    pub fn row(
        &self,
        ui: &mut egui::Ui,
        pal: &Palette,
        avail: f32,
        selected: bool,
    ) -> (Rect, egui::Response) {
        let total_w = self.total_width(avail);
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(total_w, ROW_H),
            Sense::click().union(Sense::hover()),
        );
        let painter = ui.painter_at(rect.expand(2.0));
        if selected {
            painter.rect_filled(rect, 0.0, pal.accent.gamma_multiply(0.22));
        } else if resp.hovered() {
            painter.rect_filled(rect, 0.0, Color32::from_white_alpha(8));
        }
        (rect, resp)
    }

    /// Left-aligned text cell.
    #[allow(clippy::too_many_arguments)]
    pub fn text_cell(
        &self,
        ui: &egui::Ui,
        avail: f32,
        row: Rect,
        i: usize,
        text: &str,
        pal: &Palette,
        dim: bool,
    ) {
        let cell = self.col_rect(i, avail, row);
        ui.painter().text(
            Pos2::new(cell.left() + 10.0, cell.center().y),
            Align2::LEFT_CENTER,
            text,
            FontId::proportional(12.5),
            if dim { pal.text_dim } else { pal.text },
        );
    }

    /// The contiguous blue heat block over numeric columns `from..`:
    /// base navy when the row is active, per-cell brighter blue by intensity.
    #[allow(clippy::too_many_arguments)]
    pub fn heat_cells(
        &self,
        ui: &egui::Ui,
        pal: &Palette,
        avail: f32,
        row: Rect,
        from: usize,
        cells: &[(f32, String)],
        row_active: bool,
    ) {
        let span = self.numeric_span(avail, row, from);
        let painter = ui.painter_at(row.expand(2.0));
        if row_active {
            painter.rect_filled(span, 0.0, pal.heat_base);
        }
        for (k, (t, text)) in cells.iter().enumerate() {
            let cell = self.col_rect(from + k, avail, row);
            if row_active && *t > 0.02 {
                painter.rect_filled(cell, 0.0, crate::theme::heat_blue(pal, *t));
            }
            if !text.is_empty() {
                painter.text(
                    Pos2::new(cell.right() - 10.0, cell.center().y),
                    Align2::RIGHT_CENTER,
                    text,
                    FontId::proportional(12.5),
                    pal.text,
                );
            }
        }
    }

    /// Expand/collapse chevron. Returns true when toggled.
    pub fn chevron(
        &self,
        ui: &egui::Ui,
        row: Rect,
        expanded: bool,
        enabled: bool,
        pal: &Palette,
    ) -> bool {
        let c = Pos2::new(row.left() + 16.0, row.center().y);
        let hit = Rect::from_center_size(c, egui::vec2(24.0, ROW_H));
        let resp = ui.interact(
            hit,
            egui::Id::new("chev")
                .with(row.top().to_bits())
                .with(hit.left().to_bits()),
            Sense::click(),
        );
        if enabled {
            let icon = if expanded { icons::Icon::ChevronDown } else { icons::Icon::ChevronRight };
            let mut r = hit;
            r.set_left(c.x - 9.0);
            r.set_right(c.x + 9.0);
            icons::draw_at(ui, r.shrink2(egui::vec2(0.0, 8.0)), icon, pal.text_dim);
        }
        resp.clicked()
    }

    /// Process icon slot: real texture when available, generic window glyph
    /// otherwise. `tex` comes from the shared icon cache.
    pub fn icon_cell(
        &self,
        ui: &egui::Ui,
        row: Rect,
        tex: Option<&egui::TextureHandle>,
        tint: Color32,
    ) {
        let r = Rect::from_center_size(
            Pos2::new(row.left() + 38.0, row.center().y),
            egui::vec2(18.0, 18.0),
        );
        match tex {
            Some(t) => {
                ui.painter().image(
                    t.id(),
                    r,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => icons::draw_app_window(ui, r, tint),
        }
    }
}

/// Small sort caret (chevron up/down).
pub fn caret(painter: egui::Painter, c: Pos2, ascending: bool, color: Color32) {
    let (a, b, t) = if ascending {
        (
            Pos2::new(c.x - 4.5, c.y + 2.5),
            Pos2::new(c.x + 4.5, c.y + 2.5),
            Pos2::new(c.x, c.y - 3.0),
        )
    } else {
        (
            Pos2::new(c.x - 4.5, c.y - 2.5),
            Pos2::new(c.x + 4.5, c.y - 2.5),
            Pos2::new(c.x, c.y + 3.0),
        )
    };
    painter.add(egui::Shape::convex_polygon(vec![a, b, t], color, Stroke::NONE));
}

/// Header aggregates for the resource columns: system CPU %, memory %,
/// busiest disk %, network utilization %.
pub struct Aggregates {
    pub cpu_pct: f32,
    pub mem_pct: f32,
    pub disk_pct: f32,
    pub net_pct: f32,
}

impl Aggregates {
    pub fn from_snapshot(snap: &tm_core::model::Snapshot) -> Self {
        let disk_pct = snap.disks.iter().map(|d| d.active_pct).fold(0.0f32, f32::max);
        // Network utilization: sum of (rate/link) across up adapters.
        let mut net_pct = 0.0f32;
        for n in &snap.networks {
            if n.link_bps > 0 && (n.recv_bps > 0.0 || n.sent_bps > 0.0) {
                net_pct += ((n.recv_bps + n.sent_bps) * 8.0 / n.link_bps as f64) as f32 * 100.0;
            }
        }
        Self {
            cpu_pct: snap.cpu.utilization_pct,
            mem_pct: snap.memory.used_pct(),
            disk_pct,
            net_pct: net_pct.clamp(0.0, 100.0),
        }
    }

    /// Strings aligned to the numeric columns (CPU, Arbeitsspeicher,
    /// Datenträger, Netzwerk).
    pub fn strings(&self) -> [String; 4] {
        [
            format::format_pct_de_int(self.cpu_pct),
            format::format_pct_de_int(self.mem_pct),
            format::format_pct_de_int(self.disk_pct),
            format::format_pct_de_int(self.net_pct),
        ]
    }
}

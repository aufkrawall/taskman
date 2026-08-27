//! Task-Manager-style table building blocks: bordered two-line headers with
//! aggregate values, full-row selection, blue heat-mapped value cells,
//! chevrons, sort carets — and user-resizable column widths.
//!
//! Width model (audit P0.1 parity): every column keeps its configured /
//! persisted width; unused viewport space stays blank client area on the
//! right, exactly like native Task Manager. A horizontal scroll bar appears
//! only when the summed real widths exceed the viewport.
//!
//! Correctness notes:
//! * Column resizing accumulates each frame's `drag_delta()` onto the LIVE
//!   width. In egui 0.36 `drag_delta()` is movement SINCE LAST FRAME
//!   (`pointer.delta()`); only `total_drag_delta()` is cumulative since the
//!   press. A frozen drag-start width plus one frame of delta reset the
//!   column to ~its starting width every frame, so boundaries never followed
//!   the cursor ("columns can't be resized").
//! * Double-clicking a separator applies a caller-supplied intrinsic width;
//!   callers compute it from their complete display model so virtualization
//!   never makes auto-fit depend on the current scroll position.
//! * Column geometry (`col_rect`) is precomputed once per frame into a
//!   layout vector, so cell lookup is O(1) instead of O(columns).
//! * [`scrolled_rows`] renders only the visible row window (fixed height),
//!   so tables scale to tens of thousands of rows.
//! * Widths persist by stable column id, not positional index.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, PointerButton, Pos2, Rect, Sense, Stroke,
};
use tm_core::format;

use crate::icons;
use crate::theme::Palette;

/// Row height used by all TM tables (also the virtualization unit).
pub const ROW_H: f32 = 32.0;
/// Header height for tables with aggregates (two-line).
pub const HEADER_H: f32 = 57.0;
/// Header height for single-line headers (Details/Services/Startup).
pub const HEADER_H1: f32 = 30.0;

/// Font sizes measured from Win11 TM: header aggregate values are notably
/// larger than row text, header labels slightly smaller.
pub const FONT_ROW: f32 = 13.0;
pub const FONT_HDR_LABEL: f32 = 12.0;
pub const FONT_AGG: f32 = 17.0;

/// Hard limits for user-resized columns.
const MIN_COL_W: f32 = 40.0;
const MAX_COL_W: f32 = 1200.0;

/// Empty strip kept clear on the RIGHT of the table content so the floating
/// vertical scroll bar (which egui paints ON TOP of the scroll area, without
/// reserving layout space) never covers the last column, and so the last
/// column keeps visible padding to the window border.
const BODY_PAD_RIGHT: i8 = 10;
/// Same idea for the horizontal bar: it floats over the BOTTOM of the body,
/// so the content ends a few px above it instead of under it.
const BODY_PAD_BOTTOM: i8 = 8;

/// Available width for a full-width table. The margin keeps the last
/// column's right-aligned labels clear of both the window border and the
/// floating scroll bar strip (`BODY_PAD_RIGHT` plus breathing room).
pub fn table_avail(ui: &egui::Ui) -> f32 {
    (ui.available_width() - 16.0).max(300.0)
}

/// Exact no-wrap text width in the same font atlas the table paints with.
/// Callers use this over their complete display model when preparing a
/// double-click auto-fit width; never approximate proportional text by chars.
pub fn text_width(ui: &egui::Ui, text: &str, font_size: f32) -> f32 {
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            FontId::proportional(font_size),
            Color32::WHITE,
        )
        .size()
        .x
}

/// Request that the next virtualized render of `id` center the given model
/// row. The request is consumed exactly once by [`scrolled_rows`]. This is
/// used for keyboard/list navigation where the destination row may currently
/// be outside the virtualized render range.
pub fn request_scroll_to_row(ctx: &egui::Context, id: &'static str, row: usize) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(("tm-scroll-row", id)), row));
}

/// Render a table header + body with full scrolling support.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub fn scrolled_table(
    id: &'static str,
    ui: &mut egui::Ui,
    pal: &Palette,
    table: &mut TmTable,
    avail: f32,
    sort: Option<(usize, bool)>,
    aggregates: Option<&[String]>,
    rows: impl FnOnce(&mut egui::Ui, &TmTable, f32, f32),
) -> Option<usize> {
    let content_w = table.total_width();
    let hdr_id = egui::Id::new(("tm-hdrscroll", id));
    let rows_prev_x = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(egui::Id::new(("tm-rowsx", id))))
        .unwrap_or(0.0);
    ui.spacing_mut().item_spacing.y = 0.0;

    let hdr = egui::ScrollArea::horizontal()
        .id_salt(hdr_id)
        .auto_shrink(egui::Vec2b::new(false, true))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .horizontal_scroll_offset(rows_prev_x)
        .show(ui, |ui| table.header(ui, pal, sort, aggregates));

    let body = egui::ScrollArea::both()
        .id_salt(egui::Id::new(("tm-rowscroll", id)))
        .auto_shrink(false)
        .content_margin(egui::Margin {
            left: 0,
            right: BODY_PAD_RIGHT,
            top: 0,
            bottom: BODY_PAD_BOTTOM,
        })
        .horizontal_scroll_offset(hdr.state.offset.x)
        .show(ui, |ui| rows(ui, table, avail, avail.max(content_w)));

    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new(("tm-rowsx", id)), body.state.offset.x));
    hdr.inner
}

/// Virtualized variant of [`scrolled_table`] for uniform fixed-height rows.
#[allow(clippy::too_many_arguments)]
pub fn scrolled_rows(
    id: &'static str,
    ui: &mut egui::Ui,
    pal: &Palette,
    table: &mut TmTable,
    avail: f32,
    sort: Option<(usize, bool)>,
    aggregates: Option<&[String]>,
    row_count: usize,
    rows: impl FnOnce(&mut egui::Ui, &TmTable, f32, f32, std::ops::Range<usize>),
) -> Option<usize> {
    let content_w = table.total_width();
    let hdr_id = egui::Id::new(("tm-hdrscroll", id));
    let rows_prev_x = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(egui::Id::new(("tm-rowsx", id))))
        .unwrap_or(0.0);
    ui.spacing_mut().item_spacing.y = 0.0;

    let hdr = egui::ScrollArea::horizontal()
        .id_salt(hdr_id)
        .auto_shrink(egui::Vec2b::new(false, true))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .horizontal_scroll_offset(rows_prev_x)
        .show(ui, |ui| table.header(ui, pal, sort, aggregates));

    let requested_row = ui.ctx().data_mut(|d| {
        d.remove_temp::<usize>(egui::Id::new(("tm-scroll-row", id)))
    });
    let mut body_area = egui::ScrollArea::both()
        .id_salt(egui::Id::new(("tm-rowscroll", id)))
        .auto_shrink(false)
        .content_margin(egui::Margin {
            left: 0,
            right: BODY_PAD_RIGHT,
            top: 0,
            bottom: BODY_PAD_BOTTOM,
        })
        .horizontal_scroll_offset(hdr.state.offset.x);
    if let Some(row) = requested_row.filter(|row| *row < row_count) {
        // Center when possible. ScrollArea clamps the final value to its
        // actual content extent, so first/last rows naturally land at edges.
        let viewport_h = ui.available_height();
        let centered = row as f32 * ROW_H - (viewport_h - ROW_H).max(0.0) * 0.5;
        body_area = body_area.vertical_scroll_offset(centered.max(0.0));
    }
    let body = body_area.show_rows(ui, ROW_H, row_count, |ui, range| {
        rows(ui, table, avail, avail.max(content_w), range)
    });

    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new(("tm-rowsx", id)), body.state.offset.x));
    hdr.inner
}

#[derive(Debug, Clone)]
pub struct TmColumn {
    pub id: &'static str,
    pub label: &'static str,
    pub width: f32,
    pub default_w: f32,
    pub numeric: bool,
}

impl TmColumn {
    pub const fn text(id: &'static str, label: &'static str, width: f32) -> Self {
        Self {
            id,
            label,
            width,
            default_w: width,
            numeric: false,
        }
    }
    pub const fn num(id: &'static str, label: &'static str, width: f32) -> Self {
        Self {
            id,
            label,
            width,
            default_w: width,
            numeric: true,
        }
    }
}

struct Layout {
    cols: Vec<(f32, f32)>,
}

#[derive(Debug, Clone)]
pub struct HeatCell {
    pub intensity: f32,
    pub text: String,
}

impl HeatCell {
    pub fn new(intensity: f32, text: impl Into<String>) -> Self {
        Self {
            intensity,
            text: text.into(),
        }
    }
}

pub fn norm(value: f64, max: f64) -> f32 {
    if max <= 0.0 {
        0.0
    } else {
        ((value / max) as f32).clamp(0.0, 1.0)
    }
}

pub struct TmTable {
    pub id: &'static str,
    pub cols: Vec<TmColumn>,
    layout: std::cell::RefCell<Option<Layout>>,
    dirty: bool,
    /// Full-model intrinsic widths supplied by the tab before `header()`.
    /// `None` preserves the historical default-width fallback for tables
    /// that have not yet registered an intrinsic measurement.
    auto_widths: Vec<Option<f32>>,
}

impl TmTable {
    pub fn new(
        id: &'static str,
        cols: Vec<TmColumn>,
        saved: Option<&std::collections::BTreeMap<String, f32>>,
    ) -> Self {
        let mut t = Self {
            id,
            auto_widths: vec![None; cols.len()],
            cols,
            layout: std::cell::RefCell::new(None),
            dirty: false,
        };
        if let Some(saved) = saved {
            for c in t.cols.iter_mut() {
                if let Some(w) = saved.get(c.id)
                    && (MIN_COL_W..=MAX_COL_W).contains(w)
                {
                    c.width = *w;
                }
            }
        }
        t
    }

    pub fn stored_widths(&self) -> std::collections::BTreeMap<String, f32> {
        self.cols
            .iter()
            .map(|c| (c.id.to_string(), c.width))
            .collect()
    }

    pub fn changed_this_frame(&self) -> bool {
        self.dirty
    }

    /// Register the exact intrinsic width that separator double-click should
    /// apply to column `i`. The caller should include header/cell padding and
    /// decorations (icons/tree indentation) in `width`.
    pub fn set_auto_fit_width(&mut self, i: usize, width: f32) {
        if let Some(slot) = self.auto_widths.get_mut(i) {
            *slot = Some(width.clamp(MIN_COL_W, MAX_COL_W));
        }
    }

    pub fn col_width(&self, i: usize) -> f32 {
        self.cols
            .get(i)
            .map_or(MIN_COL_W, |c| c.width.max(MIN_COL_W))
    }

    pub fn total_width(&self) -> f32 {
        self.cols.iter().map(|c| c.width.max(MIN_COL_W)).sum()
    }

    fn ensure_layout(&self) {
        let mut slot = self.layout.borrow_mut();
        if slot.is_some() {
            return;
        }
        let mut cols = Vec::with_capacity(self.cols.len());
        let mut x = 0.0f32;
        for c in &self.cols {
            let w = c.width.max(MIN_COL_W);
            cols.push((x, w));
            x += w;
        }
        *slot = Some(Layout { cols });
    }

    pub fn col_rect(&self, i: usize, row: Rect) -> Rect {
        self.ensure_layout();
        let l = self.layout.borrow();
        let Some(l) = l.as_ref() else { return row };
        match l.cols.get(i) {
            Some(&(x, w)) => Rect::from_min_max(
                Pos2::new(row.left() + x, row.top()),
                Pos2::new(row.left() + x + w, row.bottom()),
            ),
            None => row,
        }
    }

    fn numeric_span(&self, row: Rect, from: usize) -> Rect {
        let left = self.col_rect(from, row).left();
        Rect::from_min_max(
            Pos2::new(left, row.top()),
            Pos2::new(row.right(), row.bottom()),
        )
    }

    fn boundary_x(&self, rect: Rect, i: usize) -> f32 {
        self.ensure_layout();
        let l = self.layout.borrow();
        let off = l
            .as_ref()
            .and_then(|l| l.cols.get(i))
            .map_or(0.0, |&(x, _)| x);
        rect.left() + off
    }

    pub fn header(
        &mut self,
        ui: &mut egui::Ui,
        pal: &Palette,
        sort: Option<(usize, bool)>,
        aggregates: Option<&[String]>,
    ) -> Option<usize> {
        let h = if aggregates.is_some() {
            HEADER_H
        } else {
            HEADER_H1
        };
        self.layout.borrow_mut().take();
        let table_id = egui::Id::new(("tmtable", self.id));
        let total_w = self.total_width();
        let (rect, _) = ui.allocate_exact_size(egui::vec2(total_w, h), Sense::hover());
        let painter = ui.painter_at(rect.expand(2.0));
        let mut clicked = None;
        self.dirty = false;
        let mut x = rect.left();
        let mut agg_idx = 0usize;

        let mut bounds: Vec<f32> = (0..self.cols.len())
            .map(|i| self.boundary_x(rect, i))
            .collect();
        bounds.push(rect.left() + total_w);

        for (i, col) in self.cols.iter().enumerate() {
            let w = self.col_width(i);
            let cell =
                Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(x + w, rect.bottom()));

            let resp = ui.interact(cell, table_id.with(("hdr", col.id)), Sense::click());
            if resp.hovered() {
                painter.rect_filled(cell, 0.0, Color32::from_white_alpha(6));
            }
            if resp.clicked() {
                clicked = Some(i);
            }

            if i > 0 {
                painter.line_segment(
                    [
                        Pos2::new(x, rect.top() + 4.0),
                        Pos2::new(x, rect.bottom() - 4.0),
                    ],
                    Stroke::new(1.0, pal.stroke),
                );
            }

            let two_line = aggregates.is_some() && col.numeric;
            let label_y = if two_line {
                cell.bottom() - 13.0
            } else {
                cell.center().y
            };
            let align = if col.numeric {
                Align2::RIGHT_CENTER
            } else {
                Align2::LEFT_CENTER
            };
            let tx = if col.numeric {
                cell.right() - 10.0
            } else {
                cell.left() + 10.0
            };
            if col.numeric {
                agg_idx += 1;
            }

            let sorted = sort.filter(|(si, _)| *si == i);
            if two_line
                && agg_idx > 0
                && let Some(agg) = aggregates.and_then(|a| a.get(agg_idx - 1))
            {
                let agg_x = if sorted.is_some() {
                    cell.right() - 28.0
                } else {
                    cell.right() - 10.0
                };
                painter.text(
                    Pos2::new(agg_x, cell.top() + 19.0),
                    Align2::RIGHT_CENTER,
                    agg,
                    FontId::proportional(FONT_AGG),
                    pal.text,
                );
            }

            if let Some((_, asc)) = sorted {
                let cx = if col.numeric {
                    cell.right() - 16.0
                } else {
                    let label_w = painter
                        .layout_no_wrap(
                            col.label.to_owned(),
                            FontId::proportional(FONT_HDR_LABEL),
                            Color32::WHITE,
                        )
                        .size()
                        .x;
                    tx + label_w + 9.0
                };
                let cy = if two_line {
                    cell.top() + 19.0
                } else {
                    cell.top() + 10.0
                };
                caret(painter.clone(), Pos2::new(cx, cy), asc, pal.text_dim);
            }

            ui.painter_at(cell).text(
                Pos2::new(tx, label_y),
                align,
                col.label,
                FontId::proportional(FONT_HDR_LABEL),
                pal.text_dim,
            );
            x += w;
        }

        for (i, &bx) in bounds.iter().enumerate().skip(1) {
            let handle = Rect::from_min_max(
                Pos2::new(bx - 6.0, rect.top()),
                Pos2::new(bx + 6.0, rect.bottom()),
            );
            let rresp = ui.interact(
                handle,
                table_id.with(("resize", self.cols[i - 1].id)),
                Sense::drag(),
            );
            if rresp.hovered() || rresp.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
            if rresp.hovered() && !rresp.dragged() {
                painter.line_segment(
                    [
                        Pos2::new(bounds[i], rect.top() + 3.0),
                        Pos2::new(bounds[i], rect.bottom() - 3.0),
                    ],
                    Stroke::new(1.5, pal.accent.gamma_multiply(0.6)),
                );
            }

            if rresp.dragged() {
                let dx = rresp.drag_delta().x;
                if dx != 0.0 {
                    let current = self.col_width(i - 1);
                    self.cols[i - 1].width = (current + dx).clamp(MIN_COL_W, MAX_COL_W);
                    self.layout.borrow_mut().take();
                    self.dirty = true;
                }
            }

            // A drag-only response does not receive click flags, so inspect
            // the pointer's second press directly while it rests on the
            // separator. Full-model widths are supplied by each table tab.
            if rresp.hovered()
                && ui
                    .ctx()
                    .input(|i| i.pointer.button_double_clicked(PointerButton::Primary))
            {
                let col = i - 1;
                self.cols[col].width = self.auto_widths[col]
                    .unwrap_or(self.cols[col].default_w)
                    .clamp(MIN_COL_W, MAX_COL_W);
                self.layout.borrow_mut().take();
                self.dirty = true;
            }
        }

        painter.line_segment(
            [
                Pos2::new(rect.left(), rect.bottom()),
                Pos2::new(rect.right(), rect.bottom()),
            ],
            Stroke::new(1.0, pal.stroke),
        );

        clicked
    }

    pub fn row(&self, ui: &mut egui::Ui, pal: &Palette, selected: bool) -> (Rect, egui::Response) {
        let total_w = self.total_width();
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

    /// Left-aligned text cell. The painter is clipped to this exact cell so
    /// long values can never bleed into the neighbouring column.
    pub fn text_cell(
        &self,
        ui: &egui::Ui,
        row: Rect,
        i: usize,
        text: &str,
        pal: &Palette,
        dim: bool,
    ) {
        let cell = self.col_rect(i, row);
        ui.painter_at(cell).text(
            Pos2::new(cell.left() + 10.0, cell.center().y),
            Align2::LEFT_CENTER,
            text,
            FontId::proportional(FONT_ROW),
            if dim { pal.text_dim } else { pal.text },
        );
    }

    pub fn heat_cells(
        &self,
        ui: &egui::Ui,
        pal: &Palette,
        row: Rect,
        from: usize,
        cells: &[HeatCell],
        row_active: bool,
    ) {
        let span = self.numeric_span(row, from);
        let painter = ui.painter_at(row.expand(2.0));
        if row_active {
            painter.rect_filled(span, 0.0, pal.heat_base);
        }
        for (k, cell_data) in cells.iter().enumerate() {
            let cell = self.col_rect(from + k, row);
            if row_active && cell_data.intensity >= 1.0 - f32::EPSILON {
                painter.rect_filled(cell, 0.0, pal.heat_top);
            }
            if !cell_data.text.is_empty() {
                ui.painter_at(cell).text(
                    Pos2::new(cell.right() - 10.0, cell.center().y),
                    Align2::RIGHT_CENTER,
                    &cell_data.text,
                    FontId::proportional(FONT_ROW),
                    pal.text,
                );
            }
            if row_active && k + 1 < cells.len() {
                painter.line_segment(
                    [
                        Pos2::new(cell.right(), row.top()),
                        Pos2::new(cell.right(), row.bottom()),
                    ],
                    Stroke::new(1.0, pal.heat_sep),
                );
            }
        }
    }

    pub fn chevron(
        &self,
        ui: &egui::Ui,
        row: Rect,
        expanded: bool,
        enabled: bool,
        pal: &Palette,
        seed: egui::Id,
    ) -> bool {
        let c = Pos2::new(row.left() + 16.0, row.center().y);
        let hit = Rect::from_center_size(c, egui::vec2(24.0, ROW_H));
        let resp = ui.interact(hit, seed.with("chev"), Sense::click());
        if enabled {
            let icon = if expanded {
                icons::Icon::ChevronDown
            } else {
                icons::Icon::ChevronRight
            };
            let mut r = hit;
            r.set_left(c.x - 9.0);
            r.set_right(c.x + 9.0);
            icons::draw_at(ui, r.shrink2(egui::vec2(0.0, 8.0)), icon, pal.text_dim);
        }
        resp.clicked()
    }

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
        let painter = ui.painter_at(row);
        match tex {
            Some(t) => {
                painter.image(
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
    painter.add(egui::Shape::convex_polygon(
        vec![a, b, t],
        color,
        Stroke::NONE,
    ));
}

pub struct Aggregates {
    pub cpu_pct: f32,
    pub mem_pct: f32,
    pub disk_pct: f32,
    pub net_pct: f32,
}

impl Aggregates {
    pub fn from_snapshot(snap: &tm_core::model::Snapshot) -> Self {
        let disk_pct = snap
            .disks
            .iter()
            .map(|d| d.active_pct)
            .fold(0.0f32, f32::max);
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

    pub fn strings(&self) -> [String; 4] {
        [
            format::format_pct_hdr(self.cpu_pct),
            format::format_pct_hdr(self.mem_pct),
            format::format_pct_hdr(self.disk_pct),
            format::format_pct_hdr(self.net_pct),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> TmTable {
        TmTable::new(
            "t",
            vec![
                TmColumn::text("name", "Name", 340.0),
                TmColumn::text("a", "A", 200.0),
                TmColumn::num("b", "B", 100.0),
            ],
            None,
        )
    }

    #[test]
    fn columns_keep_configured_widths_at_any_viewport() {
        let t = table();
        assert_eq!(t.col_width(0), 340.0);
        assert_eq!(t.col_width(1), 200.0);
        assert_eq!(t.col_width(2), 100.0);
        assert_eq!(t.total_width(), 640.0);
        assert_eq!(t.total_width(), t.total_width());
    }

    #[test]
    fn saved_widths_restore_by_id() {
        let t = table();
        let saved = std::collections::BTreeMap::from([
            ("a".to_string(), 250.0f32),
            ("name".to_string(), 599.0f32),
            ("b".to_string(), 3.0f32),
        ]);
        let restored = TmTable::new("t", t.cols.clone(), Some(&saved));
        assert_eq!(restored.col_width(0), 599.0);
        assert_eq!(restored.col_width(1), 250.0);
        assert_eq!(restored.col_width(2), 100.0);
    }

    #[test]
    fn heat_normalization_marks_top_consumer() {
        assert_eq!(norm(30.0, 30.0), 1.0);
        assert!((norm(15.0, 30.0) - 0.5).abs() < 1e-6);
        assert_eq!(norm(7.0, 0.0), 0.0, "no maximum -> nothing highlighted");
        assert_eq!(norm(0.0, 42.0), 0.0);
    }

    fn ptr_moved(x: f32, y: f32) -> egui::Event {
        egui::Event::PointerMoved(egui::Pos2::new(x, y))
    }

    fn ptr_button(x: f32, y: f32, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: egui::Pos2::new(x, y),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        }
    }

    fn header_frame(
        ctx: &egui::Context,
        table: &mut TmTable,
        screen: egui::Rect,
        t: f64,
        events: Vec<egui::Event>,
    ) -> f32 {
        let left = std::cell::Cell::new(0.0f32);
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            time: Some(t),
            predicted_dt: 1.0 / 60.0,
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |root| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(root, |ui| {
                    left.set(ui.cursor().left());
                    table.header(ui, &crate::theme::DARK, None, None);
                });
        });
        out.textures_delta.clear();
        left.get()
    }

    #[test]
    fn dragging_name_boundary_tracks_cursor_across_frames() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let mut t = table();
        let l = header_frame(&ctx, &mut t, screen, 0.000, vec![]);
        let grab = egui::Pos2::new(l + 340.0, 20.0);
        header_frame(&ctx, &mut t, screen, 0.016, vec![ptr_moved(grab.x, grab.y)]);
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.032,
            vec![ptr_button(grab.x, grab.y, true)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.048,
            vec![ptr_moved(grab.x + 30.0, grab.y)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.064,
            vec![ptr_moved(grab.x + 60.0, grab.y)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.080,
            vec![ptr_button(grab.x + 60.0, grab.y, false)],
        );
        assert_eq!(t.col_width(0), 400.0);
        assert_eq!(t.col_width(1), 200.0, "neighbour column untouched");
    }

    #[test]
    fn dragging_other_boundary_resizes_that_column_only() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let mut t = table();
        let l = header_frame(&ctx, &mut t, screen, 0.000, vec![]);
        let grab = egui::Pos2::new(l + 540.0, 20.0);
        header_frame(&ctx, &mut t, screen, 0.016, vec![ptr_moved(grab.x, grab.y)]);
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.032,
            vec![ptr_button(grab.x, grab.y, true)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.048,
            vec![ptr_moved(grab.x - 50.0, grab.y)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.064,
            vec![ptr_moved(grab.x - 80.0, grab.y)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.080,
            vec![ptr_button(grab.x - 80.0, grab.y, false)],
        );
        assert_eq!(t.cols[1].width, 120.0);
        assert_eq!(t.cols[0].width, 340.0, "first column unaffected");
    }

    #[test]
    fn double_click_on_boundary_uses_auto_fit_width() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let mut t = table();
        t.cols[1].width = 300.0;
        t.set_auto_fit_width(1, 267.0);
        let l = header_frame(&ctx, &mut t, screen, 0.000, vec![]);
        let bx = l + 640.0;
        header_frame(&ctx, &mut t, screen, 0.016, vec![ptr_moved(bx, 20.0)]);
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.032,
            vec![ptr_button(bx, 20.0, true)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.048,
            vec![ptr_button(bx, 20.0, false)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.064,
            vec![ptr_button(bx, 20.0, true)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            0.080,
            vec![ptr_button(bx, 20.0, false)],
        );
        assert_eq!(t.cols[1].width, 267.0);
    }
}

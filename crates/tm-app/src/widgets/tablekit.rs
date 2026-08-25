//! Task-Manager-style table building blocks: bordered two-line headers with
//! aggregate values, full-row selection, blue heat-mapped value cells,
//! chevrons, sort carets — and user-resizable column widths.

use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke};
use tm_core::format;

use crate::icons;
use crate::theme::Palette;

/// Row height used by all TM tables.
pub const ROW_H: f32 = 33.0;
/// Header height for tables with aggregates (two-line).
pub const HEADER_H: f32 = 56.0;
/// Header height for single-line headers (Details/Services/Startup).
pub const HEADER_H1: f32 = 30.0;

/// Hard limits for user-resized columns.
const MIN_COL_W: f32 = 40.0;
const MAX_COL_W: f32 = 1200.0;

/// Available width for a full-width table: a few px of safety margin so the
/// last column's right-aligned labels never touch the window border.
pub fn table_avail(ui: &egui::Ui) -> f32 {
    (ui.available_width() - 6.0).max(300.0)
}

/// Render a table header + body with full scrolling support.
///
/// The header sits in its own horizontal-only scroll area whose offset
/// mirrors the body's; the body is a `ScrollArea::both()`, so whenever the
/// user widens columns past the window (or shrinks the window below the
/// minimum table width) a horizontal scroll bar appears and header + rows
/// stay aligned while scrolling. The vertical bar lives only on the body —
/// the header never scrolls out of view.
///
/// `rows` receives `(ui, avail, content_width)`; use `content_width` for
/// full-width decorations (group headers) so they cover the scrolled span.
/// Returns the clicked header column (for sorting).
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
    let content_w = table.total_width(avail);
    let hdr_id = egui::Id::new(("tm-hdrscroll", id));
    let rows_prev_x = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(egui::Id::new(("tm-rowsx", id))))
        .unwrap_or(0.0);

    // Header: horizontal-only, no visible bar; follows the body's offset.
    // On the non-scrolling (vertical) axis the area must shrink to the
    // header's height — with `auto_shrink(false)` it would claim all
    // remaining panel height and squeeze the body out of view.
    let hdr = egui::ScrollArea::horizontal()
        .id_salt(hdr_id)
        .auto_shrink(egui::Vec2b::new(false, true))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .horizontal_scroll_offset(rows_prev_x)
        .show(ui, |ui| table.header(ui, pal, avail, sort, aggregates));

    // Body: both axes; x is pinned to the header's current offset (a no-op
    // unless the header was wheel-scrolled), leaving y free for the user.
    let body = egui::ScrollArea::both()
        .id_salt(egui::Id::new(("tm-rowscroll", id)))
        .auto_shrink(false)
        .horizontal_scroll_offset(hdr.state.offset.x)
        .show(ui, |ui| rows(ui, table, avail, avail.max(content_w)));

    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new(("tm-rowsx", id)), body.state.offset.x));
    hdr.inner
}

#[derive(Debug, Clone)]
pub struct TmColumn {
    pub id: &'static str,
    pub label: &'static str,
    pub width: f32,
    /// Built-in width (double-click on the separator restores it).
    pub default_w: f32,
    /// Numeric columns get a right-aligned two-line header (aggregate on top).
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

pub struct TmTable {
    /// Stable id used for persisting resized widths in the settings file.
    pub id: &'static str,
    pub cols: Vec<TmColumn>,
    /// Minimum width of the flexible first (name) column.
    pub name_min: f32,
    /// A width was modified during THIS frame's header() call.
    dirty: bool,
    /// Whether any resize handle was being dragged during the last header().
    dragging: bool,
    /// Whether any resize handle was dragged during the previous frame.
    prev_dragging: bool,
}

impl TmTable {
    /// Build a table, restoring previously saved widths for the non-name
    /// columns when available.
    pub fn new(
        id: &'static str,
        cols: Vec<TmColumn>,
        saved: Option<&[f32]>,
        name_min: f32,
    ) -> Self {
        let mut t = Self {
            id,
            cols,
            name_min,
            dirty: false,
            dragging: false,
            prev_dragging: false,
        };
        if let Some(saved) = saved {
            if saved.len() == t.cols.len() {
                // Current schema: one entry per column (incl. the name).
                for (c, w) in t.cols.iter_mut().zip(saved.iter()) {
                    if *w >= MIN_COL_W && *w <= MAX_COL_W {
                        c.width = *w;
                    }
                }
            } else {
                // Legacy schema: non-name columns only, name stays elastic.
                for (c, w) in t.cols.iter_mut().skip(1).zip(saved.iter()) {
                    if *w >= MIN_COL_W && *w <= MAX_COL_W {
                        c.width = *w;
                    }
                }
            }
        }
        t
    }

    /// Current widths of ALL columns (for persistence). The name column is
    /// `0.0` while it is still elastic (never user-resized).
    pub fn stored_widths(&self) -> Vec<f32> {
        self.cols.iter().map(|c| c.width).collect()
    }

    /// A width changed during this frame's `header()` call.
    pub fn changed_this_frame(&self) -> bool {
        self.dirty
    }

    /// A resize drag ended with this or the previous frame.
    pub fn drag_just_ended(&self) -> bool {
        !self.dragging && self.prev_dragging
    }

    /// The name column is elastic until the user drags its boundary once
    /// (`width > 0` = stored width); afterwards it keeps the stored size.
    pub fn name_width(&self, avail: f32) -> f32 {
        match self.cols[0].width {
            w if w > 0.0 => w.clamp(self.name_min, MAX_COL_W),
            _ => {
                let fixed: f32 = self.cols[1..].iter().map(|c| c.width).sum();
                (avail - fixed).max(self.name_min)
            }
        }
    }

    /// Effective width of the LAST column: it always stretches/shrinks so the
    /// table exactly fills the window (classic Task Manager behavior — every
    /// dragged boundary moves with the cursor, the right edge stays put).
    pub fn last_width(&self, avail: f32) -> f32 {
        let last = self.cols.len() - 1;
        let others: f32 =
            self.name_width(avail) + self.cols[1..last].iter().map(|c| c.width).sum::<f32>();
        (avail - others).max(MIN_COL_W)
    }

    /// Effective width of column `i`.
    pub fn col_width(&self, i: usize, avail: f32) -> f32 {
        if i == 0 {
            self.name_width(avail)
        } else if i == self.cols.len() - 1 {
            self.last_width(avail)
        } else {
            self.cols[i].width
        }
    }

    pub fn total_width(&self, avail: f32) -> f32 {
        let last = self.cols.len() - 1;
        self.name_width(avail)
            + self.cols[1..last].iter().map(|c| c.width).sum::<f32>()
            + self.last_width(avail)
    }

    pub fn col_rect(&self, i: usize, avail: f32, row: Rect) -> Rect {
        let mut x = row.left();
        for ci in 0..self.cols.len() {
            let w = self.col_width(ci, avail);
            if ci == i {
                return Rect::from_min_max(Pos2::new(x, row.top()), Pos2::new(x + w, row.bottom()));
            }
            x += w;
        }
        row
    }

    fn numeric_span(&self, avail: f32, row: Rect, from: usize) -> Rect {
        let left = self.col_rect(from, avail, row).left();
        Rect::from_min_max(
            Pos2::new(left, row.top()),
            Pos2::new(row.right(), row.bottom()),
        )
    }

    /// Left-edge x of column `i` inside `rect`.
    fn boundary_x(&self, rect: Rect, avail: f32, i: usize) -> f32 {
        let mut x = rect.left();
        for ci in 0..i.min(self.cols.len()) {
            x += self.col_width(ci, avail);
        }
        x
    }

    /// Paint the header. Returns the clicked column index (for sorting).
    ///
    /// Every boundary between two columns carries an invisible drag handle.
    /// Dragging resizes the column to the LEFT of the boundary — the boundary
    /// itself follows the cursor, exactly like Windows Task Manager. The last
    /// column absorbs the remaining window width, so the table always fills
    /// the window. Double-click restores the built-in default width.
    pub fn header(
        &mut self,
        ui: &mut egui::Ui,
        pal: &Palette,
        avail: f32,
        sort: Option<(usize, bool)>,
        aggregates: Option<&[String]>,
    ) -> Option<usize> {
        let h = if aggregates.is_some() {
            HEADER_H
        } else {
            HEADER_H1
        };
        let table_id = egui::Id::new(("tmtable", self.id));
        let total_w = self.total_width(avail);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(total_w, h), Sense::hover());
        let painter = ui.painter_at(rect.expand(2.0));
        let mut clicked = None;
        let mut dragging_now = false;
        self.dirty = false;
        let mut x = rect.left();
        let mut agg_idx = 0usize;

        // Snapshot boundaries before any mutation this frame.
        let bounds: Vec<f32> = (0..self.cols.len())
            .map(|i| self.boundary_x(rect, avail, i))
            .collect();
        // Handle responses collected during painting, applied afterwards.
        let mut resize_hits: Vec<(usize, egui::Response)> = Vec::new();

        for (i, col) in self.cols.iter().enumerate() {
            let w = self.col_width(i, avail);
            let cell =
                Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(x + w, rect.bottom()));

            // Hover + click.
            let resp = ui.interact(cell, table_id.with(("hdr", col.id)), Sense::click());
            if resp.hovered() {
                painter.rect_filled(cell, 0.0, Color32::from_white_alpha(6));
            }
            if resp.clicked() {
                clicked = Some(i);
            }

            // Vertical separators between header cells + bottom border.
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
                cell.bottom() - 14.0
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

            // Aggregate value above numeric labels.
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
                && si == i
            {
                let cx = if col.numeric {
                    cell.right() - 16.0
                } else {
                    cell.center().x
                };
                let cy = if two_line {
                    cell.top() + 14.0
                } else {
                    cell.top() + 10.0
                };
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

            // ---- resize handle on this column's LEFT edge: dragging it
            // resizes the column to the LEFT of the boundary (the boundary
            // follows the cursor). No handle left of the first column.
            if i >= 1 {
                let bx = bounds[i];
                let handle = Rect::from_min_max(
                    Pos2::new(bx - 6.0, rect.top()),
                    Pos2::new(bx + 6.0, rect.bottom()),
                );
                let rresp = ui.interact(handle, table_id.with(("resize", col.id)), Sense::drag());
                if rresp.hovered() || rresp.dragged() {
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                }
                resize_hits.push((i, rresp));
            }

            x += w;
        }

        // Apply resize results now that painting borrowed nothing mutable.
        // Boundary i (left edge of column i) resizes column i-1.
        for (i, rresp) in resize_hits {
            if rresp.dragged() {
                dragging_now = true;
            }
            let dx = rresp.drag_delta().x;
            let min_w = if i == 1 { self.name_min } else { MIN_COL_W };
            // The elastic name column stores 0.0 until first resized;
            // seed it with the current effective width on the first drag.
            if i == 1 && dx != 0.0 && self.cols[0].width <= 0.0 {
                self.cols[0].width = self.name_width(avail);
            }
            let target = &mut self.cols[i - 1];
            if dx != 0.0 {
                target.width = (target.width + dx).clamp(min_w, MAX_COL_W);
                self.dirty = true;
                dragging_now = true;
            }
            if rresp.double_clicked() {
                // Restores the built-in default; `0.0` on the name column
                // switches it back to elastic.
                target.width = target.default_w.clamp(0.0, MAX_COL_W);
                self.dirty = true;
            }
            // Subtle affordance: brighten hovered separators.
            if rresp.hovered() && !rresp.dragged() {
                painter.line_segment(
                    [
                        Pos2::new(bounds[i], rect.top() + 3.0),
                        Pos2::new(bounds[i], rect.bottom() - 3.0),
                    ],
                    Stroke::new(1.5, pal.accent.gamma_multiply(0.6)),
                );
            }
        }

        // Bottom border of the header + box around the name cell (TM look).
        painter.line_segment(
            [
                Pos2::new(rect.left(), rect.bottom()),
                Pos2::new(rect.right(), rect.bottom()),
            ],
            Stroke::new(1.0, pal.stroke),
        );
        painter.rect_stroke(
            self.col_rect(0, avail, rect).shrink(0.5),
            0.0,
            Stroke::new(1.0, pal.stroke),
            egui::StrokeKind::Inside,
        );

        self.prev_dragging = self.dragging;
        self.dragging = dragging_now;
        clicked
    }

    /// Whether a resize drag is in progress (call after `header`).
    pub fn dragging(&self) -> bool {
        self.dragging
    }

    /// Paint a row background. Returns the row rect and response.
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
    painter.add(egui::Shape::convex_polygon(
        vec![a, b, t],
        color,
        Stroke::NONE,
    ));
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
        let disk_pct = snap
            .disks
            .iter()
            .map(|d| d.active_pct)
            .fold(0.0f32, f32::max);
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

    /// Strings aligned to the numeric columns (CPU, Memory, Disk, Network).
    pub fn strings(&self) -> [String; 4] {
        [
            format::format_pct_hdr(self.cpu_pct),
            format::format_pct_hdr(self.mem_pct),
            format::format_pct_hdr(self.disk_pct),
            format::format_pct_hdr(self.net_pct),
        ]
    }
}

//! Task-Manager-style table building blocks: bordered two-line headers with
//! aggregate values, full-row selection, blue heat-mapped value cells,
//! chevrons, sort carets — and user-resizable column widths.
//!
//! Correctness notes (implement.md §8/§9):
//! * Column resizing stores the **drag-start width** in egui's persistent
//!   temp memory keyed by `(table, column)`; during a drag the new width is
//!   `start_width + cumulative delta`. Adding the cumulative delta to an
//!   already-updated width (the old bug) compounded every frame.
//! * Column geometry (`col_rect`) is precomputed once per frame into a
//!   layout vector, so cell lookup is O(1) instead of O(columns).
//! * [`scrolled_rows`] renders only the visible row window (fixed height),
//!   so tables scale to tens of thousands of rows.
//! * Widths persist by stable column id, not positional index.

use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke};
use tm_core::format;

use crate::icons;
use crate::theme::Palette;

/// Row height used by all TM tables (also the virtualization unit).
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
#[allow(dead_code)] // kept as the escape hatch for non-uniform row content
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

/// Virtualized variant of [`scrolled_table`] for uniform fixed-height rows.
///
/// Only the visible row range (+ egui's internal overscan) is painted, so
/// widget count scales with the viewport instead of the dataset. `rows`
/// receives the visible `Range<usize>` and must paint exactly those rows.
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
    let content_w = table.total_width(avail);
    let hdr_id = egui::Id::new(("tm-hdrscroll", id));
    let rows_prev_x = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(egui::Id::new(("tm-rowsx", id))))
        .unwrap_or(0.0);

    let hdr = egui::ScrollArea::horizontal()
        .id_salt(hdr_id)
        .auto_shrink(egui::Vec2b::new(false, true))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .horizontal_scroll_offset(rows_prev_x)
        .show(ui, |ui| table.header(ui, pal, avail, sort, aggregates));

    let body = egui::ScrollArea::both()
        .id_salt(egui::Id::new(("tm-rowscroll", id)))
        .auto_shrink(false)
        .horizontal_scroll_offset(hdr.state.offset.x)
        .show_rows(ui, ROW_H, row_count, |ui, range| {
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
    /// Current width; the FIRST column is the designated elastic column and
    /// additionally absorbs leftover viewport width.
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
    /// The elastic name/description column: `width == 0` means "not yet
    /// user-resized"; the effective width fills the remaining viewport.
    pub const fn elastic(id: &'static str, label: &'static str, min_w: f32) -> Self {
        Self {
            id,
            label,
            width: 0.0,
            default_w: min_w,
            numeric: false,
        }
    }
}

/// One-frame cached column geometry (implement.md §8.7).
struct Layout {
    avail: f32,
    /// (left x offset relative to row, width) per column.
    cols: Vec<(f32, f32)>,
}

pub struct TmTable {
    /// Stable id used for persisting resized widths in the settings file.
    pub id: &'static str,
    pub cols: Vec<TmColumn>,
    /// Minimum width of the flexible first (name) column.
    pub name_min: f32,
    /// Precomputed geometry for this frame (rebuilt when `avail` changes).
    layout: std::cell::RefCell<Option<Layout>>,
    /// Set when any width was modified during THIS frame's `header()`.
    dirty: bool,
}

impl TmTable {
    /// Build a table, restoring previously saved widths by column id when
    /// available. Unknown future ids in the map are simply never read.
    pub fn new(
        id: &'static str,
        cols: Vec<TmColumn>,
        saved: Option<&std::collections::BTreeMap<String, f32>>,
        name_min: f32,
    ) -> Self {
        let mut t = Self {
            id,
            cols,
            name_min,
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

    /// Current widths keyed by column id (for persistence).
    pub fn stored_widths(&self) -> std::collections::BTreeMap<String, f32> {
        self.cols
            .iter()
            .filter(|c| c.width > 0.0)
            .map(|c| (c.id.to_string(), c.width))
            .collect()
    }

    /// A width changed during this frame's `header()` call.
    pub fn changed_this_frame(&self) -> bool {
        self.dirty
    }

    /// Effective width of the elastic first column: absorbs leftover
    /// viewport width when the other columns leave room; otherwise keeps
    /// its stored/default width and the table scrolls horizontally.
    fn name_effective(&self, avail: f32) -> f32 {
        let stored = self.cols[0]
            .width
            .max(self.name_min.min(self.cols[0].default_w.max(MIN_COL_W)));
        let others: f32 = self.cols[1..].iter().map(|c| c.width).sum();
        let spare = avail - others;
        if spare > stored {
            spare
        } else {
            stored.max(MIN_COL_W)
        }
    }

    /// Effective width of column `i`.
    pub fn col_width(&self, i: usize, avail: f32) -> f32 {
        if i == 0 {
            self.name_effective(avail)
        } else {
            self.cols
                .get(i)
                .map_or(MIN_COL_W, |c| c.width.max(MIN_COL_W))
        }
    }

    pub fn total_width(&self, avail: f32) -> f32 {
        (0..self.cols.len()).map(|i| self.col_width(i, avail)).sum()
    }

    /// Build (once per frame / per avail) the x-offset layout.
    fn ensure_layout(&self, avail: f32) {
        let mut slot = self.layout.borrow_mut();
        if let Some(l) = slot.as_ref()
            && l.avail == avail
        {
            return;
        }
        let mut cols = Vec::with_capacity(self.cols.len());
        let mut x = 0.0f32;
        for i in 0..self.cols.len() {
            let w = self.col_width(i, avail);
            cols.push((x, w));
            x += w;
        }
        *slot = Some(Layout { avail, cols });
    }

    pub fn col_rect(&self, i: usize, avail: f32, row: Rect) -> Rect {
        self.ensure_layout(avail);
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

    fn numeric_span(&self, avail: f32, row: Rect, from: usize) -> Rect {
        let left = self.col_rect(from, avail, row).left();
        Rect::from_min_max(
            Pos2::new(left, row.top()),
            Pos2::new(row.right(), row.bottom()),
        )
    }

    /// Left-edge x of column `i` inside `rect`.
    fn boundary_x(&self, rect: Rect, avail: f32, i: usize) -> f32 {
        self.ensure_layout(avail);
        let l = self.layout.borrow();
        let off = l
            .as_ref()
            .and_then(|l| l.cols.get(i))
            .map_or(0.0, |&(x, _)| x);
        rect.left() + off
    }

    /// Paint the header. Returns the clicked column index (for sorting).
    ///
    /// Every boundary between two columns carries an invisible drag handle.
    /// Dragging resizes the column to the LEFT of the boundary — the
    /// boundary follows the cursor, exactly like Windows Task Manager.
    /// Drag-start width is persisted in egui temp memory so the cumulative
    /// `drag_delta()` maps 1:1 onto the boundary movement (§8.1).
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
        self.layout.borrow_mut().take(); // widths may change below
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

            // Sorted state of THIS column; also decides where the aggregate
            // sits (shifted left so the caret fits next to it).
            let sorted = sort.filter(|(si, _)| *si == i);

            // Aggregate value above numeric labels — drawn exactly once
            // (the old code painted it a second time in the sorted branch,
            // ghosting the text at two overlapping x positions).
            if two_line
                && agg_idx > 0
                && let Some(agg) = aggregates.and_then(|a| a.get(agg_idx - 1))
            {
                let agg_x = if sorted.is_some() {
                    cell.right() - 26.0
                } else {
                    cell.right() - 10.0
                };
                painter.text(
                    Pos2::new(agg_x, cell.top() + 14.0),
                    Align2::RIGHT_CENTER,
                    agg,
                    FontId::proportional(12.5),
                    pal.text,
                );
            }

            // Sort caret above the label of the sorted column. Anchored to
            // the label's edge — numeric columns right-aligned like their
            // values, text columns right after the label. Centering it in
            // the column would float far away on wide name columns.
            if let Some((_, asc)) = sorted {
                let cx = if col.numeric {
                    cell.right() - 16.0
                } else {
                    let label_w = painter
                        .layout_no_wrap(
                            col.label.to_owned(),
                            FontId::proportional(12.5),
                            Color32::WHITE,
                        )
                        .size()
                        .x;
                    tx + label_w + 9.0
                };
                let cy = if two_line {
                    cell.top() + 14.0
                } else {
                    cell.top() + 10.0
                };
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
            let target_col_id = self.cols[i - 1].id;
            let drag_key = table_id.with(("resize-start", target_col_id));

            if rresp.drag_started() {
                // Remember the width at gesture start; the cumulative delta
                // applies against THIS value for the whole drag.
                let start = if i - 1 == 0 {
                    self.name_effective(avail)
                } else {
                    self.cols[i - 1].width.max(MIN_COL_W)
                };
                ui.ctx().data_mut(|d| d.insert_temp(drag_key, start));
            }

            if rresp.dragged() {
                let dx = rresp.drag_delta().x;
                if dx != 0.0 {
                    let start_w = ui
                        .ctx()
                        .data(|d| d.get_temp::<f32>(drag_key))
                        .unwrap_or_else(|| {
                            if i - 1 == 0 {
                                self.name_effective(avail)
                            } else {
                                self.cols[i - 1].width
                            }
                        });
                    let min_w = if i == 1 { self.name_min } else { MIN_COL_W };
                    let new_w = (start_w + dx).clamp(min_w, MAX_COL_W);
                    self.cols[i - 1].width = new_w;
                    self.layout.borrow_mut().take();
                    self.dirty = true;
                    dragging_now = true;
                }
            }
            if rresp.double_clicked() {
                // Restores the built-in default; `0.0` on the elastic name
                // column switches it back to fill-the-spare-space mode.
                self.cols[i - 1].width = self.cols[i - 1].default_w.clamp(0.0, MAX_COL_W);
                self.layout.borrow_mut().take();
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

        let _ = dragging_now; // drag state lives in egui temp data now
        clicked
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

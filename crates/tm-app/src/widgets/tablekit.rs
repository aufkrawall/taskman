//! Task-Manager-style table building blocks: bordered two-line headers with
//! aggregate values, full-row selection, blue heat-mapped value cells,
//! chevrons, sort carets — and user-resizable column widths.
//!
//! Correctness notes:
//! * Column resizing accumulates each frame's `drag_delta()` onto the LIVE
//!   width. In egui 0.36 `drag_delta()` is movement SINCE LAST FRAME
//!   (`pointer.delta()`); only `total_drag_delta()` is cumulative since the
//!   press. A frozen drag-start width plus one frame of delta reset the
//!   column to ~its starting width every frame, so boundaries never followed
//!   the cursor ("columns can't be resized").
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
pub const ROW_H: f32 = 33.0;
/// Header height for tables with aggregates (two-line).
pub const HEADER_H: f32 = 56.0;
/// Header height for single-line headers (Details/Services/Startup).
pub const HEADER_H1: f32 = 30.0;

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
    // The right/bottom content margins keep the floating scroll bars from
    // painting over cells (see `BODY_PAD_*`).
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
        .content_margin(egui::Margin {
            left: 0,
            right: BODY_PAD_RIGHT,
            top: 0,
            bottom: BODY_PAD_BOTTOM,
        })
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
    /// user-resized"; the effective width then fills the remaining viewport.
    /// The FIRST manual resize on the table materializes this width (the
    /// absorber would otherwise cancel the drag delta under the cursor);
    /// double-clicking the name separator restores fill mode via the same
    /// sentinel.
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

    /// Effective width of the elastic first column.
    ///
    /// Elastic ONLY while untouched (`width == 0.0`, the [`TmColumn::elastic`
    /// sentinel]): it then fills the viewport slack left by the other
    /// columns. The moment ANY column is drag-resized, `header()` freezes
    /// (materializes) the name width — otherwise this absorber would hand
    /// back every dragged pixel in the same frame, cancelling the delta at
    /// the boundary under the cursor (the dragged separator stayed put while
    /// the Name/Status divider shifted — "columns can't be resized"), and it
    /// flipped regimes around `spare == stored`, which read as wobbling.
    fn name_effective(&self, avail: f32) -> f32 {
        let w = self.cols[0].width;
        if w > 0.0 {
            return w.max(MIN_COL_W);
        }
        let others: f32 = self.cols[1..].iter().map(|c| c.width.max(MIN_COL_W)).sum();
        let spare = avail - others;
        if spare > self.name_min {
            spare
        } else {
            self.name_min.max(MIN_COL_W)
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
    /// Handles are registered AFTER all header cells so they win egui's hit
    /// testing across their full ±6 px (a later-registered cell used to
    /// swallow the right half of each handle, so grabs landing there did
    /// nothing and quick clicks even toggled sorting).
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
        self.dirty = false;
        let mut x = rect.left();
        let mut agg_idx = 0usize;

        // Snapshot boundaries before any mutation this frame. The extra
        // final entry is the right edge of the LAST column, which carries
        // its own resize handle like every interior boundary.
        let mut bounds: Vec<f32> = (0..self.cols.len())
            .map(|i| self.boundary_x(rect, avail, i))
            .collect();
        bounds.push(rect.left() + total_w);

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

            x += w;
        }

        // ---- resize handles for EVERY boundary (including the right edge
        // of the last column, TM parity), created after all cells so they
        // sit on top in egui's hit testing. Boundary i (left edge of column
        // i) resizes column i-1; no handle left of the first column.
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

            // First frame of a drag: freeze the elastic name column at its
            // current effective width before any delta is applied. From the
            // first manual resize on ALL columns are explicitly sized, so
            // the slack absorber in `name_effective` can no longer hand the
            // dragged pixels back within the same frame. Value-preserving —
            // no visual jump.
            if rresp.drag_started() && self.cols[0].width == 0.0 {
                self.cols[0].width = self.name_effective(avail);
                self.layout.borrow_mut().take();
                self.dirty = true;
            }

            if rresp.dragged() {
                let dx = rresp.drag_delta().x;
                if dx != 0.0 {
                    // `drag_delta()` is movement since LAST FRAME, so it
                    // must accumulate onto the LIVE width. Adding one
                    // frame's delta to a frozen drag-start width instead
                    // reset the width to ~its starting value on every frame
                    // (the boundary never followed the cursor).
                    let min_w = if i == 1 {
                        self.name_min.max(MIN_COL_W)
                    } else {
                        MIN_COL_W
                    };
                    let current = if i - 1 == 0 {
                        self.name_effective(avail)
                    } else {
                        self.cols[i - 1].width.max(MIN_COL_W)
                    };
                    self.cols[i - 1].width = (current + dx).clamp(min_w, MAX_COL_W);
                    self.layout.borrow_mut().take();
                    self.dirty = true;
                }
            }

            // Double-click restores the built-in default (for the elastic
            // name column the `0.0` sentinel switches back to fill mode).
            // A drag-only widget never receives egui's click flags, so the
            // second press is detected directly on the input state while
            // the pointer rests on the handle.
            if rresp.hovered()
                && ui
                    .ctx()
                    .input(|i| i.pointer.button_double_clicked(PointerButton::Primary))
            {
                self.cols[i - 1].width = if i - 1 == 0 {
                    0.0
                } else {
                    self.cols[i - 1].default_w.clamp(MIN_COL_W, MAX_COL_W)
                };
                self.layout.borrow_mut().take();
                self.dirty = true;
            }
        }

        // Bottom border of the header. Every header cell looks alike: only
        // the separators BETWEEN columns plus this shared bottom line (the
        // old extra box around the name cell made it stand out and put its
        // visual border half a pixel off the actual resize boundary).
        painter.line_segment(
            [
                Pos2::new(rect.left(), rect.bottom()),
                Pos2::new(rect.right(), rect.bottom()),
            ],
            Stroke::new(1.0, pal.stroke),
        );

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

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> TmTable {
        TmTable::new(
            "t",
            vec![
                TmColumn::elastic("name", "Name", 340.0),
                TmColumn::text("a", "A", 200.0),
                TmColumn::num("b", "B", 100.0),
            ],
            None,
            340.0,
        )
    }

    /// Regression: the elastic name column absorbed the viewport slack EVERY
    /// frame, so a manual drag of any other boundary was cancelled in the
    /// same frame — the dragged separator never moved ("columns can't be
    /// resized"). Materializing the name width at drag start must make the
    /// delta stick to the boundary under the cursor.
    #[test]
    fn materialized_name_stops_absorbing_drag_deltas() {
        let mut t = table();
        let avail = 1000.0;
        // Virgin table: elastic absorber fills the slack (200+100 fixed).
        assert_eq!(t.col_width(0, avail), 700.0);

        // `header()` does exactly this on the first drag_started:
        t.cols[0].width = t.name_effective(avail);

        // Growing column B by 300 must grow the TOTAL by 300 — i.e. B's left
        // boundary follows the cursor — instead of being absorbed away.
        let before = t.total_width(avail);
        t.cols[2].width += 300.0;
        assert_eq!(t.total_width(avail), before + 300.0);
        // And the name column must NOT re-absorb on later frames.
        assert_eq!(t.col_width(0, avail), 700.0);
    }

    /// While still untouched, the elastic column keeps filling slack and
    /// respects its minimum when the viewport gets too narrow.
    #[test]
    fn virgin_elastic_fills_slack_and_respects_min() {
        let mut t = table();
        assert_eq!(t.col_width(0, 1200.0), 900.0);
        // Viewport leaves less than the minimum -> clamped to name_min.
        assert_eq!(t.col_width(0, 400.0), 340.0);
        // Double-click restore re-enables fill mode via the sentinel.
        t.cols[0].width = 500.0;
        assert_eq!(t.col_width(0, 1200.0), 500.0);
        t.cols[0].width = 0.0;
        assert_eq!(t.col_width(0, 1200.0), 900.0);
    }

    // ---- input-driven regression tests: real egui passes with synthetic
    // pointer events, exercising the actual drag pipeline in `header()`. ----

    const AVAIL: f32 = 1000.0;

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

    /// One egui pass painting `table.header()` pinned to the viewport's
    /// top-left; returns the header's left x so tests can aim events.
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
                    table.header(ui, &crate::theme::DARK, AVAIL, None, None);
                });
        });
        // No renderer here: consume the font texture deltas so egui does not
        // panic about unapplied deltas when the output is dropped.
        out.textures_delta.clear();
        left.get()
    }

    /// Regression (2026-08): `drag_delta()` is movement since the LAST
    /// FRAME, so adding it to a frozen drag-start width reset the column to
    /// ~its starting width every frame — the boundary never followed the
    /// cursor. Live accumulation must land exactly on the pointer instead.
    #[test]
    fn dragging_name_boundary_tracks_cursor_across_frames() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let mut t = table();
        // Virgin elastic name column fills the slack: Name|A boundary at 700.
        assert_eq!(t.col_width(0, AVAIL), 700.0);

        let l = header_frame(&ctx, &mut t, screen, 0.000, vec![]); // register rects
        let grab = egui::Pos2::new(l + 700.0, 20.0);
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

        // +30 px per pass for two passes: exactly +60 total.
        assert_eq!(t.col_width(0, AVAIL), 760.0);
        // The materialized width persists beyond the elastic absorber.
        assert_eq!(t.cols[0].width, 760.0);
    }

    /// Dragging a non-name boundary shrinks/grows THAT column and leaves the
    /// materialized name width alone.
    #[test]
    fn dragging_other_boundary_resizes_that_column_only() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let mut t = table();
        // A|B boundary at virgin x = 700 (name) + 200 (A) = 900.
        let l = header_frame(&ctx, &mut t, screen, 0.000, vec![]);
        let grab = egui::Pos2::new(l + 900.0, 20.0);
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

        assert_eq!(t.cols[1].width, 120.0); // 200 - 80
        assert_eq!(t.cols[0].width, 700.0); // name frozen at drag start
        assert_eq!(t.col_width(0, AVAIL), 700.0);
    }

    /// Double-clicking a boundary restores the built-in default; on the
    /// elastic name column it re-enables fill mode via the `0.0` sentinel.
    #[test]
    fn double_click_on_boundary_restores_default_and_fill_mode() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let mut t = table();
        // Explicit widths: name 500, A distorted to 300 so its restore is
        // observable, B 100. Boundaries then sit at x = 500 / 800 / 1100.
        t.cols[0].width = 500.0;
        t.cols[1].width = 300.0;
        let l = header_frame(&ctx, &mut t, screen, 0.000, vec![]);

        // A|B boundary at x = 500 + 300 = 800; double-click restores A.
        let bx = l + 800.0;
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
        assert_eq!(t.cols[1].width, 200.0);

        // Name|A boundary now at x = 500; double-click returns the name
        // column to fill-the-spare-space mode (sentinel 0.0). Start well
        // past the multi-click window so these register as fresh single/
        // double clicks instead of continuing the previous count.
        let bx = l + 500.0;
        header_frame(&ctx, &mut t, screen, 1.200, vec![ptr_moved(bx, 20.0)]);
        header_frame(
            &ctx,
            &mut t,
            screen,
            1.216,
            vec![ptr_button(bx, 20.0, true)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            1.232,
            vec![ptr_button(bx, 20.0, false)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            1.248,
            vec![ptr_button(bx, 20.0, true)],
        );
        header_frame(
            &ctx,
            &mut t,
            screen,
            1.264,
            vec![ptr_button(bx, 20.0, false)],
        );
        assert_eq!(t.cols[0].width, 0.0);
        assert_eq!(t.col_width(0, AVAIL), 700.0); // refills the slack again
    }
}

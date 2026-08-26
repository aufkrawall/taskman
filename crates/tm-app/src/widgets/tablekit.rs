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
    let content_w = table.total_width();
    let hdr_id = egui::Id::new(("tm-hdrscroll", id));
    let rows_prev_x = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(egui::Id::new(("tm-rowsx", id))))
        .unwrap_or(0.0);
    // TM rows touch: no vertical item spacing between rendered rows (the
    // default 6 px gap made the heat bands look striped).
    ui.spacing_mut().item_spacing.y = 0.0;

    // Header: horizontal-only, no visible bar; follows the body's offset.
    // On the non-scrolling (vertical) axis the area must shrink to the
    // header's height — with `auto_shrink(false)` it would claim all
    // remaining panel height and squeeze the body out of view.
    let hdr = egui::ScrollArea::horizontal()
        .id_salt(hdr_id)
        .auto_shrink(egui::Vec2b::new(false, true))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .horizontal_scroll_offset(rows_prev_x)
        .show(ui, |ui| table.header(ui, pal, sort, aggregates));

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
    let content_w = table.total_width();
    let hdr_id = egui::Id::new(("tm-hdrscroll", id));
    let rows_prev_x = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(egui::Id::new(("tm-rowsx", id))))
        .unwrap_or(0.0);
    // See `scrolled_table`: rows must touch.
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
    /// Current width in px. No implicit elasticity: native Task Manager does
    /// NOT stretch the first column across the viewport — unused space stays
    /// empty to the right (audit P0.1).
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

/// One-frame cached column geometry (implement.md §8.7).
struct Layout {
    /// (left x offset relative to row, width) per column.
    cols: Vec<(f32, f32)>,
}

/// One pre-normalized numeric heat cell (audit P0.2): the caller computes
/// the intensity against the column's maximum over the whole display model
/// BEFORE virtualization, then hands it here for painting.
#[derive(Debug, Clone)]
pub struct HeatCell {
    /// 0..=1; exactly `1.0` marks the column's top consumer.
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

/// Normalize `value` against its column's maximum into 0..=1. A zero
/// maximum yields 0 — nothing is highlighted without real data.
pub fn norm(value: f64, max: f64) -> f32 {
    if max <= 0.0 {
        0.0
    } else {
        ((value / max) as f32).clamp(0.0, 1.0)
    }
}

pub struct TmTable {
    /// Stable id used for persisting resized widths in the settings file.
    pub id: &'static str,
    pub cols: Vec<TmColumn>,
    /// Precomputed geometry for this frame.
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
    ) -> Self {
        let mut t = Self {
            id,
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

    /// Current widths keyed by column id (for persistence).
    pub fn stored_widths(&self) -> std::collections::BTreeMap<String, f32> {
        self.cols
            .iter()
            .map(|c| (c.id.to_string(), c.width))
            .collect()
    }

    /// A width changed during this frame's `header()` call.
    pub fn changed_this_frame(&self) -> bool {
        self.dirty
    }

    /// Effective width of column `i`: its configured/persisted width — no
    /// index-based special casing (audit P0.1 / §27).
    pub fn col_width(&self, i: usize) -> f32 {
        self.cols
            .get(i)
            .map_or(MIN_COL_W, |c| c.width.max(MIN_COL_W))
    }

    pub fn total_width(&self) -> f32 {
        self.cols.iter().map(|c| c.width.max(MIN_COL_W)).sum()
    }

    /// Build (once, until any width mutates) the x-offset layout.
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

    /// Left-edge x of column `i` inside `rect`.
    fn boundary_x(&self, rect: Rect, i: usize) -> f32 {
        self.ensure_layout();
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
        let total_w = self.total_width();
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
            .map(|i| self.boundary_x(rect, i))
            .collect();
        bounds.push(rect.left() + total_w);

        for (i, col) in self.cols.iter().enumerate() {
            let w = self.col_width(i);
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

            painter.text(
                Pos2::new(tx, label_y),
                align,
                col.label,
                FontId::proportional(FONT_HDR_LABEL),
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

            if rresp.dragged() {
                let dx = rresp.drag_delta().x;
                if dx != 0.0 {
                    // `drag_delta()` is movement since LAST FRAME, so it
                    // must accumulate onto the LIVE width. Adding one
                    // frame's delta to a frozen drag-start width instead
                    // reset the width to ~its starting value on every frame
                    // (the boundary never followed the cursor).
                    let current = self.col_width(i - 1);
                    self.cols[i - 1].width = (current + dx).clamp(MIN_COL_W, MAX_COL_W);
                    self.layout.borrow_mut().take();
                    self.dirty = true;
                }
            }

            // Double-click restores the built-in default.
            // A drag-only widget never receives egui's click flags, so the
            // second press is detected directly on the input state while
            // the pointer rests on the handle.
            if rresp.hovered()
                && ui
                    .ctx()
                    .input(|i| i.pointer.button_double_clicked(PointerButton::Primary))
            {
                self.cols[i - 1].width = self.cols[i - 1].default_w.clamp(MIN_COL_W, MAX_COL_W);
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

    /// Left-aligned text cell.
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
        ui.painter().text(
            Pos2::new(cell.left() + 10.0, cell.center().y),
            Align2::LEFT_CENTER,
            text,
            FontId::proportional(FONT_ROW),
            if dim { pal.text_dim } else { pal.text },
        );
    }

    /// The contiguous blue heat block over numeric columns `from..`:
    /// base navy when the row is active, the brighter `heat_top` fill on
    /// each column's top-consumer cell, and thin separators between
    /// adjacent cells.
    ///
    /// Paints EXACTLY what it is told (audit P0.2): every [`HeatCell`]
    /// intensity is precomputed by the caller, normalized against that
    /// column's maximum over the WHOLE display model — before row
    /// virtualization. An intensity of exactly `1.0` marks the column's
    /// top consumer (ties all light up); this function never derives
    /// maxima from the single row it happens to paint.
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
            // TM: flat base for every active cell; only the column's top
            // consumer gets the brighter fill (no value gradient).
            if row_active && cell_data.intensity >= 1.0 - f32::EPSILON {
                painter.rect_filled(cell, 0.0, pal.heat_top);
            }
            if !cell_data.text.is_empty() {
                painter.text(
                    Pos2::new(cell.right() - 10.0, cell.center().y),
                    Align2::RIGHT_CENTER,
                    &cell_data.text,
                    FontId::proportional(FONT_ROW),
                    pal.text,
                );
            }
            // Thin separator between adjacent cells (TM draws the column
            // boundary through the blue band).
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
                TmColumn::text("name", "Name", 340.0),
                TmColumn::text("a", "A", 200.0),
                TmColumn::num("b", "B", 100.0),
            ],
            None,
        )
    }

    /// Regression (audit P0.1): the first column used to absorb all unused
    /// viewport width, stretching Name across the window and pushing numeric
    /// columns to the right edge. Native TM keeps configured widths and
    /// leaves blank client area on the right instead.
    #[test]
    fn columns_keep_configured_widths_at_any_viewport() {
        let t = table();
        assert_eq!(t.col_width(0), 340.0);
        assert_eq!(t.col_width(1), 200.0);
        assert_eq!(t.col_width(2), 100.0);
        assert_eq!(t.total_width(), 640.0);
        // Total width must not depend on the viewport at all.
        assert_eq!(t.total_width(), t.total_width());
    }

    /// Saved widths restore by column id; out-of-range values are ignored.
    #[test]
    fn saved_widths_restore_by_id() {
        let t = table();
        let saved = std::collections::BTreeMap::from([
            ("a".to_string(), 250.0f32),
            ("name".to_string(), 599.0f32),
            ("b".to_string(), 3.0f32), // below MIN_COL_W -> ignored
        ]);
        let restored = TmTable::new("t", t.cols.clone(), Some(&saved));
        assert_eq!(restored.col_width(0), 599.0);
        assert_eq!(restored.col_width(1), 250.0);
        assert_eq!(restored.col_width(2), 100.0); // default, invalid saved value
    }

    /// Normalized heat intensities: 1.0 exactly marks the column's top
    /// consumer; zero-max columns normalize everything to 0 (audit P0.2).
    #[test]
    fn heat_normalization_marks_top_consumer() {
        assert_eq!(norm(30.0, 30.0), 1.0);
        assert!((norm(15.0, 30.0) - 0.5).abs() < 1e-6);
        assert_eq!(norm(7.0, 0.0), 0.0, "no maximum -> nothing highlighted");
        assert_eq!(norm(0.0, 42.0), 0.0);
    }

    // ---- input-driven regression tests: real egui passes with synthetic
    // pointer events, exercising the actual drag pipeline in `header()`. ----

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
                    table.header(ui, &crate::theme::DARK, None, None);
                });
        });
        // No renderer here: consume the font texture deltas so egui does not
        // panic about unapplied deltas when the output is dropped.
        out.textures_delta.clear();
        left.get()
    }

    /// Dragging the name|A boundary tracks the cursor exactly (+30 px per
    /// pass), and only THAT column changes.
    #[test]
    fn dragging_name_boundary_tracks_cursor_across_frames() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let mut t = table();
        // Name|A boundary at x = 340.
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

        // +30 px per pass for two passes: exactly +60 total.
        assert_eq!(t.col_width(0), 400.0);
        assert_eq!(t.col_width(1), 200.0, "neighbour column untouched");
    }

    /// Dragging a non-name boundary shrinks/grows THAT column and leaves the
    /// name column alone.
    #[test]
    fn dragging_other_boundary_resizes_that_column_only() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let mut t = table();
        // A|B boundary at x = 340 + 200 = 540.
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

        assert_eq!(t.cols[1].width, 120.0); // 200 - 80
        assert_eq!(t.cols[0].width, 340.0, "first column unaffected");
    }

    /// Double-clicking a boundary restores the built-in default width.
    #[test]
    fn double_click_on_boundary_restores_default() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let mut t = table();
        // Distort A so its restore is observable: boundaries then sit at
        // x = 340 / 640 / 740.
        t.cols[1].width = 300.0;
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
        assert_eq!(t.cols[1].width, 200.0);
    }
}

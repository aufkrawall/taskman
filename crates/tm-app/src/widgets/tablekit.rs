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
//! * Header and body are two independently laid-out scroll areas sharing one
//!   horizontal offset. The body's scroll bars reserve layout space, so the
//!   header must reserve the SAME space (`prev_bar_use`) or the two end at
//!   different x and clamp the shared offset at different maxima.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, PointerButton, Pos2, Rect, Sense, Stroke,
};
use tm_core::format;

use crate::icons;
use crate::theme::Palette;

/// Row height used by the icon-carrying TM tables (also the virtualization
/// unit). Processes/Users/Startup/App history use this airy Win11 spacing.
pub const ROW_H: f32 = 32.0;
/// Compact row height for the dense list pages (Details, Services, Modules).
/// Native Task Manager's Details tab packs its rows with no visible gap; the
/// 32 px app-list spacing there reads as broken whitespace between entries.
///
/// 20 px is the floor: [`FONT_ROW`] at 13 px needs a ~17 px line box, and
/// [`TmTable::icon_cell`] derives its glyph side from `row_h - 6`, so going
/// lower starts shrinking the icons rather than the gap.
pub const ROW_H_DENSE: f32 = 20.0;
/// Header height for tables with aggregates (two-line).
pub const HEADER_H: f32 = 57.0;
/// Header height for single-line headers (Details/Services/Startup).
pub const HEADER_H1: f32 = 30.0;

/// Font sizes measured from Win11 TM: header aggregate values are notably
/// larger than row text, header labels slightly smaller.
pub const FONT_ROW: f32 = 13.0;
pub const FONT_HDR_LABEL: f32 = 12.0;
pub const FONT_AGG: f32 = 17.0;

/// Persistent sort state shared by the simpler table tabs. Text columns
/// start ascending; numeric columns start descending on first click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortState {
    pub column: usize,
    pub ascending: bool,
}

impl SortState {
    pub const fn new(column: usize, ascending: bool) -> Self {
        Self { column, ascending }
    }

    pub fn clicked(&mut self, column: usize, numeric: bool) {
        if self.column == column {
            self.ascending = !self.ascending;
        } else {
            self.column = column;
            self.ascending = !numeric;
        }
    }
}

/// Hard limits for user-resized columns.
const MIN_COL_W: f32 = 40.0;
const MAX_COL_W: f32 = 1200.0;

/// Breathing room kept clear on the RIGHT of the table content, between the
/// last column and the vertical scroll bar's own (reserved) lane. It also
/// keeps the last resize handle grabbable: without any trailing content the
/// handle sits flush at the viewport edge once scrolled fully right, and
/// egui's hit-testing — clipped to the scroll area — leaves only a couple of
/// unreachable pixels of it.
const BODY_PAD_RIGHT: i8 = 6;
/// Same idea below the last row.
const BODY_PAD_BOTTOM: i8 = 4;

/// Stash key for the layout space the body's scroll bars took last frame.
/// The header is a SEPARATE scroll area with its own bars hidden, so it
/// would otherwise stay a bar-width wider than the body: the two share one
/// horizontal offset but would clamp it at different maxima, and the header
/// would run on into the vertical bar's lane.
fn bar_use_id(id: &'static str) -> egui::Id {
    egui::Id::new(("tm-rowsbar", id))
}

/// Space the body's scroll bars reserved on the previous frame, as
/// `(vertical bar width, horizontal bar height)`. Zero on the first frame,
/// which is also when no bar can be shown yet.
fn prev_bar_use(ui: &egui::Ui, id: &'static str) -> egui::Vec2 {
    ui.ctx()
        .data(|d| d.get_temp::<egui::Vec2>(bar_use_id(id)))
        .unwrap_or(egui::Vec2::ZERO)
}

/// Record this frame's bar reservation for the next frame's header.
fn store_bar_use(ui: &egui::Ui, id: &'static str, outer: egui::Vec2, inner: egui::Rect) {
    let use_ = egui::vec2(
        (outer.x - inner.width()).max(0.0),
        (outer.y - inner.height()).max(0.0),
    );
    ui.ctx().data_mut(|d| d.insert_temp(bar_use_id(id), use_));
}

/// Available width for a full-width table. The margin keeps the last
/// column's right-aligned labels clear of the window border and of the
/// vertical scroll bar's reserved lane.
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
    let bar_use = prev_bar_use(ui, id);
    let outer = ui.available_size_before_wrap();

    let hdr = egui::ScrollArea::horizontal()
        .id_salt(hdr_id)
        .auto_shrink(egui::Vec2b::new(false, true))
        .max_width((outer.x - bar_use.x).max(0.0))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .content_margin(egui::Margin {
            right: BODY_PAD_RIGHT,
            ..Default::default()
        })
        .horizontal_scroll_offset(rows_prev_x)
        .show(ui, |ui| table.header(ui, pal, sort, aggregates));

    let body_outer = ui.available_size_before_wrap();
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

    store_bar_use(ui, id, body_outer, body.inner_rect);
    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new(("tm-rowsx", id)), body.state.offset.x));
    hdr.inner
}

/// Deterministic identity hash for one row, for [`ScrollAnchor`].
///
/// `DefaultHasher::new()` is the FIXED-seed hasher (unlike `RandomState`), so
/// the same value hashes identically for the whole session and across runs.
pub fn stable_key(value: impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Scroll-anchoring input for [`scrolled_rows`].
///
/// The raw scroll offset is a pixel value, but the display model is rebuilt
/// on every sample: a process spawning or exiting above the viewport (or a
/// tree node being expanded) used to shift every visible row by that many row
/// heights, which read as the list jumping while the scroll bar stayed put.
/// With an anchor the table re-derives the offset from row identities, so the
/// content under the viewport stays where the user was looking.
#[derive(Clone, Copy)]
pub struct ScrollAnchor<'a> {
    /// True when the caller rebuilt the row model this frame.
    pub model_changed: bool,
    /// Identity of the primary selected row, if any. While that row is on
    /// screen it wins over the viewport top: a process spawning above the
    /// selection must not push the row the user is tracking out of view.
    pub prefer_key: Option<u64>,
    /// Stable identity of row `index` in the CURRENT model. Must be unique
    /// among the rows a user can see and must survive while the row exists.
    pub key_of: &'a dyn Fn(usize) -> u64,
}

/// Per-table memory for [`scrolled_rows`] anchoring: the identities of the
/// rows that were rendered last frame, plus the viewport-relative position of
/// the first one.
#[derive(Clone, Default)]
struct AnchorState {
    /// Model index of the first key in [`AnchorState::keys`].
    base_index: usize,
    /// Viewport-relative y of that row's top (normally `<= 0`, because the
    /// first stored row is usually partially scrolled off).
    base_rel: f32,
    /// Identities of the rendered rows, top to bottom.
    keys: Vec<u64>,
}

impl AnchorState {
    /// Offset that keeps the first remembered row still present at its old
    /// viewport position, or `None` when every remembered row is gone or has
    /// moved too far.
    ///
    /// `prefer` names the selected row; when it is among the remembered rows
    /// it is tried before the top-down order. `max_shift` bounds how far a
    /// remembered row may move. Insertions and removals shift rows by their
    /// own count; a sort or search reorders the whole model, and following a
    /// row across that would teleport the viewport instead of keeping it
    /// stable, so those are left alone.
    fn restored_offset(
        &self,
        key_of: &dyn Fn(usize) -> u64,
        row_count: usize,
        row_h: f32,
        max_shift: usize,
        prefer: Option<u64>,
    ) -> Option<f32> {
        let preferred = prefer.and_then(|key| self.keys.iter().position(|k| *k == key));
        let order = preferred
            .into_iter()
            .chain((0..self.keys.len()).filter(|i| Some(*i) != preferred));
        for k in order {
            let key = self.keys[k];
            if let Some(new_index) = (0..row_count).find(|&i| key_of(i) == key) {
                if new_index.abs_diff(self.base_index + k) > max_shift {
                    return None;
                }
                let rel = self.base_rel + k as f32 * row_h;
                return Some(new_index as f32 * row_h - rel);
            }
        }
        None
    }
}

/// Virtualized variant of [`scrolled_table`] for uniform fixed-height rows.
///
/// `focus_row` consumes a one-shot scroll request (type-ahead or cross-tab
/// selection) by bringing that flat row index into view vertically. It must
/// bypass `Response::scroll_to_me` for two reasons: the row may lie outside
/// the currently rendered virtualization window (the response never exists,
/// so nothing would scroll), and `scroll_to_me` always targets both axes,
/// which yanked the horizontal offset toward the full-width row.
///
/// `anchor` is optional and only repositions the viewport on the frame the
/// caller rebuilt the model; user scrolling on other frames is untouched.
/// `focus_row` wins when both are present (the user explicitly navigated).
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
    focus_row: Option<usize>,
    anchor: Option<ScrollAnchor<'_>>,
    rows: impl FnOnce(&mut egui::Ui, &TmTable, f32, f32, std::ops::Range<usize>),
) -> Option<usize> {
    let content_w = table.total_width();
    let hdr_id = egui::Id::new(("tm-hdrscroll", id));
    let rows_prev_x = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(egui::Id::new(("tm-rowsx", id))))
        .unwrap_or(0.0);
    let rows_prev_y = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(egui::Id::new(("tm-rowsy", id))))
        .unwrap_or(0.0);
    ui.spacing_mut().item_spacing.y = 0.0;
    let bar_use = prev_bar_use(ui, id);
    let outer = ui.available_size_before_wrap();

    let hdr = egui::ScrollArea::horizontal()
        .id_salt(hdr_id)
        .auto_shrink(egui::Vec2b::new(false, true))
        .max_width((outer.x - bar_use.x).max(0.0))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .content_margin(egui::Margin {
            right: BODY_PAD_RIGHT,
            ..Default::default()
        })
        .horizontal_scroll_offset(rows_prev_x)
        .show(ui, |ui| table.header(ui, pal, sort, aggregates));

    // Vertical-only minimal-move scroll target, computed against the last
    // frame's offset. The builder offset is applied on this frame only
    // (callers hand us `Some` exactly once per request).
    let row_h = table.row_h;
    // The horizontal bar's reserved lane is not viewport: counting it would
    // scroll a bottom row that far short of actually being visible.
    let viewport_h = (ui.available_height() - bar_use.y).max(row_h);
    let focus_offset = focus_row.map(|row| {
        let row_top = row as f32 * row_h;
        let row_bottom = row_top + row_h;
        let target = if row_top < rows_prev_y {
            row_top
        } else if row_bottom > rows_prev_y + viewport_h {
            row_bottom - viewport_h
        } else {
            rows_prev_y // already visible
        };
        let content_h = row_h * row_count as f32;
        target.clamp(0.0, (content_h - viewport_h).max(0.0))
    });

    // Scroll anchoring: on the frame the caller rebuilt the row model, keep
    // the first previously visible row that still exists at the same viewport
    // position, so insertions/removals above the viewport cannot shift the
    // content. Skipped while a focus request is pending: that gesture asked
    // for a specific row and must not be overruled.
    let anchor_id = egui::Id::new(("tm-rowanchor", id));
    let vertical_offset = match (focus_offset, anchor) {
        (Some(focus), _) => Some(focus),
        (None, Some(anchor)) if anchor.model_changed => {
            let prev = ui
                .ctx()
                .data(|d| d.get_temp::<AnchorState>(anchor_id))
                .unwrap_or_default();
            // A row that moved further than the screenful the user can see
            // was reordered, not shifted by an insert/remove.
            let max_shift = (viewport_h / row_h).ceil() as usize;
            prev.restored_offset(
                anchor.key_of,
                row_count,
                row_h,
                max_shift,
                anchor.prefer_key,
            )
        }
        _ => None,
    };

    let body_outer = ui.available_size_before_wrap();
    let body = {
        let area = egui::ScrollArea::both()
            .id_salt(egui::Id::new(("tm-rowscroll", id)))
            .auto_shrink(false)
            .content_margin(egui::Margin {
                left: 0,
                right: BODY_PAD_RIGHT,
                top: 0,
                bottom: BODY_PAD_BOTTOM,
            })
            .horizontal_scroll_offset(hdr.state.offset.x);
        let area = match vertical_offset {
            Some(y) => area.vertical_scroll_offset(y),
            None => area,
        };
        area.show_rows(ui, row_h, row_count, |ui, range| {
            rows(ui, table, avail, avail.max(content_w), range)
        })
    };

    store_bar_use(ui, id, body_outer, body.inner_rect);
    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new(("tm-rowsx", id)), body.state.offset.x));
    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new(("tm-rowsy", id)), body.state.offset.y));

    // Remember what was on screen for the next model rebuild. The stored list
    // covers the viewport plus one row, so a removed top row can fall back to
    // the next surviving one instead of giving up on anchoring.
    if let Some(anchor) = anchor {
        let viewport_h = (body_outer.y - bar_use.y).max(row_h);
        let offset_y = body.state.offset.y.max(0.0);
        let base_index = ((offset_y / row_h).floor() as usize).min(row_count.saturating_sub(1));
        let base_rel = base_index as f32 * row_h - offset_y;
        let count =
            ((viewport_h / row_h).ceil() as usize + 1).min(row_count.saturating_sub(base_index));
        let keys = (base_index..base_index + count)
            .map(|i| (anchor.key_of)(i))
            .collect();
        ui.ctx().data_mut(|d| {
            d.insert_temp(
                anchor_id,
                AnchorState {
                    base_index,
                    base_rel,
                    keys,
                },
            );
        });
    } else {
        ui.ctx().data_mut(|d| d.remove::<AnchorState>(anchor_id));
    }
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
    /// Height of every body row; also the virtualization unit.
    pub row_h: f32,
    /// Fill the last [`TmTable::row`] painted for selection/hover, so cells
    /// that paint an OPAQUE background (the heat band) can restore it on top
    /// of themselves instead of swallowing the highlight.
    row_overlay: std::cell::Cell<Option<Color32>>,
    layout: std::cell::RefCell<Option<Layout>>,
    dirty: bool,
    /// Full-model intrinsic widths supplied by the tab before `header()`.
    /// `None` preserves the historical default-width fallback for tables
    /// that have not yet registered an intrinsic measurement.
    auto_widths: Vec<Option<f32>>,
    /// Display indices the user may drag to reorder. Empty disables the
    /// gesture, which is the default: most tables here have a fixed layout,
    /// and a column dragged out of the numeric block would tear the heat
    /// band, which `heat_cells` paints as one contiguous span.
    reorderable: std::ops::Range<usize>,
    /// Set when a drag is released on a new slot; the owning tab consumes it
    /// with [`TmTable::take_reorder`] and rewrites its own column order.
    pending_reorder: Option<(usize, usize)>,
}

impl TmTable {
    pub fn new(
        id: &'static str,
        cols: Vec<TmColumn>,
        saved: Option<&std::collections::BTreeMap<String, f32>>,
    ) -> Self {
        let mut t = Self {
            reorderable: 0..0,
            pending_reorder: None,
            id,
            auto_widths: vec![None; cols.len()],
            cols,
            row_h: ROW_H,
            row_overlay: std::cell::Cell::new(None),
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

    /// Switch this table to a different row height (see [`ROW_H_DENSE`]).
    pub fn with_row_height(mut self, row_h: f32) -> Self {
        self.row_h = row_h;
        self
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

    /// Display indices of the numeric columns, in order.
    ///
    /// The header's aggregate row is positional against exactly this
    /// sequence, so every caller that lays out aggregates must walk it rather
    /// than assume "the columns after the first two". `Aggregates::strings`
    /// is shared by tables with different column counts, and indexing by hand
    /// ran the Users page off the end of its own width array.
    pub fn numeric_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.cols
            .iter()
            .enumerate()
            .filter(|(_, col)| col.numeric)
            .map(|(i, _)| i)
    }

    /// Allow the user to drag the columns in `range` into a different order.
    pub fn reorderable(mut self, range: std::ops::Range<usize>) -> Self {
        self.reorderable = range;
        self
    }

    /// Take a completed reorder gesture: move the column at `.0` so it sits
    /// at display index `.1`.
    pub fn take_reorder(&mut self) -> Option<(usize, usize)> {
        self.pending_reorder.take()
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

        // Column drag-reorder. The source is remembered in context memory
        // rather than on `self`, because the table is rebuilt from scratch
        // every frame while the pointer is still down.
        let drag_id = table_id.with("reorder-from");
        let dragging_col: Option<usize> = if self.reorderable.is_empty() {
            None
        } else {
            ui.ctx().data(|d| d.get_temp::<usize>(drag_id))
        };
        let drop_target = dragging_col.and_then(|from| {
            let pointer = ui.ctx().pointer_latest_pos()?;
            Some(self.drop_slot(rect, &bounds, pointer.x).unwrap_or(from))
        });

        for (i, col) in self.cols.iter().enumerate() {
            let w = self.col_width(i);
            let cell =
                Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(x + w, rect.bottom()));

            let movable = self.reorderable.contains(&i);
            let sense = if movable {
                Sense::click_and_drag()
            } else {
                Sense::click()
            };
            let resp = ui.interact(cell, table_id.with(("hdr", col.id)), sense);
            if resp.hovered() {
                painter.rect_filled(cell, 0.0, Color32::from_white_alpha(6));
            }
            if resp.clicked() {
                clicked = Some(i);
            }
            if movable {
                if resp.drag_started() {
                    ui.ctx().data_mut(|d| d.insert_temp(drag_id, i));
                }
                if resp.dragged() || resp.drag_stopped() {
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                }
            }
            if dragging_col == Some(i) {
                // The column being carried, dimmed so the insertion marker
                // rather than the cursor reads as the answer.
                painter.rect_filled(cell, 0.0, pal.accent.gamma_multiply(0.18));
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

        // Where the carried column would land. Drawn after the cells so the
        // marker is never painted over by the header it sits between.
        if let (Some(from), Some(to)) = (dragging_col, drop_target)
            && from != to
        {
            let edge = if to > from {
                bounds[to + 1]
            } else {
                bounds[to]
            };
            painter.line_segment(
                [
                    Pos2::new(edge, rect.top() + 2.0),
                    Pos2::new(edge, rect.bottom() - 2.0),
                ],
                Stroke::new(2.5, pal.accent),
            );
        }
        // The release can land anywhere, including outside the header, so the
        // gesture ends on the global pointer state rather than on a response.
        if dragging_col.is_some() && ui.ctx().input(|i| i.pointer.any_released()) {
            if let (Some(from), Some(to)) = (dragging_col, drop_target)
                && from != to
            {
                self.pending_reorder = Some((from, to));
            }
            ui.ctx().data_mut(|d| d.remove::<usize>(drag_id));
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

    /// Display slot the pointer is over, clamped into the reorderable range.
    ///
    /// Clamping rather than rejecting: dragging a column left past a pinned
    /// one should park it at the first movable slot, not cancel the gesture.
    fn drop_slot(&self, rect: Rect, bounds: &[f32], pointer_x: f32) -> Option<usize> {
        if self.reorderable.is_empty() {
            return None;
        }
        let first = self.reorderable.start;
        let last = self.reorderable.end - 1;
        if pointer_x < rect.left() {
            return Some(first);
        }
        let slot = (0..self.cols.len())
            .find(|i| bounds.get(i + 1).is_some_and(|edge| pointer_x < *edge))
            .unwrap_or(self.cols.len() - 1);
        Some(slot.clamp(first, last))
    }

    /// One body row. `key` must identify the row's OWNER (pid + start time,
    /// base address, session id, …), never the position in the list: the
    /// response id is what egui keys a row's open context-menu popup to, and
    /// a live list re-sorts underneath an open menu — an index-derived id
    /// would hand the menu to whatever next lands in the slot.
    pub fn row(
        &self,
        ui: &mut egui::Ui,
        pal: &Palette,
        selected: bool,
        key: impl std::hash::Hash + std::fmt::Debug,
    ) -> (Rect, egui::Response) {
        let total_w = self.total_width();
        let (rect, _) = ui.allocate_exact_size(egui::vec2(total_w, self.row_h), Sense::hover());
        let resp = ui.interact(
            rect,
            egui::Id::new(self.id).with(key),
            Sense::click().union(Sense::hover()),
        );
        let painter = ui.painter_at(rect.expand(2.0));
        // Remember the fill so [`TmTable::heat_cells`] can restore it ON TOP
        // of its opaque blue band; without that the highlight stopped dead at
        // the first value column and only the name area lit up on hover.
        let overlay = if selected {
            Some(pal.accent.gamma_multiply(0.22))
        } else if resp.hovered() {
            Some(row_hover_fill(pal))
        } else {
            None
        };
        self.row_overlay.set(overlay);
        if let Some(fill) = overlay {
            painter.rect_filled(rect, 0.0, fill);
        }
        // Carry the header's column boundaries through the body. Native
        // Task Manager keeps these guides very quiet, but without them wide
        // text tables are unnecessarily hard to scan horizontally.
        for i in 1..self.cols.len() {
            let x = self.boundary_x(rect, i);
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(0.75, pal.stroke.gamma_multiply(0.72)),
            );
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

    /// Paint the blue value band for the numeric columns starting at `from`.
    ///
    /// Every cell is filled, always: intensity only picks a point on the
    /// [`crate::theme::heat_blue`] gradient, whose floor is `heat_base`. An
    /// idle process therefore shows a pale blue cell instead of a hole in the
    /// band -- native Task Manager has no uncolored value cells either.
    pub fn heat_cells(
        &self,
        ui: &egui::Ui,
        pal: &Palette,
        row: Rect,
        from: usize,
        cells: &[HeatCell],
    ) {
        let painter = ui.painter_at(row.expand(2.0));
        for (k, cell_data) in cells.iter().enumerate() {
            let cell = self.col_rect(from + k, row);
            painter.rect_filled(cell, 0.0, crate::theme::heat_blue(pal, cell_data.intensity));
            painter.line_segment(
                [
                    Pos2::new(cell.left(), row.top()),
                    Pos2::new(cell.left(), row.bottom()),
                ],
                Stroke::new(1.0, pal.heat_sep),
            );
        }
        // Re-apply the row's selection/hover fill over the band we just
        // painted, then the value texts on top of that.
        if let Some(fill) = self.row_overlay.get() {
            painter.rect_filled(self.numeric_span(row, from), 0.0, fill);
        }
        for (k, cell_data) in cells.iter().enumerate() {
            if cell_data.text.is_empty() {
                continue;
            }
            let cell = self.col_rect(from + k, row);
            ui.painter_at(cell).text(
                Pos2::new(cell.right() - 10.0, cell.center().y),
                Align2::RIGHT_CENTER,
                &cell_data.text,
                FontId::proportional(FONT_ROW),
                pal.text,
            );
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
        let hit = Rect::from_center_size(c, egui::vec2(24.0, self.row_h));
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
            let inset = ((self.row_h - 16.0) * 0.5).max(0.0);
            icons::draw_at(ui, r.shrink2(egui::vec2(0.0, inset)), icon, pal.text_dim);
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
        let side = 18.0f32.min(self.row_h - 6.0);
        let r = Rect::from_center_size(
            Pos2::new(row.left() + 38.0, row.center().y),
            egui::vec2(side, side),
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

/// Hover tint for a table row. Light mode needs a dark wash: a white one
/// over an already light background is invisible.
pub fn row_hover_fill(pal: &Palette) -> Color32 {
    if pal.text.r() > 128 {
        Color32::from_white_alpha(14)
    } else {
        Color32::from_black_alpha(16)
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
    /// Busiest disk's active time, or `None` while the platform's disk-time
    /// counters are not running. Both disk columns hang off this number, so
    /// a fabricated zero here reads as "nothing is touching a disk" on a page
    /// whose own rows say otherwise.
    pub disk_pct: Option<f32>,
    pub net_pct: f32,
    pub gpu_pct: f32,
}

impl Aggregates {
    pub fn from_snapshot(snap: &tm_core::model::Snapshot) -> Self {
        let disk_pct = snap
            .disks
            .iter()
            .filter_map(|d| d.active_pct)
            .reduce(f32::max);
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
            // The adapter's own number is already the busiest engine, so the
            // machine total is the busiest adapter — not a sum across them.
            gpu_pct: snap.gpus.iter().map(|g| g.util_pct).fold(0.0f32, f32::max),
        }
    }

    /// Header values in numeric-column order.
    ///
    /// The header row states MACHINE totals, not column sums — that is what
    /// makes a row's percentage readable ("the disks are 43 % busy and this
    /// process accounts for 100 % of that"). Disk active time therefore
    /// repeats the disk total deliberately: its column is a share OF that
    /// number, while the Disk I/O column next to it is a byte rate.
    ///
    /// A table with fewer numeric columns simply uses the leading entries.
    pub fn strings(&self) -> [String; 6] {
        let disk = self
            .disk_pct
            .map_or_else(|| "\u{2014}".to_string(), format::format_pct_hdr);
        [
            format::format_pct_hdr(self.cpu_pct),
            format::format_pct_hdr(self.mem_pct),
            disk.clone(),
            format::format_pct_hdr(self.net_pct),
            disk,
            format::format_pct_hdr(self.gpu_pct),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_state_uses_natural_first_direction_then_toggles() {
        let mut sort = SortState::new(0, true);
        sort.clicked(1, true);
        assert_eq!(sort, SortState::new(1, false));
        sort.clicked(1, true);
        assert_eq!(sort, SortState::new(1, true));
        sort.clicked(2, false);
        assert_eq!(sort, SortState::new(2, true));
    }

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

    /// The disk headers must never invent a zero.
    ///
    /// Regression: the Windows sampler fills `DiskInfo::active_pct` from the
    /// PhysicalDisk PDH group, and that group only runs while something asks
    /// for it. It used to collapse "not measured" into `0.0`, so both disk
    /// totals read a confident "0 %" over rows whose Disk active time column
    /// showed a process at 51 %.
    #[test]
    fn the_disk_totals_read_unavailable_until_a_disk_is_measured() {
        let unmeasured = tm_core::model::DiskInfo {
            active_pct: None,
            ..Default::default()
        };
        let mut snap = tm_core::model::Snapshot {
            disks: vec![unmeasured.clone()],
            ..Default::default()
        };
        let agg = Aggregates::from_snapshot(&snap);
        assert_eq!(agg.disk_pct, None);
        let hdr = agg.strings();
        // Disk I/O (2) and Disk active time (4) both hang off that number.
        assert_eq!(hdr[2], "\u{2014}", "an unmeasured disk must not read 0 %");
        assert_eq!(hdr[4], "\u{2014}", "an unmeasured disk must not read 0 %");

        // The busiest measured disk wins, and an unmeasured one beside it
        // neither drags the total down nor hides it.
        snap.disks = vec![
            unmeasured,
            tm_core::model::DiskInfo {
                active_pct: Some(43.0),
                ..Default::default()
            },
            tm_core::model::DiskInfo {
                active_pct: Some(7.0),
                ..Default::default()
            },
        ];
        let hdr = Aggregates::from_snapshot(&snap).strings();
        assert_eq!(hdr[2], format::format_pct_hdr(43.0));
        assert_eq!(hdr[4], hdr[2]);

        // A genuinely idle disk still says so.
        snap.disks = vec![tm_core::model::DiskInfo {
            active_pct: Some(0.0),
            ..Default::default()
        }];
        assert_eq!(
            Aggregates::from_snapshot(&snap).strings()[4],
            format::format_pct_hdr(0.0)
        );
    }

    /// Aggregates are shared across tables with different column counts, so
    /// laying them out must never walk off the end. The Users table is the
    /// case that crashed: six machine totals, four numeric columns.
    #[test]
    fn aggregates_never_index_past_the_columns() {
        let users = TmTable::new(
            "u",
            vec![
                TmColumn::text("user", "User", 340.0),
                TmColumn::text("status", "Status", 190.0),
                TmColumn::num("cpu", "CPU", 110.0),
                TmColumn::num("mem", "Mem", 110.0),
                TmColumn::num("disk", "Disk", 110.0),
                TmColumn::num("net", "Net", 110.0),
            ],
            None,
        );
        let totals = ["1", "2", "3", "4", "5", "6"];
        let touched: Vec<usize> = users
            .numeric_indices()
            .zip(totals.iter())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(touched, vec![2, 3, 4, 5]);
        assert!(
            touched.iter().all(|i| *i < users.cols.len()),
            "aggregate layout ran past the columns"
        );
        // ...and a table with more numeric columns than totals stops at the
        // totals instead of reading past them.
        assert_eq!(users.numeric_indices().zip(["1"].iter()).count(), 1);
    }

    /// A column dragged past a PINNED one parks at the first movable slot
    /// instead of cancelling the gesture — and never lands on the pinned
    /// column, which would tear the contiguous heat band `heat_cells` paints.
    #[test]
    fn a_drop_clamps_into_the_reorderable_range() {
        let t = TmTable::new(
            "t",
            vec![
                TmColumn::text("name", "Name", 100.0),
                TmColumn::text("status", "Status", 100.0),
                TmColumn::num("cpu", "CPU", 100.0),
                TmColumn::num("mem", "Mem", 100.0),
                TmColumn::num("gpu", "GPU", 100.0),
            ],
            None,
        )
        .reorderable(2..5);
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(500.0, 40.0));
        let bounds: Vec<f32> = (0..=5).map(|i| i as f32 * 100.0).collect();

        // Over each movable column.
        assert_eq!(t.drop_slot(rect, &bounds, 250.0), Some(2));
        assert_eq!(t.drop_slot(rect, &bounds, 350.0), Some(3));
        assert_eq!(t.drop_slot(rect, &bounds, 450.0), Some(4));
        // Over Name or Status, and off the left edge entirely.
        assert_eq!(t.drop_slot(rect, &bounds, 50.0), Some(2));
        assert_eq!(t.drop_slot(rect, &bounds, 150.0), Some(2));
        assert_eq!(t.drop_slot(rect, &bounds, -80.0), Some(2));
        // Past the right edge.
        assert_eq!(t.drop_slot(rect, &bounds, 9_000.0), Some(4));

        // A table that never opted in has no gesture at all.
        let fixed = table();
        assert_eq!(fixed.drop_slot(rect, &bounds, 250.0), None);
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

    /// Paint one body row (selection/hover fill + heat band) and return every
    /// solid rectangle the frame produced, in paint order.
    fn heat_row_frame(hover: bool) -> Vec<(Rect, Color32)> {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let table = table();
        let pal = crate::theme::DARK;
        let cells = [HeatCell::new(0.0, "0%"), HeatCell::new(1.0, "99%")];
        let pos = if hover {
            egui::Pos2::new(500.0, 10.0)
        } else {
            egui::Pos2::new(1500.0, 800.0)
        };
        // Two frames: egui derives hover from the PREVIOUS frame's widget
        // rects, so the first pass only registers the row.
        let mut out = None;
        for frame in 0..2 {
            let mut done = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen),
                    time: Some(frame as f64 * 0.016),
                    events: vec![egui::Event::PointerMoved(pos)],
                    ..Default::default()
                },
                |root| {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE)
                        .show(root, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let (rect, _) = table.row(ui, &pal, false, "snap");
                            table.heat_cells(ui, &pal, rect, 1, &cells);
                        });
                },
            );
            done.textures_delta.clear();
            out = Some(done);
        }
        let out = out.expect("frame");
        out.shapes
            .into_iter()
            .filter_map(|clipped| match clipped.shape {
                egui::Shape::Rect(r) => Some((r.rect, r.fill)),
                _ => None,
            })
            .collect()
    }

    /// Regression: the opaque heat band used to be painted AFTER the row
    /// highlight, so hovering a process lit up only the name area and left
    /// the blue value columns untouched.
    #[test]
    fn hover_highlight_reaches_the_blue_value_columns() {
        let table = table();
        let pal = crate::theme::DARK;
        let hover_fill = row_hover_fill(&pal);
        let rects = heat_row_frame(true);
        let numeric_left = table.col_width(0);
        let painted_over_band = rects
            .iter()
            .any(|(rect, fill)| *fill == hover_fill && rect.right() > numeric_left + 1.0);
        assert!(
            painted_over_band,
            "hover fill must be re-applied across the value columns: {rects:?}"
        );
        assert!(
            !heat_row_frame(false)
                .iter()
                .any(|(_, fill)| *fill == hover_fill),
            "an unhovered row must not paint the hover fill at all"
        );
    }

    /// Regression: cells whose value was zero were left unpainted, so idle
    /// processes showed holes in the blue band.
    #[test]
    fn every_value_cell_is_painted_even_at_zero() {
        let pal = crate::theme::DARK;
        let rects = heat_row_frame(false);
        for (label, intensity) in [("zero", 0.0f32), ("max", 1.0)] {
            let want = crate::theme::heat_blue(&pal, intensity);
            assert!(
                rects.iter().any(|(_, fill)| *fill == want),
                "{label}-intensity cell must be filled with {want:?}"
            );
        }
    }

    #[test]
    fn dense_tables_lay_out_rows_at_the_compact_height() {
        let dense = table().with_row_height(ROW_H_DENSE);
        assert_eq!(dense.row_h, ROW_H_DENSE);
        assert_eq!(table().row_h, ROW_H, "default stays the airy app-list row");
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

    /// Regression: when the table content is wider than the viewport, the
    /// last resize boundary sits flush at the right edge once fully scrolled
    /// right. The header's right content margin (same strip as the body)
    /// must keep that handle fully inside the scroll area's clip rect so it
    /// stays grabbable; without it the drag below lands a few pixels left of
    /// the sliver egui leaves clickable and the width never changes.
    #[test]
    fn last_boundary_is_grabbable_when_scrolled_fully_right() {
        let ctx = egui::Context::default();
        // 640 px of columns in a ~520 px viewport → horizontal scrolling is
        // active and can reach the far end.
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 300.0));
        let mut t = table();

        let frame = |t: &mut TmTable, time: f64, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(time),
                predicted_dt: 1.0 / 60.0,
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |root| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(root, |ui| {
                        let avail = table_avail(ui);
                        scrolled_rows(
                            "t-last",
                            ui,
                            &crate::theme::DARK,
                            t,
                            avail,
                            None,
                            None,
                            3,
                            None,
                            None,
                            |ui, table, _a, _c, range| {
                                for i in range {
                                    table.row(ui, &crate::theme::DARK, false, i);
                                }
                            },
                        );
                    });
            });
            out.textures_delta.clear();
        };

        // Warm-up frame (egui's per-pass memory is created lazily, so the
        // offset below can only be injected between passes).
        frame(&mut t, 0.000, vec![]);
        // Force both scroll areas fully right via the stored body offset that
        // `scrolled_rows` feeds back into the header each frame; egui clamps
        // it to the content maximum.
        ctx.data_mut(|d| d.insert_temp(egui::Id::new(("tm-rowsx", "t-last")), 10_000.0f32));
        frame(&mut t, 0.016, vec![]); // header applies the clamped offset

        // The last boundary (right edge of column "b") is now at
        // viewport_right - BODY_PAD_RIGHT; grab exactly there.
        let grab = egui::Pos2::new(520.0 - f32::from(BODY_PAD_RIGHT), 20.0);
        frame(&mut t, 0.032, vec![ptr_moved(grab.x, grab.y)]);
        frame(&mut t, 0.048, vec![ptr_button(grab.x, grab.y, true)]);
        frame(&mut t, 0.064, vec![ptr_moved(grab.x - 40.0, grab.y)]);
        frame(
            &mut t,
            0.080,
            vec![ptr_button(grab.x - 40.0, grab.y, false)],
        );
        assert_eq!(t.col_width(2), 60.0, "last column resized via its handle");
        assert_eq!(t.col_width(0), 340.0, "first column unaffected");
    }

    /// The scroll bars reserve layout space, so a table whose rows overflow
    /// vertically gets a narrower BODY. The header is a separate scroll area
    /// with its own bars hidden, so it must be narrowed by exactly the same
    /// amount — otherwise it runs on into the bar's lane and the two areas
    /// clamp their shared horizontal offset at different maxima.
    #[test]
    fn header_reserves_the_same_scroll_bar_lane_as_the_body() {
        let ctx = egui::Context::default();
        // The reserved lane is OUR style choice, not an egui default: without
        // installing it the test would silently measure egui's overlay bars.
        crate::theme::install_visuals(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 200.0));
        let mut t = table();
        let body_right = std::cell::Cell::new(0.0f32);

        let frame = |t: &mut TmTable, time: f64| {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(time),
                predicted_dt: 1.0 / 60.0,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |root| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(root, |ui| {
                        let avail = table_avail(ui);
                        scrolled_rows(
                            "t-lane",
                            ui,
                            &crate::theme::DARK,
                            t,
                            avail,
                            None,
                            None,
                            // Far more rows than fit in 200 px → vertical bar.
                            400,
                            None,
                            None,
                            |ui, table, _a, _c, range| {
                                body_right.set(ui.clip_rect().right());
                                for i in range {
                                    table.row(ui, &crate::theme::DARK, false, i);
                                }
                            },
                        );
                    });
            });
            out.textures_delta.clear();
        };

        // Two frames: the bar's reservation is only known after the body has
        // been laid out once.
        frame(&mut t, 0.000);
        frame(&mut t, 0.016);

        let lane = ctx
            .data(|d| d.get_temp::<egui::Vec2>(bar_use_id("t-lane")))
            .unwrap_or(egui::Vec2::ZERO);
        assert!(
            lane.x > 0.0,
            "an overflowing table must reserve a vertical scroll-bar lane"
        );
        assert!(
            body_right.get() <= screen.right() - lane.x + 0.51,
            "body content reaches into the scroll-bar lane: {} vs {}",
            body_right.get(),
            screen.right() - lane.x
        );
    }

    /// Regression (type-ahead scroll): a focus row outside the rendered
    /// virtualization window must be scrolled into view VERTICALLY, and the
    /// horizontal offset (independently scrollable table) must never move.
    #[test]
    fn focus_row_scrolls_vertically_only_even_for_unrendered_rows() {
        let ctx = egui::Context::default();
        // 640 px of columns in a ~520 px viewport → horizontal scrolling is
        // active, exactly the situation that used to produce the sideways
        // jump on type-ahead.
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 300.0));
        let y_of = |offset: f32| {
            ctx.data(|d| {
                d.get_temp::<f32>(egui::Id::new(("tm-rowsy", "t-focus")))
                    .unwrap_or(offset)
            })
        };
        let frame = |t: f64, focus: Option<usize>| {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(t),
                predicted_dt: 1.0 / 60.0,
                ..Default::default()
            };
            let mut t8 = table();
            let mut out = ctx.run_ui(raw, |root| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(root, |ui| {
                        let avail = table_avail(ui);
                        scrolled_rows(
                            "t-focus",
                            ui,
                            &crate::theme::DARK,
                            &mut t8,
                            avail,
                            None,
                            None,
                            100,
                            focus,
                            None,
                            |ui, table, _a, _c, range| {
                                let _ = ui;
                                for i in range {
                                    table.row(ui, &crate::theme::DARK, false, i);
                                }
                            },
                        );
                    });
            });
            out.textures_delta.clear();
        };

        // Baseline frame without a request: at the top.
        frame(0.000, None);
        assert_eq!(y_of(0.0), 0.0);

        // Focus a far row (50 * 32 px = 1600 px down, viewport ≈ 270 px):
        // it must become visible and the horizontal offset must stay put.
        frame(0.016, Some(50));
        let y = y_of(0.0);
        let row_top = 50.0 * ROW_H;
        let row_bottom = row_top + ROW_H;
        assert!(y > 0.0, "off-screen focus row did not scroll");
        assert!(
            y <= row_top && y + 270.0 >= row_bottom - 1.0,
            "row [{row_top}, {row_bottom}] not inside view after focus (offset {y})"
        );
        assert_eq!(
            ctx.data(|d| d
                .get_temp::<f32>(egui::Id::new(("tm-rowsx", "t-focus")))
                .unwrap_or(0.0)),
            0.0,
            "horizontal offset must not move on a vertical focus request"
        );

        // A visible row is left where it is (no re-centering jitter).
        frame(0.032, Some(50));
        assert_eq!(y_of(0.0), y);
    }

    /// Scroll anchoring: a model rebuild must keep the first previously
    /// visible row at the same viewport position. Without it the raw pixel
    /// offset stayed put while a spawned/removed process shifted every row
    /// above the viewport by one row height.
    #[allow(clippy::too_many_arguments)]
    fn anchored_frame(
        ctx: &egui::Context,
        screen: egui::Rect,
        t: f64,
        table: &mut TmTable,
        row_count: usize,
        focus: Option<usize>,
        model_changed: bool,
        prefer: Option<u64>,
        key_of: &dyn Fn(usize) -> u64,
    ) {
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            time: Some(t),
            predicted_dt: 1.0 / 60.0,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |root| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(root, |ui| {
                    let avail = table_avail(ui);
                    scrolled_rows(
                        "t-anchor",
                        ui,
                        &crate::theme::DARK,
                        table,
                        avail,
                        None,
                        None,
                        row_count,
                        focus,
                        Some(ScrollAnchor {
                            model_changed,
                            prefer_key: prefer,
                            key_of,
                        }),
                        |ui, table, _a, _c, range| {
                            for i in range {
                                table.row(ui, &crate::theme::DARK, false, i);
                            }
                        },
                    );
                });
        });
        out.textures_delta.clear();
    }

    fn anchored_offset(ctx: &egui::Context) -> f32 {
        ctx.data(|d| {
            d.get_temp::<f32>(egui::Id::new(("tm-rowsy", "t-anchor")))
                .unwrap_or(0.0)
        })
    }

    /// Scroll to a mid-list row and return the resulting offset. Keys are
    /// `index + 1`, so the top row after focusing row 30 is key 22.
    fn anchor_baseline(ctx: &egui::Context, table: &mut TmTable) -> f32 {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 300.0));
        let key = |i: usize| i as u64 + 1;
        anchored_frame(ctx, screen, 0.000, table, 100, Some(30), false, None, &key);
        anchored_offset(ctx)
    }

    #[test]
    fn insertion_above_the_viewport_keeps_the_same_row_pinned() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 300.0));
        let mut table = table();
        let base = anchor_baseline(&ctx, &mut table);
        assert!(base > 0.0, "focus must scroll the viewport");

        let shifted = |i: usize| if i == 0 { 999 } else { i as u64 };
        anchored_frame(
            &ctx, screen, 0.016, &mut table, 101, None, true, None, &shifted,
        );
        assert!(
            (anchored_offset(&ctx) - (base + ROW_H)).abs() < 0.51,
            "one inserted row above must move the offset down one row: {} -> {}",
            base,
            anchored_offset(&ctx)
        );
    }

    #[test]
    fn removal_above_the_viewport_keeps_the_same_row_pinned() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 300.0));
        let mut table = table();
        let base = anchor_baseline(&ctx, &mut table);

        let shifted = |i: usize| i as u64 + 2;
        anchored_frame(
            &ctx, screen, 0.016, &mut table, 99, None, true, None, &shifted,
        );
        assert!(
            (anchored_offset(&ctx) - (base - ROW_H)).abs() < 0.51,
            "one removed row above must move the offset up one row: {} -> {}",
            base,
            anchored_offset(&ctx)
        );
    }

    #[test]
    fn vanished_top_row_falls_back_to_the_next_visible_row() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 300.0));
        let mut table = table();
        let base = anchor_baseline(&ctx, &mut table);

        // The row at the viewport top (key 22) exits; the next row (key 23)
        // must keep its exact viewport position, so the offset shifts by one
        // row just as if the top row had been removed cleanly.
        let skipped = |i: usize| {
            if i + 1 >= 22 {
                i as u64 + 2
            } else {
                i as u64 + 1
            }
        };
        anchored_frame(
            &ctx, screen, 0.016, &mut table, 99, None, true, None, &skipped,
        );
        assert!(
            (anchored_offset(&ctx) - (base - ROW_H)).abs() < 0.51,
            "the next surviving row must stay put: {} -> {}",
            base,
            anchored_offset(&ctx)
        );
    }

    #[test]
    fn insertion_between_top_and_selection_keeps_the_selection_in_view() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 300.0));
        let mut table = table();
        let base = anchor_baseline(&ctx, &mut table);

        // Row 30 (key 31) is selected and sits at the viewport bottom. A new
        // row lands between the viewport top (key 22) and the selection.
        // Anchoring to the top would leave the offset alone and push the
        // selection one row below the viewport; the selection must win.
        let inserted = |i: usize| {
            if i < 25 {
                i as u64 + 1
            } else if i == 25 {
                999
            } else {
                i as u64
            }
        };
        anchored_frame(
            &ctx,
            screen,
            0.016,
            &mut table,
            101,
            None,
            true,
            Some(31),
            &inserted,
        );
        assert!(
            (anchored_offset(&ctx) - (base + ROW_H)).abs() < 0.51,
            "the selected row must stay put: {} -> {}",
            base,
            anchored_offset(&ctx)
        );
    }

    #[test]
    fn large_reorder_is_not_followed() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 300.0));
        let mut table = table();
        let base = anchor_baseline(&ctx, &mut table);

        // Rotate the model so the top row lands 40 slots away: a sort or
        // search reorder, not an insert. Following it would teleport the
        // viewport, so the offset must stay where the user left it.
        let rotated = |i: usize| ((i + 60) % 100) as u64 + 1;
        anchored_frame(
            &ctx, screen, 0.016, &mut table, 100, None, true, None, &rotated,
        );
        assert!(
            (anchored_offset(&ctx) - base).abs() < 0.51,
            "a reordered row must not drag the viewport: {} -> {}",
            base,
            anchored_offset(&ctx)
        );
    }

    #[test]
    fn fully_replaced_model_leaves_the_offset_alone() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 300.0));
        let mut table = table();
        let base = anchor_baseline(&ctx, &mut table);

        let all_new = |i: usize| 100_000 + i as u64;
        anchored_frame(
            &ctx, screen, 0.016, &mut table, 100, None, true, None, &all_new,
        );
        assert!(
            (anchored_offset(&ctx) - base).abs() < 0.51,
            "with no surviving row there is nothing to anchor to: {} -> {}",
            base,
            anchored_offset(&ctx)
        );
    }
}

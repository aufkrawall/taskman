//! Custom chart widgets: filled rolling line charts like Windows Task
//! Manager's Performance tab, plus a compact per-core grid variant.
//!
//! Two things every chart here shares:
//!
//! * **Pixel snapping.** Card frames, gridlines and borders are axis-aligned
//!   hairlines. Laid out at fractional point coordinates they straddle two
//!   device-pixel rows and the whole tile reads as blurry — worst on the
//!   per-core grid, whose cell size is `available_width / columns` and is
//!   therefore fractional by construction. Every frame is rounded to whole
//!   pixels and every hairline to a pixel CENTRE, at exactly one physical
//!   pixel wide.
//! * **A cursor-anchored hover readout.** egui's `on_hover_text` anchors its
//!   tooltip to the WIDGET rect, so on a 700 px wide chart the box appears at
//!   a corner far from the sample it describes — and at a different corner
//!   per chart, which is what made the readout look like it only worked on
//!   some graphs. [`readout`] paints at the pointer instead.

use eframe::egui::emath::GuiRounding;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Response, Shape, Stroke, Vec2};
use tm_core::i18n::{self, K};

/// Formats one sample for the hover readout. A plain `fn` pointer so a chart
/// can carry its unit without a lifetime parameter or a boxed closure.
pub type ValueFmt = fn(f64) -> String;

/// Percentage readout, the default for utilization charts.
pub fn fmt_percent(v: f64) -> String {
    format!("{} %", tm_core::format::num_fixed(v, 1))
}

/// The x axis of a rolling chart: when the samples were taken, and how wide
/// the axis is in time.
///
/// `window_ms` is the CONFIGURED window ("60 seconds"), never the extent of
/// whatever data has arrived so far. Deriving the axis from the samples — as
/// `last - first` — lets it rescale itself while the history fills: eight
/// seconds of data stretched across a caption that says sixty, with every new
/// sample squeezing the older ones a little further left instead of scrolling
/// them. What that looks like on screen is a graph whose left half is frozen
/// while only the right edge moves. Anchoring the axis to the window leaves
/// the part of it that has no data yet empty, and scrolls everything at the
/// one rate.
#[derive(Clone, Copy)]
pub struct TimeAxis<'a> {
    pub stamps: &'a [u64],
    pub window_ms: u64,
}

impl<'a> TimeAxis<'a> {
    pub fn new(stamps: &'a [u64], window_s: u32) -> Self {
        Self {
            stamps,
            window_ms: u64::from(window_s) * 1_000,
        }
    }

    /// `(left edge instant, axis width)`, or `None` for an axis with nothing
    /// on it yet. The right edge is the newest sample.
    fn bounds(&self) -> Option<(u64, u64)> {
        let end = *self.stamps.last()?;
        let window = self.window_ms.max(1);
        Some((end.saturating_sub(window), window))
    }
}

// ------------------------------------------------------------ series colours

/// Opacity of a series' area fill. Deliberately well under half strength so
/// the full-brightness polyline always reads as a distinct OUTLINE on top of
/// its own infill, the way Task Manager's graphs do.
const FILL_ALPHA: u8 = 72;

/// Opacity of the kernel-time band. Near-opaque: it is a darker region carved
/// out of the user-time fill, not another translucent wash over it.
const KERNEL_ALPHA: u8 = 190;

/// Translucent area fill for a series drawn with `color`.
///
/// `from_rgba_UNmultiplied` is load-bearing here. These fills used to be built
/// with `from_rgba_premultiplied(r, g, b, 34)`, which hands egui full-strength
/// colour components alongside an alpha claiming they had already been scaled
/// down by it — the blend then ADDS the series colour to the background
/// instead of mixing towards it. A CPU series over the dark card fill
/// composited to about `#71e7ff`: the "translucent" area came out brighter
/// than the `#4cc2ff` line it belonged to, so every graph looked like a solid
/// slab with no outline at all.
fn area_fill(color: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), FILL_ALPHA)
}

/// Kernel-time band fill: the caller's kernel colour, near-opaque so it carves
/// a distinct region out of the user-time fill instead of tinting it.
///
/// The colour comes from the caller rather than being derived here (it used to
/// be `color / 2`), so a core tile and the aggregate CPU chart paint kernel
/// time in the SAME colour — they were two shades of blue apart.
fn kernel_fill(color: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), KERNEL_ALPHA)
}

/// Fill the area between an x-monotone polyline and a horizontal baseline.
///
/// egui tessellates polygon fills as a fan from the first vertex
/// (`fill_closed_path`), which is only correct for CONVEX polygons — on
/// concave spans the fan cuts straight across dips, overfilling above the
/// line and leaving unpainted gaps under it (the "fake ramp / cliff"
/// artifact that made every graph look wrong). An explicit triangle strip
/// along the polyline fills exactly between line and baseline.
fn fill_area_to_baseline(painter: &egui::Painter, pts: &[Pos2], baseline_y: f32, fill: Color32) {
    painter.add(Shape::mesh(area_strip_mesh(pts, baseline_y, fill)));
}

/// Triangle strip for [`fill_area_to_baseline`]: two vertices per point
/// (the sample and its baseline projection), two triangles per segment.
fn area_strip_mesh(pts: &[Pos2], baseline_y: f32, fill: Color32) -> egui::Mesh {
    let mut mesh = egui::Mesh::default();
    if pts.len() < 2 || fill == Color32::TRANSPARENT {
        return mesh;
    }
    mesh.vertices.reserve(pts.len() * 2);
    mesh.indices.reserve((pts.len() - 1) * 6);
    for p in pts {
        let top = mesh.vertices.len() as u32;
        mesh.colored_vertex(*p, fill);
        mesh.colored_vertex(Pos2::new(p.x, baseline_y), fill);
        if top >= 2 {
            let (lt, lb) = (top - 2, top - 1);
            mesh.add_triangle(lt, top, lb);
            mesh.add_triangle(lb, top, top + 1);
        }
    }
    mesh
}

// ---------------------------------------------------------------- pixel grid

/// Width, in points, of a line exactly one physical pixel wide.
fn hairline(ppp: f32) -> f32 {
    if ppp > 0.0 { 1.0 / ppp } else { 1.0 }
}

/// Snap a laid-out rect onto the physical pixel grid.
///
/// Chart rects come out of egui's layout at fractional point coordinates
/// (`(width - gaps) / columns`). Painting a hairline border on such a rect
/// spreads it across two pixel rows, and `painter_at` then clips it at a
/// fractional boundary so the four edges do not even blur alike. Rounding
/// first makes the frame land on real pixels.
fn snap_rect(rect: Rect, ppp: f32) -> Rect {
    rect.round_to_pixels(ppp)
}

/// Snap a hairline's coordinate to the centre of a physical pixel, so a
/// one-pixel stroke covers that pixel and nothing else.
fn snap_line(v: f32, ppp: f32) -> f32 {
    v.round_to_pixel_center(ppp)
}

/// Card background, grid and border shared by every chart shape here.
/// `cols`/`rows` are the number of CELLS, so `cols = 4` draws 3 vertical
/// gridlines.
fn paint_frame(
    painter: &egui::Painter,
    rect: Rect,
    ppp: f32,
    pal: &crate::theme::Palette,
    cols: usize,
    rows: usize,
) {
    let hair = hairline(ppp);
    painter.rect_filled(rect, 4.0, pal.card_bg);
    for k in 1..rows {
        let y = snap_line(rect.top() + rect.height() * k as f32 / rows as f32, ppp);
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(hair, pal.chart_grid),
        );
    }
    for k in 1..cols {
        let x = snap_line(rect.left() + rect.width() * k as f32 / cols as f32, ppp);
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(hair, pal.chart_grid),
        );
    }
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(hair, pal.stroke),
        egui::StrokeKind::Inside,
    );
}

// ------------------------------------------------------------- hover readout

/// One line of a hover readout: colour swatch, series name, formatted value.
pub struct ReadoutRow {
    pub color: Color32,
    pub label: String,
    pub value: String,
}

/// Age caption for a hovered sample ("12 s ago" / "vor 12 s").
fn age_label(age_ms: u64) -> String {
    if age_ms < 500 {
        return i18n::tr(K::GraphNow).to_string();
    }
    let secs = (age_ms as f64 / 1000.0).round() as u64;
    if secs < 60 {
        i18n::trf(K::GraphAgoSeconds, &[&secs.to_string()])
    } else {
        i18n::trf(
            K::GraphAgoMinutes,
            &[&format!("{}:{:02}", secs / 60, secs % 60)],
        )
    }
}

/// Paint a floating value readout anchored at `at` (the pointer).
///
/// Drawn on the tooltip layer by hand rather than through
/// `Response::on_hover_text`: that helper places the box against the widget
/// rect, which on a full-width Performance chart means "somewhere near a
/// corner", and it cannot show a colour swatch per series — the thing that
/// makes a two-series chart (send vs. receive, read vs. write) readable.
fn readout(ui: &egui::Ui, at: Pos2, head: &str, rows: &[ReadoutRow]) {
    if rows.is_empty() {
        return;
    }
    let pal = crate::theme::palette(ui);
    let ppp = ui.ctx().pixels_per_point();
    let head_font = FontId::proportional(11.0);
    let font = FontId::proportional(12.5);
    let layout = |text: &str, font: &FontId| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), font.clone(), pal.text)
    };

    const PAD_X: f32 = 9.0;
    const PAD_Y: f32 = 7.0;
    const SWATCH: f32 = 8.0;
    const SWATCH_GAP: f32 = 7.0;
    const LABEL_GAP: f32 = 16.0;
    const ROW_H: f32 = 17.0;

    let head_galley = (!head.is_empty()).then(|| layout(head, &head_font));
    let label_w = rows
        .iter()
        .map(|r| layout(&r.label, &font).size().x)
        .fold(0.0f32, f32::max);
    let value_w = rows
        .iter()
        .map(|r| layout(&r.value, &font).size().x)
        .fold(0.0f32, f32::max);
    let body_w = SWATCH + SWATCH_GAP + label_w + LABEL_GAP + value_w;
    let head_w = head_galley.as_ref().map_or(0.0, |g| g.size().x);
    let width = 2.0 * PAD_X + body_w.max(head_w);
    let head_h = head_galley.as_ref().map_or(0.0, |g| g.size().y + 4.0);
    let height = 2.0 * PAD_Y + head_h + ROW_H * rows.len() as f32;

    // Place below-right of the cursor, flipping at the screen edges so the
    // box never leaves the window or covers the sample it describes.
    let screen = ui.ctx().content_rect();
    const OFFSET: f32 = 16.0;
    let mut pos = Pos2::new(at.x + OFFSET, at.y + OFFSET);
    if pos.x + width > screen.right() - 4.0 {
        pos.x = at.x - OFFSET - width;
    }
    if pos.y + height > screen.bottom() - 4.0 {
        pos.y = at.y - OFFSET - height;
    }
    pos.x = pos
        .x
        .clamp(screen.left(), (screen.right() - width).max(screen.left()));
    pos.y = pos
        .y
        .clamp(screen.top(), (screen.bottom() - height).max(screen.top()));
    let rect = snap_rect(Rect::from_min_size(pos, Vec2::new(width, height)), ppp);

    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("perf-chart-readout"),
    ));
    // A soft drop shadow so the box reads as floating above the chart it
    // overlaps; the panel colour alone is too close to the card fill.
    painter.rect_filled(
        rect.translate(Vec2::new(0.0, 1.5)).expand(1.5),
        6.0,
        Color32::from_black_alpha(40),
    );
    painter.rect_filled(rect, 5.0, pal.panel_bg);
    painter.rect_stroke(
        rect,
        5.0,
        Stroke::new(hairline(ppp), pal.stroke),
        egui::StrokeKind::Inside,
    );

    let mut y = rect.top() + PAD_Y;
    if let Some(g) = head_galley {
        painter.text(
            Pos2::new(rect.left() + PAD_X, y),
            Align2::LEFT_TOP,
            head,
            head_font,
            pal.text_dim,
        );
        y += g.size().y + 4.0;
    }
    for row in rows {
        let center_y = y + ROW_H * 0.5;
        let swatch = Rect::from_center_size(
            Pos2::new(rect.left() + PAD_X + SWATCH * 0.5, center_y),
            Vec2::splat(SWATCH),
        );
        painter.rect_filled(snap_rect(swatch, ppp), 1.5, row.color);
        if !row.label.is_empty() {
            painter.text(
                Pos2::new(rect.left() + PAD_X + SWATCH + SWATCH_GAP, center_y),
                Align2::LEFT_CENTER,
                &row.label,
                font.clone(),
                pal.text_dim,
            );
        }
        painter.text(
            Pos2::new(rect.right() - PAD_X, center_y),
            Align2::RIGHT_CENTER,
            &row.value,
            font.clone(),
            pal.text,
        );
        y += ROW_H;
    }
}

/// Vertical marker at the hovered sample plus a dot on each series.
///
/// The colour comes from the palette rather than a fixed white alpha: a
/// translucent white hairline is invisible on the light theme's card fill, and
/// barely visible on the dark one.
fn paint_marker(
    painter: &egui::Painter,
    pal: &crate::theme::Palette,
    rect: Rect,
    ppp: f32,
    x: f32,
    dots: &[(f32, Color32)],
) {
    let x = snap_line(x, ppp);
    painter.line_segment(
        [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
        Stroke::new(hairline(ppp).max(1.0), pal.text_dim),
    );
    for (y, color) in dots {
        painter.circle_filled(Pos2::new(x, *y), 2.6, *color);
    }
}

/// x of sample `i` on `axis`, falling back to `fallback` (even spacing) when
/// the chart has no axis, or the axis is short of that sample.
///
/// The offset is SIGNED. The plotted window carries one sample from just
/// before the axis opens (`performance::visible_slice`), and it only earns
/// its place if it lands LEFT of the rect: the segment from there to the
/// first in-window sample is what covers the left edge, at the slope the
/// data actually has. `saturating_sub` used to pin it onto the edge instead,
/// which is the same picture as having no lead-in at all — a flat entry, and
/// before that an unpainted wedge as wide as the first sample's offset. The
/// painter's clip rect trims the overhang; the low clamp only keeps a
/// pathological timestamp from tessellating a chart-wide mesh off-screen.
fn x_on_axis(
    rect: Rect,
    axis: Option<TimeAxis>,
    bounds: Option<(u64, u64)>,
    i: usize,
    fallback: f32,
) -> f32 {
    let Some(((t0, span), stamp)) = bounds.zip(axis.and_then(|a| a.stamps.get(i).copied())) else {
        return fallback;
    };
    // f64 throughout: these are epoch milliseconds (~1.8e12), where f32 cannot
    // represent whole minutes (see `nearest_sample`).
    let frac = (stamp as f64 - t0 as f64) / span as f64;
    rect.left() + rect.width() * frac.clamp(-1.0, 1.0) as f32
}

/// Index of the first sample that lies ON the axis.
///
/// The lead-in sample from before the window (see [`x_on_axis`]) is there to
/// give the polyline its entry slope, not to be pointed at: a readout locked
/// onto it would paint its marker outside the chart's clip rect, i.e. report
/// a value with no marker anywhere on the graph.
fn first_on_axis(axis: Option<TimeAxis>, bounds: Option<(u64, u64)>) -> usize {
    let (Some(a), Some((t0, _))) = (axis, bounds) else {
        return 0;
    };
    a.stamps.iter().position(|&t| t >= t0).unwrap_or(0)
}

/// Index of the sample closest to `frac` of the way through the window.
///
/// The arithmetic is deliberately u64/f64. Timestamps are milliseconds since
/// the UNIX epoch, around 1.8e12, where an f32 cannot even represent whole
/// minutes: `t0 as f32 + frac * span as f32` rounded the query time to a grid
/// roughly two minutes wide, so the readout reported a sample far from the
/// cursor — invisible while the tooltip sat in a corner, obvious the moment a
/// marker was drawn on the sample it had picked.
fn nearest_sample(ts: &[u64], t0: u64, span: u64, frac: f32) -> usize {
    nearest_at(
        ts,
        t0.saturating_add((f64::from(frac) * span as f64).round() as u64),
    )
}

/// Index of the sample closest to the instant `query`.
fn nearest_at(ts: &[u64], query: u64) -> usize {
    if ts.is_empty() {
        return 0;
    }
    let hi = ts.partition_point(|&t| t < query);
    if hi == 0 {
        return 0;
    }
    if hi >= ts.len() {
        return ts.len() - 1;
    }
    // Round to whichever neighbour the cursor is actually closer to, so the
    // marker lands on the sample under the pointer instead of the one before.
    let lo = hi - 1;
    if query - ts[lo] <= ts[hi] - query {
        lo
    } else {
        hi
    }
}

// -------------------------------------------------------------- pinned sample

/// The sample a chart's readout has locked onto, remembered between frames.
#[derive(Clone, Copy)]
struct Pin {
    /// Instant of the locked sample — NOT its index. Indices shift every time
    /// the window rolls; the instant is what the user actually pointed at.
    at_ms: u64,
    /// Where the pointer was when the lock was taken. Moving off it re-picks.
    pointer: Pos2,
}

/// How far the pointer may drift and still count as standing still. Small
/// enough that a deliberate nudge re-picks, large enough to absorb sub-pixel
/// jitter from the input stack.
const PIN_SLACK: f32 = 1.0;

fn pin_id(id: egui::Id) -> egui::Id {
    id.with("readout-pin")
}

/// Sample the readout should report, given the one currently `under_pointer`.
///
/// A rolling chart slides a fresh sample under a motionless cursor on every
/// tick, so a readout that always reports "whatever is here now" changes the
/// number before it can be read — the whole point of hovering a spike is to
/// find out what it was. While the pointer holds still the readout stays
/// locked to the instant it first named, and because the marker is drawn from
/// the returned INDEX it travels left with that sample instead of standing
/// still over changing data. The age line ("12 s ago") counts up as it goes,
/// which is what makes the lock legible rather than mysterious.
///
/// The lock is released when the pointer moves, when the pinned sample rolls
/// out of the window, and when the chart has no timestamps to pin against.
fn pinned_sample(
    ctx: &egui::Context,
    id: egui::Id,
    axis: Option<TimeAxis>,
    bounds: Option<(u64, u64)>,
    pointer: Pos2,
    under_pointer: usize,
) -> usize {
    // A chart whose timestamp slice is short of its samples is already
    // degrading to even spacing (see `chart_multi`); leave it on that path
    // rather than pinning it to an instant that does not describe it.
    let Some((ts, t0)) = axis
        .map(|a| a.stamps)
        .filter(|ts| under_pointer < ts.len())
        .zip(bounds.map(|(t0, _)| t0))
    else {
        return under_pointer;
    };
    let id = pin_id(id);
    // Released against the axis rather than against `ts[0]`: the oldest
    // plotted sample is the lead-in from BEFORE the axis, so holding the lock
    // until it rolls past that would keep the marker one sample outside the
    // chart for a tick.
    let held = ctx
        .data(|d| d.get_temp::<Pin>(id))
        .filter(|pin| pin.pointer.distance(pointer) <= PIN_SLACK && pin.at_ms >= t0);
    match held {
        Some(pin) => nearest_at(ts, pin.at_ms),
        None => {
            let pin = Pin {
                at_ms: ts[under_pointer],
                pointer,
            };
            ctx.data_mut(|d| d.insert_temp(id, pin));
            under_pointer
        }
    }
}

/// Pointer position when this chart is genuinely hovered.
///
/// `Response::hovered` rather than a bare rect test: an open context menu or
/// a dialog above the chart must suppress the readout, and a rect test alone
/// would keep painting it through them.
/// Doubles as the release point for [`pinned_sample`]'s lock: the pin lives
/// exactly as long as the pointer is on the chart.
fn hover_pos(ui: &egui::Ui, response: &Response, rect: Rect) -> Option<Pos2> {
    let pos = response
        .hovered()
        .then(|| ui.input(|i| i.pointer.hover_pos()))
        .flatten()
        .filter(|pos| rect.contains(*pos));
    if pos.is_none() {
        ui.ctx().data_mut(|d| d.remove::<Pin>(pin_id(response.id)));
    }
    pos
}

/// Age caption for sample `idx` of `timestamps_ms`, relative to the newest
/// sample in the window. Empty when the caller charted without timestamps.
fn sample_age(axis: Option<TimeAxis>, idx: usize) -> String {
    let Some(ts) = axis.map(|a| a.stamps) else {
        return String::new();
    };
    let (Some(&at), Some(&last)) = (ts.get(idx), ts.last()) else {
        return String::new();
    };
    age_label(last.saturating_sub(at))
}

/// Scale used by Performance-card sparklines.
///
/// Percentage resources (CPU/memory/disk/GPU) naturally stay <= 100. Network
/// cards feed this widget byte rates, so treating every value as a percentage
/// used to clamp virtually all Ethernet traffic to 100 and paint the preview
/// completely blue. Values above 100 therefore opt into a dynamic raw-value
/// scale with a small amount of headroom.
fn sparkline_y_max(samples: &[f64]) -> f64 {
    let max = samples
        .iter()
        .copied()
        .filter(|v| v.is_finite() && *v > 0.0)
        .fold(0.0f64, f64::max);
    if max > 100.0 {
        (max * 1.05).max(1.0)
    } else {
        100.0
    }
}

/// Sparkline painted into an explicit rect (no allocation) — used inside
/// hand-laid cards.
///
/// `cell_bg` is the cell's own surface. Callers that paint a background behind
/// the whole card when it is selected or hovered must pass a DARKER colour
/// then (`Palette::card_bg_sunken`); handing back `card_bg` would make the
/// cell the same colour as the card and dissolve the graph into the row at
/// exactly the moment the pointer is on it.
pub fn paint_sparkline(
    ui: &egui::Ui,
    rect: egui::Rect,
    samples: &[f64],
    color: Color32,
    cell_bg: Color32,
) {
    let ppp = ui.ctx().pixels_per_point();
    let rect = snap_rect(rect, ppp);
    let painter = ui.painter_at(rect);
    let pal = crate::theme::palette(ui);

    painter.rect_filled(rect, 2.0, cell_bg);
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(hairline(ppp), pal.stroke),
        egui::StrokeKind::Inside,
    );

    if samples.len() < 2 {
        return;
    }

    let n = samples.len();
    let y_max = sparkline_y_max(samples);
    let x = |i: usize| rect.left() + rect.width() * i as f32 / (n - 1) as f32;
    let y = |v: f64| {
        let v = if v.is_finite() { v.max(0.0) } else { 0.0 };
        rect.bottom() - (v.min(y_max) / y_max) as f32 * rect.height()
    };

    let pts: Vec<Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| Pos2::new(x(i), y(*v)))
        .collect();

    fill_area_to_baseline(&painter, &pts, rect.bottom(), area_fill(color));
    painter.add(Shape::line(pts, Stroke::new(1.25, color)));
}

/// Per-logical-processor tile: bordered, faint horizontal grid, filled area.
/// `kernels` carries the kernel-time share (same length) together with the
/// colour to overlay it in — Task Manager's "Show kernel times" (§14.4). The
/// colour travels WITH the samples because it is only meaningful when they
/// are there, and because a tile and the aggregate CPU chart have to paint
/// kernel time in the same colour.
///
/// `label` names the tile in the hover readout ("CPU 0"); `timestamps_ms`
/// gives that readout the sample's age, exactly as on the big charts.
pub fn core_chart(
    ui: &mut egui::Ui,
    size: Vec2,
    samples: &[f64],
    kernels: Option<(&[f64], Color32)>,
    color: Color32,
    label: &str,
    axis: Option<TimeAxis>,
) -> Response {
    let (alloc, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let pal = crate::theme::palette(ui);
    let ppp = ui.ctx().pixels_per_point();
    let rect = snap_rect(alloc, ppp);
    let painter = ui.painter_at(rect).with_clip_rect(rect);

    paint_frame(&painter, rect, ppp, &pal, 4, 4);

    if samples.len() < 2 {
        return response;
    }
    let n = samples.len();
    // Window-anchored x, exactly as on the big charts (see [`TimeAxis`]): a
    // tile that spread whatever it had across its full width was compressing
    // its own history rather than scrolling it.
    let bounds = axis.and_then(|a| a.bounds());
    let even = |i: usize| rect.left() + rect.width() * i as f32 / (n - 1) as f32;
    let x = |i: usize| x_on_axis(rect, axis, bounds, i, even(i));
    let y = |v: f64| rect.bottom() - (v.clamp(0.0, 100.0) / 100.0) as f32 * rect.height();
    let pts: Vec<Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| Pos2::new(x(i), y(*v)))
        .collect();
    fill_area_to_baseline(&painter, &pts, rect.bottom(), area_fill(color));

    // Kernel overlay: darker band under the user portion. Painted AFTER the
    // user fill but BEFORE the line, so it darkens the lower region without
    // burying the series line (the old order drew the line first).
    if let Some((k, kernel_color)) = kernels {
        let kpts: Vec<Pos2> = k
            .iter()
            .enumerate()
            .map(|(i, v)| Pos2::new(x(i), y(*v)))
            .collect();
        fill_area_to_baseline(&painter, &kpts, rect.bottom(), kernel_fill(kernel_color));
    }
    painter.add(Shape::line(pts, Stroke::new(1.4, color)));

    if let Some(pos) = hover_pos(ui, &response, rect) {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let first = first_on_axis(axis, bounds);
        let idx = match (bounds, axis) {
            (Some((t0, span)), Some(a)) => nearest_sample(a.stamps, t0, span, frac),
            _ => ((frac * (n - 1) as f32).round() as usize).min(n - 1),
        }
        .max(first);
        let idx = pinned_sample(ui.ctx(), response.id, axis, bounds, pos, idx)
            .max(first)
            .min(n - 1);
        let mut rows = vec![ReadoutRow {
            color,
            label: label.to_owned(),
            value: fmt_percent(samples[idx]),
        }];
        let mut dots = vec![(y(samples[idx]), color)];
        if let Some((kernel_color, k)) = kernels.and_then(|(k, c)| Some((c, *k.get(idx)?))) {
            rows.push(ReadoutRow {
                color: kernel_color,
                label: i18n::tr(K::ShowKernelTimesShort).to_owned(),
                value: fmt_percent(k),
            });
            dots.push((y(k), kernel_color));
        }
        paint_marker(&painter, &pal, rect, ppp, x(idx), &dots);
        readout(ui, pos, &sample_age(axis, idx), &rows);
    }
    response
}

/// One owned series for [`chart_multi`].
pub struct MultiSeries {
    pub samples: Vec<f64>,
    pub color: Color32,
    /// Name shown in the hover readout. Empty for a chart whose single series
    /// needs no naming beyond its caption.
    pub label: String,
}

impl MultiSeries {
    pub fn new(label: impl Into<String>, samples: Vec<f64>, color: Color32) -> Self {
        Self {
            samples,
            color,
            label: label.into(),
        }
    }
}

/// Bordered chart with several filled series sharing one y scale
/// (disk read+write, memory+commit, ...). When `timestamps_ms` is given the
/// x positions follow real sample times, so delayed/irregular samples plot
/// at their true elapsed offset instead of compressing evenly (§14.3).
///
/// `fmt` renders a sample for the hover readout, so each page states its own
/// unit (percent, gigabytes, bytes per second) instead of a bare number.
pub fn chart_multi(
    ui: &mut egui::Ui,
    size: Vec2,
    series: &[MultiSeries],
    y_max: f64,
    axis: Option<TimeAxis>,
    fmt: ValueFmt,
) -> Response {
    let (alloc, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let pal = crate::theme::palette(ui);
    let ppp = ui.ctx().pixels_per_point();
    let rect = snap_rect(alloc, ppp);
    let painter = ui.painter_at(rect).with_clip_rect(rect);

    paint_frame(&painter, rect, ppp, &pal, 6, 4);

    let y = |v: f64| rect.bottom() - (v.clamp(0.0, y_max) / y_max) as f32 * rect.height();
    // Time-proportional x over the CONFIGURED window — see [`TimeAxis`].
    let bounds = axis.and_then(|a| a.bounds());
    // Callers must hand over one timestamp per sample (every series extractor
    // maps over the same window). Indexing blind would turn a future
    // desynchronization — a bug class this code has already seen once, see
    // `series_extractors_stay_aligned_with_window` — into a panic in the paint
    // path, i.e. a crashed task manager. A short timestamp slice degrades to
    // even spacing for the samples it cannot place instead.
    let even = |i: usize, n: usize| rect.left() + rect.width() * i as f32 / (n - 1) as f32;
    let x_at = |i: usize, n: usize| x_on_axis(rect, axis, bounds, i, even(i, n));
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
        fill_area_to_baseline(&painter, &pts, rect.bottom(), area_fill(s.color));
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
        painter.add(Shape::line(pts, Stroke::new(1.25, s.color)));
    }

    // Hover: every series' value at the pointer, in a box AT the pointer.
    if let Some(pos) = hover_pos(ui, &response, rect) {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        // Time-proportional x: map the pointer TIME back to the nearest
        // sample — even-spacing math skips across sampling gaps.
        let first = first_on_axis(axis, bounds);
        let time_idx = bounds
            .zip(axis)
            .map(|((t0, span), a)| nearest_sample(a.stamps, t0, span, frac).max(first))
            .map(|under| pinned_sample(ui.ctx(), response.id, axis, bounds, pos, under).max(first));
        let mut rows = Vec::with_capacity(series.len());
        let mut dots = Vec::with_capacity(series.len());
        let mut marker = None;
        for s in series {
            let n = s.samples.len();
            if n < 2 {
                continue;
            }
            let idx = time_idx
                .map_or_else(
                    || (frac * (n - 1) as f32).round() as usize,
                    |i| i.min(n - 1),
                )
                .min(n - 1);
            if let Some(v) = s.samples.get(idx) {
                rows.push(ReadoutRow {
                    color: s.color,
                    label: s.label.clone(),
                    value: fmt(*v),
                });
                dots.push((y(*v), s.color));
                marker.get_or_insert((x_at(idx, n), idx));
            }
        }
        if let Some((x, idx)) = marker {
            paint_marker(&painter, &pal, rect, ppp, x, &dots);
            readout(ui, pos, &sample_age(axis, idx), &rows);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a graph must reach the left edge of its own axis.
    ///
    /// The axis opens at `newest - window`, but samples land where the
    /// sampler ticked, so the oldest sample inside the window is short of
    /// that edge whenever the spacing does not divide the window. The window
    /// therefore carries the sample just BEFORE it, and that sample has to
    /// plot off the left edge for the segment entering the chart to cover it
    /// — it used to saturate onto the edge, leaving the wedge unpainted.
    #[test]
    fn the_lead_in_sample_plots_left_of_the_chart() {
        let rect = Rect::from_min_size(Pos2::new(100.0, 0.0), Vec2::new(600.0, 100.0));
        // 1.3 s spacing, 10 s window: newest at 10 400 ms, axis opens at 400.
        let stamps: Vec<u64> = (0..=8).map(|i| i * 1_300).collect();
        let axis = TimeAxis::new(&stamps, 10);
        let bounds = axis.bounds();
        let at = |i: usize| x_on_axis(rect, Some(axis), bounds, i, f32::NAN);

        assert!(at(0) < rect.left(), "lead-in pinned to the edge: {}", at(0));
        // The lead-in stays ONE sample out, so the entering segment is short
        // and steep rather than a flat run-up from far off-screen.
        assert!(at(0) > rect.left() - rect.width() * 0.2);
        // Everything else keeps its true position, right edge included.
        assert!(at(1) > rect.left());
        assert!((at(stamps.len() - 1) - rect.right()).abs() < 0.5);
        // The hover readout must not lock onto the off-axis lead-in.
        assert_eq!(first_on_axis(Some(axis), bounds), 1);
    }

    /// A series longer than the shared timestamp slice must still paint.
    ///
    /// Every Performance extractor maps over the same window, so today the two
    /// always match — but they desynchronized once before
    /// (`performance::tests::series_extractors_stay_aligned_with_window`), and
    /// back then this code indexed the timestamps by series index. In the
    /// paint path that is not a wrong x position, it is a panic: the task
    /// manager disappears while the user is looking at it.
    #[test]
    fn a_series_longer_than_its_timestamps_degrades_instead_of_panicking() {
        let ctx = egui::Context::default();
        let stamps: Vec<u64> = (0..4).map(|i| 1_000 + i * 250).collect();
        let paint = |samples: Vec<f64>| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        Pos2::ZERO,
                        egui::vec2(400.0, 200.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    chart_multi(
                        ui,
                        Vec2::new(320.0, 120.0),
                        &[MultiSeries::new("CPU", samples.clone(), Color32::BLUE)],
                        100.0,
                        Some(TimeAxis::new(&stamps, 1)),
                        fmt_percent,
                    );
                },
            );
            // Nothing paints this frame to a surface, so the font-atlas delta
            // has to be dropped explicitly.
            output.textures_delta.clear();
            output.shapes.len()
        };
        // Ten samples against four timestamps, then the reverse. Returning at
        // all — rather than unwinding out of the paint path — is the assertion.
        assert!(paint((0..10).map(|i| f64::from(i) * 7.0).collect()) > 0);
        assert!(paint(vec![1.0, 2.0]) > 0);
    }

    /// Composite a translucent (premultiplied) colour over an opaque one, the
    /// way the renderer does.
    fn composite(fg: Color32, bg: Color32) -> Color32 {
        let inv = 255 - fg.a() as u32;
        let ch = |f: u8, b: u8| (f as u32 + b as u32 * inv / 255).min(255) as u8;
        Color32::from_rgb(ch(fg.r(), bg.r()), ch(fg.g(), bg.g()), ch(fg.b(), bg.b()))
    }

    fn luma(c: Color32) -> i32 {
        (2 * c.r() as i32 + 5 * c.g() as i32 + c.b() as i32) / 8
    }

    /// A series' area fill must stay DARKER than the line that bounds it, so
    /// the line reads as an outline against its own infill.
    ///
    /// The fills were built with `from_rgba_premultiplied` and full-strength
    /// components, which blends additively: over the dark card fill a CPU
    /// series composited BRIGHTER than its own line, every graph looked like a
    /// solid slab, and the outline was invisible.
    #[test]
    fn an_area_fill_stays_darker_than_the_line_that_bounds_it() {
        for pal in [crate::theme::DARK, crate::theme::LIGHT] {
            for color in [
                pal.cpu_graph,
                pal.memory_graph,
                pal.disk_graph,
                pal.network_graph,
                pal.gpu_graph,
            ] {
                for bg in [pal.card_bg, pal.card_bg_sunken] {
                    let filled = composite(area_fill(color), bg);
                    let contrast = (luma(filled) - luma(color)).abs();
                    assert!(
                        contrast >= 24,
                        "{color:?} on {bg:?}: fill {filled:?} is only {contrast} from the line"
                    );
                    // ...and darker specifically: the fill must move TOWARDS
                    // the background, never past the line away from it.
                    let toward_bg =
                        (luma(filled) - luma(bg)).abs() < (luma(color) - luma(bg)).abs();
                    assert!(
                        toward_bg,
                        "{color:?} on {bg:?}: fill {filled:?} overshot the line"
                    );
                }
            }
        }
    }

    /// The kernel band keeps the caller's kernel colour and stays short of
    /// fully opaque, so the tile's gridlines still show through it.
    #[test]
    fn the_kernel_band_paints_the_colour_it_was_given() {
        let pal = crate::theme::DARK;
        let band = kernel_fill(pal.cpu_kernel_graph);
        assert!(band.a() < 255, "the grid must still show through");
        // What lands on screen must still read as the kernel colour rather
        // than as another tone of the user-time series it sits inside.
        let opaque = composite(band, pal.card_bg);
        let d = |a: Color32, b: Color32| {
            let c = |x: u8, y: u8| (x as i32 - y as i32).abs();
            (2 * c(a.r(), b.r()) + 5 * c(a.g(), b.g()) + c(a.b(), b.b())) / 8
        };
        assert!(
            d(opaque, pal.cpu_kernel_graph) * 3 < d(opaque, pal.cpu_graph),
            "the band drifted off its own colour: {opaque:?}"
        );
    }

    /// A sparkline paints the cell background it was handed, so a card that
    /// lifts onto `card_bg` when selected or hovered can sink its graph cell
    /// instead of letting it dissolve into the row.
    #[test]
    fn a_sparkline_paints_the_cell_background_it_was_given() {
        let pal = crate::theme::DARK;
        assert_ne!(pal.card_bg_sunken, pal.card_bg);
        assert!(luma(pal.card_bg_sunken) < luma(pal.card_bg), "dark theme");
        assert!(
            luma(crate::theme::LIGHT.card_bg_sunken) < luma(crate::theme::LIGHT.card_bg),
            "light theme"
        );

        let paint = |cell_bg: Color32| {
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(200.0, 100.0))),
                    ..Default::default()
                },
                |ui| {
                    paint_sparkline(
                        ui,
                        Rect::from_min_size(Pos2::new(4.0, 4.0), Vec2::new(62.0, 40.0)),
                        &[10.0, 40.0, 20.0],
                        pal.cpu_graph,
                        cell_bg,
                    );
                },
            );
            output.textures_delta.clear();
            output
                .shapes
                .iter()
                .find_map(|s| match &s.shape {
                    Shape::Rect(r) => Some(r.fill),
                    _ => None,
                })
                .expect("sparkline paints a cell")
        };
        assert_eq!(paint(pal.card_bg), pal.card_bg);
        assert_eq!(paint(pal.card_bg_sunken), pal.card_bg_sunken);
    }

    /// [`pinned_sample`] against a window whose axis opens exactly on its
    /// oldest stamp, which is what these tests are written around.
    fn pin(ctx: &egui::Context, id: egui::Id, ts: &[u64], at: Pos2, under: usize) -> usize {
        let span_ms = match (ts.first(), ts.last()) {
            (Some(&first), Some(&last)) => last.saturating_sub(first),
            _ => 0,
        };
        let axis = TimeAxis::new(ts, ((span_ms / 1_000) as u32).max(1));
        pinned_sample(ctx, id, Some(axis), axis.bounds(), at, under)
    }

    /// A motionless pointer must keep naming the sample it first pointed at,
    /// even as the window rolls that sample towards the left edge.
    ///
    /// Without the pin the readout reports whatever has just scrolled under
    /// the cursor, so the number changes on every tick and the spike the user
    /// is trying to read is gone before they have read it.
    #[test]
    fn a_still_pointer_keeps_naming_the_sample_it_locked_onto() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("rolling-chart");
        let at = Pos2::new(120.0, 40.0);
        let window = |t0: u64| -> Vec<u64> { (0..10).map(|i| t0 + i * 1_000).collect() };

        // Pointing at sample 7 of the first window locks onto t = 8000.
        let first = window(1_000);
        assert_eq!(pin(&ctx, id, &first, at, 7), 7);

        // Two ticks later the same screen position sits over sample 9, but
        // t = 8000 has rolled back to index 5 — and that is what the readout
        // must keep reporting.
        let rolled = window(3_000);
        assert_eq!(pin(&ctx, id, &rolled, at, 9), 5);

        // Moving the pointer re-picks, and locks onto the new sample.
        let moved = Pos2::new(160.0, 40.0);
        assert_eq!(pin(&ctx, id, &rolled, moved, 9), 9);
        assert_eq!(pin(&ctx, id, &window(5_000), moved, 9), 7);

        // Sub-pixel jitter is not "moving".
        let jitter = Pos2::new(moved.x + 0.4, moved.y);
        assert_eq!(pin(&ctx, id, &window(5_000), jitter, 9), 7);
    }

    /// A pin whose sample has rolled off the left edge is released, and a
    /// chart with no timestamps never takes one.
    #[test]
    fn a_pin_is_released_once_its_sample_leaves_the_window() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("rolling-chart");
        let at = Pos2::new(120.0, 40.0);
        let window = |t0: u64| -> Vec<u64> { (0..10).map(|i| t0 + i * 1_000).collect() };

        let first = window(1_000);
        assert_eq!(pin(&ctx, id, &first, at, 2), 2); // t = 3000
        // The window has moved past t = 3000 entirely: fall back to the
        // sample under the pointer instead of clamping to the oldest one.
        assert_eq!(pin(&ctx, id, &window(20_000), at, 6), 6);

        assert_eq!(
            pinned_sample(&ctx, egui::Id::new("no-ts"), None, None, at, 4),
            4
        );
        // Fewer timestamps than samples keeps the even-spacing fallback.
        assert_eq!(pin(&ctx, egui::Id::new("short"), &[1, 2], at, 7), 7);
    }

    /// Leaving the chart drops the lock, so coming back re-reads the graph
    /// rather than resurrecting a sample from minutes ago.
    #[test]
    fn leaving_the_chart_drops_the_lock() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("rolling-chart");
        let at = Pos2::new(120.0, 40.0);
        let ts: Vec<u64> = (0..10).map(|i| 1_000 + i * 1_000).collect();
        assert_eq!(pin(&ctx, id, &ts, at, 7), 7);
        assert!(ctx.data(|d| d.get_temp::<Pin>(pin_id(id))).is_some());

        ctx.data_mut(|d| d.remove::<Pin>(pin_id(id)));
        assert!(ctx.data(|d| d.get_temp::<Pin>(pin_id(id))).is_none());
    }

    /// x positions of the painted polyline, in order.
    fn painted_xs(width: f32, stamps: &[u64], window_s: u32) -> Vec<f32> {
        let ctx = egui::Context::default();
        let samples: Vec<f64> = stamps.iter().map(|t| (t % 97) as f64).collect();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    egui::vec2(width + 40.0, 200.0),
                )),
                ..Default::default()
            },
            |ui| {
                chart_multi(
                    ui,
                    Vec2::new(width, 120.0),
                    &[MultiSeries::new("s", samples.clone(), Color32::BLUE)],
                    100.0,
                    Some(TimeAxis::new(stamps, window_s)),
                    fmt_percent,
                );
            },
        );
        output.textures_delta.clear();
        output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                Shape::Path(path) => Some(path.points.iter().map(|p| p.x).collect()),
                _ => None,
            })
            .expect("the series paints a polyline")
    }

    /// Every sample must move left at the same rate as the window rolls, and
    /// a window that is not full yet must leave its unfilled part EMPTY.
    ///
    /// The axis used to be derived from the samples (`last - first`), which
    /// pins the oldest sample to the left edge no matter how little history
    /// there is: a half-filled window drew 30 s of data across a caption that
    /// said 60, and each new sample squeezed the older ones a little further
    /// left instead of scrolling them. On screen that is a graph whose left
    /// side sits still while only the right edge moves.
    #[test]
    fn the_axis_spans_the_configured_window_not_the_samples_it_has() {
        const W: f32 = 600.0;
        let t0 = 1_797_000_000_000u64;
        // Ten seconds of a sixty-second window: the curve occupies the last
        // sixth of the chart and nothing is drawn over the rest.
        let filling: Vec<u64> = (0..=10).map(|i| t0 + i * 1_000).collect();
        let xs = painted_xs(W, &filling, 60);
        assert!(
            xs[0] > W * 0.8,
            "a tenth of the window was stretched across {}% of the chart",
            (100.0 * (W - xs[0]) / W) as i32
        );
        assert!(
            (xs[xs.len() - 1] - W).abs() < 0.5,
            "newest sample is not at the right edge"
        );

        // One tick later every sample has moved left by one second's worth of
        // width — including the oldest one.
        let rolled: Vec<u64> = (0..=11).map(|i| t0 + i * 1_000).collect();
        let next = painted_xs(W, &rolled, 60);
        let step = W / 60.0;
        for (before, after) in xs.iter().zip(next.iter()) {
            assert!(
                (before - after - step).abs() < 0.5,
                "sample moved {} px, expected {step}",
                before - after
            );
        }

        // A full window still fills the chart edge to edge.
        let full: Vec<u64> = (0..=60).map(|i| t0 + i * 1_000).collect();
        let xs = painted_xs(W, &full, 60);
        assert!(xs[0].abs() < 0.5, "oldest sample is not at the left edge");
        assert!((xs[xs.len() - 1] - W).abs() < 0.5);
    }

    #[test]
    fn sparkline_percentage_and_raw_rate_scales_are_distinct() {
        assert_eq!(sparkline_y_max(&[0.0, 25.0, 83.0]), 100.0);
        assert!(sparkline_y_max(&[1_000.0, 2_000.0, 4_000.0]) > 4_000.0);
        assert_eq!(sparkline_y_max(&[f64::NAN, -1.0]), 100.0);
    }

    /// The area fill must be a triangle STRIP along the polyline: every
    /// triangle spans at most one segment's x-range. The old fan
    /// triangulation (`Shape::convex_polygon`) created long edges from the
    /// first vertex to distant peaks, cutting straight across concave dips —
    /// the "fake ramp" rendering artifact.
    #[test]
    fn area_strip_never_spans_more_than_one_segment() {
        // Concave profile: peak in the middle, dips at both ends.
        let pts = vec![
            Pos2::new(0.0, 0.0),
            Pos2::new(5.0, 10.0),
            Pos2::new(10.0, 0.0),
        ];
        let mesh = area_strip_mesh(&pts, 12.0, Color32::BLUE);
        assert_eq!(mesh.vertices.len(), 2 * pts.len());
        assert_eq!(mesh.indices.len(), 6 * (pts.len() - 1));
        let dx = 5.0; // segment width
        for tri in mesh.indices.as_chunks::<3>().0 {
            let xs: Vec<f32> = tri
                .iter()
                .map(|&i| mesh.vertices[i as usize].pos.x)
                .collect();
            let span = xs.iter().cloned().fold(f32::MIN, f32::max)
                - xs.iter().cloned().fold(f32::MAX, f32::min);
            assert!(
                span <= dx + f32::EPSILON,
                "triangle spans {span} px — wider than one segment (fan edge)"
            );
        }
    }

    /// Degenerate inputs produce an empty mesh instead of panicking.
    #[test]
    fn area_strip_handles_degenerate_input() {
        assert!(area_strip_mesh(&[], 0.0, Color32::BLUE).indices.is_empty());
        assert!(
            area_strip_mesh(&[Pos2::ZERO], 0.0, Color32::BLUE)
                .indices
                .is_empty()
        );
        assert!(
            area_strip_mesh(
                &[Pos2::ZERO, Pos2::new(1.0, 1.0)],
                0.0,
                Color32::TRANSPARENT
            )
            .indices
            .is_empty()
        );
    }

    /// Hairlines must land on a single physical pixel. A fractional cell rect
    /// (`(width - gaps) / cols`) is exactly what made the per-core grid look
    /// blurry.
    #[test]
    fn frames_and_hairlines_snap_to_the_pixel_grid() {
        let ppp = 1.5;
        let rect = Rect::from_min_size(Pos2::new(10.333, 20.777), Vec2::new(78.4, 49.1));
        let snapped = snap_rect(rect, ppp);
        for v in [
            snapped.left(),
            snapped.top(),
            snapped.right(),
            snapped.bottom(),
        ] {
            let px = v * ppp;
            assert!(
                (px - px.round()).abs() < 1e-4,
                "{v} is not on a pixel boundary at {ppp}x"
            );
        }
        // A hairline sits at a pixel CENTRE, i.e. half a pixel off a boundary.
        let line = snap_line(30.2, ppp) * ppp;
        assert!((line.fract() - 0.5).abs() < 1e-4, "{line} is not centred");
        // ...and is exactly one physical pixel wide.
        assert!((hairline(ppp) * ppp - 1.0).abs() < 1e-6);
        assert_eq!(hairline(0.0), 1.0);
    }

    /// Hovering a chart must paint the readout AT THE POINTER, on every chart
    /// shape — the complaint that started this was "only the CPU core tiles
    /// show a field, and where it appears is unreliable".
    #[test]
    fn hovering_paints_a_readout_next_to_the_cursor() {
        let screen = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 400.0));
        let at = Pos2::new(180.0, 60.0);
        let run = |pointer: Option<Pos2>, multi: bool| -> Vec<egui::Rect> {
            let ctx = egui::Context::default();
            let stamps: Vec<u64> = (0..40).map(|i| i * 1_000).collect();
            let samples: Vec<f64> = (0..40).map(|i| f64::from(i % 20) * 5.0).collect();
            let mut rects = Vec::new();
            // Two frames: egui resolves hovering against the previous frame's
            // interaction state, so a single frame would never report a hover.
            for _ in 0..2 {
                let mut input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                };
                if let Some(p) = pointer {
                    input.events.push(egui::Event::PointerMoved(p));
                }
                let mut output = ctx.run_ui(input, |ui| {
                    if multi {
                        chart_multi(
                            ui,
                            Vec2::new(400.0, 150.0),
                            &[MultiSeries::new("Receive", samples.clone(), Color32::BLUE)],
                            100.0,
                            Some(TimeAxis::new(&stamps, 60)),
                            fmt_percent,
                        );
                    } else {
                        core_chart(
                            ui,
                            Vec2::new(400.0, 150.0),
                            &samples,
                            None,
                            Color32::BLUE,
                            "CPU 0",
                            Some(TimeAxis::new(&stamps, 60)),
                        );
                    }
                });
                output.textures_delta.clear();
                rects = output
                    .shapes
                    .iter()
                    .map(|clipped| clipped.shape.visual_bounding_rect())
                    .filter(|r| r.is_finite() && r.is_positive())
                    .collect();
            }
            rects
        };
        for multi in [false, true] {
            let idle = run(None, multi);
            let hovered = run(Some(at), multi);
            assert!(
                hovered.len() > idle.len(),
                "hovering must add the readout (multi = {multi})"
            );
            // Something the idle frame never drew has to sit within a cursor's
            // reach of the pointer, i.e. outside the 400x150 chart is fine but
            // "pinned to a corner" is not.
            let near = hovered
                .iter()
                .filter(|r| !idle.iter().any(|i| *i == **r))
                .any(|r| r.distance_to_pos(at) < 40.0);
            assert!(near, "readout is not next to the cursor (multi = {multi})");
        }
    }

    /// The hovered sample must be the one under the cursor.
    ///
    /// Timestamps are epoch milliseconds (~1.8e12). The index used to be
    /// computed as `t0 as f32 + frac * span as f32`, and an f32 at that
    /// magnitude steps in ~131 s — so hovering the middle of a 60 s window
    /// reported whatever sample the rounding happened to land on.
    #[test]
    fn the_hovered_sample_is_the_one_under_the_cursor() {
        // One minute of one-second samples, stamped in epoch milliseconds.
        let t0 = 1_797_000_000_000u64;
        let ts: Vec<u64> = (0..=60).map(|i| t0 + i * 1_000).collect();
        let span = ts.last().unwrap() - t0;
        for (frac, expected) in [
            (0.0f32, 0usize),
            (0.25, 15),
            (0.42, 25),
            (0.5, 30),
            (0.75, 45),
            (1.0, 60),
        ] {
            assert_eq!(nearest_sample(&ts, t0, span, frac), expected, "frac {frac}");
        }
        // Irregular spacing still rounds to the closer neighbour.
        let gapped = [t0, t0 + 1_000, t0 + 40_000, t0 + 41_000];
        let span = 41_000;
        assert_eq!(nearest_sample(&gapped, t0, span, 0.0), 0);
        assert_eq!(nearest_sample(&gapped, t0, span, 0.05), 1);
        // Exactly halfway between two samples is a tie, resolved to the
        // earlier one; past the midpoint it snaps across the gap.
        assert_eq!(nearest_sample(&gapped, t0, span, 0.5), 1);
        assert_eq!(nearest_sample(&gapped, t0, span, 0.55), 2);
        assert_eq!(nearest_sample(&gapped, t0, span, 1.0), 3);
        assert_eq!(nearest_sample(&[], t0, span, 0.5), 0);
    }

    /// The readout head names the sample's age, so a reading taken mid-window
    /// cannot be mistaken for the current value.
    #[test]
    fn sample_age_reports_distance_from_the_newest_sample() {
        let ts: Vec<u64> = (0..=12).map(|i| i * 5_000).collect();
        let axis = TimeAxis::new(&ts, 60);
        assert_eq!(sample_age(Some(axis), 12), i18n::tr(K::GraphNow));
        assert!(sample_age(Some(axis), 10).contains("10"));
        // Beyond a minute it switches to m:ss.
        assert!(sample_age(Some(axis), 0).contains("1:00"));
        assert!(sample_age(None, 0).is_empty());
        // An out-of-range index is a caller slip, not a panic.
        assert!(sample_age(Some(axis), 99).is_empty());
    }
}

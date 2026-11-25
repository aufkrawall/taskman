//! Custom chart widgets: filled rolling line charts like Windows Task
//! Manager's Performance tab, plus a compact per-core grid variant.
//!
//! Three things every chart here shares:
//!
//! * **One gap-aware geometry builder.** Every sample reaches the screen
//!   through [`ChartGeometry`], never as a raw value: a missing or non-finite
//!   sample, a step back in time and a change of x source all END the run
//!   instead of becoming a segment. epaint cannot help here — it has no gap
//!   support, and a non-finite point is not even culled by the clip test — so
//!   a single NaN used to be drawn as a stroke of the series colour across a
//!   per-core tile, with no area fill anywhere under it.
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

/// Readout text for one sample: a measurement that is not finite is
/// unavailable ("—"), never a fabricated number.
///
/// That is the project's missing-telemetry invariant, and here it is also
/// concrete rendering: the same non-finite value would otherwise reach a shape
/// position (the marker dot) and be formatted by `num_fixed`, which maps a
/// non-finite to 0.0 — a gap reported as "0,0 %".
fn readout_text(value: Option<f64>, fmt: ValueFmt) -> String {
    value
        .filter(|value| value.is_finite())
        .map_or_else(|| "—".to_owned(), fmt)
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
    bg: Color32,
) {
    let hair = hairline(ppp);
    painter.rect_filled(rect, 4.0, bg);
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
    let head_font = crate::theme::font_small_in(ui, 11.0);
    let font = FontId::proportional(12.0);
    let layout = |text: &str, font: &FontId, color: Color32| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), font.clone(), color)
    };

    const PAD_X: f32 = 9.0;
    const PAD_Y: f32 = 7.0;
    const SWATCH: f32 = 8.0;
    const SWATCH_GAP: f32 = 7.0;
    const LABEL_GAP: f32 = 16.0;
    const ROW_H: f32 = 17.0;

    let head_galley = (!head.is_empty()).then(|| layout(head, &head_font, pal.text_dim));
    let row_galleys: Vec<_> = rows
        .iter()
        .map(|r| {
            let label = (!r.label.is_empty()).then(|| layout(&r.label, &font, pal.text_dim));
            let value = layout(&r.value, &font, pal.text);
            (r.color, label, value)
        })
        .collect();
    let label_w = row_galleys
        .iter()
        .filter_map(|(_, label, _)| label.as_ref().map(|g| g.size().x))
        .fold(0.0f32, f32::max);
    let value_w = row_galleys
        .iter()
        .map(|(_, _, value)| value.size().x)
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
        let g_h = g.size().y;
        painter.galley(Pos2::new(rect.left() + PAD_X, y), g, pal.text_dim);
        y += g_h + 4.0;
    }
    for (color, label_galley, value_galley) in row_galleys {
        let center_y = y + ROW_H * 0.5;
        let swatch = Rect::from_center_size(
            Pos2::new(rect.left() + PAD_X + SWATCH * 0.5, center_y),
            Vec2::splat(SWATCH),
        );
        painter.rect_filled(snap_rect(swatch, ppp), 1.5, color);
        if let Some(label_g) = label_galley {
            let label_pos = Align2::LEFT_CENTER
                .anchor_size(
                    Pos2::new(rect.left() + PAD_X + SWATCH + SWATCH_GAP, center_y),
                    label_g.size(),
                )
                .min;
            painter.galley(label_pos, label_g, pal.text_dim);
        }
        let value_pos = Align2::RIGHT_CENTER
            .anchor_size(
                Pos2::new(rect.right() - PAD_X, center_y),
                value_galley.size(),
            )
            .min;
        painter.galley(value_pos, value_galley, pal.text);
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

/// x of `stamp` on the axis `bounds`: the sample's SIGNED offset from the left
/// edge of the window.
///
/// The offset is signed because the plotted window carries one sample from
/// just before the axis opens (`performance::visible_slice`), and that sample
/// only earns its place if it lands LEFT of the rect: the segment from there to
/// the first in-window sample is what covers the left edge, at the slope the
/// data actually has. `saturating_sub` used to pin it onto the edge instead,
/// which is the same picture as having no lead-in at all — a flat entry, and
/// before that an unpainted wedge as wide as the first sample's offset. The
/// painter's clip rect trims the overhang; the clamp only keeps a pathological
/// timestamp from tessellating a mesh across the screen, so the overhang stays
/// within ONE chart width.
///
/// The clamp is a BOUND, not a coordinate: callers must never use a clamped
/// position to join two samples. A sample older than the previous one — a
/// wall-clock step back after NTP correction or resume, a duplicated or stale
/// stamp — starts a new run in [`ChartGeometry`] instead of being pinned onto
/// the edge, because pinning it is what drew a straight stroke from frac 0.447
/// (value >= 100 %) down to frac 0.952 (10 %) across a per-core tile, with no
/// area fill under it.
///
/// A sample further back than that bound is not drawn at all: see
/// [`placeable_on_axis`].
fn x_on_axis(rect: Rect, bounds: (u64, u64), stamp: u64) -> f32 {
    let (t0, span) = bounds;
    // f64 throughout: these are epoch milliseconds (~1.8e12), where f32 cannot
    // represent whole minutes (see `nearest_sample`).
    let frac = (stamp as f64 - t0 as f64) / span.max(1) as f64;
    rect.left() + rect.width() * frac.clamp(-1.0, 1.0) as f32
}

/// Whether the axis can honestly place `stamp`, i.e. whether it lies within one
/// window width of either end of the axis.
///
/// The plotted window carries exactly one sample from before the axis
/// (`performance::visible_slice`), and it sits there so the polyline can enter
/// at its true slope. That lead-in is normally one sampling interval old. It
/// does NOT stay one interval old across a suspend: history `[… 60 s, 360 s]`
/// with a 60 s window is an ordinary post-resume buffer, and its lead-in is
/// four windows old. Clamping that sample to `left - width` and joining it to
/// the first in-window sample draws a slope across four minutes nobody
/// measured — the "stroke that starts at its own left edge" from the report.
/// Such a sample is dropped and breaks the run instead, which is what
/// [`ChartGeometry::runs`] does with `None`.
///
/// The RIGHT bound is not symmetric decoration. The axis' right edge is the
/// newest ARRIVAL, not the largest stamp (see [`TimeAxis::bounds`]), so a
/// wall-clock step FORWARD that is later corrected leaves samples already in
/// the buffer stamped past the axis end. Clamping those onto the edge draws
/// stale values where the newest measurement belongs — a dot beside the live
/// curve with nothing to explain it — and the run builder will happily JOIN one
/// of them to the in-window sample before it, which is the same "stroke across
/// time nobody measured" as the left-hand case. They are dropped instead.
fn placeable_on_axis(bounds: (u64, u64), stamp: u64) -> bool {
    let (t0, span) = bounds;
    let frac = (stamp as f64 - t0 as f64) / span.max(1) as f64;
    (-1.0..=1.0).contains(&frac)
}

/// x of series index `i` at even spacing across `rect`: the fallback for a
/// chart with no axis, and for the samples an axis cannot place.
///
/// A one-sample series has nothing to space evenly; it sits on the left edge,
/// which keeps the position finite where `i / (n - 1)` would be NaN.
fn even_x(rect: Rect, i: usize, n: usize) -> f32 {
    if n < 2 {
        return rect.left();
    }
    rect.left() + rect.width() * i as f32 / (n - 1) as f32
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

// ------------------------------------------------------------ chart geometry

/// Split a series into its measured runs: index/value pairs of consecutive
/// samples that actually have a value, so neither the line nor its fill bridges
/// missing data.
///
/// A sample is missing when it is `None` (the caller knows there is nothing) or
/// non-finite (the sampler reported NaN/inf, which is what `performance` hands
/// over for a tick it could not measure). Both have to break the run: epaint
/// has no gap support, so "no data" that reaches a path becomes a drawn
/// segment — exactly the shape of the garbage diagonal in the per-core tile
/// report. This is the value-only half of the split; [`ChartGeometry::runs`]
/// adds the geometry rules (one x source per run, time has to advance) and
/// builds the positions.
fn measured_runs(samples: &[Option<f64>]) -> Vec<Vec<(usize, f64)>> {
    let mut runs = Vec::new();
    let mut run = Vec::new();
    for (index, value) in samples.iter().enumerate() {
        if let Some(value) = value.filter(|value| value.is_finite()) {
            run.push((index, value));
        } else if !run.is_empty() {
            runs.push(std::mem::take(&mut run));
        }
    }
    if !run.is_empty() {
        runs.push(run);
    }
    runs
}

/// Where a sample's x comes from.
///
/// The two are DIFFERENT coordinate systems: time on the configured window, or
/// even spacing across the series. A run may only ever use one of them — a path
/// that swapped sources mid-run would draw a straight line through time that no
/// sample ever described.
#[derive(Clone, Copy)]
enum XSource {
    /// Position on the time axis, in epoch milliseconds.
    Axis(u64),
    /// Even spacing across the series: there is no timestamp to place it on.
    Even,
}

impl XSource {
    /// Whether a segment from `self` to `next` describes measured data.
    ///
    /// Consecutive points of one path ARE a segment, so every rule that has to
    /// survive into the rendered picture is enforced HERE, by ending the run:
    /// two coordinate systems cannot be joined, and time has to ADVANCE. A
    /// sample older than or as old as its predecessor — a wall-clock step back
    /// after NTP correction or resume, a duplicated or stale stamp — is never
    /// bridged.
    fn joins(self, next: Self) -> bool {
        match (self, next) {
            (Self::Axis(previous), Self::Axis(next)) => next > previous,
            (Self::Even, Self::Even) => true,
            _ => false,
        }
    }
}

/// The paths of one plotted series: one entry per measured run, in series
/// order. Naming the nesting keeps the call sites readable — the inner `Vec` is
/// a polyline, the outer level is "one per run".
type SeriesPaths = Vec<Vec<Pos2>>;

/// The single geometry builder every chart shape here paints through.
///
/// One place owns the mapping from a sample to a screen position, because
/// anything that reaches epaint as a `Pos2` gets drawn — and epaint cannot help:
/// `PathShape::line` keeps every point it is handed, `tessellate_path` has no
/// gap support, and `Rect::extend_with` skips non-finite points, so a poisoned
/// path is not even culled by the clip test. A NaN sample therefore did not
/// disappear; it rasterized as a stroke of the series colour across the chart.
/// That is the reported per-core tile artifact: a straight stroke from frac
/// 0.447 at a value of 100 % or more down to frac 0.952 at 10 %, uniform width,
/// no kinks, with no area fill anywhere under it and a normal curve beside it.
///
/// So the builder holds the invariants that make that impossible by
/// construction rather than by luck:
///
/// * Every emitted position is finite. A non-finite sample value is a gap (see
///   [`measured_runs`]), and `y` maps a verified-finite value through a
///   sanitised scale.
/// * A run never spans a gap, never spans a step back in time, and never mixes
///   two x sources — each of the three ends the run.
/// * x strictly increases along a run, which is also what keeps the lead-in
///   sample's overhang bounded to one chart width (see [`x_on_axis`]).
/// * A one-sample run is a dot, never a line to nowhere.
///
/// Every chart shape paints from this (the per-core tile, [`chart_multi`], the
/// card sparkline), so they cannot diverge again: the tile used to map its raw
/// samples straight into `Pos2` while the others were gap-aware.
#[derive(Clone, Copy)]
struct ChartGeometry<'a> {
    rect: Rect,
    /// Top of the y scale: 100 for a percentage chart, the series maximum for
    /// a raw-rate one. Sanitised in [`ChartGeometry::new`].
    y_max: f64,
    axis: Option<TimeAxis<'a>>,
    /// `(left edge instant, axis width)`, i.e. [`TimeAxis::bounds`].
    bounds: Option<(u64, u64)>,
    /// Sample count of the series. Even spacing has to describe the whole
    /// series even where a run only covers part of it.
    len: usize,
}

impl<'a> ChartGeometry<'a> {
    /// Geometry for one series of `len` samples painted into `rect`.
    ///
    /// `y_max` is sanitised here rather than at each call site: `f64::clamp`
    /// PANICS on inverted bounds, so a negative scale would take the whole
    /// Performance tab down, and a zero or NaN scale would put a non-finite y
    /// into every shape of the series.
    fn new(rect: Rect, y_max: f64, axis: Option<TimeAxis<'a>>, len: usize) -> Self {
        Self {
            rect,
            y_max: if y_max.is_finite() && y_max > 0.0 {
                y_max
            } else {
                1.0
            },
            bounds: axis.and_then(|axis| axis.bounds()),
            axis,
            len,
        }
    }

    /// The instant series index `i` was measured at, when the axis can place it
    /// there.
    ///
    /// `None` covers both "this chart has no timestamps" and "the timestamp
    /// slice is shorter than the series". Those stay one case on purpose: a
    /// sample past the end of the slice has no instant, so it must not borrow a
    /// coordinate from the time axis.
    fn stamp(&self, i: usize) -> Option<u64> {
        self.axis.and_then(|axis| axis.stamps.get(i).copied())
    }

    /// Where series index `i` is drawn, and in which coordinate system.
    ///
    /// `None` when the axis has a timestamp for this sample but cannot place it
    /// honestly (see [`placeable_on_axis`]): more than one window before the
    /// axis opens. Callers must treat that as a break, never as a position.
    fn place(&self, i: usize) -> Option<(XSource, f32)> {
        match (self.stamp(i), self.bounds) {
            (Some(stamp), Some(bounds)) if placeable_on_axis(bounds, stamp) => {
                Some((XSource::Axis(stamp), x_on_axis(self.rect, bounds, stamp)))
            }
            (Some(_), Some(_)) => None,
            _ => Some((XSource::Even, even_x(self.rect, i, self.len))),
        }
    }

    /// Screen x of series index `i` — for the hover marker, which marks a single
    /// sample rather than a run.
    ///
    /// Deliberately the clamped position rather than [`place`](Self::place): the
    /// readout never points at a sample outside the axis (`first_on_axis`), and a
    /// marker asking for an x must not have to answer "no".
    fn x(&self, i: usize) -> f32 {
        match (self.stamp(i), self.bounds) {
            (Some(stamp), Some(bounds)) => x_on_axis(self.rect, bounds, stamp),
            _ => even_x(self.rect, i, self.len),
        }
    }

    /// Screen y of a sample value that is known to be finite: 0 sits on the
    /// baseline, `y_max` on the top edge.
    ///
    /// The clamp is where the old path took a NaN and kept it — `f64::clamp`
    /// returns NaN for NaN — so callers hand over a value the run splitter has
    /// already proven finite. The assertion is local on purpose: the run
    /// splitter covers the series, but the hover marker and the kernel dots call
    /// this with a single value.
    fn y(&self, value: f64) -> f32 {
        debug_assert!(
            value.is_finite(),
            "non-finite chart value {value}: a NaN y is the garbage stroke this \
             geometry exists to prevent"
        );
        let frac = (value.clamp(0.0, self.y_max) / self.y_max) as f32;
        self.rect.bottom() - frac * self.rect.height()
    }

    /// The paths one series paints, in series order.
    ///
    /// Each inner slice becomes one polyline plus one area strip; between two of
    /// them the series has no measured data to join, and the painter must not
    /// draw across the distance.
    fn runs(&self, samples: &[Option<f64>]) -> SeriesPaths {
        let mut runs = SeriesPaths::new();
        for measured in measured_runs(samples) {
            let mut run: Vec<Pos2> = Vec::new();
            let mut previous: Option<(XSource, f32)> = None;
            for (index, value) in measured {
                let Some((source, x)) = self.place(index) else {
                    // The axis cannot place this sample: it is more than a
                    // window older than the axis opening, so any segment to the
                    // next sample would describe time nobody measured. Drop it
                    // and let the run break — the lead-in this exists for is
                    // one sampling interval old, not one suspend old.
                    if !run.is_empty() {
                        runs.push(std::mem::take(&mut run));
                    }
                    previous = None;
                    continue;
                };
                if let Some((prev_source, prev_x)) = previous
                    && !(prev_source.joins(source) && x > prev_x)
                {
                    // Whatever this check rejects becomes a fresh path rather
                    // than a bridging segment. The x comparison is what keeps
                    // the lead-in bounded as well: `x_on_axis` pins every stamp
                    // older than one window width to the same edge, and two
                    // points on that edge are not a segment worth drawing.
                    runs.push(std::mem::take(&mut run));
                }
                let point = Pos2::new(x, self.y(value));
                debug_assert!(
                    point.x.is_finite() && point.y.is_finite(),
                    "non-finite chart position {point:?} for sample {index}: epaint renders such a \
                     point as a stroke across the chart instead of a missing sample"
                );
                run.push(point);
                previous = Some((source, x));
            }
            if !run.is_empty() {
                runs.push(run);
            }
        }
        runs
    }

    /// [`runs`](Self::runs) for a raw `&[f64]` series, the form `performance`
    /// hands over: there a non-finite value already means "no measurement", and
    /// the run splitter treats it as the gap it is.
    fn runs_of(&self, samples: &[f64]) -> SeriesPaths {
        let measured: Vec<Option<f64>> = samples.iter().copied().map(Some).collect();
        self.runs(&measured)
    }
}

/// Kernel-time band geometry for a core tile.
///
/// The band is indexed against the value series it sits under: it is painted
/// only where that sample exists and where the kernel sample itself is a real
/// measurement, and only when both slices are the same length — otherwise it is
/// unavailable rather than shifted. The old path indexed the overlay by ITS OWN
/// slice index while taking x from the value series' axis stamps, so any length
/// difference slid the band sideways along the axis.
fn kernel_runs(values: &[f64], kernels: &[f64], geometry: &ChartGeometry<'_>) -> SeriesPaths {
    if kernels.len() != values.len() {
        return SeriesPaths::new();
    }
    let band: Vec<Option<f64>> = values
        .iter()
        .zip(kernels)
        .map(|(value, kernel)| value.is_finite().then_some(*kernel))
        .collect();
    geometry.runs(&band)
}

/// Area fill for every run of one series: the region between the polyline and
/// `baseline_y`.
///
/// A one-sample run has no area; it is skipped rather than handed to the
/// tessellator as a degenerate strip.
fn paint_runs_fill(painter: &egui::Painter, runs: &[Vec<Pos2>], baseline_y: f32, fill: Color32) {
    for run in runs.iter().filter(|run| run.len() >= 2) {
        fill_area_to_baseline(painter, run, baseline_y, fill);
    }
}

/// The polyline of every run of one series, at `width`.
///
/// A run with a single sample is painted as the dot it always was, never as a
/// line to nowhere: `Shape::line` through one point draws nothing at all, which
/// would silently drop the only measurement there is. Its radius follows the
/// stroke width so a tile's dot matches its own 1.4 px line instead of looking
/// like a bead glued onto it.
fn paint_runs_line(painter: &egui::Painter, runs: &[Vec<Pos2>], width: f32, color: Color32) {
    for run in runs {
        match run.as_slice() {
            [dot] => {
                painter.circle_filled(*dot, width.max(1.25), color);
            }
            points => {
                painter.add(Shape::line(points.to_vec(), Stroke::new(width, color)));
            }
        }
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
    } else {
        // The hover readout is anchored at the exact pointer position, so
        // cursor moves over the chart must keep repainting (throttled to
        // display refresh rate).
        egui_winit::set_pointer_stable_rect(ui.ctx(), Rect::NOTHING);
    }
    pos
}

/// Age caption for sample `idx` of the axis, relative to the newest sample in
/// the window. Empty when the caller charted without timestamps.
pub(crate) fn sample_age(axis: Option<TimeAxis>, idx: usize) -> String {
    let Some(ts) = axis.map(|a| a.stamps) else {
        return String::new();
    };
    let (Some(&at), Some(&last)) = (ts.get(idx), ts.last()) else {
        return String::new();
    };
    age_label(last.saturating_sub(at))
}

/// Keep a chart's keyboard cursor on a sample timestamp as the rolling
/// history moves. A live refresh only changes the readout after a key gesture.
pub(crate) fn keyboard_sample(
    ui: &mut egui::Ui,
    response: &Response,
    axis: Option<TimeAxis>,
    len: usize,
) -> Option<(usize, bool)> {
    if !response.has_focus() || !ui.is_enabled() || len == 0 {
        return None;
    }
    ui.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            response.id,
            egui::EventFilter {
                horizontal_arrows: true,
                vertical_arrows: true,
                ..Default::default()
            },
        );
    });
    let cursor_id = response.id.with("keyboard-sample");
    let old = ui.ctx().data(|data| data.get_temp::<u64>(cursor_id));
    let old_index = old.and_then(|at| {
        axis.and_then(|axis| axis.stamps.iter().position(|stamp| *stamp == at))
            .or_else(|| (at < len as u64).then_some(at as usize))
    });
    let mut index = old_index.unwrap_or(len - 1).min(len - 1);
    let key = ui.input(|input| {
        [
            egui::Key::ArrowLeft,
            egui::Key::ArrowRight,
            egui::Key::Home,
            egui::Key::End,
        ]
        .into_iter()
        .find(|key| input.key_pressed(*key) && !input.modifiers.any())
    });
    if let Some(key) = key {
        index = match key {
            egui::Key::ArrowLeft => index.saturating_sub(1),
            egui::Key::ArrowRight => (index + 1).min(len - 1),
            egui::Key::Home => 0,
            _ => len - 1,
        };
        ui.input_mut(|input| {
            input.consume_key(Default::default(), key);
        });
        ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
    }
    let at = axis
        .and_then(|axis| axis.stamps.get(index).copied())
        .unwrap_or(index as u64);
    ui.ctx().data_mut(|data| data.insert_temp(cursor_id, at));
    Some((index, key.is_some()))
}

pub(crate) fn chart_accessible_value(
    ui: &egui::Ui,
    response: &Response,
    value: String,
    changed: bool,
) {
    let id = response.id.with("spoken-sample");
    if changed || response.gained_focus() {
        ui.ctx().data_mut(|data| data.insert_temp(id, value));
    }
    if let Some(value) = ui.ctx().data(|data| data.get_temp::<String>(id)) {
        ui.ctx()
            .accesskit_node_builder(response.id, |node| node.set_value(value));
    }
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
#[cfg(test)]
pub fn paint_sparkline(
    ui: &egui::Ui,
    rect: egui::Rect,
    samples: &[f64],
    color: Color32,
    cell_bg: Color32,
) {
    let measured: Vec<_> = samples.iter().copied().map(Some).collect();
    paint_sparkline_with_gaps(ui, rect, &measured, color, cell_bg);
}

pub fn paint_sparkline_with_gaps(
    ui: &egui::Ui,
    rect: egui::Rect,
    samples: &[Option<f64>],
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
    let values: Vec<_> = samples.iter().filter_map(|value| *value).collect();
    let y_max = sparkline_y_max(&values);
    // The same builder as the big charts and the core tiles, with no axis (a
    // sparkline has no timestamps), so a missing or non-finite sample breaks
    // the line here exactly as it does there.
    let runs = ChartGeometry::new(rect, y_max, None, n).runs(samples);
    paint_runs_fill(&painter, &runs, rect.bottom(), area_fill(color));
    paint_runs_line(&painter, &runs, 1.25, color);
}

/// Per-logical-processor tile: bordered, faint horizontal grid, filled area.
/// `kernels` carries the kernel-time share (same length) together with the
/// colour to overlay it in — Task Manager's "Show kernel times" (§14.4). A band
/// whose slice is not exactly that long is dropped rather than shifted (see
/// [`kernel_runs`]): the overlay is indexed against the value series it sits
/// under, so a length difference would paint one sample's kernel time below
/// another sample's value. The colour travels WITH the samples because it is
/// only meaningful when they are there, and because a tile and the aggregate
/// CPU chart have to paint kernel time in the same colour.
///
/// `label` names the tile in the hover readout ("CPU 0"); `axis` gives that
/// readout the sample's age, exactly as on the big charts.
#[allow(clippy::too_many_arguments)]
fn core_chart_impl(
    ui: &mut egui::Ui,
    size: Vec2,
    samples: &[f64],
    kernels: Option<(&[f64], Color32)>,
    color: Color32,
    label: &str,
    axis: Option<TimeAxis>,
    grouped: bool,
    grouped_sample: Option<usize>,
) -> Response {
    let sense = if grouped {
        egui::Sense::CLICK
    } else {
        egui::Sense::click()
    };
    let (alloc, response) = ui.allocate_exact_size(size, sense);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, label));
    ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_role(egui::accesskit::Role::Image);
        node.set_label(label);
    });
    let pal = crate::theme::palette(ui);
    let ppp = ui.ctx().pixels_per_point();
    let rect = snap_rect(alloc, ppp);
    let painter = ui.painter_at(rect).with_clip_rect(rect);

    paint_frame(&painter, rect, ppp, &pal, 4, 4, pal.card_bg);

    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(1.0),
            4.0,
            Stroke::new(2.0, pal.accent),
            egui::StrokeKind::Inside,
        );
    }

    let keyboard = if grouped {
        grouped_sample.map(|index| (index, false))
    } else {
        keyboard_sample(ui, &response, axis, samples.len())
    };

    if samples.len() < 2 {
        return response;
    }
    let n = samples.len();
    // Window-anchored x, exactly as on the big charts (see [`TimeAxis`]): a
    // tile that spread whatever it had across its full width was compressing
    // its own history rather than scrolling it. These bounds are also what the
    // hover readout maps the pointer through.
    let bounds = axis.and_then(|a| a.bounds());
    // One geometry for the whole tile, from the same builder the big charts
    // use. The tile used to map its raw samples straight into `Pos2`, so every
    // gap and every NaN in the series became a point, and epaint drew them:
    // `f64::clamp` returns NaN for NaN, `PathShape::line` keeps every point it
    // is handed, and `Rect::extend_with` skips non-finite points, so the
    // poisoned path was not even culled by the clip test. That is the reported
    // tile artifact — a stroke of the series colour from frac 0.447 at a value
    // of 100 % or more down to frac 0.952 at 10 %, with no area fill under it.
    let geometry = ChartGeometry::new(rect, 100.0, axis, n);
    let runs = geometry.runs_of(samples);
    paint_runs_fill(&painter, &runs, rect.bottom(), area_fill(color));

    // Kernel overlay: darker band under the user portion. Painted AFTER the
    // user fill but BEFORE the line, so it darkens the lower region without
    // burying the series line (the old order drew the line first).
    if let Some((kernel_values, kernel_color)) = kernels {
        let band = kernel_runs(samples, kernel_values, &geometry);
        paint_runs_fill(&painter, &band, rect.bottom(), kernel_fill(kernel_color));
    }
    paint_runs_line(&painter, &runs, 1.4, color);

    let hover = if keyboard.is_some() {
        None
    } else {
        hover_pos(ui, &response, rect)
    };
    if let Some(pos) =
        hover.or_else(|| keyboard.map(|(idx, _)| Pos2::new(geometry.x(idx), rect.top() + 20.0)))
    {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let first = first_on_axis(axis, bounds);
        let idx = match (bounds, axis) {
            (Some((t0, span)), Some(a)) => nearest_sample(a.stamps, t0, span, frac),
            _ => ((frac * (n - 1) as f32).round() as usize).min(n - 1),
        }
        .max(first);
        let idx = keyboard
            .map_or_else(
                || pinned_sample(ui.ctx(), response.id, axis, bounds, pos, idx),
                |(index, _)| index,
            )
            .max(first)
            .min(n - 1);
        let mut rows = vec![ReadoutRow {
            color,
            label: label.to_owned(),
            value: readout_text(samples.get(idx).copied(), fmt_percent),
        }];
        // The marker dot is a plotted position like any other: a sample without
        // a measurement gets no dot, rather than one at a NaN y.
        let mut dots = Vec::new();
        if let Some(value) = samples.get(idx).copied().filter(|value| value.is_finite()) {
            dots.push((geometry.y(value), color));
        }
        // The band is painted only when its slice matches the value series (see
        // [`kernel_runs`]) and only where the value sample itself exists — the
        // dot has to obey the same rule, or a tile whose readout says "—" for
        // its usage would carry a kernel dot over a region with no band.
        if let Some((kernel_values, kernel_color)) = kernels
            && kernel_values.len() == n
            && samples.get(idx).copied().is_some_and(f64::is_finite)
            && let Some(kernel) = kernel_values
                .get(idx)
                .copied()
                .filter(|value| value.is_finite())
        {
            rows.push(ReadoutRow {
                color: kernel_color,
                label: i18n::tr(K::ShowKernelTimesShort).to_owned(),
                value: fmt_percent(kernel),
            });
            dots.push((geometry.y(kernel), kernel_color));
        }
        paint_marker(&painter, &pal, rect, ppp, geometry.x(idx), &dots);
        readout(ui, pos, &sample_age(axis, idx), &rows);
        if let Some((_, changed)) = keyboard.filter(|_| !grouped) {
            chart_accessible_value(
                ui,
                &response,
                format!(
                    "{}: {}",
                    sample_age(axis, idx),
                    readout_text(samples.get(idx).copied(), fmt_percent)
                ),
                changed,
            );
        }
    }
    response
}

#[allow(dead_code)] // used by render snapshots and chart regression tests
pub fn core_chart(
    ui: &mut egui::Ui,
    size: Vec2,
    samples: &[f64],
    kernels: Option<(&[f64], Color32)>,
    color: Color32,
    label: &str,
    axis: Option<TimeAxis>,
) -> Response {
    core_chart_impl(ui, size, samples, kernels, color, label, axis, false, None)
}

#[allow(clippy::too_many_arguments)]
pub fn core_chart_grouped(
    ui: &mut egui::Ui,
    size: Vec2,
    samples: &[f64],
    kernels: Option<(&[f64], Color32)>,
    color: Color32,
    label: &str,
    axis: Option<TimeAxis>,
    sample: Option<usize>,
) -> Response {
    core_chart_impl(ui, size, samples, kernels, color, label, axis, true, sample)
}

/// One owned series for [`chart_multi`].
pub struct MultiSeries {
    pub samples: Vec<Option<f64>>,
    pub color: Color32,
    /// Name shown in the hover readout. Empty for a chart whose single series
    /// needs no naming beyond its caption.
    pub label: String,
}

impl MultiSeries {
    pub fn new(label: impl Into<String>, samples: Vec<f64>, color: Color32) -> Self {
        Self::with_gaps(label, samples.into_iter().map(Some).collect(), color)
    }

    pub fn with_gaps(label: impl Into<String>, samples: Vec<Option<f64>>, color: Color32) -> Self {
        Self {
            samples,
            color,
            label: label.into(),
        }
    }
}

/// Bordered chart with several filled series sharing one y scale
/// (disk read+write, memory+commit, ...). When an `axis` is given the
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
    let pal = crate::theme::palette(ui);
    chart_multi_with_bg(ui, size, series, y_max, axis, fmt, pal.card_bg)
}

/// A multi-series rolling line chart with a caller-selected background surface
/// (such as `pal.card_bg_sunken` for charts embedded within modal dialogs whose
/// windows already use `card_bg`).
pub fn chart_multi_with_bg(
    ui: &mut egui::Ui,
    size: Vec2,
    series: &[MultiSeries],
    y_max: f64,
    axis: Option<TimeAxis>,
    fmt: ValueFmt,
    bg: Color32,
) -> Response {
    let (alloc, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let label = series
        .iter()
        .map(|item| item.label.as_str())
        .find(|label| !label.is_empty())
        .unwrap_or("Chart");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, label));
    ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_role(egui::accesskit::Role::Image);
        node.set_label(label);
    });
    let pal = crate::theme::palette(ui);
    let ppp = ui.ctx().pixels_per_point();
    let rect = snap_rect(alloc, ppp);
    let painter = ui.painter_at(rect).with_clip_rect(rect);

    paint_frame(&painter, rect, ppp, &pal, 6, 4, bg);
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(1.0),
            4.0,
            Stroke::new(2.0, pal.accent),
            egui::StrokeKind::Inside,
        );
    }
    let max_len = series
        .iter()
        .map(|item| item.samples.len())
        .max()
        .unwrap_or(0);
    let keyboard = keyboard_sample(ui, &response, axis, max_len);
    let series_id = response.id.with("keyboard-series");
    let mut active_series = ui
        .ctx()
        .data(|data| data.get_temp::<usize>(series_id))
        .unwrap_or(0);
    active_series = active_series.min(series.len().saturating_sub(1));
    let series_key = if response.has_focus() && keyboard.is_some_and(|(_, changed)| !changed) {
        ui.input(|input| {
            [egui::Key::ArrowUp, egui::Key::ArrowDown]
                .into_iter()
                .find(|key| input.key_pressed(*key) && !input.modifiers.any())
        })
    } else {
        None
    };
    if let Some(key) = series_key {
        active_series = if key == egui::Key::ArrowDown {
            (active_series + 1).min(series.len().saturating_sub(1))
        } else {
            active_series.saturating_sub(1)
        };
        ui.ctx()
            .data_mut(|data| data.insert_temp(series_id, active_series));
        ui.input_mut(|input| {
            input.consume_key(Default::default(), key);
        });
        ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
    }

    // Time-proportional x over the CONFIGURED window — see [`TimeAxis`].
    let bounds = axis.and_then(|a| a.bounds());
    // Callers must hand over one timestamp per sample (every series extractor
    // maps over the same window). Indexing blind would turn a future
    // desynchronization — a bug class this code has already seen once, see
    // `series_extractors_stay_aligned_with_window` — into a panic in the paint
    // path, i.e. a crashed task manager. The samples a short timestamp slice
    // cannot place start a run of their own instead of being stitched onto the
    // time-based one ([`ChartGeometry`]), which is also the honest picture: they
    // have no instant, so they borrow no coordinate from the axis.
    //
    // Built ONCE per series, for both paint passes: filling and outlining
    // recomputed the points separately before, which is how a fill and its own
    // outline drift apart.
    let plots: Vec<SeriesPaths> = series
        .iter()
        .map(|s| {
            // A one-sample series has no line and no area to paint, and
            // `even_x` has no spacing to divide (0 / 0) for it. Every series
            // reaches two samples on the next tick.
            if s.samples.len() < 2 {
                return SeriesPaths::new();
            }
            ChartGeometry::new(rect, y_max, axis, s.samples.len()).runs(&s.samples)
        })
        .collect();
    // Fills FIRST: an outer series' translucent fill must never dim an inner
    // series' line (the old single-pass order made overlapping series muddy and
    // hard to read).
    for (s, runs) in series.iter().zip(&plots) {
        paint_runs_fill(&painter, runs, rect.bottom(), area_fill(s.color));
    }
    // Lines in a second pass, over the SAME geometry. A run of one sample is the
    // dot it always was (see [`paint_runs_line`]).
    for (s, runs) in series.iter().zip(&plots) {
        paint_runs_line(&painter, runs, 1.25, s.color);
    }

    // Hover: every series' value at the pointer, in a box AT the pointer.
    let hover = if keyboard.is_some() {
        None
    } else {
        hover_pos(ui, &response, rect)
    };
    // A keyboard cursor marks one sample rather than a run, so it only needs
    // that index's x — through the same builder, so it cannot land anywhere but
    // on the curve it marks.
    let cursor_x = |idx: usize| ChartGeometry::new(rect, y_max, axis, max_len.max(2)).x(idx);
    if let Some(pos) =
        hover.or_else(|| keyboard.map(|(idx, _)| Pos2::new(cursor_x(idx), rect.top() + 20.0)))
    {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        // Time-proportional x: map the pointer TIME back to the nearest
        // sample — even-spacing math skips across sampling gaps.
        let first = first_on_axis(axis, bounds);
        let time_idx = keyboard.map(|(idx, _)| idx).or_else(|| {
            bounds
                .zip(axis)
                .map(|((t0, span), a)| nearest_sample(a.stamps, t0, span, frac).max(first))
                .map(|under| {
                    pinned_sample(ui.ctx(), response.id, axis, bounds, pos, under).max(first)
                })
        });
        let mut rows = Vec::with_capacity(series.len());
        let mut dots = Vec::with_capacity(series.len());
        let mut marker = None;
        for s in series {
            let n = s.samples.len();
            if n < 2 {
                continue;
            }
            let geometry = ChartGeometry::new(rect, y_max, axis, n);
            let idx = time_idx
                .map_or_else(
                    || (frac * (n - 1) as f32).round() as usize,
                    |i| i.min(n - 1),
                )
                .min(n - 1);
            if let Some(value) = s.samples.get(idx).copied() {
                rows.push(ReadoutRow {
                    color: s.color,
                    label: s.label.clone(),
                    value: readout_text(value, fmt),
                });
                if let Some(value) = value.filter(|value| value.is_finite()) {
                    dots.push((geometry.y(value), s.color));
                }
                marker.get_or_insert((geometry.x(idx), idx));
            }
        }
        if let Some((x, idx)) = marker {
            paint_marker(&painter, &pal, rect, ppp, x, &dots);
            readout(ui, pos, &sample_age(axis, idx), &rows);
            if let Some((_, changed)) = keyboard
                && let Some(row) = rows.get(active_series)
            {
                chart_accessible_value(
                    ui,
                    &response,
                    format!("{}: {} {}", sample_age(axis, idx), row.label, row.value),
                    changed || series_key.is_some(),
                );
            }
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_samples_split_lines_and_preserve_real_zeroes() {
        assert_eq!(
            measured_runs(&[Some(0.0), Some(2.0), None, Some(4.0), Some(f64::NAN)]),
            vec![vec![(0, 0.0), (1, 2.0)], vec![(3, 4.0)]]
        );
        assert!(measured_runs(&[None, None]).is_empty());
    }

    #[test]
    fn keyboard_scrub_exposes_a_stable_sample_to_accesskit() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 500.0));
        let chart_id = std::cell::Cell::new(None);
        let render = |events: Vec<egui::Event>, stamps: &[u64], values: Vec<Option<f64>>| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen),
                    focused: true,
                    events,
                    ..Default::default()
                },
                |root| {
                    egui::CentralPanel::default().show(root, |ui| {
                        chart_id.set(Some(
                            ui.push_id("test-chart", |ui| {
                                chart_multi(
                                    ui,
                                    Vec2::new(400.0, 120.0),
                                    &[MultiSeries::with_gaps(
                                        "CPU",
                                        values.clone(),
                                        Color32::LIGHT_BLUE,
                                    )],
                                    100.0,
                                    Some(TimeAxis {
                                        stamps,
                                        window_ms: 60_000,
                                    }),
                                    fmt_percent,
                                )
                            })
                            .inner
                            .id,
                        ));
                    });
                },
            );
            output.textures_delta.clear();
            output
                .platform_output
                .accesskit_update
                .expect("chart accessibility tree")
        };
        render(
            vec![],
            &[1_000, 2_000, 3_000],
            vec![Some(1.0), Some(2.0), Some(3.0)],
        );
        let id = chart_id.get().expect("chart target");
        ctx.memory_mut(|memory| memory.request_focus(id));
        let key = egui::Event::Key {
            key: egui::Key::ArrowLeft,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        };
        let tree = render(
            vec![key.clone()],
            &[1_000, 2_000, 3_000],
            vec![Some(1.0), Some(2.0), Some(3.0)],
        );
        let value = tree
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == id.accesskit_id())
            .map(|(_, node)| {
                assert_eq!(node.role(), egui::accesskit::Role::Image);
                node.value().expect("spoken sample").to_owned()
            })
            .expect("chart node");
        assert!(value.contains(&fmt_percent(2.0)));
        let tree = render(
            vec![],
            &[1_000, 2_000, 3_000, 4_000],
            vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
        );
        let after = tree
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == id.accesskit_id())
            .and_then(|(_, node)| node.value());
        assert_eq!(
            after,
            Some(value.as_str()),
            "new telemetry keeps the chosen historical sample"
        );
        let tree = render(
            vec![key],
            &[1_000, 2_000, 3_000, 4_000],
            vec![None, Some(2.0), Some(3.0), Some(4.0)],
        );
        let missing = tree
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == id.accesskit_id())
            .and_then(|(_, node)| node.value())
            .unwrap();
        assert!(
            missing.contains('—'),
            "unavailable sample must be spoken as unavailable: {missing}"
        );
    }

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
        let geometry = ChartGeometry::new(rect, 100.0, Some(axis), stamps.len());
        let at = |i: usize| geometry.x(i);

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

    /// Polyline points of every path a chart frame painted, in shape order.
    fn painted_paths(output: &mut egui::FullOutput) -> Vec<Vec<Pos2>> {
        output.textures_delta.clear();
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                Shape::Path(path) => Some(path.points.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every position a shape hands to epaint, including the ones a bounding
    /// rect would hide.
    ///
    /// `Shape::visual_bounding_rect` cannot stand in for this check:
    /// `Rect::extend_with` ignores non-finite points, so a poisoned path still
    /// reports a sane bound and passes the clip test — which is exactly how the
    /// artifact below reached the rasterizer instead of being culled.
    fn assert_shape_positions_finite(shape: &Shape) {
        fn finite(label: &str, point: Pos2) {
            assert!(
                point.x.is_finite() && point.y.is_finite(),
                "{label} carries a non-finite position: {point:?}"
            );
        }
        match shape {
            Shape::Path(path) => path.points.iter().for_each(|point| finite("path", *point)),
            Shape::Mesh(mesh) => mesh
                .vertices
                .iter()
                .for_each(|vertex| finite("mesh", vertex.pos)),
            Shape::LineSegment { points, .. } => {
                points.iter().for_each(|point| finite("segment", *point));
            }
            Shape::Circle(circle) => finite("circle", circle.center),
            Shape::Rect(rect) => {
                finite("rect min", rect.rect.min);
                finite("rect max", rect.rect.max);
            }
            Shape::Text(text) => finite("text", text.pos),
            Shape::Vec(shapes) => shapes.iter().for_each(assert_shape_positions_finite),
            _ => {}
        }
    }

    /// A missing or non-finite sample is a GAP: the runs it produces are finite,
    /// their x strictly increases, and nothing continuous spans the bad sample.
    ///
    /// The tile did not split at all before — a NaN went straight into
    /// `Pos2::new`, and `f64::clamp` returns NaN for NaN, so it stayed NaN
    /// through the scale and became a path point epaint happily stroked.
    #[test]
    fn gaps_and_non_finite_samples_produce_finite_broken_runs() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(300.0, 80.0));
        let samples = [
            1.0,
            2.0,
            f64::NAN,
            4.0,
            f64::INFINITY,
            6.0,
            f64::NEG_INFINITY,
            8.0,
        ];
        let runs = ChartGeometry::new(rect, 100.0, None, samples.len()).runs_of(&samples);
        let lengths: Vec<usize> = runs.iter().map(Vec::len).collect();
        assert_eq!(
            lengths,
            vec![2, 1, 1, 1],
            "a gap must split the line: {runs:?}"
        );
        for run in &runs {
            for point in run {
                assert!(point.x.is_finite() && point.y.is_finite(), "{point:?}");
                assert!((rect.top()..=rect.bottom()).contains(&point.y), "{point:?}");
            }
            assert!(
                run.windows(2).all(|pair| pair[0].x < pair[1].x),
                "x must advance along a run: {run:?}"
            );
        }
        // The run that ends at index 1 and the one that starts at index 3 are
        // separate paths, so no segment covers the NaN between them.
        let end_of_first = runs[0].last().expect("a measured run").x;
        assert!(
            end_of_first < runs[1][0].x,
            "the runs are out of order: {runs:?}"
        );
    }

    /// A wall-clock step back starts a new run instead of pinning the sample
    /// onto the plot edge.
    ///
    /// `x_on_axis` clamps the fraction to `[-1, 1]`, so an older stamp that
    /// follows a newer one used to land on the left edge and the two were joined
    /// by a straight, chart-wide diagonal — the stroke from frac 0.447 at a
    /// value of 100 % or more down to frac 0.952 at 10 % in the report.
    #[test]
    fn a_step_back_in_time_starts_a_new_run() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 100.0));
        // Samples tick once a second; index 3 comes from BEFORE index 2, the
        // way a clock correction or a resume leaves the history buffer.
        let stamps = [10_000u64, 11_000, 12_000, 4_000, 5_000];
        let samples = [10.0, 20.0, 30.0, 40.0, 50.0];
        let axis = TimeAxis::new(&stamps, 60);
        let runs = ChartGeometry::new(rect, 100.0, Some(axis), samples.len()).runs_of(&samples);
        assert_eq!(runs.iter().map(Vec::len).collect::<Vec<_>>(), vec![3, 2]);
        for run in &runs {
            assert!(
                run.windows(2).all(|pair| pair[0].x < pair[1].x),
                "a run bridges backwards in time: {run:?}"
            );
        }

        // A duplicated stamp is not a segment either: two samples on one instant
        // would be a zero-length line, so the second one starts its own run.
        let stamps = [10_000u64, 10_000, 11_000];
        let axis = TimeAxis::new(&stamps, 60);
        let runs = ChartGeometry::new(rect, 100.0, Some(axis), 3).runs_of(&[10.0, 20.0, 30.0]);
        assert_eq!(runs.iter().map(Vec::len).collect::<Vec<_>>(), vec![1, 2]);
    }

    /// A stamp PAST the axis end is dropped, not pinned onto the right edge.
    ///
    /// The axis' right edge is the newest ARRIVAL, not the largest stamp, so a
    /// wall clock that jumps forward and is later corrected leaves samples
    /// already in the buffer stamped beyond the window. `placeable_on_axis`
    /// only bounded the axis on the LEFT, so `x_on_axis` clamped each of those
    /// onto the right edge: stale values painted where the newest measurement
    /// belongs, and — when one followed an in-window sample — a segment
    /// bridging time nobody measured, exactly the stroke class the run builder
    /// exists to prevent.
    #[test]
    fn a_stamp_past_the_axis_end_is_dropped_instead_of_pinned_to_the_right_edge() {
        const NEWEST: u64 = 1_797_000_000_000;
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 50.0));
        // Arrival order with the newest arrival SMALLER than the three samples
        // already buffered, i.e. an NTP correction back onto the true clock.
        let stamps = [NEWEST + 5_000, NEWEST + 6_000, NEWEST + 7_000, NEWEST];
        let samples = [10.0, 20.0, 30.0, 40.0];
        let axis = TimeAxis::new(&stamps, 10);
        let geometry = ChartGeometry::new(rect, 100.0, Some(axis), samples.len());
        let runs = geometry.runs_of(&samples);
        let points: Vec<Pos2> = runs.into_iter().flatten().collect();
        assert_eq!(
            points.len(),
            1,
            "only the newest sample is on the axis, but the stale ones were painted too: \
             {points:?}"
        );
        assert!(
            (points[0].x - rect.right()).abs() < 0.5,
            "the newest sample is not on the right edge: {:?}",
            points[0]
        );
        // 40 % of a 50 pt tall chart: the NEWEST value, not one of the stale
        // ones the clamp used to draw in its place.
        assert!(
            (points[0].y - 30.0).abs() < 0.5,
            "the right edge carries a stale value: {:?}",
            points[0]
        );
    }

    /// However stale a stamp is, the lead-in overhang stays within one chart
    /// width — the bound that keeps a pathological sample from tessellating a
    /// mesh across the screen.
    #[test]
    fn a_pathological_stamp_stays_within_one_chart_width() {
        let rect = Rect::from_min_size(Pos2::new(100.0, 0.0), Vec2::new(600.0, 100.0));
        // Epoch zero followed by two samples from 2027, one sampling interval
        // apart: the lead-in is ~19 years older than the window it is carried
        // into, which is the post-suspend buffer (history `[… 60 s, 360 s]`
        // with a 60 s window) taken to its extreme.
        let stamps = [1_000u64, 1_800_000_000_000, 1_800_000_001_000];
        let axis = TimeAxis::new(&stamps, 60);
        let geometry = ChartGeometry::new(rect, 100.0, Some(axis), stamps.len());
        // The marker position is still a bounded, finite x -- it is a marker,
        // not a segment.
        for x in (0..stamps.len()).map(|i| geometry.x(i)) {
            assert!(x.is_finite(), "{x}");
            assert!(
                x >= rect.left() - rect.width(),
                "overhang past one chart width: {x}"
            );
            assert!(x <= rect.right(), "past the right edge: {x}");
        }
        // The stale stamp is the lead-in: left of the chart, inside the bound.
        assert!(geometry.x(0) < rect.left());
        assert!(geometry.x(0) >= rect.left() - rect.width());
        // But it is NOT painted: the axis cannot place a sample four windows
        // old, so the run starts at the first sample it can place. Drawing it
        // anyway is the "stroke from its own left edge" of the report.
        let samples = [10.0, 40.0, 70.0];
        let runs = geometry.runs_of(&samples);
        assert_eq!(
            runs.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![2],
            "a sample the axis cannot place must break the run, not join it"
        );
        // Not a gap, though: the axis can still place the other two, in order.
        assert!(geometry.x(1) < geometry.x(2));
        // ...and no painted edge spans more than one sampling interval, which is
        // what a bridged 19-year jump would (the two placeable samples are one
        // second apart on a sixty-second axis, so one interval is width/60).
        let interval = rect.width() / 60.0;
        for run in &runs {
            for pair in run.windows(2) {
                let dx = (pair[1].x - pair[0].x).abs();
                assert!(
                    dx <= interval + 0.5,
                    "a painted edge spans {dx} px, more than one sample interval"
                );
            }
        }
    }

    /// A series longer than the timestamp slice never stitches the time axis to
    /// the even-spacing fallback inside one run: the samples the axis cannot
    /// place start a run of their own, so every run has exactly one x source.
    #[test]
    fn a_short_timestamp_slice_never_mixes_two_coordinate_systems() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 100.0));
        let stamps = [1_000u64, 2_000, 3_000, 4_000];
        let samples: Vec<f64> = (0..10u32).map(|i| 20.0 + f64::from(i)).collect();
        let axis = TimeAxis::new(&stamps, 60);
        let geometry = ChartGeometry::new(rect, 100.0, Some(axis), samples.len());
        let runs = geometry.runs_of(&samples);
        assert_eq!(runs.iter().map(Vec::len).collect::<Vec<_>>(), vec![4, 6]);
        let bounds = axis.bounds().expect("axis bounds");
        for (i, point) in runs[0].iter().enumerate() {
            assert_eq!(point.x, x_on_axis(rect, bounds, stamps[i]), "axis x {i}");
        }
        for (k, point) in runs[1].iter().enumerate() {
            assert_eq!(point.x, even_x(rect, k + 4, samples.len()), "even x {k}");
        }
    }

    /// The kernel band is indexed against the value series it sits under.
    ///
    /// It used to iterate its own slice while taking x from the value series'
    /// stamps, so a length mismatch slid the band sideways along the axis, and a
    /// sample with no measurement still got a band under it — a stroke of the
    /// kernel colour over a region where the tile paints nothing at all.
    #[test]
    fn a_kernel_band_is_indexed_against_its_value_series() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(300.0, 100.0));
        let values = [10.0f64; 6];
        let geometry = ChartGeometry::new(rect, 100.0, None, values.len());
        // A slice of the wrong length is unavailable, never shifted.
        assert!(kernel_runs(&values, &[1.0, 2.0, 3.0], &geometry).is_empty());

        let kernels = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let band = kernel_runs(&values, &kernels, &geometry);
        assert_eq!(band.len(), 1);
        assert_eq!(
            band[0].iter().map(|point| point.x).collect::<Vec<_>>(),
            geometry.runs_of(&values)[0]
                .iter()
                .map(|point| point.x)
                .collect::<Vec<_>>(),
            "the band must sit on the value series' x"
        );
        // Kernel time is a share that rises here, so the band's y falls.
        assert!(band[0][0].y > band[0][5].y);

        // No value sample, no band: the runs break where the values do.
        let mut gapped = values;
        gapped[2] = f64::NAN;
        let band = kernel_runs(&gapped, &kernels, &geometry);
        assert_eq!(band.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 3]);
    }

    /// The per-core tile and the multi-series chart must build the SAME
    /// geometry from the same samples.
    ///
    /// They diverged once: the tile had its own gap-unaware mapping, so one NaN
    /// series painted a clean curve in the big chart and a garbage diagonal in
    /// the tile. Both now paint through `ChartGeometry`, and this pins it
    /// through the public entry points rather than through the builder alone.
    #[test]
    fn a_core_tile_and_a_multi_series_chart_build_the_same_geometry() {
        let stamps: Vec<u64> = (0..40u64).map(|i| 1_000 + i * 500).collect();
        let samples: Vec<f64> = (0..40u32)
            .map(|i| {
                if i % 11 == 7 {
                    f64::NAN
                } else {
                    f64::from(i % 9) * 11.0
                }
            })
            .collect();
        let paths = |tile: bool| -> Vec<Vec<Pos2>> {
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 300.0))),
                    ..Default::default()
                },
                |ui| {
                    let axis = Some(TimeAxis::new(&stamps, 60));
                    if tile {
                        core_chart(
                            ui,
                            Vec2::new(180.0, 60.0),
                            &samples,
                            None,
                            Color32::BLUE,
                            "CPU 0",
                            axis,
                        );
                    } else {
                        chart_multi(
                            ui,
                            Vec2::new(180.0, 60.0),
                            &[MultiSeries::new("CPU", samples.clone(), Color32::BLUE)],
                            100.0,
                            axis,
                            fmt_percent,
                        );
                    }
                },
            );
            painted_paths(&mut output)
        };
        let tile = paths(true);
        let multi = paths(false);
        assert!(
            tile.len() > 1,
            "the fixture has to produce gapped runs: {tile:?}"
        );
        assert_eq!(tile, multi, "the tile and the big chart diverge");
    }

    /// Acceptance criterion, end to end: whatever the data does, no shape a
    /// chart hands to epaint carries a non-finite position.
    ///
    /// The fixture carries all four inputs that used to produce poisoned
    /// geometry at once: non-finite values (NaN and both infinities), a
    /// wall-clock step back, a duplicated stamp, and a kernel slice shorter than
    /// the value series. All three chart shapes paint from it.
    #[test]
    fn no_chart_shape_carries_a_non_finite_position() {
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(500.0, 400.0));
        let stamps = [10_000u64, 11_000, 12_000, 3_000, 12_000, 14_000, 15_000];
        let samples = [
            10.0,
            f64::NAN,
            30.0,
            f64::INFINITY,
            50.0,
            f64::NEG_INFINITY,
            70.0,
        ];
        let kernel = [1.0, 2.0, 3.0];
        let measured: Vec<Option<f64>> = samples.iter().copied().map(Some).collect();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| {
                let axis = Some(TimeAxis::new(&stamps, 60));
                core_chart(
                    ui,
                    Vec2::new(120.0, 60.0),
                    &samples,
                    Some((kernel.as_slice(), Color32::RED)),
                    Color32::BLUE,
                    "CPU 0",
                    axis,
                );
                chart_multi(
                    ui,
                    Vec2::new(240.0, 90.0),
                    &[MultiSeries::new("CPU", samples.to_vec(), Color32::BLUE)],
                    100.0,
                    axis,
                    fmt_percent,
                );
                paint_sparkline_with_gaps(
                    ui,
                    Rect::from_min_size(Pos2::new(4.0, 4.0), Vec2::new(64.0, 40.0)),
                    &measured,
                    Color32::BLUE,
                    Color32::BLACK,
                );
            },
        );
        // Nothing paints this frame to a surface, so the font-atlas delta has to
        // be dropped explicitly.
        output.textures_delta.clear();
        assert!(output.shapes.len() > 8, "the fixtures must paint something");
        for clipped in &output.shapes {
            assert_shape_positions_finite(&clipped.shape);
        }
    }
}

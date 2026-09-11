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
fn paint_marker(painter: &egui::Painter, rect: Rect, ppp: f32, x: f32, dots: &[(f32, Color32)]) {
    let x = snap_line(x, ppp);
    painter.line_segment(
        [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
        Stroke::new(hairline(ppp), Color32::from_white_alpha(64)),
    );
    for (y, color) in dots {
        painter.circle_filled(Pos2::new(x, *y), 2.6, *color);
    }
}

/// Pointer position when this chart is genuinely hovered.
///
/// `Response::hovered` rather than a bare rect test: an open context menu or
/// a dialog above the chart must suppress the readout, and a rect test alone
/// would keep painting it through them.
fn hover_pos(ui: &egui::Ui, response: &Response, rect: Rect) -> Option<Pos2> {
    if !response.hovered() {
        return None;
    }
    let pos = ui.input(|i| i.pointer.hover_pos())?;
    rect.contains(pos).then_some(pos)
}

/// Age caption for sample `idx` of `timestamps_ms`, relative to the newest
/// sample in the window. Empty when the caller charted without timestamps.
fn sample_age(timestamps_ms: Option<&[u64]>, idx: usize) -> String {
    let Some(ts) = timestamps_ms else {
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
pub fn paint_sparkline(ui: &egui::Ui, rect: egui::Rect, samples: &[f64], color: Color32) {
    let ppp = ui.ctx().pixels_per_point();
    let rect = snap_rect(rect, ppp);
    let painter = ui.painter_at(rect);
    let pal = crate::theme::palette(ui);

    painter.rect_filled(rect, 2.0, pal.card_bg);
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

    let fill = Color32::from_rgba_premultiplied(color.r(), color.g(), color.b(), 34);
    fill_area_to_baseline(&painter, &pts, rect.bottom(), fill);
    painter.add(Shape::line(pts, Stroke::new(1.25, color)));
}

/// Per-logical-processor tile: bordered, faint horizontal grid, filled area.
/// With `kernels` (same length), the kernel-time share is overlaid darker —
/// Task Manager's "Show kernel times" (§14.4).
///
/// `label` names the tile in the hover readout ("CPU 0"); `timestamps_ms`
/// gives that readout the sample's age, exactly as on the big charts.
pub fn core_chart(
    ui: &mut egui::Ui,
    size: Vec2,
    samples: &[f64],
    kernels: Option<&[f64]>,
    color: Color32,
    label: &str,
    timestamps_ms: Option<&[u64]>,
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
    let x = |i: usize| rect.left() + rect.width() * i as f32 / (n - 1) as f32;
    let y = |v: f64| rect.bottom() - (v.clamp(0.0, 100.0) / 100.0) as f32 * rect.height();
    let pts: Vec<Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| Pos2::new(x(i), y(*v)))
        .collect();
    let fill = Color32::from_rgba_premultiplied(color.r(), color.g(), color.b(), 36);
    fill_area_to_baseline(&painter, &pts, rect.bottom(), fill);

    // Kernel overlay: darker band under the user portion. Painted AFTER the
    // user fill but BEFORE the line, so it darkens the lower region without
    // burying the series line (the old order drew the line first).
    let kernel_fill =
        Color32::from_rgba_premultiplied(color.r() / 2, color.g() / 2, color.b() / 2, 70);
    if let Some(k) = kernels {
        let kpts: Vec<Pos2> = k
            .iter()
            .enumerate()
            .map(|(i, v)| Pos2::new(x(i), y(*v)))
            .collect();
        fill_area_to_baseline(&painter, &kpts, rect.bottom(), kernel_fill);
    }
    painter.add(Shape::line(pts, Stroke::new(1.4, color)));

    if let Some(pos) = hover_pos(ui, &response, rect) {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let idx = ((frac * (n - 1) as f32).round() as usize).min(n - 1);
        let mut rows = vec![ReadoutRow {
            color,
            label: label.to_owned(),
            value: fmt_percent(samples[idx]),
        }];
        let mut dots = vec![(y(samples[idx]), color)];
        if let Some(k) = kernels.and_then(|k| k.get(idx)) {
            rows.push(ReadoutRow {
                color: kernel_fill,
                label: i18n::tr(K::ShowKernelTimesShort).to_owned(),
                value: fmt_percent(*k),
            });
            dots.push((y(*k), kernel_fill));
        }
        paint_marker(&painter, rect, ppp, x(idx), &dots);
        readout(ui, pos, &sample_age(timestamps_ms, idx), &rows);
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
    timestamps_ms: Option<&[u64]>,
    fmt: ValueFmt,
) -> Response {
    let (alloc, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let pal = crate::theme::palette(ui);
    let ppp = ui.ctx().pixels_per_point();
    let rect = snap_rect(alloc, ppp);
    let painter = ui.painter_at(rect).with_clip_rect(rect);

    paint_frame(&painter, rect, ppp, &pal, 6, 4);

    let y = |v: f64| rect.bottom() - (v.clamp(0.0, y_max) / y_max) as f32 * rect.height();
    // Time-proportional x mapping over the shared window. `saturating_sub`
    // because a wall-clock step backward can put a future-stamped older
    // point at the front (then `last - first` would underflow/wrap).
    let t_span = timestamps_ms.and_then(|ts| {
        let first = *ts.first()?;
        let last = *ts.last()?;
        Some((first, last.saturating_sub(first).max(1)))
    });
    // Callers must hand over one timestamp per sample (every series extractor
    // maps over the same window). Indexing blind would turn a future
    // desynchronization — a bug class this code has already seen once, see
    // `series_extractors_stay_aligned_with_window` — into a panic in the paint
    // path, i.e. a crashed task manager. A short timestamp slice degrades to
    // even spacing for the samples it cannot place instead.
    let even = |i: usize, n: usize| rect.left() + rect.width() * i as f32 / (n - 1) as f32;
    let x_at = |i: usize, n: usize| match t_span {
        Some((t0, span)) => match timestamps_ms.and_then(|ts| ts.get(i)) {
            Some(stamp) => {
                let t = stamp.saturating_sub(t0);
                rect.left() + rect.width() * (t as f32 / span as f32).clamp(0.0, 1.0)
            }
            None => even(i, n),
        },
        None => even(i, n),
    };
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
        let fill = Color32::from_rgba_premultiplied(s.color.r(), s.color.g(), s.color.b(), 34);
        fill_area_to_baseline(&painter, &pts, rect.bottom(), fill);
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
        let time_idx = t_span.zip(timestamps_ms).map(|((t0, span), ts)| {
            let tq = (t0 as f32 + frac * span as f32) as u64;
            ts.partition_point(|&t| t < tq).saturating_sub(1)
        });
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
            paint_marker(&painter, rect, ppp, x, &dots);
            readout(ui, pos, &sample_age(timestamps_ms, idx), &rows);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

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
                        Some(&stamps),
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
                            Some(&stamps),
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
                            Some(&stamps),
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

    /// The readout head names the sample's age, so a reading taken mid-window
    /// cannot be mistaken for the current value.
    #[test]
    fn sample_age_reports_distance_from_the_newest_sample() {
        let ts: Vec<u64> = (0..=12).map(|i| i * 5_000).collect();
        assert_eq!(sample_age(Some(&ts), 12), i18n::tr(K::GraphNow));
        assert!(sample_age(Some(&ts), 10).contains("10"));
        // Beyond a minute it switches to m:ss.
        assert!(sample_age(Some(&ts), 0).contains("1:00"));
        assert!(sample_age(None, 0).is_empty());
        // An out-of-range index is a caller slip, not a panic.
        assert!(sample_age(Some(&ts), 99).is_empty());
    }
}

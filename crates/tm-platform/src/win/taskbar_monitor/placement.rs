//! Pure geometry: choosing a free taskbar gap for the widget and anchoring
//! its context menu.

use super::PixelRect;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTALIGN, TPM_TOPALIGN,
};

pub(super) fn context_menu_anchor(
    cursor: POINT,
    taskbar: Option<PixelRect>,
    monitor: Option<PixelRect>,
) -> (POINT, u32) {
    let Some(bar) = taskbar.filter(|b| !b.is_empty()) else {
        return (cursor, TPM_BOTTOMALIGN.0);
    };
    if bar.width() >= bar.height() {
        let is_top = monitor
            .filter(|m| !m.is_empty())
            .map_or(bar.top < 200, |m| bar.top - m.top < m.bottom - bar.bottom);
        if is_top {
            (
                POINT {
                    x: cursor.x,
                    y: bar.bottom,
                },
                TPM_TOPALIGN.0,
            )
        } else {
            (
                POINT {
                    x: cursor.x,
                    y: bar.top,
                },
                TPM_BOTTOMALIGN.0,
            )
        }
    } else {
        let is_left = monitor
            .filter(|m| !m.is_empty())
            .map_or(bar.left < 200, |m| bar.left - m.left < m.right - bar.right);
        if is_left {
            (
                POINT {
                    x: bar.right,
                    y: cursor.y,
                },
                TPM_LEFTALIGN.0,
            )
        } else {
            (
                POINT {
                    x: bar.left,
                    y: cursor.y,
                },
                TPM_RIGHTALIGN.0,
            )
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AnchorSlot {
    /// Positioned beside `TrayNotifyWnd` (stepped back past any adjacent XAML
    /// controls such as Widgets or Copilot).
    TrayAdjacent,
    /// Positioned in an alternate open taskbar gap, anchored toward the
    /// taskbar edge so centered icons growing/shrinking keep it stationary.
    AlternateGap { anchor_to_end: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PlacementResult {
    pub(super) pos: (i32, i32),
    pub(super) slot: AnchorSlot,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Gap1D {
    pub(super) avail_start: i32,
    pub(super) avail_end: i32,
    pub(super) tray_adjacent: bool,
    pub(super) dist_to_tray: i32,
}

impl Gap1D {
    pub(super) fn avail_len(self) -> i32 {
        self.avail_end - self.avail_start
    }
}

/// Clamps `rect` into `bar` and reports whether the result still fills the
/// bar's cross axis (its full height on a horizontal bar, its full width on a
/// vertical one). Every edge that escapes the bar lands on the bar's edge and
/// an edge already inside is returned untouched, so the clamp only ever
/// shrinks: it can never enlarge the widget past the bar or past itself.
///
/// Filling the cross axis exactly is the placement contract, not a nicety: a
/// shorter box leaves a dead band where clicks land on the taskbar instead of
/// the widget, and a taller one overlaps the bar it was placed on.
pub(super) fn clamp_into_taskbar(rect: PixelRect, bar: PixelRect) -> (PixelRect, bool) {
    if bar.is_empty() {
        return (rect, false);
    }
    let clamped = PixelRect {
        left: rect.left.clamp(bar.left, bar.right),
        top: rect.top.clamp(bar.top, bar.bottom),
        right: rect.right.clamp(bar.left, bar.right),
        bottom: rect.bottom.clamp(bar.top, bar.bottom),
    };
    let fills_cross = if bar.width() >= bar.height() {
        clamped.height() == bar.height()
    } else {
        clamped.width() == bar.width()
    };
    (clamped, fills_cross)
}

/// Computes collision-free widget coordinates inside `taskbar_rect` from out-of-process
/// UIA obstacle rectangles plus `tray_notify_rect`, preferring the tray-adjacent slot
/// and falling back to an alternate open gap before reporting `None` (hide).
/// Unknown or empty UIA occupancy keeps the widget hidden, and so does any widget
/// that cannot fit the bar's cross axis without escaping it.
pub(super) fn solve_taskbar_placement(
    taskbar_rect: PixelRect,
    tray_notify_rect: Option<PixelRect>,
    uia_occupied: Option<&[PixelRect]>,
    widget_size: (i32, i32),
    dpi: u32,
    prev_slot: Option<AnchorSlot>,
    prefer_keep_compact: bool,
) -> Option<PlacementResult> {
    let (w, h) = widget_size;
    if w <= 0 || h <= 0 || taskbar_rect.is_empty() {
        return None;
    }

    let uia_rects = uia_occupied.filter(|s| !s.is_empty())?;

    let scale = dpi.max(96) as f32 / 96.0;
    let gap = ((6.0 * scale).round() as i32).max(2);
    let hysteresis = ((16.0 * scale).round() as i32).max(8);
    let tray_adj_limit = (320.0 * scale).round() as i32;

    let horizontal = taskbar_rect.width() >= taskbar_rect.height();
    let project = |r: PixelRect| -> (i32, i32) {
        if horizontal {
            (r.left, r.right)
        } else {
            (r.top, r.bottom)
        }
    };
    let (bar_start, bar_end) = project(taskbar_rect);
    // The widget is placed at the bar's cross start whenever it fills the bar's
    // cross axis, which is the normal case: equality is not a tolerance here.
    let (cross_start, cross_avail, widget_len, widget_cross) = if horizontal {
        (taskbar_rect.top, taskbar_rect.height(), w, h)
    } else {
        (taskbar_rect.left, taskbar_rect.width(), h, w)
    };
    if widget_cross > cross_avail {
        return None;
    }
    let cross_pos = cross_start + (cross_avail - widget_cross) / 2;
    let bar_len = bar_end - bar_start;
    let req_len = if prefer_keep_compact {
        widget_len + hysteresis
    } else {
        widget_len
    };
    if bar_len < req_len + 2 * gap {
        return None;
    }

    let notify_span = tray_notify_rect
        .filter(|n| n.intersects(taskbar_rect))
        .map(project)
        .map(|(s, e)| (s.max(bar_start), e.min(bar_end)))
        .filter(|(s, e)| e > s);

    // Filter out bogus container-sized UIA elements spanning most of the taskbar
    // (e.g., XAML root composition bridges or toolbar frames).
    let mut intervals: Vec<(i32, i32)> = uia_rects
        .iter()
        .copied()
        .filter(|r| !r.is_empty() && r.intersects(taskbar_rect))
        .filter_map(|r| {
            let (raw_s, raw_e) = project(r);
            let span = raw_e - raw_s;
            if bar_len >= 400 && span >= bar_len * 60 / 100 {
                return None;
            }
            let s = raw_s.max(bar_start);
            let e = raw_e.min(bar_end);
            (e > s).then_some((s, e))
        })
        .collect();
    if let Some(ns) = notify_span {
        intervals.push(ns);
    }
    if intervals.is_empty() {
        return None;
    }

    intervals.sort_unstable();
    let mut merged: Vec<(i32, i32)> = Vec::with_capacity(intervals.len());
    for (s, e) in intervals {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
            continue;
        }
        merged.push((s, e));
    }

    let (tray_at_end, tray_edge) = match notify_span {
        Some((ns, ne)) => {
            if ns - bar_start >= bar_end - ne {
                (true, ns)
            } else {
                (false, ne)
            }
        }
        None => (true, bar_end),
    };

    let mut gaps = Vec::new();
    let mut push_gap = |g_start: i32, g_end: i32| {
        let avail_start = g_start + gap;
        let avail_end = g_end - gap;
        if avail_end - avail_start < req_len {
            return;
        }
        let (on_main_side, dist_to_tray) = if tray_at_end {
            (g_end <= tray_edge + 2, (tray_edge - g_end).max(0))
        } else {
            (g_start >= tray_edge - 2, (g_start - tray_edge).max(0))
        };
        if !on_main_side {
            return;
        }
        gaps.push(Gap1D {
            avail_start,
            avail_end,
            tray_adjacent: dist_to_tray <= tray_adj_limit,
            dist_to_tray,
        });
    };

    let mut cursor = bar_start;
    for (s, e) in merged {
        if s > cursor {
            push_gap(cursor, s);
        }
        cursor = cursor.max(e);
    }
    if bar_end > cursor {
        push_gap(cursor, bar_end);
    }

    if gaps.is_empty() {
        return None;
    }

    let best_tray = gaps
        .iter()
        .copied()
        .filter(|g| g.tray_adjacent)
        .min_by_key(|g| (g.dist_to_tray, std::cmp::Reverse(g.avail_len())));
    let best_alt = gaps
        .iter()
        .copied()
        .filter(|g| !g.tray_adjacent)
        .max_by_key(|g| g.avail_len());

    let was_alt = matches!(prev_slot, Some(AnchorSlot::AlternateGap { .. }));
    let (chosen, slot, anchor_to_end) = match (best_tray, best_alt) {
        (Some(tray_g), Some(alt_g)) => {
            // Require extra margin before leaving an alternate open gap back to
            // the tray side so hover/animations at the threshold never oscillate.
            if was_alt && tray_g.avail_len() < widget_len + hysteresis {
                let end = (bar_end - alt_g.avail_end) < (alt_g.avail_start - bar_start);
                (alt_g, AnchorSlot::AlternateGap { anchor_to_end: end }, end)
            } else {
                (tray_g, AnchorSlot::TrayAdjacent, tray_at_end)
            }
        }
        (Some(tray_g), None) => (tray_g, AnchorSlot::TrayAdjacent, tray_at_end),
        (None, Some(alt_g)) => {
            let end = (bar_end - alt_g.avail_end) < (alt_g.avail_start - bar_start);
            (alt_g, AnchorSlot::AlternateGap { anchor_to_end: end }, end)
        }
        (None, None) => return None,
    };

    let primary = if anchor_to_end {
        chosen.avail_end - widget_len
    } else {
        chosen.avail_start
    };
    let pos = if horizontal {
        (primary, cross_pos)
    } else {
        (cross_pos, primary)
    };
    // Total contract: no returned placement puts the widget outside the bar on
    // either axis. Verify it here instead of trusting the arithmetic above.
    let candidate = PixelRect {
        left: pos.0,
        top: pos.1,
        right: pos.0.saturating_add(w),
        bottom: pos.1.saturating_add(h),
    };
    let (fitted, _) = clamp_into_taskbar(candidate, taskbar_rect);
    if fitted != candidate {
        return None;
    }
    Some(PlacementResult { pos, slot })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::taskbar_monitor::test_support::rect;

    #[test]
    fn positions_left_of_tray_notify_on_ltr_taskbar() {
        let monitor = PixelRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let taskbar = PixelRect {
            left: 0,
            top: 1032,
            right: 1920,
            bottom: 1080,
        };
        let notify = PixelRect {
            left: 1700,
            top: 1032,
            right: 1920,
            bottom: 1080,
        };
        let pos = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&[notify]),
            (160, 36),
            96,
            None,
            false,
        )
        .unwrap()
        .pos;
        assert!(monitor.covers(taskbar));
        assert_eq!(pos, (1700 - 160 - 6, 1032 + (48 - 36) / 2));
    }

    #[test]
    fn placement_steps_past_xaml_tray_adjacent_controls() {
        let taskbar = rect(0, 1032, 1920, 1080);
        let notify = rect(1700, 1032, 1920, 1080);
        // Win11 left-aligned: Start+Search+Apps occupy 0..480, Copilot sits right
        // next to TrayNotifyWnd at 1640..1694.
        let uia = [rect(0, 1032, 480, 1080), rect(1640, 1036, 1694, 1076)];
        let res = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&uia),
            (150, 36),
            96,
            None,
            false,
        )
        .expect("should fit left of Copilot");
        assert_eq!(res.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(res.pos.0, 1640 - 150 - 6);
        assert!(res.pos.0 + 150 <= 1640);
    }

    #[test]
    fn placement_relocates_to_open_centered_left_gap_with_hysteresis() {
        let taskbar = rect(0, 1032, 1920, 1080);
        let notify = rect(1700, 1032, 1920, 1080);
        let widgets = rect(8, 1036, 150, 1076);
        let copilot = rect(1640, 1036, 1694, 1076);

        // Right tray gap is 1640 - 1560 = 80px (too narrow for 150px widget),
        // while left open gap between Widgets (150) and centered Start+Apps (600) is 450px.
        let packed_right = [widgets, rect(600, 1034, 1560, 1078), copilot];
        let alt = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&packed_right),
            (150, 36),
            96,
            Some(AnchorSlot::TrayAdjacent),
            false,
        )
        .expect("should relocate to open left gap");
        assert_eq!(
            alt.slot,
            AnchorSlot::AlternateGap {
                anchor_to_end: false
            }
        );
        assert_eq!(alt.pos.0, 150 + 6);

        // Reopening right gap just above 150 + 2*6 = 162px (avail = 154px, < 150 + 16 hysteresis)
        // must keep widget anchored on the left instead of oscillating.
        let barely_open_right = [widgets, rect(600, 1034, 1474, 1078), copilot];
        let stayed = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&barely_open_right),
            (150, 36),
            96,
            Some(alt.slot),
            false,
        )
        .expect("should stay anchored in left gap within hysteresis band");
        assert_eq!(stayed.slot, alt.slot);
        assert_eq!(stayed.pos.0, 150 + 6);

        // Once right gap clears hysteresis (1640 - 1440 - 12 = 188px >= 166px), move back right.
        let wide_open_right = [widgets, rect(600, 1034, 1440, 1078), copilot];
        let returned = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&wide_open_right),
            (150, 36),
            96,
            Some(stayed.slot),
            false,
        )
        .expect("should return beside tray once ample space opens");
        assert_eq!(returned.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(returned.pos.0, 1640 - 150 - 6);
    }

    #[test]
    fn placement_adapts_compact_then_hides_when_packed_or_uia_is_unavailable() {
        let taskbar = rect(0, 1032, 1920, 1080);
        let notify = rect(1700, 1032, 1920, 1080);
        let chunk_items = |start: i32, end: i32| -> Vec<PixelRect> {
            let mut out = Vec::new();
            let mut x = start;
            while x < end {
                let next = (x + 120).min(end);
                out.push(rect(x, 1034, next, 1078));
                x = next;
            }
            out
        };

        // Only 142px raw -> 130px usable: normal (150px) does not fit, compact (118px) does.
        let tight = chunk_items(0, 1558);
        assert!(
            solve_taskbar_placement(
                taskbar,
                Some(notify),
                Some(&tight),
                (150, 36),
                96,
                None,
                false,
            )
            .is_none()
        );
        let compact_res = solve_taskbar_placement(
            taskbar,
            Some(notify),
            Some(&tight),
            (118, 36),
            96,
            None,
            false,
        )
        .expect("compact width should fit");
        assert_eq!(compact_res.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(compact_res.pos, (1700 - 118 - 6, 1032 + (48 - 36) / 2));

        // At 152px usable (>= 150 but < 150+16), prefer_keep_compact keeps compact active.
        let borderline = chunk_items(0, 1536);
        assert!(
            solve_taskbar_placement(
                taskbar,
                Some(notify),
                Some(&borderline),
                (150, 36),
                96,
                Some(AnchorSlot::TrayAdjacent),
                true,
            )
            .is_none()
        );

        // Packed end-to-end: clean None -> caller hides widget cleanly.
        let completely_full = chunk_items(0, 1695);
        assert!(
            solve_taskbar_placement(
                taskbar,
                Some(notify),
                Some(&completely_full),
                (118, 36),
                96,
                None,
                false,
            )
            .is_none()
        );

        // Unknown occupancy must not cover buttons, including before the first UIA scan.
        for empty_uia in [None, Some(&[][..])] {
            assert!(
                solve_taskbar_placement(
                    taskbar,
                    Some(notify),
                    empty_uia,
                    (150, 36),
                    96,
                    None,
                    false,
                )
                .is_none(),
                "unknown taskbar occupancy must keep the widget hidden"
            );
        }
    }

    #[test]
    fn placement_ignores_container_rects_and_handles_negative_rtl_vertical() {
        // Negative secondary/left monitor coords + bogus container-wide XAML bridge (>= 60%).
        let neg_bar = rect(-1920, 1032, 0, 1080);
        let neg_notify = rect(-220, 1032, 0, 1080);
        let uia_with_container = [
            rect(-1920, 1032, -220, 1080), // 1700/1920 = 88% bogus container bridge
            rect(-1920, 1032, -1600, 1080),
        ];
        let neg_res = solve_taskbar_placement(
            neg_bar,
            Some(neg_notify),
            Some(&uia_with_container),
            (150, 36),
            96,
            None,
            false,
        )
        .expect("bogus >=60% container rect must be ignored");
        assert_eq!(neg_res.pos, (-220 - 150 - 6, 1032 + (48 - 36) / 2));

        // RTL taskbar: tray on the left edge [0..220], apps on [900..1920].
        let rtl_bar = rect(0, 1032, 1920, 1080);
        let rtl_notify = rect(0, 1032, 220, 1080);
        let rtl_uia = [rect(900, 1032, 1920, 1080)];
        let rtl_res = solve_taskbar_placement(
            rtl_bar,
            Some(rtl_notify),
            Some(&rtl_uia),
            (150, 36),
            96,
            None,
            false,
        )
        .expect("RTL tray-adjacent gap should fit");
        assert_eq!(rtl_res.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(rtl_res.pos.0, 220 + 6);

        // Vertical taskbar on right edge [1840..1920] x [0..1080], tray at bottom [880..1080].
        let vert_bar = rect(1840, 0, 1920, 1080);
        let vert_notify = rect(1840, 880, 1920, 1080);
        let vert_uia = [rect(1840, 0, 1920, 420), rect(1840, 830, 1920, 880)];
        let vert_res = solve_taskbar_placement(
            vert_bar,
            Some(vert_notify),
            Some(&vert_uia),
            (72, 38),
            96,
            None,
            false,
        )
        .expect("vertical tray-adjacent gap above 830 should fit");
        assert_eq!(vert_res.slot, AnchorSlot::TrayAdjacent);
        assert_eq!(vert_res.pos.1, 830 - 38 - 6);
    }

    #[test]
    fn clamp_into_taskbar_shrinks_escapes_and_never_grows() {
        let bar = rect(-1_000, -10_000, -200, -9_952);

        // Already inside: untouched, and it fills the bar's cross axis.
        let inside = rect(-700, -10_000, -540, -9_952);
        assert_eq!(clamp_into_taskbar(inside, bar), (inside, true));

        // Every escaped edge (and a box escaping all four) lands on the bar's
        // edge without ever growing the rect.
        for (raw, expected) in [
            (
                rect(-1_010, -10_000, -540, -9_952),
                rect(-1_000, -10_000, -540, -9_952),
            ),
            (
                rect(-700, -10_000, -190, -9_952),
                rect(-700, -10_000, -200, -9_952),
            ),
            (
                rect(-700, -10_004, -540, -9_952),
                rect(-700, -10_000, -540, -9_952),
            ),
            (
                rect(-700, -10_000, -540, -9_940),
                rect(-700, -10_000, -540, -9_952),
            ),
            (
                rect(-1_010, -10_004, -190, -9_940),
                rect(-1_000, -10_000, -200, -9_952),
            ),
        ] {
            let (clamped, fills_cross) = clamp_into_taskbar(raw, bar);
            assert_eq!(clamped, expected, "{raw:?} clamped into {bar:?}");
            assert_eq!(
                fills_cross,
                clamped.height() == bar.height(),
                "{raw:?} cross-axis report"
            );
            assert!(bar.covers(clamped), "{raw:?} must stay inside {bar:?}");
            assert!(
                clamped.width() <= raw.width() && clamped.height() <= raw.height(),
                "{raw:?} must never be enlarged"
            );
        }

        // Inside but shorter than the bar: kept as is and reported (dead band),
        // which is what the window-side diagnostic keys on.
        let short = rect(-700, -10_000, -540, -9_968);
        assert_eq!(clamp_into_taskbar(short, bar), (short, false));

        // Vertical bar: the cross axis is the width.
        let vertical = rect(1_840, 0, 1_920, 1_080);
        let full_width = rect(1_840, 400, 1_920, 550);
        assert_eq!(clamp_into_taskbar(full_width, vertical), (full_width, true));
        let narrow = rect(1_840, 400, 1_904, 550);
        assert_eq!(clamp_into_taskbar(narrow, vertical), (narrow, false));

        // A degenerate bar has nothing to fit into: the rect is left alone.
        let empty = rect(0, 0, 0, 0);
        assert_eq!(clamp_into_taskbar(inside, empty), (inside, false));
    }

    #[test]
    fn placement_fills_the_taskbar_cross_axis_at_every_geometry() {
        #[derive(Clone, Copy, Debug)]
        enum Edge {
            Bottom,
            Top,
            Left,
            Right,
        }

        let mut checked = 0;
        for edge in [Edge::Bottom, Edge::Top, Edge::Left, Edge::Right] {
            let horizontal = matches!(edge, Edge::Bottom | Edge::Top);
            for (ox, oy) in [(0, 0), (-1920, 0)] {
                for dpi in [96, 120, 144, 168, 192, 216, 240] {
                    for cross in [24, 30, 40, 48, 60, 72, 75] {
                        for tray_at_end in [true, false] {
                            let (bar, notify) = if horizontal {
                                let (top, bottom) = if matches!(edge, Edge::Bottom) {
                                    (oy + 720 - cross, oy + 720)
                                } else {
                                    (oy, oy + cross)
                                };
                                let bar = rect(ox, top, ox + 1200, bottom);
                                let notify = if tray_at_end {
                                    rect(bar.right - 220, top, bar.right, bottom)
                                } else {
                                    rect(bar.left, top, bar.left + 220, bottom)
                                };
                                (bar, notify)
                            } else {
                                let (left, right) = if matches!(edge, Edge::Left) {
                                    (ox, ox + cross)
                                } else {
                                    (ox + 1200 - cross, ox + 1200)
                                };
                                let bar = rect(left, oy, right, oy + 720);
                                let notify = if tray_at_end {
                                    rect(left, bar.bottom - 220, right, bar.bottom)
                                } else {
                                    rect(left, bar.top, right, bar.top + 220)
                                };
                                (bar, notify)
                            };
                            let (widget_w, widget_h) = if horizontal {
                                (150, cross)
                            } else {
                                (cross, 150)
                            };
                            let uia = [notify];
                            let case = format!(
                                "{edge:?} cross={cross} dpi={dpi} origin=({ox},{oy}) \
                                 tray_at_end={tray_at_end}"
                            );

                            let Some(res) = solve_taskbar_placement(
                                bar,
                                Some(notify),
                                Some(&uia),
                                (widget_w, widget_h),
                                dpi,
                                None,
                                false,
                            ) else {
                                panic!("{case}: a widget equal to the bar cross axis must fit");
                            };
                            let (x, y) = res.pos;
                            let placed = rect(x, y, x + widget_w, y + widget_h);
                            let (cross_pos, cross_start) = if horizontal {
                                (y, bar.top)
                            } else {
                                (x, bar.left)
                            };
                            assert_eq!(
                                cross_pos, cross_start,
                                "{case}: the widget must start at the bar's cross edge, \
                                 not leave a dead band"
                            );
                            assert!(
                                bar.covers(placed),
                                "{case}: {placed:?} must stay inside {bar:?}"
                            );
                            assert!(
                                placed.width() <= bar.width() && placed.height() <= bar.height(),
                                "{case}: {placed:?} must not exceed {bar:?}"
                            );
                            assert!(
                                clamp_into_taskbar(placed, bar).1,
                                "{case}: {placed:?} must fill the bar's cross axis"
                            );

                            // One pixel over the bar's cross extent must hide
                            // instead of producing an out-of-bar rect.
                            let (over_w, over_h) = if horizontal {
                                (150, cross + 1)
                            } else {
                                (cross + 1, 150)
                            };
                            assert!(
                                solve_taskbar_placement(
                                    bar,
                                    Some(notify),
                                    Some(&uia),
                                    (over_w, over_h),
                                    dpi,
                                    None,
                                    false,
                                )
                                .is_none(),
                                "{case}: an oversized widget must hide, not escape the bar"
                            );
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 4 * 2 * 7 * 7 * 2, "the whole matrix must run");
    }

    #[test]
    fn context_menu_anchor_places_menu_outside_taskbar_edges() {
        let monitor = Some(rect(0, 0, 1920, 1080));

        let (pt, align) = context_menu_anchor(
            POINT { x: 1600, y: 1050 },
            Some(rect(0, 1032, 1920, 1080)),
            monitor,
        );
        assert_eq!((pt.x, pt.y, align), (1600, 1032, TPM_BOTTOMALIGN.0));

        let (pt, align) = context_menu_anchor(
            POINT { x: 1600, y: 20 },
            Some(rect(0, 0, 1920, 48)),
            monitor,
        );
        assert_eq!((pt.x, pt.y, align), (1600, 48, TPM_TOPALIGN.0));

        let (pt, align) =
            context_menu_anchor(POINT { x: 30, y: 900 }, Some(rect(0, 0, 64, 1080)), monitor);
        assert_eq!((pt.x, pt.y, align), (64, 900, TPM_LEFTALIGN.0));

        let (pt, align) = context_menu_anchor(
            POINT { x: 1880, y: 900 },
            Some(rect(1856, 0, 1920, 1080)),
            monitor,
        );
        assert_eq!((pt.x, pt.y, align), (1856, 900, TPM_RIGHTALIGN.0));
    }
}

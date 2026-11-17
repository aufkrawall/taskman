//! Elevated-band occlusion: scanning windows above the tray, classifying
//! occluders, and turning their rectangles into window-region cut-outs.

use super::PixelRect;
use super::fullscreen::fullscreen_above_result;
use super::hooks::{LIVE_HWND, LIVE_TRAY_HWND};
use super::identity::{
    ClassNameBuf, is_below_band, is_desktop_shell_class_buf,
    is_non_occluding_shell_host_with_class, is_taskbar_flyout_class, is_tooltip_class_buf,
    is_transparent_overlay, is_window_cloaked, visible_frame_rect,
};
use super::zorder::window_band;
use std::sync::Arc;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GWL_STYLE, GetDesktopWindow, GetShellWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, OBJID_WINDOW, WS_CHILD,
    WS_EX_TOPMOST,
};
use windows::core::BOOL;

/// How a partially occluded widget should react to a set of occluder rects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OcclusionOutcome {
    /// No occluder: any stale clip must be removed.
    Clear,
    /// Still partly visible: keep the window and cut out the covered strips.
    Clipped,
    /// Nothing left to present: fall back to hiding.
    Hidden,
}

/// Normalized, stable local clip rects used both as the region recipe and as
/// the change-detection key, so redundant `SetWindowRgn` calls are skipped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct AppliedClip {
    pub(super) size: (i32, i32),
    pub(super) local_rects: Vec<PixelRect>,
}

/// Sub-pixel jitter in an animating occluder's frame would otherwise reinstall
/// an identical window region on every refresh, forcing needless DWM
/// recomposition. Skip the region update while the difference stays tiny.
pub(super) const CLIP_TOLERANCE_PX: i32 = 2;

pub(super) fn applied_clip_equivalent(previous: &AppliedClip, next: &AppliedClip) -> bool {
    previous.size == next.size
        && previous.local_rects.len() == next.local_rects.len()
        && previous
            .local_rects
            .iter()
            .zip(&next.local_rects)
            .all(|(a, b)| {
                (a.left - b.left).abs() <= CLIP_TOLERANCE_PX
                    && (a.top - b.top).abs() <= CLIP_TOLERANCE_PX
                    && (a.right - b.right).abs() <= CLIP_TOLERANCE_PX
                    && (a.bottom - b.bottom).abs() <= CLIP_TOLERANCE_PX
            })
}

pub(super) fn local_clip_rects(target: PixelRect, occluder_rects: &[PixelRect]) -> Vec<PixelRect> {
    let mut rects: Vec<PixelRect> = occluder_rects
        .iter()
        .filter_map(|frame| target.local_intersection(*frame))
        .filter(|r| !r.is_empty())
        .collect();
    rects.sort_unstable_by_key(|r| (r.left, r.top, r.right, r.bottom));
    rects.dedup();
    rects
}

/// Exact rectangle subtraction: split each remaining piece around the clip.
pub(super) fn subtract_local_rect(pieces: &mut Vec<PixelRect>, clip: PixelRect) {
    let mut next = Vec::with_capacity(pieces.len().saturating_add(3));
    for piece in pieces.drain(..) {
        let left = piece.left.max(clip.left);
        let top = piece.top.max(clip.top);
        let right = piece.right.min(clip.right);
        let bottom = piece.bottom.min(clip.bottom);
        if right <= left || bottom <= top {
            next.push(piece);
            continue;
        }
        if piece.left < left {
            next.push(PixelRect {
                left: piece.left,
                top: piece.top,
                right: left,
                bottom: piece.bottom,
            });
        }
        if right < piece.right {
            next.push(PixelRect {
                left: right,
                top: piece.top,
                right: piece.right,
                bottom: piece.bottom,
            });
        }
        if piece.top < top {
            next.push(PixelRect {
                left,
                top: piece.top,
                right,
                bottom: top,
            });
        }
        if bottom < piece.bottom {
            next.push(PixelRect {
                left,
                top: bottom,
                right,
                bottom: piece.bottom,
            });
        }
    }
    *pieces = next;
}

pub(super) fn occlusion_outcome(size: (i32, i32), clips: &[PixelRect]) -> OcclusionOutcome {
    if clips.is_empty() || size.0 <= 0 || size.1 <= 0 {
        return OcclusionOutcome::Clear;
    }
    let mut remaining = vec![PixelRect {
        left: 0,
        top: 0,
        right: size.0,
        bottom: size.1,
    }];
    for clip in clips {
        subtract_local_rect(&mut remaining, *clip);
        if remaining.is_empty() {
            return OcclusionOutcome::Hidden;
        }
    }
    OcclusionOutcome::Clipped
}

/// Whether applying `check` at `target_rect` leaves the window without any
/// cut-out. Such a decision only drops a stale region and is applied after a
/// move/new frame; every other decision (new or larger cut-out, hide) is
/// applied before it, so the widget never paints over an occluder in between.
pub(super) fn occlusion_leaves_unclipped(
    target_rect: PixelRect,
    check: OcclusionCheck,
    occluder_rects: &[PixelRect],
) -> bool {
    match check {
        OcclusionCheck::Clear => true,
        OcclusionCheck::TrayMissing => false,
        OcclusionCheck::Occluded(_) => {
            occlusion_outcome(
                (target_rect.width(), target_rect.height()),
                &local_clip_rects(target_rect, occluder_rects),
            ) == OcclusionOutcome::Clear
        }
    }
}

/// Windows 11 taskbar flyouts (the right-click menu and tray overflow) render an
/// invisible XAML drop-shadow *inside* their HWND frame. Their frame can extend
/// below the visible menu into the taskbar even though no menu pixels are there.
/// Our widget is placed inside the taskbar, so only the part of such a popup
/// above the taskbar can actually cover it; the rest is shadow and must not be
/// subtracted from the widget region.
pub(super) fn occluder_frame_for_target(
    frame: PixelRect,
    is_taskbar_flyout: bool,
    taskbar_rect: Option<PixelRect>,
) -> PixelRect {
    if is_taskbar_flyout
        && let Some(taskbar) = taskbar_rect
        && !taskbar.is_empty()
    {
        PixelRect {
            bottom: frame.bottom.min(taskbar.top),
            ..frame
        }
    } else {
        frame
    }
}

pub(super) fn is_potential_occluder_window(
    hwnd: HWND,
    own_hwnd: HWND,
    desktop_hwnd: HWND,
    shell_hwnd: HWND,
) -> bool {
    if hwnd == own_hwnd || hwnd == desktop_hwnd || hwnd == shell_hwnd {
        return false;
    }
    if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
        return false;
    }
    if is_window_cloaked(hwnd) {
        return false;
    }
    let Some(cls) = ClassNameBuf::read(hwnd) else {
        return !is_transparent_overlay(hwnd);
    };
    if is_desktop_shell_class_buf(&cls) || is_non_occluding_shell_host_with_class(hwnd, &cls) {
        return false;
    }
    is_tooltip_class_buf(&cls) || !is_transparent_overlay(hwnd)
}

pub(super) fn is_topmost_or_elevated_window(hwnd: HWND) -> bool {
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    (ex_style & WS_EX_TOPMOST.0) != 0 || window_band(hwnd).is_some_and(|band| band > 1)
}

pub(super) fn is_topmost_location_candidate(hwnd: HWND, id_object: i32, id_child: i32) -> bool {
    if id_object != OBJID_WINDOW.0 || id_child != 0 || hwnd.0.is_null() {
        return false;
    }
    let raw = hwnd.0 as isize;
    if raw == LIVE_HWND.load(std::sync::atomic::Ordering::Acquire)
        || raw == LIVE_TRAY_HWND.load(std::sync::atomic::Ordering::Acquire)
    {
        return false;
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    if (style & WS_CHILD.0) != 0 {
        return false;
    }
    if !is_potential_occluder_window(
        hwnd,
        HWND::default(),
        unsafe { GetDesktopWindow() },
        unsafe { GetShellWindow() },
    ) {
        return false;
    }
    is_topmost_or_elevated_window(hwnd)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum OcclusionCheck {
    Clear,
    TrayMissing,
    Occluded(HWND),
}

/// Occlusion result together with the occluder frames collected by the *same*
/// enumeration. Reusing them avoids a second `EnumWindows` pass that could
/// observe a different window set and clip against a vanished occluder.
pub(super) type PrecheckedOcclusion = (PixelRect, OcclusionCheck, Arc<Vec<PixelRect>>);

pub(super) fn occlusion_check_with_rects(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    target_rect: PixelRect,
) -> (OcclusionCheck, Vec<PixelRect>) {
    let summary =
        scan_above_tray_with_collect(tray_hwnd, own_hwnd, None, Some(target_rect), None, true);
    (
        summary.occlusion.unwrap_or(OcclusionCheck::TrayMissing),
        summary.occluder_rects,
    )
}

pub(super) fn occlusion_result_after_enum(
    saw_tray: bool,
    tray_band: Option<u32>,
    occluder: Option<HWND>,
) -> OcclusionCheck {
    let tray_accounted = saw_tray || tray_band.is_some_and(|band| band > 1);
    if !tray_accounted {
        OcclusionCheck::TrayMissing
    } else if let Some(hwnd) = occluder {
        OcclusionCheck::Occluded(hwnd)
    } else {
        OcclusionCheck::Clear
    }
}

pub(super) struct AboveTraySummary {
    pub(super) topmost_tids: Vec<u32>,
    pub(super) fullscreen_coverer: Option<HWND>,
    pub(super) occlusion: Option<OcclusionCheck>,
    pub(super) occluder_rects: Vec<PixelRect>,
}

pub(super) struct AboveTrayScan<'a> {
    pub(super) tray_hwnd: HWND,
    pub(super) own_hwnd: HWND,
    pub(super) desktop_hwnd: HWND,
    pub(super) shell_hwnd: HWND,
    pub(super) tray_band: Option<u32>,
    pub(super) monitor_rect: Option<PixelRect>,
    pub(super) taskbar_rect: Option<PixelRect>,
    pub(super) probe_rect: Option<PixelRect>,
    pub(super) excluded_tids: Option<&'a [u32]>,
    pub(super) collect_rects: bool,
    pub(super) topmost_tids: Vec<u32>,
    pub(super) fullscreen_coverer: Option<HWND>,
    pub(super) occluder: Option<HWND>,
    pub(super) occluder_rects: Vec<PixelRect>,
    pub(super) saw_tray: bool,
}

pub(super) unsafe extern "system" fn above_tray_enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = unsafe { &mut *(lparam.0 as *mut AboveTrayScan<'_>) };
    if hwnd == scan.tray_hwnd {
        scan.saw_tray = true;
        return BOOL(0);
    }
    if hwnd == scan.own_hwnd || hwnd == scan.desktop_hwnd || hwnd == scan.shell_hwnd {
        return BOOL(1);
    }
    if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
        return BOOL(1);
    }
    if is_below_band(hwnd, scan.tray_band) || is_window_cloaked(hwnd) {
        return BOOL(1);
    }
    let cls = ClassNameBuf::read(hwnd);
    if let Some(ref c) = cls
        && (is_desktop_shell_class_buf(c) || is_non_occluding_shell_host_with_class(hwnd, c))
    {
        return BOOL(1);
    }
    let is_tooltip = cls.as_ref().is_some_and(is_tooltip_class_buf);
    let transparent = is_transparent_overlay(hwnd);

    if let Some(mon_rect) = scan.monitor_rect
        && scan.fullscreen_coverer.is_none()
        && !transparent
    {
        let mut r = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok()
            && PixelRect::from_win32(r).covers(mon_rect)
        {
            scan.fullscreen_coverer = Some(hwnd);
        }
    }

    if is_tooltip || !transparent {
        if let Some(excluded) = scan.excluded_tids
            && is_topmost_or_elevated_window(hwnd)
        {
            let tid = unsafe { GetWindowThreadProcessId(hwnd, None) };
            if tid != 0 && !excluded.contains(&tid) && !scan.topmost_tids.contains(&tid) {
                scan.topmost_tids.push(tid);
            }
        }
        if let Some(target_rect) = scan.probe_rect
            && !target_rect.is_empty()
            && (scan.collect_rects || scan.occluder.is_none())
            && let Some(frame) = visible_frame_rect(hwnd)
        {
            let frame = occluder_frame_for_target(
                frame,
                is_taskbar_flyout_class(cls.as_ref()),
                scan.taskbar_rect,
            );
            if !frame.is_empty() && frame.intersects(target_rect) {
                if scan.occluder.is_none() {
                    scan.occluder = Some(hwnd);
                }
                if scan.collect_rects && !scan.occluder_rects.contains(&frame) {
                    scan.occluder_rects.push(frame);
                }
            }
        }
    }
    BOOL(1)
}

pub(super) fn scan_above_tray_in_band(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    tray_band: Option<u32>,
    monitor_rect: Option<PixelRect>,
    probe_rect: Option<PixelRect>,
    excluded_tids: Option<&[u32]>,
    collect_rects: bool,
) -> (AboveTraySummary, bool) {
    if !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tray_hwnd)).as_bool() } {
        return (
            AboveTraySummary {
                topmost_tids: Vec::new(),
                fullscreen_coverer: None,
                occlusion: probe_rect.map(|_| OcclusionCheck::TrayMissing),
                occluder_rects: Vec::new(),
            },
            false,
        );
    }
    let taskbar_rect = {
        let mut r = RECT::default();
        unsafe { GetWindowRect(tray_hwnd, &mut r) }
            .ok()
            .map(|_| PixelRect::from_win32(r))
            .filter(|rect| !rect.is_empty())
    };
    let mut scan = AboveTrayScan {
        tray_hwnd,
        own_hwnd,
        desktop_hwnd: unsafe { GetDesktopWindow() },
        shell_hwnd: unsafe { GetShellWindow() },
        tray_band,
        monitor_rect,
        taskbar_rect,
        probe_rect,
        excluded_tids,
        collect_rects,
        topmost_tids: Vec::new(),
        fullscreen_coverer: None,
        occluder: None,
        occluder_rects: Vec::new(),
        saw_tray: false,
    };
    unsafe {
        let _ = EnumWindows(
            Some(above_tray_enum_cb),
            LPARAM(&mut scan as *mut AboveTrayScan<'_> as isize),
        );
    }
    let fullscreen_coverer = fullscreen_above_result(
        scan.saw_tray,
        scan.tray_band,
        scan.fullscreen_coverer.is_some(),
    )
    .then_some(scan.fullscreen_coverer)
    .flatten();
    let occlusion = probe_rect.map(|r| {
        if r.is_empty() {
            OcclusionCheck::TrayMissing
        } else {
            occlusion_result_after_enum(scan.saw_tray, scan.tray_band, scan.occluder)
        }
    });
    (
        AboveTraySummary {
            topmost_tids: scan.topmost_tids,
            fullscreen_coverer,
            occlusion,
            occluder_rects: scan.occluder_rects,
        },
        scan.saw_tray,
    )
}

pub(super) fn scan_above_tray(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    monitor_rect: Option<PixelRect>,
    probe_rect: Option<PixelRect>,
    excluded_tids: Option<&[u32]>,
) -> AboveTraySummary {
    // Any scan with a probe rect feeds the occlusion policy, which needs the
    // occluder frames from that exact enumeration to clip deterministically.
    scan_above_tray_with_collect(
        tray_hwnd,
        own_hwnd,
        monitor_rect,
        probe_rect,
        excluded_tids,
        probe_rect.is_some(),
    )
}

/// Same enumeration as [`scan_above_tray`], optionally gathering every
/// intersecting occluder frame so a partially covered widget can be clipped
/// instead of hidden. Rect collection is only enabled on that rare path.
pub(super) fn scan_above_tray_with_collect(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    monitor_rect: Option<PixelRect>,
    probe_rect: Option<PixelRect>,
    excluded_tids: Option<&[u32]>,
    collect_rects: bool,
) -> AboveTraySummary {
    let initial_band = window_band(tray_hwnd);
    let (summary, saw_tray) = scan_above_tray_in_band(
        tray_hwnd,
        own_hwnd,
        initial_band,
        monitor_rect,
        probe_rect,
        excluded_tids,
        collect_rects,
    );
    if !saw_tray {
        let current_band = window_band(tray_hwnd);
        if current_band != initial_band {
            return scan_above_tray_in_band(
                tray_hwnd,
                own_hwnd,
                current_band,
                monitor_rect,
                probe_rect,
                excluded_tids,
                collect_rects,
            )
            .0;
        }
    }
    summary
}

#[cfg(test)]
pub(super) fn check_rect_occluded_above_taskbar_in_band(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    target_rect: PixelRect,
    tray_band: Option<u32>,
) -> OcclusionCheck {
    scan_above_tray_in_band(
        tray_hwnd,
        own_hwnd,
        tray_band,
        None,
        Some(target_rect),
        None,
        false,
    )
    .0
    .occlusion
    .unwrap_or(OcclusionCheck::TrayMissing)
}

#[cfg(test)]
pub(super) fn check_rect_occluded_above_taskbar(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    target_rect: PixelRect,
) -> OcclusionCheck {
    scan_above_tray(tray_hwnd, own_hwnd, None, Some(target_rect), None)
        .occlusion
        .unwrap_or(OcclusionCheck::TrayMissing)
}

/// Detects visible, uncloaked, non-transparent windows above `tray_hwnd` (such as
/// popup menus, tooltips, or always-on-top apps) whose visible frame overlaps
/// `target_rect`. Used in elevated window band 16 so the widget never floats on
/// top of overlapping popups or always-on-top windows.
#[cfg(test)]
pub(super) fn is_rect_occluded_above_taskbar(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    target_rect: PixelRect,
) -> bool {
    check_rect_occluded_above_taskbar(tray_hwnd, own_hwnd, target_rect) != OcclusionCheck::Clear
}

#[cfg(test)]
pub(super) fn collect_topmost_occluder_tids(
    tray_hwnd: HWND,
    own_hwnd: HWND,
    excluded: &[u32],
) -> Vec<u32> {
    scan_above_tray(tray_hwnd, own_hwnd, None, None, Some(excluded)).topmost_tids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::taskbar_monitor::fullscreen::is_fullscreen_window_above_taskbar_in_band;
    use crate::win::taskbar_monitor::test_support::{
        DpiContextGuard, TestWindows, create_topmost_test_window, rect, z_order_test_lock,
    };
    use crate::win::taskbar_monitor::thread::register_window_class;
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOPMOST, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetWindowPos, ShowWindow,
    };

    #[test]
    fn tray_in_elevated_band_ignores_lower_band_occluders_and_omission_from_enum() {
        assert_eq!(
            occlusion_result_after_enum(false, Some(6), None),
            OcclusionCheck::Clear
        );
        assert_eq!(
            occlusion_result_after_enum(false, Some(1), None),
            OcclusionCheck::TrayMissing
        );
        assert_eq!(
            occlusion_result_after_enum(false, None, None),
            OcclusionCheck::TrayMissing
        );
        assert!(fullscreen_above_result(false, Some(6), true));
        assert!(!fullscreen_above_result(false, Some(1), true));

        let _zorder = z_order_test_lock();
        let _dpi = DpiContextGuard::per_monitor_v2();
        let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
        let class_name = register_window_class(hinstance).unwrap();
        let tray = create_topmost_test_window(hinstance, class_name);
        let lower_band_popup = create_topmost_test_window(hinstance, class_name);
        let _windows = TestWindows(vec![tray, lower_band_popup]);

        let mon = rect(-25_000, -25_000, -23_080, -23_920);
        let widget_rect = rect(-23_500, -23_968, -23_300, -23_920);
        unsafe {
            let _ = ShowWindow(tray, SW_SHOWNOACTIVATE);
            SetWindowPos(
                tray,
                Some(HWND_TOPMOST),
                -25_000,
                -23_968,
                1920,
                48,
                SWP_NOACTIVATE,
            )
            .unwrap();
            let _ = ShowWindow(lower_band_popup, SW_SHOWNOACTIVATE);
            SetWindowPos(
                lower_band_popup,
                Some(HWND_TOPMOST),
                mon.left,
                mon.top,
                mon.width(),
                mon.height(),
                SWP_NOACTIVATE,
            )
            .unwrap();
        }

        // With a Band-1 tray beneath it, the Band-1 fullscreen/overlapping window
        // acts as an occluder and a fullscreen coverer.
        assert_eq!(
            check_rect_occluded_above_taskbar_in_band(tray, HWND::default(), widget_rect, Some(1)),
            OcclusionCheck::Occluded(lower_band_popup)
        );
        assert!(is_fullscreen_window_above_taskbar_in_band(
            mon,
            tray,
            HWND::default(),
            Some(1),
        ));

        // When Windows promotes `Shell_TrayWnd` into an immersive band (band 6 > 1),
        // Band-1 windows sit beneath it and must never count as above-tray occluders.
        assert_eq!(
            check_rect_occluded_above_taskbar_in_band(tray, HWND::default(), widget_rect, Some(6)),
            OcclusionCheck::Clear
        );
        assert!(!is_fullscreen_window_above_taskbar_in_band(
            mon,
            tray,
            HWND::default(),
            Some(6),
        ));
    }

    #[test]
    fn taskbar_flyout_shadow_does_not_clip_the_widget() {
        assert!(is_taskbar_flyout_class(Some(&ClassNameBuf::from_ascii(
            "Xaml_WindowedPopupClass"
        ))));
        assert!(is_taskbar_flyout_class(Some(&ClassNameBuf::from_ascii(
            "TopLevelWindowForOverflowXamlIsland"
        ))));
        assert!(!is_taskbar_flyout_class(Some(&ClassNameBuf::from_ascii(
            "#32768"
        ))));
        assert!(!is_taskbar_flyout_class(None));

        let taskbar = rect(0, 1000, 1920, 1050);
        let widget = rect(1400, 1008, 1600, 1040);
        let menu_frame = rect(1380, 820, 1620, 1018); // 18 px of shadow inside the taskbar
        let clamped = occluder_frame_for_target(menu_frame, true, Some(taskbar));
        assert_eq!(clamped.bottom, taskbar.top);
        assert!(
            !clamped.intersects(widget),
            "invisible flyout shadow must not be subtracted from the widget region"
        );
        assert!(local_clip_rects(widget, &[clamped]).is_empty());

        // Ordinary occluders keep their full frame, and we cannot clamp without
        // a known taskbar geometry.
        assert_eq!(
            occluder_frame_for_target(menu_frame, false, Some(taskbar)),
            menu_frame
        );
        assert_eq!(
            occluder_frame_for_target(menu_frame, true, None),
            menu_frame
        );
    }

    #[test]
    fn local_clip_rects_intersect_translate_sort_and_dedup() {
        let target = rect(100, 200, 180, 240);
        let clips = local_clip_rects(
            target,
            &[
                rect(150, 210, 200, 230), // partial overlap -> local (50,10,80,30)
                rect(-20, 0, 110, 250),   // clipped to target's left edge
                rect(150, 210, 200, 230), // duplicate
                rect(500, 500, 600, 600), // disjoint: dropped
                rect(100, 200, 180, 205), // top strip
            ],
        );
        assert_eq!(
            clips,
            vec![rect(0, 0, 10, 40), rect(0, 0, 80, 5), rect(50, 10, 80, 30),],
            "clips must be local to the widget, bounded by it, stable and unique"
        );
        assert!(local_clip_rects(target, &[rect(500, 500, 600, 600)]).is_empty());
    }

    #[test]
    fn clip_key_tolerance_skips_jitter_but_not_real_moves() {
        let base = AppliedClip {
            size: (200, 40),
            local_rects: vec![rect(20, 0, 90, 40)],
        };
        let jitter = AppliedClip {
            size: (200, 40),
            local_rects: vec![rect(21, 0, 91, 40)],
        };
        let moved = AppliedClip {
            size: (200, 40),
            local_rects: vec![rect(30, 0, 100, 40)],
        };
        let resized = AppliedClip {
            size: (220, 40),
            local_rects: vec![rect(21, 0, 91, 40)],
        };
        assert!(applied_clip_equivalent(&base, &jitter));
        assert!(!applied_clip_equivalent(&base, &moved));
        assert!(!applied_clip_equivalent(&base, &resized));
    }

    #[test]
    fn occlusion_outcome_distinguishes_clear_clipped_and_hidden() {
        let size = (100, 40);
        assert_eq!(occlusion_outcome(size, &[]), OcclusionOutcome::Clear);
        assert_eq!(
            occlusion_outcome((0, 40), &[rect(0, 0, 10, 10)]),
            OcclusionOutcome::Clear
        );

        let partial = local_clip_rects(rect(0, 0, 100, 40), &[rect(20, 0, 60, 40)]);
        assert_eq!(occlusion_outcome(size, &partial), OcclusionOutcome::Clipped);

        // Two strips that only cover the widget when their union is considered.
        let halves = local_clip_rects(
            rect(0, 0, 100, 40),
            &[rect(-10, 0, 50, 40), rect(50, 0, 110, 40)],
        );
        assert_eq!(
            occlusion_outcome(size, &halves),
            OcclusionOutcome::Hidden,
            "a full union must hide even when no single occluder covers the widget"
        );

        // Adjacent strips leave a one-pixel gap: still clipped, not hidden.
        let gapped = local_clip_rects(
            rect(0, 0, 100, 40),
            &[rect(0, 0, 49, 40), rect(50, 0, 100, 40)],
        );
        assert_eq!(occlusion_outcome(size, &gapped), OcclusionOutcome::Clipped);
    }

    #[test]
    fn growing_cutouts_apply_before_present_and_vanishing_ones_after() {
        let target = rect(1000, 1040, 1100, 1080);
        // Any non-null handle: the occluder identity is not inspected here.
        let occluder = HWND(std::ptr::dangling_mut());
        assert!(occlusion_leaves_unclipped(
            target,
            OcclusionCheck::Clear,
            &[]
        ));
        assert!(!occlusion_leaves_unclipped(
            target,
            OcclusionCheck::TrayMissing,
            &[]
        ));
        // A reported occluder that no longer reaches the target only drops a
        // stale region: that happens after the frame/move.
        assert!(occlusion_leaves_unclipped(
            target,
            OcclusionCheck::Occluded(occluder),
            &[rect(0, 0, 500, 500)]
        ));
        // Partial cover (cut-out) and full cover (hide) happen before it.
        assert!(!occlusion_leaves_unclipped(
            target,
            OcclusionCheck::Occluded(occluder),
            &[rect(1050, 900, 1300, 1060)]
        ));
        assert!(!occlusion_leaves_unclipped(
            target,
            OcclusionCheck::Occluded(occluder),
            &[rect(900, 900, 1300, 1100)]
        ));
    }
}

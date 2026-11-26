//! Explicit visibility state of the widget window.
//!
//! The widget is either hidden for a [`HideReason`], on screen but
//! click-through while UIA occupancy is unconfirmed
//! ([`Visibility::Revalidating`]), or on screen and accepting input
//! ([`Visibility::Shown`]). Refresh code decides a target (a hide reason, an
//! [`OcclusionDecision`], or a show) and [`WindowCtx::apply_visibility`] is the
//! single place that applies a transition and its native side effects:
//! `ShowWindow`, releasing the clip region, the tracked occluder
//! (`LIVE_OCCLUDER_HWND`), the visibility notifier and the topmost recheck
//! timer.

use super::PixelRect;
use super::TOPMOST_RECHECK_MS;
use super::TOPMOST_RECHECK_TIMER_ID;
use super::hooks::LIVE_OCCLUDER_HWND;
use super::identity::{is_window_cloaked, read_window_class, visible_frame_rect};
use super::occlusion::{
    AppliedClip, OcclusionCheck, OcclusionOutcome, PrecheckedOcclusion, applied_clip_equivalent,
    local_clip_rects, occlusion_check_with_rects, occlusion_outcome, scan_above_tray,
};
use super::window::WindowCtx;
use super::zorder::{ensure_just_above_taskbar, window_band};
use std::sync::Arc;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, HGDIOBJ, RGN_DIFF, SetWindowRgn,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GWL_EXSTYLE, GetWindowLongPtrW, IsWindowVisible, KillTimer, SW_HIDE,
    SW_SHOWNOACTIVATE, SetTimer, SetWindowLongPtrW, ShowWindow, WS_EX_TRANSPARENT,
};
use windows::core::w;

/// Why the widget is off screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HideReason {
    /// Not shown yet, or a topmost recheck consumed the previous reason and
    /// the caller refreshes next.
    AwaitingRefresh,
    /// Disabled, or every section is turned off.
    Disabled,
    /// A fullscreen window covers the taskbar monitor.
    Fullscreen {
        suppressor: Option<HWND>,
    },
    /// `probe_taskbar_env` found no usable taskbar (`tray_unavailable`,
    /// `monitor_unavailable`, `tray_autohidden`).
    TaskbarUnavailable(&'static str),
    /// A peer or higher window fully covers the widget footprint `rect`
    /// (elevated band); `rect` is re-probed by the topmost recheck.
    Occluded {
        rect: PixelRect,
        occluder: HWND,
    },
    /// `Shell_TrayWnd` was missing from the band-16 enumeration, so occluders
    /// above it cannot be verified.
    TrayMissing,
    /// An occlusion check was requested without a footprint to check.
    MissingTargetRect,
    /// Band-1 fallback could not order the widget just above the taskbar.
    ZOrderFailed,
    /// The click-through style could not be applied.
    HitTestFailed,
    /// No current or held UIA occupancy for this taskbar.
    UiaUnavailable,
    NoRenderer,
    MeasureFailed,
    /// The taskbar is full even for the compact layout, or the solved box does
    /// not fill the bar's cross axis and would leave a strip of the bar taking
    /// the widget's clicks. The second case also logs its own expected/actual
    /// cross extent, which is what tells the two apart.
    NoPlacementSlot,
    PresentFailed,
    RenderFailed,
}

impl HideReason {
    /// Diagnostic name logged when a visible widget hides.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::AwaitingRefresh => "awaiting_refresh",
            Self::Disabled => "disabled",
            Self::Fullscreen { .. } => "fullscreen",
            Self::TaskbarUnavailable(reason) => reason,
            Self::Occluded { .. } => "occluded_above_tray",
            Self::TrayMissing => "tray_missing_in_enum",
            Self::MissingTargetRect => "missing_target_rect",
            Self::ZOrderFailed => "z_order_failed",
            Self::HitTestFailed => "hit_test_failed",
            Self::UiaUnavailable => "uia_unavailable",
            Self::NoRenderer => "no_renderer",
            Self::MeasureFailed => "measure_failed",
            Self::NoPlacementSlot => "no_placement_slot",
            Self::PresentFailed => "present_failed",
            Self::RenderFailed => "render_failed",
        }
    }

    /// Window whose class, band and frame are logged with the hide.
    fn detail(self) -> Option<HWND> {
        match self {
            Self::Fullscreen { suppressor } => suppressor,
            Self::Occluded { occluder, .. } => Some(occluder),
            _ => None,
        }
    }

    /// Transient environment conditions (fullscreen, a missing or covered
    /// taskbar, a failed Z-order repair) keep a confirmed layout so the widget
    /// can reappear from it while UIA revalidates; a failure of the widget's
    /// own pipeline discards it.
    pub(super) fn keeps_confirmed_layout(self) -> bool {
        matches!(
            self,
            Self::AwaitingRefresh
                | Self::Fullscreen { .. }
                | Self::TaskbarUnavailable(_)
                | Self::Occluded { .. }
                | Self::TrayMissing
                | Self::ZOrderFailed
        )
    }

    /// Hidden for these reasons, the elevated-band topmost recheck keeps
    /// polling for recovery.
    fn rechecks_topmost(self) -> bool {
        matches!(self, Self::Occluded { .. } | Self::TrayMissing)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Visibility {
    Hidden(HideReason),
    /// On screen with the last presented footprint, but clicks pass through
    /// (`WS_EX_TRANSPARENT`) until a current UIA scan confirms occupancy.
    Revalidating,
    /// On screen and accepting input.
    Shown,
}

/// Window region cutting the strips covered by a partial occluder out of the
/// widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ActiveClip {
    pub(super) key: AppliedClip,
    pub(super) occluder: HWND,
}

pub(super) struct WidgetState {
    pub(super) visibility: Visibility,
    /// The region currently installed on the window. It sits beside the
    /// phase because a cut-out is installed on a still-hidden window before it
    /// is shown; every hide releases it.
    pub(super) clip: Option<ActiveClip>,
    /// The last shown layout was confirmed by a current UIA scan and the
    /// taskbar geometry has not changed since, so a hidden widget may
    /// reappear from it (click-through) while UIA revalidates.
    pub(super) layout_confirmed: bool,
}

impl Default for WidgetState {
    fn default() -> Self {
        Self {
            visibility: Visibility::Hidden(HideReason::AwaitingRefresh),
            clip: None,
            layout_confirmed: false,
        }
    }
}

impl WidgetState {
    pub(super) fn is_visible(&self) -> bool {
        !matches!(self.visibility, Visibility::Hidden(_))
    }

    /// Footprint that was fully occluded when the widget hid for it.
    pub(super) fn occluded_rect(&self) -> Option<PixelRect> {
        match self.visibility {
            Visibility::Hidden(HideReason::Occluded { rect, .. }) => Some(rect),
            _ => None,
        }
    }

    /// The occluder WinEvent callbacks watch for (`LIVE_OCCLUDER_HWND`): the
    /// one we clip around, else the one that hid us.
    fn tracked_occluder(&self) -> HWND {
        match (&self.clip, self.visibility) {
            (Some(clip), _) => clip.occluder,
            (None, Visibility::Hidden(HideReason::Occluded { occluder, .. })) => occluder,
            _ => HWND::default(),
        }
    }
}

/// Outcome of an elevated-band occlusion check for a target footprint.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum OcclusionDecision {
    /// Nothing overlaps: drop any installed cut-out.
    Clear,
    /// Partial overlap: cut the covered strips out and stay visible.
    Clip(ActiveClip),
    Hide(HideReason),
}

pub(super) fn decide_occlusion(
    target_rect: PixelRect,
    check: OcclusionCheck,
    occluder_rects: &[PixelRect],
) -> OcclusionDecision {
    match check {
        OcclusionCheck::Clear => OcclusionDecision::Clear,
        OcclusionCheck::TrayMissing => OcclusionDecision::Hide(HideReason::TrayMissing),
        OcclusionCheck::Occluded(occluder) => {
            let key = AppliedClip {
                size: (target_rect.width(), target_rect.height()),
                local_rects: local_clip_rects(target_rect, occluder_rects),
            };
            match occlusion_outcome(key.size, &key.local_rects) {
                OcclusionOutcome::Hidden => OcclusionDecision::Hide(HideReason::Occluded {
                    rect: target_rect,
                    occluder,
                }),
                OcclusionOutcome::Clear => OcclusionDecision::Clear,
                OcclusionOutcome::Clipped => OcclusionDecision::Clip(ActiveClip { key, occluder }),
            }
        }
    }
}

impl WindowCtx {
    pub(super) fn is_visible(&self) -> bool {
        self.state.is_visible()
    }

    pub(super) fn hidden_by_occlusion(&self) -> bool {
        self.state.occluded_rect().is_some()
    }

    #[cfg(test)]
    pub(super) fn hidden_by_tray_missing(&self) -> bool {
        self.state.visibility == Visibility::Hidden(HideReason::TrayMissing)
    }

    fn wants_topmost_recheck(&self) -> bool {
        match self.state.visibility {
            Visibility::Hidden(reason) => reason.rechecks_topmost(),
            Visibility::Revalidating | Visibility::Shown => true,
        }
    }

    fn publish_occluder(&self) {
        LIVE_OCCLUDER_HWND.store(
            self.state.tracked_occluder().0 as isize,
            std::sync::atomic::Ordering::Release,
        );
    }

    pub(super) fn disarm_topmost_recheck(&mut self) {
        if self.topmost_recheck_armed && !self.hwnd.0.is_null() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TOPMOST_RECHECK_TIMER_ID);
            }
        }
        self.topmost_recheck_armed = false;
    }

    pub(super) fn sync_topmost_recheck(&mut self) {
        let should_arm = self.in_elevated_band && self.wants_topmost_recheck();
        if should_arm {
            if !self.topmost_recheck_armed && !self.hwnd.0.is_null() {
                unsafe {
                    let _ = SetTimer(
                        Some(self.hwnd),
                        TOPMOST_RECHECK_TIMER_ID,
                        TOPMOST_RECHECK_MS,
                        None,
                    );
                }
                self.topmost_recheck_armed = true;
            }
        } else {
            self.disarm_topmost_recheck();
        }
    }

    /// Returns `true` when the caller must run a full refresh to restore.
    pub(super) fn recheck_topmost_occlusion_with_tray(&mut self, tray_hwnd: HWND) -> bool {
        if !self.in_elevated_band || !self.wants_topmost_recheck() {
            self.disarm_topmost_recheck();
            return false;
        }
        let probe_rect = match self.state.visibility {
            Visibility::Hidden(HideReason::TrayMissing) => {
                // Only a full refresh can tell whether the tray is back; hand
                // the decision over without touching the window.
                self.state.visibility = Visibility::Hidden(HideReason::AwaitingRefresh);
                return true;
            }
            Visibility::Hidden(HideReason::Occluded { rect, .. }) => Some(rect),
            _ => self.last_rect(),
        };
        let excluded = self.excluded_topmost_tids();
        let summary = scan_above_tray(tray_hwnd, self.hwnd, None, probe_rect, Some(&excluded));
        self.sync_topmost_hooks_with_tids(&summary.topmost_tids);
        let occlusion = summary.occlusion;
        let occluder_rects = Arc::new(summary.occluder_rects);
        let prechecked = probe_rect
            .zip(occlusion)
            .map(|(rect, check)| (rect, check, occluder_rects));
        if self.is_visible() {
            let _ = self.hide_occluded_if_needed_cached(tray_hwnd, probe_rect, prechecked);
            return false;
        }
        self.hidden_by_occlusion() && summary.occlusion == Some(OcclusionCheck::Clear)
    }

    pub(super) fn recheck_topmost_occlusion(&mut self) {
        if !self.in_elevated_band || !self.wants_topmost_recheck() {
            self.disarm_topmost_recheck();
            return;
        }
        let tray_hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }.unwrap_or_default();
        if tray_hwnd.0.is_null()
            || unsafe { !IsWindowVisible(tray_hwnd).as_bool() }
            || is_window_cloaked(tray_hwnd)
        {
            self.refresh(false);
            return;
        }
        if self.recheck_topmost_occlusion_with_tray(tray_hwnd) {
            self.refresh(false);
        }
    }

    /// Drop any window region we installed and forget its occluder. Called when
    /// the occluder disappears, when we hide, and on teardown.
    pub(super) fn reset_clip(&mut self) {
        if self.state.clip.take().is_some() && !self.hwnd.0.is_null() {
            unsafe {
                // A null region removes the current one; the system releases it.
                let _ = SetWindowRgn(self.hwnd, None, true);
            }
        }
        self.publish_occluder();
    }

    /// Subtract the occluder strips from the layered window's region. A null
    /// result or a GDI failure means clipping is unusable and the caller must
    /// fall back to hiding, exactly as before this policy existed.
    pub(super) fn apply_window_clip(&self, key: &AppliedClip) -> bool {
        if self.hwnd.0.is_null() || key.size.0 <= 0 || key.size.1 <= 0 {
            return false;
        }
        unsafe {
            let visible = CreateRectRgn(0, 0, key.size.0, key.size.1);
            if visible.0.is_null() {
                return false;
            }
            for rect in &key.local_rects {
                let cut = CreateRectRgn(rect.left, rect.top, rect.right, rect.bottom);
                if cut.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(visible.0));
                    return false;
                }
                let combined = CombineRgn(Some(visible), Some(visible), Some(cut), RGN_DIFF);
                let _ = DeleteObject(HGDIOBJ(cut.0));
                if combined.0 == 0 {
                    // ERROR: the system region is untouched, release our copy.
                    let _ = DeleteObject(HGDIOBJ(visible.0));
                    return false;
                }
            }
            // On success the window owns the region; on failure it stays ours.
            if SetWindowRgn(self.hwnd, Some(visible), true) == 0 {
                let _ = DeleteObject(HGDIOBJ(visible.0));
                return false;
            }
        }
        true
    }

    /// Single decision point for all elevated-band occlusion checks. Partial
    /// overlap keeps the widget visible with a window region cutting out the
    /// covered strips; only a fully covered widget (or a GDI failure) hides.
    /// Returns `true` when the caller must stop refreshing because we hid.
    pub(super) fn apply_occlusion_policy(
        &mut self,
        target_rect: PixelRect,
        check: OcclusionCheck,
        occluder_rects: Vec<PixelRect>,
    ) -> bool {
        match decide_occlusion(target_rect, check, &occluder_rects) {
            OcclusionDecision::Clear => {
                self.reset_clip();
                false
            }
            OcclusionDecision::Hide(reason) => {
                self.hide(reason);
                true
            }
            OcclusionDecision::Clip(clip) => {
                let unchanged = self
                    .state
                    .clip
                    .as_ref()
                    .is_some_and(|previous| applied_clip_equivalent(&previous.key, &clip.key));
                if !unchanged && !self.apply_window_clip(&clip.key) {
                    self.hide(HideReason::Occluded {
                        rect: target_rect,
                        occluder: clip.occluder,
                    });
                    return true;
                }
                tracing::debug!(
                    occluder = ?clip.occluder,
                    strips = clip.key.local_rects.len(),
                    "clipping taskbar monitor around occluder"
                );
                self.state.clip = Some(clip);
                self.publish_occluder();
                false
            }
        }
    }

    pub(super) fn hide_occluded_if_needed_cached(
        &mut self,
        tray_hwnd: HWND,
        target_rect: Option<PixelRect>,
        prechecked: Option<PrecheckedOcclusion>,
    ) -> bool {
        if self.in_elevated_band {
            let Some(rect) = target_rect else {
                self.hide(HideReason::MissingTargetRect);
                return true;
            };
            let (check, rects) = match prechecked.as_ref() {
                Some((checked_rect, res, rects)) if *checked_rect == rect => {
                    (*res, rects.as_ref().clone())
                }
                _ => occlusion_check_with_rects(tray_hwnd, self.hwnd, rect),
            };
            self.apply_occlusion_policy(rect, check, rects)
        } else {
            self.reset_clip();
            if !ensure_just_above_taskbar(self.hwnd, tray_hwnd) {
                self.hide(HideReason::ZOrderFailed);
                true
            } else {
                false
            }
        }
    }

    pub(super) fn finish_show(&mut self, tray_hwnd: HWND, allow_input: bool) {
        let positioned = self.in_elevated_band || ensure_just_above_taskbar(self.hwnd, tray_hwnd);
        if !positioned {
            self.hide(HideReason::ZOrderFailed);
            return;
        }
        if !self.set_interactive(allow_input) {
            self.hide(HideReason::HitTestFailed);
            return;
        }
        self.show();
        if allow_input && self.is_visible() {
            self.state.layout_confirmed = true;
        }
    }

    /// Applies click-through (`interactive == false`) or input; a visible
    /// widget's phase follows the applied style.
    pub(super) fn set_interactive(&mut self, interactive: bool) -> bool {
        let style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) };
        let next = if interactive {
            style & !(WS_EX_TRANSPARENT.0 as isize)
        } else {
            style | WS_EX_TRANSPARENT.0 as isize
        };
        if style != next {
            unsafe { SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, next) };
        }
        let applied = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } == next;
        if !applied && self.is_visible() {
            tracing::warn!("cannot update taskbar monitor hit testing; keeping it hidden");
        }
        if applied && self.is_visible() {
            self.state.visibility = if interactive {
                Visibility::Shown
            } else {
                Visibility::Revalidating
            };
        }
        applied
    }

    fn native_click_through(&self) -> bool {
        let ex_style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } as u32;
        ex_style & WS_EX_TRANSPARENT.0 != 0
    }

    pub(super) fn accepts_input(&self) -> bool {
        self.state.visibility == Visibility::Shown && !self.native_click_through()
    }

    pub(super) fn hide(&mut self, reason: HideReason) {
        self.apply_visibility(Visibility::Hidden(reason));
    }

    /// Shows the window in the phase matching its current hit-test style.
    /// Returns whether it is visible.
    pub(super) fn show(&mut self) -> bool {
        let target = if self.native_click_through() {
            Visibility::Revalidating
        } else {
            Visibility::Shown
        };
        self.apply_visibility(target)
    }

    /// Applies a visibility transition. Returns whether the window is visible
    /// afterwards; a native show/hide that does not take effect leaves the
    /// phase mirroring what the window actually shows.
    fn apply_visibility(&mut self, target: Visibility) -> bool {
        let was_visible = self.is_visible();
        let previous = self.state.visibility;
        let visible = !matches!(target, Visibility::Hidden(_));
        if let Visibility::Hidden(reason) = target {
            if was_visible && tracing::enabled!(tracing::Level::DEBUG) {
                let detail = reason.detail();
                let class = detail.and_then(read_window_class);
                let band = detail.and_then(window_band);
                let frame = detail.and_then(visible_frame_rect);
                tracing::debug!(
                    reason = reason.name(),
                    ?class,
                    ?band,
                    ?frame,
                    "hiding taskbar monitor"
                );
            }
            // Hiding is mutually exclusive with presenting a clipped region: the
            // system owns the old HRGN and must get it back before SW_HIDE.
            self.reset_clip();
            if !reason.keeps_confirmed_layout() {
                self.state.layout_confirmed = false;
            }
            self.hovered = false;
            self.tracking_mouse = false;
        }
        self.state.visibility = target;
        // A clipped window still tracks the occluder it is clipped around and
        // an occluded one the occluder that hid it.
        self.publish_occluder();
        // The stacking-only WinEvent path can hide a popup whose safe ordering
        // failed. Reconcile native state instead of trusting our last refresh.
        let native_visible = unsafe { IsWindowVisible(self.hwnd).as_bool() };
        if was_visible == visible && native_visible == visible {
            self.sync_topmost_recheck();
            return visible;
        }
        if native_visible != visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
        let now_visible = unsafe { IsWindowVisible(self.hwnd).as_bool() };
        if now_visible != visible {
            self.state.visibility = match previous {
                _ if !now_visible => Visibility::Hidden(HideReason::AwaitingRefresh),
                Visibility::Revalidating | Visibility::Shown => previous,
                Visibility::Hidden(_) if self.native_click_through() => Visibility::Revalidating,
                Visibility::Hidden(_) => Visibility::Shown,
            };
        }
        if now_visible != was_visible {
            tracing::debug!(visible = now_visible, "taskbar monitor visibility changed");
        }
        self.visibility.set(now_visible);
        self.sync_topmost_recheck();
        now_visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::taskbar_monitor::test_support::rect;

    #[test]
    fn hide_reasons_keep_their_names_and_confirmed_layout_policy() {
        let occluder = HWND(std::ptr::dangling_mut());
        let kept = [
            (HideReason::Fullscreen { suppressor: None }, "fullscreen"),
            (
                HideReason::TaskbarUnavailable("tray_autohidden"),
                "tray_autohidden",
            ),
            (
                HideReason::Occluded {
                    rect: rect(0, 0, 10, 10),
                    occluder,
                },
                "occluded_above_tray",
            ),
            (HideReason::TrayMissing, "tray_missing_in_enum"),
            (HideReason::ZOrderFailed, "z_order_failed"),
        ];
        let discarded = [
            (HideReason::Disabled, "disabled"),
            (HideReason::MissingTargetRect, "missing_target_rect"),
            (HideReason::HitTestFailed, "hit_test_failed"),
            (HideReason::UiaUnavailable, "uia_unavailable"),
            (HideReason::NoRenderer, "no_renderer"),
            (HideReason::MeasureFailed, "measure_failed"),
            (HideReason::NoPlacementSlot, "no_placement_slot"),
            (HideReason::PresentFailed, "present_failed"),
            (HideReason::RenderFailed, "render_failed"),
        ];
        for (reason, name) in kept {
            assert_eq!(reason.name(), name);
            assert!(reason.keeps_confirmed_layout(), "{name}");
        }
        for (reason, name) in discarded {
            assert_eq!(reason.name(), name);
            assert!(!reason.keeps_confirmed_layout(), "{name}");
        }
        assert!(HideReason::TrayMissing.rechecks_topmost());
        assert!(!HideReason::Fullscreen { suppressor: None }.rechecks_topmost());
    }

    #[test]
    fn occlusion_decision_hides_full_cover_and_clips_partial_cover() {
        let target = rect(1000, 1040, 1100, 1080);
        let occluder = HWND(std::ptr::dangling_mut());
        assert_eq!(
            decide_occlusion(target, OcclusionCheck::Clear, &[]),
            OcclusionDecision::Clear
        );
        assert_eq!(
            decide_occlusion(target, OcclusionCheck::TrayMissing, &[]),
            OcclusionDecision::Hide(HideReason::TrayMissing)
        );
        assert_eq!(
            decide_occlusion(
                target,
                OcclusionCheck::Occluded(occluder),
                &[rect(0, 0, 500, 500)]
            ),
            OcclusionDecision::Clear,
            "a reported occluder that misses the target only drops a stale cut-out"
        );
        assert_eq!(
            decide_occlusion(
                target,
                OcclusionCheck::Occluded(occluder),
                &[rect(900, 900, 1300, 1100)]
            ),
            OcclusionDecision::Hide(HideReason::Occluded {
                rect: target,
                occluder
            })
        );
        let OcclusionDecision::Clip(clip) = decide_occlusion(
            target,
            OcclusionCheck::Occluded(occluder),
            &[rect(1050, 900, 1300, 1060)],
        ) else {
            panic!("partial cover must clip");
        };
        assert_eq!(clip.occluder, occluder);
        assert_eq!(clip.key.size, (100, 40));
    }

    #[test]
    fn tracked_occluder_prefers_the_clip_then_the_occlusion_that_hid_us() {
        let clipped_by = HWND(std::ptr::dangling_mut::<u8>().wrapping_add(1).cast());
        let hidden_by = HWND(std::ptr::dangling_mut::<u8>().wrapping_add(2).cast());
        let mut state = WidgetState::default();
        assert!(!state.is_visible());
        assert_eq!(state.tracked_occluder(), HWND::default());
        state.visibility = Visibility::Hidden(HideReason::Occluded {
            rect: rect(0, 0, 10, 10),
            occluder: hidden_by,
        });
        assert_eq!(state.tracked_occluder(), hidden_by);
        assert_eq!(state.occluded_rect(), Some(rect(0, 0, 10, 10)));
        state.clip = Some(ActiveClip {
            key: AppliedClip {
                size: (10, 10),
                local_rects: vec![rect(0, 0, 5, 10)],
            },
            occluder: clipped_by,
        });
        assert_eq!(state.tracked_occluder(), clipped_by);
        state.visibility = Visibility::Revalidating;
        state.clip = None;
        assert!(state.is_visible());
        assert_eq!(state.occluded_rect(), None);
        assert_eq!(state.tracked_occluder(), HWND::default());
    }
}

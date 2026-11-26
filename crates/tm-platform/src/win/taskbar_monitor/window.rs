//! Per-window widget state (`WindowCtx`) and the refresh pipeline that
//! measures, places, renders, clips and shows the widget.

use super::fullscreen::{TaskbarEnv, TaskbarEnvProbe, probe_taskbar_env};
use super::hooks::{FULLSCREEN_WAKE_PENDING, LayoutUpdate};
#[cfg(test)]
use super::occlusion::is_rect_occluded_above_taskbar;
use super::occlusion::{
    PrecheckedOcclusion, occlusion_check_with_rects, occlusion_leaves_unclipped,
};
use super::placement::{AnchorSlot, clamp_into_taskbar, solve_taskbar_placement};
use super::render::{LayoutBounds, RenderedDisplay, Renderer, build_display};
use super::uia::UiaScanner;
use super::visibility::{HideReason, WidgetState};
#[cfg(test)]
use super::zorder::ensure_just_above_taskbar;
use super::{
    FULLSCREEN_EXIT_MAX_RETRIES, FULLSCREEN_EXIT_RECHECK_MS, FULLSCREEN_EXIT_TIMER_ID,
    MonitorVisibility, PixelRect, SharedState, UIA_SETTLE_DELAY_MS, UIA_SETTLE_TIMER_ID,
};
use crate::taskbar_monitor::{Action, Config, Metrics};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, HDC};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetCursorPos, GetForegroundWindow, GetWindowRect, KillTimer, SWP_NOACTIVATE,
    SWP_NOOWNERZORDER, SWP_NOSENDCHANGING, SWP_NOZORDER, SetTimer, SetWindowPos, ULW_ALPHA,
    UpdateLayeredWindow,
};
use windows::core::w;

pub(super) struct WindowCtx {
    pub(super) hwnd: HWND,
    pub(super) in_elevated_band: bool,
    pub(super) user_closing: bool,
    pub(super) shared: Arc<Mutex<SharedState>>,
    pub(super) visibility: Arc<MonitorVisibility>,
    pub(super) callback: Arc<dyn Fn(Action) + Send + Sync>,
    pub(super) renderer: Option<Renderer>,
    pub(super) uia_scanner: Arc<UiaScanner>,
    pub(super) light_taskbar: bool,
    pub(super) hovered: bool,
    pub(super) tracking_mouse: bool,
    pub(super) last_display: Option<RenderedDisplay>,
    pub(super) last_size: (i32, i32),
    pub(super) last_pos: Option<(i32, i32)>,
    pub(super) last_present_error: Option<Instant>,
    /// Last refused-box diagnostic (see `diagnose_refused_box`).
    pub(super) last_cross_mismatch: Option<Instant>,
    pub(super) last_anchor_slot: Option<AnchorSlot>,
    pub(super) last_compact: bool,
    pub(super) last_stacked: bool,
    pub(super) last_env_geom: Option<(isize, PixelRect, Option<PixelRect>, u32)>,
    pub(super) pending_uia_scan: bool,
    /// The pending scan came from an event-driven recheck and may wait for
    /// the notification burst to settle (see `UiaScanner::request_debounced_recheck`).
    pub(super) debounce_uia_scan: bool,
    pub(super) invalidate_uia_layout: bool,
    pub(super) tracked_fg_tid: u32,
    pub(super) fg_loc_hook: HWINEVENTHOOK,
    pub(super) tracked_fs_tid: u32,
    pub(super) fs_loc_hook: HWINEVENTHOOK,
    pub(super) fs_lifecycle_hook: HWINEVENTHOOK,
    pub(super) fs_cloak_hook: HWINEVENTHOOK,
    pub(super) tracked_tray_tid: u32,
    pub(super) tray_hook: HWINEVENTHOOK,
    pub(super) topmost_hooks: Vec<(u32, HWINEVENTHOOK)>,
    pub(super) last_fg_hwnd: isize,
    pub(super) fullscreen_recheck_remaining: u8,
    pub(super) fullscreen_recheck_armed: bool,
    /// Visibility phase, installed clip and confirmed-layout flag; changed
    /// only through the transitions in `visibility.rs`.
    pub(super) state: WidgetState,
    pub(super) topmost_recheck_armed: bool,
}

impl Drop for WindowCtx {
    fn drop(&mut self) {
        self.visibility.set(false);
        self.unhook_dynamic_hooks();
    }
}

impl WindowCtx {
    pub(super) fn request_layout_scan(&mut self, update: LayoutUpdate, arm_settle: bool) {
        if update == LayoutUpdate::Recheck && arm_settle {
            // Event-driven recheck: the next refresh marks the layout
            // unconfirmed (click-through) but the worker scans once the burst
            // settles. That trailing scan replaces the settle-timer rescan.
            // Any immediate request queued meanwhile still wins.
            self.debounce_uia_scan = !self.pending_uia_scan || self.debounce_uia_scan;
            self.pending_uia_scan = true;
            return;
        }
        self.pending_uia_scan = true;
        self.debounce_uia_scan = false;
        if update == LayoutUpdate::Invalidate {
            self.invalidate_uia_layout = true;
            self.state.layout_confirmed = false;
            self.last_display = None;
            self.last_anchor_slot = None;
            self.last_compact = false;
        }
        if arm_settle && !self.hwnd.0.is_null() {
            unsafe {
                let _ = SetTimer(
                    Some(self.hwnd),
                    UIA_SETTLE_TIMER_ID,
                    UIA_SETTLE_DELAY_MS,
                    None,
                );
            }
        }
    }

    pub(super) fn disarm_fullscreen_exit_recheck(&mut self) {
        if self.fullscreen_recheck_armed && !self.hwnd.0.is_null() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), FULLSCREEN_EXIT_TIMER_ID);
            }
        }
        self.fullscreen_recheck_armed = false;
        self.fullscreen_recheck_remaining = 0;
    }

    pub(super) fn excluded_topmost_tids(&self) -> [u32; 3] {
        [
            unsafe { GetCurrentThreadId() },
            self.tracked_fg_tid,
            self.tracked_tray_tid,
        ]
    }

    pub(super) fn refresh(&mut self, force_render: bool) {
        let wake_recheck = FULLSCREEN_WAKE_PENDING.swap(false, std::sync::atomic::Ordering::AcqRel);
        let (config, metrics) = {
            let guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (guard.config.clone(), guard.metrics)
        };

        if !config.enabled || (!config.show_cpu && !config.show_ram && !config.show_network) {
            self.sync_fullscreen_hook(HWND::default());
            self.disarm_fullscreen_exit_recheck();
            self.sync_topmost_hooks_with_tids(&[]);
            self.hide(HideReason::Disabled);
            return;
        }

        // Track the active foreground window's thread even when hidden during
        // fullscreen so exiting fullscreen (F11 / Esc / resize) wakes immediately.
        self.sync_foreground_hook();
        let tray_hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }.unwrap_or_default();
        self.sync_tray_hook(tray_hwnd);

        let fg_raw = unsafe { GetForegroundWindow() }.0 as isize;
        let fg_changed = fg_raw != self.last_fg_hwnd;
        self.last_fg_hwnd = fg_raw;

        let occlusion_probe = if !self.in_elevated_band {
            None
        } else if self.is_visible() {
            self.last_rect()
        } else {
            self.state.occluded_rect()
        };
        let excluded_tids = self.excluded_topmost_tids();

        let (env, prechecked_occlusion, topmost_tids) = match probe_taskbar_env(
            tray_hwnd,
            self.hwnd,
            occlusion_probe,
            self.in_elevated_band.then_some(&excluded_tids[..]),
        ) {
            TaskbarEnvProbe::Ready {
                env,
                prechecked_occlusion,
                topmost_tids,
            } => {
                self.sync_fullscreen_hook(HWND::default());
                self.disarm_fullscreen_exit_recheck();
                (env, prechecked_occlusion, topmost_tids)
            }
            TaskbarEnvProbe::Fullscreen {
                suppressor,
                fg_covers_monitor,
            } => {
                self.sync_topmost_hooks_with_tids(&[]);
                self.sync_fullscreen_hook(suppressor.unwrap_or_default());
                if fg_covers_monitor {
                    self.disarm_fullscreen_exit_recheck();
                } else {
                    if wake_recheck || fg_changed {
                        self.fullscreen_recheck_remaining = FULLSCREEN_EXIT_MAX_RETRIES;
                    }
                    if self.fullscreen_recheck_remaining > 0 && !self.hwnd.0.is_null() {
                        unsafe {
                            let _ = SetTimer(
                                Some(self.hwnd),
                                FULLSCREEN_EXIT_TIMER_ID,
                                FULLSCREEN_EXIT_RECHECK_MS,
                                None,
                            );
                        }
                        self.fullscreen_recheck_armed = true;
                    } else {
                        self.disarm_fullscreen_exit_recheck();
                    }
                }
                // Preserve `last_env_geom` and the confirmed layout across
                // transient fullscreen suppression so returning to an unchanged
                // taskbar can reappear immediately while UIA revalidates.
                // No scan runs while suppressed; the one on return is immediate.
                self.pending_uia_scan = true;
                self.debounce_uia_scan = false;
                self.hide(HideReason::Fullscreen { suppressor });
                return;
            }
            TaskbarEnvProbe::Unavailable(reason) => {
                self.sync_topmost_hooks_with_tids(&[]);
                self.sync_fullscreen_hook(HWND::default());
                self.disarm_fullscreen_exit_recheck();
                self.pending_uia_scan = true;
                self.debounce_uia_scan = false;
                self.hide(HideReason::TaskbarUnavailable(reason));
                return;
            }
        };

        self.sync_topmost_hooks_with_tids(&topmost_tids);
        self.refresh_taskbar_env_with_occlusion(
            &config,
            &metrics,
            env,
            force_render,
            prechecked_occlusion,
        );
    }

    pub(super) fn last_rect(&self) -> Option<PixelRect> {
        let (x, y) = self.last_pos?;
        let (w, h) = self.last_size;
        let r = PixelRect {
            left: x,
            top: y,
            right: x.saturating_add(w),
            bottom: y.saturating_add(h),
        };
        (!r.is_empty()).then_some(r)
    }

    /// Belt and braces for the placement contract: the box handed to
    /// `SetWindowPos`, `UpdateLayeredWindow` and the occlusion check is checked
    /// against the taskbar here as well, so no code path can place or present
    /// the widget outside the bar. It returns the rect unchanged when it may be
    /// shown — a rect that would have to be moved or cut to stay inside the bar
    /// is refused, never silently adjusted — and `None` means it may not be
    /// shown at all.
    ///
    /// The cross axis must be filled, not merely respected. The box this gate
    /// replaced was a few pixels short, and that dead band is exactly where a
    /// click reached the taskbar instead of the widget, so a box that leaves
    /// such a band is refused on the same terms. `solve_taskbar_placement`
    /// already refuses it, which makes this branch defence in depth: it can
    /// only fire if a caller hands this gate a footprint that did not come from
    /// the solver, and it is diagnosed once per 30 s so that it is not silent.
    fn fit_target_rect(&mut self, rect: PixelRect, bar: PixelRect) -> Option<PixelRect> {
        let (fitted, fills_cross) = clamp_into_taskbar(rect, bar);
        if fitted != rect {
            self.diagnose_refused_box(
                fitted,
                bar,
                "taskbar monitor box would be cut to stay inside the taskbar; staying hidden",
            );
            return None;
        }
        if !fills_cross {
            self.diagnose_refused_box(
                fitted,
                bar,
                "taskbar monitor box does not fill the taskbar cross axis; staying hidden",
            );
            return None;
        }
        Some(fitted)
    }

    /// Rate-limited (once per 30 s) diagnosis for a box the gate refuses, so
    /// neither refusal reason is silent.
    fn diagnose_refused_box(&mut self, rect: PixelRect, bar: PixelRect, reason: &str) {
        if self
            .last_cross_mismatch
            .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return;
        }
        let (expected, actual) = if bar.is_horizontal() {
            (bar.height(), rect.height())
        } else {
            (bar.width(), rect.width())
        };
        tracing::warn!(
            expected,
            actual,
            left = rect.left,
            top = rect.top,
            ?bar,
            "{reason}"
        );
        self.last_cross_mismatch = Some(Instant::now());
    }

    #[cfg(test)]
    pub(super) fn ensure_visible_order(
        &self,
        tray_hwnd: HWND,
        target_rect: Option<PixelRect>,
    ) -> bool {
        if self.in_elevated_band {
            let Some(rect) = target_rect else {
                return false;
            };
            !is_rect_occluded_above_taskbar(tray_hwnd, self.hwnd, rect)
        } else {
            ensure_just_above_taskbar(self.hwnd, tray_hwnd)
        }
    }

    #[cfg(test)]
    pub(super) fn refresh_taskbar_env(
        &mut self,
        config: &Config,
        metrics: &Metrics,
        env: TaskbarEnv,
        force_render: bool,
    ) {
        self.refresh_taskbar_env_with_occlusion(config, metrics, env, force_render, None);
    }

    pub(super) fn refresh_taskbar_env_with_occlusion(
        &mut self,
        config: &Config,
        metrics: &Metrics,
        env: TaskbarEnv,
        force_render: bool,
        prechecked_occlusion: Option<PrecheckedOcclusion>,
    ) {
        let tray_raw = env.tray_hwnd.0 as isize;
        let prev_geom = self.last_env_geom;
        let geom = (tray_raw, env.taskbar_rect, env.tray_notify_rect, env.dpi);
        let core_geom_changed = prev_geom
            .is_none_or(|(t, bar, _, dpi)| (t, bar, dpi) != (tray_raw, env.taskbar_rect, env.dpi));
        let notify_geom_changed = !core_geom_changed
            && prev_geom.is_some_and(|(_, _, notify, _)| notify != env.tray_notify_rect);
        if core_geom_changed {
            self.last_env_geom = Some(geom);
            self.pending_uia_scan = true;
            self.debounce_uia_scan = false;
            self.invalidate_uia_layout = true;
            self.state.layout_confirmed = false;
            self.last_display = None;
            self.last_anchor_slot = None;
            self.last_compact = false;
            // A new bar geometry or DPI is a new chance to diagnose a real
            // mismatch instead of inheriting the previous one's 30 s window.
            // Ordinary revalidation traffic deliberately does not reset it, so
            // a shell event burst cannot turn the tripwire into a log storm.
            self.last_cross_mismatch = None;
        } else if notify_geom_changed {
            self.last_env_geom = Some(geom);
            self.request_layout_scan(LayoutUpdate::Recheck, true);
        }

        if self.pending_uia_scan {
            self.pending_uia_scan = false;
            let invalidate = std::mem::take(&mut self.invalidate_uia_layout);
            if std::mem::take(&mut self.debounce_uia_scan) && !invalidate {
                self.uia_scanner
                    .request_debounced_recheck(tray_raw, env.taskbar_rect);
            } else {
                self.uia_scanner
                    .request_scan(tray_raw, env.taskbar_rect, invalidate);
            }
        }
        let Some(uia_occupied) = self.uia_scanner.snapshot_for(tray_raw, env.taskbar_rect) else {
            self.uia_scanner
                .request_scan(tray_raw, env.taskbar_rect, false);
            self.hide(HideReason::UiaUnavailable);
            return;
        };

        if !uia_occupied.current {
            if self.is_visible() && !notify_geom_changed {
                // A layout hint is not evidence that our pixels collide. Keep
                // the already presented footprint stable while scanning, but
                // never intercept a control that may have moved underneath it.
                if !self.set_interactive(false) {
                    self.hide(HideReason::HitTestFailed);
                    return;
                }
                if !self.hide_occluded_if_needed_cached(
                    env.tray_hwnd,
                    self.last_rect(),
                    prechecked_occlusion,
                ) {
                    // Values keep updating while occupancy is unconfirmed; only
                    // the footprint is frozen.
                    self.rerender_in_place(config, metrics, &env, force_render);
                }
                return;
            }
            if !self.is_visible() && !self.state.layout_confirmed {
                // Hidden windows without a previously confirmed, geometry-matched
                // layout (e.g. startup or after `no_placement_slot`) must wait
                // for fresh UIA validation before appearing.
                if !self.set_interactive(false) {
                    self.hide(HideReason::HitTestFailed);
                }
                return;
            }
        }

        // Repair a taskbar raise before DirectWrite measurement/rasterization.
        // Only reuse our position while fresh geometry and known controls still
        // leave that rectangle clear; otherwise the placement solver runs first.
        if self.is_visible()
            && self.last_display.is_some()
            && !core_geom_changed
            && !notify_geom_changed
            && self.last_pos.is_some_and(|(x, y)| {
                let current = PixelRect {
                    left: x,
                    top: y,
                    right: x.saturating_add(self.last_size.0),
                    bottom: y.saturating_add(self.last_size.1),
                };
                uia_occupied.rects.iter().all(|r| !r.intersects(current))
            })
            && self.hide_occluded_if_needed_cached(
                env.tray_hwnd,
                self.last_rect(),
                prechecked_occlusion.clone(),
            )
        {
            return;
        }

        if self.hovered && uia_occupied.current && !cursor_inside_window(self.hwnd) {
            self.hovered = false;
            self.tracking_mouse = false;
        }

        let bounds = LayoutBounds::from_taskbar(env.taskbar_rect, env.dpi);
        let normal_display = build_display(
            config,
            metrics,
            self.light_taskbar,
            self.hovered,
            env.dpi,
            false,
        );
        let Some(renderer) = self.renderer.as_mut() else {
            self.hide(HideReason::NoRenderer);
            return;
        };
        if force_render {
            renderer.invalidate_rendering_params();
        }

        // Fast path: when display content, DPI, and taskbar bounds are unchanged
        // in normal (non-compact) mode, reuse the known pixel footprint without
        // recreating DirectWrite layouts.
        let cached_normal_placement = if !self.last_compact
            && !force_render
            && !core_geom_changed
            && self.last_display.as_ref() == Some(&normal_display)
            && self.last_size.0 > 0
            && self.last_size.1 > 0
        {
            solve_taskbar_placement(
                env.taskbar_rect,
                env.tray_notify_rect,
                Some(&uia_occupied.rects),
                self.last_size,
                env.dpi,
                self.last_anchor_slot,
                false,
            )
        } else {
            None
        };

        if let Some(placement) = cached_normal_placement {
            let (w, h) = self.last_size;
            let (x, y) = placement.pos;
            let target_rect = PixelRect {
                left: x,
                top: y,
                right: x.saturating_add(w),
                bottom: y.saturating_add(h),
            };
            let Some(target_rect) = self.fit_target_rect(target_rect, env.taskbar_rect) else {
                // Drop the cached footprint too: with unchanged content the fast
                // path would otherwise re-solve the same refused box every
                // refresh and never re-measure.
                self.last_display = None;
                self.hide(HideReason::NoPlacementSlot);
                return;
            };
            self.last_anchor_slot = Some(placement.slot);
            self.last_compact = false;
            let mut deferred_occlusion = None;
            if self.in_elevated_band {
                let (check, rects) = match prechecked_occlusion.as_ref() {
                    Some((checked_rect, res, rects)) if *checked_rect == target_rect => {
                        (*res, rects.as_ref().clone())
                    }
                    _ => occlusion_check_with_rects(env.tray_hwnd, self.hwnd, target_rect),
                };
                if occlusion_leaves_unclipped(target_rect, check, &rects) {
                    deferred_occlusion = Some((check, rects));
                } else if self.apply_occlusion_policy(target_rect, check, rects) {
                    return;
                }
            }
            if self.last_pos != Some((x, y)) {
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        x,
                        y,
                        w,
                        h,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING,
                    );
                }
                self.last_pos = Some((x, y));
            }
            if let Some((check, rects)) = deferred_occlusion
                && self.apply_occlusion_policy(target_rect, check, rects)
            {
                return;
            }
            self.finish_show(env.tray_hwnd, uia_occupied.current);
            return;
        }

        let Some(normal_layout) = renderer.measure_layout(&normal_display, bounds) else {
            self.hide(HideReason::MeasureFailed);
            return;
        };
        let normal_size = (normal_layout.px_w, normal_layout.px_h);

        let same_footprint = self.last_display.as_ref().is_some_and(|d| {
            !core_geom_changed
                && self.last_stacked == normal_layout.stacked
                && d.show_net == normal_display.show_net
                && d.cpu_val.is_some() == normal_display.cpu_val.is_some()
                && d.ram_val.is_some() == normal_display.ram_val.is_some()
                && d.net_sample == normal_display.net_sample
                && d.font_family == normal_display.font_family
                && (d.font_size_pt - normal_display.font_size_pt).abs() < 0.01
                && d.dpi == normal_display.dpi
        });
        let normal_placement = solve_taskbar_placement(
            env.taskbar_rect,
            env.tray_notify_rect,
            Some(&uia_occupied.rects),
            normal_size,
            env.dpi,
            self.last_anchor_slot,
            self.last_compact && same_footprint && !force_render,
        );

        let (display, layout, placement) = if let Some(placement) = normal_placement {
            (normal_display, normal_layout, placement)
        } else {
            let compact_display = build_display(
                config,
                metrics,
                self.light_taskbar,
                self.hovered,
                env.dpi,
                true,
            );
            let Some(compact_layout) = renderer.measure_layout(&compact_display, bounds) else {
                self.hide(HideReason::MeasureFailed);
                return;
            };
            let compact_size = (compact_layout.px_w, compact_layout.px_h);
            let Some(placement) = solve_taskbar_placement(
                env.taskbar_rect,
                env.tray_notify_rect,
                Some(&uia_occupied.rects),
                compact_size,
                env.dpi,
                self.last_anchor_slot,
                false,
            ) else {
                // Taskbar is completely full even in compact form: step out of the way
                // instead of overlapping taskbar buttons or stealing their clicks.
                self.hide(HideReason::NoPlacementSlot);
                return;
            };
            (compact_display, compact_layout, placement)
        };

        let layout_stacked = layout.stacked;
        let (w, h) = (layout.px_w, layout.px_h);
        let (x, y) = placement.pos;
        let target_rect = PixelRect {
            left: x,
            top: y,
            right: x.saturating_add(w),
            bottom: y.saturating_add(h),
        };
        let Some(target_rect) = self.fit_target_rect(target_rect, env.taskbar_rect) else {
            // Drop the cached footprint too: with unchanged content the fast
            // path would otherwise re-solve the same refused box every refresh
            // and never re-measure.
            self.last_display = None;
            self.hide(HideReason::NoPlacementSlot);
            return;
        };
        self.last_anchor_slot = Some(placement.slot);
        self.last_compact = display.compact;
        // Clipping mutates the window and its tracked occluder, and must run
        // before `finish_show` restores that handle on show. A cut-out that
        // appears or grows (or a hide) is applied before the new frame is
        // presented, so a moving/resizing widget never paints over a menu or
        // topmost window; removing a cut-out waits until the frame is in place.
        let occlusion_check = self
            .in_elevated_band
            .then(|| match prechecked_occlusion.as_ref() {
                Some((checked_rect, res, rects)) if *checked_rect == target_rect => {
                    (*res, rects.as_ref().clone())
                }
                _ => occlusion_check_with_rects(env.tray_hwnd, self.hwnd, target_rect),
            });
        let deferred_occlusion = match occlusion_check {
            Some((check, rects)) if !occlusion_leaves_unclipped(target_rect, check, &rects) => {
                if self.apply_occlusion_policy(target_rect, check, rects) {
                    return;
                }
                None
            }
            other => other,
        };
        let needs_render = force_render
            || self.last_display.as_ref() != Some(&display)
            || self.last_size != (w, h)
            || self.last_stacked != layout_stacked;

        if needs_render {
            let Some(renderer) = self.renderer.as_mut() else {
                self.hide(HideReason::NoRenderer);
                return;
            };
            if let Some((rw, rh, src_dc)) =
                renderer.render_layout(&display, layout, Some(self.hwnd.0 as isize))
            {
                if !self.present_layered(x, y, rw, rh, src_dc) {
                    self.last_display = None;
                    self.hide(HideReason::PresentFailed);
                    return;
                }
                self.last_display = Some(display);
                self.last_size = (rw, rh);
                self.last_stacked = layout_stacked;
                self.last_pos = Some((x, y));
            } else {
                self.last_display = None;
                self.hide(HideReason::RenderFailed);
                return;
            }
        }

        if self.last_pos != Some((x, y)) {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    x,
                    y,
                    w,
                    h,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING,
                );
            }
            self.last_pos = Some((x, y));
        }
        if let Some((check, rects)) = deferred_occlusion
            && self.apply_occlusion_policy(target_rect, check, rects)
        {
            return;
        }
        self.finish_show(env.tray_hwnd, uia_occupied.current);
    }

    /// Re-renders changed content at the current footprint while UIA
    /// revalidates. Never moves or resizes the window: if the new content needs
    /// a different size, the pixels stay as they are until the scan completes.
    pub(super) fn rerender_in_place(
        &mut self,
        config: &Config,
        metrics: &Metrics,
        env: &TaskbarEnv,
        force_render: bool,
    ) {
        let Some((x, y)) = self.last_pos else {
            return;
        };
        let Some(previous) = self.last_display.as_ref() else {
            return;
        };
        let display = build_display(
            config,
            metrics,
            self.light_taskbar,
            self.hovered,
            env.dpi,
            self.last_compact,
        );
        if !force_render && previous == &display {
            return;
        }
        let bounds = LayoutBounds::from_taskbar(env.taskbar_rect, env.dpi);
        // The current footprint was fitted when it was placed. A bar that has
        // shrunk since then is handled by the geometry path, which hides and
        // invalidates before this re-render can run, so a footprint that no
        // longer fits means the frame is simply deferred: this path repaints
        // content and must not decide visibility, which is why it stays passive
        // where the placement paths hide.
        let current = PixelRect {
            left: x,
            top: y,
            right: x.saturating_add(self.last_size.0),
            bottom: y.saturating_add(self.last_size.1),
        };
        if self.fit_target_rect(current, env.taskbar_rect).is_none() {
            return;
        }
        let hwnd_raw = self.hwnd.0 as isize;
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        if force_render {
            renderer.invalidate_rendering_params();
        }
        let Some(layout) = renderer.measure_layout(&display, bounds) else {
            return;
        };
        if (layout.px_w, layout.px_h) != self.last_size || layout.stacked != self.last_stacked {
            return;
        }
        let Some((w, h, src_dc)) = renderer.render_layout(&display, layout, Some(hwnd_raw)) else {
            return;
        };
        if self.present_layered(x, y, w, h, src_dc) {
            self.last_display = Some(display);
        }
    }

    pub(super) fn present_layered(&mut self, x: i32, y: i32, w: i32, h: i32, src_dc: HDC) -> bool {
        let pt_dst = POINT { x, y };
        let size = SIZE { cx: w, cy: h };
        let pt_src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let result = unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pt_dst),
                Some(&size),
                Some(src_dc),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        };
        match result {
            Ok(()) => {
                self.last_present_error = None;
                true
            }
            Err(error) => {
                if self
                    .last_present_error
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(30))
                {
                    tracing::warn!(%error, "cannot present taskbar monitor; keeping it hidden");
                    self.last_present_error = Some(Instant::now());
                }
                false
            }
        }
    }
}

pub(super) fn cursor_inside_window(hwnd: HWND) -> bool {
    let mut cur = POINT::default();
    let mut wr = RECT::default();
    let ok = unsafe { GetCursorPos(&mut cur).is_ok() && GetWindowRect(hwnd, &mut wr).is_ok() };
    ok && cur.x >= wr.left && cur.x < wr.right && cur.y >= wr.top && cur.y < wr.bottom
}

#[cfg(test)]
mod tests;

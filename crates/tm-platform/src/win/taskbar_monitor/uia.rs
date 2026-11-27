//! Out-of-process UI Automation scanner that reports occupied taskbar
//! rectangles from a dedicated MTA worker thread.

use super::hooks::post_refresh;
use super::{PixelRect, UIA_SETTLE_DELAY_MS};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    AutomationElementMode_None, CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2,
    IUIAutomationCacheRequest, IUIAutomationCondition, TreeScope_Descendants, TreeScope_Element,
    UIA_BoundingRectanglePropertyId, UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId, UIA_ControlTypePropertyId, UIA_EditControlTypeId,
    UIA_IsOffscreenPropertyId, UIA_ListItemControlTypeId, UIA_MenuItemControlTypeId,
    UIA_SplitButtonControlTypeId,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::core::Interface;

pub(super) const UIA_MIN_SCAN_INTERVAL: Duration = Duration::from_millis(80);
/// Event-driven rechecks scan once the taskbar has been quiet this long
/// (trailing edge), so a burst of shell/taskbar notifications costs Explorer
/// one cross-process UIA walk instead of one per 80 ms plus a settle rescan.
pub(super) const UIA_RECHECK_SETTLE: Duration = Duration::from_millis(UIA_SETTLE_DELAY_MS as u64);
/// Upper bound on how long a continuous notification stream may postpone a
/// debounced recheck.
pub(super) const UIA_RECHECK_MAX_DELAY: Duration = Duration::from_secs(1);
/// Consecutive failed rechecks of an unchanged taskbar that keep the last
/// confirmed layout (click-through, never current) before it is discarded and
/// the widget hides. With the backoff steps below this is roughly 28 s so
/// transient Explorer stalls under heavy RAM/CPU pressure keep updating
/// metrics click-through instead of blinking the taskbar monitor away.
pub(super) const UIA_STALE_LAYOUT_MAX_FAILURES: u32 = 8;
pub(super) const UIA_FAILURE_BACKOFF_STEPS: [Duration; 4] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];

pub(super) struct ComMtaGuard {
    pub(super) initialized: bool,
}

impl ComMtaGuard {
    pub(super) fn enter() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            initialized: hr.is_ok(),
        }
    }
}

impl Drop for ComMtaGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

pub(super) struct UiaSession {
    pub(super) uia: IUIAutomation,
    pub(super) condition: IUIAutomationCondition,
    pub(super) cache_request: IUIAutomationCacheRequest,
}

impl UiaSession {
    pub(super) fn new() -> windows::core::Result<Self> {
        let uia: IUIAutomation = unsafe {
            if let Ok(uia2) =
                CoCreateInstance::<_, IUIAutomation2>(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
            {
                let _ = uia2.SetConnectionTimeout(150);
                let _ = uia2.SetTransactionTimeout(250);
                uia2.cast()?
            } else {
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?
            }
        };

        let control_view = unsafe { uia.ControlViewCondition()? };
        let not_offscreen = unsafe {
            let v = VARIANT::from(false);
            uia.CreatePropertyCondition(UIA_IsOffscreenPropertyId, &v)?
        };
        let control_types = [
            UIA_ButtonControlTypeId,
            UIA_SplitButtonControlTypeId,
            UIA_CheckBoxControlTypeId,
            UIA_MenuItemControlTypeId,
            UIA_EditControlTypeId,
            UIA_ComboBoxControlTypeId,
            UIA_ListItemControlTypeId,
        ];
        let mut type_conds = Vec::with_capacity(control_types.len());
        for ct in control_types {
            let v = VARIANT::from(ct.0);
            let c = unsafe { uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &v)? };
            type_conds.push(Some(c));
        }
        let or_types = unsafe { uia.CreateOrConditionFromNativeArray(&type_conds)? };
        let visible_controls = unsafe { uia.CreateAndCondition(&control_view, &not_offscreen)? };
        let condition = unsafe { uia.CreateAndCondition(&visible_controls, &or_types)? };

        let cache_request = unsafe { uia.CreateCacheRequest()? };
        unsafe {
            cache_request.AddProperty(UIA_BoundingRectanglePropertyId)?;
            cache_request.SetTreeScope(TreeScope_Element)?;
            cache_request.SetAutomationElementMode(AutomationElementMode_None)?;
        }

        Ok(Self {
            uia,
            condition,
            cache_request,
        })
    }

    pub(super) fn scan_tray(&self, tray_raw: isize) -> windows::core::Result<Vec<PixelRect>> {
        let tray_hwnd = HWND(tray_raw as *mut std::ffi::c_void);
        if tray_raw == 0
            || !unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tray_hwnd)).as_bool()
            }
        {
            return Ok(Vec::new());
        }
        let root = unsafe { self.uia.ElementFromHandle(tray_hwnd)? };
        let found = unsafe {
            root.FindAllBuildCache(TreeScope_Descendants, &self.condition, &self.cache_request)?
        };
        let len = unsafe { found.Length()? };
        collect_cached_rects(len, |i| unsafe {
            let elem = found.GetElement(i)?;
            elem.CachedBoundingRectangle()
        })
    }
}

pub(super) fn collect_cached_rects<I>(
    len: i32,
    mut read_rect: I,
) -> windows::core::Result<Vec<PixelRect>>
where
    I: FnMut(i32) -> windows::core::Result<RECT>,
{
    let mut rects = Vec::with_capacity(len.max(0) as usize);
    for i in 0..len {
        // Propagate any per-element cache/COM error so a failed rectangle read
        // triggers `uia_worker_main`'s retry/backoff path instead of publishing
        // a partial obstacle list that could place the widget over an omitted
        // taskbar button and intercept its clicks.
        let rc = read_rect(i)?;
        let px = PixelRect::from_win32(rc);
        if !px.is_empty() {
            rects.push(px);
        }
    }
    rects.sort_unstable_by_key(|r| (r.left, r.top, r.right, r.bottom));
    rects.dedup();
    Ok(rects)
}

#[derive(Default)]
pub(super) struct UiaCache {
    pub(super) tray_hwnd: isize,
    pub(super) taskbar_rect: PixelRect,
    pub(super) rects: Option<Vec<PixelRect>>,
    pub(super) generation: u64,
    /// The last confirmed rectangles survived a failed recheck of the same
    /// taskbar. They keep an already placed widget where it is (click-through),
    /// but never count as current until a scan succeeds again.
    pub(super) stale: bool,
}

pub(super) struct UiaSnapshot {
    pub(super) rects: Vec<PixelRect>,
    pub(super) current: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct UiaTarget {
    pub(super) tray_hwnd: isize,
    pub(super) taskbar_rect: PixelRect,
    pub(super) generation: u64,
}

#[derive(Default)]
pub(super) struct UiaShared {
    pub(super) pending: Option<UiaTarget>,
    pub(super) cache: UiaCache,
    pub(super) generation: u64,
    pub(super) min_valid_generation: u64,
    pub(super) shutdown: bool,
    pub(super) consecutive_failures: u32,
    pub(super) backoff_until: Option<Instant>,
    pub(super) last_target: Option<(isize, PixelRect)>,
    /// Trailing-edge deadline of a debounced recheck (`None` = scan now).
    pub(super) not_before: Option<Instant>,
    /// Start of the current debounced burst, capping `not_before`.
    pub(super) burst_started: Option<Instant>,
}

pub(super) fn uia_failure_backoff(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    let idx =
        (consecutive_failures.saturating_sub(1) as usize).min(UIA_FAILURE_BACKOFF_STEPS.len() - 1);
    UIA_FAILURE_BACKOFF_STEPS[idx]
}

impl UiaShared {
    pub(super) fn next_wait(&self, last_scan: Option<Instant>, now: Instant) -> Option<Duration> {
        let wait_until = last_scan
            .map(|prev| prev + UIA_MIN_SCAN_INTERVAL)
            .into_iter()
            .chain(self.backoff_until)
            .chain(self.not_before)
            .max()?;
        (now < wait_until).then(|| wait_until - now)
    }

    /// Hands the pending target to the worker and ends any debounce burst.
    pub(super) fn take_pending(&mut self) -> Option<UiaTarget> {
        let target = self.pending.take()?;
        self.not_before = None;
        self.burst_started = None;
        Some(target)
    }

    pub(super) fn publish(&mut self, target: UiaTarget, rects: Option<Vec<PixelRect>>) -> bool {
        self.publish_at(target, rects, Instant::now())
    }

    pub(super) fn publish_at(
        &mut self,
        target: UiaTarget,
        rects: Option<Vec<PixelRect>>,
        now: Instant,
    ) -> bool {
        if self.shutdown {
            return false;
        }
        let rects = rects.filter(|r| !r.is_empty());
        if target.generation != self.generation {
            // If an in-flight scan started after the latest invalidation/target
            // change finishes with valid rectangles while `cache.rects` is still
            // empty (e.g. startup or post-resize burst), populate `cache.rects`
            // with `target.generation` (`current == false`) instead of starving
            // the cache across back-to-back recheck requests.
            let valid_for_current_target = self.last_target
                == Some((target.tray_hwnd, target.taskbar_rect))
                && target.generation.wrapping_sub(self.min_valid_generation)
                    <= self.generation.wrapping_sub(self.min_valid_generation);
            if valid_for_current_target && self.cache.rects.is_none() && rects.is_some() {
                self.consecutive_failures = 0;
                self.backoff_until = None;
                self.cache = UiaCache {
                    tray_hwnd: target.tray_hwnd,
                    taskbar_rect: target.taskbar_rect,
                    rects,
                    generation: target.generation,
                    stale: false,
                };
                return true;
            }
            return false;
        }
        let same_target = self.cache.tray_hwnd == target.tray_hwnd
            && self.cache.taskbar_rect == target.taskbar_rect;
        let had_rects_for_target = same_target && self.cache.rects.is_some();
        let changed = if rects.is_some() {
            self.consecutive_failures = 0;
            self.backoff_until = None;
            if self
                .pending
                .is_some_and(|pending| pending.generation == target.generation)
            {
                self.pending = None;
            }
            !same_target
                || self.cache.stale
                || self.cache.rects != rects
                || self.cache.generation != target.generation
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            self.backoff_until = Some(now + uia_failure_backoff(self.consecutive_failures));
            // Keep retry ownership inside the UIA worker across failures so recovery
            // continues after backoff even when sampling is paused and `changed == false`.
            if self.pending.is_none() && target.tray_hwnd != 0 && !target.taskbar_rect.is_empty() {
                self.pending = Some(target);
            }
            // A timed-out or momentarily empty recheck of an unchanged taskbar
            // is not evidence that the confirmed layout vanished. Keep it as
            // stale (click-through, never current) while the worker retries,
            // instead of blinking the widget out for the whole backoff. Only a
            // persistent failure discards it. Geometry/identity changes never
            // get here with old rects: they invalidate the cache up front.
            if had_rects_for_target && self.consecutive_failures <= UIA_STALE_LAYOUT_MAX_FAILURES {
                let newly_stale = !self.cache.stale;
                self.cache.stale = true;
                return newly_stale && self.cache.generation == target.generation;
            }
            // Only wake the window if previously cached rectangles for this target
            // must be cleared; repeated unavailable scans must not spin refresh.
            had_rects_for_target
        };
        self.cache = UiaCache {
            tray_hwnd: target.tray_hwnd,
            taskbar_rect: target.taskbar_rect,
            rects,
            generation: target.generation,
            stale: false,
        };
        changed
    }
}

/// Scans `Shell_TrayWnd` UI Automation bounding rectangles on a dedicated MTA
/// thread so cross-process UIA COM calls never pump messages or block `"tm-taskbar"`.
pub(super) struct UiaScanner {
    pub(super) state: Arc<(Mutex<UiaShared>, Condvar)>,
    pub(super) worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl UiaScanner {
    pub(super) fn start() -> Self {
        let state = Arc::new((Mutex::new(UiaShared::default()), Condvar::new()));
        let worker_state = state.clone();
        let handle = match std::thread::Builder::new()
            .name("tm-taskbar-uia".to_string())
            .spawn(move || uia_worker_main(worker_state))
        {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::warn!(%error, "cannot start taskbar layout scanner; monitor hidden");
                None
            }
        };
        Self {
            state,
            worker: Mutex::new(handle),
        }
    }

    pub(super) fn snapshot_for(
        &self,
        tray_raw: isize,
        taskbar_rect: PixelRect,
    ) -> Option<UiaSnapshot> {
        let guard = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
        if guard.cache.tray_hwnd == tray_raw && guard.cache.taskbar_rect == taskbar_rect {
            guard.cache.rects.clone().map(|rects| UiaSnapshot {
                rects,
                current: !guard.cache.stale && guard.cache.generation == guard.generation,
            })
        } else {
            None
        }
    }

    /// Requests a scan as soon as the worker's rate limit/backoff allows.
    pub(super) fn request_scan(&self, tray_raw: isize, taskbar_rect: PixelRect, invalidate: bool) {
        self.request_scan_at(tray_raw, taskbar_rect, invalidate, false, Instant::now());
    }

    /// Requests an event-driven recheck that runs once notifications have been
    /// quiet for [`UIA_RECHECK_SETTLE`] (bounded by [`UIA_RECHECK_MAX_DELAY`]).
    /// The cached layout stops being current immediately, so the widget passes
    /// clicks through until the trailing scan confirms it.
    pub(super) fn request_debounced_recheck(&self, tray_raw: isize, taskbar_rect: PixelRect) {
        self.request_scan_at(tray_raw, taskbar_rect, false, true, Instant::now());
    }

    pub(super) fn request_scan_at(
        &self,
        tray_raw: isize,
        taskbar_rect: PixelRect,
        invalidate: bool,
        debounce: bool,
        now: Instant,
    ) {
        if tray_raw == 0 || taskbar_rect.is_empty() {
            return;
        }
        let mut guard = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
        if guard.shutdown {
            return;
        }
        let target_key = (tray_raw, taskbar_rect);
        let target_changed = guard.last_target != Some(target_key);
        if target_changed {
            guard.last_target = Some(target_key);
            guard.consecutive_failures = 0;
            guard.backoff_until = None;
        }
        if invalidate {
            guard.cache.rects = None;
            guard.cache.stale = false;
        }
        // If occupancy is already unavailable and a retry is already pending for
        // the same target, avoid churning `generation` or waking the worker.
        if guard.cache.rects.is_none() && !target_changed && !invalidate && guard.pending.is_some()
        {
            return;
        }
        let pending_immediately = guard.pending.is_some() && guard.not_before.is_none();
        // Even a recheck supersedes an in-flight scan. Publishing unchanged
        // rectangles must wake the window so it can restore hit testing.
        guard.generation = guard.generation.wrapping_add(1);
        if target_changed || invalidate {
            guard.min_valid_generation = guard.generation;
        }
        guard.pending = Some(UiaTarget {
            tray_hwnd: tray_raw,
            taskbar_rect,
            generation: guard.generation,
        });
        // A debounced recheck never delays a scan that is already due now
        // (startup, invalidation, failure retry); an immediate request ends
        // any debounce burst.
        if debounce && !target_changed && !pending_immediately {
            let started = *guard.burst_started.get_or_insert(now);
            guard.not_before =
                Some((now + UIA_RECHECK_SETTLE).min(started + UIA_RECHECK_MAX_DELAY));
        } else {
            guard.not_before = None;
            guard.burst_started = None;
        }
        self.state.1.notify_one();
    }

    pub(super) fn shutdown(&self) {
        {
            let mut guard = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
            guard.shutdown = true;
            guard.pending = None;
            self.state.1.notify_all();
        }
        if let Ok(mut slot) = self.worker.lock()
            && let Some(handle) = slot.take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for UiaScanner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(super) fn uia_worker_main(state: Arc<(Mutex<UiaShared>, Condvar)>) {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _com = ComMtaGuard::enter();
    let mut session: Option<UiaSession> = None;
    let mut last_scan: Option<Instant> = None;
    let mut last_failure_report: Option<Instant> = None;

    loop {
        let target = {
            let mut guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if guard.shutdown {
                    return;
                }
                if guard.pending.is_some() {
                    if let Some(wait) = guard.next_wait(last_scan, Instant::now()) {
                        let (g, _) = state
                            .1
                            .wait_timeout(guard, wait)
                            .unwrap_or_else(|e| e.into_inner());
                        guard = g;
                        continue;
                    }
                    if let Some(target) = guard.take_pending() {
                        break target;
                    }
                }
                guard = state.1.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
        };

        last_scan = Some(Instant::now());
        if session.is_none() {
            session = UiaSession::new().ok();
        }
        let mut scan_error: Option<windows::core::Error> = None;
        let mut result = match session.as_ref() {
            Some(s) => match s.scan_tray(target.tray_hwnd) {
                Ok(rects) => Ok(rects),
                Err(err) => {
                    session = None;
                    scan_error = Some(err);
                    Err(())
                }
            },
            None => Err(()),
        };

        // If the first attempt hit a transient COM/RPC error while previously
        // healthy, re-initialize the UIA session and retry once immediately
        // before clearing the layout cache and entering progressive backoff.
        if result.is_err() && scan_error.is_some() {
            let should_retry_immediately = {
                let guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
                !guard.shutdown && guard.consecutive_failures == 0 && guard.pending.is_none()
            };
            if should_retry_immediately {
                session = UiaSession::new().ok();
                if let Some(s) = session.as_ref() {
                    match s.scan_tray(target.tray_hwnd) {
                        Ok(rects) => {
                            scan_error = None;
                            result = Ok(rects);
                        }
                        Err(err) => {
                            session = None;
                            scan_error = Some(err);
                        }
                    }
                }
            }
        }

        let rects = result.ok().filter(|r| !r.is_empty());
        let failed = rects.is_none();
        let (changed, failures, kept_stale) = {
            let mut guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
            let changed = guard.publish(target, rects);
            let kept_stale = guard.cache.stale && guard.cache.rects.is_some();
            (changed, guard.consecutive_failures, kept_stale)
        };
        if failed {
            if last_failure_report.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
                tracing::warn!(
                    error = ?scan_error,
                    consecutive_failures = failures,
                    kept_last_layout = kept_stale,
                    "taskbar layout scan failed; retrying with backoff (monitor stays put \
                     click-through on its last layout, or hides once that is discarded)"
                );
                last_failure_report = Some(Instant::now());
            }
        } else {
            last_failure_report = None;
        }
        if changed {
            post_refresh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::taskbar_monitor::test_support::rect;

    #[test]
    fn uia_cache_rejects_stale_scans_and_demotes_failed_or_empty_results() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let bar = rect(0, 1032, 1920, 1080);
        let occupied = vec![rect(500, 1032, 1000, 1080)];
        scanner.request_scan(1, bar, true);
        let old_target = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(
            scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(old_target, Some(occupied.clone()))
        );
        assert_eq!(scanner.snapshot_for(1, bar).unwrap().rects, occupied);

        scanner.request_scan(1, bar, true);
        assert!(scanner.snapshot_for(1, bar).is_none());
        assert!(
            !scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(old_target, Some(occupied.clone()))
        );
        assert!(scanner.snapshot_for(1, bar).is_none());
        let target = scanner.state.0.lock().unwrap().pending.take().unwrap();
        for unavailable in [None, Some(Vec::new())] {
            scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(target, Some(occupied.clone()));
            assert!(scanner.snapshot_for(1, bar).unwrap().current);
            assert!(
                scanner.state.0.lock().unwrap().publish(target, unavailable),
                "losing confirmation of a current layout must wake the window to drop input"
            );
            let kept = scanner
                .snapshot_for(1, bar)
                .expect("a transient failure keeps the confirmed layout");
            assert_eq!(kept.rects, occupied);
            assert!(!kept.current, "a failed layout must never count as current");
        }
        // Only a persistent failure discards the layout (and wakes the window).
        for _ in 1..UIA_STALE_LAYOUT_MAX_FAILURES {
            assert!(!scanner.state.0.lock().unwrap().publish(target, None));
            assert!(!scanner.snapshot_for(1, bar).unwrap().current);
        }
        assert!(scanner.state.0.lock().unwrap().publish(target, None));
        assert!(
            scanner.snapshot_for(1, bar).is_none(),
            "persistently failed layout must not reuse stale buttons"
        );
        assert!(scanner.snapshot_for(2, bar).is_none());
        assert!(scanner.snapshot_for(1, rect(0, 0, 1920, 48)).is_none());
    }

    #[test]
    fn uia_revalidation_preserves_pixels_but_requires_latest_scan_for_input() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let bar = rect(0, 1032, 1920, 1080);
        let occupied = vec![rect(500, 1032, 1000, 1080)];
        scanner.request_scan(1, bar, true);
        let initial = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(
            scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(initial, Some(occupied.clone()))
        );
        assert!(scanner.snapshot_for(1, bar).unwrap().current);

        scanner.request_scan(1, bar, false);
        let superseded = scanner.state.0.lock().unwrap().pending.take().unwrap();
        scanner.request_scan(1, bar, false);
        let provisional = scanner.snapshot_for(1, bar).unwrap();
        assert_eq!(provisional.rects, occupied);
        assert!(!provisional.current);
        assert!(
            !scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(superseded, Some(occupied.clone()))
        );
        assert!(!scanner.snapshot_for(1, bar).unwrap().current);

        let latest = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(
            scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish(latest, Some(occupied.clone())),
            "unchanged occupancy must still wake the window to restore input"
        );
        let validated = scanner.snapshot_for(1, bar).unwrap();
        assert!(validated.current);
        assert_eq!(validated.rects, occupied);
    }

    #[test]
    fn uia_failed_scan_requeues_retry_in_worker_without_caller_refresh() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let bar = rect(-24_000, -24_000, -22_080, -23_952);
        let t0 = Instant::now();

        scanner.request_scan(1, bar, true);
        let t1 = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(!scanner.state.0.lock().unwrap().publish_at(t1, None, t0));

        // Even with no further `request_scan` (e.g. paused sampling), the worker
        // re-queues the failed target and honors progressive backoff before retrying.
        let t2 = {
            let mut guard = scanner.state.0.lock().unwrap();
            assert_eq!(
                guard.next_wait(Some(t0), t0),
                Some(Duration::from_millis(500))
            );
            guard
                .pending
                .take()
                .expect("failed scan must leave retry queued in UiaShared::pending")
        };

        let t1_step = t0 + Duration::from_millis(500);
        assert!(
            !scanner
                .state
                .0
                .lock()
                .unwrap()
                .publish_at(t2, None, t1_step)
        );
        let t3 = {
            let mut guard = scanner.state.0.lock().unwrap();
            assert_eq!(
                guard.next_wait(Some(t1_step), t1_step),
                Some(Duration::from_secs(1))
            );
            guard
                .pending
                .take()
                .expect("repeated failure must stay queued for next backoff")
        };

        assert!(scanner.state.0.lock().unwrap().publish_at(
            t3,
            Some(vec![rect(-22_300, -24_000, -22_080, -23_952)]),
            t1_step + Duration::from_secs(1),
        ));
        assert!(scanner.state.0.lock().unwrap().pending.is_none());
    }

    #[test]
    fn uia_superseded_scan_populates_empty_cache_while_keeping_provisional_status() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let bar = rect(-24_000, -24_000, -22_080, -23_952);
        let t0 = Instant::now();

        // Start from an invalidated cache (`rects = None`).
        scanner.request_scan(1, bar, true);
        let target_1 = scanner.state.0.lock().unwrap().pending.take().unwrap();

        // While `target_1` is running on the worker thread, a soft recheck arrives and bumps generation.
        scanner.request_scan(1, bar, false);
        let target_2 = scanner.state.0.lock().unwrap().pending.unwrap();
        assert!(target_2.generation > target_1.generation);

        // When `target_1` finishes with valid rects, `publish_at` must populate the empty
        // cache immediately and wake refresh, while keeping `current == false` until `target_2` completes.
        let sample_rects = vec![rect(-22_300, -24_000, -22_080, -23_952)];
        assert!(scanner.state.0.lock().unwrap().publish_at(
            target_1,
            Some(sample_rects.clone()),
            t0,
        ));
        let snap = scanner
            .snapshot_for(1, bar)
            .expect("superseded post-invalidation scan must populate empty cache");
        assert!(!snap.current);
        assert_eq!(&*snap.rects, &sample_rects[..]);

        // Now complete `target_2`: snapshot becomes current and wakes refresh so hit-testing can be restored.
        let target_2_taken = scanner.state.0.lock().unwrap().pending.take().unwrap();
        assert!(scanner.state.0.lock().unwrap().publish_at(
            target_2_taken,
            Some(sample_rects),
            t0 + Duration::from_millis(50),
        ));
        assert!(scanner.snapshot_for(1, bar).unwrap().current);

        // Conversely, a scan started BEFORE a hard `invalidate = true` boundary must never
        // repopulate the invalidated cache.
        scanner.request_scan(1, bar, false);
        let pre_inval_target = scanner.state.0.lock().unwrap().pending.take().unwrap();
        scanner.request_scan(1, bar, true);
        assert!(!scanner.state.0.lock().unwrap().publish_at(
            pre_inval_target,
            Some(vec![rect(-22_300, -24_000, -22_080, -23_952)]),
            t0 + Duration::from_millis(100),
        ));
        assert!(
            scanner.snapshot_for(1, bar).is_none(),
            "scan from before an invalidation boundary must not populate cache"
        );
    }

    #[test]
    fn collect_cached_rects_propagates_element_error_instead_of_returning_partial_occupancy() {
        let ok_rects = [
            RECT {
                left: -9_900,
                top: -10_000,
                right: -9_850,
                bottom: -9_952,
            },
            RECT {
                left: -8_300,
                top: -10_000,
                right: -8_080,
                bottom: -9_952,
            },
        ];
        let full = collect_cached_rects(2, |i| Ok(ok_rects[i as usize])).unwrap();
        assert_eq!(full.len(), 2);

        let err = collect_cached_rects(2, |i| {
            if i == 0 {
                Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80040201u32 as i32,
                )))
            } else {
                Ok(ok_rects[1])
            }
        });
        assert!(
            err.is_err(),
            "per-element failure must abort scan rather than publishing a partial obstacle map"
        );
    }

    #[test]
    fn failed_uia_rechecks_keep_stale_layout_until_persistent_then_clear() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let target_rect = rect(0, 0, 1920, 48);
        let confirmed = vec![rect(100, 0, 200, 48)];
        let target = UiaTarget {
            tray_hwnd: 42,
            taskbar_rect: target_rect,
            generation: 0,
        };
        {
            let mut guard = scanner.state.0.lock().unwrap();
            assert!(guard.publish(target, Some(confirmed.clone())));
            for failure in 1..=UIA_STALE_LAYOUT_MAX_FAILURES {
                // Only the first failure changes what the window must do.
                assert_eq!(guard.publish(target, None), failure == 1);
                assert_eq!(guard.cache.rects.as_ref(), Some(&confirmed));
                assert!(guard.cache.stale);
                assert_eq!(guard.consecutive_failures, failure);
                assert!(guard.pending.is_some(), "the worker keeps retrying");
            }
        }
        let held = scanner.snapshot_for(42, target_rect).unwrap();
        assert!(!held.current);
        assert_eq!(held.rects, confirmed);
        {
            let mut guard = scanner.state.0.lock().unwrap();
            assert!(guard.publish(target, None));
            assert_eq!(
                guard.cache.rects, None,
                "a persistent failure must clear cached pixels"
            );
            assert!(!guard.cache.stale);
        }
        assert!(scanner.snapshot_for(42, target_rect).is_none());

        // A successful retry within the grace restores a current layout.
        let mut guard = scanner.state.0.lock().unwrap();
        assert!(guard.publish(target, Some(confirmed.clone())));
        assert!(guard.publish(target, None));
        assert!(guard.cache.stale);
        assert!(
            guard.publish(target, Some(confirmed.clone())),
            "a recovered layout must wake the window to restore input"
        );
        assert!(!guard.cache.stale);
        assert_eq!(guard.consecutive_failures, 0);
        drop(guard);
        assert!(scanner.snapshot_for(42, target_rect).unwrap().current);
    }

    #[test]
    fn debounced_rechecks_scan_once_after_the_burst_settles() {
        let scanner = UiaScanner {
            state: Arc::new((Mutex::new(UiaShared::default()), Condvar::new())),
            worker: Mutex::new(None),
        };
        let bar = rect(0, 1032, 1920, 1080);
        let occupied = vec![rect(500, 1032, 1000, 1080)];
        let t0 = Instant::now();

        // Startup/invalidation scans run immediately.
        scanner.request_scan_at(1, bar, true, false, t0);
        {
            let mut guard = scanner.state.0.lock().unwrap();
            assert_eq!(guard.next_wait(None, t0), None);
            let target = guard.take_pending().unwrap();
            assert!(guard.publish_at(target, Some(occupied.clone()), t0));
        }

        // A burst of event-driven rechecks: the layout is unconfirmed at once
        // (click-through), but the scan waits until the burst goes quiet.
        let step = Duration::from_millis(100);
        for i in 0..5u32 {
            scanner.request_scan_at(1, bar, false, true, t0 + step * i);
            assert!(!scanner.snapshot_for(1, bar).unwrap().current);
        }
        let last = t0 + step * 4;
        {
            let guard = scanner.state.0.lock().unwrap();
            assert_eq!(guard.next_wait(None, last), Some(UIA_RECHECK_SETTLE));
            assert_eq!(guard.next_wait(None, last + UIA_RECHECK_SETTLE), None);
        }

        // A continuous notification stream cannot postpone it past the cap.
        for i in 5..30u32 {
            scanner.request_scan_at(1, bar, false, true, t0 + step * i);
        }
        {
            let mut guard = scanner.state.0.lock().unwrap();
            assert_eq!(guard.not_before, Some(t0 + UIA_RECHECK_MAX_DELAY));
            assert_eq!(guard.next_wait(None, t0 + UIA_RECHECK_MAX_DELAY), None);
            // One scan answers the whole burst and restores input.
            let target = guard.take_pending().unwrap();
            assert!(guard.not_before.is_none() && guard.burst_started.is_none());
            assert!(guard.publish_at(target, Some(occupied.clone()), t0 + UIA_RECHECK_MAX_DELAY));
        }
        assert!(scanner.snapshot_for(1, bar).unwrap().current);

        // A debounced request never delays one that is already due, and an
        // immediate request ends a running burst.
        let t1 = t0 + Duration::from_secs(5);
        scanner.request_scan_at(1, bar, false, false, t1);
        scanner.request_scan_at(1, bar, false, true, t1);
        assert!(scanner.state.0.lock().unwrap().not_before.is_none());
        let _ = scanner.state.0.lock().unwrap().take_pending();
        scanner.request_scan_at(1, bar, false, true, t1);
        assert!(scanner.state.0.lock().unwrap().not_before.is_some());
        scanner.request_scan_at(1, bar, false, false, t1);
        assert!(scanner.state.0.lock().unwrap().not_before.is_none());

        // A different taskbar target has no layout yet: scan it immediately.
        let _ = scanner.state.0.lock().unwrap().take_pending();
        scanner.request_scan_at(1, rect(0, 0, 1920, 48), false, true, t1);
        assert!(scanner.state.0.lock().unwrap().not_before.is_none());
    }
}

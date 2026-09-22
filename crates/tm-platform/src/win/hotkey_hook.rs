//! Low-level keyboard hook (`WH_KEYBOARD_LL`) for intercepting Ctrl+Shift+Escape.
//!
//! Windows routes keyboard input directly to exclusive and borderless fullscreen
//! applications (such as 3D games). In that mode, Windows suppresses shell hotkeys
//! registered via `RegisterHotKey`, so Explorer never receives Ctrl+Shift+Escape and
//! never attempts to spawn `taskmgr.exe`.
//!
//! A user-mode low-level keyboard hook (`WH_KEYBOARD_LL`) runs before normal input
//! processing and reliably intercepts Ctrl+Shift+Escape even when a fullscreen game
//! holds focus. By pumping a dedicated message loop on a background thread, the hook
//! detects the key combo with zero latency, raises TaskMan, and consumes the Escape
//! key so the underlying application does not interpret it (e.g. opening game menus).
//!
//! ## What this costs the rest of the machine
//!
//! A `WH_KEYBOARD_LL` hook is not free for anyone else: EVERY keystroke on the
//! desktop is routed through this process before it reaches the application the
//! user is typing into, and a callback that blocks delays that keystroke
//! system-wide until Windows' `LowLevelHooksTimeout` gives up on it. Two rules
//! follow, and both are load-bearing:
//!
//! * **The hook exists only while it is needed.** It is installed when TaskMan
//!   is the registered Task Manager replacement and taken down again when it is
//!   not, driven by a registry change notification rather than by polling. A
//!   user who never turned the replacement on never has their keyboard routed
//!   through this process at all.
//! * **The callback does no work.** No allocation, no registry, no lock, no
//!   syscall beyond the key-state reads: it consults one atomic, signals an
//!   event and returns. Everything the hotkey actually DOES — raising the
//!   window, attaching thread input — happens on the worker thread, which
//!   pumps its own message queue precisely so that attaching to another
//!   application's input queue cannot stall that application. It is the only
//!   caller in the codebase allowed to use
//!   [`super::window_chrome::force_foreground_attached`].
//!
//! Two smaller rules fall out of the same reasoning and are easy to undo by
//! accident, so they are written down here as well:
//!
//! * **Nothing blocking runs on the hook THREAD while a hook is installed.**
//!   Between `SetWindowsHookExW` returning and the thread's next
//!   `GetMessageW` there is no keyboard input on this machine, so the one
//!   call in `set_hook` that takes locks and can reach a file — `tracing` —
//!   is issued before the hook exists, never after.
//! * **The thread is opted out of EcoQoS.** Windows throttles a process it
//!   considers background, which is the resting state of a task manager in
//!   the notification area, and `THREAD_PRIORITY_HIGHEST` does not cover
//!   that; see [`opt_out_of_power_throttling`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetEvent, SetThreadInformation, SetThreadPriority,
    THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_HIGHEST, ThreadPowerThrottling,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG,
    MsgWaitForMultipleObjects, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    QS_ALLINPUT, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP,
    WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

static CONSUMED_DOWN: AtomicBool = AtomicBool::new(false);
static LAST_DOWN_TICK: AtomicU64 = AtomicU64::new(0);
static TRIGGER_EVENT: AtomicIsize = AtomicIsize::new(0);
static WORKER_RUNNING: AtomicBool = AtomicBool::new(false);
/// Whether TaskMan is the registered Task Manager replacement.
///
/// The hook callback's only piece of state. It is a plain atomic because the
/// callback runs on the system's input path: the registry read that answers
/// this question lives on the worker thread, which re-runs it when the IFEO
/// key actually changes.
static REPLACEMENT_ENABLED: AtomicBool = AtomicBool::new(false);

static HOOK_CALLBACK: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>> =
    std::sync::Mutex::new(None);

/// Ask the hook thread to install (`wparam == 1`) or remove the keyboard hook.
/// A thread message, so it carries no window and never reaches a window proc.
const WM_SET_HOOK: u32 = WM_APP + 1;

/// Backstop wait for a worker whose registry notification could not be armed.
/// Only ever used on that fallback path; with the watch in place the worker
/// waits indefinitely and is woken by the change itself.
const UNWATCHED_POLL_MS: u32 = 5_000;

/// How long a swallowed Escape press stays eligible to have its release
/// swallowed too. Auto-repeat refreshes [`LAST_DOWN_TICK`] roughly every 30 ms
/// while the combo is held, so a genuine press-and-hold never ages out; only a
/// press whose release was delivered somewhere this hook cannot see does.
const CONSUMED_DOWN_TTL_MS: u64 = 1_000;

pub struct HotkeyHook {
    hook_thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
    worker_join: Option<std::thread::JoinHandle<()>>,
    event: HANDLE,
    registry_event: HANDLE,
}

impl HotkeyHook {
    pub fn install<F>(on_hotkey: F) -> Option<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        *tm_core::sync::lock(&HOOK_CALLBACK) = Some(Arc::new(on_hotkey));

        let event = unsafe { CreateEventW(None, false, false, None) }.ok()?;
        let registry_event = match unsafe { CreateEventW(None, false, false, None) } {
            Ok(handle) => handle,
            Err(_) => {
                unsafe {
                    let _ = CloseHandle(event);
                }
                return None;
            }
        };
        TRIGGER_EVENT.store(event.0 as isize, Ordering::Release);
        WORKER_RUNNING.store(true, Ordering::Release);

        // The hook thread comes up first and parks in its message loop with no
        // hook installed; the worker decides whether there should be one.
        let thread_id_atomic = Arc::new(AtomicU32::new(0));
        let thread_id_clone = Arc::clone(&thread_id_atomic);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let join = std::thread::Builder::new()
            .name("tm-hotkey-hook".into())
            .spawn(move || hook_thread(&thread_id_clone, &ready_tx));
        let Ok(join) = join else {
            teardown_events(event, registry_event);
            return None;
        };
        if ready_rx.recv().is_err() {
            let _ = join.join();
            teardown_events(event, registry_event);
            return None;
        }
        let hook_thread_id = thread_id_atomic.load(Ordering::Acquire);

        // The raw values, not the globals: teardown clears `TRIGGER_EVENT`, and
        // a worker that had not read it yet would then wait on a null handle
        // and spin on `WAIT_FAILED` instead of blocking. `HANDLE` is not
        // `Send`, so the isizes cross the thread boundary and are rebuilt on
        // the far side.
        let event_raw = event.0 as isize;
        let registry_raw = registry_event.0 as isize;
        let worker_join = std::thread::Builder::new()
            .name("tm-hotkey-worker".into())
            .spawn(move || worker_thread(event_raw, registry_raw, hook_thread_id));
        let Ok(worker_join) = worker_join else {
            stop_hook_thread(hook_thread_id, Some(join));
            teardown_events(event, registry_event);
            return None;
        };

        Some(HotkeyHook {
            hook_thread_id,
            join: Some(join),
            worker_join: Some(worker_join),
            event,
            registry_event,
        })
    }
}

/// Owns the `WH_KEYBOARD_LL` registration and the message loop that serves it.
///
/// Installing and removing the hook both happen here because a low-level hook
/// is bound to the thread that installed it: that thread has to be pumping
/// messages for Windows to deliver callbacks to it at all.
fn hook_thread(thread_id: &AtomicU32, ready: &std::sync::mpsc::Sender<()>) {
    // Elevate hook thread priority so scheduling delays never lag system keyboard input.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    opt_out_of_power_throttling();

    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    thread_id.store(tid, Ordering::Release);
    // Force the queue into existence before anyone posts to it: a
    // `PostThreadMessageW` that arrives before the thread's first message call
    // is discarded, which would lose the very first install request.
    let mut msg = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut msg, None, WM_APP, WM_APP, PM_NOREMOVE);
    }
    let _ = ready.send(());

    let mut hook: Option<HHOOK> = None;
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        if msg.hwnd.0.is_null() && msg.message == WM_SET_HOOK {
            set_hook(&mut hook, msg.wParam.0 != 0);
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    set_hook(&mut hook, false);
}

/// Take this thread out of Windows' managed power throttling (EcoQoS).
///
/// Priority and quality of service are different knobs, and only one of them
/// was being set. Windows applies EcoQoS to a process it considers entirely
/// background — which is exactly what a task manager parked in the
/// notification area is — and a throttled thread is scheduled on an
/// efficiency core at reduced frequency. `THREAD_PRIORITY_HIGHEST` does not
/// change that: it decides WHICH runnable thread runs, not how fast the core
/// underneath it is clocked.
///
/// For any other thread that would be the correct trade: the sampler is
/// deliberately pushed the other way (see `win::set_sampler_background`).
/// This one thread is different because it is not doing TaskMan's work — it
/// is on the path of every keystroke on the desktop, and throttling it
/// delays other applications' input, not ours. Scoped to the thread on
/// purpose: opting the whole PROCESS out would undo the sampler's background
/// mode and hand a tray icon full-speed cores again.
fn opt_out_of_power_throttling() {
    let state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        // Take explicit control of execution speed, and set it to "not
        // throttled": a zero StateMask under a set ControlMask is the
        // documented opt-out, as opposed to a zero ControlMask, which means
        // "let Windows decide" and is what this thread had before.
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    let result = unsafe {
        SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            std::ptr::from_ref(&state).cast(),
            std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if let Err(error) = result {
        // Not fatal, and not worth a warning: on a machine with no throttling
        // policy to opt out of there is nothing lost.
        tracing::debug!(%error, "hook thread could not opt out of power throttling");
    }
}

/// Install or remove the keyboard hook, idempotently.
fn set_hook(hook: &mut Option<HHOOK>, wanted: bool) {
    match (wanted, hook.take()) {
        (true, Some(existing)) => *hook = Some(existing),
        (true, None) => {
            // Logged BEFORE the hook exists, never after. Everything this
            // thread does between `SetWindowsHookExW` succeeding and its next
            // `GetMessageW` is time no keystroke on the desktop can be
            // delivered in, and `tracing` is the one call here that takes
            // locks and can reach a file. The success case therefore says
            // nothing more; this line and the warning below are an
            // unambiguous pair.
            tracing::info!("installing WH_KEYBOARD_LL hook for Ctrl+Shift+Esc");
            match unsafe {
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), None, 0)
            } {
                Ok(installed) => *hook = Some(installed),
                Err(error) => {
                    // Safe to log: no hook was installed, so this thread is
                    // not on anybody's input path while it runs.
                    tracing::warn!(error = %error, "failed to install WH_KEYBOARD_LL hook")
                }
            }
        }
        (false, Some(existing)) => {
            unsafe {
                let _ = UnhookWindowsHookEx(existing);
            }
            // Nothing can consume a keydown any more, so a keyup that arrives
            // after this must pass through instead of being swallowed.
            CONSUMED_DOWN.store(false, Ordering::SeqCst);
            LAST_DOWN_TICK.store(0, Ordering::Relaxed);
            // Safe to log here: the hook is already gone, so this thread is
            // off the input path before `tracing` can take a lock.
            tracing::info!("WH_KEYBOARD_LL hook removed");
        }
        (false, None) => {}
    }
}

/// Serves the hook: raises the window when the combo fires, and keeps the
/// hook's installed state in step with the IFEO registration.
///
/// This thread pumps messages. It owns no window and dispatches nothing of its
/// own, but [`super::window_chrome::force_foreground`] attaches this thread's
/// input queue to the foreground application's, and a thread that does not
/// pump while two input queues are merged is exactly how the OTHER application
/// ends up unable to process input.
fn worker_thread(event_raw: isize, registry_raw: isize, hook_thread_id: u32) {
    let event = HANDLE(event_raw as *mut core::ffi::c_void);
    let registry_event = HANDLE(registry_raw as *mut core::ffi::c_void);
    let watch = super::taskmgr_replacement::ReplacementWatch::open();
    let mut armed = watch
        .as_ref()
        .is_some_and(|watch| watch.arm(registry_event));
    let mut installed = false;

    // Apply the current registration before waiting for a change to it.
    sync_hook_state(hook_thread_id, &mut installed);

    while WORKER_RUNNING.load(Ordering::Acquire) {
        let timeout = if armed { u32::MAX } else { UNWATCHED_POLL_MS };
        let waited = unsafe {
            MsgWaitForMultipleObjects(Some(&[event, registry_event]), false, timeout, QS_ALLINPUT)
        };
        if !WORKER_RUNNING.load(Ordering::Acquire) {
            break;
        }
        // Four distinct wake-ups share one wait, so they are separated by
        // index rather than lumped together: a stray message must not be read
        // as "the registration changed" and send this thread to the registry.
        const TRIGGER: u32 = WAIT_OBJECT_0.0;
        const REGISTRY: u32 = WAIT_OBJECT_0.0 + 1;
        const MESSAGES: u32 = WAIT_OBJECT_0.0 + 2;
        const WAIT_TIMEOUT_CODE: u32 = windows::Win32::Foundation::WAIT_TIMEOUT.0;
        match waited.0 {
            TRIGGER => {
                let cb_opt = tm_core::sync::lock(&HOOK_CALLBACK).clone();
                if let Some(cb) = cb_opt {
                    cb();
                }
                let hwnd = super::instance::published_window();
                if hwnd != 0 {
                    // The attaching variant: this is the fullscreen-game case
                    // the hook exists for, and this thread pumps its own
                    // queue, which is what makes merging input queues safe
                    // here and nowhere else.
                    super::window_chrome::force_foreground_attached(hwnd);
                }
            }
            REGISTRY => {
                // One-shot: re-arm before reading, so a second change during
                // the read is still noticed.
                armed = watch
                    .as_ref()
                    .is_some_and(|watch| watch.arm(registry_event));
                sync_hook_state(hook_thread_id, &mut installed);
            }
            // Messages only: nothing to decide, `drain_messages` handles it.
            MESSAGES => {}
            // The fallback deadline for a worker that could not arm a watch.
            WAIT_TIMEOUT_CODE => sync_hook_state(hook_thread_id, &mut installed),
            // A wait that cannot succeed will not start succeeding, and
            // re-entering it would spin this thread on the registry. Give the
            // hotkey up instead: the hook itself stays as it is, and Explorer
            // still serves Ctrl+Shift+Esc everywhere except fullscreen.
            other => {
                tracing::warn!(status = other, "hotkey worker wait failed; giving up");
                break;
            }
        }
        drain_messages();
    }

    // Leave nothing hooked behind: the hook thread is torn down next, but the
    // ordering rules in `Drop` depend on this having already been requested.
    if installed {
        request_hook(hook_thread_id, false);
    }
}

/// Re-read the registration and tell the hook thread what it should be doing.
fn sync_hook_state(hook_thread_id: u32, installed: &mut bool) {
    let enabled = super::taskmgr_replacement::is_replacement_enabled();
    REPLACEMENT_ENABLED.store(enabled, Ordering::Release);
    if enabled == *installed {
        return;
    }
    *installed = enabled;
    request_hook(hook_thread_id, enabled);
}

fn request_hook(hook_thread_id: u32, install: bool) {
    if hook_thread_id == 0 {
        return;
    }
    unsafe {
        let _ = PostThreadMessageW(
            hook_thread_id,
            WM_SET_HOOK,
            WPARAM(usize::from(install)),
            LPARAM(0),
        );
    }
}

/// Empty this thread's message queue. Nothing here owns a window, so there is
/// nothing to dispatch to — the point is that the queue does not back up while
/// another application's input queue is attached to it.
fn drain_messages() {
    let mut msg = MSG::default();
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
        if msg.message == WM_QUIT {
            WORKER_RUNNING.store(false, Ordering::Release);
            return;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Stop the worker thread. Only ever called once no hook can signal
/// `TRIGGER_EVENT` any more.
fn stop_worker(worker_join: Option<std::thread::JoinHandle<()>>, event: HANDLE) {
    TRIGGER_EVENT.store(0, Ordering::Release);
    WORKER_RUNNING.store(false, Ordering::Release);
    let _ = unsafe { SetEvent(event) };
    if let Some(worker) = worker_join {
        let _ = worker.join();
    }
}

fn stop_hook_thread(thread_id: u32, join: Option<std::thread::JoinHandle<()>>) {
    if thread_id != 0 {
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    if let Some(join) = join {
        let _ = join.join();
    }
}

fn teardown_events(event: HANDLE, registry_event: HANDLE) {
    TRIGGER_EVENT.store(0, Ordering::Release);
    WORKER_RUNNING.store(false, Ordering::Release);
    unsafe {
        let _ = CloseHandle(event);
        let _ = CloseHandle(registry_event);
    }
}

impl Drop for HotkeyHook {
    fn drop(&mut self) {
        *tm_core::sync::lock(&HOOK_CALLBACK) = None;

        // Stop the WORKER first: it is the only thing that still reads
        // `TRIGGER_EVENT` and the registry watch, and it is what asks the hook
        // thread to unhook on its way out.
        stop_worker(self.worker_join.take(), self.event);
        // Then the hook thread, which removes the hook as it leaves its loop.
        stop_hook_thread(self.hook_thread_id, self.join.take());

        LAST_DOWN_TICK.store(0, Ordering::Relaxed);
        CONSUMED_DOWN.store(false, Ordering::SeqCst);
        REPLACEMENT_ENABLED.store(false, Ordering::Release);

        // Both threads are joined, so nothing can signal either handle now.
        unsafe {
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.registry_event);
        }
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code != HC_ACTION as i32 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // SAFETY: for `HC_ACTION` on `WH_KEYBOARD_LL` the system guarantees
    // `lparam` points at a `KBDLLHOOKSTRUCT` for the duration of this call.
    // The reference never escapes the callback, and the struct is POD.
    let kbd = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let msg = wparam.0 as u32;

    if kbd.vkCode == VK_ESCAPE.0 as u32 {
        // If Escape is being released, always clear CONSUMED_DOWN.
        // If we consumed the keydown, we must also consume the keyup so
        // the underlying window does not receive an unmatched WM_KEYUP.
        // Crucially, this must NOT depend on whether Ctrl or Shift is still
        // pressed, because users frequently release modifier keys before Escape.
        if msg == WM_KEYUP || msg == WM_SYSKEYUP {
            let down_tick = LAST_DOWN_TICK.swap(0, Ordering::Relaxed);
            let consumed = CONSUMED_DOWN.swap(false, Ordering::SeqCst);
            // ...but it MUST depend on the release still belonging to a press
            // this hook actually swallowed. A press consumed on this desktop
            // whose release lands on another one — the UAC secure desktop
            // after the hotkey elevates, the lock screen, a session switch —
            // never reaches this callback, and the flag would then be spent
            // on the next unrelated Escape release anywhere on the machine.
            // The application under that one sees Escape pressed and never
            // released. A release with no recent press is not ours.
            let matches_recent_press = down_tick != 0
                && unsafe { GetTickCount64() }.saturating_sub(down_tick) <= CONSUMED_DOWN_TTL_MS;
            if consumed && matches_recent_press {
                return LRESULT(1);
            }
        }

        // Cheap and first: a machine where the replacement is off must not pay
        // even the key-state reads. The hook is normally not installed at all
        // in that state; this still matters for the few events in flight while
        // the uninstall request is on its way to the hook thread.
        if !REPLACEMENT_ENABLED.load(Ordering::Acquire) {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        let is_ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
        let is_shift = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
        let is_alt = unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0;
        let is_win = (unsafe { GetAsyncKeyState(VK_LWIN.0 as i32) } < 0)
            || (unsafe { GetAsyncKeyState(VK_RWIN.0 as i32) } < 0);

        if is_ctrl && is_shift && !is_alt && !is_win && (msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN)
        {
            let now = unsafe { GetTickCount64() };
            let prev = LAST_DOWN_TICK.load(Ordering::Relaxed);
            let is_stale = prev == 0 || (now.saturating_sub(prev) > CONSUMED_DOWN_TTL_MS);
            LAST_DOWN_TICK.store(now, Ordering::Relaxed);

            if !CONSUMED_DOWN.swap(true, Ordering::SeqCst) || is_stale {
                // Signal the async worker thread immediately so window raising
                // and AttachThreadInput never block this hook callback or delay
                // system-wide keyboard input.
                let trigger = TRIGGER_EVENT.load(Ordering::Acquire);
                if trigger != 0 {
                    let _ = unsafe { SetEvent(HANDLE(trigger as *mut core::ffi::c_void)) };
                }
            }
            return LRESULT(1);
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The callback's state is process-global by design — it is read on the
    /// system's input path, where a lock would be a liability. Tests that
    /// drive it directly take this so they cannot interleave with each other.
    static HOOK_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serialize() -> std::sync::MutexGuard<'static, ()> {
        HOOK_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn hotkey_hook_installs_and_uninstalls_cleanly() {
        let hook = HotkeyHook::install(|| {});
        assert!(hook.is_some());
        drop(hook);
    }

    fn escape_event() -> KBDLLHOOKSTRUCT {
        KBDLLHOOKSTRUCT {
            vkCode: VK_ESCAPE.0 as u32,
            scanCode: 1,
            flags: windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT_FLAGS(0),
            time: 0,
            dwExtraInfo: 0,
        }
    }

    #[test]
    fn escape_keyup_clears_consumed_down_without_modifiers() {
        let _serial = serialize();
        CONSUMED_DOWN.store(true, Ordering::SeqCst);
        LAST_DOWN_TICK.store(unsafe { GetTickCount64() }, Ordering::Relaxed);
        let kbd = escape_event();
        let lparam = LPARAM(&kbd as *const _ as isize);
        let res =
            unsafe { low_level_keyboard_proc(HC_ACTION as i32, WPARAM(WM_KEYUP as usize), lparam) };
        assert_eq!(res, LRESULT(1));
        assert!(!CONSUMED_DOWN.load(Ordering::SeqCst));

        // Subsequent keyup when CONSUMED_DOWN is already false passes through (returns 0)
        let res =
            unsafe { low_level_keyboard_proc(HC_ACTION as i32, WPARAM(WM_KEYUP as usize), lparam) };
        assert_eq!(res, LRESULT(0));
    }

    /// The press this flag belongs to happened on a desktop whose release
    /// this hook never sees — an elevation prompt, the lock screen, a session
    /// switch. The flag must not then be spent on somebody else's Escape:
    /// that application would see the key pressed and never released.
    #[test]
    fn escape_keyup_is_not_swallowed_for_a_press_whose_release_was_lost() {
        let _serial = serialize();
        CONSUMED_DOWN.store(true, Ordering::SeqCst);
        // Older than the hold window, which is only reachable when the
        // matching release was delivered somewhere else: auto-repeat would
        // otherwise have refreshed this while the combo was held.
        LAST_DOWN_TICK.store(
            unsafe { GetTickCount64() }.saturating_sub(CONSUMED_DOWN_TTL_MS + 1),
            Ordering::Relaxed,
        );
        let kbd = escape_event();
        let lparam = LPARAM(&kbd as *const _ as isize);
        let res =
            unsafe { low_level_keyboard_proc(HC_ACTION as i32, WPARAM(WM_KEYUP as usize), lparam) };
        assert_eq!(
            res,
            LRESULT(0),
            "an unrelated Escape release must pass through"
        );
        assert!(
            !CONSUMED_DOWN.load(Ordering::SeqCst),
            "the stale flag must be cleared, not left to eat the next release too"
        );
    }

    /// The callback must never reach the registry, so the answer it uses is an
    /// atomic the worker maintains. With the replacement off, Escape has to
    /// pass straight through no matter which modifiers are down — otherwise a
    /// stale hook would eat the key for every application on the desktop.
    #[test]
    fn escape_passes_through_while_the_replacement_is_off() {
        let _serial = serialize();
        CONSUMED_DOWN.store(false, Ordering::SeqCst);
        REPLACEMENT_ENABLED.store(false, Ordering::Release);
        let kbd = escape_event();
        let lparam = LPARAM(&kbd as *const _ as isize);
        let res = unsafe {
            low_level_keyboard_proc(HC_ACTION as i32, WPARAM(WM_KEYDOWN as usize), lparam)
        };
        assert_eq!(res, LRESULT(0), "Escape must not be swallowed");
        assert!(!CONSUMED_DOWN.load(Ordering::SeqCst));
    }
}

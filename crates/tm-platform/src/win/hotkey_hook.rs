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
//!   syscall beyond the key-state reads: it consults a few atomics, signals an
//!   event and returns. Everything the hotkey actually DOES — waking the UI,
//!   taking the foreground over a fullscreen game — happens on the worker
//!   thread, which pumps its own message queue precisely so that attaching to
//!   another application's input queue cannot stall that application. It is
//!   the only caller in the codebase allowed to use
//!   [`super::window_chrome::force_foreground_attached`]. The one exception
//!   is described under "Staying at the head of the chain" below: on the
//!   keystroke that completes a fresh Ctrl+Shift chord the callback installs
//!   a new copy of the hook and posts one message.
//! * **The combo is only swallowed when something will answer it.** Before
//!   the window exists, while the worker is still serving the previous press
//!   (it waits for the UI to confirm the window is up), and after the UI
//!   failed to answer a press, the combo passes through to Explorer instead:
//!   the IFEO registration then starts a launcher, which finds this instance
//!   unresponsive and opens a fresh one. Eating the combo in those states
//!   would leave the user with no task manager at all, at exactly the moment
//!   one is needed. A press that WAS swallowed and then not answered gets a
//!   fresh task manager from the worker itself.
//!
//! ## Staying at the head of the chain
//!
//! Windows calls the most recently installed `WH_KEYBOARD_LL` hook first, and
//! a hook that returns non-zero without calling `CallNextHookEx` hides the key
//! from every hook installed before it — and from the shell's hotkeys, which
//! are processed after all of them. TaskMan's hook is installed at logon, so
//! nearly every other hook on the desktop is newer. Games built on the
//! "disable shortcut keys" pattern swallow Escape whenever Ctrl is held to
//! block Ctrl+Esc, which takes Ctrl+Shift+Esc with it: DOOM Eternal does
//! exactly that (its `win_notaskkeys` hook, installed once at startup and
//! active even while the game is in the background), and the hotkey then
//! does nothing at all.
//!
//! The hook therefore re-installs itself at the head of the chain at the one
//! moment that decides the outcome: the keydown that completes a fresh
//! Ctrl+Shift chord. Both modifiers have to be separate, earlier events for
//! the combo to match at all, and the callback runs synchronously inside the
//! second one, so the Escape that follows is guaranteed to reach this hook
//! first — whenever the other application installed its own. Auto-repeat and
//! keys typed while the chord is held do not move the hook again, and the
//! superseded copy is removed by the message loop right after the callback
//! returns. Everything except Ctrl+Shift+Esc is still passed on unchanged, so
//! the other application keeps blocking whatever it blocks.
//!
//! A launch that hands its request to this instance moves the hook to the
//! head too. That is the one signal that a Task Manager entry point reached
//! the shell instead of this hook, and re-installing also restores a hook
//! Windows removed after a `LowLevelHooksTimeout` miss, which it does
//! silently.
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
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, GetCurrentThreadId, ResetEvent, SetEvent, SetThreadInformation,
    SetThreadPriority, THREAD_POWER_THROTTLING_CURRENT_VERSION,
    THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE,
    THREAD_PRIORITY_HIGHEST, ThreadPowerThrottling,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_RCONTROL, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW,
    HC_ACTION, HHOOK, IsIconic, KBDLLHOOKSTRUCT, MSG, MsgWaitForMultipleObjects, PM_NOREMOVE,
    PM_REMOVE, PeekMessageW, PostThreadMessageW, QS_ALLINPUT, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

static CONSUMED_DOWN: AtomicBool = AtomicBool::new(false);
static LAST_DOWN_TICK: AtomicU64 = AtomicU64::new(0);
static TRIGGER_EVENT: AtomicIsize = AtomicIsize::new(0);
static WORKER_RUNNING: AtomicBool = AtomicBool::new(false);
/// Whether TaskMan is the registered Task Manager replacement.
///
/// A plain atomic because the callback runs on the system's input path: the
/// registry read that answers this question lives on the worker thread,
/// which re-runs it when the IFEO key actually changes.
static REPLACEMENT_ENABLED: AtomicBool = AtomicBool::new(false);
/// The hook `tm-hotkey-hook` installed most recently, 0 while there is none.
///
/// Only ever written on that thread — the callback runs there too. Each step
/// is published only once it is complete (a hook is removed before its handle
/// is cleared, the copy to retire is recorded before the fresh one), so an
/// observer on another thread — the tests — never sees a step half done.
static CURRENT_HOOK: AtomicIsize = AtomicIsize::new(0);
/// A copy of the hook superseded by a newer one at the head of the chain,
/// waiting for the message loop to remove it. While it is set no further move
/// starts, so a key event that reaches both copies cannot move twice.
static RETIRING_HOOK: AtomicIsize = AtomicIsize::new(0);
/// How often the hook re-installed itself at the head of the chain; reported
/// with every press the worker serves.
static MOVES_TO_FRONT: AtomicU64 = AtomicU64::new(0);
/// `tm-hotkey-hook`'s thread id while it runs, 0 otherwise. Cleared before
/// the thread exits, so a request can never be posted to a recycled id.
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// Signaled by the UI (through [`notify_shown`]) once it has shown the window
/// for a show request. The worker waits on it after handing over a press.
static SHOWN_EVENT: AtomicIsize = AtomicIsize::new(0);
/// The UI did not answer the last press it was handed. While set the combo
/// passes through to Explorer; the next window the UI shows clears it.
static UI_STALLED: AtomicBool = AtomicBool::new(false);
/// TaskMan's main window once it exists, 0 before and after.
///
/// Published through [`set_target_window`]. The callback reads it as "is
/// there anything to raise yet", the worker as the window to raise.
static TARGET_WINDOW: AtomicIsize = AtomicIsize::new(0);
/// Set by the callback when it hands a press to the worker, cleared by the
/// worker once that press has been served — the UI confirmed the window, or
/// the wait for it ended. While it is set a NEW press is not swallowed; see
/// the module docs.
static WORKER_BUSY: AtomicBool = AtomicBool::new(false);

static HOOK_CALLBACK: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>> =
    std::sync::Mutex::new(None);

/// Ask the hook thread to install (`wparam == 1`) or remove the keyboard hook.
/// A thread message, so it carries no window and never reaches a window proc.
const WM_SET_HOOK: u32 = WM_APP + 1;
/// Remove the copy of the hook in [`RETIRING_HOOK`]. Posted by the hook
/// thread to itself by the move that superseded it — usually from inside that
/// copy's own callback, which must not remove itself.
const WM_RETIRE_HOOK: u32 = WM_APP + 2;
/// Re-install the hook at the head of the chain (see the module docs).
const WM_MOVE_TO_FRONT: u32 = WM_APP + 3;

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
    events: Events,
}

/// The three auto-reset events the hook and its worker share.
#[derive(Clone, Copy)]
struct Events {
    /// A press handed to the worker (or the worker being stopped).
    trigger: HANDLE,
    /// The IFEO registration changed.
    registry: HANDLE,
    /// The UI confirmed it showed the window ([`SHOWN_EVENT`]).
    shown: HANDLE,
}

impl Events {
    fn create() -> Option<Self> {
        let create = || unsafe { CreateEventW(None, false, false, None) }.ok();
        let trigger = create()?;
        let Some(registry) = create() else {
            close_handles(&[trigger]);
            return None;
        };
        let Some(shown) = create() else {
            close_handles(&[trigger, registry]);
            return None;
        };
        Some(Self {
            trigger,
            registry,
            shown,
        })
    }

    fn close(self) {
        close_handles(&[self.trigger, self.registry, self.shown]);
    }
}

fn close_handles(handles: &[HANDLE]) {
    for handle in handles {
        unsafe {
            let _ = CloseHandle(*handle);
        }
    }
}

impl HotkeyHook {
    pub fn install<F>(on_hotkey: F) -> Option<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        *tm_core::sync::lock(&HOOK_CALLBACK) = Some(Arc::new(on_hotkey));

        let events = Events::create()?;
        WORKER_BUSY.store(false, Ordering::Release);
        UI_STALLED.store(false, Ordering::Release);
        TRIGGER_EVENT.store(events.trigger.0 as isize, Ordering::Release);
        SHOWN_EVENT.store(events.shown.0 as isize, Ordering::Release);
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
            teardown_events(events);
            return None;
        };
        if ready_rx.recv().is_err() {
            let _ = join.join();
            teardown_events(events);
            return None;
        }
        let hook_thread_id = thread_id_atomic.load(Ordering::Acquire);
        HOOK_THREAD_ID.store(hook_thread_id, Ordering::Release);

        // The raw values, not the globals: teardown clears `TRIGGER_EVENT`, and
        // a worker that had not read it yet would then wait on a null handle
        // and spin on `WAIT_FAILED` instead of blocking. `HANDLE` is not
        // `Send`, so the isizes cross the thread boundary and are rebuilt on
        // the far side.
        let raw = [
            events.trigger.0 as isize,
            events.registry.0 as isize,
            events.shown.0 as isize,
        ];
        let worker_join = std::thread::Builder::new()
            .name("tm-hotkey-worker".into())
            .spawn(move || worker_thread(raw, hook_thread_id));
        let Ok(worker_join) = worker_join else {
            HOOK_THREAD_ID.store(0, Ordering::Release);
            stop_hook_thread(hook_thread_id, Some(join));
            teardown_events(events);
            return None;
        };

        Some(HotkeyHook {
            hook_thread_id,
            join: Some(join),
            worker_join: Some(worker_join),
            events,
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

    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        if msg.hwnd.0.is_null() {
            match msg.message {
                WM_SET_HOOK => {
                    set_hook(msg.wParam.0 != 0);
                    continue;
                }
                WM_RETIRE_HOOK => {
                    finish_move_to_front();
                    continue;
                }
                WM_MOVE_TO_FRONT => {
                    finish_move_to_front();
                    begin_move_to_front();
                    continue;
                }
                _ => {}
            }
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    set_hook(false);
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
fn set_hook(wanted: bool) {
    match (wanted, CURRENT_HOOK.load(Ordering::Acquire) != 0) {
        (true, true) | (false, false) => {}
        (true, false) => {
            // Logged BEFORE the hook exists, never after. Everything this
            // thread does between `SetWindowsHookExW` succeeding and its next
            // `GetMessageW` is time no keystroke on the desktop can be
            // delivered in, and `tracing` is the one call here that takes
            // locks and can reach a file. The success case therefore says
            // nothing more; this line and the warning below are an
            // unambiguous pair.
            tracing::info!("installing WH_KEYBOARD_LL hook for Ctrl+Shift+Esc");
            match install_hook() {
                Ok(installed) => CURRENT_HOOK.store(installed.0 as isize, Ordering::Release),
                Err(error) => {
                    // Safe to log: no hook was installed, so this thread is
                    // not on anybody's input path while it runs.
                    tracing::warn!(error = %error, "failed to install WH_KEYBOARD_LL hook")
                }
            }
        }
        (false, true) => {
            // A copy superseded at the head of the chain may still be waiting
            // for its removal message; it goes too.
            finish_move_to_front();
            unhook(CURRENT_HOOK.load(Ordering::Acquire));
            CURRENT_HOOK.store(0, Ordering::Release);
            // Nothing can consume a keydown any more, so a keyup that arrives
            // after this must pass through instead of being swallowed.
            CONSUMED_DOWN.store(false, Ordering::SeqCst);
            LAST_DOWN_TICK.store(0, Ordering::Relaxed);
            // Safe to log here: the hook is already gone, so this thread is
            // off the input path before `tracing` can take a lock.
            tracing::info!("WH_KEYBOARD_LL hook removed");
        }
    }
}

fn install_hook() -> windows::core::Result<HHOOK> {
    unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), None, 0) }
}

fn unhook(raw: isize) {
    if raw != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(raw as *mut core::ffi::c_void));
        }
    }
}

/// Install a fresh copy of the hook at the head of the chain and schedule the
/// copy it supersedes for removal.
///
/// Runs on `tm-hotkey-hook` only: from inside the callback when a Ctrl+Shift
/// chord forms, and from the message loop for [`request_move_to_front`]. The
/// fresh copy is installed BEFORE the old one goes, so there is never a moment
/// without a hook, and the old one is removed by the loop rather than here:
/// the callback that asks for the move is that old copy's own invocation.
/// Logs nothing — a hook is installed throughout (see the module docs).
fn begin_move_to_front() {
    let current = CURRENT_HOOK.load(Ordering::Acquire);
    if current == 0 || RETIRING_HOOK.load(Ordering::Acquire) != 0 {
        return;
    }
    let Ok(fresh) = install_hook() else {
        // The old copy keeps serving; it is merely not first.
        return;
    };
    RETIRING_HOOK.store(current, Ordering::Release);
    CURRENT_HOOK.store(fresh.0 as isize, Ordering::Release);
    let posted =
        unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_RETIRE_HOOK, WPARAM(0), LPARAM(0)) };
    if posted.is_err() {
        // Nothing would ever remove the old copy, and two copies would see
        // every key twice for the rest of the session. Undo instead.
        unhook(fresh.0 as isize);
        CURRENT_HOOK.store(current, Ordering::Release);
        RETIRING_HOOK.store(0, Ordering::Release);
        return;
    }
    MOVES_TO_FRONT.fetch_add(1, Ordering::Relaxed);
}

/// Remove the copy [`begin_move_to_front`] superseded, if one is waiting.
fn finish_move_to_front() {
    unhook(RETIRING_HOOK.load(Ordering::Acquire));
    RETIRING_HOOK.store(0, Ordering::Release);
}

/// Ask the hook thread to move the hook to the head of the chain.
///
/// Called when another launch hands its request to this instance. Such a
/// launch is the one signal that a Task Manager entry point reached the shell
/// instead of this hook, and re-installing is also the only way to restore a
/// hook Windows removed silently after a `LowLevelHooksTimeout` miss. A no-op
/// while no hook is installed.
pub(crate) fn request_move_to_front() {
    let thread_id = HOOK_THREAD_ID.load(Ordering::Acquire);
    if thread_id == 0 {
        return;
    }
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_MOVE_TO_FRONT, WPARAM(0), LPARAM(0));
    }
}

/// The UI has shown its window for a show request. Called on the UI thread
/// (through `instance::acknowledge_show`), for every show, whichever path
/// asked for it.
pub(crate) fn notify_shown() {
    // A window that shows is a UI that answers: swallowing may resume.
    UI_STALLED.store(false, Ordering::Release);
    let shown = SHOWN_EVENT.load(Ordering::Acquire);
    if shown != 0 {
        unsafe {
            let _ = SetEvent(HANDLE(shown as *mut core::ffi::c_void));
        }
    }
}

/// Tell the hook which window the hotkey raises; 0 when there is none.
///
/// Until this names a window, Ctrl+Shift+Esc passes through to Explorer: the
/// UI that would answer the press does not exist yet.
pub(crate) fn set_target_window(hwnd: isize) {
    TARGET_WINDOW.store(hwnd, Ordering::Release);
}

/// Serves the hook: raises the window when the combo fires, and keeps the
/// hook's installed state in step with the IFEO registration.
///
/// This thread pumps messages. It owns no window and dispatches nothing of its
/// own, but [`super::window_chrome::force_foreground_attached`] attaches this
/// thread's input queue to the foreground application's, and a thread that
/// does not pump while two input queues are merged is exactly how the OTHER
/// application ends up unable to process input.
fn worker_thread(raw: [isize; 3], hook_thread_id: u32) {
    let [event, registry_event, shown_event] =
        raw.map(|handle| HANDLE(handle as *mut core::ffi::c_void));
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
                serve_press(event, shown_event);
                // Served: the next press may be swallowed again.
                WORKER_BUSY.store(false, Ordering::Release);
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

/// What became of a press after it was handed to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShowOutcome {
    /// The UI confirmed the window is up.
    Shown,
    /// No confirmation in time, but the UI still pumps messages: it is busy,
    /// not wedged, and will get to the request.
    Busy,
    /// The UI does not pump messages, or never answered.
    Stalled,
    /// The worker is being stopped or the window is going away.
    Abandoned,
}

/// Serve one swallowed press: wake the UI, take the foreground for it, and
/// wait — pumping — until the UI confirms the window is up.
///
/// The UI thread shows, restores and raises its own window; this thread only
/// takes the foreground for it, the one part the UI cannot do over a
/// fullscreen game. It does so once the UI has confirmed the window is shown
/// and restored: a minimized window cannot usefully be activated (see
/// `force_foreground_attached`), and the UI's own activation after restoring
/// it carries no right to the foreground. A window hidden in the tray is
/// activated up front as well, as it always was; for it the second call finds
/// TaskMan already in front and merges nothing.
///
/// The wait is also what keeps a swallowed press from vanishing: the press
/// never reached Explorer, so if the UI cannot answer it nobody else will.
/// While the wait runs [`WORKER_BUSY`] stays set and further presses pass
/// through to Explorer; a UI that turns out to be wedged gets a fresh task
/// manager started in its place, and the combo keeps passing through until
/// the UI shows a window again.
fn serve_press(trigger: HANDLE, shown: HANDLE) {
    use super::window_chrome::force_foreground_attached;

    let started = unsafe { GetTickCount64() };
    // Drop a confirmation left over from a show nobody waited for (a tray
    // click, a launch) before asking for this one.
    unsafe {
        let _ = ResetEvent(shown);
    }
    let cb_opt = tm_core::sync::lock(&HOOK_CALLBACK).clone();
    if let Some(cb) = cb_opt {
        cb();
    }
    let hwnd = TARGET_WINDOW.load(Ordering::Acquire);
    if hwnd != 0 {
        // The attaching variant: this is the fullscreen-game case the hook
        // exists for, and this thread pumps its own queue, which is what
        // makes merging input queues safe here and nowhere else. Refused for
        // a minimized window; that one is activated after the restore below.
        force_foreground_attached(hwnd);
    }
    let outcome = await_shown(hwnd, trigger, shown);
    let activated = matches!(outcome, ShowOutcome::Shown | ShowOutcome::Busy)
        && hwnd != 0
        && TARGET_WINDOW.load(Ordering::Acquire) == hwnd
        && force_foreground_attached(hwnd);
    let waited_ms = unsafe { GetTickCount64() }.saturating_sub(started);
    let moves_to_front = MOVES_TO_FRONT.load(Ordering::Relaxed);
    // What the user actually got, for the one case that cannot be checked
    // headlessly: a window in front, or one Windows kept behind the game.
    let window = HWND(hwnd as *mut core::ffi::c_void);
    let in_front = hwnd != 0 && unsafe { GetForegroundWindow() } == window;
    let minimized = hwnd != 0 && unsafe { IsIconic(window) }.as_bool();
    match outcome {
        ShowOutcome::Shown => {
            tracing::info!(
                waited_ms,
                moves_to_front,
                activated,
                in_front,
                minimized,
                "Ctrl+Shift+Esc answered by the hook"
            );
        }
        ShowOutcome::Busy => tracing::info!(
            waited_ms,
            moves_to_front,
            activated,
            in_front,
            minimized,
            "Ctrl+Shift+Esc answered by the hook; the UI is busy and shows the window late"
        ),
        ShowOutcome::Stalled => {
            UI_STALLED.store(true, Ordering::Release);
            tracing::warn!(
                waited_ms,
                "Ctrl+Shift+Esc: the UI does not answer; passing the hotkey to Explorer \
                 and starting a fresh task manager"
            );
            start_fallback_instance();
        }
        ShowOutcome::Abandoned => {}
    }
}

/// Wait for the UI to confirm a show, in the same stages a launch waits for
/// a running instance (`instance::handshake`): a short wait, then a
/// responsiveness probe, then a longer wait only when the probe cannot tell.
/// The same deadlines on purpose — a press must not be given up on sooner
/// than a launch would give up on the same instance.
fn await_shown(hwnd: isize, trigger: HANDLE, shown: HANDLE) -> ShowOutcome {
    use super::instance::{EXTENDED_ACK_MS, FIRST_ACK_MS};

    // Still the press this worker was handed, for a window that still exists?
    let still_wanted =
        || WORKER_RUNNING.load(Ordering::Acquire) && TARGET_WINDOW.load(Ordering::Acquire) == hwnd;
    match wait_for_shown(trigger, shown, FIRST_ACK_MS) {
        Wake::Shown => return ShowOutcome::Shown,
        Wake::Stop => return ShowOutcome::Abandoned,
        Wake::Timeout => {}
    }
    if hwnd == 0 || !still_wanted() {
        return ShowOutcome::Abandoned;
    }
    let outcome = match super::instance::window_responds(hwnd) {
        Some(true) => ShowOutcome::Busy,
        Some(false) => ShowOutcome::Stalled,
        None => match wait_for_shown(trigger, shown, EXTENDED_ACK_MS) {
            Wake::Shown => ShowOutcome::Shown,
            Wake::Stop => ShowOutcome::Abandoned,
            Wake::Timeout => ShowOutcome::Stalled,
        },
    };
    // The probe can take its whole deadline; a shutdown during it must not
    // start a second task manager on the way out.
    if outcome == ShowOutcome::Stalled && !still_wanted() {
        return ShowOutcome::Abandoned;
    }
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    Shown,
    Stop,
    Timeout,
}

/// Wait up to `timeout_ms` for `shown`, pumping this thread's queue as the
/// main wait does. `trigger` is part of the wait because it is how the worker
/// is stopped: while a press is being served the callback never signals it
/// ([`claim_worker`] refuses while [`WORKER_BUSY`] is set).
fn wait_for_shown(trigger: HANDLE, shown: HANDLE, timeout_ms: u32) -> Wake {
    const SHOWN: u32 = WAIT_OBJECT_0.0;
    const STOP: u32 = WAIT_OBJECT_0.0 + 1;
    const MESSAGES: u32 = WAIT_OBJECT_0.0 + 2;
    let deadline = unsafe { GetTickCount64() } + u64::from(timeout_ms);
    loop {
        let remaining = deadline.saturating_sub(unsafe { GetTickCount64() });
        let waited = unsafe {
            MsgWaitForMultipleObjects(
                Some(&[shown, trigger]),
                false,
                u32::try_from(remaining).unwrap_or(timeout_ms),
                QS_ALLINPUT,
            )
        };
        match waited.0 {
            SHOWN => return Wake::Shown,
            STOP => return Wake::Stop,
            MESSAGES => {
                drain_messages();
                if !WORKER_RUNNING.load(Ordering::Acquire) {
                    return Wake::Stop;
                }
            }
            // A timeout, or a wait that failed: either way no confirmation.
            _ => return Wake::Timeout,
        }
    }
}

/// Start a fresh task manager for a press the running UI could not answer —
/// the same answer the IFEO launcher gives when its handshake finds this
/// instance unresponsive, and through the same flag, so it starts at once
/// and uncoordinated (no second keyboard hook).
fn start_fallback_instance() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::warn!(%error, "cannot locate the executable for a fresh task manager");
            return;
        }
    };
    match std::process::Command::new(&exe)
        .arg(super::instance::UNRESPONSIVE_ARG)
        .spawn()
    {
        Ok(child) => {
            // Only succeeds while this process may take the foreground itself
            // (TaskMan's own window was in front); otherwise Windows decides,
            // as it does for any launch from a background process.
            let _ = unsafe { AllowSetForegroundWindow(child.id()) };
            tracing::info!(pid = child.id(), "started a fresh task manager");
        }
        Err(error) => tracing::warn!(%error, "could not start a fresh task manager"),
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
    WORKER_BUSY.store(false, Ordering::Release);
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

fn teardown_events(events: Events) {
    TRIGGER_EVENT.store(0, Ordering::Release);
    SHOWN_EVENT.store(0, Ordering::Release);
    WORKER_RUNNING.store(false, Ordering::Release);
    WORKER_BUSY.store(false, Ordering::Release);
    UI_STALLED.store(false, Ordering::Release);
    events.close();
}

impl Drop for HotkeyHook {
    fn drop(&mut self) {
        *tm_core::sync::lock(&HOOK_CALLBACK) = None;

        // Stop the WORKER first: it is the only thing that still reads
        // `TRIGGER_EVENT` and the registry watch, and it is what asks the hook
        // thread to unhook on its way out.
        stop_worker(self.worker_join.take(), self.events.trigger);
        // Then the hook thread, which removes the hook as it leaves its loop.
        // Nobody may post to its id once it is gone and possibly recycled.
        HOOK_THREAD_ID.store(0, Ordering::Release);
        stop_hook_thread(self.hook_thread_id, self.join.take());

        LAST_DOWN_TICK.store(0, Ordering::Relaxed);
        CONSUMED_DOWN.store(false, Ordering::SeqCst);
        REPLACEMENT_ENABLED.store(false, Ordering::Release);

        // Both threads are joined, so nothing but `notify_shown` can signal a
        // handle now — and that runs on the UI thread, which is this one.
        teardown_events(self.events);
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

    if (msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN) && combo_could_be_swallowed() {
        // The key-state reads see the state BEFORE this event (see
        // `completes_chord`), which is what tells a fresh press from
        // auto-repeat.
        if completes_chord(kbd.vkCode, |key| {
            (unsafe { GetAsyncKeyState(i32::from(key.0)) }) < 0
        }) {
            begin_move_to_front();
        }
    }

    if kbd.vkCode == VK_ESCAPE.0 as u32 {
        if msg == WM_KEYUP || msg == WM_SYSKEYUP {
            // If Escape is being released, always clear CONSUMED_DOWN.
            // If we consumed the keydown, we must also consume the keyup so
            // the underlying window does not receive an unmatched WM_KEYUP.
            // Crucially, this must NOT depend on whether Ctrl or Shift is still
            // pressed, because users frequently release modifier keys before Escape.
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
        } else if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            if swallow_escape_press() {
                return LRESULT(1);
            }
            // This press reaches the application, so its release must too.
            // Without this, letting go of Ctrl+Shift while still holding
            // Escape delivers the auto-repeat presses and then eats the
            // release, leaving Escape stuck down wherever focus now is.
            CONSUMED_DOWN.store(false, Ordering::SeqCst);
            LAST_DOWN_TICK.store(0, Ordering::Relaxed);
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Decide an Escape press: `true` swallows it (and, for a new press, has
/// already handed it to the worker).
///
/// Runs on the system's input path; see the module docs for what it may do.
fn swallow_escape_press() -> bool {
    // Cheap and first: a machine where the replacement is off must not pay
    // even the key-state reads. The hook is normally not installed at all in
    // that state; this still matters for the few events in flight while the
    // uninstall request is on its way to the hook thread.
    if !REPLACEMENT_ENABLED.load(Ordering::Acquire) {
        return false;
    }

    let is_ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
    let is_shift = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
    let is_alt = unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0;
    let is_win = (unsafe { GetAsyncKeyState(VK_LWIN.0 as i32) } < 0)
        || (unsafe { GetAsyncKeyState(VK_RWIN.0 as i32) } < 0);
    if !(is_ctrl && is_shift && !is_alt && !is_win) {
        return false;
    }

    let now = unsafe { GetTickCount64() };
    if is_repeat_of_swallowed_press(
        CONSUMED_DOWN.load(Ordering::SeqCst),
        LAST_DOWN_TICK.load(Ordering::Relaxed),
        now,
    ) {
        // Auto-repeat of the press already being served: keep swallowing it
        // and keep it fresh, but do not raise the window again.
        LAST_DOWN_TICK.store(now, Ordering::Relaxed);
        return true;
    }
    if !claim_worker(TARGET_WINDOW.load(Ordering::Acquire) != 0) {
        return false;
    }
    LAST_DOWN_TICK.store(now, Ordering::Relaxed);
    CONSUMED_DOWN.store(true, Ordering::SeqCst);
    true
}

/// Whether a Ctrl+Shift+Esc press would be swallowed right now, as far as the
/// atomics can tell. Only then is it worth being first in the chain: with the
/// replacement off, no window yet, or a UI that does not answer, the combo is
/// passed on anyway.
fn combo_could_be_swallowed() -> bool {
    REPLACEMENT_ENABLED.load(Ordering::Acquire)
        && TARGET_WINDOW.load(Ordering::Acquire) != 0
        && !UI_STALLED.load(Ordering::Acquire)
}

/// Whether the keydown of `vk` completes a fresh Ctrl+Shift chord — the
/// moment the hook moves to the head of the chain.
///
/// `down` reports a key's asynchronous state, which inside a low-level hook
/// is still the state BEFORE the event being processed: Windows updates it
/// after the hook chain. So `down` on this key's own family answers "was it
/// already held", which is what separates a fresh press from auto-repeat and
/// from the second key of a pair (left Ctrl held, right Ctrl pressed).
///
/// The cheapest rejection comes first: typing a capital letter asks exactly
/// one question, and every other key asks none.
fn completes_chord(vk: u32, down: impl Fn(VIRTUAL_KEY) -> bool) -> bool {
    let is = |keys: [VIRTUAL_KEY; 3]| keys.iter().any(|key| u32::from(key.0) == vk);
    let (this, other) = if is([VK_CONTROL, VK_LCONTROL, VK_RCONTROL]) {
        (VK_CONTROL, VK_SHIFT)
    } else if is([VK_SHIFT, VK_LSHIFT, VK_RSHIFT]) {
        (VK_SHIFT, VK_CONTROL)
    } else {
        return false;
    };
    down(other) && !down(this) && !down(VK_MENU) && !down(VK_LWIN) && !down(VK_RWIN)
}

/// Whether a Ctrl+Shift+Esc press continues the one this hook last
/// swallowed. Auto-repeat refreshes the tick while the combo is held, so only
/// a press whose release this hook never saw goes stale.
fn is_repeat_of_swallowed_press(consumed_down: bool, last_down_tick: u64, now: u64) -> bool {
    consumed_down
        && last_down_tick != 0
        && now.saturating_sub(last_down_tick) <= CONSUMED_DOWN_TTL_MS
}

/// Hand a new press to the worker, if there is anything to hand it to.
///
/// `false` means "let the press through": there is no window to raise yet,
/// the hook is being torn down, the worker has not finished the previous
/// press, or the UI did not answer the last one. Only a `true` signals the
/// worker, so window raising and `AttachThreadInput` never run on — or
/// block — this callback.
fn claim_worker(target_published: bool) -> bool {
    let trigger = TRIGGER_EVENT.load(Ordering::Acquire);
    if trigger == 0 || !target_published || UI_STALLED.load(Ordering::Acquire) {
        return false;
    }
    if WORKER_BUSY.swap(true, Ordering::AcqRel) {
        return false;
    }
    if unsafe { SetEvent(HANDLE(trigger as *mut core::ffi::c_void)) }.is_err() {
        WORKER_BUSY.store(false, Ordering::Release);
        return false;
    }
    true
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
        let _serial = serialize();
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

    /// Letting go of Ctrl+Shift while still holding Escape turns the rest of
    /// the hold into plain Escape presses that DO reach the application. The
    /// release then belongs to them and must reach it too, or Escape stays
    /// stuck down there.
    #[test]
    fn escape_press_that_passes_through_releases_the_swallow() {
        let _serial = serialize();
        CONSUMED_DOWN.store(true, Ordering::SeqCst);
        LAST_DOWN_TICK.store(unsafe { GetTickCount64() }, Ordering::Relaxed);
        let kbd = escape_event();
        let lparam = LPARAM(&kbd as *const _ as isize);
        // No modifiers are held on the test machine, so this press is
        // delivered, not swallowed.
        let res = unsafe {
            low_level_keyboard_proc(HC_ACTION as i32, WPARAM(WM_KEYDOWN as usize), lparam)
        };
        assert_eq!(res, LRESULT(0));
        assert!(!CONSUMED_DOWN.load(Ordering::SeqCst));
        let res =
            unsafe { low_level_keyboard_proc(HC_ACTION as i32, WPARAM(WM_KEYUP as usize), lparam) };
        assert_eq!(
            res,
            LRESULT(0),
            "the release of a delivered press must be delivered"
        );
    }

    #[test]
    fn repeat_detection_follows_the_swallowed_press() {
        let now = 10_000;
        assert!(is_repeat_of_swallowed_press(true, now - 30, now));
        assert!(is_repeat_of_swallowed_press(
            true,
            now - CONSUMED_DOWN_TTL_MS,
            now
        ));
        assert!(!is_repeat_of_swallowed_press(
            true,
            now - CONSUMED_DOWN_TTL_MS - 1,
            now
        ));
        assert!(!is_repeat_of_swallowed_press(false, now - 30, now));
        assert!(!is_repeat_of_swallowed_press(true, 0, now));
    }

    /// A press is only swallowed when the worker will serve it. With no
    /// window yet, or the worker still stuck on the previous press, the combo
    /// must reach Explorer so the user still gets a task manager.
    #[test]
    fn combo_passes_through_when_nothing_can_answer_it() {
        let _serial = serialize();
        let event = unsafe { CreateEventW(None, false, false, None) }.expect("event");
        TRIGGER_EVENT.store(event.0 as isize, Ordering::Release);
        WORKER_BUSY.store(false, Ordering::Release);
        UI_STALLED.store(false, Ordering::Release);

        assert!(!claim_worker(false), "no window to raise yet");
        assert!(!WORKER_BUSY.load(Ordering::Acquire));

        assert!(claim_worker(true));
        assert!(WORKER_BUSY.load(Ordering::Acquire));
        assert_eq!(
            unsafe { windows::Win32::System::Threading::WaitForSingleObject(event, 0) },
            WAIT_OBJECT_0,
            "a claimed press must signal the worker"
        );
        assert!(
            !claim_worker(true),
            "a worker still serving the last press must not be handed another"
        );

        WORKER_BUSY.store(false, Ordering::Release);
        TRIGGER_EVENT.store(0, Ordering::Release);
        assert!(!claim_worker(true), "a torn-down hook claims nothing");
        unsafe {
            let _ = CloseHandle(event);
        }
    }

    /// A press the UI never answered was swallowed for nothing; until the UI
    /// shows a window again, later presses must reach Explorer, whose
    /// launcher starts a fresh task manager in place of the wedged one.
    #[test]
    fn an_unanswered_press_lets_the_combo_through_until_the_ui_answers() {
        let _serial = serialize();
        let event = unsafe { CreateEventW(None, false, false, None) }.expect("event");
        TRIGGER_EVENT.store(event.0 as isize, Ordering::Release);
        WORKER_BUSY.store(false, Ordering::Release);

        UI_STALLED.store(true, Ordering::Release);
        assert!(
            !claim_worker(true),
            "a stalled UI must not be handed presses"
        );
        assert!(!WORKER_BUSY.load(Ordering::Acquire));

        notify_shown();
        assert!(
            !UI_STALLED.load(Ordering::Acquire),
            "a shown window clears the stall"
        );
        assert!(claim_worker(true));

        WORKER_BUSY.store(false, Ordering::Release);
        TRIGGER_EVENT.store(0, Ordering::Release);
        unsafe {
            let _ = CloseHandle(event);
        }
    }

    /// The wait after a press ends the moment the UI confirms, or the moment
    /// the worker is stopped — the two wake-ups share one wait and must not
    /// be confused (a stop read as "shown" would log a press as answered).
    #[test]
    fn a_confirmed_show_or_a_stop_ends_the_wait_at_once() {
        let _serial = serialize();
        let events = Events::create().expect("events");
        let started = std::time::Instant::now();

        unsafe {
            let _ = SetEvent(events.shown);
        }
        assert_eq!(
            await_shown(0x1234, events.trigger, events.shown),
            ShowOutcome::Shown
        );
        unsafe {
            let _ = SetEvent(events.trigger);
        }
        assert_eq!(
            await_shown(0x1234, events.trigger, events.shown),
            ShowOutcome::Abandoned
        );
        assert!(
            started.elapsed()
                < std::time::Duration::from_millis(super::super::instance::FIRST_ACK_MS.into()),
            "neither wake-up may cost the acknowledgement deadline"
        );
        events.close();
    }

    /// Only the keydown that completes a fresh Ctrl+Shift chord moves the
    /// hook: auto-repeat, the other side of a held modifier, a lone modifier
    /// and chords with Alt or Win must not, and ordinary typing must not pay
    /// for the check.
    #[test]
    fn only_a_fresh_ctrl_shift_chord_moves_the_hook() {
        fn held(keys: &'static [VIRTUAL_KEY]) -> impl Fn(VIRTUAL_KEY) -> bool {
            move |key| keys.contains(&key)
        }
        let vk = |key: VIRTUAL_KEY| u32::from(key.0);

        // Either side, or the generic code `SendInput` callers may use.
        for ctrl in [VK_LCONTROL, VK_RCONTROL, VK_CONTROL] {
            assert!(completes_chord(vk(ctrl), held(&[VK_SHIFT])));
        }
        for shift in [VK_LSHIFT, VK_RSHIFT, VK_SHIFT] {
            assert!(completes_chord(vk(shift), held(&[VK_CONTROL])));
        }

        // Auto-repeat of the second key, or its other side: already held.
        assert!(!completes_chord(
            vk(VK_LSHIFT),
            held(&[VK_CONTROL, VK_SHIFT])
        ));
        assert!(!completes_chord(
            vk(VK_RCONTROL),
            held(&[VK_CONTROL, VK_SHIFT])
        ));
        // Not the combo.
        assert!(!completes_chord(vk(VK_LSHIFT), held(&[])));
        assert!(!completes_chord(
            vk(VK_LCONTROL),
            held(&[VK_SHIFT, VK_MENU])
        ));
        assert!(!completes_chord(
            vk(VK_LCONTROL),
            held(&[VK_SHIFT, VK_LWIN])
        ));
        assert!(!completes_chord(
            vk(VK_LCONTROL),
            held(&[VK_SHIFT, VK_RWIN])
        ));

        // Every other key decides without reading any key state.
        assert!(!completes_chord(vk(VK_ESCAPE), |_| -> bool {
            panic!("key state read for a non-modifier")
        }));
        // A capital letter's Shift asks one question.
        let asked = std::cell::Cell::new(0);
        assert!(!completes_chord(vk(VK_LSHIFT), |_| {
            asked.set(asked.get() + 1);
            false
        }));
        assert_eq!(asked.get(), 1);
    }

    /// A real hook, served by the real hook thread: moving it to the head of
    /// the chain must leave exactly one copy installed, and removing it must
    /// leave none.
    ///
    /// No input is injected — keys pressed here would reach whatever has the
    /// focus on the test machine — so the chain ORDER is Windows' contract;
    /// what this pins is that moves never leak a hook or lose one.
    #[test]
    fn moving_to_the_head_leaves_exactly_one_hook() {
        let _serial = serialize();
        // A session without an interactive desktop cannot install low-level
        // hooks at all; there is nothing to test there.
        match install_hook() {
            Ok(probe) => unhook(probe.0 as isize),
            Err(error) => {
                eprintln!("skipped: WH_KEYBOARD_LL unavailable here ({error})");
                return;
            }
        }
        // Pass every key straight through while the real hook is installed.
        REPLACEMENT_ENABLED.store(false, Ordering::Release);

        let wait_until = |what: &str, done: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !done() {
                assert!(std::time::Instant::now() < deadline, "timed out: {what}");
                std::thread::yield_now();
            }
        };
        let still_installed = |raw: isize| {
            // Unhooking succeeds only for a hook that still exists; for the
            // copies this test inspects that is also the cleanup.
            unsafe { UnhookWindowsHookEx(HHOOK(raw as *mut core::ffi::c_void)) }.is_ok()
        };

        let thread_id = Arc::new(AtomicU32::new(0));
        let thread_id_clone = Arc::clone(&thread_id);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || hook_thread(&thread_id_clone, &ready_tx));
        ready_rx.recv().expect("hook thread ready");
        let thread_id = thread_id.load(Ordering::Acquire);

        request_hook(thread_id, true);
        wait_until("install", &|| CURRENT_HOOK.load(Ordering::Acquire) != 0);
        let first = CURRENT_HOOK.load(Ordering::Acquire);

        unsafe {
            PostThreadMessageW(thread_id, WM_MOVE_TO_FRONT, WPARAM(0), LPARAM(0))
                .expect("post move");
        }
        wait_until("move", &|| {
            let current = CURRENT_HOOK.load(Ordering::Acquire);
            current != 0 && current != first && RETIRING_HOOK.load(Ordering::Acquire) == 0
        });
        let second = CURRENT_HOOK.load(Ordering::Acquire);
        assert!(
            !still_installed(first),
            "the superseded copy must be removed"
        );

        request_hook(thread_id, false);
        wait_until("removal", &|| CURRENT_HOOK.load(Ordering::Acquire) == 0);
        assert!(
            !still_installed(second),
            "removal must leave no copy behind"
        );

        stop_hook_thread(thread_id, Some(join));
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

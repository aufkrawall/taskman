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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, MSG,
    PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

static CONSUMED_DOWN: AtomicBool = AtomicBool::new(false);
static HOOK_CALLBACK: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>> =
    std::sync::Mutex::new(None);
static LAST_DOWN_INSTANT: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

pub struct HotkeyHook {
    thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl HotkeyHook {
    pub fn install<F>(on_hotkey: F) -> Option<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        *HOOK_CALLBACK.lock().unwrap() = Some(Arc::new(on_hotkey));

        let thread_id_atomic = Arc::new(AtomicU32::new(0));
        let thread_id_clone = Arc::clone(&thread_id_atomic);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let join = std::thread::Builder::new()
            .name("tm-hotkey-hook".into())
            .spawn(move || {
                let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
                thread_id_clone.store(tid, Ordering::Release);

                let hook = match unsafe {
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), None, 0)
                } {
                    Ok(hook) => {
                        let _ = ready_tx.send(true);
                        hook
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to install WH_KEYBOARD_LL hook");
                        let _ = ready_tx.send(false);
                        return;
                    }
                };

                let mut msg = MSG::default();
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }

                unsafe {
                    let _ = UnhookWindowsHookEx(hook);
                }
            })
            .ok()?;

        let success = ready_rx.recv().unwrap_or(false);
        if !success {
            let _ = join.join();
            return None;
        }

        Some(HotkeyHook {
            thread_id: thread_id_atomic.load(Ordering::Acquire),
            join: Some(join),
        })
    }
}

impl Drop for HotkeyHook {
    fn drop(&mut self) {
        *HOOK_CALLBACK.lock().unwrap() = None;
        *LAST_DOWN_INSTANT.lock().unwrap() = None;
        CONSUMED_DOWN.store(false, Ordering::SeqCst);
        if self.thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
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

    let kbd = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let msg = wparam.0 as u32;

    if kbd.vkCode == VK_ESCAPE.0 as u32 {
        // If Escape is being released, always clear CONSUMED_DOWN.
        // If we consumed the keydown, we must also consume the keyup so
        // the underlying window does not receive an unmatched WM_KEYUP.
        // Crucially, this must NOT depend on whether Ctrl or Shift is still
        // pressed, because users frequently release modifier keys before Escape.
        if msg == WM_KEYUP || msg == WM_SYSKEYUP {
            *LAST_DOWN_INSTANT.lock().unwrap() = None;
            if CONSUMED_DOWN.swap(false, Ordering::SeqCst) {
                return LRESULT(1);
            }
        }

        let is_ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
        let is_shift = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
        let is_alt = unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0;
        let is_win = (unsafe { GetAsyncKeyState(VK_LWIN.0 as i32) } < 0)
            || (unsafe { GetAsyncKeyState(VK_RWIN.0 as i32) } < 0);

        if is_ctrl && is_shift && !is_alt && !is_win {
            // Only intercept if TaskMan is configured as the taskmgr replacement.
            if super::taskmgr_replacement::is_replacement_enabled()
                && (msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN)
            {
                let now = std::time::Instant::now();
                let mut last = LAST_DOWN_INSTANT.lock().unwrap();
                let is_stale = last.is_some_and(|prev| {
                    now.duration_since(prev) > std::time::Duration::from_millis(1000)
                });
                *last = Some(now);

                if !CONSUMED_DOWN.swap(true, Ordering::SeqCst) || is_stale {
                    // First keydown (debounce auto-repeats):
                    if let Some(cb) = HOOK_CALLBACK.lock().unwrap().as_ref() {
                        cb();
                    }
                    let hwnd = super::instance::published_window();
                    if hwnd != 0 {
                        super::window_chrome::force_foreground(hwnd);
                    }
                }
                return LRESULT(1);
            }
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_hook_installs_and_uninstalls_cleanly() {
        let hook = HotkeyHook::install(|| {});
        assert!(hook.is_some());
        drop(hook);
    }

    #[test]
    fn escape_keyup_clears_consumed_down_without_modifiers() {
        CONSUMED_DOWN.store(true, Ordering::SeqCst);
        let kbd = KBDLLHOOKSTRUCT {
            vkCode: VK_ESCAPE.0 as u32,
            scanCode: 1,
            flags: windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT_FLAGS(0),
            time: 0,
            dwExtraInfo: 0,
        };
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
}

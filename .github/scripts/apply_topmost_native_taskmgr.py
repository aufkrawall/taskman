from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


# Windows API feature used by the event-driven strict-topmost keeper.
path = Path("crates/tm-platform/Cargo.toml")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '  "Win32_UI_Shell",\n  "Win32_UI_WindowsAndMessaging",\n',
    '  "Win32_UI_Shell",\n  "Win32_UI_Accessibility",\n  "Win32_UI_WindowsAndMessaging",\n',
    "windows accessibility feature",
)
path.write_text(text, encoding="utf-8")


# Platform action: expose an explicit escape hatch to the built-in Task Manager.
path = Path("crates/tm-platform/src/actions.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '''    fn relaunch_elevated(&self) -> Result<()> {\n        Err(tm_core::TmError::Unsupported("elevation"))\n    }\n\n    // ------------------------------------------------ shell integration\n''',
    '''    fn relaunch_elevated(&self) -> Result<()> {\n        Err(tm_core::TmError::Unsupported("elevation"))\n    }\n\n    // ------------------------------------------------ shell integration\n    /// Start the built-in OS task manager even when this application owns a\n    /// Task Manager replacement/interception registration.\n    fn launch_native_task_manager(&self) -> Result<()> {\n        Err(tm_core::TmError::Unsupported("Windows Task Manager"))\n    }\n''',
    "platform native task manager action",
)
path.write_text(text, encoding="utf-8")


# Windows implementation of the IFEO-safe native Task Manager launch.
path = Path("crates/tm-platform/src/win/taskmgr_replacement.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    'use std::path::Path;\n',
    'use std::path::{Path, PathBuf};\n',
    "taskmgr path import",
)
anchor = '''fn normalize_command(s: &str) -> String {\n    s.trim().replace('/', "\\\\").to_lowercase()\n}\n\n'''
helper = r'''fn system_task_manager_path() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = vec![0u16; 260];
    let mut len = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if len == 0 {
        return Err(TmError::platform(
            "GetSystemDirectoryW",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    if len >= buffer.len() {
        buffer.resize(len + 1, 0);
        len = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
        if len == 0 || len >= buffer.len() {
            return Err(TmError::platform(
                "GetSystemDirectoryW",
                "Windows returned an invalid system-directory length",
            ));
        }
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])).join("Taskmgr.exe"))
}

/// Launch the built-in Windows Task Manager without changing the IFEO
/// replacement registration. Windows deliberately suppresses an image's IFEO
/// debugger when a debugger creates that image itself; create taskmgr with
/// `DEBUG_ONLY_THIS_PROCESS`, then immediately detach from it. This is the
/// debugger-safe escape hatch and, unlike temporarily deleting the registry
/// value, creates no race in which Ctrl+Shift+Esc can escape TaskMan.
pub fn launch_native_task_manager() -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::Debug::DebugActiveProcessStop;
    use windows::Win32::System::Threading::{
        CreateProcessW, DEBUG_ONLY_THIS_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
        TerminateProcess,
    };
    use windows::core::{PCWSTR, PWSTR};

    let path = system_task_manager_path()?;
    let application: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // CreateProcessW is allowed to mutate its command-line buffer.
    let mut command_line = application.clone();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR::from_raw(application.as_ptr()),
            Some(PWSTR::from_raw(command_line.as_mut_ptr())),
            None,
            None,
            false,
            DEBUG_ONLY_THIS_PROCESS,
            None,
            PCWSTR::null(),
            &startup,
            &mut process,
        )
    }
    .map_err(|error| TmError::platform("CreateProcessW(Taskmgr.exe)", error.to_string()))?;

    let detached = unsafe { DebugActiveProcessStop(process.dwProcessId) };
    if let Err(error) = detached {
        // A debug-created process waits for its debugger. Never leave a stuck
        // Task Manager attached to one of the executor's long-lived workers.
        unsafe {
            let _ = TerminateProcess(process.hProcess, 1);
            let _ = CloseHandle(process.hThread);
            let _ = CloseHandle(process.hProcess);
        }
        return Err(TmError::platform(
            "DebugActiveProcessStop(Taskmgr.exe)",
            error.to_string(),
        ));
    }
    unsafe {
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

'''
text = replace_once(text, anchor, anchor + helper, "native task manager helper")
text = replace_once(
    text,
    '''    #[test]\n    fn unrelated_debugger_is_not_owned() {\n''',
    '''    #[test]\n    fn native_task_manager_path_comes_from_the_windows_system_directory() {\n        let path = system_task_manager_path().expect("Windows system directory");\n        assert!(path.is_absolute());\n        assert!(path\n            .file_name()\n            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("taskmgr.exe")));\n    }\n\n    #[test]\n    fn unrelated_debugger_is_not_owned() {\n''',
    "native task manager path test",
)
path.write_text(text, encoding="utf-8")


# Wire the Windows action implementation.
path = Path("crates/tm-platform/src/win/mod.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '''    fn relaunch_elevated(&self) -> Result<()> {\n        process_ops::relaunch_elevated()\n    }\n\n    fn task_manager_replacement_state(&self) -> TaskManagerReplacementState {\n''',
    '''    fn relaunch_elevated(&self) -> Result<()> {\n        process_ops::relaunch_elevated()\n    }\n\n    fn launch_native_task_manager(&self) -> Result<()> {\n        taskmgr_replacement::launch_native_task_manager()\n    }\n\n    fn task_manager_replacement_state(&self) -> TaskManagerReplacementState {\n''',
    "windows native task manager action",
)
path.write_text(text, encoding="utf-8")


# Native topmost keeper: reassert the window at the top of the topmost band in
# response to shell/window events rather than polling. The shell's taskbar and
# Start surfaces are also topmost windows and can otherwise be reordered above
# a normal HWND_TOPMOST window.
path = Path("crates/tm-platform/src/win/window_chrome.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '''use windows::Win32::Graphics::Dwm::{\n    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_CLOAK, DWMWA_SYSTEMBACKDROP_TYPE,\n    DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE, DwmSetWindowAttribute,\n};\n''',
    '''use windows::Win32::Graphics::Dwm::{\n    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_CLOAK, DWMWA_SYSTEMBACKDROP_TYPE,\n    DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE, DwmSetWindowAttribute,\n};\nuse windows::Win32::UI::Accessibility::{\n    HWINEVENTHOOK, SetWinEventHook, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,\n};\nuse windows::Win32::UI::WindowsAndMessaging::{\n    EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, HWND_NOTOPMOST, HWND_TOPMOST,\n    IsIconic, IsWindowVisible, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING,\n    SWP_NOSIZE, SetWindowPos,\n};\n''',
    "strict topmost imports",
)
anchor = '''/// Hide or reveal a window at the COMPOSITOR, without changing whether\n'''
helper = r'''static STRICT_TOPMOST_HWND: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
static STRICT_TOPMOST_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static STRICT_TOPMOST_HOOKS: std::sync::Once = std::sync::Once::new();

const OBJID_WINDOW_I32: i32 = 0;

fn reassert_strict_topmost() {
    use std::sync::atomic::Ordering;

    if !STRICT_TOPMOST_ENABLED.load(Ordering::Acquire) {
        return;
    }
    let raw = STRICT_TOPMOST_HWND.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut std::ffi::c_void);
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return;
        }
        // HWND_TOPMOST is also an insertion point. Reapplying it moves this
        // window to the front of the topmost band without stealing focus.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOACTIVATE
                | SWP_NOOWNERZORDER
                | SWP_NOSENDCHANGING,
        );
    }
}

unsafe extern "system" fn strict_topmost_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    _event_hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if event == EVENT_SYSTEM_FOREGROUND || id_object == OBJID_WINDOW_I32 {
        reassert_strict_topmost();
    }
}

fn install_strict_topmost_hooks() {
    STRICT_TOPMOST_HOOKS.call_once(|| unsafe {
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        for event in [
            EVENT_SYSTEM_FOREGROUND,
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_REORDER,
        ] {
            let hook = SetWinEventHook(
                event,
                event,
                None,
                Some(strict_topmost_event),
                0,
                0,
                flags,
            );
            if hook.is_invalid() {
                tracing::warn!(event, "cannot install strict-topmost WinEvent hook");
            }
        }
    });
}

/// Keep TaskMan above the Windows shell as well as ordinary top-level windows.
///
/// The taskbar and Start menu use topmost shell surfaces of their own, so a
/// one-shot `HWND_TOPMOST` request does not define which topmost window wins
/// after the shell changes z-order. The WinEvent hooks above reinsert TaskMan
/// at the front of that band whenever a foreground/window show/reorder event
/// occurs. `SWP_NOACTIVATE` means this never steals keyboard focus.
pub fn set_strict_topmost(hwnd: isize, enabled: bool) {
    use std::sync::atomic::Ordering;

    let native = HWND(hwnd as *mut std::ffi::c_void);
    if enabled {
        STRICT_TOPMOST_HWND.store(hwnd, Ordering::Release);
        STRICT_TOPMOST_ENABLED.store(true, Ordering::Release);
        install_strict_topmost_hooks();
        reassert_strict_topmost();
    } else {
        STRICT_TOPMOST_ENABLED.store(false, Ordering::Release);
        STRICT_TOPMOST_HWND.store(0, Ordering::Release);
        unsafe {
            let _ = SetWindowPos(
                native,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE
                    | SWP_NOSIZE
                    | SWP_NOACTIVATE
                    | SWP_NOOWNERZORDER
                    | SWP_NOSENDCHANGING,
            );
        }
    }
}

'''
text = replace_once(text, anchor, helper + anchor, "strict topmost implementation")
path.write_text(text, encoding="utf-8")


# Cross-platform wrapper used by the app after a native HWND exists.
path = Path("crates/tm-platform/src/lib.rs")
text = path.read_text(encoding="utf-8")
anchor = '''/// Hide or reveal a window at the compositor — see\n/// [`win::window_chrome::set_cloaked`]. Non-Windows hosts ignore it.\n'''
helper = '''/// Make the Windows always-on-top setting win against shell-owned topmost\n/// surfaces too. Non-Windows hosts keep using the toolkit's window level.\n#[cfg(target_os = "windows")]\npub fn set_strict_topmost(hwnd: isize, enabled: bool) {\n    win::window_chrome::set_strict_topmost(hwnd, enabled);\n}\n\n#[cfg(not(target_os = "windows"))]\npub fn set_strict_topmost(_hwnd: isize, _enabled: bool) {}\n\n'''
text = replace_once(text, anchor, helper + anchor, "strict topmost platform wrapper")
path.write_text(text, encoding="utf-8")


# App state applies the native topmost policy once per logical setting change.
path = Path("crates/tm-app/src/app.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '''    title_bar_applied: Option<([u8; 3], bool)>,\n\n    // Tab states.\n''',
    '''    title_bar_applied: Option<([u8; 3], bool)>,\n    /// Native Win32 strict-topmost state last applied to the root HWND.\n    #[cfg(target_os = "windows")]\n    native_topmost_applied: Option<bool>,\n\n    // Tab states.\n''',
    "app topmost state field",
)
text = replace_once(
    text,
    '''            title_bar_applied: None,\n            selected_user: None,\n''',
    '''            title_bar_applied: None,\n            #[cfg(target_os = "windows")]\n            native_topmost_applied: None,\n            selected_user: None,\n''',
    "app topmost state init",
)
text = replace_once(
    text,
    '''        let pal = crate::theme::palette_ctx(&ctx);\n        self.sync_title_bar(&ctx, &pal, _frame);\n''',
    '''        let pal = crate::theme::palette_ctx(&ctx);\n        #[cfg(target_os = "windows")]\n        self.sync_native_topmost(_frame);\n        self.sync_title_bar(&ctx, &pal, _frame);\n''',
    "app topmost sync call",
)
anchor = '''    /// Make Windows paint its caption in the app's own colors.\n'''
helper = r'''    #[cfg(target_os = "windows")]
    fn sync_native_topmost(&mut self, frame: &eframe::Frame) {
        let enabled = self.shared.settings.always_on_top;
        if self.native_topmost_applied == Some(enabled) {
            return;
        }
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = frame.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            return;
        };
        tm_platform::set_strict_topmost(win32.hwnd.get(), enabled);
        self.native_topmost_applied = Some(enabled);
    }

'''
text = replace_once(text, anchor, helper + anchor, "app native topmost sync")
path.write_text(text, encoding="utf-8")


# Put a direct Windows Task Manager escape hatch in every tab's top command bar.
path = Path("crates/tm-app/src/app_ui.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '''            extra(app, ui);\n            vsep(ui, pal);\n            if cmd_button(ui, pal, Icon::RunTask, i18n::tr(K::RunNewTask), true) {\n                app.run_dialog_open = true;\n            }\n''',
    '''            extra(app, ui);\n            vsep(ui, pal);\n            #[cfg(target_os = "windows")]\n            {\n                if cmd_button(\n                    ui,\n                    pal,\n                    Icon::OpenExternal,\n                    i18n::tr(K::WindowsTaskManager),\n                    true,\n                ) {\n                    let actions = app.actions.clone();\n                    let ctx = ui.ctx().clone();\n                    app.run_action(\n                        &ctx,\n                        || i18n::tr(K::WindowsTaskManagerStarted).to_string(),\n                        move || actions.launch_native_task_manager(),\n                    );\n                }\n                vsep(ui, pal);\n            }\n            if cmd_button(ui, pal, Icon::RunTask, i18n::tr(K::RunNewTask), true) {\n                app.run_dialog_open = true;\n            }\n''',
    "top command bar native task manager button",
)
path.write_text(text, encoding="utf-8")


# Localized labels/toast.
path = Path("crates/tm-core/src/i18n.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '''    RunNewTask => ["Neuen Task ausführen", "Run new task"],\n    EndTask => ["Task beenden", "End task"],\n''',
    '''    RunNewTask => ["Neuen Task ausführen", "Run new task"],\n    WindowsTaskManager => ["Windows-Task-Manager", "Windows Task Manager"],\n    WindowsTaskManagerStarted => [\n        "Windows-Task-Manager gestartet",\n        "Windows Task Manager started"\n    ],\n    EndTask => ["Task beenden", "End task"],\n''',
    "native task manager translations",
)
path.write_text(text, encoding="utf-8")


# Durable architecture notes.
path = Path("llm-wiki/repo-map.md")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '''    `taskmgr_replacement.rs` (owned IFEO\n    `Debugger` registration for taskmgr.exe plus the guards that keep a\n    registration launchable), `instance.rs` (session-local instance\n''',
    '''    `taskmgr_replacement.rs` (owned IFEO\n    `Debugger` registration for taskmgr.exe plus the guards that keep a\n    registration launchable, and the explicit built-in-Task-Manager escape\n    hatch which debug-creates taskmgr and immediately detaches so IFEO stays\n    installed), `instance.rs` (session-local instance\n''',
    "repo map taskmgr integration",
)
text = replace_once(
    text,
    '''    `window_chrome.rs` (DWM caption colour / dark mode / backdrop / cloaking),\n''',
    '''    `window_chrome.rs` (DWM caption colour / dark mode / backdrop / cloaking,\n    plus an event-driven strict-topmost keeper that reasserts the root HWND on\n    foreground/show/reorder events so topmost shell surfaces cannot stay above it),\n''',
    "repo map topmost integration",
)
path.write_text(text, encoding="utf-8")

path = Path("llm-wiki/current.md")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '''- **Native caption** painted to match the strip below it, with immersive dark\n  mode and the Windows 11 backdrop request. The limits are in `known-debt.md`.\n''',
    '''- **Native caption** painted to match the strip below it, with immersive dark\n  mode and the Windows 11 backdrop request. The limits are in `known-debt.md`.\n- **Always on top is strict on Windows.** In addition to the toolkit window\n  level, WinEvent foreground/show/reorder notifications reinsert TaskMan at\n  the front of the topmost band without activating it, so taskbar/Start shell\n  surfaces cannot remain above an opted-in TaskMan window.\n- **Built-in Task Manager stays reachable.** A top command-bar button launches\n  Windows Task Manager even while TaskMan's IFEO replacement is enabled; it\n  uses the debugger-create bypass and immediately detaches rather than racing\n  by temporarily removing the replacement registration.\n''',
    "current topmost and taskmgr bullets",
)
path.write_text(text, encoding="utf-8")

path = Path("llm-wiki/log/recent.md")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '# Recent Activity\n\n',
    '''# Recent Activity\n\n## 2026-09-08 — Strict topmost and native Task Manager escape hatch\n\n1. **Always-on-top now wins against shell topmost surfaces.** The toolkit's\n   one-shot window level is still set, but Windows also installs out-of-context\n   WinEvent hooks for foreground, top-level show and z-order reorder events.\n   While enabled and visible, TaskMan reinserts itself at `HWND_TOPMOST` with\n   `SWP_NOACTIVATE`, preventing the taskbar/Start menu from remaining above it\n   without stealing input focus. No polling or timing retry is involved.\n2. **The built-in Task Manager remains explicitly reachable.** Every tab's top\n   command bar has a Windows Task Manager action. It starts the System32\n   `Taskmgr.exe` with `DEBUG_ONLY_THIS_PROCESS`, which deliberately bypasses\n   that image's IFEO Debugger registration, and immediately calls\n   `DebugActiveProcessStop`; TaskMan's replacement registration never has to be\n   removed or temporarily weakened.\n3. **Detach failure is fail-closed.** If Windows cannot detach the debug-created\n   Task Manager, the just-created process is terminated instead of being left\n   suspended on a long-lived action-executor debugger connection.\n\n''',
    "recent topmost/taskmgr log",
)
path.write_text(text, encoding="utf-8")

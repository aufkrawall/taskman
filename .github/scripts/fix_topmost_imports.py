from pathlib import Path

path = Path('crates/tm-platform/src/win/window_chrome.rs')
text = path.read_text(encoding='utf-8')
old = '''use windows::Win32::UI::Accessibility::{\n    HWINEVENTHOOK, SetWinEventHook, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,\n};\nuse windows::Win32::UI::WindowsAndMessaging::{\n    EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, HWND_NOTOPMOST, HWND_TOPMOST,\n    IsIconic, IsWindowVisible, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING,\n    SWP_NOSIZE, SetWindowPos,\n};\n'''
new = '''use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};\nuse windows::Win32::UI::WindowsAndMessaging::{\n    EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, HWND_NOTOPMOST, HWND_TOPMOST,\n    IsIconic, IsWindowVisible, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING,\n    SWP_NOSIZE, SetWindowPos, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,\n};\n'''
if text.count(old) != 1:
    raise SystemExit(f'WinEvent import block: expected one match, found {text.count(old)}')
path.write_text(text.replace(old, new, 1), encoding='utf-8')

from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


# Never learn Windows' iconic/minimized placement as the user's normal restore
# position. A minimized window may report shell/iconic geometry while also
# reporting maximized=false, which used to overwrite the last valid position
# and make a later restore appear to do nothing because the window reopened
# off-screen.
path = Path("crates/tm-app/src/main.rs")
text = path.read_text(encoding="utf-8")
old = '''        if self.inner.shared.settings.remember_window {\n            let (pos, maximized) = ui.ctx().input(|i| {\n                (\n                    i.viewport().outer_rect.map(|r| r.min),\n                    i.viewport().maximized.unwrap_or(false),\n                )\n            });\n            // A maximized window's outer rect is the monitor's, not the\n            // restore geometry — keep the last normal position instead.\n            if !maximized && let Some(pos) = pos {\n                ui_state::set_window_position([pos.x, pos.y]);\n            }\n            ui_state::set_window_maximized(maximized);\n        }\n'''
new = '''        if self.inner.shared.settings.remember_window {\n            let (pos, maximized, minimized) = ui.ctx().input(|i| {\n                (\n                    i.viewport().outer_rect.map(|r| r.min),\n                    i.viewport().maximized.unwrap_or(false),\n                    i.viewport().minimized.unwrap_or(false),\n                )\n            });\n            // Never persist iconic/minimized geometry. On Windows a minimized\n            // window can expose shell/off-screen placement while simultaneously\n            // reporting maximized=false; recording that as the normal restore\n            // position can strand the next restored window off-screen.\n            if !minimized {\n                // A maximized window's outer rect is the monitor's, not the\n                // restore geometry — keep the last normal position instead.\n                if !maximized && let Some(pos) = pos {\n                    ui_state::set_window_position([pos.x, pos.y]);\n                }\n                ui_state::set_window_maximized(maximized);\n            }\n        }\n'''
text = replace_once(text, old, new, "remember-window minimized guard")
path.write_text(text, encoding="utf-8")

# Put the invariant in durable repo knowledge next to the recent window work.
path = Path("llm-wiki/log/recent.md")
text = path.read_text(encoding="utf-8")
entry = '''## 2026-09-07 — Minimized windows cannot overwrite restore placement\n\n1. **Ignore iconic placement samples.** While `remember_window` is enabled, the\n   native shell no longer updates the saved position or maximized flag from a\n   minimized viewport. Windows may report iconic/off-screen geometry for a\n   minimized window while `maximized` is false; persisting that value can make a\n   later taskbar restore appear broken because the window reopens off-screen.\n2. **Normal/maximized semantics are unchanged.** Normal windows still update the\n   remembered desktop position; maximized windows still preserve the last normal\n   position while updating only the maximized flag.\n\n'''
text = replace_once(text, '# Recent Activity\n\n', '# Recent Activity\n\n' + entry, "recent log header")
path.write_text(text, encoding="utf-8")

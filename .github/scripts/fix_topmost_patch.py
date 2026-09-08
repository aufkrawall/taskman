from pathlib import Path

path = Path('.github/scripts/apply_topmost_native_taskmgr.py')
text = path.read_text(encoding='utf-8')
old = '''text = replace_once(\n    text,\n    \'\'\'    `taskmgr_replacement.rs` (owned IFEO\\n    `Debugger` registration for taskmgr.exe plus the guards that keep a\\n    registration launchable), `instance.rs` (session-local instance\\n\'\'\',\n    \'\'\'    `taskmgr_replacement.rs` (owned IFEO\\n    `Debugger` registration for taskmgr.exe plus the guards that keep a\\n    registration launchable, and the explicit built-in-Task-Manager escape\\n    hatch which debug-creates taskmgr and immediately detaches so IFEO stays\\n    installed), `instance.rs` (session-local instance\\n\'\'\',\n    "repo map taskmgr integration",\n)'''
new = '''text = replace_once(\n    text,\n    "registration launchable), `instance.rs` (session-local instance",\n    "registration launchable, and the explicit built-in-Task-Manager escape\\n"\n    "    hatch which debug-creates taskmgr and immediately detaches so IFEO stays\\n"\n    "    installed), `instance.rs` (session-local instance",\n    "repo map taskmgr integration",\n)'''
if old not in text:
    raise SystemExit('repo-map patch block not found in patch script')
path.write_text(text.replace(old, new, 1), encoding='utf-8')

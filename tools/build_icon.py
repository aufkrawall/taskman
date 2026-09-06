import subprocess
import os
from pathlib import Path

rc_candidates = [
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\rc.exe",
    r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.19041.0\x64\rc.exe",
]
rc_exe = next((p for p in rc_candidates if os.path.exists(p)), None)
assets_dir = Path("crates/tm-app/assets").resolve()
rc_file = assets_dir / "app.rc"
res_file = assets_dir / "app.res"

rc_file.write_text('1 ICON "icon.ico"\n', encoding="utf-8")

if rc_exe:
    res = subprocess.run([rc_exe, "/fo", str(res_file), str(rc_file)], cwd=assets_dir)
    print("rc.exe compiled app.res:", res.returncode == 0)

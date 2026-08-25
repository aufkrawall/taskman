# Recent Activity

## 2026-08-25 — UI polish pass + Linux backend repaired & verified under WSLg

**Column resize root cause (tablekit.rs):** the elastic name column absorbed
viewport slack EVERY frame, so a manual drag of any other boundary was
cancelled in the same frame (dragged separator stayed put, Name/Status
divider shifted instead, regime flips around `spare == stored` read as
wobbling). Fix: `name_effective` is elastic ONLY while `width == 0.0`
(virgin sentinel); the first `drag_started` on any table materializes the
name width (value-preserving), after which all columns are explicitly
sized and boundaries track the cursor 1:1. Double-click on the name
separator restores fill mode via the 0.0 sentinel. Also: last column got a
right-edge resize handle; removed the extra stroke box around the Name
header cell; `table_avail` margin 6→16 px; body ScrollAreas got
`.content_margin(right:10, bottom:8)` so FLOATING scroll bars never paint
over the last column/bottom row (floating bars never reserve layout space;
reserving would desync header/body widths).

**Graph rendering root cause (chart.rs):** all area fills used
`Shape::convex_polygon`, but epaint fan-triangulates fills from vertex 0
(`fill_closed_path`) — only valid for CONVEX polygons. Concave series got
straight fan edges cutting across dips: fake linear ramps, cliffs, fills
floating above the line, white gaps (see graphpngs/1-3.png). Fix:
`fill_area_to_baseline()` builds an explicit x-monotone triangle strip
(2 verts + 6 indices per segment) that hugs the polyline exactly; used by
sparkline, core_chart (incl. kernel band) and chart_multi. Also: series
extractors (`disk/net/gpu_series`) now return one value PER window point
(0.0 when the device is absent; no more zero-filtering in net_series) so
series indices stay aligned with `timestamps_ms` — shortened series used
to plot at wrong x positions; chart_multi hover maps pointer time back to
the nearest sample via partition_point.

**Other UI:** checkbox hover theme-aware (accent border over faint tint,
no white flood in dark mode); chart strokes/fills strengthened, kernel
times = darker accent shade (`kernel_color`), per-theme secondary colors
(`pal.ok_green` not hardcoded dark-theme green).

**Linux backend (tm-platform/linux):** was NOT compiling — model additions
from Windows work (`CpuInfo::kernel_pct/per_core_kernel_pct`,
`GpuInfo::luid/shared_used_bytes`) never landed there, plus missing
`use std::sync::Mutex` and 5 clippy lints. Filled with documented "unknown"
values (empty vec / 0 / None) matching model docs & mock. Verified:
- `--selfcheck` real backend ok:true (CPU/mem/disks/networks/processes;
  GPU honestly empty — WSL2 exposes no /dev/dri for DRM).
- GUI runs under WSLg Wayland: wgpu+Vulkan at locked ~60 fps (Fifo), surface
  1100×720; glow renderer starts too. Note: wgpu hides "Microsoft Direct3D12
  (NVIDIA ...)" as not-Vulkan-compliant → renders via llvmpipe software path,
  still vsync'd 60 fps.
- `goto_services_for_pid`: cfg_attr allow(dead_code) on non-Windows (callers
  are cfg(windows)); keep compiling everywhere.

**WSL/Windows dual-host gotcha:** WSL builds produce `target/release/taskman`
(ELF) while the launched binary is `taskman.exe` — a WSL-side
`build.py --host-only` does NOT refresh the exe. Refresh it from WSL via
interop: `cmd.exe /c "... && set PATH=C:\Users\REDACTED\.cargo\bin;%PATH% &&
py.exe build.py --host-only"` (works; selfcheck via powershell
Start-Process -RedirectStandardOutput since GUI-subsystem console attach
fails through the interop pipe). Consider separate CARGO_TARGET_DIR per
host to avoid cache thrash.

## <YYYY-MM-DD> — Template scaffold created

Initial generic `AGENTS.md` / `llm-wiki` template. No project-specific
history recorded yet.

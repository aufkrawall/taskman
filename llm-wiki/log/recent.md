# Recent Activity

## 2026-08-25 — UI polish pass + Linux backend repaired & verified under WSLg

**UI (all in tm-app):**
- `tablekit.rs`: last table column now resizable via a handle on its right
  edge (`bounds` gained a final right-edge entry); removed the extra stroke
  box around the Name header cell (made it visually diverge and sat ~0.5 px
  off the resize boundary); `table_avail` margin 6→16 px and body ScrollAreas
  got `.content_margin(right:10, bottom:8)` so the FLOATING scroll bars no
  longer paint over the last column / bottom row. Key egui fact: floating
  bars never reserve layout space (`floating_allocated_width=0`), they overlay
  content — reserving instead would desync header/body widths because only
  the body ever has a vertical bar.
- `controls.rs`: checkbox hover no longer floods white (dark-mode glare);
  now accent border over a faint card-bg/accent tint via new `blend()` helper,
  theme-correct on both palettes.
- `chart.rs`/`performance.rs`: chart readability — lines drawn in a SECOND
  pass after all fills (outer fills no longer dim inner lines), stronger
  strokes/fills, kernel times use `kernel_color()` = darker accent shade
  (was hardcoded dark-theme green that washed out in light mode); secondary
  series colors now come from `pal.ok_green` per theme.

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

## <YYYY-MM-DD> — Template scaffold created

Initial generic `AGENTS.md` / `llm-wiki` template. No project-specific
history recorded yet.

# Debug Tools

Last cross-checked: 2026-09-08

Primary sources:
- `AGENTS.md`
- `crates/tm-app/src/main.rs`
- `crates/tm-app/src/app.rs`
- `tools/capture.ps1`

## General rules

- Verify a tool exists and runs before relying on it; prefer discovery
  (`Get-Command`, `where.exe`, `command -v`) over hardcoded paths.
- Prefer project-local or repository-pinned tools over global alternatives.
- Treat dumps, logs, captures, extracted strings, and diagnostic output as
  potentially sensitive.
- Do not mutate binaries, symbols, global debugger flags, registry/system
  settings, or persistent runtime configuration unless explicitly requested.
- When a preferred tool is unavailable, use a safe equivalent and record the
  coverage limitation.
- Dump, symbol, and binary-inspection tooling: `debug-tools-security-audit.md`.

## Headless diagnostics

| Tool | Purpose | Invocation |
| --- | --- | --- |
| Core tests | model/settings/history/engine logic | `cargo test -p tm-core` |
| Platform tests | collectors/actions plus short-lived Windows process integration tests | `cargo test -p tm-platform` |
| App tests | tables, process topology, sorting, charts, module-dialog model | `cargo test -p tm-app` |
| Full gate | fmt, clippy `-D warnings`, workspace tests | `python build.py --check` |
| Release | refresh the user-launched host artifact | `python build.py --host-only` |
| Self-check | headless collector JSON smoke test; starts the binary but not its GUI | `target/release/taskman.exe --selfcheck [--mock]` |
| Service self-check | headless broker framing/ACL/path summary; does not install/start SCM service | `target/release/taskman-service.exe --selfcheck` |

Cargo tests and `build.py` do not open TaskMan windows. The capture helper and
plain `taskman.exe` do; do not use those while the desktop must remain
undisturbed. Windows integration tests may spawn short-lived helper processes.
The service self-check is non-mutating: it does not write Program Files,
ProgramData, registry, or SCM state.

## Renderer diagnostics

- Default order is WGPU, then Glow. `TASKMAN_RENDERER=wgpu|glow` forces one.
- `TASKMAN_GPU=auto|compatibility|software` overrides the persisted
  `render_mode` for one run (`0`/`1` still map to software/auto).
  `compatibility` is the Glow/OpenGL path; `software` selects the CPU adapter
  (WARP on Windows), which is correct but costs ~14 cores at 2.9 fps — see
  `known-debt.md` before using it for anything but a comparison.
- `TASKMAN_PRESENT=fifo|immediate|mailbox|autovsync` and
  `TASKMAN_FRAME_LATENCY=<n>` (0 = leave it to wgpu) tune the swapchain while
  investigating frame pacing.
- `TASKMAN_SUBPIXEL=0|1` forces sub-pixel (ClearType) text off or on. It cannot
  enable it on a GPU renderer: a sub-pixel atlas blended with one scalar alpha
  draws rainbow-tinted text, so the interlock in `theme.rs` wins. With no
  override, the mode follows Windows' own font-smoothing settings and the
  per-monitor ClearType calibration.
- `TASKMAN_RENDERER=software|wgpu|glow` now includes the native CPU renderer.
  It is what `render_mode = software` selects and what the default order tries
  first. `TASKMAN_PRESENT` and `TASKMAN_FRAME_LATENCY` are wgpu swapchain knobs
  and do nothing on the CPU path.
- `TASKMAN_TEXT_SMOOTHING=sharp|standard|smooth` overrides the persisted glyph
  weight/grid-fitting profile for A/B comparison without touching config.ini.
- Windows WGPU compiles only D3D12; Linux only Vulkan; macOS only Metal.
  `WGPU_BACKEND` may narrow the compiled set but cannot enable a backend that
  was not compiled for that host.
- WGPU defaults to the low-power adapter for this 2D monitor UI.
  `WGPU_POWER_PREF=high` is the opt-in discrete/high-performance override.
- `TASKMAN_FPS_PROBE=1` forces continuous repaint and displays frame rate, but
  it requires launching the GUI and should be reserved for an interactive run.

## UI/capture diagnostics

- `TASKMAN_DIALOG=settings|run|end_task` opens the chosen dialog at startup.
- `TASKMAN_PERF=cpu|mem|<resource-key>` preselects a Performance resource.
- `TASKMAN_TAB=<tab-key>` preselects a page.
- `tools/capture.ps1` automates a window capture with isolated config/data;
  read its header before use. It is intentionally not part of headless gates.

## Linux GUI testing under WSLg

WSLg is a Weston + RDP (RAIL/VAIL) session, not Plasma/GNOME. It is useful for
smoke tests and app-level DPI checks but cannot stand in for a real desktop:

- **Window presentation can silently degrade.** A `[WARN:COPY MODE]` prefix in
  the window title means the VAIL shared-memory path failed (known WSLg bug;
  weston logs `rdp_allocate_shared_memory: ... Input/output error`). Windows
  still exist but are laggy, and screen captures of their rectangle show only
  what is behind them. Fix without a full `wsl --shutdown`:
  `wsl --system -e bash -c 'pgrep weston | xargs -r kill -9'` (WSLg restarts).
- **No fractional scaling.** Weston exposes only integer output scale and no
  `wp_fractional_scale_manager_v1`; the `.wslgconfig` fractional options
  resample the whole framebuffer (blurry). To exercise the app's 150% path,
  run on X11 with `WINIT_X11_SCALE_FACTOR=1.5` (winit reports 1.5 and WSLg
  presents 1:1), or set `ui_zoom=1.5` in `config.ini`.
- **Sub-pixel text.** `TASKMAN_SUBPIXEL=1` forces the fork's LCD path; Linux
  otherwise follows fontconfig `rgba` (`rgb`/`bgr` only, grayscale otherwise).
  At scale 1 and at app-level 1.5x the fringes survive because WSLg does not
  resample; a real fractional-scaling compositor may blur them.
- **Fonts** come from the distro's fontconfig (`fc-match`); a minimal distro
  has few faces. Install the target desktop's fonts before judging metrics.
- **No GPU**: WSLg exposes no DRM card here, so the software renderer is the
  only path; the wgpu/Vulkan path needs real hardware.
- The static-musl artifact cannot run the GUI at all (no `dlopen`); use the
  glibc artifact.

## Data and logs

- `TASKMAN_DATA_DIR` and `TASKMAN_CONFIG_DIR` redirect state for tests or
  reproductions. Prefer fresh temporary directories.
- Daily logs live under `<taskman-data-dir>/logs/`. Treat them as sensitive;
  they are never test fixtures or commit artifacts.

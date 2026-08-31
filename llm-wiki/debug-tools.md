# Debug Tools

Last cross-checked: 2026-08-31

Primary sources:
- `AGENTS.md`
- `crates/tm-app/src/main.rs`
- `crates/tm-app/src/app.rs`
- `tools/capture.ps1`

## Headless diagnostics

| Tool | Purpose | Invocation |
| --- | --- | --- |
| Core tests | model/settings/history/engine logic | `cargo test -p tm-core` |
| Platform tests | collectors/actions plus short-lived Windows process integration tests | `cargo test -p tm-platform` |
| App tests | tables, process topology, sorting, charts, module-dialog model | `cargo test -p tm-app` |
| Full gate | fmt, clippy `-D warnings`, workspace tests | `python build.py --check` |
| Release | refresh the user-launched host artifact | `python build.py --host-only` |
| Self-check | headless collector JSON smoke test; starts the binary but not its GUI | `target/release/taskman.exe --selfcheck [--mock]` |

Cargo tests and `build.py` do not open TaskMan windows. The capture helper and
plain `taskman.exe` do; do not use those while the desktop must remain
undisturbed. Windows integration tests may spawn short-lived helper processes.

## Renderer diagnostics

- Default order is WGPU, then Glow. `TASKMAN_RENDERER=wgpu|glow` forces one.
- Windows WGPU compiles only D3D12; Linux only Vulkan; macOS only Metal.
  `WGPU_BACKEND` may narrow the compiled set but cannot enable a backend that
  was not compiled for that host.
- WGPU defaults to the low-power adapter for this 2D monitor UI.
  `WGPU_POWER_PREF=high` is the opt-in discrete/high-performance override.
- `TASKMAN_FPS_PROBE=1` forces continuous repaint and displays frame rate, but
  it requires launching the GUI and should be reserved for an interactive run.

## UI/capture diagnostics

- `TASKMAN_DIALOG=settings|run` opens the chosen dialog at startup.
- `TASKMAN_PERF=cpu|mem|<resource-key>` preselects a Performance resource.
- `TASKMAN_TAB=<tab-key>` preselects a page.
- `tools/capture.ps1` automates a window capture with isolated config/data;
  read its header before use. It is intentionally not part of headless gates.

## Data and logs

- `TASKMAN_DATA_DIR` and `TASKMAN_CONFIG_DIR` redirect state for tests or
  reproductions. Prefer fresh temporary directories.
- Daily logs live under `<taskman-data-dir>/logs/`. Treat them as sensitive;
  they are never test fixtures or commit artifacts.

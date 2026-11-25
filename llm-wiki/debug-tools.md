# Debug Tools

Last cross-checked: 2026-11-24

Primary sources:
- `AGENTS.md`
- `crates/tm-app/src/main.rs`
- `crates/tm-app/src/app.rs`
- `tools/capture.ps1`
- `tools/gpu-memory-probe.ps1`

## General rules

- Verify a tool exists and runs before relying on it; prefer discovery
  (`tools/discover-debug-tools.ps1`, `debug-tool-manifest.json`, `Get-Command`, `where.exe`, `command -v`) over hardcoded paths.
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
| Tool discovery | non-mutating Windows SDK/MSVC/Sysinternals/Rust tool discovery | `powershell -ExecutionPolicy Bypass -File .\tools\discover-debug-tools.ps1` |
| Discovery regression test | regression test for generic debug-tool discovery | `powershell -ExecutionPolicy Bypass -File .\tools\tests\test-debug-tool-discovery.ps1` |
| Core tests | model/settings/history/engine logic | `cargo test -p tm-core` |
| Platform tests | collectors/actions plus short-lived Windows process integration tests | `cargo test -p tm-platform` |
| App tests | tables, process topology, sorting, charts, module-dialog model | `cargo test -p tm-app` |
| Full gate | fmt, clippy `-D warnings`, workspace tests | `python build.py --check` |
| Release | refresh the user-launched host artifact | `python build.py --host-only` |
| Self-check | headless collector JSON smoke test; starts the binary but not its GUI | `target/release/taskman.exe --selfcheck [--mock]` |
| Code-sample end-to-end check | installed GUI → LocalSystem broker → system-profile ETW → module attribution | `"C:\Program Files\TaskMan\taskman.exe" --selfcheck-code-samples` |
| Service self-check | headless broker framing/ACL/path summary; does not install/start SCM service | `target/release/taskman-service.exe --selfcheck` |

Cargo tests and `build.py` do not open TaskMan windows. The capture helper and
plain `taskman.exe` do; do not use those while the desktop must remain
undisturbed. Windows integration tests may spawn short-lived helper processes.
The service self-check is non-mutating: it does not write Program Files,
ProgramData, registry, or SCM state.

`--selfcheck`'s JSON summary carries per-source availability counters beside the
entity counts, and they are the regression signal rather than decoration:
`process_net_readings` and `process_disk_readings` for the two per-process traces
(`0` with a running trace is a real answer — the ETW sessions need administrator
rights), `process_cpu_readings` for rows that carried a measured CPU
share, and `process_cpu_unknown` for the real (non-synthetic) processes showing
the honest dash. `process_cpu_readings` at 0 while `cpu.util_pct` is non-null is the
signature of an accountant that stopped reporting while the header stayed correct;
`process_cpu_unknown` equal to the process count is the opposite failure, every row
blanked.

`ok` is the run's own verdict, not a constant: it comes from the same
plausibility gate (at least five processes, a logical-processor count, a measured
CPU share) that decides the exit code, and `error` carries that gate's reason
when it fails — so the JSON line and the exit status can no longer disagree.
`--mock` skips the gate, since a mock snapshot describes no real machine.
`startup_to_snapshot_ms` is the time to the first *usable* snapshot, not process
startup cost. Most of the figure is deliberate: the check samples until
`REQUIRED_TICKS` (4) ticks at a 250 ms interval so the two-sample PDH groups
(GPU, disk rates, CPU speed) hold a real reading instead of their warm-up state.
What it must not contain is the per-file probe below, which waits up to three
seconds for a trace delivery — so it is measured when the snapshot arrives, never
at the print.

`file_io` reports the per-file byte accounting of the binary's own process, which
is the headless equivalent of the I/O page's state line: `state`
(`unavailable`/`undelivered`/`undecodable`/`kernel_logger_in_use`/`measured`),
`since_ms`, and the
session's `events_lost`, `unnamed_creates`, `dropped_paths` and
`cleared_file_objects`. `unavailable` on an unelevated run is the expected answer
— a per-file ETW session needs administrator rights or the
installed service — while `undelivered`/`undecodable` mean a session started and
delivered nothing usable. The session is a private system logger with
`EVENT_TRACE_FLAG_FILE_IO_INIT | EVENT_TRACE_FLAG_FILE_IO` (the global NT Kernel
Logger with the same flags is the fallback), so `undelivered` means no completed
read or write has been decoded anywhere yet, while `undecodable` means records
arrived and the payload layout did not match (the logged opcode, version and
payload length say which). The check waits until its own written-and-read-back
bytes show in `decoded_bytes`, so `measured` with `decoded_bytes` below 131072
after `waited_ms` ~3000 means the session works but this process's transfers
were not attributed. `kernel_logger_in_use` is the third answer: neither
session started because another application owns the kernel logger, so the
machine — not the token, and
not the payload — is why there is no number. `null`
means the question could not be asked (no verified process identity, or
`--mock`), and an unqueried `events_lost` stays `null` instead of a zero.

Four ignored ETW diagnostics validate real Windows payloads rather than only
synthetic records: `disk_trace_attributes_real_requests_to_the_issuing_process`,
`protected_process_modules_include_csrss_images`,
`sampled_profile_attributes_code_to_this_process`, and
`the_file_trace_measures_exact_bytes_per_file`. Run each with
`cargo test -p tm-platform --test integration <name> -- --ignored --nocapture`
from an elevated token or an account with the required performance-trace
rights. The per-file probe prints which of the ordered attempts actually started
(the private `TaskMan-File-App` session or the `NT Kernel Logger` fallback) or
the refusal that stopped both, then checks exact (read, written) bytes for a file
opened before the trace started (named from the handle table) and one opened
after it (named by its Create), with cache-flush paging writes excluded.
`the_file_trace_covers_a_process_from_its_start` (ignored) spawns a child
after the trace started and checks `traced_since_process_start`, its exact file
row and a counter window that also holds its writes to `NUL`.
`the_file_trace_costs_little` (ignored; build it with `--release`) measures the
trace's cost in CPU CYCLES of the issuing thread and of the consumer
(`QueryThreadCycleTime`/`QueryProcessCycleTime`), so other load on the machine
does not enter the numbers — wall-clock timing did, and `GetThreadTimes` is
quantized to 15.6 ms.
`the_file_trace_follows_a_live_process` (also ignored) follows another process:
set `TASKMAN_FILE_TRACE_PID`, and it prints the handle-table names, the trace's
own 15 s sample and then 20 s of the GUI's request path (`WinActions`, broker
first), so "the page shows nothing" can be split into "the process did no file
I/O", "the trace missed it" and "the request path lost it". Compare against the
process's `WriteOperationCount`/`WriteTransferCount` (`Win32_Process`): those
count every `NtWriteFile`, including pipes and devices, which the `FileIo`
stream does not carry.
`sudo` (Windows 11, enabled here) runs it elevated; with the new-window mode
redirect the output to a file from a `.cmd` wrapper.
The protected-module and profile probes print an explicit privilege
boundary skip when the system-logger session cannot start. Set
`TASKMAN_ETW_LIVE_REQUIRED=1` in a privileged diagnostic run to turn that skip
into a failure, so an unavailable profile/session cannot masquerade as a pass.

On 2026-10-26 the elevated protected-module probe decoded a 23-module
`csrss.exe` rundown including `ntdll.dll`; the elevated profile probe decoded
real `SampledProfile` records with zero reported loss. The profile provider
initially failed with `ERROR_PRIVILEGE_NOT_HELD` (1314) until the session
enabled the token's `SeSystemProfilePrivilege` before `StartTrace`; this is
required even for an elevated token.

Run `--selfcheck-code-samples` from the installed GUI path in the desktop
user's context. It targets its own process, forces a short busy workload, polls
the exact-identity broker request, and passes only after sampled instruction
pointers are attributed to a module. This is the end-to-end regression for the
UI error path; a development-tree executable is correctly rejected by the
broker as a foreign client. The success/failure detail is also written to the
normal GUI log.

## Renderer diagnostics

- CPU rendering is the default. `TASKMAN_RENDERER=cpu|d3d12|vulkan|metal|opengl|software|wgpu`
  pins one compiled backend.
- `TASKMAN_GPU=cpu|d3d12|vulkan|metal` overrides the saved preference for
  one run and permits fallbacks. The numeric legacy values select CPU.
- `TASKMAN_PRESENT=fifo|immediate|mailbox|autovsync` and
  `TASKMAN_FRAME_LATENCY=<n>` (0 = leave it to wgpu) tune the swapchain while
  investigating frame pacing.
- `TASKMAN_SUBPIXEL=0|1` forces sub-pixel (ClearType) text off or on. The
  one-scalar GPU limitation is handled by the software and dual-source WGPU
  paths; unsupported WGPU and Glow still fail closed. With no override, the
  mode follows Windows' own font-smoothing settings and per-monitor ClearType
  calibration, or Linux's fontconfig policy.
- The CPU renderer uses software presentation on the native display. The
  swapchain settings apply to WGPU only.
- `TASKMAN_TEXT_SMOOTHING=sharp|standard|smooth` overrides the persisted glyph
  weight/grid-fitting profile for A/B comparison without touching config.ini.
- Windows WGPU compiles only D3D12; Linux only Vulkan; macOS only Metal.
- WGPU defaults to the low-power adapter for this 2D monitor UI.
  `WGPU_POWER_PREF=high` is the opt-in discrete/high-performance override.
- `TASKMAN_FPS_PROBE=1` forces continuous repaint and displays frame rate, but
  it requires launching the GUI and should be reserved for an interactive run.
- `TASKMAN_INPUT_STATS=1` logs a once-per-second summary of `CursorMoved`
  decisions (`moves_s`, `dup`, `same_target`, `stable_veto`, `coalesced`,
  `repaint_now`, `trailing`, `active`), software-renderer frame timings
  (`pass_avg/max_ms`, `paint_avg/max_ms`, `present_avg/max_ms`), tile-damage
  skips/counts, AccessKit activation status, top `ctx.repaint_causes()`, and the
  table repaint work below. The last three cover only the pages that report their
  icon and text work (Processes, Details); App History, Startup and Users still
  resolve one icon per row and are not part of those numbers.
  - `model_rebuilds` — flattened-model rebuilds on Processes and Details. It
    tracks the SAMPLING cadence, never the frame rate: a scroll frame changes
    neither the data nor the ordering. A value that follows the refresh rate
    instead is the regression this counter exists for.
  - `icon_lookups` — icon-cache lookups the row loops actually made, roughly
    the DISTINCT visible images per frame.
  - `icon_memo_hits` — rows whose icon the per-frame memo answered without
    touching the cache; `hits + lookups` is the rows painted.
  - `text_measures` — clipped-text layouts spent only to answer "is this cell
    clipped". Details is the only page that reports it, and it should sit at
    ~0 unless the pointer is over the Details table; a number there is one text
    layout per visible cell per repaint.

### Per-process GPU memory

`tools/gpu-memory-probe.ps1` samples Windows' user-visible
`GPU Process Memory` dedicated/shared counters for one isolated TaskMan child.
It defaults to `target/release/taskman.exe`, renderer `d3d12`, and tray mode:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\gpu-memory-probe.ps1 -Renderer d3d12 -Tray
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\gpu-memory-probe.ps1 -Renderer d3d12 -Visible -Seconds 30
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\gpu-memory-probe.ps1 -Tray -TaskbarMonitor -Renderer d3d12
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\gpu-memory-probe.ps1 -Executable .\target\debug\taskman.exe -Renderer cpu -Visible -Json
```

The probe discovers the localized counter object and counter labels through
`Get-Counter`, then queries only `pid_<child-PID>_luid_*_phys_*` instances and
sums every matching LUID. Readiness comes from either the isolated log's
`engine started` marker or its renderer-ready
`phase="subpixel_capability_resolved"` startup record; the probe then requires
the child to stay alive through a one-second settle window before taking the
baseline. It reports baseline, peak, final, LUID details, exit/error text, or
explicit unavailable values. Tray mode treats a valid counter query with no
child GPU-process instance as 0 B only when exact-child Win32 window checks
also see no visible, titled, non-toolwindow window at least 320x200 after the
settle and after every sample. Small/toolwindow/titleless taskbar-overlay
windows are excluded. A visible main-sized window makes the probe fail rather
than emit `HiddenZero`; visible mode reports counter absence as `Unknown`
rather than inventing zero. Every launch uses
`--instance-unresponsive`, so an installed hidden instance cannot absorb it by
single-instance handoff. Inherited `TASKMAN_*`, `WGPU_*`, `RUST_LOG`, and
`RUST_BACKTRACE` variables are removed from the child environment before the
probe sets its own values.

`-TaskbarMonitor` is valid only with tray mode and seeds the isolated
`config.ini` with `taskbar_monitor_enabled=true` and `close_to_tray=true`;
`-Tray -TaskbarMonitor -Renderer d3d12` is the closest reproduction of a hidden
main window plus the software taskbar stats overlay. Omit it for a pure hidden
baseline. Fresh
`TASKMAN_DATA_DIR`/`TASKMAN_CONFIG_DIR` directories are used, and only the
spawned process is stopped and only its own temporary directory is removed.
The normal user token is sufficient; no elevation is requested. `vulkan` is
accepted by the interface, but Windows builds currently expose only D3D12 and
will report the renderer startup error.

The probe deliberately forces `TASKMAN_TAB=performance`. During this unattended
probe run, that demand starts no per-process network/disk ETW sessions, so tray
mode's exact-PID termination cannot strand those machine-wide traces. That
guarantee does not extend to interactive use, where switching or restoring tabs
can change demand. Visible mode first calls `Process.CloseMainWindow()` and
allows five seconds for graceful shutdown; if no window can be closed or
shutdown times out, it warns and falls back to killing only the exact child. A
window that exits just after the grace period is classified as a late graceful
exit rather than an unexpected process exit.

The baseline is a settled post-start sample, not by itself a steady-state
proof, and it excludes transients before readiness. Dedicated/shared values
describe this process only: driver, compositor, or desktop allocations can
remain even when TaskMan reports 0 B, and a released allocation can persist in
driver bookkeeping. `PeakState` distinguishes exact peaks from lower bounds
caused by unavailable, incomplete, or otherwise uninterpreted samples and from
wholly unknown peaks. For a same-process close-to-tray comparison, launch a
visible instance, note its PID's GPU dedicated/shared columns in Task Manager
Details, close the window to the tray, let Task Manager refresh, and compare the
same PID; use a separate tray launch as the control.

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

# Known and Accepted Debt

Last verified: 2026-09-02

Primary sources:
- `AGENTS.md`
- `audit.md` (2026 parity audit; phased plan)
- `crates/` (current behavior is the source of truth)

## Purpose

Debt that has been deliberately accepted, with the reasoning, so later
audits do not re-derive it and later agents do not "fix" it without
weighing the same trade-off. Items here are *recorded*, not endorsed.

## Remaining parity work (accepted scope, not yet implemented)

The 2026 audit (`audit.md`) used six phases, but they are no longer cleanly
"open" or "closed": the 2026-08-31 pass completed the high-value table,
keyboard, Performance visual, network-adapter, dump, and process-diagnostics
items across Phases 2–6. The following concrete gaps remain:

- **Column surface:** Details has the typed, persisted Select-columns catalog
  (now including an optional User SID column), but Processes and Users still
  expose fixed column sets rather than every native optional header-menu
  column. A shared cross-tab column registry is still desirable when the
  missing telemetry providers below are added.
- **Telemetry fidelity:** native SRUM App History;
  measured Startup Impact; packaged/MSIX startup tasks; full `.lnk` target
  resolution through `IShellLink`/`IPersistFile`; memory-composition
  categories/bar; static GPU details. (Per-GPU-engine histories landed
  2026-09-01 — `HistoryPoint.gpu_engines` plus the "change graph to" menu.)
- **Current 2026 optional columns:** NPU, NPU Engine, NPU Dedicated Memory,
  NPU Shared Memory, and Isolation/AppContainer, plus neural-engine
  Performance entries. These require capability-gated model/collector work;
  absent hardware or telemetry must remain `—`, never zero.
- **Details tree roots:** `System` (pid 4) reports parent pid 0 and sysinfo
  maps that to `None`, so it renders as a root next to `[System Process]`
  instead of nested beneath it the way System Informer shows it. Restoring
  the link would mean re-introducing a parent the collector API did not
  report; the honest root is preferred over a synthesized edge.
- **Sub-pixel (ClearType) text: DONE** (2026-09-01). Kept only so the reasoning is not
  re-derived, because this entry was half wrong. epaint's atlas is *not* single-channel --
  it has stored `Color32` since 0.36 -- so per-channel coverage needed no new format at
  all. What was genuinely blocked is per-channel *blending*: on a GPU that needs
  dual-source blending (upstream emilk/egui#2639), and on a CPU it is a multiply. Both the
  3x rasterization and the per-channel blend now live in `vendor/egui`. See
  `render-pipeline.md`. Linux follows fontconfig's `rgba` (`rgb`/`bgr` only) since
  2026-09-08; vertical orders and unknown/none stay grayscale.
- **Software rendering performance: SUPERSEDED** (2026-09-01). `render_mode = software`
  no longer means WARP -- a D3D12 driver emulated on the CPU at ~14 cores and 2.9 fps --
  but a native CPU rasterizer that draws the UI directly. The old measurement was correct
  and is retained here only to explain why the option used to carry a warning. Do not make
  WARP the *primary* meaning of `Software` again -- but note that the WARP adapter selector
  still exists and is correct where it sits: `main.rs::select_software_adapter` is installed
  only on the wgpu path, which `preferred_renderers` reaches only after the native CPU
  renderer has failed to come up. There it is the honest way to honour "no GPU"; it also
  reports back through `store_render_mode` when no CPU adapter exists at all.
- **Stack overflow under `TASKMAN_FPS_PROBE=1` on the software renderer.** Forcing
  continuous repaints crashes the process after a few seconds with "thread 'main' has
  overflowed its stack". Diagnostic-only: normal operation is event driven and stable
  (verified over repeated 15-20 s runs), and the GPU backends do not reproduce it.
  What is *known*, so it is not re-investigated from scratch:
  - It is **not** the rasterizer. Replacing the entire paint with a no-op still
    overflows, while routing the same frame through the slower tessellated path does
    not. It tracks frame *rate*, not painted content.
  - It is not re-entrancy of `run_ui_and_paint` -- a thread-local depth guard never
    fired -- and not `softbuffer::present`, which was stubbed out with no effect.
  - Upstream documents a related Windows behaviour: an invisible window burns a whole
    core (emilk/egui#7776), mitigated there by a 10 ms sleep. That mitigation is in
    place here and does not prevent this.
  The remaining suspicion is window-procedure frames accumulating around the paint at
  high repaint rates; confirming it needs a native stack trace, which a Rust stack
  overflow on Windows does not provide. Not chased further because the trigger is a
  diagnostic env var.
- **A translucent strip below the caption is not reachable from here.** The
  caption itself is painted to match what the app draws under it
  (`win/window_chrome.rs`: caption/text/border colours plus
  `IMMERSIVE_DARK_MODE`), and the Windows 11 backdrop is requested when the
  user has "Transparency effects" on — but DWM composes that material BEHIND
  the window, and it can only show where the window is transparent. It is not:
  the default renderer is the CPU one, presenting through `BitBlt` from a DIB
  whose alpha byte is always zero, so the client area is opaque by
  construction. An explicit caption colour also wins over the material on the
  caption, which is the deliberate trade — an exact colour match makes the
  caption and the search strip read as one surface, which a translucent
  caption over an opaque strip would not.
  Doing it properly means a presentation path carrying per-pixel alpha:
  writing 0xFF alpha everywhere except a declared glass region in
  `software_integration.rs`, `DwmExtendFrameIntoClientArea` for that region,
  and `with_transparent(true)` plus an alpha-aware clear on the wgpu/glow
  paths. Not attempted; it is a fork-level change that cannot be verified
  headlessly.

- **Service upgrade over a RUNNING service fails.** `stop_service_for_upgrade`
  opens the live service process with `SYNCHRONIZE` to wait for its exit, and
  that `OpenProcess` returns access denied even from an elevated installer, so
  `--core-service=install` aborts with "open core service process: Zugriff
  verweigert". Workaround: `sc stop TaskmanCore` before installing. Not
  investigated further; it predates the v2 work and only affects upgrades.
- **CPU compatibility:** current `cpu_load.rs` intentionally matches the
  standardized time-based CPU metric. The legacy frequency-weighted
  **CPU Utility** provider/column/switcher still does not exist; do not mutate
  the current accountant into the old metric.
- **Advanced Windows diagnostics:** live kernel dumps and their settings,
  analyze-wait-chain, processor-group-aware affinity above 64 logical CPUs,
  and an optional Efficiency-mode confirmation preference.
- **Shell/accessibility:** Settings is a resizable scrolling dialog rather
  than the native navigation page; a full AccessKit/screen-reader semantics
  pass, high-contrast tokens, text-scaling validation, menu/scrollbar polish,
  and multi-monitor-aware position restore remain.

The new Modules inspector is deliberately on-demand and Windows-only. Its
unload command attempts ANY enumerated module — which module to unload is
explicitly the user's call (product decision 2026-09-06; there is no
"protected module" concept), gated only by the confirmation dialog and its
crash warning. What remains is technical, not policy: same-architecture
targets only (a 64-bit FreeLibrary address cannot be injected into a 32-bit
process), process-level trust rules at the broker (critical/system/requesting-
GUI targets refused), TaskMan refusing to act on itself, and the exact
process identity + module base/path revalidation at action time. The unload repeats
FreeLibrary until the module leaves or the bounded budget is spent, and the
honest `ModuleUnloadOutcome` (still mapped, with how many references were
dropped) is what the user sees; permanently pinned or re-loaded modules stay.

## Core-service production hardening still outstanding

**Accepted security residual risk (documented in
`hardening/core-service/hardening.json`, do not re-raise as new):** any code
running as the authorized user can inject into the installed GUI process (or
run it under a debugger) and thereby issue broker requests with that GUI's
identity. The broker's client check is image-path based; the pipe DACL is the
authorization boundary and it authorizes the user, not the binary. Mitigating
this needs Authenticode signing plus a code-integrity/process-trust policy
(and possibly a signed service-side allowlist), not another path check. The
2026-09-08 security pass additionally requires the authorized SID to be a
user account (`SidTypeUser`) and decouples file logging from broker startup;
see `core-service.md`.

The service boundary, ACL installer, bounded protocol, identity checks, and
recovery policy are implemented. These release-engineering/operational items
remain follow-up rather than being simulated in headless tests:

- Sign both binaries and add publisher/signature verification to install and
  update policy. Protected paths plus pinned SHA-256 hashes prevent ordinary
  user replacement, but signing is still the right production provenance
  layer.
- Exercise install, upgrade, rollback, reparse/hard-link/pre-opened-handle
  attacks, ACL readback, SCM failure recovery, multi-session denial, and no-service fallback in a
  disposable Windows VM. The automated gate intentionally does not mutate the
  developer machine's Program Files, ProgramData, registry, or SCM.
- The pipe authorizes exactly one installing user SID. Multi-user support must
  add explicit per-user enrollment/revocation and auditability; broadening the
  DACL to `Authenticated Users` is not acceptable.
- Two workers, queue depth 16, 64 KiB frames, and 19 pipe instances bound
  broker resource growth, but I/O is synchronous. An already authenticated
  client can stall both workers. Validate slow legitimate operations under
  load, then use overlapped I/O with explicit per-request deadlines if the VM
  fault-injection matrix confirms safe timeout values.
- Uninstall removes the service registration and user redirect but leaves the
  protected binaries/data. A signed standalone uninstaller could schedule
  cleanup after the GUI exits; in-place recursive deletion is intentionally not
  attempted by the running app.

## Deliberate deviations / session-limited fixes

- **Efficiency-mode UI latency** (2026-08-26): after a toggle the UI waits
  for the next sample (plus one forced refresh) to reflect Windows'
  returned EcoQoS state. A spinner/pending affordance could be added if the
  ~1 s gap bothers users; correctness deliberately wins over optimistic UI.
- **Grouped process labels show whole-subtree counts even when collapsed**
  ("Brave Browser (43)" with children hidden). This now MATCHES native TM;
  noted so it isn't "fixed" back to direct-children counts.
- **Always-on-top only reaches the Start menu when enabled at startup**
  (2026-09-09). Native Task Manager creates its window in window band 16
  (`CreateWindowInBand`), above the Start menu's band 6. A band-16 window is
  implicitly `WS_EX_TOPMOST` and cannot be demoted: `SetWindowBand` fails with
  `ERROR_ACCESS_DENIED`, and both `SetWindowPos(HWND_NOTOPMOST)` and clearing
  the exstyle leave the window topmost. The band is therefore chosen at window
  creation from the persisted `always_on_top` setting (vendored `vendor/winit`
  patch; `TASKMAN_WINDOW_BAND=16`). Toggling the setting at runtime still
  applies ordinary band-1 topmost, and the settings dialog shows the existing
  "Takes effect at the next start." hint while the live value differs. Do not
  add polling or shell-fighting workarounds.

## Falsified findings — do not re-raise

- "heat_cells discovers maxima from one row" — fixed 2026-08-26 (P0.2);
  intensities are normalized per column over the full display model before
  virtualization (`tablekit::norm`, `normalize_heat`, users' `HeatMax`).
- "columns can't be resized" (drag delta handling) — root-caused earlier;
  egui `drag_delta()` accumulation onto the LIVE width is correct behavior.

## Per-process disk activity

- `Disk activity` is measured, so it is only available where an ETW session
  can run: through the LocalSystem broker (the normal install) or in an
  elevated GUI. Without either it renders "—", like the Network column.
- Requests issued by a thread that had already exited when the window was
  drained cannot be attributed and stay in the unattributed remainder, so the
  per-process shares sum to at most 100 %, not exactly 100 %. Charging them to
  a guess would be worse than the gap.
- `Microsoft-Windows-Kernel-Disk` publishes no documented unit for
  `HighResResponseTime`, so it is never shown as a duration — only as a share
  of the window's total. `disk_etw.rs` keeps that rule.
- The payload offsets are hand-written. `disk_etw::tests` pins the decoder
  against synthetic records and rejects implausible ones; only
  `integration::disk_trace_attributes_real_requests_to_the_issuing_process`
  (ignored, needs elevation) proves them against live kernel events. Run it
  after any Windows build that changes the provider. **The system-logger
  provider switch of 2026-09-11 has not yet been proven on live events.**
- `Microsoft-Windows-Kernel-Disk` is NOT usable for attribution: its
  `DiskRead`/`DiskWrite` template ends at `HighResResponseTime` and carries no
  issuing thread. The events come from `SystemIoProviderGuid` on a
  system-logger session instead (Windows 10 2004+). On older builds the
  session cannot start and the column reports "—".

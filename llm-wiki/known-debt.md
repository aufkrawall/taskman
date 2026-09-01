# Known and Accepted Debt

Last verified: 2026-09-01

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

- **Column surface:** Details has the typed, persisted Select-columns catalog,
  but Processes and Users still expose fixed column sets rather than every
  native optional header-menu column. A shared cross-tab column registry is
  still desirable when the missing telemetry providers below are added.
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
  `render-pipeline.md`.
- **Software rendering performance: SUPERSEDED** (2026-09-01). `render_mode = software`
  no longer means WARP -- a D3D12 driver emulated on the CPU at ~14 cores and 2.9 fps --
  but a native CPU rasterizer that draws the UI directly. The old measurement was correct
  and is retained here only to explain why the option used to carry a warning. Do not
  reintroduce the WARP adapter selector.
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

- **`ProcessEntry.service_name` is never populated on Windows.** The field and
  its consumers (search, and the single-service naming a Details/Processes row
  could use) exist, but no collector fills it, so every `svchost.exe` row reads
  as the same "Host Process for Windows Services". This is also why the
  Processes page deliberately does NOT group service hosts into one row: the
  individual rows are already indistinguishable, and hiding them behind a
  chevron would remove the only way to reach them. Filling the field from the
  services enumeration would fix both.

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
unload command supports only same-architecture third-party DLLs and refuses
the main image, Windows-path modules, critical loader modules, cross-bitness
targets, and TaskMan itself. Broader injection would add risk without useful
Task-Manager parity and is not planned.

## Core-service production hardening still outstanding

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

## Falsified findings — do not re-raise

- "heat_cells discovers maxima from one row" — fixed 2026-08-26 (P0.2);
  intensities are normalized per column over the full display model before
  virtualization (`tablekit::norm`, `normalize_heat`, users' `HeatMax`).
- "columns can't be resized" (drag delta handling) — root-caused earlier;
  egui `drag_delta()` accumulation onto the LIVE width is correct behavior.

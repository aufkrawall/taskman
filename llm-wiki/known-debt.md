# Known and Accepted Debt

Last verified: 2026-08-31

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
- **Telemetry fidelity:** ETW per-process network; native SRUM App History;
  measured Startup Impact; packaged/MSIX startup tasks; full `.lnk` target
  resolution through `IShellLink`/`IPersistFile`; memory-composition
  categories/bar; full per-GPU-engine histories and static GPU details.
- **Current 2026 optional columns:** NPU, NPU Engine, NPU Dedicated Memory,
  NPU Shared Memory, and Isolation/AppContainer, plus neural-engine
  Performance entries. These require capability-gated model/collector work;
  absent hardware or telemetry must remain `—`, never zero.
- **Details tree roots:** `System` (pid 4) reports parent pid 0 and sysinfo
  maps that to `None`, so it renders as a root next to `[System Process]`
  instead of nested beneath it the way System Informer shows it. Restoring
  the link would mean re-introducing a parent the collector API did not
  report; the honest root is preferred over a synthesized edge.
- **Sub-pixel (ClearType) text:** impossible without forking epaint and both
  render backends. epaint keeps a single-channel glyph coverage atlas and the
  renderers blend with one scalar alpha; per-channel coverage needs a
  3-channel atlas plus dual-source blending or per-channel write masks
  (upstream: emilk/egui#2639). Measured: 0 RGB channel spread in our text vs
  visible fringing in the reference Task Manager capture. The `text_smoothing`
  setting tunes grayscale weight and grid-fitting, which is all that is
  reachable.
- **Software rendering performance:** `render_mode = software` uses WARP and
  costs ~14 cores at 2.9 fps on a 16-thread desktop. The cost is fixed per
  frame (window size and UI content do not change it), so it is a wgpu/WARP
  property, not something the app can optimize. Kept as an explicitly warned
  option; `compatibility` (OpenGL) is the fallback that stays real-time.
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

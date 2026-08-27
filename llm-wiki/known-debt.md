# Known and Accepted Debt

Last verified: 2026-08-26

Primary sources:
- `AGENTS.md`
- `audit.md` (2026 parity audit; phased plan)
- `crates/` (current behavior is the source of truth)

## Purpose

Debt that has been deliberately accepted, with the reasoning, so later
audits do not re-derive it and later agents do not "fix" it without
weighing the same trade-off. Items here are *recorded*, not endorsed.

## Open audit phases (accepted scope, not yet implemented)

The 2026 audit (`audit.md`) defined six phases. **Phase 1 (correctness) was
implemented on 2026-08-26** (see `log/recent.md`). Phases 2–6 remain open
by design; do not re-audit them as new findings:

- Phase 2 table parity: generic column registry (Details Select columns
  already persist visibility/order overrides since 2026-08-27; the generic
  registry stays open), additional columns (Threads/Handles/Base priority/
  Command line/…), CPU Utility as a second explicit metric + metric
  switcher, sorting for Startup/App History/Users/Services.
- Phase 3 telemetry: ETW/SRUM per-process network provider, native App
  History source, real Startup Impact thresholds, packaged startup tasks,
  Shell-link publisher resolution via IShellLink/IPersistFile, Memory
  composition categories, adapter IP details, per-GPU-engine history.
- Phase 4 visual parity: memory composition bar, native graph stroke/fill
  tokens, resource-specific colors, removal of inline CPU graph controls,
  responsive logical-CPU grid.
- Phase 5 advanced features: process dumps on Processes tab, live kernel
  dumps for System, dump settings, wait-chain analysis, processor-group
  affinity (>64 logical CPUs).
- Phase 6 shell/a11y: Settings navigation page, keyboard nav pass,
  high contrast, screen-reader semantics, monitor-aware position restore.
- Current `cpu_load.rs` matches CURRENT (2025+) Task Manager time-based CPU;
  the legacy frequency-weighted "CPU Utility" provider does NOT exist yet.
  Do not rewrite the time-based accountant to imitate utility.

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

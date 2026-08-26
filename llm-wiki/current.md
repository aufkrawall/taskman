# Current State

Last cross-checked: 2026-08-27

## Summary

Windows Task Manager-style desktop app (Rust, eframe/egui), three-crate
workspace. The large audit/implementation plan in `implement.md` has been
implemented to the extent verifiable without interactive Windows sessions.
The 2026 parity audit (`audit.md`) re-baselined remaining work into six
phases; **Phase 1 (correctness) is complete** — see
`log/recent.md` and `known-debt.md` for what changed and which deviations
are accepted (Phases 2-6 open).

## Recently landed (2026-08-27)

- Startup architecture: single collector built lazily ON the engine thread
  after the first presented frame; no duplicate platform stack; console
  attach only for CLI modes; early ring logging with deferred file attach;
  async font + app-history loading.
- Event-driven UI: engine publications and worker completions request
  repaint; interval polling removed; toast ids stable; toasts drive timed
  repaints only while visible.
- Correctness fixes: table/splitter resize math accumulates per-frame
  `drag_delta()` onto the live width (drag-start snapshots were wrong —
  `drag_delta()` is NOT cumulative in egui); F5 forces
  one sample even when paused (state preserved); StartupApproved folder
  subkey path bug; minidump CREATE_ALWAYS truncation; non-blocking
  `run_new_task`; PID-reuse-safe process identity for destructive actions;
  app-history identity by exe path + start-time guard against recycled PIDs.
- Tables: typed ColumnId catalog for Details (every column sorts its own
  field — unit-tested), ID-keyed persisted widths with positional-schema
  migration, O(1) layout geometry, fixed-height virtualization everywhere,
  three Processes groups (Apps/Background/Windows), arbitrary-depth tree
  with cycle-safe O(n) subtree aggregation.
- Processes presentation parity: app grouping is window-ownership driven
  instead of a literal PPID tree. Explorer/console-shell launch boundaries
  keep user-launched programs as independent top-level Apps; app resource
  aggregates follow the display topology; `Apps (N)` counts app groups like
  native Task Manager while Background/Windows keep process counts. Raw PPID
  remains untouched.
- Telemetry: TelemetryDemand gating (GPU/PDH groups warm on demand),
  LUID-aware multi-GPU merge (busiest-engine semantics, dominant engine
  label per process), real token elevation/UAC virtualization/EcoQoS state,
  time-based graph windows with timestamp-proportional x positions, CPU
  Overall/Logical modes with kernel-times overlay.
- Build: `build.py` release driver (host + Linux by default), profiles
  tuned for compile speed (thin LTO, parallel codegen, line-tables-only
  debuginfo).
- UI text/alignment pass: centralized text-style ladder in
  `theme.rs::install_visuals` (Body/Button 12.5 = hand-painted chrome,
  Heading 15.5 = tab titles, Monospace 12.0) so egui widgets stop leaking
  defaults (13/18) next to custom text; Performance pages share one 16 px
  gutter (`GUTTER`) across captions, charts, core grid and stats;
  table headers draw the aggregate exactly once and anchor the sort caret
  to the label edge (never column-center); painter-drawn texts that can
  overflow (perf cards, kv rows, stats) go through `ellipsize()`;
  Users/Services/Startup show the centered "Gathering data" placeholder
  while their background fetch runs; `cmd_button` measures its label
  instead of a per-char heuristic.

## Routing

- Build/release: `build.md`.
- Layout: `repo-map.md`.
- Style: `codestyle.md`. Diagnostics: `debug-tools.md`.
- Accepted gaps: `known-debt.md`. Chronology: `log/recent.md`.

## Open Threads

- Analyze wait chain, processor-group-aware affinity (>64 CPUs), packaged/
  MSIX startup tasks, ETW per-process network, live kernel dump, AccessKit
  accessibility pass — see `known-debt.md` for scope notes.

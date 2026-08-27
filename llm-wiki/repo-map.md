# Repo Map (code map)

Last cross-checked: 2026-08-27

Primary sources:
- workspace tree (verified against working tree)
- `build.py`, `Cargo.toml`
- `crates/*/src/lib.rs`

## Core Tree

Rust workspace, three crates in a strict dependency chain
(`tm-core` ← `tm-platform` ← `tm-app`):

- `crates/tm-core`
  - Platform-agnostic heart. `model.rs` (Snapshot data model — CPU, memory,
    disks, networks, GPU incl. `AdapterLuid`, processes incl.
    elevation/UAC/power-throttle fields), `engine.rs` (sampling engine:
    lazy start via collector factory, event notifier, Refresh-while-paused),
    `settings.rs` (INI config + ID-keyed column prefs + debounced
    SettingsWriter), `app_history.rs` (per-app usage db, single serialized
    writer thread with generations), `demand.rs` (TelemetryDemand bitmask),
    `logging.rs` (early ring sink → deferred file attach), `classify.rs`
    (Apps/Background/System classification), `i18n.rs` (DE/EN keys macro),
    `format.rs`, `mock.rs`.
- `crates/tm-platform`
  - OS collectors/actions behind traits (`actions.rs`). Windows stack under
    `win/`: `sampler.rs` (sysinfo + NtQuerySystemInformation CPU accountant,
    time-based attr TTL cache with token security + command-line queries,
    measured CPU speed from PDH `% Processor Performance`), `cpu_load.rs`
    (time-based CPU accounting incl. kernel/user split), `perfcounters.rs`
    (PDH split GpuPdh/DiskPdh groups with demand gating + LUID-preserving
    GPU instance parser), `gpu.rs` (DXGI discovery + LUID-keyed merge,
    busiest-engine semantics), `process_ops.rs` (kill/suspend/priority/
    affinity/EcoQoS/token security/minidump CREATE_ALWAYS/non-blocking
    launch), `startup.rs` (Run keys + Startup folders + StartupApproved
    incl. the folder-subkey fix), `services.rs`, `users.rs`, `net_info.rs`,
    `version.rs` (cached PE metadata). Linux/macOS backends exist and are
    built by default (`build.py`).
- `crates/tm-app`
  - eframe GUI. `main.rs` (startup sequence: args → console only for CLI →
    early logging → lazy engine factory; StartupTrace markers), `app.rs`
    (TaskManApp: engine starts AFTER first frame, event-driven repaints,
    action executor, toast ids, demand updates per tab), `app_ui.rs`
    (chrome + dialogs incl. settings), `tabs/*` (processes/details/users/
    services/startup/apphistory/performance; Processes derives a presentation
    ownership topology from window owners and shell-launch boundaries rather
    than treating raw PPID as UI ownership), `widgets/tablekit.rs` (TM-style
    tables: drag-start-width resize math, O(1) layout, `scrolled_rows`
    virtualization), `widgets/chart.rs` (timestamp-aware charts, kernel
    overlay), `icon_cache.rs` (lazy worker, upload budget, bounded LRU),
    `fonts.rs` (async system-font load after first frame),
    `action_executor.rs`.

## Important Support and Output Paths

- `target/` — build artifacts (gitignored, safe to delete)
- `dist/` — packaged releases from `build.py` (gitignored)
- `bench/` — framework comparison apps + timing scripts
- `tools/` — capture/UI-test PowerShell helpers
- `implement.md` — audit + implementation plan this codebase follows
  (§ numbering is referenced from code comments)

## High-Risk / High-Value Files

- `crates/tm-core/src/engine.rs` — startup/lazy-start/notifier semantics;
  breaking these regresses the core architecture goals.
- `crates/tm-platform/src/win/perfcounters.rs` — PDH group lifecycle +
  GPU instance parsing (unit-tested real-world strings).
- `crates/tm-platform/src/win/cpu_load.rs` — hand-rolled NT structure
  offsets; verify against Process Hacker definitions before "fixing".
- `crates/tm-app/src/widgets/tablekit.rs` — resize math MUST accumulate
  each frame's `drag_delta()` onto the LIVE width; `drag_delta()` is
  per-frame movement (NOT cumulative) in egui 0.36, so frozen drag-start
  widths pin boundaries at their start position. Regression-tested with
  real pointer events (`dragging_name_boundary_tracks_cursor_across_frames`).
  Resize handles are registered after ALL header cells so they win hit
  testing across their full ±6 px; double-click restore is detected via
  input state because drag-only widgets never receive click flags.
- `crates/tm-app/src/tabs/processes.rs` — Processes UI intentionally uses a
  presentation-only app tree for Apps (Explorer/common shells are launch
  boundaries, independently discovered windowed app roots are detached from
  raw PPID, aggregates follow the display topology), while Background/Windows
  groups are FLAT lists (TM parity — a busy build tool under a console shell
  must stay identifiable) and busy absorbed external tasks (≥ 1 % CPU,
  different image) are promoted into Background. Do not collapse this back
  to a literal PPID tree; raw `ProcessEntry.ppid` remains OS truth elsewhere.

## Test Matrix

- `cargo test -p tm-core` — model/settings/history/engine/classify units.
- `cargo test -p tm-platform` — pure logic (GPU parsing, merges) + Windows
  integration tests (spawn/kill children, live sample sanity).
- `cargo test -p tm-app` — table/process/performance/detail logic tests.
- Headless smoke: `taskman --selfcheck [--mock]`.

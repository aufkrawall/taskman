# Repo Map (code map)

Last cross-checked: 2026-08-31

Primary sources:
- workspace tree (verified against working tree)
- `build.py`, `Cargo.toml`
- `crates/*/src/lib.rs`

## Core Tree

Rust workspace with four crates. The GUI and Windows service both reuse the
platform boundary (`tm-core` ← `tm-platform` ← `tm-app` / `tm-service`):

- `crates/tm-core`
  - Platform-agnostic heart. `model.rs` (Snapshot data model — CPU, memory,
    disks, networks incl. optional IP/signal metadata, GPU incl.
    `AdapterLuid`, processes incl.
    elevation/UAC/power-throttle fields), `engine.rs` (sampling engine:
    lazy start via collector factory, event notifier, Refresh-while-paused),
    `settings.rs` (INI config + persisted Details tree view, per-table sort +
    per-image priority/affinity rules, tray/autostart options +
    bounded 4 MiB startup input +
    ID-keyed column prefs: widths, visibility overrides and user order under
    `[columns.<table>]` / `.visible` / `.order` sections + debounced
    SettingsWriter), `app_history.rs` (per-app usage db, single serialized
    writer thread with generations), `demand.rs` (TelemetryDemand bitmask),
    `logging.rs` (early ring sink → deferred file attach; elevated installer
    helper remains memory/console-only), `classify.rs`
    (Apps/Background/System classification), `i18n.rs` (DE/EN keys macro),
    `format.rs`, `mock.rs`.
- `crates/tm-platform`
  - OS collectors/actions behind traits (`actions.rs`). Windows stack under
    `win/`: `sampler.rs` (sysinfo + NtQuerySystemInformation CPU accountant,
    time-based attr TTL cache with token security + command-line queries,
    measured CPU speed from PDH `% Processor Performance`, interrupt-time
    counter, synthetic CPU pseudo-rows "System Interrupts"/"Terminated
    processes"), `cpu_load.rs`
    (time-based CPU accounting incl. kernel/user split, new-process credit,
    unattributed-residual + exited-image attribution), `perfcounters.rs`
    (PDH split GpuPdh/DiskPdh groups with demand gating + LUID-preserving
    GPU instance parser), `gpu.rs` (DXGI discovery + LUID-keyed merge,
    busiest-engine semantics), `process_ops.rs` (kill/suspend/priority/
    affinity/EcoQoS/UAC virtualization/token security, identity-safe minidumps, ToolHelp module
    enumeration, guarded same-architecture DLL unload, non-blocking launch),
    `startup.rs` (Run keys + Startup folders + StartupApproved incl. the
    folder-subkey fix and best-effort publisher resolution), `services.rs`,
    `users.rs`, `net_info.rs` (cached adapter/link/IP/SSID/signal metadata),
    `net_etw.rs` (demand-gated real-time ETW session for per-process network
    bytes; needs elevation, reports unknown rather than zero without it),
    `core_service.rs` (versioned authenticated named-pipe broker plus secure
    SCM/Program Files/ProgramData install lifecycle, including pinned
    reparse/hard-link-resistant owner/group/DACL repair), `autostart.rs` (owned-command-
    only HKCU startup migration), `windows_enum.rs` (one-pass top-window/hung-state inventory),
    `version.rs` (cached PE metadata). Linux/macOS backends exist and are
    built by default (`build.py`).
- `crates/tm-app`
  - eframe GUI. `main.rs` (startup sequence: args → console only for CLI →
    early logging → lazy engine factory; StartupTrace markers), `app.rs`
    (TaskManApp: engine starts AFTER first frame, event-driven repaints,
    action executor, toast ids, demand updates per tab), `app_ui.rs`
    (chrome + dialogs incl. scrolling settings and Delete confirmation),
    `tabs/*` (processes/details/modules/users/services/startup/apphistory/
    performance; Processes keeps native grouped presentation, Details can
    switch between flat and literal raw-PPID tree; Modules is an async
    on-demand inspector with guarded unload),
    `widgets/tablekit.rs` (TM-style
    tables: drag-start-width resize math, O(1) layout, `scrolled_rows`
    virtualization), `widgets/chart.rs` (timestamp-aware charts, kernel
    overlay), `icon_cache.rs` (lazy worker, upload budget, bounded LRU),
    `fonts.rs` (async system-font load after first frame),
    `action_executor.rs`.
- `crates/tm-service`
  - Windows service executable. Starts under SCM as delayed-auto LocalSystem,
    raises only the control plane to above-normal priority, attaches protected
    ProgramData logging, reports `RUNNING` only after the broker is ready, and
    owns no telemetry/UI state. Non-Windows builds are an explicit stub.

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
  offsets; verify against Process Hacker definitions before "fixing" — but
  note the live-kernel test proved `ImageName.Buffer` is an ABSOLUTE
  pointer into the output buffer on this build (PH's record-relative
  convention decoded 0/285 names); `parse_image_name` tries all
  conventions, validated, and the live test pins it.
- `crates/tm-app/src/widgets/tablekit.rs` — the blue value band is OPAQUE and
  is painted after the row fill, so `row()` records its selection/hover color
  in `row_overlay` and `heat_cells` re-applies it; dropping that makes hover
  stop dead at the first value column. Row height is per-table (`row_h`,
  `ROW_H_DENSE`) and `scrolled_rows` must virtualize on it, never on `ROW_H`.
  Resize math MUST accumulate
  each frame's `drag_delta()` onto the LIVE width; `drag_delta()` is
  per-frame movement (NOT cumulative) in egui 0.36, so frozen drag-start
  widths pin boundaries at their start position. Regression-tested with
  real pointer events (`dragging_name_boundary_tracks_cursor_across_frames`).
  Resize handles are registered after ALL header cells so they win hit
  testing across their full ±6 px; double-click restore is detected via
  input state because drag-only widgets never receive click flags.
- `crates/tm-app/src/app.rs` — `TaskManApp.history` (Performance-chart data)
  MUST stay a contiguous, append-ordered `Vec<HistoryPoint>`; it was a
  `VecDeque` whose ring wrap once froze all Performance charts (see
  `log/recent.md` 2026-08-27). `performance::window` slices it directly.
- `crates/tm-app/src/tabs/processes.rs` — Processes UI intentionally uses a
  presentation-only app tree for Apps (Explorer/common shells, shell-session
  brokers and browsers are launch boundaries; a windowed process folds into
  a windowless ancestor only when plausibly the same application — same
  image or publisher), while Background/Windows groups are flat lists where
  connected same-image families collapse into expandable `Name (N)` rows and
  busy absorbed external tasks (≥ 1 % CPU, different image, windowless) are
  promoted into Background. Do not collapse this back to a literal PPID
  tree. Raw `ProcessEntry.ppid` remains OS truth and is exposed by the
  persisted System Informer-style tree on the Details page, which is expanded
  by DEFAULT (`details::State.collapsed` tracks the exceptions) so newly
  appearing subtrees are never silently hidden.
- `crates/tm-platform/src/win/core_service.rs` — privileged trust boundary.
  Do not add arbitrary commands, output paths, or telemetry to its protocol.
  Preserve exact PID+creation-time checks, client/server executable binding,
  bounded framing/queues, protected ACLs, reparse rejection, and pinned-copy
  upgrade semantics. See `core-service.md`.
- `crates/tm-platform/src/win/process_ops.rs` module unload — remote
  `FreeLibrary` is intentionally guarded by exact process creation timestamp,
  exact module base/path re-enumeration, same-architecture checks, and
  system/main-image refusals. Keep it off the sampler and UI thread, and never
  weaken the second confirmation in `tabs/modules.rs`.

## Test Matrix

- `cargo test -p tm-core` — model/settings/history/engine/classify units.
- `cargo test -p tm-platform` — pure logic (GPU parsing, merges) + Windows
  integration tests (spawn/kill children, live sample sanity).
- `cargo test -p tm-app` — table/process/performance/detail logic tests.
- `cargo test -p tm-service` — service entry/build surface (broker unit tests
  live in `tm-platform`).
- Headless smoke: `taskman --selfcheck [--mock]`.
- Service headless smoke: `taskman-service --selfcheck` (does not install or
  start the service).

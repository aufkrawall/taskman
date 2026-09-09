# Repo Map (code map)

Last cross-checked: 2026-09-02

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
    (conservative Apps/Background/System classification: kernel names + core
    OS images + `IsProcessCritical`; `is_core_os_image` is the shared
    core-image list used by the sampler's tree boundaries and the Processes
    page), `i18n.rs` (DE/EN keys macro), `format.rs`, `mock.rs`.
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
    affinity/EcoQoS/UAC virtualization/token security, `token_identity`
    (account + string SID) and `IsProcessCritical`, SCM service catalog
    mapping PID → executable path, hosted service display name(s) and
    configured account, identity-safe minidumps, ToolHelp module
    enumeration, guarded same-architecture DLL unload, non-blocking launch),
    `startup.rs` (Run keys + Startup folders + StartupApproved incl. the
    folder-subkey fix and best-effort publisher resolution), `services.rs`,
    `users.rs`, `net_info.rs` (cached adapter/link/IP/SSID/signal metadata),
    `net_etw.rs` (real-time ETW session for per-process network bytes; the
    session name MUST stay fixed per role — a pid in it leaks an orphaned
    session on every kill until the provider stops delivering events),
    `core_service.rs` (versioned authenticated named-pipe broker, including bounded identity-bound process-security/module reads for protected targets, plus secure
    SCM/Program Files/ProgramData install lifecycle, including pinned
    reparse/hard-link-resistant owner/group/DACL repair), `autostart.rs` (owned-command-
    only HKCU startup migration), `taskmgr_replacement.rs` (owned IFEO
    `Debugger` registration for taskmgr.exe plus the guards that keep a
    registration launchable, and the explicit built-in-Task-Manager escape
    hatch which debug-creates taskmgr and immediately detaches so IFEO stays
    installed), `instance.rs` (session-local instance
    coordination: mutex + show/acknowledge events + a published pid/HWND
    section, low-integrity-labelled so an unelevated hotkey launch reaches
    an elevated instance; an unacknowledged request starts its own
    instance), `windows_enum.rs` (one-pass top-window/hung-state inventory;
    UWP `ApplicationFrameWindow`s are attributed to their hosted
    `Windows.UI.Core.CoreWindow` process, cloaked suspended-app CoreWindows
    count as App windows and cloaked ghost frames are ignored),
    `window_chrome.rs` (DWM caption colour / dark mode / backdrop / cloaking,
    plus an event-driven strict-topmost keeper that reasserts the root HWND on
    foreground/show/reorder events so topmost shell surfaces cannot stay above it),
    `version.rs` (cached PE metadata). Linux/macOS backends exist and are
    built by default (`build.py`).
- `crates/tm-app`
  - eframe GUI. `main.rs` (startup sequence: args → console only for CLI →
    early logging → hand a duplicate launch to the running instance → lazy
    engine factory; StartupTrace markers; the tray thread — icon, event
    handler and the native popup menu all live on a dedicated thread because
    `TrackPopupMenuEx` pumps a modal loop that must never re-enter the
    UI thread's egui/winit dispatch; the UI side is atomics +
    `PostThreadMessageW` — and the cloak-restore dance), `app.rs`
    (TaskManApp: engine starts AFTER first frame, event-driven repaints,
    action executor, toast ids, demand updates per tab), `app_ui.rs`
    (chrome + dialogs incl. scrolling settings and Delete confirmation),
    `tabs/*` (processes/details/modules/users/services/startup/apphistory/
    performance; Processes keeps native grouped presentation, Details can
    switch between flat and literal raw-PPID tree, offers optional combined/receive/send
    per-process network-rate columns backed by the same on-demand ETW source as Processes,
    and owns a bounded, identity-safe Process Properties inspector (General/Statistics/Security);
    its Windows Security page lazily queries PPL/protection, critical state,
    image machine and DEP/ASLR/CFG/CET/Win32k/code-integrity/image-load and
    related mitigation policies instead of adding them to the hot sampler;
    Modules is an async on-demand inspector with guarded unload),
    `selection.rs` (multi-row process selection shared by Processes and
    Details: native click gestures, identity-keyed, primary vs. full set),
    `widgets/tablekit.rs` (TM-style
    tables: drag-start-width resize math, O(1) layout, `scrolled_rows`
    virtualization with identity-keyed scroll anchoring across model
    rebuilds), `widgets/menu.rs` (classic full-width Windows-style
    context menus: uniform 28 px gapless rows, painted check gutter,
    submenus), `widgets/chart.rs` (timestamp-aware charts, kernel
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
- `implement.md` — **REMOVED** (commit `fc4ecb7c`). The `implement.md §N`
  citations still in ~30 source comments and in `AGENTS.md` are historical
  provenance, not a live cross-reference: the sections they name no longer
  exist anywhere in the tree. Treat them as "this decision came from the
  original plan"; the current behavior is the code and, for accepted
  trade-offs, `known-debt.md`. Do not add new `§` citations.

## High-Risk / High-Value Files

- `crates/tm-core/src/engine.rs` — startup/lazy-start/notifier semantics;
  breaking these regresses the core architecture goals. While a lazy engine is
  parked before `Start`, configuration commands MUST be remembered rather than
  dropped: the UI ships its telemetry demand on its first frame (before the
  engine starts) and only re-sends on change, so a discarded `SetDemand`
  disables that provider for the whole session — this is exactly how
  per-process network stayed dark. See `log/recent.md` 2026-08-31.
- `crates/tm-platform/src/win/perfcounters.rs` — PDH group lifecycle +
  GPU instance parsing (unit-tested real-world strings).
- `crates/tm-platform/src/win/cpu_load.rs` — also the ONLY source of process
  identity and priority for the ~half of the process list that refuses
  `OpenProcess`: `start_epoch_of` and `base_priority` read the retained raw
  kernel table (never `LoadSample`, which needs two ticks to exist).
  sysinfo's `start_time()` is 0 for those processes and must never be stored
  as `Some(0)` — no handle check can ever match it. Hand-rolled NT structure
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
  `scrolled_rows` also anchors the vertical offset to the identities of the
  previously visible rows (`ScrollAnchor` + `stable_key`) on model-rebuild
  frames, preferring the primary selected row while it is visible, so
  insertions/removals around the viewport cannot shift the content;
  `model_changed` must only be set when the caller actually rebuilt the model.
  Resize math MUST accumulate
  each frame's `drag_delta()` onto the LIVE width; `drag_delta()` is
  per-frame movement (NOT cumulative) in egui 0.36, so frozen drag-start
  widths pin boundaries at their start position. Regression-tested with
  real pointer events (`dragging_name_boundary_tracks_cursor_across_frames`).
  Resize handles are registered after ALL header cells so they win hit
  testing across their full ±6 px; double-click restore is detected via
  input state because drag-only widgets never receive click flags.
  `TmTable::row` takes a row-OWNER key (pid+start epoch, base address, …):
  the response id is keyed by it because egui binds an open context-menu
  popup to the response id, and an index-derived id hands the open menu to
  whichever row next occupies the slot in a re-sorting list.
- `crates/tm-app/src/app.rs` — `TaskManApp.history` (Performance-chart data)
  MUST stay a contiguous, append-ordered `Vec<HistoryPoint>`; it was a
  `VecDeque` whose ring wrap once froze all Performance charts (see
  `log/recent.md` 2026-08-27). `performance::window` slices it directly.
- Row naming is deliberately different per page and is NOT a candidate for
  unification: `details::detail_name` is the image name alone (the diagnostic
  page; the description is an optional column there), while
  `processes::process_display_name` is `Description (image.exe)` (the app
  list). Process counts on Processes use SQUARE brackets — `Microsoft Edge
  (msedge.exe) [24]`, `Apps [7]` — because round ones mean an image name.
- `crates/tm-app/src/tabs/processes.rs` — Processes UI intentionally uses a
  presentation-only app tree for Apps (Explorer/common shells, shell-session
  brokers and browsers are launch boundaries; a windowed process folds into
  a windowless ancestor only with positive ownership evidence — the same
  image, or a helper-like image with a matching publisher; missing publisher
  metadata never merges different visible executables. Common app/game
  launchers keep their helper UI but launched titles become peer App rows.
  Background/Windows groups are flat lists where a process's own application
  collapses into an expandable `Name (N)` row and busy absorbed external tasks
  (≥ 1 % CPU, different image, windowless) are promoted into Background.
  `joins_family` lets the same image join unconditionally; different images
  require same-publisher + helper-like evidence while windowless, idle and away
  from a system/launch boundary. Repeat runs of one image under one
  parent group separately (`sibling_run_key`), including `svchost.exe` under `services.exe`,
  which collapse into one `Service Host [N]` row whose expanded children are
  named `Service Host: <hosted service>` (tooltip lists them all; no
  per-service row spam) and whose Process Properties show the full list.
  Windows-process membership is NOT inherited from Session 0 or system ancestry:
  it requires a core OS image name or `IsProcessCritical`, so third-party SCM
  services and Microsoft background machinery (WMI Provider Host, SmartScreen,
  shell brokers, windowless user tools) remain Background.
  Expandable application/family rows are virtual presentation parents: the
  virtual row owns the aggregate, while expansion reveals every concrete member
  (including the former head/root) as a child with only its own metrics. Flat
  Background/Windows family aggregates use `family_values` — the MEMBERS' own
  values, not the subtree's, because a
  foreign descendant left outside the group is rendered as its own row and must
  not be counted twice. Do not collapse this back to a literal PPID tree. Raw `ProcessEntry.ppid` remains OS truth and is exposed by the
  persisted System Informer-style tree on the Details page, which is expanded
  by DEFAULT (`details::State.collapsed` tracks the exceptions) so newly
  appearing subtrees are never silently hidden. That tree has THREE sort
  states (`details::SortOrder`), not two, and the tree is not a mode: it IS
  the Name column's third state. Clicking any OTHER column must leave it and
  sort purely by that column.
- `crates/tm-app/src/theme.rs` — `ScrollStyle.fade.strength` must stay 0:
  egui's edge fades read as a shadow lying on a dense table rather than as
  depth. `ScrollStyle.floating` must stay TRUE. egui
  decides whether a bar is needed against the OUTER rect for floating bars
  and against the shrunken INNER rect for solid ones, so a solid bar can
  oscillate on width-dependent content. Space reservation comes from
  `floating_allocated_width`, not from turning `floating` off; `tablekit`'s
  header must mirror the body's reservation (`prev_bar_use`).
- `crates/tm-platform/src/win/core_service.rs` — privileged trust boundary.
  Do not add arbitrary commands, output paths, or unbounded/generic telemetry to its protocol.
  Preserve exact PID+creation-time checks, client/server executable binding,
  bounded framing/queues, protected ACLs, reparse rejection, and pinned-copy
  upgrade semantics. See `core-service.md`.
- `crates/tm-platform/src/win/process_ops.rs` module unload — remote
  `FreeLibrary` is intentionally guarded by exact process creation timestamp,
  exact module base/path re-enumeration, same-architecture checks, and
  self/critical-process refusals. WHICH module to unload is the user's call
  (product decision 2026-09-06): there is no protected-module gate. One
  FreeLibrary drops ONE loader reference, so the unload REPEATS the remote
  call (bounded by `MAX_FREE_LIBRARY_CALLS`, identity + base/path
  revalidated per attempt) until the module leaves; the honest
  `ModuleUnloadOutcome { still_mapped, released }` crosses the broker as
  `BrokerValue::ModuleUnload` — never as an error string. Keep it off the
  sampler and UI thread, and never weaken the second confirmation in
  `tabs/modules.rs`.

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

## Rendering

`vendor/egui/` is a vendored fork of egui (subtree, tag 0.36.1). It exists for a
few things that stock egui cannot do: sub-pixel (ClearType) text, a native CPU
renderer, and the keyboard Menu/Application key (`Key::ContextMenu`, fork
divergence #5 in `TASKMAN-FORK.md`).
`vendor/egui/TASKMAN-FORK.md` is the divergence inventory and rebase runbook;
`llm-wiki/render-pipeline.md` is the design. The fork has its own quality gate,
`tools/check-fork.ps1`, because `cargo clippy --workspace` does not reach an excluded
workspace.

# Current State

Last cross-checked: 2026-09-01

## Summary

Windows Task Manager-style desktop app (Rust, eframe/egui), four-crate
workspace with a separate optional Windows core-service executable. The large audit/implementation plan in `implement.md` has been
implemented to the extent verifiable without interactive Windows sessions.
The 2026 parity audit (`audit.md`) is now substantially implemented across
correctness, table interaction, Performance visuals, and advanced process
diagnostics; remaining telemetry and accessibility work is itemized precisely
in `known-debt.md`. Normal GUI startup remains unelevated; privileged controls
can cross a protected, allowlisted service boundary after one explicit install.

## Recently landed (2026-09-01 — owners, multi-select, GPU engines, chrome)

Ten reported gaps in one pass; `log/recent.md` carries the root causes.

- **Process owners resolve natively.** `process_ops::token_user` reads
  `TokenUser` through `PROCESS_QUERY_LIMITED_INFORMATION` (memoized per SID,
  because `LookupAccountSidW` can reach a domain controller) and the kernel
  process table now supplies `session_id` for the protected processes no handle
  opens. The Details User column was "—" for most of session 0.
- **Search covers every field a person would search by** — description, user,
  service name, image path and command line on top of name/publisher/PID —
  cheapest field first. Startup and Services use the same `Query`.
- **Dumps are full-memory dumps.** `MINIDUMP_TYPE(0)` wrote stacks and module
  headers; one unreadable region also failed the whole write.
- **Multi-select** on Processes and Details (`selection.rs`): native list-view
  gestures, identities not indexes, fan-out limited to repeatable commands, and
  a confirmation that names every target when there is more than one.
- **Per-GPU-engine graphs** with a "change graph to" context menu, so NVENC is
  answerable while the 3D engine is pinned (the adapter number is the busiest
  engine, not a sum).
- **Application grouping on Processes** tolerates foreign descendants, absorbs
  idle same-publisher helpers, and collapses repeat runs of one image under one
  parent. Service hosts are deliberately exempt.
- **Native caption** painted to match the strip below it, with immersive dark
  mode and the Windows 11 backdrop request. The limits are in `known-debt.md`.
- Window drags from the strip below the caption start on the button press, the
  search box has a clear button, and the dense list rows are 20 px.

## Recently landed (2026-09-01 — CPU renderer and ClearType text)

egui is now vendored as a fork at `vendor/egui` (subtree, tag 0.36.1). Two things
that stock egui cannot do are implemented there; `llm-wiki/render-pipeline.md` is
the design and `vendor/egui/TASKMAN-FORK.md` is the divergence inventory and
rebase runbook.

- **A native CPU renderer.** New `egui_software` crate plus
  `eframe::Renderer::Software`, presenting through `softbuffer` (`CreateDIBSection`
  + `BitBlt` on Win32). It reimplements `egui_glow`'s pipeline rather than
  inventing one, so a golden-image diff against the GPU is a valid test. Idle cost
  in the tray is 0.04 cores -- the same as wgpu and glow, i.e. rendering is
  effectively free and what remains is the sampling engine.
- **Sub-pixel (ClearType) text**, which the debt list called impossible. Glyphs are
  rasterized at 3x horizontal resolution, filtered to per-channel coverage, and
  blended per channel with gamma and enhanced contrast taken from
  `IDWriteRenderingParams` for the monitor -- including the user's own
  `cttune.exe` calibration. It is gated on the display, the user's font-smoothing
  settings, and per-monitor DPI awareness, because enabling it where it is not
  valid looks worse than grayscale.
- `render_mode = software` keeps its name and its meaning to the user ("no GPU")
  but no longer means WARP at ~3 fps. `Auto` now prefers the CPU renderer and
  falls back to the GPU.
- The fork has its own quality gate, `tools/check-fork.ps1`, run by
  `build.py --check`: `cargo clippy --workspace` does not reach an excluded
  workspace, so without it the fork's crates sat outside the gate entirely.
- `tools/measure-cpu.ps1` measures the app's own CPU cost per renderer.

## Recently landed (2026-08-31 — identity/priority fallback, tray, tree sort)

- `start_epoch_s` is never a fabricated `Some(0)` any more: sysinfo returns 0
  whenever it cannot open a process handle (half the list unelevated), so the
  kernel process table fills it instead. The same table supplies a base
  priority, which resolves the priority class for every process no handle can
  be opened for — unknown priorities went from 130/261 to 1 (pid 0).
- The Details process tree is no longer a mode. It IS the Name column's third
  sort state; clicking any other column leaves it and sorts purely by that
  column. `details_tree_view` migrated into `details_tree_hierarchical`.
- Tray: a single left click restores the window, and the notification-area
  menu follows the app's light/dark theme (undocumented uxtheme ordinals 135
  and 136 — the only way Windows themes popup menus).

## Recently landed (2026-08-31 — menu/scroll-bar/tree polish)

- Context menus are drawn by `widgets/menu.rs`: uniform 28 px full-width rows
  with no gaps, a painted check gutter instead of checkbox widgets, and
  submenus in the same style. Every tab's menus and the `…` overflow menus go
  through it.
- Scroll bars reserve their lane (`floating_allocated_width`) instead of
  painting over the last ~14 px of content. `tablekit` mirrors the body's
  reservation onto the header so the two stay aligned.
- Priority / efficiency-mode / UAC-virtualization changes now invalidate the
  sampler's per-PID attribute cache and refresh AFTER the action lands, so the
  menu stops showing the old value for up to ten seconds.
- The Details tree gained a third sort state: a third click on the sorted
  column (or View ▸ Strict hierarchy) drops the column sort and orders
  siblings by creation time — a literal hierarchy, like System Informer.

## Recently landed (2026-08-31 — per-process network without elevation)

- Broker protocol v2 adds one read-only `ProcessNetworkCounters` request. The
  LocalSystem service hosts the ETW trace and answers a bounded aggregate, so
  the ordinary unelevated GUI shows real per-process rates. This reverses the
  documented "no telemetry endpoint" invariant on purpose; the reasoning and
  the constraints that keep it narrow are in `core-service.md`.
- ETW session names are fixed per role. A pid in the name meant every killed
  process leaked a session, and enough orphans stop Windows delivering events
  to the provider at all — which presents as "nothing uses the network".

## Recently landed (2026-08-31 — per-process network)

- **Engine fix that made it visible:** a parked lazy engine used to discard
  every command except `Start`, so the demand the UI sends on its first frame
  (before the engine starts) was lost, and since `update_demand` only re-sends
  on change, `PROCESS_NET` never reached the collector. Pre-start `SetDemand`
  and `SetInterval` are now honored, with regression tests.
- The Network column uses 1024-based KB/s / MB/s like the Disk column rather
  than TM's fixed Mbit/s, in which real per-process traffic always read "0,0".

- `win/net_etw.rs` supplies the Processes/App History Network column from a
  private real-time ETW session on `Microsoft-Windows-Kernel-Network`,
  accumulating the payload `(pid, size)` prefix of the TCP/UDP data events.
  The event-header PID is deliberately NOT used — kernel network events fire
  in System context.
- Requires administrator rights. Unelevated, the session never starts and
  every process keeps `None` → "—", with a hover explanation; availability is
  all-or-nothing so a fabricated zero can never appear.
- Demand-gated on `TelemetryDemand::PROCESS_NET`, so the session exists only
  while a page that shows the column is visible. Measured overhead 4.0 % of
  one core versus ~3 % without.

## Recently landed (2026-08-31 — text rendering and graphics mode)

- **Text**: Segoe UI Variable (`SegUIVar.ttf`, pinned `wght=400`/`opsz=10.5`)
  replaces the Win10 static Segoe UI, and a new `text_smoothing`
  (`sharp` default / `standard` / `smooth`) controls both halves of glyph
  weight: the coverage→alpha ramp in the visuals' text options and the
  grid-fitting target in each face's `FontTweak`. epaint's defaults switch
  horizontal grid-fitting OFF and lift dark-mode coverage by `2c − c²`, which
  together produce the "blurry and fat" look; `sharp` reverses both. Applies
  live, no restart.
- **Sub-pixel (ClearType) rendering stays impossible** — epaint's glyph atlas
  is single-channel and both renderers blend with one scalar alpha. Measured:
  0 RGB channel spread in our text, visible fringing in the reference Task
  Manager capture. See `known-debt.md`.
- **`render_mode`** (`auto` / `compatibility` / `software`) replaces the
  transient `gpu_acceleration` bool and is read once at startup. `auto` is
  wgpu/D3D12 (0.2–0.3 cores at continuous repaint), `compatibility` forces the
  OpenGL backend (1.0–1.1 cores) for machines with a broken D3D driver, and
  `software` is WARP — correct but ~14 cores at 2.9 fps, with a fixed
  per-frame cost the app cannot influence, so the settings dialog warns about
  it explicitly.

## Recently landed (2026-08-31 — table look, status glyphs and tree)

- **Efficiency mode was never detected.** `GetProcessInformation` with
  `ProcessPowerThrottling` requires `PROCESS_POWER_THROTTLING_STATE.Version`
  set on INPUT; a zeroed struct fails with `ERROR_INVALID_PARAMETER` for
  every pid, so `power_throttled` was `None` system-wide. Fixed in
  `process_ops::efficiency_mode_state` and pinned by an integration test.
  The flag now refreshes on its own 2 s sub-TTL inside the 10 s attribute
  cache, because it is a live status, not a slow-changing attribute.
- **Heat map.** Every numeric cell is painted from a continuous
  `theme::heat_blue` gradient whose floor is `heat_base`, so idle processes
  no longer leave unpainted holes in the blue band. The curve is ease-OUT:
  intensities are normalized against the column maximum, and the previous
  ease-in curve collapsed everything but the top consumer onto the base tint.
- **Row hover reaches the value columns.** `TmTable::row` records its
  selection/hover fill; `heat_cells` re-applies it over the opaque blue band
  it paints. Light mode uses a dark wash instead of an invisible white one.
- **Status column glyphs on Processes**: orange pause (suspended) and green
  leaf (efficiency mode), with the wording in the row tooltip; only
  "not responding" stays as text. A collapsed group row (`Brave Browser (24)`)
  aggregates the leaf over its display subtree exactly as it aggregates
  CPU/memory — that is where native Task Manager shows it.
- **Type-ahead accumulates.** `search::list_type_ahead` buffers keystrokes for
  1 s, so typing "svc" fast selects svchost.exe instead of the last letter's
  first match; a single letter (or one repeated) still cycles and wraps.
- **Dense list pages.** `TmTable::row_h` / `ROW_H_DENSE` (22 px) pack Details,
  Services and Modules like native TM's Details tab; Processes, Users,
  Startup and App History keep the 32 px app-list spacing.
- **Details tree is fully hierarchical by default.** `details::State.collapsed`
  replaces `expanded`, so the tree shows the complete hierarchy at all times
  instead of leaving subtrees that appeared after page load collapsed. Parent
  links whose parent started after the child are rejected
  (`is_plausible_parent`), matching System Informer's PID-reuse guard.

## Recently landed (2026-08-31 — protected service and reliability)

- Added `taskman-service.exe`, a delayed-auto LocalSystem SCM service that owns
  only allowlisted privileged controls. Sampling, rendering, settings, module
  inventory, dumps, shell commands, and user-selected paths stay in the GUI.
- Protocol v1 is a local-only, first-instance named pipe with a protected
  one-user DACL, kernel-derived client/server PID and protected executable-path
  checks, 64 KiB framed requests/responses, unknown-field rejection, two
  workers, and a bounded queue. Explicit authorization/safety rejection never
  falls back to a weaker local path.
- Process actions require exact PID + positive creation time at the broker and
  revalidate it on the action handle. Critical/system/service/requesting-GUI
  targets are refused; tree kill no longer acts on descendants without captured
  creation identity. Module unload also re-enumerates exact base/path and keeps
  main/system/loader/cross-bitness protections.
- Installer copies both binaries into ACL-protected Program Files through a
  pinned, single-link, hash-verified, synchronized staging path and protects the manifest/
  logs in admin/System-only ProgramData. It rejects reparse points and
  hard-linked or foreign-named log files, pins directories/files against conflicting mutation
  handles while assigning protected owner/group/DACL, and never opens a
  per-user file log or writes per-user redirect state from the elevated helper.
  Upgrade waits
  for the old service process; SCM uses a service SID, only
  `SeDebugPrivilege`, delayed auto-start, and 5/15/60-second recovery. See
  `core-service.md`.
- Successful install records a per-user redirect to the protected GUI after
  the SCM start request is accepted; SCM readiness remains independently
  observable. Matching portable launches transfer there before any window
  exists, while a different package hash stays open to repair/upgrade the
  protected generation. Existing autostart and owned Task Manager replacement
  are migrated. Uninstall removes the privileged SCM capability and marker
  while leaving protected files for safe rollback/reinstall.
- Reliability work adds two independent bounded GUI action lanes, explicit
  overload reporting, above-normal (never high/realtime) GUI/service control
  priority, single-instance restore signaling plus a bounded explicit-elevation
  ownership handoff, eventful race-free SCM accept-loop wakeup, lazy tray
  creation, close-to-tray, owned-command-only per-user autostart migration, and
  a 4 MiB fail-closed settings-input cap at startup. Service worker panics
  abort into SCM recovery instead of leaving a falsely healthy half-broker.
- Details now persists literal process-tree mode, table sort, and per-image
  priority/affinity rules. Its menus show current priority/UAC markers, can
  safely toggle UAC virtualization, and use background affinity queries.
  Processes reports suspended, not-responding, and efficiency-mode states.
- Table sort order persists across sessions on Processes, Details, Modules,
  Startup, Services, Users, and App History. Delete on process tables opens an
  identity-bound termination dialog; quiet body column guides improve scanning.
- Final window-free verification passed formatting, warnings-as-errors Clippy,
  and all 166 workspace tests, followed by the host release/package build and
  service protocol self-check. The package contains `taskman.exe` (13,873,664
  bytes) and `taskman-service.exe` (1,378,816 bytes).

## Recently landed (2026-08-31 — GUI parity and diagnostics)

- Details now offers flat and a persisted System Informer-style literal PPID
  tree while Processes retains native-style grouped ownership. The raw tree keeps
  ancestors during search, sorts siblings without destroying hierarchy,
  supports expand/collapse commands and tree-aware keyboard navigation, and
  never exposes actions on synthetic CPU-attribution rows.
- Processes and Details gained arrow/Home/End/Page navigation; Delete requests
  process termination through an explicit dialog. Context menus now include
  consistent copy/search/location/properties/dump/module actions as applicable.
- The on-demand Modules inspector enumerates DLL name/path/base/size outside the
  hot sampling path. Unload is an explicitly dangerous, confirmation-gated action
  restricted to same-architecture third-party DLLs; process creation time and
  exact module base/path are revalidated immediately before the remote
  `FreeLibrary` request. The image and Windows modules are fail-closed.
- Details adds typed optional columns for description, publisher, parent PID,
  session ID, image path, page faults/sec, and I/O read/write totals. Startup,
  App History, Users, and Services headers now sort; tables draw quiet body
  column guides, and settings can reset persisted widths.
- Performance uses lighter chart treatment and resource-specific colors. CPU
  graph controls live in graph context menus, logical CPU tiles adapt to core
  count and width, and Network combines receive/send while showing cached
  native IPv4, IPv6, SSID, signal, link speed, and adapter description.
- Windows WGPU is explicitly D3D12-only, with FIFO/one-frame surface latency
  and low-power adapter preference; Vulkan is not compiled into the Windows
  WGPU backend. Linux remains Vulkan, macOS Metal, and Glow remains a fallback.
  This removes unused WGPU backend dependencies without sacrificing the
  compatibility escape hatch. App-history persistence is coalesced to every
  30 seconds plus clean shutdown instead of writing after every sample.
- Registry startup entries now resolve publisher metadata on their background
  fetch. App History labels unsupported per-process network as unavailable
  (`—`) and states clearly that its database is local rather than Windows SRUM.
- That GUI-parity pass was verified headlessly: the full format/clippy/test gate and
  all 148 tests passed, followed by the host release build; no TaskMan GUI,
  capture helper, or module-unload action was launched. `taskman.exe` is
  13,470,208 bytes, down 1,185,792 bytes (8.09%) from the pre-pass binary.

## Recently landed (2026-08-28)

- Settings dialog (Windows, "Advanced" block): elevation status, a persisted
  "Always start with administrator privileges" policy (startup re-execs
  elevated via runas before the window opens; declined UAC degrades to an
  unelevated start; `TASKMAN_CONFIG_DIR` overrides skip it), and a one-shot
  "Restart as administrator" button (`relaunch_elevated()` on the action
  executor, then a graceful `ViewportCommand::Close` from the executor
  thread; declined UAC surfaces the error toast). Elevation is cached once
  at startup (`TaskManApp::is_elevated`). `tools/capture.ps1` now uses an
  isolated temp config dir. See `log/recent.md` 2026-08-28.
- Chart-freeze fix: `TaskManApp.history` was a `VecDeque` whose ring buffer
  wraps after `history_cap` pop/push cycles; the Performance tab read only
  `as_slices().0` (the OLD half), so all graphs and card sparklines froze on
  stale data for ~cap ticks per ring cycle (one stale-to-fresh blip between
  freezes) — "graphs sometimes stop updating". History is a contiguous,
  append-ordered `Vec` now (see the field doc in `app.rs`; retention via
  `push_history_point`, regression-tested). `visible_slice` also scans
  backward from the newest sample instead of `partition_point`, and
  `chart_multi`'s span math saturates, so a wall-clock step backward (NTP
  correction after resume) degrades to a briefly-too-wide window instead of
  a broken/frozen chart.
- TM-parity resource sorting: the Apps/Background/Windows sections exist
  only when sorted by Name/Status; CPU/memory/disk/network sorts flatten
  everything into one globally sorted list (`sort_blocks_globally` reorders
  depth-0 head blocks with expanded children attached), so the top CPU
  consumer — including the "Terminated processes" pseudo-row — is always
  the first row. Group-collapse toggles apply only to the grouped view.
- CPU attribution completeness: the time-based accountant credits new
  processes their since-creation time and reports the unattributed CPU
  residual (`unattributed_pct`) plus the image names of processes that
  exited during the window. The sampler surfaces the residual as synthetic
  `ProcessEntry` rows (`synthetic: bool`, sentinel pids `u32::MAX`/
  `u32::MAX-1`) — "System Interrupts" (measured `% Interrupt Time` PDH
  counter, Windows group) and "Terminated processes (N)" (Background, with
  exited-image hover tooltip) — shown only above 0.5 % with 5-tick hold
  decay; no context menu on synthetic rows. NT `ImageName.Buffer` is an
  absolute pointer into the output buffer on this build (pinned by a
  live-kernel unit test); the parser also accepts the old offset
  conventions, all validated.
- Details command lines on Windows via
  `NtQueryInformationProcess(ProcessCommandLineInformation)` (= 60 on current
  builds; needs only QUERY_LIMITED), cached in `PidAttrs` (10 s TTL).
- Performance CPU speed is measured (base × PDH `% Processor Performance`,
  demand-gated `CPU_SPEED`; sysinfo/CallNtPowerInformation CurrentMhz only a
  failure fallback — it reports the fixed nominal clock on modern Windows).
- Processes page: Background/Windows groups are flat lists (TM parity — own
  values) where connected same-image families collapse into expandable
  `Name (N)` rows ("Dropbox (7)"); mixed-image trees stay flat so busy CLI
  work under console shells is identifiable. A windowed process folds into a
  windowless ancestor only when plausibly the same application (image or
  publisher; shell brokers + browsers are launch boundaries) — otherwise it
  is its own app row. Busy absorbed external tasks (≥ 1 % CPU share,
  different image, windowless) are promoted into Background
  (`promote_busy_external_tasks`); windowed processes are never demoted.

- Window placement: "Remember window size and position" checkbox in the
  settings dialog (the flag previously had no UI); maximized state is
  persisted in `window-state.ini` and restored at startup, while maximized
  sessions never clobber the stored restore size/position with monitor
  geometry.
- Type-ahead lists: plain-letter navigation on Processes/Details scrolls
  virtualized rows into view vertically-only (one-shot `focus_row` param
  on `tablekit::scrolled_rows`; `Response::scroll_to_me` was both
  virtualization-blind and two-axis). The Performance card column has the
  same type-ahead via `search::cycle_match` (generic over the identity
  type).
- Details' Select columns dialog uses painted chevron up/down buttons,
  offset 16 px left of the floating scrollbar (the old right-aligned text
  arrows were unclickable under it). Its visibility/order choices persist
  per table in `config.ini` (`[columns.<t>.visible]`/`[columns.<t>.order]`
  sections — overrides only, applied at startup with empty-table/sort
  guards; see the 2026-08-27 log entry).
- Table headers reserve the same right content margin as the body
  (`BODY_PAD_RIGHT`): without it, once a table scrolls fully right the
  last resize handle sits flush at the viewport edge and egui's clipped
  hit-testing leaves only a few pixels of it grabbable.
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
- Process-list interaction: plain alphabetic typing on Processes and Details
  selects the next displayed matching process, cycles repeated initials, and
  scrolls virtualized rows into view (vertical-only, virtualization-aware —
  see the 2026-08-27 log entry) without stealing input from text edits.
  Details' Select columns dialog is compact and lets enabled columns move
  up/down in the rendered table while keeping sort semantics ID-based.
- Native window placement: fresh installs start at 1280×800; remembered size
  continues through `config.ini`, while desktop-space position and maximized
  state are restored from `window-state.ini` and written only on a clean
  close when both config autosave and remember-window are enabled.
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
  accessibility pass, production code signing, and disposable-VM service ACL/
  recovery validation — see `known-debt.md` and `core-service.md`.

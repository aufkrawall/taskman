## Scope and baseline

The repository has been audited against:

- The seven native Windows 11 Task Manager reference screenshots in
  `taskmanpngs/` (untracked local captures, kept out of the repository for
  privacy).
- The project's own development screenshots in `shots/` (likewise untracked
  local captures).
- The actual Rust implementation, rather than assuming `implement.md` accurately describes the current state.
- Current Microsoft Task Manager documentation through 2026.

## Implementation update — 2026-08-31

The initial report below was a static audit. The toolchain is now available
and its highest-value recommendations have been implemented and verified with
headless compilation/tests. Interactive GUI capture was intentionally skipped
for this pass so no windows would interrupt the active desktop session.

Implemented since the audit:

- A selectable, persisted literal PPID process tree alongside the native
  grouped Processes view; hierarchy-aware filtering, expansion commands,
  arrow/Home/End/Page navigation, and a confirmation-gated Delete shortcut.
- Broader process context menus, PID-reuse-safe dumps, and an on-demand loaded
  Modules inspector. Module unloading is background-only, double-confirmed,
  same-architecture, exact-process/exact-module revalidated, and blocked for
  the image and Windows system modules.
- Additional typed Details columns (description, publisher, parent PID,
  session, image path, page faults, and I/O totals), sortable secondary tabs,
  body column guides, shared online-search encoding, and resettable widths.
- Performance visual cleanup: lighter charts, resource-specific colors,
  context-menu CPU graph controls, responsive logical-CPU grids, a combined
  network throughput graph, and cached IPv4/IPv6/Wi-Fi signal metadata.
- Windows WGPU now compiles and requests D3D12 only (Vulkan is no longer part
  of the Windows WGPU backend set), prefers the low-power adapter for this 2D
  UI, keeps FIFO/one-frame latency, and retains Glow as a compatibility
  fallback. App-history writes are coalesced to 30 seconds plus shutdown.

Still intentionally open: ETW per-process network, native SRUM App History,
measured Startup Impact and packaged startup tasks, memory-composition and
full GPU-engine histories, CPU Utility, live kernel dumps/wait-chain analysis,
processor-group affinity, full accessibility/high-contrast work, and the 2026
NPU/NPU Engine/NPU memory/Isolation columns. Microsoft documents the latter as
optional current Windows columns; they need new capability-gated telemetry,
not fabricated zero values.

Validation for this update: the full headless gate passed (format, clippy with
warnings denied, and 148 tests across `tm-core`, `tm-platform`, `tm-app`, and
Windows integration), followed by the host release build. The shipped EXE is
13,470,208 bytes, 1,185,792 bytes (8.09%) smaller than the 14,656,000-byte
pre-pass baseline despite the added diagnostics.

---

# 1. CPU behavior — recommended product decision

## Current Microsoft behavior

Windows 11 changed Task Manager in 2025 so Processes, Performance, and Users use the newer standardized CPU workload calculation. The former Task Manager calculation survives as an optional **CPU Utility** column in Details.

## Recommendation for this project

Do **not** throw away the previous metric.

Implement two explicit metrics:

1. **CPU Utility / Legacy Task Manager**
   - Frequency/performance-state-aware Task Manager behavior.
   - Use this as the preferred compatibility mode for the supplied native screenshots.
   - This should be the project's preferred mode unless strict current-Windows parity is selected.

2. **CPU / Current Windows**
   - Time/workload metric matching current 25H2+ Task Manager.
   - Keep available for current-Windows compatibility.

Suggested setting:

`CPU metric: Legacy Task Manager | Current Windows`

Also expose both `CPU` and `CPU Utility` through Details → Select columns.

### Existing code issue

`crates/tm-platform/src/win/cpu_load.rs` currently implements time-based utilization, which is directionally aligned with Microsoft's current metric.

However, its module documentation still states that modern Task Manager uses `% Processor Utility`. That description is stale after the 2025 change.

**Do not simply rewrite the calculation.** Preserve the current accountant and add a separate legacy utility provider.

---

# 2. Critical correctness bugs

These should be fixed before adding additional features.

## P0.1 — Table width architecture does not match Task Manager

**File:** `crates/tm-app/src/widgets/tablekit.rs`

`TmTable::col_width()` treats column 0 specially. `name_effective()` makes the first column consume all unused viewport space.

That is visibly incorrect compared with the supplied native screenshots.

Native Task Manager keeps its normal column widths and simply leaves unused client area on the right. The clone instead stretches Name/Description/User/etc. enormously when the window is wide and pushes numeric columns toward the right edge.

### Fix

Remove implicit first-column expansion.

- Every column should normally use its configured/default/persisted width.
- `TmColumn::text()` with width `0` must not accidentally behave as a giant fill column.
- If an elastic column is desired for non-parity modes, make it an explicit column behavior rather than a rule attached to index zero.
- Horizontal scrolling should appear only when total real column width exceeds the viewport.

This single change affects Processes, App History, Startup, Users, Details, and Services.

---

# 3. Heat-map algorithm is incorrect

## P0.2 — "Top consumer" is calculated within a row instead of within a column

**File:** `crates/tm-app/src/widgets/tablekit.rs`  
**Function:** `heat_cells()`

The code calculates:

`top = max(cells in this row)`

That cannot determine the largest consumer in a CPU, Memory, Disk, or Network column.

For example, Processes supplies approximately:

- CPU intensity = 1 if CPU > 0
- Memory intensity = 1 if memory > 0

As a result, large numbers of rows can receive the bright "top consumer" color.

Native Task Manager's supplied screenshot demonstrates column-oriented highlighting: the most significant consumer in each resource column is highlighted relative to the other processes.

### Fix architecture

Do the normalization **before row virtualization**:

- Compute `max_cpu`, `max_memory`, `max_disk`, `max_network` over the filtered/display model.
- For every row, compute a per-column heat state/value.
- Pass those values into `heat_cells()`.
- `heat_cells()` should paint what it is told; it should not discover global maxima from one row.

This affects:

- Processes
- Users
- App History
- any future resource table

---

# 4. Details GPU telemetry is functionally disconnected

## P0.3 — GPU columns are visible but GPU telemetry is not requested

**Files:**

- `tabs/details.rs`
- `app.rs`

`details::State` contains:

`show_gpu_columns: bool`

It defaults to `false`.

`TaskManApp::update_demand()` requests `PROCESS_GPU` and `PROCESS_GPU_MEMORY` only when `has_gpu_column_visible()` returns true.

However:

- `show_gpu_columns` is never changed by the UI.
- `columns()` always includes GPU and GPU Engine.
- Rows always render those columns.

Therefore the UI can show GPU columns while the sampling engine has deliberately not enabled the telemetry required to populate them.

### Fix

This should be solved as part of a real Details column registry:

`visible columns → telemetry requirements`

For example:

- GPU → `PROCESS_GPU`
- GPU Engine → `PROCESS_GPU`
- Dedicated GPU memory → `PROCESS_GPU_MEMORY`
- Shared GPU memory → `PROCESS_GPU_MEMORY`

Do not maintain a separate `show_gpu_columns` Boolean that can drift from the actual UI state.

---

# 5. Global Task Manager search is not actually implemented correctly

Microsoft Task Manager supports filtering by **binary name, PID, or publisher**, persists the filter while moving between pages, and supports Alt+F.

The clone's search hint promises equivalent behavior, but most implementations only search display names.

## Processes

`build_display_rows()` checks:

- executable name
- shown/display name

Missing:

- PID
- publisher/company

## Details

Same problem.

The model already contains `ProcessEntry.company`, so part of the data required for publisher search already exists.

## Startup

Search only checks startup item name.

It should at least include publisher.

## Users — outright filtering bug

The current condition is effectively:

`if query doesn't match user AND process_count == 0 → hide`

An active user normally has `process_count > 0`, meaning the user remains visible regardless of the search query.

Search therefore mostly fails to filter the Users page.

## Keyboard behavior

Alt+F is missing.

Microsoft also shipped Ctrl+F behavior/fixes, so supporting both is reasonable.

### Fix

Build a shared normalized search matcher rather than duplicating page-specific string comparisons.

Process candidate fields:

- binary name
- display name
- PID
- publisher/company
- optionally description

Add keyboard focus handling for Alt+F, and preferably Ctrl+F.

---

# 6. Processes grouping counters are wrong

## P0.4 — group counters depend on expansion state

The group header appears to derive its count from flattened visible `DisplayRow::Process` entries.

That means expanding/collapsing process trees can change values such as:

`Apps (5)`

or

`Background processes (81)`

Native group counts should represent the group contents, not the current flattened expansion state.

### Fix

Calculate group counts from the unflattened process classification model before creating display rows.

## P0.5 — app process count only counts direct children

`emit_tree()` labels grouped processes using:

`kids.len() + 1`

That only counts immediate children.

The supplied native screenshot has entries such as:

`Brave Browser (43)`

That represents the whole grouped process hierarchy, not just first-generation children.

### Fix

While calculating `subtree_values()`, also calculate `subtree_process_count`.

Then:

`display_count = subtree_process_count[root_pid]`

Do this in O(n), not recursively per rendered row.

---

# 7. Toolbar End Task loses the project's own PID-reuse protection

The context-menu process termination path constructs a `ProcessIdentity` containing:

- PID
- start epoch

and validates it before termination.

But `TaskManApp::end_selected()` uses only:

`selected_pid: Option<u32>`

and directly calls:

`kill_process(pid, false)`

Therefore the toolbar's End Task command does not have the same identity protection as the context menu.

A PID could theoretically be recycled between selection and execution.

### Fix

Store:

`selected_process: Option<ProcessIdentity>`

instead of a bare PID for process selection, or resolve and validate the selected identity immediately before dispatch.

Apply the same identity validation to delayed operations such as:

- Efficiency mode
- priority changes
- suspend/resume
- affinity
- dump creation where appropriate

---

# 8. Efficiency mode state is not sourced from Windows

The Windows sampler already retrieves:

`ProcessEntry.power_throttled`

using `efficiency_mode_state(pid)`.

But the UI ignores that field.

Instead it maintains:

`app.efficiency_pids: HashSet<u32>`

which starts empty and is only changed after the clone itself toggles Efficiency mode.

Consequences:

- A process already in Efficiency mode before opening the clone appears normal.
- Changes made externally are not reflected.
- An API failure can leave the UI showing the optimistic state.
- PID reuse can leave stale state associated with the wrong process.

### Fix

The snapshot should be the source of truth.

Render Efficiency mode from:

`ProcessEntry.power_throttled == Some(true)`

After issuing an action:

1. show a pending state if needed;
2. trigger a fresh process attribute sample;
3. update the icon/menu from the returned OS state.

Remove `efficiency_pids` as authoritative state.

---

# 9. Performance → Refresh now does nothing

**File:** `tabs/performance.rs`

The overflow command contains:

`if RefreshNow clicked { ui.close(); }`

It does not call:

`app.refresh_all()`

This is a direct functional bug.

### Fix

Call:

`app.refresh_all();`

before closing the menu.

Add a regression test for every tab's Refresh now command.

---

# 10. Graph-history window can become shorter than requested

`history_cap` is calculated at startup from:

- update interval
- `graph_seconds`

Changing graph duration in Settings later changes the requested visible window but does not recalculate the deque capacity.

Example:

- start with 60 seconds
- change to 120 seconds
- history deque may still only be sized for the original duration

### Fix

Either:

- allocate for the maximum supported graph duration at startup, or
- recalculate and resize capacity whenever `graph_seconds` or sampling interval changes.

The maximum-allocation method is simpler and avoids repeated reallocation.

---

# 11. App History is not Windows Task Manager App History

This is an important semantic difference, not just a missing UI feature.

The source code explicitly acknowledges that it accumulates the clone's own observations rather than reading Windows' historical database.

Therefore:

**Native Task Manager**

Shows historical OS-maintained application CPU/network activity.

**Clone**

Shows only activity observed while this application has been running.

Microsoft describes App History as resource usage, including CPU time and network activity, for applications **over time**.

### Required parity implementation

Use the Windows historical resource data source, normally SRUM/SRUM-related infrastructure or another appropriate native source.

The clone-owned database can remain as:

`Fallback history source`

for platforms without a native provider.

But on Windows it should not be presented as equivalent to Task Manager history.

## Additional App History problems

- Network data currently cannot distinguish "no network telemetry available" from genuine zero usage in persisted `AppUsage`.
- The database is queued for saving after every published engine sample.
- With approximately one-second sampling and a 200 ms writer debounce this can produce a full JSON rewrite roughly every second.

### Optimization

Track dirty state and save at a much lower cadence, e.g. every 15–60 seconds plus shutdown.

---

# 12. Per-process Network is a major missing metric

On Windows, `ProcessEntry.net_*` remains optional/unavailable.

Consequently:

- Processes Network is unavailable.
- Users Network is shown as `—`.
- App History cannot reproduce native network history.

Native Task Manager shows these values.

### Implementation direction

Do not fake them from system adapter traffic.

Implement a Windows process-attribution provider, likely ETW/SRUM-backed depending the metric required.

This should be treated as a dedicated telemetry subsystem because accurate attribution, process lifetime handling, PID reuse, and overhead matter.

---

# 13. Startup Apps has several correctness gaps

Microsoft defines Startup impact as:

- **None:** app disabled
- **Not measured:** enabled but no data
- Low: CPU <300 ms and disk <292 KB
- Medium: CPU 300 ms–1 s or disk 292 KB–3 MB
- High: CPU >1 s or disk >3 MB

The Windows backend currently sets every discovered item to:

`StartupImpact::Unknown`

Therefore even disabled items can display `Not measured` when native Task Manager should show `None`.

## Immediate fix

When disabled:

`impact = StartupImpact::None`

When enabled and unavailable:

`impact = StartupImpact::Unknown`

## Proper implementation

Retrieve the actual Windows startup-impact telemetry and assign Low/Medium/High.

## Publisher resolution is incomplete

Registry Run entries set:

`publisher: None`

Startup-folder items call `resolve_publisher()`, but `.lnk` handling does not actually resolve the Shell Link target. It essentially attempts version lookup against the shortcut file itself.

### Fix

Use Windows Shell Link COM APIs:

- `IShellLink`
- `IPersistFile`

Resolve target executable first, then query version/signature publisher metadata.

Also support packaged/MSIX startup tasks.

Microsoft states that Task Manager presents registered Windows startup applications, not merely classic Run keys and Startup folders.

---

# 14. Details → Select columns is a major missing feature

Current clone catalog has only roughly:

- Name
- PID
- Status
- User
- CPU
- Memory
- Platform
- Elevated
- UAC virtualization
- GPU
- GPU Engine

Native Task Manager supports changing displayed Details columns through right-click → **Select columns**. Microsoft documents this behavior explicitly.

The project even has an unused `SelectColumns` localization key.

### Required architecture

Create a real column registry containing:

- stable ID
- localized label
- default visibility
- default width
- alignment
- renderer
- comparator
- telemetry requirements
- Windows-version/capability requirements

Then implement the Select columns dialog.

Important additions include:

- CPU Utility
- Threads
- Handles
- Base priority
- Command line
- Description
- Path/image name
- I/O metrics
- Dedicated GPU memory
- Shared GPU memory
- package-related fields where supported

Current 2026 Windows builds are also introducing optional NPU/NPU Engine, NPU memory and Isolation/AppContainer columns. These are not necessary for matching the supplied screenshots, but the registry should be capable of adding them without another redesign. Microsoft's June 2026 preview explicitly documents these additions.

---

# 15. Missing process dump functionality

The clone supports a user-mode dump from Details but does not reach full current Task Manager parity.

Task Manager can create user-mode process dumps, and for the System process can create:

- Full live kernel memory dump
- Kernel stacks memory dump

from Processes or Details.

The advanced live-kernel-dump options also exist in native Task Manager Settings.

### Required

Add:

- Create memory dump file to Processes context menu.
- Special live-kernel submenu for System.
- advanced dump settings.
- correct elevation/error handling.
- progress/busy state so multiple dumps cannot accidentally be launched.

---

# 16. Memory Performance page is structurally wrong

The clone currently renders:

1. memory usage time-series
2. committed-memory time-series

Native Task Manager's Memory page uses the large memory-usage graph followed by a **Memory composition** visualization.

The second committed time-series should not be used as the parity layout.

### Required model additions

To reproduce Memory composition accurately, acquire enough Windows memory-list information to represent appropriate categories such as:

- In use
- Modified
- Standby
- Free

Do not estimate these categories from `used_bytes`.

The existing model already contains several useful fields:

- available
- cached
- commit
- paged pool
- non-paged pool
- installed
- hardware reserved
- speed
- slots
- form factor

so the lower statistics area is relatively close.

---

# 17. Performance graph visual treatment is too heavy

`widgets/chart.rs` deliberately paints filled area charts with relatively high alpha.

The supplied native CPU screenshot primarily uses a fine resource-colored line with much more restrained fill.

The clone's final CPU capture has large opaque cyan areas that dominate the chart.

### Fix

Create native Task Manager graph tokens rather than hard-coding alpha in the generic chart:

- thinner graph stroke
- substantially lower fill opacity
- native-like grid stroke
- resource-specific graph colors
- selected-card border and background sampled from reference images

Do not use one global accent for every resource.

Native Performance cards visibly distinguish resources by color, particularly CPU, Memory, Disk, Network and GPU.

---

# 18. CPU graph controls are in the wrong place

The clone displays inline controls for:

- Overall / Logical processors
- Show kernel times

above the CPU graph.

The supplied native screenshot does not.

Task Manager exposes graph configuration from the graph's context menu.

The clone already has a chart context menu, making the inline controls redundant.

### Fix

Remove the inline controls in parity mode.

Keep:

right-click graph → Change graph to → Overall utilization / Logical processors  
right-click graph → Show kernel times

---

# 19. Logical CPU grid should be responsive

The clone hardcodes:

`cols = 4`

Native grid geometry should adapt to the number of logical processors and available detail-area aspect ratio.

Four columns happens to work for the supplied 16-thread screenshot, but scales poorly to 32/64/128 logical CPUs.

### Fix

Calculate candidate `(rows, columns)` layouts and choose the one whose cell aspect ratio most closely matches native graph tiles while fitting the available region.

---

# 20. Network Performance page needs more native data

Current model contains:

- name
- description
- kind
- rates
- totals
- link speed
- SSID

It does **not** contain IP addresses because `GetAdaptersAddresses` deliberately uses `GAA_FLAG_SKIP_UNICAST`.

Native Performance networking exposes additional adapter information.

### Add

At minimum:

- adapter name/model
- connection type
- IPv4 address
- IPv6 address
- link speed
- SSID for Wi-Fi
- signal strength where available

The network Performance layout also needs a visual comparison against the exact targeted Windows build; the clone currently uses separate Receive and Send time-series, which does not match the usual native throughput presentation.

---

# 21. GPU Performance page is substantially incomplete

The model already has:

- adapter
- utilization
- dedicated usage
- shared usage
- temperature
- engine list

but the UI stores history primarily for aggregate GPU utilization and memory.

Native Task Manager's GPU page is engine-oriented.

### Required

Maintain time history per displayed GPU engine and render multiple engine graphs.

Also render dedicated/shared GPU memory independently.

Add static properties where available:

- driver version
- driver date
- DirectX version
- physical location
- hardware-reserved memory

Do not collapse an entire GPU into one utilization graph if strict parity is the goal.

---

# 22. Sorting is missing on several pages

Processes and Details implement sorting.

Startup, Users, Services, and App History pass `sort=None` to the shared table.

Native Task Manager tables support header sorting broadly.

### Fix

Give each tab typed sort state:

- stable column ID
- ascending/descending

Do not use raw positional integers as persistent identity.

Reuse the architecture being recommended for Details.

---

# 23. Services refresh architecture is unnecessarily expensive

The Windows service backend performs status enumeration and then per-service metadata enrichment.

The tab refreshes on a five-second TTL.

Static service information does not need to be re-read every five seconds.

### Split service data

**Fast/live**

- state
- PID

**Slow/static**

- display name
- description
- group
- startup/configuration metadata

Cache static data for minutes or until explicitly refreshed.

This reduces SCM handle churn and native API work.

## UI wake bug

Unlike Startup and Users workers, the Services fetch thread does not explicitly request a repaint when fetching completes.

Normal engine samples may incidentally repaint the UI, hiding the problem.

In paused mode, however, the Services page can remain on `Gathering data` until another UI event occurs.

Add a repaint/wake callback on completion.

The service-control worker has a similar wake concern.

---

# 24. Single action executor can cause head-of-line blocking

`ActionExecutor` contains one worker thread processing a FIFO queue.

That is reasonable for short actions, but slow operations such as dump creation can prevent unrelated actions from executing.

### Recommendation

Separate actions into classes:

- short process controls
- long I/O/debug tasks

or use a small bounded worker pool.

Never spawn unlimited threads, but do not let a 30-second dump delay a priority change or End Task request.

---

# 25. Settings does not reproduce Windows Task Manager

The native Task Manager has Settings as a real navigation page.

The clone opens a centered `egui::Window` modal.

This is a significant look-and-feel mismatch.

Microsoft also documents:

- default start page
- always-on-top / appearance
- real-time update settings
- advanced live-kernel dump options

## Existing hidden setting

`Settings` already contains `default_start_page`, and localization contains a default-start-page label, but the UI does not expose it.

### Required parity section

Implement a full Settings page containing native-equivalent controls first.

Move clone-specific features into a clearly separate section:

- language
- graph duration
- UI zoom
- config autosave

These features can stay; they simply should not be confused with native Task Manager settings.

---

# 26. Window persistence claim is inaccurate

`remember_window` is documented as remembering size/position.

Only `window_size` is persisted.

Window position is not.

### Fix

Either:

- implement monitor-aware window-position restore, or
- rename documentation/UI to "Remember window size".

If position is implemented, clamp restored coordinates against current monitor topology so unplugging a monitor cannot reopen Task Manager off-screen.

---

# 27. Startup/Services first columns reveal an additional width bug

Some page definitions use:

`TmColumn::text(..., 0.0)`

for their first column.

Because `TmTable` always treats column zero specially, this interacts poorly with the elastic architecture.

The replacement table layout should make elasticity a type/flag rather than derive behavior from:

- index == 0
- width == 0

---

# 28. Accessibility needs an explicit parity pass

Microsoft specifically lists recent Task Manager improvements to:

- keyboard focus
- Tab navigation
- text scaling
- screen-reader names
- high-contrast heat maps

The clone needs a dedicated accessibility acceptance suite rather than relying on egui defaults.

Test:

- 100–225% Windows text scaling
- keyboard-only navigation
- search → results focus
- screen-reader labels
- high contrast
- light/dark themes
- disabled controls
- context menus
- table sorting announcements
- selection state

---

# 29. Documentation currently overstates implementation status

`llm-wiki/current.md` says remaining gaps are tracked in `known-debt.md`.

But `known-debt.md` is still essentially a generic/template document.

Also, several implementation claims conflict with source behavior, including the heat-map semantics.

`cpu_load.rs` comments are stale after Microsoft's CPU change.

### Required

After implementing this report:

1. Populate `known-debt.md` with actual accepted deviations.
2. Remove obsolete items from `implement.md`.
3. Update CPU documentation to distinguish Standard CPU and CPU Utility.
4. Make docs describe verified code behavior, not intended behavior.

---

# 30. Latest 2026 Windows features

These should be kept separate from the supplied screenshot baseline.

A June 2026 Windows preview added optional Task Manager columns for:

- NPU
- NPU Engine
- NPU Dedicated Memory
- NPU Shared Memory
- Isolation/AppContainer

and neural engines on Performance.

### Recommendation

Do not block screenshot parity on these.

But design the column registry and telemetry-demand system so these can be capability-gated later without architectural changes.

---

# Implementation order

## Phase 1 — correctness before new features

Implement in this order:

1. Fix table width architecture.
2. Fix per-column heat computation.
3. Fix Details GPU visibility/demand mismatch.
4. Fix Users search.
5. Fix global PID/publisher search + Alt+F.
6. Fix Performance Refresh now.
7. Fix Startup disabled impact.
8. Fix process/group counts.
9. Replace bare selected PID with process identity.
10. Source Efficiency mode from the snapshot.
11. Fix graph-history capacity changes.

Do screenshot regression captures after this phase.

## Phase 2 — table parity

1. Build generic column registry.
2. Implement Details Select columns.
3. Add CPU Utility.
4. Add missing Details columns.
5. Implement sorting for Startup/App History/Users/Services.
6. Implement column-specific telemetry demand.

## Phase 3 — Windows telemetry parity

1. Per-process network provider.
2. Native App History/SRUM source.
3. Real Startup Impact.
4. Packaged startup tasks.
5. Correct startup publisher/shortcut resolution.
6. Additional Memory composition data.
7. Network address/details provider.
8. Expanded GPU engine history.

## Phase 4 — Performance visual parity

1. Memory composition bar.
2. Native graph fill/stroke opacity.
3. Resource-specific colors.
4. Remove inline CPU graph controls.
5. Responsive logical-processor grid.
6. Native disk information layout.
7. Native network throughput layout.
8. Full GPU multi-engine layout.

## Phase 5 — advanced Windows features

1. Process dumps on Processes.
2. Live kernel dumps for System.
3. Dump Settings.
4. Analyze wait chain.
5. Processor-group-aware affinity for >64 logical processors.
6. Efficiency confirmation preference.
7. Current NPU/Isolation capability plumbing if desired.

## Phase 6 — shell/look-and-feel/accessibility

1. Full Settings navigation page.
2. Native-like dialog geometry.
3. scrollbar/menu polish.
4. keyboard navigation.
5. high contrast.
6. screen-reader semantics.
7. text-scaling tests.
8. monitor-aware window position persistence.

---

# Recommended CPU acceptance criteria

Because this project should retain the old behavior:

- [ ] `Legacy Task Manager / CPU Utility` metric implemented independently.
- [ ] Existing time-based CPU accountant retained.
- [ ] User can choose the primary metric.
- [ ] Processes uses selected metric.
- [ ] Performance uses selected metric.
- [ ] Users uses selected metric.
- [ ] Details can independently expose `CPU`.
- [ ] Details can independently expose `CPU Utility`.
- [ ] Metric names are never silently swapped.
- [ ] Sorting uses the value displayed in the selected column.
- [ ] Graph history does not mix metrics after switching; either clear the graph or keep separate histories.
- [ ] Per-process and global values use the same selected semantic.
- [ ] `cpu_load.rs` comments updated.
- [ ] Tests cover idle, 1-core saturation, all-core saturation, frequency downclock and boost scenarios.

For **reference-screenshot parity**, select Legacy Task Manager/CPU Utility.

For **strict current Windows 11 parity**, select Standard CPU, while retaining CPU Utility as an optional Details column.

---

# Highest-value conclusion

The project is considerably beyond a superficial Task Manager imitation, but there are still several architectural mismatches that make it look or behave wrong even though the individual widgets appear finished.

The three changes with the largest immediate impact are:

1. **Stop stretching the first table column across the viewport.**
2. **Fix heat-map normalization so it operates per column, not per row.**
3. **Replace intended-state UI with OS-derived state, particularly Details GPU demand and Efficiency mode.**

After those, the biggest true feature-parity projects are native App History, per-process network telemetry, Select columns, startup telemetry, and the Performance Memory/GPU pages.

The legacy CPU Utility calculation should be retained as a first-class mode rather than treated as obsolete.

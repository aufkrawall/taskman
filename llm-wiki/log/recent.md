# Recent Activity

## 2026-08-27 — Details column prefs persist; last resize handle grabbable

Two user reports, both on tables:

1. **Column visibility/order was session-only.** The Select-columns dialog
   state (`details::State.visible`/`order`) never reached `config.ini`.
   Fixed with two new settings fields: `col_visible` (`table -> id -> on`,
   ONLY entries differing from the built-in default, so future builds'
   new columns keep their designed default) and `col_order` (`table ->
   [ids]`, stored only while it differs from the built-in order). INI
   schema: `[columns.<table>.visible] <id>=0|1` and
   `[columns.<table>.order] order=<id>,<id>,...` — parsed under the
   existing `columns.*` prefix logic via `rsplit_once('.')`. Applied at
   startup in `TaskManApp::new` (`details::State::apply_saved_prefs`, with
   guards: never empty the table, sort column always visible, unknown ids
   skipped, missing ids keep built-in position); written back by
   `details::persist_column_prefs` on every dialog mutation through the
   usual debounced `save_settings()` path. Hidden GPU columns also lower
   telemetry demand correctly from startup.

2. **Last column's resize handle ungrabbable when the table is wider than
   the window.** egui hit-testing clips widget rects to the scroll area's
   clip rect; the header scroll area had NO content margin while the body
   reserves `BODY_PAD_RIGHT` (10 px), so fully scrolled right the last
   boundary sat flush at the viewport edge — only the inner ~6 px of the
   ±6 px handle were clickable, effectively unreachable. Fix: the header
   gets the same right `content_margin` as the body (both in
   `scrolled_table` and `scrolled_rows`), which also aligns header/body
   far-right geometry. Regression test
   `last_boundary_is_grabbable_when_scrolled_fully_right` drives the real
   `scrolled_rows` path (priming the stored body offset BETWEEN passes —
   egui pass memory starts lazily, and `insert_temp` is type-generic:
   an untyped `10_000.0` literal silently stored as f64 and was never read
   back by the f32 reader). Verified the test fails without the fix.

Tests: settings roundtrip + new-section parsing; details prefs
roundtrip/reorder/guards; tablekit last-handle drag. `build.py --check`
(fmt + clippy `-D warnings` + workspace tests) passed; release build
packaged.

## 2026-08-27 — Performance chart freeze (VecDeque ring wrap)

User report: sometimes the Performance graphs / card sparkline previews
stop updating while the rest of the app stays live. Root cause was NOT the
engine/wakeup path (engine → `request_repaint` → eframe event loop was
verified sound) but `TaskManApp.history`: a `VecDeque` with capacity
`history_cap + 8` that `poll_engine` keeps at `len == history_cap` via
pop-front/push-back. Once the ring wraps (after ~cap ticks — 2 min at
Normal speed), `as_slices()` returns TWO runs and the newest points live in
the SECOND one — which `performance::window()` discarded (`let (full, _)`).
Every frame then rendered a stale front-run: frozen for 119 of every 120
ticks (verified with a standalone ring probe). The one-tick catch-up blip
per cycle explains the "sometimes" flavor.

Fix: history is a plain contiguous `Vec<HistoryPoint>` (always
append-ordered; `push_history_point` extracted for the regression test
`history_retention_keeps_newest_point_visible`). Sibling hardening in the
same symptom class: `visible_slice` now scans backward from the newest
sample instead of `partition_point` (robust against a backward wall-clock
step leaving future-stamped older points), and `chart_multi` computes its
x-span with `saturating_sub` (the old `last - first` wrapped/panicked on
such data). If history ever becomes a deque again, windowing must handle
both slices or call `make_contiguous` — see the field doc in `app.rs`.

Tests: retention-through-wrap (app.rs), backward-clock-step window
(performance.rs); `cargo test -p tm-app` 55 passed, clippy clean, release
build packaged.

## 2026-08-27 — TM-parity resource sorting (flat list, no group sections)

Follow-up to the attribution fix, from a side-by-side screenshot: native
Win11 Task Manager keeps the Apps/Background/Windows sections ONLY when
sorted by Name; any resource sort (CPU/memory/disk/network) flattens the
whole list into ONE globally sorted sequence (family groups like
"Brave (29)" stay collapsible). We kept the sections, burying top
consumers.

Fix in `build_display_rows` (tm-app/tabs/processes.rs): group headers and
collapse are applied only for `sort_col < 2` (Name/Status); for resource
sorts `sort_blocks_globally` reorders the per-section emission: the
emitters produce self-contained BLOCKS (depth-0 head row + its expanded,
nested children), blocks are sorted by the head's representative value
(subtree aggregate for family/tree heads, own values otherwise) and
concatenated. Expanded families therefore stay attached while heads
compete globally. Group-collapse state is ignored in the flat view
(native TM offers no group toggles there either). Do NOT "fix" this back
to per-section grouping for resource sorts.

Also: the "Terminated processes" pseudo-row shows its count only when > 0
(a residual without observed exits — born-and-dead-within-one-window
churn is never sampled alive — must not read "(0)"); new i18n key
`TerminatedProcessesPlain`. NOTE for tests: a busy DIFFERENT-image child
of an app (≥ 1 % cpu) is intentionally promoted to a Background top-level
row by `promote_busy_external_tasks`, so it competes globally in the flat
view — block-attachment tests need same-image children.

Tests: flat ordering across categories (top consumer first, no headers),
name sort keeps sections, expanded same-image family stays attached,
pseudo-row label/tooltip.

## 2026-08-27 — CPU attribution completeness (terminated processes, interrupts)

User report: while compiling in a terminal, the Processes page showed NO
process owning the CPU load even when sorted by CPU. Root cause was in the
time-based accountant (`win/cpu_load.rs::build_sample`), not in grouping:

- **New processes got a fabricated 0 %** on their first sample
  (`.map_or(0.0, ...)` when absent from the previous sample).
- **Processes that terminated during the sampling window contributed their
  whole in-window CPU time to NO row**: the per-process loop iterates the
  CURRENT `SystemProcessInformation` table only, so a `rustc.exe` born and
  dead inside one ~1 s window (typical for small crates) was never seen at
  all. The global number (per-core accumulators) sees everything — hence
  "high load, no responsible process".

Fix (accounting completeness, all in `cpu_load.rs` + `win/sampler.rs`):
1. Processes born inside the window are credited their accumulated time
   since creation (which for them is exactly in-window time); reused pids
   get the same treatment (create_time guard).
2. `LoadSample` now carries `unattributed_pct` (global busy − Σ live-process
   in-window time) plus `exited_count`/`exited_images` (image names parsed
   from the NT table's `ImageName`, remembered from the previous sample).
3. `sampler.rs` splits the residual: measured `% Interrupt Time` (new PDH
   group `interrupt`, gated on CORE_PROCESS, counter path
   `\Processor Information(_Total)\% Interrupt Time`) → "System Interrupts"
   row (System/Windows group, TM parity); the rest → "Terminated processes
   (N)" row (Background) with exited image names as a localized hover
   tooltip. Both are SYNTHETIC `ProcessEntry` rows (`synthetic: bool` on the
   model, sentinel pids `u32::MAX`/`u32::MAX-1`), appended AFTER
   `refine_categories_and_group_apps` so the classifier never touches them;
   they sort/heat-map/search like any row. Rows show only above 0.5 % with
   a 5-tick hold-decay (`HeldPseudoRow`/`PseudoRowHold`) so bursty churn
   does not flicker; a measured-low interrupt value hides immediately (only
   UNKNOWN measurement decays — never read missing as zero).
4. Actions are withheld: no context menu on synthetic rows; the header
   aggregate comes from `snap.cpu.utilization_pct`, so no double counting;
   users tab skips them (no session); details shows them like native TM
   shows "System interrupts" (Del/kill guarded by `identity_is_live`).

KEY EMPIRICAL FINDINGS (pinned by a live-kernel unit test
`live_kernel_table_yields_sane_image_names`):
- `SYSTEM_PROCESS_INFORMATION.ImageName.Buffer` is an **absolute pointer
  into the output buffer** on this Windows build (NT writes the caller's
  buffer in place — matches ReactOS `SpiCurrent->ImageName.Buffer =
  (void*)(Current + CurrentSize)`); the Process-Hacker-style record-relative
  interpretation decoded 0/285 names here. `parse_image_name` therefore
  tries absolute / table-relative / record-relative candidates, all bounds-
  and control-character-validated, empty name on any doubt. The i18n
  `keys!` macro CANNOT take multi-line array entries (`expr` fragment
  matcher breaks on the newline before `,`) — keep entries single-line.

Tests: accountant unit tests (new-process credit, residual → exited
names, buffer conventions, live table), sampler hold/decay + append tests,
Processes-tab presentation tests (pseudo rows in the right groups, sorted
by CPU, tooltip only for synthetic rows).

## 2026-08-27 — command lines, real CPU speed, high-CPU background visibility

Three user-reported bugs fixed:

1. **Details command line always "—" on Windows**: `ProcessEntry.command_line`
   was never populated (only the Linux backend did). Fix:
   `process_ops::command_line_of(pid)` via
   `NtQueryInformationProcess(ProcessCommandLineInformation)`. IMPORTANT
   finding: on this Windows build the correct PROCESSINFOCLASS value is **60**
   (matching windows-rs 0.62's `Wdk_System_Threading` binding) — the older
   "class 92" reference does not work here (STATUS_INFO_LENGTH_MISMATCH with
   any buffer). Works with only `PROCESS_QUERY_LIMITED_INFORMATION` (no
   VM_READ); elevated/protected processes fail to open → None → "—". Wired
   through the 10 s TTL `PidAttrs` cache in `sampler.rs` (new field
   `command_line`); integration + unit tests spawn a child and assert the
   args are retrieved.
2. **Performance CPU speed stuck at base clock**: sysinfo's frequency comes
   from `CallNtPowerInformation(ProcessorInformation)` `CurrentMhz`, which
   reports the *nominal* clock constantly on modern Windows (verified: static
   3401 MHz on a 5700X even under load; WMI CurrentClockSpeed identical).
   Fix, Task-Manager-style: new demand-gated PDH group `cpu` with single
   counter `\Processor Information(_Total)\% Processor Performance`
   (`perfcounters.rs`; new `TelemetryDemand::CPU_SPEED` bit 8, set for
   Tab::Performance). `sampler.rs` computes `freq_mhz = base × pct/100`.
   Fallback ladder: counter warming → 0 (UI renders "—", never fakes data);
   counter permanently unavailable (`cpu_counter_failed`) → sysinfo value.
   Counter needs 2 PDH collections before formatting succeeds (matches
   existing `QueryGroup` warm-up). Verified live: idle 4.2 GHz, under load
   4.4 GHz (base 3.4).
3. **High-CPU background/CLI tasks invisible on Processes page**: TWO root
   causes, both fixed:
   - **Background/Windows groups render as TREES** — a busy build tool under
     a console shell (cmd → cargo → rustc, all Background) was hidden as a
     child row under the unexpanded shell row; only the aggregate leaked
     into the parent. Fix: Task Manager parity — **Background/Windows groups
     are FLAT lists** (every process its own depth-0 row with its OWN values,
     sorted by the current column, no expand handles). Only the Apps group
     keeps the family tree. Verified with a live-system repro test: busy
     powershell under cmd becomes a visible flat row.
   - App-absorbed external tasks: promotion pass in `derive_display_groups`
     (`promote_busy_external_tasks`, `is_external_family_member`,
     `PROMOTE_CPU_PCT = 1.0`): an absorbed non-root process with cpu share
     ≥ 1 % whose image differs from every family ancestor is reclassified to
     Background (with its absorbed descendants, wholesale) and appears as an
     ordinary flat Background row; same-image helpers (Chrome renderers
     etc.) stay folded like TM app children. Two-phase decisions (against
     pre-promotion categories) keep the result iteration-order independent.
     Guards learned from the HitmanPro report: **windowed processes are
     never demoted** (they are foreground apps), and the wholesale descent
     skips windowed children (they surface as their own app roots).
   - **Windowed absorption refined (`plausibly_same_application`)**: a
     windowed process folds into a windowless ancestor's family only when
     they share the image or the publisher (company from version metadata;
     unknown publisher falls back to permissive). Start-menu/COM launches
     are brokered by windowless shell-session processes (sihost,
     RuntimeBroker, dllhost — NOT explorer), which would otherwise adopt
     the launched app (HitmanPro case); those brokers plus browsers are
     launch boundaries now. Boundary check precedes the company check, so
     same-image secondary browser windows/PWAs start their own rows
     (TM shows PWAs separately); non-boundary same-company families
     (steam.exe/steamwebhelper) still absorb.
   - **Background/Windows family collapse (TM parity per user's TM
     screenshot)**: connected same-image families render as one expandable
     `Name (N)` row with the family aggregate ("Dropbox (7)"), expanding to
     member rows; unrelated same-name processes and mixed-image trees stay
     flat (`emit_flat_with_family_groups`, `same_image_family`).
   Existing test fixtures set explicit low `cpu_pct` where promotion would
   otherwise trigger (proc() helper defaults cpu = 1.0×pid). NOTE:
   `cpu_pct` is share of TOTAL machine capacity — a full core on a 16-thread
   machine shows as 6.25, not 100; don't key logic off raw "100%".

## 2026-08-27 — window placement UX, type-ahead scroll fixes, dialog chevrons

Three user-reported issues fixed:

1. **Window size/position not persisted**: root cause on the affected
   machine was `remember_window=false` in `config.ini` — a setting with NO
   settings-dialog UI, so it could not be re-enabled. The Settings dialog
   now has a "Remember window size and position" checkbox (i18n key
   `RememberWindow`, persists via autosave). Additionally, maximized state
   is now part of placement: while maximized, neither the restore size nor
   the position is clobbered with monitor geometry; `window-state.ini`
   gains a `maximized=` key and startup re-maximizes via
   `ViewportBuilder::with_maximized`. See `ui_state.rs` (Placement struct)
   and `NativeApp::ui` in main.rs.
2. **Select-columns dialog arrows**: the →/← text buttons sat under the
   floating vertical scrollbar (right edge) and could not be clicked. They
   are now painted chevron icons (`controls::icon_button`, new
   `icons::Icon::ChevronUp`), moved 16 px left of the scrollbar strip, and
   reordered to ↑/↓ (up = earlier position, down = later).
3. **Type-ahead scroll**: plain-letter navigation (Processes/Details)
   used `Response::scroll_to_me(Some(Center))` on a virtualized row, which
   (a) never fired for rows outside the rendered window (no vertical
   scroll at all) and (b) when it fired, targeted BOTH scroll axes,
   yanking the table horizontally. `tablekit::scrolled_rows` now takes a
   one-shot `focus_row: Option<usize>` and computes a vertical-only,
   minimal-move offset from the last frame's y-offset (`tm-rowsy` temp)
   applied via `ScrollArea::vertical_scroll_offset` on the request frame
   only. Callers pass the index from `scroll_to_pid.take()`; the per-row
   `scroll_to_me` mechanism is gone. Regression test:
   `focus_row_scrolls_vertically_only_even_for_unrendered_rows`.
   `search::cycle_process_initial` was genericized to `cycle_match<T:
   PartialEq + Clone>` and the Performance card column gained the same
   type-ahead (jump + vertical scroll via `scroll_to_me(None)`, which for
   full-width items can never move horizontally).

Also fixed (found by the heavy gate): `cpu_info::base_mhz_from_smbios_table`
returned `max(current, max)` while its tests document current-speed-
preferred-with-max-fallback — pre-existing failing test
`smbios_type4_current_speed_is_preferred`, now green.

Note: HEAD was not fmt-clean under the local rustfmt
(1.10.0-nightly 2026-08-25); `python build.py --check` failed on the
pristine tree. The formatting drift in previously untouched files
(fonts.rs, chart.rs, linux/*, win/mod.rs, taskmgr_replacement.rs) is
mechanical rustfmt output required to keep this machine's gate green.

Validation: `python build.py --check` (fmt, clippy -D warnings, all
workspace tests) + release build/packaging + `--selfcheck --mock` pass.

## 2026-08-27 — Processes app-grouping parity

The Processes page now builds a presentation topology instead of treating
raw PPID as UI ownership. Explorer and common shell launchers are boundaries,
so programs the user starts from Explorer/cmd/PowerShell/Terminal appear as
independent app groups while helpers remain under their app family. Raw
`ProcessEntry.ppid` is unchanged.

- App membership is rebuilt from visible-window ownership while preserving
  System classification; no-window backends retain collector categories.
- Display parent edges are cut across category boundaries and at app roots,
  so Explorer no longer inherits CPU/memory/subtree counts from launched apps.
- `Apps (N)` now counts top-level app groups (matching native `Apps (9)`);
  Background/Windows keep unflattened process counts.
- Cyclic/malformed PPID components stay visible instead of disappearing.
- Regression coverage covers Explorer launches, shell-launched GUI apps,
  app-group totals, aggregate boundaries, and cycle visibility.
- CPU load/accounting code was not changed.

Validation: code was statically reviewed through the GitHub connector. This
environment has no Rust toolchain/Windows runtime, so the requested local
build remains the final compile/runtime/UI check.

## 2026-08-26 — audit.md Phase 1 (correctness) implemented

All 11 Phase-1 items from the 2026 parity audit landed, each with
regression tests:

1. **Table width architecture (P0.1)**: `TmColumn::elastic` and the
   index-0/"width==0 sentinel" fill behavior REMOVED. Every column uses its
   configured/persisted width; unused viewport space stays blank on the
   right like native TM. `TmTable::new`/`make_table` lost the `name_min`
   param; `col_width(i)`/`total_width()` no longer take `avail`; layout is
   width-driven (rebuilt only on mutation). Double-click restores
   `default_w`.
2. **Per-column heat normalization (P0.2)**: `heat_cells` now paints given
   `HeatCell { intensity, text }` values; callers normalize per COLUMN over
   the whole display model BEFORE virtualization (`tablekit::norm`,
   processes' `normalize_heat`, users' `HeatMax`, apphistory maxima).
   "value>0 ⇒ 1.0" binary intensities are gone.
3. **Details GPU demand (P0.3)**: `show_gpu_columns` bool removed;
   `State.visible: BTreeSet<ColumnId>` is the single source of truth, both
   for rendering (dynamic column list) and `requires_gpu_telemetry()`
   demand derivation. Minimal Select-columns dialog added ("…" overflow),
   session-only persistence (see known-debt.md).
4. **Users search fixed**: old condition kept every active user visible;
   now query matches user display name OR any aggregated app name.
5. **Global search + shortcuts**: new `tm-app/src/search.rs` `Query`
   matcher (binary name/display/PID/publisher) used by Processes, Details,
   Startup (+publisher) and App History. Alt+F and Ctrl+F focus the global
   search field (`egui::Id::new("global-search")`).
6. **Performance Refresh now**: actually calls `refresh_all()`.
7. **Startup impact**: disabled items report `None` (win/startup.rs);
   enabled-without-data stays Unknown; real thresholds = later SRUM work.
8. **Group counters (P0.4/P0.5)**: `DisplayRow::GroupHeader(gi, total)`
   carries unflattened classification counts; grouped labels use O(n)
   whole-subtree process counts (`subtree_values_and_counts`). RowData lost
   its now-unread `group` field.
9. **Selection identity (§7)**: `selected_pid` replaced by
   `selected_process: Option<ProcessIdentity>`; `end_selected` validates
   start-time identity against the live snapshot before dispatch
   (`TaskManApp::identity_is_live`); Efficiency toggles validate too.
10. **Efficiency mode from OS state (§8)**: leaf icon/menu derive from
    `ProcessEntry.power_throttled`; `efficiency_pids` HashSet deleted;
    toggle issues one forced refresh so paused mode updates as well.
11. **History capacity (§10)**: `history_cap_for()` recomputes whenever
    `graph_seconds` changes (logic pass), truncating overflow.

Also fixed in passing (§23): services fetch + service-control workers wake
the UI via `Context::request_repaint` (Services page could stick on
"Gathering data" while paused). Stale cpu_load.rs doc rewritten to the 2026
metric split (current pages = time-based; utility survives as Details
"CPU Utility" column; legacy provider = future work).

Gates: fmt+clippy(-D warnings)+workspace tests+release build all green on
Windows (`build.py --check`, then `--host-only`); selfcheck --mock ok.

## 2026-08-26 — Visual parity pass vs real Win11 TM (taskmanpngs/ reference)

Measured the real Task Manager screenshots (taskmanpngs/1..7.png, captured at
133% scaling) pixel-by-pixel and re-derived the design tokens. Reference
logical sizes (Segoe cap≈0.75·font): rows 13, header labels 12, header
aggregates 17, group headers 20–21, sidebar/search/tab-title 15, titlebar 13,
kv rows 13 (pitch 22.5), captions 11, stat values 23, card titles 17,
page title 31. Colors (dark): content 0x191919, sidebar/chrome 0x202020
(sidebar LIGHTER than content), header separators 0x2D2D2D, heat base
(17,36,62), top-consumer cell (8,51,110), heat cell separators (41,50,63).

Changes:
- theme.rs: new palette (window 0x191919, sidebar 0x202020, stroke 0x2D2D2D,
  heat_top/heat_sep added), Body/Button text style 13.
- tablekit.rs: ROW_H 32, HEADER_H 57; `ui.spacing_mut().item_spacing.y = 0`
  before show_rows so rows TOUCH (the 6px default gap striped the heat
  bands); header agg font 17 at top+19, label font 12 at bottom−13;
  heat_cells now draws the flat TM style: base fill + brighter `heat_top`
  cell for each column's top consumer (max intensity) + 1px separators
  between cells. `heat_blue` gradient kept (dead_code) for future use.
- processes.rs: group headers 20px, NO background band (ref has none);
  heat intensities are binary (value>0 ⇒ 1.0) — top consumer per column wins.
- users.rs/apphistory.rs: same binary intensity model.
- app_ui.rs: nav items 15px text, h=38, accent bar 3×18; search box 495px
  wide with 15px font; cmd buttons 13px; toasts 13px.
- performance.rs: page title 31, right detail 17, card titles 17, card value
  lines 13, captions 11.5, big/med stat values 23 with 13px labels
  (56/48px blocks), kv rows 13px pitch 23.
- chart.rs: clippy `chunks_exact(3)` → `as_chunks::<3>()`.

Verified against ref by downscaling our 200%-DPI captures by 2/3 and
comparing crops side by side (shots/ui3-*.png).

## 2026-08-26 — Column/splitter resize ACTUALLY fixed (egui delta semantics)

**Root cause of "columns can't be resized" (persisted across the 2026-08-25
fix):** implement.md §8.1 claimed `Response::drag_delta()` is cumulative from
drag start — FALSE in egui 0.36. It is `pointer.delta()` = movement since the
LAST FRAME; only `total_drag_delta()` is cumulative. The shipped math froze a
drag-start width (`start_w + drag_delta().x`), so every frame reset the width
to ~its starting value: boundaries jiggled sub-pixel and snapped back when
the pointer paused. The 2026-08-25 fix (materializing the elastic Name
column) addressed a real but DIFFERENT failure layered on top; neither made
resizing work.

Fix (tablekit.rs + performance.rs splitter): accumulate each frame's delta
onto the LIVE width, `width = (width + dx).clamp(min,max)`; drop the
drag-start temp-data machinery entirely.

Also fixed in the same pass:
- Resize handles were registered DURING the cell loop, so each next header
  cell covered the right half (±6 px) of its neighbor's handle and won hit
  testing there — grabs landed on the cell and quick clicks even toggled
  sorting. Handles are now ALL created after the cell loop (topmost), full
  ±6 px grabbable; the last column keeps its right-edge handle.
- Double-click-to-default never worked: a `Sense::drag()` widget never
  receives egui click flags, so `double_clicked()` was always false. Now
  detected via `pointer.button_double_clicked(Primary)` while hovered.
- Regression tests drive REAL pointer events through an egui `Context`
  (`ctx.run_ui` + RawInput events; clear `out.textures_delta` or egui panics
  headlessly). Verified the test FAILS against the old math (730 ≠ 760).
  Test-authoring gotchas: egui counts multi-clicks ACROSS sequences within
  0.3 s (space sequences >0.3 s apart); boundary x positions must be derived
  from CURRENT column widths after any mutation.

implement.md §8.1/§14.1 corrected in place.

## 2026-08-25 — UI polish pass + Linux backend repaired & verified under WSLg

**Column resize root cause (tablekit.rs):** the elastic name column absorbed
viewport slack EVERY frame, so a manual drag of any other boundary was
cancelled in the same frame (dragged separator stayed put, Name/Status
divider shifted instead, regime flips around `spare == stored` read as
wobbling). Fix: `name_effective` is elastic ONLY while `width == 0.0`
(virgin sentinel); the first `drag_started` on any table materializes the
name width (value-preserving), after which all columns are explicitly
sized and boundaries track the cursor 1:1. Double-click on the name
separator restores fill mode via the 0.0 sentinel. Also: last column got a
right-edge resize handle; removed the extra stroke box around the Name
header cell; `table_avail` margin 6→16 px; body ScrollAreas got
`.content_margin(right:10, bottom:8)` so FLOATING scroll bars never paint
over the last column/bottom row (floating bars never reserve layout space;
reserving would desync header/body widths).

**Graph rendering root cause (chart.rs):** all area fills used
`Shape::convex_polygon`, but epaint fan-triangulates fills from vertex 0
(`fill_closed_path`) — only valid for CONVEX polygons. Concave series got
straight fan edges cutting across dips: fake linear ramps, cliffs, fills
floating above the line, white gaps (see graphpngs/1-3.png). Fix:
`fill_area_to_baseline()` builds an explicit x-monotone triangle strip
(2 verts + 6 indices per segment) that hugs the polyline exactly; used by
sparkline, core_chart (incl. kernel band) and chart_multi. Also: series
extractors (`disk/net/gpu_series`) now return one value PER window point
(0.0 when the device is absent; no more zero-filtering in net_series) so
series indices stay aligned with `timestamps_ms` — shortened series used
to plot at wrong x positions; chart_multi hover maps pointer time back to
the nearest sample via partition_point.

**Other UI:** checkbox hover theme-aware (accent border over faint tint,
no white flood in dark mode); chart strokes/fills strengthened, kernel
times = darker accent shade (`kernel_color`), per-theme secondary colors
(`pal.ok_green` not hardcoded dark-theme green).

**Linux backend (tm-platform/linux):** was NOT compiling — model additions
from Windows work (`CpuInfo::kernel_pct/per_core_kernel_pct`,
`GpuInfo::luid/shared_used_bytes`) never landed there, plus missing
`use std::sync::Mutex` and 5 clippy lints. Filled with documented "unknown"
values (empty vec / 0 / None) matching model docs & mock. Verified:
- `--selfcheck` real backend ok:true (CPU/mem/disks/networks/processes;
  GPU honestly empty — WSL2 exposes no /dev/dri for DRM).
- GUI runs under WSLg Wayland: wgpu+Vulkan at locked ~60 fps (Fifo), surface
  1100×720; glow renderer starts too. Note: wgpu hides "Microsoft Direct3D12
  (NVIDIA ...)" as not-Vulkan-compliant → renders via llvmpipe software path,
  still vsync'd 60 fps.
- `goto_services_for_pid`: cfg_attr allow(dead_code) on non-Windows (callers
  are cfg(windows)); keep compiling everywhere.

**WSL/Windows dual-host gotcha:** WSL builds produce `target/release/taskman`
(ELF) while the launched binary is `taskman.exe` — a WSL-side
`build.py --host-only` does NOT refresh the exe. Refresh it from WSL via
interop: `cmd.exe /c "... && set PATH=C:\Users\REDACTED\.cargo\bin;%PATH% &&
py.exe build.py --host-only"` (works; selfcheck via powershell
Start-Process -RedirectStandardOutput since GUI-subsystem console attach
fails through the interop pipe). Consider separate CARGO_TARGET_DIR per
host to avoid cache thrash.

## <YYYY-MM-DD> — Template scaffold created

Initial generic `AGENTS.md` / `llm-wiki` template. No project-specific
history recorded yet.

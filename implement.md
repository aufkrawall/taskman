# Taskman audit and implementation plan

**Repository audited:** `tmproject-main`  
**Audit date:** 2026-08-25  
**Primary target:** Windows 11 Task Manager–style desktop application  
**Primary goals:** correctness, near-instant startup, low sampling/UI overhead, smooth high-refresh-rate interaction, feature parity with current Windows Task Manager where practical.

> This document is intended to be executable by another coding agent in a fresh session. It names the current files/functions, describes the observed problems, specifies the desired architecture, and gives tests/acceptance criteria. Do not treat it as a request to preserve current internal APIs when those APIs are the source of startup or correctness problems.

---

## 1. Audit status and constraints

The repository is a Rust workspace containing:

- `crates/tm-app` — eframe/egui GUI
- `crates/tm-core` — models, engine, settings, history, formatting/i18n
- `crates/tm-platform` — Windows/Linux/macOS platform collectors/actions
- `BENCHMARKS.md`, `bench/`, `tools/` — benchmarks/UI automation

The local audit environment did **not** contain `rustc`/`cargo`, so this pass is a source-level/static audit. No claim in this document that depends on Windows APIs or rendering behavior should be considered runtime-validated until the Windows verification plan in §18 is completed. The implementation session must run the full build/test/lint suite and then validate on real Windows hardware.

The codebase is about 16k lines of Rust. The audit focused especially on startup, sampler construction/first sample, GPU/PDH, table/layout code, settings/history persistence, process actions, and feature parity.

### Required first commands in the implementation session

Run on a Windows development machine with Rust 1.85+:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --release
cargo build --release --no-default-features --features glow
```

Also run, if installed:

```powershell
cargo audit
cargo deny check
```

Do not start optimizing from profiler guesses until the baseline instrumentation in §3 is in place.

---

## 2. Executive summary: highest-priority findings

### P0 — confirmed correctness / UX bugs

1. **The Windows platform stack is constructed twice during GUI startup.**
   - `crates/tm-app/src/main.rs::spawn_engine()` calls `tm_platform::create_stack()` and throws away `actions`.
   - `crates/tm-app/src/app.rs::TaskManApp::new()` calls `tm_platform::create_stack()` again and throws away the collector.
   - On Windows `Sampler::new()` is not cheap: it creates/refreshes `sysinfo::System`, probes CPU topology/static info, parses SMBIOS memory data, and allocates CPU accounting state. This duplicate work happens directly in the startup path.

2. **The sampling engine starts before `eframe::run_native`, so the first heavy sample competes with GUI/GPU initialization.**
   - Initial process enumeration, windows/thread enumeration, PDH wildcard counter initialization, DXGI adapter discovery, version metadata, disk/network work can occur while eframe/wgpu initializes its adapter/device/surface.
   - This is the opposite of an “instant shell first, telemetry second” architecture.

3. **Table column resizing is mathematically wrong.**
   - CORRECTION (verified against egui 0.36 source): `Response::drag_delta()`
     is movement since the LAST FRAME (`pointer.delta()`); only
     `total_drag_delta()` is cumulative since the press.
   - The original fix therefore froze a drag-start width and added one frame
     of delta to it every frame, which reset the width to ~its starting
     value each frame — the boundary never followed the cursor.
   - Correct pattern: accumulate each per-frame `drag_delta()` onto the LIVE
     width (`width = (width + delta).clamp(min, max)`).
   - The Performance sidebar splitter had the same pattern.

4. **Table drag-end persistence state is ineffective.**
   - `TmTable` is reconstructed every frame, but `prev_dragging` lives inside `TmTable`, so the previous-frame state is lost. `drag_just_ended()` cannot reliably work as intended.

5. **F5/refresh while sampling is paused does not actually sample.**
   - `EngineCmd::Refresh` just loops; the next loop samples only when engine state is `Running`.
   - Several tab-local “Refresh now” controls are also no-ops instead of invalidating/refetching the relevant data.

6. **Startup-folder enabled/disabled state is read from the wrong registry path.**
   - In `crates/tm-platform/src/win/startup.rs`, the code constructs the `...\Explorer\StartupApproved\StartupFolder` location but calls `read_registry_binary(hive, "StartupFolder")` instead of using the full `APPROVED_KEY\StartupFolder` subkey.
   - Disabled Startup-folder items can therefore be reported as enabled.

7. **Details sorting is wrong for at least two columns.**
   - Current index-based comparator has UAC virtualization sorting by `priority`.
   - GPU column falls through to name sorting.
   - Index-based sort logic will keep creating this class of bug when columns are added/reordered.

8. **Details reports fabricated or missing security/GPU data.**
   - Windows sampler does not populate `ProcessEntry.elevated`; UI largely shows false/derived assumptions.
   - UAC virtualization is not queried; UI fabricates Disabled/NotAllowed values.
   - “GPU engine” currently displays GPU utilization percentage, not an engine identifier.

9. **Multi-GPU accounting is incorrect.**
   - `win/gpu.rs::merge()` takes one/global engine utilization value and assigns it to every DXGI adapter.
   - Current PDH process/engine aggregation loses adapter LUID and engine identity and sums/clamps values in ways that are not equivalent to Task Manager.

10. **Unavailable per-process network data is often rendered as literal zero.**
    - Windows capabilities declare per-process network unsupported, yet UI/history code commonly converts `None` to `0`.
    - “0 B/s” means measured zero; missing telemetry must be displayed as unavailable (`—`) or the column hidden/disabled.

11. **App History persistence can race and App History can misattribute usage across PID reuse.**
    - Every 30 s `save_async()` clones the full map and starts a new writer thread; writers use the same `.json.tmp` path and can race/out-of-order overwrite.
    - PID reuse is detected only indirectly by negative deltas. A new process whose counters happen to be greater than the old process can be incorrectly charged to the new/old app.
    - App identity is only lowercase executable filename, so distinct executables with the same filename collide.

12. **`save_config=false` cannot reliably persist that preference.**
    - Turning autosave off sets the flag and calls `save()`, but `save()` itself is gated by `save_config`; shutdown save is also gated. The choice to disable autosave therefore does not naturally survive restart.

13. **Minidump creation uses `OPEN_ALWAYS` rather than truncating an existing file.**
    - Reusing an existing `.dmp` may leave stale trailing bytes if the new dump is shorter.
    - Use `CREATE_ALWAYS` / explicit truncation.

14. **Processes tree expansion/cache logic has multiple correctness bugs.**
    - Expansion state is not part of the cached-row key, so expand/collapse can lag until the next sample.
    - Tree emission only renders one child level; deeper descendants never appear.
    - The subtree aggregator does not use its advertised “done” state and can devolve toward O(n²).
    - `ProcCategory::System` is folded into “background” rather than a separate “Windows processes” group.

15. **“Go to Details” is not a true PID navigation.**
    - It sets a text filter to the process name rather than selecting/scolling the exact process identity.

### P0 — startup/performance architecture

1. Remove duplicate platform construction.
2. Display the first shell before constructing/sampling the heavy Windows collector.
3. Phase telemetry: core process/CPU/memory first; PDH/DXGI/static metadata later and on demand.
4. Do not synchronously load system fonts, app-history JSON, or disk-backed logging before first usable frame unless measurement proves they are negligible.
5. Make UI wakeups event-driven: engine publication and worker completion should call `egui::Context::request_repaint()` rather than polling twice per sampling interval.
6. Virtualize all large tables. The current custom table renders every row.
7. Stop per-frame/per-comparator string allocations in hot paths.
8. Bound icon queues/cache and rate-limit texture uploads per frame.

### P1 — important Windows Task Manager parity gaps

- Header **Select columns…** with persisted visibility/order/width and capability-aware optional columns.
- CPU graph: **Overall utilization** vs **Logical processors** and **Show kernel times**.
- Details: **Analyze wait chain**.
- Settings: **Default start page**.
- Correct three Process groups: Apps / Background processes / Windows processes.
- Correct/expanded Details metrics, including optional **CPU Utility** on Windows 11 25H2.
- Accessibility parity: keyboard focus/navigation, text scaling, screen-reader names, high-contrast heatmaps.
- Startup apps parity: same registered startup-task list as Windows Settings/Task Manager, publisher/impact where data is actually available.

### P2 — advanced/current-preview parity

- Live kernel dump of `System`, including advanced options.
- NPU/NPU Engine/NPU memory and Isolation columns as capability-gated future/preview features; do not make them mandatory for stable Windows parity yet.

---

## 3. Establish objective startup and responsiveness measurements first

The existing `BENCHMARKS.md` primarily uses `MainWindowHandle` as the startup marker. That measures “a HWND exists,” not “the app has painted useful pixels and can accept input.” Keep that metric for historical comparison, but add real phase markers.

### 3.1 Add a startup trace

Create a tiny zero/low-allocation timing utility in `tm-core` or `tm-app`, compiled in release. It should record `Instant` deltas from process entry and emit them after logging becomes available.

Recommended markers:

- `process_entry`
- `args_parsed`
- `minimal_config_loaded`
- `run_native_enter`
- `creation_context_enter`
- `app_constructed`
- `first_ui_begin`
- `first_ui_end`
- `first_frame_presented` if eframe/winit exposes a suitable hook; otherwise maintain a separate pixel/present benchmark externally
- `engine_worker_started`
- `collector_construction_begin/end`
- `core_sample_begin/end/published`
- `optional_telemetry_begin/end`
- `first_process_metadata_enriched`
- `first_gpu_telemetry_published`

Use one compact record per startup rather than logging between every phase if synchronous file logging has not yet been initialized.

### 3.2 External benchmark improvements

Update/add scripts under `bench/`:

- Preserve `MainWindowHandle` benchmark.
- Add a “first painted/usable frame” test. Options, in preferred order:
  1. app emits a one-line timestamp/event after first render/present callback;
  2. Win32/DWM capture checks for a known non-background pixel/region;
  3. UI automation waits for an accessible named control and then sends an input event.
- Record P50/P95/min/max over at least 20 warm launches, not best-of-five.
- Separately record first launch after reboot/driver cold state on representative hardware.
- Record working set at first frame and 10 s steady state.

### 3.3 Startup targets

The framework benchmark in this repository suggests a ~275–320 ms floor on the original dev machine. The goal should be **minimal app overhead above the renderer/framework floor**, not an impossible zero-millisecond process launch.

Suggested acceptance targets on the benchmark machine:

- Warm first visible/interactive shell: **P50 ≤ 350 ms**, **P95 ≤ 500 ms**.
- Cold launch: **P95 ≤ 700 ms** on supported local hardware; report by hardware rather than hiding variance.
- App-owned synchronous work before first UI should add **< 50–100 ms** over a minimal same-renderer eframe test.
- First basic process/CPU/memory snapshot: **≤ 750 ms warm** without delaying shell paint.
- GPU/PDH/enriched metadata may appear progressively after the first core snapshot.

Do not regress reliability simply to win 10–20 ms. A WGPU → Glow fallback is useful; choose the default renderer only after the matrix in §18.

---

## 4. Target startup architecture

### 4.1 Current problematic sequence

Roughly:

```text
main
 ├─ AttachConsole (release Windows GUI too)
 ├─ initialize disk-backed logging
 ├─ locale
 ├─ Settings::load
 ├─ create_stack() -> Sampler::new() + actions
 ├─ start engine -> first heavy sample begins
 └─ eframe::run_native
     ├─ WGPU/graphics initialization
     ├─ load system fonts from disk
     └─ TaskManApp::new
         ├─ create_stack() AGAIN -> second Sampler::new()
         ├─ start icon thread
         └─ AppHistoryDb::open JSON read/parse
```

The sampler and renderer can both touch CPU, GPU/driver, disk, registry, process lists, and Windows APIs at the same time.

### 4.2 Required sequence

Refactor to:

```text
main
 ├─ parse args
 ├─ AttachConsole only if CLI/verbose requires it
 ├─ initialize minimal crash/startup diagnostics (no log-directory I/O)
 ├─ read only minimal startup config needed for window/theme/language/default page
 └─ eframe::run_native
     ├─ renderer initializes
     ├─ use embedded/default fonts for first frame
     ├─ construct cheap PlatformActions only
     ├─ render shell immediately (data placeholders are acceptable)
     └─ after first UI frame has been submitted:
         ├─ start/activate engine worker
         ├─ construct Sampler on engine thread
         ├─ publish Phase A core snapshot
         ├─ request_repaint()
         └─ begin Phase B / demand-driven enrichment
```

### 4.3 Split `create_stack()`

Current API in `crates/tm-platform/src/lib.rs` returns collector + actions together and makes accidental duplicate construction easy.

Implement one of these designs:

```rust
pub fn create_actions() -> Box<dyn PlatformActions>;
pub fn create_collector() -> Box<dyn SystemCollector>;
```

or:

```rust
pub struct PlatformFactory;
impl PlatformFactory {
    pub fn actions(&self) -> Arc<dyn PlatformActions>;
    pub fn collector(&self) -> Box<dyn SystemCollector>;
}
```

Prefer the first unless sharing immutable platform discovery state has a measurable benefit. The critical invariant is:

> GUI startup must never instantiate a collector merely to obtain actions, and it must instantiate only one collector.

Update examples/integration tests accordingly. `selfcheck` may continue constructing collector immediately because it is explicitly headless.

### 4.4 Lazy engine start without UI-thread sampler construction

Do **not** simply move `create_collector()` into `TaskManApp::ui()`; that would move heavy work onto the UI thread.

Preferred design:

- Introduce `EngineCmd::Start` and a lazy collector factory owned by the engine thread, or create/spawn the engine only after first frame via a nonblocking factory.
- If a parked engine thread is created before the window, its closure must not call `create_collector()` until receiving Start.
- At the end of first UI frame, schedule/start the engine. If eframe offers a reliable first-frame/present callback, use it. Otherwise start on the second logic/UI pass after a `first_frame_seen` flag has been set.
- Creating a thread itself is small, but measure whether spawning it before vs after the first frame matters.

API sketch:

```rust
pub type CollectorFactory = Box<dyn FnOnce() -> Box<dyn SystemCollector> + Send>;

pub fn spawn_lazy(factory: CollectorFactory, interval: Duration, notifier: UiNotifier)
    -> io::Result<(EngineHandle, JoinHandle<()>)>;

impl EngineHandle {
    pub fn start(&self);
}
```

The engine should be able to represent `NotStarted`, `Running`, `Paused`, `Stopped` cleanly.

---

## 5. Remove synchronous pre-first-frame I/O/work

### 5.1 Console attachment

**File:** `crates/tm-app/src/main.rs`

Current Windows release code calls `attach_parent_console()` before parsing args. Parse args first, then attach only for `--selfcheck`, `--verbose`, or another explicit console mode.

Acceptance:

- Normal GUI performs no console attach call.
- `--selfcheck` still prints to the parent terminal.

### 5.2 Logging

**File:** `crates/tm-core/src/logging.rs`

Current `logging::init` synchronously:

- resolves data dir,
- creates `%LOCALAPPDATA%\taskman\logs`,
- constructs a daily rolling appender/nonblocking worker,
- initializes the tracing stack.

Refactor GUI startup so disk-backed logging is initialized after first frame, while preserving useful early diagnostics.

Recommended approach:

- Install an early subscriber backed by a small bounded in-memory ring/no-I/O sink, or store startup markers separately.
- After first frame, initialize/attach the file sink and flush the startup records.
- CLI/selfcheck can retain synchronous console/file setup.
- Ensure early panic/fatal renderer failure does not become a silent exit. If logging is not ready, emit to debugger/stderr/MessageBox as appropriate.

Measure before/after; if Windows proves the current file logger is consistently sub-millisecond, this can remain P1, but do not assume it.

### 5.3 Fonts

**File:** `crates/tm-app/src/fonts.rs`

`fonts::install()` synchronously opens OS font files before app construction. Windows candidates include Segoe UI, bold Segoe UI, Cascadia/Consolas. These are megabyte-scale disk reads on some systems.

Options:

1. First frame uses egui default embedded fonts (`default_fonts` is already enabled); install system fonts asynchronously after first paint.
2. Bundle a small redistributable UI font and avoid system-font I/O.
3. Use only system font discovery if an API permits mapping without reading full files and benchmark it.

If installing fonts after first frame, expect a relayout. Do it once, at a controlled point, and request repaint. Verify no visible flicker/jump at 100/125/150/200% DPI.

### 5.4 App History load

**Files:** `crates/tm-app/src/app.rs`, `crates/tm-core/src/app_history.rs`

`TaskManApp::new` calls `AppHistoryDb::open`, reading and JSON-parsing synchronously.

Change to:

- create an empty/loading App History model instantly;
- load DB on a long-lived persistence worker after first frame;
- merge/replace loaded state safely before the first observation tick, or define a deterministic merge if sampling starts first;
- request repaint when loaded.

### 5.5 Icon worker

**File:** `crates/tm-app/src/icon_cache.rs`

Do not spawn the icon extraction thread in `TaskManApp::new`. Start it lazily when the first uncached icon is actually requested.

---

## 6. Phase and demand-gate Windows telemetry

### 6.1 `Sampler::new()` is not “cheap”

**File:** `crates/tm-platform/src/win/sampler.rs`

Current construction performs meaningful work such as:

- `System::new()` + targeted refresh,
- `cpu_info::CpuStatic::probe()` / topology,
- `memory_info::probe()` / SMBIOS firmware table parsing,
- CPU load accounting allocation.

The comments/docs should be corrected. Better: make construction actually cheap by moving static probes to a background enrichment phase.

### 6.2 Define telemetry phases

Add explicit phases inside the Windows collector:

#### Phase A: core first snapshot

Only data required to make Processes + primary header useful:

- process identity (PID, parent PID, start time where available)
- executable/process name
- CPU total + per-process CPU
- memory total/used + per-process memory
- minimal status/session data needed for grouping
- no file-version metadata
- no PDH GPU wildcard counters
- no DXGI enumeration
- no SMBIOS/module/static hardware descriptions unless already free
- avoid native network metadata/SSID work

Publish this snapshot as soon as possible.

#### Phase B: cheap static enrichment

After first core snapshot or after a short idle window:

- CPU brand/topology/cache/static facts
- RAM SMBIOS speed/slots
- disk static labels/media details
- adapter descriptions/link speed (low-frequency)
- process file version/company/description cache

#### Phase C: demand-driven expensive telemetry

Only active when a visible page/selected column needs it:

- per-process GPU + GPU Engine
- GPU memory counters
- physical disk PDH counters
- security token details (elevation/UAC virtualization/AppContainer/isolation)
- command line, handles, I/O counters, etc. where an optional column requires them
- per-process network ETW session
- future NPU metrics

### 6.3 Introduce `TelemetryDemand`

Create a bitflag/atomic demand model in `tm-core` or `tm-platform`, e.g.:

```rust
bitflags! {
    pub struct TelemetryDemand: u64 {
        const CORE_PROCESS        = 1 << 0;
        const DISK_RATE           = 1 << 1;
        const NET_ADAPTER_RATE    = 1 << 2;
        const PROCESS_NET         = 1 << 3;
        const GPU_ADAPTER         = 1 << 4;
        const PROCESS_GPU         = 1 << 5;
        const PROCESS_GPU_MEMORY  = 1 << 6;
        const TOKEN_SECURITY      = 1 << 7;
        const PROCESS_IO          = 1 << 8;
        const PROCESS_HANDLES     = 1 << 9;
        // ...
    }
}
```

The UI derives demand from:

- current tab,
- visible columns,
- expanded detail panels,
- dialogs that need attributes.

The engine/collector receives demand through a cheap atomic or command. Add hysteresis so flipping tabs does not constantly tear down/rebuild PDH/ETW sessions; e.g. keep expensive providers warm for 10–30 s after last use if measured cost favors it.

### 6.4 Split PDH query groups

**File:** `crates/tm-platform/src/win/perfcounters.rs`

Current first initialization registers broad GPU Engine, GPU Process Memory, and PhysicalDisk wildcard counters together. Refactor into independent groups/queries so default Processes does not initialize counters it does not need.

Suggested types:

```rust
struct GpuPdh { ... }
struct DiskPdh { ... }
struct PdhState {
    gpu: Option<GpuPdh>,
    disk: Option<DiskPdh>,
}
```

Warm each group only on demand. Preserve the two-collection warm-up requirement where PDH rate counters need a prior sample.

### 6.5 Low-frequency network metadata

**File:** `crates/tm-platform/src/win/net_info.rs` and caller in sampler

Native adapter metadata/SSID enumeration should not run every 0.5/1/4 s tick.

- Keep byte-rate counters on the sampling cadence.
- Refresh adapter metadata, SSID, link properties on a 5–10 s TTL or Windows interface-change notification.
- Key caches by stable adapter identity, not display name.

### 6.6 Process version metadata cache

**File:** `crates/tm-platform/src/win/version.rs` + sampler

File version/company/description lookup can hit disk/version resources for many processes on initial sample.

Add a cache keyed by stable executable identity:

- normalized path + mtime/file ID if available;
- value: description/company/version plus negative-cache status;
- resolve lazily on a metadata worker;
- bounded LRU;
- negative entries get a sensible TTL.

First snapshot should use executable name and fill friendly descriptions later.

### 6.7 Convert tick-based TTLs to time-based TTLs

Any native attribute cache whose expiry is defined as “N ticks” changes behavior when update speed changes. Use `Instant`/duration expiry for metadata not intrinsically sample-count based.

For process attributes, also invalidate if `(pid, start_time)` changes.

---

## 7. Engine and UI wakeup architecture

### 7.1 Fix paused refresh

**File:** `crates/tm-core/src/engine.rs`

Refactor sampling into a shared helper:

```rust
fn sample_and_publish(..., count_tick: bool) -> Result<Arc<Snapshot>>
```

`EngineCmd::Refresh` must force one sample regardless of paused/running state, without changing the paused state afterward.

Add test:

```text
engine_refresh_while_paused
- start engine
- pause and wait until state Paused
- record tick_count
- request_refresh
- wait for tick_count > before
- assert state remains Paused
```

Also decide `SampleNow` semantics. Prefer using the same publish path and incrementing the publication generation/tick consistently unless tests require a private sample. Document it.

### 7.2 Eliminate UI polling

**File:** `crates/tm-app/src/app.rs`

Current behavior:

- `logic()` polls engine,
- schedules repaint at `interval / 2`, minimum 50 ms,
- `ui()` polls engine again.

At Normal (1 s) the app wakes ~2×/s even when no input is happening, and background worker completion can be invisible until a periodic wake.

Implement a `UiNotifier`:

```rust
#[derive(Clone)]
pub struct UiNotifier(egui::Context);
impl UiNotifier {
    pub fn wake(&self) { self.0.request_repaint(); }
}
```

Or use a core-agnostic callback/channel if `tm-core` must not depend on egui.

Required wake sources:

- engine publishes new snapshot,
- services/startup/users fetch completion,
- icon decoded/texture backlog work,
- service control completion,
- app-history load/save status where visible,
- dump completion,
- service-jump completion,
- action executor completion,
- settings/background writer error,
- toast creation/expiry animation.

Then:

- poll engine once per actual repaint, not twice;
- remove normal periodic `request_repaint_after(interval/2)`;
- only schedule timed repaint while an animation/toast/FPS probe requires it.

Acceptance:

- worker result becomes visible in <50 ms under normal foreground conditions;
- idle window with paused/no animations consumes effectively zero UI CPU;
- Low update speed does not make menus/results feel delayed by seconds.

### 7.3 Stable toast IDs

The current toast identity is derived from elapsed time in a way that changes across frames. Give each toast a monotonic `ToastId` assigned on insertion. When event-driven, schedule repaint only while a toast is fading/expiring.

---

## 8. Replace table implementation with a typed, persistent column system

This is both a correctness fix and the foundation for Task Manager parity.

### 8.1 Fix resize logic immediately

**File:** `crates/tm-app/src/widgets/tablekit.rs`

CORRECTED after verifying egui 0.36 semantics: `drag_delta()` is PER-FRAME
movement, NOT cumulative from drag start (that is `total_drag_delta()`).
Storing a drag-start width and adding one frame of delta to it pinned the
column at its starting width. Accumulate the per-frame delta onto the live
width instead:

```rust
if response.drag_started() {
    // materialize any elastic/slack-absorbing column once, value-preserving
}
if response.dragged() {
    width = (width + response.drag_delta().x).clamp(min, max);
}
```

Same fix for the Performance resource-list splitter in `tabs/performance.rs`.

Regression automation must drive REAL pointer events through an egui
`Context` (see `tablekit::tests::dragging_name_boundary_tracks_cursor_across_frames`):
drag +60 px over two frames; the boundary must land exactly +60 px, not stay
near its starting width.

### 8.2 Remove `prev_dragging` from ephemeral `TmTable`

`TmTable` is reconstructed every frame, so previous-frame gesture state belongs either:

- in egui persistent/temp memory, or
- in `TablePrefs` stored on `TaskManApp`/settings model.

If settings writes become debounced, you do not need to write exactly on mouse-up; mark preferences dirty on every logical width change and let the settings writer coalesce it.

### 8.3 Typed `ColumnId` / `ColumnSpec`

Replace index-driven column behavior with stable IDs.

Suggested architecture:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnId {
    Name,
    Pid,
    Status,
    Cpu,
    CpuTime,
    CpuUtility,
    Memory,
    Disk,
    Network,
    Gpu,
    GpuEngine,
    // ...
}

pub struct ColumnSpec<R> {
    pub id: ColumnId,
    pub label: K,
    pub default_visible: bool,
    pub required: bool,
    pub default_width: f32,
    pub min_width: f32,
    pub alignment: Alignment,
    pub kind: ColumnKind,
    pub default_sort: SortDirection,
    pub capability: Option<Capability>,
    pub telemetry: TelemetryDemand,
    pub sort_key: fn(&R) -> SortKeyRef<'_>,
    pub render: fn(...),
}
```

The exact generic/enum shape may differ; the invariants matter:

- visibility, width, sorting, and rendering are all keyed by `ColumnId`, not a numeric index;
- adding/reordering a column cannot silently break comparator mappings;
- telemetry requirements can be derived from visible columns;
- unavailable columns can be disabled with a reason.

### 8.4 Persist visibility/order/width by stable ID

Current `Settings.col_widths: table -> Vec<f32>` is positional and cannot survive column insertion/reordering.

Introduce:

```rust
pub struct TablePrefs {
    pub visible_order: Vec<ColumnId>,
    pub widths: BTreeMap<ColumnId, f32>,
    pub sort: Option<(ColumnId, SortDirection)>,
}
```

Migration:

- when loading old positional `col_widths`, map each value through that tab’s old hard-coded column order;
- write new ID-based schema;
- ignore unknown future IDs rather than corrupting preferences;
- append new default-visible columns only if appropriate for migration semantics.

### 8.5 “Select columns…” UX

For tabs where Task Manager supports optional columns, header right-click should expose:

- quick checkboxes for common optional columns if desired;
- **Select columns…** dialog with searchable checklist;
- required columns disabled/checked;
- unsupported columns disabled with explanatory tooltip;
- Restore defaults;
- optional reorder controls/dragging (an app enhancement; not required to imitate Windows exactly).

Do not force every selected column to fit the viewport. Use horizontal scrolling when total width exceeds viewport.

### 8.6 Stop forcing last column to ignore its stored width

`TmTable::last_width()` always stretches/shrinks the last column to fill. This conflicts with persistent arbitrary column selection/ordering.

New layout behavior:

- honor all explicit widths;
- if total width < viewport, choose one designated elastic/fill column (normally Name/Description) to absorb spare space;
- if total width > viewport, show horizontal scroll;
- user width remains meaningful after reorder.

### 8.7 Precompute column geometry once per frame

Current `col_rect()` scans from column zero for every cell. With many optional columns this trends toward O(rows × columns²).

Build a `TableLayout` once:

```rust
struct TableLayout {
    x: Vec<f32>,
    widths: Vec<f32>,
    total_width: f32,
}
```

Cell rect lookup becomes O(1).

---

## 9. Virtualize every potentially large table

`tablekit::scrolled_table` currently uses `ScrollArea::show` and renders all rows. `BENCHMARKS.md` says tables are virtualized, but they are not.

### 9.1 Straightforward fixed-height tabs

Use `egui::ScrollArea::show_rows` or equivalent fixed-row virtualization for:

- Details
- Services
- Startup apps
- App History

Only render the visible row range plus small overscan.

### 9.2 Processes/Users hierarchical rows

Create a flattened display model per snapshot/UI state:

```rust
enum DisplayRow {
    GroupHeader { ... },
    Process { pid_identity, depth, ... },
    User { ... },
    AppGroup { ... },
}
```

Prefer one fixed row height if visually acceptable. If headers differ in height, either:

- use a virtualizer that supports variable row heights, or
- make group header rows the standard row height.

The flattened model should be rebuilt only when:

- a new snapshot arrives,
- search changes,
- sort changes,
- group/parent expansion changes,
- relevant display metadata changes.

### 9.3 Acceptance

Synthetic test/benchmark with 500 / 2,000 / 10,000 rows:

- rendered widget count should scale with viewport, not total rows;
- scrolling should remain responsive;
- no frame-time jump proportional to 10,000 rows;
- keyboard selection and context menu still work.

---

## 10. Remove hot-path allocations and frame spikes

### 10.1 Sorting

Avoid `to_lowercase()` inside O(n log n) comparators in:

- `tabs/processes.rs`
- `tabs/details.rs`
- `tabs/services.rs`
- startup/users/history where applicable

On row/model construction, cache normalized search/sort keys:

```rust
struct ProcessRow {
    display_name: String,
    sort_name: String, // Unicode-aware/case-fold strategy as chosen
    // preformatted common cell strings if useful
}
```

Use stable numeric typed keys for numeric columns.

### 10.2 Row rendering

Do not create `Vec<(f32, String)>` and several formatted `String`s for every visible row every repaint if the underlying snapshot did not change.

Build display-row cache on snapshot generation and hold compact formatted strings where formatting itself is material. For frequently changing percentages, benchmark formatting vs caching rather than blindly caching everything.

### 10.3 Performance charts

**File:** `tabs/performance.rs`

Current code repeatedly builds vectors from history. Cache series by:

- history generation,
- selected resource stable ID,
- graph time window,
- graph mode.

Incrementally append when possible. Do not allocate `Vec<Vec<f64>>` for all cores every frame if no new sample arrived.

### 10.4 Icon texture uploads

**File:** `crates/tm-app/src/icon_cache.rs`

Problems:

- `_budget` parameter is ignored;
- all completed icon images are drained/uploaded in a single call, allowing a large batch to cause a frame hitch;
- result vector and request channel are unbounded;
- cache can grow indefinitely;
- failed extraction is cached forever;
- worker starts during app construction.

Required design:

- lazy worker start;
- bounded request/result channel or explicit queue cap/backpressure;
- global upload budget per frame, e.g. 4–8 icons or a measured ≤0.5 ms time budget;
- if backlog remains, `request_repaint()`;
- LRU/memory cap (e.g. 512–1024 icons; measure actual texture memory);
- retry TTL for transient extraction failure;
- remove unused `_actions` argument from `get()`;
- worker completion wakes UI.

---

## 11. Processes tab corrections

**File:** `crates/tm-app/src/tabs/processes.rs`

### 11.1 Cache invalidation

Current display cache key includes timestamp/search/sort but not `expanded` or group-collapse state.

Add a local `view_generation` counter incremented when:

- a process parent expands/collapses,
- Expand All / Collapse All,
- group collapse changes,
- any state affecting the display rows changes.

Include it in display-model cache key.

### 11.2 Arbitrary-depth process tree

Current `emit_tree()` renders roots and only one child level. Replace with iterative DFS/BFS that supports arbitrary depth and guards cycles.

Identity should be `(pid, start_epoch_s)` or an equivalent process-generation key, not PID alone.

Pseudo:

```text
push roots in display order
while stack non-empty:
    pop node
    emit(depth)
    if expanded(node):
        push children reverse order with depth+1
```

Keep a visited identity set to break corrupt/self-referential ancestry.

### 11.3 Correct subtree aggregates in O(n)

Rewrite `subtree_values` as memoized iterative postorder or adjacency accumulation. The current state machine does not actually set/use its “done” state consistently and loops over all processes after each root.

Acceptance:

- O(n) / O(n log n), not O(n²), for a synthetic 10k-process hierarchy;
- cycle test terminates;
- aggregate includes all descendants;
- displayed group process count uses full subtree count where intended.

### 11.4 Three Windows groups

Current UI effectively splits Apps vs everything else, despite model having `ProcCategory::System`.

Use:

1. Apps
2. Background processes
3. Windows processes

Change `group_collapsed: [bool; 2]` to keyed state/three entries. Validate classification against actual Task Manager for:

- `svchost.exe`
- Windows shell components
- browsers with multi-process children
- Windows Terminal
- packaged/UWP/MSIX applications
- elevated apps

### 11.5 Exact “Go to Details” navigation

Add navigation state such as:

```rust
pub struct ProcessIdentity { pid: u32, start_epoch_s: Option<u64> }
pub pending_details_focus: Option<ProcessIdentity>;
```

When invoked:

- switch tab to Details;
- clear incompatible text filter if needed;
- select exact identity;
- scroll it into view after display model is built;
- if process exited, show a nonintrusive message rather than matching a same-name process.

### 11.6 Correct efficiency state

`efficiency_pids` currently reflects only actions performed during this app session. Query Windows process power-throttling/EcoQoS state and publish/cache it in `ProcessEntry` so external/preexisting state is shown correctly.

### 11.7 Process actions off UI thread

End task/tree, set priority, set affinity, suspend/resume, efficiency mode, shell operations can call Windows APIs and should not block the UI.

Create one reusable action executor (bounded worker thread/pool) rather than spawning ad hoc threads for every action.

- UI validates selected `ProcessIdentity`.
- Enqueue operation.
- Optionally show transient busy/optimistic state.
- Completion posts toast/result and wakes UI.
- Revalidate `(pid,start_time)` before destructive operations to avoid PID-reuse targeting.

### 11.8 Not responding

If current Windows sampler only maps sysinfo process status, add real GUI hang detection for visible top-level-window owners (`IsHungAppWindow`) and merge into process status carefully. Do not call it for every process if there is no window.

---

## 12. Details tab: correct values and broad optional-column support

**File:** `crates/tm-app/src/tabs/details.rs`

### 12.1 Replace index switch sorting

Sorting must be keyed by `ColumnId`. Add a unit test that iterates every registered sortable column and verifies its comparator uses the expected field.

Specific current bugs to cover:

- UAC virtualization must not sort by priority.
- GPU must sort by GPU value.
- GPU Engine must sort by engine label, not GPU percent/name.
- CPU is numeric with descending default.

### 12.2 Elevation

Populate `ProcessEntry.elevated` using process token `TokenElevation` (or equivalent) on Windows. Cache it under `TOKEN_SECURITY` demand; access-denied yields `None`, not false.

### 12.3 UAC virtualization

Query token information for:

- `TokenVirtualizationAllowed`
- `TokenVirtualizationEnabled`

Map to Task Manager–style values:

- Enabled
- Disabled
- Not allowed
- Unknown/unavailable if access fails

Do not infer this from SYSTEM vs non-SYSTEM.

### 12.4 Separate GPU and GPU Engine

Model should contain, at minimum:

```rust
pub gpu_util_pct: Option<f32>,
pub gpu_engine: Option<GpuEngineIdOrLabel>,
pub gpu_dedicated_bytes: Option<u64>,
pub gpu_shared_bytes: Option<u64>,
```

“GPU engine” column displays an engine like `GPU 0 - 3D` / equivalent, not a percentage.

### 12.5 Process properties path field

Current code edits `&mut path.clone()`, so edits are discarded and it misleadingly looks editable.

Make it clearly read-only/selectable, with Copy button if useful.

### 12.6 Processor affinity beyond 64 CPUs

Current action API uses one `u64` mask. That is insufficient on Windows machines with multiple processor groups / >64 logical processors.

Redesign platform action API around processor groups or CPU Sets. Do not silently truncate. The dialog should be able to represent all processors supported by the OS topology.

Cache topology outside the frame loop.

### 12.7 Add Analyze wait chain

Windows Task Manager supports **Analyze wait chain** in Details. Implement using Windows Wait Chain Traversal APIs (`OpenThreadWaitChainSession`, `GetThreadWaitChain`, etc.) or the documented API layer suitable for Rust bindings.

UI requirements:

- disabled/appropriate message when process is suspended;
- show dependency tree;
- identify involved PID/TID/process names;
- ending a dependency requires explicit selection/confirmation consistent with current destructive-action UX;
- run traversal on worker, not UI thread.

### 12.8 Initial Details column catalog

Do not implement this as one giant hardcoded display table. Register columns incrementally with capability/demand flags.

Recommended stable/core catalog to support where Windows APIs permit:

- Name
- PID
- Status
- User name
- Session ID
- CPU
- CPU time
- CPU Utility (Windows 11 25H2 optional compatibility metric)
- Memory / private working set
- Working set
- Peak working set
- Commit size
- Paged pool
- Non-paged pool
- Page faults / page-fault delta/rate
- Handles
- Threads
- User objects
- GDI objects
- I/O reads / writes / other operations
- I/O read / write / other bytes
- Image path name
- Command line
- Platform / architecture
- Elevated
- UAC virtualization
- Description/company if available
- GPU
- GPU Engine
- Dedicated GPU memory
- Shared GPU memory
- power throttling / efficiency status where useful

Before declaring exact “Windows parity,” inspect the target stable Windows 11 build live because the exact list evolves. The registry architecture must make adding columns cheap.

---

## 13. GPU/PDH correctness redesign

**Files:**

- `crates/tm-platform/src/win/gpu.rs`
- `crates/tm-platform/src/win/perfcounters.rs`
- `crates/tm-platform/src/win/sampler.rs`

This is a major correctness area and should not be patched by adjusting a clamp.

### 13.1 Preserve PDH identity

GPU Engine PDH instance strings contain fields including PID, LUID, physical/engine indexes, and engine type. Parse into a typed record:

```rust
struct GpuEngineSample {
    adapter_luid: AdapterLuid,
    pid: Option<u32>,
    phys_index: Option<u32>,
    engine_index: Option<u32>,
    engine_type: StringOrEnum,
    utilization_pct: f32,
}

struct GpuProcessMemorySample {
    adapter_luid: AdapterLuid,
    pid: u32,
    dedicated_bytes: u64,
    shared_bytes: u64,
}
```

Parse representative real instance strings in unit tests. Preserve unknown engine types rather than dropping them.

### 13.2 Map DXGI adapters by LUID

DXGI `AdapterLuid` is the join key. `gpu::merge()` should map:

- DXGI static adapter information (name, dedicated/shared capacity, device IDs)
- PDH engine/utilization records
- PDH process-memory records

by LUID.

Never copy one global utilization value to every adapter.

### 13.3 Verify Task Manager utilization semantics

Do not assume “sum all engines” equals GPU %.

Create directed workloads:

- 3D only
- Copy engine only
- Video Decode only
- Compute
- two engines concurrently
- workloads forced to GPU 0 vs GPU 1 on multi-GPU system

Compare the app to Windows Task Manager and document the chosen aggregation. Task Manager is commonly based on busiest relevant engine rather than naive sum, but use live verification for the target Windows build.

### 13.4 Per-process GPU and dominant engine

Current aggregation summing every per-PID engine instance then clamping to 100 loses engine identity and can overstate usage.

Compute:

- process GPU percentage according to verified semantics;
- dominant `(adapter, engine type/index)` for GPU Engine display;
- dedicated/shared process GPU memory per adapter and aggregate as appropriate for columns.

### 13.5 Adapter metrics

Fill correctly per adapter:

- utilization
- dedicated used
- shared used
- engine list
- driver version if the app presents it

If a metric is unavailable, use `Option`/unavailable rather than fabricated 0.

### 13.6 Avoid waking discrete GPU during startup unnecessarily

On hybrid laptops, instrument whether DXGI enumeration/queries or renderer choice wakes a dormant dGPU.

Requirements:

- default Processes page should not initialize GPU telemetry until GPU columns are visible or Performance/GPU is selected;
- WGPU renderer itself may still choose/use an adapter. Benchmark WGPU and Glow on hybrid hardware and document dGPU/power behavior.

---

## 14. Performance tab corrections

**File:** `crates/tm-app/src/tabs/performance.rs`

### 14.1 Splitter drag bug

CORRECTED: accumulate each frame's `drag_delta()` (per-frame movement) onto
the current width; do not freeze a drag-start width and add one frame of
delta to it.

### 14.2 Stable resource IDs

Current selection is an array index. Adapter/device list reordering can switch the selected resource unexpectedly.

Use:

```rust
enum ResourceKey {
    Cpu,
    Memory,
    Disk(DiskStableId),
    Network(AdapterStableId),
    Gpu(AdapterLuid),
}
```

Persist/select by key where appropriate.

### 14.3 Graph window must be time-based, not sample-count based

Current `visible_points()` treats `graph_seconds` as a point count. That produces different real durations at High/Normal/Low update speeds. `HISTORY_CAP=240` also conflicts with settings parser allowing up to 3600 s.

Fix:

- history points already contain timestamps; select points where `t >= latest - graph_seconds*1000`;
- x positions reflect real timestamps so delayed samples do not distort time;
- retention is duration-based or sized for the maximum configured window at fastest interval, with a sane cap;
- settings UI/parser and retention limits agree.

Tests:

- with 500 ms sampling and 60 s graph, visible span ≈60 s;
- with 1 s and 4 s sampling, still ≈60 s;
- irregular sample gap plots correct elapsed time.

### 14.4 CPU graph modes

Current Windows Task Manager default is overall CPU load and supports context menu:

- Change graph to → Overall utilization
- Change graph to → Logical processors
- Show kernel times

Add these modes.

For kernel time overlay, collect system/kernel CPU accounting separately. Store mode in settings if Windows behavior does; otherwise session state is acceptable after live parity check.

### 14.5 Dynamic labels

Do not hardcode a “60 seconds” caption/key if graph window is user-selectable. Format 30 s / 60 s / 2 min, etc.

### 14.6 Avoid rebuild allocations

Cache chart data as described in §10.3.

### 14.7 Disk mapping

Current mount/physical-disk matching by string/suffix is fragile for mount folders, Storage Spaces, volumes without drive letters, etc. Prefer Windows volume/device-number identity and map physical/virtual disk relationships explicitly.

---

## 15. Startup apps: correctness and parity

**Files:**

- `crates/tm-platform/src/win/startup.rs`
- `crates/tm-app/src/tabs/startup.rs`

### 15.1 Fix StartupApproved folder path

Use the full subkey:

```text
Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder
```

for file-name approval values.

Add a unit/integration abstraction test with a fake registry backend if possible; otherwise a Windows temp/user-registry integration test that creates a disabled folder approval value and verifies disabled state.

### 15.2 Do not ignore registry delete errors

When enabling a Startup-folder item, suppress only `ERROR_FILE_NOT_FOUND`; propagate other `RegDeleteValueW` failures.

### 15.3 Structured source identity

Do not infer common/user/HKLM from string contents like `ProgramData`.

Create:

```rust
enum StartupSource {
    Run { hive: Hive, view: RegistryView, value_name: String },
    StartupFolder { scope: UserOrCommon, path: PathBuf },
    PackagedTask { ... },
}
```

Use this stable ID for selection and actions.

### 15.4 Resolve command targets robustly

Current executable parsing is simplistic. Startup commands can include:

- quotes and arguments,
- environment variables,
- `.lnk` shortcuts,
- `.url`/shell entries,
- `rundll32`, script hosts, etc.

Resolve display executable/publisher on a worker. Preserve original command unchanged for diagnostics.

### 15.5 Match registered startup tasks

Microsoft states Task Manager provides the same list of startup applications as Windows Settings for apps “registered in Windows with a startup task.” Run keys + Startup folders may not cover all packaged/MSIX StartupTask registrations.

Implement/verify packaged startup task enumeration. Build a parity harness that compares your list to the target Windows Settings/Task Manager on a machine with:

- classic Run key app,
- 32-bit Run key app,
- user Startup folder shortcut,
- common Startup folder shortcut,
- packaged/MSIX startup task.

### 15.6 Startup impact

Do not invent impact values. Microsoft currently documents:

- None — disabled
- Not measured — enabled but no data
- Low — CPU <300 ms **and** disk <292 KB
- Medium — CPU 300 ms–1 s **or** disk 292 KB–3 MB
- High — CPU >1 s **or** disk >3 MB

Find the supported/appropriate Windows data source (StartupInfo/SRUM/other system telemetry). If there is no stable/public source suitable for this app, display **Not measured** rather than fabricated impact.

### 15.7 Publisher

Resolve publisher/company lazily via version metadata and/or signature info; do not block list display on it.

### 15.8 Stable selection/sorting/refresh

- selection by `StartupItem.id`, not list index;
- typed sorting;
- F5/Refresh invalidates cache and refetches;
- cache BIOS last-time value rather than registry-querying each frame;
- enable/disable runs on action executor and then refreshes.

---

## 16. App History and per-process network

### 16.1 Single serialized persistence worker

**File:** `crates/tm-core/src/app_history.rs`

Replace “clone map + spawn a new thread per save” with one long-lived writer:

```rust
enum HistoryWriteCmd {
    Save { generation: u64, snapshot: Arc<DbFile> },
    Flush(Sender<Result<()>>),
    Shutdown,
}
```

Coalesce saves so only newest pending generation is written. One writer owns the `.tmp` path; generations are monotonic; an old save can never finish after a newer one and replace it.

On exit, flush/join with a short, deterministic path. Avoid blocking for arbitrary seconds.

### 16.2 Avoid UI-thread full-map clone

Possible approaches:

- make entries immutable/Arc snapshot with copy-on-write at observation boundaries;
- serialize on worker from a transferred snapshot built incrementally;
- if map is small, cloning can remain but measure it and move it out of the UI pass.

### 16.3 Fix `clear()` double/synchronous save

`clear()` should mutate state + mark dirty/enqueue save. It should not synchronously save and then be saved again by UI.

### 16.4 Correct PID reuse

Extend `PrevTick`:

```rust
struct PrevTick {
    process_identity: ProcessIdentity, // pid + start time and/or creation time
    app_identity: AppIdentity,
    cpu_time_s: f64,
    net_total_bytes: Option<u64>,
}
```

If identity changes, treat as first sighting regardless of counter direction.

Test the dangerous case:

- old PID 7 has CPU=1, net=100;
- new unrelated PID 7 starts with CPU=5, net=1000;
- no delta must be attributed from old process to new app.

### 16.5 Stable app identity

Lowercase filename alone collides (`C:\A\app.exe` vs `D:\B\app.exe`). Prefer, in order:

- package/AUMID for packaged apps where available;
- normalized executable path plus file ID where possible;
- normalized path fallback;
- filename only as last resort.

Store display name separately from identity.

### 16.6 Network capability semantics

Until actual PID-attributed Windows network telemetry is implemented:

- do not display 0 for missing values;
- hide Network by default or show `—`;
- “Select columns” should disable/annotate unsupported telemetry if no implementation exists;
- App History should not accumulate fake zero network and imply a valid measurement.

### 16.7 Implement per-process network via ETW if parity is required

For full Windows parity, use a long-lived ETW session/provider set suitable for TCP/IP/network events. Requirements:

- one session, not started/stopped each tick;
- aggregate bytes by process identity/PID with process-start validation;
- handle dropped events and permissions explicitly;
- bound memory;
- demand-gated by visible network columns/App History;
- quantify CPU overhead under high network traffic.

If ETW privileges/provider constraints prevent reliable coverage, document limitations in UI.

### 16.8 Remove fabricated App History “Notifications”

Current UI shows a “Notifications” column with a hard-coded `0 MB`-style value. This is both wrong data and wrong unit. Remove until a real Windows source exists.

Do not silently truncate App History to 500 rows. Virtualize all entries or visibly disclose filtering/limit.

---

## 17. Settings and persistence

### 17.1 Debounced single settings writer

Current UI calls `settings.save()` synchronously on many interactions: theme, speed, language, zoom, graph duration, always-on-top, table changes, etc. Even a normally cheap local write can hitch under antivirus, redirected profile, storage pressure, or slow disk.

Implement one settings writer thread/channel:

- UI mutates in-memory settings immediately;
- enqueue/mark dirty with generation;
- coalesce for ~250–500 ms;
- serialize and atomically replace on worker;
- final flush on exit;
- expose errors as a nonblocking toast/log.

Do not spawn a thread per setting change.

### 17.2 Make `save_config` self-persisting

The master autosave flag must be persisted even when changing it to false. Possible clean design:

- `SettingsWriter::set_autosave(false)` writes the new master preference once, then stops automatically persisting other changes;
- explicit “Save now”/reset/export can bypass the gate.

Define Reset semantics clearly. If the user explicitly presses Reset, it should normally persist the reset state regardless of autosave mode or prompt them.

### 17.3 Durable atomic write

Current same-directory temp + rename is a reasonable basis. With one writer:

- create same-dir temp;
- `write_all` serialized bytes;
- flush; optionally `sync_all` if durability is a product requirement;
- rename/replace;
- optionally fsync parent directory where platform semantics matter;
- clean stale temp on next startup.

Do **not** introduce a false “rename cannot replace existing destination on Windows” workaround; current Rust `std::fs::rename` documents replacement semantics for an existing file destination.

### 17.4 `remember_window` semantics

Current startup still uses saved `window_size` and frame loop updates it regardless of whether remembering the window is enabled.

Implement:

- when disabled: launch default size/position and do not persist geometry;
- when enabled: persist size, position, maximized state;
- validate saved rect against current monitor topology and DPI; clamp/move on-screen if monitor disappeared;
- do not write geometry every frame; update in memory and debounce.

### 17.5 Default start page

Add a Task Manager–style **Default start page** setting and use it unless `--tab`/diagnostic override is supplied.

### 17.6 Clean dead settings

`show_net_column_anyway` appears not to be integrated into a proper capability/column system. Replace it with the unified column preference/capability model or remove it via migration.

---

## 18. Services, Users, and process action responsiveness

### 18.1 `run_new_task` can block UI up to 500 ms

**File:** `crates/tm-platform/src/win/process_ops.rs`

`run_new_task()` calls `shell_execute(..., wait=true)`, which does `WaitForSingleObject(process, 500)`.

This directly affects calls such as:

- Services → `services.msc`
- Users → `ms-settings:otherusers`

Make normal launch nonblocking. If the app wants to surface immediate child-process failure, do that on the action executor, not UI thread.

Also improve command parsing. Current `split_command` handles only basic quotes/spaces. Prefer a Windows-aware launch contract where file + params are separated by UI or use appropriate Win32 parsing/creation semantics.

### 18.2 Services

- worker completion must request repaint;
- add typed sorting/column preferences if target Task Manager allows them;
- cache normalized names for search/sort;
- context actions on action executor;
- F5 invalidates/refetches services.

### 18.3 Users aggregation

Current aggregation compares username strings repeatedly. `ProcessEntry` already has session ID support.

Build one `HashMap<session_id, Agg>` in a single process pass. This is faster and avoids domain/same-name ambiguity.

- session disconnect/logoff runs on worker/action executor;
- network unavailable is `—`, not zero;
- stable sort/selection;
- virtualize expanded process lists.

---

## 19. Minidump and destructive-action safety

### 19.1 Truncate dump target

**File:** `crates/tm-platform/src/win/process_ops.rs::create_dump_file`

Replace `OPEN_ALWAYS` with `CREATE_ALWAYS` (or explicit truncation) so an existing file cannot retain trailing data.

### 19.2 Process identity validation

Every destructive action should carry the selected process creation identity, not just PID:

- kill single/tree
- suspend/resume
- priority
- affinity
- efficiency mode
- dump where practical

Before action, reopen process and compare creation time/start identity when possible. If it no longer matches, report “process exited/replaced” and do nothing.

### 19.3 Handle lifetime

Windows 11 25H2 specifically calls out faster handle release after stopping a process. Audit all opened handles for RAII and prompt close. Prefer a small safe-handle wrapper to manual `CloseHandle` paths where practical.

---

## 20. Accessibility and keyboard interaction

Windows 11 25H2 lists Task Manager improvements in keyboard focus, Tab navigation, text scaling, screen-reader item names, and high-contrast heatmaps. The clone uses many custom-painted rows/cells, so default egui semantics are not enough.

### Required work

- Give custom table rows/cells AccessKit/egui widget metadata: role, accessible name, selected state, focus state, header association where feasible.
- Keyboard table navigation:
  - Tab / Shift+Tab into/out of controls
  - Up/Down selection
  - Home/End
  - Page Up/Page Down
  - Enter/Space for primary action/expand where appropriate
  - Left/Right for tree collapse/expand
  - Shift+F10/Menu key for context menu
  - F5 refresh
- Distinguish keyboard focus ring from selection color.
- Screen-reader row name should include meaningful process name and PID; individual cells expose header/value semantics.
- High-contrast mode: heatmaps cannot be the only signal; keep readable text/contrast and detect/use system high-contrast if APIs permit.
- Test Windows text scaling and mixed-DPI 100/125/150/175/200/225%.
- Test long German and English labels and narrow windows.

Do not counteract OS accessibility scaling by aggressively downscaling font sizes to fit tables.

---

## 21. Windows Task Manager parity matrix

This matrix distinguishes **stable/current documented behavior** from **preview/future-facing behavior**.

| Area | Current clone | Target | Priority |
|---|---|---|---|
| Processes: Apps / Background / Windows processes | Only 2 effective groups | 3 groups | P0/P1 |
| Header Select columns | Missing | Generic Select columns + persistence | P0/P1 |
| Details optional columns | Small fixed set | Broad registry-driven set | P1 |
| CPU graph Overall | Logical grid only | Overall default | P1 |
| CPU graph Logical processors | Present effectively | User-selectable | P1 |
| Show kernel times | Missing | Add overlay | P1 |
| Create process memory dump | Present | Correct truncate + worker | P0 |
| Analyze wait chain | Missing | Add | P1 |
| Default start page | Missing | Add Settings option | P1 |
| Real-time update speed | Present | Keep | — |
| Always on top | Present | Keep | — |
| Appearance | Present | Keep | — |
| CPU Utility optional column (Win11 25H2) | Missing | Add when metric available | P1 |
| DDR memory speed unit | MT/s appears intended | Keep/verify | — |
| Startup app enable/disable | Present, folder-state bug | Correct | P0 |
| Startup app impact | Unknown | Real source or Not measured | P1 |
| Startup packaged tasks | uncertain/missing | parity with Settings list | P1 |
| Per-process network | unavailable but shown as zero in places | ETW or unavailable | P0/P1 |
| Multi-GPU accurate adapters | Incorrect | Correct LUID-aware metrics | P0 |
| GPU Engine | mislabeled utilization | actual engine | P0 |
| Live kernel dump | Missing | Add Full / Kernel Stacks + advanced | P2 |
| Accessibility improvements | partial custom paint | full keyboard/screenreader/high contrast | P1 |
| NPU/NPU Engine/NPU memory | Missing | capability-gated future/preview | P2 |
| Isolation/AppContainer column | Missing | capability-gated future/preview | P2 |

### Note on newer preview columns

A Windows Insider Dev build published 2026-03-30 added optional NPU/NPU Engine columns to Processes/Users/Details, NPU Dedicated/Shared Memory in Details, neural engines on Performance, and an optional Isolation/AppContainer column. These are explicitly Insider/gradual-rollout features and may change; design the registry to support them, but do not treat them as a stable mandatory baseline.

---

## 22. Suggested per-tab default/optional column strategy

Exact defaults should be validated against the target stable Windows 11 build, but architecture can be implemented now.

### Processes

Default likely:

- Name
- Status
- CPU
- Memory
- Disk
- Network (only if real per-process telemetry exists)
- GPU (if supported and desired)
- GPU Engine as optional

Optional candidates:

- PID
- Publisher
- Command line (if product wants parity extension)
- Power usage / efficiency status where supported
- NPU/NPU Engine future
- Isolation future

### Details

Use broad list from §12.8. Name/PID should be required or strongly protected based on live native behavior.

### Users

Default:

- User
- Status
- CPU
- Memory
- Disk
- Network (real only)
- GPU as supported

Optional future NPU fields.

### Startup apps

Default:

- Name
- Publisher
- Status
- Startup impact

Additional/source columns may be offered as app-specific enhancements.

### Services

Default:

- Name
- PID
- Description
- Status
- Group (if collected)

### App History

Keep only metrics actually measured. Native Task Manager documentation describes CPU time and network activity; do not populate unsupported/fake columns.

---

## 23. Fix stale benchmarks and automation scripts

### `BENCHMARKS.md`

Correct these stale claims after implementation:

- “virtualized tables” is currently false;
- “Details sort comparator allocates nothing” is currently false for name sort;
- settings are INI now, not “atomic JSON”;
- `Sampler::new` is currently not cheap enough for the claim;
- some UI actions still block, notably `run_new_task` wait path and several direct process actions.

Document benchmark date, Windows build, CPU/GPU, renderer, monitor refresh, warm/cold methodology, and P50/P95.

### `tools/capture.ps1` / `tools/test-resize.ps1`

Scripts still target `%LOCALAPPDATA%\taskman\settings.json` and search for JSON `col_widths`. Update to current `config.ini` or, better, add test-only data directory override.

### Test data isolation

Add environment override such as:

```text
TASKMAN_DATA_DIR=<temp dir>
TASKMAN_CONFIG_DIR=<temp dir>
```

All automated UI tests must use a disposable directory, not delete/modify a developer’s real Taskman settings/history/logs.

---

## 24. Renderer/GPU startup decision matrix

Default features compile both WGPU and Glow; runtime prefers WGPU and falls back to Glow. Existing local benchmark reports similar `MainWindowHandle` timing, but that does not settle first-present, driver cold-start, hybrid-GPU, or frame-pacing behavior.

Benchmark:

| Environment | WGPU | Glow |
|---|---:|---:|
| modern desktop dGPU | first frame / WS / idle CPU / scroll frame time | same |
| integrated GPU laptop | same | same |
| hybrid iGPU+dGPU laptop | plus dGPU wake/power | plus dGPU wake/power |
| VM | success/fallback/start | success |
| RDP | success/fallback/start | success |
| older supported driver | success/start | success/start |
| 60 Hz | input/frame P95 | same |
| 120/144/165 Hz | input/frame P95 | same |

Keep FIFO/vsync unless testing demonstrates a better no-tear option. `desired_maximum_frame_latency=2` should be compared against 1 for input latency and stability; do not change by assumption.

### Fallback construction safety

If WGPU fails after the app creator has partially run, a second `run_native` attempt can construct another app. After refactor, ensure this cannot leave duplicate workers, persistence writers, or collectors alive.

Prefer a bootstrap/resources object whose ownership is explicit per renderer attempt, or make pre-renderer resources cheap/idempotent and shut them down before fallback.

---

## 25. Test plan: unit/regression tests to add

At minimum add:

1. `engine_refresh_while_paused`
2. `sample_now_publication_semantics` if tick generation is unified
3. `dragging_name_boundary_tracks_cursor_across_frames` (real pointer events
   through an egui `Context`; supersedes the flawed drag-start-width idea)
4. `performance_splitter` accumulation is covered by manual verification +
   the tablekit input-driven tests (same one-line math)
5. `table_preferences_migrate_positional_widths_to_ids`
6. `details_every_column_sorts_its_own_field`
7. `process_expansion_invalidates_display_cache`
8. `process_tree_supports_three_plus_levels`
9. `process_tree_cycle_terminates`
10. `subtree_aggregation_counts_all_descendants`
11. `process_groups_system_separate_from_background`
12. `goto_details_selects_exact_process_identity`
13. `startup_folder_approved_disabled_is_detected`
14. `startup_folder_enable_propagates_non_not_found_delete_errors`
15. `settings_save_config_false_persists_itself`
16. `app_history_pid_reuse_with_higher_new_counters_has_zero_cross_process_delta`
17. `app_history_identity_distinguishes_same_filename_different_path`
18. `app_history_writer_generations_cannot_reorder`
19. `missing_network_renders_unavailable_not_zero`
20. `graph_window_is_time_based_at_high_normal_low_intervals`
21. `graph_window_handles_irregular_timestamps`
22. `gpu_pdh_instance_parser_extracts_pid_luid_engine`
23. `gpu_multi_luid_records_do_not_cross_assign_utilization`
24. `gpu_process_dominant_engine_is_preserved`
25. `minidump_target_creation_truncates_existing_file` (Windows integration)
26. `worker_completion_requests_repaint` where practical through notifier mock
27. `icon_upload_budget_is_respected`
28. `icon_cache_eviction_is_bounded`
29. `remember_window_false_does_not_restore_or_persist_geometry`
30. `unknown_future_column_ids_do_not_break_settings_load`

Use property/fuzz tests where useful for process trees and PDH string parsing.

---

## 26. Runtime performance acceptance criteria

These are targets, not reasons to game measurements. Report actual hardware/Windows build.

### Startup

- first usable shell near renderer/framework floor;
- P50 warm ≤350 ms and P95 warm ≤500 ms on the reference machine if achievable without regression;
- no sampler construction/PDH/DXGI/file-version batch blocks or competes before first frame;
- first core data ≤750 ms warm;
- no unnecessary dGPU telemetry initialization on a default page.

### UI/frame pacing

On a normal process count (~300):

- no synchronous disk/registry/process wait in the paint/input path;
- 120/144 Hz scrolling/column drag feels 1:1;
- target P95 UI CPU frame time <8 ms at 120 Hz, preferably <4–5 ms on reference 144 Hz hardware;
- no >16.7 ms application-caused stalls during normal table interaction;
- resize boundary tracks pointer within a few pixels.

With 2k/10k synthetic rows:

- virtualization limits rendering to visible/overscan rows;
- sorting/model rebuild can take longer than 300 rows but must not cause repeated per-frame work; consider background sort for extremely large lists only if measured necessary.

### Idle

- no continuous repaint without animation/input/new data;
- UI idle CPU approximately 0; target <0.2–0.5% on reference hardware;
- sampler CPU proportional to selected telemetry demand.

### Sampler

After warm caches:

- ~300 processes Normal tick P95 ideally <75 ms;
- ~1000 processes P95 <150 ms target on reference hardware;
- no recurring file-version query per process;
- optional GPU/disk/network/security work disappears or drops materially when demand is off.

### Memory

- icon cache bounded;
- app history bounded/compacted;
- no thread count growth over time;
- no unbounded worker result queues;
- steady working set stable over a 2-hour soak test.

---

## 27. Windows correctness comparison suite

On a real Windows 11 25H2 system, run this clone and native Task Manager side-by-side.

### CPU/memory

- compare total CPU over 30 s;
- compare per-process CPU under a controlled single-thread load;
- compare committed/in-use memory and a known process working set/private memory;
- verify CPU Utility if implemented.

### GPU

Use GPU 0/GPU 1 directed workloads and separate 3D/video/copy/compute workloads. Confirm:

- only the correct adapter moves;
- process GPU % tracks native Task Manager reasonably;
- GPU Engine labels are correct;
- dedicated/shared memory is per correct adapter/process.

### Startup apps

Compare exact item set/status for registry, Startup folder, common Startup folder, and packaged startup tasks. Compare impact where available.

### Details

Spot-check 20 processes including SYSTEM, admin app, non-admin app, 32-bit process, suspended process, packaged app:

- PID/session/user
- architecture
- elevation
- UAC virtualization
- handles/threads if implemented
- GPU engine
- command line/path

### Actions

Verify end task/tree, suspend/resume, priority, affinity, efficiency, service start/stop, user session actions. Ensure UI never freezes while OS action is slow.

---

## 28. Suggested implementation sequence / commit plan

Keep changes reviewable and benchmark after each startup-sensitive stage.

### Commit 1 — baseline and test isolation

- add startup markers and P50/P95 benchmark script;
- add `TASKMAN_DATA_DIR`/config override;
- update stale scripts for INI;
- add missing regression tests that currently fail.

### Commit 2 — platform factory split + lazy engine

- `create_actions` / `create_collector`;
- eliminate duplicate stack;
- parked/lazy engine or post-first-frame engine start;
- phase marker output.

Benchmark startup immediately.

### Commit 3 — first-frame I/O deferral

- defer app-history load;
- lazy icons;
- defer/measure system fonts;
- defer disk logging for GUI;
- console attach only for CLI.

Benchmark WGPU/Glow again.

### Commit 4 — event-driven UI notifier

- engine publish wakes UI;
- workers wake UI;
- remove interval/2 polling/double poll;
- stable toast timing/IDs.

Measure idle CPU and worker-completion latency.

### Commit 5 — settings/history writer services

- debounced SettingsWriter;
- autosave-off persistence fix;
- serialized AppHistory writer/generations;
- async history load;
- window geometry semantics.

### Commit 6 — typed table/column prefs + resize fix

- `ColumnId`, `ColumnSpec`, `TablePrefs`;
- migration;
- Select columns dialog/menu;
- correct resize state;
- O(1) layout geometry.

### Commit 7 — table virtualization + display caches

- Details/Services/Startup/AppHistory fixed-row virtualization;
- flattened Processes/Users virtual model;
- normalized sort/search keys;
- row allocation cleanup.

### Commit 8 — Processes correctness

- arbitrary-depth tree;
- O(n) subtree aggregation;
- System/Windows group;
- expansion cache generation;
- exact Go to Details;
- process identity for selection/actions.

### Commit 9 — Details/security correctness

- typed sorting;
- elevation/UAC token queries;
- real GPU/GPU Engine fields plumbing;
- processor group affinity model;
- read-only properties path;
- Analyze wait chain.

### Commit 10 — GPU/PDH redesign

- demand split;
- LUID-aware records;
- correct multi-GPU merge;
- per-process dominant engine/memory;
- GPU benchmarks/comparison tests.

### Commit 11 — Startup apps parity

- StartupFolder approval path bug;
- structured source ID;
- packaged tasks;
- publisher/impact data source;
- stable selection/sort/refresh.

### Commit 12 — network/App History parity

- unavailable semantics first;
- ETW per-process network if product requires full parity;
- app identity/PID reuse;
- remove fake Notifications.

### Commit 13 — Performance/Settings parity and accessibility

- CPU Overall/logical/kernel modes;
- default start page;
- time-correct charts;
- AccessKit/keyboard/high contrast.

### Commit 14 — advanced features

- live kernel dump + advanced settings;
- capability plumbing for NPU/Isolation preview columns.

### Final commit — documentation/benchmark refresh

- rerun all performance numbers;
- replace stale `BENCHMARKS.md` claims with measured results;
- document stable unsupported features explicitly.

---

## 29. Definition of done

The work is not complete until all of the following are true:

### Correctness

- All P0 bugs in this document have regression tests or Windows integration tests.
- No unsupported telemetry is displayed as a measured zero.
- Multi-GPU metrics are adapter-correct.
- Details elevation/UAC/GPU Engine columns reflect actual Windows data or unknown.
- F5 refresh works in running and paused modes and tab-specific refreshes do real work.
- Startup-folder disabled state is correct.
- process actions validate identity where PID reuse could be dangerous.
- minidump overwrites/truncates safely.

### Startup

- GUI constructs only one collector.
- collector is not constructed on the UI thread.
- first frame is not delayed by first sampling tick.
- system-font/history/log-directory/icon-worker work is deferred or retained only with benchmark evidence.
- startup measurements report first usable frame, not only HWND creation.

### Responsiveness

- no known up-to-500 ms waits on UI thread;
- large tables virtualized;
- worker completions repaint immediately;
- no periodic UI polling solely to discover completed background work;
- icon upload/cache bounded;
- settings/history writes cannot hitch UI.

### Feature parity

- column visibility customization is implemented and persistent;
- CPU Overall/Logical + kernel overlay exists;
- Analyze wait chain exists;
- default start page exists;
- three Process groups are shown;
- Windows 11 25H2 accessibility expectations have been tested;
- advanced/preview gaps are documented/capability-gated.

### Quality gates

- `cargo fmt --check` clean;
- `cargo clippy ... -D warnings` clean;
- all workspace tests pass;
- release WGPU+Glow and Glow-only builds pass on Windows;
- 2-hour soak has stable thread count/memory;
- startup and frame-time report checked into docs with machine/build metadata.

---

## 30. Official references used for parity decisions

Use these sources when implementing; also verify against the actual stable Windows build being targeted because Task Manager evolves over cumulative updates.

1. **Microsoft Learn — Troubleshoot processes by using Task Manager**  
   https://learn.microsoft.com/en-us/troubleshoot/windows-server/support-tools/support-tools-task-manager  
   Documents current CPU overall view, Show kernel times, Change graph to → Logical processors, right-click header → Select columns, process memory dumps, and Analyze wait chain. Last updated 2026-02-12 at audit time.

2. **Microsoft Learn — What’s new in Windows 11, version 25H2**  
   https://learn.microsoft.com/en-us/windows/whats-new/whats-new-windows-11-version-25h2  
   Documents Task Manager sorting/reliability/accessibility improvements, MT/s memory speed, faster process-handle release, standardized CPU workload metric, and optional CPU Utility column.

3. **Microsoft Support — Configure Startup applications in Windows**  
   https://support.microsoft.com/en-US/Windows/Experience/Startup-Boot/configure-startup-applications-in-windows  
   States Task Manager presents the same registered startup-app list as Settings and documents Startup impact thresholds.

4. **Microsoft Learn — Task Manager live memory dump**  
   https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/task-manager-live-dump  
   Documents live kernel dump from System, Full vs Kernel Stacks, and advanced hypervisor/user-page/memory-pressure options.

5. **Microsoft Support — Use a screen reader to navigate Windows support tools**  
   https://support.microsoft.com/en-US/accessibility/windows/use-a-screen-reader-to-navigate-windows-support-tools  
   Summarizes Task Manager tabs/settings including default start page and advanced live-dump options.

6. **Windows Insider Blog — Build 26300.8142 (2026-03-30)**  
   https://blogs.windows.com/windows-insider/2026/03/30/announcing-windows-11-insider-preview-build-26300-8142-dev-channel/  
   Preview-only/future-facing: NPU/NPU Engine, NPU Dedicated/Shared Memory, neural engines on Performance, Isolation/AppContainer column. Treat as capability-gated preview, not stable mandatory parity.

---

## 31. Final implementation guidance

The most important architectural rule is: **paint a useful shell first, then acquire progressively richer telemetry without ever making the UI wait for it.** The second is: **missing data must remain missing, never be converted into plausible-looking zero/false values.** The third is: **table columns are data-model features, not hard-coded screen indexes.**

If implementation time is constrained, prioritize in this order:

1. duplicate sampler/startup contention removal;
2. resize/F5/startup-registry/minidump/security/GPU correctness bugs;
3. event-driven repaint + async persistence/actions;
4. table virtualization and typed column system;
5. multi-GPU + per-process telemetry correctness;
6. Windows parity/accessibility;
7. advanced/preview features.

Do not mark the audit “fully implemented” merely because the app builds. Re-run the Windows comparison/benchmark suite and attach measured before/after results for startup, idle CPU, sample time, scroll/resize frame pacing, memory, and multi-GPU correctness.

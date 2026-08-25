# GUI Framework Benchmark & Decision

Measured on the dev machine (Windows 11, AMD Ryzen 7 5700X, RTX GPU, 32 GB).
Method: minimal-but-representative app (labels, button, 10-row table, painter-drawn line chart).
`WINDOW_MS` = wall-clock from process spawn until the OS reports a visible main window
(PowerShell polling `MainWindowHandle`, 4 ms resolution), best-of-5 after warmup.
`WS` = working set at that moment.

| Framework / renderer        | Binary    | WINDOW_MS (min..med) | WS (MB) | Build (release) |
|-----------------------------|-----------|----------------------|---------|-----------------|
| egui 0.36 + glow (OpenGL)   | 6.1 MB    | 277..286 ms          | ~12     | 22 s            |
| egui 0.36 + wgpu (DX12/VK)  | 12.3 MB   | 279..296 ms          | ~14     | 36 s            |
| egui 0.36 + glow+wgpu both  | 13.3 MB   | 295..317 ms          | ~17     | 1 m 32 s        |
| iced 0.14 + wgpu            | 9.5 MB    | 273..285 ms          | ~14     | 40 s            |
| slint 1.17 + femtovg (GL)   | 10.9 MB   | 277..286 ms          | ~13     | 2 m 45 s        |

## Interpretation

* **Startup is a wash**: every native-GPU option shows its window in ~275–320 ms,
  dominated by process spawn + driver init, not by framework choice.
  All leave ample headroom under the < 1 s budget even with system sampling added.
* **Binary size**: glow-only is smallest; wgpu roughly doubles it; both-together costs
  ~+1 MB over wgpu-only. Irrelevant next to functionality.
* **RAM** differences are noise (~11–18 MB).

## Decision

**egui 0.36 via eframe, compiling BOTH renderer backends: `wgpu` primary
(DX12 / Vulkan / Metal), `glow` fallback (OpenGL).** Rationale:

1. **Runtime renderer selection** = resilience: if DX12/Vulkan surface creation fails
   (old drivers, VMs, RDP sessions), we fall back to GL at startup instead of dying.
   No other candidate offers this without extra work.
2. **Immediate-mode ergonomics** fit a dense, data-refreshing dashboard:
   sortable virtualized tables (`egui_extras::TableBuilder`), custom-painted charts,
   heatmap cells, context menus, dynamic columns — all trivial compared to retained/
   declarative models (iced's Elm architecture and Slint's property system fight you
   on per-cell coloring and ad-hoc menus).
3. **Theming**: built-in dark/light visuals with runtime switching; total control over
   colors/rounding/spacing to replicate the Win11 look.
4. **Font rendering**: glyph rasterization at exact device pixels per scale factor →
   sharp text at any DPI; we bundle Inter (+ tabular mono font for numbers).
5. **VSync** present mode (FIFO) → no tearing on all platforms/backends.
6. Fastest iteration compile times by far (22–36 s vs Slint's ~3 min).

## Candidates evaluated and excluded

| Candidate | Why not |
|---|---|
| Tauri v2 | WebView2 = multi-process (msedgewebview2.exe), 100–200 MB RAM, cold start typically ≥ 500 ms–1 s, depends on system webview version → violates single-exe/sub-second/lightweight goals. |
| GTK4 (gtk4-rs) | Windows font/theme/bundling story poor; inconsistent look across OSes. |
| Qt (C++/QML or cxxqt) | Licensing/build complexity; heavy toolchain; not "modern Rust". |
| Flutter | Not a Rust core integration; large embedder runtime. |
| Xilem / Dioxus-native | Experimental renderers; too risky for production-quality UI today. |
| Slint | Strong contender (#2); lost on compile times and custom dense-widget effort. |
| iced | Strong contender (#3); lost on table/menu ergonomics for this app class. |

## Rebuild benchmarks

Each bench dir is standalone: `cd bench/<dir> && cargo build --release`, then

```
powershell -NoProfile -ExecutionPolicy Bypass -File bench/bench-window-ms.ps1 <abs path to exe>
```

## Final app measurements (v2, full Task-Manager UI)

Same machine, `cargo build --release` (fat LTO, strip=symbols, panic=abort).
Startup = spawn → `MainWindowHandle` (40 ms polling), median of 4 runs.

| Build                        | Binary    | Startup-to-window | Working set |
|------------------------------|-----------|-------------------|-------------|
| default (wgpu + glow)        | 12.5 MB   | ~0.55–0.60 s      | ~28 MB      |
| `--no-default-features --features glow` | 7.6 MB | ~0.55–0.60 s | ~28 MB      |

The glow-only build is the size-optimized option (`cargo build --release
--no-default-features --features glow`); both stay well under the 1 s budget
while sampling ~280 processes at 1 Hz in the background.

## v3 performance pass (this tree)

Same machine/method as above, HEAD vs this tree, default (wgpu+glow) builds:

| Metric                       | HEAD        | v3          | Δ            |
|------------------------------|-------------|-------------|--------------|
| Startup-to-window (median)   | ~586 ms     | ~546 ms     | −40 ms       |
| Working set at window        | ~30 MB      | ~28 MB      | −2 MB        |
| Binary (default)             | 12.46 MB    | 12.53 MB    | +0.6 %       |
| Binary (glow-only)           | 7.52 MB     | 7.60 MB     | +1 %         |
| Direct dependencies          | 4 more      | −4          | see below    |
| Sampling tick (280 procs)    | —           | 40–55 ms    | 1 Hz budget  |
| Services enumeration         | UI thread   | worker, ~1 ms SCM + enrich | no UI stall |

Changes behind the numbers:

* **No UI-thread blocking**: services/startup/users lists, service
  start/stop/restart waits (up to 15 s!), "Neuen Task ausführen" (500 ms),
  F5 refresh (up to 10 s), icon extraction and app-history autosave all run
  on worker threads now; results arrive via channels/toasts.
* **Sampler hot path**: PDH collected once per tick (was twice), DXGI adapter
  probe cached, per-pid native attribute queries (session id/WOW64/priority/
  handles) cached with a 10-tick TTL instead of ~4 syscalls × 280 procs/tick,
  ancestor walk without per-hop `String` clones, cheap `Sampler::new` (heavy
  init moved to the engine thread → window appears sooner).
* **Per-frame**: Performance-tab charts read the history tail without cloning
  it, Details sort comparator allocates nothing, Users-tab debug print removed,
  process-tree aggregation is iterative + cycle-safe (was recursive).
* **Dependencies dropped** (all replaced with std, no side effects):
  `egui_extras` (was entirely unused), `crossbeam-channel` (→ `std::sync::mpsc`
  + `recv_timeout`), `parking_lot` (→ `std::sync` with poison-tolerant
  helpers), `thiserror` (hand-written impls). Binary size stays flat because
  the new worker-thread code offsets the removals.
* **Settings switches** apply and persist instantly (atomic JSON write per
  change); new UI-zoom switch (80–125 %) wired to the previously dead
  `ui_zoom` setting.

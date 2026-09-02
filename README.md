# TaskMan

A faithful, native-feeling Windows Task Manager reimplementation in Rust —
down to ClearType text rendering and a software renderer that needs no GPU
driver at all.

Four-crate workspace built around a strict platform boundary, with an
optional protected Windows service for privileged operations and for
telemetry an ordinary process cannot get unelevated.

## Highlights

- **Processes** — native-style grouped presentation (Apps / Background /
  Windows) with window-ownership-driven app grouping, plus a literal PPID
  tree on Details. Multi-select, search across name, publisher, PID, user,
  image path and command line, keyboard navigation, and PID-reuse-safe
  destructive actions.
- **Performance** — CPU (overall + per-logical-processor with kernel times),
  memory, disk, network (per-adapter with IPv4/IPv6/SSID/signal metadata),
  and GPU with per-engine graphs.
- **Real telemetry, honestly missing** — per-process network comes from a
  real-time ETW session (hosted by the optional service so the unelevated
  GUI can see it), GPU engines from DXGI/PDH, process owners from token
  queries. When telemetry is unavailable the UI shows "—", never a
  fabricated zero.
- **Details** — typed column catalog with persisted visibility/order/width,
  a module inspector with guarded unload, full-memory dumps, priority,
  affinity, efficiency mode, and UAC virtualization.
- **Privileged operations behind a protected broker** — `taskman-service.exe`
  is a delayed-auto LocalSystem service that owns only an allowlisted set of
  commands over a local named pipe with kernel-derived PID +
  executable-path checks, protected DACLs, and staged hash-verified upgrades
  in Program Files.
- **A vendored egui fork** (`vendor/egui`, tag 0.36.1) that adds sub-pixel
  (ClearType) text rasterization and a native CPU renderer presenting via
  `softbuffer`, alongside the glow/wgpu backends. See
  `vendor/egui/TASKMAN-FORK.md` for the divergence inventory.
- Tray with close-to-tray and autostart, single-instance coordination, an
  optional Task Manager replacement (IFEO), German/English localization,
  and light/dark themes.

## Requirements

- Windows 11 (developed for and tested on; Linux/macOS platform backends
  exist in the tree but are secondary)
- Rust 1.85+ (edition 2024)
- Python 3.10+ for the `build.py` release driver

## Building

```bash
# development loop (fast, debug)
cargo build -p tm-app

# full quality gate: fmt --check + clippy -D warnings + all workspace tests
# (including Windows integration tests) + the vendored-egui fork gate
python build.py --check

# release build of the host binary
python build.py --host-only

# release packaging into dist/ (adds Linux x86_64 when cross/zigbuild exists)
python build.py
```

The binaries land in `target/release/taskman.exe` and
`target/release/taskman-service.exe`. `taskman.exe --selfcheck` runs a
headless sampling smoke test and prints a JSON summary.

## Repository layout

| Path | What it is |
| --- | --- |
| `crates/tm-core` | Platform-agnostic model, engine, settings, app history, i18n |
| `crates/tm-platform` | OS collectors and actions behind traits (Windows stack under `win/`) |
| `crates/tm-app` | The eframe GUI |
| `crates/tm-service` | The optional Windows service executable |
| `vendor/egui` | Vendored egui fork (sub-pixel text + CPU renderer) |
| `audit.md`, `hardening/`, `llm-wiki/` | Parity audit, security-analysis artifacts, and maintainer documentation |

## Diagnostics

| Environment variable | Effect |
| --- | --- |
| `TASKMAN_RENDERER=glow\|wgpu` | Force renderer |
| `TASKMAN_FPS_PROBE=1` | Continuous repaints + fps overlay |
| `TASKMAN_DIALOG=settings\|run` | Open a dialog at startup |
| `TASKMAN_PERF=<key>` | Preselect a Performance resource |
| `TASKMAN_DATA_DIR` / `TASKMAN_CONFIG_DIR` | Isolate data/config directories |

## Status

Substantially at parity for daily use. The precise remaining gaps (SRUM-backed
app history, startup-impact telemetry, memory composition, accessibility, and
more) are itemized in `audit.md` and `llm-wiki/known-debt.md`.

## License

MIT — see [LICENSE](LICENSE). The vendored egui fork retains its
MIT OR Apache-2.0 licensing.

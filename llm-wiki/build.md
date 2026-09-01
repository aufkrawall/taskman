# Build System

Last verified: 2026-09-01

Primary sources:
- `build.py`
- `Cargo.toml` (workspace profiles)
- `AGENTS.md`

## build.py — release driver

`python build.py` (default) does, in order:

1. Host release build (`cargo build --profile release --workspace`). On
   Windows this must produce both `taskman.exe` and `taskman-service.exe`.
2. Linux x86_64 release cross-build **by default** — the workspace ships a
   real Linux collector (`crates/tm-platform/src/linux/`). Cross toolchain
   is auto-detected: `cross` first, then `cargo-zigbuild`. Without either,
   the step is skipped with a note (exit code still 0 unless
   `--require-all-targets`).
3. Packaging into `dist/`: Windows → `.zip` containing the GUI and service;
   Linux → `.tar.gz`, named `taskman-v<version>-<platform>`.

Flags: `--host-only`, `--linux-only`, `--debug`, `--no-package`,
`--require-all-targets`, `--check`.

## Pushing: no git-LFS pointers may enter this repository

Vendoring egui made the repository unpushable, in two places at once:

```
Git LFS upload failed: (missing) vendor/egui/.../dpi_2.00.png     # local pre-push hook
remote: error: GH008: Your push referenced at least 214 unknown Git LFS objects
```

egui tracks its demo/kittest snapshot PNGs in git-LFS, and those objects 404 from
upstream's LFS server — so the subtree brought ~230 pointer files describing content that
exists nowhere. GitHub validates every pointer in a push against the repository's LFS store
and declines the push when one is missing.

The stubs are therefore **deleted from this fork's history**, and `git subtree pull` must
delete them again each time it re-adds them. Nothing here builds those crates. Neither the
local hook nor GitHub's check consults `.gitattributes`, so disabling the `*.png
filter=lfs` rules (which this fork does, and which is what stops a clone from trying to
smudge them) does not substitute for removing the files. See
`vendor/egui/TASKMAN-FORK.md` for the removal procedure and why it has to key on blob OID
rather than on path.

If a clone ends up with LFS machinery active anyway, this disarms it locally:

```bash
git lfs uninstall --local
git config --local lfs.allowincompletepush true
```

## Quality gate

`python build.py --check` runs the full gate:
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features`
- `tools/check-fork.ps1` -- the vendored egui fork's own fmt/clippy/test. **Not
  redundant:** `cargo clippy --workspace -- -D warnings` only passes those flags to the
  packages cargo selected, and `vendor/egui` is a deliberately excluded separate
  workspace. Without this step the fork's crates sit outside the gate entirely.

Nested inside a release artifact build when run as
`python build.py --host-only --check`.

## Local release binary freshness

`target/release/taskman.exe` is the binary that gets launched locally.
The dev loop (`cargo build -p tm-app`, `cargo test -p …`) only refreshes
`target/debug`, so it goes stale unless a release build runs. Convention:
every code-change session closes with `python build.py --host-only`
(redundant after `--check`, which builds release anyway). Thanks to the
profile policy below, a warm incremental release rebuild is cheap
(seconds for tm-app-only changes); cargo then reports "Finished" without
recompiling when the binary already matches the sources.

## Profile policy (deliberate, do not regress)

| Setting | Value | Why |
| --- | --- | --- |
| release lto | `"thin"` | fat LTO serialized minutes of linking for ~ms of startup; rejected |
| release codegen-units | default (16) | parallel final codegen; `= 1` was removed for compile speed |
| dev/test debuginfo | `line-tables-only` | full DWARF across 16 parallel rustc processes exploded RAM; line tables keep backtraces usable |
| dev deps opt-level | 2 | wgpu/sysinfo stay usable in dev without per-dep hacks |

Measured on the 16-core dev machine: cold workspace release ≈ 2 min
(dependency-bound: wgpu/eframe), warm incremental dev rebuild of the whole
chain ≈ 4 s. The shared chain tm-core → tm-platform then fans out to
tm-app/tm-service, so cross-crate changes rebuild the shared portion before
the final binaries can build independently.

## Renderer/backend policy

`tm-app` enables eframe WGPU without its broad default backend set, then adds
one target-native backend through a target-specific direct `wgpu` dependency:

- Windows: D3D12 only (no Vulkan WGPU backend in the Windows binary).
- Linux: Vulkan only.
- macOS: Metal only.

The GUI now tries the native CPU renderer (`Renderer::Software`) first; WGPU and Glow
remain as fallbacks. The CPU path needs no driver, starts without enumerating adapters or
compiling shaders, and is the only backend that can do sub-pixel text.
Surface presentation is FIFO with one-frame maximum latency, and WGPU prefers
the low-power adapter unless `WGPU_POWER_PREF` overrides it. This trims unused
backend dependency/code without making older or unusual graphics systems a
hard failure. The first 2026-08-31 backend-trim build measured 13,470,208
bytes. The final service/tray/reliability build is 13,873,664 bytes versus the
14,656,000-byte pre-pass baseline (782,336 bytes / 5.34% smaller) while adding
the new features. The separate always-running service is 1,378,816 bytes. See
`debug-tools.md` for overrides.

The service has no GUI/eframe dependency. Keeping it in a separate binary
prevents an always-running renderer, GPU stack, or broad GUI parser from
becoming part of the LocalSystem attack surface.

## Test isolation

All automated UI/test runs should set
`TASKMAN_DATA_DIR` / `TASKMAN_CONFIG_DIR` to temp dirs so developer data is
never touched. Settings tests additionally use an in-process path override
(`set_default_path_override_for_tests`) guarded by `TEST_OVERRIDE_LOCK` to
avoid env-var races between parallel tests.

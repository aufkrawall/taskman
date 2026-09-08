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
2. Linux x86_64 release build **by default** — the workspace ships a
   real Linux collector (`crates/tm-platform/src/linux/`). Toolchain
   resolution: `cross` first, then `cargo-zigbuild` (both produce a glibc
   binary, artifact `taskman-v<version>-linux-x86_64`). Without either, the
   self-contained path is used: `x86_64-unknown-linux-musl` std (installed
   through rustup on demand) linked by the bundled `rust-lld` into a static
   PIE, artifact `taskman-v<version>-linux-x86_64-musl`. That path needs no
   compiler, container or zig, so `python build.py` always produces a Linux
   artifact on a rustup machine. It creates an empty `target/cross-stubs/libdl.a`
   because `libloading` emits `-ldl`, which musl folds into libc. The Linux
   step is skipped only when neither path is available (or with
   `--host-only`); `--require-all-targets` makes a skip fatal.
3. Packaging into `dist/`: Windows → `.zip` containing the GUI and service;
   Linux → `.tar.gz`, named `taskman-v<version>-<platform>`.

Flags: `--host-only`, `--linux-only`, `--debug`, `--no-package`,
`--require-all-targets`, `--check`, `--audit`.

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
- `cargo check --workspace --target <non-host>` for `x86_64-unknown-linux-gnu`
  and `aarch64-apple-darwin`, skipping a target whose std is not installed.
  The host gate cannot see `cfg`-gated platform code, and this is exactly the
  check that was missing when both non-Windows backends stopped compiling
  (2026-09-08 audit; CI installs both targets so the gate is complete there).
- `tools/check-fork.ps1` -- the vendored egui fork's own fmt/clippy/test. **Not
  redundant:** `cargo clippy --workspace -- -D warnings` only passes those flags to the
  packages cargo selected, and `vendor/egui` is a deliberately excluded separate
  workspace. Without this step the fork's crates sit outside the gate entirely.

Nested inside a release artifact build when run as
`python build.py --host-only --check`.

### Dependency and secrets scanning (`--audit`)

`python build.py --audit` runs the local scanners before any build:
`cargo audit` (RustSec advisories over `Cargo.lock`) and `gitleaks detect`
(working tree + Git history, redacted). A finding fails the command; a missing
scanner is printed as an incomplete-coverage warning rather than silently
passing. It is deliberately **not** part of `--check`: that gate must stay
offline-capable and tool-independent. Run `--audit` before a release, and see
`debug-tools-security-audit.md` for the tool inventory and fallbacks.

## Binary hardening flags

Windows release builds (both `cargo build --release` through
`.cargo/config.toml` and `build.py`, which restates the flags because a set
`RUSTFLAGS` overrides the config) carry:

- `-C control-flow-guard=yes` — CFG instrumentation + guard function table.
- `-C link-arg=/CETCOMPAT` — image is marked CET/hardware-enforced stack
  protection compatible. The OS only enables shadow stacks when every loaded
  module is marked, so the bit is additive; a non-compatible driver keeps the
  process on the non-CET path.
- `--remap-path-prefix` (build.py only, machine-specific) — strips the build
  user's home and checkout root from panic locations.

`strip = "symbols"` keeps shipped binaries free of usable symbols; never ship
an unstripped release. Verify with `dumpbin -headers` / `-loadconfig` (Windows)
and `llvm-readobj --program-headers --dynamic-table` (Linux). The Linux musl
artifact is a static PIE with full RELRO/BIND_NOW and a non-executable stack;
it carries no CET GNU property because musl's own objects are not built with
IBT/SHSTK, and marking the image without instrumenting the code would be
wrong.

## CI

`.github/workflows/ci.yml` runs one Windows job that executes
`python build.py --check`; triggers are push to `main` and `pull_request`.
The gate takes ~16 min on the runner. Pitfall observed 2026-09-08:
feature-branch pushes produced 9–14 s "success" runs that validated
nothing, while the corresponding main-push runs failed — bot-authored
cleanup commits (removed `Subtree::values`, unformatted merges) landed on
main through that blind spot. Never treat a branch-only CI success as
green; merge validation happens on the main push. `cargo fmt` output must
be applied as-is (no rustfmt.toml; defaults, max 100).

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

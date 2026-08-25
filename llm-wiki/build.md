# Build System

Last verified: 2026-08-25

Primary sources:
- `build.py`
- `Cargo.toml` (workspace profiles)
- `AGENTS.md`

## build.py — release driver

`python build.py` (default) does, in order:

1. Host release build (`cargo build --profile release --workspace`).
2. Linux x86_64 release cross-build **by default** — the workspace ships a
   real Linux collector (`crates/tm-platform/src/linux/`). Cross toolchain
   is auto-detected: `cross` first, then `cargo-zigbuild`. Without either,
   the step is skipped with a note (exit code still 0 unless
   `--require-all-targets`).
3. Packaging into `dist/`: Windows → `.zip`, Linux → `.tar.gz`, named
   `taskman-v<version>-<platform>`.

Flags: `--host-only`, `--linux-only`, `--debug`, `--no-package`,
`--require-all-targets`, `--check`.

## Quality gate

`python build.py --check` runs the full gate:
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features`

Nested inside a release artifact build when run as
`python build.py --host-only --check`.

## Profile policy (deliberate, do not regress)

| Setting | Value | Why |
| --- | --- | --- |
| release lto | `"thin"` | fat LTO serialized minutes of linking for ~ms of startup; rejected |
| release codegen-units | default (16) | parallel final codegen; `= 1` was removed for compile speed |
| dev/test debuginfo | `line-tables-only` | full DWARF across 16 parallel rustc processes exploded RAM; line tables keep backtraces usable |
| dev deps opt-level | 2 | wgpu/sysinfo stay usable in dev without per-dep hacks |

Measured on the 16-core dev machine: cold workspace release ≈ 2 min
(dependency-bound: wgpu/eframe), warm incremental dev rebuild of the whole
chain ≈ 4 s. The crate chain tm-core → tm-platform → tm-app is linear, so
cross-crate changes rebuild serially by nature.

## Test isolation

All automated UI/test runs should set
`TASKMAN_DATA_DIR` / `TASKMAN_CONFIG_DIR` to temp dirs so developer data is
never touched. Settings tests additionally use an in-process path override
(`set_default_path_override_for_tests`) guarded by `TEST_OVERRIDE_LOCK` to
avoid env-var races between parallel tests.

# Build System

Last verified: 2026-11-21

Primary sources:
- `build.py`
- `Cargo.toml` (workspace profiles)
- `AGENTS.md`

## build.py — release driver

Host architecture is read once from the active `rustc -vV` host triple, with
the platform machine name as fallback. Empty architecture detection fails
instead of emitting `windows--setup.exe`; Windows ARM64 hardening and cross-build
selection use the same host target. Regression: `python tools/tests/test_build_host.py`.

`python build.py` (default) does, in order:

1. Host release build (`cargo build --profile release --workspace`). On
   Windows this must produce both `taskman.exe` and `taskman-service.exe`.
2. Linux x86_64 release build **by default** — the workspace ships a
   real Linux collector (`crates/tm-platform/src/linux/`). Toolchain
   resolution (`linux_cross_command`, `find_cargo_tool`, `find_zig`): `cross` first,
   then `cargo-zigbuild` paired with `zig` (searching `ZIG`, `PATH`, Python's
   `ziglang` package, and Windows `WinGet`/`Scoop`/`Chocolatey`/Python locations;
   `build_linux` injects `ZIG` and prepends `zig.parent` to `PATH` when invoking
   `cargo zigbuild`; both produce a glibc binary, artifact
   `taskman-v<version>-linux-x86_64`; the zigbuild path is
   pinned to a glibc 2.17 ABI floor for old-LTS compatibility). Without either, the
   self-contained path is used: `x86_64-unknown-linux-musl` std (installed
   through rustup on demand) linked by the bundled `rust-lld` into a static
   PIE, artifact `taskman-v<version>-linux-x86_64-musl`. That path needs no
   compiler, container or zig, so `python build.py` always produces a Linux
   artifact on a rustup machine. **Static musl has no `dlopen`, so this
   artifact is headless/`--selfcheck` only**: winit cannot load Wayland/X11
   and the GUI cannot start on a glibc distro (`tm-app` now prints an explicit
   note if `"Dynamic loading not supported"` is seen at GUI startup). Prefer
   `cross`/`cargo-zigbuild`+`ziglang` for a GUI-capable glibc artifact; `build.py`
   warns when it falls back, and `clean_stale_linux_artifacts()` removes any
   opposite-flavor `linux-x86_64` archive (`.tar.gz` and `.sha256`) from `dist/`.
   It creates an empty `target/cross-stubs/libdl.a`
   because `libloading` emits `-ldl`, which musl folds into libc. The Linux
   step is skipped only when neither path is available (or with
   `--host-only`); `--require-all-targets` makes a skip fatal. Regression:
   `python tools/tests/test_build_linux.py`.
3. Packaging into `dist/`: Windows → `.zip` containing the GUI and service
   plus the self-contained setup installer
   `taskman-v<version>-<platform>-setup.exe` (see `installer.md`); Linux →
   `.tar.gz`, named `taskman-v<version>-<platform>`. The setup artifact is
   built by the same workspace build (`taskman-setup.exe` is linked for the
   artifact's target) and gets its payload appended by the HOST
   `taskman-payload` tool, so `--no-package` also skips it.
   The glibc Linux archive carries `bin/taskman`, `bin/taskman-service` and
   the whole `packaging/linux` tree at the archive root (the installer, the
   unit, the manifest template, the desktop entry, the icon, the AppStream
   metadata and the README), which is what
   `packaging/linux/install-linux.sh` expects to find next to `bin/`. The
   musl archive stays headless: two binaries and the README only.
   The pacman package is part of the same run: `makepkg` runs from
   `packaging/arch` with `PKGDEST` pointed at `dist/`, natively on an Arch
   host or through `wsl.exe` in an Arch WSL distro on Windows
   (`TASKMAN_ARCH_WSL_DISTRO` names one instead of searching). The WSL route
   builds in `/var/tmp/taskman-arch` inside the distro, not in the checkout:
   the 9p mount makes a cold build slow, the checkout may be mounted
   read-only there, and a rebuild then only relinks. It runs
   after checking that `pkgver` matches `Cargo.toml` and that the committed
   `.SRCINFO` matches `makepkg --printsrcinfo`; a host with no `makepkg`
   anywhere is a logged skip, never a failed build, and `--no-arch-package`
   skips it on purpose.

Distribution packages. Besides the pacman package, a glibc Linux build also
yields `taskman-v<version>-debian-ubuntu-<arch>.deb` and
`taskman-v<version>-fedora-<arch>.rpm` (`build_distro_packages()`), written by
`tools/distpkg.py` (pure Python: `build.py` runs on hosts without `dpkg-deb`
and `rpmbuild`; structure covered by `tools/tests/test_distpkg.py`, but a real
`apt`/`dnf` install is NOT verified in CI -- open question). The payload is the
tree `install-linux.sh --packaged --root` stages, so only the Linux host path
works (a Windows host logs a skip). The arch package is renamed to
`taskman-v<version>-arch-x86_64.pkg.tar.zst`. `--no-distro-packages` skips the
deb/rpm; the musl flavor never produces them. Scriptlets of all three call
`install-linux.sh --post-install`, which restarts `taskman-core.service` and then
`restart_gui()` relaunches each running `/usr/bin/taskman` as its owner
(environment and args copied from `/proc/<pid>`). The rpm declares the
dlopen'd libraries as soname capabilities, the deb by package name.

### Windows x86_64 from a Linux/macOS host

A non-Windows host also produces `taskman-v<version>-windows-x86_64.zip` and
`...-windows-x86_64-setup.exe` (`build_windows_cross()`, then the same
`package_setup()` a Windows host uses). Route: `cargo xwin build --target
x86_64-pc-windows-msvc --workspace`, i.e. `cargo-xwin` supplies the Microsoft
CRT/Windows SDK import libraries and links with `lld-link`. **MSVC, not
`x86_64-pc-windows-gnu`, on purpose:** the hardening flags (`-Ccontrol-flow-guard`,
`/CETCOMPAT`) and the installer's security properties (`/DELAYLOAD` +
`delayimp.lib`, the hybrid CRT, `/MANIFESTUAC requireAdministrator`; see
`installer.md`) are MSVC-linker features, so a mingw build would ship a
weaker installer. `app.res` is a committed compiled resource, so no resource
compiler is needed. There is no code signing on either host.

Toolchain resolution (`windows_cross_toolchain()`): the **private toolchain
in `<checkout>/.cross`** first, then an ambient `cargo-xwin` (PATH or
`~/.cargo/bin`, std via `ensure_rust_target`, SDK cache in `$XWIN_CACHE_DIR` or
`~/.cache/cargo-xwin`). A missing piece is a logged skip naming the fix, like a
missing `makepkg`, and fatal only under `--require-all-targets`. The driver never
installs a toolchain or downloads the Microsoft CRT/SDK (about 1 GB, Microsoft's
license terms); the build's own crate downloads go to `.cross/cargo-home`.

**One-time setup: `python tools/setup-windows-cross.py
--accept-microsoft-license`.** It installs, below `.cross/` (gitignored; the
system partition, `~/.cargo` and `~/.rustup` are untouched): a private rustup
with the official toolchain whose version equals the host `rustc` plus the
`x86_64-pc-windows-msvc` std, `cargo-xwin`, and the x86_64 CRT/SDK cache
(`.cross/xwin/xwin/DONE` marks completion). About 1.6 GB. Why a private rustup
and not the distribution's `rust` plus the official `rust-std` tarball: a
distribution compiler stamps its own version string
(`rustc 1.100.0 (...) (Arch Linux rust 1:1.100.0-1)`) into crate metadata, so
the official std is rejected as "compiled by an incompatible version of rustc"
(tried 2026-11-26; forcing the version string did not make it load), and
`/usr/lib/rustlib` is root-owned anyway. The private route uses the official
compiler for the Windows artifacts, with its own cargo target directory
(`target/windows-cross`) so the two compilers never invalidate each other's
host build scripts and proc macros. `XWIN_ARCH=x86_64` is pinned: the default
also demands aarch64 and would re-download the manifest every run.

Verified 2026-11-26 on Arch (no rustup, system rust 1.100.0): cold build about
3 min; all three binaries are PE32+ x86-64 with CFG guard flags and
CETCOMPAT; `taskman-setup.exe` has a `requireAdministrator` manifest, the
icon resources, the eight DLLs of `DELAY_LOADED` as delay imports and no
`vcruntime140.dll` import (hybrid CRT), and `taskman-payload verify` passes on
the finished installer. Not run: the installer itself under wine (wine aborts
inside the agent sandbox) and `tests/import_table.rs`, which needs Windows.

The payload tool and the staging file always come from the HOST build
directory (`target/release/taskman-payload`, runnable on the build machine); only
`taskman-setup.exe` and the two app binaries come from the cross target
directory (`target/windows-cross/x86_64-pc-windows-msvc/<profile>/` with the
private toolchain). `--host-only`, `--linux-only` and
`--no-windows` leave the step out; `--all-targets` does not add Windows ARM64
on a non-Windows host. Regression: `python tools/tests/test_build_windows.py`.

`cross_check()` type-checks every non-host target with `--all-targets` on
purpose: without it, `#[cfg(test)]` code behind `cfg(target_os = "linux")`
is compiled by nothing on this host, and a broken Linux test would only
surface on a Linux machine. It still skips a target whose std is not
installed (CI installs them), and it never links.

The glibc Linux artifact needs `cross` or `cargo-zigbuild`+`zig`; without
either, `build.py` produces only the headless musl fallback. That fallback no
longer deletes a GUI-capable gnu archive already in `dist/`: the host cannot
rebuild it, so removing it would downgrade the release directory. On a Linux
host the same artifact is a plain native build:
`packaging/linux/build-artifact.sh [--verify]` writes the identical layout
(binaries, installer, unit, manifest template, desktop entry, icon, AppStream,
README) and refuses a static target or a statically linked GUI. `makepkg` in
`packaging/arch` is the other GUI-capable path.

CI has three jobs (`.github/workflows/ci.yml`): the Windows `build.py --check`
gate, a Linux job (fmt, clippy `-D warnings` for the Linux code, the full test
suite including the Linux-only modules, `bash -n` + `shellcheck -S warning` on
the scripts, the python build tests, and `build-artifact.sh --verify`), and an
`archlinux:latest` job that diffs `makepkg --printsrcinfo` against `.SRCINFO`
and validates the unit and desktop entry. The Linux jobs exist because the
Windows gate can type-check that code but can never run it: a packaged service
that refused to start, and two Linux-only test fixtures, all survived the
Windows-only gate.

Flags: `--host-only`, `--linux-only`, `--all-targets`, `--debug`,
`--no-package`, `--no-windows`, `--require-all-targets`, `--check`, `--audit`,
`--live-tests`.

`--all-targets` adds two cross-built artifacts:

- **Windows ARM64** (`aarch64-pc-windows-msvc`, Windows host only): needs the
  Rust target std and the MSVC ARM64 toolset
  (`Microsoft.VisualStudio.Component.VC.Tools.ARM64`) plus the Windows SDK
  arm64 libs. `/CETCOMPAT` is x86_64-only (ARM64's linker rejects it with
  LNK1246), so the ARM64 build gets Control Flow Guard without the CET marker.
- **Linux ARM64** (`aarch64-unknown-linux-gnu`): same cross/zigbuild
  resolution as x86_64 (glibc 2.17 floor). There is no rust-lld/musl fallback
  for ARM64; without `cross`/`cargo-zigbuild` the artifact is skipped (fatal
  with `--require-all-targets`).

## Publishing a release

1. `main` must be green: `python build.py --check` locally (the CI workflow
   runs the same command) plus `python build.py --audit` for the advisory and
   secrets scan. Without `--host-only`, `--check` also builds both artifacts.
2. Bump `[workspace.package] version` in `Cargo.toml`, run any cargo command
   so `Cargo.lock` follows, and commit as
   `chore(release): bump version to X.Y.Z`. Push `main`; the tag points at
   this commit.
3. `python build.py --all-targets` produces
   `dist/taskman-vX.Y.Z-windows-x86_64.zip`,
   `dist/taskman-vX.Y.Z-windows-arm64.zip`,
   `dist/taskman-vX.Y.Z-linux-x86_64[-musl].tar.gz` and
   `dist/taskman-vX.Y.Z-linux-arm64.tar.gz`, plus the Windows setup
   installers `dist/taskman-vX.Y.Z-windows-x86_64-setup.exe` and
   `dist/taskman-vX.Y.Z-windows-arm64-setup.exe`. The same run writes the
   `.pkg.tar.zst` (`-arch-`), `.deb` (`-debian-ubuntu-`) and `.rpm`
   (`-fedora-`) packages into `dist/`; publish them too. Write
   `<sha256>  <filename>` next to each archive, installer and package.
4. Publish with the GitHub CLI:
   `gh release create vX.Y.Z --target <bump-sha> --title 'TaskMan vX.Y.Z'
   --notes-file <notes> --latest <archives + .sha256 files>`. This creates
   the remote tag on the bump commit.
5. Verify the PUBLISHED archives, not only `target/release`: the Windows zip
   must contain `taskman.exe` + `taskman-service.exe`, and both binaries
   should pass `--selfcheck` (`--mock` for the Windows GUI; the static Linux
   musl binary also runs the real backend under WSL).

Earlier releases (v0.1.0/v0.1.1) were published by a temporary workflow
pushed to a throwaway `release/vX.Y.Z-*` branch; GitHub keeps those workflows
listed after the branch and file are deleted. The local `gh release create`
path above is simpler and directly verifiable.

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
- `tools/check-fork.ps1` -- the vendored egui fork's own fmt/clippy/test (`probe_powershell()` tries `pwsh` then `powershell` with a 10 s timeout, falls back cleanly from an inaccessible WindowsApps stub, and makes the gate fail if a shell exists but cannot run). A host with no PowerShell at all (a normal Linux box) runs the same steps from Python (`fork_gate_commands()`; `tools/tests/test_build_fork.py` keeps its package list and eframe feature set in sync with the script) — it used to skip, leaving both forks ungated on the hosts whose Wayland/X11 paths they carry. **Not
  redundant:** `cargo clippy --workspace -- -D warnings` only passes those flags to the
  packages cargo selected, and `vendor/egui` is a deliberately excluded separate
  workspace. Without this step the fork's crates sit outside the gate entirely.
  It lints `--all-targets`, so a vendored fork's examples must build too —
  including their image assets. The repo-wide `*.png` ignore is scoped with
  `!vendor/**/*.png` for exactly that reason; an untracked vendored asset is
  invisible locally and only fails on a fresh CI checkout.

Nested inside a release artifact build when run as
`python build.py --host-only --check`.

### Python build tests (`tools/tests/test_*.py`)

Every file loads the driver by path and executes it without registering it in
`sys.modules`:

```python
spec = importlib.util.spec_from_file_location("taskman_build", root / "build.py")
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)
```

That keeps `build.py` importable with no package and no side effects, but it
also makes the module invisible to `sys.modules` while it executes. Python
3.14 reads `sys.modules[cls.__module__]` inside `@dataclass`, so a dataclass at
module level raises `AttributeError: 'NoneType' object has no attribute
'__dict__'` there and takes every test file down with it — Arch ships 3.14, and
that is how it was found. Use a `NamedTuple` or a plain class for the small
value types in `build.py`; `ArchPackageHost` is a `NamedTuple` for that reason,
not for style.

### Live-kernel tests (`--live-tests`)

`python build.py --live-tests` runs the `#[ignore]`d tests in `tm-platform`
and exits. Windows only, and it REFUSES to run unelevated rather than
reporting a pass it did not earn.

These are the only checks that prove the hand-written NT structure offsets and
ETW payload decoders against the thing they decode — the
`SystemMemoryListInformation` layout behind the memory-composition bar, and
the `SystemIoProviderGuid` / `Kernel-Network` attribution behind the
per-process rate columns. Everything else in the workspace feeds those
decoders synthetic records, so a Windows build that moves a field passes
`--check` and ships a column that is quietly wrong.

Deliberately NOT part of `--check`: they need an elevated token, real disk and
network traffic, and they spawn and kill processes, so they can run neither in
CI nor on a developer machine by default. Run them before cutting a release
and after any Windows feature update that touches the process, memory or
storage stacks. See `known-debt.md` § Per-process disk active time for what is
still unproven on live events.

### Dependency and secrets scanning (`--audit`)

`python build.py --audit` runs the local scanners before any build:
`cargo audit` (RustSec advisories over `Cargo.lock`) and `gitleaks detect`
(working tree + Git history, redacted). A finding fails the command; a missing
scanner is printed as an incomplete-coverage warning rather than silently
passing. It is deliberately **not** part of `--check`: that gate must stay
offline-capable and tool-independent. Run `--audit` before a release, and see
`debug-tools-security-audit.md` for the tool inventory and fallbacks.

## Building where cargo cannot reach crates.io (agent sandbox)

Symptom, seen in the agent sandbox: every `cargo` invocation that has to resolve
a dependency stops at `Updating crates.io index` with
`schannel: AcquireCredentialsHandle failed: SEC_E_NO_CREDENTIALS`, and
`--offline` then fails selection for any crate the local registry lacks
(concretely `resvg 0.49`, which `tm-platform` added for SVG icons — a fresh
checkout of this tree cannot build offline without it). The cause is the
sandbox's TLS block: cargo on Windows uses Schannel, which the sandbox denies.
Python's HTTPS and WSL's own TLS stack are not affected, so there are two
working routes.

**Linux: run it in WSL, which has real crates.io access.** This is the better
route for anything touching the Linux backend, and it is how the Linux half is
verified without a second machine:

```
wsl.exe -d <distro> -- bash -lc 'cd /mnt/c/<checkout> && cargo test -p tm-platform -p tm-app'
```

A copy inside the distro (`rsync -a --exclude target --exclude .git /mnt/c/<checkout>/ ~/tm-verify/`)
builds faster than running cargo across the 9p mount.

**Windows: `python tools/offline-crates.py -- <command>`.** It diffs `Cargo.lock`
against `~/.cargo/registry/src`, downloads the missing `.crate` files with
Python (verifying the sha256 the lock records), unpacks them under
`target/agent-crates/`, adds a temporary `[patch.crates-io]` block, runs the
command, and restores `Cargo.toml` and `Cargo.lock` byte for byte on every exit
path — including Ctrl-C. Nothing it writes is committable, and the crates are the
same bytes a normal build would download. Use a plain `cargo`/`build.py` run
wherever crates.io is reachable; this exists for the sandbox only.

Both routes work: the release artifact builds, and the resolver benchmark below
ran inside WSL against the host's real theme tree.

## Binary hardening flags

Windows release builds (both `cargo build --release` through
`.cargo/config.toml` and `build.py`, which restates the flags because a set
`RUSTFLAGS` overrides the config) carry:

- `-C control-flow-guard=yes` — CFG instrumentation + guard function table.
  Applied to x86_64 and ARM64 Windows.
- `-C link-arg=/CETCOMPAT` — image is marked CET/hardware-enforced stack
  protection compatible. **x86_64 only**; ARM64's linker rejects it
  (LNK1246) and ARM64 has no CET shadow-stack marker. The OS only enables
  shadow stacks when every loaded module is marked, so the bit is additive; a
  non-compatible driver keeps the process on the non-CET path.
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

**CI clippy is stable, the dev box is nightly, and their lint sets differ
in both directions.** `dtolnay/rust-toolchain@stable` gave 1.98.0 on
2026-09-11 while the local default is nightly 1.99; a `for i in 0..N` loop
in `normalize_heat` (`tabs/processes.rs`) passed nightly clippy and failed
CI with `needless_range_loop`. `build.py --check` uses whatever the default
toolchain is, so it cannot see this class of failure. Reproduce CI's clippy
exactly with

```bash
cargo +stable clippy --workspace --all-targets --all-features --target-dir target/clippy-stable -- -D warnings
```

The separate `--target-dir` is what keeps the two toolchains from
invalidating each other's artifacts on every run (it costs ~800 MB and a
~90 s warm pass). Converging the toolchains instead is not free: the fork
gate deliberately skips `cargo fmt` on `vendor/winit` *because* the local
rustfmt is nightly, so pinning the repo to stable would move that problem
rather than remove it.

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

The packaged GUI defaults to native CPU rendering. The Windows Graphics
setting offers CPU or D3D12, and Linux offers CPU or Vulkan. A GPU startup
failure falls back to CPU. OpenGL remains a diagnostic and final fallback,
and macOS can also select Metal or OpenGL. Preferences apply at the next
launch. Legacy auto/software preferences map to CPU, and the old compatibility
preference maps to the Windows/Linux default.

CPU rendering requires a working native display connection and software
presentation surface. It has no GPU adapter or shader compiler dependency.
Linux desktop launches require the glibc build. The static musl build supports
headless checks. WGPU accepts only surface-compatible GPUs and defaults to FIFO
presentation and a low-power adapter unless `WGPU_POWER_PREF` overrides it. This trims unused
backend dependency/code without making older or unusual graphics systems a
hard failure. The first 2026-08-31 backend-trim build measured 13,470,208
bytes. The final service/tray/reliability build is 13,873,664 bytes versus the
14,656,000-byte pre-pass baseline (782,336 bytes / 5.34% smaller) while adding
the new features. The separate always-running service is 1,378,816 bytes. See
`debug-tools.md` for overrides.

The WGPU device requests `MemoryHints::MemoryUsage`, because this 2D monitor UI has
small, infrequent uploads and does not benefit from wgpu's performance-sized
suballocation blocks. D3D12's `max_non_sampler_bindings` is capped at 8,192,
which still leaves ample room for the bounded icon cache plus custom UI bind
groups but avoids the default million-entry visible descriptor heap. Hidden
windows release the device and swapchain entirely rather than keeping this
state resident at a lower frame rate.

The service has no GUI/eframe dependency. Keeping it in a separate binary
prevents an always-running renderer, GPU stack, or broad GUI parser from
becoming part of the LocalSystem attack surface.

## Test isolation

All automated UI/test runs should set
`TASKMAN_DATA_DIR` / `TASKMAN_CONFIG_DIR` to temp dirs so developer data is
never touched. Settings tests additionally use an in-process path override
(`set_default_path_override_for_tests`) guarded by `TEST_OVERRIDE_LOCK` to
avoid env-var races between parallel tests.

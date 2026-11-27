#!/usr/bin/env python3
"""Release build driver for taskman.

Default behavior (`python build.py`): build the host and Linux x86_64 release
(where a cross toolchain is available), then package platform artifacts.
`--all-targets` additionally builds Windows ARM64 and Linux ARM64.

Windows builds also produce the self-contained setup installer
(`dist/taskman-v<version>-windows-<arch>-setup.exe`): a single 100%-Rust
`taskman-setup.exe` with the application payload embedded and SHA-256
verified at install time. A Linux or macOS host produces the same Windows
x86_64 zip and installer by cross-compiling `x86_64-pc-windows-msvc` with
`cargo-xwin`; a host without that toolchain logs a skip (fatal with
`--require-all-targets`), `--host-only` and `--no-windows` leave it out.

Linux builds produce a self-contained install tarball: `bin/taskman`,
`bin/taskman-service` and the `packaging/linux` tree (installer, systemd unit,
manifest template, desktop entry, icon, AppStream metadata), so the archive is
installable by extracting it and running `install-linux.sh`.

Every packaging run also builds the pacman package
(`dist/taskman-v<version>-arch-x86_64.pkg.tar.zst`) from
`packaging/arch`. `makepkg` runs natively on an Arch host and through `wsl.exe`
in an Arch WSL distro on Windows, where pacman is not installed at all; a host
with neither is a documented skip, never a failure. `--no-arch-package` skips
it on purpose.

Linux builds with a glibc binary also produce a Debian/Ubuntu package
(`taskman-v<version>-debian-ubuntu-<arch>.deb`) and a Fedora package
(`taskman-v<version>-fedora-<arch>.rpm`), written by `tools/distpkg.py` from
the payload `install-linux.sh --packaged` stages. `--no-distro-packages` skips
them. Their scriptlets run `install-linux.sh --post-install`, which restarts
the core service and relaunches any running GUI.
"""

from __future__ import annotations

import argparse
import difflib
import importlib.util
import os
import platform
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from functools import cache
from pathlib import Path
from typing import NamedTuple

ROOT = Path(__file__).resolve().parent
DIST = ROOT / "dist"
LINUX_TARGET = "x86_64-unknown-linux-gnu"
LINUX_ARM64_TARGET = "aarch64-unknown-linux-gnu"
WINDOWS_ARM64_TARGET = "aarch64-pc-windows-msvc"
# The Windows x86_64 release a non-Windows host cross-builds (see
# `build_windows_cross`). MSVC, not gnu: the installer's security properties
# (delay-loaded DLLs, hybrid CRT, requireAdministrator manifest) and the
# hardening flags are MSVC-linker features that a mingw build cannot carry.
WINDOWS_X64_TARGET = "x86_64-pc-windows-msvc"
# Self-contained fallback: no cross compiler, Docker or zig, only the official
# rustup std component plus the bundled rust-lld linker.
LINUX_MUSL_TARGET = "x86_64-unknown-linux-musl"
MUSL_STUB_DIR = ROOT / "target" / "cross-stubs"
LINUX_DESKTOP = ROOT / "packaging" / "linux" / "io.github.aufkrawall.Taskman.desktop"
# The Linux payload. `packaging/linux/install-linux.sh` owns the destination of
# every file below and the two binary paths, so the tarball only has to
# reproduce the layout that script expects to find next to itself — the same
# directory the Arch PKGBUILD stages from.
LINUX_PACKAGING = ROOT / "packaging" / "linux"
ARCH_PACKAGING = ROOT / "packaging" / "arch"
LINUX_SERVICE_BINARY = "taskman-service"
LINUX_PAYLOAD_FILES = (
    "install-linux.sh",
    "uninstall-linux.sh",
    "taskman-core.service",
    "core-service.json.in",
    "io.github.aufkrawall.Taskman.desktop",
    "io.github.aufkrawall.Taskman.svg",
    "io.github.aufkrawall.Taskman.metainfo.xml",
    "README.md",
)
# The static musl fallback is headless-only: no `dlopen`, so winit cannot load
# Wayland/X11 and the GUI does not start on a glibc desktop. An installer, a
# desktop entry and a unit next to that binary would promise an install the
# artifact cannot deliver, so it carries the binaries and the README only.
LINUX_MUSL_PAYLOAD_FILES = ("README.md",)
# Tar entries carry a mode, and the extracted file keeps it. Data files are
# 0644; binaries and scripts are 0755. Windows has no POSIX modes, so this is
# the only place the distinction can be made.
TEXT_ARTIFACT_SUFFIXES = (".desktop", ".service", ".svg", ".xml", ".md", ".in")


def log(msg: str) -> None:
    print(f"[build] {msg}", flush=True)


def run(cmd: list[str], env: dict | None = None, cwd: Path | None = None) -> bool:
    print(f"$ {' '.join(cmd)}", flush=True)
    merged = os.environ.copy()
    if env:
        merged.update(env)
    proc = subprocess.run(cmd, cwd=cwd or ROOT, env=merged)
    return proc.returncode == 0


def cargo() -> str:
    exe = "cargo.exe" if platform.system() == "Windows" else "cargo"
    if shutil.which(exe):
        return exe
    fallback = Path.home() / ".cargo" / "bin" / exe
    if fallback.exists():
        return str(fallback)
    raise SystemExit("cargo not found - install Rust 1.88+ or add ~/.cargo/bin to PATH")


def release_flag_list(windows_target: bool, arm64: bool = False) -> list[str]:
    """Hardening rustflags for release artifacts (security audit F-11-001).

    * ``--remap-path-prefix`` strips the build machine's home directory AND
      the checkout root from panic location strings and generated-binding
      ``file!()`` text, so shipped binaries do not leak the local user name or
      the developer's directory layout (release profile strips symbols but
      keeps panic locations). The root remap must be a longer match than the
      home one; rustc applies the longest matching prefix.
    * Control Flow Guard is restated here because a set RUSTFLAGS variable
      overrides the ``.cargo/config.toml`` rustflags entirely; without it the
      packaged build would silently lose the CFG instrumentation that plain
      ``cargo build --release`` gets from config.

    Applied only to release-profile builds so dev iteration stays untouched.
    """
    flags: list[str] = []
    home = Path.home()
    if home.is_dir():
        native = str(home)
        flags.append(f"--remap-path-prefix={native}=")
        if windows_target:
            forward = native.replace("\\", "/")
            if forward != native:
                flags.append(f"--remap-path-prefix={forward}=")
    root = str(ROOT)
    if root and root != str(home):
        flags.append(f"--remap-path-prefix={root}=taskman")
        if windows_target:
            forward_root = root.replace("\\", "/")
            if forward_root != root:
                flags.append(f"--remap-path-prefix={forward_root}=taskman")
    if windows_target:
        flags.append("-Ccontrol-flow-guard=yes")
        # Mark the image CET/hardware-enforced-stack-protection compatible.
        # The OS only enables shadow stacks when every loaded module is
        # marked, so this is additive: a non-compatible driver simply keeps
        # the process on the non-CET path. LocalSystem's broker benefits most.
        # x86_64 only: ARM64's linker rejects the flag (LNK1246).
        if not arm64:
            flags.append("-Clink-arg=/CETCOMPAT")
    return flags


def release_rustflags(
    windows_target: bool, arm64: bool = False
) -> dict[str, str] | None:
    flags = release_flag_list(windows_target, arm64)
    return {"RUSTFLAGS": " ".join(flags)} if flags else None


def read_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', text, re.M)
    if not m:
        raise SystemExit("cannot read workspace version from Cargo.toml")
    return m.group(1)


def have(tool: str) -> bool:
    return shutil.which(tool) is not None


def find_cargo_tool(name: str) -> str | None:
    """Resolve a cargo subcommand binary on PATH or in ``~/.cargo/bin``."""
    found = shutil.which(name)
    if found:
        return found
    exe = f"{name}.exe" if platform.system() == "Windows" else name
    fallback = Path.home() / ".cargo" / "bin" / exe
    return str(fallback) if fallback.is_file() else None


def find_zig() -> Path | None:
    """Locate ``zig`` on PATH, via the ``ziglang`` Python package, or Windows defaults."""
    env_zig = os.environ.get("ZIG", "").strip()
    if env_zig:
        candidate = Path(env_zig)
        if candidate.is_file():
            return candidate

    found = shutil.which("zig")
    if found:
        return Path(found)

    exe_name = "zig.exe" if platform.system() == "Windows" else "zig"
    try:
        spec = importlib.util.find_spec("ziglang")
    except (ImportError, ValueError):
        spec = None
    if spec is not None:
        search_dirs: list[Path] = []
        if spec.origin:
            search_dirs.append(Path(spec.origin).resolve().parent)
        if spec.submodule_search_locations:
            search_dirs.extend(Path(loc) for loc in spec.submodule_search_locations)
        for directory in search_dirs:
            candidate = directory / exe_name
            if candidate.is_file():
                return candidate

    if platform.system() == "Windows":
        local_app_data = Path(
            os.environ.get("LOCALAPPDATA") or (Path.home() / "AppData" / "Local")
        )
        app_data = Path(os.environ.get("APPDATA") or (Path.home() / "AppData" / "Roaming"))
        direct_candidates = [
            local_app_data / "Microsoft" / "WinGet" / "Links" / exe_name,
            Path.home() / "scoop" / "shims" / exe_name,
            Path.home() / "scoop" / "apps" / "zig" / "current" / exe_name,
            Path(os.environ.get("ProgramData") or r"C:\ProgramData")
            / "chocolatey"
            / "bin"
            / exe_name,
        ]
        for candidate in direct_candidates:
            try:
                if candidate.is_file():
                    return candidate
            except OSError:
                continue

        glob_roots: list[tuple[Path, str]] = [
            (local_app_data / "Microsoft" / "WinGet" / "Packages", f"zig.zig*/**/{exe_name}"),
            (app_data / "Python", f"Python*/site-packages/ziglang/{exe_name}"),
            (
                local_app_data / "Programs" / "Python",
                f"Python*/Lib/site-packages/ziglang/{exe_name}",
            ),
            (
                Path(os.environ.get("ProgramFiles") or r"C:\Program Files"),
                f"Python*/Lib/site-packages/ziglang/{exe_name}",
            ),
        ]
        for base, pattern in glob_roots:
            try:
                if not base.is_dir():
                    continue
                for candidate in sorted(base.glob(pattern), reverse=True):
                    if candidate.is_file():
                        return candidate
            except OSError:
                continue

    return None


# Non-host backends the project claims to support (README, AGENTS.md). The
# host gate cannot type-check cfg-gated platform code, so these are checked
# separately when their standard library is installed. The ARM64 entries
# mirror the release artifacts built by --all-targets.
CROSS_CHECK_TARGETS = [
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-pc-windows-msvc",
    "aarch64-apple-darwin",
]

# Renderer features that must each build on their own. The default build
# enables all three, so a feature-specific cfg mistake (the software-only
# eframe inspection fallback was one) stays invisible to it.
RENDERER_FEATURE_CHECKS = ["software", "wgpu", "glow"]


@cache
def host_target() -> str:
    """Use the active Rust compiler, including under a restricted environment."""
    rustc = shutil.which("rustc")
    if rustc is None:
        exe = "rustc.exe" if platform.system() == "Windows" else "rustc"
        fallback = Path.home() / ".cargo" / "bin" / exe
        if fallback.is_file():
            rustc = str(fallback)
    if rustc:
        try:
            proc = subprocess.run([rustc, "-vV"], capture_output=True, text=True, cwd=ROOT)
        except OSError:
            proc = None
        if proc is not None and proc.returncode == 0:
            for line in proc.stdout.splitlines():
                if line.startswith("host: "):
                    target = line.removeprefix("host: ").strip()
                    if re.fullmatch(r"[a-zA-Z0-9_]+(?:-[a-zA-Z0-9_]+)+", target):
                        return target
    system = platform.system()
    machine = platform.machine().lower()
    arch = {
        "amd64": "x86_64",
        "x86_64": "x86_64",
        "arm64": "aarch64",
        "aarch64": "aarch64",
    }.get(machine, machine)
    if not arch:
        raise SystemExit("cannot determine the Rust host architecture; check rustc -vV")
    if system == "Windows":
        return f"{arch}-pc-windows-msvc"
    if system == "Darwin":
        return f"{arch}-apple-darwin"
    return f"{arch}-unknown-linux-gnu"


def find_rustc() -> str | None:
    rustc = shutil.which("rustc")
    if rustc is None:
        exe = "rustc.exe" if platform.system() == "Windows" else "rustc"
        fallback = Path.home() / ".cargo" / "bin" / exe
        rustc = str(fallback) if fallback.is_file() else None
    return rustc


def sysroot_rust_targets() -> set[str]:
    """Targets whose std sits in the active sysroot, for a toolchain without rustup.

    A distribution's own `rust` package (Arch ships one) has no rustup to list
    from, but `rustc` finds a target's std at `<sysroot>/lib/rustlib/<target>/lib`
    all the same, so that is what decides whether a cross target can build.
    """
    rustc = find_rustc()
    if rustc is None:
        return set()
    try:
        proc = subprocess.run(
            [rustc, "--print", "sysroot"], capture_output=True, text=True, cwd=ROOT
        )
    except OSError:
        return set()
    if proc.returncode != 0 or not proc.stdout.strip():
        return set()
    rustlib = Path(proc.stdout.strip()) / "lib" / "rustlib"
    try:
        return {
            lib.parent.name
            for lib in rustlib.glob("*/lib")
            if any(lib.glob("libstd-*.rlib"))
        }
    except OSError:
        return set()


def installed_rust_targets() -> set[str]:
    """Targets whose std is installed (rustup's list, else the sysroot's)."""
    rustup = shutil.which("rustup")
    if rustup is None:
        return sysroot_rust_targets()
    proc = subprocess.run(
        [rustup, "target", "list", "--installed"],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        return set()
    return {line.strip() for line in proc.stdout.splitlines() if line.strip()}


def cross_check() -> bool:
    """Type-check every non-host backend the project documents as supported.

    `cargo check` never links, so it needs only the target's std -- and it is
    exactly what catches a cfg-gating mistake that the host build cannot see
    (the 2026-09 Linux/macOS build breakage). Skipped per target when its std
    is missing; CI installs both targets so the gate is complete there.

    `--all-targets` matters as much as the target itself: without it cargo
    builds no `#[cfg(test)]` code, so the unit tests of a Linux-only module are
    compiled NOWHERE -- not by the host gate (the module is cfg-gated out) and
    not here. That is how a broken Linux test stays invisible until someone
    builds on a Linux box.
    """
    installed = installed_rust_targets()
    ok = True
    for target in CROSS_CHECK_TARGETS:
        if target == host_target():
            continue  # already covered by clippy/tests
        if target not in installed:
            log(f"target {target} not installed - skipping cross check")
            continue
        if not run(
            [cargo(), "check", "--workspace", "--all-targets", "--target", target]
        ):
            ok = False
    return ok


def renderer_feature_check() -> bool:
    """Type-check every renderer feature in isolation (not just all at once)."""
    ok = True
    for feature in RENDERER_FEATURE_CHECKS:
        if not run(
            [
                cargo(),
                "check",
                "-p",
                "tm-app",
                "--no-default-features",
                "--features",
                feature,
            ]
        ):
            ok = False
    return ok


def linux_cross_command(profile: str, target: str = LINUX_TARGET) -> tuple[list[str], str] | None:
    cross = find_cargo_tool("cross")
    if cross is not None:
        return (
            [cross, "build", "--profile", profile, "--target", target],
            str(ROOT / "target" / target / profile),
        )
    if find_cargo_tool("cargo-zigbuild") is not None and find_zig() is not None:
        # Pin the glibc ABI floor. cargo-zigbuild's default tracks the host's
        # glibc (2.30+ here), which excludes older LTS distros; 2.17 is the
        # oldest glibc zig ships stubs for and what Rust's own gnu std targets.
        return (
            [
                cargo(),
                "zigbuild",
                "--profile",
                profile,
                "--target",
                f"{target}.2.17",
            ],
            str(ROOT / "target" / target / profile),
        )
    return None


def ensure_musl_std() -> bool:
    """Make sure the musl standard library is installed (rustup only)."""
    if LINUX_MUSL_TARGET in installed_rust_targets():
        return True
    rustup = shutil.which("rustup")
    if rustup is None:
        return False
    log(f"installing {LINUX_MUSL_TARGET} standard library via rustup (one-time)")
    return run([rustup, "target", "add", LINUX_MUSL_TARGET])


def prepare_musl_link_stub() -> Path:
    """Create the empty ``libdl.a`` the self-contained musl link needs.

    musl folds dlopen/dlclose into libc, but ``libloading`` (a wgpu/glow
    dependency) still emits ``-ldl`` for every Linux target. The linker only
    needs an archive of that name to exist; an empty ar archive satisfies the
    lookup without defining or shadowing any symbol.
    """
    MUSL_STUB_DIR.mkdir(parents=True, exist_ok=True)
    stub = MUSL_STUB_DIR / "libdl.a"
    if not stub.exists():
        stub.write_bytes(b"!<arch>\n")
    return MUSL_STUB_DIR


def build_linux_musl(profile: str) -> tuple[Path | None, bool]:
    """Self-contained static musl build using the bundled rust-lld linker.

    This needs no cross compiler, container or zig -- only the official rustup
    std component -- so `python build.py` can always produce a Linux x86_64
    artifact. The result is a static PIE that runs on any Linux x86_64.
    """
    if not ensure_musl_std():
        log(
            f"{LINUX_MUSL_TARGET} not installed and rustup unavailable - "
            "skipping linux artifact"
        )
        return None, False
    stub = prepare_musl_link_stub()
    flags = release_flag_list(False) if profile != "dev" else []
    flags += [
        "-C link-self-contained=yes",
        # Relative on purpose: the linker inherits cargo's cwd (the repo root),
        # so a checkout path with spaces cannot break the flag.
        f"-C link-arg=-L{stub.relative_to(ROOT).as_posix()}",
    ]
    env = {
        f"CARGO_TARGET_{LINUX_MUSL_TARGET.upper().replace('-', '_')}_LINKER": "rust-lld",
        f"CARGO_TARGET_{LINUX_MUSL_TARGET.upper().replace('-', '_')}_RUSTFLAGS": " ".join(flags),
    }
    if not run(
        [cargo(), "build", "--profile", profile, "--target", LINUX_MUSL_TARGET],
        env=env,
    ):
        return None, True
    out_dir = ROOT / "target" / LINUX_MUSL_TARGET / profile
    exe = out_dir / "taskman"
    if not exe.exists():
        log(f"linux musl binary missing after build: {exe}")
        return None, True
    log(
        "warning: static musl has no dlopen, so winit cannot load Wayland/X11 "
        "and the GUI cannot start on a glibc distro - this artifact is "
        "headless/--selfcheck only; install cross or cargo-zigbuild for a "
        "GUI-capable glibc artifact"
    )
    return exe, True


def ensure_rust_target(target: str) -> bool:
    """Make sure a target's std is installed, through rustup when there is one."""
    if target in installed_rust_targets():
        return True
    rustup = shutil.which("rustup")
    if rustup is None:
        return False
    log(f"installing {target} standard library via rustup (one-time)")
    return run([rustup, "target", "add", target]) and target in installed_rust_targets()


# The private toolchain `tools/setup-windows-cross.py` installs: its own rustup
# (the official toolchain matching the host `rustc`, plus the Windows std),
# cargo-xwin and the Microsoft CRT/SDK cache, all below the checkout so the
# system partition is untouched. Its layout is defined by that script.
CROSS_DIR = ROOT / ".cross"
CROSS_SETUP = "tools/setup-windows-cross.py"
# The private toolchain is a different compiler build from a distribution's
# `rustc`, so it gets its own target directory instead of invalidating the host
# build's artifacts (build scripts, proc macros) on every alternation.
CROSS_TARGET_DIR = ROOT / "target" / "windows-cross"


class WindowsCross(NamedTuple):
    """A usable Windows cross toolchain: the cargo-xwin to run and its environment.

    `target_dir` is where cargo writes `<target>/<profile>`; `env` is merged
    under the per-call environment (private rustup/cargo homes, SDK cache).
    """

    xwin: str
    env: dict[str, str]
    target_dir: Path
    label: str


def xwin_cache_dir() -> Path:
    """Where an ambient `cargo-xwin` keeps the Microsoft CRT and Windows SDK."""
    configured = os.environ.get("XWIN_CACHE_DIR", "").strip()
    if configured:
        return Path(configured)
    if platform.system() == "Darwin":
        base = Path.home() / "Library" / "Caches"
    else:
        base = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
    return base / "cargo-xwin"


def xwin_sdk_cached(cache_dir: Path) -> bool:
    """True when `cargo-xwin` already holds the x86_64 CRT and SDK in `cache_dir`.

    `cargo-xwin` downloads them from Microsoft on first use. That download is
    large and carries Microsoft's license terms, so the driver never starts it
    on its own: it is a one-time step the user takes, and until then the
    Windows build is a logged skip. The `DONE` marker is the one `cargo-xwin`
    itself writes after a complete download; its first line lists the
    architectures present.
    """
    try:
        lines = (cache_dir / "xwin" / "DONE").read_text(encoding="utf-8").splitlines()
    except OSError:
        return False
    return bool(lines) and "x86_64" in lines[0].split()


def private_windows_cross() -> WindowsCross | None:
    """The `.cross` toolchain, when `tools/setup-windows-cross.py` has completed."""
    cargo_home = CROSS_DIR / "cargo-home"
    rustup_home = CROSS_DIR / "rustup"
    xwin = cargo_home / "bin" / "cargo-xwin"
    if not xwin.is_file() or not xwin_sdk_cached(CROSS_DIR / "xwin"):
        return None
    toolchain = next(
        (
            lib.parents[3].name
            for lib in sorted(
                (rustup_home / "toolchains").glob(f"*/lib/rustlib/{WINDOWS_X64_TARGET}/lib")
            )
            if any(lib.glob("libstd-*.rlib"))
        ),
        None,
    )
    if toolchain is None:
        return None
    path = os.environ.get("PATH", "")
    env = {
        "RUSTUP_HOME": str(rustup_home),
        "CARGO_HOME": str(cargo_home),
        "RUSTUP_TOOLCHAIN": toolchain,
        "XWIN_CACHE_DIR": str(CROSS_DIR / "xwin"),
        "PATH": f"{cargo_home / 'bin'}{os.pathsep}{path}" if path else str(cargo_home / "bin"),
    }
    return WindowsCross(str(xwin), env, CROSS_TARGET_DIR, f"private toolchain {CROSS_DIR}")


def ambient_windows_cross() -> tuple[WindowsCross | None, str]:
    """A `cargo-xwin` the host already has (PATH, `~/.cargo/bin`), or why not."""
    xwin = find_cargo_tool("cargo-xwin")
    if xwin is None:
        return None, "cargo-xwin is not installed"
    if not ensure_rust_target(WINDOWS_X64_TARGET):
        return None, f"the {WINDOWS_X64_TARGET} standard library is not installed"
    if not xwin_sdk_cached(xwin_cache_dir()):
        return None, "the Microsoft CRT/Windows SDK is not cached"
    return WindowsCross(xwin, {}, ROOT / "target", "cargo-xwin on this host"), ""


def windows_cross_toolchain() -> tuple[WindowsCross | None, str | None]:
    """Resolve the Windows cross toolchain: ``(toolchain, None)`` or ``(None, why not)``.

    Everything checked here is a tool or component the driver cannot supply
    itself, so a missing one is a reason to skip with instructions rather than
    a build failure: that is how a missing Arch `makepkg` is treated on
    Windows. The private `.cross` toolchain wins over an ambient one because
    it is complete by construction.
    """
    private = private_windows_cross()
    if private is not None:
        return private, None
    ambient, why = ambient_windows_cross()
    if ambient is not None:
        return ambient, None
    return None, (
        f"{why}. Run `python {CROSS_SETUP} --accept-microsoft-license` once: it "
        f"installs a private toolchain into {CROSS_DIR} (nothing outside the "
        "checkout) and downloads the Microsoft CRT/SDK under Microsoft's license "
        "terms, which the driver never does by itself"
    )


def build_windows_cross(profile: str) -> tuple[Path | None, bool]:
    """Cross-build the Windows x86_64 GUI, service and setup (non-Windows host).

    Returns ``(taskman.exe, attempted)``; ``attempted`` is False when the host
    lacks the toolchain, so the caller can tell a skip from a failed build.
    `cargo-xwin` supplies the MSVC CRT and Windows SDK import libraries and
    links with lld-link, which understands the same hardening and installer
    arguments (`/CETCOMPAT`, `/DELAYLOAD`, `/MANIFESTUAC`) the MSVC linker gets
    on a Windows host, so the result is the artifact a Windows host builds and
    not a degraded stand-in.
    """
    toolchain, reason = windows_cross_toolchain()
    if toolchain is None:
        log(f"windows x86_64 cross build unavailable: {reason} - skipping")
        return None, False
    log(f"building windows x86_64 with {toolchain.label}")
    # The binary found above is run directly: a `~/.cargo/bin` that is not on
    # PATH would hide it from `cargo xwin`'s subcommand lookup. It expects the
    # subcommand name as its first argument, exactly as cargo passes it.
    command = [
        toolchain.xwin,
        "xwin",
        "build",
        "--profile",
        profile,
        "--target",
        WINDOWS_X64_TARGET,
        "--workspace",
    ]
    # Only x86_64 is built, so only x86_64 may be fetched or demanded: the
    # default also wants aarch64, which would start a second SDK download.
    env = {**toolchain.env, "XWIN_ARCH": "x86_64"}
    if toolchain.target_dir != ROOT / "target":
        env["CARGO_TARGET_DIR"] = str(toolchain.target_dir)
    if profile != "dev":
        env.update(release_rustflags(True) or {})
    if not run(command, env=env):
        return None, True
    out_dir = toolchain.target_dir / WINDOWS_X64_TARGET / ("debug" if profile == "dev" else profile)
    exe = out_dir / "taskman.exe"
    if not exe.exists():
        log(f"windows x86_64 binary missing after build: {exe}")
        return None, True
    return exe, True


def build_windows_arm64(profile: str) -> Path | None:
    """Cross-build the Windows ARM64 GUI + service (Windows host only)."""
    if platform.system() != "Windows":
        log("windows arm64 build requires a Windows host - skipping")
        return None
    if host_target().startswith("aarch64-"):
        # The host build already produced the ARM64 binaries.
        return None
    if WINDOWS_ARM64_TARGET not in installed_rust_targets():
        log(f"target {WINDOWS_ARM64_TARGET} not installed - skipping windows arm64")
        return None
    env = release_rustflags(True, arm64=True) if profile != "dev" else None
    if not run(
        [
            cargo(),
            "build",
            "--profile",
            profile,
            "--target",
            WINDOWS_ARM64_TARGET,
            "--workspace",
        ],
        env=env,
    ):
        return None
    exe = ROOT / "target" / WINDOWS_ARM64_TARGET / profile / "taskman.exe"
    if not exe.exists():
        log(f"windows arm64 binary missing after build: {exe}")
        return None
    return exe


def build_host(profile: str) -> Path | None:
    exe_name = "taskman.exe" if platform.system() == "Windows" else "taskman"
    out_dir = ROOT / "target" / ("debug" if profile == "dev" else profile)
    env = (
        release_rustflags(
            platform.system() == "Windows", arm64=host_target().startswith("aarch64-")
        )
        if profile != "dev"
        else None
    )
    if not run([cargo(), "build", "--profile", profile, "--workspace"], env=env):
        return None
    exe = out_dir / exe_name
    if not exe.exists():
        log(f"host binary missing after build: {exe}")
        return None
    return exe


def linux_cross_env(command: list[str], profile: str) -> dict[str, str]:
    """Environment for a Linux cross build, including zig for `cargo zigbuild`.

    `cargo-zigbuild` invokes the `zig` binary itself; a package-installed
    ziglang is not on PATH, so point `ZIG` at it and prepend its directory.
    """
    env = dict(release_rustflags(False) or {}) if profile != "dev" else {}
    if len(command) > 1 and command[1] == "zigbuild":
        zig_path = find_zig()
        if zig_path is not None:
            env["ZIG"] = str(zig_path)
            zig_dir = str(zig_path.parent)
            current_path = os.environ.get("PATH", "")
            env["PATH"] = f"{zig_dir}{os.pathsep}{current_path}" if current_path else zig_dir
    return env


def build_linux(profile: str, target: str = LINUX_TARGET) -> tuple[Path | None, bool, str]:
    """Build one Linux artifact, preferring a glibc cross toolchain.

    Returns ``(binary, attempted, flavor)`` where flavor is ``"gnu"`` or
    ``"musl"``. ``attempted`` is False only when no path exists at all, so the
    caller can distinguish "skipped for missing tooling" from "build failed".
    The self-contained musl fallback exists for x86_64 only.
    """
    cmd = linux_cross_command(profile, target)
    if cmd is not None:
        command, out_dir = cmd
        env = linux_cross_env(command, profile)
        if not run(command, env=env or None):
            return None, True, "gnu"
        exe = Path(out_dir) / "taskman"
        if not exe.exists():
            log(f"linux binary missing after build: {exe}")
            return None, True, "gnu"
        return exe, True, "gnu"
    if target == host_target():
        # Native Linux host matching the target (e.g. x86_64-unknown-linux-gnu
        # without cross/zigbuild): build_host() produces the native glibc
        # binaries directly so tarballs and .deb/.rpm packages still build.
        exe = build_host(profile)
        return exe, True, "gnu"
    if target != LINUX_TARGET:
        return None, False, "gnu"
    # No glibc cross toolchain: the rust-lld/musl path works with rustup alone.
    exe, attempted = build_linux_musl(profile)
    return exe, attempted, "musl"


def clean_stale_linux_artifacts(version: str, active_flavor: str) -> None:
    """Remove the archive this build replaces — never a better one.

    A gnu build replaces an older musl fallback for the same version. A musl
    build must NOT delete a GUI-capable gnu archive: this host cannot rebuild
    it (no `cross`/`zig`), and `packaging/linux/build-artifact.sh` produces it
    on a Linux host or CI, so deleting it would silently downgrade `dist/` to
    the headless artifact. It is reported instead.
    """
    gnu_base = f"taskman-v{version}-linux-x86_64.tar.gz"
    musl_base = f"taskman-v{version}-linux-x86_64-musl.tar.gz"
    if active_flavor == "gnu":
        stale_base = musl_base
    elif active_flavor == "musl":
        gnu = DIST / gnu_base
        if gnu.exists():
            log(
                "keeping the GUI-capable linux artifact already in dist "
                f"({gnu.name}); this host cannot rebuild it"
            )
        return
    else:
        return
    for name in (stale_base, f"{stale_base}.sha256"):
        stale = DIST / name
        if stale.exists():
            stale.unlink()
            log(f"removed stale linux artifact: {stale}")


def linux_payload_files(
    gui_exe: Path, flavor: str = "gnu"
) -> list[tuple[Path, str]] | None:
    """Contents of one Linux install tarball, or None when a part is missing.

    The archive reproduces the directory `packaging/linux/install-linux.sh`
    expects to run from: both binaries below `bin/`, the packaging files next to
    the script. A tarball without the broker is not installable at all, so a
    missing piece fails the run instead of shipping half a product. The static
    musl flavor carries only the binaries and the README (see
    [`LINUX_MUSL_PAYLOAD_FILES`]).
    """
    names = LINUX_MUSL_PAYLOAD_FILES if flavor == "musl" else LINUX_PAYLOAD_FILES
    service_exe = gui_exe.with_name(LINUX_SERVICE_BINARY)
    missing: list[str] = []
    if not service_exe.exists():
        missing.append(str(service_exe))
    missing += [
        str(LINUX_PACKAGING / name)
        for name in names
        if not (LINUX_PACKAGING / name).is_file()
    ]
    if missing:
        for path in missing:
            log(f"linux artifact is incomplete: {path} is missing")
        return None
    files = [
        (gui_exe, f"bin/{gui_exe.name}"),
        (service_exe, f"bin/{service_exe.name}"),
    ]
    files += [(LINUX_PACKAGING / name, name) for name in names]
    return files


# Names the distro that provides `makepkg` when the host has none. Setting it
# skips the discovery below, which has to start each candidate to look inside.
ARCH_WSL_DISTRO_ENV = "TASKMAN_ARCH_WSL_DISTRO"
# Where makepkg builds when the checkout is reached through WSL. Its own
# default is `$startdir`, i.e. `packaging/arch` inside the checkout, which is
# the wrong place for a second copy of the whole target tree: the checkout is
# on a 9p mount (slow for the thousands of object files a cold build writes,
# and not necessarily writable at all from inside the distro), and the build
# would leave gigabytes in the repository. /var/tmp survives a distro restart,
# so a rebuilt package only relinks instead of recompiling 900 crates.
ARCH_WSL_BUILD_DIR = "/var/tmp/taskman-arch/build"
ARCH_WSL_SRC_DIR = "/var/tmp/taskman-arch/src"


class ArchPackageHost(NamedTuple):
    """A `makepkg` this run can package with, and the paths it understands.

    `wsl is None` is a native `makepkg` on an Arch host, where the working
    directory and PKGDEST are the paths this process already has. Otherwise
    `makepkg` runs inside that distro and every path is the `/mnt/<drive>/...`
    spelling `wslpath` gave it: makepkg writes the package where PKGDEST
    points, so handing it a Windows path would fail the run instead of filling
    `dist/`. `env` is the environment that route always needs (build tree,
    source cache); it is merged under the per-call one.
    """

    makepkg: str
    arch_dir: str
    dist_dir: str
    env: dict[str, str] | None = None
    wsl: str | None = None
    distro: str | None = None

    @property
    def label(self) -> str:
        if self.distro is None:
            return self.makepkg
        return f"makepkg in WSL distro {self.distro}"

    def environment(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        return {**(self.env or {}), **(extra or {})}

    def argv(self, args: list[str], env: dict[str, str] | None = None) -> list[str]:
        """The command that runs `makepkg args` from `arch_dir`."""
        if self.wsl is None or self.distro is None:
            return [self.makepkg, *args]
        # One `bash -lc` carries the working directory, the environment and the
        # command: `wsl.exe` has no cwd or env of its own, and a PKGDEST set on
        # the Windows side would never reach the distro.
        command = " ".join(
            [
                *(
                    f"{name}={shlex.quote(value)}"
                    for name, value in self.environment(env).items()
                ),
                self.makepkg,
                *(shlex.quote(arg) for arg in args),
            ]
        )
        return [
            self.wsl,
            "-d",
            self.distro,
            "--",
            "bash",
            "-lc",
            f"cd {shlex.quote(self.arch_dir)} && {command}",
        ]

    def capture(self, args: list[str]) -> subprocess.CompletedProcess:
        if self.wsl is None:
            return subprocess.run(
                [self.makepkg, *args],
                cwd=ARCH_PACKAGING,
                env={**os.environ, **self.environment()},
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
        return subprocess.run(
            self.argv(args),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )

    def run(self, args: list[str], env: dict[str, str] | None = None) -> bool:
        if self.wsl is None:
            return run(
                [self.makepkg, *args],
                env=self.environment(env),
                cwd=ARCH_PACKAGING,
            )
        return run(self.argv(args, env))


def wsl_path(wsl: str, distro: str, path: Path) -> str | None:
    """`path` as that distro spells it, or None when the translation fails.

    `/mnt/c/...` is only the default: a distro that sets `root=` in
    /etc/wsl.conf mounts the Windows drives somewhere else, and `wslpath` is
    the only authority on the spelling that distro actually resolves.
    """
    proc = subprocess.run(
        [wsl, "-d", distro, "--", "wslpath", "-a", "-u", str(path)],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    translated = proc.stdout.strip()
    if proc.returncode != 0 or not translated:
        log(f"cannot translate {path} for WSL distro {distro}")
        if proc.stderr.strip():
            log(proc.stderr.strip())
        return None
    return translated


def wsl_distro_names(wsl: str) -> list[str]:
    """The installed distros, the ones named like Arch first.

    `wsl --list --quiet` writes UTF-16LE with a byte-order mark, but a
    redirected pipe has been seen in UTF-8 on newer builds, so the decode
    follows the bytes instead of assuming one. Ordering matters for time, not
    for correctness: every candidate has to be started to read its
    /etc/os-release, so the likely distro goes first and a host with several
    pays for one start instead of all of them.
    """
    configured = os.environ.get(ARCH_WSL_DISTRO_ENV, "").strip()
    if configured:
        return [configured]
    proc = subprocess.run([wsl, "--list", "--quiet"], capture_output=True)
    if proc.returncode != 0:
        return []
    if proc.stdout[:2] in (b"\xff\xfe", b"\xfe\xff"):
        text = proc.stdout.decode("utf-16", errors="replace")
    elif b"\x00" in proc.stdout:
        text = proc.stdout.decode("utf-16-le", errors="replace")
    else:
        text = proc.stdout.decode("utf-8", errors="replace")
    names = [
        line.replace("\ufeff", "").replace("\x00", "").strip()
        for line in text.splitlines()
    ]
    return sorted(
        (name for name in names if name),
        key=lambda name: 0 if "arch" in name.lower() else 1,
    )


def wsl_has_makepkg(wsl: str, distro: str) -> bool:
    """True when the distro is Arch and has makepkg, which the PKGBUILD needs.

    The distribution check is not decoration: the same probe run in a Debian
    distro would look for a package manager that distro does not have, and a
    half-matched distro would fail the whole run instead of being skipped.
    """
    proc = subprocess.run(
        [
            wsl,
            "-d",
            distro,
            "--",
            "bash",
            "-lc",
            "grep -q '^ID=arch$' /etc/os-release && command -v makepkg >/dev/null",
        ],
        capture_output=True,
    )
    return proc.returncode == 0


@cache
def arch_package_host() -> ArchPackageHost | None:
    """The `makepkg` this run packages with, or None when the host has none.

    A native `makepkg` wins: on an Arch machine the package builds without
    crossing another filesystem layer. Windows has no pacman, so there the only
    `makepkg` lives in an Arch WSL distro, and this driver reaches it with
    `wsl.exe` instead of giving up — the package is one of the artifacts a
    release ships, not an extra step someone has to remember.
    """
    native = shutil.which("makepkg")
    if native:
        return ArchPackageHost(
            native, str(ARCH_PACKAGING), str(DIST), {"PKGDEST": str(DIST)}
        )
    if platform.system() != "Windows":
        return None
    wsl = shutil.which("wsl.exe") or shutil.which("wsl")
    if wsl is None:
        return None
    for distro in wsl_distro_names(wsl):
        if not wsl_has_makepkg(wsl, distro):
            continue
        arch_dir = wsl_path(wsl, distro, ARCH_PACKAGING)
        dist_dir = wsl_path(wsl, distro, DIST)
        if arch_dir is None or dist_dir is None:
            continue
        # PKGDEST belongs here and not only on the build call: makepkg refuses
        # to run at all when its destination is not writable, and a checkout
        # mounted read-only inside the distro would otherwise fail even the
        # `--printsrcinfo` check.
        return ArchPackageHost(
            "makepkg",
            arch_dir,
            dist_dir,
            {
                "BUILDDIR": ARCH_WSL_BUILD_DIR,
                "SRCDEST": ARCH_WSL_SRC_DIR,
                "PKGDEST": dist_dir,
            },
            wsl=wsl,
            distro=distro,
        )
    return None


def arch_srcinfo_matches(host: ArchPackageHost) -> bool:
    """Check that the committed .SRCINFO is what makepkg prints for the PKGBUILD.

    .SRCINFO is the artifact AUR helpers read, and the one place where a stale
    copy describes a package this build is not producing. makepkg is the only
    authority on the exact field set and order, so the comparison uses its
    output instead of a hand-maintained expectation.
    """
    proc = host.capture(["--printsrcinfo"])
    if proc.returncode != 0:
        log("makepkg --printsrcinfo failed")
        if proc.stderr.strip():
            log(proc.stderr.strip())
        return False
    committed_file = ARCH_PACKAGING / ".SRCINFO"
    committed = (
        committed_file.read_text(encoding="utf-8") if committed_file.is_file() else ""
    )

    def lines(text: str) -> list[str]:
        # Blank lines and trailing whitespace are formatting, not content.
        return [
            line.rstrip()
            for line in text.replace("\r\n", "\n").splitlines()
            if line.strip()
        ]

    if lines(committed) == lines(proc.stdout):
        return True
    log(f"{committed_file} does not match `makepkg --printsrcinfo`")
    log(f"regenerate it: (cd {ARCH_PACKAGING} && makepkg --printsrcinfo > .SRCINFO)")
    for line in difflib.unified_diff(
        lines(committed),
        lines(proc.stdout),
        fromfile=".SRCINFO (committed)",
        tofile="makepkg --printsrcinfo",
        lineterm="",
    ):
        log(line)
    return False


def build_arch_package(version: str) -> bool:
    """Build the pacman package from `packaging/arch` into `dist/`.

    Returns False only for a real failure. A host with no makepkg anywhere is
    a skip rather than a failure, because `--check` runs here too; the package
    is still part of every release, so a release run (`--require-all-targets`)
    fails when it is missing from `dist/`. The PKGBUILD has no `source`
    array — it compiles the two binaries from THIS checkout — so the packaged
    version must be the version this run just built; a bump that forgets the
    PKGBUILD would otherwise silently package a different release.
    """
    # `package()` writes `dist/`, and the WSL path has to exist before `wslpath`
    # is asked to translate it.
    DIST.mkdir(exist_ok=True)
    host = arch_package_host()
    if host is None:
        log(
            "no makepkg on this host and no Arch WSL distro - skipping the arch "
            f"package (set {ARCH_WSL_DISTRO_ENV} to name the distro to use); "
            "it is part of every release, so build it on an Arch host or in an "
            "Arch WSL distro (see llm-wiki/build.md, 'Release artifact set')"
        )
        return True
    pkgbuild = ARCH_PACKAGING / "PKGBUILD"
    if not pkgbuild.is_file():
        log(f"arch packaging missing: {pkgbuild}")
        return False
    match = re.search(
        r"^pkgver=(.+)$", pkgbuild.read_text(encoding="utf-8"), re.MULTILINE
    )
    if not match:
        log(f"cannot read pkgver from {pkgbuild}")
        return False
    if match.group(1).strip() != version:
        log(
            f"{pkgbuild} declares pkgver {match.group(1).strip()} but the workspace "
            f"is {version} - bump the PKGBUILD and its .SRCINFO"
        )
        return False
    if not arch_srcinfo_matches(host):
        return False
    log(f"building the arch package with {host.label}")
    # PKGDEST keeps the package next to the other artifacts instead of littering
    # the packaging directory.
    if not host.run(["--force", "--noconfirm"]):
        return False
    # makepkg names it `taskman-<ver>-<rel>-<arch>.pkg.tar.zst`; the release
    # files carry the distribution instead so a user can pick theirs at a glance.
    built = sorted(DIST.glob(f"taskman-{version}-*-x86_64.pkg.tar.*"))
    built = [p for p in built if not p.name.endswith(".sig")]
    if not built:
        log(f"makepkg succeeded but no taskman-{version}-*-x86_64.pkg.tar.* is in {DIST}")
        return False
    suffix = built[-1].name.split(".pkg.tar", 1)[1]
    dest = DIST / distro_package_name(version, "arch", "x86_64", f"pkg.tar{suffix}")
    built[-1].replace(dest)
    log(f"packaged: {dest}")
    return True


DISTRO_PACKAGE_RELEASE = "1"


def distro_package_name(version: str, distro: str, arch: str, ext: str) -> str:
    """`taskman-v0.1.34-debian-ubuntu-x86_64.deb`: distro and arch in the name."""
    return f"taskman-v{version}-{distro}-{arch}.{ext}"


def release_package_names(
    version: str,
    distro_arches: list[str],
    *,
    arch_package: bool,
    distro_packages: bool,
) -> list[str]:
    """The Linux packages every release ships, by file name.

    They are not extras: a release without the pacman package and the deb/rpm
    pair is incomplete, which is why `--require-all-targets` refuses to finish
    when one of these is missing from `dist/`.
    """
    names: list[str] = []
    if arch_package:
        names.append(distro_package_name(version, "arch", "x86_64", "pkg.tar.zst"))
    if distro_packages:
        for arch in distro_arches:
            names.append(distro_package_name(version, "debian-ubuntu", arch, "deb"))
            names.append(distro_package_name(version, "fedora", arch, "rpm"))
    return names


def missing_release_packages(names: list[str], dist: Path) -> list[str]:
    """The names of `names` that `dist` does not carry."""
    return [name for name in names if not (dist / name).is_file()]


def load_distpkg():
    spec = importlib.util.spec_from_file_location(
        "taskman_distpkg", ROOT / "tools" / "distpkg.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def build_distro_packages(version: str, gui_exe: Path, arch: str) -> bool:
    """Write the Debian/Ubuntu `.deb` and the Fedora `.rpm` for one Linux target.

    `arch` is the release naming (`x86_64` or `arm64`). The payload is staged by
    `install-linux.sh --packaged`, the installer every other artifact uses, so
    the layout is described exactly once. Needs a POSIX `bash`: a Windows host
    logs a skip, like a missing makepkg.
    """
    if platform.system() != "Linux":
        log(
            "deb/rpm packages need a Linux host with POSIX file modes - skipping; "
            "they are part of every release, so build them in a Linux/WSL distro "
            "(see llm-wiki/build.md, 'Release artifact set')"
        )
        return True
    service_exe = gui_exe.with_name(LINUX_SERVICE_BINARY)
    if not service_exe.exists():
        log(f"core service binary missing for the deb/rpm packages: {service_exe}")
        return False
    distpkg = load_distpkg()
    deb_arch, rpm_arch = {"x86_64": ("amd64", "x86_64"), "arm64": ("arm64", "aarch64")}[
        arch
    ]
    mtime = int(os.environ.get("SOURCE_DATE_EPOCH", "0")) or int(
        (ROOT / "Cargo.toml").stat().st_mtime
    )
    DIST.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="taskman-distpkg-") as tmp:
        payload = Path(tmp) / "payload"
        stage = Path(tmp) / "stage"
        (payload / "bin").mkdir(parents=True)
        stage.mkdir()
        shutil.copy2(gui_exe, payload / "bin" / "taskman")
        shutil.copy2(service_exe, payload / "bin" / LINUX_SERVICE_BINARY)
        if not run(
            [
                "bash",
                str(LINUX_PACKAGING / "install-linux.sh"),
                "--packaged",
                "--quiet",
                "--root",
                str(stage),
                "--source-dir",
                str(payload),
            ]
        ):
            log("staging the deb/rpm payload failed")
            return False
        try:
            entries = distpkg.stage_tree(stage)
            deb = distpkg.build_deb(
                entries, version, DISTRO_PACKAGE_RELEASE, deb_arch, mtime
            )
            rpm = distpkg.build_rpm(
                entries, version, DISTRO_PACKAGE_RELEASE, rpm_arch, mtime
            )
        except (OSError, ValueError) as err:
            log(f"building the deb/rpm packages failed: {err}")
            return False
    for data, name in (
        (deb, distro_package_name(version, "debian-ubuntu", arch, "deb")),
        (rpm, distro_package_name(version, "fedora", arch, "rpm")),
    ):
        (DIST / name).write_bytes(data)
        log(f"packaged: {DIST / name}")
    return True


def package_zip(name: str, files: list[tuple[Path, str]]) -> Path:
    DIST.mkdir(exist_ok=True)
    dest = DIST / f"{name}.zip"
    with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as zf:
        for src, arc in files:
            zf.write(src, arc)
    return dest


def package_tar(name: str, files: list[tuple[Path, str]]) -> Path:
    DIST.mkdir(exist_ok=True)
    dest = DIST / f"{name}.tar.gz"
    with tarfile.open(dest, "w:gz") as tf:
        for src, arc in files:
            info = tf.gettarinfo(src, arcname=arc)
            # Windows has no POSIX mode bits, so `add()` records 0o644/0o666
            # and the extracted Linux binary is not executable — the artifact
            # was unusable until the user ran `chmod +x`. Stamp the
            # conventional release modes instead, and the release ownership
            # so the archive does not carry the build host's identity.
            info.mode = 0o644 if arc.endswith(TEXT_ARTIFACT_SUFFIXES) else 0o755
            info.uid = 0
            info.gid = 0
            info.uname = "root"
            info.gname = "root"
            with src.open("rb") as fh:
                tf.addfile(info, fh)
    return dest


def package_setup(
    profile: str, gui_exe: Path, service_exe: Path, label: str, version: str
) -> Path | None:
    """Build one self-contained setup installer (Windows target).

    `taskman-setup.exe` (linked for gui_exe's target) gets the payload archive
    appended by the HOST `taskman-payload` tool, so an x64 host can package an
    ARM64 installer, and a Linux host the Windows one. Archive writer and reader live in one Rust crate
    (`crates/tm-installer`), so the format cannot drift between the two.

    The artifact is embedded into a temporary file under `target/`, verified
    there, and only then moved into `dist/` with one atomic replace. Any
    failure removes the temporary file AND an existing `dist/` setup of the
    same name: a failed packaging run must not leave a broken artifact, or a
    stale one that looks like the output of this run, behind.
    """
    dest = DIST / f"taskman-v{version}-{label}-setup.exe"
    # The payload tool and the staging file belong to the HOST build directory,
    # not to `gui_exe`'s: a cross-built installer (Linux host, Windows target)
    # is embedded by the tool the host can run.
    out_dir = ROOT / "target" / ("debug" if profile == "dev" else profile)
    staged = out_dir / f"{dest.name}.partial"

    def discard(reason: str) -> None:
        log(reason)
        for path in (staged, dest):
            try:
                path.unlink(missing_ok=True)
            except OSError as error:
                log(f"cannot remove {path}: {error}")

    setup_exe = gui_exe.with_name("taskman-setup.exe")
    if not setup_exe.exists():
        discard(f"setup binary missing after build: {setup_exe}")
        return None
    payload_tool = out_dir / (
        "taskman-payload.exe" if platform.system() == "Windows" else "taskman-payload"
    )
    if not payload_tool.exists():
        discard(f"payload tool missing after build: {payload_tool}")
        return None
    staged.unlink(missing_ok=True)
    command = [
        str(payload_tool),
        str(setup_exe),
        str(staged),
        f"taskman.exe={gui_exe}",
        f"taskman-service.exe={service_exe}",
    ]
    license_file = ROOT / "LICENSE"
    if license_file.exists():
        command.append(f"LICENSE={license_file}")
    if not run(command):
        discard(f"setup payload embedding failed: {staged}")
        return None
    # Packaging is not done until the produced artifact re-parses and every
    # embedded entry hash-checks. This is the same check `taskman-setup` runs
    # at install time, run here so a broken artifact never reaches dist/.
    if not run([str(payload_tool), "verify", str(staged)]):
        discard(f"setup payload verification failed: {staged}")
        return None
    try:
        DIST.mkdir(exist_ok=True)
        os.replace(staged, dest)
    except OSError as error:
        discard(f"cannot move verified setup into {dest}: {error}")
        return None
    return dest


def audit_scan() -> bool:
    """Dependency-advisory and secrets scanning (opt-in `--audit`).

    Advisory mode on purpose: a missing scanner is reported as a coverage gap
    rather than silently passing, but only an actual finding fails the run.
    Neither tool is needed for ordinary builds, so this is never part of
    `--check` (which must stay offline-capable).
    """
    ok = True
    missing: list[str] = []
    if have("cargo-audit"):
        ok &= run([cargo(), "audit"])
    else:
        missing.append("cargo-audit")
    if have("gitleaks"):
        ok &= run(["gitleaks", "detect", "--no-banner", "--redact"])
    else:
        missing.append("gitleaks")
    if missing:
        log(
            f"audit: {' and '.join(missing)} not installed - "
            "dependency/secrets coverage is incomplete"
        )
    return ok


def probe_powershell() -> tuple[str | None, str | None]:
    """Return ``(path, problem)`` for a runnable PowerShell.

    ``problem`` is set when a PowerShell was found but could not be probed
    (an inaccessible WindowsApps alias, a policy block, or a wedged process).
    That is different from "not installed": the fork gate must not silently
    skip when the shell it needs exists but is unusable.
    """
    problem: str | None = None
    for candidate in ("pwsh", "powershell"):
        path = shutil.which(candidate)
        if path is None:
            continue
        try:
            proc = subprocess.run(
                [path, "-NoProfile", "-Command", "exit 0"],
                capture_output=True,
                timeout=10,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            problem = f"{candidate} at {path}: {exc}"
            continue
        if proc.returncode == 0:
            return path, None
        problem = f"{candidate} at {path} exited {proc.returncode}"
    return None, problem


def find_powershell() -> str | None:
    """Return a runnable PowerShell binary, skipping inaccessible WindowsApps aliases."""
    return probe_powershell()[0]


def check_fork() -> bool:
    """Lint and test the vendored egui fork at vendor/egui.

    The workspace gate above cannot cover it: `cargo clippy --workspace` only passes
    `-D warnings` to the packages it selected, and vendor/egui is deliberately an excluded
    separate workspace (it needs its own `workspace = true` field inheritance). Without
    this step the fork's crates -- including the CPU renderer -- sit outside the gate.

    Skipped with a note when the fork is absent, so a checkout that has not run
    `git subtree add` yet still builds.
    """
    fork = ROOT / "vendor" / "egui"
    if not (fork / "Cargo.toml").exists():
        log("vendor/egui not present - skipping fork gate")
        return True
    script = ROOT / "tools" / "check-fork.ps1"
    shell, problem = probe_powershell()
    if shell is None:
        if problem:
            log(
                f"PowerShell was found but cannot run ({problem}); "
                "the vendored fork gate cannot run"
            )
            return False
        # A Linux/macOS host normally has no PowerShell at all. Skipping there
        # left both vendored forks — the CPU renderer, the event loop, the
        # Wayland window code — outside every local gate on exactly the hosts
        # whose code paths they carry, so run the same steps from Python.
        log("no pwsh/powershell found - running the fork gate steps directly")
        return all(run(cmd, cwd=cwd) for cwd, cmd in fork_gate_commands())
    return run([shell, "-NoProfile", "-File", str(script)])


# The crates taskman ships from the egui fork; must match `$packages` in
# tools/check-fork.ps1 (a build test pins that).
FORK_PACKAGES = (
    "emath",
    "ecolor",
    "epaint",
    "epaint_default_fonts",
    "egui",
    "egui-winit",
    "egui_glow",
    "egui-wgpu",
    "eframe",
    "egui_software",
)
# eframe's feature set for the software renderer, as in tools/check-fork.ps1.
FORK_EFRAME_SOFTWARE = "accesskit,default_fonts,software,wayland,x11"


def fork_gate_commands() -> list[tuple[Path, list[str]]]:
    """The steps of tools/check-fork.ps1 (check mode), for hosts without PowerShell."""
    egui = ROOT / "vendor" / "egui"
    winit = ROOT / "vendor" / "winit"
    c = cargo()
    pkgs = [arg for name in FORK_PACKAGES for arg in ("-p", name)]
    software = ["-p", "eframe", "--no-default-features", "--features", FORK_EFRAME_SOFTWARE]
    steps: list[tuple[Path, list[str]]] = [
        (egui, [c, "fmt", *pkgs, "--", "--check"]),
        (egui, [c, "clippy", *pkgs, "--all-targets"]),
        (egui, [c, "clippy", "-p", "egui_software", "--all-targets", "--", "-D", "warnings"]),
        (egui, [c, "clippy", *software, "--all-targets"]),
        (egui, [c, "test", *pkgs]),
        (egui, [c, "test", *software]),
    ]
    if (winit / "Cargo.toml").exists():
        steps += [
            (winit, [c, "clippy", "--all-targets", "--", "-D", "warnings"]),
            (winit, [c, "test"]),
        ]
    return steps


def is_elevated() -> bool:
    if platform.system() != "Windows":
        return os.geteuid() == 0 if hasattr(os, "geteuid") else False
    import ctypes

    try:
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False


def live_tests() -> bool:
    """Run the `#[ignore]`d tests that need a live kernel and elevation.

    These are the ONLY checks that prove the hand-written NT structure
    offsets and ETW payload decoders against the thing they decode:
    `SystemMemoryListInformation` behind the memory-composition bar, and the
    network/disk ETW attribution behind the per-process rate columns. Every
    other test in the workspace feeds them synthetic records, so a Windows
    build that moves a field passes the whole gate and ships a column that is
    quietly wrong.

    They are excluded from `--check` on purpose: they need an elevated token
    and real traffic, so they cannot run in CI or on a developer machine by
    default. Run this before cutting a release, and after any Windows feature
    update that touches the process, memory or storage stacks.
    """
    if platform.system() != "Windows":
        log("live tests are Windows-only - skipping")
        return True
    if not is_elevated():
        log("live tests need an ELEVATED shell (ETW sessions + protected process reads)")
        log("re-run `python build.py --live-tests` from an administrator prompt")
        return False
    log("running live-kernel tests (needs real disk/network traffic to be meaningful)")
    return run(
        [
            cargo(),
            "test",
            "-p",
            "tm-platform",
            "--all-features",
            "--",
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ]
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--debug", action="store_true", help="build the dev profile instead of release")
    ap.add_argument(
        "--host-only", action="store_true", help="skip the linux and windows cross builds"
    )
    ap.add_argument("--linux-only", action="store_true", help="skip the host build")
    ap.add_argument(
        "--all-targets",
        action="store_true",
        help="also build Windows ARM64 and Linux ARM64 artifacts",
    )
    ap.add_argument(
        "--no-package", action="store_true", help="build but skip dist/ packaging"
    )
    ap.add_argument(
        "--no-arch-package",
        action="store_true",
        help="skip the pacman package (its build compiles the Linux binaries again)",
    )
    ap.add_argument(
        "--no-windows",
        action="store_true",
        help="skip the Windows x86_64 cross build a non-Windows host otherwise makes",
    )
    ap.add_argument(
        "--require-all-targets",
        action="store_true",
        help="fail when a requested target has to be skipped for missing tooling",
    )
    ap.add_argument(
        "--check",
        action="store_true",
        help="run the full quality gate first (fmt, clippy -D warnings, tests)",
    )
    ap.add_argument(
        "--audit",
        action="store_true",
        help="run dependency-advisory and secrets scanners first (cargo-audit, gitleaks)",
    )
    ap.add_argument(
        "--no-distro-packages",
        action="store_true",
        help="skip the Debian/Ubuntu .deb and Fedora .rpm packages",
    )
    ap.add_argument(
        "--live-tests",
        action="store_true",
        help="run the #[ignore]d live-kernel/ETW tests (Windows, elevated) and exit",
    )
    args = ap.parse_args()

    profile = "dev" if args.debug else "release"
    version = read_version()
    log(f"taskman v{version} - profile={profile}")

    if args.live_tests:
        return 0 if live_tests() else 1

    if args.audit and not audit_scan():
        log("audit failed")
        return 1

    if args.check:
        ok = True
        for test in sorted((ROOT / "tools" / "tests").glob("test_*.py")):
            ok &= run([sys.executable, str(test)])
        ok &= run([cargo(), "fmt", "--all", "--", "--check"])
        ok &= run(
            [
                cargo(),
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ]
        )
        ok &= run([cargo(), "test", "--workspace", "--all-features"])
        ok &= cross_check()
        ok &= renderer_feature_check()
        ok &= check_fork()
        if not ok:
            log("quality gate failed")
            return 1

    failures = 0
    artifacts: list[tuple[str, list[tuple[Path, str]]]] = []
    host_requested = not args.linux_only
    linux_requested = not args.host_only

    if host_requested:
        exe = build_host(profile)
        if exe is None:
            failures += 1
        else:
            log(f"host binary ready: {exe}")
            arc = "taskman.exe" if exe.suffix == ".exe" else "taskman"
            host_files = [(exe, arc)]
            windows_service_exe: Path | None = None
            if platform.system() == "Windows":
                service_exe = exe.with_name("taskman-service.exe")
                if not service_exe.exists():
                    log(f"core service binary missing after build: {service_exe}")
                    failures += 1
                else:
                    host_files.append((service_exe, "taskman-service.exe"))
                    windows_service_exe = service_exe
            if platform.system() != "Linux" or not linux_requested:
                if platform.system() == "Linux" and LINUX_DESKTOP.exists():
                    host_files.append(
                        (LINUX_DESKTOP, "share/applications/io.github.aufkrawall.Taskman.desktop")
                    )
                artifacts.append((f"taskman-v{version}-{host_tag()}", host_files))
            if windows_service_exe is not None and not args.no_package:
                setup = package_setup(
                    profile, exe, windows_service_exe, host_tag(), version
                )
                if setup is None:
                    failures += 1
                else:
                    log(f"setup installer ready: {setup}")

    # Windows ARM64: a Windows x64 host cross-builds it with the MSVC ARM64
    # toolset. On an ARM64 host the host artifact already IS the ARM64 build.
    windows_arm64_possible = (
        platform.system() == "Windows"
        and not host_target().startswith("aarch64-")
    )
    if args.all_targets and host_requested and windows_arm64_possible:
        exe = build_windows_arm64(profile)
        if exe is None:
            if args.require_all_targets:
                log("windows arm64 build required (--require-all-targets)")
                failures += 1
        else:
            log(f"windows arm64 binary ready: {exe}")
            files = [(exe, "taskman.exe")]
            service_exe = exe.with_name("taskman-service.exe")
            if not service_exe.exists():
                log(f"core service binary missing after build: {service_exe}")
                failures += 1
            else:
                files.append((service_exe, "taskman-service.exe"))
            artifacts.append((f"taskman-v{version}-windows-arm64", files))
            if service_exe.exists() and not args.no_package:
                setup = package_setup(profile, exe, service_exe, "windows-arm64", version)
                if setup is None:
                    failures += 1
                else:
                    log(f"setup installer ready: {setup}")

    # Windows x86_64 from a non-Windows host. It is a cross build like the
    # Linux one, so `--host-only` leaves it out, and like the Linux one a host
    # without the toolchain skips it unless `--require-all-targets` is given.
    windows_cross_requested = (
        platform.system() != "Windows"
        and not args.host_only
        and not args.linux_only
        and not args.no_windows
    )
    if windows_cross_requested:
        exe, attempted = build_windows_cross(profile)
        if exe is None:
            if attempted:
                failures += 1
            elif args.require_all_targets:
                log("windows build required (--require-all-targets) but no toolchain")
                failures += 1
        else:
            log(f"windows x86_64 binary ready: {exe}")
            service_exe = exe.with_name("taskman-service.exe")
            if not service_exe.exists():
                log(f"core service binary missing after build: {service_exe}")
                failures += 1
            else:
                artifacts.append(
                    (
                        f"taskman-v{version}-windows-x86_64",
                        [(exe, "taskman.exe"), (service_exe, "taskman-service.exe")],
                    )
                )
                if not args.no_package:
                    setup = package_setup(
                        profile, exe, service_exe, "windows-x86_64", version
                    )
                    if setup is None:
                        failures += 1
                    else:
                        log(f"setup installer ready: {setup}")

    linux_x86_64_flavor: str | None = None
    linux_distro_exes: list[tuple[Path, str]] = []
    if linux_requested:
        exe, attempted, flavor = build_linux(profile)
        if exe is not None:
            linux_x86_64_flavor = flavor
            if flavor == "gnu":
                linux_distro_exes.append((exe, "x86_64"))
            else:
                log("static musl build is headless: no deb/rpm package is made from it")
            log(f"linux binary ready ({flavor}): {exe}")
            files = linux_payload_files(exe, flavor)
            if files is None:
                failures += 1
            else:
                suffix = "" if flavor == "gnu" else f"-{flavor}"
                artifacts.append((f"taskman-v{version}-linux-x86_64{suffix}", files))
        elif attempted:
            failures += 1
        elif args.require_all_targets:
            log("linux build required (--require-all-targets) but no toolchain")
            failures += 1

    if linux_requested and args.all_targets:
        exe, attempted, flavor = build_linux(profile, LINUX_ARM64_TARGET)
        if exe is not None:
            log(f"linux arm64 binary ready ({flavor}): {exe}")
            linux_distro_exes.append((exe, "arm64"))
            files = linux_payload_files(exe, flavor)
            if files is None:
                failures += 1
            else:
                artifacts.append((f"taskman-v{version}-linux-arm64", files))
        elif attempted:
            failures += 1
        elif args.require_all_targets:
            log("linux arm64 build required (--require-all-targets) but no toolchain")
            failures += 1

    if not args.no_package:
        for name, files in artifacts:
            if files[0][0].suffix == ".exe":
                dest = package_zip(name, files)
            else:
                dest = package_tar(name, files)
            log(f"packaged: {dest}")
        if linux_x86_64_flavor is not None:
            clean_stale_linux_artifacts(version, linux_x86_64_flavor)

    # Packaging like the other artifacts, so `--no-package` and `--host-only`
    # skip it with the rest: the PKGBUILD compiles the Linux binaries itself,
    # which a host-only run deliberately avoids.
    if not args.no_package and not args.no_arch_package and linux_requested:
        if not build_arch_package(version):
            failures += 1

    if not args.no_package and not args.no_distro_packages:
        for gui_exe, arch in linux_distro_exes:
            if not build_distro_packages(version, gui_exe, arch):
                failures += 1

    # The packages above are part of the release, not an extra: the release
    # switch (`--require-all-targets`) therefore also refuses to finish without
    # them, instead of leaving a published release that quietly lacks them.
    if args.require_all_targets and not args.no_package:
        expected = release_package_names(
            version,
            [arch for _exe, arch in linux_distro_exes],
            arch_package=linux_requested and not args.no_arch_package,
            distro_packages=not args.no_distro_packages,
        )
        missing = missing_release_packages(expected, DIST)
        if missing:
            log(
                "missing release packages (--require-all-targets): "
                + ", ".join(missing)
            )
            failures += len(missing)

    if failures:
        log(f"DONE with {failures} failure(s)")
        return 1
    log("DONE")
    return 0


def host_tag() -> str:
    system = platform.system().lower()
    arch = host_target().split("-", 1)[0]
    return f"{system}-{arch}"


if __name__ == "__main__":
    sys.exit(main())

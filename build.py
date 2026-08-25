#!/usr/bin/env python3
"""Release build driver for taskman.

Default behavior (`python build.py`):
  1. Build an optimized release binary for the HOST platform.
  2. Build the Linux x86_64 release binary as well — the workspace ships a
     real Linux collector (crates/tm-platform/src/linux), so Linux is a
     first-class target. Cross-building a GUI app needs a cross toolchain
     (`cross` or `cargo-zigbuild`); if neither is installed the Linux step
     is skipped with a clear note instead of failing the Windows artifact.
  3. Package both binaries into dist/ as taskman-v<version>-<platform>.

Exit code is 0 if every *attempted* target succeeded. Use
--require-all-targets to make a skipped Linux build a hard error.

Examples:
    python build.py                     # release: host + linux, packaged
    python build.py --host-only         # just this machine's release build
    python build.py --linux-only        # just the linux cross build
    python build.py --debug             # debug profile instead of release
    python build.py --check             # fmt + clippy + tests gate first
"""

from __future__ import annotations

import argparse
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
DIST = ROOT / "dist"
LINUX_TARGET = "x86_64-unknown-linux-gnu"


def log(msg: str) -> None:
    print(f"[build] {msg}", flush=True)


def run(cmd: list[str], env: dict | None = None) -> bool:
    print(f"$ {' '.join(cmd)}", flush=True)
    merged = os.environ.copy()
    if env:
        merged.update(env)
    proc = subprocess.run(cmd, cwd=ROOT, env=merged)
    return proc.returncode == 0


def cargo() -> str:
    exe = "cargo.exe" if platform.system() == "Windows" else "cargo"
    if shutil.which(exe):
        return exe
    # Common rustup location when ~/.cargo/bin is not on PATH.
    fallback = Path.home() / ".cargo" / "bin" / exe
    if fallback.exists():
        return str(fallback)
    raise SystemExit("cargo not found - install Rust 1.85+ or add ~/.cargo/bin to PATH")


def read_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', text, re.M)
    if not m:
        raise SystemExit("cannot read workspace version from Cargo.toml")
    return m.group(1)


def have(tool: str) -> bool:
    return shutil.which(tool) is not None


def linux_cross_command(profile: str) -> tuple[list[str], str] | None:
    """Return (command, out_dir) for the best available Linux cross toolchain."""
    if have("cross"):
        # cross builds inside a container with the Linux sysroot + GTK deps.
        return (
            ["cross", "build", "--profile", profile, "--target", LINUX_TARGET],
            str(ROOT / "target" / LINUX_TARGET / profile),
        )
    if have("cargo-zigbuild"):
        return (
            [
                "cargo",
                "zigbuild",
                "--profile",
                profile,
                "--target",
                LINUX_TARGET,
            ],
            str(ROOT / "target" / LINUX_TARGET / profile),
        )
    return None


def build_host(profile: str) -> Path | None:
    exe_name = "taskman.exe" if platform.system() == "Windows" else "taskman"
    out_dir = ROOT / "target" / ("debug" if profile == "dev" else profile)
    if not run([cargo(), "build", "--profile", profile, "--workspace"]):
        return None
    exe = out_dir / exe_name
    if not exe.exists():
        log(f"host binary missing after build: {exe}")
        return None
    return exe


def build_linux(profile: str) -> tuple[Path | None, bool]:
    """Returns (binary path or None, attempted?)."""
    cmd = linux_cross_command(profile)
    if cmd is None:
        log(
            "linux cross toolchain not found (install `cross` or "
            "`cargo-zigbuild`) - skipping linux artifact"
        )
        return None, False
    command, out_dir = cmd
    if not run(command):
        return None, True
    exe = Path(out_dir) / "taskman"
    if not exe.exists():
        log(f"linux binary missing after build: {exe}")
        return None, True
    return exe, True


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
            tf.add(src, arcname=arc)
    return dest


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--debug", action="store_true", help="build the dev profile instead of release")
    ap.add_argument("--host-only", action="store_true", help="skip the linux cross build")
    ap.add_argument("--linux-only", action="store_true", help="skip the host build")
    ap.add_argument("--no-package", action="store_true", help="build but skip dist/ packaging")
    ap.add_argument(
        "--require-all-targets",
        action="store_true",
        help="fail when the linux build has to be skipped for missing tooling",
    )
    ap.add_argument(
        "--check",
        action="store_true",
        help="run the full quality gate first (fmt, clippy -D warnings, tests)",
    )
    args = ap.parse_args()

    profile = "dev" if args.debug else "release"
    version = read_version()
    log(f"taskman v{version} - profile={profile}")

    if args.check:
        ok = run([cargo(), "fmt", "--all", "--", "--check"])
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
            artifacts.append((f"taskman-v{version}-{host_tag()}", [(exe, arc)]))

    if linux_requested:
        exe, attempted = build_linux(profile)
        if exe is not None:
            log(f"linux binary ready: {exe}")
            artifacts.append((f"taskman-v{version}-linux-x86_64", [(exe, "taskman")]))
        elif attempted:
            failures += 1
        elif args.require_all_targets:
            log("linux build required (--require-all-targets) but no toolchain")
            failures += 1

    if not args.no_package:
        for name, files in artifacts:
            # Windows binaries ship as .zip; everything else as .tar.gz.
            if files[0][0].suffix == ".exe":
                dest = package_zip(name, files)
            else:
                dest = package_tar(name, files)
            log(f"packaged: {dest}")

    if failures:
        log(f"DONE with {failures} failure(s)")
        return 1
    log("DONE")
    return 0


def host_tag() -> str:
    system = platform.system().lower()
    machine = platform.machine().lower()
    arch = {
        "amd64": "x86_64",
        "x86_64": "x86_64",
        "arm64": "aarch64",
        "aarch64": "aarch64",
    }.get(machine, machine)
    return f"{system}-{arch}"


if __name__ == "__main__":
    sys.exit(main())

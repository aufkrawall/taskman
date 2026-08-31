#!/usr/bin/env python3
"""Release build driver for taskman.

Default behavior (`python build.py`): build the host and Linux x86_64 release
(where a cross toolchain is available), then package platform artifacts.
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
LINUX_DESKTOP = ROOT / "packaging" / "linux" / "io.github.aufkrawall.Taskman.desktop"


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
    fallback = Path.home() / ".cargo" / "bin" / exe
    if fallback.exists():
        return str(fallback)
    raise SystemExit("cargo not found - install Rust 1.85+ or add ~/.cargo/bin to PATH")


def release_rustflags(windows_target: bool) -> dict[str, str] | None:
    """Hardening rustflags for release artifacts (security audit F-11-001).

    * ``--remap-path-prefix`` strips the build machine's home directory from
      panic location strings, so shipped binaries do not leak the local user
      name (release profile strips symbols but keeps panic locations).
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
    if windows_target:
        flags.append("-Ccontrol-flow-guard=yes")
    return {"RUSTFLAGS": " ".join(flags)} if flags else None


def read_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', text, re.M)
    if not m:
        raise SystemExit("cannot read workspace version from Cargo.toml")
    return m.group(1)


def have(tool: str) -> bool:
    return shutil.which(tool) is not None


def linux_cross_command(profile: str) -> tuple[list[str], str] | None:
    if have("cross"):
        return (
            ["cross", "build", "--profile", profile, "--target", LINUX_TARGET],
            str(ROOT / "target" / LINUX_TARGET / profile),
        )
    if have("cargo-zigbuild"):
        return (
            ["cargo", "zigbuild", "--profile", profile, "--target", LINUX_TARGET],
            str(ROOT / "target" / LINUX_TARGET / profile),
        )
    return None


def build_host(profile: str) -> Path | None:
    exe_name = "taskman.exe" if platform.system() == "Windows" else "taskman"
    out_dir = ROOT / "target" / ("debug" if profile == "dev" else profile)
    env = release_rustflags(platform.system() == "Windows") if profile != "dev" else None
    if not run([cargo(), "build", "--profile", profile, "--workspace"], env=env):
        return None
    exe = out_dir / exe_name
    if not exe.exists():
        log(f"host binary missing after build: {exe}")
        return None
    return exe


def build_linux(profile: str) -> tuple[Path | None, bool]:
    cmd = linux_cross_command(profile)
    if cmd is None:
        log(
            "linux cross toolchain not found (install `cross` or "
            "`cargo-zigbuild`) - skipping linux artifact"
        )
        return None, False
    command, out_dir = cmd
    if not run(command, env=release_rustflags(False) if profile != "dev" else None):
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
            host_files = [(exe, arc)]
            if platform.system() == "Windows":
                service_exe = exe.with_name("taskman-service.exe")
                if not service_exe.exists():
                    log(f"core service binary missing after build: {service_exe}")
                    failures += 1
                else:
                    host_files.append((service_exe, "taskman-service.exe"))
            if platform.system() == "Linux" and LINUX_DESKTOP.exists():
                host_files.append(
                    (LINUX_DESKTOP, "share/applications/io.github.aufkrawall.Taskman.desktop")
                )
            artifacts.append((f"taskman-v{version}-{host_tag()}", host_files))

    if linux_requested:
        exe, attempted = build_linux(profile)
        if exe is not None:
            log(f"linux binary ready: {exe}")
            files = [(exe, "taskman")]
            if LINUX_DESKTOP.exists():
                files.append(
                    (LINUX_DESKTOP, "share/applications/io.github.aufkrawall.Taskman.desktop")
                )
            artifacts.append((f"taskman-v{version}-linux-x86_64", files))
        elif attempted:
            failures += 1
        elif args.require_all_targets:
            log("linux build required (--require-all-targets) but no toolchain")
            failures += 1

    if not args.no_package:
        for name, files in artifacts:
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

"""Build the Linux release packages: the .deb, the .rpm and the pacman package.

Run this inside a Linux host or a Linux WSL distro. `build.py` packages the
tarballs and the installers itself, but the three distribution packages need
things a Windows host does not have: `makepkg` for the pacman package, and for
the deb/rpm a POSIX filesystem for the payload `install-linux.sh --packaged`
stages (its integrity checks and the mode bits it stamps on the binaries are
POSIX-only). Every release ships all three, so this step is part of the release
procedure, not an extra -- see `llm-wiki/build.md` section "Release artifact set".

The payload is not described here: `install-linux.sh --packaged --root <dir>`
fills a staging root and `tools/distpkg.py` reads it back, exactly like the
in-driver Linux path (`build_distro_packages` in `build.py`), so the tarball,
the pacman package, the .deb and the .rpm cannot disagree about the layout.

Usage, from the checkout (Windows paths work through /mnt/c):

    wsl.exe -d <distro> -- python3 /mnt/c/<checkout>/tools/linux_packages.py

Options:

    --version <v>      package this version instead of the workspace one
    --arch <arch>      only this release architecture (x86_64, arm64; repeatable)
    --no-distro        skip the .deb/.rpm
    --no-arch-package  skip the pacman package
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import pathlib
import platform
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"
RELEASE = "1"
SERVICE_BINARY = "taskman-service"
# release naming -> (target dir, debian arch, rpm arch)
TARGETS = {
    "x86_64": ("x86_64-unknown-linux-gnu", "amd64", "x86_64"),
    "arm64": ("aarch64-unknown-linux-gnu", "arm64", "aarch64"),
}


def log(message: str) -> None:
    print(f"[linux-packages] {message}", flush=True)


def read_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    if not m:
        raise SystemExit("cannot read workspace version from Cargo.toml")
    return m.group(1)


def load_distpkg():
    spec = importlib.util.spec_from_file_location(
        "taskman_distpkg", ROOT / "tools" / "distpkg.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def package_name(version: str, distro: str, arch: str, ext: str) -> str:
    return f"taskman-v{version}-{distro}-{arch}.{ext}"


def stage_payload(tmp: pathlib.Path, binary_dir: pathlib.Path) -> pathlib.Path:
    """Run `install-linux.sh --packaged` and return the staged root.

    The installer refuses a payload whose binaries are not executable, so the
    copies are marked 0755 first: on Windows the cross-built binaries land with
    no execute bit, and inside the distro the /mnt/c mount carries them as-is.
    """
    gui, service = binary_dir / "taskman", binary_dir / SERVICE_BINARY
    for path in (gui, service):
        if not path.is_file():
            raise SystemExit(f"missing Linux binary: {path}")
    payload = tmp / "payload" / "bin"
    payload.mkdir(parents=True)
    stage = tmp / "stage"
    stage.mkdir()
    for src, name in ((gui, "taskman"), (service, SERVICE_BINARY)):
        dest = payload / name
        shutil.copy2(src, dest)
        dest.chmod(0o755)
    subprocess.run(
        [
            "bash",
            str(ROOT / "packaging" / "linux" / "install-linux.sh"),
            "--packaged",
            "--quiet",
            "--root",
            str(stage),
            "--source-dir",
            str(payload.parent),
        ],
        check=True,
    )
    return stage


def write_distro_packages(
    distpkg, version: str, arch: str, binary_dir: pathlib.Path, mtime: int
) -> bool:
    """Write `taskman-v<version>-debian-ubuntu-<arch>.deb` and the Fedora rpm."""
    _target, deb_arch, rpm_arch = TARGETS[arch]
    with tempfile.TemporaryDirectory(prefix=f"taskman-{arch}-") as tmp:
        stage = stage_payload(pathlib.Path(tmp), binary_dir)
        entries = distpkg.stage_tree(stage)
        log(f"{arch}: {len(entries)} entries staged")
        deb = distpkg.build_deb(entries, version, RELEASE, deb_arch, mtime)
        rpm = distpkg.build_rpm(entries, version, RELEASE, rpm_arch, mtime)
    for data, name in (
        (deb, package_name(version, "debian-ubuntu", arch, "deb")),
        (rpm, package_name(version, "fedora", arch, "rpm")),
    ):
        (DIST / name).write_bytes(data)
        log(f"packaged: {DIST / name}")
    return True


def write_arch_package(version: str) -> bool:
    """Run `makepkg` from `packaging/arch` and name the result like the release.

    The PKGBUILD compiles the binaries from this checkout itself, so it is the
    authority on the package contents; this only picks the destination and the
    release file name, exactly like `build_arch_package` in `build.py` does for
    the WSL route a Windows host takes.
    """
    makepkg = shutil.which("makepkg")
    if makepkg is None:
        log("no makepkg on this host: skipping the pacman package")
        return True
    pkgbuild = ROOT / "packaging" / "arch" / "PKGBUILD"
    m = re.search(
        r"^pkgver=(.+)$", pkgbuild.read_text(encoding="utf-8"), re.MULTILINE
    )
    if not m:
        log(f"cannot read pkgver from {pkgbuild}")
        return False
    if m.group(1).strip() != version:
        log(
            f"{pkgbuild} declares pkgver {m.group(1).strip()} but this run packages "
            f"{version} - bump the PKGBUILD and its .SRCINFO"
        )
        return False
    with tempfile.TemporaryDirectory(prefix="taskman-arch-") as tmp:
        env = dict(os.environ)
        env.update(
            {
                "PKGDEST": str(pathlib.Path(tmp) / "pkg"),
                "BUILDDIR": str(pathlib.Path(tmp) / "build"),
                "SRCDEST": str(pathlib.Path(tmp) / "src"),
            }
        )
        if not subprocess.run(
            [makepkg, "--force", "--noconfirm", "--noprogressbar"],
            cwd=ROOT / "packaging" / "arch",
            env=env,
        ).returncode == 0:
            log("makepkg failed")
            return False
        built = sorted((pathlib.Path(tmp) / "pkg").glob("taskman-*-x86_64.pkg.tar.*"))
        built = [p for p in built if not p.name.endswith(".sig")]
        if not built:
            log("makepkg produced no package")
            return False
        suffix = built[-1].name.split(".pkg.tar", 1)[1]
        dest = DIST / package_name(version, "arch", "x86_64", f"pkg.tar{suffix}")
        shutil.copy2(built[-1], dest)
        log(f"packaged: {dest}")
    return True


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Build the .deb/.rpm/pacman release packages (needs a Linux host)"
    )
    parser.add_argument("--version", default=None, help="defaults to Cargo.toml")
    parser.add_argument(
        "--arch",
        action="append",
        choices=sorted(TARGETS),
        help="release architecture to package (default: every one whose binaries exist)",
    )
    parser.add_argument("--no-distro", action="store_true", help="skip .deb/.rpm")
    parser.add_argument(
        "--no-arch-package", action="store_true", help="skip the pacman package"
    )
    args = parser.parse_args(argv)

    if platform.system() != "Linux":
        raise SystemExit(
            "the packages need a Linux host: run this inside a Linux/Arch WSL distro"
        )
    if shutil.which("bash") is None:
        raise SystemExit("bash is required to stage the payload")
    DIST.mkdir(exist_ok=True)
    version = args.version or read_version()
    log(f"taskman v{version} - {platform.system().lower()}")
    mtime = int(os.environ.get("SOURCE_DATE_EPOCH", "0")) or int(
        (ROOT / "Cargo.toml").stat().st_mtime
    )

    ok = True
    if not args.no_distro:
        distpkg = load_distpkg()
        for arch in args.arch or sorted(TARGETS):
            binary_dir = ROOT / "target" / TARGETS[arch][0] / "release"
            if not (binary_dir / "taskman").is_file():
                if args.arch:
                    log(f"{arch}: no binaries in {binary_dir}")
                    ok = False
                else:
                    log(f"{arch}: no binaries in {binary_dir} - skipping")
                continue
            ok &= write_distro_packages(distpkg, version, arch, binary_dir, mtime)
    if not args.no_arch_package:
        ok &= write_arch_package(version)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())

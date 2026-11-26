#!/usr/bin/env python3
"""One-time setup of the project-local Windows cross toolchain.

`python build.py` on Linux/macOS cross-builds the Windows installer when this
toolchain exists (see llm-wiki/build.md). Everything lands in `<checkout>/.cross`
(gitignored) and nothing outside it is touched, so the system partition, `~/.cargo`
and `~/.rustup` stay as they are:

  .cross/rustup/        a private rustup home with the official toolchain
                        whose version equals the host `rustc`, plus the
                        x86_64-pc-windows-msvc standard library
  .cross/cargo-home/    its cargo home: registry, `cargo-xwin`, proxies
  .cross/xwin/          the Microsoft CRT and Windows SDK import libraries

Why a private rustup instead of the distribution's `rust`: a distribution
build stamps its own version string into every crate's metadata, so the
official `rust-std` for the same release is rejected by it, and the system
sysroot is root-owned in any case.

The Microsoft CRT/SDK download (about 1 GB) is subject to Microsoft's license
terms (https://go.microsoft.com/fwlink/?LinkId=2086102 and the Visual Studio
license), so it only happens with --accept-microsoft-license.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CROSS = ROOT / ".cross"
TARGET = "x86_64-pc-windows-msvc"


def log(message: str) -> None:
    print(f"[setup-windows-cross] {message}", flush=True)


def host_triple_and_release() -> tuple[str, str]:
    out = subprocess.run(
        ["rustc", "-vV"], capture_output=True, text=True, check=True
    ).stdout
    host = re.search(r"^host: (\S+)$", out, re.M)
    release = re.search(r"^release: (\S+)$", out, re.M)
    if not host or not release:
        raise SystemExit("cannot read the host triple/version from `rustc -vV`")
    return host.group(1), release.group(1)


def download(url: str, dest: Path) -> None:
    with urllib.request.urlopen(url, timeout=60) as response:
        dest.write_bytes(response.read())


def fetch_rustup_init(host: str) -> Path:
    """Download rustup-init and verify the sha256 the same server publishes."""
    base = f"https://static.rust-lang.org/rustup/dist/{host}/rustup-init"
    dl = CROSS / "dl"
    dl.mkdir(parents=True, exist_ok=True)
    exe = dl / "rustup-init"
    sums = dl / "rustup-init.sha256"
    download(base, exe)
    download(base + ".sha256", sums)
    expected = sums.read_text().split()[0].lower()
    actual = hashlib.sha256(exe.read_bytes()).hexdigest()
    if actual != expected:
        exe.unlink()
        raise SystemExit(f"rustup-init sha256 mismatch: {actual} != {expected}")
    exe.chmod(0o755)
    return exe


def run(cmd: list[str], env: dict[str, str]) -> None:
    print(f"$ {' '.join(cmd)}", flush=True)
    subprocess.run(cmd, check=True, env=env, cwd=ROOT)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument(
        "--accept-microsoft-license",
        action="store_true",
        help="accept Microsoft's terms for the CRT/Windows SDK download (required)",
    )
    args = ap.parse_args()
    if not args.accept_microsoft_license:
        log("the Microsoft CRT/SDK download needs --accept-microsoft-license; see --help")
        return 2

    host, release = host_triple_and_release()
    cargo_home = CROSS / "cargo-home"
    rustup_home = CROSS / "rustup"
    env = {
        **os.environ,
        "RUSTUP_HOME": str(rustup_home),
        "CARGO_HOME": str(cargo_home),
        "RUSTUP_TOOLCHAIN": release,
        "PATH": f"{cargo_home / 'bin'}{os.pathsep}{os.environ.get('PATH', '')}",
    }
    for stale in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR"):
        env.pop(stale, None)
    rustup = cargo_home / "bin" / "rustup"

    if not rustup.exists():
        log(f"installing a private rustup into {CROSS}")
        init = fetch_rustup_init(host)
        run(
            [str(init), "-y", "--no-modify-path", "--profile", "minimal",
             "--default-toolchain", "none"],
            env,
        )
    log(f"installing Rust {release} with the {TARGET} standard library")
    run([str(rustup), "toolchain", "install", release, "--profile", "minimal",
         "--target", TARGET], env)

    xwin = cargo_home / "bin" / "cargo-xwin"
    if not xwin.exists():
        log("installing cargo-xwin")
        build_dir = CROSS / "install-target"
        run(["cargo", "install", "cargo-xwin", "--locked"],
            {**env, "CARGO_TARGET_DIR": str(build_dir)})
        shutil.rmtree(build_dir, ignore_errors=True)

    log("caching the Microsoft CRT and Windows SDK (x86_64 only)")
    run([str(xwin), "xwin", "cache", "xwin", "--xwin-arch", "x86_64"],
        {**env, "XWIN_CACHE_DIR": str(CROSS / "xwin")})
    shutil.rmtree(CROSS / "dl", ignore_errors=True)
    log("done: `python build.py` will now cross-build the Windows installer")
    return 0


if __name__ == "__main__":
    sys.exit(main())

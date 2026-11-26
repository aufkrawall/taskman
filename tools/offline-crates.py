#!/usr/bin/env python3
"""Run a cargo/build.py command when crates.io is unreachable for cargo.

The agent sandbox blocks Schannel TLS, which is the backend Windows `cargo` uses,
so a dependency the local registry is missing fails twice over: a plain build dies
at "Updating crates.io index", and `--offline` dies because the sparse index cache
has no entry for that version (this repo hits it with the `resvg` tree, which
`tm-platform` added for SVG icons). Python's HTTPS is allowed in the same sandbox,
so this script bridges exactly that gap:

1. diff `Cargo.lock` against `~/.cargo/registry/src` and list what is missing;
2. download each missing `.crate` from static.crates.io and verify the checksum
   `Cargo.lock` records, unpacking into `<target dir>/agent-crates/`;
3. add a temporary `[patch.crates-io]` block pointing those names at the unpacked
   sources;
4. run the command you pass after `--`;
5. restore `Cargo.toml` and `Cargo.lock` byte for byte — on success, on failure,
   and on Ctrl-C — so nothing it changes can be committed.

Use a normal `cargo`/`build.py` run wherever the machine has crates.io access;
this exists for the sandboxed dev environment. The crates are checksum-identical to
the ones a normal build would use, because the lock file's sha256 is enforced.

    python tools/offline-crates.py -- cargo test -p tm-platform -p tm-app
    python tools/offline-crates.py -- cargo clippy --workspace --all-targets -- -D warnings
    python tools/offline-crates.py -- python build.py --host-only

On this project the Linux half is better built inside WSL, which has its own TLS
stack and therefore real crates.io access: `wsl.exe -d <distro> -- bash -lc 'cd
/mnt/c/<checkout> && cargo test -p tm-platform'`. See llm-wiki/build.md.
"""

from __future__ import annotations

import atexit
import hashlib
import re
import signal
import subprocess
import sys
import tarfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOCK = ROOT / "Cargo.lock"
MANIFEST = ROOT / "Cargo.toml"
# Under `target/` on purpose: git-ignored, and writable from a worktree sandbox.
STAGING = ROOT / "target" / "agent-crates"
MARKER = "\n[patch.crates-io]\n"


def local_registry_sources() -> set[str]:
    """`name-version` of every crate already unpacked in the local registry."""
    src = Path.home() / ".cargo" / "registry" / "src"
    names: set[str] = set()
    if not src.is_dir():
        return names
    for index_dir in src.iterdir():
        if index_dir.is_dir():
            names.update(entry.name for entry in index_dir.iterdir() if entry.is_dir())
    return names


def workspace_members() -> set[str]:
    """Names of this workspace's own packages: never fetched, never patched."""
    members: set[str] = set()
    for manifest in [MANIFEST, *ROOT.glob("crates/*/Cargo.toml")]:
        if not manifest.is_file():
            continue
        in_package = False
        for line in manifest.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if stripped.startswith("["):
                in_package = stripped == "[package]"
                continue
            if in_package and (match := re.match(r'name\s*=\s*"([^"]+)"', stripped)):
                members.add(match.group(1))
    return members


def locked_crates() -> dict[tuple[str, str], str | None]:
    """`(name, version) -> checksum` for every registry package Cargo.lock pins."""
    pinned: dict[tuple[str, str], str | None] = {}
    for block in LOCK.read_text(encoding="utf-8").split("[[package]]"):
        name = re.search(r'name = "([^"]+)"', block)
        version = re.search(r'version = "([^"]+)"', block)
        if not (name and version):
            continue
        if 'source = "registry' not in block:
            continue
        checksum = re.search(r'checksum = "([0-9a-f]{64})"', block)
        pinned[(name.group(1), version.group(1))] = checksum.group(1) if checksum else None
    return pinned


def fetch(name: str, version: str, checksum: str | None) -> Path:
    """Download and unpack one crate; return its source directory."""
    OUT = STAGING / "crates"
    SRC = STAGING / "src"
    OUT.mkdir(parents=True, exist_ok=True)
    SRC.mkdir(parents=True, exist_ok=True)
    crate = OUT / f"{name}-{version}.crate"
    if not crate.exists():
        url = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
        print(f"fetching {name} {version}", flush=True)
        with urllib.request.urlopen(url, timeout=120) as response:
            crate.write_bytes(response.read())
    if checksum is not None:
        digest = hashlib.sha256(crate.read_bytes()).hexdigest()
        if digest != checksum:
            raise SystemExit(
                f"{name} {version}: checksum {digest} does not match Cargo.lock"
            )
    unpacked = SRC / f"{name}-{version}"
    if not unpacked.exists():
        with tarfile.open(crate) as archive:
            archive.extractall(SRC, filter="data")
    return unpacked


def restore_manifest(original: bytes, lock_original: bytes) -> None:
    MANIFEST.write_bytes(original)
    LOCK.write_bytes(lock_original)


def main(argv: list[str]) -> int:
    if "--" not in argv:
        raise SystemExit("usage: offline-crates.py -- <command …>")
    command = argv[argv.index("--") + 1 :]
    if not command:
        raise SystemExit("no command after --")

    have = local_registry_sources()
    members = workspace_members()
    missing = [
        (name, version, checksum)
        for (name, version), checksum in sorted(locked_crates().items())
        if name not in members and f"{name}-{version}" not in have
    ]
    if not missing:
        # Nothing to bridge: behave like a plain run so the tool never hides a
        # network failure by pretending it did something.
        return subprocess.run(command, cwd=ROOT).returncode

    print(
        "local registry is missing "
        + ", ".join(f"{n} {v}" for n, v, _ in missing),
        flush=True,
    )
    sources = {f"{n}-{v}": fetch(n, v, c) for n, v, c in missing}

    original = MANIFEST.read_bytes()
    lock_original = LOCK.read_bytes()
    text = original.decode("utf-8")
    if text.count(MARKER) != 1:
        raise SystemExit(f"Cargo.toml does not have exactly one {MARKER!r} header")
    patch = MARKER
    for crate in sources:
        name = crate.rsplit("-", 1)[0]
        relative = (STAGING / "src" / crate).relative_to(ROOT).as_posix()
        patch += f'{name} = {{ path = "{relative}" }}\n'

    # Restore on every exit path: a killed run must not leave a patched manifest
    # behind, because that is a source change nobody reviewed.
    atexit.register(restore_manifest, original, lock_original)

    def on_signal(signum, _frame):
        restore_manifest(original, lock_original)
        sys.exit(128 + signum)

    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, on_signal)

    MANIFEST.write_text(text.replace(MARKER, patch), encoding="utf-8")
    print(f"patched {len(sources)} crate(s); running: {' '.join(command)}", flush=True)
    code = subprocess.run(command, cwd=ROOT).returncode
    restore_manifest(original, lock_original)
    print("manifest and lock restored", flush=True)
    return code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

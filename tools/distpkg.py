"""Pure-Python writers for the Debian/Ubuntu (.deb) and Fedora (.rpm) packages.

Why not dpkg-deb / rpmbuild: `build.py` runs on Windows and on Arch hosts, where
neither tool exists, and both formats are simple enough to emit directly. The
payload is NOT described here: `stage_tree()` receives a directory that
`packaging/linux/install-linux.sh --packaged --root <dir>` already filled, so the
packages, the tarball and the pacman package cannot disagree about the layout.

Both writers are deterministic for a fixed `mtime`.
"""

from __future__ import annotations

import gzip
import hashlib
import io
import lzma
import os
import stat
import struct
import tarfile
from pathlib import Path

HOMEPAGE = "https://github.com/aufkrawall/taskman"
MAINTAINER = "aufkrawall <aufkrawall@users.noreply.github.com>"
SUMMARY = "Cross-platform task manager and system monitor"
DESCRIPTION = (
    "TaskMan is a task manager and system monitor. The package installs the GUI "
    "and the protected core service (taskman-core.service) that performs "
    "privileged actions on behalf of authorized desktop users."
)
LICENSE = "MIT"
# Staged directories the package owns. System directories (/usr, /etc, ...)
# belong to the distribution and must never be listed by this package.
OWNED_DIRS = ("etc/taskman", "usr/lib/taskman")
# Files the live system may edit; the package must not clobber them on upgrade.
CONFIG_FILES = ("etc/taskman/core-service.json",)
LIB_DIR = "/usr/lib/taskman"

# Libraries the window backend dlopens (see packaging/arch/PKGBUILD): nothing
# links them, so no automatic dependency generator could find them.
DEB_DEPENDS = (
    "libc6 (>= 2.17)",
    "libx11-6",
    "libx11-xcb1",
    "libxcursor1",
    "libxi6",
    "libxkbcommon0",
    "libxkbcommon-x11-0",
    "libwayland-client0",
    "systemd",
)
# Sonames instead of package names: Fedora/RHEL/openSUSE split these libraries
# differently, a soname capability resolves on all of them.
RPM_REQUIRES = (
    "libX11.so.6()(64bit)",
    "libX11-xcb.so.1()(64bit)",
    "libXcursor.so.1()(64bit)",
    "libXi.so.6()(64bit)",
    "libxkbcommon.so.0()(64bit)",
    "libxkbcommon-x11.so.0()(64bit)",
    "libwayland-client.so.0()(64bit)",
    "systemd",
)

DEB_POSTINST = f"""#!/bin/sh
set -e
case "$1" in
  configure)
    # Authorizes the installing user, restarts the core service and relaunches
    # a running GUI so an upgrade takes effect immediately.
    {LIB_DIR}/install-linux.sh --post-install || \\
      echo "taskman: post-install step failed; run {LIB_DIR}/install-linux.sh --post-install" >&2
    ;;
esac
exit 0
"""
DEB_PRERM = """#!/bin/sh
set -e
case "$1" in
  remove|deconfigure)
    systemctl stop taskman-core.service 2>/dev/null || true
    systemctl disable taskman-core.service 2>/dev/null || true
    ;;
esac
exit 0
"""
DEB_POSTRM = """#!/bin/sh
set -e
case "$1" in
  remove|purge)
    systemctl daemon-reload 2>/dev/null || true
    ;;
esac
if [ "$1" = purge ]; then
  rm -f /etc/taskman/core-service.json.bak
  rmdir /etc/taskman 2>/dev/null || true
fi
exit 0
"""
RPM_POST = f"""{LIB_DIR}/install-linux.sh --post-install || :
"""
RPM_PREUN = """if [ "$1" -eq 0 ]; then
  systemctl stop taskman-core.service 2>/dev/null || :
  systemctl disable taskman-core.service 2>/dev/null || :
fi
"""
RPM_POSTUN = """systemctl daemon-reload 2>/dev/null || :
"""


class Entry:
    """One package member. A plain class: this module is loaded by path."""

    def __init__(self, path, kind, mode, data=b"", target="", config=False):
        self.path = path  # no leading slash
        self.kind = kind  # "file" | "dir" | "link"
        self.mode = mode
        self.data = data
        self.target = target
        self.config = config


def stage_tree(root: Path) -> list[Entry]:
    """Read a staged install root into sorted package entries.

    Only owned directories are returned; modes are normalised so a hostile umask
    on the build machine cannot leak into the package.
    """
    entries: list[Entry] = []
    for d in OWNED_DIRS:
        if not (root / d).is_dir():
            raise ValueError(f"staged tree lacks owned directory {d}")
        entries.append(Entry(d, "dir", 0o755))
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root).as_posix()
        if path.is_symlink():
            entries.append(Entry(rel, "link", 0o777, target=os.readlink(path)))
        elif path.is_file():
            mode = 0o755 if path.stat().st_mode & stat.S_IXUSR else 0o644
            entries.append(
                Entry(rel, "file", mode, path.read_bytes(), config=rel in CONFIG_FILES)
            )
    if not any(e.kind == "file" for e in entries):
        raise ValueError(f"staged tree {root} contains no files")
    entries.sort(key=lambda e: e.path)
    return entries


def installed_size_kib(entries: list[Entry]) -> int:
    return max(1, (sum(len(e.data) for e in entries) + 1023) // 1024)


# --------------------------------------------------------------------------- deb


def _tar_gz_or_xz(entries: list[Entry], mtime: int, fmt: str) -> bytes:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tar:
        for e in entries:
            info = tarfile.TarInfo("./" + e.path + ("/" if e.kind == "dir" else ""))
            info.mtime = mtime
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mode = e.mode
            if e.kind == "dir":
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
            elif e.kind == "link":
                info.type = tarfile.SYMTYPE
                info.linkname = e.target
                tar.addfile(info)
            else:
                info.size = len(e.data)
                tar.addfile(info, io.BytesIO(e.data))
    data = raw.getvalue()
    if fmt == "xz":
        return lzma.compress(data, preset=6, check=lzma.CHECK_CRC32)
    return gzip.compress(data, mtime=0)


def _ar(members: list[tuple[str, bytes]], mtime: int) -> bytes:
    out = bytearray(b"!<arch>\n")
    for name, data in members:
        header = f"{name:<16}{mtime:<12}{0:<6}{0:<6}{'100644':<8}{len(data):<10}`\n"
        out += header.encode("ascii") + data
        if len(data) % 2:
            out += b"\n"
    return bytes(out)


def build_deb(
    entries: list[Entry], version: str, release: str, arch: str, mtime: int
) -> bytes:
    """`arch` is the Debian architecture name (amd64, arm64)."""
    md5s = "".join(
        f"{hashlib.md5(e.data).hexdigest()}  {e.path}\n"  # noqa: S324 - dpkg format
        for e in entries
        if e.kind == "file"
    )
    conffiles = "".join(f"/{c}\n" for c in CONFIG_FILES)
    long_desc = "\n".join(" " + line for line in [DESCRIPTION])
    control = (
        "Package: taskman\n"
        f"Version: {version}-{release}\n"
        f"Architecture: {arch}\n"
        f"Maintainer: {MAINTAINER}\n"
        f"Installed-Size: {installed_size_kib(entries)}\n"
        f"Depends: {', '.join(DEB_DEPENDS)}\n"
        "Section: utils\n"
        "Priority: optional\n"
        f"Homepage: {HOMEPAGE}\n"
        f"Description: {SUMMARY}\n{long_desc}\n"
    )
    ctl_entries = [
        Entry("control", "file", 0o644, control.encode()),
        Entry("md5sums", "file", 0o644, md5s.encode()),
        Entry("conffiles", "file", 0o644, conffiles.encode()),
        Entry("postinst", "file", 0o755, DEB_POSTINST.encode()),
        Entry("prerm", "file", 0o755, DEB_PRERM.encode()),
        Entry("postrm", "file", 0o755, DEB_POSTRM.encode()),
    ]
    control_tar = _tar_gz_or_xz(ctl_entries, mtime, "gz")
    data_tar = _tar_gz_or_xz(entries, mtime, "xz")
    return _ar(
        [
            ("debian-binary", b"2.0\n"),
            ("control.tar.gz", control_tar),
            ("data.tar.xz", data_tar),
        ],
        mtime,
    )


# --------------------------------------------------------------------------- rpm

STRING, BIN, STRING_ARRAY, I18NSTRING, INT16, INT32 = 6, 7, 8, 9, 3, 4
HEADER_MAGIC = b"\x8e\xad\xe8\x01\x00\x00\x00\x00"


def _encode(typ: int, value) -> tuple[bytes, int, int]:
    """Return (bytes, count, alignment)."""
    if typ in (STRING, I18NSTRING):
        return value.encode() + b"\0", 1, 1
    if typ == STRING_ARRAY:
        return b"".join(v.encode() + b"\0" for v in value), len(value), 1
    if typ == BIN:
        return bytes(value), len(value), 1
    if typ == INT16:
        return struct.pack(f">{len(value)}H", *value), len(value), 2
    if typ == INT32:
        return struct.pack(f">{len(value)}I", *value), len(value), 4
    raise ValueError(typ)


def _header(entries: list[tuple[int, int, object]], region_tag: int) -> bytes:
    store = bytearray()
    index: list[tuple[int, int, int, int]] = []
    for tag, typ, value in sorted(entries, key=lambda e: e[0]):
        blob, count, align = _encode(typ, value)
        while len(store) % align:
            store.append(0)
        index.append((tag, typ, len(store), count))
        store += blob
    nindex = len(index) + 1  # the region entry counts
    region = (region_tag, BIN, len(store), 16)
    store += struct.pack(">IIiI", region_tag, BIN, -(nindex * 16), 16)
    index.insert(0, region)
    body = b"".join(struct.pack(">IIII", *i) for i in index)
    return HEADER_MAGIC + struct.pack(">II", nindex, len(store)) + body + bytes(store)


def _cpio(entries: list[Entry], inodes: dict[str, int], mtime: int) -> bytes:
    out = bytearray()

    def pad(n: int) -> None:
        out.extend(b"\0" * (-n % 4))

    for e in entries:
        if e.kind == "dir":
            continue  # rpm creates directories from the header, not the payload
        data = e.data if e.kind == "file" else e.target.encode()
        mode = (stat.S_IFLNK if e.kind == "link" else stat.S_IFREG) | e.mode
        name = ("./" + e.path).encode() + b"\0"
        out += b"070701" + b"".join(
            f"{v:08x}".encode()
            for v in (
                inodes[e.path], mode, 0, 0, 1, mtime, len(data), 0, 0, 0, 0,
                len(name), 0,
            )
        )
        out += name
        pad(110 + len(name))
        out += data
        pad(len(data))
    trailer = b"TRAILER!!!\0"
    out += b"070701" + b"".join(
        f"{v:08x}".encode() for v in (0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, len(trailer), 0)
    )
    out += trailer
    pad(110 + len(trailer))
    return bytes(out)


def build_rpm(
    entries: list[Entry], version: str, release: str, arch: str, mtime: int
) -> bytes:
    """`arch` is the RPM architecture name (x86_64, aarch64)."""
    ents = sorted(entries, key=lambda e: e.path)
    inodes = {e.path: i + 1 for i, e in enumerate(ents)}
    dirnames: list[str] = []
    dirindex, basenames = [], []
    for e in ents:
        d, _, b = ("/" + e.path).rpartition("/")
        d += "/"
        if d not in dirnames:
            dirnames.append(d)
        dirindex.append(dirnames.index(d))
        basenames.append(b)
    ifmt = {"dir": stat.S_IFDIR, "file": stat.S_IFREG, "link": stat.S_IFLNK}
    n = len(ents)
    requires = list(RPM_REQUIRES) + [
        "rpmlib(PayloadFilesHavePrefix)",
        "rpmlib(CompressedFileNames)",
    ]
    rpmlib = 0x1000000 | 0x02 | 0x08  # RPMLIB | LESS | EQUAL
    req_flags = [0] * len(RPM_REQUIRES) + [rpmlib, rpmlib]
    req_vers = [""] * len(RPM_REQUIRES) + ["4.0-1", "3.0.4-1"]
    total = sum(len(e.data) for e in ents)
    payload_raw = _cpio(ents, inodes, mtime)
    payload = gzip.compress(payload_raw, compresslevel=9, mtime=0)

    main = _header(
        [
            (100, STRING_ARRAY, ["C"]),
            (1000, STRING, "taskman"),
            (1001, STRING, version),
            (1002, STRING, release),
            (1004, I18NSTRING, SUMMARY),
            (1005, I18NSTRING, DESCRIPTION),
            (1006, INT32, [mtime]),
            (1007, STRING, "taskman-build"),
            (1009, INT32, [total]),
            (1014, STRING, LICENSE),
            (1016, I18NSTRING, "Applications/System"),
            (1021, STRING, "linux"),
            (1022, STRING, arch),
            (1024, STRING, RPM_POST),
            (1025, STRING, RPM_PREUN),
            (1026, STRING, RPM_POSTUN),
            (1028, INT32, [len(e.data) if e.kind == "file" else (4096 if e.kind == "dir" else len(e.target)) for e in ents]),
            (1030, INT16, [ifmt[e.kind] | e.mode for e in ents]),
            (1033, INT16, [0] * n),
            (1034, INT32, [mtime] * n),
            (1035, STRING_ARRAY, [hashlib.sha256(e.data).hexdigest() if e.kind == "file" else "" for e in ents]),
            (1036, STRING_ARRAY, [e.target for e in ents]),
            (1037, INT32, [17 if e.config else 0 for e in ents]),
            (1039, STRING_ARRAY, ["root"] * n),
            (1040, STRING_ARRAY, ["root"] * n),
            (1045, INT32, [0xFFFFFFFF] * n),
            (1047, STRING_ARRAY, ["taskman", f"taskman({arch})"]),
            (1048, INT32, req_flags),
            (1049, STRING_ARRAY, requires),
            (1050, STRING_ARRAY, req_vers),
            (1064, STRING, "4.0.0"),
            (1086, STRING, "/bin/sh"),
            (1087, STRING, "/bin/sh"),
            (1088, STRING, "/bin/sh"),
            (1095, INT32, [1] * n),
            (1096, INT32, [inodes[e.path] for e in ents]),
            (1097, STRING_ARRAY, [""] * n),
            (1112, INT32, [8, 8]),
            (1113, STRING_ARRAY, [f"{version}-{release}"] * 2),
            (1116, INT32, dirindex),
            (1117, STRING_ARRAY, basenames),
            (1118, STRING_ARRAY, dirnames),
            (1124, STRING, "cpio"),
            (1125, STRING, "gzip"),
            (1126, STRING, "9"),
            (5011, INT32, [8]),  # file digest algorithm: SHA-256
        ],
        63,
    )
    sig = _header(
        [
            (273, STRING, hashlib.sha256(main).hexdigest()),
            (1000, INT32, [len(main) + len(payload)]),
            (1004, BIN, hashlib.md5(main + payload).digest()),  # noqa: S324
            (1007, INT32, [len(payload_raw)]),
        ],
        62,
    )
    sig += b"\0" * (-len(sig) % 8)
    lead = struct.pack(">IBBhh", 0xEDABEEDB, 3, 0, 0, 1 if arch == "x86_64" else 12)
    lead += f"taskman-{version}-{release}".encode().ljust(66, b"\0")
    lead += struct.pack(">hh", 1, 5) + b"\0" * 16
    assert len(lead) == 96
    return lead + sig + main + payload

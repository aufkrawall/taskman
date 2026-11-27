"""The .deb/.rpm writers: structure is checked with the tools that ship here."""

import hashlib
import importlib.util
import io
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("distpkg", ROOT / "tools" / "distpkg.py")
distpkg = importlib.util.module_from_spec(spec)
spec.loader.exec_module(distpkg)


def staged(tmp: Path) -> Path:
    payload, stage = tmp / "payload", tmp / "stage"
    (payload / "bin").mkdir(parents=True)
    stage.mkdir()
    for name in ("taskman", "taskman-service"):
        (payload / "bin" / name).write_bytes(b"\x7fELF" + name.encode())
        (payload / "bin" / name).chmod(0o755)
    subprocess.run(
        [
            "bash", str(ROOT / "packaging/linux/install-linux.sh"), "--packaged",
            "--quiet", "--root", str(stage), "--source-dir", str(payload),
        ],
        check=True,
    )
    return stage


@unittest.skipUnless(shutil.which("bash") and os.name == "posix", "needs POSIX bash")
class DistPackages(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.entries = distpkg.stage_tree(staged(Path(cls.tmp.name)))

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_entries_exclude_system_dirs_and_include_payload(self):
        paths = {e.path for e in self.entries}
        self.assertIn("usr/bin/taskman", paths)
        self.assertIn("usr/lib/taskman/taskman-service", paths)
        self.assertIn("usr/lib/systemd/system/taskman-core.service", paths)
        self.assertIn("etc/taskman/core-service.json", paths)
        self.assertNotIn("usr", paths)
        self.assertNotIn("etc", paths)

    def test_deb_layout(self):
        deb = distpkg.build_deb(self.entries, "1.2.3", "1", "amd64", 1_700_000_000)
        self.assertTrue(deb.startswith(b"!<arch>\n"))
        members, pos = {}, 8
        while pos < len(deb):
            name = deb[pos : pos + 16].decode().strip()
            size = int(deb[pos + 48 : pos + 58])
            members[name] = deb[pos + 60 : pos + 60 + size]
            pos += 60 + size + (size % 2)
        self.assertEqual(
            list(members), ["debian-binary", "control.tar.gz", "data.tar.xz"]
        )
        self.assertEqual(members["debian-binary"], b"2.0\n")
        ctl = tarfile.open(fileobj=io.BytesIO(members["control.tar.gz"]))
        control = ctl.extractfile("./control").read().decode()
        self.assertIn("Package: taskman\nVersion: 1.2.3-1\nArchitecture: amd64\n", control)
        self.assertEqual(
            ctl.extractfile("./conffiles").read(), b"/etc/taskman/core-service.json\n"
        )
        postinst = ctl.extractfile("./postinst").read().decode()
        self.assertIn("install-linux.sh --post-install", postinst)
        self.assertEqual(ctl.getmember("./postinst").mode, 0o755)
        data = tarfile.open(fileobj=io.BytesIO(members["data.tar.xz"]))
        self.assertIn("./usr/bin/taskman", data.getnames())
        self.assertEqual(data.getmember("./usr/bin/taskman").uname, "root")

    def test_rpm_structure(self):
        rpm = distpkg.build_rpm(self.entries, "1.2.3", "1", "x86_64", 1_700_000_000)
        self.assertEqual(rpm[:4], b"\xed\xab\xee\xdb")
        pos = 96
        # Signature header, then main header; both start with the header magic.
        self.assertEqual(rpm[pos : pos + 8], distpkg.HEADER_MAGIC)
        nidx, dsize = struct.unpack(">II", rpm[pos + 8 : pos + 16])
        sig_end = pos + 16 + nidx * 16 + dsize
        sig_end += -sig_end % 8
        self.assertEqual(rpm[sig_end : sig_end + 8], distpkg.HEADER_MAGIC)
        nidx2, dsize2 = struct.unpack(">II", rpm[sig_end + 8 : sig_end + 16])
        main_end = sig_end + 16 + nidx2 * 16 + dsize2
        main = rpm[sig_end:main_end]
        payload = rpm[main_end:]
        self.assertEqual(payload[:2], b"\x1f\x8b")
        # The signature header must carry the digest of the main header.
        sig = rpm[pos:sig_end]
        self.assertIn(hashlib.sha256(main).hexdigest().encode(), sig)
        self.assertIn(hashlib.md5(main + payload).digest(), sig)
        self.assertIn(b"/usr/lib/taskman/install-linux.sh --post-install", main)
        bsdtar = shutil.which("bsdtar")
        if bsdtar:
            with tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "t.rpm"
                path.write_bytes(rpm)
                listing = subprocess.run(
                    [bsdtar, "-tf", str(path)], check=True, capture_output=True, text=True
                ).stdout
                self.assertIn("./usr/bin/taskman", listing)


if __name__ == "__main__":
    unittest.main()

"""Setup packaging must never leave a broken or stale installer in dist/."""

import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "taskman_build", Path(__file__).resolve().parents[2] / "build.py"
)
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class PackageSetupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.dist = root / "dist"
        self.out_dir = root / "target" / "release"
        self.out_dir.mkdir(parents=True)
        self.gui = self.out_dir / "taskman.exe"
        self.service = self.out_dir / "taskman-service.exe"
        for path in (
            self.gui,
            self.service,
            self.out_dir / "taskman-setup.exe",
            self.out_dir / "taskman-payload.exe",
        ):
            path.write_bytes(b"MZ")
        self.dest = self.dist / "taskman-v1.2.3-windows-x86_64-setup.exe"
        patches = [
            patch.object(build, "ROOT", root),
            patch.object(build, "DIST", self.dist),
            patch.object(build, "log", lambda _msg: None),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)
        self.addCleanup(self.tmp.cleanup)

    def fake_tool(self, embed_ok=True, verify_ok=True, partial=b"broken"):
        """Stand-in for `taskman-payload`: records commands, writes output."""
        self.commands = []

        def run(command, env=None):
            self.commands.append(command)
            if command[1] == "verify":
                return verify_ok
            # The embed step writes its output even when it then fails, the
            # way a truncated write would.
            Path(command[2]).write_bytes(b"SETUP+PAYLOAD" if embed_ok else partial)
            return embed_ok

        return patch.object(build, "run", side_effect=run)

    def package(self):
        return build.package_setup(
            "release", self.gui, self.service, "windows-x86_64", "1.2.3"
        )

    def leftovers(self):
        return sorted(p.name for p in self.out_dir.glob("*.partial"))

    def test_verified_artifact_is_moved_into_dist(self):
        with self.fake_tool():
            self.assertEqual(self.package(), self.dest)
        self.assertEqual(self.dest.read_bytes(), b"SETUP+PAYLOAD")
        self.assertEqual(self.leftovers(), [])
        embed, verify = self.commands
        staged = Path(embed[2])
        self.assertEqual(staged.parent, self.out_dir, "embed must not write into dist/")
        self.assertEqual(Path(verify[2]), staged, "verify the staged file, not dist/")

    def test_failed_embed_leaves_no_broken_or_stale_setup(self):
        self.dist.mkdir()
        self.dest.write_bytes(b"previous build")
        with self.fake_tool(embed_ok=False):
            self.assertIsNone(self.package())
        self.assertFalse(self.dest.exists())
        self.assertEqual(self.leftovers(), [])

    def test_failed_verify_never_reaches_dist(self):
        self.dist.mkdir()
        self.dest.write_bytes(b"previous build")
        with self.fake_tool(verify_ok=False):
            self.assertIsNone(self.package())
        self.assertFalse(self.dest.exists())
        self.assertEqual(self.leftovers(), [])

    def test_missing_payload_tool_removes_stale_setup(self):
        (self.out_dir / "taskman-payload.exe").unlink()
        self.dist.mkdir()
        self.dest.write_bytes(b"previous build")
        with self.fake_tool():
            self.assertIsNone(self.package())
        self.assertEqual(self.commands, [])
        self.assertFalse(self.dest.exists())


if __name__ == "__main__":
    unittest.main()

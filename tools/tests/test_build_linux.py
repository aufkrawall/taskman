"""Linux cross-toolchain resolution, artifact cleanup, and shell checks."""

import importlib.util
import os
from pathlib import Path
import platform
import subprocess
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "taskman_build", Path(__file__).resolve().parents[2] / "build.py"
)
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class LinuxBuildToolingTests(unittest.TestCase):
    def test_find_zig_uses_ziglang_package_when_not_on_path(self):
        exe_name = "zig.exe" if platform.system() == "Windows" else "zig"
        with tempfile.TemporaryDirectory() as tmp:
            pkg = Path(tmp) / "ziglang"
            pkg.mkdir()
            init_py = pkg / "__init__.py"
            init_py.write_text("", encoding="utf-8")
            zig_bin = pkg / exe_name
            zig_bin.write_bytes(b"")

            fake_spec = SimpleNamespace(
                origin=str(init_py),
                submodule_search_locations=[str(pkg)],
            )
            with (
                patch.dict(os.environ, {"ZIG": ""}, clear=False),
                patch.object(build.shutil, "which", return_value=None),
                patch.object(build.importlib.util, "find_spec", return_value=fake_spec),
            ):
                self.assertEqual(build.find_zig(), zig_bin)

    def test_linux_cross_command_prefers_zigbuild_when_zig_resolved_off_path(self):
        with (
            patch.object(
                build,
                "find_cargo_tool",
                side_effect=lambda name: "/bin/cargo-zigbuild"
                if name == "cargo-zigbuild"
                else None,
            ),
            patch.object(build, "find_zig", return_value=Path("/opt/ziglang/zig")),
            patch.object(build, "cargo", return_value="cargo"),
        ):
            cmd = build.linux_cross_command("release")

        self.assertIsNotNone(cmd)
        command, out_dir = cmd
        self.assertEqual(
            command,
            [
                "cargo",
                "zigbuild",
                "--profile",
                "release",
                "--target",
                "x86_64-unknown-linux-gnu.2.17",
            ],
        )
        self.assertTrue(out_dir.endswith(str(Path("x86_64-unknown-linux-gnu") / "release")))

    def test_clean_stale_linux_artifacts_removes_only_opposite_flavor(self):
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            gnu = dist / "taskman-v1.2.3-linux-x86_64.tar.gz"
            gnu_sha = dist / "taskman-v1.2.3-linux-x86_64.tar.gz.sha256"
            musl = dist / "taskman-v1.2.3-linux-x86_64-musl.tar.gz"
            musl_sha = dist / "taskman-v1.2.3-linux-x86_64-musl.tar.gz.sha256"

            for path in (gnu, gnu_sha, musl, musl_sha):
                path.write_bytes(b"test")

            with (
                patch.object(build, "DIST", dist),
                patch.object(build, "log", lambda _msg: None),
            ):
                build.clean_stale_linux_artifacts("1.2.3", "gnu")
                self.assertTrue(gnu.exists())
                self.assertTrue(gnu_sha.exists())
                self.assertFalse(musl.exists())
                self.assertFalse(musl_sha.exists())

                musl.write_bytes(b"test")
                musl_sha.write_bytes(b"test")
                build.clean_stale_linux_artifacts("1.2.3", "musl")
                self.assertFalse(gnu.exists())
                self.assertFalse(gnu_sha.exists())
                self.assertTrue(musl.exists())
                self.assertTrue(musl_sha.exists())

    def test_find_powershell_skips_inaccessible_pwsh_alias(self):
        def fake_which(name: str):
            return {
                "pwsh": r"C:\WindowsApps\pwsh.EXE",
                "powershell": r"C:\Windows\System32\powershell.exe",
            }.get(name)

        def fake_run(cmd, **_kwargs):
            if "pwsh" in cmd[0]:
                raise OSError(1920, "The file cannot be accessed by the system")
            return subprocess.CompletedProcess(cmd, 0)

        with (
            patch.object(build.shutil, "which", side_effect=fake_which),
            patch.object(build.subprocess, "run", side_effect=fake_run),
        ):
            self.assertEqual(
                build.find_powershell(),
                r"C:\Windows\System32\powershell.exe",
            )


if __name__ == "__main__":
    unittest.main()

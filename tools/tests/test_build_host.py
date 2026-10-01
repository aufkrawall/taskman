"""Host detection must describe the compiler, even with stripped Windows env."""

import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "taskman_build", Path(__file__).resolve().parents[2] / "build.py"
)
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class HostDetectionTests(unittest.TestCase):
    def setUp(self):
        build.host_target.cache_clear()

    def tearDown(self):
        build.host_target.cache_clear()

    @patch.object(build.platform, "system", return_value="Windows")
    @patch.object(build.platform, "machine", return_value="")
    @patch.object(build.shutil, "which", return_value="rustc")
    def test_missing_machine_uses_compiler_host_and_nonempty_package_tag(self, *_):
        result = subprocess.CompletedProcess([], 0, "rustc 1.99\nhost: x86_64-pc-windows-msvc\n")
        with patch.object(build.subprocess, "run", return_value=result) as run:
            self.assertEqual(build.host_target(), "x86_64-pc-windows-msvc")
            self.assertEqual(build.host_tag(), "windows-x86_64")
            self.assertEqual(run.call_count, 1)

    @patch.object(build.platform, "system", return_value="Windows")
    @patch.object(build.platform, "machine", return_value="AMD64")
    @patch.object(build.shutil, "which", return_value="rustc")
    def test_arm_compiler_takes_precedence_over_emulated_python_architecture(self, *_):
        result = subprocess.CompletedProcess([], 0, "host: aarch64-pc-windows-msvc\n")
        with patch.object(build.subprocess, "run", return_value=result):
            self.assertEqual(build.host_target(), "aarch64-pc-windows-msvc")
            self.assertEqual(build.host_tag(), "windows-aarch64")

    @patch.object(build.platform, "system", return_value="Windows")
    @patch.object(build.shutil, "which", return_value="rustc")
    def test_failed_compiler_probe_falls_back_but_never_accepts_empty_architecture(self, *_):
        with patch.object(build.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "")):
            with patch.object(build.platform, "machine", return_value="AMD64"):
                self.assertEqual(build.host_target(), "x86_64-pc-windows-msvc")
            build.host_target.cache_clear()
            with patch.object(build.platform, "machine", return_value=""):
                with self.assertRaises(SystemExit):
                    build.host_target()


if __name__ == "__main__":
    unittest.main()

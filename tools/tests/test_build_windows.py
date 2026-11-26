"""A non-Windows host cross-builds the Windows release, or skips it loudly."""

import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "taskman_build", Path(__file__).resolve().parents[2] / "build.py"
)
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class WindowsCrossTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.cache = self.root / "xwin-cache"
        env = patch.dict(os.environ, {"XWIN_CACHE_DIR": str(self.cache)})
        env.start()
        self.addCleanup(env.stop)
        for p in (
            patch.object(build, "ROOT", self.root),
            patch.object(build, "CROSS_DIR", self.root / ".cross"),
            patch.object(build, "CROSS_TARGET_DIR", self.root / "target" / "windows-cross"),
            patch.object(build, "log", lambda _msg: None),
        ):
            p.start()
            self.addCleanup(p.stop)

    def cache_sdk(self, where=None, arches="x86_64 aarch64"):
        base = (where or self.cache) / "xwin"
        base.mkdir(parents=True, exist_ok=True)
        (base / "DONE").write_text(f"{arches}\nfile.vsix\n")

    def make_private(self):
        cross = self.root / ".cross"
        (cross / "cargo-home" / "bin").mkdir(parents=True)
        (cross / "cargo-home" / "bin" / "cargo-xwin").write_bytes(b"")
        lib = cross / "rustup" / "toolchains" / "1.2.3-host" / "lib" / "rustlib"
        lib = lib / build.WINDOWS_X64_TARGET / "lib"
        lib.mkdir(parents=True)
        (lib / "libstd-0123.rlib").write_bytes(b"")
        self.cache_sdk(cross / "xwin")
        return cross

    def ambient(self, xwin=True, std=True):
        """Pretend which of an ambient toolchain's parts this host has."""
        for p in (
            patch.object(
                build,
                "find_cargo_tool",
                side_effect=lambda name: "/bin/cargo-xwin"
                if xwin and name == "cargo-xwin"
                else None,
            ),
            patch.object(
                build,
                "installed_rust_targets",
                return_value={build.WINDOWS_X64_TARGET} if std else set(),
            ),
            patch.object(build.shutil, "which", return_value=None),
        ):
            p.start()
            self.addCleanup(p.stop)

    def test_private_toolchain_is_used_with_its_own_homes_and_target_dir(self):
        cross = self.make_private()
        toolchain, reason = build.windows_cross_toolchain()
        self.assertIsNone(reason)
        self.assertEqual(toolchain.xwin, str(cross / "cargo-home" / "bin" / "cargo-xwin"))
        self.assertEqual(toolchain.env["RUSTUP_HOME"], str(cross / "rustup"))
        self.assertEqual(toolchain.env["CARGO_HOME"], str(cross / "cargo-home"))
        self.assertEqual(toolchain.env["RUSTUP_TOOLCHAIN"], "1.2.3-host")
        self.assertEqual(toolchain.env["XWIN_CACHE_DIR"], str(cross / "xwin"))
        self.assertTrue(toolchain.env["PATH"].startswith(str(cross / "cargo-home" / "bin")))
        self.assertEqual(toolchain.target_dir, self.root / "target" / "windows-cross")

    def test_private_toolchain_without_the_sdk_is_not_usable(self):
        cross = self.make_private()
        (cross / "xwin" / "xwin" / "DONE").unlink()
        self.ambient(xwin=False)
        toolchain, reason = build.windows_cross_toolchain()
        self.assertIsNone(toolchain)
        self.assertIn(build.CROSS_SETUP, reason)

    def test_ambient_toolchain_when_complete(self):
        self.cache_sdk()
        self.ambient()
        toolchain, reason = build.windows_cross_toolchain()
        self.assertIsNone(reason)
        self.assertEqual(toolchain.xwin, "/bin/cargo-xwin")
        self.assertEqual(toolchain.target_dir, self.root / "target")

    def test_each_missing_ambient_part_is_a_skip_naming_the_setup(self):
        self.cache_sdk()
        self.ambient(xwin=False)
        toolchain, reason = build.windows_cross_toolchain()
        self.assertIsNone(toolchain)
        self.assertIn("cargo-xwin is not installed", reason)
        self.assertIn(build.CROSS_SETUP, reason)

    def test_missing_std_is_a_skip(self):
        self.cache_sdk()
        self.ambient(std=False)
        toolchain, reason = build.windows_cross_toolchain()
        self.assertIsNone(toolchain)
        self.assertIn(build.WINDOWS_X64_TARGET, reason)

    def test_sdk_is_never_downloaded_implicitly(self):
        # No DONE marker: running cargo-xwin would start a license-bearing
        # download, so the driver must skip instead.
        self.ambient()
        toolchain, reason = build.windows_cross_toolchain()
        self.assertIsNone(toolchain)
        self.assertIn("not cached", reason)

    def test_sdk_without_x86_64_is_not_enough(self):
        self.cache_sdk(arches="aarch64")
        self.assertFalse(build.xwin_sdk_cached(self.cache))

    def test_missing_toolchain_is_a_skip_not_a_failed_build(self):
        logs = []
        with (
            patch.object(build, "windows_cross_toolchain", return_value=(None, "no tool")),
            patch.object(build, "run") as run,
            patch.object(build, "log", logs.append),
        ):
            self.assertEqual(build.build_windows_cross("release"), (None, False))
        run.assert_not_called()
        self.assertTrue(any("skipping" in line and "no tool" in line for line in logs))

    def build(self, toolchain, run_ok=True):
        calls = []

        def run(command, env=None, cwd=None):
            calls.append((command, env))
            return run_ok

        with (
            patch.object(build, "windows_cross_toolchain", return_value=(toolchain, None)),
            patch.object(build, "run", side_effect=run),
        ):
            result = build.build_windows_cross("release")
        return result, calls

    def test_build_runs_cargo_xwin_for_the_msvc_target_with_hardening_flags(self):
        target_dir = self.root / "target" / "windows-cross"
        out = target_dir / build.WINDOWS_X64_TARGET / "release"
        out.mkdir(parents=True)
        (out / "taskman.exe").write_bytes(b"MZ")
        toolchain = build.WindowsCross(
            "/x/cargo-xwin", {"RUSTUP_HOME": "/h"}, target_dir, "private"
        )
        (exe, attempted), ((command, env),) = self.build(toolchain)
        self.assertEqual((exe, attempted), (out / "taskman.exe", True))
        self.assertEqual(command[:3], ["/x/cargo-xwin", "xwin", "build"])
        self.assertIn(build.WINDOWS_X64_TARGET, command)
        self.assertIn("--workspace", command)
        self.assertEqual(env["RUSTUP_HOME"], "/h")
        self.assertEqual(env["XWIN_ARCH"], "x86_64")
        self.assertEqual(env["CARGO_TARGET_DIR"], str(target_dir))
        self.assertIn("-Ccontrol-flow-guard=yes", env["RUSTFLAGS"])
        self.assertIn("-Clink-arg=/CETCOMPAT", env["RUSTFLAGS"])

    def test_ambient_build_keeps_the_default_target_dir(self):
        toolchain = build.WindowsCross("/x/cargo-xwin", {}, self.root / "target", "ambient")
        _, ((_, env),) = self.build(toolchain)
        self.assertNotIn("CARGO_TARGET_DIR", env)

    def test_failed_build_is_reported_as_attempted(self):
        toolchain = build.WindowsCross("/x", {}, self.root / "target", "ambient")
        result, _ = self.build(toolchain, run_ok=False)
        self.assertEqual(result, (None, True))


class SysrootTargetTests(unittest.TestCase):
    def test_sysroot_lists_only_targets_with_a_std(self):
        with tempfile.TemporaryDirectory() as tmp:
            rustlib = Path(tmp) / "lib" / "rustlib"
            real = rustlib / "x86_64-pc-windows-msvc" / "lib"
            real.mkdir(parents=True)
            (real / "libstd-0123abcd.rlib").write_bytes(b"")
            (rustlib / "etc" / "lib").mkdir(parents=True)  # not a target
            proc = build.subprocess.CompletedProcess([], 0, stdout=f"{tmp}\n")
            with (
                patch.object(build, "find_rustc", return_value="rustc"),
                patch.object(build.subprocess, "run", return_value=proc),
            ):
                self.assertEqual(
                    build.sysroot_rust_targets(), {"x86_64-pc-windows-msvc"}
                )

    def test_without_rustup_the_sysroot_decides(self):
        with (
            patch.object(build.shutil, "which", return_value=None),
            patch.object(build, "sysroot_rust_targets", return_value={"a"}),
        ):
            self.assertEqual(build.installed_rust_targets(), {"a"})


if __name__ == "__main__":
    unittest.main()

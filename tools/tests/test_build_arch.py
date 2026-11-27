"""The pacman package must be built wherever makepkg lives, WSL included."""

import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "taskman_build", Path(__file__).resolve().parents[2] / "build.py"
)
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)

WSL = r"C:\Windows\System32\wsl.exe"


def completed(cmd, returncode=0, stdout="", stderr=""):
    return subprocess.CompletedProcess(cmd, returncode, stdout, stderr)


def wsl_only(name):
    """`wsl.exe` resolves, `makepkg` does not: the Windows case."""
    return WSL if name in ("wsl.exe", "wsl") else None


def fake_wsl_run(distros_with_makepkg, listing, translated=None):
    """A `wsl.exe` that answers listing, os-release probes and wslpath."""
    calls = []

    def fake_run(cmd, **_kwargs):
        calls.append(cmd)
        if cmd[1:] == ["--list", "--quiet"]:
            return completed(cmd, 0, listing)
        if "wslpath" in cmd:
            name = Path(cmd[-1]).name
            mapping = translated or {"arch": "/mnt/c/repo/packaging/arch"}
            mapping.setdefault("dist", "/mnt/c/repo/dist")
            return completed(cmd, 0, mapping.get(name, ""))
        if "bash" in cmd:
            distro = cmd[cmd.index("-d") + 1]
            return completed(cmd, 0 if distro in distros_with_makepkg else 1)
        return completed(cmd, 1)

    return fake_run, calls


class ArchPackageHostTests(unittest.TestCase):
    def setUp(self):
        build.arch_package_host.cache_clear()

    def tearDown(self):
        build.arch_package_host.cache_clear()

    @patch.object(build, "log")
    @patch.object(build.shutil, "which", side_effect=wsl_only)
    @patch.object(build.platform, "system", return_value="Windows")
    def test_native_makepkg_is_used_directly(self, *_):
        with patch.object(build.shutil, "which", return_value="/usr/bin/makepkg"):
            host = build.arch_package_host()
        self.assertIsNotNone(host)
        self.assertIsNone(host.wsl)
        self.assertEqual(host.argv(["--printsrcinfo"]), ["/usr/bin/makepkg", "--printsrcinfo"])

    @patch.object(build, "log")
    @patch.object(build.shutil, "which", side_effect=wsl_only)
    @patch.object(build.platform, "system", return_value="Windows")
    def test_arch_wsl_distro_is_reached_through_wsl(self, *_):
        fake_run, _ = fake_wsl_run(
            {"rusty-route-arch"},
            "\ufeffUbuntu\r\nrusty-route-arch\r\n".encode("utf-16-le"),
        )
        with patch.object(build.subprocess, "run", side_effect=fake_run):
            host = build.arch_package_host()
        self.assertIsNotNone(host)
        self.assertEqual(host.distro, "rusty-route-arch")
        self.assertEqual(host.arch_dir, "/mnt/c/repo/packaging/arch")
        self.assertEqual(
            host.argv(["--printsrcinfo"]),
            [
                WSL,
                "-d",
                "rusty-route-arch",
                "--",
                "bash",
                "-lc",
                "cd /mnt/c/repo/packaging/arch && "
                "BUILDDIR=/var/tmp/taskman-arch/build SRCDEST=/var/tmp/taskman-arch/src "
                "PKGDEST=/mnt/c/repo/dist makepkg --printsrcinfo",
            ],
        )

    @patch.object(build, "log")
    @patch.object(build.shutil, "which", side_effect=wsl_only)
    @patch.object(build.platform, "system", return_value="Windows")
    def test_package_destination_and_quoting_survive_the_bash_layer(self, *_):
        fake_run, _ = fake_wsl_run(
            {"arch"},
            "arch\r\n".encode("utf-8"),
            translated={
                "arch": "/mnt/c/My Repo/packaging/arch",
                "dist": "/mnt/c/My Repo/dist",
            },
        )
        with patch.object(build.subprocess, "run", side_effect=fake_run):
            host = build.arch_package_host()
        self.assertIsNotNone(host)
        self.assertEqual(
            host.argv(["--force", "--noconfirm"], {"PKGDEST": host.dist_dir})[-1],
            "cd '/mnt/c/My Repo/packaging/arch' && "
            "BUILDDIR=/var/tmp/taskman-arch/build SRCDEST=/var/tmp/taskman-arch/src "
            "PKGDEST='/mnt/c/My Repo/dist' makepkg --force --noconfirm",
        )

    @patch.object(build, "log")
    @patch.object(build.shutil, "which", side_effect=wsl_only)
    @patch.object(build.platform, "system", return_value="Windows")
    def test_build_tree_stays_out_of_the_checkout(self, *_):
        """A read-only or 9p checkout must not be where the build tree lives."""
        fake_run, _ = fake_wsl_run({"arch"}, b"arch\r\n")
        with patch.object(build.subprocess, "run", side_effect=fake_run):
            host = build.arch_package_host()
        self.assertEqual(
            host.environment({"PKGDEST": host.dist_dir}),
            {
                "BUILDDIR": "/var/tmp/taskman-arch/build",
                "SRCDEST": "/var/tmp/taskman-arch/src",
                "PKGDEST": host.dist_dir,
            },
        )
        self.assertNotIn(host.arch_dir, host.env["BUILDDIR"])

    @patch.object(build, "log")
    @patch.object(build.shutil, "which", side_effect=wsl_only)
    @patch.object(build.platform, "system", return_value="Windows")
    def test_distro_named_arch_wins_without_starting_the_others(self, *_):
        fake_run, calls = fake_wsl_run(
            {"Ubuntu", "rusty-route-arch"},
            "\ufeffUbuntu\r\nrusty-route-arch\r\n".encode("utf-16-le"),
        )
        with patch.object(build.subprocess, "run", side_effect=fake_run):
            host = build.arch_package_host()
        self.assertEqual(host.distro, "rusty-route-arch")
        # A host with several distros pays for one start, not for all of them.
        probed = [call[call.index("-d") + 1] for call in calls if "bash" in call]
        self.assertEqual(probed, ["rusty-route-arch"])

    @patch.object(build, "log")
    @patch.object(build.shutil, "which", side_effect=wsl_only)
    @patch.object(build.platform, "system", return_value="Windows")
    def test_utf8_listing_is_accepted_like_the_utf16_one(self, *_):
        fake_run, _ = fake_wsl_run({"arch"}, b"arch\r\n")
        with patch.object(build.subprocess, "run", side_effect=fake_run):
            self.assertEqual(build.wsl_distro_names(WSL), ["arch"])
        fake_run, _ = fake_wsl_run({"arch"}, "arch\r\n".encode("utf-16-le"))
        with patch.object(build.subprocess, "run", side_effect=fake_run):
            self.assertEqual(build.wsl_distro_names(WSL), ["arch"])

    @patch.object(build, "log")
    @patch.object(build.shutil, "which", side_effect=wsl_only)
    @patch.object(build.platform, "system", return_value="Windows")
    def test_named_distro_skips_discovery(self, *_):
        fake_run, calls = fake_wsl_run({"custom"}, b"")
        with patch.dict(build.os.environ, {build.ARCH_WSL_DISTRO_ENV: "custom"}):
            with patch.object(build.subprocess, "run", side_effect=fake_run):
                host = build.arch_package_host()
        self.assertEqual(host.distro, "custom")
        self.assertFalse([call for call in calls if "--list" in call])

    @patch.object(build, "log")
    @patch.object(build.shutil, "which", side_effect=wsl_only)
    @patch.object(build.platform, "system", return_value="Windows")
    def test_no_arch_distro_means_no_host(self, *_):
        fake_run, _ = fake_wsl_run(set(), "\ufeffUbuntu\r\n".encode("utf-16-le"))
        with patch.object(build.subprocess, "run", side_effect=fake_run):
            self.assertIsNone(build.arch_package_host())

    @patch.object(build, "log")
    @patch.object(build.platform, "system", return_value="Linux")
    def test_linux_host_without_makepkg_has_no_host(self, *_):
        with patch.object(build.shutil, "which", return_value=None):
            self.assertIsNone(build.arch_package_host())

    @patch.object(build, "log")
    @patch.object(build.shutil, "which", side_effect=wsl_only)
    @patch.object(build.platform, "system", return_value="Windows")
    def test_unwritable_translation_is_skipped_not_guessed(self, *_):
        fake_run, _ = fake_wsl_run(
            {"arch"}, b"arch\r\n", translated={"arch": "", "dist": ""}
        )
        with patch.object(build.subprocess, "run", side_effect=fake_run):
            self.assertIsNone(build.arch_package_host())


class FakeHost:
    label = "fake makepkg"
    dist_dir = "/mnt/c/repo/dist"

    def __init__(self, srcinfo=None, ok=True):
        self.srcinfo = srcinfo
        self.ok = ok
        self.runs = []

    def capture(self, args):
        text = self.srcinfo
        if text is None:
            text = (build.ARCH_PACKAGING / ".SRCINFO").read_text(encoding="utf-8")
        return completed(args, 0, text)

    def run(self, args, env=None):
        self.runs.append((args, env))
        return self.ok


class BuildArchPackageTests(unittest.TestCase):
    def setUp(self):
        build.arch_package_host.cache_clear()

    def tearDown(self):
        build.arch_package_host.cache_clear()

    @patch.object(build, "log")
    def test_host_without_makepkg_is_a_documented_skip(self, *_):
        with patch.object(build, "arch_package_host", return_value=None):
            self.assertTrue(build.build_arch_package(build.read_version()))

    @patch.object(build, "log")
    def test_package_lands_in_dist_through_pkgdest(self, *_):
        host = FakeHost()
        version = build.read_version()
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            makepkg_name = dist / f"taskman-{version}-1-x86_64.pkg.tar.zst"

            def run(args, env=None):
                makepkg_name.write_bytes(b"pkg")
                return FakeHost.run(host, args, env)

            host.run = run
            with (
                patch.object(build, "DIST", dist),
                patch.object(build, "arch_package_host", return_value=host),
            ):
                self.assertTrue(build.build_arch_package(version))
            # Renamed so the release file names the distribution.
            self.assertFalse(makepkg_name.exists())
            self.assertTrue(
                (dist / f"taskman-v{version}-arch-x86_64.pkg.tar.zst").is_file()
            )
        self.assertEqual(
            host.runs,
            [(["--force", "--noconfirm"], None)],
        )

    @patch.object(build, "log")
    def test_stale_srcinfo_fails_before_makepkg_builds(self, *_):
        host = FakeHost(srcinfo="pkgbase = taskman\n\tpkgver = 0.0.1\n")
        with patch.object(build, "arch_package_host", return_value=host):
            self.assertFalse(build.build_arch_package(build.read_version()))
        self.assertEqual(host.runs, [])

    @patch.object(build, "log")
    def test_pkgver_mismatch_fails_before_makepkg_runs(self, *_):
        host = FakeHost()
        with patch.object(build, "arch_package_host", return_value=host):
            self.assertFalse(build.build_arch_package("9.9.9"))
        self.assertEqual(host.runs, [])


if __name__ == "__main__":
    unittest.main()

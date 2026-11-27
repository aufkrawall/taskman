"""A release run must refuse to finish without the Linux packages."""

import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    "taskman_build", ROOT / "build.py"
)
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class ReleasePackageNamesTests(unittest.TestCase):
    def test_both_architectures_expect_the_pacman_deb_and_rpm(self):
        self.assertEqual(
            build.release_package_names(
                "0.1.36",
                ["x86_64", "arm64"],
                arch_package=True,
                distro_packages=True,
            ),
            [
                "taskman-v0.1.36-arch-x86_64.pkg.tar.zst",
                "taskman-v0.1.36-debian-ubuntu-x86_64.deb",
                "taskman-v0.1.36-fedora-x86_64.rpm",
                "taskman-v0.1.36-debian-ubuntu-arm64.deb",
                "taskman-v0.1.36-fedora-arm64.rpm",
            ],
        )

    def test_the_run_options_narrow_the_expectation(self):
        self.assertEqual(
            build.release_package_names(
                "0.1.36", ["x86_64"], arch_package=False, distro_packages=True
            ),
            [
                "taskman-v0.1.36-debian-ubuntu-x86_64.deb",
                "taskman-v0.1.36-fedora-x86_64.rpm",
            ],
        )
        self.assertEqual(
            build.release_package_names(
                "0.1.36", ["x86_64"], arch_package=True, distro_packages=False
            ),
            ["taskman-v0.1.36-arch-x86_64.pkg.tar.zst"],
        )

    def test_a_linux_build_that_was_skipped_expects_no_distro_package(self):
        self.assertEqual(
            build.release_package_names(
                "0.1.36", [], arch_package=True, distro_packages=True
            ),
            ["taskman-v0.1.36-arch-x86_64.pkg.tar.zst"],
        )


class MissingReleasePackagesTests(unittest.TestCase):
    def test_names_absent_from_dist_are_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            names = ["taskman-v0.1.36-debian-ubuntu-x86_64.deb", "a.rpm"]
            self.assertEqual(build.missing_release_packages(names, dist), names)
            (dist / names[0]).write_bytes(b"deb")
            self.assertEqual(build.missing_release_packages(names, dist), ["a.rpm"])

    def test_a_complete_dist_reports_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            (dist / "a.deb").write_bytes(b"deb")
            self.assertEqual(build.missing_release_packages(["a.deb"], dist), [])


if __name__ == "__main__":
    unittest.main()

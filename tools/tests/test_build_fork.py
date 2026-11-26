"""The Python fork-gate fallback must run the same steps as tools/check-fork.ps1."""

import importlib.util
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("taskman_build", ROOT / "build.py")
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class ForkGateFallbackTests(unittest.TestCase):
    def test_package_list_matches_the_powershell_gate(self):
        script = (ROOT / "tools" / "check-fork.ps1").read_text(encoding="utf-8")
        block = re.search(r"\$packages = @\((.*?)\)", script, re.S).group(1)
        names = re.findall(r"'([^']+)'", block)
        # The script appends egui_software when the crate exists.
        self.assertIn("$packages += 'egui_software'", script)
        self.assertEqual(tuple(names) + ("egui_software",), build.FORK_PACKAGES)
        self.assertIn(f"$eframeSoftware = '{build.FORK_EFRAME_SOFTWARE}'", script)

    def test_fallback_covers_fmt_lint_and_tests_for_both_forks(self):
        steps = build.fork_gate_commands()
        commands = [" ".join(cmd[1:]) for _, cmd in steps]
        self.assertTrue(any(c.startswith("fmt ") and "--check" in c for c in commands))
        self.assertTrue(any("egui_software" in c and "-D warnings" in c for c in commands))
        self.assertTrue(any(c.startswith("test ") and "software" in c for c in commands))
        winit = [cmd for cwd, cmd in steps if cwd.name == "winit"]
        self.assertEqual(len(winit), 2, "winit fork gets clippy -D warnings and tests")


if __name__ == "__main__":
    unittest.main()

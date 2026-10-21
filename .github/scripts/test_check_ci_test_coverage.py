"""Offline checks that binding tests are named by a workflow."""

from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check_ci_test_coverage.py")


class CiTestCoverageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def write(self, relative: str, content: str = "") -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def run_gate(self, root: Path | None = None) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--repo-root", str(root or self.root)],
            capture_output=True, text=True, check=False,
        )

    def test_missing_reference_fails_and_names_the_file(self) -> None:
        self.write("bindings/flexaudio-py/tests/test_kept.py")
        self.write("bindings/flexaudio-py/tests/test_orphan.py")
        self.write("crates/flexaudio-napi/__test__/kept.test.mjs")
        self.write("crates/flexaudio-napi/__test__/orphan.test.mjs")
        self.write(".github/workflows/ci.yml", "\n".join([
            "test_kept.py",
            "kept.test.mjs",
            "not-test_orphan.py",
            "orphan.test.mjs.disabled",
        ]))
        result = self.run_gate()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("bindings/flexaudio-py/tests/test_orphan.py", result.stderr)
        self.assertIn("crates/flexaudio-napi/__test__/orphan.test.mjs", result.stderr)
        self.assertNotIn("test_kept.py", result.stderr)
        self.assertNotIn("kept.test.mjs", result.stderr)

    def test_all_referenced_passes(self) -> None:
        self.write("bindings/flexaudio-py/tests/test_kept.py")
        self.write("bindings/flexaudio-py/tests/helper.py", "not a test module\n")
        self.write("crates/flexaudio-napi/__test__/kept.test.mjs")
        self.write("crates/flexaudio-napi/__test__/notes.mjs", "not a test\n")
        self.write(".github/workflows/release.yml", "test_kept.py\nkept.test.mjs\n")
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("CI test coverage check passed: 2 binding tests", result.stdout)

    def test_empty_test_directory_fails(self) -> None:
        cases = (
            (
                "bindings/flexaudio-py/tests",
                "crates/flexaudio-napi/__test__/kept.test.mjs",
                "kept.test.mjs",
                "bindings/flexaudio-py/tests/test_*.py",
                True,
            ),
            (
                "crates/flexaudio-napi/__test__",
                "bindings/flexaudio-py/tests/test_kept.py",
                "test_kept.py",
                "crates/flexaudio-napi/__test__/*.test.mjs",
                False,
            ),
        )
        for empty, present, named, marker, create_empty in cases:
            with self.subTest(empty=empty):
                with tempfile.TemporaryDirectory() as name:
                    root = Path(name)
                    if create_empty:
                        directory = root / empty
                        directory.mkdir(parents=True)
                        (directory / "notes.txt").write_text("not a test\n", encoding="utf-8")
                    destination = root / present
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_text("", encoding="utf-8")
                    workflow = root / ".github" / "workflows" / "ci.yml"
                    workflow.parent.mkdir(parents=True)
                    workflow.write_text(named + "\n", encoding="utf-8")
                    result = self.run_gate(root)
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertIn(marker, result.stderr)


if __name__ == "__main__":
    unittest.main()

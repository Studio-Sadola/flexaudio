"""Offline checks that binding tests are named by a workflow command."""

from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import tempfile
from textwrap import dedent
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
        self.write(".github/workflows/ci.yml", dedent("""\
            jobs:
              test:
                steps:
                  - run: python test_kept.py kept.test.mjs not-test_orphan.py orphan.test.mjs.disabled
            """))
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
        self.write(".github/workflows/release.yml", dedent("""\
            jobs:
              test:
                steps:
                  - run: python test_kept.py kept.test.mjs
            """))
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

    def seed_orphans(self) -> None:
        self.write("bindings/flexaudio-py/tests/test_orphan.py")
        self.write("crates/flexaudio-napi/__test__/orphan.test.mjs")

    def assert_orphans_uncovered(self, workflow: str, filename: str = ".github/workflows/ci.yml") -> None:
        self.seed_orphans()
        self.write(filename, dedent(workflow))
        result = self.run_gate()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("bindings/flexaudio-py/tests/test_orphan.py", result.stderr)
        self.assertIn("crates/flexaudio-napi/__test__/orphan.test.mjs", result.stderr)

    def test_full_line_comment_does_not_count(self) -> None:
        outside = """\
            # TODO run test_orphan.py and orphan.test.mjs
            - run: echo hi
            """
        inside = """\
            jobs:
              test:
                steps:
                  - run: |
                      # TODO run test_orphan.py and orphan.test.mjs
                      echo hi
            """
        for label, workflow in (("outside", outside), ("inside", inside)):
            with self.subTest(label=label):
                self.assert_orphans_uncovered(workflow)

    def test_trailing_comment_does_not_count(self) -> None:
        on_run = """\
            jobs:
              test:
                steps:
                  - run: echo hi # test_orphan.py orphan.test.mjs
            """
        inside = """\
            jobs:
              test:
                steps:
                  - run: |
                      echo hi # test_orphan.py orphan.test.mjs
            """
        for label, workflow in (("run-line", on_run), ("inside-block", inside)):
            with self.subTest(label=label):
                self.assert_orphans_uncovered(workflow)

    def test_step_name_does_not_count(self) -> None:
        self.assert_orphans_uncovered("""\
            jobs:
              test:
                steps:
                  - name: run test_orphan.py and orphan.test.mjs
                    run: echo hi
            """)

    def test_literal_false_if_does_not_count(self) -> None:
        cases = (
            "false",
            "False",
            "FALSE",
            '"false"',
            "'FALSE'",
        )
        for condition in cases:
            with self.subTest(condition=condition):
                self.assert_orphans_uncovered(f"""\
                    jobs:
                      test:
                        steps:
                          - name: skipped
                            if: {condition}
                            run: python test_orphan.py orphan.test.mjs
                    """)
        self.assert_orphans_uncovered("""\
            jobs:
              test:
                steps:
                  - run: |
                      python test_orphan.py
                      node orphan.test.mjs
                    if: false
            """)

    def test_executed_run_forms_pass(self) -> None:
        self.write("bindings/flexaudio-py/tests/test_kept.py")
        self.write("crates/flexaudio-napi/__test__/kept.test.mjs")
        forms = {
            "single-line": """\
                jobs:
                  test:
                    steps:
                      - if: false
                        run: echo test_orphan.py orphan.test.mjs
                      - run: python test_kept.py kept.test.mjs # ignored
                """,
            "literal-block": """\
                jobs:
                  test:
                    steps:
                      - name: block
                        run: |
                          python test_kept.py
                          node kept.test.mjs
                """,
            "folded-block": """\
                jobs:
                  test:
                    steps:
                      - name: folded
                        run: >
                          python test_kept.py
                          node kept.test.mjs
                """,
        }
        filenames = {
            "single-line": ".github/workflows/ci.yml",
            "literal-block": ".github/workflows/ci.yml",
            "folded-block": ".github/workflows/ci.yaml",
        }
        for label, workflow in forms.items():
            with self.subTest(label=label):
                for path in (self.root / ".github" / "workflows").glob("*"):
                    path.unlink()
                self.write(filenames[label], dedent(workflow))
                result = self.run_gate()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("CI test coverage check passed: 2 binding tests", result.stdout)

    def test_no_workflow_files_fails(self) -> None:
        self.write("bindings/flexaudio-py/tests/test_kept.py")
        self.write("crates/flexaudio-napi/__test__/kept.test.mjs")
        notes = self.root / ".github" / "workflows" / "notes.txt"
        notes.parent.mkdir(parents=True)
        notes.write_text("test_kept.py kept.test.mjs\n", encoding="utf-8")
        result = self.run_gate()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("no workflow files under .github/workflows", result.stderr)

    def test_job_if_false_does_not_count(self) -> None:
        self.assert_orphans_uncovered("""\
            jobs:
              r:
                if: false
                steps:
                  - run: python test_orphan.py orphan.test.mjs
            """)

    def test_step_expression_false_does_not_count(self) -> None:
        for condition in ("${{ false }}", "${{false}}"):
            with self.subTest(condition=condition):
                self.assert_orphans_uncovered("""\
                    jobs:
                      r:
                        steps:
                          - if: {condition}
                            run: python test_orphan.py orphan.test.mjs
                    """.format(condition=condition))

    def test_matrix_include_run_does_not_count(self) -> None:
        self.assert_orphans_uncovered("""\
            jobs:
              r:
                strategy:
                  matrix:
                    include:
                      - run: python test_orphan.py orphan.test.mjs
                steps:
                  - run: echo hi
            """)

    def test_plain_scalar_comment_after_apostrophe_does_not_count(self) -> None:
        self.assert_orphans_uncovered("""\
            jobs:
              r:
                steps:
                  - run: echo don't  # test_orphan.py orphan.test.mjs
            """)

    def test_job_expression_false_does_not_count(self) -> None:
        for condition in ("${{ false }}", "${{false}}"):
            with self.subTest(condition=condition):
                self.assert_orphans_uncovered("""\
                    jobs:
                      r:
                        if: {condition}
                        steps:
                          - run: python test_orphan.py orphan.test.mjs
                    """.format(condition=condition))

    def test_non_false_if_still_counts(self) -> None:
        self.write("bindings/flexaudio-py/tests/test_kept.py")
        self.write("crates/flexaudio-napi/__test__/kept.test.mjs")
        self.write(".github/workflows/ci.yml", dedent("""\
            jobs:
              test:
                steps:
                  - if: runner.os == 'Linux'
                    run: python test_kept.py kept.test.mjs
            """))
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("CI test coverage check passed: 2 binding tests", result.stdout)

    def test_unparsable_workflow_names_the_file(self) -> None:
        self.seed_orphans()
        self.write(".github/workflows/broken.yml", "jobs: [\n")
        result = self.run_gate()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(".github/workflows/broken.yml", result.stderr)

    def test_missing_jobs_names_the_file(self) -> None:
        cases = {
            "missing": "name: CI\non: push\n",
            "not-a-mapping": "jobs:\n  - run: echo hi\n",
        }
        for label, workflow in cases.items():
            with self.subTest(label=label):
                self.seed_orphans()
                self.write(".github/workflows/ci.yml", workflow)
                result = self.run_gate()
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(".github/workflows/ci.yml", result.stderr)


if __name__ == "__main__":
    unittest.main()

"""Offline tests for release leaf selection and publish-order validation."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("workspace-leaf-crates.py")
SPEC = importlib.util.spec_from_file_location("workspace_leaf_crates", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


def dependency(crate: str, kind: str | None = None, **fields: object) -> dict[str, object]:
    return {"name": crate, "kind": kind, **fields}


def package(crate: str, *dependencies: dict[str, object]) -> dict[str, object]:
    return {"id": f"path+file:///fixture/{crate}#0.3.0", "name": crate,
            "dependencies": list(dependencies)}


def metadata(*packages: dict[str, object]) -> dict[str, object]:
    return {"version": 1, "packages": list(packages),
            "workspace_members": [item["id"] for item in packages]}


class WorkspaceReleaseTests(unittest.TestCase):
    def run_cli(self, value: object, mode: str, *crates: str) -> tuple[int, str, str]:
        return self.run_raw(json.dumps(value), mode, *crates)

    def run_raw(self, raw: str, mode: str, *crates: str) -> tuple[int, str, str]:
        output, errors = io.StringIO(), io.StringIO()
        with patch.object(sys, "stdin", io.StringIO(raw)), redirect_stdout(output), redirect_stderr(errors):
            status = release.main([mode, *crates])
        return status, output.getvalue(), errors.getvalue()

    def test_encode_depending_on_core_is_not_a_leaf(self) -> None:
        fixture = metadata(package("flexaudio-core"),
                           package("flexaudio-encode", dependency("flexaudio-core")),
                           package("flexaudio-denoise", dependency("external")))
        self.assertEqual(self.run_cli(fixture, "leaves", "flexaudio-encode", "flexaudio-core", "flexaudio-denoise"),
                         (0, "flexaudio-core\nflexaudio-denoise\n", ""))

    def test_dev_dependencies_are_ignored_in_both_modes(self) -> None:
        fixture = metadata(package("core"), package("vad", dependency("core", "dev")))
        self.assertEqual(self.run_cli(fixture, "leaves", "vad", "core"), (0, "vad\ncore\n", ""))
        self.assertEqual(self.run_cli(fixture, "verify-order", "vad", "core"), (0, "", ""))

    def test_build_optional_target_and_renamed_dependencies_count(self) -> None:
        for kind in (None, "normal", "build"):
            with self.subTest(kind=kind):
                fixture = metadata(package("core"), package("adapter", dependency(
                    "core", kind, optional=True, target='cfg(target_os = "windows")', rename="alias")))
                self.assertEqual(self.run_cli(fixture, "leaves", "core", "adapter"), (0, "core\n", ""))
                self.assertEqual(self.run_cli(fixture, "verify-order", "core", "adapter"), (0, "", ""))
                status, output, errors = self.run_cli(fixture, "verify-order", "adapter", "core")
                self.assertEqual((status, output), (1, ""))
                self.assertIn("adapter must appear after workspace dependencies: core", errors)

    def test_workspace_dependency_outside_release_set_fails_order(self) -> None:
        fixture = metadata(package("core"), package("encode", dependency("core")))
        self.assertEqual(self.run_cli(fixture, "leaves", "encode"), (0, "", ""))
        status, output, errors = self.run_cli(fixture, "verify-order", "encode")
        self.assertEqual((status, output), (1, ""))
        self.assertIn("core", errors)

    def test_cycles_and_self_dependencies_fail_order(self) -> None:
        for fixture in (metadata(package("core", dependency("core"))),
                        metadata(package("core", dependency("encode")), package("encode", dependency("core")))):
            with self.subTest(fixture=fixture):
                crates = [str(item["name"]) for item in fixture["packages"]]
                self.assertEqual(self.run_cli(fixture, "verify-order", *crates)[0], 1)

    def test_unknown_and_duplicate_release_crates_fail_without_partial_output(self) -> None:
        fixture = metadata(package("core"))
        for crates in (("core", "unknown"), ("core", "core")):
            for mode in ("leaves", "verify-order"):
                with self.subTest(crates=crates, mode=mode):
                    status, output, errors = self.run_cli(fixture, mode, *crates)
                    self.assertEqual((status, output), (1, ""))
                    self.assertTrue(errors)

    def test_malformed_metadata_fails_closed(self) -> None:
        malformed: list[object] = [None, {}, {"version": True},
                                   {"version": 1, "workspace_members": [], "packages": []}]
        for field, value in (("id", None), ("name", "bad\nname"), ("dependencies", None)):
            item = package("core")
            fixture = metadata(item)
            item[field] = value
            malformed.append(fixture)
        for dep in ({"name": "core"}, dependency("core", "unknown"), dependency("core", ""),
                    {"name": None, "kind": None}):
            malformed.append(metadata(package("core", dep)))
        missing = metadata(package("core"))
        missing["workspace_members"] = ["missing"]
        malformed.append(missing)
        duplicate = metadata(package("core"), package("core"))
        malformed.append(duplicate)
        duplicate_names = metadata(package("core"), {**package("core"), "id": "different"})
        malformed.append(duplicate_names)
        duplicate_members = metadata(package("core"))
        duplicate_members["workspace_members"] *= 2
        malformed.append(duplicate_members)
        for fixture in malformed:
            with self.subTest(fixture=fixture):
                status, output, errors = self.run_cli(fixture, "leaves", "core")
                self.assertEqual((status, output), (1, ""))
                self.assertTrue(errors)
        for raw in ("not JSON", '{"version":1,"version":1}', "{} trailing"):
            with self.subTest(raw=raw):
                self.assertEqual(self.run_raw(raw, "leaves", "core")[:2], (1, ""))

    def test_cargo_uses_argv_without_shell(self) -> None:
        fixture = metadata(package("core"))
        result = subprocess.CompletedProcess([], 0, stdout=json.dumps(fixture))
        with patch.object(release.subprocess, "run", return_value=result) as run:
            with redirect_stdout(io.StringIO()) as output:
                self.assertEqual(release.main(["leaves", "--cargo", "core"]), 0)
            self.assertEqual(output.getvalue(), "core\n")
            run.assert_called_once_with(
                ["cargo", "metadata", "--format-version", "1", "--no-deps", "--offline"],
                check=True, capture_output=True, text=True,
            )

    def test_cargo_failure_is_closed_and_does_not_echo_stderr(self) -> None:
        error = subprocess.CalledProcessError(1, ["cargo"], stderr="sensitive environment details")
        with patch.object(release.subprocess, "run", side_effect=error), redirect_stdout(io.StringIO()) as output:
            with redirect_stderr(io.StringIO()) as errors:
                self.assertEqual(release.main(["leaves", "--cargo", "core"]), 1)
        self.assertEqual(output.getvalue(), "")
        self.assertNotIn("sensitive", errors.getvalue())


if __name__ == "__main__":
    unittest.main()

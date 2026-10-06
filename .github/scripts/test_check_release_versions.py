"""Offline release version gate tests; no registry or audio hardware required."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check-release-versions.py")
VERSION = "0.3.0"
PLATFORMS = ("linux-x64-gnu", "win32-arm64-msvc")


class ReleaseVersionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.write("Cargo.toml", '[workspace.package]\nversion = "0.3.0"\n')
        self.write("bindings/flexaudio-py/pyproject.toml", '[project]\nversion = "0.3.0"\n')
        self.write_json("crates/flexaudio-napi/package.json", {
            "version": VERSION,
            "optionalDependencies": {
                f"@studio-sadola/flexaudio-{platform}": VERSION for platform in PLATFORMS
            },
        })
        for platform in PLATFORMS:
            self.write_json(f"crates/flexaudio-napi/npm/{platform}/package.json", {
                "name": f"@studio-sadola/flexaudio-{platform}", "version": VERSION,
            })

    def write(self, relative: str, content: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def write_json(self, relative: str, value: dict[str, object]) -> None:
        self.write(relative, json.dumps(value))

    def run_gate(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--repo-root", str(self.root), *arguments],
            capture_output=True, text=True, check=False,
        )

    def assert_failure(self, result: subprocess.CompletedProcess[str], message: str) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(message, result.stderr)

    def test_matching_version_and_tag(self) -> None:
        for arguments in (("--version", VERSION), ("--tag", "v0.3.0"),
                          ("--version", VERSION, "--require-platforms")):
            with self.subTest(arguments=arguments):
                result = self.run_gate(*arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("Release versions match 0.3.0", result.stdout)

    def test_invalid_tags_and_versions(self) -> None:
        for tag in ("0.3.0", "v0.3", "v0.3.0-rc.1", "v0.3.0\n", "v０.3.0", "v0.3.0;echo bad"):
            with self.subTest(tag=tag):
                self.assert_failure(self.run_gate("--tag", tag), "Release tag must match")
        for version in ("", "v0.3.0", "0.3.0+meta", "0.3.0\n", "$(echo bad)"):
            with self.subTest(version=version):
                self.assert_failure(self.run_gate("--version", version), "Release version must match")

    def test_wrong_requested_version(self) -> None:
        self.assert_failure(self.run_gate("--version", "0.3.1"), "expected 0.3.1, found 0.3.0")

    def test_each_manifest_version_must_match(self) -> None:
        paths = ("Cargo.toml", "bindings/flexaudio-py/pyproject.toml",
                 "crates/flexaudio-napi/package.json",
                 *(f"crates/flexaudio-napi/npm/{platform}/package.json" for platform in PLATFORMS))
        for relative in paths:
            with self.subTest(path=relative):
                path = self.root / relative
                original = path.read_text(encoding="utf-8")
                path.write_text(original.replace('"0.3.0"', '"0.3.1"', 1), encoding="utf-8")
                self.assert_failure(self.run_gate("--version", VERSION), "expected 0.3.0, found 0.3.1")
                path.write_text(original, encoding="utf-8")

    def test_optional_dependency_versions_must_match(self) -> None:
        for invalid in ("0.3.1", "^0.3.0", 3, None):
            with self.subTest(value=invalid):
                self.write_json("crates/flexaudio-napi/package.json", {
                    "version": VERSION,
                    "optionalDependencies": {f"@studio-sadola/flexaudio-{PLATFORMS[0]}": invalid},
                })
                self.assert_failure(self.run_gate("--version", VERSION), "optionalDependencies")

    def test_missing_malformed_and_duplicate_manifests(self) -> None:
        relative = "bindings/flexaudio-py/pyproject.toml"
        (self.root / relative).unlink()
        self.assert_failure(self.run_gate("--version", VERSION), "Cannot read/parse pyproject.toml")
        self.write(relative, "[project\nversion = broken")
        self.assert_failure(self.run_gate("--version", VERSION), "Cannot read/parse pyproject.toml")
        self.write(relative, "[project]\n")
        self.assert_failure(self.run_gate("--version", VERSION), "project.version")
        self.write(relative, '[project]\nversion = "0.3.0"\n')
        self.write("crates/flexaudio-napi/package.json", '{"version": "0.3.1", "version": "0.3.0"}')
        self.assert_failure(self.run_gate("--version", VERSION), "duplicate key")

    def test_invalid_optional_dependencies(self) -> None:
        for dependencies in ({}, [], None, {"other-package": VERSION}):
            with self.subTest(dependencies=dependencies):
                self.write_json("crates/flexaudio-napi/package.json", {
                    "version": VERSION, "optionalDependencies": dependencies,
                })
                self.assert_failure(self.run_gate("--version", VERSION), "optionalDependencies")

    def test_generated_package_set_must_be_complete(self) -> None:
        path = self.root / f"crates/flexaudio-napi/npm/{PLATFORMS[0]}/package.json"
        path.unlink()
        self.assert_failure(self.run_gate("--version", VERSION), "Cannot read/parse package.json")
        path.parent.rmdir()
        self.assert_failure(self.run_gate("--version", VERSION), "match optionalDependencies exactly")

    def test_unknown_and_duplicate_platform_names(self) -> None:
        for name in ("@studio-sadola/flexaudio-unknown", f"@studio-sadola/flexaudio-{PLATFORMS[0]}"):
            with self.subTest(name=name):
                self.write_json(f"crates/flexaudio-napi/npm/{PLATFORMS[1]}/package.json", {
                    "name": name, "version": VERSION,
                })
                self.assert_failure(self.run_gate("--version", VERSION), "unknown or duplicated")

    def test_source_checkout_and_required_generated_packages(self) -> None:
        npm = self.root / "crates/flexaudio-napi/npm"
        for directory in npm.iterdir():
            (directory / "package.json").unlink()
            directory.rmdir()
        self.assert_failure(self.run_gate("--version", VERSION), "match optionalDependencies exactly")
        npm.rmdir()
        result = self.run_gate("--version", VERSION)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_failure(self.run_gate("--version", VERSION, "--require-platforms"), "are missing")

    def test_unexpected_entry(self) -> None:
        self.write("crates/flexaudio-napi/npm/stray-file", "unexpected")
        self.assert_failure(self.run_gate("--version", VERSION), "unexpected entry")

    def test_explicit_version_or_tag_is_required(self) -> None:
        self.assert_failure(self.run_gate(), "required")
        self.assert_failure(self.run_gate("--tag", "v0.3.0", "--version", VERSION), "not allowed")


if __name__ == "__main__":
    unittest.main()

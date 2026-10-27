"""Guard npm artifact isolation and exercise the manifest CLI before tagging."""
from __future__ import annotations

from pathlib import Path, PurePosixPath
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from release_artifacts import EXPECTED, PLATFORMS, read_manifest


REPO = Path(__file__).resolve().parents[2]
TOOL = REPO / ".github/scripts/npm-release-manifest.py"


def collides(directory: str, tracked: set[str]) -> bool:
    """Reject tracked files in the directory or occupying any parent path."""
    return any(name == directory or name.startswith(directory + "/")
               or directory.startswith(name + "/") for name in tracked)


def tracked_checkout_files(directory: Path) -> set[str]:
    """Skip source archives, while keeping failures inside a checkout fatal."""
    if shutil.which("git") is None:
        raise unittest.SkipTest("requires Git to inspect tracked release download paths")
    probe = subprocess.run(
        ["git", "rev-parse", "--is-inside-work-tree"], cwd=directory,
        capture_output=True, text=True, check=False,
        env={**os.environ, "LC_ALL": "C"},
    )
    metadata_present = False
    for parent in (directory, *directory.parents):
        try:
            (parent / ".git").lstat()
        except FileNotFoundError:
            continue
        metadata_present = True
        break
    if (probe.returncode == 128 and not metadata_present
            and probe.stderr.startswith("fatal: not a git repository (or any ")):
        raise unittest.SkipTest("requires a Git worktree; source archives have no tracked file inventory")
    probe.check_returncode()
    if probe.stdout.strip() == "false":
        raise unittest.SkipTest("requires a Git worktree rather than a bare repository")
    if probe.stdout.strip() != "true":
        raise AssertionError(f"unexpected Git worktree response: {probe.stdout!r}")
    # Never catch this check's failure: a present checkout must provide its actual inventory.
    return set(subprocess.check_output(
        ["git", "ls-files", "-z"], cwd=directory).decode().split("\0")) - {""}


class NpmReleaseDownloadTests(unittest.TestCase):
    def test_download_paths_do_not_overlap_either_checkout(self) -> None:
        tracked = tracked_checkout_files(REPO)
        workflow = (REPO / ".github/workflows/release-npm.yml").read_text()
        # These literal paths are deliberately checked without a YAML dependency:
        # the existing Python CI runner installs only the standard library.
        downloads = [step for step in re.split(r"^      - ", workflow, flags=re.MULTILINE)
                     if "uses: actions/download-artifact@" in step]
        self.assertEqual(len(downloads), 3)
        # The resolved tag is checked out under source/ in addition to the outer
        # checkout. The runtime fresh-directory check also protects older tags.
        both_checkouts = tracked | {"source/" + name for name in tracked}
        self.assertTrue(collides("bindings", both_checkouts),
                        "Control must detect the original tracked bindings collision")
        self.assertFalse(collides(".release-artifacts", both_checkouts),
                         "The fresh artifact root must also be absent from tracked files")
        for step in downloads:
            paths = re.findall(r"^          path: (\S+)$", step, flags=re.MULTILINE)
            self.assertEqual(len(paths), 1, "Every download needs one literal directory")
            directory = paths[0]
            with self.subTest(directory=directory):
                self.assertFalse(collides(directory, both_checkouts),
                                 f"Download directory collides with tracked files: {directory}")
                path = PurePosixPath(directory)
                self.assertFalse(path.is_absolute())
                self.assertNotIn("..", path.parts)
                self.assertEqual(path.parts[0], ".release-artifacts",
                                 "Downloads must use the directory guarded before download")

    def test_create_verify_and_packages_cli(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            addons = root / ".release-artifacts/addons"
            manifest = root / ".release-artifacts/manifest/SHA256SUMS"
            for name in sorted(EXPECTED):
                directory = addons / ("bindings-" + name)
                directory.mkdir(parents=True)
                (directory / name).write_bytes(name.encode("ascii"))

            def run(mode: str, artifacts: Path) -> None:
                subprocess.run([sys.executable, str(TOOL), mode, "--artifacts",
                                str(artifacts), "--manifest", str(manifest)], check=True)

            run("create", addons)
            self.assertEqual(set(read_manifest(manifest)), EXPECTED)
            run("verify", addons)
            packages = root / "npm"
            for platform in PLATFORMS:
                directory = packages / platform
                directory.mkdir(parents=True)
                name = f"index.{platform}.node"
                shutil.copyfile(addons / ("bindings-" + name) / name, directory / name)
                for metadata in ("package.json", "README.md", "LICENSE", "THIRD_PARTY_NOTICES.md"):
                    (directory / metadata).write_text("metadata", encoding="ascii")
            run("packages", packages)


class GitCheckoutDiscoveryTests(unittest.TestCase):
    def test_git_absence_and_source_archive_have_explicit_skip_reasons(self) -> None:
        with mock.patch.object(shutil, "which", return_value=None):
            with self.assertRaisesRegex(unittest.SkipTest, "requires Git"):
                tracked_checkout_files(REPO)
        with tempfile.TemporaryDirectory() as temporary:
            probe = subprocess.CompletedProcess(
                ["git"], 128, "", "fatal: not a git repository (or any of the parent directories): .git\n")
            # A temporary root can itself be a worktree; this fixture models an archive's
            # absent metadata independently of the machine's temporary-directory ancestry.
            with mock.patch.object(shutil, "which", return_value="git"), \
                    mock.patch.object(subprocess, "run", return_value=probe), \
                    mock.patch.object(Path, "lstat", side_effect=FileNotFoundError):
                with self.assertRaisesRegex(unittest.SkipTest, "source archives"):
                    tracked_checkout_files(Path(temporary))
        probe = subprocess.CompletedProcess(["git"], 0, "false\n", "")
        with mock.patch.object(shutil, "which", return_value="git"), \
                mock.patch.object(subprocess, "run", return_value=probe):
            with self.assertRaisesRegex(unittest.SkipTest, "bare repository"):
                tracked_checkout_files(REPO)

    def test_checkout_probe_failures_are_not_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".git").mkdir()
            for diagnostic in (
                "fatal: not a git repository (or any of the parent directories): .git\n",
                "fatal: detected dubious ownership in repository\n",
                "fatal: cannot access repository: Permission denied\n",
            ):
                with self.subTest(diagnostic=diagnostic):
                    probe = subprocess.CompletedProcess(["git"], 128, "", diagnostic)
                    with mock.patch.object(shutil, "which", return_value="git"), \
                            mock.patch.object(subprocess, "run", return_value=probe):
                        with self.assertRaises(subprocess.CalledProcessError):
                            tracked_checkout_files(root)

    def test_tracked_inventory_failure_in_a_worktree_is_not_skipped(self) -> None:
        probe = subprocess.CompletedProcess(["git"], 0, "true\n", "")
        with mock.patch.object(shutil, "which", return_value="git"), \
                mock.patch.object(subprocess, "run", return_value=probe), \
                mock.patch.object(subprocess, "check_output", side_effect=
                                  subprocess.CalledProcessError(128, ["git", "ls-files", "-z"])):
            with self.assertRaises(subprocess.CalledProcessError):
                tracked_checkout_files(REPO)


if __name__ == "__main__":
    unittest.main()

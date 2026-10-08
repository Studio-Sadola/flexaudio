"""Guard npm artifact isolation and exercise the manifest CLI before tagging."""
from __future__ import annotations

from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

from release_artifacts import EXPECTED, PLATFORMS, read_manifest


REPO = Path(__file__).resolve().parents[2]
TOOL = REPO / ".github/scripts/npm-release-manifest.py"


def collides(directory: str, tracked: set[str]) -> bool:
    """Reject tracked files in the directory or occupying any parent path."""
    return any(name == directory or name.startswith(directory + "/")
               or directory.startswith(name + "/") for name in tracked)


class NpmReleaseDownloadTests(unittest.TestCase):
    def test_download_paths_do_not_overlap_either_checkout(self) -> None:
        workflow = (REPO / ".github/workflows/release-npm.yml").read_text()
        # These literal paths are deliberately checked without a YAML dependency:
        # the existing Python CI runner installs only the standard library.
        downloads = [step for step in re.split(r"^      - ", workflow, flags=re.MULTILINE)
                     if "uses: actions/download-artifact@" in step]
        self.assertEqual(len(downloads), 3)
        tracked = set(subprocess.check_output(
            ["git", "ls-files", "-z"], cwd=REPO).decode().split("\0")) - {""}
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


if __name__ == "__main__":
    unittest.main()

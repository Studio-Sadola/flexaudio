from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from release_artifacts import EXPECTED, PLATFORMS, digest, read_manifest, regular_files, verify


def load(name: str):
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


resolver = load("resolve-npm-release")
manifest_tool = load("npm-release-manifest")
msvc = load("configure-msvc-release")
repro = load("check-windows-reproducibility")


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bindings = self.root / "bindings"
        self.bindings.mkdir()
        self.hashes = {}
        for name in EXPECTED:
            path = self.bindings / name
            path.write_bytes(name.encode())
            self.hashes[name] = digest(path)
        self.manifest = self.root / "SHA256SUMS"
        self.records = "".join(f"{self.hashes[name]}  {name}\n" for name in sorted(EXPECTED))
        self.manifest.write_text(self.records)

    def test_exact_manifest_and_bytes(self):
        self.assertEqual(read_manifest(self.manifest), self.hashes)
        verify(self.bindings, self.hashes)

    def test_missing_extra_and_corruption(self):
        name = sorted(EXPECTED)[0]
        for mutation in ("missing", "extra", "corrupt"):
            with self.subTest(mutation=mutation):
                node = self.bindings / name
                original = node.read_bytes()
                if mutation == "missing":
                    node.unlink()
                elif mutation == "extra":
                    (self.bindings / "unexpected.txt").write_text("extra")
                else:
                    node.write_bytes(b"changed")
                with self.assertRaises(ValueError):
                    verify(self.bindings, self.hashes)
                node.write_bytes(original)
                (self.bindings / "unexpected.txt").unlink(missing_ok=True)

    def test_manifest_rejects_duplicate_missing_extra_and_paths(self):
        name = sorted(EXPECTED)[0]
        record = f"{self.hashes[name]}  {name}\n"
        for records in (self.records + record, record, self.records + "0" * 64 + "  extra.node\n",
                        self.records.replace(name, "../" + name), self.records.replace(name, "dir/" + name),
                        self.records.replace(name, "C:\\" + name), self.records.replace("  ", " ", 1)):
            with self.subTest(records=records):
                self.manifest.write_text(records)
                with self.assertRaises(ValueError):
                    read_manifest(self.manifest)

    def test_symlink_file_directory_root_and_manifest(self):
        if not hasattr(os, "symlink"):
            self.skipTest("symlinks unavailable")
        name = sorted(EXPECTED)[0]
        node = self.bindings / name
        node.unlink()
        try:
            node.symlink_to(self.manifest)
        except OSError:
            self.skipTest("symlink privilege unavailable")
        with self.assertRaises(ValueError):
            verify(self.bindings, self.hashes)
        node.unlink()
        node.write_bytes(name.encode())
        link = self.bindings / "nested"
        link.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(ValueError):
            verify(self.bindings, self.hashes)
        with self.assertRaises(ValueError):
            regular_files(link)
        link.unlink()
        manifest_link = self.root / "manifest-link"
        manifest_link.symlink_to(self.manifest)
        with self.assertRaises(ValueError):
            read_manifest(manifest_link)

    def test_duplicate_basename_rejected_before_merge(self):
        folder = self.bindings / "other-target"
        folder.mkdir()
        (folder / sorted(EXPECTED)[0]).write_bytes(b"duplicate")
        with self.assertRaises(ValueError):
            verify(self.bindings, self.hashes)

    def test_packages_layout_and_embedded_hashes(self):
        packages = self.root / "npm"
        packages.mkdir()
        for platform in PLATFORMS:
            directory = packages / platform
            directory.mkdir()
            name = f"index.{platform}.node"
            (directory / name).write_bytes(name.encode())
            for metadata in ("package.json", "README.md", "LICENSE", "THIRD_PARTY_NOTICES.md"):
                (directory / metadata).write_text("metadata")
        manifest_tool.verify_packages(packages, self.hashes)
        path = packages / PLATFORMS[0] / f"index.{PLATFORMS[0]}.node"
        path.write_bytes(b"changed after napi artifacts")
        with self.assertRaises(ValueError):
            manifest_tool.verify_packages(packages, self.hashes)
        path.write_bytes(path.name.encode())
        (packages / PLATFORMS[0] / "extra.node").write_bytes(b"extra")
        with self.assertRaises(ValueError):
            manifest_tool.verify_packages(packages, self.hashes)


class ResolverTests(unittest.TestCase):
    sha = "a" * 40
    tag = "v0.3.1"

    def run_record(self, **changes):
        return dict(id=10, event="push", head_sha=self.sha, head_branch=self.tag,
                    path=".github/workflows/release-npm.yml", conclusion="failure", **changes)

    def test_failed_original_not_successful_rebuild(self):
        original = self.run_record()
        later = dict(original, id=20, conclusion="success")
        dispatch = dict(original, id=1, event="workflow_dispatch")
        wrong_tag = dict(original, id=2, head_branch="v9.9.9")
        self.assertEqual(resolver.select_original([later, dispatch, wrong_tag, original], self.sha, self.tag), 10)

    def test_no_matching_run_and_invalid_ids(self):
        for runs in ([], [dict(self.run_record(), head_sha="b" * 40)],
                     [dict(self.run_record(), path="other.yml")], [dict(self.run_record(), id="10")]):
            with self.assertRaises(ValueError):
                resolver.select_original(runs, self.sha, self.tag)

    def test_version_allowlist(self):
        self.assertEqual(resolver.validate_version("0.3.1"), "0.3.1")
        for value in ("v0.3.1", "0.3.1-rc.1", "0.3.1\n", "$(id)", "1/2/3", "0.3.1;echo bad", ""):
            with self.assertRaises(ValueError):
                resolver.validate_version(value)

    def test_annotated_and_lightweight_tags(self):
        api = resolver.GitHub("owner/repo", "unused-test-token")
        with patch.object(api, "get", return_value={"object": {"type": "commit", "sha": self.sha}}):
            self.assertEqual(api.tag_commit(self.tag), self.sha)
        with patch.object(api, "get", side_effect=[
            {"object": {"type": "tag", "sha": "b" * 40}},
            {"object": {"type": "commit", "sha": self.sha}},
        ]):
            self.assertEqual(api.tag_commit(self.tag), self.sha)

    def test_run_pagination_does_not_filter_conclusion(self):
        api = resolver.GitHub("owner/repo", "unused-test-token")
        records = [dict(self.run_record(), id=100 + i) for i in range(100)]
        with patch.object(api, "get", side_effect=[{"workflow_runs": records}, {"workflow_runs": [self.run_record()]}]) as get:
            self.assertEqual(api.original_run(self.sha, self.tag), 10)
            self.assertIn("page=2", get.call_args.args[0])
            self.assertNotIn("status=", get.call_args.args[0])

    def test_dispatch_and_push_guard(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            environment = dict(EVENT_NAME="workflow_dispatch", INPUT_VERSION="0.3.1", GITHUB_REPOSITORY="owner/repo",
                               GH_TOKEN="unused-test-token", GITHUB_OUTPUT=str(output), GITHUB_RUN_ID="99", GITHUB_SHA=self.sha,
                               REF_TYPE="tag", REF_NAME=self.tag)
            with patch.dict(os.environ, environment), patch.object(resolver.GitHub, "tag_commit", return_value=self.sha), patch.object(resolver.GitHub, "original_run", return_value=10):
                resolver.main()
                self.assertIn("run_id=10", output.read_text())
                os.environ["EVENT_NAME"] = "push"
                with self.assertRaises(ValueError):
                    resolver.main()
                os.environ["GITHUB_RUN_ID"] = "10"
                resolver.main()


class MsvcTests(unittest.TestCase):
    def test_flags_static_crt_is_opt_in_and_paths_are_normalized(self):
        root = Path(tempfile.gettempdir()) / "workspace with spaces"
        values = msvc.flags(root, root / "cargo", root / "target", False)
        self.assertIn("link-arg=/DEBUG:NONE", values)
        self.assertNotIn("target-feature=+crt-static", values)
        self.assertIn(f"--remap-path-prefix={root.resolve()}=/src", values)
        self.assertIn("target-feature=+crt-static", msvc.flags(root, root / "cargo", root / "target", True))
        self.assertLess(values.index(f"--remap-path-prefix={root.resolve()}=/src"),
                        values.index(f"--remap-path-prefix={(root / 'target').resolve()}=/target"))

    def test_toml_and_global_override_rejection(self):
        import tomllib
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / ".cargo/config.toml"
            config.parent.mkdir()
            original = "[target.'cfg(all(target_os = \"windows\", target_env = \"msvc\"))']\nrustflags = [\"-C\", \"link-arg=/Brepro\"]\n"
            config.write_text(original)
            argv = ["script", "--workspace", str(root), "--target", msvc.TARGETS[0], "--target-dir", str(root / "target"), "--static-crt"]
            key = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS"
            with patch.object(__import__("sys"), "argv", argv), patch.dict(os.environ, {}, clear=True):
                for override in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", key):
                    with patch.dict(os.environ, {override: ""}):
                        with self.assertRaises(ValueError):
                            msvc.main()
                msvc.main()
                document = tomllib.loads(config.read_text())
                self.assertIn("target-feature=+crt-static", document["target"][msvc.TARGETS[0]]["rustflags"])
                self.assertIn("link-arg=/Brepro", document["target"]['cfg(all(target_os = "windows", target_env = "msvc"))']["rustflags"])
                with self.assertRaises(ValueError):
                    msvc.main()


class ReproducibilityTests(unittest.TestCase):
    def test_mismatch_reports_both_hashes_and_cleans_owned_targets(self):
        import contextlib
        import hashlib
        import io
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / ".cargo/config.toml"
            config.parent.mkdir()
            config.write_text("original config")
            addon = root / "crates/flexaudio-napi"
            addon.mkdir(parents=True)
            temp = root / "runner-temp"
            temp.mkdir()
            contents = iter((b"first", b"second"))
            def run(command, **kwargs):
                if command[0] == "test-npx":
                    (addon / "index.win32-x64-msvc.node").write_bytes(next(contents))
            output = io.StringIO()
            with patch.object(Path, "cwd", return_value=root), patch.dict(os.environ, {"RUNNER_TEMP": str(temp)}), patch.object(repro.shutil, "which", return_value="test-npx"), patch.object(repro.subprocess, "run", side_effect=run), contextlib.redirect_stdout(output):
                with self.assertRaisesRegex(ValueError, "not reproducible"):
                    repro.main()
            for content in (b"first", b"second"):
                self.assertIn(hashlib.sha256(content).hexdigest(), output.getvalue())
            self.assertEqual(config.read_text(), "original config")
            self.assertEqual(list(temp.iterdir()), [])

    def test_does_not_remove_preexisting_target(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / ".cargo/config.toml"
            config.parent.mkdir()
            config.write_text("original config")
            target = root / "flexaudio-repro-one"
            target.mkdir()
            sentinel = target / "keep.txt"
            sentinel.write_text("user data")
            with patch.object(Path, "cwd", return_value=root), patch.dict(os.environ, {"RUNNER_TEMP": str(root)}), patch.object(repro.shutil, "which", return_value="test-npx"):
                with self.assertRaisesRegex(ValueError, "must be new"):
                    repro.main()
            self.assertEqual(sentinel.read_text(), "user data")

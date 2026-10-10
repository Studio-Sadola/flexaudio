"""Exercise npm run resolution with an eventually consistent fake API."""
from __future__ import annotations

import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import call, patch
import urllib.parse


SPEC = importlib.util.spec_from_file_location(
    "resolve_npm_release", Path(__file__).with_name("resolve-npm-release.py"))
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("Cannot load npm release resolver")
resolver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(resolver)

SHA = "a" * 40
TAG = "v0.5.0"


def run_record(run_id: int = 200, **changes: object) -> dict[str, object]:
    record: dict[str, object] = dict(
        id=run_id, event="push", head_sha=SHA, head_branch=TAG,
        path=".github/workflows/release-npm.yml", run_attempt=1)
    record.update(changes)
    return record


class FakeGitHub(resolver.GitHub):
    def __init__(self, listings: list[list[object]], records: dict[int, dict[str, object]]):
        self.listings = listings
        self.records = records
        self.list_calls = 0
        self.paths: list[str] = []

    def get(self, path: str) -> dict[str, object]:
        self.paths.append(path)
        if path == "/git/ref/tags/" + TAG:
            return {"object": {"type": "commit", "sha": SHA}}
        if path.startswith("/actions/workflows/release-npm.yml/runs?"):
            query = urllib.parse.parse_qs(urllib.parse.urlsplit(path).query)
            if query != {"event": ["push"], "head_sha": [SHA], "per_page": ["100"], "page": ["1"]}:
                raise AssertionError("Unexpected run-list query")
            index = min(self.list_calls, len(self.listings) - 1)
            self.list_calls += 1
            return {"workflow_runs": self.listings[index]}
        if path.startswith("/actions/runs/"):
            return self.records[int(path.removeprefix("/actions/runs/"))]
        raise AssertionError("Unexpected API path")


class ResolveNpmReleaseTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.output = Path(temporary.name) / "output"
        self.env = dict(EVENT_NAME="push", REF_TYPE="tag", REF_NAME=TAG,
                        GITHUB_RUN_ID="200", GITHUB_RUN_ATTEMPT="1", GITHUB_SHA=SHA,
                        INPUT_VERSION="0.5.0", GITHUB_OUTPUT=str(self.output))
        self.sleep = unittest.mock.Mock()
        self.sleep_patch = patch.object(resolver.time, "sleep", self.sleep)
        self.sleep_patch.start()
        self.addCleanup(self.sleep_patch.stop)
        self.stdout = io.StringIO()

    def resolve(self, api: FakeGitHub) -> None:
        with contextlib.redirect_stdout(self.stdout):
            resolver.main(api=api, env=self.env)

    def assert_no_output(self) -> None:
        self.assertFalse(self.output.exists(), "Rejected runs must not emit release outputs")
        self.assertEqual(self.stdout.getvalue(), "")

    def test_push_missing_current_run_fetches_directly_and_waits_for_index(self) -> None:
        current = run_record()
        api = FakeGitHub([[], [], [current]], {200: current})
        self.resolve(api)
        self.assertEqual(api.paths[1], "/actions/runs/200")
        self.assertEqual(api.list_calls, 3)
        self.assertEqual(self.sleep.call_args_list, [call(2), call(4)])
        self.assertEqual(self.output.read_text(), f"version=0.5.0\nsha={SHA}\nrun_id=200\n")

    def test_older_original_rejects_current_push_even_when_current_is_unlisted(self) -> None:
        older, current = run_record(199), run_record()
        api = FakeGitHub([[older]], {199: older, 200: current})
        with self.assertRaisesRegex(ValueError, "Only the original tag-push run may build"):
            self.resolve(api)
        self.sleep.assert_not_called()
        self.assert_no_output()

    def test_older_original_appearing_after_retry_wins(self) -> None:
        older, current = run_record(199), run_record()
        api = FakeGitHub([[], [current, older]], {199: older, 200: current})
        with self.assertRaisesRegex(ValueError, "Only the original tag-push run may build"):
            self.resolve(api)
        self.sleep.assert_called_once_with(2)
        self.assert_no_output()

    def test_dispatch_listing_appears_on_second_attempt(self) -> None:
        self.env["EVENT_NAME"] = "workflow_dispatch"
        self.env["GITHUB_RUN_ATTEMPT"] = "2"
        original = run_record(199)
        api = FakeGitHub([[], [original]], {199: original})
        self.resolve(api)
        self.assertEqual(api.list_calls, 2)
        self.sleep.assert_called_once_with(2)
        self.assertIn("run_id=199\n", self.output.read_text())
        self.assertNotIn("/actions/runs/200", api.paths)

    def test_dispatch_never_indexed_errors_after_bounded_retries(self) -> None:
        self.env["EVENT_NAME"] = "workflow_dispatch"
        api = FakeGitHub([[]], {})
        with self.assertRaisesRegex(ValueError, "No original tag-push npm run found; rebuilding is forbidden"):
            self.resolve(api)
        self.assertEqual(api.list_calls, 6)
        self.assertEqual(self.sleep.call_args_list, [call(2), call(4), call(8), call(16), call(30)])
        self.assertEqual(sum(args.args[0] for args in self.sleep.call_args_list), 60)
        self.assert_no_output()

    def test_push_never_indexed_fails_closed_after_bounded_retries(self) -> None:
        current = run_record()
        api = FakeGitHub([[]], {200: current})
        with self.assertRaisesRegex(ValueError, "Current tag-push npm run is not indexed; rebuilding is forbidden"):
            self.resolve(api)
        self.assertEqual(api.list_calls, 6)
        self.assertEqual(self.sleep.call_args_list, [call(2), call(4), call(8), call(16), call(30)])
        self.assert_no_output()

    def test_push_rerun_attempt_two_is_rejected(self) -> None:
        for api_attempt, env_attempt in ((2, "2"), (2, "1"), (1, "2")):
            with self.subTest(api_attempt=api_attempt, env_attempt=env_attempt):
                self.env["GITHUB_RUN_ATTEMPT"] = env_attempt
                current = run_record(run_attempt=api_attempt)
                api = FakeGitHub([[current]], {200: current})
                with self.assertRaisesRegex(ValueError, "resealing is forbidden"):
                    self.resolve(api)
                self.assertEqual(api.list_calls, 0)
                self.assert_no_output()

    def test_current_run_metadata_is_strictly_validated(self) -> None:
        for changes in ({"id": 199}, {"id": "200"}, {"id": True}, {"event": "workflow_dispatch"},
                        {"head_sha": "b" * 40}, {"head_branch": "v0.4.0"}, {"path": "other.yml"},
                        {"run_attempt": True}, {"run_attempt": "1"}, {"run_attempt": 1.0}):
            with self.subTest(changes=changes):
                current = run_record(**changes)
                api = FakeGitHub([[current]], {200: current})
                with self.assertRaises(ValueError):
                    self.resolve(api)
                self.assertEqual(api.list_calls, 0)
                self.assert_no_output()

    def test_current_run_id_is_validated_before_building_api_path(self) -> None:
        for run_id in ("0", "-1", "0200", "200\n", "200/attempts/1", "$(id)"):
            with self.subTest(run_id=run_id):
                self.env["GITHUB_RUN_ID"] = run_id
                api = FakeGitHub([[]], {})
                with self.assertRaisesRegex(ValueError, "Invalid current run ID"):
                    self.resolve(api)
                self.assertEqual(api.paths, ["/git/ref/tags/" + TAG])
                self.assert_no_output()

    def test_listed_run_ids_are_strictly_validated(self) -> None:
        for run_id in ("199", True, 199.0, 0, -1):
            with self.subTest(run_id=run_id):
                current = run_record()
                api = FakeGitHub([[run_record(id=run_id)]], {200: current})
                with self.assertRaisesRegex(ValueError, "Invalid run ID"):
                    self.resolve(api)
                self.sleep.assert_not_called()
                self.assert_no_output()


if __name__ == "__main__":
    unittest.main()

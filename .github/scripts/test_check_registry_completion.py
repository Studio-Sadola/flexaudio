"""Offline registry completion tests with a fake HTTP transport and clock."""

from __future__ import annotations

import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.request import Request

import check_registry_completion as gate


VERSION = "1.2.3"


class FakeHTTP:
    def __init__(self, responses: dict[str, list[bytes | int]]) -> None:
        self.responses = responses
        self.calls: list[str] = []

    def __call__(self, request: Request, *, timeout: float) -> FakeResponse:
        self.calls.append(request.full_url)
        self.assert_timeout(timeout)
        responses = self.responses[request.full_url]
        response = responses.pop(0) if len(responses) > 1 else responses[0]
        if isinstance(response, int):
            raise HTTPError(request.full_url, response, "fake HTTP error", {}, None)
        # fetch() needs HTTP response metadata as well as the body/context manager.
        return FakeResponse(response, request.full_url)

    @staticmethod
    def assert_timeout(timeout: float) -> None:
        if not 0 < timeout <= 15:
            raise AssertionError("HTTP timeout must be bounded")


class FakeResponse(io.BytesIO):
    status = 200

    def __init__(self, body: bytes, url: str) -> None:
        super().__init__(body)
        self.url = url


class RegistryCompletionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.write("crates/core/Cargo.toml", '[package]\nname = "example-core"\n')
        self.write("crates/private/Cargo.toml", '[package]\nname = "example-private"\npublish = false\n')
        self.write("bindings/private/Cargo.toml", '[package]\nname = "example-binding"\npublish = false\n')
        self.write("crates/flexaudio-napi/package.json", json.dumps({
            "name": "@example/audio", "optionalDependencies": {"@example/audio-linux": VERSION},
        }))
        self.write("bindings/flexaudio-py/pyproject.toml", '[project]\nname = "example-audio"\n')
        self.items = gate.artifacts(self.root, VERSION)
        self.responses: dict[str, list[bytes | int]] = {}
        for artifact in self.items:
            if artifact.registry == "crates.io":
                body = {"version": {"crate": artifact.name, "num": VERSION, "yanked": False}}
            elif artifact.registry == "npm":
                body = {"name": artifact.name, "version": VERSION}
            else:
                body = {"info": {"name": artifact.name, "version": VERSION}, "urls": [
                    {"packagetype": "bdist_wheel", "filename": "example.whl", "yanked": False},
                    {"packagetype": "sdist", "filename": "example.tar.gz", "yanked": False},
                ]}
            self.responses[artifact.url] = [json.dumps(body).encode()]
        self.now = 0.0
        self.output: list[str] = []

    def write(self, relative: str, content: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def sleep(self, duration: float) -> None:
        self.now += duration

    def run_gate(self, wait: float = 0) -> int:
        self.http = FakeHTTP(self.responses)
        with patch.object(gate, "urlopen", self.http):
            return gate.run_gate(self.items, wait=wait, interval=1,
                                 clock=lambda: self.now, sleep=self.sleep, emit=self.output.append)

    def test_all_present(self) -> None:
        self.assertEqual(self.run_gate(), 0)
        self.assertEqual(self.output[-1], "Registry completion passed: 4/4 artifacts found")
        self.assertEqual(len(self.output), 5)

    def test_derives_artifacts_and_excludes_unpublished_crates(self) -> None:
        self.assertEqual([item.name for item in self.items], [
            "example-core", "@example/audio", "@example/audio-linux", "example-audio",
        ])
        self.assertTrue(all(item.url.startswith("https://") for item in self.items))
        self.assertTrue(all("registry.npmjs.org/" in item.url for item in self.items if item.registry == "npm"))

    def test_missing_npm_platform_names_artifact(self) -> None:
        self.responses[self.items[2].url] = [404]
        self.assertEqual(self.run_gate(), 1)
        self.assertIn("npm @example/audio-linux@1.2.3: missing", self.output)

    def test_404_then_present(self) -> None:
        url = self.items[2].url
        self.responses[url].insert(0, 404)
        self.assertEqual(self.run_gate(wait=2), 0)
        self.assertEqual(self.http.calls.count(url), 2)
        self.assertEqual(self.now, 1)
        self.assertEqual(self.http.calls.count(self.items[0].url), 1)

    def test_persistent_http_500_is_error(self) -> None:
        self.responses[self.items[2].url] = [500]
        self.assertEqual(self.run_gate(wait=2), 1)
        self.assertIn("npm @example/audio-linux@1.2.3: error (HTTP 500)", self.output)
        self.assertEqual(self.now, 2)

    def test_http_errors_other_than_404_are_errors(self) -> None:
        for status in (401, 403, 429, 503):
            with self.subTest(status=status):
                self.output.clear()
                self.responses[self.items[2].url] = [status]
                self.assertEqual(self.run_gate(), 1)
                self.assertIn(f"error (HTTP {status})", self.output[2])

    def test_malformed_json(self) -> None:
        self.responses[self.items[2].url] = [b"not JSON"]
        self.assertEqual(self.run_gate(), 1)
        self.assertIn("npm @example/audio-linux@1.2.3: error (malformed JSON)", self.output)

    def test_unreadable_json_is_not_success(self) -> None:
        for body in (b"[]", b"{}", b'{"name":"wrong", "version":"1.2.3"}',
                     b'{"name":"a", "name":"b"}'):
            with self.subTest(body=body):
                self.responses[self.items[2].url] = [body]
                self.assertEqual(self.run_gate(), 1)
                self.assertIn(": error (", self.output[-3])

    def test_pypi_requires_both_wheel_and_sdist(self) -> None:
        url = self.items[-1].url
        original = json.loads(self.responses[url][0])
        for files in ([], original["urls"][:1], original["urls"][1:]):
            with self.subTest(files=files):
                self.responses[url] = [json.dumps({**original, "urls": files}).encode()]
                self.assertEqual(self.run_gate(), 1)
                self.assertIn("missing (wheel and sdist required)", self.output[-2])

    def test_yanked_distribution_does_not_count(self) -> None:
        url = self.items[-1].url
        data = json.loads(self.responses[url][0])
        data["urls"][0]["yanked"] = True
        self.responses[url] = [json.dumps(data).encode()]
        self.assertEqual(self.run_gate(), 1)

    def test_invalid_version_and_empty_platforms_fail_closed(self) -> None:
        with self.assertRaises(gate.RegistryError):
            gate.artifacts(self.root, "1.2.3/other")
        self.write("crates/flexaudio-napi/package.json", '{"name":"example", "optionalDependencies":{}}')
        with self.assertRaises(gate.RegistryError):
            gate.artifacts(self.root, VERSION)

    def test_fetch_sends_descriptive_user_agent(self) -> None:
        def inspect(request: Request, *, timeout: float) -> FakeResponse:
            self.assertIn("registry-completion-gate", request.get_header("User-agent"))
            return FakeResponse(b"{}", request.full_url)

        with patch.object(gate, "urlopen", inspect):
            self.assertEqual(gate.fetch("https://registry.npmjs.org/example/1.2.3", 15), b"{}")


if __name__ == "__main__":
    unittest.main()

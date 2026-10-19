#!/usr/bin/env python3
"""Verify public registry release completeness using Python 3.11+ only."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from http.client import HTTPException
import json
import math
from pathlib import Path
import re
import sys
import time
import tomllib
from typing import Callable
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen


class RegistryError(Exception):
    """A manifest or registry response cannot be trusted."""


def mapping(value: object) -> dict[str, object]:
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        raise RegistryError("expected a JSON object or TOML table")
    return dict(value)


def unique_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise RegistryError("duplicate JSON key")
        result[key] = value
    return result


def package_name(value: object, *, npm: bool = False) -> str:
    pattern = r"(?:@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*" if npm else r"[A-Za-z0-9][A-Za-z0-9_-]*"
    if not isinstance(value, str) or re.fullmatch(pattern, value) is None:
        raise RegistryError("invalid package name")
    return value


@dataclass(frozen=True)
class Artifact:
    registry: str
    name: str
    version: str
    url: str

    @property
    def label(self) -> str:
        return f"{self.registry} {self.name}@{self.version}"


def artifacts(root: Path, version: str) -> list[Artifact]:
    if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version) is None:
        raise RegistryError("version must be MAJOR.MINOR.PATCH")
    result: list[Artifact] = []
    # Include binding manifests as well, so a future publishable binding is checked.
    manifests = sorted(root.glob("crates/*/Cargo.toml")) + sorted(root.glob("bindings/*/Cargo.toml"))
    for manifest in manifests:
        package = mapping(tomllib.loads(manifest.read_text(encoding="utf-8"))).get("package")
        package = mapping(package)
        publish = package.get("publish", True)
        if publish is False:
            continue
        if publish is not True:
            raise RegistryError("unsupported Cargo publish setting")
        name = package_name(package.get("name"))
        result.append(Artifact("crates.io", name, version, f"https://crates.io/api/v1/crates/{quote(name, safe='')}/{version}"))
    if not result:
        raise RegistryError("no publishable crates found")
    main = mapping(json.loads((root / "crates/flexaudio-napi/package.json").read_text(encoding="utf-8"), object_pairs_hook=unique_pairs))
    dependencies = mapping(main.get("optionalDependencies"))
    if not dependencies:
        raise RegistryError("npm optionalDependencies must not be empty")
    names = [package_name(main.get("name"), npm=True)]
    names.extend(package_name(name, npm=True) for name in sorted(dependencies))
    if len(set(names)) != len(names):
        raise RegistryError("duplicate npm package name")
    for name in names:
        result.append(Artifact("npm", name, version, f"https://registry.npmjs.org/{quote(name, safe='')}/{version}"))
    project = mapping(tomllib.loads((root / "bindings/flexaudio-py/pyproject.toml").read_text(encoding="utf-8"))).get("project")
    name = package_name(mapping(project).get("name"))
    result.append(Artifact("PyPI", name, version, f"https://pypi.org/pypi/{quote(name, safe='')}/{version}/json"))
    return result


def fetch(url: str, timeout: float) -> bytes | None:
    request = Request(url, headers={"User-Agent": "registry-completion-gate/1.0 (public release verification)", "Accept": "application/json"})
    try:
        with urlopen(request, timeout=timeout) as response:
            if response.status != 200 or not response.url.startswith("https://"):
                raise RegistryError("unexpected HTTP response")
            return response.read()
    except HTTPError as error:
        if error.code == 404:
            return None
        raise RegistryError(f"HTTP {error.code}") from error
    except (URLError, OSError, HTTPException) as error:
        raise RegistryError(f"network failure ({type(error).__name__})") from error


def check(artifact: Artifact, body: bytes | None) -> str:
    if body is None:
        return "missing"
    try:
        data = mapping(json.loads(body, object_pairs_hook=unique_pairs))
    except (ValueError, UnicodeError) as error:
        raise RegistryError("malformed JSON") from error
    if artifact.registry == "crates.io":
        release = mapping(data.get("version"))
        if release.get("num") != artifact.version or release.get("crate") != artifact.name:
            raise RegistryError("crate identity/version mismatch")
        if release.get("yanked") is not False:
            raise RegistryError("crate is yanked or has unreadable yank status")
    elif artifact.registry == "npm":
        if data.get("version") != artifact.version or data.get("name") != artifact.name:
            raise RegistryError("npm identity/version mismatch")
    else:
        info = mapping(data.get("info"))
        name = info.get("name")
        if not isinstance(name, str) or normalize_python_name(name) != normalize_python_name(artifact.name) or info.get("version") != artifact.version:
            raise RegistryError("PyPI identity/version mismatch")
        files = data.get("urls")
        if not isinstance(files, list):
            raise RegistryError("unreadable PyPI file list")
        kinds: set[str] = set()
        for item in files:
            file = mapping(item)
            kind = file.get("packagetype")
            filename = file.get("filename")
            if not isinstance(kind, str) or not isinstance(filename, str) or not isinstance(file.get("yanked"), bool):
                raise RegistryError("unreadable PyPI file metadata")
            if file["yanked"] is False:
                if kind == "bdist_wheel" and filename.endswith(".whl"):
                    kinds.add(kind)
                elif kind == "sdist" and filename.endswith((".tar.gz", ".zip")):
                    kinds.add(kind)
        if not {"bdist_wheel", "sdist"}.issubset(kinds):
            return "missing (wheel and sdist required)"
    return "found"


def normalize_python_name(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def run_gate(items: list[Artifact], *, wait: float = 900, interval: float = 30,
             http: Callable[[str, float], bytes | None] = fetch,
             clock: Callable[[], float] = time.monotonic,
             sleep: Callable[[float], None] = time.sleep,
             emit: Callable[[str], None] = print) -> int:
    deadline = clock() + wait
    pending = list(items)
    statuses: dict[Artifact, str] = {}
    while pending:
        for artifact in list(pending):
            # A zero-wait invocation still performs one complete inspection.
            try:
                statuses[artifact] = check(artifact, http(artifact.url, 15))
            except RegistryError as error:
                statuses[artifact] = f"error ({error})"
            if statuses[artifact] == "found":
                pending.remove(artifact)
        remaining = deadline - clock()
        if not pending or remaining <= 0:
            break
        sleep(min(interval, remaining))
    for artifact in items:
        emit(f"{artifact.label}: {statuses[artifact]}")
    passed = not pending
    emit(f"Registry completion {'passed' if passed else 'failed'}: {len(items) - len(pending)}/{len(items)} artifacts found")
    return 0 if passed else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--wait-seconds", type=float, default=900)
    parser.add_argument("--interval-seconds", type=float, default=30)
    args = parser.parse_args(argv)
    if not math.isfinite(args.wait_seconds) or args.wait_seconds < 0 or not math.isfinite(args.interval_seconds) or args.interval_seconds <= 0:
        parser.error("wait must be finite and nonnegative; interval must be finite and positive")
    try:
        return run_gate(artifacts(args.repo_root, args.version), wait=args.wait_seconds, interval=args.interval_seconds)
    except (RegistryError, OSError, ValueError) as error:
        print(f"Registry completion failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

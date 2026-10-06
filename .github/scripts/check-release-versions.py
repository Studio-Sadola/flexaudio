#!/usr/bin/env python3
"""Check release versions using only the standard library (Python 3.11+)."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys
import tomllib


VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
PACKAGE_PATTERN = re.compile(r"@studio-sadola/flexaudio-[a-z0-9-]+")


class VersionCheckError(Exception):
    """A missing, invalid, or inconsistent release manifest."""


def mapping(value: object, label: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise VersionCheckError(f"{label} must be an object/table")
    result: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise VersionCheckError(f"{label} keys must be strings")
        result[key] = item
    return result


def unique_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise VersionCheckError("JSON manifest contains a duplicate key")
        result[key] = value
    return result


def load_manifest(path: Path) -> dict[str, object]:
    try:
        if path.suffix == ".toml":
            with path.open("rb") as source:
                value: object = tomllib.load(source)
        else:
            value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_pairs)
    except (OSError, UnicodeError, ValueError) as error:
        raise VersionCheckError(f"Cannot read/parse {path.name}: {type(error).__name__}") from error
    return mapping(value, path.name)


def require_version(value: object, expected: str, label: str) -> None:
    if not isinstance(value, str) or VERSION_PATTERN.fullmatch(value) is None:
        raise VersionCheckError(f"{label} must be a numeric MAJOR.MINOR.PATCH version")
    if value != expected:
        raise VersionCheckError(f"{label}: expected {expected}, found {value}")


def check_versions(repo_root: Path, version: str, *, require_platforms: bool = False) -> None:
    if VERSION_PATTERN.fullmatch(version) is None:
        raise VersionCheckError("Release version must match [0-9]+.[0-9]+.[0-9]+ exactly")
    cargo = load_manifest(repo_root / "Cargo.toml")
    workspace = mapping(cargo.get("workspace"), "Cargo.toml workspace")
    package = mapping(workspace.get("package"), "Cargo.toml workspace.package")
    require_version(package.get("version"), version, "Cargo.toml workspace.package.version")

    pyproject = load_manifest(repo_root / "bindings/flexaudio-py/pyproject.toml")
    project = mapping(pyproject.get("project"), "pyproject.toml project")
    require_version(project.get("version"), version, "pyproject.toml project.version")

    addon = repo_root / "crates/flexaudio-napi"
    main = load_manifest(addon / "package.json")
    require_version(main.get("version"), version, "npm main package.json version")
    dependencies = mapping(main.get("optionalDependencies"), "npm optionalDependencies")
    if not dependencies:
        raise VersionCheckError("npm optionalDependencies must not be empty")
    for name, dependency_version in dependencies.items():
        if PACKAGE_PATTERN.fullmatch(name) is None:
            raise VersionCheckError("npm optionalDependencies contains an unexpected package name")
        require_version(dependency_version, version, f"optionalDependencies {name}")

    # napi creates this directory during release. A source checkout has none;
    # publishing must explicitly require the complete generated package set.
    platforms_dir = addon / "npm"
    if not platforms_dir.exists():
        if require_platforms:
            raise VersionCheckError("Generated npm platform packages are missing")
        return
    if not platforms_dir.is_dir():
        raise VersionCheckError("npm must be a directory")
    names: set[str] = set()
    for directory in sorted(platforms_dir.iterdir()):
        if not directory.is_dir() or directory.is_symlink():
            raise VersionCheckError("npm contains an unexpected entry")
        platform = load_manifest(directory / "package.json")
        name = platform.get("name")
        if not isinstance(name, str) or name not in dependencies or name in names:
            raise VersionCheckError("npm platform package name is unknown or duplicated")
        names.add(name)
        require_version(platform.get("version"), version, f"npm platform {name} version")
    if names != set(dependencies):
        raise VersionCheckError("Generated npm platform packages must match optionalDependencies exactly")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--version", help="Explicit workflow_dispatch version (without v)")
    source.add_argument("--tag", help="Release tag, exactly vMAJOR.MINOR.PATCH")
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--require-platforms", action="store_true")
    args = parser.parse_args(argv)
    try:
        version: str
        if args.tag is not None:
            if re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", args.tag) is None:
                raise VersionCheckError("Release tag must match ^v[0-9]+\\.[0-9]+\\.[0-9]+$ exactly")
            version = args.tag[1:]
        else:
            version = args.version
        check_versions(args.repo_root, version, require_platforms=args.require_platforms)
    except VersionCheckError as error:
        print(f"Release version check failed: {error}", file=sys.stderr)
        return 1
    except OSError as error:
        print(f"Release version check failed: cannot inspect manifests ({type(error).__name__})", file=sys.stderr)
        return 1
    print(f"Release versions match {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

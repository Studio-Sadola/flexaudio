#!/usr/bin/env python3
"""Select release leaves or verify publish order from Cargo metadata (stdlib only)."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys


NAME_PATTERN = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_-]*")


class MetadataError(Exception):
    """Invalid metadata or an unsafe release plan."""


def mapping(value: object, label: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise MetadataError(f"{label} must be an object")
    result: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise MetadataError(f"{label} keys must be strings")
        result[key] = item
    return result


def sequence(value: object, label: str) -> list[object]:
    if not isinstance(value, list):
        raise MetadataError(f"{label} must be an array")
    return list(value)


def string(value: object, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise MetadataError(f"{label} must be a nonempty string")
    return value


def name(value: object) -> str:
    result = string(value, "crate name")
    if NAME_PATTERN.fullmatch(result) is None:
        raise MetadataError("Invalid crate name")
    return result


def unique_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise MetadataError("Metadata contains a duplicate JSON key")
        result[key] = value
    return result


def workspace_dependencies(raw: str) -> dict[str, set[str]]:
    try:
        value: object = json.loads(raw, object_pairs_hook=unique_pairs)
    except ValueError as error:
        raise MetadataError("Metadata is not valid JSON") from error
    metadata = mapping(value, "metadata")
    if type(metadata.get("version")) is not int or metadata["version"] != 1:
        raise MetadataError("Metadata format version must be 1")
    members = [string(item, "workspace member ID") for item in
               sequence(metadata.get("workspace_members"), "workspace_members")]
    if not members or len(members) != len(set(members)):
        raise MetadataError("Workspace members must be nonempty and unique")
    packages: dict[str, tuple[str, set[str]]] = {}
    for item in sequence(metadata.get("packages"), "packages"):
        package = mapping(item, "package")
        package_id = string(package.get("id"), "package ID")
        if package_id in packages:
            raise MetadataError("Duplicate package ID")
        dependencies: set[str] = set()
        for item in sequence(package.get("dependencies"), "dependencies"):
            dependency = mapping(item, "dependency")
            dependency_name = name(dependency.get("name"))
            if "kind" not in dependency or dependency["kind"] not in (None, "normal", "build", "dev"):
                raise MetadataError("Dependency kind must be null, normal, build, or dev")
            # Metadata names the actual package, even for renamed dependencies.
            # Include optional and target-specific dependencies on every platform.
            if dependency["kind"] != "dev":
                dependencies.add(dependency_name)
        packages[package_id] = (name(package.get("name")), dependencies)
    if not set(members).issubset(packages):
        raise MetadataError("A workspace member is missing from packages")
    names = [packages[member][0] for member in members]
    if len(names) != len(set(names)):
        raise MetadataError("Workspace package names must be unique")
    workspace_names = set(names)
    return {packages[member][0]: packages[member][1] & workspace_names for member in members}


def release_plan(raw: str, crates: list[str]) -> dict[str, set[str]]:
    dependencies = workspace_dependencies(raw)
    if not crates or len(crates) != len(set(crates)):
        raise MetadataError("Release crates must be nonempty and unique")
    for crate in crates:
        name(crate)
        if crate not in dependencies:
            raise MetadataError(f"Release crate {crate} is not a workspace member")
    return dependencies


def verify_order(dependencies: dict[str, set[str]], crates: list[str]) -> None:
    preceding: set[str] = set()
    for crate in crates:
        missing = dependencies[crate] - preceding
        if missing:
            raise MetadataError(f"{crate} must appear after workspace dependencies: {', '.join(sorted(missing))}")
        preceding.add(crate)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("leaves", "verify-order"))
    parser.add_argument("--cargo", action="store_true", help="Run cargo metadata offline instead of reading stdin")
    parser.add_argument("crates", nargs="+", help="Release crates in publish order")
    args = parser.parse_args(argv)
    try:
        if args.cargo:
            result = subprocess.run(
                ["cargo", "metadata", "--format-version", "1", "--no-deps", "--offline"],
                check=True, capture_output=True, text=True,
            )
            raw = result.stdout
        else:
            raw = sys.stdin.read()
        dependencies = release_plan(raw, args.crates)
        if args.mode == "verify-order":
            verify_order(dependencies, args.crates)
        else:
            # Validate the complete input before emitting any leaf names.
            for crate in args.crates:
                if not dependencies[crate]:
                    print(crate)
    except (MetadataError, OSError, UnicodeError, subprocess.CalledProcessError) as error:
        # Cargo stderr can contain environment details; do not echo it.
        detail = str(error) if isinstance(error, MetadataError) else type(error).__name__
        print(f"Workspace release check failed: {detail}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

"""Strict npm release file contract shared by manifest creation and verification."""
from __future__ import annotations

import hashlib
import re
from pathlib import Path

PLATFORMS = (
    "linux-x64-gnu", "linux-arm64-gnu", "darwin-arm64",
    "win32-x64-msvc", "win32-arm64-msvc",
)
EXPECTED = frozenset(f"index.{platform}.node" for platform in PLATFORMS)


def regular_files(root: Path) -> dict[str, Path]:
    """Walk without following links; reject special files and duplicate basenames."""
    if root.is_symlink() or not root.is_dir():
        raise ValueError(f"Not a regular directory: {root}")
    files: dict[str, Path] = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"Symlink rejected: {path}")
        if path.is_dir():
            continue
        if not path.is_file() or path.name in files:
            raise ValueError(f"Special or duplicate file: {path}")
        files[path.name] = path
    return files


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read_manifest(path: Path) -> dict[str, str]:
    if path.is_symlink() or not path.is_file():
        raise ValueError("Manifest must be a regular file")
    hashes: dict[str, str] = {}
    for line in path.read_text(encoding="ascii").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([a-z0-9.-]+)", line)
        if match is None:
            raise ValueError("Invalid checksum record (paths are forbidden)")
        checksum, name = match.groups()
        if name not in EXPECTED or name in hashes:
            raise ValueError(f"Unexpected or duplicate manifest filename: {name}")
        hashes[name] = checksum
    if hashes.keys() != EXPECTED:
        raise ValueError("Manifest must list exactly the five supported addons")
    return hashes


def verify(root: Path, hashes: dict[str, str]) -> None:
    files = regular_files(root)
    if files.keys() != EXPECTED:
        raise ValueError("Artifacts must contain exactly the five supported addons")
    for name, checksum in hashes.items():
        if digest(files[name]) != checksum:
            raise ValueError(f"SHA256 mismatch: {name}")

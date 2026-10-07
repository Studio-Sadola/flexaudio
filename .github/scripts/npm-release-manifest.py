#!/usr/bin/env python3
"""Create or verify the immutable npm checksum manifest; never builds anything."""
from __future__ import annotations

import argparse
from pathlib import Path
from release_artifacts import EXPECTED, PLATFORMS, digest, read_manifest, regular_files, verify


def verify_packages(root: Path, hashes: dict[str, str]) -> None:
    if root.is_symlink() or not root.is_dir() or {p.name for p in root.iterdir()} != set(PLATFORMS):
        raise ValueError("Unexpected platform package set")
    for platform in PLATFORMS:
        directory = root / platform
        files = regular_files(directory)
        name = f"index.{platform}.node"
        if set(files) != {name, "package.json", "README.md", "LICENSE", "THIRD_PARTY_NOTICES.md"}:
            raise ValueError(f"Unexpected files in {platform}")
        if any(p.parent != directory for p in files.values()):
            raise ValueError("Nested package files rejected")
        if digest(files[name]) != hashes[name]:
            raise ValueError(f"Packaged SHA256 mismatch: {name}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("create", "verify", "packages"))
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    args = parser.parse_args()
    if args.mode == "create":
        files = regular_files(args.artifacts)
        if files.keys() != EXPECTED:
            raise ValueError("Artifacts must contain exactly the five supported addons")
        if args.manifest.exists() or args.manifest.is_symlink():
            raise ValueError("Refusing to replace an existing manifest")
        args.manifest.parent.mkdir(parents=True, exist_ok=True)
        args.manifest.write_text("".join(f"{digest(files[name])}  {name}\n" for name in sorted(EXPECTED)), encoding="ascii")
    else:
        # The manifest artifact must not carry additional files.
        files = regular_files(args.manifest.parent)
        if set(files) != {"SHA256SUMS"} or args.manifest.name != "SHA256SUMS":
            raise ValueError("Manifest artifact must contain only SHA256SUMS")
        hashes = read_manifest(args.manifest)
        if args.mode == "packages":
            verify_packages(args.artifacts, hashes)
        else:
            verify(args.artifacts, hashes)


if __name__ == "__main__":
    main()

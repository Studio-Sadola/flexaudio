#!/usr/bin/env python3
"""Build the release N-API addon twice without sharing compiled target objects."""
from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import sys
from pathlib import Path

TARGET = "x86_64-pc-windows-msvc"


def main() -> None:
    workspace = Path.cwd().resolve()
    scripts = Path(__file__).resolve().parent
    config = workspace / ".cargo/config.toml"
    original = config.read_bytes()
    addon = workspace / "crates/flexaudio-napi"
    npx = shutil.which("npx")
    if npx is None:
        raise ValueError("npx is required")
    hashes: list[str] = []
    owned_dirs: list[Path] = []
    try:
        for label in ("one", "two"):
            target_dir = Path(os.environ["RUNNER_TEMP"]) / f"flexaudio-repro-{label}"
            if target_dir.exists():
                raise ValueError("Independent target directory must be new")
            target_dir.mkdir()
            owned_dirs.append(target_dir)
            config.write_bytes(original)
            subprocess.run([
                sys.executable, str(scripts / "configure-msvc-release.py"),
                "--workspace", str(workspace), "--target", TARGET,
                "--target-dir", str(target_dir), "--static-crt",
            ], check=True)
            env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir), CARGO_INCREMENTAL="0", CARGO_PROFILE_RELEASE_DEBUG="0")
            node = addon / "index.win32-x64-msvc.node"
            node.unlink(missing_ok=True)
            subprocess.run([
                npx, "--no-install", "napi", "build", "--platform", "--release",
                "--target", TARGET, "--cargo-flags=--locked",
            ], cwd=addon, env=env, check=True)
            if node.is_symlink() or not node.is_file():
                raise ValueError("Build did not produce a regular addon")
            with node.open("rb") as stream:
                checksum = hashlib.file_digest(stream, "sha256").hexdigest()
            hashes.append(checksum)
            print(f"Build {label}: SHA256 {checksum}", flush=True)
        if hashes[0] != hashes[1]:
            raise ValueError(f"Windows addon is not reproducible: {hashes[0]} != {hashes[1]}")
    finally:
        config.write_bytes(original)
        for target_dir in owned_dirs:
            shutil.rmtree(target_dir, ignore_errors=True)


if __name__ == "__main__":
    main()

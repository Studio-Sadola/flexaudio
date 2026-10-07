#!/usr/bin/env python3
"""Append explicit-target release flags without overriding cfg rustflags."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path

TARGETS = ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")


def flags(workspace: Path, cargo_home: Path, target_dir: Path, static_crt: bool) -> list[str]:
    result = ["-C", "link-arg=/DEBUG:NONE"]
    if static_crt:
        result += ["-C", "target-feature=+crt-static"]
    # rustc uses the last matching mapping: put narrower paths last.
    mappings = [(target_dir.resolve(), "/target"), (cargo_home.resolve(), "/cargo"), (workspace.resolve(), "/src")]
    for path, replacement in sorted(mappings, key=lambda entry: len(str(entry[0]))):
        for spelling in sorted({str(path), path.as_posix()}):
            result.append(f"--remap-path-prefix={spelling}={replacement}")
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--workspace", type=Path, default=Path.cwd())
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--static-crt", action="store_true")
    args = parser.parse_args()
    key = "CARGO_TARGET_" + args.target.upper().replace("-", "_") + "_RUSTFLAGS"
    if any(name in os.environ for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", key)):
        raise ValueError("Environment rustflags would override the release configuration")
    config = args.workspace / ".cargo/config.toml"
    text = config.read_text(encoding="utf-8")
    if f'[target.{args.target}]' in text:
        raise ValueError("Target release flags already configured")
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    values = flags(args.workspace, cargo_home, args.target_dir, args.static_crt)
    config.write_text(text + f'\n[target.{args.target}]\nrustflags = ' + json.dumps(values) + '\n', encoding="utf-8")


if __name__ == "__main__":
    main()

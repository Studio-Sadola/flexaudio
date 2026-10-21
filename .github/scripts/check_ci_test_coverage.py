#!/usr/bin/env python3
"""Fail when a binding test file is not named by any workflow.

Enumerates bindings/flexaudio-py/tests/test_*.py and
crates/flexaudio-napi/__test__/*.test.mjs from the filesystem. A file passes
when its filename is its own token in any .github/workflows/*.yml. Workflow
steps sometimes name a crate-relative path, so the repository-relative path is
not required. An empty match list fails, so a moved directory cannot pass
silently. Standard library only.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys


SPECS = (
    ("bindings/flexaudio-py/tests", "test_*.py"),
    ("crates/flexaudio-napi/__test__", "*.test.mjs"),
)
WORKFLOWS = Path(".github/workflows")


class CoverageError(Exception):
    """A binding test is absent from CI, or a test directory matched nothing."""


def matching_files(root: Path, relative: str, pattern: str) -> list[Path]:
    directory = root / relative
    if not directory.is_dir():
        return []
    return sorted(path for path in directory.glob(pattern) if path.is_file())


def workflow_text(root: Path) -> str:
    directory = root / WORKFLOWS
    if not directory.is_dir():
        return ""
    chunks: list[str] = []
    for path in sorted(directory.glob("*.yml")):
        if path.is_file():
            chunks.append(path.read_text(encoding="utf-8"))
    return "\n".join(chunks)


def name_referenced(text: str, name: str) -> bool:
    """True when name is a whole filename token, not part of a longer name."""
    pattern = rf"(?<![A-Za-z0-9_.-]){re.escape(name)}(?![A-Za-z0-9_.-])"
    return re.search(pattern, text) is not None


def check(root: Path) -> int:
    empty: list[str] = []
    found: list[Path] = []
    for relative, pattern in SPECS:
        matches = matching_files(root, relative, pattern)
        if not matches:
            empty.append(f"no test files matched {relative}/{pattern}")
        found.extend(matches)
    if empty:
        raise CoverageError("\n".join(empty))
    text = workflow_text(root)
    missing = [path.relative_to(root).as_posix() for path in found if not name_referenced(text, path.name)]
    if missing:
        raise CoverageError("workflow files do not name:\n" + "\n".join(missing))
    return len(found)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args(argv)
    try:
        count = check(args.repo_root)
    except CoverageError as error:
        print(f"CI test coverage check failed: {error}", file=sys.stderr)
        return 1
    except (OSError, UnicodeError) as error:
        print(
            f"CI test coverage check failed: cannot read tests or workflows ({type(error).__name__})",
            file=sys.stderr,
        )
        return 1
    print(f"CI test coverage check passed: {count} binding tests")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

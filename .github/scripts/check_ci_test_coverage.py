#!/usr/bin/env python3
"""Fail when a binding test is not named by a command a workflow step runs.

Enumerates bindings/flexaudio-py/tests/test_*.py and
crates/flexaudio-napi/__test__/*.test.mjs from the filesystem. A file is
covered only when its filename is its own token in ``jobs.<id>.steps[*].run``.
Workflows are parsed with PyYAML (``yaml.safe_load``). A line scan counts
matrix entries, disabled jobs, and ``${{ false }}`` as commands, and an
unbalanced quote hides a trailing YAML comment. GitHub-hosted Ubuntu runners
do not install PyYAML for the system Python, so CI installs a pinned PyYAML
before this gate. A job or step whose ``if`` is statically false does not
count. Literal block scalars keep shell comments; those lines are not
commands. An empty test directory, a workflows directory with no ``.yml`` or
``.yaml`` file, or a workflow that cannot be parsed fails closed.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys
from types import ModuleType


SPECS = (
    ("bindings/flexaudio-py/tests", "test_*.py"),
    ("crates/flexaudio-napi/__test__", "*.test.mjs"),
)
WORKFLOWS = Path(".github/workflows")


class CoverageError(Exception):
    """A binding test is absent from CI, or a workflow cannot be trusted."""


def matching_files(root: Path, relative: str, pattern: str) -> list[Path]:
    directory = root / relative
    if not directory.is_dir():
        return []
    return sorted(path for path in directory.glob(pattern) if path.is_file())


def workflow_files(root: Path) -> list[Path]:
    directory = root / WORKFLOWS
    if not directory.is_dir():
        return []
    found: list[Path] = []
    for pattern in ("*.yml", "*.yaml"):
        found.extend(path for path in directory.glob(pattern) if path.is_file())
    return sorted(found)


def load_yaml() -> ModuleType:
    try:
        import yaml
    except ImportError as error:
        raise CoverageError("PyYAML is required") from error
    return yaml


def statically_false(value: object) -> bool:
    """True for boolean false or a string that is only ``false`` / ``${{ false }}``."""
    if value is False:
        return True
    if not isinstance(value, str):
        return False
    token = value.strip()
    if token.startswith("${{") and token.endswith("}}"):
        token = token[3:-2].strip()
    return token.casefold() == "false"


def strip_trailing_shell_comment(line: str) -> str:
    """Drop a trailing `` #`` shell comment. ``#`` inside quotes stays."""
    in_single = False
    in_double = False
    for index, char in enumerate(line):
        if char == "'" and not in_double:
            in_single = not in_single
        elif char == '"' and not in_single:
            in_double = not in_double
        elif (
            char == "#"
            and not in_single
            and not in_double
            and (index == 0 or line[index - 1].isspace())
        ):
            return line[:index].rstrip()
    return line.rstrip()


def command_text(run: str) -> str:
    """Return the command lines of a parsed ``run`` value."""
    kept: list[str] = []
    for line in run.splitlines():
        if line.lstrip().startswith("#"):
            continue
        kept.append(strip_trailing_shell_comment(line))
    return "\n".join(kept)


def require_mapping(value: object, relative: str, label: str) -> dict[object, object]:
    if not isinstance(value, dict):
        raise CoverageError(f"{relative}: {label}")
    return value


def step_commands(job: dict[object, object], relative: str) -> list[str]:
    if "uses" in job or statically_false(job.get("if")):
        return []
    if "steps" not in job or job.get("steps") is None:
        return []
    steps = job.get("steps")
    if not isinstance(steps, list):
        raise CoverageError(f"{relative}: steps is not a list")
    commands: list[str] = []
    for step in steps:
        if not isinstance(step, dict) or statically_false(step.get("if")):
            continue
        run = step.get("run")
        if not isinstance(run, str):
            continue
        text = command_text(run)
        if text.strip():
            commands.append(text)
    return commands


def workflow_commands(document: object, relative: str) -> list[str]:
    if not isinstance(document, dict):
        raise CoverageError(f"{relative}: workflow root is not a mapping")
    if "jobs" not in document or not isinstance(document.get("jobs"), dict):
        raise CoverageError(f"{relative}: jobs is missing or not a mapping")
    jobs = require_mapping(document.get("jobs"), relative, "jobs is missing or not a mapping")
    commands: list[str] = []
    for job in jobs.values():
        if not isinstance(job, dict):
            raise CoverageError(f"{relative}: job is not a mapping")
        commands.extend(step_commands(job, relative))
    return commands


def read_workflow(path: Path, root: Path, yaml: ModuleType) -> list[str]:
    relative = path.relative_to(root).as_posix()
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise CoverageError(f"{relative}: cannot read workflow ({type(error).__name__})") from error
    try:
        document = yaml.safe_load(text)
    except yaml.YAMLError as error:
        raise CoverageError(f"{relative}: workflow does not parse ({type(error).__name__})") from error
    return workflow_commands(document, relative)


def name_referenced(text: str, name: str) -> bool:
    """True when ``name`` is a whole filename token, not part of a longer name."""
    pattern = rf"(?<![A-Za-z0-9_.-]){re.escape(name)}(?![A-Za-z0-9_.-])"
    return re.search(pattern, text) is not None


def check(root: Path) -> int:
    problems: list[str] = []
    found: list[Path] = []
    for relative, pattern in SPECS:
        matches = matching_files(root, relative, pattern)
        if not matches:
            problems.append(f"no test files matched {relative}/{pattern}")
        found.extend(matches)
    workflows = workflow_files(root)
    if not workflows:
        problems.append(f"no workflow files under {WORKFLOWS.as_posix()}")
    if problems:
        raise CoverageError("\n".join(problems))
    yaml = load_yaml()
    errors: list[str] = []
    chunks: list[str] = []
    for path in workflows:
        try:
            chunks.extend(read_workflow(path, root, yaml))
        except CoverageError as error:
            errors.append(str(error))
    text = "\n".join(chunks)
    missing = [path.relative_to(root).as_posix() for path in found if not name_referenced(text, path.name)]
    if errors or missing:
        parts = list(errors)
        if missing:
            parts.append("workflow files do not name:\n" + "\n".join(missing))
        raise CoverageError("\n".join(parts))
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

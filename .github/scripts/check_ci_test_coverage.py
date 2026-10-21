#!/usr/bin/env python3
"""Fail when a binding test is not named by a command a workflow step runs.

Enumerates bindings/flexaudio-py/tests/test_*.py and
crates/flexaudio-napi/__test__/*.test.mjs from the filesystem. A file is
covered only when its filename is its own token inside a step ``run:`` value
(a single-line command, or a ``|`` / ``>`` block, including chomp markers).
The block ends when a non-comment line returns to that key's indentation or
less. Full-line comments and trailing `` #`` comments do not count, nor do
``name:``, ``with:``, ``env:``, or any other key. A step whose ``if:`` is
literally false (any casing, quoted or not) does not count. Workflow steps
sometimes name a crate-relative path, so the repository-relative path is not
required. An empty test directory, or a workflows directory with no ``.yml``
or ``.yaml`` file, fails closed. Standard library only.
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
# Comments and step names are descriptions, not executed commands.
COMMENT_OR_BLANK = re.compile(r"^\s*(#.*)?$")
KEY_LINE = re.compile(
    r"^(?P<indent> *)(?:(?P<dash>- +))?(?P<key>[A-Za-z_][\w-]*):(?P<value>.*)$"
)
BLOCK_HEADER = re.compile(r"[|>](?:[1-9][+-]?|[+-][1-9]?)?")


class CoverageError(Exception):
    """A binding test is absent from CI, or a test directory matched nothing."""


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


def strip_trailing_comment(line: str) -> str:
    """Drop a trailing YAML comment. ``#`` inside quotes stays in the command."""
    in_single = False
    in_double = False
    for index, char in enumerate(line):
        if char == "'" and not in_double:
            in_single = not in_single
        elif char == '"' and not in_single:
            in_double = not in_double
        elif char == "#" and not in_single and not in_double and (index == 0 or line[index - 1].isspace()):
            return line[:index].rstrip()
    return line.rstrip()


def is_block_header(value: str) -> bool:
    return BLOCK_HEADER.fullmatch(value) is not None


def is_literal_false(value: str) -> bool:
    token = value.strip()
    if len(token) >= 2 and token[0] == token[-1] and token[0] in "'\"":
        token = token[1:-1]
    return token.casefold() == "false"


def parse_key_line(line: str) -> tuple[int, int | None, str, str] | None:
    """Return ``(key_column, dash_indent, key, value)`` for a mapping entry."""
    match = KEY_LINE.match(strip_trailing_comment(line))
    if match is None:
        return None
    indent = len(match.group("indent"))
    dash = match.group("dash")
    key_column = indent + (len(dash) if dash is not None else 0)
    dash_indent = indent if dash is not None else None
    return key_column, dash_indent, match.group("key"), match.group("value").strip()


def indentation(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


def consume_block(lines: list[str], index: int, key_column: int, *, collect: bool) -> tuple[int, str]:
    """Read a block scalar. Stop when indentation returns to ``key_column`` or less."""
    collected: list[str] = []
    index += 1
    total = len(lines)
    while index < total:
        line = lines[index]
        if COMMENT_OR_BLANK.match(line):
            index += 1
            continue
        if indentation(line) <= key_column:
            break
        if collect:
            collected.append(strip_trailing_comment(line).strip())
        index += 1
    return index, "\n".join(collected)


def executed_commands(text: str) -> str:
    """Return the ``run:`` text of steps that are not ``if: false``."""
    lines = text.splitlines()
    executed: list[str] = []
    index = 0
    step_key_column: int | None = None
    step_dash: int | None = None
    step_disabled = False
    step_chunks: list[str] = []

    def close_step() -> None:
        nonlocal step_key_column, step_dash, step_disabled, step_chunks
        if step_key_column is not None and not step_disabled:
            executed.extend(step_chunks)
        step_key_column = None
        step_dash = None
        step_disabled = False
        step_chunks = []

    while index < len(lines):
        line = lines[index]
        if COMMENT_OR_BLANK.match(line):
            index += 1
            continue
        parsed = parse_key_line(line)
        if parsed is None:
            index += 1
            continue
        key_column, dash_indent, key, value = parsed
        if dash_indent is not None and (step_dash is None or dash_indent <= step_dash):
            close_step()
            step_dash = dash_indent
            step_key_column = key_column
        elif step_key_column is not None and key_column < step_key_column:
            close_step()
        elif step_key_column is not None and key_column != step_key_column:
            if is_block_header(value):
                index, _ = consume_block(lines, index, key_column, collect=False)
                continue
            index += 1
            continue

        in_step = step_key_column is not None and key_column == step_key_column
        if in_step and key == "if" and is_literal_false(value):
            step_disabled = True
            index += 1
            continue
        if in_step and key == "run":
            if is_block_header(value):
                index, body = consume_block(lines, index, key_column, collect=True)
                if body:
                    step_chunks.append(body)
                continue
            if value:
                step_chunks.append(value)
            index += 1
            continue
        if is_block_header(value):
            index, _ = consume_block(lines, index, key_column, collect=False)
            continue
        index += 1
    close_step()
    return "\n".join(executed)


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
    text = "\n".join(executed_commands(path.read_text(encoding="utf-8")) for path in workflows)
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
